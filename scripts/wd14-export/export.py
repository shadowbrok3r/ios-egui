#!/usr/bin/env python3
"""Export a SmilingWolf WD14 tagger ONNX to a Qualcomm HTP "pack" for local-wd14.

Pipeline (each stage skippable with --from):
  0. download        model.onnx + selected_tags.csv from Hugging Face
  1. shape           force the input to static 1x448x448x3 (NHWC); inspect the output op
  2. convert         qnn-onnx-converter        ONNX -> model.cpp + model.bin  (fp16)
  3. lib             qnn-model-lib-generator    model.cpp -> libwd14.so (x86_64 host)
  4. context         qnn-context-binary-generator  libwd14.so + HTP -> context .bin
  5. pack            assemble <out>/ : WD14 marker, model.bin, tags.csv

This script is meant to run ON THE MACHINE WHERE THE QAIRT/QNN SDK LIVES. It never
runs the SDK tools itself here unless they are present; each stage checks its tool and
fails with a clear message otherwise.

Preprocessing contract the runtime (crates/local-wd14) implements and that this graph
must match: 448x448, aspect-fit + centre-pad to square with WHITE background, channel
order RGB->BGR (WD taggers are cv2-trained), float32 in 0..255 (no 0..1 normalization),
layout NHWC. Postprocess: SmilingWolf v3 exports apply per-class sigmoid inside the
graph, so the output is already probabilities (stage 1 prints the output op to confirm);
thresholds general ~0.35, character ~0.85, ratings top-1.

Recommended model: SmilingWolf/wd-vit-tagger-v3 (default). Alternative that also works:
SmilingWolf/wd-v1-4-convnextv2-tagger-v2.

Example (from a shell with the SDK on PATH, or pass --sdk):
  source scripts/qnn-env.sh                 # sets QNN_SDK_ROOT + venv, see that file
  python3 scripts/wd14-export/export.py --out ~/wd14 --work ~/wd14-build

  # older-arch HTP build (default); V73 binaries run fine on newer HTP (V81 / S26 Ultra):
  python3 scripts/wd14-export/export.py --dsp-arch v73 --soc sm8550 --out ~/wd14

  # fallback if the fp16 logits saturate: pin activations to fp32 via an overrides file
  python3 scripts/wd14-export/export.py --fp32-activations --out ~/wd14
"""

import argparse
import os
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

HF_HOST = "https://huggingface.co"
DEFAULT_REPO = "SmilingWolf/wd-vit-tagger-v3"
INPUT_SIZE = 448
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


def download(repo: str, work: Path) -> tuple[Path, Path]:
    work.mkdir(parents=True, exist_ok=True)
    onnx = work / "model.onnx"
    tags = work / "selected_tags.csv"
    for dst, remote in [(onnx, "model.onnx"), (tags, "selected_tags.csv")]:
        if dst.exists() and dst.stat().st_size > 0:
            note(f"have {dst.name} ({dst.stat().st_size} bytes)")
            continue
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
                die(f"could not fetch {remote}: {e} (install huggingface_hub or download it manually into {work})")
    return onnx, tags


def fix_shape(onnx_in: Path, onnx_out: Path) -> None:
    try:
        import onnx  # type: ignore
    except ImportError:
        die("python 'onnx' package required for the shape stage (pip install onnx)")
    model = onnx.load(str(onnx_in))
    inp = model.graph.input[0]
    dims = inp.type.tensor_type.shape.dim
    # WD taggers are NHWC; pin the batch dim to 1 and confirm the spatial/channel dims.
    if len(dims) != 4:
        die(f"unexpected input rank {len(dims)} (expected NHWC rank 4)")
    dims[0].ClearField("dim_param")
    dims[0].dim_value = 1
    static = [d.dim_value for d in dims]
    if static[1:] != [INPUT_SIZE, INPUT_SIZE, 3]:
        print(
            f"warn: input spatial/channel dims are {static[1:]}, not [{INPUT_SIZE},{INPUT_SIZE},3] — "
            "check the model variant / layout",
            file=sys.stderr,
        )
    note(f"input '{inp.name}' fixed to {static}")
    # Report whether the graph already applies sigmoid (SmilingWolf v3 does): output = probs.
    out_name = model.graph.output[0].name
    producer = next((n for n in model.graph.node if out_name in n.output), None)
    op = producer.op_type if producer else "?"
    if op.lower() == "sigmoid":
        note(f"output '{out_name}' is a Sigmoid -> graph emits PROBABILITIES (Wd14Params.apply_sigmoid=false)")
    else:
        print(
            f"warn: output '{out_name}' producer op is '{op}', not Sigmoid — this export may emit RAW LOGITS. "
            "Set Wd14Params.apply_sigmoid=true in local-wd14 if tags look saturated.",
            file=sys.stderr,
        )
    onnx.save(model, str(onnx_out))


