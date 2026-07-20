#!/usr/bin/env python3
"""Export the CLIP ViT-B/32 visual tower to a Qualcomm HTP "pack" for local-clip.

Pipeline (each stage skippable with --from):
  0. download        onnx/vision_model.onnx from Hugging Face (+ the LAION .pth if --aesthetic)
  1. shape           force the input to static 1x3x224x224 (NCHW); trim outputs to image_embeds
  2. convert         qairt-converter            ONNX -> model.dlc  (fp16)
  3. lib             (no-op on the DLC route)
  4. context         qnn-context-binary-generator  model.dlc + HTP -> context .bin
  5. pack            assemble <out>/ : CLIPV marker, model.bin, optional aesthetic.bin

With --text the SAME stages also build the CLIP text tower into the same pack dir:
onnx/text_model.onnx + tokenizer.json are fetched, input_ids/attention_mask are pinned to
[1,77] (int64 -> int32 externally so the QAIRT converter and qnn-rs I32 path both accept
them), the graph is trimmed to text_embeds [1,512], converted to clip_text.dlc, and its
context binary is copied to text_model.bin next to tokenizer.json. Typed semantic search on
device needs both text_model.bin and tokenizer.json.

This script is meant to run ON THE MACHINE WHERE THE QAIRT/QNN SDK LIVES. It never
runs the SDK tools itself here unless they are present; each SDK stage checks its tool and
fails with a clear message otherwise.

Preprocessing contract the runtime (crates/local-clip) implements and that this graph
must match: resize the shortest side to 224 (bilinear), centre-crop 224x224, RGB, scale
to 0..1 then normalize with CLIP means [0.48145466, 0.4578275, 0.40821073] / stds
[0.26862954, 0.26130258, 0.27577711], layout NCHW (channel-planar). Output: the pooled/
projected image embedding (512-d), output name `image_embeds`. The Rust side reads the
graph's SOLE output, so stage 1 trims the ONNX (which also emits last_hidden_state) down
to image_embeds. NOTE the layout differs from WD14's NHWC — this graph is NCHW.

Aesthetic head (--aesthetic): the LAION-AI/aesthetic-predictor sa_0_4_vit_b_32_linear.pth,
a single Linear(512->1) trained on B/32 embeddings. It is written to aesthetic.bin as
little-endian f32 [w0..w511, bias]. Contract: score = dot(w, emb) + b where emb is the
L2-NORMALIZED CLIP embedding (LAION divides features by their norm before the head).
Loading the .pth needs torch; the stage fails with a clear message if torch is absent.

Example (from a shell with the SDK on PATH, or pass --sdk):
  source scripts/qnn-env.sh                 # sets QNN_SDK_ROOT + venv, see that file
  python3 scripts/clip-export/export.py --out ~/clip --work ~/clip-build --aesthetic

  # older-arch HTP build (default); V73 binaries run fine on newer HTP (V81 / S26 Ultra):
  python3 scripts/clip-export/export.py --dsp-arch v73 --soc sm8550 --out ~/clip
"""

import argparse
import os
import shutil
import struct
import subprocess
import sys
import urllib.request
from pathlib import Path

HF_HOST = "https://huggingface.co"
DEFAULT_REPO = "Xenova/clip-vit-base-patch32"
VISION_REMOTE = "onnx/vision_model.onnx"
TEXT_REMOTE = "onnx/text_model.onnx"
TOKENIZER_REMOTE = "tokenizer.json"
INPUT_SIZE = 224
EMBED_DIM = 512
EMBED_OUTPUT = "image_embeds"
TEXT_EMBED_OUTPUT = "text_embeds"
CONTEXT_LEN = 77
TEXT_NAME = "clip_text"
AESTHETIC_PTH = "sa_0_4_vit_b_32_linear.pth"
AESTHETIC_URL = f"https://raw.githubusercontent.com/LAION-AI/aesthetic-predictor/main/{AESTHETIC_PTH}"
STAGES = ["download", "shape", "convert", "lib", "context", "pack"]


def die(msg: str) -> "NoReturn":  # type: ignore[name-defined]
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def note(msg: str) -> None:
    print(f"==> {msg}", flush=True)


def stage_ge(name: str, start: str) -> bool:
    return STAGES.index(name) >= STAGES.index(start)


