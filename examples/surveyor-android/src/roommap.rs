//! Room-map rendering: room rectangle, CSI nodes, the localization blob, and
//! radar targets with short trails. Pure egui painting — compiles on host.

use std::collections::VecDeque;

use egui::{Color32, Pos2, Rect, Stroke, Vec2};

use crate::config::AppConfig;
use crate::proto::LocEstimate;

pub const TRAIL_LEN: usize = 60;

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
        let margin = 16.0;
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

    pub fn room_rect(&self, room_w: f64) -> Rect {
        Rect::from_two_pos(self.to_screen(0.0, 0.0), self.to_screen(room_w, self.room_h))
    }
}

pub struct MapInputs<'a> {
    pub loc: Option<LocEstimate>,
    pub radar_room: &'a [(f64, f64, f64)], // (x, y, speed) already in room frame
    pub trails: &'a Trails,
}

pub fn paint(ui: &mut egui::Ui, cfg: &AppConfig, inputs: &MapInputs<'_>) -> egui::Response {
    let desired = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    let t = RoomTransform::fit(rect, cfg.room_w, cfg.room_h);
    painter.rect_filled(rect, 0.0, Color32::from_gray(16));
    painter.rect_stroke(
        t.room_rect(cfg.room_w),
        2.0,
        Stroke::new(1.5, Color32::from_gray(90)),
        egui::StrokeKind::Inside,
    );

    // CSI nodes
    for n in &cfg.nodes {
        let p = t.to_screen(n.x, n.y);
        painter.circle_filled(p, 5.0, Color32::from_rgb(90, 140, 220));
        painter.text(
            p + Vec2::new(8.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            format!("n{}", n.id),
            egui::FontId::proportional(12.0),
            Color32::from_gray(150),
        );
    }

    // Radar mount marker
    let mp = t.to_screen(cfg.mount.x_m, cfg.mount.y_m);
    painter.circle_stroke(mp, 6.0, Stroke::new(1.5, Color32::from_rgb(230, 170, 60)));

    // Trails (dim)
    for &(x, y) in &inputs.trails.radar {
        painter.circle_filled(t.to_screen(x, y), 1.5, Color32::from_rgb(90, 70, 25));
    }
    for &(x, y) in &inputs.trails.loc {
        painter.circle_filled(t.to_screen(x, y), 1.5, Color32::from_rgb(30, 80, 60));
    }

    // Radar targets: amber dots
    for &(x, y, _v) in inputs.radar_room {
        painter.circle_filled(t.to_screen(x, y), 6.0, Color32::from_rgb(240, 180, 70));
    }

    // CSI localization blob: green, radius shrinks as confidence grows
    if let Some(loc) = inputs.loc {
        let p = t.to_screen(loc.x, loc.y);
        let r = 10.0 + 20.0 * (1.0 - loc.confidence.clamp(0.0, 1.0)) as f32;
        painter.circle_filled(p, r, Color32::from_rgba_unmultiplied(60, 200, 130, 70));
        painter.circle_filled(p, 5.0, Color32::from_rgb(80, 230, 150));
    }

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
        for i in 0..100 {
            tr.push_loc(i as f64, 0.0);
        }
        assert_eq!(tr.loc.len(), TRAIL_LEN);
        assert_eq!(tr.loc.back().copied(), Some((99.0, 0.0)));
    }
}
