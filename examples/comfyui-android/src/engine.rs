//! Async ComfyUI engine. A tokio runtime owns all networking; results flow back to the UI thread
//! over an mpsc channel. [`Host`] is main-thread only, so the worker never touches it — it wakes
//! the UI with a cloned [`egui::Context`] and the UI applies effects (haptics, notifications).

use std::sync::mpsc::{Receiver, Sender};

use futures::StreamExt as _;
use rucomfyui::{Client, Event};
use serde_json::Value;

use crate::logger::Logger;
use crate::schema::{self, SchemaSet};
use crate::types::{Img2ImgSource, Mode, Params};
use crate::workflow;

/// A message from the async worker to the UI thread.
pub enum Msg {
    Connected {
        schemas: SchemaSet,
        checkpoints: Vec<String>,
        samplers: Vec<String>,
        schedulers: Vec<String>,
    },
    ConnectError(String),
    Queued,
    Progress { value: u32, max: u32 },
    Status(String),
    Preview(egui::ColorImage),
    Result { image: egui::ColorImage, bytes: Vec<u8> },
    Done,
    Cancelled,
    GenError(String),
}

pub struct Engine {
    rt: tokio::runtime::Runtime,
    ctx: egui::Context,
    log: Logger,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    client: Option<Client>,
    job: Option<tokio::task::JoinHandle<()>>,
}

impl Engine {
    pub fn new(ctx: egui::Context, log: Logger) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build tokio runtime");
        let (tx, rx) = std::sync::mpsc::channel();
        Self { rt, ctx, log, tx, rx, client: None, job: None }
    }

    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    /// Drain any messages the worker has produced since the last frame.
    pub fn drain(&self) -> Vec<Msg> {
        let mut v = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            v.push(m);
        }
        v
    }

    /// Point the client at `url` (with an optional API key), fetch `/object_info` raw, and parse
    /// it leniently into a [`SchemaSet`] (rucomfyui's typed parse fails whole-catalog on servers
    /// with slightly nonconforming custom nodes).
    pub fn connect(&mut self, url: String, api_key: String) {
        let base = normalize_url(&url);
        let log = self.log.clone();
        let key_note = if api_key.trim().is_empty() { "no API key" } else { "with API key" };
        log.info(format!("connect: {base} ({key_note})"));
        let http = match apply_key(tls_builder(), &api_key).build() {
            Ok(c) => c,
            Err(e) => {
                log.error(format!("HTTP client build failed: {e}"));
                let _ = self.tx.send(Msg::ConnectError(e.to_string()));
                self.ctx.request_repaint();
                return;
            }
        };
        self.client = Some(Client::new_with_client(base.clone(), http.clone()));
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let msg = match fetch_object_info(&http, &base, &log).await {
                Ok(schemas) => {
                    let checkpoints = schemas.checkpoints();
                    let samplers = schemas.samplers();
                    let schedulers = schemas.schedulers();
                    log.info(format!(
                        "options: {} checkpoints, {} samplers, {} schedulers",
                        checkpoints.len(),
                        samplers.len(),
                        schedulers.len()
                    ));
                    if checkpoints.is_empty() {
                        log.warn("no checkpoints found in any *CheckpointLoader* node");
                    }
                    Msg::Connected { schemas, checkpoints, samplers, schedulers }
                }
                Err(e) => {
                    log.error(format!("connect failed: {e}"));
                    Msg::ConnectError(e)
                }
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Queue a generation. `current` is the last result's encoded bytes, used as the img2img
    /// source when the mode is "current result".
    pub fn generate(&mut self, params: Params, current: Option<Vec<u8>>) {
        let Some(client) = self.client.clone() else {
            let _ = self.tx.send(Msg::GenError("Not connected".into()));
            return;
        };
        self.log.info(format!(
            "generate: {:?} ckpt={} {}x{} steps={} cfg={} {}/{} seed={} denoise={}",
            params.mode,
            params.checkpoint,
            params.width,
            params.height,
            params.steps,
            params.cfg,
            params.sampler,
            params.scheduler,
            params.seed,
            params.denoise
        ));
        let (tx, ctx, log) = (self.tx.clone(), self.ctx.clone(), self.log.clone());
        self.job = Some(self.rt.spawn(async move {
            run_generate(client, params, current, tx, ctx, log).await;
        }));
    }

    /// Abort the running generation locally (the server may keep finishing its current prompt).
    pub fn cancel(&mut self) {
        if let Some(h) = self.job.take() {
            h.abort();
        }
        self.log.warn("generation cancelled locally");
        let _ = self.tx.send(Msg::Cancelled);
        self.ctx.request_repaint();
    }
}

/// GET `/object_info` raw and parse leniently, logging status/content-type/size so failures are
/// diagnosable (rucomfyui's own path parses the body without ever reporting the HTTP status).
async fn fetch_object_info(
    http: &reqwest::Client,
    base: &str,
    log: &Logger,
) -> Result<SchemaSet, String> {
    let url = format!("{base}/object_info");
    log.info(format!("GET {url}"));
    let resp = http.get(&url).send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?")
        .to_string();
    let body = resp.text().await.map_err(|e| format!("reading body failed: {e}"))?;
    log.info(format!("-> {status} [{ctype}] {} bytes", body.len()));
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", head(&body, 300)));
    }
    let value: Value = serde_json::from_str(&body).map_err(|e| {
        log.error(format!("body head: {}", head(&body, 400)));
        format!("response is not JSON: {e}")
    })?;
    let set = schema::parse(&value);
    log.info(format!("parsed {} node types ({} skipped)", set.nodes.len(), set.skipped.len()));
    for (name, reason) in set.skipped.iter().take(20) {
        log.warn(format!("skipped node {name}: {reason}"));
    }
    if set.nodes.is_empty() {
        return Err("object_info contained no parsable node types".into());
    }
    Ok(set)
}

