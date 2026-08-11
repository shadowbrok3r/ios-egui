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
        (Tab::Alphas, "\u{1F3A8}", ""),
        (Tab::Files, "\u{1F4C1}", ""),
        (Tab::Bench, "\u{26A1}", ""),
    ];

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

use crate::ring::METAL_TINT;

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
            field: None,
            stones: None,
            dfm: Vec::new(),
            preview_mesh: None,
            probe_info: None,
            show_gems: true,
            design_open: false,
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
                    self.dfm = ringdesign_core::dfm::findings(&self.design);
                    self.field = done.field;
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
                    let sel = self.pane.shade == *mode;
                    if ui.add(crate::theme::selectable(sel, mode.label())).clicked() {
                        self.pane.shade = *mode;
                    }
                }
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
                    let (tint, text) = field_chip(f);
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
                    if !self.dfm.is_empty() {
                        let text: Vec<String> = self
                            .dfm
                            .iter()
                            .map(|w| format!("{}: {}", w.label, w.message))
                            .collect();
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 170, 90),
                            format!("DFM {}", self.dfm.len()),
                        )
                        .on_hover_text(text.join("\n"));
                    }
                } else if let Some(cast) = self.cast.as_ref() {
                    ui.separator();
                    let (tint, text) = verdict_chip(cast);
                    ui.colored_label(tint, text);
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

        let mut dirty = false;
        ui.scope(|ui| {
            ui.label(egui::RichText::new("ring").weak());
            let mut size = self.design.size.0;
            if ui
                .add(egui::Slider::new(&mut size, 3.0..=13.0).step_by(0.25).text("US size"))
                .changed()
            {
                self.design.size = RingSize::new(size);
                dirty = true;
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
            ui.label(egui::RichText::new("stock generators").weak());
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button("Auto pavé")
                    .on_hover_text(
                        "Pack the wider side face with gypsy seats for 1.5 mm melee — an                          editable group, rows wrap-exact around the ring.",
                    )
                    .clicked()
                {
                    match ringdesign_core::pave::fill(
                        &self.design,
                        &ringdesign_core::pave::PaveSpec::default(),
                    ) {
                        Some((entry, out)) => {
                            self.design.layers.layers.push(entry);
                            self.status = match out.note {
                                Some(n) => format!("pavé: {} seats · {n}", out.seats),
                                None => format!("pavé: {} seats in {} rows", out.seats, out.rows),
                            };
                            dirty = true;
                        }
                        None => {
                            self.status =
                                "no side face to fill — square the sides first".into();
                        }
                    }
                }
                if ui
                    .button("Channel set")
                    .on_hover_text(
                        "Rails and a recessed channel on the wider side face. Wants a thick                          squared band: a 1.5 mm stone needs ~3 mm of face.",
                    )
                    .clicked()
                {
                    let gem = ringdesign_core::gem::Gem::calibrated(
                        ringdesign_core::gem::GemCut::Round,
                        1.5,
                    );
                    match ringdesign_core::pave::channel_set(&self.design, gem, 0.6) {
                        Some(entry) => {
                            self.design.layers.layers.push(entry);
                            self.status = "channel set added".into();
                            dirty = true;
                        }
                        None => {
                            self.status = "side face too narrow — square and thicken the band".into();
                        }
                    }
                }
                if ui.button("Clear layers").on_hover_text("Remove every layer, keep the band").clicked()
                    && !self.design.layers.layers.is_empty()
                {
                    self.design.layers.layers.clear();
                    self.status = "layers cleared".into();
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
                        self.design.bake_all(lib);
                        self.thumbs.clear();
                        self.mark_dirty();
                        host.haptic(Haptic::Light);
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
                    let path = designs.join(format!("{}.ring.json", slug(&self.design.name)));
                    let _ = std::fs::create_dir_all(&designs);
                    self.status = match library::save_design(&path, &self.design) {
                        Ok(()) => format!("saved {}", path.display()),
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
                            self.design = d;
                            let lib = Arc::make_mut(&mut self.lib);
                            self.design.bake_all(lib);
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
                    let _ = std::fs::create_dir_all(&exports);
                    let path = exports.join(format!("{}.stl", slug(&self.design.name)));
                    self.export_mesh(host, path, false);
                }
                if ui.button("3MF — units stated").clicked() {
                    let _ = std::fs::create_dir_all(&exports);
                    let path = exports.join(format!("{}.3mf", slug(&self.design.name)));
                    self.export_mesh(host, path, true);
                }
                if ui
                    .button("GLB — AR and web viewers")
                    .on_hover_text("glTF binary, metre-scaled")
                    .clicked()
                {
                    let _ = std::fs::create_dir_all(&exports);
                    let path = exports.join(format!("{}.glb", slug(&self.design.name)));
                    self.share_glb(host, path);
                }
            });
            crate::theme::up_menu(ui, "\u{2728} Share", |ui| {
                if ui.button("Casting sheet").clicked() {
                    let _ = std::fs::create_dir_all(&exports);
                    let path = exports.join(format!("{}_sheet.html", slug(&self.design.name)));
                    self.share_spec(host, path);
                }
                if ui.button("Render photo").clicked() {
                    let _ = std::fs::create_dir_all(&exports);
                    let path = exports.join(format!("{}.png", slug(&self.design.name)));
                    self.share_render(host, path);
                }
                if ui
                    .button("Turntable spin")
                    .on_hover_text("A looping 36-frame GIF")
                    .clicked()
                {
                    let _ = std::fs::create_dir_all(&exports);
                    let path = exports.join(format!("{}.gif", slug(&self.design.name)));
                    self.share_turntable(host, path);
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
                                self.design.bake_all(lib);
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
                self.design = *d;
                let lib = Arc::make_mut(&mut self.lib);
                self.design.bake_all(lib);
                self.thumbs.clear();
                self.picked_alpha = None;
                self.status = "pulled from desktop".into();
                self.mark_dirty();
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

    /// Build at export resolution and hand the file to the system share sheet.
    ///
    /// Measured at 226 ms end to end on this device, so it runs inline rather than on the worker;
    /// the part that actually stalls is `share_media`, which reads the whole file and copies it
    /// into a Java byte array on the render thread.
    /// The printable tech sheet, straight to the share sheet — the thing to
    /// send a caster from the couch.
    fn load_template_design(&mut self, d: RingDesign, what: &str) {
        self.design = d;
        let lib = Arc::make_mut(&mut self.lib);
        self.design.bake_all(lib);
        self.thumbs.clear();
        self.picked_alpha = None;
        self.status = format!("started from {what}");
        self.mark_dirty();
    }

    fn share_render(&mut self, host: &Host, path: std::path::PathBuf) {
        let out = ringdesign_core::mesh::build(&self.design, &self.lib, ring::EXPORT);
        match ringdesign_core::render::write_png(&path, &out.mesh, 0.55, 1.12, 1280, METAL_TINT) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "ring.png".into());
                host.share_media(path.to_string_lossy().into_owned(), name, "image/png");
                self.status = "render shared".into();
                host.haptic(Haptic::Success);
            }
            Err(e) => self.status = format!("render failed: {e}"),
        }
    }

    fn share_turntable(&mut self, host: &Host, path: std::path::PathBuf) {
        // The preview mesh: 36 software-rastered frames of 655k export
        // triangles is seconds of spinner for no visible gain at 480 px.
        let out = ringdesign_core::mesh::build(&self.design, &self.lib, ring::PREVIEW);
        match ringdesign_core::render::write_turntable_gif(&path, &out.mesh, 36, 480, METAL_TINT) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "ring.gif".into());
                host.share_media(path.to_string_lossy().into_owned(), name, "image/gif");
                self.status = "turntable shared".into();
                host.haptic(Haptic::Success);
            }
            Err(e) => self.status = format!("turntable failed: {e}"),
        }
    }

    fn share_glb(&mut self, host: &Host, path: std::path::PathBuf) {
        let out = ringdesign_core::mesh::build(&self.design, &self.lib, ring::EXPORT);
        match ringdesign_core::gltf::write_glb(&path, &out.mesh, &self.design.name, METAL_TINT) {
            Ok(bytes) => {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "ring.glb".into());
                host.share_media(path.to_string_lossy().into_owned(), name, "model/gltf-binary");
                self.status = format!("GLB shared · {:.1} MB", bytes as f64 / 1048576.0);
                host.haptic(Haptic::Success);
            }
            Err(e) => self.status = format!("GLB failed: {e}"),
        }
    }

    fn share_spec(&mut self, host: &Host, path: std::path::PathBuf) {
        let out = ringdesign_core::mesh::build(&self.design, &self.lib, ring::EXPORT);
        let field = ringdesign_core::castability::attributed_field_report(
            &self.design,
            &self.lib,
            &self.design.draft,
            160,
            112,
        );
        let stones = ringdesign_core::stones::report(&self.design, field.parting_z_mm);
        let dfm = ringdesign_core::dfm::findings(&self.design);
        let page = ringdesign_core::spec::html(
            &self.design,
            &out.report,
            &field,
            stones.as_ref(),
            &dfm,
            concat!("RingDesigner Android ", env!("CARGO_PKG_VERSION")),
        );
        match std::fs::write(&path, page) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "sheet.html".into());
                host.share_media(path.to_string_lossy().into_owned(), name, "text/html");
                self.status = "casting sheet shared".into();
                host.haptic(Haptic::Success);
            }
            Err(e) => self.status = format!("sheet failed: {e}"),
        }
    }

    fn export_mesh(&mut self, host: &Host, path: std::path::PathBuf, as_3mf: bool) {
        use ringdesign_core::metal;
        let out = ringdesign_core::mesh::build(&self.design, &self.lib, ring::EXPORT);
        // The patternmaker's shrink: cut oversize and *named* as a pattern,
        // so an oversize file cannot be poured as nominal by mistake.
        let (mesh, name) = match self.shrink_metal.and_then(|i| metal::METALS.get(i)) {
            Some(m) => (
                out.mesh.scaled(metal::pattern_scale(m.shrink_pct)),
                format!("{} [pattern +{:.1}% for {}]", self.design.name, m.shrink_pct, m.name),
            ),
            None => (out.mesh.clone(), self.design.name.clone()),
        };
        let written = if as_3mf {
            ringdesign_core::threemf::write_3mf(&path, &mesh, &name, &self.design.size.display())
        } else {
            ringdesign_core::stl::write_stl(&path, &mesh, &name)
        };
        match written {
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
                let _ = &name;
                // An explicit MIME: `share_file` has no `stl` entry and would fall through to
                // application/octet-stream, and MediaProvider renames a file whose extension
                // disagrees with its type.
                host.share_media(
                    path.to_string_lossy().into_owned(),
                    name,
                    if as_3mf { "model/3mf" } else { "model/stl" },
                );
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
            self.data_root = Some(root);
        }

        self.px_per_mm = device_px_per_mm(host);
        self.has_stylus = host.has_stylus();
        // Resting a hand on the glass should not draw, so pen-only is the default where there is
        // a pen. It stays a toggle: a finger is still the fastest way to rough something in.
        self.brush.stylus_only = self.has_stylus;
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

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        self.probe = host.stylus_probe();
        self.poll_sync(host);
        self.tick(ui.ctx());

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

        if let Some(chrome) = chrome {
            crate::frost::remember(ui.ctx(), chrome);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(6))
            .show(ui, |ui| match self.tab {
                Tab::Ring => self.ring_tab(ui),
                Tab::Band => self.paint_tab(ui, host, Domain::Band),
                Tab::Tile => self.paint_tab(ui, host, Domain::Tile),
                Tab::Alphas => self.alphas_tab(ui, host),
                Tab::Files => self.files_tab(ui, host),
                Tab::Bench => self.bench_tab(ui, host),
            });
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
fn field_chip(f: &ringdesign_core::castability::FieldReport) -> (egui::Color32, String) {
    use ringdesign_core::castability::Verdict;
    let tint = match f.verdict {
        Verdict::Castable => egui::Color32::from_rgb(82, 199, 115),
        Verdict::Marginal => egui::Color32::from_rgb(242, 194, 61),
        Verdict::NotCastable => egui::Color32::from_rgb(240, 105, 120),
    };
    (
        tint,
        format!(
            "{} · {:.2}% · wall {:.2} mm",
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

