//! One exchange in full: what was sent, what came back, and — where the proxy could see inside —
//! the bodies.
//!
//! There is a hard limit on what can be shown. A `CONNECT` tunnel is opaque by construction, so a
//! request captured in the default hostname-only mode has a hostname and nothing else. Rather than
//! render an empty page, those say why they are empty.
//!
//! This screen owns the whole central area rather than living inside the page scroller: a scroll
//! area nested inside another takes the entire touch drag from first contact and never hands it
//! back, so the page would freeze whenever a finger landed on the body.

use crate::app::PrivaxyApp;
use crate::proxy::state::{EventKind, RequestEvent};
use crate::ui;
use egui::collapsing_header::{CollapsingState, paint_default_icon};
use egui_json_tree::{DefaultExpand, JsonTree, JsonTreeStyle, JsonTreeVisuals};
use egui_mobile::{Haptic, Host, egui};

/// Body characters handed to egui as plain text. Past this, laying it out costs more than reading
/// it; JSON goes through the tree instead and is bounded by what the user expands.
const MAX_RENDERED: usize = 16 * 1024;
/// Bytes shown when a body is not text.
const MAX_HEX: usize = 2 * 1024;
/// Bodies larger than this are not offered to the JSON parser — a parse per relayout would cost
/// more than the tree is worth.
const MAX_JSON: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectTab {
    Overview,
    Request,
    Response,
}

impl InspectTab {
    const ALL: [InspectTab; 3] = [InspectTab::Overview, InspectTab::Request, InspectTab::Response];

    fn label(self) -> &'static str {
        match self {
            InspectTab::Overview => "Overview",
            InspectTab::Request => "Request",
            InspectTab::Response => "Response",
        }
    }
}

/// A parsed body, kept across frames. Keyed by the exchange, the side and the byte count, so a
/// still-streaming body re-parses when it grows and a settled one never re-parses at all.
#[derive(Default)]
pub struct JsonCache {
    key: Option<(u64, bool, usize)>,
    value: Option<serde_json::Value>,
}

