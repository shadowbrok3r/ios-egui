//! Async ComfyUI engine. A tokio runtime owns all networking; results flow back to the UI thread
//! over an mpsc channel. [`Host`] is main-thread only, so the worker never touches it — it wakes
//! the UI with a cloned [`egui::Context`] and the UI applies effects (haptics, notifications).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    Album, AlbumList, CheckpointCatalog, DownloadKind, DownloadProgress, DownloadTargets,
    DownloadVersions, Facets, GalleryPage, GalleryView, GateWriteResp, Img2ImgSource, LoraCatalog,
    AppUpdate, LoraDetail, LoraFacets, LoraPage, Mode, Params, GATE_WIRE_VERSION,
};
use crate::{tags, uiwf, workflow};

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

/// One `GET /queue` entry (`queue_running` / `queue_pending`): the server's run number, prompt id,
/// and the prompt summary scraped from the entry's embedded graph (checkpoint, prompt, sampler, …).
#[derive(Clone, Debug, PartialEq)]
pub struct QueueJob {
    pub number: i64,
    pub prompt_id: String,
    pub meta: Option<crate::gallery::ImageMeta>,
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
    /// The server took the prompt but will not run all of its output nodes, so the run finishes
    /// having written nothing. Surfaced on both the Create and Graph tabs.
    OutputsDropped(String),
    Queued,
    /// The server assigned this prompt id to a job we submitted (server transport only).
    /// Carries the job's display label so it can never be attributed to the wrong submission
    /// (a FIFO pop on the UI side mislabels when concurrent queue POSTs answer out of order).
    /// `expanded` is the gate's queue-time prompt rewrite (`cg_expanded`, node id -> text) when
    /// it happened; `None` when the text ran verbatim or the queue POST couldn't report it.
    PromptId {
        id: String,
        label: String,
        expanded: Option<std::collections::BTreeMap<String, String>>,
    },
    Progress { value: u32, max: u32 },
    Status(String),
    /// Server-wide queue depth from the WS `status` broadcast (includes jobs from other clients).
    QueueRemaining(u32),
    /// Per-job `GET /queue` snapshot: running + pending jobs in server order.
    QueueJobs { running: Vec<QueueJob>, pending: Vec<QueueJob> },
    Preview(egui::ColorImage),
    /// One finished output image. `label` is the submitting job's display label, so consumers
    /// that fan several jobs out at once (the character taste test) can attribute each image
    /// to its job — arrival order alone lies when the server runs jobs out of submission order.
    Result { image: egui::ColorImage, bytes: Vec<u8>, label: String },
    /// A node started executing (`None` = prompt finished). WebSocket transport only today.
    NodeExecuting(Option<u32>),
    /// A node finished and produced images (raw encoded bytes, for graph-node display).
    NodeExecuted { node: u32, images: Vec<Vec<u8>> },
    /// One job finished; carries its display label for the completion notification.
    Done(String),
    Cancelled,
    GenError(String),
    /// Server-side workflow file names (`/userdata?dir=workflows`).
    Workflows(Vec<String>),
    /// A workflow fetched and converted to API format, ready for the graph editor.
    WorkflowLoaded {
        name: String,
        workflow: Box<Workflow>,
        warnings: Vec<String>,
        /// UI node id → seed input → randomize, from `control_after_generate`.
        seed_randomize: std::collections::BTreeMap<(u64, String), bool>,
        /// UI node id → input → value, for widgets `/object_info` doesn't declare.
        extra_widgets: std::collections::BTreeMap<(u64, String), Value>,
    },
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
    /// A full-image fetch failed; consumers waiting on `key` must give up rather than wait forever.
    FullImageError { key: String, why: String },
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
    /// Trash row ids for the images a delete just moved — fuels the Undo snackbar.
    TrashedIds(Vec<i64>),
    /// Items a delete did not remove; their optimistic tombstones must lift. Never the whole
    /// request — an item the server did trash stays hidden even when a sibling failed.
    TrashFailed(Vec<(String, String)>),
    /// One page of the server trash listing (`/gallery/api/list?trash=1`).
    TrashPage { total: u64, items: Vec<crate::types::TrashItem> },
    /// A restore/purge finished; the UI reloads the trash view (and the gallery on restore).
    TrashChanged { note: String, restored: bool },
    /// Which albums one image belongs to (`GET /gallery/api/meta`); `key` is `subfolder/filename`.
    ItemAlbums { key: String, albums: Vec<i64> },
    /// The gate's pre-parsed prompt summary from the same `/gallery/api/meta` payload — fills the
    /// viewer's info/Remix panel immediately; the full workflow fetch refines it when it lands.
    ItemMeta { key: String, meta: Box<crate::gallery::ImageMeta> },
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
    /// One page of the managed LoRA library. `generation` is the request generation — a filter
    /// change bumps it, and a page answering a superseded query must be dropped (gallery's rule).
    LoraLibrary { generation: u64, page: LoraPage },
    /// Filter chip values for the whole managed library.
    LoraLibraryFacets(LoraFacets),
    /// A single LoRA's expensive extras, for the detail view.
    LoraLibraryDetail(Box<LoraDetail>),
    /// Any managed-library request failed; the string is show-ready.
    LoraLibraryError(String),
    /// The gate has no preview for this LoRA (HTTP 204) — never ask again for this id.
    LoraThumbMissing { key: String },
    /// Whether this account may run the gate's admin-only writes (downloads, edits).
    LoraAdmin(bool),
    /// Where a Civitai download of `kind` may be saved.
    DownloadTargets { kind: DownloadKind, targets: DownloadTargets },
    /// What a pasted Civitai link resolved to.
    DownloadVersions { kind: DownloadKind, found: Box<DownloadVersions> },
    /// A download was accepted and backgrounded server-side; poll `id` for progress.
    DownloadStarted { id: String, label: String },
    /// One poll of a running download.
    DownloadProgress { id: String, progress: Box<DownloadProgress> },
    /// A download request failed outright (not the transfer — the request).
    DownloadError(String),
    /// What build comfy-gate is offering for this app.
    AppUpdate(Box<AppUpdate>),
    /// Bytes downloaded so far of the update APK.
    AppUpdateProgress { got: u64, total: u64 },
    /// The APK is on disk and its checksum matched; ready to hand to the installer.
    AppUpdateReady { path: String },
    AppUpdateError(String),
    /// Server checkpoint catalog (`GET /checkpoint-catalog.json`).
    CheckpointCatalog(CheckpointCatalog),
    CheckpointCatalogError(String),
    /// Server tag dictionary override (`GET /comfyui-android/tags.csv.gz`).
    TagDict(Arc<tags::TagDict>),
    /// Decoded preview for Create img2img "From URL" (or an error string).
    Img2ImgUrlPreview {
        url: String,
        image: Option<egui::ColorImage>,
        error: Option<String>,
    },
}

/// One event from a streaming `/api/expand` request (see [`Engine::expand_prompt`]).
pub enum ExpandMsg {
    /// A token delta to append to the prompt being rewritten.
    Delta(String),
    /// The stream reached `[DONE]` (or closed cleanly) — no more deltas.
    Done,
    /// The request failed or the expander is down; the caller keeps the original text.
    Error(String),
    /// This server has no usable expander at all (404 = plain ComfyUI or a gate older than the
    /// endpoint, 501, 503 = expansion switched off). Shown like [`Self::Error`], but the app also
    /// remembers it and stops offering server-side rewriting until the connection changes.
    Unsupported(String),
}

/// One alternative from `POST /api/variations`: the same prompt with a single `axis`
/// (setting / pose / lighting / wardrobe / mood / composition) deliberately changed.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Variation {
    #[serde(default)]
    pub axis: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
}

/// `POST /api/variations`' response. Every field defaults: partial success is normal (some
/// variations can fail validation and land in `errors` while the rest come back fine), and the
/// shape of an `errors` entry isn't pinned down, so it stays raw JSON until it's rendered.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Variations {
    #[serde(default)]
    pub dialect: String,
    /// Elements the server held fixed: whatever the user weighted, plus the request's `keep`.
    #[serde(default)]
    pub anchors: Vec<String>,
    #[serde(default)]
    pub variations: Vec<Variation>,
    #[serde(default)]
    pub errors: Vec<Value>,
}

impl Variations {
    /// `errors` as display lines, whatever shape the entries take (string, or an object with a
    /// message/error/detail field, else the raw JSON).
    pub fn error_lines(&self) -> Vec<String> {
        self.errors
            .iter()
            .map(|e| match e {
                Value::String(s) => s.clone(),
                Value::Object(map) => ["error", "message", "detail", "reason"]
                    .iter()
                    .find_map(|k| map.get(*k).and_then(|v| v.as_str()))
                    .map(|s| match map.get("axis").and_then(|v| v.as_str()) {
                        Some(axis) => format!("{axis}: {s}"),
                        None => s.to_string(),
                    })
                    .unwrap_or_else(|| e.to_string()),
                other => other.to_string(),
            })
            .collect()
    }
}

/// A `POST /api/variations` request. `dialect` follows the same rule as `/api/expand`: a family
/// key or the workflow's loader filename, empty to let the server use its generic prompt.
pub struct VariationsReq {
    pub text: String,
    pub dialect: String,
    /// Server clamps to 1..=6; [`variations_body`] clamps too so the count shown is the count sent.
    pub count: u32,
    /// `subtle` | `moderate` | `wild`; empty means the server default (`moderate`).
    pub strength: String,
    /// Elements that must survive the rewrite unchanged (on top of the weights the server reads
    /// out of the text itself).
    pub keep: Vec<String>,
}

/// The outcome of a `POST /api/variations` request.
pub enum VariationsMsg {
    /// The response parsed; `variations` may still be partial (see [`Variations::error_lines`]).
    Done(Variations),
    /// This attempt failed (all variations failed, bad request, transport) — keep the original.
    Error(String),
    /// This server has no variations endpoint (404) or expansion is switched off (503), so the
    /// app stops offering it until the connection changes. Note 501 is NOT this: it means only
    /// *this dialect* has no variation prompt, which another checkpoint may well have.
    Unsupported(String),
}

pub struct Engine {
    rt: tokio::runtime::Runtime,
    ctx: egui::Context,
    log: Logger,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    client: Option<Client>,
    http: Option<reqwest::Client>,
    /// Authed client with a long read timeout, for endpoints the gate holds open a while: the
    /// queue-time expander stalls `POST /prompt` up to ~90 s, and a cold `/api/expand` can be slow
    /// to first token. Separate from `http` so ordinary browsing keeps its snappy 30 s wedge detect.
    queue_http: Option<reqwest::Client>,
    base: String,
    /// In-flight generate / graph-run tasks (more than one when Create Queue is used).
    jobs: Vec<tokio::task::JoinHandle<()>>,
    /// How many generate/graph jobs have not finished yet (UI uses this for multi-queue).
    inflight: Arc<AtomicUsize>,
    ws_task: Option<tokio::task::JoinHandle<()>>,
    current_prompt: CurrentPrompt,
    /// Shared cancel flag for local-npu `spawn_blocking` jobs (tokio abort alone won't stop them).
    #[cfg(feature = "local-npu")]
    local_cancel: Arc<AtomicBool>,
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
            queue_http: None,
            base: String::new(),
            jobs: Vec::new(),
            inflight: Arc::new(AtomicUsize::new(0)),
            ws_task: None,
            current_prompt: Arc::new(Mutex::new(None)),
            #[cfg(feature = "local-npu")]
            local_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// The prompt id of the most recent job we queued, while it is still in flight.
    pub fn current_prompt_id(&self) -> Option<String> {
        self.current_prompt.lock().unwrap().clone()
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
        let http = match apply_auth(tls_builder(READ_TIMEOUT), &api_key, &session).build() {
            Ok(c) => c,
            Err(e) => {
                log.error(format!("HTTP client build failed: {e}"));
                let _ = self.tx.send(Msg::ConnectError(e.to_string()));
                self.ctx.request_repaint();
                return;
            }
        };
        // Same auth, a long read timeout: the gate holds POST /prompt and /api/expand open well
        // past the 30 s browsing timeout. A build failure here only loses the long-hold headroom.
        self.queue_http = apply_auth(tls_builder(QUEUE_READ_TIMEOUT), &api_key, &session).build().ok();
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
        label: String,
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
        let queue_authed = self.queue_http.clone().map(|h| (self.base.clone(), h));
        let current_prompt = self.current_prompt.clone();
        let inflight = self.inflight.clone();
        inflight.fetch_add(1, Ordering::SeqCst);
        self.reap_jobs();
        self.jobs.push(self.rt.spawn(async move {
            run_generate(client, params, current, gcx, ui_workflow, label, current_prompt, authed, queue_authed, tx, ctx, log).await;
            inflight.fetch_sub(1, Ordering::SeqCst);
        }));
    }

