//! Host runtime for egui-ios WASM plugins.
//!
//! Plugins are wasm32-wasip1 modules built with `egui-ios-plugin-sdk`; each runs a full
//! egui context in-guest and ships tessellated primitives back per frame. This crate loads
//! them (wasmtime; Pulley interpreter on iOS, Cranelift JIT elsewhere), paints them into an
//! egui viewport widget, gates capability calls by manifest permissions, and hot-reloads
//! them — preserving state — from disk or a `cargo egui-ios plugin serve` dev server.
//!
//! One-time setup per host: call [`install`] on your `egui_wgpu::Renderer` so plugin
//! meshes can be painted through callback resources.

mod devsync;
mod engine;
mod loader;
mod manager;
mod manager_ui;
#[cfg(feature = "net")]
mod net_ops;
mod ops;
mod paint;
mod plugin;
mod viewport;

pub use devsync::DevSync;
pub use engine::PluginEngine;
pub use manager::{PendingLoad, PluginManager};
pub use manager_ui::PluginManagerUi;
#[cfg(feature = "net")]
pub use net_ops::NetOps;
pub use ops::{HostOps, NoOps};
pub use paint::install;
pub use plugin::{FrameResult, LoadedPlugin, PluginStatus};
pub use viewport::{PluginViewport, PluginViewportResponse};

pub use egui_ios_plugin_abi as abi;

/// wasmtime 46 ships its own anyhow-like error type; bridge it (Debug format keeps the chain).
pub(crate) fn wt_err(e: wasmtime::Error) -> anyhow::Error {
    anyhow::anyhow!("{e:?}")
}
