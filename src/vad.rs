//! Silero VAD (via sherpa-onnx), used only to detect the end of a phrase.
//! murmur never stops listening because of silence.

use std::path::Path;

use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};

use crate::asr::{AsrError, engine::SAMPLE_RATE, engine::SpeechDetector};

/// sherpa's VAD re-copies the whole current speech segment on every call, so
/// cost grows with the length of uninterrupted speech. We only need the
/// speech/silence state, so close the internal segment this often.
const SEGMENT_FLUSH_SECS: usize = 10;

pub struct SileroVad {
    vad: VoiceActivityDetector,
    /// Samples since the internal segment was last flushed, while in speech.
    in_segment: usize,
}

impl SileroVad {
    /// `min_silence` is the pause (seconds) after which a phrase counts as finished.
    pub fn load(model: &Path, min_silence: f32) -> Result<Self, AsrError> {
        if !model.is_file() {
            return Err(AsrError::ModelMissing(model.to_path_buf()));
        }
        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(model.to_string_lossy().into_owned()),
                threshold: 0.5,
                min_silence_duration: min_silence,
                min_speech_duration: 0.25,
                window_size: 512,
                // Above this, sherpa raises the threshold to force a cut; our
                // periodic flush keeps segments far shorter.
                max_speech_duration: 3.0 * SEGMENT_FLUSH_SECS as f32,
            },
            sample_rate: SAMPLE_RATE as i32,
            num_threads: 1,
            ..Default::default()
        };
        let vad = VoiceActivityDetector::create(&config, 60.0)
            .ok_or_else(|| AsrError::ModelLoad(model.to_path_buf()))?;
        Ok(Self { vad, in_segment: 0 })
    }
}

impl SpeechDetector for SileroVad {
    fn accept(&mut self, samples: &[f32]) {
        if self.in_segment >= SEGMENT_FLUSH_SECS * SAMPLE_RATE {
            // Flush *before* feeding new audio: the next window that still
            // contains speech re-opens a segment, so `is_speech` never
            // flickers to false here. The model's silence hangover is kept.
            self.vad.flush();
            self.in_segment = 0;
        }
        self.vad.accept_waveform(samples);
        // We don't use the segments; drop them so the internal queue stays empty.
        while !self.vad.is_empty() {
            self.vad.pop();
        }
        if self.vad.detected() {
            self.in_segment += samples.len();
        } else {
            self.in_segment = 0;
        }
    }

    fn is_speech(&self) -> bool {
        self.vad.detected()
    }

    fn reset(&mut self) {
        self.vad.reset();
        self.in_segment = 0;
    }
}