def find_sdk(arg: str | None) -> Path:
    root = arg or os.environ.get("QNN_SDK_ROOT") or os.environ.get("QAIRT_ROOT")
    if not root:
        die("no --sdk and no QNN_SDK_ROOT/QAIRT_ROOT in env (source scripts/qnn-env.sh)")
    p = Path(root)
    if not (p / "bin" / "x86_64-linux-clang").is_dir():
        die(f"{p}/bin/x86_64-linux-clang missing — extract the full QAIRT zip")
    return p


def sdk_tool(sdk: Path, name: str) -> Path:
    p = sdk / "bin" / "x86_64-linux-clang" / name
    if not p.exists():
        die(f"SDK tool not found: {p}")
    return p


def sdk_env(sdk: Path | None, python: str | None = None) -> dict:
    # SDK tools need libc++ (NDK host runtime) + the SDK's host libs on the loader path,
    # python wrappers additionally qti.* and libpython (mirrors qnn-env.sh).
    env = os.environ.copy()
    if sdk is None:
        return env
    env["PYTHONPATH"] = f"{sdk}/lib/python" + os.pathsep + env.get("PYTHONPATH", "")
    host_rt = os.environ.get("QAIRT_HOST_RT", str(Path.home() / "Desktop/QNN/host-runtime"))
    libs = [host_rt, f"{sdk}/lib/x86_64-linux-clang"]
    if python is not None:
        libs.append(str(Path(os.path.realpath(python)).parent.parent / "lib"))
    libs = [d for d in libs if Path(d).is_dir()]
    if "LD_LIBRARY_PATH" in env:
        libs.append(env["LD_LIBRARY_PATH"])
    env["LD_LIBRARY_PATH"] = os.pathsep.join(libs)
    return env


def run_py_tool(python: str, tool: Path, args: list[str], sdk: Path | None = None) -> None:
    # QAIRT converters are Python wrappers; launch via ld-linux so a rewritten argv0
    # (e.g. Cursor's AppImage on paths containing a version string) can't break them.
    ld = "/lib64/ld-linux-x86-64.so.2"
    cmd = [ld, python, str(tool), *args] if Path(ld).exists() else [python, str(tool), *args]
    note(" ".join(cmd))
    subprocess.run(cmd, check=True, env=sdk_env(sdk, python))


def run_native_tool(tool: Path, args: list[str], sdk: Path | None = None) -> None:
    note(f"{tool} {' '.join(args)}")
    subprocess.run([str(tool), *args], check=True, env=sdk_env(sdk))


def fetch(dst: Path, repo: str, remote: str) -> None:
    if dst.exists() and dst.stat().st_size > 0:
        note(f"have {dst.name} ({dst.stat().st_size} bytes)")
        return
    try:
        from huggingface_hub import hf_hub_download  # type: ignore
        note(f"hf_hub_download {repo}/{remote}")
        got = hf_hub_download(repo_id=repo, filename=remote)
        shutil.copyfile(got, dst)
    except Exception:
        url = f"{HF_HOST}/{repo}/resolve/main/{remote}"
        note(f"download {url}")
        try:
            urllib.request.urlretrieve(url, dst)
        except Exception as e:  # noqa: BLE001
            die(f"could not fetch {remote}: {e} (install huggingface_hub or download it manually into {dst.parent})")


def download(repo: str, work: Path, want_aesthetic: bool) -> Path:
    work.mkdir(parents=True, exist_ok=True)
    onnx = work / "model.onnx"
    fetch(onnx, repo, VISION_REMOTE)
    if want_aesthetic:
        pth = work / AESTHETIC_PTH
        if not (pth.exists() and pth.stat().st_size > 0):
            note(f"download {AESTHETIC_URL}")
            try:
                urllib.request.urlretrieve(AESTHETIC_URL, pth)
            except Exception as e:  # noqa: BLE001
                die(f"could not fetch {AESTHETIC_PTH}: {e} (place it in {work} manually)")
    return onnx


def pick_embedding_output(model, want: str = EMBED_OUTPUT) -> str:
    outs = list(model.graph.output)
    for o in outs:
        if o.name == want:
            return o.name
    # Fallback: the rank-2 [1, N] pooled/projected embedding.
    for o in outs:
        if len(o.type.tensor_type.shape.dim) == 2:
            print(f"warn: no '{want}' output; using rank-2 output '{o.name}'", file=sys.stderr)
            return o.name
    print(f"warn: no {want} / rank-2 output; using first output '{outs[0].name}'", file=sys.stderr)
    return outs[0].name


