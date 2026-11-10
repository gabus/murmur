//! murmur: live voice dictation for Wayland. See README.md for the spec.

pub mod agc;
pub mod app;
pub mod asr;
pub mod audio;
pub mod autostart;
pub mod config;
#[cfg(feature = "dev-harness")]
pub mod dev;
pub mod download;
pub mod hotkey;
pub mod icons;
pub mod instance;
pub mod model_manager;
pub mod models;
pub mod notify;
pub mod output;
pub mod paths;
pub mod portal;
pub mod tray;
pub mod ui;
pub mod ui_host;
pub mod vad;
