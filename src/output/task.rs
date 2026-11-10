//! The background task behind [`Output`](super::Output): owns the session and EIS
//! connection, queues text, and types each queued string as one burst once the compositor's
//! state is confirmed fresh by a `sync` round trip.

use std::{collections::VecDeque, time::Duration};

use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};
use tracing::{debug, trace, warn};

use super::{
    EisSource, OutputEvent, PasteKeys,
    clipboard::Offer,
    eis::{EisConnection, EisError, Update},
    paste::{self, Outcome},
    source::{self, SessionHandle, SourceError},
};

/// After every `Output` handle is dropped, how long queued text may still wait (for a held
/// modifier to be released or a paused device to resume) before it is dropped.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How long text may wait (held modifier, paused keyboard) before [`OutputEvent::Waiting`].
const WAITING_AFTER: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(2)
};

/// Requests from [`Output`](super::Output).
pub(super) enum Cmd {
    Type(String),
    Flush(oneshot::Sender<()>),
    Retry,
    PasteKeys(PasteKeys),
}

/// What waits in the queue, in order.
enum Queued {
    Text(String),
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
    /// The user holds a modifier (or has latched or locked one): wait for a modifiers event.
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
    /// Set once every `Output` handle is gone: leftover text may wait until then.
    shutdown_deadline: Option<Instant>,
    queue: VecDeque<Queued>,
    link: Option<Link>,
    phase: Phase,
    /// Something was typed on the current link. A link lost before that drops the queue
    /// instead of reconnecting right away, so a compositor that keeps closing the connection
    /// can't make us reconnect in a loop.
    typed_on_link: bool,
    /// The last connect was denied or found no portal: text is dropped without trying again
    /// until [`Cmd::Retry`], so the dialog doesn't come back for every word.
    failed: bool,
    /// Since when the text at the front of the queue has been waiting without progress.
    waiting_since: Option<Instant>,
    /// `OutputEvent::Waiting(true)` was sent and not yet answered by `Waiting(false)`.
    waiting_announced: bool,
    /// The shortcut that pastes what the layout can't type.
    paste_keys: PasteKeys,
    /// The clipboard saved for pasting the text at the front of the queue, taken before the
    /// `sync` round trip that checks the user's modifiers (see [`paste`]).
    saved_clipboard: Option<Result<Offer, &'static str>>,
}

enum Wake {
    Cmd(Option<Cmd>),
    Eis(super::eis::RawEvent),
    SessionClosed,
    WaitingTooLong,
    GraceOver,
}

/// Runs the task. `eager`: connect right away, so the first word doesn't wait for session
/// setup (or the permission dialog); otherwise wait for [`Cmd::Retry`] or text.
pub(super) async fn run(
    source: EisSource,
    events: mpsc::UnboundedSender<OutputEvent>,
    rx: mpsc::UnboundedReceiver<Cmd>,
    eager: bool,
) {
    let mut actor = Actor {
        source,
        events,
        rx,
        shutdown_deadline: None,
        queue: VecDeque::new(),
        link: None,
        phase: Phase::Stale,
        typed_on_link: false,
        failed: false,
        waiting_since: None,
        waiting_announced: false,
        paste_keys: PasteKeys::default(),
        saved_clipboard: None,
    };
    if eager {
        actor.connect().await;
    }
    actor.run().await;
}