def write_fp32_overrides(onnx_path: Path, out_json: Path) -> None:
    # Exhaustive activation override: list every activation tensor as float32 so only the
    # weights stay fp16. Mirrors the Anima fp16 export, where all-fp16 overflowed on the
    # norm's x**2. AIMET-style encodings; the converter flag is --quantization_overrides.
    import json

    try:
        import onnx  # type: ignore
    except ImportError:
        die("python 'onnx' package required for --fp32-activations (pip install onnx)")
    model = onnx.load(str(onnx_path))
    names: set[str] = set()
    for vi in list(model.graph.input) + list(model.graph.output) + list(model.graph.value_info):
        names.add(vi.name)
    for node in model.graph.node:
        names.update(node.output)
    inits = {i.name for i in model.graph.initializer}
    acts = {"activation_encodings": {}, "param_encodings": {}}
    for n in sorted(names):
        if not n or n in inits:
            continue
        acts["activation_encodings"][n] = [{"bitwidth": 32, "dtype": "float"}]
    out_json.write_text(json.dumps(acts, indent=1))
    note(f"wrote {len(acts['activation_encodings'])} fp32 activation overrides -> {out_json}")


def main() -> None:
    ap = argparse.ArgumentParser(description="Export a WD14 tagger ONNX to a local-wd14 HTP pack.")
    ap.add_argument("--repo", default=DEFAULT_REPO, help=f"HF model repo (default {DEFAULT_REPO})")
    ap.add_argument("--out", required=True, type=Path, help="pack output dir (gets WD14, model.bin, tags.csv)")
    ap.add_argument("--work", type=Path, default=None, help="scratch dir (default <out>-build)")
    ap.add_argument("--sdk", default=None, help="QAIRT/QNN SDK root (else QNN_SDK_ROOT / QAIRT_ROOT)")
    ap.add_argument("--python", default=os.environ.get("QAIRT_VENV", "") and f"{os.environ['QAIRT_VENV']}/bin/python" or sys.executable, help="python that has the converter deps")
    ap.add_argument("--dsp-arch", default="v73", help="HTP arch (default v73; runs on newer V81/S26U)")
    ap.add_argument("--soc", default="sm8550", help="offline-cache SoC model (default sm8550 = V73)")
    ap.add_argument("--vtcm-mb", default="8", help="VTCM budget MB (default 8)")
    ap.add_argument("--fp32-activations", action="store_true", help="pin activations fp32 (logit-saturation fallback)")
    ap.add_argument("--from", dest="from_stage", default="download", choices=STAGES, help="skip earlier stages")
    args = ap.parse_args()

    work = args.work or Path(f"{args.out}-build")
    work.mkdir(parents=True, exist_ok=True)
    onnx_raw = work / "model.onnx"
    onnx_static = work / "model_static.onnx"
    tags_csv = work / "selected_tags.csv"

    if stage_ge("download", args.from_stage):
        onnx_raw, tags_csv = download(args.repo, work)
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

    name = "wd14"
    model_dlc = work / f"{name}.dlc"

    if stage_ge("convert", args.from_stage):
        # DLC route: no host model-lib compile, and it works on SDK extractions without share/QNN.
        conv = sdk_tool(sdk, "qairt-converter")
        conv_args = ["--input_network", str(onnx_static), "--output_path", str(model_dlc),
                     "--float_bitwidth", "16"]
        if args.fp32_activations:
            # fp32 activations make the graph IO fp32 by themselves (the local-wd14 contract).
            ov = work / "fp32_overrides.json"
            write_fp32_overrides(onnx_static, ov)
            conv_args += ["--quantization_overrides", str(ov)]
        else:
            # Keep IO fp32 while internals stay fp16; combining this with the overrides segfaults.
            conv_args += ["--preserve_io_datatype"]
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
        (args.out / "WD14").write_text("")
        if not ctx_bin.exists():
            die(f"no context binary at {ctx_bin} — run through the context stage first")
        shutil.copyfile(ctx_bin, args.out / "model.bin")
        if not tags_csv.exists():
            die(f"no {tags_csv} — run the download stage or place selected_tags.csv there")
        shutil.copyfile(tags_csv, args.out / "tags.csv")
        note(f"pack ready: {args.out}")
        for f in ("WD14", "model.bin", "tags.csv"):
            p = args.out / f
            print(f"    {f:10} {p.stat().st_size} bytes")
        print(
            "\nPush next to the anima/sd pack dirs on the device, e.g.:\n"
            "  adb push %s /sdcard/Android/data/com.example.comfyui/files/wd14" % args.out
        )


if __name__ == "__main__":
    main()
