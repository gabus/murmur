//! The core state machine: Idle ⇄ Listening (toggled only by the user), plus
//! the setup/settings windows' state and requests.

use std::time::Duration;

use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};
use tracing::{debug, error, info, warn};

use crate::{
    asr::worker::{AsrInput, AsrOutput, AsrWorker, SessionId},
    audio::{self, Capture, Level},
    hotkey::{Hotkey, HotkeyEvent},
    model_manager::{ModelEvent, ModelManager, ModelsView, Outcome},
    models::{self, MODELS},
    notify::Notifier,
    output::{Output, OutputEvent},
    ui::protocol::{DownloadRow, FromUi, ModelRow, ShortcutState, TypingState, UiState, Window},
    ui_host::{UiEvent, UiHost},
};

/// Keep recording this long after the stop key so the last syllable isn't cut.
const STOP_TAIL: Duration = Duration::from_millis(250);
/// Opening the mic should take ~50 ms; give up if a source never delivers.
const MIC_OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// On quit, wait at most this long for the last phrase to be transcribed.
const QUIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// On quit, wait at most this long for queued words to be typed.
const QUIT_TYPE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the user should see (tray icon).
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    /// Listening, model still loading (audio is being buffered).
    Loading,
    Listening,
    /// Words are waiting until the user releases the modifier keys they hold.
    Waiting,
    Error(String),
}

/// Commands from the UI (tray, D-Bus).
#[derive(Debug)]
pub enum Command {
    Toggle,
    /// Use this model (downloads it first if needed).
    SelectModel(String),
    DeleteModel(String),
    CancelDownload,
    /// Show the settings window (or the setup page if setup isn't done).
    OpenSettings,
    Quit,
}

#[derive(Debug, PartialEq)]
enum State {
    Idle,
    Listening,
    /// Mic closing / last phrase being transcribed.
    Finishing,
}

/// Results of background work started by the app.
enum Internal {
    Microphones(Vec<String>),
    Autostart(Result<bool, String>),
}

/// Event sources the app listens to.
pub struct Channels {
    pub commands: mpsc::UnboundedReceiver<Command>,
    pub hotkey: mpsc::UnboundedReceiver<HotkeyEvent>,
    pub asr: mpsc::UnboundedReceiver<AsrOutput>,
    pub output: mpsc::UnboundedReceiver<OutputEvent>,
    pub models: mpsc::UnboundedReceiver<ModelEvent>,
    pub models_view: watch::Receiver<ModelsView>,
}

/// Long-lived handles the app owns.
pub struct Parts {
    pub asr: AsrWorker,
    pub output: Output,
    pub hotkey: Option<Hotkey>,
    pub level: Level,
    pub status: watch::Sender<Status>,
    /// The bound shortcut, for the tray tooltip.
    pub shortcut_label: watch::Sender<Option<String>>,
    pub notifier: Notifier,
    pub models: ModelManager,
}

pub struct App {
    asr: AsrWorker,
    output: Output,
    hotkey: Option<Hotkey>,
    level: Level,
    status: watch::Sender<Status>,
    shortcut_label: watch::Sender<Option<String>>,
    notifier: Notifier,
    models: ModelManager,
    state: State,
    session: SessionId,
    capture: Option<Capture>,
    stopping: Option<JoinHandle<()>>,
    audio_err_tx: mpsc::UnboundedSender<String>,
    audio_err_rx: mpsc::UnboundedReceiver<String>,
    /// When to unload the model (set after a session ends).
    unload_at: Option<Instant>,
    // Window state.
    ui: Option<UiHost>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    shortcut: ShortcutState,
    typing: TypingState,
    microphones: Vec<String>,
    autostart_error: Option<String>,
    download_error: Option<(String, String)>,
    models_view: ModelsView,
}

