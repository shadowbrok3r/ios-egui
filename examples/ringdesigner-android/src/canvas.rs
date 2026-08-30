//! The paint surface: the unrolled band, and the single tile.
//!
//! Both are the same widget over different domains. The band is `(u, v)` in millimetres — arc
//! distance around the ring by arc distance across the section — and is where you see exactly where
//! the metal lands, because the composited height field is drawn underneath. The tile is one cell
//! of a repeat, wrapping in both axes so a stroke that leaves an edge comes back on the other.
//!
//! The band is a ~7:1 landscape strip (a size-7 crest is ~67 mm around by ~10 mm across) and a
//! portrait phone is not, so zoom is the normal working mode rather than an accessory: you draw on
//! an arc at a time, with the ruler and the seam markers saying where you are.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use egui_mobile::egui;
use ringdesign_core::alpha::AlphaLibrary;
use ringdesign_core::drawn::{DrawnAlpha, Stroke};
use ringdesign_core::field::{FieldContext, LayerStack, Uv};

use crate::paint::{self, Bite, Tool};

const GOOD: egui::Color32 = egui::Color32::from_rgb(82, 199, 115);
const INFO: egui::Color32 = egui::Color32::from_rgb(92, 154, 235);
const WARN: egui::Color32 = egui::Color32::from_rgb(242, 194, 61);
const DIM: egui::Color32 = egui::Color32::from_rgb(150, 152, 160);
const GRID: egui::Color32 = egui::Color32::from_rgb(64, 66, 74);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(203, 166, 247);

/// Which domain the canvas is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// The whole unrolled band, with castability zones and the ring-angle ruler.
    Band,
    /// One repeat cell, wrapping both ways.
    Tile,
}

/// Zoom and pan over the canvas, in normalized 0..1 space.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub zoom: f32,
    /// Centre of the visible window, 0..1.
    pub centre: egui::Vec2,
}

impl Default for View {
    fn default() -> Self {
        Self { zoom: 1.0, centre: egui::vec2(0.5, 0.5) }
    }
}

impl View {
    pub const MAX_ZOOM: f32 = 16.0;

    /// Keep the window inside the canvas so there is never blank space at an edge.
    fn clamped(mut self) -> Self {
        self.zoom = self.zoom.clamp(1.0, Self::MAX_ZOOM);
        let half = 0.5 / self.zoom;
        self.centre.x = self.centre.x.clamp(half, 1.0 - half);
        self.centre.y = self.centre.y.clamp(half, 1.0 - half);
        self
    }

    /// Visible sub-rectangle of the canvas, in 0..1.
    pub fn window(&self) -> egui::Rect {
        let half = 0.5 / self.zoom;
        egui::Rect::from_min_max(
            egui::pos2(self.centre.x - half, self.centre.y - half),
            egui::pos2(self.centre.x + half, self.centre.y + half),
        )
    }

    /// Zoom about a normalized focus, so the content under two fingers stays under them.
    pub fn pinch(mut self, factor: f32, focus: egui::Vec2, translate: egui::Vec2) -> Self {
        let before = self.zoom;
        self.zoom = (self.zoom * factor).clamp(1.0, Self::MAX_ZOOM);
        let applied = self.zoom / before;
        if applied.is_finite() && applied > 0.0 {
            self.centre = focus + (self.centre - focus) / applied;
        }
        self.centre -= translate;
        self.clamped()
    }

    pub fn pan(mut self, delta: egui::Vec2) -> Self {
        self.centre -= delta;
        self.clamped()
    }
}

/// Everything the canvas needs to be drawn and painted into for one frame.
pub struct CanvasInput<'a> {
    pub domain: Domain,
    pub ctx: &'a FieldContext,
    pub layers: &'a LayerStack,
    pub lib: &'a AlphaLibrary,
    /// The drawing being edited. `None` shows the surface read-only.
    pub target: Option<&'a mut DrawnAlpha>,
    pub view: &'a mut View,
    pub brush_frac: f32,
    pub soft: f32,
    pub depth_scale: f64,
    pub erase_toggle: bool,
    pub stylus_only: bool,
    /// Live pointer state from `HostExt::stylus_probe` — tool, hover, buttons,
    /// and the pen's own geometry.
    pub probe: egui_mobile::StylusProbe,
    /// The contact that owns the stroke in progress, held across frames so a
    /// palm landing mid-stroke cannot take it over.
    pub active_touch: &'a mut Option<egui::TouchId>,
    /// The sand's detail floor, mm — what the hover preview judges the brush
    /// width against. 0 disables the check.
    pub floor_mm: f64,
}

