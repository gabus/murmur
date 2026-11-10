//! Development harness (cargo feature `dev-harness`, never in release builds).
//!
//! `MURMUR_DEV_AUDIO=<file.wav>` replaces the microphone with a WAV played in
//! real time, so the whole daemon can be tested end to end without a mic.
//! When the file ends, `MURMUR_DEV_ON_EOF` decides what happens (see main.rs).

use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};

use tracing::info;

use crate::audio::{AudioError, Level, OnError, SAMPLE_RATE, Sink};

/// Notified once when the dev audio file has been fully played.
pub static EOF: tokio::sync::Notify = tokio::sync::Notify::const_new();

fn read_wav(path: &PathBuf) -> Result<Vec<f32>, AudioError> {
    let err = |e: hound::Error| AudioError::Stream(format!("{}: {e}", path.display()));
    let mut r = hound::WavReader::open(path).map_err(err)?;
    let spec = r.spec();
    let ch = usize::from(spec.channels.max(1));
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()
                .map_err(err)?
        }
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>().map_err(err)?,
    };
    let mono: Vec<f32> = raw
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / f.len() as f32)
        .collect();
    if spec.sample_rate == SAMPLE_RATE {
        return Ok(mono);
    }
    let rs = sherpa_onnx::LinearResampler::create(spec.sample_rate as i32, SAMPLE_RATE as i32)
        .ok_or_else(|| AudioError::Config("resampler".into()))?;
    Ok(rs.resample(&mono, true))
}

/// Plays `path` into `sink` in 20 ms chunks at real-time pace until the file
/// ends (then silence) or `stop` fires.
pub(crate) fn play_file(
    path: PathBuf,
    mut sink: Sink,
    _on_error: OnError,
    level: Level,
    ready: Sender<Result<(), AudioError>>,
    stop: Receiver<()>,
) {
    let samples = match read_wav(&path) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    info!(file = %path.display(), secs = samples.len() as f32 / SAMPLE_RATE as f32, "dev audio source");
    let _ = ready.send(Ok(()));
    let chunk = (SAMPLE_RATE / 50) as usize;
    let start = Instant::now();
    let mut sent = 0usize;
    loop {
        let out: Vec<f32> = if sent < samples.len() {
            samples[sent..(sent + chunk).min(samples.len())].to_vec()
        } else {
            if sent < samples.len() + chunk {
                EOF.notify_one();
            }
            vec![0.0; chunk]
        };
        sent += chunk;
        let rms = (out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt();
        level.set(rms);
        sink(out);
        // Pace like a real device: wait until this chunk's wall-clock time.
        let due = start + Duration::from_secs_f64(sent as f64 / f64::from(SAMPLE_RATE));
        match stop.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
    }
}
