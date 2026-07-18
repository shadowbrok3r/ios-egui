//! Undo/redo for a graph tab.
//!
//! Snapshots, not a command log. Most edits happen *inside* `Snarl::show` — wires, node drags,
//! widget values, deletes from snarl's own menu — so there is no single place to record an
//! inverse without forking the viewer, and anything we failed to hook would corrupt the history
//! silently. Cloning the whole `Snarl` captures every change whatever its origin, and preserves
//! node ids exactly, so `props_node`, wires and the minimap's size cache stay coherent on restore.
//!
//! Changes are detected by hashing the graph each frame (cheap — no allocation) and cloning only
//! once the hash settles. That debounce is what makes a 60-frame drag a single undo entry.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use egui_snarl::{NodeId, Snarl};
use rucomfyui_node_graph::internal::{FlowNodeData, FlowValueType};

/// How long the graph must stop changing before the edit is committed. Long enough to swallow a
/// drag or a slider scrub, short enough that undo feels like it tracks what you just did.
const SETTLE: f64 = 0.4;
/// Deepest history per tab. Each entry is a whole graph, so this is the memory knob.
const MAX_ENTRIES: usize = 24;
/// Coarse ceiling on retained snapshots. A node carries its schema (and its option lists twice),
/// so a large workflow runs to ~1MB a snapshot and a count cap alone is not enough.
const MAX_BYTES: usize = 24 * 1024 * 1024;

type Graph = Snarl<FlowNodeData>;

struct Snapshot {
    graph: Graph,
    bytes: usize,
}

impl Snapshot {
    fn new(graph: &Graph) -> Self {
        Self { graph: graph.clone(), bytes: estimate_bytes(graph) }
    }
}

/// Per-tab undo/redo stacks. `baseline` is the last committed state — the thing an in-progress
/// edit is diffed against.
#[derive(Default)]
pub struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    baseline: Option<Snapshot>,
    /// Hash of the graph as last seen, committed or not.
    seen: u64,
    /// Hash of `baseline`. `seen != baseline_fp` means there is an edit not yet committed.
    baseline_fp: u64,
    /// When the current uncommitted change was first noticed.
    settling: Option<f64>,
    bytes: usize,
}

impl History {
    /// Adopt `graph` as the starting point and drop all history. For a tab that was just loaded,
    /// cleared, or opened — undoing across a whole-document swap is more surprising than useful.
    pub fn reset(&mut self, graph: &Graph) {
        self.undo.clear();
        self.redo.clear();
        self.bytes = 0;
        self.seen = fingerprint(graph);
        self.baseline_fp = self.seen;
        self.settling = None;
        self.baseline = Some(Snapshot::new(graph));
    }

    /// An edit exists that has not been committed yet — still inside the settle window, or held
    /// off because the finger is down.
    fn pending(&self) -> bool {
        self.seen != self.baseline_fp
    }