impl Actor {
    async fn run(&mut self) {
        loop {
            let progressed = self.pump().await;
            self.track_waiting(progressed);
            if self.shutdown_deadline.is_some() && self.queue.is_empty() {
                break;
            }
            if self.link.is_none() && self.has_text() {
                if self.shutdown_deadline.is_some() || self.failed {
                    // Shutting down, or failed until `retry`: don't open a new session (or
                    // dialog) for this text.
                    self.drop_pending();
                } else {
                    // Lazy reconnect: text is waiting.
                    self.connect().await;
                }
                continue;
            }
            let waiting_deadline = self.waiting_deadline();
            let wake = match (&mut self.link, self.shutdown_deadline) {
                (Some(Link { conn, session }), None) => tokio::select! {
                    cmd = self.rx.recv() => Wake::Cmd(cmd),
                    raw = conn.next_raw() => Wake::Eis(raw),
                    () = session.closed() => Wake::SessionClosed,
                    () = sleep_until(waiting_deadline) => Wake::WaitingTooLong,
                },
                (Some(link), Some(deadline)) => tokio::select! {
                    raw = link.conn.next_raw() => Wake::Eis(raw),
                    () = tokio::time::sleep_until(deadline) => Wake::GraceOver,
                },
                (None, None) => Wake::Cmd(self.rx.recv().await),
                // Unreachable: closed with an empty queue breaks above, and leftover text
                // without a link is dropped above. Flushes alone were resolved by `pump`.
                (None, Some(_)) => break,
            };
            match wake {
                Wake::Cmd(Some(Cmd::Type(text))) => self.queue.push_back(Queued::Text(text)),
                Wake::Cmd(Some(Cmd::Flush(done))) => self.queue.push_back(Queued::Flush(done)),
                Wake::Cmd(Some(Cmd::Retry)) => self.retry().await,
                Wake::Cmd(Some(Cmd::PasteKeys(keys))) => self.paste_keys = keys,
                Wake::Cmd(None) => {
                    self.shutdown_deadline = Some(Instant::now() + SHUTDOWN_GRACE);
                }
                Wake::Eis(raw) => self.on_eis(raw).await,
                Wake::SessionClosed => {
                    self.lose("the RemoteDesktop session was closed".into())
                        .await;
                }
                Wake::WaitingTooLong => {
                    debug!("text has been waiting for a while");
                    self.waiting_announced = true;
                    self.emit(OutputEvent::Waiting(true));
                }
                Wake::GraceOver => {
                    debug!("output dropped while text was still waiting; giving up");
                    self.drop_pending();
                }
            }
        }
        if self.waiting_announced {
            self.emit(OutputEvent::Waiting(false));
        }
        if let Some(link) = self.link.take() {
            link.conn.disconnect();
            link.session.close().await;
        }
        debug!("output task finished");
    }

