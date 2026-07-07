//! Remote project view: fetches the live project from the WireLab desktop's
//! LAN endpoint (`GET /project`, port 4520) and renders the circuit canvas
//! and flow graph with the same geometry / node UI the desktop uses.

use std::collections::HashMap;

use egui_ios_plugin_sdk::abi::{self, net};
use egui_ios_plugin_sdk::egui;
use serde::Deserialize;
use wirelab_core::board::BoardProfile;
use wirelab_core::circuit::{CompId, Endpoint, WireId};
use wirelab_core::component::ComponentDef;
use wirelab_core::geometry;
use wirelab_core::project::BoardTab;
use wirelab_flow_ui::{FlowView, FlowViewer, ViewerOptions, build_snarl};

use crate::link::Ops;

/// Wire shape of the desktop's `/project` endpoint.
#[derive(Deserialize)]
pub struct Snapshot {
    pub name: String,
    #[serde(default)]
    pub active: usize,
    pub boards: Vec<BoardTab>,
    #[serde(default)]
    pub profiles: HashMap<String, BoardProfile>,
    #[serde(default)]
    pub defs: HashMap<String, ComponentDef>,
    /// Per-board flow content hash, keyed by board id — the optimistic
    /// concurrency base echoed back when pushing edits.
    #[serde(default)]
    pub flow_bases: HashMap<String, u64>,
}

enum Fetch {
    Idle,
    Pending(u64),
    Failed(String),
}

enum Push {
    Flow(wirelab_core::flow::FlowGraph),
    Moves,
    /// A structural edit (add/remove component or wire); refetch on success.
    Edit,
}

/// How canvas taps are interpreted.
#[derive(Clone, Copy, PartialEq)]
pub enum EditMode {
    /// Drag components to reposition them.
    Move,
    /// Tap two endpoints to run a wire between them.
    Wire,
    /// Tap to drop the palette-selected component.
    Add,
    /// Tap a component or wire to remove it.
    Delete,
}

/// Poll-driven fetcher + the fetched project.
pub struct ProjectView {
    pub desktop_addr: String,
    fetch: Fetch,
    pub snapshot: Option<Snapshot>,
    pub selected_board: usize,
    /// Rebuilt when a new snapshot lands.
    flow_view: FlowView,
    flow_built_for: usize,
    last_fetch_at: f64,
    pub auto_refresh: bool,
    /// The flow as last agreed with the desktop; edits diff against this.
    flow_synced: wirelab_core::flow::FlowGraph,
    flow_dirty_at: Option<f64>,
    push: Option<(u64, Push)>,
    /// A rejected push: the desktop changed underneath — offer a reload.
    pub conflict: Option<String>,
    /// Component being dragged: (id, grab offset in world mm).
    drag: Option<(CompId, [f32; 2])>,
    moved: Vec<(u32, [f32; 2])>,
    /// Canvas interaction mode and its transient state.
    pub edit_mode: EditMode,
    place_def: Option<String>,
    wire_from: Option<Endpoint>,
    /// A structural edit landed; re-pull the authoritative circuit next frame.
    needs_refetch: bool,
    /// Script editor: which component, the buffer, and diagnostics served by
    /// the desktop linter.
    script_comp: Option<u32>,
    script_buf: String,
    script_synced: String,
    script_dirty_at: Option<f64>,
    script_push: Option<u64>,
    diagnostics: Vec<(u32, u32, String)>,
    compile_error: Option<String>,
    /// Passive listener for the desktop's own discovery beacon (UDP 4521).
    desktop_scan: DesktopScan,
}

/// A WireLab desktop found via its self-broadcast beacon.
pub struct DiscoveredDesktop {
    pub addr: String,
    pub name: String,
    pub last_seen: f64,
}

/// Listens for `WIRELAB-HOST <port> <name>` beacons; the desktop's IP comes
/// from the datagram source, so the beacon only needs the port + a name.
#[derive(Default)]
pub struct DesktopScan {
    id: Option<u64>,
    found: std::collections::BTreeMap<String, DiscoveredDesktop>,
    pub error: Option<String>,
}

impl DesktopScan {
    pub fn poll(&mut self, ops: &dyn Ops, now: f64) {
        if self.error.is_some() {
            return;
        }
        let id = match self.id {
            Some(id) => id,
            None => {
                let payload = abi::encode(&net::UdpListen { port: 4521 });
                match ops.call(net::op::UDP_LISTEN, &payload) {
                    Ok(bytes) => match net::id_from_bytes(&bytes) {
                        Some(id) => {
                            self.id = Some(id);
                            id
                        }
                        None => {
                            self.error = Some("bad listen handle".into());
                            return;
                        }
                    },
                    Err(e) => {
                        self.error = Some(e);
                        return;
                    }
                }
            }
        };
        match ops.call(net::op::UDP_POLL, &net::id_to_bytes(id)) {
            Ok(bytes) => match abi::decode::<net::UdpPoll>(&bytes) {
                Ok(poll) => {
                    if let net::TcpState::Error(e) = &poll.state {
                        self.error = Some(e.clone());
                        self.id = None;
                        return;
                    }
                    for pkt in poll.packets {
                        if let Some(d) = parse_host_beacon(&pkt.from, &pkt.data, now) {
                            self.found.insert(d.addr.clone(), d);
                        }
                    }
                }
                Err(_) => self.error = Some("bad UdpPoll".into()),
            },
            Err(e) => {
                self.error = Some(e);
                self.id = None;
            }
        }
        self.found.retain(|_, d| now - d.last_seen < 12.0);
    }