    /// Queue an arbitrary API-format workflow (from the graph editor).
    /// `ui_workflow` is the UI-format JSON to embed in the PNG via `extra_pnginfo`.
    pub fn run_workflow(
        &mut self,
        wf: Workflow,
        ui_workflow: Option<Value>,
        schemas: Arc<SchemaSet>,
        label: String,
    ) {
        let Some(client) = self.client.clone() else {
            let _ = self.tx.send(Msg::GenError("Not connected".into()));
            return;
        };
        self.log.info(format!("queue graph workflow: {} nodes", wf.0.len()));
        let (tx, ctx, log) = (self.tx.clone(), self.ctx.clone(), self.log.clone());
        let authed = self.http.clone().map(|h| (self.base.clone(), h));
        let queue_authed = self.queue_http.clone().map(|h| (self.base.clone(), h));
        let current = self.current_prompt.clone();
        let inflight = self.inflight.clone();
        inflight.fetch_add(1, Ordering::SeqCst);
        self.reap_jobs();
        self.jobs.push(self.rt.spawn(async move {
            // Graph-editor jobs always carry a UI workflow, so the direct-POST path is taken anyway.
            stream_execution(client, wf, ui_workflow, schemas, authed, queue_authed, false, label, tx, ctx, log, current).await;
            inflight.fetch_sub(1, Ordering::SeqCst);
        }));
    }

    /// Queue a video "Finish pass": upload the colour-match reference (when any), build the finish
    /// graph for the server-side `video_path`, and stream it like a graph job.
    pub fn run_finish(
        &mut self,
        video_path: String,
        reference: Option<Vec<u8>>,
        scale_by: f32,
        rife_multiplier: u32,
        output_fps: u32,
        schemas: Arc<SchemaSet>,
        label: String,
    ) {
        let Some(client) = self.client.clone() else {
            let _ = self.tx.send(Msg::GenError("Not connected".into()));
            return;
        };
        self.log.info(format!(
            "queue finish pass: {video_path} scale={scale_by} rife={rife_multiplier} fps={output_fps}"
        ));
        let (tx, ctx, log) = (self.tx.clone(), self.ctx.clone(), self.log.clone());
        let authed = self.http.clone().map(|h| (self.base.clone(), h));
        let queue_authed = self.queue_http.clone().map(|h| (self.base.clone(), h));
        let current = self.current_prompt.clone();
        let inflight = self.inflight.clone();
        inflight.fetch_add(1, Ordering::SeqCst);
        self.reap_jobs();
        self.jobs.push(self.rt.spawn(async move {
            run_finish_job(
                client, video_path, reference, scale_by, rife_multiplier, output_fps, schemas,
                authed, queue_authed, label, tx, ctx, log, current,
            )
            .await;
            inflight.fetch_sub(1, Ordering::SeqCst);
        }));
    }

    /// Stream the gate's `POST /api/expand` rewrite of `text`, forwarding token deltas over the
    /// returned channel. Uses [`Self::queue_http`] for its long read timeout so a cold expander's
    /// first token can't trip the browsing timeout. `cancel` aborts the request between chunks
    /// (set it when the user dismisses the review). A non-200 or missing connection arrives as a
    /// single [`ExpandMsg::Error`], and the caller falls back to submitting the original text.
    ///
    /// `dialect` tells the gate which model family to write for — either a dialect key
    /// (`wan-i2v`, `illustrious`, `flux`, …) or just the loader filename, which the gate
    /// classifies itself. This endpoint gets no graph, so without the hint every family falls back
    /// to one generic prompt; see [`crate::types::expand_dialect_key`]. Empty omits the field.
    pub fn expand_prompt(
        &self,
        text: String,
        dialect: String,
        cancel: Arc<AtomicBool>,
    ) -> Receiver<ExpandMsg> {
        let (tx, rx) = std::sync::mpsc::channel();
        match self.queue_http.clone() {
            Some(http) => {
                let (base, ctx, log) = (self.base.clone(), self.ctx.clone(), self.log.clone());
                self.rt.spawn(async move {
                    stream_expand(http, base, text, dialect, cancel, tx, ctx, log).await;
                });
            }
            None => {
                let _ = tx.send(ExpandMsg::Error("Not connected".into()));
            }
        }
        rx
    }

    /// Ask the gate for `POST /api/variations` alternatives to a prompt. Unlike
    /// [`Self::expand_prompt`] this answers with one JSON body rather than a stream — the point is
    /// to compare the options side by side — so the result arrives as a single [`VariationsMsg`].
    /// Each alternative costs its own completion server-side (~0.7 s each, run back to back), so
    /// this uses [`Self::queue_http`]'s long read timeout and `cancel` aborts the wait.
    pub fn variations(
        &self,
        req: VariationsReq,
        cancel: Arc<AtomicBool>,
    ) -> Receiver<VariationsMsg> {
        let (tx, rx) = std::sync::mpsc::channel();
        match self.queue_http.clone() {
            Some(http) => {
                let (base, ctx, log) = (self.base.clone(), self.ctx.clone(), self.log.clone());
                self.rt.spawn(async move {
                    fetch_variations(http, base, req, cancel, tx, ctx, log).await;
                });
            }
            None => {
                let _ = tx.send(VariationsMsg::Error("Not connected".into()));
            }
        }
        rx
    }

    /// Abort all local generate/graph jobs (the server may keep finishing queued prompts).
    pub fn cancel(&mut self) {
        #[cfg(feature = "local-npu")]
        self.local_cancel.store(true, Ordering::SeqCst);
        for h in self.jobs.drain(..) {
            h.abort();
        }
        self.inflight.store(0, Ordering::SeqCst);
        *self.current_prompt.lock().unwrap() = None;
        self.log.warn("generation cancelled locally");
        let _ = self.tx.send(Msg::Cancelled);
        self.ctx.request_repaint();
    }

