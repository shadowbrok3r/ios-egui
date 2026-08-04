//! Android UI shell: Live map, Record controls, Settings.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use egui_mobile::egui;
use egui_mobile::{app, CreateContext, EguiApp, Haptic, Host};

use crate::config::{AppConfig, NodePos};
use crate::net::{self, HttpCall, HttpOutcome};
use crate::proto::{MmwaveSnapshot, SensingSnapshot, WsMsg};
use crate::roommap::{self, MapInputs, Trails};

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

    ws_rx: Option<Receiver<WsMsg>>,
    ws_tx: Option<Sender<WsMsg>>,
    ws_stop: Option<Arc<AtomicBool>>,
    connected: bool,

    latest_sensing: Option<SensingSnapshot>,
    latest_mmwave: Option<MmwaveSnapshot>,
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
            latest_sensing: None,
            latest_mmwave: None,
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
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = net::spawn_ws(self.cfg.server.clone(), tx.clone(), ctx.clone());
        self.ws_rx = Some(rx);
        self.ws_tx = Some(tx);
        self.ws_stop = Some(stop);
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
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    WsMsg::Sensing(s) => {
                        if let Some(loc) = s.localization {
                            self.trails.push_loc(loc.x, loc.y);
                        }
                        self.latest_sensing = Some(s);
                        self.connected = true;
                    }
                    WsMsg::Mmwave(m) => {
                        for t in &m.targets {
                            let (x, y) = self.cfg.mount.to_room(t.x_m, t.y_m);
                            self.trails.push_radar(x, y);
                        }
                        self.latest_mmwave = Some(m);
                        self.connected = true;
                    }
                    WsMsg::Other(tag) => {
                        if tag == "connected" {
                            self.connected = true;
                        } else if tag == "connect failed" {
                            self.connected = false;
                        }
                    }
                }
            }
        }
        while let Ok(outcome) = self.http_rx.try_recv() {
            if outcome.label == "record start" {
                self.recording = outcome
                    .result
                    .as_ref()
                    .map(|v| v.get("success").is_some())
                    .unwrap_or(false);
            } else if outcome.label == "record stop" {
                self.recording = false;
            }
            self.last_http = Some(outcome);
        }
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
        ui.horizontal(|ui| {
            let dot = if self.connected { "connected" } else { "disconnected" };
            ui.label(format!("{} ({})", dot, self.cfg.server));
        });
        if let Some(s) = &self.latest_sensing {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("source {}", s.source));
                ui.label(format!("nodes {}", s.node_count));
                if let Some(p) = s.presence {
                    ui.label(format!("presence {p}"));
                }
                if let Some(n) = s.estimated_persons {
                    ui.label(format!("persons {n}"));
                }
                match s.localization {
                    Some(loc) => ui.label(format!(
                        "CSI ({:.2}, {:.2}) conf {:.2}",
                        loc.x, loc.y, loc.confidence
                    )),
                    None => ui.label("CSI: no estimate"),
                };
            });
        } else {
            ui.label("waiting for sensing_update...");
        }
        if let Some(m) = &self.latest_mmwave {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("radar n{}", m.node_id));
                for t in &m.targets {
                    let (x, y) = self.cfg.mount.to_room(t.x_m, t.y_m);
                    ui.label(format!("({x:.2}, {y:.2}) {:+.2} m/s", t.speed_mps));
                }
                if m.targets.is_empty() {
                    ui.label("no targets");
                }
            });
        }
        ui.separator();

        let radar_room: Vec<(f64, f64, f64)> = self
            .latest_mmwave
            .as_ref()
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
            loc: self.latest_sensing.as_ref().and_then(|s| s.localization),
            radar_room: &radar_room,
            trails: &self.trails,
        };
        roommap::paint(ui, &self.cfg, &inputs);
    }

    fn record_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        let ctx = ui.ctx().clone();
        ui.label("Calibration session recording (on the sensing server)");
        ui.horizontal(|ui| {
            ui.label("session");
            ui.text_edit_singleline(&mut self.session_name);
        });
        ui.horizontal(|ui| {
            if self.recording {
                if ui.button("Stop recording").clicked() {
                    host.haptic(Haptic::Light);
                    self.http(&ctx, "record stop", HttpCall::RecordStop);
                }
            } else if ui.button("Start recording").clicked() {
                host.haptic(Haptic::Light);
                self.http(
                    &ctx,
                    "record start",
                    HttpCall::RecordStart { session: self.session_name.clone() },
                );
            }
            if ui.button("Server status").clicked() {
                self.http(&ctx, "status", HttpCall::LocStatus);
            }
            if ui.button("Empty-room baseline").clicked() {
                self.http(&ctx, "baseline", HttpCall::Baseline);
            }
        });
        if self.recording {
            ui.colored_label(egui::Color32::from_rgb(240, 120, 100), "RECORDING");
        }
        ui.separator();
        match &self.last_http {
            Some(o) => {
                ui.label(format!("last: {}", o.label));
                match &o.result {
                    Ok(v) => {
                        ui.monospace(
                            serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
                        );
                    }
                    Err(e) => {
                        ui.colored_label(egui::Color32::from_rgb(240, 120, 100), e);
                    }
                }
            }
            None => {
                ui.label("no requests yet");
            }
        }
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
        if let Ok(w) = self.edit_room_w.trim().parse() {
            self.cfg.room_w = w;
        }
        if let Ok(h) = self.edit_room_h.trim().parse() {
            self.cfg.room_h = h;
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

impl EguiApp for SurveyorApp {
    fn theme(&self, ctx: &egui::Context) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_gray(10);
        ctx.set_visuals(visuals);
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.init_config(host);
        if self.ws_rx.is_none() {
            let ctx = ui.ctx().clone();
            self.connect(&ctx);
        }
        self.drain_channels();

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Live, "Live");
            ui.selectable_value(&mut self.tab, Tab::Record, "Record");
            ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
        });
        ui.separator();
        match self.tab {
            Tab::Live => self.live_tab(ui),
            Tab::Record => self.record_tab(ui, host),
            Tab::Settings => self.settings_tab(ui),
        }
    }
}

app!(SurveyorApp::new);
