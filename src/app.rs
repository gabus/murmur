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
    audio::{self, Capture, Level, Mic},
    hotkey::{Hotkey, HotkeyEvent},
    model_manager::{ModelEvent, ModelManager, ModelsView, Outcome},
    models::{self, MODELS},
    notify::Notifier,
    output::{Output, OutputEvent},
    ui::protocol::{DownloadRow, FromUi, ModelRow, ShortcutState, TypingState, UiState, Window},
    ui_host::{Generation, UiEvent, UiHost},
};

/// Keep recording this long after the stop key so the last syllable isn't cut.
const STOP_TAIL: Duration = Duration::from_millis(250);
/// Opening the mic should take ~50 ms; give up if a source never delivers.
const MIC_OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// On quit, wait at most this long for the last phrase to be transcribed.
const QUIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// On quit, wait at most this long for queued words to be typed.
const QUIT_TYPE_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for the idle-unload setting (one day).
const MAX_IDLE_MINUTES: u32 = 24 * 60;

/// What an error is about, so fixing one problem clears only its error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Model,
    Typing,
    Microphone,
    Shortcut,
}

/// What the user should see (tray icon).
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    /// Listening, model still loading (audio is being buffered).
    Loading,
    Listening,
    /// Words are waiting until the user releases the modifier keys they hold.
    Waiting,
    Error(ErrorKind, String),
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
    Microphones(Vec<Mic>),
    Autostart {
        requested: bool,
        result: Result<bool, String>,
    },
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
    /// Lets the app bind the shortcut again after it failed (None: no shortcut wanted).
    pub hotkey_events: Option<mpsc::UnboundedSender<HotkeyEvent>>,
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
    hotkey_events: Option<mpsc::UnboundedSender<HotkeyEvent>>,
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
    ui_generation: Generation,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    shortcut: ShortcutState,
    typing: TypingState,
    microphones: Vec<Mic>,
    autostart_error: Option<String>,
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
            hotkey_events: parts.hotkey_events,
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
            ui_generation: 0,
            ui_tx,
            ui_rx,
            internal_tx,
            internal_rx,
            shortcut,
            typing: TypingState::Unknown,
            microphones: Vec::new(),
            autostart_error: None,
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
                    let outcome = self.models.on_event(ev);
                    self.apply(outcome);
                }
                changed = ch.models_view.changed() => {
                    if changed.is_ok() {
                        self.models_view = ch.models_view.borrow_and_update().clone();
                        if self.models_view.failed.is_none() && self.models.active_installed() {
                            self.clear_error(ErrorKind::Model);
                        }
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
                        self.set_status(Status::Error(ErrorKind::Microphone, format!("Microphone error: {why}")));
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
            ui.shutdown().await;
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
        let outcome = self.models.select(id);
        self.apply(outcome);
    }

    fn apply(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::None => {}
            Outcome::Switch(spec) => {
                self.asr.send(AsrInput::SetModel(spec));
                self.clear_error(ErrorKind::Model);
            }
            Outcome::Failed(why) => {
                // The setup page shows download errors itself.
                if !self.setup_open() {
                    self.notifier.error("murmur: model download failed", &why);
                }
                self.set_status(Status::Error(
                    ErrorKind::Model,
                    format!("Model download failed: {why}"),
                ));
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
            self.set_status(Status::Error(ErrorKind::Model, why.clone()));
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
        self.set_status(Status::Error(ErrorKind::Microphone, failure));
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
                self.clear_error(ErrorKind::Shortcut);
            }
            HotkeyEvent::Failed(why) => {
                self.shortcut = ShortcutState {
                    error: Some(why),
                    // "Change" binds again.
                    can_change: self.hotkey_events.is_some(),
                    ..Default::default()
                };
                self.shortcut_label.send_replace(None);
                self.report_shortcut_problem(
                    "murmur: no global shortcut",
                    "No shortcut is set. Click the tray icon to dictate, or set one in murmur's settings.",
                );
            }
            HotkeyEvent::Lost(why) => {
                warn!(%why, "global shortcut lost");
                self.shortcut.error = Some("The shortcut stopped working".into());
                self.shortcut.can_change = self.hotkey_events.is_some();
                self.shortcut_label.send_replace(None);
                self.report_shortcut_problem(
                    "murmur: shortcut unavailable",
                    "The global shortcut stopped working. Click the tray icon to dictate, or set it again in murmur's settings.",
                );
            }
            HotkeyEvent::ConfigureFailed(why) => {
                self.shortcut.pending = false;
                self.shortcut.error = Some(format!("Couldn't open the shortcut settings: {why}"));
            }
        }
        self.push_ui();
    }

    fn report_shortcut_problem(&mut self, title: &str, body: &str) {
        if !self.setup_open() {
            self.notifier.error(title, body);
        }
        self.set_status(Status::Error(
            ErrorKind::Shortcut,
            "No global shortcut".into(),
        ));
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
                self.set_status(Status::Error(
                    ErrorKind::Model,
                    format!("Speech model unavailable: {e}"),
                ));
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
                    if !matches!(*self.status.borrow(), Status::Error(..)) {
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
                self.clear_error(ErrorKind::Typing);
            }
            OutputEvent::PermissionDenied => {
                self.typing = TypingState::Denied;
                self.set_status(Status::Error(
                    ErrorKind::Typing,
                    "Typing permission denied".into(),
                ));
                if !self.setup_open() {
                    self.notifier.error(
                        "murmur can't type",
                        "Permission to type was denied. Dictate again to be asked once more.",
                    );
                }
            }
            OutputEvent::Disconnected(why) => {
                warn!(%why, "typing session lost");
                // A failed "Allow…" attempt: let the user try again.
                if self.typing == TypingState::Asking {
                    self.typing = TypingState::Unknown;
                }
            }
            OutputEvent::Unavailable(why) => {
                self.typing = TypingState::Unavailable(why.clone());
                self.set_status(Status::Error(
                    ErrorKind::Typing,
                    format!("Typing unavailable: {why}"),
                ));
                if !self.setup_open() {
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

    fn setup_open(&self) -> bool {
        self.ui.as_ref().is_some_and(|u| u.window == Window::Setup)
    }

    fn open_window(&mut self, window: Window) {
        if self.ui.is_some() {
            debug!("a window is already open");
            return;
        }
        self.ui_generation += 1;
        match UiHost::open(
            window,
            self.ui_generation,
            self.ui_state(),
            self.ui_tx.clone(),
        ) {
            Ok(ui) => self.ui = Some(ui),
            Err(e) => {
                warn!(error = %e, "cannot open the window");
                return;
            }
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
            UiEvent::Closed(generation) => {
                if self.ui.as_ref().is_some_and(|u| u.generation == generation) {
                    info!("window closed");
                    self.ui = None;
                }
                return;
            }
            UiEvent::Request(r) => r,
        };
        match req {
            FromUi::ChangeShortcut => {
                let dead = self.hotkey.as_ref().is_none_or(Hotkey::is_finished);
                match (&self.hotkey, &self.hotkey_events) {
                    // Binding failed or the session was lost: bind again.
                    (_, Some(tx)) if dead => {
                        self.hotkey = Some(Hotkey::spawn(tx.clone()));
                        self.shortcut.pending = true;
                        self.shortcut.error = None;
                    }
                    // Opens the desktop's shortcut settings and returns at once. Don't
                    // wait: the user may close them unchanged. A change still arrives
                    // as ShortcutsChanged → Bound.
                    (Some(h), _) => h.configure(),
                    _ => {}
                }
            }
            FromUi::AllowTyping => {
                if self.typing != TypingState::Granted {
                    self.typing = TypingState::Asking;
                    self.output.retry();
                }
            }
            FromUi::SelectModel(id) => self.select_model(&id),
            FromUi::RetryDownload(id) => {
                if let Some(m) = models::find(&id) {
                    self.models.start_download(m);
                }
            }
            FromUi::SetMicrophone(label) => {
                let id = if label.is_empty() {
                    String::new()
                } else {
                    self.microphones
                        .iter()
                        .find(|m| m.label == label)
                        .map_or(label, |m| m.id.clone())
                };
                let o = self.models.update_config(|c| c.microphone = id);
                self.apply(o);
            }
            FromUi::SetPauseSecs(p) => {
                let p = if p.is_finite() {
                    p.clamp(0.3, 3.0)
                } else {
                    0.8
                };
                let o = self.models.update_config(|c| c.pause_secs = p);
                self.apply(o);
            }
            FromUi::SetIdleMinutes(m) => {
                let m = m.min(MAX_IDLE_MINUTES);
                let o = self.models.update_config(|c| c.idle_unload_minutes = m);
                self.apply(o);
                if self.unload_at.is_some() {
                    self.unload_at =
                        (m > 0).then(|| Instant::now() + Duration::from_secs(u64::from(m) * 60));
                }
            }
            FromUi::SetAutostart(enabled) => {
                self.autostart_error = None;
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let result = crate::autostart::set(enabled).await;
                    let _ = tx.send(Internal::Autostart {
                        requested: enabled,
                        result,
                    });
                });
            }
            FromUi::SetupDone => {
                let o = self.models.update_config(|c| c.setup_done = true);
                self.apply(o);
                if let Some(ui) = &mut self.ui {
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
            Internal::Autostart {
                requested,
                result: Ok(on),
            } => {
                self.autostart_error = (requested && !on)
                    .then(|| "The desktop didn't allow starting at login".to_owned());
                let o = self.models.update_config(|c| c.autostart = on);
                self.apply(o);
            }
            Internal::Autostart { result: Err(e), .. } => {
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
        let download = match (&view.download, &view.failed) {
            (Some(d), _) => Some(DownloadRow {
                model_id: d.model.to_owned(),
                percent: d.percent,
                unpacking: d.unpacking,
                error: None,
            }),
            (None, Some((id, e))) => Some(DownloadRow {
                model_id: (*id).to_owned(),
                percent: 0,
                unpacking: false,
                error: Some(e.clone()),
            }),
            (None, None) => None,
        };
        let mut microphones = vec![String::new()];
        microphones.extend(self.microphones.iter().map(|m| m.label.clone()));
        // Show the configured mic by its label (the config stores its id).
        let microphone = self
            .microphones
            .iter()
            .find(|m| m.id == cfg.microphone || m.label == cfg.microphone)
            .map_or_else(|| cfg.microphone.clone(), |m| m.label.clone());
        UiState {
            shortcut: self.shortcut.clone(),
            typing: self.typing.clone(),
            models,
            download,
            microphones,
            microphone,
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

    /// Clears an error of this kind (only while idle; a new dictation resets
    /// the status anyway).
    fn clear_error(&self, kind: ErrorKind) {
        if self.state == State::Idle {
            self.status.send_if_modified(|cur| {
                if matches!(cur, Status::Error(k, _) if *k == kind) {
                    *cur = Status::Idle;
                    true
                } else {
                    false
                }
            });
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
