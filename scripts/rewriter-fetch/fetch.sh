#!/usr/bin/env bash
# Assemble a `local-rewrite` pack: download the quantized Qwen2.5-0.5B-Instruct GGUF and its
# tokenizer from Hugging Face into an `RWTR`-marked pack dir the on-device runtime loads.
#
# Usage:
#   scripts/rewriter-fetch/fetch.sh [OUT_DIR]
# Env overrides:
#   GGUF_REPO  GGUF_FILE  TOK_REPO  TOK_FILE  HF_ENDPOINT
set -euo pipefail

OUT="${1:-./rewrite}"

# Qwen ships the GGUF and the tokenizer in separate repos (the GGUF repo has no tokenizer.json).
GGUF_REPO="${GGUF_REPO:-Qwen/Qwen2.5-0.5B-Instruct-GGUF}"
GGUF_FILE="${GGUF_FILE:-qwen2.5-0.5b-instruct-q4_k_m.gguf}"
TOK_REPO="${TOK_REPO:-Qwen/Qwen2.5-0.5B-Instruct}"
TOK_FILE="${TOK_FILE:-tokenizer.json}"
HF_ENDPOINT="${HF_ENDPOINT:-https://huggingface.co}"

mkdir -p "$OUT"

# Marker file identifies the pack (see crates/local-rewrite/src/pack.rs).
: > "$OUT/RWTR"

fetch() {
  # fetch <repo> <remote-file> <local-path>
  local url="$HF_ENDPOINT/$1/resolve/main/$2"
  echo "-> $url"
  curl -fL --retry 3 -C - -o "$3" "$url"
}

fetch "$GGUF_REPO" "$GGUF_FILE" "$OUT/model.gguf"
fetch "$TOK_REPO" "$TOK_FILE" "$OUT/tokenizer.json"

# A GGUF starts with the ASCII magic "GGUF"; catch an HTML error page saved as the model.
magic="$(head -c 4 "$OUT/model.gguf")"
if [ "$magic" != "GGUF" ]; then
  echo "ERROR: $OUT/model.gguf is not a GGUF (got magic '$magic'). Check GGUF_FILE." >&2
  exit 1
fi

echo "Pack ready: $OUT"
ls -la "$OUT"
echo
echo "Push it to the durable models root on device:"
echo "  adb push $OUT /storage/emulated/0/ComfyUI/rewrite"
