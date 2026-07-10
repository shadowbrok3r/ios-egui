//! Android demo — the same `impl EguiApp` shape as the iOS examples, built as an Android cdylib.
//! Exercises the common Host capabilities plus the Android-only `HostExt` (self-update, install /
//! overlay permission).

use egui_mobile::egui;
use egui_mobile::{CreateContext, EguiApp, Haptic, Host, HostExt, Permission, app};

struct Demo {
    count: u32,
    slider: f32,
    text: String,
}

impl Demo {
    fn new(_cc: &CreateContext) -> Self {
        Demo {
            count: 0,
            slider: 0.5,
            text: "edit me".to_owned(),
        }
    }
}

impl EguiApp for Demo {
    fn theme(&self, ctx: &egui::Context) {
        ctx.set_visuals(egui::Visuals::dark());
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.heading("egui on Android");
        ui.separator();

        if ui.button(format!("Tapped {} times", self.count)).clicked() {
            self.count += 1;
            host.haptic(Haptic::Light);
        }
        ui.add(egui::Slider::new(&mut self.slider, 0.0..=1.0).text("slider"));
        ui.text_edit_singleline(&mut self.text);

        ui.separator();
        ui.label("System:");
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open URL").clicked() {
                host.open_url("https://github.com/emilk/egui");
            }
            if ui.button("Copy").clicked() {
                host.copy_text(&self.text);
            }
            if ui.button("Share").clicked() {
                host.share_text(&self.text);
            }
            if ui.button("Notify").clicked() {
                host.notify("Android Hello", "From Rust via JNI.");
            }
        });

        ui.separator();
        ui.label("Permissions:");
        ui.horizontal_wrapped(|ui| {
            if ui.button("Camera").clicked() {
                host.request_permission(Permission::Camera);
            }
            if ui.button("Notifications").clicked() {
                host.request_notification_permission();
            }
        });
        if let Some(granted) = host.permission(Permission::Camera) {
            ui.label(format!("camera: {granted}"));
        }

        ui.separator();
        ui.label(format!("Android-only (versionCode {})", host.current_version_code()));
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("can install: {}", host.can_install_packages()));
            if ui.button("Grant install").clicked() {
                host.request_install_permission();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("can overlay: {}", host.can_draw_overlays()));
            if ui.button("Grant overlay").clicked() {
                host.request_overlay_permission();
            }
        });

        ui.ctx().request_repaint();
    }
}

app!(Demo::new);
