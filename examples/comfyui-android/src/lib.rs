//! A streamlined ComfyUI client for Android, built on egui-mobile. Connect to a ComfyUI server,
//! pick a checkpoint, write prompts, tune the core sampler params, and generate — txt2img plus a
//! lightweight img2img (refine the current result, or an image URL).
//!
//! Post-processing (upscalers, face fixes) is data: see [`apps`] and `src/apps_builtin/*.json`.
//!
//! The crate splits into a portable core (apps, engine, schema, types, workflow — compiles and
//! tests on the host: `OBJECT_INFO_JSON=<fixture> cargo test -p comfyui_android`) and the
//! Android-only UI shell in [`app`].

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

mod apps;
mod backup;
mod cooc;
mod engine;
mod gallery;
mod graphview;
mod history;
mod icons;
pub mod lint;
mod logger;
pub mod mask;
mod schema;
mod clip_index;
mod tag_index;
pub mod tags;
mod theme;
mod types;
mod uiwf;
mod workflow;

#[cfg(target_os = "android")]
mod app;
#[cfg(feature = "local-npu")]
mod local_engine;
#[cfg(target_os = "android")]
mod player;
