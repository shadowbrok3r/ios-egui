//! WireLab board panel: discover WireLab ESP32 boards on the network, connect
//! over the WireLab TCP protocol and drive them live — telemetry, GPIO,
//! analog watches, the on-board RGB LED — straight from the iPad.

pub mod link;
pub mod view;

use std::time::Duration;

use egui_ios_plugin_sdk::abi;
use egui_ios_plugin_sdk::{CreateConfig, HostCallError, HostHandle, PluginApp, egui, plugin};
use serde::{Deserialize, Serialize};
use wirelab_proto::{BEHAVIOR_SLOTS, Behavior, HostMsg, PinMode, UART_CHUNK, WifiState, heapless};

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
    #[serde(default)]
    tab: Tab,
    #[serde(default = "default_pwm_pin")]
    pwm_pin: u8,
    #[serde(default = "default_pwm_freq")]
    pwm_freq: u32,
    #[serde(default = "default_uart_tx")]
    uart_tx: u8,
    #[serde(default = "default_uart_rx")]
    uart_rx: u8,
    #[serde(default = "default_uart_baud")]
    uart_baud: u32,
}

fn default_pwm_pin() -> u8 {
    2
}
fn default_pwm_freq() -> u32 {
    5000
}
fn default_uart_tx() -> u8 {
    4
}
fn default_uart_rx() -> u8 {
    5
}
fn default_uart_baud() -> u32 {
    115_200
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            manual_addr: String::new(),
            rgb: [40, 0, 60],
            watch_pin: 4,
            desktop_addr: String::new(),
            tab: Tab::Board,
            pwm_pin: default_pwm_pin(),
            pwm_freq: default_pwm_freq(),
            uart_tx: default_uart_tx(),
            uart_rx: default_uart_rx(),
            uart_baud: default_uart_baud(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
enum Tab {
    #[default]
    Board,
    Canvas,
    Flow,
    Script,
}

/// Draft parameters for the behavior editor row.
struct BehaviorDraft {
    kind: usize,
    pin: u8,
    period_ms: u16,
    from: u8,
    to: u8,
    invert: bool,
    debounce_ms: u8,
}

impl Default for BehaviorDraft {
    fn default() -> Self {
        BehaviorDraft { kind: 0, pin: 2, period_ms: 500, from: 4, to: 2, invert: false, debounce_ms: 20 }
    }
}

const BEHAVIOR_KINDS: [&str; 4] = ["Blink", "Breathe", "Mirror", "Watch"];

fn behavior_label(b: &Behavior) -> String {
    match b {
        Behavior::Blink { pin, period_ms } => format!("Blink GPIO{pin} every {period_ms}ms"),
        Behavior::Breathe { pin, period_ms } => format!("Breathe GPIO{pin} over {period_ms}ms"),
        Behavior::Mirror { from, to, invert } => {
            format!("Mirror GPIO{from} -> GPIO{to}{}", if *invert { " inverted" } else { "" })
        }
        Behavior::Watch { pin, debounce_ms } => format!("Watch GPIO{pin} ({debounce_ms}ms debounce)"),
    }
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
    /// Pin currently PWM-driven by this panel, with the live duty in permille.
    pwm_on: Option<u8>,
    pwm_duty: u16,
    /// Behavior slots this panel has attached (the protocol has no readback).
    behaviors: std::collections::BTreeMap<u8, Behavior>,
    behavior_draft: BehaviorDraft,
    uart_open: bool,
    uart_input: String,
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
            pwm_on: None,
            pwm_duty: 300,
            behaviors: Default::default(),
            behavior_draft: BehaviorDraft::default(),
            uart_open: false,
            uart_input: String::new(),
        }
    }

    /// Forget all per-connection control state (used on disconnect).
    fn clear_session(&mut self) {
        self.driven.clear();
        self.watching = None;
        self.pwm_on = None;
        self.behaviors.clear();
        self.uart_open = false;
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
            self.tab = self.saved.tab;
            self.last_persisted = abi::encode(&self.saved);
        }

        if self.link.connected() {
            // No reason to hold the beacon socket while a board is attached.
            self.scanner.close(&ops);
            let was_ready = self.link.state == LinkState::Ready;
            self.link.poll(&ops, now);
            if !was_ready && self.link.state == LinkState::Ready {
                host.haptic(3);
            }
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
                    self.saved.tab = tab;
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
                    if let Some((state, ip)) = self.link.wifi {
                        let (text, color) = match state {
                            WifiState::Connected => {
                                (format!("wifi {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]), OK)
                            }
                            WifiState::Connecting => ("wifi connecting…".to_string(), HIGH),
                            WifiState::Failed => ("wifi failed".to_string(), ERR),
                            WifiState::Off => ("wifi off".to_string(), DIM),
                        };
                        ui.label(egui::RichText::new(text).color(color).small());
                    }
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
                    self.clear_session();
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
            self.pwm_section(ui, ops);

            ui.add_space(10.0);
            self.behaviors_section(ui, ops);

            ui.add_space(10.0);
            self.uart_section(ui, ops);

            ui.add_space(10.0);
            self.console(ui);
        });
    }

    fn pwm_section(&mut self, ui: &mut egui::Ui, ops: &dyn Ops) {
        ui.label(egui::RichText::new("PWM").color(ACCENT).small());
        ui.horizontal_wrapped(|ui| {
            ui.label("GPIO");
            ui.add_enabled(
                self.pwm_on.is_none(),
                egui::DragValue::new(&mut self.saved.pwm_pin).range(0..=48),
            );
            ui.label("Hz");
            ui.add_enabled(
                self.pwm_on.is_none(),
                egui::DragValue::new(&mut self.saved.pwm_freq).range(1..=40_000).speed(50),
            );
            match self.pwm_on {
                None => {
                    if ui.button("start").clicked() {
                        let msg = HostMsg::SetPwm {
                            pin: self.saved.pwm_pin,
                            freq_hz: self.saved.pwm_freq,
                            duty_permille: self.pwm_duty,
                        };
                        self.link.send(ops, &msg);
                        self.pwm_on = Some(self.saved.pwm_pin);
                    }
                }
                Some(pin) => {
                    if ui.button("stop").clicked() {
                        let msg = HostMsg::SetPinMode { pin, mode: PinMode::Disabled };
                        self.link.send(ops, &msg);
                        self.pwm_on = None;
                    }
                }
            }
        });
        if let Some(pin) = self.pwm_on {
            let slider = egui::Slider::new(&mut self.pwm_duty, 0..=1000)
                .custom_formatter(|n, _| format!("{:.0}%", n / 10.0))
                .custom_parser(|s| s.trim_end_matches('%').parse::<f64>().ok().map(|p| p * 10.0));
            if ui.add(slider).changed() {
                let msg = HostMsg::SetPwm {
                    pin,
                    freq_hz: self.saved.pwm_freq,
                    duty_permille: self.pwm_duty,
                };
                self.link.send(ops, &msg);
            }
        }
    }

    fn behaviors_section(&mut self, ui: &mut egui::Ui, ops: &dyn Ops) {
        ui.label(
            egui::RichText::new("Behaviors — run on the board, no round-trips")
                .color(ACCENT)
                .small(),
        );
        let entries: Vec<(u8, Behavior)> =
            self.behaviors.iter().map(|(s, b)| (*s, *b)).collect();
        for (slot, b) in entries {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{slot}")).color(DIM).monospace());
                ui.label(behavior_label(&b));
                if ui.small_button("detach").clicked() {
                    self.link.send(ops, &HostMsg::DetachBehavior { slot });
                    self.behaviors.remove(&slot);
                }
            });
        }
        let d = &mut self.behavior_draft;
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("behavior-kind")
                .selected_text(BEHAVIOR_KINDS[d.kind])
                .show_ui(ui, |ui| {
                    for (i, k) in BEHAVIOR_KINDS.iter().enumerate() {
                        ui.selectable_value(&mut d.kind, i, *k);
                    }
                });
            match d.kind {
                0 | 1 => {
                    ui.label("GPIO");
                    ui.add(egui::DragValue::new(&mut d.pin).range(0..=48));
                    ui.label("ms");
                    ui.add(egui::DragValue::new(&mut d.period_ms).range(20..=10_000).speed(10));
                }
                2 => {
                    ui.label("from");
                    ui.add(egui::DragValue::new(&mut d.from).range(0..=48));
                    ui.label("to");
                    ui.add(egui::DragValue::new(&mut d.to).range(0..=48));
                    ui.checkbox(&mut d.invert, "invert");
                }
                _ => {
                    ui.label("GPIO");
                    ui.add(egui::DragValue::new(&mut d.pin).range(0..=48));
                    ui.label("debounce ms");
                    ui.add(egui::DragValue::new(&mut d.debounce_ms).range(1..=255));
                }
            }
            let free = (0..BEHAVIOR_SLOTS as u8).find(|s| !self.behaviors.contains_key(s));
            let label = if free.is_some() { "attach" } else { "slots full" };
            if ui.add_enabled(free.is_some(), egui::Button::new(label)).clicked()
                && let Some(slot) = free
            {
                let behavior = match d.kind {
                    0 => Behavior::Blink { pin: d.pin, period_ms: d.period_ms },
                    1 => Behavior::Breathe { pin: d.pin, period_ms: d.period_ms },
                    2 => Behavior::Mirror { from: d.from, to: d.to, invert: d.invert },
                    _ => Behavior::Watch { pin: d.pin, debounce_ms: d.debounce_ms },
                };
                self.link.send(ops, &HostMsg::AttachBehavior { slot, behavior });
                self.behaviors.insert(slot, behavior);
            }
        });
    }

    fn uart_section(&mut self, ui: &mut egui::Ui, ops: &dyn Ops) {
        ui.label(egui::RichText::new("UART").color(ACCENT).small());
        ui.horizontal_wrapped(|ui| {
            ui.label("tx");
            ui.add_enabled(
                !self.uart_open,
                egui::DragValue::new(&mut self.saved.uart_tx).range(0..=48),
            );
            ui.label("rx");
            ui.add_enabled(
                !self.uart_open,
                egui::DragValue::new(&mut self.saved.uart_rx).range(0..=48),
            );
            ui.label("baud");
            ui.add_enabled(
                !self.uart_open,
                egui::DragValue::new(&mut self.saved.uart_baud).range(300..=1_000_000).speed(100),
            );
            if !self.uart_open {
                if ui.button("open").clicked() {
                    let msg = HostMsg::UartConfig {
                        tx: self.saved.uart_tx,
                        rx: self.saved.uart_rx,
                        baud: self.saved.uart_baud,
                    };
                    self.link.send(ops, &msg);
                    self.uart_open = true;
                }
            } else if ui.button("close").clicked() {
                let msg = HostMsg::UartConfig { tx: 0, rx: 0, baud: 0 };
                self.link.send(ops, &msg);
                self.uart_open = false;
            }
        });
        if !self.uart_open {
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("uart")
            .max_height(120.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.link.uart_rx {
                    ui.label(egui::RichText::new(line).monospace().small());
                }
                if !self.link.uart_tail().is_empty() {
                    ui.label(
                        egui::RichText::new(self.link.uart_tail()).monospace().small().color(DIM),
                    );
                }
            });
        ui.horizontal(|ui| {
            let edit = egui::TextEdit::singleline(&mut self.uart_input)
                .hint_text("send a line…")
                .desired_width(ui.available_width() - 64.0);
            let submitted = ui.add(edit).lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("send").clicked() || submitted) && !self.uart_input.is_empty() {
                let mut line = std::mem::take(&mut self.uart_input);
                line.push('\n');
                for chunk in line.as_bytes().chunks(UART_CHUNK) {
                    if let Ok(data) = heapless::Vec::<u8, UART_CHUNK>::from_slice(chunk) {
                        self.link.send(ops, &HostMsg::UartWrite { data });
                    }
                }
            }
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
