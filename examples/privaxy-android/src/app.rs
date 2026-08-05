//! The app struct, its lifecycle, and the tab chrome the screens are drawn into.

use crate::proxy::ProxyHandle;
use crate::proxy::config::{Config, Paths};
use crate::proxy::state::{ProxyState, Status};
use crate::ui::{self, Tab};
use crate::vpn::{CaptureSettings, VpnController};
use egui_mobile::{CreateContext, EguiApp, Haptic, Host, egui};
use std::sync::Arc;

/// The tun's MTU. Flows terminate in this process and are re-originated over the real interface,
/// so there is no tunnel overhead to leave room for.
const TUN_MTU: u16 = 1500;

/// Everything that only exists once storage has been located and the configuration read.
pub struct Loaded {
    pub paths: Paths,
    pub config: Config,
    pub state: Arc<ProxyState>,
    pub proxy: Option<ProxyHandle>,
    pub vpn: VpnController,
}

impl Loaded {
    pub fn start_proxy(&mut self) {
        if self.proxy.is_some() {
            return;
        }
        self.proxy = Some(ProxyHandle::start(
            &self.config,
            self.paths.clone(),
            self.state.clone(),
        ));
        if self.config.foreground_service {
            self.vpn
                .keep_alive(&format!("Proxy on 127.0.0.1:{}", self.config.listen_port));
        }
    }

    pub fn stop_proxy(&mut self) {
        if let Some(mut proxy) = self.proxy.take() {
            proxy.stop();
        }
        // Capture without a proxy to relay into would leave every app without a network.
        self.vpn.shutdown();
        self.state.set_status(Status::Stopped);
    }

    pub fn restart_proxy(&mut self) {
        let was_capturing = self.vpn.status().is_on();
        self.stop_proxy();
        self.start_proxy();
        if was_capturing {
            self.start_capture();
        }
    }

    /// Claim the tun. Consent is asked for on the first run and remembered by the system after.
    pub fn start_capture(&mut self) {
        self.start_proxy();
        self.vpn.start_capture(CaptureSettings {
            proxy_port: self.config.listen_port,
            mtu: TUN_MTU,
            capture_ipv6: self.config.capture_ipv6,
            block_quic: self.config.block_quic,
            dns: self.config.dns_servers(),
        });
    }

