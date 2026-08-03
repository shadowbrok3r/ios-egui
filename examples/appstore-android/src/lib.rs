//! App Store — installs and updates the personal APK catalog served by the appstore server.
//!
//! `app` holds the UI and is Android-only (it needs `HostExt` for PackageInstaller);
//! `icons` stays host-buildable so its font-coverage test runs in a normal `cargo test`.

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

mod icons;

#[cfg(target_os = "android")]
mod app;
