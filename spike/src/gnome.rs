//! `spike gnome [opts] [text]`: EIS typing against **mutter's private D-Bus API**
//! (`org.gnome.Mutter.RemoteDesktop`), with no portal and no dialog. Meant only for a headless
//! mutter / gnome-shell started by `spike/harness/gnome-headless.sh` on a private bus.
//!
//! Options:
//!   --probe              raw EI dump (negotiated interfaces, seat capabilities), no typing
//!   --mode keys|text     keys = keymap-resolved ei_keyboard events (default), text = ei_text.utf8
//!   --no-poke            don't send the `ei_connection.sync` poke while waiting for the device
//!   --repoll             with --no-poke: still wake every 200 ms and re-poll the stream, sending
//!                        nothing (tells a compositor stall from a client-side lost wakeup)
//!   --hold-ctrl          a second EIS connection ("physical keyboard") holds Control_L while typing
//!   --set-keymap <lay>   call Session.SetKeymap with an XKB keymap compiled from layouts, e.g. us,lt
//!   --layout-index <n>   initial layout index for --set-keymap (default 0)
//!   --settle-ms <n>      listen for events this long before typing (default 300)
//!   --go-file <path>     after the device is ready, wait (max 10 s) until <path> exists; the
//!                        harness creates it once the test pad has keyboard focus
//!   --click              also take an EIS pointer and left-click where the cursor is, before typing
//!   --click-at <x,y>     like --click, but first move the pointer by -100000,-100000 then x,y
//!                        (EXPERIMENTAL, didn't work: the click never landed on the pad, so it
//!                        is not used by the harness; gnome-shell focus uses a key warm-up)
//!   --pre-escape         tap Escape once after the device is ready (leaves gnome-shell's
//!                        startup overview), then wait 1 s
//!
//! Safety: refuses to run unless DISPLAY is unset, WAYLAND_DISPLAY starts with `murmur-` and the
//! session bus is not the user's default bus (and differs from $MURMUR_OUTER_BUS if set).

use std::{
    collections::HashMap,
    io::Write,
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use ashpd::zbus::{Connection, Fd, OwnedFd, OwnedObjectPath, OwnedVariant, Variant};
use futures_util::StreamExt;
use reis::{
    ei,
    enumflags2::BitFlags,
    event::{DeviceCapability, EiEvent},
};
use xkbcommon::xkb;

use crate::eis::{Typer, now_us};

const RD_BUS: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const RD_IFACE: &str = "org.gnome.Mutter.RemoteDesktop";
const SESSION_IFACE: &str = "org.gnome.Mutter.RemoteDesktop.Session";
/// `device-types` bit for keyboard (see SupportedDeviceTypes).
const DEVICE_KEYBOARD: u32 = 1;
/// `device-types` bit for pointer.
const DEVICE_POINTER: u32 = 2;
/// evdev BTN_LEFT.
const BTN_LEFT: u32 = 0x110;
/// evdev keycode of Control_L.
const KEY_LEFTCTRL: u32 = 29;
/// evdev keycode of Escape.
const KEY_ESC: u32 = 1;

fn assert_nested() -> Result<()> {
    if std::env::var_os("DISPLAY").is_some() {
        bail!("SAFETY: DISPLAY is set; run only inside the headless harness");
    }
    let wl = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    if !wl.starts_with("murmur-") {
        bail!("SAFETY: WAYLAND_DISPLAY={wl:?} is not a murmur-* nested socket");
    }
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
    if bus.is_empty() {
        bail!("SAFETY: DBUS_SESSION_BUS_ADDRESS is unset");
    }
    if let Ok(outer) = std::env::var("MURMUR_OUTER_BUS")
        && outer == bus
    {
        bail!("SAFETY: session bus is the outer (real) bus");
    }
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let default = format!("unix:path={}/bus", rt.to_string_lossy());
        if bus == default || bus.starts_with(&format!("{default},")) {
            bail!("SAFETY: session bus is the default user bus {default}");
        }
    }
    Ok(())
}

