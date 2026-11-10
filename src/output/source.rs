//! Where the EIS socket comes from: the RemoteDesktop portal (production) or KWin's private
//! D-Bus API (tests in a nested KWin).

use std::{
    io,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    sync::Arc,
};

use ashpd::{
    desktop::{
        CreateSessionOptions, PersistMode, ResponseError, Session,
        remote_desktop::{
            ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
        },
    },
    zbus,
};
use futures_util::StreamExt;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use super::EisSource;

/// KWin's `connectToEIS` capability bit for a keyboard (same value as the portal's).
const KWIN_KEYBOARD: i32 = 1;
/// How long to wait for the subscription to the session's `Closed` signal before starting.
const SUBSCRIBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Why no EIS socket could be obtained.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceError {
    /// The user cancelled or denied the portal dialog.
    #[error("permission denied")]
    PermissionDenied,
    /// The portal or EIS support is missing.
    #[error("{0}")]
    Unavailable(String),
    /// Anything else (D-Bus errors, I/O). Worth retrying later.
    #[error("{0}")]
    Failed(String),
}

/// Keeps the session that owns the EIS socket alive.
pub(crate) enum SessionHandle {
    Portal {
        // Kept for the proxy's D-Bus connection, which the session lives on.
        _portal: RemoteDesktop,
        session: Arc<Session<RemoteDesktop>>,
        /// Fires when the portal emits the session's `Closed` signal.
        closed: Option<oneshot::Receiver<()>>,
        /// The task listening for `Closed`; stopped with the handle.
        _watcher: AbortOnDrop,
    },
    Kwin {
        bus: zbus::Connection,
        cookie: i32,
    },
    #[cfg(test)]
    Test {
        /// Fires when the test closes the "portal session".
        closed: Option<oneshot::Receiver<()>>,
    },
}

/// Aborts a spawned task when dropped.
pub(crate) struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl SessionHandle {
    /// Resolves when the compositor or the user closed the session (e.g. KDE's "stop remote
    /// control"). Never resolves for sessions without such a signal. Cancel-safe.
    pub(crate) async fn closed(&mut self) {
        match self {
            Self::Portal { closed, .. } => fired(closed).await,
            #[cfg(test)]
            Self::Test { closed } => fired(closed).await,
            Self::Kwin { .. } => std::future::pending().await,
        }
    }

    /// Ends the session. Errors are logged, not returned: there's nothing left to do.
    pub(crate) async fn close(self) {
        match self {
            Self::Portal { session, .. } => {
                if let Err(e) = session.close().await {
                    debug!("closing the RemoteDesktop session failed: {e}");
                }
            }
            #[cfg(test)]
            Self::Test { .. } => {}
            Self::Kwin { bus, cookie } => {
                let res = bus
                    .call_method(
                        Some("org.kde.KWin"),
                        "/org/kde/KWin/EIS/RemoteDesktop",
                        Some("org.kde.KWin.EIS.RemoteDesktop"),
                        "disconnect",
                        &(cookie,),
                    )
                    .await;
                if let Err(e) = res {
                    debug!("KWin EIS disconnect failed: {e}");
                }
            }
        }
    }
}

/// Resolves when `signal` fires; never if its sender is gone without firing. Cancel-safe.
async fn fired(signal: &mut Option<oneshot::Receiver<()>>) {
    let fired = match signal.as_mut() {
        Some(rx) => rx.await.is_ok(),
        None => false,
    };
    // A finished receiver must not be polled again.
    *signal = None;
    if !fired {
        std::future::pending::<()>().await;
    }
}

