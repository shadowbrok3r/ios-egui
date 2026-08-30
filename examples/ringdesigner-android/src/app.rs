//! The Android shell: a bottom tab bar over the panes, plus the debounced rebuild.
//!
//! There is no dock and no pane grid — those are mouse-and-monitor affordances. Tabs, because at
//! ~411 x 890 points there is room for exactly one thing at a time.
//!
//! Autosave is not optional here. `EguiApp` has no `save` hook, `on_pause` never fires on Android,
//! and nothing replaces the desktop's eframe-storage path, so without a write on the dirty debounce
//! the design is gone the moment the OS reaps the process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui_mobile::egui;
use egui_mobile::{CreateContext, EguiApp, Haptic, Host, HostExt, StylusProbe};

use ringdesign_core::alpha::AlphaLibrary;
use ringdesign_core::castability::CastReport;
use ringdesign_core::{RingDesign, library};

use ringdesign_core::drawn::DrawnAlpha;
use ringdesign_core::field::{Layer, LayerEntry};
use ringdesign_core::tiling::TilingLayer;

use crate::bench;
use crate::canvas::{self, CanvasInput, Domain, View};
use crate::export::{self, ExportDone, ExportKind};
use crate::graph::GraphDone;
use crate::library as liblib;
use crate::paint;
use crate::util::{slug, sync_base};
use crate::ring::{self, RingPane, Worker};
use crate::viewport::{GpuMeshRenderer, ShadeMode};

/// Quiet period after the last edit before a full rebuild fires. 90 ms, unchanged from the desktop:
/// the measured worker cost at 384x144 is 47 ms, so the loop keeps up.
const DEBOUNCE: Duration = Duration::from_millis(90);

const AUTOSAVE: &str = "current.ring.json";

/// Name of the band-wide drawing, and of the tile. Both are `DrawnAlpha` entries in the design and
/// `TilingLayer`s referencing them by name, exactly like any imported alpha.
const BAND_ALPHA: &str = "band";
const TILE_ALPHA: &str = "tile";
/// 2048 px across a ~67 mm circumference is 0.033 mm per pixel — well under what the mesh resolves.
/// 512 would be 0.13 mm, about the mesh's own step, and pen detail would quantize away.
const BAND_W: u32 = 2048;
const BAND_H: u32 = 320;
const TILE_EDGE: u32 = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Ring,
    Band,
    Tile,
    Graph,
    Alphas,
    Files,
    Bench,
}

impl Tab {
    /// `(tab, emoji, label)` — a labelled button when the label is non-empty,
    /// a square icon button when it is.
    const BAR: &'static [(Tab, &'static str, &'static str)] = &[
        (Tab::Ring, "", "Ring"),
        (Tab::Band, "", "Band"),
        (Tab::Tile, "", "Tile"),
        (Tab::Graph, "\u{1F517}", ""),
        (Tab::Alphas, "\u{1F3A8}", ""),
        (Tab::Files, "\u{1F4C1}", ""),
        (Tab::Bench, "\u{26A1}", ""),
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::Ring => "Ring",
            Tab::Band => "Band",
            Tab::Tile => "Tile",
            Tab::Graph => "Graph",
            Tab::Alphas => "Alphas",
            Tab::Files => "Files",
            Tab::Bench => "Bench",
        }
    }
}


/// Name of the layer a library alpha lands on, so picking a second one replaces the first rather
/// than stacking two textures nobody asked for.
const PATTERN_LAYER: &str = "pattern";

/// Brush settings, shared by both paint surfaces.
struct Brush {
    /// Radius as a fraction of the canvas width.
    frac: f32,
    soft: f32,
    /// Scales what full pressure asks for, 0..1 of the 1.6 mm maximum.
    depth: f64,
    erase: bool,
    /// Reject finger and palm contacts. Defaults on where there is an S-Pen.
    stylus_only: bool,
}

impl Default for Brush {
    fn default() -> Self {
        Self { frac: 0.012, soft: 0.5, depth: 1.0, erase: false, stylus_only: false }
    }
}

pub struct RingApp {
    design: RingDesign,
    lib: Arc<AlphaLibrary>,
    renderer: Arc<Mutex<GpuMeshRenderer>>,
    pane: RingPane,
    worker: Option<Worker>,
    tab: Tab,

    cast: Option<CastReport>,
    field: Option<ringdesign_core::castability::FieldReport>,
    stones: Option<ringdesign_core::stones::StonesReport>,
    dfm: Vec<ringdesign_core::dfm::DfmFinding>,
    /// The settled preview mesh, kept for the tap probe's raycast.
    preview_mesh: Option<std::sync::Arc<ringdesign_core::mesh::Mesh>>,
    /// Last long-press readout, shown as a chip until dismissed.
    probe_info: Option<String>,
    show_gems: bool,
    /// The design editor rides a collapsible bottom sheet over the live view.
    design_open: bool,
    /// The DFM findings ride a second one, opened by tapping their chip.
    dfm_open: bool,
    /// Whole-design snapshots with a name read out of the diff. Shared with the
    /// desktop, so a step reads the same on both.
    history: ringdesign_core::history::History,
    /// The timeline sheet, opened by a long press on Undo.
    timeline_open: bool,
    /// The layer stack rides its own sheet, opened from the nav bar beside Design.
    layers_open: bool,
    /// Row the stack sheet has open, if any.
    selected_layer: Option<usize>,
    /// The stone the generators place, and where.
    stone: crate::stones::Pick,
    /// The settled build's own report, and whether its sheet is open.
    report: Option<ringdesign_core::mesh::Report>,
    report_open: bool,
    /// Everything remembered between launches that is not the design itself.
    prefs: crate::prefs::Prefs,
    /// Design awaiting a delete confirmation, and one being renamed.
    confirm_delete: Option<std::path::PathBuf>,
    renaming: Option<(std::path::PathBuf, String)>,
    /// Path a Save was warned about; a second Save to the same path goes through.
    overwrite_warned: Option<std::path::PathBuf>,
    /// Whether a prefs file was actually read, so first-run defaults still apply.
    prefs_seen: bool,
    /// Soften the preview at the sand's detail radius — see the pour early.
    as_cast: bool,
    /// Cut exports oversize for this metal's shrink; None is nominal.
    shrink_metal: Option<usize>,
    status: String,
    dirty_at: Option<Instant>,
    generation: u64,

    data_root: Option<std::path::PathBuf>,
    px_per_mm: Option<f32>,

    thumbs: liblib::Thumbs,
    /// Device photos offered as ornament sources, `(id, display name)`.
    photos: Vec<(i64, String)>,
    photo_thumbs: HashMap<i64, egui::TextureHandle>,
    picker_open: bool,
    alpha_filter: String,
    /// Alpha currently on the pattern layer.
    picked_alpha: Option<String>,
    pattern_repeats: u32,
    pattern_height_mm: f64,
    builtin_size: usize,

    /// Desktop sync: `host:port` and the shared token, both remembered between launches.
    sync_host: String,
    sync_token: String,
    sync_job: Option<std::sync::mpsc::Receiver<SyncResult>>,

    brush: Brush,
    band_view: View,
    tile_view: View,
    tile_repeats: u32,
    has_stylus: bool,
    readout: Option<String>,
    /// `(tool, hover px, buttons)` sampled once a frame. winit drops tool type and hover, so this
    /// comes from the patched `android-activity` side channel rather than from egui's events.
    probe: StylusProbe,
    /// Contact that owns the stroke in progress; a palm landing later is ignored.
    active_touch: Option<egui::TouchId>,
    /// Whether the ceiling was refusing depth on the previous frame, so the
    /// clamp tick fires at onset rather than every sample.
    was_clamped: bool,
    /// Castability zone the pen was last in, for the crossing tick.
    last_zone: Option<&'static str>,
    /// Last settled verdict, and whether the newest build made it worse — the
    /// buzz fires from `update`, which is where a `Host` is in scope.
    last_verdict: Option<ringdesign_core::castability::Verdict>,
    verdict_fell: bool,
    /// A slider crossed one of its steps this frame.
    detent: bool,
    /// The APK's bundled lib dir — the only place a QNN `.so` can be dlopen'd from.
    native_lib_dir: Option<String>,
    /// Model packs found on shared storage; empty without `local-npu` packs present.
    packs: Vec<crate::npu::Pack>,
    /// Prompt for on-device pattern generation, and the job in flight.
    prompt: String,
    gen_job: Option<std::sync::mpsc::Receiver<Result<Vec<u8>, String>>>,
    /// CLIP embeddings of the library, for "more like this".
    embeddings: crate::similar::Embeddings,
    /// Names ranked by the last similarity query; empty means show everything.
    similar_to: Option<(String, Vec<String>)>,
    /// The pen sketch pad: which shape is being drawn, and the stroke so far.
    sketch: crate::sketch::Sketch,
    sketch_mode: Option<crate::sketch::Mode>,

    bench: BenchState,
    /// The design's graph, when it has one: the editor and its evaluation.
    graph: crate::graph::GraphState,
    /// Exports in flight, one thread each; none is ever dropped as stale.
    exports: Vec<std::sync::mpsc::Receiver<ExportDone>>,
}

#[derive(Default)]
struct BenchState {
    running: Option<std::sync::mpsc::Receiver<bench::Report>>,
    report: Option<bench::Report>,
    started: Option<Instant>,
}

