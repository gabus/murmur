//! Microphone capture: 16 kHz mono f32, open only while listening.
//!
//! The cpal stream lives on its own thread (streams aren't `Send` everywhere).
//! `Capture::start` returns once the stream is running. Dropping or stopping it
//! closes the device.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use sherpa_onnx::LinearResampler;
use tokio::sync::watch;
use tracing::{info, warn};

pub const SAMPLE_RATE: u32 = 16_000;
/// Callbacks every 20 ms. The default buffer size gave ~1 s callbacks and
/// 2 s open latency with PulseAudio (docs/portal-findings.md).
const CALLBACKS_PER_SEC: u32 = 50;

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
/// Called (on the audio thread) when the stream fails after it started,
/// e.g. the device was unplugged.
pub type OnError = Box<dyn Fn(String) + Send + Sync + 'static>;

/// Voice-level step (0..icons::LEVELS) for the tray icon, pushed from the audio
/// thread at most every `LEVEL_INTERVAL` and only when it changes.
#[derive(Debug, Clone)]
pub struct Level {
    tx: Arc<watch::Sender<u8>>,
    last: Arc<Mutex<Option<Instant>>>,
}

const LEVEL_INTERVAL: Duration = Duration::from_millis(100);

impl Default for Level {
    fn default() -> Self {
        Self::new().0
    }
}

impl Level {
    pub fn new() -> (Self, watch::Receiver<u8>) {
        let (tx, rx) = watch::channel(0);
        (
            Self {
                tx: Arc::new(tx),
                last: Arc::new(Mutex::new(None)),
            },
            rx,
        )
    }

    /// Records the RMS of a chunk (0..1).
    pub(crate) fn set(&self, rms: f32) {
        let step = crate::icons::level_step(rms);
        let now = Instant::now();
        if let Ok(mut last) = self.last.lock() {
            if last.is_some_and(|t| now.duration_since(t) < LEVEL_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        self.tx.send_if_modified(|cur| {
            let changed = *cur != step;
            *cur = step;
            changed
        });
    }

    /// Back to silence (when listening stops).
    pub fn reset(&self) {
        self.tx
            .send_if_modified(|cur| std::mem::replace(cur, 0) != 0);
    }
}

pub struct Capture {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Opens the default input device and starts streaming to `sink`.
    /// Blocks until the stream runs (cpal's `play` waits for the first buffer).
    pub fn start(
        device: &str,
        sink: Sink,
        on_error: OnError,
        level: Level,
    ) -> Result<Self, AudioError> {
        let device = device.to_owned();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), AudioError>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("murmur-audio".into())
            .spawn(move || {
                #[cfg(feature = "dev-harness")]
                if let Some(path) = std::env::var_os("MURMUR_DEV_AUDIO") {
                    crate::dev::play_file(path.into(), sink, on_error, level, ready_tx, stop_rx);
                    return;
                }
                let stream = match open(&device, sink, on_error, level) {
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

struct Shared {
    sink: Mutex<Sink>,
    on_error: OnError,
    level: Level,
}

/// PulseAudio (lists real microphones by name), else whatever cpal defaults to.
/// Logs why PulseAudio isn't used, once.
fn host() -> cpal::Host {
    cpal::host_from_id(host_id()).unwrap_or_else(|_| cpal::default_host())
}

fn host_id() -> cpal::HostId {
    // Decided once: each failed PulseAudio attempt can take seconds.
    static CHOSEN: std::sync::OnceLock<cpal::HostId> = std::sync::OnceLock::new();
    *CHOSEN.get_or_init(|| match cpal::host_from_id(cpal::HostId::PulseAudio) {
        Ok(_) => cpal::HostId::PulseAudio,
        Err(e) => {
            warn!(error = %e, "PulseAudio unavailable, using ALSA");
            cpal::HostId::Alsa
        }
    })
}

/// Connects to PipeWire/PulseAudio afresh and lists its microphones; None when no server
/// answers (blocking). Unlike the host picked at startup this isn't cached, so the system
/// check sees a server that started later.
pub fn probe_server() -> Option<Vec<Mic>> {
    cpal::host_from_id(cpal::HostId::PulseAudio)
        .ok()
        .map(|h| list_microphones(&h))
}

/// A microphone: a stable id for the config and a label for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mic {
    /// cpal device id, e.g. `pulseaudio:alsa_input.usb-…` (locale-independent).
    pub id: String,
    pub label: String,
}

/// The available microphones (blocking; call off the async runtime).
/// Duplicate labels get a numeric suffix so they stay distinguishable.
pub fn microphones() -> Vec<Mic> {
    list_microphones(&host())
}

fn list_microphones(host: &cpal::Host) -> Vec<Mic> {
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut mics: Vec<Mic> = devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let label = d.description().ok()?.name().to_owned();
            Some(Mic { id, label })
        })
        // PulseAudio lists every output's monitor as an input; that's not a mic.
        .filter(|m| !m.id.ends_with(".monitor") && !m.label.starts_with("Monitor of "))
        .collect();
    mics.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
    mics.dedup_by(|a, b| a.id == b.id);
    let labels: Vec<String> = mics.iter().map(|m| m.label.clone()).collect();
    for (i, m) in mics.iter_mut().enumerate() {
        let n = labels[..i].iter().filter(|l| **l == m.label).count();
        if n > 0 {
            m.label = format!("{} ({})", m.label, n + 1);
        }
    }
    mics
}

/// Finds the device by id (or, for older configs, by label).
fn pick_device(host: &cpal::Host, wanted: &str) -> Option<cpal::Device> {
    if !wanted.is_empty()
        && let Ok(mut devices) = host.input_devices()
        && let Some(d) = devices.find(|d| {
            d.id().is_ok_and(|id| id.to_string() == wanted)
                || d.description().is_ok_and(|n| n.name() == wanted)
        })
    {
        return Some(d);
    }
    if !wanted.is_empty() {
        warn!(
            device = wanted,
            "selected microphone not found, using the default"
        );
    }
    host.default_input_device()
}

fn open(
    name: &str,
    sink: Sink,
    on_error: OnError,
    level: Level,
) -> Result<cpal::Stream, AudioError> {
    let host = host();
    let device = pick_device(&host, name).ok_or(AudioError::NoDevice)?;
    let name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_default();
    info!(host = ?host.id(), device = %name, "opening microphone");
    let shared = Arc::new(Shared {
        sink: Mutex::new(sink),
        on_error,
        level,
    });

    let direct = cpal::StreamConfig {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Fixed(SAMPLE_RATE / CALLBACKS_PER_SEC),
    };
    match build::<f32>(
        &device,
        &direct,
        Converter::new(1, SAMPLE_RATE)?,
        shared.clone(),
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
    let frames = def.sample_rate() / CALLBACKS_PER_SEC;
    let buffer_size = match def.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            cpal::BufferSize::Fixed(frames.clamp(*min, *max))
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(frames),
    };
    let config = cpal::StreamConfig {
        channels: def.channels(),
        sample_rate: def.sample_rate(),
        buffer_size,
    };
    let conv = || Converter::new(def.channels(), def.sample_rate());
    let stream = match def.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, conv()?, shared),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, conv()?, shared),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, conv()?, shared),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, conv()?, shared),
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
    shared: Arc<Shared>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let errors = shared.clone();
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _| {
                let out = conv.process(data);
                if out.is_empty() {
                    return;
                }
                let rms = (out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt();
                shared.level.set(rms);
                if let Ok(mut s) = shared.sink.lock() {
                    s(out);
                }
            },
            move |e| {
                warn!(error = %e, "audio stream error");
                (errors.on_error)(e.to_string());
            },
            None,
        )
        .map_err(|e| AudioError::Stream(e.to_string()))
}

