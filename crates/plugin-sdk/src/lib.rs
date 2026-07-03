//! Guest-side SDK for egui-ios WASM plugins. Implement [`PluginApp`] and invoke [`plugin!`];
//! the plugin runs a full [`egui::Context`] inside the guest and ships tessellated output
//! to the host each frame. Any code that works in eframe works here.
//!
//! ```ignore
//! use egui_ios_plugin_sdk::{egui, plugin, CreateConfig, HostHandle, PluginApp};
//!
//! struct Clock;
//! impl Clock { fn new(_: &CreateConfig) -> Self { Clock } }
//! impl PluginApp for Clock {
//!     fn update(&mut self, ui: &mut egui::Ui, _host: &HostHandle) {
//!         ui.heading("tick");
//!     }
//! }
//! plugin!(Clock::new);
//! ```

pub use egui;
pub use egui_ios_plugin_abi as abi;

pub use abi::CreateConfig;
use abi::{FrameInput, FrameOutput, HostCallRequest, HostCallResponse, PluginEvent, WirePlatform};

/// The trait a plugin implements. Only [`PluginApp::update`] is required.
pub trait PluginApp: 'static {
    /// Build one frame into the root [`egui::Ui`], which spans the plugin's viewport rect.
    /// Same shape as `egui_ios::EguiApp::update`; use `ui.ctx()` for context-level calls
    /// and `egui::CentralPanel::default().show(ui, ..)` for panels.
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle);

    /// Configure style and fonts once, after creation.
    fn theme(&self, _ctx: &egui::Context) {}

    /// An event pushed in by the embedding app.
    fn on_host_event(&mut self, _topic: &str, _payload: &[u8], _host: &HostHandle) {}

    /// Snapshot state before a hot reload. Returned bytes are handed to the
    /// replacement instance's [`PluginApp::restore_state`].
    fn save_state(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Restore state captured by the previous instance before a hot reload.
    fn restore_state(&mut self, _bytes: &[u8]) {}
}

/// Log levels for [`HostHandle::log`], mirroring the `log` crate.
#[derive(Clone, Copy, Debug)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

/// Error from a [`HostHandle::call`].
#[derive(Clone, Debug)]
pub enum HostCallError {
    /// The op is not covered by the plugin's manifest permissions.
    Denied,
    /// The host op failed or does not exist.
    Failed(String),
    /// The response could not be decoded (host/guest ABI mismatch).
    Codec,
}

impl std::fmt::Display for HostCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostCallError::Denied => write!(f, "denied by manifest permissions"),
            HostCallError::Failed(e) => write!(f, "{e}"),
            HostCallError::Codec => write!(f, "response codec error"),
        }
    }
}

impl std::error::Error for HostCallError {}

/// Guest-side handle to the host. Zero-sized; safe to copy around.
#[derive(Clone, Copy, Default)]
pub struct HostHandle;

impl HostHandle {
    /// Log through the host (shows up in the plugin manager UI and host logs).
    pub fn log(&self, level: Level, msg: &str) {
        imports::log(level as u32, msg.as_bytes());
    }

