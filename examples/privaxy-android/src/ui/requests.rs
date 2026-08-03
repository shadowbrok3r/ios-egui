//! The live request log: search, filters, sort, optional grouping by domain, and the way into
//! [`crate::ui::inspect`].

use crate::app::PrivaxyApp;
use crate::proxy::state::{EventKind, RequestEvent};
use crate::ui;
use egui_mobile::{Haptic, Host, egui};
use std::collections::BTreeMap;

/// Rows pulled from the log before filtering. The log itself holds more; drawing all of it on a
/// phone costs more than it shows.
const MAX_SHOWN: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum KindFilter {
    All,
    /// Only entries with something to inspect: everything that is not a bare CONNECT.
    Inspectable,
    Blocked,
    Proxied,
    Connects,
}

impl KindFilter {
    pub const ALL: [KindFilter; 5] = [
        KindFilter::All,
        KindFilter::Inspectable,
        KindFilter::Blocked,
        KindFilter::Proxied,
        KindFilter::Connects,
    ];

    fn label(self) -> &'static str {
        match self {
            KindFilter::All => "All",
            KindFilter::Inspectable => "Inspectable",
            KindFilter::Blocked => "Blocked",
            KindFilter::Proxied => "Proxied",
            KindFilter::Connects => "Connects",
        }
    }

    fn accepts(self, kind: &EventKind) -> bool {
        match self {
            KindFilter::All => true,
            // Every HTTPS connection produces a CONNECT row; hiding them is what turns the log
            // from a wall of tunnels into the requests actually worth reading.
            KindFilter::Inspectable => {
                matches!(kind, EventKind::Proxied | EventKind::Blocked { .. })
            }
            KindFilter::Blocked => matches!(kind, EventKind::Blocked { .. }),
            KindFilter::Proxied => matches!(kind, EventKind::Proxied),
            KindFilter::Connects => {
                matches!(kind, EventKind::Tunneled | EventKind::Intercepted)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StatusFilter {
    Any,
    Success,
    Redirect,
    ClientError,
    ServerError,
}

impl StatusFilter {
    pub const ALL: [StatusFilter; 5] = [
        StatusFilter::Any,
        StatusFilter::Success,
        StatusFilter::Redirect,
        StatusFilter::ClientError,
        StatusFilter::ServerError,
    ];

    fn label(self) -> &'static str {
        match self {
            StatusFilter::Any => "Any",
            StatusFilter::Success => "2xx",
            StatusFilter::Redirect => "3xx",
            StatusFilter::ClientError => "4xx",
            StatusFilter::ServerError => "5xx",
        }
    }

    fn accepts(self, status: Option<u16>) -> bool {
        match self {
            StatusFilter::Any => true,
            // A request with no status never got a response; it cannot match a status class.
            _ => status.is_some_and(|status| match self {
                StatusFilter::Success => (200..300).contains(&status),
                StatusFilter::Redirect => (300..400).contains(&status),
                StatusFilter::ClientError => (400..500).contains(&status),
                StatusFilter::ServerError => (500..600).contains(&status),
                StatusFilter::Any => true,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RequestSort {
    Newest,
    Oldest,
    Slowest,
    Largest,
    Host,
}

impl RequestSort {
    pub const ALL: [RequestSort; 5] = [
        RequestSort::Newest,
        RequestSort::Oldest,
        RequestSort::Slowest,
        RequestSort::Largest,
        RequestSort::Host,
    ];

    fn label(self) -> &'static str {
        match self {
            RequestSort::Newest => "Newest",
            RequestSort::Oldest => "Oldest",
            RequestSort::Slowest => "Slowest",
            RequestSort::Largest => "Largest",
            RequestSort::Host => "Host",
        }
    }
}

/// Everything the list is narrowed and ordered by. Lives on the app so it survives tab switches.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RequestFilters {
    pub kind: KindFilter,
    pub status: StatusFilter,
    /// Empty means any method.
    pub method: String,
    pub sort: RequestSort,
    pub group_by_domain: bool,
    pub show_filters: bool,
}

impl Default for RequestFilters {
    fn default() -> Self {
        Self {
            kind: KindFilter::All,
            status: StatusFilter::Any,
            method: String::new(),
            sort: RequestSort::Newest,
            group_by_domain: false,
            show_filters: false,
        }
    }
}

impl RequestFilters {
    /// Whether anything is narrowing the list, for the "showing N of M" line.
    fn is_narrowing(&self) -> bool {
        self.kind != KindFilter::All || self.status != StatusFilter::Any || !self.method.is_empty()
    }
}

/// A row's measurements, read once so the list does not lock every exchange twice.
struct Row {
    event: RequestEvent,
    status: Option<u16>,
    bytes: u64,
    millis: Option<i64>,
}

/// What a tap on a row asked for.
enum RowAction {
    /// The host is already blocked and the icon was tapped again.
    Unblock,
    None,
    Inspect,
    Block,
}

pub fn show(app: &mut PrivaxyApp, ui: &mut egui::Ui, host: &Host) {
    // The inspector is routed from `app.rs` so it can own the central area outright, rather than
    // being drawn inside the page scroller.
    let Some(loaded) = app.loaded.as_ref() else {
        return;
    };

    ui.horizontal(|ui| {
        let filters = 72.0;
        // The real spacing, not a guess: a hard-coded reserve is what pushed Clear log off the
        // screen edge the moment a third button joined this row.
        let field = (ui.available_width() - filters - ui.spacing().item_spacing.x).max(80.0);
        ui.add_sized(
            [field, 34.0],
            egui::TextEdit::singleline(&mut app.request_query).hint_text("Host, path or header"),
        );
        let filtering = app.request_filters.show_filters;
        if ui
            .add_sized(
                [filters, 34.0],
                egui::Button::selectable(filtering, "Filters"),
            )
            .clicked()
        {
            app.request_filters.show_filters = !filtering;
        }
    });

    ui.add_space(6.0);
    let mut save = false;
    ui.horizontal(|ui| {
        let gaps = ui.spacing().item_spacing.x * 2.0;
        let each = (ui.available_width() - gaps) / 3.0;

        // Freezing the log is what makes a long body readable: the ring keeps turning while you
        // scroll otherwise, and bodies evict once 60 newer entries arrive.
        let paused = loaded.state.paused();
        if ui
            .add_sized(
                [each, 34.0],
                // A plain Button in both states: `selectable` paints no frame when unselected,
                // which left it looking unlike the two buttons beside it.
                egui::Button::new(
                    egui::RichText::new(if paused { "Paused" } else { "Pause" })
                        .size(12.0)
                        .color(if paused { ui::ON_ACCENT } else { ui::TEXT }),
                )
                .fill(if paused { ui::ACCENT_FILL } else { ui::GLASS_RAISED }),
            )
            .clicked()
        {
            loaded.state.set_paused(!paused);
            host.haptic(Haptic::Light);
        }
        // Named for the log, not the search box above it.
        if ui
            .add_sized(
                [each, 34.0],
                egui::Button::new(egui::RichText::new("Clear log").size(12.0)),
            )
            .clicked()
        {
            loaded.state.clear_events();
            app.selected_request = None;
            host.haptic(Haptic::Light);
        }
        if ui
            .add_sized(
                [ui.available_width(), 34.0],
                egui::Button::new(egui::RichText::new("Save .har").size(12.0)),
            )
            .clicked()
        {
            save = true;
        }
    });

    // Outside the closure and returning: `save_capture` needs `&mut app`, which the `loaded`
    // borrow above rules out until this path stops using it.
    if save {
        save_capture(app, host);
        return;
    }

    if app.request_filters.show_filters {
        ui.add_space(6.0);
        filter_panel(&mut app.request_filters, ui);
    }

    ui.add_space(8.0);

    let query = app.request_query.trim().to_lowercase();
    let all = loaded.state.recent_events(MAX_SHOWN);
    let total = all.len();
    let mut rows = collect(all, &query, &app.request_filters);

    match app.request_filters.sort {
        RequestSort::Newest => {}
        RequestSort::Oldest => rows.reverse(),
        RequestSort::Slowest => rows.sort_by(|a, b| b.millis.unwrap_or(-1).cmp(&a.millis.unwrap_or(-1))),
        RequestSort::Largest => rows.sort_by(|a, b| b.bytes.cmp(&a.bytes)),
        RequestSort::Host => rows.sort_by(|a, b| a.event.host().cmp(b.event.host())),
    }

    if rows.is_empty() {
        let narrowed = !query.is_empty() || app.request_filters.is_narrowing();
        let mut reset = false;
        ui::card(ui, |ui| {
            ui.label(
                egui::RichText::new(if narrowed {
                    "Nothing matches the current search and filters."
                } else {
                    "No requests yet. Turn on capture, or point a device at the proxy, and browse."
                })
                .size(13.0)
                .color(ui::MUTED),
            );
            // Filters persist across restarts, so the app can open onto an empty list with no
            // visible cause. Offer the way out right where the confusion is.
            if narrowed {
                ui.add_space(8.0);
                reset = ui
                    .add_sized(
                        [ui.available_width(), ui::TOUCH_HEIGHT],
                        egui::Button::new("Clear search and filters"),
                    )
                    .clicked();
            }
        });
        if reset {
            app.request_query.clear();
            let show_filters = app.request_filters.show_filters;
            app.request_filters = RequestFilters::default();
            app.request_filters.show_filters = show_filters;
            host.haptic(Haptic::Light);
        }
        return;
    }

    ui.label(
        egui::RichText::new(format!("Showing {} of {total}", rows.len()))
            .size(10.0)
            .color(ui::MUTED),
    );
    ui.add_space(6.0);

    let is_blocked = |target: &str| {
        app.loaded
            .as_ref()
            .is_some_and(|loaded| loaded.is_blocked(target))
    };

    let mut selected = None;
    let mut to_block = None;
    let mut to_unblock = None;
    if app.request_filters.group_by_domain {
        for (domain, rows) in group(rows) {
            let blocked = rows
                .iter()
                .filter(|row| matches!(row.event.kind, EventKind::Blocked { .. }))
                .count();
            let header = if blocked > 0 {
                format!("{domain}  ({} · {blocked} blocked)", rows.len())
            } else {
                format!("{domain}  ({})", rows.len())
            };
            egui::CollapsingHeader::new(egui::RichText::new(header).size(13.0).strong())
                .id_salt(("domain", &domain))
                .show(ui, |ui| {
                    for row in &rows {
                        match row_card(row, is_blocked(row.event.host()), ui) {
                            RowAction::Inspect => selected = Some(row.event.id),
                            RowAction::Block => to_block = Some(row.event.host().to_owned()),
                            RowAction::Unblock => {
                                to_unblock = Some(row.event.host().to_owned())
                            }
                            RowAction::None => {}
                        }
                        ui.add_space(6.0);
                    }
                });
            ui.add_space(4.0);
        }
    } else {
        for row in &rows {
            match row_card(row, is_blocked(row.event.host()), ui) {
                RowAction::Inspect => selected = Some(row.event.id),
                RowAction::Block => to_block = Some(row.event.host().to_owned()),
                RowAction::Unblock => to_unblock = Some(row.event.host().to_owned()),
                RowAction::None => {}
            }
            ui.add_space(6.0);
        }
    }

    if let Some(id) = selected {
        app.selected_request = Some(id);
        app.inspect_tab = ui::inspect::InspectTab::Overview;
        host.haptic(Haptic::Selection);
    }
    if let Some(target) = to_unblock {
        ui::apply_unblock(app, &target, host);
    }
    if let Some(target) = to_block {
        ui::apply_block(app, &target, host);
    }

    ui.add_space(16.0);
}

/// Writes the whole log as HAR 1.2 and hands it to Android, which files it under Downloads and
/// offers the share sheet. HAR because DevTools, Charles and Fiddler all import it.
fn save_capture(app: &mut PrivaxyApp, host: &Host) {
    let Some(loaded) = app.loaded.as_ref() else {
        return;
    };

    // The whole log, not the filtered view: a capture the filters silently narrowed would be a
    // trap when it is read back somewhere else.
    let events = loaded.state.recent_events(usize::MAX);
    if events.is_empty() {
        app.notice = Some(String::from("Nothing captured yet."));
        host.haptic(Haptic::Error);
        return;
    }

    let count = events.len();
    match loaded.paths.export_capture(&events, chrono::Local::now()) {
        Ok(path) => {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            host.share_file(path.to_string_lossy().to_string());
            app.notice = Some(format!(
                "Saved {count} requests to Downloads as {name}. Open it with Chrome DevTools \
                 (Network > Import HAR)."
            ));
            host.haptic(Haptic::Success);
        }
        Err(error) => {
            app.notice = Some(format!("Could not write the capture: {error}"));
            host.haptic(Haptic::Error);
        }
    }
}

fn filter_panel(filters: &mut RequestFilters, ui: &mut egui::Ui) {
    ui::card(ui, |ui| {
        ui.label(egui::RichText::new("OUTCOME").size(10.0).color(ui::MUTED));
        ui.horizontal_wrapped(|ui| {
            for option in KindFilter::ALL {
                if ui
                    .selectable_label(filters.kind == option, option.label())
                    .clicked()
                {
                    filters.kind = option;
                }
            }
        });

        ui.add_space(6.0);
        ui.label(egui::RichText::new("STATUS").size(10.0).color(ui::MUTED));
        ui.horizontal_wrapped(|ui| {
            for option in StatusFilter::ALL {
                if ui
                    .selectable_label(filters.status == option, option.label())
                    .clicked()
                {
                    filters.status = option;
                }
            }
        });

        ui.add_space(6.0);
        ui.label(egui::RichText::new("METHOD").size(10.0).color(ui::MUTED));
        ui.horizontal_wrapped(|ui| {
            for option in ["", "GET", "POST", "CONNECT", "PUT", "DELETE"] {
                let label = if option.is_empty() { "Any" } else { option };
                if ui
                    .selectable_label(filters.method == option, label)
                    .clicked()
                {
                    filters.method = option.to_owned();
                }
            }
        });

        ui.add_space(6.0);
        ui.label(egui::RichText::new("SORT").size(10.0).color(ui::MUTED));
        ui.horizontal_wrapped(|ui| {
            for option in RequestSort::ALL {
                if ui
                    .selectable_label(filters.sort == option, option.label())
                    .clicked()
                {
                    filters.sort = option;
                }
            }
        });

        ui.add_space(6.0);
        ui.checkbox(&mut filters.group_by_domain, "Group by domain");

        ui.add_space(6.0);
        if ui.button("Reset").clicked() {
            let show_filters = filters.show_filters;
            *filters = RequestFilters::default();
            filters.show_filters = show_filters;
        }
    });
}

/// Reads each event's exchange once, applying the search across URL, headers and status.
fn collect(events: Vec<RequestEvent>, query: &str, filters: &RequestFilters) -> Vec<Row> {
    events
        .into_iter()
        .filter_map(|event| {
            if !filters.kind.accepts(&event.kind) {
                return None;
            }
            if !filters.method.is_empty() && event.method != filters.method {
                return None;
            }

            let (status, bytes, matches_headers) = match event.exchange.lock() {
                Ok(exchange) => {
                    let matches = !query.is_empty()
                        && exchange
                            .request_headers
                            .iter()
                            .chain(exchange.response_headers.iter())
                            .any(|(name, value)| {
                                name.to_lowercase().contains(query)
                                    || value.to_lowercase().contains(query)
                            });
                    (exchange.status, exchange.response_body.seen, matches)
                }
                Err(_) => (None, 0, false),
            };

            if !filters.status.accepts(status) {
                return None;
            }
            if !query.is_empty()
                && !event.url.to_lowercase().contains(query)
                && !matches_headers
            {
                return None;
            }

            let millis = event
                .exchange
                .lock()
                .ok()
                .and_then(|exchange| exchange.finished_at)
                .map(|finished| (finished - event.at).num_milliseconds());

            Some(Row {
                event,
                status,
                bytes,
                millis,
            })
        })
        .collect()
}

/// Busiest domain first, so the noisiest third party is at the top where it is worth looking.
fn group(rows: Vec<Row>) -> Vec<(String, Vec<Row>)> {
    let mut grouped: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.event.domain()).or_default().push(row);
    }
    let mut grouped: Vec<(String, Vec<Row>)> = grouped.into_iter().collect();
    grouped.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    grouped
}

/// One row, and whichever of its actions was tapped.
fn row_card(row: &Row, blocked: bool, ui: &mut egui::Ui) -> RowAction {
    let (badge, color) = match &row.event.kind {
        EventKind::Blocked { .. } => ("BLOCK", ui::BAD),
        EventKind::Tunneled => ("TUNNEL", ui::MUTED),
        EventKind::Intercepted => ("TLS", ui::ACCENT),
        EventKind::Proxied => ("PROXY", ui::GOOD),
    };

    ui::card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(badge).size(10.0).strong().color(color));
            ui.label(
                egui::RichText::new(row.event.at.format("%H:%M:%S").to_string())
                    .size(10.0)
                    .color(ui::MUTED),
            );
            ui.label(
                egui::RichText::new(&row.event.method)
                    .size(10.0)
                    .color(ui::MUTED),
            );
            if let Some(status) = row.status {
                ui.label(
                    egui::RichText::new(status.to_string())
                        .size(10.0)
                        .strong()
                        .color(status_color(status)),
                );
            }
            if row.bytes > 0 {
                ui.label(
                    egui::RichText::new(ui::format_bytes(row.bytes))
                        .size(10.0)
                        .color(ui::MUTED),
                );
            }
            if let Some(millis) = row.millis {
                ui.label(
                    egui::RichText::new(format!("{millis} ms"))
                        .size(10.0)
                        .color(ui::MUTED),
                );
            }
        });

        ui.label(
            egui::RichText::new(ui::elide(row.event.host(), 44))
                .size(12.0)
                .strong(),
        );
        ui.label(
            egui::RichText::new(ui::elide(row.event.path(), 60))
                .size(11.0)
                .color(ui::MUTED),
        );

        if let EventKind::Blocked { filter } = &row.event.kind {
            ui.label(
                egui::RichText::new(ui::elide(filter, 60))
                    .size(10.0)
                    .monospace()
                    .color(ui::WARN),
            );
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // Block is a small icon off to the left and Inspect takes the rest of the row: they
            // were equal halves, which made blocking a host an easy accidental tap.
            // Blocked stays tappable so the same icon undoes it — the only other route is the
            // Filters screen.
            let block = ui
                .add_sized(
                    [44.0, 32.0],
                    egui::Button::new(
                        egui::RichText::new("🚫")
                            .size(13.0)
                            .color(if blocked { ui::MUTED } else { ui::BAD }),
                    ),
                )
                .on_hover_text(if blocked {
                    "Unblock this host"
                } else {
                    "Block this host"
                })
                .clicked();

            let inspect = ui
                .add_sized(
                    [ui.available_width(), 32.0],
                    egui::Button::new(egui::RichText::new("Inspect").size(12.0)),
                )
                .clicked();

            if inspect {
                RowAction::Inspect
            } else if block {
                if blocked {
                    RowAction::Unblock
                } else {
                    RowAction::Block
                }
            } else {
                RowAction::None
            }
        })
        .inner
    })
}

pub fn status_color(status: u16) -> egui::Color32 {
    match status {
        200..300 => ui::GOOD,
        300..400 => ui::ACCENT,
        400..500 => ui::WARN,
        _ => ui::BAD,
    }
}
