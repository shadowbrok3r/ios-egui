//! GPU mesh renderer, ported from the desktop's `viewport.rs` to OpenGL ES 3.0.
//!
//! Four things differ from the desktop version, all forced by GLES:
//!
//! - **No `glPolygonMode`, no `GL_POLYGON_OFFSET_LINE`.** Neither exists in GLES at any version, and
//!   glow does not degrade gracefully — a missing entry point panics. The desktop called
//!   `polygon_mode(FILL)` unconditionally on the normal draw path, so this is fatal on frame 1 even
//!   with the wireframe off.
//! - **The wireframe is barycentric and single-pass.** The corner index comes from
//!   `gl_VertexID % 3`, which is free because the mesh is non-indexed, so it needs no extra
//!   attribute, no extra buffer and no second draw call. `glLineWidth` was never an option:
//!   `ALIASED_LINE_WIDTH_RANGE` is [1, 1] on Adreno and Mali.
//! - **The `#version` header is chosen at runtime.** `330 core` on desktop, `300 es` on GLES — not
//!   the `140` the in-tree backdrop-blur precedent uses, because all three attributes here are
//!   declared `layout(location = N)`, which needs GL 3.3. `precision highp float` is mandatory
//!   rather than stylistic: GLSL ES 3.00 defines no default float precision for fragment shaders,
//!   and the `pow(..., 58.0)` specular term needs `highp` regardless.
//! - **Shader failure is recoverable.** The desktop panicked, which on a device is a process kill
//!   with the message only in logcat.

use egui_glow::glow;
use glow::HasContext;

use ringdesign_core::castability::CastReport;
use ringdesign_core::mesh::{Mesh, Vec3};

/// Floats per vertex: position(3), normal(3), draft colour(3), wall colour(3).
const FLOATS_PER_VERTEX: usize = 12;

/// Wireframe line half-width, in fragments.
const WIRE_PX: f32 = 0.6;

const VERTEX_BODY: &str = r#"
layout(location = 0) in vec3 a_position;
layout(location = 1) in vec3 a_normal;
layout(location = 2) in vec3 a_color;
layout(location = 3) in vec3 a_wall;

uniform mat4 u_mvp;
uniform mat3 u_normal_matrix;

out vec3 v_normal;
out vec3 v_color;
out vec3 v_wall;
out vec3 v_bary;
out float v_obj_nz;

void main() {
    gl_Position = u_mvp * vec4(a_position, 1.0);
    v_normal = u_normal_matrix * a_normal;
    v_color = a_color;
    v_wall = a_wall;
    v_obj_nz = a_normal.z;
    // Non-indexed triangles, so the corner index is the vertex index mod 3 and the
    // barycentric coordinate costs nothing to carry.
    int corner = gl_VertexID % 3;
    v_bary = vec3(corner == 0 ? 1.0 : 0.0, corner == 1 ? 1.0 : 0.0, corner == 2 ? 1.0 : 0.0);
}
"#;

const FRAGMENT_BODY: &str = r#"
in vec3 v_normal;
in vec3 v_color;
in vec3 v_wall;
in vec3 v_bary;
in float v_obj_nz;

uniform int u_mode;
uniform vec3 u_light_dir;
uniform vec3 u_base_color;
uniform float u_ambient;
uniform vec3 u_wire_color;
uniform float u_wire_px;

out vec4 frag_color;

const vec3 FILL_DIR = vec3(-0.52, -0.38, 0.42);
const vec3 HIGHLIGHT = vec3(1.0, 0.96, 0.88);

