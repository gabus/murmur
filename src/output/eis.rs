//! One EI (libei protocol) sender connection: handshake, seat and device tracking, keymap,
//! modifier state, `sync` round trips and keyboard bursts.
//!
//! This works on raw `reis::ei::Event`s instead of reis' `EiEventConverter`: the converter is
//! not `Send` and swallows `ei_callback.done`, which murmur needs to know when a `sync` round
//! trip has completed.

use std::{collections::HashMap, io, os::fd::OwnedFd, os::unix::net::UnixStream, time::Duration};

use reis::{Interface, PendingRequestResult, ei};
use tracing::{debug, trace, warn};

pub(crate) use super::reader::RawEvent;
use super::{
    keymap::{KeyEvent, KeyTable, ModState},
    reader::EiSocket,
};

/// Name murmur announces in the EI handshake.
const CLIENT_NAME: &str = "murmur";
/// How long the handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for a usable keyboard after the handshake (normally a few ms).
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a connection could not be set up or was lost.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EisError {
    /// The compositor offers no EIS keyboard (or the keymap is unusable).
    #[error("{0}")]
    Unsupported(String),
    /// I/O, protocol or timeout problem. Worth retrying later.
    #[error("{0}")]
    Lost(String),
}

/// What a handled event changed, as far as typing is concerned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Update {
    /// Nothing relevant.
    None,
    /// The pending `sync` round trip completed.
    Synced,
    /// The keyboard's modifier or layout state changed.
    Modifiers,
    /// The keyboard became usable, or stopped being usable.
    Readiness,
    /// The connection is gone.
    Lost(String),
}

#[derive(Default)]
struct Seat {
    /// Interface name → capability mask, collected until `ei_seat.done`.
    caps: HashMap<String, u64>,
}

#[derive(Default)]
struct DeviceState {
    name: Option<String>,
    keyboard: Option<ei::Keyboard>,
    keymap: Option<(OwnedFd, u32)>,
    table: Option<KeyTable>,
    done: bool,
    resumed: bool,
    mods: ModState,
    /// A `modifiers` event arrived for this device, so `mods` is real, not a default.
    mods_seen: bool,
    /// The probe key was tapped to learn `mods` (at most once per device).
    probed: bool,
}

impl DeviceState {
    fn usable(&self) -> bool {
        self.done && self.resumed && self.keyboard.is_some() && self.table.is_some()
    }
}

/// A connected EI sender context.
pub(crate) struct EisConnection {
    socket: EiSocket,
    connection: ei::Connection,
    /// Negotiated `ei_callback` version, if the server supports `sync`.
    callback_version: Option<u32>,
    /// Last serial the server used in any event.
    last_serial: u32,
    /// `start_emulating` sequence number, increasing per burst.
    sequence: u32,
    seats: HashMap<ei::Seat, Seat>,
    devices: HashMap<ei::Device, DeviceState>,
    /// The `sync` whose `done` the typing logic is waiting for.
    pending_sync: Option<ei::Callback>,
    /// Whether any seat offered `ei_keyboard`, once a seat has been announced completely.
    keyboard_offered: Option<bool>,
    /// Last keymap problem, reported if setup times out.
    keymap_error: Option<String>,
}

