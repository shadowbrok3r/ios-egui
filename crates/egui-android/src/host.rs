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
// 104 was K_SAVE_GALLERY, now a synchronous call — it has to hand the folder back to its caller.
const K_REQ_MEDIA_PERM: i32 = 105;
const K_SHARE_MEDIA: i32 = 106;
const K_SET_ORIENTATION: i32 = 107;
const K_MEDIA_SCAN: i32 = 108;
const K_NOTIFY_PROGRESS: i32 = 109;
const K_INSTALL_CA: i32 = 110;
const K_OPEN_SETTINGS: i32 = 111;
const K_OPEN_APP_SETTINGS: i32 = 112;

/// `K_NOTIFY_PROGRESS` int payload: a percentage, or one of these.
const PROGRESS_INDETERMINATE: i32 = -1;
const PROGRESS_DISMISS: i32 = -2;

// ActivityInfo.SCREEN_ORIENTATION_* constants.
const SCREEN_ORIENTATION_UNSPECIFIED: i32 = -1;
const SCREEN_ORIENTATION_SENSOR_LANDSCAPE: i32 = 4;

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
            K_SHARE_MEDIA => {
                if let (Some(path), Some(meta)) = (host.drv_str_a(), host.drv_str_b()) {
                    let (name, mime) = meta.split_once('\t').unwrap_or((meta.as_str(), "image/png"));
                    share_media(&path, name, mime);
                }
            }
            K_REQ_MEDIA_PERM => request_media_permission(),
            K_OPEN_APP_SETTINGS => {
                start_settings_for_package("android.settings.APPLICATION_DETAILS_SETTINGS")
            }
            K_SET_ORIENTATION => jni_set_orientation(host.drv_int()),
            K_MEDIA_SCAN => {
                if let Some(path) = host.drv_str_a() {
                    media_scan(&path);
                }
            }
            K_NOTIFY_PROGRESS => notify_progress(
                &host.drv_str_a().unwrap_or_default(),
                &host.drv_str_b().unwrap_or_default(),
                host.drv_int(),
            ),
            K_OPEN_SETTINGS => {
                if let Some(action) = host.drv_str_a() {
                    open_settings_action(&action);
                }
            }
            K_INSTALL_CA => {
                if let Some(hex) = host.drv_str_a() {
                    let label = host.drv_str_b().unwrap_or_default();
                    match decode_hex(&hex) {
                        Some(der) => install_ca_certificate(&der, &label),
                        None => log::error!("install_ca_certificate: payload was not hex"),
                    }
                }
            }
            other => log::info!("egui-android: host request kind {other} not handled"),
        }
    }
}

/// How many local references a helper's frame reserves. A minimum, not a cap — JNI grows past it —
/// but it covers every helper here without a resize.
const JNI_FRAME: i32 = 16;

