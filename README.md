# egui-ios

Build native [egui](https://github.com/emilk/egui) iOS apps in **Rust**, cross-compiled on
Linux with [xtool](https://github.com/xtool-org/xtool). Write only Rust — the Swift host,
the FFI, and the build steps are all library code.

```rust
use egui_ios::egui;
use egui_ios::{CreateContext, EguiApp, Haptic, Host, app};

struct App { count: u32 }
impl App { fn new(_: &CreateContext) -> Self { Self { count: 0 } } }

impl EguiApp for App {
    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        if ui.button(format!("Tapped {}", self.count)).clicked() {
            self.count += 1;
            host.haptic(Haptic::Light);
        }
    }
}

app!(App::new);
```

That is the entire app. No Swift, no FFI, no build script.

## How it works

- **`crates/egui-ios`** — the runtime. A `CAMetalLayer` pointer comes in from Swift; the crate
  creates a `wgpu` Metal surface, runs the egui pass each frame (`egui-wgpu`), and forwards
  touch/keyboard/scroll input. The [`app!`] macro emits a small, frozen `extern "C"` ABI
  (`egui_ios_*`) so there is **no swift-bridge codegen** — nothing to copy, patch, or validate.
- **`EguiKit`** (this repo, as a Swift package) — the generic host: a `CAMetalLayer` view, a
  `CADisplayLink` loop, input forwarding, and the capability bridge (share sheet, notifications,
  haptics, file picker, camera/mic). It ships the frozen C header at
  `EguiKit/Sources/EguiKitC/include/egui_ios.h`. Apps reuse it unchanged.
- **`crates/cargo-egui-ios`** — the `cargo egui-ios` subcommand that replaces a per-project
  build script: it bundles the `xcrun`/`codesign` shims, cross-compiles the Rust staticlib,
  links it directly from cargo's target dir (`EGUI_IOS_RUST_TARGET_DIR`, no copy), and runs
  `xtool`.

The app talks to the host through [`Host`]: `host.share_file(..)`, `host.notify(..)`,
`host.haptic(..)`, `host.pick_file(..)`, `host.request_permission(..)`,
`host.start_camera_preview()`, plus reads like `host.safe_area_insets()` and
`host.documents_dir()`.

## Create a new app

One-time setup:

```bash
xtool setup                          # installs the Darwin Swift SDK + signing
rustup target add aarch64-apple-ios
cargo install --path crates/cargo-egui-ios   # or `cargo install cargo-egui-ios` once published
```

Then:

```bash
cargo egui-ios new my-app
cd my-app
#   edit rust/src/lib.rs
cargo egui-ios run                   # cross-compiles + xtool dev → on device
```

`cargo egui-ios run` does `build` then `xtool dev`. `build` accepts `--simulator` (macOS host
only), `--assets <DIR>` (bundles a real-file asset tree), and `--swift-bridge` (reserved for
apps that opt into swift-bridge codegen for custom typed FFI).

## Run the example

```bash
cd examples/hello
cargo egui-ios run
```

The example exercises every capability: tap→haptic, notifications, open URL, a text field
(keyboard), file picker, share sheet, camera permission + preview, and a live mic-level meter.

## WASM plugins (hot-swappable UI extensions)

Apps can host plugins: `wasm32-wasip1` modules running a **full egui context in-guest**
(wasmtime — Pulley interpreter on iOS, Cranelift JIT on desktop). Push rebuilt plugins to a
running app over WiFi — no reinstall, state preserved:

```bash
cargo egui-ios plugin new my-widget
cd my-widget && cargo egui-ios plugin serve    # build + watch + serve
# in the app's plugin manager: connect to <dev-machine>:7878 → live hot reload
```

The same `.wasm` runs in the iOS runtime (`egui-ios` feature `plugins`) and in eframe on
desktop (`examples/desktop-host`). `examples/plugins-ios` is the on-device host app;
`plugins/hello-plugin` and `plugins/ratatui-demo` (a ratatui TUI in a plugin) are working
examples. See [PLUGINS.md](PLUGINS.md) for the architecture, ABI, permissions, and op surface.

## Notes

- **Linux is device-only.** There is no iOS simulator on Linux (the bundled `xcrun` shim makes
  `simctl` a no-op); `--simulator` is meaningful only on a macOS host.
- **Pure egui needs no SDK hacks.** A pure egui+wgpu staticlib cross-compiles from Linux with
  no `[patch.crates-io]`, no `blake3 pure`, no `tracing-oslog` stub — those were bevy-specific.
  `SDKROOT` is only needed if a C dependency invokes `xcrun`.
- **Lifetime.** The `CAMetalLayer` (and its hosting view) must outlive the renderer; do not
  recreate the `EguiView` mid-session (`.id()` churn). `EguiKit` handles this for you.
- **ABI stability.** `egui_ios.h` is append-only and version-checked at startup
  (`egui_ios_abi_version`); the Rust runtime and `EguiKit` are released in lockstep.

## Versions

egui 0.35 · egui-wgpu 0.35 · wgpu 29 · wasmtime 46 · Rust 2024 edition.

## License

MIT OR Apache-2.0.
