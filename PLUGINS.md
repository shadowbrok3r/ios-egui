# WASM UI plugins

Hot-swappable UI extensions for egui-ios apps. A plugin is a `wasm32-wasip1` module that runs a
**full egui context inside the guest** — anything that works in eframe works in a plugin
(custom widgets, `ratatui` backends, editors). The host ships translated input in; the guest
ships tessellated meshes and texture deltas back; the host paints them inside a viewport
widget. New builds can be pushed to a running app — including on an iPhone, over WiFi —
without reinstalling the app, and plugin state survives the reload.

On iOS, wasmtime executes plugins with its **Pulley** interpreter (third-party apps cannot map
executable memory, so there is no JIT); on desktop hosts the same `.wasm` runs under the
Cranelift JIT. Expect roughly a 10× slowdown vs native inside the interpreter — fine for UI
code; keep heavy compute host-side behind ops.

## Bundled plugins

Working examples under `plugins/` (build any with `cargo egui-ios plugin serve` from its dir):

- **terminal** — a native-feeling terminal rendered with `ratatui` inside WASM: the iOS soft
  keyboard raises on tap, typed text and control keys drive a char-indexed line editor, touch
  drags grab-scroll the scrollback (with a "↓ N newer" indicator), a pink block cursor blinks,
  Up/Down recall history, and a built-in pocket shell runs `calc` (scientific expressions),
  `hex`/`bin`/`oct`/`dec` conversions, `b64`, `sha256`, and text utilities. This is the shell an
  SSH client grows from — sockets and PTY would be host ops (`ssh.*`), keeping crypto native.
- **regex-tester** — live regular-expression testing; matches highlight inline in the editable
  text via a `TextEdit` layouter, with a capture-group breakdown and flags.
- **json-viewer** — validate, pretty-print, minify, and browse JSON as a collapsible,
  type-colored tree; parse errors report line and column.
- **hello-plugin** / **ratatui-demo** — the minimal egui and ratatui showcases.

## Input model (keyboard & touch)

Because a plugin runs a full egui context in-guest, the host forwards translated
`egui::RawInput` (text, keys, pointer, touch) each frame — so a plugin reads input exactly like
any egui app via `ui.input(...)`. Two things a custom-drawing plugin (e.g. the terminal, which
paints its own cell grid rather than using an `egui::TextEdit`) must do:

- **Raise the keyboard**: call `host.request_keyboard(true)` when focused. The host bridges this
  to the iOS soft keyboard (`PluginViewportResponse::wants_keyboard`). An `egui::TextEdit`
  raises it automatically; a custom widget must ask.
- **Own its line editing**: iOS text arrives as `egui::Event::Text` and edits as `Event::Key`
  (Backspace/Enter/arrows); the terminal's `LineEditor` turns these into a unicode-safe cursor.
  Touch scrolling comes from the pointer position each frame while a finger is down.

## Writing a plugin

```bash
cargo egui-ios plugin new my-widget
cd my-widget
```

```rust
use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};

struct App { count: u32 }
impl App { fn new(_: &CreateConfig) -> Self { App { count: 0 } } }

impl PluginApp for App {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        if ui.button(format!("Tapped {}", self.count)).clicked() {
            self.count += 1;
            host.haptic(0);
        }
    }
    // Optional: state carried across hot reloads.
    fn save_state(&self) -> Vec<u8> { self.count.to_le_bytes().to_vec() }
    fn restore_state(&mut self, b: &[u8]) { if let Ok(b) = b.try_into() { self.count = u32::from_le_bytes(b); } }
}

plugin!(App::new);
```

`manifest.toml` declares identity and permissions:

```toml
id = "com.example.my-widget"
name = "My Widget"
version = "0.1.0"
abi_version = 1
permissions = ["haptic", "notify"]   # ops the plugin may call; "net" also grants "net.*"
```

## The dev loop (hot reload on device)

```bash
cargo egui-ios plugin serve          # builds, watches, serves on 0.0.0.0:7878
```

In the app's plugin manager, enter `your-dev-machine:7878` and Connect. Every save →
rebuild → push → in-place reload with `save_state`/`restore_state` carried across. The same
server feeds the desktop host (`cargo run -p desktop-host`) and the iOS app simultaneously.

Static installs work too: `cargo egui-ios plugin build --out <dir>` stages
`<id>/{plugin.wasm, manifest.toml}`, which any host loads from its plugins directory
(iOS: `Documents/plugins`).

## Hosting plugins in an app

