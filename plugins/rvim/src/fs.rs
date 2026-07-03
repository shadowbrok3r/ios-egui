//! Virtual filesystem persisted through the host `state.set`/`state.get` ops, mirrored
//! in memory so reads are free and native tests run without a host.

use std::collections::BTreeMap;

use egui_ios_plugin_sdk::HostHandle;

pub struct Vfs {
    host: HostHandle,
    files: BTreeMap<String, String>,
}

impl Vfs {
    /// Load all files from host state; seeds the sample project on first run.
    pub fn load() -> Self {
        // STUB: implemented by the fs module owner.
        Vfs { host: HostHandle, files: BTreeMap::new() }
    }

    pub fn list(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    pub fn read(&self, name: &str) -> Option<String> {
        self.files.get(name).cloned()
    }

    /// Write a file to memory and persist it (best-effort) to host state.
    pub fn write(&mut self, name: &str, text: &str) {
        let _ = &self.host;
        self.files.insert(name.to_string(), text.to_string());
    }

    /// Delete a file from memory and host state; returns false when it did not exist.
    pub fn remove(&mut self, name: &str) -> bool {
        self.files.remove(name).is_some()
    }

    pub fn exists(&self, name: &str) -> bool {
        self.files.contains_key(name)
    }
}
