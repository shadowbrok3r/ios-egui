//! The comfyui-android canvas look and auto-layout, lifted onto wirelab's flow graph.
//!
//! `wirelab_flow_ui::show` draws with a bare `SnarlStyle::new()`. Everything here is styling and
//! geometry over `Snarl<T>` alone — no knowledge of the node payload — so the plugin can drive it
//! against `wirelab_core::flow::NodeKind` without the shared crate changing.

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

use egui_ios_plugin_sdk::egui;
use egui::emath::TSTransform;
use wirelab_flow_ui::egui_snarl::{
    InPin, InPinId, NodeId, OutPin, OutPinId, Snarl,
    ui::{
        AnyPins, BackgroundPattern, NodeLayout, PinPlacement, PinWireInfo, SelectionStyle, SnarlPin,
        SnarlStyle, SnarlViewer, WireStyle,
    },
};

use crate::theme;

/// Zoom clamps, so a fit of a wide graph can't shrink nodes past legibility or blow one node up
/// to fill the canvas.
pub const MIN_SCALE: f32 = 0.05;
pub const MAX_SCALE: f32 = 2.5;

/// Stand-in extent for a node egui hasn't measured yet. Layout runs before the first paint, so
/// every node is laid out at this size and the result is loose rather than overlapping.
const NOMINAL_NODE: egui::Vec2 = egui::vec2(180.0, 100.0);

/// AMOLED canvas, bold axis-aligned traces, pins outside the node body.
pub fn style() -> SnarlStyle {
    let mut s = SnarlStyle::new();
    s.bg_frame = Some(egui::Frame::new().fill(theme::CANVAS));
    // The dot grid is painted by `draw_background`; snarl's own pattern is never consulted.
    s.bg_pattern = Some(BackgroundPattern::NoPattern);
    s.min_scale = Some(MIN_SCALE);
    s.max_scale = Some(MAX_SCALE);
    s.centering = Some(true);
    // Orthogonal wires with rounded corners — a structured "network diagram" look instead of
    // droopy beziers, and easier to follow where they run.
    s.wire_style = Some(WireStyle::AxisAligned { corner_radius: 8.0 });
    s.wire_width = Some(2.6);
    // Pins sit just outside the node body: their dots stop overlapping the pin labels, and they
    // become finger targets clear of the draggable node frame.
    s.pin_placement = Some(PinPlacement::Outside { margin: PIN_MARGIN });
    s.pin_size = Some(PIN_DOT);
    s.pin_fill = Some(egui::Color32::from_rgba_unmultiplied(43, 226, 214, 220));
    // `pin_info` sets a per-type fill but never a stroke, so this rim reaches every dot.
    s.pin_stroke = Some(egui::Stroke::new(1.6, theme::RIM_BRIGHT));
    // Snarl's default is `Frame::window`, which pulls in the opaque window fill.
    s.node_frame = Some(node_frame());
    // Left None it would be `node_frame` again, and the two translucent fills would stack.
    s.header_frame = Some(header_frame());
    s.select_style = Some(SelectionStyle {
        margin: egui::Margin::same(4),
        rounding: egui::CornerRadius::same(10),
        fill: egui::Color32::from_rgba_unmultiplied(255, 61, 139, 26),
        stroke: egui::Stroke::new(2.0, theme::PINK),
    });
    // Inputs above outputs rather than side by side. Snarl's default Coil layout measures both
    // columns against the full node width and sums them, so every node carries its output labels'
    // width as dead weight — which `arrange` then spaces its columns by.
    // The row floor keeps enlarged hit rects on adjacent pins from stacking on top of each other.
    s.node_layout = Some(NodeLayout::sandwich().with_min_pin_row_height(PIN_ROW));
    s
}

/// A node body: glass over the lit canvas, a neutral hairline, and a shadow to lift it off.
fn node_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .corner_radius(NODE_CORNER)
        .inner_margin(6)
        .stroke(egui::Stroke::new(1.0, theme::RIM_BRIGHT))
        .shadow(egui::epaint::Shadow {
            offset: [0, 2],
            blur: 12,
            spread: 2,
            color: egui::Color32::from_black_alpha(200),
        })
}

/// A node title bar: a faint aqua wash banding the top of the body.
fn header_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(HEADER_FILL)
        .corner_radius(egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 })
        .inner_margin(egui::Margin { left: 8, right: 8, top: 5, bottom: 5 })
}

/// Base spacing (graph units) of the canvas dot grid — anchored in graph space so it scales with
/// the nodes; coarsened by powers of two when zoomed far out.
const DOT_SPACING: f32 = 28.0;
const DOT_RADIUS: f32 = 1.7;
pub const DOT_COLOR: egui::Color32 = egui::Color32::from_rgb(30, 70, 74);

const NODE_CORNER: f32 = 8.0;
const HEADER_FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(3, 14, 13, 16);
/// Glow under a pin dot.
const PIN_HALO: egui::Color32 = egui::Color32::from_rgba_premultiplied(5, 27, 25, 30);

