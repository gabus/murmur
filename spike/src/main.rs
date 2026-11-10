//! Stage-0 spike (throwaway). Proves the uncertain pieces before the real app:
//!
//! - `shortcut [secs]`            GlobalShortcuts: bind "toggle", print Activated/Deactivated
//! - `type [opts] <text>`         RemoteDesktop: keyboard-only session, type text via keysyms
//!     --delay <s>   wait before typing (time to focus a window / hold a key)
//!     --modup       send modifier key-up events before typing
//!     --forget      ignore the saved restore token
//!     --repeat <n>  type the text n times (sessions stay open)
//! - `mic [secs] [out.wav]`       cpal capture from the default source, 16 kHz mono if possible
//! - `testpad <out-file>`         Slint window with one text field mirrored to <out-file>
//! - `gnome [opts] [text]`        EIS typing via mutter's own RemoteDesktop API (headless only)
//!
//! Findings go to docs/portal-findings.md.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, ListShortcutsOptions, NewShortcut},
    remote_desktop::{
        DeviceType, KeyState, NotifyKeyboardKeysymOptions, RemoteDesktop, SelectDevicesOptions,
        StartOptions,
    },
};
use futures_util::StreamExt;

mod eis;
mod gnome;

const APP_ID: &str = "io.github.gabus.Murmur";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        bail!("usage: spike shortcut|type|mic|testpad ...");
    };
    let rest = args[1..].to_vec();
    if cmd == "testpad" {
        // Slint wants the main thread; no tokio needed.
        return testpad(rest);
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        match cmd.as_str() {
            "shortcut" => shortcut(rest).await,
            "type" => type_text(rest).await,
            "eis" => eis::run(rest).await,
            "eisraw" => eis::raw(rest).await,
            "gnome" => gnome::run(rest).await,
            "mic" => mic(rest),
            other => bail!("unknown command {other}"),
        }
    })
}

pub(crate) async fn register_if_host() {
    if ashpd::is_sandboxed() {
        println!("sandboxed: app id comes from flatpak");
        return;
    }
    match ashpd::AppID::try_from(APP_ID) {
        Ok(id) => match ashpd::register_host_app(id).await {
            Ok(()) => println!("registered host app id {APP_ID}"),
            Err(e) => println!("register_host_app failed (continuing): {e}"),
        },
        Err(e) => println!("bad app id: {e}"),
    }
}

// ---------------------------------------------------------------- shortcut

async fn shortcut(args: Vec<String>) -> Result<()> {
    let secs: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(60);
    register_if_host().await;
    let gs = GlobalShortcuts::new()
        .await
        .context("GlobalShortcuts portal")?;
    println!("GlobalShortcuts version {}", gs.version());
    let session = gs
        .create_session(CreateSessionOptions::default())
        .await
        .context("create_session")?;

    let listed = gs
        .list_shortcuts(&session, ListShortcutsOptions::default())
        .await?
        .response()?;
    println!(
        "already bound: {:?}",
        listed
            .shortcuts()
            .iter()
            .map(|s| (s.id(), s.trigger_description()))
            .collect::<Vec<_>>()
    );

    let t0 = Instant::now();
    let shortcuts =
        [NewShortcut::new("toggle", "Start/stop dictation").preferred_trigger("LOGO+h")];
    let bound = gs
        .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
        .await?
        .response()
        .context("bind_shortcuts response")?;
    println!(
        "bind took {:?}; bound: {:?}",
        t0.elapsed(),
        bound
            .shortcuts()
            .iter()
            .map(|s| (s.id(), s.description(), s.trigger_description()))
            .collect::<Vec<_>>()
    );

    let mut act = gs.receive_activated().await?;
    let mut deact = gs.receive_deactivated().await?;
    let mut changed = gs.receive_shortcuts_changed().await?;
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    println!("listening for {secs}s — press the shortcut");
    loop {
        tokio::select! {
            Some(a) = act.next() => println!("{:>8.3}s ACTIVATED   id={} ts={:?}", t0.elapsed().as_secs_f32(), a.shortcut_id(), a.timestamp()),
            Some(d) = deact.next() => println!("{:>8.3}s DEACTIVATED id={} ts={:?}", t0.elapsed().as_secs_f32(), d.shortcut_id(), d.timestamp()),
            Some(c) = changed.next() => println!("SHORTCUTS CHANGED {:?}", c.shortcuts().iter().map(|s| (s.id(), s.trigger_description())).collect::<Vec<_>>()),
            _ = &mut deadline => break,
        }
    }
    session.close().await.ok();
    Ok(())
}

