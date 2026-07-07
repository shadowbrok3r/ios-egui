//! WASM plugin support for iOS apps (feature `plugins`). Re-exports the host runtime and
//! maps standard plugin ops onto the Swift capability bridge.

pub use egui_ios_plugin_host::*;

use std::sync::{Arc, Mutex};

use egui_ios_plugin_abi::PluginManifest;

use crate::{Haptic, Host};

/// Standard ops backed by [`Host`], plus native network ops. The wasmtime store requires
/// `Send + Sync` op handlers, while `Host` is main-thread only — so main-thread ops queue
/// here and the app applies them once per frame with [`IosOps::drain_into`]. Network ops
/// (`net.http.*`, `ssh.*`, `net.tcp.*`, `net.udp.*`) are non-blocking and run on background
/// threads via [`NetOps`], so they return synchronously without touching the main thread.
///
/// Ops handled: `haptic` (1 byte, 0..=6), `notify` (postcard `(title, body)`),
/// `url.open` / `clipboard.set` / `share.file` (utf8), `keyboard.set` (1 byte, 0/1),
/// and everything under `net.` / `ssh.`.
#[derive(Default)]
pub struct IosOps {
    queue: Mutex<Vec<(String, Vec<u8>)>>,
    net: NetOps,
}

impl IosOps {
    pub fn new() -> Arc<Self> {
        Arc::new(IosOps::default())
    }

    /// Apply queued ops to the capability bridge. Call once per frame from `update`.
    pub fn drain_into(&self, host: &Host) {
        let queued = match self.queue.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => return,
        };
        for (op, payload) in queued {
            match op.as_str() {
                "haptic" => host.haptic(haptic_from_byte(payload.first().copied().unwrap_or(0))),
                "notify" => {
                    if let Ok((title, body)) = postcard::from_bytes::<(String, String)>(&payload) {
                        host.notify(title, body);
                    }
                }
                "url.open" => host.open_url(String::from_utf8_lossy(&payload).into_owned()),
                "clipboard.set" => host.copy_text(String::from_utf8_lossy(&payload).into_owned()),
                "share.file" => host.share_file(String::from_utf8_lossy(&payload).into_owned()),
                "keyboard.set" => host.request_keyboard(payload.first() == Some(&1)),
                _ => {}
            }
        }
    }
}

impl HostOps for IosOps {
    fn call(&self, _plugin: &PluginManifest, op: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        // Network ops run synchronously on background threads; try them first.
        if let Some(result) = self.net.handle(op, payload) {
            return result;
        }
        match op {
            "haptic" | "notify" | "url.open" | "clipboard.set" | "share.file" | "keyboard.set" => {
                self.queue
                    .lock()
                    .map_err(|_| "ops queue poisoned".to_string())?
                    .push((op.to_owned(), payload.to_vec()));
                Ok(Vec::new())
            }
            _ => Err(format!("unknown op {op}")),
        }
    }
}

fn haptic_from_byte(b: u8) -> Haptic {
    match b {
        1 => Haptic::Medium,
        2 => Haptic::Heavy,
        3 => Haptic::Success,
        4 => Haptic::Warning,
        5 => Haptic::Error,
        6 => Haptic::Selection,
        _ => Haptic::Light,
    }
}
