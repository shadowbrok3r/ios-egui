//! Android UI shell: Live map, Record controls, Settings.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_mobile::egui;
use egui_mobile::{app, CreateContext, EguiApp, Haptic, Host};

use crate::config::{AppConfig, NodePos};
use crate::net::{self, HttpCall, HttpOutcome, WsEvent};
use crate::pulsecard::{self, PulseTrace};
use crate::proto::{A121Snapshot, MmwaveSnapshot, SensingSnapshot, WsMsg};
use crate::roommap::{self, MapInputs, Trails};
use crate::{frost, theme};

/// Data older than this is stale — shown as such, never as live.
const STALE_AFTER: Duration = Duration::from_secs(3);

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Live,
    Record,
    Settings,
}

struct SurveyorApp {
    cfg: AppConfig,
    cfg_path: Option<PathBuf>,
    tab: Tab,

    ws_rx: Option<Receiver<WsEvent>>,
    ws_tx: Option<SyncSender<WsEvent>>,
    ws_stop: Option<Arc<AtomicBool>>,
    connected: bool,
    link_note: String,

    latest_sensing: Option<(SensingSnapshot, Instant)>,
    latest_mmwave: Option<(MmwaveSnapshot, Instant)>,
    latest_a121: Option<(A121Snapshot, Instant)>,
    a121_trace: PulseTrace,
    trails: Trails,

    http_rx: Receiver<HttpOutcome>,
    http_tx: Sender<HttpOutcome>,
    last_http: Option<HttpOutcome>,
    recording: bool,
    session_name: String,

    // Settings scratch (applied on Save)
    edit_server: String,
    edit_room_w: String,
    edit_room_h: String,
    edit_nodes: String,
    edit_mount_x: String,
    edit_mount_y: String,
    edit_yaw: String,
    edit_flip: bool,
}

impl SurveyorApp {
    fn new(_cc: &CreateContext) -> Self {
        let (http_tx, http_rx) = std::sync::mpsc::channel();
        let cfg = AppConfig::default();
        let mut app = SurveyorApp {
            edit_server: cfg.server.clone(),
            edit_room_w: cfg.room_w.to_string(),
            edit_room_h: cfg.room_h.to_string(),
            edit_nodes: String::new(),
            edit_mount_x: cfg.mount.x_m.to_string(),
            edit_mount_y: cfg.mount.y_m.to_string(),
            edit_yaw: cfg.mount.yaw_deg.to_string(),
            edit_flip: cfg.mount.flip_x,
            cfg,
            cfg_path: None,
            tab: Tab::Live,
            ws_rx: None,
            ws_tx: None,
            ws_stop: None,
            connected: false,
            link_note: String::new(),
            latest_sensing: None,
            latest_mmwave: None,
            latest_a121: None,
            a121_trace: PulseTrace::default(),
            trails: Trails::default(),
            http_rx,
            http_tx,
            last_http: None,
            recording: false,
            session_name: "walk1".to_owned(),
        };
        app.session_name = "walk1".to_owned();
        app
    }

    fn init_config(&mut self, host: &Host) {
        if self.cfg_path.is_some() {
            return;
        }
        if let Some(dir) = host.documents_dir() {
            let path = PathBuf::from(dir).join("surveyor.json");
            self.cfg = AppConfig::load(&path);
            self.cfg_path = Some(path);
            self.refresh_edits();
        }
    }

    fn refresh_edits(&mut self) {
        self.edit_server = self.cfg.server.clone();
        self.edit_room_w = self.cfg.room_w.to_string();
        self.edit_room_h = self.cfg.room_h.to_string();
        self.edit_nodes = self
            .cfg
            .nodes
            .iter()
            .map(|n| format!("{}:{},{}", n.id, n.x, n.y))
            .collect::<Vec<_>>()
            .join(";");
        self.edit_mount_x = self.cfg.mount.x_m.to_string();
        self.edit_mount_y = self.cfg.mount.y_m.to_string();
        self.edit_yaw = self.cfg.mount.yaw_deg.to_string();
        self.edit_flip = self.cfg.mount.flip_x;
    }

    fn connect(&mut self, ctx: &egui::Context) {
        self.disconnect();
        // Old trails/snapshots belong to the previous link or room geometry.
        self.trails.clear();
        self.latest_sensing = None;
        self.latest_mmwave = None;
        self.latest_a121 = None;
        self.a121_trace.clear();
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let stop = net::spawn_ws(self.cfg.server.clone(), tx.clone(), ctx.clone());
        self.ws_rx = Some(rx);
        self.ws_tx = Some(tx);
        self.ws_stop = Some(stop);
        self.link_note = "connecting".to_owned();
    }

    fn disconnect(&mut self) {
        if let Some(stop) = self.ws_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.ws_rx = None;
        self.ws_tx = None;
        self.connected = false;
    }