/// Run `f` with a JNIEnv attached to the current thread and the real `Activity`.
///
/// Prefer this over [`ndk_context`]: android-activity stores the `Application` there, not the
/// `Activity`, so `instanceof EguiNativeActivity` and Activity-only APIs fail.
///
/// The closure runs inside a **local frame**, which is load-bearing rather than tidy: the render
/// thread is attached for the whole process (android-activity attaches `android_main` and never
/// detaches), and `jni`'s object wrappers have no `Drop`, so every `new_string` / `new_object` /
/// returned object ref would otherwise stay a GC root until exit. The thumbnail path alone leaks a
/// `Bitmap` plus a `w*h` `int[]` per tile that way. Nothing may return a `JObject` out of `f` —
/// popping the frame frees it.
pub(crate) fn with_native_activity<R>(
    f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<R>,
) -> Option<R> {
    let app = ANDROID_APP.get()?;
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    // Unowned JNI global ref from AndroidApp — must not DeleteLocalRef on drop. Made before the
    // frame so popping it leaves the reference alone.
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let out = match env.with_local_frame::<_, R, jni::errors::Error>(JNI_FRAME, |env| {
        f(env, &activity)
    }) {
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
    // Framed for the same reason as above — see `with_native_activity`.
    let out = match env.with_local_frame::<_, R, jni::errors::Error>(JNI_FRAME, |env| {
        f(env, &activity)
    }) {
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

/// MediaStore collection and relative path for a MIME.
///
/// The images collection rejects anything that is not `image/*` — MediaProvider throws
/// `IllegalArgumentException` — so a certificate, a log or a backup has to go to Downloads, which
/// accepts any type. Everything non-media used to be inserted as an image and failed silently.
fn media_target(mime: &str) -> (&'static str, &'static str) {
    if mime.starts_with("image/") {
        ("android/provider/MediaStore$Images$Media", "Pictures/ComfyUI")
    } else if mime.starts_with("video/") {
        ("android/provider/MediaStore$Video$Media", "Movies/ComfyUI")
    } else {
        ("android/provider/MediaStore$Downloads", "Download")
    }
}

/// Insert `path`'s bytes into MediaStore — the gallery for media, Downloads for everything else —
/// and return the `content://` URI. Scoped-storage insert (API 29+), so no runtime storage
/// permission is needed.
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

    let (collection_class, relative_path) = media_target(mime);

    let values = env.new_object("android/content/ContentValues", "()V", &[])?;
    for (key, val) in
        [("_display_name", name), ("mime_type", mime), ("relative_path", relative_path)]
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
        .get_static_field(collection_class, "EXTERNAL_CONTENT_URI", "Landroid/net/Uri;")?
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

/// Copy `path`'s bytes into the shared gallery via MediaStore, and report the folder it landed in
/// (`Pictures/ComfyUI`, `Movies/ComfyUI` or `Download`, per the MIME) so the caller can name it.
/// `None` when the insert failed — the caller's own copy on disk is unaffected either way.
///
/// Synchronous rather than queued through `drv_enqueue`: the caller needs the folder back, and a
/// ContentResolver insert has no UI-thread requirement (the device-media reads below do the same).
fn save_to_gallery(path: &str, name: &str, mime: &str) -> Option<String> {
    let folder = media_target(mime).1;
    let ok = with_activity(|env, activity| {
        let uri = insert_into_media_store(env, activity, path, name, mime)?;
        Ok(!uri.is_null())
    });
    match ok {
        Some(true) => {
            log::info!("save_to_gallery: {name} -> {folder}");
            Some(folder.to_string())
        }
        Some(false) => None,
        None => {
            log::error!("save_to_gallery: JNI call failed for {name}");
            None
        }
    }
}

/// Read and clear the installer's outcome latch: `0` nothing since the last drain, `1` installed,
/// `2` failed. See `EguiNativeActivity.takeInstallStatus`, which the commit broadcast feeds.
fn jni_take_install_status() -> Option<i32> {
    with_native_activity(|env, activity| {
        env.call_method(activity, "takeInstallStatus", "()I", &[])?.i()
    })
}

/// Why the last install failed (the system's `EXTRA_STATUS_MESSAGE`), or empty.
fn jni_install_message() -> Option<String> {
    with_native_activity(|env, activity| {
        let msg = env
            .call_method(activity, "getInstallMessage", "()Ljava/lang/String;", &[])?
            .l()?;
        if msg.is_null() {
            return Ok(String::new());
        }
        let s: JString = msg.into();
        Ok(env.get_string(&s)?.into())
    })
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

/// Hand `path` to MediaProvider for a rescan. A directory holding `.nomedia` drops the Photos
/// entries it was indexed under. Best-effort; failures are logged and swallowed.
fn media_scan(path: &str) {
    let done = with_activity(|env, activity| {
        let jpath = env.new_string(path)?;
        let paths = env.new_object_array(1, "java/lang/String", &jpath)?;
        let null = JObject::null();
        env.call_static_method(
            "android/media/MediaScannerConnection",
            "scanFile",
            "(Landroid/content/Context;[Ljava/lang/String;[Ljava/lang/String;Landroid/media/MediaScannerConnection$OnScanCompletedListener;)V",
            &[
                JValue::Object(activity),
                (&paths).into(),
                (&null).into(),
                (&null).into(),
            ],
        )?;
        log::info!("media_scan: {path}");
        Ok(())
    });
    if done.is_none() {
        log::error!("media_scan: JNI call failed for {path}");
    }
}

/// Hand a DER-encoded certificate to Android's own certificate installer.
///
/// **Android 11+ refuses CA certificates through this path**: `CertInstaller` answers with
/// "Can't install CA certificates — this certificate must be installed in Settings" unless the
/// caller is a device or profile owner. Verified on Android 17. It remains the right call for a
/// *client* certificate, and for CA installs below API 30 — for a CA on a modern device, write the
/// file (which now lands in `Download/`) and send the user to Settings.
fn install_ca_certificate(der: &[u8], label: &str) {
    let done = with_activity(|env, activity| {
        let intent = env
            .call_static_method(
                "android/security/KeyChain",
                "createInstallIntent",
                "()Landroid/content/Intent;",
                &[],
            )?
            .l()?;

        let extra_certificate = env
            .get_static_field(
                "android/security/KeyChain",
                "EXTRA_CERTIFICATE",
                "Ljava/lang/String;",
            )?
            .l()?;
        let bytes = env.byte_array_from_slice(der)?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;[B)Landroid/content/Intent;",
            &[(&extra_certificate).into(), (&bytes).into()],
        )?;

        if !label.is_empty() {
            let extra_name = env
                .get_static_field("android/security/KeyChain", "EXTRA_NAME", "Ljava/lang/String;")?
                .l()?;
            let jlabel = env.new_string(label)?;
            env.call_method(
                &intent,
                "putExtra",
                "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
                &[(&extra_name).into(), (&jlabel).into()],
            )?;
        }

        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&intent).into()],
        )?;
        Ok(())
    });

    if done.is_none() {
        log::error!("install_ca_certificate: JNI call failed");
    }
}

