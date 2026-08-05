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
use wirelab_core::engine::Bindings;
use wirelab_core::geometry;
use wirelab_core::library::Library;
use wirelab_core::netlist::Netlist;
use wirelab_core::project::BoardTab;
use wirelab_core::validate::{Lint, Severity};
use wirelab_flow_ui::{FlowView, FlowViewer, ViewerOptions, build_snarl};

use crate::link::Ops;
use crate::theme;

/// Closest the circuit canvas's dot grid is allowed to get on screen, in points.
const GRID_MIN_PX: f32 = 20.0;

/// Everything derivable from one board's snapshot: the reconstructed library,
/// its netlist/bindings, and the static lints. Rebuilt when a snapshot lands
/// or the board selection moves.
pub struct BoardModel {
    pub lib: Library,
    pub board: BoardProfile,
    pub netlist: Netlist,
    pub bindings: Bindings,
    pub lints: Vec<Lint>,
    /// Content keys so the live runner resyncs on real changes, not on the
    /// 3-second snapshot refresh: wiring/topology (scripts blanked), the
    /// script sources alone, and the flow graph.
    pub setup_key: u64,
    pub scripts_key: u64,
    pub flow_key: u64,
}

fn content_key<T: serde::Serialize>(seed: u64, value: &T) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    seed.hash(&mut h);
    serde_json::to_string(value).unwrap_or_default().hash(&mut h);
    h.finish()
}

impl BoardModel {
    pub fn build(snap: &Snapshot, board_idx: usize) -> Option<BoardModel> {
        let tab = snap.boards.get(board_idx)?;
        let board = snap.profiles.get(&tab.circuit.board_id)?.clone();
        let mut lib = Library::default();
        for b in snap.profiles.values() {
            lib.add_board(b.clone());
        }
        for def in snap.defs.values() {
            lib.add_component(def.clone());
        }
        let netlist = Netlist::build(&tab.circuit, &board, &lib);
        let (_, bindings) = wirelab_core::engine::plan_setup(&tab.circuit, &board, &lib, &netlist);
        let lints = wirelab_core::validate::validate(&tab.circuit, &board, &lib, &netlist);
        // True topology only: positions, labels, props and pokes must not
        // re-run pin setup mid-session (plan_setup ignores them all).
        let topo: (Vec<(u32, &str)>, Vec<(u32, &Endpoint, &Endpoint)>) = (
            tab.circuit.components.iter().map(|(id, c)| (id.0, c.def_id.as_str())).collect(),
            tab.circuit.wires.iter().map(|(id, w)| (id.0, &w.a, &w.b)).collect(),
        );
        // Labels ride along: renames change the script-visible names.
        let scripts: Vec<(u32, &str, &Option<String>)> = tab
            .circuit
            .components
            .iter()
            .map(|(id, c)| (id.0, c.label.as_str(), &c.script))
            .collect();
        Some(BoardModel {
            lib,
            board,
            netlist,
            bindings,
            lints,
            setup_key: content_key(tab.id, &topo),
            scripts_key: content_key(tab.id, &scripts),
            flow_key: content_key(tab.id, &tab.flow),
        })
    }

    /// Problem counts as (errors, warnings + binding warnings, infos).
    pub fn problem_counts(&self) -> (usize, usize, usize) {
        let mut n = (0, self.bindings.warnings.len(), 0);
        for l in &self.lints {
            match l.severity {
                Severity::Error => n.0 += 1,
                Severity::Warning => n.1 += 1,
                Severity::Info => n.2 += 1,
            }
        }
        n
    }
}

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
    /// Pushed flow with the board it was posted for — the selection may have
    /// moved by the time the response lands.
    Flow { board_id: u64, graph: wirelab_core::flow::FlowGraph },
    Moves,
    /// A structural edit (add/remove component or wire); refetch on success.
    Edit,
    /// A whole local project offered to the desktop; parked there for confirmation.
    Import,
}

/// The `from` label the desktop names this device by when it announces an import.
const IMPORT_FROM: &str = "the WireLab mobile app";

/// The `POST /project/import` body: the `{name, active, boards}` the desktop's
/// `parse_import_push` requires, plus the `from` label it announces the offer under.
pub fn import_body(name: &str, active: usize, boards: &[BoardTab]) -> String {
    serde_json::json!({
        "name": name,
        "active": active,
        "boards": boards,
        "from": IMPORT_FROM,
    })
    .to_string()
}

/// What's selected on the canvas (for highlight + delete).
#[derive(Clone, Copy, PartialEq)]
enum CanvasSel {
    Comp(u32),
    Wire(u32),
}

/// Where the shown project lives: the desktop over HTTP, or this device.
#[derive(Clone, PartialEq)]
pub enum Source {
    Desktop,
    Local(String),
}

/// Open modal over the project picker.
#[derive(Clone, Copy, PartialEq)]
enum Dialog {
    None,
    New,
    Rename,
    Delete,
}

/// An in-progress touch drag, decided at drag-start by what's under the finger.
enum CanvasDrag {
    /// Dragging a component body to reposition it.
    Move { id: CompId, grab: [f32; 2] },
    /// Dragging from a pin/terminal to run a wire.
    Wire { from: Endpoint },
    /// Dragging empty canvas to pan the view.
    Pan,
}

/// Poll-driven fetcher + the fetched project.
pub struct ProjectView {
    pub desktop_addr: String,
    fetch: Fetch,
    pub snapshot: Option<Snapshot>,
    pub selected_board: usize,
    /// Rebuilt when a new snapshot lands.
    flow_view: FlowView,
    /// View state for the flow canvas (pending fit/centre, last transform, node bounds).
    flow_canvas: crate::flowstyle::Canvas,
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
    /// Script editor: which component (on which board), the buffer, and
    /// diagnostics served by the desktop linter.
    script_comp: Option<u32>,
    script_board: Option<u64>,
    script_buf: String,
    script_synced: String,
    script_dirty_at: Option<f64>,
    /// In-flight script push: (request id, board id, comp).
    script_push: Option<(u64, u64, u32)>,
    diagnostics: Vec<(u32, u32, String)>,
    compile_error: Option<String>,
    /// Autocomplete popup: candidates + the char index the word started at,
    /// recomputed only when (cursor, buffer-hash) moves.
    completion: Vec<wirelab_core::script_api::CompletionItem>,
    completion_ws: usize,
    completion_key: (usize, u64),
    /// Passive listener for the desktop's own discovery beacon (UDP 4521).
    desktop_scan: DesktopScan,
    /// Derived per-board model, rebuilt when (snapshot, board) moves.
    model: Option<BoardModel>,
    /// Bumped on every snapshot land; keys the model cache with the board index.
    snap_rev: u64,
    model_for: (u64, usize),
    /// Checks overlay visibility.
    checks_open: bool,
    /// Warnings from the live runner's solve, folded into Checks.
    pub live_warnings: Vec<String>,
    /// Transient toolbar notice (message, shown-at).
    notice: Option<(String, f64)>,
    /// Auto-wire proposal awaiting confirmation.
    autowire_plan: Option<wirelab_core::autowire::AutoWirePlan>,
    /// Confirmed auto-wire hookups (for `pending_board`), posted one at a time.
    pending_wires: Vec<(Endpoint, Endpoint)>,
    pending_board: u64,
    /// Fit transform frozen at drag start, so a moving component can't
    /// reshape the auto-fit bbox under the finger.
    drag_view: Option<([f32; 2], f32)>,
    /// Expanded problems list under the script status row.
    problems_open: bool,
    /// Circuit-canvas view: pan offset in mm and zoom over the fitted scale.
    pan: [f32; 2],
    zoom: f32,
    /// A two-finger gesture owns the canvas: holds the frozen fit and pauses refreshes.
    pinching: bool,
    /// Whether the shown project is the desktop's or one of this device's.
    pub source: Source,
    /// Board profiles + component defs, loaded once on first use.
    lib: Library,
    lib_loaded: bool,
    /// Projects created on this device.
    store: crate::store::Store,
    store_loaded: bool,
    /// A local edit awaiting the debounced write-back to the store.
    local_dirty: bool,
    local_dirty_at: Option<f64>,
    /// Project picker modal state and its text/board fields.
    dialog: Dialog,
    dialog_name: String,
    dialog_board: String,
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
pub(crate) fn highlight_rhai(
    text: &str,
    base: egui::Color32,
    diag: &[(usize, usize)],
) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let comment = egui::Color32::from_gray(110);
    let string = theme::AQUA;
    let number = egui::Color32::from_rgb(110, 170, 255);
    let keyword = theme::VIOLET;
    let font = egui::FontId::monospace(13.0);
    let mut job = LayoutJob::default();
    let underlined = |s: usize, e: usize| diag.iter().any(|(ds, de)| s < *de && *ds < e);
    let mut push = |slice: &str, s: usize, color: egui::Color32| {
        let mut fmt = TextFormat::simple(font.clone(), color);
        if underlined(s, s + slice.len()) {
            fmt.underline = egui::Stroke::new(1.5, theme::PINK);
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
            flow_canvas: crate::flowstyle::Canvas::default(),
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
            script_board: None,
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
            model: None,
            snap_rev: 0,
            model_for: (u64::MAX, usize::MAX),
            checks_open: false,
            live_warnings: Vec::new(),
            notice: None,
            autowire_plan: None,
            pending_wires: Vec::new(),
            pending_board: 0,
            drag_view: None,
            problems_open: false,
            pan: [0.0, 0.0],
            zoom: 1.0,
            pinching: false,
            source: Source::Desktop,
            lib: Library::default(),
            lib_loaded: false,
            store: Default::default(),
            store_loaded: false,
            local_dirty: false,
            local_dirty_at: None,
            dialog: Dialog::None,
            dialog_name: String::new(),
            dialog_board: String::new(),
        }
    }
}

