//! Video decoding for in-app playback, where no system video view exists (pure egui rendering on a
//! NativeActivity). Two backends behind [`Source`]:
//!
//! - [`VideoCodec`] — hardware-accelerated streaming decode via `MediaExtractor` + `MediaCodec`
//!   (the device's dedicated video-decode block). Decoder state is kept across frames, so cost is
//!   O(1) per frame. This is the fast path.
//! - [`VideoDecoder`] — `MediaMetadataRetriever.getFrameAtIndex`, which re-seeks to a keyframe and
//!   re-decodes for every frame (O(n²) over a GOP). Kept only as a fallback for files the codec
//!   path can't open.
//!
//! Neither needs a Context — both open a plain file path — so this works from any attached thread.
//! Frames come back as raw RGBA ready for an egui texture. Audio is not decoded.

use jni::objects::{GlobalRef, JByteBuffer, JObject, JString, JValue};
use jni::{JNIEnv, JavaVM};

/// Static shape of an opened video.
#[derive(Clone, Copy, Debug)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration_ms: i64,
    pub fps: f32,
}

/// One decoded frame in presentation order.
pub struct Frame {
    /// Presentation timestamp in milliseconds (drives pacing + the seek slider).
    pub pts_ms: i64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Permanently attach the calling thread to the JVM and hand back its env. Call once at the top of
/// a decode thread; the thread detaches itself when it terminates. The `JavaVM` wrapper lives in a
/// static so the returned env's borrow is `'static`.
pub fn attach_env() -> Option<JNIEnv<'static>> {
    static VM: std::sync::OnceLock<JavaVM> = std::sync::OnceLock::new();
    let vm = match VM.get() {
        Some(vm) => vm,
        None => {
            let ctx = ndk_context::android_context();
            let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
            VM.get_or_init(|| vm)
        }
    };
    match vm.attach_current_thread_permanently() {
        Ok(env) => Some(env),
        Err(e) => {
            log::error!("video: JVM attach failed: {e:?}");
            None
        }
    }
}

/// A decode backend. The player pulls frames sequentially and seeks by time.
pub enum Source {
    Codec(VideoCodec),
    Frames { dec: VideoDecoder, info: VideoInfo, next: i32 },
}

impl Source {
    /// Open `path`, preferring the hardware codec and falling back to the frame retriever.
    pub fn open(env: &mut JNIEnv, path: &str) -> Option<(Source, VideoInfo)> {
        if let Some((codec, info)) = VideoCodec::open(env, path) {
            log::info!("video: MediaCodec decode {}x{} {:.1}s", info.width, info.height, info.duration_ms as f64 / 1000.0);
            return Some((Source::Codec(codec), info));
        }
        log::warn!("video: MediaCodec open failed, falling back to frame retriever");
        let (dec, info) = VideoDecoder::open(env, path)?;
        Some((Source::Frames { dec, info, next: 0 }, info))
    }

    /// Next frame in stream order, or `None` at end of stream.
    pub fn next_frame(&mut self, env: &mut JNIEnv) -> Option<Frame> {
        match self {
            Source::Codec(c) => c.next_frame(env),
            Source::Frames { dec, info, next } => {
                let count = ((info.duration_ms as f64 / 1000.0) * info.fps as f64).ceil() as i32;
                if *next >= count.max(1) {
                    return None;
                }
                let idx = *next;
                *next += 1;
                let (width, height, rgba) = dec.frame_rgba(env, idx)?;
                let pts_ms = (idx as f64 / info.fps.max(1.0) as f64 * 1000.0) as i64;
                Some(Frame { pts_ms, width, height, rgba })
            }
        }
    }

    /// Jump to (near) `ms`. Codec seeks to the closest keyframe; the retriever is frame-exact.
    pub fn seek(&mut self, env: &mut JNIEnv, ms: i64) {
        match self {
            Source::Codec(c) => c.seek(env, ms),
            Source::Frames { info, next, .. } => {
                let count = ((info.duration_ms as f64 / 1000.0) * info.fps as f64).ceil() as i32;
                *next = ((ms as f64 / 1000.0 * info.fps.max(1.0) as f64) as i32)
                    .clamp(0, count.max(1) - 1);
            }
        }
    }

