//! Tailscale device browser. Fetches your tailnet's devices from the Tailscale API (through
//! the native `net.http.*` ops), lists each with its 100.x address, OS and last-seen time, and
//! hands a device to the terminal to SSH into (or copies its address). API key + tailnet + the
//! default SSH user persist across reloads.

use std::time::{SystemTime, UNIX_EPOCH};

use egui_ios_plugin_sdk::abi::{self, net};
use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};
use serde::{Deserialize, Serialize};

const OK: egui::Color32 = egui::Color32::from_rgb(166, 227, 161); // green
const ERR: egui::Color32 = egui::Color32::from_rgb(243, 139, 168); // red
const DIM: egui::Color32 = egui::Color32::from_rgb(127, 132, 156); // overlay
const ACCENT: egui::Color32 = egui::Color32::from_rgb(203, 166, 247); // mauve

const STATE_KEY: &str = "settings";
/// A device seen within this many seconds counts as online.
const ONLINE_WINDOW_SECS: i64 = 300;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Settings {
    api_key: String,
    /// Tailnet name, e.g. `example.com`. Blank means `-` (the key's default tailnet).
    tailnet: String,
    ssh_user: String,
}

/// Subset of the Tailscale `GET /tailnet/{tailnet}/devices` response we render.
#[derive(Clone, Deserialize)]
struct Device {
    #[serde(default)]
    name: String,
    #[serde(default)]
    hostname: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    addresses: Vec<String>,
    #[serde(default, rename = "lastSeen")]
    last_seen: String,
}

impl Device {
    /// The tailnet (100.x) IPv4 address, if any.
    fn tailscale_ip(&self) -> Option<&str> {
        self.addresses.iter().map(String::as_str).find(|a| a.starts_with("100."))
    }
    /// Short display name (strip the MagicDNS suffix).
    fn short_name(&self) -> &str {
        let base = if !self.name.is_empty() { &self.name } else { &self.hostname };
        base.split('.').next().unwrap_or(base)
    }
}

#[derive(Deserialize)]
struct DevicesResponse {
    #[serde(default)]
    devices: Vec<Device>,
}

enum Fetch {
    Idle,
    Pending(u64),
    Failed(String),
}

struct Devices {
    settings: Settings,
    devices: Vec<Device>,
    fetch: Fetch,
    show_settings: bool,
    loaded: bool,
}

impl Devices {
    fn new(_cfg: &CreateConfig) -> Self {
        Devices {
            settings: Settings::default(),
            devices: Vec::new(),
            fetch: Fetch::Idle,
            show_settings: false,
            loaded: false,
        }
    }

    fn ensure_loaded(&mut self, host: &HostHandle) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if let Ok(Some(bytes)) = host.state_get(STATE_KEY) {
            if let Ok(s) = serde_json::from_slice::<Settings>(&bytes) {
                self.settings = s;
            }
        }
        // No key yet → open settings so the user can paste one.
        self.show_settings = self.settings.api_key.trim().is_empty();
    }

    fn persist(&self, host: &HostHandle) {
        if let Ok(bytes) = serde_json::to_vec(&self.settings) {
            let _ = host.state_set(STATE_KEY, &bytes);
        }
    }

    fn refresh(&mut self, host: &HostHandle) {
        let key = self.settings.api_key.trim();
        if key.is_empty() {
            self.fetch = Fetch::Failed("set a Tailscale API key in settings".into());
            return;
        }
        let tailnet = {
            let t = self.settings.tailnet.trim();
            if t.is_empty() { "-" } else { t }
        };
        let request = net::HttpRequest {
            method: "GET".into(),
            url: format!("https://api.tailscale.com/api/v2/tailnet/{tailnet}/devices"),
            headers: vec![
                ("Authorization".into(), format!("Bearer {key}")),
                ("Accept".into(), "application/json".into()),
            ],
            body: Vec::new(),
            timeout_ms: 20_000,
        };
        match host.call(net::op::HTTP_REQUEST, &abi::encode(&request)) {
            Ok(id_bytes) => match net::id_from_bytes(&id_bytes) {
                Some(id) => {
                    self.fetch = Fetch::Pending(id);
                    host.haptic(0);
                }
                None => self.fetch = Fetch::Failed("host returned a bad request id".into()),
            },
            Err(e) => self.fetch = Fetch::Failed(format!("{e}")),
        }
    }

    fn poll(&mut self, host: &HostHandle) {
        let Fetch::Pending(id) = self.fetch else { return };
        let Ok(bytes) = host.call(net::op::HTTP_POLL, &net::id_to_bytes(id)) else {
            self.fetch = Fetch::Failed("poll failed".into());
            return;
        };
        match abi::decode::<net::HttpPoll>(&bytes) {
            Ok(net::HttpPoll::Pending) => {}
            Ok(net::HttpPoll::Done(resp)) => self.apply_response(resp),
            Ok(net::HttpPoll::Error(e)) => self.fetch = Fetch::Failed(e),
            Err(_) => self.fetch = Fetch::Failed("bad poll response".into()),
        }
    }

    fn apply_response(&mut self, resp: net::HttpResponse) {
        if resp.status == 401 || resp.status == 403 {
            self.fetch = Fetch::Failed(format!("auth rejected (HTTP {}); check the API key", resp.status));
            return;
        }
        if !(200..=299).contains(&resp.status) {
            let msg = String::from_utf8_lossy(&resp.body);
            self.fetch = Fetch::Failed(format!("HTTP {}: {}", resp.status, msg.chars().take(120).collect::<String>()));
            return;
        }
        match serde_json::from_slice::<DevicesResponse>(&resp.body) {
            Ok(parsed) => {
                self.devices = parsed.devices;
                self.devices.sort_by(|a, b| a.short_name().cmp(b.short_name()));
                self.fetch = Fetch::Idle;
            }
            Err(e) => self.fetch = Fetch::Failed(format!("parsing devices: {e}")),
        }
    }
}

