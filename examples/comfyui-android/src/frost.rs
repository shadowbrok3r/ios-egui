//! Real backdrop blur behind the app's floating panes, via a grab-pass paint callback.
//!
//! egui cannot do this itself: `blur_width` feathers a shape's *own* edges and never samples what
//! is behind it. The grab-pass reaches into the live framebuffer mid-frame, blurs the region under
//! a pane and composites the frosted result back — which needs an OpenGL context, hence
//! [`egui_mobile::Backend::Glow`] in `lib.rs`.
//!
//! What gets frosted, and why not the chrome: an egui panel reserves its strip, so nothing is ever
//! *behind* the nav bar or the progress bar — frosting those would blur the black page. The panes
//! that do float over content are the modal windows, the menus, and the viewer's metadata sheet, so
//! those are what this module frosts.
//!
//! Three contracts from the crate, all load-bearing here:
//!
//! 1. **Frost before the pane's content paints.** The callback grabs whatever is already in the
//!    framebuffer at its position. Handled by layer order: everything here paints into one
//!    [`egui::Order::Middle`] area, which egui draws after the panels and before every floating
//!    pane, whatever order the calls are made in.
//! 2. **A pane's rect is only known after it lays out.** In immediate mode that fights (1), so this
//!    reads last frame's rect out of egui's area store. The only artifact is one frame of staleness
//!    while a pane resizes.
//! 3. **`multiply_opacity` never reaches paint callbacks** — a window fading out keeps its frost at
//!    full strength for the length of the fade.

#[cfg(target_os = "android")]
pub use android::glass_panes;

#[cfg(not(target_os = "android"))]
#[allow(unused_imports)]
pub use stub::glass_panes;

/// Off Android there is no GL context, so this is a no-op and the translucent fills stand alone.
#[cfg(not(target_os = "android"))]
mod stub {
    pub fn glass_panes(_ctx: &egui::Context) {}
}

#[cfg(target_os = "android")]
mod android {
    use backdrop_blur_egui::{
        BlurRadius, CornerRadius, FrostOutcome, GrabPassRenderer, Presence, RepaintPolicy, Surface,
        Tint,
    };
    use std::sync::OnceLock;
    use std::time::Duration;

    /// Built once from the live `glow` context. `None` if the app is not on the glow backend or the
    /// shaders failed to compile — the translucent fills then stand alone, which is a serviceable
    /// fallback rather than an error worth surfacing.
    static RENDERER: OnceLock<Option<GrabPassRenderer>> = OnceLock::new();

    /// The glass film over the blur. A *light* tint lifts a pane to roughly the brightness of the
    /// text on it, which washes the blur out and drops label contrast; this is a dark smoked film
    /// that leaves most of the blur visible and pairs with the translucent `window_fill` painted
    /// over it (the two alphas multiply — see `theme::apply`).
    const TINT: [u8; 4] = [8, 11, 16, 90];
    const BLUR: f32 = 24.0;
    /// Matches `menu_corner_radius` / `window_corner_radius`.
    const CORNER: f32 = 8.0;
    /// A pane wider and taller than this fraction of the screen is a full-screen scrim, not glass.
    const SCRIM: f32 = 0.9;

    /// Floating areas that paint their own fill — round FABs, pills, the toast, the perf HUD. They
    /// are not panes of `window_fill`, so a rectangle of glass behind them would be a rectangle
    /// hanging off a circle.
    const OPAQUE: [&str; 10] = [
        "queue-fab",
        "cancel-fab",
        "create-fab",
        "comfy-lock",
        "comfy-minimap",
        "comfy-undo",
        "inpaint-tool-stack",
        "undo-trash-pill",
        "graph-toast",
        "perf-hud",
    ];

    fn renderer() -> Option<&'static GrabPassRenderer> {
        RENDERER
            .get_or_init(|| {
                let gl = egui_mobile::glow_context()?;
                match GrabPassRenderer::new(&gl) {
                    Ok(renderer) => {
                        log::info!("comfyui: backdrop blur ready");
                        Some(renderer)
                    }
                    Err(error) => {
                        log::warn!("comfyui: backdrop blur unavailable: {error}");
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Frost every floating pane that was open last frame. Call once per frame; where in the frame
    /// does not matter, because the area this opens is ordered below every pane it frosts.
    ///
    /// Membership is by layer order rather than an opt-in list: everything above the panels is a
    /// pane of `window_fill` — modal windows, hover tooltips and the viewer's metadata sheet ride
    /// `Tooltip`, menus, submenus and combo popups ride `Foreground` — so the glass is consistent
    /// without every call site having to ask for it. The exceptions are [`OPAQUE`] and the
    /// full-screen scrims, which the size guard catches.
    pub fn glass_panes(ctx: &egui::Context) {
        let Some(renderer) = renderer() else { return };

        let screen = ctx.content_rect();
        let opaque: Vec<egui::Id> = OPAQUE.iter().map(egui::Id::new).collect();
        let mut panes: Vec<egui::Rect> = ctx.memory(|m| {
            m.areas()
                .visible_layer_ids()
                .iter()
                .filter(|layer| {
                    matches!(layer.order, egui::Order::Foreground | egui::Order::Tooltip)
                        && !opaque.contains(&layer.id)
                })
                .filter_map(|layer| m.area_rect(layer.id))
                .collect()
        });
        panes.retain(|rect| {
            let rect = rect.intersect(screen);
            rect.width() > 1.0
                && rect.height() > 1.0
                && !(rect.width() > screen.width() * SCRIM && rect.height() > screen.height() * SCRIM)
        });
        if panes.is_empty() {
            return;
        }

        // `Live` drives a repaint every frame, which is only worth paying while the backdrop is
        // actually moving; idle, a frosted pane costs nothing over the app's own cadence.
        let moving = ctx
            .input(|i| i.pointer.any_down() || i.any_touches() || i.is_scrolling());
        let repaint = if moving {
            RepaintPolicy::Live
        } else {
            RepaintPolicy::Bounded(Duration::from_millis(400))
        };
        let tint = Tint::from_srgb_unmultiplied(TINT);

        egui::Area::new(egui::Id::new("frost-glass"))
            .order(egui::Order::Middle)
            .fixed_pos(screen.min)
            .movable(false)
            .interactable(false)
            .show(ctx, |ui| {
                // The callback grabs `viewport ∩ clip_rect`, so the clip has to cover every pane
                // rather than this area's own (empty) content.
                ui.set_clip_rect(screen);
                for rect in panes {
                    renderer.frost(
                        ui,
                        Surface {
                            rect,
                            blur_radius: BlurRadius::new(BLUR),
                            tint,
                            corner_radius: CornerRadius::new(CORNER),
                            presence: Presence::new(1.0),
                            repaint,
                        },
                    );
                }
            });

        // Read-and-clear every frame: egui skips a fully-clipped callback, so a silent
        // `DidNotFire` is the wiring/version-skew signal and worth a line in logcat.
        if renderer.take_frost_outcome() == FrostOutcome::DidNotFire {
            log::debug!("comfyui: frost callback did not fire this frame");
        }
    }
}
