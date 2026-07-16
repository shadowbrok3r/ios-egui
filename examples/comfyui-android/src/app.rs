//! The Android UI: a Generate tab (connect, params, output) and a Logs tab (full request/engine
//! log with copy/share, mirrored to logcat as `comfyui`).

use std::time::Duration;

use egui_mobile::{CreateContext, EguiApp, Haptic, Host, app, egui};

use crate::engine::{Engine, Msg};
use crate::logger::{self, Logger};
use crate::schema::SchemaSet;
use crate::types::{
    FALLBACK_SAMPLERS, FALLBACK_SCHEDULERS, Img2ImgSource, Mode, Params, Settings, fallback_vec,
};

enum Conn {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Generate,
    Logs,
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
    conn: Conn,
    schemas: Option<SchemaSet>,
    checkpoints: Vec<String>,
    samplers: Vec<String>,
    schedulers: Vec<String>,
    ckpt_filter: String,

    params: Params,

    running: bool,
    progress: (u32, u32),
    status: String,

    preview: Option<egui::TextureHandle>,
    result: Option<egui::TextureHandle>,
    result_bytes: Option<Vec<u8>>,
    save_counter: u32,
    note: String,
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
            conn: Conn::Disconnected,
            schemas: None,
            checkpoints: Vec::new(),
            samplers: fallback_vec(FALLBACK_SAMPLERS),
            schedulers: fallback_vec(FALLBACK_SCHEDULERS),
            ckpt_filter: String::new(),
            params: Params::default(),
            running: false,
            progress: (0, 0),
            status: String::new(),
            preview: None,
            result: None,
            result_bytes: None,
            save_counter: 0,
            note: String::new(),
        }
    }

    fn handle(&mut self, ctx: &egui::Context, host: &Host, m: Msg) {
        match m {
            Msg::Connected { schemas, checkpoints, samplers, schedulers } => {
                self.conn = Conn::Connected;
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
                self.save_settings(host);
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
            Msg::Done => {
                self.running = false;
                self.progress = (0, 0);
                self.status = "Done".into();
                host.haptic(Haptic::Success);
                host.notify("ComfyUI", "Generation finished");
            }
            Msg::Cancelled => {
                self.running = false;
                self.progress = (0, 0);
                self.preview = None;
                self.status = "Cancelled".into();
            }
            Msg::GenError(e) => {
                self.running = false;
                self.progress = (0, 0);
                self.status = format!("Error: {e}");
                host.haptic(Haptic::Error);
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
        let params = self.params.clone();
        let current = self.result_bytes.clone();
        self.engine.as_mut().unwrap().generate(params, current);
        host.haptic(Haptic::Medium);
        self.save_settings(host);
    }

    fn save_image(&mut self, host: &Host) {
        let Some(bytes) = self.result_bytes.clone() else {
            return;
        };
        let Some(dir) = host.documents_dir() else {
            self.note = "No storage directory".into();
            return;
        };
        let folder = format!("{dir}/comfyui");
        let _ = std::fs::create_dir_all(&folder);
        self.save_counter += 1;
        let path = format!("{folder}/output-{}.png", self.save_counter);
        match std::fs::write(&path, &bytes) {
            Ok(()) => {
                self.note = format!("Saved to {path}");
                self.log.info(format!("saved image: {path}"));
                host.haptic(Haptic::Success);
            }
            Err(e) => {
                self.note = format!("Save failed: {e}");
                self.log.error(format!("save failed: {e}"));
            }
        }
    }

    fn settings_path(host: &Host) -> Option<String> {
        host.documents_dir().map(|d| format!("{d}/comfyui_settings.json"))
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
            self.params = saved.params;
        }
    }

    fn save_settings(&self, host: &Host) {
        let Some(path) = Self::settings_path(host) else {
            return;
        };
        let settings = Settings {
            server_url: self.server_url.clone(),
            api_key: self.api_key.clone(),
            params: self.params.clone(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&settings) {
            let _ = std::fs::write(path, json);
        }
    }

    fn connect(&mut self, host: &Host) {
        self.conn = Conn::Connecting;
        self.status.clear();
        let url = self.server_url.clone();
        let key = self.api_key.clone();
        self.engine.as_mut().unwrap().connect(url, key);
        host.haptic(Haptic::Light);
    }

    fn connection_panel(&mut self, ui: &mut egui::Ui, host: &Host) {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label("Server");
                ui.add(
                    egui::TextEdit::singleline(&mut self.server_url)
                        .hint_text("https://host/api  or  http://192.168.x.x:8188")
                        .desired_width(f32::INFINITY),
                );
            });
            ui.horizontal(|ui| {
                ui.label("API key");
                ui.add(
                    egui::TextEdit::singleline(&mut self.api_key)
                        .password(true)
                        .hint_text("optional — sent as X-Api-Key / Bearer")
                        .desired_width(f32::INFINITY),
                );
            });
            ui.horizontal(|ui| {
                let connecting = matches!(self.conn, Conn::Connecting);
                let label = match self.conn {
                    Conn::Disconnected => "Connect",
                    Conn::Connecting => "Connecting…",
                    Conn::Connected => "Reconnect",
                    Conn::Failed(_) => "Retry",
                };
                let enabled = !connecting && !self.server_url.trim().is_empty();
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    self.connect(host);
                }
                match &self.conn {
                    Conn::Connected => {
                        let nodes = self.schemas.as_ref().map(|s| s.nodes.len()).unwrap_or(0);
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 220, 140),
                            format!("• connected ({nodes} nodes)"),
                        );
                    }
                    Conn::Failed(e) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 120, 120),
                            format!("• {} — see Logs", elide(e, 120)),
                        );
                    }
                    Conn::Connecting => {
                        ui.spinner();
                    }
                    Conn::Disconnected => {
                        ui.weak("• not connected");
                    }
                }
            });
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
                                .hint_text("https://…/image.png")
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
                self.save_image(host);
            }
        }
    }

    fn generate_tab(&mut self, ui: &mut egui::Ui, host: &Host) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.connection_panel(ui, host);

                let connected = matches!(self.conn, Conn::Connected)
                    || self.engine.as_ref().unwrap().is_connected();
                ui.add_enabled_ui(connected, |ui| self.controls(ui, host));

                self.output(ui, host);
                ui.add_space(12.0);
            });
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
        ctx.set_visuals(egui::Visuals::dark());
    }

    fn update(&mut self, ui: &mut egui::Ui, host: &Host) {
        if self.engine.is_none() {
            self.engine = Some(Engine::new(ui.ctx().clone(), self.log.clone()));
        }
        if !self.loaded {
            self.loaded = true;
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

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("ComfyUI");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.selectable_value(&mut self.tab, Tab::Logs, "Logs");
                ui.selectable_value(&mut self.tab, Tab::Generate, "Generate");
            });
        });
        ui.separator();

        match self.tab {
            Tab::Generate => self.generate_tab(ui, host),
            Tab::Logs => self.logs_tab(ui, host),
        }

        if self.running {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
    }
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

/// Shorten a string for display so a pathological server value can't blow up text layout. The full
/// value is still stored; only what's painted is bounded.
fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
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
