//! Graph-tab presentation layer over [`ComfyUiNodeGraph`]: renders the snarl canvas with a stable
//! widget id, a view-only lock mode, programmatic view commands (fit-all / center-on-point), a
//! minimap overlay, and the node properties editor shared with the Properties tab.

use std::collections::{HashMap, HashSet};

use egui::emath::TSTransform;
use egui_snarl::ui::{PinInfo, SnarlStyle, SnarlViewer, SnarlWidget};
use egui_snarl::{InPin, InPinId, NodeId, OutPin, Snarl};
use rucomfyui_node_graph::ComfyUiNodeGraph;
use rucomfyui_node_graph::internal::{FlowInput, FlowNodeData, FlowValueType, FlowViewer};

pub const MIN_SCALE: f32 = 0.05;
pub const MAX_SCALE: f32 = 2.5;
/// Assumed node extent until its real size is measured on screen.
const NOMINAL_NODE: egui::Vec2 = egui::vec2(180.0, 100.0);
/// Rough half-node offset so centering lands mid-node rather than on its corner.
const NODE_CENTER_OFFSET: egui::Vec2 = egui::vec2(90.0, 50.0);

/// A one-shot view command applied on the next rendered frame.
pub enum ViewCmd {
    FitAll,
    Center(egui::Pos2),
    /// Center on a point and zoom into a comfortable range (for auto-follow).
    Focus(egui::Pos2),
}

/// Result of a long-press on the graph canvas.
pub enum LongPress {
    /// Empty canvas — open Add node / paste menu at this graph-space point.
    Canvas(egui::Pos2),
    /// Held on a node — open the node action menu (bypass / auto-wire).
    Node(NodeId),
}

/// A `lora_name` combo selection that changed this frame (canvas or properties).
pub struct LoraPick {
    pub node: NodeId,
    pub file: String,
}

/// View state and overlays for the graph canvas.
pub struct GraphView {
    pub locked: bool,
    /// Per-tab snarl widget id (shared ids leak draw-order / node state across tabs).
    widget_id: egui::Id,
    cmd: Option<ViewCmd>,
    arrange_queued: bool,
    /// Frames spent waiting for measured node sizes before a queued arrange runs.
    arrange_wait: u8,
    /// Frames to keep reporting a layout as in-flight after it runs, so undo does not record
    /// the settling positions as a user edit.
    arrange_settling: u8,
    /// Auto-arrange requested by a load, waiting for the canvas to paint before running.
    needs_auto_arrange: bool,
    sizes: HashMap<NodeId, egui::Vec2>,
    to_global: TSTransform,
    pub view_rect: egui::Rect,
    /// Whether the in-progress drag started on top of a node. In locked mode that drag pans the
    /// canvas instead of doing nothing, so panning never depends on finding empty space.
    drag_from_node: bool,
    /// A press held still: (start time, screen origin, node under press if any).
    press: Option<(f64, egui::Pos2, Option<NodeId>)>,
    /// One long-press has already fired for the current press.
    long_fired: bool,
    /// Long-press this frame (canvas add-menu or node menu).
    long_press: Option<LongPress>,
    /// `lora_name` picks this frame (recommended strengths applied by the app).
    lora_picks: Vec<LoraPick>,
}

impl Default for GraphView {
    fn default() -> Self {
        Self::new(0)
    }
}

impl GraphView {
    pub fn new(doc_id: u64) -> Self {
        Self {
            locked: false,
            widget_id: egui::Id::new(("comfy-graph-canvas", doc_id)),
            cmd: None,
            arrange_queued: false,
            arrange_wait: 0,
            arrange_settling: 0,
            needs_auto_arrange: false,
            sizes: HashMap::new(),
            to_global: TSTransform::IDENTITY,
            view_rect: egui::Rect::ZERO,
            drag_from_node: false,
            press: None,
            long_fired: false,
            long_press: None,
            lora_picks: Vec::new(),
        }
    }

    /// Forget cached geometry and pending commands (snarl node ids restart when a new graph is
    /// loaded, so stale sizes would attach to the wrong nodes).
    pub fn reset(&mut self) {
        self.cmd = None;
        self.arrange_queued = false;
        self.arrange_wait = 0;
        self.arrange_settling = 0;
        self.needs_auto_arrange = false;
        self.sizes.clear();
        self.press = None;
        self.long_fired = false;
        self.long_press = None;
        self.lora_picks.clear();
    }

    pub fn request_fit(&mut self) {
        self.cmd = Some(ViewCmd::FitAll);
    }

    /// Queue a compact layout once measured sizes are available (or after a short wait).
    /// An auto-layout is queued, or has just run. Undo treats the whole settling layout as part
    /// of whatever asked for it, rather than a second step to undo separately. The grace frames
    /// matter because `arrange_now` clears the queue flag *before* it moves anything, so without
    /// them the move itself looks like a user edit.
    pub fn arrange_pending(&self) -> bool {
        self.needs_auto_arrange || self.arrange_queued || self.arrange_settling > 0
    }

    /// Defer auto-arrange until the canvas paints (Create sync / background loads never call `show`).
    pub fn mark_needs_auto_arrange(&mut self) {
        self.needs_auto_arrange = true;
        self.arrange_queued = false;
        self.arrange_wait = 0;
        self.cmd = Some(ViewCmd::FitAll);
    }

    pub fn request_arrange(&mut self) {
        self.needs_auto_arrange = false;
        self.arrange_queued = true;
        self.arrange_wait = 0;
        self.cmd = Some(ViewCmd::FitAll);
    }

    /// Arrange immediately. Uses measured sizes when present, else [`NOMINAL_NODE`].
    /// Does not invent size cache entries — placeholders would fake "measured" and skip refine.
    pub fn arrange_now(&mut self, snarl: &mut Snarl<FlowNodeData>) {
        self.needs_auto_arrange = false;
        self.arrange_queued = false;
        self.arrange_wait = 0;
        self.arrange_settling = 3;
        if snarl.nodes_pos_ids().next().is_none() {
            return;
        }
        arrange(snarl, &self.sizes);
        self.cmd = Some(ViewCmd::FitAll);
    }

    /// Load-time layout: nominal arrange + fit so every node paints, then queue a refine pass
    /// once `final_node_rect` has filled real sizes.
    pub fn arrange_on_load(&mut self, snarl: &mut Snarl<FlowNodeData>) {
        self.needs_auto_arrange = false;
        self.arrange_now(snarl);
        self.arrange_queued = true;
        self.arrange_wait = 0;
    }

    /// Center the view on a node position (graph space).
    pub fn center_on(&mut self, node_pos: egui::Pos2) {
        self.cmd = Some(ViewCmd::Center(node_pos + NODE_CENTER_OFFSET));
    }

    /// Center on a node and zoom into a readable range — the auto-follow motion.
    pub fn focus_on(&mut self, node_pos: egui::Pos2) {
        self.cmd = Some(ViewCmd::Focus(node_pos + NODE_CENTER_OFFSET));
    }

    /// The graph-space point currently at the middle of the canvas.
    pub fn center_in_graph(&self) -> Option<egui::Pos2> {
        (self.view_rect.width() > 0.0)
            .then(|| self.to_global.inverse() * self.view_rect.center())
    }

    /// `lora_name` selections made while rendering the canvas this frame.
    pub fn take_lora_picks(&mut self) -> Vec<LoraPick> {
        std::mem::take(&mut self.lora_picks)
    }