    pub fn desktops(&self) -> impl Iterator<Item = &DiscoveredDesktop> {
        self.found.values()
    }
}

fn parse_host_beacon(from: &str, data: &[u8], now: f64) -> Option<DiscoveredDesktop> {
    let text = std::str::from_utf8(data).ok()?;
    let mut parts = text.splitn(3, ' ');
    if parts.next()? != "WIRELAB-HOST" {
        return None;
    }
    let port: u16 = parts.next()?.trim().parse().ok()?;
    let name = parts.next().unwrap_or("desktop").trim().to_string();
    let ip = from.rsplit_once(':').map(|(ip, _)| ip).unwrap_or(from);
    Some(DiscoveredDesktop { addr: format!("{ip}:{port}"), name, last_seen: now })
}

impl Default for ProjectView {
    fn default() -> Self {
        ProjectView {
            desktop_addr: String::new(),
            fetch: Fetch::Idle,
            snapshot: None,
            selected_board: 0,
            flow_view: FlowView::default(),
            flow_built_for: usize::MAX,
            last_fetch_at: -1e9,
            auto_refresh: true,
            flow_synced: Default::default(),
            flow_dirty_at: None,
            push: None,
            conflict: None,
            drag: None,
            moved: Vec::new(),
            edit_mode: EditMode::Move,
            place_def: None,
            wire_from: None,
            needs_refetch: false,
            script_comp: None,
            script_buf: String::new(),
            script_synced: String::new(),
            script_dirty_at: None,
            script_push: None,
            diagnostics: Vec::new(),
            compile_error: None,
            desktop_scan: DesktopScan::default(),
        }
    }
}

impl ProjectView {
    pub fn start_fetch(&mut self, ops: &dyn Ops, now: f64) {
        if matches!(self.fetch, Fetch::Pending(_)) || self.desktop_addr.trim().is_empty() {
            return;
        }
        self.last_fetch_at = now;
        let mut addr = self.desktop_addr.trim().to_string();
        if !addr.contains(':') {
            addr = format!("{addr}:4520");
        }
        let req = net::HttpRequest {
            method: "GET".into(),
            url: format!("http://{addr}/project"),
            headers: Vec::new(),
            body: Vec::new(),
            timeout_ms: 4000,
        };
        match ops.call(net::op::HTTP_REQUEST, &abi::encode(&req)) {
            Ok(bytes) => match net::id_from_bytes(&bytes) {
                Some(id) => self.fetch = Fetch::Pending(id),
                None => self.fetch = Fetch::Failed("bad request handle".into()),
            },
            Err(e) => self.fetch = Fetch::Failed(e),
        }
    }

    pub fn poll(&mut self, ops: &dyn Ops, now: f64) {
        self.poll_push(ops);
        self.poll_script(ops);
        self.desktop_scan.poll(ops, now);
        // Auto-fill the address from a discovered desktop and pull immediately.
        if self.desktop_addr.trim().is_empty() {
            let found = self.desktop_scan.desktops().next().map(|d| d.addr.clone());
            if let Some(addr) = found {
                self.desktop_addr = addr;
                self.start_fetch(ops, now);
            }
        }
        // A structural edit reshuffles ids/geometry — re-pull the truth.
        if self.needs_refetch && !matches!(self.fetch, Fetch::Pending(_)) {
            self.needs_refetch = false;
            self.start_fetch(ops, now);
        }
        if let Fetch::Pending(id) = self.fetch {
            match ops.call(net::op::HTTP_POLL, &net::id_to_bytes(id)) {
                Ok(bytes) => match abi::decode::<net::HttpPoll>(&bytes) {
                    Ok(net::HttpPoll::Pending) => {}
                    Ok(net::HttpPoll::Done(rsp)) => {
                        self.fetch = Fetch::Idle;
                        if rsp.status == 200 {
                            match serde_json::from_slice::<Snapshot>(&rsp.body) {
                                Ok(snap) => {
                                    self.selected_board =
                                        self.selected_board.min(snap.boards.len().saturating_sub(1));
                                    self.snapshot = Some(snap);
                                    self.flow_built_for = usize::MAX; // rebuild
                                    self.flow_dirty_at = None;
                                    self.conflict = None;
                                }
                                Err(e) => self.fetch = Fetch::Failed(format!("bad project: {e}")),
                            }
                        } else {
                            self.fetch = Fetch::Failed(format!("http {}", rsp.status));
                        }
                    }
                    Ok(net::HttpPoll::Error(e)) => self.fetch = Fetch::Failed(e),
                    Err(_) => self.fetch = Fetch::Failed("bad HttpPoll".into()),
                },
                Err(e) => self.fetch = Fetch::Failed(e),
            }
        } else if self.auto_refresh
            && self.snapshot.is_some()
            && now - self.last_fetch_at > 3.0
            && !self.editing()
        {
            self.start_fetch(ops, now);
        }
    }

    /// Local edits in flight — pause refreshes so they aren't clobbered.
    fn editing(&self) -> bool {
        self.flow_dirty_at.is_some()
            || self.push.is_some()
            || self.drag.is_some()
            || self.conflict.is_some()
            || !self.moved.is_empty()
            || self.edit_mode != EditMode::Move
            || self.wire_from.is_some()
            || self.needs_refetch
            || self.script_dirty_at.is_some()
            || self.script_push.is_some()
    }

