//! Speech recognition: the re-check engine and its worker thread.

pub mod committer;
pub mod engine;
pub mod moonshine;
pub mod parakeet;
pub mod words;
pub mod worker;

use std::path::PathBuf;

pub use engine::EngineStats;
use words::Word;

/// A speech-to-text engine as the worker sees it: audio in, final words out.
pub trait Engine: Send {
    /// Feeds 16 kHz mono audio; returns words that became final.
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError>;
    /// Ends the session: transcribes what's left and returns the remaining words.
    fn finish(&mut self) -> Result<Vec<Word>, AsrError>;
    /// Starts a new session.
    fn reset(&mut self);
    fn stats(&self) -> EngineStats;
}

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("model file missing: {0}")]
    ModelMissing(PathBuf),
    #[error("failed to load model from {0}")]
    ModelLoad(PathBuf),
    #[error("decoding failed")]
    Decode,
    #[error("unknown model {0}")]
    UnknownModel(String),
}