async fn run_generate(
    client: Client,
    params: Params,
    current: Option<Vec<u8>>,
    tx: Sender<Msg>,
    ctx: egui::Context,
    log: Logger,
) {
    // Send a message and wake the UI.
    macro_rules! send {
        ($m:expr) => {{
            let _ = tx.send($m);
            ctx.request_repaint();
        }};
    }

    // Resolve and upload the img2img input, if any.
    let input_image = if params.mode == Mode::Img2Img {
        let bytes = match params.img2img_source {
            Img2ImgSource::CurrentOutput => current,
            Img2ImgSource::Url => match fetch_bytes(&params.input_url, &log).await {
                Ok(b) => Some(b),
                Err(e) => {
                    log.error(format!("img2img input fetch failed: {e}"));
                    send!(Msg::GenError(format!("Fetch input failed: {e}")));
                    return;
                }
            },
        };
        let Some(bytes) = bytes else {
            send!(Msg::GenError("No input image for img2img".into()));
            return;
        };
        let name = "comfyui_android_input.png";
        log.info(format!("uploading img2img input ({} bytes)", bytes.len()));
        if let Err(e) = client
            .upload_image(name, bytes, rucomfyui::upload::UploadType::Input, true)
            .await
        {
            log.error(format!("upload failed: {e}"));
            send!(Msg::GenError(format!("Upload failed: {e}")));
            return;
        }
        Some(name.to_string())
    } else {
        None
    };

    let (wf, _out) = workflow::build(&params, input_image);
    let mut execution = match client.execute(&wf).await {
        Ok(e) => e,
        Err(e) => {
            log.error(format!("queueing workflow failed: {e}"));
            send!(Msg::GenError(e.to_string()));
            return;
        }
    };
    log.info(format!("queued prompt {}", execution.prompt_id()));
    send!(Msg::Queued);

    while let Some(event) = execution.next().await {
        match event {
            Ok(Event::Status { queue_remaining }) => {
                send!(Msg::Status(format!("Queue: {queue_remaining} ahead")))
            }
            Ok(Event::ExecutionStart { .. }) => {
                log.info("execution started");
                send!(Msg::Status("Started".into()))
            }
            Ok(Event::Progress { value, max, .. }) => {
                send!(Msg::Progress { value: value as u32, max: max as u32 })
            }
            Ok(Event::Preview { image, .. }) => {
                if let Some(ci) = decode(&image.data) {
                    send!(Msg::Preview(ci));
                }
            }
            Ok(Event::Executed { output, .. }) => {
                log.info(format!("executed: {} image(s)", output.images.len()));
                for bytes in output.images {
                    if let Some(ci) = decode(&bytes) {
                        send!(Msg::Result { image: ci, bytes });
                    }
                }
            }
            Ok(Event::Error { message, .. }) => {
                log.error(format!("server error: {message}"));
                send!(Msg::GenError(message));
                return;
            }
            Ok(Event::Completed { .. }) => break,
            Ok(_) => {}
            Err(e) => {
                log.error(format!("execution stream error: {e}"));
                send!(Msg::GenError(e.to_string()));
                return;
            }
        }
    }
    log.info("generation done");
    send!(Msg::Done);
}

/// Fetch raw bytes for an img2img input URL (http; https needs the `tls` feature).
async fn fetch_bytes(url: &str, log: &Logger) -> Result<Vec<u8>, String> {
    log.info(format!("GET {url}"));
    let client = tls_builder().build().map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

/// Decode encoded image bytes (PNG/JPEG) into an egui image for display.
fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        rgba.as_raw(),
    ))
}

/// Attach the API key to a reqwest builder as both `X-Api-Key` and `Authorization: Bearer`
/// default headers; they ride every HTTP call (object_info, queue, upload, history, view).
fn apply_key(builder: reqwest::ClientBuilder, api_key: &str) -> reqwest::ClientBuilder {
    let key = api_key.trim();
    if key.is_empty() {
        return builder;
    }
    use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    if let Ok(mut v) = HeaderValue::from_str(key) {
        v.set_sensitive(true);
        headers.insert("x-api-key", v);
    }
    if let Ok(mut v) = HeaderValue::from_str(&format!("Bearer {key}")) {
        v.set_sensitive(true);
        headers.insert(AUTHORIZATION, v);
    }
    builder.default_headers(headers)
}

/// A reqwest builder configured for TLS. With the `tls` feature it preloads a rustls config using
/// the bundled webpki-roots CA set (ring provider) — no Android platform trust store, no JNI, so it
/// can't hit the rustls-platform-verifier "not initialized" panic. Without the feature, https is
/// unsupported (http on LAN / Tailscale only).
#[cfg(feature = "tls")]
fn tls_builder() -> reqwest::ClientBuilder {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder().use_preconfigured_tls(config)
}

#[cfg(not(feature = "tls"))]
fn tls_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
}

/// Trim, drop a trailing slash, and default to http:// when no scheme is given.
fn normalize_url(raw: &str) -> String {
    let s = raw.trim().trim_end_matches('/');
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

/// First `max` chars with newlines collapsed, for one-line error/log context.
fn head(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}
