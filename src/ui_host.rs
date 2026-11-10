//! Daemon side of the setup/settings windows: spawns `murmur --ui <window>`
//! and talks to it over JSON lines (see `ui::protocol`).

use std::process::Stdio;

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
};
use tracing::{debug, info, warn};

use crate::ui::protocol::{FromUi, ToUi, UiState, Window};

#[derive(Debug)]
pub enum UiEvent {
    Request(FromUi),
    /// The window was closed (or the UI process exited).
    Closed,
}

pub struct UiHost {
    pub window: Window,
    to_ui: mpsc::UnboundedSender<ToUi>,
    _child: Child,
}

impl UiHost {
    /// Starts the UI process showing `window` with `state`.
    pub fn open(
        window: Window,
        state: UiState,
        events: mpsc::UnboundedSender<UiEvent>,
    ) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .args(["--ui", window.arg()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(std::io::Error::other("UI process has no pipes"));
        };
        let (to_ui, mut rx) = mpsc::unbounded_channel::<ToUi>();
        let _ = to_ui.send(ToUi::State(state));
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let Ok(mut line) = serde_json::to_string(&msg) else {
                    continue;
                };
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<FromUi>(&line) {
                    Ok(req) => {
                        if events.send(UiEvent::Request(req)).is_err() {
                            break;
                        }
                    }
                    Err(e) => debug!(error = %e, "ignoring bad line from the UI"),
                }
            }
            let _ = events.send(UiEvent::Closed);
        });
        info!(window = window.arg(), "window opened");
        Ok(Self {
            window,
            to_ui,
            _child: child,
        })
    }

    pub fn send(&self, state: UiState) {
        if self.to_ui.send(ToUi::State(state)).is_err() {
            warn!("UI process is gone");
        }
    }

    pub fn close(&self) {
        let _ = self.to_ui.send(ToUi::Close);
    }
}
