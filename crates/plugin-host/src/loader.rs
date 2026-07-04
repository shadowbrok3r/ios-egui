//! Background plugin loader. Compiling a module (cranelift, even targeting Pulley) and the
//! guest `plugin_create` call both cost seconds, so they run on a dedicated worker thread;
//! the UI thread submits jobs and integrates finished [`LoadedPlugin`]s via
//! [`PluginManager::pump`](crate::PluginManager::pump). Jobs run one at a time, capping
//! peak compile memory when several plugins load at once.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Context as _, Result};
use egui_ios_plugin_abi::CreateConfig;

use crate::engine::PluginEngine;
use crate::ops::HostOps;
use crate::plugin::LoadedPlugin;

pub(crate) struct LoadJob {
    pub dir: PathBuf,
    pub cfg: CreateConfig,
    /// `(manifest_toml, wasm)` written into `dir` before loading.
    pub install: Option<(String, Vec<u8>)>,
    /// `(id, hash)` of a dev-sync push, confirmed back to the poller on success.
    pub devsync: Option<(String, String)>,
}

pub(crate) struct LoadDone {
    pub dir: PathBuf,
    pub devsync: Option<(String, String)>,
    pub result: Result<LoadedPlugin>,
}

/// Handle to the worker thread; dropping it lets the worker drain its queue and exit.
pub(crate) struct Loader {
    tx: Sender<LoadJob>,
    rx: Receiver<LoadDone>,
}

impl Loader {
    /// The worker takes ownership of the engine and ops; both are shared with live plugins
    /// through the modules/stores they were built with.
    pub fn new(engine: PluginEngine, ops: Arc<dyn HostOps>) -> Result<Loader> {
        let (tx, job_rx) = channel::<LoadJob>();
        let (done_tx, rx) = channel::<LoadDone>();
        std::thread::Builder::new()
            .name("plugin-loader".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let result = run_job(&engine, &ops, &job);
                    let done = LoadDone { dir: job.dir, devsync: job.devsync, result };
                    if done_tx.send(done).is_err() {
                        break;
                    }
                }
            })
            .context("spawning plugin-loader thread")?;
        Ok(Loader { tx, rx })
    }

    pub fn submit(&self, job: LoadJob) {
        let _ = self.tx.send(job);
    }

    pub fn try_recv(&self) -> Option<LoadDone> {
        self.rx.try_recv().ok()
    }
}

fn run_job(engine: &PluginEngine, ops: &Arc<dyn HostOps>, job: &LoadJob) -> Result<LoadedPlugin> {
    if let Some((manifest_toml, wasm)) = &job.install {
        std::fs::create_dir_all(&job.dir)
            .with_context(|| format!("creating {}", job.dir.display()))?;
        std::fs::write(job.dir.join("manifest.toml"), manifest_toml)?;
        std::fs::write(job.dir.join("plugin.wasm"), wasm)?;
    }
    LoadedPlugin::load(engine, Arc::clone(ops), &job.dir, &job.cfg)
}
