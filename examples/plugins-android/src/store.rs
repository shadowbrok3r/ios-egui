//! Browse and install plugins from an appstore plugin store.
//!
//! The dev-sync client next door speaks plaintext HTTP/1.0 and cannot reach an HTTPS
//! host, so this fetches with `ureq` (already in the build graph via the host's `net`
//! feature) on a worker thread and hands the bytes to `PluginManager::install_bytes`,
//! which writes them under the managed root and hot-loads the plugin.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use egui_android::egui;
use egui_android::plugins::PluginManager;

const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct StoreSettings {
    pub url: String,
    pub key: String,
}

/// One catalog entry as `/api/plugins` reports it.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct StorePlugin {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub permissions: Vec<String>,
}

/// A fetched bundle, ready for `install_bytes`.
pub struct Fetched {
    pub id: String,
    pub manifest: String,
    pub wasm: Vec<u8>,
}

enum Msg {
    Catalog(Result<Vec<StorePlugin>, String>),
    Fetched(Result<Fetched, String>),
}

pub struct Store {
    pub settings: StoreSettings,
    pub plugins: Vec<StorePlugin>,
    pub status: String,
    pub busy: bool,
    /// Which id is mid-install, so only its row shows a spinner.
    pub installing: Option<String>,
    rx: Option<Receiver<Msg>>,
    settings_path: Option<PathBuf>,
}

impl Store {
    pub fn new() -> Self {
        Store {
            settings: StoreSettings::default(),
            plugins: Vec::new(),
            status: String::new(),
            busy: false,
            installing: None,
            rx: None,
            settings_path: None,
        }
    }

    /// `scan()` only considers directories holding a plugin.wasm, so a dotfile beside
    /// them is invisible to the manager.
    pub fn bind_root(&mut self, root: &std::path::Path) {
        if self.settings_path.is_some() {
            return;
        }
        let path = root.join(".store.json");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(s) = serde_json::from_str::<StoreSettings>(&text)
        {
            self.settings = s;
        }
        self.settings_path = Some(path);
    }

    pub fn save_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else { return };
        match serde_json::to_string_pretty(&self.settings) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    self.status = format!("could not save settings: {e}");
                }
            }
            Err(e) => self.status = format!("could not save settings: {e}"),
        }
    }

    fn base(&self) -> String {
        let s = self.settings.url.trim().trim_end_matches('/');
        if s.starts_with("http://") || s.starts_with("https://") {
            s.to_string()
        } else {
            format!("https://{s}")
        }
    }

    pub fn refresh(&mut self, ctx: &egui::Context) {
        if self.busy || self.settings.url.trim().is_empty() {
            if self.settings.url.trim().is_empty() {
                self.status = "set the store URL first".into();
            }
            return;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.status = "loading catalog…".into();
        let (base, key, ctx) = (self.base(), self.settings.key.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Catalog(fetch_catalog(&base, &key)));
            ctx.request_repaint();
        });
    }

    pub fn install(&mut self, id: &str, ctx: &egui::Context) {
        if self.busy {
            return;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.installing = Some(id.to_string());
        self.status = format!("downloading {id}…");
        let (base, key, id, ctx) = (self.base(), self.settings.key.clone(), id.to_string(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Fetched(fetch_plugin(&base, &key, &id)));
            ctx.request_repaint();
        });
    }

    /// Drain worker results; installs land through `manager`. Returns true on a change.
    pub fn poll(&mut self, manager: &mut PluginManager, ctx: &egui::Context) -> bool {
        let Some(rx) = self.rx.as_ref() else { return false };
        let msg = match rx.try_recv() {
            Ok(m) => m,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                self.busy = false;
                self.installing = None;
                return false;
            }
        };
        self.rx = None;
        self.busy = false;
        match msg {
            Msg::Catalog(Ok(list)) => {
                self.status = format!("{} plugins", list.len());
                self.plugins = list;
            }
            Msg::Catalog(Err(e)) => self.status = e,
            Msg::Fetched(Ok(f)) => {
                self.installing = None;
                match manager.install_bytes(&f.manifest, &f.wasm, ctx) {
                    Ok(()) => self.status = format!("installing {}…", f.id),
                    Err(e) => self.status = format!("install failed: {e:#}"),
                }
            }
            Msg::Fetched(Err(e)) => {
                self.installing = None;
                self.status = e;
            }
        }
        true
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(TIMEOUT).build()
}

fn get(agent: &ureq::Agent, url: &str, key: &str) -> Result<ureq::Response, String> {
    agent
        .get(url)
        .set("x-api-key", key)
        .set("Accept", "application/json, */*")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => match code {
                401 => "the store rejected the API key".to_string(),
                404 => "not found on the store".to_string(),
                _ => format!("HTTP {code}"),
            },
            ureq::Error::Transport(t) => t.to_string(),
        })
}

fn fetch_catalog(base: &str, key: &str) -> Result<Vec<StorePlugin>, String> {
    let resp = get(&agent(), &format!("{base}/api/plugins"), key)?;
    // Parsed from the string rather than ureq's `json` feature, which is not enabled
    // for the copy of ureq the host crate already pulls in.
    let text = resp.into_string().map_err(|e| format!("bad response: {e}"))?;
    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad response: {e}"))?;
    serde_json::from_value(body["plugins"].clone()).map_err(|e| e.to_string())
}

fn fetch_plugin(base: &str, key: &str, id: &str) -> Result<Fetched, String> {
    let agent = agent();
    let manifest = get(&agent, &format!("{base}/plugins/{id}/manifest.toml"), key)?
        .into_string()
        .map_err(|e| format!("bad manifest: {e}"))?;
    let resp = get(&agent, &format!("{base}/plugins/{id}/plugin.wasm"), key)?;
    let mut wasm = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut wasm)
        .map_err(|e| format!("download failed: {e}"))?;
    if !wasm.starts_with(b"\0asm") {
        return Err("that download is not a wasm module".into());
    }
    Ok(Fetched { id: id.to_string(), manifest, wasm })
}
