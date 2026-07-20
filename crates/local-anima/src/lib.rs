//! On-device Anima (DiT) text2img.
//!
//! Prompt parsing, Qwen/T5 tokenization and the f16 embedding lookup run on
//! CPU; `clip.bin`, both DiT halves and `vae_decoder.bin` run on the Qualcomm
//! HTP NPU through [`qnn_rs::Context`]. Pure logic (weight parsing, f16
//! conversion, schedule, latent normalization, image mapping) is host-testable;
//! device paths ([`pipeline::text2img`]) compile on host and `aarch64-android`
//! but need an NPU + QNN libs + a model pack at runtime.
//!
//! ```no_run
//! use local_anima::{AnimaPack, AnimaParams, text2img, Backend, QnnSystem, Session, prepare_htp_env};
//! # fn f() -> local_anima::Result<()> {
//! let lib_dir = std::path::Path::new("/data/local/tmp/qnn");
//! prepare_htp_env(lib_dir);
//! let system = QnnSystem::load(lib_dir.join("libQnnSystem.so"))?;
//! let backend = Backend::load(lib_dir.join("libQnnHtp.so"))?;
//! let session = Session::new(&backend)?;
//! session.set_htp_performance_mode()?;
//! let pack = AnimaPack::open("/sdcard/.../anima")?;
//! let params = AnimaParams::from_pack(&pack);
//! let image = text2img(&pack, &session, &system, "a cat", &params, |i, n| println!("{i}/{n}"))?;
//! image.save_png("out.png")?;
//! # Ok(()) }
//! ```

mod error;
pub mod img;
pub mod latent;
pub mod pack;
pub mod pipeline;
pub mod scheduler;
pub mod text;
pub mod tokenizer;

pub use error::{Error, Result};
pub use img::{vae_output_to_image, Image};
pub use latent::{model_to_vae, vae_to_model, LATENT_CHANNELS, VAE_SCALE, WAN_MEAN, WAN_STD};
pub use pack::{AnimaConfig, AnimaPack};
pub use pipeline::{
    apply_cfg, decode_latents, dit_velocity, encode_text, text2img, text2img_cancellable, AnimaParams, CONTEXT_DIM,
    CONTEXT_SEQ, GRAPH,
};
pub use qnn_rs::{prepare_htp_env, Backend, Context as QnnContext, ContextOpts, QnnSystem, Session, TensorIn};
pub use scheduler::{eta_for, sigma_schedule, Scheduler, MULTIPLIER, SHIFT};
pub use text::{
    build_clip_inputs, f16_to_f32, parse_weights, plain_text, qwen_embedding, t5_finalize, t5_inputs, weighted_ids,
    ClipInputs, EmbedTable, QWEN_HIDDEN, QWEN_PAD, QWEN_SEQ, T5_EOS, T5_SEQ,
};
pub use tokenizer::AnimaTokenizers;
