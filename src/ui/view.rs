//! What the windows show, derived from [`UiState`] by pure functions.
//!
//! Everything here is plain data so it can be unit-tested without a display. `window.rs`
//! copies these values into the Slint components.

use super::{
    checks::{self, Check},
    protocol::{DownloadRow, ModelRow, ShortcutState, TypingState, UiState},
};

/// Lower and upper bound of the "sentence ends after a pause" setting, in seconds.
pub const PAUSE_MIN: f32 = 0.4;
pub const PAUSE_MAX: f32 = 2.0;

/// Idle-unload choices offered in Settings, in minutes (0 = never).
const IDLE_CHOICES: [u32; 5] = [1, 5, 10, 30, 0];

/// The round status icon in front of each setup row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    /// Nothing done yet (empty circle).
    #[default]
    Todo,
    /// Waiting for a dialog or a download (spinner).
    Busy,
    /// Ready (check mark).
    Done,
    /// Failed or not possible (exclamation mark).
    Failed,
}

/// A line of secondary text under a row title.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Detail {
    pub text: String,
    /// Shown in the error colour.
    pub error: bool,
}

impl Detail {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShortcutView {
    pub status: Status,
    /// The key combination, or "" while none is bound.
    pub trigger: String,
    pub detail: Detail,
    /// The Change button is enabled.
    pub can_change: bool,
}

pub fn shortcut(s: &ShortcutState) -> ShortcutView {
    let trigger = s.trigger.clone().unwrap_or_default();
    let (status, detail) = if s.pending {
        (
            Status::Busy,
            Detail::info("Waiting for the shortcut dialog…"),
        )
    } else if let Some(e) = &s.error {
        (Status::Failed, Detail::error(e.clone()))
    } else if s.trigger.is_some() {
        let text = if s.can_change {
            "Starts and stops dictation in any app."
        } else {
            "Change it in your desktop's shortcut settings."
        };
        (Status::Done, Detail::info(text))
    } else {
        (
            Status::Todo,
            Detail::info("Pick a key to start and stop dictating."),
        )
    };
    ShortcutView {
        status,
        trigger,
        detail,
        can_change: s.can_change && !s.pending,
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TypingView {
    pub status: Status,
    pub detail: Detail,
    /// Label of the action button, or "" for none. The button sends `AllowTyping`.
    pub button: String,
}

pub fn typing(t: &TypingState) -> TypingView {
    let (status, detail, button) = match t {
        TypingState::Unknown => (
            Status::Todo,
            Detail::info("Lets Murmur type your words into other apps."),
            "Allow…",
        ),
        TypingState::Asking => (
            Status::Busy,
            Detail::info("Answer the system dialog to continue."),
            "",
        ),
        TypingState::Granted => (
            Status::Done,
            Detail::info("Murmur can type into other apps."),
            "",
        ),
        TypingState::Denied => (
            Status::Failed,
            Detail::error("Not allowed. Murmur can't type without it."),
            "Try again",
        ),
        TypingState::Unavailable(msg) => (Status::Failed, Detail::error(msg.clone()), ""),
    };
    TypingView {
        status,
        detail,
        button: button.into(),
    }
}

/// A progress bar.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Bar {
    /// 0.0 to 1.0.
    pub progress: f32,
    /// Moving bar without a known end (unpacking).
    pub indeterminate: bool,
}

/// The button next to the model on the setup page.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ModelAction {
    #[default]
    None,
    /// Retry the failed download of this model (`RetryDownload`).
    Retry(String),
    /// Start downloading this model (`SelectModel`).
    Download(String),
}

impl ModelAction {
    pub fn label(&self) -> &'static str {
        match self {
            ModelAction::None => "",
            ModelAction::Retry(_) => "Retry",
            ModelAction::Download(_) => "Download",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelView {
    pub status: Status,
    /// Progress shown in the status icon while busy; `None` spins.
    pub ring: Option<f32>,
    pub detail: Detail,
    pub bar: Option<Bar>,
    pub action: ModelAction,
}

/// The model the setup row is about: the one being downloaded, else the active one.
fn focus_model<'a>(models: &'a [ModelRow], download: Option<&DownloadRow>) -> Option<&'a ModelRow> {
    download
        .and_then(|d| models.iter().find(|m| m.id == d.model_id))
        .or_else(|| models.iter().find(|m| m.active))
        .or_else(|| models.first())
}

fn percent_fraction(percent: u8) -> f32 {
    f32::from(percent.min(100)) / 100.0
}

pub fn model(state: &UiState) -> ModelView {
    let Some(m) = focus_model(&state.models, state.download.as_ref()) else {
        return ModelView {
            status: Status::Failed,
            detail: Detail::error("No speech models are available."),
            ..Default::default()
        };
    };
    let download = state.download.as_ref().filter(|d| d.model_id == m.id);
    match download {
        Some(d) if d.error.is_some() => ModelView {
            status: Status::Failed,
            ring: None,
            detail: Detail::error(format!(
                "{} · Download failed: {}",
                m.name,
                d.error.as_deref().unwrap_or_default()
            )),
            bar: None,
            action: ModelAction::Retry(m.id.clone()),
        },
        Some(d) if d.unpacking => ModelView {
            status: Status::Busy,
            ring: None,
            detail: Detail::info(format!("{} · {} · Unpacking…", m.name, m.size)),
            bar: Some(Bar {
                progress: 1.0,
                indeterminate: true,
            }),
            action: ModelAction::None,
        },
        Some(d) => {
            let progress = percent_fraction(d.percent);
            ModelView {
                status: Status::Busy,
                ring: Some(progress),
                detail: Detail::info(format!(
                    "{} · {} · Downloading… {} %",
                    m.name,
                    m.size,
                    d.percent.min(100)
                )),
                bar: Some(Bar {
                    progress,
                    indeterminate: false,
                }),
                action: ModelAction::None,
            }
        }
        None if m.installed => ModelView {
            status: Status::Done,
            ring: None,
            detail: Detail::info(format!("{} · {} · Ready", m.name, m.size)),
            bar: None,
            action: ModelAction::None,
        },
        None => ModelView {
            status: Status::Todo,
            ring: None,
            detail: Detail::info(format!("{} · {} · Not downloaded yet", m.name, m.size)),
            bar: None,
            action: ModelAction::Download(m.id.clone()),
        },
    }
}

/// One entry of the setup page's model list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub size: String,
    /// "In use", "Downloaded" or "".
    pub note: String,
    pub active: bool,
}