    pub fn release(&mut self, env: &mut JNIEnv) {
        match self {
            Source::Codec(c) => c.release(env),
            Source::Frames { dec, .. } => dec.release(env),
        }
    }
}

// ── Hardware codec path ──────────────────────────────────────────────────────

// MediaCodec constants (stable framework ABI).
const BUFFER_FLAG_END_OF_STREAM: i32 = 4;
const INFO_TRY_AGAIN_LATER: i32 = -1;
const INFO_OUTPUT_FORMAT_CHANGED: i32 = -2;
const SEEK_TO_PREVIOUS_SYNC: i32 = 0;
const DEQUEUE_TIMEOUT_US: i64 = 10_000;

pub struct VideoCodec {
    extractor: GlobalRef,
    codec: GlobalRef,
    info: GlobalRef, // reused MediaCodec$BufferInfo
    width: i32,
    height: i32,
    /// BT.709 vs BT.601 YUV→RGB matrix. Streams rarely tag their color standard, so default the
    /// way decoders do: HD (≥720 lines) is BT.709, SD is BT.601.
    bt709: bool,
    input_done: bool,
    output_done: bool,
    /// After a seek, decode and discard frames until pts reaches this (µs); 0 = no target. Keeps
    /// seeking frame-accurate even when keyframes are sparse (these short clips often have one).
    seek_target_us: i64,
}

impl VideoCodec {
    fn open(env: &mut JNIEnv, path: &str) -> Option<(VideoCodec, VideoInfo)> {
        type Opened = Option<(GlobalRef, GlobalRef, GlobalRef, i32, i32, VideoInfo)>;
        let opened = env.with_local_frame::<_, Opened, jni::errors::Error>(32, |env| {
            let extractor = env.new_object("android/media/MediaExtractor", "()V", &[])?;
            let jpath = env.new_string(path)?;
            if env
                .call_method(&extractor, "setDataSource", "(Ljava/lang/String;)V", &[(&jpath).into()])
                .is_err()
            {
                clear_exception(env, "MediaExtractor.setDataSource");
                let _ = env.call_method(&extractor, "release", "()V", &[]);
                return Ok(None);
            }

            // Find the first video track.
            let count = env.call_method(&extractor, "getTrackCount", "()I", &[])?.i()?;
            let mut track = -1;
            let mut mime = String::new();
            let mut format = JObject::null();
            for i in 0..count {
                let f = env
                    .call_method(&extractor, "getTrackFormat", "(I)Landroid/media/MediaFormat;", &[JValue::Int(i)])?
                    .l()?;
                if let Some(m) = format_string(env, &f, "mime")
                    && m.starts_with("video/")
                {
                    track = i;
                    mime = m;
                    format = f;
                    break;
                }
            }
            if track < 0 {
                let _ = env.call_method(&extractor, "release", "()V", &[]);
                return Ok(None);
            }
            env.call_method(&extractor, "selectTrack", "(I)V", &[JValue::Int(track)])?;

            let width = format_int(env, &format, "width").unwrap_or(0);
            let height = format_int(env, &format, "height").unwrap_or(0);
            let duration_us = format_long(env, &format, "durationUs").unwrap_or(0);
            let fps = format_int(env, &format, "frame-rate").unwrap_or(30).max(1) as f32;
            if width <= 0 || height <= 0 {
                let _ = env.call_method(&extractor, "release", "()V", &[]);
                return Ok(None);
            }

            // Ask for a flexible YUV420 layout so `getOutputImage` returns consumable planes.
            let color_flex = env
                .get_static_field(
                    "android/media/MediaCodecInfo$CodecCapabilities",
                    "COLOR_FormatYUV420Flexible",
                    "I",
                )
                .and_then(|v| v.i())
                .unwrap_or(0x7F42_0888);
            let key = env.new_string("color-format")?;
            env.call_method(
                &format,
                "setInteger",
                "(Ljava/lang/String;I)V",
                &[(&key).into(), JValue::Int(color_flex)],
            )?;

            let jmime = env.new_string(&mime)?;
            let codec = match env.call_static_method(
                "android/media/MediaCodec",
                "createDecoderByType",
                "(Ljava/lang/String;)Landroid/media/MediaCodec;",
                &[(&jmime).into()],
            ) {
                Ok(v) => v.l()?,
                Err(_) => {
                    clear_exception(env, "createDecoderByType");
                    let _ = env.call_method(&extractor, "release", "()V", &[]);
                    return Ok(None);
                }
            };
            let null = JObject::null();
            if env
                .call_method(
                    &codec,
                    "configure",
                    "(Landroid/media/MediaFormat;Landroid/view/Surface;Landroid/media/MediaCrypto;I)V",
                    &[(&format).into(), (&null).into(), (&null).into(), JValue::Int(0)],
                )
                .is_err()
            {
                clear_exception(env, "MediaCodec.configure");
                let _ = env.call_method(&codec, "release", "()V", &[]);
                let _ = env.call_method(&extractor, "release", "()V", &[]);
                return Ok(None);
            }
            env.call_method(&codec, "start", "()V", &[])?;
            let info = env.new_object("android/media/MediaCodec$BufferInfo", "()V", &[])?;

            let vinfo = VideoInfo {
                width: width as u32,
                height: height as u32,
                duration_ms: duration_us / 1000,
                fps: fps.clamp(1.0, 240.0),
            };
            Ok(Some((
                env.new_global_ref(&extractor)?,
                env.new_global_ref(&codec)?,
                env.new_global_ref(&info)?,
                width,
                height,
                vinfo,
            )))
        });
        let (extractor, codec, info, width, height, vinfo) = match opened {
            Ok(Some(v)) => v,
            Ok(None) => return None,
            Err(e) => {
                clear_exception(env, "VideoCodec::open");
                log::error!("video: codec open {path} failed: {e:?}");
                return None;
            }
        };
        Some((
            VideoCodec {
                extractor,
                codec,
                info,
                width,
                height,
                bt709: height >= 720,
                input_done: false,
                output_done: false,
                seek_target_us: 0,
            },
            vinfo,
        ))
    }