pub fn show(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    let Some(id) = app.selected_request else {
        return;
    };
    let Some(event) = app
        .loaded
        .as_ref()
        .and_then(|loaded| loaded.state.event(id))
    else {
        // The log is a ring buffer; an entry can age out while it is open.
        ui::card(ui, |ui| {
            ui.label(
                egui::RichText::new("This request has scrolled out of the log.")
                    .size(13.0)
                    .color(ui::MUTED),
            );
        });
        if ui::big_button(ui, "Back", ui::ACCENT_FILL).clicked() {
            app.selected_request = None;
        }
        return;
    };

    let host_target = event.host().to_owned();
    // Every rule these buttons write drops the port, so the labels must not promise otherwise.
    let host_label = host_target
        .split(':')
        .next()
        .unwrap_or(&host_target)
        .to_owned();
    let domain_target = event.domain();
    let (host_blocked, domain_blocked) = match app.loaded.as_ref() {
        Some(loaded) => (
            loaded.is_blocked(&host_target),
            loaded.is_blocked(&domain_target),
        ),
        None => (false, false),
    };

    let allowed = app
        .loaded
        .as_ref()
        .is_some_and(|loaded| loaded.is_allowed(&host_target));

    let (excluded, intercepted) = match app.loaded.as_ref() {
        Some(loaded) => (
            loaded.is_excluded(&host_target),
            loaded.is_intercepted(&host_target),
        ),
        None => (false, false),
    };

    // Collected rather than applied inline: the buttons are drawn while `event` borrows the log.
    let mut to_block = None;
    let mut to_unblock = None;
    let mut toggle_intercept = false;
    let mut toggle_exclusion = false;
    let mut toggle_allow = false;
    let mut replay = false;

    // ── Chrome: fixed, above the panes ───────────────────────────────────────
    ui.horizontal(|ui| {
        if ui
            .add_sized([96.0, 34.0], egui::Button::new("< Back"))
            .clicked()
        {
            app.selected_request = None;
            host.haptic(Haptic::Light);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_sized([84.0, 34.0], egui::Button::new("Copy URL"))
                .clicked()
            {
                host.copy_text(event.url.clone());
                host.haptic(Haptic::Light);
            }
            let mut wrap = app.inspect_wrap;
            if ui.checkbox(&mut wrap, "Wrap").changed() {
                app.inspect_wrap = wrap;
            }
        });
    });

    // One compact line rather than a card: every point spent up here is a point the headers and
    // body panes do not get, and the full URL is a tap away on Copy URL and in Overview.
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let (badge, color) = match &event.kind {
            EventKind::Blocked { .. } => ("BLOCKED", ui::BAD),
            EventKind::Tunneled => ("TUNNELED", ui::MUTED),
            EventKind::Intercepted => ("TLS", ui::ACCENT),
            EventKind::Proxied => ("PROXIED", ui::GOOD),
        };
        ui.label(egui::RichText::new(badge).size(10.0).strong().color(color));
        ui.label(
            egui::RichText::new(&event.method)
                .size(10.0)
                .strong()
                .color(ui::MUTED),
        );
        if let Some(status) = event.exchange.lock().ok().and_then(|open| open.status) {
            ui.label(
                egui::RichText::new(status.to_string())
                    .size(10.0)
                    .strong()
                    .color(ui::requests::status_color(status)),
            );
        }
        ui.add(
            egui::Label::new(egui::RichText::new(event.host()).size(11.0).color(ui::TEXT))
                .truncate(),
        );
    });
    ui.add(
        egui::Label::new(
            egui::RichText::new(event.path())
                .size(10.0)
                .monospace()
                .color(ui::MUTED),
        )
        .truncate(),
    );

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        // Two gaps between three chips. Subtracting a guessed 12 rather than the real spacing
        // overflowed the row and clipped the last chip off the screen edge.
        let gaps = ui.spacing().item_spacing.x * (InspectTab::ALL.len() - 1) as f32;
        let width = (ui.available_width() - gaps) / InspectTab::ALL.len() as f32;
        for tab in InspectTab::ALL {
            let selected = app.inspect_tab == tab;
            let text = egui::RichText::new(tab.label())
                .size(13.0)
                .strong()
                .color(if selected { ui::ON_ACCENT } else { ui::MUTED });
            if ui
                .add_sized([width, 38.0], egui::Button::selectable(selected, text))
                .clicked()
            {
                app.inspect_tab = tab;
            }
        }
    });
    ui.add_space(8.0);

    match app.inspect_tab {
        InspectTab::Overview => {
            egui::ScrollArea::vertical()
                .id_salt("overview")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    overview(&event, ui);

                    // Blocking lives here rather than in the chrome: it is a decision, not
                    // navigation, and it should not cost the panes any height on the other tabs.
                    ui.add_space(8.0);
                    ui::card(ui, |ui| {
                        ui::section_title(ui, "Block");
                        ui.add_space(6.0);
                        let label = if host_blocked {
                            format!("Unblock {}", ui::elide(&host_label, 24))
                        } else {
                            format!("Block {}", ui::elide(&host_label, 26))
                        };
                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            if host_blocked {
                                to_unblock = Some(host_target.clone());
                            } else {
                                to_block = Some(host_target.clone());
                            }
                        }

                        ui.add_space(6.0);
                        // The whole registrable domain, for a CDN spread across subdomains.
                        let label = if domain_blocked {
                            format!("Unblock *.{}", ui::elide(&domain_target, 22))
                        } else {
                            format!("Block *.{}", ui::elide(&domain_target, 24))
                        };
                        if ui
                            .add_enabled_ui(domain_target != host_target, |ui| {
                                ui.add_sized(
                                    [ui.available_width(), ui::TOUCH_HEIGHT],
                                    egui::Button::new(label),
                                )
                            })
                            .inner
                            .clicked()
                        {
                            if domain_blocked {
                                to_unblock = Some(domain_target.clone());
                            } else {
                                to_block = Some(domain_target.clone());
                            }
                        }
                    });

                    ui.add_space(8.0);
                    ui::card(ui, |ui| {
                        ui::section_title(ui, "Actions");
                        ui.add_space(6.0);

                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new("Replay this request"),
                            )
                            .clicked()
                        {
                            replay = true;
                        }

                        ui.add_space(6.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new("Copy as cURL"),
                            )
                            .clicked()
                        {
                            let (headers, body) = match event.exchange.lock() {
                                Ok(open) => {
                                    (open.request_headers.clone(), open.request_body.bytes.clone())
                                }
                                Err(_) => (Vec::new(), Vec::new()),
                            };
                            host.copy_text(as_curl(&event, &headers, &body));
                            host.haptic(Haptic::Light);
                        }

                        ui.add_space(6.0);
                        // An exception overrides the subscriptions too, which removing a custom
                        // rule cannot do — the usual complaint is EasyList breaking a site.
                        let label = if allowed {
                            format!("Stop allowing {}", ui::elide(&host_label, 22))
                        } else {
                            format!("Never block {}", ui::elide(&host_label, 24))
                        };
                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            toggle_allow = true;
                        }
                    });

                    // The tunnelled/pinned notes tell the user exactly what to change; these are
                    // the two lists they name, so the instruction is actionable where it is read.
                    ui.add_space(8.0);
                    ui::card(ui, |ui| {
                        ui::section_title(ui, "Interception");
                        ui.add_space(6.0);
                        let label = if intercepted {
                            format!("Stop inspecting {}", ui::elide(&host_label, 20))
                        } else {
                            format!("Inspect {}", ui::elide(&host_label, 26))
                        };
                        if ui
                            .add_enabled_ui(!excluded, |ui| {
                                ui.add_sized(
                                    [ui.available_width(), ui::TOUCH_HEIGHT],
                                    egui::Button::new(label),
                                )
                            })
                            .inner
                            .clicked()
                        {
                            toggle_intercept = true;
                        }

                        ui.add_space(6.0);
                        let label = if excluded {
                            format!("Stop excluding {}", ui::elide(&host_label, 21))
                        } else {
                            format!("Never intercept {}", ui::elide(&host_label, 20))
                        };
                        if ui
                            .add_sized(
                                [ui.available_width(), ui::TOUCH_HEIGHT],
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            toggle_exclusion = true;
                        }
                    });
                });
        }
        InspectTab::Request => sides(app, &event, ui, host, true),
        InspectTab::Response => sides(app, &event, ui, host, false),
    }

    if let Some(target) = to_block {
        ui::apply_block(app, &target, host);
    }
    if let Some(target) = to_unblock {
        ui::apply_unblock(app, &target, host);
    }
    if replay {
        let (headers, body) = match event.exchange.lock() {
            Ok(open) => (open.request_headers.clone(), open.request_body.bytes.clone()),
            Err(_) => (Vec::new(), Vec::new()),
        };
        let (method, url) = (event.method.clone(), event.url.clone());
        match app.loaded.as_ref().and_then(|loaded| loaded.proxy.as_ref()) {
            Some(proxy) => {
                proxy.replay(method, url, headers, body);
                app.notice = Some(String::from("Replayed — the new row is at the top of the log."));
                host.haptic(Haptic::Success);
            }
            None => app.notice = Some(String::from("Start the proxy to replay a request.")),
        }
    }
    if toggle_allow {
        let bare = host_target.split(':').next().unwrap_or_default().to_owned();
        if let Some(loaded) = app.loaded.as_mut() {
            if allowed {
                let rule = format!("@@||{bare}^");
                loaded.unblock(&rule);
                app.notice = Some(format!("{bare} follows the filter lists again."));
            } else if loaded.allow(&bare).is_some() {
                app.notice = Some(format!("{bare} is never blocked now, by any list."));
            }
            host.haptic(Haptic::Success);
        }
    }
    if toggle_intercept || toggle_exclusion {
        let bare = host_target
            .split(':')
            .next()
            .unwrap_or_default()
            .to_owned();
        if let Some(loaded) = app.loaded.as_mut() {
            if toggle_intercept {
                let mut list: Vec<String> = loaded.config.intercepts.iter().cloned().collect();
                if intercepted {
                    list.retain(|entry| entry != &bare);
                } else {
                    list.push(bare.clone());
                }
                loaded.set_intercepts(list);
                app.notice = Some(if intercepted {
                    format!("{bare} is no longer inspected. Reconnect for it to take effect.")
                } else {
                    format!("Inspecting {bare}. Reconnect for it to take effect.")
                });
            } else {
                let mut list: Vec<String> = loaded.config.exclusions.iter().cloned().collect();
                if excluded {
                    list.retain(|entry| entry != &bare);
                } else {
                    list.push(bare.clone());
                }
                loaded.set_exclusions(list);
                app.notice = Some(if excluded {
                    format!("{bare} follows the interception mode again.")
                } else {
                    format!("{bare} is never intercepted now.")
                });
            }
            host.haptic(Haptic::Success);
        }
    }
}

