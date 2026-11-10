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
/// The suggested shortcut, in the XDG shortcuts spec syntax (D-016): Super+H, like Windows'
/// dictation key. The user can change it in the dialog or the desktop's settings.
const PREFERRED_TRIGGER: &str = "LOGO+h";
/// On GNOME, Super+H is "Hide window": the shortcut dialog accepts it, but it never fires.
const PREFERRED_TRIGGER_GNOME: &str = "LOGO+ALT+h";

/// The trigger to suggest (D-016).
fn preferred_trigger(gnome: bool) -> &'static str {
    if gnome {
        PREFERRED_TRIGGER_GNOME
    } else {
        PREFERRED_TRIGGER
    }
}
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
    /// The desktop couldn't open its shortcut dialog.
    ConfigureFailed(String),
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

    /// The task ended (binding failed or the session was lost); spawn a new
    /// `Hotkey` to bind again.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

fn trigger_of(shortcuts: &[Shortcut]) -> Option<String> {
    shortcuts
        .iter()
        .find(|s| s.id() == SHORTCUT_ID)
        .map(|s| readable_trigger(s.trigger_description()))
        .filter(|t| !t.is_empty())
}

/// GNOME describes a trigger as a sentence with GTK accelerators (`Press <Alt><Super>h`, or
/// `Press %s or %s` with two). Returns just the keys, the way GNOME Settings shows them
/// (`Alt+Super+H`); other descriptions (KDE: `Meta+H`) are returned as they are.
fn readable_trigger(description: &str) -> String {
    let keys: Vec<String> = description
        .split_whitespace()
        .filter_map(gtk_accelerator_label)
        .collect();
    if keys.is_empty() {
        description.to_owned()
    } else {
        keys.join(" / ")
    }
}

/// `<Alt><Super>h` → `Alt+Super+H`. `None` unless `word` is a GTK accelerator with at least one
/// modifier.
fn gtk_accelerator_label(word: &str) -> Option<String> {
    let mut rest = word.strip_prefix('<')?;
    let mut parts = Vec::new();
    loop {
        let (modifier, after) = rest.split_once('>')?;
        if modifier.is_empty() || !modifier.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        parts.push(match modifier.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" => "Ctrl".to_owned(),
            "super" => "Super".to_owned(),
            "alt" => "Alt".to_owned(),
            "shift" => "Shift".to_owned(),
            _ => modifier.to_owned(),
        });
        match after.strip_prefix('<') {
            Some(next) => rest = next,
            None => {
                rest = after;
                break;
            }
        }
    }
    let mut key = rest.chars();
    let first = key.next()?;
    if rest.contains(['<', '>']) {
        return None;
    }
    parts.push(first.to_uppercase().chain(key).collect());
    Some(parts.join("+"))
}

async fn run(
    events: mpsc::UnboundedSender<HotkeyEvent>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
) -> Result<(), ashpd::Error> {
    crate::portal::register_host_app().await;
    let portal = std::sync::Arc::new(GlobalShortcuts::new().await?);
    let can_configure = portal.version() >= 2;
    let session = std::sync::Arc::new(
        portal
            .create_session(CreateSessionOptions::default())
            .await?,
    );
    let shortcut = NewShortcut::new(SHORTCUT_ID, "Start or stop dictation")
        .preferred_trigger(preferred_trigger(crate::portal::on_gnome()));
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
                    // The backend may reply only when its dialog closes: don't
                    // block activations meanwhile.
                    let (p, s, ev) = (portal.clone(), session.clone(), events.clone());
                    tokio::spawn(async move {
                        if let Err(e) = p
                            .configure_shortcuts(&s, None, ConfigureShortcutsOptions::default())
                            .await
                        {
                            warn!(error = %e, "cannot open the shortcut settings");
                            let _ = ev.send(HotkeyEvent::ConfigureFailed(e.to_string()));
                        }
                    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_super_alt_h_on_gnome_only() {
        // D-016: Super+H is GNOME's "Hide window".
        assert_eq!(preferred_trigger(true), "LOGO+ALT+h");
        assert_eq!(preferred_trigger(false), "LOGO+h");
    }

    #[test]
    fn gnome_trigger_descriptions_become_keys() {
        // xdg-desktop-portal-gnome 50: "Press %s" / "Press %s or %s" with GTK accelerators.
        assert_eq!(readable_trigger("Press <Alt><Super>h"), "Alt+Super+H");
        assert_eq!(readable_trigger("Press <Super>h"), "Super+H");
        assert_eq!(
            readable_trigger("Press <Control><Shift>space or <Primary>F9"),
            "Ctrl+Shift+Space / Ctrl+F9"
        );
        // A translated sentence keeps working: only the accelerator is used.
        assert_eq!(readable_trigger("Drücken Sie <Super>d"), "Super+D");
    }

    #[test]
    fn other_trigger_descriptions_stay() {
        for kept in ["Meta+H", "Press F9", "", "<>", "Press <Super>", "a <b c"] {
            assert_eq!(readable_trigger(kept), kept);
        }
    }
}
