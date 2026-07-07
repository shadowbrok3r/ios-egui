//! Desktop twin of the iOS plugin runtime: hosts the same WASM plugins in eframe.
//!
//! ```bash
//! cargo egui-ios plugin build --out ../../plugins-dist   # from each plugin dir, or:
//! cargo run -p desktop-host -- <plugins-dir>
//! ```

use std::sync::Arc;

use egui_ios_plugin_host::abi::PluginManifest;
use egui_ios_plugin_host::{HostOps, PluginManager, PluginManagerUi, install};

/// Desktop stand-ins for the iOS capability ops: log and accept. Network ops
/// run natively through the same `NetOps` backend the iOS runtime uses.
#[derive(Default)]
struct DesktopOps {
    net: egui_ios_plugin_host::NetOps,
}

impl HostOps for DesktopOps {
    fn call(&self, plugin: &PluginManifest, op: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        if let Some(r) = self.net.handle(op, payload) {
            return r;
        }
        match op {
            "haptic" => Ok(Vec::new()),
            "notify" => {
                if let Ok((title, body)) = postcard::from_bytes::<(String, String)>(payload) {
                    log::info!("[{}] notify: {title} — {body}", plugin.id);
                }
                Ok(Vec::new())
            }
            "url.open" | "clipboard.set" | "share.file" | "keyboard.set" => Ok(Vec::new()),
            _ => Err(format!("unknown op {op}")),
        }
    }
}

struct HostApp {
    manager: PluginManager,
    manager_ui: PluginManagerUi,
    selected: usize,
    events: Vec<String>,
}

impl HostApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rs = cc
            .wgpu_render_state
            .as_ref()
            .expect("desktop-host requires the wgpu backend");
        install(&mut rs.renderer.write(), rs.target_format, 1);
        egui_ios_plugin_abi::theme::apply(&cc.egui_ctx);

        let root = std::env::args().nth(1).unwrap_or_else(|| "plugins-dist".into());
        let mut manager =
            PluginManager::new(root, Arc::new(DesktopOps::default()), "desktop").expect("plugin manager");
        manager.scan(&cc.egui_ctx);
        HostApp {
            manager,
            manager_ui: PluginManagerUi::default(),
            selected: 0,
            events: Vec::new(),
        }
    }
}

impl eframe::App for HostApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::left("manager").default_size(300.0).show(ui, |ui| {
            ui.heading("Plugins");
            self.manager_ui.ui(ui, &mut self.manager);
            if !self.events.is_empty() {
                ui.separator();
                ui.heading("Events");
                for line in self.events.iter().rev().take(10) {
                    ui.weak(line);
                }
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.manager.plugins.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.heading("no plugins loaded");
                    ui.label("stage plugins into ./plugins-dist with:");
                    ui.monospace("cargo egui-ios plugin build --out <this dir>/plugins-dist");
                    ui.label("or connect to a dev server from the sidebar");
                });
                return;
            }

            ui.horizontal(|ui| {
                for (i, plugin) in self.manager.plugins.iter().enumerate() {
                    if ui
                        .selectable_label(self.selected == i, &plugin.manifest.name)
                        .clicked()
                    {
                        self.selected = i;
                    }
                }
            });
            ui.separator();

            let index = self.selected.min(self.manager.plugins.len() - 1);
            let response = self.manager.show_plugin(ui, index);
            for event in response.events {
                self.events.push(format!(
                    "{}: {}",
                    event.topic,
                    String::from_utf8_lossy(&event.payload)
                ));
            }
        });
    }
}

fn main() -> eframe::Result {
    env_logger::init();
    eframe::run_native(
        "egui-ios plugin host (desktop)",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Wgpu,
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(HostApp::new(cc)))),
    )
}
