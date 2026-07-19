//! On-device Stable Diffusion 1.5 text2img.
//!
//! CLIP text encoding runs on CPU (`candle-transformers`); UNet and VAE run on
//! the Qualcomm HTP NPU through [`qnn_rs::Context`]. Pure logic (quant IO,
//! schedulers, tokenizer wrapping, image conversion) is host-testable; device
//! paths ([`pipeline::text2img`], [`selftest::device_selftest`]) compile on host
//! and `aarch64-android` but need an NPU + QNN libs + context binaries at runtime.
//!
//! ```no_run
//! use local_sd::{ClipTokenizer, ClipTextEncoder, Text2ImgParams, text2img};
//! # fn f(unet: &qnn_rs::Context, vae: &qnn_rs::Context) -> local_sd::Result<()> {
//! let tok = ClipTokenizer::from_file("tokenizer.json")?;
//! let clip = ClipTextEncoder::from_safetensors("clip.safetensors")?;
//! let params = Text2ImgParams::default();
//! let image = text2img(&tok, &clip, unet, vae, "a cat", "", &params, |step, total, _preview| {
//!     println!("{step}/{total}");
//! }, None)?;
//! std::fs::write("out.png", image.to_png()?)?;
//! # Ok(()) }
//! ```

pub mod clip;
mod error;
pub mod img;
pub mod pipeline;
pub mod quant;
pub mod scheduler;
pub mod selftest;
pub mod text2img_selftest;
pub mod tokenizer;

pub use clip::ClipTextEncoder;
pub use error::{Error, Result};
pub use img::Image;
pub use pipeline::{text2img, Text2ImgParams, VAE_SCALE};
pub use qnn_rs::{prepare_htp_env, set_htp_performance_mode, Backend, Context as QnnContext, QnnSystem};
pub use scheduler::{Sampler, Scheduler};
pub use selftest::{device_selftest, GraphSummary, OutputStats, SelftestConfig, SelftestReport, TensorSummary};
pub use text2img_selftest::{device_text2img, Text2ImgConfig, Text2ImgReport};
pub use tokenizer::ClipTokenizer;