impl EisConnection {
    /// Performs the EI handshake on `fd` and waits until a keyboard device is resumed and its
    /// keymap compiled.
    pub(crate) async fn establish(fd: OwnedFd) -> Result<Self, EisError> {
        let socket = EiSocket::new(UnixStream::from(fd))
            .map_err(|e| EisError::Lost(format!("EIS socket: {e}")))?;
        let resp = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.handshake(CLIENT_NAME))
            .await
            .map_err(|_| EisError::Lost("EI handshake timed out".into()))?
            .map_err(EisError::Lost)?;
        debug!(interfaces = ?resp.negotiated_interfaces, "EI handshake done");
        if !resp.negotiated_interfaces.contains_key(ei::Keyboard::NAME) {
            return Err(EisError::Unsupported(
                "the compositor's EIS has no keyboard interface".into(),
            ));
        }
        let mut conn = Self {
            socket,
            connection: resp.connection,
            callback_version: resp.negotiated_interfaces.get(ei::Callback::NAME).copied(),
            last_serial: resp.serial,
            sequence: 0,
            seats: HashMap::new(),
            devices: HashMap::new(),
            pending_sync: None,
            keyboard_offered: None,
            keymap_error: None,
        };
        conn.wait_until_usable().await?;
        Ok(conn)
    }

    async fn wait_until_usable(&mut self) -> Result<(), EisError> {
        let deadline = tokio::time::sleep(SETUP_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            let raw = tokio::select! {
                raw = self.socket.next() => raw,
                _ = &mut deadline => {
                    if self.keyboard_offered == Some(false) {
                        return Err(EisError::Unsupported(
                            "the compositor's EIS seat offers no keyboard".into(),
                        ));
                    }
                    if let Some(why) = self.keymap_error.take() {
                        return Err(EisError::Unsupported(why));
                    }
                    return Err(EisError::Lost(
                        "timed out waiting for the EIS keyboard device".into(),
                    ));
                }
            };
            if let Update::Lost(why) = self.handle(raw).await {
                return Err(EisError::Lost(why));
            }
            if self.usable().is_some() {
                return Ok(());
            }
        }
    }

    /// Next raw event. Cancel-safe: dropping the future loses nothing.
    pub(crate) async fn next_raw(&mut self) -> RawEvent {
        self.socket.next().await
    }

    /// Applies one raw event to the connection state.
    ///
    /// Not cancel-safe (it may wait for a keymap compile): call it outside `select!`.
    pub(crate) async fn handle(&mut self, raw: RawEvent) -> Update {
        let event = match raw {
            None => return Update::Lost("EIS connection closed".into()),
            Some(Err(e)) => return Update::Lost(format!("EIS read error: {e}")),
            Some(Ok(PendingRequestResult::ParseError(e))) => {
                return Update::Lost(format!("EIS protocol error: {e}"));
            }
            Some(Ok(PendingRequestResult::InvalidObject(id))) => {
                trace!(id, "EI event for an unknown object");
                return Update::None;
            }
            Some(Ok(PendingRequestResult::Request(event))) => event,
        };
        let was_usable = self.usable().is_some();
        let update = self.apply(event).await;
        if update == Update::None && was_usable != self.usable().is_some() {
            return Update::Readiness;
        }
        update
    }

    async fn apply(&mut self, event: ei::Event) -> Update {
        match event {
            ei::Event::Connection(_, ev) => match ev {
                ei::connection::Event::Seat { seat } => {
                    self.seats.insert(seat, Seat::default());
                }
                ei::connection::Event::Ping { ping } => {
                    // `done` also forgets the object.
                    ping.done(0);
                    self.socket.try_flush();
                }
                ei::connection::Event::Disconnected {
                    reason,
                    explanation,
                    ..
                } => {
                    let why = explanation.unwrap_or_default();
                    return Update::Lost(format!("EIS disconnected ({reason:?}) {why}"));
                }
                ei::connection::Event::InvalidObject { invalid_id, .. } => {
                    debug!(invalid_id, "server reported an invalid object");
                }
                _ => {}
            },
            ei::Event::Callback(callback, ei::callback::Event::Done { .. }) => {
                // The server destroys callbacks after `done`; drop our bookkeeping too.
                forget(&callback);
                if self.pending_sync.as_ref() == Some(&callback) {
                    self.pending_sync = None;
                    return Update::Synced;
                }
            }
            ei::Event::Seat(seat, ev) => self.seat_event(seat, ev),
            ei::Event::Device(device, ev) => return self.device_event(device, ev).await,
            ei::Event::Keyboard(keyboard, ev) => return self.keyboard_event(&keyboard, ev),
            other => trace!(?other, "ignored EI event"),
        }
        Update::None
    }

    fn seat_event(&mut self, seat: ei::Seat, ev: ei::seat::Event) {
        match ev {
            ei::seat::Event::Name { name } => debug!(name, "EIS seat"),
            ei::seat::Event::Capability { mask, interface } => {
                if let Some(s) = self.seats.get_mut(&seat) {
                    s.caps.insert(interface, mask);
                }
            }
            ei::seat::Event::Done => {
                let mask = self
                    .seats
                    .get(&seat)
                    .and_then(|s| s.caps.get(ei::Keyboard::NAME).copied());
                match mask {
                    Some(mask) => {
                        seat.bind(mask);
                        self.socket.try_flush();
                        self.keyboard_offered = Some(true);
                    }
                    None => {
                        debug!("EIS seat without keyboard capability");
                        self.keyboard_offered.get_or_insert(false);
                    }
                }
            }
            ei::seat::Event::Device { device } => {
                self.devices.insert(device, DeviceState::default());
            }
            ei::seat::Event::Destroyed { serial } => {
                self.last_serial = serial;
                self.seats.remove(&seat);
            }
            _ => {}
        }
    }

    async fn device_event(&mut self, device: ei::Device, ev: ei::device::Event) -> Update {
        match ev {
            ei::device::Event::Name { name } => {
                if let Some(d) = self.devices.get_mut(&device) {
                    d.name = Some(name);
                }
            }
            ei::device::Event::Interface { object } => {
                if let Some(d) = self.devices.get_mut(&device)
                    && let Some(kb) = object.downcast::<ei::Keyboard>()
                {
                    d.keyboard = Some(kb);
                }
            }
            ei::device::Event::Done => {
                let Some(d) = self.devices.get_mut(&device) else {
                    return Update::None;
                };
                d.done = true;
                if let Some((fd, size)) = d.keymap.take() {
                    let name = d.name.clone().unwrap_or_default();
                    let compiled =
                        tokio::task::spawn_blocking(move || KeyTable::from_fd(fd, size as usize))
                            .await;
                    match compiled {
                        Ok(Ok(table)) => {
                            debug!(device = name, layouts = ?table.layouts(), "keymap compiled");
                            if let Some(d) = self.devices.get_mut(&device) {
                                d.table = Some(table);
                            }
                        }
                        Ok(Err(e)) => {
                            warn!(device = name, "unusable keymap: {e}");
                            self.keymap_error = Some(format!("unusable keymap: {e}"));
                        }
                        Err(e) => {
                            warn!(device = name, "keymap compile task failed: {e}");
                            self.keymap_error = Some(format!("keymap compile failed: {e}"));
                        }
                    }
                }
            }
            ei::device::Event::Resumed { serial } => {
                self.last_serial = serial;
                if let Some(d) = self.devices.get_mut(&device) {
                    debug!(device = d.name.as_deref().unwrap_or("?"), "device resumed");
                    d.resumed = true;
                }
            }
            ei::device::Event::Paused { serial } => {
                self.last_serial = serial;
                if let Some(d) = self.devices.get_mut(&device) {
                    debug!(device = d.name.as_deref().unwrap_or("?"), "device paused");
                    d.resumed = false;
                }
            }
            ei::device::Event::Destroyed { serial } => {
                self.last_serial = serial;
                if let Some(d) = self.devices.remove(&device) {
                    debug!(device = d.name.as_deref().unwrap_or("?"), "device removed");
                }
            }
            ei::device::Event::StartEmulating { serial, .. }
            | ei::device::Event::StopEmulating { serial }
            | ei::device::Event::Frame { serial, .. } => self.last_serial = serial,
            _ => {}
        }
        Update::None
    }

    fn keyboard_event(&mut self, keyboard: &ei::Keyboard, ev: ei::keyboard::Event) -> Update {
        let device = self
            .devices
            .values_mut()
            .find(|d| d.keyboard.as_ref() == Some(keyboard));
        match ev {
            ei::keyboard::Event::Keymap {
                keymap_type,
                size,
                keymap,
            } => match (device, keymap_type) {
                (Some(d), ei::keyboard::KeymapType::Xkb) => d.keymap = Some((keymap, size)),
                (_, other) => debug!(?other, "ignored keymap"),
            },
            ei::keyboard::Event::Modifiers {
                serial,
                depressed,
                locked,
                latched,
                group,
            } => {
                self.last_serial = serial;
                if let Some(d) = device {
                    d.mods_seen = true;
                    d.mods = ModState {
                        depressed,
                        latched,
                        locked,
                        group,
                    };
                    trace!(depressed, locked, latched, group, "modifiers");
                    return Update::Modifiers;
                }
            }
            ei::keyboard::Event::Destroyed { serial } => {
                self.last_serial = serial;
                if let Some(d) = device {
                    d.keyboard = None;
                }
            }
            _ => {}
        }
        Update::None
    }

    fn usable(&self) -> Option<&DeviceState> {
        self.devices.values().find(|d| d.usable())
    }

    /// The usable keyboard's keymap table and modifier state, if there is one.
    pub(crate) fn keyboard(&self) -> Option<(&KeyTable, ModState)> {
        let d = self.usable()?;
        Some((d.table.as_ref()?, d.mods))
    }

    /// Starts a `sync` round trip; [`Update::Synced`] reports its completion. Returns `false`
    /// if the server doesn't support `sync` (then there is nothing to wait for).
    pub(crate) async fn start_sync(&mut self) -> io::Result<bool> {
        let Some(version) = self.callback_version else {
            return Ok(false);
        };
        self.pending_sync = Some(self.connection.sync(version));
        self.socket.flush().await?;
        Ok(true)
    }

    /// KWin sends `ei_keyboard.modifiers` only when the state changes, so for a new device the
    /// layout group, Caps Lock and keys the user already holds are unknown. If no modifiers
    /// event has arrived yet, taps the keymap's probe key (AltGr on a key that is AltGr in
    /// every group) once per device, which makes the compositor report the full state. The
    /// caller's next `sync` round trip then waits for that report.
    pub(crate) async fn learn_modifiers(&mut self) -> io::Result<()> {
        let Some(d) = self.devices.values_mut().find(|d| d.usable()) else {
            return Ok(());
        };
        if d.mods_seen || d.probed {
            return Ok(());
        }
        d.probed = true;
        let Some(key) = d.table.as_ref().and_then(KeyTable::probe_key) else {
            debug!("no probe key in this keymap; assuming group 0 until the state changes");
            return Ok(());
        };
        debug!(key, "tapping the probe key to learn the modifier state");
        let tap = [
            KeyEvent { key, pressed: true },
            KeyEvent {
                key,
                pressed: false,
            },
        ];
        self.burst(&tap).await
    }

    /// Sends `events` as one emulation burst on the usable keyboard and flushes once.
    pub(crate) async fn burst(&mut self, events: &[KeyEvent]) -> io::Result<()> {
        let Some((device, kb)) = self
            .devices
            .iter()
            .find(|(_, d)| d.usable())
            .and_then(|(dev, d)| Some((dev.clone(), d.keyboard.clone()?)))
        else {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "no keyboard"));
        };
        let serial = self.last_serial;
        self.sequence = self.sequence.wrapping_add(1);
        device.start_emulating(serial, self.sequence);
        for e in events {
            let state = if e.pressed {
                ei::keyboard::KeyState::Press
            } else {
                ei::keyboard::KeyState::Released
            };
            kb.key(e.key, state);
            device.frame(serial, now_us());
        }
        device.stop_emulating(serial);
        // One flush per burst: the whole string reaches the compositor together, so the
        // user's own keystrokes can only land between bursts.
        self.socket.flush().await
    }

    /// Politely ends the connection.
    pub(crate) fn disconnect(self) {
        self.connection.disconnect();
        self.socket.try_flush();
    }
}

/// Forgets a client-side object the server has destroyed, so reis doesn't keep it forever.
fn forget(callback: &ei::Callback) {
    let object = callback.as_object();
    if let Some(backend) = object.backend() {
        backend.remove_id(object.id());
    }
}

/// CLOCK_MONOTONIC in microseconds, the time base EI frames use.
fn now_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let micros = u64::try_from(ts.tv_nsec).unwrap_or(0) / 1_000;
    secs * 1_000_000 + micros
}
