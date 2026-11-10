//! The real portal path (`EisSource::Portal`: RemoteDesktop → `ConnectToEIS`, the Clipboard
//! portal paste fallback and the restore token) against the host's xdg-desktop-portal and
//! xdg-desktop-portal-kde, running inside a nested, invisible KWin with a private D-Bus daemon.
//! The permission dialogs appear in the nested KWin and are answered with keys from a second
//! EIS client. Nothing reaches the real desktop: `assert_isolated` and `assert_portals_isolated`
//! check the bus, the display and the portal processes before anything is typed.
//!
//! Needs the host's `kwin_wayland`, `/usr/libexec/xdg-desktop-portal{,-kde}`,
//! `xdg-permission-store`, `wl-copy`/`wl-paste` and `spectacle` (from the toolbox, through
//! `flatpak-spawn --host`), and the spike's test pad (`cd spike && cargo build`). Run with:
//!
//! ```sh
//! cargo test --test portal_nested_kde -- --ignored --nocapture
//! # screenshots of the dialogs: MURMUR_SCREENSHOTS=<dir>; more detail: RUST_LOG=murmur=debug
//! ```

mod harness;

use std::time::{Duration, Instant};

use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
};
use murmur::output::{EisSource, Output, OutputEvent};
use tokio::sync::mpsc;

// Evdev keycodes (linux/input-event-codes.h).
const KEY_ESC: u32 = 1;
const KEY_ENTER: u32 = 28;
const KEY_CAPSLOCK: u32 = 58;
const WAIT: Duration = Duration::from_secs(5);
/// Stands in for a copied image: only the mime type matters.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really an image";
const UTF8: &str = "text/plain;charset=utf-8";

