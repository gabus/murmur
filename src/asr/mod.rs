//! Speech recognition: the re-check engine and its worker thread.

pub mod committer;
pub mod engine;
pub mod parakeet;
pub mod words;
pub mod worker;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("model file missing: {0}")]
    ModelMissing(PathBuf),
    #[error("failed to load model from {0}")]
    ModelLoad(PathBuf),
    #[error("decoding failed")]
    Decode,
}
