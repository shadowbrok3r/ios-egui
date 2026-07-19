//! The Android UI: Generate (params, output), Graph (node editor over server workflows), Properties,
//! Gallery (server output browser with albums), and Settings (server, API key, account, logs).

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use egui_mobile::{CreateContext, EguiApp, Haptic, Host, HostExt, app, egui};
use egui_snarl::{InPinId, OutPinId};
use rucomfyui::workflow::WorkflowNodeId;
use rucomfyui_node_graph::{ComfyUiNodeGraph, NodeId, internal::FlowNodeData, internal::FlowValueType};

use crate::apps::{AppDef, AppSet, KnobTy, Status};
use crate::engine::{Engine, GenCtx, Msg};
use crate::gallery::{self, ImageMeta, ThumbCache};
use crate::graphview::{self, GraphView, LongPress, LoraPick, elide, elide_width, sanitize_ui_text};
use crate::icons;
use crate::logger::{self, Logger};
use crate::player::Player;
use crate::schema::{self, SchemaSet};
use crate::uiwf;
use crate::types::{
    ActiveLora, Album, AppPack, AppStep, CHECKPOINT_RECENT_MAX, CheckpointCatalog, CheckpointSort,
    CreatePreset, FALLBACK_SAMPLERS, FALLBACK_SCHEDULERS, Facets, FontSizes, GalleryGroup,
    GalleryItem, GalleryMedia, GallerySort, GalleryView, Img2ImgSource, LoraCatalog, LoraPack, Mode,
    ModelKind, Params, SamplerPack, Settings, append_negatives, checkpoint_family, fallback_vec,
    file_basename, merge_triggers, strip_injected,
};

/// Ceiling on auto-loaded gallery items, so a huge namespace can't page forever.
const GALLERY_LOAD_ALL_CAP: u64 = 5000;
/// comfy-gate clamps `/gallery/api/list` `limit` at this.
const GALLERY_PAGE_MAX: u64 = 500;

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
}

/// Whether companion resolution is seeding a newly-picked model or repairing an existing choice.
#[derive(PartialEq, Clone, Copy)]
enum Companions {
    /// The model just changed — the catalog recommendation outranks the previous selection.
    Seed,
    /// Reconnect / preset load — the existing selection is the user's and outranks the catalog.
    Repair,
}

#[derive(PartialEq, Clone, Copy)]
enum CreatePane {
    Main,
    Models,
    Loras,
    Enhance,
    Presets,
}

#[derive(PartialEq, Clone, Copy)]
enum SettingsPane {
    Server,
    Logs,
}

/// Destination for the app picker's selection.
#[derive(PartialEq, Clone, Copy)]
enum AppPickTarget {
    Enhance,
    /// A position on a specific graph tab. The picker is a non-modal window drawn outside the
    /// tab dispatch, so it outlives a tab switch — the doc id keeps the insert from landing on
    /// whatever tab happens to be active when the user finally picks.
    Canvas { doc: u64, at: egui::Pos2 },
}

/// In-progress "Save tab as app": the derived definition plus which widgets to promote.
struct PublishDraft {
    id: String,
    name: String,
    group: String,
    description: String,
    /// One promotable widget. `local` identifies the exact node, so two nodes of the same class
    /// yield two distinct knobs instead of colliding on one id.
    widgets: Vec<PublishWidget>,
    def: AppDef,
    error: String,
    /// A socket no Create-graph handle can supply; saving is refused until it is wired.
    blocked: bool,
}

struct PublishWidget {
    /// `AppDef` node id ("n0"), not the class — a graph may hold several of one class.
    local: String,
    class: String,
    input: String,
    label: String,
    value: serde_json::Value,
    promote: bool,
}

impl Tab {
    /// Bottom navigation order: icon plus optional short label (empty = icon only).
    const BAR: &'static [(Tab, &'static str, &'static str)] = &[
        (Tab::Generate, icons::GENERATE, "Create"),
        (Tab::Graph, icons::GRAPH, "Graph"),
        (Tab::Gallery, icons::GALLERY, ""),
        (Tab::Settings, icons::SETTINGS, ""),
    ];
}

/// Which tab's queue action the shared play FAB should run.
#[derive(Clone, Copy)]
enum QueueFabKind {
    Create,
    Graph,
}

/// Panes within the Graph tab.
#[derive(PartialEq, Clone, Copy)]
enum GraphPane {
    Canvas,
    Props,
}

/// Source for the LoadImage image picker: the server's uploaded inputs, or the phone's gallery.
#[derive(PartialEq, Clone, Copy, Default)]
enum ImgPickSource {
    #[default]
    Server,
    Device,
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
    /// Floating metadata header expanded over the image.
    meta_open: bool,
    /// Raw embedded workflow JSON (for Copy); `None` until fetched or unavailable.
    workflow_json: Option<String>,
    /// Parsed prompts / LoRAs / sampler summary from the workflow.
    meta: Option<ImageMeta>,
    meta_loading: bool,
}

/// One open workflow editor document (multi-tab Graph workspace).
struct GraphDoc {
    #[allow(dead_code)]
    id: u64,
    name: String,
    graph: ComfyUiNodeGraph,
    view: GraphView,
    outputs: HashMap<NodeId, Vec<Vec<u8>>>,
    node_map: HashMap<u32, NodeId>,
    props_node: Option<NodeId>,
    /// Nodes marked bypassed (ComfyUI mode 4) — spliced out at queue/export time.
    bypassed: HashSet<NodeId>,
    /// Bumped whenever the snarl is replaced, so stale node ids can be detected.
    epoch: u64,
    /// Undo/redo for this tab. Per-tab: tabs are independent documents.
    history: crate::history::History,
    /// A load is still settling its auto-layout; re-baseline the history once it does, so the
    /// refined positions are the starting point rather than an edit the user never made.
    history_rebase: bool,
}

impl GraphDoc {
    fn new(id: u64, name: String, object_info: rucomfyui::object_info::ObjectInfo) -> Self {
        Self {
            id,
            name,
            graph: ComfyUiNodeGraph::new(object_info),
            view: GraphView::new(id),
            outputs: HashMap::new(),
            node_map: HashMap::new(),
            props_node: None,
            bypassed: HashSet::new(),
            epoch: 0,
            history: crate::history::History::default(),
            history_rebase: false,
        }
    }

    fn is_empty(&self) -> bool {
        self.graph.snarl.nodes_pos_ids().next().is_none()
    }

    fn clear_content(&mut self) {
        self.graph.clear();
        self.name.clear();
        self.outputs.clear();
        self.node_map.clear();
        self.props_node = None;
        self.bypassed.clear();
        self.view.reset();
        // Snarl ids restart at 0 on a fresh graph, so anything holding old ids must be stale.
        self.epoch += 1;
        // A new document gets a new history: undoing across a whole-document swap back into the
        // previous workflow is more surprising than useful.
        self.history.reset(&self.graph.snarl);
    }

    fn title(&self) -> String {
        if self.name.is_empty() {
            if self.is_empty() {
                "Untitled".into()
            } else {
                "Untitled graph".into()
            }
        } else {
            elide(&self.name, 28)
        }
    }
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
    /// Diffusion models (`models/diffusion_models`, `models/unet`) needing separate CLIP + VAE.
    unets: Vec<String>,
    clip_files: Vec<String>,
    vaes: Vec<String>,
    clip_types: Vec<String>,
    clip_devices: Vec<String>,
    weight_dtypes: Vec<String>,
    samplers: Vec<String>,
    schedulers: Vec<String>,
    ckpt_filter: String,
    /// Model-list filter: `None` shows checkpoints and diffusion models together.
    models_kind_filter: Option<ModelKind>,
    /// Collapse all checkpoint groups on the next Models pane paint.
    checkpoints_force_collapse: bool,
    /// Create Checkpoints list sort (persisted).
    checkpoint_sort: CheckpointSort,
    /// Locally pinned favorite checkpoint filenames (persisted).
    checkpoint_favorites: Vec<String>,
    /// Most-recently-used checkpoint filenames, newest first (persisted).
    checkpoint_recent: Vec<String>,
    create_pane: CreatePane,
    settings_pane: SettingsPane,
    lora_catalog: LoraCatalog,
    checkpoint_catalog: CheckpointCatalog,
    /// Installed LoRA filenames from `object_info` (`LoraLoader.lora_name`).
    installed_loras: Vec<String>,
    lora_filter: String,
    presets: Vec<CreatePreset>,
    selected_preset: String,
    preset_save_open: bool,
    preset_name_edit: String,
    /// Builtin enhance apps plus any under `{documents}/comfyui/apps`.
    apps: Arc<AppSet>,
    /// Where a picked app goes: the Create chain, or the canvas at a graph position.
    app_picker: Option<AppPickTarget>,
    app_filter: String,
    /// Steps skipped or inputs dropped on the last build; pinned next to the result.
    enhance_note: String,
    /// Nodes from the last `Insert app`, so a mis-tap is one undo rather than N deletes.
    /// Keyed by (doc id, epoch): node ids are per-document AND restart when a tab is cleared
    /// or reloaded, so undoing against either would delete unrelated nodes.
    publish: Option<PublishDraft>,

    params: Params,
    last_saved: Option<String>,
    last_save_check: f64,

    running: bool,
    progress: (u32, u32),
    status: String,
    /// Server-wide queue depth (WS status / `/queue`), includes jobs from other clients.
    queue_remaining: u32,
    /// Last time we polled `GET /queue`.
    last_queue_poll: f64,

    preview: Option<egui::TextureHandle>,
    /// Latest Create result (also the last entry in [`Self::results`]) — img2img / Save default.
    result: Option<egui::TextureHandle>,
    result_bytes: Option<Vec<u8>>,
    /// All images from the current Create run(s), in arrival order (batch + multi-queue).
    results: Vec<(egui::TextureHandle, Vec<u8>)>,
    /// Fullscreen Create result index into [`Self::results`].
    result_view: Option<usize>,
    /// Texture id salt so batch frames do not overwrite each other in the egui atlas.
    result_seq: u64,
    save_counter: u32,
    note: String,

