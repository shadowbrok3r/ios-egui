//! The Live radar view: a sector-scan instrument drawn from real data only.
//! Range rings and the sweep originate at the radar's surveyed mount; targets
//! are the LD2450's actual tracks, the violet blob is the CSI estimate. The
//! sweep and afterglow are presentation — nothing they show is invented.

use std::collections::VecDeque;

use egui::{Color32, Pos2, Rect, Stroke, Vec2};

use crate::config::AppConfig;
use crate::proto::LocEstimate;

// ~2.7 s of afterglow at the radar's 11 Hz. At 90 the streak ran 8 s behind a
// walker and read as render lag, though the head dot was always current.
pub const TRAIL_LEN: usize = 30;

// Canvas bed + instrument colors; accents come from the app theme.
// SWEEP = hot pink, TARGET = aqua, CSI = electric violet.
pub const BG: Color32 = Color32::from_rgb(7, 5, 10);
pub use crate::theme::{AQUA as TARGET, PINK as SWEEP, VIOLET as CSI};
const GRID: Color32 = Color32::from_rgb(30, 20, 46);
const RING: Color32 = Color32::from_rgb(58, 36, 88);
const ROOM_LINE: Color32 = Color32::from_rgb(84, 56, 128);

/// LD2450 azimuth field of view, half-angle in degrees.
const FOV_HALF_DEG: f32 = 60.0;
/// Sector sweep period (one full left-right-left cycle), seconds.
const SWEEP_PERIOD_S: f64 = 4.0;

#[derive(Default)]
pub struct Trails {
    pub loc: VecDeque<(f64, f64)>,
    pub radar: VecDeque<(f64, f64)>,
}

impl Trails {
    pub fn clear(&mut self) {
        self.loc.clear();
        self.radar.clear();
    }

    pub fn push_loc(&mut self, x: f64, y: f64) {
        if self.loc.len() >= TRAIL_LEN {
            self.loc.pop_front();
        }
        self.loc.push_back((x, y));
    }
    pub fn push_radar(&mut self, x: f64, y: f64) {
        if self.radar.len() >= TRAIL_LEN {
            self.radar.pop_front();
        }
        self.radar.push_back((x, y));
    }
}

/// Fit the room (with a margin) into the widget rect, +y up (room frame has
/// +y into the room; screen y grows downward, so flip).
pub struct RoomTransform {
    origin: Pos2,
    scale: f32,
    room_h: f64,
}

impl RoomTransform {
    pub fn fit(rect: Rect, room_w: f64, room_h: f64) -> Self {
        let margin = 14.0;
        let avail = rect.shrink(margin);
        let sx = avail.width() / room_w.max(0.1) as f32;
        let sy = avail.height() / room_h.max(0.1) as f32;
        let scale = sx.min(sy);
        let used = Vec2::new(room_w as f32 * scale, room_h as f32 * scale);
        let origin = avail.center() - used / 2.0;
        Self { origin, scale, room_h }
    }

    pub fn to_screen(&self, x: f64, y: f64) -> Pos2 {
        Pos2::new(
            self.origin.x + x as f32 * self.scale,
            self.origin.y + (self.room_h - y) as f32 * self.scale,
        )
    }

    pub fn px_per_m(&self) -> f32 {
        self.scale
    }

    pub fn room_rect(&self, room_w: f64) -> Rect {
        Rect::from_two_pos(self.to_screen(0.0, 0.0), self.to_screen(room_w, self.room_h))
    }
}

/// Sector-scan angle at time `t`: ping-pongs across [-half, +half] degrees.
/// Offset from boresight; the caller adds the mount yaw.
pub fn sweep_offset_deg(t: f64, half_deg: f32) -> f32 {
    let phase = (t / SWEEP_PERIOD_S).fract() as f32; // 0..1
    let tri = if phase < 0.5 { phase * 2.0 } else { 2.0 - phase * 2.0 }; // 0..1..0
    -half_deg + tri * 2.0 * half_deg
}

fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// A glowing dot: layered translucent halos under a bright core.
fn glow_dot(painter: &egui::Painter, p: Pos2, r: f32, color: Color32) {
    painter.circle_filled(p, r * 3.0, with_alpha(color, 18));
    painter.circle_filled(p, r * 1.8, with_alpha(color, 60));
    painter.circle_filled(p, r, color);
}

pub struct MapInputs<'a> {
    pub loc: Option<LocEstimate>,
    /// (x, y, radial speed m/s) already in room frame.
    pub radar_room: &'a [(f64, f64, f64)],
    pub trails: &'a Trails,
    /// Radar mount position and boresight yaw in room frame.
    pub radar_pos: (f64, f64),
    pub radar_yaw_deg: f64,
    pub live: bool,
}

pub fn paint(ui: &mut egui::Ui, cfg: &AppConfig, inputs: &MapInputs<'_>) -> egui::Response {
    let desired = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let t = RoomTransform::fit(rect, cfg.room_w, cfg.room_h);

    // Instrument bed
    painter.rect_filled(rect, 10.0, BG);
    let room = t.room_rect(cfg.room_w);

    // 1 m grid, very dim
    let mut m = 1.0;
    while m < cfg.room_w {
        let p = t.to_screen(m, 0.0);
        painter.line_segment(
            [Pos2::new(p.x, room.min.y), Pos2::new(p.x, room.max.y)],
            Stroke::new(1.0, GRID),
        );
        m += 1.0;
    }
    m = 1.0;
    while m < cfg.room_h {
        let p = t.to_screen(0.0, m);
        painter.line_segment(
            [Pos2::new(room.min.x, p.y), Pos2::new(room.max.x, p.y)],
            Stroke::new(1.0, GRID),
        );
        m += 1.0;
    }

    // Room boundary
    painter.rect_stroke(room, 4.0, Stroke::new(1.5, ROOM_LINE), egui::StrokeKind::Inside);

    let radar_px = t.to_screen(inputs.radar_pos.0, inputs.radar_pos.1);
    let max_range_m = (cfg.room_w.hypot(cfg.room_h)).ceil();

    // Range rings around the radar, 1 m apart, clipped to the room
    let ring_painter = painter.with_clip_rect(room);
    let mut r = 1.0;
    while r <= max_range_m {
        ring_painter.circle_stroke(
            radar_px,
            r as f32 * t.px_per_m(),
            Stroke::new(1.0, with_alpha(RING, 140)),
        );
        r += 1.0;
    }

    // Boresight axis angle in screen space. Room yaw is CCW from +y (up);
    // screen angles measure from +x with y flipped, so boresight(up) = -90deg.
    let boresight = (-90.0 + inputs.radar_yaw_deg) as f32;
    let ray = |deg: f32, range_m: f32| -> Pos2 {
        let a = deg.to_radians();
        radar_px + Vec2::new(a.cos(), a.sin()) * range_m * t.px_per_m()
    };

    // FOV edges
    for edge in [-FOV_HALF_DEG, FOV_HALF_DEG] {
        ring_painter.line_segment(
            [radar_px, ray(boresight + edge, max_range_m as f32)],
            Stroke::new(1.0, with_alpha(SWEEP, 70)),
        );
    }

    // Sweep: fading wedge behind a bright leading edge, sector ping-pong.
    if inputs.live {
        let now = ui.input(|i| i.time);
        let lead = boresight + sweep_offset_deg(now, FOV_HALF_DEG);
        let dir = if (now / SWEEP_PERIOD_S).fract() < 0.5 { 1.0 } else { -1.0 };
        for i in 0..24 {
            let a0 = lead - dir * (i as f32) * 1.6;
            let a1 = lead - dir * (i as f32 + 1.0) * 1.6;
            // Skip wedge slices that have swung outside the FOV.
            if (a0 - boresight).abs() > FOV_HALF_DEG + 1.0 {
                continue;
            }
            let alpha = (36.0 * (1.0 - i as f32 / 24.0)) as u8;
            ring_painter.add(egui::Shape::convex_polygon(
                vec![radar_px, ray(a0, max_range_m as f32), ray(a1, max_range_m as f32)],
                with_alpha(SWEEP, alpha),
                Stroke::NONE,
            ));
        }
        ring_painter.line_segment(
            [radar_px, ray(lead, max_range_m as f32)],
            Stroke::new(2.0, with_alpha(SWEEP, 200)),
        );
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }

    // CSI nodes: small violet diamonds with labels
    for n in &cfg.nodes {
        let p = t.to_screen(n.x, n.y);
        let d = 5.0;
        painter.add(egui::Shape::convex_polygon(
            vec![
                p + Vec2::new(0.0, -d),
                p + Vec2::new(d, 0.0),
                p + Vec2::new(0.0, d),
                p + Vec2::new(-d, 0.0),
            ],
            with_alpha(CSI, 200),
            Stroke::NONE,
        ));
        painter.text(
            p + Vec2::new(8.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            format!("n{}", n.id),
            egui::FontId::monospace(11.0),
            with_alpha(CSI, 150),
        );
    }

    // Radar emitter
    glow_dot(&painter, radar_px, 5.0, TARGET);

    // Afterglow trails, alpha decaying with age
    let decay = |i: usize, len: usize, max_a: f32| -> u8 {
        (max_a * (i as f32 + 1.0) / len.max(1) as f32) as u8
    };
    let n = inputs.trails.radar.len();
    for (i, &(x, y)) in inputs.trails.radar.iter().enumerate() {
        ring_painter.circle_filled(t.to_screen(x, y), 2.0, with_alpha(TARGET, decay(i, n, 110.0)));
    }
    let n = inputs.trails.loc.len();
    for (i, &(x, y)) in inputs.trails.loc.iter().enumerate() {
        ring_painter.circle_filled(t.to_screen(x, y), 2.0, with_alpha(CSI, decay(i, n, 90.0)));
    }

    // Radar targets: amber glow + radial velocity tick (along the radar ray,
    // outward when receding)
    for &(x, y, v) in inputs.radar_room {
        let p = t.to_screen(x, y);
        glow_dot(&painter, p, 5.5, TARGET);
        if v.abs() > 0.02 {
            let to_target = (p - radar_px).normalized();
            let tick = to_target * (v as f32 * 0.6 * t.px_per_m());
            painter.line_segment([p, p + tick], Stroke::new(2.0, with_alpha(TARGET, 180)));
        }
    }

    // CSI estimate: cyan blob, halo radius = inverse confidence
    if let Some(loc) = inputs.loc {
        let p = t.to_screen(loc.x, loc.y);
        let halo = 10.0 + 22.0 * (1.0 - loc.confidence.clamp(0.0, 1.0)) as f32;
        painter.circle_filled(p, halo, with_alpha(CSI, 46));
        glow_dot(&painter, p, 5.0, CSI);
    }

    // Corner legend, instrument-style
    painter.text(
        rect.left_bottom() + Vec2::new(10.0, -8.0),
        egui::Align2::LEFT_BOTTOM,
        format!("{}x{} m  rings 1 m", cfg.room_w, cfg.room_h),
        egui::FontId::monospace(10.0),
        with_alpha(SWEEP, 120),
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_flips_y_and_fits() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 400.0));
        let t = RoomTransform::fit(rect, 5.0, 4.0);
        let origin = t.to_screen(0.0, 0.0);
        let far = t.to_screen(5.0, 4.0);
        assert!(far.x > origin.x, "x grows right");
        assert!(far.y < origin.y, "room +y is up on screen");
    }

    #[test]
    fn trails_cap_length() {
        let mut tr = Trails::default();
        for i in 0..200 {
            tr.push_loc(i as f64, 0.0);
        }
        assert_eq!(tr.loc.len(), TRAIL_LEN);
        assert_eq!(tr.loc.back().copied(), Some((199.0, 0.0)));
    }

    #[test]
    fn sweep_ping_pongs_within_fov() {
        for step in 0..100 {
            let t = step as f64 * 0.1;
            let a = sweep_offset_deg(t, 60.0);
            assert!((-60.0..=60.0).contains(&a), "t={t} a={a}");
        }
        // Extremes are reached
        assert!((sweep_offset_deg(0.0, 60.0) - -60.0).abs() < 1e-3);
        assert!((sweep_offset_deg(2.0, 60.0) - 60.0).abs() < 1e-3);
    }
}