impl App {
    pub fn new(parts: Parts, models_view: ModelsView) -> Self {
        let (audio_err_tx, audio_err_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let shortcut = ShortcutState {
            pending: parts.hotkey.is_some(),
            error: parts
                .hotkey
                .is_none()
                .then(|| "No global shortcut on this desktop".to_owned()),
            ..Default::default()
        };
        Self {
            asr: parts.asr,
            output: parts.output,
            hotkey: parts.hotkey,
            level: parts.level,
            status: parts.status,
            shortcut_label: parts.shortcut_label,
            notifier: parts.notifier,
            models: parts.models,
            state: State::Idle,
            session: 0,
            capture: None,
            stopping: None,
            audio_err_tx,
            audio_err_rx,
            unload_at: None,
            ui: None,
            ui_tx,
            ui_rx,
            internal_tx,
            internal_rx,
            shortcut,
            typing: TypingState::Unknown,
            microphones: Vec::new(),
            autostart_error: None,
            download_error: None,
            models_view,
        }
    }

    /// Runs until `Command::Quit` or all command senders are gone.
    pub async fn run(mut self, mut ch: Channels) {
        if !self.models.active_installed() && self.models.downloading().is_none() {
            self.models.start_download(self.models.active());
        }
        if !self.models.config().setup_done {
            self.open_window(Window::Setup);
        }
        loop {
            let unload_at = self.unload_at;
            tokio::select! {
                cmd = ch.commands.recv() => match cmd {
                    Some(Command::Toggle) => self.toggle().await,
                    Some(Command::SelectModel(id)) => self.select_model(&id),
                    Some(Command::DeleteModel(id)) => {
                        if let Err(e) = self.models.delete(&id).await {
                            warn!(error = %e, "could not delete model");
                        }
                    }
                    Some(Command::CancelDownload) => self.models.cancel(),
                    Some(Command::OpenSettings) => {
                        let w = if self.models.config().setup_done {
                            Window::Settings
                        } else {
                            Window::Setup
                        };
                        self.open_window(w);
                    }
                    Some(Command::Quit) | None => break,
                },
                Some(ev) = ch.models.recv() => {
                    if let ModelEvent::Finished(id, Err(e)) = &ev {
                        self.download_error = Some(((*id).to_owned(), e.to_string()));
                    }
                    let outcome = self.models.on_event(ev);
                    self.apply(outcome);
                }
                changed = ch.models_view.changed() => {
                    if changed.is_ok() {
                        self.models_view = ch.models_view.borrow_and_update().clone();
                        self.push_ui();
                    }
                }
                () = async { if let Some(t) = unload_at { tokio::time::sleep_until(t).await } }, if unload_at.is_some() => {
                    self.unload_at = None;
                    if self.state == State::Idle {
                        info!("unloading the model after being idle");
                        self.asr.send(AsrInput::Unload);
                    }
                }
                Some(ev) = ch.hotkey.recv() => self.on_hotkey(ev).await,
                Some(out) = ch.asr.recv() => self.on_asr(out).await,
                Some(ev) = ch.output.recv() => self.on_output(ev),
                Some(ev) = self.ui_rx.recv() => self.on_ui(ev),
                Some(r) = self.internal_rx.recv() => self.on_internal(r),
                Some(why) = self.audio_err_rx.recv() => {
                    if self.state == State::Listening {
                        error!(%why, "microphone failed while listening");
                        self.set_status(Status::Error(format!("Microphone error: {why}")));
                        self.notifier.error("murmur: microphone lost", &why);
                        self.stop().await;
                    }
                }
            }
        }
        self.shutdown(ch.asr).await;
    }

    /// Finishes the phrase in progress (nothing is lost), then tears down
    /// blocking parts off the async runtime.
    async fn shutdown(mut self, mut asr_out: mpsc::UnboundedReceiver<AsrOutput>) {
        info!("shutting down");
        if let Some(ui) = self.ui.take() {
            ui.close();
        }
        if self.state == State::Listening {
            self.stop().await;
        }
        if self.state == State::Finishing {
            let drain = async {
                while let Some(out) = asr_out.recv().await {
                    match out {
                        AsrOutput::Text(t) => self.output.type_text(t),
                        AsrOutput::Finished(id) if id == self.session => break,
                        _ => {}
                    }
                }
            };
            if tokio::time::timeout(QUIT_DRAIN_TIMEOUT, drain)
                .await
                .is_err()
            {
                warn!("gave up waiting for the last phrase");
            }
        }
        if let Some(h) = self.stopping.take() {
            let _ = h.await;
        }
        if tokio::time::timeout(QUIT_TYPE_TIMEOUT, self.output.flush())
            .await
            .is_err()
        {
            warn!("gave up typing the last words (keys held or typing unavailable)");
        }
        let asr = self.asr;
        let _ = tokio::task::spawn_blocking(move || drop(asr)).await;
    }

    async fn toggle(&mut self) {
        match self.state {
            State::Idle | State::Finishing => self.start().await,
            State::Listening => self.stop().await,
        }
    }

    fn select_model(&mut self, id: &str) {
        self.download_error = None;
        let outcome = self.models.select(id);
        self.apply(outcome);
        self.push_ui();
    }

    fn apply(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::None => {}
            Outcome::Switch(spec) => {
                self.asr.send(AsrInput::SetModel(spec));
                if matches!(*self.status.borrow(), Status::Error(_)) && self.state == State::Idle {
                    self.set_status(Status::Idle);
                }
            }
            Outcome::Failed(why) => {
                // The setup window shows download errors itself.
                if self.ui.is_none() {
                    self.notifier.error("murmur: model download failed", &why);
                }
                self.set_status(Status::Error(format!("Model download failed: {why}")));
            }
        }
        self.push_ui();
    }

