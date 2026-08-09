//! The Android shell: a bottom tab bar over the panes, plus the debounced rebuild.
//!
//! There is no dock and no pane grid — those are mouse-and-monitor affordances. Tabs, because at
//! ~411 x 890 points there is room for exactly one thing at a time.
//!
//! Autosave is not optional here. `EguiApp` has no `save` hook, `on_pause` never fires on Android,
//! and nothing replaces the desktop's eframe-storage path, so without a write on the dirty debounce
//! the design is gone the moment the OS reaps the process.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui_mobile::egui;
use egui_mobile::{CreateContext, EguiApp, Haptic, Host, HostExt};

use ringdesign_core::alpha::AlphaLibrary;
use ringdesign_core::castability::CastReport;
use ringdesign_core::{RingDesign, library};

use ringdesign_core::drawn::DrawnAlpha;
use ringdesign_core::field::{Layer, LayerEntry};
use ringdesign_core::tiling::TilingLayer;

use crate::bench;
use crate::canvas::{self, CanvasInput, Domain, View};
use crate::library as liblib;
use crate::paint;
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
    Alphas,
    Files,
    Bench,
}

impl Tab {
    const ALL: &'static [Tab] =
        &[Tab::Ring, Tab::Band, Tab::Tile, Tab::Alphas, Tab::Files, Tab::Bench];

    fn label(self) -> &'static str {
        match self {
            Tab::Ring => "Ring",
            Tab::Band => "Band",
            Tab::Tile => "Tile",
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
    status: String,
    dirty_at: Option<Instant>,
    generation: u64,

    data_root: Option<std::path::PathBuf>,
    px_per_mm: Option<f32>,

    thumbs: liblib::Thumbs,
    alpha_filter: String,
    /// Alpha currently on the pattern layer.
    picked_alpha: Option<String>,
    pattern_repeats: u32,
    pattern_height_mm: f64,
    builtin_size: usize,

    brush: Brush,
    band_view: View,
    tile_view: View,
    tile_repeats: u32,
    has_stylus: bool,
    readout: Option<String>,
    /// `(tool, hover px, buttons)` sampled once a frame. winit drops tool type and hover, so this
    /// comes from the patched `android-activity` side channel rather than from egui's events.
    probe: (u8, Option<(f32, f32)>, u32),

    bench: BenchState,
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
            status: "starting".into(),
            dirty_at: None,
            generation: 0,
            data_root: None,
            px_per_mm: None,
            thumbs: liblib::Thumbs::default(),
            alpha_filter: String::new(),
            picked_alpha: None,
            pattern_repeats: 24,
            pattern_height_mm: 0.35,
            builtin_size: 256,
            brush: Brush::default(),
            band_view: View::default(),
            tile_view: View::default(),
            tile_repeats: 24,
            has_stylus: false,
            readout: None,
            probe: (0, None, 0),
            bench: BenchState::default(),
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty_at = Some(Instant::now());
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
        if !worker.dispatch(self.generation, &self.design, &self.lib, ring::PREVIEW, analyze) {
            self.status = "build worker stopped".into();
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        if let Some(worker) = self.worker.as_ref() {
            while let Some(done) = worker.poll() {
                if done.generation != self.generation {
                    continue;
                }
                self.pane.camera.fit(done.bounds);
                if let Ok(mut r) = self.renderer.lock() {
                    r.set_pending(done.verts);
                }
                if let Some(cast) = done.cast {
                    self.status = format!(
                        "{} tris · {:.1} mm³ · {} ms · {}",
                        done.triangles,
                        done.volume_mm3,
                        done.build_ms,
                        cast.verdict.label()
                    );
                    self.cast = Some(cast);
                } else {
                    self.status =
                        format!("{} tris · {} ms", done.triangles, done.build_ms);
                }
                ctx.request_repaint();
            }
        }

        if let Some(at) = self.dirty_at {
            let waited = at.elapsed();
            if waited >= DEBOUNCE {
                self.dirty_at = None;
                self.dispatch(true);
                self.autosave();
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
                    if ui.selectable_label(self.pane.shade == *mode, mode.label()).clicked() {
                        self.pane.shade = *mode;
                    }
                }
                ui.separator();
                ui.toggle_value(&mut self.pane.wireframe, "Wire");
                if let Some(cast) = self.cast.as_ref() {
                    ui.separator();
                    let (tint, text) = verdict_chip(cast);
                    ui.colored_label(tint, text);
                }
                if self.px_per_mm.is_some() {
                    ui.toggle_value(&mut self.pane.actual_size, "1:1");
                }
            });
            ui.horizontal_wrapped(|ui| {
                for view in crate::camera::StandardView::ALL {
                    if ui.button(view.label()).clicked() {
                        self.pane.camera.set_view(*view);
                        self.pane.actual_size = false;
                    }
                }
                if ui.button("Reset").clicked() {
                    self.pane.camera.reset();
                    self.pane.actual_size = false;
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.pane.ui(ui, &self.renderer, self.px_per_mm) {
                ui.ctx().request_repaint();
            }
        });
    }

    /// Index of a drawing by name, creating it (and the layer that shows it) on first use.
    ///
    /// The drawing and the layer are two halves of one thing: strokes travel inside the design so a
    /// shared file is self-contained, and the layer is an ordinary `TilingLayer` so every existing
    /// blend, window and lattice control applies and the desktop opens it unchanged.
    fn ensure_drawing(&mut self, name: &str, w: u32, h: u32, wrap_y: bool, repeats: u32) -> usize {
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

    fn paint_tab(&mut self, ui: &mut egui::Ui, host: &Host, domain: Domain) {
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
                },
            );
            if let Some(slot) = self.design.drawn.get_mut(index) {
                *slot = drawing;
            }

            if out.readout.is_some() {
                self.readout = out.readout;
            }
            if out.stroke_ended {
                self.bake(index);
                self.mark_dirty();
                host.haptic(Haptic::Selection);
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

    fn alphas_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
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
                        self.design.bake_drawn(lib);
                        self.thumbs.clear();
                        self.mark_dirty();
                        host.haptic(Haptic::Light);
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
            let picked = liblib::grid(
                ui,
                &self.lib,
                &mut self.thumbs,
                self.picked_alpha.as_deref(),
                &self.alpha_filter,
            );
            match picked {
                liblib::Pick::Use(name) => {
                    self.apply_alpha(&name);
                    host.haptic(Haptic::Selection);
                }
                liblib::Pick::Preview(name) => {
                    self.status = format!(
                        "{name}: {}",
                        self.lib
                            .get(&name)
                            .map(|a| format!("{}x{}", a.width, a.height))
                            .unwrap_or_default()
                    );
                }
                liblib::Pick::None => {}
            }
        });
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