    fn has_text(&self) -> bool {
        self.queue.iter().any(|c| matches!(c, Queued::Text(_)))
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
                        EisError::Handshake(e) => OutputEvent::Disconnected(error_chain(&e)),
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
                self.failed = false;
                self.emit(OutputEvent::Ready);
            }
            Err(event) => {
                warn!("typing unavailable: {event:?}");
                self.failed = matches!(
                    event,
                    OutputEvent::PermissionDenied | OutputEvent::Unavailable(_)
                );
                self.drop_pending();
                self.emit(event);
            }
        }
    }

    /// [`Cmd::Retry`]: leave the failed state and connect now if there is no connection.
    async fn retry(&mut self) {
        self.failed = false;
        if self.link.is_none() && self.shutdown_deadline.is_none() {
            self.connect().await;
        }
    }

    /// Drops everything queued so far, including what is still in the channel, so that a
    /// failed connection isn't retried once per queued word. Pending flushes resolve. A
    /// [`Cmd::Retry`] found in the channel ends the failed state, and what was sent after it is
    /// kept: it belongs to the new attempt (which then connects when that text is pumped).
    /// Settings ([`Cmd::PasteKeys`]) are applied, not dropped.
    fn drop_pending(&mut self) {
        self.saved_clipboard = None;
        let mut dropped = std::mem::take(&mut self.queue);
        let mut retried = false;
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                Cmd::Type(text) => self.queue.push_back(Queued::Text(text)),
                Cmd::Flush(done) => self.queue.push_back(Queued::Flush(done)),
                Cmd::Retry => {
                    retried = true;
                    dropped.append(&mut self.queue);
                }
                Cmd::PasteKeys(keys) => self.paste_keys = keys,
            }
        }
        if retried {
            self.failed = false;
        } else {
            dropped.append(&mut self.queue);
        }
        let words = dropped
            .iter()
            .filter(|c| matches!(c, Queued::Text(_)))
            .count();
        if words > 0 {
            debug!(words, "dropping queued text");
        }
        // Dropping the flush senders resolves those flushes.
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
                // Also while a `sync` is in flight: a new device's state must be learned
                // (probe tap) and synced again. The old sync's `done` no longer matches the
                // pending one and is ignored.
                self.phase = Phase::Stale;
                if link.conn.keyboard().is_some() {
                    debug!("EIS keyboard usable again");
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
        self.saved_clipboard = None; // It belonged to that session's clipboard.
        if !self.typed_on_link {
            // Lost before typing anything: don't retry for the queued text right away.
            self.drop_pending();
        }
        self.emit(OutputEvent::Disconnected(why));
    }

    /// Advances the queue as far as the current state allows. Returns whether any text was
    /// consumed (typed, or skipped as untypeable).
    async fn pump(&mut self) -> bool {
        let mut progressed = false;
        loop {
            match self.queue.front() {
                None => return progressed,
                Some(Queued::Flush(_)) => {
                    if let Some(Queued::Flush(done)) = self.queue.pop_front() {
                        let _ = done.send(());
                    }
                    continue;
                }
                Some(Queued::Text(_)) => {}
            }
            let Some(link) = self.link.as_mut() else {
                return progressed;
            };
            let Some((table, mods)) = link.conn.keyboard() else {
                return progressed; // Paused or being replaced; `Update::Readiness` resumes us.
            };
            match self.phase {
                Phase::Syncing | Phase::Held => return progressed,
                Phase::Stale => {
                    // Text that will be pasted: save the clipboard now, so that the modifier
                    // check below is fresh when the paste shortcut is pressed.
                    if self.saved_clipboard.is_none()
                        && let Some(Queued::Text(text)) = self.queue.front()
                        && table.plan(text, &mods).unmapped > 0
                        && let Some(clipboard) = link.session.clipboard()
                    {
                        self.saved_clipboard = Some(paste::save(&*clipboard).await);
                    }
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
                            return progressed;
                        }
                    }
                    continue;
                }
                Phase::Fresh => {}
            }
            // Checked right after the round trip, while murmur holds no keys itself.
            if table.blocks_typing(&mods) {
                debug!(
                    depressed = mods.depressed,
                    latched = mods.latched,
                    locked = mods.locked,
                    "modifier active: holding text until it is released"
                );
                self.phase = Phase::Held;
                // The user may copy something while holding Ctrl: save it again later.
                self.saved_clipboard = None;
                return progressed;
            }
            let Some(Queued::Text(text)) = self.queue.pop_front() else {
                return progressed; // Unreachable: the front was checked to be text above.
            };
            progressed = true;
            // Whatever happens below, the next text gets a fresh round trip first.
            self.phase = Phase::Stale;
            let saved_clipboard = self.saved_clipboard.take();
            let plan = table.plan(&text, &mods);
            trace!(chars = text.chars().count(), group = mods.group, "typing");
            if plan.control > 0 {
                debug!(count = plan.control, "skipped control characters");
            }
            // Characters the layout lacks: paste the whole string instead, if the session has
            // the clipboard (D-011; see `paste` for why not just the missing characters).
            let paste_keys = (plan.unmapped > 0)
                .then(|| table.paste_keys(&mods, self.paste_keys))
                .flatten();
            if let (Some(keys), Some(clipboard)) = (paste_keys, link.session.clipboard()) {
                // Control characters are never pasted either.
                let text: String = text.chars().filter(|c| !c.is_control()).collect();
                let previous = match saved_clipboard {
                    Some(saved) => saved,
                    // Not saved before the round trip (the layout changed during it).
                    None => paste::save(&*clipboard).await,
                };
                let conn = &mut link.conn;
                let keys = &keys;
                match paste::paste(&*clipboard, previous, &text, move || conn.burst(keys)).await {
                    Ok(Outcome::Pasted) => {
                        trace!(chars = text.chars().count(), "pasted");
                        self.typed_on_link = true;
                        continue;
                    }
                    Ok(Outcome::Skipped(why)) => debug!(why, "not pasted; typing what we can"),
                    Err(e) => {
                        self.lose(format!("EIS write error: {e}")).await;
                        return progressed;
                    }
                }
            }
            if plan.unmapped > 0 {
                debug!(
                    count = plan.unmapped,
                    group = mods.group,
                    "skipped characters missing from the active layout"
                );
                // Field access, not `emit`: `link` still borrows `self.link`.
                let _ = self.events.send(OutputEvent::Unmapped {
                    count: plan.unmapped,
                });
            }
            if plan.events.is_empty() {
                continue; // Nothing typeable.
            }
            match link.conn.burst(&plan.events).await {
                Ok(()) => self.typed_on_link = true,
                Err(e) => {
                    self.lose(format!("EIS write error: {e}")).await;
                    return progressed;
                }
            }
        }
    }

    /// Starts or stops the "text is waiting" clock after a `pump`. Text waits when it sits at
    /// the front of the queue on a live connection without being typed (held modifier, paused
    /// keyboard, unanswered `sync`).
    fn track_waiting(&mut self, progressed: bool) {
        let stuck = self.link.is_some() && matches!(self.queue.front(), Some(Queued::Text(_)));
        if (progressed || !stuck) && self.waiting_announced {
            self.waiting_announced = false;
            self.emit(OutputEvent::Waiting(false));
        }
        self.waiting_since = match (stuck, self.waiting_since) {
            (false, _) => None,
            (true, Some(since)) if !progressed => Some(since),
            (true, _) => Some(Instant::now()),
        };
    }

    /// When to announce `Waiting(true)`, if text waits and it hasn't been announced yet.
    fn waiting_deadline(&self) -> Option<Instant> {
        if self.waiting_announced {
            return None;
        }
        self.waiting_since.map(|since| since + WAITING_AFTER)
    }
}

