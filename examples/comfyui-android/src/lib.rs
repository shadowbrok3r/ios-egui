//! A streamlined ComfyUI client for Android, built on egui-mobile. Connect to a ComfyUI server,
//! pick a checkpoint, write prompts, tune the core sampler params, and generate — txt2img plus a
//! lightweight img2img (refine the current result, or an image URL).
//!
//! The crate splits into a portable core (engine, schema, types, workflow — compiles and tests on
//! the host: `OBJECT_INFO_JSON=<fixture> cargo test -p comfyui_android`) and the Android-only UI
//! shell in [`app`].

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

mod engine;
mod logger;
mod schema;
mod types;
mod workflow;

#[cfg(target_os = "android")]
mod app;
