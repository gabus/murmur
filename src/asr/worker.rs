//! Dedicated thread that owns the models and runs the re-check engine.
//!
//! Audio goes straight from the capture thread into this worker's channel, so
//! everything said after the key press is buffered even while the model is
//! still loading ("nothing is lost").

use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread::JoinHandle,
    time::Instant,
};

use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, trace};

use super::{
    AsrError,
    engine::{EngineConfig, RecheckEngine, SAMPLE_RATE},
    parakeet::Parakeet,
};
use crate::vad::SileroVad;

/// Messages into the worker, processed strictly in order.
#[derive(Debug)]
pub enum AsrInput {
    /// A listening session starts. Loads the model if needed.
    Begin,
    /// 16 kHz mono samples.
    Audio(Vec<f32>),
    /// The session ends: transcribe what's left, then report `Finished`.
    End,
    /// Drop the models (idle unload).
    Unload,
    Shutdown,
}

#[derive(Debug)]
pub enum AsrOutput {
    /// Model loading started (the session is buffering audio meanwhile).
    Loading,
    /// Model ready.
    Ready,
    /// A committed word, already followed by its space (D-002).
    Text(String),
    /// Session finished; everything has been emitted.
    Finished,
    Error(AsrError),
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub parakeet_dir: PathBuf,
    pub vad_model: PathBuf,
    /// Pause that ends a phrase (seconds).
    pub min_silence: f32,
    pub threads: i32,
    pub engine: EngineConfig,
}

pub struct AsrWorker {
    tx: Sender<AsrInput>,
    join: Option<JoinHandle<()>>,
}

impl AsrWorker {
    pub fn spawn(cfg: WorkerConfig, out: UnboundedSender<AsrOutput>) -> std::io::Result<Self> {
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
        // panicked; the error has already been reported via the log.
        if self.tx.send(msg).is_err() {
            error!("ASR worker is gone");
        }
    }
}

impl Drop for AsrWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(AsrInput::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

type Engine = RecheckEngine<Parakeet, SileroVad>;

fn load(cfg: &WorkerConfig) -> Result<Engine, AsrError> {
    let t = Instant::now();
    let vad = SileroVad::load(&cfg.vad_model, cfg.min_silence)?;
    let model = Parakeet::load(&cfg.parakeet_dir, cfg.threads)?;
    info!(elapsed = ?t.elapsed(), "model loaded");
    Ok(RecheckEngine::new(cfg.engine.clone(), model, vad))
}

/// VAD state is sampled per push; feed backlogs in slices this long (seconds)
/// so a pause inside a backlog is still noticed.
const PUSH_SLICE: f64 = 0.1;

fn run(cfg: WorkerConfig, rx: Receiver<AsrInput>, out: UnboundedSender<AsrOutput>) {
    let mut engine: Option<Engine> = None;
    let mut active = false;
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
            AsrInput::Begin => {
                active = true;
                match engine.as_mut() {
                    Some(e) => e.reset(),
                    None => {
                        let _ = out.send(AsrOutput::Loading);
                        match load(&cfg) {
                            Ok(e) => {
                                engine = Some(e);
                                let _ = out.send(AsrOutput::Ready);
                            }
                            Err(e) => {
                                error!(error = %e, "model load failed");
                                let _ = out.send(AsrOutput::Error(e));
                            }
                        }
                    }
                }
            }
            AsrInput::Audio(mut batch) => {
                if !active {
                    continue;
                }
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
                let Some(e) = engine.as_mut() else { continue };
                let slice = (PUSH_SLICE * SAMPLE_RATE as f64) as usize;
                for part in batch.chunks(slice) {
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
                if let Some(e) = engine.as_mut() {
                    match e.finish() {
                        Ok(words) => emit(words),
                        Err(err) => {
                            let _ = out.send(AsrOutput::Error(err));
                        }
                    }
                    let s = e.stats();
                    debug!(
                        decodes = s.decodes,
                        words = s.committed_words,
                        avg_lag = s.total_commit_lag / f64::from(s.committed_words.max(1)),
                        max_lag = s.max_commit_lag,
                        "session stats"
                    );
                    e.reset();
                }
                active = false;
                let _ = out.send(AsrOutput::Finished);
            }
            AsrInput::Unload => {
                if !active && engine.take().is_some() {
                    info!("model unloaded");
                }
            }
            AsrInput::Shutdown => break,
        }
    }
}