    async fn start(&mut self) {
        if !self.models.active_installed() {
            let model = self.models.active();
            let why = if self.models.downloading() == Some(model.id) {
                format!("{} is still downloading", model.name)
            } else {
                self.models.start_download(model);
                format!("{} isn't downloaded yet; downloading now", model.name)
            };
            info!(%why, "can't start dictation");
            self.set_status(Status::Error(why.clone()));
            self.notifier.error("murmur: no speech model yet", &why);
            return;
        }
        self.unload_at = None;
        // A new dictation is a fresh chance after a denied/unavailable typing session.
        self.output.retry();
        // A previous stop must have delivered its End before the next Begin.
        if let Some(h) = self.stopping.take() {
            let _ = h.await;
        }
        self.session += 1;
        self.asr.send(AsrInput::Begin(self.session));
        self.state = State::Listening;
        // The icon switches immediately; Ready flips Loading → Listening.
        self.set_status(Status::Loading);

        let tx = self.asr.sender();
        let sink = Box::new(move |chunk: Vec<f32>| {
            let _ = tx.send(AsrInput::Audio(chunk));
        });
        let err_tx = self.audio_err_tx.clone();
        let on_error = Box::new(move |why: String| {
            let _ = err_tx.send(why);
        });
        let level = self.level.clone();
        let mic = self.models.config().microphone.clone();
        let open = tokio::task::spawn_blocking(move || Capture::start(&mic, sink, on_error, level));
        let failure = match tokio::time::timeout(MIC_OPEN_TIMEOUT, open).await {
            Ok(Ok(Ok(c))) => {
                info!("listening");
                self.capture = Some(c);
                return;
            }
            Ok(Ok(Err(e))) => e.to_string(),
            Ok(Err(e)) => format!("audio task failed: {e}"),
            // The blocking task keeps running; if it ever opens, the Capture is
            // dropped with the task's result, which closes the stream.
            Err(_) => "the microphone did not respond".to_owned(),
        };
        error!(error = %failure, "cannot open microphone");
        self.asr.send(AsrInput::End);
        self.state = State::Finishing;
        self.notifier.error("murmur: no microphone", &failure);
        self.set_status(Status::Error(failure));
    }

    async fn stop(&mut self) {
        info!("stopping");
        self.state = State::Finishing;
        self.level.reset();
        let capture = self.capture.take();
        let asr = self.asr.sender();
        self.stopping = Some(tokio::spawn(async move {
            if let Some(c) = capture {
                tokio::time::sleep(STOP_TAIL).await;
                let _ = tokio::task::spawn_blocking(move || c.stop()).await;
            }
            // No more audio can arrive now; let the worker finish the phrase.
            let _ = asr.send(AsrInput::End);
        }));
    }

