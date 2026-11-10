//! The core state machine: Idle ⇄ Listening. Toggled only by the user.

use std::time::Duration;

use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};

use crate::{
    asr::worker::{AsrInput, AsrOutput, AsrWorker},
    audio::{Capture, Level},
    hotkey::HotkeyEvent,
    output::{Output, OutputEvent},
};

/// Keep recording this long after the stop key so the last syllable isn't cut.
const STOP_TAIL: Duration = Duration::from_millis(250);

/// What the user should see (tray icon in stage 2).
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    /// Listening, model still loading (audio is being buffered).
    Loading,
    Listening,
    Error(String),
}

/// Commands from the UI (hotkey, tray).
#[derive(Debug)]
pub enum Command {
    Toggle,
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
    state: State,
    capture: Option<Capture>,
    stopping: Option<JoinHandle<()>>,
    model_ready: bool,
}

impl App {
    pub fn new(
        asr: AsrWorker,
        output: Output,
        level: Level,
        status: watch::Sender<Status>,
    ) -> Self {
        Self {
            asr,
            output,
            level,
            status,
            state: State::Idle,
            capture: None,
            stopping: None,
            model_ready: false,
        }
    }

    /// Runs until `Command::Quit` or all command senders are gone.
    pub async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut hotkey: mpsc::UnboundedReceiver<HotkeyEvent>,
        mut asr_out: mpsc::UnboundedReceiver<AsrOutput>,
        mut output_events: mpsc::UnboundedReceiver<OutputEvent>,
    ) {
        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    Some(Command::Toggle) => self.toggle().await,
                    Some(Command::Quit) | None => break,
                },
                Some(ev) = hotkey.recv() => match ev {
                    HotkeyEvent::Toggle => self.toggle().await,
                    HotkeyEvent::Lost(why) => {
                        warn!(%why, "global shortcut lost");
                        self.set_status(Status::Error("Shortcut unavailable".into()));
                    }
                },
                Some(out) = asr_out.recv() => self.on_asr(out),
                Some(ev) = output_events.recv() => self.on_output(ev),
            }
        }
        info!("shutting down");
        if self.state == State::Listening {
            self.stop().await;
        }
        if let Some(h) = self.stopping.take() {
            let _ = h.await;
        }
        self.output.flush().await;
    }

    async fn toggle(&mut self) {
        match self.state {
            State::Idle | State::Finishing => self.start().await,
            State::Listening => self.stop().await,
        }
    }

    async fn start(&mut self) {
        // A previous stop must have delivered its End before the next Begin.
        if let Some(h) = self.stopping.take() {
            let _ = h.await;
        }
        self.asr.send(AsrInput::Begin);
        self.state = State::Listening;
        self.set_status(if self.model_ready {
            Status::Listening
        } else {
            Status::Loading
        });
        let tx = self.asr.sender();
        let sink = Box::new(move |chunk: Vec<f32>| {
            let _ = tx.send(AsrInput::Audio(chunk));
        });
        let level = self.level.clone();
        match tokio::task::spawn_blocking(move || Capture::start(sink, level)).await {
            Ok(Ok(c)) => {
                info!("listening");
                self.capture = Some(c);
            }
            Ok(Err(e)) => {
                error!(error = %e, "cannot open microphone");
                self.asr.send(AsrInput::End);
                self.state = State::Finishing;
                self.set_status(Status::Error(e.to_string()));
            }
            Err(e) => {
                error!(error = %e, "audio start task failed");
                self.asr.send(AsrInput::End);
                self.state = State::Finishing;
                self.set_status(Status::Error("Microphone error".into()));
            }
        }
    }

    async fn stop(&mut self) {
        info!("stopping");
        self.state = State::Finishing;
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

    fn on_asr(&mut self, out: AsrOutput) {
        match out {
            AsrOutput::Loading => debug!("model loading"),
            AsrOutput::Ready => {
                self.model_ready = true;
                if self.state == State::Listening {
                    self.set_status(Status::Listening);
                }
            }
            AsrOutput::Text(t) => self.output.type_text(t),
            AsrOutput::Finished => {
                if self.state == State::Finishing {
                    self.state = State::Idle;
                    if !matches!(*self.status.borrow(), Status::Error(_)) {
                        self.set_status(Status::Idle);
                    }
                    info!("idle");
                }
            }
            AsrOutput::Error(e) => {
                error!(error = %e, "speech recognition error");
                self.model_ready = false;
                self.set_status(Status::Error(e.to_string()));
            }
        }
    }

    fn on_output(&mut self, ev: OutputEvent) {
        match ev {
            OutputEvent::Ready => info!("typing ready"),
            OutputEvent::PermissionDenied => {
                self.set_status(Status::Error("Typing permission denied".into()));
            }
            OutputEvent::Disconnected(why) => warn!(%why, "typing session lost"),
            OutputEvent::Unavailable(why) => {
                self.set_status(Status::Error(format!("Typing unavailable: {why}")));
            }
            OutputEvent::Unmapped { count } => {
                warn!(
                    count,
                    "characters not on the active keyboard layout were skipped"
                );
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