impl RingApp {
    pub fn new(_cc: &CreateContext) -> Self {
        Self {
            design: RingDesign::default(),
            lib: Arc::new(AlphaLibrary::builtin()),
            renderer: Arc::new(Mutex::new(GpuMeshRenderer::default())),
            pane: RingPane::default(),
            worker: None,
            tab: Tab::Ring,
            cast: None,
            field: None,
            stones: None,
            dfm: Vec::new(),
            preview_mesh: None,
            probe_info: None,
            show_gems: true,
            design_open: false,
            dfm_open: false,
            history: ringdesign_core::history::History::new(&RingDesign::default()),
            timeline_open: false,
            layers_open: false,
            selected_layer: None,
            stone: crate::stones::Pick::default(),
            report: None,
            report_open: false,
            prefs: crate::prefs::Prefs::default(),
            confirm_delete: None,
            renaming: None,
            overwrite_warned: None,
            prefs_seen: false,
            as_cast: false,
            shrink_metal: None,
            status: "starting".into(),
            dirty_at: None,
            generation: 0,
            data_root: None,
            px_per_mm: None,
            thumbs: liblib::Thumbs::default(),
            photos: Vec::new(),
            photo_thumbs: HashMap::new(),
            picker_open: false,
            alpha_filter: String::new(),
            picked_alpha: None,
            pattern_repeats: 24,
            pattern_height_mm: 0.35,
            builtin_size: 256,
            sync_host: String::new(),
            sync_token: String::new(),
            sync_job: None,
            brush: Brush::default(),
            band_view: View::default(),
            tile_view: View::default(),
            tile_repeats: 24,
            has_stylus: false,
            readout: None,
            probe: Default::default(),
            active_touch: None,
            was_clamped: false,
            last_zone: None,
            last_verdict: None,
            verdict_fell: false,
            detent: false,
            native_lib_dir: None,
            packs: Vec::new(),
            prompt: String::new(),
            gen_job: None,
            embeddings: Default::default(),
            similar_to: None,
            sketch: Default::default(),
            sketch_mode: None,
            bench: BenchState::default(),
            graph: crate::graph::GraphState::new(),
            exports: Vec::new(),
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty_at = Some(Instant::now());
        // Every editor funnels through here, so this is the one place a step
        // has to be noticed. The label comes out of the diff later — no call
        // site threads one through.
        self.history.touch();
    }

    /// Take a design from somewhere other than an edit — a file, a paste, a
    /// pull, a template — and start the history over from it.
    fn adopt(&mut self, design: RingDesign) {
        self.design = design;
        let lib = Arc::make_mut(&mut self.lib);
        self.design.bake_all(lib);
        self.thumbs.clear();
        self.picked_alpha = None;
        // A row index from the old stack would point at a different layer.
        self.selected_layer = None;
        self.history.reset(&self.design);
        self.mark_dirty();
    }

    /// Put a design the history handed back on screen without recording it as a
    /// fresh edit — `touch` here would make undo its own undoable step.
    fn apply_history(&mut self, design: RingDesign, what: &str) {
        self.design = design;
        let lib = Arc::make_mut(&mut self.lib);
        self.design.bake_all(lib);
        self.thumbs.clear();
        self.selected_layer = None;
        self.dirty_at = Some(Instant::now());
        self.status = what.to_string();
    }

    /// Push the loaded preferences into the live state.
    fn apply_prefs(&mut self) {
        let p = &self.prefs;
        self.prefs_seen = true;
        self.sync_host = p.sync_host.clone();
        self.sync_token = p.sync_token.clone();
        self.brush.frac = p.brush_frac;
        self.brush.depth = p.brush_depth;
        self.brush.erase = p.brush_erase;
        self.brush.stylus_only = p.stylus_only;
        self.pane.shade = ShadeMode::ALL[p.shade];
        self.pane.wireframe = p.wireframe;
        self.as_cast = p.as_cast;
        self.show_gems = p.show_gems;
        self.shrink_metal = p.shrink_metal;
        self.pattern_repeats = p.pattern_repeats;
        self.pattern_height_mm = p.pattern_height_mm;
    }

    /// Collect the live state back into `prefs` and write it.
    ///
    /// Rides the same debounce as the autosave: an edit is one write of both,
    /// and neither happens per frame.
    fn save_prefs(&mut self) {
        let Some(root) = self.data_root.clone() else { return };
        self.prefs.sync_host = self.sync_host.clone();
        self.prefs.sync_token = self.sync_token.clone();
        self.prefs.brush_frac = self.brush.frac;
        self.prefs.brush_depth = self.brush.depth;
        self.prefs.brush_erase = self.brush.erase;
        self.prefs.stylus_only = self.brush.stylus_only;
        self.prefs.shade =
            ShadeMode::ALL.iter().position(|m| *m == self.pane.shade).unwrap_or(0);
        self.prefs.wireframe = self.pane.wireframe;
        self.prefs.as_cast = self.as_cast;
        self.prefs.show_gems = self.show_gems;
        self.prefs.shrink_metal = self.shrink_metal;
        self.prefs.pattern_repeats = self.pattern_repeats;
        self.prefs.pattern_height_mm = self.pattern_height_mm;
        if let Err(e) = crate::prefs::save(&root, &self.prefs) {
            log::warn!("prefs: {e}");
        }
    }

    /// Look for model packs under the app's own models folder and the shared
    /// one the sibling app uses, so a pack downloaded once serves both.
    fn rescan_packs(&mut self) {
        let Some(root) = self.data_root.as_ref() else { return };
        let mine = root.join("models");
        let shared = std::path::PathBuf::from("/storage/emulated/0/ComfyUI");
        self.packs = crate::npu::scan_many(&[mine.as_path(), shared.as_path()]);
    }

    fn autosave(&self) {
        let Some(root) = self.data_root.as_ref() else { return };
        let path = root.join(AUTOSAVE);
        if let Err(e) = library::save_design(&path, &self.design) {
            log::warn!("autosave {}: {e}", path.display());
        }
    }

    /// Dispatch a rebuild. `analyze` is only worth its ~30% once the edit has settled.
    fn dispatch(&mut self, analyze: bool) {
        let Some(worker) = self.worker.as_ref() else { return };
        self.generation += 1;
        let mut params = ring::PREVIEW;
        if self.as_cast {
            params.soften_mm = self.design.draft.min_detail_mm;
        }
        if !worker.dispatch(self.generation, &self.design, &self.lib, params, analyze, self.show_gems) {
            self.status = "build worker stopped".into();
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        if let Some(worker) = self.worker.as_ref() {
            while let Some(done) = worker.poll() {
                if done.generation != self.generation {
                    continue;
                }
                if let Some(g) = done.graph {
                    if g.ok {
                        self.design = g.design;
                    }
                    self.graph.apply(&GraphDone { design: RingDesign::default(), ..g });
                }
                self.pane.camera.fit(done.bounds);
                if let Ok(mut r) = self.renderer.lock() {
                    r.set_pending(done.verts);
                    r.set_pending_gems(done.gems);
                }
                self.preview_mesh = Some(done.mesh);
                if let Some(cast) = done.cast {
                    let verdict = done
                        .field
                        .as_ref()
                        .map(|f| f.verdict)
                        .unwrap_or(cast.verdict);
                    self.status = format!(
                        "{} tris · {:.1} mm³ · {} ms · {}",
                        done.triangles,
                        done.volume_mm3,
                        done.build_ms,
                        verdict.label()
                    );
                    self.cast = Some(cast);
                    self.stones = done
                        .field
                        .as_ref()
                        .and_then(|f| ringdesign_core::stones::report(&self.design, f.parting_z_mm));
                    self.dfm = ringdesign_core::dfm::findings_in(&self.design, &self.lib);
                    self.field = done.field;
                    self.report = Some(done.report);
                    // A downgrade is news; an upgrade is not. Verdict is ordered
                    // Castable < Marginal < NotCastable by how bad it is, so a
                    // rank comparison is the whole test.
                    let rank = |v: ringdesign_core::castability::Verdict| match v {
                        ringdesign_core::castability::Verdict::Castable => 0u8,
                        ringdesign_core::castability::Verdict::Marginal => 1,
                        ringdesign_core::castability::Verdict::NotCastable => 2,
                    };
                    if let (Some(was), Some(now)) = (self.last_verdict, self.field.as_ref().map(|f| f.verdict)) {
                        if rank(now) > rank(was) {
                            self.verdict_fell = true;
                        }
                    }
                    self.last_verdict = self.field.as_ref().map(|f| f.verdict);
                } else {
                    self.status =
                        format!("{} tris · {} ms", done.triangles, done.build_ms);
                }
                ctx.request_repaint();
            }
        }

        // Unconditionally, and outside the debounce: history keeps its own 400 ms
        // settle against this file's 90 ms, and the two windows are independent
        // — a drag that never stops dirtying would otherwise never commit.
        if self.history.commit_if_settled(&self.design).is_some() {
            ctx.request_repaint();
        } else if self.history.is_pending() {
            // The screen stops drawing when nothing moves, and the settle window
            // is longer than the build debounce — without this the last edit of
            // a session sits uncommitted and undo loses it.
            ctx.request_repaint_after(Duration::from_millis(self.history.settle_ms()));
        }

        if let Some(at) = self.dirty_at {
            let waited = at.elapsed();
            if waited >= DEBOUNCE {
                self.dirty_at = None;
                self.dispatch(true);
                self.autosave();
                self.save_prefs();
            } else {
                // The debounce only fires from `tick`, and egui draws nothing unless something asks
                // it to — without this the settled build waits for the next unrelated frame, which
                // on an idle screen never comes.
                ctx.request_repaint_after(DEBOUNCE - waited);
            }
        }
    }

    fn ring_tab(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top(egui::Id::new("ring_tools")).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for mode in ShadeMode::ALL {
                    let sel = self.pane.shade == *mode;
                    if ui.add(crate::theme::selectable(sel, mode.label())).clicked() {
                        self.pane.shade = *mode;
                    }
                }
                ui.separator();
                self.undo_row(ui);
                ui.separator();
                ui.toggle_value(&mut self.pane.wireframe, "Wire");
                if ui
                    .toggle_value(&mut self.show_gems, "Stones")
                    .on_hover_text("Preview stones on their seats — display only, never cast.")
                    .changed()
                {
                    self.mark_dirty();
                }
                if ui
                    .toggle_value(&mut self.as_cast, "As-cast")
                    .on_hover_text("Soften the preview at the sand's detail radius — the pour, early.")
                    .changed()
                {
                    self.mark_dirty();
                }
                if let Some(f) = self.field.as_ref() {
                    ui.separator();
                    let (tint, text) = field_chip(f, self.design.draft.process);
                    ui.colored_label(tint, text).on_hover_text(f.notes.join("\n"));
                    if let Some(s) = self.stones.as_ref() {
                        let warns: Vec<&str> = s
                            .seats
                            .iter()
                            .flat_map(|seat| seat.warnings.iter().map(String::as_str))
                            .collect();
                        let tint = if warns.is_empty() {
                            egui::Color32::from_rgb(150, 190, 150)
                        } else {
                            egui::Color32::from_rgb(220, 170, 90)
                        };
                        ui.colored_label(
                            tint,
                            format!("{} stones · {:.2} ct", s.stone_count, s.total_carats),
                        )
                        .on_hover_text(if warns.is_empty() {
                            "Every seat checks out at the bench.".to_string()
                        } else {
                            warns.join("\n")
                        });
                    }
                    // Tappable, not a tooltip: a hover text is a press-and-hold
                    // on glass, and these are the messages that say what the
                    // sand will not hold.
                    if !self.dfm.is_empty() {
                        let label = egui::RichText::new(format!("DFM {}", self.dfm.len()))
                            .color(egui::Color32::from_rgb(220, 170, 90));
                        if ui.add(crate::theme::selectable(self.dfm_open, label)).clicked() {
                            self.dfm_open = !self.dfm_open;
                        }
                    }
                } else if let Some(cast) = self.cast.as_ref() {
                    ui.separator();
                    let (tint, text) = verdict_chip(cast);
                    ui.colored_label(tint, text);
                }
                // Beside the chips rather than in the nav bar, which is already
                // full at three labelled tabs, Design and four icon squares.
                ui.separator();
                let n = self.design.layers.layers.len();
                if ui
                    .add(crate::theme::selectable(self.layers_open, format!("Layers {n}")))
                    .on_hover_text("List, mute, reorder and window the stack")
                    .clicked()
                {
                    self.layers_open = !self.layers_open;
                }
                if ui
                    .add(crate::theme::selectable(self.report_open, "Report"))
                    .on_hover_text("Dimensions, weight in every alloy, and the seats")
                    .clicked()
                {
                    self.report_open = !self.report_open;
                }
                if self.px_per_mm.is_some() {
                    ui.toggle_value(&mut self.pane.actual_size, "1:1");
                }
            });
            if self.pane.shade == ShadeMode::Wall {
                ui.horizontal_wrapped(|ui| {
                    let min = self.design.draft.min_section_mm;
                    let chip = |ui: &mut egui::Ui, rgb: [f32; 3], label: String| {
                        let c = egui::Color32::from_rgb(
                            (rgb[0] * 255.0) as u8,
                            (rgb[1] * 255.0) as u8,
                            (rgb[2] * 255.0) as u8,
                        );
                        ui.colored_label(c, label);
                    };
                    use crate::viewport::{wall_color, WALL_NEUTRAL};
                    chip(ui, wall_color(min * 0.5, min), format!("< {min:.1} mm won't fill"));
                    chip(ui, wall_color(min * 1.5, min), format!("to {:.1}", min * 2.0));
                    chip(ui, wall_color(min * 2.7, min), "healthy".into());
                    chip(ui, wall_color(min * 6.0, min), "heavy".into());
                    chip(ui, WALL_NEUTRAL, "bore".into());
                });
            }
            ui.horizontal_wrapped(|ui| {
                crate::theme::up_menu(ui, "\u{1F4D0} View", |ui| {
                    for view in crate::camera::StandardView::ALL {
                        if ui.button(view.label()).clicked() {
                            self.pane.camera.set_view(*view);
                            self.pane.actual_size = false;
                        }
                    }
                    ui.separator();
                    if ui.button("Reset camera").clicked() {
                        self.pane.camera.reset();
                        self.pane.actual_size = false;
                    }
                });
            });
        });

        if let Some(text) = self.probe_info.clone() {
            egui::Panel::top(egui::Id::new("probe_info")).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(text).small());
                    if ui.small_button("x").clicked() {
                        self.probe_info = None;
                    }
                });
            });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            let (moved, ray) = self.pane.ui(ui, &self.renderer, self.px_per_mm);
            if moved {
                ui.ctx().request_repaint();
            }
            if let Some((origin, dir)) = ray {
                self.probe(origin, dir);
            }
        });
    }

    /// Long-press readout: where on the band the touch landed, what stands
    /// there, and how much metal is under it.
    /// Undo and redo, each naming the step it would take back.
    ///
    /// The label comes out of the diff between snapshots, so it says "Half
    /// Round" or "Flat boss" rather than "undo" — which on a phone, where the
    /// edit that needs taking back is usually a mistap nobody saw, is most of
    /// the value.
    fn undo_row(&mut self, ui: &mut egui::Ui) {
        let undo = self.history.undo_label().map(str::to_owned);
        let redo = self.history.redo_label().map(str::to_owned);

        let r = ui.add_enabled(undo.is_some(), egui::Button::new("Undo"));
        if let Some(l) = &undo {
            r.clone().on_hover_text(format!("Undo {l}"));
        }
        if r.clicked() {
            if let Some(d) = self.history.undo() {
                let what = undo.unwrap_or_else(|| "undone".into());
                self.apply_history(d, &format!("undid {what}"));
            }
        }
        // The timeline is a long press, the way the alpha grid and the 3D probe
        // already are — there is no room for a third button here.
        if r.long_touched() && self.history.present() > 0 {
            self.timeline_open = !self.timeline_open;
        }

        let r = ui.add_enabled(redo.is_some(), egui::Button::new("Redo"));
        if let Some(l) = &redo {
            r.clone().on_hover_text(format!("Redo {l}"));
        }
        if r.clicked() {
            if let Some(d) = self.history.redo() {
                let what = redo.unwrap_or_else(|| "redone".into());
                self.apply_history(d, &format!("redid {what}"));
            }
        }
    }

    /// Every committed step, newest last, with the present marked — a long press
    /// on Undo opens it and a tap jumps.
    fn timeline_sheet(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("history").small().weak());
            if ui.small_button("close").clicked() {
                self.timeline_open = false;
            }
        });
        ui.separator();
        let mut jump: Option<usize> = None;
        for (i, (label, is_present)) in self.history.timeline().into_iter().enumerate() {
            if ui.add(crate::theme::selectable(is_present, label.clone())).clicked() && !is_present {
                jump = Some(i);
            }
        }
        if let Some(i) = jump {
            if let Some(d) = self.history.jump_to(i) {
                self.apply_history(d, "jumped");
                self.timeline_open = false;
            }
        }
    }

    /// The pen sketch pad — a full-tab overlay while a shape is being drawn.
    ///
    /// A face becomes a `CustomOutline` in `design.shank.custom_outlines`, which
    /// is a registry that lives *in the design*, so a phone-drawn face opens on
    /// the desktop unchanged. A section becomes the `DropCurve` that
    /// `ProfileStyle::Custom` has always read and nothing could author.
    fn sketch_pad(&mut self, ui: &mut egui::Ui, host: &Host) {
        let Some(mode) = self.sketch_mode else { return };
        let title = match mode {
            crate::sketch::Mode::Face => "draw the face",
            crate::sketch::Mode::Section => "draw the section",
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(title).small().weak());
            if ui.button("Use it").clicked() {
                let msg = match mode {
                    crate::sketch::Mode::Face => {
                        let name = if self.sketch.name.trim().is_empty() {
                            "Drawn".to_string()
                        } else {
                            self.sketch.name.trim().to_string()
                        };
                        match crate::sketch::to_outline(&self.sketch, &name) {
                            Ok(o) => {
                                // Save it to the user library too: the desktop's
                                // "from the outline library" picker reads that
                                // directory and had no writer anywhere.
                                if let Some(root) = self.data_root.as_ref() {
                                    let dir = root.join("outlines");
                                    if let Err(e) =
                                        ringdesign_core::library::save_outline_in(&dir, &o)
                                    {
                                        log::warn!("save outline: {e}");
                                    }
                                }
                                let v = self.design.shank.adopt_outline(o);
                                self.design.shank.head.outline = v;
                                self.mark_dirty();
                                format!("{name}: on the head")
                            }
                            Err(e) => e.to_string(),
                        }
                    }
                    crate::sketch::Mode::Section => {
                        match crate::sketch::to_drop_curve(&self.sketch) {
                            Ok(c) => {
                                self.design.profile.drop_curve = c;
                                self.design.profile.style =
                                    ringdesign_core::ProfileStyle::Custom;
                                self.mark_dirty();
                                if self.sketch.allow_undercut {
                                    "section adopted — undercut allowed, the verdict will say"
                                        .to_string()
                                } else {
                                    "section adopted".to_string()
                                }
                            }
                            Err(e) => e.to_string(),
                        }
                    }
                };
                self.status = msg;
                self.sketch_mode = None;
                host.haptic(Haptic::Success);
            }
            if ui.button("Clear").clicked() {
                self.sketch.clear();
            }
            if ui.button("Cancel").clicked() {
                self.sketch_mode = None;
            }
        });
        if mode == crate::sketch::Mode::Face {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("name").small().weak());
                ui.add(egui::TextEdit::singleline(&mut self.sketch.name).desired_width(120.0));
            });
        } else {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .checkbox(&mut self.sketch.allow_undercut, "allow an undercut")
                    .on_hover_text(
                        "Let the section fall back on itself. That is what makes a profile \
                         uncastable in two-part sand — the line turns red while it is on.",
                    )
                    .changed()
                {
                    // Nothing to rebuild yet; the switch only changes what the
                    // curve is allowed to be when it is adopted.
                }
            });
        }
        // Pen-only while sketching, for the same reason the paint tab is: a
        // resting hand should not add a point to the boundary.
        let tool = paint::Tool::from_code(self.probe.tool);
        let accepts = paint::accepts(tool, self.brush.stylus_only);
        if crate::sketch::pad(ui, &mut self.sketch, mode, accepts) {
            ui.ctx().request_repaint();
        }
    }

    fn probe(&mut self, origin: [f32; 3], dir: [f32; 3]) {
        use ringdesign_core::field::Uv;
        let Some(mesh) = self.preview_mesh.clone() else { return };
        let Some((fi, world)) = raycast(&mesh, origin, dir) else {
            self.probe_info = None;
            return;
        };
        let theta = (world[1] as f64).atan2(world[0] as f64).to_degrees().rem_euclid(360.0);
        let r = (world[0] as f64).hypot(world[1] as f64);
        let inner_r = self.design.inner_radius_mm();
        let ctx = self.design.field_context();
        let section = ringdesign_core::castability::section_at(&self.design, &self.lib, theta, 160);
        let surface: Vec<_> = section.points.iter().filter(|p| p.surface).collect();
        let mut v_mm = 0.0;
        if surface.len() >= 2 {
            let total: f64 = surface
                .windows(2)
                .map(|w| ((w[1].r - w[0].r).powi(2) + (w[1].z - w[0].z).powi(2)).sqrt())
                .sum();
            let mut acc = 0.0;
            let mut best_d = f64::MAX;
            let mut at = 0.0;
            for w in surface.windows(2) {
                let seg = ((w[1].r - w[0].r).powi(2) + (w[1].z - w[0].z).powi(2)).sqrt();
                acc += seg;
                let d = (w[1].r - r).powi(2) + (w[1].z - world[2] as f64).powi(2);
                if d < best_d {
                    best_d = d;
                    at = acc;
                }
            }
            v_mm = at / total.max(1e-9) * ctx.band_v_len_mm;
        }
        let uv = Uv { u: ctx.u_of_theta(theta), v: v_mm };
        let h = self.design.layers.height(uv, &ctx, &self.lib);
        let class = self
            .cast
            .as_ref()
            .and_then(|c| c.classes.get(fi))
            .map(|k| k.label())
            .unwrap_or("-");
        let mut named = None;
        for e in self.design.layers.layers.iter().rev() {
            if !e.enabled {
                continue;
            }
            let m = e.window.mask(uv, &ctx) * e.opacity.max(0.0);
            if m <= 1e-4 {
                continue;
            }
            if e.layer.height(uv, &ctx, &self.lib).abs() * m > 5e-3 {
                named = Some(e.name.clone());
                break;
            }
        }
        self.probe_info = Some(format!(
            "{:.0} deg · v {:.2} · relief {:+.2} mm · wall {:.2} mm · {}{}",
            theta,
            v_mm,
            h,
            r - inner_r,
            class,
            named.map(|n| format!(" · {n}")).unwrap_or_default()
        ));
    }

    /// Labelled tabs and the Design-sheet toggle split the width; the rarer
    /// tabs collapse to icon squares so nothing ever runs off the edge.
    fn nav_bar(&mut self, ui: &mut egui::Ui, host: &Host) {
        const ROW_H: f32 = 40.0;
        const ICON_BTN: f32 = 44.0;
        const ICON_GAP: f32 = 2.0;
        let labeled_n =
            Tab::BAR.iter().filter(|(_, _, l)| !l.is_empty()).count() as f32 + 1.0;
        let icon_n = Tab::BAR.iter().filter(|(_, _, l)| l.is_empty()).count() as f32;
        let icon_cluster_w = icon_n * ICON_BTN + (icon_n - 1.0).max(0.0) * ICON_GAP;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let labeled_w = ((ui.available_width() - icon_cluster_w - 6.0 * labeled_n)
                / labeled_n)
                .max(58.0);

            for (tab, _, label) in Tab::BAR.iter().filter(|(_, _, l)| !l.is_empty()) {
                let selected = self.tab == *tab;
                let text = egui::RichText::new(*label).size(13.0);
                if crate::theme::selectable_label(
                    ui,
                    selected,
                    [labeled_w, ROW_H],
                    text,
                )
                .clicked()
                    && !selected
                {
                    self.tab = *tab;
                    host.haptic(Haptic::Light);
                }
            }

            // The Design sheet's toggle lives with the tabs but is not one:
            // it slides the editor up over whatever tab is showing.
            let text = egui::RichText::new("Design").size(13.0);
            if crate::theme::selectable_label(ui, self.design_open, [labeled_w, ROW_H], text)
                .on_hover_text("Slide the design controls over the live ring")
                .clicked()
            {
                self.design_open = !self.design_open;
                if self.design_open {
                    self.tab = Tab::Ring;
                }
                host.haptic(Haptic::Light);
            }

            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = ICON_GAP;
                ui.spacing_mut().button_padding = egui::vec2(0.0, 0.0);
                for (tab, icon, _) in Tab::BAR.iter().filter(|(_, _, l)| l.is_empty()) {
                    let selected = self.tab == *tab;
                    let resp = crate::theme::selectable_label(
                        ui,
                        selected,
                        [ICON_BTN, ROW_H],
                        egui::RichText::new(*icon).size(18.0),
                    )
                    .on_hover_text(tab.label());
                    if resp.clicked() && !selected {
                        self.tab = *tab;
                        host.haptic(Haptic::Light);
                    }
                }
            });
        });
    }

    /// The whole design editor: size, profile, shank, head, and the stock
    /// generators. Every control writes straight into the design and marks
    /// dirty, so the Ring tab shows the result on the next settled build.
    fn design_tab(&mut self, ui: &mut egui::Ui) {
        use ringdesign_core::field::SignetOutline;
        use ringdesign_core::profile::TOP_DEG;
        use ringdesign_core::{ProfileStyle, RingSize, ShankKind};

        if self.driven_banner(ui) {
            return;
        }
        let mut dirty = false;
        ui.scope(|ui| {
            ui.label(egui::RichText::new("ring").weak());
            let mut size = self.design.size.0;
            if ui
                .add(egui::Slider::new(&mut size, 3.0..=13.0).step_by(0.25).text("US size"))
                .changed()
            {
                let next = RingSize::new(size);
                // A detent per quarter step: the slider already snaps there, and
                // a tick makes the step findable without watching the number.
                if next.0 != self.design.size.0 {
                    self.detent = true;
                }
                self.design.size = next;
                dirty = true;
            }

            ui.separator();
            ui.label(egui::RichText::new("casting").weak());
            {
                use ringdesign_core::castability::{CastProcess, SandProcess};
                let cur = self.design.draft.process;
                ui.horizontal_wrapped(|ui| {
                    for &p in CastProcess::ALL {
                        if ui.add(crate::theme::selectable(cur == p, p.label())).clicked() && cur != p
                        {
                            crate::casting::set_process(&mut self.design.draft, p);
                            dirty = true;
                        }
                    }
                });
                if cur == CastProcess::SandTwoPart {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("sand").small().weak());
                        for &s in SandProcess::ALL {
                            if ui.small_button(s.label()).clicked() {
                                s.apply(&mut self.design.draft);
                                dirty = true;
                            }
                        }
                    });
                }
                ui.label(
                    egui::RichText::new(format!(
                        "{:.1}° draft · {:.2} mm fill · {:.2} mm detail",
                        self.design.draft.min_draft_deg,
                        self.design.draft.min_section_mm,
                        self.design.draft.min_detail_mm
                    ))
                    .small()
                    .weak(),
                );
            }

            ui.separator();
            ui.label(egui::RichText::new("profile").weak());
            let current = self.design.profile.style;
            egui::ComboBox::from_label("style")
                .selected_text(current.label())
                .show_ui(ui, |ui| {
                    for &style in ProfileStyle::ALL {
                        if ui.selectable_label(current == style, style.label()).clicked()
                            && current != style
                        {
                            self.design.profile.apply_style(style);
                            dirty = true;
                        }
                    }
                });
            dirty |= ui
                .add(egui::Slider::new(&mut self.design.profile.width_mm, 2.0..=18.0).text("width mm"))
                .changed();
            dirty |= ui
                .add(
                    egui::Slider::new(&mut self.design.profile.thickness_mm, 1.0..=5.0)
                        .text("thickness mm"),
                )
                .changed();
            dirty |= ui
                .add(
                    egui::Slider::new(&mut self.design.profile.comfort_fit_mm, 0.0..=0.6)
                        .text("comfort fit mm"),
                )
                .changed();
            if ui
                .button("Draw the section")
                .on_hover_text(
                    "Sketch half the cross-section with the pen — crest at the left, edge at \
                     the right. Monotone by default, which is the no-undercut guarantee.",
                )
                .clicked()
            {
                self.sketch_mode = Some(crate::sketch::Mode::Section);
                self.sketch.clear();
            }
            if ui
                .button("Square the sides")
                .on_hover_text(
                    "Drop the side draft and shrink the edge fillet — squared sides are the                      castable ground for deep relief.",
                )
                .clicked()
            {
                self.design.profile.flatten_sides();
                dirty = true;
            }

            ui.separator();
            ui.label(egui::RichText::new("shank").weak());
            let was = self.design.shank.kind;
            egui::ComboBox::from_label("shape")
                .selected_text(format!("{:?}", was))
                .show_ui(ui, |ui| {
                    for &kind in ShankKind::ALL {
                        if ui
                            .selectable_label(self.design.shank.kind == kind, format!("{kind:?}"))
                            .clicked()
                            && self.design.shank.kind != kind
                        {
                            self.design.shank.kind = kind;
                            if kind == ShankKind::Signet {
                                let w = self.design.profile.width_mm;
                                self.design.shank.apply_signet(w);
                            }
                            dirty = true;
                        }
                    }
                });
            dirty |= ui
                .add(egui::Slider::new(&mut self.design.shank.amount, 0.0..=1.0).text("amount"))
                .changed();
            if matches!(self.design.shank.kind, ShankKind::Wave | ShankKind::Twist) {
                let mut waves = self.design.shank.waves as i32;
                if ui.add(egui::Slider::new(&mut waves, 1..=6).text("waves")).changed() {
                    self.design.shank.waves = waves.max(1) as u32;
                    dirty = true;
                }
            }

            if self.design.shank.kind == ShankKind::Signet {
                ui.separator();
                ui.label(egui::RichText::new("head").weak());
                let cur = self.design.shank.head.outline;
                egui::ComboBox::from_label("outline")
                    .selected_text(cur.label())
                    .show_ui(ui, |ui| {
                        for &o in SignetOutline::ALL {
                            if ui.selectable_label(cur == o, o.label()).clicked() && cur != o {
                                self.design.shank.head.outline = o;
                                dirty = true;
                            }
                        }
                    });
                if ui
                    .button("Draw the face")
                    .on_hover_text("Sketch a closed plan with the pen — it becomes this head's outline")
                    .clicked()
                {
                    self.sketch_mode = Some(crate::sketch::Mode::Face);
                    self.sketch.clear();
                }
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut self.design.shank.head.length_mm, 6.0..=20.0)
                            .text("face length mm"),
                    )
                    .changed();
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut self.design.shank.head.rise_mm, 0.0..=2.2)
                            .text("rise mm"),
                    )
                    .changed();
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut self.design.shank.head.rim_round_mm, 0.0..=2.0)
                            .text("rim round mm"),
                    )
                    .changed();
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut self.design.shank.head.table_dome_mm, 0.0..=3.0)
                            .text("table dome mm"),
                    )
                    .changed();
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut self.design.shank.head.dome, 0.0..=1.0)
                            .text("cut dome"),
                    )
                    .on_hover_text(
                        "1 cuts the face from a swollen dome: no pinched corners, no \
                         prism walls. Concave outlines (heart, shield) soften there.",
                    )
                    .changed();

                let mut second = !self.design.shank.extra_heads.is_empty();
                if ui.checkbox(&mut second, "second head (toi et moi)").changed() {
                    if second {
                        let mut h = self.design.shank.head.clone();
                        h.outline = SignetOutline::Heart;
                        h.length_mm = (h.length_mm * 0.8).max(6.0);
                        h.theta_deg = TOP_DEG + 26.0;
                        self.design.shank.head.theta_deg = TOP_DEG - 26.0;
                        self.design.shank.extra_heads.push(h);
                    } else {
                        self.design.shank.extra_heads.clear();
                        self.design.shank.head.theta_deg = TOP_DEG;
                    }
                    dirty = true;
                }
                if let Some(h) = self.design.shank.extra_heads.first_mut() {
                    let cur = h.outline;
                    egui::ComboBox::from_label("second outline")
                        .selected_text(cur.label())
                        .show_ui(ui, |ui| {
                            for &o in SignetOutline::ALL {
                                if ui.selectable_label(cur == o, o.label()).clicked() && cur != o {
                                    h.outline = o;
                                    dirty = true;
                                }
                            }
                        });
                    dirty |= ui
                        .add(
                            egui::Slider::new(&mut h.length_mm, 5.0..=16.0)
                                .text("second face mm"),
                        )
                        .changed();
                    let mut sep = h.theta_deg - self.design.shank.head.theta_deg;
                    if ui
                        .add(egui::Slider::new(&mut sep, 24.0..=110.0).text("separation deg"))
                        .changed()
                    {
                        self.design.shank.head.theta_deg = TOP_DEG - sep * 0.5;
                        h.theta_deg = TOP_DEG + sep * 0.5;
                        dirty = true;
                    }
                }
            }

            if self.design.shank.kind == ShankKind::Keyframes {
                ui.separator();
                ui.label(egui::RichText::new("stations").weak());
                let keys = &mut self.design.shank.keys;
                let mut remove: Option<usize> = None;
                for (i, k) in keys.iter_mut().enumerate() {
                    ui.push_id(i, |ui| {
                        ui.horizontal(|ui| {
                            dirty |= ui
                                .add(
                                    egui::DragValue::new(&mut k.theta_deg)
                                        .speed(0.5)
                                        .range(0.0..=360.0)
                                        .suffix(" deg"),
                                )
                                .changed();
                            dirty |= ui
                                .add(
                                    egui::DragValue::new(&mut k.width_scale)
                                        .speed(0.01)
                                        .range(0.3..=3.0)
                                        .prefix("w "),
                                )
                                .changed();
                            dirty |= ui
                                .add(
                                    egui::DragValue::new(&mut k.thickness_scale)
                                        .speed(0.01)
                                        .range(0.3..=3.0)
                                        .prefix("t "),
                                )
                                .changed();
                            dirty |= ui
                                .add(
                                    egui::DragValue::new(&mut k.crown_scale)
                                        .speed(0.01)
                                        .range(0.0..=2.5)
                                        .prefix("c "),
                                )
                                .changed();
                            if ui.small_button("x").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    keys.remove(i);
                    dirty = true;
                }
                if keys.len() < 16 && ui.button("Add station").clicked() {
                    let presets = [90.0, 270.0, 0.0, 180.0, 45.0, 135.0, 225.0, 315.0];
                    let taken: Vec<f64> = keys.iter().map(|k| k.theta_deg).collect();
                    let theta = presets
                        .iter()
                        .copied()
                        .find(|p| taken.iter().all(|t| (t - p).abs() > 1.0))
                        .unwrap_or(90.0);
                    keys.push(ringdesign_core::profile::ShankKey {
                        theta_deg: theta,
                        ..Default::default()
                    });
                    dirty = true;
                }
                ui.label(
                    egui::RichText::new(
                        "Width, thickness and crown per station, blended smoothly round the ring.",
                    )
                    .small()
                    .weak(),
                );
            }

            ui.separator();
            ui.separator();
            ui.label(egui::RichText::new("stones").weak());
            let v_len = self.design.field_context().band_v_len_mm;
            dirty |= crate::stones::picker(ui, &mut self.stone, v_len);

            ui.horizontal_wrapped(|ui| {
                if ui
                    .button("Auto pavé")
                    .on_hover_text("Pack the chosen arc with seats for the chosen stone")
                    .clicked()
                {
                    let spec = self.stone.spec();
                    match ringdesign_core::pave::fill(&self.design, &spec) {
                        Some((entry, out)) => {
                            self.design.layers.layers.push(entry);
                            self.selected_layer = Some(self.design.layers.layers.len() - 1);
                            self.status = match out.note {
                                Some(n) => format!("pavé: {} seats · {n}", out.seats),
                                None => format!("pavé: {} seats in {} rows", out.seats, out.rows),
                            };
                            dirty = true;
                        }
                        // The refusal is always the band or the region, and the
                        // user just chose both — say which one to move.
                        None => {
                            self.status = if self.stone.v_band.is_some() {
                                "that strip is too narrow for this stone — widen the band or \
                                 pick a smaller one"
                                    .into()
                            } else {
                                "no side face to fill — square the sides, or turn off \
                                 \"on the side face\""
                                    .into()
                            }
                        }
                    }
                }
                if ui
                    .button("Channel set")
                    .on_hover_text("Rails and a recessed channel on the wider side face")
                    .clicked()
                {
                    match ringdesign_core::pave::channel_set(&self.design, self.stone.gem(), 0.6) {
                        Some(entry) => {
                            self.design.layers.layers.push(entry);
                            self.selected_layer = Some(self.design.layers.layers.len() - 1);
                            self.status = "channel set added".into();
                            dirty = true;
                        }
                        None => {
                            self.status = format!(
                                "side face too narrow — a {:.1} mm stone needs about {:.1} mm of face",
                                self.stone.w_mm,
                                self.stone.w_mm + 1.4
                            )
                        }
                    }
                }
                if ui.button("Clear layers").on_hover_text("Remove every layer, keep the band").clicked()
                    && !self.design.layers.layers.is_empty()
                {
                    self.design.layers.layers.clear();
                    self.selected_layer = None;
                    self.status = "layers cleared — Undo brings them back".into();
                    dirty = true;
                }
            });
        });
        if dirty {
            self.mark_dirty();
        }
    }

    /// Index of a drawing by name, creating it (and the layer that shows it) on first use.
    ///
    /// The drawing and the layer are two halves of one thing: strokes travel inside the design so a
    /// shared file is self-contained, and the layer is an ordinary `TilingLayer` so every existing
    /// blend, window and lattice control applies and the desktop opens it unchanged.
    fn ensure_drawing(&mut self, name: &str, w: u32, h: u32, wrap_y: bool, repeats: u32) -> usize {
        // The band is the shared convention: one seam-wrapped cell over the
        // whole band, identical to the desktop's paint mode, so files
        // roundtrip between devices.
        if name == paint::BAND_ALPHA && repeats <= 1 {
            let created = !self.design.drawn.iter().any(|d| d.name == name);
            let index = ringdesign_core::paint::ensure_band_layer(&mut self.design);
            if created {
                self.mark_dirty();
            }
            return index;
        }
        if let Some(i) = self.design.drawn.iter().position(|d| d.name == name) {
            return i;
        }
        let mut d = DrawnAlpha::new(name, w, h);
        d.wrap_x = true;
        d.wrap_y = wrap_y;
        self.design.drawn.push(d);

        if !self.design.layers.layers.iter().any(|e| matches!(&e.layer,
            Layer::Tiling(t) if t.alpha == name))
        {
            let ctx = self.design.field_context();
            let mut t = TilingLayer::default_for(name.to_string(), &ctx);
            t.repeats_around = repeats.max(1);
            t.rows = 1;
            t.continuous = true;
            // The alpha stores depth as a fraction of the 1.6 mm maximum, so the layer's height is
            // that maximum and the composite gives back the millimetres the pen asked for.
            t.height_mm = paint::MAX_RELIEF_MM;
            if repeats <= 1 {
                // A band-wide drawing covers the whole section; a tile keeps its default span.
                t.v_center_mm = ctx.band_v_len_mm * 0.5;
                t.v_span_mm = ctx.band_v_len_mm;
            }
            self.design.layers.layers.push(LayerEntry::new(name.to_string(), Layer::Tiling(t)));
            self.mark_dirty();
        }
        self.design.drawn.len() - 1
    }

    /// Re-bake one drawing into the shared library. `Arc::make_mut` deep-copies the whole library,
    /// so this is done on stroke end rather than per sample — painting is a continuous stream of
    /// edits against a continuously rebuilding preview, which is the pathological case for it.
    fn bake(&mut self, index: usize) {
        let Some(d) = self.design.drawn.get(index) else { return };
        if d.is_empty() {
            return;
        }
        let baked = d.rasterize();
        Arc::make_mut(&mut self.lib).insert(baked);
    }

    /// While a graph drives the design, the editing tabs say so and offer
    /// the way out instead of edits the next evaluation would overwrite.
    fn driven_banner(&mut self, ui: &mut egui::Ui) -> bool {
        if !self.graph.is_driven() {
            return false;
        }
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 6))
            .corner_radius(6.0)
            .stroke(egui::Stroke::new(1.0, crate::theme::INK_DIM))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Driven by the graph").strong());
                ui.label(
                    egui::RichText::new("Edit the nodes in the Graph tab, or bake the graph to edit here.")
                        .small()
                        .color(crate::theme::INK_DIM),
                );
                ui.horizontal(|ui| {
                    if ui.button("Open graph").clicked() {
                        self.tab = Tab::Graph;
                    }
                    if ui.button("Bake").clicked() && self.graph.bake(&mut self.design) {
                        self.status = "baked: the graph is gone and the design is yours".into();
                        self.mark_dirty();
                    }
                });
            });
        true
    }

    /// The node editor over the design's graph.
    fn graph_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        use ringdesign_graph::templates;
        if !self.graph.is_driven() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("No graph behind this design yet").size(16.0));
                ui.label(
                    egui::RichText::new("Turn the design into nodes you can rewire, or start from a graph.")
                        .small()
                        .color(crate::theme::INK_DIM),
                );
                ui.add_space(12.0);
                if ui.button("Convert this design to a graph").clicked() {
                    match self.graph.convert(&mut self.design, &self.lib) {
                        Ok(()) => {
                            self.status = "converted: the graph drives the design now".into();
                            self.mark_dirty();
                            host.haptic(Haptic::Success);
                        }
                        Err(e) => {
                            self.status = format!("could not convert: {e}");
                            host.haptic(Haptic::Error);
                        }
                    }
                }
                ui.add_space(6.0);
                if ui.button("Start from the simple graph").clicked() {
                    self.graph.open(&mut self.design, templates::simple());
                    self.mark_dirty();
                }
                ui.add_space(12.0);
                ui.label(egui::RichText::new("template graphs").weak());
                for (name, g) in templates::all() {
                    if ui.button(name).clicked() {
                        self.graph.open(&mut self.design, g);
                        self.status = format!("started from the {name} graph");
                        self.mark_dirty();
                    }
                }
            });
            return;
        }
        enum Act {
            Arrange,
            Fit,
            Lock(bool),
            Bake,
        }
        let mut act = None;
        let nodes = self.graph.ed.as_ref().map(|e| e.graph().nodes.len()).unwrap_or(0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Arrange").clicked() {
                act = Some(Act::Arrange);
            }
            if ui.button("Fit").clicked() {
                act = Some(Act::Fit);
            }
            let locked = self.graph.locked;
            if ui.selectable_label(locked, if locked { "Locked" } else { "Lock" }).clicked() {
                act = Some(Act::Lock(!locked));
            }
            if ui.button("Bake").clicked() {
                act = Some(Act::Bake);
            }
            ui.label(egui::RichText::new(format!("{nodes} nodes")).small().color(crate::theme::INK_DIM));
        });
        if !self.graph.errors.is_empty() {
            ui.label(
                egui::RichText::new(self.graph.errors.join("; "))
                    .small()
                    .color(egui::Color32::from_rgb(230, 190, 90)),
            );
        }
        match act {
            Some(Act::Arrange) => {
                let reg = self.graph.reg.clone();
                if let Some(ed) = &mut self.graph.ed {
                    ed.arrange(&reg);
                }
                if self.graph.changed(&mut self.design) {
                    self.mark_dirty();
                }
            }
            Some(Act::Fit) => {
                if let Some(ed) = &mut self.graph.ed {
                    ed.fit();
                }
            }
            Some(Act::Lock(l)) => {
                self.graph.set_locked(l);
                host.haptic(Haptic::Light);
            }
            Some(Act::Bake) => {
                if self.graph.bake(&mut self.design) {
                    self.status = "baked: the graph is gone and the design is yours".into();
                    self.mark_dirty();
                }
                return;
            }
            None => {}
        }
        ui.label(
            egui::RichText::new("drag to pan · pinch to zoom · long-press for menus · drag pins to wire")
                .small()
                .color(crate::theme::INK_DIM),
        );
        let reg = self.graph.reg.clone();
        let Some(mut ed) = self.graph.ed.take() else { return };
        let resp = egui::Frame::new()
            .fill(egui::Color32::from_rgb(18, 18, 20))
            .corner_radius(8.0)
            .show(ui, |ui| ed.show(&reg, ui, "phone-graph"))
            .inner;
        self.graph.ed = Some(ed);
        if let Some(r) = resp.refused {
            self.status = format!("wire refused: {r}");
            host.haptic(Haptic::Error);
        }
        if let Some(id) = resp.selected {
            if let Some(n) = self.graph.ed.as_ref().and_then(|e| e.node(id)) {
                self.status = match &n.label {
                    Some(l) => format!("{l} ({})", n.kind),
                    None => n.kind.clone(),
                };
            }
        }
        if resp.changed && self.graph.changed(&mut self.design) {
            self.mark_dirty();
        }
    }

    fn paint_tab(&mut self, ui: &mut egui::Ui, host: &Host, domain: Domain) {
        if self.driven_banner(ui) {
            return;
        }
        let (name, w, h, wrap_y, repeats) = match domain {
            Domain::Band => (BAND_ALPHA, BAND_W, BAND_H, false, 1),
            Domain::Tile => (TILE_ALPHA, TILE_EDGE, TILE_EDGE, true, self.tile_repeats),
        };
        let index = self.ensure_drawing(name, w, h, wrap_y, repeats);

        egui::Panel::top(egui::Id::new(("brush", name))).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("brush");
                ui.add(egui::Slider::new(&mut self.brush.frac, 0.002..=0.08).show_value(false));
                ui.label("depth");
                ui.add(egui::Slider::new(&mut self.brush.depth, 0.05..=1.0).show_value(false));
                ui.label(format!("{:.2} mm", paint::wanted_mm(1.0, self.brush.depth)));
            });
            ui.horizontal_wrapped(|ui| {
                ui.toggle_value(&mut self.brush.erase, "Carve");
                if self.has_stylus {
                    ui.toggle_value(&mut self.brush.stylus_only, "Pen only");
                }
                if ui.button("Undo").clicked() {
                    if let Some(d) = self.design.drawn.get_mut(index) {
                        d.strokes.pop();
                    }
                    self.bake(index);
                    self.mark_dirty();
                    host.haptic(Haptic::Light);
                }
                if ui.button("Clear").clicked() {
                    if let Some(d) = self.design.drawn.get_mut(index) {
                        d.strokes.clear();
                    }
                    self.bake(index);
                    self.mark_dirty();
                    host.haptic(Haptic::Warning);
                }
                if domain == Domain::Tile
                    && ui
                        .add(egui::Slider::new(&mut self.tile_repeats, 1..=120).text("around"))
                        .changed()
                {
                    self.set_repeats(TILE_ALPHA, self.tile_repeats);
                    self.mark_dirty();
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            // Taken out so the canvas can hold `&mut DrawnAlpha` while the rest of the design is
            // still borrowed for the height field underneath it.
            let Some(slot) = self.design.drawn.get_mut(index) else { return };
            let mut drawing = std::mem::take(slot);
            let ctx = self.design.field_context();
            let view = match domain {
                Domain::Band => &mut self.band_view,
                Domain::Tile => &mut self.tile_view,
            };
            let out = canvas::show(
                ui,
                CanvasInput {
                    domain,
                    ctx: &ctx,
                    layers: &self.design.layers,
                    lib: &self.lib,
                    target: Some(&mut drawing),
                    view,
                    brush_frac: self.brush.frac,
                    soft: self.brush.soft,
                    depth_scale: self.brush.depth,
                    erase_toggle: self.brush.erase,
                    stylus_only: self.brush.stylus_only,
                    probe: self.probe,
                    active_touch: &mut self.active_touch,
                    floor_mm: self.design.draft.min_detail_mm,
                },
            );
            if let Some(slot) = self.design.drawn.get_mut(index) {
                *slot = drawing;
            }

            if out.readout.is_some() {
                self.readout = out.readout;
            }
            if let Some(d) = out.pick_depth {
                self.brush.depth = d;
                self.readout = Some(format!(
                    "depth picked up: {:.2} mm",
                    paint::wanted_mm(1.0, d)
                ));
                host.haptic(Haptic::Selection);
            }
            if out.undo_step {
                if let Some(design) = self.history.undo() {
                    self.apply_history(design, "undone");
                    host.haptic(Haptic::Light);
                }
            }
            // Haptics on the geometry rather than on the tap. The eye is on the
            // stroke, not on the status line at the bottom of the screen, so a
            // refused depth or a zone crossing reads better through the skin.
            if out.clamped && !self.was_clamped {
                // Once per stroke, at onset — a tick per sample would buzz
                // continuously for as long as the pen stayed on the crest.
                host.haptic(Haptic::Warning);
            }
            self.was_clamped = out.clamped;
            if out.zone.is_some() && out.zone != self.last_zone {
                if self.last_zone.is_some() {
                    host.haptic(Haptic::Light);
                }
                self.last_zone = out.zone;
            }
            if out.stroke_ended {
                self.bake(index);
                self.mark_dirty();
                host.haptic(Haptic::Selection);
                self.was_clamped = false;
                self.last_zone = None;
            } else if out.painted {
                // Redraw the strokes without paying for a mesh rebuild mid-gesture.
                ui.ctx().request_repaint();
            }
            if out.wants_repaint {
                ui.ctx().request_repaint();
            }
        });
    }

    fn set_repeats(&mut self, name: &str, repeats: u32) {
        for e in &mut self.design.layers.layers {
            if let Layer::Tiling(t) = &mut e.layer {
                if t.alpha == name {
                    t.repeats_around = repeats.max(1);
                }
            }
        }
    }

    /// Put a library alpha on the band as an ordinary tiling layer, replacing whatever was there.
    fn apply_alpha(&mut self, name: &str) {
        let ctx = self.design.field_context();
        let existing = self.design.layers.layers.iter_mut().find(|e| e.name == PATTERN_LAYER);
        if let Some(entry) = existing {
            if let Layer::Tiling(t) = &mut entry.layer {
                t.alpha = name.to_string();
                t.repeats_around = self.pattern_repeats.max(1);
                t.height_mm = self.pattern_height_mm;
                self.picked_alpha = Some(name.to_string());
                self.mark_dirty();
                return;
            }
        }
        let mut t = TilingLayer::default_for(name.to_string(), &ctx);
        t.repeats_around = self.pattern_repeats.max(1);
        t.height_mm = self.pattern_height_mm;
        // Ornament belongs on the faces that pull straight out of the sand. `fit_to_side_faces`
        // snaps the band to them when the profile actually has any; on a half-round it does not,
        // and the default centred span is the honest fallback.
        t.fit_to_side_faces(&ctx, ringdesign_core::field::SIDE_FACE_MIN_DRAFT_DEG);
        self.design
            .layers
            .layers
            .push(LayerEntry::new(PATTERN_LAYER.to_string(), Layer::Tiling(t)));
        self.picked_alpha = Some(name.to_string());
        self.mark_dirty();
    }

    fn update_pattern_layer(&mut self) {
        for e in &mut self.design.layers.layers {
            if e.name == PATTERN_LAYER {
                if let Layer::Tiling(t) = &mut e.layer {
                    t.repeats_around = self.pattern_repeats.max(1);
                    t.height_mm = self.pattern_height_mm;
                }
            }
        }
        self.mark_dirty();
    }

    /// The cell the Alphas grid would lay a pattern down on, for the sand readout.
    ///
    /// Built from the pattern layer if one exists, otherwise from the same
    /// `default_for` + `fit_to_side_faces` pair `apply_alpha` uses — so the
    /// millimetres shown are the millimetres a tap would produce. The cell does
    /// not depend on which alpha, so one probe serves the whole grid.
    fn pattern_cell(&self) -> Option<liblib::CellScale> {
        let ctx = self.design.field_context();
        let existing = self
            .design
            .layers
            .layers
            .iter()
            .find(|e| e.name == PATTERN_LAYER)
            .and_then(|e| match &e.layer {
                Layer::Tiling(t) => Some(t.clone()),
                _ => None,
            });
        let t = existing.unwrap_or_else(|| {
            let mut t = TilingLayer::default_for(
                self.picked_alpha.clone().unwrap_or_default(),
                &ctx,
            );
            t.fit_to_side_faces(&ctx, ringdesign_core::field::SIDE_FACE_MIN_DRAFT_DEG);
            t
        });
        let mut t = t;
        t.repeats_around = self.pattern_repeats.max(1);
        let (cell_w_mm, cell_h_mm) = t.cell_size(&ctx);
        Some(liblib::CellScale {
            cell_w_mm,
            cell_h_mm,
            floor_mm: self.design.draft.min_detail_mm,
        })
    }

    /// The findings sheet: every DFM message as text, and a one-tap fit for the
    /// tilings whose repeat count is the thing that is wrong.
    fn dfm_sheet(&mut self, ui: &mut egui::Ui) {
        use ringdesign_core::dfm::{fit_to_floor, FloorFit};

        let floor = self.design.draft.min_detail_mm;
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!("the sand holds {floor:.2} mm here"))
                    .small()
                    .weak(),
            );
            if ui.small_button("close").clicked() {
                self.dfm_open = false;
            }
        });
        ui.separator();

        let mut fit: Option<usize> = None;
        for f in &self.dfm {
            ui.label(egui::RichText::new(&f.label).strong());
            ui.label(egui::RichText::new(&f.message).small());
            // Only a tiling has a repeat count to solve for; a stamp or a bead
            // row is fixed by its own size and there is nothing to fit.
            if self
                .design
                .layers
                .layers
                .get(f.layer)
                .is_some_and(|e| first_tiling(&e.layer).is_some())
                && ui
                    .button("Fit to the floor")
                    .on_hover_text("Set the repeats to the most this pattern can carry and still cast")
                    .clicked()
            {
                fit = Some(f.layer);
            }
            ui.add_space(6.0);
        }

        let Some(i) = fit else { return };
        let ctx = self.design.field_context();
        let lib = std::sync::Arc::clone(&self.lib);
        let Some(entry) = self.design.layers.layers.get_mut(i) else { return };
        let name = entry.name.clone();
        let Some(t) = first_tiling_mut(&mut entry.layer) else { return };
        self.status = match fit_to_floor(t, &lib, &ctx, floor) {
            FloorFit::Repeats(n) => {
                self.mark_dirty();
                format!("{name}: {n} repeats — the most that still casts")
            }
            // The layer is left untouched here on purpose: no repeat count
            // helps, and the figure is what the face has to measure instead.
            FloorFit::NeedsTallerCell { min_cell_h_mm } => format!(
                "{name}: no repeat count clears {floor:.2} mm — the cell must be at least \
                 {min_cell_h_mm:.2} mm tall. Widen the face or drop a row."
            ),
            FloorFit::Unmeasurable => {
                format!("{name}: nothing measurable in that mask")
            }
        };
    }

    /// Kick off a tile generation on its own thread.
    ///
    /// Off the UI thread for the same reason the exports are: a diffusion run is
    /// seconds to minutes, and an ANR is not a progress bar.
    #[cfg(feature = "local-npu")]
    fn start_generate(&mut self, ctx: &egui::Context) {
        let Some(pack) = crate::npu::first(&self.packs, crate::npu::Kind::Sd15).cloned() else {
            return;
        };
        let Some(lib_dir) = self.native_lib_dir.clone() else {
            self.status = "no QNN runtime in this build".into();
            return;
        };
        let prompt = self.prompt.trim().to_string();
        let seed = self.design.layers.layers.len() as u64 ^ prompt.len() as u64;
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("tile-gen".into())
            .spawn(move || {
                let out = crate::npu::device::generate_tile(
                    &pack,
                    std::path::Path::new(&lib_dir),
                    &prompt,
                    20,
                    seed,
                    |_, _| {},
                );
                let _ = tx.send(out);
                ctx.request_repaint();
            })
            .ok();
        self.gen_job = Some(rx);
        self.status = "generating…".into();
    }

    /// Without the feature there is nothing to start, and the button that calls
    /// this is never shown because no pack can classify.
    #[cfg(not(feature = "local-npu"))]
    fn start_generate(&mut self, _ctx: &egui::Context) {
        self.status = "this build has no on-device models".into();
    }

    /// Take a finished generation and put it in the library, with the sand's
    /// verdict on it.
    fn poll_generate(&mut self, host: &Host) {
        let Some(rx) = self.gen_job.as_ref() else { return };
        let Ok(done) = rx.try_recv() else { return };
        self.gen_job = None;
        match done {
            Ok(png) => {
                let name = format!("Gen {}", self.prompt.trim());
                match ringdesign_core::alpha::Alpha::from_bytes(&name, &png) {
                    Ok(a) => {
                        // The point of doing this in *this* app: measure it
                        // before it is offered. A tile finer than the detail
                        // floor casts as mush however good it looks.
                        let finest = a.min_feature_px();
                        Arc::make_mut(&mut self.lib).insert(a);
                        self.thumbs.forget(&name);
                        self.status = match finest {
                            Some((ink, gap)) => format!(
                                "{name}: finest {:.0} texels — check the mm on the tile",
                                ink.min(gap)
                            ),
                            None => format!("{name}: added"),
                        };
                        host.haptic(Haptic::Success);
                    }
                    Err(e) => self.status = format!("generated image unreadable: {e}"),
                }
            }
            Err(e) => {
                self.status = e;
                host.haptic(Haptic::Error);
            }
        }
    }

    /// Embed every library alpha once, so "more like this" is a local sort.
    ///
    /// Synchronous and deliberately explicit — it is a one-off pass over the
    /// whole library behind a button, not something that should happen quietly
    /// while someone is drawing.
    #[cfg(feature = "local-npu")]
    fn embed_library(&mut self) {
        let Some(pack) = crate::npu::first(&self.packs, crate::npu::Kind::Clip).cloned() else {
            return;
        };
        let Some(lib_dir) = self.native_lib_dir.clone() else { return };
        let dir = std::path::PathBuf::from(lib_dir);
        let mut done = 0usize;
        for name in self.lib.names() {
            let Some(a) = self.lib.get(&name) else { continue };
            let hash = crate::similar::content_hash(a);
            if self.embeddings.get(&name, hash).is_some() {
                continue;
            }
            let Ok(png) = a.to_png16() else { continue };
            match crate::npu::device::embed_png(&pack, &dir, &png) {
                Ok(e) => {
                    self.embeddings.insert(&name, hash, e);
                    done += 1;
                }
                Err(e) => {
                    self.status = format!("embed failed: {e}");
                    return;
                }
            }
        }
        self.status = format!("{done} embedded, {} in the index", self.embeddings.len());
    }

    #[cfg(not(feature = "local-npu"))]
    fn embed_library(&mut self) {}

    /// Rank the library against one entry and filter the grid to the result.
    fn show_similar_to(&mut self, name: &str) {
        let Some(a) = self.lib.get(name) else { return };
        let hash = crate::similar::content_hash(a);
        let Some(q) = self.embeddings.get(name, hash).map(|e| e.to_vec()) else {
            self.status = "index that alpha first — Index library".into();
            return;
        };
        let ranked: Vec<String> =
            self.embeddings.rank(&q, Some(name)).into_iter().map(|(n, _)| n).collect();
        self.status = format!("{} like {name}", ranked.len());
        self.similar_to = Some((name.to_string(), ranked));
    }

    fn alphas_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        if self.driven_banner(ui) {
            return;
        }
        egui::Panel::top(egui::Id::new("alpha_tools")).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("find");
                ui.add(egui::TextEdit::singleline(&mut self.alpha_filter).desired_width(110.0));
                if ui.button("x").clicked() {
                    self.alpha_filter.clear();
                }
            });
            ui.horizontal_wrapped(|ui| {
                let mut changed = false;
                changed |= ui
                    .add(egui::Slider::new(&mut self.pattern_repeats, 1..=200).text("around"))
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.pattern_height_mm, 0.02..=1.6)
                            .text("mm")
                            .logarithmic(true),
                    )
                    .changed();
                if changed {
                    self.update_pattern_layer();
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("regenerate at").weak());
                for size in [128usize, 256, 512] {
                    if ui.selectable_label(self.builtin_size == size, size.to_string()).clicked()
                        && self.builtin_size != size
                    {
                        self.builtin_size = size;
                        liblib::regenerate_builtins(Arc::make_mut(&mut self.lib), size);
                        // The design's own drawings are not procedural; put them back on top.
                        let lib = Arc::make_mut(&mut self.lib);
                        self.design.bake_all(lib);
                        self.thumbs.clear();
                        self.mark_dirty();
                        host.haptic(Haptic::Light);
                    }
                }
                // Only when a pack is actually present: an offer the app cannot
            // honour is worse than no offer.
            if crate::npu::first(&self.packs, crate::npu::Kind::Sd15).is_some() {
                ui.separator();
                ui.label(egui::RichText::new("describe").small().weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.prompt)
                        .hint_text("a woven basket texture")
                        .desired_width(150.0),
                );
                let busy = self.gen_job.is_some();
                if ui
                    .add_enabled(!busy && !self.prompt.trim().is_empty(), egui::Button::new("Generate"))
                    .on_hover_text("Make a seamless tile on the NPU, then measure it against the sand")
                    .clicked()
                {
                    self.start_generate(ui.ctx());
                }
                if busy {
                    ui.spinner();
                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                }
            }
            if crate::npu::first(&self.packs, crate::npu::Kind::Clip).is_some() {
                if ui
                    .button("Index library")
                    .on_hover_text("Embed every pattern once so \"more like this\" is instant")
                    .clicked()
                {
                    self.embed_library();
                }
                if self.similar_to.is_some() && ui.button("Show all").clicked() {
                    self.similar_to = None;
                }
            }
            if ui.button("From photo").clicked() {
                    self.picker_open = !self.picker_open;
                    if self.picker_open && self.photos.is_empty() {
                        self.photos = host.list_device_media(false, 60);
                    }
                }
                if self.picked_alpha.is_some() && ui.button("Remove").clicked() {
                    self.design.layers.layers.retain(|e| e.name != PATTERN_LAYER);
                    self.picked_alpha = None;
                    self.mark_dirty();
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.picker_open {
                self.photo_picker(ui, host);
                return;
            }
            let scale = self.pattern_cell();
            let only = self.similar_to.as_ref().map(|(_, v)| v.as_slice());
            let picked = liblib::grid(
                ui,
                &self.lib,
                &mut self.thumbs,
                self.picked_alpha.as_deref(),
                &self.alpha_filter,
                scale,
                only,
            );
            match picked {
                liblib::Pick::Use(name) => {
                    self.apply_alpha(&name);
                    host.haptic(Haptic::Selection);
                }
                liblib::Pick::Preview(name) => {
                    // A long press asks "what else is like this" when the index
                    // exists, and otherwise says what it always did.
                    if !self.embeddings.is_empty() {
                        self.show_similar_to(&name);
                    } else {
                        self.status = format!(
                            "{name}: {}",
                            self.lib
                                .get(&name)
                                .map(|a| format!("{}x{}", a.width, a.height))
                                .unwrap_or_default()
                        );
                    }
                }
                liblib::Pick::None => {}
            }
        });
    }

    /// The device photo picker, for turning a real surface into ornament.
    ///
    /// A photograph is *luminance*, not elevation — a hammered surface under raking light puts a
    /// highlight and a shadow on the same facet, and the alpha will then stand the highlight proud
    /// and sink the shadow. Flat, even light is the difference between a texture and a rubbing of
    /// the lighting, so the UI says so rather than letting it be discovered in metal.
    fn photo_picker(&mut self, ui: &mut egui::Ui, host: &Host) {
        if !host.has_media_permission(false) {
            ui.label("RingDesigner needs access to your photos to use one as a texture.");
            if ui.button("Grant access").clicked() {
                host.request_media_images_permission();
            }
            if ui.button("Open app settings").clicked() {
                host.open_app_settings();
            }
            return;
        }

        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh").clicked() || self.photos.is_empty() {
                self.photos = host.list_device_media(false, 60);
                self.photo_thumbs.clear();
            }
            if ui.button("Close").clicked() {
                self.picker_open = false;
            }
        });
        ui.label(
            egui::RichText::new(
                "Shoot flat, even light. A photo records the lighting as much as the surface, and \
                 raking light becomes bumps that are not there.",
            )
            .small()
            .weak(),
        );

        if self.photos.is_empty() {
            ui.label(egui::RichText::new("no photos found").weak());
            return;
        }

        let cell = 84.0f32;
        let spacing = ui.spacing().item_spacing.x;
        let cols = (((ui.available_width() + spacing) / (cell + spacing)).floor() as usize).max(1);
        let mut chosen: Option<(i64, String)> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("photo_grid").num_columns(cols).show(ui, |ui| {
                for (i, (id, name)) in self.photos.clone().into_iter().enumerate() {
                    let tex = self.photo_thumbs.get(&id).cloned().or_else(|| {
                        let (w, h, rgba) = host.load_device_thumbnail(false, id, 192)?;
                        if w == 0 || h == 0 {
                            return None;
                        }
                        let img = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &rgba,
                        );
                        let t = ui.ctx().load_texture(
                            format!("photo:{id}"),
                            img,
                            egui::TextureOptions::LINEAR,
                        );
                        self.photo_thumbs.insert(id, t.clone());
                        Some(t)
                    });
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(cell, cell), egui::Sense::click());
                    let p = ui.painter_at(rect);
                    p.rect_filled(rect.shrink(2.0), 3.0, egui::Color32::from_rgb(26, 27, 31));
                    if let Some(t) = tex {
                        p.image(
                            t.id(),
                            rect.shrink(2.0),
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                    if resp.clicked() {
                        chosen = Some((id, name.clone()));
                    }
                    if (i + 1) % cols == 0 {
                        ui.end_row();
                    }
                }
            });
        });

        if let Some((id, name)) = chosen {
            self.import_photo(host, id, &name);
        }
    }

    /// Decode a device photo into an alpha, keep a copy on disk, and put it on the band.
    fn import_photo(&mut self, host: &Host, id: i64, display: &str) {
        let Some(bytes) = host.load_device_media(false, id) else {
            self.status = "could not read that photo".into();
            host.haptic(Haptic::Error);
            return;
        };
        let stem = display.rsplit_once('.').map(|(a, _)| a).unwrap_or(display).to_string();
        match ringdesign_core::alpha::Alpha::from_bytes(stem.clone(), &bytes) {
            Ok(alpha) => {
                // Persist the source so the library still has it next launch; the design references
                // alphas by name and an unsaved import would come back as a blank layer.
                if let Some(root) = self.data_root.as_ref() {
                    let dir = root.join("alphas");
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join(format!("{stem}.png")), &bytes);
                }
                self.status = format!("{stem}: {}x{}", alpha.width, alpha.height);
                Arc::make_mut(&mut self.lib).insert(alpha);
                self.thumbs.forget(&stem);
                self.picker_open = false;
                self.apply_alpha(&stem);
                host.haptic(Haptic::Success);
            }
            Err(e) => {
                self.status = format!("import failed: {e}");
                host.haptic(Haptic::Error);
            }
        }
    }

    fn files_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        let root = self.data_root.clone();
        ui.horizontal_wrapped(|ui| {
            ui.label("name");
            if ui
                .add(egui::TextEdit::singleline(&mut self.design.name).desired_width(150.0))
                .changed()
            {
                self.mark_dirty();
            }
        });
        ui.separator();

        let Some(root) = root else {
            ui.label("no writable directory");
            return;
        };
        let designs = root.join("designs");
        let exports = root.join("exports");

        // One row of menus instead of a dozen wrapping buttons: every popup
        // opens upward so it never lands under the system gesture area.
        ui.horizontal_wrapped(|ui| {
            crate::theme::up_menu(ui, "\u{1F4C4} File", |ui| {
                if ui.button("Save").clicked() {
                    let path = crate::util::design_path(&designs, &self.design.name);
                    let _ = std::fs::create_dir_all(&designs);
                    // Two designs both called "untitled" slug to one path, so a
                    // save used to overwrite the other silently. Say so once and
                    // let the second tap through.
                    if path.exists() && self.overwrite_warned.as_ref() != Some(&path) {
                        self.overwrite_warned = Some(path.clone());
                        self.status =
                            format!("{} already exists — Save again to replace it", self.design.name);
                        host.haptic(Haptic::Warning);
                    } else {
                        self.overwrite_warned = None;
                        self.status = match library::save_design(&path, &self.design) {
                            Ok(()) => {
                                self.prefs.push_recent(&path.to_string_lossy());
                                self.save_prefs();
                                format!("saved {}", path.display())
                            }
                            Err(e) => format!("save failed: {e}"),
                        };
                        host.haptic(Haptic::Success);
                    }
                }
                if ui
                    .button("Save a copy to Downloads")
                    .on_hover_text("A copy in shared storage that survives uninstalling the app")
                    .clicked()
                {
                    let name = format!("{}.ring.json", slug(&self.design.name));
                    let path = designs.join(&name);
                    let _ = std::fs::create_dir_all(&designs);
                    self.status = match library::save_design(&path, &self.design) {
                        Ok(()) => match host.save_to_gallery(path.to_string_lossy().into_owned(), name, "application/json") {
                            Some(folder) => format!("copy saved to {folder}"),
                            None => "could not write to Downloads".into(),
                        },
                        Err(e) => format!("save failed: {e}"),
                    };
                    host.haptic(Haptic::Success);
                }
                if ui.button("Copy design as JSON").clicked() {
                    if let Ok(json) = serde_json::to_string_pretty(&self.design) {
                        host.copy_text(json);
                        self.status = "design copied as JSON".into();
                        host.haptic(Haptic::Success);
                    }
                }
                if ui.button("Paste design").clicked() {
                    match host
                        .clipboard_text()
                        .and_then(|t| serde_json::from_str::<RingDesign>(&t).ok())
                    {
                        Some(d) => {
                            self.adopt(d);
                            self.status = "design pasted".into();
                            host.haptic(Haptic::Success);
                        }
                        None => {
                            self.status = "clipboard is not a design".into();
                            host.haptic(Haptic::Error);
                        }
                    }
                }
            });
            crate::theme::up_menu(ui, "\u{1F48D} New", |ui| {
                if ui.button("Blank band").clicked() {
                    self.load_template_design(RingDesign::default(), "new blank design");
                }
                ui.separator();
                for t in ringdesign_core::templates::all() {
                    if ui.button(t.name).on_hover_text(t.blurb).clicked() {
                        self.load_template_design(t.design(), t.name);
                    }
                }
            });
            crate::theme::up_menu(ui, "\u{1F4E4} Export", |ui| {
                if ui.button("STL — the pattern to cut").clicked() {
                    self.export(ExportKind::Stl, &exports, ui.ctx());
                }
                if ui.button("3MF — units stated").clicked() {
                    self.export(ExportKind::ThreeMf, &exports, ui.ctx());
                }
                if ui
                    .button("GLB — AR and web viewers")
                    .on_hover_text("glTF binary, metre-scaled")
                    .clicked()
                {
                    self.export(ExportKind::Glb, &exports, ui.ctx());
                }
            });
            crate::theme::up_menu(ui, "\u{2728} Share", |ui| {
                if ui.button("Casting sheet").clicked() {
                    self.export(ExportKind::Sheet, &exports, ui.ctx());
                }
                if ui
                    .button("Stone map")
                    .on_hover_text("Every stone to scale with the tight gaps drawn: the setter's map")
                    .clicked()
                {
                    self.export(ExportKind::StoneMap, &exports, ui.ctx());
                }
                if ui.button("Render photo").clicked() {
                    self.export(ExportKind::Render, &exports, ui.ctx());
                }
                if ui
                    .button("Turntable spin")
                    .on_hover_text("A looping 36-frame GIF")
                    .clicked()
                {
                    self.export(ExportKind::Turntable, &exports, ui.ctx());
                }
            });
            {
                use ringdesign_core::metal::METALS;
                let current = self
                    .shrink_metal
                    .and_then(|i| METALS.get(i))
                    .map(|m| format!("{} +{:.1}%", m.name, m.shrink_pct))
                    .unwrap_or_else(|| "nominal".into());
                egui::ComboBox::from_id_salt("shrink_metal")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.shrink_metal, None, "nominal");
                        for (i, m) in METALS.iter().enumerate() {
                            ui.selectable_value(
                                &mut self.shrink_metal,
                                Some(i),
                                format!("{} +{:.1}%", m.name, m.shrink_pct),
                            );
                        }
                    });
            }
        });

        if !self.exports.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new(format!("exporting {}…", self.exports.len())).small().weak());
            });
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }

        ui.separator();
        ui.label(egui::RichText::new("desktop").weak());
        ui.horizontal_wrapped(|ui| {
            ui.label("host");
            ui.add(
                egui::TextEdit::singleline(&mut self.sync_host)
                    .hint_text("100.x.y.z or name.ts.net")
                    .desired_width(170.0),
            );
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("token");
            ui.add(
                egui::TextEdit::singleline(&mut self.sync_token)
                    .password(true)
                    .desired_width(170.0),
            );
        });
        ui.horizontal_wrapped(|ui| {
            let busy = self.sync_job.is_some();
            let ready = !self.sync_host.trim().is_empty();
            ui.add_enabled_ui(!busy && ready, |ui| {
                if ui.button("Pull").clicked() {
                    self.sync(false);
                }
                if ui.button("Push").clicked() {
                    self.sync(true);
                }
            });
            if busy {
                ui.spinner();
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            }
        });
        ui.label(
            egui::RichText::new(
                "Start the sync server on the desktop first. Over Tailscale this works from \
                 anywhere, not just your own network.",
            )
            .small()
            .weak(),
        );

        ui.separator();
        ui.label(egui::RichText::new("saved designs").weak());
        egui::ScrollArea::vertical().show(ui, |ui| {
            let files = crate::util::list_designs(&designs);
            if files.is_empty() {
                ui.label(egui::RichText::new("none yet").weak());
            }
            // Recents first, as their own group: on a phone this list is the
            // only way back to a design, and there is no search.
            let recent: Vec<&crate::util::DesignFile> = self
                .prefs
                .recent
                .iter()
                .filter_map(|r| files.iter().find(|f| f.path.to_string_lossy() == *r))
                .collect();
            let mut order: Vec<&crate::util::DesignFile> = recent.clone();
            order.extend(files.iter().filter(|f| !recent.iter().any(|r| r.path == f.path)));

            let recent_n = recent.len();
            for (i, f) in order.into_iter().enumerate() {
                if i == 0 && recent_n > 0 {
                    ui.label(egui::RichText::new("recent").small().weak());
                }
                if i == recent_n && recent_n > 0 {
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("all").small().weak());
                }
                self.design_row(ui, host, f);
            }
        });

        ui.separator();
        ui.label(egui::RichText::new("on-device models").weak());
        {
            let lib_dir = host.native_lib_dir().map(std::path::PathBuf::from);
            ui.label(
                egui::RichText::new(crate::npu::status(&self.packs, lib_dir.as_deref()))
                    .small()
                    .weak(),
            );
            for p in &self.packs {
                ui.label(
                    egui::RichText::new(format!("{} · {} — {}", p.kind.label(), p.name, p.kind.buys()))
                        .small(),
                );
            }
            if ui.small_button("Rescan").clicked() {
                self.rescan_packs();
                self.status = format!("{} model packs", self.packs.len());
            }
        }

        ui.separator();
        ui.label(
            egui::RichText::new(
                "App storage is wiped if you uninstall. Share anything you want to keep.",
            )
            .small()
            .weak(),
        );
    }

    /// One row of the saved-designs list: open, share, rename, delete.
    ///
    /// Delete and rename are two-step. App storage is unreachable from any file
    /// manager, so a file the app deletes is gone with no other way back, and a
    /// mistap on a 44 dp row is cheap.
    fn design_row(&mut self, ui: &mut egui::Ui, host: &Host, f: &crate::util::DesignFile) {
        let key = f.path.to_string_lossy().into_owned();

        if self.renaming.as_ref().is_some_and(|(p, _)| *p == f.path) {
            ui.horizontal_wrapped(|ui| {
                let Some((_, draft)) = self.renaming.as_mut() else { return };
                ui.add(egui::TextEdit::singleline(draft).desired_width(140.0));
                let draft = draft.clone();
                let target = crate::util::design_path(
                    f.path.parent().unwrap_or(std::path::Path::new(".")),
                    &draft,
                );
                let clash = target != f.path && target.exists();
                if ui.add_enabled(!draft.trim().is_empty() && !clash, egui::Button::new("Save")).clicked() {
                    self.status = match std::fs::rename(&f.path, &target) {
                        Ok(()) => {
                            self.prefs.forget_recent(&key);
                            self.prefs.push_recent(&target.to_string_lossy());
                            self.save_prefs();
                            format!("renamed to {draft}")
                        }
                        Err(e) => format!("rename failed: {e}"),
                    };
                    self.renaming = None;
                }
                if ui.button("Cancel").clicked() {
                    self.renaming = None;
                }
                if clash {
                    ui.label(
                        egui::RichText::new("that name is taken")
                            .small()
                            .color(egui::Color32::from_rgb(220, 170, 90)),
                    );
                }
            });
            return;
        }

        if self.confirm_delete.as_ref() == Some(&f.path) {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!("delete {}?", f.stem))
                        .color(egui::Color32::from_rgb(240, 105, 120)),
                );
                if ui.button("Delete").clicked() {
                    self.status = match std::fs::remove_file(&f.path) {
                        Ok(()) => {
                            self.prefs.forget_recent(&key);
                            self.save_prefs();
                            host.haptic(Haptic::Warning);
                            format!("deleted {}", f.stem)
                        }
                        Err(e) => format!("delete failed: {e}"),
                    };
                    self.confirm_delete = None;
                }
                if ui.button("Keep").clicked() {
                    self.confirm_delete = None;
                }
            });
            return;
        }

        ui.horizontal_wrapped(|ui| {
            if ui.button("Open").clicked() {
                match library::load_design(&f.path) {
                    Ok(d) => {
                        self.adopt(d);
                        self.prefs.push_recent(&key);
                        self.save_prefs();
                        self.status = format!("opened {}", f.stem);
                    }
                    Err(e) => self.status = format!("open failed: {e}"),
                }
            }
            if ui.button("Share").clicked() {
                host.share_media(key.clone(), f.file_name.clone(), "application/json");
            }
            if ui.small_button("Rename").clicked() {
                self.renaming = Some((f.path.clone(), f.stem.clone()));
            }
            if ui.small_button("Delete").clicked() {
                self.confirm_delete = Some(f.path.clone());
            }
            ui.label(&f.stem);
        });
    }

    /// Pull the desktop's live design, or push this one to it.
    ///
    /// On a worker: a network round trip on the UI thread is an ANR waiting for a slow tailnet.
    fn sync(&mut self, push: bool) {
        let base = sync_base(&self.sync_host);
        let token = self.sync_token.trim().to_string();
        let body = push.then(|| serde_json::to_vec(&self.design).unwrap_or_default());
        let (tx, rx) = std::sync::mpsc::channel();
        self.status = if push { "pushing…".into() } else { "pulling…".into() };
        std::thread::Builder::new()
            .name("ring-sync".into())
            .spawn(move || {
                let out = match body {
                    Some(bytes) => match minreq::post(format!("{base}/design"))
                        .with_header("x-ring-token", &token)
                        .with_header("content-type", "application/json")
                        .with_timeout(15)
                        .with_body(bytes)
                        .send()
                    {
                        Ok(r) if r.status_code == 200 => {
                            SyncResult::Pushed("pushed to desktop".into())
                        }
                        Ok(r) => SyncResult::Failed(format!(
                            "push failed ({}): {}",
                            r.status_code,
                            r.as_str().unwrap_or("").chars().take(90).collect::<String>()
                        )),
                        Err(e) => SyncResult::Failed(format!("push failed: {e}")),
                    },
                    None => match minreq::get(format!("{base}/design"))
                        .with_header("x-ring-token", &token)
                        .with_timeout(15)
                        .send()
                    {
                        Ok(r) if r.status_code == 200 => match r
                            .as_str()
                            .ok()
                            .and_then(|t| serde_json::from_str::<RingDesign>(t).ok())
                        {
                            Some(d) => SyncResult::Pulled(Box::new(d)),
                            None => SyncResult::Failed("desktop sent something else".into()),
                        },
                        Ok(r) => SyncResult::Failed(format!("pull failed ({})", r.status_code)),
                        Err(e) => SyncResult::Failed(format!("pull failed: {e}")),
                    },
                };
                let _ = tx.send(out);
            })
            .expect("spawn sync thread");
        self.sync_job = Some(rx);
    }

    fn poll_sync(&mut self, host: &Host) {
        let Some(rx) = self.sync_job.as_ref() else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.sync_job = None;
        match result {
            SyncResult::Pulled(d) => {
                self.adopt(*d);
                self.status = "pulled from desktop".into();
                host.haptic(Haptic::Success);
            }
            SyncResult::Pushed(msg) => {
                self.status = msg;
                host.haptic(Haptic::Success);
            }
            SyncResult::Failed(msg) => {
                self.status = msg;
                host.haptic(Haptic::Error);
            }
        }
    }

    fn load_template_design(&mut self, d: RingDesign, what: &str) {
        self.adopt(d);
        self.status = format!("started from {what}");
    }

    /// Builds and writes an export on its own thread; the share sheet opens
    /// from `poll_exports` when the file lands.
    fn export(&mut self, kind: ExportKind, dir: &std::path::Path, ctx: &egui::Context) {
        use ringdesign_core::metal;
        // The preview mesh for the spin: 36 software-rastered frames of the
        // export mesh is seconds of spinner for no visible gain at 480 px.
        let params = if kind == ExportKind::Turntable { ring::PREVIEW } else { ring::EXPORT };
        let shrink = match kind {
            ExportKind::Stl | ExportKind::ThreeMf => self
                .shrink_metal
                .and_then(|i| metal::METALS.get(i))
                .map(|m| (m.shrink_pct, m.name.to_string())),
            _ => None,
        };
        let job = export::ExportJob {
            kind,
            path: dir.join(format!("{}{}", slug(&self.design.name), kind.ext())),
            design: self.design.clone(),
            lib: self.lib.clone(),
            params,
            shrink,
            generator: concat!("RingDesigner Android ", env!("CARGO_PKG_VERSION")).into(),
        };
        self.status = format!("{} export started", kind.label());
        self.exports.push(export::spawn(job, ctx.clone()));
    }

    fn poll_exports(&mut self, host: &Host) {
        let mut landed = Vec::new();
        self.exports.retain(|rx| match rx.try_recv() {
            Ok(done) => {
                landed.push(done);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
        });
        for done in landed {
            if done.ok {
                host.share_media(done.path.to_string_lossy().into_owned(), done.name, done.kind.mime());
                self.status = format!("{} · shared", done.status);
                host.haptic(Haptic::Success);
            } else {
                self.status = done.status;
                host.haptic(Haptic::Error);
            }
        }
    }

    fn bench_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        if let Some(rx) = self.bench.running.as_ref() {
            if let Ok(report) = rx.try_recv() {
                self.bench.report = Some(report);
                self.bench.running = None;
            }
        }
        let busy = self.bench.running.is_some();

        ui.add_enabled_ui(!busy, |ui| {
            if ui.button("Run bench").clicked() {
                host.haptic(Haptic::Medium);
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::Builder::new()
                    .name("ring-bench".into())
                    .spawn(move || {
                        let report = bench::run();
                        log::info!("ringdesigner bench\n{}", report.to_text());
                        let _ = tx.send(report);
                    })
                    .expect("spawn bench thread");
                self.bench.running = Some(rx);
                self.bench.report = None;
                self.bench.started = Some(Instant::now());
            }
        });

        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                let secs = self.bench.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
                ui.label(format!("building… {secs:.0}s"));
            });
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        if let Some(report) = self.bench.report.as_ref() {
            egui::ScrollArea::both().show(ui, |ui| {
                ui.monospace(report.to_text());
            });
            if ui.button("Copy").clicked() {
                host.copy_text(report.to_text());
                host.haptic(Haptic::Success);
            }
        }
    }
}

