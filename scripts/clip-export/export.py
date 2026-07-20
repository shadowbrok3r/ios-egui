#!/usr/bin/env python3
"""Export the CLIP ViT-B/32 visual tower to a Qualcomm HTP "pack" for local-clip.

Pipeline (each stage skippable with --from):
  0. download        onnx/vision_model.onnx from Hugging Face (+ the LAION .pth if --aesthetic)
  1. shape           force the input to static 1x3x224x224 (NCHW); trim outputs to image_embeds
  2. convert         qairt-converter            ONNX -> model.dlc  (fp16)
  3. lib             (no-op on the DLC route)
  4. context         qnn-context-binary-generator  model.dlc + HTP -> context .bin
  5. pack            assemble <out>/ : CLIPV marker, model.bin, optional aesthetic.bin

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
INPUT_SIZE = 224
EMBED_DIM = 512
EMBED_OUTPUT = "image_embeds"
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


def pick_embedding_output(model) -> str:
    outs = list(model.graph.output)
    for o in outs:
        if o.name == EMBED_OUTPUT:
            return o.name
    # Fallback: the rank-2 [1, N] pooled/projected embedding.
    for o in outs:
        if len(o.type.tensor_type.shape.dim) == 2:
            print(f"warn: no '{EMBED_OUTPUT}' output; using rank-2 output '{o.name}'", file=sys.stderr)
            return o.name
    print(f"warn: no image_embeds / rank-2 output; using first output '{outs[0].name}'", file=sys.stderr)
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
    ap.add_argument("--convert-arg", action="append", default=[], help="extra token passed through to qairt-converter (repeatable), e.g. for --desired_input_layout if it complains about NCHW")
    ap.add_argument("--from", dest="from_stage", default="download", choices=STAGES, help="skip earlier stages")
    args = ap.parse_args()

    work = args.work or Path(f"{args.out}-build")
    work.mkdir(parents=True, exist_ok=True)
    onnx_raw = work / "model.onnx"
    onnx_static = work / "model_static.onnx"

    if stage_ge("download", args.from_stage):
        onnx_raw = download(args.repo, work, args.aesthetic)
    if stage_ge("shape", args.from_stage):
        fix_shape(onnx_raw, onnx_static)

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