/// Open a system Settings screen by action, e.g. `android.settings.SECURITY_SETTINGS`.
fn open_settings_action(action: &str) {
    let done = with_activity(|env, activity| {
        let jaction = env.new_string(action)?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[(&jaction).into()],
        )?;
        // FLAG_ACTIVITY_NEW_TASK, so this also works from the Application context.
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

    if done.is_none() {
        log::error!("open_settings: no activity answered {action}");
    }
}

/// Bytes from a lowercase hex string. The request queue carries strings, and a certificate is
/// binary.
fn decode_hex(input: &str) -> Option<Vec<u8>> {
    if input.len() % 2 != 0 {
        return None;
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(input.len() / 2);
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    Some(out)
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
        // MediaProvider renames a file whose extension disagrees with its MIME, so anything the
        // app shares by name needs its real type here.
        "crt" | "cer" | "der" => "application/x-x509-ca-cert",
        "pem" => "application/x-pem-file",
        "json" => "application/json",
        // Deliberately NOT application/json: MediaProvider rewrites the extension to the one
        // canonical for the MIME, which turned `.har` into `.har.json` and hid the file from
        // DevTools' Import HAR picker. Android knows no MIME for .har, so octet-stream is what
        // leaves the name alone.
        "har" => "application/octet-stream",
        "txt" | "log" => "text/plain",
        _ => "application/octet-stream",
    };
    (name, mime)
}

fn sdk_int() -> i32 {
    with_activity(|env, _| env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i())
        .unwrap_or(0)
}

/// The runtime permission that gates reading the shared gallery: scoped `READ_MEDIA_IMAGES` /
/// `READ_MEDIA_VIDEO` on Android 13+ (API 33), the broad `READ_EXTERNAL_STORAGE` below.
fn media_permission(video: bool) -> &'static str {
    if sdk_int() >= 33 {
        if video {
            "android.permission.READ_MEDIA_VIDEO"
        } else {
            "android.permission.READ_MEDIA_IMAGES"
        }
    } else {
        "android.permission.READ_EXTERNAL_STORAGE"
    }
}

/// Whether the gallery can be read at all — including through Android 14's partial "Select photos"
/// grant, which denies the full permission and grants `READ_MEDIA_VISUAL_USER_SELECTED` instead.
/// MediaStore then lists only the hand-picked items rather than refusing the query.
fn jni_has_media_permission(video: bool) -> Option<bool> {
    if check_permission(media_permission(video)) == Some(true) {
        return Some(true);
    }
    if sdk_int() >= 34 {
        return check_permission("android.permission.READ_MEDIA_VISUAL_USER_SELECTED");
    }
    Some(false)
}

/// Whether the whole gallery is readable, as opposed to the partial Android 14+ selection. Drives
/// the "Select more" affordance: re-asking is the only way to reopen the system's photo chooser.
fn jni_has_full_media_permission(video: bool) -> Option<bool> {
    check_permission(media_permission(video))
}

/// MediaStore collection for the kind being browsed.
fn media_collection(video: bool) -> &'static str {
    if video {
        "android/provider/MediaStore$Video$Media"
    } else {
        "android/provider/MediaStore$Images$Media"
    }
}