type Events = mpsc::UnboundedReceiver<OutputEvent>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the host's KWin, portals and wl-clipboard, and the spike test pad; see the file header"]
async fn portal_nested_kde() {
    let mut nested = harness::Nested::start("us,lt").await;
    nested.start_portals().await;
    let pad = nested.dir.join("pad.txt");
    nested.spawn_pad(&pad);

    let exe = std::env::current_exe().expect("test binary path");
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "portal_kde_scenarios",
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
        Duration::from_secs(300),
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
        for log in [
            "kwin-err",
            "permission-store-err",
            "portal-err",
            "portal-kde-err",
            "portal-kde-2-err",
            "pad-err",
        ] {
            eprintln!("--- {log} ---\n{}", nested.log_tail(log));
        }
    }
    assert!(ok, "portal scenarios failed (output above)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "started by portal_nested_kde inside the nested session"]
async fn portal_kde_scenarios() {
    let Some(env) = harness::ChildEnv::from_env() else {
        eprintln!("portal_kde_scenarios: skipped (run portal_nested_kde)");
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    env.assert_isolated().await;
    env.assert_portals_isolated().await;

    // 0. The portal offers what murmur needs.
    assert_eq!(
        harness::portal_version(&env.bus, "org.freedesktop.portal.RemoteDesktop").await,
        Some(2),
        "RemoteDesktop portal version"
    );
    assert!(
        harness::portal_version(&env.bus, "org.freedesktop.portal.Clipboard")
            .await
            .is_some_and(|v| v >= 1),
        "no Clipboard portal"
    );
    let pad = harness::Pad::new(env.pad.clone());
    let mut kb = harness::RawKeyboard::connect(&env.bus).await;
    let state = env.dir.join("state/murmur");
    let token = state.join("remote-desktop.token");

    // 1. First start: the permission dialog; approving it starts the session and saves a restore
    //    token. Something is on the clipboard already: KDE announces no owner at `Start`, so
    //    murmur doesn't know its mime types.
    env.copy(None, b"copied before the session").await;
    let (tx, mut events) = mpsc::unbounded_channel();
    let out = Output::spawn(
        EisSource::Portal {
            token_path: token.clone(),
        },
        tx,
    );
    harness::wait_portal_dialog(&env.bus, Duration::from_secs(20))
        .await
        .expect("no permission dialog on the first start");
    env.screenshot("portal-1-dialog").await;
    harness::answer_portal_dialog(&env.bus, &mut kb, KEY_ENTER).await;
    expect_event(&mut events, OutputEvent::Ready).await;
    assert!(token.is_file(), "no restore token saved");
    let mut want = focus(&out, &pad).await;

    // 2. Typing through the portal's EIS keyboard.
    type_words(&out, "Hello, World! ").await;
    want += "Hello, World! ";
    pad.expect(&want, WAIT).await;
    assert_no_unmapped(&mut events);

    // 3. `ą` isn't on the active (us) layout: the word is pasted, then the previous clipboard
    //    (whose owner murmur had to probe) is offered again.
    let started = Instant::now();
    type_words(&out, "mąstau ").await;
    want += "mąstau ";
    pad.expect(&want, WAIT).await;
    eprintln!("paste (owner unknown) took {:?}", started.elapsed());
    assert_no_unmapped(&mut events);
    assert_clipboard(&env, b"copied before the session").await;

    // 4. Another app copies during the session (KDE announces it): paste, restore that.
    env.copy(None, b"copied during the session").await;
    let started = Instant::now();
    type_words(&out, "ąžuolas ").await;
    want += "ąžuolas ";
    pad.expect(&want, WAIT).await;
    eprintln!("paste (owner announced) took {:?}", started.elapsed());
    assert_no_unmapped(&mut events);
    assert_clipboard(&env, b"copied during the session").await;

    // 5. Caps Lock on. Whether Ctrl+V pastes is up to the app: the Slint test pad matches the
    //    shortcut by its text ("v"), which Caps Lock makes "V", so it ignores it (Qt and GTK
    //    match the key). Either way the text is exact or only `Č` is missing, and the previous
    //    clipboard is back.
    kb.key(KEY_CAPSLOCK, true).await;
    kb.key(KEY_CAPSLOCK, false).await;
    type_words(&out, "Čia ").await;
    let text = pad.settle(Duration::from_millis(300)).await;
    let typed = text.strip_prefix(&want).unwrap_or(&text).to_owned();
    eprintln!("Caps Lock + paste: typed {typed:?}");
    match typed.as_str() {
        "Čia " => assert_no_unmapped(&mut events),
        "ia " => expect_event(&mut events, OutputEvent::Unmapped { count: 1 }).await,
        other => panic!("Caps Lock + paste typed {other:?}"),
    }
    want = text;
    assert_clipboard(&env, b"copied during the session").await;
    kb.key(KEY_CAPSLOCK, true).await;
    kb.key(KEY_CAPSLOCK, false).await;
    drop(out);
    // The task closes the session once its queue is empty.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 6. Restart with the token: no dialog (a dialog would block `Ready`, nobody answers it).
    //    The clipboard now holds an image, which the session again learns nothing about.
    env.copy(Some("image/png"), PNG).await;
    let (tx, mut events) = mpsc::unbounded_channel();
    let started = Instant::now();
    let out = Output::spawn(
        EisSource::Portal {
            token_path: token.clone(),
        },
        tx,
    );
    expect_event(&mut events, OutputEvent::Ready).await;
    eprintln!("restored session ready after {:?}", started.elapsed());
    assert!(
        harness::portal_dialogs(&env.bus).await.is_empty(),
        "a dialog appeared despite the restore token"
    );
    type_words(&out, "Hello again ").await;
    want += "Hello again ";
    pad.expect(&want, WAIT).await;

    // 7. An image on the clipboard can't be saved as text, so the word isn't pasted (D-012):
    //    typed without `ą`, reported, the image left alone, and without a long stall. The
    //    next such word doesn't ask the portal again (KDE never answers those reads).
    for (word, typed, unmapped, limit) in [
        ("mąstau ", "mstau ", 1, Duration::from_millis(1000)),
        ("ąžuolas ", "uolas ", 2, Duration::from_millis(200)),
    ] {
        let started = Instant::now();
        type_words(&out, word).await;
        let took = started.elapsed();
        want += typed;
        pad.expect(&want, WAIT).await;
        expect_event(&mut events, OutputEvent::Unmapped { count: unmapped }).await;
        eprintln!("skipped paste of {word:?} (image on the clipboard) took {took:?}");
        assert!(took < limit, "took {took:?}");
    }
    assert_eq!(env.clipboard_types().await, ["image/png"]);
    assert_eq!(env.paste("image/png").await.as_deref(), Some(PNG));

    // 8. The user ends remote control (the tray icon's "End"): the portal closes the session.
    //    The next word reconnects with the token, without a dialog.
    harness::end_remote_control(&env.bus).await;
    let event = wait_event(&mut events, "Disconnected", |e| {
        matches!(e, OutputEvent::Disconnected(_))
    })
    .await;
    eprintln!("after End: {event:?}");
    type_words(&out, "reconnected ").await;
    expect_event(&mut events, OutputEvent::Ready).await;
    want += "reconnected ";
    pad.expect(&want, WAIT).await;
    drop(out);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 9. A token granted before murmur asked for the clipboard: KDE asks again.
    env.copy(None, b"text again").await;
    let old_token = state.join("no-clipboard.token");
    std::fs::write(&old_token, token_without_clipboard(&env, &mut kb).await).expect("write token");
    let (tx, mut events) = mpsc::unbounded_channel();
    let out = Output::spawn(
        EisSource::Portal {
            token_path: old_token.clone(),
        },
        tx,
    );
    harness::wait_portal_dialog(&env.bus, Duration::from_secs(10))
        .await
        .expect("no dialog for a token without clipboard access");
    env.screenshot("portal-9-reprompt").await;
    harness::answer_portal_dialog(&env.bus, &mut kb, KEY_ENTER).await;
    expect_event(&mut events, OutputEvent::Ready).await;
    want = focus(&out, &pad).await;
    type_words(&out, "ąžuolas ").await;
    want += "ąžuolas ";
    pad.expect(&want, WAIT).await;
    assert_no_unmapped(&mut events);
    assert_clipboard(&env, b"text again").await;

    // 10. Denying the dialog.
    let (tx, mut denied) = mpsc::unbounded_channel();
    let refused = Output::spawn(
        EisSource::Portal {
            token_path: state.join("denied.token"),
        },
        tx,
    );
    harness::answer_portal_dialog(&env.bus, &mut kb, KEY_ESC).await;
    expect_event(&mut denied, OutputEvent::PermissionDenied).await;
    assert!(!state.join("denied.token").exists());
    drop(refused);

    // 11. The KDE backend dies under a live session: KWin drops its EIS clients. Without the
    //    backend the reconnect fails (here: there is no D-Bus activation to restart it) and the
    //    word is dropped. Once the backend is back, `retry` (the next dictation) reconnects with
    //    the token, without a dialog.
    harness::kill_portal_backend(&env.bus).await;
    let event = wait_event(&mut events, "Disconnected", |e| {
        matches!(e, OutputEvent::Disconnected(_))
    })
    .await;
    eprintln!("after killing the backend: {event:?}");
    out.type_text("orphan ".into());
    tokio::time::timeout(Duration::from_secs(10), out.flush())
        .await
        .expect("flush without a backend");
    let event = wait_event(&mut events, "a failed reconnect", |e| {
        !matches!(e, OutputEvent::Waiting(_))
    })
    .await;
    eprintln!("reconnecting without a backend: {event:?}");
    assert!(matches!(event, OutputEvent::Unavailable(_)), "{event:?}");
    assert_eq!(pad.settle(Duration::from_millis(300)).await, want);
    let _backend = env.restart_portal_backend().await;
    env.assert_portals_isolated().await;
    out.retry();
    expect_event(&mut events, OutputEvent::Ready).await;
    type_words(&out, "recovered ").await;
    want += "recovered ";
    pad.expect(&want, WAIT).await;
}

/// A restore token from a session that never asked for the clipboard (as tokens from before
/// D-011 were). Approves the dialog.
async fn token_without_clipboard(env: &harness::ChildEnv, kb: &mut harness::RawKeyboard) -> String {
    murmur::portal::register_host_app().await;
    let portal = RemoteDesktop::new().await.expect("RemoteDesktop portal");
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .expect("create session");
    portal
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::KEYBOARD)
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await
        .and_then(|r| r.response())
        .expect("select devices");
    let (started, ()) = tokio::join!(
        async {
            portal
                .start(&session, None, StartOptions::default())
                .await
                .and_then(|r| r.response())
        },
        harness::answer_portal_dialog(&env.bus, kb, KEY_ENTER),
    );
    let started = started.expect("start");
    assert!(!started.is_clipboard_enabled(), "clipboard granted unasked");
    let token = started.restore_token().expect("restore token").to_owned();
    session.close().await.expect("close session");
    token
}

async fn assert_clipboard(env: &harness::ChildEnv, want: &[u8]) {
    let got = env.paste(UTF8).await;
    assert_eq!(
        got.as_deref().map(String::from_utf8_lossy),
        Some(String::from_utf8_lossy(want)),
        "previous clipboard not restored"
    );
}

async fn type_words(out: &Output, text: &str) {
    for word in text.split_inclusive(' ') {
        out.type_text(word.to_owned());
    }
    tokio::time::timeout(WAIT, out.flush())
        .await
        .expect("flush timed out");
}

/// Types a probe word until it lands in the pad (which may get focus a little after a dialog
/// closes), then returns the settled text, which later checks extend.
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

/// Waits for the first event matching `pred`; other events are logged and skipped.
async fn wait_event(
    events: &mut Events,
    what: &str,
    pred: impl Fn(&OutputEvent) -> bool,
) -> OutputEvent {
    let got = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await {
                Some(e) if pred(&e) => return e,
                Some(e) => eprintln!("(event while waiting for {what}: {e:?})"),
                None => panic!("output event channel closed"),
            }
        }
    })
    .await;
    got.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Waits for `want`; a failure event first fails the test.
async fn expect_event(events: &mut Events, want: OutputEvent) {
    let failure = |e: &OutputEvent| {
        matches!(
            e,
            OutputEvent::Disconnected(_)
                | OutputEvent::Unavailable(_)
                | OutputEvent::PermissionDenied
        )
    };
    let got = wait_event(events, &format!("{want:?}"), |e| *e == want || failure(e)).await;
    assert_eq!(got, want, "unexpected output event");
}

fn assert_no_unmapped(events: &mut Events) {
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
