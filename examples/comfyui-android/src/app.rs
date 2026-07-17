//! The Android UI: Generate (params, output), Graph (node editor over server workflows), Properties,
//! Gallery (server output browser with albums), Settings (server, API key, account), and Logs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use egui_mobile::{CreateContext, EguiApp, Haptic, Host, app, egui};
use rucomfyui_node_graph::{ComfyUiNodeGraph, NodeId, internal::FlowNodeData};

use crate::engine::{Engine, Msg};
use crate::gallery::ThumbCache;
use crate::graphview::{self, GraphView, elide};
use crate::icons;
use crate::logger::{self, Logger};
use crate::schema::{self, SchemaSet};
use crate::types::{
    Album, FALLBACK_SAMPLERS, FALLBACK_SCHEDULERS, Facets, GalleryGroup, GalleryItem, GallerySort,
    GalleryView, Img2ImgSource, Mode, Params, Settings, fallback_vec,
};

/// Gallery page size per `/gallery/api/list` request.
const GALLERY_PAGE: u64 = 60;

enum Conn {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Generate,
    Graph,
    Gallery,
    Settings,
    Logs,
}

impl Tab {
    /// Bottom navigation order, with the icon and short label for each entry.
    const BAR: &'static [(Tab, &'static str, &'static str)] = &[
        (Tab::Generate, icons::GENERATE, "Create"),
        (Tab::Graph, icons::GRAPH, "Graph"),
        (Tab::Gallery, icons::GALLERY, "Gallery"),
        (Tab::Settings, icons::SETTINGS, "Settings"),
        (Tab::Logs, icons::LOGS, "Logs"),
    ];
}

/// Panes within the Graph tab.
#[derive(PartialEq, Clone, Copy)]
enum GraphPane {
    Canvas,
    Props,
}

/// Panes within the Gallery tab.
#[derive(PartialEq, Clone, Copy)]
enum GalleryPane {
    Images,
    Albums,
}

/// Full-screen state for one opened gallery image.
struct Viewer {
    item: GalleryItem,
    /// Index into the current listing, for the filmstrip and its neighbours.
    idx: usize,
    tex: Option<egui::TextureHandle>,
    bytes: Option<Vec<u8>>,
    loading: bool,
    /// Album ids this image belongs to; `None` until `/gallery/api/meta` answers.
    albums: Option<Vec<i64>>,
}

struct ComfyApp {
    engine: Option<Engine>,
    loaded: bool,
    tab: Tab,

    log: Logger,
    log_lines: Vec<logger::Line>,
    log_cursor: u64,

    server_url: String,
    api_key: String,
    /// Account sign-in. The password is never persisted — only the session token it returns is.
    username: String,
    password: String,
    session: String,
    signing_in: bool,
    auth_note: String,
    conn: Conn,
    schemas: Option<Arc<SchemaSet>>,
    checkpoints: Vec<String>,
    samplers: Vec<String>,
    schedulers: Vec<String>,
    ckpt_filter: String,

    params: Params,
    last_saved: Option<String>,
    last_save_check: f64,

    running: bool,
    progress: (u32, u32),
    status: String,

    preview: Option<egui::TextureHandle>,
    result: Option<egui::TextureHandle>,
    result_bytes: Option<Vec<u8>>,
    save_counter: u32,
    note: String,

    graph: Option<ComfyUiNodeGraph>,
    graph_pane: GraphPane,
    graph_name: String,
    graph_status: String,
    view: GraphView,
    props_node: Option<NodeId>,
    wf_names: Vec<String>,
    wf_open: bool,
    wf_loading: bool,
    wf_filter: String,
    save_open: bool,
    save_name: String,
    saving: bool,
    add_open: bool,
    add_filter: String,
    add_pos: egui::Pos2,
    search_open: bool,
    search_filter: String,
    node_map: HashMap<u32, NodeId>,
    graph_outputs: HashMap<NodeId, Vec<Vec<u8>>>,
    executing: Option<NodeId>,
    run_seq: u64,
    /// Nodes in the running workflow and the ones seen executing, for the global progress bar.
    run_total: usize,
    run_seen: HashSet<u32>,

    gallery: Vec<GalleryItem>,
    gallery_total: u64,
    gallery_loading: bool,
    gallery_status: String,
    gallery_q: String,
    /// Query + layout of the Gallery tab (model filter, album, sort, grouping, columns).
    gallery_view: GalleryView,
    gallery_pane: GalleryPane,
    thumbs: ThumbCache,
    viewer: Option<Viewer>,
    albums: Vec<Album>,
    facets: Facets,
    album_new_name: String,
    /// The image queued for "add to album" while the picker is open.
    album_target: Option<GalleryItem>,
}

impl ComfyApp {
    fn new(_cc: &CreateContext) -> Self {
        let log = Logger::new();
        log.info("ComfyUI app start");
        Self {
            engine: None,
            loaded: false,
            tab: Tab::Generate,
            log,
            log_lines: Vec::new(),
            log_cursor: 0,
            server_url: String::new(),
            api_key: String::new(),
            username: String::new(),
            password: String::new(),
            session: String::new(),
            signing_in: false,
            auth_note: String::new(),
            conn: Conn::Disconnected,
            schemas: None,
            checkpoints: Vec::new(),
            samplers: fallback_vec(FALLBACK_SAMPLERS),
            schedulers: fallback_vec(FALLBACK_SCHEDULERS),
            ckpt_filter: String::new(),
            params: Params::default(),
            last_saved: None,
            last_save_check: 0.0,
            running: false,
            progress: (0, 0),
            status: String::new(),
            preview: None,
            result: None,
            result_bytes: None,
            save_counter: 0,
            note: String::new(),
            graph: None,
            graph_pane: GraphPane::Canvas,
            graph_name: String::new(),
            graph_status: String::new(),
            view: GraphView::default(),
            props_node: None,
            wf_names: Vec::new(),
            wf_open: false,
            wf_loading: false,
            wf_filter: String::new(),
            save_open: false,
            save_name: String::new(),
            saving: false,
            add_open: false,
            add_filter: String::new(),
            add_pos: egui::pos2(80.0, 80.0),
            search_open: false,
            search_filter: String::new(),
            node_map: HashMap::new(),
            graph_outputs: HashMap::new(),
            executing: None,
            run_seq: 0,
            run_total: 0,
            run_seen: HashSet::new(),
            gallery: Vec::new(),
            gallery_total: 0,
            gallery_loading: false,
            gallery_status: String::new(),
            gallery_q: String::new(),
            gallery_view: GalleryView::default(),
            gallery_pane: GalleryPane::Images,
            thumbs: ThumbCache::default(),
            viewer: None,
            albums: Vec::new(),
            facets: Facets::default(),
            album_new_name: String::new(),
            album_target: None,
        }
    }

