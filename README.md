# egui-mobile

Build native [egui](https://github.com/emilk/egui) apps for **iOS and Android** in pure Rust,
cross-compiled from Linux. Write only Rust — the platform hosts, the FFI/JNI, and the build
steps are all library code. One `impl EguiApp` runs on both platforms:

```rust
use egui_mobile::{egui, CreateContext, EguiApp, Haptic, Host, app};

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

app!(App::new); // C ABI on iOS, android_main on Android
```

That is the entire app. No Swift, no Kotlin, no build script.

## Crates

| crate | role |
| --- | --- |
| **`egui-mobile`** | the facade apps depend on: shared API + platform-selected `app!` macro |
| **`egui-mobile-core`** | `EguiApp` trait and the `Host` capability bridge (queue + pushed-in state) |
| **`egui-ios`** | iOS runtime: `CAMetalLayer` in from Swift, `wgpu` Metal surface, egui pass, frozen `egui_ios_*` C ABI (no swift-bridge codegen) |
| **`egui-android`** | Android runtime: eframe (winit + NativeActivity + wgpu) render loop, JNI capability bridge, soft-keyboard + clipboard integration |
| **`cargo-egui-mobile`** | the one CLI to install: `new`/`build`/`run` with `-i/--ios` or `-a/--android`, plus the platform-neutral `plugin` commands |
| **`cargo-egui-ios`** / **`cargo-egui-android`** | per-platform tools (still installable standalone; the unified CLI wraps them) |
| **`plugin-abi` / `plugin-host` / `plugin-sdk`** | the WASM plugin system shared by iOS, Android, and desktop hosts — see [PLUGINS.md](PLUGINS.md) |

`EguiKit/` is the generic Swift host package for iOS (Metal view, display link, input
forwarding, capability bridge). Apps reuse it unchanged.

## One-time setup

Both platforms:

```bash
cargo install --path crates/cargo-egui-mobile   # one install covers iOS + Android
```

iOS (device deploys from Linux via [xtool](https://github.com/xtool-org/xtool)):

```bash
xtool setup
rustup target add aarch64-apple-ios
```

Android (gradle-free APKs via [cargo-apk2](https://crates.io/crates/cargo-apk2)):

```bash
cargo install cargo-apk2
rustup target add aarch64-linux-android
# plus an SDK + NDK under ~/Android/Sdk (sdkmanager: platform-tools, platforms, build-tools, ndk),
# a JDK 17..21. Prefer `cargo egui-mobile … -a` (auto-sets ANDROID_*/JAVA_HOME).
# Bare `cargo apk2`: eval "$(cargo egui-mobile env -a)"
```

## Create and run an app

```bash
cargo egui-mobile new my-app --android     # or --ios / -i / -a
cd my-app
#   edit src/lib.rs
cargo egui-mobile run -a                   # build APK + adb install + launch
cargo egui-mobile run -a --tcp 192.168.1.20  # wireless adb (default port 5555)
cargo egui-mobile run -i                   # cross-compile + xtool dev → on device
```

`build`/`run` accept `--release` on both platforms; iOS additionally `--simulator` (macOS
host only) and `--assets <DIR>`. Android `run` accepts `--tcp HOST[:PORT]` for wireless
debugging (one-time phone setup in [ANDROID_SETUP.md](ANDROID_SETUP.md));
`cargo egui-mobile adb-connect HOST` is the bare connect helper. Android release builds need a
signing entry in the app's Cargo.toml (`[package.metadata.android.signing.release]` with `path` +
`keystore_password`; pointing at `~/.android/debug.keystore` with password `android` is fine for
sideloads).

## Host capabilities

The app talks to the platform through [`Host`]: `share_file`/`share_text`, `notify`,
`haptic`, `open_url`, `copy_text`, `pick_file`, `request_permission`, camera preview and
mic level (iOS), plus reads like `safe_area_insets()`, `keyboard_height()`, and
`documents_dir()`. Android extras live behind the `HostExt` trait (self-update via
`PackageInstaller`, install/overlay/notification permissions, version code).

Android specifics handled by the runtime:

- **Soft keyboard**: `host.request_keyboard(..)` shows/hides the IME on a hidden
  `EditText` (`EguiNativeActivity`) so Gboard gets a real `InputConnection` — this is what
  enables hold-space trackpad cursor movement. Measure height with
  `keyboard_height()` (`window_soft_input_mode = "adjustResize"`). Apps must set
  `has_code = true`, `java_sources` → `egui-android/java`, and activity
  `com.github.egui_mobile.EguiNativeActivity` (plain `NativeActivity` falls back to the
  old show/hide path without spacebar cursor).
- **Clipboard + text actions**: egui has no Android selection menu, so while a text field
  is being edited the runtime overlays a floating **Paste / Copy / Cut / Select all** bar,
  bridges egui copies into the system clipboard via JNI, and injects clipboard text back
  as paste events. This works for host-side text fields and WASM-plugin text fields alike.
- **Insets**: status bar / cutout / nav bar are fed into `safe_area_insets()` each frame
  and the root UI is inset automatically (Android 15 edge-to-edge).

## WASM plugins (hot-swappable UI extensions)

Apps can host plugins: `wasm32-wasip1` modules running a **full egui context in-guest**
(wasmtime — Pulley interpreter on iOS *and* Android, Cranelift JIT on desktop). Push rebuilt
plugins to a running app over WiFi — no reinstall, state preserved:

```bash
cargo egui-mobile plugin new my-widget
cd my-widget && cargo egui-mobile plugin serve    # build + watch + serve on :7878
# in the app's plugin manager: connect to <dev-machine>:7878 → live hot reload
```

The same `.wasm` runs in the iOS runtime (`egui-ios` feature `plugins`), the Android runtime
(`egui-android` feature `plugins`), and in eframe on desktop (`examples/desktop-host`).
On-device host apps: `examples/plugins-ios` and `examples/plugins-android` (same manager UI,
dev-sync, and native `net.http.*`/`net.tcp.*`/`net.udp.*`/`ssh.*` ops on both). See
[PLUGINS.md](PLUGINS.md) for the architecture, ABI, permissions, and op surface.

## Examples

- `examples/hello` (iOS) and `examples/android-hello` — the capability tour per platform.
- `examples/plugins-ios` / `examples/plugins-android` — on-device plugin hosts with
  wireless hot reload.
- `examples/desktop-host` — the same plugin host on desktop
  (`cargo run -p desktop-host -- plugins-dist`).
- `plugins/` — working plugins: terminal (with SSH client), http-client, devices
  (Tailscale), regex-tester, json-viewer, rvim, ratatui-demo, and **wirelab** (live panel
  for WireLab ESP32 boards: discovery, telemetry, GPIO/PWM, behaviors, UART, plus canvas /
  flow / script mirroring of the desktop app).

## Notes

- **Linux is device-only for iOS.** No simulator on Linux; `--simulator` is meaningful only
  on a macOS host.
- **Interpreter budgets.** Plugins run under Pulley on phones (~10× slower than native).
  The host gives the first frames of each plugin a long "cold" deadline (font-atlas build)
  and a ~2 s steady-state deadline on mobile targets; a hung plugin still traps into an
  error panel instead of freezing the app.
- **Android IME path.** Text input uses a hidden `EditText` on `EguiNativeActivity` for a
  real `InputConnection` (Gboard spacebar trackpad + text commits). The text-actions bar
  still pins focus/`keep_soft_keyboard`. Host `TextEdit` is in scope; WASM plugin guest
  fields may lack spacebar cursor until they share the same bridge. Swipe-typing polish
  is best-effort.
- **ABI stability (iOS).** `egui_ios.h` is append-only and version-checked at startup;
  the Rust runtime and `EguiKit` are released in lockstep.

## Versions

egui 0.35 · egui-wgpu 0.35 · wgpu 29 · wasmtime 46 · Rust 2024 edition.

## License

MIT OR Apache-2.0.
