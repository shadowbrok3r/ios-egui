# qnn-rs

Pure-Rust `dlopen` wrapper for Qualcomm **QNN** (AI Engine Direct / QAIRT). It
loads the QNN system/backend libraries at runtime (via `libloading`, no linking)
and, for milestone **D0**, parses a Stable Diffusion **context binary**'s
metadata — graphs, tensors, shapes, datatypes, and per-tensor scale-offset
quantization — on any host (x86_64 Linux included). On device it is the entry
point for running UNet/VAE on the Snapdragon HTP NPU (milestone D1).

## What works today (D0, host)

- `QnnSystem::load(path)` — dlopen `libQnnSystem.so`, resolve
  `QnnSystemInterface_getProviders`, pick the newest provider whose
  `QNN_SYSTEM_API_VERSION` major matches.
- `Backend::load(path)` — same for a backend lib's `QnnInterface_getProviders`
  (`libQnnHtp.so` on device, `libQnnCpu.so`/`libQnnHtp.so` on host). Exposes
  provider name / backend id / API version; its function table is kept for D1.
- `ContextBinaryInfo::parse(&system, bytes) -> ContextBinaryInfo` —
  `systemContextCreate` → `systemContextGetBinaryInfo` → walk the binary-info
  (V1/V2/V3), each graph's `QnnSystemContext_GraphInfo` (V1/V2/V3), and each
  `Qnn_Tensor_t` (V1/V2). Everything is copied into owned Rust types before
  `systemContextFree`. Returns:
  - `graphs: Vec<GraphInfo { name, inputs, outputs }>`
  - `TensorInfo { name, id, dims: Vec<u32>, dtype: DataType, quant: Option<ScaleOffset { scale, offset }> }`
  - `DataType` incl. `UFixedPoint16/8`, `Float32/16`, `SFixedPoint*`, `Int*`/`UInt*`.
  - plus `backend_id`, `build_id`, `core_api_version`, `soc_model` (V3).

### Verified output (AnythingV5 SD1.5, `qnn2.28_8gen2`, unet.bin)

```
provider: "SYSTEM_QTI_AISW"  system API v1.12.0
backend_id=6 build_id=Some("v2.28.0.241029232508_102474") core_api=v2.21.0 soc_model=Some(43)
graph "model"
  input  sample          dims=[1, 4, 64, 64]  dtype=UFixedPoint16  quant(scale=0.00014645085, offset=-33954)
  input  timestamp       dims=[1]             dtype=Int32
  input  text_embedding  dims=[1, 77, 768]    dtype=UFixedPoint16  quant(scale=0.00093294616, offset=-30096)
  output output          dims=[1, 4, 64, 64]  dtype=UFixedPoint16  quant(scale=0.00014031577, offset=-34181)
```

`vae_encoder.bin` (in `[1,3,512,512]` → mean/std `[1,4,64,64]`) and
`vae_decoder.bin` (`[1,4,64,64]` → `[1,3,512,512]`) parse the same way. Shapes,
dtypes, and the 3-input UNet contract match local-dream's `QnnModel.hpp`.

**Compatibility finding:** the **2.48.40** `libQnnSystem.so` parses a binary
built with **QNN 2.28.0** without issue (binary-info V3). No fallback to 2.48.0
was needed.

## D1 (on device): implemented

The device path is implemented against the captured backend function table
(`QnnInterface_ImplementationV2_37_t`) and compiles for both `x86_64-linux` and
`aarch64-android` (identical LP64 bindings). The HTP bring-up itself only runs on
device (NPU + skel present); on host it fails cleanly at `backendCreate`/
`contextCreateFromBinary`. No `Error::Unimplemented` remains.

- `Session::new(backend)` — unsigned-PD request → `backendCreate` →
  `deviceCreate` (if present). One session is one DSP/FastRPC session; every
  context created on it shares that backend + device, and the session is freed
  after all of them.