    async fn on_hotkey(&mut self, ev: HotkeyEvent) {
        match ev {
            HotkeyEvent::Toggle => self.toggle().await,
            HotkeyEvent::Bound {
                trigger,
                can_configure,
            } => {
                self.shortcut = ShortcutState {
                    trigger: trigger.clone(),
                    can_change: can_configure,
                    pending: false,
                    error: None,
                };
                self.shortcut_label.send_replace(trigger);
                self.push_ui();
            }
            HotkeyEvent::Failed(why) => {
                self.shortcut = ShortcutState {
                    error: Some(why),
                    ..Default::default()
                };
                self.push_ui();
            }
            HotkeyEvent::Lost(why) => {
                warn!(%why, "global shortcut lost");
                self.shortcut.error = Some("The shortcut stopped working".into());
                self.set_status(Status::Error("Shortcut unavailable".into()));
                self.notifier.error(
                    "murmur: shortcut unavailable",
                    "The global shortcut stopped working. Click the tray icon to dictate, or restart murmur.",
                );
                self.push_ui();
            }
        }
    }

    async fn on_asr(&mut self, out: AsrOutput) {
        match out {
            AsrOutput::Loading => debug!("model loading"),
            AsrOutput::Ready => {
                if self.state == State::Listening {
                    self.set_status(Status::Listening);
                }
            }
            AsrOutput::LoadFailed(e) => {
                error!(error = %e, "speech model unavailable");
                self.set_status(Status::Error(format!("Speech model unavailable: {e}")));
                self.notifier
                    .error("murmur: speech model missing", &format!("{e}"));
                if self.state == State::Listening {
                    // Don't keep the mic open for nothing.
                    self.stop().await;
                }
            }
            AsrOutput::Text(t) => self.output.type_text(t),
            AsrOutput::Finished(id) => {
                if self.state == State::Finishing && id == self.session {
                    self.state = State::Idle;
                    let mins = self.models.config().idle_unload_minutes;
                    if mins > 0 {
                        self.unload_at =
                            Some(Instant::now() + Duration::from_secs(u64::from(mins) * 60));
                    }
                    if !matches!(*self.status.borrow(), Status::Error(_)) {
                        self.set_status(Status::Idle);
                    }
                    info!("idle");
                }
            }
            AsrOutput::Error(e) => {
                error!(error = %e, "speech recognition error");
            }
        }
    }

    fn on_output(&mut self, ev: OutputEvent) {
        match ev {
            OutputEvent::Ready => {
                info!("typing ready");
                self.typing = TypingState::Granted;
            }
            OutputEvent::PermissionDenied => {
                self.typing = TypingState::Denied;
                self.set_status(Status::Error("Typing permission denied".into()));
                if self.ui.is_none() {
                    self.notifier.error(
                        "murmur can't type",
                        "Permission to type was denied. Dictate again to be asked once more.",
                    );
                }
            }
            OutputEvent::Disconnected(why) => warn!(%why, "typing session lost"),
            OutputEvent::Unavailable(why) => {
                self.typing = TypingState::Unavailable(why.clone());
                self.set_status(Status::Error(format!("Typing unavailable: {why}")));
                if self.ui.is_none() {
                    self.notifier.error("murmur can't type", &why);
                }
            }
            OutputEvent::Unmapped { count } => {
                warn!(
                    count,
                    "characters not on the active keyboard layout were skipped"
                );
            }
            OutputEvent::Waiting(true) => {
                info!("typing waits for held modifier keys");
                self.set_status(Status::Waiting);
            }
            OutputEvent::Waiting(false) => {
                if *self.status.borrow() == Status::Waiting {
                    self.set_status(if self.state == State::Listening {
                        Status::Listening
                    } else {
                        Status::Idle
                    });
                }
            }
        }
        self.push_ui();
    }

