//! Hidden-EditText IME bridge for Gboard spacebar trackpad / InputConnection selection.
//!
//! Requires the app activity to be [`com.github.egui_mobile.EguiNativeActivity`]. Plain
//! `android.app.NativeActivity` falls back to `AndroidApp::show_soft_input`.
//!
//! JNI calls must use [`crate::host::with_native_activity`]: `ndk_context` only holds the
//! `Application`, so Activity methods never reach the hidden EditText. Class checks must use
//! `getObjectClass` (not `FindClass`) — the render thread's ClassLoader cannot see app classes.

use std::sync::{Mutex, OnceLock};

use jni::objects::{JObjectArray, JString, JValue};

const ACTIVITY_CLASS_NAME: &str = "com.github.egui_mobile.EguiNativeActivity";

/// Events drained from the Java `InputConnectionWrapper` queue.
#[derive(Debug, Clone)]
pub enum ImeEvent {
    Selection { start: usize, end: usize },
    Commit(String),
    Preedit(String),
    /// `finishComposingText`: end composition KEEPING the composing text as committed.
    Finish,
    Delete { before: usize, after: usize },
    Key(i32),
}

static LAST_SYNC: Mutex<Option<(String, i32, i32)>> = Mutex::new(None);
static IS_EGUI_ACTIVITY: OnceLock<bool> = OnceLock::new();
static WAKE_CTX: Mutex<Option<egui::Context>> = Mutex::new(None);

/// Register the egui context to wake when Java enqueues an InputConnection event.
pub fn set_wake_context(ctx: &egui::Context) {
    if let Ok(mut g) = WAKE_CTX.lock() {
        *g = Some(ctx.clone());
    }
}

/// JNI: `EguiNativeActivity.nativeImeWake()`. InputConnection events (commitText etc.) arrive on
/// the Android UI thread and produce no winit input event, so the render loop sleeps and typed
/// text sits in the Java queue until the next touch/key — this wakes it immediately instead.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiNativeActivity_nativeImeWake(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
) {
    if let Ok(g) = WAKE_CTX.lock() {
        if let Some(ctx) = g.as_ref() {
            ctx.request_repaint();
        }
    }
}

/// Whether `activity` is `EguiNativeActivity`.
///
/// Must not use `FindClass` for the app class: on the render thread JNI uses the system
/// ClassLoader, which cannot see `com.github.egui_mobile.*` and throws ClassNotFoundException.
fn is_egui_activity(env: &mut jni::JNIEnv, activity: &jni::objects::JObject) -> jni::errors::Result<bool> {
    if let Some(&cached) = IS_EGUI_ACTIVITY.get() {
        return Ok(cached);
    }
    let cls = env.get_object_class(activity)?;
    let name_obj = env
        .call_method(&cls, "getName", "()Ljava/lang/String;", &[])?
        .l()?;
    let js: JString = name_obj.into();
    let name: String = env.get_string(&js)?.into();
    let ok = name == ACTIVITY_CLASS_NAME;
    let _ = IS_EGUI_ACTIVITY.set(ok);
    Ok(ok)
}

/// Show or hide the soft keyboard on the hidden EditText. Returns false if the activity is
/// not `EguiNativeActivity` (caller should fall back to `AndroidApp` IME APIs).
pub fn set_soft_keyboard(show: bool) -> bool {
    let ok = crate::host::with_native_activity(|env, activity| {
        if !is_egui_activity(env, activity)? {
            log::debug!("egui-android ime: activity is not EguiNativeActivity; falling back");
            return Ok(false);
        }
        let method = if show { "showIme" } else { "hideIme" };
        env.call_method(activity, method, "()V", &[])?;
        if !show {
            if let Ok(mut g) = LAST_SYNC.lock() {
                *g = None;
            }
        }
        Ok(true)
    })
    .unwrap_or(false);
    if ok {
        log::debug!(
            "egui-android ime: {method} via EditText",
            method = if show { "showIme" } else { "hideIme" }
        );
    }
    ok
}

/// Keep the hidden EditText focused/visible without requesting another IME show animation.
pub fn bind_ime() -> bool {
    crate::host::with_native_activity(|env, activity| {
        if !is_egui_activity(env, activity)? {
            return Ok(false);
        }
        env.call_method(activity, "bindIme", "()V", &[])?;
        Ok(true)
    })
    .unwrap_or(false)
}

/// Re-show the soft keyboard on the EditText, bypassing the rising-edge throttle.
pub fn show_ime_force() -> bool {
    let ok = crate::host::with_native_activity(|env, activity| {
        if !is_egui_activity(env, activity)? {
            return Ok(false);
        }
        env.call_method(activity, "showImeForce", "()V", &[])?;
        Ok(true)
    })
    .unwrap_or(false);
    if ok {
        log::debug!("egui-android ime: showImeForce via EditText");
    }
    ok
}

/// Drop the egui→EditText dedupe cache so the next sync pushes even if text matches.
pub fn invalidate_last_sync() {
    if let Ok(mut g) = LAST_SYNC.lock() {
        *g = None;
    }
}