    pub fn stop_capture(&mut self) {
        self.vpn
            .stop_capture(&format!("Proxy on 127.0.0.1:{}", self.config.listen_port));
        if !self.config.foreground_service {
            self.vpn.shutdown();
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.paths
            .save(&self.config)
            .map_err(|error| error.to_string())
    }

    /// Adds `||target^` to the custom rules and rebuilds the engine from the on-disk cache.
    ///
    /// The domain anchor matches the host and every subdomain, which is the form
    /// [`FilterEngine::check_host`](crate::proxy::blocker::FilterEngine::check_host) is built to
    /// hit at CONNECT time — so it blocks in both interception modes.
    ///
    /// Returns the rule, or `None` if that target was already blocked.
    pub fn block(&mut self, target: &str) -> Option<String> {
        // RequestEvent::host() keeps the port; `||example.com:8443^` is not a valid host rule.
        let target = target.split(':').next().unwrap_or_default().trim();
        if target.is_empty() {
            return None;
        }

        let rule = format!("||{target}^");
        if self.config.custom_filters.iter().any(|existing| existing == &rule) {
            return None;
        }
        self.config.custom_filters.push(rule.clone());
        let _ = self.save();

        if let Some(proxy) = self.proxy.as_ref() {
            // `force: false` reads the cached lists — adding one rule must not refetch twelve.
            proxy.reload_filters(
                self.config.filters.clone(),
                self.config.custom_filters.clone(),
                false,
            );
        }
        Some(rule)
    }

    /// Adds an Adblock exception so a host is never blocked, whichever list matched it.
    ///
    /// The common failure is a subscription breaking a site the user never blocked themselves, so
    /// removing a custom rule is not enough — `@@` is what overrides EasyList.
    pub fn allow(&mut self, target: &str) -> Option<String> {
        let target = target.split(':').next().unwrap_or_default().trim();
        if target.is_empty() {
            return None;
        }
        let rule = format!("@@||{target}^");
        if self.config.custom_filters.iter().any(|existing| existing == &rule) {
            return None;
        }
        let mut next = self.config.custom_filters.clone();
        next.push(rule.clone());
        self.set_custom_filters(next);
        Some(rule)
    }

    pub fn is_allowed(&self, target: &str) -> bool {
        let target = target.split(':').next().unwrap_or_default();
        let rule = format!("@@||{target}^");
        self.config.custom_filters.iter().any(|existing| existing == &rule)
    }

    /// Replaces the custom rule set and rebuilds the engine from the cached lists.
    pub fn set_custom_filters(&mut self, list: Vec<String>) {
        self.config.custom_filters = list;
        let _ = self.save();
        if let Some(proxy) = self.proxy.as_ref() {
            proxy.reload_filters(
                self.config.filters.clone(),
                self.config.custom_filters.clone(),
                false,
            );
        }
    }

    /// Removes a previously added rule. Returns whether one went away.
    pub fn unblock(&mut self, rule: &str) -> bool {
        let before = self.config.custom_filters.len();
        self.config.custom_filters.retain(|existing| existing != rule);
        if self.config.custom_filters.len() == before {
            return false;
        }
        let _ = self.save();
        if let Some(proxy) = self.proxy.as_ref() {
            proxy.reload_filters(
                self.config.filters.clone(),
                self.config.custom_filters.clone(),
                false,
            );
        }
        true
    }

    /// Replaces the never-intercept list and pushes it to a running proxy.
    pub fn set_exclusions(&mut self, list: Vec<String>) {
        self.config.exclusions = list.iter().cloned().collect();
        let _ = self.save();
        if let Some(proxy) = self.proxy.as_ref() {
            proxy.set_exclusions(list);
        }
    }

    /// Replaces the intercept list and pushes it to a running proxy.
    pub fn set_intercepts(&mut self, list: Vec<String>) {
        self.config.intercepts = list.iter().cloned().collect();
        let _ = self.save();
        if let Some(proxy) = self.proxy.as_ref() {
            proxy.set_intercepts(list);
        }
    }

    pub fn is_excluded(&self, target: &str) -> bool {
        let target = target.split(':').next().unwrap_or_default();
        self.config.exclusions.iter().any(|entry| entry == target)
    }

    pub fn is_intercepted(&self, target: &str) -> bool {
        let target = target.split(':').next().unwrap_or_default();
        self.config.intercepts.iter().any(|entry| entry == target)
    }

    pub fn is_blocked(&self, target: &str) -> bool {
        let target = target.split(':').next().unwrap_or_default();
        let rule = format!("||{target}^");
        self.config.custom_filters.iter().any(|existing| existing == &rule)
    }
}

pub struct PrivaxyApp {
    pub loaded: Option<Loaded>,
    pub init_error: Option<String>,
    pub tab: Tab,

    // Text buffers the settings and filter screens edit, flushed into the config on save.
    pub port_text: String,
    pub vpn_dns_text: String,
    pub exclusions_draft: String,
    pub custom_filters_draft: String,
    pub intercepts_draft: String,
    pub request_query: String,
    pub block_domain_text: String,
    pub request_filters: ui::requests::RequestFilters,
    /// The logged exchange the inspector is open on, if any.
    pub selected_request: Option<u64>,
    pub inspect_tab: ui::inspect::InspectTab,
    /// Wrap headers and bodies, or let them run and scroll sideways.
    pub inspect_wrap: bool,
    pub inspect_json: ui::inspect::JsonCache,
    pub notice: Option<String>,
    /// Filter subscriptions changed but the engine has not been rebuilt yet.
    pub filters_dirty: bool,
    /// The notification permission has been asked for this session.
    pub notifications_asked: bool,
}

impl PrivaxyApp {
    pub fn new(_cc: &CreateContext) -> Self {
        Self {
            loaded: None,
            init_error: None,
            tab: Tab::Dashboard,
            port_text: String::new(),
            vpn_dns_text: String::new(),
            exclusions_draft: String::new(),
            custom_filters_draft: String::new(),
            intercepts_draft: String::new(),
            request_query: String::new(),
            block_domain_text: String::new(),
            request_filters: ui::requests::RequestFilters::default(),
            selected_request: None,
            inspect_tab: ui::inspect::InspectTab::Overview,
            inspect_wrap: true,
            inspect_json: ui::inspect::JsonCache::default(),
            notice: None,
            filters_dirty: false,
            notifications_asked: false,
        }
    }

