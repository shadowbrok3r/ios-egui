#!/usr/bin/env bash
# Push QNN context binaries (unet.bin, optionally vae_*.bin) to the ComfyUI app's external files
# dir on the device. Model data is read-only at runtime, so it needs no exec bit and rides on
# /sdcard (unlike the .so, which must be bundled — see scripts/qnn-stage-libs.sh). adb (shell UID)
# can write /sdcard/Android/data/<pkg>/files; the owning app reads it with no runtime permission.
#
# Usage:
#   scripts/qnn-push-model.sh [--tcp HOST:PORT] [--pkg com.example.comfyui] FILE [FILE...]
#   scripts/qnn-push-model.sh --tcp 100.89.76.57:36069 ~/models/sd15/unet.bin
set -euo pipefail

PKG="com.example.comfyui"
SUBDIR="qnn"
TCP=""
FILES=()
while [ $# -gt 0 ]; do
  case "$1" in
    --tcp) TCP="$2"; shift 2 ;;
    --pkg) PKG="$2"; shift 2 ;;
    --subdir) SUBDIR="$2"; shift 2 ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) FILES+=("$1"); shift ;;
  esac
done
[ "${#FILES[@]}" -gt 0 ] || { echo "no files given; pass at least a unet.bin path" >&2; exit 1; }

SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
ADB="$SDK/platform-tools/adb"; [ -x "$ADB" ] || ADB="adb"
ADBS=("$ADB")
if [ -n "$TCP" ]; then
  case "$TCP" in *:*) : ;; *) TCP="$TCP:5555" ;; esac
  "$ADB" connect "$TCP" >/dev/null
  ADBS=("$ADB" -s "$TCP")
fi

DEST="/sdcard/Android/data/$PKG/files/$SUBDIR"
"${ADBS[@]}" shell mkdir -p "$DEST"
for f in "${FILES[@]}"; do
  [ -f "$f" ] || { echo "missing: $f" >&2; exit 1; }
  echo "push $(basename "$f") ($(du -h "$f" | cut -f1)) -> $DEST/"
  "${ADBS[@]}" push "$f" "$DEST/$(basename "$f")"
done
echo "device model dir: $DEST"
"${ADBS[@]}" shell ls -l "$DEST"