/// Preedit text egui currently displays (mirrors the last applied `C` event); consumed when
/// `finishComposingText` solidifies it. Cleared on commit, field switch, and keyboard hide so a
/// late Finish cannot re-commit a stale word into the newly focused field.
static LAST_PREEDIT: Mutex<String> = Mutex::new(String::new());

/// Forget the tracked preedit (field switch / keyboard hide).
pub fn clear_preedit_tracking() {
    if let Ok(mut g) = LAST_PREEDIT.lock() {
        g.clear();
    }
}

/// Push egui text + selection into the hidden EditText (no-op when unchanged).
pub fn sync_to_ime(text: &str, sel_start: usize, sel_end: usize) {
    let start = sel_start as i32;
    let end = sel_end as i32;
    if let Ok(g) = LAST_SYNC.lock() {
        if g.as_ref().is_some_and(|(t, s, e)| t == text && *s == start && *e == end) {
            return;
        }
    }
    let synced = crate::host::with_native_activity(|env, activity| {
        if !is_egui_activity(env, activity)? {
            return Ok(false);
        }
        let jtext = env.new_string(text)?;
        env.call_method(
            activity,
            "setImeState",
            "(Ljava/lang/String;II)V",
            &[(&jtext).into(), JValue::Int(start), JValue::Int(end)],
        )?;
        Ok(true)
    })
    .unwrap_or(false);
    if synced {
        log::debug!(
            "egui-android ime: sync_to_ime len={} sel={start}..{end}",
            text.chars().count()
        );
        if let Ok(mut g) = LAST_SYNC.lock() {
            *g = Some((text.to_owned(), start, end));
        }
    }
}

