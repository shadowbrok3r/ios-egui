//! Filter list subscriptions and custom rules.

use crate::app::PrivaxyApp;
use crate::proxy::config::FilterGroup;
use crate::proxy::state::FiltersStatus;
use crate::ui;
use egui_mobile::{Haptic, Host, egui};

pub fn show(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };

    ui::card(ui, |ui| {
        match loaded.state.filters_status() {
            FiltersStatus::Updating { completed, total } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new(format!("Loading {completed} of {total}…")).size(13.0),
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
            FiltersStatus::Idle => {
                ui.label(egui::RichText::new("Not loaded").size(13.0).color(ui::MUTED));
            }
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 8.0) / 2.0;
            if ui
                .add_sized([width, ui::TOUCH_HEIGHT], egui::Button::new("Re-download"))
                .clicked()
            {
                if let Some(proxy) = loaded.proxy.as_ref() {
                    proxy.reload_filters(
                        loaded.config.filters.clone(),
                        loaded.config.custom_filters.clone(),
                        true,
                    );
                }
                app.filters_dirty = false;
                host.haptic(Haptic::Medium);
            }

            let apply = egui::Button::new("Apply changes").fill(if app.filters_dirty {
                ui::ACCENT
            } else {
                ui.visuals().widgets.inactive.bg_fill
            });
            if ui
                .add_enabled_ui(app.filters_dirty, |ui| {
                    ui.add_sized([width, ui::TOUCH_HEIGHT], apply)
                })
                .inner
                .clicked()
            {
                let _ = loaded.save();
                if let Some(proxy) = loaded.proxy.as_ref() {
                    proxy.reload_filters(
                        loaded.config.filters.clone(),
                        loaded.config.custom_filters.clone(),
                        false,
                    );
                }
                app.filters_dirty = false;
                host.haptic(Haptic::Medium);
            }
        });
    });

    ui.add_space(10.0);

    for group in FilterGroup::ALL {
        let indices: Vec<usize> = loaded
            .config
            .filters
            .iter()
            .enumerate()
            .filter(|(_, filter)| filter.group == group)
            .map(|(index, _)| index)
            .collect();

        if indices.is_empty() {
            continue;
        }

        ui::card(ui, |ui| {
            ui::section_title(ui, group.label());
            ui.add_space(6.0);
            for index in indices {
                let filter = &mut loaded.config.filters[index];
                let mut enabled = filter.enabled;
                if ui.checkbox(&mut enabled, &filter.title).changed() {
                    filter.enabled = enabled;
                    app.filters_dirty = true;
                }
            }
        });
        ui.add_space(8.0);
    }

    let mut to_block = None;
    ui::card(ui, |ui| {
        ui::section_title(ui, "Block a domain");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Blocks the domain and every subdomain, in both interception modes.",
            )
            .size(12.0)
            .color(ui::MUTED),
        );
        ui.add_space(6.0);

        let entered = ui
            .add(
                egui::TextEdit::singleline(&mut app.block_domain_text)
                    .desired_width(ui.available_width())
                    .hint_text("ads.example.com"),
            )
            .lost_focus()
            && ui.input(|input| input.key_pressed(egui::Key::Enter));

        ui.add_space(6.0);
        let tapped = ui
            .add_sized(
                [ui.available_width(), ui::TOUCH_HEIGHT],
                egui::Button::new("Add to blocklist"),
            )
            .clicked();

        if (entered || tapped) && !app.block_domain_text.trim().is_empty() {
            to_block = Some(app.block_domain_text.trim().to_owned());
        }
    });

    if let Some(target) = to_block {
        // Strip a pasted scheme/path so "https://ads.example.com/x" still yields a host rule.
        let target = target
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        ui::apply_block(app, &target, host);
        app.block_domain_text.clear();
        return;
    }

    ui.add_space(10.0);

    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };

    ui::card(ui, |ui| {
        ui::section_title(ui, "Custom rules");
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Tap a rule to remove it. Blocking a host from the log adds one here.")
                .size(12.0)
                .color(ui::MUTED),
        );
        ui.add_space(6.0);

        let rules = loaded.config.custom_filters.clone();
        match ui::chips::editor(
            ui,
            "custom_rules",
            &rules,
            &mut app.custom_filters_draft,
            "||ads.example.com^",
            "No custom rules yet.",
            host,
        ) {
            ui::chips::Action::Added(rule) => {
                let mut next = rules;
                next.push(rule);
                loaded.set_custom_filters(next);
            }
            ui::chips::Action::Removed(rule) => {
                loaded.unblock(&rule);
            }
            ui::chips::Action::None => {}
        }
    });

    ui.add_space(16.0);
}
