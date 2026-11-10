//! The background task behind [`Output`](super::Output): owns the session and EIS
//! connection, queues text, and types each queued string as one burst once the compositor's
//! state is confirmed fresh by a `sync` round trip.

use std::collections::VecDeque;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, trace, warn};

use super::{
    EisSource, OutputEvent,
    eis::{EisConnection, EisError, Update},
    source::{self, SessionHandle, SourceError},
};

/// Requests from [`Output`](super::Output).
pub(super) enum Cmd {
    Type(String),
    Flush(oneshot::Sender<()>),
}

/// Where the text at the front of the queue stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// The known modifier/layout state may be outdated: `sync` before typing.
    Stale,
    /// A `sync` is in flight.
    Syncing,
    /// The `sync` completed; the state is current.
    Fresh,
    /// Control/Alt/Super is held: wait for a modifiers event.
    Held,
}

struct Link {
    conn: EisConnection,
    session: SessionHandle,
}

struct Actor {
    source: EisSource,
    events: mpsc::UnboundedSender<OutputEvent>,
    rx: mpsc::UnboundedReceiver<Cmd>,
    rx_closed: bool,
    queue: VecDeque<Cmd>,
    link: Option<Link>,
    phase: Phase,
    /// Something was typed on the current link. A link lost before that drops the queue
    /// instead of reconnecting right away, so a compositor that keeps closing the connection
    /// can't make us reconnect in a loop.
    typed_on_link: bool,
}

enum Wake {
    Cmd(Option<Cmd>),
    Eis(super::eis::RawEvent),
}

pub(super) async fn run(
    source: EisSource,
    events: mpsc::UnboundedSender<OutputEvent>,
    rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut actor = Actor {
        source,
        events,
        rx,
        rx_closed: false,
        queue: VecDeque::new(),
        link: None,
        phase: Phase::Stale,
        typed_on_link: false,
    };
    // Eager: the first word shouldn't wait for session setup (or the permission dialog).
    actor.connect().await;
    actor.run().await;
}

impl Actor {
    async fn run(&mut self) {
        loop {
            self.pump().await;
            if self.rx_closed && self.queue.is_empty() {
                break;
            }
            if self.link.is_none() && self.has_text() {
                // Lazy reconnect: text is waiting.
                self.connect().await;
                continue;
            }
            let wake = match (&mut self.link, self.rx_closed) {
                (Some(link), false) => tokio::select! {
                    cmd = self.rx.recv() => Wake::Cmd(cmd),
                    raw = link.conn.next_raw() => Wake::Eis(raw),
                },
                (Some(link), true) => Wake::Eis(link.conn.next_raw().await),
                (None, false) => Wake::Cmd(self.rx.recv().await),
                // Unreachable: closed with an empty queue breaks above, and queued text
                // without a link reconnects above. Flushes alone were resolved by `pump`.
                (None, true) => break,
            };
            match wake {
                Wake::Cmd(Some(cmd)) => self.queue.push_back(cmd),
                Wake::Cmd(None) => self.rx_closed = true,
                Wake::Eis(raw) => self.on_eis(raw).await,
            }
        }
        if let Some(link) = self.link.take() {
            link.conn.disconnect();
            link.session.close().await;
        }
        debug!("output task finished");
    }

    fn has_text(&self) -> bool {
        self.queue.iter().any(|c| matches!(c, Cmd::Type(_)))
    }

    fn emit(&self, event: OutputEvent) {
        // The receiver may be gone during shutdown; nothing to do then.
        let _ = self.events.send(event);
    }

    async fn connect(&mut self) {
        let result = match source::open(&self.source).await {
            Ok((fd, session)) => match EisConnection::establish(fd).await {
                Ok(conn) => Ok(Link { conn, session }),
                Err(e) => {
                    session.close().await;
                    Err(match e {
                        EisError::Unsupported(why) => OutputEvent::Unavailable(why),
                        EisError::Lost(why) => OutputEvent::Disconnected(why),
                    })
                }
            },
            Err(SourceError::PermissionDenied) => Err(OutputEvent::PermissionDenied),
            Err(SourceError::Unavailable(why)) => Err(OutputEvent::Unavailable(why)),
            Err(SourceError::Failed(why)) => Err(OutputEvent::Disconnected(why)),
        };
        match result {
            Ok(link) => {
                debug!("EIS keyboard ready");
                self.link = Some(link);
                self.phase = Phase::Stale;
                self.typed_on_link = false;
                self.emit(OutputEvent::Ready);
            }
            Err(event) => {
                warn!("typing unavailable: {event:?}");
                self.drop_pending();
                self.emit(event);
            }
        }
    }