impl EguiApp for RingApp {
    fn theme(&self, ctx: &egui::Context) {
        crate::theme::apply(ctx);
    }

    fn on_start(&mut self, ctx: &egui::Context, host: &Host) {
        // `library.rs` reads XDG_DATA_HOME / HOME, both unset on Android, and falls back to ".".
        if let Some(dir) = host.documents_dir() {
            let root = std::path::PathBuf::from(dir).join("ringdesigner");
            for sub in ["designs", "alphas", "exports"] {
                let _ = std::fs::create_dir_all(root.join(sub));
            }
            // Imported textures were written here; without this they come back as blank layers,
            // because a design references alphas by name and never embeds them.
            library::set_data_root(root.clone());
            match Arc::make_mut(&mut self.lib).load_dir(root.join("alphas")) {
                Ok(n) if n > 0 => log::info!("loaded {n} user alphas"),
                Ok(_) => {}
                Err(e) => log::warn!("user alphas: {e}"),
            }
            if let Ok(loaded) = library::load_design(root.join(AUTOSAVE)) {
                self.design = loaded;
                log::info!("restored {}", AUTOSAVE);
            }
            self.prefs = crate::prefs::load(&root);
            self.prefs.sanitize(ShadeMode::ALL.len(), ringdesign_core::metal::METALS.len());
            self.apply_prefs();
            self.data_root = Some(root);
            self.rescan_packs();
        }

        self.native_lib_dir = host.native_lib_dir();
        self.px_per_mm = device_px_per_mm(host);
        self.has_stylus = host.has_stylus();
        // Resting a hand on the glass should not draw, so pen-only is the default where there is
        // a pen — but only on a first run: once the user has said otherwise, that is the answer.
        if self.data_root.is_none() || !self.prefs_seen {
            self.brush.stylus_only = self.has_stylus;
        }
        // Strokes are the source of truth; the raster is derived, so bake before the first build.
        {
            let lib = Arc::make_mut(&mut self.lib);
            self.design.bake_all(lib);
        }
        self.worker = Some(Worker::spawn(ctx.clone()));
        self.mark_dirty();
        // One immediate draft build so there is something on screen before the debounce elapses.
        self.dispatch(false);
    }

