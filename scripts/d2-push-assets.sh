#!/usr/bin/env bash
# Push D2 text2img assets (unet + vae_decoder + tokenizer + CLIP) to the app files dir.
# Defaults point at a local QAIRT model tree + the HF SD1.5 text encoder.
#
# Usage:
#   scripts/d2-push-assets.sh [--tcp HOST:PORT] [--pkg com.example.comfyui] [--durable]
#   MODEL_DIR=... CLIP=... scripts/d2-push-assets.sh --tcp 100.89.76.57:36069
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
MODEL_DIR="${MODEL_DIR:-$HOME/Desktop/QNN/models/AnythingV5_8gen2/output_512/qnn_models_8gen2}"
CLIP="${CLIP:-$HOME/Desktop/QNN/models/clip-text-sd15/model.safetensors}"
TCP=""
PKG=""
EXTRA=()
while [ $# -gt 0 ]; do
  case "$1" in
    --tcp) TCP="$2"; shift 2 ;;
    --pkg) PKG="$2"; shift 2 ;;
    --durable) EXTRA+=(--durable); shift ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) EXTRA+=("$1"); shift ;;
  esac
done

for f in "$MODEL_DIR/unet.bin" "$MODEL_DIR/vae_decoder.bin" "$MODEL_DIR/tokenizer.json" "$CLIP"; do
  [ -f "$f" ] || { echo "missing: $f" >&2; exit 1; }
done

# Stage CLIP as clip.safetensors so the app can look for a fixed name.
STAGED="$(mktemp -d)"
cp -f "$CLIP" "$STAGED/clip.safetensors"

ARGS=()
[ -n "$TCP" ] && ARGS+=(--tcp "$TCP")
[ -n "$PKG" ] && ARGS+=(--pkg "$PKG")
"$HERE/scripts/qnn-push-model.sh" "${ARGS[@]}" \
  "$MODEL_DIR/unet.bin" \
  "$MODEL_DIR/vae_decoder.bin" \
  "$MODEL_DIR/tokenizer.json" \
  "$STAGED/clip.safetensors" \
  "${EXTRA[@]}"

rm -rf "$STAGED"
