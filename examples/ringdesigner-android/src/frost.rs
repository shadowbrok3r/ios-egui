//! Real backdrop blur behind the bottom nav, via a grab-pass paint callback
//! (the vendored patches/backdrop-blur fork; needs the Glow backend).
//!
//! The load-bearing contracts, same as privaxy/comfyui:
//! 1. Frost BEFORE the pane's content paints — the callback grabs whatever is
//!    already in the framebuffer, so frosting after would blur the content.
//! 2. A pane's rect is only known after layout, so LAST frame's rect is
//!    stashed and frosted at the top of this one (one frame of staleness on
//!    rotation, invisible in practice).
//! 3. `multiply_opacity` never reaches paint callbacks; fades must use
//!    `Presence`. Nothing here fades — noted for whoever adds it.
//! 4. Poll `take_frost_outcome` once per frame: `DidNotFire` is the
//!    wiring/version-skew canary.

#[cfg(target_os = "android")]
pub use android::{frost_chrome, remember};

/// Off Android there is no GL context; the translucent theme fills stand alone.
#[cfg(not(target_os = "android"))]
mod stub {
    pub fn remember(_ctx: &egui::Context, _bar: egui::Rect) {}
    pub fn frost_chrome(_ui: &mut egui::Ui) {}
}

#[cfg(not(target_os = "android"))]
pub use stub::{frost_chrome, remember};

#[cfg(target_os = "android")]
mod android {
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use backdrop_blur_egui::{
        BlurRadius, CornerRadius, FrostOutcome, GrabPassRenderer, Presence, RepaintPolicy, Surface,
        Tint,
    };
    use egui_mobile::egui;

    /// `None` when not on the glow backend or the shaders failed — the
    /// simulated glass is the designed fallback, not an error.
    static RENDERER: OnceLock<Option<GrabPassRenderer>> = OnceLock::new();
    /// Last frame's nav-bar rect (contract 2).
    static BAR: Mutex<Option<egui::Rect>> = Mutex::new(None);

    fn renderer() -> Option<&'static GrabPassRenderer> {
        RENDERER
            .get_or_init(|| {
                let gl = egui_mobile::glow_context()?;
                match GrabPassRenderer::new(&gl) {
                    Ok(r) => {
                        log::info!("ringdesigner: backdrop blur ready");
                        Some(r)
                    }
                    Err(e) => {
                        log::warn!("ringdesigner: backdrop blur unavailable: {e}");
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Stash this frame's nav-bar rect for the next frame's frost.
    pub fn remember(_ctx: &egui::Context, bar: egui::Rect) {
        *BAR.lock().unwrap() = Some(bar);
    }

    /// Frost last frame's nav-bar rect. Call at the very top of `update`,
    /// before any chrome paints (contract 1).
    pub fn frost_chrome(ui: &mut egui::Ui) {
        let Some(renderer) = renderer() else { return };
        let Some(bar) = *BAR.lock().unwrap() else { return };

        // Live only while the user is interacting; otherwise a bounded cadence
        // (the radar view's 33 ms repaints keep it fresh on the Live tab).
        let interacting =
            ui.input(|i| i.pointer.any_down() || i.any_touches() || i.is_scrolling());
        let repaint = if interacting {
            RepaintPolicy::Live
        } else {
            RepaintPolicy::Bounded(Duration::from_millis(500))
        };

        renderer.frost(
            ui,
            Surface {
                rect: bar,
                blur_radius: BlurRadius::new(26.0),
                // Dark violet smoke: matches the galactic identity; light
                // tints wash the blur out and kill label contrast.
                tint: Tint::from_srgb_unmultiplied([13, 9, 20, 96]),
                corner_radius: CornerRadius::new(0.0),
                presence: Presence::new(1.0),
                repaint,
            },
        );

        if renderer.take_frost_outcome() == FrostOutcome::DidNotFire {
            log::debug!("ringdesigner: frost did not fire (wiring/version skew?)");
        }
    }
}
