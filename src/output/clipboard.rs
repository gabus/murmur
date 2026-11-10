//! Clipboard access for the paste fallback (D-011): the portal's Clipboard interface on the
//! RemoteDesktop session, behind [`ClipboardAccess`] so the paste flow can be tested with a
//! fake.
//!
//! Through the portal, murmur can only *own* the selection (offer data and answer
//! `SelectionTransfer` requests) or read the selection someone else owns. "Restoring" the
//! previous clipboard therefore means reading it before pasting and then offering it again:
//! murmur keeps serving it until another app takes the selection over. If murmur's session
//! ends first, that content is gone (as with any app that quits after a copy).
//!
//! Clipboard data is never logged.

use std::{
    future::Future,
    io,
    os::fd::OwnedFd,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use ashpd::{
    desktop::{
        Session,
        clipboard::{Clipboard, SelectionOwnerChanged, SetSelectionOptions},
        remote_desktop::RemoteDesktop,
    },
    zbus::{
        self, ObjectPath, OwnedObjectPath,
        wire::{LE, serialized::Context},
    },
};
use futures_util::StreamExt;
use tokio::{net::unix::pipe, sync::watch};
use tracing::debug;

use super::source::AbortOnDrop;

/// A boxed future, so [`ClipboardAccess`] can be used as a trait object.
pub(crate) type ClipFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Mime types murmur offers for pasted text (what `wl-copy` offers for text).
pub(crate) const TEXT_MIME_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// X11 selection targets that XWayland may advertise but that aren't content.
const META_TARGETS: [&str; 7] = [
    "TARGETS",
    "MULTIPLE",
    "TIMESTAMP",
    "SAVE_TARGETS",
    "DELETE",
    "INSERT_PROPERTY",
    "INSERT_SELECTION",
];

/// Largest content kept per mime type, and in total, when saving the previous clipboard.
const MAX_TYPE_BYTES: usize = 8 << 20;
const MAX_TOTAL_BYTES: usize = 32 << 20;
/// How long reading one mime type of the previous clipboard may take.
const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// How long writing the offered content to a reader may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClipError {
    /// A portal call failed.
    #[error("clipboard portal: {0}")]
    Portal(String),
    /// The previous clipboard is too large to keep for restoring.
    #[error("the clipboard content is too large to keep")]
    TooLarge,
    /// The previous clipboard announced content but none of it could be read.
    #[error("the clipboard content could not be read")]
    Unreadable,
    /// Reading or writing a transfer pipe failed.
    #[error("clipboard transfer: {0}")]
    Io(#[from] io::Error),
}

/// Clipboard content: data per mime type, in the owner's order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Offer(Vec<(String, Arc<[u8]>)>);

impl Offer {
    /// `text` as UTF-8 under every [`TEXT_MIME_TYPES`] entry.
    pub(crate) fn text(text: &str) -> Self {
        let data: Arc<[u8]> = Arc::from(text.as_bytes());
        Self(
            TEXT_MIME_TYPES
                .iter()
                .map(|m| ((*m).to_owned(), Arc::clone(&data)))
                .collect(),
        )
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn mime_types(&self) -> Vec<&str> {
        self.0.iter().map(|(m, _)| m.as_str()).collect()
    }

    pub(crate) fn get(&self, mime_type: &str) -> Option<Arc<[u8]>> {
        self.0
            .iter()
            .find(|(m, _)| m == mime_type)
            .map(|(_, d)| Arc::clone(d))
    }
}

/// What the paste flow needs from the clipboard.
pub(crate) trait ClipboardAccess: Send + Sync {
    /// The current selection, saved so it can be offered again after pasting. An error means
    /// it can't be saved well enough to restore it; then nothing should be pasted.
    fn snapshot(&self) -> ClipFuture<'_, Result<Offer, ClipError>>;

    /// Makes `offer` the selection, owned and served by murmur from now on. An empty offer
    /// keeps murmur the owner but serves nothing, which reads as an empty clipboard.
    fn set(&self, offer: Offer) -> ClipFuture<'_, Result<(), ClipError>>;

    /// Counts the transfers murmur has served, i.e. reads of a selection murmur owns.
    fn served(&self) -> watch::Receiver<u64>;
}

/// Decides what to keep of the previous clipboard after reading it. `announced`: the owner
/// announced these types (`false`: nothing was announced, so the text types were tried).
pub(crate) fn keep_snapshot(
    announced: bool,
    reads: Vec<(String, Result<Vec<u8>, ClipError>)>,
) -> Result<Offer, ClipError> {
    let mut kept = Vec::new();
    let mut total = 0usize;
    let mut failed = 0usize;
    for (mime_type, read) in reads {
        match read {
            Ok(data) => {
                total += data.len();
                if total > MAX_TOTAL_BYTES {
                    return Err(ClipError::TooLarge);
                }
                kept.push((mime_type, Arc::from(data)));
            }
            Err(ClipError::TooLarge) => return Err(ClipError::TooLarge),
            Err(e) => {
                debug!(mime_type, "cannot read this clipboard type: {e}");
                failed += 1;
            }
        }
    }
    if announced && kept.is_empty() && failed > 0 {
        // There is content, but restoring it isn't possible: don't overwrite it.
        return Err(ClipError::Unreadable);
    }
    Ok(Offer(kept))
}

/// Who owns the selection, as far as the portal told us.
#[derive(Clone, Debug)]
enum Owner {
    /// Nothing announced yet (the portal reports changes only).
    Unknown,
    Us,
    /// Another app, offering these mime types.
    Other(Vec<String>),
}

struct State {
    /// What murmur serves while it owns the selection.
    offer: Offer,
    owner: Owner,
}

struct Inner {
    clipboard: Clipboard,
    session: Arc<Session<RemoteDesktop>>,
    state: Mutex<State>,
    served: watch::Sender<u64>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Answers a `SelectionTransfer` from what murmur currently offers.
    async fn serve(&self, mime_type: &str, serial: u32) {
        let data = self.state().offer.get(mime_type);
        let ok = match data {
            Some(data) => match self.clipboard.selection_write(&self.session, serial).await {
                Ok(fd) => match write_fd(fd.into(), data).await {
                    Ok(()) => true,
                    Err(e) => {
                        debug!("serving the clipboard failed: {e}");
                        false
                    }
                },
                Err(e) => {
                    debug!("SelectionWrite failed: {e}");
                    false
                }
            },
            None => false,
        };
        if let Err(e) = self
            .clipboard
            .selection_write_done(&self.session, serial, ok)
            .await
        {
            debug!("SelectionWriteDone failed: {e}");
        }
        if ok {
            self.served.send_modify(|n| *n += 1);
        }
    }

    fn owner_changed(&self, change: &SelectionOwnerChanged) {
        let mut state = self.state();
        match change.session_is_owner() {
            Some(true) => state.owner = Owner::Us,
            Some(false) => {
                state.owner = Owner::Other(change.mime_types().to_vec());
                // Someone else's now; murmur no longer needs to keep its copy.
                state.offer = Offer::default();
            }
            // Unspecified: keep serving what we have, but remember the announced types.
            None => {
                if !matches!(state.owner, Owner::Us) {
                    state.owner = Owner::Other(change.mime_types().to_vec());
                }
            }
        }
    }

    async fn read(&self, mime_type: &str) -> Result<Vec<u8>, ClipError> {
        let fd = self
            .clipboard
            .selection_read(&self.session, mime_type)
            .await
            .map_err(|e| ClipError::Portal(e.to_string()))?;
        match tokio::time::timeout(READ_TIMEOUT, read_fd(fd.into(), MAX_TYPE_BYTES)).await {
            Ok(read) => read,
            Err(_) => Err(ClipError::Io(io::ErrorKind::TimedOut.into())),
        }
    }
}

/// The portal's clipboard on a started RemoteDesktop session with clipboard access.
pub(crate) struct PortalClipboard {
    inner: Arc<Inner>,
    _server: AbortOnDrop,
}

impl PortalClipboard {
    /// Starts answering `SelectionTransfer` and tracking `SelectionOwnerChanged` for
    /// `session`. Returns once subscribed to both signals.
    pub(crate) async fn start(
        clipboard: Clipboard,
        session: Arc<Session<RemoteDesktop>>,
    ) -> Result<Self, ClipError> {
        let portal = |e: zbus::Error| ClipError::Portal(e.to_string());
        let mut transfers = clipboard
            .receive_signal("SelectionTransfer")
            .await
            .map_err(portal)?;
        let mut owners = clipboard
            .receive_signal("SelectionOwnerChanged")
            .await
            .map_err(portal)?;
        let ours = session_path(&session);
        let (served, _) = watch::channel(0);
        let inner = Arc::new(Inner {
            clipboard,
            session,
            state: Mutex::new(State {
                offer: Offer::default(),
                owner: Owner::Unknown,
            }),
            served,
        });
        let server = Arc::clone(&inner);
        let mine = move |path: &ObjectPath<'_>| ours.as_ref().is_none_or(|o| o.as_ref() == *path);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = transfers.next() => {
                        let body = msg.body();
                        match body.deserialize::<(ObjectPath<'_>, String, u32)>() {
                            Ok((path, mime_type, serial)) if mine(&path) => {
                                server.serve(&mime_type, serial).await;
                            }
                            Ok(_) => {}
                            Err(e) => debug!("bad SelectionTransfer signal: {e}"),
                        }
                    }
                    Some(msg) = owners.next() => {
                        let body = msg.body();
                        match body.deserialize::<(ObjectPath<'_>, SelectionOwnerChanged)>() {
                            Ok((path, change)) if mine(&path) => server.owner_changed(&change),
                            Ok(_) => {}
                            Err(e) => debug!("bad SelectionOwnerChanged signal: {e}"),
                        }
                    }
                    else => break,
                }
            }
        });
        Ok(Self {
            inner,
            _server: AbortOnDrop::new(task.abort_handle()),
        })
    }
}