def fix_shape(onnx_in: Path, onnx_out: Path) -> None:
    try:
        import onnx  # type: ignore
    except ImportError:
        die("python 'onnx' package required for the shape stage (pip install onnx)")
    model = onnx.load(str(onnx_in))
    inp = model.graph.input[0]
    dims = inp.type.tensor_type.shape.dim
    # CLIP vision is NCHW; pin batch to 1 and fill any dynamic C/H/W with the expected values.
    if len(dims) != 4:
        die(f"unexpected input rank {len(dims)} (expected NCHW rank 4)")
    dims[0].ClearField("dim_param")
    dims[0].dim_value = 1
    for idx, val in ((1, 3), (2, INPUT_SIZE), (3, INPUT_SIZE)):
        d = dims[idx]
        if d.dim_value == 0 or d.HasField("dim_param"):
            d.ClearField("dim_param")
            d.dim_value = val
    static = [d.dim_value for d in dims]
    if static[1:] != [3, INPUT_SIZE, INPUT_SIZE]:
        print(
            f"warn: input C/H/W are {static[1:]}, not [3,{INPUT_SIZE},{INPUT_SIZE}] — check the model variant / layout",
            file=sys.stderr,
        )
    note(f"input '{inp.name}' fixed to {static} (NCHW)")
    emb = pick_embedding_output(model)
    out_names = [o.name for o in model.graph.output]
    note(f"embedding output '{emb}' (all outputs: {out_names})")
    onnx.save(model, str(onnx_out))
    # The Rust side uses the graph's SOLE output; trim any extra outputs (e.g. last_hidden_state).
    if len(out_names) > 1:
        import onnx.utils  # type: ignore
        note(f"trimming outputs {out_names} -> ['{emb}']")
        onnx.utils.extract_model(str(onnx_out), str(onnx_out), [inp.name], [emb])


def cast_int64_inputs_to_int32(model) -> None:
    # QAIRT chokes on int64 graph inputs; declare each int64 input as int32 and prepend a
    # Cast(int32->int64) whose output reuses the original input name, so consumers are unchanged.
    from onnx import TensorProto, helper  # type: ignore

    g = model.graph
    casts, new_inputs = [], []
    for inp in list(g.input):
        tt = inp.type.tensor_type
        if tt.elem_type != TensorProto.INT64:
            new_inputs.append(inp)
            continue
        dims = [d.dim_value for d in tt.shape.dim]
        new_inputs.append(helper.make_tensor_value_info(inp.name + "_i32", TensorProto.INT32, dims))
        casts.append(helper.make_node("Cast", [inp.name + "_i32"], [inp.name],
                                      to=TensorProto.INT64, name=f"cast_{inp.name}_to_i64"))
        note(f"text input '{inp.name}' int64 -> int32 (external), Cast to int64 inside")
    if not casts:
        return
    del g.input[:]
    g.input.extend(new_inputs)
    nodes = list(g.node)
    del g.node[:]
    g.node.extend(casts + nodes)


def fix_text_shape(onnx_in: Path, onnx_out: Path) -> None:
    try:
        import onnx  # type: ignore
    except ImportError:
        die("python 'onnx' package required for the shape stage (pip install onnx)")
    model = onnx.load(str(onnx_in))
    for inp in model.graph.input:
        dims = inp.type.tensor_type.shape.dim
        if len(dims) != 2:
            print(f"warn: text input '{inp.name}' rank {len(dims)} != 2 (expected [batch, seq])", file=sys.stderr)
            continue
        dims[0].ClearField("dim_param")
        dims[0].dim_value = 1
        dims[1].ClearField("dim_param")
        dims[1].dim_value = CONTEXT_LEN
        note(f"text input '{inp.name}' fixed to [1, {CONTEXT_LEN}]")
    cast_int64_inputs_to_int32(model)
    emb = pick_embedding_output(model, TEXT_EMBED_OUTPUT)
    out_names = [o.name for o in model.graph.output]
    in_names = [i.name for i in model.graph.input]
    note(f"text embedding output '{emb}' (all outputs: {out_names})")
    onnx.save(model, str(onnx_out))
    if len(out_names) > 1:
        import onnx.utils  # type: ignore
        note(f"trimming outputs {out_names} -> ['{emb}']")
        onnx.utils.extract_model(str(onnx_out), str(onnx_out), in_names, [emb])


