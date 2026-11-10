//! Async I/O for a reis `ei::Context`, driving reis' synchronous `read()`, `pending_event()`
//! and `flush()` from tokio readiness.
//!
//! This replaces `reis::tokio::EiEventStream`, which in reis 0.7.1 can lose a wakeup: when a
//! read leaves only part of a message buffered, it clears readiness and returns `Pending`
//! without re-arming it, so the task sleeps until something else wakes it. That looked like a
//! compositor stall (the "KWin flush stall" in docs/portal-findings.md); the `sync` pokes only
//! helped because their timer polled the stream again.

use std::{io, os::unix::net::UnixStream};

use reis::{PendingRequestResult, ei};
use tokio::io::Interest;

/// One read from the EI socket: `None` at end of stream.
pub(crate) type RawEvent = Option<io::Result<PendingRequestResult<ei::Event>>>;

/// An EI client socket. All bytes go through reis' `ei::Context`; a dup of the socket is
/// registered with tokio only to wait for readiness (a safe API, unlike `AsyncFd` in current
/// tokio).
pub(crate) struct EiSocket {
    ctx: ei::Context,
    readiness: tokio::net::UnixStream,
}

impl EiSocket {
    /// Wraps a connected socket. Must be called inside a tokio runtime.
    pub(crate) fn new(socket: UnixStream) -> io::Result<Self> {
        let dup = socket.try_clone()?;
        // Sets O_NONBLOCK on the file description, which the dup shares.
        let ctx = ei::Context::new(socket)?;
        let readiness = tokio::net::UnixStream::from_std(dup)?;
        Ok(Self { ctx, readiness })
    }

    /// Next event from the server. Cancel-safe: bytes already read stay buffered in reis.
    pub(crate) async fn next(&self) -> RawEvent {
        loop {
            if let Some(event) = self.ctx.pending_event() {
                return Some(Ok(event));
            }
            if let Err(e) = self.readiness.readable().await {
                return Some(Err(e));
            }
            // reis reads until the socket would block. Readiness is cleared only when this
            // returns WouldBlock, so a read that got nothing must report it: otherwise tokio
            // would keep the stale "readable" flag, or (the reis bug) never re-arm.
            let read = self
                .readiness
                .try_io(Interest::READABLE, || match self.ctx.read() {
                    Ok(0) => Err(io::ErrorKind::WouldBlock.into()),
                    other => other,
                });
            match read {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }

    /// Sends everything buffered, waiting while the socket is full.
    pub(crate) async fn flush(&self) -> io::Result<()> {
        let flush = || self.ctx.flush().map_err(io::Error::from);
        loop {
            match flush() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                done => return done,
            }
            // The socket is full (rare: bursts are small). reis keeps the unsent rest.
            self.readiness.writable().await?;
            // `try_io` clears the readiness flag if the flush hits WouldBlock again.
            match self.readiness.try_io(Interest::WRITABLE, flush) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                done => return done,
            }
        }
    }

    /// Flush for small control messages; if the socket is full, the next flush sends them.
    pub(crate) fn try_flush(&self) {
        match self.ctx.flush() {
            Ok(()) | Err(rustix::io::Errno::AGAIN) => {}
            Err(e) => tracing::debug!("EIS flush failed: {e}"),
        }
    }

    /// Runs the client side of the EI handshake.
    pub(crate) async fn handshake(
        &self,
        name: &str,
    ) -> Result<reis::handshake::HandshakeResp, String> {
        let mut handshaker =
            reis::handshake::EiHandshaker::new(name, ei::handshake::ContextType::Sender);
        loop {
            match self.next().await {
                Some(Ok(PendingRequestResult::Request(event))) => {
                    match handshaker.handle_event(event) {
                        Ok(Some(resp)) => return Ok(resp),
                        Ok(None) => {}
                        Err(e) => return Err(format!("EI handshake: {e}")),
                    }
                }
                Some(Ok(PendingRequestResult::ParseError(e))) => {
                    return Err(format!("EI handshake: {e}"));
                }
                Some(Ok(PendingRequestResult::InvalidObject(id))) => {
                    return Err(format!("EI handshake: invalid object {id}"));
                }
                Some(Err(e)) => return Err(format!("EI handshake: {e}")),
                None => return Err("EI handshake: connection closed".into()),
            }
        }
    }
}