    fn next_frame(&mut self, env: &mut JNIEnv) -> Option<Frame> {
        if self.output_done {
            return None;
        }
        // Bound the pump so a stall can't spin forever; plenty for B-frame reordering.
        // After input EOS the decoder still drains its reorder buffer, returning TRY_AGAIN between
        // trailing frames — wait a bounded number of those before declaring end-of-stream so the
        // last frames aren't dropped, but don't stall long if the EOS flag never arrives.
        let mut drain_wait = 0u32;
        for _ in 0..512 {
            // Feed one input buffer if the codec has a slot free.
            // Re-fetch jobject refs after each &mut self call so borrows don't overlap.
            if !self.input_done {
                let codec = self.codec.as_obj();
                match env
                    .call_method(codec, "dequeueInputBuffer", "(J)I", &[JValue::Long(DEQUEUE_TIMEOUT_US)])
                    .and_then(|v| v.i())
                {
                    Ok(in_idx) if in_idx >= 0 => self.feed_input(env, in_idx),
                    Ok(_) => {}
                    Err(_) => clear_exception(env, "dequeueInputBuffer"),
                }
            }

            let codec = self.codec.as_obj();
            let info = self.info.as_obj();
            let out_idx = match env
                .call_method(
                    codec,
                    "dequeueOutputBuffer",
                    "(Landroid/media/MediaCodec$BufferInfo;J)I",
                    &[(&info).into(), JValue::Long(DEQUEUE_TIMEOUT_US)],
                )
                .and_then(|v| v.i())
            {
                Ok(v) => v,
                Err(_) => {
                    clear_exception(env, "dequeueOutputBuffer");
                    self.output_done = true;
                    return None;
                }
            };

            if out_idx >= 0 {
                let flags = env.get_field(info, "flags", "I").and_then(|v| v.i()).unwrap_or(0);
                let pts_us =
                    env.get_field(info, "presentationTimeUs", "J").and_then(|v| v.j()).unwrap_or(0);
                let eos = flags & BUFFER_FLAG_END_OF_STREAM != 0;
                // Frame-accurate seek: after seeking to the previous keyframe, discard frames
                // (without the YUV→RGBA conversion) until we reach the requested time.
                let skip = self.seek_target_us > 0 && pts_us < self.seek_target_us && !eos;
                let frame = if skip { None } else { self.read_output_image(env, out_idx, pts_us) };
                let codec = self.codec.as_obj();
                let _ = env.call_method(
                    codec,
                    "releaseOutputBuffer",
                    "(IZ)V",
                    &[JValue::Int(out_idx), JValue::Bool(0)],
                );
                let _ = env.exception_clear();
                drain_wait = 0;
                if eos {
                    self.output_done = true;
                }
                if skip {
                    continue; // keep decoding forward toward the seek target
                }
                self.seek_target_us = 0;
                if let Some(f) = frame {
                    return Some(f);
                }
                if self.output_done {
                    return None;
                }
                // A config/empty buffer — keep pumping for a real frame.
            } else if out_idx == INFO_OUTPUT_FORMAT_CHANGED {
                let codec = self.codec.as_obj();
                if let Ok(of) =
                    env.call_method(codec, "getOutputFormat", "()Landroid/media/MediaFormat;", &[])
                        .and_then(|v| v.l())
                {
                    self.width = format_int(env, &of, "width").unwrap_or(self.width);
                    self.height = format_int(env, &of, "height").unwrap_or(self.height);
                }
            } else if out_idx == INFO_TRY_AGAIN_LATER && self.input_done {
                // Draining after EOS: give trailing reorder frames ~120ms to appear (each timeout
                // is 10ms) before concluding the stream is done.
                drain_wait += 1;
                if drain_wait >= 12 {
                    self.output_done = true;
                    return None;
                }
            }
        }
        None
    }

