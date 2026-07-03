//! End-to-end host test against the real `hello-plugin` wasm: load, frame, state
//! save/restore, and manager hot reload — all headless (no GPU).

use std::path::PathBuf;
use std::sync::Arc;

use egui_ios_plugin_host::abi::{self, CreateConfig, FrameInput};
use egui_ios_plugin_host::{LoadedPlugin, NoOps, PluginEngine, PluginManager, PluginStatus};

fn hello_plugin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/hello-plugin")
}

/// Build hello-plugin for wasm32-wasip1 (cached by cargo) and return the wasm path.
fn build_hello_plugin() -> PathBuf {
    let dir = hello_plugin_dir();
    let status = std::process::Command::new("cargo")
        .args(["build", "--target", "wasm32-wasip1", "--release"])
        .current_dir(&dir)
        .status()
        .expect("running cargo build for hello-plugin");
    assert!(status.success(), "hello-plugin build failed");
    dir.join("target/wasm32-wasip1/release/hello_plugin.wasm")
}

/// Stage `<root>/com.example.hello/{plugin.wasm, manifest.toml}` in a temp dir.
fn stage_plugin(wasm: &std::path::Path) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join("com.example.hello");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(wasm, dir.join("plugin.wasm")).unwrap();
    std::fs::copy(hello_plugin_dir().join("manifest.toml"), dir.join("manifest.toml")).unwrap();
    root
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

fn frame_input(time: f64, events: Vec<egui::Event>) -> FrameInput {
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(320.0, 480.0));
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
            events,
            ..Default::default()
        },
    }
}

#[test]
fn load_frame_state_and_reload() {
    let wasm = build_hello_plugin();
    let root = stage_plugin(&wasm);
    let engine = PluginEngine::new().expect("engine");

    // --- direct load + frames --------------------------------------------------------
    let mut plugin = LoadedPlugin::load(
        &engine,
        Arc::new(NoOps),
        &root.path().join("com.example.hello"),
        &create_config(),
    )
    .expect("plugin load");
    assert_eq!(plugin.status, PluginStatus::Ready);
    assert_eq!(plugin.manifest.id, "com.example.hello");

    let first = plugin.run_frame(&frame_input(0.0, vec![])).expect("frame 1");
    assert!(!first.primitives.is_empty(), "first frame should paint UI");
    assert!(
        !first.textures_set.is_empty(),
        "first frame should upload the font atlas"
    );
    assert!(
        first.platform.repaint_delay_secs.is_some(),
        "hello-plugin animates, so it should request a repaint"
    );
    assert_eq!(first.skipped_callbacks, 0);

    // Texture ids must be remapped into the host's namespaced range.
    for (id, _) in &first.textures_set {
        match id {
            egui::TextureId::User(n) => assert!(n >> 63 == 1, "plugin texture bit set"),
            other => panic!("expected remapped User texture id, got {other:?}"),
        }
    }

    let second = plugin.run_frame(&frame_input(0.05, vec![])).expect("frame 2");
    assert!(!second.primitives.is_empty());

    // --- guest state snapshot --------------------------------------------------------
    let mut state = 7u32.to_le_bytes().to_vec();
    state.extend_from_slice(b"carried across");
    plugin.restore_state(&state).expect("restore");
    let saved = plugin.save_state().expect("save");
    assert_eq!(saved, state, "save must roundtrip what restore loaded");

    // --- manager scan + hot reload preserves state ------------------------------------
    let ctx = egui::Context::default();
    let mut manager = PluginManager::new(root.path(), Arc::new(NoOps), "test").expect("manager");
    manager.scan(&ctx);
    assert_eq!(manager.plugins.len(), 1, "load_errors: {:?}", manager.load_errors);

    manager.plugins[0].restore_state(&state).expect("restore into managed");
    manager.reload_at(0, &ctx).expect("hot reload");
    assert_eq!(manager.plugins[0].status, PluginStatus::Ready);
    let after = manager.plugins[0].save_state().expect("save after reload");
    assert_eq!(after, state, "hot reload must carry guest state across instances");

    // Reloaded instance still renders.
    let frame = manager.plugins[0]
        .run_frame(&frame_input(0.1, vec![]))
        .expect("frame after reload");
    assert!(!frame.primitives.is_empty());
}
