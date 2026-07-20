# Rewriter fetch → `local-rewrite` pack

`fetch.sh` assembles a **rewrite pack**: the quantized LLM and tokenizer the on-device
`local-rewrite` runtime loads to power the app's **Rewrite** menu (tags ↔ prose, model-family
dialect swaps). Unlike the WD14/CLIP exports, this needs no QAIRT SDK — it just downloads two
files from Hugging Face and drops a marker.

## Model

- **`Qwen/Qwen2.5-0.5B-Instruct-GGUF`** → `qwen2.5-0.5b-instruct-q4_k_m.gguf` (~400 MB).
  candle-transformers 0.11 ships a `quantized_qwen2` arch, so this Qwen2 GGUF loads directly —
  no llama-arch fallback needed.
- Tokenizer comes from the base repo **`Qwen/Qwen2.5-0.5B-Instruct`** → `tokenizer.json`
  (the GGUF repo has no tokenizer.json).

Runs on the device CPU via candle; there is no HTP/NPU path for this crate.

## Run

```sh
scripts/rewriter-fetch/fetch.sh ~/rewrite
```

Overrides (env vars): `GGUF_REPO`, `GGUF_FILE`, `TOK_REPO`, `TOK_FILE`, `HF_ENDPOINT`. To try a
llama-arch fallback model, point `GGUF_REPO`/`GGUF_FILE` at a Llama-3.2-1B-Instruct GGUF — but
note the runtime currently only wires the `qwen2` arch, so a llama GGUF is rejected until
`crates/local-rewrite/src/rewriter.rs` adds `quantized_llama`.

The script needs only `curl`. It verifies the downloaded `model.gguf` starts with the `GGUF`
magic to catch an HTML error page saved in its place.

## Pack layout it emits

```
<out>/                 e.g. ~/rewrite
├── RWTR               marker file (empty; identifies the pack)
├── model.gguf         quantized Qwen2.5-0.5B-Instruct (q4_k_m)
└── tokenizer.json     the Qwen2.5 tokenizer
```

## On-device location

Push the pack to the durable ComfyUI models root — the app scans it (alongside the app external
files dir) via `find_rewrite_pack_many`, the same discovery used for WD14/CLIP packs:

```sh
adb push ~/rewrite /storage/emulated/0/ComfyUI/rewrite
```

`/storage/emulated/0/ComfyUI` survives app uninstall (sideload; grant All files access). A
subdir carrying the `RWTR` marker enables the **Rewrite** menu in the Create pane's prompt
composer (feature `local-npu`). The app also scans its own external files dir
(`/sdcard/Android/data/com.example.comfyui/files/`), so a pack pushed there works too.

## What the runtime does with it

`local_rewrite::Rewriter::open(dir)` loads `model.gguf` (CPU) + `tokenizer.json`, then
`rewrite(system, user, max_tokens)` greedy-decodes an assistant reply using the Qwen2.5 chat
template, stopping on `<|im_end|>`/`<|endoftext|>`. The builtin system prompts
(`RewriteKind::{TagsToVideo, ProseToTags, ToPony, ToIllustrious}`) live in
`crates/local-rewrite/src/templates.rs`.
