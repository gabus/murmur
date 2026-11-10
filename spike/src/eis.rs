//! `spike eis [--delay s] [--mode keys|text] [--forget] <text>`
//!
//! RemoteDesktop session + ConnectToEIS (libei via `reis`). Prints what the
//! compositor advertises (seat caps, devices, keymap layouts, ei_text), then
//! types the text either with keycodes resolved through the real XKB keymap
//! (we press Shift / AltGr ourselves) or via ei_text.utf8.

use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{
        ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    },
};
use futures_util::StreamExt;
use reis::{
    ei,
    enumflags2::BitFlags,
    event::{DeviceCapability, EiEvent},
};
use xkbcommon::xkb;

pub(crate) fn now_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

pub(crate) struct KeyPlan {
    pub(crate) keycode: u32, // XKB keycode (evdev + 8)
    pub(crate) mods: xkb::ModMask,
}

pub(crate) struct Typer {
    pub(crate) keymap: xkb::Keymap,
    pub(crate) group: u32,
}

impl Typer {
    /// Find keycode + modifier mask producing `sym` in `layout`.
    pub(crate) fn plan(&self, sym: xkb::Keysym, layout: u32) -> Option<KeyPlan> {
        let km = &self.keymap;
        for kc in km.min_keycode().raw()..=km.max_keycode().raw() {
            let key = xkb::Keycode::new(kc);
            for level in 0..km.num_levels_for_key(key, layout) {
                if km.key_get_syms_by_level(key, layout, level) == [sym] {
                    let mut masks = [0u32; 4];
                    let n = km.key_get_mods_for_level(key, layout, level, &mut masks);
                    if n == 0 {
                        continue;
                    }
                    // Prefer the smallest mask (fewest modifiers).
                    let mods = masks[..n].iter().copied().min_by_key(|m| m.count_ones())?;
                    return Some(KeyPlan { keycode: kc, mods });
                }
            }
        }
        None
    }

    pub(crate) fn modifier_key(&self, sym: xkb::Keysym) -> Option<u32> {
        self.plan(sym, self.group).map(|p| p.keycode)
    }
}