    fn drain_channels(&mut self) {
        if let Some(rx) = &self.ws_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    WsEvent::Connected => {
                        self.connected = true;
                        self.link_note = "connected".to_owned();
                    }
                    WsEvent::Disconnected(why) => {
                        self.connected = false;
                        self.link_note = why;
                    }
                    WsEvent::Msg(WsMsg::Sensing(s)) => {
                        if let Some(loc) = s.localization {
                            self.trails.push_loc(loc.x, loc.y);
                        }
                        self.latest_sensing = Some((s, Instant::now()));
                        self.connected = true;
                    }
                    WsEvent::Msg(WsMsg::Mmwave(m)) => {
                        for t in &m.targets {
                            let (x, y) = self.cfg.mount.to_room(t.x_m, t.y_m);
                            self.trails.push_radar(x, y);
                        }
                        self.latest_mmwave = Some((m, Instant::now()));
                        self.connected = true;
                    }
                    WsEvent::Msg(WsMsg::A121(a)) => {
                        self.a121_trace.push(&a);
                        self.latest_a121 = Some((a, Instant::now()));
                        self.connected = true;
                    }
                    WsEvent::Msg(WsMsg::Other(_)) => {}
                }
            }
        }
        while let Ok(outcome) = self.http_rx.try_recv() {
            match (outcome.label.as_str(), &outcome.result) {
                // Only a real {"success": true} means the server started; an
                // "already recording" rejection also means it IS recording.
                ("record start", Ok(v)) => {
                    let started = v.get("success").and_then(serde_json::Value::as_bool) == Some(true);
                    let already = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .is_some_and(|e| e.contains("already recording"));
                    self.recording = started || already;
                }
                ("record start", Err(_)) => {}
                // A failed stop leaves the server recording — don't lie about it.
                ("record stop", Ok(v)) => {
                    if v.get("success").and_then(serde_json::Value::as_bool) == Some(true) {
                        self.recording = false;
                    }
                }
                ("record stop", Err(_)) => {}
                ("status", Ok(v)) => {
                    if let Some(b) = v.get("recording").and_then(serde_json::Value::as_bool) {
                        self.recording = b;
                    }
                }
                _ => {}
            }
            self.last_http = Some(outcome);
        }
    }

    fn sensing_fresh(&self) -> Option<&SensingSnapshot> {
        self.latest_sensing
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STALE_AFTER)
            .map(|(s, _)| s)
    }

    fn mmwave_fresh(&self) -> Option<&MmwaveSnapshot> {
        self.latest_mmwave
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STALE_AFTER)
            .map(|(m, _)| m)
    }

    fn a121_fresh(&self) -> Option<&A121Snapshot> {
        self.latest_a121
            .as_ref()
            .filter(|(_, at)| at.elapsed() < STALE_AFTER)
            .map(|(a, _)| a)
    }

    fn http(&self, ctx: &egui::Context, label: &str, call: HttpCall) {
        net::spawn_http(
            self.cfg.server.clone(),
            label.to_owned(),
            call,
            self.http_tx.clone(),
            ctx.clone(),
        );
    }

    fn live_tab(&mut self, ui: &mut egui::Ui) {
        let live = self.connected && self.sensing_fresh().is_some();
        // Instrument status strip: monospace, phosphor when live, amber when not.
        ui.horizontal_wrapped(|ui| {
            let (state, color) = if live {
                ("LIVE", theme::PINK_BRIGHT)
            } else if self.connected {
                ("NO DATA", theme::VIOLET_BRIGHT)
            } else {
                ("OFFLINE", theme::VIOLET_BRIGHT)
            };
            ui.label(egui::RichText::new(state).monospace().size(13.0).color(color));
            ui.label(
                egui::RichText::new(self.cfg.server.clone())
                    .monospace()
                    .size(12.0)
                    .color(theme::INK),
            );
            if !self.link_note.is_empty() && !live {
                ui.label(
                    egui::RichText::new(self.link_note.clone())
                        .monospace()
                        .size(11.0)
                        .color(theme::VIOLET_BRIGHT),
                );
            }
        });
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal_wrapped(|ui| {
            let mono = |t: String, c: egui::Color32| egui::RichText::new(t).monospace().size(12.0).color(c);
            if let Some(s) = self.sensing_fresh() {
                ui.label(mono(format!("nodes {}", s.node_count), theme::VIOLET));
                if let Some(p) = s.presence {
                    ui.label(mono(format!("presence {p}"), theme::VIOLET));
                }
                if let Some(n) = s.estimated_persons {
                    ui.label(mono(format!("persons {n}"), theme::VIOLET));
                }
                match s.localization {
                    Some(loc) => ui.label(mono(
                        format!("CSI ({:.2},{:.2}) c{:.2}", loc.x, loc.y, loc.confidence),
                        theme::VIOLET,
                    )),
                    None => ui.label(mono("CSI: no estimate".into(), theme::INK)),
                };
            } else if self.latest_sensing.is_some() {
                ui.label(mono("sensing stale - link quiet".into(), theme::VIOLET_BRIGHT));
            } else {
                ui.label(mono("waiting for sensing_update...".into(), theme::INK));
            }
            if let Some(m) = self.mmwave_fresh() {
                for t in &m.targets {
                    let (x, y) = self.cfg.mount.to_room(t.x_m, t.y_m);
                    ui.label(mono(
                        format!("tgt ({x:.2},{y:.2}) {:+.2}m/s", t.speed_mps),
                        theme::AQUA_BRIGHT,
                    ));
                }
            }
        });
        // A121 micro-motion strip lives up here with the status text — putting
        // it below the map starved the bottom nav bar of room. Rendered only
        // once the channel has ever spoken, so setups without an XM125 see no
        // dead chrome.
        if !self.a121_trace.is_empty() {
            pulsecard::paint(ui, &self.a121_trace, self.a121_fresh());
        }
        ui.add_space(4.0);

        let radar_room: Vec<(f64, f64, f64)> = self
            .mmwave_fresh()
            .map(|m| {
                m.targets
                    .iter()
                    .map(|t| {
                        let (x, y) = self.cfg.mount.to_room(t.x_m, t.y_m);
                        (x, y, t.speed_mps)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let inputs = MapInputs {
            loc: self.sensing_fresh().and_then(|s| s.localization),
            radar_room: &radar_room,
            trails: &self.trails,
            radar_pos: (self.cfg.mount.x_m, self.cfg.mount.y_m),
            radar_yaw_deg: self.cfg.mount.yaw_deg,
            live,
        };
        roommap::paint(ui, &self.cfg, &inputs);
    }

    fn record_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        let ctx = ui.ctx().clone();
        theme::card().show(ui, |ui| {
            ui.label("Calibration session (recorded on the sensing server)");
            ui.horizontal(|ui| {
                ui.label("session");
                ui.add(
                    egui::TextEdit::singleline(&mut self.session_name)
                        .desired_width(f32::INFINITY),
                );
            });
            // Two rows of half-width buttons: nothing gets cut off, targets
            // stay big enough for thumbs.
            let half = |ui: &egui::Ui| (ui.available_width() - 8.0) / 2.0;
            let btn = |text: &str| egui::Button::new(text).min_size(egui::vec2(0.0, 40.0));
            ui.horizontal(|ui| {
                let w = half(ui);
                if ui.add_sized([w, 40.0], btn("Start recording")).clicked() {
                    host.haptic(Haptic::Light);
                    self.http(
                        &ctx,
                        "record start",
                        HttpCall::RecordStart { session: self.session_name.clone() },
                    );
                }
                // Always available: the local flag can drift from the server's
                // real state, and an unstoppable session is the worse failure.
                if ui.add_sized([w, 40.0], btn("Stop recording")).clicked() {
                    host.haptic(Haptic::Light);
                    self.http(&ctx, "record stop", HttpCall::RecordStop);
                }
            });
            ui.horizontal(|ui| {
                let w = half(ui);
                if ui.add_sized([w, 40.0], btn("Server status")).clicked() {
                    self.http(&ctx, "status", HttpCall::LocStatus);
                }
                if ui.add_sized([w, 40.0], btn("Empty-room baseline")).clicked() {
                    self.http(&ctx, "baseline", HttpCall::Baseline);
                }
            });
            if self.recording {
                ui.label(
                    egui::RichText::new("REC")
                        .monospace()
                        .size(14.0)
                        .color(theme::AQUA_BRIGHT)
                        .strong(),
                );
            }
        });
        ui.add_space(8.0);
        theme::card().show(ui, |ui| {
            match &self.last_http {
                Some(o) => {
                    ui.label(
                        egui::RichText::new(format!("last: {}", o.label))
                            .monospace()
                            .color(theme::VIOLET),
                    );
                    match &o.result {
                        Ok(v) => {
                            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                                ui.monospace(
                                    serde_json::to_string_pretty(v)
                                        .unwrap_or_else(|_| v.to_string()),
                                );
                            });
                        }
                        Err(e) => {
                            ui.colored_label(theme::VIOLET_BRIGHT, e);
                        }
                    }
                }
                None => {
                    ui.label("no requests yet");
                }
            }
        });
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
            ui.label("server host:port");
            ui.text_edit_singleline(&mut self.edit_server);
            ui.end_row();
            ui.label("room w (m)");
            ui.text_edit_singleline(&mut self.edit_room_w);
            ui.end_row();
            ui.label("room h (m)");
            ui.text_edit_singleline(&mut self.edit_room_h);
            ui.end_row();
            ui.label("nodes id:x,y;...");
            ui.text_edit_singleline(&mut self.edit_nodes);
            ui.end_row();
            ui.label("radar x (m)");
            ui.text_edit_singleline(&mut self.edit_mount_x);
            ui.end_row();
            ui.label("radar y (m)");
            ui.text_edit_singleline(&mut self.edit_mount_y);
            ui.end_row();
            ui.label("radar yaw (deg)");
            ui.text_edit_singleline(&mut self.edit_yaw);
            ui.end_row();
            ui.label("radar flip x");
            ui.checkbox(&mut self.edit_flip, "");
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("Save + reconnect").clicked() {
                self.apply_edits();
                // Re-render the fields from what was actually accepted, so
                // silently dropped entries are visible instead of assumed.
                self.refresh_edits();
                if let Some(path) = &self.cfg_path {
                    self.cfg.save(path);
                }
                self.connect(&ctx);
            }
            if ui.button("Revert").clicked() {
                self.refresh_edits();
            }
        });
        ui.separator();
        ui.label(
            "Values come from your survey: node positions tape-measured, radar \
             pose from solve_mount. The map only shows what the server reports.",
        );
    }

    fn apply_edits(&mut self) {
        self.cfg.server = self.edit_server.trim().to_owned();
        // Non-finite or non-positive dimensions would break the map transform.
        if let Ok(w) = self.edit_room_w.trim().parse::<f64>() {
            if w.is_finite() && w > 0.0 {
                self.cfg.room_w = w;
            }
        }
        if let Ok(h) = self.edit_room_h.trim().parse::<f64>() {
            if h.is_finite() && h > 0.0 {
                self.cfg.room_h = h;
            }
        }
        self.cfg.nodes = self
            .edit_nodes
            .split(';')
            .filter(|p| !p.trim().is_empty())
            .filter_map(|p| {
                let (id, xy) = p.split_once(':')?;
                let (x, y) = xy.split_once(',')?;
                Some(NodePos {
                    id: id.trim().parse().ok()?,
                    x: x.trim().parse().ok()?,
                    y: y.trim().parse().ok()?,
                })
            })
            .collect();
        if let Ok(x) = self.edit_mount_x.trim().parse() {
            self.cfg.mount.x_m = x;
        }
        if let Ok(y) = self.edit_mount_y.trim().parse() {
            self.cfg.mount.y_m = y;
        }
        if let Ok(yaw) = self.edit_yaw.trim().parse() {
            self.cfg.mount.yaw_deg = yaw;
        }
        self.cfg.mount.flip_x = self.edit_flip;
    }
}

