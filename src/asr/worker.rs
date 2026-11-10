//! Dedicated thread that owns the models and runs the re-check engine.
//!
//! Audio goes straight from the capture thread into this worker's channel, so
//! everything said after the key press is buffered even while the model is
//! still loading ("nothing is lost").

use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread::JoinHandle,
    time::Instant,
};

use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, trace};

use super::{AsrError, Engine, engine::SAMPLE_RATE};
use crate::{
    agc::Agc,
    models::{self, LoadSpec},
};

/// Identifies one listening session (one press-to-press dictation).
pub type SessionId = u64;

/// Messages into the worker, processed strictly in order.
#[derive(Debug)]
pub enum AsrInput {
    /// A listening session starts. Loads the model if needed.
    Begin(SessionId),
    /// 16 kHz mono samples.
    Audio(Vec<f32>),
    /// The current session ends: transcribe what's left, then report `Finished`.
    End,
    /// Drop the models (idle unload).
    Unload,
    /// Use another model from the next session on (unloads the current one).
    SetModel(LoadSpec),
    Shutdown,
}

#[derive(Debug)]
pub enum AsrOutput {
    /// Model loading started (the session is buffering audio meanwhile).
    Loading,
    /// Model loaded and the session is being transcribed.
    Ready,
    /// The model couldn't be loaded; the session's audio is discarded.
    LoadFailed(AsrError),
    /// A committed word, already followed by its space (D-002).
    Text(String),
    /// Session finished; everything has been emitted.
    Finished(SessionId),
    /// Transcription failed for a chunk (the session continues).
    Error(AsrError),
}

pub struct AsrWorker {
    tx: Sender<AsrInput>,
    join: Option<JoinHandle<()>>,
}

impl AsrWorker {
    pub fn spawn(cfg: LoadSpec, out: UnboundedSender<AsrOutput>) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("murmur-asr".into())
            .spawn(move || run(cfg, rx, out))?;
        Ok(Self {
            tx,
            join: Some(join),
        })
    }

    /// A sender for audio and control messages (cloned into the capture thread).
    pub fn sender(&self) -> Sender<AsrInput> {
        self.tx.clone()
    }

    pub fn send(&self, msg: AsrInput) {
        // The worker only exits on Shutdown/drop, so a failed send means it
        // panicked; that has already been reported via the log.
        if self.tx.send(msg).is_err() {
            error!("ASR worker is gone");
        }
    }
}

impl Drop for AsrWorker {
    /// Blocks until the worker exits; drop it off the async runtime.
    fn drop(&mut self) {
        let _ = self.tx.send(AsrInput::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn load(spec: &LoadSpec) -> Result<Box<dyn Engine>, AsrError> {
    let t = Instant::now();
    let engine = models::load_engine(spec)?;
    info!(model = spec.model.id, elapsed = ?t.elapsed(), "model loaded");
    Ok(engine)
}

/// VAD state is sampled per push; feed backlogs in slices this long (seconds)
/// so a pause inside a backlog is still noticed.
const PUSH_SLICE: f64 = 0.1;

struct Session {
    id: SessionId,
    /// False if the model failed to load: audio is dropped.
    transcribing: bool,
    agc: Agc,
}

fn run(mut cfg: LoadSpec, rx: Receiver<AsrInput>, out: UnboundedSender<AsrOutput>) {
    let mut engine: Option<Box<dyn Engine>> = None;
    let mut next_model: Option<LoadSpec> = None;
    let mut session: Option<Session> = None;
    let mut held: Option<AsrInput> = None;
    let emit = |words: Vec<super::words::Word>| {
        for w in words {
            trace!(word = %w.text, start = w.start, end = w.end, "commit");
            let _ = out.send(AsrOutput::Text(format!("{} ", w.text)));
        }
    };
    loop {
        let msg = match held.take() {
            Some(m) => m,
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            },
        };
        match msg {
            AsrInput::Begin(id) => {
                if let Some(spec) = next_model.take() {
                    cfg = spec;
                    engine = None;
                }
                if engine.is_none() {
                    let _ = out.send(AsrOutput::Loading);
                    match load(&cfg) {
                        Ok(e) => engine = Some(e),
                        Err(e) => {
                            error!(error = %e, "model load failed");
                            let _ = out.send(AsrOutput::LoadFailed(e));
                        }
                    }
                }
                if let Some(e) = engine.as_mut() {
                    e.reset();
                    let _ = out.send(AsrOutput::Ready);
                }
                session = Some(Session {
                    id,
                    transcribing: engine.is_some(),
                    agc: Agc::new(SAMPLE_RATE as u32),
                });
            }
            AsrInput::Audio(mut batch) => {
                // Drain whatever else is queued (e.g. audio buffered during load).
                while let Ok(next) = rx.try_recv() {
                    match next {
                        AsrInput::Audio(more) => batch.extend_from_slice(&more),
                        other => {
                            held = Some(other);
                            break;
                        }
                    }
                }
                let (Some(s), Some(e)) = (session.as_mut(), engine.as_mut()) else {
                    continue;
                };
                if !s.transcribing {
                    continue;
                }
                let slice = (PUSH_SLICE * SAMPLE_RATE as f64) as usize;
                for part in batch.chunks_mut(slice) {
                    s.agc.process(part);
                    match e.push(part) {
                        Ok(words) => emit(words),
                        Err(err) => {
                            error!(error = %err, "transcription failed");
                            let _ = out.send(AsrOutput::Error(err));
                        }
                    }
                }
            }
            AsrInput::End => {
                let Some(s) = session.take() else { continue };
                if let (true, Some(e)) = (s.transcribing, engine.as_mut()) {
                    match e.finish() {
                        Ok(words) => emit(words),
                        Err(err) => {
                            let _ = out.send(AsrOutput::Error(err));
                        }
                    }
                    let st = e.stats();
                    debug!(
                        decodes = st.decodes,
                        words = st.committed_words,
                        avg_lag = st.total_commit_lag / f64::from(st.committed_words.max(1)),
                        max_lag = st.max_commit_lag,
                        agc_gain = s.agc.gain(),
                        "session stats"
                    );
                    e.reset();
                }
                let _ = out.send(AsrOutput::Finished(s.id));
            }
            AsrInput::Unload => {
                if session.is_none() && engine.take().is_some() {
                    info!("model unloaded");
                }
            }
            AsrInput::SetModel(spec) => {
                if session.is_none() {
                    if engine.take().is_some() {
                        info!("model unloaded for a model change");
                    }
                    cfg = spec;
                } else {
                    // Takes effect when the next session begins.
                    next_model = Some(spec);
                }
            }
            AsrInput::Shutdown => break,
        }
    }
}