    /// Open workflow editor tabs (created on connect / when loading graphs).
    graph_tabs: Vec<GraphDoc>,
    active_graph: usize,
    next_graph_id: u64,
    /// Graph tab kept in sync with Create (`Open Graph`).
    create_graph_id: Option<u64>,
    /// Fingerprint of [`Self::params`] last pushed into the linked graph.
    create_sync_fp: u64,
    /// Fingerprint of the linked graph export last pulled into Create.
    create_graph_export_fp: u64,
    /// Debounce timer for Create → Graph pushes.
    create_sync_dirty_at: Option<f64>,
    graph_pane: GraphPane,
    auto_follow: bool,
    /// Auto-arrange when a loaded workflow's canvas is first shown.
    auto_arrange: bool,
    fonts: FontSizes,
    /// Rows fetched per gallery page / Load more (clamped 20..=500).
    gallery_page: u64,
    /// Transient graph editor note (errors / saving); not shown in the tab bar.
    graph_status: String,
    /// UI-format JSON restored after connect (from settings).
    restore_workflow: Option<(String, String)>,
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
    thumbs: ThumbCache,
    viewer: Option<Viewer>,
    /// Live video playback for the opened viewer item, when it's a video.
    player: Option<Player>,
    /// Distinguishes each playback's cache file, so a new video never truncates the file a
    /// still-winding-down decode thread is reading.
    playback_seq: u64,
    /// Ignore gallery pages from queries older than this (filter changed mid auto-load chain).
    gallery_gen: u64,
    albums: Vec<Album>,
    facets: Facets,
    album_new_name: String,
    album_manage_open: bool,
    /// Filter text for the LoadImage thumbnail picker in the Properties tab.
    img_pick_filter: String,
    /// LoadImage picker source: the server's input images vs the phone's photo gallery.
    img_pick_source: ImgPickSource,
    /// Cached device gallery listing `(MediaStore id, display name)`, newest first (Android only).
    device_images: Vec<(i64, String)>,
    /// Whether `device_images` has been queried this session (avoids re-querying every frame).
    device_images_loaded: bool,
    /// In-flight device-image uploads: token → the LoadImage node the pick targets. The token is
    /// echoed back in the result message so a slow upload lands on the node it was chosen for.
    /// In-flight device-image uploads, keyed by request token. The node is qualified by (doc,
    /// epoch) because the value lands asynchronously: the user can switch tabs or undo in the
    /// meantime, and snarl reuses freed slab keys, so a bare NodeId can resolve to a DIFFERENT
    /// node and quietly rewrite it.
    pending_uploads: HashMap<u64, (u64, u64, NodeId)>,
    /// Monotonic id handed to each device-image upload.
    next_upload_id: u64,
    /// The image queued for "add to album" while the picker is open.
    album_target: Option<GalleryItem>,
    /// Create-album dialog open for these `(subfolder, filename)` pairs (selection kept).
    album_create_draft: Option<Vec<(String, String)>>,
    /// After `album_create`, add these items once an album with this name appears.
    album_pending_add: Option<(String, Vec<(String, String)>)>,
    /// Multi-select in the gallery grid: `selected` holds item keys (`subfolder/filename`).
    select_mode: bool,
    selected: HashSet<String>,
    /// Confirm before deleting gallery images (persisted; "never again" clears it).
    confirm_gallery_delete: bool,
    /// Pending delete confirmation: items + "never show again" checkbox.
    delete_confirm: Option<(Vec<(String, String)>, bool)>,
    /// Close the viewer after a confirmed single-image delete.
    delete_closes_viewer: bool,
    /// Long-press-to-paint gesture: (press start time, screen origin, cancelled-as-a-scroll).
    sel_press: Option<(f64, egui::Pos2, bool)>,
    sel_long_fired: bool,
    /// A long-press-initiated paint-select drag is in progress (disables scroll so it doesn't pan).
    sel_painting: bool,
    /// Visible tile rects this frame `(rect, gallery index)`, for the paint gesture.
    tile_hits: Vec<(egui::Rect, usize)>,
    /// Last workflow JSON copied from a gallery image (also written to the system clipboard).
    workflow_clip: Option<String>,
    /// Sampler / steps / CFG copied from a gallery image (also on the system clipboard).
    sampler_clip: Option<SamplerPack>,
    /// LoRA stack copied from a gallery image (also on the system clipboard).
    lora_clip: Option<LoraPack>,
    /// Create/graph prompts still being tracked locally (supports Queue while running).
    jobs_left: usize,
    /// Thumbnail for Create img2img "From URL".
    img2img_url_tex: Option<egui::TextureHandle>,
    /// URL currently shown in `img2img_url_tex` (or last failed fetch).
    img2img_url_key: String,
    /// URL of the in-flight preview fetch.
    img2img_url_req: String,
    img2img_url_loading: bool,
    img2img_url_err: String,
    /// Debounce: last seen input_url and when it changed.
    img2img_url_pending: String,
    img2img_url_pending_at: f64,
    /// Long-press menu on empty graph canvas: `(graph_pos, screen_pos, armed)`.
    /// `armed` stays false until the opening press is released so that release doesn't dismiss.
    canvas_menu: Option<(egui::Pos2, egui::Pos2, bool)>,
    /// Long-press menu on a graph node: `(node, screen_pos, armed)`.
    node_menu: Option<(NodeId, egui::Pos2, bool)>,
    /// Gallery list scroll offset (Y); kept across viewer open/close.
    gallery_scroll_y: f32,
    /// Apply this offset once when returning from the viewer.
    gallery_scroll_restore: Option<f32>,
    /// Pull-to-refresh: finger-down drag started at the top of the list.
    gallery_pull_tracking: bool,
    /// Rubber-band distance while pulling (screen px).
    gallery_pull: f32,
    /// Item-key → height/width from the last decoded thumb (keeps 1-column rows from jumping).
    thumb_aspects: HashMap<String, f32>,
    /// Center the filmstrip on the current viewer index once (open / swipe / tap).
    filmstrip_center: bool,
    /// Press origin for a viewer left/right swipe (egui clears `press_origin` on release).
    viewer_swipe_origin: Option<egui::Pos2>,
    /// Re-fetch the gallery listing at this time (server indexing lag after generate).
    gallery_refresh_at: Option<f64>,
    /// Create-tab menu FAB position; `None` = default left of the queue FAB.
    create_fab_pos: Option<egui::Pos2>,
    create_fab_open: bool,
    /// Shared Create/Graph queue (play) FAB position; `None` = default bottom-right.
    queue_fab_pos: Option<egui::Pos2>,
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
            unets: Vec::new(),
            clip_files: Vec::new(),
            vaes: Vec::new(),
            clip_types: Vec::new(),
            clip_devices: Vec::new(),
            weight_dtypes: Vec::new(),
            samplers: fallback_vec(FALLBACK_SAMPLERS),
            schedulers: fallback_vec(FALLBACK_SCHEDULERS),
            ckpt_filter: String::new(),
            models_kind_filter: None,
            checkpoints_force_collapse: true,
            checkpoint_sort: CheckpointSort::Name,
            checkpoint_favorites: Vec::new(),
            checkpoint_recent: Vec::new(),
            create_pane: CreatePane::Main,
            settings_pane: SettingsPane::Server,
            lora_catalog: LoraCatalog::default(),
            checkpoint_catalog: CheckpointCatalog::default(),
            installed_loras: Vec::new(),
            lora_filter: String::new(),
            presets: Vec::new(),
            selected_preset: String::new(),
            preset_save_open: false,
            preset_name_edit: String::new(),
            apps: Arc::new(AppSet::builtin()),
            app_picker: None,
            app_filter: String::new(),
            enhance_note: String::new(),
            publish: None,
            params: Params::default(),
            last_saved: None,
            last_save_check: 0.0,
            running: false,
            progress: (0, 0),
            status: String::new(),
            queue_remaining: 0,
            last_queue_poll: 0.0,
            preview: None,
            result: None,
            result_bytes: None,
            results: Vec::new(),
            result_view: None,
            result_seq: 0,
            save_counter: 0,
            note: String::new(),
            graph_tabs: Vec::new(),
            active_graph: 0,
            next_graph_id: 1,
            create_graph_id: None,
            create_sync_fp: 0,
            create_graph_export_fp: 0,
            create_sync_dirty_at: None,
            graph_pane: GraphPane::Canvas,
            auto_follow: false,
            auto_arrange: true,
            fonts: FontSizes::default(),
            gallery_page: 60,
            graph_status: String::new(),
            restore_workflow: None,
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
            thumbs: ThumbCache::default(),
            viewer: None,
            player: None,
            playback_seq: 0,
            gallery_gen: 0,
            albums: Vec::new(),
            facets: Facets::default(),
            album_new_name: String::new(),
            album_manage_open: false,
            img_pick_filter: String::new(),
            img_pick_source: ImgPickSource::default(),
            device_images: Vec::new(),
            device_images_loaded: false,
            pending_uploads: HashMap::new(),
            next_upload_id: 0,
            album_target: None,
            album_create_draft: None,
            album_pending_add: None,
            select_mode: false,
            selected: HashSet::new(),
            confirm_gallery_delete: true,
            delete_confirm: None,
            delete_closes_viewer: false,
            sel_press: None,
            sel_long_fired: false,
            sel_painting: false,
            tile_hits: Vec::new(),
            workflow_clip: None,
            sampler_clip: None,
            lora_clip: None,
            jobs_left: 0,
            img2img_url_tex: None,
            img2img_url_key: String::new(),
            img2img_url_req: String::new(),
            img2img_url_loading: false,
            img2img_url_err: String::new(),
            img2img_url_pending: String::new(),
            img2img_url_pending_at: 0.0,
            canvas_menu: None,
            node_menu: None,
            gallery_scroll_y: 0.0,
            gallery_scroll_restore: None,
            gallery_pull_tracking: false,
            gallery_pull: 0.0,
            thumb_aspects: HashMap::new(),
            filmstrip_center: false,
            viewer_swipe_origin: None,
            gallery_refresh_at: None,
            create_fab_pos: None,
            create_fab_open: false,
            queue_fab_pos: None,
        }
    }

    fn active_doc(&self) -> Option<&GraphDoc> {
        self.graph_tabs.get(self.active_graph)
    }

    fn active_doc_mut(&mut self) -> Option<&mut GraphDoc> {
        self.graph_tabs.get_mut(self.active_graph)
    }

    fn has_graph_editor(&self) -> bool {
        !self.graph_tabs.is_empty()
    }

    /// Ensure at least one empty tab exists once schemas are known.
    fn ensure_graph_tab(&mut self) {
        let Some(schemas) = self.schemas.as_ref() else { return };
        if !self.graph_tabs.is_empty() {
            return;
        }
        let object_info = schema::to_object_info(schemas);
        let id = self.next_graph_id;
        self.next_graph_id += 1;
        self.graph_tabs.push(GraphDoc::new(id, String::new(), object_info));
        self.active_graph = 0;
    }

    /// Open `workflow` in a new tab, or reuse the active tab when it is empty.
    fn load_workflow_into_tab(
        &mut self,
        name: String,
        workflow: &rucomfyui::Workflow,
    ) -> Result<(), String> {
        let schemas = self.schemas.as_ref().ok_or_else(|| "not connected".to_string())?;
        let object_info = schema::to_object_info(schemas);
        let reuse = self.active_doc().is_some_and(|d| d.is_empty());
        if !reuse {
            let id = self.next_graph_id;
            self.next_graph_id += 1;
            self.graph_tabs.push(GraphDoc::new(id, String::new(), object_info));
            self.active_graph = self.graph_tabs.len() - 1;
        } else if let Some(doc) = self.active_doc_mut() {
            doc.graph.object_info = object_info;
        }
        self.replace_active_workflow(name, workflow)
    }

    /// Replace the workflow in tab `idx` (keeps the tab id / link).
    fn replace_workflow_in_tab(
        &mut self,
        idx: usize,
        name: String,
        workflow: &rucomfyui::Workflow,
    ) -> Result<(), String> {
        let schemas = self.schemas.as_ref().ok_or_else(|| "not connected".to_string())?;
        let object_info = schema::to_object_info(schemas);
        let doc = self.graph_tabs.get_mut(idx).ok_or_else(|| "no graph tab".to_string())?;
        doc.graph.object_info = object_info;
        let auto = self.auto_arrange;
        doc.outputs.clear();
        doc.node_map.clear();
        doc.props_node = None;
        doc.bypassed.clear();
        doc.view.reset();
        doc.epoch += 1;
        doc.graph.load_api_workflow(workflow).map_err(|e| e.to_string())?;
        doc.name = name;
        if auto {
            // Defer until the canvas paints — Create sync / off-tab loads never call `show`.
            doc.view.mark_needs_auto_arrange();
        } else {
            doc.view.request_fit();
        }
        doc.history.reset(&doc.graph.snarl);
        doc.history_rebase = true;
        Ok(())
    }

    fn replace_active_workflow(
        &mut self,
        name: String,
        workflow: &rucomfyui::Workflow,
    ) -> Result<(), String> {
        let idx = self.active_graph;
        self.replace_workflow_in_tab(idx, name, workflow)
    }

    fn close_graph_tab(&mut self, idx: usize) {
        if idx >= self.graph_tabs.len() {
            return;
        }
        let closed_id = self.graph_tabs[idx].id;
        if self.graph_tabs.len() == 1 {
            self.graph_tabs[0].clear_content();
            self.active_graph = 0;
            self.executing = None;
            if self.create_graph_id == Some(closed_id) {
                self.create_graph_id = None;
            }
            return;
        }
        self.graph_tabs.remove(idx);
        if self.create_graph_id == Some(closed_id) {
            self.create_graph_id = None;
        }
        if self.active_graph >= self.graph_tabs.len() {
            self.active_graph = self.graph_tabs.len() - 1;
        } else if idx < self.active_graph {
            self.active_graph -= 1;
        }
        self.executing = None;
    }

    fn close_all_graph_tabs(&mut self) {
        let Some(schemas) = self.schemas.as_ref() else {
            self.graph_tabs.clear();
            self.active_graph = 0;
            self.create_graph_id = None;
            return;
        };
        let object_info = schema::to_object_info(schemas);
        let id = self.next_graph_id;
        self.next_graph_id += 1;
        self.graph_tabs.clear();
        self.graph_tabs.push(GraphDoc::new(id, String::new(), object_info));
        self.active_graph = 0;
        self.executing = None;
        self.create_graph_id = None;
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
                // Finish "create album then add" from the selection / viewer picker.
                if let Some((name, items)) = self.album_pending_add.take() {
                    if let Some(id) = self.albums.iter().find(|a| a.name == name).map(|a| a.id) {
                        self.engine.as_ref().unwrap().album_add(id, items);
                        self.selected.clear();
                        self.select_mode = false;
                    } else {
                        self.album_pending_add = Some((name, items));
                    }
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
                self.album_pending_add = None;
                host.haptic(Haptic::Error);
            }
            Msg::GalleryMutated(note) => {
                self.gallery_status = note;
                self.selected.clear();
                self.select_mode = false;
                if self.delete_closes_viewer {
                    self.delete_closes_viewer = false;
                    self.viewer = None;
                    self.player = None;
                    self.viewer_swipe_origin = None;
                }
                self.refresh_gallery();
                host.haptic(Haptic::Success);
            }
            Msg::ItemAlbums { key, albums } => {
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.albums = Some(albums);
                }
            }
            Msg::Img2ImgUrlPreview { url, image, error } => {
                if url != self.img2img_url_req {
                    return;
                }
                self.img2img_url_loading = false;
                self.img2img_url_key = url.clone();
                if let Some(ci) = image {
                    self.img2img_url_tex =
                        Some(ctx.load_texture("img2img_url", ci, egui::TextureOptions::LINEAR));
                    self.img2img_url_err.clear();
                } else {
                    self.img2img_url_tex = None;
                    self.img2img_url_err = error.unwrap_or_else(|| "Preview failed".into());
                }
            }
            Msg::InputUploaded { token, image_ref } => {
                let target = self.pending_uploads.remove(&token);
                // Only write it back if the very same node is still there. A tab switch, an undo
                // or a reload all invalidate the id, and writing anyway would edit a stranger.
                let still_ours = target.is_some_and(|(doc_id, epoch, _)| {
                    self.active_doc().is_some_and(|d| d.id == doc_id && d.epoch == epoch)
                });
                let mut wrote = false;
                if let (true, Some((_, _, node)), Some(doc)) =
                    (still_ours, target, self.active_doc_mut())
                    && let Some(data) = doc.graph.snarl.get_node_mut(node)
                {
                    // Select the uploaded image on the node's `image` input, adding it to the
                    // option list so the picker and the node body show it.
                    for inp in data.inputs.iter_mut() {
                        if inp.name.eq_ignore_ascii_case("image")
                            && let FlowValueType::Array { options, selected } = &mut inp.value
                        {
                            if !options.iter().any(|o| o == &image_ref) {
                                options.insert(0, image_ref.clone());
                            }
                            *selected = image_ref.clone();
                            wrote = true;
                            break;
                        }
                    }
                }
                // Reporting success outside the write meant a retargeted or input-less node still
                // said "Loaded", having changed nothing.
                if wrote {
                    self.note = format!("Loaded {} from device", elide(&image_ref, 40));
                    host.haptic(Haptic::Success);
                } else if target.is_some() {
                    self.note = "Upload finished, but that node is gone — pick it again".into();
                    host.haptic(Haptic::Warning);
                }
            }
            Msg::InputUploadError { token, error } => {
                if self.pending_uploads.remove(&token).is_some() {
                    self.note = elide(&error, 120);
                    host.haptic(Haptic::Error);
                }
            }
            Msg::LoraCatalog(catalog) => {
                self.lora_catalog = catalog;
            }
            Msg::LoraCatalogError(err) => {
                self.log.warn(format!("lora catalog: {err}"));
            }
            Msg::CheckpointCatalog(catalog) => {
                // Feed base tags into the LoRA filter map (file + basename keys).
                for e in &catalog.checkpoints {
                    if e.bases.is_empty() {
                        continue;
                    }
                    self.lora_catalog
                        .checkpoints
                        .insert(e.file.clone(), e.bases.clone());
                    let base = crate::types::file_basename(&e.file);
                    if base != e.file {
                        self.lora_catalog
                            .checkpoints
                            .insert(base.to_string(), e.bases.clone());
                    }
                }
                self.checkpoint_catalog = catalog;
            }
            Msg::CheckpointCatalogError(err) => {
                self.log.warn(format!("checkpoint catalog: {err}"));
            }
            Msg::Connected { schemas, models } => {
                self.conn = Conn::Connected;
                // Albums and model facets are per-account, so they follow the credential.
                self.engine.as_ref().unwrap().albums();
                self.engine.as_ref().unwrap().facets();
                self.engine.as_ref().unwrap().fetch_lora_catalog();
                self.engine.as_ref().unwrap().fetch_checkpoint_catalog();
                self.installed_loras = schemas.loras();
                // Swap the node catalog in place so a reconnect keeps open tabs.
                let object_info = schema::to_object_info(&schemas);
                let had_nodes = self.graph_tabs.iter().any(|d| !d.is_empty());
                if self.graph_tabs.is_empty() {
                    let id = self.next_graph_id;
                    self.next_graph_id += 1;
                    self.graph_tabs.push(GraphDoc::new(id, String::new(), object_info));
                    self.active_graph = 0;
                } else {
                    for doc in &mut self.graph_tabs {
                        doc.graph.object_info = object_info.clone();
                    }
                }
                self.schemas = Some(schemas.clone());
                // Availability of every enhance app against the catalog we just got, so the Logs
                // tab answers "why is that step greyed out" without opening the Create tab.
                for def in self.apps.by_id.values() {
                    let st = crate::apps::status(def, None, Some(&schemas));
                    match st {
                        Status::Ready => self.log.info(format!("app {}: ready", def.id)),
                        other => self.log.info(format!("app {}: {}", def.id, other.chip())),
                    }
                }
                if !models.checkpoints.is_empty() {
                    self.checkpoints = models.checkpoints;
                }
                if !models.unets.is_empty() {
                    self.unets = models.unets;
                }
                if !models.clips.is_empty() {
                    self.clip_files = models.clips;
                }
                if !models.vaes.is_empty() {
                    self.vaes = models.vaes;
                }
                if !models.clip_types.is_empty() {
                    self.clip_types = models.clip_types;
                }
                if !models.clip_devices.is_empty() {
                    self.clip_devices = models.clip_devices;
                }
                if !models.weight_dtypes.is_empty() {
                    self.weight_dtypes = models.weight_dtypes;
                }
                if !models.samplers.is_empty() {
                    self.samplers = models.samplers;
                }
                if !models.schedulers.is_empty() {
                    self.schedulers = models.schedulers;
                }
                // A restored selection may not exist on this server; fall back to the first model
                // of either kind, otherwise re-resolve the companions against what is installed.
                let selected = self.params.model_file().to_string();
                let known = match self.params.model_kind {
                    ModelKind::Checkpoint => self.checkpoints.contains(&selected),
                    ModelKind::Diffusion => self.unets.contains(&selected),
                };
                if selected.is_empty() || !known {
                    let first = self
                        .checkpoints
                        .first()
                        .map(|f| (f.clone(), ModelKind::Checkpoint))
                        .or_else(|| {
                            self.unets.first().map(|f| (f.clone(), ModelKind::Diffusion))
                        });
                    if let Some((file, kind)) = first {
                        self.select_model(&file, Some(kind));
                    }
                } else if self.params.model_kind == ModelKind::Diffusion {
                    self.resolve_companions(Companions::Repair);
                }
                self.status.clear();
                // Restore the last opened graph once schemas are available (skip if canvas already
                // has nodes from this session).
                if !had_nodes
                    && let Some((name, body)) = self.restore_workflow.take()
                {
                    self.wf_loading = true;
                    self.engine.as_ref().unwrap().load_workflow_json(name, body, schemas);
                }
                host.haptic(Haptic::Success);
            }
            Msg::ConnectError(e) => {
                self.conn = Conn::Failed(e);
                host.haptic(Haptic::Error);
            }
            Msg::EnhanceNote(note) => self.enhance_note = note,
            Msg::Queued => self.status = "Queued".into(),
            Msg::Progress { value, max } => {
                self.progress = (value, max);
                self.status = format!("Sampling {value}/{max}");
            }
            Msg::QueueRemaining(n) => {
                self.queue_remaining = n;
                if n == 0 {
                    if !self.running {
                        self.progress = (0, 0);
                        if self.status.starts_with("Server queue") {
                            self.status = "Done".into();
                        }
                    }
                } else if !self.running {
                    self.status = format!("Server queue: {n}");
                }
            }
            Msg::Status(s) => self.status = s,
            Msg::Preview(ci) => {
                self.preview = Some(ctx.load_texture("preview", ci, egui::TextureOptions::LINEAR));
            }
            Msg::Result { image, bytes } => {
                self.result_seq = self.result_seq.wrapping_add(1);
                let name = format!("result-{}", self.result_seq);
                let tex = ctx.load_texture(name, image, egui::TextureOptions::LINEAR);
                self.result = Some(tex.clone());
                self.result_bytes = Some(bytes.clone());
                self.results.push((tex, bytes));
                self.preview = None;
                self.note.clear();
            }
            Msg::NodeExecuting(node) => {
                if let Some(n) = node {
                    self.run_seen.insert(n);
                }
                // Prefer the Create-linked tab so progress tracks even when Create is focused.
                let doc_idx = self.progress_doc_idx();
                let nid = doc_idx.and_then(|i| {
                    node.and_then(|n| self.graph_tabs.get(i)?.node_map.get(&n).copied())
                });
                self.executing = nid;
                // Select the running node like ComfyUI does: it shows in Properties and (unless the
                // green executing stroke wins) gets the focus border.
                if let Some(nid) = nid
                    && let Some(i) = doc_idx
                {
                    let follow = self.auto_follow;
                    if let Some(doc) = self.graph_tabs.get_mut(i) {
                        doc.props_node = Some(nid);
                        if follow
                            && let Some(info) = doc.graph.snarl.get_node_info(nid)
                        {
                            doc.view.focus_on(info.pos);
                        }
                    }
                }
            }
            Msg::NodeExecuted { node, images } => {
                let run_seq = self.run_seq;
                if let Some(i) = self.progress_doc_idx()
                    && let Some(doc) = self.graph_tabs.get_mut(i)
                    && let Some(&nid) = doc.node_map.get(&node)
                {
                    doc.outputs.entry(nid).or_default().extend(images);
                    let prefix = format!("run{run_seq}");
                    let imgs: Vec<_> = doc.outputs.iter().map(|(k, v)| (*k, v.clone())).collect();
                    doc.graph.populate_output_images(&prefix, imgs.into_iter());
                }
            }
            Msg::Done => {
                self.jobs_left = self.jobs_left.saturating_sub(1);
                self.progress = (0, 0);
                self.executing = None;
                if self.jobs_left == 0 {
                    self.running = false;
                    self.status = "Done".into();
                    host.haptic(Haptic::Success);
                    host.notify("ComfyUI", "Generation finished");
                    // New outputs should show up without a manual refresh (retry once for index lag).
                    if matches!(self.conn, Conn::Connected) {
                        self.refresh_gallery();
                        self.gallery_refresh_at = Some(ctx.input(|i| i.time) + 2.0);
                    }
                } else {
                    self.status = format!("{} still in queue", self.jobs_left);
                    host.haptic(Haptic::Light);
                }
            }
            Msg::Cancelled => {
                self.jobs_left = 0;
                self.running = false;
                self.progress = (0, 0);
                self.executing = None;
                self.preview = None;
                self.status = "Cancelled".into();
            }
            Msg::GenError(e) => {
                self.jobs_left = self.jobs_left.saturating_sub(1);
                self.progress = (0, 0);
                self.executing = None;
                if self.jobs_left == 0 {
                    self.running = false;
                }
                self.status = format!("Error: {e}");
                host.haptic(Haptic::Error);
            }
            Msg::Workflows(names) => {
                self.wf_loading = false;
                self.wf_names = names;
            }
            Msg::WorkflowLoaded { name, workflow, warnings } => {
                self.wf_loading = false;
                self.executing = None;
                match self.load_workflow_into_tab(name, &workflow) {
                    Ok(()) => {
                        if !warnings.is_empty() {
                            self.log.warn(format!(
                                "workflow loaded with {} warning(s) — see earlier log lines",
                                warnings.len()
                            ));
                        }
                        self.graph_status.clear();
                        self.wf_open = false;
                        self.tab = Tab::Graph;
                        self.graph_pane = GraphPane::Canvas;
                        if self.viewer.is_some() {
                            self.gallery_scroll_restore = Some(self.gallery_scroll_y);
                            self.viewer = None;
                        }
                        host.haptic(Haptic::Success);
                    }
                    Err(e) => {
                        self.graph_status = format!("Load failed: {e}");
                        self.log.error(format!("graph load: {e}"));
                        host.haptic(Haptic::Error);
                    }
                }
            }
            Msg::WorkflowSaved(name) => {
                self.saving = false;
                self.save_open = false;
                if let Some(doc) = self.active_doc_mut() {
                    doc.name = name.clone();
                }
                self.graph_status.clear();
                self.log.info(format!("saved workflow {name}"));
                host.haptic(Haptic::Success);
            }
            Msg::WorkflowError(e) => {
                self.wf_loading = false;
                self.saving = false;
                self.graph_status = elide(&e, 200);
                self.log.error(elide(&e, 200));
                host.haptic(Haptic::Error);
            }
            Msg::Gallery { generation, page } => {
                // A filter change bumps the generation and clears the listing; pages answering
                // the old query may still land afterwards and must not corrupt the fresh one.
                if generation != self.gallery_gen {
                    return;
                }
                self.gallery_loading = false;
                self.gallery_total = page.total;
                if page.offset == 0 {
                    self.gallery = page.items;
                } else {
                    self.gallery.extend(page.items);
                }
                self.gallery_status.clear();
                // With a model filter, album, or grouping active, the whole set has to be present
                // for the groups/results to be complete — keep paging (in big chunks) instead of
                // making the user tap "Load more". Capped so a huge namespace can't runaway.
                let loaded = self.gallery.len() as u64;
                if self.gallery_wants_all()
                    && loaded < self.gallery_total
                    && loaded < GALLERY_LOAD_ALL_CAP
                {
                    self.gallery_loading = true;
                    self.engine.as_ref().unwrap().gallery_list(
                        self.gallery_gen,
                        loaded,
                        self.gallery_page_size(),
                        &self.gallery_q,
                        &self.gallery_view,
                    );
                }
            }
            Msg::GalleryError(e) => {
                self.gallery_loading = false;
                if let Some(v) = &mut self.viewer {
                    v.loading = false;
                }
                self.gallery_status = elide(&e, 200);
            }
            Msg::Thumb { key, image } => {
                let (w, h) = (image.width(), image.height());
                if w > 0 {
                    let item_key = key.rsplit_once('#').map(|(k, _)| k).unwrap_or(&key);
                    self.thumb_aspects.insert(item_key.to_string(), h as f32 / w as f32);
                }
                let bytes = w * h * 4;
                let tex = ctx.load_texture(&key, image, egui::TextureOptions::LINEAR);
                self.thumbs.insert(key, tex, bytes);
            }
            Msg::FullImage { key, image, bytes } => {
                let (w, h) = (image.width(), image.height());
                if w > 0 {
                    self.thumb_aspects.insert(key.clone(), h as f32 / w as f32);
                }
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.tex = Some(ctx.load_texture(&key, image, egui::TextureOptions::LINEAR));
                    v.bytes = Some(bytes);
                    v.loading = false;
                }
            }
            Msg::ItemWorkflow { key, json } => {
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    let meta =
                        gallery::parse_workflow_meta_for(&json, Some(v.item.filename.as_str()));
                    self.log.info(format!(
                        "workflow meta {}: {} models, {} loras, prompt={}",
                        elide(&key, 48),
                        meta.models.len(),
                        meta.loras.len(),
                        meta.positive.as_ref().map(|p| p.len()).unwrap_or(0)
                    ));
                    v.meta = Some(meta);
                    v.workflow_json = Some(json);
                    v.item.has_workflow = true;
                    v.meta_loading = false;
                }
            }
            Msg::ItemWorkflowError { key, error } => {
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.meta_loading = false;
                    self.log.warn(format!("workflow meta {key}: {error}"));
                }
            }
            Msg::VideoReady { key, bytes } => {
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.bytes = Some(bytes.clone());
                    v.loading = false;
                    // Write the file where MediaMetadataRetriever can open it, then start playback.
                    // A fresh name per playback: the previous player's decode thread may still be
                    // winding down with its file open, and truncating that in place would yank the
                    // data out from under its decoder. (Player::drop unlinks its own file.)
                    if let Some(dir) = host.documents_dir() {
                        self.playback_seq += 1;
                        let path = format!("{dir}/playback_{}.mp4", self.playback_seq);
                        match std::fs::write(&path, &bytes) {
                            Ok(()) => {
                                self.player = Some(Player::start(
                                    path,
                                    key,
                                    ctx.clone(),
                                    self.log.clone(),
                                ));
                            }
                            Err(e) => self.log.error(format!("video cache write failed: {e}")),
                        }
                    }
                }
            }
            Msg::SaveToGallery { name, bytes } => {
                self.gallery_status = self.save_bytes(host, &bytes, &name);
            }
        }
    }

    /// Whether a Create-tab generation can be queued right now.
    fn can_queue_create(&self) -> Result<(), &'static str> {
        if let Some(missing) = self.params.missing_model_part() {
            return Err(missing);
        }
        if !matches!(self.conn, Conn::Connected)
            && !self.engine.as_ref().is_some_and(|e| e.is_connected())
        {
            return Err("Connect to the server first");
        }
        Ok(())
    }

    /// Tab that should receive execution highlights (Create-linked, else active).
    fn progress_doc_idx(&self) -> Option<usize> {
        if let Some(id) = self.create_graph_id {
            if let Some(i) = self.graph_tabs.iter().position(|d| d.id == id) {
                return Some(i);
            }
        }
        (self.active_graph < self.graph_tabs.len()).then_some(self.active_graph)
    }

    /// Catalogs the workflow builder needs, snapshotted for the worker thread.
    fn gen_ctx(&self) -> GenCtx {
        GenCtx {
            apps: self.apps.clone(),
            schemas: self.schemas.clone().unwrap_or_default(),
        }
    }

    /// Queue a Create-tab generation (adds to the server queue if something is already running).
    fn start_generation(&mut self, ctx: &egui::Context, host: &Host) {
        if let Err(e) = self.can_queue_create() {
            self.status = e.into();
            host.haptic(Haptic::Warning);
            return;
        }
        if self.params.randomize_seed {
            self.params.seed = random_seed();
        }
        // Keep the linked graph current so highlights / auto-follow work during the run.
        self.push_create_to_linked_graph();
        // txt2img: queue the linked graph itself so node ids match execution events.
        if self.params.mode == Mode::Txt2Img
            && let Some(id) = self.create_graph_id
            && let Some(idx) = self.graph_tabs.iter().position(|d| d.id == id)
        {
            let prev = self.active_graph;
            self.active_graph = idx;
            self.queue_graph(ctx, host);
            self.active_graph = prev;
            return;
        }
        let fresh = !self.running;
        if fresh {
            self.progress = (0, 0);
            self.preview = None;
            self.results.clear();
            self.result_view = None;
            self.run_total = 0;
            self.run_seen.clear();
        }
        // Best-effort map for img2img / unlinked runs (IDs match when topo order matches build).
        if let Some(id) = self.create_graph_id
            && let Some(doc) = self.graph_tabs.iter_mut().find(|d| d.id == id)
        {
            let (wg, mapping) = doc.graph.as_workflow_graph_with_mapping();
            doc.node_map = mapping.into_iter().map(|(nid, wid)| (wid.0, nid)).collect();
            if fresh {
                self.run_total = wg.into_workflow().0.len();
            }
        }
        self.running = true;
        self.jobs_left += 1;
        self.status = if !fresh {
            format!("Queued ({} in flight)", self.jobs_left)
        } else {
            "Queued".into()
        };
        let params = self.params.clone();
        let current = self.result_bytes.clone();
        let gcx = self.gen_ctx();
        self.enhance_note.clear();
        self.engine.as_mut().unwrap().generate(params, current, gcx);
        host.haptic(Haptic::Medium);
    }

    fn queue_graph(&mut self, ctx: &egui::Context, host: &Host) {
        let Some(schemas) = self.schemas.clone() else {
            self.graph_status = "Connect to the server first".into();
            return;
        };
        let Some(doc) = self.active_doc_mut() else { return };
        // UI export + convert respects bypass (mode 4); the snarl API path does not.
        let ui_json = doc.view.export_ui(&doc.graph, &schemas, &doc.bypassed);
        let converted = match uiwf::convert(&ui_json, &schemas) {
            Ok(c) => c,
            Err(e) => {
                self.graph_status = format!("Queue failed: {e}");
                host.haptic(Haptic::Error);
                return;
            }
        };
        let wf = converted.workflow;
        if wf.0.is_empty() {
            self.graph_status = "Graph is empty".into();
            return;
        }
        // export_ui uses snarl id + 1 as the UI/API node id.
        doc.node_map = doc
            .graph
            .snarl
            .node_ids()
            .filter(|(nid, _)| !doc.bypassed.contains(nid))
            .map(|(nid, _)| ((nid.0 as u32).saturating_add(1), nid))
            .filter(|(wid, _)| wf.0.contains_key(&WorkflowNodeId(*wid)))
            .collect();
        doc.outputs.clear();
        doc.graph.populate_output_images("none", std::iter::empty());
        let n = wf.0.len();
        self.run_seq += 1;
        self.run_total = n;
        self.run_seen.clear();
        ctx.forget_all_images();
        self.running = true;
        self.jobs_left += 1;
        self.status = "Queued".into();
        self.progress = (0, 0);
        self.preview = None;
        self.executing = None;
        self.graph_status.clear();
        self.engine.as_mut().unwrap().run_workflow(wf);
        host.haptic(Haptic::Medium);
    }

    /// Load the current Create params into a linked graph tab (reuse if already open).
    fn open_create_as_graph(&mut self, host: &Host) {
        if let Some(missing) = self.params.missing_model_part() {
            self.status = missing.into();
            host.haptic(Haptic::Warning);
            return;
        }
        if self.schemas.is_none() {
            self.status = "Connect to the server first".into();
            host.haptic(Haptic::Warning);
            return;
        }
        self.ensure_graph_tab();
        let schemas = self.schemas.clone().unwrap_or_default();
        // The real LoadImage reference is only known after the input is uploaded at queue time
        // (the server may namespace it into a subfolder), but this graph can itself be queued
        // from the Graph tab — so it has to have the img2img SHAPE. Building it with no input
        // would hand back an EmptyLatentImage, and queueing that silently makes a txt2img.
        let input = (self.params.mode == Mode::Img2Img)
            .then(|| crate::engine::INPUT_IMAGE_NAME.to_string());
        let placeholder = input.is_some();
        let (wf, _, report) = crate::workflow::build(&self.params, input, &self.apps, &schemas);
        self.enhance_note = report.note();
        self.executing = None;

        let linked = self
            .create_graph_id
            .and_then(|id| self.graph_tabs.iter().position(|d| d.id == id));
        let result = if let Some(idx) = linked {
            self.active_graph = idx;
            self.replace_workflow_in_tab(idx, "create.json".into(), &wf)
        } else {
            self.load_workflow_into_tab("create.json".into(), &wf)
        };
        match result {
            Ok(()) => {
                let (doc_id, mapped_total) = if let Some(doc) = self.active_doc_mut() {
                    let (wg, mapping) = doc.graph.as_workflow_graph_with_mapping();
                    doc.node_map =
                        mapping.into_iter().map(|(nid, wid)| (wid.0, nid)).collect();
                    (Some(doc.id), Some(wg.into_workflow().0.len()))
                } else {
                    (None, None)
                };
                if let Some(id) = doc_id {
                    self.create_graph_id = Some(id);
                }
                if self.running && self.run_total == 0
                    && let Some(n) = mapped_total
                {
                    self.run_total = n;
                }
                self.create_sync_fp = params_fingerprint(&self.params);
                self.create_graph_export_fp = self.linked_export_fingerprint().unwrap_or(0);
                self.create_sync_dirty_at = None;
                self.graph_status = if placeholder {
                    // Say it plainly: queueing this tab as-is will not find the image.
                    format!(
                        "LoadImage is a placeholder ('{}') — Create re-uploads the input at queue \
                         time. Queue from Create, or set a real filename here.",
                        crate::engine::INPUT_IMAGE_NAME
                    )
                } else {
                    String::new()
                };
                self.tab = Tab::Graph;
                self.graph_pane = GraphPane::Canvas;
                self.status.clear();
                host.haptic(Haptic::Success);
            }
            Err(e) => {
                self.status = format!("Open Graph failed: {e}");
                self.log.error(format!("open create as graph: {e}"));
                host.haptic(Haptic::Error);
            }
        }
    }

    /// Rebuild the Create-linked graph from current params (no tab switch).
    fn push_create_to_linked_graph(&mut self) {
        let Some(id) = self.create_graph_id else { return };
        let Some(idx) = self.graph_tabs.iter().position(|d| d.id == id) else {
            self.create_graph_id = None;
            return;
        };
        let Some(schemas) = self.schemas.clone() else { return };
        let input = (self.params.mode == Mode::Img2Img)
            .then(|| crate::engine::INPUT_IMAGE_NAME.to_string());
        let (wf, _, report) = crate::workflow::build(&self.params, input, &self.apps, &schemas);
        self.enhance_note = report.note();
        if self.replace_workflow_in_tab(idx, "create.json".into(), &wf).is_err() {
            return;
        }
        if let Some(doc) = self.graph_tabs.get_mut(idx) {
            let (wg, mapping) = doc.graph.as_workflow_graph_with_mapping();
            doc.node_map = mapping.into_iter().map(|(nid, wid)| (wid.0, nid)).collect();
            let _ = wg;
        }
        self.create_sync_fp = params_fingerprint(&self.params);
        self.create_graph_export_fp = self.linked_export_fingerprint().unwrap_or(0);
        self.create_sync_dirty_at = None;
    }

    /// Pull sampler / prompts / models from the linked graph into Create.
    fn pull_linked_graph_to_create(&mut self) {
        let Some(id) = self.create_graph_id else { return };
        let Some(schemas) = self.schemas.as_ref() else { return };
        let Some(doc) = self.graph_tabs.iter().find(|d| d.id == id) else {
            self.create_graph_id = None;
            return;
        };
        let exported = doc.view.export_ui(&doc.graph, schemas, &doc.bypassed);
        let Ok(body) = serde_json::to_string(&exported) else { return };
        let fp = str_fingerprint(&body);
        if fp == self.create_graph_export_fp {
            return;
        }
        let meta = gallery::parse_workflow_meta(&body);
        if meta.is_empty() {
            self.create_graph_export_fp = fp;
            return;
        }
        self.apply_image_meta(&meta);
        self.create_graph_export_fp = fp;
        // Avoid echoing the pull straight back as a Create → Graph rebuild.
        self.create_sync_fp = params_fingerprint(&self.params);
        self.create_sync_dirty_at = None;
    }

    fn linked_export_fingerprint(&self) -> Option<u64> {
        let id = self.create_graph_id?;
        let schemas = self.schemas.as_ref()?;
        let doc = self.graph_tabs.iter().find(|d| d.id == id)?;
        let exported = doc.view.export_ui(&doc.graph, schemas, &doc.bypassed);
        let body = serde_json::to_string(&exported).ok()?;
        Some(str_fingerprint(&body))
    }

    /// Debounced Create ↔ Graph sync for the linked tab.
    fn sync_create_graph_link(&mut self, now: f64) {
        let Some(id) = self.create_graph_id else { return };
        if !self.graph_tabs.iter().any(|d| d.id == id) {
            self.create_graph_id = None;
            return;
        }
        let fp = params_fingerprint(&self.params);
        if fp != self.create_sync_fp {
            match self.create_sync_dirty_at {
                None => self.create_sync_dirty_at = Some(now),
                Some(at) if now - at >= 0.4 => self.push_create_to_linked_graph(),
                Some(_) => {}
            }
        } else {
            self.create_sync_dirty_at = None;
        }
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
                // Also copy it into the phone's Photos gallery (Pictures/ComfyUI) via MediaStore.
                let mime = if name.to_lowercase().ends_with(".mp4") {
                    "video/mp4"
                } else if name.to_lowercase().ends_with(".webp") {
                    "image/webp"
                } else {
                    "image/png"
                };
                host.save_to_gallery(&path, name, mime);
                host.haptic(Haptic::Success);
                "Saved to Photos (Pictures/ComfyUI)".to_string()
            }
            Err(e) => {
                self.log.error(format!("save failed: {e}"));
                format!("Save failed: {e}")
            }
        }
    }

    fn save_result_at(&mut self, host: &Host, idx: usize) {
        let bytes = match self.results.get(idx) {
            Some((_, b)) => b.clone(),
            None => match self.result_bytes.clone() {
                Some(b) => b,
                None => return,
            },
        };
        self.save_counter += 1;
        let name = if self.results.len() > 1 {
            format!("output-{}-{}.png", self.save_counter, idx + 1)
        } else {
            format!("output-{}.png", self.save_counter)
        };
        self.note = self.save_bytes(host, &bytes, &name);
    }

    fn settings_path(host: &Host) -> Option<String> {
        host.documents_dir().map(|d| format!("{d}/comfyui_settings.json"))
    }

    /// Page size for gallery list / Load more, clamped to the server's accepted range.
    fn gallery_page_size(&self) -> u64 {
        self.gallery_page.clamp(20, GALLERY_PAGE_MAX)
    }

    fn settings_json(&self) -> Option<String> {
        let (workflow_name, workflow_json) = self.snapshot_workflow();
        let settings = Settings {
            server_url: self.server_url.clone(),
            api_key: self.api_key.clone(),
            username: self.username.clone(),
            session: self.session.clone(),
            params: self.params.clone(),
            gallery: self.gallery_view.clone(),
            gallery_q: self.gallery_q.clone(),
            gallery_page: self.gallery_page_size(),
            auto_follow: self.auto_follow,
            auto_arrange: self.auto_arrange,
            fonts: self.fonts.clone(),
            workflow_name,
            workflow_json,
            presets: self.presets.clone(),
            selected_preset: self.selected_preset.clone(),
            checkpoint_sort: self.checkpoint_sort,
            checkpoint_favorites: self.checkpoint_favorites.clone(),
            checkpoint_recent: self.checkpoint_recent.clone(),
            confirm_gallery_delete: self.confirm_gallery_delete,
        };
        serde_json::to_string_pretty(&settings).ok()
    }

    /// Active graph as UI-format JSON for persistence, when a schema-backed editor exists.
    fn snapshot_workflow(&self) -> (String, Option<String>) {
        if let (Some(doc), Some(schemas)) = (self.active_doc(), self.schemas.as_ref()) {
            if !doc.is_empty() {
                let exported = doc.view.export_ui(&doc.graph, schemas, &doc.bypassed);
                if let Ok(body) = serde_json::to_string(&exported) {
                    return (doc.name.clone(), Some(body));
                }
            }
        }
        // Keep the last restored snapshot until a live graph replaces it.
        match &self.restore_workflow {
            Some((name, json)) => (name.clone(), Some(json.clone())),
            None => (
                self.active_doc().map(|d| d.name.clone()).unwrap_or_default(),
                None,
            ),
        }
    }

    fn load_settings(&mut self, host: &Host) {
        let apps = AppSet::load(host.documents_dir().as_deref());
        for (file, why) in &apps.bad {
            self.log.error(format!("app '{file}': {why}"));
        }
        self.log.info(format!("{} enhance app(s) loaded", apps.by_id.len()));
        self.apps = Arc::new(apps);

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
            self.gallery_q = saved.gallery_q;
            self.gallery_page = saved.gallery_page.clamp(20, GALLERY_PAGE_MAX);
            self.auto_follow = saved.auto_follow;
            self.auto_arrange = saved.auto_arrange;
            self.fonts = saved.fonts;
            self.fonts.clamp();
            self.gallery_view.columns = self.gallery_view.columns.clamp(1, 3);
            self.presets = saved.presets;
            self.selected_preset = saved.selected_preset;
            self.checkpoint_sort = saved.checkpoint_sort;
            self.checkpoint_favorites = saved.checkpoint_favorites;
            self.checkpoint_recent = saved.checkpoint_recent;
            self.confirm_gallery_delete = saved.confirm_gallery_delete;
            if let Some(json) = saved.workflow_json.filter(|s| !s.trim().is_empty()) {
                self.restore_workflow = Some((saved.workflow_name, json));
            }
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
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.settings_pane, SettingsPane::Server, "Server");
            ui.selectable_value(
                &mut self.settings_pane,
                SettingsPane::Logs,
                format!("{} Logs", icons::LOGS),
            );
        });
        ui.separator();
        match self.settings_pane {
            SettingsPane::Server => self.settings_server_pane(ui, host),
            SettingsPane::Logs => self.logs_tab(ui, host),
        }
    }

    fn settings_server_pane(&mut self, ui: &mut egui::Ui, host: &Host) {
        crate::theme::scroll_vertical().auto_shrink([false, false]).show(ui, |ui| {
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
            ui.heading(format!("{} Gallery", icons::GALLERY));
            ui.group(|ui| {
                ui.label("Images per page");
                ui.add(
                    egui::Slider::new(&mut self.gallery_page, 20..=GALLERY_PAGE_MAX)
                        .suffix(" images")
                        .logarithmic(true),
                );
                ui.weak("How many rows Load more / preload fetches at once.");
                ui.add_space(4.0);
                ui.checkbox(&mut self.gallery_view.groups_open, "Open group headers by default");
                ui.checkbox(
                    &mut self.confirm_gallery_delete,
                    "Confirm before deleting gallery images",
                );
            });

            ui.add_space(12.0);
            ui.heading("Text size");
            ui.group(|ui| {
                let mut changed = false;
                ui.horizontal(|ui| {
                    ui.label("Heading");
                    changed |= ui
                        .add(egui::DragValue::new(&mut self.fonts.heading).range(12.0..=36.0).speed(0.5))
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("Body");
                    changed |= ui
                        .add(egui::DragValue::new(&mut self.fonts.body).range(10.0..=28.0).speed(0.5))
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("Button");
                    changed |= ui
                        .add(egui::DragValue::new(&mut self.fonts.button).range(10.0..=28.0).speed(0.5))
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("Small");
                    changed |= ui
                        .add(egui::DragValue::new(&mut self.fonts.small).range(8.0..=20.0).speed(0.5))
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("Mono");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut self.fonts.monospace)
                                .range(9.0..=24.0)
                                .speed(0.5),
                        )
                        .changed();
                });
                if changed {
                    self.fonts.clamp();
                    crate::theme::apply_fonts(ui.ctx(), &self.fonts);
                }
                if ui.button("Reset text sizes").clicked() {
                    self.fonts = FontSizes::default();
                    crate::theme::apply_fonts(ui.ctx(), &self.fonts);
                }
            });

            ui.add_space(12.0);
            ui.heading(format!("{} Graph", icons::GRAPH));
            ui.group(|ui| {
                ui.checkbox(&mut self.auto_follow, "Auto-follow executing node");
                ui.checkbox(
                    &mut self.auto_arrange,
                    "Auto-arrange when you open a loaded workflow",
                );
                ui.weak("The open workflow is saved automatically and restored after connect.");
            });

            ui.add_space(12.0);
            ui.weak("Server, key, account and generation settings save automatically.");
            ui.add_space(12.0);
        });
    }

    fn create_pane_bar(&mut self, ui: &mut egui::Ui) {
        let prev = self.create_pane;
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.create_pane, CreatePane::Main, "Main");
            let model_n = self.checkpoints.len() + self.unets.len();
            ui.selectable_value(
                &mut self.create_pane,
                CreatePane::Models,
                if model_n > 0 { format!("Models ({model_n})") } else { "Models".into() },
            );
            let lora_n = self.params.loras.len();
            ui.selectable_value(
                &mut self.create_pane,
                CreatePane::Loras,
                if lora_n > 0 { format!("LoRAs ({lora_n})") } else { "LoRAs".into() },
            );
            let app_n = self.params.apps.iter().filter(|a| a.enabled).count();
            ui.selectable_value(
                &mut self.create_pane,
                CreatePane::Enhance,
                if app_n > 0 { format!("Enhance ({app_n})") } else { "Enhance".into() },
            );
            let preset_n = self.presets.len();
            ui.selectable_value(
                &mut self.create_pane,
                CreatePane::Presets,
                if preset_n > 0 { format!("Presets ({preset_n})") } else { "Presets".into() },
            );
        });
        if self.create_pane == CreatePane::Models && prev != CreatePane::Models {
            self.checkpoints_force_collapse = true;
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui, host: &Host) {
        // The theme's roomy button padding makes sliders/combos tall enough to graze the next grid
        // row; trim the interactive height a little so each row keeps clear of the one below.
        ui.spacing_mut().interact_size.y = 20.0;
        match self.create_pane {
            CreatePane::Main => self.create_main_pane(ui, host),
            CreatePane::Models => self.create_models_pane(ui),
            CreatePane::Loras => self.create_loras_pane(ui),
            CreatePane::Enhance => self.create_enhance_pane(ui),
            CreatePane::Presets => self.create_presets_pane(ui, host),
        }
    }

    /// Debounced fetch of the img2img "From URL" preview thumbnail.
    fn tick_img2img_url_preview(&mut self, ctx: &egui::Context) {
        let url = self.params.input_url.trim().to_string();
        let now = ctx.input(|i| i.time);
        if url != self.img2img_url_pending {
            self.img2img_url_pending = url.clone();
            self.img2img_url_pending_at = now;
        }
        if url.is_empty() {
            self.img2img_url_tex = None;
            self.img2img_url_key.clear();
            self.img2img_url_req.clear();
            self.img2img_url_loading = false;
            self.img2img_url_err.clear();
            return;
        }
        if url == self.img2img_url_key || url == self.img2img_url_req {
            return;
        }
        let looks_ok = (url.starts_with("http://") || url.starts_with("https://")) && url.len() > 12;
        if !looks_ok {
            return;
        }
        let wait = 0.45 - (now - self.img2img_url_pending_at);
        if wait > 0.0 {
            ctx.request_repaint_after(Duration::from_secs_f64(wait));
            return;
        }
        self.img2img_url_req = url.clone();
        self.img2img_url_loading = true;
        self.img2img_url_err.clear();
        if let Some(engine) = self.engine.as_ref() {
            engine.fetch_img2img_url_preview(url);
        }
    }

    fn create_main_pane(&mut self, ui: &mut egui::Ui, _host: &Host) {
        let model_file = self.params.model_file().to_string();
        let ckpt_label = if model_file.is_empty() {
            "no model".to_string()
        } else {
            self.checkpoint_catalog
                .entry(&model_file)
                .map(|e| e.display_name().to_string())
                .unwrap_or_else(|| elide(&model_file, 40))
        };
        let preset_label = if self.selected_preset.is_empty() {
            "custom".to_string()
        } else {
            elide(&self.selected_preset, 24)
        };
        let app_n = self.params.apps.iter().filter(|a| a.enabled).count();
        let enhance_label = if app_n > 0 { format!(" · +{app_n} enhance") } else { String::new() };
        ui.weak(format!("{ckpt_label} · {preset_label}{enhance_label}"));

        // Diffusion models (Anima, Flux, Qwen-Image) ship without a text encoder or VAE, so those
        // are picked here. select_model seeds them; this is the override.
        if self.params.model_kind == ModelKind::Diffusion {
            ui.group(|ui| {
                ui.label("Diffusion model companions");

                let clip_n = self.params.clip_names.len().max(1);
                for i in 0..clip_n {
                    if self.params.clip_names.len() <= i {
                        self.params.clip_names.push(String::new());
                    }
                    ui.horizontal(|ui| {
                        ui.label(if i == 0 { "Text encoder" } else { "  + encoder" });
                        if i > 0 && ui.small_button(icons::TRASH).clicked() {
                            self.params.clip_names.remove(i);
                        }
                    });
                    if i < self.params.clip_names.len() {
                        let mut val = self.params.clip_names[i].clone();
                        combo_full(ui, &format!("clip_name_{i}"), &mut val, &self.clip_files);
                        self.params.clip_names[i] = val;
                    }
                }
                // Two encoders is the cap: DualCLIPLoader is the widest typed loader available.
                if self.params.clip_names.len() < 2 && ui.button("+ second encoder").clicked() {
                    self.params.clip_names.push(String::new());
                }

                ui.label("VAE");
                combo_full(ui, "vae_name", &mut self.params.vae_name, &self.vaes);

                egui::CollapsingHeader::new("Advanced").id_salt("diffusion_adv").show(ui, |ui| {
                    ui.label("Encoder type");
                    let mut ty = self.params.effective_clip_type();
                    combo_full(ui, "clip_type", &mut ty, &self.clip_types);
                    self.params.clip_type = ty;
                    ui.label("Weight dtype");
                    combo_full(ui, "weight_dtype", &mut self.params.weight_dtype, &self.weight_dtypes);
                    if !self.clip_devices.is_empty() {
                        ui.label("Encoder device");
                        combo_full(ui, "clip_device", &mut self.params.clip_device, &self.clip_devices);
                    }
                });

                if let Some(missing) = self.params.missing_model_part() {
                    ui.colored_label(ui.visuals().warn_fg_color, missing);
                }
            });
        }

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
                        self.tick_img2img_url_preview(ui.ctx());
                        ui.horizontal(|ui| {
                            if let Some(tex) = &self.img2img_url_tex {
                                let sized = egui::load::SizedTexture::from_handle(tex);
                                ui.add(egui::Image::new(sized).max_size(egui::vec2(96.0, 96.0)));
                            } else if self.img2img_url_loading {
                                ui.spinner();
                                ui.weak("loading preview…");
                            } else if !self.img2img_url_err.is_empty() {
                                ui.weak(elide(&self.img2img_url_err, 64));
                            }
                        });
                    }
                    Img2ImgSource::CurrentOutput if self.result_bytes.is_none() => {
                        ui.weak("Generate an image first to use it as input.");
                    }
                    Img2ImgSource::CurrentOutput => {
                        if let Some(tex) = &self.result {
                            let sized = egui::load::SizedTexture::from_handle(tex);
                            ui.add(egui::Image::new(sized).max_size(egui::vec2(96.0, 96.0)));
                        }
                    }
                }
                full_width_slider(ui, "Denoise", |ui, w| {
                    ui.add_sized(
                        [w, 24.0],
                        egui::Slider::new(&mut self.params.denoise, 0.0..=1.0),
                    );
                });
            });
        }

        ui.label("Prompt");
        ui.add(
            egui::TextEdit::multiline(&mut self.params.positive)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("what you want to see"),
        );
        ui.label("LoRA triggers");
        ui.add(
            egui::TextEdit::multiline(&mut self.params.lora_triggers)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("trigger words from LoRAs (auto-filled on Add)"),
        );
        ui.label("Negative");
        ui.add(
            egui::TextEdit::multiline(&mut self.params.negative)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("what to avoid"),
        );

        full_width_slider(ui, "Steps", |ui, w| {
            ui.add_sized(
                [w, 24.0],
                egui::Slider::new(&mut self.params.steps, 5..=150)
                    .step_by(5.0)
                    .clamping(egui::SliderClamping::Always),
            );
        });
        full_width_slider(ui, "CFG", |ui, w| {
            ui.add_sized(
                [w, 24.0],
                egui::Slider::new(&mut self.params.cfg, 1.0..=20.0)
                    .clamping(egui::SliderClamping::Always),
            );
        });
        full_width_slider(ui, "Batch", |ui, w| {
            ui.add_sized(
                [w, 24.0],
                egui::Slider::new(&mut self.params.batch_size, 1..=8).text("images"),
            );
        });

        ui.add_space(4.0);
        ui.label("Size");
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut self.params.width).range(64..=2048).speed(8.0));
            ui.label("×");
            ui.add(egui::DragValue::new(&mut self.params.height).range(64..=2048).speed(8.0));
            size_preset_combo(ui, &mut self.params.width, &mut self.params.height);
        });
        // An enabled step may render at a different size than the one above (hi-res fix works by
        // generating small and scaling up). Show what it will actually do — the stored value is
        // never touched, so this line simply disappears when the step is removed.
        self.param_override_note(ui);

        ui.add_space(4.0);
        ui.label("Sampler");
        combo_full(ui, "sampler", &mut self.params.sampler, &self.samplers);
        ui.label("Scheduler");
        combo_full(ui, "scheduler", &mut self.params.scheduler, &self.schedulers);

        ui.add_space(4.0);
        ui.label("Seed");
        ui.horizontal(|ui| {
            ui.add_enabled(
                !self.params.randomize_seed,
                egui::DragValue::new(&mut self.params.seed).speed(1.0),
            );
            ui.checkbox(&mut self.params.randomize_seed, "random");
        });

        if !self.params.loras.is_empty() {
            ui.add_space(4.0);
            ui.weak(format!(
                "{} LoRA(s) active — edit on the LoRAs tab",
                self.params.loras.len()
            ));
        }

        // An enabled upscale or face fix is never invisible from the main flow.
        if !self.params.apps.is_empty() {
            ui.add_space(4.0);
            let names: Vec<String> = self
                .params
                .apps
                .iter()
                .filter(|s| s.enabled)
                .map(|s| {
                    self.apps.get(&s.app).map(|d| d.name.clone()).unwrap_or_else(|| s.app.clone())
                })
                .collect();
            let label = if names.is_empty() {
                "Enhance steps off".to_string()
            } else {
                format!("Enhance: {}", names.join(" -> "))
            };
            if ui.link(elide(&label, 60)).clicked() {
                self.create_pane = CreatePane::Enhance;
            }
        }

        // Room for the floating action bubble.
        ui.add_space(72.0);
    }

    fn create_models_pane(&mut self, ui: &mut egui::Ui) {
        let list_w = (ui.clip_rect().width() - 12.0).clamp(160.0, ui.available_width());
        ui.set_max_width(list_w);

        let catalog_n = self.checkpoint_catalog.checkpoints.len();
        if catalog_n == 0 {
            ui.weak("No checkpoint catalog yet — showing installed models.");
        } else {
            ui.weak(format!("Catalog: {catalog_n} entries · grouped by family"));
        }

        if !self.unets.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.models_kind_filter, None, "All");
                ui.selectable_value(
                    &mut self.models_kind_filter,
                    Some(ModelKind::Checkpoint),
                    format!("Checkpoints ({})", self.checkpoints.len()),
                );
                ui.selectable_value(
                    &mut self.models_kind_filter,
                    Some(ModelKind::Diffusion),
                    format!("Diffusion ({})", self.unets.len()),
                );
            });
        }

        ui.add(
            egui::TextEdit::singleline(&mut self.ckpt_filter)
                .hint_text("filter models")
                .desired_width(list_w - 8.0),
        );

        ui.horizontal(|ui| {
            ui.label(format!("{} Sort", icons::SORT));
            for sort in [CheckpointSort::Name, CheckpointSort::Recent] {
                ui.selectable_value(&mut self.checkpoint_sort, sort, sort.label());
            }
        });

        type ModelRow = (String, ModelKind, Option<crate::types::CheckpointEntry>);
        let filter = self.ckpt_filter.to_lowercase();
        let current = self.params.model_file().to_string();
        let recent_rank: HashMap<String, usize> = self
            .checkpoint_recent
            .iter()
            .enumerate()
            .map(|(i, f)| (f.clone(), i))
            .collect();
        let sort = self.checkpoint_sort;

        let listed: Vec<(String, ModelKind)> = self
            .checkpoints
            .iter()
            .map(|f| (f.clone(), ModelKind::Checkpoint))
            .chain(self.unets.iter().map(|f| (f.clone(), ModelKind::Diffusion)))
            .filter(|(_, k)| self.models_kind_filter.is_none_or(|want| want == *k))
            .collect();

        let mut rows: Vec<ModelRow> = Vec::new();
        for (file, kind) in listed {
            let meta = self.checkpoint_catalog.entry(&file).cloned();
            let name = meta
                .as_ref()
                .map(|e| e.display_name().to_string())
                .unwrap_or_else(|| file_basename(&file).to_string());
            let family = checkpoint_family(meta.as_ref());
            let hay = format!(
                "{family} {name} {file} {}",
                meta.as_ref().and_then(|e| e.version.as_deref()).unwrap_or("")
            )
            .to_lowercase();
            if !filter.is_empty() && !hay.contains(&filter) {
                continue;
            }
            rows.push((file, kind, meta));
        }

        let name_of = |r: &ModelRow| {
            r.2.as_ref()
                .map(|e| e.display_name().to_string())
                .unwrap_or_else(|| file_basename(&r.0).to_string())
                .to_lowercase()
        };
        let cmp_rows = |a: &ModelRow, b: &ModelRow| -> std::cmp::Ordering {
            match sort {
                CheckpointSort::Recent => {
                    let ra = recent_rank.get(&a.0).copied().unwrap_or(usize::MAX);
                    let rb = recent_rank.get(&b.0).copied().unwrap_or(usize::MAX);
                    ra.cmp(&rb).then_with(|| name_of(a).cmp(&name_of(b)))
                }
                CheckpointSort::Name => name_of(a).cmp(&name_of(b)).then_with(|| {
                    let av = a
                        .2
                        .as_ref()
                        .map(|e| e.version_label())
                        .unwrap_or_else(|| a.0.clone())
                        .to_lowercase();
                    let bv = b
                        .2
                        .as_ref()
                        .map(|e| e.version_label())
                        .unwrap_or_else(|| b.0.clone())
                        .to_lowercase();
                    av.cmp(&bv)
                }),
            }
        };

        let fav_files: HashSet<String> = rows
            .iter()
            .filter(|(f, _, _)| self.is_checkpoint_favorite(f))
            .map(|(f, _, _)| f.clone())
            .collect();

        // Favorites: Name → versions (same nesting as family groups).
        let mut fav_groups: std::collections::BTreeMap<String, Vec<ModelRow>> =
            std::collections::BTreeMap::new();
        for (file, kind, meta) in rows.iter().filter(|(f, _, _)| fav_files.contains(f)) {
            let group = meta
                .as_ref()
                .map(|e| e.display_name().to_string())
                .unwrap_or_else(|| file_basename(file).to_string());
            fav_groups
                .entry(group)
                .or_default()
                .push((file.clone(), *kind, meta.clone()));
        }
        for versions in fav_groups.values_mut() {
            versions.sort_by(cmp_rows);
        }
        let mut fav_group_names: Vec<String> = fav_groups.keys().cloned().collect();
        if sort == CheckpointSort::Recent {
            fav_group_names.sort_by(|a, b| {
                let best = |g: &str| {
                    fav_groups
                        .get(g)
                        .into_iter()
                        .flatten()
                        .map(|(f, _, _)| recent_rank.get(f).copied().unwrap_or(usize::MAX))
                        .min()
                        .unwrap_or(usize::MAX)
                };
                best(a).cmp(&best(b)).then_with(|| a.cmp(b))
            });
        }

        let mut families: std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, Vec<ModelRow>>,
        > = std::collections::BTreeMap::new();
        for (file, kind, meta) in rows.into_iter().filter(|(f, _, _)| !fav_files.contains(f)) {
            let family = checkpoint_family(meta.as_ref());
            let group = meta
                .as_ref()
                .map(|e| e.display_name().to_string())
                .unwrap_or_else(|| file_basename(&file).to_string());
            families
                .entry(family)
                .or_default()
                .entry(group)
                .or_default()
                .push((file, kind, meta));
        }
        for groups in families.values_mut() {
            for versions in groups.values_mut() {
                versions.sort_by(cmp_rows);
            }
        }

        let mut family_order: Vec<String> = families.keys().cloned().collect();
        if sort == CheckpointSort::Recent {
            family_order.sort_by(|a, b| {
                let best = |fam: &str| {
                    families
                        .get(fam)
                        .into_iter()
                        .flat_map(|g| g.values())
                        .flatten()
                        .map(|(f, _, _)| recent_rank.get(f).copied().unwrap_or(usize::MAX))
                        .min()
                        .unwrap_or(usize::MAX)
                };
                best(a).cmp(&best(b)).then_with(|| a.cmp(b))
            });
        }

        let mut pick: Option<(String, ModelKind)> = None;
        let mut toggle_fav: Option<String> = None;
        let force_closed = self.checkpoints_force_collapse;
        let mut shown = 0usize;

        if !fav_groups.is_empty() {
            let fav_n: usize = fav_groups.values().map(|v| v.len()).sum();
            let any_selected = fav_groups.values().flatten().any(|(f, _, _)| *f == current);
            let header = if any_selected {
                format!("{} {} Favorites ({fav_n})", icons::CHECK, icons::STAR)
            } else {
                format!("{} Favorites ({fav_n})", icons::STAR)
            };
            egui::CollapsingHeader::new(header)
                .id_salt("ckpt_favorites")
                .default_open(true)
                .open(if force_closed { Some(true) } else { None })
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    for group_name in &fav_group_names {
                        let Some(versions) = fav_groups.get(group_name) else {
                            continue;
                        };
                        if shown >= 120 {
                            break;
                        }
                        let any_sel = versions.iter().any(|(f, _, _)| *f == current);
                        let max_name_w = (ui.available_width() - 22.0).max(32.0);
                        let name_label =
                            elide_width(ui, &sanitize_ui_text(ui, group_name), max_name_w);
                        let group_header = if any_sel {
                            format!("{} {name_label}", icons::CHECK)
                        } else {
                            name_label
                        };
                        egui::CollapsingHeader::new(group_header)
                            .id_salt(("ckpt_fav_group", group_name.as_str()))
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.set_max_width(ui.available_width());
                                for (file, kind, meta) in versions {
                                    model_version_row(
                                        ui,
                                        file,
                                        *kind,
                                        meta,
                                        &current,
                                        true,
                                        "ckpt_fav",
                                        &mut pick,
                                        &mut toggle_fav,
                                    );
                                    shown += 1;
                                }
                            });
                    }
                });
        }

        for family in &family_order {
            let Some(groups) = families.get(family) else {
                continue;
            };
            if shown >= 120 {
                ui.weak("… more — type to filter");
                break;
            }
            let family_count: usize = groups.values().map(|v| v.len()).sum();
            let any_selected = groups.values().flatten().any(|(f, _, _)| *f == current);
            let family_header = if any_selected {
                format!("{} {family} ({family_count})", icons::CHECK)
            } else {
                format!("{family} ({family_count})")
            };
            egui::CollapsingHeader::new(family_header)
                .id_salt(("ckpt_family", family.as_str()))
                .default_open(false)
                .open(if force_closed { Some(false) } else { None })
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    let mut group_names: Vec<&String> = groups.keys().collect();
                    if sort == CheckpointSort::Recent {
                        group_names.sort_by(|a, b| {
                            let best = |g: &String| {
                                groups
                                    .get(g)
                                    .into_iter()
                                    .flatten()
                                    .map(|(f, _, _)| {
                                        recent_rank.get(f).copied().unwrap_or(usize::MAX)
                                    })
                                    .min()
                                    .unwrap_or(usize::MAX)
                            };
                            best(a).cmp(&best(b)).then_with(|| a.cmp(b))
                        });
                    }
                    for group_name in group_names {
                        let Some(versions) = groups.get(group_name) else {
                            continue;
                        };
                        // Always Name, then versions — never flatten a lone version to the family list.
                        let any_sel = versions.iter().any(|(f, _, _)| *f == current);
                        let max_name_w = (ui.available_width() - 22.0).max(32.0);
                        let name_label =
                            elide_width(ui, &sanitize_ui_text(ui, group_name), max_name_w);
                        let group_header = if any_sel {
                            format!("{} {name_label}", icons::CHECK)
                        } else {
                            name_label
                        };
                        egui::CollapsingHeader::new(group_header)
                            .id_salt(("ckpt_group", family.as_str(), group_name.as_str()))
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.set_max_width(ui.available_width());
                                for (file, kind, meta) in versions {
                                    let fav = fav_files.contains(file);
                                    model_version_row(
                                        ui,
                                        file,
                                        *kind,
                                        meta,
                                        &current,
                                        fav,
                                        "ckpt_ver",
                                        &mut pick,
                                        &mut toggle_fav,
                                    );
                                    shown += 1;
                                }
                            });
                    }
                });
        }

        self.checkpoints_force_collapse = false;
        if let Some((file, kind)) = pick {
            self.select_model(&file, Some(kind));
        }
        if let Some(file) = toggle_fav {
            self.toggle_checkpoint_favorite(&file);
        }
        if fav_groups.is_empty() && families.is_empty() {
            let empty = self.checkpoints.is_empty() && self.unets.is_empty();
            ui.weak(if empty {
                "No models on the server."
            } else {
                "No matches."
            });
        }
    }

    fn create_presets_pane(&mut self, ui: &mut egui::Ui, host: &Host) {
        let list_w = (ui.clip_rect().width() - 12.0).clamp(160.0, ui.available_width());
        ui.set_max_width(list_w);

        ui.horizontal(|ui| {
            if ui.button(icons::SAVE).on_hover_text("Save current Create settings as a preset").clicked()
            {
                self.preset_name_edit = self.selected_preset.clone();
                self.preset_save_open = true;
            }
            let can_del = !self.selected_preset.is_empty();
            if ui
                .add_enabled(can_del, egui::Button::new(icons::TRASH))
                .on_hover_text("Delete selected preset")
                .clicked()
            {
                self.delete_selected_preset();
                host.haptic(Haptic::Warning);
            }
        });

        if self.preset_save_open {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.preset_name_edit)
                        .hint_text("preset name")
                        .desired_width((list_w - 100.0).max(100.0)),
                );
                let named = !self.preset_name_edit.trim().is_empty();
                if ui.add_enabled(named, egui::Button::new(icons::CHECK)).clicked() {
                    self.save_preset(self.preset_name_edit.trim().to_string());
                    self.preset_save_open = false;
                    host.haptic(Haptic::Success);
                }
                if ui.button("Cancel").clicked() {
                    self.preset_save_open = false;
                }
            });
        }

        if self.presets.is_empty() {
            ui.weak("No presets yet — tap 💾 to save the current Create settings.");
            return;
        }

        let mut apply: Option<String> = None;
        let mut delete: Option<String> = None;
        for preset in self.presets.clone() {
            let selected = self.selected_preset == preset.name;
            let header = if selected {
                format!("{} {}", icons::CHECK, elide(&preset.name, 28))
            } else {
                elide(&preset.name, 32)
            };
            ui.horizontal(|ui| {
                ui.set_max_width(list_w);
                let (use_clicked, trash_clicked) = ui
                    .with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        let trash = ui.small_button(icons::TRASH).clicked();
                        let use_btn = ui
                            .add_enabled(!selected, egui::Button::new("Use").small())
                            .clicked();
                        egui::CollapsingHeader::new(header)
                            .id_salt(("preset_row", preset.name.as_str()))
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.set_max_width((list_w - 80.0).max(100.0));
                                preset_meta_body(ui, &preset);
                            });
                        (use_btn, trash)
                    })
                    .inner;
                if use_clicked {
                    apply = Some(preset.name.clone());
                }
                if trash_clicked {
                    delete = Some(preset.name.clone());
                }
            });
        }
        if let Some(name) = apply {
            self.apply_preset(&name);
            host.haptic(Haptic::Light);
        }
        if let Some(name) = delete {
            self.presets.retain(|p| p.name != name);
            if self.selected_preset == name {
                self.selected_preset.clear();
            }
            host.haptic(Haptic::Warning);
        }
    }

    /// Which loader a model needs: the caller's hint, else the catalog's `directory`, else which
    /// of the server's two lists it appears in.
    fn kind_for(&self, file: &str, hint: Option<ModelKind>) -> ModelKind {
        if let Some(k) = hint {
            return k;
        }
        if let Some(k) = self.checkpoint_catalog.entry(file).and_then(|e| e.model_kind()) {
            return k;
        }
        let known = |list: &[String]| list.iter().any(|f| f == file);
        if known(&self.unets) && !known(&self.checkpoints) {
            ModelKind::Diffusion
        } else {
            ModelKind::Checkpoint
        }
    }

    fn select_model(&mut self, file: &str, hint: Option<ModelKind>) {
        let kind = self.kind_for(file, hint);
        self.params.model_kind = kind;
        match kind {
            ModelKind::Checkpoint => self.params.checkpoint = file.to_string(),
            ModelKind::Diffusion => self.params.unet_name = file.to_string(),
        }
        if let Some(rec) = self
            .checkpoint_catalog
            .entry(file)
            .and_then(|e| e.recommended.as_ref())
            .cloned()
        {
            if let Some(v) = rec.steps {
                self.params.steps = v;
            }
            if let Some(v) = rec.cfg {
                self.params.cfg = v;
            }
            if let Some(v) = rec.width {
                self.params.width = v;
            }
            if let Some(v) = rec.height {
                self.params.height = v;
            }
            if let Some(name) = rec.sampler.as_ref().and_then(|s| match_sampler_name(s, &self.samplers))
            {
                self.params.sampler = name;
            }
            if let Some(name) =
                rec.scheduler.as_ref().and_then(|s| match_sampler_name(s, &self.schedulers))
            {
                self.params.scheduler = name;
            }
        }
        if kind == ModelKind::Diffusion {
            self.resolve_companions(Companions::Seed);
        }
        self.selected_preset.clear();
        self.touch_checkpoint_recent(file);
    }

    /// Push `file` to the front of the MRU list (deduped, capped).
    fn touch_checkpoint_recent(&mut self, file: &str) {
        self.checkpoint_recent.retain(|f| f != file);
        self.checkpoint_recent.insert(0, file.to_string());
        self.checkpoint_recent.truncate(CHECKPOINT_RECENT_MAX);
    }

    /// Catalog favorite or a local pin.
    fn is_checkpoint_favorite(&self, file: &str) -> bool {
        self.checkpoint_favorites.iter().any(|f| f == file)
            || self.checkpoint_catalog.entry(file).map(|e| e.favorite).unwrap_or(false)
    }

    /// Toggle a local pin. Catalog `favorite` entries stay starred from server metadata.
    fn toggle_checkpoint_favorite(&mut self, file: &str) {
        if let Some(i) = self.checkpoint_favorites.iter().position(|f| f == file) {
            self.checkpoint_favorites.remove(i);
            return;
        }
        let catalog_fav =
            self.checkpoint_catalog.entry(file).map(|e| e.favorite).unwrap_or(false);
        if !catalog_fav {
            self.checkpoint_favorites.push(file.to_string());
        }
    }

    /// Fill in the text encoder / VAE a diffusion model needs.
    ///
    /// [`Companions::Seed`] runs when the user picks a different model, so the catalog's
    /// recommendation outranks the companions left over from the previous one. [`Companions::Repair`]
    /// runs on reconnect and preset load, where whatever is already selected is the user's own
    /// choice and outranks the recommendation.
    ///
    /// Empty option lists mean "not connected yet", never "the server has none": those fields are
    /// left untouched rather than blanked, so an offline preset load keeps its saved companions.
    fn resolve_companions(&mut self, mode: Companions) {
        let rec = self
            .checkpoint_catalog
            .entry(self.params.model_file())
            .and_then(|e| e.recommended.as_ref())
            .cloned()
            .unwrap_or_default();
        let bases = self.model_bases_for(self.params.model_file());
        let seeding = mode == Companions::Seed;

        let clips = self.clip_files.clone();
        if !clips.is_empty() {
            let hinted: Vec<String> = rec
                .clip_names
                .unwrap_or_default()
                .iter()
                .filter_map(|n| installed_match(n, &clips))
                .collect();
            let current: Vec<String> = self
                .params
                .active_clips()
                .iter()
                .filter_map(|n| installed_match(n, &clips))
                .collect();
            let (first, second) =
                if seeding { (hinted, current) } else { (current, hinted) };
            self.params.clip_names = if !first.is_empty() {
                first
            } else if !second.is_empty() {
                second
            } else {
                best_by_bases(&clips, &bases)
                    .or_else(|| self.schemas_enum_default("CLIPLoader", "clip_name", &clips))
                    .or_else(|| (clips.len() == 1).then(|| clips[0].clone()))
                    .map(|c| vec![c])
                    .unwrap_or_default()
            };
        }

        let vaes = self.vaes.clone();
        if !vaes.is_empty() {
            let hint = rec.vae.as_deref().and_then(|n| installed_match(n, &vaes));
            let current = installed_match(&self.params.vae_name, &vaes);
            let (first, second) = if seeding { (hint, current) } else { (current, hint) };
            self.params.vae_name = first
                .or(second)
                .or_else(|| best_by_bases(&vaes, &bases))
                .or_else(|| self.schemas_enum_default("VAELoader", "vae_name", &vaes))
                .or_else(|| (vaes.len() == 1).then(|| vaes[0].clone()))
                .unwrap_or_default();
        }

        // Deliberately not base-matched: the proven Anima graph uses `stable_diffusion` even
        // though its encoder is Qwen3, so name overlap would pick the wrong type.
        let types = self.clip_types.clone();
        if !types.is_empty() {
            let hint = rec.clip_type.as_deref().and_then(|n| installed_match(n, &types));
            let current = installed_match(&self.params.clip_type, &types);
            let (first, second) = if seeding { (hint, current) } else { (current, hint) };
            self.params.clip_type = first
                .or(second)
                .or_else(|| self.schemas_enum_default("CLIPLoader", "type", &types))
                .or_else(|| installed_match("stable_diffusion", &types))
                .unwrap_or_default();
        }

        let dtypes = self.weight_dtypes.clone();
        if !dtypes.is_empty() {
            let hint = rec.weight_dtype.as_deref().and_then(|n| installed_match(n, &dtypes));
            let current = installed_match(&self.params.weight_dtype, &dtypes);
            let (first, second) = if seeding { (hint, current) } else { (current, hint) };
            self.params.weight_dtype = first.or(second).unwrap_or_default();
        }
    }

    /// The server's declared default for an enum input, kept only if it is a real option.
    fn schemas_enum_default(&self, class: &str, input: &str, options: &[String]) -> Option<String> {
        let d = self.schemas.as_ref()?.enum_default(class, input)?;
        installed_match(&d, options)
    }

    /// Base tags for the selected checkpoint (checkpoint catalog first, then LoRA catalog map).
    fn model_bases_for(&self, checkpoint: &str) -> Vec<String> {
        let from_ckpt = self.checkpoint_catalog.bases_for(checkpoint);
        if !from_ckpt.is_empty() {
            return from_ckpt;
        }
        self.lora_catalog.bases_for_checkpoint(checkpoint)
    }

    fn create_loras_pane(&mut self, ui: &mut egui::Ui) {
        // ScrollArea can report infinite width; pin to the clip so trailing buttons stay visible.
        let list_w = (ui.clip_rect().width() - 12.0).clamp(160.0, ui.available_width());
        ui.set_max_width(list_w);

        let catalog_n = self.lora_catalog.loras.len();
        let model_bases = self.model_bases_for(self.params.model_file());
        if catalog_n == 0 {
            ui.weak("No LoRA catalog on the server yet — showing installed LoRAs with default strength.");
        } else if self.params.model_file().is_empty() {
            ui.weak("Pick a model to filter LoRAs by base tags.");
        } else if model_bases.is_empty() {
            ui.weak("Selected model has no base tags — only universal LoRAs are shown.");
        } else {
            ui.weak(format!(
                "Filtered to bases: {} ({} catalog entries)",
                model_bases.join(", "),
                catalog_n
            ));
        }

        if !self.params.loras.is_empty() {
            ui.label("Active");
            let mut remove: Option<usize> = None;
            for (i, lora) in self.params.loras.clone().iter().enumerate() {
                let title = self
                    .lora_catalog
                    .entry(&lora.file)
                    .map(|e| e.display_name().to_string())
                    .unwrap_or_else(|| lora.file.clone());
                let meta = self.lora_catalog.entry(&lora.file).cloned();
                ui.group(|ui| {
                    ui.set_max_width(list_w - 8.0);
                    ui.horizontal(|ui| {
                        let kill = ui
                            .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let kill = ui.small_button("Remove").clicked();
                                ui.add_space(6.0);
                                let max_w = (ui.available_width() - 4.0).max(32.0);
                                let title = elide_width(ui, &sanitize_ui_text(ui, &title), max_w);
                                ui.strong(title);
                                kill
                            })
                            .inner;
                        if kill {
                            remove = Some(i);
                        }
                    });
                    if let Some(slot) = self.params.loras.get_mut(i) {
                        let (lo, hi) = meta
                            .as_ref()
                            .map(|e| {
                                let lo = e.strength_model_min.unwrap_or(-2.0);
                                let hi = e.strength_model_max.unwrap_or(2.0);
                                (lo.min(hi), lo.max(hi).max(lo + 0.01))
                            })
                            .unwrap_or((-2.0, 2.0));
                        ui.add(egui::Slider::new(&mut slot.strength_model, lo..=hi).text("Model"));
                        if !slot.model_only {
                            ui.add(egui::Slider::new(&mut slot.strength_clip, lo..=hi).text("CLIP"));
                        }
                        ui.checkbox(&mut slot.model_only, "Model only (no CLIP)");
                    }
                    egui::CollapsingHeader::new("Details")
                        .id_salt(("lora_active", i, lora.file.as_str()))
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.set_max_width(list_w - 24.0);
                            lora_meta_body(ui, &lora.file, meta.as_ref());
                        });
                });
            }
            if let Some(i) = remove {
                self.remove_lora_at(i);
            }
            ui.separator();
        }

        ui.label("Add");
        ui.add(
            egui::TextEdit::singleline(&mut self.lora_filter)
                .hint_text("filter LoRAs")
                .desired_width(list_w - 8.0),
        );

        let filter = self.lora_filter.to_lowercase();
        let ckpt = self.params.model_file().to_string();
        let active: HashSet<String> = self.params.loras.iter().map(|l| l.file.clone()).collect();
        let rows: Vec<(String, String, Option<crate::types::LoraEntry>)> = self
            .compatible_loras(&ckpt)
            .into_iter()
            .filter(|(file, _)| !active.contains(file))
            .map(|(file, entry)| {
                let label = entry
                    .map(|e| e.display_name().to_string())
                    .unwrap_or_else(|| file.clone());
                (file, label, entry.cloned())
            })
            .filter(|(file, label, _)| {
                filter.is_empty() || format!("{label} {file}").to_lowercase().contains(&filter)
            })
            .collect();
        let mut shown = 0usize;
        let mut hidden = 0usize;
        let mut add: Option<String> = None;
        for (file, label, meta) in &rows {
            if shown >= 80 {
                hidden += 1;
                continue;
            }
            ui.horizontal(|ui| {
                ui.set_max_width(list_w);
                let clicked = ui
                    .with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        let clicked = ui.small_button("Add").clicked();
                        ui.add_space(6.0);
                        // Collapse arrow (~18px) + gap; keep the label clear of Add.
                        let max_w = (ui.available_width() - 22.0).max(32.0);
                        let header = elide_width(ui, &sanitize_ui_text(ui, label), max_w);
                        egui::CollapsingHeader::new(header)
                            .id_salt(("lora_add", file.as_str()))
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.set_max_width((list_w - 56.0).max(100.0));
                                lora_meta_body(ui, file, meta.as_ref());
                            });
                        clicked
                    })
                    .inner;
                if clicked {
                    add = Some(file.clone());
                }
            });
            shown += 1;
        }
        if let Some(file) = add {
            self.add_lora(&file);
        }
        if shown == 0 {
            ui.weak(if self.installed_loras.is_empty() {
                "No LoRAs installed on the server (or object_info has none)."
            } else {
                "No matching LoRAs for this model / filter."
            });
        } else if hidden > 0 {
            ui.weak(format!("… {hidden} more — type to filter"));
        }
    }

    /// Undo/redo, floating at the TOP right — far from the queue FAB and the lock at the bottom,
    /// which are the taps you least want to hit by accident while reaching for undo.
    fn undo_redo_buttons(&mut self, ui: &mut egui::Ui, host: &Host) {
        let Some(doc) = self.active_doc() else { return };
        let view = doc.view.view_rect;
        if !view.is_finite() || view.width() < 160.0 {
            return;
        }
        let (can_undo, can_redo) = (doc.history.can_undo(), doc.history.can_redo());
        if !can_undo && !can_redo {
            return;
        }

        let mut action = None;
        egui::Area::new(egui::Id::new("comfy-undo"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(view.right() - 10.0 - 104.0, view.top() + 10.0))
            .show(ui.ctx(), |aui| {
                aui.horizontal(|aui| {
                    for (icon, tip, enabled, act) in [
                        (icons::UNDO, "Undo", can_undo, true),
                        (icons::REDO, "Redo", can_redo, false),
                    ] {
                        let btn = egui::Button::new(egui::RichText::new(icon).size(20.0))
                            .min_size(egui::vec2(48.0, 48.0))
                            .corner_radius(24.0)
                            .fill(egui::Color32::from_rgb(45, 55, 85));
                        if aui.add_enabled(enabled, btn).on_hover_text(tip).clicked() {
                            action = Some(act);
                        }
                    }
                });
            });

        match action {
            Some(true) => self.undo_graph(host),
            Some(false) => self.redo_graph(host),
            None => {}
        }
    }

    fn undo_graph(&mut self, host: &Host) {
        let Some(doc) = self.active_doc_mut() else { return };
        if !doc.history.undo(&mut doc.graph.snarl) {
            return;
        }
        self.after_history_jump();
        self.graph_status = "Undo".into();
        host.haptic(Haptic::Medium);
    }

    fn redo_graph(&mut self, host: &Host) {
        let Some(doc) = self.active_doc_mut() else { return };
        if !doc.history.redo(&mut doc.graph.snarl) {
            return;
        }
        self.after_history_jump();
        self.graph_status = "Redo".into();
        host.haptic(Haptic::Medium);
    }

    /// The snarl was replaced wholesale, so everything keyed by node id is now suspect. Snarl's
    /// slab reuses freed keys, so a stale id does not merely dangle — it can resolve to a
    /// DIFFERENT node, which would silently paint one node's output onto another.
    fn after_history_jump(&mut self) {
        // A queued run's id mapping belongs to the pre-undo graph; drop it rather than let live
        // progress and finished images land on whatever now occupies those slots.
        self.executing = None;
        self.pending_uploads.clear();
        let Some(doc) = self.active_doc_mut() else { return };
        doc.epoch += 1;
        doc.node_map.clear();
        doc.outputs.clear();
        if doc.props_node.is_some_and(|n| doc.graph.snarl.get_node(n).is_none()) {
            doc.props_node = None;
        }
        doc.graph.set_live_execution(None, None, None);
        // Node sizes are cached by id for the minimap, and a restored graph can reuse ids.
        doc.view.reset();
    }

    /// What the enabled enhance steps change about the Create settings for this run. Nothing is
    /// written back into `params`, so this is purely a description of the layer.
    fn param_override_note(&mut self, ui: &mut egui::Ui) {
        let (_, notes) = crate::apps::effective_params(
            &self.params,
            &self.params.apps,
            &self.apps,
            self.schemas.as_deref(),
        );
        if notes.is_empty() {
            return;
        }
        // Size is the case worth spelling out: the number above becomes the FINAL size.
        let (w, h) = (
            notes.iter().find(|n| n.param == "width"),
            notes.iter().find(|n| n.param == "height"),
        );
        if let (Some(w), Some(h)) = (w, h) {
            ui.weak(format!(
                "{} renders at {} × {}, final {} × {}",
                w.app, w.to as u32, h.to as u32, w.from as u32, h.from as u32
            ));
        }
        for n in notes.iter().filter(|n| n.param != "width" && n.param != "height") {
            ui.weak(format!("{}: {} → {} ({})", n.param, n.from, n.to, n.app));
        }
    }

    /// Ordered enhance chain: enable, reorder, retune, remove.
    fn create_enhance_pane(&mut self, ui: &mut egui::Ui) {
        let list_w = (ui.clip_rect().width() - 12.0).clamp(160.0, ui.available_width());
        ui.set_max_width(list_w);

        ui.horizontal(|ui| {
            ui.label(format!("Steps ({})", self.params.apps.len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("+ Add").clicked() {
                    self.app_picker = Some(AppPickTarget::Enhance);
                    self.app_filter.clear();
                }
            });
        });

        if self.params.apps.is_empty() {
            ui.weak(
                "Nothing added. Steps run after the image is generated — upscale it, fix faces, \
                 sharpen. Tap Add.",
            );
            ui.add_space(72.0);
            return;
        }
        if self.schemas.is_none() {
            ui.weak("Not connected — availability is unchecked until the catalog loads.");
        }

        // Deferred so the list is not mutated while it is being drawn.
        let mut remove: Option<usize> = None;
        let mut swap: Option<(usize, usize)> = None;
        let n = self.params.apps.len();

        for i in 0..n {
            let step = self.params.apps[i].clone();
            let def = self.apps.get(&step.app).cloned();
            let status = def
                .as_ref()
                .map(|d| crate::apps::status(d, Some(&step), self.schemas.as_deref()));
            let title = def.as_ref().map(|d| d.name.clone()).unwrap_or_else(|| step.app.clone());

            ui.group(|ui| {
                ui.set_max_width(list_w - 8.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(icons::TRASH).clicked() {
                            remove = Some(i);
                        }
                        ui.add_space(4.0);
                        if ui.add_enabled(i + 1 < n, egui::Button::new("Dn").small()).clicked() {
                            swap = Some((i, i + 1));
                        }
                        if ui.add_enabled(i > 0, egui::Button::new("Up").small()).clicked() {
                            swap = Some((i, i - 1));
                        }
                        ui.add_space(4.0);
                        if let Some(slot) = self.params.apps.get_mut(i) {
                            ui.checkbox(&mut slot.enabled, "");
                        }
                        let max_w = (ui.available_width() - 4.0).max(32.0);
                        let text = elide_width(ui, &sanitize_ui_text(ui, &title), max_w);
                        match &status {
                            // An unavailable step still renders, so its settings survive a
                            // preset that moves between servers.
                            Some(s) if !s.runnable() => {
                                ui.weak(text);
                            }
                            _ => {
                                ui.strong(text);
                            }
                        }
                    });
                });

                match (&def, &status) {
                    (None, _) => {
                        ui.weak(format!("'{}' is not installed — will be skipped.", step.app));
                    }
                    (Some(def), Some(status)) => {
                        self.enhance_card_body(ui, i, def, status, list_w);
                    }
                    _ => {}
                }
            });
        }

        if let Some((a, b)) = swap {
            self.params.apps.swap(a, b);
        }
        if let Some(i) = remove {
            self.params.apps.remove(i);
        }

        ui.add_space(4.0);
        ui.weak("Steps run top to bottom on the finished image.");
        ui.add_space(72.0);
    }

    /// One card's status note and knob widgets.
    fn enhance_card_body(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        def: &AppDef,
        status: &Status,
        list_w: f32,
    ) {
        // The app was re-published since this step was set up. Most drift self-corrects (a new
        // knob takes the def's default, a narrowed range is clamped on display), but a knob that
        // KEPT its id and type while changing meaning cannot be detected any other way. Version 0
        // predates this field being stored, so it is treated as unknown rather than stale.
        let stale = self.params.apps[i].version != 0 && self.params.apps[i].version != def.version;
        if stale {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().warn_fg_color, "Updated since you set this up");
                if ui.small_button("Reset").clicked() {
                    let enabled = self.params.apps[i].enabled;
                    self.params.apps[i] = crate::types::AppStep { enabled, ..AppStep::new(def) };
                }
            });
        }

        match status {
            Status::Ready => {}
            Status::Missing(reqs) => {
                let packs: Vec<&str> = reqs.iter().map(|r| r.pack.as_str()).collect();
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("Needs {} — will be skipped.", packs.join(", ")),
                );
            }
            Status::Broken(b) => {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("{} is installed but its schema failed to parse: {}", b[0].0, b[0].1),
                );
            }
            // Everything is installed, but some input has no value this server would accept.
            // Queuing it would have ComfyUI reject the whole prompt, base image included.
            Status::Unsatisfiable(why) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Can't run here — will be skipped: {}", why.join(", ")),
                );
            }
            Status::Mismatch(labels) => {
                ui.weak(format!("This build has no: {}", labels.join(", ")));
            }
            Status::Degraded(notes) => {
                ui.colored_label(ui.visuals().warn_fg_color, notes.join(" · "));
            }
            Status::NoCatalog => {}
        }
        if !status.runnable() {
            return;
        }

        // A knob whose target input vanished cannot be sent, so it is not offered.
        let hidden: Vec<&str> = match status {
            Status::Mismatch(labels) => def
                .knobs
                .iter()
                .filter(|k| labels.contains(&k.label))
                .map(|k| k.id.as_str())
                .collect(),
            _ => Vec::new(),
        };

        for knob in def.knobs.iter().filter(|k| !k.advanced && !hidden.contains(&k.id.as_str())) {
            self.knob_widget(ui, i, def, knob);
        }
        if def.knobs.iter().any(|k| k.advanced && !hidden.contains(&k.id.as_str())) {
            egui::CollapsingHeader::new("More")
                .id_salt(("enhance_more", i, def.id.as_str()))
                .default_open(false)
                .show(ui, |ui| {
                    ui.set_max_width(list_w - 24.0);
                    for knob in
                        def.knobs.iter().filter(|k| k.advanced && !hidden.contains(&k.id.as_str()))
                    {
                        self.knob_widget(ui, i, def, knob);
                    }
                });
        }
    }

    /// Render one knob over the pane's existing widget helpers, writing back into the step.
    fn knob_widget(&mut self, ui: &mut egui::Ui, i: usize, def: &AppDef, knob: &crate::apps::Knob) {
        let salt = format!("knob_{i}_{}_{}", def.id, knob.id);
        let stored = self.params.apps[i]
            .value(def, &knob.id)
            .unwrap_or_else(|| knob.default.clone());
        // Sliders clamp and type-mismatches fall back without marking the response changed, so a
        // pasted or hand-edited value could display corrected while the stored one is still sent.
        // Normalise up front and write back, so what the card shows is what the build uses.
        let current = coerce_knob(&stored, &knob.ty);
        let mut next: Option<serde_json::Value> =
            (current != stored).then(|| current.clone());

        let resp = match &knob.ty {
            KnobTy::Enum { class, input, prefix } => {
                let options = match self.schemas.as_deref() {
                    Some(set) => crate::apps::enum_options(set, class, input, prefix.as_deref()),
                    None => Vec::new(),
                };
                ui.label(&knob.label);
                let mut v = current.as_str().unwrap_or_default().to_string();
                if options.is_empty() {
                    // No catalog (or nothing installed): keep the stored name editable rather
                    // than silently replacing it with a blank combo.
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut v).desired_width(ui.available_width()),
                    );
                    if r.changed() {
                        next = Some(serde_json::Value::from(v.clone()));
                    }
                    r
                } else {
                    let before = v.clone();
                    combo_full(ui, &salt, &mut v, &options);
                    if v != before {
                        next = Some(serde_json::Value::from(v.clone()));
                    }
                    ui.response()
                }
            }
            KnobTy::Choice { options } => {
                ui.label(&knob.label);
                let mut v = current.as_str().unwrap_or_default().to_string();
                let before = v.clone();
                combo_full(ui, &salt, &mut v, options);
                if v != before {
                    next = Some(serde_json::Value::from(v));
                }
                ui.response()
            }
            KnobTy::Int { min, max, step } => {
                let mut v = current.as_i64().unwrap_or(*min);
                let r = full_width_slider_resp(ui, &knob.label, |ui, w| {
                    ui.spacing_mut().slider_width = w - 56.0;
                    ui.add(egui::Slider::new(&mut v, *min..=*max).step_by(*step as f64))
                });
                if r.changed() {
                    next = Some(serde_json::Value::from(v));
                }
                r
            }
            KnobTy::Float { min, max, step } => {
                let mut v = current.as_f64().unwrap_or(*min);
                let r = full_width_slider_resp(ui, &knob.label, |ui, w| {
                    ui.spacing_mut().slider_width = w - 56.0;
                    let s = egui::Slider::new(&mut v, *min..=*max);
                    ui.add(if *step > 0.0 { s.step_by(*step) } else { s })
                });
                if r.changed() {
                    next = Some(serde_json::Value::from(v));
                }
                r
            }
            KnobTy::Bool => {
                let mut v = current.as_bool().unwrap_or(false);
                let r = ui.checkbox(&mut v, &knob.label);
                if r.changed() {
                    next = Some(serde_json::Value::from(v));
                }
                r
            }
            KnobTy::Text { multiline } => {
                ui.label(&knob.label);
                let mut v = current.as_str().unwrap_or_default().to_string();
                let w = ui.available_width();
                let r = if *multiline {
                    ui.add(egui::TextEdit::multiline(&mut v).desired_width(w).desired_rows(2))
                } else {
                    ui.add(egui::TextEdit::singleline(&mut v).desired_width(w))
                };
                if r.changed() {
                    next = Some(serde_json::Value::from(v));
                }
                r
            }
        };
        if !knob.tooltip.is_empty() {
            resp.on_hover_text(&knob.tooltip);
        }
        if let Some(v) = next {
            self.params.apps[i].values.insert(knob.id.clone(), v);
        }
    }

    /// Add-step sheet: grouped, filterable, with availability shown before the tap.
    fn app_picker_window(&mut self, ctx: &egui::Context, host: &Host) {
        let Some(target) = self.app_picker else { return };
        let title = match target {
            AppPickTarget::Enhance => "Add enhance step",
            AppPickTarget::Canvas { .. } => "Insert app",
        };
        let mut open = true;
        let mut pick: Option<String> = None;
        centered(egui::Window::new(title).open(&mut open)).show(ctx, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.app_filter)
                    .hint_text("filter")
                    .desired_width(ui.available_width()),
            );
            let filter = self.app_filter.to_lowercase();
            crate::theme::scroll_vertical().max_height(360.0).show(ui, |ui| {
                let mut groups = self.apps.grouped();
                groups.sort_by_key(|(g, _)| crate::apps::group_rank(g));
                for (group, defs) in groups {
                    let rows: Vec<&AppDef> = defs
                        .into_iter()
                        .filter(|d| {
                            filter.is_empty()
                                || format!("{} {}", d.name, d.description)
                                    .to_lowercase()
                                    .contains(&filter)
                        })
                        .collect();
                    if rows.is_empty() {
                        continue;
                    }
                    egui::CollapsingHeader::new(&group)
                        .id_salt(("app_group", group.as_str()))
                        .default_open(true)
                        .show(ui, |ui| {
                            for def in rows {
                                let st =
                                    crate::apps::status(def, None, self.schemas.as_deref());
                                ui.horizontal(|ui| {
                                    let add = ui
                                        .add_enabled(
                                            st.runnable(),
                                            egui::Button::new("Add")
                                                .min_size(egui::vec2(56.0, 34.0)),
                                        )
                                        .clicked();
                                    ui.vertical(|ui| {
                                        ui.strong(&def.name);
                                        if !def.description.is_empty() {
                                            ui.weak(&def.description);
                                        }
                                        let chip = st.chip();
                                        if !chip.is_empty() {
                                            ui.colored_label(ui.visuals().warn_fg_color, chip);
                                        }
                                    });
                                    if add {
                                        pick = Some(def.id.clone());
                                    }
                                });
                                ui.separator();
                            }
                        });
                }
                if !self.apps.bad.is_empty() {
                    ui.weak(format!("{} app file(s) failed to load", self.apps.bad.len()));
                }
            });
        });
        if let Some(id) = pick {
            match target {
                AppPickTarget::Enhance => {
                    self.add_app_step(&id);
                    host.haptic(Haptic::Light);
                }
                AppPickTarget::Canvas { doc, at } => self.insert_app_into_graph(&id, doc, at, host),
            }
            self.app_picker = None;
        }
        if !open {
            self.app_picker = None;
        }
    }

    /// Materialize an app's fragment as loose nodes on the active graph tab, wired to each other.
    /// Boundary inputs (`$image`, `$model`) are left open for the user to connect.
    fn insert_app_into_graph(&mut self, id: &str, doc_id: u64, at: egui::Pos2, host: &Host) {
        let Some(def) = self.apps.get(id).cloned() else { return };
        // The picker outlives a tab switch, and `at` is a position in the tab it was opened on.
        if self.active_doc().is_none_or(|d| d.id != doc_id) {
            self.graph_status = "That tab is no longer open — reopen Insert app".into();
            host.haptic(Haptic::Warning);
            return;
        }
        // Direct snarl mutation bypasses the FlowViewer lock gate, so check it here.
        if self.active_doc().is_some_and(|d| d.view.locked) {
            self.graph_status = "Graph is locked — unlock to insert".into();
            host.haptic(Haptic::Warning);
            return;
        }
        let plan = def.plan(None);
        let Some(doc) = self.active_doc_mut() else { return };

        let mut made: HashMap<String, NodeId> = HashMap::new();
        let mut inserted: Vec<NodeId> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        let mut open: Vec<String> = Vec::new();
        // Inputs the app specified that this build would not take.
        let mut unset: Vec<String> = Vec::new();

        for (i, p) in plan.iter().enumerate() {
            let Some(object) = doc.graph.object_info.get(&p.class).cloned() else {
                // An unmet optional node is expected; a required one is worth reporting.
                if p.optional.is_none() {
                    missing.push(p.class.clone());
                }
                continue;
            };
            let pos = at + egui::vec2(0.0, i as f32 * 140.0);
            let node = doc.graph.snarl.insert_node(pos, FlowNodeData::new(object));
            inserted.push(node);
            made.insert(p.local.clone(), node);

            if let Some(data) = doc.graph.snarl.get_node_mut(node) {
                for (name, v) in &p.literals {
                    // This build renamed or dropped the input, or the value is not one this
                    // widget offers — either way the node keeps its own default, which is not
                    // what the app asked for. Say so rather than report a clean insert.
                    let took = data
                        .inputs
                        .iter_mut()
                        .find(|i| i.name == *name)
                        .is_some_and(|input| set_flow_value(&mut input.value, v));
                    if !took {
                        unset.push(format!("{}.{name}", p.class));
                    }
                }
            }
            for (_, r) in &p.open {
                let label = r.label();
                if !open.contains(&label) {
                    open.push(label);
                }
            }
        }

        // Wire by input NAME: FlowNodeData::new re-sorts inputs, so declaration order is not
        // slot order. Disconnect first — an input holds a single wire.
        for p in &plan {
            let Some(&to_node) = made.get(&p.local) else { continue };
            for (name, from_local, slot) in &p.links {
                let Some(&from_node) = made.get(from_local) else { continue };
                let Some(idx) = doc
                    .graph
                    .snarl
                    .get_node(to_node)
                    .and_then(|d| d.inputs.iter().position(|i| i.name == *name))
                else {
                    // This build has no input by that name — the wire cannot be made.
                    unset.push(format!("{}.{name}", p.class));
                    continue;
                };
                // snarl's connect() checks only that the nodes exist, so an out-of-range slot
                // would make a pin that renders and serializes as a broken wire.
                let outs = doc.graph.snarl.get_node(from_node).map_or(0, |d| d.outputs.len());
                if *slot as usize >= outs {
                    unset.push(format!("{}.{name}", p.class));
                    continue;
                }
                let to = InPinId { node: to_node, input: idx };
                let from = OutPinId { node: from_node, output: *slot as usize };
                for remote in doc.graph.snarl.in_pin(to).remotes.clone() {
                    doc.graph.snarl.disconnect(remote, to);
                }
                doc.graph.snarl.connect(from, to);
            }
        }

        doc.view.request_arrange();
        let n_inserted = inserted.len();
        // Reverting the insert is the general undo's job now — it snapshots this edit like any
        // other, which is both correct across later hand-edits and one less thing to keep in sync.

        self.graph_status = if !missing.is_empty() {
            format!("Inserted {n_inserted} node(s) — missing: {}", missing.join(", "))
        } else if !unset.is_empty() {
            unset.dedup();
            format!("Inserted {n_inserted} node(s) — this build ignored: {}", unset.join(", "))
        } else if !open.is_empty() {
            format!("Inserted {n_inserted} node(s) — connect: {}", open.join(", "))
        } else {
            format!("Inserted {n_inserted} node(s)")
        };
        host.haptic(Haptic::Success);
    }

    /// Derive an [`AppDef`] from the active tab: wires become `$node:` refs, dangling typed
    /// inputs become boundary refs, widgets become literals the publish dialog can promote.
    fn derive_app_draft(&self) -> Option<PublishDraft> {
        let doc = self.active_doc()?;
        if doc.is_empty() {
            return None;
        }
        let snarl = &doc.graph.snarl;

        // (to node, to input index) -> (from node, from output slot).
        let mut incoming: HashMap<(NodeId, usize), (NodeId, u32)> = HashMap::new();
        let mut consumed: HashSet<(NodeId, usize)> = HashSet::new();
        for (from, to) in snarl.wires() {
            incoming.insert((to.node, to.input), (from.node, from.output as u32));
            consumed.insert((from.node, from.output));
        }

        // Emit in dependency order so every `$node:` ref points backwards.
        let mut order: Vec<NodeId> = snarl.nodes_pos_ids().map(|(id, _, _)| id).collect();
        order.sort_by_key(|id| id.0);
        let order = toposort_nodes(&order, &incoming);
        let local: HashMap<NodeId, String> =
            order.iter().enumerate().map(|(i, id)| (*id, format!("n{i}"))).collect();

        let mut nodes = Vec::new();
        let mut unbound: Vec<String> = Vec::new();
        let mut widgets = Vec::new();
        let mut requires: Vec<crate::apps::Require> = Vec::new();

        for id in &order {
            let Some(data) = snarl.get_node(*id) else { continue };
            let class = data.object.name.clone();
            if !requires.iter().any(|r| r.class == class) {
                requires.push(crate::apps::Require {
                    class: class.clone(),
                    pack: pack_guess(&data.object.category, &class),
                    optional: false,
                });
            }
            let mut inputs = std::collections::BTreeMap::new();
            // Per node: a node's first dangling CONDITIONING is its positive, the second its
            // negative. Counting across the whole fragment would mark every later node negative.
            let mut cond_seen = 0;
            for (i, input) in data.inputs.iter().enumerate() {
                if let Some((from, slot)) = incoming.get(&(*id, i)) {
                    if let Some(src) = local.get(from) {
                        inputs.insert(
                            input.name.clone(),
                            serde_json::Value::from(format!("$node:{src}:{slot}")),
                        );
                    }
                    continue;
                }
                if input.value.is_connection_only() {
                    // Unwired socket: bind it to the matching handle the Create graph publishes.
                    let ty = graphview::type_str(&input.typ);
                    // Name wins over position: an explicit "negative" socket must not become
                    // $positive just because it is declared first.
                    let bound = match ty.as_str() {
                        "IMAGE" => Some("$image"),
                        "LATENT" => Some("$latent"),
                        "MODEL" => Some("$model"),
                        "CLIP" => Some("$clip"),
                        "VAE" => Some("$vae"),
                        "CONDITIONING" => Some(match input.name.as_str() {
                            "positive" => "$positive",
                            "negative" => "$negative",
                            _ => {
                                cond_seen += 1;
                                if cond_seen == 1 { "$positive" } else { "$negative" }
                            }
                        }),
                        _ => None,
                    };
                    match bound {
                        Some(b) => {
                            inputs.insert(input.name.clone(), serde_json::Value::from(b));
                        }
                        // Nothing in the Create graph can feed this socket. Only a REQUIRED one
                        // blocks the save — an optional socket left unwired is exactly how the
                        // node is meant to run, and treating it as an error made Save unreachable
                        // for any graph with one. Unknown (not connected) stays conservative.
                        None => {
                            let required = self
                                .schemas
                                .as_ref()
                                .and_then(|s| s.input(&class, &input.name))
                                .is_none_or(|i| i.required);
                            if required {
                                unbound.push(format!("{class}.{} ({ty})", input.name));
                            }
                        }
                    }
                    continue;
                }
                if let Some(v) = flow_value_json(&input.value) {
                    widgets.push(PublishWidget {
                        local: local[id].clone(),
                        class: class.clone(),
                        input: input.name.clone(),
                        label: input.name.replace('_', " "),
                        value: v.clone(),
                        promote: false,
                    });
                    // The stored form is mini-syntax, so a widget string that happens to start
                    // with '$' has to be escaped. The knob default above stays verbatim — it is
                    // never re-parsed as a reference.
                    inputs.insert(input.name.clone(), crate::apps::escape_literal(&v));
                }
            }
            nodes.push(crate::apps::NodeTpl {
                id: local[id].clone(),
                class,
                inputs,
                needs: None,
            });
        }

        // The output is the last node with an unconsumed IMAGE output.
        let output = order.iter().rev().find_map(|id| {
            let data = snarl.get_node(*id)?;
            let slot = data.outputs.iter().position(|o| {
                graphview::type_str(&o.typ) == "IMAGE"
            })?;
            (!consumed.contains(&(*id, slot))).then(|| crate::apps::LocalRef {
                node: local[id].clone(),
                slot: slot as u32,
            })
        })?;

        let name = doc.name.trim_end_matches(".json").to_string();
        let id = slug(&name);
        Some(PublishDraft {
            def: AppDef {
                id: id.clone(),
                name: name.clone(),
                description: String::new(),
                group: "Finish".into(),
                version: 1,
                requires,
                knobs: Vec::new(),
                // A derived app adjusts nothing about the Create settings; that has to be
                // authored deliberately in the JSON.
                overrides: Vec::new(),
                nodes,
                output,
            },
            id,
            name,
            group: "Finish".into(),
            description: String::new(),
            widgets,
            error: if unbound.is_empty() {
                String::new()
            } else {
                format!(
                    "Nothing can feed: {} — wire these before saving.",
                    unbound.join(", ")
                )
            },
            blocked: !unbound.is_empty(),
        })
    }

    /// Name the app, choose which widgets become knobs, and write it to the apps directory.
    fn publish_window(&mut self, ctx: &egui::Context, host: &Host) {
        if self.publish.is_none() {
            return;
        }
        let mut open = true;
        let mut save = false;
        let mut draft = self.publish.take().unwrap();
        centered(egui::Window::new("Save tab as app").open(&mut open)).show(ctx, |ui| {
            ui.label("Name");
            ui.add(
                egui::TextEdit::singleline(&mut draft.name).desired_width(ui.available_width()),
            );
            ui.label("Id");
            ui.add(egui::TextEdit::singleline(&mut draft.id).desired_width(ui.available_width()));
            ui.label("Group");
            combo_full(
                ui,
                "publish_group",
                &mut draft.group,
                &crate::apps::GROUP_ORDER.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            );
            ui.label("Description");
            ui.add(
                egui::TextEdit::multiline(&mut draft.description)
                    .desired_rows(2)
                    .desired_width(ui.available_width()),
            );

            ui.separator();
            ui.label(format!(
                "{} node(s) · output {}:{}",
                draft.def.nodes.len(),
                draft.def.output.node,
                draft.def.output.slot
            ));
            ui.weak("Tick the settings to expose as controls in the Create tab.");
            crate::theme::scroll_vertical().max_height(220.0).show(ui, |ui| {
                for w in &mut draft.widgets {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut w.promote, "");
                        ui.vertical(|ui| {
                            ui.label(format!("{}.{} [{}]", w.class, w.input, w.local));
                            if w.promote {
                                ui.add(
                                    egui::TextEdit::singleline(&mut w.label)
                                        .desired_width(ui.available_width().min(200.0)),
                                );
                            } else {
                                ui.weak(elide(&w.value.to_string(), 40));
                            }
                        });
                    });
                }
            });
            if !draft.error.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, &draft.error);
            }
            ui.separator();
            if ui
                .add_enabled(!draft.blocked, egui::Button::new(format!("{} Save app", icons::SAVE)))
                .clicked()
            {
                save = true;
            }
        });

        if save {
            match self.write_app(&mut draft, host) {
                Ok(path) => {
                    self.graph_status = format!("Saved app to {path}");
                    host.haptic(Haptic::Success);
                    return;
                }
                Err(e) => {
                    draft.error = e;
                    host.haptic(Haptic::Error);
                }
            }
        }
        if open {
            self.publish = Some(draft);
        }
    }

    /// Build the final definition from the draft, validate it, and write it to disk.
    fn write_app(&mut self, draft: &mut PublishDraft, host: &Host) -> Result<String, String> {
        let id = slug(&draft.id);
        if id.is_empty() {
            return Err("Give the app an id".into());
        }
        // A user file with a builtin's id replaces it in `AppSet::load`, silently changing what
        // every already-saved step referencing that id does.
        if AppSet::builtin().by_id.contains_key(&id) {
            return Err(format!("'{id}' is a built-in app — pick another id"));
        }
        let mut def = draft.def.clone();
        def.id = id.clone();
        // Re-publishing under an existing id is the normal authoring loop, so step the version
        // past what is installed. That is the only signal a saved chain has that the app it
        // points at changed underneath it.
        def.version = self.apps.get(&id).map_or(1, |old| old.version.saturating_add(1));
        def.name = draft.name.trim().to_string();
        def.group = draft.group.clone();
        def.description = draft.description.trim().to_string();
        if def.name.is_empty() {
            return Err("Give the app a name".into());
        }

        // Promote each ticked widget: the literal becomes a $knob: ref plus a Knob carrying the
        // editor's own bounds and options. Keyed by node, so two nodes of one class stay distinct.
        let doc = self.active_doc().ok_or("No graph tab")?;
        def.knobs.clear();
        for w in draft.widgets.iter().filter(|w| w.promote) {
            let knob_id = slug(&format!("{}_{}", w.local, w.input)).replace('.', "_");
            let ty = doc
                .graph
                .snarl
                .nodes_pos_ids()
                .find(|(_, _, d)| d.object.name == w.class)
                .and_then(|(_, _, d)| d.inputs.iter().find(|i| i.name == w.input))
                .map(|i| knob_ty_for(&w.class, &w.input, &i.value))
                .ok_or_else(|| format!("{}.{} is no longer on the graph", w.class, w.input))?;
            def.knobs.push(crate::apps::Knob {
                id: knob_id.clone(),
                label: if w.label.trim().is_empty() {
                    w.input.replace('_', " ")
                } else {
                    w.label.trim().to_string()
                },
                ty,
                default: w.value.clone(),
                advanced: false,
                tooltip: String::new(),
            });
            if let Some(node) = def.nodes.iter_mut().find(|n| n.id == w.local) {
                node.inputs
                    .insert(w.input.clone(), serde_json::Value::from(format!("$knob:{knob_id}")));
            }
        }

        let body = serde_json::to_string_pretty(&def).map_err(|e| e.to_string())?;
        // Round-trip through the loader so a bad app never reaches disk.
        let mut probe = AppSet::default();
        probe.insert_json("draft", &body);
        if let Some((_, why)) = probe.bad.first() {
            return Err(why.clone());
        }

        let dir = host.documents_dir().ok_or("No storage directory")?;
        let folder = format!("{dir}/comfyui/apps");
        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let path = format!("{folder}/{id}.json");
        std::fs::write(&path, &body).map_err(|e| e.to_string())?;

        let apps = AppSet::load(Some(dir.as_str()));
        self.log.info(format!("saved app '{id}' ({} total)", apps.by_id.len()));
        self.apps = Arc::new(apps);
        self.publish = None;
        Ok(path)
    }

    /// Insert a step at its group's place in the pipeline so the common order needs no taps.
    fn add_app_step(&mut self, id: &str) {
        let Some(def) = self.apps.get(id) else { return };
        let rank = crate::apps::group_rank(&def.group);
        let at = self
            .params
            .apps
            .iter()
            .position(|s| {
                self.apps
                    .get(&s.app)
                    .is_some_and(|d| crate::apps::group_rank(&d.group) > rank)
            })
            .unwrap_or(self.params.apps.len());
        self.params.apps.insert(at, AppStep::new(def));
        self.create_pane = CreatePane::Enhance;
    }

    /// Installed LoRAs compatible with `checkpoint`, with optional catalog metadata.
    fn compatible_loras(&self, checkpoint: &str) -> Vec<(String, Option<&crate::types::LoraEntry>)> {
        let has_catalog = !self.lora_catalog.loras.is_empty();
        let model_bases = self.model_bases_for(checkpoint);
        let mut out = Vec::new();
        for file in &self.installed_loras {
            let entry = self.lora_catalog.entry(file);
            if has_catalog {
                match entry {
                    Some(e) if e.matches_checkpoint(checkpoint, &model_bases) => {
                        out.push((file.clone(), Some(e)));
                    }
                    Some(_) => {}
                    // Installed but uncatalogued: hide when a catalog exists (bases unknown).
                    None => {}
                }
            } else {
                out.push((file.clone(), None));
            }
        }
        out.sort_by(|a, b| {
            let an = a.1.map(|e| e.display_name()).unwrap_or(a.0.as_str());
            let bn = b.1.map(|e| e.display_name()).unwrap_or(b.0.as_str());
            an.to_lowercase().cmp(&bn.to_lowercase())
        });
        out
    }

    fn add_lora(&mut self, file: &str) {
        if self.params.loras.iter().any(|l| l.file == file) {
            return;
        }
        let (sm, sc, triggers, negatives) = match self.lora_catalog.entry(file) {
            Some(e) => {
                let (sm, sc) = e.add_strengths();
                (sm, sc, e.trigger_text(), e.negative_text())
            }
            None => (1.0, 1.0, String::new(), String::new()),
        };
        let injected = merge_triggers(
            &mut self.params.lora_triggers,
            &triggers,
            &self.params.positive,
        );
        append_negatives(&mut self.params.negative, &negatives);
        self.params.loras.push(ActiveLora {
            file: file.to_string(),
            strength_model: sm,
            strength_clip: sc,
            injected,
            model_only: false,
        });
        self.selected_preset.clear();
    }

    fn remove_lora_at(&mut self, index: usize) {
        if index >= self.params.loras.len() {
            return;
        }
        let removed = self.params.loras.remove(index);
        strip_injected(&mut self.params.lora_triggers, &removed.injected);
        self.selected_preset.clear();
    }

    /// Fill Create params from gallery/`workflow_clip` or system clipboard JSON.
    fn paste_workflow_into_create(&mut self, host: &Host) {
        let body = self.workflow_clip.clone().or_else(|| {
            host.clipboard_text().filter(|t| {
                let t = t.trim();
                t.starts_with('{') || t.starts_with('[')
            })
        });
        let Some(body) = body else {
            self.status = "Nothing to paste".into();
            return;
        };
        let meta = gallery::parse_workflow_meta(&body);
        if meta.is_empty() {
            self.status = "Could not read workflow".into();
            host.haptic(Haptic::Warning);
            return;
        }
        self.apply_image_meta(&meta);
        self.status = "Create filled from workflow".into();
        host.haptic(Haptic::Medium);
    }

    fn paste_sampler_pack(&mut self, host: &Host) {
        let pack = self.sampler_clip.clone().or_else(|| {
            host.clipboard_text()
                .as_deref()
                .and_then(SamplerPack::from_clipboard_json)
        });
        let Some(pack) = pack else {
            self.status = "No sampler pack to paste".into();
            return;
        };
        self.apply_sampler_pack(&pack);
        self.status = "Sampler settings pasted".into();
        host.haptic(Haptic::Medium);
    }

    fn paste_lora_pack(&mut self, host: &Host) {
        let pack = self.lora_clip.clone().or_else(|| {
            host.clipboard_text()
                .as_deref()
                .and_then(LoraPack::from_clipboard_json)
        });
        let Some(pack) = pack else {
            self.status = "No LoRAs to paste".into();
            return;
        };
        self.apply_lora_pack(&pack);
        self.status = format!("{} LoRA(s) pasted", pack.loras.len());
        host.haptic(Haptic::Medium);
    }

    fn apply_sampler_pack(&mut self, pack: &SamplerPack) {
        if let Some(s) = pack.sampler.as_ref().and_then(|s| match_sampler_name(s, &self.samplers))
        {
            self.params.sampler = s;
        }
        if let Some(s) =
            pack.scheduler.as_ref().and_then(|s| match_sampler_name(s, &self.schedulers))
        {
            self.params.scheduler = s;
        }
        if let Some(v) = pack.steps {
            self.params.steps = v;
        }
        if let Some(v) = pack.cfg {
            self.params.cfg = v;
        }
        self.selected_preset.clear();
    }

    fn apply_lora_pack(&mut self, pack: &LoraPack) {
        self.params.loras = pack.loras.clone();
        self.selected_preset.clear();
    }

    fn copy_sampler_pack_from_meta(&mut self, meta: &ImageMeta, host: &Host) {
        let pack = SamplerPack {
            sampler: meta.sampler.clone(),
            scheduler: meta.scheduler.clone(),
            steps: meta.steps.map(|n| n as u32),
            cfg: meta.cfg.map(|n| n as f32),
        };
        if pack.is_empty() {
            return;
        }
        host.copy_text(pack.to_clipboard_json());
        self.sampler_clip = Some(pack);
        self.gallery_status = "Sampler settings copied".into();
        host.haptic(Haptic::Light);
    }

    fn copy_lora_pack_from_meta(&mut self, meta: &ImageMeta, host: &Host) {
        if meta.loras.is_empty() {
            return;
        }
        let pack = LoraPack {
            loras: meta
                .loras
                .iter()
                .map(|l| ActiveLora {
                    file: l.name.clone(),
                    strength_model: l.strength_model as f32,
                    strength_clip: l.strength_clip.unwrap_or(l.strength_model) as f32,
                    injected: String::new(),
                    model_only: l.model_only,
                })
                .collect(),
        };
        host.copy_text(pack.to_clipboard_json());
        self.lora_clip = Some(pack);
        self.gallery_status = "LoRAs copied".into();
        host.haptic(Haptic::Light);
    }

    fn apply_image_meta(&mut self, meta: &ImageMeta) {
        // A UNET in the graph means the diffusion topology; the image's own encoders and VAE beat
        // whatever select_model would have seeded.
        if let Some(unet) = &meta.unet {
            self.select_model(unet, Some(ModelKind::Diffusion));
            if !meta.clips.is_empty() {
                self.params.clip_names = meta.clips.clone();
            }
            if let Some(t) = &meta.clip_type {
                self.params.clip_type = t.clone();
            }
            if let Some(v) = &meta.vae {
                self.params.vae_name = v.clone();
            }
            if let Some(d) = &meta.weight_dtype {
                self.params.weight_dtype = d.clone();
            }
        } else if let Some(m) = meta.models.first() {
            self.select_model(m, Some(ModelKind::Checkpoint));
        }
        if let Some(p) = &meta.positive {
            self.params.positive = p.clone();
        }
        if let Some(n) = &meta.negative {
            self.params.negative = n.clone();
        }
        // Workflow positive already includes triggers — keep the dedicated field clear.
        self.params.lora_triggers.clear();
        self.apply_sampler_pack(&SamplerPack {
            sampler: meta.sampler.clone(),
            scheduler: meta.scheduler.clone(),
            steps: meta.steps.map(|n| n as u32),
            cfg: meta.cfg.map(|n| n as f32),
        });
        if let Some(v) = meta.seed.filter(|&s| s >= 0) {
            self.params.seed = v as u64;
            self.params.randomize_seed = false;
        }
        self.apply_lora_pack(&LoraPack {
            loras: meta
                .loras
                .iter()
                .map(|l| ActiveLora {
                    file: l.name.clone(),
                    strength_model: l.strength_model as f32,
                    strength_clip: l.strength_clip.unwrap_or(l.strength_model) as f32,
                    injected: String::new(),
                    model_only: l.model_only,
                })
                .collect(),
        });
        self.create_pane = CreatePane::Main;
    }

    fn apply_preset(&mut self, name: &str) {
        if let Some(p) = self.presets.iter().find(|p| p.name == name) {
            self.params = p.params.clone();
            self.selected_preset = name.to_string();
            // A preset saved against another server may name companions this one lacks.
            if self.params.model_kind == ModelKind::Diffusion {
                self.resolve_companions(Companions::Repair);
            }
        }
    }

    fn save_preset(&mut self, name: String) {
        if name.is_empty() {
            return;
        }
        if let Some(slot) = self.presets.iter_mut().find(|p| p.name == name) {
            slot.params = self.params.clone();
        } else {
            self.presets.push(CreatePreset { name: name.clone(), params: self.params.clone() });
            self.presets.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }
        self.selected_preset = name;
    }

    fn delete_selected_preset(&mut self) {
        let name = self.selected_preset.clone();
        if name.is_empty() {
            return;
        }
        self.presets.retain(|p| p.name != name);
        self.selected_preset.clear();
    }

    fn output(&mut self, ui: &mut egui::Ui, host: &Host) {
        if self.running || self.queue_remaining > 0 || !self.status.is_empty() {
            ui.add_space(6.0);
            let (v, m) = self.progress;
            if m > 0 && (self.running || self.queue_remaining > 0) {
                ui.add(egui::ProgressBar::new(v as f32 / m as f32).text(elide(&self.status, 300)));
            } else if self.running || self.queue_remaining > 0 {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(elide(&self.status, 300));
                });
            } else {
                ui.label(elide(&self.status, 300));
            }
        }

        // Pinned rather than transient: a skipped upscale must outlive the status line.
        if !self.enhance_note.is_empty() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("{} {}", icons::WARN, self.enhance_note),
                );
                if ui.small_button("Dismiss").clicked() {
                    self.enhance_note.clear();
                }
            });
        }

        if let Some(tex) = &self.preview {
            image_view(ui, tex);
        }

        if !self.results.is_empty() {
            ui.add_space(6.0);
            ui.separator();
            let n = self.results.len();
            ui.horizontal(|ui| {
                ui.label(if n == 1 {
                    "Result".into()
                } else {
                    format!("Results ({n})")
                });
                if !self.note.is_empty() {
                    ui.weak(self.note.clone());
                }
            });
            let mut open: Option<usize> = None;
            let mut save_idx: Option<usize> = None;
            const THUMB: f32 = 96.0;
            ui.horizontal_wrapped(|ui| {
                for (i, (tex, _)) in self.results.iter().enumerate() {
                    let sized = egui::load::SizedTexture::from_handle(tex);
                    let resp = ui
                        .add(
                            egui::Image::new(sized)
                                .max_size(egui::vec2(THUMB, THUMB))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(format!("Open fullscreen ({}/{})", i + 1, n));
                    if resp.clicked() {
                        open = Some(i);
                    }
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Save last").clicked() {
                    save_idx = Some(n - 1);
                }
                if n > 1 && ui.button("Save all").clicked() {
                    for i in 0..n {
                        self.save_result_at(host, i);
                    }
                }
            });
            if let Some(i) = open {
                self.result_view = Some(i);
            }
            if let Some(i) = save_idx {
                self.save_result_at(host, i);
            }
        }
    }

    /// Fullscreen Create-result viewer (Android Back / Esc returns to the thumb strip).
    fn result_viewer(&mut self, ui: &mut egui::Ui, host: &Host) {
        let Some(idx) = self.result_view else { return };
        if idx >= self.results.len() {
            self.result_view = None;
            return;
        }
        if ui.ctx().input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
        }) {
            self.result_view = None;
            return;
        }

        let n = self.results.len();
        let mut close = false;
        let mut save = false;
        let mut go: Option<isize> = None;
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(icons::BACK).min_size(egui::vec2(40.0, 36.0)))
                .on_hover_text("Back to results")
                .clicked()
            {
                close = true;
            }
            ui.label(format!("{}/{}", idx + 1, n));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Save").clicked() {
                    save = true;
                }
                if n > 1 {
                    if ui
                        .add_enabled(idx + 1 < n, egui::Button::new("▶"))
                        .on_hover_text("Next")
                        .clicked()
                    {
                        go = Some(1);
                    }
                    if ui
                        .add_enabled(idx > 0, egui::Button::new("◀"))
                        .on_hover_text("Previous")
                        .clicked()
                    {
                        go = Some(-1);
                    }
                }
            });
        });
        ui.separator();

        let image_rect = ui.available_rect_before_wrap();
        let avail = image_rect.size().max(egui::vec2(1.0, 1.0));
        let sized = egui::load::SizedTexture::from_handle(&self.results[idx].0);
        ui.scope_builder(egui::UiBuilder::new().max_rect(image_rect), |ui| {
            ui.centered_and_justified(|ui| {
                ui.add(
                    egui::Image::new(sized)
                        .max_size(avail)
                        .maintain_aspect_ratio(true),
                );
            });
        });

        if close {
            self.result_view = None;
        } else if save {
            self.save_result_at(host, idx);
        } else if let Some(d) = go {
            let next = idx as isize + d;
            if next >= 0 && (next as usize) < n {
                self.result_view = Some(next as usize);
            }
        }
    }

    fn generate_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        if self.result_view.is_some() {
            let pane = ui.available_rect_before_wrap();
            self.result_viewer(ui, host);
            // Keep Queue reachable while inspecting a batch frame.
            if matches!(self.create_pane, CreatePane::Main | CreatePane::Enhance) {
                self.queue_fab(ui.ctx(), host, pane, QueueFabKind::Create);
            }
            return;
        }

        self.create_pane_bar(ui);
        ui.separator();
        let pane = ui.available_rect_before_wrap();
        crate::theme::scroll_vertical()
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
        // Enhance keeps the FABs so tuning a slider and requeueing is not a pane round trip.
        if matches!(self.create_pane, CreatePane::Main | CreatePane::Enhance) {
            self.queue_fab(ui.ctx(), host, pane, QueueFabKind::Create);
            self.create_fab(ui.ctx(), host, pane);
        }
    }

    /// Queue FAB (+ Cancel while running) shared by Create and Graph (bottom-right).
    fn queue_fab(&mut self, ctx: &egui::Context, host: &Host, pane: egui::Rect, kind: QueueFabKind) {
        if !pane.is_finite() || pane.width() < 80.0 {
            return;
        }
        let default = egui::pos2(pane.right() - 58.0, pane.bottom() - 58.0);
        let mut pos = self.queue_fab_pos.unwrap_or(default);
        pos.x = pos.x.clamp(pane.left() + 8.0, pane.right() - 52.0);
        pos.y = pos.y.clamp(pane.top() + 8.0, pane.bottom() - 52.0);

        let can_queue = match kind {
            QueueFabKind::Create => self.can_queue_create().is_ok(),
            QueueFabKind::Graph => {
                self.has_graph_editor()
                    && (matches!(self.conn, Conn::Connected)
                        || self.engine.as_ref().is_some_and(|e| e.is_connected()))
            }
        };

        let mut queue_clicked = false;
        let mut cancel_clicked = false;
        egui::Area::new(egui::Id::new("queue-fab"))
            .order(egui::Order::Foreground)
            .current_pos(pos)
            .show(ctx, |ui| {
                let tip = if self.running {
                    format!("Queue ({} in flight)", self.jobs_left.max(1))
                } else {
                    "Queue".into()
                };
                let btn = egui::Button::new(egui::RichText::new(icons::RUN).size(22.0))
                    .min_size(egui::vec2(48.0, 48.0))
                    .corner_radius(24.0)
                    .fill(egui::Color32::from_rgb(45, 55, 85));
                let resp = ui.add_enabled(can_queue, btn).on_hover_text(tip);
                if resp.dragged() {
                    let delta = resp.drag_delta();
                    if delta != egui::Vec2::ZERO {
                        pos += delta;
                        self.queue_fab_pos = Some(pos);
                    }
                }
                if resp.clicked() {
                    queue_clicked = true;
                }
            });

        if self.running {
            let cancel_pos = egui::pos2(pos.x, (pos.y - 56.0).max(pane.top() + 8.0));
            egui::Area::new(egui::Id::new("cancel-fab"))
                .order(egui::Order::Foreground)
                .fixed_pos(cancel_pos)
                .show(ctx, |ui| {
                    let btn = egui::Button::new(egui::RichText::new(icons::STOP).size(22.0))
                        .min_size(egui::vec2(48.0, 48.0))
                        .corner_radius(24.0)
                        .fill(egui::Color32::from_rgb(120, 55, 55));
                    if ui.add(btn).on_hover_text("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                });
        }

        if cancel_clicked {
            if let Some(engine) = self.engine.as_mut() {
                engine.cancel();
                host.haptic(Haptic::Warning);
            }
            return;
        }
        if queue_clicked {
            match kind {
                QueueFabKind::Create => self.start_generation(ctx, host),
                QueueFabKind::Graph => self.queue_graph(ctx, host),
            }
        }
    }

    /// Draggable Create-tab menu bubble (paste / open graph).
    fn create_fab(&mut self, ctx: &egui::Context, host: &Host, pane: egui::Rect) {
        if !pane.is_finite() || pane.width() < 80.0 {
            return;
        }
        let queue = self
            .queue_fab_pos
            .unwrap_or(egui::pos2(pane.right() - 58.0, pane.bottom() - 58.0));
        let default = egui::pos2(queue.x - 56.0, queue.y);
        let mut pos = self.create_fab_pos.unwrap_or(default);
        pos.x = pos.x.clamp(pane.left() + 8.0, pane.right() - 52.0);
        pos.y = pos.y.clamp(pane.top() + 8.0, pane.bottom() - 52.0);

        let can_open_graph =
            self.params.missing_model_part().is_none() && self.schemas.is_some();
        let clip = host.clipboard_text();
        let has_wf = self.workflow_clip.is_some()
            || clip.as_deref().is_some_and(|t| {
                let t = t.trim();
                t.starts_with('{') || t.starts_with('[')
            });
        let has_sampler = self.sampler_clip.is_some()
            || clip.as_deref().and_then(SamplerPack::from_clipboard_json).is_some();
        let has_loras = self.lora_clip.is_some()
            || clip.as_deref().and_then(LoraPack::from_clipboard_json).is_some();
        let has_apps = clip.as_deref().and_then(AppPack::from_clipboard_json).is_some();
        let has_steps = !self.params.apps.is_empty();

        enum FabAct {
            OpenGraph,
            PasteWf,
            PasteSampler,
            PasteLoras,
            CopySteps,
            PasteSteps,
            Toggle,
            Close,
        }
        let mut act: Option<FabAct> = None;
        let open = self.create_fab_open;

        let mut menu_rect = egui::Rect::NOTHING;
        if open {
            let menu = egui::Area::new(egui::Id::new("create-fab-menu"))
                .order(egui::Order::Foreground)
                .pivot(egui::Align2::RIGHT_BOTTOM)
                .fixed_pos(egui::pos2(pos.x + 48.0, pos.y - 8.0))
                .constrain_to(pane.expand(4.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.set_min_width(160.0);
                            let mut any = false;
                            if has_wf {
                                any = true;
                                if ui
                                    .add(
                                        egui::Button::new(format!(
                                            "{} Paste workflow",
                                            icons::PROPS
                                        ))
                                        .min_size(egui::vec2(160.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    act = Some(FabAct::PasteWf);
                                }
                            }
                            if has_sampler {
                                any = true;
                                if ui
                                    .add(
                                        egui::Button::new(format!(
                                            "{} Paste sampler",
                                            icons::PROPS
                                        ))
                                        .min_size(egui::vec2(160.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    act = Some(FabAct::PasteSampler);
                                }
                            }
                            if has_loras {
                                any = true;
                                if ui
                                    .add(
                                        egui::Button::new(format!("{} Paste LoRAs", icons::PROPS))
                                            .min_size(egui::vec2(160.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    act = Some(FabAct::PasteLoras);
                                }
                            }
                            if has_steps {
                                any = true;
                                if ui
                                    .add(
                                        egui::Button::new(format!("{} Copy steps", icons::PROPS))
                                            .min_size(egui::vec2(160.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    act = Some(FabAct::CopySteps);
                                }
                            }
                            if has_apps {
                                any = true;
                                if ui
                                    .add(
                                        egui::Button::new(format!("{} Paste steps", icons::PROPS))
                                            .min_size(egui::vec2(160.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    act = Some(FabAct::PasteSteps);
                                }
                            }
                            if any {
                                ui.separator();
                            }
                            if ui
                                .add_enabled(
                                    can_open_graph,
                                    egui::Button::new(format!("{} Open Graph", icons::GRAPH))
                                        .min_size(egui::vec2(160.0, 36.0)),
                                )
                                .clicked()
                            {
                                act = Some(FabAct::OpenGraph);
                            }
                        });
                });
            menu_rect = menu.response.rect;
        }

        let fab = egui::Area::new(egui::Id::new("create-fab"))
            .order(egui::Order::Foreground)
            .current_pos(pos)
            .show(ctx, |ui| {
                let fill = if open {
                    egui::Color32::from_rgb(70, 90, 140)
                } else {
                    egui::Color32::from_rgb(45, 55, 85)
                };
                let label = if open { icons::CHECK } else { icons::MENU };
                let btn = egui::Button::new(egui::RichText::new(label).size(22.0))
                    .min_size(egui::vec2(48.0, 48.0))
                    .corner_radius(24.0)
                    .fill(fill);
                let resp = ui.add(btn).on_hover_text("Actions — drag to move");
                if resp.dragged() {
                    let delta = resp.drag_delta();
                    if delta != egui::Vec2::ZERO {
                        pos += delta;
                        self.create_fab_pos = Some(pos);
                    }
                }
                if resp.clicked() {
                    act = Some(FabAct::Toggle);
                }
                resp
            });

        if open && act.is_none() {
            let click_pos = ctx.input(|i| i.pointer.interact_pos().filter(|_| i.pointer.any_click()));
            if let Some(p) = click_pos
                && !menu_rect.contains(p)
                && !fab.response.contains_pointer()
            {
                act = Some(FabAct::Close);
            }
        }

        match act {
            Some(FabAct::Toggle) => self.create_fab_open = !self.create_fab_open,
            Some(FabAct::Close) => self.create_fab_open = false,
            Some(FabAct::OpenGraph) => {
                self.create_fab_open = false;
                self.open_create_as_graph(host);
            }
            Some(FabAct::PasteWf) => {
                self.create_fab_open = false;
                self.paste_workflow_into_create(host);
            }
            Some(FabAct::PasteSampler) => {
                self.create_fab_open = false;
                self.paste_sampler_pack(host);
            }
            Some(FabAct::PasteLoras) => {
                self.create_fab_open = false;
                self.paste_lora_pack(host);
            }
            Some(FabAct::CopySteps) => {
                self.create_fab_open = false;
                let pack = AppPack { apps: self.params.apps.clone() };
                host.copy_text(pack.to_clipboard_json());
                self.status = format!("Copied {} enhance step(s)", pack.apps.len());
                host.haptic(Haptic::Success);
            }
            Some(FabAct::PasteSteps) => {
                self.create_fab_open = false;
                self.paste_app_pack(host);
            }
            None => {}
        }
    }

    /// Replace the enhance chain from a `comfyui_android_apps_v1` clipboard payload.
    fn paste_app_pack(&mut self, host: &Host) {
        let Some(pack) = host.clipboard_text().as_deref().and_then(AppPack::from_clipboard_json)
        else {
            self.status = "No enhance steps on the clipboard".into();
            host.haptic(Haptic::Warning);
            return;
        };
        let unknown = pack.apps.iter().filter(|s| self.apps.get(&s.app).is_none()).count();
        let n = pack.apps.len();
        self.params.apps = pack.apps;
        self.status = if unknown > 0 {
            format!("Pasted {n} step(s) — {unknown} not installed here")
        } else {
            format!("Pasted {n} enhance step(s)")
        };
        self.create_pane = CreatePane::Enhance;
        host.haptic(Haptic::Success);
    }

    fn graph_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        let has_graph = self.has_graph_editor();

        // Top: open workflow tabs only.
        if has_graph {
            ui.horizontal(|ui| {
                self.graph_tabs_menu(ui);
            });
            ui.separator();
        }

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

        // Bottom: File/Edit/View | Canvas/Properties.
        egui::Panel::bottom("graph-controls").show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                self.graph_controls(ui, host);
                ui.separator();
                ui.selectable_value(&mut self.graph_pane, GraphPane::Canvas, "Canvas");
                ui.selectable_value(
                    &mut self.graph_pane,
                    GraphPane::Props,
                    format!("{} Properties", icons::PROPS),
                );
            });
            ui.add_space(2.0);
        });

        match self.graph_pane {
            GraphPane::Canvas => self.graph_canvas(ui, host),
            GraphPane::Props => self.props_tab(ui, host),
        }
    }

    /// Dropdown of open workflow tabs, plus close-current / close-all.
    fn graph_tabs_menu(&mut self, ui: &mut egui::Ui) {
        let title = self
            .active_doc()
            .map(|d| d.title())
            .unwrap_or_else(|| "Tabs".into());
        let n = self.graph_tabs.len();
        let label = if n > 1 {
            format!("{title} ({n})")
        } else {
            title
        };
        let mut switch_to: Option<usize> = None;
        let mut close_idx: Option<usize> = None;
        let mut close_all = false;
        // Header control: open below and left so the list isn't clipped by the status bar.
        down_menu(ui, label, |ui| {
            const ROW_W: f32 = 260.0;
            const CLOSE_W: f32 = 36.0;
            const ROW_H: f32 = 32.0;
            ui.set_min_width(ROW_W);
            for (i, doc) in self.graph_tabs.iter().enumerate() {
                let mark = if i == self.active_graph {
                    format!("{} {}", icons::CHECK, doc.title())
                } else {
                    format!("     {}", doc.title())
                };
                ui.horizontal(|ui| {
                    let gap = ui.spacing().item_spacing.x;
                    let label_w = ROW_W - CLOSE_W - gap;
                    if ui
                        .add_sized(
                            [label_w, ROW_H],
                            egui::Button::selectable(i == self.active_graph, mark),
                        )
                        .clicked()
                    {
                        switch_to = Some(i);
                    }
                    if ui
                        .add_sized([CLOSE_W, ROW_H], egui::Button::new(icons::TRASH))
                        .on_hover_text("Close tab")
                        .clicked()
                    {
                        close_idx = Some(i);
                    }
                });
            }
            ui.separator();
            if ui.button("Close all").clicked() {
                close_all = true;
            }
        });
        if let Some(i) = switch_to {
            self.active_graph = i;
            self.executing = None;
        }
        if let Some(i) = close_idx {
            self.close_graph_tab(i);
        }
        if close_all {
            self.close_all_graph_tabs();
        }
    }

    fn graph_canvas(&mut self, ui: &mut egui::Ui, host: &Host) {
        let fallback_pane = ui.available_rect_before_wrap();

        let preview = self
            .running
            .then(|| {
                self.preview
                    .as_ref()
                    .map(|t| egui::ImageSource::Texture(egui::load::SizedTexture::from_handle(t)))
            })
            .flatten();
        let progress = (self.running && self.progress.1 > 0).then_some(self.progress);
        let executing = self.executing;
        let loras = self.installed_loras.clone();
        let (long_press, lora_picks) = {
            let Some(doc) = self.active_doc_mut() else { return };
            let props = doc.props_node;
            doc.graph.set_live_execution(executing, progress, preview);
            if let Some(tapped) =
                doc.view.show(ui, &mut doc.graph, executing, props, &doc.bypassed, &loras)
            {
                doc.props_node = Some(tapped);
            }
            (doc.view.take_long_press(), doc.view.take_lora_picks())
        };
        for pick in lora_picks {
            self.apply_lora_pick(pick);
        }
        // Snapshot the graph once an edit settles. This has to run after `show`, which is where
        // snarl applies wire, drag and widget changes we never see directly.
        let mut graph_committed = false;
        {
            let now = ui.input(|i| i.time);
            let held = ui.ctx().input(|i| i.pointer.any_down());
            if let Some(doc) = self.active_doc_mut() {
                // A queued auto-layout is about to move every node; let it land inside the same
                // entry as the edit that triggered it instead of becoming a second undo step.
                let busy = held || doc.view.arrange_pending();
                if doc.history_rebase && !busy {
                    doc.history_rebase = false;
                    doc.history.reset(&doc.graph.snarl);
                }
                graph_committed = doc.history.observe(&doc.graph.snarl, now, busy);
            }
        }
        if graph_committed
            && self
                .create_graph_id
                .is_some_and(|id| self.active_doc().is_some_and(|d| d.id == id))
        {
            self.pull_linked_graph_to_create();
        }
        self.undo_redo_buttons(ui, host);
        match long_press {
            Some(LongPress::Canvas(graph_pos)) => {
                let screen = ui
                    .ctx()
                    .input(|i| i.pointer.interact_pos())
                    .unwrap_or(ui.clip_rect().center());
                // Offset so the finger-up doesn't immediately hit a button.
                self.node_menu = None;
                self.canvas_menu = Some((graph_pos, screen + egui::vec2(12.0, -48.0), false));
                host.haptic(Haptic::Medium);
            }
            Some(LongPress::Node(nid)) => {
                let screen = ui
                    .ctx()
                    .input(|i| i.pointer.interact_pos())
                    .unwrap_or(ui.clip_rect().center());
                self.canvas_menu = None;
                self.node_menu = Some((nid, screen + egui::vec2(12.0, -48.0), false));
                host.haptic(Haptic::Medium);
            }
            None => {}
        }
        self.canvas_context_menu(ui, host);
        self.node_context_menu(ui, host);
        // Prefer the canvas view rect so the play FAB lines up with the lock button.
        let fab_pane = self
            .active_doc()
            .map(|d| d.view.view_rect)
            .filter(|r| r.is_finite() && r.width() >= 80.0)
            .unwrap_or(fallback_pane);
        self.queue_fab(ui.ctx(), host, fab_pane, QueueFabKind::Graph);
        // After canvas overlays (minimap/lock) so Tooltip-order windows always win the stack.
        self.workflow_window(ui.ctx());
        self.add_node_window(ui.ctx());
        self.search_window(ui.ctx());
        self.save_window(ui.ctx());
    }

    /// Popup after a long-press on empty graph canvas.
    fn canvas_context_menu(&mut self, ui: &mut egui::Ui, host: &Host) {
        let Some((graph_pos, screen, armed)) = self.canvas_menu else { return };
        // Arm only after the opening press is fully idle (not on the release frame itself).
        if !armed {
            let idle = ui.ctx().input(|i| !i.pointer.any_down() && !i.pointer.any_click());
            if idle {
                if let Some(m) = self.canvas_menu.as_mut() {
                    m.2 = true;
                }
            }
        }
        let sys_clip = host.clipboard_text().filter(|t| {
            let t = t.trim();
            t.starts_with('{') || t.starts_with('[')
        });
        let has_clip = self.workflow_clip.is_some() || sys_clip.is_some();
        let mut close = false;
        let mut add = false;
        let mut paste = false;
        let mut insert_app = false;
        let resp = egui::Area::new(egui::Id::new("graph-canvas-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    if ui.button(format!("{} Add node…", icons::ADD)).clicked() {
                        add = true;
                    }
                    if ui
                        .button(format!("{} Insert app…", icons::ADD))
                        .on_hover_text("Drop a saved app's nodes in, already wired together")
                        .clicked()
                    {
                        insert_app = true;
                    }
                    if ui
                        .add_enabled(has_clip, egui::Button::new(format!("{} Paste workflow", icons::PROPS)))
                        .on_hover_text("Load a workflow previously copied from the gallery")
                        .clicked()
                    {
                        paste = true;
                    }
                });
            });
        let armed = self.canvas_menu.map(|m| m.2).unwrap_or(false);
        // Close on a tap that lands outside the menu (only after armed).
        let outside_click = armed
            && ui.ctx().input(|i| i.pointer.any_click())
            && !resp.response.contains_pointer()
            && !ui.ctx().input(|i| i.pointer.any_down());
        if outside_click {
            close = true;
        }
        if add {
            self.add_open = true;
            self.add_pos = graph_pos - egui::vec2(90.0, 50.0);
            close = true;
        }
        if insert_app {
            self.app_picker = self.active_doc().map(|d| AppPickTarget::Canvas {
                doc: d.id,
                at: graph_pos - egui::vec2(90.0, 50.0),
            });
            self.app_filter.clear();
            close = true;
        }
        if paste {
            let body = self.workflow_clip.clone().or(sys_clip);
            if let (Some(body), Some(schemas)) = (body, self.schemas.clone()) {
                self.graph_status.clear();
                self.wf_loading = true;
                self.engine.as_ref().unwrap().load_workflow_json(
                    "pasted.json".into(),
                    body,
                    schemas,
                );
            }
            close = true;
        }
        if close {
            self.canvas_menu = None;
        }
    }

    /// Popup after a long-press on a graph node (bypass / auto-wire).
    fn node_context_menu(&mut self, ui: &mut egui::Ui, host: &Host) {
        let Some((nid, screen, armed)) = self.node_menu else { return };
        if !armed {
            let idle = ui.ctx().input(|i| !i.pointer.any_down() && !i.pointer.any_click());
            if idle
                && let Some(m) = self.node_menu.as_mut()
            {
                m.2 = true;
            }
        }
        let bypassed = self.active_doc().is_some_and(|d| d.bypassed.contains(&nid));
        let class = self
            .active_doc()
            .and_then(|d| d.graph.snarl.get_node(nid))
            .map(|n| n.object.name.clone())
            .unwrap_or_default();
        let mut close = false;
        let mut toggle_bypass = false;
        let mut auto_wire = false;
        let resp = egui::Area::new(egui::Id::new("graph-node-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let bypass_label = if bypassed {
                        format!("{} Unbypass", icons::CHECK)
                    } else {
                        format!("{} Bypass", icons::WARN)
                    };
                    if ui.button(bypass_label).clicked() {
                        toggle_bypass = true;
                    }
                    if ui
                        .button(format!("{} Auto wire", icons::GRAPH))
                        .on_hover_text("Insert this node into the MODEL/CLIP chain")
                        .clicked()
                    {
                        auto_wire = true;
                    }
                    if !class.is_empty() {
                        ui.weak(elide(&class, 36));
                    }
                });
            });
        let armed = self.node_menu.map(|m| m.2).unwrap_or(false);
        let outside_click = armed
            && ui.ctx().input(|i| i.pointer.any_click())
            && !resp.response.contains_pointer()
            && !ui.ctx().input(|i| i.pointer.any_down());
        if outside_click {
            close = true;
        }
        if toggle_bypass {
            if let Some(doc) = self.active_doc_mut() {
                if !doc.bypassed.remove(&nid) {
                    doc.bypassed.insert(nid);
                }
                host.haptic(Haptic::Medium);
            }
            close = true;
        }
        if auto_wire {
            self.auto_wire_node(nid, host);
            close = true;
        }
        if close {
            self.node_menu = None;
        }
    }

    /// Apply catalog strengths (and prompt triggers when a positive CLIP encode exists).
    fn apply_lora_pick(&mut self, pick: LoraPick) {
        let (sm, sc, triggers) = match self.lora_catalog.entry(&pick.file) {
            Some(e) => {
                let (sm, sc) = e.add_strengths();
                (sm, sc, e.trigger_text())
            }
            None => (1.0, 1.0, String::new()),
        };
        let Some(doc) = self.active_doc_mut() else { return };
        if let Some(data) = doc.graph.snarl.get_node_mut(pick.node) {
            graphview::apply_lora_strengths(data, sm, sc);
        }
        if !triggers.is_empty() {
            inject_lora_triggers(&mut doc.graph.snarl, &triggers);
        }
        self.graph_status = format!(
            "LoRA {} — strength {:.2}/{:.2}",
            elide(&pick.file, 40),
            sm,
            sc
        );
    }

    /// Splice a LoRA (or similar) into the MODEL / CLIP chain ahead of the sampler.
    fn auto_wire_node(&mut self, nid: NodeId, host: &Host) {
        if self.active_doc().is_some_and(|d| d.view.locked) {
            self.graph_status = "Graph is locked — unlock to auto-wire".into();
            host.haptic(Haptic::Warning);
            return;
        }
        let (class, model_in, model_out, clip_in, clip_out, lora_file) = {
            let Some(doc) = self.active_doc() else { return };
            let Some(data) = doc.graph.snarl.get_node(nid) else {
                self.graph_status = "Node gone".into();
                return;
            };
            let lora_file = data.inputs.iter().find(|i| i.name == "lora_name").and_then(|i| {
                match &i.value {
                    FlowValueType::Array { selected, .. } if !selected.is_empty() => {
                        Some(selected.clone())
                    }
                    _ => None,
                }
            });
            (
                data.object.name.clone(),
                data.inputs.iter().position(|i| i.name == "model"),
                data.outputs.iter().position(|o| {
                    matches!(o.typ, rucomfyui::object_info::ObjectType::Model)
                        || o.name.eq_ignore_ascii_case("MODEL")
                }),
                data.inputs.iter().position(|i| i.name == "clip"),
                data.outputs.iter().position(|o| {
                    matches!(o.typ, rucomfyui::object_info::ObjectType::Clip)
                        || o.name.eq_ignore_ascii_case("CLIP")
                }),
                lora_file,
            )
        };
        let wants_model = model_in.is_some() && model_out.is_some();
        let wants_clip = clip_in.is_some() && clip_out.is_some();
        if !wants_model && !wants_clip {
            self.graph_status = format!("Auto wire: {class} has no MODEL/CLIP ports");
            host.haptic(Haptic::Warning);
            return;
        }

        let mut wired = 0usize;
        {
            let Some(doc) = self.active_doc_mut() else { return };
            if wants_model
                && let (Some(mi), Some(mo)) = (model_in, model_out)
                && let Some((from, to)) = find_chain_edge(
                    &doc.graph.snarl,
                    nid,
                    rucomfyui::object_info::ObjectType::Model,
                    "model",
                )
            {
                splice_edge(&mut doc.graph.snarl, from, to, nid, mi, mo);
                wired += 1;
            }
            if wants_clip
                && let (Some(ci), Some(co)) = (clip_in, clip_out)
                && let Some((from, to)) = find_chain_edge(
                    &doc.graph.snarl,
                    nid,
                    rucomfyui::object_info::ObjectType::Clip,
                    "clip",
                )
            {
                splice_edge(&mut doc.graph.snarl, from, to, nid, ci, co);
                wired += 1;
            }
        }

        if let Some(file) = lora_file {
            self.apply_lora_pick(LoraPick { node: nid, file });
        }

        if wired == 0 {
            self.graph_status = "Auto wire: no MODEL/CLIP chain found".into();
            host.haptic(Haptic::Warning);
        } else {
            self.graph_status = format!("Auto-wired {class} ({wired} link(s))");
            host.haptic(Haptic::Success);
        }
    }

    fn graph_controls(&mut self, ui: &mut egui::Ui, _host: &Host) {
        let connected = matches!(self.conn, Conn::Connected);
        let has_graph = self.has_graph_editor();
        let has_nodes = self.active_doc().is_some_and(|d| !d.is_empty());
        let locked = self.active_doc().is_some_and(|d| d.view.locked);
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
                self.save_name = self
                    .active_doc()
                    .map(|d| {
                        if d.name.is_empty() {
                            "mobile/untitled.json".to_string()
                        } else {
                            d.name.clone()
                        }
                    })
                    .unwrap_or_else(|| "mobile/untitled.json".into());
            }
            // The active tab is the unit of publication — there is no selection model.
            if ui
                .add_enabled(has_nodes, egui::Button::new(format!("{} Save tab as app…", icons::ADD)))
                .on_hover_text("Publish this graph as a reusable Create-tab enhance step")
                .clicked()
            {
                match self.derive_app_draft() {
                    Some(draft) => self.publish = Some(draft),
                    None => {
                        self.graph_status =
                            "Needs a graph whose final IMAGE output is unconnected".into();
                    }
                }
            }
            ui.separator();
            if ui
                .add_enabled(
                    has_graph && !locked,
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
                    has_graph && !locked,
                    egui::Button::new(format!("{} Add node…", icons::ADD)),
                )
                .clicked()
            {
                self.add_open = true;
                if let Some(center) = self.active_doc().and_then(|d| d.view.center_in_graph()) {
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
                .add_enabled(has_nodes, egui::Button::new("Auto-arrange"))
                .clicked()
            {
                if let Some(doc) = self.active_doc_mut() {
                    doc.view.request_arrange();
                }
            }
        });

        up_menu(ui, format!("{} View", icons::SEARCH), |ui| {
            if ui.add_enabled(has_nodes, egui::Button::new("Fit to screen")).clicked() {
                if let Some(doc) = self.active_doc_mut() {
                    doc.view.request_fit();
                }
            }
            if ui.add_enabled(has_nodes, egui::Button::new("Go to first node")).clicked() {
                let pos = self
                    .active_doc()
                    .and_then(|d| graphview::first_node_pos(&d.graph.snarl));
                if let Some(pos) = pos
                    && let Some(doc) = self.active_doc_mut()
                {
                    doc.view.center_on(pos);
                }
            }
            ui.separator();
            let follow = if self.auto_follow {
                format!("{} Auto-follow: on", icons::CHECK)
            } else {
                "     Auto-follow: off".to_string()
            };
            if ui
                .selectable_label(self.auto_follow, follow)
                .on_hover_text("Pan and zoom to the running node during a queue")
                .clicked()
            {
                self.auto_follow = !self.auto_follow;
            }
            let arrange = if self.auto_arrange {
                format!("{} Auto-arrange: on", icons::CHECK)
            } else {
                "     Auto-arrange: off".to_string()
            };
            if ui
                .selectable_label(self.auto_arrange, arrange)
                .on_hover_text("Relayout nodes when you open the graph after a workflow loads")
                .clicked()
            {
                self.auto_arrange = !self.auto_arrange;
            }
        });
    }

    fn clear_graph(&mut self) {
        if let Some(doc) = self.active_doc_mut() {
            doc.clear_content();
        }
        self.restore_workflow = None;
        self.graph_status.clear();
        self.executing = None;
        self.add_pos = egui::pos2(80.0, 80.0);
    }

    fn save_window(&mut self, ctx: &egui::Context) {
        if !self.save_open {
            return;
        }
        let mut open = true;
        let mut submit = false;
        let active_name = self.active_doc().map(|d| d.name.clone()).unwrap_or_default();
        centered(egui::Window::new("Save workflow"))
            .collapsible(false)
            .open(&mut open)
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
                    } else if self.save_name.trim() == active_name.trim() && !active_name.is_empty()
                    {
                        ui.weak("overwrites the opened workflow");
                    }
                });
            });
        if submit {
            let mut name = self.save_name.trim().trim_matches('/').to_string();
            if !name.to_lowercase().ends_with(".json") {
                name.push_str(".json");
            }
            self.save_name = name.clone();
            let exported = self.active_doc().and_then(|doc| {
                let schemas = self.schemas.as_ref()?;
                Some(doc.view.export_ui(&doc.graph, schemas, &doc.bypassed))
            });
            match exported.and_then(|v| serde_json::to_string(&v).ok()) {
                Some(body) => {
                    self.saving = true;
                    self.graph_status.clear();
                    self.engine.as_ref().unwrap().save_workflow(name, body);
                }
                None => {
                    self.graph_status = "Export failed".into();
                    self.log.error("workflow export failed");
                }
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
        let props = self.active_doc().and_then(|d| d.props_node);
        centered(egui::Window::new("Find node"))
            .collapsible(false)
            .open(&mut open)
            .default_size([340.0, 400.0])
            .show(ctx, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_filter)
                        .hint_text("search this workflow")
                        .desired_width(f32::INFINITY),
                );
                ui.separator();
                let Some(doc) = self.active_doc() else { return };
                let filter = self.search_filter.to_lowercase();
                crate::theme::scroll_vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let mut shown = 0usize;
                    for (id, pos, data) in doc.graph.snarl.nodes_pos_ids() {
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
                        if ui.selectable_label(props == Some(id), label).clicked() {
                            jump = Some((id, pos));
                        }
                    }
                    if shown == 0 {
                        ui.weak("no matches");
                    }
                });
            });
        if let Some((id, pos)) = jump {
            if let Some(doc) = self.active_doc_mut() {
                doc.props_node = Some(id);
                doc.view.center_on(pos);
            }
            open = false;
        }
        self.search_open = open;
    }

    fn props_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        // Android system Back / Esc returns to the canvas.
        if ui.ctx().input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
        }) {
            self.graph_pane = GraphPane::Canvas;
            return;
        }
        if !self.has_graph_editor() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| ui.label("Connect to a server first."));
            return;
        }
        let Some(node) = self.active_doc().and_then(|d| d.props_node) else {
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
                if let Some(doc) = self.active_doc_mut()
                    && let Some(info) = doc.graph.snarl.get_node_info(node)
                {
                    doc.view.center_on(info.pos);
                }
                self.graph_pane = GraphPane::Canvas;
            }
        });
        ui.separator();
        // Deliberate edits stay possible here even when the canvas is in view-only mode.
        // Capture width *before* the ScrollArea: inside a vertical ScrollArea, available_width
        // grows with content desired sizes, so TextEdit(INFINITY / available) clips on the right.
        let content_width = ui.available_width();
        let mut exists = true;
        let loras = self.installed_loras.clone();
        let mut lora_picks = Vec::new();
        crate::theme::scroll_vertical()
            .auto_shrink([false, false])
            .max_width(content_width)
            .show(ui, |ui| {
                ui.set_width(content_width);
                ui.set_max_width(content_width);
                if let Some(doc) = self.active_doc_mut() {
                    exists = graphview::node_properties(
                        ui,
                        &mut doc.graph,
                        node,
                        false,
                        &loras,
                        &mut lora_picks,
                    );
                }
                // For LoadImage-style nodes, a thumbnail picker over the server inputs or the phone.
                self.loadimage_picker(ui, host, node);
                ui.add_space(12.0);
            });
        for pick in lora_picks {
            self.apply_lora_pick(pick);
        }
        if !exists
            && let Some(doc) = self.active_doc_mut()
        {
            doc.props_node = None;
        }
    }

    /// If `node` has an `image` selector (LoadImage), render a thumbnail picker so you can see what
    /// you're choosing — either the server's input images (previewed via `/view?type=input`) or the
    /// phone's own photo gallery (MediaStore), uploaded to the server on tap.
    fn loadimage_picker(&mut self, ui: &mut egui::Ui, host: &Host, node: NodeId) {
        let (input_idx, options, selected) = {
            let Some(doc) = self.active_doc() else { return };
            let Some(data) = doc.graph.snarl.get_node(node) else { return };
            let found = data.inputs.iter().enumerate().find_map(|(i, inp)| {
                if !inp.name.eq_ignore_ascii_case("image") {
                    return None;
                }
                match &inp.value {
                    FlowValueType::Array { options, selected } => {
                        Some((i, options.clone(), selected.clone()))
                    }
                    _ => None,
                }
            });
            match found {
                Some(v) => v,
                None => return,
            }
        };

        ui.separator();
        ui.strong(format!("{} Choose image", icons::IMAGE));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.img_pick_source, ImgPickSource::Server, "Server");
            ui.selectable_value(&mut self.img_pick_source, ImgPickSource::Device, "Device");
        });
        match self.img_pick_source {
            ImgPickSource::Server => self.loadimage_server_grid(ui, node, input_idx, &options, &selected),
            ImgPickSource::Device => self.loadimage_device_grid(ui, host, node),
        }
    }

    /// Column count and square tile size for a picker grid inside the props scroll area, clamped to
    /// the visible width so the row can't spill past the screen edge (see the grid-width note).
    fn picker_grid_dims(ui: &egui::Ui) -> (usize, f32) {
        let spacing = ui.spacing().item_spacing.x;
        // `available_width()` over-reports inside this vertical scroll area (the scrollbar gutter
        // isn't reserved), which once spilled the grid a whole column past the screen edge. Clamp
        // to what's actually visible from the grid's left edge, leaving the scrollbar its width.
        let bar = ui.spacing().scroll.bar_width + ui.spacing().scroll.bar_inner_margin;
        let visible_right = ui.clip_rect().right() - bar - 4.0;
        let avail = (visible_right - ui.max_rect().left()).min(ui.available_width()).max(120.0);
        let cols = ((avail / 104.0).floor() as usize).clamp(2, 5);
        let tile = ((avail - spacing * (cols as f32 - 1.0)) / cols as f32).max(64.0);
        (cols, tile)
    }

    /// Grid over the server's uploaded input images.
    fn loadimage_server_grid(
        &mut self,
        ui: &mut egui::Ui,
        node: NodeId,
        input_idx: usize,
        options: &[String],
        selected: &str,
    ) {
        if options.is_empty() {
            ui.weak("No input images on the server yet — pick one from Device.");
            return;
        }
        ui.add(
            egui::TextEdit::singleline(&mut self.img_pick_filter)
                .hint_text("filter input images")
                .desired_width(f32::INFINITY),
        );
        let filter = self.img_pick_filter.to_lowercase();
        let matches: Vec<&String> = options
            .iter()
            .filter(|o| filter.is_empty() || o.to_lowercase().contains(&filter))
            .take(120)
            .collect();

        let (cols, tile) = Self::picker_grid_dims(ui);
        let mut picked: Option<String> = None;
        for row in matches.chunks(cols) {
            ui.horizontal(|ui| {
                for name in row {
                    let key = format!("input#{name}");
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(tile, tile), egui::Sense::hover());
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    let is_sel = **name == selected;
                    match self.thumbs.get(&key) {
                        Some(tex) => {
                            let img = egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                                .fit_to_exact_size(egui::vec2(tile, tile))
                                .sense(egui::Sense::click());
                            if ui.put(rect, img).clicked() {
                                picked = Some((*name).clone());
                            }
                        }
                        None => {
                            if self.thumbs.claim(&key) {
                                self.engine.as_ref().unwrap().fetch_input_thumb((*name).clone());
                            }
                            if ui.put(rect, egui::Button::new(elide(name, 12)).wrap()).clicked() {
                                picked = Some((*name).clone());
                            }
                        }
                    }
                    if is_sel {
                        ui.painter().rect_stroke(
                            rect,
                            3.0,
                            egui::Stroke::new(2.5, egui::Color32::from_rgb(150, 140, 226)),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
            });
        }
        if options.len() > matches.len() {
            ui.weak(format!("… {} more — type to filter", options.len() - matches.len()));
        }

        if let Some(chosen) = picked
            && let Some(doc) = self.active_doc_mut()
            && let Some(data) = doc.graph.snarl.get_node_mut(node)
            && let Some(inp) = data.inputs.get_mut(input_idx)
            && let FlowValueType::Array { selected, .. } = &mut inp.value
        {
            *selected = chosen;
        }
    }

    /// Grid over the phone's photo gallery (MediaStore). Tapping an image uploads it to the server
    /// as a LoadImage input; the node's selection updates when the upload finishes.
    /// How many device thumbnails to load synchronously per frame. Each load is a blocking JNI
    /// round trip on the render thread, so cap it to keep scrolling smooth; the rest fill in over
    /// the next frames (repaint is requested while any tile is still pending).
    const DEVICE_THUMBS_PER_FRAME: usize = 2;

    fn loadimage_device_grid(&mut self, ui: &mut egui::Ui, host: &Host, node: NodeId) {
        if !host.has_media_images_permission() {
            ui.add_space(4.0);
            ui.label("Allow access to your photos to pick an image from this device.");
            if ui.button(format!("{} Open settings to allow", icons::IMAGE)).clicked() {
                host.request_media_images_permission();
                self.device_images_loaded = false;
            }
            ui.weak("Grant “Photos and videos”, then return here.");
            // Poll (not every frame) so the grid appears when the user returns having granted it.
            ui.ctx().request_repaint_after(Duration::from_millis(400));
            return;
        }
        if !self.device_images_loaded {
            self.device_images = host.list_device_images(300);
            self.device_images_loaded = true;
        }
        ui.horizontal(|ui| {
            if ui.button(format!("{} Refresh", icons::REFRESH)).clicked() {
                self.thumbs.reset_pending();
                self.device_images = host.list_device_images(300);
            }
            if !self.pending_uploads.is_empty() {
                ui.spinner();
                ui.weak("uploading…");
            }
        });
        if self.device_images.is_empty() {
            ui.weak("No photos found on this device.");
            return;
        }

        let (cols, tile) = Self::picker_grid_dims(ui);
        // Clone the listing so the per-tile thumbnail cache can be mutated without aliasing it.
        let images = self.device_images.clone();
        let mut pick: Option<(i64, String)> = None;
        let mut loaded_this_frame = 0usize;
        let mut more_pending = false;
        for row in images.chunks(cols) {
            ui.horizontal(|ui| {
                for (id, name) in row {
                    let key = format!("dev#{id}");
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(tile, tile), egui::Sense::hover());
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    match self.thumbs.get(&key) {
                        Some(tex) => {
                            let img = egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                                .fit_to_exact_size(egui::vec2(tile, tile))
                                .sense(egui::Sense::click());
                            if ui.put(rect, img).clicked() {
                                pick = Some((*id, name.clone()));
                            }
                        }
                        None => {
                            // Load a bounded number of thumbnails per frame; the rest wait a frame.
                            // Only claim when actually loading (claim marks the tile done, so
                            // claiming without loading would strand it). load_device_thumbnail
                            // returns raw RGBA, so there's no re-decode.
                            if loaded_this_frame < Self::DEVICE_THUMBS_PER_FRAME {
                                if self.thumbs.claim(&key) {
                                    loaded_this_frame += 1;
                                    if let Some((w, h, rgba)) = host.load_device_thumbnail(*id, 256)
                                    {
                                        let image = egui::ColorImage::from_rgba_unmultiplied(
                                            [w as usize, h as usize],
                                            &rgba,
                                        );
                                        let cost = (w * h * 4) as usize;
                                        let tex = ui.ctx().load_texture(
                                            &key,
                                            image,
                                            egui::TextureOptions::LINEAR,
                                        );
                                        self.thumbs.insert(key.clone(), tex, cost);
                                    }
                                }
                            } else {
                                // Hit the per-frame budget; this tile still wants loading.
                                more_pending = true;
                            }
                            let label = if name.is_empty() { "photo" } else { name.as_str() };
                            if ui.put(rect, egui::Button::new(elide(label, 12)).wrap()).clicked() {
                                pick = Some((*id, name.clone()));
                            }
                        }
                    }
                }
            });
        }
        // Keep animating only while more thumbnails are waiting to load.
        if more_pending {
            ui.ctx().request_repaint();
        }

        if let Some((id, name)) = pick {
            match host.load_device_image(id) {
                Some(bytes) if !bytes.is_empty() => {
                    let fname = if name.is_empty() { format!("device_{id}.jpg") } else { name };
                    let token = self.next_upload_id;
                    self.next_upload_id += 1;
                    let owner = self.active_doc().map(|d| (d.id, d.epoch));
                    if let Some((doc_id, epoch)) = owner {
                        self.pending_uploads.insert(token, (doc_id, epoch, node));
                    }
                    self.engine.as_ref().unwrap().upload_input_image(token, fname, bytes);
                    host.haptic(Haptic::Light);
                }
                _ => {
                    self.note = "Couldn't read that photo from the device".into();
                    host.haptic(Haptic::Error);
                }
            }
        }
    }

    fn workflow_window(&mut self, ctx: &egui::Context) {
        if !self.wf_open {
            return;
        }
        let mut open = true;
        let mut picked: Option<String> = None;
        centered(egui::Window::new("Server workflows"))
            .collapsible(false)
            .open(&mut open)
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
                crate::theme::scroll_vertical().auto_shrink([false, false]).show(ui, |ui| {
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
            self.graph_status.clear();
            self.engine.as_ref().unwrap().open_workflow(name, schemas);
        }
        self.wf_open = open;
    }

    fn add_node_window(&mut self, ctx: &egui::Context) {
        if !self.add_open {
            return;
        }
        let mut open = true;
        let mut inserted: Option<NodeId> = None;
        let insert_pos = self.add_pos;
        let loras = self.installed_loras.clone();
        centered(egui::Window::new("Add node"))
            .collapsible(false)
            .open(&mut open)
            .default_size([340.0, 420.0])
            .show(ctx, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_filter)
                        .hint_text("search node types")
                        .desired_width(f32::INFINITY),
                );
                ui.separator();
                let filter = self.add_filter.to_lowercase();
                let Some(doc) = self.active_doc_mut() else { return };
                let mut pick = None;
                {
                    // Group the matching node types by category (nested categories keep their
                    // prefix), so the picker is browsable headers rather than one 2800-row list.
                    let mut cats: std::collections::BTreeMap<&str, Vec<_>> =
                        std::collections::BTreeMap::new();
                    for object in doc.graph.object_info.values() {
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
                    crate::theme::scroll_vertical().auto_shrink([false, false]).show(ui, |ui| {
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
                    let nid = doc.graph.snarl.insert_node(insert_pos, FlowNodeData::new(object));
                    if let Some(data) = doc.graph.snarl.get_node_mut(nid) {
                        graphview::ensure_file_combos(data, &doc.graph.object_info, &loras);
                    }
                    inserted = Some(nid);
                }
            });
        if let Some(nid) = inserted {
            if let Some(file) = self.active_doc().and_then(|doc| {
                doc.graph.snarl.get_node(nid).and_then(|data| {
                    data.inputs.iter().find(|i| i.name == "lora_name").and_then(|i| {
                        match &i.value {
                            FlowValueType::Array { selected, .. } if !selected.is_empty() => {
                                Some(selected.clone())
                            }
                            _ => None,
                        }
                    })
                })
            }) {
                self.apply_lora_pick(LoraPick { node: nid, file });
            }
            self.add_pos += egui::vec2(48.0, 48.0);
            if self.add_pos.y > 800.0 {
                self.add_pos = egui::pos2(120.0, 80.0);
            }
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
        // Supersede any in-flight pages of the previous query (auto-load chains overlap).
        self.gallery_gen = self.gallery_gen.wrapping_add(1);
        self.engine.as_ref().unwrap().gallery_list(
            self.gallery_gen,
            0,
            self.gallery_page_size(),
            &self.gallery_q,
            &self.gallery_view,
        );
    }

    /// The gallery's bottom control bar: search, model filter, sort, grouping and column count.
    /// Returns whether the listing must be re-queried — every control except the column count is
    /// applied server-side across the whole listing, not to the page already fetched.
    /// Should the whole filtered/grouped set be auto-loaded (rather than paged by hand)?
    fn gallery_wants_all(&self) -> bool {
        self.gallery_view.group != GalleryGroup::None
            || !self.gallery_view.model.is_empty()
            || self.gallery_view.album.is_some()
            // The media filter is client-side, so the full set must be present to filter over.
            || self.gallery_view.media != GalleryMedia::All
    }

    fn gallery_controls(&mut self, ui: &mut egui::Ui, connected: bool) -> bool {
        let mut changed = false;
        // One row: search + refresh + View (rightmost). Filters live in View submenus.
        ui.horizontal(|ui| {
            let refresh_w = 40.0;
            let view_w = 72.0;
            let search_w = (ui.available_width() - refresh_w - view_w - 8.0).max(96.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.gallery_q)
                    .hint_text(format!("{} search", icons::SEARCH))
                    .desired_width(search_w),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                changed = true;
            }
            if ui
                .add_enabled(connected, egui::Button::new(icons::REFRESH).min_size(egui::vec2(36.0, 28.0)))
                .on_hover_text("Refresh")
                .clicked()
            {
                changed = true;
            }

            up_menu(ui, format!("{} View", icons::GALLERY), |ui| {
                if ui
                    .button(format!("{} Select", icons::CHECK))
                    .on_hover_text("Multi-select — or long-press a photo")
                    .clicked()
                {
                    self.select_mode = true;
                }
                ui.separator();

                ui.menu_button(
                    format!("{} Sort · {}", icons::SORT, self.gallery_view.sort.label()),
                    |ui| {
                        for s in GallerySort::ALL {
                            changed |= ui
                                .selectable_value(&mut self.gallery_view.sort, *s, s.label())
                                .clicked();
                        }
                    },
                );

                ui.menu_button(format!("Group · {}", self.gallery_view.group.label()), |ui| {
                    for g in GalleryGroup::ALL {
                        changed |= ui
                            .selectable_value(&mut self.gallery_view.group, *g, g.label())
                            .clicked();
                    }
                    if self.gallery_view.group != GalleryGroup::None {
                        ui.separator();
                        let open_label = if self.gallery_view.groups_open {
                            format!("{} Headers open", icons::CHECK)
                        } else {
                            "     Headers closed".to_string()
                        };
                        if ui
                            .selectable_label(self.gallery_view.groups_open, open_label)
                            .on_hover_text("Default open/closed state for group headers")
                            .clicked()
                        {
                            self.gallery_view.groups_open = !self.gallery_view.groups_open;
                        }
                    }
                });

                ui.menu_button(format!("Columns · {}", self.gallery_view.columns), |ui| {
                    for n in 1..=3usize {
                        if ui
                            .selectable_label(
                                self.gallery_view.columns == n,
                                format!("{n} column{}", if n == 1 { "" } else { "s" }),
                            )
                            .clicked()
                        {
                            self.gallery_view.columns = n;
                        }
                    }
                });

                let media_label = match self.gallery_view.media {
                    GalleryMedia::All => format!("{} Media · All", icons::GALLERY),
                    GalleryMedia::Images => format!("{} Media · Images", icons::IMAGE),
                    GalleryMedia::Videos => format!("{} Media · Videos", icons::RUN),
                };
                ui.menu_button(media_label, |ui| {
                    for m in GalleryMedia::ALL {
                        changed |= ui
                            .selectable_value(&mut self.gallery_view.media, *m, m.label())
                            .clicked();
                    }
                });

                let model_label = if self.gallery_view.model.is_empty() {
                    format!("{} Model · All", icons::MODEL)
                } else {
                    format!("{} Model · {}", icons::MODEL, elide(&self.gallery_view.model, 18))
                };
                ui.menu_button(model_label, |ui| {
                    crate::theme::scroll_vertical().max_height(280.0).show(ui, |ui| {
                        changed |= ui
                            .selectable_value(
                                &mut self.gallery_view.model,
                                String::new(),
                                "All models",
                            )
                            .clicked();
                        for m in &self.facets.models {
                            let label = format!("{}  ({})", elide(&m.name, 40), m.count);
                            changed |= ui
                                .selectable_value(
                                    &mut self.gallery_view.model,
                                    m.name.clone(),
                                    label,
                                )
                                .clicked();
                        }
                        if self.facets.models.is_empty() {
                            ui.weak("no models indexed yet");
                        }
                    });
                });

                let album_label = match self.gallery_view.album {
                    None => format!("{} Album · All", icons::ALBUM),
                    Some(id) => self
                        .albums
                        .iter()
                        .find(|a| a.id == id)
                        .map(|a| format!("{} Album · {}", icons::ALBUM, elide(&a.name, 18)))
                        .unwrap_or_else(|| format!("{} Album", icons::ALBUM)),
                };
                ui.menu_button(album_label, |ui| {
                    crate::theme::scroll_vertical().max_height(280.0).show(ui, |ui| {
                        changed |= ui
                            .selectable_value(&mut self.gallery_view.album, None, "All images")
                            .clicked();
                        for a in &self.albums {
                            let label =
                                format!("{} {}  ({})", icons::ALBUM, elide(&a.name, 28), a.count);
                            changed |= ui
                                .selectable_value(
                                    &mut self.gallery_view.album,
                                    Some(a.id),
                                    label,
                                )
                                .clicked();
                        }
                        ui.separator();
                        if ui.button(format!("{} Manage albums…", icons::FOLDER)).clicked() {
                            self.album_manage_open = true;
                        }
                    });
                });
            });
        });
        changed
    }

    /// Create / rename / delete albums. Album *selection* is under View → Album; this window is
    /// only management. Rename uses the text field's contents as the new name.
    fn album_manage_window(&mut self, ctx: &egui::Context) {
        if !self.album_manage_open {
            return;
        }
        let mut open = true;
        centered(egui::Window::new("Manage albums"))
            .collapsible(false)
            .open(&mut open)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.album_new_name)
                        .hint_text("album name (for Create / Rename)")
                        .desired_width(f32::INFINITY),
                );
                let named = !self.album_new_name.trim().is_empty();
                if ui
                    .add_enabled(named, egui::Button::new(format!("{} Create album", icons::ADD)))
                    .clicked()
                {
                    self.engine.as_ref().unwrap().album_create(self.album_new_name.trim().to_string());
                    self.album_new_name.clear();
                }
                ui.separator();
                let mut rename: Option<i64> = None;
                let mut delete: Option<(i64, String)> = None;
                crate::theme::scroll_vertical().max_height(300.0).auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        if self.albums.is_empty() {
                            ui.weak("No albums yet.");
                        }
                        for a in &self.albums {
                            ui.horizontal(|ui| {
                                ui.label(format!("{} {}  ({})", icons::ALBUM, elide(&a.name, 22), a.count));
                                if ui.small_button(icons::TRASH).on_hover_text("Delete").clicked() {
                                    delete = Some((a.id, a.name.clone()));
                                }
                                if ui
                                    .add_enabled(named, egui::Button::new("Rename"))
                                    .on_hover_text("Rename to the text above")
                                    .clicked()
                                {
                                    rename = Some(a.id);
                                }
                            });
                        }
                    },
                );
                if let Some(id) = rename {
                    self.engine.as_ref().unwrap().album_rename(id, self.album_new_name.trim().to_string());
                    self.album_new_name.clear();
                }
                if let Some((id, name)) = delete {
                    self.engine.as_ref().unwrap().album_delete(id, name);
                    if self.gallery_view.album == Some(id) {
                        self.gallery_view.album = None;
                    }
                }
            });
        self.album_manage_open = open;
    }

    fn gallery_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        let connected = matches!(self.conn, Conn::Connected);
        if self.viewer.is_some() {
            self.gallery_viewer(ui, host);
            self.album_create_window(ui.ctx());
            self.delete_confirm_window(ui.ctx(), host);
            return;
        }

        // Android Back / Esc exits multi-select (same as Done).
        if self.select_mode
            && ui.ctx().input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
            })
        {
            self.exit_select_mode();
        }

        ui.horizontal(|ui| {
            ui.strong(format!("{} Gallery", icons::GALLERY));
            if let Some(name) = self
                .gallery_view
                .album
                .and_then(|id| self.albums.iter().find(|a| a.id == id))
                .map(|a| a.name.clone())
            {
                ui.separator();
                ui.strong(format!("{} {}", icons::ALBUM, elide(&name, 20)));
            }
            if self.gallery_loading {
                ui.spinner();
            }
            if self.gallery_total > 0 {
                if self.gallery_view.media == GalleryMedia::All {
                    ui.weak(format!("{} of {}", self.gallery.len(), self.gallery_total));
                } else {
                    let media = self.gallery_view.media;
                    let n = self.gallery.iter().filter(|it| media.matches(it.is_video)).count();
                    ui.weak(format!(
                        "{n} {} · {} of {} scanned",
                        media.label().to_lowercase(),
                        self.gallery.len(),
                        self.gallery_total
                    ));
                }
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

        self.album_manage_window(ui.ctx());
        self.album_create_window(ui.ctx());
        self.delete_confirm_window(ui.ctx(), host);

        let mut refresh = false;
        egui::Panel::bottom("gallery-controls").show(ui, |ui| {
            ui.add_space(2.0);
            if self.select_mode {
                self.selection_bar(ui, host);
            } else {
                refresh = self.gallery_controls(ui, connected);
            }
            ui.add_space(2.0);
        });
        if self.gallery_pull_to_refresh(ui) {
            refresh = true;
        }
        if refresh && connected {
            self.refresh_gallery();
            self.gallery_pull = 0.0;
            self.gallery_pull_tracking = false;
        }
        if self.gallery.is_empty() && self.gallery_total == 0 && !self.gallery_loading {
            self.gallery_loading = true;
            self.engine.as_ref().unwrap().gallery_list(
                self.gallery_gen,
                0,
                self.gallery_page_size(),
                &self.gallery_q,
                &self.gallery_view,
            );
        }

        // Media filter is client-side: group over the matching subset only (indices stay original).
        let media = self.gallery_view.media;
        let visible: Vec<usize> = self
            .gallery
            .iter()
            .enumerate()
            .filter(|(_, it)| media.matches(it.is_video))
            .map(|(i, _)| i)
            .collect();
        let groups =
            crate::gallery::group_selected(&self.gallery, &visible, self.gallery_view.group);
        let cols = self.gallery_view.columns.clamp(1, 3);
        let mut open: Option<usize> = None;
        let mut load_more = false;
        self.tile_hits.clear();

        // Pull indicator sits above the list (does not disturb scroll offset).
        if self.gallery_pull > 4.0 || (self.gallery_loading && self.gallery_pull_tracking) {
            let ready = self.gallery_pull >= Self::GALLERY_PULL_THRESHOLD;
            let h = (self.gallery_pull * 0.55).clamp(18.0, 56.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), h),
                egui::Layout::top_down(egui::Align::Center),
                |ui| {
                    ui.add_space((h - 18.0) * 0.35);
                    if self.gallery_loading {
                        ui.spinner();
                    } else if ready {
                        ui.label(format!("{} Release to refresh", icons::REFRESH));
                    } else {
                        ui.weak(format!("{} Pull to refresh", icons::REFRESH));
                    }
                },
            );
        }

        // While a long-press paint-select is in progress, dragging must select tiles, not scroll.
        let mut scroll = crate::theme::scroll_vertical()
            .id_salt("gallery_list")
            .auto_shrink([false, false]);
        if self.sel_painting {
            use egui::containers::scroll_area::{DragScroll, ScrollSource};
            scroll = scroll.scroll_source(ScrollSource { drag: DragScroll::Never, ..Default::default() });
        }
        if let Some(y) = self.gallery_scroll_restore.take() {
            scroll = scroll.vertical_scroll_offset(y);
        }
        let scroll_out = scroll.show(ui, |ui| {
            for group in &groups {
                if group.label.is_empty() {
                    open = self.gallery_grid(ui, &group.items, cols).or(open);
                    continue;
                }
                let header = format!("{} ({})", elide(&group.label, 40), group.items.len());
                egui::CollapsingHeader::new(header)
                    .id_salt(&group.key)
                    .default_open(self.gallery_view.groups_open)
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
            } else if visible.is_empty() && !self.gallery_loading {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| ui.weak("Nothing matches these filters."));
            }
            ui.add_space(12.0);
        });
        self.gallery_scroll_y = scroll_out.state.offset.y;

        if load_more {
            self.gallery_loading = true;
            self.engine.as_ref().unwrap().gallery_list(
                self.gallery_gen,
                self.gallery.len() as u64,
                self.gallery_page_size(),
                &self.gallery_q,
                &self.gallery_view,
            );
        }
        // Tapping a tile opens it only in browse mode; in select mode the grid handled the toggle.
        if let Some(idx) = open {
            self.gallery_scroll_restore = Some(self.gallery_scroll_y);
            self.open_viewer(idx, host);
        }
        self.handle_gallery_gesture(ui, host);
    }

    const GALLERY_PULL_THRESHOLD: f32 = 72.0;

    /// Pull-down at the top of the gallery list → refresh. Returns true when a refresh should run.
    fn gallery_pull_to_refresh(&mut self, ui: &egui::Ui) -> bool {
        if self.sel_painting {
            self.gallery_pull = 0.0;
            self.gallery_pull_tracking = false;
            return false;
        }
        let at_top = self.gallery_scroll_y <= 1.0;
        let (pressed, released, down, delta_y) = ui.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.any_released(),
                i.pointer.any_down(),
                i.pointer.delta().y,
            )
        });

        if !at_top {
            self.gallery_pull = 0.0;
            self.gallery_pull_tracking = false;
            return false;
        }

        if pressed {
            self.gallery_pull_tracking = at_top;
            self.gallery_pull = 0.0;
        }

        if self.gallery_pull_tracking && down && at_top {
            // Finger down → positive dy; rubber-band so it takes a deliberate pull.
            if delta_y > 0.0 {
                self.gallery_pull = (self.gallery_pull + delta_y * 0.85).min(140.0);
            } else if delta_y < 0.0 {
                self.gallery_pull = (self.gallery_pull + delta_y).max(0.0);
            }
        }

        if released {
            let fire = self.gallery_pull_tracking
                && self.gallery_pull >= Self::GALLERY_PULL_THRESHOLD
                && !self.gallery_loading;
            self.gallery_pull = 0.0;
            self.gallery_pull_tracking = false;
            return fire;
        }

        if !down {
            self.gallery_pull = 0.0;
            self.gallery_pull_tracking = false;
        }
        false
    }

    /// The `(subfolder, filename)` pairs of the currently multi-selected images.
    fn selected_items(&self) -> Vec<(String, String)> {
        self.gallery
            .iter()
            .filter(|it| self.selected.contains(&it.key()))
            .map(|it| (it.subfolder.clone(), it.filename.clone()))
            .collect()
    }

    /// Actions on the multi-selection, shown in the bottom bar while selecting.
    fn selection_bar(&mut self, ui: &mut egui::Ui, host: &Host) {
        let items = self.selected_items();
        let n = items.len();
        let mut add_to: Option<i64> = None;
        let mut create_album = false;
        let mut delete = false;
        let mut clear = false;
        let mut done = false;
        let mut save_all = false;
        let mut select_all = false;
        let mut invert = false;
        const ICON: f32 = 36.0;
        ui.horizontal(|ui| {
            ui.strong(format!("{n}"));
            if ui.small_button("All").on_hover_text("Select every visible image").clicked() {
                select_all = true;
            }
            if ui.small_button("Inv").on_hover_text("Flip the current selection").clicked() {
                invert = true;
            }
            ui.add_enabled_ui(n > 0, |ui| {
                let album_label = format!("{}{}", icons::ALBUM, icons::ADD);
                up_menu_sized(ui, album_label, egui::vec2(ICON + 8.0, ICON), |ui| {
                    if ui
                        .button(format!("{} New album…", icons::ADD))
                        .on_hover_text("Create an album and add the selection")
                        .clicked()
                    {
                        create_album = true;
                        ui.close();
                    }
                    ui.separator();
                    if self.albums.is_empty() {
                        ui.weak("No albums yet.");
                    }
                    for a in &self.albums {
                        let label = format!("{} {}", icons::ALBUM, elide(&a.name, 28));
                        if ui.selectable_label(false, label).clicked() {
                            add_to = Some(a.id);
                        }
                    }
                });
                if ui
                    .add(egui::Button::new(icons::SAVE).min_size(egui::vec2(ICON, ICON)))
                    .on_hover_text("Save to Photos")
                    .clicked()
                {
                    save_all = true;
                }
                if ui
                    .add(egui::Button::new(icons::TRASH).min_size(egui::vec2(ICON, ICON)))
                    .on_hover_text("Delete selected")
                    .clicked()
                {
                    delete = true;
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(icons::CHECK).min_size(egui::vec2(ICON, ICON)))
                    .on_hover_text("Done")
                    .clicked()
                {
                    done = true;
                }
                if ui
                    .add(egui::Button::new(icons::CLOSE).min_size(egui::vec2(ICON, ICON)))
                    .on_hover_text("Clear selection")
                    .clicked()
                {
                    clear = true;
                }
            });
        });
        if select_all {
            let media = self.gallery_view.media;
            for it in &self.gallery {
                if media.matches(it.is_video) {
                    self.selected.insert(it.key());
                }
            }
        } else if invert {
            let media = self.gallery_view.media;
            for it in &self.gallery {
                if !media.matches(it.is_video) {
                    continue;
                }
                let key = it.key();
                if !self.selected.remove(&key) {
                    self.selected.insert(key);
                }
            }
        } else if save_all {
            self.engine.as_ref().unwrap().download_for_save(items.clone());
            self.gallery_status = format!("Saving {n} to Photos…");
            host.haptic(Haptic::Light);
        } else if create_album {
            self.album_new_name.clear();
            self.album_create_draft = Some(items);
        } else if let Some(id) = add_to {
            self.engine.as_ref().unwrap().album_add(id, items.clone());
            self.selected.clear();
            self.select_mode = false;
            host.haptic(Haptic::Light);
        } else if delete {
            self.request_delete_images(items, false);
            host.haptic(Haptic::Warning);
        } else if clear {
            self.selected.clear();
        } else if done {
            self.exit_select_mode();
        }
    }

    fn exit_select_mode(&mut self) {
        self.select_mode = false;
        self.selected.clear();
        self.sel_painting = false;
    }

    /// Queue a gallery delete, optionally after a confirmation dialog.
    fn request_delete_images(&mut self, items: Vec<(String, String)>, close_viewer: bool) {
        if items.is_empty() {
            return;
        }
        self.delete_closes_viewer = close_viewer;
        if self.confirm_gallery_delete {
            self.delete_confirm = Some((items, false));
        } else {
            self.engine.as_ref().unwrap().delete_images(items);
        }
    }

    /// Create-album dialog opened from the add-to-album picker (keeps the current selection).
    fn album_create_window(&mut self, ctx: &egui::Context) {
        let Some(items) = self.album_create_draft.clone() else {
            return;
        };
        let mut open = true;
        let mut create = false;
        let mut cancel = false;
        centered(egui::Window::new("New album"))
            .collapsible(false)
            .open(&mut open)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.weak(format!("Add {} selected image(s) after creating.", items.len()));
                ui.add(
                    egui::TextEdit::singleline(&mut self.album_new_name)
                        .hint_text("album name")
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    let named = !self.album_new_name.trim().is_empty();
                    if ui
                        .add_enabled(named, egui::Button::new(format!("{} Create", icons::ADD)))
                        .clicked()
                    {
                        create = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if !open || cancel {
            self.album_create_draft = None;
            return;
        }
        if create {
            let name = self.album_new_name.trim().to_string();
            self.engine.as_ref().unwrap().album_create(name.clone());
            self.album_pending_add = Some((name, items));
            self.album_new_name.clear();
            self.album_create_draft = None;
        }
    }

    /// Delete confirmation with optional "never show again".
    fn delete_confirm_window(&mut self, ctx: &egui::Context, host: &Host) {
        let Some((items, mut never)) = self.delete_confirm.clone() else {
            return;
        };
        let n = items.len();
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        centered(egui::Window::new("Delete images?"))
            .collapsible(false)
            .open(&mut open)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label(if n == 1 {
                    "Move this image to the server trash? You can restore it later.".into()
                } else {
                    format!("Move {n} images to the server trash? You can restore them later.")
                });
                ui.add_space(6.0);
                ui.checkbox(&mut never, "Don't ask again");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(format!("{} Delete", icons::TRASH)))
                        .clicked()
                    {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if let Some((_, nref)) = self.delete_confirm.as_mut() {
            *nref = never;
        }
        if !open || cancel {
            self.delete_confirm = None;
            self.delete_closes_viewer = false;
            return;
        }
        if confirm {
            if never {
                self.confirm_gallery_delete = false;
            }
            self.delete_confirm = None;
            self.engine.as_ref().unwrap().delete_images(items);
            host.haptic(Haptic::Warning);
        }
    }

    /// Long-press-then-drag paint selection over the gallery grid.
    ///
    /// A finger held ~0.4s still on a tile enters select mode and starts painting; dragging without
    /// lifting then selects every tile it passes over (scroll is suppressed for that gesture). A
    /// drag that moves before the hold completes is a normal scroll and never paints.
    fn handle_gallery_gesture(&mut self, ui: &egui::Ui, host: &Host) {
        let (down, pos, time) =
            ui.input(|i| (i.pointer.any_down(), i.pointer.interact_pos(), i.time));
        if !down {
            self.sel_press = None;
            self.sel_long_fired = false;
            self.sel_painting = false;
            return;
        }
        let Some(pos) = pos else { return };
        let tile_at = |p: egui::Pos2, hits: &[(egui::Rect, usize)]| {
            hits.iter().find(|(r, _)| r.contains(p)).map(|(_, i)| *i)
        };
        match self.sel_press {
            None => self.sel_press = Some((time, pos, false)),
            Some((start, origin, cancelled)) => {
                if !cancelled && !self.sel_painting {
                    if (origin - pos).length() > 18.0 {
                        // Moved before the hold completed: it's a scroll, not a selection.
                        self.sel_press = Some((start, origin, true));
                    } else if time - start > 0.4 {
                        if let Some(idx) = tile_at(origin, &self.tile_hits) {
                            self.select_mode = true;
                            self.sel_long_fired = true;
                            self.sel_painting = true;
                            if let Some(item) = self.gallery.get(idx) {
                                self.selected.insert(item.key());
                            }
                            host.haptic(Haptic::Medium);
                        } else {
                            self.sel_press = Some((start, origin, true));
                        }
                    } else {
                        // Still waiting for the hold; keep the clock running.
                        ui.ctx().request_repaint();
                    }
                }
            }
        }
        if self.sel_painting
            && let Some(idx) = tile_at(pos, &self.tile_hits)
            && let Some(item) = self.gallery.get(idx)
        {
            self.selected.insert(item.key());
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
        let select_mode = self.select_mode;
        // A click that ends a long-press IS the select gesture, not a tap — don't also toggle/open.
        let suppress_click = self.sel_long_fired;

        for row in indices.chunks(cols) {
            ui.horizontal(|ui| {
                for &idx in row {
                    let (item_key, thumb_key, subfolder, filename, is_video) = {
                        let Some(item) = self.gallery.get(idx) else { continue };
                        (item.key(), item.thumb_key(size), item.subfolder.clone(), item.filename.clone(), item.is_video)
                    };
                    // Prefer cached aspect so 1-column rows keep a stable height while thumbs load.
                    let aspect = self.thumb_aspects.get(&item_key).copied().or_else(|| {
                        self.thumbs.get(&thumb_key).map(|t| t.size_vec2()).and_then(|s| {
                            (s.x > 0.0).then_some(s.y / s.x)
                        })
                    });
                    let alloc = match (cols, aspect) {
                        (1, Some(a)) => egui::vec2(tile, tile * a),
                        _ => egui::vec2(tile, tile),
                    };
                    let (rect, _) = ui.allocate_exact_size(alloc, egui::Sense::hover());
                    // Off-screen tiles keep their space but skip paint + fetch.
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    self.tile_hits.push((rect, idx));
                    let selected = self.selected.contains(&item_key);
                    let clicked = match self.thumbs.get(&thumb_key) {
                        Some(tex) => {
                            let img = egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                                .fit_to_exact_size(alloc)
                                .sense(egui::Sense::click());
                            ui.put(rect, img).clicked()
                        }
                        None => {
                            if self.thumbs.claim(&thumb_key) {
                                self.engine.as_ref().unwrap().fetch_thumb(subfolder, filename, size);
                            }
                            ui.put(rect, egui::Button::new(elide(&item_key, 14)).wrap()).clicked()
                        }
                    };
                    // Videos (which the server may not thumbnail) get a play badge so they're
                    // recognizable even as a blank tile.
                    if is_video {
                        video_badge(ui, rect);
                    }
                    if select_mode {
                        selection_overlay(ui, rect, selected);
                    }
                    if clicked && !suppress_click {
                        if select_mode {
                            if selected {
                                self.selected.remove(&item_key);
                            } else {
                                self.selected.insert(item_key);
                            }
                        } else {
                            open = Some(idx);
                        }
                    }
                }
            });
        }
        open
    }

    fn open_viewer(&mut self, idx: usize, host: &Host) {
        let Some(item) = self.gallery.get(idx).cloned() else { return };
        // Any previous item's playback ends here (drop stops the decode thread).
        self.player = None;
        let engine = self.engine.as_ref().unwrap();
        let cache_dir = host.documents_dir();
        // Videos download the raw file — the poster shows immediately, Save works, and playback
        // starts once the bytes land (Msg::VideoReady). Images decode as usual (disk-cached).
        if item.is_video {
            engine.fetch_video(item.subfolder.clone(), item.filename.clone());
        } else {
            engine.fetch_full(item.subfolder.clone(), item.filename.clone(), cache_dir);
        }
        engine.fetch_item_albums(item.subfolder.clone(), item.filename.clone());
        // Always try the workflow endpoint — list `has_workflow` is often missing/false even when
        // the PNG embeds a graph (models still appear because the indexer scraped them separately).
        engine.fetch_item_workflow(item.subfolder.clone(), item.filename.clone());
        self.gallery_status.clear();
        self.filmstrip_center = true;
        self.viewer_swipe_origin = None;
        self.viewer = Some(Viewer {
            item,
            idx,
            tex: None,
            bytes: None,
            loading: true,
            albums: None,
            meta_open: false,
            workflow_json: None,
            meta: None,
            meta_loading: true,
        });
    }

    /// Next/previous gallery index matching the media filter, or `None` at the ends.
    fn gallery_neighbor(&self, from: usize, dir: i32) -> Option<usize> {
        let media = self.gallery_view.media;
        let mut i = from as i32;
        loop {
            i += dir;
            if i < 0 || i >= self.gallery.len() as i32 {
                return None;
            }
            let idx = i as usize;
            if media.matches(self.gallery[idx].is_video) {
                return Some(idx);
            }
        }
    }

    /// Horizontal swipe over `rect`: `1` = next, `-1` = previous. Vertical-dominant drags ignored.
    ///
    /// egui clears `press_origin` on the release frame, so the press is tracked in
    /// [`Self::viewer_swipe_origin`].
    fn viewer_horizontal_swipe(&mut self, ui: &egui::Ui, rect: egui::Rect) -> Option<i32> {
        let (pressed, released, down, pos) = ui.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.any_released(),
                i.pointer.any_down(),
                i.pointer.latest_pos().or(i.pointer.interact_pos()),
            )
        });
        if pressed {
            self.viewer_swipe_origin = pos.filter(|p| rect.contains(*p));
        }
        if released {
            let origin = self.viewer_swipe_origin.take()?;
            let pos = pos?;
            let d = pos - origin;
            if d.x.abs() > 56.0 && d.x.abs() > d.y.abs() * 1.25 {
                return Some(if d.x < 0.0 { 1 } else { -1 });
            }
            return None;
        }
        if !down {
            self.viewer_swipe_origin = None;
        }
        None
    }

    fn gallery_viewer(&mut self, ui: &mut egui::Ui, host: &Host) {
        enum Act {
            Close,
            Save,
            UseAsInput,
            OpenWorkflow,
            CopyWorkflow,
            ToggleMeta,
            AlbumAdd(i64),
            AlbumRemove(i64),
            AlbumCreate,
            Delete,
            Show(usize),
        }
        let mut act: Option<Act> = None;
        // Android system Back / Esc returns to the gallery list.
        if ui.ctx().input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
        }) {
            act = Some(Act::Close);
        }
        // Move decoded frames into the texture before anything samples it this frame.
        if let Some(p) = &mut self.player {
            p.pump(ui.ctx());
        }
        let meta_anchor;
        {
            let v = self.viewer.as_ref().unwrap();
            // Filename + copy (icon) at the top; actions live in the bottom bar.
            let header = ui.horizontal(|ui| {
                let chevron = if v.meta_open { "▼" } else { "▶" };
                let title = format!(
                    "{chevron} {}  ({:.1} MB)",
                    elide(&v.item.filename, 48),
                    v.item.size as f64 / 1e6
                );
                if ui
                    .add(egui::Button::new(title).frame(false))
                    .on_hover_text("Show generation metadata")
                    .clicked()
                {
                    act = Some(Act::ToggleMeta);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            v.workflow_json.is_some(),
                            egui::Button::new(icons::PROPS).min_size(egui::vec2(36.0, 32.0)),
                        )
                        .on_hover_text("Copy embedded workflow JSON")
                        .clicked()
                    {
                        act = Some(Act::CopyWorkflow);
                    }
                });
            });
            meta_anchor = header.response.rect;
            if v.item.is_video {
                match self.player.as_mut().filter(|p| p.key == v.item.key()) {
                    Some(p) if p.failed.is_some() => {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 160, 120),
                            format!(
                                "{} {} — Save still works",
                                icons::WARN,
                                p.failed.as_deref().unwrap_or("playback failed")
                            ),
                        );
                    }
                    Some(p) if p.frame_count > 0 => {
                        ui.horizontal(|ui| {
                            let paused = p.ctrl.paused.load(Ordering::Relaxed);
                            let label = if paused { icons::RUN } else { "⏸" };
                            if ui.button(label).clicked() {
                                // Play at the end of a non-looping video restarts it — otherwise
                                // the loop would decode the last frame and immediately re-pause.
                                if paused && p.cur >= p.frame_count - 1 {
                                    p.cur = 0;
                                    p.ctrl.seek.store(0, Ordering::Relaxed);
                                }
                                p.ctrl.paused.store(!paused, Ordering::Relaxed);
                                p.auto_paused = false;
                            }
                            let mut looping = p.ctrl.looping.load(Ordering::Relaxed);
                            if ui.toggle_value(&mut looping, "🔁").on_hover_text("Loop").changed()
                            {
                                p.ctrl.looping.store(looping, Ordering::Relaxed);
                            }
                            let secs = |f: i32| f as f32 / p.fps.max(1.0);
                            ui.weak(format!(
                                "{:>5.1}s / {:.1}s",
                                secs(p.cur),
                                p.duration_ms as f32 / 1000.0
                            ));
                            let mut pos = p.cur;
                            let max = (p.frame_count - 1).max(0);
                            let slider = ui.add(
                                egui::Slider::new(&mut pos, 0..=max)
                                    .show_value(false)
                                    .trailing_fill(true),
                            );
                            if slider.changed() {
                                p.cur = pos;
                                p.ctrl.seek.store(pos, Ordering::Relaxed);
                                // Flush frames queued before the seek so the image and slider
                                // don't briefly snap back to pre-seek positions.
                                while p.rx.try_recv().is_ok() {}
                            }
                        });
                    }
                    Some(_) => {
                        ui.weak(format!("{} opening video…", icons::RUN));
                    }
                    None => {
                        ui.weak(format!("{} downloading video…", icons::RUN));
                    }
                }
            }
            if !self.gallery_status.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 160, 120),
                    elide(&self.gallery_status, 120),
                );
            }
            ui.separator();

            // Bottom: action bar (lowest) then filmstrip, so thumbs sit above the buttons.
            let can_save = v.bytes.is_some();
            let can_open_wf = v.item.has_workflow || v.workflow_json.is_some();
            let albums_known = v.albums.is_some();
            egui::Panel::bottom("viewer-actions").show(ui, |ui| {
                const BTN_H: f32 = 36.0;
                const ICON_W: f32 = 40.0;
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add(egui::Button::new(icons::BACK).min_size(egui::vec2(ICON_W, BTN_H)))
                        .on_hover_text("Back to gallery")
                        .clicked()
                    {
                        act = Some(Act::Close);
                    }
                    if ui
                        .add_enabled(
                            can_save,
                            egui::Button::new(icons::SAVE).min_size(egui::vec2(ICON_W, BTN_H)),
                        )
                        .on_hover_text("Save to device")
                        .clicked()
                    {
                        act = Some(Act::Save);
                    }
                    if ui
                        .add(
                            egui::Button::new(format!("{} Use", icons::IMAGE))
                                .min_size(egui::vec2(0.0, BTN_H)),
                        )
                        .on_hover_text("Use as img2img input")
                        .clicked()
                    {
                        act = Some(Act::UseAsInput);
                    }
                    if ui
                        .add_enabled(
                            can_open_wf,
                            egui::Button::new(format!("{} Workflow", icons::GRAPH))
                                .min_size(egui::vec2(0.0, BTN_H)),
                        )
                        .on_hover_text("Open embedded workflow")
                        .clicked()
                    {
                        act = Some(Act::OpenWorkflow);
                    }
                    // Opens upward so the list clears the Android nav / gesture bar.
                    let album_label = format!("{}{}", icons::ALBUM, icons::ADD);
                    up_menu_sized(ui, album_label, egui::vec2(ICON_W + 8.0, BTN_H), |ui| {
                        if ui
                            .button(format!("{} New album…", icons::ADD))
                            .on_hover_text("Create an album and add this image")
                            .clicked()
                        {
                            act = Some(Act::AlbumCreate);
                            ui.close();
                        }
                        ui.separator();
                        if !albums_known {
                            ui.weak("loading…");
                            return;
                        }
                        if self.albums.is_empty() {
                            ui.weak("No albums yet.");
                            return;
                        }
                        let member = self.viewer.as_ref().unwrap().albums.as_ref().unwrap();
                        for a in &self.albums {
                            let is_in = member.contains(&a.id);
                            let label = if is_in {
                                format!("{} {}", icons::CHECK, elide(&a.name, 28))
                            } else {
                                format!("     {}", elide(&a.name, 28))
                            };
                            if ui.selectable_label(is_in, label).clicked() {
                                act = Some(if is_in {
                                    Act::AlbumRemove(a.id)
                                } else {
                                    Act::AlbumAdd(a.id)
                                });
                                ui.close();
                            }
                        }
                    });
                    if ui
                        .add(egui::Button::new(icons::TRASH).min_size(egui::vec2(ICON_W, BTN_H)))
                        .on_hover_text("Delete image")
                        .clicked()
                    {
                        act = Some(Act::Delete);
                    }
                });
                ui.add_space(2.0);
            });
            act = self.filmstrip(ui).map(Act::Show).or(act);

            let v = self.viewer.as_ref().unwrap();
            if v.loading {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| ui.spinner());
            }
            // Live video frame first, then the decoded image, then any cached thumbnail so
            // something shows while the full read lands.
            let video_tex = self
                .player
                .as_ref()
                .filter(|p| p.key == v.item.key())
                .and_then(|p| p.tex.as_ref());
            let cached = [1024u32, 512, 320]
                .iter()
                .find_map(|s| self.thumbs.get(&v.item.thumb_key(*s)));
            let image_rect = ui.available_rect_before_wrap();
            if let Some(tex) = video_tex.or(v.tex.as_ref()).or(cached) {
                // Fit inside the slot left by header / filmstrip / actions — no ScrollArea, so
                // aspect-fit never leaves a one-pixel vertical scroll under the carousel.
                let avail = image_rect.size().max(egui::vec2(1.0, 1.0));
                let sized = egui::load::SizedTexture::from_handle(tex);
                ui.scope_builder(egui::UiBuilder::new().max_rect(image_rect), |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.add(
                            egui::Image::new(sized)
                                .max_size(avail)
                                .maintain_aspect_ratio(true),
                        );
                    });
                });
            }
            // Horizontal swipe changes the picture (dominant X drag from the image area).
            if act.is_none()
                && let Some(dir) = self.viewer_horizontal_swipe(ui, image_rect)
            {
                let cur = self.viewer.as_ref().unwrap().idx;
                if let Some(n) = self.gallery_neighbor(cur, dir) {
                    act = Some(Act::Show(n));
                }
            }
        }
        // Expanded metadata floats over the image so the layout below does not shift.
        if self.viewer.as_ref().is_some_and(|v| v.meta_open) {
            self.viewer_meta_overlay(ui.ctx(), host, meta_anchor);
        }
        match act {
            Some(Act::Close) => {
                self.gallery_scroll_restore = Some(self.gallery_scroll_y);
                self.viewer = None;
                self.player = None;
                self.viewer_swipe_origin = None;
                self.gallery_status.clear();
            }
            Some(Act::Show(idx)) => self.open_viewer(idx, host),
            Some(Act::ToggleMeta) => {
                if let Some(v) = &mut self.viewer {
                    v.meta_open = !v.meta_open;
                }
            }
            Some(Act::CopyWorkflow) => {
                if let Some(json) = self.viewer.as_ref().and_then(|v| v.workflow_json.clone()) {
                    self.workflow_clip = Some(json.clone());
                    host.copy_text(json);
                    host.haptic(Haptic::Light);
                    self.gallery_status = "Workflow copied".into();
                }
            }
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
            Some(Act::AlbumCreate) => {
                let v = self.viewer.as_ref().unwrap();
                self.album_new_name.clear();
                self.album_create_draft =
                    Some(vec![(v.item.subfolder.clone(), v.item.filename.clone())]);
            }
            Some(Act::Delete) => {
                let v = self.viewer.as_ref().unwrap();
                let items = vec![(v.item.subfolder.clone(), v.item.filename.clone())];
                self.request_delete_images(items, true);
                host.haptic(Haptic::Warning);
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
                    self.graph_status.clear();
                    self.wf_loading = true;
                    // Prefer the already-fetched body so we don't wait on a second download.
                    if let Some(body) = v.workflow_json.clone() {
                        self.engine.as_ref().unwrap().load_workflow_json(
                            v.item.filename.clone(),
                            body,
                            schemas,
                        );
                    } else {
                        self.engine.as_ref().unwrap().open_gallery_workflow(
                            v.item.subfolder.clone(),
                            v.item.filename.clone(),
                            schemas,
                        );
                    }
                    self.tab = Tab::Graph;
                }
            }
            None => {}
        }
    }

    /// Floating metadata panel anchored under the filename header, painted over the image.
    fn viewer_meta_overlay(&mut self, ctx: &egui::Context, host: &Host, anchor: egui::Rect) {
        let Some(v) = self.viewer.as_ref() else { return };
        let screen = ctx.content_rect();
        let margin = 8.0;
        let left = anchor.left().clamp(screen.left() + margin, screen.right() - 180.0);
        // Stay inside the screen so the popup frame/margins are not clipped on the right.
        let width = (screen.right() - margin - left).max(160.0);
        let meta_loading = v.meta_loading;
        let has_workflow = v.item.has_workflow;
        let item_models = v.item.models.clone();
        let meta = v.meta.clone();
        let mut copy_positive: Option<String> = None;
        let mut copy_negative: Option<String> = None;
        let mut copy_sampler = false;
        let mut copy_loras = false;
        // Fixed column so every section's copy button stacks on the same left edge.
        const COPY_W: f32 = 28.0;
        let meta_section = |ui: &mut egui::Ui,
                            title: &str,
                            hover: &str,
                            copy: &mut bool| {
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(COPY_W, ui.spacing().interact_size.y),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        if ui.small_button(icons::PROPS).on_hover_text(hover).clicked() {
                            *copy = true;
                        }
                    },
                );
                ui.strong(title);
            });
        };
        let area = egui::Area::new(egui::Id::new("viewer-meta-overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(left, anchor.bottom() + 2.0))
            .constrain_to(screen.shrink(margin))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.set_width(width);
                        ui.set_max_width(width);
                        ui.set_max_height((screen.height() * 0.55).clamp(180.0, 360.0));
                        crate::theme::scroll_vertical().show(ui, |ui| {
                            ui.set_max_width((width - 20.0).max(120.0));
                            if meta_loading {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.weak("loading workflow…");
                                });
                                return;
                            }
                            let models = meta
                                .as_ref()
                                .map(|m| m.models.as_slice())
                                .filter(|m| !m.is_empty())
                                .unwrap_or(item_models.as_slice());
                            if !models.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.add_space(COPY_W);
                                    ui.label(format!(
                                        "{} {}",
                                        icons::MODEL,
                                        elide(&models.join(", "), 120)
                                    ));
                                });
                            }
                            if let Some(meta) = meta.as_ref() {
                                if !meta.loras.is_empty() {
                                    ui.add_space(4.0);
                                    meta_section(
                                        ui,
                                        "LoRAs",
                                        "Copy LoRAs + strengths for Create",
                                        &mut copy_loras,
                                    );
                                    for l in &meta.loras {
                                        let clip = l
                                            .strength_clip
                                            .map(|c| format!(" / clip {c:.2}"))
                                            .unwrap_or_default();
                                        ui.horizontal(|ui| {
                                            ui.add_space(COPY_W);
                                            ui.label(format!(
                                                "{} {}  (model {:.2}{clip})",
                                                icons::DOT,
                                                elide(&l.name, 64),
                                                l.strength_model
                                            ));
                                        });
                                    }
                                }
                                if let Some(p) = meta.positive.as_deref().filter(|s| !s.is_empty()) {
                                    ui.add_space(4.0);
                                    let mut go = false;
                                    meta_section(ui, "Positive", "Copy positive prompt", &mut go);
                                    if go {
                                        copy_positive = Some(p.to_string());
                                    }
                                    ui.horizontal(|ui| {
                                        ui.add_space(COPY_W);
                                        ui.add(egui::Label::new(p).wrap());
                                    });
                                }
                                if let Some(n) = meta.negative.as_deref().filter(|s| !s.is_empty()) {
                                    ui.add_space(4.0);
                                    let mut go = false;
                                    meta_section(ui, "Negative", "Copy negative prompt", &mut go);
                                    if go {
                                        copy_negative = Some(n.to_string());
                                    }
                                    ui.horizontal(|ui| {
                                        ui.add_space(COPY_W);
                                        ui.add(egui::Label::new(n).wrap());
                                    });
                                }
                                let mut bits = Vec::new();
                                if let Some(s) = &meta.sampler {
                                    bits.push(s.clone());
                                }
                                if let Some(s) = &meta.scheduler {
                                    bits.push(s.clone());
                                }
                                if let Some(n) = meta.steps {
                                    bits.push(format!("{n} steps"));
                                }
                                if let Some(c) = meta.cfg {
                                    bits.push(format!("cfg {c:.1}"));
                                }
                                if let Some(seed) = meta.seed {
                                    bits.push(format!("seed {seed}"));
                                }
                                if !bits.is_empty() {
                                    ui.add_space(4.0);
                                    meta_section(
                                        ui,
                                        "Sampler",
                                        "Copy sampler / scheduler / steps / CFG for Create",
                                        &mut copy_sampler,
                                    );
                                    ui.horizontal(|ui| {
                                        ui.add_space(COPY_W);
                                        ui.weak(bits.join(" · "));
                                    });
                                }
                                if meta.is_empty() && models.is_empty() {
                                    ui.weak("No prompt metadata in this workflow.");
                                }
                            } else if !has_workflow {
                                ui.weak("No embedded workflow on this file.");
                            } else {
                                ui.weak("Could not load workflow metadata.");
                            }
                        });
                    });
            });
        // Tap outside the panel (and outside the header toggle) closes it.
        if ctx.input(|i| i.pointer.any_click())
            && let Some(pos) = ctx.pointer_interact_pos()
            && !area.response.rect.contains(pos)
            && !anchor.contains(pos)
        {
            if let Some(v) = &mut self.viewer {
                v.meta_open = false;
            }
        }
        if copy_sampler {
            if let Some(meta) = self.viewer.as_ref().and_then(|v| v.meta.clone()) {
                self.copy_sampler_pack_from_meta(&meta, host);
            }
        } else if copy_loras {
            if let Some(meta) = self.viewer.as_ref().and_then(|v| v.meta.clone()) {
                self.copy_lora_pack_from_meta(&meta, host);
            }
        } else if let Some(text) = copy_positive {
            host.copy_text(text);
            host.haptic(Haptic::Light);
            self.gallery_status = "Positive prompt copied".into();
        } else if let Some(text) = copy_negative {
            host.copy_text(text);
            host.haptic(Haptic::Light);
            self.gallery_status = "Negative prompt copied".into();
        }
    }

    /// Horizontal strip of the listing along the bottom of the viewer. Returns the index of any
    /// tapped frame. Frames always request a small thumb so a 1-column open doesn't pull 4 MB each.
    fn filmstrip(&mut self, ui: &mut egui::Ui) -> Option<usize> {
        const FRAME: f32 = 64.0;
        let current = self.viewer.as_ref().map(|v| v.idx);
        let center = self.filmstrip_center;
        let mut picked = None;
        let mut centered = false;
        egui::Panel::bottom("filmstrip")
            .exact_size(FRAME + 12.0)
            .show(ui, |ui| {
                crate::theme::scroll_horizontal().id_salt("viewer_filmstrip").auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        ui.horizontal(|ui| {
                            for idx in 0..self.gallery.len() {
                                let Some(item) = self.gallery.get(idx) else { continue };
                                // Keep the strip consistent with the grid's media filter.
                                if !self.gallery_view.media.matches(item.is_video) {
                                    continue;
                                }
                                let key = item.thumb_key(320);
                                let size = egui::vec2(FRAME, FRAME);
                                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                                let is_current = current == Some(idx);
                                // Scroll the opened / swiped frame to the middle of the strip.
                                if center && is_current {
                                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                    centered = true;
                                }
                                if !ui.is_rect_visible(rect) && !(center && is_current) {
                                    continue;
                                }
                                match self.thumbs.get(&key) {
                                    Some(tex) => {
                                        let img = egui::Image::new(
                                            egui::load::SizedTexture::from_handle(tex),
                                        )
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
                                if is_current {
                                    ui.painter().rect_stroke(
                                        rect,
                                        2.0,
                                        egui::Stroke::new(
                                            2.0,
                                            egui::Color32::from_rgb(110, 170, 255),
                                        ),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                            }
                        });
                    },
                );
            });
        if centered {
            self.filmstrip_center = false;
        }
        picked
    }

    fn logs_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        egui::Panel::bottom("logs-actions").show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
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
            ui.add_space(2.0);
        });

        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        crate::theme::scroll_both()
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
            crate::theme::apply_fonts(ui.ctx(), &self.fonts);
            if !self.server_url.trim().is_empty() {
                self.log.info("auto-connecting to saved server");
                self.connect(host);
            }
        }

        for m in self.engine.as_ref().unwrap().drain() {
            self.handle(ui.ctx(), host, m);
        }
        let now = ui.ctx().input(|i| i.time);
        self.sync_create_graph_link(now);
        // Don't burn CPU decoding video nobody can see: pause while the viewer is off-screen and
        // resume where it left off on return (unless the user paused it themselves).
        if let Some(p) = &mut self.player {
            let visible = self.tab == Tab::Gallery && self.viewer.is_some();
            if !visible {
                if !p.ctrl.paused.swap(true, Ordering::Relaxed) {
                    p.auto_paused = true;
                }
            } else if p.auto_paused {
                p.ctrl.paused.store(false, Ordering::Relaxed);
                p.auto_paused = false;
            }
        }
        self.log_lines.extend(self.log.take_new(&mut self.log_cursor));
        if self.log_lines.len() > logger::MAX_LINES {
            let excess = self.log_lines.len() - logger::MAX_LINES;
            self.log_lines.drain(..excess);
        }
        self.autosave_settings(ui.ctx(), host);

        // Second gallery refresh after generate — server index often lags the write.
        if let Some(at) = self.gallery_refresh_at {
            let now = ui.ctx().input(|i| i.time);
            if now >= at {
                self.gallery_refresh_at = None;
                if matches!(self.conn, Conn::Connected) {
                    self.refresh_gallery();
                }
            } else {
                ui.ctx().request_repaint_after(Duration::from_secs_f64((at - now).max(0.05)));
            }
        }

        // Navigation sits at the bottom, within thumb reach. Panels are laid out before the
        // central content so the tab bar always keeps its height on a short screen.
        egui::Panel::bottom("nav").show(ui, |ui| {
            ui.add_space(2.0);
            // Global run progress (local jobs and server-wide queue from other clients).
            if self.running || self.queue_remaining > 0 {
                let (v, m) = self.progress;
                let (frac, label) = if m > 0 {
                    (v as f32 / m as f32, format!("{} {v}/{m}", elide(&self.status, 40)))
                } else if self.running && self.run_total > 0 {
                    let done = self.run_seen.len().saturating_sub(1).min(self.run_total);
                    (
                        done as f32 / self.run_total as f32,
                        format!(
                            "node {} of {}",
                            self.run_seen.len().min(self.run_total),
                            self.run_total
                        ),
                    )
                } else if self.queue_remaining > 0 {
                    (
                        0.0,
                        format!(
                            "{} · {} in queue",
                            elide(&self.status, 32),
                            self.queue_remaining
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
            });

        self.app_picker_window(ui.ctx(), host);
        self.publish_window(ui.ctx(), host);

        // Keep the server-wide queue in view even when jobs were started on the website.
        if matches!(self.conn, Conn::Connected) {
            let now = ui.ctx().input(|i| i.time);
            if now - self.last_queue_poll > 2.5 {
                self.last_queue_poll = now;
                self.engine.as_ref().unwrap().poll_queue();
            }
        }

        if self.running || self.queue_remaining > 0 {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
    }
}

impl ComfyApp {
    /// Create/Graph labeled on the left; Gallery/Settings as a tight icon cluster.
    fn nav_bar(&mut self, ui: &mut egui::Ui) {
        const ROW_H: f32 = 32.0;
        const ICON_BTN: f32 = 40.0;
        const ICON_GAP: f32 = 2.0;
        let labeled_n = Tab::BAR.iter().filter(|(_, _, l)| !l.is_empty()).count().max(1);
        let icon_n = Tab::BAR.iter().filter(|(_, _, l)| l.is_empty()).count() as f32;
        let icon_cluster_w = icon_n * ICON_BTN + (icon_n - 1.0).max(0.0) * ICON_GAP;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let labeled_w =
                ((ui.available_width() - icon_cluster_w - 6.0) / labeled_n as f32).max(72.0);

            for (tab, icon, label) in Tab::BAR.iter().filter(|(_, _, l)| !l.is_empty()) {
                let selected = self.tab == *tab;
                let text = egui::RichText::new(format!("{icon} {label}")).size(12.0);
                let btn = egui::Button::selectable(selected, text)
                    .wrap_mode(egui::TextWrapMode::Extend)
                    .min_size(egui::vec2(labeled_w, ROW_H));
                if ui.add(btn).clicked() {
                    self.tab = *tab;
                    if *tab == Tab::Graph {
                        ui.ctx().request_repaint();
                    }
                }
            }

            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = ICON_GAP;
                // Zero padding so the glyph centers in the fixed square hit target.
                ui.spacing_mut().button_padding = egui::vec2(0.0, 0.0);
                for (tab, icon, _) in Tab::BAR.iter().filter(|(_, _, l)| l.is_empty()) {
                    let selected = self.tab == *tab;
                    let text = egui::RichText::new(*icon).size(18.0);
                    let btn = egui::Button::selectable(selected, text)
                        .min_size(egui::vec2(ICON_BTN, ROW_H));
                    if ui.add(btn).clicked() {
                        self.tab = *tab;
                    }
                }
            });
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
    menu_popup(
        ui,
        label,
        None,
        egui::RectAlign::TOP_START,
        &[egui::RectAlign::TOP_END, egui::RectAlign::BOTTOM_START],
        content,
    );
}

/// [`up_menu`] with a fixed button size (viewer action icons).
fn up_menu_sized<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    min_size: egui::Vec2,
    content: impl FnOnce(&mut egui::Ui) -> R,
) {
    menu_popup(
        ui,
        label,
        Some(min_size),
        egui::RectAlign::TOP_START,
        &[egui::RectAlign::TOP_END, egui::RectAlign::BOTTOM_START],
        content,
    );
}

/// Header menu: popup opens below the button, right-aligned so it grows left.
fn down_menu<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    content: impl FnOnce(&mut egui::Ui) -> R,
) {
    menu_popup(
        ui,
        label,
        None,
        egui::RectAlign::BOTTOM_END,
        &[egui::RectAlign::BOTTOM_START, egui::RectAlign::TOP_END],
        content,
    );
}

