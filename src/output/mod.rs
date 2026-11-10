//! Typing text into the focused app through the RemoteDesktop portal's EIS channel.
//! TEMPORARY STUB with the agreed interface. Replaced by the real
//! implementation (being built on a separate branch).

use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

pub enum EisSource {
    Portal { token_path: std::path::PathBuf },
    KwinDirect,
}

#[derive(Debug, Clone)]
pub enum OutputEvent {
    Ready,
    PermissionDenied,
    Disconnected(String),
    Unavailable(String),
    Unmapped { count: usize },
}

pub struct Output;

impl Output {
    pub fn spawn(_source: EisSource, _events: UnboundedSender<OutputEvent>) -> Self {
        Self
    }

    pub fn type_text(&self, text: String) {
        debug!(len = text.len(), "stub output");
    }

    pub async fn flush(&self) {}
}
