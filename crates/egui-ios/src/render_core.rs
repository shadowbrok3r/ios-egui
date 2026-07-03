//! Generic egui → wgpu(Metal) renderer driven by a Swift-owned `CAMetalLayer`. App-agnostic.

use std::ffi::c_void;

use egui::{Event, Modifiers, PointerButton, Pos2, Vec2};

use crate::input::{hid_to_egui_key, ios_modifiers_to_egui};

const LONG_PRESS_SECS: f64 = 0.5;
const MOVE_SLOP_PTS: f32 = 10.0;

struct TouchTrack {
    start_pos: Pos2,
    down_time: Option<f64>,
    moved: bool,
    long_fired: bool,
}

pub struct RenderCore {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,

    pub(crate) egui_ctx: egui::Context,
    pixels_per_point: f32,
    width_px: u32,
    height_px: u32,

    events: Vec<Event>,
    modifiers: Modifiers,
    pointer_pos: Pos2,
    touch: Option<TouchTrack>,
    active: bool,
    pub(crate) wants_keyboard: bool,
    pending_open_url: Option<String>,
    pending_copy: Option<String>,
}

impl RenderCore {
    /// Create the renderer from a `CAMetalLayer` pointer. The caller must keep the layer (and
    /// its hosting view) alive until the runtime is freed.
    #[cfg_attr(not(target_vendor = "apple"), allow(unreachable_code, unused_variables))]
    pub fn new(ca_metal_layer: *mut c_void, width_px: u32, height_px: u32, ppp: f32) -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });

        #[cfg(target_vendor = "apple")]
        let surface = unsafe {
            instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                    ca_metal_layer,
                ))
                .expect("create_surface_unsafe(CoreAnimationLayer)")
        };
        #[cfg(not(target_vendor = "apple"))]
        let surface: wgpu::Surface<'static> =
            unimplemented!("egui-ios renders only on Apple (iOS) targets");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("request_adapter");

        // Request exactly what the adapter supports so device creation never exceeds iOS limits.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("egui-ios"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .expect("request_device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        // Opaque compositing matches the proven iOS path; prefer Opaque, else the first reported.
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|m| *m == wgpu::CompositeAlphaMode::Opaque)
            .unwrap_or(caps.alpha_modes[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width_px.max(1),
            height: height_px.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let renderer = egui_wgpu::Renderer::new(
            &device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: true,
                predictable_texture_filtering: false,
            },
        );

        let egui_ctx = egui::Context::default();
        egui_ctx.set_pixels_per_point(ppp);

        RenderCore {
            device,
            queue,
            surface,
            config,
            renderer,
            egui_ctx,
            pixels_per_point: ppp,
            width_px: width_px.max(1),
            height_px: height_px.max(1),
            events: Vec::new(),
            modifiers: Modifiers::default(),
            pointer_pos: Pos2::ZERO,
            touch: None,
            active: true,
            wants_keyboard: false,
            pending_open_url: None,
            pending_copy: None,
        }
    }

    /// Wire plugin paint callbacks into this renderer (feature `plugins`).
    #[cfg(feature = "plugins")]
    pub(crate) fn install_plugin_painter(&mut self) {
        egui_ios_plugin_host::install(&mut self.renderer, self.config.format, 1);
    }

    /// Take an `open url` command produced by egui (e.g. a hyperlink click) this frame.
    pub(crate) fn take_open_url(&mut self) -> Option<String> {
        self.pending_open_url.take()
    }

    /// Take a `copy to clipboard` command produced by egui this frame.
    pub(crate) fn take_copied_text(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    pub fn resize(&mut self, width_px: u32, height_px: u32) {
        self.width_px = width_px.max(1);
        self.height_px = height_px.max(1);
        self.config.width = self.width_px;
        self.config.height = self.height_px;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_pixels_per_point(&mut self, ppp: f32) {
        self.pixels_per_point = ppp;
        self.egui_ctx.set_pixels_per_point(ppp);
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    pub fn touch_began(&mut self, x_pt: f32, y_pt: f32) {
        let pos = Pos2::new(x_pt, y_pt);
        self.pointer_pos = pos;
        self.events.push(Event::PointerMoved(pos));
        self.events.push(Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: self.modifiers,
        });
        self.touch = Some(TouchTrack {
            start_pos: pos,
            down_time: None,
            moved: false,
            long_fired: false,
        });
    }

    pub fn touch_moved(&mut self, x_pt: f32, y_pt: f32) {
        let pos = Pos2::new(x_pt, y_pt);
        self.pointer_pos = pos;
        self.events.push(Event::PointerMoved(pos));
        if let Some(t) = &mut self.touch {
            if (pos - t.start_pos).length() > MOVE_SLOP_PTS {
                t.moved = true;
            }
        }
    }

    pub fn touch_ended(&mut self, x_pt: f32, y_pt: f32) {
        let pos = Pos2::new(x_pt, y_pt);
        self.events.push(Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: self.modifiers,
        });
        self.events.push(Event::PointerGone);
        self.touch = None;
    }

    pub fn touch_cancelled(&mut self, x_pt: f32, y_pt: f32) {
        self.touch_ended(x_pt, y_pt);
    }

    pub fn insert_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.events.push(Event::Text(text.to_owned()));
        }
    }

    pub fn delete_backward(&mut self) {
        for pressed in [true, false] {
            self.events.push(Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: self.modifiers,
            });
        }
    }

    pub fn key_event(&mut self, hid_key_code: i32, modifier_flags: i32, pressed: bool) {
        self.modifiers = ios_modifiers_to_egui(modifier_flags);
        if let Some(key) = hid_to_egui_key(hid_key_code) {
            self.events.push(Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: self.modifiers,
            });
        }
    }

    pub fn scroll(&mut self, dx_pt: f32, dy_pt: f32) {
        self.events.push(Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(dx_pt, dy_pt),
            phase: egui::TouchPhase::Move,
            modifiers: self.modifiers,
        });
    }

    pub fn pointer_moved(&mut self, x_pt: f32, y_pt: f32) {
        let pos = Pos2::new(x_pt, y_pt);
        self.pointer_pos = pos;
        self.events.push(Event::PointerMoved(pos));
    }

    pub fn pointer_gone(&mut self) {
        self.events.push(Event::PointerGone);
    }

    /// Synthesize a secondary (context-menu) click when a touch is held in place.
    fn apply_long_press(&mut self, time: f64) {
        let fire_at = match &mut self.touch {
            Some(t) => {
                let down = *t.down_time.get_or_insert(time);
                if !t.moved && !t.long_fired && time - down >= LONG_PRESS_SECS {
                    t.long_fired = true;
                    Some(t.start_pos)
                } else {
                    None
                }
            }
            None => None,
        };
        if let Some(pos) = fire_at {
            for pressed in [true, false] {
                self.events.push(Event::PointerButton {
                    pos,
                    button: PointerButton::Secondary,
                    pressed,
                    modifiers: self.modifiers,
                });
            }
        }
    }

    /// Run one frame: assemble input, run the UI closure, paint, and present.
    pub fn render(&mut self, time: f64, mut build_ui: impl FnMut(&mut egui::Ui)) {
        self.apply_long_press(time);

        let screen_rect = egui::Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(
                self.width_px as f32 / self.pixels_per_point,
                self.height_px as f32 / self.pixels_per_point,
            ),
        );
        let raw_input = egui::RawInput {
            screen_rect: Some(screen_rect),
            time: Some(time),
            modifiers: self.modifiers,
            focused: self.active,
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };

        let full_output = self.egui_ctx.run_ui(raw_input, |ui| build_ui(ui));
        // Text-edit focus only: plugin viewports focus on any press, and any-widget focus
        // would raise the soft keyboard for plain taps. Plugins use Host::request_keyboard.
        self.wants_keyboard = self.egui_ctx.text_edit_focused();

        for cmd in &full_output.platform_output.commands {
            match cmd {
                egui::OutputCommand::OpenUrl(o) => self.pending_open_url = Some(o.url.clone()),
                egui::OutputCommand::CopyText(t) => self.pending_copy = Some(t.clone()),
                _ => {}
            }
        }

        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        for (id, delta) in &full_output.textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }

        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.width_px, self.height_px],
            pixels_per_point: self.pixels_per_point,
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("egui-ios"),
            });
        let user_buffers =
            self.renderer
                .update_buffers(&self.device, &self.queue, &mut encoder, &paint_jobs, &screen);

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(f)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                    _ => return,
                }
            }
            _ => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui-ios"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            self.renderer.render(&mut pass, &paint_jobs, &screen);
        }

        self.queue
            .submit(user_buffers.into_iter().chain(std::iter::once(encoder.finish())));
        frame.present();

        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }
}
