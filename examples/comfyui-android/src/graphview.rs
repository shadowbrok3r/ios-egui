//! Graph-tab presentation layer over [`ComfyUiNodeGraph`]: renders the snarl canvas with a stable
//! widget id, a view-only lock mode, programmatic view commands (fit-all / center-on-point), a
//! minimap overlay, and the node properties editor shared with the Properties tab.

use std::collections::{HashMap, HashSet};

use egui::emath::TSTransform;
use egui_snarl::ui::{NodeLayout, 
    BackgroundPattern, PinInfo, PinPlacement, SnarlStyle, SnarlViewer, SnarlWidget, WireStyle,
};
use egui_snarl::{InPin, InPinId, NodeId, OutPin, OutPinId, Snarl};
use rucomfyui_node_graph::ComfyUiNodeGraph;
use rucomfyui_node_graph::internal::{FlowInput, FlowNodeData, FlowValueType, FlowViewer};

pub const MIN_SCALE: f32 = 0.05;
pub const MAX_SCALE: f32 = 2.5;
/// Assumed node extent until its real size is measured on screen.
const NOMINAL_NODE: egui::Vec2 = egui::vec2(180.0, 100.0);
/// Rough half-node offset so centering lands mid-node rather than on its corner.
const NODE_CENTER_OFFSET: egui::Vec2 = egui::vec2(90.0, 50.0);

/// A one-shot view command applied on the next rendered frame.
pub enum ViewCmd {
    FitAll,
    Center(egui::Pos2),
    /// Center on a point and zoom into a comfortable range (for auto-follow).
    Focus(egui::Pos2),
}

/// Result of a long-press on the graph canvas.
pub enum LongPress {
    /// Empty canvas — open Add node / paste menu at this graph-space point.
    Canvas(egui::Pos2),
    /// Held on a node — open the node action menu (bypass / auto-wire).
    Node(NodeId),
}

/// A model-file combo selection that changed this frame (canvas or properties).
///
/// `lora_name` drives recommended strengths and trigger injection; `ckpt_name` / `unet_name` drive
/// the checkpoint's recommended sampler settings, so swapping a model on the canvas re-seeds the
/// graph the way it already re-seeds the Create tab.
pub struct ModelPick {
    pub node: NodeId,
    /// Which input changed — one of [`PICK_INPUTS`].
    pub input: &'static str,
    pub file: String,
}

/// The file inputs a [`ModelPick`] is raised for. A node carries at most one of them, so the first
/// match wins and the order is only a probe order.
pub const PICK_INPUTS: [&str; 3] = ["lora_name", "ckpt_name", "unet_name"];

/// View state and overlays for the graph canvas.
pub struct GraphView {
    pub locked: bool,
    /// Per-tab snarl widget id (shared ids leak draw-order / node state across tabs).
    widget_id: egui::Id,
    cmd: Option<ViewCmd>,
    arrange_queued: bool,
    /// Frames spent waiting for measured node sizes before a queued arrange runs.
    arrange_wait: u8,
    /// Frames to keep reporting a layout as in-flight after it runs, so undo does not record
    /// the settling positions as a user edit.
    arrange_settling: u8,
    /// Auto-arrange requested by a load, waiting for the canvas to paint before running.
    needs_auto_arrange: bool,
    sizes: HashMap<NodeId, egui::Vec2>,
    /// Last frame's node rects in graph space, for the backdrop blur behind each node. Kept on the
    /// view rather than rebuilt per frame because the glass has to be painted before the nodes that
    /// define it — see `Wrapper::frost_nodes`.
    node_rects: Vec<egui::Rect>,
    /// `sizes` as of the previous frame: a queued arrange waits for two frames to agree, since a
    /// node's first measure is taken before egui has settled its content.
    prev_sizes: HashMap<NodeId, egui::Vec2>,
    /// `sizes` the last arrange actually laid out from, for the load-time refine check.
    arranged_sizes: HashMap<NodeId, egui::Vec2>,
    /// Re-arranges still allowed if measures move after a *load* arrange (never after a manual
    /// one — re-arranging under the user because a node grew would yank the canvas away).
    refine_left: u8,
    to_global: TSTransform,
    pub view_rect: egui::Rect,
    /// Where the in-progress drag started (classified once at press). Header drags move a node;
    /// body drags pan (and the node move is vetoed — see [`Self::drag_gate`]); canvas/pin drags
    /// are left to snarl. In locked mode any node drag pans instead.
    drag_kind: DragKind,
    /// A press held still: (start time, screen origin, node under press if any).
    press: Option<(f64, egui::Pos2, Option<NodeId>)>,
    /// One long-press has already fired for the current press.
    long_fired: bool,
    /// Long-press this frame (canvas add-menu or node menu).
    long_press: Option<LongPress>,
    /// `lora_name` picks this frame (recommended strengths applied by the app).
    model_picks: Vec<ModelPick>,
    /// A file-selector widget was tapped on the canvas this frame; the app opens its picker.
    file_pick: Option<NodeId>,
    /// Lay the graph out top-to-bottom rather than left-to-right, chosen from the canvas shape so
    /// the flow runs along the screen's LONG axis (see [`arrange`]).
    vertical: bool,
    /// Thumbnails of the input files this graph's file-selector nodes point at, keyed by filename.
    /// Supplied by the app each frame (it owns the thumb cache and the fetches); the canvas paints
    /// them onto the nodes so a `LoadImage` shows its picture instead of just a filename.
    input_thumbs: HashMap<String, egui::TextureHandle>,
    /// Screen-space pan queued for the next `show` to lift a focused node field above the keyboard.
    pending_pan: egui::Vec2,
}

/// File extensions the node file pickers treat as still images / as videos. `gif` counts as a
/// video: VHS accepts animated gifs as clips, and a poster is the useful preview either way.
pub const PICK_IMAGE_EXT: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "avif"];
pub const PICK_VIDEO_EXT: [&str; 7] = ["mp4", "webm", "mkv", "mov", "avi", "m4v", "gif"];

/// Lowercase extension of a listed file (`"clipspace/foo.png [input]"` → `"png"`).
pub fn pick_ext(name: &str) -> String {
    let head = name.split_whitespace().next().unwrap_or(name);
    head.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

pub fn is_pick_video(name: &str) -> bool {
    PICK_VIDEO_EXT.contains(&pick_ext(name).as_str())
}

/// MIME type for a file we are handing to Android, keyed off its extension. This is what decides
/// which MediaStore collection a save lands in, so a clip that guesses `image/png` is filed under
/// Images and never reaches the phone's gallery. Lives next to the extension tables above so the
/// two can't drift on what counts as a video.
pub fn media_mime(name: &str) -> &'static str {
    match pick_ext(name).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        // A gif is a still to MediaStore (Images.Media indexes it) even though the pickers offer it
        // as a clip — filing it under Video.Media would hide it from the phone's photo grid.
        "gif" => "image/gif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        _ => "image/png",
    }
}

fn is_pick_media(name: &str) -> bool {
    let ext = pick_ext(name);
    PICK_IMAGE_EXT.contains(&ext.as_str()) || PICK_VIDEO_EXT.contains(&ext.as_str())
}

/// A node input that selects a file from the server's `input` directory — `LoadImage.image`,
/// `VHS_LoadVideo.video`, and every other upload-style widget. Found by content rather than by
/// class name so custom loaders come along for free.
#[derive(Clone)]
pub struct MediaInput {
    pub idx: usize,
    pub name: String,
    pub options: Vec<String>,
    pub selected: String,
    /// The picker should offer videos rather than stills.
    pub video: bool,
}

/// Is this input a file selector, and does it hold videos? Allocation-free, because the canvas asks
/// this for every input row of every node, every frame — [`media_input_of`] clones the whole option
/// list, which for a `LoadImage` on a busy server is hundreds of strings.
fn media_input_kind(inp: &FlowInput) -> Option<bool> {
    let FlowValueType::Array { options, .. } = &inp.value else { return None };
    let named = matches!(
        inp.name.to_ascii_lowercase().as_str(),
        "image" | "video" | "file" | "media" | "images" | "video_file" | "image_file"
    );
    // A handful of options is enough to tell a file list from a sampler list; a widget with a
    // media-ish name qualifies even while the server has nothing uploaded yet.
    let sample = options.iter().take(12).count();
    let media_options = sample > 0 && options.iter().take(12).filter(|o| is_pick_media(o)).count() * 2 > sample;
    if !media_options && !(named && options.is_empty()) {
        return None;
    }
    // Kind from the files themselves; the widget name decides an empty list.
    Some(if options.is_empty() {
        inp.name.to_ascii_lowercase().contains("video")
    } else {
        options.iter().filter(|o| is_pick_video(o)).count() * 2 > options.len()
    })
}

/// Index of the node's file-selector input, if it has one. Allocation-free — prefer this on the
/// per-frame canvas paths and read the input out of `data.inputs` when you need its value.
pub fn media_input_idx(data: &FlowNodeData) -> Option<usize> {
    data.inputs.iter().position(|inp| media_input_kind(inp).is_some())
}

/// The file the node's selector currently names, plus whether that selector takes videos.
/// Allocation-free; the per-frame canvas and thumbnail paths use this.
pub fn media_selected(data: &FlowNodeData) -> Option<(&str, bool)> {
    let inp = data.inputs.get(media_input_idx(data)?)?;
    let video = media_input_kind(inp)?;
    match &inp.value {
        FlowValueType::Array { selected, .. } => Some((selected.as_str(), video)),
        _ => None,
    }
}

/// The node's file selector, if it has one: an enum input whose options read as media filenames,
/// or (before any file exists on the server) one named like an upload widget. Clones the option
/// list for the picker UI; on per-frame paths use [`media_input_idx`] instead.
pub fn media_input_of(data: &FlowNodeData) -> Option<MediaInput> {
    let idx = media_input_idx(data)?;
    let inp = data.inputs.get(idx)?;
    let video = media_input_kind(inp)?;
    let FlowValueType::Array { options, selected } = &inp.value else { return None };
    Some(MediaInput {
        idx,
        name: inp.name.clone(),
        options: options.clone(),
        selected: selected.clone(),
        video,
    })
}

/// Whether this input is a ComfyUI seed widget that carries `control_after_generate`.
pub fn is_seed_widget(input: &FlowInput) -> bool {
    (input.name == "seed" || input.name == "noise_seed")
        && matches!(
            input.value,
            FlowValueType::UnsignedInt { .. } | FlowValueType::SignedInt { .. }
        )
}

/// Write a fresh random value into a seed widget.
pub fn roll_seed_value(input: &mut FlowInput) {
    let seed = random_u64();
    match &mut input.value {
        FlowValueType::UnsignedInt { value, .. } => *value = seed,
        FlowValueType::SignedInt { value, .. } => *value = seed as i64,
        _ => {}
    }
}

fn random_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Apply `seed_randomize` flags keyed by workflow node id onto snarl nodes after an API load.
/// Snarl ids are assigned in the same topological insert order as `load_api_workflow`.
pub fn apply_seed_randomize_from_workflow(
    snarl: &Snarl<FlowNodeData>,
    workflow: &rucomfyui::Workflow,
    from_ui: &std::collections::BTreeMap<(u64, String), bool>,
    out: &mut HashMap<(NodeId, String), bool>,
) {
    out.clear();
    if from_ui.is_empty() {
        return;
    }
    let layers = workflow.topological_sort_with_depth();
    let mut wids = Vec::new();
    for layer in layers {
        for wid in layer {
            wids.push(wid.0 as u64);
        }
    }
    let mut nids: Vec<NodeId> = snarl.node_ids().map(|(id, _)| id).collect();
    nids.sort_by_key(|id| id.0);
    if wids.len() != nids.len() {
        return;
    }
    for (wid, nid) in wids.into_iter().zip(nids) {
        for ((id, name), &rand) in from_ui {
            if *id == wid {
                out.insert((nid, name.clone()), rand);
            }
        }
    }
}

/// Re-key undeclared widget values from UI node ids onto the snarl ids `load_api_workflow` gave
/// them, recording each node's class so a later id reuse can't attach them to a different node.
pub fn apply_extra_widgets_from_workflow(
    snarl: &Snarl<FlowNodeData>,
    workflow: &rucomfyui::Workflow,
    from_ui: &std::collections::BTreeMap<(u64, String), serde_json::Value>,
    out: &mut HashMap<(NodeId, String), (String, serde_json::Value)>,
) {
    out.clear();
    if from_ui.is_empty() {
        return;
    }
    let wids: Vec<rucomfyui::WorkflowNodeId> =
        workflow.topological_sort_with_depth().into_iter().flatten().collect();
    let mut nids: Vec<NodeId> = snarl.node_ids().map(|(id, _)| id).collect();
    nids.sort_by_key(|id| id.0);
    if wids.len() != nids.len() {
        return;
    }
    for (wid, nid) in wids.into_iter().zip(nids) {
        let Some(class) = workflow.0.get(&wid).map(|n| n.class_type.clone()) else { continue };
        for ((id, name), v) in from_ui {
            if *id == wid.0 as u64 {
                out.insert((nid, name.clone()), (class.clone(), v.clone()));
            }
        }
    }
}

/// Set every seed / noise_seed widget on the graph to the same randomize flag.
pub fn set_all_seed_randomize(
    snarl: &Snarl<FlowNodeData>,
    out: &mut HashMap<(NodeId, String), bool>,
    randomize: bool,
) {
    out.clear();
    for (nid, data) in snarl.node_ids() {
        for input in &data.inputs {
            if is_seed_widget(input) {
                out.insert((nid, input.name.clone()), randomize);
            }
        }
    }
}

/// Roll seeds marked randomize in place (ComfyUI client-side control_after_generate).
pub fn apply_pending_seed_rolls(
    snarl: &mut Snarl<FlowNodeData>,
    seed_randomize: &HashMap<(NodeId, String), bool>,
) {
    for ((nid, name), &rand) in seed_randomize {
        if !rand {
            continue;
        }
        let Some(data) = snarl.get_node_mut(*nid) else { continue };
        if let Some(input) = data.inputs.iter_mut().find(|i| i.name == *name) {
            roll_seed_value(input);
        }
    }
}

/// Re-roll EVERY seed widget in the graph, returning how many changed. The duplicate-run guard's
/// "New seed & run": an unchanged fixed-seed workflow is a whole-graph server cache replay, so
/// every seed moves — and the widgets update so the canvas shows what actually ran.
pub fn roll_all_seeds(snarl: &mut Snarl<FlowNodeData>) -> usize {
    let ids: Vec<NodeId> = snarl.node_ids().map(|(id, _)| id).collect();
    let mut rolled = 0;
    for id in ids {
        let Some(data) = snarl.get_node_mut(id) else { continue };
        for input in data.inputs.iter_mut() {
            if is_seed_widget(input) {
                roll_seed_value(input);
                rolled += 1;
            }
        }
    }
    rolled
}

impl Default for GraphView {
    fn default() -> Self {
        Self::new(0)
    }
}

impl GraphView {
    pub fn new(doc_id: u64) -> Self {
        Self {
            locked: false,
            widget_id: egui::Id::new(("comfy-graph-canvas", doc_id)),
            cmd: None,
            arrange_queued: false,
            arrange_wait: 0,
            arrange_settling: 0,
            needs_auto_arrange: false,
            sizes: HashMap::new(),
            node_rects: Vec::new(),
            prev_sizes: HashMap::new(),
            arranged_sizes: HashMap::new(),
            refine_left: 0,
            to_global: TSTransform::IDENTITY,
            view_rect: egui::Rect::ZERO,
            drag_kind: DragKind::None,
            press: None,
            long_fired: false,
            long_press: None,
            model_picks: Vec::new(),
            file_pick: None,
            vertical: false,
            input_thumbs: HashMap::new(),
            pending_pan: egui::Vec2::ZERO,
        }
    }

    /// Forget cached geometry and pending commands (snarl node ids restart when a new graph is
    /// loaded, so stale sizes would attach to the wrong nodes).
    pub fn reset(&mut self) {
        self.cmd = None;
        self.arrange_queued = false;
        self.arrange_wait = 0;
        self.arrange_settling = 0;
        self.needs_auto_arrange = false;
        self.sizes.clear();
        self.prev_sizes.clear();
        self.arranged_sizes.clear();
        self.refine_left = 0;
        self.press = None;
        self.long_fired = false;
        self.long_press = None;
        self.model_picks.clear();
        self.file_pick = None;
        self.input_thumbs.clear();
        self.pending_pan = egui::Vec2::ZERO;
    }

    pub fn request_fit(&mut self) {
        self.cmd = Some(ViewCmd::FitAll);
    }

    /// Queue a compact layout once measured sizes are available (or after a short wait).
    /// An auto-layout is queued, or has just run. Undo treats the whole settling layout as part
    /// of whatever asked for it, rather than a second step to undo separately. The grace frames
    /// matter because `arrange_now` clears the queue flag *before* it moves anything, so without
    /// them the move itself looks like a user edit.
    pub fn arrange_pending(&self) -> bool {
        self.needs_auto_arrange || self.arrange_queued || self.arrange_settling > 0
    }

    /// Defer auto-arrange until the canvas paints (Create sync / background loads never call `show`).
    pub fn mark_needs_auto_arrange(&mut self) {
        self.needs_auto_arrange = true;
        self.arrange_queued = false;
        self.arrange_wait = 0;
        // One correction pass is allowed if the measures the load arrange used turn out to have
        // been taken before the canvas settled.
        self.refine_left = 1;
        self.cmd = Some(ViewCmd::FitAll);
    }

    pub fn request_arrange(&mut self) {
        self.needs_auto_arrange = false;
        self.arrange_queued = true;
        self.arrange_wait = 0;
        self.cmd = Some(ViewCmd::FitAll);
    }

    /// Arrange immediately. Uses measured sizes when present, else [`NOMINAL_NODE`].
    /// Does not invent size cache entries — placeholders would fake "measured" and skip refine.
    pub fn arrange_now(&mut self, snarl: &mut Snarl<FlowNodeData>) {
        self.needs_auto_arrange = false;
        self.arrange_queued = false;
        self.arrange_wait = 0;
        self.arrange_settling = 3;
        if snarl.nodes_pos_ids().next().is_none() {
            return;
        }
        arrange(snarl, &self.sizes, self.vertical);
        self.arranged_sizes = self.sizes.clone();
        self.cmd = Some(ViewCmd::FitAll);
    }

    /// Load-time layout: nominal arrange + fit so every node paints, then queue a refine pass
    /// once `final_node_rect` has filled real sizes.
    pub fn arrange_on_load(&mut self, snarl: &mut Snarl<FlowNodeData>) {
        log::info!("graphview: auto-arrange on load ({} nodes)", snarl.nodes_pos_ids().count());
        // Exactly what the manual Auto-arrange button does: fit, wait for measured sizes, then
        // arrange. A nominal-size pre-arrange here spread nodes so wide that off-screen ones
        // never got measured and the refine settled on placeholder sizes.
        self.request_arrange();
    }

    /// Pan the scene up so a focused in-node TextEdit clears the keyboard. `avoid_bottom` is the
    /// screen y below which content is hidden; queues a one-frame pan applied by the next `show`.
    pub fn keep_focus_above(&mut self, ctx: &egui::Context, avoid_bottom: f32) {
        if let Some(id) = ctx.memory(|m| m.focused())
            && let Some(resp) = ctx.read_response(id)
        {
            // Node widgets live in the snarl Scene's local space; map to screen before comparing,
            // and clamp so a stale rect can never fling the canvas off-screen.
            let screen = self.to_global * resp.rect;
            let over = (screen.bottom() - avoid_bottom).min(600.0);
            if over > 0.0 {
                self.pending_pan.y -= over;
                ctx.request_repaint();
            }
        }
    }

    /// Center the view on a node position (graph space).
    pub fn center_on(&mut self, node_pos: egui::Pos2) {
        self.cmd = Some(ViewCmd::Center(node_pos + NODE_CENTER_OFFSET));
    }

    /// Center on a node and zoom into a readable range — the auto-follow motion.
    pub fn focus_on(&mut self, node_pos: egui::Pos2) {
        self.cmd = Some(ViewCmd::Focus(node_pos + NODE_CENTER_OFFSET));
    }

    /// The graph-space point currently at the middle of the canvas.
    pub fn center_in_graph(&self) -> Option<egui::Pos2> {
        (self.view_rect.width() > 0.0)
            .then(|| self.to_global.inverse() * self.view_rect.center())
    }

