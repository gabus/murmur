//! The real portal path on GNOME: `EisSource::Portal` (RemoteDesktop → `ConnectToEIS`, the
//! Clipboard portal paste fallback, the restore token), the GlobalShortcuts portal
//! (`murmur::hotkey`) and the first-run setup window of the real daemon, against
//! xdg-desktop-portal, xdg-desktop-portal-gnome and gnome-control-center's shortcuts provider on
//! a headless gnome-shell with a private D-Bus daemon (`tests/harness/gnome.rs`). The dialogs are
//! answered with keys from a second EIS client. Nothing reaches the real desktop:
//! `assert_isolated` and `assert_portals_isolated` check the bus, the display, the runtime dir
//! and the portal processes before anything is typed.
//!
//! Needs, in the `murmur-dev` toolbox: gnome-shell, mutter, xdg-desktop-portal,
//! xdg-desktop-portal-gnome, xdg-permission-store, gnome-control-center (its
//! `global-shortcuts-provider`), gsettings-desktop-schemas, dbus-daemon, and gjs with GTK 4 for
//! the test pad (`tests/harness/gtk-testpad.js`). The setup-window test also needs the default model in the dev
//! model cache (`$XDG_DATA_HOME/murmur/models`), which it links into its private profile, so
//! nothing is downloaded; without it, that test is skipped. Run with:
//!
//! ```sh
//! cargo test --test portal_headless_gnome -- --ignored --nocapture
//! # screenshots: MURMUR_SCREENSHOTS=<dir>; more detail: RUST_LOG=murmur=debug
//! ```

mod harness;

use std::time::{Duration, Instant};

use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
};
use harness::{
    RawKeyboard, assert_no_unmapped, expect_event, focus,
    gnome::{self, Answer, PAD_TITLE, REMOTE_DESKTOP_DIALOG, SHORTCUTS_DIALOG},
    type_words, wait_event, wait_for,
};
use murmur::{
    hotkey::{Hotkey, HotkeyEvent},
    output::{EisSource, Output, OutputEvent},
};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);
/// Stands in for a copied image: only the mime type matters.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really an image";
const UTF8: &str = "text/plain;charset=utf-8";

/// One headless gnome-shell at a time: the tests share the CPU (software rendering).
static SESSION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Starts a headless session with the portals and a test pad, and runs `scenarios` in it.
async fn run_scenarios(scenarios: &str) {
    let _one = SESSION.lock().await;
    let mut session = gnome::Headless::start().await;
    session.start_portals().await;
    let pad = session.dir.join("pad.txt");
    session.spawn_pad(&pad);
    let cmd = session.scenario_command(scenarios, &pad);
    let ok = session.run_scenarios(cmd, Duration::from_secs(300)).await;
    assert!(ok, "{scenarios} failed (output above)");
}

