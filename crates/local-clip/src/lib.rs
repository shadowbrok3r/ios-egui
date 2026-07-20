//! On-device CLIP image embeddings + LAION aesthetic score.
//!
//! Preprocessing (RGB -> CLIP-normalized NCHW f32) and the vector math
//! (L2-normalize, cosine, aesthetic linear head) are pure and host-testable;
//! the visual tower ([`embed`]) runs on the Qualcomm HTP NPU through
//! [`qnn_rs::Context`]. Device paths compile on host and `aarch64-android` but
//! need an NPU + QNN libs + a CLIP pack at runtime. The device helpers return an
//! L2-normalized embedding, which [`cosine`] and [`aesthetic_score`] expect.
//!
//! ```no_run
//! use local_clip::{ClipPack, embed_bytes, aesthetic_score, Backend, QnnSystem, Session, prepare_htp_env};
//! # fn f() -> local_clip::Result<()> {
//! let lib_dir = std::path::Path::new("/data/local/tmp/qnn");
//! prepare_htp_env(lib_dir);
//! let system = QnnSystem::load(lib_dir.join("libQnnSystem.so"))?;
//! let backend = Backend::load(lib_dir.join("libQnnHtp.so"))?;
//! let session = Session::new(&backend)?;
//! session.set_htp_performance_mode()?;
//! let pack = ClipPack::open("/sdcard/.../clip")?;
//! let png = std::fs::read("in.png")?;
//! let emb = embed_bytes(&pack, &session, &system, &png)?;
//! if let Some(head) = pack.aesthetic()? {
//!     println!("aesthetic {:.2}", aesthetic_score(&head, &emb));
//! }
//! # Ok(()) }
//! ```

mod error;
pub mod embed;
pub mod img;
pub mod pack;
pub mod text;

pub use embed::{aesthetic_score, cosine, embed, embed_bytes, embed_image, l2_normalize};
pub use error::{Error, Result};
pub use img::{center_crop_offset, fit_shortest, normalize_nchw, preprocess, CLIP_MEAN, CLIP_STD, INPUT_SIZE};
pub use pack::{parse_aesthetic, AestheticHead, ClipPack, AESTHETIC_FILE, MARKER, TEXT_MODEL_FILE, TOKENIZER_FILE};
pub use text::{build_inputs, embed_text, encode_query, BOS, CONTEXT_LEN, EOS};
pub use qnn_rs::{prepare_htp_env, Backend, Context as QnnContext, ContextOpts, QnnSystem, Session};