fn overview(event: &RequestEvent, ui: &mut egui::Ui) {
    let Ok(exchange) = event.exchange.lock() else {
        return;
    };

    ui::card(ui, |ui| {
        ui::detail_row(
            ui,
            "Started",
            egui::RichText::new(event.at.format("%H:%M:%S%.3f").to_string()).monospace(),
        );
        if let Some(finished) = exchange.finished_at {
            let millis = (finished - event.at).num_milliseconds();
            ui::detail_row(
                ui,
                "Duration",
                egui::RichText::new(format!("{millis} ms")).monospace(),
            );
        }
        ui::detail_row(ui, "Host", egui::RichText::new(event.host()).monospace());
        ui::detail_row(
            ui,
            "Domain",
            egui::RichText::new(event.domain()).monospace(),
        );
        if let Some(status) = exchange.status {
            ui::detail_row(
                ui,
                "Status",
                egui::RichText::new(status.to_string())
                    .monospace()
                    .color(ui::requests::status_color(status)),
            );
        }
        if !exchange.request_body.is_empty() {
            ui::detail_row(
                ui,
                "Sent",
                egui::RichText::new(ui::format_bytes(exchange.request_body.seen)).monospace(),
            );
        }
        if !exchange.response_body.is_empty() {
            ui::detail_row(
                ui,
                "Received",
                egui::RichText::new(ui::format_bytes(exchange.response_body.seen)).monospace(),
            );
        }
    });

    if let EventKind::Blocked { filter } = &event.kind {
        ui.add_space(8.0);
        ui::card(ui, |ui| {
            ui::section_title(ui, "Matched rule");
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(filter)
                    .size(12.0)
                    .monospace()
                    .color(ui::WARN),
            );
        });
    }

    if let Some(note) = &exchange.note {
        ui.add_space(8.0);
        ui::card(ui, |ui| {
            ui.label(egui::RichText::new(note).size(12.0).color(ui::MUTED));
        });
    }
}

