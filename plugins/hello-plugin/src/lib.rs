//! Demo plugin exercising the whole guest surface. Full egui runs inside the WASM module.

use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};

struct Hello {
    count: u32,
    note: String,
    status: String,
}

impl Hello {
    fn new(cfg: &CreateConfig) -> Self {
        Hello {
            count: 0,
            note: String::new(),
            status: format!("created on host \"{}\"", cfg.host_name),
        }
    }
}

impl PluginApp for Hello {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("hello-plugin");
            ui.label("full egui, tessellated in WASM, painted by the host");
            ui.weak(&self.status);
            ui.separator();

            if ui.button(format!("Tapped {} times", self.count)).clicked() {
                self.count += 1;
                host.haptic(0);
            }

            // Animated strip: proves repaint scheduling crosses the boundary.
            let t = ui.input(|i| i.time);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
            let hue = ((t * 0.25).fract() * 360.0) as f32;
            ui.painter().rect_filled(rect, 4.0, egui::ecolor::Hsva::new(hue / 360.0, 0.8, 0.8, 1.0));
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));

            ui.separator();
            ui.label("Note (tests the keyboard path):");
            ui.text_edit_singleline(&mut self.note);
            ui.horizontal(|ui| {
                if ui.button("Persist").clicked() {
                    match host.state_set("note", self.note.as_bytes()) {
                        Ok(()) => self.status = "note persisted".into(),
                        Err(e) => self.status = format!("state.set failed: {e}"),
                    }
                }
                if ui.button("Recall").clicked() {
                    match host.state_get("note") {
                        Ok(Some(bytes)) => {
                            self.note = String::from_utf8_lossy(&bytes).into_owned();
                            self.status = "note recalled".into();
                        }
                        Ok(None) => self.status = "no note stored yet".into(),
                        Err(e) => self.status = format!("state.get failed: {e}"),
                    }
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Notify").clicked() {
                    host.notify("hello-plugin", &self.note);
                }
                if ui.button("Emit event").clicked() {
                    host.emit("hello.ping", self.note.as_bytes());
                    self.status = "event emitted".into();
                }
                if ui.button("Log").clicked() {
                    host.log(egui_ios_plugin_sdk::Level::Info, "hello from the guest");
                }
            });
        });
    }

    fn save_state(&self) -> Vec<u8> {
        let mut bytes = self.count.to_le_bytes().to_vec();
        bytes.extend_from_slice(self.note.as_bytes());
        bytes
    }

    fn restore_state(&mut self, bytes: &[u8]) {
        if bytes.len() >= 4 {
            self.count = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            self.note = String::from_utf8_lossy(&bytes[4..]).into_owned();
            self.status = "state restored across hot reload".into();
        }
    }

    fn on_host_event(&mut self, topic: &str, payload: &[u8], _host: &HostHandle) {
        self.status = format!("host event {topic}: {}", String::from_utf8_lossy(payload));
    }
}

plugin!(Hello::new);
