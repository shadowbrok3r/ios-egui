//! Proxy status, the interception mode switch, and counters.

use crate::app::PrivaxyApp;
use crate::proxy::config::MitmMode;
use crate::proxy::state::{FiltersStatus, Status};
use crate::ui;
use crate::vpn::VpnStatus;
use egui_mobile::{Haptic, Host, egui};

pub fn show(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };

    let status = loaded.state.status();

    ui::card(ui, |ui| {
        match &status {
            Status::Running { address } => {
                ui.label(
                    egui::RichText::new("Proxy running")
                        .size(17.0)
                        .strong()
                        .color(ui::GOOD),
                );
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(address)
                            .monospace()
                            .size(15.0)
                            .color(ui::ACCENT),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Copy").clicked() {
                            host.copy_text(address.clone());
                            host.haptic(Haptic::Light);
                        }
                    });
                });
            }
            Status::Starting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Starting…").size(16.0));
                });
            }
            Status::Failed(error) => {
                ui.label(
                    egui::RichText::new("Proxy failed")
                        .size(17.0)
                        .strong()
                        .color(ui::BAD),
                );
                ui.label(egui::RichText::new(error).size(12.0).color(ui::MUTED));
            }
            Status::Stopped => {
                ui.label(
                    egui::RichText::new("Proxy stopped")
                        .size(17.0)
                        .strong()
                        .color(ui::MUTED),
                );
            }
        }

        ui.add_space(10.0);

        if status.is_running() || matches!(status, Status::Starting) {
            if ui::big_button(ui, "Stop", ui::DANGER_FILL).clicked() {
                loaded.stop_proxy();
                host.haptic(Haptic::Medium);
            }
        } else if ui::big_button(ui, "Start", ui::ACCENT_FILL).clicked() {
            loaded.start_proxy();
            host.haptic(Haptic::Medium);
        }
    });

    ui.add_space(10.0);

    capture_card(loaded, ui, host);

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Interception");
        ui.add_space(6.0);

        let mut mode = loaded.config.mode;
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 8.0) / 2.0;
            for option in [MitmMode::HostnameOnly, MitmMode::Full] {
                let selected = mode == option;
                if ui
                    .add_sized(
                        [width, ui::TOUCH_HEIGHT],
                        egui::Button::selectable(selected, option.label()),
                    )
                    .clicked()
                {
                    mode = option;
                }
            }
        });

        if mode != loaded.config.mode {
            loaded.config.mode = mode;
            loaded.state.set_mode(mode);
            if let Some(proxy) = loaded.proxy.as_ref() {
                proxy.set_mode(mode);
            }
            let _ = loaded.save();
            host.haptic(Haptic::Selection);
        }

        ui.add_space(6.0);
        match loaded.config.mode {
            MitmMode::HostnameOnly => {
                ui.label(
                    egui::RichText::new(
                        "Blocks whole hosts as connections open, and passes everything else \
                         through untouched. Needs no certificate and works for every app on \
                         the device.",
                    )
                    .size(12.0)
                    .color(ui::MUTED),
                );
            }
            MitmMode::Full => {
                ui.label(
                    egui::RichText::new(
                        "Decrypts traffic so individual URLs and page content can be filtered.",
                    )
                    .size(12.0)
                    .color(ui::MUTED),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Needs the Privaxy certificate installed. Even then, Android 7 and later \
                         only lets apps that opt in trust user certificates — in practice that \
                         means browsers. Other apps will fail to connect on hosts this mode \
                         intercepts.",
                    )
                    .size(12.0)
                    .color(ui::WARN),
                );
            }
        }

        ui.add_space(8.0);
        let mut blocking = loaded.config.blocking_enabled;
        if ui.checkbox(&mut blocking, "Blocking enabled").changed() {
            loaded.config.blocking_enabled = blocking;
            if let Some(proxy) = loaded.proxy.as_ref() {
                proxy.set_blocking_enabled(blocking);
            }
            let _ = loaded.save();
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Since launch");
        ui.add_space(8.0);

        let counters = loaded.state.counters.snapshot();
        ui.columns(2, |columns| {
            ui::stat(&mut columns[0], "Blocked", counters.blocked, ui::BAD);
            ui::stat(&mut columns[1], "Tunneled", counters.tunneled, ui::MUTED);
        });
        ui.add_space(10.0);
        ui.columns(2, |columns| {
            ui::stat(&mut columns[0], "Proxied", counters.proxied, ui::GOOD);
            ui::stat(&mut columns[1], "Rewritten", counters.modified, ui::ACCENT);
        });
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Filters");
        ui.add_space(6.0);
        match loaded.state.filters_status() {
            FiltersStatus::Idle => {
                ui.label(egui::RichText::new("Not loaded").color(ui::MUTED).size(13.0));
            }
            FiltersStatus::Updating { completed, total } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new(format!("Loading {completed} of {total}…"))
                            .size(13.0)
                            .color(ui::MUTED),
                    );
                });
            }
            FiltersStatus::Ready { lists } => {
                ui.label(
                    egui::RichText::new(format!("{lists} lists active"))
                        .size(13.0)
                        .color(ui::GOOD),
                );
            }
            FiltersStatus::Failed(error) => {
                ui.label(egui::RichText::new(error).size(12.0).color(ui::WARN));
            }
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Pointing traffic here by hand");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Without capture, Android has no system-wide proxy setting, so this is per \
                 network: Settings > Network & internet > Internet, tap the gear on your Wi-Fi, \
                 Edit > Advanced > Proxy > Manual.",
            )
            .size(12.0)
            .color(ui::MUTED),
        );
        ui.add_space(6.0);
        if let Status::Running { address } = &status {
            let (host_part, port_part) = address.split_once(':').unwrap_or((address, "8100"));
            ui::detail_row(
                ui,
                "Hostname",
                egui::RichText::new(host_part).monospace().color(ui::ACCENT),
            );
            ui::detail_row(
                ui,
                "Port",
                egui::RichText::new(port_part).monospace().color(ui::ACCENT),
            );
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "A Wi-Fi proxy does not cover mobile data, and apps are free to ignore it.",
            )
            .size(12.0)
            .color(ui::WARN),
        );
    });

    ui.add_space(16.0);
}