    /// Render the canvas (with lock gating and pending view commands), then the minimap overlay.
    /// Returns the node tapped this frame, if any — snarl itself only selects on shift-click,
    /// which doesn't exist on touch.
    ///
    /// `lora_files` fills empty `lora_name` combos (Create-tab union across loader classes).
    #[must_use]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        g: &mut ComfyUiNodeGraph,
        executing: Option<NodeId>,
        focus: Option<NodeId>,
        bypassed: &HashSet<NodeId>,
        lora_files: &[String],
    ) -> Option<NodeId> {
        self.sizes.retain(|id, _| g.snarl.get_node(*id).is_some());
        self.arrange_settling = self.arrange_settling.saturating_sub(1);
        self.lora_picks.clear();
        // Keep file combos populated even when a single loader class shipped an empty list.
        for data in g.snarl.nodes_mut() {
            ensure_file_combos(data, &g.object_info, lora_files);
        }

        // Background loads only mark the flag — arrange once the canvas is actually painting.
        if self.needs_auto_arrange {
            self.arrange_on_load(&mut g.snarl);
        }

        if self.arrange_queued {
            let ids: Vec<NodeId> = g.snarl.nodes_pos_ids().map(|(id, _, _)| id).collect();
            if ids.is_empty() {
                self.arrange_queued = false;
                self.arrange_wait = 0;
            } else {
                let ready = ids.iter().all(|id| self.sizes.contains_key(id));
                self.arrange_wait = self.arrange_wait.saturating_add(1);
                // Prefer real measures; after a few FitAll frames, arrange with what we have so a
                // never-drawn node cannot stall the queue forever.
                if ready || self.arrange_wait >= 3 {
                    self.arrange_now(&mut g.snarl);
                } else {
                    self.cmd = Some(ViewCmd::FitAll);
                    ui.ctx().request_repaint();
                }
            }
        }

        // Snapshot after arrange so lock-restore cannot undo a fresh layout.
        let saved: Option<Vec<(NodeId, egui::Pos2)>> = self
            .locked
            .then(|| g.snarl.nodes_pos_ids().map(|(id, pos, _)| (id, pos)).collect());

        // The snarl response rect is unbounded (scene ui); measure the canvas region ourselves.
        let canvas = ui.available_rect_before_wrap();
        if canvas.is_finite() && canvas.width() > 0.0 {
            self.view_rect = canvas;
        }
        let pan = self.locked_pan(ui.ctx(), &g.snarl);
        let cmd = if self.view_rect.width() > 0.0 { self.cmd.take() } else { None };
        let mut viewer = Wrapper {
            inner: FlowViewer { user_state: &mut g.user_state, object_info: &g.object_info },
            locked: self.locked,
            focus,
            bypassed,
            lora_picks: &mut self.lora_picks,
            cmd,
            pan,
            bounds: bounds(&g.snarl, &self.sizes),
            ui_rect: self.view_rect,
            sizes: &mut self.sizes,
            out_transform: &mut self.to_global,
        };
        SnarlWidget::new()
            .id(self.widget_id)
            .style(style())
            .show(&mut g.snarl, &mut viewer, ui);

        if let Some(saved) = saved {
            for (id, pos) in saved {
                if let Some(info) = g.snarl.get_node_info_mut(id) {
                    info.pos = pos;
                }
            }
        }

        let tapped = self.tapped_node(ui.ctx(), &g.snarl);
        self.long_press = self.detect_long_press(ui.ctx(), &g.snarl);
        self.minimap(ui, &g.snarl, executing, focus);
        self.lock_button(ui);
        tapped
    }

    /// A long-press this frame. Taken so it fires once.
    pub fn take_long_press(&mut self) -> Option<LongPress> {
        self.long_press.take()
    }

    /// Detect a finger held still for ~0.5s: on a node → bypass toggle; on empty canvas → add menu.
    /// Locked mode uses the same drag to pan, so long-press is disabled while locked.
    fn detect_long_press(
        &mut self,
        ctx: &egui::Context,
        snarl: &Snarl<FlowNodeData>,
    ) -> Option<LongPress> {
        if self.locked {
            self.press = None;
            self.long_fired = false;
            return None;
        }
        let (down, pos, time, dragging) = ctx.input(|i| {
            (
                i.pointer.any_down(),
                i.pointer.interact_pos(),
                i.time,
                i.pointer.is_decidedly_dragging(),
            )
        });
        if !down {
            self.press = None;
            self.long_fired = false;
            return None;
        }
        let Some(pos) = pos else { return None };
        if !self.view_rect.contains(pos)
            || ctx.layer_id_at(pos).is_some_and(|l| l.order != egui::Order::Background)
        {
            self.press = None;
            return None;
        }
        let under = self.node_at(ctx, pos, snarl);
        match self.press {
            None => {
                self.press = Some((time, pos, under));
                None
            }
            Some((start, origin, node)) => {
                if dragging || (origin - pos).length() > 12.0 {
                    self.press = None;
                    return None;
                }
                // Finger slid onto a different target — cancel.
                if under != node {
                    self.press = None;
                    return None;
                }
                if !self.long_fired && time - start > 0.5 {
                    self.long_fired = true;
                    ctx.request_repaint();
                    return Some(match node {
                        Some(id) => LongPress::Node(id),
                        None => LongPress::Canvas(self.to_global.inverse() * pos),
                    });
                }
                ctx.request_repaint();
                None
            }
        }
    }

    /// The node under a tap released this frame. Taps that land on higher layers (windows, the
    /// minimap, the lock button) don't count.
    fn tapped_node(&self, ctx: &egui::Context, snarl: &Snarl<FlowNodeData>) -> Option<NodeId> {
        let pos = ctx.input(|i| {
            (i.pointer.any_click() && !i.pointer.is_decidedly_dragging())
                .then(|| i.pointer.interact_pos())
                .flatten()
        })?;
        self.node_at(ctx, pos, snarl)
    }

    /// The node under a screen position, ignoring positions owned by an overlay.
    fn node_at(
        &self,
        ctx: &egui::Context,
        pos: egui::Pos2,
        snarl: &Snarl<FlowNodeData>,
    ) -> Option<NodeId> {
        if !self.view_rect.contains(pos) {
            return None;
        }
        if ctx.layer_id_at(pos).is_some_and(|l| l.order != egui::Order::Background) {
            return None;
        }
        let graph_pos = self.to_global.inverse() * pos;
        for (id, node_pos, _) in snarl.nodes_pos_ids() {
            let size = self.sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
            if egui::Rect::from_min_size(node_pos, size).contains(graph_pos) {
                return Some(id);
            }
        }
        None
    }

    /// The pan to apply this frame because a locked-mode drag began on a node.
    ///
    /// Snarl only pans from empty canvas, and a dense graph leaves little of it — so in view-only
    /// mode a drag starting anywhere, node or not, moves the view. The node itself can't move: its
    /// position is snapshotted and restored around the canvas.
    fn locked_pan(&mut self, ctx: &egui::Context, snarl: &Snarl<FlowNodeData>) -> egui::Vec2 {
        if !self.locked {
            self.drag_from_node = false;
            return egui::Vec2::ZERO;
        }
        let (pressed, down, press_origin, delta) = ctx.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.any_down(),
                i.pointer.press_origin(),
                i.pointer.delta(),
            )
        });
        if pressed {
            self.drag_from_node =
                press_origin.is_some_and(|p| self.node_at(ctx, p, snarl).is_some());
        }
        if !down {
            self.drag_from_node = false;
        }
        if self.drag_from_node && delta.is_finite() { delta } else { egui::Vec2::ZERO }
    }

    /// Floating lock toggle in the canvas's bottom-right (left of the queue FAB).
    fn lock_button(&mut self, ui: &mut egui::Ui) {
        let view = self.view_rect;
        if !view.is_finite() || view.width() < 80.0 {
            return;
        }
        let (icon, tip) = if self.locked {
            (crate::icons::LOCKED, "View only — tap to edit")
        } else {
            (crate::icons::UNLOCKED, "Editing — tap to lock")
        };
        // Primary queue FAB sits at (right-58, bottom-58); lock is one slot left.
        egui::Area::new(egui::Id::new("comfy-lock"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(view.right() - 58.0 - 56.0, view.bottom() - 58.0))
            .show(ui.ctx(), |aui| {
                let btn = egui::Button::new(egui::RichText::new(icon).size(22.0))
                    .min_size(egui::vec2(48.0, 48.0))
                    .corner_radius(24.0)
                    .fill(egui::Color32::from_rgb(45, 55, 85));
                if aui.add(btn).on_hover_text(tip).clicked() {
                    self.locked = !self.locked;
                }
            });
    }

    /// Corner overlay showing every node and the current viewport; tap or drag to jump.
    fn minimap(
        &mut self,
        ui: &mut egui::Ui,
        snarl: &Snarl<FlowNodeData>,
        executing: Option<NodeId>,
        focus: Option<NodeId>,
    ) {
        let view = self.view_rect;
        if !view.is_finite() || view.width() < 160.0 || view.height() < 160.0 {
            return;
        }
        let Some(b) = bounds(snarl, &self.sizes) else { return };
        if !b.is_finite() {
            return;
        }
        let b = b.expand(60.0);
        let w = (view.width() * 0.30).clamp(96.0, 200.0);
        let h = (w * (b.height() / b.width()).clamp(0.35, 1.4)).clamp(60.0, 200.0);
        let corner = egui::pos2(view.left() + 10.0, view.top() + 10.0);

        egui::Area::new(egui::Id::new("comfy-minimap"))
            .order(egui::Order::Foreground)
            .fixed_pos(corner)
            .show(ui.ctx(), |aui| {
                let (resp, p) =
                    aui.allocate_painter(egui::vec2(w, h), egui::Sense::click_and_drag());
                let rect = resp.rect;
                let scale = (rect.size() / b.size()).min_elem();
                let tf = TSTransform::new(
                    rect.center().to_vec2() - b.center().to_vec2() * scale,
                    scale,
                );

                p.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(170));
                for (id, pos, _) in snarl.nodes_pos_ids() {
                    let size = self.sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
                    let mut m = tf * egui::Rect::from_min_size(pos, size);
                    if m.width() < 2.0 || m.height() < 2.0 {
                        m = egui::Rect::from_center_size(m.center(), m.size().max(egui::vec2(2.0, 2.0)));
                    }
                    let color = if executing == Some(id) {
                        egui::Color32::from_rgb(90, 200, 110)
                    } else if focus == Some(id) {
                        egui::Color32::from_rgb(110, 170, 255)
                    } else {
                        egui::Color32::from_gray(150)
                    };
                    p.rect_filled(m, 1.0, color);
                }
                let viewport = (tf * (self.to_global.inverse() * view)).intersect(rect);
                p.rect_stroke(
                    viewport,
                    0.0,
                    egui::Stroke::new(1.0, egui::Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
                p.rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
                    egui::StrokeKind::Inside,
                );

                if (resp.clicked() || resp.dragged())
                    && let Some(pointer) = resp.interact_pointer_pos()
                {
                    self.cmd = Some(ViewCmd::Center(tf.inverse() * pointer));
                }
            });
    }
}

