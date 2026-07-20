# CLIP visual + aesthetic + text export → `local-clip` HTP pack

`export.py` turns the **CLIP ViT-B/32 visual tower** ONNX into a Qualcomm HTP "pack"
that the on-device `local-clip` runtime loads for image embeddings and (optionally) a
LAION aesthetic score. It downloads the model, forces a static input shape, trims the
graph to the single embedding output, runs the QAIRT/QNN converters, and assembles the
pack directory. With `--text` it also builds the **text tower** (`text_model.bin` +
`tokenizer.json`) into the same pack, which enables typed semantic gallery search.

The script must run **on the machine that has the QAIRT/QNN SDK** (the converters are
x86-64 host tools). It never runs the SDK here in-repo; each SDK stage checks that its
tool exists and fails with a clear message otherwise.

## Model

- Vision tower: **`Xenova/clip-vit-base-patch32`**, file `onnx/vision_model.onnx` (default
  `--repo`). This is `CLIPVisionModelWithProjection`: input `pixel_values`
  `[1,3,224,224]` NCHW, outputs `image_embeds` `[1,512]` (the projected embedding) **and**
  `last_hidden_state`. No torch needed for the vision tower.
- Fallback source: `openai/clip-vit-base-patch32` exported with
  [optimum](https://github.com/huggingface/optimum)
  (`optimum-cli export onnx --model openai/clip-vit-base-patch32 ...`), then point `--repo`
  at a local dir / adjust the fetch. Confirm the projected output is `image_embeds`.
- Text tower (`--text`): file `onnx/text_model.onnx` (`CLIPTextModelWithProjection`), inputs
  `input_ids` / `attention_mask` `[1,77]` (int64 in ONNX), output `text_embeds` `[1,512]` (the
  projected embedding) **and** `last_hidden_state`. The tokenizer is fetched from
  `tokenizer.json` in the same repo. No torch needed.

### int64 → int32 inputs (text)

The QAIRT converter rejects int64 graph inputs, so the shape stage declares `input_ids` /
`attention_mask` as **int32** and prepends a `Cast(int32→int64)` whose output reuses the
original input name (consumers unchanged). On device qnn-rs feeds these two inputs via its
`TensorIn::I32` path. The graph pools at the first EOS via an internal argmax over
`input_ids`, so the runtime keeps BOS `49406` first, EOS `49407` last, and pads with the EOS
id (attention_mask 0) — matching HF CLIP.

### Output trimming

The Rust runtime reads the graph's **sole** output. When the ONNX emits both
`image_embeds` and `last_hidden_state`, stage 1 (`shape`) keeps only `image_embeds` via
`onnx.utils.extract_model`. The pack contract's output is therefore the **512-d projected
image embedding** (`image_embeds`). If a variant lacks that name, the script falls back to
the first rank-2 `[1,N]` output and warns. The text tower is trimmed the same way to
`text_embeds` (same fallback).

### Aesthetic head (`--aesthetic`)

`sa_0_4_vit_b_32_linear.pth` from
[LAION-AI/aesthetic-predictor](https://github.com/LAION-AI/aesthetic-predictor) is a single
`Linear(512→1)` trained on B/32 embeddings. With `--aesthetic` the script loads it (needs
**torch**; it fails with a clear message if absent) and writes `aesthetic.bin`:
little-endian f32 `[w0..w511, bias]` (513 floats).

**Contract**: `score = dot(w, emb) + b` where `emb` is the **L2-normalized** CLIP
embedding. This matches the LAION reference, which normalizes before the head:

```python
image_features = model.encode_image(image)
image_features /= image_features.norm(dim=-1, keepdim=True)   # L2 normalize
prediction = amodel(image_features)
```

`local-clip`'s device helpers return the embedding already L2-normalized, and
`aesthetic_score(head, emb)` applies `dot(w, emb) + b`.

## Prerequisites

- The QAIRT/QNN SDK extracted (needs `bin/x86_64-linux-clang/qairt-converter`,
  `qnn-context-binary-generator`, and `lib/x86_64-linux-clang/libQnnHtp.so` +
  `libQnnModelDlc.so`). Point at it with `--sdk` or `QNN_SDK_ROOT`. The script goes
  ONNX -> DLC -> context binary, so it needs no host compiler and works on SDK
  extractions without `share/QNN`.
- The script sets PYTHONPATH/LD_LIBRARY_PATH for the SDK tools itself; sourcing
  `scripts/qnn-env.sh` first is optional.
- A Python with the converter deps and `onnx` **< 1.19** (QAIRT 2.48's converters crash
  on newer: `uv pip install --python <venv>/bin/python 'onnx==1.18.0'`); optionally
  `huggingface_hub` for the download (else a direct HTTPS pull). `torch` is needed only
  for `--aesthetic`.

## Run

```sh
<qairt-venv>/bin/python scripts/clip-export/export.py \
  --out ~/clip --work ~/clip-build --sdk ~/Desktop/QNN/qairt/<ver> --aesthetic --text
```

`--aesthetic` and `--text` are independent add-ons; pass either, both, or neither. `--text`
runs through the same stages (a separate `clip_text.dlc` + context binary under
`<work>/text_ctx/`) and appends `text_model.bin` + `tokenizer.json` to the pack.

Defaults target an **older-arch V73** HTP build (`--dsp-arch v73 --soc sm8550`). V73
context binaries run fine on newer HTP (V81 / S26 Ultra); building for the older arch
keeps one artifact working across devices. Override for a native V81 build:

```sh
python3 scripts/clip-export/export.py --dsp-arch v81 --soc sm8850 --out ~/clip
```

Skip already-done stages with `--from` (`download shape convert lib context pack`):

```sh
python3 scripts/clip-export/export.py --from context --out ~/clip --work ~/clip-build
```

### Precision

`--float_bitwidth 16` (fp16) is the only precision flag, and nothing else is passed.
The graph's IO tensors come out fp16 too — qnn-rs converts f32↔f16 at the tensor
boundary (`quant::f32_to_f16` / `f16_to_f32`), so this needs nothing on the app side.
Do **not** add `--preserve_io_datatype` (fails HTP finalize, `q::QNN_Convert`) or
`--quantization_overrides` (segfaults qairt-converter 2.48).

### Layout note

CLIP vision is **NCHW** (`1×3×224×224`) — unlike WD14's NHWC. The ONNX carries the NCHW
layout and qairt-converter honors it, so no layout flag is passed by default. If the
converter complains about layout, pass the SDK's flag through, e.g.:

```sh
... --convert-arg --desired_input_layout --convert-arg pixel_values --convert-arg NCHW
```

## Pack layout it emits

```
<out>/                 e.g. ~/clip
├── CLIPV              marker file (empty; identifies the pack)
├── model.bin          the HTP context binary (the visual tower graph)
├── aesthetic.bin      optional; f32 LE [w0..w511, bias]  (only with --aesthetic)
├── text_model.bin     optional; the HTP context binary (the text tower graph)  (only with --text)
└── tokenizer.json     optional; CLIP tokenizer paired with text_model.bin       (only with --text)
```

Push it next to the existing anima / sd / wd14 packs. The durable location the app scans is
`/storage/emulated/0/ComfyUI/clip` (survives uninstall); the app's external files dir works too:

```sh
adb push ~/clip /storage/emulated/0/ComfyUI/clip
# or the app-private files dir:
adb push ~/clip /sdcard/Android/data/com.example.comfyui/files/clip
```

On device the app scans those dirs; a `CLIPV`-marked subdir enables CLIP embeddings (and the
aesthetic score if `aesthetic.bin` is present). Typed **semantic gallery search** additionally
needs **both** `text_model.bin` and `tokenizer.json` present; without them the Tags menu shows a
dim line saying so. CLIP packs are a distinct pack kind, discovered independently of the SD /
Anima / WD14 packs.

## Preprocessing contract

The graph is fed exactly what `crates/local-clip` produces, so the export input must be
NCHW `1×3×224×224`:

- **Preprocess**: RGB (alpha dropped), resize the **shortest side to 224** (bilinear),
  **centre-crop 224×224**, scale to `0..1`, normalize with CLIP means
  `[0.48145466, 0.4578275, 0.40821073]` / stds `[0.26862954, 0.26130258, 0.27577711]`,
  layout **NCHW** (channel-planar) float32.
- **Postprocess**: the 512-d `image_embeds` output is **L2-normalized** by the runtime;
  `cosine(a, b)` is then a plain dot product, and the aesthetic head applies
  `dot(w, emb) + b`.

## IO dtype note

The runtime feeds float32 and reads float32; the fp16 conversion keeps IO tensors as
float, computing internally in fp16 (qnn-rs handles the f32↔f16 boundary). Do not request
quantized (int8) IO for this pack.