def gen_context_binary(sdk: Path, out_dir: Path, name: str, dlc: Path, args) -> Path:
    # DLC + HTP -> offline context binary; out_dir is per-tower so the *.bin fallback can't collide.
    htp_backend = sdk / "lib" / "x86_64-linux-clang" / "libQnnHtp.so"
    if not htp_backend.exists():
        die(f"missing HTP backend {htp_backend}")
    gen = sdk_tool(sdk, "qnn-context-binary-generator")
    htp_cfg = out_dir / f"htp_{args.dsp_arch}.json"
    htp_cfg.write_text(
        '{\n'
        f'  "graphs": [{{"graph_names": ["{name}"], "vtcm_mb": {args.vtcm_mb}, "fp16_relaxed_precision": 1, "O": 3.0}}],\n'
        f'  "devices": [{{"dsp_arch": "{args.dsp_arch}", "cores": [{{"core_id": 0, "perf_profile": "burst", "rpc_control_latency": 100}}]}}]\n'
        '}\n'
    )
    dlc_loader = sdk / "lib" / "x86_64-linux-clang" / "libQnnModelDlc.so"
    if not dlc_loader.exists():
        die(f"missing {dlc_loader}")
    ctx_bin = out_dir / f"{name}_ctx.bin"
    gen_args = ["--model", str(dlc_loader), "--dlc_path", str(dlc),
                "--backend", str(htp_backend), "--binary_file", str(ctx_bin),
                "--output_dir", str(out_dir), "--htp_socs", args.soc, "--vtcm_override", args.vtcm_mb]
    ext_so = sdk / "lib" / "x86_64-linux-clang" / "libQnnHtpNetRunExtensions.so"
    if ext_so.exists():
        backend_cfg = out_dir / "backend_extensions.json"
        backend_cfg.write_text(
            '{"backend_extensions": {"shared_library_path": "%s", "config_file_path": "%s"}}\n'
            % (ext_so, htp_cfg)
        )
        gen_args += ["--config_file", str(backend_cfg)]
    else:
        print(f"warn: no {ext_so} — generating with --htp_socs only", file=sys.stderr)
    run_native_tool(gen, gen_args, sdk)
    if not ctx_bin.exists():
        found = [p for p in out_dir.rglob("*.bin")]
        if not found:
            die(f"context binary not produced under {out_dir}")
        ctx_bin = found[0]
    return ctx_bin


def build_aesthetic(work: Path, out_bin: Path) -> None:
    pth = work / AESTHETIC_PTH
    if not (pth.exists() and pth.stat().st_size > 0):
        note(f"download {AESTHETIC_URL}")
        try:
            urllib.request.urlretrieve(AESTHETIC_URL, pth)
        except Exception as e:  # noqa: BLE001
            die(f"could not fetch {AESTHETIC_PTH}: {e} (place it in {work} manually)")
    try:
        import torch  # type: ignore
    except ImportError:
        die("--aesthetic needs torch to load the .pth (pip install torch), or omit --aesthetic to skip the head")
    state = torch.load(str(pth), map_location="cpu", weights_only=True)
    weight = None
    bias = None
    for k, v in state.items():
        if k.endswith("weight"):
            weight = [float(x) for x in v.reshape(-1).tolist()]
        elif k.endswith("bias"):
            bias = [float(x) for x in v.reshape(-1).tolist()]
    if weight is None or bias is None or len(bias) != 1:
        die(f"{AESTHETIC_PTH} is not a Linear(512->1) state dict (keys: {list(state.keys())})")
    if len(weight) != EMBED_DIM:
        print(f"warn: aesthetic weight has {len(weight)} dims, expected {EMBED_DIM}", file=sys.stderr)
    floats = weight + bias  # little-endian f32 [w0..w(n-1), bias]
    out_bin.write_bytes(struct.pack(f"<{len(floats)}f", *floats))
    note(f"wrote aesthetic head ({len(weight)} weights + bias) -> {out_bin}")


