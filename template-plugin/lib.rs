//! {{display_name}} — an egui-ios plugin. The whole plugin is this file.

use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};

struct App {
    count: u32,
}

impl App {
    fn new(_cfg: &CreateConfig) -> Self {
        App { count: 0 }
    }
}

impl PluginApp for App {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        ui.heading("{{display_name}}");
        if ui.button(format!("Tapped {} times", self.count)).clicked() {
            self.count += 1;
            host.haptic(0);
        }
    }

    fn save_state(&self) -> Vec<u8> {
        self.count.to_le_bytes().to_vec()
    }

    fn restore_state(&mut self, bytes: &[u8]) {
        if let Ok(b) = bytes.try_into() {
            self.count = u32::from_le_bytes(b);
        }
    }
}

plugin!(App::new);