/// Watches `session` for the portal's `Closed` signal; returns once subscribed, so a close
/// right after this call isn't missed.
async fn watch_closed(
    session: &Arc<Session<RemoteDesktop>>,
) -> (oneshot::Receiver<()>, AbortOnDrop) {
    let (subscribed_tx, subscribed) = oneshot::channel();
    let (closed_tx, closed) = oneshot::channel();
    let session = Arc::clone(session);
    let task = tokio::spawn(async move {
        match session.receive_closed().await {
            Ok(mut signal) => {
                let _ = subscribed_tx.send(());
                // An item is the signal; the end of the stream means the bus connection is
                // gone, and the session with it.
                let _ = signal.next().await;
                let _ = closed_tx.send(());
            }
            Err(e) => {
                warn!("cannot watch the RemoteDesktop session for closing: {e}");
                let _ = subscribed_tx.send(());
                // Keep `closed_tx` alive: the EIS socket still tells us about a lost session.
                std::future::pending::<()>().await;
            }
        }
    });
    let watcher = AbortOnDrop(task.abort_handle());
    // Subscribing is one D-Bus round trip; don't let a stuck bus hold up the session.
    if tokio::time::timeout(SUBSCRIBE_TIMEOUT, subscribed)
        .await
        .is_err()
    {
        debug!("subscribing to the RemoteDesktop session's Closed signal is slow; going on");
    }
    (closed, watcher)
}

/// Opens a session and returns its EIS socket.
pub(crate) async fn open(source: &EisSource) -> Result<(OwnedFd, SessionHandle), SourceError> {
    match source {
        EisSource::Portal { token_path } => open_portal(token_path).await,
        EisSource::KwinDirect => open_kwin().await,
        #[cfg(test)]
        EisSource::Test(connections) => connections.open(),
    }
}

async fn open_portal(token_path: &Path) -> Result<(OwnedFd, SessionHandle), SourceError> {
    crate::portal::register_host_app().await;
    let token = read_token(token_path.to_owned()).await;
    debug!(
        restore_token = token.is_some(),
        "starting RemoteDesktop session"
    );

    let portal = RemoteDesktop::new().await.map_err(portal_error)?;
    if portal.version() < 2 {
        // ashpd also reports version 1 when it couldn't read the property at all.
        let version = read_portal_version().await?;
        if version < 2 {
            return Err(SourceError::Unavailable(format!(
                "RemoteDesktop portal version {version} has no ConnectToEIS (needs 2)"
            )));
        }
    }
    let session = Arc::new(
        portal
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(portal_error)?,
    );
    // Subscribe before starting, so a close right after the start isn't missed.
    let (closed, watcher) = watch_closed(&session).await;
    let opened = start_portal_session(&portal, &session, token.as_deref(), token_path).await;
    match opened {
        Ok(fd) => Ok((
            fd,
            SessionHandle::Portal {
                _portal: portal,
                session,
                closed: Some(closed),
                _watcher: watcher,
            },
        )),
        Err(e) => {
            // Don't leave a half-set-up session behind.
            session.close().await.ok();
            Err(e)
        }
    }
}

async fn start_portal_session(
    portal: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    token: Option<&str>,
    token_path: &Path,
) -> Result<OwnedFd, SourceError> {
    portal
        .select_devices(
            session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::KEYBOARD)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token),
        )
        .await
        .and_then(|r| r.response())
        .map_err(portal_error)?;
    // Shows the permission dialog unless the restore token is accepted.
    let started = portal
        .start(session, None, StartOptions::default())
        .await
        .and_then(|r| r.response())
        .map_err(portal_error);
    let started = match started {
        Ok(s) if s.devices().contains(DeviceType::KEYBOARD) => s,
        Ok(_) | Err(SourceError::PermissionDenied) => {
            // The token (if any) didn't grant typing; don't offer it again.
            if token.is_some() {
                remove_token(token_path.to_owned()).await;
            }
            return Err(SourceError::PermissionDenied);
        }
        Err(e) => return Err(e),
    };
    // Tokens are single-use: keep the newest one, or none if the portal returned none.
    let saved = match started.restore_token() {
        Some(new_token) => write_token(token_path.to_owned(), new_token.to_owned()).await,
        None => {
            debug!("the portal returned no restore token");
            remove_token(token_path.to_owned()).await;
            Ok(())
        }
    };
    if let Err(e) = saved {
        warn!("cannot save the RemoteDesktop restore token: {e}");
    }
    portal
        .connect_to_eis(session, ConnectToEISOptions::default())
        .await
        .map_err(portal_error)
}