fn menu_popup<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    min_size: Option<egui::Vec2>,
    align: egui::RectAlign,
    alternatives: &'static [egui::RectAlign],
    content: impl FnOnce(&mut egui::Ui) -> R,
) {
    use egui::containers::menu::MenuConfig;
    let mut btn = egui::Button::new(label.into());
    if let Some(size) = min_size {
        btn = btn.min_size(size);
    }
    let response = ui.add(btn);
    let config = MenuConfig::default();
    egui::Popup::menu(&response)
        .align(align)
        .align_alternatives(alternatives)
        .gap(4.0)
        .close_behavior(config.close_behavior)
        .style(config.style.clone())
        .info(
            egui::UiStackInfo::new(egui::UiKind::Menu)
                .with_tag_value(MenuConfig::MENU_CONFIG_TAG, config),
        )
        .show(|ui| {
            crate::theme::scroll_vertical()
                .max_height(320.0)
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    content(ui)
                })
                .inner
        });
}


/// Anchor a popup window to the center of the screen, above canvas overlays (minimap / FABs).
///
/// A top-anchored `egui::Window` can push its title bar above the app's content area — up under
/// the status-bar icons. Centering keeps every window fully inside the usable area, and it
/// re-centers above the keyboard when the content shrinks for the IME.
fn centered(window: egui::Window<'_>) -> egui::Window<'_> {
    window
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .order(egui::Order::Tooltip)
}

/// Draw a play-button badge centered on a tile, marking it as a video.
/// A centered, mostly transparent play button over a video tile: faint dark disc, thin ring, and
/// a soft-white triangle — enough to read as "playable" without hiding the thumbnail.
fn video_badge(ui: &egui::Ui, rect: egui::Rect) {
    let c = rect.center();
    let r = (rect.width().min(rect.height()) * 0.20).clamp(12.0, 40.0);
    let p = ui.painter();
    p.circle_filled(c, r, egui::Color32::from_black_alpha(70));
    p.circle_stroke(c, r, egui::Stroke::new(1.5, egui::Color32::from_white_alpha(110)));
    let t = r * 0.52;
    // Nudge right so the triangle's centroid sits on the disc's center.
    let tri = vec![
        c + egui::vec2(-t * 0.5, -t),
        c + egui::vec2(-t * 0.5, t),
        c + egui::vec2(t, 0.0),
    ];
    p.add(egui::Shape::convex_polygon(
        tri,
        egui::Color32::from_white_alpha(210),
        egui::Stroke::NONE,
    ));
}

/// Draw the multi-select overlay on a gallery tile: a tint plus a corner check badge.
fn selection_overlay(ui: &egui::Ui, rect: egui::Rect, selected: bool) {
    let p = ui.painter();
    let (tint, ring) = if selected {
        (egui::Color32::from_rgba_unmultiplied(120, 70, 150, 110), egui::Color32::from_rgb(180, 150, 230))
    } else {
        (egui::Color32::from_black_alpha(70), egui::Color32::from_gray(120))
    };
    p.rect_filled(rect, 3.0, tint);
    p.rect_stroke(rect, 3.0, egui::Stroke::new(2.0, ring), egui::StrokeKind::Inside);
    let center = rect.right_top() + egui::vec2(-14.0, 14.0);
    p.circle_filled(
        center,
        10.0,
        if selected { egui::Color32::from_rgb(150, 90, 190) } else { egui::Color32::from_black_alpha(130) },
    );
    p.circle_stroke(center, 10.0, egui::Stroke::new(1.0, egui::Color32::WHITE));
    if selected {
        p.text(
            center,
            egui::Align2::CENTER_CENTER,
            icons::CHECK,
            egui::FontId::proportional(12.0),
            egui::Color32::WHITE,
        );
    }
}

fn wrap_meta(ui: &mut egui::Ui, label: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    ui.add(egui::Label::new(egui::RichText::new(format!("{label}: {value}")).small()).wrap());
}

/// One model row: Use, pin, and expandable catalog metadata.
/// Buttons are placed first (RTL) so they keep the right edge; the label elides into what's left.
fn model_version_row(
    ui: &mut egui::Ui,
    file: &str,
    kind: ModelKind,
    meta: &Option<crate::types::CheckpointEntry>,
    current: &str,
    favorite: bool,
    salt: &str,
    pick: &mut Option<(String, ModelKind)>,
    toggle_fav: &mut Option<String>,
) {
    let selected = current == file;
    let mut ver = meta
        .as_ref()
        .map(|e| e.version_label())
        .unwrap_or_else(|| file_basename(file).to_string());
    if kind == ModelKind::Diffusion {
        ver.push_str(" • diffusion");
    }
    let ver_header = if selected {
        format!("{} {ver}", icons::CHECK)
    } else {
        ver
    };
    ui.horizontal(|ui| {
        // Nested collapsing indents shrink the row — never size past what's left.
        let row_w = ui.available_width();
        ui.set_max_width(row_w);
        let (use_clicked, star_clicked) = ui
            .with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let use_clicked = ui
                    .add_enabled(!selected, egui::Button::new("Use").small())
                    .clicked();
                let star_label = if favorite { icons::STAR } else { "+" };
                let star_clicked = ui
                    .small_button(star_label)
                    .on_hover_text(if favorite {
                        "Unpin favorite"
                    } else {
                        "Pin favorite"
                    })
                    .clicked();
                // Collapse arrow (~18px); keep the label clear of Use / pin.
                let max_w = (ui.available_width() - 22.0).max(32.0);
                let header = elide_width(ui, &sanitize_ui_text(ui, &ver_header), max_w);
                egui::CollapsingHeader::new(header)
                    .id_salt((salt, file))
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width().max(40.0));
                        checkpoint_meta_body(ui, file, meta.as_ref());
                    });
                (use_clicked, star_clicked)
            })
            .inner;
        if use_clicked {
            *pick = Some((file.to_string(), kind));
        }
        if star_clicked {
            *toggle_fav = Some(file.to_string());
        }
    });
}