/// Headers and Body as two collapsible panes that share the remaining height: collapse either and
/// the other takes the space.
fn sides(app: &mut PrivaxyApp, event: &RequestEvent, ui: &mut egui::Ui, host: &Host, request: bool) {
    let salt_headers = if request { "req_headers" } else { "res_headers" };
    let salt_body = if request { "req_body" } else { "res_body" };

    let (headers, note) = match event.exchange.lock() {
        Ok(exchange) => (
            if request {
                exchange.request_headers.clone()
            } else {
                exchange.response_headers.clone()
            },
            exchange.note.clone(),
        ),
        Err(_) => (Vec::new(), None),
    };

    let wrap = app.inspect_wrap;

    // ONE scroller for the entire tab. The panes used to be fixed-height scrollers of their own,
    // which meant the body could only ever show its own little window and the page itself did not
    // move — so the end of a long body was unreachable. Now the sections lay out at their natural
    // height and this single area scrolls past all of it. It owns both axes when wrapping is off,
    // because a horizontal-only area nested in a vertical one eats the vertical drag.
    let area = if wrap {
        egui::ScrollArea::vertical()
    } else {
        egui::ScrollArea::both()
    };
    area.id_salt(if request { "req_pane" } else { "res_pane" })
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .show(ui, |ui| {

    // A drag on a selectable label is a text selection, not a scroll, and egui then scrolls to
    // follow the selection anchor — dragging the view backwards. Each section has its own Copy,
    // so nothing is lost by making a drag here mean only scroll.
    ui.style_mut().interaction.selectable_labels = false;

    section(ui, salt_headers, "Headers", true, |ui| {
        if !headers.is_empty() && ui.small_button("Copy").clicked() {
            let text = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .collect::<Vec<_>>()
                .join("\n");
            host.copy_text(text);
            host.haptic(Haptic::Light);
        }
    }, |ui| {
        if headers.is_empty() {
            well(ui, |ui| {
                ui.label(
                    egui::RichText::new(match &note {
                        Some(note) => note.as_str(),
                        None if request => "No headers were captured for this request.",
                        None => "No response came back.",
                    })
                    .size(12.0)
                    .color(ui::MUTED),
                );
            });
            return;
        }

        well(ui, |ui| {
            {
                for (name, value) in &headers {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{name}:"))
                                .size(11.0)
                                .monospace()
                                .color(ui::AQUA),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(value).size(11.0).monospace(),
                            )
                            .wrap_mode(text_mode(wrap)),
                        );
                    });
                }
            }
        });
    });

    ui.add_space(6.0);

    // Shared rather than cloned: the title row's Copy and the pane below both need the bytes, and
    // one of the two closures would otherwise move them out from under the other.
    let (bytes, seen, evicted, truncated) = match event.exchange.lock() {
        Ok(exchange) => {
            let body = if request {
                &exchange.request_body
            } else {
                &exchange.response_body
            };
            (
                std::sync::Arc::new(body.bytes.clone()),
                body.seen,
                body.evicted,
                body.truncated(),
            )
        }
        Err(_) => (std::sync::Arc::new(Vec::new()), 0, false, false),
    };
    let for_copy = bytes.clone();

    section(ui, salt_body, "Body", true, |ui| {
        if !for_copy.is_empty() && ui.small_button("Copy").clicked() {
            host.copy_text(String::from_utf8_lossy(&for_copy).into_owned());
            host.haptic(Haptic::Light);
        }
    }, |ui| {
        if seen == 0 {
            well(ui, |ui| {
                ui.label(egui::RichText::new("Empty.").size(12.0).color(ui::MUTED));
            });
            return;
        }

        ui.label(
            egui::RichText::new(format!(
                "{}{}",
                ui::format_bytes(seen),
                if truncated {
                    format!(", showing the first {}", ui::format_bytes(bytes.len() as u64))
                } else {
                    String::new()
                }
            ))
            .size(10.0)
            .color(ui::MUTED),
        );
        ui.add_space(4.0);

        if evicted {
            well(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "The body was dropped to keep the log's memory bounded — only the newest \
                         requests keep theirs.",
                    )
                    .size(12.0)
                    .color(ui::MUTED),
                );
            });
            return;
        }

        // JSON gets the tree; everything else falls back to text, then to hex.
        let key = (event.id, request, bytes.len());
        if app.inspect_json.key != Some(key) {
            app.inspect_json.key = Some(key);
            app.inspect_json.value = (bytes.len() <= MAX_JSON)
                .then(|| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .flatten();
        }

        well(ui, |ui| {
            if let Some(value) = app.inspect_json.value.as_ref() {
                {
                    JsonTree::new(("body", event.id, request), value)
                        .style(
                            JsonTreeStyle::new()
                                .visuals(json_visuals())
                                .font_id(egui::FontId::monospace(11.0))
                                .abbreviate_root(true),
                        )
                        // One level open: the shape of the document is visible, and a large
                        // array does not cost a full walk before it has been asked for.
                        .default_expand(DefaultExpand::ToLevel(1))
                        .show(ui);
                }
                return;
            }

            let text = match as_text(&bytes) {
                Some(text) => text,
                None => hex_dump(&bytes),
            };
            ui.add(
                egui::Label::new(egui::RichText::new(text).size(11.0).monospace())
                    .wrap_mode(text_mode(wrap)),
            );
        });
    });

            // Enough slack to scroll the last line clear of the tab bar.
            ui.add_space(32.0);
        });
}