    /// Flush before the OS may reap us.
    ///
    /// The 90 ms debounce means an edit made in the last moment before the app
    /// backgrounds has not been written yet, and Android kills a backgrounded
    /// process without warning. Commit the pending history step too, so undo
    /// still reaches that edit on the next launch.
    fn on_pause(&mut self, _host: &Host) {
        self.dirty_at = None;
        self.history.commit(&self.design);
        self.autosave();
        self.save_prefs();
        log::info!("on_pause: design and prefs flushed");
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.probe = host.stylus_probe();
        self.poll_sync(host);
        self.tick(ui.ctx());
        if std::mem::take(&mut self.verdict_fell) {
            host.haptic(Haptic::Warning);
        }
        if std::mem::take(&mut self.detent) {
            host.haptic(Haptic::Selection);
        }
        self.graph.sync(&self.design);
        self.poll_exports(host);
        self.poll_generate(host);

        // Order is load-bearing: ambience lights the page, then the frost
        // grabs what is already in the framebuffer, then chrome paints on top.
        crate::theme::ambience(ui.ctx());
        crate::frost::frost_chrome(ui);

        // Chrome collapses while typing (focus leads the keyboard slide-in
        // and the inset trails slide-out; the union avoids flicker).
        let kb_editing = host.keyboard_height() > 1.0 || ui.ctx().text_edit_focused();
        let mut chrome: Option<egui::Rect> = None;
        let mut grow = |r: egui::Rect| {
            chrome = Some(match chrome {
                Some(c) => c.union(r),
                None => r,
            })
        };

        let mut nav_open = !kb_editing;
        let bar = egui::Panel::bottom(egui::Id::new("tabs"))
            .frame(crate::theme::bar())
            .drag_to_open(false)
            .show_collapsible(ui, &mut nav_open, |ui| {
                self.nav_bar(ui, host);
            });
        if let Some(bar) = &bar {
            grow(bar.response.rect);
        }

        let line = match (self.tab, self.readout.as_ref()) {
            (Tab::Band | Tab::Tile, Some(r)) => r.clone(),
            _ => self.status.clone(),
        };
        let status = egui::Panel::bottom(egui::Id::new("status"))
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 3)))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(line).small().color(crate::theme::INK_DIM),
                );
            });
        grow(status.response.rect);

        // The design sheet slides up over the live ring, so every slider is
        // seen on the mesh without leaving the view.
        let mut sheet_open = self.design_open && !kb_editing;
        let sheet = egui::Panel::bottom(egui::Id::new("design-sheet"))
            .frame(crate::theme::bar())
            .drag_to_open(false)
            .show_collapsible(ui, &mut sheet_open, |ui| {
                let cap = ui.ctx().content_rect().height() * 0.44;
                crate::theme::scroll_vertical().max_height(cap).show(ui, |ui| {
                    self.design_tab(ui);
                });
            });
        if let Some(sheet) = &sheet {
            grow(sheet.response.rect);
        }

        // The findings sit above the design sheet, and close themselves when the
        // build that raised them comes back clean.
        if self.dfm.is_empty() {
            self.dfm_open = false;
        }
        let mut dfm_open = self.dfm_open && !kb_editing;
        let dfm = egui::Panel::bottom(egui::Id::new("dfm-sheet"))
            .frame(crate::theme::bar())
            .drag_to_open(false)
            .show_collapsible(ui, &mut dfm_open, |ui| {
                let cap = ui.ctx().content_rect().height() * 0.44;
                crate::theme::scroll_vertical().max_height(cap).show(ui, |ui| {
                    self.dfm_sheet(ui);
                });
            });
        if let Some(dfm) = &dfm {
            grow(dfm.response.rect);
        }

        let mut layers_open = self.layers_open && !kb_editing;
        let layers = egui::Panel::bottom(egui::Id::new("layers-sheet"))
            .frame(crate::theme::bar())
            .drag_to_open(false)
            .show_collapsible(ui, &mut layers_open, |ui| {
                let cap = ui.ctx().content_rect().height() * 0.5;
                crate::theme::scroll_vertical().max_height(cap).show(ui, |ui| {
                    if let Some(note) = crate::layers::add_menu(
                        ui,
                        &mut self.design,
                        &mut self.selected_layer,
                    ) {
                        self.status = note;
                        self.mark_dirty();
                    }
                    let ctx = self.design.field_context();
                    let dirty = crate::layers::sheet(
                        ui,
                        &mut self.design.layers,
                        &ctx,
                        &self.dfm,
                        &mut self.selected_layer,
                    );
                    if dirty {
                        self.mark_dirty();
                    }
                });
            });
        if let Some(layers) = &layers {
            grow(layers.response.rect);
        }

        let mut report_open = self.report_open && !kb_editing;
        let report = egui::Panel::bottom(egui::Id::new("report-sheet"))
            .frame(crate::theme::bar())
            .drag_to_open(false)
            .show_collapsible(ui, &mut report_open, |ui| {
                let cap = ui.ctx().content_rect().height() * 0.5;
                crate::theme::scroll_vertical().max_height(cap).show(ui, |ui| {
                    let size = self.design.size.display();
                    let mut close = false;
                    crate::report::sheet(
                        ui,
                        self.report.as_ref(),
                        self.stones.as_ref(),
                        &size,
                        &mut close,
                    );
                    if close {
                        self.report_open = false;
                    }
                });
            });
        if let Some(report) = &report {
            grow(report.response.rect);
        }

        let mut timeline_open = self.timeline_open && !kb_editing;
        let timeline = egui::Panel::bottom(egui::Id::new("timeline-sheet"))
            .frame(crate::theme::bar())
            .drag_to_open(false)
            .show_collapsible(ui, &mut timeline_open, |ui| {
                let cap = ui.ctx().content_rect().height() * 0.44;
                crate::theme::scroll_vertical().max_height(cap).show(ui, |ui| {
                    self.timeline_sheet(ui);
                });
            });
        if let Some(timeline) = &timeline {
            grow(timeline.response.rect);
        }

        if let Some(chrome) = chrome {
            crate::frost::remember(ui.ctx(), chrome);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(6))
            .show(ui, |ui| {
                // The sketch pad takes the whole pane while it is open: a
                // boundary is drawn against the guides, not over a live mesh.
                if self.sketch_mode.is_some() {
                    self.sketch_pad(ui, host);
                    return;
                }
                match self.tab {
                Tab::Ring => self.ring_tab(ui),
                Tab::Band => self.paint_tab(ui, host, Domain::Band),
                Tab::Tile => self.paint_tab(ui, host, Domain::Tile),
                Tab::Graph => self.graph_tab(ui, host),
                Tab::Alphas => self.alphas_tab(ui, host),
                Tab::Files => self.files_tab(ui, host),
                Tab::Bench => self.bench_tab(ui, host),
            }});
    }
}