/// One `org.gnome.Mutter.RemoteDesktop.Session`. Mutter closes it when our bus connection drops.
struct RdSession {
    conn: Connection,
    path: OwnedObjectPath,
}

impl RdSession {
    async fn create(conn: &Connection) -> Result<Self> {
        let reply = conn
            .call_method(Some(RD_BUS), RD_PATH, Some(RD_IFACE), "CreateSession", &())
            .await
            .context("CreateSession")?;
        let path: OwnedObjectPath = reply.body().deserialize()?;
        Ok(Self {
            conn: conn.clone(),
            path,
        })
    }

    async fn start(&self) -> Result<()> {
        self.conn
            .call_method(Some(RD_BUS), &self.path, Some(SESSION_IFACE), "Start", &())
            .await
            .context("Start")?;
        Ok(())
    }

    async fn stop(&self) {
        let _ = self
            .conn
            .call_method(Some(RD_BUS), &self.path, Some(SESSION_IFACE), "Stop", &())
            .await;
    }

    async fn connect_to_eis(&self, device_types: u32) -> Result<std::os::fd::OwnedFd> {
        let mut opts: HashMap<&str, Variant> = HashMap::new();
        opts.insert("device-types", Variant::from(device_types));
        let reply = self
            .conn
            .call_method(
                Some(RD_BUS),
                &self.path,
                Some(SESSION_IFACE),
                "ConnectToEIS",
                &(opts,),
            )
            .await
            .context("ConnectToEIS")?;
        let fd: OwnedFd = reply.body().deserialize()?;
        Ok(fd.into())
    }

    async fn props(&self) -> Result<HashMap<String, OwnedVariant>> {
        let reply = self
            .conn
            .call_method(
                Some(RD_BUS),
                &self.path,
                Some("org.freedesktop.DBus.Properties"),
                "GetAll",
                &(SESSION_IFACE,),
            )
            .await?;
        Ok(reply.body().deserialize()?)
    }

    /// Session.SetKeymap with an XKB text-v1 keymap compiled from `layouts` (e.g. "us,lt").
    async fn set_keymap(&self, layouts: &str, index: u32) -> Result<()> {
        let xctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let km = xkb::Keymap::new_from_names(
            &xctx,
            "evdev",
            "pc105",
            layouts,
            "",
            None,
            xkb::COMPILE_NO_FLAGS,
        )
        .context("compile keymap from names")?;
        let text = km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let path = crate::state_dir().join("set-keymap.xkb");
        std::fs::create_dir_all(crate::state_dir())?;
        let mut f = std::fs::File::create(&path)?;
        f.write_all(text.as_bytes())?;
        drop(f);
        let file = std::fs::File::open(&path)?;
        let mut opts: HashMap<&str, Variant> = HashMap::new();
        opts.insert("keymap-type", Variant::from(0u32)); // XKB
        opts.insert("xkb-keymap-format", Variant::from(1u32)); // XKB_TEXT_V1
        opts.insert("xkb-keymap", Variant::from(Fd::from(&file)));
        opts.insert("xkb-keymap-layout-index", Variant::from(index));
        self.conn
            .call_method(
                Some(RD_BUS),
                &self.path,
                Some(SESSION_IFACE),
                "SetKeymap",
                &(opts,),
            )
            .await
            .context("SetKeymap")?;
        Ok(())
    }
}

/// One EI sender connection bound to the keyboard (and ei_text, if offered).
struct EisKb {
    label: &'static str,
    t0: Instant,
    conn: reis::event::Connection,
    events: reis::tokio::EiConvertEventStream,
    repoll: bool,
    keyboard: Option<reis::event::Device>,
    text_dev: Option<reis::event::Device>,
    pointer: Option<reis::event::Device>,
    resumed: Vec<reis::event::Device>,
    typer: Option<Typer>,
    group: u32,
    depressed: u32,
    seq: u32,
}