/// Sleeps until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// `error: source: source…`, for messages that cross into [`OutputEvent`].
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::sync::mpsc;

    use super::*;
    use crate::output::{
        Output,
        clipboard::{FakeClipboard, Offer},
        test_eis::{Connections, Next, Server, Setup},
    };

    // Evdev keycodes (linux/input-event-codes.h).
    const KEY_LEFTCTRL: u32 = 29;
    const KEY_RIGHTALT: u32 = 100;

    const WAIT: Duration = Duration::from_secs(5);

    fn setup(layouts: &'static str, group: u32) -> Setup {
        Setup {
            layouts,
            group,
            held: &[],
            replace_before_sync: None,
            clipboard: None,
        }
    }

    fn spawn(conns: &Connections) -> (Output, mpsc::UnboundedReceiver<OutputEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Output::spawn(EisSource::Test(conns.clone()), tx), rx)
    }

    /// Next event that isn't `Ready`.
    async fn next_event(events: &mut mpsc::UnboundedReceiver<OutputEvent>) -> OutputEvent {
        let next = tokio::time::timeout(WAIT, async {
            loop {
                match events.recv().await {
                    Some(OutputEvent::Ready) => {}
                    Some(e) => return e,
                    None => panic!("output event channel closed"),
                }
            }
        })
        .await;
        next.expect("an output event")
    }

    async fn expect_ready(events: &mut mpsc::UnboundedReceiver<OutputEvent>) {
        let got = tokio::time::timeout(WAIT, events.recv()).await;
        assert_eq!(got.expect("an output event"), Some(OutputEvent::Ready));
    }

    async fn type_words(out: &Output, words: &[&str]) {
        for w in words {
            out.type_text((*w).to_owned());
        }
        tokio::time::timeout(WAIT, out.flush())
            .await
            .expect("flush");
    }

    #[tokio::test]
    async fn types_through_the_keymap_in_the_active_group() {
        let conns = Connections::default();
        // Lithuanian active before murmur connects: digits need AltGr there.
        let (mut server, next) = Server::start(setup("us,lt", 1));
        conns.push(next);
        let (out, mut events) = spawn(&conns);
        expect_ready(&mut events).await;
        type_words(&out, &["Ačiū ", "1 "]).await;
        server.expect_typed("Ačiū 1 ").await;
        while let Ok(e) = events.try_recv() {
            assert!(matches!(e, OutputEvent::Waiting(_)), "unexpected {e:?}");
        }
    }

    #[tokio::test]
    async fn a_device_replaced_during_a_sync_is_learned_before_typing() {
        let conns = Connections::default();
        // Sync 1 follows the probe tap on the first device; sync 2 precedes "1 ".
        let (mut server, next) = Server::start(Setup {
            replace_before_sync: Some(2),
            ..setup("us,lt", 1)
        });
        conns.push(next);
        let (out, mut events) = spawn(&conns);
        expect_ready(&mut events).await;
        type_words(&out, &["a "]).await;
        type_words(&out, &["1 "]).await;
        // Typed with the first device's group 0 assumption, "1" would come out as "ą".
        server.expect_typed("a 1 ").await;
    }

    #[tokio::test]
    async fn altgr_held_before_the_first_word_holds_text_back() {
        let conns = Connections::default();
        // The user holds AltGr while murmur connects: KWin reports nothing, and the probe tap
        // (another Level3 key) changes nothing either.
        let (mut server, next) = Server::start(Setup {
            held: &[KEY_RIGHTALT],
            ..setup("de", 0)
        });
        conns.push(next);
        let (out, mut events) = spawn(&conns);
        expect_ready(&mut events).await;
        out.type_text("a ".into());
        assert_eq!(next_event(&mut events).await, OutputEvent::Waiting(true));
        assert_eq!(*server.typed.borrow(), "", "typed with AltGr held (\"æ \")");
        server.key(KEY_RIGHTALT, false);
        server.expect_typed("a ").await;
        assert_eq!(next_event(&mut events).await, OutputEvent::Waiting(false));
    }

    #[tokio::test]
    async fn held_ctrl_holds_text_and_a_dropped_flush_loses_nothing() {
        let conns = Connections::default();
        let (mut server, next) = Server::start(Setup {
            held: &[KEY_LEFTCTRL],
            ..setup("us", 0)
        });
        conns.push(next);
        let (out, mut events) = spawn(&conns);
        expect_ready(&mut events).await;
        out.type_text("held ".into());
        let flushed = tokio::time::timeout(Duration::from_millis(100), out.flush()).await;
        assert!(flushed.is_err(), "flush resolved while Ctrl was held");
        assert_eq!(next_event(&mut events).await, OutputEvent::Waiting(true));
        assert_eq!(*server.typed.borrow(), "");
        server.key(KEY_LEFTCTRL, false);
        server.expect_typed("held ").await;
        assert_eq!(next_event(&mut events).await, OutputEvent::Waiting(false));
        type_words(&out, &["back "]).await;
        server.expect_typed("held back ").await;
    }

    #[tokio::test]
    async fn a_failed_connect_stays_failed_until_retry() {
        for failure in [Next::Deny, Next::Unavailable] {
            let conns = Connections::default();
            conns.push(failure);
            let (mut server, next) = Server::start(setup("us", 0));
            conns.push(next);
            let (out, mut events) = spawn(&conns);
            let failed = next_event(&mut events).await;
            assert!(
                matches!(
                    failed,
                    OutputEvent::PermissionDenied | OutputEvent::Unavailable(_)
                ),
                "{failed:?}"
            );
            // Text is dropped without asking again (no dialog per word).
            type_words(&out, &["dropped ", "too "]).await;
            assert_eq!(conns.left(), 1, "connected again for text");
            assert!(events.try_recv().is_err(), "no events for dropped text");
            out.retry();
            expect_ready(&mut events).await;
            type_words(&out, &["typed "]).await;
            server.expect_typed("typed ").await;
        }
    }

    #[tokio::test]
    async fn text_sent_after_a_retry_survives_the_failure_before_it() {
        let conns = Connections::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let (events, _events_rx) = mpsc::unbounded_channel();
        let mut actor = Actor {
            source: EisSource::Test(conns),
            events,
            rx,
            shutdown_deadline: None,
            queue: VecDeque::from([Queued::Text("old ".into())]),
            link: None,
            phase: Phase::Stale,
            typed_on_link: false,
            failed: true,
            waiting_since: None,
            waiting_announced: false,
            paste_keys: PasteKeys::default(),
            saved_clipboard: None,
        };
        for cmd in [
            Cmd::Type("before ".into()),
            Cmd::Retry,
            Cmd::Type("after ".into()),
        ] {
            assert!(tx.send(cmd).is_ok());
        }
        actor.drop_pending();
        assert!(!actor.failed);
        let kept: Vec<_> = actor
            .queue
            .iter()
            .map(|q| match q {
                Queued::Text(t) => t.as_str(),
                Queued::Flush(_) => "<flush>",
            })
            .collect();
        assert_eq!(kept, ["after "]);
    }

    #[tokio::test]
    async fn a_closed_portal_session_disconnects() {
        let conns = Connections::default();
        let (mut server, next) = Server::start(setup("us", 0));
        conns.push(next);
        let (_out, mut events) = spawn(&conns);
        expect_ready(&mut events).await;
        server.close_session();
        assert!(matches!(
            next_event(&mut events).await,
            OutputEvent::Disconnected(_)
        ));
    }

    #[tokio::test]
    async fn characters_off_the_layout_are_pasted_and_the_clipboard_restored() {
        let previous = Offer::text("the user's copy");
        let clipboard = FakeClipboard::new(previous.clone());
        let conns = Connections::default();
        let (mut server, next) = Server::start(Setup {
            clipboard: Some(Arc::clone(&clipboard)),
            ..setup("us", 0)
        });
        conns.push(next);
        let (out, mut events) = spawn(&conns);
        expect_ready(&mut events).await;
        // `ą` isn't on us: the whole word is pasted, the next one typed.
        type_words(&out, &["mą\tstau ", "Ok "]).await;
        server.expect_typed("mąstau Ok ").await;
        assert_eq!(clipboard.offer(), previous, "previous clipboard restored");
        assert_eq!(
            clipboard.sets().first(),
            Some(&Offer::text("mąstau ")),
            "control characters are never pasted"
        );
        while let Ok(e) = events.try_recv() {
            assert!(matches!(e, OutputEvent::Waiting(_)), "unexpected {e:?}");
        }
    }

    #[tokio::test]
    async fn the_paste_shortcut_can_be_ctrl_shift_v() {
        let clipboard = FakeClipboard::new(Offer::default());
        let conns = Connections::default();
        let (mut server, next) = Server::start(Setup {
            clipboard: Some(Arc::clone(&clipboard)),
            ..setup("us", 0)
        });
        conns.push(next);
        let (out, mut events) = spawn(&conns);
        expect_ready(&mut events).await;
        out.set_paste_keys(crate::output::PasteKeys::CtrlShiftV);
        type_words(&out, &["ž "]).await;
        server.expect_typed("ž ").await;
        assert!(clipboard.offer().is_empty(), "empty clipboard restored");
    }

    #[tokio::test]
    async fn without_clipboard_access_off_layout_characters_are_skipped() {
        let conns = Connections::default();
        let (mut server, next) = Server::start(setup("us", 0));
        conns.push(next);
        let (out, mut events) = spawn(&conns);
        expect_ready(&mut events).await;
        type_words(&out, &["mąstau "]).await;
        server.expect_typed("mstau ").await;
        assert_eq!(
            next_event(&mut events).await,
            OutputEvent::Unmapped { count: 1 }
        );
    }

    #[tokio::test]
    async fn an_idle_output_connects_only_when_asked() {
        let conns = Connections::default();
        let (mut server, next) = Server::start(setup("us", 0));
        conns.push(next);
        let (tx, mut events) = mpsc::unbounded_channel();
        let out = Output::spawn_idle(EisSource::Test(conns.clone()), tx);
        let early = tokio::time::timeout(Duration::from_millis(200), events.recv()).await;
        assert!(early.is_err(), "connected without being asked: {early:?}");
        assert_eq!(conns.left(), 1);
        out.retry();
        expect_ready(&mut events).await;
        type_words(&out, &["hi "]).await;
        server.expect_typed("hi ").await;

        // Text alone also connects.
        let conns = Connections::default();
        let (mut server, next) = Server::start(setup("us", 0));
        conns.push(next);
        let (tx, mut events) = mpsc::unbounded_channel();
        let out = Output::spawn_idle(EisSource::Test(conns.clone()), tx);
        type_words(&out, &["hello "]).await;
        expect_ready(&mut events).await;
        server.expect_typed("hello ").await;
    }
}