/// Drawn diameter of a pin dot, in graph units.
const PIN_DOT: f32 = 20.0;
/// Gap between a pin dot and the node body, in graph units.
const PIN_MARGIN: f32 = 10.0;
/// Pitch snarl spaces pin rows at, in graph units.
const PIN_ROW: f32 = 32.0;
/// Drag target the enlarged hit rect aims at, in screen points.
const PIN_TOUCH: f32 = 44.0;
/// Ceiling on the hit rect in graph units. Unbounded, `PIN_TOUCH / MIN_SCALE` is 880 units — one
/// pin would cover the whole node.
const PIN_TOUCH_MAX: f32 = PIN_DOT * 3.0;
/// Widest a centred hit rect can be without reaching past the node edge: snarl centres the dot
/// `PIN_MARGIN + PIN_DOT/2` out from the body, and the rect grows both ways.
const PIN_HIT_W_MAX: f32 = 2.0 * (PIN_MARGIN + PIN_DOT * 0.5);

/// Side of a pin's hit square in graph units: a roughly constant on-screen target, capped so one
/// pin can't cover the node it belongs to.
fn pin_hit(scaling: f32) -> f32 {
    (PIN_TOUCH / scaling.max(MIN_SCALE)).clamp(PIN_DOT, PIN_TOUCH_MAX)
}

/// Widens a pin's drag target without widening its dot.
///
/// Snarl uses one rect for both: `pin_rect` is handed to `interact` and then straight on to
/// `draw`, and it is in graph units, so at a fit-all zoom of ~0.3 a 20-unit pin is a 6pt target.
/// Overriding `pin_rect` and shrinking the rect back before delegating `draw` separates the two.
struct FatPin<P> {
    inner: P,
    hit: f32,
    dot: f32,
}

/// A pin's hit square, centred on the dot — snarl takes the wire endpoint from `rect.center()` —
/// and bounded so it reaches neither into the node body nor over the next pin row.
fn pin_hit_rect(x: f32, y0: f32, y1: f32, hit: f32, dot: f32) -> egui::Rect {
    // Taller than the row and adjacent rects overlap, where egui hands the tap to the later pin
    // and the wire starts from the wrong port.
    let h = hit.min((y1 - y0).max(dot));
    egui::Rect::from_center_size(egui::pos2(x, (y0 + y1) * 0.5), egui::vec2(hit.min(PIN_HIT_W_MAX), h))
}

impl<P: SnarlPin> SnarlPin for FatPin<P> {
    fn pin_rect(&self, x: f32, y0: f32, y1: f32, _size: f32) -> egui::Rect {
        pin_hit_rect(x, y0, y1, self.hit, self.dot)
    }

    fn draw(
        self,
        snarl_style: &SnarlStyle,
        style: &egui::Style,
        rect: egui::Rect,
        painter: &egui::Painter,
    ) -> PinWireInfo {
        // Snarl pops the rect 1.2x on hover; carry that factor onto the dot.
        let k = (rect.width() / self.hit.min(PIN_HIT_W_MAX)).max(1.0);
        let visual =
            egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(self.dot * k));
        painter.circle_filled(visual.center(), visual.width() * 0.85, PIN_HALO);
        let mut info = self.inner.draw(snarl_style, style, visual, painter);
        // The shared viewer's type colours are desktop-tuned; lift them for the black page.
        info.color = info.color.gamma_multiply(1.15);
        info
    }
}

/// A one-shot view move, applied by the next [`Styled`] pass.
#[derive(Clone, Copy, PartialEq)]
pub enum ViewCmd {
    /// Fit every node on screen.
    FitAll,
    /// Centre on a graph-space point, keeping the current zoom.
    Center(egui::Pos2),
}

/// View state the wrapper reads and writes across frames: the canvas rect and transform the last
/// pass drew with, the node bounds it measured, and a pending [`ViewCmd`].
///
/// Snarl owns the live transform, so a fit or a pan can only be applied from inside
/// [`SnarlViewer::current_transform`] — which is why this is a separate object the plugin keeps
/// beside its `FlowView` rather than something callable directly.
pub struct Canvas {
    cmd: Option<ViewCmd>,
    ui_rect: egui::Rect,
    to_global: TSTransform,
    bounds: Option<egui::Rect>,
    /// Node extents measured by the previous pass, for hit-testing a press against a node.
    sizes: HashMap<NodeId, egui::Vec2>,
    /// Filled by `final_node_rect` during a pass, promoted to `sizes` at the next one.
    sizes_next: HashMap<NodeId, egui::Vec2>,
    /// Press being timed for a long-press: (start time, screen position).
    press: Option<(f64, egui::Pos2)>,
    long_fired: bool,
    /// Graph-space position the open graph menu was summoned at.
    menu: Option<egui::Pos2>,
    menu_open: bool,
    /// Set while the finger that summoned the menu is still down; suppresses the close write-back.
    menu_hold: bool,
}

