//! Surveyor — an honest RuView calibration companion.
//!
//! Connects to the wifi-densepose sensing server over the LAN and shows only
//! what is actually measured: the room, the CSI localization blob, and the
//! LD2450 radar targets, with session record controls for calibration walks.
//! Nothing speculative, nothing the pipeline can't back with data.
//!
//! Portable core (config, mount, proto, roommap, net — host-testable via
//! `cargo test -p surveyor_android`) plus the Android-only UI in [`app`].

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

pub mod config;
pub mod frost;
pub mod mount;
pub mod net;
pub mod proto;
pub mod pulsecard;
pub mod roommap;
pub mod theme;

#[cfg(target_os = "android")]
mod app;
