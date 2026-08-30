//! Drawing a face plan and a band section with the pen.
//!
//! Two shapes the core has always been able to carry and the phone could not
//! author. A signet face was whichever of the eleven builtins someone had
//! enumerated; `ProfileStyle::Custom` was in the combo and did nothing, because
//! a custom profile is shaped by `BandProfile.drop_curve` and there was no way
//! to make one.
//!
//! Sketching a section and a face is what a jeweller does on paper first, and
//! it is the one authoring act where a pen beats a mouse and a slider outright.
//! Nothing generative belongs here: the drawn line *is* the boundary, so a model
//! could only add error to exact data.

use egui_mobile::egui;

/// Which shape is being drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A closed plan for a signet face, traced to a `CustomOutline`.
    Face,
    /// Half a cross-section, resampled to a `DropCurve`.
    Section,
}

/// A sketch in progress — points in 0..1, newest last.
#[derive(Clone, Debug, Default)]
pub struct Sketch {
    pub points: Vec<[f32; 2]>,
    /// Let the section fall back on itself, which is what makes it uncastable
    /// in sand. Off by default and warned about when on.
    pub allow_undercut: bool,
    pub name: String,
}

impl Sketch {
    pub fn clear(&mut self) {
        self.points.clear();
    }

    pub fn push(&mut self, x: f32, y: f32) {
        // The same de-duplication `Stroke::push` uses, for the same reason: a
        // 240 Hz pen against a slow hand is mostly repeats.
        if let Some(l) = self.points.last() {
            if (l[0] - x).abs() < 1e-3 && (l[1] - y).abs() < 1e-3 {
                return;
            }
        }
        if self.points.len() < 4096 {
            self.points.push([x, y]);
        }
    }
}

/// Rasterise a closed sketch and trace it to an outline.
///
/// Goes through a raster rather than handing the raw stroke to `from_points`
/// because a hand-drawn plan is an open, self-crossing, unevenly sampled path;
/// filling it and reading the silhouette is what makes it a boundary.
pub fn to_outline(
    s: &Sketch,
    name: &str,
) -> Result<ringdesign_core::field::CustomOutline, &'static str> {
    if s.points.len() < 8 {
        return Err("draw a closed shape — a few more points than that");
    }
    const N: u32 = 256;
    let mut d = ringdesign_core::drawn::DrawnAlpha::new("sketch", N, N);
    let mut st = ringdesign_core::drawn::Stroke::new(0.02, 0.0, false);
    for p in &s.points {
        st.push(p[0], p[1], 1.0);
    }
    // Close it: the last point back to the first, so the fill has a boundary.
    if let Some(f) = s.points.first() {
        st.push(f[0], f[1], 1.0);
    }
    d.strokes.push(st);
    let a = d.rasterize();
    let pts = ringdesign_core::contour::trace(&a, 0.4).ok_or("that mark is too small to be a plan")?;
    ringdesign_core::field::CustomOutline::from_points(name, &pts)
        .ok_or("could not read a boundary out of that")
}

/// Resample a drawn half-section into a drop curve.
///
/// `x` runs across the half-width from the crest to the edge, `y` is the drop.
/// The curve is stored as at most `MAX_DROP_POINTS` control points, so this is
/// a decimation, not a fit.
pub fn to_drop_curve(s: &Sketch) -> Result<ringdesign_core::profile::DropCurve, &'static str> {
    use ringdesign_core::profile::MAX_DROP_POINTS;
    if s.points.len() < 3 {
        return Err("draw a line across the section");
    }
    // Left to right, so the curve reads from crest to edge whichever way it was
    // drawn — a jeweller sketching right-to-left should not get it mirrored.
    let mut pts: Vec<[f32; 2]> = s.points.clone();
    pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));

    let take = MAX_DROP_POINTS.min(pts.len());
    let step = pts.len() as f64 / take as f64;
    let picked: Vec<[f64; 2]> = (0..take)
        .map(|i| {
            let p = pts[((i as f64 * step) as usize).min(pts.len() - 1)];
            [p[0] as f64, p[1] as f64]
        })
        .collect();

    let mut c = ringdesign_core::profile::DropCurve::from_points(&picked);
    c.monotone = !s.allow_undercut;
    if !c.is_active() {
        return Err("that did not resolve to a curve — try a longer line");
    }
    Ok(c)
}

