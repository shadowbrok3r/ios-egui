//! The facade every cross-platform egui mobile app depends on. Re-exports the shared app API
//! ([`EguiApp`], [`Host`], value types, and `egui` itself) and selects the platform backend +
//! `app!` entry macro via `cfg(target_os)`.
//!
//! ```ignore
//! use egui_mobile::{egui, CreateContext, EguiApp, Host, app};
//!
//! struct MyApp;
//! impl MyApp { fn new(_: &CreateContext) -> Self { Self } }
//! impl EguiApp for MyApp {
//!     fn update(&mut self, ui: &mut egui::Ui, host: &Host) { ui.label("cross-platform"); }
//! }
//! app!(MyApp::new); // → C ABI on iOS, android_main on Android
//! ```

pub use egui_mobile_core::{CreateContext, EguiApp, Haptic, Host, Insets, Permission, StylusProbe, egui};

/// The entry-point macro. Emits the iOS C ABI when building for iOS and `android_main` when
/// building for Android. `$crate` inside the re-exported macro still resolves to the backend
/// crate, so an app depending only on `egui-mobile` builds correctly for either target.
#[cfg(target_os = "ios")]
pub use egui_ios::app;

#[cfg(target_os = "android")]
pub use egui_android::app;

/// Renderer selection, Android only. `Backend::Glow` is required for an OpenGL paint callback
/// (backdrop blur); everything else should stay on the default wgpu.
///
/// `run_with_depth` additionally asks for a depth attachment — eframe requests 0 bits by default, so
/// anything that depth-tests renders see-through without it. Reach it through `app!` with a third
/// argument: `app!(MyApp::new, Backend::Glow, 24)`.
#[cfg(target_os = "android")]
pub use egui_android::{Backend, glow_context, run_with, run_with_depth};

/// Android-only host capabilities (self-update, install/overlay permissions, …). Gate calls
/// behind `#[cfg(target_os = "android")]` in shared app code.
#[cfg(target_os = "android")]
pub use egui_android::HostExt;

/// Screen orientation lock (landscape / unspecified).
#[cfg(target_os = "android")]
pub use egui_android::ScreenOrientation;

/// Current display rotation in degrees (0 = portrait, 90 = landscape, …).
#[cfg(target_os = "android")]
pub use egui_android::device_orientation_deg;

/// Android-only frame-accurate video decoding (MediaMetadataRetriever) for in-app playback.
#[cfg(target_os = "android")]
pub use egui_android::video;

/// Android-only `VpnService` capture: consent, the foreground service, and the tun descriptor.
#[cfg(target_os = "android")]
pub use egui_android::vpn;

/// Tell the Android IME bridge a text buffer changed outside the IME (programmatic edit while
/// possibly focused), so it drops carried IME state and reseeds the hidden EditText.
#[cfg(target_os = "android")]
pub use egui_android::ime_bridge::notify_out_of_band_edit as note_ime_out_of_band_edit;
