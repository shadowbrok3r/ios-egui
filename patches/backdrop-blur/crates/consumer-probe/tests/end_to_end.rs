//! Drives a full egui 0.35 frame through egui_glow 0.35 onto a real surfaceless GL framebuffer and
//! asserts the grab-pass paint callback actually fired, blurred and composited.

use std::sync::Arc;

use backdrop_blur_glow::gl_harness::{headless_gl, read_rgba8};
use consumer_probe::{build_surface, poll, shutdown};
use glow::HasContext as _;

const W: u32 = 320;
const H: u32 = 240;

/// Allocate an RGBA8 color target and bind it as the draw+read framebuffer.
unsafe fn make_target(gl: &glow::Context) -> glow::Framebuffer {
    unsafe {
        let tex = gl.create_texture().expect("texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            W as i32,
            H as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        let fbo = gl.create_framebuffer().expect("fbo");
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE
        );
        gl.viewport(0, 0, W as i32, H as i32);
        fbo
    }
}

/// A hard vertical split: bottom half red, top half blue. A blur across the seam must produce a
/// pixel that is neither pure red nor pure blue.
unsafe fn paint_two_tone(gl: &glow::Context) {
    unsafe {
        gl.disable(glow::SCISSOR_TEST);
        gl.clear_color(1.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.enable(glow::SCISSOR_TEST);
        gl.scissor(0, (H / 2) as i32, W as i32, (H / 2) as i32);
        gl.clear_color(0.0, 0.0, 1.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.disable(glow::SCISSOR_TEST);
    }
}

#[test]
fn grab_pass_composites_a_real_egui_035_frame() {
    let harness = headless_gl();
    let gl: Arc<glow::Context> = harness.context_arc();

    let fbo = unsafe { make_target(&gl) };
    unsafe { paint_two_tone(&gl) };

    // Baseline: the seam is a hard red/blue edge before any frost.
    let below = read_rgba8(&gl, (W / 2) as i32, (H / 2 - 4) as i32);
    let above = read_rgba8(&gl, (W / 2) as i32, (H / 2 + 4) as i32);
    assert_eq!(below, [255, 0, 0, 255], "pre-frost bottom half is red");
    assert_eq!(above, [0, 0, 255, 255], "pre-frost top half is blue");

    let renderer = consumer_probe::make_renderer(&gl).expect("GrabPassRenderer::new");
    let mut painter =
        egui_glow::Painter::new(Arc::clone(&gl), "", None, false).expect("egui_glow::Painter::new");

    let ctx = egui::Context::default();
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(W as f32, H as f32),
    ));

    // A surface straddling the seam, so the blur has something to smear.
    let surface_rect = egui::Rect::from_min_size(egui::pos2(60.0, 60.0), egui::vec2(200.0, 120.0));

    let outcome_before_paint = poll(&renderer);
    assert_eq!(
        outcome_before_paint,
        backdrop_blur_egui::FrostOutcome::DidNotFire,
        "nothing painted yet"
    );

    // egui 0.35: `Context::run` -> `Context::run_ui`, whose closure gets a `&mut Ui` (was `&Context`),
    // and `CentralPanel::show` takes that `&mut Ui` (was `&Context`).
    let output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::no_frame().show(ui, |ui| {
            consumer_probe::frame(&renderer, ui, surface_rect);
        });
    });

    let prims = ctx.tessellate(output.shapes, output.pixels_per_point);

    // The painter must paint into OUR framebuffer, not the (nonexistent) default one.
    unsafe { gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo)) };
    painter.paint_and_update_textures([W, H], output.pixels_per_point, &prims, &output.textures_delta);

    let outcome = poll(&renderer);
    assert_eq!(
        outcome,
        backdrop_blur_egui::FrostOutcome::Composited,
        "the grab-pass callback must have blurred and composited"
    );
    // Read-and-clear.
    assert_eq!(poll(&renderer), backdrop_blur_egui::FrostOutcome::DidNotFire);

    // Inside the surface, straddling the seam: the hard edge must now be a gradient.
    unsafe { gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo)) };
    let seam = read_rgba8(&gl, 160, (H / 2) as i32);
    let seam_lo = read_rgba8(&gl, 160, (H / 2 - 10) as i32);
    let seam_hi = read_rgba8(&gl, 160, (H / 2 + 10) as i32);
    eprintln!("SEAM  y=center     {seam:?}");
    eprintln!("SEAM  y=center-10  {seam_lo:?}   (was pure red)");
    eprintln!("SEAM  y=center+10  {seam_hi:?}   (was pure blue)");
    eprintln!("OUTSIDE (10,10)    {:?}", read_rgba8(&gl, 10, 10));
    assert!(
        seam[0] > 8 && seam[2] > 8,
        "blurred seam must mix red and blue, got {seam:?}"
    );
    assert!(
        seam != [255, 0, 0, 255] && seam != [0, 0, 255, 255],
        "blurred seam must not be a pure source color, got {seam:?}"
    );

    // Outside the surface, far from it, the backdrop is untouched.
    let untouched = read_rgba8(&gl, 10, 10);
    assert_eq!(untouched, [255, 0, 0, 255], "outside the surface stays red");

    // The declared surface geometry matches what we asked for (no silent rect drift).
    assert_eq!(build_surface(surface_rect).rect, surface_rect);

    painter.destroy();
    shutdown(&renderer, &gl);
}
