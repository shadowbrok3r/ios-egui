<!-- Cross-platform (iOS + Android) design for the egui-ios / egui-mobile library. -->
> Auto-synthesized from a 6-way research workflow, verified against this repo's current state
> (egui 0.35, existing `egui-ios-plugin-*` wasmtime host, `desktop-host` + `plugins-ios` examples).
> Repo root: `$HOME/Documents/Rust/IOS/ios-egui`.

# egui-mobile: Unified iOS + Android Design & Implementation Plan

## 1. Goals & the cross-platform vision

**One `impl EguiApp` runs unchanged on iOS and Android.** An app author writes a single struct, implements one trait, and picks a target with a build tool. No `#[cfg]` in app logic, no platform branches in the UI, no second codebase.

Concretely:

```rust
struct MyApp { /* ... */ }

impl egui_mobile::EguiApp for MyApp {
    fn update(&mut self, ui: &mut egui::Ui, host: &Host) { /* pure egui */ }
    fn theme(&self, ctx: &egui::Context) { /* ... */ }
    fn on_start(&mut self, host: &Host) {}
    fn on_resume(&mut self, host: &Host) {}
    fn on_pause(&mut self, host: &Host) {}
}

egui_mobile::app!(MyApp::new);
```

The **only** line that changes across platforms is nothing in the source — `egui_mobile` re-exports the correct platform crate via `cfg(target_os)`, and the `app!` macro expands to a C ABI on iOS and `android_main`/JNI on Android. The author's `Cargo.toml` depends on `egui-mobile`; the build tool (`cargo egui-mobile build --target ios|android`) does the rest.

**Design principles, decided:**

1. **Identical Host method surface.** Every iOS `Host` method exists on Android with the same name and signature. Android *adds* a superset (services, overlays, self-update, intents) but never renames or diverges the common core.
2. **Rust owns the loop everywhere, but via different mechanisms.** iOS: Swift owns the loop, Rust is a linked staticlib behind a frozen C ABI + poll bridge (unchanged — it works). Android: Rust owns the process via `android_main`, so the Host bridge is **direct JNI**, not a cross-language poll queue. The poll/`HostRequest` machinery is kept internally on Android only for genuinely-async results.
3. **The plugin story is write-once.** The existing wasmtime WASM plugin host runs identically on both. WASM is the canonical, policy-safe, cross-platform "load code at runtime" path.
4. **Latest crates, in lockstep.** Bump iOS and Android together: egui/egui-winit/egui-wgpu **0.35.0**, wgpu **29** (do NOT jump to wgpu 30 — it needs egui 0.36+). Bump the iOS side from 0.34→0.35 as part of Phase 0 so the shared core has one version.

**The payoff:** the same binary logic that renders a UI on an iPad renders on Android *and* can draw over other apps, self-update its own APK, spawn foreground services, and hot-load plugins — capabilities that are outright impossible or heavily sandboxed on iOS.

---

## 2. Workspace / crate layout

**Decision: shared `egui-mobile-core` crate + thin per-platform crates + a `egui-mobile` facade.** Not cfg-in-one-crate (the dependency trees are wildly different — jni/ndk/android-activity vs Swift/Metal — and a single crate would need enormous cfg-gating that breaks `cargo check` on the host). Not two fully independent crates either (that duplicates the trait, the Host request enum, the tessellation glue, and drifts).

Three layers:

- **`egui-mobile-core`** — platform-neutral. Owns the `EguiApp` trait, `CreateContext`, `HostRequest`/`HostEvent` enums, the shared value types (`Insets`, `Haptic`, `Permission`, `ServiceKind`, `OverlaySpec`, …), the `HostBackend` trait, and the reusable half of RenderCore: the egui `Context`, input-event synthesis helpers, and the tessellate→`egui_wgpu::Renderer`→paint pass. Zero platform deps.
- **`egui-ios`** — existing crate, refactored to depend on `egui-mobile-core`. Keeps the Swift C ABI, `FfiPollBackend`, CAMetalLayer surface source.
- **`egui-android`** — new sibling. Owns `android_main`, the winit/android-activity event loop, the Vulkan/GL surface source, and `JniBackend`.
- **`egui-mobile`** — the facade every app depends on. It is ~20 lines:

```rust
// egui-mobile/src/lib.rs
pub use egui_mobile_core::{EguiApp, Host, Insets, Haptic, Permission, /* ... */};

#[cfg(target_os = "ios")]     pub use egui_ios::app;
#[cfg(target_os = "android")] pub use egui_android::app;
```