    /// `lora_name` selections made while rendering the canvas this frame.
    pub fn take_model_picks(&mut self) -> Vec<ModelPick> {
        std::mem::take(&mut self.model_picks)
    }

    /// The node whose file selector was tapped on the canvas this frame.
    pub fn take_file_pick(&mut self) -> Option<NodeId> {
        self.file_pick.take()
    }

    /// Is the graph currently laid out top-to-bottom? (Portrait canvases; see [`arrange`].)
    pub fn flow_vertical(&self) -> bool {
        self.vertical
    }

    /// Hand the canvas this frame's input-file thumbnails (see [`Self::input_thumbs`]), keeping
    /// the previous picture for any name this frame's map has no entry for.
    ///
    /// The caller builds that map from the thumbnail cache, so a name drops out of it whenever the
    /// cache misses — an eviction, or the frames between a pick and its download landing. Assigning
    /// wholesale also dropped the last outstanding handle, so egui freed the texture and the node
    /// footer went blank until the refetch returned. Names no node points at any more are the only
    /// ones removed: `wanted` is what the walk actually looked for, hit or miss.
    pub fn set_input_thumbs(
        &mut self,
        thumbs: HashMap<String, egui::TextureHandle>,
        wanted: &HashSet<String>,
    ) {
        self.input_thumbs.retain(|name, _| wanted.contains(name));
        self.input_thumbs.extend(thumbs);
    }

    /// Render the canvas (with lock gating and pending view commands), then the minimap overlay.
    /// Returns the node tapped this frame, if any — snarl itself only selects on shift-click,
    /// which doesn't exist on touch.
    ///
    /// `lora_files` fills empty `lora_name` combos (Create-tab union across loader classes).
    #[must_use]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        g: &mut ComfyUiNodeGraph,
        executing: Option<NodeId>,
        focus: Option<NodeId>,
        bypassed: &HashSet<NodeId>,
        lora_files: &[String],
        seed_randomize: &mut HashMap<(NodeId, String), bool>,
    ) -> Option<NodeId> {
        self.sizes.retain(|id, _| g.snarl.get_node(*id).is_some());
        // Canvas shape decides the flow axis, before any queued arrange runs this frame. The 1.15
        // factor keeps a near-square canvas from flipping the whole layout back and forth.
        let canvas = ui.available_rect_before_wrap();
        if canvas.is_finite() && canvas.width() > 0.0 {
            self.view_rect = canvas;
        }
        self.vertical = self.view_rect.height() > self.view_rect.width() * 1.15;
        self.arrange_settling = self.arrange_settling.saturating_sub(1);
        self.model_picks.clear();
        self.file_pick = None;
        // Keep file combos populated even when a single loader class shipped an empty list.
        for data in g.snarl.nodes_mut() {
            ensure_file_combos(data, &g.object_info, lora_files);
        }

        // Background loads only mark the flag — arrange once the canvas is actually painting.
        if self.needs_auto_arrange {
            self.arrange_on_load(&mut g.snarl);
        }

        let ids: Vec<NodeId> = g.snarl.nodes_pos_ids().map(|(id, _, _)| id).collect();
        // A node's size settles over a frame or two (egui discards and re-runs the pass that
        // initialises snarl's node state). Arranging from the first measure laid the canvas out
        // from numbers that were about to change, so require two frames to agree.
        let settled = sizes_agree(&self.sizes, &self.prev_sizes);
        if self.arrange_queued {
            if ids.is_empty() {
                self.arrange_queued = false;
                self.arrange_wait = 0;
            } else {
                let measured = ids.iter().filter(|id| self.sizes.contains_key(id)).count();
                let ready = measured == ids.len() && settled;
                self.arrange_wait = self.arrange_wait.saturating_add(1);
                // Prefer settled measures; after a few FitAll frames, arrange with what we have so
                // a never-drawn node cannot stall the queue forever.
                if ready || self.arrange_wait >= 6 {
                    log::info!(
                        "graphview: queued arrange firing ({} nodes, {measured} measured, settled={settled}, widest={:.0}, wait={}, locked={})",
                        ids.len(),
                        self.sizes.values().map(|s| s.x).fold(0.0f32, f32::max),
                        self.arrange_wait,
                        self.locked
                    );
                    self.arrange_now(&mut g.snarl);
                } else {
                    self.cmd = Some(ViewCmd::FitAll);
                    ui.ctx().request_repaint();
                }
            }
        } else if self.refine_left > 0 && self.arrange_settling == 0 && !ids.is_empty() && settled {
            // The load arrange laid out from sizes that have since moved (a node measured before
            // its content settled, or one that had no measure at all). Re-run it once, now.
            let moved = ids.iter().any(|id| {
                let now = self.sizes.get(id);
                match (now, self.arranged_sizes.get(id)) {
                    (Some(now), Some(then)) => (*now - *then).abs().max_elem() > 8.0,
                    (Some(_), None) => true,
                    _ => false,
                }
            });
            if moved {
                self.refine_left -= 1;
                log::info!("graphview: node sizes moved after the load arrange — refining");
                self.request_arrange();
            } else {
                self.refine_left = 0;
            }
        }
        self.prev_sizes.clone_from(&self.sizes);

        // The snarl response rect is unbounded (scene ui); measure the canvas region ourselves.
        // (Before drag_gate: classify_press needs an up-to-date view_rect.)
        let canvas = ui.available_rect_before_wrap();
        if canvas.is_finite() && canvas.width() > 0.0 {
            self.view_rect = canvas;
        }
        // Header-only node dragging: pan from a body drag, and veto any node move that didn't
        // start on a title bar. Snapshot the positions to restore after show() (same mechanism as
        // lock). Snapshot after arrange so a fresh layout is never undone.
        let (pan, veto_move) = self.drag_gate(ui.ctx(), &g.snarl);
        let pan = pan + std::mem::take(&mut self.pending_pan);
        let saved: Option<Vec<(NodeId, egui::Pos2)>> = (self.locked || veto_move)
            .then(|| g.snarl.nodes_pos_ids().map(|(id, pos, _)| (id, pos)).collect());
        let cmd = if self.view_rect.width() > 0.0 { self.cmd.take() } else { None };
        let mut viewer = Wrapper {
            inner: FlowViewer { user_state: &mut g.user_state, object_info: &g.object_info },
            locked: self.locked,
            focus,
            bypassed,
            model_picks: &mut self.model_picks,
            file_pick: &mut self.file_pick,
            input_thumbs: &self.input_thumbs,
            seed_randomize,
            cmd,
            pan,
            bounds: bounds(&g.snarl, &self.sizes),
            ui_rect: self.view_rect,
            sizes: &mut self.sizes,
            out_transform: &mut self.to_global,
            node_rects: &mut self.node_rects,
        };
        SnarlWidget::new()
            .id(self.widget_id)
            .style(style())
            .show(&mut g.snarl, &mut viewer, ui);

        if let Some(saved) = saved {
            for (id, pos) in saved {
                if let Some(info) = g.snarl.get_node_info_mut(id) {
                    info.pos = pos;
                }
            }
        }

        let tapped = self.tapped_node(ui.ctx(), &g.snarl);
        self.long_press = self.detect_long_press(ui.ctx(), &g.snarl);
        self.minimap(ui, &g.snarl, executing, focus);
        self.lock_button(ui);
        tapped
    }

    /// A long-press this frame. Taken so it fires once.
    pub fn take_long_press(&mut self) -> Option<LongPress> {
        self.long_press.take()
    }

    /// Detect a finger held still for ~0.5s: on a node → bypass toggle; on empty canvas → add menu.
    /// Locked mode uses the same drag to pan, so long-press is disabled while locked.
    fn detect_long_press(
        &mut self,
        ctx: &egui::Context,
        snarl: &Snarl<FlowNodeData>,
    ) -> Option<LongPress> {
        if self.locked {
            self.press = None;
            self.long_fired = false;
            return None;
        }
        let (down, pos, time, dragging) = ctx.input(|i| {
            (
                i.pointer.any_down(),
                i.pointer.interact_pos(),
                i.time,
                i.pointer.is_decidedly_dragging(),
            )
        });
        if !down {
            self.press = None;
            self.long_fired = false;
            return None;
        }
        let Some(pos) = pos else { return None };
        if !self.view_rect.contains(pos)
            || ctx.layer_id_at(pos).is_some_and(|l| l.order != egui::Order::Background)
        {
            self.press = None;
            return None;
        }
        let under = self.node_at(ctx, pos, snarl);
        match self.press {
            None => {
                self.press = Some((time, pos, under));
                None
            }
            Some((start, origin, node)) => {
                if dragging || (origin - pos).length() > 12.0 {
                    self.press = None;
                    return None;
                }
                // Finger slid onto a different target — cancel.
                if under != node {
                    self.press = None;
                    return None;
                }
                if !self.long_fired && time - start > 0.5 {
                    self.long_fired = true;
                    ctx.request_repaint();
                    return Some(match node {
                        Some(id) => LongPress::Node(id),
                        None => LongPress::Canvas(self.to_global.inverse() * pos),
                    });
                }
                ctx.request_repaint();
                None
            }
        }
    }

    /// The node under a tap released this frame. Taps that land on higher layers (windows, the
    /// minimap, the lock button) don't count.
    fn tapped_node(&self, ctx: &egui::Context, snarl: &Snarl<FlowNodeData>) -> Option<NodeId> {
        let pos = ctx.input(|i| {
            (i.pointer.any_click() && !i.pointer.is_decidedly_dragging())
                .then(|| i.pointer.interact_pos())
                .flatten()
        })?;
        self.node_at(ctx, pos, snarl)
    }

    /// The node under a screen position, ignoring positions owned by an overlay.
    fn node_at(
        &self,
        ctx: &egui::Context,
        pos: egui::Pos2,
        snarl: &Snarl<FlowNodeData>,
    ) -> Option<NodeId> {
        if !self.view_rect.contains(pos) {
            return None;
        }
        if ctx.layer_id_at(pos).is_some_and(|l| l.order != egui::Order::Background) {
            return None;
        }
        let graph_pos = self.to_global.inverse() * pos;
        for (id, node_pos, _) in snarl.nodes_pos_ids() {
            let size = self.sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
            if egui::Rect::from_min_size(node_pos, size).contains(graph_pos) {
                return Some(id);
            }
        }
        None
    }

    /// Classify a drag by where it began and return `(pan, veto_move)`:
    /// - **Header** drag → the node moves (snarl handles it): no pan, no veto.
    /// - **Body** drag (node, below the title bar) → pan the canvas and veto the node move, so a
    ///   finger that misses a pin scrolls the view instead of dragging the node around.
    /// - **Canvas / pin** drag → left to snarl (empty-canvas pan or wire drag): no pan, no veto.
    /// - **Locked**: any node drag pans; every node move is vetoed by the caller's snapshot.
    ///
    /// The classification is captured at press and held for the whole drag (press_origin is fixed),
    /// so a drag that starts in the body can't "slide" into moving a node.
    fn drag_gate(&mut self, ctx: &egui::Context, snarl: &Snarl<FlowNodeData>) -> (egui::Vec2, bool) {
        let (pressed, down, press_origin, delta, zooming) = ctx.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.any_down(),
                i.pointer.press_origin(),
                i.pointer.delta(),
                (i.zoom_delta() - 1.0).abs() > f32::EPSILON || i.multi_touch().is_some(),
            )
        });
        if pressed {
            self.drag_kind =
                press_origin.map(|p| self.classify_press(ctx, p, snarl)).unwrap_or(DragKind::None);
        }
        if !down {
            self.drag_kind = DragKind::None;
        }
        // During a pinch, snarl zooms the scene and the primary pointer still reports a delta —
        // don't add a body-pan on top of that (it drifts the view while zooming).
        let d = if delta.is_finite() && !zooming { delta } else { egui::Vec2::ZERO };
        match (self.locked, self.drag_kind) {
            // Locked: a drag from any node pans; the snapshot/restore freezes every position.
            (true, DragKind::Header | DragKind::Body) => (d, false),
            (true, _) => (egui::Vec2::ZERO, false),
            // Unlocked header-only dragging: body drag pans + vetoes the node move.
            (false, DragKind::Body) => (d, true),
            (false, _) => (egui::Vec2::ZERO, false),
        }
    }

    /// Which region a screen-space press landed on, for [`Self::drag_gate`].
    fn classify_press(
        &self,
        ctx: &egui::Context,
        pos: egui::Pos2,
        snarl: &Snarl<FlowNodeData>,
    ) -> DragKind {
        if !self.view_rect.contains(pos)
            || ctx.layer_id_at(pos).is_some_and(|l| l.order != egui::Order::Background)
        {
            return DragKind::None;
        }
        let gp = self.to_global.inverse() * pos;
        // Any overlapping node's HEADER wins over any body: this classify order is slab order,
        // not snarl's draw order, so it can't tell which overlapping node snarl will actually drag
        // — but biasing to "header" guarantees a title-bar grab can always move a node, and only
        // ever mis-allows a move (never wrongly blocks one) where nodes overlap.
        let mut on_body = false;
        for (id, node_pos, _) in snarl.nodes_pos_ids() {
            let size = self.sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
            // Shift by the frame margin so the hit rect matches the drawn frame; with pins placed
            // outside, this keeps the pin dots out of the body region (a wire-start there must not
            // be read as a body-pan).
            let frame =
                egui::Rect::from_min_size(node_pos - egui::vec2(FRAME_MARGIN, FRAME_MARGIN), size);
            if !frame.contains(gp) {
                continue;
            }
            let header =
                egui::Rect::from_min_size(frame.min, egui::vec2(size.x, NODE_HEADER_H.min(size.y)));
            if header.contains(gp) {
                return DragKind::Header;
            }
            on_body = true;
        }
        if on_body { DragKind::Body } else { DragKind::Canvas }
    }

    /// Floating lock toggle under the queue FAB (bottom-right stack).
    fn lock_button(&mut self, ui: &mut egui::Ui) {
        let view = self.view_rect;
        if !view.is_finite() || view.width() < 80.0 {
            return;
        }
        let (icon, tip) = if self.locked {
            (crate::icons::LOCKED, "View only — tap to edit")
        } else {
            (crate::icons::UNLOCKED, "Editing — tap to lock")
        };
        // Queue default is one slot above; lock is the bottom of the stack.
        egui::Area::new(egui::Id::new("comfy-lock"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                view.right() - crate::theme::FAB_EDGE,
                view.bottom() - crate::theme::FAB_EDGE,
            ))
            .show(ui.ctx(), |aui| {
                if crate::theme::fab(aui, icon, crate::theme::fab_bg())
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.locked = !self.locked;
                }
            });
    }

    /// Corner overlay showing every node and the current viewport; tap or drag to jump.
    fn minimap(
        &mut self,
        ui: &mut egui::Ui,
        snarl: &Snarl<FlowNodeData>,
        executing: Option<NodeId>,
        focus: Option<NodeId>,
    ) {
        let view = self.view_rect;
        if !view.is_finite() || view.width() < 160.0 || view.height() < 160.0 {
            return;
        }
        let Some(b) = bounds(snarl, &self.sizes) else { return };
        if !b.is_finite() {
            return;
        }
        let b = b.expand(60.0);
        let w = (view.width() * 0.30).clamp(96.0, 200.0);
        let h = (w * (b.height() / b.width()).clamp(0.35, 1.4)).clamp(60.0, 200.0);
        let corner = egui::pos2(view.left() + 10.0, view.top() + 10.0);

        egui::Area::new(egui::Id::new("comfy-minimap"))
            .order(egui::Order::Foreground)
            .fixed_pos(corner)
            .show(ui.ctx(), |aui| {
                let (resp, p) =
                    aui.allocate_painter(egui::vec2(w, h), egui::Sense::click_and_drag());
                let rect = resp.rect;
                let scale = (rect.size() / b.size()).min_elem();
                let tf = TSTransform::new(
                    rect.center().to_vec2() - b.center().to_vec2() * scale,
                    scale,
                );

                p.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(170));
                for (id, pos, _) in snarl.nodes_pos_ids() {
                    let size = self.sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
                    let mut m = tf * egui::Rect::from_min_size(pos, size);
                    if m.width() < 2.0 || m.height() < 2.0 {
                        m = egui::Rect::from_center_size(m.center(), m.size().max(egui::vec2(2.0, 2.0)));
                    }
                    let color = if executing == Some(id) {
                        egui::Color32::from_rgb(90, 200, 110)
                    } else if focus == Some(id) {
                        egui::Color32::from_rgb(110, 170, 255)
                    } else {
                        egui::Color32::from_gray(150)
                    };
                    p.rect_filled(m, 1.0, color);
                }
                let viewport = (tf * (self.to_global.inverse() * view)).intersect(rect);
                p.rect_stroke(
                    viewport,
                    0.0,
                    egui::Stroke::new(1.0, egui::Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
                p.rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
                    egui::StrokeKind::Inside,
                );

                // egui 0.36 reports a drag as soon as the finger leaves the minimap, and the
                // pointer is then outside `rect` — centring on it would jump to a wild graph
                // coordinate. Only points still over the minimap select a view centre.
                if (resp.clicked() || resp.dragged())
                    && let Some(pointer) = resp.interact_pointer_pos()
                    && rect.contains(pointer)
                {
                    self.cmd = Some(ViewCmd::Center(tf.inverse() * pointer));
                }
            });
    }
}

/// Graph-space width of a node's text fields, and with it the width of a text-carrying node.
/// A constant on purpose: a width taken from `available_width` feeds back into the node size snarl
/// derives it from, so the node grows a little every frame (see [`Wrapper::show_input`]) and the
/// measured `sizes` auto-arrange lays out from never settle. Sized to read on a phone at fit zoom.
const NODE_FIELD_W: f32 = 260.0;

/// Width budget for a combo's label on the canvas, and in the Properties pane (which has a whole
/// panel to play with). The canvas figure keeps a node inside [`NODE_FIELD_W`] + its chrome.
const CANVAS_COMBO_W: f32 = 168.0;
const PROPS_COMBO_W: f32 = 280.0;

/// Edge of the square preview a file-selector node shows for its currently selected image.
const NODE_PREVIEW_W: f32 = 156.0;

/// The size cache's entry for a measured node. Clamped at the point of capture rather than inside
/// `arrange`, so every consumer — layout, the fit's bounds, the minimap, hit-testing — agrees on
/// one number. Nothing in a node ratchets any more, so this is a backstop: it only stops a single
/// pathological measure from throwing the whole layout off the canvas.
pub fn clamp_measure(size: egui::Vec2) -> egui::Vec2 {
    size.min(MAX_LAYOUT_NODE)
}

/// Layout backstop: no measured node size larger than this enters the size cache. A single
/// pathological measure (a node drawn mid-transform, a runaway text layout) would otherwise push
/// every later column off the canvas.
const MAX_LAYOUT_NODE: egui::Vec2 = egui::vec2(1600.0, 3000.0);

/// Graph-space height of a node's title bar, for header-only node dragging. Roughly the title
/// row (one line + header frame margins); generous enough to grab, thin enough that the body and
/// its pin rows below stay free for wiring.
const NODE_HEADER_H: f32 = 30.0;

/// egui-snarl's node frame margin (default `Frame::window` inner margin). `sizes` stores the outer
/// frame rect while `nodes_pos_ids` reports the inner content origin, so the frame starts this far
/// above-left of the stored position; shifting by it aligns hit-testing with what snarl draws.
const FRAME_MARGIN: f32 = 6.0;

/// Where a canvas drag began — decides whether it moves a node, pans, or wires. See
/// [`GraphView::drag_gate`].
#[derive(Clone, Copy, PartialEq)]
enum DragKind {
    None,
    /// On a node's title bar → moves the node.
    Header,
    /// On a node body below the title → pans (node move vetoed).
    Body,
    /// Empty canvas or a pin → left to snarl.
    Canvas,
}