fn style() -> SnarlStyle {
    let mut s = SnarlStyle::new();
    s.bg_frame = Some(egui::Frame::new().fill(egui::Color32::from_rgb(10, 10, 13)));
    s.min_scale = Some(MIN_SCALE);
    s.max_scale = Some(MAX_SCALE);
    s.centering = Some(true);
    s
}

/// A node body fill a step brighter than the canvas, so nodes read as raised.
const NODE_FILL: egui::Color32 = egui::Color32::from_rgb(34, 34, 42);

/// Bounding box of all nodes in graph space (measured sizes where known).
fn bounds(snarl: &Snarl<FlowNodeData>, sizes: &HashMap<NodeId, egui::Vec2>) -> Option<egui::Rect> {
    let mut b: Option<egui::Rect> = None;
    for (id, pos, _) in snarl.nodes_pos_ids() {
        let size = sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
        let r = egui::Rect::from_min_size(pos, size);
        b = Some(b.map_or(r, |b| b.union(r)));
    }
    b
}

/// The transform that fits `view` (graph space) into `ui_rect` (screen space), scale clamped.
fn fit_transform(view: egui::Rect, ui_rect: egui::Rect) -> TSTransform {
    let scale = (ui_rect.size() / view.size()).min_elem().clamp(MIN_SCALE, MAX_SCALE);
    TSTransform::new(ui_rect.center().to_vec2() - view.center().to_vec2() * scale, scale)
}

/// Compact layout: columns by longest-path depth (left to right), nodes stacked within each
/// column with small gaps, columns vertically centered — measured sizes, so nothing overlaps.
/// Returns the placed rects.
pub fn arrange(
    snarl: &mut Snarl<FlowNodeData>,
    sizes: &HashMap<NodeId, egui::Vec2>,
) -> Vec<egui::Rect> {
    const H_GAP: f32 = 60.0;
    const V_GAP: f32 = 24.0;

    let ids: Vec<NodeId> = snarl.nodes_pos_ids().map(|(id, _, _)| id).collect();
    let mut successors: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut predecessors: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (from, to) in snarl.wires() {
        if from.node == to.node {
            continue;
        }
        successors.entry(from.node).or_default().push(to.node);
        predecessors.entry(to.node).or_default().push(from.node);
    }

    // Pseudo-topological order via iterative DFS post-order — robust to cycles, which a converted
    // workflow can contain (SetNode/GetNode and "Anything Everywhere" links reconstruct as
    // back-edges). Kahn's-style layering would let one such cycle poison every downstream node's
    // depth and collapse the whole graph into a single column.
    let mut order: Vec<NodeId> = Vec::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    for &start in &ids {
        if visited.contains(&start) {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((node, processed)) = stack.pop() {
            if processed {
                order.push(node);
                continue;
            }
            if !visited.insert(node) {
                continue;
            }
            stack.push((node, true));
            for &next in successors.get(&node).into_iter().flatten() {
                if !visited.contains(&next) {
                    stack.push((next, false));
                }
            }
        }
    }
    order.reverse(); // producers before consumers
    let topo: HashMap<NodeId, usize> = order.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    // Longest-path layer over forward edges only (topo index increases); back-edges wrap around
    // rather than shoving their target into a late column.
    let mut depth: HashMap<NodeId, usize> = ids.iter().map(|&id| (id, 0)).collect();
    for &node in &order {
        let d = depth[&node];
        for &next in successors.get(&node).into_iter().flatten() {
            if topo.get(&next).copied().unwrap_or(0) > topo[&node] {
                let e = depth.entry(next).or_insert(0);
                *e = (*e).max(d + 1);
            }
        }
    }
    let deepest = depth.values().copied().max().unwrap_or(0);
    let mut columns: Vec<Vec<NodeId>> = vec![Vec::new(); deepest + 1];
    for (id, _, _) in snarl.nodes_pos_ids() {
        let d = depth.get(&id).copied().unwrap_or(0);
        columns[d].push(id);
    }
    // Seed each column's vertical order from the original layout, then reduce edge crossings with
    // barycenter sweeps (each node drifts toward the average row of its neighbours) so wires run
    // mostly straight left-to-right and the order of execution reads down each column.
    for column in &mut columns {
        column.sort_by(|a, b| {
            let ya = snarl.get_node_info(*a).map(|n| n.pos.y).unwrap_or(0.0);
            let yb = snarl.get_node_info(*b).map(|n| n.pos.y).unwrap_or(0.0);
            ya.total_cmp(&yb)
        });
    }
    let indices = |columns: &[Vec<NodeId>]| -> HashMap<NodeId, f32> {
        let mut m = HashMap::new();
        for column in columns {
            for (i, &id) in column.iter().enumerate() {
                m.insert(id, i as f32);
            }
        }
        m
    };
    let barycenter = |id: NodeId, neighbors: &HashMap<NodeId, Vec<NodeId>>, idx: &HashMap<NodeId, f32>, fallback: f32| -> f32 {
        match neighbors.get(&id) {
            Some(ns) if !ns.is_empty() => {
                ns.iter().filter_map(|n| idx.get(n)).sum::<f32>() / ns.len() as f32
            }
            _ => fallback,
        }
    };
    let reorder = |column: &mut Vec<NodeId>, neighbors: &HashMap<NodeId, Vec<NodeId>>, idx: &HashMap<NodeId, f32>| {
        let mut keyed: Vec<(NodeId, f32)> = column
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, barycenter(id, neighbors, idx, i as f32)))
            .collect();
        keyed.sort_by(|a, b| a.1.total_cmp(&b.1));
        *column = keyed.into_iter().map(|(id, _)| id).collect();
    };
    for _ in 0..4 {
        let idx = indices(&columns);
        for d in 1..columns.len() {
            let mut column = std::mem::take(&mut columns[d]);
            reorder(&mut column, &predecessors, &idx);
            columns[d] = column;
        }
        let idx = indices(&columns);
        for d in (0..columns.len().saturating_sub(1)).rev() {
            let mut column = std::mem::take(&mut columns[d]);
            reorder(&mut column, &successors, &idx);
            columns[d] = column;
        }
    }

    let size_of = |id: NodeId| sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
    let mut rects = Vec::new();
    let mut x = 0.0f32;
    for column in columns {
        if column.is_empty() {
            continue;
        }
        let col_width = column.iter().map(|&id| size_of(id).x).fold(1.0f32, f32::max);
        let total_height: f32 = column.iter().map(|&id| size_of(id).y + V_GAP).sum::<f32>() - V_GAP;
        let mut y = -total_height / 2.0;
        for id in column {
            let size = size_of(id);
            if let Some(info) = snarl.get_node_info_mut(id) {
                info.pos = egui::pos2(x, y);
            }
            rects.push(egui::Rect::from_min_size(egui::pos2(x, y), size));
            y += size.y + V_GAP;
        }
        x += col_width + H_GAP;
    }
    rects
}

