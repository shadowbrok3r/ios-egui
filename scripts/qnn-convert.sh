#!/usr/bin/env bash
# D4: ONNX → quantized QNN model → HTP v81 context binary (SM8850 / S26 Ultra).
#
# Pipeline (each stage can be skipped with --from):
#   1. qnn-onnx-converter   ONNX → model.cpp + model.bin (+ optional quantize)
#   2. qnn-model-lib-generator  model.cpp → libmodel.so (x86_64 host)
#   3. qnn-context-binary-generator  libmodel.so + HTP → <name>.bin context
#   4. optional: scripts/qnn-push-model.sh to device
#
# Prerequisites (one-time):
#   - Full QAIRT zip extracted (bin/x86_64-linux-clang + lib/python), see scripts/qnn-env.sh
#   - Python 3.12 venv at ~/Desktop/QNN/qairt-venv with converter deps (numpy, onnx, …)
#   - Android NDK (host libc++ for context-binary-generator)
#
# Usage:
#   scripts/qnn-convert.sh --onnx unet.onnx --name unet --out ~/Desktop/QNN/build/unet
#   scripts/qnn-convert.sh --onnx vae.onnx --name vae_decoder --out ... --input-list calib.txt
#   scripts/qnn-convert.sh --onnx unet.onnx --name unet --out ... --push --tcp HOST:PORT
#
# Quantization: pass --input-list with one line per calibration sample (raw float tensors
# as expected by the converter). Without it, conversion stays float / float_fallback.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

ONNX=""
NAME=""
OUT=""
INPUT_LIST=""
FROM="onnx"   # onnx | lib | context
PUSH=0
TCP=""
DSP_ARCH="${DSP_ARCH:-v81}"
SOC_MODEL="${SOC_MODEL:-sm8850}"
VTCM_MB="${VTCM_MB:-8}"
FLOAT_FALLBACK=0

while [ $# -gt 0 ]; do
  case "$1" in
    --onnx) ONNX="$2"; shift 2 ;;
    --name) NAME="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --input-list) INPUT_LIST="$2"; shift 2 ;;
    --from) FROM="$2"; shift 2 ;;
    --push) PUSH=1; shift ;;
    --tcp) TCP="$2"; shift 2 ;;
    --dsp-arch) DSP_ARCH="$2"; shift 2 ;;
    --soc-model) SOC_MODEL="$2"; shift 2 ;;
    --vtcm-mb) VTCM_MB="$2"; shift 2 ;;
    --float-fallback) FLOAT_FALLBACK=1; shift ;;
    -h|--help)
      sed -n '2,21p' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

[ -n "$NAME" ] || { echo "--name required (output basename, e.g. unet)" >&2; exit 1; }
[ -n "$OUT" ] || { echo "--out required (work directory)" >&2; exit 1; }
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

# shellcheck disable=SC1091
source "$HERE/qnn-env.sh"

BIN_DIR="$QAIRT_ROOT/bin/x86_64-linux-clang"
LIB_DIR="$QAIRT_ROOT/lib/x86_64-linux-clang"
HTP_BACKEND="$LIB_DIR/libQnnHtp.so"
[ -f "$HTP_BACKEND" ] || { echo "missing $HTP_BACKEND" >&2; exit 1; }

# Cursor AppImage rewrites argv0 when paths contain "3.12"; always launch via ld-linux.
run_py() {
  /lib64/ld-linux-x86-64.so.2 "$QAIRT_VENV/bin/python" "$@"
}

# Stage order: onnx → lib → context. --from skips earlier stages.
stage_rank() {
  case "$1" in onnx) echo 0 ;; lib) echo 1 ;; context) echo 2 ;; *) echo -1 ;; esac
}
FROM_RANK="$(stage_rank "$FROM")"
[ "$FROM_RANK" -ge 0 ] || { echo "bad --from $FROM (onnx|lib|context)" >&2; exit 1; }
stage_ge() { [ "$(stage_rank "$1")" -ge "$FROM_RANK" ]; }

# --- 1. ONNX → QNN C++/bin ---------------------------------------------------
CPP="$OUT/${NAME}.cpp"
WEIGHTS="$OUT/${NAME}.bin"
if stage_ge onnx; then
  [ -f "$ONNX" ] || { echo "--onnx file missing: $ONNX" >&2; exit 1; }
  echo "==> [1/3] qnn-onnx-converter → $CPP"
  CONV_ARGS=(
    --input_network "$ONNX"
    --output_path "$OUT/$NAME"
    --float_bitwidth 16
  )
  if [ -n "$INPUT_LIST" ]; then
    [ -f "$INPUT_LIST" ] || { echo "--input-list missing: $INPUT_LIST" >&2; exit 1; }
    CONV_ARGS+=(--input_list "$INPUT_LIST" --use_per_channel_quantization)
  elif [ "$FLOAT_FALLBACK" -eq 1 ]; then
    CONV_ARGS+=(--float_fallback)
  else
    echo "note: no --input-list; using --float_fallback (pass calib list for int8)" >&2
    CONV_ARGS+=(--float_fallback)
  fi
  run_py "$BIN_DIR/qnn-onnx-converter" "${CONV_ARGS[@]}"
  [ -f "$CPP" ] || { echo "converter did not write $CPP" >&2; exit 1; }