/// The re-executed scenario's environment, after the isolation checks; `None` when the test
/// wasn't started by [`run_scenarios`].
async fn scenario_env(name: &str) -> Option<harness::ChildEnv> {
    let Some(env) = harness::ChildEnv::from_env() else {
        eprintln!("{name}: skipped (run by its *_headless_gnome test)");
        return None;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    env.assert_isolated().await;
    env.assert_portals_isolated().await;
    Some(env)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs gnome-shell, the GNOME portals and gjs in the toolbox; see the file header"]
async fn portal_headless_gnome() {
    run_scenarios("portal_gnome_scenarios").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "started by portal_headless_gnome inside the headless session"]
async fn portal_gnome_scenarios() {
    let Some(env) = scenario_env("portal_gnome_scenarios").await else {
        return;
    };
    let bus = env.bus.as_str();

    // 0. What the portal offers.
    for interface in [
        "RemoteDesktop",
        "Clipboard",
        "GlobalShortcuts",
        "Background",
        "Notification",
    ] {
        let version =
            harness::portal_version(bus, &format!("org.freedesktop.portal.{interface}")).await;
        eprintln!("portal {interface}: version {version:?}");
    }
    assert_eq!(
        harness::portal_version(bus, "org.freedesktop.portal.RemoteDesktop").await,
        Some(2),
        "RemoteDesktop portal version"
    );
    let devices = harness::portal_property(
        bus,
        "org.freedesktop.portal.RemoteDesktop",
        "AvailableDeviceTypes",
    )
    .await;
    eprintln!("RemoteDesktop AvailableDeviceTypes: {devices:?}");
    assert!(
        devices.is_some_and(|d| d & 1 != 0),
        "no keyboard device type"
    );
    assert!(
        harness::portal_version(bus, "org.freedesktop.portal.Clipboard")
            .await
            .is_some_and(|v| v >= 1),
        "no Clipboard portal"
    );
    // Settings → Your system (D-019) sees the same, and no tray host (no AppIndicator).
    let report = murmur::syscheck::probe().await;
    eprintln!("system check: {report:?}");
    assert!(
        report.wayland && report.portal && murmur::portal::is_gnome(Some(&report.desktop)),
        "{report:?}"
    );
    assert_eq!(
        (report.global_shortcuts, report.remote_desktop),
        (Some(1), Some(2)),
        "{report:?}"
    );
    assert!(
        report.remote_desktop_keyboard
            && report.clipboard.is_some()
            && report.notification.is_some()
            && report.background.is_some()
            && !report.tray,
        "{report:?}"
    );
    wait_for("no screen sharing", WAIT, || async {
        gnome::sharing_sessions(bus).await == 0
    })
    .await;
    let pad = harness::Pad::new(env.pad.clone());
    let state = env.dir.join("state/murmur");
    let token = state.join("remote-desktop.token");

    // 1. First start: the permission dialog; approving it starts the session and saves a restore
    //    token. Something is on the clipboard already.
    gnome::copy(bus, None, b"copied before the session").await;
    let (tx, mut events) = mpsc::unbounded_channel();
    let out = Output::spawn(
        EisSource::Portal {
            token_path: token.clone(),
        },
        tx,
    );
    let dialog = gnome::wait_dialog(bus, REMOTE_DESKTOP_DIALOG, Duration::from_secs(20))
        .await
        .expect("no permission dialog on the first start");
    eprintln!("dialog: {dialog:?}");
    gnome::screenshot(bus, &env.dir, "gnome-1-dialog").await;
    gnome::answer_remote_desktop(bus, Answer::Approve).await;
    expect_event(&mut events, OutputEvent::Ready).await;
    assert!(token.is_file(), "no restore token saved");
    // The panel's "Stop Screen Sharing" indicator shows murmur's session (the answering
    // keyboard's own session is gone).
    wait_for("murmur's session in the panel", WAIT, || async {
        gnome::sharing_sessions(bus).await == 1
    })
    .await;
    // The approved dialog may still be fading out here.
    gnome::screenshot(bus, &env.dir, "gnome-1-approved").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    gnome::screenshot(bus, &env.dir, "gnome-1-sharing").await;
    let pad_window = gnome::window(bus, PAD_TITLE)
        .await
        .expect("test pad window");
    if !pad_window.focused {
        gnome::activate(bus, pad_window.id).await;
    }
    let mut want = focus(&out, &pad).await;

    // 2. Typing through the portal's EIS keyboard.
    type_words(&out, "Hello, World! ").await;
    want += "Hello, World! ";
    pad.expect(&want, WAIT).await;
    assert_no_unmapped(&mut events);

    // 3. `ą` isn't on the active (us) layout: the word is pasted, then the previous clipboard is
    //    offered again.
    let started = Instant::now();
    type_words(&out, "mąstau ").await;
    want += "mąstau ";
    pad.expect(&want, WAIT).await;
    eprintln!(
        "paste (copied before the session) took {:?}",
        started.elapsed()
    );
    assert_no_unmapped(&mut events);
    assert_clipboard(bus, b"copied before the session").await;
    // Only what gnome-shell offered comes back (GNOME answers the other text types empty).
    assert_eq!(gnome::clipboard_types(bus).await, [UTF8]);

    // 4. Another app copies during the session: paste, restore that. The new owner reaches
    //    murmur through mutter, the backend and the frontend; give it a moment (a word pasted
    //    within those few milliseconds would restore murmur's older copy).
    gnome::copy(bus, None, b"copied during the session").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    type_words(&out, "ąžuolas ").await;
    want += "ąžuolas ";
    pad.expect(&want, WAIT).await;
    eprintln!(
        "paste (copied during the session) took {:?}",
        started.elapsed()
    );
    assert_no_unmapped(&mut events);
    assert_clipboard(bus, b"copied during the session").await;

    // 5. Caps Lock on: GTK matches Ctrl+V by key, so the paste still works (the Slint pad
    //    on KDE ignores it). Either way the text is exact or only `Č` is missing, and the
    //    previous clipboard is back.
    let mut kb = RawKeyboard::connect_mutter(bus).await;
    kb.tap(&[gnome::KEY_CAPSLOCK]).await;
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
    assert_clipboard(bus, b"copied during the session").await;
    kb.tap(&[gnome::KEY_CAPSLOCK]).await;
    drop(kb);
    drop(out);
    // The task closes the session once its queue is empty.
    wait_for("murmur's session to end", WAIT, || async {
        gnome::sharing_sessions(bus).await == 0
    })
    .await;

    // 6. Restart with the token: no dialog (a dialog would block `Ready`, nobody answers it).
    //    The clipboard now holds an image.
    gnome::copy(bus, Some("image/png"), PNG).await;
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
        gnome::dialog(bus, REMOTE_DESKTOP_DIALOG).await.is_none(),
        "a dialog appeared despite the restore token"
    );
    type_words(&out, "Hello again ").await;
    want += "Hello again ";
    pad.expect(&want, WAIT).await;

    // 7. An image copied before the session can't be saved as text (GNOME reads every text type
    //    as empty), so the word isn't pasted (D-012): typed without `ą`, reported, the image
    //    left alone. The next such word doesn't ask the portal again.
    for (word, typed, unmapped) in [("mąstau ", "mstau ", 1), ("ąžuolas ", "uolas ", 2)] {
        let started = Instant::now();
        type_words(&out, word).await;
        eprintln!(
            "skipped paste of {word:?} (image on the clipboard) took {:?}",
            started.elapsed()
        );
        want += typed;
        pad.expect(&want, WAIT).await;
        expect_event(&mut events, OutputEvent::Unmapped { count: unmapped }).await;
    }
    assert_eq!(gnome::clipboard_types(bus).await, ["image/png"]);
    assert_eq!(gnome::paste(bus, "image/png").await.as_deref(), Some(PNG));

    // 8. The user ends remote control: the panel's "Stop Screen Sharing" button. The next word
    //    reconnects with the token, without a dialog.
    assert_eq!(
        gnome::sharing_sessions(bus).await,
        1,
        "only murmur's session"
    );
    gnome::stop_sharing(bus).await;
    let event = wait_event(&mut events, "Disconnected", |e| {
        matches!(e, OutputEvent::Disconnected(_))
    })
    .await;
    eprintln!("after Stop Screen Sharing: {event:?}");
    gnome::screenshot(bus, &env.dir, "gnome-8-stopped").await;
    type_words(&out, "reconnected ").await;
    expect_event(&mut events, OutputEvent::Ready).await;
    want += "reconnected ";
    pad.expect(&want, WAIT).await;
    assert!(gnome::dialog(bus, REMOTE_DESKTOP_DIALOG).await.is_none());
    drop(out);
    wait_for("murmur's session to end", WAIT, || async {
        gnome::sharing_sessions(bus).await == 0
    })
    .await;

    // 9. A token granted before murmur asked for the clipboard: unlike KDE, GNOME doesn't ask
    //    again; it restores the session without clipboard access, so nothing is pasted.
    gnome::copy(bus, None, b"text again").await;
    let old_token = state.join("no-clipboard.token");
    std::fs::write(&old_token, token_without_clipboard(bus).await).expect("write token");
    let (tx, mut events) = mpsc::unbounded_channel();
    let out = Output::spawn(
        EisSource::Portal {
            token_path: old_token.clone(),
        },
        tx,
    );
    expect_event(&mut events, OutputEvent::Ready).await;
    assert!(
        gnome::dialog(bus, REMOTE_DESKTOP_DIALOG).await.is_none(),
        "GNOME asked again for a token without clipboard access"
    );
    want = focus(&out, &pad).await;
    type_words(&out, "ąžuolas ").await;
    want += "uolas ";
    pad.expect(&want, WAIT).await;
    expect_event(&mut events, OutputEvent::Unmapped { count: 2 }).await;
    assert_clipboard(bus, b"text again").await;

    // 10. Denying the dialog.
    let (tx, mut denied) = mpsc::unbounded_channel();
    let refused = Output::spawn(
        EisSource::Portal {
            token_path: state.join("denied.token"),
        },
        tx,
    );
    gnome::wait_dialog(bus, REMOTE_DESKTOP_DIALOG, Duration::from_secs(20))
        .await
        .expect("no dialog for a new session");
    gnome::answer_remote_desktop(bus, Answer::Deny).await;
    expect_event(&mut denied, OutputEvent::PermissionDenied).await;
    assert!(!state.join("denied.token").exists());
    drop(refused);

    // 11. The GNOME backend dies under a live session. Without the backend the reconnect fails
    //    (here: there is no D-Bus activation to restart it) and the word is dropped. Once the
    //    backend is back, `retry` (the next dictation) reconnects with the token, without a
    //    dialog. (xdg-desktop-portal 1.22.1 then refuses RequestClipboard with "Invalid state"
    //    until it restarts itself, so there's no pasting; typing works.)
    gnome::kill_gnome_backend(bus).await;
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
    let _backend = env.restart_gnome_backend().await;
    env.assert_portals_isolated().await;
    out.retry();
    expect_event(&mut events, OutputEvent::Ready).await;
    assert!(gnome::dialog(bus, REMOTE_DESKTOP_DIALOG).await.is_none());
    type_words(&out, "recovered ").await;
    want += "recovered ";
    pad.expect(&want, WAIT).await;
}

/// A restore token from a session that never asked for the clipboard (as tokens from before
/// D-011 were). Approves the dialog.
async fn token_without_clipboard(bus: &str) -> String {
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
        gnome::answer_remote_desktop(bus, Answer::Approve),
    );
    let started = started.expect("start");
    assert!(!started.is_clipboard_enabled(), "clipboard granted unasked");
    let token = started.restore_token().expect("restore token").to_owned();
    session.close().await.expect("close session");
    token
}