/// Wrapped checkpoint catalog fields for a collapsing details body.
fn checkpoint_meta_body(
    ui: &mut egui::Ui,
    file: &str,
    entry: Option<&crate::types::CheckpointEntry>,
) {
    wrap_meta(ui, "File", file);
    let Some(e) = entry else {
        ui.weak("No catalog metadata for this checkpoint.");
        return;
    };
    if !e.name.trim().is_empty() && e.name != e.file {
        wrap_meta(ui, "Name", &e.name);
    }
    if let Some(v) = e.version.as_ref().filter(|s| !s.trim().is_empty()) {
        wrap_meta(ui, "Version", v);
    }
    if let Some(v) = e.base_model_type.as_ref().filter(|s| !s.trim().is_empty()) {
        wrap_meta(ui, "Base type", v);
    }
    if let Some(n) = e.size {
        wrap_meta(ui, "Size", &format_bytes(n));
    }
    if let Some(rec) = &e.recommended {
        let mut parts = Vec::new();
        if let Some(v) = rec.steps {
            parts.push(format!("steps {v}"));
        } else if let (Some(a), Some(b)) = (rec.steps_min, rec.steps_max) {
            parts.push(format!("steps {a}–{b}"));
        }
        if let Some(v) = rec.cfg {
            parts.push(format!("CFG {v}"));
        } else if let (Some(a), Some(b)) = (rec.cfg_min, rec.cfg_max) {
            parts.push(format!("CFG {a}–{b}"));
        }
        if let (Some(w), Some(h)) = (rec.width, rec.height) {
            parts.push(format!("{w}×{h}"));
        }
        if let Some(s) = rec.sampler.as_ref().filter(|s| !s.is_empty()) {
            parts.push(format!("sampler {s}"));
        }
        if let Some(s) = rec.scheduler.as_ref().filter(|s| !s.is_empty()) {
            parts.push(format!("scheduler {s}"));
        }
        if let Some(v) = rec.clip_skip {
            parts.push(format!("clip skip {v}"));
        }
        if !parts.is_empty() {
            wrap_meta(ui, "Recommended", &parts.join(" · "));
        }
    }
    wrap_meta(ui, "Notes", e.notes.trim());
    if let Some(d) = e.description.as_ref().filter(|s| !s.trim().is_empty()) {
        wrap_meta(ui, "Description", &strip_simple_html(d));
    }
    if !e.tags.is_empty() {
        wrap_meta(ui, "Tags", &e.tags.join(", "));
    }
}