- `Session::load_context(system, bytes, opts)` — parse `ContextBinaryInfo`, then
  `contextCreateFromBinary` → `graphRetrieve` per graph, keeping the graph
  handles and metadata. `ContextOpts` carries `spill_fill_bytes` +
  `group_head` (HTP `REGISTER_MULTI_CONTEXTS`: co-resident contexts share one
  scratch buffer instead of each allocating its own) and `extended_udma`
  (`USE_EXTENDED_UDMA`, far-maps weights and spill/fill; v81 and above).
- `Context::max_spill_fill_size()` — `contextGetProperty` with
  `QNN_HTP_CONTEXT_GET_PROP_MAX_SPILLFILL_BUFFER_SIZE`. Returns 0 when the
  binary carries no requirement (pre-2.35 binaries, and both Anima DiT halves),
  meaning the caller must supply the group size.
- `Context::from_binary(backend, system, bytes)` — thin wrapper creating a
  private `Session` for a single context.
- `Context::execute(graph, &[(name, &[f32])]) -> HashMap<name, Vec<f32>>` —
  quantize each named input to its tensor dtype via the metadata scale-offset,
  build V2 `Qnn_Tensor_t` IO arrays (RAW client buffers, tensor `id` from
  metadata), `graphExecute`, then dequantize outputs to f32.