/// Downmixes to mono and resamples to 16 kHz (kaldi's windowed-sinc
/// resampler from sherpa-onnx, so 48 kHz input doesn't alias).
struct Converter {
    channels: usize,
    resampler: Option<LinearResampler>,
}

impl Converter {
    fn new(channels: u16, rate: u32) -> Result<Self, AudioError> {
        let resampler = if rate == SAMPLE_RATE {
            None
        } else {
            let r = LinearResampler::create(rate as i32, SAMPLE_RATE as i32)
                .ok_or_else(|| AudioError::Config(format!("cannot resample from {rate} Hz")))?;
            Some(r)
        };
        Ok(Self {
            channels: usize::from(channels.max(1)),
            resampler,
        })
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
        match &self.resampler {
            None => mono,
            Some(r) => r.resample(&mono, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(ch: u16, rate: u32) -> Converter {
        Converter::new(ch, rate).expect("resampler for a standard rate")
    }

    #[test]
    fn passthrough_at_16k_mono() {
        let mut c = conv(1, 16_000);
        assert_eq!(c.process(&[0.5f32, -0.5]), vec![0.5, -0.5]);
    }

    #[test]
    fn downmixes_stereo() {
        let mut c = conv(2, 16_000);
        assert_eq!(c.process(&[1.0f32, 0.0, -1.0, -1.0]), vec![0.5, -1.0]);
    }

    #[test]
    fn resamples_48k_and_44k1_to_16k() {
        for rate in [48_000u32, 44_100] {
            let mut c = conv(1, rate);
            let mut total = 0;
            // 1 s of input in 10 ms callbacks
            for _ in 0..100 {
                total += c.process(&vec![0.0f32; (rate / 100) as usize]).len();
            }
            assert!((15_900..=16_000).contains(&total), "{rate}: {total}");
        }
    }

    #[test]
    fn i16_input_is_scaled() {
        let mut c = conv(1, 16_000);
        let out = c.process(&[i16::MAX, i16::MIN]);
        assert!((out[0] - 1.0).abs() < 1e-3 && (out[1] + 1.0).abs() < 1e-3);
    }
}
