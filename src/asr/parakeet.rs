//! Parakeet TDT (NeMo transducer) via sherpa-onnx's offline recognizer.

use std::path::{Path, PathBuf};

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};

use super::{
    AsrError,
    engine::{SAMPLE_RATE, Transcriber},
    words::{Word, tokens_to_words},
};

pub struct Parakeet {
    recognizer: OfflineRecognizer,
}

impl Parakeet {
    /// Loads the int8 Parakeet TDT model from `dir` (encoder/decoder/joiner + tokens.txt).
    pub fn load(dir: &Path, threads: i32) -> Result<Self, AsrError> {
        let file = |name: &str| -> Result<String, AsrError> {
            let p: PathBuf = dir.join(name);
            // sherpa-onnx may exit the process on some malformed inputs; at least
            // make sure every file exists before handing paths to it.
            if !p.is_file() {
                return Err(AsrError::ModelMissing(p));
            }
            p.into_os_string()
                .into_string()
                .map_err(|p| AsrError::ModelMissing(PathBuf::from(p)))
        };
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(file("encoder.int8.onnx")?);
        config.model_config.transducer.decoder = Some(file("decoder.int8.onnx")?);
        config.model_config.transducer.joiner = Some(file("joiner.int8.onnx")?);
        config.model_config.tokens = Some(file("tokens.txt")?);
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.num_threads = threads;
        // modified_beam_search drops speech for TDT models (sherpa-onnx #4084).
        config.decoding_method = Some("greedy_search".into());
        let recognizer = OfflineRecognizer::create(&config)
            .ok_or_else(|| AsrError::ModelLoad(dir.to_path_buf()))?;
        Ok(Self { recognizer })
    }
}

impl Transcriber for Parakeet {
    fn transcribe(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE as i32, samples);
        self.recognizer.decode(&stream);
        let result = stream.get_result().ok_or(AsrError::Decode)?;
        Ok(tokens_to_words(
            &result.tokens,
            result.timestamps.as_deref().unwrap_or_default(),
            result.durations.as_deref().unwrap_or_default(),
            0.0,
        ))
    }
}
