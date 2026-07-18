//! In-app video playback: a dedicated decode thread pulls frames through `egui_mobile::video`
//! (MediaCodec, with MediaMetadataRetriever fallback), paces them by presentation timestamp, and
//! hands RGBA frames to the UI over a channel. The UI keeps one texture updated per frame. No audio.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use egui_mobile::egui;
use egui_mobile::video::{Source, attach_env};

use crate::logger::Logger;

/// Playback controls shared with the decode thread. All relaxed atomics — worst case a control
/// lands one frame late.
pub struct PlayerCtrl {
    pub stop: AtomicBool,
    pub paused: AtomicBool,
    /// Frame index to jump to, or -1 for none (the decode thread converts it to a time). Frame
    /// indices keep the UI slider identical to the previous frame-retriever playback path.
    pub seek: AtomicI32,
    pub looping: AtomicBool,
}

/// What the decode thread sends back.
pub enum PlayerEvent {
    /// Sent once after the file opens.
    Info { frame_count: i32, fps: f32, duration_ms: i64 },
    /// One decoded frame, in presentation order.
    Frame { index: i32, image: egui::ColorImage },
    /// The file couldn't be opened or decoded at all.
    Failed(String),
    /// Drain probe while the UI isn't consuming (see the idle throttle); carries nothing.
    Ping,
}

/// UI-side handle to a running playback. Dropping it stops the decode thread and removes the
/// cache file (the thread's open fd survives the unlink until it exits).
pub struct Player {
    /// Gallery key (`subfolder/filename`) this playback belongs to.
    pub key: String,
    /// The on-disk file being decoded; unlinked on drop.
    path: String,
    pub ctrl: Arc<PlayerCtrl>,
    pub rx: Receiver<PlayerEvent>,
    pub tex: Option<egui::TextureHandle>,
    pub frame_count: i32,
    pub fps: f32,
    pub duration_ms: i64,
    /// Last presented frame index (drives the seek slider).
    pub cur: i32,
    pub failed: Option<String>,
    /// Set when playback was paused because the viewer left the screen (vs. by the user), so
    /// returning resumes it.
    pub auto_paused: bool,
}