    fn handle(&mut self, ctx: &egui::Context, host: &Host, m: Msg) {
        match m {
            Msg::SignedIn { username, session } => {
                self.signing_in = false;
                self.username = username;
                self.session = session;
                self.password.clear();
                self.auth_note = format!("Signed in as {}", self.username);
                host.haptic(Haptic::Success);
                // The session is a new credential: reconnect so every request carries it.
                self.connect(host);
            }
            Msg::SignedOut => {
                self.signing_in = false;
                self.session.clear();
                self.username.clear();
                self.albums.clear();
                self.auth_note = "Signed out".into();
                self.connect(host);
            }
            Msg::AuthError(e) => {
                self.signing_in = false;
                self.auth_note = elide(&e, 160);
                host.haptic(Haptic::Error);
            }
            Msg::Albums(albums) => {
                self.albums = albums;
                // An album that vanished server-side must not keep filtering the listing.
                if let Some(id) = self.gallery_view.album
                    && !self.albums.iter().any(|a| a.id == id)
                {
                    self.gallery_view.album = None;
                    self.refresh_gallery();
                }
            }
            Msg::Facets(f) => {
                // A model filter whose option disappeared would silently return nothing.
                if !self.gallery_view.model.is_empty()
                    && !f.models.iter().any(|m| m.name == self.gallery_view.model)
                {
                    self.gallery_view.model.clear();
                }
                self.facets = f;
            }
            Msg::AlbumChanged(note) => {
                self.gallery_status = note;
                self.album_target = None;
                self.engine.as_ref().unwrap().albums();
                if let Some(v) = &mut self.viewer {
                    v.albums = None;
                    self.engine
                        .as_ref()
                        .unwrap()
                        .fetch_item_albums(v.item.subfolder.clone(), v.item.filename.clone());
                }
                // An album view's contents just changed under it.
                if self.gallery_view.album.is_some() {
                    self.refresh_gallery();
                }
                host.haptic(Haptic::Success);
            }
            Msg::AlbumError(e) => {
                self.gallery_status = elide(&e, 160);
                host.haptic(Haptic::Error);
            }
            Msg::ItemAlbums { key, albums } => {
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.albums = Some(albums);
                }
            }
            Msg::Connected { schemas, checkpoints, samplers, schedulers } => {
                self.conn = Conn::Connected;
                // Albums and model facets are per-account, so they follow the credential.
                self.engine.as_ref().unwrap().albums();
                self.engine.as_ref().unwrap().facets();
                // Swap the node catalog in place so a reconnect keeps the graph canvas.
                let object_info = schema::to_object_info(&schemas);
                match &mut self.graph {
                    Some(g) => g.object_info = object_info,
                    None => self.graph = Some(ComfyUiNodeGraph::new(object_info)),
                }
                self.schemas = Some(schemas);
                if !checkpoints.is_empty() {
                    if self.params.checkpoint.is_empty()
                        || !checkpoints.contains(&self.params.checkpoint)
                    {
                        self.params.checkpoint = checkpoints[0].clone();
                    }
                    self.checkpoints = checkpoints;
                }
                if !samplers.is_empty() {
                    self.samplers = samplers;
                }
                if !schedulers.is_empty() {
                    self.schedulers = schedulers;
                }
                self.status.clear();
                host.haptic(Haptic::Success);
            }
            Msg::ConnectError(e) => {
                self.conn = Conn::Failed(e);
                host.haptic(Haptic::Error);
            }
            Msg::Queued => self.status = "Queued".into(),
            Msg::Progress { value, max } => {
                self.progress = (value, max);
                self.status = format!("Sampling {value}/{max}");
            }
            Msg::Status(s) => self.status = s,
            Msg::Preview(ci) => {
                self.preview = Some(ctx.load_texture("preview", ci, egui::TextureOptions::LINEAR));
            }
            Msg::Result { image, bytes } => {
                self.result = Some(ctx.load_texture("result", image, egui::TextureOptions::LINEAR));
                self.result_bytes = Some(bytes);
                self.preview = None;
                self.note.clear();
            }
            Msg::NodeExecuting(node) => {
                if let Some(n) = node {
                    self.run_seen.insert(n);
                }
                self.executing = node.and_then(|n| self.node_map.get(&n).copied());
                // Select the running node like ComfyUI does: it shows in Properties and (unless the
                // green executing stroke wins) gets the focus border. No auto-pan — that would
                // fight a user scrolling the canvas.
                if let Some(nid) = self.executing {
                    self.props_node = Some(nid);
                }
            }
            Msg::NodeExecuted { node, images } => {
                if let Some(&nid) = self.node_map.get(&node) {
                    self.graph_outputs.entry(nid).or_default().extend(images);
                    if let Some(g) = &mut self.graph {
                        let prefix = format!("run{}", self.run_seq);
                        g.populate_output_images(
                            &prefix,
                            self.graph_outputs.iter().map(|(k, v)| (*k, v.clone())),
                        );
                    }
                }
            }
            Msg::Done => {
                self.running = false;
                self.progress = (0, 0);
                self.executing = None;
                self.status = "Done".into();
                host.haptic(Haptic::Success);
                host.notify("ComfyUI", "Generation finished");
            }
            Msg::Cancelled => {
                self.running = false;
                self.progress = (0, 0);
                self.executing = None;
                self.preview = None;
                self.status = "Cancelled".into();
            }
            Msg::GenError(e) => {
                self.running = false;
                self.progress = (0, 0);
                self.executing = None;
                self.status = format!("Error: {e}");
                host.haptic(Haptic::Error);
            }
            Msg::Workflows(names) => {
                self.wf_loading = false;
                self.wf_names = names;
            }
            Msg::WorkflowLoaded { name, workflow, warnings } => {
                self.wf_loading = false;
                if let Some(g) = &mut self.graph {
                    self.graph_outputs.clear();
                    self.node_map.clear();
                    self.executing = None;
                    self.props_node = None;
                    self.view.reset();
                    match g.load_api_workflow(&workflow) {
                        Ok(()) => {
                            self.graph_name = name;
                            self.graph_status = if warnings.is_empty() {
                                format!("{} nodes", workflow.0.len())
                            } else {
                                format!(
                                    "{} nodes, {} warnings — see Logs",
                                    workflow.0.len(),
                                    warnings.len()
                                )
                            };
                            self.wf_open = false;
                            self.tab = Tab::Graph;
                            self.viewer = None;
                            // Compact the frontend's sprawling saved layout once sizes measure.
                            self.view.request_arrange();
                            host.haptic(Haptic::Success);
                        }
                        Err(e) => {
                            self.graph_status = format!("Load failed: {e}");
                            self.log.error(format!("graph load: {e}"));
                            host.haptic(Haptic::Error);
                        }
                    }
                }
            }
            Msg::WorkflowSaved(name) => {
                self.saving = false;
                self.save_open = false;
                self.graph_name = name.clone();
                self.graph_status = format!("Saved {}", elide(&name, 48));
                host.haptic(Haptic::Success);
            }
            Msg::WorkflowError(e) => {
                self.wf_loading = false;
                self.saving = false;
                self.graph_status = elide(&e, 200);
                host.haptic(Haptic::Error);
            }
            Msg::Gallery(page) => {
                self.gallery_loading = false;
                self.gallery_total = page.total;
                if page.offset == 0 {
                    self.gallery = page.items;
                } else {
                    self.gallery.extend(page.items);
                }
                self.gallery_status.clear();
            }
            Msg::GalleryError(e) => {
                self.gallery_loading = false;
                if let Some(v) = &mut self.viewer {
                    v.loading = false;
                }
                self.gallery_status = elide(&e, 200);
            }
            Msg::Thumb { key, image } => {
                let bytes = image.width() * image.height() * 4;
                let tex = ctx.load_texture(&key, image, egui::TextureOptions::LINEAR);
                self.thumbs.insert(key, tex, bytes);
            }
            Msg::FullImage { key, image, bytes } => {
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.tex = Some(ctx.load_texture(&key, image, egui::TextureOptions::LINEAR));
                    v.bytes = Some(bytes);
                    v.loading = false;
                }
            }
        }
    }

    fn start_generation(&mut self, host: &Host) {
        if self.params.randomize_seed {
            self.params.seed = random_seed();
        }
        self.running = true;
        self.status = "Queued".into();
        self.progress = (0, 0);
        self.preview = None;
        self.node_map.clear();
        self.run_total = 0;
        self.run_seen.clear();
        let params = self.params.clone();
        let current = self.result_bytes.clone();
        self.engine.as_mut().unwrap().generate(params, current);
        host.haptic(Haptic::Medium);
    }

    fn queue_graph(&mut self, ctx: &egui::Context, host: &Host) {
        let Some(g) = &mut self.graph else { return };
        let (wg, mapping) = g.as_workflow_graph_with_mapping();
        let wf = wg.into_workflow();
        if wf.0.is_empty() {
            self.graph_status = "Graph is empty".into();
            return;
        }
        self.node_map = mapping.into_iter().map(|(nid, wid)| (wid.0, nid)).collect();
        self.graph_outputs.clear();
        self.run_seq += 1;
        self.run_total = wf.0.len();
        self.run_seen.clear();
        ctx.forget_all_images();
        g.populate_output_images("none", std::iter::empty());
        self.running = true;
        self.status = "Queued".into();
        self.progress = (0, 0);
        self.preview = None;
        self.executing = None;
        self.graph_status.clear();
        self.engine.as_mut().unwrap().run_workflow(wf);
        host.haptic(Haptic::Medium);
    }

    fn save_bytes(&mut self, host: &Host, bytes: &[u8], name: &str) -> String {
        let Some(dir) = host.documents_dir() else {
            return "No storage directory".into();
        };
        let folder = format!("{dir}/comfyui");
        let _ = std::fs::create_dir_all(&folder);
        let path = format!("{folder}/{name}");
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.log.info(format!("saved image: {path}"));
                host.haptic(Haptic::Success);
                format!("Saved to {path}")
            }
            Err(e) => {
                self.log.error(format!("save failed: {e}"));
                format!("Save failed: {e}")
            }
        }
    }

    fn save_result(&mut self, host: &Host) {
        let Some(bytes) = self.result_bytes.clone() else { return };
        self.save_counter += 1;
        let name = format!("output-{}.png", self.save_counter);
        self.note = self.save_bytes(host, &bytes, &name);
    }

    fn settings_path(host: &Host) -> Option<String> {
        host.documents_dir().map(|d| format!("{d}/comfyui_settings.json"))
    }

    fn settings_json(&self) -> Option<String> {
        let settings = Settings {
            server_url: self.server_url.clone(),
            api_key: self.api_key.clone(),
            username: self.username.clone(),
            session: self.session.clone(),
            params: self.params.clone(),
            gallery: self.gallery_view.clone(),
        };
        serde_json::to_string_pretty(&settings).ok()
    }

    fn load_settings(&mut self, host: &Host) {
        let Some(path) = Self::settings_path(host) else {
            return;
        };
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(saved) = serde_json::from_str::<Settings>(&text)
        {
            self.server_url = saved.server_url;
            self.api_key = saved.api_key;
            self.username = saved.username;
            self.session = saved.session;
            self.params = saved.params;
            self.gallery_view = saved.gallery;
            self.gallery_view.columns = self.gallery_view.columns.clamp(1, 3);
            self.last_saved = self.settings_json();
        }
    }

    /// Persist settings whenever they differ from the last write, checked at most once a second —
    /// the server URL and API key survive restarts even if a connect never succeeds.
    fn autosave_settings(&mut self, ctx: &egui::Context, host: &Host) {
        let now = ctx.input(|i| i.time);
        if now - self.last_save_check < 1.0 {
            return;
        }
        self.last_save_check = now;
        let Some(json) = self.settings_json() else { return };
        if self.last_saved.as_deref() == Some(&json) {
            return;
        }
        let Some(path) = Self::settings_path(host) else { return };
        if std::fs::write(&path, &json).is_ok() {
            self.last_saved = Some(json);
        }
    }

    fn connect(&mut self, host: &Host) {
        self.conn = Conn::Connecting;
        self.status.clear();
        let url = self.server_url.clone();
        let key = self.api_key.clone();
        let session = self.session.clone();
        self.engine.as_mut().unwrap().connect(url, key, session);
        host.haptic(Haptic::Light);
    }

    /// One-line connection state, shown in the bottom bar and on the Settings tab.
    fn conn_status(&self, ui: &mut egui::Ui) {
        match &self.conn {
            Conn::Connected => {
                let nodes = self.schemas.as_ref().map(|s| s.nodes.len()).unwrap_or(0);
                ui.colored_label(
                    egui::Color32::from_rgb(120, 220, 140),
                    format!("{} connected ({nodes} nodes)", icons::DOT),
                );
            }
            Conn::Failed(e) => {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 120, 120),
                    format!("{} {} — see Logs", icons::WARN, elide(e, 120)),
                );
            }
            Conn::Connecting => {
                ui.spinner();
                ui.weak("connecting…");
            }
            Conn::Disconnected => {
                ui.weak(format!("{} not connected", icons::DOT));
            }
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            ui.heading(format!("{} Server", icons::LINK));
            ui.group(|ui| {
                ui.label("URL");
                ui.add(
                    egui::TextEdit::singleline(&mut self.server_url)
                        .hint_text("https://host/api  or  http://192.168.x.x:8188")
                        .desired_width(f32::INFINITY),
                );
                ui.weak("Include any path prefix the API is served under.");
                ui.add_space(6.0);
                ui.label(format!("{} API key", icons::KEY));
                ui.add(
                    egui::TextEdit::singleline(&mut self.api_key)
                        .password(true)
                        .hint_text("optional — sent as X-Api-Key / Bearer")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let connecting = matches!(self.conn, Conn::Connecting);
                    let label = match self.conn {
                        Conn::Disconnected => "Connect",
                        Conn::Connecting => "Connecting…",
                        Conn::Connected => "Reconnect",
                        Conn::Failed(_) => "Retry",
                    };
                    let enabled = !connecting && !self.server_url.trim().is_empty();
                    let btn = egui::Button::new(label).min_size(egui::vec2(120.0, 32.0));
                    if ui.add_enabled(enabled, btn).clicked() {
                        self.connect(host);
                    }
                    self.conn_status(ui);
                });
            });

            ui.add_space(12.0);
            ui.heading(format!("{} Account", icons::USER));
            ui.group(|ui| {
                if self.session.is_empty() {
                    ui.weak(
                        "Sign in to use your own gallery and albums. An API key alone also works — \
                         both authenticate as the same user.",
                    );
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.username)
                            .hint_text("username")
                            .desired_width(f32::INFINITY),
                    );
                    let pw = ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .hint_text("password")
                            .desired_width(f32::INFINITY),
                    );
                    let submit = pw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let ready = !self.signing_in
                            && !self.server_url.trim().is_empty()
                            && !self.username.trim().is_empty()
                            && !self.password.is_empty();
                        let btn = egui::Button::new("Sign in").min_size(egui::vec2(120.0, 32.0));
                        if (ui.add_enabled(ready, btn).clicked() || (submit && ready))
                            && let Some(engine) = self.engine.as_ref()
                        {
                            self.signing_in = true;
                            self.auth_note = "Signing in…".into();
                            engine.sign_in(
                                self.server_url.clone(),
                                self.username.trim().to_string(),
                                std::mem::take(&mut self.password),
                            );
                        }
                        if self.signing_in {
                            ui.spinner();
                        }
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} Signed in as", icons::USER));
                        ui.strong(elide(&self.username, 40));
                    });
                    ui.add_space(6.0);
                    if ui.button("Sign out").clicked()
                        && let Some(engine) = self.engine.as_ref()
                    {
                        engine.sign_out(self.server_url.clone(), self.session.clone());
                    }
                }
                if !self.auth_note.is_empty() {
                    ui.add_space(4.0);
                    let signed_in = !self.session.is_empty();
                    ui.colored_label(
                        if signed_in {
                            egui::Color32::from_rgb(120, 220, 140)
                        } else {
                            egui::Color32::from_rgb(230, 180, 120)
                        },
                        elide(&self.auth_note, 160),
                    );
                }
            });
            ui.add_space(12.0);
            ui.weak("Server, key, account and generation settings save automatically.");
            ui.add_space(12.0);
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.horizontal(|ui| {
            let n = self.checkpoints.len();
            ui.label(if n > 0 { format!("Model ({n})") } else { "Model".to_string() });
            self.checkpoint_combo(ui);
        });

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.params.mode, Mode::Txt2Img, "Text to Image");
            ui.selectable_value(&mut self.params.mode, Mode::Img2Img, "Image to Image");
        });

        if self.params.mode == Mode::Img2Img {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.params.img2img_source,
                        Img2ImgSource::CurrentOutput,
                        "Current result",
                    );
                    ui.selectable_value(
                        &mut self.params.img2img_source,
                        Img2ImgSource::Url,
                        "From URL",
                    );
                });
                match self.params.img2img_source {
                    Img2ImgSource::Url => {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.params.input_url)
                                .hint_text("https://…/image.png — or pick from Gallery")
                                .desired_width(f32::INFINITY),
                        );
                    }
                    Img2ImgSource::CurrentOutput if self.result_bytes.is_none() => {
                        ui.weak("Generate an image first to use it as input.");
                    }
                    Img2ImgSource::CurrentOutput => {}
                }
                ui.add(egui::Slider::new(&mut self.params.denoise, 0.0..=1.0).text("Denoise"));
            });
        }

        ui.label("Prompt");
        ui.add(
            egui::TextEdit::multiline(&mut self.params.positive)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("what you want to see"),
        );
        ui.label("Negative");
        ui.add(
            egui::TextEdit::multiline(&mut self.params.negative)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("what to avoid"),
        );

        egui::Grid::new("params")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Steps");
                ui.add(egui::DragValue::new(&mut self.params.steps).range(1..=150));
                ui.end_row();

                ui.label("CFG");
                ui.add(egui::Slider::new(&mut self.params.cfg, 1.0..=20.0));
                ui.end_row();

                ui.label("Size");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.params.width).range(64..=2048).speed(8.0));
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut self.params.height).range(64..=2048).speed(8.0));
                });
                ui.end_row();

                ui.label("Sampler");
                combo(ui, "sampler", &mut self.params.sampler, &self.samplers);
                ui.end_row();

                ui.label("Scheduler");
                combo(ui, "scheduler", &mut self.params.scheduler, &self.schedulers);
                ui.end_row();

                ui.label("Seed");
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        !self.params.randomize_seed,
                        egui::DragValue::new(&mut self.params.seed).speed(1.0),
                    );
                    ui.checkbox(&mut self.params.randomize_seed, "random");
                });
                ui.end_row();
            });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if self.running {
                if ui.button("Cancel").clicked() {
                    self.engine.as_mut().unwrap().cancel();
                    host.haptic(Haptic::Warning);
                }
            } else {
                let can_gen = !self.params.checkpoint.is_empty();
                let btn = egui::Button::new("Generate").min_size(egui::vec2(140.0, 34.0));
                if ui.add_enabled(can_gen, btn).clicked() {
                    self.start_generation(host);
                }
            }
        });
    }

    /// Checkpoint dropdown, hardened against very large / very long server lists: the popup filters
    /// by substring and caps how many rows it lays out, so no amount of `object_info` data can
    /// overflow the GPU vertex buffer.
    fn checkpoint_combo(&mut self, ui: &mut egui::Ui) {
        let selected = if self.params.checkpoint.is_empty() {
            "—".to_string()
        } else {
            elide(&self.params.checkpoint, 40)
        };
        egui::ComboBox::from_id_salt("ckpt")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.ckpt_filter)
                        .hint_text("filter")
                        .desired_width(220.0),
                );
                let filter = self.ckpt_filter.to_lowercase();
                let mut shown = 0usize;
                let mut hidden = 0usize;
                for opt in &self.checkpoints {
                    if !filter.is_empty() && !opt.to_lowercase().contains(&filter) {
                        continue;
                    }
                    if shown >= 200 {
                        hidden += 1;
                        continue;
                    }
                    ui.selectable_value(&mut self.params.checkpoint, opt.clone(), elide(opt, 56));
                    shown += 1;
                }
                if hidden > 0 {
                    ui.weak(format!("… {hidden} more — type to filter"));
                } else if shown == 0 {
                    ui.weak("no matches");
                }
            });
    }

    fn output(&mut self, ui: &mut egui::Ui, host: &Host) {
        if self.running || !self.status.is_empty() {
            ui.add_space(6.0);
            let (v, m) = self.progress;
            if self.running && m > 0 {
                ui.add(egui::ProgressBar::new(v as f32 / m as f32).text(elide(&self.status, 300)));
            } else if self.running {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(elide(&self.status, 300));
                });
            } else {
                ui.label(elide(&self.status, 300));
            }
        }

        if let Some(tex) = &self.preview {
            image_view(ui, tex);
        }

        if self.result.is_some() {
            ui.add_space(6.0);
            ui.separator();
            if let Some(tex) = &self.result {
                image_view(ui, tex);
            }
            let mut save = false;
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    save = true;
                }
                if !self.note.is_empty() {
                    ui.weak(self.note.clone());
                }
            });
            if save {
                self.save_result(host);
            }
        }
    }

    fn generate_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let connected = matches!(self.conn, Conn::Connected)
                    || self.engine.as_ref().unwrap().is_connected();
                if !connected {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        self.conn_status(ui);
                        if ui.button(format!("{} Settings", icons::SETTINGS)).clicked() {
                            self.tab = Tab::Settings;
                        }
                    });
                    ui.separator();
                }
                ui.add_enabled_ui(connected, |ui| self.controls(ui, host));

                self.output(ui, host);
                ui.add_space(12.0);
            });
    }

    fn graph_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        let has_graph = self.graph.is_some();

        // Identity stays at the top; the controls sit at the bottom, in reach.
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.graph_pane, GraphPane::Canvas, "Canvas");
            ui.selectable_value(
                &mut self.graph_pane,
                GraphPane::Props,
                format!("{} Properties", icons::PROPS),
            );
            ui.separator();
            if !self.graph_name.is_empty() {
                ui.strong(elide(&self.graph_name, 28));
            }
            if self.running {
                ui.spinner();
                ui.label(elide(&self.status, 60));
            } else if !self.graph_status.is_empty() {
                ui.weak(elide(&self.graph_status, 90));
            }
        });
        ui.separator();

        if !has_graph {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label("Connect to a server first — the editor needs its node catalog.");
                if ui.button(format!("{} Settings", icons::SETTINGS)).clicked() {
                    self.tab = Tab::Settings;
                }
            });
            return;
        }

        egui::Panel::bottom("graph-controls").show(ui, |ui| {
            ui.add_space(2.0);
            self.graph_controls(ui, host);
            ui.add_space(2.0);
        });

        match self.graph_pane {
            GraphPane::Canvas => self.graph_canvas(ui, host),
            GraphPane::Props => self.props_tab(ui),
        }
    }

    fn graph_canvas(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.workflow_window(ui.ctx());
        self.add_node_window(ui.ctx());
        self.search_window(ui.ctx());
        self.save_window(ui.ctx());

        let g = self.graph.as_mut().unwrap();
        let preview = self
            .running
            .then(|| {
                self.preview
                    .as_ref()
                    .map(|t| egui::ImageSource::Texture(egui::load::SizedTexture::from_handle(t)))
            })
            .flatten();
        let progress = (self.running && self.progress.1 > 0).then_some(self.progress);
        g.set_live_execution(self.executing, progress, preview);
        if let Some(tapped) = self.view.show(ui, g, self.executing, self.props_node) {
            self.props_node = Some(tapped);
        }
        // Long-press on empty canvas opens the (persistent, categorized) Add node picker there.
        if let Some(pos) = self.view.take_long_press() {
            self.add_open = true;
            self.add_pos = pos - egui::vec2(90.0, 50.0);
            host.haptic(Haptic::Medium);
        }
    }

    fn graph_controls(&mut self, ui: &mut egui::Ui, host: &Host) {
        let connected = matches!(self.conn, Conn::Connected);
        let has_graph = self.graph.is_some();
        let has_nodes =
            self.graph.as_ref().is_some_and(|g| g.snarl.nodes_pos_ids().next().is_some());
        ui.horizontal_wrapped(|ui| {
            up_menu(ui, format!("{} File", icons::FOLDER), |ui| {
                if ui
                    .add_enabled(connected, egui::Button::new(format!("{} Workflows…", icons::FOLDER)))
                    .clicked()
                {
                    self.wf_open = true;
                    self.wf_loading = true;
                    self.engine.as_ref().unwrap().list_workflows();
                }
                if ui
                    .add_enabled(
                        has_nodes && connected,
                        egui::Button::new(format!("{} Save to server…", icons::SAVE)),
                    )
                    .clicked()
                {
                    self.save_open = true;
                    self.save_name = if self.graph_name.is_empty() {
                        "mobile/untitled.json".to_string()
                    } else {
                        self.graph_name.clone()
                    };
                }
                ui.separator();
                if ui
                    .add_enabled(
                        has_graph && !self.view.locked,
                        egui::Button::new(format!("{} Clear canvas", icons::TRASH)),
                    )
                    .clicked()
                {
                    self.clear_graph();
                }
            });

            up_menu(ui, format!("{} Edit", icons::ADD), |ui| {
                if ui
                    .add_enabled(
                        has_graph && !self.view.locked,
                        egui::Button::new(format!("{} Add node…", icons::ADD)),
                    )
                    .clicked()
                {
                    self.add_open = true;
                    if let Some(center) = self.view.center_in_graph() {
                        self.add_pos = center - egui::vec2(90.0, 50.0);
                    }
                }
                if ui
                    .add_enabled(has_nodes, egui::Button::new(format!("{} Find node…", icons::SEARCH)))
                    .clicked()
                {
                    self.search_open = true;
                }
                if ui
                    .add_enabled(has_nodes && !self.view.locked, egui::Button::new("Auto-arrange"))
                    .clicked()
                {
                    self.view.request_arrange();
                }
            });

            up_menu(ui, format!("{} View", icons::SEARCH), |ui| {
                if ui.add_enabled(has_nodes, egui::Button::new("Fit to screen")).clicked() {
                    self.view.request_fit();
                }
                if ui.add_enabled(has_nodes, egui::Button::new("Go to first node")).clicked() {
                    if let Some(pos) =
                        self.graph.as_ref().and_then(|g| graphview::first_node_pos(&g.snarl))
                    {
                        self.view.center_on(pos);
                    }
                }
            });

            ui.separator();
            if self.running {
                if ui.button(format!("{} Cancel", icons::STOP)).clicked() {
                    self.engine.as_mut().unwrap().cancel();
                    host.haptic(Haptic::Warning);
                }
            } else {
                let can_queue = connected && has_graph;
                let btn = egui::Button::new(format!("{} Queue", icons::RUN));
                if ui.add_enabled(can_queue, btn).clicked() {
                    self.queue_graph(ui.ctx(), host);
                }
            }
            if let Some(node) = self.props_node
                && let Some(data) = self.graph.as_ref().and_then(|g| g.snarl.get_node(node))
            {
                ui.weak(format!("{} {}", icons::DOT, elide(data.object.display_name(), 18)));
            }
        });
    }

    fn clear_graph(&mut self) {
        if let Some(g) = &mut self.graph {
            g.clear();
        }
        self.graph_name.clear();
        self.graph_status.clear();
        self.graph_outputs.clear();
        self.node_map.clear();
        self.executing = None;
        self.props_node = None;
        self.view.reset();
        self.add_pos = egui::pos2(80.0, 80.0);
    }

    fn save_window(&mut self, ctx: &egui::Context) {
        if !self.save_open {
            return;
        }
        let mut open = true;
        let mut submit = false;
        centered(egui::Window::new("Save workflow"))
            .open(&mut open)
            .collapsible(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.label("Name on the server (a new name saves a copy):");
                ui.add(
                    egui::TextEdit::singleline(&mut self.save_name)
                        .hint_text("folder/name.json")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let ready = !self.saving && !self.save_name.trim().is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Save")).clicked() {
                        submit = true;
                    }
                    if self.saving {
                        ui.spinner();
                        ui.weak("saving…");
                    } else if self.save_name.trim() == self.graph_name.trim()
                        && !self.graph_name.is_empty()
                    {
                        ui.weak("overwrites the opened workflow");
                    }
                });
            });
        if submit
            && let (Some(g), Some(schemas)) = (self.graph.as_ref(), self.schemas.as_ref())
        {
            let mut name = self.save_name.trim().trim_matches('/').to_string();
            if !name.to_lowercase().ends_with(".json") {
                name.push_str(".json");
            }
            self.save_name = name.clone();
            let exported = self.view.export_ui(g, schemas);
            match serde_json::to_string(&exported) {
                Ok(body) => {
                    self.saving = true;
                    self.graph_status = format!("Saving {}…", elide(&name, 40));
                    self.engine.as_ref().unwrap().save_workflow(name, body);
                }
                Err(e) => self.graph_status = format!("Export failed: {e}"),
            }
        }
        self.save_open = open && self.save_open;
    }

    fn search_window(&mut self, ctx: &egui::Context) {
        if !self.search_open {
            return;
        }
        let mut open = true;
        let mut jump: Option<(NodeId, egui::Pos2)> = None;
        centered(egui::Window::new("Find node"))
            .open(&mut open)
            .collapsible(false)
            .default_size([340.0, 400.0])
            .show(ctx, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_filter)
                        .hint_text("search this workflow")
                        .desired_width(f32::INFINITY),
                );
                ui.separator();
                let Some(g) = self.graph.as_ref() else { return };
                let filter = self.search_filter.to_lowercase();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let mut shown = 0usize;
                    for (id, pos, data) in g.snarl.nodes_pos_ids() {
                        let title = data.object.display_name();
                        if !filter.is_empty()
                            && !title.to_lowercase().contains(&filter)
                            && !data.object.name.to_lowercase().contains(&filter)
                        {
                            continue;
                        }
                        if shown >= 100 {
                            ui.weak("… type to narrow down");
                            break;
                        }
                        shown += 1;
                        let label = format!("{}  ({})", elide(title, 36), elide(&data.object.name, 24));
                        if ui.selectable_label(self.props_node == Some(id), label).clicked() {
                            jump = Some((id, pos));
                        }
                    }
                    if shown == 0 {
                        ui.weak("no matches");
                    }
                });
            });
        if let Some((id, pos)) = jump {
            self.props_node = Some(id);
            self.view.center_on(pos);
            open = false;
        }
        self.search_open = open;
    }

    fn props_tab(&mut self, ui: &mut egui::Ui) {
        let Some(g) = self.graph.as_mut() else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| ui.label("Connect to a server first."));
            return;
        };
        let Some(node) = self.props_node else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label("Tap a node on the canvas (or use Find) to inspect it.");
                if ui.button("Back to canvas").clicked() {
                    self.graph_pane = GraphPane::Canvas;
                }
            });
            return;
        };
        ui.horizontal(|ui| {
            if ui.button("Show in graph").clicked() {
                if let Some(info) = g.snarl.get_node_info(node) {
                    self.view.center_on(info.pos);
                }
                self.graph_pane = GraphPane::Canvas;
            }
        });
        ui.separator();
        // Deliberate edits stay possible here even when the canvas is in view-only mode.
        let mut exists = true;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            exists = graphview::node_properties(ui, g, node, false);
            ui.add_space(12.0);
        });
        if !exists {
            self.props_node = None;
        }
    }

    fn workflow_window(&mut self, ctx: &egui::Context) {
        if !self.wf_open {
            return;
        }
        let mut open = true;
        let mut picked: Option<String> = None;
        centered(egui::Window::new("Server workflows"))
            .open(&mut open)
            .collapsible(false)
            .default_size([340.0, 420.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.wf_filter)
                            .hint_text("filter")
                            .desired_width(200.0),
                    );
                    if ui.button("Refresh").clicked() {
                        self.wf_loading = true;
                        self.engine.as_ref().unwrap().list_workflows();
                    }
                    if self.wf_loading {
                        ui.spinner();
                    }
                });
                ui.separator();
                let filter = self.wf_filter.to_lowercase();
                // Group by folder (the path before the last '/'); nested folders keep their prefix.
                let mut folders: std::collections::BTreeMap<&str, Vec<&String>> =
                    std::collections::BTreeMap::new();
                for name in &self.wf_names {
                    if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                        continue;
                    }
                    let folder = name.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("Root");
                    folders.entry(folder).or_default().push(name);
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if folders.is_empty() && !self.wf_loading {
                        ui.weak(if self.wf_names.is_empty() {
                            "no workflows on server"
                        } else {
                            "no matches"
                        });
                    }
                    for (folder, names) in &folders {
                        let header =
                            format!("{} {}  ({})", icons::FOLDER, elide(folder, 40), names.len());
                        egui::CollapsingHeader::new(header)
                            .id_salt(folder)
                            .default_open(!filter.is_empty())
                            .show(ui, |ui| {
                                for name in names {
                                    let leaf =
                                        name.rsplit_once('/').map(|(_, f)| f).unwrap_or(name);
                                    if ui.selectable_label(false, elide(leaf, 52)).clicked() {
                                        picked = Some((*name).clone());
                                    }
                                }
                            });
                    }
                });
            });
        if let Some(name) = picked
            && let Some(schemas) = self.schemas.clone()
        {
            self.wf_loading = true;
            self.graph_status = format!("Loading {name}…");
            self.engine.as_ref().unwrap().open_workflow(name, schemas);
        }
        self.wf_open = open;
    }

    fn add_node_window(&mut self, ctx: &egui::Context) {
        if !self.add_open {
            return;
        }
        let mut open = true;
        let mut inserted = false;
        centered(egui::Window::new("Add node"))
            .open(&mut open)
            .collapsible(false)
            .default_size([340.0, 420.0])
            .show(ctx, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_filter)
                        .hint_text("search node types")
                        .desired_width(f32::INFINITY),
                );
                ui.separator();
                let filter = self.add_filter.to_lowercase();
                let g = self.graph.as_mut().unwrap();
                let mut pick = None;
                {
                    // Group the matching node types by category (nested categories keep their
                    // prefix), so the picker is browsable headers rather than one 2800-row list.
                    let mut cats: std::collections::BTreeMap<&str, Vec<_>> =
                        std::collections::BTreeMap::new();
                    for object in g.object_info.values() {
                        if !filter.is_empty()
                            && !object.name.to_lowercase().contains(&filter)
                            && !object.display_name().to_lowercase().contains(&filter)
                        {
                            continue;
                        }
                        let cat = if object.category.is_empty() {
                            "Uncategorized"
                        } else {
                            object.category.as_str()
                        };
                        cats.entry(cat).or_default().push(object);
                    }
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        if cats.is_empty() {
                            ui.weak("no matches");
                        }
                        for (cat, objects) in &cats {
                            let header =
                                format!("{}  ({})", elide(cat, 40), objects.len());
                            egui::CollapsingHeader::new(header)
                                .id_salt(cat)
                                // A search means the user wants to see the hits; browsing keeps
                                // categories closed so the list stays short.
                                .default_open(!filter.is_empty())
                                .show(ui, |ui| {
                                    for object in objects {
                                        if ui
                                            .selectable_label(false, elide(object.display_name(), 46))
                                            .clicked()
                                        {
                                            pick = Some((*object).clone());
                                        }
                                    }
                                });
                        }
                    });
                }
                if let Some(object) = pick {
                    g.snarl.insert_node(self.add_pos, FlowNodeData::new(object));
                    self.add_pos += egui::vec2(48.0, 48.0);
                    if self.add_pos.y > 800.0 {
                        self.add_pos = egui::pos2(120.0, 80.0);
                    }
                    inserted = true;
                }
            });
        if inserted {
            open = false;
        }
        self.add_open = open;
    }

    fn refresh_gallery(&mut self) {
        self.gallery.clear();
        self.gallery_total = 0;
        self.gallery_loading = true;
        self.gallery_status.clear();
        // Forget in-flight thumb requests so earlier failures get retried.
        self.thumbs.reset_pending();
        self.engine.as_ref().unwrap().gallery_list(0, GALLERY_PAGE, &self.gallery_q, &self.gallery_view);
    }

    /// The gallery's bottom control bar: search, model filter, sort, grouping and column count.
    /// Returns whether the listing must be re-queried — every control except the column count is
    /// applied server-side across the whole listing, not to the page already fetched.
    fn gallery_controls(&mut self, ui: &mut egui::Ui, connected: bool) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(connected, egui::Button::new(icons::REFRESH))
                .on_hover_text("Refresh")
                .clicked()
            {
                changed = true;
            }
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.gallery_q)
                    .hint_text(format!("{} search", icons::SEARCH))
                    .desired_width(ui.available_width() - 130.0),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                changed = true;
            }
            // Column count only changes layout and thumb size — no re-query.
            ui.weak("cols");
            for n in 1..=3usize {
                if ui.selectable_label(self.gallery_view.columns == n, format!("{n}")).clicked() {
                    self.gallery_view.columns = n;
                }
            }
        });
        // Filters live in a bottom bar, so their popups must open upward (`up_menu`); an
        // `egui::ComboBox` only flips a short list up when egui thinks it won't fit, and egui's
        // screen rect runs under the Android nav bar so short lists never flip.
        ui.horizontal(|ui| {
            let model_label = if self.gallery_view.model.is_empty() {
                format!("{} All models", icons::MODEL)
            } else {
                format!("{} {}", icons::MODEL, elide(&self.gallery_view.model, 16))
            };
            up_menu(ui, model_label, |ui| {
                changed |= ui
                    .selectable_value(&mut self.gallery_view.model, String::new(), "All models")
                    .clicked();
                for m in &self.facets.models {
                    let label = format!("{}  ({})", elide(&m.name, 40), m.count);
                    changed |= ui
                        .selectable_value(&mut self.gallery_view.model, m.name.clone(), label)
                        .clicked();
                }
                if self.facets.models.is_empty() {
                    ui.weak("no models indexed yet");
                }
            });

            up_menu(ui, format!("{} {}", icons::SORT, self.gallery_view.sort.label()), |ui| {
                for s in GallerySort::ALL {
                    changed |=
                        ui.selectable_value(&mut self.gallery_view.sort, *s, s.label()).clicked();
                }
            });

            up_menu(ui, self.gallery_view.group.label().to_string(), |ui| {
                for g in GalleryGroup::ALL {
                    changed |=
                        ui.selectable_value(&mut self.gallery_view.group, *g, g.label()).clicked();
                }
            });
        });
        changed
    }

    /// Albums pane: pick which album the Images pane shows, and create/rename/delete albums.
    fn albums_pane(&mut self, ui: &mut egui::Ui) {
        let mut view_changed = false;
        egui::Panel::bottom("album-controls").show(ui, |ui| {
            ui.add_space(2.0);
            let selected = self
                .gallery_view
                .album
                .and_then(|id| self.albums.iter().find(|a| a.id == id))
                .map(|a| (a.id, a.name.clone()));
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.album_new_name)
                        .hint_text("album name")
                        .desired_width(ui.available_width() - 190.0),
                );
                let named = !self.album_new_name.trim().is_empty();
                if ui
                    .add_enabled(named, egui::Button::new(format!("{} Create", icons::ADD)))
                    .clicked()
                {
                    self.engine
                        .as_ref()
                        .unwrap()
                        .album_create(self.album_new_name.trim().to_string());
                    self.album_new_name.clear();
                }
                if let Some((id, _)) = selected
                    && ui
                        .add_enabled(named, egui::Button::new("Rename"))
                        .on_hover_text("Rename the selected album")
                        .clicked()
                {
                    self.engine
                        .as_ref()
                        .unwrap()
                        .album_rename(id, self.album_new_name.trim().to_string());
                    self.album_new_name.clear();
                }
            });
            ui.add_space(2.0);
        });

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let all = self.gallery_view.album.is_none();
            if ui.selectable_label(all, format!("{} All images", icons::GALLERY)).clicked() {
                self.gallery_view.album = None;
                view_changed = true;
            }
            ui.separator();
            if self.albums.is_empty() {
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    ui.weak("No albums yet. Name one below and tap Create,");
                    ui.weak("then add images from the viewer's Albums menu.");
                });
                return;
            }
            let mut delete: Option<(i64, String)> = None;
            for a in &self.albums {
                ui.horizontal(|ui| {
                    let selected = self.gallery_view.album == Some(a.id);
                    let label = format!("{} {}  ({})", icons::ALBUM, elide(&a.name, 28), a.count);
                    if ui.selectable_label(selected, label).clicked() {
                        self.gallery_view.album = Some(a.id);
                        view_changed = true;
                    }
                    if ui.small_button(icons::TRASH).on_hover_text("Delete album").clicked() {
                        delete = Some((a.id, a.name.clone()));
                    }
                });
            }
            if let Some((id, name)) = delete {
                self.engine.as_ref().unwrap().album_delete(id, name);
                if self.gallery_view.album == Some(id) {
                    self.gallery_view.album = None;
                }
            }
        });

        // Picking an album is a different listing; show it straight away.
        if view_changed {
            self.gallery_pane = GalleryPane::Images;
            self.refresh_gallery();
        }
    }

    fn gallery_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        let connected = matches!(self.conn, Conn::Connected);
        if self.viewer.is_some() {
            self.gallery_viewer(ui, host);
            return;
        }

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.gallery_pane, GalleryPane::Images, "Images");
            ui.selectable_value(
                &mut self.gallery_pane,
                GalleryPane::Albums,
                format!("{} Albums", icons::ALBUM),
            );
            ui.separator();
            if let Some(name) = self
                .gallery_view
                .album
                .and_then(|id| self.albums.iter().find(|a| a.id == id))
                .map(|a| a.name.clone())
            {
                ui.strong(format!("{} {}", icons::ALBUM, elide(&name, 20)));
            }
            if self.gallery_loading {
                ui.spinner();
            }
            if self.gallery_total > 0 {
                ui.weak(format!("{} of {}", self.gallery.len(), self.gallery_total));
            }
        });
        if !self.gallery_status.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(230, 160, 120),
                elide(&self.gallery_status, 120),
            );
        }
        ui.separator();

        if !connected {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label("Connect to a server to browse its gallery.");
                if ui.button(format!("{} Settings", icons::SETTINGS)).clicked() {
                    self.tab = Tab::Settings;
                }
            });
            return;
        }

        if self.gallery_pane == GalleryPane::Albums {
            self.albums_pane(ui);
            return;
        }

        let mut refresh = false;
        egui::Panel::bottom("gallery-controls").show(ui, |ui| {
            ui.add_space(2.0);
            refresh = self.gallery_controls(ui, connected);
            ui.add_space(2.0);
        });
        if refresh && connected {
            self.refresh_gallery();
        }
        if self.gallery.is_empty() && self.gallery_total == 0 && !self.gallery_loading {
            self.gallery_loading = true;
            self.engine.as_ref().unwrap().gallery_list(
                0,
                GALLERY_PAGE,
                &self.gallery_q,
                &self.gallery_view,
            );
        }

        let groups = crate::gallery::group_items(&self.gallery, self.gallery_view.group);
        let cols = self.gallery_view.columns.clamp(1, 3);
        let mut open: Option<usize> = None;
        let mut load_more = false;

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for group in &groups {
                if group.label.is_empty() {
                    open = self.gallery_grid(ui, &group.items, cols).or(open);
                    continue;
                }
                let header = format!("{} ({})", elide(&group.label, 40), group.items.len());
                egui::CollapsingHeader::new(header)
                    .id_salt(&group.key)
                    .default_open(true)
                    .show(ui, |ui| {
                        open = self.gallery_grid(ui, &group.items, cols).or(open);
                    });
            }

            ui.add_space(6.0);
            if self.gallery.len() < self.gallery_total as usize {
                ui.vertical_centered(|ui| {
                    if self.gallery_loading {
                        ui.spinner();
                    } else if ui.button("Load more").clicked() {
                        load_more = true;
                    }
                });
            } else if self.gallery.is_empty() && !self.gallery_loading {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| ui.weak("Nothing matches these filters."));
            }
            ui.add_space(12.0);
        });

        if load_more {
            self.gallery_loading = true;
            self.engine.as_ref().unwrap().gallery_list(
                self.gallery.len() as u64,
                GALLERY_PAGE,
                &self.gallery_q,
                &self.gallery_view,
            );
        }
        if let Some(idx) = open {
            self.open_viewer(idx);
        }
    }

    /// Lay out `indices` as `cols` tiles per row, returning the index of any tile tapped.
    ///
    /// At one column tiles take the image's own aspect ratio (full-width reading), so the row
    /// height is only known once the thumbnail decodes; before that a 1:1 placeholder holds the
    /// space. In the grid, tiles stay square so rows line up.
    fn gallery_grid(&mut self, ui: &mut egui::Ui, indices: &[usize], cols: usize) -> Option<usize> {
        let mut open = None;
        let spacing = ui.spacing().item_spacing.x;
        let avail = ui.available_width();
        let tile = ((avail - spacing * (cols as f32 - 1.0)) / cols as f32).max(48.0);
        let size = self.gallery_view.thumb_size();

        for row in indices.chunks(cols) {
            ui.horizontal(|ui| {
                for &idx in row {
                    let Some(item) = self.gallery.get(idx) else { continue };
                    let key = item.thumb_key(size);
                    let dims = self
                        .thumbs
                        .get(&key)
                        .map(|t| t.size_vec2())
                        .filter(|s| s.x > 0.0 && s.y > 0.0);
                    let alloc = match (cols, dims) {
                        (1, Some(d)) => egui::vec2(tile, tile * d.y / d.x),
                        _ => egui::vec2(tile, tile),
                    };
                    let (rect, _) = ui.allocate_exact_size(alloc, egui::Sense::hover());
                    // Off-screen tiles keep their space but skip paint + fetch.
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    match self.thumbs.get(&key) {
                        Some(tex) => {
                            let img = egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                                .fit_to_exact_size(alloc)
                                .sense(egui::Sense::click());
                            if ui.put(rect, img).clicked() {
                                open = Some(idx);
                            }
                            if item.is_video {
                                ui.painter().text(
                                    rect.left_top() + egui::vec2(4.0, 2.0),
                                    egui::Align2::LEFT_TOP,
                                    "video",
                                    egui::FontId::proportional(12.0),
                                    egui::Color32::WHITE,
                                );
                            }
                        }
                        None => {
                            if self.thumbs.claim(&key) {
                                self.engine.as_ref().unwrap().fetch_thumb(
                                    item.subfolder.clone(),
                                    item.filename.clone(),
                                    size,
                                );
                            }
                            let btn = egui::Button::new(elide(&item.filename, 14)).wrap();
                            if ui.put(rect, btn).clicked() {
                                open = Some(idx);
                            }
                        }
                    }
                }
            });
        }
        open
    }

    fn open_viewer(&mut self, idx: usize) {
        let Some(item) = self.gallery.get(idx).cloned() else { return };
        if item.is_video {
            self.gallery_status = "Video playback isn't supported yet".into();
            return;
        }
        let engine = self.engine.as_ref().unwrap();
        engine.fetch_full(item.subfolder.clone(), item.filename.clone());
        engine.fetch_item_albums(item.subfolder.clone(), item.filename.clone());
        self.gallery_status.clear();
        self.viewer =
            Some(Viewer { item, idx, tex: None, bytes: None, loading: true, albums: None });
    }

    fn gallery_viewer(&mut self, ui: &mut egui::Ui, host: &Host) {
        enum Act {
            Close,
            Save,
            UseAsInput,
            OpenWorkflow,
            AlbumAdd(i64),
            AlbumRemove(i64),
            Show(usize),
        }
        let mut act: Option<Act> = None;
        {
            let v = self.viewer.as_ref().unwrap();
            ui.horizontal_wrapped(|ui| {
                if ui.button(format!("{} Back", icons::BACK)).clicked() {
                    act = Some(Act::Close);
                }
                if ui
                    .add_enabled(v.bytes.is_some(), egui::Button::new(format!("{} Save", icons::SAVE)))
                    .clicked()
                {
                    act = Some(Act::Save);
                }
                if ui.button(format!("{} Use as input", icons::IMAGE)).clicked() {
                    act = Some(Act::UseAsInput);
                }
                if ui
                    .add_enabled(
                        v.item.has_workflow,
                        egui::Button::new(format!("{} Open workflow", icons::GRAPH)),
                    )
                    .clicked()
                {
                    act = Some(Act::OpenWorkflow);
                }
                // Membership drives the menu, so it stays disabled until /meta answers.
                let known = v.albums.is_some();
                ui.menu_button(format!("{} Albums", icons::ALBUM), |ui| {
                    if !known {
                        ui.weak("loading…");
                        return;
                    }
                    if self.albums.is_empty() {
                        ui.weak("No albums yet — create one in the Gallery.");
                        return;
                    }
                    let member = v.albums.as_ref().unwrap();
                    for a in &self.albums {
                        let is_in = member.contains(&a.id);
                        let label = if is_in {
                            format!("{} {}", icons::CHECK, elide(&a.name, 28))
                        } else {
                            format!("     {}", elide(&a.name, 28))
                        };
                        if ui.selectable_label(is_in, label).clicked() {
                            act = Some(if is_in { Act::AlbumRemove(a.id) } else { Act::AlbumAdd(a.id) });
                            ui.close();
                        }
                    }
                });
            });
            ui.weak(format!(
                "{}  ({:.1} MB)",
                elide(&v.item.filename, 56),
                v.item.size as f64 / 1e6
            ));
            if !v.item.models.is_empty() {
                ui.weak(format!("{} {}", icons::MODEL, elide(&v.item.models.join(", "), 76)));
            }
            if !self.gallery_status.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 160, 120),
                    elide(&self.gallery_status, 120),
                );
            }
            ui.separator();
            if v.loading {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| ui.spinner());
            }
            // The filmstrip is a bottom panel so the image gets whatever height is left.
            act = self.filmstrip(ui).map(Act::Show).or(act);
            let v = self.viewer.as_ref().unwrap();
            // Fall back to any cached thumbnail so something shows while the full read lands.
            let cached = [1024u32, 512, 320]
                .iter()
                .find_map(|s| self.thumbs.get(&v.item.thumb_key(*s)));
            if let Some(tex) = v.tex.as_ref().or(cached) {
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    let avail = ui.available_width();
                    let sized = egui::load::SizedTexture::from_handle(tex);
                    ui.add(egui::Image::new(sized).max_width(avail));
                });
            }
        }
        match act {
            Some(Act::Close) => {
                self.viewer = None;
                self.gallery_status.clear();
            }
            Some(Act::Show(idx)) => self.open_viewer(idx),
            Some(Act::AlbumAdd(id)) => {
                let v = self.viewer.as_ref().unwrap();
                let items = vec![(v.item.subfolder.clone(), v.item.filename.clone())];
                self.engine.as_ref().unwrap().album_add(id, items);
            }
            Some(Act::AlbumRemove(id)) => {
                let v = self.viewer.as_ref().unwrap();
                let items = vec![(v.item.subfolder.clone(), v.item.filename.clone())];
                self.engine.as_ref().unwrap().album_remove(id, items);
            }
            Some(Act::Save) => {
                let v = self.viewer.as_ref().unwrap();
                let (bytes, name) = (v.bytes.clone().unwrap(), v.item.filename.clone());
                self.gallery_status = self.save_bytes(host, &bytes, &name);
            }
            Some(Act::UseAsInput) => {
                let v = self.viewer.as_ref().unwrap();
                if let Some(url) =
                    self.engine.as_ref().unwrap().view_url(&v.item.subfolder, &v.item.filename)
                {
                    self.params.mode = Mode::Img2Img;
                    self.params.img2img_source = Img2ImgSource::Url;
                    self.params.input_url = url;
                    self.tab = Tab::Generate;
                    self.note = "Gallery image set as img2img input".into();
                }
            }
            Some(Act::OpenWorkflow) => {
                let v = self.viewer.as_ref().unwrap();
                if let Some(schemas) = self.schemas.clone() {
                    self.graph_status = format!("Loading workflow of {}…", elide(&v.item.filename, 40));
                    self.wf_loading = true;
                    self.engine.as_ref().unwrap().open_gallery_workflow(
                        v.item.subfolder.clone(),
                        v.item.filename.clone(),
                        schemas,
                    );
                    self.tab = Tab::Graph;
                }
            }
            None => {}
        }
    }

    /// Horizontal strip of the rest of the listing along the bottom of the viewer. Returns the
    /// index of any tapped frame.
    ///
    /// Frames always request the smallest thumbnail regardless of the grid's column setting, so
    /// opening a one-column image doesn't pull a 4 MB read per neighbour.
    fn filmstrip(&mut self, ui: &mut egui::Ui) -> Option<usize> {
        const FRAME: f32 = 64.0;
        let current = self.viewer.as_ref().map(|v| v.idx);
        let mut picked = None;
        egui::Panel::bottom("filmstrip")
            .exact_size(FRAME + 12.0)
            .show(ui, |ui| {
                egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for idx in 0..self.gallery.len() {
                            let Some(item) = self.gallery.get(idx) else { continue };
                            let key = item.thumb_key(320);
                            let size = egui::vec2(FRAME, FRAME);
                            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                            if !ui.is_rect_visible(rect) {
                                continue;
                            }
                            match self.thumbs.get(&key) {
                                Some(tex) => {
                                    let img =
                                        egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                                            .fit_to_exact_size(size)
                                            .sense(egui::Sense::click());
                                    if ui.put(rect, img).clicked() {
                                        picked = Some(idx);
                                    }
                                }
                                None => {
                                    if self.thumbs.claim(&key) {
                                        self.engine.as_ref().unwrap().fetch_thumb(
                                            item.subfolder.clone(),
                                            item.filename.clone(),
                                            320,
                                        );
                                    }
                                    if ui.put(rect, egui::Button::new("")).clicked() {
                                        picked = Some(idx);
                                    }
                                }
                            }
                            if current == Some(idx) {
                                ui.painter().rect_stroke(
                                    rect,
                                    2.0,
                                    egui::Stroke::new(2.0, egui::Color32::from_rgb(110, 170, 255)),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    });
                });
            });
        picked
    }

    fn logs_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.horizontal(|ui| {
            if ui.button("Copy all").clicked() {
                host.copy_text(self.logs_text());
                host.haptic(Haptic::Light);
                self.note = "Logs copied".into();
            }
            if ui.button("Share").clicked() {
                host.share_text(self.logs_text());
            }
            if ui.button("Clear").clicked() {
                self.log_lines.clear();
                self.log.clear();
            }
            ui.weak(format!("{} lines", self.log_lines.len()));
        });
        ui.add_space(4.0);

        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show_rows(ui, row_h, self.log_lines.len(), |ui, range| {
                for line in &self.log_lines[range] {
                    let color = match line.level {
                        logger::Level::Info => ui.visuals().text_color(),
                        logger::Level::Warn => egui::Color32::from_rgb(230, 200, 120),
                        logger::Level::Error => egui::Color32::from_rgb(230, 120, 120),
                    };
                    let text = format!("[{:>7.1}s] {}", line.secs, elide(&line.text, 2000));
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).monospace().color(color))
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                }
            });
    }

    /// Full log text for copy/share, newest-biased: capped near 400KB because Android clipboard
    /// transactions fail around 1MB.
    fn logs_text(&self) -> String {
        let mut total = 0usize;
        let mut start = self.log_lines.len();
        for (i, l) in self.log_lines.iter().enumerate().rev() {
            if total + l.text.len() + 16 > 400_000 {
                break;
            }
            total += l.text.len() + 16;
            start = i;
        }
        let mut out = String::new();
        if start > 0 {
            out.push_str(&format!("[{start} earlier lines omitted]\n"));
        }
        for l in &self.log_lines[start..] {
            let lvl = match l.level {
                logger::Level::Info => "I",
                logger::Level::Warn => "W",
                logger::Level::Error => "E",
            };
            out.push_str(&format!("[{:>8.1}s {lvl}] {}\n", l.secs, l.text));
        }
        out
    }
}

