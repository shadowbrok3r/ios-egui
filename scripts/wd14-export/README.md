# WD14 tagger export → `local-wd14` HTP pack

`export.py` turns a [SmilingWolf WD14 tagger](https://huggingface.co/SmilingWolf) ONNX
into a Qualcomm HTP "pack" that the on-device `local-wd14` runtime loads for the app's
**Read tags** action. It downloads the model, forces a static input shape, runs the
three QAIRT/QNN converters, and assembles the pack directory.

The script must run **on the machine that has the QAIRT/QNN SDK** (the converters are
x86-64 host tools). It never runs the SDK here in-repo; each SDK stage checks that its
tool exists and fails with a clear message otherwise.

## Model

- Recommended: **`SmilingWolf/wd-vit-tagger-v3`** (default `--repo`). ViT, 448×448,
  10 861 tags, per-class sigmoid inside the graph.
- Alternative: `SmilingWolf/wd-v1-4-convnextv2-tagger-v2`.

Both ship `model.onnx` and `selected_tags.csv` (the tag table: `tag_id,name,category,count`).

## Prerequisites

- The QAIRT/QNN SDK extracted (needs `bin/x86_64-linux-clang/qairt-converter`,
  `qnn-context-binary-generator`, and `lib/x86_64-linux-clang/libQnnHtp.so` +
  `libQnnModelDlc.so`). Point at it with `--sdk` or `QNN_SDK_ROOT`. The script goes
  ONNX -> DLC -> context binary, so it needs no host compiler and works on SDK
  extractions without `share/QNN` (which `qnn-model-lib-generator` would require).
- The script sets PYTHONPATH/LD_LIBRARY_PATH for the SDK tools itself; sourcing
  `scripts/qnn-env.sh` first is optional.
- A Python with the converter deps and `onnx` **< 1.19** (QAIRT 2.48's converters
  crash on newer: `uv pip install --python <venv>/bin/python 'onnx==1.18.0'`);
  optionally `huggingface_hub` for the download (else a direct HTTPS pull).

## Run

```sh
<qairt-venv>/bin/python scripts/wd14-export/export.py --out ~/wd14 --work ~/wd14-build --sdk ~/Desktop/QNN/qairt/<ver>
```

Defaults target an **older-arch V73** HTP build (`--dsp-arch v73 --soc sm8550`). V73
context binaries run fine on newer HTP (V81 / S26 Ultra); building for the older arch
keeps one artifact working across devices. Override for a native V81 build:

```sh
python3 scripts/wd14-export/export.py --dsp-arch v81 --soc sm8850 --out ~/wd14
```

Skip already-done stages with `--from` (`download shape convert lib context pack`):

```sh
python3 scripts/wd14-export/export.py --from context --out ~/wd14 --work ~/wd14-build
```

### Precision

`--float_bitwidth 16` (fp16) is the default and is usually correct. The graph's IO
tensors come out fp16 too — qnn-rs handles f16 tensor IO (quant::f32_to_f16 /
f16_to_f32), so this needs nothing on the app side. Known-broken alternatives on
QAIRT 2.48 + DLC: `--preserve_io_datatype` fails HTP finalize (q::QNN_Convert), and
`--quantization_overrides` segfaults the converter. If the resulting tags look
saturated (many at ~100 %, or garbage), the fp16 activations may be overflowing —
try the overrides route on a newer SDK:

```sh
python3 scripts/wd14-export/export.py --fp32-activations --out ~/wd14
```

That writes an exhaustive AIMET-style overrides file pinning **every activation to
float32** while weights stay fp16, then passes it as `--quantization_overrides`. This is
the same technique used for the Anima fp16 exports (all-fp16 overflowed on a norm's
`x²`). The exact converter flag name can vary by QAIRT version — adjust in `export.py`
if your SDK rejects it.

### Sigmoid check

Stage 1 (`shape`) prints whether the graph's output is produced by a `Sigmoid`:

- **Sigmoid present** (SmilingWolf v3): the graph emits probabilities. Keep the runtime
  default `Wd14Params { apply_sigmoid: false }`.
- **No Sigmoid** (raw logits): set `apply_sigmoid: true` in `crates/local-wd14`
  (`tagger.rs`) so the runtime applies it before thresholding.

## Pack layout it emits

```
<out>/                 e.g. ~/wd14
├── WD14               marker file (empty; identifies the pack)
├── model.bin          the HTP context binary (the classifier graph)
└── tags.csv           selected_tags.csv (tag_id,name,category,count), logit order
```

Push it next to the existing anima / sd packs under the app's external files dir:

```sh
adb push ~/wd14 /sdcard/Android/data/com.example.comfyui/files/wd14
```

On device the app scans that files dir; a `WD14`-marked subdir enables **Read tags** in
the gallery image viewer. WD14 packs coexist with the SD1.5 / Anima generate packs — a
different pack kind, discovered independently.

## Preprocessing / postprocessing contract

The graph is fed exactly what `crates/local-wd14` produces, so the export input must be
NHWC `1×448×448×3`:

- **Preprocess**: composite over WHITE, aspect-fit + centre-pad to a 448 square with
  WHITE borders, channel order **RGB→BGR** (WD taggers are cv2-trained), **float32 in
  0..255** (no `0..1` normalization), layout **NHWC**.
- **Postprocess**: per-class sigmoid is in-graph for these ONNX exports (see the check
  above); thresholds **general ≈ 0.35**, **character ≈ 0.85**, **ratings top-1**.
  Categories: `0` general, `4` character, `9` rating.

## IO dtype note

The runtime feeds float32 and reads float32 (`qnn-rs` quantizes/dequantizes per the
tensor's own scale-offset; it has no fp16 IO path). The fp16 conversion above keeps IO
tensors float32 while computing internally in fp16 — the same shape the Anima packs use.
Do not request quantized (int8) IO for this pack.
