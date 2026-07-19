#!/usr/bin/env bash
# Stage the proprietary QNN/QAIRT HTP libraries for APK bundling (qnn-rs D1).
# Copies the arm64 device .so from a QAIRT SDK plus the NDK libc++_shared.so into
# examples/comfyui-android/qnn-runtime-libs/arm64-v8a/, the folder cargo-apk2 bundles when
# `[package.metadata.android] runtime_libs = "qnn-runtime-libs"` is enabled. The .so must ship in
# the APK's nativeLibraryDir — pushed .so cannot be dlopen'd (W^X + SELinux on non-rooted API24+).
#
# The QNN libraries are Qualcomm-proprietary and are NEVER committed; the destination folder gets a
# .gitignore that excludes everything.
#
# Usage:
#   QAIRT_ROOT=~/Desktop/QNN/qairt/2.48.40.260702 ANDROID_NDK_HOME=... scripts/qnn-stage-libs.sh
set -euo pipefail

QAIRT_ROOT="${QAIRT_ROOT:-$HOME/Desktop/QNN/qairt/2.48.40.260702}"
HTP_ARCH="${HTP_ARCH:-hexagon-v81}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$HERE/examples/comfyui-android/qnn-runtime-libs/arm64-v8a"

AA="$QAIRT_ROOT/lib/aarch64-android"
SKEL_DIR="$QAIRT_ROOT/lib/$HTP_ARCH/unsigned"
[ -d "$AA" ] || { echo "no $AA — set QAIRT_ROOT to an extracted qairt/<version>" >&2; exit 1; }
[ -d "$SKEL_DIR" ] || { echo "no $SKEL_DIR — check HTP_ARCH (got $HTP_ARCH)" >&2; exit 1; }

# Resolve the NDK's arm64 libc++_shared.so (QNN links the C++ runtime).
NDK="${ANDROID_NDK_HOME:-}"
if [ -z "$NDK" ]; then
  SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
  NDK="$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -n1 || true)"
fi
[ -n "$NDK" ] && [ -d "$NDK" ] || { echo "no NDK — set ANDROID_NDK_HOME" >&2; exit 1; }
LIBCXX="$NDK/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so"
[ -f "$LIBCXX" ] || { echo "no libc++_shared.so at $LIBCXX" >&2; exit 1; }

mkdir -p "$DEST"
printf '*\n!.gitignore\n' > "$DEST/../.gitignore"

copy() { install -m 0644 "$1" "$DEST/$(basename "$1")" && echo "  $(basename "$1")"; }

echo "staging QNN libs -> $DEST"
copy "$AA/libQnnHtp.so"
copy "$AA/libQnnSystem.so"
copy "$AA/libQnnHtpV81Stub.so"
[ -f "$AA/libQnnHtpPrepare.so" ] && copy "$AA/libQnnHtpPrepare.so" || true
copy "$SKEL_DIR/libQnnHtpV81Skel.so"
copy "$LIBCXX"

echo "done. Enable bundling in examples/comfyui-android/Cargo.toml:"
echo "  [package.metadata.android] runtime_libs = \"qnn-runtime-libs\""
echo "  [package.metadata.android.application] extract_native_libs = true"