/// The request as a `curl` command, single-quoted so header values with spaces survive a paste.
fn as_curl(event: &RequestEvent, headers: &[(String, String)], body: &[u8]) -> String {
    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    let mut out = format!("curl -X {} {}", event.method, quote(&event.url));
    for (name, value) in headers {
        let lowered = name.to_ascii_lowercase();
        if lowered == "host" || lowered == "content-length" {
            continue;
        }
        out.push_str(&format!(" \\\n  -H {}", quote(&format!("{name}: {value}"))));
    }
    if !body.is_empty() {
        match std::str::from_utf8(body) {
            Ok(text) => out.push_str(&format!(" \\\n  --data-raw {}", quote(text))),
            Err(_) => out.push_str(" \\\n  # body omitted: not valid UTF-8"),
        }
    }
    out
}

/// A tappable title row plus its collapsing body. `CollapsingHeader` is not used because it derives
/// its id inside a private child `Ui`, so the open state cannot be read back before laying out —
/// which is exactly what the height split needs.
fn section(
    ui: &mut egui::Ui,
    id_salt: &str,
    title: &str,
    default_open: bool,
    header_extra: impl FnOnce(&mut egui::Ui),
    body: impl FnOnce(&mut egui::Ui),
) {
    let id = ui.make_persistent_id(id_salt);
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, default_open);

    let row = ui
        .horizontal(|ui| {
            state.show_toggle_button(ui, paint_default_icon);
            ui.label(
                egui::RichText::new(title.to_uppercase())
                    .size(11.0)
                    .strong()
                    .color(ui::MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Right-to-left, so this insets from the screen edge rather than the label.
                ui.add_space(4.0);
                header_extra(ui);
            });
        })
        .response
        // The whole row toggles, not just the ~18pt arrow. A button inside it still wins the tap.
        .interact(egui::Sense::click());
    if row.clicked() {
        state.toggle(ui);
    }

    state.show_body_unindented(ui, body);
}