**Where the Host facade lives:** the *type* `Host` and its common methods live in `egui-mobile-core`, parameterized over a `HostBackend`. The backend is platform-specific (`FfiPollBackend` in egui-ios, `JniBackend` in egui-android). Common methods dispatch through the trait; Android-only methods live in an **`HostExt` trait** in `egui-android` (so `use egui_android::HostExt` unlocks `self_update`, `show_overlay`, etc. — they simply don't exist when compiling for iOS, giving a compile error if an app author accidentally calls an Android-only method in shared code without a cfg).

**Capability split — decided:**
- **Common (on `Host`, both platforms):** `share_file`, `notify`, `request_keyboard`, `haptic`, `open_url`, `copy_text`, `pick_file`, `request_permission`, `start_camera_preview`/`stop_camera_preview`; reads `documents_dir`, `safe_area_insets`, `keyboard_height`, `is_active`, `take_picked_file`, `permission`, `mic_level`, `read_clipboard`, `toast` (iOS falls back to a transient egui toast).
- **Android-only (`HostExt` in egui-android, behind cargo features):** `self_update`, `start_service`/`stop_service`/`schedule_work`, `load_plugin` (shared but exposed here), `show_overlay`/`hide_overlay`/`request_overlay_permission`, `intent`, `with_activity` (raw JNI escape hatch), plus the broad catalog (BLE, sensors, NFC, tiles, widgets, wallpaper, IME, roles).

**Crate tree:**

```
ios-egui/                         (existing workspace root)
├── Cargo.toml                    workspace members
├── crates/
│   ├── egui-mobile-core/         NEW  platform-neutral trait+Host+RenderCore core
│   │   └── src/{lib,app,host,render_core,input,events,types}.rs
│   ├── egui-mobile/              NEW  facade app authors depend on
│   │   └── src/lib.rs
│   ├── egui-ios/                 EXISTING, refactored onto core
│   │   └── src/{lib,render_core,host,input,plugins,__ffi}.rs
│   ├── egui-android/             NEW
│   │   └── src/{lib,android_main,render_core,host,jni_backend,
│   │             input,ime,insets,plugins,marquee/,catalog/}.rs
│   ├── egui-mobile-plugin-host/  RENAMED from egui-ios-plugin-host (shared)
│   ├── egui-mobile-plugin-abi/   RENAMED (shared, frozen ABI)
│   └── egui-mobile-plugin-sdk/
├── tools/
│   ├── cargo-egui-ios/           EXISTING
│   └── cargo-egui-android/       NEW  wraps cargo-apk2
├── kotlin/                       NEW  bundled EguiHost.kt/EguiService.kt (source, DEX'd)
└── swift/EguiKit/                EXISTING Swift package
```

(Rename `egui-ios-plugin-*` → `egui-mobile-plugin-*` in Phase 0; it is already cross-platform in fact, just misnamed.)

---

## 3. The unified app-author API

### The `EguiApp` trait (unchanged, in core)

```rust
pub trait EguiApp: 'static {
    fn update(&mut self, ui: &mut egui::Ui, host: &Host);
    fn theme(&self, ctx: &egui::Context) {}
    fn on_start(&mut self, host: &Host) {}
    fn on_resume(&mut self, host: &Host) {}
    fn on_pause(&mut self, host: &Host) {}
}
```

### The Host facade — common methods (both platforms)

```rust
impl Host {
    // Actions (fire-and-forget; enqueued on iOS, direct-JNI/UI-post on Android)
    pub fn share_file(&self, path: impl Into<String>);
    pub fn share_text(&self, text: impl Into<String>);
    pub fn notify(&self, title: impl Into<String>, body: impl Into<String>);
    pub fn toast(&self, text: impl Into<String>, long: bool);
    pub fn request_keyboard(&self, visible: bool);
    pub fn haptic(&self, kind: Haptic);
    pub fn open_url(&self, url: impl Into<String>);
    pub fn copy_text(&self, text: impl Into<String>);
    pub fn pick_file(&self, mime_or_uti: &[&str]);
    pub fn request_permission(&self, p: Permission);
    pub fn start_camera_preview(&self);
    pub fn stop_camera_preview(&self);

    // Reads (synchronous, cheap, value-typed)
    pub fn documents_dir(&self) -> Option<String>;
    pub fn safe_area_insets(&self) -> Insets;
    pub fn keyboard_height(&self) -> f32;
    pub fn is_active(&self) -> bool;
    pub fn take_picked_file(&self) -> Option<PickedFile>;
    pub fn permission(&self, p: Permission) -> Option<bool>;
    pub fn mic_level(&self) -> f32;
    pub fn read_clipboard(&self) -> Option<String>;
}
```

`Permission` is a shared enum, broadened for Android but harmless on iOS:

```rust
pub enum Permission {
    Camera, Microphone, Notifications,
    LocationFine, LocationCoarse, LocationBackground,
    Contacts, BluetoothScan, BluetoothConnect, NearbyWifi,
    Storage, // maps to Photo Picker / SAF on Android, PHPicker on iOS
    Overlay, InstallPackages, // Android special-access; return None on iOS
}
```

### Android-only extensions (`egui_android::HostExt`)

```rust
pub trait HostExt {
    // ── Self-update (feature = "self-update") ──
    fn can_install_packages(&self) -> bool;
    fn request_install_permission(&self);
    fn self_update(&self, apk_path: impl Into<String>);
    fn take_install_result(&self) -> Option<InstallOutcome>;
    fn current_version_code(&self) -> i64;

    // ── Background services / WorkManager (feature = "services") ──
    fn start_service(&self, spec: ServiceSpec);   // ServiceSpec{kind,title,body,worker}
    fn stop_service(&self, kind: ServiceKind);
    fn is_service_running(&self, kind: ServiceKind) -> bool;
    fn schedule_work(&self, spec: WorkSpec);      // WorkManager, >=15min periodic
    fn cancel_work(&self, tag: &str);
    fn schedule_exact_alarm(&self, at_unix_ms: i64, worker: &str);

    // ── Plugins (feature = "plugins") ──
    fn load_plugin(&self, source: PluginSource) -> Result<PluginId, String>;
    fn unload_plugin(&self, id: PluginId);
    fn list_plugins(&self) -> Vec<PluginId>;

    // ── Overlays (feature = "overlays") ──
    fn can_draw_overlays(&self) -> bool;
    fn request_overlay_permission(&self);
    fn show_overlay(&self, spec: OverlaySpec) -> OverlayHandle;
    fn update_overlay(&self, h: OverlayHandle, x: i32, y: i32, w: i32, h_: i32);
    fn hide_overlay(&self, h: OverlayHandle);

    // ── Escape hatches ──
    fn intent(&self, action: &str, uri: Option<&str>, extras: &[(&str, IntentValue)]);
    fn with_activity<R>(&self, f: impl FnOnce(&mut jni::Env, &JObject) -> R) -> R;
}
```

Register background workers at init (headless Rust, no egui):

```rust
egui_android::register_worker("sync", |ctx: &WorkerCtx| { /* runs in Service/Worker */ });
```

### One `app!` macro, two entry points

The macro lives in `egui-mobile` and re-exports the platform macro. Each platform macro is the sole place that emits the entry point:

**iOS** (`egui_ios::app!`) — unchanged, emits the frozen C ABI:

```rust
#[no_mangle] pub extern "C" fn egui_ios_new(layer: *mut c_void, ...) -> *mut c_void { ... }
#[no_mangle] pub extern "C" fn egui_ios_render(...) { ... }
// egui_ios_touch/keyboard/scroll/poll_request/on_*/last_error ...
```

**Android** (`egui_android::app!`) — emits `android_main` + the JNI async-result native methods:

```rust
#[no_mangle]
pub extern "C" fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info));
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        egui_android::set_last_error(info.to_string());
    }));
    egui_android::run(app, || Box::new(MyApp::new()));
}

// Emitted async-result setters (analogs of egui_ios_on_*):
// Java_..._nativeOnFilePicked, nativeOnPermissionResult, nativeOnMicLevel,
// nativeOnInstallStatus, nativeOnServiceState, nativeServiceTick,
// registered via RegisterNatives in JNI_OnLoad to decouple from package names.
```

Same one-liner in app code (`egui_mobile::app!(MyApp::new)`); the facade forwards to whichever platform macro `cfg` selected.

---

## 4. The Android runtime

**Decided stack: `android-activity` (GameActivity) → own winit-free poll loop → egui-winit input mapping is *not* used; instead reuse the core input synthesis** — because GameActivity's GameTextInput is required for working IME and winit hides it. We drive the loop directly off `android-activity`'s `poll_events`, giving full control and the same input path as iOS.

> Decision rationale: the two credible options are (a) `winit 0.30` + `egui-winit` on top of android-activity, or (b) android-activity `poll_events` directly. We pick **(b)** as the primary path because winit historically breaks `show_soft_input` on native-activity (android-activity#44) and hides `native_window()` needed for the surface-drop-on-suspend dance. egui-winit is kept as a documented fallback. This costs us a keycode table and a touch mapper (both ~100 lines, ported from the iOS `input.rs` shape) but buys reliable IME and lifecycle control.

**Recommended crates + versions (crates.io, current):**

| Crate | Version | Role |
|---|---|---|
| `android-activity` (feature `game-activity`) | 0.6.1 | `android_main`, lifecycle/input/IME, `native_window()`, seeds `ndk-context` |
| `egui` / `egui-wgpu` | 0.35.0 | UI + renderer (lockstep with iOS) |
| `wgpu` | 29 | Vulkan primary, GL fallback |
| `raw-window-handle` | 0.6.2 | `AndroidNdkWindowHandle` → wgpu surface |
| `ndk` | 0.9.0 | `NativeWindow`, `Configuration` (density), `Choreographer`, `MotionEvent` |
| `ndk-context` | 0.1.1 | `android_context()` → JavaVM + Activity |
| `jni` | 0.22.4 | JNI Host bridge — **pin to match android-activity's transitive jni** |
| `pollster` | 0.4 | block on adapter/device request |
| `android_logger` | 0.15.1 | route logs to logcat |
| `log` | 0.4 | facade |

> **jni 0.22 caveat:** 0.22 is a recent rewrite (`Env`/`EnvUnowned` split, closure-based `attach_current_thread`, `JavaVM::from_raw`). If 0.22 churn bites, `jni = "0.21.1"` is the battle-tested fallback — but then obtain the raw `*mut JavaVM` from `ndk-context` (version-agnostic) so the ecosystem's internal jni version doesn't matter. **Ship a compiling spike on 0.22.4 before committing.**

**Surface recreation on resume — the #1 correctness rule.** Split RenderCore state:

- **Persistent across suspend:** `Device`, `Queue`, `egui::Context`, `egui_wgpu::Renderer`, `Box<dyn EguiApp>`.
- **Transient, rebuilt each resume:** `Surface`, `SurfaceConfiguration`.

```rust
match event {
    MainEvent::InitWindow{..} | MainEvent::Resume{..} => {
        let win = app.native_window().unwrap();
        core.create_surface(&win); // freshly read win.width()/height()
    }
    MainEvent::TerminateWindow{..} | MainEvent::Pause{..} => {
        core.surface = None; // DROP surface BEFORE callback returns, then NativeWindow guard
    }
    MainEvent::WindowResized{..} => core.resize_from(app.native_window()),
    MainEvent::ConfigChanged{..} => core.set_pixels_per_point(density_ppp(&app)),
    MainEvent::InsetsChanged{..} => host.refresh_insets(),
    MainEvent::RedrawNeeded | PollEvent::Timeout | PollEvent::Wake => {
        host.pump_input(&app);
        core.render(now(), |ui| app.update(ui, &host));
    }
    MainEvent::Destroy => quit = true, // return normally; never process::exit
    _ => {}
}
```

`wgpu::Instance` backends = `VULKAN | GL`. Surface created via `create_surface_unsafe(SurfaceTargetUnsafe::RawHandle{ RawWindowHandle::AndroidNdk(...), RawDisplayHandle::Android(...) })`. The `get_current_texture` `Outdated`/`Lost` → reconfigure branch from the iOS render_core ports verbatim.

**Input / IME.** Drain `app.input_events_iter()` each frame. `MotionEvent` (TOUCHSCREEN) → touch began/moved/ended/cancelled; `MotionEvent` (MOUSE, `AXIS_VSCROLL/HSCROLL`) → scroll; `KeyEvent` → a `KEYCODE_* → egui::Key` table (the Android analog of iOS `hid_to_egui_key`); `meta_state` → `egui::Modifiers`. Coordinates are physical px → divide by ppp. **Committed text arrives as GameTextInput `EditorState`, not key events** — diff old vs new `text_input_state()` to synthesize `Event::Text` for insertions and `Event::Key{Backspace}` for deletions; seed with `set_text_input_state()` on focus. Gate `show_soft_input()`/`hide_soft_input()` on `ctx.text_edit_focused()`.

**Insets / density.** On `InsetsChanged`, JNI into the decor view: `getRootWindowInsets()` → `getInsets(Type.systemBars() | Type.displayCutout())`, take per-edge max, `/ppp`, feed the existing `Insets`. `Type.ime()` bottom inset → `keyboard_height`. Under Android 15 edge-to-edge is mandatory, so this is not optional. `pixels_per_point = config.density()`; recompute on `ConfigChanged` (fold/rotate).

**Redraw scheduling.** After `run_ui`, read `repaint_delay`: `ZERO` → poll timeout 0; `MAX` → block in `poll_events(None)`; else pass the `Duration`. For vsync-locked animation, register an `ndk` `Choreographer` frame callback that `create_waker().wake()`s each vsync (the Android analog of iOS `CADisplayLink`). `present_mode: Fifo` is the simpler baseline.

---

## 5. The JNI capability bridge

**The inversion:** on iOS, Rust can't call UIKit, so Swift polls a queue each frame. On Android, Rust owns the loop and *can* call the JVM directly, so the "poll" becomes an **internal per-frame drain** (`JniBackend`), not a cross-language contract. This deletes the entire `egui_ios_request_str_a/str_b/int` marshalling layer on Android for synchronous work.

**`HostBackend` trait** (in core) unifies the two:

```rust
pub trait HostBackend {
    fn dispatch(&self, req: &HostRequest); // iOS: enqueue; Android: JNI now
    fn haptic(&self, kind: Haptic);        // sync fast path
}
```

- **iOS** → `FfiPollBackend` (existing poll queue + `egui_ios_on_*` setters).
- **Android** → `JniBackend`: three routing tiers.

**Routing tiers (the axis is only *which thread runs the Java*, both are Rust-initiated):**

1. **Direct-JNI from the render thread** (thread-safe, no Activity/Looper needed): `haptic`/`vibrate`, and all reads — `documents_dir`, `safe_area_insets`, battery, connectivity, sensors snapshot, `read_clipboard` (foreground). Attach the render thread once; wrap each call in `env.with_local_frame(16, …)` so local refs free per frame.
2. **Post to UI thread via the Kotlin `EguiHost`** (helper does `runOnUiThread`): `share_file`, `open_url`, `notify`, `pick_file`, `request_permission`, `copy_text`, camera preview, overlays, service start/stop, `self_update`. These are UI-thread-only or use `startActivityForResult`.
3. **Async results back to Rust** via `#[no_mangle] extern "system"` native methods bound with `RegisterNatives`: `nativeOnFilePicked`, `nativeOnPermissionResult`, `nativeOnMicLevel`, `nativeOnInstallStatus`, `nativeOnServiceState`. Each locks a `Mutex<HostState>` (Android needs `Mutex`, not iOS's `Rc<RefCell>` — callbacks arrive on the UI thread while the render thread reads) and writes the field. Next frame's `permission()`/`take_picked_file()` read picks it up.

**The tiny Kotlin helper — worth it, ~150 lines total.** Pure reflection-JNI for `share`/`vibrate`/`permissions` means dozens of fragile signature strings and manual API-level branching. A bundled `EguiHost.kt` collapses each capability to one clean JNI call and centralizes Android-version quirks in Kotlin. It ships as source in the `egui-android` package (compiled to DEX by cargo-apk2), so **app authors write zero Kotlin**. The unavoidable Kotlin files:

- `EguiHost.kt` — the GlobalRef helper object (share, notify, vibrate branch, permission requests, overlay add/remove, install session).
- `EguiService.kt` — the foreground Service (Section 6).
- `EguiActivity : GameActivity` — overrides `onRequestPermissionsResult`/`onActivityResult` → calls the native setters.

**Thread rules (must get right):** `android_main` runs on a raw pthread — attach it permanently once (`attach_current_thread`, TLS auto-detach at exit). Native-method threads (UI thread) are *already* attached — use the passed `EnvUnowned`, **never detach** (detaching a JVM-owned thread aborts the process). Cache the `JavaVM` (process-global), never the `JNIEnv` (thread-local). The `ndk-context` Context is a global ref owned by ndk-context — borrow per-frame, never `DeleteGlobalRef`.

**End-to-end example — `pick_file` (the async path, mirror of iOS):**

```rust
// App code (identical on both platforms):
if ui.button("Open").clicked() { host.pick_file(&["image/*"]); }
if let Some(f) = host.take_picked_file() { self.load(f.local_path); }
```

```rust
// Android JniBackend.dispatch(HostRequest::PickFile(mimes)):
self.call_ui(|env, host| {
    let arr = env.new_string(mimes.join(","))?;
    env.call_method(host, "pickFile", "(Ljava/lang/String;)V", &[(&arr).into()])?;
    Ok(())
});
```

```kotlin
// EguiHost.kt
fun pickFile(mimes: String) = activity.runOnUiThread {
    val i = Intent(Intent.ACTION_OPEN_DOCUMENT).setType("*/*")
        .putExtra(Intent.EXTRA_MIME_TYPES, mimes.split(","))
        .addCategory(Intent.CATEGORY_OPENABLE)
    activity.startActivityForResult(i, REQ_PICK)
}
// In EguiActivity.onActivityResult(REQ_PICK): copy content:// to cacheDir,
// then nativeOnFilePicked(uri.toString(), localPath)
```

```rust
// Emitted native method (jni 0.22):
#[no_mangle]
pub extern "system" fn Java_..._nativeOnFilePicked(
    mut env: jni::EnvUnowned, _c: JClass, uri: JString, path: JString) {
    let _ = env.with_env(|env| {
        let uri: String = env.get_string(&uri)?.into();
        let path: String = env.get_string(&path)?.into();
        HOST_STATE.lock().unwrap().picked_file = Some(PickedFile{uri, local_path: path});
        Ok(())
    });
}
```

Same `HostRequest` enum, same `take_picked_file()` getter, same app code — only the backend differs.

---

## 6. The four marquee capabilities

### Self-update (`feature = "self-update"`)

**API:**
```rust
host.can_install_packages() -> bool;
host.request_install_permission();          // ACTION_MANAGE_UNKNOWN_APP_SOURCES
host.self_update(apk_path);                 // PackageInstaller.Session
host.take_install_result() -> Option<InstallOutcome>; // Pending|UserAction|Success|Failure(code,msg)
host.current_version_code() -> i64;
```

**Mechanism:** download signed APK to app storage → `PackageInstaller.createSession(MODE_FULL_INSTALL)` → `openWrite`/copy/`fsync`/`close` → `commit(PendingIntent broadcast, FLAG_MUTABLE)`. Session streams bytes, so **no FileProvider/content:// needed**. The result receiver reads `EXTRA_STATUS`; on `STATUS_PENDING_USER_ACTION`, `startActivity(EXTRA_INTENT)`.

**Manifest/permissions:** `<uses-permission android:name="android.permission.REQUEST_INSTALL_PACKAGES"/>` + `INTERNET`. Runtime gate on `canRequestPackageInstalls()`.

**Caveats:** (1) The updated APK **must be signed with the same key** as the installed app + `versionCode >=` current + identical `packageName`. (2) `setRequireUserAction(USER_ACTION_NOT_REQUIRED)` skips the confirm dialog **only if your app is the installer-of-record** — a sideloaded app usually is not, so **design for the confirm dialog every time**. (3) `PendingIntent` must be `FLAG_MUTABLE` on Android 12+ or `EXTRA_STATUS` never arrives. (4) **Google Play forbids self-update via `REQUEST_INSTALL_PACKAGES`** — this feature is for sideload/enterprise/F-Droid only; surface that to the developer. The Play-safe hot-update path is **WASM plugins** (Section 6, Plugins).

### Background services (`feature = "services"`)

**API:**
```rust
egui_android::register_worker("sync", |ctx| { /* headless Rust */ });
host.start_service(ServiceSpec{kind: ServiceKind::DataSync, title, body, worker:"sync"});
host.stop_service(ServiceKind::DataSync);
host.schedule_work(WorkSpec{tag, worker, initial_delay, period, network, charging});
host.schedule_exact_alarm(at_ms, "sync");
```

**Mechanism:** `EguiService.kt` (the unavoidable Kotlin, ~25 lines) `System.loadLibrary`s the same `.so`, in `onStartCommand` builds a NotificationChannel + `startForeground(id, notif, FOREGROUND_SERVICE_TYPE_DATA_SYNC)` within ~5s, then spins a worker thread that attaches to the JVM, inits `ndk_context`, and calls `nativeServiceTick(worker_name)`. WorkManager (`EguiWorker : CoroutineWorker`) for deferrable/periodic (≥15 min) work.

**Manifest/permissions:** `FOREGROUND_SERVICE` + a typed `FOREGROUND_SERVICE_DATA_SYNC` (Android 14+ mandatory) + `POST_NOTIFICATIONS` (runtime, Android 13+) + `RECEIVE_BOOT_COMPLETED` (reschedule after reboot) + `SCHEDULE_EXACT_ALARM` (exact alarms, user-gated on 14+). `<service android:foregroundServiceType="dataSync"/>`.

**Caveats:** `startForeground` without a declared type throws `MissingForegroundServiceTypeException` (14+). Starting an FGS from background throws `ForegroundServiceStartNotAllowedException` (12+) unless exempt — start while visible. **Android 15:** `dataSync` is capped at 6h/24h and **cannot start from `BOOT_COMPLETED`** — use WorkManager for long/periodic work, FGS only for must-run-now. `POST_NOTIFICATIONS` denied → FGS still runs but notification hidden. OEM battery killers (Xiaomi/Huawei/Samsung) kill background work unpredictably — don't promise perpetual execution.

### Plugins (`feature = "plugins"`)

**API:**
```rust
host.load_plugin(PluginSource::Wasm(bytes|path)) -> Result<PluginId,_>; // canonical
host.load_plugin(PluginSource::Dex(path, entry_class));                 // Android-only, verified
host.unload_plugin(id);
```

**Mechanism:** **reuse `egui-mobile-plugin-host` (wasmtime 46) verbatim** — the WASM plugin ABI is identical on aarch64 Android. Add an `AndroidOps` impl of `HostOps` mirroring `IosOps` (same op names: haptic/notify/url.open/clipboard.set/share.file/keyboard.set/net.*), main-thread ops drained into the Android Host each frame, net ops on background threads. **PluginManager/loader/engine unchanged.**

**Engine backend decision:** the research disagrees — one source says Android permits JIT so use **Cranelift** (faster, drop the Pulley/`memory_reservation(0)` workarounds); another warns SELinux `execmem` policy blocks RWX on many devices, so use **Pulley** like iOS. **Decision: default to Pulley for portability and zero surprises, expose Cranelift behind a `feature = "plugins-jit"` for sideload/known-device builds.** One code path, opt-in speed.

**Escape hatches:** `DexClassLoader` for downloaded JVM/dex plugins (works — ART, not native W^X — but **signature-verify against a pinned key before loading**, and it's Play-restricted). Native `.so` via `libloading` **only from the APK's own `nativeLibraryDir`** — downloaded native code is blocked by W^X + SELinux + linker namespace on non-rooted API24+, and `dlclose` of a Rust cdylib crashes on aarch64 (rust#135815), so load-once-never-unload.

**Manifest:** none for WASM. Android 14 requires dynamically-loaded dex be read-only (`chmod 0444` before writing) for the Dex path.

### Overlays (`feature = "overlays"`)

**API:**
```rust
host.can_draw_overlays() -> bool;
host.request_overlay_permission();          // ACTION_MANAGE_OVERLAY_PERMISSION
host.show_overlay(OverlaySpec{kind, gravity, x,y,w,h, focusable, pass_through}) -> OverlayHandle;
host.update_overlay(h, x,y,w,h_);
host.hide_overlay(h);
egui_android::overlay!(OverlayApp::new); // 2nd EguiApp rendered into the overlay surface
```

**Mechanism:** gate on `Settings.canDrawOverlays`. From `EguiService` (overlays die with a backgrounded Activity — they need a running Service), `WindowManager.addView(surfaceView, LayoutParams(TYPE_APPLICATION_OVERLAY, FLAG_NOT_FOCUSABLE, TRANSLUCENT))`. Take that SurfaceView's `ANativeWindow` → a **second wgpu Surface + second egui Context + Renderer** → the overlay app reuses the whole egui/wgpu stack. `OverlayKind::ChatHead` is a plain native ImageView with an `OnTouchListener` updating `lp.x/lp.y`.

**Manifest/permissions:** `SYSTEM_ALERT_WINDOW` (special access, Settings grant, not a runtime dialog). minSdk 26 for `TYPE_APPLICATION_OVERLAY`.

**Caveats:** overlays require a **foreground Service** to persist. **Android 15:** the overlay window must be **created and visible before** starting the FGS that maintains it, else `ForegroundServiceStartNotAllowedException` — order overlay-show before service-start. Text input into an overlay is awkward (`FLAG_NOT_FOCUSABLE` blocks the IME; making it focusable steals focus). Play scrutinizes `SYSTEM_ALERT_WINDOW`.

---

## 7. The capability catalog (Host method list, tiered)

Legend: **perm** = manifest/runtime permission; **A?** = Android-only (blank = common with iOS); **route** = poll(UI)/jni(read)/hybrid(stream).

### Tier 1 — must-have (iOS parity + the four marquee); ship in v1

| Method | perm | A? | route |
|---|---|---|---|
| `notify` / `ensure_channel` | POST_NOTIFICATIONS (33+); channels 26+ | | poll |
| `toast` | — | | poll |
| `share_file` / `share_text` | — (FileProvider) | | poll |
| `open_url` | — (+`<queries>`) | | poll |
| `copy_text` / `read_clipboard` | — (bg read blocked 29+) | | jni |
| `haptic` / `vibrate` | VIBRATE | | jni (sync) |
| `pick_file` / `create_file` / `pick_directory` | — (SAF) | | poll→push |
| `keep_screen_on` | — | | poll |
| `start_camera_preview` / `stop_camera_preview` / `capture_photo` | CAMERA (preview) | | poll+jni |
| `start_mic` / `stop_mic` / `mic_level` | RECORD_AUDIO | | poll+push |
| `request_permission` / `permission` / `permission_status` | — | | poll→push |
| `battery` / `device_info` / `network_status` | ACCESS_NETWORK_STATE | | jni |
| `secure_put/get/delete` | — (Keystore) | | jni |
| **`self_update`** + install-result | REQUEST_INSTALL_PACKAGES | ✅ | poll→push |
| **`start_service`/`stop_service`/`schedule_work`/`schedule_exact_alarm`** | FOREGROUND_SERVICE(+type), POST_NOTIFICATIONS, SCHEDULE_EXACT_ALARM, RECEIVE_BOOT_COMPLETED | ✅ | poll+service |
| **`load_plugin`/`unload_plugin`** | — (WASM) | ✅(exposed) | native |
| **`show_overlay`/`hide_overlay`/`request_overlay_permission`** | SYSTEM_ALERT_WINDOW | ✅ | poll |
| `intent` / `with_activity` (escape hatch) | per-intent | ✅ | poll/jni |

### Tier 2 — high-value; v2

| Method | perm | A? | route |
|---|---|---|---|
| `authenticate` (BiometricPrompt) / `keystore_sign` | USE_BIOMETRIC | | poll→push |
| `start_location`/`stop_location`/`last_location` | ACCESS_FINE/COARSE_LOCATION (+BACKGROUND, FGS type) | | hybrid |
| `start_sensor`/`stop_sensor`/`sensor_value` | — (HIGH_SAMPLING_RATE 31+) | | hybrid |
| `ble_scan`/`ble_connect`/`ble_write`/`ble_subscribe`/`take_ble_event` | BLUETOOTH_SCAN/CONNECT (31+) | | hybrid |
| `pick_contact`/`read_contacts` | READ_CONTACTS (read) | | poll/jni |
| `set_shortcuts`/`request_pin_shortcut` | — | ✅ | jni |
| `take_incoming` (deep links / share-target) | — (autoVerify) | | jni→push |
| `request_screen_capture`/`start_screen_record` (MediaProjection) | per-session consent + FGS mediaProjection | ✅ | poll+FGS |
| `set_tile`/`take_tile_click`/`request_add_tile` (QS tile) | BIND_QUICK_SETTINGS_TILE | ✅ | jni |
| `update_widget`/`request_pin_widget` (App Widget) | BIND_APPWIDGET | ✅ | jni |
| `set_wallpaper` | SET_WALLPAPER | ✅ | poll/jni |
| `request_ignore_battery_optimizations` | REQUEST_IGNORE_BATTERY_OPTIMIZATIONS (Play-restricted) | ✅ | poll |

### Tier 3 — advanced / policy-sensitive; v3, sideload/enterprise-gated

| Method | perm | A? | route |
|---|---|---|---|
| `enable_nfc_reader`/`take_nfc_tag`; `hce_register_aid`/`hce_respond` | NFC | ✅(HCE) | jni/service |
| `ble_advertise`/`ble_gatt_server`/`ble_notify` | BLUETOOTH_ADVERTISE | ✅ | hybrid |
| `wifi_direct_discover`/`connect`/`take_p2p_event` | NEARBY_WIFI_DEVICES (33+) | ✅ | hybrid |
| `send_sms`/`read_sms`/`dial`/`telephony_info` | SEND_SMS/READ_SMS/CALL_PHONE (Play-restricted) | ✅ | poll/jni |
| `ime_commit_text`/`ime_send_key` + `ime_app!` | BIND_INPUT_METHOD | ✅ | service |
| `request_role`/`is_role_held` (launcher/browser/dialer/assistant) | RoleManager | ✅ | poll→push |
| `take_a11y_event`/`perform_global_action`/`dispatch_gesture` | BIND_ACCESSIBILITY_SERVICE (Play-restricted) | ✅ | service |
| `load_dex_module`/`call_dex` | — (signature-verify) | ✅ | native |

The build tool **synthesizes only the manifest entries for enabled cargo features** — so a Tier-1 app never declares SMS/overlay/accessibility permissions that trip Play review.

---

## 8. The build tool — `cargo-egui-android`

**Decision: a sibling `cargo-egui-android` that wraps/vendors `cargo-apk2 1.3.11`**, the direct analog of how `cargo-egui-ios` wraps xtool. Do not invent a packager. cargo-apk2 is Gradle-free, reads `[package.metadata.android]` from Cargo.toml, auto-creates `~/.android/debug.keystore`, compiles bundled Kotlin → DEX, bundles `libc++_shared.so`, and its `run` does `adb install` + launch. (`cargo-ndk 4.1.2` is the compile-only fallback if we ever emit a real Gradle project.)

**Subcommands:**

- **`cargo egui-android new <name>`** — scaffold a cdylib crate from an embedded template: `android_main` via `egui_mobile::app!`, the `[package.metadata.android]` block, and (only if a Kotlin-needing feature is enabled) the bundled `EguiHost.kt`/`EguiService.kt`/`EguiActivity.kt`.
- **`cargo egui-android build [--release]`** — synthesize the manifest block from enabled features, shell to cargo-apk2 to cross-compile + package + sign. Emits 16 KB-aligned `.so` (NDK r28+ default; else `-Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384`).
- **`cargo egui-android run`** — `adb install -r` + `am start` + `adb logcat` tail.

**Toolchain setup (one-time, on the fresh Manjaro box):**

```bash
# JDK + SDK cmdline-tools + platform-tools (adb) + NDK r28 + build-tools + platform-35
sdkmanager "platform-tools" "platforms;android-35" "build-tools;35.0.0" "ndk;28.0.12674087"
export ANDROID_HOME=$HOME/Android/Sdk ANDROID_NDK_HOME=$ANDROID_HOME/ndk/28.0.12674087
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-apk2 cargo-ndk
```

**Targets:** `aarch64-linux-android` (arm64-v8a) is the only must-ship ABI for real devices; add `x86_64-linux-android` for the emulator. minSdk 26, targetSdk 35.

**MVP zero-Kotlin manifest block (render-only, NativeActivity, `has_code=false`):**

```toml
[package.metadata.android.sdk]
min_sdk_version = 26
target_sdk_version = 35
[package.metadata.android.application]
has_code = false
[[package.metadata.android.application.activity]]
name = "android.app.NativeActivity"
[[package.metadata.android.application.activity.meta_data]]
name = "android.app.lib_name"   # = your cdylib name
[[package.metadata.android.application.activity.intent_filter]]
actions = ["android.intent.action.MAIN"]
categories = ["android.intent.category.LAUNCHER"]
```

For GameActivity/IME/services: `has_code = true`, activity name → the games-activity class, add `kotlin_sources`, winit/android-activity feature `game-activity`.

**Keystore / self-signed (no cert treadmill):** cargo-apk2 auto-generates `~/.android/debug.keystore` on first dev build — **zero manual keytool**. For release **and** for self-update, generate a stable keystore **once** and point `[package.metadata.android.signing.release]` (or `CARGO_APK_RELEASE_KEYSTORE`) at it — the self-update APK must be signed with the same key as the installed app or `PackageInstaller` rejects it. `cargo egui-android new` writes a `keygen` helper that runs the one-time `keytool` invocation and records the path.

---

## 9. Phased implementation roadmap

**Phase 0 — Foundation & version lockstep (no Android device yet).**
- Extract `egui-mobile-core`: `EguiApp`, `CreateContext`, `HostRequest`/`HostEvent`, `HostBackend`, value types, and the reusable RenderCore half (Context + tessellate/paint + input synthesis). Refactor `egui-ios` onto it with `FfiPollBackend`.
- Bump iOS from egui 0.34/wgpu 29 → egui **0.35**/wgpu 29. Rename `egui-ios-plugin-*` → `egui-mobile-plugin-*`.
- Create the `egui-mobile` facade crate.
- **Verify:** existing iOS app still builds and runs unchanged (`cargo egui-ios run`); `cargo check` of `egui-mobile-core` on the Linux host with no platform deps. This de-risks the refactor before any Android code exists.

**Phase 1 — Android toolchain + MVP render loop.**
- Install SDK/NDK/cargo-apk2/adb/targets (Section 8). This gates everything Android.
- Build `egui-android` skeleton: `android_main`, android-activity **native-activity** first (zero Kotlin), RenderCore with the surface split, Vulkan|GL, the input mapper.
- Build `cargo-egui-android new/build/run` around cargo-apk2 with the zero-Kotlin manifest.
- **Verify:** a hello-triangle `impl EguiApp` renders on the emulator (x86_64) and, if a device is available, on arm64. Explicitly test **background→foreground** (surface drop/recreate — the #1 bug) and **rotate/fold** (density/`ConfigChanged`). Confirm the *same* app crate that runs on iOS renders here with only the target flag changed.

**Phase 2 — JNI Host bridge (common surface) + GameActivity/IME.**
- Switch to GameActivity; wire GameTextInput diff → egui events; `show/hide_soft_input`. Bundle `EguiHost.kt` + `EguiActivity`.
- Implement `JniBackend` and all **Tier-1 common** methods (notify, toast, share, open_url, clipboard, haptic, pick_file, permissions, camera, mic, battery/device/network, secure storage, insets). Wire the async native setters via `RegisterNatives`.
- **Verify:** drive each capability from a test app; confirm `pick_file`/`request_permission` results flow back through `take_picked_file`/`permission` exactly as on iOS. Diff the app source against the iOS build — must be byte-identical except the target.

**Phase 3 — Marquee capabilities.**
- Plugins first (lowest risk — reuse the wasmtime host + `AndroidOps`; verify a WASM plugin built for iOS loads on Android unchanged). Then self-update (`PackageInstaller`, sideload keystore), background services (`EguiService.kt` + WorkManager), overlays (second wgpu surface).
- **Verify each:** WASM plugin round-trip iOS↔Android; self-update installs a v+1 APK signed with the same key (confirm the confirm-dialog path); an FGS survives backgrounding and ticks Rust; an egui overlay draws over the launcher. Test on Android 13/14/15 images for the behavior gates (FGS types, POST_NOTIFICATIONS, W^X, overlay+FGS ordering).

**Phase 4 — Broad catalog (Tiers 2–3), feature-gated.**
- Biometrics, location, sensors, BLE, deep links, MediaProjection, tiles, widgets, wallpaper (Tier 2); NFC/HCE, Wi-Fi Direct, SMS, custom IME, roles, accessibility (Tier 3, sideload-gated).
- Extend the build tool's manifest synthesis to emit permissions/components/intent-filters per enabled feature.
- **Verify:** each behind its cargo feature; confirm a Tier-1-only build declares zero Tier-2/3 permissions (Play-review cleanliness). Add the async-result plumbing tests for streaming capabilities (sensors/BLE/location push into pushed-state getters).

**Cross-cutting verification at every phase:** a shared example app in the workspace that compiles for both targets from one source; CI that runs `cargo egui-ios build` and `cargo egui-android build` on every PR (cap `cargo test` with `ulimit` per the workspace memory rail).

---

## 10. Risks / unknowns and the Android-only superpowers

**Risks & unknowns:**

- **jni 0.22 maturity.** The Feb–Mar 2026 rewrite is bleeding-edge; every online snippet is 0.21. *Mitigation:* ship a compiling spike on 0.22.4 in Phase 1; keep 0.21.1 as a pinned fallback (obtain the raw VM from ndk-context so the ecosystem's internal jni version is irrelevant).
- **Plugin engine backend (Cranelift vs Pulley) on Android is genuinely disputed** — JIT may or may not be blocked by SELinux `execmem` on a given device. *Mitigation:* default Pulley, `feature = "plugins-jit"` opt-in Cranelift; test on a range of devices before recommending JIT.
- **GameActivity trade-offs:** solid IME but needs the games-activity AAR (DEX) and a reported scroll-smoothness regression (egui#5652). *Mitigation:* keep native-activity as the zero-Kotlin render-only path; ship GameActivity when text entry matters.
- **16 KB pages (Android 15+):** the `.so` must be aligned or `dlopen` fails. *Mitigation:* pin NDK r28+ (aligns by default); add linker flags for r27.
- **Play policy** blocks self-update, downloaded native/dex code, `SYSTEM_ALERT_WINDOW`, SMS, accessibility. *Mitigation:* these are opt-in features gated for sideload/enterprise/F-Droid; WASM is the Play-safe hot-update path; the build tool keeps them out of default manifests.
- **wgpu 29 pin:** wgpu 30 is stable but needs egui 0.36+. *Mitigation:* stay on 29 until a coordinated egui 0.36 bump across both platforms.
- **OEM battery killers** make perpetual background execution unreliable regardless of correct FGS code — a product-expectation risk, not a bug.
- **Surface-across-suspend** has no iOS analog and is the easiest correctness bug to reintroduce when porting — enforce the split-state RenderCore invariant in code review.

**Android-only superpowers this unlocks vs iOS** (all impossible or heavily sandboxed on iOS):

1. **Draw over other apps** — floating egui panels / chat-heads via `TYPE_APPLICATION_OVERLAY`.
2. **Self-update the APK in-app** via `PackageInstaller` (sideload/enterprise).
3. **Unrestricted long-running foreground services + WorkManager** hosting headless Rust.
4. **Runtime plugins with a real JIT option** (Cranelift) — faster than iOS's interpreter-only Pulley.
5. **Become the default launcher / browser / dialer / assistant** (RoleManager).
6. **System-wide custom IME** — an egui-powered keyboard for the whole OS.
7. **Quick-settings tiles, home-screen widgets, live wallpaper** hosting egui.
8. **NFC card emulation (HCE), Wi-Fi Direct, BLE peripheral/GATT server.**
9. **Full-screen capture (MediaProjection), SMS/telephony, accessibility automation.**
10. **A generic `intent()` + `with_activity()` raw-JNI escape hatch** — curated typed methods cover the common 90%, the escape hatch reaches *any* Android API, so egui-android out-reaches the iOS Host despite Android's far larger surface.

The iOS Host stays exactly as capable as it is; the shared core guarantees an app written once gains all of the above for free the moment it's built for Android — with no change to a single line of `impl EguiApp`.

---

Key files (all absolute):
- `$HOME/Documents/Rust/IOS/ios-egui/crates/egui-mobile-core/` (new shared core)
- `$HOME/Documents/Rust/IOS/ios-egui/crates/egui-android/` (new runtime)
- `$HOME/Documents/Rust/IOS/ios-egui/crates/egui-mobile/` (new facade)
- `$HOME/Documents/Rust/IOS/ios-egui/tools/cargo-egui-android/` (new build tool)
- `$HOME/Documents/Rust/IOS/ios-egui/kotlin/{EguiHost,EguiService,EguiActivity}.kt` (bundled shims)
- Refactor targets in existing `$HOME/Documents/Rust/IOS/ios-egui/crates/egui-ios/src/{lib,render_core,host,input,plugins}.rs`