impl EisKb {
    async fn connect(
        label: &'static str,
        fd: std::os::fd::OwnedFd,
        t0: Instant,
        repoll: bool,
    ) -> Result<Self> {
        let ctx = ei::Context::new(UnixStream::from(fd))?;
        // reis 0.7.1's stream can return Pending without registering a waker (lost wakeup),
        // which also hangs the handshake. Re-poll it every 200 ms until it completes.
        let hs = ctx.handshake_tokio("murmur-spike-gnome", ei::handshake::ContextType::Sender);
        tokio::pin!(hs);
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut repolls = 0u32;
        let (conn, events) = loop {
            tokio::select! {
                r = &mut hs => break r.context("EI handshake")?,
                _ = tick.tick() => repolls += 1,
            }
        };
        if repolls > 1 {
            println!("[{label}] handshake needed {} re-poll(s)", repolls - 1);
        }
        println!("[{label}] EI handshake ok at {:?}", t0.elapsed());
        Ok(Self {
            label,
            t0,
            conn,
            events,
            repoll,
            keyboard: None,
            text_dev: None,
            pointer: None,
            resumed: Vec::new(),
            typer: None,
            group: 0,
            depressed: 0,
            seq: 1,
        })
    }

    fn keyboard_ready(&self) -> bool {
        self.keyboard
            .as_ref()
            .is_some_and(|k| self.resumed.contains(k))
    }

    fn handle(&mut self, ev: EiEvent) -> Result<()> {
        let (l, at) = (self.label, self.t0.elapsed());
        match ev {
            EiEvent::SeatAdded(s) => {
                // Capabilities the seat doesn't offer map to 0 and are ignored.
                let caps = BitFlags::from(DeviceCapability::Keyboard)
                    | DeviceCapability::Text
                    | DeviceCapability::Pointer
                    | DeviceCapability::Button;
                println!(
                    "[{l}] seat added {:?} at {at:?}; binding {caps:?}",
                    s.seat.name()
                );
                s.seat.bind_capabilities(caps);
                self.conn.flush().ok();
            }
            EiEvent::DeviceAdded(d) => {
                let dev = d.device;
                let caps: Vec<_> = [
                    DeviceCapability::Keyboard,
                    DeviceCapability::Text,
                    DeviceCapability::Pointer,
                    DeviceCapability::Button,
                    DeviceCapability::PointerAbsolute,
                ]
                .into_iter()
                .filter(|c| dev.has_capability(*c))
                .collect();
                println!(
                    "[{l}] device added {:?} type={:?} caps={caps:?} keymap={} at {at:?}",
                    dev.name(),
                    dev.device_type(),
                    dev.keymap().map(|k| k.size).unwrap_or(0)
                );
                if let Some(km) = dev.keymap() {
                    let fd = km.fd.try_clone()?;
                    let xctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                    // SAFETY: fd is a keymap fd from the EIS server with the advertised size.
                    let keymap = unsafe {
                        xkb::Keymap::new_from_fd(
                            &xctx,
                            fd,
                            km.size as usize,
                            xkb::KEYMAP_FORMAT_TEXT_V1,
                            xkb::COMPILE_NO_FLAGS,
                        )
                    }?
                    .context("keymap compile failed")?;
                    let layouts: Vec<_> = (0..keymap.num_layouts())
                        .map(|i| keymap.layout_get_name(i).to_string())
                        .collect();
                    println!("[{l}]   keymap layouts: {layouts:?}");
                    self.typer = Some(Typer { keymap, group: 0 });
                }
                if dev.has_capability(DeviceCapability::Keyboard) {
                    self.keyboard = Some(dev.clone());
                }
                if dev.has_capability(DeviceCapability::Button) {
                    self.pointer = Some(dev.clone());
                }
                if dev.has_capability(DeviceCapability::Text) {
                    self.text_dev = Some(dev);
                }
            }
            EiEvent::DeviceResumed(r) => {
                println!("[{l}] device resumed {:?} at {at:?}", r.device.name());
                self.resumed.push(r.device);
            }
            EiEvent::DevicePaused(p) => {
                println!("[{l}] device paused {:?} at {at:?}", p.device.name());
                self.resumed.retain(|d| *d != p.device);
            }
            EiEvent::DeviceRemoved(r) => {
                println!("[{l}] device removed {:?} at {at:?}", r.device.name());
                if self.keyboard.as_ref() == Some(&r.device) {
                    self.keyboard = None;
                }
            }
            EiEvent::KeyboardModifiers(m) => {
                println!(
                    "[{l}] MODIFIERS depressed={:#x} latched={:#x} locked={:#x} group={} at {at:?}",
                    m.depressed, m.latched, m.locked, m.group
                );
                self.group = m.group;
                self.depressed = m.depressed;
            }
            other => println!("[{l}] event at {at:?}: {other:?}"),
        }
        Ok(())
    }

