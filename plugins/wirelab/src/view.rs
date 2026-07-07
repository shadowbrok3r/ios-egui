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

/// What's selected on the canvas (for highlight + delete).
#[derive(Clone, Copy, PartialEq)]
enum CanvasSel {
    Comp(u32),
    Wire(u32),
}

/// An in-progress touch drag, decided at drag-start by what's under the finger.
enum CanvasDrag {
    /// Dragging a component body to reposition it.
    Move { id: CompId, grab: [f32; 2] },
    /// Dragging from a pin/terminal to run a wire.
    Wire { from: Endpoint },
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
    /// In-progress canvas drag (move a part, or pull a wire).
    drag: Option<CanvasDrag>,
    moved: Vec<(u32, [f32; 2])>,
    /// Palette part armed for placing; tap empty canvas to drop it.
    armed_def: Option<String>,
    /// Current canvas selection (highlight + Delete target).
    sel: Option<CanvasSel>,
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
    /// Autocomplete popup: candidates + the char index the word started at,
    /// recomputed only when (cursor, buffer-hash) moves.
    completion: Vec<wirelab_core::script_api::CompletionItem>,
    completion_ws: usize,
    completion_key: (usize, u64),
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

const RHAI_KEYWORDS: &[&str] = &[
    "fn", "let", "const", "if", "else", "switch", "while", "loop", "for", "in", "do", "until",
    "return", "break", "continue", "true", "false", "this", "throw", "try", "catch",
];

fn char_to_byte(buf: &str, ci: usize) -> usize {
    buf.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(buf.len())
}

/// A diagnostic point (1-based line/col) → byte span covering the token there.
fn line_col_to_span(text: &str, line: u32, col: u32) -> Option<(usize, usize)> {
    let mut cur = 1u32;
    let mut it = text.char_indices().peekable();
    // Walk to the start of `line`.
    while cur < line {
        match it.next() {
            Some((_, '\n')) => cur += 1,
            Some(_) => {}
            None => return None,
        }
    }
    // Advance col-1 chars into the line.
    for _ in 1..col {
        match it.peek() {
            Some((_, '\n')) | None => break,
            Some(_) => {
                it.next();
            }
        }
    }
    let s = it.peek().map(|(b, _)| *b).unwrap_or(text.len());
    // Extend over the identifier token (or one char).
    let mut e = s;
    for (b, ch) in text[s..].char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            e = s + b + ch.len_utf8();
        } else {
            break;
        }
    }
    if e == s {
        e = (s + 1).min(text.len());
    }
    Some((s, e))
}