void main() {
    vec3 n = normalize(v_normal);
    vec3 eye = vec3(0.0, 0.0, 1.0);
    vec3 l = normalize(u_light_dir);
    vec3 color;

    if (u_mode == 4) {
        float lambert = max(dot(n, l), 0.0);
        vec3 half_c = v_obj_nz > 0.0 ? vec3(0.42, 0.62, 0.82) : vec3(0.80, 0.62, 0.38);
        float band = 1.0 - smoothstep(0.035, 0.09, abs(v_obj_nz));
        color = mix(half_c, vec3(1.0, 0.92, 0.25), band) * (0.72 + 0.28 * lambert);
    } else if (u_mode == 3) {
        float lambert = max(dot(n, l), 0.0);
        color = v_wall * (0.74 + 0.26 * lambert);
    } else if (u_mode == 2) {
        color = n * 0.5 + 0.5;
    } else if (u_mode == 1) {
        float lambert = max(dot(n, l), 0.0);
        color = v_color * (0.74 + 0.26 * lambert);
    } else {
        float key = pow(dot(n, l) * 0.5 + 0.5, 1.7);
        float fill = max(dot(n, normalize(FILL_DIR)), 0.0) * 0.22;
        vec3 h = normalize(l + eye);
        float spec = pow(max(dot(n, h), 0.0), 58.0) * 0.85;
        float rim = pow(1.0 - max(dot(n, eye), 0.0), 3.5) * 0.30;
        color = u_base_color * (u_ambient + (1.0 - u_ambient) * key + fill + rim)
              + HIGHLIGHT * spec;
    }

    if (u_wire_px > 0.0) {
        vec3 w = fwidth(v_bary) * u_wire_px;
        vec3 a = smoothstep(vec3(0.0), w, v_bary);
        float edge = 1.0 - min(min(a.x, a.y), a.z);
        color = mix(color, u_wire_color, edge * 0.55);
    }

    frag_color = vec4(color, 1.0);
}
"#;

/// `330 core` on desktop GL, `300 es` on GLES. Classified from the driver's version string the way
/// backdrop-blur's `profile.rs` does it, rather than from a build-time cfg, so one binary is right
/// either way.
fn shader_header(gl: &glow::Context) -> &'static str {
    let version = unsafe { gl.get_parameter_string(glow::VERSION) };
    if version.contains("OpenGL ES") || version.contains("WebGL") {
        "#version 300 es\nprecision highp float;\nprecision highp int;\n"
    } else {
        "#version 330 core\n"
    }
}

/// Uniform locations, resolved once at link time. On mobile drivers
/// `glGetUniformLocation` is a real string comparison, and the desktop did seven of them per frame.
struct Uniforms {
    mvp: Option<glow::NativeUniformLocation>,
    normal_matrix: Option<glow::NativeUniformLocation>,
    mode: Option<glow::NativeUniformLocation>,
    light_dir: Option<glow::NativeUniformLocation>,
    base_color: Option<glow::NativeUniformLocation>,
    ambient: Option<glow::NativeUniformLocation>,
    wire_color: Option<glow::NativeUniformLocation>,
    wire_px: Option<glow::NativeUniformLocation>,
}

struct GpuResources {
    program: glow::NativeProgram,
    vao: glow::NativeVertexArray,
    vbo: glow::NativeBuffer,
    gem_vao: glow::NativeVertexArray,
    gem_vbo: glow::NativeBuffer,
    uniforms: Uniforms,
}

#[derive(Default)]
pub struct GpuMeshRenderer {
    resources: Option<GpuResources>,
    vertex_count: i32,
    gem_count: i32,
    pending: Option<Vec<f32>>,
    pending_gems: Option<Vec<f32>>,
    depth_checked: bool,
    /// Set once if the shaders will not build, so the pane can say so instead of drawing nothing.
    pub failed: Option<String>,
}

// glow handles are u32 integers on native, safe to send across threads.
unsafe impl Send for GpuMeshRenderer {}
unsafe impl Sync for GpuMeshRenderer {}