fn portal_error(e: ashpd::Error) -> SourceError {
    match e {
        // KDE and GNOME report a denied dialog as "cancelled".
        ashpd::Error::Response(ResponseError::Cancelled) => SourceError::PermissionDenied,
        // Response code 2: xdg-desktop-portal also uses it when the backend call failed, so it
        // isn't the user's answer. (The restore token is spent either way: the portal deletes
        // it on `SelectDevices` and saves it again only when `Start` succeeds.)
        ashpd::Error::Response(ResponseError::Other) => SourceError::Unavailable(
            "the RemoteDesktop portal could not complete the request".into(),
        ),
        ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)) => SourceError::PermissionDenied,
        ashpd::Error::Portal(ashpd::PortalError::NotAllowed(msg)) => {
            SourceError::Unavailable(format!("RemoteDesktop portal not allowed: {msg}"))
        }
        ashpd::Error::PortalNotFound(_) => {
            SourceError::Unavailable("no RemoteDesktop portal on this desktop".into())
        }
        ashpd::Error::RequiresVersion(need, have) => SourceError::Unavailable(format!(
            "RemoteDesktop portal version {have} is too old (needs {need})"
        )),
        other => SourceError::Failed(format!("RemoteDesktop portal: {other}")),
    }
}

/// Reads the RemoteDesktop portal's `version` property directly, to tell "too old" apart from
/// "couldn't ask".
async fn read_portal_version() -> Result<u32, SourceError> {
    let failed = |e: zbus::Error| match &e {
        zbus::Error::MethodError(name, ..)
            if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
                || name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs"
                || name.as_str() == "org.freedesktop.DBus.Error.UnknownInterface" =>
        {
            SourceError::Unavailable(format!("no RemoteDesktop portal on this desktop ({e})"))
        }
        _ => SourceError::Failed(format!("cannot read the RemoteDesktop portal version: {e}")),
    };
    let bus = zbus::Connection::session().await.map_err(failed)?;
    let reply = bus
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.portal.RemoteDesktop", "version"),
        )
        .await
        .map_err(failed)?;
    let value: zbus::OwnedVariant = reply.body().deserialize().map_err(failed)?;
    u32::try_from(value).map_err(|e| {
        SourceError::Failed(format!(
            "the RemoteDesktop portal version is not a number: {e}"
        ))
    })
}

async fn read_token(path: PathBuf) -> Option<String> {
    let read = tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await;
    match read {
        Ok(Ok(s)) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => None,
        Ok(Err(e)) => {
            warn!("cannot read the RemoteDesktop restore token: {e}");
            None
        }
        Err(e) => {
            warn!("restore token read task failed: {e}");
            None
        }
    }
}

/// Deletes a used-up token, so it isn't sent again.
async fn remove_token(path: PathBuf) {
    let removed = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
    match removed {
        Ok(Ok(())) => {}
        Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => {}
        Ok(Err(e)) => warn!("cannot remove the RemoteDesktop restore token: {e}"),
        Err(e) => warn!("restore token removal task failed: {e}"),
    }
}

/// Writes the token atomically, readable only by the user.
async fn write_token(path: PathBuf, token: String) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        io::Write::write_all(&mut file, token.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    })
    .await
    .map_err(io::Error::other)?
}

/// KWin's private EIS entry point on the current session bus. Test-only: there is no dialog,
/// so this must only ever run against a nested KWin with its own bus.
async fn open_kwin() -> Result<(OwnedFd, SessionHandle), SourceError> {
    let bus = zbus::Connection::session()
        .await
        .map_err(|e| SourceError::Unavailable(format!("session bus: {e}")))?;
    let reply = bus
        .call_method(
            Some("org.kde.KWin"),
            "/org/kde/KWin/EIS/RemoteDesktop",
            Some("org.kde.KWin.EIS.RemoteDesktop"),
            "connectToEIS",
            &(KWIN_KEYBOARD,),
        )
        .await
        .map_err(|e| SourceError::Unavailable(format!("KWin connectToEIS: {e}")))?;
    let (fd, cookie): (zbus::OwnedFd, i32) = reply
        .body()
        .deserialize()
        .map_err(|e| SourceError::Failed(format!("KWin connectToEIS reply: {e}")))?;
    Ok((fd.into(), SessionHandle::Kwin { bus, cookie }))
}
