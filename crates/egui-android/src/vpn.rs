//! `VpnService` bridge: the system consent dialog, starting and stopping
//! [`EguiVpnService`](../../java/com/github/egui_mobile/EguiVpnService.java), and the tun file
//! descriptor the service hands down.
//!
//! The service also runs the app's foreground notification, so [`start_foreground`] keeps a
//! process alive without capturing anything.
//!
//! Java calls back into three natives on the service class. `FindClass` cannot see app classes
//! from the render thread, so [`register_natives`] resolves it through the activity's ClassLoader
//! (the `getObjectClass` trick used for the IME bridge needs an instance, and there is none here).

use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use jni::objects::{JObject, JString, JValue};

use crate::host::with_native_activity;

const SERVICE_CLASS: &str = "com.github.egui_mobile.EguiVpnService";
const ACTION_START: &str = "com.github.egui_mobile.action.VPN_START";
const ACTION_FOREGROUND: &str = "com.github.egui_mobile.action.FOREGROUND";
const ACTION_STOP_VPN: &str = "com.github.egui_mobile.action.VPN_STOP";
/// Matches `EguiNativeActivity.REQUEST_VPN_CONSENT`.
const REQUEST_VPN_CONSENT: i32 = 0x5670;

static EVENTS: Mutex<VecDeque<VpnEvent>> = Mutex::new(VecDeque::new());
static ACTIVE: AtomicBool = AtomicBool::new(false);
static REGISTERED: OnceLock<bool> = OnceLock::new();
/// Set while a consent dialog is outstanding, so a poll can tell "waiting" from "never asked".
static AWAITING_CONSENT: AtomicBool = AtomicBool::new(false);

/// What the tun interface is built with. Addresses are the tun's own, not the device's.
#[derive(Debug, Clone)]
pub struct VpnConfig {
    /// Shown in Android's VPN settings and the key icon's detail sheet.
    pub session: String,
    pub mtu: u16,
    pub address: String,
    pub prefix: u8,
    /// `None` leaves IPv6 unrouted, which lets IPv6 traffic bypass capture entirely.
    pub address6: Option<String>,
    pub prefix6: u8,
    /// Resolvers handed to captured apps. These are addresses the tun itself answers for.
    pub dns: Vec<String>,
    pub notification_title: String,
    pub notification_text: String,
}

