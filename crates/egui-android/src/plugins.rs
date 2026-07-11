//! WASM plugin support for Android apps (feature `plugins`). Re-exports the shared wasmtime host
//! and maps standard plugin ops onto the Android capability bridge. The op handlers are identical
//! to iOS (both target the platform-neutral [`Host`]); the wasmtime engine runs the Pulley
//! interpreter on Android too, so there's no JIT/executable-memory requirement.

pub use egui_ios_plugin_host::*;

use std::sync::{Arc, Mutex};

use egui_ios_plugin_host::abi::PluginManifest;

use crate::{Haptic, Host};

/// Standard ops backed by [`Host`], plus native network ops. Main-thread ops queue here and the
/// app applies them once per frame with [`AndroidOps::drain_into`]; network ops run on background
/// threads via [`NetOps`].
#[derive(Default)]
pub struct AndroidOps {
    queue: Mutex<Vec<(String, Vec<u8>)>>,
    net: NetOps,
}

impl AndroidOps {
    pub fn new() -> Arc<Self> {
        Arc::new(AndroidOps::default())
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

impl HostOps for AndroidOps {
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
