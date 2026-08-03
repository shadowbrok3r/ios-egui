//! Android plugin-host example: the same wasmtime plugin manager + viewport + dev-sync hot reload
//! as `plugins-ios`, built as an Android cdylib. Plugins are WASM, so a plugin built for iOS runs
//! here unchanged.

mod store;

use std::sync::Arc;

use egui_android::egui;
use egui_android::plugins::{AndroidOps, HostOps, PluginManager, PluginManagerUi};
use egui_android::{CreateContext, EguiApp, Host, app};

struct App {
    ops: Arc<AndroidOps>,
    manager: Option<PluginManager>,
    manager_ui: PluginManagerUi,
    store: store::Store,
    show_store: bool,
    selected: usize,
    show_manager: bool,
    wants_keyboard: bool,
}

impl App {
    fn new(_cc: &CreateContext) -> Self {
        App {
            ops: AndroidOps::new(),
            manager: None,
            manager_ui: PluginManagerUi::default(),
            store: store::Store::new(),
            show_store: false,
            selected: 0,
            show_manager: true,
            wants_keyboard: false,
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
                        self.store.bind_root(manager.root());
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
                    if ui.selectable_label(self.show_manager && !self.show_store, "Manager").clicked() {
                        pick = Some(None);
                        ui.close();
                    }
                    if ui.selectable_label(self.show_store, "Plugin store").clicked() {
                        pick = Some(Some(usize::MAX));
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
                    None => {
                        self.show_manager = true;
                        self.show_store = false;
                    }
                    // usize::MAX is the store sentinel; it is not a plugin index.
                    Some(usize::MAX) => {
                        self.show_manager = true;
                        self.show_store = true;
                    }
                    Some(i) => {
                        self.selected = i;
                        self.show_manager = false;
                        self.show_store = false;
                    }
                }
            }
        });
        ui.separator();

        // Poll dev-sync every frame (autoconnect + hot-reload pushes).
        self.manager_ui.tick(manager, ui.ctx());

        // Desired keyboard state this frame; the manager view never wants it.
        let mut wants_keyboard = false;
        if self.show_store {
            self.store.poll(manager, ui.ctx());
            show_store_ui(ui, &mut self.store, manager);
        } else if self.show_manager || manager.plugins.is_empty() {
            self.manager_ui.ui(ui, manager);
            if manager.plugins.is_empty() {
                ui.separator();
                ui.label("No plugins yet — push a .wasm plugin to <filesDir>/plugins,");
                ui.label("or connect to the dev-sync server above for wireless hot reload.");
            }
        } else {
            let index = self.selected.min(manager.plugins.len() - 1);
            // Shrink the viewport by the keyboard overlap beyond the already-inset nav bar.
            let bottom = (host.keyboard_height() - host.safe_area_insets().bottom).max(0.0);
            let avail = ui.available_size();
            let size = egui::vec2(avail.x, (avail.y - bottom).max(64.0));
            let response = ui.allocate_ui(size, |ui| manager.show_plugin(ui, index)).inner;
            wants_keyboard = response.wants_keyboard;

            // Cross-plugin hand-off: Devices asks the terminal to SSH into a host.
            for ev in &response.events {
                if ev.topic == egui_android::plugins::abi::net::EVENT_SSH_OPEN
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

        // Apply queued plugin ops (haptics, notifications, …) via the Android host bridge.
        self.ops.drain_into(host);
        ui.ctx().request_repaint();
    }
}

/// Catalog browser: server settings, the plugin list, and per-row install/update.
fn show_store_ui(ui: &mut egui::Ui, store: &mut store::Store, manager: &PluginManager) {
    ui.horizontal(|ui| {
        ui.label("Store");
        ui.add(
            egui::TextEdit::singleline(&mut store.settings.url)
                .hint_text("appstore.example.com")
                .desired_width(200.0),
        );
        if ui.button("Save").clicked() {
            store.save_settings();
            store.refresh(ui.ctx());
        }
    });
    ui.horizontal(|ui| {
        ui.label("Key");
        ui.add(
            egui::TextEdit::singleline(&mut store.settings.key)
                .password(true)
                .desired_width(200.0),
        );
        if ui.add_enabled(!store.busy, egui::Button::new("Refresh")).clicked() {
            store.refresh(ui.ctx());
        }
        if store.busy {
            ui.spinner();
        }
    });
    if !store.status.is_empty() {
        ui.label(egui::RichText::new(&store.status).small());
    }
    ui.separator();

    let mut install: Option<String> = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        for p in &store.plugins {
            let installed = manager.plugins.iter().find(|lp| lp.manifest.id == p.id);
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(&p.name).strong());
                    ui.label(egui::RichText::new(format!("v{}", p.version)).small());
                });
                ui.label(egui::RichText::new(&p.id).small().color(egui::Color32::from_gray(140)));
                if !p.description.is_empty() {
                    ui.label(egui::RichText::new(&p.description).small());
                }
                if !p.permissions.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("permissions: {}", p.permissions.join(", ")))
                            .small()
                            .color(egui::Color32::from_gray(150)),
                    );
                }
                ui.horizontal(|ui| {
                    let label = match installed {
                        Some(lp) if lp.manifest.version == p.version => "Reinstall",
                        Some(_) => "Update",
                        None => "Install",
                    };
                    if let Some(lp) = installed {
                        ui.label(
                            egui::RichText::new(format!("installed v{}", lp.manifest.version))
                                .small()
                                .color(egui::Color32::from_rgb(90, 220, 120)),
                        );
                    }
                    let busy_here = store.installing.as_deref() == Some(p.id.as_str());
                    if ui.add_enabled(!store.busy, egui::Button::new(label)).clicked() {
                        install = Some(p.id.clone());
                    }
                    if busy_here {
                        ui.spinner();
                    }
                });
            });
        }
        if store.plugins.is_empty() && !store.busy {
            ui.label(egui::RichText::new("no plugins — set the URL and key, then Refresh").weak());
        }
    });
    if let Some(id) = install {
        store.install(&id, ui.ctx());
    }
}

app!(App::new);
