//! Typing text into the focused app through the RemoteDesktop portal's EIS channel
//! (libei, [D-005](../../docs/decisions.md)).
//!
//! [`Output`] owns one long-lived session and EIS connection in a background task. Each
//! queued string is typed as one burst of key events resolved through the compositor's real
//! XKB keymap and active layout ([`keymap`]), with murmur pressing Shift/AltGr itself. Before
//! each burst a `sync` round trip makes sure the known layout and modifier state are current;
//! while the user physically holds Ctrl, Alt or Super, text waits until they are released.

mod eis;
pub mod keymap;
mod reader;
mod source;
mod task;

use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};

/// Where the EIS connection comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EisSource {
    /// Production: xdg RemoteDesktop portal, keyboard only, `persist_mode = ExplicitlyRevoked`,
    /// restore token loaded from and saved to `token_path`. Calls `ashpd::register_host_app`
    /// first when not sandboxed (failure is logged at warn and ignored).
    Portal {
        /// Restore token file, e.g. `paths::state_dir()/remote-desktop.token`.
        token_path: PathBuf,
    },
    /// Tests: KWin's private D-Bus API `org.kde.KWin /org/kde/KWin/EIS/RemoteDesktop
    /// connectToEIS(i capabilities) -> (h fd, i cookie)` on the *current* session bus.
    /// There is no permission dialog, so only ever use this inside a nested
    /// `kwin_wayland --virtual` with its own private bus.
    KwinDirect,
}

/// What happened to the output, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputEvent {
    /// The keyboard device is resumed and its keymap is known: typing works.
    Ready,
    /// The portal dialog was cancelled or denied. Queued text was dropped; the next
    /// [`Output::type_text`] tries again (which may show the dialog again).
    PermissionDenied,
    /// The session or EIS connection was lost (or couldn't be set up). Text queued from now on
    /// triggers a reconnect, using the restore token, so no dialog.
    Disconnected(String),
    /// No RemoteDesktop portal or no EIS keyboard support (e.g. an old desktop). Queued text
    /// was dropped.
    Unavailable(String),
    /// Characters skipped because the active layout can't produce them. Carries no text.
    Unmapped {
        /// Number of skipped characters.
        count: usize,
    },
}

/// Handle to the typing task. Cheap to clone; the task ends when every handle is dropped,
/// after typing what was already queued.
#[derive(Debug, Clone)]
pub struct Output {
    tx: mpsc::UnboundedSender<task::Cmd>,
}

impl Output {
    /// Spawns the background task on the current tokio runtime (panics outside one, like
    /// `tokio::spawn`). Connects eagerly, so the first word isn't delayed by session setup,
    /// and reports progress and problems on `events`.
    pub fn spawn(source: EisSource, events: mpsc::UnboundedSender<OutputEvent>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(task::run(source, events, rx));
        Self { tx }
    }

    /// Queues text to type, usually one word plus its trailing space (D-002). Queued strings
    /// are typed in order, each as one burst.
    pub fn type_text(&self, text: String) {
        if text.is_empty() {
            return;
        }
        // Fails only if the task is gone, and then there's nobody to type.
        let _ = self.tx.send(task::Cmd::Type(text));
    }

    /// Resolves once everything queued so far has been typed (or dropped on error).
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(task::Cmd::Flush(done)).is_ok() {
            // An error means the task dropped the request while shutting down: also done.
            let _ = wait.await;
        }
    }
}
