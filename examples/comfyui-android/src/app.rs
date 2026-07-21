//! The Android UI: Generate (params, output), Graph (node editor over server workflows), Properties,
//! Gallery (server output browser with albums), and Settings (server, API key, account, logs).

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use egui_mobile::{CreateContext, EguiApp, Haptic, Host, HostExt, ScreenOrientation, device_orientation_deg, app, egui};
use egui_snarl::{InPinId, OutPinId};
use rucomfyui::workflow::WorkflowNodeId;
use rucomfyui_node_graph::{ComfyUiNodeGraph, NodeId, internal::FlowNodeData, internal::FlowValueType};

use crate::apps::{AppDef, AppSet, KnobTy, Status};
use crate::engine::{Engine, GenCtx, Msg, QueueJob};
use crate::gallery::{self, ImageMeta, RemixDiffRow, RemixField, ThumbCache};
use crate::graphview::{self, GraphView, LongPress, LoraPick, elide, elide_width, sanitize_ui_text};
use crate::icons;
use crate::mask;
use crate::{cooc, lint, tags};
use crate::logger::{self, Logger};
use crate::player::Player;
use crate::schema::{self, SchemaSet};
use crate::{clip_index, tag_index};
use crate::uiwf;
use crate::types::{
    ActiveLora, Album, AppPack, AppStep, AppliedCharacter, CHECKPOINT_RECENT_MAX, CharacterCard,
    CharacterPack, CheckpointCatalog, CheckpointSort,
    character_tags_from_prompt, dedupe_loras, extract_triggers_from_positive,
    CreatePreset, FALLBACK_SAMPLERS, FALLBACK_SCHEDULERS, Facets, FontSizes, GalleryGroup,
    GalleryItem, GalleryMedia, GallerySort, GalleryView, Img2ImgSource, LoraCatalog, LoraPack, Mode,
    ModelKind, Params, PromptHist, RatingFilter, SamplerPack, Settings, append_negatives,
    checkpoint_family, fallback_vec, file_basename, merge_triggers, push_prompt_hist, strip_injected,
};
#[cfg(feature = "local-npu")]
use crate::types::LocalBackend;

/// Ceiling on auto-loaded gallery items, so a huge namespace can't page forever.
const GALLERY_LOAD_ALL_CAP: u64 = 5000;
/// comfy-gate clamps `/gallery/api/list` `limit` at this.
const GALLERY_PAGE_MAX: u64 = 500;
/// Window for the second Create Reset tap to confirm.
const RESET_CONFIRM_SECS: f64 = 4.0;

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
    Characters,
}

/// An in-progress character edit: which card is being replaced (by name), plus the working copy.
struct CharacterDraft {
    /// The existing card's name being edited; `None` for a brand-new card.
    editing: Option<String>,
    card: CharacterCard,
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

/// Clipboard kinds for the Create menu FAB (classified once when the menu opens).
struct FabClipSnap {
    has_wf: bool,
    has_sampler: bool,
    has_loras: bool,
    has_apps: bool,
}

impl FabClipSnap {
    fn from_text(text: &str) -> Self {
        let t = text.trim();
        Self {
            has_wf: t.starts_with('{') || t.starts_with('['),
            has_sampler: SamplerPack::from_clipboard_json(t).is_some(),
            has_loras: LoraPack::from_clipboard_json(t).is_some(),
            has_apps: AppPack::from_clipboard_json(t).is_some(),
        }
    }
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

/// Ranked gallery override: image-similarity ("More like this") or text semantic search.
enum RankedGallery {
    Similar(Vec<String>),
    Semantic(Vec<String>),
}

impl RankedGallery {
    fn keys(&self) -> &[String] {
        match self {
            Self::Similar(k) | Self::Semantic(k) => k,
        }
    }

    fn is_similar(&self) -> bool {
        matches!(self, Self::Similar(_))
    }

    fn is_semantic(&self) -> bool {
        matches!(self, Self::Semantic(_))
    }
}

/// Minimum cosine similarity for a "Find images" character match (tunable).
const CHARACTER_MATCH_COS: f32 = 0.55;
/// Higher bar for an unprompted auto-suggestion when a freshly indexed image scores against a card.
#[cfg(feature = "local-npu")]
const CHARACTER_SUGGEST_COS: f32 = 0.62;
/// Cap on a character's pending-suggestions list.
#[cfg(feature = "local-npu")]
const CHARACTER_SUGGEST_CAP: usize = 200;

/// A swipe session over a deck of gallery keys, shared by the grade pass and character review.
struct Triage {
    /// Gallery keys in deck order (grade: score/mtime descending; review: cosine descending).
    deck: Vec<String>,
    /// Index of the current undecided card.
    pos: usize,
    kept: usize,
    trashed: usize,
    /// Keys swiped right, batch album-added on commit.
    keep: Vec<String>,
    /// Keys swiped left (grade: trashed; review: denied), batched on commit.
    trash: Vec<String>,
    /// Grade-mode album kept cards join on commit; `None` leaves them in the gallery only.
    album: Option<i64>,
    /// Last recorded decision, for one-step Undo.
    last: Option<TriagePick>,
    /// What the deck's swipes mean and where the batch lands.
    mode: TriageMode,
}

/// What a [`Triage`] deck is grading and where committed decisions go.
#[derive(Clone)]
enum TriageMode {
    /// Grade pass over a burst: right keeps (optional album), left trashes (soft-delete), up reuses.
    Grade,
    /// Character review: right accepts into the card's album, left denies (remembered), up skips.
    Character { card: String },
}

/// A triage card outcome: swipe right keeps/accepts, left trashes/denies, up reuses or skips.
#[derive(Clone, Copy)]
enum TriagePick {
    Keep,
    Trash,
    Input,
}

/// Which remix fields an apply should write. All-true reproduces the one-tap Remix.
#[derive(Clone, Copy)]
struct RemixApply {
    model: bool,
    positive: bool,
    negative: bool,
    sampler: bool,
    scheduler: bool,
    steps: bool,
    cfg: bool,
    seed: bool,
    loras: bool,
}

impl RemixApply {
    const ALL: Self = Self {
        model: true,
        positive: true,
        negative: true,
        sampler: true,
        scheduler: true,
        steps: true,
        cfg: true,
        seed: true,
        loras: true,
    };
    const NONE: Self = Self {
        model: false,
        positive: false,
        negative: false,
        sampler: false,
        scheduler: false,
        steps: false,
        cfg: false,
        seed: false,
        loras: false,
    };
    fn set(&mut self, field: RemixField, on: bool) {
        match field {
            RemixField::Model => self.model = on,
            RemixField::Positive => self.positive = on,
            RemixField::Negative => self.negative = on,
            RemixField::Sampler => self.sampler = on,
            RemixField::Scheduler => self.scheduler = on,
            RemixField::Steps => self.steps = on,
            RemixField::Cfg => self.cfg = on,
            RemixField::Seed => self.seed = on,
            RemixField::Loras => self.loras = on,
        }
    }
}

/// The gallery image behind an open remix sheet, kept for the "as img2img" action.
enum RemixInput {
    Picked { name: String, bytes: Vec<u8> },
    Url(String),
    None,
}

/// Per-field remix diff sheet: pick which of an image's settings to port into Create.
struct RemixSheet {
    meta: ImageMeta,
    rows: Vec<RemixDiffRow>,
    /// Parallel to `rows`; each field is applied only while checked.
    enabled: Vec<bool>,
    input: RemixInput,
    seeds: usize,
}

/// A device photo chosen as img2img input this session; the bytes are never persisted.
struct PickedInput {
    name: String,
    bytes: Vec<u8>,
    tex: Option<egui::TextureHandle>,
}

/// Where the finish-pass colour-match reference frame comes from.
#[derive(Clone, Copy, PartialEq)]
enum FinishRef {
    /// The Create tab's current img2img input photo.
    CurrentInput,
    /// A photo picked from the device inside the sheet.
    Pick,
}

/// Video "Finish pass" sheet state: server-side post-process for a gallery video.
struct FinishSheet {
    /// Container-side path VHS_LoadVideoPath reads.
    video_path: String,
    ref_source: FinishRef,
    /// A device photo picked in-sheet: `(name, bytes)`.
    picked: Option<(String, Vec<u8>)>,
    scale_by: f32,
    rife_multiplier: u32,
    output_fps: u32,
}

/// Session-only finger-paint inpainting: strokes over a base image, baked into an alpha mask.
struct InpaintState {
    source_bytes: Vec<u8>,
    source_name: String,
    img_size: [u32; 2],
    base_tex: egui::TextureHandle,
    strokes: Vec<mask::StrokeRec>,
    /// Start index in `strokes` of each drag gesture, for whole-gesture undo.
    groups: Vec<usize>,
    canvas: mask::MaskCanvas,
    brush_uv: f32,
    erase: bool,
    overlay_tex: Option<egui::TextureHandle>,
    overlay_dirty: bool,
    /// Accept only stylus strokes (palm rejection); defaulted from device stylus presence once.
    stylus_only: bool,
    /// One-shot init of `stylus_only` from host stylus detection.
    input_inited: bool,
    /// Show live pointer telemetry (tool type, force, contact) over the canvas.
    show_debug: bool,
    /// Latest pointer force (0..1) from `egui::Event::Touch`, if any.
    dbg_force: Option<f32>,
    /// A touch was in contact this frame (Start/Move seen).
    dbg_contact: bool,
    /// A touch event arrived this frame (else the pointer is a mouse or absent).
    dbg_saw_touch: bool,
    /// Keep the center brush-size preview visible until this time (seconds).
    brush_preview_until: f64,
    /// Presentation-only zoom/pan for the paint surface; reset to fit when a new image opens.
    view: mask::ViewXform,
    /// A paint stroke is mid-gesture, so a second finger can cancel it before pinching.
    stroke_active: bool,
    /// A two-finger gesture is in progress; blocks painting until all pointers lift.
    nav_latch: bool,
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
    /// Per seed / noise_seed widget: `true` = randomize before each queue (`control_after_generate`).
    seed_randomize: HashMap<(NodeId, String), bool>,
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
            seed_randomize: HashMap::new(),
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
        self.seed_randomize.clear();
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

/// A prompt this app submitted, tracked for the queue sheet's "Yours" label and targeted cancel.
struct MyPrompt {
    id: String,
    label: String,
    added: f64,
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
    /// Container-side path of ComfyUI's output dir, for building VHS_LoadVideoPath finish paths.
    server_output_root: String,
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
    /// On-device recurring-character cards.
    characters: Vec<CharacterCard>,
    /// Undo bookkeeping for the currently applied character (at most one at a time).
    active_character: Option<AppliedCharacter>,
    /// Open card editor, or `None` when showing the card list.
    character_draft: Option<CharacterDraft>,
    /// Per-character denied gallery keys (persisted), keyed by card name; never re-surfaced.
    character_denied: std::collections::BTreeMap<String, Vec<String>>,
    /// Per-character pending match suggestions (persisted, capped), keyed by card name.
    character_suggestions: std::collections::BTreeMap<String, Vec<String>>,
    /// Session cache of per-character CLIP centroids for the suggest hot loop; cleared on change.
    character_centroids: HashMap<String, Vec<f32>>,
    /// After creating a character's collection album, stamp its id onto the card and add these
    /// items: `(card name, album name, items)`.
    char_album_pending: Option<(String, String, Vec<(String, String)>)>,
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
    /// When set, autosave will not overwrite on-disk settings (corrupt / unreadable file).
    settings_write_blocked: Option<String>,
    /// Passphrase for encrypted config export.
    backup_pass: String,
    backup_pass_confirm: String,
    /// Passphrase for encrypted config import.
    import_pass: String,
    /// Status line under the Backup section.
    backup_note: String,
    /// Cached `*.comfybk` paths (name, full path) under documents + external files.
    backup_list: Vec<(String, String)>,

    running: bool,
    progress: (u32, u32),
    status: String,
    /// Server-wide queue depth (WS status / `/queue`), includes jobs from other clients.
    queue_remaining: u32,
    /// Last time we polled `GET /queue`.
    last_queue_poll: f64,
    /// Latest per-job `GET /queue` snapshot for the queue sheet + targeted cancel.
    queue_jobs: (Vec<QueueJob>, Vec<QueueJob>),
    /// Prompts this app submitted (id, short label, added time) for "Yours" rows + our-pending cancel.
    my_prompts: Vec<MyPrompt>,
    /// Job labels awaiting a server prompt id, paired in submit order.
    pending_job_labels: std::collections::VecDeque<String>,
    /// The pending-jobs queue sheet is open.
    queue_sheet_open: bool,
    /// Two-tap arm state for the sheet's "Clear pending" button.
    queue_clear_arm: bool,

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
    /// First Reset tap arms the confirm; a second within `RESET_CONFIRM_SECS` runs it.
    reset_armed_at: Option<f64>,

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
    /// Graph-tab landscape fullscreen mode (OS orientation locked to landscape).
    graph_fullscreen: bool,
    /// Debounce timer: when the device has been near portrait for this long (seconds) exit fs.
    graph_fs_portrait_since: Option<f64>,
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
    /// Main search box runs CLIP semantic search instead of the server text query.
    gallery_semantic: bool,
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
    /// After a viewer delete, reopen this `(subfolder, filename)` once the list refreshes.
    viewer_after_delete: Option<(String, String)>,
    /// Scroll the Create tab to the result strip after a new image lands.
    create_scroll_bottom: bool,
    /// Soft keyboard was visible last frame; used to detect the open edge.
    kb_was_open: bool,
    /// The soft keyboard opened this frame; scroll the focused field into the shrunk viewport.
    kb_open_edge: bool,
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
    /// Device photo chosen as img2img input (session-only bytes + decoded preview).
    picked_input: Option<PickedInput>,
    /// Grid to change the device img2img photo is expanded inline.
    picked_input_grid_open: bool,
    /// Server-gallery picker sheet (choose an existing gallery image as the input) is open.
    gallery_pick_open: bool,
    /// Awaiting full bytes for a gallery-picked input: `(item key, filename)`.
    gallery_pick_pending: Option<(String, String)>,
    /// Full-screen finger-paint inpainting session (session-only, never persisted).
    inpaint: Option<InpaintState>,
    /// A Remix tap is waiting on the viewer's workflow meta before opening the diff sheet.
    viewer_remix_pending: bool,
    /// Open per-field remix diff sheet.
    remix_sheet: Option<RemixSheet>,
    /// Open video "Finish pass" sheet (server-side post-process for a gallery video).
    finish_sheet: Option<FinishSheet>,
    /// A finish job is in flight; its completion sets the gallery status note.
    finish_pending: bool,
    /// Long-press-on-Remix clock; a held press applies the full meta instantly.
    viewer_remix_press: Option<f64>,
    viewer_remix_long_fired: bool,
    /// How many separate Create jobs one Queue tap enqueues (1..=8).
    queue_variants: usize,
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
    /// Active grade-pass triage deck over recent results; `None` when not triaging.
    triage: Option<Triage>,
    /// Press origin for a triage card swipe (left/right/up).
    triage_swipe_origin: Option<egui::Pos2>,
    /// Gallery keys before the post-burst refresh, to diff out genuinely new results.
    pre_burst_keys: HashSet<String>,
    /// Result image count captured at burst end, consumed when the post-burst listing lands.
    pending_triage_n: usize,
    /// Remaining attempts to collect `untriaged` from an incoming offset-0 gallery listing.
    triage_collect: u8,
    /// Keys of recent burst results not yet triaged; drives the Triage entry chips.
    untriaged: Vec<String>,
    /// Re-fetch the gallery listing at this time (server indexing lag after generate).
    gallery_refresh_at: Option<f64>,
    /// Create-tab menu FAB position; `None` = default under the queue FAB.
    create_fab_pos: Option<egui::Pos2>,
    create_fab_open: bool,
    /// System clipboard snapshotted while the Create menu FAB is open (not polled every frame).
    create_fab_clip: Option<FabClipSnap>,
    /// Shared Create/Graph queue (play) FAB position; `None` = default above the menu/lock FAB.
    queue_fab_pos: Option<egui::Pos2>,
    /// Create Main: companions / img2img source block is expanded (persisted).
    create_setup_open: bool,
    /// Create Main: companions & image source block open state (persisted separately).
    create_companions_open: bool,
    /// Positive prompt shown as editable chips (session-only: the Settings struct is off-limits).
    prompt_chips: bool,
    /// Negative prompt shown as editable chips (session-only).
    neg_prompt_chips: bool,
    /// Recorded Create-tab prompt pairs for the history scrubber (newest last, capped; persisted).
    prompt_history: Vec<PromptHist>,
    /// Live draft stashed at the newest slider slot while scrubbing (session-only).
    hist_stash: Option<PromptHist>,
    /// Current 1-based scrubber slider position while `hist_stash` is set.
    hist_slider: usize,
    /// Prompt pair last written by a scrub, to detect a manual edit that detaches the scrubber.
    hist_applied: Option<(String, String)>,
    /// Bundled tag dictionary parsed and ready to query off-thread.
    tag_dict_warm: bool,
    /// Completion signal for the background tag-dictionary warmup.
    tag_dict_warming: Option<std::sync::mpsc::Receiver<()>>,
    /// Server tag-dictionary override; used ahead of the bundled dictionary when present.
    tag_dict_override: Option<Arc<tags::TagDict>>,
    /// Personal tag co-occurrence model learned from queued positive prompts.
    cooc: cooc::CoocModel,
    /// The co-occurrence model finished loading (or found no file) off-thread.
    cooc_loaded: bool,
    /// Delivery channel for the background co-occurrence load.
    cooc_loading: Option<std::sync::mpsc::Receiver<cooc::CoocModel>>,
    /// Cached prompt lint issues plus the fingerprint they were computed from.
    lint_issues: Vec<lint::LintIssue>,
    lint_fp: u64,
    /// Persistent on-device auto-tag index (gallery key -> WD14 tags); loaded off-thread on first use.
    tag_index: tag_index::TagIndex,
    tag_index_loaded: bool,
    tag_index_loading: Option<std::sync::mpsc::Receiver<tag_index::TagIndex>>,
    /// New index entries stored since the last save (writes are batched, never per frame).
    #[cfg(feature = "local-npu")]
    tag_index_dirty: usize,
    /// Persistent CLIP embedding index (gallery key -> embedding + aesthetic score).
    clip_index: clip_index::ClipIndex,
    clip_index_loaded: bool,
    clip_index_loading: Option<std::sync::mpsc::Receiver<clip_index::ClipIndex>>,
    #[cfg(feature = "local-npu")]
    clip_index_dirty: usize,
    #[cfg(feature = "local-npu")]
    clip_pack: Option<std::path::PathBuf>,
    #[cfg(feature = "local-npu")]
    clipemb_pending: Option<String>,
    #[cfg(feature = "local-npu")]
    clipemb_rx: Option<std::sync::mpsc::Receiver<(String, Result<(Vec<f32>, Option<f32>), String>)>>,
    #[cfg(feature = "local-npu")]
    clipemb_failed: HashSet<String>,
    /// Last semantic-search query text; poll_clip_search labels results from it (session-only).
    #[cfg(feature = "local-npu")]
    clip_text_q: String,
    /// Text-embedding worker result (the L2-normalized query embedding or an error string).
    #[cfg(feature = "local-npu")]
    clip_search_rx: Option<std::sync::mpsc::Receiver<Result<Vec<f32>, String>>>,
    #[cfg(feature = "local-npu")]
    clip_search_running: bool,
    /// Ranked gallery override (More like this / semantic search); overrides filters while set.
    ranked: Option<RankedGallery>,
    /// The gallery grid's visible viewport this frame; gates pull-to-refresh and tile hits.
    gallery_grid_clip: egui::Rect,
    /// Gallery client-side tag search box (session-only).
    tag_q: String,
    /// Filter box inside the all-tags browser window (session-only).
    tag_browse_q: String,
    /// Whether the pinned all-tags browser window is showing (session-only).
    tags_window_open: bool,
    /// 0 = all, 1 = indexed only, 2 = unindexed only (session-only).
    index_filter: u8,
    /// Active facet-chip tag filters, AND-combined with the search box (session-only).
    tag_facets: Vec<String>,

    /// D3 Anima smoke worker result.
    #[cfg(feature = "local-npu")]
    d3_rx: Option<std::sync::mpsc::Receiver<crate::local_engine::AnimaSmoke>>,
    /// Pack import: URL box, worker channel and last status line.
    #[cfg(feature = "local-npu")]
    pack_url: String,
    #[cfg(feature = "local-npu")]
    pack_name: String,
    #[cfg(feature = "local-npu")]
    pack_import_rx: Option<std::sync::mpsc::Receiver<crate::local_engine::ImportMsg>>,
    #[cfg(feature = "local-npu")]
    pack_import_status: String,
    #[cfg(feature = "local-npu")]
    d3_running: bool,
    #[cfg(feature = "local-npu")]
    d3_last: Option<String>,
    #[cfg(feature = "local-npu")]
    d3_ok: Option<bool>,
    /// WD14 "Read tags" worker result (ranked tags or an error string).
    #[cfg(feature = "local-npu")]
    wd14_rx: Option<std::sync::mpsc::Receiver<Result<local_wd14::TagResult, String>>>,
    #[cfg(feature = "local-npu")]
    wd14_running: bool,
    /// The ranked-tags sheet shown over the gallery once a tag read finishes.
    #[cfg(feature = "local-npu")]
    wd14_sheet: Option<local_wd14::TagResult>,
    /// Cached WD14 tagger pack dir under the app external files dir, if one is present.
    #[cfg(feature = "local-npu")]
    wd14_pack: Option<std::path::PathBuf>,
    /// Cached rewrite (CPU LLM) pack dir, if one is present.
    #[cfg(feature = "local-npu")]
    rewrite_pack: Option<std::path::PathBuf>,
    /// Prompt-rewrite worker result (rewritten positive prompt or an error string).
    #[cfg(feature = "local-npu")]
    rewrite_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    #[cfg(feature = "local-npu")]
    rewrite_running: bool,
    /// Settings: background-tag the server gallery when idle.
    #[cfg(feature = "local-npu")]
    auto_tag: bool,
    /// Settings: idle-download full gallery images into the on-device cache.
    cache_prefetch: bool,
    /// Resolved `gallery_full` directory (durable or app files); filled on first Settings/update.
    full_cache_root: Option<String>,
    /// Key of the full-image prefetch currently in flight.
    prefetch_pending: Option<String>,
    /// Prefetch failures this session; skipped until restart.
    prefetch_failed: HashSet<String>,
    /// Keys verified present in the full cache this session; spares per-frame re-stats (FUSE is slow).
    prefetch_cached: HashSet<String>,
    /// Last finished Settings cache scan, the worker computing the next one, and its kick time.
    full_cache_report: Option<FullCacheReport>,
    full_cache_report_rx: Option<std::sync::mpsc::Receiver<FullCacheReport>>,
    full_cache_report_at: f64,
    /// Confirm wipe of the full-image cache.
    cache_clear_confirm: bool,
    /// Awaiting full bytes for an auto-tag job: the item key being fetched.
    #[cfg(feature = "local-npu")]
    autotag_pending: Option<String>,
    /// In-flight auto-tag worker result: (item key, ranked tags or error).
    #[cfg(feature = "local-npu")]
    autotag_rx: Option<std::sync::mpsc::Receiver<(String, Result<local_wd14::TagResult, String>)>>,
    /// Item keys whose auto-tag failed this session; not retried until restart.
    #[cfg(feature = "local-npu")]
    autotag_failed: HashSet<String>,
    /// Settings: route Create Queue to on-device HTP instead of the ComfyUI server.
    #[cfg(feature = "local-npu")]
    local_npu: bool,
    /// Settings: which on-device pipeline the Local NPU path runs.
    #[cfg(feature = "local-npu")]
    local_backend: LocalBackend,
    /// Settings: selected pack subdir name under the app external files dir.
    #[cfg(feature = "local-npu")]
    local_pack: String,
    /// Settings: route Create generation to the server (Server model pick) while the stack stays on.
    #[cfg(feature = "local-npu")]
    local_use_server: bool,
    /// Cached external-files-dir scan; refreshed on demand from Settings.
    #[cfg(feature = "local-npu")]
    local_packs: Vec<crate::local_engine::PackEntry>,
    #[cfg(feature = "local-npu")]
    local_packs_scanned: bool,
    /// Newest file mtime per pack dir, captured at scan time (stat per frame is too slow on FUSE).
    #[cfg(feature = "local-npu")]
    pack_mtimes: HashMap<std::path::PathBuf, std::time::SystemTime>,
    /// Skip pump_clipemb's idle cache-dir walk until this time (egui clock).
    #[cfg(feature = "local-npu")]
    clipemb_rescan_after: f64,
}

/// Settings-pane cache numbers, computed on a worker thread: stat-ing every cached file through
/// Android's FUSE storage takes far too long to run per frame.
#[derive(Clone)]
struct FullCacheReport {
    /// Listed non-video keys present in the cache when the scan ran.
    cached: usize,
    /// Non-video keys in the loaded listing when the scan ran.
    listed: usize,
    stats: gallery::FullCacheStats,
    /// `.key` sidecars with a live image (CLIP index candidates).
    #[cfg_attr(not(feature = "local-npu"), allow(dead_code))]
    keyed: usize,
    root: String,
}

/// Which prompt string a chip/text view edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptField {
    Positive,
    Negative,
}

impl PromptField {
    /// The `TextEdit` id backing this field's editor.
    fn edit_id(self) -> egui::Id {
        egui::Id::new(match self {
            Self::Positive => "create_positive_edit",
            Self::Negative => "create_negative_edit",
        })
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Positive => "what you want to see",
            Self::Negative => "what to avoid",
        }
    }

    fn rows(self) -> usize {
        match self {
            Self::Positive => 3,
            Self::Negative => 2,
        }
    }

    /// The opposite field, target of a cross-field chip move.
    fn other(self) -> Self {
        match self {
            Self::Positive => Self::Negative,
            Self::Negative => Self::Positive,
        }
    }

    /// Chip-menu label for moving a chip to the other field.
    fn move_label(self) -> &'static str {
        match self {
            Self::Positive => "To negative",
            Self::Negative => "To positive",
        }
    }

    /// Stable discriminant salting per-field widget ids and drag payloads.
    fn disc(self) -> u8 {
        match self {
            Self::Positive => 0,
            Self::Negative => 1,
        }
    }
}

/// Chip drag payload: the source field's discriminant and the dragged chip index.
struct ChipDrag {
    field: u8,
    idx: usize,
}

