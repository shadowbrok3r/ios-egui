//! Wire format shared by the plugin host (`egui-ios-plugin-host`) and WASM guests
//! (`egui-ios-plugin-sdk`). Guests run a full egui pass and ship tessellated primitives
//! plus texture deltas back; hosts ship translated `egui::RawInput` in.
//!
//! All multi-byte payloads are little-endian (wasm32 and every supported host are LE).

use egui::epaint::{self, ClippedPrimitive, ImageData, ImageDelta, Primitive};
use serde::{Deserialize, Serialize};

pub mod net;
pub mod theme;

/// Bumped on any breaking change to exports, imports, or wire types.
pub const ABI_VERSION: u32 = 1;

/// egui minor version whose serde encoding rides the wire (`RawInput`, `TextureOptions`, …).
/// Host and guest must agree; bump alongside workspace egui upgrades.
pub const WIRE_FORMAT: u32 = 35;

/// Import module name the guest links host functions from.
pub const HOST_MODULE: &str = "egui_plugin_host";

// -------------------------------------------------------------------------------------------
// Handshake
// -------------------------------------------------------------------------------------------

/// Sent to `plugin_create` when a plugin is instantiated.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateConfig {
    pub abi_version: u32,
    /// Host's [`WIRE_FORMAT`]; the guest refuses to start on mismatch.
    #[serde(default)]
    pub wire_format: u32,
    /// Initial pixels-per-point of the hosting surface.
    pub pixels_per_point: f32,
    /// True if the host UI uses dark visuals.
    pub dark_mode: bool,
    /// Host identifier, e.g. `"ios"` or `"desktop"`.
    pub host_name: String,
}

/// Plugin metadata, stored as `manifest.toml` next to `plugin.wasm`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Stable identifier, e.g. `com.example.clock`. Also the install directory name.
    pub id: String,
    pub name: String,
    pub version: String,
    pub abi_version: u32,
    #[serde(default)]
    pub description: String,
    /// Host ops the plugin may call. A permission grants the op with the same name
    /// and any op under it, e.g. `net` grants `net.tcp.connect`.
    #[serde(default)]
    pub permissions: Vec<String>,
}

impl PluginManifest {
    /// Whether `op` is covered by the declared permissions.
    pub fn allows(&self, op: &str) -> bool {
        self.permissions.iter().any(|p| {
            p == "*" || p == op || (op.len() > p.len() && op.starts_with(p.as_str()) && op.as_bytes()[p.len()] == b'.')
        })
    }
}

// -------------------------------------------------------------------------------------------
// Per-frame input (host → guest)
// -------------------------------------------------------------------------------------------

/// One frame of input. `raw_input` is already translated into plugin-local coordinates
/// with `screen_rect` at the origin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameInput {
    pub raw_input: egui::RawInput,
}

// -------------------------------------------------------------------------------------------
// Per-frame output (guest → host)
// -------------------------------------------------------------------------------------------

/// `egui::TextureId` packed for the wire: `Managed(n)` → `n << 1`, `User(n)` → `n << 1 | 1`.
pub type WireTextureId = u64;

pub fn texture_id_to_wire(id: epaint::TextureId) -> WireTextureId {
    match id {
        epaint::TextureId::Managed(n) => n << 1,
        epaint::TextureId::User(n) => (n << 1) | 1,
    }
}

pub fn texture_id_from_wire(id: WireTextureId) -> epaint::TextureId {
    if id & 1 == 0 {
        epaint::TextureId::Managed(id >> 1)
    } else {
        epaint::TextureId::User(id >> 1)
    }
}

/// One tessellated mesh, clipped. Positions and clip rect are in plugin-local points.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WirePrimitive {
    /// `min.x, min.y, max.x, max.y`.
    pub clip_rect: [f32; 4],
    pub texture_id: WireTextureId,
    /// `epaint::Vertex` (pos, uv, color — 20 bytes each), raw LE bytes.
    #[serde(with = "serde_bytes")]
    pub vertices: Vec<u8>,
    /// `u32` triangle indices, raw LE bytes.
    #[serde(with = "serde_bytes")]
    pub indices: Vec<u8>,
}