    /// Process events until `until`. If `stop_when_ready`, return as soon as the keyboard is
    /// resumed (and ei_text, when `want_text`). Sends a sync poke every 200 ms while waiting.
    async fn pump(
        &mut self,
        until: Instant,
        stop_when_ready: bool,
        want_text: bool,
        poke: bool,
    ) -> Result<bool> {
        let mut next_poke = Instant::now();
        loop {
            let ready = self.keyboard_ready() && (!want_text || self.text_dev.is_some());
            if stop_when_ready && ready {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= until {
                return Ok(ready);
            }
            let mut wake = until;
            if (poke || self.repoll) && stop_when_ready && !ready {
                if now >= next_poke {
                    if poke {
                        println!("[{}] poke: sync at {:?}", self.label, self.t0.elapsed());
                        let _cb = self.conn.connection().sync(1);
                        self.conn.flush().ok();
                    } else {
                        println!(
                            "[{}] re-poll (nothing sent) at {:?}",
                            self.label,
                            self.t0.elapsed()
                        );
                    }
                    next_poke = now + Duration::from_millis(200);
                }
                wake = wake.min(next_poke);
            }
            match tokio::time::timeout(wake.saturating_duration_since(now), self.events.next())
                .await
            {
                Err(_) => {}
                Ok(None) => bail!("[{}] EIS disconnected", self.label),
                Ok(Some(ev)) => self.handle(ev?)?,
            }
        }
    }

    fn key(&self, kc_evdev: u32, down: bool) -> Result<()> {
        let dev = self.keyboard.as_ref().context("no keyboard")?;
        let kb = dev.interface::<ei::Keyboard>().context("no ei_keyboard")?;
        let st = if down {
            ei::keyboard::KeyState::Press
        } else {
            ei::keyboard::KeyState::Released
        };
        kb.key(kc_evdev, st);
        dev.device().frame(self.conn.serial(), now_us());
        Ok(())
    }

    fn start_emulating(&mut self) -> Result<()> {
        let dev = self.keyboard.as_ref().context("no keyboard")?;
        dev.device().start_emulating(self.conn.serial(), self.seq);
        self.seq += 1;
        Ok(())
    }

    fn stop_emulating(&self) -> Result<()> {
        let dev = self.keyboard.as_ref().context("no keyboard")?;
        dev.device().stop_emulating(self.conn.serial());
        self.conn.flush().ok();
        Ok(())
    }

    /// Left click at the current pointer position (to focus the window under it, like a user).
    async fn click(&mut self, at: Option<(f32, f32)>) -> Result<()> {
        let give_up = Instant::now() + Duration::from_secs(3);
        while !self
            .pointer
            .as_ref()
            .is_some_and(|p| self.resumed.contains(p))
        {
            if Instant::now() >= give_up {
                bail!("[{}] no resumed pointer device with ei_button", self.label);
            }
            self.pump(
                Instant::now() + Duration::from_millis(50),
                false,
                false,
                false,
            )
            .await?;
        }
        let dev = self.pointer.clone().context("no pointer")?;
        let btn = dev.interface::<ei::Button>().context("no ei_button")?;
        let serial = self.conn.serial();
        dev.device().start_emulating(serial, self.seq);
        self.seq += 1;
        if let Some((x, y)) = at {
            // No absolute pointer without a screencast viewport: clamp to (0,0), then move.
            let ptr = dev.interface::<ei::Pointer>().context("no ei_pointer")?;
            ptr.motion_relative(-100_000.0, -100_000.0);
            dev.device().frame(serial, now_us());
            ptr.motion_relative(x, y);
            dev.device().frame(serial, now_us());
        }
        btn.button(BTN_LEFT, ei::button::ButtonState::Press);
        dev.device().frame(serial, now_us());
        btn.button(BTN_LEFT, ei::button::ButtonState::Released);
        dev.device().frame(serial, now_us());
        dev.device().stop_emulating(serial);
        self.conn.flush().ok();
        println!("[{}] clicked at {:?}", self.label, self.t0.elapsed());
        Ok(())
    }

    /// Same strategy as `eis::run` keys mode: one emulation burst per word; resolve each char
    /// through the device keymap in the current group, pressing Shift / Level3 ourselves.
    fn type_keys(&mut self, text: &str) -> Result<()> {
        let typer = self.typer.as_ref().context("no keymap")?;
        let group = self.group;
        let shift_kc = typer.modifier_key(xkb::Keysym::Shift_L);
        let lvl3_kc = typer.modifier_key(xkb::Keysym::ISO_Level3_Shift);
        let shift_idx = typer.keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let lvl3_idx = typer.keymap.mod_get_index("Mod5");
        println!(
            "[{}] typing in group {group}: shift kc={shift_kc:?} level3 kc={lvl3_kc:?}",
            self.label
        );
        let mut plans: Vec<Vec<(u32, bool)>> = Vec::new(); // per word: (xkb keycode, down)
        let (mut typed, mut missing) = (0, Vec::new());
        for word in text.split_inclusive(' ') {
            let mut evs = Vec::new();
            for c in word.chars() {
                let sym = xkb::Keysym::from_char(c);
                let Some(plan) = typer.plan(sym, group) else {
                    let other: Vec<u32> = (0..typer.keymap.num_layouts())
                        .filter(|l| typer.plan(sym, *l).is_some())
                        .collect();
                    missing.push((c, other));
                    continue;
                };
                let mods: Vec<u32> = [
                    (plan.mods & (1 << shift_idx) != 0, shift_kc),
                    (plan.mods & (1 << lvl3_idx) != 0, lvl3_kc),
                ]
                .into_iter()
                .filter_map(|(need, kc)| if need { kc } else { None })
                .collect();
                evs.extend(mods.iter().map(|m| (*m, true)));
                evs.push((plan.keycode, true));
                evs.push((plan.keycode, false));
                evs.extend(mods.iter().rev().map(|m| (*m, false)));
                typed += 1;
            }
            plans.push(evs);
        }
        let t = Instant::now();
        for evs in plans {
            self.start_emulating()?;
            for (kc, down) in evs {
                self.key(kc - 8, down)?;
            }
            self.stop_emulating()?;
        }
        println!(
            "[{}] typed {typed} chars in {:?}; not in group {group} (char, layouts having it): {missing:?}",
            self.label,
            t.elapsed()
        );
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        let dev = self
            .text_dev
            .clone()
            .context("compositor did not offer ei_text")?;
        let txt = dev
            .interface::<ei::Text>()
            .context("no ei_text interface")?;
        let serial = self.conn.serial();
        dev.device().start_emulating(serial, self.seq);
        self.seq += 1;
        txt.utf8(text);
        dev.device().frame(serial, now_us());
        dev.device().stop_emulating(serial);
        self.conn.flush().ok();
        println!("[{}] sent ei_text.utf8 ({} bytes)", self.label, text.len());
        Ok(())
    }
}

pub async fn run(args: Vec<String>) -> Result<()> {
    assert_nested()?;
    let mut probe = false;
    let mut mode = String::from("keys");
    let mut poke = std::env::var_os("NO_POKE").is_none();
    let mut hold_ctrl = false;
    let mut repoll = false;
    let mut set_keymap: Option<String> = None;
    let mut layout_index = 0u32;
    let mut settle_ms = 300u64;
    let mut pre_escape = false;
    let mut click = false;
    let mut click_at: Option<(f32, f32)> = None;
    let mut go_file: Option<std::path::PathBuf> = None;
    let mut text = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--probe" => probe = true,
            "--mode" => mode = it.next().unwrap_or_default(),
            "--no-poke" => poke = false,
            "--repoll" => repoll = true,
            "--hold-ctrl" => hold_ctrl = true,
            "--pre-escape" => pre_escape = true,
            "--click" => click = true,
            "--click-at" => {
                click = true;
                click_at = it.next().and_then(|s| {
                    let (x, y) = s.split_once(',')?;
                    Some((x.parse().ok()?, y.parse().ok()?))
                });
            }
            "--go-file" => go_file = it.next().map(Into::into),
            "--set-keymap" => set_keymap = it.next(),
            "--layout-index" => layout_index = it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            "--settle-ms" => settle_ms = it.next().and_then(|s| s.parse().ok()).unwrap_or(300),
            _ => text = Some(a),
        }
    }
    if text
        .as_deref()
        .is_some_and(|t| t.contains(['\n', '\r', '\t']))
    {
        bail!("refusing to type control characters (safety)");
    }

    let t0 = Instant::now();
    let bus = Connection::session().await.context("private session bus")?;
    let session = RdSession::create(&bus).await?;
    println!("session {} created", session.path.as_str());
    session.start().await?;
    println!("session started at {:?}", t0.elapsed());
    for (k, v) in session.props().await? {
        println!("  prop {k} = {v:?}");
    }
    if let Some(layouts) = &set_keymap {
        session.set_keymap(layouts, layout_index).await?;
        println!("SetKeymap({layouts}, index {layout_index}) ok");
        for (k, v) in session.props().await? {
            println!("  prop {k} = {v:?}");
        }
    }

    let types = if click {
        DEVICE_KEYBOARD | DEVICE_POINTER
    } else {
        DEVICE_KEYBOARD
    };
    let fd = session.connect_to_eis(types).await?;
    if probe {
        return raw_probe(fd, &session).await;
    }
    let mut a = EisKb::connect("typer", fd, t0, repoll).await?;
    let ready = a
        .pump(t0 + Duration::from_secs(10), true, false, poke)
        .await?;
    println!(
        "[typer] ready={ready} at {:?} (poke={poke}, text offered={})",
        t0.elapsed(),
        a.text_dev.is_some()
    );
    if !ready {
        bail!("typer device not ready within 10 s");
    }
    if click {
        a.click(click_at).await?;
        a.pump(
            Instant::now() + Duration::from_millis(300),
            false,
            false,
            false,
        )
        .await?;
    }
    if pre_escape {
        a.start_emulating()?;
        a.key(KEY_ESC, true)?;
        a.key(KEY_ESC, false)?;
        a.stop_emulating()?;
        println!("[typer] tapped Escape at {:?}", t0.elapsed());
        a.pump(Instant::now() + Duration::from_secs(1), false, false, false)
            .await?;
    }
    if let Some(go) = &go_file {
        println!("[typer] waiting for {} at {:?}", go.display(), t0.elapsed());
        let give_up = Instant::now() + Duration::from_secs(10);
        // Test-only sync with the harness; a 50 ms file check is fine here.
        while !go.exists() && Instant::now() < give_up {
            a.pump(
                Instant::now() + Duration::from_millis(50),
                false,
                false,
                false,
            )
            .await?;
        }
        println!("[typer] go={} at {:?}", go.exists(), t0.elapsed());
    }

    // Optional second connection, acting as the user's physical keyboard holding Ctrl.
    let mut phys = None;
    if hold_ctrl {
        let s2 = RdSession::create(&bus).await?;
        s2.start().await?;
        let mut b = EisKb::connect(
            "physical",
            s2.connect_to_eis(DEVICE_KEYBOARD).await?,
            t0,
            repoll,
        )
        .await?;
        if !b
            .pump(t0 + Duration::from_secs(20), true, false, poke)
            .await?
        {
            bail!("physical device not ready");
        }
        b.start_emulating()?;
        b.key(KEY_LEFTCTRL, true)?;
        b.conn.flush().ok();
        println!("[physical] Control_L DOWN at {:?}", t0.elapsed());
        phys = Some((s2, b));
    }

    // Listen a bit (modifier events, late devices) before typing.
    a.pump(
        Instant::now() + Duration::from_millis(settle_ms),
        false,
        false,
        false,
    )
    .await?;
    if let Some(text) = &text {
        println!(
            "[typer] before typing: depressed={:#x} group={}",
            a.depressed, a.group
        );
        if mode == "text" {
            a.type_text(text)?;
        } else {
            a.type_keys(text)?;
        }
        a.pump(
            Instant::now() + Duration::from_millis(500),
            false,
            false,
            false,
        )
        .await?;
    }

    if let Some((s2, b)) = phys {
        b.key(KEY_LEFTCTRL, false)?;
        b.stop_emulating()?;
        println!("[physical] Control_L UP at {:?}", t0.elapsed());
        a.pump(
            Instant::now() + Duration::from_millis(500),
            false,
            false,
            false,
        )
        .await?;
        s2.stop().await;
    }
    session.stop().await;
    Ok(())
}