- `Context::execute_mixed(graph, &[(name, TensorIn)])` — same, but each input is
  `TensorIn::F32` (quantized as above) or `TensorIn::I32` (written raw, LE, in
  the tensor's own integer width, no quantization). An `I32` bound to a
  float/fixed-point tensor errors with `IntInputTypeMismatch`. Needed for token
  ids such as Anima `clip.bin`'s `t5_ids` `[1,512] Int32`.
- `Session::set_htp_performance_mode()` / `set_htp_performance_mode(backend)` —
  `deviceGetInfrastructure` → `createPowerConfigId(0,0)` → `setPowerConfig` with
  RPC control latency 100 µs, RPC polling 9999 µs, and DCVS_V3 burst (DCVS
  disabled, bus+core corners pinned to `MAX`, PERFORMANCE_MODE, 40 µs sleep
  latency). The power-config id is kept alive so the vote persists.
  **Must be called after `Session::new`/`Context::from_binary`.** Before a
  backend and device exist, `deviceGetInfrastructure` returns 1100
  (`QNN_COMMON_ERROR_GENERAL`) and the vote never lands; a multi-second graph
  then outruns the FastRPC timeout and the DSP subsystem restarts, surfacing as
  `graphExecute` 1007/1011 (`SYSTEM_COMMUNICATION[_FATAL]`).
- `prepare_htp_env(skel_dir)` — prepend `skel_dir` to `ADSP_LIBRARY_PATH`/
  `DSP_LIBRARY_PATH` (`;`) and `LD_LIBRARY_PATH` (`:`) via `setenv` so the native
  HTP/FastRPC loaders find the skel. Call once before the HTP backend is dlopened.

Quant/dequant (`floatToTfN`/`tfNToFloat`, `real = (q + offset) * scale`) lives in
the pure `quant` module and is unit-tested on host (no device needed).

The HTP power-config and context config/property structs are hand-declared
byte-exact in `htp.rs` from `HTP/QnnHtpPerfInfrastructure.h`,
`HTP/QnnHtpDevice.h`, and `HTP/QnnHtpContext.h` (those headers use C++/`bool`
and are excluded from `wrapper.h`, so bindgen never emitted them). `const`
`size_of`/`offset_of` asserts guard the layout: `DcvsV3_t` = 64 B, `PowerConfig_t`
= 68 B (matching the header's own `static_assert`), `PerfInfrastructure_t` = 32 B,
`Infrastructure_t` = 40 B, `GroupRegistration_t` = 16 B, `Context_CustomConfig_t`
= 24 B and `Context_CustomProperty_t` = 16 B (4-byte enum, 4 bytes of padding,
then the 8-byte-aligned union).

**HTP context binaries require the HTP backend + skel; the host CPU backend
cannot execute them** — host stays metadata-parse only.

### Validated on device (S26 Ultra / SM8850 / HTP V81, QAIRT 2.48)

`examples/anima_smoke.rs` loads both ~2 GB Anima DiT halves co-resident in one
`Session` and runs split steps. Verified: both halves resident, `graphExecute`
OK on both, `output[1,16,1,128,128]` finite, ~3.1 s per half (~6.2 s per step,
~62 s for 10 steps), stable over 10 consecutive steps.

- The burst vote is accepted on V81 and is required, not just an optimization:
  without it `part1` takes 8.2 s and `part2`'s `graphExecute` fails 1007/1011.
- Both DiT halves report a spill-fill requirement of 0 bytes, and they execute
  correctly with or without the shared group. `ANIMA_SF_BYTES=0` disables it.

### Needs on-device validation

- `execute` matches IO by the tensor `id` parsed from the binary (V2 tensors) —
  confirm a real SD1.5/SDXL UNet/VAE step yields sane latents.
- Confirm `ADSP_LIBRARY_PATH` resolves the skel and that it is extracted to disk
  (`android:extractNativeLibs="true"`, or extract from assets at runtime).

### Device packaging (cargo-apk2)

See the commented `runtime_libs` block in `examples/comfyui-android/Cargo.toml`.
Ship for `arm64-v8a`: `libQnnHtp.so`, `libQnnSystem.so`, `libQnnHtpV81Stub.so`
(+ optional `libQnnHtpPrepare.so`), the `hexagon-v81/unsigned/libQnnHtpV81Skel.so`
skel, and `libc++_shared.so` (QNN links the C++ runtime). Then call
`prepare_htp_env(skel_dir)` at startup. SDK libs are proprietary and never
committed; the block references a placeholder folder and the APK builds without
it.

## Bindings

`src/bindings.rs` is generated by `bindgen` from Qualcomm's QNN C headers and is
**committed**, so the crate builds and tests without the SDK present. Chosen over
hand-written FFI because the API is a large set of versioned structs with nested
unions and function-pointer tables (`QnnInterface_t` / `QnnSystemInterface_t`
providers, binary-info V1/V2/V3, tensor V1/V2) where a byte-exact layout is
required — bindgen guarantees this from the headers and also yields the full
backend table needed for D1 for free. The bindings are LP64 and identical for
`x86_64-linux` and `aarch64-android`.

Regenerate (needs `bindgen-cli` + libclang):

```sh
QNN_SDK_ROOT=/path/to/qairt/2.xx.xx.xxxxxx ./scripts/regen-bindings.sh
```

HTP perf-infra headers use C++/`bool` and are excluded from `wrapper.h`; the
device power-config structs are instead hand-declared byte-exact in `src/htp.rs`
(guarded by `size_of`/`offset_of` asserts), not bindgen-generated.

## Running the dumper

```sh
cargo run -p qnn-rs --example dump_context -- <libQnnSystem.so> <context.bin>
# or: QNN_SYSTEM_LIB=... QNN_CONTEXT_BIN=... cargo run -p qnn-rs --example dump_context
```

The env-gated integration test:

```sh
QNN_SYSTEM_LIB=.../lib/x86_64-linux-clang/libQnnSystem.so \
QNN_CONTEXT_BIN=.../unet.bin \
cargo test -p qnn-rs -- --ignored
```

Host note: the QAIRT x86_64 libs are built against LLVM `libc++` and link
`libc++.so.1`, `libc++abi.so.1`, and `libunwind.so.1`. On a stock Manjaro box
(GNU `libunwind.so.8`), point `LD_LIBRARY_PATH` at those runtime libs; metadata
parsing throws no C++ exceptions, so the unwinder only needs to resolve at load.

## Licensing

The QNN/QAIRT libraries and headers are **proprietary Qualcomm** software and are
**never committed** to this repo. Only `src/bindings.rs`, mechanically derived
from the headers, lives in-tree. Anyone building the device path must obtain the
QAIRT SDK from Qualcomm.