        ui.horizontal_wrapped(|ui| {
            if ui.button("Save").clicked() {
                let path = designs.join(format!("{}.ring.json", slug(&self.design.name)));
                let _ = std::fs::create_dir_all(&designs);
                self.status = match library::save_design(&path, &self.design) {
                    Ok(()) => format!("saved {}", path.display()),
                    Err(e) => format!("save failed: {e}"),
                };
                host.haptic(Haptic::Success);
            }
            if ui.button("Export STL").clicked() {
                let _ = std::fs::create_dir_all(&exports);
                let path = exports.join(format!("{}.stl", slug(&self.design.name)));
                self.export_stl(host, path);
            }
            if ui.button("Copy design").clicked() {
                if let Ok(json) = serde_json::to_string_pretty(&self.design) {
                    host.copy_text(json);
                    self.status = "design copied as JSON".into();
                    host.haptic(Haptic::Success);
                }
            }
            if ui.button("Paste design").clicked() {
                match host.clipboard_text().and_then(|t| serde_json::from_str::<RingDesign>(&t).ok())
                {
                    Some(d) => {
                        self.design = d;
                        let lib = Arc::make_mut(&mut self.lib);
                        self.design.bake_drawn(lib);
                        self.thumbs.clear();
                        self.picked_alpha = None;
                        self.status = "design pasted".into();
                        self.mark_dirty();
                        host.haptic(Haptic::Success);
                    }
                    None => {
                        self.status = "clipboard is not a design".into();
                        host.haptic(Haptic::Error);
                    }
                }
            }
        });

