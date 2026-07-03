//! Shared wasmtime engine. On iOS wasmtime targets Pulley (its portable interpreter) because
//! third-party apps cannot map executable memory; elsewhere Cranelift JIT is used.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

/// Milliseconds per epoch tick; guest-call deadlines are expressed in ticks.
pub const EPOCH_TICK_MS: u64 = 20;

/// Ticks a plugin frame may run before it traps (~500 ms).
pub const FRAME_DEADLINE_TICKS: u64 = 25;

/// Ticks allowed for create/save/restore (~10 s; first frame under Pulley is slow).
pub const COLD_DEADLINE_TICKS: u64 = 500;

/// Wraps `wasmtime::Engine` plus the epoch ticker thread that enforces call deadlines.
pub struct PluginEngine {
    engine: wasmtime::Engine,
    stop: Arc<AtomicBool>,
}

impl PluginEngine {
    pub fn new() -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        // Explicit for clarity; wasmtime also auto-selects Pulley on iOS.
        #[cfg(target_os = "ios")]
        config.target("pulley64").map_err(crate::wt_err)?;

        let engine = wasmtime::Engine::new(&config).map_err(crate::wt_err)?;

        let stop = Arc::new(AtomicBool::new(false));
        {
            let engine = engine.clone();
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("plugin-epoch".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(EPOCH_TICK_MS));
                        engine.increment_epoch();
                    }
                })?;
        }

        Ok(PluginEngine { engine, stop })
    }

    pub fn inner(&self) -> &wasmtime::Engine {
        &self.engine
    }
}

impl Drop for PluginEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