impl Default for Canvas {
    fn default() -> Self {
        // NOTHING, not ZERO: an empty rect reads as "never painted" for `ready`, and a fit
        // against a zero-sized rect would divide the scale to nothing.
        Self {
            cmd: None,
            ui_rect: egui::Rect::NOTHING,
            to_global: TSTransform::IDENTITY,
            bounds: None,
            sizes: HashMap::new(),
            sizes_next: HashMap::new(),
            press: None,
            long_fired: false,
            menu: None,
            menu_open: false,
            menu_hold: false,
        }
    }
}

impl Canvas {
    /// Fit every node on screen at the next paint.
    pub fn fit(&mut self) {
        self.cmd = Some(ViewCmd::FitAll);
    }

    /// Pan to the graph's first node — the leftmost (or topmost) with nothing feeding it.
    pub fn go_to_start<T>(&mut self, snarl: &Snarl<T>, vertical: bool) {
        if let Some(p) = first_node_pos(snarl, vertical) {
            self.cmd = Some(ViewCmd::Center(p));
        }
    }

    /// Whether the canvas has been painted at least once (so fit/minimap have real numbers).
    pub fn ready(&self) -> bool {
        self.ui_rect.is_finite() && self.ui_rect.width() > 1.0
    }

    /// The graph laid out inside a small overview panel, with the current viewport drawn on it.
    /// Tap or drag anywhere on it to centre the canvas there.
    pub fn minimap<T>(&mut self, ui: &mut egui::Ui, snarl: &Snarl<T>, size: egui::Vec2) {
        let (Some(graph), true) = (self.bounds, self.ready()) else { return };
        if graph.width() < 1.0 || graph.height() < 1.0 {
            return;
        }
        let (resp, painter) = ui.allocate_painter(size, egui::Sense::click_and_drag());
        let rect = resp.rect;
        theme::glass_shadow(&painter, rect, 8);
        theme::glass(&painter, rect, 8.0, egui::Color32::from_black_alpha(180), theme::RIM_BRIGHT);
        // One uniform scale for both axes, so the overview isn't a distorted squash of the graph.
        let pad = graph.expand(40.0);
        let scale = (rect.width() / pad.width()).min(rect.height() / pad.height());
        let to_map = |p: egui::Pos2| rect.center() + (p - pad.center()) * scale;
        for (_, pos, _) in snarl.nodes_pos_ids() {
            let r = egui::Rect::from_min_size(pos, NOMINAL_NODE);
            let mapped = egui::Rect::from_min_max(to_map(r.min), to_map(r.max));
            // A 2px floor, or a zoomed-out graph maps every node to nothing.
            let mapped = egui::Rect::from_center_size(
                mapped.center(),
                mapped.size().max(egui::Vec2::splat(2.0)),
            );
            painter.rect_filled(mapped, 1.0, egui::Color32::from_gray(150));
        }
        // The visible canvas, back-projected into graph space.
        let inv = self.to_global.inverse();
        let view = egui::Rect::from_min_max(
            inv * self.ui_rect.min,
            inv * self.ui_rect.max,
        );
        painter.rect_stroke(
            egui::Rect::from_min_max(to_map(view.min), to_map(view.max)),
            0.0,
            egui::Stroke::new(1.5, theme::AQUA),
            egui::StrokeKind::Inside,
        );
        if let Some(p) = resp.interact_pointer_pos() {
            self.cmd = Some(ViewCmd::Center(pad.center() + (p - rect.center()) / scale));
        }
    }

    /// The node under a screen position, using the extents the previous pass measured.
    fn node_at<T>(&self, snarl: &Snarl<T>, screen: egui::Pos2) -> Option<NodeId> {
        let g = self.to_global.inverse() * screen;
        snarl
            .nodes_pos_ids()
            .find(|(id, pos, _)| {
                let size = self.sizes.get(id).copied().unwrap_or(NOMINAL_NODE);
                egui::Rect::from_min_size(*pos, size).contains(g)
            })
            .map(|(id, _, _)| id)
    }

