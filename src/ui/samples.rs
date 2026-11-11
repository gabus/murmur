//! Sample states for the screenshot example (`examples/ui_shots.rs`) and the tests.

use super::protocol::{DownloadRow, ModelRow, ShortcutState, SystemReport, TypingState, UiState};

fn row(id: &str, name: &str, summary: &str, size: &str) -> ModelRow {
    ModelRow {
        id: id.into(),
        name: name.into(),
        summary: summary.into(),
        size: size.into(),
        installed: false,
        active: false,
    }
}

/// The model catalogue as the daemon describes it (README → Supported models).
pub fn models() -> Vec<ModelRow> {
    let mut m = vec![
        row(
            "parakeet-v3",
            "Parakeet v3",
            "Most accurate · 25 European languages",
            "490 MB",
        ),
        row(
            "moonshine-base-en",
            "Moonshine base",
            "Lighter · English",
            "110 MB",
        ),
    ];
    m[0].active = true;
    m
}

/// First run: shortcut bound, typing not asked yet, default model at 45 %.
pub fn setup_fresh() -> UiState {
    UiState {
        shortcut: ShortcutState {
            trigger: Some("Meta+H".into()),
            can_change: true,
            pending: false,
            error: None,
        },
        typing: TypingState::Unknown,
        models: models(),
        download: Some(DownloadRow {
            model_id: "parakeet-v3".into(),
            percent: 45,
            unpacking: false,
            error: None,
        }),
        microphones: vec![String::new()],
        microphone: String::new(),
        pause_secs: 0.8,
        idle_minutes: 10,
        autostart: false,
        autostart_error: None,
        system: None,
    }
}

/// A Plasma 6 desktop that has everything (Settings → Your system).
pub fn system_ok() -> SystemReport {
    SystemReport {
        wayland: true,
        desktop: "KDE".into(),
        kde_version: Some(6),
        portal: true,
        global_shortcuts: Some(2),
        remote_desktop: Some(2),
        remote_desktop_keyboard: true,
        clipboard: Some(1),
        audio_server: true,
        microphones: 2,
        tray: true,
    }
}

/// Everything finished.
pub fn setup_done() -> UiState {
    let mut s = setup_fresh();
    s.typing = TypingState::Granted;
    s.download = None;
    s.models[0].installed = true;
    s.autostart = true;
    s
}

/// The user refused the typing permission.
pub fn typing_denied() -> UiState {
    let mut s = setup_done();
    s.typing = TypingState::Denied;
    s.autostart = false;
    s
}

/// The download failed; the shortcut dialog is open.
pub fn download_error() -> UiState {
    let mut s = setup_fresh();
    s.shortcut.trigger = None;
    s.shortcut.pending = true;
    s.typing = TypingState::Asking;
    s.download = Some(DownloadRow {
        model_id: "parakeet-v3".into(),
        percent: 12,
        unpacking: false,
        error: Some("connection reset by peer".into()),
    });
    s
}

/// No GlobalShortcuts or RemoteDesktop portal (e.g. an older desktop).
pub fn unsupported_desktop() -> UiState {
    let mut s = setup_done();
    s.shortcut = ShortcutState {
        trigger: None,
        can_change: false,
        pending: false,
        error: Some(
            "This desktop has no global shortcut support. Click the tray icon to dictate.".into(),
        ),
    };
    s.typing = TypingState::Unavailable(
        "This desktop can't let apps type (needs Plasma 6.1+ or GNOME 46+).".into(),
    );
    s.autostart_error = Some("The desktop refused to add Murmur to autostart.".into());
    s
}

/// Settings with a second model downloading.
pub fn settings() -> UiState {
    let mut s = setup_done();
    s.microphones = vec![
        String::new(),
        "Blue Yeti Analog Stereo".into(),
        "Built-in Audio Analog Stereo".into(),
    ];
    s.microphone = "Blue Yeti Analog Stereo".into();
    s.download = Some(DownloadRow {
        model_id: "moonshine-base-en".into(),
        percent: 62,
        unpacking: false,
        error: None,
    });
    s.system = Some(system_ok());
    s
}

/// Settings with no download and a failed autostart change, on GNOME without the
/// AppIndicator extension.
pub fn settings_idle() -> UiState {
    let mut s = settings();
    s.download = None;
    s.autostart = true;
    s.autostart_error = Some("The Background portal refused the request.".into());
    s.system = Some(SystemReport {
        desktop: "GNOME".into(),
        kde_version: None,
        global_shortcuts: Some(1),
        tray: false,
        ..system_ok()
    });
    s
}
