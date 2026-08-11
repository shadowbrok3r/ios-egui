//! Orbit camera with an orthographic projection.
//!
//! World up is +Z, the finger axis, so a pitch of 90 degrees looks straight
//! down at the face of the ring. Matrices are column-major for OpenGL.

use ringdesign_core::mesh::Vec3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandardView {
    Face,
    Edge,
    Profile,
    Iso,
}

impl StandardView {
    pub const ALL: &'static [StandardView] = &[
        StandardView::Face,
        StandardView::Edge,
        StandardView::Profile,
        StandardView::Iso,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StandardView::Face => "Face",
            StandardView::Edge => "Edge",
            StandardView::Profile => "Profile",
            StandardView::Iso => "3/4",
        }
    }

    /// `(yaw, pitch)` in radians.
    fn angles(self) -> (f32, f32) {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
        match self {
            StandardView::Face => (-FRAC_PI_2, FRAC_PI_2 - 0.001),
            StandardView::Edge => (-FRAC_PI_2, 0.0),
            StandardView::Profile => (0.0, 0.0),
            StandardView::Iso => (-FRAC_PI_2 - FRAC_PI_4 * 0.5, FRAC_PI_4 * 0.85),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OrbitCamera {
    pub yaw: f32,
    pub pitch: f32,
    /// Zoom factor: 1.0 frames the model exactly.
    pub zoom: f32,
    pub target: [f32; 3],
    pub pan: [f32; 2],
    /// Radius of the fitted bounding sphere, mm.
    radius: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        let (yaw, pitch) = StandardView::Iso.angles();
        Self {
            yaw,
            pitch,
            zoom: 1.0,
            target: [0.0; 3],
            pan: [0.0; 2],
            radius: 12.0,
        }
    }
}

impl OrbitCamera {
    /// Recentre on new bounds, keeping the current orientation and zoom.
    pub fn fit(&mut self, bounds: Option<(Vec3, Vec3)>) {
        let Some((min, max)) = bounds else { return };
        self.target = [
            (min.0 + max.0) * 0.5,
            (min.1 + max.1) * 0.5,
            (min.2 + max.2) * 0.5,
        ];
        let ext = [max.0 - min.0, max.1 - min.1, max.2 - min.2];
        let r = 0.5 * (ext[0] * ext[0] + ext[1] * ext[1] + ext[2] * ext[2]).sqrt();
        self.radius = r.max(1.0);
    }

    pub fn reset(&mut self) {
        let keep_radius = self.radius;
        *self = Self::default();
        self.radius = keep_radius;
    }

    pub fn set_view(&mut self, view: StandardView) {
        let (yaw, pitch) = view.angles();
        self.yaw = yaw;
        self.pitch = pitch;
        self.pan = [0.0, 0.0];
    }

    pub fn orbit(&mut self, delta: egui::Vec2) {
        use std::f32::consts::FRAC_PI_2;
        self.yaw -= delta.x * 0.008;
        self.pitch = (self.pitch + delta.y * 0.008).clamp(-FRAC_PI_2 + 0.001, FRAC_PI_2 - 0.001);
    }

    pub fn zoom_by(&mut self, scroll: f32) {
        self.zoom = (self.zoom * (1.0 + scroll * 0.0015)).clamp(0.15, 24.0);
    }

    /// Multiply the zoom directly. This is what a pinch reports —
    /// `MultiTouchInfo::zoom_delta` is already a ratio, so running it through the scroll-wheel
    /// curve in [`zoom_by`] would scale a 10% pinch down to a fifth of a percent.
    pub fn zoom_by_factor(&mut self, factor: f32) {
        if factor.is_finite() && factor > 0.0 {
            self.zoom = (self.zoom * factor).clamp(0.15, 24.0);
        }
    }

    /// Set the zoom so the view volume is `half_mm` tall in millimetres. The projection is
    /// orthographic and already in mm, so true physical scale is this one value.
    pub fn set_half_extent(&mut self, half_mm: f32) {
        if half_mm > 1e-6 {
            self.zoom = (self.radius * 1.15 / half_mm).clamp(0.15, 24.0);
        }
    }

    pub fn pan_by(&mut self, delta: egui::Vec2, rect_height: f32) {
        let scale = self.half_extent() * 2.0 / rect_height.max(1.0);
        self.pan[0] -= delta.x * scale;
        self.pan[1] += delta.y * scale;
    }

    /// Half-height of the orthographic view volume, in mm.
    pub fn half_extent(&self) -> f32 {
        self.radius * 1.15 / self.zoom.max(1e-3)
    }

    /// World-space ray under a screen position: the orthographic unproject.
    /// Origin sits on the eye plane, direction is the view forward.
    pub fn ray(&self, rect: egui::Rect, pos: egui::Pos2) -> ([f32; 3], [f32; 3]) {
        let eye = self.eye();
        let up = if self.pitch.abs() > std::f32::consts::FRAC_PI_2 - 0.02 {
            [-self.yaw.cos(), -self.yaw.sin(), 0.0]
        } else {
            [0.0, 0.0, 1.0]
        };
        let f = normalize(sub(self.target, eye));
        let s = normalize(cross(f, up));
        let u = cross(s, f);

        let centre = rect.center();
        let half = rect.size() * 0.5;
        let x_ndc = (pos.x - centre.x) / half.x.max(1.0);
        let y_ndc = -(pos.y - centre.y) / half.y.max(1.0);
        let aspect = (rect.width() / rect.height().max(1.0)).max(1e-3);
        let hh = self.half_extent();
        let vx = self.pan[0] + x_ndc * hh * aspect;
        let vy = self.pan[1] + y_ndc * hh;

        let origin = [
            eye[0] + s[0] * vx + u[0] * vy,
            eye[1] + s[1] * vx + u[1] * vy,
            eye[2] + s[2] * vx + u[2] * vy,
        ];
        (origin, f)
    }

    fn eye(&self) -> [f32; 3] {
        let d = self.radius * 4.0;
        let (sp, cp) = self.pitch.sin_cos();
        let (sy, cy) = self.yaw.sin_cos();
        [
            self.target[0] + d * cp * cy,
            self.target[1] + d * cp * sy,
            self.target[2] + d * sp,
        ]
    }

    /// `(mvp, normal_matrix)` for the given viewport rect.
    pub fn matrices(&self, rect: egui::Rect) -> ([f32; 16], [f32; 9]) {
        let eye = self.eye();
        let up = if self.pitch.abs() > std::f32::consts::FRAC_PI_2 - 0.02 {
            [-self.yaw.cos(), -self.yaw.sin(), 0.0]
        } else {
            [0.0, 0.0, 1.0]
        };
        let view = look_at(eye, self.target, up);

        let aspect = (rect.width() / rect.height().max(1.0)).max(1e-3);
        let hh = self.half_extent();
        let hw = hh * aspect;
        let far = self.radius * 12.0;
        let proj = ortho(
            -hw + self.pan[0],
            hw + self.pan[0],
            -hh + self.pan[1],
            hh + self.pan[1],
            -far,
            far,
        );

        let mvp = mat4_mul(&proj, &view);
        // View rotation is orthonormal, so it is its own normal matrix.
        let normal = [
            view[0], view[1], view[2],
            view[4], view[5], view[6],
            view[8], view[9], view[10],
        ];
        (mvp, normal)
    }

    /// The projection for one viewport rect, with the matrices resolved once.
    ///
    /// Taken once per overlay rather than per point: the ground grid alone
    /// projects over a hundred, and every one would rebuild the matrices.
    pub fn projector(&self, rect: egui::Rect) -> Projector {
        let (mvp, _) = self.matrices(rect);
        Projector { mvp, centre: rect.center(), half: rect.size() * 0.5 }
    }
}

/// World-to-screen for a fixed camera and rect.
#[derive(Clone, Copy)]
pub struct Projector {
    mvp: [f32; 16],
    centre: egui::Pos2,
    half: egui::Vec2,
}

impl Projector {
    pub fn at(&self, p: [f32; 3]) -> egui::Pos2 {
        let m = &self.mvp;
        let x = m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12];
        let y = m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13];
        egui::pos2(self.centre.x + x * self.half.x, self.centre.y - y * self.half.y)
    }
}

fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> [f32; 16] {
    let f = normalize(sub(target, eye));
    let s = normalize(cross(f, up));
    let u = cross(s, f);
    [
        s[0], u[0], -f[0], 0.0,
        s[1], u[1], -f[1], 0.0,
        s[2], u[2], -f[2], 0.0,
        -dot(s, eye), -dot(u, eye), dot(f, eye), 1.0,
    ]
}

fn ortho(l: f32, r: f32, b: f32, t: f32, n: f32, f: f32) -> [f32; 16] {
    let (rl, tb, fnn) = ((r - l).max(1e-6), (t - b).max(1e-6), (f - n).max(1e-6));
    [
        2.0 / rl, 0.0, 0.0, 0.0,
        0.0, 2.0 / tb, 0.0, 0.0,
        0.0, 0.0, -2.0 / fnn, 0.0,
        -(r + l) / rl, -(t + b) / tb, -(f + n) / fnn, 1.0,
    ]
}

/// Column-major 4x4 product `a * b`.
fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[k * 4 + row] * b[col * 4 + k];
            }
            out[col * 4 + row] = sum;
        }
    }
    out
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let len = dot(a, a).sqrt();
    if len > 1e-9 { [a[0] / len, a[1] / len, a[2] / len] } else { [0.0, 0.0, 1.0] }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0))
    }

    #[test]
    fn origin_projects_to_the_centre_when_centred() {
        let cam = OrbitCamera::default();
        let p = cam.projector(rect()).at([0.0, 0.0, 0.0]);
        assert!((p.x - rect().center().x).abs() < 1.0);
        assert!((p.y - rect().center().y).abs() < 1.0);
    }

    #[test]
    fn fit_centres_on_the_bounds() {
        let mut cam = OrbitCamera::default();
        cam.fit(Some((Vec3(-2.0, -4.0, -1.0), Vec3(4.0, 2.0, 3.0))));
        assert!((cam.target[0] - 1.0).abs() < 1e-6);
        assert!((cam.target[1] + 1.0).abs() < 1e-6);
        assert!((cam.target[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut cam = OrbitCamera::default();
        for _ in 0..500 {
            cam.zoom_by(1000.0);
        }
        assert!(cam.zoom <= 24.0);
        for _ in 0..1000 {
            cam.zoom_by(-1000.0);
        }
        assert!(cam.zoom >= 0.15);
    }

    #[test]
    fn pitch_never_flips_past_the_pole() {
        let mut cam = OrbitCamera::default();
        for _ in 0..400 {
            cam.orbit(egui::vec2(0.0, 100.0));
        }
        assert!(cam.pitch < std::f32::consts::FRAC_PI_2);
        for _ in 0..800 {
            cam.orbit(egui::vec2(0.0, -100.0));
        }
        assert!(cam.pitch > -std::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn mat4_mul_has_identity() {
        let id: [f32; 16] = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let m = ortho(-1.0, 1.0, -1.0, 1.0, -1.0, 1.0);
        let out = mat4_mul(&m, &id);
        for i in 0..16 {
            assert!((out[i] - m[i]).abs() < 1e-6);
        }
    }
}
