//! The 3D ring pane, and the background rebuild that feeds it.
//!
//! Touch mapping follows the Nomad convention the framework's stylus probe makes possible: one
//! finger orbits, two fingers pinch-zoom and pan. The desktop's shift-drag and scroll-wheel have no
//! touch equivalent and are gone.
//!
//! The worker differs from the desktop's in two measured ways. `Mesh::validate` is 26–43% of its
//! job and re-proves watertightness the sweep guarantees by construction, so it is skipped;
//! `castability::analyze` is another ~30% and only matters when you stop moving, so it is deferred
//! to the settled build. And the vertex buffer is staged here rather than on the UI thread — at
//! 384x144 it is ~12 MB, which is more than a frame.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};

use egui_mobile::egui;
use ringdesign_core::alpha::AlphaLibrary;
use ringdesign_core::castability::{self, CastReport};
use ringdesign_core::mesh::{BuildParams, Vec3};
use ringdesign_core::RingDesign;

use crate::camera::OrbitCamera;
use crate::viewport::{GpuMeshRenderer, ShadeMode, paint_callback};

/// Measured on an S26 Ultra: 34 ms build + 14 ms analyze at 384x144. The desktop's own Preview
/// resolution is interactive on the phone, so there is no scrub tier.
pub const PREVIEW: BuildParams =
    BuildParams { theta_steps: 384, profile_steps: 144, min_wall_mm: 0.5, adaptive: false, refine: None };
/// 655k triangles, 226 ms end to end on device. The ceiling — 1536x448 is a 149 MB vertex buffer.
pub const EXPORT: BuildParams =
    BuildParams { theta_steps: 1024, profile_steps: 320, min_wall_mm: 0.5, adaptive: false, refine: None };

pub struct RingPane {
    pub camera: OrbitCamera,
    pub shade: ShadeMode,
    pub wireframe: bool,
    /// Render at true physical size using the panel's real pixel density.
    pub actual_size: bool,
}

impl Default for RingPane {
    fn default() -> Self {
        Self {
            camera: OrbitCamera::default(),
            shade: ShadeMode::default(),
            wireframe: false,
            actual_size: false,
        }
    }
}

impl RingPane {
    /// Draw the pane. Returns true if the camera moved, so the caller can keep repainting.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        renderer: &Arc<Mutex<GpuMeshRenderer>>,
        px_per_mm: Option<f32>,
    ) -> bool {
        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        if !ui.is_rect_visible(rect) {
            return false;
        }

        ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 18, 20));

        if let Ok(r) = renderer.lock() {
            if let Some(err) = r.failed.as_ref() {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("shader failed\n{err}"),
                    egui::FontId::monospace(12.0),
                    egui::Color32::from_rgb(240, 120, 120),
                );
                return false;
            }
            if !r.has_mesh() {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "building…",
                    egui::FontId::proportional(14.0),
                    egui::Color32::GRAY,
                );
                return false;
            }
        }

        let moved = self.handle_touch(ui, &response, rect);

        // True scale: the camera is orthographic and already in millimetres, so physical size is
        // one zoom value rather than a different render path.
        if let (true, Some(ppmm)) = (self.actual_size, px_per_mm) {
            let ppp = ui.ctx().pixels_per_point();
            let pt_per_mm = ppmm / ppp;
            let half_mm = rect.height() * 0.5 / pt_per_mm;
            self.camera.set_half_extent(half_mm);
        }

        let (mvp, normal_matrix) = self.camera.matrices(rect);
        paint_callback(
            ui,
            rect,
            renderer.clone(),
            mvp,
            normal_matrix,
            self.shade,
            [0.86, 0.80, 0.62],
            self.wireframe,
            [0.10, 0.10, 0.12],
        );
        moved
    }

    /// One finger orbits, two pinch-zoom and pan. Returns whether anything changed.
    fn handle_touch(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        rect: egui::Rect,
    ) -> bool {
        let multi = ui.input(|i| i.multi_touch());
        if let Some(mt) = multi {
            // A second finger takes the gesture from the orbit outright, so a pinch never also
            // spins the ring.
            if (mt.zoom_delta - 1.0).abs() > 1e-4 {
                self.camera.zoom_by_factor(mt.zoom_delta);
            }
            if mt.translation_delta != egui::Vec2::ZERO {
                self.camera.pan_by(mt.translation_delta, rect.height());
            }
            return true;
        }
        if response.dragged() {
            self.camera.orbit(response.drag_delta());
            return true;
        }
        false
    }
}

