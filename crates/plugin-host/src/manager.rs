//! Loads plugins from a directory tree (`<root>/<id>/{plugin.wasm, manifest.toml}`),
//! hot-reloads them with state carried across instances, and applies dev-sync updates.
//! All compilation and guest creation happens on the loader thread; `scan`, `reload_at`,
//! and `install_bytes` only enqueue work, and [`PluginManager::pump`] — called every frame
//! by [`PluginManagerUi::tick`](crate::PluginManagerUi::tick) — integrates the results.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use egui_ios_plugin_abi as abi;
use abi::{CreateConfig, PluginManifest};

use crate::devsync::DevSync;
use crate::engine::PluginEngine;
use crate::loader::{LoadJob, Loader};
use crate::ops::HostOps;
use crate::plugin::{LoadedPlugin, PluginStatus};
use crate::viewport::{PluginViewport, PluginViewportResponse};

/// One in-flight background load, for status UI.
pub struct PendingLoad {
    /// Directory name or plugin id, whichever was known at submit time.
    pub name: String,
    /// What queued it: `"scan"`, `"reload"`, `"install"`, or `"dev sync"`.
    pub what: &'static str,
    dir: PathBuf,
    devsync: Option<(String, String)>,
}

pub struct PluginManager {
    loader: Loader,
    root: PathBuf,
    host_name: String,
    pub plugins: Vec<LoadedPlugin>,
    /// Load failures, for the manager UI. One entry per source; retries overwrite in place.
    pub load_errors: Vec<(String, String)>,
    retired_keys: Vec<u64>,
    pending: Vec<PendingLoad>,
}

impl PluginManager {
    /// `root` is the plugins directory (created if missing); `host_name` reaches guests via
    /// `CreateConfig::host_name` (e.g. `"ios"`, `"desktop"`).
    pub fn new(root: impl Into<PathBuf>, ops: Arc<dyn HostOps>, host_name: &str) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(PluginManager {
            loader: Loader::new(PluginEngine::new()?, ops)?,
            root,
            host_name: host_name.to_owned(),
            plugins: Vec::new(),
            load_errors: Vec::new(),
            retired_keys: Vec::new(),
            pending: Vec::new(),
        })
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Loads queued but not yet finished, oldest first.
    pub fn pending_loads(&self) -> &[PendingLoad] {
        &self.pending
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

    fn is_pending(&self, dir: &Path) -> bool {
        self.pending.iter().any(|p| p.dir == dir)
    }

    fn submit(&mut self, name: String, what: &'static str, job: LoadJob) {
        self.pending.push(PendingLoad {
            name,
            what,
            dir: job.dir.clone(),
            devsync: job.devsync.clone(),
        });
        self.loader.submit(job);
    }

    /// Record a load failure, replacing any earlier failure from the same source so a
    /// dev-sync retry loop cannot grow the list without bound.
    fn push_error(&mut self, what: String, err: String) {
        match self.load_errors.iter_mut().find(|(w, _)| *w == what) {
            Some(entry) => entry.1 = err,
            None => self.load_errors.push((what, err)),
        }
    }

    /// Queue every plugin directory not yet loaded or loading. Failures land in
    /// `load_errors` as the background loads finish.
    pub fn scan(&mut self, ctx: &egui::Context) {
        self.load_errors.clear();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("plugin.wasm").exists() {
                continue;
            }
            if self.plugins.iter().any(|p| p.dir == dir) || self.is_pending(&dir) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let job = LoadJob {
                dir,
                cfg: self.create_cfg(ctx),
                install: None,
                devsync: None,
            };
            self.submit(name, "scan", job);
        }
    }

    /// Queue a hot reload of the plugin at `index` from its directory. The old instance
    /// keeps running until the replacement is ready; its state carries across at swap time.
    pub fn reload_at(&mut self, index: usize, ctx: &egui::Context) -> Result<()> {
        let Some(plugin) = self.plugins.get(index) else {
            bail!("no plugin at index {index}");
        };
        if self.is_pending(&plugin.dir) {
            return Ok(());
        }
        let name = plugin.manifest.name.clone();
        let job = LoadJob {
            dir: plugin.dir.clone(),
            cfg: self.create_cfg(ctx),
            install: None,
            devsync: None,
        };
        self.submit(name, "reload", job);
        Ok(())
    }

    /// Queue an install: plugin files are written under the managed root and the plugin is
    /// (re)loaded, all on the loader thread.
    pub fn install_bytes(
        &mut self,
        manifest_toml: &str,
        wasm: &[u8],
        ctx: &egui::Context,
    ) -> Result<()> {
        self.queue_install(manifest_toml, wasm, None, ctx)
    }

