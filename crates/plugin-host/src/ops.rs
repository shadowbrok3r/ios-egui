//! Host ops: the synchronous call surface plugins reach through `host_call`.
//! `state.get`/`state.set` are built in; everything else is dispatched to the
//! embedding app's [`HostOps`] and gated by the plugin's manifest permissions.

use egui_ios_plugin_abi::PluginManifest;

/// App-provided op dispatch. Ops are dot-namespaced strings (`haptic`, `net.tcp.connect`);
/// payloads and responses are opaque bytes (postcard by convention).
///
/// `Send + Sync` because the wasmtime store data holds it; fire-and-forget ops that must
/// touch main-thread-only APIs (e.g. `egui_ios::Host`) should queue through a mutex and be
/// drained by the app after the frame.
pub trait HostOps: Send + Sync {
    fn call(&self, plugin: &PluginManifest, op: &str, payload: &[u8]) -> Result<Vec<u8>, String>;
}

/// Denies every op. Useful for tests and pure-UI plugin hosts.
pub struct NoOps;

impl HostOps for NoOps {
    fn call(&self, _plugin: &PluginManifest, op: &str, _payload: &[u8]) -> Result<Vec<u8>, String> {
        Err(format!("op {op} not provided by this host"))
    }
}

/// Sanitize a state key into a filename (alphanumerics kept, the rest `_`).
pub(crate) fn state_file_name(key: &str) -> String {
    let mut name: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    name.truncate(64);
    format!("{name}.bin")
}
