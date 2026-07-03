//! HTTP/REST client plugin. Builds a request (method, URL, headers, body), sends it through
//! the native `net.http.*` host ops (non-blocking request→poll), and shows the status,
//! headers, and pretty-printed JSON response. The last request persists across reloads.

use egui_ios_plugin_sdk::abi::{self, net};
use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};
use serde::{Deserialize, Serialize};

const OK: egui::Color32 = egui::Color32::from_rgb(166, 227, 161); // green
const REDIRECT: egui::Color32 = egui::Color32::from_rgb(137, 180, 250); // blue
const ERR: egui::Color32 = egui::Color32::from_rgb(243, 139, 168); // red
const DIM: egui::Color32 = egui::Color32::from_rgb(127, 132, 156); // overlay
const ACCENT: egui::Color32 = egui::Color32::from_rgb(203, 166, 247); // mauve

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
const STATE_KEY: &str = "request";

/// The editable request; serialized to the state store so it survives relaunch and reload.
#[derive(Clone, Serialize, Deserialize)]
struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Default for Request {
    fn default() -> Self {
        Request {
            method: "GET".into(),
            url: "https://api.github.com/zen".into(),
            headers: vec![("Accept".into(), "*/*".into())],
            body: String::new(),
        }
    }
}

enum Response {
    Idle,
    Pending(u64),
    Done(net::HttpResponse),
    Failed(String),
}

struct HttpClient {
    req: Request,
    resp: Response,
    loaded: bool,
}

impl HttpClient {
    fn new(_cfg: &CreateConfig) -> Self {
        HttpClient {
            req: Request::default(),
            resp: Response::Idle,
            loaded: false,
        }
    }

    /// Load the persisted request on the first frame (the factory has no host handle).
    fn ensure_loaded(&mut self, host: &HostHandle) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if let Ok(Some(bytes)) = host.state_get(STATE_KEY) {
            if let Ok(req) = serde_json::from_slice::<Request>(&bytes) {
                self.req = req;
            }
        }
    }

    fn persist(&self, host: &HostHandle) {
        if let Ok(bytes) = serde_json::to_vec(&self.req) {
            let _ = host.state_set(STATE_KEY, &bytes);
        }
    }

    fn send(&mut self, host: &HostHandle) {
        let headers = self
            .req
            .headers
            .iter()
            .filter(|(k, _)| !k.trim().is_empty())
            .cloned()
            .collect();
        let request = net::HttpRequest {
            method: self.req.method.clone(),
            url: self.req.url.trim().to_string(),
            headers,
            body: self.req.body.clone().into_bytes(),
            timeout_ms: 0,
        };
        match host.call(net::op::HTTP_REQUEST, &abi::encode(&request)) {
            Ok(id_bytes) => match net::id_from_bytes(&id_bytes) {
                Some(id) => {
                    self.resp = Response::Pending(id);
                    host.haptic(0);
                    self.persist(host);
                }
                None => self.resp = Response::Failed("host returned a bad request id".into()),
            },
            Err(e) => self.resp = Response::Failed(format!("{e}")),
        }
    }

    fn poll(&mut self, host: &HostHandle) {
        let Response::Pending(id) = self.resp else { return };
        match host.call(net::op::HTTP_POLL, &net::id_to_bytes(id)) {
            Ok(bytes) => match abi::decode::<net::HttpPoll>(&bytes) {
                Ok(net::HttpPoll::Pending) => {}
                Ok(net::HttpPoll::Done(resp)) => self.resp = Response::Done(resp),
                Ok(net::HttpPoll::Error(e)) => self.resp = Response::Failed(e),
                Err(_) => self.resp = Response::Failed("bad poll response".into()),
            },
            Err(e) => self.resp = Response::Failed(format!("{e}")),
        }
    }
}

impl PluginApp for HttpClient {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        self.ensure_loaded(host);
        self.poll(host);
        if matches!(self.resp, Response::Pending(_)) {
            ui.ctx().request_repaint();
        }

