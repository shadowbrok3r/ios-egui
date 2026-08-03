//! Rules editor: the desktop's trigger -> action program UI, operating on the
//! plugin's local copy of a board's program (the LAN API has no program push,
//! so edits run from this device via the live runner and shadow the desktop's).

use egui_ios_plugin_sdk::egui::{self, ComboBox, DragValue, RichText};
use wirelab_core::circuit::{CompId, Circuit};
use wirelab_core::library::Library;
use wirelab_core::program::{Action, Program, Rule, Trigger};

/// Components that can fire events / receive actions, with their verb lists.
pub struct CompCatalog {
    pub events: Vec<(CompId, String, Vec<String>)>,
    pub actions: Vec<(CompId, String, Vec<String>)>,
}

impl CompCatalog {
    pub fn build(circuit: &Circuit, lib: &Library) -> CompCatalog {
        let mut events = Vec::new();
        let mut actions = Vec::new();
        for c in circuit.components.values() {
            let Some(def) = lib.component(&c.def_id) else { continue };
            let name = if c.label.is_empty() { def.name.clone() } else { c.label.clone() };
            if !def.events.is_empty() {
                events.push((c.id, name.clone(), def.events.iter().map(|e| e.id.clone()).collect()));
            }
            if !def.actions.is_empty() {
                actions.push((c.id, name, def.actions.iter().map(|a| a.id.clone()).collect()));
            }
        }
        CompCatalog { events, actions }
    }
}

fn default_action(action_comps: &[(CompId, String, Vec<String>)]) -> Action {
    action_comps
        .first()
        .map(|(id, _, verbs)| Action::CompAction {
            comp: *id,
            action: verbs.first().cloned().unwrap_or_else(|| "toggle".into()),
            params: Default::default(),
        })
        .unwrap_or(Action::Log { text: "no components with actions yet".into() })
}

/// The rules list with per-rule editors; `recent` holds rule indices that
/// fired in the last half second (highlighted while the program runs).
pub fn show(ui: &mut egui::Ui, program: &mut Program, cat: &CompCatalog, recent: &[usize]) {
    let mut remove: Option<usize> = None;
    let n_rules = program.rules.len();
    for idx in 0..n_rules {
        let fired = recent.contains(&idx);
        let frame = egui::Frame::group(ui.style()).fill(if fired {
            egui::Color32::from_rgb(40, 70, 45)
        } else {
            egui::Color32::from_gray(30)
        });
        frame.show(ui, |ui| {
            let rule = &mut program.rules[idx];
            ui.horizontal(|ui| {
                ui.checkbox(&mut rule.enabled, "");
                // Delete lays out from the right edge so a narrow screen
                // squeezes the name field, not the button.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✖").on_hover_text("delete rule").clicked() {
                        remove = Some(idx);
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut rule.name).desired_width(f32::INFINITY),
                    );
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("when").strong());
                trigger_editor(ui, idx, &mut rule.trigger, &cat.events);
            });
            let mut remove_action: Option<usize> = None;
            for (ai, action) in rule.actions.iter_mut().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(if ai == 0 { "do" } else { "then" }).strong());
                    action_editor(ui, idx, ai, action, &cat.actions);
                    if ui.small_button("✖").clicked() {
                        remove_action = Some(ai);
                    }
                });
            }
            if let Some(ai) = remove_action {
                rule.actions.remove(ai);
            }
            if ui.small_button("+ action").clicked() {
                rule.actions.push(default_action(&cat.actions));
            }
        });
        ui.add_space(4.0);
    }
    if let Some(idx) = remove {
        program.rules.remove(idx);
    }
    if ui.button("+ add rule").clicked() {
        let trigger = cat
            .events
            .first()
            .map(|(id, _, evs)| Trigger::CompEvent {
                comp: *id,
                event: evs.first().cloned().unwrap_or_else(|| "pressed".into()),
            })
            .unwrap_or(Trigger::OnStart);
        program.rules.push(Rule {
            name: format!("rule {}", program.rules.len() + 1),
            enabled: true,
            trigger,
            actions: vec![default_action(&cat.actions)],
        });
    }
}