    fn feed_input(&mut self, env: &mut JNIEnv, in_idx: i32) {
        let codec = self.codec.as_obj();
        let extractor = self.extractor.as_obj();
        let _ = env.with_local_frame::<_, (), jni::errors::Error>(8, |env| {
            let in_buf = env
                .call_method(codec, "getInputBuffer", "(I)Ljava/nio/ByteBuffer;", &[JValue::Int(in_idx)])?
                .l()?;
            let size = env
                .call_method(
                    extractor,
                    "readSampleData",
                    "(Ljava/nio/ByteBuffer;I)I",
                    &[(&in_buf).into(), JValue::Int(0)],
                )?
                .i()?;
            if size < 0 {
                env.call_method(
                    codec,
                    "queueInputBuffer",
                    "(IIIJI)V",
                    &[
                        JValue::Int(in_idx),
                        JValue::Int(0),
                        JValue::Int(0),
                        JValue::Long(0),
                        JValue::Int(BUFFER_FLAG_END_OF_STREAM),
                    ],
                )?;
                self.input_done = true;
            } else {
                let pts = env.call_method(extractor, "getSampleTime", "()J", &[])?.j()?;
                env.call_method(
                    codec,
                    "queueInputBuffer",
                    "(IIIJI)V",
                    &[
                        JValue::Int(in_idx),
                        JValue::Int(0),
                        JValue::Int(size),
                        JValue::Long(pts),
                        JValue::Int(0),
                    ],
                )?;
                env.call_method(extractor, "advance", "()Z", &[])?;
            }
            Ok(())
        });
    }