/// First tiling inside a layer, in the order `dfm::findings_in` walks them.
///
/// A finding's `layer` is the *top-level* entry index, and the tiling that
/// measured short may be nested in a group — the core's own loop collects the
/// same way and stops at the first one that fails, so these two walks have to
/// agree or Fit would solve a different tiling than the one reported.
fn first_tiling(layer: &Layer) -> Option<&TilingLayer> {
    match layer {
        Layer::Tiling(t) => Some(t),
        Layer::Openwork(o) => Some(&o.tiling),
        Layer::Group(g) => g.stack.layers.iter().filter(|e| e.enabled).find_map(|e| first_tiling(&e.layer)),
        _ => None,
    }
}

fn first_tiling_mut(layer: &mut Layer) -> Option<&mut TilingLayer> {
    match layer {
        Layer::Tiling(t) => Some(t),
        Layer::Openwork(o) => Some(&mut o.tiling),
        Layer::Group(g) => {
            g.stack.layers.iter_mut().filter(|e| e.enabled).find_map(|e| first_tiling_mut(&mut e.layer))
        }
        _ => None,
    }
}

/// Outcome of a sync call, handed back from the worker thread.
enum SyncResult {
    Pulled(Box<RingDesign>),
    Pushed(String),
    Failed(String),
}