// ---------------------------------------------------------------- type

pub(crate) fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("murmur-spike")
}

/// X11 keysym for a character: Latin-1 maps directly, everything else uses the
/// Unicode keysym range (0x0100_0000 + code point).
pub(crate) fn keysym(c: char) -> i32 {
    let cp = c as u32;
    let sym = match cp {
        0x20..=0x7e | 0xa0..=0xff => cp,
        _ => 0x0100_0000 | cp,
    };
    sym as i32
}

const MODIFIER_KEYSYMS: [(i32, &str); 9] = [
    (0xffe1, "Shift_L"),
    (0xffe2, "Shift_R"),
    (0xffe3, "Control_L"),
    (0xffe4, "Control_R"),
    (0xffe9, "Alt_L"),
    (0xffea, "Alt_R"),
    (0xffeb, "Super_L"),
    (0xffec, "Super_R"),
    (0xfe03, "ISO_Level3_Shift"),
];

async fn type_text(args: Vec<String>) -> Result<()> {
    let mut delay = 3u64;
    let mut modup = false;
    let mut forget = false;
    let mut repeat = 1usize;
    let mut pace_us = 0u64;
    let mut shift_mode = String::from("portal");
    let mut text = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--delay" => delay = it.next().and_then(|s| s.parse().ok()).unwrap_or(3),
            "--repeat" => repeat = it.next().and_then(|s| s.parse().ok()).unwrap_or(1),
            "--pace-us" => pace_us = it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            "--shift-mode" => shift_mode = it.next().unwrap_or_default(),
            "--modup" => modup = true,
            "--forget" => forget = true,
            _ => text = Some(a),
        }
    }
    let Some(text) = text else {
        bail!("usage: spike type [--delay s] [--modup] [--forget] [--repeat n] <text>")
    };
    if text.contains(['\n', '\r', '\t']) {
        bail!("refusing to type control characters (safety)");
    }

    register_if_host().await;
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    let token_file = dir.join("remote-desktop.token");
    let token = if forget {
        None
    } else {
        std::fs::read_to_string(&token_file).ok()
    };
    println!("restore token present: {}", token.is_some());

    let rd = RemoteDesktop::new().await.context("RemoteDesktop portal")?;
    println!("RemoteDesktop version {}", rd.version());
    println!("available devices: {:?}", rd.available_device_types().await);
    let t0 = Instant::now();
    let session = rd.create_session(CreateSessionOptions::default()).await?;
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::KEYBOARD)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await?
    .response()
    .context("select_devices")?;
    let started = rd
        .start(&session, None, StartOptions::default())
        .await?
        .response()
        .context("start (dialog denied?)")?;
    println!(
        "session started in {:?}; devices={:?} new token={}",
        t0.elapsed(),
        started.devices(),
        started.restore_token().is_some()
    );
    if let Some(tok) = started.restore_token() {
        std::fs::write(&token_file, tok)?;
    }

    println!("typing in {delay}s — focus the target window");
    tokio::time::sleep(Duration::from_secs(delay)).await;

    let opts = || NotifyKeyboardKeysymOptions::default();
    if modup {
        for (sym, name) in MODIFIER_KEYSYMS {
            rd.notify_keyboard_keysym(&session, sym, KeyState::Released, opts())
                .await
                .with_context(|| format!("release {name}"))?;
        }
        println!("sent modifier releases");
    }

    for round in 0..repeat {
        let t = Instant::now();
        let mut n = 0usize;
        for c in text.chars() {
            // Explicit-shift strategies: send Shift ourselves and the unshifted letter.
            let explicit = shift_mode != "portal" && c.is_ascii_uppercase();
            let c = if explicit { c.to_ascii_lowercase() } else { c };
            if explicit {
                if shift_mode == "keycode" {
                    rd.notify_keyboard_keycode(&session, 42, KeyState::Pressed, Default::default())
                        .await?;
                } else {
                    rd.notify_keyboard_keysym(&session, 0xffe1, KeyState::Pressed, opts())
                        .await?;
                }
            }
            let sym = keysym(c);
            rd.notify_keyboard_keysym(&session, sym, KeyState::Pressed, opts())
                .await?;
            if pace_us > 0 {
                tokio::time::sleep(Duration::from_micros(pace_us)).await;
            }
            rd.notify_keyboard_keysym(&session, sym, KeyState::Released, opts())
                .await?;
            if pace_us > 0 {
                tokio::time::sleep(Duration::from_micros(pace_us)).await;
            }
            if explicit {
                if shift_mode == "keycode" {
                    rd.notify_keyboard_keycode(
                        &session,
                        42,
                        KeyState::Released,
                        Default::default(),
                    )
                    .await?;
                } else {
                    rd.notify_keyboard_keysym(&session, 0xffe1, KeyState::Released, opts())
                        .await?;
                }
            }
            n += 1;
        }
        let el = t.elapsed();
        println!(
            "round {round}: {n} chars in {:?} = {:.0} chars/s",
            el,
            n as f64 / el.as_secs_f64()
        );
    }
    // Give the compositor a moment to flush before tearing the session down.
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.close().await.ok();
    Ok(())
}

