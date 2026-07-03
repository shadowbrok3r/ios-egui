//! Demo egui iOS app exercising every host capability. The entire app is this file.

use egui_ios::egui;
use egui_ios::{CreateContext, EguiApp, Haptic, Host, Permission, app};

struct Demo {
    count: u32,
    note: String,
    camera_on: bool,
}

impl Demo {
    fn new(_cc: &CreateContext) -> Self {
        Demo {
            count: 0,
            note: String::new(),
            camera_on: false,
        }
    }
}

impl EguiApp for Demo {
    // The runtime applies the default Mastertech theme before the first frame.

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        let insets = host.safe_area_insets();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(insets.top);
            ui.heading("egui-ios demo");
            ui.separator();

            if ui.button(format!("Tapped {} times", self.count)).clicked() {
                self.count += 1;
                host.haptic(Haptic::Light);
            }

            ui.horizontal(|ui| {
                if ui.button("Notify").clicked() {
                    host.notify("Hello", "A local notification from Rust.");
                }
                if ui.button("Open URL").clicked() {
                    host.open_url("https://github.com/emilk/egui");
                }
            });

            ui.separator();
            ui.label("Document:");
            ui.text_edit_singleline(&mut self.note);
            ui.horizontal(|ui| {
                if ui.button("Pick file").clicked() {
                    host.pick_file(&["public.item"]);
                }
                if let Some(path) = host.take_picked_file() {
                    self.note = path;
                }
                if ui.button("Share note").clicked() {
                    if let Some(dir) = host.documents_dir() {
                        let path = format!("{dir}/note.txt");
                        let _ = std::fs::write(&path, &self.note);
                        host.share_file(path);
                    }
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                let toggle = if self.camera_on { "Stop camera" } else { "Start camera" };
                if ui.button(toggle).clicked() {
                    if self.camera_on {
                        host.stop_camera_preview();
                        self.camera_on = false;
                    } else {
                        host.request_permission(Permission::Camera);
                        host.start_camera_preview();
                        self.camera_on = true;
                    }
                }
                if ui.button("Mic permission").clicked() {
                    host.request_permission(Permission::Microphone);
                }
            });
            if let Some(granted) = host.permission(Permission::Camera) {
                ui.label(format!("Camera permission: {granted}"));
            }
            ui.add(egui::ProgressBar::new(host.mic_level()).text("mic level"));
        });

        ui.ctx().request_repaint();
    }
}

app!(Demo::new);
