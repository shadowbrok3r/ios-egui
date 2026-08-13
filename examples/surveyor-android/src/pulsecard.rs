//! Micro-motion instrument for the A121 presence channel.
//!
//! Scrolling traces of the presence detector's two scores: inter (slow,
//! breathing-band — literally pulses with breath) in aqua, intra (fast
//! motion) in pink, over a violet presence tint. The A121 is 1D, so this is
//! deliberately a strip-chart, not a map: it shows *that* and *how much*,
//! never *where*.

use egui::{Color32, Pos2, Rect, Stroke, Vec2};

use crate::proto::A121Snapshot;
use crate::theme;

/// ~30 s of history at the detector's ~12 Hz.
pub const TRACE_LEN: usize = 360;

/// Log-compressed vertical scale: scores sit at 0-3 empty and spike to 20+
/// when someone breathes nearby; raw linear either clips or flattens.
fn squash(v: f64) -> f32 {
    ((1.0 + v.max(0.0)).ln() / (1.0f64 + 30.0).ln()).min(1.0) as f32
}

#[derive(Default)]
pub struct PulseTrace {
    /// (inter, intra, presence) per sample, oldest first.
    samples: Vec<(f32, f32, bool)>,
}

impl PulseTrace {
    pub fn push(&mut self, s: &A121Snapshot) {
        if self.samples.len() >= TRACE_LEN {
            self.samples.remove(0);
        }
        self.samples.push((squash(s.inter_score), squash(s.intra_score), s.presence));
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn samples(&self) -> &[(f32, f32, bool)] {
        &self.samples
    }
}

fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// Paint the instrument strip. `latest` is None when the channel is stale.
pub fn paint(ui: &mut egui::Ui, trace: &PulseTrace, latest: Option<&A121Snapshot>) {
    let h = 74.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 8.0, Color32::from_rgb(7, 5, 10));
    painter.rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, Color32::from_rgba_premultiplied(46, 46, 52, 46)),
        egui::StrokeKind::Inside,
    );

    let pad = 8.0;
    let plot = Rect::from_min_max(
        Pos2::new(rect.left() + pad, rect.top() + pad),
        Pos2::new(rect.right() - pad, rect.bottom() - pad),
    );

    let n = trace.samples().len();
    if n >= 2 {
        let dx = plot.width() / (TRACE_LEN - 1) as f32;
        let x0 = plot.right() - dx * (n - 1) as f32;

        // Presence tint behind the traces, per-sample so gaps read as gaps.
        for (i, &(_, _, present)) in trace.samples().iter().enumerate() {
            if present {
                let x = x0 + dx * i as f32;
                painter.line_segment(
                    [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                    Stroke::new(dx.max(1.0), with_alpha(theme::VIOLET, 16)),
                );
            }
        }
        let pts = |pick: fn(&(f32, f32, bool)) -> f32| -> Vec<Pos2> {
            trace
                .samples()
                .iter()
                .enumerate()
                .map(|(i, s)| Pos2::new(x0 + dx * i as f32, plot.bottom() - pick(s) * plot.height()))
                .collect()
        };
        let inter: Vec<Pos2> = pts(|s| s.0);
        let intra: Vec<Pos2> = pts(|s| s.1);
        // Soft glow pass under each trace, then the crisp line.
        painter.add(egui::Shape::line(inter.clone(), Stroke::new(4.0, with_alpha(theme::AQUA, 40))));
        painter.add(egui::Shape::line(intra.clone(), Stroke::new(4.0, with_alpha(theme::PINK, 36))));
        painter.add(egui::Shape::line(inter, Stroke::new(1.4, theme::AQUA)));
        painter.add(egui::Shape::line(intra, Stroke::new(1.4, with_alpha(theme::PINK, 210))));
    }

    // Threshold tick: squash(1.0) is the detector's presence trigger level.
    let ty = plot.bottom() - squash(1.0) * plot.height();
    painter.line_segment(
        [Pos2::new(plot.left(), ty), Pos2::new(plot.left() + 14.0, ty)],
        Stroke::new(1.0, with_alpha(theme::INK, 90)),
    );

    let font = egui::FontId::monospace(10.0);
    match latest {
        Some(a) => {
            let (label, color) = if a.presence {
                (format!("PRESENT {:.2} m", a.distance_m), theme::AQUA_BRIGHT)
            } else {
                ("CLEAR".to_owned(), with_alpha(theme::INK, 150))
            };
            painter.text(plot.right_top(), egui::Align2::RIGHT_TOP, label, font.clone(), color);
            let mut spec = format!("breath {:.1}  fast {:.1}", a.inter_score, a.intra_score);
            if let Some(b) = a.breathing_bpm {
                spec = format!("{spec}  {b:.1} bpm");
            }
            painter.text(
                plot.right_bottom(),
                egui::Align2::RIGHT_BOTTOM,
                spec,
                font.clone(),
                with_alpha(theme::INK, 140),
            );
        }
        None => {
            painter.text(
                plot.right_top(),
                egui::Align2::RIGHT_TOP,
                "A121 quiet",
                font.clone(),
                with_alpha(theme::INK, 110),
            );
        }
    }
    painter.text(
        plot.left_top(),
        egui::Align2::LEFT_TOP,
        "MICRO-MOTION 60 GHz",
        font,
        with_alpha(theme::VIOLET_BRIGHT, 170),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(inter: f64, intra: f64, presence: bool) -> A121Snapshot {
        A121Snapshot {
            node_id: 9,
            presence,
            distance_m: 0.5,
            inter_score: inter,
            intra_score: intra,
            breathing_bpm: None,
        }
    }

    #[test]
    fn trace_caps_length() {
        let mut t = PulseTrace::default();
        for i in 0..(TRACE_LEN + 50) {
            t.push(&snap(i as f64 * 0.1, 0.0, true));
        }
        assert_eq!(t.len(), TRACE_LEN);
    }

    #[test]
    fn squash_is_monotone_and_bounded() {
        assert_eq!(squash(0.0), 0.0);
        assert!(squash(1.0) > 0.0 && squash(1.0) < squash(5.0));
        assert!(squash(30.0) <= 1.0);
        assert!(squash(1e9) <= 1.0);
        assert!(squash(-4.0) == 0.0, "negative scores clamp to baseline");
    }

    #[test]
    fn oldest_sample_is_evicted_first() {
        let mut t = PulseTrace::default();
        for i in 0..TRACE_LEN {
            t.push(&snap(i as f64, 0.0, false));
        }
        let first_before = t.samples()[0].0;
        t.push(&snap(999.0, 0.0, true));
        assert!(t.samples()[0].0 > first_before, "front sample should have shifted");
        assert!(t.samples()[TRACE_LEN - 1].2, "newest sample at the back");
    }
}