fn format_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = n as f64;
    if n >= GB {
        format!("{:.2} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

fn strip_simple_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn preset_meta_body(ui: &mut egui::Ui, preset: &CreatePreset) {
    let p = &preset.params;
    wrap_meta(ui, "Model", p.model_file());
    wrap_meta(
        ui,
        "Mode",
        match p.mode {
            Mode::Txt2Img => "Text to Image",
            Mode::Img2Img => "Image to Image",
        },
    );
    wrap_meta(ui, "Prompt", &p.positive);
    if !p.lora_triggers.trim().is_empty() {
        wrap_meta(ui, "LoRA triggers", &p.lora_triggers);
    }
    wrap_meta(ui, "Negative", &p.negative);
    wrap_meta(
        ui,
        "Sampler",
        &format!(
            "{} steps, CFG {}, {}×{}, {}/{}",
            p.steps, p.cfg, p.width, p.height, p.sampler, p.scheduler
        ),
    );
    if !p.loras.is_empty() {
        let names: Vec<&str> = p.loras.iter().map(|l| l.file.as_str()).collect();
        wrap_meta(ui, "LoRAs", &names.join(", "));
    }
}

/// Wrapped LoRA catalog fields for a collapsing details body.
fn lora_meta_body(ui: &mut egui::Ui, file: &str, entry: Option<&crate::types::LoraEntry>) {
    wrap_meta(ui, "File", file);
    let Some(e) = entry else {
        ui.weak("No catalog metadata for this LoRA.");
        return;
    };
    if !e.name.trim().is_empty() && e.name != e.file {
        wrap_meta(ui, "Name", &e.name);
    }
    if !e.bases.is_empty() {
        wrap_meta(ui, "Bases", &e.bases.join(", "));
    }
    if !e.checkpoints.is_empty() {
        wrap_meta(ui, "Checkpoints", &e.checkpoints.join(", "));
    }
    let mut strength = format!("model {:.2}, CLIP {:.2}", e.strength_model, e.strength_clip);
    match (e.strength_model_min, e.strength_model_max) {
        (Some(a), Some(b)) => strength.push_str(&format!(" (range {a:.2}–{b:.2})")),
        (Some(a), None) => strength.push_str(&format!(" (min {a:.2})")),
        (None, Some(b)) => strength.push_str(&format!(" (max {b:.2})")),
        _ => {}
    }
    if !e.strength_source.is_empty() {
        strength.push_str(&format!(" · via {}", e.strength_source));
    }
    wrap_meta(ui, "Strength", &strength);
    wrap_meta(ui, "Triggers", &e.trigger_text());
    wrap_meta(ui, "Negative", &e.negative_text());
    wrap_meta(ui, "Notes", e.notes.trim());
    if !e.tags.is_empty() {
        wrap_meta(ui, "Tags", &e.tags.join(", "));
    }
}

/// Resolve `want` to the option the server actually published: exact, then case-insensitive,
/// then by basename (the catalog may carry a subdirectory the loader name lacks, or vice versa).
fn installed_match(want: &str, options: &[String]) -> Option<String> {
    let want = want.trim();
    if want.is_empty() {
        return None;
    }
    if let Some(o) = options.iter().find(|o| o.as_str() == want) {
        return Some(o.clone());
    }
    if let Some(o) = options.iter().find(|o| o.eq_ignore_ascii_case(want)) {
        return Some(o.clone());
    }
    let base = crate::types::file_basename(want).to_lowercase();
    options
        .iter()
        .find(|o| crate::types::file_basename(o).to_lowercase() == base)
        .cloned()
}

/// Split a filename into lowercase alphanumeric tokens of 3+ chars.
fn name_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(str::to_string)
        .collect()
}

/// The option sharing the most name tokens with the model's base tags — this is what makes a
/// `qwen_image` base pick `qwen_image_vae.safetensors` with no user action. `None` when nothing
/// overlaps, so the caller can fall through rather than guess.
fn best_by_bases(options: &[String], bases: &[String]) -> Option<String> {
    let wanted: Vec<String> = bases.iter().flat_map(|b| name_tokens(b)).collect();
    if wanted.is_empty() {
        return None;
    }
    options
        .iter()
        .map(|o| {
            let toks = name_tokens(o);
            (o, wanted.iter().filter(|w| toks.contains(w)).count())
        })
        .filter(|(_, score)| *score > 0)
        .max_by_key(|(_, score)| *score)
        .map(|(o, _)| o.clone())
}

/// Match a catalog sampler/scheduler name to a server option (ComfyUI vs Civitai spellings).
fn match_sampler_name(want: &str, options: &[String]) -> Option<String> {
    let want = want.trim();
    if want.is_empty() {
        return None;
    }
    if let Some(o) = options.iter().find(|o| o.eq_ignore_ascii_case(want)) {
        return Some(o.clone());
    }
    let norm = |s: &str| {
        s.trim()
            .to_lowercase()
            .replace("++", "pp")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
            .split('_')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("_")
    };
    let target = norm(want);
    options.iter().find(|o| norm(o) == target).cloned()
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

fn combo_full(ui: &mut egui::Ui, id: &str, current: &mut String, options: &[String]) {
    let w = ui.available_width().max(80.0);
    egui::ComboBox::from_id_salt(id)
        .selected_text(if current.is_empty() { "—".to_string() } else { elide(current, 48) })
        .width(w)
        .show_ui(ui, |ui| {
            for opt in options.iter().take(300) {
                ui.selectable_value(current, opt.clone(), elide(opt, 56));
            }
        });
}

fn full_width_slider(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui, f32)) {
    ui.horizontal(|ui| {
        ui.label(label);
        let w = ui.available_width();
        add(ui, w);
    });
}

/// Dependency order over `ids`, so an app's `$node:` refs only ever point backwards. Nodes in a
/// cycle keep their original order — the loader rejects those, which is the honest outcome.
fn toposort_nodes(
    ids: &[NodeId],
    incoming: &HashMap<(NodeId, usize), (NodeId, u32)>,
) -> Vec<NodeId> {
    let mut deps: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for ((to, _), (from, _)) in incoming {
        if ids.contains(to) && ids.contains(from) {
            deps.entry(*to).or_default().insert(*from);
        }
    }
    let mut out: Vec<NodeId> = Vec::new();
    let mut left: Vec<NodeId> = ids.to_vec();
    while !left.is_empty() {
        let ready: Vec<NodeId> = left
            .iter()
            .copied()
            .filter(|id| {
                deps.get(id).is_none_or(|d| d.iter().all(|p| !left.contains(p)))
            })
            .collect();
        if ready.is_empty() {
            out.extend(left.drain(..));
            break;
        }
        for id in ready {
            out.push(id);
            left.retain(|x| *x != id);
        }
    }
    out
}

/// The custom-node pack a class most likely comes from, read off its `/object_info` category.
fn pack_guess(category: &str, class: &str) -> String {
    let head = category.split('/').next().unwrap_or_default().trim();
    match head {
        "" => class.to_string(),
        // Stock ComfyUI categories; anything else names a pack closely enough to search for.
        "image" | "latent" | "sampling" | "loaders" | "conditioning" | "mask" | "advanced"
        | "_for_testing" | "utils" => "core".into(),
        other => other.to_string(),
    }
}

/// Lowercase, dot/dash-safe identifier derived from free text.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut last_sep = true;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep && (c == '.' || c == '_' || c == '-' || c == ' ') {
            out.push(if c == ' ' { '.' } else { c });
            last_sep = true;
        }
    }
    out.trim_matches(['.', '_', '-']).to_string()
}

