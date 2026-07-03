//! Paints decoded guest meshes inside the host render pass via a dedicated per-plugin
//! `egui_wgpu::Renderer` stored in the host renderer's callback resources.

use std::collections::HashMap;

use egui::epaint;
use egui_wgpu::{CallbackResources, CallbackTrait, ScreenDescriptor};

/// Stored in `Renderer::callback_resources` by [`install`].
pub(crate) struct PluginPaintResources {
    format: wgpu::TextureFormat,
    msaa_samples: u32,
    renderers: HashMap<u64, egui_wgpu::Renderer>,
}

/// One-time host setup: give plugin paint callbacks a renderer factory matching the host
/// surface. `msaa_samples` must match the host render pass (1 unless you enable MSAA).
pub fn install(renderer: &mut egui_wgpu::Renderer, format: wgpu::TextureFormat, msaa_samples: u32) {
    renderer.callback_resources.insert(PluginPaintResources {
        format,
        msaa_samples,
        renderers: HashMap::new(),
    });
}

/// Per-frame paint payload for one plugin viewport. Primitives are in host space (points).
pub(crate) struct PluginPaint {
    pub key: u64,
    pub primitives: Vec<epaint::ClippedPrimitive>,
    pub textures_set: Vec<(epaint::TextureId, epaint::ImageDelta)>,
    pub textures_free: Vec<epaint::TextureId>,
    /// Instance keys retired by hot reloads; their GPU resources are dropped here.
    pub retired_keys: Vec<u64>,
}

impl CallbackTrait for PluginPaint {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_descriptor: &ScreenDescriptor,
        egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let res: &mut PluginPaintResources = callback_resources
            .get_mut()
            .expect("egui_ios_plugin_host::install() was not called on this renderer");
        for key in &self.retired_keys {
            res.renderers.remove(key);
        }
        let renderer = res.renderers.entry(self.key).or_insert_with(|| {
            egui_wgpu::Renderer::new(
                device,
                res.format,
                egui_wgpu::RendererOptions {
                    msaa_samples: res.msaa_samples,
                    depth_stencil_format: None,
                    dithering: true,
                    predictable_texture_filtering: false,
                },
            )
        });
        for (id, delta) in &self.textures_set {
            renderer.update_texture(device, queue, *id, delta);
        }
        for id in &self.textures_free {
            renderer.free_texture(id);
        }
        renderer.update_buffers(device, queue, egui_encoder, &self.primitives, screen_descriptor)
    }

    fn paint(
        &self,
        info: epaint::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &CallbackResources,
    ) {
        let Some(res) = callback_resources.get::<PluginPaintResources>() else {
            return;
        };
        let Some(renderer) = res.renderers.get(&self.key) else {
            return;
        };
        let screen = ScreenDescriptor {
            size_in_pixels: info.screen_size_px,
            pixels_per_point: info.pixels_per_point,
        };
        renderer.render(render_pass, &self.primitives, &screen);
    }
}
