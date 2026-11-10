//! Output integration tests against a nested, invisible KWin. Nothing is typed into the real
//! desktop: KWin runs with `--virtual` on its own Wayland socket and a private D-Bus daemon.
//!
//! Needs `kwin_wayland` (in the toolbox, the host's is used through `flatpak-spawn --host`)
//! and the spike's test pad (`cd spike && cargo build`, or set `MURMUR_TESTPAD` to the spike
//! binary). Run with:
//!
//! ```sh
//! cargo test --test output_nested_kwin -- --ignored --nocapture
//! # more detail: RUST_LOG=murmur=trace; keep the session dir: MURMUR_KEEP_NESTED=1
//! ```
//!
//! `nested_kwin_typing` starts the nested session, then re-runs this test binary with the
//! nested bus and display as its session environment (`nested_kwin_scenarios`). The code under
//! test (`EisSource::KwinDirect`, "the current session bus") can therefore only reach the
//! nested KWin, and `assert_isolated` checks that before anything is typed.

mod harness;

use std::time::Duration;

use murmur::output::{EisSource, Output, OutputEvent};
use tokio::sync::mpsc;

// Evdev keycodes (linux/input-event-codes.h).
const KEY_LEFTCTRL: u32 = 29;
const KEY_CAPSLOCK: u32 = 58;
const SENTENCE: &str = "The Quick Brown Fox? Hello, World! (ok) \"quoted\" 123 ";
const WAIT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs kwin_wayland and the spike test pad; see the file header"]
async fn nested_kwin_typing() {
    let mut nested = harness::Nested::start("us,lt").await;
    let pad = nested.dir.join("pad.txt");
    nested.spawn_pad(&pad);

    let exe = std::env::current_exe().expect("test binary path");
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "nested_kwin_scenarios",
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env("DBUS_SESSION_BUS_ADDRESS", &nested.bus)
    .env("WAYLAND_DISPLAY", &nested.socket)
    .env_remove("DISPLAY")
    .env(harness::ENV_DIR, &nested.dir)
    .env(harness::ENV_SOCKET, &nested.socket)
    .env(harness::ENV_BUS, &nested.bus)
    .env(harness::ENV_PAD, &pad)
    .env(
        harness::ENV_OUTER_BUS,
        std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default(),
    );
    let mut child = cmd.spawn().expect("re-run test binary");
    let pid = child.id();
    let waited = tokio::time::timeout(
        Duration::from_secs(180),
        tokio::task::spawn_blocking(move || child.wait()),
    )
    .await;
    let ok = match waited {
        Ok(Ok(Ok(status))) => status.success(),
        _ => {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            false
        }
    };
    if !ok {
        eprintln!("--- kwin log ---\n{}", nested.log_tail("kwin-err"));
        eprintln!("--- pad log ---\n{}", nested.log_tail("pad-err"));
    }
    assert!(ok, "nested scenarios failed (output above)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "started by nested_kwin_typing inside the nested session"]
async fn nested_kwin_scenarios() {
    let Some(env) = harness::ChildEnv::from_env() else {
        eprintln!("nested_kwin_scenarios: skipped (run nested_kwin_typing)");
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    env.assert_isolated().await;

    // 0. The user is on the second layout before murmur connects. KWin reports the group only
    //    when it changes, so Output must learn it (probe tap) before typing the first word.
    harness::set_layout(&env.bus, 1).await;
    let pad = harness::Pad::new(env.pad.clone());
    let (tx, mut events) = mpsc::unbounded_channel();
    let out = Output::spawn(EisSource::KwinDirect, tx);
    expect_event(&mut events, OutputEvent::Ready).await;
    // "probe " uses the same keys on us and lt.
    let mut want = focus(&out, &pad).await;
    // Lithuanian letters; `!` and `1` need AltGr (+Shift) on lt.
    let lt = "Ačiū, ąžuole! 1 ";
    type_words(&out, lt).await;
    want += lt;
    pad.expect(&want, WAIT).await;
    assert_no_unmapped(&mut events);
    harness::set_layout(&env.bus, 0).await;

    // 1. Mixed case, punctuation, quotes and digits, one word at a time like the ASR sends.
    type_words(&out, SENTENCE).await;
    want += SENTENCE;
    pad.expect(&want, WAIT).await;
    assert_no_unmapped(&mut events);

    // 2. `ą` is not on the active (us) layout: skipped and reported.
    type_words(&out, "mąstau ").await;
    want += "mstau ";
    pad.expect(&want, WAIT).await;
    expect_event(&mut events, OutputEvent::Unmapped { count: 1 }).await;

    // 3. Caps Lock (set by the "physical" keyboard) doesn't change what gets typed.
    let mut kb = harness::RawKeyboard::connect(&env.bus).await;
    kb.key(KEY_CAPSLOCK, true).await;
    kb.key(KEY_CAPSLOCK, false).await;
    let caps = "Caps Lock: Still Fine? ";
    type_words(&out, caps).await;
    want += caps;
    pad.expect(&want, WAIT).await;
    kb.key(KEY_CAPSLOCK, true).await;
    kb.key(KEY_CAPSLOCK, false).await;

    // 4. While the user holds Ctrl, words wait; they appear once it's released.
    kb.key(KEY_LEFTCTRL, true).await;
    kb.wait_mods("Ctrl down", |depressed| depressed != 0).await;
    out.type_text("held ".into());
    out.type_text("back ".into());
    assert!(
        tokio::time::timeout(Duration::from_millis(800), out.flush())
            .await
            .is_err(),
        "text was typed while Ctrl was held"
    );
    assert_eq!(pad.text(), want, "nothing may be typed while Ctrl is held");
    kb.key(KEY_LEFTCTRL, false).await;
    tokio::time::timeout(WAIT, out.flush())
        .await
        .expect("held text typed after Ctrl was released");
    want += "held back ";
    pad.expect(&want, WAIT).await;

    // 5. Keymap change: KWin replaces the EIS keyboard device. The new keymap is used, and the
    //    new device's state is learned again (no modifiers event comes by itself).
    harness::set_layout(&env.bus, 1).await;
    std::fs::write(
        env.dir.join("config/kxkbrc"),
        "[Layout]\nLayoutList=us,de\nUse=true\n",
    )
    .expect("write kxkbrc");
    harness::reload_keyboard_config(&env.bus).await;
    expect_event(&mut events, OutputEvent::Ready).await;
    let (layouts, active) = harness::layouts(&env.bus).await;
    assert_eq!(layouts, ["us", "de"]);
    let (typed, skipped) = if active == 1 {
        ("Grüße @ ", 0)
    } else {
        ("Gre @ ", 2)
    };
    type_words(&out, "Grüße @ ").await;
    want += typed;
    pad.expect(&want, WAIT).await;
    if skipped > 0 {
        expect_event(&mut events, OutputEvent::Unmapped { count: skipped }).await;
    }
    // And after switching to German explicitly, `ü`, `ß` and `@` (AltGr+Q) type.
    harness::set_layout(&env.bus, 1).await;
    type_words(&out, "Grüße @ ").await;
    want += "Grüße @ ";
    pad.expect(&want, WAIT).await;
    assert_no_unmapped(&mut events);
}

async fn type_words(out: &Output, text: &str) {
    for word in text.split_inclusive(' ') {
        out.type_text(word.to_owned());
    }
    tokio::time::timeout(WAIT, out.flush())
        .await
        .expect("flush timed out");
}

/// The pad maps and takes focus asynchronously. Types a probe word until it lands, then
/// returns the settled text, which all later checks extend.
async fn focus(out: &Output, pad: &harness::Pad) -> String {
    for _ in 0..20 {
        out.type_text("probe ".into());
        out.flush().await;
        let landed = tokio::time::timeout(Duration::from_secs(1), async {
            while !pad.text().ends_with("probe ") {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        if landed.is_ok() {
            return pad.settle(Duration::from_millis(300)).await;
        }
    }
    panic!("the test pad never got keyboard focus");
}

async fn expect_event(events: &mut mpsc::UnboundedReceiver<OutputEvent>, want: OutputEvent) {
    let got = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await {
                Some(e) if e == want => return,
                Some(
                    e @ (OutputEvent::Disconnected(_)
                    | OutputEvent::Unavailable(_)
                    | OutputEvent::PermissionDenied),
                ) => panic!("unexpected output event {e:?} while waiting for {want:?}"),
                Some(_) => {}
                None => panic!("output event channel closed"),
            }
        }
    })
    .await;
    assert!(got.is_ok(), "timed out waiting for {want:?}");
}

fn assert_no_unmapped(events: &mut mpsc::UnboundedReceiver<OutputEvent>) {
    while let Ok(e) = events.try_recv() {
        assert!(
            !matches!(
                e,
                OutputEvent::Unmapped { .. }
                    | OutputEvent::Disconnected(_)
                    | OutputEvent::Unavailable(_)
            ),
            "unexpected output event {e:?}"
        );
    }
}
