//! Listener settings, the certificate authority, and pass-through hosts.

use crate::app::PrivaxyApp;
use crate::proxy::ca::CertAuthority;
use crate::ui;
use egui_mobile::{Haptic, Host, egui};

pub fn show(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };

    ui::card(ui, |ui| {
        ui::section_title(ui, "Listener");
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("Port");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_sized(
                    [90.0, 34.0],
                    egui::TextEdit::singleline(&mut app.port_text).char_limit(5),
                );
            });
        });

        ui.add_space(4.0);
        let mut share = loaded.config.share_on_network;
        if ui
            .checkbox(&mut share, "Reachable from other devices")
            .changed()
        {
            loaded.config.share_on_network = share;
            let _ = loaded.save();
            // The bind address is chosen once in ProxyHandle::start, so without a restart the
            // warning below would describe a proxy that is still on loopback only.
            if loaded.proxy.is_some() {
                loaded.restart_proxy();
            }
        }
        if loaded.config.share_on_network {
            ui.label(
                egui::RichText::new(
                    "The proxy binds every interface. Anyone on the same network can route \
                     traffic through this phone.",
                )
                .size(12.0)
                .color(ui::WARN),
            );
        }

        ui.add_space(4.0);
        let mut start_on_launch = loaded.config.start_on_launch;
        if ui
            .checkbox(&mut start_on_launch, "Start when the app opens")
            .changed()
        {
            loaded.config.start_on_launch = start_on_launch;
            let _ = loaded.save();
        }

        ui.add_space(10.0);
        if ui::big_button(ui, "Apply and restart proxy", ui::ACCENT_FILL).clicked() {
            match app.port_text.trim().parse::<u16>() {
                Ok(port) if port >= 1024 => {
                    loaded.config.listen_port = port;
                    let _ = loaded.save();
                    loaded.restart_proxy();
                    app.notice = Some(format!("Listening on port {port}."));
                    host.haptic(Haptic::Success);
                }
                _ => {
                    app.notice = Some(String::from(
                        "Enter a port between 1024 and 65535 — lower ports need root.",
                    ));
                    host.haptic(Haptic::Error);
                }
            }
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Capture");
        ui.add_space(8.0);

        let mut foreground = loaded.config.foreground_service;
        if ui
            .checkbox(&mut foreground, "Keep running in the background")
            .changed()
        {
            loaded.config.foreground_service = foreground;
            let _ = loaded.save();
            if foreground {
                ui::request_notifications(host);
                loaded
                    .vpn
                    .keep_alive(&format!("Proxy on 127.0.0.1:{}", loaded.config.listen_port));
            } else if !loaded.vpn.status().is_on() {
                loaded.vpn.shutdown();
            }
        }
        ui.label(
            egui::RichText::new(
                "Runs a foreground service with an ongoing notification. Without it Android \
                 reclaims the process once the app is backgrounded, and the proxy goes with it.",
            )
            .size(12.0)
            .color(ui::MUTED),
        );

        ui.add_space(8.0);
        let mut capture_ipv6 = loaded.config.capture_ipv6;
        if ui.checkbox(&mut capture_ipv6, "Capture IPv6").changed() {
            loaded.config.capture_ipv6 = capture_ipv6;
            let _ = loaded.save();
        }
        if !loaded.config.capture_ipv6 {
            ui.label(
                egui::RichText::new("IPv6 traffic bypasses the proxy entirely while this is off.")
                    .size(12.0)
                    .color(ui::WARN),
            );
        }

        ui.add_space(4.0);
        let mut block_quic = loaded.config.block_quic;
        if ui
            .checkbox(&mut block_quic, "Drop QUIC (UDP 443)")
            .changed()
        {
            loaded.config.block_quic = block_quic;
            let _ = loaded.save();
        }
        ui.label(
            egui::RichText::new(
                "An HTTP proxy carries no datagrams, so QUIC would pass unfiltered. Dropping it \
                 makes apps fall back to TCP, where the proxy sees the traffic.",
            )
            .size(12.0)
            .color(ui::MUTED),
        );

        ui.add_space(8.0);
        ui.label(egui::RichText::new("Resolvers for captured apps").size(12.0));
        ui.add(
            egui::TextEdit::singleline(&mut app.vpn_dns_text)
                .desired_width(ui.available_width())
                .hint_text("1.1.1.1,1.0.0.1"),
        );

        ui.add_space(10.0);
        if ui::big_button(ui, "Apply and restart capture", ui::ACCENT_FILL).clicked() {
            loaded.config.vpn_dns = app.vpn_dns_text.trim().to_owned();
            let _ = loaded.save();
            if loaded.vpn.status().is_on() {
                loaded.stop_capture();
                loaded.start_capture();
                app.notice = Some(String::from("Capture restarted."));
            } else {
                app.notice = Some(String::from("Saved. These apply the next time capture starts."));
            }
            host.haptic(Haptic::Success);
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Certificate authority");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Only needed for full inspection. Android will not let an app install a CA \
                 certificate itself, so: save the file, then in Settings go to Security > \
                 Encryption & credentials > Install a certificate > CA certificate and pick \
                 privaxy-ca.crt from Downloads.",
            )
            .size(12.0)
            .color(ui::MUTED),
        );

        ui.add_space(8.0);
        ui.label(egui::RichText::new("SHA-256").size(11.0).color(ui::MUTED));
        ui.label(
            egui::RichText::new(ui::elide(&loaded.config.ca.fingerprint(), 60))
                .size(10.0)
                .monospace(),
        );

        ui.add_space(10.0);
        // Two steps, because Android 11 and later refuse a programmatic CA install outright: the
        // installer answers "Can't install CA certificates" however the intent is built. Writing
        // the file and opening Settings is the only route left.
        if ui::big_button(ui, "1. Save certificate to Downloads", ui::ACCENT_FILL).clicked() {
            match loaded.paths.export_ca(&loaded.config) {
                Ok(path) => {
                    host.share_file(path.to_string_lossy().to_string());
                    app.notice = Some(String::from(
                        "Saved to Downloads as privaxy-ca.crt. Now open security settings and \
                         pick it from Downloads.",
                    ));
                    host.haptic(Haptic::Success);
                }
                Err(error) => {
                    app.notice = Some(format!("Could not write the certificate: {error}"));
                    host.haptic(Haptic::Error);
                }
            }
        }

        ui.add_space(6.0);
        if ui
            .add_sized(
                [ui.available_width(), ui::TOUCH_HEIGHT],
                egui::Button::new("2. Open security settings"),
            )
            .clicked()
        {
            ui::open_security_settings(host);
            host.haptic(Haptic::Light);
        }

        ui.add_space(6.0);
        if ui
            .add_sized(
                [ui.available_width(), ui::TOUCH_HEIGHT],
                egui::Button::new("Regenerate (invalidates the installed copy)"),
            )
            .clicked()
        {
            match CertAuthority::generate() {
                Ok(authority) => {
                    loaded.config.ca = authority;
                    let _ = loaded.save();
                    let _ = loaded.paths.export_ca(&loaded.config);
                    loaded.restart_proxy();
                    app.notice = Some(String::from(
                        "New certificate authority generated. Remove the old certificate on the \
                         device and install the new one.",
                    ));
                    host.haptic(Haptic::Warning);
                }
                Err(error) => {
                    app.notice = Some(format!("Could not generate a certificate: {error}"));
                    host.haptic(Haptic::Error);
                }
            }
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Inspect these hosts");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Terminated even in hostname-only mode, so you can read a few hosts without \
                 switching the whole device to Full inspection — which breaks every app that does \
                 not trust the certificate. Never intercept wins if a host is on both lists.",
            )
            .size(12.0)
            .color(ui::MUTED),
        );
        ui.add_space(6.0);

        let intercepts: Vec<String> = loaded.config.intercepts.iter().cloned().collect();
        match ui::chips::editor(
            ui,
            "intercepts",
            &intercepts,
            &mut app.intercepts_draft,
            "api.example.com",
            "Nothing named — hostname-only mode tunnels everything.",
            host,
        ) {
            ui::chips::Action::Added(entry) => {
                let mut next = intercepts;
                next.push(entry);
                loaded.set_intercepts(next);
                app.notice = Some(String::from("Intercept list saved."));
            }
            ui::chips::Action::Removed(entry) => {
                loaded.set_intercepts(
                    intercepts.into_iter().filter(|item| item != &entry).collect(),
                );
            }
            ui::chips::Action::None => {}
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Never intercept");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "`*.example.com` covers subdomains. These are always tunneled untouched, which \
                 is what certificate-pinned apps need.",
            )
            .size(12.0)
            .color(ui::MUTED),
        );
        ui.add_space(6.0);

        let exclusions: Vec<String> = loaded.config.exclusions.iter().cloned().collect();
        match ui::chips::editor(
            ui,
            "exclusions",
            &exclusions,
            &mut app.exclusions_draft,
            "*.bank.example",
            "Nothing excluded — every host follows the interception mode above.",
            host,
        ) {
            ui::chips::Action::Added(entry) => {
                let mut next = exclusions;
                next.push(entry);
                loaded.set_exclusions(next);
                app.notice = Some(String::from("Never-intercept list saved."));
            }
            ui::chips::Action::Removed(entry) => {
                loaded.set_exclusions(
                    exclusions.into_iter().filter(|item| item != &entry).collect(),
                );
            }
            ui::chips::Action::None => {}
        }
    });

    ui.add_space(10.0);

    ui::card(ui, |ui| {
        ui::section_title(ui, "Storage");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(loaded.paths.root.to_string_lossy())
                .size(10.0)
                .monospace()
                .color(ui::MUTED),
        );
    });

    ui.add_space(16.0);
}