fn model_choices(models: &[ModelRow]) -> Vec<ModelChoice> {
    models
        .iter()
        .map(|m| ModelChoice {
            id: m.id.clone(),
            name: m.name.clone(),
            summary: m.summary.clone(),
            size: m.size.clone(),
            note: if m.active && m.installed {
                "In use".into()
            } else if m.installed {
                "Downloaded".into()
            } else {
                String::new()
            },
            active: m.active,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SetupView {
    pub shortcut: ShortcutView,
    pub typing: TypingView,
    pub model: ModelView,
    pub models: Vec<ModelChoice>,
    pub autostart: bool,
    pub autostart_error: String,
    pub done_enabled: bool,
    /// What's still missing, shown next to Done while it's disabled.
    pub done_hint: String,
}

/// "a", "a and b", "a, b and c".
fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

pub fn setup(state: &UiState) -> SetupView {
    let shortcut_ok = state.shortcut.trigger.is_some() || state.shortcut.error.is_some();
    let typing_ok = matches!(
        state.typing,
        TypingState::Granted | TypingState::Denied | TypingState::Unavailable(_)
    );
    let active = state.models.iter().find(|m| m.active);
    let model_ok = active.is_some_and(|m| m.installed);

    let mut missing = Vec::new();
    if !shortcut_ok {
        missing.push("a shortcut");
    }
    if !typing_ok {
        missing.push("the typing permission");
    }
    if !model_ok {
        let downloading = state
            .download
            .as_ref()
            .is_some_and(|d| d.error.is_none() && active.is_some_and(|m| m.id == d.model_id));
        missing.push(if downloading {
            "the model download"
        } else {
            "the speech model"
        });
    }
    let done_hint = if missing.is_empty() {
        String::new()
    } else {
        format!("Waiting for {}.", join_and(&missing))
    };

    SetupView {
        shortcut: shortcut(&state.shortcut),
        typing: typing(&state.typing),
        model: model(state),
        models: model_choices(&state.models),
        autostart: state.autostart,
        autostart_error: state.autostart_error.clone().unwrap_or_default(),
        done_enabled: missing.is_empty(),
        done_hint,
    }
}

/// The download line under the model chooser in Settings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DownloadView {
    pub detail: Detail,
    /// `None` when the download failed.
    pub bar: Option<Bar>,
    /// Model to retry (the Retry button is shown when set).
    pub retry: Option<String>,
}

fn download(state: &UiState) -> Option<DownloadView> {
    let d = state.download.as_ref()?;
    let name = state
        .models
        .iter()
        .find(|m| m.id == d.model_id)
        .map_or(d.model_id.as_str(), |m| m.name.as_str());
    Some(if let Some(e) = &d.error {
        DownloadView {
            detail: Detail::error(format!("Couldn't download {name}: {e}")),
            bar: None,
            retry: Some(d.model_id.clone()),
        }
    } else if d.unpacking {
        DownloadView {
            detail: Detail::info(format!("Unpacking {name}…")),
            bar: Some(Bar {
                progress: 1.0,
                indeterminate: true,
            }),
            retry: None,
        }
    } else {
        DownloadView {
            detail: Detail::info(format!("Downloading {name}… {} %", d.percent.min(100))),
            bar: Some(Bar {
                progress: percent_fraction(d.percent),
                indeterminate: false,
            }),
            retry: None,
        }
    })
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsView {
    pub shortcut: ShortcutView,
    /// Microphone values to send back ("" = system default), parallel to the labels.
    pub microphones: Vec<String>,
    pub microphone_labels: Vec<String>,
    pub microphone_index: usize,
    pub model_ids: Vec<String>,
    pub model_labels: Vec<String>,
    pub model_index: usize,
    pub download: Option<DownloadView>,
    pub pause_secs: f32,
    pub idle_minutes: Vec<u32>,
    pub idle_labels: Vec<String>,
    pub idle_index: usize,
    pub autostart: bool,
    pub autostart_error: String,
    /// The "Your system" checklist (D-019).
    pub system: SystemView,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SystemView {
    pub checks: Vec<Check>,
    /// "All good", "2 problems" or "Checking…".
    pub summary: String,
    pub all_ok: bool,
    pub checking: bool,
}

fn system(state: &UiState) -> SystemView {
    let Some(report) = state.system.as_ref() else {
        return SystemView {
            checks: checks::pending(),
            summary: "Checking…".into(),
            all_ok: false,
            checking: true,
        };
    };
    let checks = checks::list(report, &state.typing);
    SystemView {
        summary: checks::summary(&checks),
        all_ok: checks.iter().all(|c| c.ok),
        checks,
        checking: false,
    }
}

impl SettingsView {
    pub fn microphone_at(&self, index: i32) -> Option<&str> {
        let i = usize::try_from(index).ok()?;
        self.microphones.get(i).map(String::as_str)
    }

    pub fn model_at(&self, index: i32) -> Option<&str> {
        let i = usize::try_from(index).ok()?;
        self.model_ids.get(i).map(String::as_str)
    }

    pub fn idle_at(&self, index: i32) -> Option<u32> {
        let i = usize::try_from(index).ok()?;
        self.idle_minutes.get(i).copied()
    }
}

fn microphone_label(name: &str) -> String {
    if name.is_empty() {
        "System default".into()
    } else {
        name.into()
    }
}

/// "System default" first, then the offered devices, then the selected one if it's missing.
fn microphones(state: &UiState) -> (Vec<String>, usize) {
    let mut values = vec![String::new()];
    for m in &state.microphones {
        if !values.contains(m) {
            values.push(m.clone());
        }
    }
    if !values.contains(&state.microphone) {
        values.push(state.microphone.clone());
    }
    let index = values
        .iter()
        .position(|m| *m == state.microphone)
        .unwrap_or(0);
    (values, index)
}

fn model_label(m: &ModelRow) -> String {
    if m.installed {
        format!("{} · {}", m.name, m.size)
    } else {
        format!("{} · download {}", m.name, m.size)
    }
}

fn idle_label(minutes: u32) -> String {
    match minutes {
        0 => "Never".into(),
        1 => "1 minute idle".into(),
        n => format!("{n} minutes idle"),
    }
}

/// The standard choices plus the configured value if it isn't one of them.
fn idle_choices(current: u32) -> (Vec<u32>, usize) {
    let mut values: Vec<u32> = IDLE_CHOICES.to_vec();
    if !values.contains(&current) {
        // Keep "Never" (0) last.
        let pos = values
            .iter()
            .position(|&v| v == 0 || v > current)
            .unwrap_or(values.len());
        values.insert(pos, current);
    }
    let index = values.iter().position(|&v| v == current).unwrap_or(0);
    (values, index)
}

/// Rounds to 0.1 s and clamps to the offered range.
pub fn clamp_pause(secs: f32) -> f32 {
    let secs = if secs.is_finite() { secs } else { PAUSE_MIN };
    ((secs * 10.0).round() / 10.0).clamp(PAUSE_MIN, PAUSE_MAX)
}

pub fn settings(state: &UiState) -> SettingsView {
    let (microphones, microphone_index) = microphones(state);
    let microphone_labels = microphones.iter().map(|m| microphone_label(m)).collect();
    let (idle_minutes, idle_index) = idle_choices(state.idle_minutes);
    let idle_labels = idle_minutes.iter().map(|&m| idle_label(m)).collect();
    SettingsView {
        shortcut: shortcut(&state.shortcut),
        microphones,
        microphone_labels,
        microphone_index,
        model_ids: state.models.iter().map(|m| m.id.clone()).collect(),
        model_labels: state.models.iter().map(model_label).collect(),
        model_index: state.models.iter().position(|m| m.active).unwrap_or(0),
        download: download(state),
        pause_secs: clamp_pause(state.pause_secs),
        idle_minutes,
        idle_labels,
        idle_index,
        autostart: state.autostart,
        autostart_error: state.autostart_error.clone().unwrap_or_default(),
        system: system(state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::samples;

    #[test]
    fn fresh_setup_shows_progress_and_blocks_done() {
        let v = setup(&samples::setup_fresh());
        assert_eq!(v.shortcut.status, Status::Done);
        assert_eq!(v.shortcut.trigger, "Meta+H");
        assert!(v.shortcut.can_change);
        assert_eq!(v.typing.status, Status::Todo);
        assert_eq!(v.typing.button, "Allow…");
        assert_eq!(v.model.status, Status::Busy);
        assert_eq!(v.model.ring, Some(0.45));
        assert_eq!(
            v.model.bar,
            Some(Bar {
                progress: 0.45,
                indeterminate: false
            })
        );
        assert!(v.model.detail.text.contains("45 %"));
        assert!(!v.done_enabled);
        assert_eq!(
            v.done_hint,
            "Waiting for the typing permission and the model download."
        );
        assert_eq!(v.models.len(), 2);
        assert!(v.models[0].active);
    }

    #[test]
    fn finished_setup_enables_done() {
        let v = setup(&samples::setup_done());
        assert_eq!(v.typing.status, Status::Done);
        assert_eq!(v.typing.button, "");
        assert_eq!(v.model.status, Status::Done);
        assert_eq!(v.model.bar, None);
        assert_eq!(v.model.action, ModelAction::None);
        assert_eq!(v.models[0].note, "In use");
        assert!(v.done_enabled);
        assert_eq!(v.done_hint, "");
        assert!(v.autostart);
    }

    #[test]
    fn denied_typing_offers_retry_and_still_allows_done() {
        let v = setup(&samples::typing_denied());
        assert_eq!(v.typing.status, Status::Failed);
        assert_eq!(v.typing.button, "Try again");
        assert!(v.typing.detail.error);
        assert!(v.done_enabled);
    }

    #[test]
    fn download_error_offers_retry() {
        let v = setup(&samples::download_error());
        assert_eq!(v.model.status, Status::Failed);
        assert_eq!(v.model.action, ModelAction::Retry("parakeet-v3".into()));
        assert_eq!(v.model.action.label(), "Retry");
        assert!(v.model.detail.error);
        assert!(v.model.detail.text.contains("connection reset"));
        assert_eq!(v.shortcut.status, Status::Busy);
        assert!(!v.shortcut.can_change, "no Change while the dialog is open");
        assert_eq!(v.typing.status, Status::Busy);
        assert_eq!(
            v.done_hint,
            "Waiting for a shortcut, the typing permission and the speech model."
        );
    }

    #[test]
    fn errors_count_as_handled_for_done() {
        let v = setup(&samples::unsupported_desktop());
        assert_eq!(v.shortcut.status, Status::Failed);
        assert_eq!(v.shortcut.trigger, "");
        assert!(!v.shortcut.can_change);
        assert_eq!(v.typing.status, Status::Failed);
        assert!(v.typing.detail.text.contains("Plasma"));
        assert!(v.done_enabled);
        assert!(!v.autostart_error.is_empty());
    }

    #[test]
    fn unpacking_shows_indeterminate_bar() {
        let mut s = samples::setup_fresh();
        if let Some(d) = s.download.as_mut() {
            d.unpacking = true;
        }
        let v = model(&s);
        assert_eq!(v.status, Status::Busy);
        assert_eq!(v.ring, None);
        assert!(v.bar.is_some_and(|b| b.indeterminate));
    }

    #[test]
    fn row_follows_the_model_being_downloaded() {
        // The user picked another model; the daemon may keep the old one active until it's ready.
        let mut s = samples::setup_fresh();
        if let Some(d) = s.download.as_mut() {
            d.model_id = "moonshine-base-en".into();
        }
        let v = model(&s);
        assert!(v.detail.text.starts_with("Moonshine base"));
        assert!(!setup(&s).done_enabled);
    }

    #[test]
    fn missing_model_without_download_offers_download() {
        let mut s = samples::setup_fresh();
        s.download = None;
        let v = model(&s);
        assert_eq!(v.status, Status::Todo);
        assert_eq!(v.action, ModelAction::Download("parakeet-v3".into()));
        assert_eq!(
            setup(&s).done_hint,
            "Waiting for the typing permission and the speech model."
        );
    }

    #[test]
    fn no_models_is_an_error_not_a_panic() {
        let s = UiState::default();
        assert_eq!(model(&s).status, Status::Failed);
        let v = settings(&s);
        assert!(v.model_ids.is_empty());
        assert_eq!(v.model_at(0), None);
    }

    #[test]
    fn unset_shortcut() {
        let v = shortcut(&ShortcutState {
            can_change: true,
            ..Default::default()
        });
        assert_eq!(v.status, Status::Todo);
        assert_eq!(v.trigger, "");
        assert!(v.can_change);
    }

    #[test]
    fn settings_lists() {
        let v = settings(&samples::settings());
        assert_eq!(
            v.microphone_labels,
            [
                "System default",
                "Blue Yeti Analog Stereo",
                "Built-in Audio Analog Stereo"
            ]
        );
        assert_eq!(v.microphone_index, 1);
        assert_eq!(v.microphone_at(0), Some(""));
        assert_eq!(v.microphone_at(-1), None);
        assert_eq!(v.model_labels[0], "Parakeet v3 · 490 MB");
        assert_eq!(v.model_labels[1], "Moonshine base · download 110 MB");
        assert_eq!(v.model_index, 0);
        assert_eq!(v.model_at(1), Some("moonshine-base-en"));
        assert_eq!(v.model_at(2), None);
        let d = v.download.as_ref().map(|d| d.detail.text.as_str());
        assert_eq!(d, Some("Downloading Moonshine base… 62 %"));
        assert_eq!(v.idle_labels[v.idle_index], "10 minutes idle");
        assert_eq!(v.idle_at(4), Some(0));
        assert_eq!(v.pause_secs, 0.8);
    }

    #[test]
    fn settings_keep_unknown_values_visible() {
        let mut s = samples::settings();
        s.microphones = vec!["USB Mic".into()];
        s.microphone = "Unplugged Mic".into();
        s.idle_minutes = 15;
        s.pause_secs = 7.0;
        let v = settings(&s);
        assert_eq!(v.microphones, ["", "USB Mic", "Unplugged Mic"]);
        assert_eq!(v.microphone_index, 2);
        assert_eq!(v.idle_minutes, [1, 5, 10, 15, 30, 0]);
        assert_eq!(v.idle_labels[v.idle_index], "15 minutes idle");
        assert_eq!(v.pause_secs, PAUSE_MAX);
    }

    #[test]
    fn settings_download_error_has_retry() {
        let mut s = samples::settings();
        if let Some(d) = s.download.as_mut() {
            d.error = Some("checksum mismatch".into());
        }
        let v = settings(&s);
        let d = v.download.unwrap_or_default();
        assert!(d.detail.error);
        assert_eq!(d.bar, None);
        assert_eq!(d.retry.as_deref(), Some("moonshine-base-en"));
    }

    #[test]
    fn pause_rounding() {
        assert_eq!(clamp_pause(0.83), 0.8);
        assert_eq!(clamp_pause(0.0), PAUSE_MIN);
        assert_eq!(clamp_pause(f32::NAN), PAUSE_MIN);
        assert_eq!(clamp_pause(1.25), 1.3);
    }

    #[test]
    fn join_words() {
        assert_eq!(join_and(&[]), "");
        assert_eq!(join_and(&["a"]), "a");
        assert_eq!(join_and(&["a", "b"]), "a and b");
        assert_eq!(join_and(&["a", "b", "c"]), "a, b and c");
    }
}
