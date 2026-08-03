//! Icon glyphs for UI text.
//!
//! egui renders only from the fonts loaded into its `Context`, and this app adds none — so the
//! usable set is whatever the default `FontFamily::Proportional` chain covers: Ubuntu-Light ->
//! NotoEmoji-Regular -> emoji-icon-font. Geometric shapes and arrows (`▸` `▾` `→`) live in Hack,
//! which is Monospace-only, so they are tofu boxes in buttons and labels.
//! `every_icon_has_a_glyph` asserts each constant below against the real font chain.

pub const SETTINGS: &str = "⚙";
pub const REFRESH: &str = "🔄";
pub const CHANGELOG: &str = "📜";
pub const INSTALL: &str = "⬇";
pub const DOT: &str = "•";
pub const WARN: &str = "⚠";

/// Every icon constant, for the font-coverage test.
#[cfg(test)]
const ALL: &[(&str, &str)] = &[
    ("SETTINGS", SETTINGS),
    ("REFRESH", REFRESH),
    ("CHANGELOG", CHANGELOG),
    ("INSTALL", INSTALL),
    ("DOT", DOT),
    ("WARN", WARN),
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
    /// negative for every glyph that lives in the replacement face itself.
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

    /// The carets this screen used to draw with, kept as a standing counter-example: Hack has
    /// them, but Hack is Monospace-only, so they render as tofu in buttons and labels.
    #[test]
    fn caret_glyphs_are_absent_from_proportional() {
        for glyph in ["▸", "▾", "▲", "▼"] {
            assert!(
                !chain_covers(&FontFamily::Proportional, glyph),
                "{glyph} unexpectedly gained a Proportional glyph — the icon rule may be stale"
            );
        }
    }
}
