//! End-to-end: the real `murmur` daemon dictates a recording into a text field on a
//! nested, invisible KWin. Exercises app → ASR worker → output → EIS → compositor.
//! Nothing touches the real desktop: the daemon gets the nested bus and display only,
//! and its EIS connection comes from the nested KWin (`MURMUR_DEV_EIS=kwin-direct`).
//!
//! Needs `kwin_wayland`, the spike test pad (`cd spike && cargo build`) and the default
//! models in `$XDG_DATA_HOME/murmur/models`. Run with:
//!
//! ```sh
//! cargo test --features dev-harness --test e2e_nested_kwin -- --ignored --nocapture
//! ```
#![cfg(feature = "dev-harness")]

mod harness;

use std::{path::PathBuf, process::Command, time::Duration};

use ashpd::zbus;

const KEY_X: u32 = 45;

fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs kwin_wayland, the spike test pad and the models; see the file header"]
async fn dictation_end_to_end() {
    let models = murmur::paths::models_dir().expect("XDG data dir");
    let wav: PathBuf = models.join("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/test_wavs/en.wav");
    assert!(wav.is_file(), "models missing: {}", wav.display());
    let data_home = models
        .parent()
        .and_then(|p| p.parent())
        .expect("models dir has a data home")
        .to_path_buf();

    let mut nested = harness::Nested::start("us").await;
    let pad_path = nested.dir.join("pad.txt");
    nested.spawn_pad(&pad_path);
    let pad = harness::Pad::new(pad_path.clone());

    // Wait until the pad has keyboard focus: tap "x" until it shows up.
    let mut kb = harness::RawKeyboard::connect(&nested.bus).await;
    let mut focused = false;
    for _ in 0..50 {
        kb.key(KEY_X, true).await;
        kb.key(KEY_X, false).await;
        kb.sync().await;
        if !pad.text().is_empty() {
            focused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        focused,
        "test pad never got focus:\n{}",
        nested.log_tail("kwin")
    );
    let prefix = pad.settle(Duration::from_millis(300)).await;

    let env = nested.session_env();
    assert!(
        env.iter()
            .any(|(k, v)| k == "DBUS_SESSION_BUS_ADDRESS" && *v == nested.bus),
        "daemon must only see the nested bus"
    );
    // Setup is done in this profile, so no setup window steals the pad's focus.
    let cfg_dir = nested.dir.join("config/murmur");
    std::fs::create_dir_all(&cfg_dir).expect("config dir");
    std::fs::write(cfg_dir.join("config.toml"), "setup_done = true\n").expect("config");
    let log = std::fs::File::create(nested.dir.join("murmur.log")).expect("log file");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_murmur"))
        .env_clear()
        .envs(env)
        .env("WAYLAND_DISPLAY", &nested.socket)
        // models from the real data dir; everything else stays private
        .env("XDG_DATA_HOME", &data_home)
        .env("MURMUR_DEV_AUDIO", &wav)
        .env("MURMUR_DEV_EIS", "kwin-direct")
        .env("MURMUR_DEV_NO_HOTKEY", "1")
        .env("MURMUR_DEV_AUTORUN", "1")
        .env("RUST_LOG", "murmur=debug")
        .stdout(log.try_clone().expect("log fd"))
        .stderr(log)
        .spawn()
        .expect("start murmur");

    // The WAV is ~4 s; give load + transcription plenty of time.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = daemon.try_wait().expect("wait murmur") {
            break Some(s);
        }
        if std::time::Instant::now() > deadline {
            let _ = daemon.kill();
            break None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let log = nested.log_tail("murmur");
    let text = pad.settle(Duration::from_millis(500)).await;
    let typed = text.strip_prefix(&prefix).unwrap_or(&text).to_owned();
    println!("typed: {typed:?}\n--- murmur log ---\n{log}");
    assert!(
        status.is_some_and(|s| s.success()),
        "murmur did not exit cleanly"
    );
    assert_eq!(
        words(&typed),
        words("Ask not what your country can do for you, ask what you can do for your country."),
        "dictated text"
    );
    assert!(
        typed.ends_with(' '),
        "every word is followed by a space (D-002)"
    );
}

const IDLE_ICON: &str = "audio-input-microphone-symbolic";

async fn sni_name(conn: &zbus::Connection) -> String {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.expect("dbus proxy");
    for _ in 0..100 {
        let names = dbus.list_names().await.expect("list names");
        if let Some(n) = names
            .iter()
            .find(|n| n.as_str().starts_with("org.kde.StatusNotifierItem-"))
        {
            return n.to_string();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("murmur's tray item never appeared on the nested bus");
}

async fn sni_prop(conn: &zbus::Connection, name: &str, prop: &str) -> zbus::zvariant::OwnedValue {
    let reply = conn
        .call_method(
            Some(name),
            "/StatusNotifierItem",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.kde.StatusNotifierItem", prop),
        )
        .await
        .expect("get tray property");
    let v: zbus::zvariant::OwnedValue = reply.body().deserialize().expect("variant");
    v
}

async fn icon_name(conn: &zbus::Connection, name: &str) -> String {
    String::try_from(sni_prop(conn, name, "IconName").await).unwrap_or_default()
}

async fn wait_icon(conn: &zbus::Connection, name: &str, want_idle: bool) {
    for _ in 0..200 {
        let idle = icon_name(conn, name).await == IDLE_ICON;
        if idle == want_idle {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "tray icon never became {}",
        if want_idle { "idle" } else { "active" }
    );
}

async fn activate(conn: &zbus::Connection, name: &str) {
    conn.call_method(
        Some(name),
        "/StatusNotifierItem",
        Some("org.kde.StatusNotifierItem"),
        "Activate",
        &(0i32, 0i32),
    )
    .await
    .expect("activate tray item");
}

/// Left-click the tray icon to start, again to stop: the spec's "Clicks tray icon".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs kwin_wayland, the spike test pad and the models; see the file header"]
async fn dictation_via_tray_icon() {
    let models = murmur::paths::models_dir().expect("XDG data dir");
    let wav: PathBuf = models.join("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/test_wavs/en.wav");
    assert!(wav.is_file(), "models missing: {}", wav.display());
    let data_home = models
        .parent()
        .and_then(|p| p.parent())
        .expect("models dir has a data home")
        .to_path_buf();

    let mut nested = harness::Nested::start("us").await;
    let pad_path = nested.dir.join("pad.txt");
    nested.spawn_pad(&pad_path);
    let pad = harness::Pad::new(pad_path.clone());
    let mut kb = harness::RawKeyboard::connect(&nested.bus).await;
    for _ in 0..50 {
        kb.key(KEY_X, true).await;
        kb.key(KEY_X, false).await;
        if !pad.text().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let prefix = pad.settle(Duration::from_millis(300)).await;
    assert!(!prefix.is_empty(), "test pad never got focus");

    // Setup is done in this profile, so no setup window steals the pad's focus.
    let cfg_dir = nested.dir.join("config/murmur");
    std::fs::create_dir_all(&cfg_dir).expect("config dir");
    std::fs::write(cfg_dir.join("config.toml"), "setup_done = true\n").expect("config");
    let log = std::fs::File::create(nested.dir.join("murmur.log")).expect("log file");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_murmur"))
        .env_clear()
        .envs(nested.session_env())
        .env("WAYLAND_DISPLAY", &nested.socket)
        .env("XDG_DATA_HOME", &data_home)
        .env("MURMUR_DEV_AUDIO", &wav)
        .env("MURMUR_DEV_EIS", "kwin-direct")
        .env("MURMUR_DEV_NO_HOTKEY", "1")
        .env("RUST_LOG", "murmur=debug")
        .stdout(log.try_clone().expect("log fd"))
        .stderr(log)
        .spawn()
        .expect("start murmur");

    let conn = harness::connect(&nested.bus).await;
    let name = sni_name(&conn).await;
    assert_eq!(icon_name(&conn, &name).await, IDLE_ICON, "starts idle");

    activate(&conn, &name).await; // start
    wait_icon(&conn, &name, false).await;
    let pixmaps: Vec<(i32, i32, Vec<u8>)> = sni_prop(&conn, &name, "IconPixmap")
        .await
        .try_into()
        .expect("pixmap array");
    assert!(!pixmaps.is_empty(), "listening icon has pixmaps");
    // en.wav is ~3.9 s; leave time for the model to load and the phrase to end.
    tokio::time::sleep(Duration::from_secs(7)).await;
    activate(&conn, &name).await; // stop
    wait_icon(&conn, &name, true).await;

    let text = pad.settle(Duration::from_millis(500)).await;
    let typed = text.strip_prefix(&prefix).unwrap_or(&text).to_owned();
    let _ = Command::new("kill")
        .arg("-INT")
        .arg(daemon.id().to_string())
        .status();
    let status = daemon.wait().expect("murmur exit");
    println!(
        "typed: {typed:?}\n--- murmur log ---\n{}",
        nested.log_tail("murmur")
    );
    assert!(status.success(), "murmur exit status {status:?}");
    assert_eq!(
        words(&typed),
        words("Ask not what your country can do for you, ask what you can do for your country."),
    );
}
