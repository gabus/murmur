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

    /// Runs the client side of the EI handshake (reis' sync `EiHandshaker`).
    pub(crate) async fn handshake(
        &self,
        name: &str,
    ) -> Result<reis::handshake::HandshakeResp, HandshakeError> {
        let mut handshaker =
            reis::handshake::EiHandshaker::new(name, ei::handshake::ContextType::Sender);
        loop {
            let event = match self.next().await {
                Some(Ok(PendingRequestResult::Request(event))) => event,
                Some(Ok(PendingRequestResult::ParseError(e))) => return Err(e.into()),
                Some(Ok(PendingRequestResult::InvalidObject(id))) => {
                    return Err(HandshakeError::InvalidObject(id));
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Err(HandshakeError::Closed),
            };
            if let Some(resp) = handshaker.handle_event(event)? {
                return Ok(resp);
            }
        }
    }
}

/// Why the EI handshake failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HandshakeError {
    #[error("EI handshake I/O error")]
    Io(#[from] io::Error),
    #[error("EI handshake message could not be parsed")]
    Parse(#[from] reis::ParseError),
    #[error("EI handshake protocol error")]
    Protocol(#[from] reis::handshake::HandshakeError),
    #[error("EI handshake referenced unknown object {0}")]
    InvalidObject(u64),
    #[error("EIS closed the connection during the handshake")]
    Closed,
}

#[cfg(test)]
mod tests {
    use std::{io::Write, time::Duration};

    use super::*;

    /// `ei_handshake.handshake_version(1)`: object 0, length 20, opcode 0, then the version.
    fn handshake_version() -> Vec<u8> {
        [
            0u64.to_ne_bytes().as_slice(),
            &20u32.to_ne_bytes(),
            &0u32.to_ne_bytes(),
            &1u32.to_ne_bytes(),
        ]
        .concat()
    }

    fn is_handshake_version(event: &RawEvent) -> bool {
        matches!(
            event,
            Some(Ok(PendingRequestResult::Request(ei::Event::Handshake(
                _,
                ei::handshake::Event::HandshakeVersion { version: 1 }
            ))))
        )
    }

    // reis 0.7.1's tokio stream fails this: its first read finds half a message, clears
    // readiness and returns Pending without re-arming, so the second half, arriving while the
    // same `next()` is pending, doesn't wake it. (Measured: it returned only when the 3 s
    // timeout's timer re-polled it, which is also why periodic `sync` pokes "fixed" setup.)
    #[tokio::test]
    async fn message_split_across_writes_wakes_the_reader() {
        let (client, mut server) = UnixStream::pair().expect("socketpair");
        let socket = EiSocket::new(client).expect("EI socket");
        let writer = std::thread::spawn(move || {
            let msg = handshake_version();
            server.write_all(&msg[..10]).expect("write first half");
            std::thread::sleep(Duration::from_millis(100));
            server.write_all(&msg[10..]).expect("write second half");
            server
        });
        let start = std::time::Instant::now();
        let event = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("reader woken by the rest of the message");
        let waited = start.elapsed();
        let _server = writer.join().expect("writer thread");
        assert!(is_handshake_version(&event), "got {event:?}");
        assert!(
            waited < Duration::from_secs(1),
            "woken only after {waited:?}: lost wakeup"
        );
    }

    #[tokio::test]
    async fn buffered_events_come_before_end_of_stream() {
        let (client, mut server) = UnixStream::pair().expect("socketpair");
        let socket = EiSocket::new(client).expect("EI socket");
        server
            .write_all(&[handshake_version(), handshake_version()].concat())
            .expect("write");
        drop(server);
        assert!(is_handshake_version(&socket.next().await));
        assert!(is_handshake_version(&socket.next().await));
        assert!(socket.next().await.is_none(), "end of stream");
    }
}
