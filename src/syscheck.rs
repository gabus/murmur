//! Probes what the host provides (D-019): the facts behind Settings → Your system and
//! `murmur --check` (D-020). Read-only: no dialogs, no permission requests.

use std::{future::Future, path::Path, time::Duration};

use ashpd::zbus;
use tracing::debug;

use crate::{
    audio::{self, Mic},
    portal,
    ui::protocol::SystemReport,
};

const TRAY_WATCHER: &str = "org.kde.StatusNotifierWatcher";
/// A portal or audio server that hangs must not keep the checklist empty.
const TIMEOUT: Duration = Duration::from_secs(3);
/// `AvailableDeviceTypes` bit for a keyboard (RemoteDesktop portal).
const KEYBOARD: u32 = 1;

/// The environment part of the report (pure, for tests).
fn from_env(
    wayland_display: Option<&str>,
    runtime_dir: Option<&str>,
    current_desktop: Option<&str>,
    kde_session_version: Option<&str>,
    exists: impl Fn(&Path) -> bool,
) -> SystemReport {
    let wayland = match wayland_display.filter(|d| !d.is_empty()) {
        Some(d) if Path::new(d).is_absolute() => exists(Path::new(d)),
        Some(d) => runtime_dir.is_some_and(|r| exists(&Path::new(r).join(d))),
        None => false,
    };
    SystemReport {
        wayland,
        desktop: current_desktop.unwrap_or_default().trim().to_owned(),
        kde_version: kde_session_version.and_then(|v| v.trim().parse().ok()),
        ..Default::default()
    }
}

async fn timed<T>(what: &str, f: impl Future<Output = zbus::Result<T>>) -> Option<T> {
    match tokio::time::timeout(TIMEOUT, f).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            debug!(what, error = %e, "system check");
            None
        }
        Err(_) => {
            debug!(what, "system check timed out");
            None
        }
    }
}

/// A `u32` property of a portal interface; None when the portal doesn't offer it.
async fn portal_u32(conn: &zbus::Connection, interface: &str, name: &str) -> Option<u32> {
    let value = timed(interface, portal::property(conn, interface, name)).await?;
    u32::try_from(value).ok()
}

async fn portal_answers(conn: &zbus::Connection) -> bool {
    timed("portal ping", async {
        let peer = zbus::fdo::PeerProxy::builder(conn)
            .destination(portal::BUS_NAME)
            .path(portal::PATH)
            .build()
            .await?;
        peer.ping().await
    })
    .await
    .is_some()
}

async fn tray_running(conn: &zbus::Connection) -> bool {
    timed("tray watcher", async {
        let dbus = zbus::fdo::DBusProxy::new(conn).await?;
        let name = zbus::names::BusName::from_static_str(TRAY_WATCHER)?;
        Ok(dbus.name_has_owner(name).await?)
    })
    .await
    .unwrap_or(false)
}

/// Probes the host. Also returns the microphones it found, so the caller doesn't list them
/// a second time (None when no audio server answered). Takes as long as the slowest answer:
/// milliseconds normally, at most about [`TIMEOUT`].
pub async fn probe_with_microphones() -> (SystemReport, Option<Vec<Mic>>) {
    let env = |k: &str| std::env::var(k).ok();
    let mut report = from_env(
        env("WAYLAND_DISPLAY").as_deref(),
        env("XDG_RUNTIME_DIR").as_deref(),
        env("XDG_CURRENT_DESKTOP").as_deref(),
        env("KDE_SESSION_VERSION").as_deref(),
        Path::exists,
    );
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    // Connecting to the audio server blocks (and can take seconds when there is none).
    let audio = tokio::task::spawn_blocking(audio::probe_server);
    if let Some(conn) = timed("session bus", zbus::Connection::session()).await {
        const RD: &str = "org.freedesktop.portal.RemoteDesktop";
        let (answers, shortcuts, remote, devices, clipboard, notification, background, tray) = tokio::join!(
            portal_answers(&conn),
            portal_u32(&conn, "org.freedesktop.portal.GlobalShortcuts", "version"),
            portal_u32(&conn, RD, "version"),
            portal_u32(&conn, RD, "AvailableDeviceTypes"),
            portal_u32(&conn, "org.freedesktop.portal.Clipboard", "version"),
            portal_u32(&conn, "org.freedesktop.portal.Notification", "version"),
            portal_u32(&conn, "org.freedesktop.portal.Background", "version"),
            tray_running(&conn),
        );
        report.portal = answers;
        report.global_shortcuts = shortcuts;
        report.remote_desktop = remote;
        report.remote_desktop_keyboard = devices.is_some_and(|d| d & KEYBOARD != 0);
        report.clipboard = clipboard;
        report.notification = notification;
        report.background = background;
        report.tray = tray;
    }
    // A hung audio server counts as none; its thread is left to finish on its own.
    let mics = match tokio::time::timeout_at(deadline, audio).await {
        Ok(Ok(mics)) => mics,
        Ok(Err(e)) => {
            debug!(error = %e, "audio check failed");
            None
        }
        Err(_) => {
            debug!("audio check timed out");
            None
        }
    };
    report.audio_server = mics.is_some();
    report.microphones = mics.as_ref().map_or(0, Vec::len);
    (report, mics)
}

/// [`probe_with_microphones`] without the list.
pub async fn probe() -> SystemReport {
    probe_with_microphones().await.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_needs_an_existing_socket() {
        let only = |p: &'static str| move |q: &Path| q == Path::new(p);
        let run = Some("/run/user/1000");
        let r = from_env(
            Some("wayland-0"),
            run,
            None,
            None,
            only("/run/user/1000/wayland-0"),
        );
        assert!(r.wayland);
        let r = from_env(Some("/tmp/wl"), None, None, None, only("/tmp/wl"));
        assert!(r.wayland, "absolute WAYLAND_DISPLAY");
        let r = from_env(
            Some("wayland-1"),
            run,
            None,
            None,
            only("/run/user/1000/wayland-0"),
        );
        assert!(!r.wayland, "socket missing");
        assert!(!from_env(None, run, None, None, |_| true).wayland);
        assert!(!from_env(Some(""), run, None, None, |_| true).wayland);
        assert!(
            !from_env(Some("wayland-0"), None, None, None, |_| true).wayland,
            "no runtime dir"
        );
    }

    #[test]
    fn desktop_and_plasma_version() {
        let r = from_env(None, None, Some("KDE"), Some("6"), |_| false);
        assert_eq!((r.desktop.as_str(), r.kde_version), ("KDE", Some(6)));
        let r = from_env(None, None, Some(" ubuntu:GNOME "), Some("x"), |_| false);
        assert_eq!((r.desktop.as_str(), r.kde_version), ("ubuntu:GNOME", None));
        assert_eq!(from_env(None, None, None, None, |_| false).desktop, "");
    }
}
