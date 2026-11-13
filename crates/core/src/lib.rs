//! shadersweep core: Steam discovery, cache sizing, safety guards and deletion.
//! No UI code lives here.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod gpu;
pub mod guard;
pub mod ops;
pub mod scan;
pub mod settings;
pub mod stats;
pub mod steam;
pub mod util;
pub mod vdf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("VDF parse error in {path}: {source}")]
    Vdf {
        path: std::path::PathBuf,
        source: vdf::VdfError,
    },
    #[error("cannot determine home directory")]
    NoHome,
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

impl CoreError {
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        CoreError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