    /// Synchronously call a host op. Ops are permission-gated by the plugin manifest.
    pub fn call(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>, HostCallError> {
        let req = abi::encode(&HostCallRequest {
            op: op.to_owned(),
            payload: payload.to_vec(),
        });
        let rsp = imports::call(&req);
        match abi::decode::<HostCallResponse>(&rsp) {
            Ok(HostCallResponse::Ok(bytes)) => Ok(bytes),
            Ok(HostCallResponse::Err(e)) => Err(HostCallError::Failed(e)),
            Ok(HostCallResponse::Denied) => Err(HostCallError::Denied),
            Err(_) => Err(HostCallError::Codec),
        }
    }

    /// Queue an event for the embedding app; delivered with this frame's output.
    pub fn emit(&self, topic: &str, payload: &[u8]) {
        rt::EVENTS.with(|e| {
            e.borrow_mut().push(PluginEvent {
                topic: topic.to_owned(),
                payload: payload.to_vec(),
            })
        });
    }

    /// Request or dismiss the on-screen keyboard for this frame. A plugin that draws its own
    /// text UI (not an `egui::TextEdit`) must call this — the host bridges it to the iOS soft
    /// keyboard. Latched: call every frame with your focus state. Ignored on desktop hosts,
    /// which have a hardware keyboard.
    pub fn request_keyboard(&self, wanted: bool) {
        rt::WANT_KEYBOARD.with(|c| c.set(wanted));
    }

    /// Copy text to the system clipboard (op `clipboard.set`; permission-gated).
    pub fn copy_text(&self, text: &str) {
        let _ = self.call("clipboard.set", text.as_bytes());
    }

    /// Fire haptic feedback (op `haptic`; payload one byte, 0..=6 = light..selection).
    pub fn haptic(&self, kind: u8) {
        let _ = self.call("haptic", &[kind]);
    }

    /// Post a local notification (op `notify`).
    pub fn notify(&self, title: &str, body: &str) {
        let _ = self.call("notify", &abi::encode(&(title, body)));
    }

    /// Open a URL in the system browser (op `url.open`).
    pub fn open_url(&self, url: &str) {
        let _ = self.call("url.open", url.as_bytes());
    }

    /// Persist a small value in the plugin's state store (op `state.set`).
    pub fn state_set(&self, key: &str, value: &[u8]) -> Result<(), HostCallError> {
        self.call("state.set", &abi::encode(&(key, value))).map(|_| ())
    }

    /// Read a value from the plugin's state store (op `state.get`); `None` if unset.
    pub fn state_get(&self, key: &str) -> Result<Option<Vec<u8>>, HostCallError> {
        let rsp = self.call("state.get", key.as_bytes())?;
        abi::decode::<Option<Vec<u8>>>(&rsp).map_err(|_| HostCallError::Codec)
    }
}

/// Raw host imports, with native stubs so plugin crates also compile for host targets.
mod imports {
    #[cfg(target_arch = "wasm32")]
    mod ffi {
        #[link(wasm_import_module = "egui_plugin_host")]
        unsafe extern "C" {
            pub fn host_log(level: u32, ptr: *const u8, len: u32);
            pub fn host_call(req_ptr: *const u8, req_len: u32) -> u32;
            pub fn host_take_response(dst_ptr: *mut u8, cap: u32) -> u32;
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn log(level: u32, msg: &[u8]) {
        unsafe { ffi::host_log(level, msg.as_ptr(), msg.len() as u32) }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn call(req: &[u8]) -> Vec<u8> {
        unsafe {
            let len = ffi::host_call(req.as_ptr(), req.len() as u32);
            let mut buf = vec![0u8; len as usize];
            let written = ffi::host_take_response(buf.as_mut_ptr(), len);
            buf.truncate(written as usize);
            buf
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn log(level: u32, msg: &[u8]) {
        eprintln!("[plugin log {level}] {}", String::from_utf8_lossy(msg));
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn call(_req: &[u8]) -> Vec<u8> {
        super::abi::encode(&super::HostCallResponse::Err("not running under a plugin host".into()))
    }
}

/// Runtime internals used by the [`plugin!`] macro. Not a stable API.
#[doc(hidden)]
pub mod rt {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    pub struct GuestRuntime {
        ctx: egui::Context,
        app: Box<dyn PluginApp>,
        out_buf: Vec<u8>,
    }

    thread_local! {
        static RUNTIME: RefCell<Option<GuestRuntime>> = const { RefCell::new(None) };
        static ALLOCS: RefCell<HashMap<u32, Vec<u8>>> = RefCell::new(HashMap::new());
        pub(super) static EVENTS: RefCell<Vec<PluginEvent>> = const { RefCell::new(Vec::new()) };
        pub(super) static WANT_KEYBOARD: Cell<bool> = const { Cell::new(false) };
    }

    /// Allocate a guest buffer the host can write into. Returns the pointer.
    pub fn alloc(len: u32) -> u32 {
        let mut v = vec![0u8; (len as usize).max(1)];
        let ptr = v.as_mut_ptr() as u32;
        ALLOCS.with(|a| a.borrow_mut().insert(ptr, v));
        ptr
    }

    /// Free a buffer the host allocated but will not consume (error paths).
    pub fn dealloc(ptr: u32) {
        ALLOCS.with(|a| a.borrow_mut().remove(&ptr));
    }

    fn take(ptr: u32, len: u32) -> Vec<u8> {
        let mut v = ALLOCS
            .with(|a| a.borrow_mut().remove(&ptr))
            .expect("plugin: unknown input buffer");
        v.truncate(len as usize);
        v
    }

    fn install_panic_hook() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::panic::set_hook(Box::new(|info| {
                imports::log(Level::Error as u32, info.to_string().as_bytes());
            }));
        });
    }

    pub fn create(cfg_ptr: u32, cfg_len: u32, factory: impl FnOnce(&CreateConfig) -> Box<dyn PluginApp>) -> u32 {
        install_panic_hook();
        let bytes = take(cfg_ptr, cfg_len);
        let Ok(cfg) = abi::decode::<CreateConfig>(&bytes) else {
            imports::log(Level::Error as u32, b"plugin_create: bad CreateConfig");
            return 0;
        };
        if cfg.abi_version != abi::ABI_VERSION || cfg.wire_format != abi::WIRE_FORMAT {
            imports::log(
                Level::Error as u32,
                format!(
                    "plugin_create: host ABI {}/wire {} != guest ABI {}/wire {} — rebuild the plugin",
                    cfg.abi_version,
                    cfg.wire_format,
                    abi::ABI_VERSION,
                    abi::WIRE_FORMAT,
                )
                .as_bytes(),
            );
            return 0;
        }
        let ctx = egui::Context::default();
        // Default to the shared Mastertech theme; honor a light-mode host, then let the plugin
        // override in its own `theme()`.
        abi::theme::apply(&ctx);
        if !cfg.dark_mode {
            ctx.set_visuals(egui::Visuals::light());
        }
        let app = factory(&cfg);
        app.theme(&ctx);
        RUNTIME.with(|rt| {
            *rt.borrow_mut() = Some(GuestRuntime {
                ctx,
                app,
                out_buf: Vec::new(),
            })
        });
        1
    }

    pub fn frame(ptr: u32, len: u32) -> u64 {
        let bytes = take(ptr, len);
        let input: FrameInput = match abi::decode(&bytes) {
            Ok(i) => i,
            Err(_) => {
                imports::log(Level::Error as u32, b"plugin_frame: bad FrameInput");
                return 0;
            }
        };
        RUNTIME.with(|cell| {
            let mut guard = cell.borrow_mut();
            let rt = guard.as_mut().expect("plugin_frame before plugin_create");
            let GuestRuntime { ctx, app, out_buf } = rt;

            let host = HostHandle;
            let full = ctx.run_ui(input.raw_input, |ui| app.update(ui, &host));

            let clipped = ctx.tessellate(full.shapes, full.pixels_per_point);
            let (primitives, skipped_callbacks) = abi::primitives_to_wire(&clipped);
            let (textures_set, textures_free) = abi::textures_delta_to_wire(&full.textures_delta);

            let mut open_url = None;
            let mut copy_text = None;
            for cmd in &full.platform_output.commands {
                match cmd {
                    egui::OutputCommand::OpenUrl(o) => open_url = Some(o.url.clone()),
                    egui::OutputCommand::CopyText(t) => copy_text = Some(t.clone()),
                    _ => {}
                }
            }

            let repaint_delay_secs = full
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .map(|v| v.repaint_delay)
                .filter(|d| *d != std::time::Duration::MAX)
                .map(|d| d.as_secs_f64());

            let out = FrameOutput {
                primitives,
                textures_set,
                textures_free,
                platform: WirePlatform {
                    repaint_delay_secs,
                    wants_keyboard: ctx.egui_wants_keyboard_input()
                        || WANT_KEYBOARD.with(|c| c.get()),
                    wants_pointer: ctx.egui_wants_pointer_input(),
                    cursor_icon: Some(full.platform_output.cursor_icon),
                    open_url,
                    copy_text,
                    events: EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut())),
                },
                skipped_callbacks,
            };
            *out_buf = abi::encode(&out);
            abi::pack_ptr_len(out_buf.as_ptr() as u32, out_buf.len() as u32)
        })
    }

