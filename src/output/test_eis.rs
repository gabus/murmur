//! Unit-test stand-ins for the compositor: [`Connections`] hands EIS sockets to
//! [`EisSource::Test`](super::EisSource::Test), and [`Server`] is a fake EIS server on the
//! other end of one.
//!
//! The server behaves like KWin as far as murmur is concerned (docs/portal-findings.md): one
//! seat with a keyboard device carrying a real XKB keymap, `ei_keyboard.modifiers` sent only
//! when the seat's state changes (never initially), and `sync` answered after everything sent
//! before it. Its own `xkb::State` stands in for the seat: it applies murmur's keys and the
//! "physical" keys a test presses, and records the text a focused client would see.

use std::{
    collections::VecDeque,
    io::Write,
    os::fd::{AsFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use reis::{PendingRequestResult, eis, handshake::EisHandshaker};
use tokio::{
    io::Interest,
    sync::{mpsc, oneshot, watch},
};
use xkbcommon::xkb;

use super::source::{SessionHandle, SourceError};

/// What the next connect attempt gets.
pub(crate) enum Next {
    Eis {
        fd: OwnedFd,
        closed: oneshot::Receiver<()>,
    },
    Deny,
    Unavailable,
}

/// Connections for [`EisSource::Test`](super::EisSource::Test), handed out in order. When none
/// are left, connecting fails as unavailable.
#[derive(Clone, Default)]
pub struct Connections(Arc<Mutex<VecDeque<Next>>>);

impl Connections {
    pub(crate) fn push(&self, next: Next) {
        self.lock().push_back(next);
    }

    /// Connections not yet taken.
    pub(crate) fn left(&self) -> usize {
        self.lock().len()
    }

    pub(super) fn open(&self) -> Result<(OwnedFd, SessionHandle), SourceError> {
        match self.lock().pop_front() {
            Some(Next::Eis { fd, closed }) => Ok((
                fd,
                SessionHandle::Test {
                    closed: Some(closed),
                },
            )),
            Some(Next::Deny) => Err(SourceError::PermissionDenied),
            Some(Next::Unavailable) | None => {
                Err(SourceError::Unavailable("test: no connection".into()))
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Next>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Connections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Connections")
    }
}

impl PartialEq for Connections {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Connections {}

/// How the fake compositor starts out.
pub(crate) struct Setup {
    /// RMLVO layout list, e.g. `"us,lt"`.
    pub layouts: &'static str,
    /// Active layout group.
    pub group: u32,
    /// Evdev keys the user already holds when murmur connects.
    pub held: &'static [u32],
    /// Replace the keyboard device (new device first, then the old one removed) right before
    /// answering this `sync` (1-based), like KWin does on a keymap change.
    pub replace_before_sync: Option<u32>,
}

enum Control {
    /// A physical key press (`true`) or release.
    Key(u32, bool),
}

/// A running fake EIS server.
pub(crate) struct Server {
    /// What a focused client would have received as text.
    pub typed: watch::Receiver<String>,
    control: mpsc::UnboundedSender<Control>,
    close: Option<oneshot::Sender<()>>,
}

impl Server {
    /// Starts the server on its own thread and returns the client end for [`Connections`].
    pub(crate) fn start(setup: Setup) -> (Self, Next) {
        let (client, server) = UnixStream::pair().expect("socketpair");
        let (typed_tx, typed) = watch::channel(String::new());
        let (control, control_rx) = mpsc::unbounded_channel();
        let (close, closed) = oneshot::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            rt.block_on(serve(server, setup, typed_tx, control_rx));
        });
        let next = Next::Eis {
            fd: client.into(),
            closed,
        };
        (
            Self {
                typed,
                control,
                close: Some(close),
            },
            next,
        )
    }

    /// The user presses (`true`) or releases a key on their physical keyboard.
    pub(crate) fn key(&self, key: u32, pressed: bool) {
        let _ = self.control.send(Control::Key(key, pressed));
    }

    /// The portal closes the session (e.g. the user stopped remote control).
    pub(crate) fn close_session(&mut self) {
        if let Some(close) = self.close.take() {
            let _ = close.send(());
        }
    }

    /// Waits until the client has typed exactly `want`.
    pub(crate) async fn expect_typed(&mut self, want: &str) {
        let waited = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.typed.wait_for(|t| t == want),
        )
        .await
        .is_ok();
        assert!(waited, "typed {:?}, want {want:?}", *self.typed.borrow());
    }
}

/// XKB keymap text in a file, as the compositor shares it.
fn keymap_file(keymap: &xkb::Keymap) -> (std::fs::File, u32) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut text = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    text.push(0);
    let path = std::env::temp_dir().join(format!(
        "murmur-test-keymap-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::File::create(&path)
        .and_then(|mut f| f.write_all(&text))
        .expect("write keymap");
    let file = std::fs::File::open(&path).expect("open keymap");
    let _ = std::fs::remove_file(&path);
    (file, u32::try_from(text.len()).expect("keymap size"))
}

struct Device {
    device: eis::Device,
    keyboard: eis::Keyboard,
}

struct Seat {
    ctx: eis::Context,
    seat: eis::Seat,
    keymap: (std::fs::File, u32),
    devices: Vec<Device>,
    serial: u32,
    state: xkb::State,
    /// Last reported (depressed, locked, latched, group).
    reported: (u32, u32, u32, u32),
    typed: watch::Sender<String>,
}

impl Seat {
    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    fn add_device(&mut self) {
        let serial = self.next_serial();
        let device = self.seat.device(1);
        device.name("test keyboard");
        device.device_type(eis::device::DeviceType::Virtual);
        let keyboard: eis::Keyboard = device.interface(1);
        keyboard.keymap(
            eis::keyboard::KeymapType::Xkb,
            self.keymap.1,
            self.keymap.0.as_fd(),
        );
        device.done();
        device.resumed(serial);
        self.devices.push(Device { device, keyboard });
    }

    fn remove_oldest_device(&mut self) {
        if self.devices.is_empty() {
            return;
        }
        let old = self.devices.remove(0);
        let serial = self.next_serial();
        old.keyboard.destroyed(serial);
        let serial = self.next_serial();
        old.device.destroyed(serial);
    }

    /// Applies a key from any device; `typing` keys also produce text in the focused client.
    fn key(&mut self, key: u32, pressed: bool, typing: bool) {
        let code = xkb::Keycode::new(key + 8);
        if pressed {
            if typing {
                let text = self.state.key_get_utf8(code);
                self.typed.send_modify(|t| t.push_str(&text));
            }
            let _ = self.state.update_key(code, xkb::KeyDirection::Down);
        } else {
            let _ = self.state.update_key(code, xkb::KeyDirection::Up);
        }
        let now = self.mods();
        if now != self.reported {
            self.reported = now;
            let serial = self.next_serial();
            for d in &self.devices {
                d.keyboard.modifiers(serial, now.0, now.1, now.2, now.3);
            }
        }
    }

    fn mods(&self) -> (u32, u32, u32, u32) {
        (
            self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.state.serialize_mods(xkb::STATE_MODS_LOCKED),
            self.state.serialize_mods(xkb::STATE_MODS_LATCHED),
            self.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        )
    }
}

/// Next request from the client, or `None` when it is gone.
async fn next_request(
    ctx: &eis::Context,
    readiness: &tokio::net::UnixStream,
) -> Option<eis::Request> {
    loop {
        match ctx.pending_request() {
            Some(PendingRequestResult::Request(request)) => return Some(request),
            Some(_) => return None, // Protocol error: hang up.
            None => {}
        }
        readiness.readable().await.ok()?;
        let read = readiness.try_io(Interest::READABLE, || match ctx.read() {
            Ok(0) => Err(std::io::ErrorKind::WouldBlock.into()),
            other => other,
        });
        match read {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return None,
        }
    }
}

async fn serve(
    socket: UnixStream,
    setup: Setup,
    typed: watch::Sender<String>,
    mut control: mpsc::UnboundedReceiver<Control>,
) {
    let dup = socket.try_clone().expect("dup socket");
    let ctx = eis::Context::new(socket).expect("EIS context");
    let readiness = tokio::net::UnixStream::from_std(dup).expect("register socket");
    let mut handshaker = EisHandshaker::new(&ctx, 1);
    let connection = loop {
        let Some(request) = next_request(&ctx, &readiness).await else {
            return;
        };
        match handshaker.handle_request(request) {
            Ok(Some(resp)) => break resp.connection,
            Ok(None) => {}
            Err(_) => return,
        }
    };
    let xkb_ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &xkb_ctx,
        "evdev",
        "pc105",
        setup.layouts,
        "",
        None,
        xkb::COMPILE_NO_FLAGS,
    )
    .expect("test keymap compiles");
    let mut state = xkb::State::new(&keymap);
    state.update_mask(0, 0, 0, 0, 0, setup.group);
    for &key in setup.held {
        let _ = state.update_key(xkb::Keycode::new(key + 8), xkb::KeyDirection::Down);
    }
    let seat = connection.seat(1);
    seat.name("seat0");
    seat.capability(0x20, "ei_keyboard");
    seat.done();
    let mut s = Seat {
        keymap: keymap_file(&keymap),
        ctx,
        seat,
        devices: Vec::new(),
        serial: 10,
        state,
        reported: (0, 0, 0, 0),
        typed,
    };
    // Nothing is reported until the state changes, like KWin.
    s.reported = s.mods();
    let _ = s.ctx.flush();
    let mut syncs = 0;
    loop {
        tokio::select! {
            request = next_request(&s.ctx, &readiness) => {
                let Some(request) = request else {
                    return;
                };
                match request {
                    eis::Request::Seat(_, eis::seat::Request::Bind { .. }) => s.add_device(),
                    eis::Request::Keyboard(_, eis::keyboard::Request::Key { key, state }) => {
                        s.key(key, matches!(state, eis::keyboard::KeyState::Press), true);
                    }
                    eis::Request::Connection(_, eis::connection::Request::Sync { callback }) => {
                        syncs += 1;
                        if setup.replace_before_sync == Some(syncs) {
                            s.add_device();
                            s.remove_oldest_device();
                        }
                        callback.done(0);
                    }
                    _ => {}
                }
            }
            Some(Control::Key(key, pressed)) = control.recv() => s.key(key, pressed, false),
        }
        let _ = s.ctx.flush();
    }
}