impl Default for VpnConfig {
    fn default() -> Self {
        Self {
            session: String::from("VPN"),
            mtu: 1500,
            address: String::from("10.7.0.1"),
            prefix: 32,
            address6: Some(String::from("fd00:7::1")),
            prefix6: 128,
            dns: vec![String::from("1.1.1.1")],
            notification_title: String::from("VPN"),
            notification_text: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum VpnEvent {
    /// The tun is up. The receiver owns `fd` and closes it to tear the interface down.
    Started { fd: RawFd, mtu: u16 },
    Stopped,
    Failed(String),
}

/// Drain one queued service callback. Poll this from the frame loop.
pub fn poll_event() -> Option<VpnEvent> {
    EVENTS.lock().ok()?.pop_front()
}

/// Whether a tun is currently established.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Whether the system still needs to show the VPN consent dialog.
pub fn consent_required() -> bool {
    prepare_intent_present().unwrap_or(true)
}

/// Show the consent dialog if it is needed. Returns true when consent already exists and the VPN
/// can be started straight away.
pub fn request_consent() -> bool {
    if !consent_required() {
        AWAITING_CONSENT.store(false, Ordering::Relaxed);
        return true;
    }

    let shown = with_native_activity(|env, activity| {
        let intent = env
            .call_static_method(
                "android/net/VpnService",
                "prepare",
                "(Landroid/content/Context;)Landroid/content/Intent;",
                &[JValue::Object(activity)],
            )?
            .l()?;
        if intent.is_null() {
            return Ok(false);
        }
        env.call_method(
            activity,
            "startActivityForResult",
            "(Landroid/content/Intent;I)V",
            &[(&intent).into(), JValue::Int(REQUEST_VPN_CONSENT)],
        )?;
        Ok(true)
    })
    .unwrap_or(false);

    AWAITING_CONSENT.store(shown, Ordering::Relaxed);
    !shown
}

/// The consent dialog's answer, once. `None` while it is still open or was never opened.
///
/// Falls back to re-checking `VpnService.prepare` when the activity is not `EguiNativeActivity`
/// and therefore has no result latch — consent granted is indistinguishable from not yet answered
/// there, so only the granted case is reported.
pub fn poll_consent() -> Option<bool> {
    if !AWAITING_CONSENT.load(Ordering::Relaxed) {
        return None;
    }

    let latched = with_native_activity(|env, activity| {
        Ok(env.call_method(activity, "takeVpnConsent", "()I", &[])?.i()?)
    });

    let granted = match latched {
        Some(1) => true,
        Some(2) => false,
        Some(_) => return None,
        // No latch (plain NativeActivity): granted is observable, denied is not.
        None if prepare_intent_present() == Some(false) => true,
        None => return None,
    };

    AWAITING_CONSENT.store(false, Ordering::Relaxed);
    Some(granted)
}

/// Start capturing. Consent must already be granted — see [`request_consent`]. The tun descriptor
/// arrives later as [`VpnEvent::Started`].
pub fn start(config: &VpnConfig) -> bool {
    register_natives();

    let dns = config.dns.join(",");
    let address6 = config.address6.clone().unwrap_or_default();

    with_native_activity(|env, activity| {
        let intent = service_intent(env, activity, ACTION_START)?;
        put_string(env, &intent, "session", &config.session)?;
        put_string(env, &intent, "address", &config.address)?;
        put_string(env, &intent, "address6", &address6)?;
        put_string(env, &intent, "dns", &dns)?;
        put_string(env, &intent, "title", &config.notification_title)?;
        put_string(env, &intent, "text", &config.notification_text)?;
        put_int(env, &intent, "mtu", i32::from(config.mtu))?;
        put_int(env, &intent, "prefix", i32::from(config.prefix))?;
        put_int(env, &intent, "prefix6", i32::from(config.prefix6))?;
        start_foreground_service(env, activity, &intent)?;
        Ok(true)
    })
    .unwrap_or(false)
}

/// Run the foreground notification without a tun, so Android keeps the process alive.
pub fn start_foreground(title: &str, text: &str) -> bool {
    register_natives();

    with_native_activity(|env, activity| {
        let intent = service_intent(env, activity, ACTION_FOREGROUND)?;
        put_string(env, &intent, "title", title)?;
        put_string(env, &intent, "text", text)?;
        start_foreground_service(env, activity, &intent)?;
        Ok(true)
    })
    .unwrap_or(false)
}

/// Close the tun but leave the foreground notification up, for a process that still has work.
pub fn stop_capture(title: &str, text: &str) -> bool {
    with_native_activity(|env, activity| {
        let intent = service_intent(env, activity, ACTION_STOP_VPN)?;
        put_string(env, &intent, "title", title)?;
        put_string(env, &intent, "text", text)?;
        start_foreground_service(env, activity, &intent)?;
        Ok(true)
    })
    .unwrap_or(false)
}

/// Stop the service, tearing down the tun and the notification with it.
pub fn stop() {
    with_native_activity(|env, activity| {
        let intent = service_intent(env, activity, ACTION_FOREGROUND)?;
        env.call_method(
            activity,
            "stopService",
            "(Landroid/content/Intent;)Z",
            &[(&intent).into()],
        )?;
        Ok(())
    });
}

/// The app Android has pinned as the always-on VPN, when it is not this one.
///
/// Only one VPN holds the tun at a time, and while another app is always-on the consent dialog
/// does not merely get declined — `ConfirmDialog` finishes without asking, which reaches
/// `onActivityResult` as `RESULT_CANCELED` and is indistinguishable from the user saying no.
/// Returns the app's label where it resolves, otherwise its package name.
pub fn blocking_always_on_vpn() -> Option<String> {
    with_native_activity(|env, activity| {
        let resolver = env
            .call_method(
                activity,
                "getContentResolver",
                "()Landroid/content/ContentResolver;",
                &[],
            )?
            .l()?;
        let key = env.new_string("always_on_vpn_app")?;
        let value = env
            .call_static_method(
                "android/provider/Settings$Secure",
                "getString",
                "(Landroid/content/ContentResolver;Ljava/lang/String;)Ljava/lang/String;",
                &[(&resolver).into(), (&key).into()],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let package: String = env.get_string(&JString::from(value))?.into();
        if package.is_empty() {
            return Ok(None);
        }

        let own: String = {
            let obj = env
                .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
                .l()?;
            env.get_string(&JString::from(obj))?.into()
        };
        if package == own {
            return Ok(None);
        }

        Ok(Some(app_label(env, activity, &package).unwrap_or(package)))
    })
    .flatten()
}

/// A package's user-visible label, or `None` if it cannot be resolved.
fn app_label(env: &mut jni::JNIEnv, activity: &JObject, package: &str) -> Option<String> {
    let manager = env
        .call_method(
            activity,
            "getPackageManager",
            "()Landroid/content/pm/PackageManager;",
            &[],
        )
        .ok()?
        .l()
        .ok()?;
    let jpackage = env.new_string(package).ok()?;
    let info = env
        .call_method(
            &manager,
            "getApplicationInfo",
            "(Ljava/lang/String;I)Landroid/content/pm/ApplicationInfo;",
            &[(&jpackage).into(), JValue::Int(0)],
        )
        .ok()?
        .l()
        .ok()?;
    let label = env
        .call_method(
            &manager,
            "getApplicationLabel",
            "(Landroid/content/pm/ApplicationInfo;)Ljava/lang/CharSequence;",
            &[(&info).into()],
        )
        .ok()?
        .l()
        .ok()?;
    let label = env
        .call_method(&label, "toString", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;
    env.get_string(&JString::from(label)).ok().map(Into::into)
}

/// Open Android's VPN settings — where the app appears once consent is granted, and the only
/// place it can be revoked or made always-on.
pub fn open_vpn_settings() -> bool {
    start_settings("android.net.vpn.SETTINGS", false)
}

/// Open this app's notification settings. The foreground service runs whether or not
/// `POST_NOTIFICATIONS` was granted, but its row — and the Stop button on it — is invisible
/// until it is.
pub fn open_notification_settings() -> bool {
    // ACTION_APP_NOTIFICATION_SETTINGS takes the package as an extra, not a `package:` Uri.
    let opened = with_native_activity(|env, activity| {
        let action = env.new_string("android.settings.APP_NOTIFICATION_SETTINGS")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[(&action).into()],
        )?;
        let key = env.new_string("android.provider.extra.APP_PACKAGE")?;
        let package = env
            .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
            .l()?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
            &[(&key).into(), (&package).into()],
        )?;
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&intent).into()],
        )?;
        Ok(true)
    })
    .unwrap_or(false);