/// Write a JSON literal into an editor widget, keeping the widget's own kind.
/// Returns whether the value was actually taken. A combo that does not list the value, or a JSON
/// type the widget cannot hold, leaves the widget at its own default — the caller has to know,
/// or an app inserts looking clean while silently ignoring what it asked for.
#[must_use]
fn set_flow_value(slot: &mut FlowValueType, v: &serde_json::Value) -> bool {
    match slot {
        FlowValueType::Array { options, selected } => {
            let Some(s) = v.as_str() else { return false };
            if !options.is_empty() && !options.iter().any(|o| o == s) {
                return false;
            }
            *selected = s.to_string();
            true
        }
        FlowValueType::String { value, .. } => {
            match v {
                serde_json::Value::String(s) => *value = s.clone(),
                other => *value = other.to_string(),
            }
            true
        }
        FlowValueType::Float { value, min, max, .. } => match v.as_f64() {
            Some(f) => {
                *value = f.clamp(*min, *max);
                true
            }
            None => false,
        },
        FlowValueType::SignedInt { value, min, max, .. } => {
            match v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)) {
                Some(i) => {
                    *value = i.clamp(*min, *max);
                    true
                }
                None => false,
            }
        }
        FlowValueType::UnsignedInt { value, min, max, .. } => {
            match v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)) {
                Some(u) => {
                    *value = u.clamp(*min, *max);
                    true
                }
                None => false,
            }
        }
        FlowValueType::Boolean(value) => match v.as_bool() {
            Some(b) => {
                *value = b;
                true
            }
            None => false,
        },
        // Connection-only inputs carry no literal.
        _ => false,
    }
}

