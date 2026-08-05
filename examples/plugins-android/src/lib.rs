//! Android plugin-host example: the same wasmtime plugin manager + viewport + dev-sync hot reload
//! as `plugins-ios`, built as an Android cdylib. Plugins are WASM, so a plugin built for iOS runs
//! here unchanged.

mod store;
mod theme;

use std::sync::Arc;

use egui_android::egui;
use egui_android::plugins::{AndroidOps, HostOps, PluginManager, PluginManagerUi, PluginStatus};
use egui_android::{CreateContext, EguiApp, Host, app};

/// Which screen the bottom nav has selected.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Plugin,
    Manage,
    Store,
}

struct App {
    ops: Arc<AndroidOps>,
    manager: Option<PluginManager>,
    manager_ui: PluginManagerUi,
    store: store::Store,
    tab: Tab,
    selected: usize,
    wants_keyboard: bool,
}

impl App {
    fn new(_cc: &CreateContext) -> Self {
        App {
            ops: AndroidOps::new(),
            manager: None,
            manager_ui: PluginManagerUi::default(),
            store: store::Store::new(),
            tab: Tab::Manage,
            selected: 0,
            wants_keyboard: false,
        }
    }
}

/// Bottom nav: three equal tabs, aqua ink on the selected one. A free function so it can run
/// while `self.manager` is mutably borrowed.
fn nav_bar(ui: &mut egui::Ui, tab: &mut Tab, host: &Host) {
    ui.horizontal(|ui| {
        // Width minus the two spacing gaps between three buttons.
        let w = ((ui.available_width() - ui.spacing().item_spacing.x * 2.0) / 3.0).max(48.0);
        for (t, label) in [(Tab::Plugin, "Plugin"), (Tab::Manage, "Manage"), (Tab::Store, "Store")]
        {
            let selected = *tab == t;
            let text = egui::RichText::new(label).size(15.0).color(if selected {
                theme::AQUA_BRIGHT
            } else {
                theme::INK
            });
            if ui.add_sized([w, 42.0], theme::selectable(selected, text)).clicked() && !selected {
                *tab = t;
                host.haptic(egui_android::Haptic::Light);
            }
        }
    });
}

impl EguiApp for App {
    fn theme(&self, ctx: &egui::Context) {
        theme::apply(ctx);
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        theme::ambience(ui.ctx());

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
                        ui.colored_label(theme::PINK, format!("{e:#}"));
                    }
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.4);
                    ui.spinner();
                });
                return;
            }
        }
        let Some(manager) = &mut self.manager else {
            return;
        };

        // Poll dev-sync every frame (autoconnect + hot-reload pushes).
        self.manager_ui.tick(manager, ui.ctx());

        // The nav collapses while typing: focus leads the keyboard's slide-in and the inset trails
        // its slide-out, so the union collapses once and restores once with no gap in between.
        let mut nav_open = !(host.keyboard_height() > 1.0 || ui.ctx().text_edit_focused());
        egui::Panel::bottom("plugins-nav")
            .show_separator_line(false)
            .frame(theme::bar())
            .show_collapsible(ui, &mut nav_open, |ui| {
                nav_bar(ui, &mut self.tab, host);
            });

        // Desired keyboard state this frame; only a plugin viewport can ask for it.
        let mut wants_keyboard = false;
        // eframe's `App::ui` hands over a bare `Ui` with no background, so the page needs a panel
        // of its own or `panel_fill` never paints and `ambience` shows at full strength.
        let page = theme::page(ui.style());
        egui::CentralPanel::default().frame(page).show(ui, |ui| match self.tab {
            Tab::Store => {
                self.store.poll(manager, ui.ctx());
                show_store_ui(ui, &mut self.store, manager);
            }
            Tab::Manage => {
                theme::scroll_vertical().show(ui, |ui| {
                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.manager_ui.ui(ui, manager);
                    });
                    if manager.plugins.is_empty() {
                        ui.add_space(8.0);
                        theme::card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                egui::RichText::new("No plugins yet").color(theme::AQUA_BRIGHT),
                            );
                            ui.label(
                                egui::RichText::new(
                                    "Install one from the Store, push a .wasm to \
                                     <filesDir>/plugins, or connect to the dev-sync server \
                                     above for wireless hot reload.",
                                )
                                .small()
                                .color(theme::INK_DIM),
                            );
                        });
                    }
                });
            }
            Tab::Plugin => {
                if manager.plugins.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.35);
                        ui.label(
                            egui::RichText::new("No plugins installed").color(theme::INK_DIM),
                        );
                        if ui.button("Open the Store").clicked() {
                            self.tab = Tab::Store;
                        }
                    });
                } else {
                    plugin_picker(ui, manager, &mut self.selected);
                    let index = self.selected.min(manager.plugins.len() - 1);
                    // No keyboard math here: the runtime already shrank `screen_rect` by the
                    // keyboard's occlusion, so `available_size` is the area above the keys.
                    let response = manager.show_plugin(ui, index);
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
            }
        });
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

