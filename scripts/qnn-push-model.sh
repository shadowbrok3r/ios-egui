#!/usr/bin/env bash
# Push QNN context binaries (unet.bin, optionally vae_*.bin) to the ComfyUI app's model dir.
# Model data is read-only at runtime, so it needs no exec bit and rides on /sdcard (unlike the
# .so, which must be bundled — see scripts/qnn-stage-libs.sh). adb (shell UID) can write these
# paths; the owning app reads app-specific files with no runtime permission.
#
# Destinations:
#   default: /sdcard/Android/data/<pkg>/files/<subdir>  (wiped on uninstall)
#   --durable: /sdcard/ComfyUI/<subdir>                 (survives uninstall; app scans both)
#
# Usage:
#   scripts/qnn-push-model.sh [--tcp HOST:PORT] [--serial SERIAL] [--pkg com.example.comfyui] FILE [FILE...]
#   scripts/qnn-push-model.sh --durable --subdir anima_nova DIR_OR_FILES...
#   scripts/qnn-push-model.sh --tcp 100.89.76.57:36069 ~/models/sd15/unet.bin
set -euo pipefail

PKG="com.example.comfyui"
SUBDIR="qnn"
TCP=""
SERIAL=""
DURABLE=0
FILES=()
while [ $# -gt 0 ]; do
  case "$1" in
    --tcp) TCP="$2"; shift 2 ;;
    --serial|-s) SERIAL="$2"; shift 2 ;;
    --pkg) PKG="$2"; shift 2 ;;
    --subdir) SUBDIR="$2"; shift 2 ;;
    --durable) DURABLE=1; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) FILES+=("$1"); shift ;;
  esac
done
[ "${#FILES[@]}" -gt 0 ] || { echo "no files given; pass at least a unet.bin path" >&2; exit 1; }

HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
ADB="$SDK/platform-tools/adb"; [ -x "$ADB" ] || ADB="adb"
# shellcheck source=adb-target.sh
source "$HERE/adb-target.sh"
adb_resolve_target

if [ "$DURABLE" -eq 1 ]; then
  DEST="/sdcard/ComfyUI/$SUBDIR"
else
  DEST="/sdcard/Android/data/$PKG/files/$SUBDIR"
fi
"${ADBS[@]}" shell mkdir -p "$DEST"
for f in "${FILES[@]}"; do
  if [ -d "$f" ]; then
    echo "push dir $(basename "$f") ($(du -sh "$f" | cut -f1)) -> $DEST/"
    "${ADBS[@]}" push "$f/." "$DEST/"
  else
    [ -f "$f" ] || { echo "missing: $f" >&2; exit 1; }
    echo "push $(basename "$f") ($(du -h "$f" | cut -f1)) -> $DEST/"
    "${ADBS[@]}" push "$f" "$DEST/$(basename "$f")"
  fi
done
echo "device model dir: $DEST"
"${ADBS[@]}" shell ls -l "$DEST"