fn style() -> SnarlStyle {
    let mut s = SnarlStyle::new();
    // AMOLED canvas; the faint aqua dot grid is drawn in `draw_background` (default grid off).
    s.bg_frame = Some(egui::Frame::new().fill(egui::Color32::from_rgb(3, 3, 5)));
    s.bg_pattern = Some(BackgroundPattern::NoPattern);
    s.min_scale = Some(MIN_SCALE);
    s.max_scale = Some(MAX_SCALE);
    s.centering = Some(true);
    // Orthogonal wires with rounded corners — a structured "network diagram" look instead of
    // droopy beziers, and easier to read where they run.
    s.wire_style = Some(WireStyle::AxisAligned { corner_radius: 8.0 });
    // Bolder wires read as bright circuit traces against the black canvas.
    s.wire_width = Some(2.6);
    // Pins sit just outside the node body: their dots stop overlapping the input/output labels,
    // and they become fat finger targets clear of the draggable node frame.
    s.pin_placement = Some(PinPlacement::Outside { margin: 3.0 });
    s.pin_size = Some(15.0);
    // Stack inputs above outputs instead of side by side. In snarl's default Coil layout the input
    // column and the output column are measured against the same full node width and then SUMMED,
    // so every node carries its output labels' width as dead horizontal weight — and arrange spaces
    // its columns by exactly that width. Sandwich made a realistic txt2img graph 2039 -> 1775 units
    // wide (fit zoom 0.175 -> 0.199 on a 393x873 phone); nodes come out uniform and a little taller,
    // which is the right trade on a portrait screen. Revert by deleting this line.
    s.node_layout = Some(NodeLayout::sandwich());
    s
}

/// A node body: dark glass a step above the black canvas; the rim comes from [`Wrapper::node_frame`].
///
/// Translucent, so the blur `Wrapper::frost_nodes` lays down behind each node reaches the eye. An
/// opaque fill here would hide it completely and the whole pass would be paid for and never seen.
/// Premultiplied because only that constructor is `const`; this is `(22, 21, 34)` at alpha 190.
const NODE_FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(16, 16, 25, 190);
/// Node corner radius, kept in one place because the frosted rect behind a node has to round to
/// the same figure — a mismatch shows up as bright corners poking out from under the glass.
const NODE_CORNER: f32 = 8.0;
/// Base spacing (graph units) of the canvas dot grid — anchored in graph space so it scales with
/// the nodes; coarsened by powers of two when zoomed far out.
const DOT_SPACING: f32 = 28.0;
/// Dot radius in GRAPH units, so dots grow/shrink with the zoom exactly like the nodes do.
const DOT_RADIUS: f32 = 1.7;
/// Dim teal ink for the dot grid — reads as a faint field because the dots are small.
const DOT_COLOR: egui::Color32 = egui::Color32::from_rgb(30, 70, 74);
/// Resting-node edge — the same dim white hairline every other surface carries. See `theme::RIM`.
const NODE_RIM: egui::Color32 = crate::theme::RIM_BRIGHT;

/// Do two measure snapshots describe the same canvas — same nodes, no size moved by more than a
/// unit? Tells a settled layout from one egui is still converging.
fn sizes_agree(a: &HashMap<NodeId, egui::Vec2>, b: &HashMap<NodeId, egui::Vec2>) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(id, s)| b.get(id).is_some_and(|p| (*s - *p).abs().max_elem() <= 1.0))
}

/// Bounding box of all nodes in graph space (measured sizes where known).
fn bounds(snarl: &Snarl<FlowNodeData>, sizes: &HashMap<NodeId, egui::Vec2>) -> Option<egui::Rect> {
    let mut b: Option<egui::Rect> = None;
    for (id, pos, _) in snarl.nodes_pos_ids() {
        let size = sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
        let r = egui::Rect::from_min_size(pos, size);
        b = Some(b.map_or(r, |b| b.union(r)));
    }
    b
}

/// The transform that fits `view` (graph space) into `ui_rect` (screen space), scale clamped.
fn fit_transform(view: egui::Rect, ui_rect: egui::Rect) -> TSTransform {
    let scale = (ui_rect.size() / view.size()).min_elem().clamp(MIN_SCALE, MAX_SCALE);
    TSTransform::new(ui_rect.center().to_vec2() - view.center().to_vec2() * scale, scale)
}

/// Compact layout: bands by longest-path depth, nodes stacked within each band with small gaps,
/// bands centred across the flow — measured sizes, so nothing overlaps. Returns the placed rects.
///
/// `vertical` runs execution top-to-bottom instead of left-to-right. A deep workflow laid out
/// across a portrait phone is a long thin ribbon, and fitting it puts every node at ~55px wide;
/// turning the flow to match the screen's long axis is what makes the same graph readable
/// (measured on a 393x873 viewport: 1775x368 at fit zoom 0.199, versus 608x1046 at 0.65).
pub fn arrange(
    snarl: &mut Snarl<FlowNodeData>,
    sizes: &HashMap<NodeId, egui::Vec2>,
    vertical: bool,
) -> Vec<egui::Rect> {
    // Gaps along the flow (between depths) and across it (between siblings in a depth).
    const FLOW_GAP: f32 = 60.0;
    const CROSS_GAP: f32 = 24.0;
    // A node's extent along the flow axis and across it, and how to build a position from the two.
    let flow = |v: egui::Vec2| if vertical { v.y } else { v.x };
    let cross = |v: egui::Vec2| if vertical { v.x } else { v.y };
    let at = |along: f32, across: f32| {
        if vertical { egui::pos2(across, along) } else { egui::pos2(along, across) }
    };

    let ids: Vec<NodeId> = snarl.nodes_pos_ids().map(|(id, _, _)| id).collect();
    let mut successors: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut predecessors: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (from, to) in snarl.wires() {
        if from.node == to.node {
            continue;
        }
        successors.entry(from.node).or_default().push(to.node);
        predecessors.entry(to.node).or_default().push(from.node);
    }

    // Pseudo-topological order via iterative DFS post-order — robust to cycles, which a converted
    // workflow can contain (SetNode/GetNode and "Anything Everywhere" links reconstruct as
    // back-edges). Kahn's-style layering would let one such cycle poison every downstream node's
    // depth and collapse the whole graph into a single column.
    let mut order: Vec<NodeId> = Vec::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    for &start in &ids {
        if visited.contains(&start) {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((node, processed)) = stack.pop() {
            if processed {
                order.push(node);
                continue;
            }
            if !visited.insert(node) {
                continue;
            }
            stack.push((node, true));
            for &next in successors.get(&node).into_iter().flatten() {
                if !visited.contains(&next) {
                    stack.push((next, false));
                }
            }
        }
    }
    order.reverse(); // producers before consumers
    let topo: HashMap<NodeId, usize> = order.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    // Longest-path layer over forward edges only (topo index increases); back-edges wrap around
    // rather than shoving their target into a late column.
    let mut depth: HashMap<NodeId, usize> = ids.iter().map(|&id| (id, 0)).collect();
    for &node in &order {
        let d = depth[&node];
        for &next in successors.get(&node).into_iter().flatten() {
            if topo.get(&next).copied().unwrap_or(0) > topo[&node] {
                let e = depth.entry(next).or_insert(0);
                *e = (*e).max(d + 1);
            }
        }
    }
    let deepest = depth.values().copied().max().unwrap_or(0);
    let mut columns: Vec<Vec<NodeId>> = vec![Vec::new(); deepest + 1];
    for (id, _, _) in snarl.nodes_pos_ids() {
        let d = depth.get(&id).copied().unwrap_or(0);
        columns[d].push(id);
    }
    // Seed each column's vertical order from the original layout, then reduce edge crossings with
    // barycenter sweeps (each node drifts toward the average row of its neighbours) so wires run
    // mostly straight left-to-right and the order of execution reads down each column.
    for column in &mut columns {
        column.sort_by(|a, b| {
            let key = |id: &NodeId| {
                snarl
                    .get_node_info(*id)
                    .map(|n| if vertical { n.pos.x } else { n.pos.y })
                    .unwrap_or(0.0)
            };
            key(a).total_cmp(&key(b))
        });
    }
    let indices = |columns: &[Vec<NodeId>]| -> HashMap<NodeId, f32> {
        let mut m = HashMap::new();
        for column in columns {
            for (i, &id) in column.iter().enumerate() {
                m.insert(id, i as f32);
            }
        }
        m
    };
    let barycenter = |id: NodeId, neighbors: &HashMap<NodeId, Vec<NodeId>>, idx: &HashMap<NodeId, f32>, fallback: f32| -> f32 {
        match neighbors.get(&id) {
            Some(ns) if !ns.is_empty() => {
                ns.iter().filter_map(|n| idx.get(n)).sum::<f32>() / ns.len() as f32
            }
            _ => fallback,
        }
    };
    let reorder = |column: &mut Vec<NodeId>, neighbors: &HashMap<NodeId, Vec<NodeId>>, idx: &HashMap<NodeId, f32>| {
        let mut keyed: Vec<(NodeId, f32)> = column
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, barycenter(id, neighbors, idx, i as f32)))
            .collect();
        keyed.sort_by(|a, b| a.1.total_cmp(&b.1));
        *column = keyed.into_iter().map(|(id, _)| id).collect();
    };
    for _ in 0..4 {
        let idx = indices(&columns);
        for d in 1..columns.len() {
            let mut column = std::mem::take(&mut columns[d]);
            reorder(&mut column, &predecessors, &idx);
            columns[d] = column;
        }
        let idx = indices(&columns);
        for d in (0..columns.len().saturating_sub(1)).rev() {
            let mut column = std::mem::take(&mut columns[d]);
            reorder(&mut column, &successors, &idx);
            columns[d] = column;
        }
    }

    let size_of = |id: NodeId| sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
    // Band offsets and thicknesses along the flow, from the deepest-extent node in each band.
    let mut col_x = Vec::with_capacity(columns.len());
    let mut col_w = Vec::with_capacity(columns.len());
    let mut x = 0.0f32;
    for column in &columns {
        col_x.push(x);
        let w = if column.is_empty() {
            0.0
        } else {
            column.iter().map(|&id| flow(size_of(id))).fold(1.0f32, f32::max)
        };
        col_w.push(w);
        if !column.is_empty() {
            x += w + FLOW_GAP;
        }
    }
    // Seed each node's cross-axis centre from a centred stack per band (a non-overlapping start).
    let mut cy: HashMap<NodeId, f32> = HashMap::new();
    for column in &columns {
        let total: f32 =
            column.iter().map(|&id| cross(size_of(id)) + CROSS_GAP).sum::<f32>() - CROSS_GAP;
        let mut top = -total / 2.0;
        for &id in column {
            let h = cross(size_of(id));
            cy.insert(id, top + h / 2.0);
            top += h + CROSS_GAP;
        }
    }
    // Push each node down to keep V_GAP from the one above while preserving column order.
    let resolve = |column: &[NodeId], cy: &mut HashMap<NodeId, f32>| {
        for w in 1..column.len() {
            let prev = column[w - 1];
            let cur = column[w];
            let min_c =
                cy[&prev] + cross(size_of(prev)) / 2.0 + CROSS_GAP + cross(size_of(cur)) / 2.0;
            if cy[&cur] < min_c {
                cy.insert(cur, min_c);
            }
        }
    };
    // A column's mean centre — the relaxation puts each column back on its own mean after every
    // separation pass, so the pass can only SPREAD a column, never translate it.
    let column_mean = |column: &[NodeId], cy: &HashMap<NodeId, f32>| -> f32 {
        if column.is_empty() {
            return 0.0;
        }
        column.iter().filter_map(|id| cy.get(id)).sum::<f32>() / column.len() as f32
    };
    // What the relaxation is FOR: wires that run straight across, rather than diagonally.
    let edges: Vec<(NodeId, NodeId)> = successors
        .iter()
        .flat_map(|(from, tos)| tos.iter().map(|to| (*from, *to)))
        .collect();
    let wire_cost = |cy: &HashMap<NodeId, f32>| -> f32 {
        edges
            .iter()
            .filter_map(|(a, b)| Some((cy.get(a)?, cy.get(b)?)))
            .map(|(a, b)| (a - b).abs())
            .sum()
    };
    let vspan = |cy: &HashMap<NodeId, f32>| -> f32 {
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for (&id, &c) in cy {
            let h = cross(size_of(id));
            lo = lo.min(c - h / 2.0);
            hi = hi.max(c + h / 2.0);
        }
        if hi > lo { hi - lo } else { 0.0 }
    };
    // Straighter wires are worth some height, but only if they buy twice as much straightening as
    // the height they cost — otherwise the compact seeded stack wins.
    let seed_span = vspan(&cy);
    let score = |cy: &HashMap<NodeId, f32>| wire_cost(cy) + 0.5 * (vspan(cy) - seed_span).max(0.0);

    // Relax each node toward its neighbours' mean centre-y, then restore spacing within columns.
    //
    // Both halves used to make the layout taller every single iteration: `resolve` can only push a
    // node DOWN, so each pass translated whichever nodes sat in a fan-in/fan-out conflict further
    // down while everything else stayed put. Over the 8 iterations that inflated the vertical span
    // to ~2.9x the seeded stack (measured: a chain+diamond went 648 -> 1432, an img2img graph with
    // a disconnected node 948 -> 2798), and that multiplier lands on the measured node HEIGHT. So:
    // every separation pass is now mean-preserving, and the best iterate is kept rather than the
    // last, which means the relaxation can never return a layout worse than the seed it started
    // from.
    let mut best = cy.clone();
    let mut best_score = score(&cy);
    for _ in 0..8 {
        for neighbors in [&predecessors, &successors] {
            for column in &columns {
                for &id in column {
                    let Some(ns) = neighbors.get(&id) else { continue };
                    // Average over the neighbours actually found: dividing by `ns.len()` when one
                    // is missing biases the result toward 0, i.e. toward the top of the layout.
                    let (sum, found) = ns
                        .iter()
                        .filter_map(|n| cy.get(n))
                        .fold((0.0, 0usize), |(s, k), v| (s + v, k + 1));
                    if found > 0 {
                        cy.insert(id, sum / found as f32);
                    }
                }
            }
            for column in &columns {
                let before = column_mean(column, &cy);
                resolve(column, &mut cy);
                let drift = column_mean(column, &cy) - before;
                if drift != 0.0 {
                    for id in column {
                        if let Some(c) = cy.get_mut(id) {
                            *c -= drift;
                        }
                    }
                }
            }
        }
        let s = score(&cy);
        if s < best_score {
            best_score = s;
            best = cy.clone();
        }
    }
    let cy = best;

    let mut rects = Vec::new();
    for (d, column) in columns.iter().enumerate() {
        for &id in column {
            let size = size_of(id);
            // Centred within the band rather than pinned to its leading edge: a thin node beside a
            // deep one used to sit against the edge with the whole difference left as a void.
            let along = col_x[d] + (col_w[d] - flow(size)) / 2.0;
            let across = cy[&id] - cross(size) / 2.0;
            let pos = at(along, across);
            if let Some(info) = snarl.get_node_info_mut(id) {
                info.pos = pos;
            }
            rects.push(egui::Rect::from_min_size(pos, size));
        }
    }
    rects
}

/// Position of a workflow's first node: the leftmost node with no incoming wires (any of them),
/// falling back to the leftmost node overall.
pub fn first_node_pos(snarl: &Snarl<FlowNodeData>, vertical: bool) -> Option<egui::Pos2> {
    let has_input: HashSet<NodeId> = snarl.wires().map(|(_, in_pin)| in_pin.node).collect();
    // "First" is along the flow: leftmost when execution runs left-to-right, topmost when the
    // canvas is portrait and the layout runs top-to-bottom.
    let along = |p: egui::Pos2| if vertical { p.y } else { p.x };
    let mut root: Option<egui::Pos2> = None;
    let mut first: Option<egui::Pos2> = None;
    for (id, pos, _) in snarl.nodes_pos_ids() {
        if first.is_none_or(|p| along(pos) < along(p)) {
            first = Some(pos);
        }
        if !has_input.contains(&id) && root.is_none_or(|p| along(pos) < along(p)) {
            root = Some(pos);
        }
    }
    root.or(first)
}

/// Delete `nid`, first bridging its MODEL/CLIP inputs to the matching outputs so a loader chain
/// stays connected: the predecessor feeding each rail's input is wired to every successor the
/// rail's output fed. Rails with no predecessor or no successor are dropped with the node.
pub fn bridge_and_remove(snarl: &mut Snarl<FlowNodeData>, nid: NodeId) {
    use rucomfyui::object_info::ObjectType;
    let rails: [(&str, ObjectType, &str); 2] =
        [("model", ObjectType::Model, "MODEL"), ("clip", ObjectType::Clip, "CLIP")];
    let mut plan: Vec<(OutPinId, Vec<InPinId>)> = Vec::new();
    if let Some(data) = snarl.get_node(nid) {
        for (in_name, out_type, out_name) in rails {
            let Some(in_idx) = data.inputs.iter().position(|i| i.name.eq_ignore_ascii_case(in_name))
            else {
                continue;
            };
            let Some(out_idx) = data
                .outputs
                .iter()
                .position(|o| o.typ == out_type || o.name.eq_ignore_ascii_case(out_name))
            else {
                continue;
            };
            let pred = snarl.in_pin(InPinId { node: nid, input: in_idx }).remotes.first().copied();
            let succs: Vec<InPinId> =
                snarl.out_pin(OutPinId { node: nid, output: out_idx }).remotes.clone();
            if let Some(pred) = pred
                && !succs.is_empty()
            {
                plan.push((pred, succs));
            }
        }
    }
    for (pred, succs) in plan {
        for succ in succs {
            snarl.connect(pred, succ);
        }
    }
    snarl.remove_node(nid);
}

/// Delegates to [`FlowViewer`], gating all mutations when locked, measuring node sizes for the
/// minimap, and applying pending view commands through the transform hook.
struct Wrapper<'a> {
    inner: FlowViewer<'a>,
    locked: bool,
    focus: Option<NodeId>,
    bypassed: &'a HashSet<NodeId>,
    model_picks: &'a mut Vec<ModelPick>,
    file_pick: &'a mut Option<NodeId>,
    input_thumbs: &'a HashMap<String, egui::TextureHandle>,
    seed_randomize: &'a mut HashMap<(NodeId, String), bool>,
    cmd: Option<ViewCmd>,
    /// Screen-space pan to add this frame (locked-mode drag started on a node).
    pan: egui::Vec2,
    bounds: Option<egui::Rect>,
    ui_rect: egui::Rect,
    sizes: &'a mut HashMap<NodeId, egui::Vec2>,
    out_transform: &'a mut TSTransform,
    /// Node rects in GRAPH space, for the backdrop blur. Written by `final_node_rect` as the nodes
    /// lay out and read by the *next* frame's `draw_background` — the same one-frame staleness the
    /// `frost` module already lives with, and for the same reason: a pane's rect is not known until
    /// it has laid out, but the glass has to be painted before it.
    node_rects: &'a mut Vec<egui::Rect>,
}

impl Wrapper<'_> {
    /// Blur the canvas behind every node laid out last frame, then clear the list for this one.
    ///
    /// Called at the end of `draw_background` because that is the only point in snarl's frame that
    /// is after the grid and before any node: the grab-pass composites whatever is already in the
    /// framebuffer at its rect, so anywhere later would blur the nodes into themselves. Snarl keeps
    /// the whole canvas in one transformed layer, so the callbacks are added to `painter`'s layer
    /// in graph coordinates and egui's layer transform puts them on screen.
    fn frost_nodes(&mut self, painter: &egui::Painter, viewport: &egui::Rect, scale: f32) {
        let mut ui = egui::Ui::new(
            painter.ctx().clone(),
            egui::Id::new("graph-node-frost"),
            egui::UiBuilder::new().layer_id(painter.layer_id()).max_rect(*viewport),
        );
        // The callback grabs `viewport ∩ clip_rect`, and this ui has no content of its own to
        // derive a useful clip from — so it is set to the visible canvas explicitly.
        ui.set_clip_rect(*viewport);
        crate::frost::glass_rects(&ui, self.node_rects, NODE_CORNER, scale);
        self.node_rects.clear();
    }
}