impl GpuMeshRenderer {
    /// Flatten the mesh into an interleaved vertex buffer awaiting upload. Runs on the build
    /// worker, not the UI thread — at Preview resolution this fills ~12 MB, which is more than a
    /// frame's budget.
    /// `wall` is `(inner_radius_mm, min_section_mm)`, baked alongside the
    /// draft colours so switching shade modes never re-uploads.
    pub fn stage(mesh: &Mesh, cast: Option<&CastReport>, wall: (f64, f64)) -> Vec<f32> {
        let (inner_r, min_section) = wall;
        let mut data: Vec<f32> = Vec::with_capacity(mesh.faces.len() * 3 * FLOATS_PER_VERTEX);

        'faces: for (i, face) in mesh.faces.iter().enumerate() {
            let rgb = match cast {
                Some(c) => c.classes.get(i).map_or([1.0; 3], |k| k.rgb()),
                None => [1.0; 3],
            };
            let mut tri = [[0.0f32; FLOATS_PER_VERTEX]; 3];
            for (k, &vi) in face.iter().enumerate() {
                let Some(p) = mesh.vertices.get(vi as usize).filter(|p| p.is_finite()) else {
                    continue 'faces;
                };
                let n = match mesh.normals.get(vi as usize) {
                    Some(n) if n.is_finite() => *n,
                    _ => Vec3(0.0, 0.0, 1.0),
                };
                // Radial metal under this vertex; the bore itself (facing
                // inward) is not a wall and sits out in neutral grey.
                let r = (p.0 as f64).hypot(p.1 as f64);
                let inward = (n.0 as f64 * p.0 as f64 + n.1 as f64 * p.1 as f64) < 0.0;
                let w = if inward {
                    WALL_NEUTRAL
                } else {
                    wall_color(r - inner_r, min_section)
                };
                tri[k] = [
                    p.0, p.1, p.2, n.0, n.1, n.2, rgb[0], rgb[1], rgb[2], w[0], w[1], w[2],
                ];
            }
            for v in &tri {
                data.extend_from_slice(v);
            }
        }
        data
    }

    /// Hand the renderer a buffer built by [`stage`](Self::stage) on the worker.
    pub fn set_pending(&mut self, verts: Vec<f32>) {
        self.pending = Some(verts);
    }

    /// Queue stone-preview triangles in the same layout. Empty clears them.
    pub fn set_pending_gems(&mut self, verts: Vec<f32>) {
        self.pending_gems = Some(verts);
    }

    pub fn has_mesh(&self) -> bool {
        self.vertex_count > 0 || self.pending.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    fn paint(
        &mut self,
        gl: &glow::Context,
        info: egui::PaintCallbackInfo,
        mvp: &[f32; 16],
        normal_matrix: &[f32; 9],
        mode: i32,
        base_color: [f32; 3],
        wireframe: bool,
        wire_color: [f32; 3],
    ) {
        self.ensure_resources(gl);
        self.warn_if_no_depth_buffer(gl);
        let Some(res) = self.resources.as_ref() else { return };

        if let Some(verts) = self.pending.take() {
            self.vertex_count = (verts.len() / FLOATS_PER_VERTEX) as i32;
            unsafe {
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(res.vbo));
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, as_u8_slice(&verts), glow::STATIC_DRAW);
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
            }
        }
        if let Some(verts) = self.pending_gems.take() {
            self.gem_count = (verts.len() / FLOATS_PER_VERTEX) as i32;
            unsafe {
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(res.gem_vbo));
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, as_u8_slice(&verts), glow::STATIC_DRAW);
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
            }
        }

        if self.vertex_count == 0 {
            return;
        }

        unsafe {
            let vp = info.viewport_in_pixels();
            gl.viewport(vp.left_px, vp.from_bottom_px, vp.width_px, vp.height_px);
            gl.scissor(vp.left_px, vp.from_bottom_px, vp.width_px, vp.height_px);

            gl.enable(glow::DEPTH_TEST);
            gl.depth_mask(true);
            gl.depth_func(glow::LESS);
            gl.enable(glow::CULL_FACE);
            gl.cull_face(glow::BACK);
            gl.enable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.clear(glow::DEPTH_BUFFER_BIT);

            gl.use_program(Some(res.program));
            gl.bind_vertex_array(Some(res.vao));

            let u = &res.uniforms;
            gl.uniform_matrix_4_f32_slice(u.mvp.as_ref(), false, mvp);
            gl.uniform_matrix_3_f32_slice(u.normal_matrix.as_ref(), false, normal_matrix);
            gl.uniform_1_i32(u.mode.as_ref(), mode);
            gl.uniform_3_f32(u.light_dir.as_ref(), -0.38, 0.46, 0.80);
            gl.uniform_3_f32(u.base_color.as_ref(), base_color[0], base_color[1], base_color[2]);
            gl.uniform_1_f32(u.ambient.as_ref(), 0.20);
            gl.uniform_3_f32(u.wire_color.as_ref(), wire_color[0], wire_color[1], wire_color[2]);
            gl.uniform_1_f32(u.wire_px.as_ref(), if wireframe { WIRE_PX } else { 0.0 });

            gl.draw_arrays(glow::TRIANGLES, 0, self.vertex_count);

            // Stones ride in a second buffer with the same program: metal
            // shading, their own tint, whatever the ring's mode is.
            if self.gem_count > 0 {
                gl.uniform_1_i32(u.mode.as_ref(), 0);
                gl.uniform_3_f32(
                    u.base_color.as_ref(),
                    ringdesign_core::gems::GEM_TINT[0],
                    ringdesign_core::gems::GEM_TINT[1],
                    ringdesign_core::gems::GEM_TINT[2],
                );
                gl.bind_vertex_array(Some(res.gem_vao));
                gl.draw_arrays(glow::TRIANGLES, 0, self.gem_count);
            }

            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::SCISSOR_TEST);
        }
    }

    /// Depth testing is silently a no-op on a window with no depth attachment, which reads as a
    /// see-through ring rather than as an error. Checked once.
    ///
    /// The attachment type is queried first: on GLES 3.0, asking for `DEPTH_SIZE` of a `NONE`
    /// attachment raises `GL_INVALID_OPERATION`, and egui_glow's post-callback error check would
    /// then report it every frame.
    fn warn_if_no_depth_buffer(&mut self, gl: &glow::Context) {
        if self.depth_checked {
            return;
        }
        self.depth_checked = true;
        let kind = unsafe {
            gl.get_framebuffer_attachment_parameter_i32(
                glow::FRAMEBUFFER,
                glow::DEPTH,
                glow::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
            )
        };
        let bits = if kind == glow::NONE as i32 {
            0
        } else {
            unsafe {
                gl.get_framebuffer_attachment_parameter_i32(
                    glow::FRAMEBUFFER,
                    glow::DEPTH,
                    glow::FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE,
                )
            }
        };
        if bits <= 0 {
            log::warn!(
                "no depth buffer on the default framebuffer ({bits} bits): the ring will draw \
                 see-through. app! needs its third argument, e.g. app!(App::new, Backend::Glow, 24)."
            );
        } else {
            log::info!("depth buffer: {bits} bits");
        }
    }

    fn ensure_resources(&mut self, gl: &glow::Context) {
        if self.resources.is_some() || self.failed.is_some() {
            return;
        }

        let header = shader_header(gl);
        log::info!("GL_VERSION: {}", unsafe { gl.get_parameter_string(glow::VERSION) });

        let program = match compile_program(
            gl,
            &format!("{header}{VERTEX_BODY}"),
            &format!("{header}{FRAGMENT_BODY}"),
        ) {
            Ok(p) => p,
            Err(e) => {
                log::error!("ring shader: {e}");
                self.failed = Some(e);
                return;
            }
        };

        let (Some(vao), Some(vbo), Some(gem_vao), Some(gem_vbo)) = (
            unsafe { gl.create_vertex_array() }.ok(),
            unsafe { gl.create_buffer() }.ok(),
            unsafe { gl.create_vertex_array() }.ok(),
            unsafe { gl.create_buffer() }.ok(),
        ) else {
            self.failed = Some("could not create VAO/VBO".into());
            return;
        };

        let uniforms = unsafe {
            Uniforms {
                mvp: gl.get_uniform_location(program, "u_mvp"),
                normal_matrix: gl.get_uniform_location(program, "u_normal_matrix"),
                mode: gl.get_uniform_location(program, "u_mode"),
                light_dir: gl.get_uniform_location(program, "u_light_dir"),
                base_color: gl.get_uniform_location(program, "u_base_color"),
                ambient: gl.get_uniform_location(program, "u_ambient"),
                wire_color: gl.get_uniform_location(program, "u_wire_color"),
                wire_px: gl.get_uniform_location(program, "u_wire_px"),
            }
        };

        unsafe {
            let f = std::mem::size_of::<f32>() as i32;
            let stride = FLOATS_PER_VERTEX as i32 * f;
            for (va, vb) in [(vao, vbo), (gem_vao, gem_vbo)] {
                gl.bind_vertex_array(Some(va));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vb));
                for (loc, offset) in [(0, 0), (1, 3 * f), (2, 6 * f), (3, 9 * f)] {
                    gl.enable_vertex_attrib_array(loc);
                    gl.vertex_attrib_pointer_f32(loc, 3, glow::FLOAT, false, stride, offset);
                }
                gl.bind_vertex_array(None);
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
            }
        }

        self.resources = Some(GpuResources { program, vao, vbo, gem_vao, gem_vbo, uniforms });
    }
}

