//! Daemon side of the setup/settings windows: spawns `murmur --ui <window>`
//! and talks to it over JSON lines (see `ui::protocol`).

use std::{process::Stdio, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot, watch},
};
use tracing::{debug, info};

use crate::ui::protocol::{FromUi, ToUi, UiState, Window};

/// Each opened window gets a new generation, so a late `Closed` from an old
/// window can't be mistaken for the current one closing.
pub type Generation = u64;

#[derive(Debug)]
pub enum UiEvent {
    Request(FromUi),
    /// The UI process of this generation exited.
    Closed(Generation),
}

pub struct UiHost {
    pub window: Window,
    pub generation: Generation,
    state: watch::Sender<UiState>,
    close: Option<oneshot::Sender<()>>,
    kill: Option<oneshot::Sender<()>>,
    exited: watch::Receiver<bool>,
}

fn self_exe() -> std::io::Result<std::path::PathBuf> {
    // /proc/self/exe still works after the binary was replaced on disk
    // (current_exe() would then end in " (deleted)").
    let proc_exe = std::path::PathBuf::from("/proc/self/exe");
    if proc_exe.exists() {
        Ok(proc_exe)
    } else {
        std::env::current_exe()
    }
}

impl UiHost {
    /// Starts the UI process showing `window` with `state`.
    pub fn open(
        window: Window,
        generation: Generation,
        state: UiState,
        events: mpsc::UnboundedSender<UiEvent>,
    ) -> std::io::Result<Self> {
        let mut child = Command::new(self_exe()?)
            .args(["--ui", window.arg()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(std::io::Error::other("UI process has no pipes"));
        };
        // Only the latest state matters: a watch coalesces bursts of updates.
        let (state_tx, mut state_rx) = watch::channel(state);
        let (close_tx, mut close_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut write = async |msg: &ToUi| -> bool {
                let Ok(mut line) = serde_json::to_string(msg) else {
                    return true;
                };
                line.push('\n');
                stdin.write_all(line.as_bytes()).await.is_ok() && stdin.flush().await.is_ok()
            };
            let first = ToUi::State(state_rx.borrow_and_update().clone());
            if !write(&first).await {
                return;
            }
            loop {
                tokio::select! {
                    changed = state_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let msg = ToUi::State(state_rx.borrow_and_update().clone());
                        if !write(&msg).await {
                            break;
                        }
                    }
                    _ = &mut close_rx => {
                        let _ = write(&ToUi::Close).await;
                        break;
                    }
                }
            }
        });
        let requests = events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<FromUi>(&line) {
                    Ok(req) => {
                        if requests.send(UiEvent::Request(req)).is_err() {
                            break;
                        }
                    }
                    Err(e) => debug!(error = %e, "ignoring bad line from the UI"),
                }
            }
        });
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let (exited_tx, exited) = watch::channel(false);
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = kill_rx => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
            let _ = exited_tx.send(true);
            let _ = events.send(UiEvent::Closed(generation));
        });
        info!(window = window.arg(), "window opened");
        Ok(Self {
            window,
            generation,
            state: state_tx,
            close: Some(close_tx),
            kill: Some(kill_tx),
            exited,
        })
    }

    pub fn send(&self, state: UiState) {
        self.state.send_replace(state);
    }

    /// Asks the window to close (it exits on its own).
    pub fn close(&mut self) {
        if let Some(c) = self.close.take() {
            let _ = c.send(());
        }
    }

    /// Closes the window and waits briefly for it to exit (then kills it).
    pub async fn shutdown(mut self) {
        self.close();
        let mut exited = self.exited.clone();
        let _ = tokio::time::timeout(Duration::from_secs(1), exited.wait_for(|e| *e)).await;
    }
}

impl Drop for UiHost {
    fn drop(&mut self) {
        if let Some(k) = self.kill.take() {
            let _ = k.send(());
        }
    }
}
