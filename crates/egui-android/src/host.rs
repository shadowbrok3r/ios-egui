//! Android host capability bridge. Each frame the runtime drains the [`Host`] request queue via
//! the core `drv_*` API and dispatches to Android through JNI (jni 0.21; the raw JavaVM/Activity
//! come from `ndk-context`, so android-activity's internal jni version is irrelevant).
//!
//! Everything here runs on the render thread. Capabilities that are thread-safe or use
//! `startActivity`/`NotificationManager`/`PackageInstaller` work directly. Capabilities that
//! require the Java UI thread (toast, an overlay `WindowManager` view, a foreground `Service`)
//! need a bundled Kotlin helper and are the next pass — their permission/gating flows are already
//! wired here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use android_activity::AndroidApp;
use egui_mobile_core::Host;
use jni::JavaVM;
use jni::objects::{JObject, JString, JValue};

// AndroidApp handle for IME control and content-rect reads; set once by `run`.
static ANDROID_APP: OnceLock<AndroidApp> = OnceLock::new();

// Last keyboard state the app requested; drives the text-actions bar visibility.
static KEYBOARD_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_android_app(app: AndroidApp) {
    let _ = ANDROID_APP.set(app);
}

pub(crate) fn keyboard_requested() -> bool {
    KEYBOARD_REQUESTED.load(Ordering::Relaxed)
}

// Backend-specific request kinds enqueued by `HostExt` (via `Host::drv_enqueue`).
const K_SELF_UPDATE: i32 = 100;
const K_REQ_INSTALL_PERM: i32 = 101;
const K_REQ_OVERLAY_PERM: i32 = 102;
const K_REQ_NOTIF_PERM: i32 = 103;
const K_SAVE_GALLERY: i32 = 104;
const K_REQ_MEDIA_PERM: i32 = 105;
const K_SHARE_MEDIA: i32 = 106;

// Pending permission checks: (core permission index, android permission string, frames left).
static PENDING_PERMS: Mutex<Vec<(usize, String, u32)>> = Mutex::new(Vec::new());
const PERM_POLL_FRAMES: u32 = 600; // ~10s at 60fps before giving up (marking denied).

/// Drain queued host requests and dispatch them. Called once per frame from the render loop.
pub fn drain(host: &Host) {
    poll_pending_permissions(host);
    while let Some(kind) = host.drv_pop() {
        match kind {
            0 => {
                if let Some(path) = host.drv_str_a() {
                    let (name, mime) = share_name_and_mime(&path);
                    share_media(&path, &name, mime);
                }
            }
            1 => notify(
                &host.drv_str_a().unwrap_or_default(),
                &host.drv_str_b().unwrap_or_default(),
            ),
            2 => {
                let show = host.drv_int() != 0;
                KEYBOARD_REQUESTED.store(show, Ordering::Relaxed);
                set_soft_keyboard(show);
            }
            3 => vibrate(haptic_ms(host.drv_int())),
            4 => {
                if let Some(url) = host.drv_str_a() {
                    open_url(&url);
                }
            }
            6 => request_permission(Some(0), "android.permission.CAMERA"),
            7 => request_permission(Some(1), "android.permission.RECORD_AUDIO"),
            10 => {
                if let Some(text) = host.drv_str_a() {
                    set_clipboard(&text);
                }
            }
            11 => {
                if let Some(text) = host.drv_str_a() {
                    share_text(&text);
                }
            }
            K_SELF_UPDATE => {
                if let Some(path) = host.drv_str_a() {
                    self_update(&path);
                }
            }
            K_REQ_INSTALL_PERM => request_install_permission(),
            K_REQ_OVERLAY_PERM => request_overlay_permission(),
            K_REQ_NOTIF_PERM => request_permission(None, "android.permission.POST_NOTIFICATIONS"),
            K_SAVE_GALLERY => {
                if let (Some(path), Some(meta)) = (host.drv_str_a(), host.drv_str_b()) {
                    let (name, mime) = meta.split_once('\t').unwrap_or((meta.as_str(), "image/png"));
                    save_to_gallery(&path, name, mime);
                }
            }
            K_SHARE_MEDIA => {
                if let (Some(path), Some(meta)) = (host.drv_str_a(), host.drv_str_b()) {
                    let (name, mime) = meta.split_once('\t').unwrap_or((meta.as_str(), "image/png"));
                    share_media(&path, name, mime);
                }
            }
            // The runtime permission dialog needs the Activity, but `ndk_context` only exposes the
            // Application here (android-activity 0.6 keeps the Activity private), so
            // `requestPermissions` throws NoSuchMethodError. Send the user to the app's Settings
            // page to grant Photos access instead — the same fallback used for install/overlay perms.
            K_REQ_MEDIA_PERM => {
                start_settings_for_package("android.settings.APPLICATION_DETAILS_SETTINGS")
            }
            other => log::info!("egui-android: host request kind {other} not handled"),
        }
    }
}

/// Run `f` with a JNIEnv attached to the current thread and the real `Activity`.
///
/// Prefer this over [`ndk_context`]: android-activity stores the `Application` there, not the
/// `Activity`, so `instanceof EguiNativeActivity` and Activity-only APIs fail.
pub(crate) fn with_native_activity<R>(
    f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<R>,
) -> Option<R> {
    let app = ANDROID_APP.get()?;
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    // Unowned JNI global ref from AndroidApp — must not DeleteLocalRef on drop.
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let out = match f(&mut env, &activity) {
        Ok(r) => Some(r),
        Err(e) => {
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
            }
            let _ = env.exception_clear();
            log::error!("egui-android JNI error: {e:?}");
            None
        }
    };
    std::mem::forget(activity);
    out
}

