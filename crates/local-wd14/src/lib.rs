//! On-device WD14 danbooru tagger.
//!
//! Preprocessing (RGBA -> white-padded BGR NHWC f32) and postprocessing
//! (per-class probabilities -> thresholded, ranked general/character tags) are
//! pure and host-testable; the classifier graph ([`tag`]) runs on the Qualcomm
//! HTP NPU through [`qnn_rs::Context`]. Device paths compile on host and
//! `aarch64-android` but need an NPU + QNN libs + a tagger pack at runtime.
//!
//! ```no_run
//! use local_wd14::{Wd14Pack, Wd14Params, tag_bytes, Backend, QnnSystem, Session, prepare_htp_env};
//! # fn f() -> local_wd14::Result<()> {
//! let lib_dir = std::path::Path::new("/data/local/tmp/qnn");
//! prepare_htp_env(lib_dir);
//! let system = QnnSystem::load(lib_dir.join("libQnnSystem.so"))?;
//! let backend = Backend::load(lib_dir.join("libQnnHtp.so"))?;
//! let session = Session::new(&backend)?;
//! session.set_htp_performance_mode()?;
//! let pack = Wd14Pack::open("/sdcard/.../wd14")?;
//! let png = std::fs::read("in.png")?;
//! let result = tag_bytes(&pack, &session, &system, &png, &Wd14Params::default())?;
//! for t in &result.general { println!("{}  {}%", t.name, t.percent()); }
//! # Ok(()) }
//! ```

mod error;
pub mod img;
pub mod pack;
pub mod tagger;

pub use error::{Error, Result};
pub use img::{composite_over_white, fit_dims, preprocess, rgb_to_bgr_nhwc, INPUT_SIZE};
pub use pack::{
    parse_tags_csv, Wd14Pack, Wd14Tag, CATEGORY_CHARACTER, CATEGORY_GENERAL, CATEGORY_RATING, MARKER,
};
pub use qnn_rs::{prepare_htp_env, Backend, Context as QnnContext, ContextOpts, QnnSystem, Session};
pub use tagger::{infer, tag, tag_bytes, ScoredTag, TagResult, Wd14Params};
