//! Android backend for the shared [`egui_mobile_core`] runtime. Implement [`EguiApp`] and invoke
//! [`app!`]; the macro emits `android_main`. The render loop is driven by `eframe` (winit + wgpu,
//! Vulkan/GL) which handles the Android surface-recreation-on-resume dance and input/IME; the
//! `Host` capability bridge is threaded through and (in the JNI layer) drained to Android APIs.

pub use android_activity::AndroidApp;
pub use egui;
pub use egui_mobile_core::{CreateContext, EguiApp, Haptic, Host, Insets, Permission};

/// Adapts an [`EguiApp`] + [`Host`] to `eframe::App`. Each frame it opens a central panel, hands
/// the root `ui` to the app, then drains queued host requests (JNI dispatch lives in `host`).
struct Adapter {
    app: Box<dyn EguiApp>,
    host: Host,
    started: bool,
    /// Events queued by the text-actions bar, injected into the next frame's input.
    pending_events: Vec<egui::Event>,
    frame: u64,
    /// Cached "clipboard has text" plus the frame at which to re-poll it.
    has_clip: bool,
    next_clip_poll: u64,
    /// Most recent focused widget; restored after a bar tap surrenders focus.
    last_focus: Option<egui::Id>,
    /// Text-actions bar rect from the previous frame.
    bar_rect: Option<egui::Rect>,
    /// A pointer press that began inside the bar has not been released yet.
    bar_touch: bool,
}

impl eframe::App for Adapter {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.started {
            self.started = true;
            crate::host::init_documents_dir(&self.host);
            self.app.on_start(ui.ctx(), &self.host);
        }
        self.frame += 1;
        // Feed Android WindowInsets (status bar / camera cutout / nav bar) into the host, then
        // inset the UI so app content isn't drawn under the system bars (Android 15 is edge-to-edge
        // by default). Apps can still read `host.safe_area_insets()` for finer control.
        crate::host::update_insets(&self.host, ui.ctx().pixels_per_point());
        // While a tap is on the bar, disable click-away focus surrender so the focused text
        // field (or plugin viewport) still has focus when the queued event lands.
        let pressed_in_bar = self.bar_rect.is_some_and(|r| {
            ui.ctx().input(|i| {
                i.events.iter().any(|e| {
                    matches!(e, egui::Event::PointerButton { pos, pressed: true, .. } if r.contains(*pos))
                })
            })
        });
        self.bar_touch |= pressed_in_bar;
        let hold = self.bar_touch;
        ui.ctx().options_mut(|o| {
            o.input_options.surrender_focus_on = if hold {
                egui::SurrenderFocusOn::Never
            } else {
                egui::SurrenderFocusOn::Clicks
            };
        });
        if ui.ctx().input(|i| !i.pointer.any_down()) {
            self.bar_touch = false;
        }
        if !self.pending_events.is_empty() {
            let events = std::mem::take(&mut self.pending_events);
            // Extend `raw` too: the plugin viewport forwards guest input from `raw.events`.
            ui.ctx().input_mut(|i| {
                i.raw.events.extend(events.iter().cloned());
                i.events.extend(events);
            });
        }
        let insets = self.host.safe_area_insets();
        let mut rect = ui.max_rect();
        rect.min.x += insets.left;
        rect.min.y += insets.top;
        rect.max.x -= insets.right;
        rect.max.y -= insets.bottom;
        // `full_rect` (system-bar insets only) positions the floating text-actions bar; the app
        // itself gets a rect shortened by the soft keyboard so bottom-anchored fields and panels
        // reflow above it instead of hiding underneath.
        let full_rect = rect;
        let keyboard = self.host.keyboard_height();
        if keyboard > 0.0 {
            let overlap = (keyboard - insets.bottom).max(0.0);
            rect.max.y = (rect.max.y - overlap).max(rect.min.y + 1.0);
        }
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            self.app.update(ui, &self.host);
        });
        if let Some(id) = ui.ctx().memory(|m| m.focused()) {
            self.last_focus = Some(id);
        }
        // Mirror this frame's egui copies (host widgets and plugin viewports alike) into the
        // system clipboard; winit has no Android clipboard backend.
        let copied = ui.ctx().output(|o| {
            o.commands.iter().rev().find_map(|c| match c {
                egui::OutputCommand::CopyText(t) if !t.is_empty() => Some(t.clone()),
                _ => None,
            })
        });
        if let Some(text) = copied {
            self.host.copy_text(text);
        }
        self.text_actions_bar(ui, full_rect);
        crate::host::drain(&self.host);
    }
}

