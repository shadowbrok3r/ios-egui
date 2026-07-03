//! Loads the staged network plugins (`plugins-dist/*`) in the real host with a `NetOps`-backed
//! op surface and runs a few frames — verifying they instantiate over the ABI and paint. This
//! catches handshake/decode regressions a bare wasm compile can't. Requires the `net` feature.
#![cfg(feature = "net")]

use std::path::PathBuf;
use std::sync::Arc;

use egui_ios_plugin_abi::PluginManifest;
use egui_ios_plugin_host::abi::{self, CreateConfig, FrameInput};
use egui_ios_plugin_host::{HostOps, LoadedPlugin, NetOps, PluginEngine, PluginStatus};

/// Ops that route `net.*`/`ssh.*` to `NetOps` and accept the main-thread ops as no-ops.
struct SmokeOps(NetOps);

impl HostOps for SmokeOps {
    fn call(&self, _m: &PluginManifest, op: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        if let Some(r) = self.0.handle(op, payload) {
            return r;
        }
        match op {
            "haptic" | "clipboard.set" | "keyboard.set" | "notify" | "url.open" | "share.file" => {
                Ok(Vec::new())
            }
            _ => Err(format!("unknown op {op}")),
        }
    }
}

fn dist_dir(id: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins-dist").join(id)
}

fn create_config() -> CreateConfig {
    CreateConfig {
        abi_version: abi::ABI_VERSION,
        wire_format: abi::WIRE_FORMAT,
        pixels_per_point: 2.0,
        dark_mode: true,
        host_name: "test".into(),
    }
}

fn frame_input(time: f64) -> FrameInput {
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(360.0, 640.0));
    let mut viewports = egui::ViewportIdMap::default();
    viewports.insert(
        egui::ViewportId::ROOT,
        egui::ViewportInfo {
            native_pixels_per_point: Some(2.0),
            inner_rect: Some(rect),
            focused: Some(true),
            ..Default::default()
        },
    );
    FrameInput {
        raw_input: egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            viewports,
            screen_rect: Some(rect),
            time: Some(time),
            focused: true,
            ..Default::default()
        },
    }
}

fn load_and_frame(id: &str) {
    let dir = dist_dir(id);
    if !dir.join("plugin.wasm").exists() {
        panic!("staged plugin missing: {} — build + stage plugins-dist first", dir.display());
    }
    let engine = PluginEngine::new().expect("engine");
    let ops = Arc::new(SmokeOps(NetOps::new()));
    let mut plugin = LoadedPlugin::load(&engine, ops, &dir, &create_config())
        .unwrap_or_else(|e| panic!("load {id}: {e:#}"));
    assert_eq!(plugin.status, PluginStatus::Ready, "{id} should be Ready");

    for t in 0..3 {
        let frame = plugin
            .run_frame(&frame_input(t as f64 * 0.05))
            .unwrap_or_else(|e| panic!("{id} frame {t}: {e:#}"));
        assert!(!frame.primitives.is_empty(), "{id} frame {t} should paint");
    }
    assert_eq!(plugin.status, PluginStatus::Ready, "{id} still Ready after frames");
}

#[test]
fn http_client_loads_and_paints() {
    load_and_frame("com.example.http");
}

#[test]
fn devices_loads_and_paints() {
    load_and_frame("com.example.devices");
}

#[test]
fn terminal_loads_and_paints() {
    load_and_frame("com.example.terminal");
}
