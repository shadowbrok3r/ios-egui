//! The egui widget that hosts one plugin: allocates a rect, translates input into the
//! guest's local space, runs the guest frame, and paints the returned meshes.

use egui::{Event, Pos2, Rect, Sense, Vec2};
use egui_ios_plugin_abi as abi;

use crate::paint::PluginPaint;
use crate::plugin::{LoadedPlugin, PluginStatus};

pub struct PluginViewport {
    pub min_size: Vec2,
}

impl Default for PluginViewport {
    fn default() -> Self {
        PluginViewport { min_size: Vec2::new(64.0, 64.0) }
    }
}

pub struct PluginViewportResponse {
    pub response: egui::Response,
    /// Events the plugin emitted for the embedding app this frame.
    pub events: Vec<abi::PluginEvent>,
    /// The plugin has a focused text field; on iOS bridge this to `Host::request_keyboard`.
    pub wants_keyboard: bool,
    pub error: Option<String>,
}

impl PluginViewport {
    /// Fill the available space with the plugin's UI. `retired_keys` carries instance keys
    /// whose GPU resources should be dropped (from hot reloads); it is drained only when a
    /// paint callback is actually issued, so pending keys survive error/disabled frames.
    pub fn show(
        &self,
        ui: &mut egui::Ui,
        plugin: &mut LoadedPlugin,
        retired_keys: &mut Vec<u64>,
    ) -> PluginViewportResponse {
        let size = ui.available_size().max(self.min_size);
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());

