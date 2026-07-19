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

- `Context::from_binary(backend, system, bytes)` — parse `ContextBinaryInfo`,
  then `backendCreate` → `deviceCreate` (if present) → `contextCreateFromBinary`
  → `graphRetrieve` per graph. Keeps the context/device/backend + graph handles
  and the parsed metadata; frees them in reverse on `Drop`. Borrows the `Backend`
  so the loaded library outlives every call.
- `Context::execute(graph, &[(name, &[f32])]) -> HashMap<name, Vec<f32>>` —
  quantize each named input to its tensor dtype via the metadata scale-offset,
  build V2 `Qnn_Tensor_t` IO arrays (RAW client buffers, tensor `id` from
  metadata), `graphExecute`, then dequantize outputs to f32.
- `set_htp_performance_mode(backend)` — `deviceGetInfrastructure` →
  `createPowerConfigId(0,0)` → `setPowerConfig` with DCVS_V3 burst (DCVS
  disabled, bus+core corners pinned to `MAX`, PERFORMANCE_MODE, 40 µs sleep
  latency). The power-config id is kept alive so the vote persists.
- `prepare_htp_env(skel_dir)` — prepend `skel_dir` to `ADSP_LIBRARY_PATH`/
  `DSP_LIBRARY_PATH` (`;`) and `LD_LIBRARY_PATH` (`:`) via `setenv` so the native
  HTP/FastRPC loaders find the skel. Call once before the HTP backend is dlopened.

Quant/dequant (`floatToTfN`/`tfNToFloat`, `real = (q + offset) * scale`) lives in
the pure `quant` module and is unit-tested on host (no device needed).

The HTP power-config structs are hand-declared byte-exact in `htp.rs` from
`HTP/QnnHtpPerfInfrastructure.h` + `HTP/QnnHtpDevice.h` (those headers use C++/
`bool` and are excluded from `wrapper.h`, so bindgen never emitted them). `const`
`size_of`/`offset_of` asserts guard the layout: `DcvsV3_t` = 64 B, `PowerConfig_t`
= 68 B (matching the header's own `static_assert`), `PerfInfrastructure_t` = 32 B,
`Infrastructure_t` = 40 B.

**HTP context binaries require the HTP backend + skel; the host CPU backend
cannot execute them** — host stays metadata-parse only.

### Needs on-device validation (S26 Ultra / SM8850 / HTP V81)

- `from_binary` / `execute` / `set_htp_performance_mode` have not run on hardware.
- `execute` matches IO by the tensor `id` parsed from the binary (V2 tensors) —
  confirm a real UNet/VAE step yields sane latents.
- Confirm the DCVS_V3 burst vote is accepted on V81 and improves latency.
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
