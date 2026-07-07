//! Ready-made manager panel: plugin list with status, enable/reload controls, per-plugin
//! logs, and dev-sync connection controls. The dev-server address and an autoconnect flag
//! persist under the plugins root so a reinstall keeps the last server and reconnects.

use std::path::PathBuf;

use crate::devsync::DevSync;
use crate::manager::PluginManager;
use crate::plugin::PluginStatus;

/// Persisted manager settings, stored as `<root>/settings.json`.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Settings {
    #[serde(default)]
    devsync_addr: String,
    #[serde(default)]
    autoconnect: bool,
}

pub struct PluginManagerUi {
    pub devsync_addr: String,
    pub devsync: Option<DevSync>,
    pub selected: Option<usize>,
    autoconnect: bool,
    /// Where settings persist; resolved from the manager root on first tick.
    settings_path: Option<PathBuf>,
    /// Guards one-time settings load + autoconnect.
    initialized: bool,
}

impl Default for PluginManagerUi {
    fn default() -> Self {
        PluginManagerUi {
            devsync_addr: String::new(),
            devsync: None,
            selected: None,
            autoconnect: true,
            settings_path: None,
            initialized: false,
        }
    }
}

impl PluginManagerUi {
    /// Pump background loads, poll dev-sync, and, on the first call, load persisted
    /// settings and autoconnect. Safe to call every frame regardless of which view is on
    /// screen, so hot-reload pushes land even while a plugin (not the manager) is showing.
    pub fn tick(&mut self, manager: &mut PluginManager, ctx: &egui::Context) {
        if !self.initialized {
            self.initialized = true;
            self.settings_path = Some(manager.root().join("settings.json"));
            self.load_settings();
            if self.autoconnect && !self.devsync_addr.is_empty() {
                self.devsync = Some(DevSync::start(&self.devsync_addr));
            }
        }
        let applied = manager.pump(self.devsync.as_ref(), ctx);
        if applied > 0 {
            log::info!("{applied} plugin(s) loaded");
        }
        if let Some(sync) = &self.devsync {
            manager.poll_devsync(sync, ctx);
            // Keep polling for pushes while connected, even with no user input.
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, manager: &mut PluginManager) {
        self.tick(manager, ui.ctx());

        // Dev sync -------------------------------------------------------------------
        ui.horizontal(|ui| {
            ui.label("Dev server:");
            ui.add(
                egui::TextEdit::singleline(&mut self.devsync_addr)
                    .hint_text("192.168.1.50:7878")
                    .desired_width(150.0),
            );
            match &self.devsync {
                None => {
                    if ui.button("Connect").clicked() && !self.devsync_addr.is_empty() {
                        self.devsync = Some(DevSync::start(&self.devsync_addr));
                        self.save_settings();
                    }
                }
                Some(sync) => {
                    if ui.button("Disconnect").clicked() {
                        sync.stop();
                        self.devsync = None;
                    }
                }
            }
        });
        if ui
            .checkbox(&mut self.autoconnect, "Reconnect on launch")
            .changed()
        {
            self.save_settings();
        }
        if let Some(sync) = &self.devsync {
            ui.weak(sync.status());
        }

        ui.separator();

        // Plugin list ----------------------------------------------------------------
        if ui.button("Rescan plugins dir").clicked() {
            manager.scan(ui.ctx());
        }
        for p in manager.pending_loads() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.weak(format!("{} — {}…", p.name, p.what));
            });
        }
        let mut reload: Option<usize> = None;
        for (i, plugin) in manager.plugins.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let (dot, color) = match (&plugin.status, plugin.enabled) {
                    (PluginStatus::Errored(_), _) => ("*", egui::Color32::RED),
                    (_, false) => ("*", egui::Color32::GRAY),
                    (PluginStatus::Ready, true) => ("*", egui::Color32::GREEN),
                };
                ui.colored_label(color, dot);
                let label = format!("{} v{}", plugin.manifest.name, plugin.manifest.version);
                if ui
                    .selectable_label(self.selected == Some(i), label)
                    .clicked()
                {
                    self.selected = if self.selected == Some(i) { None } else { Some(i) };
                }
                ui.checkbox(&mut plugin.enabled, "");
                if ui.small_button("⟳").on_hover_text("Hot reload from disk").clicked() {
                    reload = Some(i);
                }
            });
        }
        if let Some(i) = reload
            && let Err(e) = manager.reload_at(i, ui.ctx())
        {
            manager.load_errors.push((format!("reload #{i}"), format!("{e:#}")));
        }

        for (what, err) in &manager.load_errors {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("{what}: {err}"));
        }

        // Logs -----------------------------------------------------------------------
        if let Some(i) = self.selected
            && let Some(plugin) = manager.plugins.get(i)
        {
            ui.separator();
            if let PluginStatus::Errored(e) = &plugin.status {
                ui.colored_label(egui::Color32::LIGHT_RED, e);
            }
            egui::ScrollArea::vertical()
                .max_height(160.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for (level, line) in plugin.logs() {
                        let color = match level {
                            4 => egui::Color32::LIGHT_RED,
                            3 => egui::Color32::YELLOW,
                            _ => ui.visuals().text_color(),
                        };
                        ui.colored_label(color, egui::RichText::new(line).monospace());
                    }
                });
        }
    }

    fn load_settings(&mut self) {
        let Some(path) = &self.settings_path else { return };
        let Ok(text) = std::fs::read_to_string(path) else { return };
        if let Ok(s) = serde_json::from_str::<Settings>(&text) {
            self.devsync_addr = s.devsync_addr;
            self.autoconnect = s.autoconnect;
        }
    }

    fn save_settings(&self) {
        let Some(path) = &self.settings_path else { return };
        let s = Settings {
            devsync_addr: self.devsync_addr.clone(),
            autoconnect: self.autoconnect,
        };
        if let Ok(text) = serde_json::to_string_pretty(&s) {
            let _ = std::fs::write(path, text);
        }
    }
}