        egui::CentralPanel::default().show(ui, |ui| {
            // Method + URL + Send -----------------------------------------------------
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("method")
                    .selected_text(&self.req.method)
                    .width(96.0)
                    .show_ui(ui, |ui| {
                        for m in METHODS {
                            ui.selectable_value(&mut self.req.method, m.to_string(), m);
                        }
                    });
                let pending = matches!(self.resp, Response::Pending(_));
                let send = ui.add_enabled(!pending, egui::Button::new(if pending { "…" } else { "Send" }));
                if send.clicked() {
                    self.send(host);
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.req.url)
                        .hint_text("https://…")
                        .desired_width(f32::INFINITY),
                );
            });

            // Headers -----------------------------------------------------------------
            egui::CollapsingHeader::new(format!("Headers ({})", self.req.headers.len()))
                .default_open(false)
                .show(ui, |ui| {
                    let mut remove = None;
                    for (i, (k, v)) in self.req.headers.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                            ui.add(
                                egui::TextEdit::singleline(k)
                                    .hint_text("Header")
                                    .desired_width(120.0),
                            );
                            ui.add(
                                egui::TextEdit::singleline(v)
                                    .hint_text("value")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                    }
                    if let Some(i) = remove {
                        self.req.headers.remove(i);
                    }
                    if ui.button("+ header").clicked() {
                        self.req.headers.push((String::new(), String::new()));
                    }
                });

            // Body (methods that carry one) -------------------------------------------
            let has_body = !matches!(self.req.method.as_str(), "GET" | "HEAD");
            if has_body {
                egui::CollapsingHeader::new("Body")
                    .default_open(!self.req.body.is_empty())
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.req.body)
                                .code_editor()
                                .desired_rows(4)
                                .desired_width(f32::INFINITY)
                                .hint_text("request body (JSON, form, …)"),
                        );
                    });
            }

            ui.separator();
            self.show_response(ui, host);
        });
    }

    fn save_state(&self) -> Vec<u8> {
        serde_json::to_vec(&self.req).unwrap_or_default()
    }

    fn restore_state(&mut self, bytes: &[u8]) {
        if let Ok(req) = serde_json::from_slice::<Request>(bytes) {
            self.req = req;
            self.loaded = true;
        }
    }
}

impl HttpClient {
    fn show_response(&self, ui: &mut egui::Ui, host: &HostHandle) {
        match &self.resp {
            Response::Idle => {
                ui.colored_label(DIM, "No response yet — set a URL and tap Send.");
            }
            Response::Pending(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.colored_label(DIM, "Requesting…");
                });
            }
            Response::Failed(e) => {
                ui.colored_label(ERR, format!("✗ {e}"));
            }
            Response::Done(resp) => {
                let color = match resp.status {
                    200..=299 => OK,
                    300..=399 => REDIRECT,
                    _ => ERR,
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, egui::RichText::new(resp.status.to_string()).strong());
                    ui.colored_label(DIM, format!("{} bytes", resp.body.len()));
                    if ui.button("Copy body").clicked() {
                        host.copy_text(&String::from_utf8_lossy(&resp.body));
                    }
                });

                egui::CollapsingHeader::new(format!("Response headers ({})", resp.headers.len()))
                    .default_open(false)
                    .show(ui, |ui| {
                        for (k, v) in &resp.headers {
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(ACCENT, format!("{k}:"));
                                ui.monospace(v);
                            });
                        }
                    });

                let (text, is_json) = render_body(resp);
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height())
                    .show(ui, |ui| {
                        if is_json {
                            ui.colored_label(DIM, "application/json");
                        }
                        ui.add(
                            egui::Label::new(egui::RichText::new(text).monospace())
                                .wrap_mode(egui::TextWrapMode::Wrap),
                        );
                    });
            }
        }
    }
}

/// Decode the body to text, pretty-printing when it parses as JSON.
fn render_body(resp: &net::HttpResponse) -> (String, bool) {
    let raw = String::from_utf8_lossy(&resp.body);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
        if let Ok(pretty) = serde_json::to_string_pretty(&value) {
            return (pretty, true);
        }
    }
    (raw.into_owned(), false)
}

plugin!(HttpClient::new);