fn trigger_editor(
    ui: &mut egui::Ui,
    idx: usize,
    trigger: &mut Trigger,
    event_comps: &[(CompId, String, Vec<String>)],
) {
    let kind_name = match trigger {
        Trigger::CompEvent { .. } => "component event",
        Trigger::PinRises { .. } => "pin rises",
        Trigger::PinFalls { .. } => "pin falls",
        Trigger::AnalogAbove { .. } => "analog above",
        Trigger::AnalogBelow { .. } => "analog below",
        Trigger::Every { .. } => "every",
        Trigger::OnStart => "program starts",
    };
    ComboBox::from_id_salt(("trig-kind", idx))
        .selected_text(kind_name)
        .show_ui(ui, |ui| {
            if ui.selectable_label(false, "component event").clicked() {
                let (comp, event) = event_comps
                    .first()
                    .map(|(id, _, evs)| (*id, evs[0].clone()))
                    .unwrap_or((CompId(0), "pressed".into()));
                *trigger = Trigger::CompEvent { comp, event };
            }
            if ui.selectable_label(false, "pin rises").clicked() {
                *trigger = Trigger::PinRises { gpio: 0 };
            }
            if ui.selectable_label(false, "pin falls").clicked() {
                *trigger = Trigger::PinFalls { gpio: 0 };
            }
            if ui.selectable_label(false, "analog above").clicked() {
                *trigger = Trigger::AnalogAbove { gpio: 0, millivolts: 1650 };
            }
            if ui.selectable_label(false, "analog below").clicked() {
                *trigger = Trigger::AnalogBelow { gpio: 0, millivolts: 1650 };
            }
            if ui.selectable_label(false, "every").clicked() {
                *trigger = Trigger::Every { ms: 1000 };
            }
            if ui.selectable_label(false, "program starts").clicked() {
                *trigger = Trigger::OnStart;
            }
        });
    match trigger {
        Trigger::CompEvent { comp, event } => {
            let name = event_comps
                .iter()
                .find(|(id, _, _)| id == comp)
                .map(|(_, n, _)| n.clone())
                .unwrap_or_else(|| format!("#{}", comp.0));
            ComboBox::from_id_salt(("trig-comp", idx)).selected_text(name).show_ui(ui, |ui| {
                for (id, n, _) in event_comps {
                    ui.selectable_value(comp, *id, n);
                }
            });
            let events = event_comps
                .iter()
                .find(|(id, _, _)| id == comp)
                .map(|(_, _, e)| e.clone())
                .unwrap_or_default();
            ComboBox::from_id_salt(("trig-ev", idx)).selected_text(event.clone()).show_ui(
                ui,
                |ui| {
                    for e in &events {
                        ui.selectable_value(event, e.clone(), e);
                    }
                },
            );
        }
        Trigger::PinRises { gpio } | Trigger::PinFalls { gpio } => {
            ui.label("GPIO");
            ui.add(DragValue::new(gpio).range(0..=48));
        }
        Trigger::AnalogAbove { gpio, millivolts } | Trigger::AnalogBelow { gpio, millivolts } => {
            ui.label("GPIO");
            ui.add(DragValue::new(gpio).range(0..=48));
            ui.label("mV");
            ui.add(DragValue::new(millivolts).range(0..=3300));
        }
        Trigger::Every { ms } => {
            ui.add(DragValue::new(ms).range(20..=600_000).suffix(" ms"));
        }
        Trigger::OnStart => {}
    }
}