/// Read an editor widget back out as JSON, for promoting it into a knob.
fn flow_value_json(v: &FlowValueType) -> Option<serde_json::Value> {
    Some(match v {
        FlowValueType::Array { selected, .. } => serde_json::Value::from(selected.clone()),
        FlowValueType::String { value, .. } => serde_json::Value::from(value.clone()),
        FlowValueType::Float { value, .. } => serde_json::Value::from(*value),
        FlowValueType::SignedInt { value, .. } => serde_json::Value::from(*value),
        FlowValueType::UnsignedInt { value, .. } => serde_json::Value::from(*value),
        FlowValueType::Boolean(value) => serde_json::Value::from(*value),
        _ => return None,
    })
}

/// Normalise a stored knob value to its declared type and range.
fn coerce_knob(v: &serde_json::Value, ty: &KnobTy) -> serde_json::Value {
    use serde_json::Value;
    match ty {
        KnobTy::Int { min, max, .. } => {
            let n = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(*min);
            Value::from(n.clamp(*min, *max))
        }
        KnobTy::Float { min, max, .. } => {
            let f = v.as_f64().unwrap_or(*min);
            Value::from(if f.is_finite() { f.clamp(*min, *max) } else { *min })
        }
        KnobTy::Bool => Value::from(v.as_bool().unwrap_or(false)),
        KnobTy::Choice { options } => match v.as_str() {
            Some(s) if options.iter().any(|o| o == s) => v.clone(),
            _ => Value::from(options.first().cloned().unwrap_or_default()),
        },
        // Enum options come from the live catalog, so the build layer does that substitution.
        KnobTy::Enum { .. } | KnobTy::Text { .. } => match v {
            Value::String(_) => v.clone(),
            other => Value::from(other.to_string()),
        },
    }
}