/// Compact strip above a plugin viewport: a status dot plus a dropdown of the loaded plugins.
/// A dropdown rather than a tab strip because the list can grow past the screen width.
fn plugin_picker(ui: &mut egui::Ui, manager: &PluginManager, selected: &mut usize) {
    let index = (*selected).min(manager.plugins.len().saturating_sub(1));
    ui.horizontal(|ui| {
        if let Some(p) = manager.plugins.get(index) {
            theme::status_dot(ui, status_color(&p.status, p.enabled));
        }
        let current = manager
            .plugins
            .get(index)
            .map(|p| p.manifest.name.clone())
            .unwrap_or_else(|| "Plugin".to_owned());
        let mut pick: Option<usize> = None;
        ui.menu_button(current, |ui| {
            theme::scroll_vertical().max_height(360.0).show(ui, |ui| {
                for (i, plugin) in manager.plugins.iter().enumerate() {
                    ui.horizontal(|ui| {
                        theme::status_dot(ui, status_color(&plugin.status, plugin.enabled));
                        if theme::selectable_label(ui, i == index, plugin.manifest.name.clone())
                            .clicked()
                        {
                            pick = Some(i);
                            ui.close();
                        }
                    });
                }
            });
        });
        if let Some(p) = manager.plugins.get(index) {
            ui.label(
                egui::RichText::new(format!("v{}", p.manifest.version))
                    .small()
                    .color(theme::INK_DIM),
            );
        }
        if let Some(i) = pick {
            *selected = i;
        }
    });
}

/// Ready = aqua (the "live" accent), errored = pink, disabled = dim ink.
fn status_color(status: &PluginStatus, enabled: bool) -> egui::Color32 {
    match (status, enabled) {
        (PluginStatus::Errored(_), _) => theme::PINK,
        (_, false) => theme::INK_DIM,
        (PluginStatus::Ready, true) => theme::AQUA,
    }
}

/// Catalog browser: server settings, the plugin list, and per-row install/update.
fn show_store_ui(ui: &mut egui::Ui, store: &mut store::Store, manager: &PluginManager) {
    let mut install: Option<String> = None;
    theme::scroll_vertical().show(ui, |ui| {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new("Store").color(theme::AQUA_BRIGHT));
            ui.add(
                egui::TextEdit::singleline(&mut store.settings.url)
                    .hint_text("appstore.example.com")
                    .desired_width(f32::INFINITY),
            );
            ui.add(
                egui::TextEdit::singleline(&mut store.settings.key)
                    .password(true)
                    .hint_text("API key")
                    .desired_width(f32::INFINITY),
            );
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    store.save_settings();
                    store.refresh(ui.ctx());
                }
                if ui.add_enabled(!store.busy, egui::Button::new("Refresh")).clicked() {
                    store.refresh(ui.ctx());
                }
                if store.busy {
                    ui.spinner();
                }
            });
            if !store.status.is_empty() {
                ui.label(egui::RichText::new(&store.status).small().color(theme::INK_DIM));
            }
        });

        for p in &store.plugins {
            ui.add_space(8.0);
            let installed = manager.plugins.iter().find(|lp| lp.manifest.id == p.id);
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(&p.name).strong().color(theme::INK));
                    ui.label(
                        egui::RichText::new(format!("v{}", p.version))
                            .small()
                            .color(theme::AQUA),
                    );
                });
                ui.label(egui::RichText::new(&p.id).small().color(theme::INK_DIM));
                if !p.description.is_empty() {
                    ui.label(egui::RichText::new(&p.description).small());
                }
                if !p.permissions.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("permissions: {}", p.permissions.join(", ")))
                            .small()
                            .color(theme::VIOLET),
                    );
                }
                ui.horizontal(|ui| {
                    let label = match installed {
                        Some(lp) if lp.manifest.version == p.version => "Reinstall",
                        Some(_) => "Update",
                        None => "Install",
                    };
                    let busy_here = store.installing.as_deref() == Some(p.id.as_str());
                    if ui.add_enabled(!store.busy, egui::Button::new(label)).clicked() {
                        install = Some(p.id.clone());
                    }
                    if busy_here {
                        ui.spinner();
                    }
                    if let Some(lp) = installed {
                        ui.label(
                            egui::RichText::new(format!("installed v{}", lp.manifest.version))
                                .small()
                                .color(theme::AQUA_BRIGHT),
                        );
                    }
                });
            });
        }
        if store.plugins.is_empty() && !store.busy {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("no plugins — set the URL and key, then Refresh")
                    .small()
                    .color(theme::INK_DIM),
            );
        }
    });
    if let Some(id) = install {
        store.install(&id, ui.ctx());
    }
}

app!(App::new);