/// Run `f` with a JNIEnv and a Context. Uses the real Activity when registered; falls back to
/// `ndk_context`'s Application (enough for `getSystemService` / `startActivity` + NEW_TASK).
fn with_activity<R>(
    f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<R>,
) -> Option<R> {
    if ANDROID_APP.get().is_some() {
        return with_native_activity(f);
    }
    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
    let out = match f(&mut env, &activity) {
        Ok(r) => Some(r),
        Err(e) => {
            // Surface the Java exception's stack to logcat, then clear it so it can't poison the
            // next JNI call this frame.
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
            }
            let _ = env.exception_clear();
            log::error!("egui-android JNI error: {e:?}");
            None
        }
    };
    std::mem::forget(activity);
    out
}

fn package_name(env: &mut jni::JNIEnv, activity: &JObject) -> jni::errors::Result<String> {
    let obj = env
        .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
        .l()?;
    let s: JString = obj.into();
    Ok(env.get_string(&s)?.into())
}

// ── Common capabilities (render-thread safe) ─────────────────────────────────

fn open_url(url: &str) {
    with_activity(|env, activity| {
        let jurl = env.new_string(url)?;
        let uri = env
            .call_static_method(
                "android/net/Uri",
                "parse",
                "(Ljava/lang/String;)Landroid/net/Uri;",
                &[(&jurl).into()],
            )?
            .l()?;
        let action = env.new_string("android.intent.action.VIEW")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;Landroid/net/Uri;)V",
            &[(&action).into(), (&uri).into()],
        )?;
        env.call_method(
            &intent,
            "addFlags",
            "(I)Landroid/content/Intent;",
            &[JValue::Int(0x1000_0000)],
        )?;
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&intent).into()],
        )?;
        Ok(())
    });
}

fn vibrate(ms: i64) {
    with_activity(|env, activity| {
        let svc = env.new_string("vibrator")?;
        let vibrator = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&svc).into()],
            )?
            .l()?;
        let effect = env
            .call_static_method(
                "android/os/VibrationEffect",
                "createOneShot",
                "(JI)Landroid/os/VibrationEffect;",
                &[JValue::Long(ms), JValue::Int(-1)],
            )?
            .l()?;
        env.call_method(
            &vibrator,
            "vibrate",
            "(Landroid/os/VibrationEffect;)V",
            &[(&effect).into()],
        )?;
        Ok(())
    });
}

fn set_clipboard(text: &str) {
    with_activity(|env, activity| {
        let svc = env.new_string("clipboard")?;
        let cm = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&svc).into()],
            )?
            .l()?;
        let label = env.new_string("egui")?;
        let content = env.new_string(text)?;
        let clip = env
            .call_static_method(
                "android/content/ClipData",
                "newPlainText",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                &[(&label).into(), (&content).into()],
            )?
            .l()?;
        env.call_method(
            &cm,
            "setPrimaryClip",
            "(Landroid/content/ClipData;)V",
            &[(&clip).into()],
        )?;
        Ok(())
    });
}

/// True when the primary clip exists (no string materialization — safe to poll).
pub fn clipboard_has_text() -> bool {
    with_activity(|env, activity| {
        let svc = env.new_string("clipboard")?;
        let cm = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&svc).into()],
            )?
            .l()?;
        if cm.is_null() {
            return Ok(false);
        }
        Ok(env.call_method(&cm, "hasPrimaryClip", "()Z", &[])?.z()?)
    })
    .unwrap_or(false)
}

/// Read the system clipboard as text (Android grants reads only while the app has focus).
pub fn read_clipboard_text() -> Option<String> {
    with_activity(|env, activity| {
        let svc = env.new_string("clipboard")?;
        let cm = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&svc).into()],
            )?
            .l()?;
        let clip = env
            .call_method(&cm, "getPrimaryClip", "()Landroid/content/ClipData;", &[])?
            .l()?;
        if clip.is_null() {
            return Ok(None);
        }
        if env.call_method(&clip, "getItemCount", "()I", &[])?.i()? == 0 {
            return Ok(None);
        }
        let item = env
            .call_method(
                &clip,
                "getItemAt",
                "(I)Landroid/content/ClipData$Item;",
                &[JValue::Int(0)],
            )?
            .l()?;
        let text = env
            .call_method(
                &item,
                "coerceToText",
                "(Landroid/content/Context;)Ljava/lang/CharSequence;",
                &[JValue::Object(activity)],
            )?
            .l()?;
        if text.is_null() {
            return Ok(None);
        }
        let s = env
            .call_method(&text, "toString", "()Ljava/lang/String;", &[])?
            .l()?;
        let js: JString = s.into();
        let out: String = env.get_string(&js)?.into();
        Ok((!out.is_empty()).then_some(out))
    })
    .flatten()
}

fn share_text(text: &str) {
    with_activity(|env, activity| {
        let action = env.new_string("android.intent.action.SEND")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[(&action).into()],
        )?;
        let mime = env.new_string("text/plain")?;
        env.call_method(
            &intent,
            "setType",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[(&mime).into()],
        )?;
        let key = env.new_string("android.intent.extra.TEXT")?;
        let jtext = env.new_string(text)?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
            &[(&key).into(), (&jtext).into()],
        )?;
        let null = JObject::null();
        let chooser = env
            .call_static_method(
                "android/content/Intent",
                "createChooser",
                "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;",
                &[(&intent).into(), (&null).into()],
            )?
            .l()?;
        env.call_method(
            &chooser,
            "addFlags",
            "(I)Landroid/content/Intent;",
            &[JValue::Int(0x1000_0000)],
        )?;
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&chooser).into()],
        )?;
        Ok(())
    });
}

