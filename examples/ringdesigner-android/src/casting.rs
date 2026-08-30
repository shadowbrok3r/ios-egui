//! Which process the verdict judges against, and the floors that come with it.
//!
//! `mod app` is Android-only, so anything load-bearing that lives in a panel
//! cannot be tested on the host. This is the load-bearing part of the Design
//! sheet's process row, kept pure so it can be.
//!
//! The core's `CastProcess::apply` writes the fill and detail floors on the
//! lost-wax arm and nothing on the sand arm, so switching to lost wax and back
//! leaves a sand pour judged at investment numbers — 0.5 mm fill against the
//! 0.8 mm Delft clay actually holds. [`set_process`] names the sand explicitly
//! rather than inheriting that. When the core grows its else arm the two agree
//! and this stays correct.

use ringdesign_core::castability::{CastProcess, DraftSettings, SandProcess};

/// The sand a design falls back to when the process returns to two-part.
///
/// `DraftSettings` has nowhere to remember which sand was last chosen, so the
/// finer of the two is the safe default: Delft clay wants more section and more
/// draft than Petrobond, and a floor that is too generous refuses a ring the
/// sand would have held, where one that is too lax passes one it will not fill.
pub const DEFAULT_SAND: SandProcess = SandProcess::DelftClay;

/// Switch `d` to `process`, leaving every floor consistent with it.
pub fn set_process(d: &mut DraftSettings, process: CastProcess) {
    process.apply(d);
    if process == CastProcess::SandTwoPart {
        DEFAULT_SAND.apply(d);
    }
}

/// How to name the process on a verdict chip.
pub fn short_label(process: CastProcess) -> &'static str {
    match process {
        CastProcess::SandTwoPart => "sand",
        CastProcess::LostWax => "lost wax",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_wax_lowers_the_floors_to_investment() {
        let mut d = DraftSettings::default();
        set_process(&mut d, CastProcess::LostWax);
        assert_eq!(d.process, CastProcess::LostWax);
        assert!(d.min_section_mm <= 0.5, "fill floor {}", d.min_section_mm);
        assert!(d.min_detail_mm <= 0.15, "detail floor {}", d.min_detail_mm);
    }

    /// The bug this module exists for: the core's `apply` has no sand arm, so a
    /// round trip through lost wax would otherwise leave a sand ring judged at
    /// 0.5 / 0.15 and reading Castable on walls Delft clay will not fill.
    #[test]
    fn returning_to_sand_restores_a_sand_floor() {
        let mut d = DraftSettings::default();
        set_process(&mut d, CastProcess::LostWax);
        set_process(&mut d, CastProcess::SandTwoPart);
        assert_eq!(d.process, CastProcess::SandTwoPart);
        assert!(d.min_section_mm >= 0.6, "fill floor came back as {}", d.min_section_mm);
        assert!(d.min_detail_mm >= 0.25, "detail floor came back as {}", d.min_detail_mm);
        assert!(d.min_draft_deg >= 2.5, "draft came back as {}", d.min_draft_deg);
    }

    /// `CastProcess::apply` never touches the draft angle on either arm, so the
    /// sand preset is the only thing that restores it.
    #[test]
    fn the_draft_angle_is_restored_too_not_just_the_two_floors() {
        let mut d = DraftSettings::default();
        d.min_draft_deg = 0.0;
        set_process(&mut d, CastProcess::SandTwoPart);
        assert!(d.min_draft_deg > 0.0, "a zero draft floor survived the switch");
    }

    #[test]
    fn a_round_trip_is_idempotent() {
        let mut once = DraftSettings::default();
        set_process(&mut once, CastProcess::SandTwoPart);
        let mut twice = DraftSettings::default();
        for _ in 0..3 {
            set_process(&mut twice, CastProcess::LostWax);
            set_process(&mut twice, CastProcess::SandTwoPart);
        }
        assert_eq!(once.min_section_mm, twice.min_section_mm);
        assert_eq!(once.min_detail_mm, twice.min_detail_mm);
        assert_eq!(once.min_draft_deg, twice.min_draft_deg);
    }

    #[test]
    fn both_processes_have_a_short_name() {
        for &p in CastProcess::ALL {
            assert!(!short_label(p).is_empty(), "{p:?}");
        }
    }
}