pub async fn run(args: Vec<String>) -> Result<()> {
    let mut delay = 5u64;
    let mut mode = String::from("keys");
    let mut forget = false;
    let mut repeat = 1usize;
    let mut word_gap_ms = 0u64;
    let mut modup = false;
    let mut text = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--delay" => delay = it.next().and_then(|s| s.parse().ok()).unwrap_or(5),
            "--repeat" => repeat = it.next().and_then(|s| s.parse().ok()).unwrap_or(1),
            "--word-gap-ms" => word_gap_ms = it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            "--modup" => modup = true,
            "--mode" => mode = it.next().unwrap_or_default(),
            "--forget" => forget = true,
            _ => text = Some(a),
        }
    }
    let Some(text) = text else {
        bail!("usage: spike eis [--delay s] [--mode keys|text] [--forget] <text>")
    };
    if text.contains(['\n', '\r', '\t']) {
        bail!("refusing to type control characters (safety)");
    }

    crate::register_if_host().await;
    let dir = crate::state_dir();
    std::fs::create_dir_all(&dir)?;
    let token_file = dir.join("remote-desktop.token");
    let token = if forget {
        None
    } else {
        std::fs::read_to_string(&token_file).ok()
    };
    println!("restore token present: {}", token.is_some());

    let rd = RemoteDesktop::new().await?;
    let session = rd.create_session(CreateSessionOptions::default()).await?;
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::KEYBOARD)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await?
    .response()?;
    let t0 = Instant::now();
    let started = rd
        .start(&session, None, StartOptions::default())
        .await?
        .response()
        .context("start")?;
    println!("session started in {:?}", t0.elapsed());
    if let Some(tok) = started.restore_token() {
        std::fs::write(&token_file, tok)?;
    }
    let fd = rd
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .context("ConnectToEIS")?;
    let ctx = ei::Context::new(UnixStream::from(fd))?;
    let (conn, mut events) = ctx
        .handshake_tokio("murmur-spike", ei::handshake::ContextType::Sender)
        .await
        .context("EI handshake")?;
    println!("EI handshake ok after {:?}", t0.elapsed());

    let mut keyboard: Option<reis::event::Device> = None;
    let mut text_dev: Option<reis::event::Device> = None;
    let mut resumed: HashMap<String, u32> = HashMap::new();
    let mut typer: Option<Typer> = None;
    let mut group = 0u32;
    let deadline = tokio::time::sleep(Duration::from_secs(40));
    tokio::pin!(deadline);
    // KWin 6.7 doesn't flush queued EIS events until something wakes it; poke it.
    let poke = std::env::var_os("NO_POKE").is_none();
    let mut poke_timer = tokio::time::interval(Duration::from_millis(200));
    let mut seat_bound = false;

    // Collect devices until we have a resumed keyboard (and text, if offered).
    loop {
        tokio::select! {
            ev = events.next() => {
                let Some(ev) = ev else { bail!("EIS disconnected") };
                match ev? {
                    EiEvent::SeatAdded(s) => {
                        println!("seat added: {:?} at {:?}", s.seat.name(), t0.elapsed());
                        let caps = if std::env::var_os("NO_TEXT").is_some() { BitFlags::from(DeviceCapability::Keyboard) } else { BitFlags::from(DeviceCapability::Keyboard) | DeviceCapability::Text };
                        println!("binding {caps:?}");
                        s.seat.bind_capabilities(caps);
                        conn.flush().ok();
                        seat_bound = true;
                    }
                    EiEvent::DeviceAdded(d) => {
                        let dev = d.device;
                        let caps: Vec<_> = [DeviceCapability::Keyboard, DeviceCapability::Text, DeviceCapability::Pointer]
                            .into_iter().filter(|c| dev.has_capability(*c)).collect();
                        println!("device added: {:?} type={:?} caps={caps:?} keymap={}", dev.name(), dev.device_type(), dev.keymap().is_some());
                        if let Some(km) = dev.keymap() {
                            let fd = km.fd.try_clone()?;
                            let xctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                            // SAFETY: fd is a keymap fd from the EIS server with the advertised size.
                            let keymap = unsafe { xkb::Keymap::new_from_fd(&xctx, fd, km.size as usize, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::COMPILE_NO_FLAGS) }?
                                .context("keymap compile failed")?;
                            let layouts: Vec<_> = (0..keymap.num_layouts()).map(|i| keymap.layout_get_name(i).to_string()).collect();
                            println!("  keymap layouts: {layouts:?}");
                            typer = Some(Typer { keymap, group: 0 });
                        }
                        if dev.has_capability(DeviceCapability::Keyboard) { keyboard = Some(dev.clone()); }
                        if dev.has_capability(DeviceCapability::Text) { text_dev = Some(dev.clone()); }
                    }
                    EiEvent::DeviceResumed(r) => {
                        println!("device resumed: {:?} serial={} after {:?}", r.device.name(), r.serial, t0.elapsed());
                        resumed.insert(r.device.name().unwrap_or("?").to_string(), r.serial);
                    }
                    EiEvent::KeyboardModifiers(m) => {
                        println!("modifiers: depressed={:#x} locked={:#x} group={}", m.depressed, m.locked, m.group);
                        group = m.group;
                    }
                    other => println!("event at {:?}: {other:?}", t0.elapsed()),
                }
                let kb_ready = keyboard.as_ref().is_some_and(|k| resumed.contains_key(k.name().unwrap_or("?")));
                if kb_ready && (mode != "text" || text_dev.is_some()) { break; }
            }
            _ = poke_timer.tick(), if poke && !seat_bound || poke && keyboard.is_none() => {
                println!("poke: sync at {:?}", t0.elapsed());
                let _cb = conn.connection().sync(1);
                conn.flush().ok();
            }
            _ = &mut deadline => {
                println!("timeout waiting for devices (keyboard={} text={})", keyboard.is_some(), text_dev.is_some());
                break;
            }
        }
    }

    if mode == "probe" {
        println!(
            "probe only: keyboard={} text={} group={group}",
            keyboard.is_some(),
            text_dev.is_some()
        );
        session.close().await.ok();
        return Ok(());
    }
    println!("typing in {delay}s (mode={mode}, group={group}) — focus the target window");
    tokio::time::sleep(Duration::from_secs(delay)).await;
    let serial = conn.serial();
    let t = Instant::now();

    if mode == "text" {
        let Some(dev) = text_dev else {
            bail!("compositor did not offer ei_text")
        };
        let Some(txt) = dev.interface::<ei::Text>() else {
            bail!("no ei_text interface on device")
        };
        dev.device().start_emulating(serial, 1);
        // ei_text.utf8 is limited to 254 bytes per request; one request per frame.
        let mut chunk = String::new();
        let mut chunks = Vec::new();
        for c in text.chars() {
            if chunk.len() + c.len_utf8() > 254 {
                chunks.push(std::mem::take(&mut chunk));
            }
            chunk.push(c);
        }
        chunks.push(chunk);
        for c in &chunks {
            txt.utf8(c);
            dev.device().frame(serial, now_us());
        }
        dev.device().stop_emulating(serial);
        conn.flush().ok();
        println!("sent {} utf8 chunk(s) in {:?}", chunks.len(), t.elapsed());
    } else {
        let Some(dev) = keyboard else {
            bail!("no keyboard device")
        };
        let Some(kb) = dev.interface::<ei::Keyboard>() else {
            bail!("no ei_keyboard interface")
        };
        let Some(mut typer) = typer else {
            bail!("no keymap")
        };
        typer.group = group;
        let shift_kc = typer.modifier_key(xkb::Keysym::Shift_L);
        let lvl3_kc = typer.modifier_key(xkb::Keysym::ISO_Level3_Shift);
        let shift_idx = typer.keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let lvl3_idx = typer.keymap.mod_get_index("Mod5");
        println!("shift keycode={shift_kc:?} level3 keycode={lvl3_kc:?}");
        let ctrl_kcs: Vec<u32> = [
            xkb::Keysym::Control_L,
            xkb::Keysym::Control_R,
            xkb::Keysym::Alt_L,
            xkb::Keysym::Super_L,
        ]
        .into_iter()
        .filter_map(|k| typer.modifier_key(k))
        .collect();
        let press = |kc: u32, down: bool| {
            let st = if down {
                ei::keyboard::KeyState::Press
            } else {
                ei::keyboard::KeyState::Released
            };
            kb.key(kc - 8, st);
            dev.device().frame(serial, now_us());
        };
        let (mut typed, mut missing) = (0, Vec::new());
        let mut seq = 1u32;
        let words: Vec<String> = text.split_inclusive(' ').map(str::to_owned).collect();
        for _ in 0..repeat {
            for word in &words {
                dev.device().start_emulating(serial, seq);
                seq += 1;
                if modup {
                    for kc in &ctrl_kcs {
                        press(*kc, false);
                    }
                }
                for c in word.chars() {
                    let sym = xkb::Keysym::from_char(c);
                    let Some(plan) = typer.plan(sym, group) else {
                        let other: Vec<u32> = (0..typer.keymap.num_layouts())
                            .filter(|l| typer.plan(sym, *l).is_some())
                            .collect();
                        missing.push((c, other));
                        continue;
                    };
                    let need_shift = plan.mods & (1 << shift_idx) != 0;
                    let need_l3 = plan.mods & (1 << lvl3_idx) != 0;
                    let mods: Vec<u32> = [(need_shift, shift_kc), (need_l3, lvl3_kc)]
                        .into_iter()
                        .filter_map(|(need, kc)| if need { kc } else { None })
                        .collect();
                    for m in &mods {
                        press(*m, true);
                    }
                    press(plan.keycode, true);
                    press(plan.keycode, false);
                    for m in mods.iter().rev() {
                        press(*m, false);
                    }
                    typed += 1;
                }
                dev.device().stop_emulating(serial);
                conn.flush().ok();
                if word_gap_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(word_gap_ms)).await;
                }
            }
        }
        println!(
            "typed {typed} chars in {:?}; not in active layout (char, layouts having it): {missing:?}",
            t.elapsed()
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.close().await.ok();
    Ok(())
}