/// A texture allocation or partial update.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireTextureSet {
    pub id: WireTextureId,
    /// Top-left offset for partial updates; `None` allocates/replaces the whole texture.
    pub pos: Option<[u32; 2]>,
    pub size: [u32; 2],
    /// Logical size in points (`ColorImage::source_size`).
    pub source_size: [f32; 2],
    /// RGBA8 premultiplied, row-major.
    #[serde(with = "serde_bytes")]
    pub pixels: Vec<u8>,
    pub options: egui::TextureOptions,
}

/// An event a plugin emits for the embedding app.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginEvent {
    pub topic: String,
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
}

/// Everything a frame produced besides pixels.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WirePlatform {
    /// Seconds until the plugin wants a repaint; `None` = only on new input.
    pub repaint_delay_secs: Option<f64>,
    pub wants_keyboard: bool,
    pub wants_pointer: bool,
    pub cursor_icon: Option<egui::CursorIcon>,
    pub open_url: Option<String>,
    pub copy_text: Option<String>,
    pub events: Vec<PluginEvent>,
}

/// One frame of output.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FrameOutput {
    pub primitives: Vec<WirePrimitive>,
    pub textures_set: Vec<WireTextureSet>,
    pub textures_free: Vec<WireTextureId>,
    pub platform: WirePlatform,
    /// Paint callbacks cannot cross the wasm boundary; count of dropped ones for diagnostics.
    pub skipped_callbacks: u32,
}

// -------------------------------------------------------------------------------------------
// Host calls (guest → host, synchronous)
// -------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostCallRequest {
    pub op: String,
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostCallResponse {
    Ok(#[serde(with = "serde_bytes")] Vec<u8>),
    Err(String),
    /// The op is not covered by the plugin's manifest permissions.
    Denied,
}

// -------------------------------------------------------------------------------------------
// epaint ↔ wire conversion
// -------------------------------------------------------------------------------------------

/// Convert tessellated primitives to wire form. Returns the number of skipped paint callbacks.
pub fn primitives_to_wire(primitives: &[ClippedPrimitive]) -> (Vec<WirePrimitive>, u32) {
    let mut out = Vec::with_capacity(primitives.len());
    let mut skipped = 0u32;
    for cp in primitives {
        match &cp.primitive {
            Primitive::Mesh(mesh) => out.push(WirePrimitive {
                clip_rect: [
                    cp.clip_rect.min.x,
                    cp.clip_rect.min.y,
                    cp.clip_rect.max.x,
                    cp.clip_rect.max.y,
                ],
                texture_id: texture_id_to_wire(mesh.texture_id),
                vertices: bytemuck::cast_slice(&mesh.vertices).to_vec(),
                indices: bytemuck::cast_slice(&mesh.indices).to_vec(),
            }),
            Primitive::Callback(_) => skipped += 1,
        }
    }
    (out, skipped)
}

/// Upper bound on a texture side accepted from a guest; also bounds `size[0] * size[1]`.
pub const MAX_TEXTURE_SIDE: u32 = 16384;

/// Rebuild an epaint mesh from a wire primitive, validating it before it can reach the GPU.
/// Returns `None` for a hostile/corrupt primitive: any index out of range of the vertex
/// count, a non-finite clip rect, or a vertex/index byte buffer of the wrong stride. Callers
/// (the host) MUST drop `None`; egui-wgpu does not CPU-scan index values, so an unchecked
/// mesh would issue an out-of-bounds `draw_indexed`.
pub fn wire_to_primitive(wp: &WirePrimitive) -> Option<ClippedPrimitive> {
    if !wp.vertices.len().is_multiple_of(std::mem::size_of::<epaint::Vertex>())
        || !wp.indices.len().is_multiple_of(std::mem::size_of::<u32>())
    {
        return None;
    }
    let vertices: Vec<epaint::Vertex> = bytemuck::pod_collect_to_vec(&wp.vertices);
    let indices: Vec<u32> = bytemuck::pod_collect_to_vec(&wp.indices);
    let vcount = vertices.len() as u32;
    if indices.iter().any(|&i| i >= vcount) {
        return None;
    }
    if !wp.clip_rect.iter().all(|c| c.is_finite()) {
        return None;
    }
    Some(ClippedPrimitive {
        clip_rect: egui::Rect::from_min_max(
            egui::pos2(wp.clip_rect[0], wp.clip_rect[1]),
            egui::pos2(wp.clip_rect[2], wp.clip_rect[3]),
        ),
        primitive: Primitive::Mesh(epaint::Mesh {
            indices,
            vertices,
            texture_id: texture_id_from_wire(wp.texture_id),
        }),
    })
}

/// Convert a texture delta to wire form (pixels expanded to RGBA8).
pub fn textures_delta_to_wire(delta: &epaint::textures::TexturesDelta) -> (Vec<WireTextureSet>, Vec<WireTextureId>) {
    let mut set = Vec::with_capacity(delta.set.len());
    for (id, image_delta) in &delta.set {
        let (size, source_size, pixels) = image_data_to_rgba(&image_delta.image);
        set.push(WireTextureSet {
            id: texture_id_to_wire(*id),
            pos: image_delta.pos.map(|p| [p[0] as u32, p[1] as u32]),
            size,
            source_size,
            pixels,
            options: image_delta.options,
        });
    }
    let free = delta.free.iter().map(|id| texture_id_to_wire(*id)).collect();
    (set, free)
}

/// Rebuild an `ImageDelta` from the wire (with the *wire* texture id, unmapped), validating
/// it against the pixel buffer. Returns `None` if `size` exceeds [`MAX_TEXTURE_SIDE`] or the
/// pixel buffer is not exactly `4 * w * h` bytes — otherwise `egui_wgpu::Renderer::update_texture`
/// would assert or `write_texture` would fault the host render thread. Partial-update bounds
/// (`pos` within the target extent) are checked by the host, which knows the existing size.
pub fn wire_to_image_delta(ts: &WireTextureSet) -> Option<ImageDelta> {
    let (w, h) = (ts.size[0], ts.size[1]);
    if w > MAX_TEXTURE_SIDE || h > MAX_TEXTURE_SIDE {
        return None;
    }
    let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if ts.pixels.len() != expected {
        return None;
    }
    let pixels: Vec<egui::Color32> = ts
        .pixels
        .chunks_exact(4)
        .map(|c| egui::Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3]))
        .collect();
    let image = egui::ColorImage {
        size: [w as usize, h as usize],
        pixels,
        source_size: egui::vec2(ts.source_size[0], ts.source_size[1]),
    };
    Some(ImageDelta {
        image: ImageData::Color(std::sync::Arc::new(image)),
        options: ts.options,
        pos: ts.pos.map(|p| [p[0] as usize, p[1] as usize]),
    })
}