/// Minimal Rhai syntax highlighter → egui LayoutJob, with red underlines over
/// the diagnostic spans. Colors track the desktop's dark code theme.
fn highlight_rhai(
    text: &str,
    base: egui::Color32,
    diag: &[(usize, usize)],
) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let comment = egui::Color32::from_gray(110);
    let string = egui::Color32::from_rgb(166, 227, 161);
    let number = egui::Color32::from_rgb(137, 180, 250);
    let keyword = egui::Color32::from_rgb(203, 166, 247);
    let font = egui::FontId::monospace(13.0);
    let mut job = LayoutJob::default();
    let underlined = |s: usize, e: usize| diag.iter().any(|(ds, de)| s < *de && *ds < e);
    let mut push = |slice: &str, s: usize, color: egui::Color32| {
        let mut fmt = TextFormat::simple(font.clone(), color);
        if underlined(s, s + slice.len()) {
            fmt.underline = egui::Stroke::new(1.5, egui::Color32::from_rgb(243, 139, 168));
        }
        job.append(slice, 0.0, fmt);
    };
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let ch = rest.chars().next().unwrap();
        if rest.starts_with("//") {
            let end = rest.find('\n').map(|n| i + n).unwrap_or(text.len());
            push(&text[i..end], i, comment);
            i = end;
        } else if ch == '"' || ch == '`' {
            let quote = ch;
            let mut j = i + 1;
            while j < text.len() {
                let c = text[j..].chars().next().unwrap();
                j += c.len_utf8();
                if c == '\\' && j < text.len() {
                    j += text[j..].chars().next().unwrap().len_utf8();
                } else if c == quote {
                    break;
                }
            }
            push(&text[i..j], i, string);
            i = j;
        } else if ch.is_ascii_digit() {
            let mut j = i;
            while j < text.len() {
                let c = text[j..].chars().next().unwrap();
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                    j += c.len_utf8();
                } else {
                    break;
                }
            }
            push(&text[i..j], i, number);
            i = j;
        } else if ch.is_alphabetic() || ch == '_' {
            let mut j = i;
            while j < text.len() {
                let c = text[j..].chars().next().unwrap();
                if c.is_alphanumeric() || c == '_' {
                    j += c.len_utf8();
                } else {
                    break;
                }
            }
            let word = &text[i..j];
            let color = if RHAI_KEYWORDS.contains(&word) { keyword } else { base };
            push(word, i, color);
            i = j;
        } else {
            let n = ch.len_utf8();
            push(&text[i..i + n], i, base);
            i += n;
        }
    }
    job
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
            armed_def: None,
            sel: None,
            needs_refetch: false,
            script_comp: None,
            script_buf: String::new(),
            script_synced: String::new(),
            script_dirty_at: None,
            script_push: None,
            diagnostics: Vec::new(),
            compile_error: None,
            completion: Vec::new(),
            completion_ws: 0,
            completion_key: (usize::MAX, 0),
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
            || self.armed_def.is_some()
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
                self.drag = None;
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
                self.drag = None;
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

    /// Modeless toolbar (like the desktop): a palette to arm a part for
    /// placing, a Delete button for the current selection, and a hint. Drag a
    /// body to move it; drag from a pin/terminal to wire; tap to select.
    fn canvas_toolbar(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, board_id: u64) {
        ui.horizontal_wrapped(|ui| {
            let cats = self.categories();
            let armed_name = self
                .armed_def
                .as_ref()
                .and_then(|id| cats.iter().flat_map(|(_, v)| v).find(|(i, _)| i == id))
                .map(|(_, n)| n.clone());
            let btn_text = match &armed_name {
                Some(n) => format!("placing: {n}"),
                None => "add part".to_string(),
            };
            let mut chosen = None;
            ui.menu_button(btn_text, |ui| {
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
                self.armed_def = Some(c);
            }
            if armed_name.is_some() && ui.button("stop").on_hover_text("stop placing").clicked() {
                self.armed_def = None;
            }

            // Delete the current selection.
            if let Some(sel) = self.sel {
                ui.separator();
                if ui.button("Delete").clicked() {
                    let op = match sel {
                        CanvasSel::Comp(id) => {
                            serde_json::json!({ "op": "remove_comp", "comp": id })
                        }
                        CanvasSel::Wire(id) => {
                            serde_json::json!({ "op": "remove_wire", "wire": id })
                        }
                    };
                    self.edit(ops, board_id, op);
                    self.sel = None;
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hint = if self.armed_def.is_some() {
                    "tap the board to place"
                } else if matches!(self.drag, Some(CanvasDrag::Wire { .. })) {
                    "release on a pin/terminal"
                } else {
                    "drag a part to move · drag from a pin to wire · tap to select"
                };
                ui.label(egui::RichText::new(hint).small().weak());
            });
        });
    }

    pub fn show_canvas(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) {
        self.conflict_banner(ui, ops, now);
        let toolbar_board = self
            .snapshot
            .as_ref()
            .and_then(|s| s.boards.get(self.selected_board))
            .map(|t| t.id);
        if let Some(bid) = toolbar_board {
            self.canvas_toolbar(ui, ops, bid);
        }

        // Owned interaction data collected during the draw pass, so mutation
        // below doesn't fight the read borrow of the snapshot.
        let mut endpoints: Vec<(Endpoint, egui::Pos2)> = Vec::new();
        let mut comp_bodies: Vec<(CompId, [f32; 2], [f32; 2])> = Vec::new();
        let mut wire_segs: Vec<(WireId, egui::Pos2, egui::Pos2)> = Vec::new();

        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 6.0, egui::Color32::from_rgb(13, 13, 18));

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

            // Dot grid (5 mm), matching the desktop canvas.
            let step = 5.0 * scale;
            if step > 7.0 {
                let dot = egui::Color32::from_gray(30);
                let mut x = rect.min.x + (origin.x - rect.min.x).rem_euclid(step);
                while x < rect.max.x {
                    let mut y = rect.min.y + (origin.y - rect.min.y).rem_euclid(step);
                    while y < rect.max.y {
                        p.circle_filled(egui::pos2(x, y), 1.0, dot);
                        y += step;
                    }
                    x += step;
                }
            }

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

        // Selection highlight.
        match self.sel {
            Some(CanvasSel::Comp(id)) => {
                if let Some((_, c, h)) = comp_bodies.iter().find(|(i, _, _)| i.0 == id) {
                    let r = egui::Rect::from_two_pos(
                        origin
                            + egui::vec2((c[0] - h[0] - min[0]) * scale, (c[1] - h[1] - min[1]) * scale),
                        origin
                            + egui::vec2((c[0] + h[0] - min[0]) * scale, (c[1] + h[1] - min[1]) * scale),
                    )
                    .expand(3.0);
                    p.rect_stroke(
                        r,
                        4.0,
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(249, 226, 175)),
                        egui::StrokeKind::Middle,
                    );
                }
            }
            Some(CanvasSel::Wire(id)) => {
                for (wid, s0, s1) in &wire_segs {
                    if wid.0 == id {
                        p.line_segment(
                            [*s0, *s1],
                            egui::Stroke::new(3.5, egui::Color32::from_rgb(249, 226, 175)),
                        );
                    }
                }
            }
            None => {}
        }

        // While pulling a wire, ring every endpoint and rubber-band to the finger.
        let to_world = |px: egui::Pos2| {
            [(px.x - origin.x) / scale + min[0], (px.y - origin.y) / scale + min[1]]
        };
        if let Some(CanvasDrag::Wire { from }) = &self.drag {
            for (_, px) in &endpoints {
                p.circle_stroke(
                    *px,
                    3.5,
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(203, 166, 247)),
                );
            }
            if let (Some(src), Some(ptr)) = (
                endpoints.iter().find(|(ep, _)| ep == from).map(|(_, px)| *px),
                response.interact_pointer_pos(),
            ) {
                p.line_segment(
                    [src, ptr],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(249, 226, 175)),
                );
            }
        }

        // ── interaction (modeless, like the desktop) ───────────────────
        let nearest_endpoint = |ptr: egui::Pos2, radius: f32| -> Option<Endpoint> {
            endpoints
                .iter()
                .map(|(ep, px)| (ep, px.distance(ptr)))
                .filter(|(_, d)| *d <= radius)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(ep, _)| ep.clone())
        };
        let hit_comp = |w: [f32; 2]| -> Option<CompId> {
            comp_bodies
                .iter()
                .find(|(_, c, h)| {
                    (w[0] - c[0]).abs() <= h[0].max(3.0) && (w[1] - c[1]).abs() <= h[1].max(3.0)
                })
                .map(|(id, _, _)| *id)
        };

        // Drag start: a pin/terminal begins a wire, a body begins a move.
        if response.drag_started()
            && let Some(ptr) = response.interact_pointer_pos()
        {
            if let Some(ep) = nearest_endpoint(ptr, 18.0) {
                self.drag = Some(CanvasDrag::Wire { from: ep });
            } else {
                let w = to_world(ptr);
                self.drag = comp_bodies
                    .iter()
                    .find(|(_, c, h)| {
                        (w[0] - c[0]).abs() <= h[0].max(3.0) && (w[1] - c[1]).abs() <= h[1].max(3.0)
                    })
                    .map(|(id, c, _)| CanvasDrag::Move { id: *id, grab: [w[0] - c[0], w[1] - c[1]] });
            }
        }

        match &self.drag {
            Some(CanvasDrag::Move { id, grab }) => {
                let (id, grab) = (*id, *grab);
                if let Some(ptr) = response.interact_pointer_pos() {
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
            }
            Some(CanvasDrag::Wire { from }) if response.drag_stopped() => {
                let from = from.clone();
                let target =
                    response.interact_pointer_pos().and_then(|ptr| nearest_endpoint(ptr, 22.0));
                self.drag = None;
                if let Some(to) = target
                    && to != from
                {
                    self.edit(
                        ops,
                        board_id,
                        serde_json::json!({
                            "op": "add_wire",
                            "a": serde_json::to_value(&from).unwrap_or_default(),
                            "b": serde_json::to_value(&to).unwrap_or_default(),
                        }),
                    );
                }
            }
            _ => {}
        }

        // Flush finished moves.
        if !self.moved.is_empty() && self.push.is_none() {
            let moves = std::mem::take(&mut self.moved);
            let body = serde_json::json!({ "board_id": board_id, "moves": moves }).to_string();
            self.post(ops, "/project/positions", body, Push::Moves);
        }

        // A plain tap: place the armed part, else select what's under it.
        if response.clicked()
            && let Some(ptr) = response.interact_pointer_pos()
        {
            let w = to_world(ptr);
            if let Some(def) = self.armed_def.clone() {
                self.edit(
                    ops,
                    board_id,
                    serde_json::json!({ "op": "add_comp", "def_id": def, "pos": [w[0], w[1]] }),
                );
            } else if let Some(id) = hit_comp(w) {
                self.sel = Some(CanvasSel::Comp(id.0));
            } else if let Some(wid) = wire_segs
                .iter()
                .map(|(id, a, b)| (*id, Self::seg_dist(ptr, *a, *b)))
                .filter(|(_, d)| *d <= 10.0)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(id, _)| id)
            {
                self.sel = Some(CanvasSel::Wire(wid.0));
            } else {
                self.sel = None;
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

        // Inline squiggle spans mapped from the served diagnostics.
        let diag_ranges: Vec<(usize, usize)> = self
            .diagnostics
            .iter()
            .filter_map(|(l, c, _)| line_col_to_span(&self.script_buf, *l, *c))
            .collect();

        // Status + first problem, above the editor so the editor fills height.
        ui.horizontal_wrapped(|ui| {
            let n = self.diagnostics.len() + usize::from(self.compile_error.is_some());
            let ok = egui::Color32::from_rgb(166, 227, 161);
            let err = egui::Color32::from_rgb(243, 139, 168);
            let status = match (&self.script_dirty_at, &self.script_push) {
                (_, Some(_)) => egui::RichText::new("linting…").small().weak(),
                (Some(_), _) => egui::RichText::new("editing…").small().weak(),
                _ if n == 0 => egui::RichText::new("no problems").small().color(ok),
                _ => egui::RichText::new(format!("{n} problem(s)")).small().color(err),
            };
            ui.label(status);
            if let Some((l, c, msg)) = self.diagnostics.first() {
                ui.label(egui::RichText::new(format!("· {l}:{c} {msg}")).small().color(err));
            } else if let Some(e) = &self.compile_error {
                ui.label(egui::RichText::new(format!("· {e}")).small().color(err));
            }
        });

        // The editor fills ~99% of the remaining height; the completion bar,
        // when present, reserves a thin strip below it.
        let has_suggestions = !self.completion.is_empty();
        let editor_id = egui::Id::new("wirelab-script-editor");
        let base = ui.visuals().text_color();
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let mut job = highlight_rhai(buf.as_str(), base, &diag_ranges);
            job.wrap.max_width = wrap;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let reserve = if has_suggestions { 34.0 } else { 0.0 };
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace).max(1.0);
        let avail = (ui.available_height() * 0.99 - reserve).max(row_h * 4.0);
        let rows = ((avail / row_h).floor() as usize).max(4);
        let out = egui::TextEdit::multiline(&mut self.script_buf)
            .id(editor_id)
            .code_editor()
            .desired_width(f32::INFINITY)
            .desired_rows(rows)
            .layouter(&mut layouter)
            .show(ui);

        // Touch-first completion bar (tap a candidate to insert it).
        if has_suggestions {
            let mut accept = None;
            egui::ScrollArea::horizontal().id_salt("script-suggest").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, item) in self.completion.iter().enumerate() {
                        if ui
                            .small_button(&item.label)
                            .on_hover_text(&item.detail)
                            .clicked()
                        {
                            accept = Some(i);
                        }
                    }
                });
            });
            if let Some(i) = accept {
                self.apply_completion(ui, editor_id, i);
            }
        }

        // Recompute candidates when the cursor or buffer moved.
        let focused = out.response.has_focus();
        if focused && let Some(cr) = out.cursor_range {
            use std::hash::{Hash, Hasher};
            let ci = cr.primary.index.0;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            self.script_buf.hash(&mut h);
            let key = (ci, h.finish());
            if key != self.completion_key {
                self.completion_key = key;
                let names = self.script_comp_names();
                match wirelab_core::script_api::completions(&self.script_buf, ci, &names) {
                    Some(c) => {
                        self.completion = c.items;
                        self.completion_ws = c.word_start;
                    }
                    None => self.completion.clear(),
                }
            }
        } else if !focused {
            self.completion.clear();
        }

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
    }

    /// Component names for the active board, matching the desktop's naming.
    fn script_comp_names(&self) -> Vec<String> {
        let Some(snap) = &self.snapshot else { return Vec::new() };
        let Some(tab) = snap.boards.get(self.selected_board) else { return Vec::new() };
        let mut lib = wirelab_core::library::Library::default();
        for def in snap.defs.values() {
            lib.add_component(def.clone());
        }
        wirelab_core::script::component_names(&tab.circuit, &lib).into_values().collect()
    }

    /// Insert the chosen completion, replacing the in-progress word.
    fn apply_completion(&mut self, ui: &egui::Ui, editor_id: egui::Id, idx: usize) {
        let Some(item) = self.completion.get(idx).cloned() else { return };
        let ws = self.completion_ws;
        let ci = self.completion_key.0.min(self.script_buf.chars().count());
        let sb = char_to_byte(&self.script_buf, ws);
        let eb = char_to_byte(&self.script_buf, ci);
        if sb <= eb && eb <= self.script_buf.len() {
            self.script_buf.replace_range(sb..eb, &item.insert);
            let caret = ws + item.insert.chars().count() - item.back;
            if let Some(mut st) = egui::text_edit::TextEditState::load(ui.ctx(), editor_id) {
                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(caret),
                )));
                st.store(ui.ctx(), editor_id);
            }
            ui.ctx().memory_mut(|m| m.request_focus(editor_id));
        }
        self.completion.clear();
        self.completion_key = (usize::MAX, 0);
    }
}