impl SurveyorApp {
    fn nav_bar(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 16.0) / 3.0;
            for (tab, label) in
                [(Tab::Live, "Radar"), (Tab::Record, "Record"), (Tab::Settings, "Setup")]
            {
                let selected = self.tab == tab;
                let text = egui::RichText::new(label).size(15.0).color(if selected {
                    theme::AQUA_BRIGHT
                } else {
                    theme::INK
                });
                if ui
                    .add_sized([w, 42.0], egui::Button::selectable(selected, text))
                    .clicked()
                    && !selected
                {
                    self.tab = tab;
                    host.haptic(Haptic::Light);
                }
            }
        });
    }
}

impl EguiApp for SurveyorApp {
    fn theme(&self, ctx: &egui::Context) {
        theme::apply(ctx);
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.init_config(host);
        if self.ws_rx.is_none() {
            let ctx = ui.ctx().clone();
            self.connect(&ctx);
        }
        self.drain_channels();

        // Order is load-bearing: ambience lights the page, then the frost
        // grabs what is already in the framebuffer, then chrome paints on top.
        theme::ambience(ui.ctx());
        frost::frost_chrome(ui);

        // Bottom nav collapses while typing (focus leads the keyboard slide-in
        // and the inset trails slide-out; the union avoids flicker).
        let kb_editing = host.keyboard_height() > 1.0 || ui.ctx().text_edit_focused();
        let mut nav_open = !kb_editing;
        let bar = egui::Panel::bottom("nav")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)))
            // egui 0.36's collapsed-panel resize strip would eat taps at the bottom screen edge.
            .drag_to_open(false)
            .show_collapsible(ui, &mut nav_open, |ui| {
                self.nav_bar(ui, host);
            });
        if let Some(bar) = &bar {
            frost::remember(ui.ctx(), bar.response.rect);
        }

        match self.tab {
            Tab::Live => self.live_tab(ui),
            Tab::Record => self.record_tab(ui, host),
            Tab::Settings => self.settings_tab(ui),
        }
    }
}

app!(SurveyorApp::new, egui_mobile::Backend::Glow);