    fn read_output_image(&self, env: &mut JNIEnv, out_idx: i32, pts_us: i64) -> Option<Frame> {
        let codec = self.codec.as_obj();
        let (w, h) = (self.width, self.height);
        let got = env.with_local_frame::<_, Option<Frame>, jni::errors::Error>(16, |env| {
            let image = env
                .call_method(codec, "getOutputImage", "(I)Landroid/media/Image;", &[JValue::Int(out_idx)])?
                .l()?;
            if image.is_null() {
                return Ok(None);
            }
            let rgba = image_to_rgba(env, &image, w, h, self.bt709);
            let _ = env.call_method(&image, "close", "()V", &[]);
            Ok(rgba.map(|rgba| Frame { pts_ms: pts_us / 1000, width: w as u32, height: h as u32, rgba }))
        });
        match got {
            Ok(v) => v,
            Err(_) => {
                clear_exception(env, "read_output_image");
                None
            }
        }
    }

    fn seek(&mut self, env: &mut JNIEnv, ms: i64) {
        let _ = env.call_method(
            self.extractor.as_obj(),
            "seekTo",
            "(JI)V",
            &[JValue::Long(ms * 1000), JValue::Int(SEEK_TO_PREVIOUS_SYNC)],
        );
        let _ = env.exception_clear();
        let _ = env.call_method(self.codec.as_obj(), "flush", "()V", &[]);
        clear_exception(env, "seek");
        self.input_done = false;
        self.output_done = false;
        self.seek_target_us = ms * 1000;
    }

    fn release(&mut self, env: &mut JNIEnv) {
        // Clear any pending exception between calls — e.g. stop() throws IllegalStateException from
        // an error state, and the following release() must not run with an exception pending.
        let _ = env.call_method(self.codec.as_obj(), "stop", "()V", &[]);
        let _ = env.exception_clear();
        let _ = env.call_method(self.codec.as_obj(), "release", "()V", &[]);
        let _ = env.exception_clear();
        let _ = env.call_method(self.extractor.as_obj(), "release", "()V", &[]);
        let _ = env.exception_clear();
    }
}

/// Convert a flexible-YUV420 `android.media.Image` (as `MediaCodec` output) to RGBA. Reads the Y/U/V
/// planes' direct buffers in place (no copy) and does an integer limited-range YUV→RGB conversion
/// (BT.709 when `bt709`, else BT.601).
fn image_to_rgba(env: &mut JNIEnv, image: &JObject, w: i32, h: i32, bt709: bool) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let planes = env
        .call_method(image, "getPlanes", "()[Landroid/media/Image$Plane;", &[])
        .ok()?
        .l()
        .ok()?;
    let planes: jni::objects::JObjectArray = planes.into();
    let (yp, yr, ypx) = plane(env, &planes, 0)?;
    let (up, ur, upx) = plane(env, &planes, 1)?;
    let (vp, vr, vpx) = plane(env, &planes, 2)?;

    let (w, h) = (w as usize, h as usize);
    // Validate the exact max index the loop will touch, so the hot loop needs no per-pixel checks.
    // Chroma indexes at (y/2, x/2) for y in 0..h, x in 0..w — max is ((h-1)/2, (w-1)/2), which for
    // odd dimensions is one past `h/2 - 1` (YUV420 is normally even, but be defensive).
    let y_ok = (h - 1) * yr + (w - 1) * ypx < yp.len();
    let (cy, cx) = (h.saturating_sub(1) / 2, w.saturating_sub(1) / 2);
    let c_ok = |p: &[u8], r: usize, px: usize| cy * r + cx * px < p.len();
    if !(y_ok && c_ok(up, ur, upx) && c_ok(vp, vr, vpx)) {
        log::error!("video: plane strides out of bounds ({w}x{h})");
        return None;
    }

    // Integer limited-range coefficients (×256): [r_v, g_u, g_v, b_u]. Luma is 298 (1.164×256).
    let (kr, kgu, kgv, kb) = if bt709 { (459, -55, -136, 541) } else { (409, -100, -208, 516) };

    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let yrow = y * yr;
        let crow = (y / 2) * ur;
        let vrow = (y / 2) * vr;
        let orow = y * w * 4;
        for x in 0..w {
            let yy = yp[yrow + x * ypx] as i32;
            let uu = up[crow + (x / 2) * upx] as i32;
            let vv = vp[vrow + (x / 2) * vpx] as i32;
            let c = 298 * (yy - 16);
            let d = uu - 128;
            let e = vv - 128;
            let o = orow + x * 4;
            rgba[o] = clamp8((c + kr * e + 128) >> 8);
            rgba[o + 1] = clamp8((c + kgu * d + kgv * e + 128) >> 8);
            rgba[o + 2] = clamp8((c + kb * d + 128) >> 8);
            rgba[o + 3] = 255;
        }
    }
    Some(rgba)
}