    /// Drops everything queued so far (including what is still in the channel), so that a
    /// failed connection isn't retried once per queued word. Pending flushes resolve.
    fn drop_pending(&mut self) {
        while let Ok(cmd) = self.rx.try_recv() {
            self.queue.push_back(cmd);
        }
        let words = self
            .queue
            .iter()
            .filter(|c| matches!(c, Cmd::Type(_)))
            .count();
        if words > 0 {
            debug!(words, "dropping queued text");
        }
        self.queue.clear();
    }

    async fn on_eis(&mut self, raw: super::eis::RawEvent) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        match link.conn.handle(raw).await {
            Update::None => {}
            Update::Synced => {
                if self.phase == Phase::Syncing {
                    self.phase = Phase::Fresh;
                }
            }
            Update::Modifiers => {
                if self.phase == Phase::Held {
                    // Re-check with a fresh round trip before typing.
                    self.phase = Phase::Stale;
                }
            }
            Update::Readiness => {
                if link.conn.keyboard().is_some() {
                    debug!("EIS keyboard usable again");
                    if self.phase != Phase::Syncing {
                        self.phase = Phase::Stale;
                    }
                    self.emit(OutputEvent::Ready);
                } else {
                    debug!("EIS keyboard not usable (paused or removed)");
                }
            }
            Update::Lost(why) => self.lose(why).await,
        }
    }

    async fn lose(&mut self, why: String) {
        warn!("EIS connection lost: {why}");
        if let Some(link) = self.link.take() {
            link.session.close().await;
        }
        self.phase = Phase::Stale;
        if !self.typed_on_link {
            // Lost before typing anything: don't retry for the queued text right away.
            self.drop_pending();
        }
        self.emit(OutputEvent::Disconnected(why));
    }

    /// Advances the queue as far as the current state allows.
    async fn pump(&mut self) {
        loop {
            match self.queue.front() {
                None => return,
                Some(Cmd::Flush(_)) => {
                    if let Some(Cmd::Flush(done)) = self.queue.pop_front() {
                        let _ = done.send(());
                    }
                    continue;
                }
                Some(Cmd::Type(_)) => {}
            }
            let Some(link) = self.link.as_mut() else {
                return;
            };
            let Some((table, mods)) = link.conn.keyboard() else {
                return; // Paused or being replaced; `Update::Readiness` resumes us.
            };
            match self.phase {
                Phase::Syncing | Phase::Held => return,
                Phase::Stale => {
                    let synced = match link.conn.learn_modifiers().await {
                        Ok(()) => link.conn.start_sync().await,
                        Err(e) => Err(e),
                    };
                    match synced {
                        Ok(true) => self.phase = Phase::Syncing,
                        // No `sync` support: the state we have is the best we'll get.
                        Ok(false) => self.phase = Phase::Fresh,
                        Err(e) => {
                            self.lose(format!("EIS write error: {e}")).await;
                            return;
                        }
                    }
                    continue;
                }
                Phase::Fresh => {}
            }
            if table.blocks_typing(&mods) {
                debug!("Ctrl/Alt/Super held: holding text until released");
                self.phase = Phase::Held;
                return;
            }
            let Some(Cmd::Type(text)) = self.queue.pop_front() else {
                return; // Unreachable: the front was checked to be text above.
            };
            let plan = table.plan(&text, &mods);
            trace!(chars = text.chars().count(), group = mods.group, "typing");
            if plan.control > 0 {
                debug!(count = plan.control, "skipped control characters");
            }
            if plan.unmapped > 0 {
                debug!(
                    count = plan.unmapped,
                    group = mods.group,
                    "skipped characters missing from the active layout"
                );
            }
            let unmapped = plan.unmapped;
            if unmapped > 0 {
                // Field access, not `emit`: `link` still borrows `self.link`.
                let _ = self.events.send(OutputEvent::Unmapped { count: unmapped });
            }
            if plan.events.is_empty() {
                continue; // Nothing typeable; the state is still fresh.
            }
            let result = link.conn.burst(&plan.events).await;
            // Typing changes the modifier state; the next burst syncs again.
            self.phase = Phase::Stale;
            match result {
                Ok(()) => self.typed_on_link = true,
                Err(e) => {
                    self.lose(format!("EIS write error: {e}")).await;
                    return;
                }
            }
        }
    }
}