/// Position of a workflow's first node: the leftmost node with no incoming wires (any of them),
/// falling back to the leftmost node overall.
pub fn first_node_pos(snarl: &Snarl<FlowNodeData>) -> Option<egui::Pos2> {
    let has_input: HashSet<NodeId> = snarl.wires().map(|(_, in_pin)| in_pin.node).collect();
    let mut root: Option<egui::Pos2> = None;
    let mut leftmost: Option<egui::Pos2> = None;
    for (id, pos, _) in snarl.nodes_pos_ids() {
        if leftmost.is_none_or(|p| pos.x < p.x) {
            leftmost = Some(pos);
        }
        if !has_input.contains(&id) && root.is_none_or(|p| pos.x < p.x) {
            root = Some(pos);
        }
    }
    root.or(leftmost)
}

/// Delegates to [`FlowViewer`], gating all mutations when locked, measuring node sizes for the
/// minimap, and applying pending view commands through the transform hook.
struct Wrapper<'a> {
    inner: FlowViewer<'a>,
    locked: bool,
    focus: Option<NodeId>,
    bypassed: &'a HashSet<NodeId>,
    lora_picks: &'a mut Vec<LoraPick>,
    cmd: Option<ViewCmd>,
    /// Screen-space pan to add this frame (locked-mode drag started on a node).
    pan: egui::Vec2,
    bounds: Option<egui::Rect>,
    ui_rect: egui::Rect,
    sizes: &'a mut HashMap<NodeId, egui::Vec2>,
    out_transform: &'a mut TSTransform,
}

impl SnarlViewer<FlowNodeData> for Wrapper<'_> {
    fn title(&mut self, node: &FlowNodeData) -> String {
        self.inner.title(node)
    }

    fn inputs(&mut self, node: &FlowNodeData) -> usize {
        self.inner.inputs(node)
    }

    fn outputs(&mut self, node: &FlowNodeData) -> usize {
        self.inner.outputs(node)
    }

    #[allow(refining_impl_trait)]
    fn show_input(
        &mut self,
        pin: &InPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<FlowNodeData>,
    ) -> PinInfo {
        let before = lora_name_selected(snarl, pin.id.node);
        let info = if self.locked {
            let mut info = None;
            ui.add_enabled_ui(false, |ui| info = Some(self.inner.show_input(pin, ui, snarl)));
            info.unwrap_or_else(PinInfo::circle)
        } else {
            self.inner.show_input(pin, ui, snarl)
        };
        if let Some(file) = lora_name_changed(snarl, pin.id.node, before.as_deref()) {
            self.lora_picks.push(LoraPick { node: pin.id.node, file });
        }
        info
    }

    #[allow(refining_impl_trait)]
    fn show_output(
        &mut self,
        pin: &OutPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<FlowNodeData>,
    ) -> PinInfo {
        self.inner.show_output(pin, ui, snarl)
    }

    fn node_frame(
        &mut self,
        default: egui::Frame,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        snarl: &Snarl<FlowNodeData>,
    ) -> egui::Frame {
        let mut frame = self.inner.node_frame(default, node, inputs, outputs, snarl).fill(NODE_FILL);
        if self.bypassed.contains(&node) {
            // Dimmed fill + dashed-feel orange stroke marks a bypassed (mode-4) node.
            frame = frame
                .fill(egui::Color32::from_rgb(55, 48, 40))
                .stroke(egui::Stroke::new(2.0, egui::Color32::from_rgb(210, 140, 70)));
        } else if self.focus == Some(node) && frame.stroke.width < 2.0 {
            // The executing highlight (green stroke from the inner viewer) wins over focus.
            frame = frame.stroke(egui::Stroke::new(2.0, egui::Color32::from_rgb(150, 140, 226)));
        }
        frame
    }

    fn has_body(&mut self, node: &FlowNodeData) -> bool {
        self.inner.has_body(node)
    }

    fn has_footer(&mut self, node: &FlowNodeData) -> bool {
        self.inner.has_footer(node)
    }

    fn show_footer(
        &mut self,
        node_id: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<FlowNodeData>,
    ) {
        self.inner.show_footer(node_id, inputs, outputs, ui, snarl);
    }

    fn final_node_rect(
        &mut self,
        node: NodeId,
        rect: egui::Rect,
        _ui: &mut egui::Ui,
        _snarl: &mut Snarl<FlowNodeData>,
    ) {
        self.sizes.insert(node, rect.size());
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<FlowNodeData>) {
        if !self.locked {
            self.inner.connect(from, to, snarl);
        }
    }

    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<FlowNodeData>) {
        if !self.locked {
            snarl.disconnect(from.id, to.id);
        }
    }

    fn drop_outputs(&mut self, pin: &OutPin, snarl: &mut Snarl<FlowNodeData>) {
        if !self.locked {
            snarl.drop_outputs(pin.id);
        }
    }

    fn drop_inputs(&mut self, pin: &InPin, snarl: &mut Snarl<FlowNodeData>) {
        if !self.locked {
            snarl.drop_inputs(pin.id);
        }
    }

    // The empty-canvas menu is handled by our own long-press detection + Add node window, not
    // snarl's native context menu (which is transient on touch — it closes the instant the finger
    // lifts). Reporting no graph menu keeps snarl from opening it.
    fn has_graph_menu(&mut self, _pos: egui::Pos2, _snarl: &mut Snarl<FlowNodeData>) -> bool {
        false
    }

    fn show_graph_menu(&mut self, _pos: egui::Pos2, _ui: &mut egui::Ui, _snarl: &mut Snarl<FlowNodeData>) {}

    fn has_node_menu(&mut self, node: &FlowNodeData) -> bool {
        !self.locked && self.inner.has_node_menu(node)
    }

    fn show_node_menu(
        &mut self,
        node_id: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<FlowNodeData>,
    ) {
        self.inner.show_node_menu(node_id, inputs, outputs, ui, snarl);
    }

    fn current_transform(&mut self, to_global: &mut TSTransform, _snarl: &mut Snarl<FlowNodeData>) {
        match self.cmd.take() {
            Some(ViewCmd::FitAll) => {
                if let Some(b) = self.bounds
                    && b.is_finite()
                    && self.ui_rect.is_finite()
                {
                    *to_global = fit_transform(b.expand(60.0), self.ui_rect);
                }
            }
            Some(ViewCmd::Center(p)) => {
                if p.x.is_finite() && p.y.is_finite() && self.ui_rect.is_finite() {
                    let s = to_global.scaling;
                    *to_global =
                        TSTransform::new(self.ui_rect.center().to_vec2() - p.to_vec2() * s, s);
                }
            }
            Some(ViewCmd::Focus(p)) => {
                if p.x.is_finite() && p.y.is_finite() && self.ui_rect.is_finite() {
                    // Zoom into a comfortable band: pull a far-out view in, leave a close one be.
                    let s = to_global.scaling.clamp(0.7, 1.2);
                    *to_global =
                        TSTransform::new(self.ui_rect.center().to_vec2() - p.to_vec2() * s, s);
                }
            }
            None => {}
        }
        if self.pan != egui::Vec2::ZERO {
            to_global.translation += self.pan;
        }
        *self.out_transform = *to_global;
    }
}