/// `spike eisraw` — dump raw EI protocol events after the handshake (diagnostics).
pub async fn raw(_args: Vec<String>) -> Result<()> {
    crate::register_if_host().await;
    let token = std::fs::read_to_string(crate::state_dir().join("remote-desktop.token")).ok();
    let rd = RemoteDesktop::new().await?;
    let session = rd.create_session(CreateSessionOptions::default()).await?;
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::KEYBOARD)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await?
    .response()?;
    let started = rd
        .start(&session, None, StartOptions::default())
        .await?
        .response()?;
    println!("started; devices={:?}", started.devices());
    if let Some(tok) = started.restore_token() {
        std::fs::write(crate::state_dir().join("remote-desktop.token"), tok)?;
    }
    let fd = rd
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await?;
    let ctx = ei::Context::new(UnixStream::from(fd))?;
    let mut stream = reis::tokio::EiEventStream::new(ctx.clone())?;
    let resp = reis::tokio::ei_handshake(
        &mut stream,
        "murmur-spike",
        ei::handshake::ContextType::Sender,
    )
    .await
    .context("handshake")?;
    println!("negotiated: {:?}", resp.negotiated_interfaces);
    ctx.flush().ok();
    let mut caps: HashMap<String, u64> = HashMap::new();
    let deadline = tokio::time::sleep(Duration::from_secs(8));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            ev = stream.next() => {
                let Some(ev) = ev else { println!("EOF"); break };
                match ev? {
                    reis::PendingRequestResult::Request(e) => {
                        println!("raw: {e:?}");
                        match e {
                            ei::Event::Connection(_, ei::connection::Event::Ping { ping }) => { ping.done(0); }
                            ei::Event::Seat(_, ei::seat::Event::Capability { mask, interface }) => { caps.insert(interface, mask); }
                            ei::Event::Seat(seat, ei::seat::Event::Done) => {
                                println!("seat caps: {caps:?}");
                                let mask = caps.get("ei_keyboard").copied().unwrap_or(0) | caps.get("ei_text").copied().unwrap_or(0);
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
    session.close().await.ok();
    Ok(())
}
