//! murmur: live voice dictation for Wayland. See README.md for the spec.

pub mod agc;
pub mod app;
pub mod asr;
pub mod audio;
#[cfg(feature = "dev-harness")]
pub mod dev;
pub mod hotkey;
pub mod output;
pub mod paths;
pub mod portal;
pub mod vad;