// ── UI-format export ──────────────────────────────────────────────────────────

impl GraphView {
    /// Serialize the editor graph to ComfyUI **UI-format** JSON (legacy 0.4 shape, which every
    /// frontend opens), so workflows saved from the phone round-trip with the website. Node
    /// positions come from the canvas; measured sizes where known.
    pub fn export_ui(
        &self,
        g: &ComfyUiNodeGraph,
        schemas: &crate::schema::SchemaSet,
        bypassed: &HashSet<NodeId>,
    ) -> serde_json::Value {
        use serde_json::json;

        let node_id = |id: NodeId| id.0 as u64 + 1;
        let mut in_links: HashMap<(NodeId, usize), u64> = HashMap::new();
        let mut out_links: HashMap<(NodeId, usize), Vec<u64>> = HashMap::new();
        let mut link_rows = Vec::new();
        let mut last_link = 0u64;
        for (from, to) in g.snarl.wires() {
            last_link += 1;
            let ty = g
                .snarl
                .get_node(from.node)
                .and_then(|n| n.outputs.get(from.output))
                .map(|o| type_str(&o.typ))
                .unwrap_or_else(|| "*".to_string());
            link_rows.push(json!([
                last_link,
                node_id(from.node),
                from.output,
                node_id(to.node),
                to.input,
                ty
            ]));
            in_links.insert((to.node, to.input), last_link);
            out_links.entry((from.node, from.output)).or_default().push(last_link);
        }

        let mut nodes = Vec::new();
        let mut entries: Vec<_> = g.snarl.nodes_pos_ids().collect();
        entries.sort_by_key(|(id, _, _)| *id);
        let mut last_node = 0u64;
        for (order, (id, pos, data)) in entries.into_iter().enumerate() {
            last_node = last_node.max(node_id(id));
            let schema = schemas.nodes.get(&data.object.name);

            let inputs: Vec<serde_json::Value> = data
                .inputs
                .iter()
                .enumerate()
                .map(|(i, input)| {
                    let link = in_links.get(&(id, i));
                    let mut entry = json!({
                        "name": input.name,
                        "type": type_str(&input.typ),
                        "link": link,
                    });
                    if !input.value.is_connection_only() {
                        entry["widget"] = json!({ "name": input.name });
                    }
                    entry
                })
                .collect();
            let outputs: Vec<serde_json::Value> = data
                .outputs
                .iter()
                .enumerate()
                .map(|(i, out)| {
                    json!({
                        "name": out.name,
                        "type": type_str(&out.typ),
                        "links": out_links.get(&(id, i)).cloned().unwrap_or_default(),
                        "slot_index": i,
                    })
                })
                .collect();

            let mut widgets_values = Vec::new();
            for input in &data.inputs {
                let value = match &input.value {
                    FlowValueType::Array { selected, .. } => json!(selected),
                    FlowValueType::String { value, .. } => json!(value),
                    FlowValueType::Float { value, .. } => json!(value),
                    FlowValueType::SignedInt { value, .. } => json!(value),
                    FlowValueType::UnsignedInt { value, .. } => json!(value),
                    FlowValueType::Boolean(b) => json!(b),
                    _ => continue,
                };
                widgets_values.push(value);
                // The web frontend expects the phantom control value after these ints.
                if schema
                    .and_then(|s| s.inputs.iter().find(|si| si.name == input.name))
                    .is_some_and(crate::uiwf::takes_seed_control)
                {
                    widgets_values.push(json!("fixed"));
                }
            }

            let size = self.sizes.get(&id).copied().unwrap_or(egui::vec2(240.0, 120.0));
            nodes.push(json!({
                "id": node_id(id),
                "type": data.object.name,
                "pos": [pos.x, pos.y],
                "size": [size.x, size.y],
                "flags": {},
                "order": order,
                "mode": if bypassed.contains(&id) { 4 } else { 0 },
                "inputs": inputs,
                "outputs": outputs,
                "properties": { "Node name for S&R": data.object.name },
                "widgets_values": widgets_values,
            }));
        }

        json!({
            "last_node_id": last_node,
            "last_link_id": last_link,
            "nodes": nodes,
            "links": link_rows,
            "groups": [],
            "config": {},
            "extra": {},
            "version": 0.4,
        })
    }
}

/// The server-side name of an [`ObjectType`] (its serde rename; `Other` is untagged).
pub fn type_str(typ: &rucomfyui::object_info::ObjectType) -> String {
    serde_json::to_value(typ)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "*".to_string())
}

// ── Combo / LoRA helpers ──────────────────────────────────────────────────────

/// Fill empty (or under-populated) file combos from `object_info` and the Create-tab LoRA list.
pub fn ensure_file_combos(
    data: &mut FlowNodeData,
    object_info: &rucomfyui::object_info::ObjectInfo,
    lora_files: &[String],
) {
    let class = data.object.name.clone();
    let template = object_info.get(&class);
    for input in &mut data.inputs {
        let from_template = template.and_then(|obj| {
            obj.all_inputs().find(|(n, _, _)| *n == input.name).and_then(|(_, inp, _)| {
                match inp.as_input_type() {
                    rucomfyui::object_info::ObjectInputType::Array(vec) => {
                        let opts: Vec<String> =
                            vec.iter().map(|v| v.as_str().to_string()).collect();
                        (!opts.is_empty()).then_some(opts)
                    }
                    _ => None,
                }
            })
        });
        let is_lora = input.name == "lora_name"
            || class == "LoraLoader"
            || class == "LoraLoaderModelOnly";
        let mut opts = from_template.unwrap_or_default();
        if is_lora {
            for l in lora_files {
                if !opts.iter().any(|o| o == l) {
                    opts.push(l.clone());
                }
            }
        }
        if opts.is_empty() {
            continue;
        }
        match &mut input.value {
            FlowValueType::Array { options, selected } => {
                if options.is_empty() || (is_lora && options.len() < opts.len()) {
                    *options = opts;
                }
                if selected.is_empty() || !options.iter().any(|o| o == selected) {
                    *selected = options.first().cloned().unwrap_or_default();
                }
            }
            // Empty COMBO parsed as a connection pin — promote to a real dropdown.
            other if is_lora && other.is_connection_only() => {
                let selected = opts.first().cloned().unwrap_or_default();
                input.value = FlowValueType::Array { options: opts, selected };
                input.typ = rucomfyui::object_info::ObjectType::String;
            }
            _ => {}
        }
    }
}