impl ProjectView {
    /// True when the shown project lives on this device.
    pub fn is_local(&self) -> bool {
        matches!(self.source, Source::Local(_))
    }

    fn ensure_lib(&mut self, ops: &dyn Ops) {
        if !self.lib_loaded {
            self.lib_loaded = true;
            self.lib = crate::library::load(ops);
        }
    }

    /// Load the project store once and reopen whatever was last open.
    fn ensure_store(&mut self, ops: &dyn Ops, now: f64) {
        if self.store_loaded {
            return;
        }
        self.store_loaded = true;
        self.store = crate::store::Store::load(ops);
        if let Some(id) = self.store.active.clone()
            && self.store.get(&id).is_some()
        {
            self.open_local(ops, now, &id);
        }
    }

    /// Show a stored project, rehydrating its board/component library.
    pub fn open_local(&mut self, ops: &dyn Ops, now: f64, id: &str) {
        self.ensure_lib(ops);
        if self.local_dirty {
            self.flush_local(ops, now);
        }
        let Some(p) = self.store.get(id).cloned() else { return };
        let snap = Snapshot {
            name: p.name,
            active: p.active,
            boards: p.boards,
            profiles: self.lib.boards.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            defs: self.lib.components.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            flow_bases: HashMap::new(),
        };
        self.source = Source::Local(id.to_string());
        self.selected_board = snap.active.min(snap.boards.len().saturating_sub(1));
        self.snapshot = Some(snap);
        self.reset_editors();
        self.store.active = Some(id.to_string());
        self.store.save(ops);
    }

    /// Drop the local project and go back to the desktop source.
    fn close_local(&mut self, ops: &dyn Ops, now: f64) {
        if self.local_dirty {
            self.flush_local(ops, now);
        }
        self.source = Source::Desktop;
        self.snapshot = None;
        self.reset_editors();
        self.store.active = None;
        self.store.save(ops);
        self.start_fetch(ops, now);
    }

    /// Clear every editor's in-flight and per-project state.
    fn reset_editors(&mut self) {
        self.fetch = Fetch::Idle;
        self.push = None;
        self.script_push = None;
        self.conflict = None;
        self.drag = None;
        self.moved.clear();
        self.armed_def = None;
        self.sel = None;
        self.needs_refetch = false;
        self.flow_built_for = usize::MAX;
        self.flow_dirty_at = None;
        self.script_comp = None;
        self.script_board = None;
        self.script_buf.clear();
        self.script_synced.clear();
        self.script_dirty_at = None;
        self.diagnostics.clear();
        self.compile_error = None;
        self.autowire_plan = None;
        self.pending_wires.clear();
        self.local_dirty = false;
        self.local_dirty_at = None;
        self.snap_rev += 1;
    }

    /// Mark the open local project changed; the store write is debounced.
    fn touch_local(&mut self) {
        self.local_dirty = true;
    }

    /// Write the open local project back to the store.
    fn flush_local(&mut self, ops: &dyn Ops, now: f64) {
        self.local_dirty = false;
        self.local_dirty_at = None;
        let Source::Local(id) = self.source.clone() else { return };
        let Some(snap) = &self.snapshot else { return };
        self.store.put(&id, &snap.name, self.selected_board, &snap.boards, (now * 1000.0) as u64);
        self.store.save(ops);
    }

    /// Apply a structural edit in-process, with the desktop's wire verdict.
    fn edit_local(&mut self, board_id: u64, op: serde_json::Value) {
        let op: wirelab_core::edit::EditOp = match serde_json::from_value(op) {
            Ok(op) => op,
            Err(e) => {
                self.conflict = Some(format!("bad edit: {e}"));
                return;
            }
        };
        let outs: Vec<u8> = self
            .model
            .as_ref()
            .map(|m| m.bindings.outputs.values().map(|b| b.gpio).collect())
            .unwrap_or_default();
        let result = {
            let ProjectView { snapshot, lib, .. } = self;
            let Some(tab) =
                snapshot.as_mut().and_then(|s| s.boards.iter_mut().find(|b| b.id == board_id))
            else {
                return;
            };
            wirelab_core::edit::apply_edit(&mut tab.circuit, lib, &outs, op)
        };
        match result {
            Ok(_) => {
                self.drag = None;
                self.sel = None;
                self.snap_rev += 1;
                self.touch_local();
            }
            Err(e) => self.conflict = Some(e),
        }
    }

    /// Replace a board's rules (local projects own their program).
    pub fn set_program(&mut self, board_id: u64, program: wirelab_core::program::Program) {
        if let Some(snap) = &mut self.snapshot
            && let Some(tab) = snap.boards.iter_mut().find(|b| b.id == board_id)
        {
            tab.program = program;
        }
        self.snap_rev += 1;
        self.touch_local();
    }