/// The `VpnService` toggle: consent, capture status, and what the relay has been doing.
fn capture_card(loaded: &mut crate::app::Loaded, ui: &mut egui::Ui, host: &Host) {
    ui::card(ui, |ui| {
        ui::section_title(ui, "Capture");
        ui.add_space(6.0);

        if !loaded.vpn.is_supported() {
            ui.label(
                egui::RichText::new("Transparent capture needs Android's VpnService.")
                    .size(12.0)
                    .color(ui::MUTED),
            );
            return;
        }

        let status = loaded.vpn.status().clone();
        let (label, color) = match &status {
            VpnStatus::Off => ("• Not capturing", ui::MUTED),
            VpnStatus::Requesting => ("• Waiting for permission", ui::WARN),
            VpnStatus::Starting => ("• Starting", ui::WARN),
            VpnStatus::Running => ("• Capturing all traffic", ui::GOOD),
            VpnStatus::Failed(_) => ("• Capture failed", ui::BAD),
        };
        ui.label(egui::RichText::new(label).size(15.0).strong().color(color));

        if let VpnStatus::Failed(error) = &status {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(error).size(12.0).color(ui::MUTED));
        }

        ui.add_space(10.0);

        if status.is_on() {
            if ui::big_button(ui, "Stop capturing", ui::DANGER_FILL).clicked() {
                loaded.stop_capture();
                loaded.config.capture_all = false;
                let _ = loaded.save();
                host.haptic(Haptic::Medium);
            }
        } else if ui::big_button(ui, "Capture all traffic", ui::ACCENT_FILL).clicked() {
            // Notifications are asked for once capture is up, not here: a runtime permission
            // dialog raised in the same frame lands on top of the VPN consent dialog and finishes
            // it, which comes back as a refusal.
            loaded.start_capture();
            loaded.config.capture_all = true;
            let _ = loaded.save();
            host.haptic(Haptic::Medium);
        }

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Routes every app through the proxy over a local VPN, including mobile data and \
                 apps that ignore the Wi-Fi proxy setting. Android shows a consent dialog the \
                 first time, then a key icon while it runs. Nothing leaves the phone that would \
                 not have left it anyway.",
            )
            .size(12.0)
            .color(ui::MUTED),
        );

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 8.0) / 2.0;
            if ui
                .add_sized([width, ui::TOUCH_HEIGHT], egui::Button::new("VPN settings"))
                .clicked()
            {
                loaded.vpn.open_vpn_settings();
                host.haptic(Haptic::Light);
            }
            if ui
                .add_sized(
                    [width, ui::TOUCH_HEIGHT],
                    egui::Button::new("Notifications"),
                )
                .clicked()
            {
                loaded.vpn.open_notification_settings();
                host.haptic(Haptic::Light);
            }
        });
        ui.label(
            egui::RichText::new(
                "There is no permission to grant ahead of time — Capture all traffic raises \
                 Android's consent dialog, and only then does Privaxy appear under VPN settings, \
                 where it can be revoked or made always-on. Notifications are what make the \
                 ongoing row and its Stop button visible; capture runs either way.",
            )
            .size(11.0)
            .color(ui::MUTED),
        );

        if status == VpnStatus::Running {
            ui.add_space(10.0);
            let stats = loaded.vpn.stats();
            ui.columns(2, |columns| {
                ui::stat(&mut columns[0], "Connections", stats.tcp_flows, ui::ACCENT);
                ui::stat(&mut columns[1], "Refused", stats.refused, ui::BAD);
            });
            ui.add_space(10.0);
            ui.columns(2, |columns| {
                ui::stat(&mut columns[0], "DNS lookups", stats.dns_queries, ui::MUTED);
                ui::stat(
                    &mut columns[1],
                    "QUIC dropped",
                    stats.quic_dropped,
                    ui::WARN,
                );
            });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!(
                    "{} addresses currently attributable to a hostname",
                    loaded.vpn.known_hosts()
                ))
                .size(11.0)
                .color(ui::MUTED),
            );
        }
    });
}