fn image_data_to_rgba(image: &ImageData) -> ([u32; 2], [f32; 2], Vec<u8>) {
    match image {
        ImageData::Color(c) => {
            let size = [c.size[0] as u32, c.size[1] as u32];
            let source_size = [c.source_size.x, c.source_size.y];
            let mut bytes = Vec::with_capacity(c.pixels.len() * 4);
            for p in &c.pixels {
                bytes.extend_from_slice(&p.to_array());
            }
            (size, source_size, bytes)
        }
    }
}

// -------------------------------------------------------------------------------------------
// Buffer helpers
// -------------------------------------------------------------------------------------------

/// Pack a guest pointer + length into the u64 returned by guest exports.
pub fn pack_ptr_len(ptr: u32, len: u32) -> u64 {
    ((ptr as u64) << 32) | len as u64
}

pub fn unpack_ptr_len(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_stdvec(value).expect("postcard encode")
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_id_roundtrip() {
        for id in [
            epaint::TextureId::Managed(0),
            epaint::TextureId::Managed(7),
            epaint::TextureId::User(0),
            epaint::TextureId::User(u64::MAX >> 1),
        ] {
            assert_eq!(texture_id_from_wire(texture_id_to_wire(id)), id);
        }
    }

    #[test]
    fn primitive_roundtrip() {
        let mesh = epaint::Mesh {
            indices: vec![0, 1, 2],
            vertices: vec![
                epaint::Vertex { pos: egui::pos2(0.0, 0.0), uv: egui::pos2(0.0, 0.0), color: egui::Color32::RED },
                epaint::Vertex { pos: egui::pos2(10.0, 0.0), uv: egui::pos2(1.0, 0.0), color: egui::Color32::GREEN },
                epaint::Vertex { pos: egui::pos2(0.0, 10.0), uv: egui::pos2(0.0, 1.0), color: egui::Color32::BLUE },
            ],
            texture_id: epaint::TextureId::Managed(0),
        };
        let cp = ClippedPrimitive {
            clip_rect: egui::Rect::from_min_max(egui::pos2(1.0, 2.0), egui::pos2(3.0, 4.0)),
            primitive: Primitive::Mesh(mesh.clone()),
        };
        let (wire, skipped) = primitives_to_wire(std::slice::from_ref(&cp));
        assert_eq!(skipped, 0);
        let back = wire_to_primitive(&wire[0]).expect("valid primitive");
        assert_eq!(back.clip_rect, cp.clip_rect);
        match back.primitive {
            Primitive::Mesh(m) => {
                assert_eq!(m.indices, mesh.indices);
                assert_eq!(m.vertices.len(), mesh.vertices.len());
                assert_eq!(m.vertices[1].color, mesh.vertices[1].color);
                assert_eq!(m.texture_id, mesh.texture_id);
            }
            _ => panic!("expected mesh"),
        }
    }

    #[test]
    fn rejects_out_of_range_indices() {
        // 1 vertex (20 bytes), index 5 → out of range.
        let vertex = epaint::Vertex {
            pos: egui::pos2(0.0, 0.0),
            uv: egui::pos2(0.0, 0.0),
            color: egui::Color32::WHITE,
        };
        let wp = WirePrimitive {
            clip_rect: [0.0, 0.0, 1.0, 1.0],
            texture_id: 0,
            vertices: bytemuck::bytes_of(&vertex).to_vec(),
            indices: bytemuck::cast_slice(&[0u32, 5, 0]).to_vec(),
        };
        assert!(wire_to_primitive(&wp).is_none());
    }

    #[test]
    fn rejects_mismatched_texture() {
        // Claims 4x4 but ships only 4 bytes.
        let ts = WireTextureSet {
            id: 0,
            pos: None,
            size: [4, 4],
            source_size: [4.0, 4.0],
            pixels: vec![0u8; 4],
            options: egui::TextureOptions::LINEAR,
        };
        assert!(wire_to_image_delta(&ts).is_none());

        // Oversized side is rejected even with a plausible-looking length claim.
        let huge = WireTextureSet {
            id: 0,
            pos: None,
            size: [MAX_TEXTURE_SIDE + 1, 1],
            source_size: [1.0, 1.0],
            pixels: vec![],
            options: egui::TextureOptions::LINEAR,
        };
        assert!(wire_to_image_delta(&huge).is_none());
    }

    #[test]
    fn frame_output_encode_roundtrip() {
        let out = FrameOutput {
            primitives: vec![WirePrimitive {
                clip_rect: [0.0, 0.0, 100.0, 100.0],
                texture_id: 0,
                vertices: vec![1, 2, 3, 4],
                indices: vec![0; 12],
            }],
            textures_set: vec![WireTextureSet {
                id: 0,
                pos: None,
                size: [2, 1],
                source_size: [2.0, 1.0],
                pixels: vec![255; 8],
                options: egui::TextureOptions::LINEAR,
            }],
            textures_free: vec![3],
            platform: WirePlatform {
                repaint_delay_secs: Some(0.5),
                wants_keyboard: true,
                events: vec![PluginEvent { topic: "t".into(), payload: vec![9] }],
                ..Default::default()
            },
            skipped_callbacks: 0,
        };
        let bytes = encode(&out);
        let back: FrameOutput = decode(&bytes).unwrap();
        assert_eq!(back.primitives[0].vertices, out.primitives[0].vertices);
        assert_eq!(back.textures_set[0].size, [2, 1]);
        assert_eq!(back.platform.events[0].topic, "t");
        assert_eq!(back.platform.repaint_delay_secs, Some(0.5));
    }

    #[test]
    fn manifest_permissions() {
        let m = PluginManifest {
            id: "t".into(),
            name: "t".into(),
            version: "0".into(),
            abi_version: ABI_VERSION,
            description: String::new(),
            permissions: vec!["haptic".into(), "net".into()],
        };
        assert!(m.allows("haptic"));
        assert!(m.allows("net.tcp.connect"));
        assert!(!m.allows("notify"));
        assert!(!m.allows("network"));
    }

    #[test]
    fn ptr_len_pack() {
        let (p, l) = unpack_ptr_len(pack_ptr_len(0xDEAD_BEEF, 42));
        assert_eq!((p, l), (0xDEAD_BEEF, 42));
    }
}
