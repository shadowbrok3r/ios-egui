#!/usr/bin/env bash
# Source this to put QAIRT host tools + a Python 3.12 venv on PATH for D4 conversion.
#
#   source scripts/qnn-env.sh
#   qnn-onnx-converter --help
#   qnn-context-binary-generator --help
#
# Defaults expect the SDK at ~/Desktop/QNN/qairt/<ver> (extracted from the full zip,
# including bin/x86_64-linux-clang and lib/python). Host libc++ comes from the NDK
# (staged under ~/Desktop/QNN/host-runtime). Python deps live in ~/Desktop/QNN/qairt-venv.
#
# Cursor's AppImage rewrites argv0 when a path contains "3.12"; converters are invoked
# via scripts/qnn-convert.sh which uses ld-linux to avoid that. Interactive shells can
# call qnn_py <script> ... as a helper.

# Must be sourced (bash or zsh) so exports persist.
if ! (return 0 2>/dev/null); then
  echo "source this file: source scripts/qnn-env.sh" >&2
  exit 1
fi

QAIRT_ROOT="${QAIRT_ROOT:-$HOME/Desktop/QNN/qairt/2.48.40.260702}"
QAIRT_VENV="${QAIRT_VENV:-$HOME/Desktop/QNN/qairt-venv}"
QAIRT_HOST_RT="${QAIRT_HOST_RT:-$HOME/Desktop/QNN/host-runtime}"
ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-}"

if [ ! -d "$QAIRT_ROOT/bin/x86_64-linux-clang" ]; then
  echo "missing $QAIRT_ROOT/bin/x86_64-linux-clang — extract from the full QAIRT zip" >&2
  return 1 2>/dev/null || exit 1
fi
if [ ! -x "$QAIRT_VENV/bin/python" ]; then
  echo "missing $QAIRT_VENV — create with: uv venv --python 3.12 \$HOME/Desktop/QNN/qairt-venv" >&2
  return 1 2>/dev/null || exit 1
fi

# Stage NDK host libc++/unwind once if the runtime dir is empty.
if [ ! -e "$QAIRT_HOST_RT/libc++.so.1" ]; then
  NDK="$ANDROID_NDK_HOME"
  if [ -z "$NDK" ]; then
    SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
    NDK="$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -n1 || true)"
  fi
  NDK_LIB="$NDK/toolchains/llvm/prebuilt/linux-x86_64/lib/x86_64-unknown-linux-gnu"
  if [ -f "$NDK_LIB/libc++.so.1" ]; then
    mkdir -p "$QAIRT_HOST_RT"
    ln -sfn "$NDK_LIB/libc++.so.1" "$QAIRT_HOST_RT/libc++.so.1"
    ln -sfn "$NDK_LIB/libc++abi.so.1" "$QAIRT_HOST_RT/libc++abi.so.1"
    ln -sfn "$NDK_LIB/libunwind.so" "$QAIRT_HOST_RT/libunwind.so.1"
  else
    echo "warn: no NDK host libc++ at $NDK_LIB — install libc++ or set ANDROID_NDK_HOME" >&2
  fi
fi

PY_ROOT="$(cd "$(dirname "$(readlink -f "$QAIRT_VENV/bin/python")")/.." && pwd)"
export QAIRT_ROOT QNN_SDK_ROOT="$QAIRT_ROOT" QAIRT_VENV QAIRT_HOST_RT
export PYTHONPATH="$QAIRT_ROOT/lib/python${PYTHONPATH:+:$PYTHONPATH}"
export PATH="$QAIRT_ROOT/bin/x86_64-linux-clang:$QAIRT_VENV/bin:$PATH"
export LD_LIBRARY_PATH="$QAIRT_HOST_RT:$QAIRT_ROOT/lib/x86_64-linux-clang:$PY_ROOT/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export VIRTUAL_ENV="$QAIRT_VENV"

# Run a Python entrypoint without Cursor rewriting argv0 on paths containing 3.12.
qnn_py() {
  /lib64/ld-linux-x86-64.so.2 "$QAIRT_VENV/bin/python" "$@"
}

echo "[qnn-env] QAIRT_ROOT=$QAIRT_ROOT"
echo "[qnn-env] QAIRT_VENV=$QAIRT_VENV"
echo "[qnn-env] use: qnn_py \$QAIRT_ROOT/bin/x86_64-linux-clang/qnn-onnx-converter --help"