    /// The private files directory is pushed in by the runtime rather than known at construction,
    /// so loading is retried each frame until it appears.
    fn ensure_loaded(&mut self, host: &Host) {
        if self.loaded.is_some() || self.init_error.is_some() {
            return;
        }

        let Some(directory) = host.documents_dir() else {
            return;
        };

        let paths = Paths::new(directory);
        match paths.load_or_create() {
            Ok(config) => {
                self.port_text = config.listen_port.to_string();
                self.vpn_dns_text = config.vpn_dns.clone();
                self.request_filters = config.request_filters.clone();
                self.inspect_wrap = config.inspect_wrap;

                let state = Arc::new(ProxyState::new(config.mode));
                let start_now = config.start_on_launch;
                let capture_now = config.capture_all;
                let mut loaded = Loaded {
                    paths,
                    config,
                    state,
                    proxy: None,
                    vpn: VpnController::new(),
                };
                if start_now {
                    loaded.start_proxy();
                }
                // Consent survives a restart, so this normally comes back up without a dialog.
                if start_now && capture_now && loaded.vpn.is_supported() {
                    loaded.start_capture();
                }
                self.loaded = Some(loaded);
            }
            Err(error) => {
                self.init_error = Some(format!("Could not open Privaxy's storage: {error}"));
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Privaxy")
                    .size(16.0)
                    .strong()
                    .color(ui::ACCENT),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, color) = match self.loaded.as_ref().map(|loaded| loaded.state.status()) {
                    Some(Status::Running { .. }) => ("• Running", ui::GOOD),
                    Some(Status::Starting) => ("• Starting", ui::WARN),
                    Some(Status::Failed(_)) => ("• Failed", ui::BAD),
                    _ => ("• Stopped", ui::MUTED),
                };
                ui.label(egui::RichText::new(text).size(12.0).color(color));
            });
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.horizontal(|ui| {
            let gaps = ui.spacing().item_spacing.x * (Tab::ALL.len() - 1) as f32;
            let width = (ui.available_width() - gaps) / Tab::ALL.len() as f32;
            for tab in Tab::ALL {
                let selected = self.tab == tab;
                // White on the violet chip, not violet on violet — the latter was 2:1.
                let text = egui::RichText::new(tab.label())
                    .size(13.0)
                    .strong()
                    .color(if selected { ui::ON_ACCENT } else { ui::MUTED });

                if ui
                    .add_sized(
                        [width, ui::TOUCH_HEIGHT],
                        egui::Button::selectable(selected, text),
                    )
                    .clicked()
                    && !selected
                {
                    self.tab = tab;
                    host.haptic(Haptic::Selection);
                }
            }
        });
    }
}

impl EguiApp for PrivaxyApp {
    fn theme(&self, ctx: &egui::Context) {
        ui::apply_theme(ctx);
    }

    fn on_start(&mut self, _ctx: &egui::Context, host: &Host) {
        self.ensure_loaded(host);
    }