    pub fn can_undo(&self) -> bool {
        // A change the user can see but that has not settled yet is still undoable: offering a
        // dead button for 0.4s after every edit would feel broken.
        !self.undo.is_empty() || self.pending()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Call once a frame with the live graph. Commits an entry when the graph has changed and
    /// then held still for [`SETTLE`]. `interacting` holds the commit off while a drag or a
    /// slider is still under the finger, so one gesture is one entry.
    pub fn observe(&mut self, graph: &Graph, now: f64, interacting: bool) -> bool {
        if self.baseline.is_none() {
            self.reset(graph);
            return false;
        }
        let now_fp = fingerprint(graph);
        if now_fp != self.seen {
            // Still moving: restart the timer rather than committing mid-gesture.
            self.seen = now_fp;
            self.settling = Some(now);
            return false;
        }
        let Some(started) = self.settling else { return false };
        if interacting || now - started < SETTLE {
            return false;
        }
        self.settling = None;
        self.commit(graph)
    }

    /// Push the previous committed state and adopt the current one.
    fn commit(&mut self, graph: &Graph) -> bool {
        let Some(prev) = self.baseline.take() else { return false };
        // The fingerprint can settle back onto the committed state (drag a node and put it back);
        // there is nothing to undo in that case.
        if fingerprint(&prev.graph) == fingerprint(graph) {
            self.baseline = Some(prev);
            return false;
        }
        self.bytes += prev.bytes;
        self.undo.push(prev);
        self.redo.clear();
        self.baseline = Some(Snapshot::new(graph));
        self.baseline_fp = fingerprint(graph);
        self.trim();
        true
    }

    /// Drop the oldest entries until both budgets are satisfied. Always keeps one.
    fn trim(&mut self) {
        while self.undo.len() > MAX_ENTRIES || (self.bytes > MAX_BYTES && self.undo.len() > 1) {
            let dropped = self.undo.remove(0);
            self.bytes = self.bytes.saturating_sub(dropped.bytes);
        }
    }

    /// Restore the previous state into `graph`. The caller still has to bump the doc's epoch and
    /// drop anything holding a node id that may now be gone.
    pub fn undo(&mut self, graph: &mut Graph) -> bool {
        // An edit still inside the settle window is its own step. Without this, one tap would
        // jump back past BOTH it and the previous edit.
        if self.pending() {
            self.settling = None;
            self.commit(graph);
        }
        let Some(prev) = self.undo.pop() else { return false };
        self.bytes = self.bytes.saturating_sub(prev.bytes);
        self.redo.push(Snapshot::new(graph));
        self.apply(prev, graph);
        true
    }

    pub fn redo(&mut self, graph: &mut Graph) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        let snap = Snapshot::new(graph);
        self.bytes += snap.bytes;
        self.undo.push(snap);
        self.apply(next, graph);
        true
    }

    fn apply(&mut self, snap: Snapshot, graph: &mut Graph) {
        *graph = snap.graph.clone();
        self.seen = fingerprint(graph);
        self.baseline_fp = self.seen;
        // An in-flight edit is abandoned by the jump; do not attribute it to the restored state.
        self.settling = None;
        self.baseline = Some(snap);
    }
}

/// A hash of everything an edit can change: which nodes exist, where they are, and what is in
/// them. Deliberately excludes the schema, which never changes for a given class.
fn fingerprint(graph: &Graph) -> u64 {
    let mut h = DefaultHasher::new();
    // `nodes_ids_data` rather than `nodes_pos_ids`: the latter drops `Node::open`, and collapsing
    // a node is an edit the user can see and expects to be able to undo.
    let mut nodes: Vec<(NodeId, &egui_snarl::Node<FlowNodeData>)> = graph.nodes_ids_data().collect();
    // Iteration order is not guaranteed, and a reordering is not an edit.
    nodes.sort_by_key(|(id, _)| id.0);
    nodes.len().hash(&mut h);
    for (id, node) in nodes {
        id.0.hash(&mut h);
        // Sub-pixel jitter is not an edit worth an undo entry.
        (node.pos.x.round() as i64).hash(&mut h);
        (node.pos.y.round() as i64).hash(&mut h);
        node.open.hash(&mut h);
        node.value.object.name.hash(&mut h);
        for input in &node.value.inputs {
            hash_value(&input.value, &mut h);
        }
    }
    let mut wires: Vec<(usize, usize, usize, usize)> = graph
        .wires()
        .map(|(from, to)| (from.node.0, from.output, to.node.0, to.input))
        .collect();
    wires.sort_unstable();
    wires.hash(&mut h);
    h.finish()
}

/// Only the part of a widget a user can change — not its bounds or its option list.
fn hash_value(v: &FlowValueType, h: &mut DefaultHasher) {
    std::mem::discriminant(v).hash(h);
    match v {
        FlowValueType::Array { selected, .. } => selected.hash(h),
        FlowValueType::String { value, .. } => value.hash(h),
        FlowValueType::Float { value, .. } => value.to_bits().hash(h),
        FlowValueType::SignedInt { value, .. } => value.hash(h),
        FlowValueType::UnsignedInt { value, .. } => value.hash(h),
        FlowValueType::Boolean(b) => b.hash(h),
        FlowValueType::Other(_) | FlowValueType::Unknown => {}
    }
}