impl Adapter {
    /// Floating Paste/Copy/Cut/Select-all bar shown while a text field is being edited — the
    /// Android equivalent of the selection context menu, since egui draws its own text widgets.
    fn text_actions_bar(&mut self, ui: &egui::Ui, rect: egui::Rect) {
        let ctx = ui.ctx().clone();
        let keyboard = self.host.keyboard_height();
        // ime is set while a host-side TextEdit has focus; keyboard_requested covers guest
        // (plugin) text fields, and a nonzero measured height covers everything else.
        let show = keyboard > 0.0
            || ctx.output(|o| o.ime.is_some())
            || crate::host::keyboard_requested();
        if !show {
            self.next_clip_poll = 0;
            self.bar_rect = None;
            return;
        }
        if self.frame >= self.next_clip_poll {
            self.has_clip = crate::host::clipboard_text().is_some();
            self.next_clip_poll = self.frame + 30;
        }
        let insets = self.host.safe_area_insets();
        // Shown but unmeasured (guest field, or an inset read that returned 0) → assume a
        // typical keyboard fraction so the bar still floats above it.
        let kb = if keyboard > 0.0 { keyboard } else { rect.height() * 0.4 };
        let overlap = (kb - insets.bottom).max(0.0);
        let keyboard_top = rect.bottom() - overlap;
        // Above the keyboard, raised further to clear the focused field when egui reports it.
        let field = ctx.output(|o| o.ime.as_ref().map(|ime| (ime.rect.top(), ime.rect.center().x)));
        let (anchor_y, anchor_x) = match field {
            Some((top, cx)) => (keyboard_top.min(top), cx),
            None => (keyboard_top, rect.center().x),
        };
        let pos = egui::pos2(anchor_x, anchor_y - 8.0);
        let mut acted = false;
        let area = egui::Area::new(egui::Id::new("egui-android-text-actions"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::CENTER_BOTTOM)
            .fixed_pos(pos)
            .constrain_to(rect)
            .show(&ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.add_enabled(self.has_clip, egui::Button::new("Paste")).clicked()
                            && let Some(text) = crate::host::clipboard_text()
                        {
                            self.pending_events.push(egui::Event::Paste(text));
                            acted = true;
                        }
                        if ui.button("Copy").clicked() {
                            self.pending_events.push(egui::Event::Copy);
                            acted = true;
                        }
                        if ui.button("Cut").clicked() {
                            self.pending_events.push(egui::Event::Cut);
                            acted = true;
                        }
                        if ui.button("Select all").clicked() {
                            self.pending_events.push(egui::Event::Key {
                                key: egui::Key::A,
                                physical_key: None,
                                pressed: true,
                                repeat: false,
                                modifiers: egui::Modifiers::COMMAND,
                            });
                            acted = true;
                        }
                    });
                });
            });
        self.bar_rect = Some(area.response.rect);
        // Backstop for a tap that landed before the bar rect was known: hand focus back so
        // the queued event reaches the field next frame.
        if acted && let Some(id) = self.last_focus {
            ctx.memory_mut(|m| m.request_focus(id));
        }
    }
}

/// Entry point invoked by [`app!`]. Boots logging, installs a panic logger, and runs eframe with
/// the Android app handle and the wgpu renderer.
pub fn run(app: AndroidApp, mut factory: impl FnMut(&CreateContext) -> Box<dyn EguiApp> + 'static) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    std::panic::set_hook(Box::new(|info| {
        log::error!("egui-android panic: {info}");
    }));

    host::set_android_app(app.clone());

    let mut options = eframe::NativeOptions::default();
    options.android_app = Some(app);
    options.renderer = eframe::Renderer::Wgpu;

    let result = eframe::run_native(
        "egui-android",
        options,
        Box::new(move |cc| {
            // Install the plugin paint callback into eframe's wgpu renderer (feature `plugins`).
            #[cfg(feature = "plugins")]
            if let Some(rs) = cc.wgpu_render_state.as_ref() {
                let mut renderer = rs.renderer.write();
                egui_ios_plugin_host::install(&mut renderer, rs.target_format, 1);
            }
            let cx = CreateContext {
                width_px: 0,
                height_px: 0,
                pixels_per_point: cc.egui_ctx.pixels_per_point(),
            };
            let app = factory(&cx);
            Ok(Box::new(Adapter {
                app,
                host: Host::new(),
                started: false,
                pending_events: Vec::new(),
                frame: 0,
                has_clip: false,
                next_clip_poll: 0,
                last_focus: None,
                bar_rect: None,
                bar_touch: false,
            }))
        }),
    );
    if let Err(e) = result {
        log::error!("egui-android run_native failed: {e}");
    }
}

pub mod host;
pub use host::HostExt;

#[cfg(feature = "plugins")]
pub mod plugins;

/// Generates `android_main` for a type implementing [`EguiApp`].
///
/// `factory` is any `Fn(&CreateContext) -> impl EguiApp`, e.g. `app!(MyApp::new)`.
#[macro_export]
macro_rules! app {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        fn android_main(app: $crate::AndroidApp) {
            $crate::run(app, |cc| ::std::boxed::Box::new($factory(cc)));
        }
    };
}