    /// Graph-space position of a long-press on empty canvas, once per press.
    fn long_press<T>(&mut self, ctx: &egui::Context, snarl: &Snarl<T>) -> Option<egui::Pos2> {
        if !self.ready() {
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
        // Off-canvas, or over an overlay — the minimap sits on Order::Foreground.
        if !self.ui_rect.contains(pos)
            || ctx.layer_id_at(pos).is_some_and(|l| l.order != egui::Order::Background)
        {
            self.press = None;
            return None;
        }
        // Nodes are drawn on the same layer order as the canvas, so they need an explicit
        // hit-test; a press on one belongs to snarl's node menu.
        if self.node_at(snarl, pos).is_some() {
            self.press = None;
            return None;
        }
        match self.press {
            None => {
                self.press = Some((time, pos));
                None
            }
            Some((start, origin)) => {
                if dragging || (origin - pos).length() > 12.0 {
                    self.press = None;
                    return None;
                }
                ctx.request_repaint();
                if !self.long_fired && time - start > 0.5 {
                    self.long_fired = true;
                    return Some(self.to_global.inverse() * pos);
                }
                None
            }
        }
    }
}

/// The graph's add-node menu, opened by long-press and kept open until dismissed.
///
/// Snarl only calls `.context_menu()` while a pointer position exists (its `ui.rs:1291` guard), so
/// on touch the popup is garbage-collected the frame the finger lifts. Driving an egui `Popup` from
/// a bool this side of the ABI re-shows it every pass, which is what keeps it alive.
///
/// A long-press while the menu is already open moves it to the new spot. That press lands outside
/// the popup, so `CloseOnClickOutside` would shut it on the finger-lift and the menu would blink to
/// the new position and vanish — `menu_hold` drops the close write-back until that press ends.
pub fn graph_menu<T, V: SnarlViewer<T>>(
    ui: &mut egui::Ui,
    canvas: &mut Canvas,
    viewer: &mut V,
    snarl: &mut Snarl<T>,
) {
    if let Some(pos) = canvas.long_press(ui.ctx(), snarl)
        && viewer.has_graph_menu(pos, snarl)
    {
        canvas.menu = Some(pos);
        canvas.menu_open = true;
        canvas.menu_hold = true;
    }
    let Some(graph_pos) = canvas.menu else { return };
    let mut open = canvas.menu_open;
    egui::Popup::new(
        egui::Id::new("wirelab-flow-graph-menu"),
        ui.ctx().clone(),
        egui::PopupAnchor::Position(canvas.to_global * graph_pos),
        ui.layer_id(),
    )
    .open_bool(&mut open)
    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
    .show(|ui| viewer.show_graph_menu(graph_pos, ui, snarl));
    if canvas.menu_hold {
        // The lift is the frame the click registers, so clear the hold only after skipping it.
        canvas.menu_hold = ui.ctx().input(|i| i.pointer.any_down());
        return;
    }
    canvas.menu_open = open;
    if !open {
        canvas.menu = None;
    }
}

/// The transform that fits `view` (graph space) into `ui_rect` (screen space), scale clamped.
fn fit_transform(view: egui::Rect, ui_rect: egui::Rect) -> TSTransform {
    let scale = (ui_rect.size() / view.size()).min_elem().clamp(MIN_SCALE, MAX_SCALE);
    TSTransform::new(ui_rect.center().to_vec2() - view.center().to_vec2() * scale, scale)
}

/// Position of the graph's first node: the leftmost (topmost when `vertical`) node with nothing
/// wired into it, falling back to the leading node overall.
pub fn first_node_pos<T>(snarl: &Snarl<T>, vertical: bool) -> Option<egui::Pos2> {
    let fed: HashSet<NodeId> = snarl.wires().map(|(_, to)| to.node).collect();
    let along = |p: egui::Pos2| if vertical { p.y } else { p.x };
    let leading = |it: &mut dyn Iterator<Item = egui::Pos2>| -> Option<egui::Pos2> {
        it.fold(None, |best: Option<egui::Pos2>, p| match best {
            Some(b) if along(b) <= along(p) => Some(b),
            _ => Some(p),
        })
    };
    leading(&mut snarl.nodes_pos_ids().filter(|(id, _, _)| !fed.contains(id)).map(|(_, p, _)| p))
        .or_else(|| leading(&mut snarl.nodes_pos_ids().map(|(_, p, _)| p)))
}

/// Wraps any [`SnarlViewer`] to add the canvas chrome the node payload knows nothing about: the
/// dot grid, and the view moves that can only be applied from inside `current_transform`.
///
/// Every other method forwards untouched, so the wrapped viewer keeps full control of its nodes.
pub struct Styled<'a, T, V: SnarlViewer<T>> {
    inner: &'a mut V,
    canvas: &'a mut Canvas,
    /// Node bounds measured before the pass, published back through `current_transform`.
    bounds: Option<egui::Rect>,
    marker: PhantomData<T>,
}

impl<'a, T, V: SnarlViewer<T>> Styled<'a, T, V> {
    pub fn new(inner: &'a mut V, canvas: &'a mut Canvas, snarl: &Snarl<T>) -> Self {
        let bounds = bounds(snarl);
        Self { inner, canvas, bounds, marker: PhantomData }
    }

    fn pin_hit(&self) -> f32 {
        pin_hit(self.canvas.to_global.scaling)
    }
}