fn lora_name_selected(snarl: &Snarl<FlowNodeData>, node: NodeId) -> Option<String> {
    let data = snarl.get_node(node)?;
    data.inputs.iter().find(|i| i.name == "lora_name").and_then(|i| match &i.value {
        FlowValueType::Array { selected, .. } => Some(selected.clone()),
        _ => None,
    })
}

fn lora_name_changed(
    snarl: &Snarl<FlowNodeData>,
    node: NodeId,
    before: Option<&str>,
) -> Option<String> {
    let after = lora_name_selected(snarl, node)?;
    if before == Some(after.as_str()) {
        return None;
    }
    (!after.is_empty()).then_some(after)
}

/// Write catalog strengths onto a LoRA node's strength widgets.
pub fn apply_lora_strengths(data: &mut FlowNodeData, strength_model: f32, strength_clip: f32) {
    for input in &mut data.inputs {
        match (input.name.as_str(), &mut input.value) {
            ("strength_model", FlowValueType::Float { value, min, max, .. }) => {
                *value = (strength_model as f64).clamp(*min, *max);
            }
            ("strength_clip", FlowValueType::Float { value, min, max, .. }) => {
                *value = (strength_clip as f64).clamp(*min, *max);
            }
            _ => {}
        }
    }
}

// ── Node properties editor ────────────────────────────────────────────────────

/// Inspector for one node: type/category header, every input (connection source or editable
/// value), and outputs. Returns `false` when the node no longer exists.
/// `lora_picks` collects `lora_name` changes for recommended-strength application.
pub fn node_properties(
    ui: &mut egui::Ui,
    g: &mut ComfyUiNodeGraph,
    node: NodeId,
    locked: bool,
    lora_files: &[String],
    lora_picks: &mut Vec<LoraPick>,
) -> bool {
    let Some(data) = g.snarl.get_node(node) else { return false };

    // Connection sources, resolved before taking the node mutably.
    let sources: Vec<Option<String>> = (0..data.inputs.len())
        .map(|i| {
            let pin = g.snarl.in_pin(InPinId { node, input: i });
            pin.remotes.first().map(|r| {
                let Some(src) = g.snarl.get_node(r.node) else { return "?".to_string() };
                match src.outputs.get(r.output) {
                    Some(out) => format!("{} / {}", src.object.display_name(), out.name),
                    None => src.object.display_name().to_string(),
                }
            })
        })
        .collect();

    let Some(data) = g.snarl.get_node_mut(node) else { return false };
    ensure_file_combos(data, &g.object_info, lora_files);
    ui.strong(data.object.display_name());
    ui.weak(format!("{}  •  {}", data.object.name, data.object.category));
    if !data.object.description.is_empty() {
        ui.add(
            egui::Label::new(egui::RichText::new(elide(&data.object.description, 300)).weak().small())
                .wrap(),
        );
    }
    ui.separator();

    ui.strong("Inputs");
    for (i, input) in data.inputs.iter_mut().enumerate() {
        match &sources[i] {
            Some(src) => {
                ui.horizontal(|ui| {
                    ui.label(&input.name);
                    ui.weak(format!("<- {}", elide(src, 40)));
                });
            }
            None => {
                let before = match &input.value {
                    FlowValueType::Array { selected, .. } if input.name == "lora_name" => {
                        Some(selected.clone())
                    }
                    _ => None,
                };
                value_editor(ui, egui::Id::new((node, i)), input, locked);
                if input.name == "lora_name"
                    && let FlowValueType::Array { selected, .. } = &input.value
                    && before.as_deref() != Some(selected.as_str())
                    && !selected.is_empty()
                {
                    lora_picks.push(LoraPick { node, file: selected.clone() });
                }
            }
        }
    }

    if !data.outputs.is_empty() {
        ui.add_space(6.0);
        ui.strong("Outputs");
        for out in &data.outputs {
            ui.horizontal(|ui| {
                ui.label(&out.name);
                ui.weak(format!("{:?}", out.typ));
            });
        }
    }
    true
}

/// One editable input row, mirroring the widgets the node body renders.
fn value_editor(ui: &mut egui::Ui, salt: egui::Id, input: &mut FlowInput, locked: bool) {
    ui.add_enabled_ui(!locked, |ui| match &mut input.value {
        FlowValueType::Array { options, selected } => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                option_combo(ui, salt, selected, options);
            });
        }
        FlowValueType::String { value, multiline } => {
            // Label above, field to the visible right edge. Prefer clip_rect over available_width:
            // inside a vertical ScrollArea the latter grows with content and the field overruns.
            ui.label(&input.name);
            let width = (ui.clip_rect().right() - ui.cursor().left() - 8.0).max(48.0);
            let edit = if *multiline {
                egui::TextEdit::multiline(value).desired_rows(3)
            } else {
                egui::TextEdit::singleline(value)
            };
            ui.scope(|ui| {
                ui.set_max_width(width);
                ui.add(edit.desired_width(width).clip_text(true));
            });
        }
        FlowValueType::Float { value, min, max, step, .. } => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.add(
                    egui::DragValue::new(value).range(*min..=*max).speed(step.max(0.001)),
                );
            });
        }
        FlowValueType::SignedInt { value, min, max, step } => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.add(
                    egui::DragValue::new(value).range(*min..=*max).speed((*step as f64).max(1.0)),
                );
            });
        }
        FlowValueType::UnsignedInt { value, min, max, step } => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.add(
                    egui::DragValue::new(value).range(*min..=*max).speed((*step as f64).max(1.0)),
                );
            });
        }
        FlowValueType::Boolean(value) => {
            ui.checkbox(value, &input.name);
        }
        _ => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.weak("connection");
            });
        }
    });
}

/// Dropdown over a possibly-huge option list: filters by substring and caps rendered rows.
fn option_combo(ui: &mut egui::Ui, salt: egui::Id, selected: &mut String, options: &[String]) {
    egui::ComboBox::from_id_salt(salt)
        .selected_text(elide(selected, 32))
        .show_ui(ui, |ui| {
            let filter_id = salt.with("filter");
            let mut filter: String =
                ui.ctx().data_mut(|d| d.get_temp(filter_id)).unwrap_or_default();
            if options.len() > 12 {
                ui.add(egui::TextEdit::singleline(&mut filter).hint_text("filter"));
                ui.ctx().data_mut(|d| d.insert_temp(filter_id, filter.clone()));
            }
            let f = filter.to_lowercase();
            let mut shown = 0usize;
            for opt in options {
                if !f.is_empty() && !opt.to_lowercase().contains(&f) {
                    continue;
                }
                if shown >= 200 {
                    ui.weak("… type to narrow down");
                    break;
                }
                shown += 1;
                ui.selectable_value(selected, opt.clone(), elide(opt, 48));
            }
            if shown == 0 {
                ui.weak("no matches");
            }
        });
}

/// Shorten a string for display so a pathological value can't blow up text layout.
pub fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Drop control chars and replace glyphs the active font cannot draw.
pub fn sanitize_ui_text(ui: &egui::Ui, s: &str) -> String {
    let font = egui::TextStyle::Body.resolve(ui.style());
    ui.ctx().fonts_mut(|fonts| {
        s.chars()
            .map(|c| {
                if c.is_control() {
                    ' '
                } else if fonts.has_glyph(&font, c) {
                    c
                } else {
                    '?'
                }
            })
            .collect()
    })
}

