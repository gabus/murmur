//! Global toggle shortcut via the GlobalShortcuts portal.

use std::time::{Duration, Instant};

use ashpd::desktop::{
    CreateSessionOptions, Session,
    global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut},
};
use futures_util::StreamExt;
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};
use tracing::{debug, info, warn};

const SHORTCUT_ID: &str = "toggle";
/// XDG shortcuts spec syntax; the user can change it in the desktop's settings.
const PREFERRED_TRIGGER: &str = "LOGO+h";
/// Ignore repeated activations closer than this (key bounce / auto-repeat).
const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum HotkeyEvent {
    Toggle,
    /// The portal session ended; the shortcut no longer works.
    Lost(String),
}

#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    #[error("global shortcuts portal unavailable: {0}")]
    Portal(#[from] ashpd::Error),
}

pub struct Hotkey {
    task: JoinHandle<()>,
    trigger: Option<String>,
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Hotkey {
    /// The key combination the user bound, as the desktop describes it (e.g. "Meta+H").
    pub fn trigger(&self) -> Option<&str> {
        self.trigger.as_deref()
    }

    /// Binds the toggle shortcut (the desktop asks the user once) and forwards activations.
    pub async fn start(events: UnboundedSender<HotkeyEvent>) -> Result<Self, HotkeyError> {
        let portal = GlobalShortcuts::new().await?;
        let session = portal
            .create_session(CreateSessionOptions::default())
            .await?;
        let shortcut = NewShortcut::new(SHORTCUT_ID, "Start or stop dictation")
            .preferred_trigger(PREFERRED_TRIGGER);
        let bound = portal
            .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
            .await?
            .response()?;
        let mut trigger = None;
        for s in bound.shortcuts() {
            info!(
                id = s.id(),
                trigger = s.trigger_description(),
                "shortcut bound"
            );
            if s.id() == SHORTCUT_ID && !s.trigger_description().is_empty() {
                trigger = Some(s.trigger_description().to_owned());
            }
        }
        let mut activated = portal.receive_activated().await?;
        let task = tokio::spawn(async move {
            // The session must stay alive as long as we listen.
            let session: Session<GlobalShortcuts> = session;
            let mut closed = match session.receive_closed().await {
                Ok(c) => c,
                Err(e) => {
                    let _ = events.send(HotkeyEvent::Lost(e.to_string()));
                    return;
                }
            };
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
                    c = closed.next() => {
                        warn!(details = ?c, "shortcut session closed");
                        let _ = events.send(HotkeyEvent::Lost("session closed".into()));
                        break;
                    }
                }
            }
        });
        Ok(Self { task, trigger })
    }
}