    // Some OEM builds do not answer that action; the app's own settings page always exists.
    opened || start_settings("android.settings.APPLICATION_DETAILS_SETTINGS", true)
}

/// Launch a Settings screen, optionally scoped to this package by `package:` Uri.
fn start_settings(action: &str, for_package: bool) -> bool {
    with_native_activity(|env, activity| {
        let jaction = env.new_string(action)?;
        let intent = if for_package {
            let package = env
                .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
                .l()?;
            let package: String = env.get_string(&JString::from(package))?.into();
            let target = env.new_string(format!("package:{package}"))?;
            let uri = env
                .call_static_method(
                    "android/net/Uri",
                    "parse",
                    "(Ljava/lang/String;)Landroid/net/Uri;",
                    &[(&target).into()],
                )?
                .l()?;
            env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;Landroid/net/Uri;)V",
                &[(&jaction).into(), (&uri).into()],
            )?
        } else {
            env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[(&jaction).into()],
            )?
        };
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&intent).into()],
        )?;
        Ok(true)
    })
    .unwrap_or(false)
}

// ── JNI ──────────────────────────────────────────────────────────────────────

fn push(event: VpnEvent) {
    if let Ok(mut events) = EVENTS.lock() {
        events.push_back(event);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiVpnService_nativeVpnStarted(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    fd: jni::sys::jint,
    mtu: jni::sys::jint,
) {
    log::info!("egui-android vpn: tun fd {fd} mtu {mtu}");
    ACTIVE.store(true, Ordering::Relaxed);
    push(VpnEvent::Started {
        fd: fd as RawFd,
        mtu: mtu.clamp(576, u16::MAX as i32) as u16,
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiVpnService_nativeVpnStopped(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
) {
    log::info!("egui-android vpn: tun closed");
    ACTIVE.store(false, Ordering::Relaxed);
    push(VpnEvent::Stopped);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiVpnService_nativeVpnFailed(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    reason: JString,
) {
    let reason: String = env
        .get_string(&reason)
        .map(Into::into)
        .unwrap_or_else(|_| String::from("unknown error"));
    log::warn!("egui-android vpn: {reason}");
    ACTIVE.store(false, Ordering::Relaxed);
    push(VpnEvent::Failed(reason));
}

/// Bind the three service natives. `NativeActivity` dlopens the native library, so ART's own
/// symbol lookup may never see them however they are exported.
fn register_natives() {
    if REGISTERED.get().is_some() {
        return;
    }

    let ok = with_native_activity(|env, activity| {
        let class = service_class(env, activity)?;
        let methods = [
            jni::NativeMethod {
                name: jni::strings::JNIString::from("nativeVpnStarted"),
                sig: jni::strings::JNIString::from("(II)V"),
                fn_ptr: Java_com_github_egui_1mobile_EguiVpnService_nativeVpnStarted
                    as *mut std::ffi::c_void,
            },
            jni::NativeMethod {
                name: jni::strings::JNIString::from("nativeVpnStopped"),
                sig: jni::strings::JNIString::from("()V"),
                fn_ptr: Java_com_github_egui_1mobile_EguiVpnService_nativeVpnStopped
                    as *mut std::ffi::c_void,
            },
            jni::NativeMethod {
                name: jni::strings::JNIString::from("nativeVpnFailed"),
                sig: jni::strings::JNIString::from("(Ljava/lang/String;)V"),
                fn_ptr: Java_com_github_egui_1mobile_EguiVpnService_nativeVpnFailed
                    as *mut std::ffi::c_void,
            },
        ];
        env.register_native_methods(&class, &methods)?;
        Ok(true)
    })
    .unwrap_or(false);

    let _ = REGISTERED.set(ok);
    log::info!("egui-android vpn: register_natives ok={ok}");
}

fn service_class<'l>(
    env: &mut jni::JNIEnv<'l>,
    activity: &JObject,
) -> jni::errors::Result<jni::objects::JClass<'l>> {
    let loader = env
        .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let name = env.new_string(SERVICE_CLASS)?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[(&name).into()],
        )?
        .l()?;
    Ok(class.into())
}

fn service_intent<'l>(
    env: &mut jni::JNIEnv<'l>,
    activity: &JObject,
    action: &str,
) -> jni::errors::Result<JObject<'l>> {
    let class = service_class(env, activity)?;
    let intent = env.new_object(
        "android/content/Intent",
        "(Landroid/content/Context;Ljava/lang/Class;)V",
        &[JValue::Object(activity), (&class).into()],
    )?;
    let jaction = env.new_string(action)?;
    env.call_method(
        &intent,
        "setAction",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[(&jaction).into()],
    )?;
    Ok(intent)
}

fn put_string(
    env: &mut jni::JNIEnv,
    intent: &JObject,
    key: &str,
    value: &str,
) -> jni::errors::Result<()> {
    let jkey = env.new_string(key)?;
    let jvalue = env.new_string(value)?;
    env.call_method(
        intent,
        "putExtra",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[(&jkey).into(), (&jvalue).into()],
    )?;
    Ok(())
}

fn put_int(
    env: &mut jni::JNIEnv,
    intent: &JObject,
    key: &str,
    value: i32,
) -> jni::errors::Result<()> {
    let jkey = env.new_string(key)?;
    env.call_method(
        intent,
        "putExtra",
        "(Ljava/lang/String;I)Landroid/content/Intent;",
        &[(&jkey).into(), JValue::Int(value)],
    )?;
    Ok(())
}

fn start_foreground_service(
    env: &mut jni::JNIEnv,
    activity: &JObject,
    intent: &JObject,
) -> jni::errors::Result<()> {
    env.call_method(
        activity,
        "startForegroundService",
        "(Landroid/content/Intent;)Landroid/content/ComponentName;",
        &[intent.into()],
    )?;
    Ok(())
}

/// `true` when `VpnService.prepare` still returns a consent Intent. `None` if the call failed.
fn prepare_intent_present() -> Option<bool> {
    with_native_activity(|env, activity| {
        let intent = env
            .call_static_method(
                "android/net/VpnService",
                "prepare",
                "(Landroid/content/Context;)Landroid/content/Intent;",
                &[JValue::Object(activity)],
            )?
            .l()?;
        Ok(!intent.is_null())
    })
}
