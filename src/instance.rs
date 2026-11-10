//! Single instance + a tiny D-Bus control interface.
//!
//! The first murmur owns `io.github.gabus.Murmur` on the session bus (Flatpak
//! apps may own their app-id name). Starting murmur again calls `Activate` on
//! the running one, which opens Settings, and exits. `Toggle` is exposed too,
//! so desktops without a GlobalShortcuts portal can bind a command to it.

use ashpd::zbus::{self, fdo::RequestNameFlags, fdo::RequestNameReply};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{app::Command, paths::APP_ID};

const OBJECT_PATH: &str = "/io/github/gabus/Murmur";

struct Remote {
    commands: mpsc::UnboundedSender<Command>,
}

#[zbus::interface(name = "io.github.gabus.Murmur")]
impl Remote {
    /// Opens the settings window (or the setup page if setup isn't done).
    fn activate(&self) {
        let _ = self.commands.send(Command::OpenSettings);
    }

    /// Starts or stops dictation, like the shortcut.
    fn toggle(&self) {
        let _ = self.commands.send(Command::Toggle);
    }
}

pub enum Instance {
    /// We are the only murmur; keep the connection alive for the lifetime.
    Primary(zbus::Connection),
    /// Another murmur was running and has been asked to show itself.
    AlreadyRunning,
}

#[derive(Debug, thiserror::Error)]
#[error("session bus: {0}")]
pub struct InstanceError(#[from] zbus::Error);

/// Claims the app-id name, or activates the running instance.
pub async fn acquire(commands: mpsc::UnboundedSender<Command>) -> Result<Instance, InstanceError> {
    let conn = zbus::Connection::session().await?;
    conn.object_server()
        .at(OBJECT_PATH, Remote { commands })
        .await?;
    // zbus reports a taken name (with DO_NOT_QUEUE) as `Error::NameTaken`.
    let taken = match conn
        .request_name_with_flags(APP_ID, RequestNameFlags::DO_NOT_QUEUE)
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => false,
        Ok(_) | Err(zbus::Error::NameTaken) => true,
        Err(e) => return Err(e.into()),
    };
    match taken {
        false => Ok(Instance::Primary(conn)),
        true => {
            info!("murmur is already running; opening its settings");
            let call = conn.call_method(Some(APP_ID), OBJECT_PATH, Some(APP_ID), "Activate", &());
            // Never become a second daemon, even if the running one doesn't answer.
            match tokio::time::timeout(std::time::Duration::from_secs(3), call).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => warn!(error = %e, "the running murmur didn't respond"),
                Err(_) => warn!("the running murmur didn't respond in time"),
            }
            Ok(Instance::AlreadyRunning)
        }
    }
}
