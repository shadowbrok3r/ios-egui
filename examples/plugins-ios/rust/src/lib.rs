//! iOS plugin-host example: plugin manager + viewport + dev-sync hot reload, all on device.

use std::sync::Arc;

use egui_ios::egui;
use egui_ios::plugins::{HostOps, IosOps, PluginManager, PluginManagerUi};
use egui_ios::{CreateContext, EguiApp, Host, app};

struct App {
    ops: Arc<IosOps>,
    manager: Option<PluginManager>,
    manager_ui: PluginManagerUi,
    selected: usize,
    show_manager: bool,
    wants_keyboard: bool,
}

impl App {
    fn new(_cc: &CreateContext) -> Self {
        App {
            ops: IosOps::new(),
            manager: None,
            manager_ui: PluginManagerUi::default(),
            selected: 0,
            show_manager: true,
            wants_keyboard: false,
        }
    }
}

impl EguiApp for App {
    // No theme override: the runtime applies the default Mastertech theme before this runs.

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        // Plugins live in Documents/plugins; the documents dir arrives shortly after startup.
        if self.manager.is_none() {
            if let Some(docs) = host.documents_dir() {
                match PluginManager::new(
                    format!("{docs}/plugins"),
                    Arc::clone(&self.ops) as Arc<dyn HostOps>,
                    "ios",
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
        let Some(manager) = &mut self.manager else { return };

        let insets = host.safe_area_insets();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(insets.top);
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.show_manager, "⚙ manager")
                    .clicked()
                {
                    self.show_manager = !self.show_manager;
                }
                for (i, plugin) in manager.plugins.iter().enumerate() {
                    if ui
                        .selectable_label(!self.show_manager && self.selected == i, &plugin.manifest.name)
                        .clicked()
                    {
                        self.selected = i;
                        self.show_manager = false;
                    }
                }
            });
            ui.separator();

            // Poll dev-sync every frame (autoconnect + hot-reload pushes) regardless of view.
            self.manager_ui.tick(manager, ui.ctx());

            // Desired keyboard state this frame; the manager view never wants it.
            let mut wants_keyboard = false;
            if self.show_manager || manager.plugins.is_empty() {
                self.manager_ui.ui(ui, manager);
                if manager.plugins.is_empty() {
                    ui.separator();
                    ui.label("No plugins yet — run `cargo egui-ios plugin serve` on your dev");
                    ui.label("machine and connect to it above for wireless hot reload.");
                }
            } else {
                let index = self.selected.min(manager.plugins.len() - 1);
                // Shrink the viewport so the soft keyboard doesn't cover the plugin.
                let bottom = host.keyboard_height().max(insets.bottom);
                let avail = ui.available_size();
                let size = egui::vec2(avail.x, (avail.y - bottom).max(64.0));
                let response = ui.allocate_ui(size, |ui| manager.show_plugin(ui, index)).inner;
                wants_keyboard = response.wants_keyboard;

                // Cross-plugin hand-off: Devices asks the terminal to SSH into a host.
                for ev in &response.events {
                    if ev.topic == egui_ios::plugins::abi::net::EVENT_SSH_OPEN
                        && manager.send_event_to("com.example.terminal", &ev.topic, &ev.payload)
                    {
                        if let Some(t) = manager.index_of("com.example.terminal") {
                            self.selected = t;
                        }
                    }
                }
            }
            // Reconcile on every path so leaving a plugin lowers the keyboard.
            if wants_keyboard != self.wants_keyboard {
                self.wants_keyboard = wants_keyboard;
                host.request_keyboard(self.wants_keyboard);
            }
            ui.add_space(insets.bottom);
        });

        // Apply queued plugin ops (haptics, notifications, …) on the main thread.
        self.ops.drain_into(host);
        ui.ctx().request_repaint();
    }
}

app!(App::new);