        ui.separator();
        ui.label(egui::RichText::new("saved designs").weak());
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&designs)
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .map(|e| e.path())
                        .filter(|p| p.to_string_lossy().ends_with(".ring.json"))
                        .collect()
                })
                .unwrap_or_default();
            entries.sort();
            if entries.is_empty() {
                ui.label(egui::RichText::new("none yet").weak());
            }
            for path in entries {
                let label = path.file_name().map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() {
                        match library::load_design(&path) {
                            Ok(d) => {
                                self.design = d;
                                let lib = Arc::make_mut(&mut self.lib);
                                self.design.bake_drawn(lib);
                                self.thumbs.clear();
                                self.picked_alpha = None;
                                self.status = format!("opened {label}");
                                self.mark_dirty();
                            }
                            Err(e) => self.status = format!("open failed: {e}"),
                        }
                    }
                    if ui.button("Share").clicked() {
                        host.share_media(
                            path.to_string_lossy().into_owned(),
                            label.clone(),
                            "application/json",
                        );
                    }
                    ui.label(label);
                });
            }
        });

        ui.separator();
        ui.label(
            egui::RichText::new(
                "App storage is wiped if you uninstall. Share anything you want to keep.",
            )
            .small()
            .weak(),
        );
    }

    /// Build at export resolution and hand the file to the system share sheet.
    ///
    /// Measured at 226 ms end to end on this device, so it runs inline rather than on the worker;
    /// the part that actually stalls is `share_media`, which reads the whole file and copies it
    /// into a Java byte array on the render thread.
    fn export_stl(&mut self, host: &Host, path: std::path::PathBuf) {
        let out = ringdesign_core::mesh::build(&self.design, &self.lib, ring::EXPORT);
        match ringdesign_core::stl::write_stl(&path, &out.mesh) {
            Ok(bytes) => {
                self.status = format!(
                    "{} tris · {:.1} MB",
                    out.report.validation.triangle_count,
                    bytes as f64 / 1048576.0
                );
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "ring.stl".into());
                // An explicit MIME: `share_file` has no `stl` entry and would fall through to
                // application/octet-stream, and MediaProvider renames a file whose extension
                // disagrees with its type.
                host.share_media(path.to_string_lossy().into_owned(), name, "model/stl");
                host.haptic(Haptic::Success);
            }
            Err(e) => {
                self.status = format!("export failed: {e}");
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
        ctx.set_visuals(egui::Visuals::dark());
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = egui::vec2(6.0, 6.0);
            s.spacing.button_padding = egui::vec2(10.0, 8.0);
            s.spacing.scroll.bar_width = 14.0;
        });
    }

    fn on_start(&mut self, ctx: &egui::Context, host: &Host) {
        // `library.rs` reads XDG_DATA_HOME / HOME, both unset on Android, and falls back to ".".
        if let Some(dir) = host.documents_dir() {
            let root = std::path::PathBuf::from(dir).join("ringdesigner");
            for sub in ["designs", "alphas", "exports"] {
                let _ = std::fs::create_dir_all(root.join(sub));
            }
            if let Ok(loaded) = library::load_design(root.join(AUTOSAVE)) {
                self.design = loaded;
                log::info!("restored {}", AUTOSAVE);
            }
            self.data_root = Some(root);
        }

        self.px_per_mm = device_px_per_mm();
        self.has_stylus = host.has_stylus();
        // Resting a hand on the glass should not draw, so pen-only is the default where there is
        // a pen. It stays a toggle: a finger is still the fastest way to rough something in.
        self.brush.stylus_only = self.has_stylus;
        // Strokes are the source of truth; the raster is derived, so bake before the first build.
        {
            let lib = Arc::make_mut(&mut self.lib);
            self.design.bake_drawn(lib);
        }
        self.worker = Some(Worker::spawn(ctx.clone()));
        self.mark_dirty();
        // One immediate draft build so there is something on screen before the debounce elapses.
        self.dispatch(false);
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.probe = host.stylus_probe();
        self.tick(ui.ctx());

        egui::Panel::bottom(egui::Id::new("tabs")).show(ui, |ui| {
            ui.horizontal(|ui| {
                for tab in Tab::ALL {
                    if ui.selectable_label(self.tab == *tab, tab.label()).clicked() {
                        self.tab = *tab;
                    }
                }
            });
        });

        egui::Panel::bottom(egui::Id::new("status")).show(ui, |ui| {
            // While painting, what the pen is doing beats what the mesh last cost.
            let line = match (self.tab, self.readout.as_ref()) {
                (Tab::Band | Tab::Tile, Some(r)) => r.as_str(),
                _ => self.status.as_str(),
            };
            ui.label(egui::RichText::new(line).small().weak());
        });

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Ring => self.ring_tab(ui),
            Tab::Band => self.paint_tab(ui, host, Domain::Band),
            Tab::Tile => self.paint_tab(ui, host, Domain::Tile),
            Tab::Alphas => self.alphas_tab(ui, host),
            Tab::Files => self.files_tab(ui, host),
            Tab::Bench => self.bench_tab(ui, host),
        });
    }
}

/// Filesystem-safe stem from a design name.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "ring".into() } else { s }
}

/// Verdict colour and text for the toolbar chip. The undercut fraction is the number that decides
/// it, so it is shown rather than the label alone.
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
/// `None` for now: the framework exposes no display DPI, and the obvious substitute —
/// `pixels_per_point`, which is Android's *bucketed* density — is off by around 10% from a panel's
/// real xdpi. A 1:1 mode that is 10% wrong is worse than no 1:1 mode, because the whole reason the
/// feature beats a monitor is that a monitor's reported DPI is a lie. Needs a
/// `DisplayMetrics.xdpi` accessor on `HostExt`, following `jni_has_stylus`.
fn device_px_per_mm() -> Option<f32> {
    None
}
