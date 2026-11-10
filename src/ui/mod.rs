//! Setup and Settings windows. They run in a separate process
//! (`murmur --ui setup|settings`) so the daemon never loads a GUI toolkit
//! while idle; see `protocol` for the pipe protocol.

mod components;
pub mod offscreen;
pub mod protocol;
pub mod samples;
mod view;
mod window;

pub use window::{APP_ID, UiError, run};
