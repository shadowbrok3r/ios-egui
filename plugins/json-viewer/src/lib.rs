//! JSON explorer: validate, pretty-print, and browse JSON as a collapsible tree with
//! type-colored values. Parse errors report line and column.

use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};
use serde_json::Value;

const KEY: egui::Color32 = egui::Color32::from_rgb(203, 166, 247); // mauve
const STR: egui::Color32 = egui::Color32::from_rgb(166, 227, 161); // green
const NUM: egui::Color32 = egui::Color32::from_rgb(250, 179, 135); // peach
const BOOL: egui::Color32 = egui::Color32::from_rgb(137, 180, 250); // blue
const NULL: egui::Color32 = egui::Color32::from_rgb(127, 132, 156); // overlay
const ERR: egui::Color32 = egui::Color32::from_rgb(243, 139, 168);
const OK: egui::Color32 = egui::Color32::from_rgb(166, 227, 161);

const SAMPLE: &str = r#"{
  "name": "egui-ios",
  "version": 0.35,
  "plugins": ["terminal", "regex", "json"],
  "wasm": true,
  "limits": { "memory_mb": 256, "jit": null }
}"#;

struct JsonViewer {
    input: String,
    show_tree: bool,
}

impl JsonViewer {
    fn new(_cfg: &CreateConfig) -> Self {
        JsonViewer { input: SAMPLE.to_string(), show_tree: true }
    }
}

impl PluginApp for JsonViewer {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("JSON Explorer");

            let parsed: Result<Value, serde_json::Error> = serde_json::from_str(&self.input);

            ui.horizontal(|ui| {
                match &parsed {
                    Ok(_) => ui.colored_label(OK, "✓ valid JSON"),
                    Err(e) => ui.colored_label(ERR, format!("✗ line {}, col {}: {}", e.line(), e.column(), e)),
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.selectable_value(&mut self.show_tree, true, "Tree");
                    ui.selectable_value(&mut self.show_tree, false, "Pretty");
                    if let Ok(v) = &parsed {
                        if ui.button("Copy formatted").clicked() {
                            if let Ok(s) = serde_json::to_string_pretty(v) {
                                host.copy_text(&s);
                            }
                        }
                        if ui.button("Minify").clicked() {
                            if let Ok(s) = serde_json::to_string(v) {
                                self.input = s;
                            }
                        }
                    }
                });
            });

            ui.separator();
            ui.add(
                egui::TextEdit::multiline(&mut self.input)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(6)
                    .hint_text("paste JSON here"),
            );

            ui.separator();
            match &parsed {
                Err(_) => {
                    ui.weak("fix the errors above to explore the document");
                }
                Ok(value) => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        if self.show_tree {
                            show_value(ui, "root", value, true);
                        } else {
                            let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
                            ui.add(
                                egui::Label::new(egui::RichText::new(pretty).monospace())
                                    .selectable(true),
                            );
                        }
                    });
                }
            }
        });
    }
}

/// Render one JSON node. Objects and arrays are collapsible; scalars are one colored line.
fn show_value(ui: &mut egui::Ui, key: &str, value: &Value, default_open: bool) {
    match value {
        Value::Object(map) => {
            let summary = format!("{key}  {{{}}}", map.len());
            egui::CollapsingHeader::new(rich_key(&summary))
                .default_open(default_open)
                .id_salt(ui.next_auto_id())
                .show(ui, |ui| {
                    for (k, v) in map {
                        show_value(ui, k, v, false);
                    }
                });
        }
        Value::Array(items) => {
            let summary = format!("{key}  [{}]", items.len());
            egui::CollapsingHeader::new(rich_key(&summary))
                .default_open(default_open)
                .id_salt(ui.next_auto_id())
                .show(ui, |ui| {
                    for (i, v) in items.iter().enumerate() {
                        show_value(ui, &i.to_string(), v, false);
                    }
                });
        }
        scalar => {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{key}:")).color(KEY).monospace());
                let (text, color) = scalar_display(scalar);
                ui.label(egui::RichText::new(text).color(color).monospace());
            });
        }
    }
}

fn rich_key(text: &str) -> egui::RichText {
    egui::RichText::new(text).color(KEY).monospace()
}

fn scalar_display(v: &Value) -> (String, egui::Color32) {
    match v {
        Value::String(s) => (format!("\"{s}\""), STR),
        Value::Number(n) => (n.to_string(), NUM),
        Value::Bool(b) => (b.to_string(), BOOL),
        Value::Null => ("null".to_string(), NULL),
        _ => (v.to_string(), STR),
    }
}

plugin!(JsonViewer::new);
