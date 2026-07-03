//! Loads plugins from a directory tree (`<root>/<id>/{plugin.wasm, manifest.toml}`),
//! hot-reloads them with state carried across instances, and applies dev-sync updates.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use egui_ios_plugin_abi as abi;
use abi::{CreateConfig, PluginManifest};

use crate::devsync::DevSync;
use crate::engine::PluginEngine;
use crate::ops::HostOps;
use crate::plugin::{LoadedPlugin, PluginStatus};
use crate::viewport::{PluginViewport, PluginViewportResponse};

pub struct PluginManager {
    engine: PluginEngine,
    ops: Arc<dyn HostOps>,
    root: PathBuf,
    host_name: String,
    pub plugins: Vec<LoadedPlugin>,
    /// Load failures from the last `scan`/install, for the manager UI.
    pub load_errors: Vec<(String, String)>,
    retired_keys: Vec<u64>,
}

impl PluginManager {
    /// `root` is the plugins directory (created if missing); `host_name` reaches guests via
    /// `CreateConfig::host_name` (e.g. `"ios"`, `"desktop"`).
    pub fn new(root: impl Into<PathBuf>, ops: Arc<dyn HostOps>, host_name: &str) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(PluginManager {
            engine: PluginEngine::new()?,
            ops,
            root,
            host_name: host_name.to_owned(),
            plugins: Vec::new(),
            load_errors: Vec::new(),
            retired_keys: Vec::new(),
        })
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    fn create_cfg(&self, ctx: &egui::Context) -> CreateConfig {
        CreateConfig {
            abi_version: abi::ABI_VERSION,
            wire_format: abi::WIRE_FORMAT,
            pixels_per_point: ctx.pixels_per_point(),
            dark_mode: ctx.theme() == egui::Theme::Dark,
            host_name: self.host_name.clone(),
        }
    }

    /// Load every plugin directory not yet loaded. Failures land in `load_errors`.
    pub fn scan(&mut self, ctx: &egui::Context) {
        self.load_errors.clear();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let cfg = self.create_cfg(ctx);
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("plugin.wasm").exists() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            if self.plugins.iter().any(|p| p.dir == dir) {
                continue;
            }
            match LoadedPlugin::load(&self.engine, Arc::clone(&self.ops), &dir, &cfg) {
                Ok(p) => {
                    // Dedup by manifest id, not directory: a dir whose name differs from its
                    // manifest id must not shadow an already-loaded plugin of the same id.
                    if self.plugins.iter().any(|q| q.manifest.id == p.manifest.id) {
                        self.load_errors.push((
                            dir_name,
                            format!("duplicate plugin id {} — ignoring this directory", p.manifest.id),
                        ));
                    } else {
                        self.plugins.push(p);
                    }
                }
                Err(e) => self.load_errors.push((dir_name, format!("{e:#}"))),
            }
        }
        self.plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    }

    /// Hot-reload the plugin at `index` from its directory, carrying guest state across.
    /// The old instance keeps running if the new one fails to load.
    pub fn reload_at(&mut self, index: usize, ctx: &egui::Context) -> Result<()> {
        let cfg = self.create_cfg(ctx);
        let old = &mut self.plugins[index];
        // Never snapshot a trapped guest — its memory may be inconsistent. Start fresh instead.
        let state = if old.status == PluginStatus::Ready {
            old.save_state().unwrap_or_default()
        } else {
            Vec::new()
        };
        let dir = old.dir.clone();
        let mut fresh = LoadedPlugin::load(&self.engine, Arc::clone(&self.ops), &dir, &cfg)?;
        if let Err(e) = fresh.restore_state(&state) {
            // A restore trap must degrade to fresh state, not a dead-on-arrival plugin.
            log::warn!("plugin {}: state restore failed, starting fresh: {e:#}", fresh.manifest.id);
            fresh.clear_error();
        }
        let mut old = std::mem::replace(&mut self.plugins[index], fresh);
        old.destroy();
        self.retired_keys.push(old.instance_key);
        Ok(())
    }

    /// Write plugin files under the managed root and (re)load the plugin.
    pub fn install_bytes(
        &mut self,
        manifest_toml: &str,
        wasm: &[u8],
        ctx: &egui::Context,
    ) -> Result<()> {
        let manifest: PluginManifest = toml::from_str(manifest_toml).context("parsing manifest.toml")?;
        let safe_id: String = manifest
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .collect();
        if safe_id.is_empty() {
            bail!("plugin id {:?} is empty after sanitizing", manifest.id);
        }
        let dir = self.root.join(&safe_id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("manifest.toml"), manifest_toml)?;
        std::fs::write(dir.join("plugin.wasm"), wasm)?;

        if let Some(index) = self.plugins.iter().position(|p| p.dir == dir) {
            self.reload_at(index, ctx)
        } else {
            let cfg = self.create_cfg(ctx);
            let plugin = LoadedPlugin::load(&self.engine, Arc::clone(&self.ops), &dir, &cfg)?;
            self.plugins.push(plugin);
            Ok(())
        }
    }

    /// Show the plugin at `index`. Retired GPU keys from hot reloads ride along and are
    /// freed by the first viewport that actually paints.
    pub fn show_plugin(&mut self, ui: &mut egui::Ui, index: usize) -> PluginViewportResponse {
        PluginViewport::default().show(ui, &mut self.plugins[index], &mut self.retired_keys)
    }

    /// Apply pending dev-sync pushes; returns how many plugins were (re)installed. Only a
    /// successful install is confirmed back to the poller, so a failed push is retried.
    pub fn poll_devsync(&mut self, sync: &DevSync, ctx: &egui::Context) -> usize {
        let mut applied = 0;
        while let Some(update) = sync.try_recv() {
            match self.install_bytes(&update.manifest_toml, &update.wasm, ctx) {
                Ok(()) => {
                    sync.mark_installed(&update.id, &update.hash);
                    applied += 1;
                }
                Err(e) => self.load_errors.push((update.id, format!("{e:#}"))),
            }
        }
        applied
    }
}