/// Insert `path`'s bytes into MediaStore under `Pictures/ComfyUI` and return the `content://` URI.
/// Scoped-storage insert (API 29+), so no runtime storage permission is needed.
fn insert_into_media_store<'l>(
    env: &mut jni::JNIEnv<'l>,
    activity: &JObject,
    path: &str,
    name: &str,
    mime: &str,
) -> jni::errors::Result<JObject<'l>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            log::error!("insert_into_media_store: reading {path} failed: {e}");
            return Ok(JObject::null());
        }
    };
    let resolver = env
        .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
        .l()?;

    let values = env.new_object("android/content/ContentValues", "()V", &[])?;
    for (key, val) in
        [("_display_name", name), ("mime_type", mime), ("relative_path", "Pictures/ComfyUI")]
    {
        let k = env.new_string(key)?;
        let v = env.new_string(val)?;
        env.call_method(
            &values,
            "put",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[(&k).into(), (&v).into()],
        )?;
    }
    // is_pending = 1 while writing, so nothing sees a half-written file.
    let pending_key = env.new_string("is_pending")?;
    let one = env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(1)])?;
    env.call_method(
        &values,
        "put",
        "(Ljava/lang/String;Ljava/lang/Integer;)V",
        &[(&pending_key).into(), (&one).into()],
    )?;

    let collection = env
        .get_static_field(
            "android/provider/MediaStore$Images$Media",
            "EXTERNAL_CONTENT_URI",
            "Landroid/net/Uri;",
        )?
        .l()?;
    let uri = env
        .call_method(
            &resolver,
            "insert",
            "(Landroid/net/Uri;Landroid/content/ContentValues;)Landroid/net/Uri;",
            &[(&collection).into(), (&values).into()],
        )?
        .l()?;
    if uri.is_null() {
        log::error!("insert_into_media_store: MediaStore insert returned null");
        return Ok(uri);
    }

    let stream = env
        .call_method(
            &resolver,
            "openOutputStream",
            "(Landroid/net/Uri;)Ljava/io/OutputStream;",
            &[(&uri).into()],
        )?
        .l()?;
    let array = env.byte_array_from_slice(&bytes)?;
    env.call_method(&stream, "write", "([B)V", &[(&array).into()])?;
    env.call_method(&stream, "close", "()V", &[])?;

    // Clear is_pending so the image becomes visible in the gallery.
    let finalize = env.new_object("android/content/ContentValues", "()V", &[])?;
    let pk = env.new_string("is_pending")?;
    let zero = env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(0)])?;
    env.call_method(
        &finalize,
        "put",
        "(Ljava/lang/String;Ljava/lang/Integer;)V",
        &[(&pk).into(), (&zero).into()],
    )?;
    let null_obj = JObject::null();
    env.call_method(
        &resolver,
        "update",
        "(Landroid/net/Uri;Landroid/content/ContentValues;Ljava/lang/String;[Ljava/lang/String;)I",
        &[(&uri).into(), (&finalize).into(), (&null_obj).into(), (&null_obj).into()],
    )?;
    Ok(uri)
}

/// Copy `path`'s bytes into the shared Photos gallery under `Pictures/ComfyUI` via MediaStore.
/// Best-effort: failures are logged and swallowed, never crashing the render loop.
fn save_to_gallery(path: &str, name: &str, mime: &str) {
    let done = with_activity(|env, activity| {
        let uri = insert_into_media_store(env, activity, path, name, mime)?;
        if !uri.is_null() {
            log::info!("save_to_gallery: {name} -> Pictures/ComfyUI");
        }
        Ok(())
    });
    if done.is_none() {
        log::error!("save_to_gallery: JNI call failed for {name}");
    }
}

/// Insert `path` into MediaStore, then present the system share sheet for the resulting URI.
/// Best-effort; failures are logged and swallowed.
fn share_media(path: &str, name: &str, mime: &str) {
    let done = with_activity(|env, activity| {
        let uri = insert_into_media_store(env, activity, path, name, mime)?;
        if uri.is_null() {
            return Ok(());
        }
        let action = env.new_string("android.intent.action.SEND")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[(&action).into()],
        )?;
        let jmime = env.new_string(mime)?;
        env.call_method(
            &intent,
            "setType",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[(&jmime).into()],
        )?;
        let key = env.new_string("android.intent.extra.STREAM")?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Landroid/os/Parcelable;)Landroid/content/Intent;",
            &[(&key).into(), (&uri).into()],
        )?;
        // Grant the receiving app read access to the content URI, and launch outside the task.
        env.call_method(
            &intent,
            "addFlags",
            "(I)Landroid/content/Intent;",
            &[JValue::Int(0x1000_0001)],
        )?;
        let null = JObject::null();
        let chooser = env
            .call_static_method(
                "android/content/Intent",
                "createChooser",
                "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;",
                &[(&intent).into(), (&null).into()],
            )?
            .l()?;
        env.call_method(
            &chooser,
            "addFlags",
            "(I)Landroid/content/Intent;",
            &[JValue::Int(0x1000_0000)],
        )?;
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&chooser).into()],
        )?;
        log::info!("share_media: {name} ({mime})");
        Ok(())
    });
    if done.is_none() {
        log::error!("share_media: JNI call failed for {name}");
    }
}

/// Display name and MIME for a shared file path, keyed off the extension.
fn share_name_and_mime(path: &str) -> (String, &'static str) {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_string();
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    };
    (name, mime)
}

/// The runtime permission that gates reading the shared image gallery: scoped
/// `READ_MEDIA_IMAGES` on Android 13+ (API 33), the broad `READ_EXTERNAL_STORAGE` below.
fn media_images_permission() -> &'static str {
    let sdk = with_activity(|env, _| {
        env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()
    })
    .unwrap_or(0);
    if sdk >= 33 {
        "android.permission.READ_MEDIA_IMAGES"
    } else {
        "android.permission.READ_EXTERNAL_STORAGE"
    }
}

