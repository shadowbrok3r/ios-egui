//! Android plugin-host example: the same wasmtime plugin manager + viewport + dev-sync hot reload
//! as `plugins-ios`, built as an Android cdylib. Plugins are WASM, so a plugin built for iOS runs
//! here unchanged.

use std::sync::Arc;

use egui_android::egui;
use egui_android::plugins::{AndroidOps, HostOps, PluginManager, PluginManagerUi};
use egui_android::{CreateContext, EguiApp, Host, app};

struct App {
    ops: Arc<AndroidOps>,
    manager: Option<PluginManager>,
    manager_ui: PluginManagerUi,
    selected: usize,
    show_manager: bool,
}

impl App {
    fn new(_cc: &CreateContext) -> Self {
        App {
            ops: AndroidOps::new(),
            manager: None,
            manager_ui: PluginManagerUi::default(),
            selected: 0,
            show_manager: true,
        }
    }
}

impl EguiApp for App {
    fn theme(&self, ctx: &egui::Context) {
        ctx.set_visuals(egui::Visuals::dark());
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        // Plugins live in <filesDir>/plugins; the documents dir arrives shortly after startup.
        if self.manager.is_none() {
            if let Some(docs) = host.documents_dir() {
                match PluginManager::new(
                    format!("{docs}/plugins"),
                    Arc::clone(&self.ops) as Arc<dyn HostOps>,
                    "android",
                ) {
                    Ok(mut manager) => {
                        manager.scan(ui.ctx());
                        self.manager = Some(manager);
                    }
                    Err(e) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("{e:#}"));
                    }
                }
            } else {
                ui.spinner();
                return;
            }
        }
        let Some(manager) = &mut self.manager else {
            return;
        };

        // A dropdown to pick the Manager or a loaded plugin (plain ASCII: the default egui fonts
        // don't include the fancy glyphs the iOS build uses via its theme font).
        ui.horizontal(|ui| {
            let current = if self.show_manager {
                "Menu: Manager".to_owned()
            } else {
                manager
                    .plugins
                    .get(self.selected)
                    .map(|p| format!("Menu: {}", p.manifest.name))
                    .unwrap_or_else(|| "Menu: Manager".to_owned())
            };
            let mut pick: Option<Option<usize>> = None;
            ui.menu_button(current, |ui| {
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    if ui.selectable_label(self.show_manager, "Manager").clicked() {
                        pick = Some(None);
                        ui.close();
                    }
                    if !manager.plugins.is_empty() {
                        ui.separator();
                    }
                    for (i, plugin) in manager.plugins.iter().enumerate() {
                        let selected = !self.show_manager && self.selected == i;
                        if ui.selectable_label(selected, plugin.manifest.name.clone()).clicked() {
                            pick = Some(Some(i));
                            ui.close();
                        }
                    }
                });
            });
            if let Some(sel) = pick {
                match sel {
                    None => self.show_manager = true,
                    Some(i) => {
                        self.selected = i;
                        self.show_manager = false;
                    }
                }
            }
        });
        ui.separator();

        // Poll dev-sync every frame (autoconnect + hot-reload pushes).
        self.manager_ui.tick(manager, ui.ctx());

        if self.show_manager || manager.plugins.is_empty() {
            self.manager_ui.ui(ui, manager);
            if manager.plugins.is_empty() {
                ui.separator();
                ui.label("No plugins yet — push a .wasm plugin to <filesDir>/plugins,");
                ui.label("or connect to the dev-sync server above for wireless hot reload.");
            }
        } else {
            let index = self.selected.min(manager.plugins.len() - 1);
            let _ = manager.show_plugin(ui, index);
        }

        // Apply queued plugin ops (haptics, notifications, …) via the Android host bridge.
        self.ops.drain_into(host);
        ui.ctx().request_repaint();
    }
}

app!(App::new);