async fn assert_clipboard(bus: &str, want: &[u8]) {
    let got = gnome::paste(bus, UTF8).await;
    assert_eq!(
        got.as_deref().map(String::from_utf8_lossy),
        Some(String::from_utf8_lossy(want)),
        "previous clipboard not restored"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs gnome-shell, the GNOME portals and gjs in the toolbox; see the file header"]
async fn shortcuts_headless_gnome() {
    run_scenarios("shortcuts_gnome_scenarios").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "started by shortcuts_headless_gnome inside the headless session"]
async fn shortcuts_gnome_scenarios() {
    let Some(env) = scenario_env("shortcuts_gnome_scenarios").await else {
        return;
    };
    let bus = env.bus.as_str();
    // Why murmur suggests Super+Alt+H on GNOME (D-016): Super+H is "Hide window".
    assert!(!gnome::accelerator_free(bus, "<Super>h").await);
    assert!(gnome::accelerator_free(bus, "<Alt><Super>h").await);

    // 1. Binding shows gnome-control-center's dialog with the suggested key.
    let (tx, mut hotkey_events) = mpsc::unbounded_channel();
    let hotkey = Hotkey::spawn(tx);
    gnome::wait_dialog(bus, SHORTCUTS_DIALOG, Duration::from_secs(20))
        .await
        .expect("no shortcuts dialog");
    gnome::screenshot(bus, &env.dir, "gnome-shortcuts-dialog").await;
    gnome::answer_shortcuts(bus, Answer::Approve).await;
    let bound = next_hotkey(&mut hotkey_events).await;
    eprintln!("bound: {bound:?}");
    assert_eq!(
        bound,
        HotkeyEvent::Bound {
            trigger: Some("Alt+Super+H".into()),
            // GNOME's GlobalShortcuts portal is version 1: no ConfigureShortcuts.
            can_configure: false,
        }
    );

    // 2. The key, pressed on a keyboard (an EIS client), toggles. A second press after the
    //    debounce time toggles again.
    let mut kb = RawKeyboard::connect_mutter(bus).await;
    for _ in 0..2 {
        let pressed = Instant::now();
        kb.tap(&[gnome::KEY_LEFTMETA, gnome::KEY_LEFTALT, gnome::KEY_H])
            .await;
        assert_eq!(next_hotkey(&mut hotkey_events).await, HotkeyEvent::Toggle);
        eprintln!("Toggle {:?} after the key press", pressed.elapsed());
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    drop(kb);

    // 3. "Change…" can't open GNOME's dialog: the portal has no ConfigureShortcuts.
    hotkey.configure();
    let failed = next_hotkey(&mut hotkey_events).await;
    eprintln!("configure: {failed:?}");
    assert!(
        matches!(failed, HotkeyEvent::ConfigureFailed(_)),
        "{failed:?}"
    );

    // 4. The next start (a new portal session) gets the binding back without a dialog. (Here the
    //    binding lives in the harness's in-memory GSettings; across logins is untested.)
    drop(hotkey);
    let (tx, mut hotkey_events) = mpsc::unbounded_channel();
    let _hotkey = Hotkey::spawn(tx);
    let bound = next_hotkey(&mut hotkey_events).await;
    assert!(
        gnome::dialog(bus, SHORTCUTS_DIALOG).await.is_none(),
        "the shortcuts dialog appeared again"
    );
    assert_eq!(
        bound,
        HotkeyEvent::Bound {
            trigger: Some("Alt+Super+H".into()),
            can_configure: false,
        }
    );
    let mut kb = RawKeyboard::connect_mutter(bus).await;
    kb.tap(&[gnome::KEY_LEFTMETA, gnome::KEY_LEFTALT, gnome::KEY_H])
        .await;
    assert_eq!(next_hotkey(&mut hotkey_events).await, HotkeyEvent::Toggle);
}

async fn next_hotkey(events: &mut mpsc::UnboundedReceiver<HotkeyEvent>) -> HotkeyEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("no hotkey event within 10 s")
        .expect("hotkey channel closed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs gnome-shell and the GNOME portals in the toolbox, and the dev model cache; see the file header"]
async fn setup_window_headless_gnome() {
    // The default model from the dev model cache, so that nothing is downloaded.
    let cache = murmur::paths::models_dir().expect("models dir");
    let model = murmur::models::find(murmur::models::DEFAULT_MODEL).expect("default model");
    if !model.is_installed(&cache) {
        eprintln!(
            "setup_window_headless_gnome: skipped (no {} in {})",
            model.id,
            cache.display()
        );
        return;
    }
    let _one = SESSION.lock().await;
    let mut session = gnome::Headless::start().await;
    session.start_portals().await;
    session.assert_isolated().await;
    let models = session.dir.join("data/murmur/models");
    std::fs::create_dir_all(&models).expect("create models dir");
    for entry in std::fs::read_dir(&cache).expect("read model cache") {
        let entry = entry.expect("model cache entry");
        std::os::unix::fs::symlink(entry.path(), models.join(entry.file_name()))
            .expect("link model");
    }
    let bus = session.bus.clone();
    let shots = session.dir.clone();

    // A fresh profile: first run, so the setup window opens and the shortcut is bound.
    session.spawn(
        "murmur",
        env!("CARGO_BIN_EXE_murmur"),
        &[],
        &[("RUST_LOG", "murmur=debug,info")],
    );
    gnome::wait_dialog(&bus, SHORTCUTS_DIALOG, Duration::from_secs(30))
        .await
        .expect("no shortcuts dialog");
    let window = |title: &'static str| {
        let bus = bus.clone();
        async move { gnome::window(&bus, title).await.is_some() }
    };
    wait_for("the setup window", Duration::from_secs(30), || {
        window("murmur")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    gnome::screenshot(&bus, &shots, "gnome-setup-1").await;
    gnome::answer_shortcuts(&bus, Answer::Approve).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    gnome::screenshot(&bus, &shots, "gnome-setup-2").await;
    eprintln!("windows: {:?}", gnome::windows(&bus).await);

    // No tray host on GNOME without the AppIndicator extension: murmur keeps running.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let log = std::fs::read_to_string(session.dir.join("murmur.log")).unwrap_or_default();
    let alive = session.running("murmur");
    if !alive || !log.contains("no system tray available") {
        eprintln!("--- murmur ---\n{}", session.log_tail("murmur"));
    }
    assert!(alive, "murmur exited");
    assert!(
        log.contains("no system tray available"),
        "no watcher_offline"
    );
    assert!(
        log.contains("shortcut bound") && log.contains("Alt+Super+H"),
        "shortcut not bound as Alt+Super+H"
    );
    assert!(session.stop("murmur").is_none(), "murmur exited");
}