fn jni_has_media_permission() -> Option<bool> {
    check_permission(media_images_permission())
}

/// List the most recent device gallery images as `(MediaStore id, display name)`, newest first,
/// capped at `limit`.
fn jni_list_device_images(limit: i32) -> Option<Vec<(i64, String)>> {
    with_activity(|env, activity| {
        let resolver = env
            .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
            .l()?;
        let collection = env
            .get_static_field(
                "android/provider/MediaStore$Images$Media",
                "EXTERNAL_CONTENT_URI",
                "Landroid/net/Uri;",
            )?
            .l()?;
        // Projection [_id, _display_name]; MediaStore sorts by date_added descending (newest first).
        let col_id = env.new_string("_id")?;
        let projection = env.new_object_array(2, "java/lang/String", &col_id)?;
        let col_name = env.new_string("_display_name")?;
        env.set_object_array_element(&projection, 1, &col_name)?;
        let sort = env.new_string("date_added DESC")?;
        let null = JObject::null();
        let cursor = env
            .call_method(
                &resolver,
                "query",
                "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
                &[
                    (&collection).into(),
                    (&projection).into(),
                    (&null).into(),
                    (&null).into(),
                    (&sort).into(),
                ],
            )?
            .l()?;
        if cursor.is_null() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        while (out.len() as i32) < limit {
            if !env.call_method(&cursor, "moveToNext", "()Z", &[])?.z()? {
                break;
            }
            // Read the row inside a local frame so the per-row `getString` local reference is freed
            // each iteration — otherwise hundreds of rows overflow ART's local reference table.
            let row = env.with_local_frame::<_, (i64, String), jni::errors::Error>(8, |env| {
                let id = env.call_method(&cursor, "getLong", "(I)J", &[JValue::Int(0)])?.j()?;
                let name_obj = env
                    .call_method(&cursor, "getString", "(I)Ljava/lang/String;", &[JValue::Int(1)])?
                    .l()?;
                let name = if name_obj.is_null() {
                    String::new()
                } else {
                    let s: JString = name_obj.into();
                    env.get_string(&s)?.into()
                };
                Ok((id, name))
            })?;
            out.push(row);
        }
        env.call_method(&cursor, "close", "()V", &[])?;
        Ok(out)
    })
}

/// `content://` URI for a MediaStore image id.
fn image_uri<'a>(
    env: &mut jni::JNIEnv<'a>,
    id: i64,
) -> jni::errors::Result<JObject<'a>> {
    let base = env
        .get_static_field(
            "android/provider/MediaStore$Images$Media",
            "EXTERNAL_CONTENT_URI",
            "Landroid/net/Uri;",
        )?
        .l()?;
    env.call_static_method(
        "android/content/ContentUris",
        "withAppendedId",
        "(Landroid/net/Uri;J)Landroid/net/Uri;",
        &[(&base).into(), JValue::Long(id)],
    )?
    .l()
}

/// A device image's thumbnail as raw RGBA pixels `(width, height, rgba)` (≈ `size`×`size`).
/// `loadThumbnail` needs API 29+. Returns pixels directly (no PNG round-trip) so the caller can
/// build a texture without a re-decode.
fn jni_load_device_thumb(id: i64, size: i32) -> Option<(u32, u32, Vec<u8>)> {
    with_activity(|env, activity| {
        let resolver = env
            .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
            .l()?;
        let uri = image_uri(env, id)?;
        let size_obj =
            env.new_object("android/util/Size", "(II)V", &[JValue::Int(size), JValue::Int(size)])?;
        let null = JObject::null();
        let bitmap = env
            .call_method(
                &resolver,
                "loadThumbnail",
                "(Landroid/net/Uri;Landroid/util/Size;Landroid/os/CancellationSignal;)Landroid/graphics/Bitmap;",
                &[(&uri).into(), (&size_obj).into(), (&null).into()],
            )?
            .l()?;
        bitmap_to_rgba(env, &bitmap)
    })
}

/// Read an ARGB_8888 `Bitmap` into an egui-ready RGBA byte buffer via `getPixels`.
pub(crate) fn bitmap_to_rgba(env: &mut jni::JNIEnv, bitmap: &JObject) -> jni::errors::Result<(u32, u32, Vec<u8>)> {
    let w = env.call_method(bitmap, "getWidth", "()I", &[])?.i()?;
    let h = env.call_method(bitmap, "getHeight", "()I", &[])?.i()?;
    let n = (w.max(0) as usize) * (h.max(0) as usize);
    let pixels = env.new_int_array(n as i32)?;
    env.call_method(
        bitmap,
        "getPixels",
        "([IIIIIII)V",
        &[
            (&pixels).into(),
            JValue::Int(0),      // offset
            JValue::Int(w),      // stride
            JValue::Int(0),      // x
            JValue::Int(0),      // y
            JValue::Int(w),
            JValue::Int(h),
        ],
    )?;
    let mut argb = vec![0i32; n];
    env.get_int_array_region(&pixels, 0, &mut argb)?;
    // getPixels yields 0xAARRGGBB per int (non-premultiplied); unpack to RGBA bytes.
    let mut rgba = Vec::with_capacity(n * 4);
    for px in &argb {
        let p = *px as u32;
        rgba.push(((p >> 16) & 0xff) as u8); // R
        rgba.push(((p >> 8) & 0xff) as u8); // G
        rgba.push((p & 0xff) as u8); // B
        rgba.push(((p >> 24) & 0xff) as u8); // A
    }
    Ok((w as u32, h as u32, rgba))
}