impl<T, V: SnarlViewer<T>> SnarlViewer<T> for Styled<'_, T, V> {
    fn draw_background(
        &mut self,
        _background: Option<&BackgroundPattern>,
        viewport: &egui::Rect,
        _snarl_style: &SnarlStyle,
        _style: &egui::Style,
        painter: &egui::Painter,
        _snarl: &Snarl<T>,
    ) {
        // Light pools under the nodes: without them the translucent node fills read as flat
        // plastic. Anchored to the viewport, so the light stays put as the graph pans under it.
        theme::ambience(painter, *viewport, 3);
        // Drawn in graph space (the layer transform sizes it to screen), so spacing and radius
        // scale 1:1 with the nodes. Zoomed far out the spacing coarsens by powers of two, keeping
        // the on-screen density and the dot count bounded.
        let scale = self.canvas.to_global.scaling.max(0.001);
        let mut spacing = DOT_SPACING;
        while spacing * scale < 26.0 {
            spacing *= 2.0;
        }
        let min_x = (viewport.min.x / spacing).floor() as i64;
        let max_x = (viewport.max.x / spacing).ceil() as i64;
        let min_y = (viewport.min.y / spacing).floor() as i64;
        let max_y = (viewport.max.y / spacing).ceil() as i64;
        // Backstop against a pathological transform.
        if (max_x - min_x).saturating_mul(max_y - min_y) > 6500 {
            return;
        }
        for xi in min_x..=max_x {
            for yi in min_y..=max_y {
                painter.circle_filled(
                    egui::pos2(xi as f32 * spacing, yi as f32 * spacing),
                    DOT_RADIUS,
                    DOT_COLOR,
                );
            }
        }
        // The viewport arrives here in graph space, so this is also where the canvas learns the
        // rect a fit has to land in.
        self.canvas.ui_rect = self.canvas.to_global * *viewport;
    }

    fn current_transform(&mut self, to_global: &mut TSTransform, _snarl: &mut Snarl<T>) {
        match self.canvas.cmd.take() {
            Some(ViewCmd::FitAll) => {
                if let Some(b) = self.bounds
                    && b.is_finite()
                    && self.canvas.ui_rect.is_finite()
                {
                    *to_global = fit_transform(b.expand(60.0), self.canvas.ui_rect);
                }
            }
            Some(ViewCmd::Center(p)) => {
                if p.x.is_finite() && p.y.is_finite() && self.canvas.ui_rect.is_finite() {
                    let s = to_global.scaling;
                    *to_global = TSTransform::new(
                        self.canvas.ui_rect.center().to_vec2() - p.to_vec2() * s,
                        s,
                    );
                }
            }
            None => {}
        }
        self.canvas.to_global = *to_global;
        self.canvas.bounds = self.bounds;
        // Runs once per pass, before the nodes: last pass's measurements become the current ones.
        self.canvas.sizes = std::mem::take(&mut self.canvas.sizes_next);
    }

    // ── Everything below forwards to the wrapped viewer unchanged ──

    fn title(&mut self, node: &T) -> String {
        self.inner.title(node)
    }
    fn inputs(&mut self, node: &T) -> usize {
        self.inner.inputs(node)
    }
    fn outputs(&mut self, node: &T) -> usize {
        self.inner.outputs(node)
    }
    fn show_input(
        &mut self,
        pin: &InPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) -> impl SnarlPin + 'static {
        let hit = self.pin_hit();
        FatPin { inner: self.inner.show_input(pin, ui, snarl), hit, dot: PIN_DOT }
    }
    fn show_output(
        &mut self,
        pin: &OutPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) -> impl SnarlPin + 'static {
        let hit = self.pin_hit();
        FatPin { inner: self.inner.show_output(pin, ui, snarl), hit, dot: PIN_DOT }
    }
    /// Glass over whatever the wrapped viewer returned. A 2px stroke is a status rim the inner
    /// viewer painted deliberately (executing, error) and keeps its colour.
    fn node_frame(
        &mut self,
        default: egui::Frame,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        snarl: &Snarl<T>,
    ) -> egui::Frame {
        let frame = self
            .inner
            .node_frame(default, node, inputs, outputs, snarl)
            .fill(theme::SURFACE)
            .corner_radius(NODE_CORNER);
        if frame.stroke.width < 2.0 {
            frame.stroke(egui::Stroke::new(1.0, theme::RIM_BRIGHT))
        } else {
            frame
        }
    }
    fn header_frame(
        &mut self,
        default: egui::Frame,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        snarl: &Snarl<T>,
    ) -> egui::Frame {
        self.inner
            .header_frame(default, node, inputs, outputs, snarl)
            .fill(HEADER_FILL)
            .shadow(egui::epaint::Shadow::NONE)
    }
    /// Always true: snarl builds each node's `Ui` from this style, so without it the widgets
    /// inside a node keep whatever palette surrounds the canvas.
    fn has_node_style(
        &mut self,
        _node: NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        _snarl: &Snarl<T>,
    ) -> bool {
        true
    }
    fn apply_node_style(
        &mut self,
        style: &mut egui::Style,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        snarl: &Snarl<T>,
    ) {
        self.inner.apply_node_style(style, node, inputs, outputs, snarl);
        theme::widget_palette(&mut style.visuals.widgets);
    }
    fn node_layout(
        &mut self,
        default: NodeLayout,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        snarl: &Snarl<T>,
    ) -> NodeLayout {
        self.inner.node_layout(default, node, inputs, outputs, snarl)
    }
    fn show_header(
        &mut self,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) {
        self.inner.show_header(node, inputs, outputs, ui, snarl);
    }
    fn has_body(&mut self, node: &T) -> bool {
        self.inner.has_body(node)
    }
    fn show_body(
        &mut self,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) {
        self.inner.show_body(node, inputs, outputs, ui, snarl);
    }
    fn has_footer(&mut self, node: &T) -> bool {
        self.inner.has_footer(node)
    }
    fn show_footer(
        &mut self,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) {
        self.inner.show_footer(node, inputs, outputs, ui, snarl);
    }
    fn final_node_rect(
        &mut self,
        node: NodeId,
        rect: egui::Rect,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) {
        self.canvas.sizes_next.insert(node, rect.size());
        self.inner.final_node_rect(node, rect, ui, snarl);
    }
    fn has_on_hover_popup(&mut self, node: &T) -> bool {
        self.inner.has_on_hover_popup(node)
    }
    fn show_on_hover_popup(
        &mut self,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) {
        self.inner.show_on_hover_popup(node, inputs, outputs, ui, snarl);
    }
    fn has_wire_widget(&mut self, from: &OutPinId, to: &InPinId, snarl: &Snarl<T>) -> bool {
        self.inner.has_wire_widget(from, to, snarl)
    }
    fn show_wire_widget(
        &mut self,
        from: &OutPin,
        to: &InPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) {
        self.inner.show_wire_widget(from, to, ui, snarl);
    }
    /// Suppressed: snarl's own graph menu cannot survive a finger lift on touch. `graph_menu`
    /// drives the same `show_graph_menu` content from a long-press instead.
    fn has_graph_menu(&mut self, _pos: egui::Pos2, _snarl: &mut Snarl<T>) -> bool {
        false
    }
    fn show_graph_menu(&mut self, pos: egui::Pos2, ui: &mut egui::Ui, snarl: &mut Snarl<T>) {
        self.inner.show_graph_menu(pos, ui, snarl);
    }
    fn has_dropped_wire_menu(&mut self, src_pins: AnyPins, snarl: &mut Snarl<T>) -> bool {
        self.inner.has_dropped_wire_menu(src_pins, snarl)
    }
    fn show_dropped_wire_menu(
        &mut self,
        pos: egui::Pos2,
        ui: &mut egui::Ui,
        src_pins: AnyPins,
        snarl: &mut Snarl<T>,
    ) {
        self.inner.show_dropped_wire_menu(pos, ui, src_pins, snarl);
    }
    fn has_node_menu(&mut self, node: &T) -> bool {
        self.inner.has_node_menu(node)
    }
    fn show_node_menu(
        &mut self,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<T>,
    ) {
        self.inner.show_node_menu(node, inputs, outputs, ui, snarl);
    }
    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<T>) {
        self.inner.connect(from, to, snarl);
    }
    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<T>) {
        self.inner.disconnect(from, to, snarl);
    }
    fn drop_outputs(&mut self, pin: &OutPin, snarl: &mut Snarl<T>) {
        self.inner.drop_outputs(pin, snarl);
    }
    fn drop_inputs(&mut self, pin: &InPin, snarl: &mut Snarl<T>) {
        self.inner.drop_inputs(pin, snarl);
    }
}

