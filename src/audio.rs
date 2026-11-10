//! Microphone capture: 16 kHz mono f32, open only while listening.
//!
//! The cpal stream lives on its own thread (streams aren't `Send` everywhere).
//! `Capture::start` returns once the stream is running. Dropping or stopping it
//! closes the device.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use tracing::{info, warn};

pub const SAMPLE_RATE: u32 = 16_000;
/// 20 ms callbacks. The default buffer size gave ~1 s callbacks and 2 s open
/// latency with PulseAudio (docs/portal-findings.md).
const FRAMES: u32 = 320;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no microphone found")]
    NoDevice,
    #[error("microphone configuration not supported: {0}")]
    Config(String),
    #[error("could not open the microphone: {0}")]
    Stream(String),
    #[error("audio thread failed: {0}")]
    Thread(String),
}

/// Receives captured audio (called on the audio thread; must not block).
pub type Sink = Box<dyn FnMut(Vec<f32>) + Send + 'static>;

/// Current input level (RMS of the last chunk, f32 bits) for the tray icon.
#[derive(Debug, Clone, Default)]
pub struct Level(Arc<AtomicU32>);

impl Level {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

pub struct Capture {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Opens the default input device and starts streaming to `sink`.
    pub fn start(sink: Sink, level: Level) -> Result<Self, AudioError> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), AudioError>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("murmur-audio".into())
            .spawn(move || {
                let stream = match open(sink, level) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                // Block until stop() or the Capture is dropped.
                let _ = stop_rx.recv();
                drop(stream);
            })
            .map_err(|e| AudioError::Thread(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(AudioError::Thread("audio thread exited".into()))
            }
        }
    }

    /// Closes the stream and waits for the audio thread to exit.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open(sink: Sink, level: Level) -> Result<cpal::Stream, AudioError> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or(AudioError::NoDevice)?;
    let name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_default();
    info!(host = ?host.id(), device = %name, "opening microphone");

    let direct = cpal::StreamConfig {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Fixed(FRAMES),
    };
    let sink = Arc::new(std::sync::Mutex::new(sink));
    match build::<f32>(
        &device,
        &direct,
        Converter::new(1, SAMPLE_RATE),
        sink.clone(),
        level.clone(),
    ) {
        Ok(stream) => {
            stream
                .play()
                .map_err(|e| AudioError::Stream(e.to_string()))?;
            return Ok(stream);
        }
        Err(e) => warn!(error = %e, "16 kHz mono not accepted, converting from the device format"),
    }

    let def = device
        .default_input_config()
        .map_err(|e| AudioError::Config(e.to_string()))?;
    let config = cpal::StreamConfig {
        channels: def.channels(),
        sample_rate: def.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let conv = Converter::new(def.channels(), def.sample_rate());
    let stream = match def.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, conv, sink, level),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, conv, sink, level),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, conv, sink, level),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, conv, sink, level),
        other => return Err(AudioError::Config(format!("sample format {other:?}"))),
    }?;
    stream
        .play()
        .map_err(|e| AudioError::Stream(e.to_string()))?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut conv: Converter,
    sink: Arc<std::sync::Mutex<Sink>>,
    level: Level,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _| {
                let out = conv.process(data);
                if out.is_empty() {
                    return;
                }
                let rms = (out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt();
                level.set(rms);
                if let Ok(mut s) = sink.lock() {
                    s(out);
                }
            },
            |e| warn!(error = %e, "audio stream error"),
            None,
        )
        .map_err(|e| AudioError::Stream(e.to_string()))
}

/// Downmixes to mono and resamples to 16 kHz. Integer ratios (48k, 32k) are
/// decimated with a box filter; others use linear interpolation.
struct Converter {
    channels: usize,
    rate: u32,
    /// Fractional read position for linear resampling, in input frames.
    pos: f64,
    prev: f32,
    acc: Vec<f32>,
}

impl Converter {
    fn new(channels: u16, rate: u32) -> Self {
        Self {
            channels: usize::from(channels.max(1)),
            rate,
            pos: 0.0,
            prev: 0.0,
            acc: Vec::new(),
        }
    }

    fn process<T>(&mut self, data: &[T]) -> Vec<f32>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let mono: Vec<f32> = if self.channels == 1 {
            data.iter().map(|s| s.to_sample::<f32>()).collect()
        } else {
            data.chunks(self.channels)
                .map(|f| f.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / f.len() as f32)
                .collect()
        };
        if self.rate == SAMPLE_RATE {
            return mono;
        }
        if self.rate.is_multiple_of(SAMPLE_RATE) {
            let n = (self.rate / SAMPLE_RATE) as usize;
            self.acc.extend_from_slice(&mono);
            let whole = self.acc.len() / n * n;
            let out = self.acc[..whole]
                .chunks(n)
                .map(|c| c.iter().sum::<f32>() / n as f32)
                .collect();
            self.acc.drain(..whole);
            return out;
        }
        // Linear interpolation between `prev` (frame -1) and the new frames.
        let step = f64::from(self.rate) / f64::from(SAMPLE_RATE);
        let mut out = Vec::with_capacity((mono.len() as f64 / step) as usize + 1);
        while self.pos < mono.len() as f64 {
            let i = self.pos.floor() as isize;
            let frac = (self.pos - self.pos.floor()) as f32;
            let a = if i == 0 {
                self.prev
            } else {
                mono[(i - 1) as usize]
            };
            let b = mono[i as usize];
            out.push(a + (b - a) * frac);
            self.pos += step;
        }
        self.pos -= mono.len() as f64;
        if let Some(last) = mono.last() {
            self.prev = *last;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_at_16k_mono() {
        let mut c = Converter::new(1, 16_000);
        assert_eq!(c.process(&[0.5f32, -0.5]), vec![0.5, -0.5]);
    }

    #[test]
    fn downmix_and_decimate_48k_stereo() {
        let mut c = Converter::new(2, 48_000);
        // 6 stereo frames → 2 output samples
        let data = [
            1.0f32, 0.0, 1.0, 0.0, 1.0, 0.0, -1.0, -1.0, -1.0, -1.0, -1.0, -1.0,
        ];
        assert_eq!(c.process(&data), vec![0.5, -1.0]);
        // leftovers carry over between callbacks
        assert!(c.process(&[1.0f32, 1.0]).is_empty());
        assert_eq!(c.process(&[1.0f32, 1.0, 1.0, 1.0]), vec![1.0]);
    }

    #[test]
    fn linear_resample_44k1_length() {
        let mut c = Converter::new(1, 44_100);
        let mut total = 0;
        for _ in 0..100 {
            total += c.process(&[0.0f32; 441]).len();
        }
        // 1 s of input → 16000 ± 1 samples
        assert!((15_999..=16_001).contains(&total), "{total}");
    }

    #[test]
    fn i16_input_is_scaled() {
        let mut c = Converter::new(1, 16_000);
        let out = c.process(&[i16::MAX, i16::MIN]);
        assert!((out[0] - 1.0).abs() < 1e-3 && (out[1] + 1.0).abs() < 1e-3);
    }
}
