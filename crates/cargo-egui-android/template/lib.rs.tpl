//! {{display_name}} — a native egui Android app. This file is the whole app.

use egui_mobile::egui;
use egui_mobile::{CreateContext, EguiApp, Haptic, Host, app};

struct App {
    count: u32,
}

impl App {
    fn new(_cc: &CreateContext) -> Self {
        App { count: 0 }
    }
}

impl EguiApp for App {
    fn theme(&self, ctx: &egui::Context) {
        ctx.set_visuals(egui::Visuals::dark());
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.heading("{{display_name}}");
        if ui.button(format!("Tapped {} times", self.count)).clicked() {
            self.count += 1;
            host.haptic(Haptic::Light);
        }
        ui.ctx().request_repaint();
    }
}

app!(App::new);