impl SnarlViewer<FlowNodeData> for Wrapper<'_> {
    fn title(&mut self, node: &FlowNodeData) -> String {
        self.inner.title(node)
    }

    fn inputs(&mut self, node: &FlowNodeData) -> usize {
        self.inner.inputs(node)
    }

    fn outputs(&mut self, node: &FlowNodeData) -> usize {
        self.inner.outputs(node)
    }

    #[allow(refining_impl_trait)]
    fn show_input(
        &mut self,
        pin: &InPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<FlowNodeData>,
    ) -> PinInfo {
        let before = picked_file(snarl, pin.id.node);
        let seed = snarl
            .get_node(pin.id.node)
            .and_then(|n| n.inputs.get(pin.id.input))
            .is_some_and(is_seed_widget)
            && pin.remotes.is_empty();
        // A file selector opens the thumbnail picker instead of a dropdown of bare filenames.
        let media = pin.remotes.is_empty()
            && snarl
                .get_node(pin.id.node)
                .and_then(media_input_idx)
                .is_some_and(|idx| idx == pin.id.input);
        // Every other enum (model / sampler / scheduler / LoRA names) gets the elided combo.
        let enum_row = !media
            && pin.remotes.is_empty()
            && snarl
                .get_node(pin.id.node)
                .and_then(|n| n.inputs.get(pin.id.input))
                .is_some_and(|i| matches!(i.value, FlowValueType::Array { .. }));
        // Cap the row's width before the widget lays out. `rucomfyui_node_graph` draws string
        // inputs with a plain `text_edit_singleline`/`multiline`, which take `available_width` —
        // and snarl derives that from the node's size measured on the PREVIOUS frame. Each frame's
        // row therefore comes out a little wider than the node it was measured in, the node grows
        // to fit, and the pair ratchets outward frame after frame (121 → 173 → 225 → … units in
        // `load_arrange_matches_a_later_manual_arrange`) until it hits the graph-space viewport —
        // a ceiling that scales with 1/zoom, so a far-out fit lets it run a very long way. Those
        // measures are what `final_node_rect` caches and auto-arrange spaces its columns by.
        let info = ui
            .scope(|ui| {
                ui.set_max_width(NODE_FIELD_W);
                if media {
                    show_media_input(ui, pin, snarl, self.locked, self.file_pick)
                } else if enum_row {
                    show_enum_input(ui, pin, snarl, self.locked)
                } else if seed {
                    show_seed_input(ui, pin, snarl, self.locked, self.seed_randomize)
                } else if self.locked {
                    let mut info = None;
                    ui.add_enabled_ui(false, |ui| {
                        info = Some(self.inner.show_input(pin, ui, snarl))
                    });
                    info.unwrap_or_else(PinInfo::circle)
                } else {
                    self.inner.show_input(pin, ui, snarl)
                }
            })
            .inner;
        if let Some(pick) =
            pick_changed(pin.id.node, before.as_ref(), picked_file(snarl, pin.id.node))
        {
            self.model_picks.push(pick);
        }
        info
    }

    #[allow(refining_impl_trait)]
    fn show_output(
        &mut self,
        pin: &OutPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<FlowNodeData>,
    ) -> PinInfo {
        self.inner.show_output(pin, ui, snarl)
    }

    fn node_frame(
        &mut self,
        default: egui::Frame,
        node: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        snarl: &Snarl<FlowNodeData>,
    ) -> egui::Frame {
        let mut frame = self
            .inner
            .node_frame(default, node, inputs, outputs, snarl)
            .fill(NODE_FILL)
            .corner_radius(NODE_CORNER);
        // The inner viewer paints a green 2px stroke on the executing node; anything below 2px is
        // just its default hairline, so we only override our own rims when the width is < 2.
        let inner_width = frame.stroke.width;
        let is_app = snarl
            .get_node(node)
            .is_some_and(|d| d.object.name.starts_with(crate::apps::APP_CLASS_PREFIX));
        if self.bypassed.contains(&node) {
            // Dimmed fill + orange stroke marks a bypassed (mode-4) node.
            frame = frame
                .fill(egui::Color32::from_rgb(38, 32, 24))
                .stroke(egui::Stroke::new(2.0, egui::Color32::from_rgb(214, 140, 70)));
        } else if is_app {
            // Collapsed app nodes get a violet coat so they can be spotted across a big graph.
            // Focus (pink) still needs to read on top of it, so only the fill marks a focused one.
            frame = frame.fill(egui::Color32::from_rgb(34, 26, 46)).stroke(egui::Stroke::new(
                2.0,
                if self.focus == Some(node) && inner_width < 2.0 {
                    crate::theme::PINK
                } else {
                    crate::theme::VIOLET
                },
            ));
        } else if self.focus == Some(node) && inner_width < 2.0 {
            // Selected / focused: a vivid pink rim (the primary accent).
            frame = frame.stroke(egui::Stroke::new(2.0, crate::theme::PINK));
        } else if inner_width < 2.0 {
            // Resting node: a subtle cool glass rim so it reads as a raised pane on black.
            frame = frame.stroke(egui::Stroke::new(1.0, NODE_RIM));
        }
        frame
    }

    fn draw_background(
        &mut self,
        _background: Option<&BackgroundPattern>,
        viewport: &egui::Rect,
        _snarl_style: &SnarlStyle,
        _style: &egui::Style,
        painter: &egui::Painter,
        _snarl: &Snarl<FlowNodeData>,
    ) {
        // Dot grid drawn in graph space (the layer transform sizes it to screen). Spacing and
        // radius are anchored in GRAPH units, so the grid scales 1:1 with the nodes as you zoom.
        // When zoomed far out we coarsen the spacing by powers of two so the on-screen density and
        // the dot count stay bounded, and dots shrink toward sub-pixel (like the nodes) so a very
        // zoomed-out canvas isn't cluttered.
        let scale = self.out_transform.scaling.max(0.001);
        // Light first, then the grid, then the glass — the node frost grabs the framebuffer, so
        // anything it should reveal has to be painted before `frost_nodes` at the end of this fn.
        // Anchored to the viewport rather than to graph space, so the canvas stays evenly lit
        // instead of the light sliding away as you pan.
        crate::theme::ambience(painter, *viewport, 3);
        let mut spacing = DOT_SPACING;
        while spacing * scale < 26.0 {
            spacing *= 2.0;
        }
        let min_x = (viewport.min.x / spacing).floor() as i64;
        let max_x = (viewport.max.x / spacing).ceil() as i64;
        let min_y = (viewport.min.y / spacing).floor() as i64;
        let max_y = (viewport.max.y / spacing).ceil() as i64;
        // Backstop against a pathological transform.
        if (max_x - min_x).saturating_mul(max_y - min_y) > 6500 {
            return;
        }
        for xi in min_x..=max_x {
            for yi in min_y..=max_y {
                let p = egui::pos2(xi as f32 * spacing, yi as f32 * spacing);
                painter.circle_filled(p, DOT_RADIUS, DOT_COLOR);
            }
        }
        self.frost_nodes(painter, viewport, scale);
    }

    fn has_body(&mut self, node: &FlowNodeData) -> bool {
        self.inner.has_body(node)
    }

    fn has_footer(&mut self, node: &FlowNodeData) -> bool {
        self.inner.has_footer(node)
    }

    fn show_footer(
        &mut self,
        node_id: NodeId,
        inputs: &[InPin],
        outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<FlowNodeData>,
    ) {
        // The file a LoadImage-style node points at, drawn as the picture it actually is. The
        // footer (not the body) is where node images go — `rucomfyui_node_graph::has_body` is
        // hardcoded false and it renders output images here too.
        let preview = snarl
            .get_node(node_id)
            .and_then(media_selected)
            .filter(|(sel, video)| !video && !sel.is_empty())
            .and_then(|(sel, _)| self.input_thumbs.get(sel))
            .cloned();
        if let Some(tex) = preview {
            ui.add(
                egui::Image::new(egui::load::SizedTexture::from_handle(&tex))
                    // A FIXED graph-space box, never `available_width`: a node's measured size is
                    // fed back as next frame's layout width, so anything elastic in here would
                    // ratchet the node wider every frame (and auto-arrange lays out from those
                    // measurements). See [`NODE_FIELD_W`].
                    .fit_to_exact_size(egui::vec2(NODE_PREVIEW_W, NODE_PREVIEW_W))
                    .corner_radius(4.0),
            );
        }
        self.inner.show_footer(node_id, inputs, outputs, ui, snarl);
    }

    fn final_node_rect(
        &mut self,
        node: NodeId,
        rect: egui::Rect,
        _ui: &mut egui::Ui,
        _snarl: &mut Snarl<FlowNodeData>,
    ) {
        self.sizes.insert(node, clamp_measure(rect.size()));
        self.node_rects.push(rect);
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<FlowNodeData>) {
        if !self.locked {
            self.inner.connect(from, to, snarl);
        }
    }

    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<FlowNodeData>) {
        if !self.locked {
            snarl.disconnect(from.id, to.id);
        }
    }

    fn drop_outputs(&mut self, pin: &OutPin, snarl: &mut Snarl<FlowNodeData>) {
        if !self.locked {
            snarl.drop_outputs(pin.id);
        }
    }

    fn drop_inputs(&mut self, pin: &InPin, snarl: &mut Snarl<FlowNodeData>) {
        if !self.locked {
            snarl.drop_inputs(pin.id);
        }
    }

    // The empty-canvas menu is handled by our own long-press detection + Add node window, not
    // snarl's native context menu (which is transient on touch — it closes the instant the finger
    // lifts). Reporting no graph menu keeps snarl from opening it.
    fn has_graph_menu(&mut self, _pos: egui::Pos2, _snarl: &mut Snarl<FlowNodeData>) -> bool {
        false
    }

    fn show_graph_menu(&mut self, _pos: egui::Pos2, _ui: &mut egui::Ui, _snarl: &mut Snarl<FlowNodeData>) {}

    fn has_node_menu(&mut self, node: &FlowNodeData) -> bool {
        !self.locked && self.inner.has_node_menu(node)
    }

    fn show_node_menu(
        &mut self,
        node_id: NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<FlowNodeData>,
    ) {
        if self.locked {
            return;
        }
        let label = snarl.get_node(node_id).map(|n| self.inner.title(n)).unwrap_or_default();
        ui.label(sanitize_ui_text(ui, &label));
        ui.separator();
        // Delete, bridging any MODEL/CLIP chain so the loader run stays connected.
        if ui.button("Delete").clicked() {
            bridge_and_remove(snarl, node_id);
            ui.close();
        }
    }

    fn current_transform(&mut self, to_global: &mut TSTransform, _snarl: &mut Snarl<FlowNodeData>) {
        match self.cmd.take() {
            Some(ViewCmd::FitAll) => {
                if let Some(b) = self.bounds
                    && b.is_finite()
                    && self.ui_rect.is_finite()
                {
                    *to_global = fit_transform(b.expand(60.0), self.ui_rect);
                }
            }
            Some(ViewCmd::Center(p)) => {
                if p.x.is_finite() && p.y.is_finite() && self.ui_rect.is_finite() {
                    let s = to_global.scaling;
                    *to_global =
                        TSTransform::new(self.ui_rect.center().to_vec2() - p.to_vec2() * s, s);
                }
            }
            Some(ViewCmd::Focus(p)) => {
                if p.x.is_finite() && p.y.is_finite() && self.ui_rect.is_finite() {
                    // Zoom into a comfortable band: pull a far-out view in, leave a close one be.
                    let s = to_global.scaling.clamp(0.7, 1.2);
                    *to_global =
                        TSTransform::new(self.ui_rect.center().to_vec2() - p.to_vec2() * s, s);
                }
            }
            None => {}
        }
        if self.pan != egui::Vec2::ZERO {
            to_global.translation += self.pan;
        }
        *self.out_transform = *to_global;
    }
}

/// Seed row on the canvas: value + randomize checkbox (ComfyUI `control_after_generate`).
fn show_seed_input(
    ui: &mut egui::Ui,
    pin: &InPin,
    snarl: &mut Snarl<FlowNodeData>,
    locked: bool,
    seed_randomize: &mut HashMap<(NodeId, String), bool>,
) -> PinInfo {
    let node = &mut snarl[pin.id.node];
    let input = &mut node.inputs[pin.id.input];
    let key = (pin.id.node, input.name.clone());
    let mut randomize = seed_randomize.get(&key).copied().unwrap_or(false);
    let color = pin_color(input);
    ui.add_enabled_ui(!locked, |ui| {
        ui.horizontal(|ui| {
            ui.label(&input.name);
            ui.add_enabled_ui(!randomize, |ui| match &mut input.value {
                FlowValueType::UnsignedInt { value, min, max, step } => {
                    ui.add(
                        egui::DragValue::new(value)
                            .range(*min..=*max)
                            .speed((*step as f64).max(1.0)),
                    );
                }
                FlowValueType::SignedInt { value, min, max, step } => {
                    ui.add(
                        egui::DragValue::new(value)
                            .range(*min..=*max)
                            .speed((*step as f64).max(1.0)),
                    );
                }
                _ => {}
            });
            if ui.checkbox(&mut randomize, "random").changed() {
                seed_randomize.insert(key, randomize);
            }
        });
    });
    PinInfo::circle().with_fill(color)
}

/// A file-selector row on the canvas: the current filename as a button that asks the app to open
/// the thumbnail picker. The dropdown it replaces listed bare filenames, which is no way to choose
/// between a dozen `ComfyUI_00042_.png`s — let alone between videos.
fn show_media_input(
    ui: &mut egui::Ui,
    pin: &InPin,
    snarl: &mut Snarl<FlowNodeData>,
    locked: bool,
    pick: &mut Option<NodeId>,
) -> PinInfo {
    let node = &snarl[pin.id.node];
    let input = &node.inputs[pin.id.input];
    let color = pin_color(input);
    let video = media_input_kind(input).unwrap_or(false);
    let selected = match &input.value {
        FlowValueType::Array { selected, .. } => selected.clone(),
        _ => String::new(),
    };
    let name = input.name.clone();
    ui.add_enabled_ui(!locked, |ui| {
        ui.label(&name);
        // No film glyph in the bundled font; the play triangle is what marks video everywhere else.
        let icon = if video { crate::icons::RUN } else { crate::icons::IMAGE };
        let label = if selected.is_empty() {
            format!("{icon} Choose…")
        } else {
            // Filenames are long and front-loaded with sameness; the tail identifies the file.
            format!("{icon} {}", elide_tail(&sanitize_ui_text(ui, &selected), 22))
        };
        if ui
            .button(label)
            .on_hover_text(if video { "Pick a video" } else { "Pick an image" })
            .clicked()
        {
            *pick = Some(pin.id.node);
        }
    });
    PinInfo::circle().with_fill(color)
}

/// Enum row on the canvas (checkpoint / sampler / scheduler / LoRA / VAE names). Drawn here rather
/// than delegated so the label is elided to the node's width budget — see [`option_combo`].
fn show_enum_input(
    ui: &mut egui::Ui,
    pin: &InPin,
    snarl: &mut Snarl<FlowNodeData>,
    locked: bool,
) -> PinInfo {
    let node = &mut snarl[pin.id.node];
    let input = &mut node.inputs[pin.id.input];
    let color = pin_color(input);
    let name = input.name.clone();
    let salt = egui::Id::new(("canvas-enum", pin.id.node, pin.id.input));
    ui.add_enabled_ui(!locked, |ui| {
        ui.horizontal(|ui| {
            ui.label(&name);
            if let FlowValueType::Array { options, selected } = &mut input.value {
                option_combo(ui, salt, selected, options, CANVAS_COMBO_W);
            }
        });
    });
    PinInfo::circle().with_fill(color)
}

/// Pin colour for an input, matching `rucomfyui_node_graph`'s type-hash scheme (its own
/// `data_type_color` is private).
fn pin_color(input: &FlowInput) -> egui::Color32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    format!("{:?}", input.typ).hash(&mut hasher);
    let hash = (hasher.finish() % 3600) as f32 / 3600.0;
    egui::ecolor::Hsva::new(hash, 0.5, 0.5, 1.0).into()
}

/// Like [`elide`], but keeps the END of the string — filenames differ in their tail.
pub fn elide_tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - max).collect();
    format!("…{tail}")
}

// ── UI-format export ──────────────────────────────────────────────────────────

impl GraphView {
    /// Serialize the editor graph to ComfyUI **UI-format** JSON (legacy 0.4 shape, which every
    /// frontend opens), so workflows saved from the phone round-trip with the website. Node
    /// positions come from the canvas; measured sizes where known.
    pub fn export_ui(
        &self,
        g: &ComfyUiNodeGraph,
        schemas: &crate::schema::SchemaSet,
        bypassed: &HashSet<NodeId>,
        seed_randomize: &HashMap<(NodeId, String), bool>,
    ) -> serde_json::Value {
        use serde_json::json;

        let node_id = |id: NodeId| id.0 as u64 + 1;
        let mut in_links: HashMap<(NodeId, usize), u64> = HashMap::new();
        let mut out_links: HashMap<(NodeId, usize), Vec<u64>> = HashMap::new();
        let mut link_rows = Vec::new();
        let mut last_link = 0u64;
        for (from, to) in g.snarl.wires() {
            last_link += 1;
            let ty = g
                .snarl
                .get_node(from.node)
                .and_then(|n| n.outputs.get(from.output))
                .map(|o| type_str(&o.typ))
                .unwrap_or_else(|| "*".to_string());
            link_rows.push(json!([
                last_link,
                node_id(from.node),
                from.output,
                node_id(to.node),
                to.input,
                ty
            ]));
            in_links.insert((to.node, to.input), last_link);
            out_links.entry((from.node, from.output)).or_default().push(last_link);
        }

        let mut nodes = Vec::new();
        let mut entries: Vec<_> = g.snarl.nodes_pos_ids().collect();
        entries.sort_by_key(|(id, _, _)| *id);
        let mut last_node = 0u64;
        for (order, (id, pos, data)) in entries.into_iter().enumerate() {
            last_node = last_node.max(node_id(id));
            let schema = schemas.nodes.get(&data.object.name);

            let inputs: Vec<serde_json::Value> = data
                .inputs
                .iter()
                .enumerate()
                .map(|(i, input)| {
                    let link = in_links.get(&(id, i));
                    let mut entry = json!({
                        "name": input.name,
                        "type": type_str(&input.typ),
                        "link": link,
                    });
                    if !input.value.is_connection_only() {
                        entry["widget"] = json!({ "name": input.name });
                    }
                    entry
                })
                .collect();
            let outputs: Vec<serde_json::Value> = data
                .outputs
                .iter()
                .enumerate()
                .map(|(i, out)| {
                    json!({
                        "name": out.name,
                        "type": type_str(&out.typ),
                        "links": out_links.get(&(id, i)).cloned().unwrap_or_default(),
                        "slot_index": i,
                    })
                })
                .collect();

            let mut widgets_values = Vec::new();
            for input in &data.inputs {
                // Only schema widgets belong in widgets_values; a link input that carries a
                // stray value would shift every later widget on the next positional read.
                if let Some(s) = schema
                    && let Some(si) = s.inputs.iter().find(|si| si.name == input.name)
                    && !crate::uiwf::is_widget(&si.kind)
                {
                    continue;
                }
                let value = match &input.value {
                    // A numeric COMBO is a number in the file the web frontend writes; exporting
                    // the display text instead leaves the workflow embedded in the PNG unqueueable
                    // there. The editor holds only the text, so the schema supplies the original.
                    FlowValueType::Array { selected, .. } => schema
                        .and_then(|s| s.inputs.iter().find(|si| si.name == input.name))
                        .and_then(|si| si.kind.enum_typed_value(selected))
                        .cloned()
                        .unwrap_or_else(|| json!(selected)),
                    FlowValueType::String { value, .. } => json!(value),
                    FlowValueType::Float { value, .. } => json!(value),
                    FlowValueType::SignedInt { value, .. } => json!(value),
                    FlowValueType::UnsignedInt { value, .. } => json!(value),
                    FlowValueType::Boolean(b) => json!(b),
                    _ => continue,
                };
                widgets_values.push(value);
                // The web frontend expects the phantom control value after these ints.
                if schema
                    .and_then(|s| s.inputs.iter().find(|si| si.name == input.name))
                    .is_some_and(crate::uiwf::takes_seed_control)
                {
                    let control = if seed_randomize
                        .get(&(id, input.name.clone()))
                        .copied()
                        .unwrap_or(false)
                    {
                        "randomize"
                    } else {
                        "fixed"
                    };
                    widgets_values.push(json!(control));
                }
            }

            let size = self.sizes.get(&id).copied().unwrap_or(egui::vec2(240.0, 120.0));
            nodes.push(json!({
                "id": node_id(id),
                "type": data.object.name,
                "pos": [pos.x, pos.y],
                "size": [size.x, size.y],
                "flags": {},
                "order": order,
                "mode": if bypassed.contains(&id) { 4 } else { 0 },
                "inputs": inputs,
                "outputs": outputs,
                "properties": { "Node name for S&R": data.object.name },
                "widgets_values": widgets_values,
            }));
        }

        json!({
            "last_node_id": last_node,
            "last_link_id": last_link,
            "nodes": nodes,
            "links": link_rows,
            "groups": [],
            "config": {},
            "extra": {},
            "version": 0.4,
        })
    }
}