/// What the canvas did this frame.
#[derive(Default)]
pub struct CanvasOutput {
    /// A stroke was extended or finished, so the design changed.
    pub painted: bool,
    /// A stroke ended — the moment to commit and rebuild at full quality.
    pub stroke_ended: bool,
    /// Live readout for the status line.
    pub readout: Option<String>,
    pub wants_repaint: bool,
    /// The ceiling refused some of the depth asked for, this frame.
    pub clamped: bool,
    /// Which castability zone the last sample landed in.
    pub zone: Option<&'static str>,
    /// Barrel-tap eyedropper: the depth scale sampled under the tip.
    pub pick_depth: Option<f64>,
    /// Barrel-tap on the secondary button: step undo once.
    pub undo_step: bool,
}

/// Hover distance at which the pre-flight preview has faded out entirely.
///
/// `Axis::Distance` has no documented unit — it is whatever the digitiser
/// reports — so this is a display constant, not a measurement.
const HOVER_FADE_UNITS: f32 = 1.0;

/// Cached composited height field, rebuilt when the stack or the size changes.
#[derive(Clone)]
struct FieldTex {
    key: u64,
    tex: egui::TextureHandle,
}

pub fn show(ui: &mut egui::Ui, input: CanvasInput<'_>) -> CanvasOutput {
    let mut out = CanvasOutput::default();
    let (rect, response) =
        ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
    if !ui.is_rect_visible(rect) || rect.width() < 40.0 || rect.height() < 40.0 {
        return out;
    }

    let CanvasInput {
        domain,
        ctx,
        layers,
        lib,
        mut target,
        view,
        brush_frac,
        soft,
        depth_scale,
        erase_toggle,
        stylus_only,
        active_touch,
        floor_mm,
        probe,
    } = input;

    if ctx.circumference_mm <= 1e-6 || ctx.band_v_len_mm <= 1e-6 {
        return out;
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 18, 20));

    // --- Gesture: two fingers move the view, one draws --------------------------------------
    let multi = ui.input(|i| i.multi_touch());
    if let Some(mt) = multi {
        let focus = to_norm(rect, view, mt.center_pos);
        let translate = egui::vec2(
            mt.translation_delta.x / rect.width().max(1.0) / view.zoom,
            mt.translation_delta.y / rect.height().max(1.0) / view.zoom,
        );
        *view = view.pinch(mt.zoom_delta, focus, translate);
        out.wants_repaint = true;
    }

    let win = view.window();
    let plot = rect;

    // --- Composited height field underneath -------------------------------------------------
    // Coarse on purpose: every pixel is a full `LayerStack::height`, the app's hot loop. It is an
    // indicative underlay, not the artwork — the strokes are drawn over it at screen resolution.
    // Only the band has a height field worth showing: a tile is drawn in its own space and has no
    // position on the ring yet, so sampling the stack for it would be 73k evaluations of the app's
    // hot loop to produce one flat colour.
    if domain == Domain::Band {
        let (tex_w, tex_h) = (384usize, 96usize);
        let key = field_key(layers, ctx, win, tex_w, tex_h, domain);
        let cache_id = ui.id().with("field_tex");
        let cached = ui.memory(|m| m.data.get_temp::<FieldTex>(cache_id));
        let tex = match cached {
            Some(c) if c.key == key => c.tex,
            _ => {
                let image = field_image(layers, ctx, lib, win, tex_w, tex_h);
                let tex = ui.ctx().load_texture("band-field", image, egui::TextureOptions::LINEAR);
                ui.memory_mut(|m| m.data.insert_temp(cache_id, FieldTex { key, tex: tex.clone() }));
                tex
            }
        };
        painter.image(
            tex.id(),
            plot,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }

    // --- Castability zones -------------------------------------------------------------------
    if domain == Domain::Band {
        draw_zones(&painter, plot, ctx, win);
        draw_ruler(&painter, plot, ctx, win);
    } else {
        draw_tile_guides(&painter, plot);
    }

    // --- The strokes ---------------------------------------------------------------------------
    if let Some(d) = target.as_deref() {
        draw_strokes(&painter, plot, win, d);
    }

    // --- Paint ---------------------------------------------------------------------------------
    let tool = Tool::from_code(probe.tool);
    let buttons = probe.buttons;
    let erase = paint::erasing(tool, erase_toggle);
    let accepted = paint::accepts(tool, stylus_only);
    // A held barrel button is a modifier, not a second eraser: it takes the
    // gesture away from the brush entirely so the pen can pan without lifting.
    let held = paint::barrel(buttons);

    // Barrel held: pan and pinch, and on a press that never moved, act.
    if held.is_some() && multi.is_none() {
        if response.dragged() {
            let delta = egui::vec2(
                response.drag_delta().x / plot.width().max(1.0) / view.zoom,
                response.drag_delta().y / plot.height().max(1.0) / view.zoom,
            );
            *view = view.pan(delta);
            out.wants_repaint = true;
        }
        // A press that ends without a drag is a tap: primary samples the depth
        // under the tip into the brush, secondary steps undo.
        if response.drag_stopped() && response.drag_delta().length() < 2.0 {
            match held {
                Some(paint::Barrel::Primary) => {
                    if let Some(p) = response.interact_pointer_pos().filter(|p| plot.contains(*p)) {
                        let n = to_norm(plot, view, p);
                        let uv = ringdesign_core::field::Uv {
                            u: n.x as f64 * ctx.circumference_mm,
                            v: n.y as f64 * ctx.band_v_len_mm,
                        };
                        let h = layers.height(uv, ctx, lib);
                        out.pick_depth = Some((h / paint::MAX_RELIEF_MM).clamp(0.05, 1.0));
                    }
                }
                Some(paint::Barrel::Secondary) => out.undo_step = true,
                None => {}
            }
        }
        return out;
    }

    if let Some(d) = target.as_deref_mut() {
        if multi.is_none() && accepted {
            if response.drag_started() {
                d.strokes.push(Stroke::new(brush_frac, soft, erase));
                out.painted = true;
                *active_touch = None;
            }

            // Every touch sample this frame, in order, each carrying its *own*
            // force. The old path took one position from `interact_pointer_pos`
            // and paired it with the last force found anywhere in the event
            // list — so a resting finger's pressure was applied to the pen's
            // coordinate, and at 120-240 Hz against a 60 fps loop three of every
            // four samples were dropped and a fast arc landed as a polygon.
            let samples: Vec<(egui::TouchId, egui::Pos2, f32)> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Touch { id, pos, force, phase, .. } if *phase != egui::TouchPhase::Cancel => {
                            Some((*id, *pos, force.unwrap_or(0.85)))
                        }
                        _ => None,
                    })
                    .collect()
            });
            // The first id seen this stroke owns it; a second contact is a palm.
            if active_touch.is_none() {
                *active_touch = samples.first().map(|(id, _, _)| *id);
            }
            let owner = *active_touch;

            let mut fed = 0usize;
            if response.dragged() || response.drag_started() {
                for (id, pos, force) in samples.iter().copied().filter(|(id, _, _)| {
                    owner.is_none_or(|o| o == *id)
                }) {
                    if !plot.contains(pos) {
                        continue;
                    }
                    let n = to_norm(plot, view, pos);
                    let Some(st) = d.strokes.last_mut() else { break };
                    let v_mm = n.y as f64 * ctx.band_v_len_mm;
                    let b = bite_for(domain, ctx, v_mm, force, depth_scale);
                    // How the pen is held shapes the stamp; pressure still sets
                    // the depth. A device that reports no tilt sends 0, which
                    // rasterises as the round disc it always was.
                    st.push_held(n.x, n.y, b.alpha_value(), probe.tilt, probe.azimuth);
                    out.painted = true;
                    out.clamped = out.clamped || b.clamped();
                    out.readout = Some(readout(&b, tool, force));
                    out.zone = Some(zone_name(ctx, v_mm));
                    fed += 1;
                    let _ = id;
                }
                // A mouse or an unknown tool emits no Touch events at all, so
                // fall back to the pointer position at a firm press.
                if fed == 0 {
                    if let Some(p) = response.interact_pointer_pos() {
                        let n = to_norm(plot, view, p);
                        if let Some(st) = d.strokes.last_mut() {
                            let v_mm = n.y as f64 * ctx.band_v_len_mm;
                            let b = bite_for(domain, ctx, v_mm, 0.85, depth_scale);
                            st.push_held(n.x, n.y, b.alpha_value(), probe.tilt, probe.azimuth);
                            out.painted = true;
                            out.clamped = out.clamped || b.clamped();
                            out.readout = Some(readout(&b, tool, 0.85));
                            out.zone = Some(zone_name(ctx, v_mm));
                        }
                    }
                }
            }
            if response.drag_stopped() {
                if d.strokes.last().is_some_and(|s| s.is_empty()) {
                    d.strokes.pop();
                }
                out.stroke_ended = true;
                *active_touch = None;
            }
        } else if multi.is_none() && !accepted && response.dragged() {
            // A finger under stylus-only pans instead of drawing, so the hand still works.
            let delta = egui::vec2(
                response.drag_delta().x / plot.width().max(1.0) / view.zoom,
                response.drag_delta().y / plot.height().max(1.0) / view.zoom,
            );
            *view = view.pan(delta);
            out.wants_repaint = true;
        }
    }

    // --- Hover: the pen is a depth gauge before it is a brush -----------------------------------
    let ppp = ui.ctx().pixels_per_point();
    let hover_pt = probe.hover.map(|(x, y)| egui::pos2(x / ppp, y / ppp));
    let cursor = response
        .interact_pointer_pos()
        .or_else(|| ui.input(|i| i.pointer.hover_pos()))
        .or(hover_pt);
    if let Some(p) = cursor.filter(|p| plot.contains(*p)) {
        let n = to_norm(plot, view, p);
        let v_mm = n.y as f64 * ctx.band_v_len_mm;
        let ceiling = match domain {
            Domain::Band => paint::ceiling_mm(ctx, v_mm),
            Domain::Tile => paint::MAX_RELIEF_MM,
        };
        // What a full-pressure press would ask for here, against what the local
        // draft will actually allow. `bite` already answers both; until now the
        // answer only arrived after the stroke landed.
        let b = bite_for(domain, ctx, v_mm, 1.0, depth_scale);
        let wanted = paint::wanted_mm(1.0, depth_scale).max(1e-6);
        let allowed = (b.depth_mm / wanted).clamp(0.0, 1.0) as f32;

        let r_px = (brush_frac * plot.width() * view.zoom).clamp(3.0, 240.0);
        // Fade the preview in as the tip approaches. `Axis::Distance` is in
        // device units and simply absent on hardware that does not report it,
        // where 0 reads as "in contact" and the preview is always on — which is
        // the behaviour before this existed.
        let near = if probe.distance > 0.0 {
            (1.0 - (probe.distance / HOVER_FADE_UNITS)).clamp(0.15, 1.0)
        } else {
            1.0
        };
        let tint = (if erase { WARN } else { ACCENT }).gamma_multiply(near);

        // Two rings: the depth the surface will take, and the part it refuses.
        // A pen laid on a crest sees the inner ring collapse before it commits.
        if allowed > 0.001 {
            painter.circle_stroke(p, r_px * allowed.max(0.05), egui::Stroke::new(2.0, tint));
        }
        if b.clamped() {
            painter.circle_stroke(
                p,
                r_px,
                egui::Stroke::new(1.0, WARN.gamma_multiply(0.45 * near)),
            );
        } else {
            painter.circle_stroke(p, r_px, egui::Stroke::new(1.0, tint.gamma_multiply(0.35)));
        }

        // A brush finer than the sand's own detail floor casts as mush whatever
        // depth it is given, so mark it before any metal is committed.
        let brush_mm = (brush_frac as f64 * 2.0) * ctx.circumference_mm;
        if floor_mm > 0.0 && brush_mm < floor_mm {
            painter.circle_stroke(p, r_px + 3.0, egui::Stroke::new(1.0, WARN.gamma_multiply(near)));
        }

        if out.readout.is_none() {
            out.readout = Some(match domain {
                Domain::Band => {
                    let mush = if floor_mm > 0.0 && brush_mm < floor_mm {
                        format!(" · brush {brush_mm:.2} mm is under the {floor_mm:.2} mm floor")
                    } else {
                        String::new()
                    };
                    format!(
                        "max {ceiling:.2} mm here · {}{mush}",
                        zone_name(ctx, v_mm)
                    )
                }
                Domain::Tile => format!("tile · max {ceiling:.2} mm"),
            });
        }
    }

    out
}

