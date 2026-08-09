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

use ringdesign_core::field::{FieldContext, SIDE_FACE_MIN_DRAFT_DEG};

/// Deepest relief allowed anywhere, mm. The measured side-face figure.
pub const MAX_RELIEF_MM: f64 = 1.6;
/// Shallowest mark worth making, mm. Below `MIN_EDGE_MM` (0.2) a feather edge will not fill, so a
/// lighter touch than this is not a fainter mark — it is no mark at all, and pretending otherwise
/// is how you get a design that looks right on screen and comes out blank.
pub const MIN_RELIEF_MM: f64 = 0.2;
/// Draft at or above this is a side face and takes the full depth.
const FREE_DRAFT_DEG: f64 = SIDE_FACE_MIN_DRAFT_DEG;
/// Below this the surface is effectively crown and takes almost nothing.
const CREST_DRAFT_DEG: f64 = 20.0;
/// What the crest can hold, mm.
const CREST_RELIEF_MM: f64 = 0.05;

/// What the surface at a given `v` will take.
///
/// Interpolates between the crest figure and the side-face figure over the draft angles between
/// them, so there is no cliff in the middle of the band for the pen to fall off.
pub fn ceiling_mm(ctx: &FieldContext, v_mm: f64) -> f64 {
    let Some(draft) = ctx.surface.draft_deg(v_mm, ctx.band_v_len_mm) else {
        return CREST_RELIEF_MM;
    };
    if draft >= FREE_DRAFT_DEG {
        MAX_RELIEF_MM
    } else if draft <= CREST_DRAFT_DEG {
        CREST_RELIEF_MM
    } else {
        let t = (draft - CREST_DRAFT_DEG) / (FREE_DRAFT_DEG - CREST_DRAFT_DEG);
        // Smoothstep, so the transition has no slope discontinuity to read as a ridge.
        let t = t * t * (3.0 - 2.0 * t);
        CREST_RELIEF_MM + (MAX_RELIEF_MM - CREST_RELIEF_MM) * t
    }
}

/// The mark a press of `pressure` wants to make at `depth_scale`, before the surface has its say.
///
/// Floored at [`MIN_RELIEF_MM`] rather than at a fraction of the maximum: the usual
/// `0.35 + 0.65 * p` curve bottoms out at a *proportion*, which against a 0.35 mm layer is 0.12 mm
/// — under the minimum edge the metal can hold.
pub fn wanted_mm(pressure: f32, depth_scale: f64) -> f64 {
    let p = pressure.clamp(0.0, 1.0) as f64;
    let top = (MAX_RELIEF_MM * depth_scale.clamp(0.05, 1.0)).max(MIN_RELIEF_MM);
    MIN_RELIEF_MM + (top - MIN_RELIEF_MM) * p
}

/// A brush sample resolved against the surface under it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bite {
    /// What will actually be cut, mm.
    pub depth_mm: f64,
    /// What the pressure asked for, mm.
    pub wanted_mm: f64,
    /// What the surface allows here, mm.
    pub ceiling_mm: f64,
}

impl Bite {
    /// The press is asking for more than the surface can hold. Worth showing: it is the moment the
    /// geometry pushes back, and the answer is usually to move to a flank or widen the side faces,
    /// not to press more gently.
    pub fn clamped(&self) -> bool {
        self.wanted_mm > self.ceiling_mm + 1e-9
    }

    /// Depth as a fraction of the global maximum, which is what the alpha stores.
    pub fn alpha_value(&self) -> f32 {
        (self.depth_mm / MAX_RELIEF_MM).clamp(0.0, 1.0) as f32
    }
}

/// Resolve a press at `v_mm` across the band.
pub fn bite(ctx: &FieldContext, v_mm: f64, pressure: f32, depth_scale: f64) -> Bite {
    let ceiling = ceiling_mm(ctx, v_mm);
    let wanted = wanted_mm(pressure, depth_scale);
    Bite { depth_mm: wanted.min(ceiling), wanted_mm: wanted, ceiling_mm: ceiling }
}

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
    use ringdesign_core::{ProfileStyle, RingDesign};

    fn ctx_for(style: ProfileStyle) -> FieldContext {
        let mut d = RingDesign::default();
        d.profile.apply_style(style);
        d.field_context()
    }

    #[test]
    fn a_half_round_crest_takes_almost_nothing() {
        let ctx = ctx_for(ProfileStyle::HalfRound);
        let at_crest = ceiling_mm(&ctx, ctx.crest_v_mm);
        assert!(
            at_crest <= 0.1,
            "the crown of a half-round undercuts at 0.30 mm; got a {at_crest} mm ceiling"
        );
    }

    #[test]
    fn a_flat_bands_side_face_takes_the_full_depth() {
        let ctx = ctx_for(ProfileStyle::Flat);
        // A little in from the bore edge, where the squared face lives.
        let v = ctx.band_v_len_mm * 0.06;
        assert_eq!(ceiling_mm(&ctx, v), MAX_RELIEF_MM);
    }

    #[test]
    fn the_ceiling_never_leaves_the_measured_range() {
        let ctx = ctx_for(ProfileStyle::DShape);
        for i in 0..=100 {
            let v = ctx.band_v_len_mm * i as f64 / 100.0;
            let c = ceiling_mm(&ctx, v);
            assert!((0.05..=MAX_RELIEF_MM).contains(&c), "v={v} gave {c}");
        }
    }

    #[test]
    fn pressure_never_asks_for_less_than_the_metal_can_hold() {
        for p in [0.0, 0.01, 0.2, 0.5, 1.0] {
            assert!(wanted_mm(p, 1.0) >= MIN_RELIEF_MM, "p={p} fell under the minimum edge");
        }
    }

    #[test]
    fn pressure_is_monotonic_and_tops_out_at_the_scale() {
        assert!(wanted_mm(0.2, 1.0) < wanted_mm(0.8, 1.0));
        assert!((wanted_mm(1.0, 1.0) - MAX_RELIEF_MM).abs() < 1e-9);
        assert!((wanted_mm(1.0, 0.5) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn a_hard_press_on_the_crest_reports_itself_clamped() {
        let ctx = ctx_for(ProfileStyle::HalfRound);
        let b = bite(&ctx, ctx.crest_v_mm, 1.0, 1.0);
        assert!(b.clamped(), "1.6 mm asked for where 0.05 is allowed");
        assert_eq!(b.depth_mm, b.ceiling_mm);
    }

    #[test]
    fn the_same_press_on_a_side_face_is_not_clamped() {
        let ctx = ctx_for(ProfileStyle::Flat);
        let b = bite(&ctx, ctx.band_v_len_mm * 0.06, 1.0, 1.0);
        assert!(!b.clamped());
        assert!((b.depth_mm - MAX_RELIEF_MM).abs() < 1e-9);
    }

    #[test]
    fn alpha_value_is_the_depth_as_a_fraction_of_the_maximum() {
        let b = Bite { depth_mm: 0.8, wanted_mm: 0.8, ceiling_mm: 1.6 };
        assert!((b.alpha_value() - 0.5).abs() < 1e-6);
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
