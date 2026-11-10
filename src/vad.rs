//! Silero VAD (via sherpa-onnx), used only to detect the end of a phrase.
//! murmur never stops listening because of silence.

use std::path::Path;

use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};

use crate::asr::{AsrError, engine::SAMPLE_RATE, engine::SpeechDetector};

pub struct SileroVad {
    vad: VoiceActivityDetector,
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
                // We only use the speech/silence state; never split long speech.
                max_speech_duration: 3600.0,
            },
            sample_rate: SAMPLE_RATE as i32,
            num_threads: 1,
            ..Default::default()
        };
        let vad = VoiceActivityDetector::create(&config, 60.0)
            .ok_or_else(|| AsrError::ModelLoad(model.to_path_buf()))?;
        Ok(Self { vad })
    }
}

impl SpeechDetector for SileroVad {
    fn accept(&mut self, samples: &[f32]) {
        self.vad.accept_waveform(samples);
        // We don't use the segments; drop them so the internal queue stays empty.
        while !self.vad.is_empty() {
            self.vad.pop();
        }
    }

    fn is_speech(&self) -> bool {
        self.vad.detected()
    }

    fn reset(&mut self) {
        self.vad.reset();
    }
}