/// Nearest triangle of the mesh under the ray — every face tested on a tap,
/// which is a millisecond at preview resolution and needs no BVH.
fn raycast(
    mesh: &ringdesign_core::mesh::Mesh,
    origin: [f32; 3],
    dir: [f32; 3],
) -> Option<(usize, [f32; 3])> {
    let o = [origin[0] as f64, origin[1] as f64, origin[2] as f64];
    let d = [dir[0] as f64, dir[1] as f64, dir[2] as f64];
    let mut best: Option<(usize, f64)> = None;
    for (fi, f) in mesh.faces.iter().enumerate() {
        let Some((a, b, c)) = mesh.triangle(f) else { continue };
        let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let p = [
            d[1] * e2[2] - d[2] * e2[1],
            d[2] * e2[0] - d[0] * e2[2],
            d[0] * e2[1] - d[1] * e2[0],
        ];
        let det = e1[0] * p[0] + e1[1] * p[1] + e1[2] * p[2];
        if det.abs() < 1e-12 {
            continue;
        }
        let inv = 1.0 / det;
        let t_vec = [o[0] - a[0], o[1] - a[1], o[2] - a[2]];
        let u = (t_vec[0] * p[0] + t_vec[1] * p[1] + t_vec[2] * p[2]) * inv;
        if !(0.0..=1.0).contains(&u) {
            continue;
        }
        let q = [
            t_vec[1] * e1[2] - t_vec[2] * e1[1],
            t_vec[2] * e1[0] - t_vec[0] * e1[2],
            t_vec[0] * e1[1] - t_vec[1] * e1[0],
        ];
        let v = (d[0] * q[0] + d[1] * q[1] + d[2] * q[2]) * inv;
        if v < 0.0 || u + v > 1.0 {
            continue;
        }
        let t = (e2[0] * q[0] + e2[1] * q[1] + e2[2] * q[2]) * inv;
        if t > 1e-6 && best.map_or(true, |(_, bt)| t < bt) {
            best = Some((fi, t));
        }
    }
    best.map(|(fi, t)| {
        (
            fi,
            [
                (o[0] + d[0] * t) as f32,
                (o[1] + d[1] * t) as f32,
                (o[2] + d[2] * t) as f32,
            ],
        )
    })
}

