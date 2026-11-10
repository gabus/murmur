//! Global toggle shortcut via the GlobalShortcuts portal.
//!
//! Runs as a background task: binding may show a desktop dialog (first run),
//! which must not block the rest of murmur (the setup window, the tray).

use std::time::{Duration, Instant};

use ashpd::desktop::{
    CreateSessionOptions,
    global_shortcuts::{
        BindShortcutsOptions, ConfigureShortcutsOptions, GlobalShortcuts, NewShortcut, Shortcut,
    },
};
use futures_util::StreamExt;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info, warn};

const SHORTCUT_ID: &str = "toggle";
/// XDG shortcuts spec syntax; the user can change it in the desktop's settings.
const PREFERRED_TRIGGER: &str = "LOGO+h";
/// Ignore repeated activations closer than this (key bounce / auto-repeat).
const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq)]
pub enum HotkeyEvent {
    Toggle,
    /// Bound (or rebound): the trigger as the desktop describes it ("Meta+H"),
    /// `None` if the user left it unassigned. `can_configure`: the desktop can
    /// open its own dialog to change it.
    Bound {
        trigger: Option<String>,
        can_configure: bool,
    },
    /// No shortcut possible (no portal, or it failed).
    Failed(String),
    /// The portal session ended; the shortcut no longer works.
    Lost(String),
}

enum Cmd {
    Configure,
}

/// Handle to the shortcut task. Dropping it removes the shortcut session.
pub struct Hotkey {
    cmd: mpsc::UnboundedSender<Cmd>,
    task: JoinHandle<()>,
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Hotkey {
    /// Starts binding in the background and reports via `events`.
    pub fn spawn(events: mpsc::UnboundedSender<HotkeyEvent>) -> Self {
        let (cmd, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            if let Err(e) = run(events.clone(), rx).await {
                warn!(error = %e, "global shortcut unavailable");
                let _ = events.send(HotkeyEvent::Failed(e.to_string()));
            }
        });
        Self { cmd, task }
    }

    /// Opens the desktop's dialog for changing the shortcut (portal v2).
    pub fn configure(&self) {
        let _ = self.cmd.send(Cmd::Configure);
    }
}

fn trigger_of(shortcuts: &[Shortcut]) -> Option<String> {
    shortcuts
        .iter()
        .find(|s| s.id() == SHORTCUT_ID)
        .map(|s| s.trigger_description().to_owned())
        .filter(|t| !t.is_empty())
}

async fn run(
    events: mpsc::UnboundedSender<HotkeyEvent>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
) -> Result<(), ashpd::Error> {
    crate::portal::register_host_app().await;
    let portal = GlobalShortcuts::new().await?;
    let can_configure = portal.version() >= 2;
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await?;
    let shortcut = NewShortcut::new(SHORTCUT_ID, "Start or stop dictation")
        .preferred_trigger(PREFERRED_TRIGGER);
    let bound = portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?
        .response()?;
    let trigger = trigger_of(bound.shortcuts());
    info!(trigger = ?trigger, "shortcut bound");
    let _ = events.send(HotkeyEvent::Bound {
        trigger,
        can_configure,
    });

    let mut activated = portal.receive_activated().await?;
    let mut changed = portal.receive_shortcuts_changed().await?;
    let mut closed = session.receive_closed().await?;
    let mut last: Option<Instant> = None;
    loop {
        tokio::select! {
            a = activated.next() => {
                let Some(a) = a else {
                    let _ = events.send(HotkeyEvent::Lost("signal stream ended".into()));
                    break;
                };
                if a.shortcut_id() != SHORTCUT_ID {
                    continue;
                }
                let now = Instant::now();
                if last.is_some_and(|t| now.duration_since(t) < DEBOUNCE) {
                    debug!("debounced shortcut activation");
                    continue;
                }
                last = Some(now);
                if events.send(HotkeyEvent::Toggle).is_err() {
                    break;
                }
            }
            Some(c) = changed.next() => {
                let trigger = trigger_of(c.shortcuts());
                info!(trigger = ?trigger, "shortcut changed");
                let _ = events.send(HotkeyEvent::Bound { trigger, can_configure });
            }
            Some(cmd) = cmds.recv() => match cmd {
                Cmd::Configure => {
                    if let Err(e) = portal
                        .configure_shortcuts(&session, None, ConfigureShortcutsOptions::default())
                        .await
                    {
                        warn!(error = %e, "cannot open the shortcut settings");
                    }
                }
            },
            c = closed.next() => {
                warn!(details = ?c, "shortcut session closed");
                let _ = events.send(HotkeyEvent::Lost("session closed".into()));
                break;
            }
        }
    }
    Ok(())
}