    /// Post a structural edit `{board_id, op, ...}` to the desktop.
    fn edit(&mut self, ops: &dyn Ops, board_id: u64, mut op: serde_json::Value) {
        if self.push.is_some() {
            return; // one structural edit at a time
        }
        if let Some(m) = op.as_object_mut() {
            m.insert("board_id".into(), board_id.into());
        }
        self.post(ops, "/project/edit", op.to_string(), Push::Edit);
    }

    /// Shared error/conflict banner used by both the canvas and flow editors.
    fn conflict_banner(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) {
        let Some(err) = self.conflict.clone() else { return };
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(err).color(egui::Color32::from_rgb(243, 139, 168)));
            if ui.button("Dismiss").clicked() {
                self.conflict = None;
            }
            if ui.button("Reload").clicked() {
                self.conflict = None;
                self.wire_from = None;
                self.flow_dirty_at = None;
                self.moved.clear();
                self.start_fetch(ops, now);
            }
        });
    }

    /// The palette, grouped by category, sorted — the whole library rides in
    /// the snapshot's `defs`.
    fn categories(&self) -> Vec<(String, Vec<(String, String)>)> {
        let Some(snap) = &self.snapshot else { return Vec::new() };
        let mut by_cat: std::collections::BTreeMap<String, Vec<(String, String)>> =
            std::collections::BTreeMap::new();
        for (id, def) in &snap.defs {
            by_cat
                .entry(def.category.clone())
                .or_default()
                .push((id.clone(), def.name.clone()));
        }
        by_cat
            .into_iter()
            .map(|(c, mut v)| {
                v.sort_by(|a, b| a.1.cmp(&b.1));
                (c, v)
            })
            .collect()
    }

    fn post(&mut self, ops: &dyn Ops, path: &str, body: String, kind: Push) {
        let mut addr = self.desktop_addr.trim().to_string();
        if !addr.contains(':') {
            addr = format!("{addr}:4520");
        }
        let req = net::HttpRequest {
            method: "POST".into(),
            url: format!("http://{addr}{path}"),
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into_bytes(),
            timeout_ms: 4000,
        };
        match ops.call(net::op::HTTP_REQUEST, &abi::encode(&req)) {
            Ok(bytes) => {
                if let Some(id) = net::id_from_bytes(&bytes) {
                    self.push = Some((id, kind));
                }
            }
            Err(e) => self.conflict = Some(e),
        }
    }

    fn poll_push(&mut self, ops: &dyn Ops) {
        let Some((id, _)) = &self.push else { return };
        let id = *id;
        let rsp = match ops.call(net::op::HTTP_POLL, &net::id_to_bytes(id)) {
            Ok(bytes) => match abi::decode::<net::HttpPoll>(&bytes) {
                Ok(net::HttpPoll::Pending) => return,
                Ok(net::HttpPoll::Done(rsp)) => Some(rsp),
                Ok(net::HttpPoll::Error(e)) => {
                    self.push = None;
                    self.conflict = Some(e);
                    return;
                }
                Err(_) => None,
            },
            Err(_) => None,
        };
        let Some((_, kind)) = self.push.take() else { return };
        let Some(rsp) = rsp else {
            self.conflict = Some("push failed".into());
            return;
        };
        let v: serde_json::Value = serde_json::from_slice(&rsp.body).unwrap_or_default();
        let ok = v.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
        if !ok {
            let err = v
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("push rejected")
                .to_string();
            self.conflict = Some(err);
            return;
        }
        match kind {
            Push::Flow(graph) => {
                // Agreed: the pushed graph is the new shared truth.
                if let (Some(snap), Some(base)) = (
                    &mut self.snapshot,
                    v.get("base").and_then(serde_json::Value::as_u64),
                ) {
                    let idx = self.selected_board.min(snap.boards.len().saturating_sub(1));
                    let bid = snap.boards[idx].id;
                    snap.flow_bases.insert(bid.to_string(), base);
                    snap.boards[idx].flow = graph.clone();
                }
                self.flow_synced = graph;
                self.flow_dirty_at = None;
            }
            Push::Moves => {}
            Push::Edit => {
                // The circuit changed structurally: pull fresh ids/geometry.
                self.wire_from = None;
                self.needs_refetch = true;
            }
        }
    }

    /// Address bar + board chips; returns whether a snapshot is showing.
    pub fn header(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) -> bool {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("desktop").small());
            ui.add(
                egui::TextEdit::singleline(&mut self.desktop_addr)
                    .hint_text("192.168.1.x (WireLab app)")
                    .desired_width(150.0),
            );
            if ui.button("Fetch").clicked() {
                self.start_fetch(ops, now);
            }
            if matches!(self.fetch, Fetch::Pending(_)) {
                ui.spinner();
            }
            ui.checkbox(&mut self.auto_refresh, egui::RichText::new("live").small());
        });
        if let Fetch::Failed(e) = &self.fetch {
            ui.label(
                egui::RichText::new(e)
                    .small()
                    .color(egui::Color32::from_rgb(243, 139, 168)),
            );
        }
        // Auto-discovered desktops — tap to connect.
        let found: Vec<(String, String)> =
            self.desktop_scan.desktops().map(|d| (d.addr.clone(), d.name.clone())).collect();
        if !found.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("found:").small().weak());
                for (addr, name) in &found {
                    if ui.button(egui::RichText::new(format!("🖥 {name}")).small()).clicked() {
                        self.desktop_addr = addr.clone();
                        self.start_fetch(ops, now);
                    }
                }
            });
        }
        let Some(snap) = &self.snapshot else {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(if found.is_empty() {
                    "enter the desktop's IP, or open WireLab on your Mac/PC \
                     and it will appear here automatically"
                } else {
                    "tap a discovered desktop above to connect"
                })
                .weak(),
            );
            return false;
        };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&snap.name).strong());
            ui.separator();
            for (i, b) in snap.boards.iter().enumerate() {
                if ui.selectable_label(self.selected_board == i, &b.name).clicked() {
                    self.selected_board = i;
                }
            }
        });
        true
    }

    // ── circuit canvas ─────────────────────────────────────────────────

    /// Point-to-segment distance in pixels, for tapping a wire.
    fn seg_dist(pt: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
        let ab = b - a;
        let len2 = ab.length_sq().max(1e-3);
        let t = ((pt - a).dot(ab) / len2).clamp(0.0, 1.0);
        pt.distance(a + ab * t)
    }

    fn canvas_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for (m, label) in [
                (EditMode::Move, "Move"),
                (EditMode::Wire, "Wire"),
                (EditMode::Add, "Add"),
                (EditMode::Delete, "Delete"),
            ] {
                if ui.selectable_label(self.edit_mode == m, label).clicked() {
                    self.edit_mode = m;
                    self.wire_from = None;
                }
            }
            ui.separator();
            match self.edit_mode {
                EditMode::Add => {
                    let cats = self.categories();
                    let text = self
                        .place_def
                        .as_ref()
                        .and_then(|id| cats.iter().flat_map(|(_, v)| v).find(|(i, _)| i == id))
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "pick part".to_string());
                    let mut chosen = None;
                    ui.menu_button(text, |ui| {
                        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            for (cat, parts) in &cats {
                                ui.menu_button(cat, |ui| {
                                    for (id, name) in parts {
                                        if ui.button(name).clicked() {
                                            chosen = Some(id.clone());
                                            ui.close();
                                        }
                                    }
                                });
                            }
                        });
                    });
                    if let Some(c) = chosen {
                        self.place_def = Some(c);
                    }
                    ui.label(egui::RichText::new("tap to place").small().weak());
                }
                EditMode::Wire => {
                    let hint = if self.wire_from.is_some() {
                        "tap the second pin/terminal"
                    } else {
                        "tap a pin or terminal"
                    };
                    ui.label(egui::RichText::new(hint).small().weak());
                }
                EditMode::Delete => {
                    ui.label(egui::RichText::new("tap a part or wire to remove").small().weak());
                }
                EditMode::Move => {
                    ui.label(egui::RichText::new("drag parts to reposition").small().weak());
                }
            }
        });
    }

    pub fn show_canvas(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) {
        self.conflict_banner(ui, ops, now);
        self.canvas_toolbar(ui);

        // Owned interaction data collected during the draw pass, so mutation
        // below doesn't fight the read borrow of the snapshot.
        let mut endpoints: Vec<(Endpoint, egui::Pos2)> = Vec::new();
        let mut comp_bodies: Vec<(CompId, [f32; 2], [f32; 2])> = Vec::new();
        let mut wire_segs: Vec<(WireId, egui::Pos2, egui::Pos2)> = Vec::new();

        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 6.0, egui::Color32::from_gray(16));

        let (min, scale, origin, board_id) = {
            let Some(snap) = &self.snapshot else { return };
            let Some(tab) = snap.boards.get(self.selected_board) else { return };
            let Some(profile) = snap.profiles.get(&tab.circuit.board_id) else {
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "board profile missing",
                    egui::FontId::proportional(12.0),
                    egui::Color32::from_gray(160),
                );
                return;
            };
            let board_id = tab.id;

            // World bounding box (mm): board + components, padded.
            let mut min = tab.circuit.board_pos;
            let mut max = [min[0] + profile.width_mm, min[1] + profile.height_mm];
            for c in tab.circuit.components.values() {
                min[0] = min[0].min(c.pos[0] - 12.0);
                min[1] = min[1].min(c.pos[1] - 12.0);
                max[0] = max[0].max(c.pos[0] + 12.0);
                max[1] = max[1].max(c.pos[1] + 12.0);
            }
            let span = ((max[0] - min[0]).max(1.0), (max[1] - min[1]).max(1.0));
            let scale = ((rect.width() - 24.0) / span.0)
                .min((rect.height() - 24.0) / span.1)
                .clamp(0.5, 12.0);
            let origin = rect.min + egui::vec2(12.0, 12.0);
            let to_px = |w: [f32; 2]| {
                origin + egui::vec2((w[0] - min[0]) * scale, (w[1] - min[1]) * scale)
            };

            // Board body.
            let board_rect = egui::Rect::from_two_pos(
                to_px(tab.circuit.board_pos),
                to_px([
                    tab.circuit.board_pos[0] + profile.width_mm,
                    tab.circuit.board_pos[1] + profile.height_mm,
                ]),
            );
            p.rect_filled(board_rect, 4.0, egui::Color32::from_rgb(24, 30, 24));
            p.rect_stroke(
                board_rect,
                4.0,
                egui::Stroke::new(1.0, egui::Color32::from_gray(70)),
                egui::StrokeKind::Middle,
            );
            p.text(
                board_rect.center_top() + egui::vec2(0.0, 8.0),
                egui::Align2::CENTER_TOP,
                &profile.name,
                egui::FontId::proportional(10.0),
                egui::Color32::from_gray(140),
            );

            let ep_pos = |ep: &Endpoint| -> Option<[f32; 2]> {
                match ep {
                    Endpoint::BoardPin { key } => {
                        let pin = profile.pins.iter().find(|p| &p.key == key)?;
                        Some(geometry::board_pin_world_pos(profile, pin, tab.circuit.board_pos))
                    }
                    Endpoint::Terminal { comp, terminal } => {
                        let c = tab.circuit.components.get(comp)?;
                        let def = snap.defs.get(&c.def_id)?;
                        let idx = def.terminals.iter().position(|t| &t.id == terminal)?;
                        Some(geometry::terminal_world_pos(c, def, idx))
                    }
                }
            };
            // Exit direction / body clearance, matching the desktop so wires
            // route identically through the shared geometry::wire_path.
            let exit_dir = |ep: &Endpoint| -> [f32; 2] {
                match ep {
                    Endpoint::BoardPin { key } => profile
                        .pins
                        .iter()
                        .find(|p| &p.key == key)
                        .map(|p| {
                            use wirelab_core::board::Side;
                            match p.side {
                                Side::Left => [-1.0, 0.0],
                                Side::Right => [1.0, 0.0],
                                Side::Top => [0.0, -1.0],
                                Side::Bottom => [0.0, 1.0],
                            }
                        })
                        .unwrap_or([0.0, 0.0]),
                    Endpoint::Terminal { comp, terminal } => tab
                        .circuit
                        .components
                        .get(comp)
                        .and_then(|c| {
                            let def = snap.defs.get(&c.def_id)?;
                            let idx = def.terminals.iter().position(|t| &t.id == terminal)?;
                            let t = geometry::terminal_world_pos(c, def, idx);
                            Some([t[0] - c.pos[0], t[1] - c.pos[1]])
                        })
                        .unwrap_or([0.0, 0.0]),
                }
            };
            let clearance = |ep: &Endpoint| -> f32 {
                match ep {
                    Endpoint::BoardPin { .. } => 0.0,
                    Endpoint::Terminal { comp, .. } => tab
                        .circuit
                        .components
                        .get(comp)
                        .and_then(|c| snap.defs.get(&c.def_id))
                        .map(|d| (d.visual.width_mm.max(d.visual.height_mm) / 2.0 + 2.5) * scale)
                        .unwrap_or(0.0),
                }
            };
            for (id, w) in &tab.circuit.wires {
                if let (Some(a), Some(b)) = (ep_pos(&w.a), ep_pos(&w.b)) {
                    let (pa, pb) = (to_px(a), to_px(b));
                    let route = geometry::Route {
                        exit_a: exit_dir(&w.a),
                        exit_b: exit_dir(&w.b),
                        lane: (id.0 % 5) as i32 - 2,
                        stub: (3.0 * scale).clamp(8.0, 20.0),
                        clear_a: clearance(&w.a),
                        clear_b: clearance(&w.b),
                    };
                    let pts = geometry::wire_path([pa.x, pa.y], [pb.x, pb.y], &route);
                    let color = egui::Color32::from_rgb(
                        w.color[0].max(90),
                        w.color[1].max(90),
                        w.color[2].max(90),
                    );
                    for seg in pts.windows(2) {
                        let s0 = egui::pos2(seg[0][0], seg[0][1]);
                        let s1 = egui::pos2(seg[1][0], seg[1][1]);
                        p.line_segment([s0, s1], egui::Stroke::new(1.5, color));
                        // Every segment is a delete hit-target.
                        wire_segs.push((*id, s0, s1));
                    }
                }
            }

            // Board pins (also tappable endpoints).
            for pin in &profile.pins {
                let w = geometry::board_pin_world_pos(profile, pin, tab.circuit.board_pos);
                let color = match &pin.kind {
                    wirelab_core::board::PinKind::Gnd => egui::Color32::from_gray(90),
                    wirelab_core::board::PinKind::V3_3 => egui::Color32::from_rgb(220, 90, 90),
                    wirelab_core::board::PinKind::V5 => egui::Color32::from_rgb(240, 160, 60),
                    _ => egui::Color32::from_gray(160),
                };
                let px = to_px(w);
                p.circle_filled(px, (1.1 * scale).clamp(1.5, 4.0), color);
                // Pin label, placed just inside the board so it reads against
                // the dark body (only when there's room).
                if scale >= 2.5 {
                    use wirelab_core::board::Side;
                    let (align, off) = match pin.side {
                        Side::Left => (egui::Align2::LEFT_CENTER, egui::vec2(6.0, 0.0)),
                        Side::Right => (egui::Align2::RIGHT_CENTER, egui::vec2(-6.0, 0.0)),
                        Side::Top => (egui::Align2::CENTER_TOP, egui::vec2(0.0, 5.0)),
                        Side::Bottom => (egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -5.0)),
                    };
                    p.text(
                        px + off,
                        align,
                        &pin.key,
                        egui::FontId::proportional(7.0),
                        egui::Color32::from_gray(150),
                    );
                }
                endpoints.push((Endpoint::BoardPin { key: pin.key.clone() }, px));
            }

            // Components + their terminals.
            for c in tab.circuit.components.values() {
                let Some(def) = snap.defs.get(&c.def_id) else { continue };
                let half = [def.visual.width_mm / 2.0, def.visual.height_mm / 2.0];
                comp_bodies.push((c.id, c.pos, half));
                let r = egui::Rect::from_two_pos(
                    to_px([c.pos[0] - half[0], c.pos[1] - half[1]]),
                    to_px([c.pos[0] + half[0], c.pos[1] + half[1]]),
                );
                let fill = egui::Color32::from_rgb(
                    (def.visual.color[0] / 3).max(30),
                    (def.visual.color[1] / 3).max(30),
                    (def.visual.color[2] / 3).max(30),
                );
                p.rect_filled(r, 3.0, fill);
                p.rect_stroke(
                    r,
                    3.0,
                    egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(
                            def.visual.color[0],
                            def.visual.color[1],
                            def.visual.color[2],
                        ),
                    ),
                    egui::StrokeKind::Middle,
                );
                let label = if c.label.is_empty() { &def.name } else { &c.label };
                p.text(
                    r.center_top() - egui::vec2(0.0, 3.0),
                    egui::Align2::CENTER_BOTTOM,
                    label,
                    egui::FontId::proportional(9.5),
                    egui::Color32::from_gray(200),
                );
                if c.script.is_some() {
                    p.text(
                        r.right_top() + egui::vec2(3.0, -3.0),
                        egui::Align2::LEFT_BOTTOM,
                        "📜",
                        egui::FontId::proportional(9.0),
                        egui::Color32::from_gray(200),
                    );
                }
                // Multi-terminal parts (LCD, sensors) get pin-id labels like
                // the desktop; 2-3 pin parts are self-evident.
                let label_terms = def.terminals.len() > 3 && scale >= 3.0;
                for (i, t) in def.terminals.iter().enumerate() {
                    let w = geometry::terminal_world_pos(c, def, i);
                    let tpx = to_px(w);
                    p.circle_filled(tpx, 1.6, egui::Color32::from_gray(150));
                    if label_terms {
                        // Push the label outward from the component centre.
                        let out = if w[0] >= c.pos[0] {
                            egui::vec2(4.0, 0.0)
                        } else {
                            egui::vec2(-4.0, 0.0)
                        };
                        let align = if w[0] >= c.pos[0] {
                            egui::Align2::LEFT_CENTER
                        } else {
                            egui::Align2::RIGHT_CENTER
                        };
                        p.text(
                            tpx + out,
                            align,
                            &t.id,
                            egui::FontId::proportional(6.5),
                            egui::Color32::from_gray(130),
                        );
                    }
                    endpoints.push((
                        Endpoint::Terminal { comp: c.id, terminal: t.id.clone() },
                        tpx,
                    ));
                }
            }
            (min, scale, origin, board_id)
        };

        // In wire mode, make every endpoint an obvious tap target and show
        // the pending source ringed.
        if self.edit_mode == EditMode::Wire {
            for (ep, px) in &endpoints {
                let picked = self.wire_from.as_ref() == Some(ep);
                p.circle_stroke(
                    *px,
                    if picked { 6.0 } else { 3.5 },
                    egui::Stroke::new(
                        if picked { 2.0 } else { 1.0 },
                        if picked {
                            egui::Color32::from_rgb(249, 226, 175)
                        } else {
                            egui::Color32::from_rgb(203, 166, 247)
                        },
                    ),
                );
            }
        }

        // ── interaction ────────────────────────────────────────────────
        let to_world = |px: egui::Pos2| {
            [(px.x - origin.x) / scale + min[0], (px.y - origin.y) / scale + min[1]]
        };
        match self.edit_mode {
            EditMode::Move => {
                if response.drag_started()
                    && let Some(ptr) = response.interact_pointer_pos()
                {
                    let w = to_world(ptr);
                    self.drag = comp_bodies
                        .iter()
                        .find(|(_, c, h)| {
                            (w[0] - c[0]).abs() <= h[0].max(3.0)
                                && (w[1] - c[1]).abs() <= h[1].max(3.0)
                        })
                        .map(|(id, c, _)| (*id, [w[0] - c[0], w[1] - c[1]]));
                }
                if let Some((id, grab)) = self.drag
                    && let Some(ptr) = response.interact_pointer_pos()
                {
                    let wp = to_world(ptr);
                    let pos = [wp[0] - grab[0], wp[1] - grab[1]];
                    if let Some(snap) = &mut self.snapshot
                        && let Some(tab) = snap.boards.get_mut(self.selected_board)
                        && let Some(c) = tab.circuit.components.get_mut(&id)
                    {
                        c.pos = pos;
                    }
                    if response.drag_stopped() {
                        self.drag = None;
                        self.moved.push((id.0, pos));
                    }
                }
                if !self.moved.is_empty() && self.push.is_none() {
                    let moves = std::mem::take(&mut self.moved);
                    let body =
                        serde_json::json!({ "board_id": board_id, "moves": moves }).to_string();
                    self.post(ops, "/project/positions", body, Push::Moves);
                }
            }
            EditMode::Add => {
                if response.clicked()
                    && let Some(ptr) = response.interact_pointer_pos()
                    && let Some(def) = self.place_def.clone()
                {
                    let w = to_world(ptr);
                    self.edit(
                        ops,
                        board_id,
                        serde_json::json!({ "op": "add_comp", "def_id": def, "pos": [w[0], w[1]] }),
                    );
                }
            }
            EditMode::Wire => {
                if response.clicked()
                    && let Some(ptr) = response.interact_pointer_pos()
                {
                    let nearest = endpoints
                        .iter()
                        .map(|(ep, px)| (ep, px.distance(ptr)))
                        .filter(|(_, d)| *d <= 22.0)
                        .min_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|(ep, _)| ep.clone());
                    if let Some(ep) = nearest {
                        match self.wire_from.take() {
                            None => self.wire_from = Some(ep),
                            Some(from) if from != ep => self.edit(
                                ops,
                                board_id,
                                serde_json::json!({
                                    "op": "add_wire",
                                    "a": serde_json::to_value(&from).unwrap_or_default(),
                                    "b": serde_json::to_value(&ep).unwrap_or_default(),
                                }),
                            ),
                            _ => {}
                        }
                    }
                }
            }
            EditMode::Delete => {
                if response.clicked()
                    && let Some(ptr) = response.interact_pointer_pos()
                {
                    let w = to_world(ptr);
                    let comp = comp_bodies.iter().find(|(_, c, h)| {
                        (w[0] - c[0]).abs() <= h[0].max(3.0)
                            && (w[1] - c[1]).abs() <= h[1].max(3.0)
                    });
                    if let Some((id, _, _)) = comp {
                        self.edit(
                            ops,
                            board_id,
                            serde_json::json!({ "op": "remove_comp", "comp": id.0 }),
                        );
                    } else if let Some(wid) = wire_segs
                        .iter()
                        .map(|(id, a, b)| (*id, Self::seg_dist(ptr, *a, *b)))
                        .filter(|(_, d)| *d <= 10.0)
                        .min_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|(id, _)| id)
                    {
                        self.edit(
                            ops,
                            board_id,
                            serde_json::json!({ "op": "remove_wire", "wire": wid.0 }),
                        );
                    }
                }
            }
        }
    }

    // ── flow graph ─────────────────────────────────────────────────────

    pub fn show_flow(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) {
        self.conflict_banner(ui, ops, now);
        let Some(snap) = &self.snapshot else { return };
        let Some(tab) = snap.boards.get(self.selected_board) else { return };
        let board_id = tab.id;
        if self.flow_built_for != self.selected_board {
            self.flow_view = FlowView { snarl: build_snarl(&tab.flow), built_rev: 0 };
            self.flow_built_for = self.selected_board;
            self.flow_synced = tab.flow.clone();
            self.flow_dirty_at = None;
        }
        let comp_names: Vec<String> =
            tab.circuit.components.values().map(|c| c.label.clone()).collect();
        let base = snap.flow_bases.get(&board_id.to_string()).copied();
        let index: HashMap<wirelab_flow_ui::egui_snarl::NodeId, usize> = self
            .flow_view
            .snarl
            .nodes_pos_ids()
            .enumerate()
            .map(|(i, (id, _, _))| (id, i))
            .collect();
        let mut viewer = FlowViewer::new(comp_names, None, index);
        viewer.options = ViewerOptions { editable: true };
        wirelab_flow_ui::show(&mut self.flow_view, &mut viewer, "ipad-flow", ui);

        // Push edits once the graph has sat still for a moment.
        let graph = wirelab_flow_ui::extract_graph(&self.flow_view.snarl);
        if graph != self.flow_synced {
            if self.flow_dirty_at.is_none() {
                self.flow_dirty_at = Some(now);
            }
            if let (Some(t0), None, None, Some(base)) =
                (self.flow_dirty_at, self.push.as_ref(), self.conflict.as_ref(), base)
                && now - t0 > 0.7
            {
                let body = serde_json::json!({
                    "board_id": board_id,
                    "base": base,
                    "nodes": serde_json::to_value(&graph.nodes).unwrap_or_default(),
                    "wires": serde_json::to_value(&graph.wires).unwrap_or_default(),
                })
                .to_string();
                self.post(ops, "/project/flow", body, Push::Flow(graph));
            }
        } else {
            self.flow_dirty_at = None;
        }
        ui.label(
            egui::RichText::new(match (&self.flow_dirty_at, &self.push) {
                (_, Some(_)) => "syncing…",
                (Some(_), _) => "• editing (auto-syncs)",
                _ => "✔ in sync with the desktop",
            })
            .small()
            .weak(),
        );
    }

    // ── script editor ──────────────────────────────────────────────────

    /// Load a component's script into the buffer. A component with no script
    /// gets the same starter template the desktop seeds (component-tailored,
    /// with the `me`/`this`/by-name explainer), left dirty so it applies.
    fn load_script(&mut self, comp: u32) {
        let (buf, synced) = {
            let Some(snap) = &self.snapshot else { return };
            let Some(tab) = snap.boards.get(self.selected_board) else { return };
            let stored =
                tab.circuit.components.get(&CompId(comp)).and_then(|c| c.script.clone());
            match stored {
                Some(s) if !s.is_empty() => (s.clone(), s),
                _ => {
                    // Build a throwaway library from the snapshot's defs so the
                    // generated names match the desktop's exactly.
                    let mut lib = wirelab_core::library::Library::default();
                    for def in snap.defs.values() {
                        lib.add_component(def.clone());
                    }
                    let names = wirelab_core::script::component_names(&tab.circuit, &lib);
                    let own = names.get(&CompId(comp)).cloned().unwrap_or_default();
                    let mut peers: Vec<String> = names
                        .iter()
                        .filter(|(id, _)| id.0 != comp)
                        .map(|(_, n)| n.clone())
                        .collect();
                    peers.sort();
                    let template = tab
                        .circuit
                        .components
                        .get(&CompId(comp))
                        .and_then(|c| snap.defs.get(&c.def_id))
                        .map(|d| wirelab_core::script::script_template(d, &own, &peers))
                        .unwrap_or_default();
                    // Shown as a starting point; not attached until the user
                    // edits it (buf == synced → not dirty → no auto-push).
                    (template.clone(), template)
                }
            }
        };
        self.script_comp = Some(comp);
        self.script_buf = buf;
        self.script_synced = synced;
        self.script_dirty_at = None;
        self.diagnostics.clear();
        self.compile_error = None;
    }

    fn post_script(&mut self, ops: &dyn Ops, board_id: u64, comp: u32) {
        let mut addr = self.desktop_addr.trim().to_string();
        if !addr.contains(':') {
            addr = format!("{addr}:4520");
        }
        let body = serde_json::json!({
            "board_id": board_id, "comp": comp, "source": self.script_buf
        })
        .to_string();
        let req = net::HttpRequest {
            method: "POST".into(),
            url: format!("http://{addr}/project/script"),
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into_bytes(),
            timeout_ms: 4000,
        };
        match ops.call(net::op::HTTP_REQUEST, &abi::encode(&req)) {
            Ok(bytes) => {
                if let Some(id) = net::id_from_bytes(&bytes) {
                    self.script_push = Some(id);
                    // Optimistic: don't re-push identical text while in flight.
                    self.script_synced = self.script_buf.clone();
                    self.script_dirty_at = None;
                }
            }
            Err(e) => self.compile_error = Some(e),
        }
    }

    fn poll_script(&mut self, ops: &dyn Ops) {
        let Some(id) = self.script_push else { return };
        let rsp = match ops.call(net::op::HTTP_POLL, &net::id_to_bytes(id)) {
            Ok(bytes) => match abi::decode::<net::HttpPoll>(&bytes) {
                Ok(net::HttpPoll::Pending) => return,
                Ok(net::HttpPoll::Done(rsp)) => Some(rsp),
                Ok(net::HttpPoll::Error(e)) => {
                    self.script_push = None;
                    self.compile_error = Some(e);
                    return;
                }
                Err(_) => None,
            },
            Err(_) => None,
        };
        self.script_push = None;
        let Some(rsp) = rsp else { return };
        let v: serde_json::Value = serde_json::from_slice(&rsp.body).unwrap_or_default();
        if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            self.compile_error = Some(
                v.get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("script push failed")
                    .to_string(),
            );
            return;
        }
        self.diagnostics = v
            .get("diagnostics")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|d| {
                        Some((
                            d.get("line")?.as_u64()? as u32,
                            d.get("col")?.as_u64()? as u32,
                            d.get("message")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.compile_error =
            v.get("compile_error").and_then(serde_json::Value::as_str).map(str::to_string);
    }

    pub fn show_script(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) {
        self.conflict_banner(ui, ops, now);
        // Component list + the current board id, under a short read borrow.
        let (board_id, comps): (u64, Vec<(u32, String, bool)>) = {
            let Some(snap) = &self.snapshot else {
                return;
            };
            let Some(tab) = snap.boards.get(self.selected_board) else {
                return;
            };
            let comps = tab
                .circuit
                .components
                .iter()
                .map(|(id, c)| {
                    let name = if c.label.is_empty() { c.def_id.clone() } else { c.label.clone() };
                    (id.0, name, c.script.is_some())
                })
                .collect();
            (tab.id, comps)
        };

        // If the selection no longer exists (board switched), drop it.
        if let Some(sel) = self.script_comp
            && !comps.iter().any(|(id, _, _)| *id == sel)
        {
            self.script_comp = None;
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("editing").small());
            let cur = self
                .script_comp
                .and_then(|id| comps.iter().find(|(i, _, _)| *i == id))
                .map(|(_, n, _)| n.clone())
                .unwrap_or_else(|| "pick a component".to_string());
            let mut pick = None;
            egui::ComboBox::from_id_salt("script-comp").selected_text(cur).width(160.0).show_ui(
                ui,
                |ui| {
                    for (id, name, has) in &comps {
                        let label = if *has { format!("📜 {name}") } else { name.clone() };
                        if ui.selectable_label(self.script_comp == Some(*id), label).clicked() {
                            pick = Some(*id);
                        }
                    }
                },
            );
            if let Some(id) = pick {
                self.load_script(id);
            }
        });

        let Some(comp) = self.script_comp else {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("pick a component to edit its Rhai script").weak(),
            );
            return;
        };

        // The editor. A TextEdit raises the iOS soft keyboard on focus.
        egui::ScrollArea::vertical().id_salt("script-scroll").max_height(320.0).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.script_buf)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(16),
            );
        });

        // Debounced push to the desktop, which applies + lints the script.
        if self.script_buf != self.script_synced {
            if self.script_dirty_at.is_none() {
                self.script_dirty_at = Some(now);
            }
            if self.script_push.is_none()
                && let Some(t0) = self.script_dirty_at
                && now - t0 > 0.7
            {
                self.post_script(ops, board_id, comp);
            }
        } else if self.script_push.is_none() {
            self.script_dirty_at = None;
        }

        // Status + diagnostics served by the desktop linter.
        ui.horizontal(|ui| {
            let n = self.diagnostics.len() + usize::from(self.compile_error.is_some());
            let status = match (&self.script_dirty_at, &self.script_push) {
                (_, Some(_)) => egui::RichText::new("linting…").small().weak(),
                (Some(_), _) => egui::RichText::new("• editing").small().weak(),
                _ if n == 0 => egui::RichText::new("✔ no problems")
                    .small()
                    .color(egui::Color32::from_rgb(166, 227, 161)),
                _ => egui::RichText::new(format!("✖ {n} problem(s)"))
                    .small()
                    .color(egui::Color32::from_rgb(243, 139, 168)),
            };
            ui.label(status);
        });
        if let Some(e) = &self.compile_error {
            ui.label(
                egui::RichText::new(e).small().color(egui::Color32::from_rgb(243, 139, 168)),
            );
        }
        for (line, col, msg) in &self.diagnostics {
            ui.label(
                egui::RichText::new(format!("{line}:{col}  {msg}"))
                    .small()
                    .color(egui::Color32::from_rgb(243, 139, 168)),
            );
        }
    }
}