/// Read one `Image.Plane`'s direct byte buffer (as a slice) plus its row/pixel strides.
fn plane<'a>(
    env: &mut JNIEnv,
    planes: &jni::objects::JObjectArray,
    i: i32,
) -> Option<(&'a [u8], usize, usize)> {
    let p = env.get_object_array_element(planes, i).ok()?;
    let row = env.call_method(&p, "getRowStride", "()I", &[]).ok()?.i().ok()? as usize;
    let pix = env.call_method(&p, "getPixelStride", "()I", &[]).ok()?.i().ok()? as usize;
    let buf = env.call_method(&p, "getBuffer", "()Ljava/nio/ByteBuffer;", &[]).ok()?.l().ok()?;
    let buf: JByteBuffer = buf.into();
    let addr = env.get_direct_buffer_address(&buf).ok()?;
    let cap = env.get_direct_buffer_capacity(&buf).ok()?;
    if addr.is_null() || cap == 0 {
        return None;
    }
    // SAFETY: the plane's direct buffer is valid until the Image is closed / the output buffer is
    // released, both of which happen after this frame's conversion completes.
    Some((unsafe { std::slice::from_raw_parts(addr, cap) }, row, pix))
}

#[inline]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

// ── MediaFormat helpers ──────────────────────────────────────────────────────

fn format_string(env: &mut JNIEnv, format: &JObject, key: &str) -> Option<String> {
    let jkey = env.new_string(key).ok()?;
    let s = env
        .call_method(format, "getString", "(Ljava/lang/String;)Ljava/lang/String;", &[(&jkey).into()])
        .ok()?
        .l()
        .ok()?;
    if s.is_null() {
        return None;
    }
    let s: JString = s.into();
    Some(env.get_string(&s).ok()?.into())
}

fn format_int(env: &mut JNIEnv, format: &JObject, key: &str) -> Option<i32> {
    if !format_has(env, format, key) {
        return None;
    }
    let jkey = env.new_string(key).ok()?;
    let v = env.call_method(format, "getInteger", "(Ljava/lang/String;)I", &[(&jkey).into()]);
    match v.and_then(|v| v.i()) {
        Ok(v) => Some(v),
        Err(_) => {
            let _ = env.exception_clear();
            None
        }
    }
}

fn format_long(env: &mut JNIEnv, format: &JObject, key: &str) -> Option<i64> {
    if !format_has(env, format, key) {
        return None;
    }
    let jkey = env.new_string(key).ok()?;
    env.call_method(format, "getLong", "(Ljava/lang/String;)J", &[(&jkey).into()]).ok()?.j().ok()
}

fn format_has(env: &mut JNIEnv, format: &JObject, key: &str) -> bool {
    let Ok(jkey) = env.new_string(key) else { return false };
    env.call_method(format, "containsKey", "(Ljava/lang/String;)Z", &[(&jkey).into()])
        .and_then(|v| v.z())
        .unwrap_or(false)
}

fn clear_exception(env: &mut JNIEnv, ctx: &str) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
        log::error!("video: JNI exception in {ctx}");
    }
}

// ── Frame-retriever fallback (MediaMetadataRetriever) ────────────────────────

/// An open `MediaMetadataRetriever` held as a global ref so it outlives local frames.
pub struct VideoDecoder {
    retriever: GlobalRef,
}