/// Truncate `s` so its laid-out width fits within `max_width` (appends `…` when cut).
pub fn elide_width(ui: &egui::Ui, s: &str, max_width: f32) -> String {
    if s.is_empty() {
        return String::new();
    }
    if max_width <= 12.0 {
        return "…".into();
    }
    let font = egui::TextStyle::Body.resolve(ui.style());
    let measure = |text: &str| {
        ui.ctx()
            .fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE).size().x)
    };
    if measure(s) <= max_width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let candidate: String = chars[..mid].iter().chain(std::iter::once(&'…')).collect();
        if measure(&candidate) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        "…".into()
    } else {
        chars[..lo].iter().chain(std::iter::once(&'…')).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Headless repro harness: load a real workflow (fixture env vars) and sweep taps across the
    /// canvas — egui hit-testing runs on the pointer events, so widget-soup panics surface here.
    #[test]
    fn tap_sweep_over_loaded_workflow() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = crate::schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));

        for wf_path in wf_paths.split(':') {
            let ui_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(wf_path).unwrap()).unwrap();
            let converted = crate::uiwf::convert(&ui_json, &schemas).unwrap();
            graph.load_api_workflow(&converted.workflow).unwrap();

            let mut view = GraphView::default();
            view.request_fit();
            let ctx = egui::Context::default();
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 840.0));

            let mut frame_no = 0u32;
            let mut frame = |view: &mut GraphView,
                             graph: &mut ComfyUiNodeGraph,
                             events: Vec<egui::Event>|
             -> Option<NodeId> {
                frame_no += 1;
                let desc = format!("{events:?}");
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                };
                let mut tapped = None;
                let _ = ctx.run_ui(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        tapped = view.show(ui, graph, None, None, &HashSet::new(), &[]);
                    });
                });
                for (id, pos, data) in graph.snarl.nodes_pos_ids() {
                    assert!(
                        pos.x.is_finite() && pos.y.is_finite(),
                        "frame {frame_no} ({desc}): node {id:?} ({}) pos went NaN",
                        data.object.name
                    );
                }
                for (id, size) in view.sizes.iter() {
                    assert!(
                        size.x.is_finite() && size.y.is_finite(),
                        "frame {frame_no} ({desc}): node {id:?} size went NaN: {size:?}"
                    );
                }
                assert!(
                    view.to_global.scaling.is_finite() && view.to_global.translation.x.is_finite(),
                    "frame {frame_no} ({desc}): transform NaN: {:?}",
                    view.to_global
                );
                tapped
            };
            let tap = |view: &mut GraphView,
                       graph: &mut ComfyUiNodeGraph,
                       frame: &mut dyn FnMut(
                &mut GraphView,
                &mut ComfyUiNodeGraph,
                Vec<egui::Event>,
            ) -> Option<NodeId>,
                       pos: egui::Pos2|
             -> Option<NodeId> {
                frame(view, graph, vec![egui::Event::PointerMoved(pos)]);
                frame(view, graph, vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                }]);
                frame(view, graph, vec![
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    },
                    egui::Event::PointerGone,
                ])
            };

            frame(&mut view, &mut graph, vec![]);
            frame(&mut view, &mut graph, vec![]);
            // 5x9 tap grid over the canvas: press, release, lift between taps.
            for gy in 0..9 {
                for gx in 0..5 {
                    let pos = egui::pos2(30.0 + gx as f32 * 90.0, 40.0 + gy as f32 * 88.0);
                    tap(&mut view, &mut graph, &mut frame, pos);
                }
            }
            // Dismiss any popup a sweep tap left open, then targeted-tap a known node header:
            // it must focus exactly that node.
            for pressed in [true, false] {
                frame(&mut view, &mut graph, vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::default(),
                }]);
            }
            // Sweep taps may have hit the minimap and panned away; re-fit first.
            view.request_fit();
            frame(&mut view, &mut graph, vec![]);
            // Tap a node whose interior point is on-screen, clear of the corner overlays (minimap
            // top-left, lock top-right), and unambiguously that node (no earlier node covers it).
            let sizes = view.sizes.clone();
            let size_of = |id: NodeId| sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
            let safe = |p: egui::Pos2| -> bool {
                screen.shrink(8.0).contains(p)
                    && !(p.x < 220.0 && p.y < 220.0)
                    && !(p.x > screen.right() - 60.0 && p.y < 60.0)
            };
            let mut target = None;
            for (id, node_pos, _) in graph.snarl.nodes_pos_ids() {
                let size = size_of(id);
                if size.x < 30.0 || size.y < 26.0 {
                    continue;
                }
                let interior = node_pos + egui::vec2(size.x * 0.4, 13.0);
                let first = graph.snarl.nodes_pos_ids().find(|(id2, p2, _)| {
                    egui::Rect::from_min_size(*p2, size_of(*id2)).contains(interior)
                });
                if first.map(|(i, _, _)| i) == Some(id) && safe(view.to_global * interior) {
                    target = Some((id, view.to_global * interior));
                    break;
                }
            }
            let (want, screen_pt) = target.expect("no unobstructed node to tap");
            let tapped = tap(&mut view, &mut graph, &mut frame, screen_pt);
            assert_eq!(tapped, Some(want), "{wf_path}: targeted tap missed its node");
            println!("{wf_path}: tap sweep ok");
        }
    }

    /// Canonical text for a constant input value: exact for integers, numeric-collapsed for
    /// integral floats (the editor turns `I64(1)` into `F64(1.0)` on float inputs).
    fn norm_value(v: &rucomfyui::workflow::WorkflowInput) -> Option<String> {
        use rucomfyui::workflow::WorkflowInput as W;
        match v {
            W::I64(i) => Some(format!("n{i}")),
            W::U64(u) => Some(format!("n{u}")),
            W::F64(f) if f.fract() == 0.0 && f.abs() < 9e15 => Some(format!("n{}", *f as i64)),
            W::F64(f) => Some(format!("f{f}")),
            W::String(s) => Some(format!("s{s}")),
            W::Boolean(b) => Some(format!("b{b}")),
            _ => None,
        }
    }

    /// Loading a workflow into the editor and saving it straight back must preserve every widget
    /// value. Regression: the editor's u64 heuristic used to wrap `stop_at_clip_layer: -2` into
    /// 18446744073709551614, which the server rejected.
    #[test]
    fn editor_round_trip_preserves_values() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = crate::schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        let is_widget_input = |class: &str, input: &str| {
            schemas.nodes.get(class).is_some_and(|n| {
                n.inputs.iter().any(|i| {
                    i.name == input
                        && !matches!(
                            i.kind,
                            crate::schema::InputKind::Connection { .. }
                                | crate::schema::InputKind::Opaque
                        )
                })
            })
        };
        let collect = |wf: &rucomfyui::Workflow| {
            let mut multiset: HashMap<(String, String, String), i32> = HashMap::new();
            for node in wf.0.values() {
                for (name, input) in &node.inputs {
                    if !is_widget_input(&node.class_type, name) {
                        continue;
                    }
                    if let Some(v) = norm_value(input) {
                        *multiset
                            .entry((node.class_type.clone(), name.clone(), v))
                            .or_default() += 1;
                    }
                }
            }
            multiset
        };

        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        for wf_path in wf_paths.split(':') {
            let ui_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(wf_path).unwrap()).unwrap();
            let converted = crate::uiwf::convert(&ui_json, &schemas).unwrap();
            graph.load_api_workflow(&converted.workflow).unwrap();
            let saved = graph.save_api_workflow();
            let (source, round) = (collect(&converted.workflow), collect(&saved));
            for ((class, input, value), count) in &source {
                let got = round.get(&(class.clone(), input.clone(), value.clone())).copied();
                assert_eq!(
                    got,
                    Some(*count),
                    "{wf_path}: {class}.{input} lost value {value} in the editor round trip"
                );
            }
            println!("{wf_path}: {} values survive the round trip", source.len());
        }
    }

    /// Arrange must produce non-overlapping nodes even with wildly varying node sizes.
    #[test]
    fn arrange_never_overlaps() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = crate::schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        for wf_path in wf_paths.split(':') {
            let ui_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(wf_path).unwrap()).unwrap();
            let converted = crate::uiwf::convert(&ui_json, &schemas).unwrap();
            graph.load_api_workflow(&converted.workflow).unwrap();

            // Deterministic pseudo-varied sizes standing in for measured ones.
            let sizes: HashMap<NodeId, egui::Vec2> = graph
                .snarl
                .nodes_pos_ids()
                .enumerate()
                .map(|(i, (id, _, _))| {
                    (id, egui::vec2(150.0 + (i * 37 % 250) as f32, 60.0 + (i * 53 % 400) as f32))
                })
                .collect();
            let rects = arrange(&mut graph.snarl, &sizes);
            for (i, a) in rects.iter().enumerate() {
                for b in &rects[i + 1..] {
                    assert!(
                        !a.shrink(1.0).intersects(b.shrink(1.0)),
                        "{wf_path}: nodes overlap after arrange: {a:?} vs {b:?}"
                    );
                }
            }
            // Positions were actually applied to the snarl.
            let applied = graph.snarl.nodes_pos_ids().all(|(id, pos, _)| {
                rects.iter().any(|r| (r.min - pos).length() < 0.5) || sizes.get(&id).is_none()
            });
            assert!(applied, "{wf_path}: arrange did not move nodes");

            // Execution flows left-to-right: a consumer sits right of its producer. A converted
            // workflow can still contain back-edges (SetNode/GetNode and "Anything Everywhere"
            // links reconstruct into cycles), so require forward flow to dominate rather than be
            // absolute — the backbone reads as order of execution.
            let pos_of: HashMap<NodeId, egui::Pos2> =
                graph.snarl.nodes_pos_ids().map(|(id, pos, _)| (id, pos)).collect();
            let (mut forward, mut total) = (0u32, 0u32);
            for (from, to) in graph.snarl.wires() {
                if from.node == to.node {
                    continue;
                }
                let (Some(a), Some(b)) = (pos_of.get(&from.node), pos_of.get(&to.node)) else {
                    continue;
                };
                total += 1;
                if b.x > a.x {
                    forward += 1;
                }
            }
            if total > 0 {
                assert!(
                    forward * 10 >= total * 8,
                    "{wf_path}: only {forward}/{total} wires flow left-to-right"
                );
            }
        }
    }

    /// Export to UI format re-converts to the same API workflow the editor holds.
    #[test]
    fn export_ui_reimports_cleanly() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = crate::schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        for wf_path in wf_paths.split(':') {
            let ui_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(wf_path).unwrap()).unwrap();
            let converted = crate::uiwf::convert(&ui_json, &schemas).unwrap();
            graph.load_api_workflow(&converted.workflow).unwrap();
            let editor_wf = graph.save_api_workflow();

            let view = GraphView::default();
            let bypassed = HashSet::new();
            let exported = view.export_ui(&graph, &schemas, &bypassed);
            let reimported = crate::uiwf::convert(&exported, &schemas)
                .unwrap_or_else(|e| panic!("{wf_path}: exported UI json failed to convert: {e}"));
            assert_eq!(
                reimported.workflow.0.len(),
                editor_wf.0.len(),
                "{wf_path}: exported workflow node count changed"
            );
            for w in &reimported.warnings {
                assert!(
                    !w.contains("unused widget"),
                    "{wf_path}: export produced misaligned widgets: {w}"
                );
            }
            println!("{wf_path}: export/reimport ok ({} nodes)", editor_wf.0.len());
        }
    }

    #[test]
    fn fit_maps_view_center_to_screen_center_and_clamps_scale() {
        let view = egui::Rect::from_min_size(egui::pos2(1000.0, 2000.0), egui::vec2(4000.0, 2000.0));
        let ui = egui::Rect::from_min_size(egui::pos2(0.0, 100.0), egui::vec2(400.0, 600.0));
        let tf = fit_transform(view, ui);
        let mapped = tf * view.center();
        assert!((mapped - ui.center()).length() < 0.01);
        assert!((tf.scaling - 0.1).abs() < 1e-6, "400/4000 wins over 600/2000");

        let tiny = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0));
        assert_eq!(fit_transform(tiny, ui).scaling, MAX_SCALE);
        let huge = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1e6, 1e6));
        assert_eq!(fit_transform(huge, ui).scaling, MIN_SCALE);
    }

    /// `arrange_now` must move nodes without waiting for canvas size measures.
    #[test]
    fn arrange_now_compacts_without_measured_sizes() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"A": {"input": {"required": {"in": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        let a = snarl.insert_node(egui::pos2(0.0, 0.0), FlowNodeData::new(obj.clone()));
        let b = snarl.insert_node(egui::pos2(0.0, 400.0), FlowNodeData::new(obj.clone()));
        let c = snarl.insert_node(egui::pos2(600.0, 0.0), FlowNodeData::new(obj));
        snarl.connect(
            egui_snarl::OutPinId { node: a, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        snarl.connect(
            egui_snarl::OutPinId { node: b, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        let before: HashMap<NodeId, egui::Pos2> =
            snarl.nodes_pos_ids().map(|(id, pos, _)| (id, pos)).collect();
        let mut view = GraphView::new(1);
        view.arrange_now(&mut snarl);
        let moved = snarl.nodes_pos_ids().any(|(id, pos, _)| before.get(&id) != Some(&pos));
        assert!(moved, "arrange_now left every node in place");
        assert!(view.sizes.is_empty(), "arrange_now must not fake measured sizes");
        // Consumer sits to the right of its producers.
        let pos = |id| snarl.get_node_info(id).unwrap().pos;
        assert!(pos(c).x > pos(a).x);
        assert!(pos(c).x > pos(b).x);
    }

    #[test]
    fn mark_needs_auto_arrange_defers_until_applied() {
        let mut view = GraphView::new(3);
        view.mark_needs_auto_arrange();
        assert!(view.needs_auto_arrange);
        assert!(view.arrange_pending());
        assert!(!view.arrange_queued);
    }

    /// Load path must queue a refine pass; seeding nominal sizes used to make that pass a no-op.
    #[test]
    fn arrange_on_load_queues_refine_without_faking_sizes() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"A": {"input": {"required": {"in": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        let a = snarl.insert_node(egui::pos2(0.0, 0.0), FlowNodeData::new(obj.clone()));
        let c = snarl.insert_node(egui::pos2(800.0, 0.0), FlowNodeData::new(obj));
        snarl.connect(
            egui_snarl::OutPinId { node: a, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        let mut view = GraphView::new(2);
        view.arrange_on_load(&mut snarl);
        assert!(view.arrange_queued, "load must queue a measured refine pass");
        assert!(view.sizes.is_empty(), "nominal placeholders must not mark sizes ready");
        // Simulate canvas measures, then the refine arrange that show() would run.
        view.sizes.insert(a, egui::vec2(220.0, 360.0));
        view.sizes.insert(c, egui::vec2(220.0, 360.0));
        let before = snarl.get_node_info(c).unwrap().pos;
        view.arrange_now(&mut snarl);
        let after = snarl.get_node_info(c).unwrap().pos;
        assert_ne!(before, after, "refine with tall measured sizes must re-pack");
        assert!(after.x > snarl.get_node_info(a).unwrap().pos.x);
    }

    #[test]
    fn first_node_prefers_leftmost_root() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"A": {"input": {"required": {"in": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        let a = snarl.insert_node(egui::pos2(500.0, 0.0), FlowNodeData::new(obj.clone()));
        let b = snarl.insert_node(egui::pos2(100.0, 0.0), FlowNodeData::new(obj.clone()));
        let c = snarl.insert_node(egui::pos2(-50.0, 0.0), FlowNodeData::new(obj));
        // b -> a and b -> c: roots are b (x=100); c has an input so the leftmost node loses.
        snarl.connect(
            egui_snarl::OutPinId { node: b, output: 0 },
            egui_snarl::InPinId { node: a, input: 0 },
        );
        snarl.connect(
            egui_snarl::OutPinId { node: b, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        assert_eq!(first_node_pos(&snarl), Some(egui::pos2(100.0, 0.0)));
    }
}