/// The recessed pane the monospace content sits in.
fn well<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let mut frame = ui::glass_frame(ui::WELL).inner_margin(egui::Margin::same(10));
    frame.stroke = egui::Stroke::new(1.0, ui::HAIRLINE);
    frame
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn text_mode(wrap: bool) -> egui::TextWrapMode {
    if wrap {
        egui::TextWrapMode::Wrap
    } else {
        egui::TextWrapMode::Extend
    }
}

/// Neon JSON palette: cyan keys against the violet chrome, warm numbers, green strings.
fn json_visuals() -> JsonTreeVisuals {
    JsonTreeVisuals {
        object_key_color: ui::AQUA,
        array_idx_color: ui::MUTED,
        null_color: ui::ACCENT,
        bool_color: ui::ACCENT,
        number_color: ui::WARN,
        string_color: ui::PINK,
        highlight_color: ui::ACCENT_FILL,
        punctuation_color: ui::MUTED,
    }
}

/// The body as text, if it is text. Invalid UTF-8 or a run of control bytes means it is not.
fn as_text(bytes: &[u8]) -> Option<String> {
    let end = bytes.len().min(MAX_RENDERED);
    // Cut on a character boundary so a truncated multi-byte sequence does not read as binary.
    let slice = match std::str::from_utf8(&bytes[..end]) {
        Ok(text) => text,
        Err(error) if error.valid_up_to() > 0 && end < bytes.len() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };

    let control = slice
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    if control * 32 > slice.len() {
        return None;
    }
    Some(slice.to_owned())
}

/// Classic offset / hex / ASCII dump, for bodies that are not text.
fn hex_dump(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(MAX_HEX)];
    let mut out = String::with_capacity(shown.len() * 4);
    for (index, chunk) in shown.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", index * 16));
        for byte in chunk {
            out.push_str(&format!("{byte:02x} "));
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push(' ');
        for byte in chunk {
            out.push(if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            });
        }
        out.push('\n');
    }
    if bytes.len() > shown.len() {
        out.push_str("...\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_bodies_are_read_as_text() {
        assert_eq!(as_text(b"{\"a\":1}").as_deref(), Some("{\"a\":1}"));
        assert_eq!(as_text("héllo\n".as_bytes()).as_deref(), Some("héllo\n"));
    }

    #[test]
    fn binary_bodies_fall_back_to_hex() {
        // A PNG header: valid bytes, but full of control characters.
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01";
        assert!(as_text(png).is_none());
        let dump = hex_dump(png);
        assert!(dump.starts_with("00000000  89 50 4e 47"));
        assert!(dump.contains(".PNG"));
    }

    #[test]
    fn a_body_truncated_mid_character_keeps_its_valid_prefix() {
        // Padded so the render cap lands inside the two-byte é.
        let mut text = "a".repeat(MAX_RENDERED - 1);
        text.push('é');
        text.push_str("tail");

        let shown = as_text(text.as_bytes()).expect("a truncated character is not binary");
        assert_eq!(shown.len(), MAX_RENDERED - 1);
        assert!(shown.chars().all(|character| character == 'a'));
    }

    #[test]
    fn a_short_body_that_is_not_utf8_is_not_text() {
        // Not truncated, just invalid: nothing to salvage, so it reads as binary.
        assert_eq!(as_text(&[0x61, 0xc3]), None);
    }
}