/// Drain pending InputConnection events from Kotlin.
pub fn take_pending() -> Vec<ImeEvent> {
    crate::host::with_native_activity(|env, activity| {
        if !is_egui_activity(env, activity)? {
            return Ok(Vec::new());
        }
        let arr = env
            .call_method(activity, "takePending", "()[Ljava/lang/String;", &[])?
            .l()?;
        if arr.is_null() {
            return Ok(Vec::new());
        }
        let arr: JObjectArray = arr.into();
        let n = env.get_array_length(&arr)?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let obj = env.get_object_array_element(&arr, i)?;
            if obj.is_null() {
                continue;
            }
            let js: JString = obj.into();
            let s: String = env.get_string(&js)?.into();
            if let Some(ev) = parse_event(&s) {
                out.push(ev);
            } else {
                log::debug!("egui-android ime: unparsed pending event {s:?}");
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}

fn parse_event(s: &str) -> Option<ImeEvent> {
    let (kind, rest) = s.split_once('\t')?;
    match kind {
        "S" => {
            let (a, b) = rest.split_once('\t')?;
            Some(ImeEvent::Selection {
                start: a.parse().ok()?,
                end: b.parse().ok()?,
            })
        }
        "T" => Some(ImeEvent::Commit(rest.to_owned())),
        "C" => Some(ImeEvent::Preedit(rest.to_owned())),
        "F" => Some(ImeEvent::Finish),
        "D" => {
            let (a, b) = rest.split_once('\t')?;
            Some(ImeEvent::Delete {
                before: a.parse().ok()?,
                after: b.parse().ok()?,
            })
        }
        "K" => Some(ImeEvent::Key(rest.parse().ok()?)),
        _ => None,
    }
}

/// Probe the latest undoer snapshot string without mutating the real undoer.
pub fn probe_undoer_text(state: &egui::text_edit::TextEditState) -> Option<String> {
    let mut undoer = state.undoer();
    let sentinel = (
        egui::text::CCursorRange::one(egui::text::CCursor::new(usize::MAX / 4)),
        "\u{FFFC}".to_owned(),
    );
    // Clone-only undo against a sentinel yields the latest committed snapshot.
    // While typing (flux), that lags `stable_time` (~1s) behind the live buffer.
    let text = undoer.undo(&sentinel).map(|(_, text)| text.clone());
    if text.is_none() {
        log::debug!("egui-android ime: undoer probe empty (no snapshot yet)");
    }
    text
}

/// True while egui has not yet committed a stable undo snapshot (active typing).
pub fn undoer_in_flux(state: &egui::text_edit::TextEditState) -> bool {
    state.undoer().is_in_flux()
}

/// Char-index selection from `TextEditState`, or `(0, 0)` if unset.
pub fn selection_chars(state: &egui::text_edit::TextEditState) -> (usize, usize) {
    match state.cursor.char_range() {
        Some(range) => {
            let r = range.as_sorted_char_range();
            (usize::from(r.start), usize::from(r.end))
        }
        None => (0, 0),
    }
}

// Android keycodes mirrored from KeyEvent.
const KEYCODE_DPAD_UP: i32 = 19;
const KEYCODE_DPAD_DOWN: i32 = 20;
const KEYCODE_DPAD_LEFT: i32 = 21;
const KEYCODE_DPAD_RIGHT: i32 = 22;
const KEYCODE_DEL: i32 = 67;
const KEYCODE_FORWARD_DEL: i32 = 112;

/// Apply drained IME events: text/keys → `pending_events`; selection → `TextEditState`.
/// Returns `true` if any events were applied.
pub fn apply_pending(
    ctx: &egui::Context,
    focus: Option<egui::Id>,
    pending_events: &mut Vec<egui::Event>,
) -> bool {
    let events = take_pending();
    if events.is_empty() {
        return false;
    }
    log::debug!(
        "egui-android ime: apply_pending n={} focus={focus:?}",
        events.len()
    );
    // Text/delete first. Selection in the same batch is usually a caret move from commitText and
    // would be applied before egui inserts the character — snapping the cursor to the start.
    let mut last_sel: Option<(usize, usize)> = None;
    let mut had_mutate = false;
    for ev in events {
        match ev {
            ImeEvent::Selection { start, end } => {
                last_sel = Some((start, end));
            }
            ImeEvent::Commit(text) => {
                had_mutate = true;
                if let Ok(mut g) = LAST_PREEDIT.lock() {
                    g.clear();
                }
                // Ime::Commit (not Event::Text): replaces the active preedit and resets egui's
                // composition state; a bare Text leaves stale ImeComposition cursor purpose.
                pending_events.push(egui::Event::Ime(egui::ImeEvent::Commit(text)));
            }
            ImeEvent::Preedit(text) => {
                had_mutate = true;
                if let Ok(mut g) = LAST_PREEDIT.lock() {
                    g.clone_from(&text);
                }
                pending_events.push(egui::Event::Ime(egui::ImeEvent::Preedit {
                    text,
                    active_range_chars: None,
                }));
            }
            ImeEvent::Finish => {
                // Commit the preedit egui is showing, unchanged. Only when one is tracked:
                // Gboard also finishes composition it started via setComposingRegion over
                // already-committed text, where committing again would duplicate the word.
                let preedit = LAST_PREEDIT
                    .lock()
                    .map(|mut g| std::mem::take(&mut *g))
                    .unwrap_or_default();
                if !preedit.is_empty() {
                    had_mutate = true;
                    pending_events.push(egui::Event::Ime(egui::ImeEvent::Commit(preedit)));
                }
            }
            ImeEvent::Delete { before, after } => {
                had_mutate = true;
                for _ in 0..before {
                    pending_events.push(key(egui::Key::Backspace));
                }
                for _ in 0..after {
                    pending_events.push(key(egui::Key::Delete));
                }
            }
            ImeEvent::Key(code) => {
                let egui_key = match code {
                    KEYCODE_DPAD_LEFT => Some(egui::Key::ArrowLeft),
                    KEYCODE_DPAD_RIGHT => Some(egui::Key::ArrowRight),
                    KEYCODE_DPAD_UP => Some(egui::Key::ArrowUp),
                    KEYCODE_DPAD_DOWN => Some(egui::Key::ArrowDown),
                    KEYCODE_DEL => Some(egui::Key::Backspace),
                    KEYCODE_FORWARD_DEL => Some(egui::Key::Delete),
                    _ => None,
                };
                if matches!(
                    code,
                    KEYCODE_DEL | KEYCODE_FORWARD_DEL
                ) {
                    had_mutate = true;
                }
                if let Some(k) = egui_key {
                    pending_events.push(key(k));
                }
            }
        }
    }
    // Trackpad / explicit caret move only — not selection attached to a text mutation.
    if !had_mutate {
        if let Some((start, end)) = last_sel {
            let Some(id) = focus else {
                return true;
            };
            let Some(mut state) = egui::text_edit::TextEditState::load(ctx, id) else {
                return true;
            };
            let range = egui::text::CCursorRange::two(
                egui::text::CCursor::new(start),
                egui::text::CCursor::new(end),
            );
            state.cursor.set_char_range(Some(range));
            state.store(ctx, id);
            if let Ok(mut g) = LAST_SYNC.lock() {
                if let Some((_, s, e)) = g.as_mut() {
                    *s = start as i32;
                    *e = end as i32;
                }
            }
        }
    }
    true
}

fn key(k: egui::Key) -> egui::Event {
    egui::Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

/// Sync focused `TextEdit` undoer text + cursor into the hidden EditText.
///
/// Skips while the undoer is in flux: the EditText already has live IME text from
/// `commitText` / `deleteSurroundingText`, and pushing a lagged undoer snapshot via
/// `setText` triggers `invalidateInput` every frame (breaks typing).
///
/// Non-collapsed egui selections are mirrored as a caret at the selection end. Pushing a full
/// range into the selectable EditText puts Android into selection mode, which dismisses the
/// keyboard (Select All). Gboard trackpad still updates egui via `onSelectionChanged`.
pub fn sync_focused_text_edit(ctx: &egui::Context, focus: Option<egui::Id>) {
    let Some(id) = focus else { return };
    let Some(state) = egui::text_edit::TextEditState::load(ctx, id) else {
        return;
    };
    if undoer_in_flux(&state) {
        return;
    }
    let text = probe_undoer_text(&state).unwrap_or_default();
    let (start, end) = selection_chars(&state);
    let caret = if start == end { start } else { end };
    sync_to_ime(&text, caret, caret);
}