fi

# --- 2. C++ → host model .so -------------------------------------------------
MODEL_SO="$OUT/x86_64-linux-clang/lib${NAME}.so"
if stage_ge lib; then
  [ -f "$CPP" ] || { echo "missing $CPP (run from onnx or produce it)" >&2; exit 1; }
  echo "==> [2/3] qnn-model-lib-generator → $MODEL_SO"
  LIBGEN_ARGS=(-c "$CPP" -o "$OUT" -t x86_64-linux-clang -l "$NAME")
  [ -f "$WEIGHTS" ] && LIBGEN_ARGS+=(-b "$WEIGHTS")
  # generator is a Python wrapper
  run_py "$BIN_DIR/qnn-model-lib-generator" "${LIBGEN_ARGS[@]}"
  # layout varies slightly by SDK version
  if [ ! -f "$MODEL_SO" ]; then
    FOUND="$(find "$OUT" -name "lib${NAME}.so" | head -n1 || true)"
    [ -n "$FOUND" ] || { echo "model .so not found under $OUT" >&2; exit 1; }
    MODEL_SO="$FOUND"
  fi
fi

# --- 3. HTP context binary ---------------------------------------------------
CTX_BIN="$OUT/${NAME}_ctx.bin"
HTP_CFG="$OUT/htp_${DSP_ARCH}.json"
BACKEND_CFG="$OUT/backend_extensions.json"
if stage_ge context; then
  [ -f "$MODEL_SO" ] || { echo "missing model .so" >&2; exit 1; }
  echo "==> [3/3] qnn-context-binary-generator ($SOC_MODEL / $DSP_ARCH) → $CTX_BIN"

  # Optional backend-extensions JSON (graph VTCM / perf). --htp_socs drives offline cache SoC.
  cat > "$HTP_CFG" <<EOF
{
  "graphs": [
    {
      "graph_names": ["${NAME}"],
      "vtcm_mb": ${VTCM_MB},
      "fp16_relaxed_precision": 1,
      "O": 3.0
    }
  ],
  "devices": [
    {
      "dsp_arch": "${DSP_ARCH}",
      "cores": [
        {
          "core_id": 0,
          "perf_profile": "burst",
          "rpc_control_latency": 100
        }
      ]
    }
  ]
}
EOF

  GEN_ARGS=(
    --model "$MODEL_SO"
    --backend "$HTP_BACKEND"
    --binary_file "$CTX_BIN"
    --output_dir "$OUT"
    --htp_socs "$SOC_MODEL"
    --vtcm_override "$VTCM_MB"
  )
  EXT_SO="$LIB_DIR/libQnnHtpNetRunExtensions.so"
  if [ -f "$EXT_SO" ]; then
    cat > "$BACKEND_CFG" <<EOF
{
  "backend_extensions": {
    "shared_library_path": "${EXT_SO}",
    "config_file_path": "${HTP_CFG}"
  }
}
EOF
    GEN_ARGS+=(--config_file "$BACKEND_CFG")
  else
    echo "warn: no libQnnHtpNetRunExtensions.so — generating with --htp_socs only" >&2
  fi

  "$BIN_DIR/qnn-context-binary-generator" "${GEN_ARGS[@]}"
  # generator may append .bin or place under output_dir
  if [ ! -f "$CTX_BIN" ]; then
    ALT="$OUT/$(basename "$CTX_BIN")"
    [ -f "$ALT" ] && CTX_BIN="$ALT"
  fi
  if [ ! -f "$CTX_BIN" ]; then
    FOUND="$(find "$OUT" -maxdepth 2 -name '*.bin' ! -name "${NAME}.bin" | head -n1 || true)"
    [ -n "$FOUND" ] && CTX_BIN="$FOUND"
  fi
  [ -f "$CTX_BIN" ] || { echo "context binary not produced under $OUT" >&2; exit 1; }
  # App expects unet.bin / vae_decoder.bin names
  FINAL="$OUT/${NAME}.ctx.bin"
  cp -f "$CTX_BIN" "$FINAL"
  echo "context binary: $FINAL ($(du -h "$FINAL" | cut -f1))"
fi

if [ "$PUSH" -eq 1 ]; then
  DEST_NAME="${NAME}.bin"
  PUSH_ARGS=("$FINAL")
  # copy to expected runtime name in a temp? push script keeps basename — stage rename
  STAGE="$OUT/_push"
  mkdir -p "$STAGE"
  cp -f "$FINAL" "$STAGE/$DEST_NAME"
  PUSH_CMD=("$HERE/qnn-push-model.sh")
  [ -n "$TCP" ] && PUSH_CMD+=(--tcp "$TCP")
  PUSH_CMD+=("$STAGE/$DEST_NAME")
  echo "==> push $DEST_NAME"
  "${PUSH_CMD[@]}"
fi

echo "done. artifacts in $OUT"
echo "  next: rename/copy ${NAME}.ctx.bin → device as ${NAME}.bin (see scripts/qnn-push-model.sh)"
echo "  target SoC: $SOC_MODEL / HTP $DSP_ARCH"