    pub fn event(ptr: u32, len: u32) {
        let bytes = take(ptr, len);
        let Ok(ev) = abi::decode::<PluginEvent>(&bytes) else {
            imports::log(Level::Error as u32, b"plugin_event: bad PluginEvent");
            return;
        };
        RUNTIME.with(|cell| {
            if let Some(rt) = cell.borrow_mut().as_mut() {
                rt.app.on_host_event(&ev.topic, &ev.payload, &HostHandle);
            }
        });
    }

    pub fn save() -> u64 {
        RUNTIME.with(|cell| {
            let mut guard = cell.borrow_mut();
            let Some(rt) = guard.as_mut() else { return 0 };
            rt.out_buf = rt.app.save_state();
            abi::pack_ptr_len(rt.out_buf.as_ptr() as u32, rt.out_buf.len() as u32)
        })
    }

    pub fn restore(ptr: u32, len: u32) {
        let bytes = take(ptr, len);
        RUNTIME.with(|cell| {
            if let Some(rt) = cell.borrow_mut().as_mut() {
                rt.app.restore_state(&bytes);
            }
        });
    }

    pub fn destroy() {
        RUNTIME.with(|cell| cell.borrow_mut().take());
    }
}

/// Generates the WASM exports for a type implementing [`PluginApp`].
///
/// `factory` is any `Fn(&CreateConfig) -> impl PluginApp`, e.g. `plugin!(MyPlugin::new)`.
#[macro_export]
macro_rules! plugin {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_abi_version() -> u32 {
            $crate::abi::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_alloc(len: u32) -> u32 {
            $crate::rt::alloc(len)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_dealloc(ptr: u32) {
            $crate::rt::dealloc(ptr)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_create(cfg_ptr: u32, cfg_len: u32) -> u32 {
            $crate::rt::create(cfg_ptr, cfg_len, |cfg| {
                ::std::boxed::Box::new($factory(cfg)) as ::std::boxed::Box<dyn $crate::PluginApp>
            })
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_frame(ptr: u32, len: u32) -> u64 {
            $crate::rt::frame(ptr, len)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_event(ptr: u32, len: u32) {
            $crate::rt::event(ptr, len)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_save() -> u64 {
            $crate::rt::save()
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_restore(ptr: u32, len: u32) {
            $crate::rt::restore(ptr, len)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_destroy() {
            $crate::rt::destroy()
        }
    };
}