fn bite_for(domain: Domain, ctx: &FieldContext, v_mm: f64, pressure: f32, scale: f64) -> Bite {
    match domain {
        Domain::Band => paint::bite(ctx, v_mm, pressure, scale),
        // A tile does not know where on the band it will land, so it stores the full range and the
        // layer's own height caps it later.
        Domain::Tile => {
            let wanted = paint::wanted_mm(pressure, scale);
            Bite { depth_mm: wanted, wanted_mm: wanted, ceiling_mm: paint::MAX_RELIEF_MM }
        }
    }
}

fn readout(b: &Bite, tool: Tool, pressure: f32) -> String {
    if b.clamped() {
        format!(
            "{:.2} mm — capped from {:.2} · {} {:.0}%",
            b.depth_mm,
            b.wanted_mm,
            tool.label(),
            pressure * 100.0
        )
    } else {
        format!("{:.2} mm · {} {:.0}%", b.depth_mm, tool.label(), pressure * 100.0)
    }
}

fn zone_name(ctx: &FieldContext, v_mm: f64) -> &'static str {
    match ctx.surface.draft_deg(v_mm, ctx.band_v_len_mm) {
        Some(d) if d >= ringdesign_core::field::SIDE_FACE_MIN_DRAFT_DEG => "side face, free",
        Some(d) if d >= 45.0 => "flank",
        _ => "crest, needs draft",
    }
}