impl PluginApp for Devices {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        self.ensure_loaded(host);
        self.poll(host);
        if matches!(self.fetch, Fetch::Pending(_)) {
            ui.ctx().request_repaint();
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Devices");
                let pending = matches!(self.fetch, Fetch::Pending(_));
                if ui.add_enabled(!pending, egui::Button::new("↻ Refresh")).clicked() {
                    self.refresh(host);
                }
                if ui.selectable_label(self.show_settings, "⚙").clicked() {
                    self.show_settings = !self.show_settings;
                }
                if pending {
                    ui.spinner();
                }
            });

            if self.show_settings {
                self.settings_ui(ui, host);
                ui.separator();
            }

            if let Fetch::Failed(e) = &self.fetch {
                ui.colored_label(ERR, format!("✗ {e}"));
            }

            self.device_list(ui, host);
        });
    }

    fn save_state(&self) -> Vec<u8> {
        serde_json::to_vec(&self.settings).unwrap_or_default()
    }

    fn restore_state(&mut self, bytes: &[u8]) {
        if let Ok(s) = serde_json::from_slice::<Settings>(bytes) {
            self.settings = s;
            self.loaded = true;
        }
    }
}

impl Devices {
    fn settings_ui(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        egui::Grid::new("settings").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("API key");
            let key = ui.add(
                egui::TextEdit::singleline(&mut self.settings.api_key)
                    .password(true)
                    .hint_text("tskey-api-…")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Tailnet");
            let net_field = ui.add(
                egui::TextEdit::singleline(&mut self.settings.tailnet)
                    .hint_text("example.com  (blank = default)")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("SSH user");
            let user = ui.add(
                egui::TextEdit::singleline(&mut self.settings.ssh_user)
                    .hint_text("root, ubuntu, …")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            if key.lost_focus() || net_field.lost_focus() || user.lost_focus() {
                self.persist(host);
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Save & refresh").clicked() {
                self.persist(host);
                self.show_settings = false;
                self.refresh(host);
            }
            ui.hyperlink_to("get a key", "https://login.tailscale.com/admin/settings/keys");
        });
    }

    fn device_list(&self, ui: &mut egui::Ui, host: &HostHandle) {
        if self.devices.is_empty() && matches!(self.fetch, Fetch::Idle) {
            ui.add_space(8.0);
            ui.colored_label(DIM, "No devices loaded — tap Refresh.");
            return;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for dev in &self.devices {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let online = now
                        .zip(parse_rfc3339_to_unix(&dev.last_seen))
                        .map(|(n, seen)| n - seen <= ONLINE_WINDOW_SECS);
                    let (dot, color) = match online {
                        Some(true) => ("●", OK),
                        Some(false) => ("●", DIM),
                        None => ("○", DIM),
                    };
                    ui.colored_label(color, dot);
                    ui.strong(dev.short_name());
                    if !dev.os.is_empty() {
                        ui.colored_label(DIM, &dev.os);
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    match dev.tailscale_ip() {
                        Some(ip) => {
                            if ui.add(egui::Label::new(egui::RichText::new(ip).monospace().color(ACCENT)).sense(egui::Sense::click())).clicked() {
                                host.copy_text(ip);
                                host.haptic(6);
                            }
                            if ui.small_button("SSH").clicked() {
                                let user = default_user(&self.settings.ssh_user);
                                let req = net::SshOpenRequest { host: ip.to_string(), user, port: 22 };
                                host.emit(net::EVENT_SSH_OPEN, &abi::encode(&req));
                                host.haptic(3);
                            }
                        }
                        None => {
                            ui.colored_label(DIM, "no tailnet IP");
                        }
                    }
                });
                let age = now.zip(parse_rfc3339_to_unix(&dev.last_seen)).map(|(n, s)| n - s);
                if let Some(age) = age {
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.colored_label(DIM, format!("seen {}", human_age(age)));
                    });
                }
                ui.separator();
            }
        });
    }
}

fn default_user(configured: &str) -> String {
    let u = configured.trim();
    if u.is_empty() { "root".into() } else { u.to_string() }
}

fn human_age(secs: i64) -> String {
    if secs < 0 {
        return "just now".into();
    }
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// Parse a UTC `YYYY-MM-DDTHH:MM:SS…Z` timestamp to unix seconds (Howard Hinnant's civil
/// algorithm). Ignores fractional seconds and assumes UTC; `None` on a malformed prefix.
fn parse_rfc3339_to_unix(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let year = num(0, 4)?;
    let mon = num(5, 7)?;
    let day = num(8, 10)?;
    let hh = num(11, 13)?;
    let mm = num(14, 16)?;
    let ss = num(17, 19)?;
    if !(1..=12).contains(&mon) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if mon <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if mon > 2 { mon - 3 } else { mon + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

plugin!(Devices::new);
