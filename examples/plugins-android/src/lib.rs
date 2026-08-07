//! Android plugin-host example: the same wasmtime plugin manager + viewport + dev-sync hot reload
//! as `plugins-ios`, built as an Android cdylib. Plugins are WASM, so a plugin built for iOS runs
//! here unchanged.

mod store;
mod theme;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui_android::egui;
use egui_android::plugins::{AndroidOps, HostOps, PluginManager, PluginManagerUi, PluginStatus};
use egui_android::{CreateContext, EguiApp, Host, app};

/// Which screen the bottom nav has selected. A plugin tab carries the manifest id rather than an
/// index into `manager.plugins`, which shifts whenever a plugin is installed or removed.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Tab {
    Plugins,
    Manage,
    Store,
    Plugin(String),
}

/// The tab state written to `<root>/.tabs.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedTabs {
    active: Tab,
    open: Vec<String>,
}

/// Bottom-nav state: the three fixed pages plus one tab per opened plugin.
struct Tabs {
    active: Tab,
    /// Manifest ids of the open plugin tabs, in bar order.
    open: Vec<String>,
    /// Scrolls the active tab into view on the next nav frame.
    reveal: bool,
    path: Option<PathBuf>,
    saved: Option<SavedTabs>,
}

impl Tabs {
    fn new() -> Self {
        Tabs { active: Tab::Manage, open: Vec::new(), reveal: false, path: None, saved: None }
    }

    /// Load persisted tabs from `<root>/.tabs.json`, beside the store's own settings dotfile.
    fn bind_root(&mut self, root: &Path) {
        if self.path.is_some() {
            return;
        }
        let path = root.join(".tabs.json");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(s) = serde_json::from_str::<SavedTabs>(&text)
        {
            self.active = s.active.clone();
            self.open = s.open.clone();
            self.saved = Some(s);
        }
        self.path = Some(path);
    }

    /// Write the tab state when it differs from what is already on disk.
    fn save_if_changed(&mut self) {
        let Some(path) = self.path.clone() else { return };
        if self.saved.as_ref().is_some_and(|s| s.active == self.active && s.open == self.open) {
            return;
        }
        let now = SavedTabs { active: self.active.clone(), open: self.open.clone() };
        if let Ok(text) = serde_json::to_string(&now)
            && std::fs::write(&path, text).is_ok()
        {
            self.saved = Some(now);
        }
    }

    fn is_open(&self, id: &str) -> bool {
        self.open.iter().any(|o| o == id)
    }

    /// Add `id` as a tab if it is not already open, then select it.
    fn open_plugin(&mut self, id: &str) {
        if !self.is_open(id) {
            self.open.push(id.to_owned());
        }
        self.active = Tab::Plugin(id.to_owned());
        self.reveal = true;
    }

    /// Close a plugin tab, selecting its neighbour or falling back to the launcher.
    fn close_plugin(&mut self, id: &str) {
        let Some(i) = self.open.iter().position(|o| o == id) else { return };
        self.open.remove(i);
        if self.active == Tab::Plugin(id.to_owned()) {
            let next = self.open.get(i).or_else(|| i.checked_sub(1).and_then(|p| self.open.get(p)));
            self.active = match next {
                Some(id) => Tab::Plugin(id.clone()),
                None => Tab::Plugins,
            };
            self.reveal = true;
        }
    }

    /// Drop tabs whose plugin is no longer installed.
    fn prune(&mut self, manager: &PluginManager) {
        self.open.retain(|id| manager.index_of(id).is_some());
        if let Tab::Plugin(id) = &self.active
            && !self.is_open(id)
        {
            self.active = Tab::Plugins;
        }
    }
}

struct App {
    ops: Arc<AndroidOps>,
    manager: Option<PluginManager>,
    manager_ui: PluginManagerUi,
    store: store::Store,
    tabs: Tabs,
    wants_keyboard: bool,
}

impl App {
    fn new(_cc: &CreateContext) -> Self {
        App {
            ops: AndroidOps::new(),
            manager: None,
            manager_ui: PluginManagerUi::default(),
            store: store::Store::new(),
            tabs: Tabs::new(),
            wants_keyboard: false,
        }
    }
}