fn compile_program(
    gl: &glow::Context,
    vert_src: &str,
    frag_src: &str,
) -> Result<glow::NativeProgram, String> {
    let program = unsafe { gl.create_program() }?;

    let mut shaders = Vec::with_capacity(2);
    for (kind, src, what) in [
        (glow::VERTEX_SHADER, vert_src, "vertex"),
        (glow::FRAGMENT_SHADER, frag_src, "fragment"),
    ] {
        let shader = unsafe { gl.create_shader(kind) }?;
        unsafe {
            gl.shader_source(shader, src);
            gl.compile_shader(shader);
        }
        if !unsafe { gl.get_shader_compile_status(shader) } {
            let log = unsafe { gl.get_shader_info_log(shader) };
            unsafe { gl.delete_shader(shader) };
            for s in shaders {
                unsafe { gl.delete_shader(s) };
            }
            unsafe { gl.delete_program(program) };
            return Err(format!("{what} shader: {log}"));
        }
        unsafe { gl.attach_shader(program, shader) };
        shaders.push(shader);
    }

    unsafe { gl.link_program(program) };
    if !unsafe { gl.get_program_link_status(program) } {
        let log = unsafe { gl.get_program_info_log(program) };
        unsafe { gl.delete_program(program) };
        return Err(format!("link: {log}"));
    }

    for shader in shaders {
        unsafe {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
    }
    Ok(program)
}

fn as_u8_slice<T: Copy>(data: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data))
    }
}