// ---------------------------------------------------------------- mic

fn mic(args: Vec<String>) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let secs: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(3);
    let out = args.get(1).map(PathBuf::from);

    println!("available hosts: {:?}", cpal::available_hosts());
    let host = cpal::default_host();
    println!("using host: {:?}", host.id());
    let dev = host
        .default_input_device()
        .context("no default input device")?;
    println!(
        "device: {}",
        dev.description().map(|d| d.to_string()).unwrap_or_default()
    );
    for c in dev.supported_input_configs()? {
        println!(
            "  supports ch={} {}..{} Hz {:?}",
            c.channels(),
            c.min_sample_rate(),
            c.max_sample_rate(),
            c.sample_format()
        );
    }
    let want = cpal::StreamConfig {
        channels: 1,
        sample_rate: 16_000,
        buffer_size: match std::env::var("BUF").ok().and_then(|b| b.parse().ok()) {
            Some(n) => cpal::BufferSize::Fixed(n),
            None => cpal::BufferSize::Default,
        },
    };
    let samples: Arc<Mutex<Vec<f32>>> = Arc::default();
    let callbacks: Arc<Mutex<Vec<usize>>> = Arc::default();
    let (s2, c2) = (samples.clone(), callbacks.clone());
    let t0 = Instant::now();
    let first_cb: Arc<Mutex<Option<Duration>>> = Arc::default();
    let f2 = first_cb.clone();
    let stream = dev.build_input_stream::<f32, _, _>(
        want,
        move |data: &[f32], _| {
            if let Ok(mut s) = s2.lock() {
                s.extend_from_slice(data);
            }
            if let Ok(mut c) = c2.lock() {
                c.push(data.len());
            }
            if let Ok(mut f) = f2.lock() {
                f.get_or_insert(t0.elapsed());
            }
        },
        |e| eprintln!("stream error: {e}"),
        None,
    )?;
    println!("built after {:?}", t0.elapsed());
    stream.play()?;
    println!("stream open after {:?}; recording {secs}s", t0.elapsed());
    std::thread::sleep(Duration::from_secs(secs));
    drop(stream);
    println!(
        "stream dropped; first callback at {:?}",
        first_cb.lock().ok().and_then(|f| *f)
    );

    let s = samples.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
    let c = callbacks.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
    let rms = (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt();
    let peak = s.iter().fold(0f32, |m, x| m.max(x.abs()));
    println!(
        "got {} samples ({:.2}s @16k), {} callbacks, sizes min={:?} max={:?}, rms={rms:.4} peak={peak:.4}",
        s.len(),
        s.len() as f32 / 16_000.0,
        c.len(),
        c.iter().min(),
        c.iter().max()
    );
    if let Some(out) = out {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&out, spec)?;
        for x in s.iter() {
            w.write_sample((x.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        w.finalize()?;
        println!("wrote {}", out.display());
    }
    Ok(())
}

// ---------------------------------------------------------------- testpad

slint::slint! {
    import { TextEdit, VerticalBox } from "std-widgets.slint";
    export component Pad inherits Window {
        title: "murmur testpad";
        preferred-width: 720px;
        preferred-height: 260px;
        forward-focus: te;
        callback edited(string);
        VerticalBox {
            Text { text: "murmur test pad — injected text lands here"; }
            te := TextEdit {
                font-size: 18px;
                edited(t) => { root.edited(t); }
            }
        }
    }
}

fn testpad(args: Vec<String>) -> Result<()> {
    let Some(out) = args.first().map(PathBuf::from) else {
        bail!("usage: spike testpad <out-file>");
    };
    std::fs::write(&out, "")?;
    let pad = Pad::new()?;
    pad.on_edited(move |t| {
        if let Err(e) = std::fs::write(&out, t.as_str()) {
            eprintln!("write failed: {e}");
        }
    });
    pad.run()?;
    Ok(())
}
