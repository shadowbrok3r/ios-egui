//! Async ComfyUI engine. A tokio runtime owns all networking; results flow back to the UI thread
//! over an mpsc channel. [`Host`] is main-thread only, so the worker never touches it — it wakes
//! the UI with a cloned [`egui::Context`] and the UI applies effects (haptics, notifications).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use rucomfyui::workflow::WorkflowNodeId;
use rucomfyui::{Client, Event, Workflow};
use serde_json::Value;

use crate::logger::Logger;
use crate::schema::{self, SchemaSet};
use crate::types::{
    Album, AlbumList, CheckpointCatalog, Facets, GalleryPage, GalleryView, Img2ImgSource,
    LoraCatalog, Mode, Params,
};
use crate::{uiwf, workflow};

/// The prompt id currently executing, shared with the websocket listener so it can filter
/// broadcast events down to our run.
type CurrentPrompt = Arc<Mutex<Option<String>>>;

/// Filename every img2img input is uploaded under. The LoadImage node cannot be built with the
/// real reference until the upload returns (the server may namespace it into a subfolder), so a
/// preview of the graph uses this bare name as a stand-in.
pub const INPUT_IMAGE_NAME: &str = "comfyui_android_input.png";

/// Catalogs the workflow builder needs. The UI owns both, so a generation carries them across.
#[derive(Clone)]
pub struct GenCtx {
    pub apps: Arc<crate::apps::AppSet>,
    pub schemas: Arc<SchemaSet>,
}

/// Every option list the Create tab's pickers offer, read off `/object_info` on connect.
#[derive(Clone, Default)]
pub struct ModelLists {
    pub checkpoints: Vec<String>,
    /// `models/diffusion_models` + `models/unet` — the Anima/Flux/Qwen-Image family.
    pub unets: Vec<String>,
    pub clips: Vec<String>,
    pub vaes: Vec<String>,
    pub clip_types: Vec<String>,
    pub clip_devices: Vec<String>,
    pub weight_dtypes: Vec<String>,
    pub samplers: Vec<String>,
    pub schedulers: Vec<String>,
}

/// A message from the async worker to the UI thread.
pub enum Msg {
    Connected {
        schemas: Arc<SchemaSet>,
        models: Box<ModelLists>,
    },
    ConnectError(String),
    /// Enhance-chain steps that were skipped or inputs dropped while building the prompt.
    EnhanceNote(String),
    Queued,
    Progress { value: u32, max: u32 },
    Status(String),
    /// Server-wide queue depth from the WS `status` broadcast (includes jobs from other clients).
    QueueRemaining(u32),
    Preview(egui::ColorImage),
    Result { image: egui::ColorImage, bytes: Vec<u8> },
    /// A node started executing (`None` = prompt finished). WebSocket transport only today.
    NodeExecuting(Option<u32>),
    /// A node finished and produced images (raw encoded bytes, for graph-node display).
    NodeExecuted { node: u32, images: Vec<Vec<u8>> },
    Done,
    Cancelled,
    GenError(String),
    /// Server-side workflow file names (`/userdata?dir=workflows`).
    Workflows(Vec<String>),
    /// A workflow fetched and converted to API format, ready for the graph editor.
    WorkflowLoaded { name: String, workflow: Box<Workflow>, warnings: Vec<String> },
    /// A workflow file written to the server.
    WorkflowSaved(String),
    WorkflowError(String),
    /// One page of the gallery listing; `generation` echoes the query generation it answers.
    Gallery { generation: u64, page: GalleryPage },
    GalleryError(String),
    /// A decoded gallery thumbnail; `key` is `subfolder/filename#size`.
    Thumb { key: String, image: egui::ColorImage },
    /// A decoded full-resolution gallery image with its raw bytes.
    FullImage { key: String, image: egui::ColorImage, bytes: Vec<u8> },
    /// A downloaded video's raw bytes (no decode — for the poster viewer + Save).
    VideoReady { key: String, bytes: Vec<u8> },
    /// One downloaded file to save to the device gallery (batch "Save all"); `name` is the filename.
    SaveToGallery { name: String, bytes: Vec<u8> },
    /// A `POST /login` succeeded; `session` is the `cg_session` cookie token to send from now on.
    SignedIn { username: String, session: String },
    SignedOut,
    AuthError(String),
    /// The account's albums (`GET /gallery/api/albums`).
    Albums(Vec<Album>),
    /// Distinct model names across the account's gallery (`GET /gallery/api/facets`).
    Facets(Facets),
    /// An album mutation finished; the note is for the status line and the UI re-lists albums.
    AlbumChanged(String),
    AlbumError(String),
    /// A gallery mutation (delete) finished; the UI clears its selection and reloads the listing.
    GalleryMutated(String),
    /// Which albums one image belongs to (`GET /gallery/api/meta`); `key` is `subfolder/filename`.
    ItemAlbums { key: String, albums: Vec<i64> },
    /// Raw embedded workflow JSON for a gallery image (`GET /gallery/api/workflow`).
    ItemWorkflow { key: String, json: String },
    /// Fetching the embedded workflow failed (image may still have `has_workflow: false` scrapes).
    ItemWorkflowError { key: String, error: String },
    /// A device-gallery image was uploaded to the server as a LoadImage input; `image_ref` is the
    /// `subfolder/name` (or bare name) to select on the node. `token` correlates the result to the
    /// specific pick so a slow upload lands on the node it was chosen for.
    InputUploaded { token: u64, image_ref: String },
    /// Uploading a device-gallery image to the server failed; `token` identifies the pick.
    InputUploadError { token: u64, error: String },
    /// Server LoRA catalog (`GET /comfyui-android/lora-catalog.json`).
    LoraCatalog(LoraCatalog),
    /// Catalog missing or invalid — Create LoRAs fall back to installed names only.
    LoraCatalogError(String),
    /// Server checkpoint catalog (`GET /checkpoint-catalog.json`).
    CheckpointCatalog(CheckpointCatalog),
    CheckpointCatalogError(String),
    /// Decoded preview for Create img2img "From URL" (or an error string).
    Img2ImgUrlPreview {
        url: String,
        image: Option<egui::ColorImage>,
        error: Option<String>,
    },
}

pub struct Engine {
    rt: tokio::runtime::Runtime,
    ctx: egui::Context,
    log: Logger,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    client: Option<Client>,
    http: Option<reqwest::Client>,
    base: String,
    /// In-flight generate / graph-run tasks (more than one when Create Queue is used).
    jobs: Vec<tokio::task::JoinHandle<()>>,
    /// How many generate/graph jobs have not finished yet (UI uses this for multi-queue).
    inflight: Arc<AtomicUsize>,
    ws_task: Option<tokio::task::JoinHandle<()>>,
    current_prompt: CurrentPrompt,
}

