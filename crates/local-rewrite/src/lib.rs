//! On-device CPU prompt rewriter.
//!
//! A quantized Qwen2.5-0.5B-Instruct GGUF is run through candle's
//! [`candle_transformers::models::quantized_qwen2`] on the CPU to rewrite a prompt:
//! danbooru tags <-> natural-language prose and model-family dialect swaps. The prompt
//! templates ([`templates`]) are pure string logic and host-testable; the model itself
//! needs a [`RewritePack`] at runtime (marker `RWTR`, `model.gguf`, `tokenizer.json`).
//!
//! ```no_run
//! use local_rewrite::{Rewriter, RewriteKind};
//! # fn f() -> local_rewrite::Result<()> {
//! let rw = Rewriter::open("/storage/emulated/0/ComfyUI/rewrite")?;
//! let out = rw.rewrite(RewriteKind::TagsToVideo.system(), "1girl, running, rain", 128)?;
//! println!("{out}");
//! # Ok(()) }
//! ```

mod error;
pub mod pack;
pub mod rewriter;
pub mod templates;

pub use error::{Error, Result};
pub use pack::{RewritePack, MARKER, MODEL_FILE, TOKENIZER_FILE};
pub use rewriter::Rewriter;
pub use templates::{
    build_prompt, convert_family, PromptFamily, RewriteKind, ILLUSTRIOUS_QUALITY_BLOCK,
    PONY_QUALITY_BLOCK, SYS_FAMILY_TO_ILLUSTRIOUS, SYS_FAMILY_TO_PONY, SYS_PROSE_TO_TAGS,
    SYS_TAGS_TO_VIDEO,
};
