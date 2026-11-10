//! Typing text into the focused app through the RemoteDesktop portal's EIS channel
//! (libei, [D-005](../../docs/decisions.md)).
//!
//! [`Output`] owns one long-lived session and EIS connection in a background task. Each
//! queued string is typed as one burst of key events resolved through the compositor's real
//! XKB keymap and active layout ([`keymap`]), with murmur pressing Shift/AltGr itself. Before
//! each burst a `sync` round trip makes sure the known layout and modifier state are current;
//! while the user holds, latches or locks a modifier the plan can't account for (anything but
//! Caps Lock and Num Lock, D-006), text waits until it is released.
//!
//! A string with characters the active layout can't produce is pasted instead, when the
//! portal granted clipboard access (D-011): murmur offers it as the clipboard selection,
//! presses the paste shortcut and then restores the previous selection. Without clipboard
//! access those characters are skipped and reported as [`OutputEvent::Unmapped`].

mod clipboard;
mod eis;
pub mod keymap;
mod paste;
mod reader;
mod source;
mod task;
#[cfg(test)]
pub mod test_eis;

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
    /// Unit tests: connections handed out by a fake EIS server.
    #[cfg(test)]
    Test(test_eis::Connections),
}

/// What happened to the output, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputEvent {
    /// The keyboard device is resumed and its keymap is known: typing works.
    Ready,
    /// The portal dialog was cancelled or denied. Queued text was dropped, and so is text queued
    /// later, until [`Output::retry`] (which may show the dialog again).
    PermissionDenied,
    /// The session or EIS connection was lost, the portal closed the session, or the
    /// connection couldn't be set up for a reason worth retrying. Text queued from now on
    /// triggers a reconnect, using the restore token, so no dialog.
    Disconnected(String),
    /// No RemoteDesktop portal, no EIS keyboard support (e.g. an old desktop) or the portal
    /// failed. Queued text was dropped, and so is text queued later, until [`Output::retry`].
    Unavailable(String),
    /// `true`: text has been waiting for more than about 2 s, because the user holds (or has
    /// latched or locked) a modifier, or the compositor paused or removed the keyboard. `false`:
    /// text flows again (or the waiting text was dropped). Sent only after a `true`.
    Waiting(bool),
    /// Characters skipped because the active layout can't produce them and pasting them wasn't
    /// possible (no clipboard access, the paste wasn't picked up, or the previous clipboard
    /// couldn't be saved for restoring). Carries no text.
    Unmapped {
        /// Number of skipped characters.
        count: usize,
    },
}

/// The shortcut murmur presses to paste ([`Output::set_paste_keys`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PasteKeys {
    /// Ctrl+V, what most apps use.
    #[default]
    CtrlV,
    /// Ctrl+Shift+V, what terminals use.
    CtrlShiftV,
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
        Self::start(source, events, true)
    }

    /// Like [`Output::spawn`], but doesn't connect until [`Output::retry`] is called or text
    /// arrives, so the permission dialog appears only when asked for (first-run setup, D-009).
    pub fn spawn_idle(source: EisSource, events: mpsc::UnboundedSender<OutputEvent>) -> Self {
        Self::start(source, events, false)
    }

    fn start(source: EisSource, events: mpsc::UnboundedSender<OutputEvent>, eager: bool) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(task::run(source, events, rx, eager));
        Self { tx }
    }

    /// Sets the shortcut used to paste characters the layout can't type (default Ctrl+V;
    /// terminals want Ctrl+Shift+V). Applies to every paste after the task receives it.
    pub fn set_paste_keys(&self, keys: PasteKeys) {
        // Fails only if the task is gone, and then there's nobody to type.
        let _ = self.tx.send(task::Cmd::PasteKeys(keys));
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

    /// Leaves the failed state that [`OutputEvent::PermissionDenied`] and
    /// [`OutputEvent::Unavailable`] put the output in, and connects right away if there is no
    /// connection (which may show the portal dialog). Call it when the user starts dictating.
    /// Without a failure it only connects if disconnected, so it is cheap to call every time.
    pub fn retry(&self) {
        // Fails only if the task is gone, and then there's nobody to type.
        let _ = self.tx.send(task::Cmd::Retry);
    }

    /// Resolves once everything queued so far has been typed (or dropped on error). This can
    /// take arbitrarily long while text waits for a held modifier ([`OutputEvent::Waiting`]),
    /// a paused keyboard or the portal dialog. Cancel-safe: dropping the future (e.g. on a
    /// timeout) leaves the queued text alone.
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(task::Cmd::Flush(done)).is_ok() {
            // An error means the task dropped the request while shutting down: also done.
            let _ = wait.await;
        }
    }
}
