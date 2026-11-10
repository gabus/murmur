//! Light engine: a streaming transducer (Zipformer) with greedy decoding,
//! plus a separate punctuation + casing model.
//!
//! Greedy streaming output is append-only, so a word can be typed once it is
//! `commit_delay` old and no longer the last (still growing) word. Punctuation
//! is computed over the whole phrase so far each time words are committed, and
//! once more for the rest of the phrase at a pause.

use std::path::{Path, PathBuf};

use sherpa_onnx::{
    OnlinePunctuation, OnlinePunctuationConfig, OnlinePunctuationModelConfig, OnlineRecognizer,
    OnlineRecognizerConfig, OnlineStream,
};
use tracing::debug;

use super::{
    AsrError, Engine, EngineStats,
    engine::{SAMPLE_RATE, SpeechDetector},
    words::{Word, tokens_to_words},
};
use crate::vad::SileroVad;

const SR: f64 = SAMPLE_RATE as f64;
/// Silence fed before `input_finished`, so the last chunk (and word) is decoded.
const TAIL_PADDING_SECS: f64 = 0.6;
/// Restart the stream after this much silence with no words (bounds memory).
const IDLE_STREAM_RESET: f64 = 10.0;

pub struct LightEngine {
    recognizer: OnlineRecognizer,
    stream: OnlineStream,
    punct: OnlinePunctuation,
    vad: SileroVad,
    commit_delay: f64,
    /// Samples received this session.
    total: u64,
    /// Absolute time the current stream started.
    phrase_start: f64,
    /// Words of the current phrase already committed.
    committed: usize,
    /// Raw words of the previous result (to detect non-append-only output).
    last_raw: Vec<String>,
    was_speech: bool,
    stats: EngineStats,
    /// Times an earlier word changed after being output (should stay 0).
    pub append_violations: u32,
}

fn path(dir: &Path, name: &str) -> Result<String, AsrError> {
    let p: PathBuf = dir.join(name);
    if !p.is_file() {
        return Err(AsrError::ModelMissing(p));
    }
    p.into_os_string()
        .into_string()
        .map_err(|p| AsrError::ModelMissing(PathBuf::from(p)))
}

impl LightEngine {
    /// `files` are the encoder, decoder, joiner and tokens file names in `asr_dir`.
    pub fn load(
        asr_dir: &Path,
        files: [&str; 4],
        punct_dir: &Path,
        vad: SileroVad,
        threads: i32,
        commit_delay: f64,
    ) -> Result<Self, AsrError> {
        let [encoder, decoder, joiner, tokens] = files;
        let mut config = OnlineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(path(asr_dir, encoder)?);
        config.model_config.transducer.decoder = Some(path(asr_dir, decoder)?);
        config.model_config.transducer.joiner = Some(path(asr_dir, joiner)?);
        config.model_config.tokens = Some(path(asr_dir, tokens)?);
        config.model_config.num_threads = threads;
        config.decoding_method = Some("greedy_search".into());
        let recognizer = OnlineRecognizer::create(&config)
            .ok_or_else(|| AsrError::ModelLoad(asr_dir.to_path_buf()))?;
        let punct = OnlinePunctuation::create(&OnlinePunctuationConfig {
            model: OnlinePunctuationModelConfig {
                cnn_bilstm: Some(path(punct_dir, "model.int8.onnx")?),
                bpe_vocab: Some(path(punct_dir, "bpe.vocab")?),
                num_threads: 1,
                ..Default::default()
            },
        })
        .ok_or_else(|| AsrError::ModelLoad(punct_dir.to_path_buf()))?;
        let stream = recognizer.create_stream();
        Ok(Self {
            recognizer,
            stream,
            punct,
            vad,
            commit_delay,
            total: 0,
            phrase_start: 0.0,
            committed: 0,
            last_raw: Vec::new(),
            was_speech: false,
            stats: EngineStats::default(),
            append_violations: 0,
        })
    }

    fn now(&self) -> f64 {
        self.total as f64 / SR
    }