/// Bounding box of all nodes in graph space.
pub fn bounds<T>(snarl: &Snarl<T>) -> Option<egui::Rect> {
    let mut b: Option<egui::Rect> = None;
    for (_, pos, _) in snarl.nodes_pos_ids() {
        let r = egui::Rect::from_min_size(pos, NOMINAL_NODE);
        b = Some(b.map_or(r, |b| b.union(r)));
    }
    b
}

/// Compact layout: bands by longest-path depth, nodes stacked within each band, bands centred
/// across the flow, then relaxed toward straight wires. Returns the placed rects.
///
/// `vertical` runs execution top-to-bottom instead of left-to-right, so a deep graph lays out
/// along the screen's long axis rather than as a thin ribbon across it.
pub fn arrange<T>(
    snarl: &mut Snarl<T>,
    sizes: &HashMap<NodeId, egui::Vec2>,
    vertical: bool,
) -> Vec<egui::Rect> {
    // Gaps along the flow (between depths) and across it (between siblings in a depth).
    const FLOW_GAP: f32 = 60.0;
    const CROSS_GAP: f32 = 24.0;
    let flow = |v: egui::Vec2| if vertical { v.y } else { v.x };
    let cross = |v: egui::Vec2| if vertical { v.x } else { v.y };
    let at = |along: f32, across: f32| {
        if vertical { egui::pos2(across, along) } else { egui::pos2(along, across) }
    };

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

    // Pseudo-topological order via iterative DFS post-order — robust to cycles, which a flow
    // graph can contain. Kahn's-style layering lets one cycle poison every downstream depth and
    // collapse the whole graph into a single column.
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

    // Longest-path layer over forward edges only; back-edges wrap around rather than shoving
    // their target into a late column.
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
        columns[depth.get(&id).copied().unwrap_or(0)].push(id);
    }
    // Seed each column's order from the current layout, then reduce crossings with barycenter
    // sweeps so wires run mostly straight and execution reads down each column.
    for column in &mut columns {
        column.sort_by(|a, b| {
            let key = |id: &NodeId| {
                snarl
                    .get_node_info(*id)
                    .map(|n| if vertical { n.pos.x } else { n.pos.y })
                    .unwrap_or(0.0)
            };
            key(a).total_cmp(&key(b))
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
    let reorder = |column: &mut Vec<NodeId>,
                   neighbors: &HashMap<NodeId, Vec<NodeId>>,
                   idx: &HashMap<NodeId, f32>| {
        let mut keyed: Vec<(NodeId, f32)> = column
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                let bary = match neighbors.get(&id) {
                    Some(ns) if !ns.is_empty() => {
                        ns.iter().filter_map(|n| idx.get(n)).sum::<f32>() / ns.len() as f32
                    }
                    _ => i as f32,
                };
                (id, bary)
            })
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
    // Band offsets and thicknesses along the flow, from the deepest-extent node in each band.
    let mut col_x = Vec::with_capacity(columns.len());
    let mut col_w = Vec::with_capacity(columns.len());
    let mut x = 0.0f32;
    for column in &columns {
        col_x.push(x);
        let w = if column.is_empty() {
            0.0
        } else {
            column.iter().map(|&id| flow(size_of(id))).fold(1.0f32, f32::max)
        };
        col_w.push(w);
        if !column.is_empty() {
            x += w + FLOW_GAP;
        }
    }
    // Seed each node's cross-axis centre from a centred stack per band.
    let mut cy: HashMap<NodeId, f32> = HashMap::new();
    for column in &columns {
        let total: f32 =
            column.iter().map(|&id| cross(size_of(id)) + CROSS_GAP).sum::<f32>() - CROSS_GAP;
        let mut top = -total / 2.0;
        for &id in column {
            let h = cross(size_of(id));
            cy.insert(id, top + h / 2.0);
            top += h + CROSS_GAP;
        }
    }
    // Push each node along to keep CROSS_GAP from the one before it, preserving column order.
    let resolve = |column: &[NodeId], cy: &mut HashMap<NodeId, f32>| {
        for w in 1..column.len() {
            let (prev, cur) = (column[w - 1], column[w]);
            let min_c =
                cy[&prev] + cross(size_of(prev)) / 2.0 + CROSS_GAP + cross(size_of(cur)) / 2.0;
            if cy[&cur] < min_c {
                cy.insert(cur, min_c);
            }
        }
    };
    let column_mean = |column: &[NodeId], cy: &HashMap<NodeId, f32>| -> f32 {
        if column.is_empty() {
            return 0.0;
        }
        column.iter().filter_map(|id| cy.get(id)).sum::<f32>() / column.len() as f32
    };
    let edges: Vec<(NodeId, NodeId)> = successors
        .iter()
        .flat_map(|(from, tos)| tos.iter().map(|to| (*from, *to)))
        .collect();
    let wire_cost = |cy: &HashMap<NodeId, f32>| -> f32 {
        edges
            .iter()
            .filter_map(|(a, b)| Some((cy.get(a)?, cy.get(b)?)))
            .map(|(a, b)| (a - b).abs())
            .sum()
    };
    let vspan = |cy: &HashMap<NodeId, f32>| -> f32 {
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for (&id, &c) in cy {
            let h = cross(size_of(id));
            lo = lo.min(c - h / 2.0);
            hi = hi.max(c + h / 2.0);
        }
        if hi > lo { hi - lo } else { 0.0 }
    };
    // Straighter wires are worth some height, but only if they buy twice the straightening they
    // cost — otherwise the compact seeded stack wins.
    let seed_span = vspan(&cy);
    let score = |cy: &HashMap<NodeId, f32>| wire_cost(cy) + 0.5 * (vspan(cy) - seed_span).max(0.0);

    // Relax toward neighbour mean, then restore spacing. Every separation pass is
    // mean-preserving and the best iterate is kept, so relaxation can never return a layout
    // worse (or taller) than the seed it started from.
    let mut best = cy.clone();
    let mut best_score = score(&cy);
    for _ in 0..8 {
        for neighbors in [&predecessors, &successors] {
            for column in &columns {
                for &id in column {
                    let Some(ns) = neighbors.get(&id) else { continue };
                    // Averaged over the neighbours actually found: dividing by `ns.len()` with one
                    // missing biases toward 0, i.e. toward the top of the layout.
                    let (sum, found) = ns
                        .iter()
                        .filter_map(|n| cy.get(n))
                        .fold((0.0, 0usize), |(s, k), v| (s + v, k + 1));
                    if found > 0 {
                        cy.insert(id, sum / found as f32);
                    }
                }
            }
            for column in &columns {
                let before = column_mean(column, &cy);
                resolve(column, &mut cy);
                let drift = column_mean(column, &cy) - before;
                if drift != 0.0 {
                    for id in column {
                        if let Some(c) = cy.get_mut(id) {
                            *c -= drift;
                        }
                    }
                }
            }
        }
        let s = score(&cy);
        if s < best_score {
            best_score = s;
            best = cy.clone();
        }
    }
    let cy = best;

    let mut rects = Vec::new();
    for (d, column) in columns.iter().enumerate() {
        for &id in column {
            let size = size_of(id);
            // Centred within the band rather than pinned to its leading edge, so a thin node
            // beside a deep one doesn't sit against the edge with the difference left as a void.
            let pos = at(col_x[d] + (col_w[d] - flow(size)) / 2.0, cy[&id] - cross(size) / 2.0);
            if let Some(info) = snarl.get_node_info_mut(id) {
                info.pos = pos;
            }
            rects.push(egui::Rect::from_min_size(pos, size));
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two producers into one consumer: three nodes, two depths, and nothing overlapping.
    fn chain() -> Snarl<u32> {
        let mut s = Snarl::new();
        let a = s.insert_node(egui::pos2(500.0, 500.0), 0);
        let b = s.insert_node(egui::pos2(20.0, 900.0), 1);
        let c = s.insert_node(egui::pos2(-300.0, 40.0), 2);
        s.connect(
            wirelab_flow_ui::egui_snarl::OutPinId { node: a, output: 0 },
            wirelab_flow_ui::egui_snarl::InPinId { node: c, input: 0 },
        );
        s.connect(
            wirelab_flow_ui::egui_snarl::OutPinId { node: b, output: 0 },
            wirelab_flow_ui::egui_snarl::InPinId { node: c, input: 1 },
        );
        s
    }

    #[test]
    fn arrange_never_overlaps() {
        let mut s = chain();
        let rects = arrange(&mut s, &HashMap::new(), false);
        assert_eq!(rects.len(), 3);
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(!rects[i].intersects(rects[j]), "{:?} overlaps {:?}", rects[i], rects[j]);
            }
        }
    }

    /// Producers land in an earlier band than the consumer they feed, whichever way the flow runs.
    #[test]
    fn arrange_puts_producers_before_consumers() {
        for vertical in [false, true] {
            let mut s = chain();
            let rects = arrange(&mut s, &HashMap::new(), vertical);
            let along = |r: &egui::Rect| if vertical { r.min.y } else { r.min.x };
            let consumer = rects.iter().map(along).fold(f32::MIN, f32::max);
            let producers: Vec<f32> = rects.iter().map(along).filter(|v| *v < consumer).collect();
            assert_eq!(producers.len(), 2, "vertical={vertical}");
        }
    }

    /// A graph whose wires form a cycle still lays out — the DFS ordering must not hang or
    /// collapse every node into one band.
    #[test]
    fn arrange_survives_a_cycle() {
        let mut s = chain();
        let ids: Vec<NodeId> = s.nodes_pos_ids().map(|(id, _, _)| id).collect();
        s.connect(
            wirelab_flow_ui::egui_snarl::OutPinId { node: ids[2], output: 0 },
            wirelab_flow_ui::egui_snarl::InPinId { node: ids[0], input: 0 },
        );
        assert_eq!(arrange(&mut s, &HashMap::new(), false).len(), 3);
    }

    /// The hit square holds a usable on-screen target across the zoom range without ever growing
    /// large enough to cover the node it hangs off.
    #[test]
    fn the_pin_hit_target_stays_grabbable_and_bounded() {
        // Two rows a pitch apart, as snarl lays them out.
        let (y0, y1) = (0.0, PIN_ROW);
        for scale in [MIN_SCALE, 0.1, 0.3, 1.0, MAX_SCALE] {
            let hit = pin_hit(scale);
            assert!(hit >= PIN_DOT, "scale {scale}: hit {hit} is smaller than the dot");
            // x is where snarl centres an input dot: PIN_MARGIN + PIN_DOT/2 left of the body.
            let node_left = 0.0;
            let r = pin_hit_rect(node_left - PIN_MARGIN - PIN_DOT * 0.5, y0, y1, hit, PIN_DOT);
            assert!(
                r.right() <= node_left + 0.001,
                "scale {scale}: hit rect reaches {} into the node body",
                r.right() - node_left
            );
            assert!(
                r.height() <= PIN_ROW + 0.001,
                "scale {scale}: hit rect is {} tall against a {PIN_ROW} row — adjacent pins overlap",
                r.height()
            );
            assert!(r.width() >= PIN_DOT, "scale {scale}: target narrower than the dot");
        }
        // Below the cap the target is constant on screen, which a fixed pin_size cannot be.
        assert!((pin_hit(1.5) * 1.5 - PIN_TOUCH).abs() < 0.01);
        assert!(pin_hit(0.3) > PIN_DOT, "zoomed out, the target must beat the bare dot");
    }

    #[test]
    fn an_empty_graph_arranges_to_nothing() {
        let mut s: Snarl<u32> = Snarl::new();
        assert!(arrange(&mut s, &HashMap::new(), false).is_empty());
        assert!(bounds(&s).is_none());
    }
}
