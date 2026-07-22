#!/usr/bin/env bash
# Drive the on-device D1 NPU self-test and read its diagnostic back over adb logcat.
# Assumes the app is built + installed (cargo egui-mobile run ...) and the app-side hook logs the
# report via the comfyui logger (target "comfyui", prefixed "D1-SELFTEST").
#
# Flow: connect (wireless adb) -> clear log -> launch app -> stream the app/QNN log tags.
# The self-test itself is triggered in-app (Settings -> Diagnostics -> "Run D1 NPU self-test");
# pass --tap "X Y" to inject the tap, or trigger it by hand on the device.
#
# Usage:
#   scripts/d1-selftest.sh [--tcp HOST:PORT] [--serial SERIAL] [--pkg com.example.comfyui] [--tap "X Y"] [--no-launch]
#   scripts/d1-selftest.sh --tcp 100.89.76.57:36069
set -euo pipefail

PKG="com.example.comfyui"
ACTIVITY="com.github.egui_mobile.EguiNativeActivity"
TCP=""
SERIAL=""
TAP=""
LAUNCH=1
while [ $# -gt 0 ]; do
  case "$1" in
    --tcp) TCP="$2"; shift 2 ;;
    --serial|-s) SERIAL="$2"; shift 2 ;;
    --pkg) PKG="$2"; shift 2 ;;
    --tap) TAP="$2"; shift 2 ;;
    --no-launch) LAUNCH=0; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
ADB="$SDK/platform-tools/adb"; [ -x "$ADB" ] || ADB="adb"
# shellcheck source=adb-target.sh
source "$HERE/adb-target.sh"
adb_resolve_target

"${ADBS[@]}" get-state >/dev/null 2>&1 || { echo "device offline; check --tcp / --serial / wireless debugging" >&2; exit 1; }
"${ADBS[@]}" logcat -c || true
if [ "$LAUNCH" -eq 1 ]; then
  echo "launch $PKG/$ACTIVITY"
  "${ADBS[@]}" shell am start -n "$PKG/$ACTIVITY" >/dev/null
  sleep 2
fi
if [ -n "$TAP" ]; then
  echo "input tap $TAP"
  # shellcheck disable=SC2086
  "${ADBS[@]}" shell input tap $TAP
fi

echo "streaming logs (Ctrl-C to stop)."
echo "  Trigger D1: Settings -> Diagnostics -> Run D1 NPU self-test"
echo "  Trigger D2: Settings -> Diagnostics -> Run D2 text2img (8 steps)"
echo "----------------------------------------------------------------------------------------"
exec "${ADBS[@]}" logcat -s comfyui:V local_sd:V qnn_rs:V egui-android:V RustStdoutStderr:V