    fn decode(&mut self) -> Vec<Word> {
        while self.recognizer.is_ready(&self.stream) {
            self.recognizer.decode(&self.stream);
            self.stats.decodes += 1;
        }
        let Some(r) = self.recognizer.get_result(&self.stream) else {
            return Vec::new();
        };
        let words = tokens_to_words(
            &r.tokens,
            r.timestamps.as_deref().unwrap_or_default(),
            &[],
            self.phrase_start,
        );
        // Append-only check: every word but the last must keep its text.
        let raw: Vec<String> = words.iter().map(|w| w.text.clone()).collect();
        let stable = self.last_raw.len().saturating_sub(1);
        if raw.len() < stable || raw[..stable] != self.last_raw[..stable] {
            self.append_violations += 1;
            debug!(
                violations = self.append_violations,
                "streaming output changed earlier words"
            );
        }
        if tracing::enabled!(tracing::Level::TRACE) {
            tracing::trace!(now = self.now(), words = ?self.last_raw, "light result");
        }
        self.last_raw = raw;
        words
    }

    /// Punctuated + cased forms of `words` (falls back to lowercase if the
    /// punctuation model changes the word count).
    fn punctuate(&self, words: &[Word]) -> Vec<String> {
        let lower: Vec<String> = words.iter().map(|w| w.text.to_lowercase()).collect();
        let text = lower.join(" ");
        if text.is_empty() {
            return Vec::new();
        }
        match self.punct.add_punctuation(&text) {
            Some(p) => {
                let out: Vec<String> = p.split_whitespace().map(str::to_owned).collect();
                if out.len() == lower.len() { out } else { lower }
            }
            None => lower,
        }
    }

    fn commit_upto(&mut self, words: &[Word], upto: usize) -> Vec<Word> {
        if upto <= self.committed {
            return Vec::new();
        }
        let forms = self.punctuate(words);
        let now = self.now();
        let out: Vec<Word> = (self.committed..upto)
            .map(|i| Word {
                text: forms
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| words[i].text.to_lowercase()),
                start: words[i].start,
                end: words[i].end,
            })
            .collect();
        for w in &out {
            let lag = (now - w.end).max(0.0);
            self.stats.committed_words += 1;
            self.stats.total_commit_lag += lag;
            self.stats.max_commit_lag = self.stats.max_commit_lag.max(lag);
        }
        self.committed = upto;
        out
    }

    /// Ends the current phrase: decode everything and commit the rest.
    fn end_phrase(&mut self) -> Vec<Word> {
        let pad = vec![0.0f32; (TAIL_PADDING_SECS * SR) as usize];
        self.stream.accept_waveform(SAMPLE_RATE as i32, &pad);
        self.stream.input_finished();
        let words = self.decode();
        let mut out = self.commit_upto(&words, words.len());
        // A pause ends the sentence (spec): make sure it has final punctuation.
        if let Some(last) = out.last_mut()
            && !last.text.ends_with(['.', '?', '!', ',', ';', ':', '…'])
        {
            last.text.push('.');
        }
        self.new_stream();
        out
    }

    fn new_stream(&mut self) {
        self.stream = self.recognizer.create_stream();
        self.phrase_start = self.now();
        self.committed = 0;
        self.last_raw.clear();
    }
}

impl Engine for LightEngine {
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError> {
        self.total += samples.len() as u64;
        self.vad.accept(samples);
        self.stream.accept_waveform(SAMPLE_RATE as i32, samples);
        let speech = self.vad.is_speech();
        let ended = self.was_speech && !speech;
        self.was_speech = speech;
        if ended {
            return Ok(self.end_phrase());
        }
        let words = self.decode();
        if words.is_empty() && !speech && self.now() - self.phrase_start > IDLE_STREAM_RESET {
            self.new_stream();
            return Ok(Vec::new());
        }
        // The last word may still grow; older ones are final once old enough.
        let now = self.now();
        let upto = words[..words.len().saturating_sub(1)]
            .iter()
            .take_while(|w| w.end <= now - self.commit_delay)
            .count();
        Ok(self.commit_upto(&words, upto))
    }

    fn finish(&mut self) -> Result<Vec<Word>, AsrError> {
        Ok(self.end_phrase())
    }

    fn reset(&mut self) {
        self.total = 0;
        self.vad.reset();
        self.was_speech = false;
        self.stats = EngineStats::default();
        self.new_stream();
    }

    fn stats(&self) -> EngineStats {
        self.stats.clone()
    }
}