/// How the solid pane shades the mesh.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShadeMode {
    #[default]
    Metal,
    Draft,
    Wall,
    Halves,
    Normals,
}

impl ShadeMode {
    pub const ALL: &'static [ShadeMode] = &[
        ShadeMode::Metal,
        ShadeMode::Draft,
        ShadeMode::Wall,
        ShadeMode::Halves,
        ShadeMode::Normals,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ShadeMode::Metal => "Metal",
            ShadeMode::Draft => "Draft",
            ShadeMode::Wall => "Wall",
            ShadeMode::Halves => "Halves",
            ShadeMode::Normals => "Normals",
        }
    }

    fn code(self) -> i32 {
        match self {
            ShadeMode::Metal => 0,
            ShadeMode::Draft => 1,
            ShadeMode::Normals => 2,
            ShadeMode::Wall => 3,
            ShadeMode::Halves => 4,
        }
    }
}

/// Wall-heatmap colour for a radial thickness, linear RGB — the desktop's
/// ramp: red at the minimum fill section, amber to twice it, green beyond,
/// easing into blue-grey for comfortably thick metal.
pub fn wall_color(thickness_mm: f64, min_section_mm: f64) -> [f32; 3] {
    let m = min_section_mm.max(0.05);
    let t = (thickness_mm / m).max(0.0);
    let lerp3 = |a: [f32; 3], b: [f32; 3], k: f64| {
        let k = k.clamp(0.0, 1.0) as f32;
        [a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k]
    };
    const RED: [f32; 3] = [0.93, 0.27, 0.36];
    const AMBER: [f32; 3] = [0.95, 0.76, 0.24];
    const GREEN: [f32; 3] = [0.32, 0.78, 0.45];
    const THICK: [f32; 3] = [0.36, 0.55, 0.72];
    if t <= 1.0 {
        RED
    } else if t <= 2.0 {
        lerp3(RED, AMBER, t - 1.0)
    } else if t <= 3.5 {
        lerp3(AMBER, GREEN, (t - 2.0) / 1.5)
    } else {
        lerp3(GREEN, THICK, (t - 3.5) / 2.5)
    }
}

