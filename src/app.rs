//! The core state machine: Idle ⇄ Listening. Toggled only by the user.

use std::time::Duration;

use tokio::time::Instant;

use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};

use crate::{
    asr::worker::{AsrInput, AsrOutput, AsrWorker, SessionId},
    audio::{Capture, Level},
    hotkey::HotkeyEvent,
    model_manager::{ModelEvent, ModelManager, Outcome},
    notify::Notifier,
    output::{Output, OutputEvent},
};

/// Keep recording this long after the stop key so the last syllable isn't cut.
const STOP_TAIL: Duration = Duration::from_millis(250);
/// Opening the mic should take ~50 ms; give up if a source never delivers.
const MIC_OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// On quit, wait at most this long for the last phrase to be transcribed.
const QUIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// On quit, wait at most this long for queued words to be typed.
const QUIT_TYPE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the user should see (tray icon in stage 2).
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

/// Commands from the UI (hotkey, tray).
#[derive(Debug)]
pub enum Command {
    Toggle,
    /// Use this model (downloads it first if needed).
    SelectModel(String),
    DeleteModel(String),
    CancelDownload,
    Quit,
}

#[derive(Debug, PartialEq)]
enum State {
    Idle,
    Listening,
    /// Mic closing / last phrase being transcribed.
    Finishing,
}

pub struct App {
    asr: AsrWorker,
    output: Output,
    level: Level,
    status: watch::Sender<Status>,
    notifier: Notifier,
    state: State,
    session: SessionId,
    capture: Option<Capture>,
    stopping: Option<JoinHandle<()>>,
    audio_err_tx: mpsc::UnboundedSender<String>,
    audio_err_rx: mpsc::UnboundedReceiver<String>,
    models: ModelManager,
    /// When to unload the model (set after a session ends).
    unload_at: Option<Instant>,
}

impl App {
    pub fn new(
        asr: AsrWorker,
        output: Output,
        level: Level,
        status: watch::Sender<Status>,
        notifier: Notifier,
        models: ModelManager,
    ) -> Self {
        let (audio_err_tx, audio_err_rx) = mpsc::unbounded_channel();
        Self {
            asr,
            output,
            level,
            status,
            notifier,
            state: State::Idle,
            session: 0,
            capture: None,
            stopping: None,
            audio_err_tx,
            audio_err_rx,
            models,
            unload_at: None,
        }
    }

    /// Runs until `Command::Quit` or all command senders are gone.
    pub async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut hotkey: mpsc::UnboundedReceiver<HotkeyEvent>,
        mut asr_out: mpsc::UnboundedReceiver<AsrOutput>,
        mut output_events: mpsc::UnboundedReceiver<OutputEvent>,
        mut model_events: mpsc::UnboundedReceiver<ModelEvent>,
    ) {
        if !self.models.active_installed() && self.models.downloading().is_none() {
            // Until the first-run window exists (stage 4): fetch the model right away.
            self.models.start_download(self.models.active());
        }
        loop {
            let unload_at = self.unload_at;
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    Some(Command::Toggle) => self.toggle().await,
                    Some(Command::SelectModel(id)) => {
                        let outcome = self.models.select(&id);
                        self.apply(outcome);
                    }
                    Some(Command::DeleteModel(id)) => {
                        if let Err(e) = self.models.delete(&id).await {
                            warn!(error = %e, "could not delete model");
                        }
                    }
                    Some(Command::CancelDownload) => self.models.cancel(),
                    Some(Command::Quit) | None => break,
                },
                Some(ev) = model_events.recv() => {
                    let outcome = self.models.on_event(ev);
                    self.apply(outcome);
                }
                () = async { if let Some(t) = unload_at { tokio::time::sleep_until(t).await } }, if unload_at.is_some() => {
                    self.unload_at = None;
                    if self.state == State::Idle {
                        info!("unloading the model after being idle");
                        self.asr.send(AsrInput::Unload);
                    }
                }
                Some(ev) = hotkey.recv() => match ev {
                    HotkeyEvent::Toggle => self.toggle().await,
                    HotkeyEvent::Lost(why) => {
                        warn!(%why, "global shortcut lost");
                        self.set_status(Status::Error("Shortcut unavailable".into()));
                        self.notifier.error(
                            "murmur: shortcut unavailable",
                            "The global shortcut stopped working. Click the tray icon to dictate, or restart murmur.",
                        );
                    }
                },
                Some(out) = asr_out.recv() => self.on_asr(out).await,
                Some(ev) = output_events.recv() => self.on_output(ev),
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
        self.shutdown(asr_out).await;
    }

    /// Finishes the phrase in progress (nothing is lost), then tears down
    /// blocking parts off the async runtime.
    async fn shutdown(mut self, mut asr_out: mpsc::UnboundedReceiver<AsrOutput>) {
        info!("shutting down");
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
                self.notifier.error("murmur: model download failed", &why);
                self.set_status(Status::Error(format!("Model download failed: {why}")));
            }
        }
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
        let open = tokio::task::spawn_blocking(move || Capture::start(sink, on_error, level));
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
            OutputEvent::Ready => info!("typing ready"),
            OutputEvent::PermissionDenied => {
                self.set_status(Status::Error("Typing permission denied".into()));
                self.notifier.error(
                    "murmur can't type",
                    "Permission to type was denied. Dictate again to be asked once more.",
                );
            }
            OutputEvent::Disconnected(why) => warn!(%why, "typing session lost"),
            OutputEvent::Unavailable(why) => {
                self.set_status(Status::Error(format!("Typing unavailable: {why}")));
                self.notifier.error("murmur can't type", &why);
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
