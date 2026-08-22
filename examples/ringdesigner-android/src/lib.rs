//! RingDesigner on Android.
//!
//! Phase 0: the device bench. The UI is one button, and its whole job is to answer the questions
//! the desktop cannot — which build preset is interactive on this phone, and where the milliseconds
//! actually go — before anything is designed around an extrapolation from a 16-core desktop.
//!
//! The crate splits into a portable core (`bench`, host-testable with
//! `cargo test -p ringdesigner_android`) and the Android-only shell in [`app`].

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

pub mod bench;
pub mod theme;
pub mod canvas;
pub mod library;
pub mod paint;
pub mod camera;
pub mod ring;
pub mod util;
pub mod viewport;

pub mod frost;
pub mod graph;

#[cfg(target_os = "android")]
mod app;

// Glow because the mesh is drawn by a raw `egui_glow::CallbackFn`, and 24 because eframe asks for a
// 0-bit depth buffer by default — with no depth attachment the far wall of the ring paints over the
// near one and nothing reports an error.
#[cfg(target_os = "android")]
egui_mobile::app!(app::RingApp::new, egui_mobile::Backend::Glow, 24);