impl ClipboardAccess for PortalClipboard {
    fn snapshot(&self) -> ClipFuture<'_, Result<Offer, ClipError>> {
        Box::pin(async move {
            let (owner, ours) = {
                let state = self.inner.state();
                (state.owner.clone(), state.offer.clone())
            };
            let (announced, types) = match owner {
                Owner::Us => return Ok(ours),
                Owner::Other(types) => (
                    true,
                    types
                        .into_iter()
                        .filter(|t| !META_TARGETS.contains(&t.as_str()))
                        .collect(),
                ),
                Owner::Unknown => (false, TEXT_MIME_TYPES.map(str::to_owned).to_vec()),
            };
            let mut reads = Vec::with_capacity(types.len());
            for mime_type in types {
                let read = self.inner.read(&mime_type).await;
                reads.push((mime_type, read));
            }
            keep_snapshot(announced, reads)
        })
    }

    fn set(&self, offer: Offer) -> ClipFuture<'_, Result<(), ClipError>> {
        Box::pin(async move {
            let empty = offer.is_empty();
            let types: Vec<String> = offer.mime_types().into_iter().map(str::to_owned).collect();
            {
                let mut state = self.inner.state();
                state.offer = offer;
                state.owner = Owner::Us;
            }
            if empty {
                return Ok(()); // Keep the selection, serve nothing.
            }
            let types: Vec<&str> = types.iter().map(String::as_str).collect();
            self.inner
                .clipboard
                .set_selection(
                    &self.inner.session,
                    SetSelectionOptions::default().set_mime_types(&types),
                )
                .await
                .map_err(|e| ClipError::Portal(e.to_string()))
        })
    }

    fn served(&self) -> watch::Receiver<u64> {
        self.inner.served.subscribe()
    }
}