/// Screen position to normalized canvas coordinates.
fn to_norm(rect: egui::Rect, view: &View, p: egui::Pos2) -> egui::Vec2 {
    let win = view.window();
    let fx = ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
    let fy = ((p.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
    egui::vec2(win.min.x + fx * win.width(), win.min.y + fy * win.height())
}

/// Normalized canvas coordinates to screen position.
fn to_screen(rect: egui::Rect, win: egui::Rect, n: egui::Vec2) -> egui::Pos2 {
    egui::pos2(
        rect.left() + (n.x - win.min.x) / win.width().max(1e-6) * rect.width(),
        rect.top() + (n.y - win.min.y) / win.height().max(1e-6) * rect.height(),
    )
}

fn draw_strokes(painter: &egui::Painter, rect: egui::Rect, win: egui::Rect, d: &DrawnAlpha) {
    for s in &d.strokes {
        if s.points.len() < 2 {
            if let Some(p) = s.points.first() {
                let c = to_screen(rect, win, egui::vec2(p[0], p[1]));
                painter.circle_filled(c, 2.0, stroke_color(s.erase, p[2]));
            }
            continue;
        }
        let pts: Vec<egui::Pos2> =
            s.points.iter().map(|p| to_screen(rect, win, egui::vec2(p[0], p[1]))).collect();
        let avg = s.points.iter().map(|p| p[2]).sum::<f32>() / s.points.len() as f32;
        let w = (s.radius * rect.width() / win.width().max(1e-6) * 2.0).clamp(1.5, 200.0);
        painter.add(egui::Shape::line(
            pts,
            egui::Stroke::new(w, stroke_color(s.erase, avg).gamma_multiply(0.8)),
        ));
    }
}

fn stroke_color(erase: bool, value: f32) -> egui::Color32 {
    if erase {
        egui::Color32::from_rgb(70, 74, 86)
    } else {
        // Warmer and brighter with depth, matching the height field's ramp.
        let t = value.clamp(0.0, 1.0);
        egui::Color32::from_rgb(
            (140.0 + 110.0 * t) as u8,
            (110.0 + 110.0 * t) as u8,
            (60.0 + 120.0 * t) as u8,
        )
    }
}

fn draw_zones(painter: &egui::Painter, rect: egui::Rect, ctx: &FieldContext, win: egui::Rect) {
    let y_of = |v_norm: f32| {
        rect.top() + (v_norm - win.min.y) / win.height().max(1e-6) * rect.height()
    };
    // Sample the draft across v and tint each band by what it can hold.
    const N: usize = 64;
    for i in 0..N {
        let a = i as f32 / N as f32;
        let b = (i + 1) as f32 / N as f32;
        let v_mm = (a + b) as f64 * 0.5 * ctx.band_v_len_mm;
        let ceiling = paint::ceiling_mm(ctx, v_mm);
        let t = ((ceiling - 0.05) / (paint::MAX_RELIEF_MM - 0.05)) as f32;
        let color = if t > 0.8 {
            GOOD.gamma_multiply(0.10)
        } else if t > 0.25 {
            WARN.gamma_multiply(0.07)
        } else {
            INFO.gamma_multiply(0.12)
        };
        let seg = egui::Rect::from_min_max(
            egui::pos2(rect.left(), y_of(a)),
            egui::pos2(rect.right(), y_of(b)),
        );
        if seg.intersects(rect) {
            painter.rect_filled(seg.intersect(rect), 0.0, color);
        }
    }
}

fn draw_ruler(painter: &egui::Painter, rect: egui::Rect, ctx: &FieldContext, win: egui::Rect) {
    let small = egui::FontId::proportional(10.0);
    for deg in (0..360).step_by(30) {
        let u_norm = ctx.u_of_theta(deg as f64) / ctx.circumference_mm;
        let x = rect.left()
            + ((u_norm as f32 - win.min.x) / win.width().max(1e-6)) * rect.width();
        if x < rect.left() - 1.0 || x > rect.right() + 1.0 {
            continue;
        }
        let top = ringdesign_core::profile::TOP_DEG as i32 == deg;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.0, GRID.gamma_multiply(if top { 1.0 } else { 0.5 })),
        );
        painter.text(
            egui::pos2(x, rect.top() + 2.0),
            egui::Align2::CENTER_TOP,
            if top { format!("{deg}° top") } else { format!("{deg}°") },
            small.clone(),
            if top { ACCENT } else { DIM },
        );
    }
    // The joint, where the pattern has to meet itself.
    for x in [rect.left(), rect.right()] {
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.5)),
        );
    }
}