/// A device image's full file bytes (for upload to ComfyUI).
fn jni_load_device_bytes(id: i64) -> Option<Vec<u8>> {
    with_activity(|env, activity| {
        let resolver = env
            .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
            .l()?;
        let uri = image_uri(env, id)?;
        let stream = env
            .call_method(
                &resolver,
                "openInputStream",
                "(Landroid/net/Uri;)Ljava/io/InputStream;",
                &[(&uri).into()],
            )?
            .l()?;
        if stream.is_null() {
            return Ok(Vec::new());
        }
        let out = read_stream_fully(env, &stream);
        let _ = env.call_method(&stream, "close", "()V", &[]);
        out
    })
}

fn read_stream_fully(env: &mut jni::JNIEnv, stream: &JObject) -> jni::errors::Result<Vec<u8>> {
    const CHUNK: i32 = 64 * 1024;
    let buf = env.new_byte_array(CHUNK)?;
    let mut out = Vec::new();
    let mut tmp = vec![0i8; CHUNK as usize];
    loop {
        let n = env.call_method(stream, "read", "([B)I", &[(&buf).into()])?.i()?;
        if n < 0 {
            break;
        }
        if n > 0 {
            env.get_byte_array_region(&buf, 0, &mut tmp[..n as usize])?;
            out.extend(tmp[..n as usize].iter().map(|&b| b as u8));
        }
    }
    Ok(out)
}

fn notify(title: &str, body: &str) {
    with_activity(|env, activity| {
        let svc = env.new_string("notification")?;
        let nm = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&svc).into()],
            )?
            .l()?;
        // NotificationChannel (API 26+): id, name, IMPORTANCE_DEFAULT=3.
        let chan_id = env.new_string("egui")?;
        let chan_name = env.new_string("egui")?;
        let channel = env.new_object(
            "android/app/NotificationChannel",
            "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
            &[(&chan_id).into(), (&chan_name).into(), JValue::Int(3)],
        )?;
        env.call_method(
            &nm,
            "createNotificationChannel",
            "(Landroid/app/NotificationChannel;)V",
            &[(&channel).into()],
        )?;
        // App icon (required for a valid notification).
        let app_info = env
            .call_method(
                activity,
                "getApplicationInfo",
                "()Landroid/content/pm/ApplicationInfo;",
                &[],
            )?
            .l()?;
        let icon = env.get_field(&app_info, "icon", "I")?.i()?;

        let jtitle = env.new_string(title)?;
        let jbody = env.new_string(body)?;
        let builder = env.new_object(
            "android/app/Notification$Builder",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(activity), (&chan_id).into()],
        )?;
        env.call_method(
            &builder,
            "setContentTitle",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[(&jtitle).into()],
        )?;
        env.call_method(
            &builder,
            "setContentText",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[(&jbody).into()],
        )?;
        env.call_method(
            &builder,
            "setSmallIcon",
            "(I)Landroid/app/Notification$Builder;",
            &[JValue::Int(icon)],
        )?;
        env.call_method(
            &builder,
            "setAutoCancel",
            "(Z)Landroid/app/Notification$Builder;",
            &[JValue::Bool(1)],
        )?;
        let notif = env
            .call_method(&builder, "build", "()Landroid/app/Notification;", &[])?
            .l()?;
        env.call_method(
            &nm,
            "notify",
            "(ILandroid/app/Notification;)V",
            &[JValue::Int(1), (&notif).into()],
        )?;
        Ok(())
    });
}

// ── Permissions (poll-based; the proper callback path needs a Kotlin activity) ──

fn request_permission(index: Option<usize>, perm: &str) {
    with_activity(|env, activity| {
        let jperm = env.new_string(perm)?;
        let arr = env.new_object_array(1, "java/lang/String", &jperm)?;
        env.call_method(
            activity,
            "requestPermissions",
            "([Ljava/lang/String;I)V",
            &[(&arr).into(), JValue::Int(0)],
        )?;
        Ok(())
    });
    if let Some(idx) = index {
        if let Ok(mut p) = PENDING_PERMS.lock() {
            if !p.iter().any(|(i, _, _)| *i == idx) {
                p.push((idx, perm.to_string(), PERM_POLL_FRAMES));
            }
        }
    }
}

fn check_permission(perm: &str) -> Option<bool> {
    with_activity(|env, activity| {
        let jperm = env.new_string(perm)?;
        let r = env
            .call_method(
                activity,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
                &[(&jperm).into()],
            )?
            .i()?;
        Ok(r == 0)
    })
}

fn poll_pending_permissions(host: &Host) {
    let pending = match PENDING_PERMS.lock() {
        Ok(p) if !p.is_empty() => p.clone(),
        _ => return,
    };
    let mut still = Vec::new();
    for (idx, perm, frames) in pending {
        match check_permission(&perm) {
            Some(true) => host.drv_set_permission(idx, true),
            _ if frames == 0 => host.drv_set_permission(idx, false),
            _ => still.push((idx, perm, frames - 1)),
        }
    }
    if let Ok(mut p) = PENDING_PERMS.lock() {
        *p = still;
    }
}

/// Feed the app's private files directory into the host as its documents dir. Called once at
/// startup. `getFilesDir()` is a Context method (thread-safe, no UI thread needed).
pub fn init_documents_dir(host: &Host) {
    let dir = with_activity(|env, activity| {
        let files = env
            .call_method(activity, "getFilesDir", "()Ljava/io/File;", &[])?
            .l()?;
        let path = env
            .call_method(&files, "getAbsolutePath", "()Ljava/lang/String;", &[])?
            .l()?;
        let s: JString = path.into();
        let out: String = env.get_string(&s)?.into();
        Ok(out)
    });
    if let Some(dir) = dir {
        host.drv_set_documents_dir(dir);
    }
}

