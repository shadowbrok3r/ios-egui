//! Android host capability bridge. Each frame the runtime drains the [`Host`] request queue via
//! the core `drv_*` API and dispatches to Android through JNI (jni 0.21; the raw JavaVM/Activity
//! come from `ndk-context`, so android-activity's internal jni version is irrelevant).
//!
//! Everything here runs on the render thread. Capabilities that are thread-safe or use
//! `startActivity`/`NotificationManager`/`PackageInstaller` work directly. Capabilities that
//! require the Java UI thread (toast, an overlay `WindowManager` view, a foreground `Service`)
//! need a bundled Kotlin helper and are the next pass — their permission/gating flows are already
//! wired here.

use std::sync::Mutex;

use egui_mobile_core::Host;
use jni::JavaVM;
use jni::objects::{JObject, JString, JValue};

// Backend-specific request kinds enqueued by `HostExt` (via `Host::drv_enqueue`).
const K_SELF_UPDATE: i32 = 100;
const K_REQ_INSTALL_PERM: i32 = 101;
const K_REQ_OVERLAY_PERM: i32 = 102;
const K_REQ_NOTIF_PERM: i32 = 103;

// Pending permission checks: (core permission index, android permission string, frames left).
static PENDING_PERMS: Mutex<Vec<(usize, String, u32)>> = Mutex::new(Vec::new());
const PERM_POLL_FRAMES: u32 = 600; // ~10s at 60fps before giving up (marking denied).

/// Drain queued host requests and dispatch them. Called once per frame from the render loop.
pub fn drain(host: &Host) {
    poll_pending_permissions(host);
    while let Some(kind) = host.drv_pop() {
        match kind {
            0 => log::info!("egui-android: share_file needs a FileProvider (Kotlin pass); ignored"),
            1 => notify(
                &host.drv_str_a().unwrap_or_default(),
                &host.drv_str_b().unwrap_or_default(),
            ),
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
            other => log::info!("egui-android: host request kind {other} not handled"),
        }
    }
}

/// Run `f` with a JNIEnv attached to the current thread and the Activity (Context). Logs and
/// swallows JNI errors so a failed capability never crashes the render loop.
fn with_activity<R>(
    f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<R>,
) -> Option<R> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
    match f(&mut env, &activity) {
        Ok(r) => Some(r),
        Err(e) => {
            // Clear any pending Java exception so it can't poison the next JNI call this frame.
            let _ = env.exception_clear();
            log::error!("egui-android JNI error: {e:?}");
            None
        }
    }
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
    if let Some((t, b, l, r)) = read_root_insets_px() {
        let p = pixels_per_point.max(0.1);
        host.drv_set_safe_area(t / p, b / p, l / p, r / p);
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
}