/// Bore and inward faces sit out of the heatmap in a neutral grey.
pub const WALL_NEUTRAL: [f32; 3] = [0.42, 0.42, 0.45];

/// Queue the mesh draw as an egui paint callback covering `rect`.
///
/// Multiple callbacks coexist safely: `egui_glow::Painter` dispatches per primitive and calls
/// `prepare_painting` to restore its own state after each one.
#[allow(clippy::too_many_arguments)]
pub fn paint_callback(
    ui: &egui::Ui,
    rect: egui::Rect,
    renderer: std::sync::Arc<std::sync::Mutex<GpuMeshRenderer>>,
    mvp: [f32; 16],
    normal_matrix: [f32; 9],
    shade: ShadeMode,
    base_color: [f32; 3],
    wireframe: bool,
    wire_color: [f32; 3],
) {
    let cb = egui_glow::CallbackFn::new(move |info, painter| {
        if let Ok(mut r) = renderer.lock() {
            r.paint(
                painter.gl(),
                info,
                &mvp,
                &normal_matrix,
                shade.code(),
                base_color,
                wireframe,
                wire_color,
            );
        }
    });
    ui.painter().add(egui::PaintCallback { rect, callback: std::sync::Arc::new(cb) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shade_modes_have_distinct_codes() {
        let codes: Vec<i32> = ShadeMode::ALL.iter().map(|m| m.code()).collect();
        assert_eq!(codes, vec![0, 1, 3, 4, 2]);
    }

    #[test]
    fn staging_emits_three_vertices_per_face() {
        let mesh = Mesh {
            vertices: vec![Vec3(0.0, 0.0, 0.0), Vec3(1.0, 0.0, 0.0), Vec3(0.0, 1.0, 0.0)],
            normals: vec![Vec3(0.0, 0.0, 1.0); 3],
            faces: vec![[0, 1, 2]],
        };
        let data = GpuMeshRenderer::stage(&mesh, None, (8.5, 0.8));
        assert_eq!(data.len(), 3 * FLOATS_PER_VERTEX);
        // Position of the second vertex.
        assert_eq!(data[FLOATS_PER_VERTEX], 1.0);
        // No cast report means white.
        assert_eq!(data[6], 1.0);
    }

    #[test]
    fn a_face_referencing_a_missing_vertex_is_dropped_not_panicked() {
        let mesh = Mesh {
            vertices: vec![Vec3(0.0, 0.0, 0.0)],
            normals: vec![Vec3(0.0, 0.0, 1.0)],
            faces: vec![[0, 9, 9]],
        };
        assert!(GpuMeshRenderer::stage(&mesh, None, (8.5, 0.8)).is_empty());
    }
}