/// The knob type matching an editor widget, with its live options and bounds.
fn knob_ty_for(class: &str, input: &str, v: &FlowValueType) -> KnobTy {
    match v {
        FlowValueType::Array { .. } => {
            KnobTy::Enum { class: class.into(), input: input.into(), prefix: None }
        }
        FlowValueType::String { multiline, .. } => KnobTy::Text { multiline: *multiline },
        FlowValueType::Float { min, max, step, .. } => {
            KnobTy::Float { min: *min, max: *max, step: *step }
        }
        FlowValueType::SignedInt { min, max, step, .. } => {
            KnobTy::Int { min: *min, max: *max, step: (*step).max(1) }
        }
        FlowValueType::UnsignedInt { min, max, step, .. } => KnobTy::Int {
            min: (*min).min(i64::MAX as u64) as i64,
            max: (*max).min(i64::MAX as u64) as i64,
            step: (*step).max(1) as i64,
        },
        _ => KnobTy::Bool,
    }
}

/// [`full_width_slider`] returning the inner widget's response, for change detection.
fn full_width_slider_resp(
    ui: &mut egui::Ui,
    label: &str,
    add: impl FnOnce(&mut egui::Ui, f32) -> egui::Response,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(label);
        let w = ui.available_width();
        add(ui, w)
    })
    .inner
}

const SIZE_PRESETS: &[(&str, u32, u32)] = &[
    ("512 × 512", 512, 512),
    ("768 × 768", 768, 768),
    ("1024 × 1024", 1024, 1024),
    ("832 × 1216", 832, 1216),
    ("1216 × 832", 1216, 832),
    ("896 × 1152", 896, 1152),
    ("1152 × 896", 1152, 896),
    ("768 × 1344", 768, 1344),
    ("1344 × 768", 1344, 768),
    ("1536 × 640", 1536, 640),
    ("640 × 1536", 640, 1536),
];

fn size_preset_combo(ui: &mut egui::Ui, width: &mut u32, height: &mut u32) {
    let label = SIZE_PRESETS
        .iter()
        .find(|(_, w, h)| *w == *width && *h == *height)
        .map(|(name, _, _)| (*name).to_string())
        .unwrap_or_else(|| "Custom".into());
    egui::ComboBox::from_id_salt("size_preset")
        .selected_text(label)
        .width(120.0)
        .show_ui(ui, |ui| {
            for (name, w, h) in SIZE_PRESETS {
                if ui.selectable_label(*width == *w && *height == *h, *name).clicked() {
                    *width = *w;
                    *height = *h;
                }
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

fn params_fingerprint(p: &Params) -> u64 {
    str_fingerprint(&serde_json::to_string(p).unwrap_or_default())
}

fn str_fingerprint(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Find a MODEL/CLIP wire to splice into — prefer edges into a sampler, else the last chain edge.
fn find_chain_edge(
    snarl: &egui_snarl::Snarl<FlowNodeData>,
    exclude: NodeId,
    typ: rucomfyui::object_info::ObjectType,
    input_name: &str,
) -> Option<(OutPinId, InPinId)> {
    let mut into_sampler = None;
    let mut other = None;
    for (from, to) in snarl.wires() {
        if from.node == exclude || to.node == exclude {
            continue;
        }
        let Some(src) = snarl.get_node(from.node) else { continue };
        let Some(dst) = snarl.get_node(to.node) else { continue };
        let Some(out) = src.outputs.get(from.output) else { continue };
        let Some(inp) = dst.inputs.get(to.input) else { continue };
        let type_ok = out.typ == typ || inp.typ == typ;
        let name_ok = inp.name.eq_ignore_ascii_case(input_name);
        if !(type_ok && name_ok) {
            continue;
        }
        let edge = (from, to);
        if dst.object.name.contains("Sampler") {
            into_sampler = Some(edge);
        } else {
            other = Some(edge);
        }
    }
    into_sampler.or(other)
}

/// Disconnect `from→to`, then wire `from→node.in` and `node.out→to`.
fn splice_edge(
    snarl: &mut egui_snarl::Snarl<FlowNodeData>,
    from: OutPinId,
    to: InPinId,
    node: NodeId,
    in_idx: usize,
    out_idx: usize,
) {
    snarl.disconnect(from, to);
    let node_in = InPinId { node, input: in_idx };
    let node_out = OutPinId { node, output: out_idx };
    for remote in snarl.in_pin(node_in).remotes.clone() {
        snarl.disconnect(remote, node_in);
    }
    for remote in snarl.out_pin(node_out).remotes.clone() {
        snarl.disconnect(node_out, remote);
    }
    snarl.connect(from, node_in);
    snarl.connect(node_out, to);
}

/// Append LoRA trigger tokens into the first non-empty CLIPTextEncode prompt.
fn inject_lora_triggers(snarl: &mut egui_snarl::Snarl<FlowNodeData>, triggers: &str) {
    if triggers.trim().is_empty() {
        return;
    }
    for data in snarl.nodes_mut() {
        if data.object.name != "CLIPTextEncode" {
            continue;
        }
        let Some(text) = data.inputs.iter_mut().find(|i| i.name == "text") else {
            continue;
        };
        let FlowValueType::String { value, .. } = &mut text.value else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        let injected = merge_triggers(value, triggers, "");
        if !injected.is_empty() {
            return;
        }
    }
}

app!(ComfyApp::new);
