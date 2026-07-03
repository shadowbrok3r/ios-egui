//! The default egui visual theme shared by the iOS runtime, plugin guests, and desktop hosts.
//! The scheme is stored as a serialized [`egui::Style`]; it is merged onto the running egui
//! version's default so a field added in a newer egui patch fills from the default instead of
//! failing to parse.

use serde_json::Value;

const STYLE_JSON: &str = include_str!("mastertech_style.json");

/// The Mastertech dark theme (near-black surfaces, pink/purple accents) as an [`egui::Style`].
/// Falls back to `Style::default()` if the embedded JSON is somehow incompatible.
pub fn mastertech_style() -> egui::Style {
    build_style(STYLE_JSON).unwrap_or_default()
}

/// Apply [`mastertech_style`] to a context for both light and dark themes, and force dark
/// (egui 0.35 keeps a style per theme; this makes the scheme active regardless of preference).
pub fn apply(ctx: &egui::Context) {
    let style = std::sync::Arc::new(mastertech_style());
    ctx.all_styles_mut(|s| *s = (*style).clone());
    ctx.set_theme(egui::Theme::Dark);
}

fn build_style(json: &str) -> Option<egui::Style> {
    let mut base = serde_json::to_value(egui::Style::default()).ok()?;
    let over: Value = serde_json::from_str(json).ok()?;
    merge(&mut base, over);
    serde_json::from_value(base).ok()
}

/// Recursive object merge: `over` wins on scalars/arrays; keys only in `base` are kept.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                merge(b.entry(k).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_style_parses() {
        let style = super::mastertech_style();
        // A field unique to the provided scheme; if we silently fell back to default this fails.
        assert_eq!(style.visuals.panel_fill, egui::Color32::from_rgb(0, 0, 0));
        assert_eq!(style.spacing.item_spacing, egui::vec2(3.0, 3.0));
        assert!(style.visuals.dark_mode);
    }
}