/// Rough retained size of a snapshot. Only the strings are worth counting, and the dominant term
/// is the option lists — which each node holds twice (once in `object`, once per input widget),
/// hence the doubling. An estimate is enough: it only has to keep the budget proportional.
fn estimate_bytes(graph: &Graph) -> usize {
    let mut n = 0usize;
    for (_, _, data) in graph.nodes_pos_ids() {
        n += 256 + data.object.name.len();
        for input in &data.inputs {
            n += 64;
            match &input.value {
                FlowValueType::Array { options, selected } => {
                    n += selected.len() + options.iter().map(|o| o.len() + 24).sum::<usize>();
                }
                FlowValueType::String { value, .. } => n += value.len(),
                _ => {}
            }
        }
    }
    n * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node with one editable string widget, built through the same bridge the app uses so the
    /// `Object` shape cannot drift from the real one.
    fn node(name: &str) -> FlowNodeData {
        let spec = format!(
            r#"{{"{name}": {{"input": {{"required": {{"text": ["STRING", {{"default": ""}}]}}}}, "output": ["IMAGE"]}}}}"#
        );
        let set = crate::schema::parse(&serde_json::from_str(&spec).unwrap());
        let info = crate::schema::to_object_info(&set);
        FlowNodeData::new(info[name].clone())
    }

    fn graph_with(names: &[&str]) -> Graph {
        let mut g = Snarl::new();
        for (i, n) in names.iter().enumerate() {
            g.insert_node(egui::pos2(i as f32 * 10.0, 0.0), node(n));
        }
        g
    }

    /// Drive `observe` past the settle window the way a frame loop would.
    fn settle(h: &mut History, g: &Graph, t: &mut f64) -> bool {
        h.observe(g, *t, false);
        *t += SETTLE + 0.1;
        h.observe(g, *t, false)
    }

    #[test]
    fn an_edit_commits_once_it_settles_and_undo_restores_it() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        let mut t = 0.0;
        h.reset(&g);
        assert!(!h.can_undo());

        g.insert_node(egui::pos2(50.0, 0.0), node("B"));
        assert!(settle(&mut h, &g, &mut t), "the edit never committed");
        assert!(h.can_undo());

        assert!(h.undo(&mut g));
        assert_eq!(g.nodes_pos_ids().count(), 1, "the added node survived undo");
        assert!(h.can_redo());
        assert!(h.redo(&mut g));
        assert_eq!(g.nodes_pos_ids().count(), 2);
    }

    #[test]
    fn a_gesture_in_progress_does_not_commit() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        h.reset(&g);

        g.insert_node(egui::pos2(50.0, 0.0), node("B"));
        h.observe(&g, 0.0, true);
        // Well past the settle window, but the finger is still down.
        assert!(!h.observe(&g, 10.0, true));
        assert!(h.undo.is_empty(), "committed mid-gesture");
        assert!(h.observe(&g, 10.1, false), "release should commit");
    }

    #[test]
    fn a_drag_that_keeps_moving_stays_one_entry() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        h.reset(&g);
        let id = g.nodes_pos_ids().next().unwrap().0;

        // Sixty frames of movement, each shorter than the settle window.
        let mut t = 0.0;
        for step in 1..=60 {
            if let Some(pos) = g.get_node_info_mut(id) {
                pos.pos = egui::pos2(step as f32, 0.0);
            }
            t += 0.016;
            h.observe(&g, t, false);
        }
        assert!(h.undo.is_empty(), "committed mid-drag");
        t += SETTLE + 0.1;
        h.observe(&g, t, false);
        assert_eq!(h.undo.len(), 1, "a single drag produced {} entries", h.undo.len());
    }

    #[test]
    fn a_change_that_reverts_itself_is_not_an_entry() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        let mut t = 0.0;
        h.reset(&g);
        let id = g.nodes_pos_ids().next().unwrap().0;

        if let Some(n) = g.get_node_info_mut(id) {
            n.pos = egui::pos2(99.0, 0.0);
        }
        h.observe(&g, t, false);
        if let Some(n) = g.get_node_info_mut(id) {
            n.pos = egui::pos2(0.0, 0.0);
        }
        t += SETTLE + 0.1;
        h.observe(&g, t, false);
        t += SETTLE + 0.1;
        assert!(!settle(&mut h, &g, &mut t), "a no-op move became an undo entry");
        assert!(!h.can_undo());
    }

    #[test]
    fn a_widget_edit_is_captured() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        let mut t = 0.0;
        h.reset(&g);
        let id = g.nodes_pos_ids().next().unwrap().0;

        if let Some(d) = g.get_node_mut(id) {
            d.inputs[0].value =
                FlowValueType::String { value: "a cat".into(), multiline: false };
        }
        assert!(settle(&mut h, &g, &mut t));
        h.undo(&mut g);
        let restored = &g.get_node(id).unwrap().inputs[0].value;
        assert!(
            matches!(restored, FlowValueType::String { value, .. } if value.is_empty()),
            "widget text was not restored"
        );
    }

    #[test]
    fn a_new_edit_after_undo_drops_the_redo_branch() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        let mut t = 0.0;
        h.reset(&g);

        g.insert_node(egui::pos2(50.0, 0.0), node("B"));
        settle(&mut h, &g, &mut t);
        h.undo(&mut g);
        assert!(h.can_redo());

        g.insert_node(egui::pos2(80.0, 0.0), node("C"));
        settle(&mut h, &g, &mut t);
        assert!(!h.can_redo(), "redo survived a divergent edit");
    }

    #[test]
    fn history_is_bounded() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        let mut t = 0.0;
        h.reset(&g);
        for i in 0..(MAX_ENTRIES + 10) {
            g.insert_node(egui::pos2(i as f32, 5.0), node("N"));
            settle(&mut h, &g, &mut t);
        }
        assert!(h.undo.len() <= MAX_ENTRIES, "history grew to {}", h.undo.len());
        assert!(h.can_undo());
    }

    #[test]
    fn undo_during_the_settle_window_reverts_one_edit_not_two() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        let mut t = 0.0;
        h.reset(&g);

        // First edit, committed.
        g.insert_node(egui::pos2(50.0, 0.0), node("B"));
        settle(&mut h, &g, &mut t);
        assert_eq!(g.nodes_pos_ids().count(), 2);

        // Second edit, still inside the settle window when undo is tapped.
        g.insert_node(egui::pos2(80.0, 0.0), node("C"));
        h.observe(&g, t, false);
        assert!(h.can_undo(), "an uncommitted edit must still be undoable");

        h.undo(&mut g);
        assert_eq!(g.nodes_pos_ids().count(), 2, "one tap undid two edits");
        h.undo(&mut g);
        assert_eq!(g.nodes_pos_ids().count(), 1);
    }

    #[test]
    fn the_very_first_edit_is_undoable_before_it_settles() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        h.reset(&g);
        assert!(!h.can_undo());

        g.insert_node(egui::pos2(50.0, 0.0), node("B"));
        h.observe(&g, 0.0, false);
        assert!(h.can_undo());
        assert!(h.undo(&mut g));
        assert_eq!(g.nodes_pos_ids().count(), 1);
    }

    /// Snarl draws a collapse triangle on every node header and handles the tap itself, so this
    /// change never passes through the app at all — the hash is the only thing that can see it.
    #[test]
    fn collapsing_a_node_is_an_edit() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        let mut t = 0.0;
        h.reset(&g);
        let id = g.nodes_pos_ids().next().unwrap().0;

        g.open_node(id, false);
        assert!(settle(&mut h, &g, &mut t), "collapse was invisible to the history");
        h.undo(&mut g);
        assert!(g.get_node_info(id).unwrap().open, "collapse was not undone");
    }

    #[test]
    fn reset_forgets_everything() {
        let mut g = graph_with(&["A"]);
        let mut h = History::default();
        let mut t = 0.0;
        h.reset(&g);
        g.insert_node(egui::pos2(50.0, 0.0), node("B"));
        settle(&mut h, &g, &mut t);
        assert!(h.can_undo());

        let fresh = graph_with(&["X", "Y"]);
        h.reset(&fresh);
        assert!(!h.can_undo() && !h.can_redo());
    }
}
