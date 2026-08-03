//! Consumer probe: the exact public API a downstream app touches, typed against egui 0.35.
//! Compiling this file is the API report; `tests/end_to_end.rs` executes it on real GL.

use std::sync::Arc;

use backdrop_blur_egui::{
    BlurRadius, CornerRadius, FrostOutcome, GrabPassRenderer, Presence, RepaintPolicy, Surface,
    Tint,
};

/// Every constructor a consumer needs to build a frosted surface.
pub fn build_surface(rect: egui::Rect) -> Surface {
    Surface {
        rect,
        blur_radius: BlurRadius::new(24.0),
        tint: Tint::from_srgb_unmultiplied([255, 255, 255, 40]),
        corner_radius: CornerRadius::new(12.0),
        presence: Presence::new(0.85),
        repaint: RepaintPolicy::Live,
    }
}

/// The other two `Presence` spellings, and the linear-color `Tint` constructor.
pub fn alternate_material() -> (Presence, Presence, Tint) {
    (
        Presence::FULL,
        Presence::default(),
        Tint::new(backdrop_blur_egui::LinearRgba::new(0.02, 0.02, 0.03, 0.35)),
    )
}

/// `GrabPassRenderer::new` takes `&Arc<glow::Context>` — the `glow` re-exported by the adapter.
/// The error type is `backdrop_blur_core::BlurError`, which the adapter does NOT re-export, so a
/// consumer that wants to name it must depend on `backdrop-blur-core` too. Erasing it works:
pub fn make_renderer(
    gl: &Arc<backdrop_blur_egui::glow::Context>,
) -> Result<GrabPassRenderer, Box<dyn std::error::Error>> {
    Ok(GrabPassRenderer::new(gl)?)
}

/// The per-frame call: enqueue the frost, then paint the foreground on top.
pub fn frame(renderer: &GrabPassRenderer, ui: &mut egui::Ui, rect: egui::Rect) {
    renderer.frost(ui, build_surface(rect));
    ui.painter()
        .text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "on top of the glass",
            egui::FontId::proportional(14.0),
            egui::Color32::WHITE,
        );
}

/// Read-and-clear, once per frame after paint.
pub fn poll(renderer: &GrabPassRenderer) -> FrostOutcome {
    renderer.take_frost_outcome()
}

/// Teardown takes `&glow::Context` (not the `Arc`).
pub fn shutdown(renderer: &GrabPassRenderer, gl: &backdrop_blur_egui::glow::Context) {
    renderer.destroy(gl);
}