    fn on_pause(&mut self, _host: &Host) {
        // The proxy is deliberately left running: backgrounding the app should not drop every
        // tunnel. Android may still reclaim the process — see the note on the dashboard.
        if let Some(loaded) = self.loaded.as_ref() {
            let _ = loaded.save();
        }
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        // Redundant since egui-android's `run_with` started calling `EguiApp::theme` before the
        // first frame; kept because it is idempotent and the backdrop below has to run per frame.
        ui::apply_theme(ui.ctx());
        ui::paint_backdrop(ui.ctx());
        // Before anything paints into the chrome rects — the grab-pass blurs whatever is already
        // in the framebuffer where it lands.
        ui::frost::frost_chrome(ui);

        self.ensure_loaded(host);

        // The consent dialog's answer and the VPN service's callbacks both arrive on Java threads.
        if let Some(loaded) = self.loaded.as_mut() {
            if loaded.vpn.poll() {
                // Revoking the VPN in Settings stops the service outright, notification included,
                // so a proxy that is still listening has to claim the foreground again.
                if loaded.proxy.is_some() && loaded.config.foreground_service {
                    let detail = format!("Proxy on 127.0.0.1:{}", loaded.config.listen_port);
                    loaded.vpn.keep_alive(&detail);
                }
                // Only once capture is up: a permission dialog raised alongside the VPN consent
                // dialog lands on top of it and cancels it.
                if *loaded.vpn.status() == crate::vpn::VpnStatus::Running
                    && !self.notifications_asked
                {
                    self.notifications_asked = true;
                    ui::request_notifications(host);
                }
            }
        }

        // Tight: this bar is a status strip, not a title block, and every point it gives up goes
        // to the list underneath it.
        let top = egui::Panel::top("header")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 3)))
            .show(ui, |ui| self.header(ui))
            .response
            .rect;

        let bottom = egui::Panel::bottom("tabs")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)))
            .show(ui, |ui| self.tab_bar(ui, host))
            .response
            .rect;

        // Stashed for the next frame to frost: the rect is only known now, and the blur has to be
        // enqueued before the content paints.
        ui::frost::remember(ui.ctx(), top, bottom);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                if let Some(error) = self.init_error.clone() {
                    ui::card(ui, |ui| {
                        ui.label(egui::RichText::new(error).color(ui::BAD));
                    });
                    return;
                }

                if self.loaded.is_none() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.spinner();
                        ui.label(egui::RichText::new("Opening storage…").color(ui::MUTED));
                    });
                    return;
                }

                if let Some(notice) = self.notice.clone() {
                    ui::card(ui, |ui| {
                        // Wrapped, and Dismiss on its own full-width row. A long notice laid out
                        // in a `horizontal` does not wrap, so it widened the whole page and
                        // pushed the only way to close it off the right edge.
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(notice).color(ui::GOOD).size(13.0),
                            )
                            .wrap_mode(egui::TextWrapMode::Wrap),
                        );
                        ui.add_space(8.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), 34.0],
                                egui::Button::new("Dismiss"),
                            )
                            .clicked()
                        {
                            self.notice = None;
                        }
                    });
                    ui.add_space(4.0);
                }

                // The inspector owns the whole central area rather than living inside the page
                // scroller. Its own panes scroll, and a scroll area nested in another captures the
                // entire touch drag from first contact — once the inner one hits its end the page
                // is frozen until the finger lifts, because drag scrolling never chains to a parent.
                if self.tab == Tab::Requests && self.selected_request.is_some() {
                    ui::inspect::show(self, ui, host);
                    return;
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    // The request log is long and its position matters; the bar doubles as the
                    // "how far down am I" readout, so it stays drawn rather than fading in.
                    .scroll_bar_visibility(if self.tab == Tab::Requests {
                        egui::scroll_area::ScrollBarVisibility::AlwaysVisible
                    } else {
                        egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded
                    })
                    .show(ui, |ui| match self.tab {
                        Tab::Dashboard => ui::dashboard::show(self, ui, host),
                        Tab::Requests => ui::requests::show(self, ui, host),
                        Tab::Filters => ui::filters::show(self, ui, host),
                        Tab::Settings => ui::settings::show(self, ui, host),
                    });
            });

        // Persist the log's filters and the wrap toggle when they change, so they survive a
        // restart. Compared rather than written every frame: this is a whole-config JSON write.
        if let Some(loaded) = self.loaded.as_mut() {
            if loaded.config.request_filters != self.request_filters
                || loaded.config.inspect_wrap != self.inspect_wrap
            {
                loaded.config.request_filters = self.request_filters.clone();
                loaded.config.inspect_wrap = self.inspect_wrap;
                let _ = loaded.save();
            }
        }

        // Back unwinds one layer per press, innermost first. Drawn last so a layer opened this
        // frame is not closed by the same press that opened it.
        if ui::back_pressed(ui.ctx()) {
            if self.notice.is_some() {
                self.notice = None;
            } else if self.selected_request.is_some() {
                self.selected_request = None;
            } else if self.request_filters.show_filters {
                self.request_filters.show_filters = false;
            } else if self.tab != Tab::Dashboard {
                self.tab = Tab::Dashboard;
            }
            host.haptic(Haptic::Light);
        }

        // Counters and the request log change on the proxy's threads, which cannot wake the frame
        // loop, so the UI polls instead of idling.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
}