def main() -> None:
    ap = argparse.ArgumentParser(description="Export the CLIP ViT-B/32 visual tower to a local-clip HTP pack.")
    ap.add_argument("--repo", default=DEFAULT_REPO, help=f"HF model repo (default {DEFAULT_REPO})")
    ap.add_argument("--out", required=True, type=Path, help="pack output dir (gets CLIPV, model.bin, aesthetic.bin)")
    ap.add_argument("--work", type=Path, default=None, help="scratch dir (default <out>-build)")
    ap.add_argument("--sdk", default=None, help="QAIRT/QNN SDK root (else QNN_SDK_ROOT / QAIRT_ROOT)")
    ap.add_argument("--python", default=os.environ.get("QAIRT_VENV", "") and f"{os.environ['QAIRT_VENV']}/bin/python" or sys.executable, help="python that has the converter deps")
    ap.add_argument("--dsp-arch", default="v73", help="HTP arch (default v73; runs on newer V81/S26U)")
    ap.add_argument("--soc", default="sm8550", help="offline-cache SoC model (default sm8550 = V73)")
    ap.add_argument("--vtcm-mb", default="8", help="VTCM budget MB (default 8)")
    ap.add_argument("--aesthetic", action="store_true", help="also build aesthetic.bin from the LAION .pth (needs torch)")
    ap.add_argument("--text", action="store_true", help="also build the text tower (text_model.bin + tokenizer.json) for typed semantic search")
    ap.add_argument("--convert-arg", action="append", default=[], help="extra token passed through to qairt-converter (repeatable), e.g. for --desired_input_layout if it complains about NCHW")
    ap.add_argument("--from", dest="from_stage", default="download", choices=STAGES, help="skip earlier stages")
    args = ap.parse_args()

    work = args.work or Path(f"{args.out}-build")
    work.mkdir(parents=True, exist_ok=True)
    onnx_raw = work / "model.onnx"
    onnx_static = work / "model_static.onnx"
    text_onnx_raw = work / "text_model.onnx"
    text_onnx_static = work / "text_static.onnx"
    text_dlc = work / f"{TEXT_NAME}.dlc"
    text_ctx_dir = work / "text_ctx"
    tokenizer_src = work / "tokenizer.json"

    if stage_ge("download", args.from_stage):
        onnx_raw = download(args.repo, work, args.aesthetic)
        if args.text:
            fetch(text_onnx_raw, args.repo, TEXT_REMOTE)
            fetch(tokenizer_src, args.repo, TOKENIZER_REMOTE)
    if stage_ge("shape", args.from_stage):
        fix_shape(onnx_raw, onnx_static)
        if args.text:
            fix_text_shape(text_onnx_raw, text_onnx_static)

    # Stages 2-4 need the SDK; resolve it lazily so the download/shape stages work without one.
    need_sdk = any(stage_ge(s, args.from_stage) for s in ("convert", "lib", "context"))
    sdk = find_sdk(args.sdk) if need_sdk else None

    # QAIRT 2.48 converters crash on onnx >= 1.19 (removed onnx.version); fail fast with the fix.
    if need_sdk:
        chk = subprocess.run([args.python, "-c", "import onnx; print(onnx.__version__)"],
                             capture_output=True, text=True)
        if chk.returncode != 0:
            die(f"{args.python} lacks the 'onnx' package the QAIRT converters need")
        ver = chk.stdout.strip()
        if tuple(int(x) for x in ver.split(".")[:2]) >= (1, 19):
            die(f"onnx {ver} in {args.python} is too new for the QAIRT converters (need < 1.19); "
                f"fix: uv pip install --python {args.python} 'onnx==1.18.0'")

    name = "clip"
    model_dlc = work / f"{name}.dlc"

    if stage_ge("convert", args.from_stage):
        # DLC route: no host model-lib compile, and it works on SDK extractions without share/QNN.
        # Precision: plain fp16. IO tensors come out fp16; qnn-rs converts f32<->f16 at the boundary.
        conv = sdk_tool(sdk, "qairt-converter")
        conv_args = ["--input_network", str(onnx_static), "--output_path", str(model_dlc),
                     "--float_bitwidth", "16", *args.convert_arg]
        run_py_tool(args.python, conv, conv_args, sdk)
        if not model_dlc.exists():
            die(f"converter did not write {model_dlc}")
        if args.text:
            text_conv_args = ["--input_network", str(text_onnx_static), "--output_path", str(text_dlc),
                              "--float_bitwidth", "16", *args.convert_arg]
            run_py_tool(args.python, sdk_tool(sdk, "qairt-converter"), text_conv_args, sdk)
            if not text_dlc.exists():
                die(f"converter did not write {text_dlc}")

    if stage_ge("lib", args.from_stage):
        note("lib stage: not needed on the DLC route (context loads via libQnnModelDlc.so)")

    ctx_bin = work / f"{name}_ctx.bin"
    if stage_ge("context", args.from_stage):
        htp_backend = sdk / "lib" / "x86_64-linux-clang" / "libQnnHtp.so"
        if not htp_backend.exists():
            die(f"missing HTP backend {htp_backend}")
        gen = sdk_tool(sdk, "qnn-context-binary-generator")
        htp_cfg = work / f"htp_{args.dsp_arch}.json"
        htp_cfg.write_text(
            '{\n'
            f'  "graphs": [{{"graph_names": ["{name}"], "vtcm_mb": {args.vtcm_mb}, "fp16_relaxed_precision": 1, "O": 3.0}}],\n'
            f'  "devices": [{{"dsp_arch": "{args.dsp_arch}", "cores": [{{"core_id": 0, "perf_profile": "burst", "rpc_control_latency": 100}}]}}]\n'
            '}\n'
        )
        dlc_loader = sdk / "lib" / "x86_64-linux-clang" / "libQnnModelDlc.so"
        if not dlc_loader.exists():
            die(f"missing {dlc_loader}")
        gen_args = ["--model", str(dlc_loader), "--dlc_path", str(model_dlc),
                    "--backend", str(htp_backend), "--binary_file", str(ctx_bin),
                    "--output_dir", str(work), "--htp_socs", args.soc, "--vtcm_override", args.vtcm_mb]
        ext_so = sdk / "lib" / "x86_64-linux-clang" / "libQnnHtpNetRunExtensions.so"
        if ext_so.exists():
            backend_cfg = work / "backend_extensions.json"
            backend_cfg.write_text(
                '{"backend_extensions": {"shared_library_path": "%s", "config_file_path": "%s"}}\n'
                % (ext_so, htp_cfg)
            )
            gen_args += ["--config_file", str(backend_cfg)]
        else:
            print(f"warn: no {ext_so} — generating with --htp_socs only", file=sys.stderr)
        run_native_tool(gen, gen_args, sdk)
        if not ctx_bin.exists():
            found = [p for p in work.rglob("*.bin") if p.name not in (f"{name}.bin",)]
            if not found:
                die(f"context binary not produced under {work}")
            ctx_bin = found[0]

    text_ctx_bin = text_ctx_dir / f"{TEXT_NAME}_ctx.bin"
    if stage_ge("context", args.from_stage) and args.text:
        text_ctx_dir.mkdir(parents=True, exist_ok=True)
        text_ctx_bin = gen_context_binary(sdk, text_ctx_dir, TEXT_NAME, text_dlc, args)

    if stage_ge("pack", args.from_stage):
        args.out.mkdir(parents=True, exist_ok=True)
        (args.out / "CLIPV").write_text("")
        if not ctx_bin.exists():
            die(f"no context binary at {ctx_bin} — run through the context stage first")
        shutil.copyfile(ctx_bin, args.out / "model.bin")
        emitted = ["CLIPV", "model.bin"]
        if args.aesthetic:
            build_aesthetic(work, args.out / "aesthetic.bin")
            emitted.append("aesthetic.bin")
        if args.text:
            if not text_ctx_bin.exists():
                die(f"no text context binary at {text_ctx_bin} — run --text through the context stage first")
            shutil.copyfile(text_ctx_bin, args.out / "text_model.bin")
            if not (tokenizer_src.exists() and tokenizer_src.stat().st_size > 0):
                die(f"tokenizer.json missing at {tokenizer_src} — run --text through the download stage first")
            shutil.copyfile(tokenizer_src, args.out / "tokenizer.json")
            emitted += ["text_model.bin", "tokenizer.json"]
        note(f"pack ready: {args.out}")
        for f in emitted:
            p = args.out / f
            print(f"    {f:14} {p.stat().st_size} bytes")
        print(
            "\nPush next to the anima/sd/wd14 pack dirs on the device, e.g.:\n"
            "  adb push %s /sdcard/Android/data/com.example.comfyui/files/clip" % args.out
        )


if __name__ == "__main__":
    main()