/// Verdict colour and text for the toolbar chip. The undercut fraction is the number that decides
/// it, so it is shown rather than the label alone.
/// The verdict chip, and the process that produced it.
///
/// Under lost wax the undercut percentage is measured and reported but never
/// gates, so a chip reading "Castable · 3.10%" is true and unreadable without
/// the process beside it. `FieldReport` does not carry the process, so it is
/// passed in from the design that was judged.
fn field_chip(
    f: &ringdesign_core::castability::FieldReport,
    process: ringdesign_core::castability::CastProcess,
) -> (egui::Color32, String) {
    use ringdesign_core::castability::Verdict;
    let tint = match f.verdict {
        Verdict::Castable => egui::Color32::from_rgb(82, 199, 115),
        Verdict::Marginal => egui::Color32::from_rgb(242, 194, 61),
        Verdict::NotCastable => egui::Color32::from_rgb(240, 105, 120),
    };
    let how = crate::casting::short_label(process);
    (
        tint,
        format!(
            "{} · {how} · {:.2}% · wall {:.2} mm",
            f.verdict.label(),
            f.undercut_fraction() * 100.0,
            f.thinnest_wall_mm
        ),
    )
}

fn verdict_chip(cast: &CastReport) -> (egui::Color32, String) {
    use ringdesign_core::castability::Verdict;
    let tint = match cast.verdict {
        Verdict::Castable => egui::Color32::from_rgb(82, 199, 115),
        Verdict::Marginal => egui::Color32::from_rgb(242, 194, 61),
        Verdict::NotCastable => egui::Color32::from_rgb(237, 69, 92),
    };
    let pct = if cast.total_area_mm2 > 0.0 {
        cast.undercut_area_mm2 / cast.total_area_mm2 * 100.0
    } else {
        0.0
    };
    (tint, format!("{} · {pct:.2}% undercut", cast.verdict.label()))
}

/// Physical pixels per millimetre, for true-scale rendering.
///
/// From `DisplayMetrics.xdpi`, the panel's real DPI — deliberately not from `pixels_per_point`,
/// which is Android's rounded density bucket and is typically ~10% out. A 1:1 mode that is 10%
/// wrong is worse than none, because the reason to trust a phone over a monitor is that the
/// monitor's DPI is a lie. `None` when the panel will not say, and then the toggle is not offered.
fn device_px_per_mm(host: &Host) -> Option<f32> {
    let (x, y) = host.display_dpi()?;
    // Square pixels on every phone panel worth the name; averaging costs nothing and guards a
    // driver that fills only one field sensibly.
    let dpi = (x + y) * 0.5;
    (dpi.is_finite() && dpi > 40.0).then(|| dpi / 25.4)
}