/// The server-side name of an [`ObjectType`] (its serde rename; `Other` is untagged).
pub fn type_str(typ: &rucomfyui::object_info::ObjectType) -> String {
    serde_json::to_value(typ)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "*".to_string())
}

// ── Combo / LoRA helpers ──────────────────────────────────────────────────────

/// Fill empty (or under-populated) file combos from `object_info` and the Create-tab LoRA list.
pub fn ensure_file_combos(
    data: &mut FlowNodeData,
    object_info: &rucomfyui::object_info::ObjectInfo,
    lora_files: &[String],
) {
    let class = data.object.name.clone();
    let template = object_info.get(&class);
    for input in &mut data.inputs {
        let from_template = template.and_then(|obj| {
            obj.all_inputs().find(|(n, _, _)| *n == input.name).and_then(|(_, inp, _)| {
                match inp.as_input_type() {
                    rucomfyui::object_info::ObjectInputType::Array(vec) => {
                        let opts: Vec<String> =
                            vec.iter().map(|v| v.as_str().to_string()).collect();
                        (!opts.is_empty()).then_some(opts)
                    }
                    _ => None,
                }
            })
        });
        let is_lora = input.name == "lora_name"
            || class == "LoraLoader"
            || class == "LoraLoaderModelOnly";
        let mut opts = from_template.unwrap_or_default();
        if is_lora {
            for l in lora_files {
                if !opts.iter().any(|o| o == l) {
                    opts.push(l.clone());
                }
            }
        }
        if opts.is_empty() {
            continue;
        }
        match &mut input.value {
            FlowValueType::Array { options, selected } => {
                if options.is_empty() || (is_lora && options.len() < opts.len()) {
                    *options = opts;
                }
                // A value that misses the list snaps only when it numerically names an option
                // (`1` for the `1.0` entry). Anything else keeps what the workflow stored: an
                // uninstalled model is preflight's to report, not ours to silently replace.
                if !options.iter().any(|o| o == selected) {
                    if let Some(m) = numeric_option(options, selected) {
                        *selected = m;
                    } else if selected.is_empty() {
                        *selected = options.first().cloned().unwrap_or_default();
                    }
                }
            }
            // Empty COMBO parsed as a connection pin — promote to a real dropdown.
            other if is_lora && other.is_connection_only() => {
                let selected = opts.first().cloned().unwrap_or_default();
                input.value = FlowValueType::Array { options: opts, selected };
                input.typ = rucomfyui::object_info::ObjectType::String;
            }
            _ => {}
        }
    }
}

/// The option `value` names numerically, for combos whose options are numbers (`"1"` -> `"1.0"`).
fn numeric_option(options: &[String], value: &str) -> Option<String> {
    let n: f64 = value.trim().parse().ok()?;
    options.iter().find(|o| o.trim().parse::<f64>().is_ok_and(|f| f == n)).cloned()
}

/// Whichever of [`PICK_INPUTS`] this node carries, and its current selection.
fn picked_file_of(data: &FlowNodeData) -> Option<(&'static str, String)> {
    PICK_INPUTS.iter().find_map(|name| {
        let i = data.inputs.iter().find(|i| i.name == *name)?;
        match &i.value {
            FlowValueType::Array { selected, .. } => Some((*name, selected.clone())),
            _ => None,
        }
    })
}

fn picked_file(snarl: &Snarl<FlowNodeData>, node: NodeId) -> Option<(&'static str, String)> {
    picked_file_of(snarl.get_node(node)?)
}

/// A pick, when this frame's selection differs from `before`. An empty selection raises nothing —
/// clearing a combo is not a request to re-seed the graph from a model that is no longer chosen.
fn pick_changed(
    node: NodeId,
    before: Option<&(&'static str, String)>,
    after: Option<(&'static str, String)>,
) -> Option<ModelPick> {
    let (input, file) = after?;
    if before.map(|(_, f)| f.as_str()) == Some(file.as_str()) || file.is_empty() {
        return None;
    }
    Some(ModelPick { node, input, file })
}

/// A checkpoint's recommended settings, already resolved against this server's option lists.
///
/// Name matching stays with the caller: only the Create tab knows the live `samplers`/`schedulers`,
/// and a name this server does not offer must be dropped rather than written into a combo whose
/// options do not contain it.
#[derive(Default)]
pub struct GraphDefaults<'a> {
    pub steps: Option<u32>,
    pub cfg: Option<f32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sampler: Option<&'a str>,
    pub scheduler: Option<&'a str>,
    pub clip_skip: Option<u32>,
}

/// Apply `d` across the graph the way the Create tab applies it to its params, and report what
/// actually moved so the caller can say so rather than claiming a silent success.
///
/// Sizes are written only to latent *sources*. `width`/`height` are common input names — an upscale
/// node has them too — and seeding a 896×1152 recommendation into an upscale would quietly resize
/// the wrong stage of the pipeline.
pub fn apply_defaults(snarl: &mut Snarl<FlowNodeData>, d: &GraphDefaults<'_>) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    let mut note = |s: String| {
        if !changed.contains(&s) {
            changed.push(s);
        }
    };
    for data in snarl.nodes_mut() {
        let class = data.object.name.clone();
        let is_latent_source = class.starts_with("Empty") && class.contains("Latent");
        for input in &mut data.inputs {
            let hit = match input.name.as_str() {
                "steps" => d.steps.filter(|v| set_int_input(&mut input.value, *v as i64)).map(|v| format!("steps {v}")),
                "width" if is_latent_source => d
                    .width
                    .filter(|v| set_int_input(&mut input.value, *v as i64))
                    .map(|v| format!("width {v}")),
                "height" if is_latent_source => d
                    .height
                    .filter(|v| set_int_input(&mut input.value, *v as i64))
                    .map(|v| format!("height {v}")),
                // The graph stores what ComfyUI stores: a NEGATIVE layer index. `clip_skip` counts
                // back from the end, so 2 means `-2` — see `workflow::build`.
                "stop_at_clip_layer" => d
                    .clip_skip
                    .filter(|v| *v >= 1 && set_int_input(&mut input.value, -((*v).min(24) as i64)))
                    .map(|v| format!("CLIP skip {v}")),
                "cfg" => match (&d.cfg, &mut input.value) {
                    (Some(v), FlowValueType::Float { value, min, max, .. }) => {
                        *value = (*v as f64).clamp(*min, *max);
                        Some(format!("CFG {v}"))
                    }
                    _ => None,
                },
                "sampler_name" => set_option(&mut input.value, d.sampler).map(|v| format!("sampler {v}")),
                "scheduler" => set_option(&mut input.value, d.scheduler).map(|v| format!("scheduler {v}")),
                _ => None,
            };
            if let Some(msg) = hit {
                note(msg);
            }
        }
    }
    changed
}

/// Write `n` into whichever integer variant this input uses, clamped to its own range. `false` when
/// the input is not an integer at all, which is how a name collision on another node is skipped.
fn set_int_input(value: &mut FlowValueType, n: i64) -> bool {
    match value {
        FlowValueType::UnsignedInt { value, min, max, .. } => {
            *value = (n.max(0) as u64).clamp(*min, *max);
            true
        }
        FlowValueType::SignedInt { value, min, max, .. } => {
            *value = n.clamp(*min, *max);
            true
        }
        _ => false,
    }
}

/// Select `want` on an enum input, but only if this server actually offers it — writing a name
/// that is not in `options` leaves a combo displaying a value it cannot round-trip.
fn set_option<'a>(value: &mut FlowValueType, want: Option<&'a str>) -> Option<&'a str> {
    let want = want?;
    match value {
        FlowValueType::Array { options, selected } if options.iter().any(|o| o == want) => {
            *selected = want.to_string();
            Some(want)
        }
        _ => None,
    }
}

/// The checkpoint or diffusion model this graph loads, if a loader has one selected. LoRAs are
/// excluded: they are not what a model recommendation or a family quality block keys off.
pub fn graph_model_file(snarl: &Snarl<FlowNodeData>) -> Option<String> {
    snarl.nodes_pos_ids().find_map(|(_, _, data)| match picked_file_of(data) {
        Some((input, file)) if input != "lora_name" && !file.is_empty() => Some(file),
        _ => None,
    })
}

/// The `(positive, negative)` prompt nodes a sampler reads.
///
/// Found by walking the sampler's conditioning inputs back to the first node upstream carrying a
/// `text` widget, rather than by taking the first two `CLIPTextEncode`s in the graph: which encode
/// is positive is a wiring fact, and guessing it puts quality tags in the negative prompt.
pub fn prompt_nodes(snarl: &Snarl<FlowNodeData>) -> (Option<NodeId>, Option<NodeId>) {
    /// Conditioning can pass through combiners/controlnets before the encode; bounded so a cycle
    /// or a long chain cannot spin.
    const MAX_HOPS: usize = 8;
    let walk = |start: NodeId, input: &str| -> Option<NodeId> {
        let mut node = start;
        let mut want = input.to_string();
        for _ in 0..MAX_HOPS {
            let data = snarl.get_node(node)?;
            let idx = data.inputs.iter().position(|i| i.name == want)?;
            let remote = *snarl.in_pin(InPinId { node, input: idx }).remotes.first()?;
            let src = snarl.get_node(remote.node)?;
            if src.inputs.iter().any(|i| {
                i.name == "text" && matches!(i.value, FlowValueType::String { .. })
            }) {
                return Some(remote.node);
            }
            // Keep climbing the first conditioning input this node has.
            let next = src
                .inputs
                .iter()
                .find(|i| i.name.starts_with("conditioning") || i.name == "positive")?;
            want = next.name.clone();
            node = remote.node;
        }
        None
    };
    let sampler = snarl.nodes_pos_ids().find(|(_, _, d)| {
        d.inputs.iter().any(|i| i.name == "positive") && d.inputs.iter().any(|i| i.name == "negative")
    });
    match sampler {
        Some((id, _, _)) => (walk(id, "positive"), walk(id, "negative")),
        None => (None, None),
    }
}

/// Read a node's `text` widget.
pub fn prompt_text(snarl: &Snarl<FlowNodeData>, node: NodeId) -> Option<String> {
    let data = snarl.get_node(node)?;
    data.inputs.iter().find(|i| i.name == "text").and_then(|i| match &i.value {
        FlowValueType::String { value, .. } => Some(value.clone()),
        _ => None,
    })
}

/// Overwrite a node's `text` widget. `false` when the node has none.
pub fn set_prompt_text(snarl: &mut Snarl<FlowNodeData>, node: NodeId, text: String) -> bool {
    let Some(data) = snarl.get_node_mut(node) else { return false };
    for input in &mut data.inputs {
        if input.name == "text"
            && let FlowValueType::String { value, .. } = &mut input.value
        {
            *value = text;
            return true;
        }
    }
    false
}

/// Write catalog strengths onto a LoRA node's strength widgets.
pub fn apply_lora_strengths(data: &mut FlowNodeData, strength_model: f32, strength_clip: f32) {
    for input in &mut data.inputs {
        match (input.name.as_str(), &mut input.value) {
            ("strength_model", FlowValueType::Float { value, min, max, .. }) => {
                *value = (strength_model as f64).clamp(*min, *max);
            }
            ("strength_clip", FlowValueType::Float { value, min, max, .. }) => {
                *value = (strength_clip as f64).clamp(*min, *max);
            }
            _ => {}
        }
    }
}

// ── Node properties editor ────────────────────────────────────────────────────

/// Inspector for one node: type/category header, every input (connection source or editable
/// value), and outputs. Returns `false` when the node no longer exists.
/// `model_picks` collects model-file changes for recommended-settings application.
pub fn node_properties(
    ui: &mut egui::Ui,
    g: &mut ComfyUiNodeGraph,
    node: NodeId,
    locked: bool,
    lora_files: &[String],
    model_picks: &mut Vec<ModelPick>,
    seed_randomize: &mut HashMap<(NodeId, String), bool>,
) -> bool {
    let Some(data) = g.snarl.get_node(node) else { return false };
    // Captured before the editors run, so a change made below can be told from a repaint.
    let before = picked_file_of(data);

    // Connection sources, resolved before taking the node mutably.
    let sources: Vec<Option<String>> = (0..data.inputs.len())
        .map(|i| {
            let pin = g.snarl.in_pin(InPinId { node, input: i });
            pin.remotes.first().map(|r| {
                let Some(src) = g.snarl.get_node(r.node) else { return "?".to_string() };
                match src.outputs.get(r.output) {
                    Some(out) => format!("{} / {}", src.object.display_name(), out.name),
                    None => src.object.display_name().to_string(),
                }
            })
        })
        .collect();

    let Some(data) = g.snarl.get_node_mut(node) else { return false };
    ensure_file_combos(data, &g.object_info, lora_files);
    ui.strong(sanitize_ui_text(ui, data.object.display_name()));
    ui.weak(sanitize_ui_text(
        ui,
        &format!("{}  •  {}", data.object.name, data.object.category),
    ));
    if !data.object.description.is_empty() {
        ui.add(
            egui::Label::new(
                egui::RichText::new(elide(&sanitize_ui_text(ui, &data.object.description), 300))
                    .weak()
                    .small(),
            )
            .wrap(),
        );
    }
    ui.separator();

    ui.strong("Inputs");
    for (i, input) in data.inputs.iter_mut().enumerate() {
        match &sources[i] {
            Some(src) => {
                ui.horizontal(|ui| {
                    ui.label(&input.name);
                    ui.weak(format!("<- {}", elide(&sanitize_ui_text(ui, src), 40)));
                });
            }
            None => {
                value_editor(ui, egui::Id::new((node, i)), node, input, locked, seed_randomize);
            }
        }
    }
    // One check for the whole node rather than one per input: the editors above have run, so this
    // sees the settled selection, and a node carries at most one model-file input anyway.
    if let Some(pick) = pick_changed(node, before.as_ref(), picked_file_of(data)) {
        model_picks.push(pick);
    }

    if !data.outputs.is_empty() {
        ui.add_space(6.0);
        ui.strong("Outputs");
        for out in &data.outputs {
            ui.horizontal(|ui| {
                ui.label(&out.name);
                ui.weak(format!("{:?}", out.typ));
            });
        }
    }
    true
}

/// One editable input row, mirroring the widgets the node body renders.
fn value_editor(
    ui: &mut egui::Ui,
    salt: egui::Id,
    node: NodeId,
    input: &mut FlowInput,
    locked: bool,
    seed_randomize: &mut HashMap<(NodeId, String), bool>,
) {
    if is_seed_widget(input) {
        let key = (node, input.name.clone());
        let mut randomize = seed_randomize.get(&key).copied().unwrap_or(false);
        ui.add_enabled_ui(!locked, |ui| {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.add_enabled_ui(!randomize, |ui| match &mut input.value {
                    FlowValueType::UnsignedInt { value, min, max, step } => {
                        ui.add(
                            egui::DragValue::new(value)
                                .range(*min..=*max)
                                .speed((*step as f64).max(1.0)),
                        );
                    }
                    FlowValueType::SignedInt { value, min, max, step } => {
                        ui.add(
                            egui::DragValue::new(value)
                                .range(*min..=*max)
                                .speed((*step as f64).max(1.0)),
                        );
                    }
                    _ => {}
                });
                if ui.checkbox(&mut randomize, "random").changed() {
                    seed_randomize.insert(key, randomize);
                }
            });
        });
        return;
    }
    ui.add_enabled_ui(!locked, |ui| match &mut input.value {
        FlowValueType::Array { options, selected } => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                option_combo(ui, salt, selected, options, PROPS_COMBO_W);
            });
        }
        FlowValueType::String { value, multiline } => {
            // Label above, field to the visible right edge. Prefer clip_rect over available_width:
            // inside a vertical ScrollArea the latter grows with content and the field overruns.
            // (This is the Properties pane — a normal panel, where both are in screen units. The
            // canvas is the opposite case: see [`NODE_FIELD_W`].)
            ui.label(&input.name);
            let width = (ui.clip_rect().right() - ui.cursor().left() - 8.0).max(48.0);
            let edit = if *multiline {
                egui::TextEdit::multiline(value).desired_rows(3)
            } else {
                egui::TextEdit::singleline(value)
            };
            ui.scope(|ui| {
                ui.set_max_width(width);
                ui.add(edit.desired_width(width).clip_text(true));
            });
        }
        FlowValueType::Float { value, min, max, step, .. } => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.add(
                    egui::DragValue::new(value).range(*min..=*max).speed(step.max(0.001)),
                );
            });
        }
        FlowValueType::SignedInt { value, min, max, step } => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.add(
                    egui::DragValue::new(value).range(*min..=*max).speed((*step as f64).max(1.0)),
                );
            });
        }
        FlowValueType::UnsignedInt { value, min, max, step } => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.add(
                    egui::DragValue::new(value).range(*min..=*max).speed((*step as f64).max(1.0)),
                );
            });
        }
        FlowValueType::Boolean(value) => {
            ui.checkbox(value, &input.name);
        }
        _ => {
            ui.horizontal(|ui| {
                ui.label(&input.name);
                ui.weak("connection");
            });
        }
    });
}

/// Dropdown over a possibly-huge option list: filters by substring and caps rendered rows.
fn option_combo(
    ui: &mut egui::Ui,
    salt: egui::Id,
    selected: &mut String,
    options: &[String],
    max_w: f32,
) {
    // Elided to a WIDTH, not a character count: a ComboBox lays its selected text out at infinite
    // width in `TextWrapMode::Extend` and allocates the result, so on the canvas a long
    // `SDXL/juggernautXL_version9Rundiffusion.safetensors` blows straight past the node's width cap
    // and widens its whole arrange column (measured: long model names alone were 19% of a txt2img
    // graph's horizontal span).
    egui::ComboBox::from_id_salt(salt)
        .selected_text(elide_width(ui, &sanitize_ui_text(ui, selected), max_w))
        // egui defaults a ComboBox popup to `CloseOnClick`, which counts a click on the filter
        // field as a click and shuts the popup the instant you try to type in it — leaving the
        // list stuck on whatever its first screenful happens to be. Close on an outside click
        // instead, and close explicitly below when an option is actually chosen.
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            let filter_id = salt.with("filter");
            let mut filter: String =
                ui.ctx().data_mut(|d| d.get_temp(filter_id)).unwrap_or_default();
            if options.len() > 12 {
                ui.add(egui::TextEdit::singleline(&mut filter).hint_text("filter"));
                ui.ctx().data_mut(|d| d.insert_temp(filter_id, filter.clone()));
            }
            let f = filter.to_lowercase();
            let matches: Vec<&String> = options
                .iter()
                .filter(|o| f.is_empty() || o.to_lowercase().contains(&f))
                .collect();
            if matches.is_empty() {
                ui.weak("no matches");
                return;
            }
            // Every match is reachable — the old 200-row cap silently hid the tail of a big model
            // library, and with the filter broken there was no way to reach past it. Showing a
            // count makes a long list legible rather than merely long.
            if matches.len() > 12 {
                ui.weak(format!("{} of {}", matches.len(), options.len()));
            }
            for opt in matches {
                let row = crate::theme::selectable_value(
                    ui,
                    selected,
                    opt.clone(),
                    elide(&sanitize_ui_text(ui, opt), 48),
                );
                if row.clicked() {
                    ui.close();
                }
            }
        });
}

/// Shorten a string for display so a pathological value can't blow up text layout.
pub fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Drop control chars and glyphs the active font cannot draw (space instead of tofu/`?`).
pub fn sanitize_ui_text(ui: &egui::Ui, s: &str) -> String {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    ui.ctx().fonts_mut(|fonts| {
        for c in s.chars() {
            let ok = !c.is_control() && !c.is_whitespace() && fonts.has_glyph(&font, c);
            if ok {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                out.push(c);
                pending_space = false;
            } else {
                pending_space = true;
            }
        }
    });
    out
}