/// Raw protocol dump: negotiated interface versions, seat capabilities, device announcements.
async fn raw_probe(fd: std::os::fd::OwnedFd, session: &RdSession) -> Result<()> {
    let ctx = ei::Context::new(UnixStream::from(fd))?;
    let mut stream = reis::tokio::EiEventStream::new(ctx.clone())?;
    let resp = reis::tokio::ei_handshake(
        &mut stream,
        "murmur-spike-gnome",
        ei::handshake::ContextType::Sender,
    )
    .await
    .context("handshake")?;
    println!("negotiated: {:?}", resp.negotiated_interfaces);
    ctx.flush().ok();
    let mut caps: HashMap<String, u64> = HashMap::new();
    let deadline = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(deadline);
    // reis 0.7.1 can lose the stream wakeup (see harness notes); re-poll every 200 ms.
    let mut repoll = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            _ = repoll.tick() => {}
            ev = stream.next() => {
                let Some(ev) = ev else { println!("EOF"); break };
                match ev? {
                    reis::PendingRequestResult::Request(e) => {
                        println!("raw: {e:?}");
                        match e {
                            ei::Event::Connection(_, ei::connection::Event::Ping { ping }) => { ping.done(0); }
                            ei::Event::Seat(_, ei::seat::Event::Capability { mask, interface }) => { caps.insert(interface, mask); }
                            ei::Event::Seat(seat, ei::seat::Event::Done) => {
                                println!("SEAT CAPS: {caps:?}");
                                let mask = caps.get("ei_keyboard").copied().unwrap_or(0)
                                    | caps.get("ei_text").copied().unwrap_or(0);
                                seat.bind(mask);
                            }
                            _ => {}
                        }
                        ctx.flush().ok();
                    }
                    other => println!("raw other: {other:?}"),
                }
            }
            _ = &mut deadline => break,
        }
    }
    session.stop().await;
    Ok(())
}