/// Read the current system-bar + display-cutout insets and push them (in points) into the host so
/// `host.safe_area_insets()` works on Android like on iOS. Called each frame by the runtime.
pub fn update_insets(host: &Host, pixels_per_point: f32) {
    let p = pixels_per_point.max(0.1);
    if let Some((t, b, l, r)) = read_root_insets_px() {
        host.drv_set_safe_area(t / p, b / p, l / p, r / p);
    }
    host.drv_set_keyboard_height(keyboard_pts(host, p));
}

/// Show or hide the soft keyboard. Explicit (non-implicit) calls so winit's implicit-only IME
/// hides don't cancel an app-requested keyboard. Prefers the hidden EditText on
/// `EguiNativeActivity` so Gboard attaches a real InputConnection (spacebar trackpad).
fn set_soft_keyboard(show: bool) {
    if crate::ime_bridge::set_soft_keyboard(show) {
        return;
    }
    let Some(app) = ANDROID_APP.get() else {
        log::warn!("egui-android: SetKeyboard before the AndroidApp handle is registered");
        return;
    };
    if show {
        app.show_soft_input(false);
    } else {
        app.hide_soft_input(false);
    }
}

/// Keep the soft keyboard up (e.g. while the text-actions bar is handling a tap).
/// Does not set [`KEYBOARD_REQUESTED`] — that flag is only for explicit `Host::request_keyboard`
/// (plugins); pinning it here left the text-actions bar stuck after click-away.
pub(crate) fn keep_soft_keyboard() {
    set_soft_keyboard(true);
}

// Keyboard occlusion in points: the WindowInsets IME inset when available, else the
// window-height/content-rect delta (needs windowSoftInputMode=adjustResize; the delta equals
// the nav-bar inset while the keyboard is hidden, so within 40pt of that reads as hidden).
fn keyboard_pts(host: &Host, pixels_per_point: f32) -> f32 {
    if let Some(px) = ime_inset_px() {
        return px / pixels_per_point;
    }
    let Some(app) = ANDROID_APP.get() else { return 0.0 };
    let Some(win) = app.native_window() else { return 0.0 };
    let raw = (win.height() as f32 - app.content_rect().bottom as f32).max(0.0) / pixels_per_point;
    if raw > host.safe_area_insets().bottom + 40.0 { raw } else { 0.0 }
}

// IME occlusion in px via `getCurrentWindowMetrics().getWindowInsets()` — WindowManager is a
// system service, not a View, so this is render-thread safe. Latches off on API < 30 or the
// first JNI failure and falls back to the content-rect path above.
fn ime_inset_px() -> Option<f32> {
    enum Probe {
        Px(f32),
        NotReady,
        Unsupported,
    }
    static IME_INSET_OFF: AtomicBool = AtomicBool::new(false);
    if IME_INSET_OFF.load(Ordering::Relaxed) {
        return None;
    }
    let probe = with_activity(|env, activity| {
        let sdk = env
            .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
            .i()?;
        if sdk < 30 {
            return Ok(Probe::Unsupported);
        }
        let wm = env
            .call_method(activity, "getWindowManager", "()Landroid/view/WindowManager;", &[])?
            .l()?;
        let metrics = env
            .call_method(&wm, "getCurrentWindowMetrics", "()Landroid/view/WindowMetrics;", &[])?
            .l()?;
        let insets = env
            .call_method(&metrics, "getWindowInsets", "()Landroid/view/WindowInsets;", &[])?
            .l()?;
        if insets.is_null() {
            return Ok(Probe::NotReady);
        }
        let ime = env
            .call_static_method("android/view/WindowInsets$Type", "ime", "()I", &[])?
            .i()?;
        let args = [JValue::Int(ime)];
        let obj = env
            .call_method(&insets, "getInsets", "(I)Landroid/graphics/Insets;", &args)?
            .l()?;
        Ok(Probe::Px(env.get_field(&obj, "bottom", "I")?.i()? as f32))
    });
    match probe {
        Some(Probe::Px(px)) => Some(px),
        Some(Probe::NotReady) => None,
        Some(Probe::Unsupported) | None => {
            IME_INSET_OFF.store(true, Ordering::Relaxed);
            None
        }
    }
}

// Insets are read via `Resources` (Context method, thread-safe) rather than the View hierarchy —
// `getRootWindowInsets`/`getDecorView` are View methods that MUST run on the UI thread and throw
// `CalledFromWrongThreadException` from the render thread. `status_bar_height` covers the top
// notch/camera region on virtually all phones; `navigation_bar_height` covers the bottom.
fn read_root_insets_px() -> Option<(f32, f32, f32, f32)> {
    with_activity(|env, activity| {
        let res = env
            .call_method(activity, "getResources", "()Landroid/content/res/Resources;", &[])?
            .l()?;
        let top = android_dimen_px(env, &res, "status_bar_height")?;
        let bottom = android_dimen_px(env, &res, "navigation_bar_height")?;
        Ok((top, bottom, 0.0, 0.0))
    })
}

/// Look up a framework `dimen` resource (e.g. `status_bar_height`) in pixels; 0 if absent.
fn android_dimen_px(
    env: &mut jni::JNIEnv,
    res: &JObject,
    name: &str,
) -> jni::errors::Result<f32> {
    let jname = env.new_string(name)?;
    let jtype = env.new_string("dimen")?;
    let jpkg = env.new_string("android")?;
    let id = env
        .call_method(
            res,
            "getIdentifier",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
            &[(&jname).into(), (&jtype).into(), (&jpkg).into()],
        )?
        .i()?;
    if id <= 0 {
        return Ok(0.0);
    }
    let px = env
        .call_method(res, "getDimensionPixelSize", "(I)I", &[JValue::Int(id)])?
        .i()?;
    Ok(px as f32)
}

fn haptic_ms(kind: i32) -> i64 {
    match kind {
        0 => 12,
        1 => 22,
        2 => 40,
        3 => 28,
        4 => 36,
        5 => 55,
        6 => 10,
        _ => 18,
    }
}