/// Truncate `s` so its laid-out width fits within `max_width` (appends `…` when cut).
pub fn elide_width(ui: &egui::Ui, s: &str, max_width: f32) -> String {
    let s = sanitize_ui_text(ui, s);
    if s.is_empty() {
        return String::new();
    }
    if max_width <= 12.0 {
        return "…".into();
    }
    let font = egui::TextStyle::Body.resolve(ui.style());
    let measure = |text: &str| {
        ui.ctx()
            .fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE).size().x)
    };
    if measure(&s) <= max_width {
        return s;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let candidate: String = chars[..mid].iter().chain(std::iter::once(&'…')).collect();
        if measure(&candidate) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        "…".into()
    } else {
        chars[..lo].iter().chain(std::iter::once(&'…')).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The symptom this exists for: a saved video went into MediaStore's Images collection because
    /// the MIME was guessed from a two-entry list, and a video filed under images never appears in
    /// the phone's gallery. Every extension the pickers accept has to answer with its real type.
    #[test]
    fn every_pickable_extension_has_its_real_mime() {
        for ext in PICK_VIDEO_EXT {
            let mime = media_mime(&format!("clip.{ext}"));
            if ext == "gif" {
                assert_eq!(mime, "image/gif", "an animated gif is still an Images row");
                continue;
            }
            assert!(mime.starts_with("video/"), "{ext} must be a video MIME, got {mime}");
        }
        for ext in PICK_IMAGE_EXT {
            let mime = media_mime(&format!("still.{ext}"));
            assert!(mime.starts_with("image/"), "{ext} must be an image MIME, got {mime}");
        }
        // Names the pickers annotate, and names with no extension at all, must not read as video.
        assert_eq!(media_mime("ComfyUI_00042_.png [output]"), "image/png");
        assert_eq!(media_mime("no-extension"), "image/png");
    }

    /// The editor builds its widgets from `/object_info`, so a VideoHelperSuite node's
    /// format-dependent encoder settings have no widget to live in and are dropped on load. They
    /// ride beside the graph instead and are re-attached to the prompt at queue time — asserted on
    /// what the wire carries, not on what `convert` produced.
    #[test]
    fn undeclared_widgets_survive_the_editor_round_trip_to_the_wire() {
        let schemas = crate::schema::parse(
            &serde_json::from_str(
                r#"{
                "LoadImage": {"input": {"required": {"image": [["a.png"]]}},
                    "output": ["IMAGE","MASK"], "output_name": ["IMAGE","MASK"], "output_is_list": [false,false]},
                "VHS_VideoCombine": {"input": {"required": {
                    "images": ["IMAGE"],
                    "frame_rate": ["FLOAT", {"default": 8.0}],
                    "filename_prefix": ["STRING", {"default": "AnimateDiff"}],
                    "format": [["video/h264-mp4"]],
                    "save_output": ["BOOLEAN", {"default": true}]
                }}, "output": [], "output_name": [], "output_is_list": []}}"#,
            )
            .unwrap(),
        );
        let ui = serde_json::json!({
            "nodes": [
                {"id": 1, "type": "LoadImage", "mode": 0,
                 "outputs": [{"name": "IMAGE", "type": "IMAGE", "links": [5]}],
                 "widgets_values": ["a.png"]},
                {"id": 2, "type": "VHS_VideoCombine", "mode": 0,
                 "inputs": [{"name": "images", "type": "IMAGE", "link": 5}],
                 "widgets_values": {
                    "frame_rate": 32, "filename_prefix": "vid", "format": "video/h264-mp4",
                    "save_output": true, "crf": 30, "pix_fmt": "yuv420p10le",
                    "save_metadata": false, "trim_to_audio": true,
                    "videopreview": {"hidden": false, "params": {"filename": "x.mp4"}}
                 }}
            ],
            "links": [[5, 1, 0, 2, 0, "IMAGE"]]
        });

        // Load: convert, then park the undeclared widgets against their snarl ids.
        let loaded = crate::uiwf::convert(&ui, &schemas).unwrap();
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        graph.load_api_workflow(&loaded.workflow).unwrap();
        let mut parked = HashMap::new();
        apply_extra_widgets_from_workflow(
            &graph.snarl,
            &loaded.workflow,
            &loaded.extra_widgets,
            &mut parked,
        );
        assert_eq!(parked.len(), 4, "parked: {parked:?}");

        // Queue: export, convert, then re-attach exactly as queue_graph does.
        let view = GraphView::default();
        let exported = view.export_ui(&graph, &schemas, &HashSet::new(), &HashMap::new());
        let mut wf = crate::uiwf::convert(&exported, &schemas).unwrap().workflow;
        for ((nid, name), (class, v)) in &parked {
            let wid = rucomfyui::workflow::WorkflowNodeId((nid.0 as u32).saturating_add(1));
            let Some(node) = wf.0.get_mut(&wid) else { continue };
            if node.class_type != *class || node.inputs.contains_key(name) {
                continue;
            }
            if let Some(wi) = crate::preflight::input_of(v) {
                node.inputs.insert(name.clone(), wi);
            }
        }

        let vhs = wf.0.values().find(|n| n.class_type == "VHS_VideoCombine").expect("no VHS");
        use rucomfyui::workflow::WorkflowInput;
        assert_eq!(vhs.inputs["crf"], WorkflowInput::I64(30));
        assert_eq!(vhs.inputs["pix_fmt"], WorkflowInput::String("yuv420p10le".into()));
        assert_eq!(vhs.inputs["save_metadata"], WorkflowInput::Boolean(false));
        assert_eq!(vhs.inputs["trim_to_audio"], WorkflowInput::Boolean(true));
        assert!(!vhs.inputs.contains_key("videopreview"));
        // The declared widgets still come through the editor as before.
        assert_eq!(vhs.inputs["filename_prefix"], WorkflowInput::String("vid".into()));
        assert_eq!(vhs.inputs["frame_rate"], WorkflowInput::F64(32.0));
    }

    /// What `build_video` emits for `RIFE VFI.scale_factor` is a number, and the editor can only
    /// bind a dropdown to a string — without the display pass the widget becomes an empty text box
    /// and the queue sends `""`. Asserted end to end, on the value that reaches the wire.
    #[test]
    fn a_numeric_combo_survives_open_as_graph() {
        let schemas = crate::schema::parse(
            &serde_json::from_str(
                r#"{"RIFE VFI": {"input": {"required": {
                    "ckpt_name": [["rife49.pth"]],
                    "scale_factor": [[0.25, 0.5, 1.0, 2.0, 4.0], {"default": 1.0}]
                }}, "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        );
        use rucomfyui::workflow::{WorkflowInput, WorkflowNode, WorkflowNodeId};
        let mut built = rucomfyui::Workflow::new([(WorkflowNodeId(1), {
            let mut n = WorkflowNode::new("RIFE VFI");
            n.add_input("ckpt_name".to_string(), WorkflowInput::String("rife49.pth".into()));
            // Exactly what workflow.rs's video builder emits.
            n.add_input("scale_factor".to_string(), WorkflowInput::I64(1));
            n
        })]);

        crate::preflight::display_combo_values(&mut built, &schemas);
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        graph.load_api_workflow(&built).unwrap();
        // The widget is still a dropdown, on the right option.
        let data = graph.snarl.node_ids().next().unwrap().1;
        let input = data.inputs.iter().find(|i| i.name == "scale_factor").unwrap();
        match &input.value {
            FlowValueType::Array { selected, options } => {
                assert_eq!(selected, "1.0");
                assert_eq!(options.len(), 5);
            }
            other => panic!("dropdown collapsed to {other:?}"),
        }

        let view = GraphView::default();
        let exported = view.export_ui(&graph, &schemas, &HashSet::new(), &HashMap::new());
        let mut wf = crate::uiwf::convert(&exported, &schemas).unwrap().workflow;
        crate::preflight::retype_combo_values(&mut wf, &schemas);
        let rife = wf.0.values().find(|n| n.class_type == "RIFE VFI").expect("no RIFE");
        assert_eq!(rife.inputs["scale_factor"], WorkflowInput::F64(1.0));
    }

    /// A model recommendation seeds the sampler, but `width`/`height` are not sampler-specific
    /// names — an upscale node has them too, and writing a 896×1152 recommendation into one would
    /// silently resize the wrong stage of the pipeline. Sizes must reach latent *sources* only.
    ///
    /// Also pins the CLIP-skip sign: the graph stores ComfyUI's negative layer index, so a
    /// recommendation of `2` has to land as `-2`.
    #[test]
    fn recommended_defaults_reach_the_sampler_and_the_latent_but_not_an_upscale() {
        let schemas = crate::schema::parse(
            &serde_json::from_str(
                r#"{
                  "KSampler": {"input": {"required": {
                    "steps": ["INT", {"default": 20, "min": 1, "max": 200}],
                    "cfg": ["FLOAT", {"default": 8.0, "min": 0.0, "max": 100.0}],
                    "sampler_name": [["euler", "dpmpp_2m"]],
                    "scheduler": [["normal", "karras"]]
                  }}, "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
                  "EmptyLatentImage": {"input": {"required": {
                    "width": ["INT", {"default": 512, "min": 16, "max": 4096}],
                    "height": ["INT", {"default": 512, "min": 16, "max": 4096}]
                  }}, "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
                  "ImageScale": {"input": {"required": {
                    "width": ["INT", {"default": 1024, "min": 16, "max": 4096}],
                    "height": ["INT", {"default": 1024, "min": 16, "max": 4096}]
                  }}, "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]},
                  "CLIPSetLastLayer": {"input": {"required": {
                    "stop_at_clip_layer": ["INT", {"default": -1, "min": -24, "max": -1}]
                  }}, "output": ["CLIP"], "output_name": ["CLIP"], "output_is_list": [false]}
                }"#,
            )
            .unwrap(),
        );
        let oi = crate::schema::to_object_info(&schemas);
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        for class in ["KSampler", "EmptyLatentImage", "ImageScale", "CLIPSetLastLayer"] {
            snarl.insert_node(egui::pos2(0.0, 0.0), FlowNodeData::new(oi[class].clone()));
        }
        let changed = apply_defaults(
            &mut snarl,
            &GraphDefaults {
                steps: Some(36),
                cfg: Some(6.5),
                width: Some(896),
                height: Some(1152),
                sampler: Some("dpmpp_2m"),
                scheduler: Some("karras"),
                clip_skip: Some(2),
            },
        );

        fn value_of(snarl: &Snarl<FlowNodeData>, class: &str, input: &str) -> FlowValueType {
            snarl
                .nodes_pos_ids()
                .find(|(_, _, d)| d.object.name == class)
                .and_then(|(_, _, d)| d.inputs.iter().find(|i| i.name == input))
                .map(|i| i.value.clone())
                .expect("missing input")
        }
        let int_of = |snarl: &Snarl<FlowNodeData>, class: &str, input: &str| -> i64 {
            match value_of(snarl, class, input) {
                FlowValueType::UnsignedInt { value, .. } => value as i64,
                FlowValueType::SignedInt { value, .. } => value,
                other => panic!("{class}.{input} is {other:?}"),
            }
        };

        assert_eq!(int_of(&snarl, "KSampler", "steps"), 36);
        assert_eq!(int_of(&snarl, "EmptyLatentImage", "width"), 896);
        assert_eq!(int_of(&snarl, "EmptyLatentImage", "height"), 1152);
        // The whole point: the upscale keeps its own size.
        assert_eq!(int_of(&snarl, "ImageScale", "width"), 1024);
        assert_eq!(int_of(&snarl, "ImageScale", "height"), 1024);
        assert_eq!(int_of(&snarl, "CLIPSetLastLayer", "stop_at_clip_layer"), -2);
        match value_of(&snarl, "KSampler", "sampler_name") {
            FlowValueType::Array { selected, .. } => assert_eq!(selected, "dpmpp_2m"),
            other => panic!("sampler_name is {other:?}"),
        }

        // A sampler this server does not offer is dropped rather than written into the combo.
        let none = apply_defaults(
            &mut snarl,
            &GraphDefaults { sampler: Some("res_multistep"), ..Default::default() },
        );
        assert!(none.is_empty(), "unknown sampler was written: {none:?}");
        match value_of(&snarl, "KSampler", "sampler_name") {
            FlowValueType::Array { selected, .. } => assert_eq!(selected, "dpmpp_2m"),
            other => panic!("sampler_name is {other:?}"),
        }
        // The report names what moved, so the status line cannot claim a silent success.
        assert!(changed.iter().any(|c| c.contains("steps 36")), "{changed:?}");
        assert!(changed.iter().any(|c| c.contains("CLIP skip 2")), "{changed:?}");
    }

    /// `ensure_file_combos` runs over every node every frame, so a value it does not recognise is
    /// the last chance to lose the user's setting before it reaches the wire. A numeric spelling
    /// snaps to the option it names; an unknown one survives for preflight to report.
    #[test]
    fn a_combo_value_is_never_silently_swapped_for_the_first_option() {
        let schemas = crate::schema::parse(
            &serde_json::from_str(
                r#"{"RIFE VFI": {"input": {"required": {
                    "ckpt_name": [["rife49.pth", "rife47.pth"]],
                    "scale_factor": [[0.25, 0.5, 1.0, 2.0, 4.0], {"default": 1.0}]
                }}, "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        );
        let object_info = crate::schema::to_object_info(&schemas);
        let selected_after = |input: &str, stored: &str| -> String {
            let mut data = FlowNodeData::new(object_info["RIFE VFI"].clone());
            let slot = data.inputs.iter_mut().find(|i| i.name == input).expect("no such input");
            let FlowValueType::Array { selected, .. } = &mut slot.value else { panic!("not a combo") };
            *selected = stored.to_string();
            ensure_file_combos(&mut data, &object_info, &[]);
            match &data.inputs.iter().find(|i| i.name == input).unwrap().value {
                FlowValueType::Array { selected, .. } => selected.clone(),
                other => panic!("combo became {other:?}"),
            }
        };

        // The integer spelling of a float option is the same choice, so it snaps to that option.
        assert_eq!(selected_after("scale_factor", "1"), "1.0");
        assert_eq!(selected_after("scale_factor", "1.0"), "1.0");
        assert_eq!(selected_after("scale_factor", "2"), "2.0");
        // A value naming no option is kept: swapping it for 0.25 would run settings nobody chose.
        assert_eq!(selected_after("scale_factor", "8.0"), "8.0");
        assert_eq!(selected_after("ckpt_name", "rife422.pth"), "rife422.pth");
        // Nothing stored at all still needs a default to render.
        assert_eq!(selected_after("scale_factor", ""), "0.25");
    }

    /// Headless repro harness: load a real workflow (fixture env vars) and sweep taps across the
    /// canvas — egui hit-testing runs on the pointer events, so widget-soup panics surface here.
    #[test]
    fn tap_sweep_over_loaded_workflow() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = crate::schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));

        for wf_path in wf_paths.split(':') {
            let ui_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(wf_path).unwrap()).unwrap();
            let converted = crate::uiwf::convert(&ui_json, &schemas).unwrap();
            graph.load_api_workflow(&converted.workflow).unwrap();

            let mut view = GraphView::default();
            view.request_fit();
            let ctx = egui::Context::default();
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 840.0));

            let mut frame_no = 0u32;
            let mut frame = |view: &mut GraphView,
                             graph: &mut ComfyUiNodeGraph,
                             events: Vec<egui::Event>|
             -> Option<NodeId> {
                frame_no += 1;
                let desc = format!("{events:?}");
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                };
                let mut tapped = None;
                // Measurement pass only — nothing uploads the textures, and epaint panics on an unapplied delta.
                ctx.run_ui(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        tapped = view.show(
                            ui,
                            graph,
                            None,
                            None,
                            &HashSet::new(),
                            &[],
                            &mut HashMap::new(),
                        );
                    });
                }).textures_delta.clear();
                for (id, pos, data) in graph.snarl.nodes_pos_ids() {
                    assert!(
                        pos.x.is_finite() && pos.y.is_finite(),
                        "frame {frame_no} ({desc}): node {id:?} ({}) pos went NaN",
                        data.object.name
                    );
                }
                for (id, size) in view.sizes.iter() {
                    assert!(
                        size.x.is_finite() && size.y.is_finite(),
                        "frame {frame_no} ({desc}): node {id:?} size went NaN: {size:?}"
                    );
                }
                assert!(
                    view.to_global.scaling.is_finite() && view.to_global.translation.x.is_finite(),
                    "frame {frame_no} ({desc}): transform NaN: {:?}",
                    view.to_global
                );
                tapped
            };
            let tap = |view: &mut GraphView,
                       graph: &mut ComfyUiNodeGraph,
                       frame: &mut dyn FnMut(
                &mut GraphView,
                &mut ComfyUiNodeGraph,
                Vec<egui::Event>,
            ) -> Option<NodeId>,
                       pos: egui::Pos2|
             -> Option<NodeId> {
                frame(view, graph, vec![egui::Event::PointerMoved(pos)]);
                frame(view, graph, vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                }]);
                frame(view, graph, vec![
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    },
                    egui::Event::PointerGone,
                ])
            };

            frame(&mut view, &mut graph, vec![]);
            frame(&mut view, &mut graph, vec![]);
            // 5x9 tap grid over the canvas: press, release, lift between taps.
            for gy in 0..9 {
                for gx in 0..5 {
                    let pos = egui::pos2(30.0 + gx as f32 * 90.0, 40.0 + gy as f32 * 88.0);
                    tap(&mut view, &mut graph, &mut frame, pos);
                }
            }
            // Dismiss any popup a sweep tap left open, then targeted-tap a known node header:
            // it must focus exactly that node.
            for pressed in [true, false] {
                frame(&mut view, &mut graph, vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::default(),
                }]);
            }
            // Sweep taps may have hit the minimap and panned away; re-fit first.
            view.request_fit();
            frame(&mut view, &mut graph, vec![]);
            // Tap a node whose interior point is on-screen, clear of the corner overlays (minimap
            // top-left, lock top-right), and unambiguously that node (no earlier node covers it).
            let sizes = view.sizes.clone();
            let size_of = |id: NodeId| sizes.get(&id).copied().unwrap_or(NOMINAL_NODE);
            let safe = |p: egui::Pos2| -> bool {
                screen.shrink(8.0).contains(p)
                    && !(p.x < 220.0 && p.y < 220.0)
                    && !(p.x > screen.right() - 60.0 && p.y < 60.0)
            };
            let mut target = None;
            for (id, node_pos, _) in graph.snarl.nodes_pos_ids() {
                let size = size_of(id);
                if size.x < 30.0 || size.y < 26.0 {
                    continue;
                }
                let interior = node_pos + egui::vec2(size.x * 0.4, 13.0);
                let first = graph.snarl.nodes_pos_ids().find(|(id2, p2, _)| {
                    egui::Rect::from_min_size(*p2, size_of(*id2)).contains(interior)
                });
                if first.map(|(i, _, _)| i) == Some(id) && safe(view.to_global * interior) {
                    target = Some((id, view.to_global * interior));
                    break;
                }
            }
            let (want, screen_pt) = target.expect("no unobstructed node to tap");
            let tapped = tap(&mut view, &mut graph, &mut frame, screen_pt);
            assert_eq!(tapped, Some(want), "{wf_path}: targeted tap missed its node");
            println!("{wf_path}: tap sweep ok");
        }
    }

    /// Canonical text for a constant input value: exact for integers, numeric-collapsed for
    /// integral floats (the editor turns `I64(1)` into `F64(1.0)` on float inputs).
    fn norm_value(v: &rucomfyui::workflow::WorkflowInput) -> Option<String> {
        use rucomfyui::workflow::WorkflowInput as W;
        match v {
            W::I64(i) => Some(format!("n{i}")),
            W::U64(u) => Some(format!("n{u}")),
            W::F64(f) if f.fract() == 0.0 && f.abs() < 9e15 => Some(format!("n{}", *f as i64)),
            W::F64(f) => Some(format!("f{f}")),
            W::String(s) => Some(format!("s{s}")),
            W::Boolean(b) => Some(format!("b{b}")),
            _ => None,
        }
    }

    /// Loading a workflow into the editor and saving it straight back must preserve every widget
    /// value. Regression: the editor's u64 heuristic used to wrap `stop_at_clip_layer: -2` into
    /// 18446744073709551614, which the server rejected.
    #[test]
    fn editor_round_trip_preserves_values() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = crate::schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        let is_widget_input = |class: &str, input: &str| {
            schemas.nodes.get(class).is_some_and(|n| {
                n.inputs.iter().any(|i| {
                    i.name == input
                        && !matches!(
                            i.kind,
                            crate::schema::InputKind::Connection { .. }
                                | crate::schema::InputKind::Opaque
                        )
                })
            })
        };
        let collect = |wf: &rucomfyui::Workflow| {
            let mut multiset: HashMap<(String, String, String), i32> = HashMap::new();
            for node in wf.0.values() {
                for (name, input) in &node.inputs {
                    if !is_widget_input(&node.class_type, name) {
                        continue;
                    }
                    if let Some(v) = norm_value(input) {
                        *multiset
                            .entry((node.class_type.clone(), name.clone(), v))
                            .or_default() += 1;
                    }
                }
            }
            multiset
        };

        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        for wf_path in wf_paths.split(':') {
            let ui_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(wf_path).unwrap()).unwrap();
            let converted = crate::uiwf::convert(&ui_json, &schemas).unwrap();
            graph.load_api_workflow(&converted.workflow).unwrap();
            let saved = graph.save_api_workflow();
            let (source, round) = (collect(&converted.workflow), collect(&saved));
            for ((class, input, value), count) in &source {
                let got = round.get(&(class.clone(), input.clone(), value.clone())).copied();
                assert_eq!(
                    got,
                    Some(*count),
                    "{wf_path}: {class}.{input} lost value {value} in the editor round trip"
                );
            }
            println!("{wf_path}: {} values survive the round trip", source.len());
        }
    }

    /// Arrange must produce non-overlapping nodes even with wildly varying node sizes.
    #[test]
    fn arrange_never_overlaps() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = crate::schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        for wf_path in wf_paths.split(':') {
            let ui_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(wf_path).unwrap()).unwrap();
            let converted = crate::uiwf::convert(&ui_json, &schemas).unwrap();
            graph.load_api_workflow(&converted.workflow).unwrap();

            // Deterministic pseudo-varied sizes standing in for measured ones.
            let sizes: HashMap<NodeId, egui::Vec2> = graph
                .snarl
                .nodes_pos_ids()
                .enumerate()
                .map(|(i, (id, _, _))| {
                    (id, egui::vec2(150.0 + (i * 37 % 250) as f32, 60.0 + (i * 53 % 400) as f32))
                })
                .collect();
            let rects = arrange(&mut graph.snarl, &sizes, false);
            for (i, a) in rects.iter().enumerate() {
                for b in &rects[i + 1..] {
                    assert!(
                        !a.shrink(1.0).intersects(b.shrink(1.0)),
                        "{wf_path}: nodes overlap after arrange: {a:?} vs {b:?}"
                    );
                }
            }
            // Positions were actually applied to the snarl.
            let applied = graph.snarl.nodes_pos_ids().all(|(id, pos, _)| {
                rects.iter().any(|r| (r.min - pos).length() < 0.5) || sizes.get(&id).is_none()
            });
            assert!(applied, "{wf_path}: arrange did not move nodes");

            // Execution flows left-to-right: a consumer sits right of its producer. A converted
            // workflow can still contain back-edges (SetNode/GetNode and "Anything Everywhere"
            // links reconstruct into cycles), so require forward flow to dominate rather than be
            // absolute — the backbone reads as order of execution.
            let pos_of: HashMap<NodeId, egui::Pos2> =
                graph.snarl.nodes_pos_ids().map(|(id, pos, _)| (id, pos)).collect();
            let (mut forward, mut total) = (0u32, 0u32);
            for (from, to) in graph.snarl.wires() {
                if from.node == to.node {
                    continue;
                }
                let (Some(a), Some(b)) = (pos_of.get(&from.node), pos_of.get(&to.node)) else {
                    continue;
                };
                total += 1;
                if b.x > a.x {
                    forward += 1;
                }
            }
            if total > 0 {
                assert!(
                    forward * 10 >= total * 8,
                    "{wf_path}: only {forward}/{total} wires flow left-to-right"
                );
            }
        }
    }

    /// Export to UI format re-converts to the same API workflow the editor holds.
    #[test]
    fn export_ui_reimports_cleanly() {
        let (Ok(oi_path), Ok(wf_paths)) = (
            std::env::var("OBJECT_INFO_JSON"),
            std::env::var("WORKFLOW_UI_JSON"),
        ) else {
            eprintln!("OBJECT_INFO_JSON/WORKFLOW_UI_JSON not set; skipping");
            return;
        };
        let schemas = crate::schema::parse(
            &serde_json::from_str(&std::fs::read_to_string(&oi_path).unwrap()).unwrap(),
        );
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        for wf_path in wf_paths.split(':') {
            let ui_json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(wf_path).unwrap()).unwrap();
            let converted = crate::uiwf::convert(&ui_json, &schemas).unwrap();
            graph.load_api_workflow(&converted.workflow).unwrap();
            let editor_wf = graph.save_api_workflow();

            let view = GraphView::default();
            let bypassed = HashSet::new();
            let exported = view.export_ui(&graph, &schemas, &bypassed, &HashMap::new());
            let reimported = crate::uiwf::convert(&exported, &schemas)
                .unwrap_or_else(|e| panic!("{wf_path}: exported UI json failed to convert: {e}"));
            assert_eq!(
                reimported.workflow.0.len(),
                editor_wf.0.len(),
                "{wf_path}: exported workflow node count changed"
            );
            for w in &reimported.warnings {
                assert!(
                    !w.contains("unused widget"),
                    "{wf_path}: export produced misaligned widgets: {w}"
                );
            }
            println!("{wf_path}: export/reimport ok ({} nodes)", editor_wf.0.len());
        }
    }

    /// A minimal object_info covering a standard SDXL img2img graph.
    fn img2img_schemas() -> crate::schema::SchemaSet {
        crate::schema::parse(
            &serde_json::from_str(
                r#"{
            "CheckpointLoaderSimple": {"input": {"required": {"ckpt_name": [["sd.safetensors"]]}},
                "output": ["MODEL","CLIP","VAE"], "output_name": ["MODEL","CLIP","VAE"], "output_is_list": [false,false,false]},
            "LoadImage": {"input": {"required": {"image": [["photo.png"]]}},
                "output": ["IMAGE","MASK"], "output_name": ["IMAGE","MASK"], "output_is_list": [false,false]},
            "VAEEncode": {"input": {"required": {"pixels": ["IMAGE"], "vae": ["VAE"]}},
                "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
            "CLIPTextEncode": {"input": {"required": {"text": ["STRING", {"multiline": true}], "clip": ["CLIP"]}},
                "output": ["CONDITIONING"], "output_name": ["CONDITIONING"], "output_is_list": [false]},
            "KSampler": {"input": {"required": {
                "model": ["MODEL"], "positive": ["CONDITIONING"], "negative": ["CONDITIONING"], "latent_image": ["LATENT"],
                "seed": ["INT", {"default": 0}], "steps": ["INT", {"default": 20}], "cfg": ["FLOAT", {"default": 8.0}],
                "sampler_name": [["euler"]], "scheduler": [["normal"]], "denoise": ["FLOAT", {"default": 1.0}]}},
                "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
            "VAEDecode": {"input": {"required": {"samples": ["LATENT"], "vae": ["VAE"]}},
                "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]},
            "SaveImage": {"input": {"required": {"images": ["IMAGE"], "filename_prefix": ["STRING", {"default": "ComfyUI"}]}},
                "output": [], "output_name": [], "output_is_list": []}
        }"#,
            )
            .unwrap(),
        )
    }

    /// A UI-format img2img workflow whose image source has node type `loader_ty` (vary it to a
    /// custom node the server lacks). node 2 feeds VAEEncode(3).pixels.
    fn img2img_ui(loader_ty: &str) -> serde_json::Value {
        serde_json::json!({
            "nodes": [
                {"id": 1, "type": "CheckpointLoaderSimple", "mode": 0,
                 "outputs": [
                    {"name": "MODEL", "type": "MODEL", "links": [10]},
                    {"name": "CLIP", "type": "CLIP", "links": [11, 12]},
                    {"name": "VAE", "type": "VAE", "links": [13, 14]}],
                 "widgets_values": ["sd.safetensors"]},
                {"id": 2, "type": loader_ty, "mode": 0,
                 "outputs": [
                    {"name": "IMAGE", "type": "IMAGE", "links": [15]},
                    {"name": "MASK", "type": "MASK", "links": []}],
                 "widgets_values": ["photo.png", "image"]},
                {"id": 3, "type": "VAEEncode", "mode": 0,
                 "inputs": [
                    {"name": "pixels", "type": "IMAGE", "link": 15},
                    {"name": "vae", "type": "VAE", "link": 13}],
                 "outputs": [{"name": "LATENT", "type": "LATENT", "links": [16]}],
                 "widgets_values": []},
                {"id": 4, "type": "CLIPTextEncode", "mode": 0,
                 "inputs": [{"name": "clip", "type": "CLIP", "link": 11}],
                 "outputs": [{"name": "CONDITIONING", "type": "CONDITIONING", "links": [17]}],
                 "widgets_values": ["a cat"]},
                {"id": 5, "type": "CLIPTextEncode", "mode": 0,
                 "inputs": [{"name": "clip", "type": "CLIP", "link": 12}],
                 "outputs": [{"name": "CONDITIONING", "type": "CONDITIONING", "links": [18]}],
                 "widgets_values": ["blurry"]},
                {"id": 6, "type": "KSampler", "mode": 0,
                 "inputs": [
                    {"name": "model", "type": "MODEL", "link": 10},
                    {"name": "positive", "type": "CONDITIONING", "link": 17},
                    {"name": "negative", "type": "CONDITIONING", "link": 18},
                    {"name": "latent_image", "type": "LATENT", "link": 16}],
                 "outputs": [{"name": "LATENT", "type": "LATENT", "links": [19]}],
                 "widgets_values": [123, "fixed", 20, 8.0, "euler", "normal", 0.6]},
                {"id": 7, "type": "VAEDecode", "mode": 0,
                 "inputs": [
                    {"name": "samples", "type": "LATENT", "link": 19},
                    {"name": "vae", "type": "VAE", "link": 14}],
                 "outputs": [{"name": "IMAGE", "type": "IMAGE", "links": [20]}],
                 "widgets_values": []},
                {"id": 8, "type": "SaveImage", "mode": 0,
                 "inputs": [{"name": "images", "type": "IMAGE", "link": 20}],
                 "widgets_values": ["ComfyUI"]}
            ],
            "links": [
                [10, 1, 0, 6, 0, "MODEL"],
                [11, 1, 1, 4, 0, "CLIP"],
                [12, 1, 1, 5, 0, "CLIP"],
                [13, 1, 2, 3, 1, "VAE"],
                [14, 1, 2, 7, 1, "VAE"],
                [15, 2, 0, 3, 0, "IMAGE"],
                [16, 3, 0, 6, 3, "LATENT"],
                [17, 4, 0, 6, 1, "CONDITIONING"],
                [18, 5, 0, 6, 2, "CONDITIONING"],
                [19, 6, 0, 7, 0, "LATENT"],
                [20, 7, 0, 8, 0, "IMAGE"]
            ]
        })
    }

    /// The VAEEncode node's `pixels` input in a converted workflow, as (has_key, is_slot).
    fn pixels_state(wf: &rucomfyui::Workflow) -> (bool, bool) {
        let enc = wf.0.values().find(|n| n.class_type == "VAEEncode").expect("no VAEEncode");
        match enc.inputs.get("pixels") {
            Some(rucomfyui::workflow::WorkflowInput::Slot(..)) => (true, true),
            Some(_) => (true, false),
            None => (false, false),
        }
    }

    /// Load an img2img graph from an image and queue it: the full convert -> load -> export -> convert
    /// round trip must keep VAEEncode.pixels wired to the LoadImage.
    #[test]
    fn img2img_graph_roundtrip_keeps_pixels() {
        let schemas = img2img_schemas();
        let ui = img2img_ui("LoadImage");

        // Load into the editor (what tapping "open workflow" on a gallery image does).
        let loaded = crate::uiwf::convert(&ui, &schemas).unwrap();
        assert_eq!(pixels_state(&loaded.workflow), (true, true), "convert-on-load dropped pixels");
        let mut graph = ComfyUiNodeGraph::new(crate::schema::to_object_info(&schemas));
        graph.load_api_workflow(&loaded.workflow).unwrap();

        // Queue from the editor (export_ui -> convert).
        let view = GraphView::default();
        let exported = view.export_ui(&graph, &schemas, &HashSet::new(), &HashMap::new());
        let queued = crate::uiwf::convert(&exported, &schemas).unwrap();
        assert_eq!(
            pixels_state(&queued.workflow),
            (true, true),
            "round trip lost VAEEncode.pixels: warnings={:?}",
            queued.warnings
        );
    }

    /// When the image source is a custom node the server lacks, convert drops it and VAEEncode is
    /// left with no `pixels` — reproducing the "VAEEncode missing pixels" server rejection. The
    /// pre-flight must catch it so the queue is blocked with a clear message instead of failing
    /// opaquely on the server.
    #[test]
    fn img2img_unknown_loader_is_caught_by_preflight() {
        let schemas = img2img_schemas();
        let ui = img2img_ui("Image Load");
        let loaded = crate::uiwf::convert(&ui, &schemas).unwrap();
        assert_eq!(
            pixels_state(&loaded.workflow),
            (false, false),
            "expected pixels to be dropped when its source node is unknown"
        );
        let problems = crate::preflight::validate(&loaded.workflow, &schemas);
        assert!(
            problems.iter().any(|p| p.class == "VAEEncode" && p.input == "pixels"),
            "preflight missed the dropped pixels input: {problems:?}"
        );
    }

    #[test]
    fn fit_maps_view_center_to_screen_center_and_clamps_scale() {
        let view = egui::Rect::from_min_size(egui::pos2(1000.0, 2000.0), egui::vec2(4000.0, 2000.0));
        let ui = egui::Rect::from_min_size(egui::pos2(0.0, 100.0), egui::vec2(400.0, 600.0));
        let tf = fit_transform(view, ui);
        let mapped = tf * view.center();
        assert!((mapped - ui.center()).length() < 0.01);
        assert!((tf.scaling - 0.1).abs() < 1e-6, "400/4000 wins over 600/2000");

        let tiny = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0));
        assert_eq!(fit_transform(tiny, ui).scaling, MAX_SCALE);
        let huge = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1e6, 1e6));
        assert_eq!(fit_transform(huge, ui).scaling, MIN_SCALE);
    }

    /// `arrange_now` must move nodes without waiting for canvas size measures.
    #[test]
    fn arrange_now_compacts_without_measured_sizes() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"A": {"input": {"required": {"in": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        let a = snarl.insert_node(egui::pos2(0.0, 0.0), FlowNodeData::new(obj.clone()));
        let b = snarl.insert_node(egui::pos2(0.0, 400.0), FlowNodeData::new(obj.clone()));
        let c = snarl.insert_node(egui::pos2(600.0, 0.0), FlowNodeData::new(obj));
        snarl.connect(
            egui_snarl::OutPinId { node: a, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        snarl.connect(
            egui_snarl::OutPinId { node: b, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        let before: HashMap<NodeId, egui::Pos2> =
            snarl.nodes_pos_ids().map(|(id, pos, _)| (id, pos)).collect();
        let mut view = GraphView::new(1);
        view.arrange_now(&mut snarl);
        let moved = snarl.nodes_pos_ids().any(|(id, pos, _)| before.get(&id) != Some(&pos));
        assert!(moved, "arrange_now left every node in place");
        assert!(view.sizes.is_empty(), "arrange_now must not fake measured sizes");
        // Consumer sits to the right of its producers.
        let pos = |id| snarl.get_node_info(id).unwrap().pos;
        assert!(pos(c).x > pos(a).x);
        assert!(pos(c).x > pos(b).x);
    }

    #[test]
    fn mark_needs_auto_arrange_defers_until_applied() {
        let mut view = GraphView::new(3);
        view.mark_needs_auto_arrange();
        assert!(view.needs_auto_arrange);
        assert!(view.arrange_pending());
        assert!(!view.arrange_queued);
    }

    /// Load path must queue a refine pass; seeding nominal sizes used to make that pass a no-op.
    #[test]
    fn arrange_on_load_queues_refine_without_faking_sizes() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"A": {"input": {"required": {"in": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        let a = snarl.insert_node(egui::pos2(0.0, 0.0), FlowNodeData::new(obj.clone()));
        let c = snarl.insert_node(egui::pos2(800.0, 0.0), FlowNodeData::new(obj));
        snarl.connect(
            egui_snarl::OutPinId { node: a, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        let mut view = GraphView::new(2);
        view.arrange_on_load(&mut snarl);
        assert!(view.arrange_queued, "load must queue a measured refine pass");
        assert!(view.sizes.is_empty(), "nominal placeholders must not mark sizes ready");
        // Simulate canvas measures, then the refine arrange that show() would run.
        view.sizes.insert(a, egui::vec2(220.0, 360.0));
        view.sizes.insert(c, egui::vec2(220.0, 360.0));
        let before = snarl.get_node_info(c).unwrap().pos;
        view.arrange_now(&mut snarl);
        let after = snarl.get_node_info(c).unwrap().pos;
        assert_ne!(before, after, "refine with tall measured sizes must re-pack");
        assert!(after.x > snarl.get_node_info(a).unwrap().pos.x);
    }

    /// The vertical relaxation must not inflate the layout. It used to: `resolve` could only push a
    /// node DOWN, so each of the 8x2 passes translated whichever nodes sat in a fan-in/fan-out
    /// conflict further down while unconflicted nodes (a disconnected note, a second component)
    /// stayed put — which is what put "some nodes 10-20 node-widths away from the rest" on screen.
    /// Measured before the fix on this exact shape: 948 units tall seeded, 2798 after 8 iterations.
    #[test]
    fn relaxation_does_not_inflate_the_vertical_span() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"A": {"input": {"required": {"a": ["MODEL"], "b": ["MODEL"], "c": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        // A fan-out into a fan-in (the shape that conflicts), plus a node wired to nothing.
        let root = snarl.insert_node(egui::pos2(0.0, 0.0), FlowNodeData::new(obj.clone()));
        let mid: Vec<NodeId> = (0..4)
            .map(|i| {
                snarl.insert_node(egui::pos2(600.0, i as f32 * 400.0), FlowNodeData::new(obj.clone()))
            })
            .collect();
        let sink = snarl.insert_node(egui::pos2(1200.0, 0.0), FlowNodeData::new(obj.clone()));
        let _loose = snarl.insert_node(egui::pos2(600.0, 2000.0), FlowNodeData::new(obj));
        for (i, &m) in mid.iter().enumerate() {
            snarl.connect(OutPinId { node: root, output: 0 }, InPinId { node: m, input: 0 });
            snarl.connect(OutPinId { node: m, output: 0 }, InPinId { node: sink, input: i.min(2) });
        }
        let sizes: HashMap<NodeId, egui::Vec2> = snarl
            .nodes_pos_ids()
            .map(|(id, _, _)| (id, egui::vec2(280.0, 300.0)))
            .collect();

        let rects = arrange(&mut snarl, &sizes, false);
        let span = rects.iter().fold(egui::Rect::NOTHING, |acc, r| acc.union(*r));
        // The tallest column is the 4 fanned nodes + the loose one: 5*300 + 4*V_GAP(24) = 1596.
        let minimal = 5.0 * 300.0 + 4.0 * 24.0;
        println!("vspan {:.0} (minimal stack {minimal:.0})", span.height());
        assert!(
            span.height() <= minimal * 1.15,
            "relaxation inflated the layout to {:.0} units tall against a {minimal:.0} minimum",
            span.height()
        );
        // And it still has to be a valid layout.
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.shrink(1.0).intersects(b.shrink(1.0)), "overlap: {a:?} vs {b:?}");
            }
        }
    }

    /// A single absurd measure must not throw the rest of the graph off the canvas — the shape of
    /// the load-arrange bug in [`load_arrange_matches_a_later_manual_arrange`], where one ratcheted
    /// node width put every column after it a screen away.
    #[test]
    fn arrange_survives_a_pathological_measure() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"A": {"input": {"required": {"in": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        let a = snarl.insert_node(egui::pos2(0.0, 0.0), FlowNodeData::new(obj.clone()));
        let b = snarl.insert_node(egui::pos2(600.0, 0.0), FlowNodeData::new(obj.clone()));
        let c = snarl.insert_node(egui::pos2(1200.0, 0.0), FlowNodeData::new(obj));
        snarl.connect(
            egui_snarl::OutPinId { node: a, output: 0 },
            egui_snarl::InPinId { node: b, input: 0 },
        );
        snarl.connect(
            egui_snarl::OutPinId { node: b, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        // `a` measured absurdly — as `final_node_rect` would have received it. The cache clamps on
        // the way in, which is the boundary this invariant now lives at.
        assert_eq!(
            clamp_measure(egui::vec2(9000.0, 40000.0)),
            MAX_LAYOUT_NODE,
            "a pathological measure must be clamped before it reaches the size cache"
        );
        let sizes: HashMap<NodeId, egui::Vec2> = [
            (a, clamp_measure(egui::vec2(9000.0, 40000.0))),
            (b, clamp_measure(egui::vec2(220.0, 300.0))),
            (c, clamp_measure(egui::vec2(220.0, 300.0))),
        ]
        .into_iter()
        .collect();
        let rects = arrange(&mut snarl, &sizes, false);
        let span = rects
            .iter()
            .fold(egui::Rect::NOTHING, |acc, r| acc.union(*r));
        assert!(
            span.width() <= 3.0 * MAX_LAYOUT_NODE.x,
            "one bad measure spread the layout {} units wide",
            span.width()
        );
        // Flow still reads left to right.
        let pos = |id| snarl.get_node_info(id).unwrap().pos;
        assert!(pos(b).x > pos(a).x && pos(c).x > pos(b).x);
    }

    /// The file picker finds upload widgets by what their options look like, so custom loaders get
    /// it for free — and it must not fire on the enums that merely happen to be lists of names.
    #[test]
    fn media_inputs_are_found_by_their_files() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{
                 "LoadImage": {"input": {"required": {"image": [["a.png", "clipspace/b.png [input]"]]}},
                     "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]},
                 "VHS_LoadVideo": {"input": {"required": {"video": [["clip.mp4", "loop.webm", "anim.gif"]],
                                                          "frame_load_cap": ["INT"]}},
                     "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]},
                 "KSampler": {"input": {"required": {"sampler_name": [["euler", "dpmpp_2m", "ddim"]]}},
                     "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
                 "CheckpointLoaderSimple": {"input": {"required": {"ckpt_name": [["sd15.safetensors", "xl.ckpt"]]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]},
                 "EmptyLoader": {"input": {"required": {"video": [[]]}},
                     "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]}
                }"#,
            )
            .unwrap(),
        ));
        let of = |class: &str| media_input_of(&FlowNodeData::new(oi.get(class).unwrap().clone()));

        let image = of("LoadImage").expect("LoadImage.image is a file selector");
        assert_eq!(image.name, "image");
        assert!(!image.video);

        let video = of("VHS_LoadVideo").expect("VHS_LoadVideo.video is a file selector");
        assert_eq!(video.name, "video");
        assert!(video.video, "a list of .mp4/.webm/.gif is a video selector");

        assert!(of("KSampler").is_none(), "sampler names are not files");
        assert!(of("CheckpointLoaderSimple").is_none(), "checkpoints are not media files");

        // Nothing uploaded yet: the widget name is all there is to go on.
        let empty = of("EmptyLoader").expect("an empty upload widget still gets the picker");
        assert!(empty.video);
    }

    /// Two frames must agree before a queued arrange fires: egui discards and re-runs the pass
    /// that initialises snarl node state, so the first measure is not the final one.
    #[test]
    fn sizes_agree_only_on_matching_snapshots() {
        let mut a: HashMap<NodeId, egui::Vec2> = HashMap::new();
        let mut b: HashMap<NodeId, egui::Vec2> = HashMap::new();
        assert!(sizes_agree(&a, &b), "two empty snapshots agree");
        a.insert(NodeId(0), egui::vec2(200.0, 100.0));
        assert!(!sizes_agree(&a, &b), "a new node is not settled");
        b.insert(NodeId(0), egui::vec2(200.4, 100.0));
        assert!(sizes_agree(&a, &b), "sub-unit jitter still counts as settled");
        b.insert(NodeId(0), egui::vec2(260.0, 100.0));
        assert!(!sizes_agree(&a, &b), "a real size change is not settled");
    }

    #[test]
    fn first_node_prefers_leftmost_root() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"A": {"input": {"required": {"in": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut snarl: Snarl<FlowNodeData> = Snarl::new();
        let a = snarl.insert_node(egui::pos2(500.0, 0.0), FlowNodeData::new(obj.clone()));
        let b = snarl.insert_node(egui::pos2(100.0, 0.0), FlowNodeData::new(obj.clone()));
        let c = snarl.insert_node(egui::pos2(-50.0, 0.0), FlowNodeData::new(obj));
        // b -> a and b -> c: roots are b (x=100); c has an input so the leftmost node loses.
        snarl.connect(
            egui_snarl::OutPinId { node: b, output: 0 },
            egui_snarl::InPinId { node: a, input: 0 },
        );
        snarl.connect(
            egui_snarl::OutPinId { node: b, output: 0 },
            egui_snarl::InPinId { node: c, input: 0 },
        );
        assert_eq!(first_node_pos(&snarl, false), Some(egui::pos2(100.0, 0.0)));
    }

    /// The auto-arrange a load runs must land where a manual Auto-arrange later would — the whole
    /// point of doing it automatically.
    ///
    /// Regression: a node's text widgets took `ui.available_width()`, which snarl derives from the
    /// node's size *from the previous frame*, so each frame's content came out a little wider than
    /// the last and the node ratcheted outward (measured 121 → 173 → 225 → … units here) until it
    /// hit the graph-space viewport — a bound that scales with 1/zoom, so the far-out fit right
    /// after a load let it run for a long way. The load arrange fired on the first frame every
    /// node merely *had* a size, i.e. mid-ratchet, and spaced its columns by those inflated
    /// widths: a compact cluster with the trailing columns flung off-screen. Pressing the button
    /// later worked because by then the sizes had stopped moving.
    #[test]
    fn load_arrange_matches_a_later_manual_arrange() {
        let oi = crate::schema::to_object_info(&crate::schema::parse(
            &serde_json::from_str(
                r#"{"T": {"input": {"required": {"text": ["STRING", {"multiline": true}],
                                                 "in": ["MODEL"]}},
                     "output": ["MODEL"], "output_name": ["MODEL"], "output_is_list": [false]}}"#,
            )
            .unwrap(),
        ));
        let obj = oi.values().next().unwrap().clone();
        let mut graph = ComfyUiNodeGraph::new(oi.clone());
        // Positions as `load_api_workflow` lays them out: 600 per depth column, 400 per row.
        let mut prev: Option<NodeId> = None;
        for i in 0..6 {
            let id = graph.snarl.insert_node(
                egui::pos2(i as f32 * 600.0, 0.0),
                FlowNodeData::new(obj.clone()),
            );
            if let Some(p) = prev {
                graph.snarl.connect(OutPinId { node: p, output: 0 }, InPinId { node: id, input: 1 });
            }
            prev = Some(id);
        }
        let mut view = GraphView::new(9);
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 840.0));
        let run = |view: &mut GraphView, graph: &mut ComfyUiNodeGraph, frames: usize| {
            for _ in 0..frames {
                let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
                // Measurement pass only — nothing uploads the textures, and epaint panics on an unapplied delta.
                ctx.run_ui(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let _ = view.show(
                            ui,
                            graph,
                            None,
                            None,
                            &HashSet::new(),
                            &[],
                            &mut HashMap::new(),
                        );
                    });
                }).textures_delta.clear();
            }
        };
        let positions = |graph: &ComfyUiNodeGraph| -> Vec<(NodeId, egui::Pos2)> {
            let mut v: Vec<_> = graph.snarl.nodes_pos_ids().map(|(id, p, _)| (id, p)).collect();
            v.sort_by_key(|(id, _)| id.0);
            v
        };

        // What a workflow load does.
        view.mark_needs_auto_arrange();
        run(&mut view, &mut graph, 8);
        let auto = positions(&graph);
        assert!(!view.arrange_pending(), "the load arrange never settled");
        // Measures stopped moving, and no node grew to viewport size.
        let widest = view.sizes.values().map(|s| s.x).fold(0.0f32, f32::max);
        assert!(widest < 600.0, "a node measured {widest} units wide");
        let measured = view.sizes.clone();
        run(&mut view, &mut graph, 2);
        assert!(sizes_agree(&view.sizes, &measured), "node sizes are still ratcheting");

        // What tapping Auto-arrange later does. Same layout, or the automatic one was not usable.
        view.request_arrange();
        run(&mut view, &mut graph, 8);
        for ((id, a), (_, m)) in auto.iter().zip(positions(&graph).iter()) {
            assert!(
                (*a - *m).length() < 1.0,
                "{id:?}: load put the node at {a:?}, a later manual arrange at {m:?}"
            );
        }
    }

    // ---------------------------------------------------------------------------------------
    // TEMPORARY INVESTIGATION PROBE — delete with everything below this marker.
    // ---------------------------------------------------------------------------------------

    /// object_info for a realistic SDXL txt2img graph: long enum filenames, multiline prompts,
    /// several INT/FLOAT drag values, two more enums on the sampler, a STRING filename prefix.
    fn probe_schemas() -> crate::schema::SchemaSet {
        crate::schema::parse(
            &serde_json::from_str(
                r#"{
        "CheckpointLoaderSimple": {"input": {"required": {
            "ckpt_name": [["sd_xl_base_1.0_0.9vae.safetensors","juggernautXL_v9Rundiffusion.safetensors"]]}},
            "output": ["MODEL","CLIP","VAE"], "output_name": ["MODEL","CLIP","VAE"], "output_is_list": [false,false,false]},
        "LoraLoader": {"input": {"required": {
            "model": ["MODEL"], "clip": ["CLIP"],
            "lora_name": [["SDXL/detail_tweaker_xl_v1.0_offset_noise.safetensors","add-detail-xl.safetensors"]],
            "strength_model": ["FLOAT", {"default": 1.0, "min": -20.0, "max": 20.0, "step": 0.01}],
            "strength_clip": ["FLOAT", {"default": 1.0, "min": -20.0, "max": 20.0, "step": 0.01}]}},
            "output": ["MODEL","CLIP"], "output_name": ["MODEL","CLIP"], "output_is_list": [false,false]},
        "CLIPTextEncode": {"input": {"required": {
            "text": ["STRING", {"multiline": true}], "clip": ["CLIP"]}},
            "output": ["CONDITIONING"], "output_name": ["CONDITIONING"], "output_is_list": [false]},
        "EmptyLatentImage": {"input": {"required": {
            "width": ["INT", {"default": 1024, "min": 16, "max": 16384, "step": 8}],
            "height": ["INT", {"default": 1024, "min": 16, "max": 16384, "step": 8}],
            "batch_size": ["INT", {"default": 1, "min": 1, "max": 64}]}},
            "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
        "KSampler": {"input": {"required": {
            "model": ["MODEL"], "positive": ["CONDITIONING"], "negative": ["CONDITIONING"], "latent_image": ["LATENT"],
            "seed": ["INT", {"default": 0, "min": 0, "max": 18446744073709551615}],
            "steps": ["INT", {"default": 25, "min": 1, "max": 10000}],
            "cfg": ["FLOAT", {"default": 7.5, "min": 0.0, "max": 100.0, "step": 0.1}],
            "sampler_name": [["dpmpp_2m_sde_gpu","euler_ancestral","dpmpp_3m_sde"]],
            "scheduler": [["karras","exponential","sgm_uniform"]],
            "denoise": ["FLOAT", {"default": 1.0, "min": 0.0, "max": 1.0, "step": 0.01}]}},
            "output": ["LATENT"], "output_name": ["LATENT"], "output_is_list": [false]},
        "VAEDecode": {"input": {"required": {"samples": ["LATENT"], "vae": ["VAE"]}},
            "output": ["IMAGE"], "output_name": ["IMAGE"], "output_is_list": [false]},
        "SaveImage": {"input": {"required": {
            "images": ["IMAGE"], "filename_prefix": ["STRING", {"default": "ComfyUI"}]}},
            "output": [], "output_name": [], "output_is_list": []}
    }"#,
            )
            .unwrap(),
        )
    }

    const PROBE_POS_PROMPT: &str = "cinematic portrait of a weathered lighthouse keeper, \
        salt-crusted wool coat, volumetric rim light through sea fog, 85mm, shallow depth of \
        field, hyperdetailed skin texture, film grain, muted teal and amber palette, \
        award-winning photography, shot on Kodak Portra 400";
    const PROBE_NEG_PROMPT: &str = "blurry, lowres, jpeg artifacts, watermark, text, signature, \
        extra fingers, deformed hands, oversaturated, plastic skin";

    fn probe_set_string(data: &mut FlowNodeData, name: &str, text: &str) {
        for inp in data.inputs.iter_mut() {
            if inp.name == name
                && let FlowValueType::String { value, .. } = &mut inp.value
            {
                *value = text.to_string();
            }
        }
    }

    fn probe_in_idx(data: &FlowNodeData, name: &str) -> usize {
        data.inputs.iter().position(|i| i.name == name).unwrap_or_else(|| panic!("no input {name}"))
    }

    fn probe_out_idx(data: &FlowNodeData, name: &str) -> usize {
        data.outputs
            .iter()
            .position(|o| o.name == name)
            .unwrap_or_else(|| panic!("no output {name}"))
    }

    /// Build the 8-node txt2img chain at load-time positions (600 per depth column).
    fn probe_graph() -> ComfyUiNodeGraph {
        let oi = crate::schema::to_object_info(&probe_schemas());
        let mut graph = ComfyUiNodeGraph::new(oi.clone());
        let mk = |class: &str| FlowNodeData::new(oi.get(class).expect(class).clone());

        let ckpt = graph.snarl.insert_node(egui::pos2(0.0, 0.0), mk("CheckpointLoaderSimple"));
        let lora = graph.snarl.insert_node(egui::pos2(600.0, 0.0), mk("LoraLoader"));
        let mut pos_n = mk("CLIPTextEncode");
        probe_set_string(&mut pos_n, "text", PROBE_POS_PROMPT);
        let pos = graph.snarl.insert_node(egui::pos2(1200.0, 0.0), pos_n);
        let mut neg_n = mk("CLIPTextEncode");
        probe_set_string(&mut neg_n, "text", PROBE_NEG_PROMPT);
        let neg = graph.snarl.insert_node(egui::pos2(1200.0, 400.0), neg_n);
        let empty = graph.snarl.insert_node(egui::pos2(1200.0, 800.0), mk("EmptyLatentImage"));
        let ks = graph.snarl.insert_node(egui::pos2(1800.0, 0.0), mk("KSampler"));
        let vae = graph.snarl.insert_node(egui::pos2(2400.0, 0.0), mk("VAEDecode"));
        let mut save_n = mk("SaveImage");
        probe_set_string(&mut save_n, "filename_prefix", "portraits/lighthouse_keeper_v3");
        let save = graph.snarl.insert_node(egui::pos2(3000.0, 0.0), save_n);

        let wire = |graph: &mut ComfyUiNodeGraph, from: NodeId, out: &str, to: NodeId, inp: &str| {
            let o = probe_out_idx(graph.snarl.get_node(from).unwrap(), out);
            let i = probe_in_idx(graph.snarl.get_node(to).unwrap(), inp);
            graph.snarl.connect(OutPinId { node: from, output: o }, InPinId { node: to, input: i });
        };
        wire(&mut graph, ckpt, "MODEL", lora, "model");
        wire(&mut graph, ckpt, "CLIP", lora, "clip");
        wire(&mut graph, lora, "CLIP", pos, "clip");
        wire(&mut graph, lora, "CLIP", neg, "clip");
        wire(&mut graph, lora, "MODEL", ks, "model");
        wire(&mut graph, pos, "CONDITIONING", ks, "positive");
        wire(&mut graph, neg, "CONDITIONING", ks, "negative");
        wire(&mut graph, empty, "LATENT", ks, "latent_image");
        wire(&mut graph, ks, "LATENT", vae, "samples");
        wire(&mut graph, ckpt, "VAE", vae, "vae");
        wire(&mut graph, vae, "IMAGE", save, "images");
        graph
    }

    /// Drive the real canvas headlessly at phone size and print, every frame, the transform
    /// scale, every node's measured size and the graph's x/y span — first through a workflow
    /// load (`mark_needs_auto_arrange`), then through a manual `request_arrange`.
    /// A realistic txt2img graph must arrange into a layout a phone can actually read, and the two
    /// entry points must agree. Guards the numbers this was tuned against (measured on a 393x873
    /// viewport): before the fixes the same graph came out 2255 units wide at fit zoom 0.159, with
    /// its vertical span inflated ~2.9x by a relaxation pass that only ever pushed nodes DOWN.
    #[test]
    fn realistic_graph_arranges_compactly() {
        // Portrait phone: the flow turns top-to-bottom so a deep workflow fits at a readable zoom.
        let portrait = arrange_probe(egui::vec2(393.0, 873.0));
        assert!(portrait.vertical, "a portrait canvas must lay the flow out top-to-bottom");
        assert!(
            portrait.zoom > 0.4,
            "fit zoom {:.3} — nodes render at {:.0}px on a 393px screen, too small to work with",
            portrait.zoom,
            portrait.widest * portrait.zoom
        );
        assert!(
            portrait.span.width() < 900.0 && portrait.span.height() < 1600.0,
            "portrait span {:.0}x{:.0}",
            portrait.span.width(),
            portrait.span.height()
        );

        // Landscape / graph fullscreen keeps the familiar left-to-right reading.
        let landscape = arrange_probe(egui::vec2(873.0, 393.0));
        assert!(!landscape.vertical, "a landscape canvas must keep the left-to-right flow");
        assert!(
            landscape.span.width() > landscape.span.height(),
            "landscape span {:.0}x{:.0} is not a left-to-right ribbon",
            landscape.span.width(),
            landscape.span.height()
        );
        // Was 2255 units wide at zoom 0.159 before the width fixes (combos elided, sandwich layout).
        assert!(
            landscape.span.width() < 2000.0,
            "landscape span {:.0} units wide",
            landscape.span.width()
        );

        // No single node may dominate a band: combos elide, string fields are capped.
        for probe in [&portrait, &landscape] {
            assert!(probe.widest < 400.0, "widest node {:.0} units", probe.widest);
        }
    }

    struct ArrangeProbe {
        span: egui::Rect,
        zoom: f32,
        widest: f32,
        vertical: bool,
    }

    /// Load the realistic graph onto a `screen`-sized canvas, let the auto-arrange settle, then
    /// press the manual button and confirm it reproduces the same layout.
    fn arrange_probe(screen: egui::Vec2) -> ArrangeProbe {
        let mut graph = probe_graph();
        let mut view = GraphView::new(9);
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, screen);
        let frame = |view: &mut GraphView, graph: &mut ComfyUiNodeGraph| {
            let input = egui::RawInput { screen_rect: Some(rect), ..Default::default() };
            // Measurement pass only — nothing uploads the textures, and epaint panics on an unapplied delta.
            ctx.run_ui(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ =
                        view.show(ui, graph, None, None, &HashSet::new(), &[], &mut HashMap::new());
                });
            }).textures_delta.clear();
        };

        view.mark_needs_auto_arrange();
        for _ in 0..15 {
            frame(&mut view, &mut graph);
        }
        let loaded: Vec<(NodeId, egui::Pos2)> =
            graph.snarl.nodes_pos_ids().map(|(id, p, _)| (id, p)).collect();

        view.request_arrange();
        for _ in 0..10 {
            frame(&mut view, &mut graph);
        }
        let manual: Vec<(NodeId, egui::Pos2)> =
            graph.snarl.nodes_pos_ids().map(|(id, p, _)| (id, p)).collect();
        for ((id, a), (_, m)) in loaded.iter().zip(manual.iter()) {
            assert!(
                (*a - *m).length() < 1.0,
                "{id:?}: load put it at {a:?}, the manual button at {m:?}"
            );
        }

        let span = bounds(&graph.snarl, &view.sizes).expect("laid out");
        println!(
            "{}x{} canvas -> span {:.0}x{:.0} zoom {:.3} vertical={}",
            screen.x,
            screen.y,
            span.width(),
            span.height(),
            view.to_global.scaling,
            view.flow_vertical()
        );
        ArrangeProbe {
            span,
            zoom: view.to_global.scaling,
            widest: view.sizes.values().map(|s| s.x).fold(0.0f32, f32::max),
            vertical: view.flow_vertical(),
        }
    }
}
