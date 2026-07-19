//! Icon glyphs for UI text.
//!
//! egui renders only from the fonts loaded into its `Context`, and this app adds none — so the
//! usable set is whatever the default `FontFamily::Proportional` chain covers: Ubuntu-Light ->
//! NotoEmoji-Regular -> emoji-icon-font. Hack is Monospace-only, so glyphs it alone carries
//! (`●` `▲` `▼` `→` `▦`) are tofu boxes in buttons and labels. `every_icon_has_a_glyph` asserts
//! each constant below against the real font chain.

pub const GENERATE: &str = "✨";
pub const GRAPH: &str = "🔗";
pub const PROPS: &str = "📋";
pub const GALLERY: &str = "🖼";
pub const LOGS: &str = "📜";
pub const SETTINGS: &str = "⚙";

pub const REFRESH: &str = "🔄";
pub const SEARCH: &str = "🔍";
pub const SAVE: &str = "💾";
pub const FOLDER: &str = "📁";
pub const TRASH: &str = "🗑";
pub const ADD: &str = "➕";
pub const RUN: &str = "▶";
pub const STOP: &str = "⏹";
pub const BACK: &str = "◀";
pub const LOCKED: &str = "🔒";
pub const UNLOCKED: &str = "🔓";
pub const ALBUM: &str = "📚";
pub const MODEL: &str = "🎨";
pub const SORT: &str = "↕";
pub const USER: &str = "👤";
pub const KEY: &str = "🔑";
pub const LINK: &str = "🔗";
pub const IMAGE: &str = "🖼";
pub const WARN: &str = "⚠";
pub const UNDO: &str = "↩";
pub const REDO: &str = "↪";
pub const DOT: &str = "•";
pub const CHECK: &str = "✔";
pub const CLOSE: &str = "✖";
pub const MENU: &str = "☰";
pub const STAR: &str = "⭐";

/// Every icon constant, for the font-coverage test.
#[cfg(test)]
const ALL: &[(&str, &str)] = &[
    ("GENERATE", GENERATE),
    ("GRAPH", GRAPH),
    ("PROPS", PROPS),
    ("GALLERY", GALLERY),
    ("LOGS", LOGS),
    ("SETTINGS", SETTINGS),
    ("REFRESH", REFRESH),
    ("SEARCH", SEARCH),
    ("SAVE", SAVE),
    ("FOLDER", FOLDER),
    ("TRASH", TRASH),
    ("ADD", ADD),
    ("RUN", RUN),
    ("STOP", STOP),
    ("BACK", BACK),
    ("LOCKED", LOCKED),
    ("UNLOCKED", UNLOCKED),
    ("ALBUM", ALBUM),
    ("MODEL", MODEL),
    ("SORT", SORT),
    ("USER", USER),
    ("KEY", KEY),
    ("LINK", LINK),
    ("IMAGE", IMAGE),
    ("WARN", WARN),
    ("UNDO", UNDO),
    ("REDO", REDO),
    ("DOT", DOT),
    ("CHECK", CHECK),
    ("CLOSE", CLOSE),
    ("MENU", MENU),
    ("STAR", STAR),
];

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{FontDefinitions, FontFamily};
    use skrifa::MetadataProvider as _;

    /// Does any font in `family`'s fallback chain have a real glyph for every char of `s`?
    ///
    /// This reads the cmaps of the exact font bytes egui will load rather than asking
    /// `Fonts::has_glyph`, which answers "is this char's face the replacement face?" — a false
    /// negative for every glyph that lives in the replacement face itself (NotoEmoji for
    /// Proportional, Hack for Monospace), i.e. precisely the emoji this module is made of.
    fn chain_covers(family: &FontFamily, s: &str) -> bool {
        let defs = FontDefinitions::default();
        let chain = &defs.families[family];
        s.chars().all(|c| {
            chain.iter().any(|name| {
                let data = &defs.font_data[name];
                skrifa::FontRef::from_index(&data.font, data.index)
                    .map(|font| font.charmap().map(c).is_some())
                    .unwrap_or(false)
            })
        })
    }

    /// Guards against picking a glyph the default fonts don't carry — it would ship as a tofu box
    /// on the phone with nothing failing at compile time.
    #[test]
    fn every_icon_has_a_glyph() {
        let missing: Vec<&str> = ALL
            .iter()
            .filter(|(_, glyph)| !chain_covers(&FontFamily::Proportional, glyph))
            .map(|(name, _)| *name)
            .collect();
        assert!(missing.is_empty(), "no glyph in the Proportional chain for: {missing:?}");
    }

    /// The counter-examples this module is calibrated against: Hack carries these, but Hack is in
    /// the Monospace chain only, so they are tofu in buttons and labels.
    #[test]
    fn monospace_only_glyphs_are_still_absent_from_proportional() {
        for glyph in ["●", "▲", "▼", "→"] {
            assert!(chain_covers(&FontFamily::Monospace, glyph), "{glyph} should exist in Hack");
            assert!(
                !chain_covers(&FontFamily::Proportional, glyph),
                "{glyph} unexpectedly gained a Proportional glyph — the icon rule may be stale"
            );
        }
    }

    /// The oracle must be able to fail: a private-use codepoint is in none of the bundled fonts.
    #[test]
    fn coverage_check_rejects_an_absent_glyph() {
        assert!(!chain_covers(&FontFamily::Proportional, "\u{E000}"));
        assert!(chain_covers(&FontFamily::Proportional, "A"));
    }
}