    fn queue_install(
        &mut self,
        manifest_toml: &str,
        wasm: &[u8],
        devsync: Option<(String, String)>,
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
        let what = if devsync.is_some() { "dev sync" } else { "install" };
        let job = LoadJob {
            dir: self.root.join(&safe_id),
            cfg: self.create_cfg(ctx),
            install: Some((manifest_toml.to_owned(), wasm.to_vec())),
            devsync,
        };
        self.submit(safe_id, what, job);
        Ok(())
    }

    /// Queue pending dev-sync pushes; returns how many were queued. A push already in
    /// flight at the same hash is dropped — the poller resends until an install confirms.
    pub fn poll_devsync(&mut self, sync: &DevSync, ctx: &egui::Context) -> usize {
        let mut queued = 0;
        while let Some(update) = sync.try_recv() {
            let key = Some((update.id.clone(), update.hash.clone()));
            if self.pending.iter().any(|p| p.devsync == key) {
                continue;
            }
            match self.queue_install(&update.manifest_toml, &update.wasm, key, ctx) {
                Ok(()) => queued += 1,
                Err(e) => self.push_error(update.id, format!("{e:#}")),
            }
        }
        queued
    }

    /// Integrate finished background loads; returns how many plugins were (re)installed.
    /// Call every frame (`PluginManagerUi::tick` does). Only a successful install is
    /// confirmed back to the dev-sync poller, so a failed push is retried.
    pub fn pump(&mut self, sync: Option<&DevSync>, ctx: &egui::Context) -> usize {
        let mut applied = 0;
        while let Some(done) = self.loader.try_recv() {
            if let Some(i) = self.pending.iter().position(|p| p.dir == done.dir) {
                self.pending.remove(i);
            }
            let name = done
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| done.dir.display().to_string());
            let mut fresh = match done.result {
                Ok(fresh) => fresh,
                Err(e) => {
                    self.push_error(name, format!("{e:#}"));
                    continue;
                }
            };
            self.load_errors.retain(|(w, _)| *w != name);
            if let Some(i) = self.plugins.iter().position(|p| p.dir == done.dir) {
                // Never carry state from a trapped guest — its memory may be inconsistent.
                if self.plugins[i].status == PluginStatus::Ready {
                    let state = self.plugins[i].save_state().unwrap_or_default();
                    if let Err(e) = fresh.restore_state(&state) {
                        // A restore trap degrades to fresh state, not a dead-on-arrival plugin.
                        log::warn!(
                            "plugin {}: state restore failed, starting fresh: {e:#}",
                            fresh.manifest.id
                        );
                        fresh.clear_error();
                    }
                }
                let mut old = std::mem::replace(&mut self.plugins[i], fresh);
                old.destroy();
                self.retired_keys.push(old.instance_key);
            } else if self.plugins.iter().any(|q| q.manifest.id == fresh.manifest.id) {
                // Dedup by manifest id, not directory: a dir whose name differs from its
                // manifest id must not shadow an already-loaded plugin of the same id.
                self.push_error(
                    name,
                    format!("duplicate plugin id {} — ignoring this directory", fresh.manifest.id),
                );
                continue;
            } else {
                self.plugins.push(fresh);
                self.plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
            }
            if let (Some(sync), Some((id, hash))) = (sync, &done.devsync) {
                sync.mark_installed(id, hash);
            }
            applied += 1;
        }
        if applied > 0 {
            ctx.request_repaint();
        }
        if !self.pending.is_empty() {
            // Completions arrive without user input; keep frames coming while work is queued.
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        applied
    }

    /// Show the plugin at `index`. Retired GPU keys from hot reloads ride along and are
    /// freed by the first viewport that actually paints.
    pub fn show_plugin(&mut self, ui: &mut egui::Ui, index: usize) -> PluginViewportResponse {
        PluginViewport::default().show(ui, &mut self.plugins[index], &mut self.retired_keys)
    }

    /// Index of the enabled, ready plugin whose manifest id is `id`.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.plugins.iter().position(|p| p.manifest.id == id)
    }

    /// Deliver a host event to the plugin with manifest id `id` (e.g. a cross-plugin handoff).
    /// Returns whether a matching, ready plugin received it.
    pub fn send_event_to(&mut self, id: &str, topic: &str, payload: &[u8]) -> bool {
        let Some(index) = self.index_of(id) else { return false };
        match self.plugins[index].send_event(topic, payload) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("send_event_to {id}/{topic}: {e:#}");
                false
            }
        }
    }
}
