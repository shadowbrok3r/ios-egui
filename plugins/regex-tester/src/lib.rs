//! Live regex tester: matches are highlighted inline in the test text via a `TextEdit`
//! layouter, with a capture-group breakdown below. Genuinely useful and missing on iOS.

use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};
use regex::{Regex, RegexBuilder};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(255, 20, 147);
const MATCH_BG: egui::Color32 = egui::Color32::from_rgb(70, 30, 60);
const OK: egui::Color32 = egui::Color32::from_rgb(166, 227, 161);
const ERR: egui::Color32 = egui::Color32::from_rgb(243, 139, 168);

struct RegexTester {
    pattern: String,
    text: String,
    case_insensitive: bool,
    multiline: bool,
    dot_all: bool,
}

impl RegexTester {
    fn new(_cfg: &CreateConfig) -> Self {
        RegexTester {
            pattern: r"(\w+)@(\w+\.\w+)".to_string(),
            text: "contact alice@example.com or bob@test.org for details".to_string(),
            case_insensitive: false,
            multiline: false,
            dot_all: false,
        }
    }

    fn build(&self) -> Result<Regex, regex::Error> {
        RegexBuilder::new(&self.pattern)
            .case_insensitive(self.case_insensitive)
            .multi_line(self.multiline)
            .dot_matches_new_line(self.dot_all)
            .build()
    }
}

impl PluginApp for RegexTester {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Regex Tester");
            ui.horizontal(|ui| {
                ui.label("Pattern:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.pattern)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .hint_text("regular expression"),
                );
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.case_insensitive, "i (ignore case)");
                ui.checkbox(&mut self.multiline, "m (multiline)");
                ui.checkbox(&mut self.dot_all, "s (. matches \\n)");
            });

            let compiled = self.build();
            let ranges: Vec<(usize, usize)> = match &compiled {
                Ok(re) => re.find_iter(&self.text).map(|m| (m.start(), m.end())).collect(),
                Err(_) => Vec::new(),
            };

            ui.separator();
            ui.label("Test text:");
            let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap: f32| {
                let job = highlight(text.as_str(), &ranges, ui.visuals().text_color());
                let mut job = job;
                job.wrap.max_width = wrap;
                ui.ctx().fonts_mut(|f| f.layout_job(job))
            };
            ui.add(
                egui::TextEdit::multiline(&mut self.text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(5)
                    .layouter(&mut layouter),
            );

            ui.separator();
            match &compiled {
                Err(e) => {
                    ui.colored_label(ERR, format!("✗ {e}"));
                }
                Ok(re) => {
                    let caps: Vec<_> = re.captures_iter(&self.text).collect();
                    ui.colored_label(
                        OK,
                        format!("✓ {} match{}", caps.len(), if caps.len() == 1 { "" } else { "es" }),
                    );
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for (i, cap) in caps.iter().enumerate() {
                            let whole = cap.get(0).map(|m| m.as_str()).unwrap_or("");
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!("{}.", i + 1));
                                ui.colored_label(ACCENT, egui::RichText::new(whole).monospace());
                            });
                            for g in 1..cap.len() {
                                if let Some(m) = cap.get(g) {
                                    let name = re
                                        .capture_names()
                                        .nth(g)
                                        .flatten()
                                        .map(|n| format!("${n}"))
                                        .unwrap_or_else(|| format!("${g}"));
                                    ui.horizontal_wrapped(|ui| {
                                        ui.add_space(16.0);
                                        ui.weak(format!("{name}:"));
                                        ui.label(egui::RichText::new(m.as_str()).monospace());
                                    });
                                }
                            }
                        }
                    });
                    if ui.button("Copy matches").clicked() {
                        let joined = re
                            .find_iter(&self.text)
                            .map(|m| m.as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        host.copy_text(&joined);
                    }
                }
            }
        });
    }
}

/// Build a `LayoutJob` that colors the byte `ranges` with the match background.
fn highlight(text: &str, ranges: &[(usize, usize)], base: egui::Color32) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let font = egui::FontId::monospace(13.0);
    let plain = TextFormat { font_id: font.clone(), color: base, ..Default::default() };
    let hit = TextFormat {
        font_id: font,
        color: egui::Color32::WHITE,
        background: MATCH_BG,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    let mut pos = 0usize;
    for &(s, e) in ranges {
        if s < pos || e > text.len() || s > e || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue; // stale ranges from a mid-edit frame; skip defensively
        }
        if s > pos {
            job.append(&text[pos..s], 0.0, plain.clone());
        }
        job.append(&text[s..e], 0.0, hit.clone());
        pos = e;
    }
    if pos < text.len() {
        job.append(&text[pos..], 0.0, plain);
    }
    job
}

plugin!(RegexTester::new);
