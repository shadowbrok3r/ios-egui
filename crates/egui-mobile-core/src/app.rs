//! The single trait an app implements, shared across platforms.

use crate::Host;

/// Passed to the app factory at creation; carries the initial drawable geometry.
pub struct CreateContext {
    pub width_px: u32,
    pub height_px: u32,
    pub pixels_per_point: f32,
}

/// The single trait an app implements. Only [`EguiApp::update`] is required. The same
/// `impl EguiApp` compiles for both iOS and Android.
pub trait EguiApp: 'static {
    /// Build one frame of UI into the root [`egui::Ui`]. Use `ui.ctx()` for context-level calls
    /// and `egui::CentralPanel::default().show_inside(ui, ..)` for panels.
    fn update(&mut self, ui: &mut egui::Ui, host: &Host);

    /// Configure style and fonts once, before the first frame.
    fn theme(&self, _ctx: &egui::Context) {}

    /// Called once after the renderer and context are ready.
    fn on_start(&mut self, _ctx: &egui::Context, _host: &Host) {}

    /// Called when the app returns to the foreground.
    fn on_resume(&mut self, _host: &Host) {}

    /// Called when the app is backgrounded.
    fn on_pause(&mut self, _host: &Host) {}
}