fn draw_tile_guides(painter: &egui::Painter, rect: egui::Rect) {
    let s = egui::Stroke::new(1.0, GRID);
    painter.rect_stroke(rect.shrink(1.0), 0.0, s, egui::StrokeKind::Inside);
    painter.text(
        egui::pos2(rect.left() + 6.0, rect.top() + 4.0),
        egui::Align2::LEFT_TOP,
        "wraps both ways",
        egui::FontId::proportional(10.0),
        DIM,
    );
}

#[allow(clippy::too_many_arguments)]
fn field_key(
    layers: &LayerStack,
    ctx: &FieldContext,
    win: egui::Rect,
    w: usize,
    h: usize,
    _domain: Domain,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(layers).unwrap_or_default().hash(&mut hasher);
    ctx.circumference_mm.to_bits().hash(&mut hasher);
    ctx.band_v_len_mm.to_bits().hash(&mut hasher);
    for f in [win.min.x, win.min.y, win.max.x, win.max.y] {
        // Quantized, so a pixel of pan does not throw the cache away.
        ((f * 256.0) as i32).hash(&mut hasher);
    }
    w.hash(&mut hasher);
    h.hash(&mut hasher);
    hasher.finish()
}

#[allow(clippy::too_many_arguments)]
fn field_image(
    layers: &LayerStack,
    ctx: &FieldContext,
    lib: &AlphaLibrary,
    win: egui::Rect,
    w: usize,
    h: usize,
) -> egui::ColorImage {
    let mut heights = vec![0.0f64; w * h];
    let mut max_mm: f64 = 0.0;
    for j in 0..h {
        let fy = (j as f32 + 0.5) / h as f32;
        let v = (win.min.y + fy * win.height()) as f64 * ctx.band_v_len_mm;
        for i in 0..w {
            let fx = (i as f32 + 0.5) / w as f32;
            let u = (win.min.x + fx * win.width()) as f64 * ctx.circumference_mm;
            let x = layers.height(Uv { u, v }, ctx, lib);
            let x = if x.is_finite() { x } else { 0.0 };
            heights[j * w + i] = x;
            max_mm = max_mm.max(x.abs());
        }
    }
    let inv = if max_mm > 1e-9 { 1.0 / max_mm } else { 0.0 };
    let pixels = heights.iter().map(|&x| ramp(x * inv)).collect();
    egui::ColorImage::new([w, h], pixels)
}

