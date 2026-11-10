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