    /// Queue an on-device HTP text2img (feature `local-npu`). Uses `spawn_blocking` so the NPU
    /// work doesn't starve the async runtime; cancel is cooperative via [`Self::cancel`].
    #[cfg(feature = "local-npu")]
    pub fn generate_local(&mut self, paths: crate::local_engine::LocalPaths, params: Params) {
        self.local_cancel.store(false, Ordering::SeqCst);
        let cancel = self.local_cancel.clone();
        let (tx, ctx, log) = (self.tx.clone(), self.ctx.clone(), self.log.clone());
        let inflight = self.inflight.clone();
        inflight.fetch_add(1, Ordering::SeqCst);
        self.reap_jobs();
        self.jobs.push(self.rt.spawn_blocking(move || {
            crate::local_engine::run(paths, params, tx, ctx, log, cancel);
            inflight.fetch_sub(1, Ordering::SeqCst);
        }));
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
            let (running, pending) = parse_queue(&v);
            let remaining = (running.len() + pending.len()) as u32;
            let _ = tx.send(Msg::QueueRemaining(remaining));
            let _ = tx.send(Msg::QueueJobs { running, pending });
            ctx.request_repaint();
        });
    }

    /// Interrupt one prompt (`POST /interrupt {"prompt_id": …}`), defaulting to our own in-flight
    /// one. ComfyUI's targeted path only fires while that prompt is still the running one, so it
    /// cannot land on a job that started in the meantime. With no id to name it sends nothing at
    /// all: a bodyless interrupt is a process-global kill, and on a shared server the only job it
    /// could reach in that window belongs to someone else.
    pub fn interrupt(&self, prompt_id: Option<String>) {
        let Some(id) = prompt_id.or_else(|| self.current_prompt.lock().unwrap().clone()) else {
            self.log.warn("interrupt skipped — no prompt of ours is running");
            return;
        };
        let Some((http, url)) = self.authed_url("/interrupt", &[]) else { return };
        let log = self.log.clone();
        let body = serde_json::json!({ "prompt_id": id });
        self.rt.spawn(async move {
            log.info(format!("POST /interrupt {body}"));
            match http.post(url).json(&body).send().await {
                Ok(r) => log.info(format!("-> {}", r.status())),
                Err(e) => log.warn(format!("interrupt failed: {e}")),
            }
        });
    }

    /// Remove pending jobs from the server queue by prompt id (`POST /queue {"delete":[...]}`).
    pub fn queue_delete(&self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let Some((http, url)) = self.authed_url("/queue", &[]) else { return };
        let log = self.log.clone();
        let body = serde_json::json!({ "delete": ids });
        self.rt.spawn(async move {
            log.info(format!("POST /queue {body}"));
            match http.post(url).json(&body).send().await {
                Ok(r) => log.info(format!("-> {}", r.status())),
                Err(e) => log.warn(format!("queue delete failed: {e}")),
            }
        });
    }

    /// Clear every pending job from the server queue (`POST /queue {"clear":true}`).
    pub fn queue_clear(&self) {
        let Some((http, url)) = self.authed_url("/queue", &[]) else { return };
        let log = self.log.clone();
        let body = serde_json::json!({ "clear": true });
        self.rt.spawn(async move {
            log.info("POST /queue clear");
            match http.post(url).json(&body).send().await {
                Ok(r) => log.info(format!("-> {}", r.status())),
                Err(e) => log.warn(format!("queue clear failed: {e}")),
            }
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

    // ---- App self-update (`/comfyui-android/update.json`, `/comfyui-android/app.apk`) ----------

    /// Ask the gate what build it is offering. Quiet on failure — a gate too old to have the
    /// endpoint is the normal case, not an error worth showing.
    pub fn check_app_update(&self) {
        let Some((http, url)) = self.authed_url("/comfyui-android/update.json", &[]) else { return };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<AppUpdate>(&body) {
                    Ok(u) if wire_ok(u.version, "update.json").is_ok() => {
                        if u.available {
                            log.info(format!(
                                "app update: {} (code {})",
                                u.version_name, u.version_code
                            ));
                        }
                        let _ = tx.send(Msg::AppUpdate(Box::new(u)));
                        ctx.request_repaint();
                    }
                    Ok(_) => {}
                    Err(e) => log.warn(format!("update.json decode: {e}")),
                },
                // A gate without the route 404s; that is a deployment state, not a failure.
                Err(e) => log.info(format!("app update check: {e}")),
            }
        });
    }

    /// Stream the APK to `dest`, verifying its sha256, reporting progress as it goes.
    ///
    /// Streamed rather than buffered: this is a 50-200 MB file on a phone, and `get_ok_bytes`
    /// would hold all of it in RAM alongside the copy being written. Written to `<dest>.part` and
    /// renamed only after the hash matches, so an interrupted download can never be handed to the
    /// package installer as a complete APK.
    pub fn download_app_update(&self, dest: String, expect_sha256: String, expect_size: u64) {
        let Some((http, url)) = self.authed_url("/comfyui-android/app.apk", &[]) else { return };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            use sha2::{Digest, Sha256};
            use std::io::Write;

            let part = format!("{dest}.part");
            let _ = std::fs::remove_file(&part);
            let mut resp = match http.get(url).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    let _ = tx.send(Msg::AppUpdateError(format!("HTTP {}", r.status())));
                    ctx.request_repaint();
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Msg::AppUpdateError(e.to_string()));
                    ctx.request_repaint();
                    return;
                }
            };
            let total = resp.content_length().unwrap_or(expect_size);
            let mut file = match std::fs::File::create(&part) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Msg::AppUpdateError(format!("can't write {part}: {e}")));
                    ctx.request_repaint();
                    return;
                }
            };
            let mut hasher = Sha256::new();
            let mut got: u64 = 0;
            let mut last_report = 0u64;
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        hasher.update(&chunk);
                        if let Err(e) = file.write_all(&chunk) {
                            let _ = tx.send(Msg::AppUpdateError(format!("write failed: {e}")));
                            ctx.request_repaint();
                            let _ = std::fs::remove_file(&part);
                            return;
                        }
                        got += chunk.len() as u64;
                        // Every ~1%: a message per 64 KB chunk would be thousands of repaints.
                        if total == 0 || got - last_report > total / 100 {
                            last_report = got;
                            let _ = tx.send(Msg::AppUpdateProgress { got, total });
                            ctx.request_repaint();
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Msg::AppUpdateError(e.to_string()));
                        ctx.request_repaint();
                        let _ = std::fs::remove_file(&part);
                        return;
                    }
                }
            }
            drop(file);
            let sum = hex_lower(&hasher.finalize());
            // A truncated or corrupted APK fails the installer with a bare "parse error"; catching
            // it here means the message names the real problem.
            if !expect_sha256.is_empty() && sum != expect_sha256 {
                log.error(format!("app update: sha256 {sum} != expected {expect_sha256}"));
                let _ = std::fs::remove_file(&part);
                let _ = tx.send(Msg::AppUpdateError(
                    "The download didn't match its checksum — try again".into(),
                ));
                ctx.request_repaint();
                return;
            }
            if let Err(e) = std::fs::rename(&part, &dest) {
                let _ = tx.send(Msg::AppUpdateError(format!("can't finish the download: {e}")));
                ctx.request_repaint();
                return;
            }
            log.info(format!("app update: downloaded {got} bytes to {dest}"));
            let _ = tx.send(Msg::AppUpdateReady { path: dest });
            ctx.request_repaint();
        });
    }

    // ---- The gate's managed LoRA library (`/comfyui-android/lora/*`) --------------------------
    //
    // Distinct from `fetch_lora_catalog` above: that is the read-only strength/trigger catalog the
    // Create picker uses, keyed by ComfyUI `lora_name`. This is the *library manager*, keyed by the
    // model's sha256, which is the only identifier that survives a move or a rename.

    /// One page of the library. `generation` is echoed back so a page answering a superseded
    /// filter can be dropped rather than corrupting the list.
    #[allow(clippy::too_many_arguments)]
    pub fn lora_list(
        &self,
        generation: u64,
        page: u32,
        page_size: u32,
        sort_by: &str,
        folder: &str,
        search: &str,
        base_model: &str,
        tag: &str,
    ) {
        // The gate clamps page_size to 100 regardless; asking for more just wastes the round trip.
        let (page_s, size_s) = (page.to_string(), page_size.clamp(1, 100).to_string());
        let mut q: Vec<(&str, &str)> =
            vec![("page", &page_s), ("page_size", &size_s), ("sort_by", sort_by)];
        for (k, v) in [("folder", folder), ("search", search), ("base_model", base_model), ("tag", tag)] {
            if !v.is_empty() {
                q.push((k, v));
            }
        }
        let Some((http, url)) = self.authed_url("/comfyui-android/lora/list", &q) else { return };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<LoraPage>(&body) {
                    Ok(p) => match wire_ok(p.version, "lora/list") {
                        Ok(()) => {
                            log.info(format!("lora library: page {} of {}", p.page, p.total_pages));
                            Msg::LoraLibrary { generation, page: p }
                        }
                        Err(e) => Msg::LoraLibraryError(e),
                    },
                    Err(e) => Msg::LoraLibraryError(format!("Couldn't read the library: {e}")),
                },
                Err(e) => Msg::LoraLibraryError(lora_http_hint(&e)),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Filter chip values, counted over the whole library rather than the current page.
    pub fn lora_facets(&self) {
        let Some((http, url)) = self.authed_url("/comfyui-android/lora/facets", &[]) else { return };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            // A failure here just leaves the chips empty; it must not block the grid.
            match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<LoraFacets>(&body) {
                    Ok(f) if wire_ok(f.version, "lora/facets").is_ok() => {
                        let _ = tx.send(Msg::LoraLibraryFacets(f));
                    }
                    Ok(_) => {}
                    Err(e) => log.warn(format!("lora facets decode: {e}")),
                },
                Err(e) => log.warn(format!("lora facets: {e}")),
            }
            ctx.request_repaint();
        });
    }

    /// One LoRA's trigger words / notes / description. Costs the gate a second backend call, so
    /// this is a detail-view request — never call it per grid cell.
    pub fn lora_detail(&self, id: String) {
        let Some((http, url)) = self.authed_url("/comfyui-android/lora/detail", &[("id", &id)])
        else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<LoraDetail>(&body) {
                    Ok(d) => match wire_ok(d.version, "lora/detail") {
                        Ok(()) => Msg::LoraLibraryDetail(Box::new(d)),
                        Err(e) => Msg::LoraLibraryError(e),
                    },
                    Err(e) => Msg::LoraLibraryError(format!("Couldn't read that LoRA: {e}")),
                },
                Err(e) => Msg::LoraLibraryError(lora_http_hint(&e)),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// A library preview, always JPEG (the gate transcodes video previews with ffmpeg).
    ///
    /// Separate from [`Self::fetch_thumb`], which is hardwired to the gallery's
    /// `subfolder`/`filename` pair and its on-disk full-image cache. A `204` here means "there is
    /// no preview" rather than a failure, and must latch so the tile never asks again.
    pub fn fetch_lora_thumb(&self, id: String, size: u32) {
        let size_s = size.to_string();
        let Some((http, url)) =
            self.authed_url("/comfyui-android/lora/thumb", &[("id", &id), ("size", &size_s)])
        else {
            return;
        };
        let key = format!("lora#{id}#{size}");
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let resp = match http.get(url).send().await {
                Ok(r) => r,
                Err(e) => {
                    log.warn(format!("lora thumb {id}: {e}"));
                    return;
                }
            };
            // 204 = no preview / undecodable, 404 = unknown id. Both are permanent for this id:
            // latch them so the grid stops re-claiming the tile every frame.
            let status = resp.status();
            if status == reqwest::StatusCode::NO_CONTENT || status == reqwest::StatusCode::NOT_FOUND {
                let _ = tx.send(Msg::LoraThumbMissing { key });
                ctx.request_repaint();
                return;
            }
            if !status.is_success() {
                log.warn(format!("lora thumb {id}: HTTP {status}"));
                let _ = tx.send(Msg::LoraThumbMissing { key });
                ctx.request_repaint();
                return;
            }
            let Ok(bytes) = resp.bytes().await else { return };
            // An empty 200 is the same thing as a 204 as far as the grid is concerned.
            if bytes.is_empty() {
                let _ = tx.send(Msg::LoraThumbMissing { key });
                ctx.request_repaint();
                return;
            }
            let bytes = bytes.to_vec();
            // Off-thread: a full-size decode is tens of ms of CPU that would block a tokio worker,
            // and the gate honours `size` as a bounding box rather than a guarantee.
            let decoded =
                tokio::task::spawn_blocking(move || decode_thumb(&bytes, size)).await.ok().flatten();
            match decoded {
                Some(image) => {
                    let _ = tx.send(Msg::Thumb { key, image });
                }
                None => {
                    log.warn(format!("lora thumb {id}: undecodable"));
                    let _ = tx.send(Msg::LoraThumbMissing { key });
                }
            }
            ctx.request_repaint();
        });
    }

    /// Probe whether this account may run the gate's admin-only writes.
    ///
    /// There is no "who am I" endpoint, so this posts a write that cannot succeed: the gate
    /// resolves the id BEFORE validating the body, so a nonexistent id is rejected with 404 for an
    /// admin and 403 for everyone else, and nothing is mutated either way.
    pub fn probe_lora_admin(&self) {
        let Some((http, url)) = self.authed_url("/comfyui-android/lora/update", &[]) else { return };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let body = serde_json::json!({ "id": "", "notes": "" });
            let admin = match http.post(url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    // 403 is the only definite "no". Anything else (404 unknown id, 400 validation)
                    // means the request got past the admin gate.
                    let admin = status != reqwest::StatusCode::FORBIDDEN
                        && status != reqwest::StatusCode::UNAUTHORIZED;
                    log.info(format!("lora admin probe: HTTP {status} -> admin={admin}"));
                    admin
                }
                Err(e) => {
                    log.warn(format!("lora admin probe: {e}"));
                    return;
                }
            };
            let _ = tx.send(Msg::LoraAdmin(admin));
            ctx.request_repaint();
        });
    }

    // ---- Civitai downloads (`/comfyui-android/download/*`) -------------------------------------

    /// Where a download of `kind` may be saved. Checkpoints have three roots here and saving to the
    /// wrong one hides the file from the loader, so the caller must not collapse the choice.
    pub fn download_targets(&self, kind: DownloadKind) {
        let Some((http, url)) =
            self.authed_url("/comfyui-android/download/targets", &[("type", kind.wire())])
        else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<DownloadTargets>(&body) {
                    Ok(t) => match wire_ok(t.version, "download/targets") {
                        Ok(()) => Msg::DownloadTargets { kind, targets: t },
                        Err(e) => Msg::DownloadError(e),
                    },
                    Err(e) => Msg::DownloadError(format!("Couldn't read the save locations: {e}")),
                },
                Err(e) => Msg::DownloadError(lora_http_hint(&e)),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Resolve whatever the user pasted — a model page, a version link, or a bare id.
    pub fn download_versions(&self, kind: DownloadKind, model: String) {
        let Some((http, url)) = self.authed_url(
            "/comfyui-android/download/versions",
            &[("type", kind.wire()), ("model", &model)],
        ) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<DownloadVersions>(&body) {
                    Ok(v) => match wire_ok(v.version, "download/versions") {
                        Ok(()) => {
                            log.info(format!(
                                "download versions: model={:?} selected={:?} n={}",
                                v.model_id,
                                v.selected_version_id,
                                v.versions.len()
                            ));
                            Msg::DownloadVersions { kind, found: Box::new(v) }
                        }
                        Err(e) => Msg::DownloadError(e),
                    },
                    Err(e) => Msg::DownloadError(format!("That link didn't resolve: {e}")),
                },
                Err(e) => Msg::DownloadError(lora_http_hint(&e)),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Ask the gate to fetch a version. Returns as soon as the transfer is accepted — the gate
    /// backgrounds it, so it survives the app closing, the screen locking or the link dropping.
    pub fn download_start(
        &self,
        kind: DownloadKind,
        version_id: u64,
        root: String,
        folder: String,
        label: String,
    ) {
        let Some((http, url)) = self.authed_url("/comfyui-android/download/start", &[]) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let body = serde_json::json!({
                "type": kind.wire(),
                "model_version_id": version_id,
                "root": root,
                "folder": folder,
            });
            log.info(format!("POST {url} {body}"));
            let msg = match http.post(url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    match serde_json::from_str::<GateWriteResp>(&text) {
                        // Status first, then `ok` — the gate normalises LoRA Manager's
                        // inconsistent failures into this shape.
                        Ok(r) if status.is_success() && r.ok => match r.download_id {
                            Some(id) => Msg::DownloadStarted { id, label },
                            None => Msg::DownloadError("The server accepted it but named no download".into()),
                        },
                        Ok(r) => Msg::DownloadError(
                            r.error.unwrap_or_else(|| download_status_hint(status)),
                        ),
                        Err(_) => Msg::DownloadError(download_status_hint(status)),
                    }
                }
                Err(e) => Msg::DownloadError(e.to_string()),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// One progress poll. Cheap in-memory read server-side; call it every second or two.
    pub fn download_progress(&self, id: String) {
        let Some((http, url)) =
            self.authed_url("/comfyui-android/download/progress", &[("id", &id)])
        else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            // Deliberately quiet: this runs on a timer and a transient failure is not news.
            let Ok(resp) = http.get(url).send().await else { return };
            if !resp.status().is_success() {
                return;
            }
            let Ok(text) = resp.text().await else { return };
            match serde_json::from_str::<DownloadProgress>(&text) {
                Ok(p) => {
                    let _ = tx.send(Msg::DownloadProgress { id, progress: Box::new(p) });
                    ctx.request_repaint();
                }
                Err(e) => log.warn(format!("download progress decode: {e}")),
            }
        });
    }

    pub fn download_cancel(&self, id: String) {
        let Some((http, url)) = self.authed_url("/comfyui-android/download/cancel", &[]) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let body = serde_json::json!({ "id": id });
            log.info(format!("POST {url} {body}"));
            if let Ok(resp) = http.post(url).json(&body).send().await
                && !resp.status().is_success()
            {
                let status = resp.status();
                let _ = tx.send(Msg::DownloadError(download_status_hint(status)));
                ctx.request_repaint();
            }
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

    /// Fetch the server tag-dictionary sidecar (gzip). Tries `/comfyui-android/tags.csv.gz`, then
    /// `/tags.csv.gz`. Soft-fails (logs only) so autocomplete falls back to the bundled dictionary.
    pub fn fetch_tag_dict(&self) {
        let Some(http) = self.http.clone() else { return };
        let base = self.base.clone();
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let paths = ["/comfyui-android/tags.csv.gz", "/tags.csv.gz"];
            for path in paths {
                let Ok(url) = reqwest::Url::parse(&format!("{base}{path}")) else {
                    continue;
                };
                match get_ok_bytes(&http, url).await {
                    Ok(bytes) => match tags::TagDict::parse_csv_gz(&bytes) {
                        Ok(dict) => {
                            log.info(format!("tag dict: {} entries (from {path})", dict.len()));
                            let _ = tx.send(Msg::TagDict(Arc::new(dict)));
                            ctx.request_repaint();
                            return;
                        }
                        Err(e) => log.warn(format!("tag dict {path}: parse error: {e}")),
                    },
                    Err(_) => continue,
                }
            }
            log.warn("tag dict: not found");
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
        let key = format!("{subfolder}/{filename}");
        let Some((http, url)) = self.authed_url(
            "/gallery/api/workflow",
            &[("subfolder", &subfolder), ("filename", &filename)],
        ) else {
            // Offline. This MUST still answer: the viewer waits on `wf_fetched` before it will try
            // to pull a graph out of the file's own bytes, so returning silently here left an
            // offline image stuck on "loading workflow" forever with the local copy right there.
            let _ = self.tx.send(Msg::ItemWorkflowError {
                key,
                error: "not connected — reading the file's own metadata".into(),
            });
            self.ctx.request_repaint();
            return;
        };
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
        let builder = tls_builder(READ_TIMEOUT).redirect(reqwest::redirect::Policy::none());
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
        let http = apply_auth(tls_builder(READ_TIMEOUT).redirect(reqwest::redirect::Policy::none()), "", &session)
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
        let keys = items.clone();
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
                            // Row ids of what just moved to trash — the client's Undo handle.
                            let ids: Vec<i64> = v
                                .get("ids")
                                .and_then(Value::as_array)
                                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                                .unwrap_or_default();
                            if !ids.is_empty() {
                                let _ = tx.send(Msg::TrashedIds(ids));
                            }
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
                            // Un-hide only what the server still holds: everything on a total
                            // failure, else just the items its `errors` name.
                            let live =
                                if gone == 0 { keys.clone() } else { failed_keys(&keys, &errors) };
                            if !live.is_empty() {
                                let _ = tx.send(Msg::TrashFailed(live));
                            }
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
                    let _ = tx.send(Msg::TrashFailed(keys));
                    Msg::AlbumError(format!("Delete failed: HTTP {status}"))
                }
                Err(e) => {
                    log.error(format!("delete failed: {e}"));
                    let _ = tx.send(Msg::TrashFailed(keys));
                    Msg::AlbumError(format!("Delete failed: {e}"))
                }
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// One page of the server trash listing.
    pub fn trash_list(&self, offset: u64, limit: u64) {
        let Some((http, url)) = self.authed_url(
            "/gallery/api/list",
            &[("trash", "1"), ("offset", &offset.to_string()), ("limit", &limit.to_string())],
        ) else {
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            let msg = match get_ok_text(&http, url, &log).await {
                Ok(body) => match serde_json::from_str::<Value>(&body) {
                    Ok(v) => {
                        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
                        let items = v
                            .get("items")
                            .cloned()
                            .map(|i| serde_json::from_value(i).unwrap_or_default())
                            .unwrap_or_default();
                        Msg::TrashPage { total, items }
                    }
                    Err(e) => Msg::GalleryError(format!("trash listing: {e}")),
                },
                Err(e) => Msg::GalleryError(format!("trash listing: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Restore trashed rows to where they came from (`all` ignores `ids`).
    pub fn trash_restore(&self, ids: Vec<i64>, all: bool) {
        self.trash_op("/gallery/api/restore", ids, all, "restored", true);
    }

    /// Permanently unlink trashed rows — the only destructive call in the gallery API.
    pub fn trash_purge(&self, ids: Vec<i64>, all: bool) {
        self.trash_op("/gallery/api/purge", ids, all, "purged", false);
    }

    fn trash_op(&self, path: &str, ids: Vec<i64>, all: bool, verb: &'static str, restored: bool) {
        let Some((http, url)) = self.authed_url(path, &[]) else { return };
        let body = serde_json::json!({ "ids": ids, "all": all });
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            log.info(format!("POST {url} ({} id(s), all={all})", body["ids"].as_array().map(|a| a.len()).unwrap_or(0)));
            let msg = match http.post(url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    log.info(format!("-> {}", head(&text, 200)));
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let n = v.get(verb).and_then(Value::as_u64).unwrap_or(0);
                    let errs = v
                        .get("errors")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let note = if errs > 0 {
                        format!("{verb} {n} ({errs} failed)")
                    } else {
                        format!("{verb} {n}")
                    };
                    Msg::TrashChanged { note, restored }
                }
                Ok(resp) => {
                    let status = resp.status();
                    log.error(format!("{verb} failed: HTTP {status}"));
                    Msg::AlbumError(format!("Trash {verb} failed: HTTP {status}"))
                }
                Err(e) => {
                    log.error(format!("{verb} failed: {e}"));
                    Msg::AlbumError(format!("Trash {verb} failed: {e}"))
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
                // comfy-gate parses the prompt summary server-side (a 2MB prefix read) and ships
                // it on this same payload — use it so info/Remix don't wait on the workflow fetch.
                if let Some(meta) = v.get("meta").map(item_meta_from_gate) {
                    if !meta.is_empty() {
                        let _ = tx.send(Msg::ItemMeta { key: key.clone(), meta: Box::new(meta) });
                    }
                }
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
        if !view.lora.is_empty() {
            query.push(("lora", view.lora.as_str()));
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

    /// Fetch and decode a gallery thumbnail at `size` (clamped 64..=1024).
    /// When `cache_root` has the full image, downscale locally and skip the network.
    pub fn fetch_thumb(
        &self,
        subfolder: String,
        filename: String,
        size: u32,
        cache_root: Option<String>,
    ) {
        let key = format!("{subfolder}/{filename}");
        let thumb_key = format!("{key}#{size}");
        let size_s = size.to_string();
        // Resolved eagerly (borrows self); the task uses it only on a cache miss.
        let net = self.authed_url(
            "/gallery/api/thumb",
            &[("subfolder", &subfolder), ("filename", &filename), ("size", &size_s)],
        );
        let (tx, ctx, _log) = self.emitters();
        self.rt.spawn(async move {
            // Cache-hit path off the UI thread: with a fully cached library every claim used to
            // read + decode a multi-MB PNG synchronously (~13ms per tile — measured as the whole
            // scroll-time frame cost).
            if let Some(root) = cache_root {
                let key2 = key.clone();
                let cached = tokio::task::spawn_blocking(move || {
                    crate::gallery::read_full_cache(&root, &key2)
                        .and_then(|bytes| decode_thumb(&bytes, size))
                })
                .await
                .ok()
                .flatten();
                if let Some(image) = cached {
                    let _ = tx.send(Msg::Thumb { key: thumb_key, image });
                    ctx.request_repaint();
                    return;
                }
            }
            let Some((http, url)) = net else { return };
            // Clamped the same way the cache branch is. `?size=` is a REQUEST, not a guarantee —
            // anything the server won't or can't downscale (an animated format, a poster frame, an
            // error that falls back to the original) comes back full size, and storing that under
            // a `#320` key put multi-megabyte entries in a 64 MB cache whose keys all claim to be
            // 0.4 MB. A screenful of those exceeds the budget on its own, so every insert evicted a
            // tile that was still on screen and the grid re-downloaded it forever. A no-op when the
            // server did honour the size, and off-thread because a full-size PNG decode is tens of
            // ms of pure CPU that would otherwise block a tokio worker.
            if let Ok(bytes) = get_ok_bytes(&http, url).await
                && let Some(image) =
                    tokio::task::spawn_blocking(move || decode_thumb(&bytes, size))
                        .await
                        .ok()
                        .flatten()
            {
                let _ = tx.send(Msg::Thumb { key: thumb_key, image });
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
                    // Keyed first, so a waiter (the viewer, a node's file pick) can tell whether
                    // THIS download is the one that failed; then the visible gallery message.
                    let _ = tx.send(Msg::FullImageError {
                        key: format!("{subfolder}/{filename}"),
                        why: e.clone(),
                    });
                    let _ = tx.send(Msg::GalleryError(e));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Fetch a server input image (for the node file pickers), decoded and cached under the key
    /// `input#<filename>`.
    ///
    /// Downscaled to [`INPUT_THUMB_PX`] like every other thumbnail. `/view` hands back the
    /// original file — a phone photo uploaded as a LoadImage input is 4000x3000, which decodes to
    /// **48 MB of RGBA**, and this used to store that whole thing in the 64 MB [`ThumbCache`].
    /// One such entry evicted every gallery tile around it, the tiles re-fetched, the re-fetches
    /// pushed it out again: the picker sat there reloading forever. Nothing displays these above
    /// 156px (the canvas node footer).
    ///
    /// [`ThumbCache`]: crate::gallery::ThumbCache
    pub fn fetch_input_thumb(&self, filename: String) {
        // The node's selection is whatever ComfyUI put in the widget, and that is rarely a bare
        // name: a comfy-gate upload leaves `<namespace>/name.png`, and a workflow loaded from a
        // gallery PNG can carry `clipspace/foo.png [input]`. `/view` basenames whatever it is
        // handed, so passing the decorated string as `filename` with an empty `subfolder` looks in
        // `input/` for a file that lives in `input/<ns>/` — a 404, no `Msg::Thumb`, and a claim
        // that never clears, i.e. a preview that is blank forever. The cache key keeps the full
        // undecorated selection so all three lookup sites still agree.
        let (subfolder, file) = split_input_ref(&filename);
        let Some((http, url)) = self.authed_url(
            "/view",
            &[("type", "input"), ("subfolder", &subfolder), ("filename", &file)],
        ) else {
            return;
        };
        let (tx, ctx, _log) = self.emitters();
        self.rt.spawn(async move {
            if let Ok(bytes) = get_ok_bytes(&http, url).await
                && let Some(image) =
                    tokio::task::spawn_blocking(move || decode_thumb(&bytes, INPUT_THUMB_PX))
                        .await
                        .ok()
                        .flatten()
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
            let _ = self.tx.send(Msg::FullImageError { key, why: "not connected".into() });
            self.ctx.request_repaint();
            return;
        };
        let (tx, ctx, log) = self.emitters();
        self.rt.spawn(async move {
            match get_ok_bytes(&http, url).await {
                Ok(bytes) => {
                    // Cache even undecodable files (e.g. animated webp) so they never re-download.
                    if let Some(dir) = cache_dir.as_ref() {
                        crate::gallery::write_full_cache(dir, &key, &bytes);
                    }
                    if let Some(image) = decode(&bytes) {
                        let _ = tx.send(Msg::FullImage { key, image, bytes });
                    } else {
                        log.warn(format!("full image {key}: decode failed ({} bytes)", bytes.len()));
                        let _ = tx.send(Msg::FullImageError { key, why: "image decode failed".into() });
                    }
                }
                Err(e) => {
                    log.error(format!("full image {key}: {e}"));
                    let _ = tx.send(Msg::FullImageError { key, why: e });
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

/// Map comfy-gate's `/gallery/api/meta` `meta` object onto the viewer's [`ImageMeta`]. The gate
/// scrapes positive/negative/model/loras/sampler/seed/steps/cfg server-side; LoRA strengths and
/// encoder details aren't in the payload, so those slots stay default until the workflow lands.
fn item_meta_from_gate(v: &Value) -> crate::gallery::ImageMeta {
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    crate::gallery::ImageMeta {
        models: s("model").into_iter().collect(),
        loras: v
            .get("loras")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(gate_lora_meta).collect())
            .unwrap_or_default(),
        positive: s("positive"),
        negative: s("negative"),
        sampler: s("sampler"),
        scheduler: s("scheduler"),
        steps: v.get("steps").and_then(Value::as_u64),
        cfg: v.get("cfg").and_then(Value::as_f64),
        // Seeds run to 64 bits; the gate ships them lossless as raw JSON numbers, which
        // serde_json stores as u64 above i64::MAX — bit-cast so the value round-trips into
        // the u64 Params slot instead of vanishing.
        seed: v
            .get("seed")
            .and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|u| u as i64))),
        ..Default::default()
    }
}

/// The gate serializes each LoRA as `"file (strength)"` — split the suffix back out so a
/// remix from this summary doesn't ask the server for a file literally named `"x (0.85)"`
/// loaded at strength 0.
fn gate_lora_meta(s: &str) -> crate::gallery::LoraMeta {
    if let Some((name, tail)) = s.rsplit_once(" (")
        && let Some(num) = tail.strip_suffix(')')
        && let Ok(strength) = num.parse::<f64>()
    {
        return crate::gallery::LoraMeta {
            name: name.to_string(),
            strength_model: strength,
            ..Default::default()
        };
    }
    crate::gallery::LoraMeta { name: s.to_string(), ..Default::default() }
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
            // Numeric COMBOs survive as numbers here; the editor-load path retypes them for the
            // dropdown, and the queue path retypes them back.
            .map(|workflow| uiwf::Converted {
                workflow,
                warnings: Vec::new(),
                seed_randomize: Default::default(),
                extra_widgets: Default::default(),
            })
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
                seed_randomize: c.seed_randomize,
                extra_widgets: c.extra_widgets,
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
/// Lowercase hex of a digest, to compare against the gate's recorded sha256.
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Refuse a gate payload whose wire version we don't understand rather than mis-parsing it.
/// `0` is what a missing field deserialises to — an older gate that predates the field — and is
/// accepted, because every response that carries data also carries `version: 1`.
fn wire_ok(version: u32, what: &str) -> Result<(), String> {
    if version == 0 || version == GATE_WIRE_VERSION {
        return Ok(());
    }
    Err(format!(
        "This server's {what} speaks version {version}; this app understands {GATE_WIRE_VERSION}. Update the app."
    ))
}

/// Turn a transport error string from `get_ok_text` into something worth showing a user. The
/// managed-library endpoints only exist on a current gate, so a 404 is a deployment answer, not a
/// bug in the request.
fn lora_http_hint(e: &str) -> String {
    if e.contains("404") {
        "This server has no LoRA manager — update comfy-gate".to_string()
    } else if e.contains("401") {
        "Not signed in".to_string()
    } else if e.contains("403") {
        "This account can't manage models".to_string()
    } else if e.contains("502") {
        "LoRA Manager isn't responding on the server".to_string()
    } else {
        e.to_string()
    }
}

/// The same mapping for a write, which answers with a status rather than a transport error.
fn download_status_hint(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        401 => "Not signed in".to_string(),
        403 => "This account can't download models — admin only".to_string(),
        404 => "This server has no download API — update comfy-gate".to_string(),
        502 => "LoRA Manager isn't responding on the server".to_string(),
        _ => format!("HTTP {status}"),
    }
}

async fn get_ok_bytes(http: &reqwest::Client, url: reqwest::Url) -> Result<Vec<u8>, String> {
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn run_generate(
    client: Client,
    params: Params,
    current: Option<Vec<u8>>,
    gcx: GenCtx,
    ui_workflow: Option<Value>,
    label: String,
    current_prompt: CurrentPrompt,
    authed: Option<(String, reqwest::Client)>,
    queue_authed: Option<(String, reqwest::Client)>,
    tx: Sender<Msg>,
    ctx: egui::Context,
    log: Logger,
) {
    // Resolve and upload the img2img / video start image, if any.
    let wants_input = params.mode == Mode::Img2Img
        || (params.mode == Mode::Video && !params.video.video_t2v);
    let input_image = if wants_input {
        let bytes = match params.img2img_source {
            Img2ImgSource::CurrentOutput | Img2ImgSource::Picked => current,
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
            let _ = tx.send(Msg::GenError("No input image selected".into()));
            ctx.request_repaint();
            return;
        };
        // N queued variants upload identical bytes to the fixed INPUT_IMAGE_NAME; the overwrite is benign.
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

    let (mut wf, _out, report) =
        workflow::build_dispatch(&params, input_image, &gcx.apps, &gcx.schemas);
    for n in workflow::sanitize_clip_types(&mut wf, &gcx.schemas) {
        log.info(format!("repair: {n}"));
    }
    let note = report.note();
    if !note.is_empty() {
        log.info(format!("enhance: {note}"));
        let _ = tx.send(Msg::EnhanceNote(note));
    }
    // Video POSTs need the long queue timeout even with no linked graph (the gate holds the
    // request open while expanding); image jobs keep the faster streaming path.
    let force_queue_post = params.mode == Mode::Video;
    stream_execution(client, wf, ui_workflow, gcx.schemas, authed, queue_authed, force_queue_post, label, tx, ctx, log, current_prompt).await;
}

/// Upload the colour-match reference (when any), build the finish graph, and stream it. Mirrors
/// [`run_generate`]'s upload-then-queue path; the finished video lands in the gallery via VHS.
#[allow(clippy::too_many_arguments)]
async fn run_finish_job(
    client: Client,
    video_path: String,
    reference: Option<Vec<u8>>,
    scale_by: f32,
    rife_multiplier: u32,
    output_fps: u32,
    schemas: Arc<SchemaSet>,
    authed: Option<(String, reqwest::Client)>,
    queue_authed: Option<(String, reqwest::Client)>,
    label: String,
    tx: Sender<Msg>,
    ctx: egui::Context,
    log: Logger,
    current_prompt: CurrentPrompt,
) {
    let reference_name = if let Some(bytes) = reference {
        log.info(format!("uploading finish reference ({} bytes)", bytes.len()));
        match client
            .upload_image(INPUT_IMAGE_NAME, bytes, rucomfyui::upload::UploadType::Input, true)
            .await
        {
            Ok(resp) => Some(if resp.subfolder.is_empty() {
                resp.name
            } else {
                format!("{}/{}", resp.subfolder, resp.name)
            }),
            Err(e) => {
                log.error(format!("finish reference upload failed: {e}"));
                let _ = tx.send(Msg::GenError(format!("Upload failed: {e}")));
                ctx.request_repaint();
                return;
            }
        }
    } else {
        None
    };

    let (wf, report) = workflow::build_finish(
        &video_path,
        reference_name.as_deref(),
        scale_by,
        rife_multiplier,
        output_fps,
        &schemas,
    );
    let note = report.note();
    if !note.is_empty() {
        log.info(format!("finish: {note}"));
        let _ = tx.send(Msg::EnhanceNote(note));
    }
    // The finish graph carries no positive prompt, so no queue-time expansion holds the request.
    stream_execution(client, wf, None, schemas, authed, queue_authed, false, label, tx, ctx, log, current_prompt).await;
}

/// The gate's queue-time expansion of the positive prompt: node id -> expanded text.
type CgExpanded = std::collections::BTreeMap<String, String>;

/// What `POST /prompt` answered: the assigned id, the gate's queue-time prompt rewrite, and the
/// per-node validation failures ComfyUI reports on an otherwise-accepted prompt.
struct Queued {
    prompt_id: String,
    expanded: Option<CgExpanded>,
    node_errors: Vec<String>,
}

/// One line per node ComfyUI refused, from a `node_errors` map. A 200 response carrying these has
/// dropped those output branches and will run the rest.
fn node_error_lines(v: &Value) -> Vec<String> {
    let Some(map) = v.get("node_errors").and_then(Value::as_object) else { return Vec::new() };
    map.iter()
        .map(|(id, e)| {
            let class = e.get("class_type").and_then(Value::as_str).unwrap_or("?");
            let why: Vec<String> = e
                .get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|x| {
                    let msg = x.get("message").and_then(Value::as_str).unwrap_or("invalid");
                    match x.get("details").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                        Some(d) => format!("{msg} ({d})"),
                        None => msg.to_string(),
                    }
                })
                .collect();
            format!("{class} (node {id}): {}", why.join(", "))
        })
        .collect()
}

/// Read the shared fields out of a `POST /prompt` body.
fn parse_queue_response(value: &Value, text: &str) -> Result<Queued, String> {
    let prompt_id = value
        .get("prompt_id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("queue: no prompt_id in {}", head(text, 200)))?
        .to_string();
    let expanded = value
        .get("cg_expanded")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<CgExpanded>()
        })
        .filter(|m| !m.is_empty());
    Ok(Queued { prompt_id, expanded, node_errors: node_error_lines(value) })
}

/// POST `/prompt` with `extra_pnginfo.workflow` so `SaveImage` embeds the UI JSON in the PNG.
///
/// When `queue_authed` is present the POST goes through it directly (long read timeout, so the
/// gate's up-to-90 s queue-time expander can't trip the browsing timeout) and the full response is
/// read so the `cg_expanded` rewrite can be surfaced. Without it, falls back to rucomfyui's
/// `post_json` (browsing timeout, no `cg_expanded`) — the case where no connection was live.
async fn queue_prompt_with_workflow_meta(
    client: &Client,
    queue_authed: Option<&(String, reqwest::Client)>,
    wf: &Workflow,
    ui_workflow: &Value,
    log: &Logger,
) -> Result<Queued, String> {
    let body = serde_json::json!({
        "prompt": wf,
        "client_id": client.client_id(),
        "extra_data": {
            "extra_pnginfo": {
                "workflow": ui_workflow
            }
        }
    });
    let Some((base, http)) = queue_authed else {
        let value: Value = client
            .post_json("prompt", &body)
            .await
            .map_err(|e| format!("queue failed: {e}"))?;
        let queued = parse_queue_response(&value, &value.to_string())?;
        log.info(format!("queued with workflow meta: {}", queued.prompt_id));
        return Ok(queued);
    };

    let resp = http
        .post(format!("{base}/prompt"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("queue failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("queue read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("queue failed: HTTP {status}: {}", head(&text, 200)));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("queue: bad JSON ({e}): {}", head(&text, 200)))?;
    let queued = parse_queue_response(&value, &text)?;
    log.info(format!(
        "queued with workflow meta: {}{}",
        queued.prompt_id,
        if queued.expanded.is_some() { " (prompt expanded)" } else { "" }
    ));
    Ok(queued)
}

/// The ids of every node in `wf` the server treats as an output (`SaveImage`, `VHS_VideoCombine`,
/// and the utility nodes that also declare `output_node`).
fn output_node_ids(wf: &Workflow, schemas: &SchemaSet) -> Vec<u32> {
    wf.0.iter()
        .filter(|(_, n)| schemas.nodes.get(&n.class_type).is_some_and(|s| s.output_node))
        .map(|(id, _)| id.0)
        .collect()
}

/// The queued outputs of `wants` the server did NOT schedule, or `None` when the prompt's queue
/// entry could not be read (it already finished, or `/queue` is unavailable). Queue entries are
/// `[number, prompt_id, prompt, extra_data, outputs_to_execute]`.
async fn unscheduled_outputs(
    authed: &Option<(String, reqwest::Client)>,
    prompt_id: &str,
    wants: &[u32],
) -> Option<Vec<u32>> {
    let (base, http) = authed.as_ref()?;
    let resp = http.get(format!("{base}/queue")).send().await.ok()?;
    let v: Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
    let entry = ["queue_running", "queue_pending"]
        .iter()
        .filter_map(|k| v.get(*k)?.as_array())
        .flatten()
        .find(|e| e.get(1).and_then(Value::as_str) == Some(prompt_id))?;
    let scheduled: Vec<String> = entry
        .get(4)?
        .as_array()?
        .iter()
        .map(|o| o.as_str().map(str::to_string).unwrap_or_else(|| o.to_string()))
        .collect();
    Some(wants.iter().copied().filter(|id| !scheduled.contains(&id.to_string())).collect())
}

/// Say so, loudly, when the server accepted a prompt but will not run all of its outputs. ComfyUI
/// validates per output node: with only some of them valid it answers 200, drops the rest, and
/// executes the remainder — a job that holds the GPU for twenty minutes and writes no file.
///
/// Detached, because it costs a `/queue` round trip and the caller has an event stream to read.
fn warn_dropped_outputs(
    authed: Option<(String, reqwest::Client)>,
    prompt_id: String,
    wants: Vec<u32>,
    node_errors: Vec<String>,
    tx: Sender<Msg>,
    ctx: egui::Context,
    log: Logger,
) {
    if wants.is_empty() && node_errors.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let missing = unscheduled_outputs(&authed, &prompt_id, &wants).await.unwrap_or_default();
        if missing.is_empty() && node_errors.is_empty() {
            return;
        }
        let why = if node_errors.is_empty() {
            String::new()
        } else {
            format!(" — {}", node_errors.join("; "))
        };
        let note = if missing.is_empty() {
            format!("Server rejected part of this graph{why}")
        } else {
            let ids: Vec<String> = missing.iter().map(u32::to_string).collect();
            format!("Output node(s) {} will not run — this job saves nothing{why}", ids.join(", "))
        };
        log.error(format!("queued but incomplete: {note}"));
        let _ = tx.send(Msg::OutputsDropped(note));
        ctx.request_repaint();
    });
}

/// Pull `choices[0].delta.content` out of one SSE `data:` payload (OpenAI streaming shape).
fn parse_sse_delta(data: &str) -> Option<String> {
    let v: Value = serde_json::from_str(data).ok()?;
    let content = v.get("choices")?.get(0)?.get("delta")?.get("content")?.as_str()?;
    (!content.is_empty()).then(|| content.to_string())
}

/// Resolve once `cancel` is set, so a blocking read can be raced against it in `select!`.
async fn wait_cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Parse one SSE line and forward its token. Returns true on the terminal `data: [DONE]`.
fn handle_sse_line(line: &[u8], tx: &Sender<ExpandMsg>, ctx: &egui::Context) -> bool {
    let line = String::from_utf8_lossy(line);
    let Some(data) = line.trim().strip_prefix("data:") else { return false };
    let data = data.trim();
    if data == "[DONE]" {
        let _ = tx.send(ExpandMsg::Done);
        ctx.request_repaint();
        return true;
    }
    if let Some(tok) = parse_sse_delta(data) {
        let _ = tx.send(ExpandMsg::Delta(tok));
        ctx.request_repaint();
    }
    false
}

/// `POST /api/expand`'s request body. `dialect` is a family key (`wan-i2v`, `illustrious`, …) or
/// the workflow's loader filename for the gate to classify; it's omitted when empty rather than
/// sent blank, so an app that can't name a family gets the gate's generic prompt. Older gates
/// ignore the field, and a value they don't know falls back to that same generic prompt — a
/// dialect hint is never an error.
fn expand_body(text: &str, dialect: &str) -> serde_json::Value {
    let mut body = serde_json::json!({ "text": text });
    let dialect = dialect.trim();
    if !dialect.is_empty() {
        body["dialect"] = serde_json::Value::String(dialect.to_string());
    }
    body
}

/// `POST /api/variations`' request body. `count` is clamped here as well as server-side so the
/// number the UI promised is the number asked for; `dialect`, `strength` and `keep` are omitted
/// when empty so the server applies its own defaults (generic dialect, `moderate`, no extra
/// anchors) instead of being handed a blank.
fn variations_body(req: &VariationsReq) -> serde_json::Value {
    let mut body = serde_json::json!({
        "text": req.text,
        "count": req.count.clamp(1, 6),
    });
    let dialect = req.dialect.trim();
    if !dialect.is_empty() {
        body["dialect"] = Value::String(dialect.to_string());
    }
    let strength = req.strength.trim();
    if !strength.is_empty() {
        body["strength"] = Value::String(strength.to_string());
    }
    let keep: Vec<Value> = req
        .keep
        .iter()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .map(|k| Value::String(k.to_string()))
        .collect();
    if !keep.is_empty() {
        body["keep"] = Value::Array(keep);
    }
    body
}

/// Why `/api/variations` said no, and whether that verdict is about the whole server (`true` →
/// [`VariationsMsg::Unsupported`], stop offering it) or just this attempt. 501 is deliberately the
/// latter: it means this *dialect* has no variation prompt, so another checkpoint may still work.
fn variations_status_hint(status: u16) -> (String, bool) {
    match status {
        400 => ("The server rejected this prompt".into(), false),
        401 | 403 => ("Not signed in — sign in and try again".into(), false),
        404 => ("This server has no prompt variations".into(), true),
        501 => ("This model family has no variations prompt".into(), false),
        502 => ("Every variation failed — keeping your prompt".into(), false),
        503 => ("Prompt expansion is switched off on the server".into(), true),
        504 => ("The prompt expander timed out".into(), false),
        other => (format!("Variations unavailable (HTTP {other})"), false),
    }
}

/// `POST /api/variations` and hand back the parsed body. One request, one answer: the endpoint
/// doesn't stream (the options are meant to be compared side by side), so `cancel` races the whole
/// request — dropping the future aborts it — rather than stopping between chunks.
async fn fetch_variations(
    http: reqwest::Client,
    base: String,
    req: VariationsReq,
    cancel: Arc<AtomicBool>,
    tx: Sender<VariationsMsg>,
    ctx: egui::Context,
    log: Logger,
) {
    macro_rules! send {
        ($m:expr) => {{
            let _ = tx.send($m);
            ctx.request_repaint();
        }};
    }
    let body = variations_body(&req);
    let sent = http.post(format!("{base}/api/variations")).json(&body).send();
    let resp = tokio::select! {
        biased;
        _ = wait_cancelled(&cancel) => return,
        r = sent => r,
    };
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log.warn(format!("variations request failed: {e}"));
            send!(VariationsMsg::Error(format!("variations: {e}")));
            return;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let (hint, unsupported) = variations_status_hint(status.as_u16());
        log.warn(format!("variations unavailable: HTTP {status}"));
        send!(if unsupported {
            VariationsMsg::Unsupported(hint)
        } else {
            VariationsMsg::Error(hint)
        });
        return;
    }
    let parsed = tokio::select! {
        biased;
        _ = wait_cancelled(&cancel) => return,
        j = resp.json::<Variations>() => j,
    };
    match parsed {
        Ok(v) => {
            // Each option is its own completion run back to back, so the summed server time is
            // what the user waited for — worth having in the log when a set feels slow.
            let ms: u64 = v.variations.iter().filter_map(|x| x.elapsed_ms).sum();
            log.info(format!(
                "variations: {} option(s), {} error(s), dialect '{}', {:.1}s server time",
                v.variations.len(),
                v.errors.len(),
                v.dialect,
                ms as f32 / 1000.0
            ));
            // A 200 with nothing usable is still a failure for the caller; the gate documents an
            // empty list as a 502, so this is a defensive path, not the expected one.
            if v.variations.iter().all(|x| x.text.trim().is_empty()) {
                send!(VariationsMsg::Error("The server returned no variations".into()));
            } else {
                send!(VariationsMsg::Done(v));
            }
        }
        Err(e) => {
            log.warn(format!("variations parse failed: {e}"));
            send!(VariationsMsg::Error(format!("variations: {e}")));
        }
    }
}

/// Why `/api/expand` said no, in words worth showing in the review modal. The gate documents 400
/// (no text), 502 (expander unreachable), 503 (expansion disabled) and 504 (expander timed out);
/// a 404 means the server has no expander at all — a plain ComfyUI, or a gate older than the
/// endpoint — which is worth saying differently from a broken one.
fn expand_status_hint(status: u16) -> String {
    match status {
        400 => "The server rejected this prompt".into(),
        401 | 403 => "Not signed in — sign in and try again".into(),
        404 => "This server has no prompt expander".into(),
        502 => "The prompt expander is unreachable".into(),
        503 => "Prompt expansion is switched off on the server".into(),
        504 => "The prompt expander timed out".into(),
        other => format!("Expand unavailable (HTTP {other})"),
    }
}

/// Stream `POST /api/expand`, forwarding each `choices[].delta.content` token as
/// [`ExpandMsg::Delta`] until `data: [DONE]` (or the stream closes cleanly). Parses OpenAI-style
/// SSE a line at a time. A non-200 (502/503/504 = expander down/disabled/timed out) or transport
/// error arrives as a single [`ExpandMsg::Error`]; the caller then submits the original text
/// unchanged. `cancel` (set when the user dismisses the review) stops between chunks — dropping the
/// response future aborts the in-flight request.
#[allow(clippy::too_many_arguments)]
async fn stream_expand(
    http: reqwest::Client,
    base: String,
    text: String,
    dialect: String,
    cancel: Arc<AtomicBool>,
    tx: Sender<ExpandMsg>,
    ctx: egui::Context,
    log: Logger,
) {
    macro_rules! send {
        ($m:expr) => {{
            let _ = tx.send($m);
            ctx.request_repaint();
        }};
    }
    let body = expand_body(&text, &dialect);
    let resp = match http.post(format!("{base}/api/expand")).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            log.warn(format!("expand request failed: {e}"));
            send!(ExpandMsg::Error(format!("expand: {e}")));
            return;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        log.warn(format!("expand unavailable: HTTP {status}"));
        let hint = expand_status_hint(code);
        // 404/501/503 are "this server can't do this at all", not "this attempt failed": the app
        // takes them as a standing answer, so it stops pointing the user at a button that can't work.
        send!(if matches!(code, 404 | 501 | 503) {
            ExpandMsg::Unsupported(hint)
        } else {
            ExpandMsg::Error(hint)
        });
        return;
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // Race the read against the cancel flag so a dismiss aborts even during a long idle wait for
        // the first token (a cold expander), not only between chunks. Dropping `resp` on return
        // closes the request.
        let chunk = tokio::select! {
            biased;
            _ = wait_cancelled(&cancel) => return,
            c = resp.chunk() => c,
        };
        match chunk {
            Ok(Some(bytes)) => {
                buf.extend_from_slice(&bytes);
                // Drain whole lines; a partial trailing line stays buffered for the next chunk.
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=nl).collect();
                    if handle_sse_line(&line, &tx, &ctx) {
                        return;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                log.warn(format!("expand stream error: {e}"));
                send!(ExpandMsg::Error(format!("expand stream: {e}")));
                return;
            }
        }
    }
    // Flush a final line the server left un-terminated before closing the stream.
    let done_in_flush = !buf.is_empty() && handle_sse_line(&buf, &tx, &ctx);
    if !done_in_flush {
        send!(ExpandMsg::Done);
    }
}

/// Queue a workflow and forward its event stream to the UI. Shared by the Generate tab and the
/// graph editor. A dropped event stream (one failed poll kills rucomfyui's whole stream) falls
/// back to patiently reconciling results from the history endpoint instead of failing the run.
#[allow(clippy::too_many_arguments)]
async fn stream_execution(
    client: Client,
    mut wf: Workflow,
    ui_workflow: Option<Value>,
    schemas: Arc<SchemaSet>,
    authed: Option<(String, reqwest::Client)>,
    queue_authed: Option<(String, reqwest::Client)>,
    // Force the direct-POST path even with no UI workflow, so the long queue timeout covers video
    // jobs (the gate holds POST /prompt open while its queue-time expander runs). Image jobs leave
    // this false so plain txt2img keeps rucomfyui's lower-latency streaming transport.
    force_queue_post: bool,
    label: String,
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

    // Last stop before the wire: every producer (Create builder, graph editor, finish pass) carries
    // COMBO selections as display text, and ComfyUI membership-tests them without coercion.
    for n in crate::preflight::retype_combo_values(&mut wf, &schemas) {
        log.info(format!("retype: {n}"));
    }
    let wants_outputs = output_node_ids(&wf, &schemas);

    // Queue via our own POST when there is UI metadata to embed, or when a video job needs the long
    // queue timeout — then rely on the persistent ws_listener for progress and reconcile_from_history
    // for final images. A bare image job with nothing to embed falls through to execute()'s
    // lower-latency streaming transport instead.
    if ui_workflow.is_some() || force_queue_post {
        // Synthesized embed when no graph doc supplied one, so `workflow` is never null.
        let ui_meta = ui_workflow.unwrap_or_else(|| crate::uiwf::api_to_ui(&wf, &schemas));
        let queued = match queue_prompt_with_workflow_meta(
            &client,
            queue_authed.as_ref(),
            &wf,
            &ui_meta,
            &log,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                log.error(format!("queueing workflow failed: {e}"));
                send!(Msg::GenError(e.to_string()));
                return;
            }
        };
        let Queued { prompt_id, expanded, node_errors } = queued;
        *current_prompt.lock().unwrap() = Some(prompt_id.clone());
        send!(Msg::PromptId { id: prompt_id.clone(), label: label.clone(), expanded });
        send!(Msg::Queued);
        warn_dropped_outputs(
            authed.clone(),
            prompt_id.clone(),
            wants_outputs,
            node_errors,
            tx.clone(),
            ctx.clone(),
            log.clone(),
        );
        let outcome =
            reconcile_from_history(&client, &authed, &prompt_id, &label, &tx, &ctx, &log).await;
        *current_prompt.lock().unwrap() = None;
        match outcome {
            Ok(()) => send!(Msg::Done(label)),
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
    send!(Msg::PromptId { id: prompt_id.clone(), label: label.clone(), expanded: None });
    send!(Msg::Queued);
    // No response body on this transport, so the queue entry is the only source of truth.
    warn_dropped_outputs(
        authed.clone(),
        prompt_id.clone(),
        wants_outputs,
        Vec::new(),
        tx.clone(),
        ctx.clone(),
        log.clone(),
    );

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
                // rucomfyui downloads these without checking the HTTP status, so an auth/proxy
                // error page can arrive here as "image bytes" — drop those instead of letting the
                // graph render garbage.
                let images: Vec<Vec<u8>> = output
                    .images
                    .into_iter()
                    .filter(|b| {
                        let ok = looks_like_image(b);
                        if !ok {
                            log.error(format!(
                                "node {}: output is not an image ({} bytes: {})",
                                node.0,
                                b.len(),
                                head(&String::from_utf8_lossy(&b[..b.len().min(160)]), 120)
                            ));
                        }
                        ok
                    })
                    .collect();
                log.info(format!("node {} executed: {} image(s)", node.0, images.len()));
                send!(Msg::NodeExecuted { node: node.0, images: images.clone() });
                for bytes in images {
                    if let Some(ci) = decode(&bytes) {
                        send!(Msg::Result { image: ci, bytes, label: label.clone() });
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
        None => reconcile_from_history(&client, &authed, &prompt_id, &label, &tx, &ctx, &log).await,
    };
    *current_prompt.lock().unwrap() = None;
    match outcome {
        Ok(()) => {
            log.info("generation done");
            send!(Msg::Done(label));
        }
        Err(message) => send!(Msg::GenError(message)),
    }
}

/// The server's own exception text for a failed prompt, read out of its `/history` status
/// messages. `status_str` alone is just `"error"`, which sends the user to the ComfyUI console for
/// something the app can show verbatim — including the failing node's class, which is usually the
/// whole diagnosis (`CLIPTextEncode: ERROR: clip input is invalid: None`).
async fn execution_error_detail(
    authed: &Option<(String, reqwest::Client)>,
    prompt_id: &str,
) -> Option<String> {
    let (base, http) = authed.as_ref()?;
    let resp = http.get(format!("{base}/history/{prompt_id}")).send().await.ok()?;
    let v: Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
    Some(error_detail_from_history(&v, prompt_id)?)
}

/// The last `execution_error` message in a `/history` response, as `"<node class>: <message>"`.
fn error_detail_from_history(v: &Value, prompt_id: &str) -> Option<String> {
    let messages = v.get(prompt_id)?.pointer("/status/messages")?.as_array()?;
    let data = messages
        .iter()
        .filter(|m| m.get(0).and_then(Value::as_str) == Some("execution_error"))
        .filter_map(|m| m.get(1))
        .next_back()?;
    let msg = data.get("exception_message").and_then(Value::as_str)?.trim();
    if msg.is_empty() {
        return None;
    }
    match data.get("node_type").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(node) => Some(format!("{node}: {msg}")),
        None => Some(msg.to_string()),
    }
}

/// Poll `/history` (gently, tolerating errors) until the prompt completes, then emit its outputs.
async fn reconcile_from_history(
    client: &Client,
    authed: &Option<(String, reqwest::Client)>,
    prompt_id: &str,
    label: &str,
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
                        return Err(execution_error_detail(authed, prompt_id).await.unwrap_or_else(
                            || "execution failed on the server — see its console".into(),
                        ));
                    }
                    continue;
                }
                let mut delivered = 0usize;
                let mut first_fail: Option<String> = None;
                let mut fails = 0usize;
                let mut all_missing = true;
                for (name, node_output) in &data.outputs.nodes {
                    let Ok(node) = name.parse::<WorkflowNodeId>() else { continue };
                    let mut images = Vec::new();
                    for image in &node_output.images {
                        match fetch_output_image(client, authed, image, log).await {
                            Ok(bytes) => images.push(bytes),
                            Err(e) => {
                                log.error(format!(
                                    "output download failed ({}/{} type={}): {e}",
                                    image.subfolder, image.filename, image.image_type
                                ));
                                all_missing &= e.starts_with("HTTP 404");
                                fails += 1;
                                first_fail.get_or_insert(e);
                            }
                        }
                    }
                    delivered += images.len();
                    log.info(format!("node {} finished: {} image(s)", node.0, images.len()));
                    let _ = tx.send(Msg::NodeExecuted { node: node.0, images: images.clone() });
                    for bytes in images {
                        if let Some(ci) = decode(&bytes) {
                            let _ =
                                tx.send(Msg::Result { image: ci, bytes, label: label.to_string() });
                        }
                    }
                    ctx.request_repaint();
                }
                // Every produced image failed to fetch: fail the run so the user gets the reason
                // in a modal instead of a blank/broken node preview. (Partial failures only log.)
                if delivered == 0 && fails > 0 {
                    // All-404 means the server "finished" from its node cache but the cached
                    // files are gone (deleted from the gallery): an unchanged re-run of a
                    // fixed-seed workflow reproduces exactly this. A new seed re-generates.
                    let hint = if all_missing {
                        "\n\nThe server answered from its cache with files that no longer exist \
                         (deleted outputs?). Change the seed and run again to re-generate."
                    } else {
                        ""
                    };
                    return Err(format!(
                        "{fails} output download(s) failed — first: {}{hint}",
                        first_fail.unwrap_or_default()
                    ));
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

/// Fetch one history output via `/api/view`, with a properly encoded query (rucomfyui's own
/// `HistoryImage::download` concatenates raw values, so a filename/subfolder with `&`, `#`, `+`
/// or `%` builds a different request than intended) and a checked response: comfy-gate answers
/// denied or missing files with a text/HTML body, which reqwest happily returns as "bytes" — the
/// graph editor then renders it as a broken image with no hint of why. Falls back to rucomfyui's
/// downloader when no authed client is available (not expected once connected).
async fn fetch_output_image(
    client: &Client,
    authed: &Option<(String, reqwest::Client)>,
    image: &rucomfyui::history::HistoryImage,
    log: &Logger,
) -> Result<Vec<u8>, String> {
    let Some((base, http)) = authed.as_ref() else {
        return image.download(client).await.map_err(|e| e.to_string());
    };
    let mut url =
        reqwest::Url::parse(&format!("{base}/api/view")).map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("filename", &image.filename)
        .append_pair("subfolder", &image.subfolder)
        .append_pair("type", &image.image_type);
    log.info(format!("GET {url}"));
    let resp = http.get(url).send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| format!("reading body failed: {e}"))?;
    log.info(format!("-> {status} {} bytes ({mime})", bytes.len()));
    if !status.is_success() || !looks_like_image(&bytes) {
        let peek = head(&String::from_utf8_lossy(&bytes[..bytes.len().min(300)]), 200);
        return Err(format!("HTTP {status} ({mime}): {peek}"));
    }
    Ok(bytes.to_vec())
}

/// True when the bytes start with a magic number of a format the app can actually decode
/// (PNG / JPEG / WebP — see the `image` crate features). Content-type alone can't be trusted
/// and a body that fails this check would only ever render as egui's broken-image glyph.
fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || (bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
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
        _ => tls_builder(READ_TIMEOUT).build().map_err(|e| e.to_string())?,
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

/// Decode encoded image bytes and downscale so the longest edge is at most `size`.
/// Long-side pixels every `input#` thumbnail is decoded to. One size for all three consumers (the
/// canvas node footer at 156px, the picker's `now:` row at 44px, the Server grid's ~160px tiles)
/// so they share one cache entry rather than each holding its own copy of the same image.
pub(crate) const INPUT_THUMB_PX: u32 = 320;

/// Split a node's file selection into the `(subfolder, filename)` pair `/view` expects.
///
/// ComfyUI writes a file widget as `name.png`, `sub/name.png`, or either of those with a trailing
/// ` [input]` / ` [output]` / ` [temp]` annotation; comfy-gate namespaces uploads, so a pick made
/// on the phone comes back as `<user>/name.png`.
pub(crate) fn split_input_ref(sel: &str) -> (String, String) {
    let sel = sel.trim();
    // Only a real trailing `[…]` annotation is stripped — a filename may legitimately contain
    // " [" and must survive intact.
    let head = match sel.rsplit_once(" [") {
        Some((head, tail)) if tail.ends_with(']') => head,
        _ => sel,
    };
    match head.rsplit_once('/') {
        Some((sub, file)) => (sub.to_string(), file.to_string()),
        None => (String::new(), head.to_string()),
    }
}

pub(crate) fn decode_thumb(bytes: &[u8], size: u32) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let size = size.clamp(64, 1024);
    let img = img.thumbnail(size, size);
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw()))
}

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

/// Parse a `GET /queue` body into `(running, pending)` job lists, tolerating missing/extra fields.
fn parse_queue(v: &Value) -> (Vec<QueueJob>, Vec<QueueJob>) {
    (parse_queue_array(v.get("queue_running")), parse_queue_array(v.get("queue_pending")))
}

/// Map a `queue_running`/`queue_pending` array to jobs; a non-array is an empty list.
fn parse_queue_array(v: Option<&Value>) -> Vec<QueueJob> {
    v.and_then(Value::as_array)
        .map(|items| items.iter().map(parse_queue_entry).collect())
        .unwrap_or_default()
}

/// One queue entry: `[number, prompt_id, graph, ...]`. A malformed entry degrades to an unknown row.
fn parse_queue_entry(e: &Value) -> QueueJob {
    let number = e
        .get(0)
        .and_then(|n| n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)))
        .unwrap_or(0);
    let prompt_id =
        e.get(1).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| "?".into());
    // Index 2 is the prompt graph (API format). Scrape a summary so the queue sheet can show what
    // each job is, not just an opaque id. Parsed straight from the value (no string round-trip).
    let meta = e.get(2).and_then(|g| {
        let m = crate::gallery::parse_workflow_meta_value(g);
        (!m.is_empty()).then_some(m)
    });
    QueueJob { number, prompt_id, meta }
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

/// Which of `keys` a delete response's `errors` name. comfy-gate formats each line
/// `"<filename>: <why>"`, so the match is by filename — a name shared across subfolders lifts both.
fn failed_keys(keys: &[(String, String)], errors: &[String]) -> Vec<(String, String)> {
    let named: std::collections::HashSet<&str> = errors
        .iter()
        .map(|e| e.split_once(": ").map(|(f, _)| f).unwrap_or(e.as_str()))
        .collect();
    keys.iter().filter(|(_, f)| named.contains(f.as_str())).cloned().collect()
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
/// Idle-read timeout for ordinary requests: a wedged server surfaces an error instead of hanging a
/// request (and its spinner) forever.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle-read timeout for [`Engine::queue_http`]: the gate holds `POST /prompt` open up to ~90 s
/// while its queue-time expander runs, and a cold `/api/expand` can be slow to the first token —
/// both need headroom past the 30 s browsing timeout.
const QUEUE_READ_TIMEOUT: Duration = Duration::from_secs(150);

#[cfg(feature = "tls")]
fn tls_builder(read_timeout: Duration) -> reqwest::ClientBuilder {
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
    with_timeouts(reqwest::Client::builder().use_preconfigured_tls(config), read_timeout)
}

#[cfg(not(feature = "tls"))]
fn tls_builder(read_timeout: Duration) -> reqwest::ClientBuilder {
    with_timeouts(reqwest::Client::builder(), read_timeout)
}

/// Connect and idle-read timeouts so a wedged server surfaces an error instead of hanging a
/// request forever. No total timeout: big-but-flowing downloads are fine.
fn with_timeouts(builder: reqwest::ClientBuilder, read_timeout: Duration) -> reqwest::ClientBuilder {
    builder.connect_timeout(Duration::from_secs(10)).read_timeout(read_timeout)
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

    /// The symptom this exists for: `/view` basenames whatever `filename` it is given, so sending
    /// a node's whole selection with an empty `subfolder` looked in `input/` for a file that lives
    /// in `input/<ns>/`. That 404s, no `Msg::Thumb` is ever sent, and the claim never clears — a
    /// preview blank forever. Every shape ComfyUI and comfy-gate actually write has to split.
    #[test]
    fn a_node_selection_splits_into_the_pair_view_expects() {
        // comfy-gate namespaces uploads, which is every pick made from the phone.
        assert_eq!(split_input_ref("shadowbroker/IMG_4821.jpg"), ("shadowbroker".into(), "IMG_4821.jpg".into()));
        // ComfyUI's annotated form, as carried by a workflow loaded from a gallery PNG.
        assert_eq!(split_input_ref("clipspace/foo.png [input]"), ("clipspace".into(), "foo.png".into()));
        assert_eq!(split_input_ref("bare.png [output]"), (String::new(), "bare.png".into()));
        // A plain name stays a plain name, subfolder empty.
        assert_eq!(split_input_ref("ComfyUI_00042_.png"), (String::new(), "ComfyUI_00042_.png".into()));
        // Nested namespaces keep everything but the last segment as the subfolder.
        assert_eq!(split_input_ref("a/b/c.png"), ("a/b".into(), "c.png".into()));
        // A filename may legitimately contain " [" — only a real trailing annotation is stripped.
        assert_eq!(split_input_ref("render [wip].png"), (String::new(), "render [wip].png".into()));
    }

    /// The symptom this exists for: `input#` previews were decoded at full resolution, so one
    /// uploaded phone photo held tens of MB of the 64 MB `ThumbCache` and evicted every tile
    /// around it — which re-fetched, which evicted it, forever. Every path into that cache must
    /// come out bounded, and small enough that a screenful cannot approach the budget.
    #[test]
    fn an_input_preview_decodes_small_enough_to_share_the_cache() {
        let (w, h) = (1600u32, 1200u32);
        let src = image::RgbaImage::from_pixel(w, h, image::Rgba([120, 40, 200, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(src)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode");
        let full = decode(png.get_ref()).expect("full decode");
        assert_eq!(full.size, [w as usize, h as usize], "decode is deliberately full-size");

        let thumb = decode_thumb(png.get_ref(), INPUT_THUMB_PX).expect("thumb decode");
        let cap = INPUT_THUMB_PX as usize;
        assert!(thumb.size[0] <= cap && thumb.size[1] <= cap, "got {:?}", thumb.size);
        // What ThumbCache charges for it (w*h*4, as the Msg::Thumb handler computes).
        let cost = thumb.size[0] * thumb.size[1] * 4;
        let full_cost = full.size[0] * full.size[1] * 4;
        assert!(cost <= 512 * 1024, "an input preview costs {cost} bytes");
        assert!(full_cost > 20 * cost, "full-res was {full_cost} vs {cost} — the cap must bite");
    }

    /// A partial delete must lift the tombstone for the failed item only — lifting the whole
    /// request is what used to make just-deleted siblings reappear in the grid.
    #[test]
    fn failed_keys_names_only_the_items_the_server_rejected() {
        let keys = vec![
            ("u1/a".to_string(), "one.png".to_string()),
            ("u1/a".to_string(), "two.png".to_string()),
            ("u1/b".to_string(), "three.png".to_string()),
        ];
        let errors = vec!["two.png: Permission denied".to_string()];
        assert_eq!(failed_keys(&keys, &errors), vec![("u1/a".into(), "two.png".into())]);
        assert!(failed_keys(&keys, &[]).is_empty());
    }

    /// The same filename under two folders is one error line; both copies stop being hidden.
    #[test]
    fn failed_keys_lifts_every_folder_sharing_a_named_filename() {
        let keys = vec![
            ("u1/a".to_string(), "dup.png".to_string()),
            ("u1/b".to_string(), "dup.png".to_string()),
        ];
        let errors = vec!["dup.png: not found".to_string()];
        assert_eq!(failed_keys(&keys, &errors).len(), 2);
    }

    /// The gate has no graph to read the family from on this endpoint, so a present-but-blank
    /// `dialect` would be worse than none: the field is omitted unless we can say something.
    #[test]
    fn expand_body_only_carries_a_dialect_when_there_is_one() {
        let terse = "girl on a windowsill at night";
        assert_eq!(expand_body(terse, ""), serde_json::json!({ "text": terse }));
        assert_eq!(expand_body(terse, "   "), serde_json::json!({ "text": terse }));
        assert_eq!(
            expand_body(terse, " illustrious "),
            serde_json::json!({ "text": terse, "dialect": "illustrious" })
        );
        // A loader filename is a legal dialect too — the gate classifies it into a family.
        assert_eq!(
            expand_body(terse, "Illustrious/JANKU_v777.safetensors"),
            serde_json::json!({ "text": terse, "dialect": "Illustrious/JANKU_v777.safetensors" })
        );
    }

    fn vreq(count: u32, strength: &str, keep: &[&str]) -> VariationsReq {
        VariationsReq {
            text: "1girl, (pink hair:1.2), bathtub".into(),
            dialect: "illustrious".into(),
            count,
            strength: strength.into(),
            keep: keep.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Optional fields are omitted rather than sent blank (a blank would override the server's own
    /// defaults), and `count` is clamped client-side so the modal can't promise 9 and ask for 9.
    #[test]
    fn variations_body_omits_blanks_and_clamps_count() {
        let body = variations_body(&vreq(3, "wild", &["black choker", "  ", "horns"]));
        assert_eq!(
            body,
            serde_json::json!({
                "text": "1girl, (pink hair:1.2), bathtub",
                "count": 3,
                "dialect": "illustrious",
                "strength": "wild",
                "keep": ["black choker", "horns"],
            })
        );
        let mut req = vreq(99, "  ", &[]);
        req.dialect = "  ".into();
        let body = variations_body(&req);
        assert_eq!(body["count"], 6);
        assert!(body.get("dialect").is_none());
        assert!(body.get("strength").is_none());
        assert!(body.get("keep").is_none());
        assert_eq!(variations_body(&vreq(0, "", &[]))["count"], 1);
    }

    /// 404/503 are verdicts about the server (hide the button); 501 is only about the *dialect*,
    /// so another checkpoint may still have variations and the button must stay.
    #[test]
    fn variations_status_hint_only_disables_for_server_wide_failures() {
        assert!(variations_status_hint(404).1);
        assert!(variations_status_hint(503).1);
        assert!(!variations_status_hint(501).1);
        assert!(!variations_status_hint(502).1);
        assert!(!variations_status_hint(400).1);
        assert!(variations_status_hint(418).0.contains("418"));
    }

    /// `errors` entries aren't a pinned shape, so rendering must survive strings, objects with any
    /// of the usual message keys, and anything else.
    #[test]
    fn variation_error_lines_render_whatever_shape_arrives() {
        let got: Variations = serde_json::from_value(serde_json::json!({
            "variations": [{"axis": "pose", "text": "a"}],
            "errors": [
                "flat string",
                {"axis": "mood", "error": "dropped an anchor"},
                {"message": "no axis here"},
                42,
            ],
        }))
        .unwrap();
        let lines = got.error_lines();
        assert_eq!(lines[0], "flat string");
        assert_eq!(lines[1], "mood: dropped an anchor");
        assert_eq!(lines[2], "no axis here");
        assert_eq!(lines[3], "42");
        // Partial success is normal: the good option still parsed.
        assert_eq!(got.variations.len(), 1);
        assert_eq!(got.variations[0].axis, "pose");
    }

    /// A response missing every optional field must still parse — the app renders what it gets.
    #[test]
    fn variations_response_tolerates_missing_fields() {
        let got: Variations = serde_json::from_value(serde_json::json!({
            "variations": [{"text": "just text"}]
        }))
        .unwrap();
        assert!(got.dialect.is_empty());
        assert!(got.anchors.is_empty());
        assert!(got.errors.is_empty());
        assert_eq!(got.variations[0].elapsed_ms, None);
    }

    /// Every documented expander failure reads as a sentence, and an undocumented one still says
    /// which status came back rather than swallowing it.
    #[test]
    fn expand_status_hint_names_the_documented_failures() {
        for (status, want) in [
            (503u16, "switched off"),
            (502, "unreachable"),
            (504, "timed out"),
            (404, "no prompt expander"),
        ] {
            let hint = expand_status_hint(status);
            assert!(hint.contains(want), "HTTP {status} said {hint:?}");
        }
        assert!(expand_status_hint(418).contains("418"));
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

    /// A realistic `/queue` body: one running job, two pending, and a malformed entry that must
    /// degrade to an unknown row rather than fail the whole parse.
    #[test]
    fn parse_queue_reads_running_pending_and_tolerates_junk() {
        let body = serde_json::json!({
            "queue_running": [
                [7, "run-aaaa", {"3": {"class_type": "KSampler"}}]
            ],
            "queue_pending": [
                [8, "pend-bbbb", {"nodes": []}],
                [9.0, "pend-cccc"],
                {"oops": "not an array"}
            ]
        });
        let (running, pending) = parse_queue(&body);
        assert_eq!(running, vec![QueueJob { number: 7, prompt_id: "run-aaaa".into(), meta: None }]);
        assert_eq!(
            pending,
            vec![
                QueueJob { number: 8, prompt_id: "pend-bbbb".into(), meta: None },
                QueueJob { number: 9, prompt_id: "pend-cccc".into(), meta: None },
                QueueJob { number: 0, prompt_id: "?".into(), meta: None },
            ]
        );
    }

    /// Missing arrays, a non-object body, and empty queues all parse to empty lists, never a panic.
    #[test]
    fn parse_queue_tolerates_missing_and_malformed_shapes() {
        assert_eq!(parse_queue(&serde_json::json!({})), (vec![], vec![]));
        assert_eq!(parse_queue(&serde_json::json!("nonsense")), (vec![], vec![]));
        let only_running = serde_json::json!({ "queue_running": [], "queue_pending": [] });
        assert_eq!(parse_queue(&only_running), (vec![], vec![]));
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

    /// A failed prompt's real cause lives only in the history status messages; without it the app
    /// could only say "see its console", which is where this bug hid.
    #[test]
    fn history_error_detail_names_the_node_and_the_exception() {
        let v: Value = serde_json::from_str(
            r#"{"p1": {"status": {"status_str": "error", "completed": false, "messages": [
                ["execution_start", {"prompt_id": "p1"}],
                ["execution_error", {"node_id": "7", "node_type": "CLIPTextEncode",
                    "exception_type": "RuntimeError",
                    "exception_message": "ERROR: clip input is invalid: None\n\nIf the clip is from a checkpoint loader node your checkpoint does not contain a valid clip or text encoder model."}]
            ]}}}"#,
        )
        .unwrap();
        let detail = error_detail_from_history(&v, "p1").unwrap();
        assert!(detail.starts_with("CLIPTextEncode: ERROR: clip input is invalid: None"));

        // A prompt that never errored, an unknown id, and an empty message all fall through so the
        // caller keeps its generic wording rather than reporting a blank failure.
        let ok: Value = serde_json::from_str(
            r#"{"p1": {"status": {"status_str": "success", "messages": [["execution_start", {}]]}}}"#,
        )
        .unwrap();
        assert_eq!(error_detail_from_history(&ok, "p1"), None);
        assert_eq!(error_detail_from_history(&v, "other"), None);
        let blank: Value = serde_json::from_str(
            r#"{"p1": {"status": {"messages": [["execution_error", {"exception_message": "  "}]]}}}"#,
        )
        .unwrap();
        assert_eq!(error_detail_from_history(&blank, "p1"), None);
    }

    /// No `node_type` (older ComfyUI): the message still comes through on its own.
    #[test]
    fn history_error_detail_without_a_node_type_is_the_bare_message() {
        let v: Value = serde_json::from_str(
            r#"{"p1": {"status": {"messages": [["execution_error", {"exception_message": "boom"}]]}}}"#,
        )
        .unwrap();
        assert_eq!(error_detail_from_history(&v, "p1").as_deref(), Some("boom"));
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

    /// The gate meta payload maps into ImageMeta: `"file (strength)"` LoRA strings split, and
    /// full-u64 seeds bit-cast instead of vanishing.
    #[test]
    fn gate_meta_parses_lora_suffixes_and_u64_seeds() {
        let l = gate_lora_meta("Illustrious/BentBack.safetensors (0.85)");
        assert_eq!(l.name, "Illustrious/BentBack.safetensors");
        assert!((l.strength_model - 0.85).abs() < 1e-9);
        // No suffix / unparsable suffix: whole string is the name, strength stays default.
        assert_eq!(gate_lora_meta("plain.safetensors").name, "plain.safetensors");
        assert_eq!(gate_lora_meta("odd (name)").name, "odd (name)");
        let v: Value = serde_json::json!({
            "positive": "1girl", "model": "m.safetensors",
            "loras": ["a.safetensors (0.5)"],
            "seed": 18446744073709551615u64, "steps": 25, "cfg": 4.0
        });
        let m = item_meta_from_gate(&v);
        assert_eq!(m.seed, Some(-1), "u64::MAX must bit-cast, not drop");
        assert_eq!(m.loras.len(), 1);
        assert_eq!(m.steps, Some(25));
        assert!(!m.is_empty());
    }

    /// Output fetches must reject bodies that aren't decodable images (comfy-gate error pages,
    /// JSON, videos) so they never reach the graph's image slots.
    #[test]
    fn image_sniff_accepts_supported_formats_only() {
        assert!(looks_like_image(b"\x89PNG\r\n\x1a\nrest"));
        assert!(looks_like_image(b"\xff\xd8\xff\xe0jfif"));
        assert!(looks_like_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "));
        assert!(!looks_like_image(b"401 Unauthorized"));
        assert!(!looks_like_image(b"<!DOCTYPE html><html>error</html>"));
        assert!(!looks_like_image(b"{\"error\":\"denied\"}"));
        assert!(!looks_like_image(b"RIFF\x00\x00\x00\x00AVI LIST"));
        assert!(!looks_like_image(b""));
    }
}