    /// Append a board to the open local project and select it.
    fn add_local_board(&mut self) {
        let Some(snap) = &mut self.snapshot else { return };
        let board_id = match snap.boards.get(self.selected_board) {
            Some(b) => b.circuit.board_id.clone(),
            None => return,
        };
        let id = snap.boards.iter().map(|b| b.id).max().unwrap_or(0) + 1;
        let name = format!("Board {}", snap.boards.len() + 1);
        snap.boards.push(BoardTab {
            id,
            name,
            circuit: wirelab_core::circuit::Circuit::new(&board_id),
            program: Default::default(),
            flow: Default::default(),
        });
        self.selected_board = snap.boards.len() - 1;
        self.flow_built_for = usize::MAX;
        self.snap_rev += 1;
        self.touch_local();
    }

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
        self.ensure_store(ops, now);
        if self.is_local() {
            // A local project's only outbound push is the project offered to the desktop.
            self.poll_push(ops, now);
            self.desktop_scan.poll(ops, now);
            if self.conflict.is_some() {
                self.pending_wires.clear();
            } else if !self.pending_wires.is_empty() {
                let board_id = self.pending_board;
                let (a, b) = self.pending_wires.remove(0);
                self.edit(
                    ops,
                    board_id,
                    serde_json::json!({
                        "op": "add_wire",
                        "a": serde_json::to_value(&a).unwrap_or_default(),
                        "b": serde_json::to_value(&b).unwrap_or_default(),
                    }),
                );
            }
            if self.local_dirty && self.local_dirty_at.is_none() {
                self.local_dirty_at = Some(now);
            }
            if let Some(t0) = self.local_dirty_at
                && now - t0 > 1.0
            {
                self.flush_local(ops, now);
            }
            return;
        }
        self.poll_push(ops, now);
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
        // Confirmed auto-wire hookups drain one edit (plus refetch) at a time.
        if self.conflict.is_some() {
            self.pending_wires.clear();
        } else if !self.pending_wires.is_empty()
            && self.push.is_none()
            && !self.needs_refetch
            && !matches!(self.fetch, Fetch::Pending(_))
        {
            // Posted against the board the plan was confirmed for, however
            // the selection has moved since.
            let board_id = self.pending_board;
            let (a, b) = self.pending_wires.remove(0);
            self.edit(
                ops,
                board_id,
                serde_json::json!({
                    "op": "add_wire",
                    "a": serde_json::to_value(&a).unwrap_or_default(),
                    "b": serde_json::to_value(&b).unwrap_or_default(),
                }),
            );
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
                                    self.ensure_lib(ops);
                                    if crate::library::merge(
                                        &mut self.lib,
                                        &snap.profiles,
                                        &snap.defs,
                                    ) {
                                        crate::library::cache(ops, &self.lib);
                                    }
                                    self.selected_board =
                                        self.selected_board.min(snap.boards.len().saturating_sub(1));
                                    self.snapshot = Some(snap);
                                    self.flow_built_for = usize::MAX; // rebuild
                                    self.flow_dirty_at = None;
                                    self.conflict = None;
                                    self.snap_rev += 1;
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

    /// Drops the pinch latch when the canvas isn't the visible tab.
    pub fn clear_pinch(&mut self) {
        self.pinching = false;
    }

    /// Local edits in flight — pause refreshes so they aren't clobbered.
    fn editing(&self) -> bool {
        self.flow_dirty_at.is_some()
            || self.push.is_some()
            || self.drag.is_some()
            || self.pinching
            || self.conflict.is_some()
            || !self.moved.is_empty()
            || self.armed_def.is_some()
            || self.needs_refetch
            || self.script_dirty_at.is_some()
            || self.script_push.is_some()
            || !self.pending_wires.is_empty()
    }

    /// Rebuild the derived model if the snapshot or board selection moved.
    pub fn ensure_model(&mut self) {
        let key = (self.snap_rev, self.selected_board);
        if self.model_for != key {
            self.model_for = key;
            self.model = self.snapshot.as_ref().and_then(|s| BoardModel::build(s, key.1));
        }
    }

    /// Derived model for the selected board, rebuilt when the snapshot moves.
    pub fn model(&mut self) -> Option<&BoardModel> {
        self.ensure_model();
        self.model.as_ref()
    }

    /// Judge a candidate wire locally before posting it (desktop parity).
    fn wire_ok(&self, from: &Endpoint, to: &Endpoint) -> wirelab_core::netlist::WireVerdict {
        let Some(m) = self.model.as_ref() else {
            return wirelab_core::netlist::WireVerdict::Ok;
        };
        let outs: Vec<u8> = m.bindings.outputs.values().map(|b| b.gpio).collect();
        wirelab_core::netlist::wire_verdict(&m.netlist, &m.board, from, to, &outs)
    }

    /// The built model without rebuilding (call `ensure_model` first).
    pub fn model_ref(&self) -> Option<&BoardModel> {
        self.model.as_ref()
    }

    /// Cache key identifying the current (snapshot, board) pair.
    pub fn snap_key(&self) -> (u64, usize) {
        (self.snap_rev, self.selected_board)
    }

    pub fn board_tab(&self) -> Option<&BoardTab> {
        self.snapshot.as_ref()?.boards.get(self.selected_board)
    }

    /// Text and colour of the checks chip, or `None` when there is nothing to check.
    pub fn checks_label(&mut self) -> Option<(String, egui::Color32)> {
        let (mut err, warn, _info) = self.model().map(|m| m.problem_counts())?;
        err += self.live_warnings.len();
        Some(if err > 0 {
            (format!("✖ {err}"), theme::PINK)
        } else if warn > 0 {
            (format!("⚠ {warn}"), theme::AQUA_BRIGHT)
        } else {
            ("✔ checks".to_string(), theme::AQUA)
        })
    }

    /// Width the checks chip will take, for a row that reserves its space up front.
    /// Width the chip needs, measured from the same label the row will draw.
    pub fn checks_width(ui: &egui::Ui, chip: Option<&(String, egui::Color32)>) -> f32 {
        let Some((label, _)) = chip else { return 0.0 };
        let font = egui::TextStyle::Small.resolve(ui.style());
        let text = ui.painter().layout_no_wrap(label.clone(), font, egui::Color32::PLACEHOLDER);
        text.size().x + ui.spacing().button_padding.x * 2.0 + 2.0
    }

    /// Checks chip plus the overlay it opens: the desktop Checks panel (binding
    /// warnings + static lints), tap a row to select the first involved component.
    pub fn checks_chip(&mut self, ui: &mut egui::Ui, chip: Option<(String, egui::Color32)>) {
        let Some((label, color)) = chip else { return };
        if ui.button(egui::RichText::new(label).color(color).small()).clicked() {
            self.checks_open = !self.checks_open;
        }
        if !self.checks_open {
            return;
        }
        let screen = ui.ctx().content_rect();
        let mut still_open = true;
        let mut select: Option<u32> = None;
        let local = self.is_local();
        egui::Window::new("Checks")
            .open(&mut still_open)
            .collapsible(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(screen.center())
            .default_size([screen.width() - 32.0, (screen.height() * 0.6).min(420.0)])
            .vscroll(false)
            .show(ui.ctx(), |ui| {
                let Some(model) = self.model.as_ref() else { return };
                let err = theme::PINK;
                let warn = theme::AQUA_BRIGHT;
                let dim = theme::INK_DIM;
                theme::scroll_vertical().show(ui, |ui| {
                    for w in &self.live_warnings {
                        ui.label(egui::RichText::new(format!("✖ {w}")).color(err));
                    }
                    for w in &model.bindings.warnings {
                        ui.label(egui::RichText::new(format!("⚠ {w}")).color(warn));
                    }
                    for lint in &model.lints {
                        let (color, icon) = match lint.severity {
                            Severity::Error => (err, "✖"),
                            Severity::Warning => (warn, "⚠"),
                            Severity::Info => (dim, "•"),
                        };
                        let row = ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("{icon} {}", lint.message))
                                    .color(color),
                            )
                            .sense(egui::Sense::click()),
                        );
                        if row.clicked()
                            && let Some(c) = lint.comps.first()
                        {
                            select = Some(c.0);
                        }
                        if let Some(wirelab_core::validate::LintFix::SpliceResistor {
                            ohms, ..
                        }) = &lint.fix
                        {
                            ui.label(
                                egui::RichText::new(if local {
                                    format!("    fix: splice a ~{ohms:.0} ohm resistor in series")
                                } else {
                                    format!(
                                        "    fix on the desktop: splice a ~{ohms:.0} ohm resistor"
                                    )
                                })
                                .color(dim)
                                .small(),
                            );
                        }
                    }
                    if model.lints.is_empty() && model.bindings.warnings.is_empty() {
                        ui.label(egui::RichText::new("all clear").color(dim));
                    }
                });
            });
        if let Some(id) = select {
            self.sel = Some(CanvasSel::Comp(id));
            self.checks_open = false;
        }
        if !still_open {
            self.checks_open = false;
        }
    }

    /// Apply a structural edit `{board_id, op, ...}` — locally, or to the desktop.
    fn edit(&mut self, ops: &dyn Ops, board_id: u64, mut op: serde_json::Value) {
        if self.is_local() {
            self.edit_local(board_id, op);
            return;
        }
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
            ui.label(egui::RichText::new(err).color(theme::PINK));
            if ui.button("Dismiss").clicked() {
                self.conflict = None;
            }
            // A local project has no second writer to reload from.
            if self.is_local() {
                return;
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

    /// The desktop to talk to: the typed address, else the first one discovered.
    fn desktop_target(&self) -> Option<String> {
        let typed = self.desktop_addr.trim();
        if !typed.is_empty() {
            return Some(typed.to_string());
        }
        self.desktop_scan.desktops().next().map(|d| d.addr.clone())
    }

    /// Offer the open local project to the desktop's `/project/import`.
    fn send_to_desktop(&mut self, ops: &dyn Ops, now: f64, addr: String) {
        let body = {
            let Some(snap) = &self.snapshot else { return };
            if snap.boards.is_empty() {
                self.conflict = Some("nothing to send: this project has no boards".into());
                return;
            }
            import_body(&snap.name, self.selected_board, &snap.boards)
        };
        self.desktop_addr = addr;
        self.post(ops, "/project/import", body, Push::Import);
        self.notice = Some(("sending to the desktop…".into(), now));
    }

    fn poll_push(&mut self, ops: &dyn Ops, now: f64) {
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
            Push::Flow { board_id, graph } => {
                // Agreed: the pushed graph is the new shared truth — for the
                // board it was posted for, wherever the selection is now.
                if let (Some(snap), Some(base)) = (
                    &mut self.snapshot,
                    v.get("base").and_then(serde_json::Value::as_u64),
                ) {
                    snap.flow_bases.insert(board_id.to_string(), base);
                    if let Some(tab) = snap.boards.iter_mut().find(|b| b.id == board_id) {
                        tab.flow = graph.clone();
                    }
                }
                // Editor sync state belongs to the board on screen only.
                if self.board_tab().map(|t| t.id) == Some(board_id) {
                    self.flow_synced = graph;
                    self.flow_dirty_at = None;
                }
                // The mirrored flow changed; dependents (live runner) resync.
                self.snap_rev += 1;
            }
            Push::Moves => {}
            Push::Edit => {
                // The circuit changed structurally: pull fresh ids/geometry.
                self.drag = None;
                self.needs_refetch = true;
            }
            Push::Import => {
                // The desktop parks the project behind a confirm modal; nothing is applied yet.
                let name = v
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("the project")
                    .to_string();
                self.notice =
                    Some((format!("sent '{name}' — accept it on the desktop"), now));
            }
        }
    }

    /// Project menu: pick or edit a project, pick a board, and reach the desktop connection
    /// controls (address, Fetch, live, discovered desktops). One button in the caller's row.
    pub fn project_menu(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) {
        self.ensure_lib(ops);
        let local = self.is_local();
        let target = self.desktop_target();
        let sending = self.push.is_some();
        let boards: Vec<String> = self
            .snapshot
            .as_ref()
            .map(|s| s.boards.iter().map(|b| b.name.clone()).collect())
            .unwrap_or_default();
        let found: Vec<(String, String)> =
            self.desktop_scan.desktops().map(|d| (d.addr.clone(), d.name.clone())).collect();
        let mut title = match &self.snapshot {
            Some(s) if local => format!("📁 {}", s.name),
            Some(s) => format!("🖥 {}", s.name),
            None => "📁 no project".to_string(),
        };
        if boards.len() > 1
            && let Some(b) = boards.get(self.selected_board)
        {
            title = format!("{title} • {b}");
        }
        let mut open: Option<String> = None;
        let mut to_desktop = false;
        let mut add_board = false;
        let mut send: Option<String> = None;
        let mut fetch = false;
        let mut select = self.selected_board;
        theme::menu_button(ui, title, |ui| {
            if ui.button("New project…").clicked() {
                self.dialog = Dialog::New;
                self.dialog_name = format!("Project {}", self.store.projects.len() + 1);
                self.dialog_board = self.lib.boards.keys().next().cloned().unwrap_or_default();
                ui.close();
            }
            if local {
                if ui.button("Rename…").clicked() {
                    self.dialog = Dialog::Rename;
                    self.dialog_name =
                        self.snapshot.as_ref().map(|s| s.name.clone()).unwrap_or_default();
                    ui.close();
                }
                if ui.button("🗑 Delete…").clicked() {
                    self.dialog = Dialog::Delete;
                    ui.close();
                }
                if ui.button("Add board").clicked() {
                    add_board = true;
                    ui.close();
                }
                // Only with a desktop to send to; it lands there for confirmation.
                if let Some(addr) = &target {
                    let row = egui::Button::new("🖥 Send to desktop…");
                    if ui.add_enabled(!sending, row).clicked() {
                        send = Some(addr.clone());
                        ui.close();
                    }
                }
            }
            if boards.len() > 1 {
                ui.separator();
                for (i, b) in boards.iter().enumerate() {
                    if ui.selectable_label(i == select, b).clicked() {
                        select = i;
                        ui.close();
                    }
                }
            }
            let listing = self.store.listing();
            if !listing.is_empty() {
                ui.separator();
                for (id, name) in listing {
                    let sel = self.source == Source::Local(id.clone());
                    if ui.selectable_label(sel, format!("📁 {name}")).clicked() {
                        open = Some(id);
                        ui.close();
                    }
                }
            }
            ui.separator();
            if ui.selectable_label(!local, "🖥 Desktop project").clicked() {
                to_desktop = true;
                ui.close();
            }
            if !local {
                ui.add(
                    egui::TextEdit::singleline(&mut self.desktop_addr)
                        .hint_text("192.168.1.x (WireLab app)")
                        .desired_width(200.0),
                );
                // No ui.close(): the spinner and the discovered list stay up during a fetch.
                ui.horizontal(|ui| {
                    fetch |= ui.button("Fetch").clicked();
                    if matches!(self.fetch, Fetch::Pending(_)) {
                        ui.spinner();
                    }
                    ui.checkbox(&mut self.auto_refresh, egui::RichText::new("live").small());
                });
                for (addr, name) in &found {
                    if ui.button(format!("🖥 {name}")).clicked() {
                        self.desktop_addr = addr.clone();
                        fetch = true;
                    }
                }
            }
        });
        if let Some(addr) = send {
            self.send_to_desktop(ops, now, addr);
        }
        if add_board {
            self.add_local_board();
        }
        if fetch {
            self.start_fetch(ops, now);
        }
        if select != self.selected_board {
            self.selected_board = select;
            if local {
                self.touch_local();
            }
        }
        if let Some(id) = open {
            self.open_local(ops, now, &id);
        } else if to_desktop && local {
            self.close_local(ops, now);
        }
    }

    /// Status lines under the top row plus the project modals; returns whether a project is
    /// showing.
    pub fn header(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) -> bool {
        // Exactly one status line for every project tab, so the chrome stays within two rows:
        // sends, auto-wire and wiring refusals outrank a sticky fetch error.
        if matches!(&self.notice, Some((_, at)) if now - at >= 3.5) {
            self.notice = None;
        }
        let status = match (&self.notice, &self.fetch) {
            (Some((msg, _)), _) => Some((msg.clone(), theme::AQUA_BRIGHT)),
            (None, Fetch::Failed(e)) if !self.is_local() => Some((e.clone(), theme::PINK)),
            _ => None,
        };
        if let Some((msg, color)) = status {
            ui.label(egui::RichText::new(msg).small().color(color));
        }
        self.project_dialogs(ui, ops, now);
        if self.snapshot.is_none() && !matches!(self.fetch, Fetch::Failed(_)) {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "open the project menu above to start a project here, or to reach a \
                     WireLab desktop on your network",
                )
                .weak(),
            );
            return false;
        }
        true
    }

    /// New / Rename / Delete modals over the project picker.
    fn project_dialogs(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, now: f64) {
        if self.dialog == Dialog::None {
            return;
        }
        let screen = ui.ctx().content_rect();
        let mut close = false;
        let mut created: Option<String> = None;
        let mut deleted = false;
        let title = match self.dialog {
            Dialog::New => "New project",
            Dialog::Rename => "Rename project",
            _ => "Delete project",
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(screen.center())
            .default_width((screen.width() - 48.0).min(360.0))
            .show(ui.ctx(), |ui| match self.dialog {
                Dialog::Delete => {
                    let name =
                        self.snapshot.as_ref().map(|s| s.name.clone()).unwrap_or_default();
                    ui.label(format!("Delete '{name}' from this device? This cannot be undone."));
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            deleted = true;
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Rename => {
                    theme::card().show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.dialog_name)
                                .desired_width(f32::INFINITY),
                        );
                    });
                    ui.horizontal(|ui| {
                        let ok = !self.dialog_name.trim().is_empty();
                        if ui.add_enabled(ok, egui::Button::new("Rename")).clicked() {
                            let name = self.dialog_name.trim().to_string();
                            if let Source::Local(id) = self.source.clone() {
                                self.store.rename(&id, &name, (now * 1000.0) as u64);
                                self.store.save(ops);
                            }
                            if let Some(s) = &mut self.snapshot {
                                s.name = name;
                            }
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                _ => {
                    let current = self
                        .lib
                        .board(&self.dialog_board)
                        .map(|b| b.name.clone())
                        .unwrap_or_else(|| "pick a board".to_string());
                    let boards: Vec<(String, String)> =
                        self.lib.boards.values().map(|b| (b.id.clone(), b.name.clone())).collect();
                    theme::card().show(ui, |ui| {
                        ui.label(egui::RichText::new("name").small().weak());
                        ui.add(
                            egui::TextEdit::singleline(&mut self.dialog_name)
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("board").small().weak());
                        egui::ComboBox::from_id_salt("new-project-board")
                            .selected_text(current)
                            .width(ui.available_width())
                            .show_ui(ui, |ui| {
                                for (id, name) in boards {
                                    theme::selectable_value(ui, &mut self.dialog_board, id, name);
                                }
                            });
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let ok = !self.dialog_name.trim().is_empty()
                            && self.lib.board(&self.dialog_board).is_some();
                        if ui.add_enabled(ok, egui::Button::new("Create")).clicked() {
                            created = Some(self.store.create(
                                self.dialog_name.trim(),
                                &self.dialog_board.clone(),
                                (now * 1000.0) as u64,
                            ));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
            });
        if deleted && let Source::Local(id) = self.source.clone() {
            self.store.delete(&id);
            self.close_local(ops, now);
        }
        if let Some(id) = created {
            self.open_local(ops, now, &id);
        }
        if close {
            self.dialog = Dialog::None;
        }
    }

    // ── circuit canvas ─────────────────────────────────────────────────

    /// Point-to-segment distance in pixels, for tapping a wire.
    fn seg_dist(pt: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
        let ab = b - a;
        let len2 = ab.length_sq().max(1e-3);
        let t = ((pt - a).dot(ab) / len2).clamp(0.0, 1.0);
        pt.distance(a + ab * t)
    }

    /// Terminal role -> short pad label ('-' replaces the desktop's U+2212,
    /// which the default egui fonts can't render).
    fn terminal_short_label(role: wirelab_core::component::TerminalRole) -> &'static str {
        use wirelab_core::component::TerminalRole;
        match role {
            TerminalRole::Anode => "+",
            TerminalRole::Cathode => "-",
            TerminalRole::A | TerminalRole::EndA => "A",
            TerminalRole::B | TerminalRole::EndB => "B",
            TerminalRole::Common => "COM",
            TerminalRole::NormallyOpen => "NO",
            TerminalRole::NormallyClosed => "NC",
            TerminalRole::Wiper => "W",
            TerminalRole::Vcc => "V+",
            TerminalRole::Gnd => "G",
            TerminalRole::Signal => "SIG",
        }
    }

    /// Live pin paint, matching the desktop's mode/level palette.
    fn pin_state_color(mode: wirelab_proto::PinMode, high: bool) -> egui::Color32 {
        use wirelab_proto::PinMode;
        match mode {
            PinMode::Disabled => egui::Color32::from_gray(120),
            PinMode::Analog => egui::Color32::from_rgb(80, 160, 255),
            PinMode::Pwm => egui::Color32::from_rgb(240, 180, 40),
            m if m.is_input() => {
                if high {
                    egui::Color32::from_rgb(90, 220, 120)
                } else {
                    egui::Color32::from_rgb(50, 90, 60)
                }
            }
            _ => {
                if high {
                    egui::Color32::from_rgb(255, 90, 80)
                } else {
                    egui::Color32::from_rgb(110, 60, 55)
                }
            }
        }
    }

    /// Modeless toolbar (like the desktop): a palette to arm a part for placing, a Delete button
    /// for the current selection, and a mid-gesture hint.
    fn canvas_toolbar(&mut self, ui: &mut egui::Ui, ops: &dyn Ops, board_id: u64, now: f64) {
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
            theme::menu_button(ui, btn_text, |ui| {
                // Submenus inherit the root's close behavior from the ui stack.
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
            if let Some(c) = chosen {
                self.armed_def = Some(c);
            }
            if armed_name.is_some() && ui.button("stop").on_hover_text("stop placing").clicked() {
                self.armed_def = None;
            }

            let panned = self.pan != [0.0, 0.0] || self.zoom != 1.0;
            if panned && ui.button("Fit").on_hover_text("reset pan and zoom").clicked() {
                self.pan = [0.0, 0.0];
                self.zoom = 1.0;
            }

            // Plan hookups for every unwired part; confirm before posting.
            if ui
                .button("⚡ Auto")
                .on_hover_text("wire unconnected parts to sensible free pins")
                .clicked()
            {
                self.ensure_model();
                let plan = (|| {
                    let m = self.model.as_ref()?;
                    let tab = self.snapshot.as_ref()?.boards.get(self.selected_board)?;
                    let ids: Vec<CompId> = tab.circuit.components.keys().copied().collect();
                    Some(wirelab_core::autowire::auto_wire(&tab.circuit, &m.board, &m.lib, &ids))
                })();
                match plan {
                    Some(p) if !p.wires.is_empty() => self.autowire_plan = Some(p),
                    _ => self.notice = Some(("auto-wire: nothing to do".into(), now)),
                }
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
                // Only the mid-gesture states: the idle sentence overflowed the row and ran
                // under the buttons (right_to_left never wraps and inherits the parent clip).
                let hint = if self.armed_def.is_some() {
                    Some("tap the board to place")
                } else if matches!(self.drag, Some(CanvasDrag::Wire { .. })) {
                    Some("release on a pin/terminal")
                } else {
                    None
                };
                if let Some(hint) = hint {
                    ui.label(egui::RichText::new(hint).small().weak());
                }
            });
        });
    }

    pub fn show_canvas(
        &mut self,
        ui: &mut egui::Ui,
        ops: &dyn Ops,
        now: f64,
        live: Option<&wirelab_core::sim::SimOutput>,
        bank: Option<&wirelab_core::sim::PinBank>,
    ) {
        self.conflict_banner(ui, ops, now);
        let toolbar_board = self
            .snapshot
            .as_ref()
            .and_then(|s| s.boards.get(self.selected_board))
            .map(|t| t.id);
        if let Some(bid) = toolbar_board {
            self.canvas_toolbar(ui, ops, bid, now);
        }

        // Auto-wire proposal: the plan's notes, confirmed or dismissed.
        if let Some(plan) = &self.autowire_plan {
            let screen = ui.ctx().content_rect();
            let (mut confirm, mut cancel) = (false, false);
            egui::Window::new("⚡ Auto wire")
                .collapsible(false)
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(screen.center())
                .default_size([screen.width() - 40.0, (screen.height() * 0.5).min(360.0)])
                .vscroll(false)
                .show(ui.ctx(), |ui| {
                    theme::scroll_vertical().max_height(240.0).show(ui, |ui| {
                        for note in &plan.notes {
                            ui.label(egui::RichText::new(note).monospace().small());
                        }
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        confirm = ui
                            .button(format!("Wire {} connection(s)", plan.wires.len()))
                            .clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if confirm {
                if let Some(plan) = self.autowire_plan.take() {
                    self.pending_wires = plan.wires;
                    self.pending_board = toolbar_board.unwrap_or(0);
                }
            } else if cancel {
                self.autowire_plan = None;
            }
        }

        // Owned interaction data collected during the draw pass, so mutation
        // below doesn't fight the read borrow of the snapshot.
        let mut endpoints: Vec<(Endpoint, egui::Pos2)> = Vec::new();
        let mut comp_bodies: Vec<(CompId, [f32; 2], [f32; 2])> = Vec::new();
        let mut wire_segs: Vec<(WireId, egui::Pos2, egui::Pos2)> = Vec::new();

        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 8.0, theme::CANVAS);
        // Same lit page the flow canvas stands on, so the two read as one surface.
        theme::ambience(&p, rect, 3);

        // Read before the early returns below: a latched `pinching` would freeze the fit and
        // pause snapshot refreshes for the rest of the session.
        let multi = ui.input(|i| i.multi_touch());
        let was_pinching = std::mem::replace(&mut self.pinching, multi.is_some());

        let (min, fit, scale, origin, board_id, band_route) = {
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
            let fit = ((rect.width() - 24.0) / span.0)
                .min((rect.height() - 24.0) / span.1)
                .clamp(0.5, 12.0);
            // A drag or pinch must not reshape the auto-fit under the finger.
            let (min, fit) = if self.drag.is_some() || self.pinching {
                *self.drag_view.get_or_insert((min, fit))
            } else {
                self.drag_view = None;
                (min, fit)
            };
            let scale = (fit * self.zoom).clamp(0.05, 40.0);
            let origin = rect.min + egui::vec2(12.0, 12.0);
            let pan = self.pan;
            let to_px = |w: [f32; 2]| {
                origin
                    + egui::vec2((w[0] - min[0] - pan[0]) * scale, (w[1] - min[1] - pan[1]) * scale)
            };

            // Dot grid (5 mm), anchored in world space so it pans with the view. Coarsened by
            // powers of two so on-screen density — and the dot count — stays bounded at every
            // zoom instead of spiking just before the grid cuts out.
            let mut mm = 5.0;
            while mm * scale < GRID_MIN_PX && mm < 5_000.0 {
                mm *= 2.0;
            }
            let step = mm * scale;
            let cols = (rect.width() / step).ceil();
            let rows = (rect.height() / step).ceil();
            // Backstop against a degenerate scale.
            if step > 0.5 && cols * rows <= 8_000.0 {
                let dot = crate::flowstyle::DOT_COLOR;
                let phase = to_px([0.0, 0.0]) - rect.min;
                let mut x = rect.min.x + phase.x.rem_euclid(step);
                while x < rect.max.x {
                    let mut y = rect.min.y + phase.y.rem_euclid(step);
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
            p.rect_filled(board_rect, 4.0, theme::SURFACE);
            p.rect_stroke(
                board_rect,
                4.0,
                egui::Stroke::new(1.0, theme::RIM_BRIGHT),
                egui::StrokeKind::Middle,
            );
            p.text(
                board_rect.center_top() + egui::vec2(0.0, 8.0),
                egui::Align2::CENTER_TOP,
                &profile.name,
                egui::FontId::proportional(10.0),
                theme::VIOLET,
            );
            // The on-board WS2812, glowing its last commanded color.
            if let Some([r, g, b]) = live.and_then(|o| o.rgb) {
                let at = board_rect.right_top() + egui::vec2(-9.0, 9.0);
                let col = egui::Color32::from_rgb(r.max(20), g.max(20), b.max(20));
                p.circle_filled(at, 4.5, col.gamma_multiply(0.4));
                p.circle_filled(at, 2.5, col);
            }

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
            // Net potential per wire while live, for the energize glow.
            let net_mv = |ep: &Endpoint| -> Option<f32> {
                let m = self.model.as_ref()?;
                let lo = live?;
                let net = m.netlist.net_of(ep)?;
                lo.net_avg_mv.get(net).copied().flatten()
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
                    let glow = net_mv(&w.a)
                        .filter(|mv| *mv > 150.0)
                        .map(|mv| (mv / 3300.0).clamp(0.15, 0.8));
                    for seg in pts.windows(2) {
                        let s0 = egui::pos2(seg[0][0], seg[0][1]);
                        let s1 = egui::pos2(seg[1][0], seg[1][1]);
                        if let Some(alpha) = glow {
                            p.line_segment(
                                [s0, s1],
                                egui::Stroke::new(4.5, color.gamma_multiply(alpha * 0.5)),
                            );
                        }
                        p.line_segment([s0, s1], egui::Stroke::new(1.5, color));
                        // Every segment is a delete hit-target.
                        wire_segs.push((*id, s0, s1));
                    }
                }
            }

            // Board pins (also tappable endpoints).
            for pin in &profile.pins {
                let w = geometry::board_pin_world_pos(profile, pin, tab.circuit.board_pos);
                // Live GPIOs recolor by mode/level; the rest keep the kind palette.
                let live_pin = pin.kind.gpio().zip(bank).map(|(g, bank)| {
                    let d = bank.get(g);
                    let high = live
                        .and_then(|o| o.digital.get(&g).copied())
                        .unwrap_or(d.out_high);
                    (g, Self::pin_state_color(d.mode, high))
                });
                let color = match (&pin.kind, live_pin) {
                    (_, Some((_, c))) => c,
                    (wirelab_core::board::PinKind::Gnd, _) => egui::Color32::from_gray(90),
                    (wirelab_core::board::PinKind::V3_3, _) => egui::Color32::from_rgb(220, 90, 90),
                    (wirelab_core::board::PinKind::V5, _) => egui::Color32::from_rgb(240, 160, 60),
                    _ => egui::Color32::from_gray(160),
                };
                let px = to_px(w);
                p.circle_filled(px, (1.1 * scale).clamp(1.5, 4.0), color);
                // Millivolt readout beside watched analog pins.
                if scale >= 2.5
                    && let Some((g, _)) = live_pin
                    && let Some(mv) = live.and_then(|o| o.analog_mv.get(&g))
                {
                    p.text(
                        px + egui::vec2(0.0, 7.0),
                        egui::Align2::CENTER_TOP,
                        format!("{mv} mV"),
                        egui::FontId::proportional((scale * 2.2).clamp(6.5, 13.0)),
                        egui::Color32::from_rgb(80, 160, 255),
                    );
                }
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
                        egui::FontId::proportional((scale * 2.4).clamp(7.0, 13.0)),
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
                // Live part states from the solve: LED glow, activity outline,
                // servo angle, LCD contents.
                if let Some(vis) = live.and_then(|o| o.visuals.get(&c.id)) {
                    use wirelab_core::component::VisualState;
                    let full = egui::Color32::from_rgb(
                        def.visual.color[0],
                        def.visual.color[1],
                        def.visual.color[2],
                    );
                    match vis {
                        VisualState::LedBrightness(b) if *b > 0.02 => {
                            let rr = (r.width().min(r.height()) * 0.4).max(3.0);
                            p.circle_filled(
                                r.center(),
                                rr + 2.5,
                                full.gamma_multiply(0.35 * b),
                            );
                            p.circle_filled(
                                r.center(),
                                rr,
                                full.gamma_multiply(0.3 + 0.7 * b),
                            );
                        }
                        VisualState::RelayClosed(true) | VisualState::BuzzerOn { .. } => {
                            p.rect_stroke(
                                r.expand(2.0),
                                3.0,
                                egui::Stroke::new(
                                    1.5,
                                    egui::Color32::from_rgb(249, 226, 175),
                                ),
                                egui::StrokeKind::Middle,
                            );
                        }
                        VisualState::ServoAngle(a) => {
                            p.text(
                                r.center_bottom() + egui::vec2(0.0, 3.0),
                                egui::Align2::CENTER_TOP,
                                format!("{a:.0}°"),
                                egui::FontId::proportional((scale * 2.5).clamp(7.5, 13.0)),
                                egui::Color32::from_gray(180),
                            );
                        }
                        _ => {}
                    }
                }
                if c.def_id.starts_with("st7735")
                    && let Some(ops) = live.and_then(|o| o.lcd.as_ref())
                {
                    use wirelab_core::sim::LcdOp;
                    let lp = ui.painter_at(r.shrink(1.0));
                    let s = r.shrink(1.0).width().max(1.0) / 128.0;
                    let o = r.shrink(1.0).min;
                    lp.rect_filled(r.shrink(1.0), 1.0, egui::Color32::BLACK);
                    for op in ops {
                        match op {
                            LcdOp::Clear(rgb) => {
                                lp.rect_filled(
                                    r.shrink(1.0),
                                    1.0,
                                    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
                                );
                            }
                            LcdOp::Rect { x, y, w, h, rgb } => {
                                lp.rect_filled(
                                    egui::Rect::from_min_size(
                                        o + egui::vec2(f32::from(*x) * s, f32::from(*y) * s),
                                        egui::vec2(f32::from(*w) * s, f32::from(*h) * s),
                                    ),
                                    0.0,
                                    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
                                );
                            }
                            LcdOp::Text { x, y, rgb, text } => {
                                let _ = lp.text(
                                    o + egui::vec2(f32::from(*x) * s, f32::from(*y) * s),
                                    egui::Align2::LEFT_TOP,
                                    text,
                                    egui::FontId::monospace(8.0 * s.max(0.5)),
                                    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
                                );
                            }
                        }
                    }
                }
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
                // the desktop; 2-3 pin parts get role labels (LED polarity!).
                let label_terms = scale >= 3.0;
                let by_id = def.terminals.len() > 3;
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
                        let label: &str =
                            if by_id { &t.id } else { Self::terminal_short_label(t.role) };
                        p.text(
                            tpx + out,
                            align,
                            label,
                            egui::FontId::proportional((scale * 2.2).clamp(6.5, 12.0)),
                            egui::Color32::from_gray(130),
                        );
                    }
                    endpoints.push((
                        Endpoint::Terminal { comp: c.id, terminal: t.id.clone() },
                        tpx,
                    ));
                }
            }
            // Routing hints for the in-progress wire's rubber band.
            let band_route = match &self.drag {
                Some(CanvasDrag::Wire { from }) => Some((exit_dir(from), clearance(from))),
                _ => None,
            };
            (min, fit, scale, origin, board_id, band_route)
        };

        // Selection highlight.
        let sel_px = |w: [f32; 2]| {
            origin
                + egui::vec2(
                    (w[0] - min[0] - self.pan[0]) * scale,
                    (w[1] - min[1] - self.pan[1]) * scale,
                )
        };
        match self.sel {
            Some(CanvasSel::Comp(id)) => {
                if let Some((_, c, h)) = comp_bodies.iter().find(|(i, _, _)| i.0 == id) {
                    let r = egui::Rect::from_two_pos(
                        sel_px([c[0] - h[0], c[1] - h[1]]),
                        sel_px([c[0] + h[0], c[1] + h[1]]),
                    )
                    .expand(3.0);
                    p.rect_stroke(
                        r,
                        4.0,
                        egui::Stroke::new(2.0, theme::PINK),
                        egui::StrokeKind::Middle,
                    );
                }
            }
            Some(CanvasSel::Wire(id)) => {
                for (wid, s0, s1) in &wire_segs {
                    if wid.0 == id {
                        p.line_segment(
                            [*s0, *s1],
                            egui::Stroke::new(3.5, theme::PINK),
                        );
                    }
                }
            }
            None => {}
        }

        // While pulling a wire, ring every endpoint and rubber-band to the finger.
        let pan = self.pan;
        let to_world = |px: egui::Pos2| {
            [
                (px.x - origin.x) / scale + min[0] + pan[0],
                (px.y - origin.y) / scale + min[1] + pan[1],
            ]
        };

        // A second finger takes the canvas from any pan/move/wire drag, so the pinch owns the
        // gesture instead of being discarded by it. A Move flushes its position first, or the
        // next snapshot reverts it.
        if multi.is_some()
            && let Some(drag) = self.drag.take()
            && let CanvasDrag::Move { id, grab } = drag
            && let Some(ptr) = response.interact_pointer_pos()
        {
            let wp = to_world(ptr);
            self.moved.push((id.0, [wp[0] - grab[0], wp[1] - grab[1]]));
        }
        // Lifting one finger of a pinch leaves egui's dragged id unchanged, so drag_started()
        // never fires again — re-arm a pan or the canvas is inert until every finger is up.
        if was_pinching && multi.is_none() && response.dragged() {
            self.drag = Some(CanvasDrag::Pan);
        }

        // Pinch zoom anchored on the gesture centroid, so the spot you pinch stays put. Zoom is
        // clamped so the effective scale never hits its outer clamp — a clamped product would
        // break the anchor math and drift.
        let (zd, anchor) = match &multi {
            Some(m) => (m.zoom_delta, Some(m.center_pos)),
            None => (ui.input(|i| i.zoom_delta()), response.hover_pos()),
        };
        if let Some(ptr) = anchor {
            if (zd - 1.0).abs() > 1e-4 {
                let w = to_world(ptr);
                self.zoom = (self.zoom * zd).clamp((0.05 / fit).max(0.05), (40.0 / fit).min(10.0));
                let s = (fit * self.zoom).clamp(0.05, 40.0);
                self.pan[0] = w[0] - (ptr.x - origin.x) / s - min[0];
                self.pan[1] = w[1] - (ptr.y - origin.y) / s - min[1];
            }
            // Two-finger translation pans whether or not the pinch also scaled.
            if let Some(m) = &multi {
                self.pan[0] -= m.translation_delta.x / scale;
                self.pan[1] -= m.translation_delta.y / scale;
            }
        }
        if let Some(CanvasDrag::Wire { from }) = &self.drag {
            // Every candidate endpoint carries its verdict: ring = ok,
            // crossed disc = blocked short, nothing = redundant.
            use wirelab_core::netlist::WireVerdict;
            for (ep, px) in &endpoints {
                match self.wire_ok(from, ep) {
                    WireVerdict::Ok => {
                        p.circle_stroke(*px, 3.5, egui::Stroke::new(1.0, theme::AQUA));
                    }
                    WireVerdict::Blocked(_) => {
                        p.circle_filled(*px, 4.0, egui::Color32::from_black_alpha(200));
                        let s = egui::Stroke::new(1.3, theme::PINK);
                        p.line_segment([*px + egui::vec2(-3.0, -3.0), *px + egui::vec2(3.0, 3.0)], s);
                        p.line_segment([*px + egui::vec2(-3.0, 3.0), *px + egui::vec2(3.0, -3.0)], s);
                    }
                    WireVerdict::Redundant => {}
                }
            }
            if let (Some(src), Some(ptr)) = (
                endpoints.iter().find(|(ep, _)| ep == from).map(|(_, px)| *px),
                response.interact_pointer_pos(),
            ) {
                // The real routed path (exit stub honored), like the desktop.
                let (exit_a, clear_a) = band_route.unwrap_or(([0.0, 0.0], 0.0));
                let route = geometry::Route {
                    exit_a,
                    exit_b: [0.0, 0.0],
                    lane: 0,
                    stub: (3.0 * scale).clamp(8.0, 20.0),
                    clear_a,
                    clear_b: 0.0,
                };
                let pts = geometry::wire_path([src.x, src.y], [ptr.x, ptr.y], &route);
                for seg in pts.windows(2) {
                    p.line_segment(
                        [egui::pos2(seg[0][0], seg[0][1]), egui::pos2(seg[1][0], seg[1][1])],
                        egui::Stroke::new(2.0, theme::PINK),
                    );
                }
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

        // Drag start: a pin/terminal begins a wire, a body begins a move,
        // empty canvas pans the view.
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
                    .map(|(id, c, _)| CanvasDrag::Move { id: *id, grab: [w[0] - c[0], w[1] - c[1]] })
                    .or(Some(CanvasDrag::Pan));
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
            Some(CanvasDrag::Pan) => {
                let d = response.drag_delta();
                self.pan[0] -= d.x / scale;
                self.pan[1] -= d.y / scale;
                if response.drag_stopped() {
                    self.drag = None;
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
                    use wirelab_core::netlist::WireVerdict;
                    match self.wire_ok(&from, &to) {
                        WireVerdict::Ok => self.edit(
                            ops,
                            board_id,
                            serde_json::json!({
                                "op": "add_wire",
                                "a": serde_json::to_value(&from).unwrap_or_default(),
                                "b": serde_json::to_value(&to).unwrap_or_default(),
                            }),
                        ),
                        WireVerdict::Blocked(why) => {
                            self.notice = Some((format!("not wired: {why}"), now));
                        }
                        WireVerdict::Redundant => {
                            self.notice =
                                Some(("already connected — wire skipped".into(), now));
                        }
                    }
                }
            }
            _ => {}
        }

        // Flush finished moves. The drag already wrote the snapshot, so a
        // local project only has to record that it changed.
        if !self.moved.is_empty() && self.push.is_none() {
            let moves = std::mem::take(&mut self.moved);
            if self.is_local() {
                self.touch_local();
            } else {
                let body = serde_json::json!({ "board_id": board_id, "moves": moves }).to_string();
                self.post(ops, "/project/positions", body, Push::Moves);
            }
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

    pub fn show_flow(
        &mut self,
        ui: &mut egui::Ui,
        ops: &dyn Ops,
        now: f64,
        live_values: Option<HashMap<String, String>>,
    ) {
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
        // Names match the desktop's sanitized script identifiers, so node
        // combos label unlabeled parts too.
        let comp_names: Vec<String> = match self.model.as_ref() {
            Some(m) => wirelab_core::script::component_names(&tab.circuit, &m.lib)
                .into_values()
                .collect(),
            None => tab.circuit.components.values().map(|c| c.label.clone()).collect(),
        };
        let base = snap.flow_bases.get(&board_id.to_string()).copied();
        let index: HashMap<wirelab_flow_ui::egui_snarl::NodeId, usize> = self
            .flow_view
            .snarl
            .nodes_pos_ids()
            .enumerate()
            .map(|(i, (id, _, _))| (id, i))
            .collect();
        let mut viewer = FlowViewer::new(comp_names, live_values, index);
        viewer.options = ViewerOptions { editable: true };

        // Flow runs along the canvas's long axis, so a deep graph isn't a thin ribbon across it.
        let vertical = ui.available_height() > ui.available_width();
        ui.horizontal(|ui| {
            if ui.button("Arrange").on_hover_text("Lay the flow out by execution order").clicked() {
                crate::flowstyle::arrange(&mut self.flow_view.snarl, &HashMap::new(), vertical);
                self.flow_canvas.fit();
            }
            if ui.button("Fit").on_hover_text("Bring every node on screen").clicked() {
                self.flow_canvas.fit();
            }
            if ui.button("Start").on_hover_text("Pan to the first node").clicked() {
                self.flow_canvas.go_to_start(&self.flow_view.snarl, vertical);
            }
        });

        // Drawn directly rather than through `wirelab_flow_ui::show`, which hard-codes a bare
        // `SnarlStyle::new()`. `Styled` adds the dot grid and the view moves on top of the shared
        // viewer, which keeps full control of the nodes themselves.
        let mut styled =
            crate::flowstyle::Styled::new(&mut viewer, &mut self.flow_canvas, &self.flow_view.snarl);
        self.flow_view.snarl.show(&mut styled, &crate::flowstyle::style(), "ipad-flow", ui);
        drop(styled);
        crate::flowstyle::graph_menu(
            ui,
            &mut self.flow_canvas,
            &mut viewer,
            &mut self.flow_view.snarl,
        );

        // Overview only once the graph is big enough to lose your place in.
        if self.flow_view.snarl.nodes_pos_ids().count() > 3 {
            let canvas = ui.min_rect();
            let (map, snarl) = (&mut self.flow_canvas, &self.flow_view.snarl);
            egui::Area::new(egui::Id::new("flow-minimap"))
                .order(egui::Order::Foreground)
                .fixed_pos(canvas.right_top() + egui::vec2(-152.0, 8.0))
                .show(ui.ctx(), |ui| {
                    map.minimap(ui, snarl, egui::vec2(144.0, 88.0));
                });
        }

        // Push edits once the graph has sat still for a moment.
        let graph = wirelab_flow_ui::extract_graph(&self.flow_view.snarl);
        if graph != self.flow_synced {
            if self.flow_dirty_at.is_none() {
                self.flow_dirty_at = Some(now);
            }
            let settled = self.flow_dirty_at.is_some_and(|t0| now - t0 > 0.7);
            if self.is_local() {
                // No second writer, so no optimistic-concurrency base.
                if settled {
                    self.apply_flow_local(board_id, graph);
                }
            } else if let (Some(t0), None, None, Some(base)) =
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
                self.post(ops, "/project/flow", body, Push::Flow { board_id, graph });
            }
        } else {
            self.flow_dirty_at = None;
        }
        ui.label(
            egui::RichText::new(match (&self.flow_dirty_at, &self.push, self.is_local()) {
                (_, Some(_), _) => "syncing…",
                (Some(_), _, _) => "• editing (auto-saves)",
                (_, _, true) => "✔ saved on this device",
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
        let (buf, synced, board_id) = {
            let Some(snap) = &self.snapshot else { return };
            let Some(tab) = snap.boards.get(self.selected_board) else { return };
            let board_id = tab.id;
            let stored =
                tab.circuit.components.get(&CompId(comp)).and_then(|c| c.script.clone());
            match stored {
                Some(s) if !s.is_empty() => (s.clone(), s, board_id),
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
                    (template.clone(), template, board_id)
                }
            }
        };
        self.script_comp = Some(comp);
        self.script_board = Some(board_id);
        self.script_buf = buf;
        self.script_synced = synced;
        self.script_dirty_at = None;
        self.diagnostics.clear();
        self.compile_error = None;
    }

    /// Commit the edited flow graph into the open local project.
    fn apply_flow_local(&mut self, board_id: u64, graph: wirelab_core::flow::FlowGraph) {
        if let Some(snap) = &mut self.snapshot
            && let Some(tab) = snap.boards.iter_mut().find(|b| b.id == board_id)
        {
            tab.flow = graph.clone();
        }
        self.flow_synced = graph;
        self.flow_dirty_at = None;
        self.snap_rev += 1;
        self.touch_local();
    }

    /// Store a script on the open local project; syntax is checked in-process,
    /// the desktop's semantic diagnostics are not available.
    fn apply_script_local(&mut self, board_id: u64, comp: u32) {
        let src = self.script_buf.clone();
        self.compile_error = rhai::Engine::new().compile(&src).err().map(|e| e.to_string());
        self.diagnostics.clear();
        if let Some(snap) = &mut self.snapshot
            && let Some(tab) = snap.boards.iter_mut().find(|b| b.id == board_id)
            && let Some(c) = tab.circuit.components.get_mut(&CompId(comp))
        {
            c.script = Some(src.clone());
        }
        self.script_synced = src;
        self.script_dirty_at = None;
        self.snap_rev += 1;
        self.touch_local();
    }

    fn post_script(&mut self, ops: &dyn Ops, board_id: u64, comp: u32) {
        if self.is_local() {
            self.apply_script_local(board_id, comp);
            return;
        }
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
                    self.script_push = Some((id, board_id, comp));
                    // Optimistic: don't re-push identical text while in flight.
                    self.script_synced = self.script_buf.clone();
                    self.script_dirty_at = None;
                }
            }
            Err(e) => self.compile_error = Some(e),
        }
    }

    fn poll_script(&mut self, ops: &dyn Ops) {
        let Some((id, board_id, comp)) = self.script_push else { return };
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
        // Response state only applies while the editor still shows the
        // (board, comp) the push was for; a stale reply otherwise poisons
        // whatever board is selected now.
        let still_editing =
            self.script_board == Some(board_id) && self.script_comp == Some(comp);
        let v: serde_json::Value = serde_json::from_slice(&rsp.body).unwrap_or_default();
        if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            if still_editing {
                self.compile_error = Some(
                    v.get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("script push failed")
                        .to_string(),
                );
            }
            return;
        }
        // The desktop applied it: mirror the script into the snapshot so
        // dependents (live runner) hot-swap without a refetch.
        if still_editing
            && let Some(snap) = &mut self.snapshot
            && let Some(tab) = snap.boards.iter_mut().find(|b| b.id == board_id)
            && let Some(c) = tab.circuit.components.get_mut(&CompId(comp))
        {
            c.script = Some(self.script_synced.clone());
            self.snap_rev += 1;
        }
        if !still_editing {
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

        // A board switch invalidates the selection even when comp ids
        // collide across boards (they all start at 1).
        if let Some(sel) = self.script_comp
            && (self.script_board != Some(board_id) || !comps.iter().any(|(id, _, _)| *id == sel))
        {
            self.script_comp = None;
            self.script_board = None;
            self.script_dirty_at = None;
            self.script_buf.clear();
            self.script_synced.clear();
            self.diagnostics.clear();
            self.compile_error = None;
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

        // A caret move requested by a problem row, outline chip or snippet.
        let mut jump: Option<usize> = None;
        let editor_id = egui::Id::new("wirelab-script-editor");

        // Status (tap to expand the problems list) + snippet insert menu.
        let snips = self.model.as_ref().map(|m| crate::snippets::snippets_for(&m.board));
        let mut insert: Option<&'static str> = None;
        ui.horizontal_wrapped(|ui| {
            let n = self.diagnostics.len() + usize::from(self.compile_error.is_some());
            let ok = theme::AQUA;
            let err = theme::PINK;
            let status = match (&self.script_dirty_at, &self.script_push) {
                (_, Some(_)) => egui::RichText::new("linting…").small().weak(),
                (Some(_), _) => egui::RichText::new("editing…").small().weak(),
                _ if n == 0 => egui::RichText::new("no problems").small().color(ok),
                _ => egui::RichText::new(format!("{n} problem(s)")).small().color(err),
            };
            if n > 0 {
                if ui.add(egui::Button::new(status).small()).clicked() {
                    self.problems_open = !self.problems_open;
                }
            } else {
                ui.label(status);
                self.problems_open = false;
            }
            if !self.problems_open {
                if let Some((l, c, msg)) = self.diagnostics.first() {
                    ui.label(egui::RichText::new(format!("· {l}:{c} {msg}")).small().color(err));
                } else if let Some(e) = &self.compile_error {
                    ui.label(egui::RichText::new(format!("· {e}")).small().color(err));
                }
            }
            if self.is_local() {
                ui.label(
                    egui::RichText::new("• syntax only — connect a desktop for full checks")
                        .small()
                        .weak(),
                );
            }
            if let Some(snips) = &snips {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::menu_button(ui, "+ insert", |ui| {
                        for s in snips {
                            if ui.button(s.title).clicked() {
                                insert = Some(s.code);
                                ui.close();
                            }
                            // Touch has no hover: the blurb rides below.
                            ui.label(egui::RichText::new(s.blurb).small().weak());
                            ui.add_space(2.0);
                        }
                    });
                });
            }
        });

        // Every diagnostic as a tap-to-jump row (desktop Problems tab).
        if self.problems_open {
            let err = theme::PINK;
            let rows: Vec<(u32, u32, String)> = self.diagnostics.clone();
            theme::scroll_vertical().id_salt("problems").max_height(110.0).show(ui, |ui| {
                for (l, c, msg) in &rows {
                    let row = ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{l}:{c} {msg}")).small().color(err),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if row.clicked()
                        && let Some((s, _)) = line_col_to_span(&self.script_buf, *l, *c)
                    {
                        jump = Some(self.script_buf[..s].chars().count());
                    }
                }
                if let Some(e) = &self.compile_error {
                    ui.label(egui::RichText::new(e).small().color(err));
                }
            });
        }

        // Outline: tap a function to jump to it (scripts are callback-shaped).
        let fns: Vec<(usize, String)> = {
            let mut fns = Vec::new();
            let mut ci = 0usize;
            for line in self.script_buf.split_inclusive('\n') {
                let t = line.trim_start();
                if let Some(rest) = t.strip_prefix("fn ")
                    && let Some(name) = rest.split(['(', ' ']).next()
                    && !name.is_empty()
                {
                    fns.push((ci, name.to_string()));
                }
                ci += line.chars().count();
            }
            fns
        };
        if fns.len() >= 2 {
            egui::ScrollArea::horizontal().id_salt("outline").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (ci, name) in &fns {
                        if ui.small_button(format!("fn {name}")).clicked() {
                            jump = Some(*ci);
                        }
                    }
                });
            });
        }

        if let Some(code) = insert {
            if !self.script_buf.is_empty() && !self.script_buf.ends_with("\n\n") {
                self.script_buf.push_str(if self.script_buf.ends_with('\n') { "\n" } else { "\n\n" });
            }
            self.script_buf.push_str(code);
            jump = Some(self.script_buf.chars().count());
        }

        // The editor fills ~99% of the remaining height; the completion bar,
        // when present, reserves a thin strip below it.
        let has_suggestions = !self.completion.is_empty();
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

        // Apply a requested caret move (problem row, outline chip, snippet).
        if let Some(ci) = jump {
            let ci = ci.min(self.script_buf.chars().count());
            if let Some(mut st) = egui::text_edit::TextEditState::load(ui.ctx(), editor_id) {
                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(ci),
                )));
                st.store(ui.ctx(), editor_id);
            }
            ui.ctx().memory_mut(|m| m.request_focus(editor_id));
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

        // Debounced apply: in-process for a local project, otherwise a push
        // the desktop applies + lints.
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