/// The session's object path, to pick its signals out (they carry the path). ashpd keeps the
/// path private but serializes a session as it.
fn session_path(session: &Session<RemoteDesktop>) -> Option<OwnedObjectPath> {
    let data = zbus::wire::to_bytes(Context::new(LE, 0), session).ok()?;
    let (path, _) = data.deserialize::<ObjectPath<'_>>().ok()?;
    Some(path.into())
}

fn is_fifo(fd: OwnedFd) -> (OwnedFd, bool) {
    use std::os::unix::fs::FileTypeExt;
    let file = std::fs::File::from(fd);
    let fifo = file.metadata().is_ok_and(|m| m.file_type().is_fifo());
    (file.into(), fifo)
}

/// Reads `fd` to its end, failing with [`ClipError::TooLarge`] beyond `cap` bytes.
async fn read_fd(fd: OwnedFd, cap: usize) -> Result<Vec<u8>, ClipError> {
    let (fd, fifo) = is_fifo(fd);
    if !fifo {
        // Not a pipe (tokio only drives pipes): read it on a blocking thread.
        return tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut data = Vec::new();
            std::fs::File::from(fd)
                .take(u64::try_from(cap).unwrap_or(u64::MAX) + 1)
                .read_to_end(&mut data)?;
            if data.len() > cap {
                return Err(ClipError::TooLarge);
            }
            Ok(data)
        })
        .await
        .map_err(|e| ClipError::Io(io::Error::other(e)))?;
    }
    let pipe = pipe::Receiver::from_owned_fd(fd)?;
    let mut data = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        pipe.readable().await?;
        match pipe.try_read(&mut chunk) {
            Ok(0) => return Ok(data),
            Ok(n) => {
                data.extend_from_slice(&chunk[..n]);
                if data.len() > cap {
                    return Err(ClipError::TooLarge);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// Writes all of `data` to `fd` and closes it.
async fn write_fd(fd: OwnedFd, data: Arc<[u8]>) -> Result<(), ClipError> {
    let (fd, fifo) = is_fifo(fd);
    let write = async move {
        if !fifo {
            return tokio::task::spawn_blocking(move || {
                use std::io::Write;
                std::fs::File::from(fd).write_all(&data)
            })
            .await
            .map_err(io::Error::other)?;
        }
        let pipe = pipe::Sender::from_owned_fd(fd)?;
        let mut rest: &[u8] = &data;
        while !rest.is_empty() {
            pipe.writable().await?;
            match pipe.try_write(rest) {
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    };
    match tokio::time::timeout(WRITE_TIMEOUT, write).await {
        Ok(result) => result.map_err(ClipError::from),
        Err(_) => Err(ClipError::Io(io::ErrorKind::TimedOut.into())),
    }
}

/// A clipboard for tests: [`FakeClipboard::paste`] plays the app that reads the selection.
#[cfg(test)]
pub(crate) struct FakeClipboard {
    state: Mutex<FakeState>,
    served: watch::Sender<u64>,
}

#[cfg(test)]
#[derive(Default)]
struct FakeState {
    offer: Offer,
    /// Every offer set, in order.
    sets: Vec<Offer>,
    /// `snapshot` fails like an unreadable clipboard.
    unreadable: bool,
    /// A clipboard manager reads every new selection as soon as it is set.
    manager: bool,
}

#[cfg(test)]
impl FakeClipboard {
    pub(crate) fn new(current: Offer) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(FakeState {
                offer: current,
                ..FakeState::default()
            }),
            served: watch::channel(0).0,
        })
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn make_unreadable(&self) {
        self.lock().unreadable = true;
    }

    /// Adds a clipboard manager that reads every new selection right away (like Klipper).
    pub(crate) fn add_manager(&self) {
        self.lock().manager = true;
    }

    /// The current selection.
    pub(crate) fn offer(&self) -> Offer {
        self.lock().offer.clone()
    }

    /// Offers set so far.
    pub(crate) fn sets(&self) -> Vec<Offer> {
        self.lock().sets.clone()
    }

    /// An app pastes: reads the selection's text, which counts as a served transfer.
    pub(crate) fn paste(&self) -> Option<String> {
        let data = self.lock().offer.get(TEXT_MIME_TYPES[0])?;
        self.served.send_modify(|n| *n += 1);
        Some(String::from_utf8_lossy(&data).into_owned())
    }
}

#[cfg(test)]
impl ClipboardAccess for FakeClipboard {
    fn snapshot(&self) -> ClipFuture<'_, Result<Offer, ClipError>> {
        Box::pin(async move {
            let state = self.lock();
            if state.unreadable {
                return Err(ClipError::Unreadable);
            }
            Ok(state.offer.clone())
        })
    }

    fn set(&self, offer: Offer) -> ClipFuture<'_, Result<(), ClipError>> {
        Box::pin(async move {
            let manager = {
                let mut state = self.lock();
                state.sets.push(offer.clone());
                state.offer = offer;
                state.manager && !state.offer.is_empty()
            };
            if manager {
                self.served.send_modify(|n| *n += 1);
            }
            Ok(())
        })
    }

    fn served(&self) -> watch::Receiver<u64> {
        self.served.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(data: &str) -> Result<Vec<u8>, ClipError> {
        Ok(data.as_bytes().to_vec())
    }

    fn failed() -> Result<Vec<u8>, ClipError> {
        Err(ClipError::Portal("no such type".into()))
    }

    #[test]
    fn text_offer_has_every_text_type() {
        let offer = Offer::text("ą");
        assert_eq!(offer.mime_types(), TEXT_MIME_TYPES);
        assert_eq!(offer.get("UTF8_STRING").as_deref(), Some("ą".as_bytes()));
        assert_eq!(offer.get("image/png"), None);
    }

    #[test]
    fn snapshot_keeps_what_could_be_read() {
        let kept = keep_snapshot(
            true,
            vec![
                ("text/html".into(), failed()),
                ("text/plain".into(), read("hello")),
            ],
        );
        let kept = kept.expect("restorable");
        assert_eq!(kept.mime_types(), ["text/plain"]);
    }

    #[test]
    fn announced_but_unreadable_content_is_not_overwritten() {
        let kept = keep_snapshot(true, vec![("image/png".into(), failed())]);
        assert!(matches!(kept, Err(ClipError::Unreadable)));
    }

    #[test]
    fn unknown_unreadable_content_counts_as_empty() {
        let kept = keep_snapshot(false, vec![("text/plain".into(), failed())]);
        assert!(kept.expect("restorable").is_empty());
        // An announced empty clipboard is empty too.
        assert!(keep_snapshot(true, vec![]).expect("empty").is_empty());
    }

    #[test]
    fn too_large_content_is_not_overwritten() {
        let kept = keep_snapshot(
            true,
            vec![
                ("text/plain".into(), read("hello")),
                ("image/png".into(), Err(ClipError::TooLarge)),
            ],
        );
        assert!(matches!(kept, Err(ClipError::TooLarge)));
        let big = vec![0u8; MAX_TOTAL_BYTES / 2 + 1];
        let kept = keep_snapshot(
            true,
            vec![
                ("image/png".into(), Ok(big.clone())),
                ("image/bmp".into(), Ok(big)),
            ],
        );
        assert!(matches!(kept, Err(ClipError::TooLarge)));
    }

    #[tokio::test]
    async fn pipes_are_read_and_written_completely() {
        let (rx, tx) = std::io::pipe().expect("pipe");
        let data: Arc<[u8]> = Arc::from(vec![7u8; 200 << 10]);
        let (wrote, read) = tokio::join!(
            write_fd(OwnedFd::from(tx), Arc::clone(&data)),
            read_fd(OwnedFd::from(rx), MAX_TYPE_BYTES)
        );
        assert!(wrote.is_ok(), "{wrote:?}");
        assert_eq!(read.expect("read").as_slice(), &*data);

        let (rx, tx) = std::io::pipe().expect("pipe");
        let (_, read) = tokio::join!(
            write_fd(OwnedFd::from(tx), Arc::clone(&data)),
            read_fd(OwnedFd::from(rx), 1000)
        );
        assert!(matches!(read, Err(ClipError::TooLarge)));
    }
}