/// Cool where the stack carves, warm where it stands proud.
fn ramp(t: f64) -> egui::Color32 {
    const STOPS: [[f32; 3]; 5] = [
        [0.30, 0.44, 0.64],
        [0.14, 0.17, 0.23],
        [0.09, 0.10, 0.13],
        [0.55, 0.40, 0.22],
        [0.96, 0.87, 0.70],
    ];
    let p = ((t.clamp(-1.0, 1.0) + 1.0) * 2.0) as f32;
    let i = (p.floor() as usize).min(STOPS.len() - 2);
    let f = (p - i as f32).clamp(0.0, 1.0);
    let (a, b) = (STOPS[i], STOPS[i + 1]);
    let c = |k: usize| ((a[k] + (b[k] - a[k]) * f) * 255.0) as u8;
    egui::Color32::from_rgb(c(0), c(1), c(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_view_shows_the_whole_canvas() {
        let w = View::default().window();
        assert_eq!(w.min, egui::pos2(0.0, 0.0));
        assert_eq!(w.max, egui::pos2(1.0, 1.0));
    }

    #[test]
    fn zooming_in_never_shows_anything_outside_the_canvas() {
        for (z, cx, cy) in [(4.0, -3.0, 9.0), (2.0, 0.0, 1.0), (16.0, 0.5, 0.5)] {
            let v = View { zoom: z, centre: egui::vec2(cx, cy) }.clamped();
            let w = v.window();
            assert!(w.min.x >= -1e-6 && w.min.y >= -1e-6, "{w:?}");
            assert!(w.max.x <= 1.0 + 1e-6 && w.max.y <= 1.0 + 1e-6, "{w:?}");
        }
    }

    #[test]
    fn zoom_is_bounded_at_both_ends() {
        assert_eq!(View { zoom: 0.01, ..Default::default() }.clamped().zoom, 1.0);
        assert_eq!(View { zoom: 1e6, ..Default::default() }.clamped().zoom, View::MAX_ZOOM);
    }

    #[test]
    fn pinching_keeps_the_focus_under_the_fingers() {
        let v = View { zoom: 2.0, centre: egui::vec2(0.5, 0.5) };
        let focus = egui::vec2(0.5, 0.5);
        let after = v.pinch(2.0, focus, egui::Vec2::ZERO);
        assert!((after.centre.x - 0.5).abs() < 1e-6);
        assert!((after.zoom - 4.0).abs() < 1e-6);
    }

    #[test]
    fn screen_and_canvas_coordinates_round_trip() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(300.0, 100.0));
        let view = View { zoom: 3.0, centre: egui::vec2(0.4, 0.6) }.clamped();
        let win = view.window();
        for n in [egui::vec2(0.35, 0.55), egui::vec2(0.4, 0.6)] {
            let p = to_screen(rect, win, n);
            let back = to_norm(rect, &view, p);
            assert!((back.x - n.x).abs() < 1e-4, "{back:?} vs {n:?}");
            assert!((back.y - n.y).abs() < 1e-4, "{back:?} vs {n:?}");
        }
    }

    #[test]
    fn the_ramp_stays_in_range_across_its_whole_domain() {
        for i in -20..=20 {
            let _ = ramp(i as f64 / 10.0);
        }
    }
}