/// List the most recent device gallery entries as `(MediaStore id, display name)`, newest first,
/// capped at `limit`. `video` picks the clips collection instead of the stills one.
fn jni_list_device_media(video: bool, limit: i32) -> Option<Vec<(i64, String)>> {
    with_activity(|env, activity| {
        let resolver = env
            .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
            .l()?;
        let collection = env
            .get_static_field(media_collection(video), "EXTERNAL_CONTENT_URI", "Landroid/net/Uri;")?
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

/// `content://` URI for a MediaStore id in the stills or clips collection.
fn media_uri<'a>(
    env: &mut jni::JNIEnv<'a>,
    video: bool,
    id: i64,
) -> jni::errors::Result<JObject<'a>> {
    let base = env
        .get_static_field(media_collection(video), "EXTERNAL_CONTENT_URI", "Landroid/net/Uri;")?
        .l()?;
    env.call_static_method(
        "android/content/ContentUris",
        "withAppendedId",
        "(Landroid/net/Uri;J)Landroid/net/Uri;",
        &[(&base).into(), JValue::Long(id)],
    )?
    .l()
}

/// A device image or video frame's thumbnail as raw RGBA pixels `(width, height, rgba)`
/// (≈ `size`×`size`). `loadThumbnail` needs API 29+ and covers both collections. Returns pixels
/// directly (no PNG round-trip) so the caller can build a texture without a re-decode.
fn jni_load_device_thumb(video: bool, id: i64, size: i32) -> Option<(u32, u32, Vec<u8>)> {
    with_activity(|env, activity| {
        let resolver = env
            .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
            .l()?;
        let uri = media_uri(env, video, id)?;
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
        let out = bitmap_to_rgba(env, &bitmap);
        // The pixels are copied out by now, and a grid pulls hundreds of these: recycle frees the
        // native buffer immediately rather than leaving it to the next GC.
        let _ = env.call_method(&bitmap, "recycle", "()V", &[]);
        out
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

/// A device image or video's full file bytes (for upload).
fn jni_load_device_bytes(video: bool, id: i64) -> Option<Vec<u8>> {
    with_activity(|env, activity| {
        let resolver = env
            .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
            .l()?;
        let uri = media_uri(env, video, id)?;
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

/// The ongoing job notification: a persistent row in the shade with a progress bar, the way a
/// music player keeps its transport there. Its own notification id and channel, so posting and
/// updating it never disturbs the one-shot [`notify`] messages.
///
/// `percent` is 0..=100, or [`PROGRESS_INDETERMINATE`] for a job with no measurable progress yet,
/// or [`PROGRESS_DISMISS`] to take the row away.
///
/// The channel is `IMPORTANCE_LOW` and the builder sets `setOnlyAlertOnce`, because this is posted
/// again on every step — at DEFAULT importance a 30-step render would buzz thirty times.
fn notify_progress(title: &str, body: &str, percent: i32) {
    const IMPORTANCE_LOW: i32 = 2;
    // PendingIntent.FLAG_UPDATE_CURRENT | FLAG_IMMUTABLE (required from Android 12).
    const PI_FLAGS: i32 = 0x0800_0000 | 0x0400_0000;
    const NOTIF_ID: i32 = 2;

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
        if percent == PROGRESS_DISMISS {
            env.call_method(&nm, "cancel", "(I)V", &[JValue::Int(NOTIF_ID)])?;
            return Ok(());
        }

        let chan_id = env.new_string("egui_progress")?;
        let chan_name = env.new_string("Progress")?;
        let channel = env.new_object(
            "android/app/NotificationChannel",
            "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
            &[(&chan_id).into(), (&chan_name).into(), JValue::Int(IMPORTANCE_LOW)],
        )?;
        env.call_method(
            &nm,
            "createNotificationChannel",
            "(Landroid/app/NotificationChannel;)V",
            &[(&channel).into()],
        )?;

        let app_info = env
            .call_method(
                activity,
                "getApplicationInfo",
                "()Landroid/content/pm/ApplicationInfo;",
                &[],
            )?
            .l()?;
        let icon = env.get_field(&app_info, "icon", "I")?.i()?;

        let builder = env.new_object(
            "android/app/Notification$Builder",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(activity), (&chan_id).into()],
        )?;
        let jtitle = env.new_string(title)?;
        let jbody = env.new_string(body)?;
        let jcat = env.new_string("progress")?;
        let b = "Landroid/app/Notification$Builder;";
        env.call_method(
            &builder,
            "setContentTitle",
            format!("(Ljava/lang/CharSequence;){b}"),
            &[(&jtitle).into()],
        )?;
        env.call_method(
            &builder,
            "setContentText",
            format!("(Ljava/lang/CharSequence;){b}"),
            &[(&jbody).into()],
        )?;
        env.call_method(&builder, "setSmallIcon", format!("(I){b}"), &[JValue::Int(icon)])?;
        env.call_method(&builder, "setCategory", format!("(Ljava/lang/String;){b}"), &[(&jcat).into()])?;
        // Ongoing + not auto-cancel: the row can't be swiped away while the job runs, which is
        // what makes it read as a transport rather than a message.
        env.call_method(&builder, "setOngoing", format!("(Z){b}"), &[JValue::Bool(1)])?;
        env.call_method(&builder, "setAutoCancel", format!("(Z){b}"), &[JValue::Bool(0)])?;
        env.call_method(&builder, "setOnlyAlertOnce", format!("(Z){b}"), &[JValue::Bool(1)])?;
        env.call_method(&builder, "setShowWhen", format!("(Z){b}"), &[JValue::Bool(0)])?;
        let indeterminate = i32::from(percent < 0);
        env.call_method(
            &builder,
            "setProgress",
            format!("(IIZ){b}"),
            &[JValue::Int(100), JValue::Int(percent.clamp(0, 100)), JValue::Bool(indeterminate as u8)],
        )?;

        // Tap returns to the app, like tapping a player's notification reopens it.
        let pm = env
            .call_method(activity, "getPackageManager", "()Landroid/content/pm/PackageManager;", &[])?
            .l()?;
        let pkg = env
            .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
            .l()?;
        let launch = env
            .call_method(
                &pm,
                "getLaunchIntentForPackage",
                "(Ljava/lang/String;)Landroid/content/Intent;",
                &[(&pkg).into()],
            )?
            .l()?;
        if !launch.is_null() {
            let pi = env
                .call_static_method(
                    "android/app/PendingIntent",
                    "getActivity",
                    "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
                    &[
                        JValue::Object(activity),
                        JValue::Int(0),
                        (&launch).into(),
                        JValue::Int(PI_FLAGS),
                    ],
                )?
                .l()?;
            env.call_method(
                &builder,
                "setContentIntent",
                format!("(Landroid/app/PendingIntent;){b}"),
                &[(&pi).into()],
            )?;
        }

        let notif = env
            .call_method(&builder, "build", "()Landroid/app/Notification;", &[])?
            .l()?;
        env.call_method(
            &nm,
            "notify",
            "(ILandroid/app/Notification;)V",
            &[JValue::Int(NOTIF_ID), (&notif).into()],
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

// The WindowMetrics IME-inset probe is unavailable (API < 30 or a JNI failure); the same
// boundary gates the Java insets-listener dismissal signal.
static IME_INSET_OFF: AtomicBool = AtomicBool::new(false);

/// Whether inset-based IME signals (occlusion + external-dismissal edge) work on this device.
pub(crate) fn ime_inset_reliable() -> bool {
    !IME_INSET_OFF.load(Ordering::Relaxed)
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

/// Call `Activity.setRequestedOrientation(value)`.
fn jni_set_orientation(value: i32) {
    with_native_activity(|env, activity| {
        env.call_method(activity, "setRequestedOrientation", "(I)V", &[JValue::Int(value)])?;
        Ok(())
    });
}

/// Gravity-sensor roll in degrees, sampled once per call. Returns `None` on non-Android or when
/// the sensor is unavailable. Roll ≈ 0/±180 → portrait; ≈ ±90 → landscape.
pub fn device_orientation_deg() -> Option<f32> {
    with_native_activity(|env, activity| {
        // Context.getSystemService("window") → WindowManager.getDefaultDisplay() → Display.getRotation()
        // Rotation: 0 = portrait, 1 = landscape, 2 = reverse-portrait, 3 = reverse-landscape.
        let wm_str = env.new_string("window")?;
        let wm = env
            .call_method(activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[(&wm_str).into()])?
            .l()?;
        let display = env
            .call_method(&wm, "getDefaultDisplay", "()Landroid/view/Display;", &[])?
            .l()?;
        let rotation = env.call_method(&display, "getRotation", "()I", &[])?.i()?;
        // Map Android rotation constant to approximate degree so the app can detect portrait.
        let deg = match rotation {
            0 => 0.0_f32,
            1 => 90.0,
            2 => 180.0,
            3 => 270.0,
            _ => 0.0,
        };
        Ok(deg)
    })
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
        // Explicit, via the package. FLAG_MUTABLE is mandatory here — PackageInstaller delivers
        // EXTRA_STATUS / EXTRA_STATUS_MESSAGE / EXTRA_INTENT as fill-in extras, which an immutable
        // PendingIntent drops — and from targetSdk 34 the system *throws* on a mutable PendingIntent
        // whose Intent names neither a component nor a package, which killed the whole commit.
        let pkg = package_name(env, activity)?;
        let jpkg = env.new_string(pkg)?;
        env.call_method(
            &intent,
            "setPackage",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[(&jpkg).into()],
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

/// Raise the system's photos-and-videos permission dialog.
///
/// The permission list and the dialog call live in `EguiNativeActivity.requestMediaPermissions`,
/// which marshals onto the UI thread — `requestPermissions` runs through `startActivityForResult`,
/// which the render thread must not drive. An app on a plain `NativeActivity` has no such method,
/// so this falls back to the Settings page, which is also where a twice-denied permission has to be
/// granted from.
fn request_media_permission() {
    let asked = with_native_activity(|env, activity| {
        env.call_method(activity, "requestMediaPermissions", "()V", &[])?;
        Ok(())
    });
    if asked.is_none() {
        log::warn!("request_media_permission: no dialog, falling back to app settings");
        start_settings_for_package("android.settings.APPLICATION_DETAILS_SETTINGS");
    }
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

/// Whether the device reports a built-in stylus: Samsung S-Pen (`com.sec.feature.spen_usp`) or the
/// standard bundled-stylus feature (`android.hardware.stylus`, API 34+).
fn jni_has_stylus() -> Option<bool> {
    with_activity(|env, activity| {
        let pm = env
            .call_method(
                activity,
                "getPackageManager",
                "()Landroid/content/pm/PackageManager;",
                &[],
            )?
            .l()?;
        for feat in ["com.sec.feature.spen_usp", "android.hardware.stylus"] {
            let jfeat = env.new_string(feat)?;
            let has = env
                .call_method(&pm, "hasSystemFeature", "(Ljava/lang/String;)Z", &[(&jfeat).into()])?
                .z()?;
            if has {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

/// `ApplicationInfo.nativeLibraryDir`: the APK's extracted lib dir, the only path a non-rooted
/// device can `dlopen` bundled `.so` from.
fn jni_native_lib_dir() -> Option<String> {
    with_activity(|env, activity| {
        let app_info = env
            .call_method(activity, "getApplicationInfo", "()Landroid/content/pm/ApplicationInfo;", &[])?
            .l()?;
        let dir = env.get_field(&app_info, "nativeLibraryDir", "Ljava/lang/String;")?.l()?;
        let s: JString = dir.into();
        Ok(env.get_string(&s)?.into())
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

/// `versionCode` of an arbitrary installed package; the NameNotFoundException of a package that
/// is not installed surfaces as `None` via `with_activity`'s exception handling. Seeing other
/// packages on API 30+ needs `QUERY_ALL_PACKAGES` (or a `<queries>` entry) in the caller's APK.
fn jni_installed_version_code(pkg: &str) -> Option<i64> {
    with_activity(|env, activity| {
        let pm = env
            .call_method(
                activity,
                "getPackageManager",
                "()Landroid/content/pm/PackageManager;",
                &[],
            )?
            .l()?;
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

/// Screen-orientation request.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScreenOrientation {
    /// Let the system decide (back to normal).
    Unspecified,
    /// Lock to landscape (sensor-allowed flip between natural and reverse).
    Landscape,
}

/// Android-only host capabilities beyond the common cross-platform surface. Import with
/// `use egui_android::HostExt;`. On iOS these methods do not exist (compile error), so keep
/// Android-only calls behind `#[cfg(target_os = "android")]` in shared app code.
pub trait HostExt {
    /// Lock or release the screen orientation. See [`ScreenOrientation`].
    fn set_screen_orientation(&self, o: ScreenOrientation);
    /// Install an APK on disk (self-update / sideload). Requires `REQUEST_INSTALL_PACKAGES` and
    /// the same signing key + `versionCode >=` current.
    ///
    /// Asynchronous, and the confirm dialog is not raised by the system: the session reports
    /// `STATUS_PENDING_USER_ACTION` to a broadcast that `EguiNativeActivity`'s receiver turns into
    /// the dialog, then latches the outcome for
    /// [`take_install_status`](HostExt::take_install_status). An app on a plain `NativeActivity`
    /// gets no dialog and no result.
    fn self_update(&self, apk_path: impl Into<String>);
    /// Open Settings to grant "install unknown apps" for this app.
    fn request_install_permission(&self);
    /// Whether this app may request package installs right now.
    fn can_install_packages(&self) -> bool;
    /// This app's `versionCode` (for update checks).
    fn current_version_code(&self) -> i64;
    /// `versionCode` of another installed package, `None` when it is not installed (or not
    /// visible — API 30+ package visibility needs `QUERY_ALL_PACKAGES` or a `<queries>` entry).
    fn installed_version_code(&self, package: &str) -> Option<i64> {
        let _ = package;
        None
    }
    /// Open Settings to grant "draw over other apps" (overlays).
    fn request_overlay_permission(&self);
    /// Whether this app may draw overlays right now.
    fn can_draw_overlays(&self) -> bool;
    /// Request the runtime `POST_NOTIFICATIONS` permission (Android 13+).
    fn request_notification_permission(&self);
    /// Copy a media file on disk into the shared gallery via MediaStore, returning the folder it
    /// landed in (`Pictures/ComfyUI`, `Movies/ComfyUI` or `Download`, chosen by `mime`) or `None`
    /// if the insert failed. Scoped-storage insert — needs no runtime permission on Android 10+.
    /// `mime` is e.g. `"image/png"` or `"video/mp4"`, and must match the file: MediaProvider
    /// renames anything whose extension disagrees with it.
    fn save_to_gallery(
        &self,
        path: impl Into<String>,
        display_name: impl Into<String>,
        mime: impl Into<String>,
    ) -> Option<String>;
    /// Insert an image/video file into MediaStore, then present the system share sheet for it.
    /// `mime` is e.g. `"image/png"` or `"video/mp4"`.
    fn share_media(&self, path: impl Into<String>, display_name: impl Into<String>, mime: impl Into<String>);
    /// Rescan a file or directory into MediaStore. A directory holding `.nomedia` loses the Photos
    /// entries an earlier scan gave it, so this is how an app un-publishes its own image folder.
    fn media_scan(&self, path: impl Into<String>);
    /// Post or update the ongoing job notification — a persistent shade row with a progress bar,
    /// like a music player's transport. `percent` is clamped to 0..=100; `None` shows an
    /// indeterminate bar for a job whose length isn't known yet. Tapping the row reopens the app.
    /// Call [`notify_progress_done`](HostExt::notify_progress_done) to take it away.
    ///
    /// Silent by design (its own `IMPORTANCE_LOW` channel, alert-once), so calling it per step
    /// costs nothing. Needs `POST_NOTIFICATIONS` on Android 13+ — see
    /// [`request_notification_permission`](HostExt::request_notification_permission).
    fn notify_progress(&self, title: impl Into<String>, body: impl Into<String>, percent: Option<u32>);
    /// Remove the ongoing job notification.
    fn notify_progress_done(&self);
    /// Open Android's certificate installer on a DER-encoded certificate. `label` pre-fills the
    /// name the system dialog asks for.
    ///
    /// Android 11+ **refuses CA certificates** here unless the app is a device or profile owner;
    /// it shows "Can't install CA certificates". Use it for client certificates, and send CA
    /// installs through a file plus [`open_settings`](HostExt::open_settings).
    fn install_ca_certificate(&self, der: &[u8], label: impl Into<String>);
    /// Open a Settings screen by action string, e.g. `android.settings.SECURITY_SETTINGS`.
    fn open_settings(&self, action: impl Into<String>);
    /// Raise the system's photos-and-videos permission dialog (both kinds, plus Android 14's
    /// partial-access permission). There is no result callback — poll
    /// [`has_media_permission`](HostExt::has_media_permission) for the answer. Android stops
    /// showing the dialog after two denials, so pair this with
    /// [`open_app_settings`](HostExt::open_app_settings).
    fn request_media_images_permission(&self);
    /// Open this app's page in Settings, where a twice-denied permission can still be granted.
    fn open_app_settings(&self);
    /// Whether the device gallery can be read at all — `true` under Android 14's partial
    /// "Select photos" grant too, which lists only the hand-picked items.
    fn has_media_permission(&self, video: bool) -> bool;
    /// Whether the *whole* gallery is readable, as opposed to Android 14's partial selection.
    /// When this is false but [`has_media_permission`](HostExt::has_media_permission) is true,
    /// re-requesting is what reopens the system's photo chooser.
    fn has_full_media_permission(&self, video: bool) -> bool;
    /// Recent device gallery entries as `(MediaStore id, display name)`, newest first, capped at
    /// `limit`. `video` lists clips instead of stills. Empty when denied or there are none.
    fn list_device_media(&self, video: bool, limit: i32) -> Vec<(i64, String)>;
    /// A device image or video's thumbnail as raw RGBA pixels `(width, height, rgba)`
    /// (≈ `size`×`size`), or `None` on failure.
    fn load_device_thumbnail(&self, video: bool, id: i64, size: i32) -> Option<(u32, u32, Vec<u8>)>;
    /// A device image or video's full file bytes (for upload), or `None` on failure.
    fn load_device_media(&self, video: bool, id: i64) -> Option<Vec<u8>>;
    /// Read and clear the outcome of a [`self_update`](HostExt::self_update): `0` nothing has
    /// happened since the last call, `1` installed, anything else failed — the install is
    /// asynchronous and the user still has a system dialog to accept, so the answer arrives frames
    /// or minutes later. Read [`install_message`](HostExt::install_message) after a failure.
    fn take_install_status(&self) -> i32;
    /// Why the last install failed — the system's own `EXTRA_STATUS_MESSAGE`, which names
    /// `INSTALL_FAILED_UPDATE_INCOMPATIBLE` for the signing-key mismatch — or empty.
    fn install_message(&self) -> String;
    /// Current system clipboard text, if any (requires app focus).
    fn clipboard_text(&self) -> Option<String>;
    /// Whether the primary clip exists (no string copy — safe to poll every frame).
    fn clipboard_has_text(&self) -> bool {
        false
    }
    /// Whether the device reports a built-in stylus (Samsung S-Pen or the standard bundled-stylus
    /// feature). Device capability only: the winit input path forwards touch pressure but not
    /// per-event tool type, so this is not live pointer data.
    fn has_stylus(&self) -> bool {
        false
    }
    /// This app's `ApplicationInfo.nativeLibraryDir` — the APK-bundled lib dir. `None` off-device.
    /// The only directory bundled `.so` (e.g. QNN HTP libs) can be `dlopen`'d from on non-rooted
    /// Android.
    fn native_lib_dir(&self) -> Option<String> {
        None
    }
    /// Live pointer probe from the android-activity input side channel, sampled at motion-event
    /// dispatch (winit drops tool type + hover). Returns `(tool, hover_px, buttons)`: `tool` is
    /// 0 unknown / 1 finger / 2 stylus / 3 mouse / 4 eraser / 5 palm; `hover_px` is the hover
    /// position in window physical pixels (pen near, not touching); `buttons` is the raw
    /// button-state bitfield (stylus-primary `0x20`, stylus-secondary `0x40`).
    fn stylus_probe(&self) -> (u8, Option<(f32, f32)>, u32) {
        (0, None, 0)
    }
}

impl HostExt for Host {
    fn set_screen_orientation(&self, o: ScreenOrientation) {
        let v = match o {
            ScreenOrientation::Unspecified => SCREEN_ORIENTATION_UNSPECIFIED,
            ScreenOrientation::Landscape => SCREEN_ORIENTATION_SENSOR_LANDSCAPE,
        };
        self.drv_enqueue(K_SET_ORIENTATION, None, None, v);
    }

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
    fn installed_version_code(&self, package: &str) -> Option<i64> {
        jni_installed_version_code(package)
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
    fn save_to_gallery(
        &self,
        path: impl Into<String>,
        display_name: impl Into<String>,
        mime: impl Into<String>,
    ) -> Option<String> {
        save_to_gallery(&path.into(), &display_name.into(), &mime.into())
    }
    fn share_media(&self, path: impl Into<String>, display_name: impl Into<String>, mime: impl Into<String>) {
        // Same tab-packed meta as save_to_gallery: path in str_a, "name\tmime" in str_b.
        let meta = format!("{}\t{}", display_name.into(), mime.into());
        self.drv_enqueue(K_SHARE_MEDIA, Some(path.into()), Some(meta), 0);
    }
    fn media_scan(&self, path: impl Into<String>) {
        self.drv_enqueue(K_MEDIA_SCAN, Some(path.into()), None, 0);
    }
    fn notify_progress(&self, title: impl Into<String>, body: impl Into<String>, percent: Option<u32>) {
        let pct = percent.map_or(PROGRESS_INDETERMINATE, |p| p.min(100) as i32);
        self.drv_enqueue(K_NOTIFY_PROGRESS, Some(title.into()), Some(body.into()), pct);
    }
    fn notify_progress_done(&self) {
        self.drv_enqueue(K_NOTIFY_PROGRESS, None, None, PROGRESS_DISMISS);
    }
    fn install_ca_certificate(&self, der: &[u8], label: impl Into<String>) {
        // The request queue carries strings; hex keeps the certificate intact across it.
        let hex: String = der.iter().map(|byte| format!("{byte:02x}")).collect();
        self.drv_enqueue(K_INSTALL_CA, Some(hex), Some(label.into()), 0);
    }
    fn open_settings(&self, action: impl Into<String>) {
        self.drv_enqueue(K_OPEN_SETTINGS, Some(action.into()), None, 0);
    }
    fn request_media_images_permission(&self) {
        self.drv_enqueue(K_REQ_MEDIA_PERM, None, None, 0);
    }
    fn open_app_settings(&self) {
        self.drv_enqueue(K_OPEN_APP_SETTINGS, None, None, 0);
    }
    fn has_media_permission(&self, video: bool) -> bool {
        jni_has_media_permission(video).unwrap_or(false)
    }
    fn has_full_media_permission(&self, video: bool) -> bool {
        jni_has_full_media_permission(video).unwrap_or(false)
    }
    fn list_device_media(&self, video: bool, limit: i32) -> Vec<(i64, String)> {
        jni_list_device_media(video, limit).unwrap_or_default()
    }
    fn load_device_thumbnail(&self, video: bool, id: i64, size: i32) -> Option<(u32, u32, Vec<u8>)> {
        jni_load_device_thumb(video, id, size)
    }
    fn load_device_media(&self, video: bool, id: i64) -> Option<Vec<u8>> {
        jni_load_device_bytes(video, id)
    }
    fn take_install_status(&self) -> i32 {
        jni_take_install_status().unwrap_or(0)
    }
    fn install_message(&self) -> String {
        jni_install_message().unwrap_or_default()
    }
    fn clipboard_text(&self) -> Option<String> {
        read_clipboard_text()
    }
    fn clipboard_has_text(&self) -> bool {
        clipboard_has_text()
    }
    fn has_stylus(&self) -> bool {
        jni_has_stylus().unwrap_or(false)
    }
    fn native_lib_dir(&self) -> Option<String> {
        jni_native_lib_dir()
    }
    fn stylus_probe(&self) -> (u8, Option<(f32, f32)>, u32) {
        let p = android_activity::input::pointer_probe();
        let tool = match p.tool {
            android_activity::input::PointerTool::Finger => 1,
            android_activity::input::PointerTool::Stylus => 2,
            android_activity::input::PointerTool::Mouse => 3,
            android_activity::input::PointerTool::Eraser => 4,
            android_activity::input::PointerTool::Palm => 5,
            android_activity::input::PointerTool::Unknown => 0,
        };
        (tool, p.hover, p.buttons)
    }
}