impl Engine {
    pub fn new(ctx: egui::Context, log: Logger) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build tokio runtime");
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            rt,
            ctx,
            log,
            tx,
            rx,
            client: None,
            http: None,
            base: String::new(),
            jobs: Vec::new(),
            inflight: Arc::new(AtomicUsize::new(0)),
            ws_task: None,
            current_prompt: Arc::new(Mutex::new(None)),
        }
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
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

    /// The authenticated `/view` URL for an output image (also usable as an img2img input URL).
    pub fn view_url(&self, subfolder: &str, filename: &str) -> Option<String> {
        let mut u = reqwest::Url::parse(&format!("{}/view", self.base)).ok()?;
        u.query_pairs_mut()
            .append_pair("type", "output")
            .append_pair("subfolder", subfolder)
            .append_pair("filename", filename);
        Some(u.to_string())
    }

    /// Point the client at `url` (with an optional API key), fetch `/object_info` raw, and parse
    /// it leniently into a [`SchemaSet`] (rucomfyui's typed parse fails whole-catalog on servers
    /// with slightly nonconforming custom nodes).
    pub fn connect(&mut self, url: String, api_key: String, session: String) {
        let base = normalize_url(&url);
        let log = self.log.clone();
        let key_note = if api_key.trim().is_empty() { "no API key" } else { "with API key" };
        let sess_note = if session.trim().is_empty() { "" } else { " + signed-in session" };
        log.info(format!("connect: {base} ({key_note}{sess_note})"));
        let http = match apply_auth(tls_builder(), &api_key, &session).build() {
            Ok(c) => c,
            Err(e) => {
                log.error(format!("HTTP client build failed: {e}"));
                let _ = self.tx.send(Msg::ConnectError(e.to_string()));
                self.ctx.request_repaint();
                return;
            }
        };
        let client = Client::new_with_client(base.clone(), http.clone());
        // The ws MUST use the same clientId the client queues prompts with — ComfyUI routes
        // executing/progress events only to the socket whose clientId matches the prompt's, so a
        // separately-generated id would silently receive nothing.
        let client_id = client.client_id().to_string();
        self.client = Some(client);
        self.http = Some(http.clone());
        self.base = base.clone();

        // Live progress listener: our own authenticated /ws connection (headers on the
        // handshake), independent of the polling execution transport.
        if let Some(task) = self.ws_task.take() {
            task.abort();
        }
        self.ws_task = Some(self.rt.spawn(ws_listener(
            base.clone(),
            api_key.clone(),
            session.clone(),
            client_id,
            self.tx.clone(),
            self.ctx.clone(),
            self.log.clone(),
            self.current_prompt.clone(),
        )));

        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let msg = match fetch_object_info(&http, &base, &log).await {
                Ok(schemas) => {
                    let models = ModelLists {
                        checkpoints: schemas.checkpoints(),
                        unets: schemas.unets(),
                        clips: schemas.clips(),
                        vaes: schemas.vaes(),
                        clip_types: schemas.clip_types(),
                        clip_devices: schemas.clip_devices(),
                        weight_dtypes: schemas.weight_dtypes(),
                        samplers: schemas.samplers(),
                        schedulers: schemas.schedulers(),
                    };
                    log.info(format!(
                        "options: {} checkpoints, {} diffusion models, {} clips, {} vaes, {} samplers, {} schedulers",
                        models.checkpoints.len(),
                        models.unets.len(),
                        models.clips.len(),
                        models.vaes.len(),
                        models.samplers.len(),
                        models.schedulers.len()
                    ));
                    if models.checkpoints.is_empty() && models.unets.is_empty() {
                        log.warn("no models found in any *CheckpointLoader* or UNETLoader node");
                    }
                    Msg::Connected { schemas: Arc::new(schemas), models: Box::new(models) }
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

    /// Queue a generation from the simple Generate tab. `current` is the last result's encoded
    /// bytes, used as the img2img source when the mode is "current result".
    /// `ui_workflow` is the UI-format JSON to embed in the PNG via `extra_pnginfo`.
    /// Does not cancel other in-flight jobs — use [`Self::cancel`] for that.
    pub fn generate(
        &mut self,
        params: Params,
        current: Option<Vec<u8>>,
        gcx: GenCtx,
        ui_workflow: Option<Value>,
    ) {
        let Some(client) = self.client.clone() else {
            let _ = self.tx.send(Msg::GenError("Not connected".into()));
            return;
        };
        self.log.info(format!(
            "generate: {:?} {:?}={} clips={} vae={} {}x{} batch={} steps={} cfg={} {}/{} seed={} denoise={} loras={} apps={}",
            params.mode,
            params.model_kind,
            params.model_file(),
            params.active_clips().join("+"),
            params.vae_name,
            params.width,
            params.height,
            params.batch_size,
            params.steps,
            params.cfg,
            params.sampler,
            params.scheduler,
            params.seed,
            params.denoise,
            params.loras.len(),
            params.apps.iter().filter(|a| a.enabled).count()
        ));
        let (tx, ctx, log) = (self.tx.clone(), self.ctx.clone(), self.log.clone());
        let authed = self.http.clone().map(|h| (self.base.clone(), h));
        let current_prompt = self.current_prompt.clone();
        let inflight = self.inflight.clone();
        inflight.fetch_add(1, Ordering::SeqCst);
        self.reap_jobs();
        self.jobs.push(self.rt.spawn(async move {
            run_generate(client, params, current, gcx, ui_workflow, current_prompt, authed, tx, ctx, log).await;
            inflight.fetch_sub(1, Ordering::SeqCst);
        }));
    }

    /// Queue an arbitrary API-format workflow (from the graph editor).
    /// `ui_workflow` is the UI-format JSON to embed in the PNG via `extra_pnginfo`.
    pub fn run_workflow(&mut self, wf: Workflow, ui_workflow: Option<Value>) {
        let Some(client) = self.client.clone() else {
            let _ = self.tx.send(Msg::GenError("Not connected".into()));
            return;
        };
        self.log.info(format!("queue graph workflow: {} nodes", wf.0.len()));
        let (tx, ctx, log) = (self.tx.clone(), self.ctx.clone(), self.log.clone());
        let current = self.current_prompt.clone();
        let inflight = self.inflight.clone();
        inflight.fetch_add(1, Ordering::SeqCst);
        self.reap_jobs();
        self.jobs.push(self.rt.spawn(async move {
            stream_execution(client, wf, ui_workflow, tx, ctx, log, current).await;
            inflight.fetch_sub(1, Ordering::SeqCst);
        }));
    }

    /// Abort all local generate/graph jobs (the server may keep finishing queued prompts).
    pub fn cancel(&mut self) {
        for h in self.jobs.drain(..) {
            h.abort();
        }
        self.inflight.store(0, Ordering::SeqCst);
        *self.current_prompt.lock().unwrap() = None;
        self.log.warn("generation cancelled locally");
        let _ = self.tx.send(Msg::Cancelled);
        self.ctx.request_repaint();
    }

    fn reap_jobs(&mut self) {
        self.jobs.retain(|h| !h.is_finished());
    }

    /// Snapshot the server queue (`GET /queue`) so the UI can show jobs started elsewhere.
    pub fn poll_queue(&self) {
        let Some((http, url)) = self.authed_url("/queue", &[]) else { return };
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let Ok(resp) = http.get(url).send().await else { return };
            let Ok(body) = resp.text().await else { return };
            let Ok(v) = serde_json::from_str::<Value>(&body) else { return };
            let running = v.get("queue_running").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            let pending = v.get("queue_pending").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            let remaining = (running + pending) as u32;
            let _ = tx.send(Msg::QueueRemaining(remaining));
            ctx.request_repaint();
        });
    }

    /// Download and decode an img2img input URL for the Create-tab thumbnail.
    pub fn fetch_img2img_url_preview(&self, url: String) {
        let (tx, ctx, log) = self.emitters();
        let authed = self.http.clone().map(|h| (self.base.clone(), h));
        self.rt.spawn(async move {
            let msg = match fetch_bytes(&url, &authed, &log).await {
                Ok(bytes) => match decode(&bytes) {
                    Some(image) => Msg::Img2ImgUrlPreview {
                        url,
                        image: Some(image),
                        error: None,
                    },
                    None => Msg::Img2ImgUrlPreview {
                        url,
                        image: None,
                        error: Some("Could not decode image".into()),
                    },
                },
                Err(e) => Msg::Img2ImgUrlPreview {
                    url,
                    image: None,
                    error: Some(e),
                },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Fetch the Create-tab LoRA catalog. Tries `/comfyui-android/lora-catalog.json`, then
    /// `/lora-catalog.json`. Soft-fails so generation still works without it.
    pub fn fetch_lora_catalog(&self) {
        let Some(http) = self.http.clone() else { return };
        let base = self.base.clone();
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let paths = [
                "/comfyui-android/lora-catalog.json",
                "/lora-catalog.json",
            ];
            for path in paths {
                let Ok(url) = reqwest::Url::parse(&format!("{base}{path}")) else {
                    continue;
                };
                match get_ok_text(&http, url, &log).await {
                    Ok(body) => match serde_json::from_str::<LoraCatalog>(&body) {
                        Ok(catalog) => {
                            log.info(format!(
                                "lora catalog: {} entries (from {path})",
                                catalog.loras.len()
                            ));
                            let _ = tx.send(Msg::LoraCatalog(catalog));
                            ctx.request_repaint();
                            return;
                        }
                        Err(e) => {
                            log.warn(format!("lora catalog {path}: parse error: {e}"));
                            let _ = tx.send(Msg::LoraCatalogError(format!("parse error: {e}")));
                            ctx.request_repaint();
                            return;
                        }
                    },
                    Err(_) => continue,
                }
            }
            log.warn("lora catalog: not found");
            let _ = tx.send(Msg::LoraCatalogError("catalog not found".into()));
            ctx.request_repaint();
        });
    }

    /// Fetch checkpoint metadata (`/checkpoint-catalog.json`, then android-prefixed path).
    pub fn fetch_checkpoint_catalog(&self) {
        let Some(http) = self.http.clone() else { return };
        let base = self.base.clone();
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let paths = [
                "/checkpoint-catalog.json",
                "/comfyui-android/checkpoint-catalog.json",
            ];
            for path in paths {
                let Ok(url) = reqwest::Url::parse(&format!("{base}{path}")) else {
                    continue;
                };
                match get_ok_text(&http, url, &log).await {
                    Ok(body) => match serde_json::from_str::<CheckpointCatalog>(&body) {
                        Ok(catalog) => {
                            log.info(format!(
                                "checkpoint catalog: {} entries (from {path})",
                                catalog.checkpoints.len()
                            ));
                            let _ = tx.send(Msg::CheckpointCatalog(catalog));
                            ctx.request_repaint();
                            return;
                        }
                        Err(e) => {
                            log.warn(format!("checkpoint catalog {path}: parse error: {e}"));
                            let _ =
                                tx.send(Msg::CheckpointCatalogError(format!("parse error: {e}")));
                            ctx.request_repaint();
                            return;
                        }
                    },
                    Err(_) => continue,
                }
            }
            log.warn("checkpoint catalog: not found");
            let _ = tx.send(Msg::CheckpointCatalogError("catalog not found".into()));
            ctx.request_repaint();
        });
    }

    /// List server-side workflow files (`/userdata?dir=workflows`, `.json` only).
    pub fn list_workflows(&self) {
        let Some((http, url)) = self.authed_url("/userdata", &[("dir", "workflows"), ("recurse", "true")]) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<Vec<String>>(&body) {
                    Ok(names) => {
                        let mut names: Vec<String> =
                            names.into_iter().filter(|n| n.ends_with(".json")).collect();
                        names.sort_by_key(|n| n.to_lowercase());
                        log.info(format!("{} workflow files", names.len()));
                        Msg::Workflows(names)
                    }
                    Err(e) => Msg::WorkflowError(format!("workflow list is not a name array: {e}")),
                },
                Err(e) => Msg::WorkflowError(e),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// `{base}/userdata/workflows%2F{name}` — the workflow path rides in one percent-encoded
    /// segment, matching the web frontend.
    fn workflow_url(&self, name: &str) -> Option<reqwest::Url> {
        let mut url = reqwest::Url::parse(&format!("{}/userdata", self.base)).ok()?;
        url.path_segments_mut().ok()?.push(&format!("workflows/{name}"));
        Some(url)
    }

    /// Fetch a server workflow file and convert it for the graph editor.
    pub fn open_workflow(&self, name: String, schemas: Arc<SchemaSet>) {
        let Some(http) = self.http.clone() else { return };
        let Some(url) = self.workflow_url(&name) else { return };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => workflow_msg(&name, &body, &schemas, &log),
                Err(e) => Msg::WorkflowError(e),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Write a UI-format workflow file to the server (`POST /userdata`, overwriting).
    pub fn save_workflow(&self, name: String, body: String) {
        let Some(http) = self.http.clone() else { return };
        let Some(mut url) = self.workflow_url(&name) else { return };
        url.query_pairs_mut().append_pair("overwrite", "true");
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            log.info(format!("POST {url} ({} bytes)", body.len()));
            let resp = http
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await;
            let msg = match resp {
                Ok(resp) if resp.status().is_success() => {
                    log.info(format!("saved workflow {name}"));
                    Msg::WorkflowSaved(name)
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    log.error(format!("save failed: HTTP {status}: {}", head(&body, 200)));
                    Msg::WorkflowError(format!("save failed: HTTP {status}"))
                }
                Err(e) => {
                    log.error(format!("save failed: {e}"));
                    Msg::WorkflowError(format!("save failed: {e}"))
                }
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Fetch the workflow embedded in a gallery image and convert it for the graph editor.
    pub fn open_gallery_workflow(&self, subfolder: String, filename: String, schemas: Arc<SchemaSet>) {
        let Some((http, url)) = self.authed_url(
            "/gallery/api/workflow",
            &[("subfolder", &subfolder), ("filename", &filename)],
        ) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => workflow_msg(&filename, &body, &schemas, &log),
                Err(e) => Msg::WorkflowError(e),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Fetch the raw embedded workflow JSON for the viewer's metadata panel / copy button.
    pub fn fetch_item_workflow(&self, subfolder: String, filename: String) {
        let Some((http, url)) = self.authed_url(
            "/gallery/api/workflow",
            &[("subfolder", &subfolder), ("filename", &filename)],
        ) else {
            return;
        };
        let key = format!("{subfolder}/{filename}");
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(json) => Msg::ItemWorkflow { key, json },
                Err(e) => Msg::ItemWorkflowError { key, error: e },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Convert a workflow JSON string (clipboard / gallery copy) for the graph editor.
    pub fn load_workflow_json(&self, name: String, body: String, schemas: Arc<SchemaSet>) {
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = workflow_msg(&name, &body, &schemas, &log);
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Sign in to comfy-gate with a user account (`POST /login`, an HTML form flow). Redirects are
    /// disabled deliberately: the gate answers both a good and a bad password with a 303, and only
    /// the `cg_session` cookie distinguishes them — following the redirect would just fetch a page.
    ///
    /// Takes the URL explicitly so signing in works before (or instead of) a successful connect.
    pub fn sign_in(&self, url: String, username: String, password: String) {
        let base = normalize_url(&url);
        let (tx, ctx, log) = self.emitters();
        let builder = tls_builder().redirect(reqwest::redirect::Policy::none());
        let http = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Msg::AuthError(format!("HTTP client build failed: {e}")));
                ctx.request_repaint();
                return;
            }
        };
        self.rt.spawn(async move {
            let endpoint = format!("{base}/login");
            log.info(format!("POST {endpoint} (sign in as {username})"));
            let resp = http
                .post(&endpoint)
                .form(&[("username", username.as_str()), ("password", password.as_str())])
                .send()
                .await;
            let msg = match resp {
                Ok(resp) => {
                    let status = resp.status();
                    let cookies: Vec<String> = resp
                        .headers()
                        .get_all(reqwest::header::SET_COOKIE)
                        .iter()
                        .filter_map(|v| v.to_str().ok().map(str::to_string))
                        .collect();
                    log.info(format!("-> {status}, {} cookie(s)", cookies.len()));
                    match session_from_set_cookie(cookies.iter().map(String::as_str)) {
                        Some(session) => {
                            log.info(format!("signed in as {username}"));
                            Msg::SignedIn { username, session }
                        }
                        None if status.as_u16() == 429 => {
                            Msg::AuthError("Too many attempts — try again in a few minutes".into())
                        }
                        None if status.is_redirection() || status.is_success() => {
                            Msg::AuthError("Wrong username or password".into())
                        }
                        None => Msg::AuthError(format!("Sign in failed: HTTP {status}")),
                    }
                }
                Err(e) => {
                    log.error(format!("sign in failed: {e}"));
                    Msg::AuthError(format!("Sign in failed: {e}"))
                }
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// End the server-side session (`POST /logout`). Any API key keeps working — it is a separate
    /// credential the gate never revokes here.
    pub fn sign_out(&self, url: String, session: String) {
        let base = normalize_url(&url);
        let (tx, ctx, log) = self.emitters();
        let http = apply_auth(tls_builder().redirect(reqwest::redirect::Policy::none()), "", &session)
            .build();
        self.rt.spawn(async move {
            if let Ok(http) = http {
                let endpoint = format!("{base}/logout");
                log.info(format!("POST {endpoint} (sign out)"));
                match http.post(&endpoint).send().await {
                    Ok(r) => log.info(format!("-> {}", r.status())),
                    Err(e) => log.warn(format!("sign out: {e}")),
                }
            }
            let _ = tx.send(Msg::SignedOut);
            ctx.request_repaint();
        });
    }

    /// The account's albums.
    pub fn albums(&self) {
        let Some((http, url)) = self.authed_url("/gallery/api/albums", &[]) else { return };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<AlbumList>(&body) {
                    Ok(list) => Msg::Albums(list.albums),
                    Err(e) => Msg::AlbumError(format!("album list decode: {e}")),
                },
                Err(e) => Msg::AlbumError(e),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Distinct model names across the account's gallery, for the model filter.
    pub fn facets(&self) {
        let Some((http, url)) = self.authed_url("/gallery/api/facets", &[]) else { return };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            // A cold server reindexes here, so this can be slow; a failure just leaves the filter
            // empty rather than blocking the gallery.
            match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<Facets>(&body) {
                    Ok(f) => {
                        log.info(format!("facets: {} distinct models", f.models.len()));
                        let _ = tx.send(Msg::Facets(f));
                    }
                    Err(e) => log.warn(format!("facets decode: {e}")),
                },
                Err(e) => log.warn(format!("facets: {e}")),
            }
            ctx.request_repaint();
        });
    }

    /// Create an album.
    pub fn album_create(&self, name: String) {
        let Some((http, url)) = self.authed_url("/gallery/api/albums", &[]) else { return };
        self.album_post(http, url, serde_json::json!({ "name": name }), format!("Created {name}"));
    }

    pub fn album_rename(&self, id: i64, name: String) {
        let Some((http, url)) = self.authed_url(&format!("/gallery/api/albums/{id}/rename"), &[])
        else {
            return;
        };
        self.album_post(http, url, serde_json::json!({ "name": name }), format!("Renamed to {name}"));
    }

    pub fn album_delete(&self, id: i64, name: String) {
        let Some((http, url)) = self.authed_url(&format!("/gallery/api/albums/{id}/delete"), &[])
        else {
            return;
        };
        self.album_post(http, url, serde_json::json!({}), format!("Deleted {name}"));
    }

    /// Add images to an album. Items are identified by their `(subfolder, filename)` pair exactly
    /// as the gallery listing returned them — the server has no image id, and it silently ignores
    /// pairs it can't match to the caller's own files.
    pub fn album_add(&self, id: i64, items: Vec<(String, String)>) {
        let Some((http, url)) = self.authed_url(&format!("/gallery/api/albums/{id}/add"), &[])
        else {
            return;
        };
        let n = items.len();
        let note = if n == 1 { "Added to album".to_string() } else { format!("Added {n} to album") };
        self.album_post(http, url, items_body(items), note);
    }

    pub fn album_remove(&self, id: i64, items: Vec<(String, String)>) {
        let Some((http, url)) = self.authed_url(&format!("/gallery/api/albums/{id}/remove"), &[])
        else {
            return;
        };
        self.album_post(http, url, items_body(items), "Removed from album".to_string());
    }

    /// POST an album mutation and report the outcome. The count in the reply matters: `add` filters
    /// out items it doesn't recognise instead of erroring, so a 200 with `added: 0` means nothing
    /// landed.
    fn album_post(&self, http: reqwest::Client, url: reqwest::Url, body: Value, note: String) {
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            log.info(format!("POST {url}"));
            let resp = http.post(url).json(&body).send().await;
            let msg = match resp {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    log.info(format!("-> {}", head(&text, 120)));
                    match serde_json::from_str::<Value>(&text) {
                        Ok(v) if v.get("added").and_then(Value::as_u64) == Some(0) => {
                            Msg::AlbumError("Nothing was added — the server didn't match those images".into())
                        }
                        _ => Msg::AlbumChanged(note),
                    }
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    log.error(format!("album op failed: HTTP {status}: {}", head(&body, 200)));
                    // The gate reports errors as plain text, so surface the body, not a bare code.
                    let detail = head(&body, 120);
                    Msg::AlbumError(if detail.is_empty() {
                        format!("HTTP {status}")
                    } else {
                        detail
                    })
                }
                Err(e) => {
                    log.error(format!("album op failed: {e}"));
                    Msg::AlbumError(e.to_string())
                }
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Soft-delete images (comfy-gate moves them to `<ns>/.trash/`; recoverable, not a hard unlink).
    /// Identified by `(subfolder, filename)` pairs, same as albums.
    pub fn delete_images(&self, items: Vec<(String, String)>) {
        let Some((http, url)) = self.authed_url("/gallery/api/delete", &[]) else { return };
        let n = items.len();
        let sample = items
            .first()
            .map(|(sf, f)| format!("{sf}/{f}"))
            .unwrap_or_default();
        let body = items_body(items);
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            log.info(format!("POST {url} (delete {n}; e.g. {sample})"));
            let msg = match http.post(url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    log.info(format!("-> {}", head(&text, 300)));
                    match serde_json::from_str::<Value>(&text) {
                        Ok(v) => {
                            let trashed = v.get("trashed").and_then(Value::as_u64).unwrap_or(0);
                            let cleared = v.get("cleared").and_then(Value::as_u64).unwrap_or(0);
                            let errors: Vec<String> = v
                                .get("errors")
                                .and_then(Value::as_array)
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|e| e.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let gone = trashed + cleared;
                            if gone == 0 {
                                let why = if errors.is_empty() {
                                    "server rejected every item".into()
                                } else {
                                    errors.into_iter().take(3).collect::<Vec<_>>().join("; ")
                                };
                                log.error(format!("delete: trashed 0 — {why}"));
                                Msg::AlbumError(format!("Delete failed: {why}"))
                            } else if !errors.is_empty() {
                                Msg::GalleryMutated(format!(
                                    "Moved {trashed} to trash ({cleared} already gone); {}",
                                    errors.into_iter().take(2).collect::<Vec<_>>().join("; ")
                                ))
                            } else if cleared > 0 && trashed == 0 {
                                Msg::GalleryMutated(format!(
                                    "Removed {cleared} missing item(s) from the gallery"
                                ))
                            } else {
                                Msg::GalleryMutated(format!("Moved {trashed} to trash"))
                            }
                        }
                        Err(_) => Msg::GalleryMutated(format!("Moved {n} to trash")),
                    }
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    log.error(format!("delete failed: HTTP {status}: {}", head(&body, 200)));
                    Msg::AlbumError(format!("Delete failed: HTTP {status}"))
                }
                Err(e) => {
                    log.error(format!("delete failed: {e}"));
                    Msg::AlbumError(format!("Delete failed: {e}"))
                }
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Which albums one image is in — the only endpoint that reports membership.
    ///
    /// Also forwards any embedded workflow / prompt JSON the meta payload may carry (some gate
    /// builds put the graph here instead of exposing `/gallery/api/workflow`).
    pub fn fetch_item_albums(&self, subfolder: String, filename: String) {
        let Some((http, url)) = self.authed_url(
            "/gallery/api/meta",
            &[("subfolder", &subfolder), ("filename", &filename)],
        ) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            if let Ok(body) = get_ok_text(&http, url, &log).await
                && let Ok(v) = serde_json::from_str::<Value>(&body)
            {
                let key = format!("{subfolder}/{filename}");
                let albums = v
                    .get("albums")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|x| x.get("id").and_then(Value::as_i64)).collect())
                    .unwrap_or_default();
                let _ = tx.send(Msg::ItemAlbums { key: key.clone(), albums });
                // Prefer an embedded graph on the meta payload when present.
                let embedded = v
                    .get("workflow")
                    .or_else(|| v.get("prompt"))
                    .or_else(|| v.get("graph"));
                if let Some(graph) = embedded {
                    let json = match graph {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if json.contains("class_type") || json.contains("\"nodes\"") {
                        log.info(format!("meta {key}: embedded workflow ({} bytes)", json.len()));
                        let _ = tx.send(Msg::ItemWorkflow { key, json });
                    }
                }
                ctx.request_repaint();
            }
        });
    }

    /// Fetch one page of the server's image gallery with the view's search, model filter, album,
    /// sort and grouping applied server-side.
    /// `generation` is echoed back in [`Msg::Gallery`] so the UI can discard pages from a query that a
    /// filter change has since superseded (auto-load chains keep several requests in flight).
    pub fn gallery_list(&self, generation: u64, offset: u64, limit: u64, q: &str, view: &GalleryView) {
        let (offset_s, limit_s) = (offset.to_string(), limit.to_string());
        let mut query = vec![
            ("offset", offset_s.as_str()),
            ("limit", limit_s.as_str()),
            ("sort", view.sort.param()),
            ("group", view.group.param()),
        ];
        let q = q.trim();
        if !q.is_empty() {
            query.push(("q", q));
        }
        if !view.model.is_empty() {
            query.push(("model", view.model.as_str()));
        }
        let album_s;
        if let Some(id) = view.album {
            album_s = id.to_string();
            query.push(("album", album_s.as_str()));
        }
        let Some((http, url)) = self.authed_url("/gallery/api/list", &query) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<GalleryPage>(&body) {
                    Ok(mut page) => {
                        page.offset = offset;
                        log.info(format!("gallery: {} items of {}", page.items.len(), page.total));
                        Msg::Gallery { generation, page }
                    }
                    Err(e) => Msg::GalleryError(format!("gallery list decode: {e}")),
                },
                Err(e) => Msg::GalleryError(e),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Fetch and decode a gallery thumbnail at `size` (server-side downscale, clamped 64..=1024).
    pub fn fetch_thumb(&self, subfolder: String, filename: String, size: u32) {
        let size_s = size.to_string();
        let Some((http, url)) = self.authed_url(
            "/gallery/api/thumb",
            &[("subfolder", &subfolder), ("filename", &filename), ("size", &size_s)],
        ) else {
            return;
        };
        let (tx, ctx, _log) = self.emitters();
        self.rt.spawn(async move {
            if let Ok(bytes) = get_ok_bytes(&http, url).await
                && let Some(image) = decode(&bytes)
            {
                let key = format!("{subfolder}/{filename}#{size}");
                let _ = tx.send(Msg::Thumb { key, image });
                ctx.request_repaint();
            }
        });
    }

    /// Download the full files for a set of gallery items so the UI can save them to the device
    /// gallery. Each finished download arrives as its own [`Msg::SaveToGallery`].
    pub fn download_for_save(&self, items: Vec<(String, String)>) {
        for (subfolder, filename) in items {
            let Some((http, url)) = self.authed_url(
                "/view",
                &[("type", "output"), ("subfolder", &subfolder), ("filename", &filename)],
            ) else {
                return;
            };
            let (tx, ctx, log) = self.emitters();
            self.rt.spawn(async move {
                match get_ok_bytes(&http, url).await {
                    Ok(bytes) => {
                        let _ = tx.send(Msg::SaveToGallery { name: filename, bytes });
                        ctx.request_repaint();
                    }
                    Err(e) => log.warn(format!("save-all download failed for {filename}: {e}")),
                }
            });
        }
    }

    /// Download a video file's raw bytes (no image decode) for the poster viewer and Save.
    pub fn fetch_video(&self, subfolder: String, filename: String) {
        let Some((http, url)) = self.authed_url(
            "/view",
            &[("type", "output"), ("subfolder", &subfolder), ("filename", &filename)],
        ) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            match get_ok_bytes(&http, url).await {
                Ok(bytes) => {
                    let _ = tx.send(Msg::VideoReady { key: format!("{subfolder}/{filename}"), bytes });
                }
                Err(e) => {
                    log.error(format!("video download: {e}"));
                    let _ = tx.send(Msg::GalleryError(e));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Fetch a server input image (for the LoadImage thumbnail picker), decoded and cached under
    /// the key `input#<filename>`.
    pub fn fetch_input_thumb(&self, filename: String) {
        let Some((http, url)) = self.authed_url(
            "/view",
            &[("type", "input"), ("subfolder", ""), ("filename", &filename)],
        ) else {
            return;
        };
        let (tx, ctx, _log) = self.emitters();
        self.rt.spawn(async move {
            if let Ok(bytes) = get_ok_bytes(&http, url).await
                && let Some(image) = decode(&bytes)
            {
                let _ = tx.send(Msg::Thumb { key: format!("input#{filename}"), image });
                ctx.request_repaint();
            }
        });
    }

    /// Upload a locally-picked image (from the device gallery) to the server as a LoadImage input,
    /// then report the resulting `subfolder/name` reference so the node can select it. Mirrors the
    /// img2img upload path (comfy-gate namespaces uploads into a per-user subfolder).
    pub fn upload_input_image(&self, token: u64, filename: String, bytes: Vec<u8>) {
        let Some(client) = self.client.clone() else {
            let _ = self.tx.send(Msg::InputUploadError { token, error: "Not connected".into() });
            self.ctx.request_repaint();
            return;
        };
        let (tx, ctx, log) = (self.tx.clone(), self.ctx.clone(), self.log.clone());
        log.info(format!("uploading device image '{filename}' ({} bytes)", bytes.len()));
        self.rt.spawn(async move {
            match client
                .upload_image(&filename, bytes, rucomfyui::upload::UploadType::Input, true)
                .await
            {
                Ok(resp) => {
                    let image_ref = if resp.subfolder.is_empty() {
                        resp.name.clone()
                    } else {
                        format!("{}/{}", resp.subfolder, resp.name)
                    };
                    log.info(format!("uploaded device image as '{image_ref}'"));
                    let _ = tx.send(Msg::InputUploaded { token, image_ref });
                }
                Err(e) => {
                    log.error(format!("device image upload failed: {e}"));
                    let _ = tx.send(Msg::InputUploadError { token, error: format!("Upload failed: {e}") });
                }
            }
            ctx.request_repaint();
        });
    }

    /// Fetch and decode a full-resolution gallery image.
    ///
    /// When `cache_dir` is set, a prior download is served from disk immediately and the network
    /// fetch is skipped. Successful downloads are written back into that directory.
    pub fn fetch_full(&self, subfolder: String, filename: String, cache_dir: Option<String>) {
        let key = format!("{subfolder}/{filename}");
        if let Some(dir) = cache_dir.as_ref()
            && let Some(bytes) = crate::gallery::read_full_cache(dir, &key)
            && let Some(image) = decode(&bytes)
        {
            let _ = self.tx.send(Msg::FullImage { key: key.clone(), image, bytes });
            self.ctx.request_repaint();
            return;
        }
        let Some((http, url)) = self.authed_url(
            "/view",
            &[("type", "output"), ("subfolder", &subfolder), ("filename", &filename)],
        ) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            match get_ok_bytes(&http, url).await {
                Ok(bytes) => {
                    if let Some(image) = decode(&bytes) {
                        if let Some(dir) = cache_dir.as_ref() {
                            crate::gallery::write_full_cache(dir, &key, &bytes);
                        }
                        let _ = tx.send(Msg::FullImage { key, image, bytes });
                    } else {
                        let _ = tx.send(Msg::GalleryError("image decode failed".into()));
                    }
                }
                Err(e) => {
                    log.error(format!("full image: {e}"));
                    let _ = tx.send(Msg::GalleryError(e));
                }
            }
            ctx.request_repaint();
        });
    }

    fn emitters(&self) -> (Sender<Msg>, egui::Context, Logger) {
        (self.tx.clone(), self.ctx.clone(), self.log.clone())
    }

    /// The authed client plus `base + path` with query pairs; `None` (with a log line) before a
    /// connection exists.
    fn authed_url(&self, path: &str, query: &[(&str, &str)]) -> Option<(reqwest::Client, reqwest::Url)> {
        let Some(http) = self.http.clone() else {
            self.log.warn(format!("{path}: not connected"));
            return None;
        };
        let mut url = reqwest::Url::parse(&format!("{}{path}", self.base)).ok()?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        Some((http, url))
    }
}

/// Build the Loaded/Error message from a fetched workflow body: UI-format bodies convert via
/// [`uiwf`], API-format bodies parse directly.
fn workflow_msg(name: &str, body: &str, schemas: &SchemaSet, log: &Logger) -> Msg {
    let value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return Msg::WorkflowError(format!("{name}: not JSON ({e})")),
    };
    // Some endpoints wrap the workflow in a field.
    let value = value.get("workflow").cloned().unwrap_or(value);
    let converted = if value.get("nodes").is_some() {
        uiwf::convert(&value, schemas)
    } else {
        serde_json::from_value::<Workflow>(value)
            .map(|workflow| uiwf::Converted { workflow, warnings: Vec::new() })
            .map_err(|e| format!("neither UI- nor API-format workflow: {e}"))
    };
    match converted {
        Ok(c) => {
            log.info(format!(
                "workflow {name}: {} nodes, {} warnings",
                c.workflow.0.len(),
                c.warnings.len()
            ));
            for w in &c.warnings {
                log.warn(format!("{name}: {w}"));
            }
            Msg::WorkflowLoaded {
                name: name.to_string(),
                workflow: Box::new(c.workflow),
                warnings: c.warnings,
            }
        }
        Err(e) => {
            log.error(format!("workflow {name}: {e}"));
            Msg::WorkflowError(format!("{name}: {e}"))
        }
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

/// GET a URL, log the exchange, and return the body when 2xx.
async fn get_ok_text(
    http: &reqwest::Client,
    url: reqwest::Url,
    log: &Logger,
) -> Result<String, String> {
    log.info(format!("GET {url}"));
    let resp = http.get(url).send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("reading body failed: {e}"))?;
    log.info(format!("-> {status} {} bytes", body.len()));
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", head(&body, 200)));
    }
    Ok(body)
}

/// GET a URL and return raw bytes when 2xx (no logging: used for bulk image fetches).
async fn get_ok_bytes(http: &reqwest::Client, url: reqwest::Url) -> Result<Vec<u8>, String> {
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
}

async fn run_generate(
    client: Client,
    params: Params,
    current: Option<Vec<u8>>,
    gcx: GenCtx,
    ui_workflow: Option<Value>,
    current_prompt: CurrentPrompt,
    authed: Option<(String, reqwest::Client)>,
    tx: Sender<Msg>,
    ctx: egui::Context,
    log: Logger,
) {
    // Resolve and upload the img2img input, if any.
    let input_image = if params.mode == Mode::Img2Img {
        let bytes = match params.img2img_source {
            Img2ImgSource::CurrentOutput => current,
            Img2ImgSource::Url => match fetch_bytes(&params.input_url, &authed, &log).await {
                Ok(b) => Some(b),
                Err(e) => {
                    log.error(format!("img2img input fetch failed: {e}"));
                    let _ = tx.send(Msg::GenError(format!("Fetch input failed: {e}")));
                    ctx.request_repaint();
                    return;
                }
            },
        };
        let Some(bytes) = bytes else {
            let _ = tx.send(Msg::GenError("No input image for img2img".into()));
            ctx.request_repaint();
            return;
        };
        let name = INPUT_IMAGE_NAME;
        log.info(format!("uploading img2img input ({} bytes)", bytes.len()));
        let resp = match client
            .upload_image(name, bytes, rucomfyui::upload::UploadType::Input, true)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                log.error(format!("upload failed: {e}"));
                let _ = tx.send(Msg::GenError(format!("Upload failed: {e}")));
                ctx.request_repaint();
                return;
            }
        };
        // Reference the image where the server actually stored it. comfy-gate namespaces uploads
        // into a per-user subfolder, so LoadImage needs "subfolder/name" — the bare filename gets
        // "Invalid image file" because ComfyUI looks in the plain input dir.
        let image_ref = if resp.subfolder.is_empty() {
            resp.name.clone()
        } else {
            format!("{}/{}", resp.subfolder, resp.name)
        };
        log.info(format!("uploaded input as '{image_ref}'"));
        Some(image_ref)
    } else {
        None
    };

    let (wf, _out, report) = workflow::build(&params, input_image, &gcx.apps, &gcx.schemas);
    let note = report.note();
    if !note.is_empty() {
        log.info(format!("enhance: {note}"));
        let _ = tx.send(Msg::EnhanceNote(note));
    }
    stream_execution(client, wf, ui_workflow, tx, ctx, log, current_prompt).await;
}

/// POST `/prompt` with `extra_pnginfo.workflow` so `SaveImage` embeds the UI JSON in the PNG.
async fn queue_prompt_with_workflow_meta(
    client: &Client,
    wf: &Workflow,
    ui_workflow: Option<&Value>,
    log: &Logger,
) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct QueueResult {
        prompt_id: String,
    }

    let body = serde_json::json!({
        "prompt": wf,
        "client_id": client.client_id(),
        "extra_data": {
            "extra_pnginfo": {
                "workflow": ui_workflow
            }
        }
    });
    let result: QueueResult = client
        .post_json("prompt", &body)
        .await
        .map_err(|e| format!("queue failed: {e}"))?;
    log.info(format!("queued with workflow meta: {}", result.prompt_id));
    Ok(result.prompt_id)
}

/// Queue a workflow and forward its event stream to the UI. Shared by the Generate tab and the
/// graph editor. A dropped event stream (one failed poll kills rucomfyui's whole stream) falls
/// back to patiently reconciling results from the history endpoint instead of failing the run.
async fn stream_execution(
    client: Client,
    wf: Workflow,
    ui_workflow: Option<Value>,
    tx: Sender<Msg>,
    ctx: egui::Context,
    log: Logger,
    current_prompt: CurrentPrompt,
) {
    // Send a message and wake the UI.
    macro_rules! send {
        ($m:expr) => {{
            let _ = tx.send($m);
            ctx.request_repaint();
        }};
    }

    // When we have UI workflow metadata to embed, queue via a custom POST that includes
    // extra_pnginfo, then rely on the persistent ws_listener for progress and
    // reconcile_from_history for final images. Otherwise use the standard execute() path.
    if let Some(ui) = ui_workflow.as_ref() {
        let prompt_id = match queue_prompt_with_workflow_meta(&client, &wf, Some(ui), &log).await {
            Ok(id) => id,
            Err(e) => {
                log.error(format!("queueing workflow with metadata failed: {e}"));
                send!(Msg::GenError(e.to_string()));
                return;
            }
        };
        *current_prompt.lock().unwrap() = Some(prompt_id.clone());
        send!(Msg::Queued);
        let outcome = reconcile_from_history(&client, &prompt_id, &tx, &ctx, &log).await;
        *current_prompt.lock().unwrap() = None;
        match outcome {
            Ok(()) => send!(Msg::Done),
            Err(m) => send!(Msg::GenError(m)),
        }
        return;
    }

    let mut execution = match client.execute(&wf).await {
        Ok(e) => e,
        Err(e) => {
            log.error(format!("queueing workflow failed: {e}"));
            send!(Msg::GenError(e.to_string()));
            return;
        }
    };
    let prompt_id = execution.prompt_id().to_string();
    log.info(format!("queued prompt {prompt_id}"));
    *current_prompt.lock().unwrap() = Some(prompt_id.clone());
    send!(Msg::Queued);

    let mut outcome = None;
    while let Some(event) = execution.next().await {
        match event {
            Ok(Event::Status { queue_remaining }) => {
                send!(Msg::Status(format!("Queue: {queue_remaining} ahead")))
            }
            Ok(Event::ExecutionStart { .. }) => {
                log.info("execution started");
                send!(Msg::Status("Started".into()))
            }
            Ok(Event::Executing { node, .. }) => {
                send!(Msg::NodeExecuting(node.as_ref().map(|n| n.0)));
            }
            Ok(Event::Progress { value, max, .. }) => {
                send!(Msg::Progress { value, max })
            }
            Ok(Event::Preview { image, .. }) => {
                if let Some(ci) = decode(&image.data) {
                    send!(Msg::Preview(ci));
                }
            }
            Ok(Event::Executed { node, output, .. }) => {
                log.info(format!("node {} executed: {} image(s)", node.0, output.images.len()));
                send!(Msg::NodeExecuted { node: node.0, images: output.images.clone() });
                for bytes in output.images {
                    if let Some(ci) = decode(&bytes) {
                        send!(Msg::Result { image: ci, bytes });
                    }
                }
            }
            Ok(Event::Error { message, .. }) => {
                log.error(format!("server error: {message}"));
                outcome = Some(Err(message));
                break;
            }
            Ok(Event::Completed { .. }) => {
                outcome = Some(Ok(()));
                break;
            }
            Err(e) => {
                // Transient transport failure: the server is still running the prompt.
                log.warn(format!("execution stream dropped ({e}); waiting on history instead"));
                break;
            }
        }
    }

    let outcome = match outcome {
        Some(o) => o,
        // Stream ended without a verdict: reconcile from the history endpoint.
        None => reconcile_from_history(&client, &prompt_id, &tx, &ctx, &log).await,
    };
    *current_prompt.lock().unwrap() = None;
    match outcome {
        Ok(()) => {
            log.info("generation done");
            send!(Msg::Done);
        }
        Err(message) => send!(Msg::GenError(message)),
    }
}

/// Poll `/history` (gently, tolerating errors) until the prompt completes, then emit its outputs.
async fn reconcile_from_history(
    client: &Client,
    prompt_id: &str,
    tx: &Sender<Msg>,
    ctx: &egui::Context,
    log: &Logger,
) -> Result<(), String> {
    let mut errors = 0u32;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match client.get_history_for_prompt(prompt_id).await {
            Ok(history) => {
                errors = 0;
                let Some(data) = history.data.get(prompt_id) else { continue };
                if !data.status.completed {
                    if data.status.status_str == "error" {
                        return Err("execution failed on the server — see its console".into());
                    }
                    continue;
                }
                for (name, node_output) in &data.outputs.nodes {
                    let Ok(node) = name.parse::<WorkflowNodeId>() else { continue };
                    let mut images = Vec::new();
                    for image in &node_output.images {
                        match image.download(client).await {
                            Ok(bytes) => images.push(bytes),
                            Err(e) => log.warn(format!("output download failed: {e}")),
                        }
                    }
                    log.info(format!("node {} finished: {} image(s)", node.0, images.len()));
                    let _ = tx.send(Msg::NodeExecuted { node: node.0, images: images.clone() });
                    for bytes in images {
                        if let Some(ci) = decode(&bytes) {
                            let _ = tx.send(Msg::Result { image: ci, bytes });
                        }
                    }
                    ctx.request_repaint();
                }
                return Ok(());
            }
            Err(e) => {
                errors += 1;
                if errors == 1 {
                    log.warn(format!("history poll failed (will retry): {e}"));
                }
                if errors > 120 {
                    return Err("lost contact with the server while waiting for results".into());
                }
            }
        }
    }
}

/// Persistent authenticated `/ws` listener. ComfyUI broadcasts `executing`/`progress`/preview
/// events for our `clientId`; execution results still come from polling/history. Cloudflare
/// tunnels idle-cap ~100s TCP sessions, so this sends keepalive pings, refreshes before that
/// cap, and reconnects forever with exponential backoff. Ends when the UI drops its receiver.
async fn ws_listener(
    base: String,
    api_key: String,
    session: String,
    client_id: String,
    tx: Sender<Msg>,
    ctx: egui::Context,
    log: Logger,
    current: CurrentPrompt,
) {
    use futures::SinkExt as _;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::protocol::Message;

    // Keep under Cloudflare's ~100s idle/session cap on free tunnels.
    const KEEPALIVE: Duration = Duration::from_secs(25);
    const MAX_SESSION: Duration = Duration::from_secs(90);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);

    let ws_base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let url = format!("{ws_base}/ws?clientId={client_id}");
    let mut ever_ok = false;
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut request = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => {
                log.warn(format!("ws: invalid url: {e}"));
                return;
            }
        };
        let key = api_key.trim();
        if !key.is_empty() {
            if let Ok(v) = key.parse() {
                request.headers_mut().insert("x-api-key", v);
            }
            if let Ok(v) = format!("Bearer {key}").parse() {
                request.headers_mut().insert("authorization", v);
            }
        }
        let sess = session.trim();
        if !sess.is_empty()
            && let Ok(v) = format!("{SESSION_COOKIE}={sess}").parse()
        {
            request.headers_mut().insert("cookie", v);
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((stream, _)) => {
                if ever_ok {
                    log.info("ws: reconnected");
                } else {
                    log.info("ws: connected — live progress enabled");
                    ever_ok = true;
                }
                backoff = Duration::from_secs(1);
                let (mut write, mut read) = stream.split();
                let mut ping = tokio::time::interval(KEEPALIVE);
                ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                ping.tick().await;
                let refresh_at = tokio::time::Instant::now() + MAX_SESSION;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(refresh_at) => {
                            log.info("ws: refreshing before tunnel session limit");
                            let _ = write.close().await;
                            break;
                        }
                        _ = ping.tick() => {
                            if write.send(Message::Ping(Vec::new())).await.is_err() {
                                log.warn("ws: ping failed; reconnecting");
                                break;
                            }
                        }
                        message = read.next() => {
                            match message {
                                Some(Ok(Message::Text(text))) => {
                                    if let Some(msg) = parse_ws_text(&text, &current) {
                                        if tx.send(msg).is_err() {
                                            return;
                                        }
                                        ctx.request_repaint();
                                    }
                                }
                                Some(Ok(Message::Binary(bytes))) => {
                                    if current.lock().unwrap().is_some()
                                        && let Some(image) = parse_ws_preview(&bytes)
                                        && let Some(ci) = decode(image)
                                    {
                                        if tx.send(Msg::Preview(ci)).is_err() {
                                            return;
                                        }
                                        ctx.request_repaint();
                                    }
                                }
                                Some(Ok(Message::Ping(payload))) => {
                                    if write.send(Message::Pong(payload)).await.is_err() {
                                        break;
                                    }
                                }
                                Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                                Some(Ok(Message::Close(_))) => {
                                    log.warn("ws: closed; reconnecting");
                                    break;
                                }
                                Some(Err(e)) => {
                                    log.warn(format!("ws: dropped ({e}); reconnecting"));
                                    break;
                                }
                                None => {
                                    log.warn("ws: ended; reconnecting");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                if ever_ok {
                    log.warn(format!("ws: reconnect failed ({e}); retry in {backoff:?}"));
                } else {
                    log.warn(format!(
                        "ws: connect failed ({e}) — live progress off until reconnect, polling still works"
                    ));
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff.saturating_mul(2)).min(MAX_BACKOFF);
    }
}

/// Map a ComfyUI websocket text frame onto a UI message.
///
/// `status` is broadcast to every client (website jobs included). Progress / executing stay scoped
/// to our `clientId`; when we have a current prompt, non-matching `prompt_id`s are dropped.
fn parse_ws_text(text: &str, current: &CurrentPrompt) -> Option<Msg> {
    let v: Value = serde_json::from_str(text).ok()?;
    let data = v.get("data")?;
    let kind = v.get("type")?.as_str()?;
    if kind == "status" {
        let remaining = data
            .pointer("/status/exec_info/queue_remaining")
            .and_then(Value::as_u64)
            .or_else(|| data.get("queue_remaining").and_then(Value::as_u64))?;
        return Some(Msg::QueueRemaining(remaining as u32));
    }
    let cur = current.lock().unwrap().clone();
    let pid = data.get("prompt_id").and_then(Value::as_str);
    if let Some(cur) = cur.as_deref() {
        if pid.is_some_and(|p| p != cur) {
            return None;
        }
    } else if pid.is_some() {
        // Idle: still surface progress/executing for any prompt our socket receives (same clientId
        // re-queued from elsewhere, or a server that broadcasts execution events).
    }
    match kind {
        "executing" => {
            let node = data.get("node").and_then(Value::as_str).and_then(|s| s.parse().ok());
            Some(Msg::NodeExecuting(node))
        }
        "progress" => Some(Msg::Progress {
            value: data.get("value").and_then(Value::as_f64).unwrap_or(0.0) as u32,
            max: data.get("max").and_then(Value::as_f64).unwrap_or(0.0) as u32,
        }),
        "progress_state" => {
            let nodes = data.get("nodes")?.as_object()?;
            let running = nodes.values().find(|n| {
                n.get("state").and_then(Value::as_str) == Some("running")
                    && n.get("max").and_then(Value::as_f64).unwrap_or(0.0) > 0.0
            })?;
            Some(Msg::Progress {
                value: running.get("value").and_then(Value::as_f64).unwrap_or(0.0) as u32,
                max: running.get("max").and_then(Value::as_f64).unwrap_or(0.0) as u32,
            })
        }
        _ => None,
    }
}

/// The image bytes of a binary preview frame (framing type 1 legacy, 4 with-metadata).
fn parse_ws_preview(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 8 {
        return None;
    }
    let event = u32::from_be_bytes(bytes[0..4].try_into().ok()?);
    match event {
        1 => Some(&bytes[8..]),
        4 => {
            let metadata_len = u32::from_be_bytes(bytes[4..8].try_into().ok()?) as usize;
            bytes.get(8usize.checked_add(metadata_len)?..)
        }
        _ => None,
    }
}

/// Fetch raw bytes for an img2img input URL. Auth headers are attached only for the connected
/// server's own origin, never leaked to third-party hosts.
async fn fetch_bytes(
    url: &str,
    authed: &Option<(String, reqwest::Client)>,
    log: &Logger,
) -> Result<Vec<u8>, String> {
    log.info(format!("GET {url}"));
    let client = match authed {
        Some((base, http)) if url.starts_with(base.as_str()) => http.clone(),
        _ => tls_builder().build().map_err(|e| e.to_string())?,
    };
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
pub(crate) fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
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
/// Default headers for every request to the connected server: the API key (as both header spellings
/// the gate accepts), the `cg_session` login cookie when signed in, and a JSON `Accept`.
///
/// The `Accept` matters: comfy-gate answers an unauthenticated request with a 303 to its HTML login
/// page when `Accept` contains `text/html`, and a plain 401 otherwise — so asking for JSON turns an
/// expired credential into an error we can report instead of a login page parsed as a workflow.
///
/// Both credentials only ever ride on the connected server's own origin, never a third-party host.
fn apply_auth(builder: reqwest::ClientBuilder, api_key: &str, session: &str) -> reqwest::ClientBuilder {
    builder.default_headers(auth_headers(api_key, session))
}

/// The default header set [`apply_auth`] installs.
fn auth_headers(api_key: &str, session: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{ACCEPT, AUTHORIZATION, COOKIE, HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json, */*"));
    let key = api_key.trim();
    if !key.is_empty() {
        if let Ok(mut v) = HeaderValue::from_str(key) {
            v.set_sensitive(true);
            headers.insert("x-api-key", v);
        }
        if let Ok(mut v) = HeaderValue::from_str(&format!("Bearer {key}")) {
            v.set_sensitive(true);
            headers.insert(AUTHORIZATION, v);
        }
    }
    let session = session.trim();
    if !session.is_empty()
        && let Ok(mut v) = HeaderValue::from_str(&format!("{SESSION_COOKIE}={session}"))
    {
        v.set_sensitive(true);
        headers.insert(COOKIE, v);
    }
    headers
}

/// comfy-gate's session cookie name.
const SESSION_COOKIE: &str = "cg_session";

/// `{"items":[{"subfolder":…,"filename":…}]}` — the album add/remove body shape.
fn items_body(items: Vec<(String, String)>) -> Value {
    let items: Vec<Value> = items
        .into_iter()
        .map(|(subfolder, filename)| serde_json::json!({ "subfolder": subfolder, "filename": filename }))
        .collect();
    serde_json::json!({ "items": items })
}

/// Pull the session token out of a login response's `Set-Cookie` headers.
///
/// A wrong password is not an HTTP error from comfy-gate — success and failure are both a 303, and
/// only the presence of this cookie tells them apart.
fn session_from_set_cookie<'a>(values: impl Iterator<Item = &'a str>) -> Option<String> {
    for raw in values {
        for part in raw.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else { continue };
            if name.trim() == SESSION_COOKIE && !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    None
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
    with_timeouts(reqwest::Client::builder().use_preconfigured_tls(config))
}

#[cfg(not(feature = "tls"))]
fn tls_builder() -> reqwest::ClientBuilder {
    with_timeouts(reqwest::Client::builder())
}

/// Connect and idle-read timeouts so a wedged server surfaces an error instead of hanging a
/// request (and its spinner) forever. No total timeout: big-but-flowing downloads are fine.
fn with_timeouts(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder.connect_timeout(Duration::from_secs(10)).read_timeout(Duration::from_secs(30))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn current(id: Option<&str>) -> CurrentPrompt {
        Arc::new(Mutex::new(id.map(str::to_string)))
    }

    /// Sign-in hinges entirely on spotting this cookie: comfy-gate answers a wrong password with
    /// the same 303 as a right one, so a miss here reads as "wrong password" on a good login.
    #[test]
    fn session_cookie_is_found_among_attributes_and_other_cookies() {
        let real = "cg_session=abc123; Path=/; HttpOnly; SameSite=Lax; Max-Age=1209600";
        assert_eq!(
            session_from_set_cookie([real].into_iter()),
            Some("abc123".to_string())
        );
        // Ordering must not matter, and unrelated cookies must not shadow it.
        assert_eq!(
            session_from_set_cookie(["other=1; Path=/", real].into_iter()),
            Some("abc123".to_string())
        );
        // A failed login sets no cookie; the logout clear-cookie has an empty value.
        assert_eq!(session_from_set_cookie(["other=1; Path=/"].into_iter()), None);
        assert_eq!(
            session_from_set_cookie(["cg_session=; Path=/; Max-Age=0"].into_iter()),
            None
        );
        assert_eq!(session_from_set_cookie(std::iter::empty()), None);
    }

    /// A name that merely ends in the cookie's name is a different cookie.
    #[test]
    fn session_cookie_match_is_exact() {
        assert_eq!(session_from_set_cookie(["xcg_session=nope; Path=/"].into_iter()), None);
    }

    #[test]
    fn album_items_body_uses_subfolder_filename_pairs() {
        let body = items_body(vec![("user_a/2026".into(), "out_1.png".into())]);
        assert_eq!(
            body,
            serde_json::json!({"items":[{"subfolder":"user_a/2026","filename":"out_1.png"}]})
        );
    }

    /// Auth rides only on default headers, so a client built without either credential must still
    /// ask for JSON — that is what keeps a 401 from arriving as an HTML login page.
    #[test]
    fn auth_headers_carry_key_session_and_json_accept() {
        use reqwest::header::{ACCEPT, AUTHORIZATION, COOKIE};
        let headers = auth_headers;

        let h = headers("k3y", "s3ss");
        assert_eq!(h.get("x-api-key").unwrap(), "k3y");
        assert_eq!(h.get(AUTHORIZATION).unwrap(), "Bearer k3y");
        assert_eq!(h.get(COOKIE).unwrap(), "cg_session=s3ss");
        assert_eq!(h.get(ACCEPT).unwrap(), "application/json, */*");

        let h = headers("", "");
        assert!(h.get("x-api-key").is_none());
        assert!(h.get(AUTHORIZATION).is_none());
        assert!(h.get(COOKIE).is_none());
        assert_eq!(h.get(ACCEPT).unwrap(), "application/json, */*");

        // Signed in with no API key: the cookie alone authenticates.
        let h = headers("  ", "s3ss");
        assert!(h.get("x-api-key").is_none());
        assert_eq!(h.get(COOKIE).unwrap(), "cg_session=s3ss");
    }

    #[test]
    fn ws_text_maps_progress_and_executing_for_our_prompt() {
        let cur = current(Some("abc"));
        let m = parse_ws_text(
            r#"{"type":"progress","data":{"value":3,"max":8,"prompt_id":"abc"}}"#,
            &cur,
        );
        assert!(matches!(m, Some(Msg::Progress { value: 3, max: 8 })));

        let m = parse_ws_text(
            r#"{"type":"executing","data":{"node":"14","prompt_id":"abc"}}"#,
            &cur,
        );
        assert!(matches!(m, Some(Msg::NodeExecuting(Some(14)))));

        let m = parse_ws_text(
            r#"{"type":"progress_state","data":{"prompt_id":"abc","nodes":{
                "3":{"state":"finished","value":1,"max":1},
                "7":{"state":"running","value":5,"max":20}}}}"#,
            &cur,
        );
        assert!(matches!(m, Some(Msg::Progress { value: 5, max: 20 })));
    }

    #[test]
    fn ws_text_ignores_other_prompts_when_ours_is_running() {
        let other = r#"{"type":"progress","data":{"value":1,"max":8,"prompt_id":"zzz"}}"#;
        assert!(parse_ws_text(other, &current(Some("abc"))).is_none());
    }

    #[test]
    fn ws_text_status_broadcast_works_while_idle() {
        let m = parse_ws_text(
            r#"{"type":"status","data":{"status":{"exec_info":{"queue_remaining":3}}}}"#,
            &current(None),
        );
        assert!(matches!(m, Some(Msg::QueueRemaining(3))));
    }

    /// ComfyUI's plain `progress` frames often omit `prompt_id`; since our ws only receives events
    /// for our own clientId's prompts, a prompt-less frame while running is ours.
    #[test]
    fn ws_text_accepts_progress_without_prompt_id_while_running() {
        let cur = current(Some("abc"));
        let m = parse_ws_text(r#"{"type":"progress","data":{"value":4,"max":10}}"#, &cur);
        assert!(matches!(m, Some(Msg::Progress { value: 4, max: 10 })));
    }

    #[test]
    fn ws_text_accepts_progress_while_idle() {
        let m = parse_ws_text(r#"{"type":"progress","data":{"value":1,"max":8}}"#, &current(None));
        assert!(matches!(m, Some(Msg::Progress { value: 1, max: 8 })));
    }

    #[test]
    fn ws_preview_framings() {
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&1u32.to_be_bytes());
        legacy.extend_from_slice(&1u32.to_be_bytes());
        legacy.extend_from_slice(b"jpegbytes");
        assert_eq!(parse_ws_preview(&legacy), Some(b"jpegbytes".as_slice()));

        let metadata = br#"{"image_type":"image/png"}"#;
        let mut with_meta = Vec::new();
        with_meta.extend_from_slice(&4u32.to_be_bytes());
        with_meta.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        with_meta.extend_from_slice(metadata);
        with_meta.extend_from_slice(b"pngbytes");
        assert_eq!(parse_ws_preview(&with_meta), Some(b"pngbytes".as_slice()));

        let mut truncated = Vec::new();
        truncated.extend_from_slice(&4u32.to_be_bytes());
        truncated.extend_from_slice(&9999u32.to_be_bytes());
        truncated.extend_from_slice(b"short");
        assert_eq!(parse_ws_preview(&truncated), None);
    }
}