/// Nav tap-target height.
const NAV_H: f32 = 42.0;

/// Bottom nav: the three fixed pages, then one tab per open plugin. Scrolls horizontally, since
/// the tab count grows with whatever the user has opened. A free function so it can run while
/// `self.manager` is mutably borrowed. Returns the id of a plugin tab the user closed.
fn nav_bar(
    ui: &mut egui::Ui,
    tabs: &mut Tabs,
    manager: &PluginManager,
    host: &Host,
) -> Option<String> {
    let gap = ui.spacing().item_spacing.x;
    // Width minus the two spacing gaps between three buttons, as a per-tab minimum.
    let w = ((ui.available_width() - gap * 2.0) / 3.0).max(48.0);
    let Tabs { active, open, reveal, .. } = tabs;
    let mut close = None;
    theme::scroll_horizontal().show(ui, |ui| {
        ui.horizontal(|ui| {
            for (t, label) in
                [(Tab::Plugins, "Plugins"), (Tab::Manage, "Manage"), (Tab::Store, "Store")]
            {
                let selected = *active == t;
                let button = theme::selectable(selected, nav_text(label, selected));
                let resp = ui.add(button.min_size(egui::vec2(w, NAV_H)));
                if resp.clicked() && !selected {
                    *active = t;
                    host.haptic(egui_android::Haptic::Light);
                }
                if selected && *reveal {
                    resp.scroll_to_me(Some(egui::Align::Center));
                }
            }
            for id in open.iter() {
                let selected = matches!(&*active, Tab::Plugin(a) if a == id);
                let name = manager
                    .index_of(id)
                    .map(|i| manager.plugins[i].manifest.name.as_str())
                    .unwrap_or(id.as_str());
                let button = theme::selectable(selected, nav_text(&elide(name), selected));
                let resp = ui.add(button.min_size(egui::vec2(96.0, NAV_H)));
                if resp.clicked() && !selected {
                    *active = Tab::Plugin(id.clone());
                    host.haptic(egui_android::Haptic::Light);
                }
                if selected && *reveal {
                    resp.scroll_to_me(Some(egui::Align::Center));
                }
                // Tight spacing between a tab and its ✖.
                ui.spacing_mut().item_spacing.x = 1.0;
                let x = egui::RichText::new("✖").size(12.0).color(theme::INK_DIM);
                if ui.add_sized([30.0, NAV_H], egui::Button::new(x).frame(false)).clicked() {
                    close = Some(id.clone());
                    host.haptic(egui_android::Haptic::Light);
                }
                ui.spacing_mut().item_spacing.x = gap;
            }
        });
    });
    *reveal = false;
    close
}

fn nav_text(label: &str, selected: bool) -> egui::RichText {
    egui::RichText::new(label)
        .size(15.0)
        .color(if selected { theme::AQUA_BRIGHT } else { theme::INK })
}

