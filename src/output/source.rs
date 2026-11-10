//! Where the EIS socket comes from: the RemoteDesktop portal (production) or KWin's private
//! D-Bus API (tests in a nested KWin).

use std::{
    io,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
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
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};

use super::EisSource;

/// KWin's `connectToEIS` capability bit for a keyboard (same value as the portal's).
const KWIN_KEYBOARD: i32 = 1;

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
        session: Session<RemoteDesktop>,
    },
    Kwin {
        bus: zbus::Connection,
        cookie: i32,
    },
}

impl SessionHandle {
    /// Ends the session. Errors are logged, not returned: there's nothing left to do.
    pub(crate) async fn close(self) {
        match self {
            Self::Portal { session, .. } => {
                if let Err(e) = session.close().await {
                    debug!("closing the RemoteDesktop session failed: {e}");
                }
            }
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

/// Opens a session and returns its EIS socket.
pub(crate) async fn open(source: &EisSource) -> Result<(OwnedFd, SessionHandle), SourceError> {
    match source {
        EisSource::Portal { token_path } => open_portal(token_path).await,
        EisSource::KwinDirect => open_kwin().await,
    }
}

async fn open_portal(token_path: &Path) -> Result<(OwnedFd, SessionHandle), SourceError> {
    register_host_app().await;
    let token = read_token(token_path.to_owned()).await;
    debug!(
        restore_token = token.is_some(),
        "starting RemoteDesktop session"
    );

    let portal = RemoteDesktop::new().await.map_err(portal_error)?;
    if portal.version() < 2 {
        return Err(SourceError::Unavailable(format!(
            "RemoteDesktop portal version {} has no ConnectToEIS (needs 2)",
            portal.version()
        )));
    }
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(portal_error)?;
    let opened = start_portal_session(&portal, &session, token.as_deref(), token_path).await;
    match opened {
        Ok(fd) => Ok((
            fd,
            SessionHandle::Portal {
                _portal: portal,
                session,
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
        .map_err(portal_error)?;
    if !started.devices().contains(DeviceType::KEYBOARD) {
        return Err(SourceError::PermissionDenied);
    }
    // Tokens are single-use: always keep the newest one.
    if let Some(new_token) = started.restore_token()
        && let Err(e) = write_token(token_path.to_owned(), new_token.to_owned()).await
    {
        warn!("cannot save the RemoteDesktop restore token: {e}");
    }
    portal
        .connect_to_eis(session, ConnectToEISOptions::default())
        .await
        .map_err(portal_error)
}

fn portal_error(e: ashpd::Error) -> SourceError {
    match e {
        ashpd::Error::Response(ResponseError::Cancelled) => SourceError::PermissionDenied,
        // KDE and GNOME report a denied dialog as "cancelled"; "other" is any other way the
        // interaction ended. Neither granted typing, so both count as a denial.
        ashpd::Error::Response(ResponseError::Other) => SourceError::PermissionDenied,
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

/// Host apps (not Flatpak) must register their app ID before the first portal call.
/// Done once per process; failure is logged and ignored.
async fn register_host_app() {
    static REGISTERED: OnceCell<()> = OnceCell::const_new();
    REGISTERED
        .get_or_init(|| async {
            if ashpd::is_sandboxed() {
                return;
            }
            let res = match ashpd::AppID::try_from(crate::paths::APP_ID) {
                Ok(id) => ashpd::register_host_app(id).await,
                Err(e) => Err(e),
            };
            match res {
                Ok(()) => info!(app_id = crate::paths::APP_ID, "registered host app"),
                Err(e) => warn!("register_host_app failed (continuing): {e}"),
            }
        })
        .await;
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