// ── Marquee: self-update, install/overlay permission ─────────────────────────

/// Install an APK the app has on disk, via `PackageInstaller.Session` (streams the bytes, so no
/// FileProvider is needed). The system shows its confirm dialog; the result broadcast is
/// best-effort (no receiver is registered without the Kotlin pass).
fn self_update(apk_path: &str) {
    let bytes = match std::fs::read(apk_path) {
        Ok(b) => b,
        Err(e) => {
            log::error!("self_update: reading {apk_path}: {e}");
            return;
        }
    };
    with_activity(|env, activity| {
        let pm = env
            .call_method(
                activity,
                "getPackageManager",
                "()Landroid/content/pm/PackageManager;",
                &[],
            )?
            .l()?;
        let installer = env
            .call_method(
                &pm,
                "getPackageInstaller",
                "()Landroid/content/pm/PackageInstaller;",
                &[],
            )?
            .l()?;
        let params = env.new_object(
            "android/content/pm/PackageInstaller$SessionParams",
            "(I)V",
            &[JValue::Int(1)], // MODE_FULL_INSTALL
        )?;
        let session_id = env
            .call_method(
                &installer,
                "createSession",
                "(Landroid/content/pm/PackageInstaller$SessionParams;)I",
                &[(&params).into()],
            )?
            .i()?;
        let session = env
            .call_method(
                &installer,
                "openSession",
                "(I)Landroid/content/pm/PackageInstaller$Session;",
                &[JValue::Int(session_id)],
            )?
            .l()?;
        let name = env.new_string("egui-update")?;
        let out = env
            .call_method(
                &session,
                "openWrite",
                "(Ljava/lang/String;JJ)Ljava/io/OutputStream;",
                &[(&name).into(), JValue::Long(0), JValue::Long(-1)],
            )?
            .l()?;
        for chunk in bytes.chunks(1 << 16) {
            let arr = env.byte_array_from_slice(chunk)?;
            env.call_method(
                &out,
                "write",
                "([BII)V",
                &[(&arr).into(), JValue::Int(0), JValue::Int(chunk.len() as i32)],
            )?;
        }
        env.call_method(
            &session,
            "fsync",
            "(Ljava/io/OutputStream;)V",
            &[(&out).into()],
        )?;
        env.call_method(&out, "close", "()V", &[])?;

        let act = env.new_string("com.egui.SELF_UPDATE")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[(&act).into()],
        )?;
        // FLAG_MUTABLE (1<<25) | FLAG_UPDATE_CURRENT (1<<27)
        let flags = 0x0200_0000 | 0x0800_0000;
        let pi = env
            .call_static_method(
                "android/app/PendingIntent",
                "getBroadcast",
                "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
                &[
                    JValue::Object(activity),
                    JValue::Int(0),
                    (&intent).into(),
                    JValue::Int(flags),
                ],
            )?
            .l()?;
        let sender = env
            .call_method(&pi, "getIntentSender", "()Landroid/content/IntentSender;", &[])?
            .l()?;
        env.call_method(
            &session,
            "commit",
            "(Landroid/content/IntentSender;)V",
            &[(&sender).into()],
        )?;
        Ok(())
    });
}

fn start_settings_for_package(action: &str) {
    with_activity(|env, activity| {
        let pkg = package_name(env, activity)?;
        let jaction = env.new_string(action)?;
        let uri_str = env.new_string(format!("package:{pkg}"))?;
        let uri = env
            .call_static_method(
                "android/net/Uri",
                "parse",
                "(Ljava/lang/String;)Landroid/net/Uri;",
                &[(&uri_str).into()],
            )?
            .l()?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;Landroid/net/Uri;)V",
            &[(&jaction).into(), (&uri).into()],
        )?;
        env.call_method(
            &intent,
            "addFlags",
            "(I)Landroid/content/Intent;",
            &[JValue::Int(0x1000_0000)],
        )?;
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&intent).into()],
        )?;
        Ok(())
    });
}

fn request_install_permission() {
    start_settings_for_package("android.settings.MANAGE_UNKNOWN_APP_SOURCES");
}

fn request_overlay_permission() {
    start_settings_for_package("android.settings.action.MANAGE_OVERLAY_PERMISSION");
}

fn jni_can_install_packages() -> Option<bool> {
    with_activity(|env, activity| {
        let pm = env
            .call_method(
                activity,
                "getPackageManager",
                "()Landroid/content/pm/PackageManager;",
                &[],
            )?
            .l()?;
        Ok(env
            .call_method(&pm, "canRequestPackageInstalls", "()Z", &[])?
            .z()?)
    })
}

fn jni_can_draw_overlays() -> Option<bool> {
    with_activity(|env, activity| {
        Ok(env
            .call_static_method(
                "android/provider/Settings",
                "canDrawOverlays",
                "(Landroid/content/Context;)Z",
                &[JValue::Object(activity)],
            )?
            .z()?)
    })
}

fn jni_version_code() -> Option<i64> {
    with_activity(|env, activity| {
        let pm = env
            .call_method(
                activity,
                "getPackageManager",
                "()Landroid/content/pm/PackageManager;",
                &[],
            )?
            .l()?;
        let pkg = package_name(env, activity)?;
        let jpkg = env.new_string(pkg)?;
        let info = env
            .call_method(
                &pm,
                "getPackageInfo",
                "(Ljava/lang/String;I)Landroid/content/pm/PackageInfo;",
                &[(&jpkg).into(), JValue::Int(0)],
            )?
            .l()?;
        Ok(env.call_method(&info, "getLongVersionCode", "()J", &[])?.j()?)
    })
}

