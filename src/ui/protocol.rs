//! Messages between the daemon and the UI process (`murmur --ui setup|settings`).
//!
//! The daemon starts the UI process with stdin/stdout piped. Each message is one
//! line of JSON. The daemon sends [`ToUi`]; the UI sends [`FromUi`]. The UI quits
//! when stdin closes or it receives [`ToUi::Close`]. The daemon treats a closed
//! stdout as the window being closed.

use serde::{Deserialize, Serialize};

/// Which window the UI process shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Window {
    /// First-run page (D-009).
    Setup,
    /// Settings window (D-010).
    Settings,
}

impl Window {
    pub fn arg(self) -> &'static str {
        match self {
            Window::Setup => "setup",
            Window::Settings => "settings",
        }
    }

    pub fn from_arg(s: &str) -> Option<Self> {
        match s {
            "setup" => Some(Window::Setup),
            "settings" => Some(Window::Settings),
            _ => None,
        }
    }
}

/// Everything the windows display. Sent whole whenever anything changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UiState {
    pub shortcut: ShortcutState,
    pub typing: TypingState,
    pub models: Vec<ModelRow>,
    /// The running model download, if any.
    pub download: Option<DownloadRow>,
    /// Microphone names to offer; the empty string means "System default".
    pub microphones: Vec<String>,
    /// Selected microphone ("" = system default).
    pub microphone: String,
    /// Pause (seconds) after which a sentence is finished.
    pub pause_secs: f32,
    /// Free the model's memory after this many idle minutes (0 = never).
    pub idle_minutes: u32,
    /// Start murmur at login.
    pub autostart: bool,
    /// Why autostart couldn't be changed, if it failed.
    pub autostart_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ShortcutState {
    /// The bound key combination as the desktop describes it ("Meta+H"); None while unbound.
    pub trigger: Option<String>,
    /// The desktop can open its own dialog to change the shortcut.
    pub can_change: bool,
    /// The desktop's shortcut dialog is open (waiting for the user).
    pub pending: bool,
    /// Why there's no shortcut (e.g. no GlobalShortcuts portal), if so.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "state", content = "detail", rename_all = "lowercase")]
pub enum TypingState {
    /// Not asked yet.
    #[default]
    Unknown,
    /// The permission dialog is open.
    Asking,
    Granted,
    Denied,
    /// No usable RemoteDesktop portal (message for the user).
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRow {
    pub id: String,
    pub name: String,
    /// "Most accurate · 25 European languages"
    pub summary: String,
    /// "490 MB"
    pub size: String,
    pub installed: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadRow {
    pub model_id: String,
    pub percent: u8,
    pub unpacking: bool,
    /// Set when the download failed (the UI shows it with a retry option).
    pub error: Option<String>,
}

/// Daemon → UI.
// Sent a few times per second at most; boxing the state isn't worth the noise.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ToUi {
    State(UiState),
    /// Close the window (e.g. the daemon is quitting).
    Close,
}

/// UI → daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FromUi {
    /// Open the desktop's dialog to change the shortcut.
    ChangeShortcut,
    /// Ask for the typing permission (opens the RemoteDesktop dialog).
    AllowTyping,
    /// Use this model (downloads it first if needed).
    SelectModel(String),
    /// Retry a failed download of this model.
    RetryDownload(String),
    SetMicrophone(String),
    SetPauseSecs(f32),
    SetIdleMinutes(u32),
    SetAutostart(bool),
    /// The user clicked Done on the setup page.
    SetupDone,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_roundtrip() {
        let msgs = vec![
            ToUi::State(UiState {
                typing: TypingState::Unavailable("old desktop".into()),
                models: vec![ModelRow {
                    id: "parakeet-v3".into(),
                    name: "Parakeet v3".into(),
                    summary: "Most accurate".into(),
                    size: "490 MB".into(),
                    installed: false,
                    active: true,
                }],
                ..Default::default()
            }),
            ToUi::Close,
        ];
        for m in msgs {
            let line = serde_json::to_string(&m).expect("serialize");
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<ToUi>(&line).expect("parse"), m);
        }
        for m in [
            FromUi::SelectModel("x".into()),
            FromUi::SetPauseSecs(0.8),
            FromUi::SetupDone,
        ] {
            let line = serde_json::to_string(&m).expect("serialize");
            assert_eq!(serde_json::from_str::<FromUi>(&line).expect("parse"), m);
        }
    }
}