impl Drop for Player {
    fn drop(&mut self) {
        self.ctrl.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Player {
    /// Spawn a decode thread for `path` and return the UI handle. The thread requests a repaint
    /// after every event so playback advances even while the UI is otherwise idle.
    pub fn start(path: String, key: String, ctx: egui::Context, log: Logger) -> Self {
        let ctrl = Arc::new(PlayerCtrl {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            seek: AtomicI32::new(-1),
            looping: AtomicBool::new(true),
        });
        // Bounded so undrained frames can't accumulate; the decode thread drops frames instead.
        let (tx, rx) = sync_channel(4);
        let thread_ctrl = ctrl.clone();
        let thread_path = path.clone();
        let spawned = std::thread::Builder::new()
            .name("video-decode".into())
            .spawn(move || decode_loop(thread_path, thread_ctrl, tx, ctx, log));
        let failed = spawned.is_err().then(|| "Couldn't start the decode thread".to_string());
        Self {
            key,
            path,
            ctrl,
            rx,
            tex: None,
            frame_count: 0,
            fps: 0.0,
            duration_ms: 0,
            cur: 0,
            failed,
            auto_paused: false,
        }
    }

    /// Drain pending frames/events into the texture. Call once per UI frame.
    pub fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                PlayerEvent::Info { frame_count, fps, duration_ms, .. } => {
                    self.frame_count = frame_count;
                    self.fps = fps;
                    self.duration_ms = duration_ms;
                }
                PlayerEvent::Frame { index, image } => {
                    self.cur = index;
                    match &mut self.tex {
                        Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                        None => {
                            self.tex = Some(ctx.load_texture(
                                format!("video#{}", self.key),
                                image,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                }
                PlayerEvent::Failed(e) => self.failed = Some(e),
                PlayerEvent::Ping => {}
            }
        }
    }
}

fn decode_loop(
    path: String,
    ctrl: Arc<PlayerCtrl>,
    tx: SyncSender<PlayerEvent>,
    ctx: egui::Context,
    log: Logger,
) {
    let Some(mut env) = attach_env() else {
        let _ = tx.send(PlayerEvent::Failed("JVM attach failed".into()));
        ctx.request_repaint();
        return;
    };
    let Some((mut src, info)) = Source::open(&mut env, &path) else {
        let _ = tx.send(PlayerEvent::Failed(
            "This video can't be decoded on this device".into(),
        ));
        ctx.request_repaint();
        return;
    };
    let frame_count =
        ((info.duration_ms as f64 / 1000.0) * info.fps as f64).ceil().max(1.0) as i32;
    log.info(format!(
        "video: {}x{} ~{} frames @{:.1}fps, {:.1}s",
        info.width,
        info.height,
        frame_count,
        info.fps,
        info.duration_ms as f64 / 1000.0
    ));
    let _ = tx.send(PlayerEvent::Info {
        frame_count,
        fps: info.fps,
        duration_ms: info.duration_ms,
    });
    ctx.request_repaint();

    let mut full_streak: u32 = 0;
    let mut last_pts = 0i64;
    // Whether any frame was produced since the last (re)start, and how many restarts in a row
    // produced nothing — a dead decoder returns None immediately, which would otherwise busy-spin.
    let mut produced = false;
    let mut empty_passes: u32 = 0;
    loop {
        if ctrl.stop.load(Ordering::Relaxed) {
            break;
        }
        if full_streak >= 8 {
            std::thread::sleep(Duration::from_millis(300));
            match tx.try_send(PlayerEvent::Ping) {
                Ok(()) => {
                    full_streak = 0;
                    ctx.request_repaint();
                }
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }
            continue;
        }
        let seek = ctrl.seek.swap(-1, Ordering::Relaxed);
        if seek >= 0 {
            let ms = ((seek as f64 / info.fps.max(1.0) as f64) * 1000.0) as i64;
            src.seek(&mut env, ms.clamp(0, info.duration_ms.max(0)));
            last_pts = ms;
            produced = false;
        } else if ctrl.paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(40));
            continue;
        }

        let started = Instant::now();
        let Some(frame) = src.next_frame(&mut env) else {
            // End of stream. A pass that produced NO frames means a broken/dead decoder — back off
            // so it can't busy-spin, and give up after a few consecutive empty passes.
            if !produced {
                empty_passes += 1;
                if empty_passes >= 5 {
                    let _ = tx.send(PlayerEvent::Failed("Playback stopped — decode error".into()));
                    ctx.request_repaint();
                    break;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            if ctrl.looping.load(Ordering::Relaxed) {
                src.seek(&mut env, 0);
                last_pts = 0;
                produced = false;
                continue;
            }
            ctrl.paused.store(true, Ordering::Relaxed);
            continue;
        };
        produced = true;
        empty_passes = 0;
        let index = ((frame.pts_ms as f64 / 1000.0) * info.fps.max(1.0) as f64).round() as i32;
        let index = index.clamp(0, frame_count - 1);
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match tx.try_send(PlayerEvent::Frame { index, image }) {
            Ok(()) => {
                full_streak = 0;
                ctx.request_repaint();
            }
            Err(TrySendError::Full(_)) => full_streak += 1,
            Err(TrySendError::Disconnected(_)) => break,
        }

        // Pace by presentation timestamps so variable-framerate streams stay in sync.
        let gap_ms = (frame.pts_ms - last_pts).max(0) as u64;
        last_pts = frame.pts_ms;
        let target = if gap_ms > 0 && gap_ms < 200 {
            Duration::from_millis(gap_ms)
        } else {
            Duration::from_secs_f64(1.0 / info.fps.max(1.0) as f64)
        };
        let spent = started.elapsed();
        if spent < target {
            std::thread::sleep(target - spent);
        }
    }
    src.release(&mut env);
}
