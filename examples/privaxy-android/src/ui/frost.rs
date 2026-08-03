//! Real backdrop blur behind the fixed chrome, via a grab-pass paint callback.
//!
//! egui itself cannot do this: `blur_width` feathers a shape's *own* edges and never samples what
//! is behind it. The grab-pass reaches into the live framebuffer mid-frame, blurs the region under
//! a pane and composites the frosted result back — which needs an OpenGL context, hence
//! [`egui_mobile::Backend::Glow`].
//!
//! Three contracts from the crate, all load-bearing here:
//!
//! 1. **Frost before the pane's content paints.** The callback grabs whatever is already in the
//!    framebuffer at its position, so frosting after the content would blur the content away.
//! 2. **A pane's rect is only known after it lays out.** In immediate mode that is a
//!    chicken-and-egg with (1), so last frame's rect is stashed and frosted at the top of this one.
//!    The only artifact is one frame of staleness on rotation.
//! 3. **`multiply_opacity` never reaches paint callbacks** — fading has to go through `Presence`.
//!    Nothing here fades, so this is just why no opacity is applied.

#[cfg(target_os = "android")]
pub use android::{Panes, frost_chrome, remember};

/// Off Android there is no GL context, so these are no-ops and the simulated glass stands alone.
#[cfg(not(target_os = "android"))]
mod stub {
    #[derive(Default)]
    pub struct Panes;

    pub fn remember(_ctx: &egui_mobile::egui::Context, _top: egui_mobile::egui::Rect, _bottom: egui_mobile::egui::Rect) {}
    pub fn frost_chrome(_ui: &mut egui_mobile::egui::Ui) {}
}

#[cfg(not(target_os = "android"))]
pub use stub::{Panes, frost_chrome, remember};

#[cfg(target_os = "android")]
mod android {
    use backdrop_blur_egui::{
        BlurRadius, CornerRadius, FrostOutcome, GrabPassRenderer, Presence, RepaintPolicy, Surface,
        Tint,
    };
    use std::time::Duration;
    use egui_mobile::egui;
    use std::sync::{Mutex, OnceLock};

    /// Built once from the live `glow` context. `None` if the app is not on the glow backend or
    /// the shaders failed to compile — in which case the simulated glass is what shows, which is
    /// a perfectly good fallback rather than an error worth surfacing.
    static RENDERER: OnceLock<Option<GrabPassRenderer>> = OnceLock::new();
    /// Last frame's chrome rects. See contract 2.
    static PANES: Mutex<Option<(egui::Rect, egui::Rect)>> = Mutex::new(None);

    #[derive(Default)]
    pub struct Panes;

    fn renderer() -> Option<&'static GrabPassRenderer> {
        RENDERER
            .get_or_init(|| {
                let gl = egui_mobile::glow_context()?;
                match GrabPassRenderer::new(&gl) {
                    Ok(renderer) => {
                        log::info!("privaxy: backdrop blur ready");
                        Some(renderer)
                    }
                    Err(error) => {
                        log::warn!("privaxy: backdrop blur unavailable: {error}");
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Records this frame's chrome rects for the next frame to frost.
    pub fn remember(_ctx: &egui::Context, top: egui::Rect, bottom: egui::Rect) {
        if let Ok(mut panes) = PANES.lock() {
            *panes = Some((top, bottom));
        }
    }

    /// Frosts last frame's header and tab bar. Call at the very top of `update`, before anything
    /// paints into those rects.
    pub fn frost_chrome(ui: &mut egui::Ui) {
        let Some(renderer) = renderer() else {
            return;
        };
        let Some((top, bottom)) = PANES.lock().ok().and_then(|panes| *panes) else {
            return;
        };

        // A DARK violet film, not a light one. A pale tint lifts the chrome to roughly the
        // brightness of the text on it, which both washes the blur out and drops the contrast of
        // the tab labels; a smoked film keeps the bars dark, leaves the light text readable, and
        // lets the blur read as movement behind glass.
        let tint = Tint::from_srgb_unmultiplied([28, 10, 38, 148]);

        // `Live` drives a repaint every frame, which is only worth paying while the backdrop is
        // actually moving. Idle, this drops to the app's own 500ms poll cadence, so the blur costs
        // nothing over the existing baseline. (Backgrounded it costs nothing at all: the request
        // is issued *from* a frame, and a stopped activity paints none.)
        let moving = ui
            .ctx()
            .input(|input| input.pointer.any_down() || input.any_touches() || input.is_scrolling());
        let repaint = if moving {
            RepaintPolicy::Live
        } else {
            RepaintPolicy::Bounded(Duration::from_millis(500))
        };
        for rect in [top, bottom] {
            if rect.width() <= 0.0 || rect.height() <= 0.0 {
                continue;
            }
            renderer.frost(
                ui,
                Surface {
                    rect,
                    blur_radius: BlurRadius::new(28.0),
                    tint,
                    corner_radius: CornerRadius::new(0.0),
                    presence: Presence::new(1.0),
                    // Static would freeze the blur on whatever was behind it at grab time, so
                    // this tracks whether the content underneath is actually moving.
                    repaint,
                },
            );
        }

        // Read-and-clear every frame: egui skips a fully-clipped callback, so a silent
        // `DidNotFire` is the wiring/version-skew signal and worth a line in logcat once.
        let outcome = renderer.take_frost_outcome();
        if outcome == FrostOutcome::DidNotFire {
            log::debug!("privaxy: frost callback did not fire this frame");
        }
    }
}
