//! The brush: pen pressure to millimetres of metal, bounded by what the surface can actually hold.
//!
//! This is the part that makes the app worth building. Everywhere else pressure means opacity; here
//! it means *depth*, and the ceiling is not a preference — it comes from the geometry. The band's
//! local draft angle says how much relief a spot can carry and still pull out of sand:
//!
//! - a squared side face is measured clean to **1.6 mm**
//! - the crest of a half-round undercuts at 0.30 mm and is honest only to about **0.05 mm**
//!
//! So the same hard press gives you a deep cut on the flank and almost nothing on the crown, and
//! the stroke says so while you draw it rather than in a report afterwards.
//!
//! Pure — no egui, no Android — so all of it is host-testable.

// The pressure-to-millimetres math lives in the core now, shared with the
// desktop's unrolled editor — one behavior, one band-layer convention, so a
// band painted on either device opens on the other as the same layers. This
// module keeps what is Android's own: tool classification and palm policy.
pub use ringdesign_core::paint::{
    bite, ceiling_mm, ensure_band_layer, wanted_mm, Bite, BAND_ALPHA, MAX_RELIEF_MM,
    MIN_RELIEF_MM,
};

/// What the pointer is, as the paint surface sees it. Mirrors the framework's tool codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Unknown,
    Finger,
    Stylus,
    Mouse,
    Eraser,
    Palm,
}

impl Tool {
    /// From `HostExt::stylus_probe`'s first element.
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => Tool::Finger,
            2 => Tool::Stylus,
            3 => Tool::Mouse,
            4 => Tool::Eraser,
            5 => Tool::Palm,
            _ => Tool::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::Unknown => "?",
            Tool::Finger => "finger",
            Tool::Stylus => "pen",
            Tool::Mouse => "mouse",
            Tool::Eraser => "eraser",
            Tool::Palm => "palm",
        }
    }
}

/// Stylus button bits from `stylus_probe`: primary `0x20`, secondary `0x40`.
pub const STYLUS_BUTTONS: u32 = 0x60;

/// Whether a contact from `tool` should draw.
///
/// With `stylus_only` on, a finger navigates and a palm is ignored — which is what makes it possible
/// to rest a hand on the glass. `Unknown` is accepted: it is what the side channel reports before
/// the first motion event, and refusing it would swallow the opening of the first stroke.
pub fn accepts(tool: Tool, stylus_only: bool) -> bool {
    if !stylus_only {
        return !matches!(tool, Tool::Palm);
    }
    !matches!(tool, Tool::Finger | Tool::Palm)
}

/// Whether this contact is erasing: the flipped tip, or a barrel button held.
pub fn erasing(tool: Tool, buttons: u32, toggle: bool) -> bool {
    toggle || tool == Tool::Eraser || (buttons & STYLUS_BUTTONS) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The core math is tested in `ringdesign_core::paint`; this pins the
    /// re-export so a drifting signature cannot go unnoticed here.
    #[test]
    fn the_shared_math_is_reachable_and_behaves() {
        use ringdesign_core::{ProfileStyle, RingDesign};
        let mut d = RingDesign::default();
        d.profile.apply_style(ProfileStyle::HalfRound);
        let ctx = d.field_context();
        let b = bite(&ctx, ctx.crest_v_mm, 1.0, 1.0);
        assert!(b.clamped());
        assert!(wanted_mm(1.0, 1.0) == MAX_RELIEF_MM);
        assert!(MIN_RELIEF_MM > 0.0);
        let _ = ensure_band_layer;
    }

    #[test]
    fn stylus_only_rejects_fingers_and_palms_but_never_the_pen() {
        assert!(!accepts(Tool::Finger, true));
        assert!(!accepts(Tool::Palm, true));
        assert!(accepts(Tool::Stylus, true));
        assert!(accepts(Tool::Eraser, true));
        assert!(accepts(Tool::Unknown, true), "pre-first-event must not swallow the stroke");
    }

    #[test]
    fn a_palm_is_rejected_even_with_stylus_only_off() {
        assert!(!accepts(Tool::Palm, false));
        assert!(accepts(Tool::Finger, false));
    }

    #[test]
    fn the_barrel_button_and_the_flipped_tip_both_erase() {
        assert!(erasing(Tool::Stylus, 0x20, false));
        assert!(erasing(Tool::Stylus, 0x40, false));
        assert!(erasing(Tool::Eraser, 0, false));
        assert!(erasing(Tool::Stylus, 0, true), "the on-screen toggle still works");
        assert!(!erasing(Tool::Stylus, 0, false));
    }

    #[test]
    fn tool_codes_match_the_frameworks() {
        assert_eq!(Tool::from_code(2), Tool::Stylus);
        assert_eq!(Tool::from_code(4), Tool::Eraser);
        assert_eq!(Tool::from_code(5), Tool::Palm);
        assert_eq!(Tool::from_code(99), Tool::Unknown);
    }
}