/// Android-only host capabilities beyond the common cross-platform surface. Import with
/// `use egui_android::HostExt;`. On iOS these methods do not exist (compile error), so keep
/// Android-only calls behind `#[cfg(target_os = "android")]` in shared app code.
pub trait HostExt {
    /// Install an APK on disk (self-update / sideload). Requires `REQUEST_INSTALL_PACKAGES` and
    /// the same signing key + `versionCode >=` current. Shows the system confirm dialog.
    fn self_update(&self, apk_path: impl Into<String>);
    /// Open Settings to grant "install unknown apps" for this app.
    fn request_install_permission(&self);
    /// Whether this app may request package installs right now.
    fn can_install_packages(&self) -> bool;
    /// This app's `versionCode` (for update checks).
    fn current_version_code(&self) -> i64;
    /// Open Settings to grant "draw over other apps" (overlays).
    fn request_overlay_permission(&self);
    /// Whether this app may draw overlays right now.
    fn can_draw_overlays(&self) -> bool;
    /// Request the runtime `POST_NOTIFICATIONS` permission (Android 13+).
    fn request_notification_permission(&self);
    /// Copy an image file on disk into the shared Photos gallery (`Pictures/<subdir>`) via
    /// MediaStore. Scoped-storage insert — needs no runtime permission on Android 10+. `mime` is
    /// e.g. `"image/png"` or `"video/mp4"`.
    fn save_to_gallery(&self, path: impl Into<String>, display_name: impl Into<String>, mime: impl Into<String>);
    /// Insert an image/video file into MediaStore, then present the system share sheet for it.
    /// `mime` is e.g. `"image/png"` or `"video/mp4"`.
    fn share_media(&self, path: impl Into<String>, display_name: impl Into<String>, mime: impl Into<String>);
    /// Ask the user to grant photo-gallery access. Because `ndk_context` only exposes the
    /// Application (not the Activity) under android-activity 0.6, the runtime permission dialog
    /// can't be shown from here, so this opens the app's Settings page where the user toggles
    /// Photos access. Poll [`has_media_images_permission`](HostExt::has_media_images_permission)
    /// (checkable on the Application context) for the result on return.
    fn request_media_images_permission(&self);
    /// Whether this app may currently read the device photo gallery.
    fn has_media_images_permission(&self) -> bool;
    /// Recent device gallery images as `(MediaStore id, display name)`, newest first, capped at
    /// `limit`. Empty when the permission is denied or there are none.
    fn list_device_images(&self, limit: i32) -> Vec<(i64, String)>;
    /// A device image's thumbnail as raw RGBA pixels `(width, height, rgba)` (≈ `size`×`size`),
    /// or `None` on failure.
    fn load_device_thumbnail(&self, id: i64, size: i32) -> Option<(u32, u32, Vec<u8>)>;
    /// A device image's full file bytes (for upload), or `None` on failure.
    fn load_device_image(&self, id: i64) -> Option<Vec<u8>>;
    /// Current system clipboard text, if any (requires app focus).
    fn clipboard_text(&self) -> Option<String>;
    /// Whether the primary clip exists (no string copy — safe to poll every frame).
    fn clipboard_has_text(&self) -> bool {
        false
    }
}

impl HostExt for Host {
    fn self_update(&self, apk_path: impl Into<String>) {
        self.drv_enqueue(K_SELF_UPDATE, Some(apk_path.into()), None, 0);
    }
    fn request_install_permission(&self) {
        self.drv_enqueue(K_REQ_INSTALL_PERM, None, None, 0);
    }
    fn can_install_packages(&self) -> bool {
        jni_can_install_packages().unwrap_or(false)
    }
    fn current_version_code(&self) -> i64 {
        jni_version_code().unwrap_or(0)
    }
    fn request_overlay_permission(&self) {
        self.drv_enqueue(K_REQ_OVERLAY_PERM, None, None, 0);
    }
    fn can_draw_overlays(&self) -> bool {
        jni_can_draw_overlays().unwrap_or(false)
    }
    fn request_notification_permission(&self) {
        self.drv_enqueue(K_REQ_NOTIF_PERM, None, None, 0);
    }
    fn save_to_gallery(&self, path: impl Into<String>, display_name: impl Into<String>, mime: impl Into<String>) {
        // path in str_a, "name\tmime" in str_b (the request channel carries only two strings).
        let meta = format!("{}\t{}", display_name.into(), mime.into());
        self.drv_enqueue(K_SAVE_GALLERY, Some(path.into()), Some(meta), 0);
    }
    fn share_media(&self, path: impl Into<String>, display_name: impl Into<String>, mime: impl Into<String>) {
        // Same tab-packed meta as save_to_gallery: path in str_a, "name\tmime" in str_b.
        let meta = format!("{}\t{}", display_name.into(), mime.into());
        self.drv_enqueue(K_SHARE_MEDIA, Some(path.into()), Some(meta), 0);
    }
    fn request_media_images_permission(&self) {
        self.drv_enqueue(K_REQ_MEDIA_PERM, None, None, 0);
    }
    fn has_media_images_permission(&self) -> bool {
        jni_has_media_permission().unwrap_or(false)
    }
    fn list_device_images(&self, limit: i32) -> Vec<(i64, String)> {
        jni_list_device_images(limit).unwrap_or_default()
    }
    fn load_device_thumbnail(&self, id: i64, size: i32) -> Option<(u32, u32, Vec<u8>)> {
        jni_load_device_thumb(id, size)
    }
    fn load_device_image(&self, id: i64) -> Option<Vec<u8>> {
        jni_load_device_bytes(id)
    }
    fn clipboard_text(&self) -> Option<String> {
        read_clipboard_text()
    }
    fn clipboard_has_text(&self) -> bool {
        clipboard_has_text()
    }
}