// --- Background rebuild ------------------------------------------------------

/// A finished build, with the vertex buffer already staged off the UI thread.
pub struct Done {
    pub generation: u64,
    pub verts: Vec<f32>,
    pub bounds: Option<(Vec3, Vec3)>,
    pub triangles: usize,
    pub volume_mm3: f64,
    pub build_ms: u128,
    /// Only present on a settled build — analyze is ~30% of the worker and is not worth paying
    /// while a slider is still moving.
    pub cast: Option<CastReport>,
}

struct Job {
    generation: u64,
    design: RingDesign,
    lib: Arc<AlphaLibrary>,
    params: BuildParams,
    analyze: bool,
}

pub struct Worker {
    jobs: Sender<Job>,
    pub done: Receiver<Done>,
}

impl Worker {
    pub fn spawn(ctx: egui::Context) -> Self {
        let (jobs_tx, jobs_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Done>();
        std::thread::Builder::new()
            .name("ring-build".into())
            .spawn(move || {
                while let Ok(mut job) = jobs_rx.recv() {
                    // Skip stale work: only the newest queued job matters.
                    while let Ok(newer) = jobs_rx.try_recv() {
                        job = newer;
                    }
                    let out = ringdesign_core::mesh::build(&job.design, &job.lib, job.params);
                    let cast = job.analyze.then(|| {
                        castability::analyze(
                            &out.mesh,
                            &job.design.draft,
                            job.design.inner_radius_mm(),
                        )
                    });
                    let verts = GpuMeshRenderer::stage(&out.mesh, cast.as_ref());
                    let done = Done {
                        generation: job.generation,
                        verts,
                        bounds: out.mesh.bounds(),
                        triangles: out.report.validation.triangle_count,
                        volume_mm3: out.report.volume_mm3,
                        build_ms: out.report.build_ms,
                        cast,
                    };
                    if done_tx.send(done).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn build worker");
        Self { jobs: jobs_tx, done: done_rx }
    }

    pub fn dispatch(
        &self,
        generation: u64,
        design: &RingDesign,
        lib: &Arc<AlphaLibrary>,
        params: BuildParams,
        analyze: bool,
    ) -> bool {
        self.jobs
            .send(Job {
                generation,
                design: design.clone(),
                lib: lib.clone(),
                params,
                analyze,
            })
            .is_ok()
    }

    pub fn poll(&self) -> Option<Done> {
        match self.done.try_recv() {
            Ok(d) => Some(d),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preview_preset_is_the_desktops_own() {
        assert_eq!(PREVIEW.theta_steps, 384);
        assert_eq!(PREVIEW.profile_steps, 144);
        assert_eq!(PREVIEW.triangle_estimate(), 110_592);
    }

    #[test]
    fn export_stays_under_the_memory_ceiling() {
        // 36 bytes a vertex, three vertices a triangle. Maximum (1536x448) would be 149 MB.
        let bytes = EXPORT.triangle_estimate() * 3 * 36;
        assert!(bytes < 80 * 1024 * 1024, "{bytes} bytes is too much to re-upload");
    }

    #[test]
    fn neither_preset_asks_for_a_refined_build() {
        assert!(PREVIEW.refine.is_none());
        assert!(EXPORT.refine.is_none());
    }
}