impl EguiApp for ComfyApp {
    fn theme(&self, ctx: &egui::Context) {
        crate::theme::apply(ctx);
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        if self.engine.is_none() {
            self.engine = Some(Engine::new(ui.ctx().clone(), self.log.clone()));
        }
        if !self.loaded {
            self.loaded = true;
            // The framework never calls EguiApp::theme, so apply the color scheme here.
            crate::theme::apply(ui.ctx());
            egui_extras::install_image_loaders(ui.ctx());
            self.load_settings(host);
            if !self.server_url.trim().is_empty() {
                self.log.info("auto-connecting to saved server");
                self.connect(host);
            }
        }

        for m in self.engine.as_ref().unwrap().drain() {
            self.handle(ui.ctx(), host, m);
        }
        self.log_lines.extend(self.log.take_new(&mut self.log_cursor));
        if self.log_lines.len() > logger::MAX_LINES {
            let excess = self.log_lines.len() - logger::MAX_LINES;
            self.log_lines.drain(..excess);
        }
        self.autosave_settings(ui.ctx(), host);

        // Navigation sits at the bottom, within thumb reach. Panels are laid out before the
        // central content so the tab bar always keeps its height on a short screen.
        egui::Panel::bottom("nav").show(ui, |ui| {
            ui.add_space(2.0);
            // Global run progress, visible from every tab: real step progress when the server
            // streams it, node completion otherwise.
            if self.running {
                let (v, m) = self.progress;
                let (frac, label) = if m > 0 {
                    (v as f32 / m as f32, format!("{} {v}/{m}", elide(&self.status, 40)))
                } else if self.run_total > 0 {
                    let done = self.run_seen.len().saturating_sub(1).min(self.run_total);
                    (
                        done as f32 / self.run_total as f32,
                        format!(
                            "node {} of {}",
                            self.run_seen.len().min(self.run_total),
                            self.run_total
                        ),
                    )
                } else {
                    (0.0, elide(&self.status, 48))
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_height(14.0)
                        .text(format!("{:.0}%  {label}", frac * 100.0))
                        .animate(true),
                );
                ui.add_space(2.0);
            }
            self.nav_bar(ui);
            ui.add_space(2.0);
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| match self.tab {
                Tab::Generate => self.generate_tab(ui, host),
                Tab::Graph => self.graph_tab(ui, host),
                Tab::Gallery => self.gallery_tab(ui, host),
                Tab::Settings => self.settings_tab(ui, host),
                Tab::Logs => self.logs_tab(ui, host),
            });

        if self.running {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
    }
}

impl ComfyApp {
    /// Equal-width tab targets, icon and label on one line to keep the bar short.
    fn nav_bar(&mut self, ui: &mut egui::Ui) {
        let n = Tab::BAR.len();
        ui.columns(n, |cols| {
            for (i, (tab, icon, label)) in Tab::BAR.iter().enumerate() {
                let ui = &mut cols[i];
                let selected = self.tab == *tab;
                let text = egui::RichText::new(format!("{icon} {label}")).size(12.0);
                let btn = egui::Button::selectable(selected, text)
                    .wrap_mode(egui::TextWrapMode::Extend)
                    .min_size(egui::vec2(ui.available_width(), 28.0));
                if ui.add(btn).clicked() {
                    self.tab = *tab;
                }
            }
        });
    }
}

/// A menu / combo-box button whose popup opens *upward* and scrolls.
///
/// `Ui::menu_button` and `egui::ComboBox` only flip their popup above the button when it wouldn't
/// otherwise fit — but egui's screen rect extends under the Android navigation bar, so a short menu
/// "fits" below and ends up covering the nav bar and the system gesture area. Everything in a
/// bottom control bar uses this instead, which always prefers opening upward. The bounded scroll
/// area keeps a long list (e.g. every model) from running off the top of the screen.
fn up_menu<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    content: impl FnOnce(&mut egui::Ui) -> R,
) {
    use egui::containers::menu::MenuConfig;
    let response = ui.button(label.into());
    let config = MenuConfig::default();
    egui::Popup::menu(&response)
        .align(egui::RectAlign::TOP_START)
        // Fall back to the other corners before ever covering the bar again.
        .align_alternatives(&[egui::RectAlign::TOP_END, egui::RectAlign::BOTTOM_START])
        .close_behavior(config.close_behavior)
        .style(config.style.clone())
        .info(
            egui::UiStackInfo::new(egui::UiKind::Menu)
                .with_tag_value(MenuConfig::MENU_CONFIG_TAG, config),
        )
        .show(|ui| {
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    content(ui)
                })
                .inner
        });
}

/// Anchor a popup window to the center of the screen.
///
/// A top-anchored `egui::Window` can push its title bar (and its close button) above the app's
/// content area — up under the status-bar icons, where the close button is hard to hit. Centering
/// keeps every window fully inside the usable area, and it re-centers above the keyboard when the
/// content shrinks for the IME.
fn centered(window: egui::Window<'_>) -> egui::Window<'_> {
    window.anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
}

fn combo(ui: &mut egui::Ui, id: &str, current: &mut String, options: &[String]) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(if current.is_empty() { "—".to_string() } else { elide(current, 40) })
        .show_ui(ui, |ui| {
            for opt in options.iter().take(300) {
                ui.selectable_value(current, opt.clone(), elide(opt, 56));
            }
        });
}

fn image_view(ui: &mut egui::Ui, tex: &egui::TextureHandle) {
    let avail = ui.available_width().min(720.0);
    let sized = egui::load::SizedTexture::from_handle(tex);
    ui.add(egui::Image::new(sized).max_width(avail));
}

fn random_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

app!(ComfyApp::new);