/// Draw the sketch pad. Returns true when a point was added.
pub fn pad(ui: &mut egui::Ui, s: &mut Sketch, mode: Mode, accepts: bool) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(20, 21, 26));

    // The reference the shape is drawn against: a face is centred, a section
    // runs crest to edge left to right.
    let guide = egui::Stroke::new(1.0, egui::Color32::from_rgb(58, 60, 68));
    match mode {
        Mode::Face => {
            painter.line_segment([rect.center_top(), rect.center_bottom()], guide);
            painter.line_segment([rect.left_center(), rect.right_center()], guide);
        }
        Mode::Section => {
            painter.line_segment([rect.left_top(), rect.left_bottom()], guide);
            painter.line_segment([rect.left_bottom(), rect.right_bottom()], guide);
        }
    }

    let mut added = false;
    if accepts {
        if response.drag_started() {
            s.clear();
        }
        if let Some(p) = response.interact_pointer_pos().filter(|p| rect.contains(*p)) {
            if response.dragged() || response.drag_started() {
                s.push(
                    ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0),
                    ((p.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0),
                );
                added = true;
            }
        }
    }

    let to_screen = |q: &[f32; 2]| {
        egui::pos2(rect.left() + q[0] * rect.width(), rect.top() + q[1] * rect.height())
    };
    // Red while an undercut is permitted: that is the state that makes the
    // shape uncastable in sand, and it should be visible without reading a
    // checkbox.
    let ink = if mode == Mode::Section && s.allow_undercut {
        egui::Color32::from_rgb(240, 105, 120)
    } else {
        egui::Color32::from_rgb(203, 166, 247)
    };
    if s.points.len() >= 2 {
        let path: Vec<egui::Pos2> = s.points.iter().map(to_screen).collect();
        painter.add(egui::Shape::line(path, egui::Stroke::new(2.0, ink)));
        if mode == Mode::Face {
            // Show the closure the trace will make.
            painter.line_segment(
                [to_screen(s.points.last().unwrap()), to_screen(&s.points[0])],
                egui::Stroke::new(1.0, ink.gamma_multiply(0.5)),
            );
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circle(n: usize) -> Sketch {
        let mut s = Sketch::default();
        for i in 0..n {
            let t = i as f32 / n as f32 * std::f32::consts::TAU;
            s.push(0.5 + 0.35 * t.cos(), 0.5 + 0.35 * t.sin());
        }
        s
    }

    #[test]
    fn a_drawn_circle_becomes_a_round_outline() {
        let o = to_outline(&circle(64), "drawn").expect("traced");
        assert_eq!(o.name, "drawn");
        assert!((o.aspect - 1.0).abs() < 0.2, "aspect {}", o.aspect);
    }

    #[test]
    fn too_few_points_is_refused_with_a_reason() {
        let mut s = Sketch::default();
        s.push(0.1, 0.1);
        s.push(0.9, 0.9);
        assert!(to_outline(&s, "x").is_err());
    }

    #[test]
    fn a_drawn_section_becomes_a_monotone_curve_by_default() {
        let mut s = Sketch::default();
        for i in 0..40 {
            let x = i as f32 / 39.0;
            s.push(x, x * 0.8);
        }
        let c = to_drop_curve(&s).expect("curve");
        assert!(c.is_active());
        assert!(c.monotone, "the no-undercut guarantee is the default");
        assert!(c.len() <= ringdesign_core::profile::MAX_DROP_POINTS);
    }

    #[test]
    fn allowing_an_undercut_clears_monotone_and_nothing_else_does() {
        let mut s = Sketch::default();
        for i in 0..20 {
            s.push(i as f32 / 19.0, 0.5);
        }
        assert!(to_drop_curve(&s).expect("curve").monotone);
        s.allow_undercut = true;
        assert!(!to_drop_curve(&s).expect("curve").monotone);
    }

    /// Sketching right-to-left must not mirror the section.
    #[test]
    fn a_section_drawn_backwards_reads_the_same_way_round() {
        let mut fwd = Sketch::default();
        let mut back = Sketch::default();
        for i in 0..30 {
            let x = i as f32 / 29.0;
            fwd.push(x, x * 0.6);
        }
        for i in (0..30).rev() {
            let x = i as f32 / 29.0;
            back.push(x, x * 0.6);
        }
        let (a, b) = (to_drop_curve(&fwd).unwrap(), to_drop_curve(&back).unwrap());
        assert_eq!(a.len(), b.len());
        for (p, q) in a.points().iter().zip(b.points()) {
            assert!((p[0] - q[0]).abs() < 1e-6 && (p[1] - q[1]).abs() < 1e-6, "{p:?} vs {q:?}");
        }
    }

    #[test]
    fn push_drops_repeats_the_way_a_stroke_does() {
        let mut s = Sketch::default();
        s.push(0.5, 0.5);
        s.push(0.5, 0.5);
        s.push(0.50001, 0.5);
        assert_eq!(s.points.len(), 1);
    }
}