/// A pack dir's root: ("app files", wiped=true) under the app external files dir, ("/sdcard/ComfyUI",
/// false) under the durable dir, else the parent path.
#[cfg(feature = "local-npu")]
fn pack_root_note(dir: &std::path::Path, app_root: Option<&str>, durable: &str) -> (String, bool) {
    if let Some(app) = app_root
        && dir.starts_with(app)
    {
        return ("app files".into(), true);
    }
    if dir.starts_with(durable) {
        return ("/sdcard/ComfyUI".into(), false);
    }
    (dir.parent().map(|p| p.display().to_string()).unwrap_or_default(), false)
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
            server_output_root: crate::types::default_server_output_root(),
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
            characters: Vec::new(),
            active_character: None,
            character_draft: None,
            character_denied: std::collections::BTreeMap::new(),
            character_suggestions: std::collections::BTreeMap::new(),
            character_centroids: HashMap::new(),
            char_album_pending: None,
            apps: Arc::new(AppSet::builtin()),
            app_picker: None,
            app_filter: String::new(),
            enhance_note: String::new(),
            publish: None,
            params: Params::default(),
            last_saved: None,
            last_save_check: 0.0,
            settings_write_blocked: None,
            backup_pass: String::new(),
            backup_pass_confirm: String::new(),
            import_pass: String::new(),
            backup_note: String::new(),
            backup_list: Vec::new(),
            running: false,
            progress: (0, 0),
            status: String::new(),
            queue_remaining: 0,
            last_queue_poll: 0.0,
            queue_jobs: (Vec::new(), Vec::new()),
            my_prompts: Vec::new(),
            pending_job_labels: std::collections::VecDeque::new(),
            queue_sheet_open: false,
            queue_clear_arm: false,
            preview: None,
            result: None,
            result_bytes: None,
            results: Vec::new(),
            result_view: None,
            result_seq: 0,
            save_counter: 0,
            note: String::new(),
            reset_armed_at: None,
            graph_tabs: Vec::new(),
            active_graph: 0,
            next_graph_id: 1,
            create_graph_id: None,
            create_sync_fp: 0,
            create_graph_export_fp: 0,
            create_sync_dirty_at: None,
            graph_pane: GraphPane::Canvas,
            graph_fullscreen: false,
            graph_fs_portrait_since: None,
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
            gallery_semantic: true,
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
            viewer_after_delete: None,
            create_scroll_bottom: false,
            kb_was_open: false,
            kb_open_edge: false,
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
            picked_input: None,
            picked_input_grid_open: false,
            gallery_pick_open: false,
            gallery_pick_pending: None,
            inpaint: None,
            viewer_remix_pending: false,
            remix_sheet: None,
            finish_sheet: None,
            finish_pending: false,
            viewer_remix_press: None,
            viewer_remix_long_fired: false,
            queue_variants: 1,
            canvas_menu: None,
            node_menu: None,
            gallery_scroll_y: 0.0,
            gallery_scroll_restore: None,
            gallery_pull_tracking: false,
            gallery_pull: 0.0,
            thumb_aspects: HashMap::new(),
            filmstrip_center: false,
            viewer_swipe_origin: None,
            triage: None,
            triage_swipe_origin: None,
            pre_burst_keys: HashSet::new(),
            pending_triage_n: 0,
            triage_collect: 0,
            untriaged: Vec::new(),
            gallery_refresh_at: None,
            create_fab_pos: None,
            create_fab_open: false,
            create_fab_clip: None,
            queue_fab_pos: None,
            create_setup_open: true,
            create_companions_open: true,
            prompt_chips: false,
            neg_prompt_chips: false,
            prompt_history: Vec::new(),
            hist_stash: None,
            hist_slider: 0,
            hist_applied: None,
            tag_dict_warm: false,
            tag_dict_warming: None,
            tag_dict_override: None,
            cooc: cooc::CoocModel::default(),
            cooc_loaded: false,
            cooc_loading: None,
            lint_issues: Vec::new(),
            lint_fp: 0,
            tag_index: tag_index::TagIndex::default(),
            tag_index_loaded: false,
            tag_index_loading: None,
            #[cfg(feature = "local-npu")]
            tag_index_dirty: 0,
            gallery_grid_clip: egui::Rect::NOTHING,
            tag_browse_q: String::new(),
            tags_window_open: false,
            index_filter: 0,
            clip_index: clip_index::ClipIndex::default(),
            clip_index_loaded: false,
            clip_index_loading: None,
            #[cfg(feature = "local-npu")]
            clip_index_dirty: 0,
            #[cfg(feature = "local-npu")]
            clip_pack: None,
            #[cfg(feature = "local-npu")]
            clipemb_pending: None,
            #[cfg(feature = "local-npu")]
            clipemb_rx: None,
            #[cfg(feature = "local-npu")]
            clipemb_failed: HashSet::new(),
            #[cfg(feature = "local-npu")]
            clip_text_q: String::new(),
            #[cfg(feature = "local-npu")]
            clip_search_rx: None,
            #[cfg(feature = "local-npu")]
            clip_search_running: false,
            ranked: None,
            tag_q: String::new(),
            tag_facets: Vec::new(),
            #[cfg(feature = "local-npu")]
            d3_rx: None,
            #[cfg(feature = "local-npu")]
            pack_url: String::new(),
            #[cfg(feature = "local-npu")]
            pack_name: String::new(),
            #[cfg(feature = "local-npu")]
            pack_import_rx: None,
            #[cfg(feature = "local-npu")]
            pack_import_status: String::new(),
            #[cfg(feature = "local-npu")]
            d3_running: false,
            #[cfg(feature = "local-npu")]
            d3_last: None,
            #[cfg(feature = "local-npu")]
            d3_ok: None,
            #[cfg(feature = "local-npu")]
            wd14_rx: None,
            #[cfg(feature = "local-npu")]
            wd14_running: false,
            #[cfg(feature = "local-npu")]
            wd14_sheet: None,
            #[cfg(feature = "local-npu")]
            wd14_pack: None,
            #[cfg(feature = "local-npu")]
            rewrite_pack: None,
            #[cfg(feature = "local-npu")]
            rewrite_rx: None,
            #[cfg(feature = "local-npu")]
            rewrite_running: false,
            #[cfg(feature = "local-npu")]
            auto_tag: false,
            cache_prefetch: true,
            full_cache_root: None,
            prefetch_pending: None,
            prefetch_failed: HashSet::new(),
            prefetch_cached: HashSet::new(),
            full_cache_report: None,
            full_cache_report_rx: None,
            full_cache_report_at: 0.0,
            cache_clear_confirm: false,
            #[cfg(feature = "local-npu")]
            autotag_pending: None,
            #[cfg(feature = "local-npu")]
            autotag_rx: None,
            #[cfg(feature = "local-npu")]
            autotag_failed: HashSet::new(),
            #[cfg(feature = "local-npu")]
            local_npu: false,
            #[cfg(feature = "local-npu")]
            local_backend: LocalBackend::default(),
            #[cfg(feature = "local-npu")]
            local_pack: String::new(),
            #[cfg(feature = "local-npu")]
            local_use_server: false,
            #[cfg(feature = "local-npu")]
            local_packs: Vec::new(),
            #[cfg(feature = "local-npu")]
            local_packs_scanned: false,
            #[cfg(feature = "local-npu")]
            pack_mtimes: HashMap::new(),
            #[cfg(feature = "local-npu")]
            clipemb_rescan_after: 0.0,
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
        self.log.info(format!("graph load '{name}': auto-arrange {auto}"));
        doc.outputs.clear();
        doc.node_map.clear();
        doc.props_node = None;
        doc.bypassed.clear();
        doc.seed_randomize.clear();
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

    /// Like [`Self::replace_workflow_in_tab`], then apply seed-randomize flags from a UI load or Create.
    fn replace_workflow_in_tab_with_seeds(
        &mut self,
        idx: usize,
        name: String,
        workflow: &rucomfyui::Workflow,
        seed_randomize: &std::collections::BTreeMap<(u64, String), bool>,
        default_randomize: Option<bool>,
    ) -> Result<(), String> {
        self.replace_workflow_in_tab(idx, name, workflow)?;
        let Some(doc) = self.graph_tabs.get_mut(idx) else {
            return Err("no graph tab".into());
        };
        if let Some(flag) = default_randomize {
            graphview::set_all_seed_randomize(&doc.graph.snarl, &mut doc.seed_randomize, flag);
        } else {
            graphview::apply_seed_randomize_from_workflow(
                &doc.graph.snarl,
                workflow,
                seed_randomize,
                &mut doc.seed_randomize,
            );
        }
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

    fn load_workflow_into_tab_with_seeds(
        &mut self,
        name: String,
        workflow: &rucomfyui::Workflow,
        seed_randomize: &std::collections::BTreeMap<(u64, String), bool>,
        default_randomize: Option<bool>,
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
        let idx = self.active_graph;
        self.replace_workflow_in_tab_with_seeds(idx, name, workflow, seed_randomize, default_randomize)
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
                // Finish creating a character's collection album: stamp its id onto the card, add.
                if let Some((card, album_name, items)) = self.char_album_pending.take() {
                    if let Some(id) = self.albums.iter().find(|a| a.name == album_name).map(|a| a.id) {
                        if let Some(c) = self.characters.iter_mut().find(|c| c.name == card) {
                            c.album_id = id;
                        }
                        if !items.is_empty() {
                            self.engine.as_ref().unwrap().album_add(id, items);
                        }
                    } else {
                        self.char_album_pending = Some((card, album_name, items));
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
                // Paste may have landed before the catalog; peel triggers out now.
                if !self.params.loras.is_empty() {
                    self.pull_lora_triggers_from_positive();
                }
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
            Msg::TagDict(dict) => {
                self.log.info(format!("tag dict override: {} entries", dict.len()));
                self.tag_dict_override = Some(dict);
                self.tag_dict_warm = true;
            }
            Msg::Connected { schemas, models } => {
                self.conn = Conn::Connected;
                // Albums and model facets are per-account, so they follow the credential.
                self.engine.as_ref().unwrap().albums();
                self.engine.as_ref().unwrap().facets();
                self.engine.as_ref().unwrap().fetch_lora_catalog();
                self.engine.as_ref().unwrap().fetch_checkpoint_catalog();
                self.engine.as_ref().unwrap().fetch_tag_dict();
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
            Msg::PromptId(id) => {
                let label = self.pending_job_labels.pop_front().unwrap_or_else(|| "Create".into());
                let added = ctx.input(|i| i.time);
                self.my_prompts.push(MyPrompt { id, label, added });
            }
            Msg::QueueJobs { running, pending } => {
                let now = ctx.input(|i| i.time);
                let live: HashSet<&str> = running
                    .iter()
                    .chain(pending.iter())
                    .map(|j| j.prompt_id.as_str())
                    .collect();
                // Drop finished prompts, keeping just-submitted ids not yet in a snapshot.
                self.my_prompts.retain(|p| live.contains(p.id.as_str()) || now - p.added < 6.0);
                self.queue_jobs = (running, pending);
            }
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
                self.create_scroll_bottom = true;
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
                    self.status = if self.params.mode == Mode::Video {
                        "Done — video saved to the Gallery".into()
                    } else {
                        "Done".into()
                    };
                    if std::mem::take(&mut self.finish_pending) {
                        self.gallery_status = "Finished video saved to the Gallery".into();
                    }
                    host.haptic(Haptic::Success);
                    host.notify("ComfyUI", "Generation finished");
                    // New outputs should show up without a manual refresh (retry once for index lag).
                    if matches!(self.conn, Conn::Connected) {
                        // Snapshot the pre-refresh listing to diff out the post-burst results.
                        self.pre_burst_keys = self.gallery.iter().map(|it| it.key()).collect();
                        self.pending_triage_n = self.results.len();
                        self.triage_collect = 2;
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
                self.finish_pending = false;
                self.my_prompts.clear();
                self.pending_job_labels.clear();
                self.status = if matches!(self.conn, Conn::Connected) {
                    "Cancelled — server interrupted".into()
                } else {
                    "Cancelled".into()
                };
            }
            Msg::GenError(e) => {
                self.jobs_left = self.jobs_left.saturating_sub(1);
                self.progress = (0, 0);
                self.executing = None;
                if self.jobs_left == 0 {
                    self.running = false;
                    self.finish_pending = false;
                }
                self.status = format!("Error: {e}");
                host.haptic(Haptic::Error);
            }
            Msg::Workflows(names) => {
                self.wf_loading = false;
                self.wf_names = names;
            }
            Msg::WorkflowLoaded { name, workflow, warnings, seed_randomize } => {
                self.wf_loading = false;
                self.executing = None;
                match self.load_workflow_into_tab_with_seeds(name, &workflow, &seed_randomize, None) {
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
                if self.triage_collect > 0 && page.offset == 0 {
                    self.triage_collect -= 1;
                    self.collect_untriaged();
                    if !self.untriaged.is_empty() {
                        self.triage_collect = 0;
                    }
                }
                self.gallery_status.clear();
                // With a model filter, album, or grouping active, the whole set has to be present
                // for the groups/results to be complete — keep paging (in big chunks) instead of
                // making the user tap "Load more". Capped so a huge namespace can't runaway.
                let loaded = self.gallery.len() as u64;
                let more = self.gallery_wants_all()
                    && loaded < self.gallery_total
                    && loaded < GALLERY_LOAD_ALL_CAP;
                if more {
                    self.gallery_loading = true;
                    self.engine.as_ref().unwrap().gallery_list(
                        self.gallery_gen,
                        loaded,
                        self.gallery_page_size(),
                        self.gallery_list_q(),
                        &self.gallery_view,
                    );
                } else if self.viewer_after_delete.is_some() || page.offset == 0 {
                    self.resume_viewer_after_delete(host);
                }
            }
            Msg::GalleryError(e) => {
                self.gallery_loading = false;
                if let Some(v) = &mut self.viewer {
                    v.loading = false;
                }
                self.gallery_status = elide(&e, 200);
            }
            // A full-image fetch died (HTTP error / undecodable file / not connected). Fail exactly
            // the consumer waiting on that key — a dangling pending wedges its pump forever.
            Msg::FullImageError { key, why } => {
                self.log.warn(format!("full image {}: {}", elide(&key, 60), elide(&why, 120)));
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.loading = false;
                }
                if self.prefetch_pending.as_deref() == Some(key.as_str()) {
                    self.prefetch_pending = None;
                    self.prefetch_failed.insert(key.clone());
                }
                #[cfg(feature = "local-npu")]
                {
                    if self.autotag_pending.as_deref() == Some(key.as_str()) {
                        self.autotag_pending = None;
                        self.autotag_failed.insert(key.clone());
                    }
                    if self.clipemb_pending.as_deref() == Some(key.as_str()) {
                        self.clipemb_pending = None;
                        self.clipemb_failed.insert(key.clone());
                    }
                }
                if self.gallery_pick_pending.as_ref().is_some_and(|(k, _)| *k == key) {
                    self.gallery_pick_pending = None;
                    self.note = format!("Image load failed: {}", elide(&why, 80));
                }
                self.gallery_status = elide(&format!("{key}: {why}"), 200);
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
                let for_prefetch = self.prefetch_pending.as_deref() == Some(key.as_str());
                #[cfg(feature = "local-npu")]
                let for_pump = self.autotag_pending.as_deref() == Some(key.as_str());
                #[cfg(not(feature = "local-npu"))]
                let for_pump = false;
                #[cfg(feature = "local-npu")]
                let for_emb = self.clipemb_pending.as_deref() == Some(key.as_str());
                #[cfg(not(feature = "local-npu"))]
                let for_emb = false;
                if for_prefetch {
                    self.prefetch_pending = None;
                    // Bytes already written by fetch_full; nothing else to do.
                } else if for_pump {
                    #[cfg(feature = "local-npu")]
                    {
                        self.autotag_pending = None;
                        // A generation or Read-tags may have started while this fetch was in flight;
                        // defer rather than contend for the HTP. The bytes are disk-cached, so the
                        // pump re-picks this image cheaply once idle.
                        if !self.running && !self.wd14_running {
                            self.autotag_run(ctx, host, key, bytes);
                        }
                    }
                } else if for_emb {
                    #[cfg(feature = "local-npu")]
                    {
                        self.clipemb_pending = None;
                        // Same defer rule as tags: never contend with a generation or Read-tags.
                        if !self.running && !self.wd14_running {
                            self.clipemb_run(ctx, host, key, bytes);
                        }
                    }
                } else if self.gallery_pick_pending.as_ref().is_some_and(|(k, _)| *k == key) {
                    let name = self.gallery_pick_pending.take().map(|(_, n)| n).unwrap_or_default();
                    self.set_picked_input(ctx, name, bytes);
                    self.params.img2img_source = Img2ImgSource::Picked;
                    self.note = "Gallery image set as input".into();
                    host.haptic(Haptic::Light);
                } else if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.tex = Some(ctx.load_texture(&key, image, egui::TextureOptions::LINEAR));
                    v.bytes = Some(bytes);
                    v.loading = false;
                }
            }
            Msg::ItemWorkflow { key, json } => {
                let mut open_sheet = false;
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
                    let empty = meta.is_empty();
                    v.meta = Some(meta);
                    v.workflow_json = Some(json);
                    v.item.has_workflow = true;
                    v.meta_loading = false;
                    open_sheet = self.viewer_remix_pending && !empty;
                }
                if open_sheet {
                    self.viewer_remix_pending = false;
                    self.gallery_status.clear();
                    if let Some(meta) = self.viewer.as_ref().and_then(|v| v.meta.clone()) {
                        self.open_remix_sheet(meta);
                    }
                    host.haptic(Haptic::Light);
                }
            }
            Msg::ItemWorkflowError { key, error } => {
                let mut for_current = false;
                if let Some(v) = &mut self.viewer
                    && v.item.key() == key
                {
                    v.meta_loading = false;
                    for_current = true;
                    self.log.warn(format!("workflow meta {key}: {error}"));
                }
                if for_current && self.viewer_remix_pending {
                    self.viewer_remix_pending = false;
                    self.gallery_status = "No workflow metadata to remix".into();
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
        #[cfg(feature = "local-npu")]
        if self.route_local_gen() {
            if self.params.mode != Mode::Txt2Img {
                return Err("Local NPU is txt2img only for now");
            }
            if self.selected_pack().is_none() {
                return Err("No local model pack for this backend — check Settings -> Local NPU");
            }
            return Ok(());
        }
        if self.params.mode == Mode::Video {
            return self.can_queue_video();
        }
        if let Some(missing) = self.params.missing_model_part() {
            return Err(missing);
        }
        if self.params.mode == Mode::Img2Img
            && self.params.img2img_source == Img2ImgSource::Picked
            && self.picked_input.is_none()
        {
            return Err("Pick a device photo for img2img first");
        }
        if self.params.mode == Mode::Img2Img
            && self.params.inpaint_mask
            && (self.params.img2img_source != Img2ImgSource::Picked || self.picked_input.is_none())
        {
            return Err("Re-apply the inpaint mask first");
        }
        if !matches!(self.conn, Conn::Connected)
            && !self.engine.as_ref().is_some_and(|e| e.is_connected())
        {
            return Err("Connect to the server first");
        }
        Ok(())
    }

    /// Video-mode preflight: the Wan nodes must exist, the models must be chosen, and a selected
    /// device-photo source must have a photo.
    fn can_queue_video(&self) -> Result<(), &'static str> {
        if let Some(schemas) = self.schemas.as_ref() {
            if !schemas.has_node("WanImageToVideo") {
                return Err("This server has no WanImageToVideo node");
            }
            if !schemas.has_node("VHS_VideoCombine") {
                return Err("This server has no VHS_VideoCombine (VideoHelperSuite) node");
            }
        }
        let v = &self.params.video;
        if v.unet_high.trim().is_empty() || v.unet_low.trim().is_empty() {
            return Err("Pick the high and low noise Wan models");
        }
        if v.clip_name.trim().is_empty() {
            return Err("Pick a text encoder for Wan");
        }
        if v.vae_name.trim().is_empty() {
            return Err("Pick a VAE for Wan");
        }
        if !v.video_t2v
            && self.params.img2img_source == Img2ImgSource::Picked
            && self.picked_input.is_none()
        {
            return Err("Pick a device photo for the start image first");
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
        // Record the queued prompt pair; the dedupe collapses a variant/neighbor loop to one entry.
        push_prompt_hist(
            &mut self.prompt_history,
            PromptHist { positive: self.params.positive.clone(), negative: self.params.negative.clone() },
        );
        if self.params.randomize_seed {
            self.params.seed = random_seed();
        }
        // Record once per user queue action: variant/neighbor loops re-enter with running already set.
        if !self.running {
            self.observe_prompt_cooc(host);
        }

        #[cfg(feature = "local-npu")]
        if self.route_local_gen() {
            self.start_local_npu_generation(host);
            return;
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
        // Picked bytes live outside Params; other sources pass the last result (Url ignores it).
        let current = match self.params.img2img_source {
            Img2ImgSource::Picked => self.picked_input.as_ref().map(|p| p.bytes.clone()),
            _ => self.result_bytes.clone(),
        };
        let gcx = self.gen_ctx();
        self.enhance_note.clear();
        // Export the UI workflow from the linked graph so SaveImage can embed it in the PNG.
        let ui_workflow = self.schemas.as_ref().and_then(|schemas| {
            self.create_graph_id
                .and_then(|id| self.graph_tabs.iter().find(|d| d.id == id))
                .map(|doc| doc.view.export_ui(&doc.graph, schemas, &doc.bypassed, &doc.seed_randomize))
        });
        let label = {
            let p = self.params.positive.trim();
            if p.is_empty() { "Create".to_string() } else { elide(p, 28) }
        };
        self.pending_job_labels.push_back(label);
        self.engine.as_mut().unwrap().generate(params, current, gcx, ui_workflow);
        host.haptic(Haptic::Medium);
    }

    #[cfg(feature = "local-npu")]
    fn start_local_npu_generation(&mut self, host: &Host) {
        let Some(lib_dir) = host.native_lib_dir() else {
            self.status = "Local NPU: nativeLibraryDir unavailable".into();
            host.haptic(Haptic::Warning);
            return;
        };
        self.ensure_local_packs(host, false);
        let Some((model_dir, backend, pack_name)) =
            self.selected_pack().map(|p| (p.dir.clone(), p.backend, p.name.clone()))
        else {
            self.status = format!(
                "Local NPU: no {} pack found — push one to the app files dir, then Refresh in Settings",
                self.local_backend.label()
            );
            self.log.error(self.status.clone());
            host.haptic(Haptic::Warning);
            return;
        };
        let fresh = !self.running;
        if fresh {
            self.progress = (0, 0);
            self.preview = None;
            self.results.clear();
            self.result_view = None;
            self.run_total = 0;
            self.run_seen.clear();
        }
        self.running = true;
        self.jobs_left += 1;
        let what = format!("{} · {pack_name}", backend.label());
        self.status = if !fresh {
            format!("Local {what} queued ({} in flight)", self.jobs_left)
        } else {
            format!("Local {what} queued")
        };
        self.enhance_note.clear();
        let paths = crate::local_engine::LocalPaths {
            lib_dir: std::path::PathBuf::from(lib_dir),
            model_dir,
            backend,
        };
        self.engine.as_mut().unwrap().generate_local(paths, self.params.clone());
        host.haptic(Haptic::Medium);
    }

    /// Queue `queue_variants` Create jobs. With seed randomization off, iterations after the first
    /// re-roll the seed so the variants differ; iteration 0 keeps the user's seed. (With it on,
    /// start_generation already re-rolls per call.)
    fn queue_create_variants(&mut self, ctx: &egui::Context, host: &Host) {
        if let Err(e) = self.can_queue_create() {
            self.status = e.into();
            host.haptic(Haptic::Warning);
            return;
        }
        let n = self.queue_variants.clamp(1, 8);
        let base_seed = self.params.seed;
        let restore_seed = !self.params.randomize_seed;
        for i in 0..n {
            if i > 0 && !self.params.randomize_seed {
                self.params.seed = random_seed();
            }
            self.start_generation(ctx, host);
        }
        // Restore the user's seed; each variant already captured its own.
        if restore_seed {
            self.params.seed = base_seed;
        }
    }

    fn queue_graph(&mut self, ctx: &egui::Context, host: &Host) {
        let Some(schemas) = self.schemas.clone() else {
            self.graph_status = "Connect to the server first".into();
            return;
        };
        let Some(doc) = self.active_doc_mut() else { return };
        // Roll seeds marked randomize before export (ComfyUI control_after_generate client-side).
        graphview::apply_pending_seed_rolls(&mut doc.graph.snarl, &doc.seed_randomize);
        // UI export + convert respects bypass (mode 4); the snarl API path does not.
        let ui_json = doc.view.export_ui(&doc.graph, &schemas, &doc.bypassed, &doc.seed_randomize);
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
        let fresh = !self.running;
        self.run_seq += 1;
        if fresh {
            self.run_total = n;
            self.run_seen.clear();
            ctx.forget_all_images();
            self.progress = (0, 0);
            self.preview = None;
            self.executing = None;
        }
        self.running = true;
        self.jobs_left += 1;
        self.status = if self.jobs_left > 1 {
            format!("Queued ({} in flight)", self.jobs_left)
        } else {
            "Queued".into()
        };
        self.graph_status.clear();
        self.pending_job_labels.push_back("Graph".into());
        let mut wf = wf;
        if let Some(schemas) = self.schemas.as_deref() {
            for n in crate::workflow::sanitize_clip_types(&mut wf, schemas) {
                self.log.info(format!("repair: {n}"));
            }
        }
        self.engine.as_mut().unwrap().run_workflow(wf, Some(ui_json));
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
        let wants_input = self.params.mode == Mode::Img2Img
            || (self.params.mode == Mode::Video && !self.params.video.video_t2v);
        let input = wants_input.then(|| crate::engine::INPUT_IMAGE_NAME.to_string());
        let placeholder = input.is_some();
        let (wf, _, report) =
            crate::workflow::build_dispatch(&self.params, input, &self.apps, &schemas);
        self.enhance_note = report.note();
        self.executing = None;

        let linked = self
            .create_graph_id
            .and_then(|id| self.graph_tabs.iter().position(|d| d.id == id));
        let result = if let Some(idx) = linked {
            self.active_graph = idx;
            self.replace_workflow_in_tab_with_seeds(
                idx,
                "create.json".into(),
                &wf,
                &Default::default(),
                Some(self.params.randomize_seed),
            )
        } else {
            self.load_workflow_into_tab_with_seeds(
                "create.json".into(),
                &wf,
                &Default::default(),
                Some(self.params.randomize_seed),
            )
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
        let wants_input = self.params.mode == Mode::Img2Img
            || (self.params.mode == Mode::Video && !self.params.video.video_t2v);
        let input = wants_input.then(|| crate::engine::INPUT_IMAGE_NAME.to_string());
        let (wf, _, report) =
            crate::workflow::build_dispatch(&self.params, input, &self.apps, &schemas);
        self.enhance_note = report.note();
        if self
            .replace_workflow_in_tab_with_seeds(
                idx,
                "create.json".into(),
                &wf,
                &Default::default(),
                Some(self.params.randomize_seed),
            )
            .is_err()
        {
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
        let exported = doc.view.export_ui(&doc.graph, schemas, &doc.bypassed, &doc.seed_randomize);
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
        let exported = doc.view.export_ui(&doc.graph, schemas, &doc.bypassed, &doc.seed_randomize);
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

    fn share_bytes(&mut self, host: &Host, bytes: &[u8], name: &str) -> String {
        let Some(dir) = host.documents_dir() else {
            return "No storage directory".into();
        };
        let folder = format!("{dir}/comfyui/share");
        let _ = std::fs::create_dir_all(&folder);
        let path = format!("{folder}/{name}");
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                let mime = if name.to_lowercase().ends_with(".mp4") {
                    "video/mp4"
                } else if name.to_lowercase().ends_with(".webp") {
                    "image/webp"
                } else {
                    "image/png"
                };
                host.share_media(&path, name, mime);
                host.haptic(Haptic::Light);
                "Opening share sheet…".to_string()
            }
            Err(e) => {
                self.log.error(format!("share failed: {e}"));
                format!("Share failed: {e}")
            }
        }
    }

    /// The per-source preview + picker for the selected [`Img2ImgSource`], shared by the img2img
    /// setup block and the Video start-image row.
    fn image_source_preview(&mut self, ui: &mut egui::Ui, host: &Host) {
        match self.params.img2img_source {
            Img2ImgSource::Picked => {
                let mut change = false;
                if let Some(picked) = &self.picked_input {
                    ui.horizontal(|ui| {
                        if let Some(tex) = &picked.tex {
                            let sized = egui::load::SizedTexture::from_handle(tex);
                            ui.add(egui::Image::new(sized).max_size(egui::vec2(96.0, 96.0)));
                        }
                        ui.vertical(|ui| {
                            ui.weak(elide(&picked.name, 28));
                            if ui.button("Change").clicked() {
                                change = true;
                            }
                        });
                    });
                } else {
                    ui.weak("Pick a photo from this device.");
                    self.picked_input_grid_open = true;
                }
                if change {
                    self.picked_input_grid_open = !self.picked_input_grid_open;
                }
                if self.picked_input_grid_open
                    && let Some((id, name)) = self.device_photo_grid(ui, host)
                {
                    match host.load_device_image(id) {
                        Some(bytes) if !bytes.is_empty() => {
                            let fname =
                                if name.is_empty() { format!("device_{id}.jpg") } else { name };
                            self.set_picked_input(ui.ctx(), fname, bytes);
                            host.haptic(Haptic::Light);
                        }
                        _ => {
                            self.note = "Couldn't read that photo from the device".into();
                            host.haptic(Haptic::Error);
                        }
                    }
                }
            }
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
    }

    /// Decode picked photo bytes into a preview texture and stash them as the img2img input.
    fn set_picked_input(&mut self, ctx: &egui::Context, name: String, bytes: Vec<u8>) {
        let tex = crate::engine::decode(&bytes)
            .map(|ci| ctx.load_texture("picked_input", ci, egui::TextureOptions::LINEAR));
        self.picked_input = Some(PickedInput { name, bytes, tex });
        self.picked_input_grid_open = false;
        // A plain pick carries no mask; inpaint re-sets the flag after this call.
        self.params.inpaint_mask = false;
    }

    /// Decode `bytes`, build the base texture and a half-res mask canvas, and open the overlay.
    fn open_inpaint(&mut self, ctx: &egui::Context, bytes: Vec<u8>, name: String) {
        let Some(ci) = crate::engine::decode(&bytes) else {
            self.note = "Couldn't open that image for inpainting".into();
            return;
        };
        let [iw, ih] = [ci.size[0] as u32, ci.size[1] as u32];
        let base_tex = ctx.load_texture("inpaint_base", ci, egui::TextureOptions::LINEAR);
        // Canvas at ~half image resolution, long side capped at 1024.
        let long = iw.max(ih).max(1);
        let target = (long / 2).clamp(1, 1024);
        let scale = target as f32 / long as f32;
        let cw = ((iw as f32 * scale).round() as u32).max(1);
        let ch = ((ih as f32 * scale).round() as u32).max(1);
        self.inpaint = Some(InpaintState {
            source_bytes: bytes,
            source_name: name,
            img_size: [iw.max(1), ih.max(1)],
            base_tex,
            strokes: Vec::new(),
            groups: Vec::new(),
            canvas: mask::MaskCanvas::new(cw, ch),
            brush_uv: 0.06,
            erase: false,
            overlay_tex: None,
            overlay_dirty: true,
            stylus_only: false,
            input_inited: false,
            show_debug: false,
            dbg_force: None,
            dbg_contact: false,
            dbg_saw_touch: false,
            brush_preview_until: 0.0,
            view: mask::ViewXform::FIT,
            stroke_active: false,
            nav_latch: false,
        });
    }

    /// Bake the current mask into the source alpha and route it to Create as a masked img2img input.
    fn apply_inpaint_mask(&mut self, ctx: &egui::Context, host: &Host) {
        let baked = {
            let Some(st) = self.inpaint.as_ref() else { return };
            if st.canvas.is_empty() {
                return;
            }
            let base = file_basename(&st.source_name);
            let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
            let name = format!("inpaint_{stem}.png");
            mask::bake_alpha(&st.source_bytes, &st.canvas).map(|png| (png, name))
        };
        match baked {
            Ok((png, name)) => {
                self.set_picked_input(ctx, name, png);
                self.params.mode = Mode::Img2Img;
                self.params.img2img_source = Img2ImgSource::Picked;
                self.params.inpaint_mask = true;
                if self.params.denoise >= 0.999 {
                    self.params.denoise = 0.85;
                }
                self.inpaint = None;
                self.tab = Tab::Generate;
                self.note = "Masked image set for inpainting".into();
                host.haptic(Haptic::Success);
            }
            Err(e) => {
                self.note = format!("Inpaint bake failed: {e}");
                host.haptic(Haptic::Error);
            }
        }
    }

    /// Full-screen brush overlay: paint the mask over the fitted base image, then bake it.
    fn inpaint_overlay(&mut self, ui: &mut egui::Ui, host: &Host) {
        if ui.ctx().input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
        }) {
            self.inpaint = None;
            return;
        }

        if let Some(st) = self.inpaint.as_mut()
            && !st.input_inited
        {
            st.stylus_only = host.has_stylus();
            st.input_inited = true;
        }

        let mut close = false;
        let mut use_mask = false;
        let mut do_undo = false;
        let mut do_clear = false;

        ui.horizontal(|ui| {
            let st = self.inpaint.as_ref().unwrap();
            ui.label(format!("{} Fix area", icons::MODEL));
            ui.weak(elide(&st.source_name, 32));
        });
        ui.separator();

        let empty = self.inpaint.as_ref().unwrap().canvas.is_empty();
        egui::Panel::bottom("inpaint-actions").show(ui, |ui| {
            const BTN_H: f32 = 36.0;
            const ICON_W: f32 = 40.0;
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let st = self.inpaint.as_mut().unwrap();
                if ui
                    .add(egui::Button::new(icons::CLOSE).min_size(egui::vec2(ICON_W, BTN_H)))
                    .on_hover_text("Cancel and discard")
                    .clicked()
                {
                    close = true;
                }
                if ui
                    .add(egui::Button::new(icons::TRASH).min_size(egui::vec2(ICON_W, BTN_H)))
                    .on_hover_text("Clear mask")
                    .clicked()
                {
                    do_clear = true;
                }
                ui.label("Brush");
                let slider_w = (ui.available_width() - 120.0).max(80.0);
                let brush_resp = ui.add_sized(
                    [slider_w, BTN_H],
                    egui::Slider::new(&mut st.brush_uv, 0.01..=0.15).show_value(false),
                );
                if brush_resp.changed() || brush_resp.dragged() {
                    st.brush_preview_until = ui.input(|i| i.time) + 0.9;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !empty,
                            egui::Button::new(format!("{} Use mask", icons::CHECK))
                                .min_size(egui::vec2(0.0, BTN_H)),
                        )
                        .on_hover_text("Bake the mask and send to Create")
                        .clicked()
                    {
                        use_mask = true;
                    }
                });
            });
            ui.add_space(2.0);
        });

        {
            let st = self.inpaint.as_mut().unwrap();
            let [iw, ih] = st.img_size;
            let ar = iw as f32 / ih as f32;
            let area = ui.available_rect_before_wrap();
            // Aspect-fit the image inside the area left below the header and toolbar.
            let fitted = {
                let (aw, ah) = (area.width().max(1.0), area.height().max(1.0));
                let (w, h) = if aw / ah > ar { (ah * ar, ah) } else { (aw, aw / ar) };
                egui::Rect::from_center_size(area.center(), egui::vec2(w, h))
            };

            // Icon-only tool stack on the right: undo, erase, stylus, debug.
            let stack_pos =
                egui::pos2(area.right() - crate::theme::FAB_EDGE, area.top() + 10.0);
            egui::Area::new(egui::Id::new("inpaint-tool-stack"))
                .order(egui::Order::Foreground)
                .fixed_pos(stack_pos)
                .show(ui.ctx(), |aui| {
                    aui.spacing_mut().item_spacing.y = 8.0;
                    aui.vertical(|aui| {
                        if crate::theme::fab(aui, icons::UNDO, crate::theme::fab_bg())
                            .on_hover_text("Undo last stroke")
                            .clicked()
                        {
                            do_undo = true;
                        }
                        if st.view.zoom > 1.001
                            && crate::theme::fab(aui, icons::FULLSCREEN, crate::theme::fab_bg())
                                .on_hover_text("Reset zoom to fit")
                                .clicked()
                        {
                            st.view = mask::ViewXform::FIT;
                        }
                        for (on_flag, icon, tip) in [
                            (&mut st.erase, icons::ERASE, "Wipe mask instead of painting"),
                            (
                                &mut st.stylus_only,
                                icons::STYLUS,
                                "Accept only stylus strokes so you can rest your palm",
                            ),
                            (&mut st.show_debug, icons::BUG, "Show live S-Pen telemetry over the canvas"),
                        ] {
                            let fill = if *on_flag {
                                crate::theme::fab_bg_on()
                            } else {
                                crate::theme::fab_bg()
                            };
                            if crate::theme::fab(aui, icon, fill).on_hover_text(tip).clicked() {
                                *on_flag = !*on_flag;
                            }
                        }
                    });
                });
            let (resp, painter) =
                ui.allocate_painter(area.size(), egui::Sense::click_and_drag());
            let fit = (fitted.center().x, fitted.center().y, fitted.width(), fitted.height());
            let area_size = (area.width(), area.height());
            // Two-finger pinch/pan navigates and never paints; a second finger cancels the stroke
            // its first finger may have started in the frames before it registered.
            let mt = ui.input(|i| i.multi_touch());
            let any_down = ui.input(|i| i.pointer.any_down());
            if let Some(mt) = &mt {
                if st.stroke_active {
                    if let Some(start) = st.groups.pop() {
                        st.strokes.truncate(start);
                        st.canvas = mask::rasterize(st.canvas.w, st.canvas.h, &st.strokes);
                        st.overlay_dirty = true;
                    }
                    st.stroke_active = false;
                }
                st.nav_latch = true;
                st.view = st.view.pinch(
                    fit,
                    area_size,
                    mt.zoom_delta,
                    (mt.center_pos.x, mt.center_pos.y),
                    (mt.translation_delta.x, mt.translation_delta.y),
                );
            }
            if st.nav_latch && !any_down {
                st.nav_latch = false;
            }
            // Desktop: ctrl+scroll (folded into zoom_delta) zooms about the pointer when not pinching.
            if mt.is_none() {
                let zd = ui.input(|i| i.zoom_delta());
                if (zd - 1.0).abs() > 1e-3 {
                    let f = resp.hover_pos().unwrap_or(fitted.center());
                    st.view = st.view.pinch(fit, area_size, zd, (f.x, f.y), (0.0, 0.0));
                }
            }
            if resp.double_clicked() {
                st.view = mask::ViewXform::FIT;
            }
            let view = {
                let (mx, my, w, h) = st.view.view_rect(fit);
                egui::Rect::from_min_size(egui::pos2(mx, my), egui::vec2(w, h))
            };
            let uv_full = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            painter.image(st.base_tex.id(), view, uv_full, egui::Color32::WHITE);

            let soft = 0.5;
            // Latest touch force + contact this frame; mouse/desktop reports no touch events.
            let (force, contact, saw_touch) = ui.input(|i| {
                let mut force = None;
                let mut contact = false;
                let mut saw = false;
                for e in &i.events {
                    if let egui::Event::Touch { phase, force: f, .. } = e {
                        saw = true;
                        if matches!(phase, egui::TouchPhase::Start | egui::TouchPhase::Move) {
                            contact = true;
                            force = *f;
                        }
                    }
                }
                (force, contact, saw)
            });
            if saw_touch {
                st.dbg_force = force;
            }
            st.dbg_contact = contact;
            st.dbg_saw_touch = saw_touch;
            // Live S-Pen state from the android-activity input side channel (tool type, hover, and
            // buttons that winit drops); hover px converts to egui points.
            let (tool_u8, hover_px, buttons) = host.stylus_probe();
            let kind = match tool_u8 {
                1 => mask::PointerKind::Finger,
                2 => mask::PointerKind::Stylus,
                3 => mask::PointerKind::Mouse,
                4 => mask::PointerKind::Eraser,
                5 => mask::PointerKind::Palm,
                _ => mask::PointerKind::Unknown,
            };
            let ppp = ui.ctx().pixels_per_point();
            let hover_pt = hover_px.map(|(x, y)| egui::pos2(x / ppp, y / ppp));
            // Stylus barrel button (primary 0x20 / secondary 0x40) or the flipped eraser tip erases.
            let btn_erase = buttons & 0x60 != 0;
            let erase = st.erase || btn_erase || matches!(kind, mask::PointerKind::Eraser);
            // Brush radius shrinks with zoom so its screen-pixel size stays constant.
            let brush = mask::pressure_brush(st.brush_uv / st.view.zoom, force);
            let can_paint = mt.is_none() && !st.nav_latch;
            if can_paint
                && resp.dragged()
                && mask::accept_paint(kind, st.stylus_only)
                && let Some(pos) = resp.interact_pointer_pos()
                && view.width() > 0.0
                && view.height() > 0.0
            {
                if !st.stroke_active {
                    st.groups.push(st.strokes.len());
                    st.stroke_active = true;
                }
                let to_uv = |p: egui::Pos2| {
                    (
                        ((p.x - view.left()) / view.width()).clamp(0.0, 1.0),
                        ((p.y - view.top()) / view.height()).clamp(0.0, 1.0),
                    )
                };
                let cur = to_uv(pos);
                let d = resp.drag_delta();
                let prev = (
                    (cur.0 - d.x / view.width()).clamp(0.0, 1.0),
                    (cur.1 - d.y / view.height()).clamp(0.0, 1.0),
                );
                st.canvas.stroke(prev, cur, brush.radius_uv, soft, brush.intensity, erase);
                st.strokes.push(mask::StrokeRec {
                    from: prev,
                    to: cur,
                    radius_uv: brush.radius_uv,
                    soft,
                    intensity: brush.intensity,
                    erase,
                });
                st.overlay_dirty = true;
            }
            if !resp.dragged() {
                st.stroke_active = false;
            }

            if st.overlay_dirty {
                let (mw, mh) = (st.canvas.w as usize, st.canvas.h as usize);
                let mut px = vec![0u8; mw * mh * 4];
                for (i, &m) in st.canvas.buf.iter().enumerate() {
                    if m > 0 {
                        let o = i * 4;
                        px[o] = 220;
                        px[o + 1] = 40;
                        px[o + 2] = 40;
                        px[o + 3] = m / 2;
                    }
                }
                let ci = egui::ColorImage::from_rgba_unmultiplied([mw, mh], &px);
                st.overlay_tex =
                    Some(ui.ctx().load_texture("inpaint_overlay", ci, egui::TextureOptions::LINEAR));
                st.overlay_dirty = false;
            }
            if let Some(tex) = &st.overlay_tex {
                painter.image(tex.id(), view, uv_full, egui::Color32::WHITE);
            }

            // Brush-size cursor ring at the pointer or stylus hover; reflects pressure and erase.
            let brush_col = if erase {
                egui::Color32::from_rgb(90, 170, 255)
            } else {
                egui::Color32::from_rgb(230, 70, 70)
            };
            if let Some(pos) = resp.interact_pointer_pos().or(resp.hover_pos()).or(hover_pt)
                && view.contains(pos)
            {
                let eff = if force.is_some() { brush.radius_uv } else { st.brush_uv / st.view.zoom };
                let r = (eff * view.size().min_elem()).max(2.0);
                painter.circle_stroke(pos, r, egui::Stroke::new(1.5, brush_col));
            }

            // Center preview while scrubbing the brush slider (and briefly after).
            let now = ui.input(|i| i.time);
            if now < st.brush_preview_until {
                let r = (st.brush_uv * fitted.size().min_elem()).max(2.0);
                let c = fitted.center();
                painter.circle_filled(c, r, brush_col.gamma_multiply(0.25));
                painter.circle_stroke(c, r, egui::Stroke::new(2.0, brush_col));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            }

            if st.show_debug {
                let tool = match kind {
                    mask::PointerKind::Finger => "finger",
                    mask::PointerKind::Stylus => "stylus",
                    mask::PointerKind::Eraser => "eraser",
                    mask::PointerKind::Palm => "palm",
                    mask::PointerKind::Mouse => "mouse",
                    mask::PointerKind::Unknown => "unknown",
                };
                let force_s = st.dbg_force.map(|f| format!("{f:.2}")).unwrap_or_else(|| "-".into());
                let state = if st.dbg_contact {
                    "contact"
                } else if hover_pt.is_some() {
                    "hover"
                } else if st.dbg_saw_touch {
                    "release"
                } else {
                    "idle"
                };
                let hover_s =
                    hover_pt.map(|p| format!("{:.0},{:.0}", p.x, p.y)).unwrap_or_else(|| "-".into());
                let text = format!(
                    "S-Pen telemetry\ntool: {tool}\nforce: {force_s}\nstate: {state}\nhover: {hover_s}\nbuttons: 0x{buttons:02x}\nerase: {erase}\nbrush r/i: {:.3}/{:.2}\nstylus-only: {}",
                    brush.radius_uv, brush.intensity, st.stylus_only,
                );
                let galley =
                    painter.layout(text, egui::FontId::monospace(12.0), egui::Color32::WHITE, 240.0);
                let pad = egui::vec2(6.0, 5.0);
                let origin = area.left_top() + egui::vec2(8.0, 8.0);
                let bg = egui::Rect::from_min_size(origin, galley.size() + pad * 2.0);
                painter.rect_filled(bg, 4.0, egui::Color32::from_black_alpha(180));
                painter.galley(origin + pad, galley, egui::Color32::WHITE);
            }
        }

        if close {
            self.inpaint = None;
            return;
        }
        if do_undo {
            if let Some(st) = &mut self.inpaint
                && let Some(start) = st.groups.pop()
            {
                st.strokes.truncate(start);
                st.canvas = mask::rasterize(st.canvas.w, st.canvas.h, &st.strokes);
                st.overlay_dirty = true;
                host.haptic(Haptic::Light);
            }
        }
        if do_clear {
            if let Some(st) = &mut self.inpaint {
                st.strokes.clear();
                st.groups.clear();
                st.canvas = mask::MaskCanvas::new(st.canvas.w, st.canvas.h);
                st.overlay_dirty = true;
            }
        }
        if use_mask {
            self.apply_inpaint_mask(ui.ctx(), host);
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

    /// Mirror under the app external files dir (same tree as model packs; `adb pull`-able).
    fn settings_backup_path(host: &Host) -> Option<String> {
        let docs = host.documents_dir()?;
        let pkg = std::path::Path::new(&docs).parent()?.file_name()?.to_str()?;
        Some(format!("/storage/emulated/0/Android/data/{pkg}/files/comfyui_settings.json"))
    }

    /// Internal file first, then the external mirror.
    fn settings_candidates(host: &Host) -> Vec<String> {
        [Self::settings_path(host), Self::settings_backup_path(host)]
            .into_iter()
            .flatten()
            .collect()
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
            server_output_root: self.server_output_root.clone(),
            username: self.username.clone(),
            session: self.session.clone(),
            params: self.params.clone(),
            gallery: self.gallery_view.clone(),
            // Search box is session-only (especially semantic queries).
            gallery_q: String::new(),
            gallery_semantic: self.gallery_semantic,
            gallery_page: self.gallery_page_size(),
            auto_follow: self.auto_follow,
            auto_arrange: self.auto_arrange,
            fonts: self.fonts.clone(),
            workflow_name,
            workflow_json,
            presets: self.presets.clone(),
            selected_preset: self.selected_preset.clone(),
            characters: self.characters.clone(),
            active_character: self.active_character.clone(),
            checkpoint_sort: self.checkpoint_sort,
            checkpoint_favorites: self.checkpoint_favorites.clone(),
            checkpoint_recent: self.checkpoint_recent.clone(),
            confirm_gallery_delete: self.confirm_gallery_delete,
            create_setup_open: self.create_setup_open,
            create_companions_open: self.create_companions_open,
            #[cfg(feature = "local-npu")]
            local_npu: self.local_npu,
            #[cfg(not(feature = "local-npu"))]
            local_npu: false,
            #[cfg(feature = "local-npu")]
            auto_tag: self.auto_tag,
            #[cfg(not(feature = "local-npu"))]
            auto_tag: false,
            cache_prefetch: self.cache_prefetch,
            #[cfg(feature = "local-npu")]
            local_backend: self.local_backend,
            #[cfg(not(feature = "local-npu"))]
            local_backend: Default::default(),
            #[cfg(feature = "local-npu")]
            local_pack: self.local_pack.clone(),
            #[cfg(not(feature = "local-npu"))]
            local_pack: String::new(),
            #[cfg(feature = "local-npu")]
            local_use_server: self.local_use_server,
            #[cfg(not(feature = "local-npu"))]
            local_use_server: false,
            prompt_history: self.prompt_history.clone(),
            character_denied: self.character_denied.clone(),
            character_suggestions: self.character_suggestions.clone(),
        };
        serde_json::to_string_pretty(&settings).ok()
    }

    /// Active graph as UI-format JSON for persistence, when a schema-backed editor exists.
    fn snapshot_workflow(&self) -> (String, Option<String>) {
        if let (Some(doc), Some(schemas)) = (self.active_doc(), self.schemas.as_ref()) {
            if !doc.is_empty() {
                let exported = doc.view.export_ui(&doc.graph, schemas, &doc.bypassed, &doc.seed_randomize);
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

    fn apply_saved_settings(&mut self, saved: Settings) {
        self.server_url = saved.server_url;
        self.api_key = saved.api_key;
        self.server_output_root = saved.server_output_root;
        self.username = saved.username;
        self.session = saved.session;
        self.params = saved.params;
        // Picked device-photo bytes are session-only; fall back to the current result on restore.
        if self.params.img2img_source == Img2ImgSource::Picked {
            self.params.img2img_source = Img2ImgSource::CurrentOutput;
        }
        // The masked photo went with the Picked bytes, so drop the flag on restore too.
        self.params.inpaint_mask = false;
        self.gallery_view = saved.gallery;
        // gallery_q is session-only; ignore any legacy persisted value.
        self.gallery_semantic = saved.gallery_semantic;
        self.gallery_page = saved.gallery_page.clamp(20, GALLERY_PAGE_MAX);
        self.auto_follow = saved.auto_follow;
        self.auto_arrange = saved.auto_arrange;
        self.fonts = saved.fonts;
        self.fonts.clamp();
        self.gallery_view.columns = self.gallery_view.columns.clamp(1, 3);
        self.presets = saved.presets;
        self.selected_preset = saved.selected_preset;
        self.characters = saved.characters;
        self.active_character = saved.active_character;
        self.character_denied = saved.character_denied;
        self.character_suggestions = saved.character_suggestions;
        self.character_centroids.clear();
        self.checkpoint_sort = saved.checkpoint_sort;
        self.checkpoint_favorites = saved.checkpoint_favorites;
        self.checkpoint_recent = saved.checkpoint_recent;
        self.confirm_gallery_delete = saved.confirm_gallery_delete;
        self.create_setup_open = saved.create_setup_open;
        self.create_companions_open = saved.create_companions_open;
        self.prompt_history = saved.prompt_history;
        self.cache_prefetch = saved.cache_prefetch;
        #[cfg(feature = "local-npu")]
        {
            self.local_npu = saved.local_npu;
            self.auto_tag = saved.auto_tag;
            self.local_backend = saved.local_backend;
            self.local_pack = saved.local_pack;
            self.local_use_server = saved.local_use_server;
        }
        if let Some(json) = saved.workflow_json.filter(|s| !s.trim().is_empty()) {
            self.restore_workflow = Some((saved.workflow_name, json));
        }
        self.settings_write_blocked = None;
        self.last_saved = self.settings_json();
    }

    fn load_settings(&mut self, host: &Host) {
        let apps = AppSet::load(host.documents_dir().as_deref());
        for (file, why) in &apps.bad {
            self.log.error(format!("app '{file}': {why}"));
        }
        self.log.info(format!("{} enhance app(s) loaded", apps.by_id.len()));
        self.apps = Arc::new(apps);
        #[cfg(feature = "local-npu")]
        self.ensure_local_packs(host, false);
        self.refresh_backup_list(host);

        let candidates = Self::settings_candidates(host);
        if candidates.is_empty() {
            return;
        }

        let mut saw_unreadable = false;
        for path in &candidates {
            let text = match std::fs::read_to_string(path) {
                Ok(t) if !t.trim().is_empty() => t,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    saw_unreadable = true;
                    self.log.error(format!("settings: cannot read {path}: {e}"));
                    continue;
                }
            };
            match serde_json::from_str::<Settings>(&text) {
                Ok(saved) => {
                    self.log.info(format!("settings: loaded from {path}"));
                    self.apply_saved_settings(saved);
                    return;
                }
                Err(e) => {
                    saw_unreadable = true;
                    self.log.error(format!(
                        "settings: parse failed for {path}: {e} — refusing to overwrite"
                    ));
                }
            }
        }

        // A corrupt on-disk file must not be replaced by empty defaults on the next autosave.
        if saw_unreadable {
            let msg = "settings file present but unreadable — autosave paused until you fix or delete it"
                .to_string();
            self.settings_write_blocked = Some(msg.clone());
            self.log.error(msg);
        }
    }

    /// Persist settings whenever they differ from the last write, checked at most once a second.
    /// Writes the internal file and an external mirror; never clobbers a corrupt on-disk file.
    fn autosave_settings(&mut self, ctx: &egui::Context, host: &Host) {
        let now = ctx.input(|i| i.time);
        if now - self.last_save_check < 1.0 {
            return;
        }
        self.last_save_check = now;
        if self.settings_write_blocked.is_some() {
            return;
        }
        let Some(json) = self.settings_json() else { return };
        if self.last_saved.as_deref() == Some(&json) {
            return;
        }
        let paths = Self::settings_candidates(host);
        if paths.is_empty() {
            return;
        }
        let mut wrote = false;
        for path in &paths {
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(path, &json).is_ok() {
                wrote = true;
            }
        }
        if wrote {
            self.last_saved = Some(json);
        }
    }

    /// Public durable model root: survives app uninstall (unlike Android/data/<pkg>/files).
    fn durable_models_dir() -> &'static str {
        "/storage/emulated/0/ComfyUI"
    }

    fn refresh_backup_list(&mut self, host: &Host) {
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        if let Some(d) = host.documents_dir() {
            dirs.push(d.into());
        }
        if let Some(p) = Self::settings_backup_path(host) {
            if let Some(parent) = std::path::Path::new(&p).parent() {
                dirs.push(parent.to_path_buf());
            }
        }
        dirs.push(Self::durable_models_dir().into());
        self.backup_list = crate::backup::list_backups(&dirs);
        self.backup_note = format!("{} backup(s) found", self.backup_list.len());
    }

    fn export_encrypted_backup(&mut self, host: &Host) {
        if self.backup_pass != self.backup_pass_confirm {
            self.backup_note = "passphrases do not match".into();
            host.haptic(Haptic::Warning);
            return;
        }
        let Some(settings) = self.settings_json().and_then(|j| serde_json::from_str(&j).ok()) else {
            self.backup_note = "could not snapshot settings".into();
            return;
        };
        let blob = match crate::backup::encrypt(&settings, &self.backup_pass) {
            Ok(b) => b,
            Err(e) => {
                self.backup_note = e;
                host.haptic(Haptic::Warning);
                return;
            }
        };
        let name = crate::backup::default_filename();
        let mut written: Vec<String> = Vec::new();
        for dir in [
            host.documents_dir(),
            Self::settings_backup_path(host)
                .and_then(|p| std::path::Path::new(&p).parent().map(|d| d.display().to_string())),
            Some(Self::durable_models_dir().into()),
        ]
        .into_iter()
        .flatten()
        {
            let path = format!("{dir}/{name}");
            if let Some(parent) = std::path::Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&path, &blob).is_ok() {
                written.push(path);
            }
        }
        if written.is_empty() {
            self.backup_note = "failed to write backup file".into();
            host.haptic(Haptic::Warning);
            return;
        }
        self.backup_pass.clear();
        self.backup_pass_confirm.clear();
        self.backup_note = format!("wrote {} — use Share to copy off-device", written[0]);
        host.share_file(written[0].clone());
        self.refresh_backup_list(host);
        host.haptic(Haptic::Success);
        self.log.info(format!("backup: exported {}", written.join(", ")));
    }

    fn import_encrypted_backup(&mut self, host: &Host, path: &str) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.backup_note = format!("read {path}: {e}");
                host.haptic(Haptic::Warning);
                return;
            }
        };
        match crate::backup::decrypt(&bytes, &self.import_pass) {
            Ok(saved) => {
                let n_chars = saved.characters.len();
                let n_presets = saved.presets.len();
                self.apply_saved_settings(saved);
                self.import_pass.clear();
                self.last_saved = None;
                self.autosave_settings_now(host);
                self.backup_note = format!(
                    "imported {n_chars} character(s), {n_presets} preset(s), credentials restored"
                );
                host.haptic(Haptic::Success);
                self.log.info(format!("backup: imported from {path}"));
                if !self.server_url.trim().is_empty() {
                    self.connect(host);
                }
            }
            Err(e) => {
                self.backup_note = e;
                host.haptic(Haptic::Warning);
            }
        }
    }

    /// Immediate dual-write after import (bypasses the 1s autosave throttle).
    fn autosave_settings_now(&mut self, host: &Host) {
        self.settings_write_blocked = None;
        let Some(json) = self.settings_json() else { return };
        for path in Self::settings_candidates(host) {
            if let Some(parent) = std::path::Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, &json);
        }
        self.last_saved = Some(json);
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
        self.scroll_focus_into_view(ui);
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
                ui.add_space(6.0);
                ui.label("Server output root");
                ui.add(
                    egui::TextEdit::singleline(&mut self.server_output_root)
                        .hint_text("/data/output/")
                        .desired_width(f32::INFINITY),
                );
                ui.weak("Container path of ComfyUI's output dir — used to finish gallery videos.");
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
            #[cfg(feature = "local-npu")]
            {
                self.ensure_local_packs(host, false);
                ui.add_space(12.0);
                ui.heading("Local NPU");
                ui.group(|ui| {
                    ui.checkbox(&mut self.local_npu, "Local NPU features (experimental)");
                    ui.weak(
                        "Enables the on-device stack: auto-tag, CLIP embeds, Read tags and Rewrite. \
                         Create generation runs on-device only when a local pack is the chosen \
                         model; pick 'Server model' to generate on the server with these features \
                         still on. SD1.5 packs are 512² (unet.bin / vae_decoder.bin / \
                         tokenizer.json / clip.safetensors); Anima packs are 1024² with an ANIMA \
                         marker file.",
                    );
                    ui.add_space(6.0);
                    ui.weak("Pick the Create model (a pack or Server model), test it, or import a pack in Create -> Models.");
                    ui.add_space(6.0);
                    ui.checkbox(&mut self.auto_tag, "Auto-tag gallery (NPU)");
                    ui.weak(
                        "Tags the whole server gallery on-device while idle; results power the \
                         gallery tag search, facet chips and rating filter. Pauses during \
                         generation. Needs a wd14 pack.",
                    );
                    ui.add_space(4.0);
                    if ui.button("Unload NPU cache").clicked() {
                        crate::local_engine::drop_cache();
                        self.log.info("local-npu: asset caches dropped");
                    }
                });
                self.local_pack_status_panel(ui, host);
            }

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
                ui.add_space(8.0);
                self.gallery_cache_settings(ui, host);
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
            ui.heading(format!("{} Backup", icons::SAVE));
            ui.group(|ui| {
                ui.weak("Encrypts server URL, API key, session, characters, presets, and Create settings.");
                ui.weak("Password is never included. Share the .comfybk file off-device before reinstall.");
                ui.label("Export passphrase");
                ui.add(
                    egui::TextEdit::singleline(&mut self.backup_pass)
                        .password(true)
                        .hint_text("min 8 chars")
                        .desired_width(f32::INFINITY),
                );
                ui.label("Confirm");
                ui.add(
                    egui::TextEdit::singleline(&mut self.backup_pass_confirm)
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    if ui.button("Export encrypted backup").clicked() {
                        self.export_encrypted_backup(host);
                    }
                    if ui.button("Refresh list").clicked() {
                        self.refresh_backup_list(host);
                    }
                });
                ui.add_space(6.0);
                ui.label("Import passphrase");
                ui.add(
                    egui::TextEdit::singleline(&mut self.import_pass)
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                if self.backup_list.is_empty() {
                    ui.weak("No .comfybk files in app files yet — export one, or adb push a backup here.");
                } else {
                    for (name, path) in self.backup_list.clone() {
                        ui.horizontal(|ui| {
                            ui.label(&name);
                            if ui.button("Import").clicked() {
                                self.import_encrypted_backup(host, &path);
                            }
                            if ui.button("Share").clicked() {
                                host.share_file(path);
                            }
                        });
                    }
                }
                if !self.backup_note.is_empty() {
                    ui.label(&self.backup_note);
                }
            });

            ui.add_space(12.0);
            ui.weak("Server, key, account and generation settings save automatically.");
            ui.weak("Mirrored to Android/data/…/files/comfyui_settings.json (adb pull-able).");
            ui.weak("Password is never stored — only the session token after Sign in.");
            ui.weak("Models: prefer /sdcard/ComfyUI/ (survives uninstall); app files/ is wiped with the app.");
            if let Some(why) = &self.settings_write_blocked {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), why);
                if ui.button("Force overwrite broken settings").clicked() {
                    self.settings_write_blocked = None;
                    self.last_saved = None;
                }
            }
            ui.add_space(12.0);
        });
    }

    fn create_pane_bar(&mut self, ui: &mut egui::Ui) {
        let prev = self.create_pane;
        const N: f32 = 6.0;
        const GAP: f32 = 4.0;
        const ROW_H: f32 = 28.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            let btn_w = ((ui.available_width() - GAP * (N - 1.0)) / N).max(28.0);
            let size = egui::vec2(btn_w, ROW_H);

            let panes: [(CreatePane, String, String); 6] = {
                let model_n = self.checkpoints.len() + self.unets.len();
                let lora_n = self.params.loras.len();
                let app_n = self.params.apps.iter().filter(|a| a.enabled).count();
                let preset_n = self.presets.len();
                let char_n = self.characters.len();
                [
                    (CreatePane::Main, "Main".into(), "Main".into()),
                    (
                        CreatePane::Models,
                        if model_n > 0 {
                            format!("{}{model_n}", icons::MODEL)
                        } else {
                            icons::MODEL.into()
                        },
                        if model_n > 0 {
                            format!("Models ({model_n})")
                        } else {
                            "Models".into()
                        },
                    ),
                    (
                        CreatePane::Loras,
                        if lora_n > 0 { format!("LoRA {lora_n}") } else { "LoRA".into() },
                        if lora_n > 0 {
                            format!("LoRAs ({lora_n})")
                        } else {
                            "LoRAs".into()
                        },
                    ),
                    (
                        CreatePane::Enhance,
                        if app_n > 0 {
                            format!("{}{app_n}", icons::GENERATE)
                        } else {
                            icons::GENERATE.into()
                        },
                        if app_n > 0 {
                            format!("Enhance ({app_n})")
                        } else {
                            "Enhance".into()
                        },
                    ),
                    (
                        CreatePane::Presets,
                        if preset_n > 0 {
                            format!("{}{preset_n}", icons::SAVE)
                        } else {
                            icons::SAVE.into()
                        },
                        if preset_n > 0 {
                            format!("Presets ({preset_n})")
                        } else {
                            "Presets".into()
                        },
                    ),
                    (
                        CreatePane::Characters,
                        if char_n > 0 {
                            format!("{}{char_n}", icons::USER)
                        } else {
                            icons::USER.into()
                        },
                        if char_n > 0 {
                            format!("Characters ({char_n})")
                        } else {
                            "Characters".into()
                        },
                    ),
                ]
            };

            for (pane, label, hover) in panes {
                let selected = self.create_pane == pane;
                if ui
                    .add_sized(size, egui::Button::selectable(selected, label))
                    .on_hover_text(hover)
                    .clicked()
                {
                    self.create_pane = pane;
                }
            }
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
            CreatePane::Models => self.create_models_pane(ui, host),
            CreatePane::Loras => self.create_loras_pane(ui),
            CreatePane::Enhance => self.create_enhance_pane(ui),
            CreatePane::Presets => self.create_presets_pane(ui, host),
            CreatePane::Characters => self.create_characters_pane(ui, host),
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

    /// Warm the bundled tag dictionary on a background thread; mark ready once its parse finishes.
    fn ensure_tag_dict_warm(&mut self, ctx: &egui::Context) {
        if self.tag_dict_warm || self.tag_dict_override.is_some() {
            return;
        }
        if let Some(rx) = &self.tag_dict_warming {
            match rx.try_recv() {
                Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.tag_dict_warm = true;
                    self.tag_dict_warming = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
            }
        } else {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tags::TagDict::bundled();
                let _ = tx.send(());
            });
            self.tag_dict_warming = Some(rx);
            ctx.request_repaint();
        }
    }

    /// On-disk path of the personal co-occurrence model.
    fn cooc_path(host: &Host) -> Option<String> {
        host.documents_dir().map(|d| format!("{d}/comfyui/cooc.json"))
    }

    /// Load the co-occurrence model once on a background thread; empty on absence or parse failure.
    fn ensure_cooc_warm(&mut self, ctx: &egui::Context, host: &Host) {
        if self.cooc_loaded {
            return;
        }
        if let Some(rx) = &self.cooc_loading {
            match rx.try_recv() {
                Ok(model) => {
                    self.cooc = model;
                    self.cooc_loaded = true;
                    self.cooc_loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.cooc_loaded = true;
                    self.cooc_loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
            }
        } else {
            let path = Self::cooc_path(host);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let model = path
                    .and_then(|p| std::fs::read_to_string(&p).ok())
                    .and_then(|t| serde_json::from_str::<cooc::CoocModel>(&t).ok())
                    .unwrap_or_default();
                let _ = tx.send(model);
            });
            self.cooc_loading = Some(rx);
            ctx.request_repaint();
        }
    }

    /// Persist the co-occurrence model (queue-time only; small file).
    fn save_cooc(&self, host: &Host) {
        let Some(path) = Self::cooc_path(host) else { return };
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(&self.cooc) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// Learn the current positive prompt's tags into the co-occurrence model, then persist.
    fn observe_prompt_cooc(&mut self, host: &Host) {
        if !self.cooc_loaded {
            return;
        }
        let tags: Vec<String> =
            tags::parse_chips(&self.params.positive).into_iter().map(|c| c.tag).collect();
        if self.cooc.observe(&tags) {
            self.save_cooc(host);
        }
    }

    /// On-disk path of the persistent auto-tag index.
    fn tag_index_path(host: &Host) -> Option<String> {
        host.documents_dir().map(|d| format!("{d}/comfyui/tag_index.json"))
    }

    /// Load the auto-tag index once on a background thread; empty on absence or parse failure.
    fn ensure_tag_index_warm(&mut self, ctx: &egui::Context, host: &Host) {
        if self.tag_index_loaded {
            return;
        }
        if let Some(rx) = &self.tag_index_loading {
            match rx.try_recv() {
                Ok(index) => {
                    self.tag_index = index;
                    self.tag_index_loaded = true;
                    self.tag_index_loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.tag_index_loaded = true;
                    self.tag_index_loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
            }
        } else {
            let path = Self::tag_index_path(host);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let index = path
                    .and_then(|p| std::fs::read_to_string(&p).ok())
                    .and_then(|t| serde_json::from_str::<tag_index::TagIndex>(&t).ok())
                    .unwrap_or_default();
                let _ = tx.send(index);
            });
            self.tag_index_loading = Some(rx);
            ctx.request_repaint();
        }
    }

    /// Persist the auto-tag index and clear the batched-write counter.
    #[cfg(feature = "local-npu")]
    fn save_tag_index(&mut self, host: &Host) {
        self.tag_index_dirty = 0;
        let Some(path) = Self::tag_index_path(host) else { return };
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(&self.tag_index) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// Prefer durable `/sdcard/ComfyUI/clip_index.bin` (survives app data clears); fall back to documents.
    fn clip_index_path(host: &Host) -> Option<String> {
        if let Some(full) = gallery::resolve_full_cache_root(host.documents_dir().as_deref())
            && let Some(parent) = std::path::Path::new(&full).parent()
        {
            return Some(parent.join("clip_index.bin").to_string_lossy().into_owned());
        }
        host.documents_dir().map(|d| format!("{d}/comfyui/clip_index.bin"))
    }

    /// Load the CLIP embedding index once on a background thread; empty on absence or junk.
    fn ensure_clip_index_warm(&mut self, ctx: &egui::Context, host: &Host) {
        if self.clip_index_loaded {
            return;
        }
        if let Some(rx) = &self.clip_index_loading {
            match rx.try_recv() {
                Ok(index) => {
                    self.clip_index = index;
                    self.clip_index_loaded = true;
                    self.clip_index_loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.clip_index_loaded = true;
                    self.clip_index_loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
            }
        } else {
            let primary = Self::clip_index_path(host);
            let legacy = host.documents_dir().map(|d| format!("{d}/comfyui/clip_index.bin"));
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let read = |p: &str| std::fs::read(p).ok().map(|b| clip_index::ClipIndex::from_bytes(&b));
                let mut index = primary
                    .as_deref()
                    .and_then(read)
                    .unwrap_or_default();
                // Migrate a larger legacy documents index onto the durable root.
                if let Some(leg) = legacy.as_deref()
                    && primary.as_deref() != Some(leg)
                {
                    if let Some(old) = read(leg)
                        && old.len() > index.len()
                    {
                        index = old;
                        if let Some(dst) = primary.as_deref() {
                            if let Some(dir) = std::path::Path::new(dst).parent() {
                                let _ = std::fs::create_dir_all(dir);
                            }
                            let _ = std::fs::write(dst, index.to_bytes());
                        }
                    }
                }
                let _ = tx.send(index);
            });
            self.clip_index_loading = Some(rx);
            ctx.request_repaint();
        }
    }

    /// Persist the CLIP embedding index and clear the batched-write counter.
    #[cfg(feature = "local-npu")]
    fn save_clip_index(&mut self, host: &Host) {
        self.clip_index_dirty = 0;
        let Some(path) = Self::clip_index_path(host) else { return };
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, self.clip_index.to_bytes());
    }

    /// Drop in-flight embed work and failed-key skips so indexing can resume.
    #[cfg(feature = "local-npu")]
    fn reset_clipemb_pump(&mut self) {
        self.clipemb_pending = None;
        self.clipemb_rx = None;
        self.clipemb_failed.clear();
    }

    /// Wipe the on-disk CLIP index and restart embedding from scratch.
    #[cfg(feature = "local-npu")]
    fn rebuild_clip_index(&mut self, host: &Host) {
        self.reset_clipemb_pump();
        self.clip_index = clip_index::ClipIndex::default();
        self.clip_index_loaded = true;
        self.clip_index_loading = None;
        self.clip_index_dirty = 0;
        if let Some(path) = Self::clip_index_path(host) {
            let _ = std::fs::remove_file(path);
        }
        self.save_clip_index(host);
    }

    /// Dict count + category for a folded tag, for styling co-oc suggestion buttons like prefix ones.
    fn dict_lookup_meta(&self, tag: &str) -> (u32, u8) {
        let entry = match &self.tag_dict_override {
            Some(d) => d.lookup(tag),
            None if self.tag_dict_warm => tags::TagDict::bundled().lookup(tag),
            None => None,
        };
        entry.map(|e| (e.count, e.category)).unwrap_or((0, 0))
    }

    /// Next-tag co-oc suggestions shaped like [`tag_suggestions`] for an empty cursor token.
    fn cooc_suggestions(&self, present: &[String], limit: usize) -> Vec<(String, u32, u8)> {
        if !self.cooc_loaded {
            return Vec::new();
        }
        self.cooc
            .suggest(present, limit)
            .into_iter()
            .map(|(name, _)| {
                let (count, cat) = self.dict_lookup_meta(&name);
                (name, count, cat)
            })
            .collect()
    }

    /// Prefix suggestions as owned `(insert_text, count, category)` tuples; override first, else bundled.
    fn tag_suggestions(&self, prefix: &str, limit: usize) -> Vec<(String, u32, u8)> {
        let entries = match &self.tag_dict_override {
            Some(d) => d.suggest(prefix, limit),
            None if self.tag_dict_warm => tags::TagDict::bundled().suggest(prefix, limit),
            None => return Vec::new(),
        };
        entries.iter().map(|e| (e.insert_text(), e.count, e.category)).collect()
    }

    /// Danbooru category of `tag` from the active dictionary, if known.
    fn tag_category(&self, tag: &str) -> Option<u8> {
        match &self.tag_dict_override {
            Some(d) => d.category_of(tag),
            None if self.tag_dict_warm => tags::TagDict::bundled().category_of(tag),
            None => None,
        }
    }

    /// The mutable prompt string for `field`.
    fn field_text_mut(&mut self, field: PromptField) -> &mut String {
        match field {
            PromptField::Positive => &mut self.params.positive,
            PromptField::Negative => &mut self.params.negative,
        }
    }

    /// The prompt string for `field`.
    fn field_text(&self, field: PromptField) -> &String {
        match field {
            PromptField::Positive => &self.params.positive,
            PromptField::Negative => &self.params.negative,
        }
    }

    /// Whether `field` is currently in chip-editing mode.
    fn field_chips(&self, field: PromptField) -> bool {
        match field {
            PromptField::Positive => self.prompt_chips,
            PromptField::Negative => self.neg_prompt_chips,
        }
    }

    /// Set `field`'s chip-editing mode.
    fn set_field_chips(&mut self, field: PromptField, on: bool) {
        match field {
            PromptField::Positive => self.prompt_chips = on,
            PromptField::Negative => self.neg_prompt_chips = on,
        }
    }

    /// Positive prompt: label + chip toggle, then the chip editor or text field, then the scrubber.
    fn positive_prompt_ui(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.prompt_field_ui(ui, PromptField::Positive, "Prompt");
        ui.horizontal(|ui| {
            self.rewrite_menu_ui(ui, host);
            self.dup_fix_chip_ui(ui);
        });
        self.prompt_history_ui(ui);
    }

    /// Prompt-history scrubber: a compact row that slides `params.positive`/`negative` back through
    /// recorded generations. The live draft is stashed as the newest slot, so the far end restores
    /// it; a manual edit detaches. Hidden while history is empty.
    fn prompt_history_ui(&mut self, ui: &mut egui::Ui) {
        let n = self.prompt_history.len();
        if n == 0 {
            self.hist_stash = None;
            self.hist_applied = None;
            return;
        }
        // A manual edit to either field while scrubbing detaches: drop the stash, snap to live.
        let edited = self
            .hist_applied
            .as_ref()
            .is_some_and(|(p, neg)| *p != self.params.positive || *neg != self.params.negative);
        if self.hist_stash.is_some() && edited {
            self.hist_stash = None;
            self.hist_applied = None;
        }
        let total = n + 1;
        let mut val = if self.hist_stash.is_some() { self.hist_slider.clamp(1, total) } else { total };
        let before = val;
        ui.horizontal(|ui| {
            ui.label("Hist");
            ui.spacing_mut().slider_width = (ui.available_width() - 52.0).max(80.0);
            ui.add(egui::Slider::new(&mut val, 1..=total).show_value(false));
            ui.label(format!("{val}/{total}"));
        });
        if val != before {
            self.scrub_to(ui.ctx(), val, total);
        }
    }

    /// Move the scrubber to slider position `val` (1..=`total`), writing that slot's prompt pair.
    fn scrub_to(&mut self, ctx: &egui::Context, val: usize, total: usize) {
        // First move away from live stashes the current draft into the newest slot.
        if self.hist_stash.is_none() {
            self.hist_stash = Some(PromptHist {
                positive: self.params.positive.clone(),
                negative: self.params.negative.clone(),
            });
        }
        self.hist_slider = val;
        let entry = if val >= total {
            self.hist_stash.clone().unwrap_or_default()
        } else {
            self.prompt_history[val - 1].clone()
        };
        // Plain field assignment so the chip view and autocomplete re-read the new text.
        self.params.positive = entry.positive.clone();
        self.params.negative = entry.negative.clone();
        self.hist_applied = Some((entry.positive, entry.negative));
        // Drop focus so a live caret in a prompt field doesn't fight the replaced text.
        ctx.memory_mut(|m| {
            m.surrender_focus(PromptField::Positive.edit_id());
            m.surrender_focus(PromptField::Negative.edit_id());
        });
        // Landing back on the live slot detaches cleanly.
        if val >= total {
            self.hist_stash = None;
            self.hist_applied = None;
        }
    }

    /// Negative prompt: label + chip toggle, then the chip editor or text field.
    fn negative_prompt_ui(&mut self, ui: &mut egui::Ui) {
        self.prompt_field_ui(ui, PromptField::Negative, "Negative");
    }

    /// One prompt field: a `label` + chip-view toggle, then the chip editor or the text field.
    fn prompt_field_ui(&mut self, ui: &mut egui::Ui, field: PromptField, label: &str) {
        ui.horizontal(|ui| {
            ui.label(label);
            let on = self.field_chips(field);
            if ui
                .add(egui::Button::new("chips").selected(on))
                .on_hover_text("Edit as tag chips")
                .clicked()
            {
                self.set_field_chips(field, !on);
            }
        });
        if self.field_chips(field) {
            self.prompt_chip_view(ui, field);
        } else {
            self.prompt_text_view(ui, field);
        }
    }

    /// A prompt text field plus a tag-autocomplete row under it while the field has focus.
    fn prompt_text_view(&mut self, ui: &mut egui::Ui, field: PromptField) {
        let id = field.edit_id();
        let out = egui::TextEdit::multiline(self.field_text_mut(field))
            .id(id)
            .desired_rows(field.rows())
            .desired_width(f32::INFINITY)
            .hint_text(field.hint())
            .show(ui);
        if !out.response.has_focus() {
            return;
        }
        let Some(cursor_char) = out.cursor_range.map(|r| r.primary.index.0) else {
            return;
        };
        let text = self.field_text(field).clone();
        let cursor_byte =
            text.char_indices().nth(cursor_char).map(|(b, _)| b).unwrap_or(text.len());
        let (range, tok) = tags::token_at(&text, cursor_byte);
        // Co-oc runs on the positive field only; present = the field's already-typed tags.
        let present: Vec<String> = if field == PromptField::Positive {
            tags::parse_chips(&text).into_iter().map(|c| c.tag).collect()
        } else {
            Vec::new()
        };
        let sugg = if tok.chars().count() < 2 {
            // Empty cursor token: co-oc next-tag suggestions from the present set.
            self.cooc_suggestions(&present, 8)
        } else {
            let mut m = self.tag_suggestions(tok, 8);
            if field == PromptField::Positive {
                cooc::blend_rank(&mut m, |name| self.cooc.rerank_boost(&present, name));
            }
            m
        };
        if sugg.is_empty() {
            return;
        }
        let mut accepted: Option<(String, usize)> = None;
        crate::theme::scroll_horizontal()
            .id_salt(("tag_suggest_row", field.disc()))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (name, count, cat) in &sugg {
                        let label = format!("{name}  {}", fmt_count(*count));
                        let mut btn = egui::Button::new(sanitize_ui_text(ui, &label)).small();
                        if let Some(fill) = crate::theme::tag_category_fill(*cat) {
                            btn = btn.fill(fill);
                        }
                        if ui.add(btn).clicked() {
                            accepted = Some(tags::accept_suggestion(&text, range.clone(), name));
                        }
                    }
                });
            });
        if let Some((new_text, cursor_byte)) = accepted {
            let char_idx = new_text[..cursor_byte].chars().count();
            *self.field_text_mut(field) = new_text;
            if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), id) {
                let cur = egui::text::CCursor::new(char_idx);
                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(cur)));
                egui::TextEdit::store_state(ui.ctx(), id, st);
            }
            ui.memory_mut(|m| m.request_focus(id));
        }
    }

    /// A prompt rendered as draggable tap-to-edit chips; the text string stays the source of truth.
    fn prompt_chip_view(&mut self, ui: &mut egui::Ui, field: PromptField) {
        let text = self.field_text(field).clone();
        let chips = tags::parse_chips(&text);
        let disc = field.disc();
        let mut new_text: Option<String> = None;
        let mut to_text = false;
        let mut reorder: Option<(usize, usize)> = None;
        let mut to_other: Option<usize> = None;
        ui.horizontal_wrapped(|ui| {
            for (i, chip) in chips.iter().enumerate() {
                let label = if (chip.weight - 1.0).abs() < 0.005 {
                    chip.tag.clone()
                } else {
                    format!("{} x{}", chip.tag, fmt_weight(chip.weight))
                };
                let mut btn = egui::Button::new(sanitize_ui_text(ui, &label))
                    .sense(egui::Sense::click_and_drag());
                if let Some(fill) = self.tag_category(&chip.tag).and_then(crate::theme::tag_category_fill) {
                    btn = btn.fill(fill);
                }
                let resp = ui.add(btn);
                resp.dnd_set_drag_payload(ChipDrag { field: disc, idx: i });
                if let (Some(pointer), Some(held)) =
                    (ui.input(|i| i.pointer.interact_pos()), resp.dnd_hover_payload::<ChipDrag>())
                {
                    if held.field == disc {
                        let rect = resp.rect;
                        let stroke = egui::Stroke::new(2.0, ui.visuals().selection.stroke.color);
                        let gap = if pointer.x < rect.center().x {
                            ui.painter().vline(rect.left(), rect.y_range(), stroke);
                            i
                        } else {
                            ui.painter().vline(rect.right(), rect.y_range(), stroke);
                            i + 1
                        };
                        if let Some(dropped) = resp.dnd_release_payload::<ChipDrag>() {
                            if dropped.field == disc {
                                reorder = Some((dropped.idx, gap));
                            }
                        }
                    }
                }
                egui::Popup::menu(&resp).show(|ui| {
                    if ui.button("Weight +").clicked() {
                        new_text = Some(tags::bump_weight(&text, i, 0.05));
                    }
                    if ui.button("Weight -").clicked() {
                        new_text = Some(tags::bump_weight(&text, i, -0.05));
                    }
                    if ui.button("Move left").clicked() {
                        new_text = Some(tags::move_chip(&text, i, -1));
                    }
                    if ui.button("Move right").clicked() {
                        new_text = Some(tags::move_chip(&text, i, 1));
                    }
                    if ui.button(field.move_label()).clicked() {
                        to_other = Some(i);
                    }
                    if ui.button(format!("{} Delete", icons::TRASH)).clicked() {
                        new_text = Some(tags::remove_chip(&text, i));
                    }
                });
            }
            if ui.button(format!("{} Add", icons::ADD)).clicked() {
                to_text = true;
            }
        });
        if let Some((from, to)) = reorder {
            new_text = Some(tags::move_chip_to(&text, from, to));
        }
        if let Some(i) = to_other {
            // Negatives rarely carry attention syntax: the bare tag moves without its weight wrapper.
            if let Some(tag) = chips.get(i).map(|c| c.tag.clone()) {
                let other = field.other();
                let joined = tags::push_chip(self.field_text(other), &tag);
                *self.field_text_mut(other) = joined;
                new_text = Some(tags::remove_chip(&text, i));
            }
        }
        if let Some(t) = new_text {
            *self.field_text_mut(field) = t;
        }
        if to_text {
            self.set_field_chips(field, false);
            ui.memory_mut(|m| m.request_focus(field.edit_id()));
        }
    }

    /// Recompute the cached lint issues when the prompt/model/LoRA fingerprint changes.
    fn refresh_lint(&mut self) {
        let model = self.params.model_file().to_string();
        let mut key = String::new();
        key.push_str(&self.params.positive);
        key.push('\u{1}');
        key.push_str(&self.params.negative);
        key.push('\u{1}');
        key.push_str(&self.params.lora_triggers);
        key.push('\u{1}');
        key.push_str(&model);
        for al in &self.params.loras {
            key.push('\u{1}');
            key.push_str(&al.file);
            key.push_str(&format!(":{}:{}", al.strength_model, al.strength_clip));
        }
        let fp = str_fingerprint(&key);
        if fp == self.lint_fp {
            return;
        }
        self.lint_fp = fp;
        // Wan takes natural-language motion prompts, not danbooru quality blocks — a checkpoint of
        // None suppresses the family-quality lints while keeping paren / duplicate / count checks.
        let ckpt =
            (self.params.mode != Mode::Video).then(|| self.checkpoint_catalog.entry(&model)).flatten();
        let loras: Vec<_> =
            self.params.loras.iter().map(|al| (al, self.lora_catalog.entry(&al.file))).collect();
        self.lint_issues = lint::lint(&self.params, ckpt, &loras);
    }

    /// One wrapped row of lint chips; a fixable issue applies its fix on tap.
    /// The duplicate-tags fix as an inline chip beside Rewrite (skipped by the lint row below).
    fn dup_fix_chip_ui(&mut self, ui: &mut egui::Ui) {
        self.refresh_lint();
        let dup = self.lint_issues.iter().find(|i| i.msg.contains("uplicate"));
        let Some(fix) = dup.and_then(|i| i.fix.clone()) else { return };
        if ui.button("Dedupe tags").on_hover_text("Remove duplicate tags from the prompt").clicked()
        {
            self.apply_fix(fix);
            self.lint_fp = 0;
        }
    }

    fn lint_chips_ui(&mut self, ui: &mut egui::Ui) {
        self.refresh_lint();
        if self.lint_issues.is_empty() {
            return;
        }
        ui.add_space(4.0);
        let mut applied: Option<lint::Fix> = None;
        ui.horizontal_wrapped(|ui| {
            for issue in &self.lint_issues {
                if issue.msg.contains("uplicate") {
                    continue;
                }
                let color = match issue.severity {
                    lint::Severity::Warn => ui.visuals().warn_fg_color,
                    lint::Severity::Info => ui.visuals().weak_text_color(),
                };
                let text = egui::RichText::new(sanitize_ui_text(ui, &issue.msg)).small().color(color);
                if let Some(fix) = &issue.fix {
                    if ui.button(text).on_hover_text("Tap to apply fix").clicked() {
                        applied = Some(fix.clone());
                    }
                } else {
                    ui.add(egui::Label::new(text));
                }
            }
        });
        if let Some(fix) = applied {
            self.apply_fix(fix);
            self.lint_fp = 0;
        }
    }

    /// Apply a lint fix: one whole-field assignment.
    fn apply_fix(&mut self, fix: lint::Fix) {
        match fix {
            lint::Fix::SetPositive(s) => self.params.positive = s,
            lint::Fix::SetNegative(s) => self.params.negative = s,
            lint::Fix::SetLoraTriggers(s) => self.params.lora_triggers = s,
        }
    }

    fn create_main_pane(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.ensure_tag_dict_warm(ui.ctx());
        self.ensure_cooc_warm(ui.ctx(), host);
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
        #[cfg(feature = "local-npu")]
        let local_badge = if self.route_local_gen() {
            format!(" · Local · {}", self.local_backend.label())
        } else {
            String::new()
        };
        #[cfg(not(feature = "local-npu"))]
        let local_badge = String::new();
        ui.weak(sanitize_ui_text(
            ui,
            &format!("{ckpt_label} · {preset_label}{enhance_label}{local_badge}"),
        ));
        #[cfg(feature = "local-npu")]
        if self.route_local_gen() {
            self.ensure_local_packs(host, false);
            let pack = self.selected_pack().map(|p| p.name.clone());
            let line = match (&pack, self.local_backend) {
                (Some(p), LocalBackend::Anima) => {
                    format!("Local NPU — Anima '{p}' runs on-device (txt2img 1024²)")
                }
                (Some(p), LocalBackend::Sd15) => {
                    format!("Local NPU — SD1.5 '{p}' runs on-device (txt2img 512²)")
                }
                (None, b) => format!("Local NPU — no {} pack found (Settings -> Local NPU)", b.label()),
            };
            let colour = if pack.is_some() {
                egui::Color32::from_rgb(120, 200, 230)
            } else {
                egui::Color32::from_rgb(230, 180, 120)
            };
            ui.colored_label(colour, sanitize_ui_text(ui, &line));
        }
        let anima = self.anima_active();
        if let Some(hint) = self
            .checkpoint_catalog
            .entry(&model_file)
            .and_then(|e| e.recommended.as_ref())
            .and_then(|r| r.short_hint())
        {
            ui.weak(sanitize_ui_text(ui, &format!("rec: {hint}")));
        }

        let show_video = self.params.mode == Mode::Video
            || self.schemas.as_ref().is_some_and(|s| s.has_node("WanImageToVideo"));
        ui.horizontal(|ui| {
            let mut mode_btn = |ui: &mut egui::Ui, m: Mode, label: &str| {
                if ui.add(egui::Button::selectable(self.params.mode == m, label)).clicked() {
                    self.params.mode = m;
                }
            };
            mode_btn(ui, Mode::Txt2Img, "Text to Image");
            ui.add_enabled_ui(!anima, |ui| {
                mode_btn(ui, Mode::Img2Img, "Image to Image");
            });
            if show_video {
                mode_btn(ui, Mode::Video, "Video");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.reset_button(ui, host);
            });
        });
        // Anima has no img2img, but its checkpoint is unrelated to Wan video.
        if anima && self.params.mode == Mode::Img2Img {
            self.params.mode = Mode::Txt2Img;
        }
        if anima && self.params.mode != Mode::Video {
            ui.weak("Anima has no img2img yet — text to image only.");
        }

        if self.params.mode == Mode::Video {
            self.create_video_body(ui, host);
            ui.add_space(130.0);
            return;
        }

        let show_companions = self.params.model_kind == ModelKind::Diffusion;
        let show_img_source = self.params.mode == Mode::Img2Img;
        if show_companions || show_img_source {
            let setup_open = self.create_companions_open;
            let setup_title = match (show_companions, show_img_source, setup_open) {
                (true, true, true) => "Companions & image source".into(),
                (true, false, true) => "Text encoder / VAE".into(),
                (false, true, true) => "Image source".into(),
                (true, true, false) => "Companions & image source".into(),
                (true, false, false) => {
                    let clip = self
                        .params
                        .clip_names
                        .first()
                        .map(|s| file_basename(s).to_string())
                        .unwrap_or_else(|| "no encoder".into());
                    let vae = if self.params.vae_name.is_empty() {
                        "no VAE".to_string()
                    } else {
                        file_basename(&self.params.vae_name).to_string()
                    };
                    // Header text never wraps; a long title pushes the whole pane off-screen.
                    format!("Encoders · {}", elide(&format!("{clip} · {vae}"), 26))
                }
                (false, true, false) => match self.params.img2img_source {
                    Img2ImgSource::CurrentOutput => "Image source · Current result".into(),
                    Img2ImgSource::Url => "Image source · From URL".into(),
                    Img2ImgSource::Picked => match self.picked_input.as_ref() {
                        Some(p) => format!("Image source · {}", elide(&p.name, 20)),
                        None => "Image source · Photo".into(),
                    },
                },
                (false, false, _) => String::new(),
            };
            let setup = egui::CollapsingHeader::new(setup_title)
                .id_salt("create_t2i_i2i_setup")
                .open(Some(setup_open))
                .show(ui, |ui| {
                    // Diffusion models ship without a text encoder or VAE; select_model seeds these.
                    if show_companions {
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
                        if self.params.clip_names.len() < 2 && ui.button("+ second encoder").clicked()
                        {
                            self.params.clip_names.push(String::new());
                        }

                        ui.label("VAE");
                        combo_full(ui, "vae_name", &mut self.params.vae_name, &self.vaes);

                        egui::CollapsingHeader::new("Advanced").id_salt("diffusion_adv").show(
                            ui,
                            |ui| {
                                ui.label("Encoder type");
                                let mut ty = self.params.effective_clip_type();
                                combo_full(ui, "clip_type", &mut ty, &self.clip_types);
                                self.params.clip_type = ty;
                                ui.label("Weight dtype");
                                combo_full(
                                    ui,
                                    "weight_dtype",
                                    &mut self.params.weight_dtype,
                                    &self.weight_dtypes,
                                );
                                if !self.clip_devices.is_empty() {
                                    ui.label("Encoder device");
                                    combo_full(
                                        ui,
                                        "clip_device",
                                        &mut self.params.clip_device,
                                        &self.clip_devices,
                                    );
                                }
                            },
                        );

                        if let Some(missing) = self.params.missing_model_part() {
                            ui.colored_label(ui.visuals().warn_fg_color, missing);
                        }
                    }

                    if show_img_source {
                        if show_companions {
                            ui.add_space(4.0);
                        }
                        let prev_src = self.params.img2img_source;
                        ui.horizontal_wrapped(|ui| {
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
                            ui.selectable_value(
                                &mut self.params.img2img_source,
                                Img2ImgSource::Picked,
                                "Device photo",
                            );
                            if ui.selectable_label(false, "From gallery").clicked() {
                                self.gallery_pick_open = true;
                            }
                        });
                        // Leaving Picked drops the masked photo it applied to.
                        if prev_src == Img2ImgSource::Picked
                            && self.params.img2img_source != Img2ImgSource::Picked
                        {
                            self.params.inpaint_mask = false;
                        }
                        self.image_source_preview(ui, host);
                        // Denoise at the bottom of the section, only relevant for img2img.
                        ui.add_space(4.0);
                        full_width_slider(ui, "Denoise", |ui, w| {
                            ui.add_sized(
                                [w, 24.0],
                                egui::Slider::new(&mut self.params.denoise, 0.0..=1.0),
                            );
                        });
                    }
                });
            if setup.header_response.clicked() {
                self.create_companions_open = !setup_open;
            }
        }

        self.positive_prompt_ui(ui, host);
        ui.label("LoRA triggers");
        ui.add(
            egui::TextEdit::multiline(&mut self.params.lora_triggers)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("trigger words from LoRAs (auto-filled on Add)"),
        );
        self.negative_prompt_ui(ui);
        if anima {
            ui.weak("Negative only applies when the pack's CFG is above 1.0.");
        }

        self.lint_chips_ui(ui);

        ui.add_space(4.0);
        ui.columns(2, |cols| {
            cols[0].vertical_centered(|ui| {
                stepper_u32(ui, "Steps", &mut self.params.steps, 5..=150, 1);
            });
            cols[1].vertical_centered(|ui| {
                if anima {
                    section_title(ui, "CFG");
                    ui.weak("pack default");
                } else {
                    stepper_f32(ui, "CFG", &mut self.params.cfg, 1.0..=20.0, 0.5);
                }
            });
        });
        ui.add_space(4.0);
        ui.columns(2, |cols| {
            cols[0].vertical_centered(|ui| {
                stepper_u32(ui, "Batch", &mut self.params.batch_size, 1..=8, 1);
            });
            cols[1].vertical_centered(|ui| {
                let mut variants = self.queue_variants.clamp(1, 8) as u32;
                stepper_u32(ui, "Variants", &mut variants, 1..=8, 1);
                self.queue_variants = variants as usize;
            });
        });

        ui.add_space(4.0);
        ui.vertical_centered(|ui| {
            section_title(ui, "Size");
        });
        ui.add_enabled_ui(!anima, |ui| {
            centered_row(ui, |ui| {
                uint_text_edit(ui, "size_w", &mut self.params.width, 64..=2048);
                ui.label("×");
                uint_text_edit(ui, "size_h", &mut self.params.height, 64..=2048);
                size_preset_combo(ui, &mut self.params.width, &mut self.params.height);
            });
        });
        if anima {
            ui.vertical_centered(|ui| ui.weak("Anima renders 1024x1024 — size is fixed."));
        }
        // An enabled step may render at a different size than the one above (hi-res fix works by
        // generating small and scaling up). Show what it will actually do — the stored value is
        // never touched, so this line simply disappears when the step is removed.
        self.param_override_note(ui);

        ui.add_space(4.0);
        if anima {
            ui.vertical_centered(|ui| {
                section_title(ui, "Sampler");
                ui.weak("euler (flow match)");
            });
        } else {
            let mut sampler = self.params.sampler.clone();
            let mut scheduler = self.params.scheduler.clone();
            let samplers = self.samplers.clone();
            let schedulers = self.schedulers.clone();
            ui.columns(2, |cols| {
                cols[0].vertical_centered(|ui| {
                    section_title(ui, "Sampler");
                    combo_full(ui, "sampler", &mut sampler, &samplers);
                });
                cols[1].vertical_centered(|ui| {
                    section_title(ui, "Scheduler");
                    combo_full(ui, "scheduler", &mut scheduler, &schedulers);
                });
            });
            self.params.sampler = sampler;
            self.params.scheduler = scheduler;
        }

        ui.add_space(4.0);
        ui.label("Seed");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.params.randomize_seed, |ui| {
                uint_text_edit_u64(ui, "seed", &mut self.params.seed, 0..=u64::MAX);
            });
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

        // Room for the stacked queue + menu FABs.
        ui.add_space(130.0);
    }

    /// The Create Main body for [`Mode::Video`]: Wan 2.2 image-to-video.
    fn create_video_body(&mut self, ui: &mut egui::Ui, host: &Host) {
        let has_wan = self.schemas.as_ref().is_some_and(|s| s.has_node("WanImageToVideo"));
        let has_vhs = self.schemas.as_ref().is_some_and(|s| s.has_node("VHS_VideoCombine"));
        let has_rife = self.schemas.as_ref().is_some_and(|s| s.has_node("RIFE VFI"));
        let has_clean = self.schemas.as_ref().is_some_and(|s| s.has_node("easy cleanGpuUsed"));
        if self.schemas.is_some() && (!has_wan || !has_vhs) {
            let mut missing = Vec::new();
            if !has_wan {
                missing.push("WanImageToVideo");
            }
            if !has_vhs {
                missing.push("VHS_VideoCombine (VideoHelperSuite)");
            }
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("Server is missing: {}", missing.join(", ")),
            );
        }
        ui.weak("Wan 2.2 i2v — describe the motion; canned defaults do the rest.");

        // Prompt + negative, with a one-tap canonical Wan negative.
        self.positive_prompt_ui(ui, host);
        ui.horizontal(|ui| {
            ui.label("Negative");
            if ui
                .small_button("Wan negative")
                .on_hover_text("Load the canonical Wan negative prompt")
                .clicked()
            {
                self.params.negative = crate::types::WAN_NEGATIVE.to_string();
            }
        });
        ui.add(
            egui::TextEdit::multiline(&mut self.params.negative)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("what to avoid"),
        );
        self.lint_chips_ui(ui);

        // Start image source, with a text-to-video option.
        ui.add_space(6.0);
        ui.label("Start image");
        let t2v = self.params.video.video_t2v;
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(t2v, "None (text to video)").clicked() {
                self.params.video.video_t2v = true;
            }
            for (src, label) in [
                (Img2ImgSource::CurrentOutput, "Current result"),
                (Img2ImgSource::Url, "From URL"),
                (Img2ImgSource::Picked, "Device photo"),
            ] {
                let selected = !t2v && self.params.img2img_source == src;
                if ui.selectable_label(selected, label).clicked() {
                    self.params.img2img_source = src;
                    self.params.video.video_t2v = false;
                }
            }
            if ui.selectable_label(false, "From gallery").clicked() {
                self.params.video.video_t2v = false;
                self.gallery_pick_open = true;
            }
        });
        if self.params.video.video_t2v {
            ui.weak("No start image — Wan generates motion from the prompt alone.");
        } else {
            self.image_source_preview(ui, host);
        }

        // Size.
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            cols[0].vertical_centered(|ui| {
                stepper_u32(ui, "Width", &mut self.params.video.width, 128..=1280, 16);
            });
            cols[1].vertical_centered(|ui| {
                stepper_u32(ui, "Height", &mut self.params.video.height, 128..=1280, 16);
            });
        });

        // Length, snapped to Wan's 4n+1, shown in seconds at the 16fps base.
        ui.add_space(6.0);
        ui.vertical_centered(|ui| section_title(ui, "Length"));
        let mut len = self.params.video.length;
        full_width_slider(ui, "frames", |ui, w| {
            ui.add_sized([w, 24.0], egui::Slider::new(&mut len, 5..=161));
        });
        centered_row(ui, |ui| {
            if ui.small_button("-4").clicked() {
                len = len.saturating_sub(4);
            }
            if ui.small_button("+4").clicked() {
                len = len.saturating_add(4);
            }
        });
        let len = crate::workflow::snap_wan_length(len);
        self.params.video.length = len;
        ui.vertical_centered(|ui| {
            ui.weak(format!("{len}f ~ {:.1}s at 16fps", len as f32 / 16.0));
        });

        // Seed (shared with the image modes).
        ui.add_space(6.0);
        ui.label("Seed");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.params.randomize_seed, |ui| {
                uint_text_edit_u64(ui, "video_seed", &mut self.params.seed, 0..=u64::MAX);
            });
            ui.checkbox(&mut self.params.randomize_seed, "random");
        });

        // LoRA stacks per expert; strength 0 stays listed but inert.
        ui.add_space(6.0);
        let loras = self.installed_loras.clone();
        video_lora_list(ui, "High noise LoRAs", &mut self.params.video.loras_high, &loras, "vlora_hi");
        ui.add_space(4.0);
        video_lora_list(ui, "Low noise LoRAs", &mut self.params.video.loras_low, &loras, "vlora_lo");

        // Advanced: models, sampler math, post-processing.
        ui.add_space(6.0);
        egui::CollapsingHeader::new("Advanced").id_salt("video_advanced").show(ui, |ui| {
            ui.label("High noise model");
            combo_full(ui, "v_unet_high", &mut self.params.video.unet_high, &self.unets);
            ui.label("Low noise model");
            combo_full(ui, "v_unet_low", &mut self.params.video.unet_low, &self.unets);
            ui.label("Text encoder");
            combo_full(ui, "v_clip", &mut self.params.video.clip_name, &self.clip_files);
            ui.label("VAE");
            combo_full(ui, "v_vae", &mut self.params.video.vae_name, &self.vaes);

            ui.add_space(4.0);
            ui.columns(2, |cols| {
                cols[0].vertical_centered(|ui| {
                    stepper_u32(ui, "Steps", &mut self.params.video.steps, 1..=40, 1);
                });
                cols[1].vertical_centered(|ui| {
                    stepper_u32(ui, "Split", &mut self.params.video.split_step, 1..=40, 1);
                });
            });
            self.params.video.split_step = self.params.video.split_step.min(self.params.video.steps);
            ui.columns(2, |cols| {
                cols[0].vertical_centered(|ui| {
                    stepper_f32(ui, "CFG high", &mut self.params.video.cfg_high, 1.0..=10.0, 0.1);
                });
                cols[1].vertical_centered(|ui| {
                    stepper_f32(ui, "CFG low", &mut self.params.video.cfg_low, 1.0..=10.0, 0.1);
                });
            });
            ui.vertical_centered(|ui| {
                stepper_f32(ui, "Shift", &mut self.params.video.shift, 1.0..=12.0, 0.5);
            });

            ui.add_space(4.0);
            ui.columns(2, |cols| {
                let mut sampler = self.params.video.sampler.clone();
                let mut scheduler = self.params.video.scheduler.clone();
                let samplers = self.samplers.clone();
                let schedulers = self.schedulers.clone();
                cols[0].vertical_centered(|ui| {
                    section_title(ui, "Sampler");
                    combo_full(ui, "v_sampler", &mut sampler, &samplers);
                });
                cols[1].vertical_centered(|ui| {
                    section_title(ui, "Scheduler");
                    combo_full(ui, "v_scheduler", &mut scheduler, &schedulers);
                });
                self.params.video.sampler = sampler;
                self.params.video.scheduler = scheduler;
            });

            ui.add_space(6.0);
            ui.add_enabled_ui(has_rife, |ui| {
                ui.checkbox(&mut self.params.video.rife, "RIFE frame interpolation (2x -> 32fps)");
            });
            if !has_rife {
                ui.weak("Server has no 'RIFE VFI' node — interpolation is skipped.");
            } else if self.params.video.rife {
                stepper_u32(ui, "RIFE x", &mut self.params.video.rife_multiplier, 2..=4, 1);
            }
            if has_clean {
                ui.checkbox(&mut self.params.video.gpu_clean, "Free VRAM between stages");
            }
        });
    }

    /// Import a pack by URL: download the zip and unpack it into the app files dir. `open` forces
    /// the section expanded (from the Model packs status rows); `None` keeps the remembered state.
    #[cfg(feature = "local-npu")]
    fn local_import_ui(&mut self, ui: &mut egui::Ui, host: &Host, open: Option<bool>) {
        self.poll_pack_import(host);
        let busy = self.pack_import_rx.is_some();
        ui.add_space(6.0);
        egui::CollapsingHeader::new("Import a pack").open(open).show(ui, |ui| {
            ui.weak("Paste a direct .zip link (e.g. a HuggingFace resolve URL). Files land in the app files dir and appear above.");
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.pack_name)
                        .desired_width(140.0)
                        .hint_text("folder name"),
                );
            });
            ui.add(
                egui::TextEdit::singleline(&mut self.pack_url)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://.../pack.zip"),
            );
            let name_ok = !self.pack_name.trim().is_empty()
                && !self.pack_name.contains('/')
                && !self.pack_name.starts_with('.');
            let ready = !busy && name_ok && self.pack_url.trim().starts_with("http");
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(ready, egui::Button::new("Download and install")).clicked() {
                    self.start_pack_import(ui.ctx(), host);
                }
                if busy {
                    ui.spinner();
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
                }
            });
            if !self.pack_import_status.is_empty() {
                let st = self.pack_import_status.clone();
                ui.weak(sanitize_ui_text(ui, &st));
            }
        });
    }

    #[cfg(not(feature = "local-npu"))]
    fn local_import_ui(&mut self, _ui: &mut egui::Ui, _host: &Host, _open: Option<bool>) {}

    #[cfg(feature = "local-npu")]
    fn start_pack_import(&mut self, ctx: &egui::Context, host: &Host) {
        let Some(root) = self.external_files_dir(host) else {
            self.pack_import_status = "app files dir unavailable".into();
            return;
        };
        let name = self.pack_name.trim().to_string();
        let url = self.pack_url.trim().to_string();
        self.pack_import_status = "starting...".into();
        self.log.info(format!("local-npu: importing pack '{name}' from {url}"));
        self.pack_import_rx = Some(crate::local_engine::spawn_import(
            url,
            std::path::PathBuf::from(root),
            name,
            ctx.clone(),
        ));
    }

    #[cfg(feature = "local-npu")]
    fn poll_pack_import(&mut self, host: &Host) {
        let Some(rx) = self.pack_import_rx.as_ref() else { return };
        let mut done = false;
        while let Ok(m) = rx.try_recv() {
            match m {
                crate::local_engine::ImportMsg::Progress(s) => self.pack_import_status = s,
                crate::local_engine::ImportMsg::Done(r) => {
                    done = true;
                    match r {
                        Ok(s) => {
                            self.pack_import_status = s.clone();
                            self.log.info(format!("local-npu: {s}"));
                        }
                        Err(e) => {
                            self.pack_import_status = format!("failed: {e}");
                            self.log.error(format!("local-npu: import failed: {e}"));
                        }
                    }
                }
            }
        }
        if done {
            self.pack_import_rx = None;
            self.ensure_local_packs(host, true);
        }
    }

    /// Settings: a status row per known on-device pack kind - each discovered generation pack plus
    /// fixed WD14 / CLIP / Rewriter rows - with a found/missing dot, location, wiped-on-reinstall
    /// flag, and a humanized last-updated time. Missing rows open the URL importer below.
    #[cfg(feature = "local-npu")]
    fn local_pack_status_panel(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.ensure_local_packs(host, false);
        let app_root = self.external_files_dir(host);
        let durable = Self::durable_models_dir();
        let green = egui::Color32::from_rgb(120, 220, 140);
        let warn = egui::Color32::from_rgb(230, 180, 120);
        let dim = ui.visuals().weak_text_color();
        // (kind, backend label, found dir, expected-path hint when missing)
        let mut rows: Vec<(String, Option<String>, Option<std::path::PathBuf>, String)> = Vec::new();
        for p in &self.local_packs {
            rows.push((p.name.clone(), Some(p.backend.label().to_string()), Some(p.dir.clone()), String::new()));
        }
        let gen_count = rows.len();
        rows.push(("WD14 tagger".into(), None, self.wd14_pack.clone(), format!("{durable}/wd14")));
        rows.push(("CLIP embeddings".into(), None, self.clip_pack.clone(), format!("{durable}/clip")));
        rows.push(("Rewriter".into(), None, self.rewrite_pack.clone(), format!("{durable}/rewrite")));
        let mut rescan = false;
        let mut open_import = false;
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong("Model packs");
                let rescan_btn = format!("{} Rescan", icons::REFRESH);
                if ui.small_button(rescan_btn).on_hover_text("Rescan both pack roots").clicked() {
                    rescan = true;
                }
            });
            ui.weak("On-device packs and where they live. App-files packs are wiped on reinstall.");
            ui.add_space(4.0);
            if gen_count == 0 {
                ui.horizontal(|ui| {
                    ui.colored_label(dim, icons::DOT);
                    ui.weak("No generation packs found.");
                });
            }
            let n = rows.len();
            for (i, (kind, backend, dir, hint)) in rows.iter().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(if dir.is_some() { green } else { dim }, icons::DOT);
                    let title = match backend {
                        Some(b) => format!("{kind} ({b})"),
                        None => kind.clone(),
                    };
                    ui.strong(sanitize_ui_text(ui, &title));
                    if let Some(dir) = dir {
                        let base = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
                        let (loc, wiped) = pack_root_note(dir, app_root.as_deref(), durable);
                        ui.weak(sanitize_ui_text(ui, &format!("{base} - {loc}")));
                        if let Some(secs) = self
                            .pack_mtimes
                            .get(dir)
                            .and_then(|m| m.elapsed().ok())
                            .map(|d| d.as_secs())
                        {
                            ui.weak(format!("updated {}", crate::local_engine::humanize_ago(secs)));
                        }
                        if wiped {
                            ui.colored_label(warn, "(wiped on reinstall - move to /sdcard/ComfyUI)");
                        }
                    } else {
                        ui.weak(sanitize_ui_text(ui, &format!("missing - expected {hint}")));
                        if ui.small_button("Import…").clicked() {
                            open_import = true;
                        }
                    }
                });
                if i + 1 < n {
                    ui.add_space(2.0);
                }
            }
        });
        self.local_import_ui(ui, host, open_import.then_some(true));
        if rescan {
            self.ensure_local_packs(host, true);
        }
    }

    /// On-device model list: packs installed in the app files dir, plus test and import.
    #[cfg(feature = "local-npu")]
    fn local_models_section(&mut self, ui: &mut egui::Ui, host: &Host) {
        if !self.local_npu {
            return;
        }
        self.ensure_local_packs(host, false);
        let warn = egui::Color32::from_rgb(230, 180, 120);
        let packs = self.local_packs.clone();
        let use_server = self.local_use_server;
        let mut pick: Option<(String, LocalBackend)> = None;
        let mut server_pick = false;
        let mut rescan = false;
        let mut test: Option<std::path::PathBuf> = None;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong("Create model");
                if ui.small_button(icons::REFRESH).on_hover_text("Rescan the app files dir").clicked() {
                    rescan = true;
                }
            });
            ui.weak("A pack runs on the NPU; 'Server model' sends generation to the server while the NPU features stay on.");
            ui.add_space(4.0);
            let srv_label = if use_server {
                format!("{} Server model", icons::CHECK)
            } else {
                "     Server model".to_string()
            };
            if ui.selectable_label(use_server, sanitize_ui_text(ui, &srv_label)).clicked() {
                server_pick = true;
            }
            if packs.is_empty() {
                ui.colored_label(warn, "No packs installed yet - import one below.");
            }
            for p in &packs {
                let on = !use_server && p.name == self.local_pack && p.backend == self.local_backend;
                let label = if on {
                    format!("{} {}", icons::CHECK, p.label())
                } else {
                    format!("     {}", p.label())
                };
                if ui.selectable_label(on, sanitize_ui_text(ui, &label)).clicked() {
                    pick = Some((p.name.clone(), p.backend));
                }
            }
            if !use_server && let Some(sel) = self.selected_pack().cloned() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    let can_test = sel.backend == LocalBackend::Anima && !self.d3_running;
                    if ui
                        .add_enabled(can_test, egui::Button::new(format!("{} Test pack", icons::RUN)))
                        .on_hover_text("Two-step render to prove this pack loads and produces an image")
                        .clicked()
                    {
                        test = Some(sel.dir.clone());
                    }
                    if self.d3_running {
                        ui.spinner();
                        ui.weak("testing...");
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                    }
                    if let Some(ok) = self.d3_ok {
                        let (c, t) = if ok {
                            (egui::Color32::from_rgb(120, 220, 140), "pack OK")
                        } else {
                            (egui::Color32::from_rgb(230, 120, 120), "pack FAILED")
                        };
                        ui.colored_label(c, t);
                    }
                });
                ui.weak(sanitize_ui_text(ui, &elide(&sel.dir.display().to_string(), 64)));
            }
        });
        self.local_import_ui(ui, host, None);
        ui.add_space(6.0);
        let eff_server = if server_pick { true } else if pick.is_some() { false } else { use_server };
        ui.weak(if eff_server {
            "Server models (used — 'Server model' selected above)"
        } else {
            "Server models (not used — a local pack is selected above)"
        });
        ui.add_space(2.0);
        if rescan {
            self.ensure_local_packs(host, true);
        }
        if server_pick && !self.local_use_server {
            self.local_use_server = true;
            crate::local_engine::drop_cache();
            self.log.info("local-npu: Create model -> server (NPU features stay on)");
        }
        if let Some((name, backend)) = pick
            && (self.local_use_server || name != self.local_pack || backend != self.local_backend)
        {
            self.local_use_server = false;
            self.local_pack = name;
            self.local_backend = backend;
            crate::local_engine::drop_cache();
            self.log.info(format!(
                "local-npu: pack -> {} ({}), asset caches dropped",
                self.local_pack,
                self.local_backend.label()
            ));
        }
        if let Some(dir) = test
            && let Some(lib) = host.native_lib_dir()
        {
            self.start_d3_anima(ui.ctx(), lib, dir);
        }
    }

    #[cfg(not(feature = "local-npu"))]
    fn local_models_section(&mut self, _ui: &mut egui::Ui, _host: &Host) {}

    fn create_models_pane(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.local_models_section(ui, host);
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

    fn create_characters_pane(&mut self, ui: &mut egui::Ui, host: &Host) {
        let list_w = (ui.clip_rect().width() - 12.0).clamp(160.0, ui.available_width());
        ui.set_max_width(list_w);

        if self.character_draft.is_some() {
            self.character_editor(ui, host, list_w);
            return;
        }

        ui.horizontal(|ui| {
            if ui
                .button(format!("{} New", icons::ADD))
                .on_hover_text("Create a character card")
                .clicked()
            {
                self.character_draft =
                    Some(CharacterDraft { editing: None, card: CharacterCard::default() });
            }
            if ui.button("Import").on_hover_text("Paste a shared character pack").clicked() {
                self.import_character(host);
            }
        });

        if let Some(active) = self.active_character.as_ref().map(|a| a.name.clone()) {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.weak(format!("{} Active: {}", icons::CHECK, elide(&active, 22)));
                if ui.small_button("Remove").clicked() {
                    self.remove_active_character();
                    host.haptic(Haptic::Light);
                }
            });
        }

        if self.characters.is_empty() {
            ui.add_space(4.0);
            ui.weak(format!(
                "No characters yet — tap {} New, or Save as character from a gallery image.",
                icons::ADD
            ));
            return;
        }

        enum Act {
            Apply(usize),
            Remove,
            Edit(usize),
            Share(usize),
            Delete(String),
            Find(usize),
            Suggestions(usize),
        }
        let active = self.active_character.as_ref().map(|a| a.name.clone());
        let mut act: Option<Act> = None;
        for (i, card) in self.characters.clone().iter().enumerate() {
            let is_active = active.as_deref() == Some(card.name.as_str());
            let header = if is_active {
                format!("{} {}", icons::CHECK, elide(&card.name, 26))
            } else {
                elide(&card.name, 30)
            };
            let suggested = self.character_suggestions.get(&card.name).map(|s| s.len()).unwrap_or(0);
            ui.group(|ui| {
                ui.set_max_width(list_w - 8.0);
                ui.horizontal(|ui| {
                    // Profile square on the left; falls back to no square when unset.
                    if !card.portrait_key.is_empty() {
                        self.portrait_thumb(ui, &card.portrait_key, 38.0);
                        ui.add_space(4.0);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if is_active {
                            if ui.small_button("Remove").clicked() {
                                act = Some(Act::Remove);
                            }
                        } else if ui.small_button("Apply").clicked() {
                            act = Some(Act::Apply(i));
                        }
                        ui.add_space(4.0);
                        let max_w = (ui.available_width() - 4.0).max(32.0);
                        let title = elide_width(ui, &sanitize_ui_text(ui, &header), max_w);
                        ui.strong(title);
                    });
                });
                ui.horizontal(|ui| {
                    if ui
                        .small_button(format!("{} Find images", icons::SEARCH))
                        .on_hover_text("Rank gallery images by CLIP similarity, then review them")
                        .clicked()
                    {
                        act = Some(Act::Find(i));
                    }
                    if suggested > 0
                        && ui
                            .small_button(format!("{} {suggested} suggested", icons::STAR))
                            .on_hover_text("Review images auto-matched to this character")
                            .clicked()
                    {
                        act = Some(Act::Suggestions(i));
                    }
                });
                egui::CollapsingHeader::new("Details")
                    .id_salt(("character_row", card.name.as_str()))
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.set_max_width(list_w - 24.0);
                        character_meta_body(ui, card);
                        ui.horizontal(|ui| {
                            if ui.small_button(format!("{} Edit", icons::STYLUS)).clicked() {
                                act = Some(Act::Edit(i));
                            }
                            if ui
                                .small_button("Share")
                                .on_hover_text("Copy this card as a pack")
                                .clicked()
                            {
                                act = Some(Act::Share(i));
                            }
                            if ui.small_button(icons::TRASH).clicked() {
                                act = Some(Act::Delete(card.name.clone()));
                            }
                        });
                    });
            });
        }
        match act {
            Some(Act::Apply(i)) => {
                self.apply_character(i);
                host.haptic(Haptic::Light);
            }
            Some(Act::Remove) => {
                self.remove_active_character();
                host.haptic(Haptic::Light);
            }
            Some(Act::Edit(i)) => {
                if let Some(card) = self.characters.get(i).cloned() {
                    self.character_draft =
                        Some(CharacterDraft { editing: Some(card.name.clone()), card });
                }
            }
            Some(Act::Share(i)) => {
                if let Some(card) = self.characters.get(i).cloned() {
                    host.copy_text(CharacterPack { card }.to_clipboard_json());
                    host.haptic(Haptic::Light);
                    self.status = "Character copied".into();
                }
            }
            Some(Act::Delete(name)) => {
                if self.active_character.as_ref().is_some_and(|a| a.name == name) {
                    self.remove_active_character();
                }
                self.characters.retain(|c| c.name != name);
                self.character_denied.remove(&name);
                self.character_suggestions.remove(&name);
                self.character_centroids.remove(&name);
                host.haptic(Haptic::Warning);
            }
            Some(Act::Find(i)) => {
                if let Some(card) = self.characters.get(i).cloned() {
                    self.find_character_images(card, host);
                }
            }
            Some(Act::Suggestions(i)) => {
                if let Some(card) = self.characters.get(i).cloned() {
                    let keys = self.character_suggestions.get(&card.name).cloned().unwrap_or_default();
                    if keys.is_empty() {
                        self.status = "No suggestions to review".into();
                    } else {
                        self.open_character_review(card.name, keys, host);
                    }
                }
            }
            None => {}
        }
    }

    /// The card editor, shown in place of the list while [`Self::character_draft`] is set.
    fn character_editor(&mut self, ui: &mut egui::Ui, host: &Host, list_w: f32) {
        let Some(mut draft) = self.character_draft.take() else { return };
        let w = list_w - 8.0;
        ui.heading(if draft.editing.is_some() { "Edit character" } else { "New character" });

        ui.add_space(4.0);
        ui.label("Name");
        ui.add(
            egui::TextEdit::singleline(&mut draft.card.name)
                .hint_text("character name")
                .desired_width(w),
        );

        ui.add_space(4.0);
        ui.label("Identity tags");
        ui.add(
            egui::TextEdit::multiline(&mut draft.card.identity)
                .hint_text("1girl, silver hair, red eyes, twin braids")
                .desired_width(w)
                .desired_rows(2),
        );

        ui.add_space(4.0);
        ui.label("Trigger words");
        ui.add(
            egui::TextEdit::singleline(&mut draft.card.triggers)
                .hint_text("LoRA activator tokens")
                .desired_width(w),
        );

        ui.add_space(4.0);
        ui.label("Negatives");
        ui.add(
            egui::TextEdit::singleline(&mut draft.card.negatives)
                .hint_text("per-character negatives")
                .desired_width(w),
        );

        ui.add_space(4.0);
        ui.label("Face prompt");
        ui.add(
            egui::TextEdit::singleline(&mut draft.card.face_prompt)
                .hint_text("optional; fed to the Face fix app")
                .desired_width(w),
        );

        ui.add_space(6.0);
        ui.separator();
        ui.label("Preferred model");
        let cur_label = if draft.card.checkpoint.trim().is_empty() {
            "none".to_string()
        } else {
            elide(&sanitize_ui_text(ui, file_basename(&draft.card.checkpoint)), 32)
        };
        egui::ComboBox::from_id_salt("char_model")
            .selected_text(cur_label)
            .width(w)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.card.checkpoint, String::new(), "none");
                let target = &mut draft.card.checkpoint;
                let mut row = |ui: &mut egui::Ui, file: &str| {
                    let base = elide(&sanitize_ui_text(ui, file_basename(file)), 44);
                    ui.selectable_value(target, file.to_string(), base)
                        .on_hover_text(sanitize_ui_text(ui, file));
                };
                for f in self.checkpoints.iter().take(300) {
                    row(ui, f);
                }
                if !self.unets.is_empty() {
                    ui.separator();
                    ui.weak("Diffusion");
                    for f in self.unets.iter().take(300) {
                        row(ui, f);
                    }
                }
            });
        ui.horizontal(|ui| {
            if ui
                .small_button("Use current model")
                .on_hover_text("Copy the model selected in Create")
                .clicked()
            {
                draft.card.checkpoint = self.params.model_file().to_string();
            }
            if !draft.card.checkpoint.trim().is_empty() && ui.small_button("Clear").clicked() {
                draft.card.checkpoint.clear();
                draft.card.switch_checkpoint = false;
            }
        });
        if !draft.card.checkpoint.trim().is_empty() {
            ui.checkbox(&mut draft.card.switch_checkpoint, "Switch to this checkpoint on apply");
        }

        ui.add_space(6.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!("LoRAs ({})", draft.card.loras.len()));
            if ui
                .small_button("Capture current stack")
                .on_hover_text("Copy the active LoRA stack from Create")
                .clicked()
            {
                draft.card.loras = self
                    .params
                    .loras
                    .iter()
                    .map(|l| ActiveLora { injected: String::new(), ..l.clone() })
                    .collect();
            }
        });
        let mut drop_lora: Option<usize> = None;
        for (i, lora) in draft.card.loras.clone().iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button(icons::CLOSE).clicked() {
                    drop_lora = Some(i);
                }
                ui.weak(sanitize_ui_text(
                    ui,
                    &format!("{} @{:.2}", file_basename(&lora.file), lora.strength_model),
                ));
            });
        }
        if let Some(i) = drop_lora {
            draft.card.loras.remove(i);
        }

        ui.add_space(8.0);
        let named = !draft.card.name.trim().is_empty();
        let mut close = false;
        ui.horizontal(|ui| {
            if ui.add_enabled(named, egui::Button::new(format!("{} Save", icons::SAVE))).clicked() {
                let mut card = draft.card.clone();
                card.name = card.name.trim().to_string();
                self.save_character(draft.editing.clone(), card);
                host.haptic(Haptic::Success);
                close = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
        if !named {
            ui.weak("Name the character to save.");
        }
        if !close {
            self.character_draft = Some(draft);
        }
    }

    fn import_character(&mut self, host: &Host) {
        let pack = host.clipboard_text().as_deref().and_then(CharacterPack::from_clipboard_json);
        let Some(pack) = pack else {
            self.status = "No character pack on the clipboard".into();
            host.haptic(Haptic::Warning);
            return;
        };
        // Profile picture and album id reference the sharer's account; drop them on import.
        let mut card = pack.card;
        card.portrait_key.clear();
        card.album_id = 0;
        // Open the imported card in the editor for review before saving.
        self.character_draft = Some(CharacterDraft { editing: None, card });
        self.status = "Character imported — review and save".into();
        host.haptic(Haptic::Light);
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
        self.apply_recommended_settings(file);
        if kind == ModelKind::Diffusion {
            self.resolve_companions(Companions::Seed);
        }
        self.selected_preset.clear();
        self.touch_checkpoint_recent(file);
    }

    /// Overwrite sampler / steps / cfg / size from `file`'s catalog recommendation, where present.
    fn apply_recommended_settings(&mut self, file: &str) {
        let Some(rec) = self
            .checkpoint_catalog
            .entry(file)
            .and_then(|e| e.recommended.as_ref())
            .cloned()
        else {
            return;
        };
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
        if let Some(name) = rec.sampler.as_ref().and_then(|s| match_sampler_name(s, &self.samplers)) {
            self.params.sampler = name;
        }
        if let Some(name) =
            rec.scheduler.as_ref().and_then(|s| match_sampler_name(s, &self.schedulers))
        {
            self.params.scheduler = name;
        }
    }

    /// Remove the active character, clear all Create-tab creative state, then re-seed the current
    /// model's recommended settings (or the selected local pack's defaults).
    fn reset_create(&mut self, host: &Host) {
        self.remove_active_character();
        self.params.reset_creative();
        self.picked_input = None;
        self.picked_input_grid_open = false;
        self.inpaint = None;
        self.img2img_url_tex = None;
        self.img2img_url_key.clear();
        self.img2img_url_req.clear();
        self.img2img_url_err.clear();
        self.img2img_url_loading = false;
        self.selected_preset.clear();
        self.seed_reset_recommendation();
        self.note = "Reset to model defaults".into();
        host.haptic(Haptic::Warning);
    }

    /// Re-seed sampler / steps / cfg / size after a reset from the current model's recommendation.
    #[cfg(feature = "local-npu")]
    fn seed_reset_recommendation(&mut self) {
        if self.route_local_gen() {
            if let Some(entry) = self.selected_pack().cloned() {
                let d = crate::local_engine::local_defaults(&entry);
                self.params.width = d.width;
                self.params.height = d.height;
                self.params.steps = d.steps;
                self.params.cfg = d.cfg;
                self.params.scheduler = d.scheduler;
            }
            return;
        }
        let file = self.params.model_file().to_string();
        self.apply_recommended_settings(&file);
    }

    #[cfg(not(feature = "local-npu"))]
    fn seed_reset_recommendation(&mut self) {
        let file = self.params.model_file().to_string();
        self.apply_recommended_settings(&file);
    }

    /// Two-tap Reset: first tap arms ("Sure?"), a second within the window clears Create.
    fn reset_button(&mut self, ui: &mut egui::Ui, host: &Host) {
        let now = ui.input(|i| i.time);
        let armed = self.reset_armed_at.map(|t| now - t < RESET_CONFIRM_SECS).unwrap_or(false);
        if !armed {
            self.reset_armed_at = None;
        }
        let resp = ui.small_button(if armed { "Sure?" } else { "Reset" });
        if armed {
            ui.ctx().request_repaint_after(Duration::from_secs_f64(RESET_CONFIRM_SECS));
        }
        if resp.clicked() {
            if armed {
                self.reset_armed_at = None;
                self.reset_create(host);
            } else {
                self.reset_armed_at = Some(now);
                host.haptic(Haptic::Light);
            }
        }
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
        self.params.loras = dedupe_loras(std::mem::take(&mut self.params.loras));

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
                    if let Some(meta) = meta.as_ref() {
                        ui.weak(sanitize_ui_text(ui, &format!("rec: {}", meta.strength_hint())));
                    }
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

    /// Undo/redo, floating at the TOP right — far from the queue/lock stack at the bottom,
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
        let undo_w = crate::theme::FAB_SIZE * 2.0 + 8.0;
        egui::Area::new(egui::Id::new("comfy-undo"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(view.right() - 10.0 - undo_w, view.top() + 10.0))
            .show(ui.ctx(), |aui| {
                aui.spacing_mut().item_spacing.x = 8.0;
                aui.horizontal(|aui| {
                    for (icon, tip, enabled, act) in [
                        (icons::UNDO, "Undo", can_undo, true),
                        (icons::REDO, "Redo", can_redo, false),
                    ] {
                        aui.add_enabled_ui(enabled, |aui| {
                            if crate::theme::fab(aui, icon, crate::theme::fab_bg())
                                .on_hover_text(tip)
                                .clicked()
                            {
                                action = Some(act);
                            }
                        });
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
            ui.add_space(130.0);
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
        ui.add_space(130.0);
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
        centered(ctx, egui::Window::new(title).open(&mut open)).show(ctx, |ui| {
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
        centered(ctx, egui::Window::new("Save tab as app").open(&mut open)).show(ctx, |ui| {
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
        self.status = format!("{} LoRA(s) pasted", self.params.loras.len());
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
        self.params.loras = dedupe_loras(pack.loras.clone());
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
            loras: dedupe_loras(
                meta
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
            ),
        };
        host.copy_text(pack.to_clipboard_json());
        self.lora_clip = Some(pack);
        self.gallery_status = "LoRAs copied".into();
        host.haptic(Haptic::Light);
    }

    fn apply_image_meta(&mut self, meta: &ImageMeta) {
        self.apply_image_meta_sel(meta, RemixApply::ALL);
    }

    /// Write only the `sel`-enabled fields of `meta` into Params; unchecked slots keep their value.
    fn apply_image_meta_sel(&mut self, meta: &ImageMeta, sel: RemixApply) {
        // A UNET in the graph means the diffusion topology; the image's own encoders and VAE beat
        // whatever select_model would have seeded.
        if sel.model {
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
        }
        if sel.positive {
            if let Some(p) = &meta.positive {
                self.params.positive = p.clone();
            }
        }
        if sel.negative {
            if let Some(n) = &meta.negative {
                self.params.negative = n.clone();
            }
        }
        self.apply_sampler_pack(&SamplerPack {
            sampler: sel.sampler.then(|| meta.sampler.clone()).flatten(),
            scheduler: sel.scheduler.then(|| meta.scheduler.clone()).flatten(),
            steps: sel.steps.then(|| meta.steps.map(|n| n as u32)).flatten(),
            cfg: sel.cfg.then(|| meta.cfg.map(|n| n as f32)).flatten(),
        });
        if sel.seed {
            if let Some(v) = meta.seed.filter(|&s| s >= 0) {
                self.params.seed = v as u64;
                self.params.randomize_seed = false;
            }
        }
        if sel.loras {
            self.params.lora_triggers.clear();
            self.apply_lora_pack(&LoraPack { loras: gallery::meta_to_active_loras(&meta.loras) });
            // Workflow positives usually bake LoRA tags into the CLIP text — split them back out.
            self.pull_lora_triggers_from_positive();
        }
        self.create_pane = CreatePane::Main;
    }

    /// Load a gallery image's scraped meta into Create for an exact re-generation.
    fn remix_from_meta(&mut self, meta: &ImageMeta) {
        self.remix_from_meta_sel(meta, RemixApply::ALL);
    }

    /// Remix, applying only the `sel`-enabled fields, then repair companions and jump to Create.
    fn remix_from_meta_sel(&mut self, meta: &ImageMeta, sel: RemixApply) {
        self.apply_image_meta_sel(meta, sel);
        // Repair a diffusion model's companions against this server's installed files; when the
        // model row is unchecked this ports the prompt / LoRAs onto the current checkpoint.
        if self.params.model_kind == ModelKind::Diffusion {
            self.resolve_companions(Companions::Repair);
        }
        // Disable seed randomization so the seed reproduces.
        if sel.seed {
            self.params.randomize_seed = false;
        }
        self.selected_preset.clear();
        self.tab = Tab::Generate;
        self.note = "Remixed into Create".into();
    }

    /// Build the per-field remix diff sheet for `meta`, capturing the viewer image for img2img reuse.
    fn open_remix_sheet(&mut self, meta: ImageMeta) {
        let rows = gallery::remix_diff_rows(&meta, &self.params);
        let enabled = vec![true; rows.len()];
        let input = match self.viewer.as_ref() {
            Some(v) if v.bytes.is_some() => {
                RemixInput::Picked { name: v.item.filename.clone(), bytes: v.bytes.clone().unwrap() }
            }
            Some(v) => {
                match self.engine.as_ref().and_then(|e| e.view_url(&v.item.subfolder, &v.item.filename))
                {
                    Some(url) => RemixInput::Url(url),
                    None => RemixInput::None,
                }
            }
            None => RemixInput::None,
        };
        self.remix_sheet = Some(RemixSheet { meta, rows, enabled, input, seeds: 6 });
    }

    /// Map the sheet's checked rows onto a [`RemixApply`]; unlisted (unchanged) fields stay off.
    fn remix_apply_from_sheet(sheet: &RemixSheet) -> RemixApply {
        let mut sel = RemixApply::NONE;
        for (row, on) in sheet.rows.iter().zip(&sheet.enabled) {
            if *on {
                sel.set(row.field, true);
            }
        }
        sel
    }

    /// Set the remembered gallery image as the img2img input, defaulting denoise for refining.
    fn remix_set_img2img(&mut self, ctx: &egui::Context, input: RemixInput) {
        match input {
            RemixInput::Picked { name, bytes } => {
                self.set_picked_input(ctx, name, bytes);
                self.params.mode = Mode::Img2Img;
                self.params.img2img_source = Img2ImgSource::Picked;
            }
            RemixInput::Url(url) => {
                self.params.mode = Mode::Img2Img;
                self.params.img2img_source = Img2ImgSource::Url;
                self.params.input_url = url;
            }
            RemixInput::None => return,
        }
        // The meta carries no denoise; back a full-strength value off so img2img actually refines.
        if self.params.denoise >= 0.9 {
            self.params.denoise = 0.6;
        }
        self.note = "Remixed as img2img".into();
    }

    /// Queue `n` full-quality jobs at seed+1..=seed+n using the image's exact meta.
    fn queue_neighbor_seeds(&mut self, ctx: &egui::Context, host: &Host, meta: &ImageMeta, n: usize) {
        self.apply_image_meta(meta);
        if self.params.model_kind == ModelKind::Diffusion {
            self.resolve_companions(Companions::Repair);
        }
        self.params.randomize_seed = false;
        self.selected_preset.clear();
        if let Err(e) = self.can_queue_create() {
            self.status = e.into();
            host.haptic(Haptic::Warning);
            return;
        }
        let n = n.clamp(1, 8);
        let base = self.params.seed;
        for i in 1..=n as u64 {
            self.params.seed = base.wrapping_add(i);
            self.start_generation(ctx, host);
        }
        // Restore the source seed so the Create tab still shows the image's own.
        self.params.seed = base;
        self.tab = Tab::Generate;
        self.note = format!("Queued {n} neighbor seeds");
    }

    /// Tear down the fullscreen viewer and any remix sheet, remembering the gallery scroll.
    fn close_viewer(&mut self) {
        self.gallery_scroll_restore = Some(self.gallery_scroll_y);
        self.viewer = None;
        self.player = None;
        self.viewer_swipe_origin = None;
        self.viewer_remix_pending = false;
        self.remix_sheet = None;
        self.finish_sheet = None;
        self.gallery_status.clear();
    }

    /// Why the video finish pass can't run, if anything is missing. `None` means it can.
    fn finish_disabled_reason(&self) -> Option<&'static str> {
        let Some(schemas) = self.schemas.as_ref() else {
            return Some("Connect to the server first");
        };
        if !schemas.has_node("VHS_LoadVideoPath") {
            return Some("This server has no VHS_LoadVideoPath node");
        }
        if !schemas.has_node("VHS_VideoCombine") {
            return Some("This server has no VHS_VideoCombine node");
        }
        None
    }

    /// Video "Finish pass" sheet: reference source, scale, RIFE multiplier and fps, then Queue.
    fn finish_sheet_window(&mut self, ctx: &egui::Context, host: &Host) {
        if self.finish_sheet.is_none() {
            return;
        }
        enum FAct {
            Queue,
            Cancel,
        }
        let mut open = true;
        let mut act: Option<FAct> = None;
        let sheet = self.finish_sheet.as_ref().unwrap();
        let mut ref_source = sheet.ref_source;
        let mut scale_by = sheet.scale_by;
        let mut rife = sheet.rife_multiplier;
        let mut fps = sheet.output_fps;
        let mut picked = sheet.picked.clone();
        let has_input = self.picked_input.is_some();
        // Colour-match needs a reference only when the server has the node.
        let want_ref = self.schemas.as_ref().is_some_and(|s| s.has_node("easy imageColorMatch"));
        let has_rife = self.schemas.as_ref().is_some_and(|s| s.has_node("RIFE VFI"));
        let mut open_picker = false;
        centered(ctx, egui::Window::new(format!("{} Finish pass", icons::GENERATE)))
            .collapsible(false)
            .open(&mut open)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.weak("Colour-match, upscale, interpolate and re-encode this video server-side.");
                ui.add_space(6.0);
                if want_ref {
                    ui.strong("Reference frame (colour-match)");
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(has_input, |ui| {
                            ui.selectable_value(
                                &mut ref_source,
                                FinishRef::CurrentInput,
                                "Current input image",
                            );
                        });
                        ui.selectable_value(&mut ref_source, FinishRef::Pick, "Pick photo");
                    });
                    match ref_source {
                        FinishRef::CurrentInput if !has_input => {
                            ui.weak("No current input image — pick a photo instead.");
                        }
                        FinishRef::Pick => {
                            if let Some((name, _)) = &picked {
                                ui.weak(format!("Reference: {}", elide(name, 32)));
                            }
                            open_picker = true;
                        }
                        _ => {}
                    }
                } else {
                    ui.weak("This server has no colour-match node — that step is skipped.");
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Scale");
                    ui.selectable_value(&mut scale_by, 1.0, "1x");
                    ui.selectable_value(&mut scale_by, 1.5, "1.5x");
                    ui.selectable_value(&mut scale_by, 2.0, "2x");
                });
                ui.horizontal(|ui| {
                    ui.label("RIFE multiplier");
                    if ui.small_button("-").clicked() {
                        rife = rife.saturating_sub(1).max(1);
                    }
                    ui.monospace(rife.to_string());
                    if ui.small_button("+").clicked() {
                        rife = (rife + 1).min(8);
                    }
                    if !has_rife {
                        ui.weak("(no RIFE node — interpolation skipped)");
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Output fps");
                    if ui.small_button("-").clicked() {
                        fps = fps.saturating_sub(1).max(1);
                    }
                    ui.monospace(fps.to_string());
                    if ui.small_button("+").clicked() {
                        fps = (fps + 1).min(120);
                    }
                });
                if open_picker
                    && let Some((id, name)) = self.device_photo_grid(ui, host)
                {
                    match host.load_device_image(id) {
                        Some(bytes) if !bytes.is_empty() => {
                            let fname =
                                if name.is_empty() { format!("device_{id}.jpg") } else { name };
                            picked = Some((fname, bytes));
                            host.haptic(Haptic::Light);
                        }
                        _ => self.note = "Couldn't read that photo from the device".into(),
                    }
                }
                // Colour-match requires a resolved reference; the node's absence lifts that.
                let ref_ready = !want_ref
                    || matches!(ref_source, FinishRef::CurrentInput if has_input)
                    || (ref_source == FinishRef::Pick && picked.is_some());
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    let queue = ui.add_enabled(
                        ref_ready,
                        egui::Button::new(format!("{} Queue", icons::RUN)),
                    );
                    if queue
                        .on_hover_text(if ref_ready {
                            "Queue the finish pass; the result lands in the Gallery"
                        } else {
                            "Pick a reference frame for colour-match first"
                        })
                        .clicked()
                    {
                        act = Some(FAct::Queue);
                    }
                    if ui.button("Cancel").clicked() {
                        act = Some(FAct::Cancel);
                    }
                });
            });
        if let Some(s) = self.finish_sheet.as_mut() {
            s.ref_source = ref_source;
            s.scale_by = scale_by;
            s.rife_multiplier = rife;
            s.output_fps = fps;
            s.picked = picked;
        }
        match act {
            Some(FAct::Queue) => {
                let Some(sheet) = self.finish_sheet.take() else { return };
                self.queue_finish(&sheet, want_ref, host);
            }
            Some(FAct::Cancel) => self.finish_sheet = None,
            None => {
                if !open {
                    self.finish_sheet = None;
                }
            }
        }
    }

    /// Resolve the reference bytes and queue the finish pass on the engine.
    fn queue_finish(&mut self, sheet: &FinishSheet, want_ref: bool, host: &Host) {
        if let Some(reason) = self.finish_disabled_reason() {
            self.gallery_status = reason.into();
            host.haptic(Haptic::Warning);
            return;
        }
        // Only upload a reference when colour-match will actually use it.
        let reference = want_ref
            .then(|| match sheet.ref_source {
                FinishRef::CurrentInput => self.picked_input.as_ref().map(|p| p.bytes.clone()),
                FinishRef::Pick => sheet.picked.as_ref().map(|(_, b)| b.clone()),
            })
            .flatten();
        let schemas = self.schemas.clone().unwrap_or_default();
        self.pending_job_labels.push_back("Video finish".into());
        self.engine.as_mut().unwrap().run_finish(
            sheet.video_path.clone(),
            reference,
            sheet.scale_by,
            sheet.rife_multiplier,
            sheet.output_fps,
            schemas,
        );
        self.finish_pending = true;
        self.running = true;
        self.jobs_left += 1;
        self.gallery_status = "Finishing video — it'll appear in the Gallery when done".into();
        host.haptic(Haptic::Medium);
    }

    /// Per-field remix diff sheet: toggle which of an image's settings port into Create.
    fn remix_sheet_window(&mut self, ctx: &egui::Context, host: &Host) {
        if self.remix_sheet.is_none() {
            return;
        }
        #[derive(Clone, Copy)]
        enum SAct {
            Apply,
            ApplyImg2Img,
            Seeds,
            Cancel,
        }
        let mut open = true;
        let mut act: Option<SAct> = None;
        let sheet = self.remix_sheet.as_ref().unwrap();
        let mut toggles = sheet.enabled.clone();
        let mut seeds = sheet.seeds;
        let has_input = !matches!(sheet.input, RemixInput::None);
        let rows = &sheet.rows;
        // The window never exceeds 70% of the app height and never the viewport width;
        // the row list scrolls inside whatever the chrome leaves.
        let max_win_h = ctx.content_rect().height() * 0.70;
        let max_h = (max_win_h - 190.0).max(96.0);
        let win_w = (ctx.content_rect().width() - 24.0).clamp(240.0, 380.0);
        let body_w = win_w - 28.0;
        centered(ctx, egui::Window::new(format!("{} Remix", icons::GENERATE)))
            .collapsible(false)
            .open(&mut open)
            .default_width(win_w)
            .max_width(win_w)
            .max_height(max_win_h)
            .show(ctx, |ui| {
                ui.set_max_width(body_w);
                if rows.is_empty() {
                    ui.weak("These settings already match the current Create tab.");
                } else {
                    ui.weak("Pick which settings to port into Create.");
                    ui.add_space(4.0);
                    crate::theme::scroll_vertical().max_height(max_h).show(ui, |ui| {
                        ui.set_max_width(body_w);
                        for (i, row) in rows.iter().enumerate() {
                            let mut on = toggles[i];
                            ui.checkbox(&mut on, row.label);
                            toggles[i] = on;
                            ui.indent(("remix_row", i), |ui| {
                                ui.set_max_width(body_w - 20.0);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(elide(&row.current, 120)).weak().small(),
                                    )
                                    .wrap(),
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("-> {}", elide(&row.new, 120))).small(),
                                    )
                                    .wrap(),
                                );
                            });
                            ui.add_space(2.0);
                        }
                    });
                }
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(format!("{} Apply", icons::CHECK))).clicked() {
                        act = Some(SAct::Apply);
                    }
                    if ui
                        .add_enabled(
                            has_input,
                            egui::Button::new(format!("{} img2img", icons::IMAGE)),
                        )
                        .on_hover_text("Apply and set this image as the img2img input")
                        .clicked()
                    {
                        act = Some(SAct::ApplyImg2Img);
                    }
                    if ui.button("Cancel").clicked() {
                        act = Some(SAct::Cancel);
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Neighbor seeds");
                    if ui.small_button("-").clicked() {
                        seeds = seeds.saturating_sub(1).max(1);
                    }
                    ui.monospace(seeds.to_string());
                    if ui.small_button("+").clicked() {
                        seeds = (seeds + 1).min(8);
                    }
                    if ui
                        .add(egui::Button::new(format!("{} Queue", icons::RUN)))
                        .on_hover_text(
                            "Queue seed+1..seed+N at full quality from this image's exact settings",
                        )
                        .clicked()
                    {
                        act = Some(SAct::Seeds);
                    }
                });
            });
        if let Some(s) = self.remix_sheet.as_mut() {
            s.enabled = toggles;
            s.seeds = seeds;
        }
        match act {
            Some(SAct::Apply) | Some(SAct::ApplyImg2Img) => {
                let Some(sheet) = self.remix_sheet.take() else { return };
                let sel = Self::remix_apply_from_sheet(&sheet);
                self.remix_from_meta_sel(&sheet.meta, sel);
                if matches!(act, Some(SAct::ApplyImg2Img)) {
                    self.remix_set_img2img(ctx, sheet.input);
                }
                self.close_viewer();
                host.haptic(Haptic::Light);
            }
            Some(SAct::Seeds) => {
                let Some(sheet) = self.remix_sheet.take() else { return };
                self.queue_neighbor_seeds(ctx, host, &sheet.meta, sheet.seeds);
                self.close_viewer();
            }
            Some(SAct::Cancel) => self.remix_sheet = None,
            None => {
                if !open {
                    self.remix_sheet = None;
                }
            }
        }
    }

    /// Pending-jobs queue sheet: the running job (marked) then pending jobs in order, each with a
    /// Cancel, plus a two-tap "Clear pending" footer. Reads the latest `GET /queue` snapshot, which
    /// the poll refreshes while the sheet is open.
    fn queue_sheet_window(&mut self, ctx: &egui::Context, host: &Host) {
        if !self.queue_sheet_open {
            return;
        }
        enum QAct {
            Interrupt,
            Delete(String),
            Clear,
        }
        let mut open = true;
        let mut act: Option<QAct> = None;
        let mut clear_arm = self.queue_clear_arm;
        let (running, pending) = &self.queue_jobs;
        let labels: HashMap<&str, &str> =
            self.my_prompts.iter().map(|p| (p.id.as_str(), p.label.as_str())).collect();
        let row_label = |job: &QueueJob| -> String {
            match labels.get(job.prompt_id.as_str()) {
                Some(l) => format!("Yours · {l}"),
                None => job.prompt_id.chars().take(8).collect(),
            }
        };
        let total = running.len() + pending.len();
        let mut close = false;
        let max_h = (ctx.content_rect().height() * 0.5).clamp(160.0, 360.0);
        centered(ctx, egui::Window::new(format!("{} Queue", icons::RUN)))
            .collapsible(false)
            .open(&mut open)
            .default_width(360.0)
            .show(ctx, |ui| {
                if total == 0 {
                    ui.weak("The server queue is empty.");
                } else {
                    ui.weak(format!("{total} job(s) on the server."));
                    ui.add_space(4.0);
                    crate::theme::scroll_vertical().show(ui, |ui| {
                        ui.set_max_height(max_h);
                        ui.set_min_width(320.0);
                        let mut pos = 0usize;
                        for job in running {
                            pos += 1;
                            ui.horizontal(|ui| {
                                ui.strong(format!("{pos}."));
                                ui.colored_label(
                                    egui::Color32::from_rgb(120, 200, 140),
                                    "Running",
                                );
                                ui.label(elide(&row_label(job), 32));
                                if ui.small_button("Cancel").clicked() {
                                    act = Some(QAct::Interrupt);
                                }
                            });
                        }
                        for job in pending {
                            pos += 1;
                            ui.horizontal(|ui| {
                                ui.strong(format!("{pos}."));
                                ui.label(elide(&row_label(job), 40));
                                if ui.small_button("Cancel").clicked() {
                                    act = Some(QAct::Delete(job.prompt_id.clone()));
                                }
                            });
                        }
                    });
                }
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    let txt = if clear_arm { "Sure? Clear pending" } else { "Clear pending" };
                    if ui
                        .add_enabled(
                            !pending.is_empty(),
                            egui::Button::new(format!("{} {txt}", icons::TRASH)),
                        )
                        .clicked()
                    {
                        if clear_arm {
                            act = Some(QAct::Clear);
                            clear_arm = false;
                        } else {
                            clear_arm = true;
                        }
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        self.queue_clear_arm = clear_arm;
        if !open || close {
            self.queue_sheet_open = false;
            self.queue_clear_arm = false;
        }
        match act {
            Some(QAct::Interrupt) => {
                if let Some(e) = self.engine.as_ref() {
                    e.interrupt();
                }
                self.status = "Interrupted the running job".into();
                host.haptic(Haptic::Warning);
                self.last_queue_poll = 0.0;
            }
            Some(QAct::Delete(id)) => {
                if let Some(e) = self.engine.as_ref() {
                    e.queue_delete(vec![id]);
                }
                host.haptic(Haptic::Light);
                self.last_queue_poll = 0.0;
            }
            Some(QAct::Clear) => {
                if let Some(e) = self.engine.as_ref() {
                    e.queue_clear();
                }
                self.status = "Cleared the pending queue".into();
                host.haptic(Haptic::Warning);
                self.last_queue_poll = 0.0;
            }
            None => {}
        }
    }

    /// Move catalog trigger words for the active LoRA stack out of `positive` into `lora_triggers`.
    fn pull_lora_triggers_from_positive(&mut self) {
        let known: Vec<(usize, String)> = self
            .params
            .loras
            .iter()
            .enumerate()
            .flat_map(|(i, lora)| {
                self.lora_catalog
                    .entry(&lora.file)
                    .into_iter()
                    .flat_map(|e| e.trigger_words.iter())
                    .filter_map(move |t| {
                        let t = t.trim();
                        (!t.is_empty()).then(|| (i, t.to_string()))
                    })
            })
            .collect();
        if known.is_empty() {
            return;
        }
        let moved = extract_triggers_from_positive(
            &mut self.params.positive,
            &mut self.params.lora_triggers,
            &known,
        );
        for (idx, inj) in moved {
            if let Some(slot) = self.params.loras.get_mut(idx) {
                slot.injected = inj;
            }
        }
    }

    fn apply_preset(&mut self, name: &str) {
        if let Some(p) = self.presets.iter().find(|p| p.name == name) {
            self.params = p.params.clone();
            self.params.loras = dedupe_loras(std::mem::take(&mut self.params.loras));
            // Picked device-photo bytes are session-only; a preset can't carry them.
            if self.params.img2img_source == Img2ImgSource::Picked {
                self.params.img2img_source = Img2ImgSource::CurrentOutput;
            }
            // No Picked bytes means no masked photo, so the inpaint flag can't apply.
            self.params.inpaint_mask = false;
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

    /// Apply the character card at `idx`: identity tags, LoRAs, triggers, negatives, and (opt-in)
    /// checkpoint + face-detailer prompt. Reverses any already-active card first.
    fn apply_character(&mut self, idx: usize) {
        let Some(card) = self.characters.get(idx).cloned() else { return };
        self.remove_active_character();

        // Catalog trigger/negative words for each of the card's LoRAs.
        let words: HashMap<String, (String, String)> = card
            .loras
            .iter()
            .map(|l| {
                let pair = self
                    .lora_catalog
                    .entry(&l.file)
                    .map(|e| (e.trigger_text(), e.negative_text()))
                    .unwrap_or_default();
                (l.file.clone(), pair)
            })
            .collect();

        let mut applied =
            self.params.apply_character(&card, |f| words.get(f).cloned().unwrap_or_default());

        if card.switch_checkpoint && !card.checkpoint.trim().is_empty() {
            applied.switched_checkpoint = true;
            applied.prev_checkpoint = self.params.checkpoint.clone();
            applied.prev_unet = self.params.unet_name.clone();
            applied.prev_model_kind = Some(self.params.model_kind);
            self.select_model(&card.checkpoint, None);
        }

        if !card.face_prompt.trim().is_empty() {
            if let Some(step) =
                self.params.apps.iter_mut().find(|a| a.app == "face.detailer" && a.enabled)
            {
                applied.face_touched = true;
                applied.face_prev =
                    step.values.get("face_prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
                step.values
                    .insert("face_prompt".into(), serde_json::Value::String(card.face_prompt.clone()));
            }
        }

        self.active_character = Some(applied);
        self.selected_preset.clear();
    }

    /// Reverse the active character: strip its tokens, drop its LoRAs, restore any switched
    /// checkpoint and face-detailer prompt.
    fn remove_active_character(&mut self) {
        let Some(applied) = self.active_character.take() else { return };
        self.params.remove_character(&applied);
        if applied.switched_checkpoint {
            self.params.checkpoint = applied.prev_checkpoint.clone();
            self.params.unet_name = applied.prev_unet.clone();
            if let Some(k) = applied.prev_model_kind {
                self.params.model_kind = k;
            }
        }
        if applied.face_touched {
            if let Some(step) = self.params.apps.iter_mut().find(|a| a.app == "face.detailer") {
                step.values
                    .insert("face_prompt".into(), serde_json::Value::String(applied.face_prev.clone()));
            }
        }
        self.selected_preset.clear();
    }

    /// Insert or replace a card by name, keeping the list sorted. If the edited card was active,
    /// reverse the old application and reapply the saved version.
    fn save_character(&mut self, editing: Option<String>, card: CharacterCard) {
        let active = self.active_character.as_ref().map(|a| a.name.clone());
        let reapply = match (&editing, &active) {
            (Some(old), Some(act)) => old == act,
            (None, Some(act)) => act == &card.name,
            _ => false,
        };
        if let Some(old) = editing.as_ref().filter(|o| *o != &card.name) {
            self.characters.retain(|c| &c.name != old);
            // Carry the denied / suggestion history across the rename.
            if let Some(v) = self.character_denied.remove(old) {
                self.character_denied.insert(card.name.clone(), v);
            }
            if let Some(v) = self.character_suggestions.remove(old) {
                self.character_suggestions.insert(card.name.clone(), v);
            }
        }
        if let Some(slot) = self.characters.iter_mut().find(|c| c.name == card.name) {
            *slot = card.clone();
        } else {
            self.characters.push(card.clone());
            self.characters.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }
        // Seeds may have changed (LoRAs / portrait), so drop the cached centroids.
        self.character_centroids.clear();
        if reapply {
            self.remove_active_character();
            if let Some(i) = self.characters.iter().position(|c| c.name == card.name) {
                self.apply_character(i);
            }
        }
    }

    /// Seed a new card from a gallery image's scraped meta: identity from the prompt (quality tags
    /// dropped), LoRAs + strengths copied, checkpoint recorded. The user edits before saving.
    fn character_from_meta(meta: &ImageMeta) -> CharacterCard {
        let identity =
            meta.positive.as_deref().map(character_tags_from_prompt).unwrap_or_default();
        let loras = meta
            .loras
            .iter()
            .map(|l| ActiveLora {
                file: l.name.clone(),
                strength_model: l.strength_model as f32,
                strength_clip: l.strength_clip.unwrap_or(l.strength_model) as f32,
                injected: String::new(),
                model_only: l.model_only,
            })
            .collect();
        let checkpoint =
            meta.unet.clone().or_else(|| meta.models.first().cloned()).unwrap_or_default();
        CharacterCard { identity, loras, checkpoint, ..Default::default() }
    }

    /// A small square gallery thumbnail for `key` at `edge` px, fetched on demand and served from
    /// the same thumb cache the gallery tiles use.
    fn portrait_thumb(&mut self, ui: &mut egui::Ui, key: &str, edge: f32) {
        let size = 96u32;
        let thumb_key = format!("{key}#{size}");
        let (rect, _) = ui.allocate_exact_size(egui::vec2(edge, edge), egui::Sense::hover());
        match self.thumbs.get(&thumb_key) {
            Some(tex) => {
                let sized = egui::load::SizedTexture::from_handle(tex);
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.add(
                            egui::Image::new(sized).max_size(rect.size()).maintain_aspect_ratio(true),
                        );
                    });
                });
            }
            None => {
                if self.thumbs.claim(&thumb_key)
                    && let Some((sub, file)) = key.rsplit_once('/')
                {
                    self.engine.as_ref().unwrap().fetch_thumb(
                        sub.to_string(),
                        file.to_string(),
                        size,
                        self.full_cache_root.clone(),
                    );
                }
                ui.painter().rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
            }
        }
    }

    /// Confirmed-set keys for a character's CLIP centroid, strongest signal first: members of the
    /// card's album while that album is the loaded view, else LoRA-name matches (the Character
    /// grouping rule), always folding in the portrait. Only keys with an embedding actually count.
    fn character_seed_keys(&self, card: &CharacterCard) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        // Album membership lives server-side, so it's only known while the gallery is filtered to it.
        if card.album_id != 0 && self.gallery_view.album == Some(card.album_id) {
            for it in self.gallery.iter().filter(|it| !it.is_video) {
                if seen.insert(it.key()) {
                    keys.push(it.key());
                }
            }
        }
        if keys.is_empty() {
            for it in self.gallery.iter().filter(|it| !it.is_video) {
                if crate::gallery::item_matches_character(it, card) && seen.insert(it.key()) {
                    keys.push(it.key());
                }
            }
        }
        if !card.portrait_key.is_empty() && seen.insert(card.portrait_key.clone()) {
            keys.push(card.portrait_key.clone());
        }
        keys
    }

    /// Rank gallery images by CLIP similarity to the character and open the review deck over them.
    fn find_character_images(&mut self, card: CharacterCard, host: &Host) {
        self.character_centroids.remove(&card.name);
        let seeds = self.character_seed_keys(&card);
        let Some(centroid) = clip_index::character_centroid(&seeds, &self.clip_index) else {
            self.status = "No indexed images for this character yet — index the gallery first".into();
            host.haptic(Haptic::Warning);
            return;
        };
        let mut exclude: HashSet<String> = seeds.into_iter().collect();
        if let Some(d) = self.character_denied.get(&card.name) {
            exclude.extend(d.iter().cloned());
        }
        let ranked =
            clip_index::rank_candidates(&centroid, &self.clip_index, &exclude, CHARACTER_MATCH_COS);
        if ranked.is_empty() {
            self.status = "No new matches found".into();
            host.haptic(Haptic::Warning);
            return;
        }
        let keys: Vec<String> = ranked.into_iter().map(|(k, _)| k).collect();
        self.open_character_review(card.name, keys, host);
    }

    /// Enter the shared swipe deck in character-review mode over `keys` (already ranked best-first),
    /// keeping only those still present as still images in the loaded gallery.
    fn open_character_review(&mut self, card_name: String, keys: Vec<String>, host: &Host) {
        let present: HashSet<String> =
            self.gallery.iter().filter(|it| !it.is_video).map(|it| it.key()).collect();
        let deck: Vec<String> = keys.into_iter().filter(|k| present.contains(k)).collect();
        if deck.is_empty() {
            self.status = "Matched images aren't in the loaded gallery — load more first".into();
            host.haptic(Haptic::Warning);
            return;
        }
        self.tab = Tab::Gallery;
        self.viewer = None;
        self.triage_swipe_origin = None;
        self.triage = Some(Triage {
            deck,
            pos: 0,
            kept: 0,
            trashed: 0,
            keep: Vec::new(),
            trash: Vec::new(),
            album: None,
            last: None,
            mode: TriageMode::Character { card: card_name },
        });
        host.haptic(Haptic::Light);
    }

    /// Add accepted images to the character's collection album, creating it (named after the card)
    /// on first use and stamping its id back onto the card.
    fn add_to_character_album(&mut self, card_name: &str, items: Vec<(String, String)>) {
        let album_id =
            self.characters.iter().find(|c| c.name == card_name).map(|c| c.album_id).unwrap_or(0);
        if album_id != 0 {
            self.engine.as_ref().unwrap().album_add(album_id, items);
        } else {
            let album_name = card_name.to_string();
            self.engine.as_ref().unwrap().album_create(album_name.clone());
            self.char_album_pending = Some((card_name.to_string(), album_name, items));
        }
        // The assembled album is the training set a LoRA-trainer workflow would consume; queueing
        // that is out of scope here (the server's trainer node inventory is unknown).
    }

    /// A character's cached CLIP centroid, computed from its seeds on a cache miss.
    #[cfg(feature = "local-npu")]
    fn character_centroid_cached(&mut self, card: &CharacterCard) -> Option<Vec<f32>> {
        if !self.character_centroids.contains_key(&card.name) {
            let keys = self.character_seed_keys(card);
            let cen = clip_index::character_centroid(&keys, &self.clip_index).unwrap_or_default();
            self.character_centroids.insert(card.name.clone(), cen);
        }
        self.character_centroids.get(&card.name).filter(|c| !c.is_empty()).cloned()
    }

    /// Score a freshly indexed image against every character; record high-confidence hits as pending
    /// suggestions (never a silent move — the user reviews them). Denied, seed, and already-pending
    /// keys are skipped.
    #[cfg(feature = "local-npu")]
    fn suggest_for_new_key(&mut self, key: &str) {
        if self.characters.is_empty() {
            return;
        }
        let matched_item = self.gallery.iter().find(|it| it.key() == key).cloned();
        for card in self.characters.clone() {
            if card.portrait_key == key {
                continue;
            }
            if self.character_denied.get(&card.name).is_some_and(|d| d.iter().any(|k| k == key)) {
                continue;
            }
            if self.character_suggestions.get(&card.name).is_some_and(|s| s.iter().any(|k| k == key)) {
                continue;
            }
            // A LoRA-name match is already a confirmed seed — no need to suggest it.
            if matched_item.as_ref().is_some_and(|it| crate::gallery::item_matches_character(it, &card))
            {
                continue;
            }
            let Some(cen) = self.character_centroid_cached(&card) else { continue };
            let Some(cos) = self.clip_index.cosine_to(key, &cen) else { continue };
            if cos >= CHARACTER_SUGGEST_COS {
                let sug = self.character_suggestions.entry(card.name.clone()).or_default();
                sug.push(key.to_string());
                let overflow = sug.len().saturating_sub(CHARACTER_SUGGEST_CAP);
                if overflow > 0 {
                    sug.drain(..overflow);
                }
            }
        }
    }

    fn output(&mut self, ui: &mut egui::Ui, host: &Host) {
        // Sampling progress is on the bottom nav; keep idle notes (errors / Done) here only.
        if !self.running && self.queue_remaining == 0 && !self.status.is_empty() {
            ui.add_space(6.0);
            ui.label(elide(&self.status, 300));
        }

        // After a multi-job burst, offer a grade-pass over the fresh results.
        if !self.running && self.untriaged.len() >= 2 {
            ui.add_space(4.0);
            if ui
                .button(format!("{} Triage {} new results", icons::STAR, self.untriaged.len()))
                .on_hover_text("Swipe through this batch: keep, trash, or reuse as input")
                .clicked()
            {
                self.open_triage(host);
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
        let mut share = false;
        let mut inpaint = false;
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
                if ui.button("Share").on_hover_text("Share via other apps").clicked() {
                    share = true;
                }
                if ui
                    .button(format!("{} Fix area", icons::MODEL))
                    .on_hover_text("Paint a mask to inpaint")
                    .clicked()
                {
                    inpaint = true;
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
        } else if share {
            let bytes = self.results[idx].1.clone();
            let name = format!("output-{}.png", idx + 1);
            self.note = self.share_bytes(host, &bytes, &name);
        } else if inpaint {
            let bytes = self.results[idx].1.clone();
            let name = format!("output-{}.png", idx + 1);
            self.result_view = None;
            self.open_inpaint(ui.ctx(), bytes, name);
        } else if let Some(d) = go {
            let next = idx as isize + d;
            if next >= 0 && (next as usize) < n {
                self.result_view = Some(next as usize);
            }
        }
    }

    /// When the soft keyboard opens, scroll the focused field into the shrunk viewport.
    fn scroll_focus_into_view(&self, ui: &egui::Ui) {
        if !self.kb_open_edge {
            return;
        }
        if let Some(id) = ui.ctx().memory(|m| m.focused())
            && let Some(resp) = ui.ctx().read_response(id)
        {
            resp.scroll_to_me(None);
        }
    }

    fn generate_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        if self.result_view.is_some() {
            let pane = ui.available_rect_before_wrap();
            self.result_viewer(ui, host);
            // Keep Queue reachable while inspecting a batch frame.
            self.queue_fab(ui.ctx(), host, pane, QueueFabKind::Create);
            return;
        }

        // Above the app nav bar (Create / Graph / Gallery / Settings).
        egui::Panel::bottom("create-panes").show(ui, |ui| {
            ui.add_space(2.0);
            self.create_pane_bar(ui);
            ui.add_space(2.0);
        });

        let pane = ui.available_rect_before_wrap();
        self.scroll_focus_into_view(ui);
        crate::theme::scroll_vertical()
            .id_salt("create-main-scroll")
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
                // Results first so a new image is immediately visible without scrolling.
                let results_top = ui.cursor().min;
                self.output(ui, host);
                if self.create_scroll_bottom {
                    ui.scroll_to_rect(
                        egui::Rect::from_min_size(results_top, egui::vec2(1.0, 1.0)),
                        Some(egui::Align::TOP),
                    );
                    self.create_scroll_bottom = false;
                }

                ui.add_enabled_ui(connected, |ui| self.controls(ui, host));
                ui.add_space(12.0);
            });
        self.queue_fab(ui.ctx(), host, pane, QueueFabKind::Create);
        self.create_fab(ui.ctx(), host, pane);
    }

    /// Cancel the active generation: abort local awaiters, `POST /interrupt` the running server
    /// prompt, and `POST /queue` delete our still-pending server jobs.
    fn cancel_generation(&mut self, host: &Host) {
        let ours: HashSet<&str> = self.my_prompts.iter().map(|p| p.id.as_str()).collect();
        let pending_ours: Vec<String> = self
            .queue_jobs
            .1
            .iter()
            .filter(|j| ours.contains(j.prompt_id.as_str()))
            .map(|j| j.prompt_id.clone())
            .collect();
        if let Some(engine) = self.engine.as_mut() {
            engine.cancel();
            engine.interrupt();
            engine.queue_delete(pending_ours);
            host.haptic(Haptic::Warning);
        }
    }

    /// Queue FAB (+ Cancel while running) shared by Create and Graph (bottom-right stack).
    fn queue_fab(&mut self, ctx: &egui::Context, host: &Host, pane: egui::Rect, kind: QueueFabKind) {
        if !pane.is_finite() || pane.width() < 80.0 {
            return;
        }
        // One slot above the menu/lock FAB so the stack reads queue-on-top.
        let edge = crate::theme::FAB_EDGE;
        let step = crate::theme::FAB_STEP;
        let default = egui::pos2(pane.right() - edge, pane.bottom() - edge - step);
        let mut pos = self.queue_fab_pos.unwrap_or(default);
        pos.x = pos.x.clamp(pane.left() + 8.0, pane.right() - crate::theme::FAB_SIZE - 4.0);
        pos.y = pos.y.clamp(pane.top() + 8.0, pane.bottom() - crate::theme::FAB_SIZE - 4.0);

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
        let jobs = self.jobs_left;
        let server_q = self.queue_remaining;
        egui::Area::new(egui::Id::new("queue-fab"))
            .order(egui::Order::Foreground)
            .current_pos(pos)
            .show(ctx, |ui| {
                let tip = if jobs > 0 {
                    if server_q > jobs as u32 {
                        format!("Queue another ({jobs} yours, {server_q} on server)")
                    } else {
                        format!("Queue another ({jobs} in flight)")
                    }
                } else if server_q > 0 {
                    format!("Queue (server has {server_q})")
                } else {
                    "Queue".into()
                };
                let label = if jobs > 0 {
                    format!("{}{jobs}", icons::RUN)
                } else {
                    icons::RUN.to_owned()
                };
                let fill = if jobs > 0 {
                    crate::theme::fab_bg_ok()
                } else {
                    crate::theme::fab_bg()
                };
                let resp = ui
                    .add_enabled_ui(can_queue, |ui| crate::theme::fab(ui, &label, fill))
                    .inner
                    .on_hover_text(tip);
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
            let cancel_pos = egui::pos2(pos.x, (pos.y - step).max(pane.top() + 8.0));
            egui::Area::new(egui::Id::new("cancel-fab"))
                .order(egui::Order::Foreground)
                .fixed_pos(cancel_pos)
                .show(ctx, |ui| {
                    let stop = if jobs > 1 {
                        format!("{}{jobs}", icons::STOP)
                    } else {
                        icons::STOP.to_owned()
                    };
                    if crate::theme::fab(ui, &stop, crate::theme::fab_bg_danger())
                        .on_hover_text(if jobs > 1 {
                            format!("Cancel all ({jobs} in flight)")
                        } else {
                            "Cancel".into()
                        })
                        .clicked()
                    {
                        cancel_clicked = true;
                    }
                });
        }

        if cancel_clicked {
            self.cancel_generation(host);
            return;
        }
        if queue_clicked {
            match kind {
                QueueFabKind::Create => self.queue_create_variants(ctx, host),
                QueueFabKind::Graph => self.queue_graph(ctx, host),
            }
        }
    }

    /// Draggable Create-tab menu bubble (paste / open graph), under the queue FAB.
    fn create_fab(&mut self, ctx: &egui::Context, host: &Host, pane: egui::Rect) {
        if !pane.is_finite() || pane.width() < 80.0 {
            return;
        }
        let edge = crate::theme::FAB_EDGE;
        let step = crate::theme::FAB_STEP;
        let queue = self
            .queue_fab_pos
            .unwrap_or(egui::pos2(pane.right() - edge, pane.bottom() - edge - step));
        let default = egui::pos2(queue.x, queue.y + step);
        let mut pos = self.create_fab_pos.unwrap_or(default);
        pos.x = pos.x.clamp(pane.left() + 8.0, pane.right() - crate::theme::FAB_SIZE - 4.0);
        pos.y = pos.y.clamp(pane.top() + 8.0, pane.bottom() - crate::theme::FAB_SIZE - 4.0);

        let can_open_graph =
            self.params.missing_model_part().is_none() && self.schemas.is_some();
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
        // Full clipboard reads allocate the entire clip on the Java heap — never do that every
        // frame. Snapshot + classify once when the menu opens; drop when it closes.
        if open {
            if self.create_fab_clip.is_none() {
                // Classify then drop the string so a huge workflow clip isn't retained.
                self.create_fab_clip =
                    host.clipboard_text().as_deref().map(FabClipSnap::from_text);
            }
        } else {
            self.create_fab_clip = None;
        }
        let snap = self.create_fab_clip.as_ref();
        let has_wf = self.workflow_clip.is_some() || snap.is_some_and(|s| s.has_wf);
        let has_sampler = self.sampler_clip.is_some() || snap.is_some_and(|s| s.has_sampler);
        let has_loras = self.lora_clip.is_some() || snap.is_some_and(|s| s.has_loras);
        let has_apps = snap.is_some_and(|s| s.has_apps);

        let mut menu_rect = egui::Rect::NOTHING;
        if open {
            let menu = egui::Area::new(egui::Id::new("create-fab-menu"))
                .order(egui::Order::Foreground)
                .pivot(egui::Align2::RIGHT_BOTTOM)
                .fixed_pos(egui::pos2(pos.x + crate::theme::FAB_SIZE, pos.y - 8.0))
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
                    crate::theme::fab_bg_on()
                } else {
                    crate::theme::fab_bg()
                };
                let label = if open { icons::CHECK } else { icons::MENU };
                let resp = crate::theme::fab(ui, label, fill).on_hover_text("Actions — drag to move");
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

    fn enter_graph_fullscreen(&mut self, host: &Host) {
        self.graph_fullscreen = true;
        self.graph_pane = GraphPane::Canvas;
        self.graph_fs_portrait_since = None;
        host.set_screen_orientation(ScreenOrientation::Landscape);
    }

    fn exit_graph_fullscreen(&mut self, host: &Host) {
        self.graph_fullscreen = false;
        self.graph_fs_portrait_since = None;
        host.set_screen_orientation(ScreenOrientation::Unspecified);
    }

    fn graph_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        // Auto-exit fullscreen when the device is tilted back to portrait for >0.4s.
        if self.graph_fullscreen {
            let now = ui.input(|i| i.time);
            let near_portrait = device_orientation_deg().is_some_and(|d| {
                let d = d.rem_euclid(360.0);
                d < 25.0 || d > 335.0 || (d > 155.0 && d < 205.0)
            });
            if near_portrait {
                let since = *self.graph_fs_portrait_since.get_or_insert(now);
                if now - since > 0.4 {
                    self.exit_graph_fullscreen(host);
                }
            } else {
                self.graph_fs_portrait_since = None;
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }

        // Back / Esc exits fullscreen (doesn't navigate away).
        if self.graph_fullscreen
            && ui.ctx().input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
            })
        {
            self.exit_graph_fullscreen(host);
            return;
        }

        let has_graph = self.has_graph_editor();

        // Top: open workflow tabs only (hidden in fullscreen).
        if has_graph && !self.graph_fullscreen {
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

        // Bottom: File/Edit/View | Canvas/Properties | fullscreen toggle.
        let fs = self.graph_fullscreen;
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
                // Fullscreen toggle (rightmost).
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (icon, tip) = if fs {
                        (icons::FULLSCREEN_EXIT, "Exit fullscreen")
                    } else {
                        (icons::FULLSCREEN, "Fullscreen — landscape editor")
                    };
                    if ui
                        .add(egui::Button::new(icon).min_size(egui::vec2(32.0, 0.0)))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        if fs {
                            self.exit_graph_fullscreen(host);
                        } else {
                            self.enter_graph_fullscreen(host);
                        }
                    }
                });
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
                doc.view.show(
                    ui,
                    &mut doc.graph,
                    executing,
                    props,
                    &doc.bypassed,
                    &loras,
                    &mut doc.seed_randomize,
                )
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
        // Prefer the in-app clip; only touch the system clipboard when pasting.
        let has_clip = self.workflow_clip.is_some()
            || host.clipboard_has_text();
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
            let body = self.workflow_clip.clone().or_else(|| {
                host.clipboard_text().filter(|t| {
                    let t = t.trim();
                    t.starts_with('{') || t.starts_with('[')
                })
            });
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
        let mut duplicate = false;
        let resp = egui::Area::new(egui::Id::new("graph-node-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    if ui
                        .button(format!("{} Duplicate", icons::ADD))
                        .on_hover_text("Copy this node (values + input wires)")
                        .clicked()
                    {
                        duplicate = true;
                    }
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
        if duplicate {
            self.duplicate_node(nid, host);
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

    /// Clone a node with its widget values and incoming wires, offset from the original.
    fn duplicate_node(&mut self, nid: NodeId, host: &Host) {
        if self.active_doc().is_some_and(|d| d.view.locked) {
            self.graph_status = "Graph is locked — unlock to duplicate".into();
            host.haptic(Haptic::Warning);
            return;
        }
        let Some(doc) = self.active_doc_mut() else { return };
        let Some(info) = doc.graph.snarl.get_node_info(nid) else {
            self.graph_status = "Node gone".into();
            host.haptic(Haptic::Warning);
            return;
        };
        let data = info.value.clone();
        let open = info.open;
        let pos = info.pos + egui::vec2(48.0, 48.0);
        let class = data.object.name.clone();
        let n_inputs = data.inputs.len();
        let was_bypassed = doc.bypassed.contains(&nid);

        let mut incoming: Vec<(OutPinId, usize)> = Vec::new();
        for input in 0..n_inputs {
            let pin = doc.graph.snarl.in_pin(InPinId { node: nid, input });
            for remote in pin.remotes {
                incoming.push((remote, input));
            }
        }

        let new_id = doc.graph.snarl.insert_node(pos, data);
        if let Some(info) = doc.graph.snarl.get_node_info_mut(new_id) {
            info.open = open;
        }
        for (from, input) in incoming {
            doc.graph.snarl.connect(from, InPinId { node: new_id, input });
        }
        if was_bypassed {
            doc.bypassed.insert(new_id);
        }
        let seed_flags: Vec<(String, bool)> = doc
            .seed_randomize
            .iter()
            .filter(|((n, _), _)| *n == nid)
            .map(|((_, name), &v)| (name.clone(), v))
            .collect();
        for (name, v) in seed_flags {
            doc.seed_randomize.insert((new_id, name), v);
        }
        doc.props_node = Some(new_id);
        self.graph_status = format!("Duplicated {class}");
        host.haptic(Haptic::Success);
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
        centered(ctx, egui::Window::new("Save workflow"))
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
                Some(doc.view.export_ui(&doc.graph, schemas, &doc.bypassed, &doc.seed_randomize))
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
        centered(ctx, egui::Window::new("Find node"))
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
                        &mut doc.seed_randomize,
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

    /// Shared device-photo grid (permission gate + MediaStore thumbnails, 2 loads/frame). Returns
    /// the tapped `(MediaStore id, display name)`; callers decide what to do with the pick.
    fn device_photo_grid(&mut self, ui: &mut egui::Ui, host: &Host) -> Option<(i64, String)> {
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
            return None;
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
            return None;
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
        pick
    }

    /// Graph LoadImage device picker: a pick eagerly uploads to the server for `node`.
    fn loadimage_device_grid(&mut self, ui: &mut egui::Ui, host: &Host, node: NodeId) {
        if let Some((id, name)) = self.device_photo_grid(ui, host) {
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
        centered(ctx, egui::Window::new("Server workflows"))
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
        centered(ctx, egui::Window::new("Add node"))
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
                        for input in &data.inputs {
                            if graphview::is_seed_widget(input) {
                                doc.seed_randomize.insert((nid, input.name.clone()), true);
                            }
                        }
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
        // Seeds are drawn from the gallery listing, so a new query invalidates cached centroids.
        self.character_centroids.clear();
        // Forget in-flight thumb requests so earlier failures get retried.
        self.thumbs.reset_pending();
        // Supersede any in-flight pages of the previous query (auto-load chains overlap).
        self.gallery_gen = self.gallery_gen.wrapping_add(1);
        self.engine.as_ref().unwrap().gallery_list(
            self.gallery_gen,
            0,
            self.gallery_page_size(),
            self.gallery_list_q(),
            &self.gallery_view,
        );
    }

    /// The gallery's bottom control bar: search, model filter, sort, grouping and column count.
    /// Returns whether the listing must be re-queried — every control except the column count is
    /// applied server-side across the whole listing, not to the page already fetched.
    /// Should the whole filtered/grouped set be auto-loaded (rather than paged by hand)?
    fn gallery_wants_all(&self) -> bool {
        if self.gallery_view.group != GalleryGroup::None
            || !self.gallery_view.model.is_empty()
            || self.gallery_view.album.is_some()
            // The media filter is client-side, so the full set must be present to filter over.
            || self.gallery_view.media != GalleryMedia::All
        {
            return true;
        }
        // CLIP embed / full-cache prefetch need every listed key, not just the first page.
        #[cfg(feature = "local-npu")]
        if self.clip_pack.is_some() {
            return true;
        }
        self.cache_prefetch
    }

    fn gallery_controls(&mut self, ui: &mut egui::Ui, connected: bool, host: &Host) -> bool {
        let mut changed = false;
        #[cfg(not(feature = "local-npu"))]
        let _ = host;
        // One row: search + refresh + View (rightmost). Filters live in View submenus.
        ui.horizontal(|ui| {
            let refresh_w = 40.0;
            let view_w = 72.0;
            let tags_w = 60.0;
            let search_w = (ui.available_width() - refresh_w - view_w - tags_w - 12.0).max(96.0);
            #[cfg(feature = "local-npu")]
            let semantic = self.gallery_semantic_active();
            #[cfg(not(feature = "local-npu"))]
            let semantic = false;
            let hint = if semantic {
                format!("{} describe an image", icons::SEARCH)
            } else {
                format!("{} search", icons::SEARCH)
            };
            let resp = ui.add_sized(
                egui::vec2(search_w, 28.0),
                egui::TextEdit::singleline(&mut self.gallery_q).hint_text(hint),
            );
            #[cfg(feature = "local-npu")]
            if semantic && self.gallery_q.trim().is_empty() {
                self.clear_semantic_ranked();
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if semantic {
                    #[cfg(feature = "local-npu")]
                    {
                        let q = self.gallery_q.trim().to_string();
                        if q.is_empty() {
                            self.clear_semantic_ranked();
                        } else {
                            self.start_clip_search(ui.ctx(), host, q);
                        }
                    }
                } else {
                    changed = true;
                }
            }
            if ui
                .add_enabled(connected, egui::Button::new(icons::REFRESH).min_size(egui::vec2(36.0, 28.0)))
                .on_hover_text("Refresh")
                .clicked()
            {
                changed = true;
            }

            if ui
                .add_sized(egui::vec2(56.0, 28.0), egui::Button::selectable(self.tags_window_open, "Tags"))
                .clicked()
            {
                self.tags_window_open = !self.tags_window_open;
            }

            up_menu_sized(ui, format!("{} View", icons::GALLERY), egui::vec2(68.0, 28.0), |ui| {
                if ui
                    .button(format!("{} Select", icons::CHECK))
                    .on_hover_text("Multi-select — or long-press a photo")
                    .clicked()
                {
                    self.select_mode = true;
                }
                ui.separator();

                #[cfg(feature = "local-npu")]
                {
                    let was = self.gallery_semantic;
                    ui.checkbox(&mut self.gallery_semantic, "Semantic search").on_hover_text(
                        "Search box describes an image; ranks the CLIP index instead of the server text query",
                    );
                    if was && !self.gallery_semantic {
                        self.clear_semantic_ranked();
                    }
                }

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

                ui.menu_button(format!("Rating · {}", self.gallery_view.rating.label()), |ui| {
                    for r in RatingFilter::ALL {
                        ui.selectable_value(&mut self.gallery_view.rating, *r, r.label());
                    }
                    ui.separator();
                    ui.weak("Unindexed images count as Safe.");
                    ui.separator();
                    ui.selectable_value(&mut self.index_filter, 0, "Indexed + not");
                    ui.selectable_value(&mut self.index_filter, 1, "Indexed only");
                    ui.selectable_value(&mut self.index_filter, 2, "Unindexed only");
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
        centered(ctx, egui::Window::new("Manage albums"))
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

    /// Whether the main gallery box should run semantic search: toggle on, pack has the text tower,
    /// and the CLIP index holds at least one embedding.
    #[cfg(feature = "local-npu")]
    fn gallery_semantic_active(&self) -> bool {
        self.gallery_semantic
            && self.clip_index.len() > 0
            && self.clip_pack.as_ref().is_some_and(|d| {
                d.join(local_clip::TEXT_MODEL_FILE).is_file()
                    && d.join(local_clip::TOKENIZER_FILE).is_file()
            })
    }

    /// Query string for server gallery list calls. Empty while semantic search owns the box so the
    /// server does not filter away images the CLIP ranker needs.
    fn gallery_list_q(&self) -> &str {
        #[cfg(feature = "local-npu")]
        if self.gallery_semantic_active() {
            return "";
        }
        self.gallery_q.as_str()
    }

    /// Durable or app-files `gallery_full` directory for full-resolution image cache.
    fn resolve_full_cache_root(host: &Host) -> Option<String> {
        gallery::resolve_full_cache_root(host.documents_dir().as_deref())
    }

    /// Memoized cache root; refreshes when missing.
    fn ensure_full_cache_root(&mut self, host: &Host) -> Option<&str> {
        if self.full_cache_root.is_none() {
            self.full_cache_root = Self::resolve_full_cache_root(host);
        }
        self.full_cache_root.as_deref()
    }

    fn format_bytes(n: u64) -> String {
        const KB: f64 = 1024.0;
        const MB: f64 = KB * 1024.0;
        const GB: f64 = MB * 1024.0;
        let x = n as f64;
        if x >= GB {
            format!("{:.1} GB", x / GB)
        } else if x >= MB {
            format!("{:.0} MB", x / MB)
        } else if x >= KB {
            format!("{:.0} KB", x / KB)
        } else {
            format!("{n} B")
        }
    }

    /// Last finished cache scan, kicking a worker refresh at most every few seconds. The scan
    /// stats every listed key plus the whole cache dir — minutes-of-jank territory if run per frame.
    fn full_cache_progress(&mut self, ctx: &egui::Context, host: &Host) -> Option<FullCacheReport> {
        let root = self.ensure_full_cache_root(host)?.to_string();
        if let Some(rx) = &self.full_cache_report_rx {
            match rx.try_recv() {
                Ok(report) => {
                    self.full_cache_report = Some(report);
                    self.full_cache_report_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.full_cache_report_rx = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let now = ctx.input(|i| i.time);
        let stale = self.full_cache_report.is_none() || now - self.full_cache_report_at > 2.5;
        if stale && self.full_cache_report_rx.is_none() {
            self.full_cache_report_at = now;
            let keys: Vec<String> = self
                .gallery
                .iter()
                .filter(|it| !it.is_video)
                .map(|it| it.key())
                .collect();
            let (tx, rx) = std::sync::mpsc::channel();
            self.full_cache_report_rx = Some(rx);
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let report = FullCacheReport {
                    cached: gallery::full_cache_hits(&root, &keys),
                    listed: keys.len(),
                    stats: gallery::full_cache_stats(&root),
                    keyed: gallery::full_cache_keys(&root).len(),
                    root,
                };
                let _ = tx.send(report);
                ctx.request_repaint();
            });
        }
        self.full_cache_report.clone()
    }

    fn gallery_cache_settings(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.label("Full-image cache");
        let report = self.full_cache_progress(ui.ctx(), host);
        // Keep the numbers ticking over while the pane sits open without input.
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(3));
        if self.full_cache_root.is_none() {
            ui.weak("Cache unavailable (need storage / All files access for /sdcard/ComfyUI).");
        } else if let Some(r) = report {
            let missing = r.listed.saturating_sub(r.cached);
            ui.label(format!(
                "{} cached · {missing} not cached · {} on disk ({})",
                r.cached,
                Self::format_bytes(r.stats.bytes),
                elide(&r.root, 42)
            ));
            #[cfg(feature = "local-npu")]
            if self.clip_pack.is_some() {
                let (embedded, target, stuck) = self.clip_index_progress(&r);
                ui.weak(format!("CLIP index: {embedded} / {target} embedded"));
                if stuck {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 160, 120),
                        "CLIP embed looks stuck — use Resume below.",
                    );
                }
            }
        } else {
            ui.weak("Measuring cache…");
        }
        ui.checkbox(&mut self.cache_prefetch, "Prefetch full images")
            .on_hover_text("Download missing full-resolution gallery images while idle");
        ui.horizontal(|ui| {
            if ui.button("Cache missing now").clicked() {
                self.cache_prefetch = true;
                self.prefetch_failed.clear();
                self.prefetch_cached.clear();
                self.note = "Caching missing full images…".into();
            }
            let pause_label = if self.cache_prefetch { "Pause" } else { "Resume" };
            if ui.button(pause_label).clicked() {
                self.cache_prefetch = !self.cache_prefetch;
            }
            if !self.cache_clear_confirm {
                if ui.button("Clear cache").clicked() {
                    self.cache_clear_confirm = true;
                }
            } else {
                if ui.button("Confirm clear").clicked() {
                    if let Some(root) = self.ensure_full_cache_root(host).map(|s| s.to_string()) {
                        match gallery::clear_full_cache(&root) {
                            Ok(n) => self.note = format!("Cleared {n} cached images"),
                            Err(e) => self.note = format!("Clear failed: {e}"),
                        }
                    }
                    self.cache_clear_confirm = false;
                    self.prefetch_failed.clear();
                    self.prefetch_cached.clear();
                    self.full_cache_report = None;
                }
                if ui.button("Cancel").clicked() {
                    self.cache_clear_confirm = false;
                }
            }
        });
        #[cfg(feature = "local-npu")]
        if self.clip_pack.is_some() {
            ui.horizontal(|ui| {
                if ui
                    .button("Resume CLIP index")
                    .on_hover_text("Clear stuck in-flight embeds and failed keys, then keep indexing")
                    .clicked()
                {
                    self.reset_clipemb_pump();
                    if let Some(root) = self.ensure_full_cache_root(host).map(|s| s.to_string()) {
                        for it in &self.gallery {
                            if it.is_video {
                                continue;
                            }
                            gallery::ensure_full_cache_key(&root, &it.key());
                        }
                    }
                    self.note = "CLIP indexing resumed".into();
                }
                if ui
                    .button("Rebuild CLIP index")
                    .on_hover_text("Delete the saved CLIP index and re-embed from the full-image cache")
                    .clicked()
                {
                    self.rebuild_clip_index(host);
                    self.note = "CLIP index cleared — re-embedding from cache…".into();
                }
            });
        }
        ui.weak("Full images (not thumbs). Powers offline viewer loads and CLIP semantic search.");
    }

    /// Embedded count vs the best available library size (cache / listing / server total).
    #[cfg(feature = "local-npu")]
    fn clip_index_progress(&self, report: &FullCacheReport) -> (usize, usize, bool) {
        let embedded = self.clip_index.len();
        let listed = self.gallery.iter().filter(|it| !it.is_video).count();
        let total = self.gallery_total as usize;
        let target = embedded.max(listed).max(report.stats.files).max(report.keyed).max(total);
        let has_work = embedded < target;
        let stuck = has_work && (self.clipemb_pending.is_some() || !self.clipemb_failed.is_empty());
        (embedded, target, stuck)
    }

    /// Idle download of the next listed image missing from the full-image cache.
    fn pump_full_cache(&mut self, ctx: &egui::Context, host: &Host) {
        if !self.cache_prefetch || self.prefetch_pending.is_some() {
            return;
        }
        if !matches!(self.conn, Conn::Connected) || self.gallery.is_empty() || self.engine.is_none() {
            return;
        }
        let Some(root) = self.ensure_full_cache_root(host).map(|s| s.to_string()) else { return };
        let mut newly_cached: Vec<String> = Vec::new();
        let next = self.gallery.iter().find_map(|it| {
            if it.is_video {
                return None;
            }
            let key = it.key();
            if self.prefetch_cached.contains(&key) || self.prefetch_failed.contains(&key) {
                return None;
            }
            // Stat each key once, then trust the session memo — FUSE stats per frame add up fast.
            if gallery::full_cache_has(&root, &key) {
                newly_cached.push(key);
                return None;
            }
            // Avoid racing a viewer / pick / tag / embed fetch for the same key.
            #[cfg(feature = "local-npu")]
            if self.autotag_pending.as_deref() == Some(key.as_str())
                || self.clipemb_pending.as_deref() == Some(key.as_str())
            {
                return None;
            }
            if self.gallery_pick_pending.as_ref().is_some_and(|(k, _)| *k == key) {
                return None;
            }
            if self.viewer.as_ref().is_some_and(|v| v.item.key() == key && v.loading) {
                return None;
            }
            Some((key, it.subfolder.clone(), it.filename.clone()))
        });
        self.prefetch_cached.extend(newly_cached);
        if let Some((key, subfolder, filename)) = next {
            self.prefetch_pending = Some(key);
            self.engine.as_ref().unwrap().fetch_full(subfolder, filename, Some(root));
            ctx.request_repaint();
        }
    }

    /// Drop a semantic ranked view and any in-flight text embed when the search box is emptied.
    #[cfg(feature = "local-npu")]
    fn clear_semantic_ranked(&mut self) {
        if self.ranked.as_ref().is_some_and(|r| r.is_semantic()) {
            self.ranked = None;
        }
        self.clip_search_rx = None;
        self.clip_search_running = false;
        self.clip_text_q.clear();
    }

    /// Pinned all-tags browser: a filter box over every indexed tag with counts, tap to toggle a
    /// facet. A real window keeps the TextEdit focused where a menu popup would auto-close on IME.
    fn tags_window(&mut self, ctx: &egui::Context) {
        if !self.tags_window_open {
            return;
        }
        let mut open = true;
        centered(ctx, egui::Window::new(format!("{} Tags", icons::SEARCH)))
            .collapsible(false)
            .open(&mut open)
            .default_width(360.0)
            .default_height(420.0)
            .show(ctx, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.tag_browse_q)
                        .hint_text("filter tags")
                        .desired_width(f32::INFINITY),
                );
                let keys: Vec<String> = self.gallery.iter().map(|it| it.key()).collect();
                let all = self.tag_index.top_tags(&keys, 400);
                let q = self.tag_browse_q.trim().to_lowercase();
                crate::theme::scroll_vertical().max_height(340.0).auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        for (tag, n) in all.iter().filter(|(t, _)| q.is_empty() || t.contains(&q)) {
                            let on = self.tag_facets.contains(tag);
                            if ui.add(egui::Button::selectable(on, format!("{tag}  ({n})"))).clicked() {
                                if on {
                                    self.tag_facets.retain(|f| f != tag);
                                } else {
                                    self.tag_facets.push(tag.clone());
                                }
                            }
                        }
                    },
                );
            });
        self.tags_window_open = open;
    }

    /// Spawn the CLIP text-embedding worker; the L2-normalized query embedding returns via the
    /// channel and `poll_clip_search` ranks the index into the similarity view.
    #[cfg(feature = "local-npu")]
    fn start_clip_search(&mut self, ctx: &egui::Context, host: &Host, query: String) {
        let (Some(lib_dir), Some(pack_dir)) = (host.native_lib_dir(), self.clip_pack.clone()) else {
            self.status = "Semantic search: no NPU libs or CLIP pack".into();
            host.haptic(Haptic::Warning);
            return;
        };
        // poll_clip_search labels the result from this held query text.
        self.clip_text_q = query.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.clip_search_rx = Some(rx);
        self.clip_search_running = true;
        self.status = format!("Searching \"{}\"…", elide(&query, 32));
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result =
                crate::local_engine::embed_clip_text(std::path::PathBuf::from(lib_dir), pack_dir, query);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        host.haptic(Haptic::Medium);
    }

    /// Drain a finished query embedding: rank the index by cosine into the similarity view, or note.
    #[cfg(feature = "local-npu")]
    fn poll_clip_search(&mut self) {
        let Some(rx) = self.clip_search_rx.as_ref() else { return };
        match rx.try_recv() {
            Ok(Ok(emb)) => {
                self.clip_search_rx = None;
                self.clip_search_running = false;
                let exclude = HashSet::new();
                let ranked = clip_index::rank_candidates(&emb, &self.clip_index, &exclude, 0.15);
                let n = ranked.len().min(200);
                let keys: Vec<String> = ranked.into_iter().take(200).map(|(k, _)| k).collect();
                let q = self.clip_text_q.trim().to_string();
                // Stale result after the box was cleared or edited.
                if q.is_empty() || self.gallery_q.trim() != q {
                    return;
                }
                if keys.is_empty() {
                    self.status = format!(
                        "No matches for \"{}\" ({} indexed)",
                        elide(&q, 32),
                        self.clip_index.len()
                    );
                    if self.ranked.as_ref().is_some_and(|r| r.is_semantic()) {
                        self.ranked = None;
                    }
                } else {
                    let visible_n =
                        keys.iter().filter(|k| self.gallery.iter().any(|it| it.key() == **k)).count();
                    self.status = format!(
                        "{n} matches for \"{}\" ({visible_n} in view, {} indexed)",
                        elide(&q, 32),
                        self.clip_index.len()
                    );
                    self.ranked = Some(RankedGallery::Semantic(keys));
                }
            }
            Ok(Err(e)) => {
                self.clip_search_rx = None;
                self.clip_search_running = false;
                self.log.error(format!("clip text search: {e}"));
                self.status = format!("Semantic search failed: {e}");
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.clip_search_rx = None;
                self.clip_search_running = false;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn gallery_tag_bar(&mut self, ui: &mut egui::Ui, facets: &[(String, usize)]) {
        if self.tag_index.is_empty() {
            return;
        }
        ui.horizontal(|ui| {
            let clear = !self.tag_q.is_empty() || !self.tag_facets.is_empty();
            let clear_w = if clear { 34.0 } else { 0.0 };
            let box_w = (ui.available_width() - clear_w - 8.0).max(72.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.tag_q)
                    .hint_text(format!("{} tags", icons::SEARCH))
                    .desired_width(box_w),
            );
            if clear && ui.button(icons::CLOSE).on_hover_text("Clear tag filters").clicked() {
                self.tag_q.clear();
                self.tag_facets.clear();
            }
        });
        if !facets.is_empty() {
            crate::theme::scroll_horizontal().id_salt("gallery_facets").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (tag, count) in facets {
                        let on = self.tag_facets.iter().any(|f| f == tag);
                        if ui
                            .add(egui::Button::selectable(on, format!("{tag} ({count})")))
                            .clicked()
                        {
                            if on {
                                self.tag_facets.retain(|f| f != tag);
                            } else {
                                self.tag_facets.push(tag.clone());
                            }
                        }
                    }
                });
            });
        }
    }

    fn gallery_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        let connected = matches!(self.conn, Conn::Connected);
        // A finished WD14 read floats over the gallery, viewer open or not.
        #[cfg(feature = "local-npu")]
        self.wd14_sheet_window(ui.ctx(), host);
        if self.triage.is_some() {
            self.triage_view(ui, host);
            return;
        }
        if self.viewer.is_some() {
            self.gallery_viewer(ui, host);
            self.remix_sheet_window(ui.ctx(), host);
            self.finish_sheet_window(ui.ctx(), host);
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

        let mut open_triage = false;
        ui.horizontal(|ui| {
            ui.strong(format!("{} Gallery", icons::GALLERY));
            if self.untriaged.len() >= 2 {
                if ui
                    .button(format!("{} Triage ({})", icons::STAR, self.untriaged.len()))
                    .on_hover_text("Grade-pass the recent results — swipe to keep, trash, or reuse")
                    .clicked()
                {
                    open_triage = true;
                }
            }
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
            #[cfg(feature = "local-npu")]
            if let Some((done, listed)) = self.autotag_progress() {
                ui.separator();
                ui.weak(format!("Auto-tag {done}/{listed}"));
            }
        });
        if open_triage {
            self.open_triage(host);
            if self.triage.is_some() {
                return;
            }
        }
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
        self.tags_window(ui.ctx());

        let mut refresh = false;
        egui::Panel::bottom("gallery-controls").show(ui, |ui| {
            ui.add_space(2.0);
            if self.select_mode {
                self.selection_bar(ui, host);
            } else {
                refresh = self.gallery_controls(ui, connected, host);
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
                self.gallery_list_q(),
                &self.gallery_view,
            );
        }

        // Client-side filters: media, then the local auto-tag layer (search box, facet chips, rating).
        let media = self.gallery_view.media;
        let rating = self.gallery_view.rating;
        let tag_q = self.tag_q.trim().to_string();
        // A ranked view overrides the filters and keeps cosine order.
        let visible: Vec<usize> = if let Some(ranked) = &self.ranked {
            let by_key: HashMap<String, usize> =
                self.gallery.iter().enumerate().map(|(i, it)| (it.key(), i)).collect();
            ranked.keys().iter().filter_map(|k| by_key.get(k).copied()).collect()
        } else {
            self.gallery
                .iter()
                .enumerate()
                .filter(|(_, it)| media.matches(it.is_video))
                .filter(|(_, it)| {
                    let key = it.key();
                    (tag_q.is_empty() || self.tag_index.matches(&key, &tag_q))
                        && self.tag_facets.iter().all(|f| self.tag_index.matches(&key, f))
                        && rating.matches(self.tag_index.is_nsfw(&key))
                        && match self.index_filter {
                            1 => self.tag_index.contains(&key),
                            2 => !self.tag_index.contains(&key),
                            _ => true,
                        }
                })
                .map(|(i, _)| i)
                .collect()
        };
        let mut visible = visible;
        // Aesthetic order: indexed scores descending, unscored after, stable within ties.
        if self.gallery_view.sort == GallerySort::Score && self.ranked.is_none() {
            let score_of =
                |i: usize| self.gallery.get(i).and_then(|it| self.clip_index.score(&it.key()));
            visible.sort_by(|&a, &b| match (score_of(a), score_of(b)) {
                (Some(x), Some(y)) => y.total_cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
        }
        // Banner only for More like this — semantic search clears by emptying the box.
        if self.ranked.as_ref().is_some_and(|r| r.is_similar()) {
            ui.horizontal(|ui| {
                ui.label(format!("{} Similar images", icons::SEARCH));
                if ui.small_button("Show all").clicked() {
                    self.ranked = None;
                }
            });
        }
        // Facet chips reflect the currently visible set.
        let facet_keys: Vec<String> =
            visible.iter().filter_map(|&i| self.gallery.get(i).map(|it| it.key())).collect();
        let facets = self.tag_index.top_tags(&facet_keys, 12);
        self.gallery_tag_bar(ui, &facets);
        // Ranked order is the point of that view; grouping would destroy it.
        let group = if self.ranked.is_some() {
            crate::types::GalleryGroup::None
        } else {
            self.gallery_view.group
        };
        let groups =
            crate::gallery::group_selected(&self.gallery, &visible, group, &self.characters);
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
        // Same when a View/menu popup is open — otherwise scrolls and holds bleed into the grid.
        let mut scroll = crate::theme::scroll_vertical()
            .id_salt("gallery_list")
            .auto_shrink([false, false]);
        let menu_open = ui.ctx().any_popup_open();
        if self.sel_painting || menu_open {
            use egui::containers::scroll_area::{DragScroll, ScrollSource};
            scroll = scroll.scroll_source(ScrollSource { drag: DragScroll::Never, ..Default::default() });
        }
        if let Some(y) = self.gallery_scroll_restore.take() {
            scroll = scroll.vertical_scroll_offset(y);
        }
        let scroll_out = scroll.show(ui, |ui| {
            self.gallery_grid_clip = ui.clip_rect();
            for group in &groups {
                if group.label.is_empty() {
                    open = self.gallery_grid(ui, &group.items, cols).or(open);
                    continue;
                }
                let header = format!("{} ({})", elide(&group.label, 40), group.items.len());
                let id = ui.make_persistent_id(&group.key);
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    id,
                    self.gallery_view.groups_open,
                )
                .show_header(ui, |ui| {
                    ui.label(&header);
                    let keys: Vec<String> = group
                        .items
                        .iter()
                        .filter_map(|&i| self.gallery.get(i).map(|it| it.key()))
                        .collect();
                    let all_sel =
                        !keys.is_empty() && keys.iter().all(|k| self.selected.contains(k));
                    let btn = if all_sel { "None" } else { "All" };
                    if ui
                        .small_button(btn)
                        .on_hover_text(if all_sel {
                            "Clear selection in this group"
                        } else {
                            "Select every image in this group"
                        })
                        .clicked()
                    {
                        self.select_mode = true;
                        if all_sel {
                            for k in &keys {
                                self.selected.remove(k);
                            }
                        } else {
                            for k in keys {
                                self.selected.insert(k);
                            }
                        }
                    }
                })
                .body(|ui| {
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
                self.gallery_list_q(),
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
        if self.sel_painting || ui.ctx().any_popup_open() {
            self.gallery_pull = 0.0;
            self.gallery_pull_tracking = false;
            return false;
        }
        let at_top = self.gallery_scroll_y <= 1.0;
        let (pressed, released, down, delta_y, pos) = ui.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.any_released(),
                i.pointer.any_down(),
                i.pointer.delta().y,
                i.pointer.interact_pos(),
            )
        });

        if !at_top {
            self.gallery_pull = 0.0;
            self.gallery_pull_tracking = false;
            return false;
        }

        if pressed {
            // Only presses starting inside the grid arm the pull — a drag on the tag chip row
            // or header must never turn into a refresh.
            let in_grid = pos.is_some_and(|p| self.gallery_grid_clip.contains(p));
            self.gallery_pull_tracking = at_top && in_grid;
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
            self.request_delete_images(items);
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
    fn request_delete_images(&mut self, items: Vec<(String, String)>) {
        if items.is_empty() {
            return;
        }
        if self.confirm_gallery_delete {
            self.delete_confirm = Some((items, false));
        } else {
            self.engine.as_ref().unwrap().delete_images(items);
        }
    }

    /// Prefer the next filmstrip neighbor after a viewer delete; fall back to previous.
    fn remember_viewer_neighbor_after_delete(&mut self) {
        let Some(v) = self.viewer.as_ref() else {
            self.viewer_after_delete = None;
            return;
        };
        let idx = v.idx;
        let key = self
            .gallery_neighbor(idx, 1)
            .or_else(|| self.gallery_neighbor(idx, -1))
            .and_then(|i| {
                self.gallery
                    .get(i)
                    .map(|it| (it.subfolder.clone(), it.filename.clone()))
            });
        self.viewer_after_delete = key;
    }

    /// Reopen the neighbor captured before delete, or close if the listing is empty.
    fn resume_viewer_after_delete(&mut self, host: &Host) {
        let Some((sub, file)) = self.viewer_after_delete.take() else {
            // Keep an open viewer in sync when the list reloads under it.
            if let Some(v) = &self.viewer {
                let key = v.item.key();
                if let Some(idx) = self.gallery.iter().position(|it| it.key() == key) {
                    if let Some(v) = self.viewer.as_mut() {
                        v.idx = idx;
                    }
                } else if self.viewer.is_some() {
                    self.viewer = None;
                    self.player = None;
                    self.viewer_swipe_origin = None;
                }
            }
            return;
        };
        if let Some(idx) = self
            .gallery
            .iter()
            .position(|it| it.subfolder == sub && it.filename == file)
        {
            self.open_viewer(idx, host);
        } else {
            self.viewer = None;
            self.player = None;
            self.viewer_swipe_origin = None;
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
        centered(ctx, egui::Window::new("New album"))
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
        centered(ctx, egui::Window::new("Delete images?"))
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
            self.viewer_after_delete = None;
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
        // Menus sit above the grid but this handler reads raw pointer pos — ignore while any
        // popup is open so Model/Album lists and hold-on-item don't paint-select tiles behind.
        if ui.ctx().any_popup_open() {
            self.sel_press = None;
            self.sel_long_fired = false;
            self.sel_painting = false;
            return;
        }
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
        if self.sel_painting {
            if let Some(idx) = tile_at(pos, &self.tile_hits)
                && let Some(item) = self.gallery.get(idx)
            {
                self.selected.insert(item.key());
            }
            // Auto-scroll when the finger sits in the top/bottom edge of the grid.
            let clip = self.gallery_grid_clip;
            if clip.height() > 8.0 {
                const ZONE: f32 = 72.0;
                let dt = ui.input(|i| i.stable_dt).clamp(1.0 / 120.0, 0.05);
                let mut dy = 0.0_f32;
                if pos.y < clip.top() + ZONE {
                    let t = ((clip.top() + ZONE - pos.y) / ZONE).clamp(0.0, 1.0);
                    dy = -(280.0 + 720.0 * t) * dt;
                } else if pos.y > clip.bottom() - ZONE {
                    let t = ((pos.y - (clip.bottom() - ZONE)) / ZONE).clamp(0.0, 1.0);
                    dy = (280.0 + 720.0 * t) * dt;
                }
                if dy != 0.0 {
                    self.gallery_scroll_restore = Some((self.gallery_scroll_y + dy).max(0.0));
                    ui.ctx().request_repaint();
                }
            }
        }
    }

    /// Lay out `indices` as `cols` tiles per row, returning the index of any tile tapped.
    ///
    /// At one column tiles take the image's own aspect ratio (full-width reading), so the row
    /// height is only known once the thumbnail decodes; before that a 1:1 placeholder holds the
    /// space. In the grid, tiles stay square so rows line up.
    fn gallery_grid(&mut self, ui: &mut egui::Ui, indices: &[usize], cols: usize) -> Option<usize> {
        let mut open = None;
        let clip = ui.clip_rect();
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
                    // Clip to the viewport so a straddling tile can't catch presses under the nav bar.
                    self.tile_hits.push((rect.intersect(clip), idx));
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
                                self.engine.as_ref().unwrap().fetch_thumb(
                                    subfolder,
                                    filename,
                                    size,
                                    self.full_cache_root.clone(),
                                );
                            }
                            ui.put(rect, egui::Button::new(elide(&item_key, 14)).wrap()).clicked()
                        }
                    };
                    // Videos (which the server may not thumbnail) get a play badge so they're
                    // recognizable even as a blank tile.
                    if is_video {
                        video_badge(ui, rect);
                    }
                    // Tiny corner dot marks tag-indexed images.
                    if self.tag_index.contains(&item_key) {
                        let c = rect.right_top() + egui::vec2(-7.0, 7.0);
                        ui.painter().circle_filled(c, 3.0, egui::Color32::from_rgb(120, 220, 140));
                    }
                    if select_mode {
                        selection_overlay(ui, rect, selected);
                    }
                    // Skip tile taps while a menu is open (same frame as an outside-dismiss).
                    if clicked && !suppress_click && !ui.ctx().any_popup_open() {
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

    /// Centered picker over the server gallery's image items; a tap fetches full bytes and sets
    /// them as the img2img / video start input. Reuses the Gallery tab's listing + thumb cache.
    fn gallery_pick_window(&mut self, ctx: &egui::Context, host: &Host) {
        if !self.gallery_pick_open {
            return;
        }
        let connected = matches!(self.conn, Conn::Connected);
        let mut open = true;
        let mut pick: Option<usize> = None;
        let mut refresh = false;
        centered(ctx, egui::Window::new(format!("{} From gallery", icons::GALLERY)))
            .collapsible(false)
            .open(&mut open)
            .default_size([360.0, 460.0])
            .show(ctx, |ui| {
                if !connected {
                    ui.add_space(12.0);
                    ui.weak("Connect to a server to browse its gallery.");
                    return;
                }
                ui.horizontal(|ui| {
                    if ui.button(format!("{} Refresh", icons::REFRESH)).clicked() {
                        refresh = true;
                    }
                    if self.gallery_loading {
                        ui.spinner();
                    }
                    if self.gallery_pick_pending.is_some() {
                        ui.spinner();
                        ui.weak("loading image…");
                    }
                });
                if !self.gallery_status.is_empty() {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 160, 120),
                        elide(&self.gallery_status, 120),
                    );
                }
                ui.separator();
                let images: Vec<usize> = self
                    .gallery
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| !it.is_video)
                    .map(|(i, _)| i)
                    .collect();
                if images.is_empty() {
                    ui.add_space(16.0);
                    ui.vertical_centered(|ui| {
                        if self.gallery_loading {
                            ui.spinner();
                        } else {
                            ui.weak("No gallery images yet.");
                        }
                    });
                    return;
                }
                crate::theme::scroll_vertical()
                    .id_salt("gallery_pick_grid")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        pick = self.gallery_pick_grid(ui, &images);
                        ui.add_space(8.0);
                    });
            });
        // Load the listing if the Gallery tab hasn't already fetched it.
        if connected && self.gallery.is_empty() && self.gallery_total == 0 && !self.gallery_loading {
            self.gallery_loading = true;
            self.engine.as_ref().unwrap().gallery_list(
                self.gallery_gen,
                0,
                self.gallery_page_size(),
                self.gallery_list_q(),
                &self.gallery_view,
            );
        }
        if refresh && connected {
            self.refresh_gallery();
        }
        if let Some(idx) = pick {
            self.pick_gallery_input(idx, host);
        }
        if !open {
            self.gallery_pick_open = false;
        }
    }

    /// Thumbnail grid for the gallery picker; returns the tapped listing index.
    fn gallery_pick_grid(&mut self, ui: &mut egui::Ui, indices: &[usize]) -> Option<usize> {
        let (cols, tile) = Self::picker_grid_dims(ui);
        let size = 320u32;
        let mut pick = None;
        for row in indices.chunks(cols) {
            ui.horizontal(|ui| {
                for &idx in row {
                    let (thumb_key, subfolder, filename) = {
                        let Some(item) = self.gallery.get(idx) else { continue };
                        (item.thumb_key(size), item.subfolder.clone(), item.filename.clone())
                    };
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(tile, tile), egui::Sense::hover());
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    let clicked = match self.thumbs.get(&thumb_key) {
                        Some(tex) => {
                            let img = egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                                .fit_to_exact_size(egui::vec2(tile, tile))
                                .sense(egui::Sense::click());
                            ui.put(rect, img).clicked()
                        }
                        None => {
                            if self.thumbs.claim(&thumb_key) {
                                self.engine.as_ref().unwrap().fetch_thumb(
                                    subfolder,
                                    filename,
                                    size,
                                    self.full_cache_root.clone(),
                                );
                            }
                            ui.put(rect, egui::Button::new(elide(&thumb_key, 12)).wrap()).clicked()
                        }
                    };
                    if clicked {
                        pick = Some(idx);
                    }
                }
            });
        }
        pick
    }

    /// Fetch the tapped gallery image's full bytes; the result lands as the picked input.
    fn pick_gallery_input(&mut self, idx: usize, host: &Host) {
        let Some(item) = self.gallery.get(idx).cloned() else { return };
        let cache_dir = self.ensure_full_cache_root(host).map(|s| s.to_string());
        self.gallery_pick_pending = Some((item.key(), item.filename.clone()));
        self.engine.as_ref().unwrap().fetch_full(item.subfolder, item.filename, cache_dir);
        self.gallery_pick_open = false;
        self.note = "Loading gallery image…".into();
        host.haptic(Haptic::Light);
    }

    fn open_viewer(&mut self, idx: usize, host: &Host) {
        let Some(item) = self.gallery.get(idx).cloned() else { return };
        // Any previous item's playback ends here (drop stops the decode thread).
        self.player = None;
        let cache_dir = self.ensure_full_cache_root(host).map(|s| s.to_string());
        let engine = self.engine.as_ref().unwrap();
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
        self.viewer_remix_pending = false;
        self.remix_sheet = None;
        self.viewer_remix_press = None;
        self.viewer_remix_long_fired = false;
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

    /// Recompute untriaged keys: still images new since the pre-burst snapshot, else the N newest.
    fn collect_untriaged(&mut self) {
        let n = self.pending_triage_n;
        let prev = std::mem::take(&mut self.pre_burst_keys);
        if prev.is_empty() && n == 0 {
            self.untriaged.clear();
            return;
        }
        let mut cand: Vec<usize> = (0..self.gallery.len())
            .filter(|&i| !self.gallery[i].is_video)
            .filter(|&i| prev.is_empty() || !prev.contains(&self.gallery[i].key()))
            .collect();
        cand.sort_by(|&a, &b| {
            self.gallery[b].mtime.unwrap_or(0.0).total_cmp(&self.gallery[a].mtime.unwrap_or(0.0))
        });
        if n > 0 {
            cand.truncate(n);
        }
        self.untriaged = cand.into_iter().map(|i| self.gallery[i].key()).collect();
    }

    /// Deck order: aesthetic score descending when any card is scored, else newest first.
    fn triage_deck_order(&self, keys: &[String]) -> Vec<String> {
        let scored = keys.iter().any(|k| self.clip_index.score(k).is_some());
        let mtime: HashMap<String, f64> =
            self.gallery.iter().map(|it| (it.key(), it.mtime.unwrap_or(0.0))).collect();
        let mut order = keys.to_vec();
        order.sort_by(|a, b| {
            if scored {
                match (self.clip_index.score(a), self.clip_index.score(b)) {
                    (Some(x), Some(y)) => y.total_cmp(&x),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            } else {
                let ma = mtime.get(a).copied().unwrap_or(0.0);
                let mb = mtime.get(b).copied().unwrap_or(0.0);
                mb.total_cmp(&ma)
            }
        });
        order
    }

    /// Enter the grade-pass triage overlay over the current untriaged results.
    fn open_triage(&mut self, host: &Host) {
        let present: HashSet<String> =
            self.gallery.iter().filter(|it| !it.is_video).map(|it| it.key()).collect();
        self.untriaged.retain(|k| present.contains(k));
        if self.untriaged.is_empty() {
            return;
        }
        let deck = self.triage_deck_order(&self.untriaged);
        self.tab = Tab::Gallery;
        self.viewer = None;
        self.triage_swipe_origin = None;
        self.triage = Some(Triage {
            deck,
            pos: 0,
            kept: 0,
            trashed: 0,
            keep: Vec::new(),
            trash: Vec::new(),
            album: None,
            last: None,
            mode: TriageMode::Grade,
        });
        host.haptic(Haptic::Light);
    }

    /// Map gallery keys to `(subfolder, filename)` pairs for engine calls.
    fn items_for_keys(&self, keys: &[String]) -> Vec<(String, String)> {
        let by_key: HashMap<String, &GalleryItem> =
            self.gallery.iter().map(|it| (it.key(), it)).collect();
        keys.iter()
            .filter_map(|k| by_key.get(k).map(|it| (it.subfolder.clone(), it.filename.clone())))
            .collect()
    }

    /// Flush the batched decisions. Grade: soft-delete left-swipes, album-add kept, drop triaged
    /// keys. Character: accept into the card's album, remember denials, clear reviewed suggestions.
    fn commit_triage(&mut self, host: &Host) {
        let Some(t) = self.triage.take() else { return };
        self.triage_swipe_origin = None;
        match &t.mode {
            TriageMode::Grade => {
                let decided: HashSet<String> =
                    t.deck[..t.pos.min(t.deck.len())].iter().cloned().collect();
                self.untriaged.retain(|k| !decided.contains(k));
                if let Some(id) = t.album {
                    let items = self.items_for_keys(&t.keep);
                    if !items.is_empty() {
                        self.engine.as_ref().unwrap().album_add(id, items);
                    }
                }
                if !t.trash.is_empty() {
                    let items = self.items_for_keys(&t.trash);
                    if !items.is_empty() {
                        let n = items.len();
                        self.engine.as_ref().unwrap().delete_images(items);
                        self.gallery_status = format!("Triage: moved {n} to trash");
                        host.haptic(Haptic::Warning);
                    }
                }
            }
            TriageMode::Character { card } => {
                let card_name = card.clone();
                let items = self.items_for_keys(&t.keep);
                if !items.is_empty() {
                    self.add_to_character_album(&card_name, items);
                }
                if !t.trash.is_empty() {
                    let denied = self.character_denied.entry(card_name.clone()).or_default();
                    for k in &t.trash {
                        if !denied.contains(k) {
                            denied.push(k.clone());
                        }
                    }
                }
                if let Some(sug) = self.character_suggestions.get_mut(&card_name) {
                    let decided: HashSet<&String> = t.keep.iter().chain(&t.trash).collect();
                    sug.retain(|k| !decided.contains(k));
                }
                self.character_centroids.remove(&card_name);
                self.gallery_status =
                    format!("Review: accepted {}, denied {}", t.keep.len(), t.trash.len());
                host.haptic(Haptic::Success);
            }
        }
    }

    /// Record a card decision and advance. Grade swipe-up loads the image as input and closes the
    /// deck; character swipe-up is a skip (decide later).
    fn triage_pick(&mut self, pick: TriagePick, host: &Host) {
        let (key, char_skip) = {
            let Some(t) = self.triage.as_mut() else { return };
            let Some(key) = t.deck.get(t.pos).cloned() else { return };
            let char_mode = matches!(t.mode, TriageMode::Character { .. });
            let mut char_skip = false;
            match pick {
                TriagePick::Keep => {
                    t.keep.push(key.clone());
                    t.kept += 1;
                    t.pos += 1;
                    t.last = Some(pick);
                }
                TriagePick::Trash => {
                    t.trash.push(key.clone());
                    t.trashed += 1;
                    t.pos += 1;
                    t.last = Some(pick);
                }
                TriagePick::Input => {
                    t.pos += 1;
                    // A skip isn't a keep/trash, so it isn't an undoable step.
                    t.last = if char_mode { None } else { Some(pick) };
                    char_skip = char_mode;
                }
            }
            (key, char_skip)
        };
        match pick {
            TriagePick::Keep => host.haptic(Haptic::Light),
            TriagePick::Trash => host.haptic(Haptic::Warning),
            TriagePick::Input if char_skip => host.haptic(Haptic::Light),
            TriagePick::Input => {
                self.use_key_as_input(&key, host);
                self.commit_triage(host);
                host.haptic(Haptic::Medium);
            }
        }
    }

    /// Revert the last keep/trash decision, stepping the deck back one card.
    fn undo_triage(&mut self, host: &Host) {
        let Some(t) = self.triage.as_mut() else { return };
        let Some(last) = t.last.take() else { return };
        if t.pos == 0 {
            return;
        }
        t.pos -= 1;
        match last {
            TriagePick::Keep => {
                t.keep.pop();
                t.kept = t.kept.saturating_sub(1);
            }
            TriagePick::Trash => {
                t.trash.pop();
                t.trashed = t.trashed.saturating_sub(1);
            }
            TriagePick::Input => {}
        }
        host.haptic(Haptic::Light);
    }

    /// Fetch a gallery image's full bytes as the img2img input and jump to Create.
    fn use_key_as_input(&mut self, key: &str, host: &Host) {
        let Some(item) = self.gallery.iter().find(|it| it.key() == key).cloned() else { return };
        let cache_dir = self.ensure_full_cache_root(host).map(|s| s.to_string());
        self.gallery_pick_pending = Some((item.key(), item.filename.clone()));
        self.engine.as_ref().unwrap().fetch_full(item.subfolder, item.filename, cache_dir);
        self.params.mode = Mode::Img2Img;
        self.tab = Tab::Generate;
        self.note = "Gallery image set as img2img input".into();
    }

    /// Card swipe over `rect`: right keeps, left trashes, up reuses; small/downward drags ignored.
    fn triage_swipe(&mut self, ui: &egui::Ui, rect: egui::Rect) -> Option<TriagePick> {
        let (pressed, released, down, pos) = ui.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.any_released(),
                i.pointer.any_down(),
                i.pointer.latest_pos().or(i.pointer.interact_pos()),
            )
        });
        if pressed {
            self.triage_swipe_origin = pos.filter(|p| rect.contains(*p));
        }
        if released {
            let origin = self.triage_swipe_origin.take()?;
            let pos = pos?;
            let d = pos - origin;
            let (ax, ay) = (d.x.abs(), d.y.abs());
            if ax < 56.0 && ay < 56.0 {
                return None;
            }
            if ax > ay {
                return Some(if d.x > 0.0 { TriagePick::Keep } else { TriagePick::Trash });
            }
            if d.y < 0.0 {
                return Some(TriagePick::Input);
            }
            return None;
        }
        if !down {
            self.triage_swipe_origin = None;
        }
        None
    }

    /// Fullscreen swipe deck: grade pass (keep/trash/reuse) or character review (accept/deny/skip);
    /// batch committed on exit.
    fn triage_view(&mut self, ui: &mut egui::Ui, host: &Host) {
        if ui.ctx().input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
        }) {
            self.commit_triage(host);
            return;
        }
        enum TA {
            Pick(TriagePick),
            Undo,
            SetAlbum(Option<i64>),
            Skip,
            Done,
        }
        let mut act: Option<TA> = None;
        let (total, pos, kept, trashed, album, cur_key, review) = {
            let t = self.triage.as_ref().unwrap();
            let review = match &t.mode {
                TriageMode::Grade => None,
                TriageMode::Character { card } => Some(card.clone()),
            };
            (t.deck.len(), t.pos, t.kept, t.trashed, t.album, t.deck.get(t.pos).cloned(), review)
        };
        let left = total.saturating_sub(pos);

        ui.horizontal(|ui| {
            match &review {
                Some(card) => ui.strong(format!("{} Review: {}", icons::STAR, elide(card, 22))),
                None => ui.strong(format!("{} Grade pass", icons::STAR)),
            };
            ui.separator();
            if pos < total {
                ui.weak(format!("{}/{}", pos + 1, total));
                if let Some(k) = &cur_key {
                    if let Some(s) = self.clip_index.score(k) {
                        ui.weak(format!("· score {s:.2}"));
                    }
                }
                ui.separator();
            }
            if review.is_some() {
                ui.weak(format!("Accepted {kept} · Denied {trashed} · {left} left"));
            } else {
                ui.weak(format!("Kept {kept} · Trashed {trashed} · {left} left"));
            }
        });
        ui.separator();

        let can_undo = pos > 0;
        egui::Panel::bottom("triage-actions").show(ui, |ui| {
            const BTN_H: f32 = 40.0;
            const GAP: f32 = 4.0;
            ui.add_space(2.0);
            if cur_key.is_some() {
                let (left_lbl, left_hint, mid_lbl, mid_hint, right_lbl) = if review.is_some() {
                    (
                        format!("{} Deny", icons::CLOSE),
                        "Swipe left — never suggest again",
                        format!("{} Skip", icons::REDO),
                        "Swipe up — decide later",
                        format!("{} Accept", icons::CHECK),
                    )
                } else {
                    (
                        format!("{} Trash", icons::TRASH),
                        "Swipe left",
                        format!("{} Input", icons::IMAGE),
                        "Swipe up — use as img2img input",
                        format!("{} Keep", icons::CHECK),
                    )
                };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    let w = ((ui.available_width() - GAP * 3.0) / 4.0).max(40.0);
                    let size = egui::vec2(w, BTN_H);
                    if ui
                        .add_sized(size, egui::Button::new(left_lbl))
                        .on_hover_text(left_hint)
                        .clicked()
                    {
                        act = Some(TA::Pick(TriagePick::Trash));
                    }
                    if ui
                        .add_sized(size, egui::Button::new(mid_lbl))
                        .on_hover_text(mid_hint)
                        .clicked()
                    {
                        act = Some(TA::Pick(TriagePick::Input));
                    }
                    if ui
                        .add_enabled(can_undo, egui::Button::new(icons::UNDO).min_size(size))
                        .on_hover_text("Undo last")
                        .clicked()
                    {
                        act = Some(TA::Undo);
                    }
                    if ui
                        .add_sized(size, egui::Button::new(right_lbl))
                        .on_hover_text("Swipe right")
                        .clicked()
                    {
                        act = Some(TA::Pick(TriagePick::Keep));
                    }
                });
                match &review {
                    // Character mode: accepted images join the card's album (created on demand).
                    Some(card) => {
                        ui.weak(format!(
                            "{} Accept adds to the {} album",
                            icons::ALBUM,
                            elide(card, 20)
                        ));
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.weak(format!("{} Keep to", icons::ALBUM));
                            let label = album
                                .and_then(|id| self.albums.iter().find(|a| a.id == id))
                                .map(|a| elide(&a.name, 20))
                                .unwrap_or_else(|| "gallery only".into());
                            up_menu(ui, label, |ui| {
                                if ui.selectable_label(album.is_none(), "Gallery only").clicked() {
                                    act = Some(TA::SetAlbum(None));
                                    ui.close();
                                }
                                for a in &self.albums {
                                    if ui
                                        .selectable_label(album == Some(a.id), elide(&a.name, 28))
                                        .clicked()
                                    {
                                        act = Some(TA::SetAlbum(Some(a.id)));
                                        ui.close();
                                    }
                                }
                            });
                        });
                    }
                }
            } else {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    let w = ((ui.available_width() - GAP) / 2.0).max(40.0);
                    let size = egui::vec2(w, BTN_H);
                    if ui
                        .add_enabled(
                            can_undo,
                            egui::Button::new(format!("{} Undo", icons::UNDO)).min_size(size),
                        )
                        .clicked()
                    {
                        act = Some(TA::Undo);
                    }
                    if ui
                        .add_sized(size, egui::Button::new(format!("{} Done", icons::CHECK)))
                        .clicked()
                    {
                        act = Some(TA::Done);
                    }
                });
            }
            ui.add_space(2.0);
        });

        if let Some(key) = cur_key.clone() {
            match self.gallery.iter().find(|it| it.key() == key).cloned() {
                Some(item) => {
                    let size = 1024u32;
                    let thumb_key = item.thumb_key(size);
                    let rect = ui.available_rect_before_wrap();
                    match self.thumbs.get(&thumb_key) {
                        Some(tex) => {
                            let sized = egui::load::SizedTexture::from_handle(tex);
                            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                                ui.centered_and_justified(|ui| {
                                    ui.add(
                                        egui::Image::new(sized)
                                            .max_size(rect.size())
                                            .maintain_aspect_ratio(true),
                                    );
                                });
                            });
                        }
                        None => {
                            if self.thumbs.claim(&thumb_key) {
                                self.engine.as_ref().unwrap().fetch_thumb(
                                    item.subfolder.clone(),
                                    item.filename.clone(),
                                    size,
                                    self.full_cache_root.clone(),
                                );
                            }
                            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                                ui.centered_and_justified(|ui| ui.spinner());
                            });
                        }
                    }
                    if act.is_none()
                        && let Some(pick) = self.triage_swipe(ui, rect)
                    {
                        act = Some(TA::Pick(pick));
                    }
                }
                None => act = Some(TA::Skip),
            }
        } else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                if review.is_some() {
                    ui.label(format!("All {total} reviewed — accepted {kept}, denied {trashed}."));
                    ui.weak("Accepted images join the album; denied never resurface.");
                } else {
                    ui.label(format!("All {total} triaged — kept {kept}, trashed {trashed}."));
                    ui.weak("Trashed images go to the server trash on Done.");
                }
            });
        }

        match act {
            Some(TA::Pick(p)) => self.triage_pick(p, host),
            Some(TA::Undo) => self.undo_triage(host),
            Some(TA::SetAlbum(a)) => {
                if let Some(t) = &mut self.triage {
                    t.album = a;
                }
            }
            Some(TA::Skip) => {
                if let Some(t) = &mut self.triage {
                    t.pos += 1;
                    t.last = None;
                }
            }
            Some(TA::Done) => self.commit_triage(host),
            None => {}
        }
    }

    fn gallery_viewer(&mut self, ui: &mut egui::Ui, host: &Host) {
        enum Act {
            Close,
            Save,
            Remix,
            RemixInstant,
            SaveCharacter,
            MoreLike,
            UseAsInput,
            Inpaint,
            Finish,
            OpenWorkflow,
            CopyWorkflow,
            ToggleMeta,
            AlbumAdd(i64),
            AlbumRemove(i64),
            AlbumCreate,
            SetPortrait(String),
            Delete,
            Show(usize),
            #[cfg(feature = "local-npu")]
            ReadTags,
        }
        let mut act: Option<Act> = None;
        // Video-only finish button availability; a reason disables it via hover.
        let finish_disabled = self.finish_disabled_reason();
        #[cfg(feature = "local-npu")]
        self.ensure_local_packs(host, false);
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
            // Remix once the scraped meta is non-empty, or while it is still loading (deferred).
            let can_remix = v.meta.as_ref().is_some_and(|m| !m.is_empty()) || v.meta_loading;
            // WD14 Read tags: a still image with loaded bytes, a pack present, no read in flight.
            #[cfg(feature = "local-npu")]
            let can_read_tags =
                !v.item.is_video && v.bytes.is_some() && self.wd14_pack.is_some() && !self.wd14_running;
            let mut remix_held = false;
            egui::Panel::bottom("viewer-actions").show(ui, |ui| {
                const BTN_H: f32 = 36.0;
                const GAP: f32 = 4.0;
                ui.add_space(2.0);
                // Back · Save · [Finish] · Remix · Trash · More — More last so delete isn't the far-right tap.
                let n = if v.item.is_video { 6.0 } else { 5.0 };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    let btn_w = ((ui.available_width() - GAP * (n - 1.0)) / n).max(36.0);
                    let size = egui::vec2(btn_w, BTN_H);
                    if ui
                        .add_sized(size, egui::Button::new(icons::BACK))
                        .on_hover_text("Back to gallery")
                        .clicked()
                    {
                        act = Some(Act::Close);
                    }
                    if ui
                        .add_enabled(can_save, egui::Button::new(icons::SAVE).min_size(size))
                        .on_hover_text("Save to device")
                        .clicked()
                    {
                        act = Some(Act::Save);
                    }
                    if v.item.is_video {
                        let btn = ui.add_enabled(
                            finish_disabled.is_none(),
                            egui::Button::new(icons::RUN).min_size(size),
                        );
                        if btn
                            .on_hover_text(finish_disabled.unwrap_or(
                                "Finish — colour-match, upscale, RIFE-interpolate and re-encode",
                            ))
                            .clicked()
                        {
                            act = Some(Act::Finish);
                        }
                    }
                    let remix = ui
                        .add_enabled(can_remix, egui::Button::new(icons::GENERATE).min_size(size))
                        .on_hover_text("Remix — tap: choose fields, hold: instant");
                    if remix.clicked() {
                        act = Some(Act::Remix);
                    }
                    remix_held = remix.is_pointer_button_down_on();
                    if ui
                        .add_sized(size, egui::Button::new(icons::TRASH))
                        .on_hover_text("Delete image")
                        .clicked()
                    {
                        act = Some(Act::Delete);
                    }
                    up_menu_sized(ui, icons::MENU, size, |ui| {
                        if ui
                            .add_enabled(can_remix, egui::Button::new(format!("{} Save as character", icons::USER)))
                            .on_hover_text("Save this image's tags + LoRAs as a character card")
                            .clicked()
                        {
                            act = Some(Act::SaveCharacter);
                            ui.close();
                        }
                        // Set this image as a character's profile picture.
                        if self.characters.is_empty() {
                            ui.add_enabled(false, egui::Button::new(format!("{} Set as profile", icons::USER)))
                                .on_hover_text("Create a character card first");
                        } else {
                            let active = self.active_character.as_ref().map(|a| a.name.clone());
                            ui.menu_button(format!("{} Set as profile", icons::USER), |ui| {
                                for c in &self.characters {
                                    let is_active = active.as_deref() == Some(c.name.as_str());
                                    let label = if is_active {
                                        format!("{} {}", icons::CHECK, elide(&c.name, 26))
                                    } else {
                                        elide(&c.name, 30)
                                    };
                                    if ui.button(label).clicked() {
                                        act = Some(Act::SetPortrait(c.name.clone()));
                                        ui.close();
                                    }
                                }
                            });
                        }
                        if ui.button(format!("{} Use as img2img input", icons::IMAGE)).clicked() {
                            act = Some(Act::UseAsInput);
                            ui.close();
                        }
                        #[cfg(feature = "local-npu")]
                        if ui
                            .add_enabled(can_read_tags, egui::Button::new(format!("{} Read tags", icons::SEARCH)))
                            .on_hover_text("Tag this image on the NPU (WD14 danbooru tagger)")
                            .clicked()
                        {
                            act = Some(Act::ReadTags);
                        }
                        let can_similar = !v.item.is_video && self.clip_index.contains(&v.item.key());
                        if ui
                            .add_enabled(can_similar, egui::Button::new(format!("{} More like this", icons::SEARCH)))
                            .on_hover_text("Gallery images ranked by CLIP similarity (needs the clip pack + indexing)")
                            .clicked()
                        {
                            act = Some(Act::MoreLike);
                            ui.close();
                        }
                        if ui
                            .add_enabled(can_save, egui::Button::new(format!("{} Fix area (inpaint)", icons::MODEL)))
                            .on_hover_text("Paint a mask to inpaint")
                            .clicked()
                        {
                            act = Some(Act::Inpaint);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(can_open_wf, egui::Button::new(format!("{} Open workflow", icons::GRAPH)))
                            .clicked()
                        {
                            act = Some(Act::OpenWorkflow);
                            ui.close();
                        }
                        if ui
                            .add_enabled(can_open_wf, egui::Button::new(format!("{} Copy workflow", icons::PROPS)))
                            .clicked()
                        {
                            act = Some(Act::CopyWorkflow);
                            ui.close();
                        }
                        ui.separator();
                        ui.weak(format!("{} Albums", icons::ALBUM));
                        if ui
                            .button(format!("{} New album…", icons::ADD))
                            .on_hover_text("Create an album and add this image")
                            .clicked()
                        {
                            act = Some(Act::AlbumCreate);
                            ui.close();
                        }
                        if !albums_known {
                            ui.weak("loading…");
                        } else if self.albums.is_empty() {
                            ui.weak("No albums yet.");
                        } else {
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
                        }
                    })
                    .on_hover_text("More");
                });
                ui.add_space(2.0);
            });
            // A held Remix skips the diff sheet and applies the full meta instantly.
            let meta_ready = self.remix_sheet.is_none()
                && self.viewer.as_ref().and_then(|v| v.meta.as_ref()).is_some_and(|m| !m.is_empty());
            if remix_held && meta_ready {
                let now = ui.input(|i| i.time);
                ui.ctx().request_repaint();
                match self.viewer_remix_press {
                    None => self.viewer_remix_press = Some(now),
                    Some(t) if now - t > 0.5 && !self.viewer_remix_long_fired => {
                        self.viewer_remix_long_fired = true;
                        act = Some(Act::RemixInstant);
                    }
                    _ => {}
                }
            } else {
                self.viewer_remix_press = None;
                self.viewer_remix_long_fired = false;
            }
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
                && self.remix_sheet.is_none()
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
                self.viewer_remix_pending = false;
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
            Some(Act::SetPortrait(name)) => {
                let key = self.viewer.as_ref().map(|v| v.item.key());
                if let (Some(key), Some(c)) =
                    (key, self.characters.iter_mut().find(|c| c.name == name))
                {
                    c.portrait_key = key;
                    self.gallery_status = format!("Profile set for {}", elide(&name, 24));
                    host.haptic(Haptic::Light);
                }
            }
            Some(Act::Delete) => {
                self.remember_viewer_neighbor_after_delete();
                let v = self.viewer.as_ref().unwrap();
                let items = vec![(v.item.subfolder.clone(), v.item.filename.clone())];
                self.request_delete_images(items);
                host.haptic(Haptic::Warning);
            }
            Some(Act::Save) => {
                let v = self.viewer.as_ref().unwrap();
                let (bytes, name) = (v.bytes.clone().unwrap(), v.item.filename.clone());
                self.gallery_status = self.save_bytes(host, &bytes, &name);
            }
            Some(Act::Remix) => {
                if let Some(meta) =
                    self.viewer.as_ref().and_then(|v| v.meta.clone()).filter(|m| !m.is_empty())
                {
                    self.open_remix_sheet(meta);
                    host.haptic(Haptic::Light);
                } else {
                    // Workflow still fetching — open the sheet when Msg::ItemWorkflow lands.
                    self.viewer_remix_pending = true;
                    self.gallery_status = "Loading workflow to remix…".into();
                }
            }
            Some(Act::RemixInstant) => {
                if let Some(meta) =
                    self.viewer.as_ref().and_then(|v| v.meta.clone()).filter(|m| !m.is_empty())
                {
                    self.remix_from_meta(&meta);
                    self.close_viewer();
                    host.haptic(Haptic::Medium);
                }
            }
            Some(Act::SaveCharacter) => {
                if let Some(meta) =
                    self.viewer.as_ref().and_then(|v| v.meta.clone()).filter(|m| !m.is_empty())
                {
                    self.character_draft = Some(CharacterDraft {
                        editing: None,
                        card: Self::character_from_meta(&meta),
                    });
                    self.viewer = None;
                    self.player = None;
                    self.viewer_swipe_origin = None;
                    self.viewer_remix_pending = false;
                    self.gallery_status.clear();
                    self.tab = Tab::Generate;
                    self.create_pane = CreatePane::Characters;
                    self.note = "New character — review and save".into();
                    host.haptic(Haptic::Light);
                } else {
                    self.gallery_status = "Loading workflow…".into();
                }
            }
            Some(Act::MoreLike) => {
                if let Some(key) = self.viewer.as_ref().map(|v| v.item.key()) {
                    let similar: Vec<String> =
                        self.clip_index.top_similar(&key, 60).into_iter().map(|(k, _)| k).collect();
                    if similar.is_empty() {
                        self.gallery_status = "No similar images indexed yet".into();
                    } else {
                        self.ranked = Some(RankedGallery::Similar(similar));
                        self.close_viewer();
                        host.haptic(Haptic::Light);
                    }
                }
            }
            Some(Act::UseAsInput) => {
                let v = self.viewer.as_ref().unwrap();
                // Use the loaded bytes when present, else the server view URL.
                if let Some(bytes) = v.bytes.clone() {
                    let name = v.item.filename.clone();
                    self.set_picked_input(ui.ctx(), name, bytes);
                    self.params.mode = Mode::Img2Img;
                    self.params.img2img_source = Img2ImgSource::Picked;
                    self.tab = Tab::Generate;
                    self.note = "Gallery image set as img2img input".into();
                } else if let Some(url) =
                    self.engine.as_ref().unwrap().view_url(&v.item.subfolder, &v.item.filename)
                {
                    self.params.mode = Mode::Img2Img;
                    self.params.img2img_source = Img2ImgSource::Url;
                    self.params.input_url = url;
                    self.tab = Tab::Generate;
                    self.note = "Gallery image set as img2img input".into();
                }
            }
            #[cfg(feature = "local-npu")]
            Some(Act::ReadTags) => {
                if let Some(bytes) = self.viewer.as_ref().and_then(|v| v.bytes.clone()) {
                    self.start_wd14(ui.ctx(), host, bytes);
                }
            }
            Some(Act::Inpaint) => {
                if let Some((bytes, name)) = self
                    .viewer
                    .as_ref()
                    .and_then(|v| v.bytes.clone().map(|b| (b, v.item.filename.clone())))
                {
                    self.viewer = None;
                    self.player = None;
                    self.viewer_swipe_origin = None;
                    self.viewer_remix_pending = false;
                    self.gallery_status.clear();
                    self.open_inpaint(ui.ctx(), bytes, name);
                }
            }
            Some(Act::Finish) => {
                let v = self.viewer.as_ref().unwrap();
                let video_path = crate::workflow::finish_video_path(
                    &self.server_output_root,
                    &v.item.subfolder,
                    &v.item.filename,
                );
                // Prefer the Create tab's current input photo; else start on the device picker.
                let ref_source = if self.picked_input.is_some() {
                    FinishRef::CurrentInput
                } else {
                    FinishRef::Pick
                };
                self.finish_sheet = Some(FinishSheet {
                    video_path,
                    ref_source,
                    picked: None,
                    scale_by: 2.0,
                    rife_multiplier: 2,
                    output_fps: 32,
                });
                host.haptic(Haptic::Light);
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
        let indexed_tags = self.tag_index.display_names(&v.item.key());
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
                            // Read-only auto-tag chips (the Read tags sheet stays the interactive path).
                            if !indexed_tags.is_empty() {
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    ui.add_space(COPY_W);
                                    ui.strong(format!("{} Tags", icons::SEARCH));
                                });
                                ui.horizontal_wrapped(|ui| {
                                    ui.add_space(COPY_W);
                                    for t in &indexed_tags {
                                        ui.weak(format!("{} {}", icons::DOT, t));
                                    }
                                });
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
                                                self.full_cache_root.clone(),
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
        // Newest first: row 0 is the latest line, so long sessions need no scrolling.
        let total = self.log_lines.len();
        crate::theme::scroll_both()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, total, |ui, range| {
                for line in range.map(|i| &self.log_lines[total - 1 - i]) {
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

    /// App external files dir: internal documents dir's <pkg> → /storage/emulated/0/Android/data/<pkg>/files.
    #[cfg(feature = "local-npu")]
    fn external_files_dir(&self, host: &Host) -> Option<String> {
        let docs = host.documents_dir()?;
        let pkg = std::path::Path::new(&docs).parent()?.file_name()?.to_str()?.to_string();
        Some(format!("/storage/emulated/0/Android/data/{pkg}/files"))
    }

    /// Scan app external files + durable `/sdcard/ComfyUI` for model packs; `force` re-reads.
    #[cfg(feature = "local-npu")]
    fn ensure_local_packs(&mut self, host: &Host, force: bool) {
        if self.local_packs_scanned && !force {
            return;
        }
        self.local_packs_scanned = true;
        let app_root = self.external_files_dir(host);
        let durable = Self::durable_models_dir();
        let mut roots: Vec<&std::path::Path> = Vec::new();
        if let Some(r) = app_root.as_deref() {
            roots.push(std::path::Path::new(r));
        }
        roots.push(std::path::Path::new(durable));
        self.local_packs = crate::local_engine::scan_packs_many(&roots);
        self.wd14_pack = crate::local_engine::find_wd14_pack_many(&roots);
        self.clip_pack = crate::local_engine::find_clip_pack_many(&roots);
        self.rewrite_pack = crate::local_engine::find_rewrite_pack_many(&roots);
        self.pack_mtimes = self
            .local_packs
            .iter()
            .map(|p| p.dir.clone())
            .chain(
                [self.wd14_pack.clone(), self.clip_pack.clone(), self.rewrite_pack.clone()]
                    .into_iter()
                    .flatten(),
            )
            .filter_map(|d| crate::local_engine::dir_newest_mtime(&d).map(|m| (d, m)))
            .collect();
        self.log.info(format!(
            "local-npu: {} pack(s) found: {}; wd14 pack: {}; rewrite pack: {} (roots: app files + {durable})",
            self.local_packs.len(),
            self.local_packs.iter().map(|p| p.label()).collect::<Vec<_>>().join(", "),
            self.wd14_pack.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into()),
            self.rewrite_pack.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into())
        ));
    }

    /// The pack the Local NPU path will use, from the persisted name then the backend.
    #[cfg(feature = "local-npu")]
    fn selected_pack(&self) -> Option<&crate::local_engine::PackEntry> {
        crate::local_engine::pick_pack(&self.local_packs, &self.local_pack, self.local_backend)
    }

    /// True when Create generation runs on the NPU rather than the server (stack on, local model).
    #[cfg(feature = "local-npu")]
    fn route_local_gen(&self) -> bool {
        crate::types::routes_local_generation(self.local_npu, self.local_use_server)
    }

    /// True when the Create tab should present the Anima pipeline (fixed size, euler, txt2img).
    #[cfg(feature = "local-npu")]
    fn anima_active(&self) -> bool {
        self.route_local_gen() && self.local_backend == LocalBackend::Anima
    }

    #[cfg(not(feature = "local-npu"))]
    fn anima_active(&self) -> bool {
        false
    }

    /// Spawn the D3 Anima smoke on a worker thread.
    #[cfg(feature = "local-npu")]
    fn start_d3_anima(&mut self, ctx: &egui::Context, lib_dir: String, pack_dir: std::path::PathBuf) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.d3_rx = Some(rx);
        self.d3_running = true;
        self.d3_ok = None;
        self.log.info(format!("D3-ANIMA starting (libs={lib_dir}, pack={})", pack_dir.display()));
        let prompt = "1girl, portrait, anime".to_string();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report =
                crate::local_engine::anima_smoke(std::path::PathBuf::from(lib_dir), pack_dir, 2, prompt);
            let _ = tx.send(report);
            ctx.request_repaint();
        });
    }

    #[cfg(feature = "local-npu")]
    fn poll_d3_anima(&mut self) {
        let Some(rx) = self.d3_rx.as_ref() else { return };
        match rx.try_recv() {
            Ok(report) => {
                self.d3_rx = None;
                self.d3_running = false;
                self.d3_ok = Some(report.ok);
                let pretty = report.pretty();
                for line in pretty.lines() {
                    self.log.info(format!("D3-ANIMA {line}"));
                }
                self.d3_last = Some(pretty);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.d3_rx = None;
                self.d3_running = false;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Spawn the WD14 tagger on a worker thread; the ranked-tags result returns via the channel.
    #[cfg(feature = "local-npu")]
    fn start_wd14(&mut self, ctx: &egui::Context, host: &Host, bytes: Vec<u8>) {
        let Some(lib_dir) = host.native_lib_dir() else {
            self.gallery_status = "Read tags: nativeLibraryDir unavailable".into();
            host.haptic(Haptic::Warning);
            return;
        };
        let Some(pack_dir) = self.wd14_pack.clone() else {
            self.gallery_status =
                "Read tags: no wd14 pack — push one to the app files dir, then Refresh in Settings".into();
            host.haptic(Haptic::Warning);
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.wd14_rx = Some(rx);
        self.wd14_running = true;
        self.gallery_status = "Reading tags on NPU…".into();
        self.log.info(format!("local-wd14: tagging (libs={lib_dir}, pack={})", pack_dir.display()));
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result =
                crate::local_engine::read_tags(std::path::PathBuf::from(lib_dir), pack_dir, bytes);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        host.haptic(Haptic::Medium);
    }

    /// Drain a finished tag read into the sheet, or surface the error as a status note.
    #[cfg(feature = "local-npu")]
    fn poll_wd14(&mut self) {
        let Some(rx) = self.wd14_rx.as_ref() else { return };
        match rx.try_recv() {
            Ok(Ok(result)) => {
                self.wd14_rx = None;
                self.wd14_running = false;
                if result.general.is_empty() && result.character.is_empty() {
                    self.gallery_status = "Read tags: nothing above threshold".into();
                } else {
                    self.gallery_status.clear();
                    self.wd14_sheet = Some(result);
                }
            }
            Ok(Err(e)) => {
                self.wd14_rx = None;
                self.wd14_running = false;
                self.log.error(format!("local-wd14: {e}"));
                self.gallery_status = format!("Read tags failed: {e}");
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.wd14_rx = None;
                self.wd14_running = false;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// The Create-pane Rewrite menu: rewrite the positive prompt on the CPU LLM. Only shown when a
    /// rewrite pack is present; each item spawns the worker in `start_rewrite`.
    #[cfg(feature = "local-npu")]
    fn rewrite_menu_ui(&mut self, ui: &mut egui::Ui, host: &Host) {
        if self.rewrite_pack.is_none() {
            return;
        }
        use local_rewrite::RewriteKind;
        let video = self.params.mode == Mode::Video;
        ui.menu_button("Rewrite", |ui| {
            if self.rewrite_running {
                ui.add(egui::Spinner::new());
                ui.label("Rewriting…");
                return;
            }
            // Video prose targets the Wan i2v prompt; tags target the image models.
            let kind = if video { RewriteKind::TagsToVideo } else { RewriteKind::ProseToTags };
            if ui.button(kind.label()).clicked() {
                self.start_rewrite(ui.ctx(), host, kind);
                ui.close();
            }
            if ui.button(RewriteKind::ToPony.label()).clicked() {
                self.start_rewrite(ui.ctx(), host, RewriteKind::ToPony);
                ui.close();
            }
            if ui.button(RewriteKind::ToIllustrious.label()).clicked() {
                self.start_rewrite(ui.ctx(), host, RewriteKind::ToIllustrious);
                ui.close();
            }
            if ui.button(RewriteKind::ToAnima.label()).clicked() {
                self.start_rewrite(ui.ctx(), host, RewriteKind::ToAnima);
                ui.close();
            }
        });
    }

    #[cfg(not(feature = "local-npu"))]
    fn rewrite_menu_ui(&mut self, _ui: &mut egui::Ui, _host: &Host) {}

    /// Spawn the CPU prompt rewriter on a worker thread; the rewritten positive prompt returns via
    /// the channel and only replaces the field on success (see `poll_rewrite`).
    #[cfg(feature = "local-npu")]
    fn start_rewrite(&mut self, ctx: &egui::Context, host: &Host, kind: local_rewrite::RewriteKind) {
        let Some(pack_dir) = self.rewrite_pack.clone() else {
            self.status = "Rewrite: no pack — push one to /storage/emulated/0/ComfyUI/rewrite".into();
            host.haptic(Haptic::Warning);
            return;
        };
        let text = self.params.positive.trim().to_string();
        if text.is_empty() {
            self.status = "Rewrite: the prompt is empty".into();
            host.haptic(Haptic::Warning);
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.rewrite_rx = Some(rx);
        self.rewrite_running = true;
        self.status = format!("Rewriting ({}) on CPU…", kind.label());
        self.log.info(format!("local-rewrite: {} (pack={})", kind.label(), pack_dir.display()));
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::local_engine::rewrite_prompt(pack_dir, kind, text);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        host.haptic(Haptic::Medium);
    }

    /// Drain a finished rewrite: replace the positive prompt on success, else a status note.
    #[cfg(feature = "local-npu")]
    fn poll_rewrite(&mut self) {
        let Some(rx) = self.rewrite_rx.as_ref() else { return };
        match rx.try_recv() {
            Ok(Ok(text)) => {
                self.rewrite_rx = None;
                self.rewrite_running = false;
                self.params.positive = text;
                self.status = "Rewrite done".into();
            }
            Ok(Err(e)) => {
                self.rewrite_rx = None;
                self.rewrite_running = false;
                self.log.error(format!("local-rewrite: {e}"));
                self.status = format!("Rewrite failed: {e}");
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.rewrite_rx = None;
                self.rewrite_running = false;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// The ranked-tags sheet: tap a chip to append it to the positive prompt, or Add top 10.
    #[cfg(feature = "local-npu")]
    fn wd14_sheet_window(&mut self, ctx: &egui::Context, host: &Host) {
        if self.wd14_sheet.is_none() {
            return;
        }
        enum WAct {
            Add(String),
            AddTop(usize),
            Close,
        }
        let mut open = true;
        let mut act: Option<WAct> = None;
        let result = self.wd14_sheet.clone().unwrap();
        let max_h = (ctx.content_rect().height() * 0.5).clamp(160.0, 380.0);
        centered(ctx, egui::Window::new(format!("{} Tags", icons::SEARCH)))
            .collapsible(false)
            .open(&mut open)
            .default_width(360.0)
            .show(ctx, |ui| {
                if let Some(r) = &result.rating {
                    ui.weak(format!("Rating: {}  {}%", r.insert_text(), r.percent()));
                }
                ui.horizontal(|ui| {
                    ui.label(format!("{} general tags", result.general.len()));
                    if ui.button(format!("{} Add top 10", icons::ADD)).clicked() {
                        act = Some(WAct::AddTop(10));
                    }
                });
                ui.weak("Tap a tag to add it to the prompt.");
                ui.add_space(4.0);
                crate::theme::scroll_vertical().show(ui, |ui| {
                    ui.set_max_height(max_h);
                    ui.set_min_width(320.0);
                    if !result.character.is_empty() {
                        ui.strong("Character");
                        ui.horizontal_wrapped(|ui| {
                            for t in &result.character {
                                let label = format!("{}  {}%", t.insert_text(), t.percent());
                                if ui.button(label).clicked() {
                                    act = Some(WAct::Add(t.insert_text()));
                                }
                            }
                        });
                        ui.add_space(6.0);
                        ui.strong("General");
                    }
                    ui.horizontal_wrapped(|ui| {
                        for t in &result.general {
                            let label = format!("{}  {}%", t.insert_text(), t.percent());
                            if ui.button(label).clicked() {
                                act = Some(WAct::Add(t.insert_text()));
                            }
                        }
                    });
                });
                ui.add_space(6.0);
                ui.separator();
                if ui.button("Close").clicked() {
                    act = Some(WAct::Close);
                }
            });
        match act {
            Some(WAct::Add(tag)) => {
                self.params.positive = tags::push_chip(&self.params.positive, &tag);
                host.haptic(Haptic::Light);
            }
            Some(WAct::AddTop(n)) => {
                for tag in result.top_general(n) {
                    self.params.positive = tags::push_chip(&self.params.positive, &tag);
                }
                host.haptic(Haptic::Light);
                self.wd14_sheet = None;
            }
            Some(WAct::Close) => self.wd14_sheet = None,
            None => {
                if !open {
                    self.wd14_sheet = None;
                }
            }
        }
    }

    /// New index entries to accumulate before a batched write.
    #[cfg(feature = "local-npu")]
    const AUTOTAG_SAVE_EVERY: usize = 8;

    /// Spawn the WD14 tagger for `key`'s bytes; the ranked result returns tagged with `key`.
    #[cfg(feature = "local-npu")]
    fn autotag_run(&mut self, ctx: &egui::Context, host: &Host, key: String, bytes: Vec<u8>) {
        let (Some(lib_dir), Some(pack_dir)) = (host.native_lib_dir(), self.wd14_pack.clone()) else {
            // Prerequisites vanished mid-fetch; drop the job so the pump moves on.
            self.autotag_failed.insert(key);
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.autotag_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result =
                crate::local_engine::read_tags(std::path::PathBuf::from(lib_dir), pack_dir, bytes);
            let _ = tx.send((key, result));
            ctx.request_repaint();
        });
    }

    /// Drain a finished auto-tag into the index; feed cooc, batch-save, and mark failures.
    #[cfg(feature = "local-npu")]
    fn poll_autotag(&mut self, host: &Host) {
        let Some(rx) = self.autotag_rx.as_ref() else { return };
        match rx.try_recv() {
            Ok((key, Ok(result))) => {
                self.autotag_rx = None;
                self.store_tags(host, key, result);
            }
            Ok((key, Err(e))) => {
                self.autotag_rx = None;
                self.log.warn(format!("auto-tag {}: {e}", elide(&key, 48)));
                self.autotag_failed.insert(key);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.autotag_rx = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Convert a WD14 read into an index entry, feed the personal cooc model once, batch-persist.
    #[cfg(feature = "local-npu")]
    fn store_tags(&mut self, host: &Host, key: String, result: local_wd14::TagResult) {
        let conv =
            |t: &local_wd14::ScoredTag| tag_index::Scored { name: t.name.clone(), prob: t.prob };
        let entry = tag_index::TagEntry {
            version: tag_index::SCHEMA_VERSION,
            general: result.general.iter().map(conv).collect(),
            character: result.character.iter().map(conv).collect(),
            rating: result.rating.as_ref().map(conv),
        };
        if self.cooc_loaded {
            let names: Vec<String> =
                entry.general.iter().chain(&entry.character).map(|t| t.name.clone()).collect();
            if self.cooc.observe(&names) {
                self.save_cooc(host);
            }
        }
        self.tag_index.insert(key, entry);
        self.tag_index_dirty += 1;
        if self.tag_index_dirty >= Self::AUTOTAG_SAVE_EVERY {
            self.save_tag_index(host);
        }
    }

    /// Pick and start the next idle auto-tag job. One image at a time; a generation starting just
    /// stops it choosing new work (an in-flight tag holds the run_lock and finishes).
    #[cfg(feature = "local-npu")]
    fn pump_autotag(&mut self, ctx: &egui::Context, host: &Host) {
        if self.autotag_pending.is_some() || self.autotag_rx.is_some() {
            return;
        }
        // Don't contend with CLIP embed for the HTP.
        if self.clipemb_pending.is_some() || self.clipemb_rx.is_some() {
            return;
        }
        let idle = self.auto_tag
            && self.wd14_pack.is_some()
            && matches!(self.conn, Conn::Connected)
            && !self.running
            && !self.wd14_running
            && self.tag_index_loaded
            && !self.gallery.is_empty();
        if !idle {
            if self.tag_index_dirty > 0 {
                self.save_tag_index(host);
            }
            return;
        }
        let next = self.gallery.iter().find_map(|it| {
            if it.is_video {
                return None;
            }
            let key = it.key();
            if self.tag_index.contains(&key) || self.autotag_failed.contains(&key) {
                return None;
            }
            Some((key, it.subfolder.clone(), it.filename.clone()))
        });
        if let Some((key, subfolder, filename)) = next {
            let cache_dir = self.ensure_full_cache_root(host).map(|s| s.to_string());
            // Prefer disk bytes so we skip the network when prefetch already won.
            if let Some(root) = cache_dir.as_ref()
                && let Some(bytes) = gallery::read_full_cache(root, &key)
            {
                self.autotag_pending = None;
                self.autotag_run(ctx, host, key, bytes);
                return;
            }
            self.autotag_pending = Some(key);
            self.engine.as_ref().unwrap().fetch_full(subfolder, filename, cache_dir);
            ctx.request_repaint();
        } else if self.tag_index_dirty > 0 {
            self.save_tag_index(host);
        }
    }

    /// Embed the next un-indexed image for semantic search. Independent of Auto-tag.
    /// Prefers the loaded listing, then any keyed files already in the full-image cache.
    #[cfg(feature = "local-npu")]
    fn pump_clipemb(&mut self, ctx: &egui::Context, host: &Host) {
        if self.clipemb_pending.is_some() || self.clipemb_rx.is_some() {
            return;
        }
        // Don't contend with tagging for the HTP.
        if self.autotag_pending.is_some() || self.autotag_rx.is_some() {
            return;
        }
        if self.clip_pack.is_none() || !self.clip_index_loaded || self.running || self.wd14_running {
            if self.clip_index_dirty > 0 {
                self.save_clip_index(host);
            }
            return;
        }
        let cache_dir = self.ensure_full_cache_root(host).map(|s| s.to_string());
        let mut next = self.gallery.iter().find_map(|it| {
            if it.is_video {
                return None;
            }
            let key = it.key();
            if self.clip_index.contains(&key) || self.clipemb_failed.contains(&key) {
                return None;
            }
            Some((key, it.subfolder.clone(), it.filename.clone()))
        });
        if next.is_none()
            && let Some(root) = cache_dir.as_ref()
        {
            // Walking the cache dir reads every .key sidecar — off-limits per frame on FUSE, so
            // once a walk comes up empty, hold off before checking the dir again.
            let now = ctx.input(|i| i.time);
            if now >= self.clipemb_rescan_after {
                next = gallery::full_cache_keys(root).into_iter().find_map(|key| {
                    if self.clip_index.contains(&key) || self.clipemb_failed.contains(&key) {
                        return None;
                    }
                    let (subfolder, filename) = match key.rsplit_once('/') {
                        Some((s, f)) => (s.to_string(), f.to_string()),
                        None => (String::new(), key.clone()),
                    };
                    Some((key, subfolder, filename))
                });
                if next.is_none() {
                    self.clipemb_rescan_after = now + 10.0;
                }
            }
        }
        let Some((key, subfolder, filename)) = next else {
            // Still paging the gallery — keep the UI alive until the listing is complete.
            if matches!(self.conn, Conn::Connected)
                && self.gallery.len() < self.gallery_total as usize
                && self.gallery.len() < GALLERY_LOAD_ALL_CAP as usize
                && !self.gallery_loading
                && self.engine.is_some()
            {
                self.gallery_loading = true;
                self.engine.as_ref().unwrap().gallery_list(
                    self.gallery_gen,
                    self.gallery.len() as u64,
                    self.gallery_page_size(),
                    self.gallery_list_q(),
                    &self.gallery_view,
                );
            }
            if self.clip_index_dirty > 0 {
                self.save_clip_index(host);
            }
            return;
        };
        if let Some(root) = cache_dir.as_ref()
            && let Some(bytes) = gallery::read_full_cache(root, &key)
        {
            self.clipemb_run(ctx, host, key, bytes);
            return;
        }
        if !matches!(self.conn, Conn::Connected) || self.engine.is_none() {
            return;
        }
        self.clipemb_pending = Some(key);
        self.engine.as_ref().unwrap().fetch_full(subfolder, filename, cache_dir);
        ctx.request_repaint();
    }

    #[cfg(feature = "local-npu")]
    fn clipemb_run(&mut self, ctx: &egui::Context, host: &Host, key: String, bytes: Vec<u8>) {
        let (Some(lib_dir), Some(pack_dir)) = (host.native_lib_dir(), self.clip_pack.clone()) else {
            self.clipemb_failed.insert(key);
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.clipemb_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result =
                crate::local_engine::embed_clip(std::path::PathBuf::from(lib_dir), pack_dir, bytes);
            let _ = tx.send((key, result));
            ctx.request_repaint();
        });
    }

    /// Drain a finished embedding into the index; batch-save and mark failures.
    #[cfg(feature = "local-npu")]
    fn poll_clipemb(&mut self, host: &Host) {
        let Some(rx) = self.clipemb_rx.as_ref() else { return };
        match rx.try_recv() {
            Ok((key, Ok((emb, score)))) => {
                self.clipemb_rx = None;
                self.clip_index.insert(key.clone(), emb, score);
                self.clip_index_dirty += 1;
                // A newly indexed image may match a character — record high-confidence suggestions.
                self.suggest_for_new_key(&key);
                if self.clip_index_dirty >= Self::AUTOTAG_SAVE_EVERY {
                    self.save_clip_index(host);
                }
            }
            Ok((key, Err(e))) => {
                self.clipemb_rx = None;
                self.log.warn(format!("clip embed {}: {e}", elide(&key, 48)));
                self.clipemb_failed.insert(key);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.clipemb_rx = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Indexed images / listed images, for the gallery's auto-tag progress line.
    #[cfg(feature = "local-npu")]
    fn autotag_progress(&self) -> Option<(usize, usize)> {
        if !self.auto_tag || self.wd14_pack.is_none() {
            return None;
        }
        let mut listed = 0usize;
        let mut done = 0usize;
        for it in &self.gallery {
            if it.is_video {
                continue;
            }
            listed += 1;
            if self.tag_index.contains(&it.key()) {
                done += 1;
            }
        }
        (listed > 0 && done < listed).then_some((done, listed))
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

        // Rising edge of the soft keyboard; the shrunk viewport can drop the focused field below
        // the fold, so scroll it back into view (egui only auto-scrolls to the cursor on edits).
        let kb_open = host.keyboard_height() > 1.0;
        self.kb_open_edge = kb_open && !self.kb_was_open;
        self.kb_was_open = kb_open;

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
        #[cfg(feature = "local-npu")]
        #[cfg(feature = "local-npu")]
        #[cfg(feature = "local-npu")]
        self.poll_d3_anima();
        #[cfg(feature = "local-npu")]
        self.poll_wd14();
        #[cfg(feature = "local-npu")]
        self.poll_rewrite();
        #[cfg(feature = "local-npu")]
        self.poll_clip_search();
        let _ = self.ensure_full_cache_root(host);
        self.ensure_tag_index_warm(ui.ctx(), host);
        self.ensure_clip_index_warm(ui.ctx(), host);
        self.pump_full_cache(ui.ctx(), host);
        #[cfg(feature = "local-npu")]
        {
            self.poll_autotag(host);
            self.poll_clipemb(host);
            self.pump_autotag(ui.ctx(), host);
            self.pump_clipemb(ui.ctx(), host);
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

        // Inpaint takes over the whole screen, above the tabs, until closed.
        if self.inpaint.is_some() {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.inpaint_overlay(ui, host));
            return;
        }

        // Graph fullscreen: hide nav bar + progress, give the whole screen to graph_tab.
        if self.graph_fullscreen && self.tab == Tab::Graph {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.graph_tab(ui, host));
            self.app_picker_window(ui.ctx(), host);
            self.publish_window(ui.ctx(), host);
            self.queue_sheet_window(ui.ctx(), host);
            if self.running || self.queue_remaining > 0 {
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            }
            return;
        }
        // If fullscreen was active but the user navigated away, release the lock.
        if self.graph_fullscreen {
            self.exit_graph_fullscreen(host);
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
                let bar = ui
                    .add(
                        egui::ProgressBar::new(frac)
                            .desired_height(14.0)
                            .text(format!("{:.0}%  {label}", frac * 100.0))
                            .animate(true),
                    )
                    .interact(egui::Sense::click())
                    .on_hover_text("Tap to see the queue");
                if bar.clicked() {
                    self.queue_sheet_open = true;
                    self.queue_clear_arm = false;
                }
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
        self.gallery_pick_window(ui.ctx(), host);
        self.queue_sheet_window(ui.ctx(), host);

        // Keep the server-wide queue in view even when jobs were started on the website. Poll faster
        // while the queue sheet is open so per-row actions reflect quickly.
        if matches!(self.conn, Conn::Connected) {
            let now = ui.ctx().input(|i| i.time);
            let interval = if self.queue_sheet_open { 1.0 } else { 2.5 };
            if now - self.last_queue_poll > interval {
                self.last_queue_poll = now;
                self.engine.as_ref().unwrap().poll_queue();
            }
        }

        if self.running || self.queue_remaining > 0 || self.queue_sheet_open {
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
    let _ = menu_popup(
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
) -> egui::Response {
    menu_popup(
        ui,
        label,
        Some(min_size),
        egui::RectAlign::TOP_START,
        &[egui::RectAlign::TOP_END, egui::RectAlign::BOTTOM_START],
        content,
    )
}

/// Header menu: popup opens below the button, right-aligned so it grows left.
fn down_menu<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    content: impl FnOnce(&mut egui::Ui) -> R,
) {
    let _ = menu_popup(
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
) -> egui::Response {
    use egui::containers::menu::MenuConfig;
    let response = if let Some(size) = min_size {
        ui.add_sized(size, egui::Button::new(label.into()))
    } else {
        ui.add(egui::Button::new(label.into()))
    };
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
    response
}


/// Anchor a popup window to the center of the screen, above canvas overlays (minimap / FABs).
///
/// A top-anchored `egui::Window` can push its title bar above the app's content area — up under
/// the status-bar icons. Centering keeps every window fully inside the usable area, and it
/// re-centers above the keyboard when the content shrinks for the IME.
fn centered<'a>(ctx: &egui::Context, window: egui::Window<'a>) -> egui::Window<'a> {
    // Long values (paths, prompts, model names) otherwise grow a window past the viewport.
    let cap = (ctx.content_rect().width() - 24.0).max(240.0);
    window
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .order(egui::Order::Tooltip)
        .max_width(cap)
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
    let value = sanitize_ui_text(ui, value);
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
            Mode::Video => "Video",
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

/// Wrapped character-card fields for a collapsing details body.
fn character_meta_body(ui: &mut egui::Ui, card: &CharacterCard) {
    wrap_meta(ui, "Identity", &card.identity);
    if !card.triggers.trim().is_empty() {
        wrap_meta(ui, "Triggers", &card.triggers);
    }
    if !card.negatives.trim().is_empty() {
        wrap_meta(ui, "Negatives", &card.negatives);
    }
    if !card.loras.is_empty() {
        let names: Vec<String> = card
            .loras
            .iter()
            .map(|l| format!("{} @{:.2}", file_basename(&l.file), l.strength_model))
            .collect();
        wrap_meta(ui, "LoRAs", &names.join(", "));
    }
    if !card.checkpoint.trim().is_empty() {
        let sw = if card.switch_checkpoint { " (switch on apply)" } else { "" };
        wrap_meta(ui, "Checkpoint", &format!("{}{}", file_basename(&card.checkpoint), sw));
    }
    if !card.face_prompt.trim().is_empty() {
        wrap_meta(ui, "Face prompt", &card.face_prompt);
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

/// A model-only LoRA stack for one Wan expert: file combo + strength per row, add / remove.
/// Zero-strength rows stay listed (spare slots) but the graph builder skips them.
fn video_lora_list(
    ui: &mut egui::Ui,
    title: &str,
    list: &mut Vec<crate::types::ActiveLora>,
    installed: &[String],
    salt: &str,
) {
    section_title(ui, title);
    let mut remove: Option<usize> = None;
    for (i, lora) in list.iter_mut().enumerate() {
        ui.group(|ui| {
            combo_full(ui, &format!("{salt}_{i}"), &mut lora.file, installed);
            ui.horizontal(|ui| {
                ui.add(egui::Slider::new(&mut lora.strength_model, 0.0..=2.0).text("Model"));
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
            });
            if lora.strength_model == 0.0 || lora.file.trim().is_empty() {
                ui.weak("inert — spare slot");
            }
        });
    }
    if let Some(i) = remove {
        list.remove(i);
    }
    if ui.button(format!("{} LoRA", icons::ADD)).clicked() {
        list.push(crate::types::ActiveLora {
            file: String::new(),
            strength_model: 1.0,
            strength_clip: 1.0,
            injected: String::new(),
            model_only: true,
        });
    }
}

fn combo_full(ui: &mut egui::Ui, id: &str, current: &mut String, options: &[String]) {
    let w = ui.available_width().max(80.0);
    let selected = if current.is_empty() {
        "—".to_string()
    } else {
        elide(&sanitize_ui_text(ui, current), 48)
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(w)
        .show_ui(ui, |ui| {
            for opt in options.iter().take(300) {
                ui.selectable_value(current, opt.clone(), elide(&sanitize_ui_text(ui, opt), 56));
            }
        });
}

/// Underlined section heading.
fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.separator();
    ui.label(egui::RichText::new(title).strong().underline());
}

/// Lay out controls centered on the main axis (plain `horizontal` left-aligns in a wide parent).
fn centered_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(w, ui.spacing().interact_size.y + 4.0),
        egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Center),
        add,
    );
}

/// Underlined title with − / value / + (text field, not a drag slider).
fn stepper_u32(
    ui: &mut egui::Ui,
    title: &str,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    step: u32,
) {
    section_title(ui, title);
    centered_row(ui, |ui| {
        if ui.small_button("-").clicked() {
            *value = (*value).saturating_sub(step).max(*range.start());
        }
        let mut s = value.to_string();
        if ui
            .add(
                egui::TextEdit::singleline(&mut s)
                    .desired_width(52.0)
                    .horizontal_align(egui::Align::Center),
            )
            .changed()
            && let Ok(v) = s.parse::<u32>()
        {
            *value = v.clamp(*range.start(), *range.end());
        }
        if ui.small_button("+").clicked() {
            *value = (*value).saturating_add(step).min(*range.end());
        }
    });
}

fn stepper_f32(
    ui: &mut egui::Ui,
    title: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
) {
    section_title(ui, title);
    centered_row(ui, |ui| {
        if ui.small_button("-").clicked() {
            *value = (*value - step).max(*range.start());
        }
        let mut s = format!("{value:.2}");
        if ui
            .add(
                egui::TextEdit::singleline(&mut s)
                    .desired_width(52.0)
                    .horizontal_align(egui::Align::Center),
            )
            .changed()
            && let Ok(v) = s.parse::<f32>()
        {
            *value = v.clamp(*range.start(), *range.end());
        }
        if ui.small_button("+").clicked() {
            *value = (*value + step).min(*range.end());
        }
    });
}

fn uint_text_edit(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
) {
    let mut s = value.to_string();
    let h = ui.spacing().interact_size.y;
    if ui
        .add_sized(
            egui::vec2(64.0, h),
            egui::TextEdit::singleline(&mut s)
                .id_salt(id)
                .horizontal_align(egui::Align::Center),
        )
        .changed()
        && let Ok(v) = s.parse::<u32>()
    {
        *value = v.clamp(*range.start(), *range.end());
    }
}

fn uint_text_edit_u64(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut u64,
    range: std::ops::RangeInclusive<u64>,
) {
    let mut s = value.to_string();
    if ui
        .add(
            egui::TextEdit::singleline(&mut s)
                .id_salt(id)
                .desired_width(120.0)
                .horizontal_align(egui::Align::Center),
        )
        .changed()
        && let Ok(v) = s.parse::<u64>()
    {
        *value = v.clamp(*range.start(), *range.end());
    }
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

/// Compact tag-count label: `1.2M`, `87k`, or the bare number.
fn fmt_count(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Weight at 2 decimals with trailing zeros / dot trimmed.
fn fmt_weight(w: f32) -> String {
    let s = format!("{w:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
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

