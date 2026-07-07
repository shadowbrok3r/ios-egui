//! WireLab board panel: discover WireLab ESP32 boards on the network, connect
//! over the WireLab TCP protocol and drive them live — telemetry, GPIO,
//! analog watches, the on-board RGB LED — straight from the iPad.

pub mod link;
pub mod view;

use std::time::Duration;

use egui_ios_plugin_sdk::abi;
use egui_ios_plugin_sdk::{CreateConfig, HostCallError, HostHandle, PluginApp, egui, plugin};
use serde::{Deserialize, Serialize};
use wirelab_proto::HostMsg;

use link::{BoardLink, LinkState, Ops, Scanner};

const OK: egui::Color32 = egui::Color32::from_rgb(166, 227, 161);
const ERR: egui::Color32 = egui::Color32::from_rgb(243, 139, 168);
const DIM: egui::Color32 = egui::Color32::from_rgb(127, 132, 156);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(203, 166, 247);
const HIGH: egui::Color32 = egui::Color32::from_rgb(249, 226, 175);

/// `HostHandle` as the link's op surface.
struct HostOps<'a>(&'a HostHandle);

impl Ops for HostOps<'_> {
    fn call(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
        self.0.call(op, payload).map_err(|e| match e {
            HostCallError::Denied => "denied — add \"net\" to manifest permissions".into(),
            other => other.to_string(),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Persisted {
    manual_addr: String,
    rgb: [u8; 3],
    watch_pin: u8,
    #[serde(default)]
    desktop_addr: String,
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            manual_addr: String::new(),
            rgb: [40, 0, 60],
            watch_pin: 4,
            desktop_addr: String::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Tab {
    Board,
    Canvas,
    Flow,
    Script,
}

/// Disk key for settings that must survive a full app restart (not just a
/// hot reload, which `save_state`/`restore_state` already covers).
const STATE_KEY_STR: &str = "settings";

struct App {
    saved: Persisted,
    scanner: Scanner,
    link: BoardLink,
    /// GPIOs this panel has claimed as outputs, with the commanded level.
    driven: std::collections::BTreeMap<u8, bool>,
    watching: Option<u8>,
    tab: Tab,
    project: crate::view::ProjectView,
    /// Loaded from disk once; last bytes written, to persist only on change.
    loaded_from_disk: bool,
    last_persisted: Vec<u8>,
}

impl App {
    fn new(_: &CreateConfig) -> Self {
        App {
            saved: Persisted::default(),
            scanner: Scanner::default(),
            link: BoardLink::default(),
            driven: Default::default(),
            watching: None,
            tab: Tab::Board,
            project: Default::default(),
            loaded_from_disk: false,
            last_persisted: Vec::new(),
        }
    }
}

impl PluginApp for App {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        let ops = HostOps(host);
        let now = ui.input(|i| i.time);

        // Load persisted settings once (survives full app restarts, unlike
        // save_state which only carries across hot reloads).
        if !self.loaded_from_disk {
            self.loaded_from_disk = true;
            if let Ok(bytes) = host.call("state.get", STATE_KEY_STR.as_bytes())
                && let Ok(Some(data)) = abi::decode::<Option<Vec<u8>>>(&bytes)
                && let Ok(s) = abi::decode::<Persisted>(&data)
            {
                self.saved = s;
            }
            self.last_persisted = abi::encode(&self.saved);
        }

        if self.link.connected() {
            // No reason to hold the beacon socket while a board is attached.
            self.scanner.close(&ops);
            self.link.poll(&ops, now);
        } else if self.tab == Tab::Board {
            self.scanner.poll(&ops, now);
        }
        self.project.desktop_addr = std::mem::take(&mut self.saved.desktop_addr);
        self.project.poll(&ops, now);
        // Sockets are host-side; keep frames coming while anything is live.
        ui.ctx().request_repaint_after(Duration::from_millis(50));

        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::Board, "⚡ Board"),
                (Tab::Canvas, "🗺 Canvas"),
                (Tab::Flow, "⛓ Flow"),
                (Tab::Script, "📜 Script"),
            ] {
                if ui.selectable_label(self.tab == tab, label).clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.separator();

        match self.tab {
            Tab::Board => {
                if self.link.connected() {
                    self.board_panel(ui, &ops);
                } else {
                    self.connect_panel(ui, &ops);
                }
            }
            Tab::Canvas => {
                if self.project.header(ui, &ops, now) {
                    self.project.show_canvas(ui, &ops, now);
                }
            }
            Tab::Flow => {
                if self.project.header(ui, &ops, now) {
                    self.project.show_flow(ui, &ops, now);
                }
            }
            Tab::Script => {
                if self.project.header(ui, &ops, now) {
                    self.project.show_script(ui, &ops, now);
                }
            }
        }
        self.saved.desktop_addr = std::mem::take(&mut self.project.desktop_addr);

        // Persist settings to disk whenever they change (so the desktop
        // address survives a full app restart).
        let cur = abi::encode(&self.saved);
        if cur != self.last_persisted {
            let _ = host.call("state.set", &abi::encode(&(STATE_KEY_STR.to_string(), cur.clone())));
            self.last_persisted = cur;
        }
    }

    fn save_state(&self) -> Vec<u8> {
        abi::encode(&self.saved)
    }

    fn restore_state(&mut self, bytes: &[u8]) {
        if let Ok(saved) = abi::decode::<Persisted>(bytes) {
            self.saved = saved;
        }
    }
}

impl App {
    fn connect_panel(&mut self, ui: &mut egui::Ui, ops: &dyn Ops) {
        ui.heading("WireLab boards");
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "boards on your network announce themselves once their Wi-Fi is set up \
                 (WireLab desktop -> Wi-Fi menu -> Join)",
            )
            .color(DIM)
            .small(),
        );
        ui.add_space(8.0);

        let boards: Vec<_> = self.scanner.boards().cloned().collect();
        if boards.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("listening for beacons on UDP 4519…").color(DIM));
            });
        }
        for b in &boards {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("•").color(OK));
                ui.label(egui::RichText::new(&b.chip).strong());
                ui.label(egui::RichText::new(&b.addr).color(DIM).monospace());
                if ui.button("Connect").clicked() {
                    self.link.connect(ops, &b.addr);
                }
            });
        }
        if let Some(err) = &self.scanner.error {
            ui.label(egui::RichText::new(format!("discovery: {err}")).color(ERR).small());
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(egui::RichText::new("or connect by address").color(DIM).small());
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.saved.manual_addr)
                    .hint_text("192.168.1.x:4518")
                    .desired_width(180.0),
            );
            let ok = !self.saved.manual_addr.trim().is_empty();
            if ui.add_enabled(ok, egui::Button::new("Connect")).clicked() {
                let addr = self.saved.manual_addr.trim().to_string();
                self.link.connect(ops, &addr);
            }
        });

        if let LinkState::Failed(e) = &self.link.state {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(e).color(ERR));
        }
        if !self.link.log.is_empty() {
            ui.add_space(8.0);
            self.console(ui);
        }
    }

    fn board_panel(&mut self, ui: &mut egui::Ui, ops: &dyn Ops) {
        ui.horizontal(|ui| {
            match (&self.link.state, self.link.info) {
                (LinkState::Ready, Some(info)) => {
                    ui.label(egui::RichText::new("• live").color(OK));
                    ui.label(egui::RichText::new(info.chip.name()).strong());
                    ui.label(
                        egui::RichText::new(format!(
                            "fw {}.{}",
                            info.fw_version >> 8,
                            info.fw_version & 0xff
                        ))
                        .color(DIM)
                        .small(),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "up {:.0}s",
                            f64::from(self.link.uptime_ms) / 1000.0
                        ))
                        .color(DIM)
                        .small(),
                    );
                }
                _ => {
                    ui.spinner();
                    ui.label(format!("connecting to {}…", self.link.addr));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Disconnect").clicked() {
                    let reset = HostMsg::Reset;
                    self.link.send(ops, &reset);
                    self.link.disconnect(ops);
                    self.driven.clear();
                    self.watching = None;
                }
            });
        });
        if self.link.state != LinkState::Ready {
            return;
        }
        let Some(info) = self.link.info else { return };

        ui.add_space(8.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            // ── RGB LED ────────────────────────────────────────────────
            ui.label(egui::RichText::new("RGB LED").color(ACCENT).small());
            ui.horizontal(|ui| {
                if ui.color_edit_button_srgb(&mut self.saved.rgb).changed() {
                    let [r, g, b] = self.saved.rgb;
                    let msg = HostMsg::SetRgb { pin: self.link.rgb_gpio(), r, g, b };
                    self.link.send(ops, &msg);
                }
                if ui.small_button("off").clicked() {
                    let msg = HostMsg::SetRgb { pin: self.link.rgb_gpio(), r: 0, g: 0, b: 0 };
                    self.link.send(ops, &msg);
                }
            });

            ui.add_space(10.0);

            // ── GPIO grid ─────────────────────────────────────────────
            ui.label(egui::RichText::new("GPIO — tap to drive high/low").color(ACCENT).small());
            let gpios: Vec<u8> =
                (0..64).filter(|n| info.gpio_mask & (1u64 << n) != 0).collect();
            egui::Grid::new("pins").num_columns(8).spacing([6.0, 6.0]).show(ui, |ui| {
                for (i, gpio) in gpios.iter().copied().enumerate() {
                    let level = self.link.levels & (1u64 << gpio) != 0;
                    let input_only = info.input_only_mask & (1u64 << gpio) != 0;
                    let driven = self.driven.contains_key(&gpio);
                    let fill = match (driven, level) {
                        (true, true) => HIGH,
                        (true, false) => egui::Color32::from_rgb(90, 70, 110),
                        (false, true) => OK,
                        (false, false) => egui::Color32::from_gray(45),
                    };
                    let text = egui::RichText::new(format!("{gpio}"))
                        .color(if level || driven {
                            egui::Color32::BLACK
                        } else {
                            egui::Color32::from_gray(160)
                        })
                        .strong();
                    if ui
                        .add_enabled(
                            !input_only,
                            egui::Button::new(text).fill(fill).min_size([34.0, 30.0].into()),
                        )
                        .clicked()
                    {
                        let next = !self.driven.get(&gpio).copied().unwrap_or(level);
                        self.driven.insert(gpio, next);
                        let msg = HostMsg::WriteDigital { pin: gpio, high: next };
                        self.link.send(ops, &msg);
                    }
                    if (i + 1) % 8 == 0 {
                        ui.end_row();
                    }
                }
            });
            ui.label(
                egui::RichText::new("green = reads high · yellow = driven high by you")
                    .color(DIM)
                    .small(),
            );

            ui.add_space(10.0);

            // ── analog watch ──────────────────────────────────────────
            ui.label(egui::RichText::new("Analog watch").color(ACCENT).small());
            ui.horizontal(|ui| {
                ui.label("GPIO");
                ui.add(egui::DragValue::new(&mut self.saved.watch_pin).range(0..=48));
                let label = if self.watching == Some(self.saved.watch_pin) {
                    "watching"
                } else {
                    "watch"
                };
                if ui
                    .add_enabled(
                        self.watching != Some(self.saved.watch_pin),
                        egui::Button::new(label),
                    )
                    .clicked()
                {
                    if let Some(prev) = self.watching {
                        let stop = HostMsg::WatchAnalog { pin: prev, interval_ms: 0 };
                        self.link.send(ops, &stop);
                    }
                    let pin = self.saved.watch_pin;
                    let msg = HostMsg::WatchAnalog { pin, interval_ms: 100 };
                    self.link.send(ops, &msg);
                    self.watching = Some(pin);
                }
                if let Some(pin) = self.watching
                    && let Some(mv) = self.link.analog.get(&pin)
                {
                    ui.label(egui::RichText::new(format!("{mv} mV")).color(HIGH).monospace());
                }
            });
            if let Some(hist) =
                self.watching.and_then(|pin| self.link.analog_hist.get(&pin))
                && hist.len() >= 2
            {
                let (rect, _) = ui.allocate_exact_size(
                    [ui.available_width().min(360.0), 60.0].into(),
                    egui::Sense::hover(),
                );
                let p = ui.painter_at(rect);
                p.rect_filled(rect, 4.0, egui::Color32::from_gray(25));
                let pts: Vec<egui::Pos2> = hist
                    .iter()
                    .enumerate()
                    .map(|(i, &mv)| {
                        let x = rect.left()
                            + rect.width() * i as f32 / (hist.len() - 1) as f32;
                        let y = rect.bottom() - rect.height() * f32::from(mv) / 3100.0;
                        egui::pos2(x, y)
                    })
                    .collect();
                p.add(egui::Shape::line(pts, egui::Stroke::new(1.5, ACCENT)));
            }

            ui.add_space(10.0);
            self.console(ui);
        });
    }

    fn console(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Console").color(ACCENT).small());
        egui::ScrollArea::vertical()
            .id_salt("console")
            .max_height(120.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.link.log {
                    ui.label(egui::RichText::new(line).monospace().small().color(DIM));
                }
            });
    }
}

plugin!(App::new);