        if let PluginStatus::Errored(msg) = plugin.status.clone() {
            self.error_panel(ui, rect, &msg);
            return PluginViewportResponse {
                response,
                events: Vec::new(),
                wants_keyboard: false,
                error: Some(msg),
            };
        }
        if !plugin.enabled {
            ui.painter().rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("{} (disabled)", plugin.manifest.name),
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().weak_text_color(),
            );
            return PluginViewportResponse {
                response,
                events: Vec::new(),
                wants_keyboard: false,
                error: None,
            };
        }

        // Acquire focus on any pointer press inside the rect, not just a full click: a
        // drag-first gesture (grab-scroll, drag-to-select) otherwise leaves the guest's text
        // field focused and the keyboard raised while Key/Text events stay gated out.
        let pressed_inside = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(e, Event::PointerButton { pos, pressed: true, .. } if rect.contains(*pos))
            })
        });
        if response.clicked() || pressed_inside {
            response.request_focus();
        }

        let focused = response.has_focus();
        let hovered = response.hovered();

        // While focused, lock Tab/arrows/Escape to this widget so egui's end-of-pass
        // focus traversal never acts on keys meant for the guest.
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }

        let raw_input = self.gather_input(ui, plugin, rect, focused, hovered);
        let result = plugin.run_frame(&abi::FrameInput { raw_input });

        match result {
            Ok(mut frame) => {
                let off = rect.min.to_vec2();
                for cp in &mut frame.primitives {
                    cp.clip_rect = cp.clip_rect.translate(off).intersect(rect);
                    if let egui::epaint::Primitive::Mesh(mesh) = &mut cp.primitive {
                        for v in &mut mesh.vertices {
                            v.pos += off;
                        }
                    }
                }
                ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                    rect,
                    PluginPaint {
                        key: plugin.instance_key,
                        primitives: frame.primitives,
                        textures_set: frame.textures_set,
                        textures_free: frame.textures_free,
                        retired_keys: std::mem::take(retired_keys),
                    },
                ));

                if let Some(secs) = frame.platform.repaint_delay_secs {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs_f64(secs.max(0.0)));
                }
                if hovered
                    && let Some(cursor) = frame.platform.cursor_icon
                {
                    ui.ctx().output_mut(|o| o.cursor_icon = cursor);
                }
                if let Some(url) = frame.platform.open_url.take()
                    && plugin.manifest.allows("url.open")
                {
                    ui.ctx().open_url(egui::OpenUrl::same_tab(url));
                }
                if let Some(text) = frame.platform.copy_text.take()
                    && plugin.manifest.allows("clipboard.set")
                {
                    ui.ctx().copy_text(text);
                }
                if frame.skipped_callbacks > 0 {
                    log::warn!(
                        "plugin {}: {} paint callbacks cannot cross the wasm boundary and were skipped",
                        plugin.manifest.id,
                        frame.skipped_callbacks
                    );
                }

                PluginViewportResponse {
                    response,
                    events: frame.platform.events,
                    wants_keyboard: frame.platform.wants_keyboard,
                    error: None,
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.error_panel(ui, rect, &msg);
                PluginViewportResponse {
                    response,
                    events: Vec::new(),
                    wants_keyboard: false,
                    error: Some(msg),
                }
            }
        }
    }

    fn gather_input(
        &self,
        ui: &egui::Ui,
        plugin: &LoadedPlugin,
        rect: Rect,
        focused: bool,
        hovered: bool,
    ) -> egui::RawInput {
        let capture_id = egui::Id::new(("egui_ios_plugin_capture", plugin.instance_key));
        let mut captured: bool = ui.ctx().data(|d| d.get_temp(capture_id)).unwrap_or(false);

        let host_raw = ui.input(|i| i.raw.clone());
        let time = ui.input(|i| i.time);
        let off = rect.min.to_vec2();
        let translate = |p: Pos2| p - off;

        let mut events = Vec::new();
        for ev in &host_raw.events {
            match ev {
                Event::PointerMoved(pos) => {
                    if rect.contains(*pos) || captured {
                        events.push(Event::PointerMoved(translate(*pos)));
                    }
                }
                Event::PointerButton { pos, button, pressed, modifiers } => {
                    let inside = rect.contains(*pos);
                    if *pressed && inside {
                        captured = true;
                    }
                    if inside || captured {
                        events.push(Event::PointerButton {
                            pos: translate(*pos),
                            button: *button,
                            pressed: *pressed,
                            modifiers: *modifiers,
                        });
                    }
                    if !*pressed {
                        captured = false;
                    }
                }
                Event::PointerGone => {
                    captured = false;
                    events.push(Event::PointerGone);
                }
                Event::Touch { device_id, id, phase, pos, force } => {
                    if rect.contains(*pos) || captured {
                        events.push(Event::Touch {
                            device_id: *device_id,
                            id: *id,
                            phase: *phase,
                            pos: translate(*pos),
                            force: *force,
                        });
                    }
                }
                Event::MouseWheel { .. } | Event::Zoom(_) => {
                    if hovered {
                        events.push(ev.clone());
                    }
                }
                Event::Key { .. } | Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_) | Event::Ime(_)
                    if focused =>
                {
                    events.push(ev.clone());
                }
                _ => {}
            }
        }
        ui.ctx().data_mut(|d| d.insert_temp(capture_id, captured));

        let local_rect = Rect::from_min_size(Pos2::ZERO, rect.size());
        let ppp = ui.ctx().pixels_per_point();
        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                native_pixels_per_point: Some(ppp),
                inner_rect: Some(local_rect),
                outer_rect: Some(local_rect),
                focused: Some(focused),
                ..Default::default()
            },
        );

        egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            viewports,
            screen_rect: Some(local_rect),
            max_texture_side: host_raw.max_texture_side,
            time: Some(time),
            predicted_dt: host_raw.predicted_dt,
            modifiers: host_raw.modifiers,
            events,
            focused: host_raw.focused,
            ..Default::default()
        }
    }

    fn error_panel(&self, ui: &egui::Ui, rect: Rect, msg: &str) {
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(40, 8, 8));
        painter.text(
            rect.left_top() + Vec2::new(8.0, 8.0),
            egui::Align2::LEFT_TOP,
            format!("plugin error\n\n{msg}"),
            egui::TextStyle::Monospace.resolve(ui.style()),
            egui::Color32::from_rgb(255, 160, 160),
        );
    }
}