fn action_editor(
    ui: &mut egui::Ui,
    rule_idx: usize,
    action_idx: usize,
    action: &mut Action,
    action_comps: &[(CompId, String, Vec<String>)],
) {
    let salt = (rule_idx, action_idx);
    let kind_name = match action {
        Action::CompAction { .. } => "component",
        Action::SetPin { .. } => "set pin",
        Action::TogglePin { .. } => "toggle pin",
        Action::SetPwm { .. } => "set pwm",
        Action::Wait { .. } => "wait",
        Action::Log { .. } => "log",
        Action::SetPinMode { .. } => "pin mode",
        Action::SetRgb { .. } => "rgb led",
        Action::WatchAnalog { .. } => "watch analog",
        Action::UartConfig { .. } => "uart config",
        Action::UartWrite { .. } => "uart send",
        Action::SpiConfig { .. } => "spi config",
        Action::SpiTransfer { .. } => "spi transfer",
        Action::I2cConfig { .. } => "i2c config",
        Action::I2cWrite { .. } => "i2c write",
        Action::I2cRead { .. } => "i2c read",
        Action::LcdInit { .. } => "lcd init",
        Action::LcdClear { .. } => "lcd clear",
        Action::LcdRect { .. } => "lcd rect",
        Action::LcdText { .. } => "lcd text",
        Action::BoardMsg { .. } => "board msg",
        Action::HttpGet { .. } => "http get",
    };
    ComboBox::from_id_salt(("act-kind", salt))
        .selected_text(kind_name)
        .show_ui(ui, |ui| {
            if ui.selectable_label(false, "component").clicked() {
                *action = default_action(action_comps);
            }
            if ui.selectable_label(false, "set pin").clicked() {
                *action = Action::SetPin { gpio: 2, high: true };
            }
            if ui.selectable_label(false, "toggle pin").clicked() {
                *action = Action::TogglePin { gpio: 2 };
            }
            if ui.selectable_label(false, "set pwm").clicked() {
                *action = Action::SetPwm { gpio: 2, freq_hz: 1000, duty_permille: 500 };
            }
            if ui.selectable_label(false, "wait").clicked() {
                *action = Action::Wait { ms: 500 };
            }
            if ui.selectable_label(false, "log").clicked() {
                *action = Action::Log { text: "hello".into() };
            }
            if ui.selectable_label(false, "pin mode").clicked() {
                *action =
                    Action::SetPinMode { gpio: 28, mode: wirelab_proto::PinMode::InputPullUp };
            }
            if ui.selectable_label(false, "rgb led").clicked() {
                *action = Action::SetRgb { gpio: 27, r: 64, g: 0, b: 64 };
            }
        });
    match action {
        Action::CompAction { comp, action: verb, params } => {
            let name = action_comps
                .iter()
                .find(|(id, _, _)| id == comp)
                .map(|(_, n, _)| n.clone())
                .unwrap_or_else(|| format!("#{}", comp.0));
            ComboBox::from_id_salt(("act-comp", salt)).selected_text(name).show_ui(ui, |ui| {
                for (id, n, _) in action_comps {
                    ui.selectable_value(comp, *id, n);
                }
            });
            let verbs = action_comps
                .iter()
                .find(|(id, _, _)| id == comp)
                .map(|(_, _, v)| v.clone())
                .unwrap_or_default();
            ComboBox::from_id_salt(("act-verb", salt))
                .selected_text(verb.clone())
                .show_ui(ui, |ui| {
                    for v in &verbs {
                        ui.selectable_value(verb, v.clone(), v);
                    }
                });
            // Common tunables per verb.
            match verb.as_str() {
                "blink" | "breathe" => {
                    let mut v = params.get("period_ms").copied().unwrap_or(500.0);
                    ui.label("period");
                    if ui
                        .add(DragValue::new(&mut v).range(40.0..=10000.0).suffix(" ms"))
                        .changed()
                    {
                        params.insert("period_ms".into(), v);
                    }
                }
                "dim" => {
                    let mut v = params.get("percent").copied().unwrap_or(50.0);
                    if ui.add(DragValue::new(&mut v).range(0.0..=100.0).suffix(" %")).changed() {
                        params.insert("percent".into(), v);
                    }
                }
                "set_angle" => {
                    let mut v = params.get("degrees").copied().unwrap_or(90.0);
                    if ui.add(DragValue::new(&mut v).range(0.0..=180.0).suffix(" °")).changed() {
                        params.insert("degrees".into(), v);
                    }
                }
                "beep" => {
                    let mut v = params.get("ms").copied().unwrap_or(200.0);
                    if ui.add(DragValue::new(&mut v).range(10.0..=5000.0).suffix(" ms")).changed()
                    {
                        params.insert("ms".into(), v);
                    }
                }
                "tone" => {
                    let mut f = params.get("freq_hz").copied().unwrap_or(880.0);
                    let mut d = params.get("ms").copied().unwrap_or(300.0);
                    if ui
                        .add(DragValue::new(&mut f).range(20.0..=20000.0).suffix(" Hz"))
                        .changed()
                    {
                        params.insert("freq_hz".into(), f);
                    }
                    if ui
                        .add(DragValue::new(&mut d).range(10.0..=10000.0).suffix(" ms"))
                        .changed()
                    {
                        params.insert("ms".into(), d);
                    }
                }
                _ => {}
            }
        }
        Action::SetPin { gpio, high } => {
            ui.label("GPIO");
            ui.add(DragValue::new(gpio).range(0..=48));
            ui.checkbox(high, "high");
        }
        Action::TogglePin { gpio } => {
            ui.label("GPIO");
            ui.add(DragValue::new(gpio).range(0..=48));
        }
        Action::SetPwm { gpio, freq_hz, duty_permille } => {
            ui.label("GPIO");
            ui.add(DragValue::new(gpio).range(0..=48));
            ui.add(DragValue::new(freq_hz).range(1..=40000).suffix(" Hz"));
            ui.add(DragValue::new(duty_permille).range(0..=1000).suffix(" /1000"));
        }
        Action::Wait { ms } => {
            ui.add(DragValue::new(ms).range(1..=600_000).suffix(" ms"));
        }
        Action::Log { text } => {
            ui.text_edit_singleline(text);
        }
        Action::SetPinMode { gpio, mode } => {
            ui.label("GPIO");
            ui.add(DragValue::new(gpio).range(0..=48));
            use wirelab_proto::PinMode;
            ComboBox::from_id_salt(("act-mode", salt))
                .selected_text(format!("{mode:?}"))
                .show_ui(ui, |ui| {
                    for m in [
                        PinMode::Input,
                        PinMode::InputPullUp,
                        PinMode::InputPullDown,
                        PinMode::Output,
                        PinMode::Pwm,
                        PinMode::Analog,
                    ] {
                        ui.selectable_value(mode, m, format!("{m:?}"));
                    }
                });
        }
        Action::WatchAnalog { gpio, interval_ms } => {
            ui.label("GPIO");
            ui.add(DragValue::new(gpio).range(0..=48));
            ui.add(DragValue::new(interval_ms).range(0..=60000).suffix(" ms"));
        }
        Action::UartConfig { tx, rx, baud } => {
            ui.label("TX");
            ui.add(DragValue::new(tx).range(0..=48));
            ui.label("RX");
            ui.add(DragValue::new(rx).range(0..=48));
            ui.add(DragValue::new(baud).range(300..=921600).suffix(" baud"));
        }
        Action::UartWrite { data } => {
            let mut text = String::from_utf8_lossy(data).to_string();
            if ui.text_edit_singleline(&mut text).changed() {
                *data = text.into_bytes();
            }
        }
        Action::SpiConfig { sck, mosi, miso, freq_khz } => {
            for (label, v) in [("SCK", sck), ("MOSI", mosi), ("MISO", miso)] {
                ui.label(label);
                ui.add(DragValue::new(v).range(0..=48));
            }
            ui.add(DragValue::new(freq_khz).range(1..=40000).suffix(" kHz"));
        }
        Action::SpiTransfer { cs, data } => {
            ui.label("CS");
            ui.add(DragValue::new(cs).range(0..=48));
            ui.label(format!("{} bytes", data.len()));
        }
        Action::I2cConfig { sda, scl, freq_khz } => {
            for (label, v) in [("SDA", sda), ("SCL", scl)] {
                ui.label(label);
                ui.add(DragValue::new(v).range(0..=48));
            }
            ui.add(DragValue::new(freq_khz).range(1..=1000).suffix(" kHz"));
        }
        Action::I2cWrite { addr, data } => {
            ui.label("addr");
            ui.add(DragValue::new(addr).range(0..=127));
            ui.label(format!("{} bytes", data.len()));
        }
        Action::I2cRead { addr, reg, len } => {
            ui.label("addr");
            ui.add(DragValue::new(addr).range(0..=127));
            ui.label("reg");
            ui.add(DragValue::new(reg).range(0..=256));
            ui.label("len");
            ui.add(DragValue::new(len).range(1..=48));
        }
        Action::BoardMsg { to, text } => {
            ui.label("board");
            ui.add(egui::TextEdit::singleline(to).desired_width(80.0));
            ui.label("text");
            ui.add(egui::TextEdit::singleline(text).desired_width(120.0));
        }
        Action::HttpGet { url } => {
            ui.label("url");
            ui.add(egui::TextEdit::singleline(url).desired_width(200.0));
        }
        Action::LcdInit { sck, mosi, cs, dc, rst, .. } => {
            for (label, v) in [("SCK", sck), ("MOSI", mosi), ("CS", cs), ("DC", dc), ("RST", rst)]
            {
                ui.label(label);
                ui.add(DragValue::new(v).range(0..=48));
            }
        }
        Action::LcdClear { rgb } | Action::LcdRect { rgb, .. } | Action::LcdText { rgb, .. } => {
            let mut color = [
                f32::from(rgb[0]) / 255.0,
                f32::from(rgb[1]) / 255.0,
                f32::from(rgb[2]) / 255.0,
            ];
            if ui.color_edit_button_rgb(&mut color).changed() {
                *rgb = [
                    (color[0] * 255.0) as u8,
                    (color[1] * 255.0) as u8,
                    (color[2] * 255.0) as u8,
                ];
            }
        }
        Action::SetRgb { gpio, r, g, b } => {
            ui.label("GPIO");
            ui.add(DragValue::new(gpio).range(0..=48));
            let mut color = [f32::from(*r) / 255.0, f32::from(*g) / 255.0, f32::from(*b) / 255.0];
            if ui.color_edit_button_rgb(&mut color).changed() {
                *r = (color[0] * 255.0) as u8;
                *g = (color[1] * 255.0) as u8;
                *b = (color[2] * 255.0) as u8;
            }
        }
    }
}