impl VideoDecoder {
    /// Open `path` and read the video's shape.
    fn open(env: &mut JNIEnv, path: &str) -> Option<(Self, VideoInfo)> {
        type Opened = Option<(GlobalRef, VideoInfo)>;
        let opened = env.with_local_frame::<_, Opened, jni::errors::Error>(16, |env| {
            let retriever = env.new_object("android/media/MediaMetadataRetriever", "()V", &[])?;
            let release = |env: &mut JNIEnv, r: &JObject| {
                let _ = env.call_method(r, "release", "()V", &[]);
                let _ = env.exception_clear();
            };
            let jpath = env.new_string(path)?;
            if env
                .call_method(&retriever, "setDataSource", "(Ljava/lang/String;)V", &[(&jpath).into()])
                .is_err()
            {
                clear_exception(env, "retriever.setDataSource");
                release(env, &retriever);
                return Ok(None);
            }
            let width = meta_i64(env, &retriever, "METADATA_KEY_VIDEO_WIDTH").unwrap_or(0);
            let height = meta_i64(env, &retriever, "METADATA_KEY_VIDEO_HEIGHT").unwrap_or(0);
            let duration_ms = meta_i64(env, &retriever, "METADATA_KEY_DURATION").unwrap_or(0);
            let frame_count =
                meta_i64(env, &retriever, "METADATA_KEY_VIDEO_FRAME_COUNT").unwrap_or(0);
            if width <= 0 || height <= 0 || frame_count <= 0 {
                release(env, &retriever);
                return Ok(None);
            }
            let fps = if duration_ms > 0 {
                (frame_count as f64 / (duration_ms as f64 / 1000.0)) as f32
            } else {
                24.0
            };
            let info = VideoInfo {
                width: width as u32,
                height: height as u32,
                duration_ms,
                fps: fps.clamp(1.0, 240.0),
            };
            Ok(Some((env.new_global_ref(&retriever)?, info)))
        });
        match opened {
            Ok(v) => v.map(|(retriever, info)| (Self { retriever }, info)),
            Err(e) => {
                clear_exception(env, "VideoDecoder::open");
                log::error!("video: retriever open {path} failed: {e:?}");
                None
            }
        }
    }

    fn frame_rgba(&self, env: &mut JNIEnv, index: i32) -> Option<(u32, u32, Vec<u8>)> {
        let got = env.with_local_frame::<_, Option<(u32, u32, Vec<u8>)>, jni::errors::Error>(8, |env| {
            let bitmap = env
                .call_method(
                    self.retriever.as_obj(),
                    "getFrameAtIndex",
                    "(I)Landroid/graphics/Bitmap;",
                    &[JValue::Int(index)],
                )?
                .l()?;
            if bitmap.is_null() {
                return Ok(None);
            }
            let rgba = crate::host::bitmap_to_rgba(env, &bitmap)?;
            let _ = env.call_method(&bitmap, "recycle", "()V", &[]);
            let _ = env.exception_clear();
            Ok(Some(rgba))
        });
        match got {
            Ok(v) => v,
            Err(_) => {
                clear_exception(env, "frame_rgba");
                None
            }
        }
    }

    fn release(&self, env: &mut JNIEnv) {
        let _ = env.call_method(self.retriever.as_obj(), "release", "()V", &[]);
        let _ = env.exception_clear();
    }
}

/// Read an integer `MediaMetadataRetriever.METADATA_KEY_*` via `extractMetadata` (decimal string).
fn meta_i64(env: &mut JNIEnv, retriever: &JObject, key_const: &str) -> Option<i64> {
    let got = (|| -> jni::errors::Result<Option<i64>> {
        let key = env
            .get_static_field("android/media/MediaMetadataRetriever", key_const, "I")?
            .i()?;
        let s = env
            .call_method(retriever, "extractMetadata", "(I)Ljava/lang/String;", &[JValue::Int(key)])?
            .l()?;
        if s.is_null() {
            return Ok(None);
        }
        let s: JString = s.into();
        let text: String = env.get_string(&s)?.into();
        Ok(text.trim().parse::<i64>().ok())
    })();
    match got {
        Ok(v) => v,
        Err(_) => {
            let _ = env.exception_clear();
            None
        }
    }
}
