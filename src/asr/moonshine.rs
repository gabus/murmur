//! Moonshine (v2, `.ort` encoder + merged decoder) via sherpa-onnx. English,
//! cased and punctuated, much lighter than Parakeet, but without timestamps.

use std::path::{Path, PathBuf};

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};

use super::{
    AsrError,
    engine::{SAMPLE_RATE, Transcriber},
    words::{Word, tokens_to_words},
};

pub struct Moonshine {
    recognizer: OfflineRecognizer,
}

impl Moonshine {
    pub fn load(dir: &Path, threads: i32) -> Result<Self, AsrError> {
        let file = |name: &str| -> Result<String, AsrError> {
            let p: PathBuf = dir.join(name);
            if !p.is_file() {
                return Err(AsrError::ModelMissing(p));
            }
            p.into_os_string()
                .into_string()
                .map_err(|p| AsrError::ModelMissing(PathBuf::from(p)))
        };
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.moonshine.encoder = Some(file("encoder_model.ort")?);
        config.model_config.moonshine.merged_decoder = Some(file("decoder_model_merged.ort")?);
        config.model_config.tokens = Some(file("tokens.txt")?);
        config.model_config.num_threads = threads;
        config.decoding_method = Some("greedy_search".into());
        let recognizer = OfflineRecognizer::create(&config)
            .ok_or_else(|| AsrError::ModelLoad(dir.to_path_buf()))?;
        Ok(Self { recognizer })
    }
}

impl Transcriber for Moonshine {
    fn transcribe(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE as i32, samples);
        self.recognizer.decode(&stream);
        let result = stream.get_result().ok_or(AsrError::Decode)?;
        // No timestamps: words get placeholder times (sequence mode ignores them).
        Ok(tokens_to_words(&result.tokens, &[], &[], 0.0))
    }

    fn has_timestamps(&self) -> bool {
        false
    }
}