iOS (feature `plugins` on `egui-ios`):

```rust
use egui_ios::plugins::{HostOps, IosOps, PluginManager, PluginManagerUi};

// once, when host.documents_dir() is available:
let mut manager = PluginManager::new(format!("{docs}/plugins"), IosOps::new() as _, "ios")?;
manager.scan(ui.ctx());

// per frame:
let response = manager.show_plugin(ui, index);       // full egui viewport
host.request_keyboard(response.wants_keyboard);      // bridge the soft keyboard
ops.drain_into(host);                                // apply queued haptics/notifications/…
```

Desktop (eframe 0.35 + wgpu): see `examples/desktop-host`. The one-time hookup on any host is
`egui_ios_plugin_host::install(&mut renderer, surface_format, msaa_samples)`; the iOS runtime
does this automatically when the `plugins` feature is on.

Apps extend the op surface by implementing `HostOps` — e.g. a Termius-style app registers
native `ssh.*`/`net.tcp.*` ops (native crypto speed) and plugins drive them via
`host.call("ssh.connect", …)`, gated by their manifest.

## Architecture

```
┌────────────── host app (iOS runtime or eframe) ──────────────┐
│ PluginViewport: rect + input translation                     │
│   FrameInput { egui::RawInput (plugin-local) }  ──postcard──▶│──┐
│                                                              │  │ wasmtime
│  ◀─postcard── FrameOutput { vertices/indices (bytemuck),     │  │ (Pulley on iOS,
│               texture deltas, cursor/url/copy/events,        │  │  Cranelift JIT
│               repaint delay, wants_keyboard }                │  │  elsewhere)
│ texture-id remap → per-plugin egui_wgpu sub-renderer         │  │
│ painted via CallbackTrait inside the host render pass        │◀─┘ guest: full egui
└──────────────────────────────────────────────────────────────┘    Context + PluginApp
```

- **Guest exports**: `plugin_abi_version`, `plugin_alloc/dealloc`, `plugin_create`,
  `plugin_frame`, `plugin_event`, `plugin_save`, `plugin_restore`, `plugin_destroy`.
- **Host imports** (module `egui_plugin_host`): `host_log`, `host_call`, `host_take_response`.
- **Versioning**: `ABI_VERSION` covers exports/imports/wire types; `WIRE_FORMAT` pins the egui
  minor whose serde encoding rides the wire. Guests refuse to start on either mismatch —
  rebuild the plugin after an egui upgrade.
- **Sandboxing**: WASI with no preopens (only the built-in `state.get`/`state.set` ops touch
  disk, inside the plugin's own directory); caps on memory (256 MB), tables, and instances;
  epoch deadlines (~500 ms per frame call, host-op time not charged) so a hung plugin traps
  and shows an error panel instead of freezing the app; every other capability is an op gated
  by manifest permissions.
- **Untrusted output is validated**: guest meshes with out-of-range indices and texture sets
  whose pixel buffer doesn't match the declared size are dropped before reaching wgpu, so a
  hostile or buggy plugin cannot fault the host GPU/render thread.
- **Compile cache**: the wasmtime module is compiled once and cached as `plugin.cwasm` (keyed
  by content hash) next to `plugin.wasm`, so relaunches and hot reloads skip the multi-second
  recompile — important on iOS where Pulley compiles on the UI thread.
- **Paint callbacks** (`egui::PaintCallback`) cannot cross the wasm boundary and are skipped
  (counted in `FrameOutput::skipped_callbacks`).

## Theming

The host and every plugin default to a shared dark egui theme (near-black surfaces, pink /
purple accents) via `egui_ios_plugin_abi::theme` — the iOS runtime, plugin guests, and the
desktop host all apply it automatically before the first frame; a plugin's `theme()` can still
override it. `ratatui-demo` shows the matching terminal palette (Catppuccin Mocha + deep-pink
accent) for TUI plugins.

## Built-in ops

| op | payload | notes |
| --- | --- | --- |
| `state.set` / `state.get` | postcard `(key, bytes)` / key utf8 | always allowed, per-plugin dir |
| `haptic` | 1 byte, 0..=6 | iOS: UIImpactFeedbackGenerator |
| `notify` | postcard `(title, body)` | local notification |
| `url.open`, `clipboard.set`, `share.file` | utf8 | also auto-bridged from egui outputs, permission-gated |
| `keyboard.set` | 1 byte 0/1 | explicit soft-keyboard control |

Everything else is app-defined via `HostOps`.
