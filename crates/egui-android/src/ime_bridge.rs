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
    Delete { before: usize, after: usize },
    Key(i32),
}

static LAST_SYNC: Mutex<Option<(String, i32, i32)>> = Mutex::new(None);
static IS_EGUI_ACTIVITY: OnceLock<bool> = OnceLock::new();

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
    } else if undoer.is_in_flux() {
        log::debug!("egui-android ime: undoer in flux; syncing last stable snapshot");
    }
    text
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

/// Apply drained IME events: selection → `TextEditState`; text/keys → `pending_events`.
pub fn apply_pending(
    ctx: &egui::Context,
    focus: Option<egui::Id>,
    pending_events: &mut Vec<egui::Event>,
) {
    let events = take_pending();
    if events.is_empty() {
        return;
    }
    log::debug!(
        "egui-android ime: apply_pending n={} focus={focus:?}",
        events.len()
    );
    for ev in events {
        match ev {
            ImeEvent::Selection { start, end } => {
                let Some(id) = focus else {
                    log::debug!("egui-android ime: selection {start}..{end} dropped (no focus)");
                    continue;
                };
                let Some(mut state) = egui::text_edit::TextEditState::load(ctx, id) else {
                    log::debug!("egui-android ime: selection {start}..{end} dropped (no TextEditState)");
                    continue;
                };
                let range = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(start),
                    egui::text::CCursor::new(end),
                );
                state.cursor.set_char_range(Some(range));
                state.store(ctx, id);
                log::debug!("egui-android ime: applied selection {start}..{end} to {id:?}");
                if let Ok(mut g) = LAST_SYNC.lock() {
                    if let Some((_, s, e)) = g.as_mut() {
                        *s = start as i32;
                        *e = end as i32;
                    }
                }
            }
            ImeEvent::Commit(text) => {
                if !text.is_empty() {
                    pending_events.push(egui::Event::Text(text));
                }
            }
            ImeEvent::Preedit(text) => {
                pending_events.push(egui::Event::Ime(egui::ImeEvent::Preedit {
                    text,
                    active_range_chars: None,
                }));
            }
            ImeEvent::Delete { before, after } => {
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
                if let Some(k) = egui_key {
                    pending_events.push(key(k));
                }
            }
        }
    }
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
/// Non-collapsed egui selections are mirrored as a caret at the selection end. Pushing a full
/// range into the selectable EditText puts Android into selection mode, which dismisses the
/// keyboard (Select All). Gboard trackpad still updates egui via `onSelectionChanged`.
pub fn sync_focused_text_edit(ctx: &egui::Context, focus: Option<egui::Id>) {
    let Some(id) = focus else { return };
    let Some(state) = egui::text_edit::TextEditState::load(ctx, id) else {
        return;
    };
    let text = probe_undoer_text(&state).unwrap_or_default();
    let (start, end) = selection_chars(&state);
    let caret = if start == end { start } else { end };
    sync_to_ime(&text, caret, caret);
}