/// Shorten a tab label so one long plugin name cannot push the rest of the bar off-screen.
fn elide(name: &str) -> String {
    const MAX: usize = 14;
    if name.chars().count() <= MAX {
        return name.to_owned();
    }
    name.chars().take(MAX - 1).chain(std::iter::once('…')).collect()
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
                        self.tabs.bind_root(manager.root());
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

        // Reconcile only once no load is in flight; a restart would otherwise prune every
        // persisted tab before its plugin has landed.
        if manager.pending_loads().is_empty() {
            self.tabs.prune(manager);
        }

        // The nav collapses while typing: focus leads the keyboard's slide-in and the inset trails
        // its slide-out, so the union collapses once and restores once with no gap in between.
        // Never over a plugin: the tab strip and its close buttons are the only way out, and a
        // plugin that re-requests focus every frame would keep the bar collapsed for good.
        let on_plugin = matches!(self.tabs.active, Tab::Plugin(_));
        let mut nav_open =
            on_plugin || !(host.keyboard_height() > 1.0 || ui.ctx().text_edit_focused());
        let mut closed = None;
        egui::Panel::bottom("plugins-nav")
            .show_separator_line(false)
            .frame(theme::bar())
            // egui 0.36's collapsed-panel resize strip would eat taps at the bottom screen edge.
            .drag_to_open(false)
            .show_collapsible(ui, &mut nav_open, |ui| {
                closed = nav_bar(ui, &mut self.tabs, manager, host);
            });
        if let Some(id) = closed {
            self.tabs.close_plugin(&id);
        }

        // Desired keyboard state this frame; only a plugin viewport can ask for it.
        let mut wants_keyboard = false;
        // Tab moves are deferred: the match below borrows `self.tabs` to read the active tab.
        let mut open_id: Option<String> = None;
        let mut goto: Option<Tab> = None;
        // eframe's `App::ui` hands over a bare `Ui` with no background, so the page needs a panel
        // of its own or `panel_fill` never paints and `ambience` shows at full strength.
        let page = theme::page(ui.style());
        egui::CentralPanel::default().frame(page).show(ui, |ui| match &self.tabs.active {
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
            Tab::Plugins => {
                if manager.plugins.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.35);
                        ui.label(
                            egui::RichText::new("No plugins installed").color(theme::INK_DIM),
                        );
                        if ui.button("Open the Store").clicked() {
                            goto = Some(Tab::Store);
                        }
                    });
                } else {
                    open_id = plugin_launcher(ui, manager, &self.tabs.open);
                }
            }
            Tab::Plugin(id) => {
                let Some(index) = manager.index_of(id) else {
                    // Reloading or still loading; `prune` drops the tab if it never comes back.
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.4);
                        ui.spinner();
                    });
                    return;
                };
                // No keyboard math here: the runtime already shrank `screen_rect` by the
                // keyboard's occlusion, so `available_size` is the area above the keys.
                let response = manager.show_plugin(ui, index);
                wants_keyboard = response.wants_keyboard;

                // Cross-plugin hand-off: Devices asks the terminal to SSH into a host.
                for ev in &response.events {
                    if ev.topic == egui_android::plugins::abi::net::EVENT_SSH_OPEN
                        && manager.send_event_to("com.example.terminal", &ev.topic, &ev.payload)
                    {
                        open_id = Some("com.example.terminal".to_owned());
                    }
                }
            }
        });
        if let Some(id) = open_id {
            self.tabs.open_plugin(&id);
        }
        if let Some(t) = goto {
            self.tabs.active = t;
        }
        // Reconcile on every path so leaving a plugin lowers the keyboard.
        if wants_keyboard != self.wants_keyboard {
            self.wants_keyboard = wants_keyboard;
            host.request_keyboard(self.wants_keyboard);
        }

        self.tabs.save_if_changed();

        // Apply queued plugin ops (haptics, notifications, …) via the Android host bridge.
        self.ops.drain_into(host);
        ui.ctx().request_repaint();
    }
}

/// Launcher page: a status dot and a full-width row per installed plugin. Tapping one returns its
/// id to open as a tab. Rows already open are drawn selected.
fn plugin_launcher(ui: &mut egui::Ui, manager: &PluginManager, open: &[String]) -> Option<String> {
    let mut pick = None;
    theme::scroll_vertical().show(ui, |ui| {
        for plugin in &manager.plugins {
            let id = &plugin.manifest.id;
            ui.horizontal(|ui| {
                theme::status_dot(ui, status_color(&plugin.status, plugin.enabled));
                // A growing spacer pins the name left and the version right.
                let text = (
                    egui::Atom::from(
                        egui::RichText::new(&plugin.manifest.name).size(15.0).color(theme::INK),
                    ),
                    egui::Atom::grow(),
                    egui::Atom::from(
                        egui::RichText::new(format!("v{}", plugin.manifest.version))
                            .small()
                            .color(theme::INK_DIM),
                    ),
                );
                let selected = open.iter().any(|o| o == id);
                let w = ui.available_width();
                if theme::selectable_label(ui, selected, [w, 44.0], text).clicked() {
                    pick = Some(id.clone());
                }
            });
        }
    });
    pick
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