    fn open_window(&mut self, window: Window) {
        if self.ui.is_some() {
            debug!("a window is already open");
            return;
        }
        match UiHost::open(window, self.ui_state(), self.ui_tx.clone()) {
            Ok(ui) => self.ui = Some(ui),
            Err(e) => warn!(error = %e, "cannot open the window"),
        }
        // Microphone names are slow to list; fill them in when ready.
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            if let Ok(mics) = tokio::task::spawn_blocking(audio::microphones).await {
                let _ = tx.send(Internal::Microphones(mics));
            }
        });
    }

    fn on_ui(&mut self, ev: UiEvent) {
        let req = match ev {
            UiEvent::Closed => {
                info!("window closed");
                self.ui = None;
                return;
            }
            UiEvent::Request(r) => r,
        };
        match req {
            FromUi::ChangeShortcut => {
                if let Some(h) = &self.hotkey {
                    h.configure();
                }
            }
            FromUi::AllowTyping => {
                self.typing = TypingState::Asking;
                self.output.retry();
            }
            FromUi::SelectModel(id) => self.select_model(&id),
            FromUi::RetryDownload(id) => {
                self.download_error = None;
                if let Some(m) = models::find(&id) {
                    self.models.start_download(m);
                }
            }
            FromUi::SetMicrophone(name) => {
                let o = self.models.update_config(|c| c.microphone = name);
                self.apply(o);
            }
            FromUi::SetPauseSecs(p) => {
                let o = self
                    .models
                    .update_config(|c| c.pause_secs = p.clamp(0.3, 3.0));
                self.apply(o);
            }
            FromUi::SetIdleMinutes(m) => {
                let o = self.models.update_config(|c| c.idle_unload_minutes = m);
                self.apply(o);
            }
            FromUi::SetAutostart(enabled) => {
                self.autostart_error = None;
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let r = crate::autostart::set(enabled).await;
                    let _ = tx.send(Internal::Autostart(r));
                });
            }
            FromUi::SetupDone => {
                let o = self.models.update_config(|c| c.setup_done = true);
                self.apply(o);
                if let Some(ui) = &self.ui {
                    ui.close();
                }
                info!("setup complete");
            }
        }
        self.push_ui();
    }

    fn on_internal(&mut self, r: Internal) {
        match r {
            Internal::Microphones(m) => self.microphones = m,
            Internal::Autostart(Ok(on)) => {
                let o = self.models.update_config(|c| c.autostart = on);
                self.apply(o);
            }
            Internal::Autostart(Err(e)) => {
                warn!(error = %e, "could not change autostart");
                self.autostart_error = Some(e);
            }
        }
        self.push_ui();
    }

    /// Everything the windows display.
    fn ui_state(&self) -> UiState {
        let cfg = self.models.config();
        let view = &self.models_view;
        let models = MODELS
            .iter()
            .map(|m| ModelRow {
                id: m.id.to_owned(),
                name: m.name.to_owned(),
                summary: m.summary.to_owned(),
                size: m.size_label(),
                installed: view.installed.contains(&m.id),
                active: view.active == m.id,
            })
            .collect();
        let download = match (&view.download, &self.download_error) {
            (Some(d), _) => Some(DownloadRow {
                model_id: d.model.to_owned(),
                percent: d.percent,
                unpacking: d.unpacking,
                error: None,
            }),
            (None, Some((id, e))) => Some(DownloadRow {
                model_id: id.clone(),
                percent: 0,
                unpacking: false,
                error: Some(e.clone()),
            }),
            (None, None) => None,
        };
        let mut microphones = vec![String::new()];
        microphones.extend(self.microphones.iter().cloned());
        UiState {
            shortcut: self.shortcut.clone(),
            typing: self.typing.clone(),
            models,
            download,
            microphones,
            microphone: cfg.microphone.clone(),
            pause_secs: cfg.pause_secs,
            idle_minutes: cfg.idle_unload_minutes,
            autostart: cfg.autostart,
            autostart_error: self.autostart_error.clone(),
        }
    }

    fn push_ui(&self) {
        if let Some(ui) = &self.ui {
            ui.send(self.ui_state());
        }
    }

    fn set_status(&self, s: Status) {
        self.status.send_if_modified(|cur| {
            if *cur == s {
                false
            } else {
                *cur = s;
                true
            }
        });
    }
}
