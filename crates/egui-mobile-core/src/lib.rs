//! Platform-neutral core shared by `egui-ios` and `egui-android`. Owns the [`EguiApp`] trait and
//! the [`Host`] capability bridge; the per-platform runtime crates drive them via the doc-hidden
//! `drv_*` methods. Depends only on `egui`.

pub use egui;

mod app;
mod host;

pub use app::{CreateContext, EguiApp};
pub use host::{Haptic, Host, HostRequest, Insets, Permission};
