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

pub use egui_mobile_core::{CreateContext, EguiApp, Haptic, Host, Insets, Permission, egui};

/// The entry-point macro. Emits the iOS C ABI when building for iOS and `android_main` when
/// building for Android. `$crate` inside the re-exported macro still resolves to the backend
/// crate, so an app depending only on `egui-mobile` builds correctly for either target.
#[cfg(target_os = "ios")]
pub use egui_ios::app;

#[cfg(target_os = "android")]
pub use egui_android::app;

/// Android-only host capabilities (self-update, install/overlay permissions, …). Gate calls
/// behind `#[cfg(target_os = "android")]` in shared app code.
#[cfg(target_os = "android")]
pub use egui_android::HostExt;
