//! Re-check engine: re-transcribes the current sentence buffer every
//! `rerun_interval` of audio and commits words two runs agree on.
//!
//! All times are *audio* time (samples received / 16 kHz), so the engine is
//! deterministic for a given input and can be tested with recorded audio.

use super::{
    AsrError,
    committer::{Committer, SeqCommitter},
    words::{Word, ends_sentence},
};

pub const SAMPLE_RATE: usize = 16_000;
const SR: f64 = SAMPLE_RATE as f64;

/// Speech-to-words model run on a whole buffer.
pub trait Transcriber {
    /// Transcribes 16 kHz mono samples. Word times are relative to `samples[0]`.
    fn transcribe(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError>;

    /// False for models without token timestamps (e.g. Moonshine): their word
    /// times are meaningless, so the engine aligns by word sequence instead.
    fn has_timestamps(&self) -> bool {
        true
    }
}

impl Transcriber for Box<dyn Transcriber + Send> {
    fn transcribe(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError> {
        (**self).transcribe(samples)
    }
    fn has_timestamps(&self) -> bool {
        (**self).has_timestamps()
    }
}

/// Voice activity detection with hangover: `is_speech` stays true until the
/// configured minimum silence has passed.
pub trait SpeechDetector {
    fn accept(&mut self, samples: &[f32]);
    fn is_speech(&self) -> bool;
    fn reset(&mut self);
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Re-transcribe after this much new audio (seconds).
    pub rerun_interval: f64,
    /// A word must have ended at least this long ago to be committed (seconds).
    pub min_age: f64,
    /// Audio kept before the first detected speech, so onsets aren't clipped (seconds).
    pub preroll: f64,
    /// Above this buffer length, trim at the last committed word even mid-sentence (seconds).
    pub max_buffer: f64,
    /// Hard cap: if the buffer still grows past this (nothing agrees), flush it (seconds).
    pub hard_max_buffer: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            rerun_interval: 0.5,
            min_age: 0.5,
            preroll: 0.5,
            max_buffer: 15.0,
            hard_max_buffer: 30.0,
        }
    }
}

/// Statistics for the current session (for logs and benchmarks).
#[derive(Debug, Default, Clone)]
pub struct EngineStats {
    pub decodes: u32,
    pub committed_words: u32,
    /// Sum of (audio time at commit − word end) over committed words.
    pub total_commit_lag: f64,
    pub max_commit_lag: f64,
}

pub struct RecheckEngine<T, V> {
    cfg: EngineConfig,
    transcriber: T,
    vad: V,
    buf: Vec<f32>,
    /// Absolute time of `buf[0]`.
    buf_start: f64,
    /// Samples received this session.
    total: u64,
    /// `buf.len()` at the last decode.
    decoded_len: usize,
    heard_speech: bool,
    was_speech: bool,
    commit: Commit,
    stats: EngineStats,
}

/// How hypotheses are aligned: by word times, or by word sequence for models
/// without timestamps (then the buffer is only cut at pauses).
#[derive(Debug)]
enum Commit {
    Timed(Committer),
    Seq(SeqCommitter),
}

impl Commit {
    fn for_model(timestamps: bool) -> Self {
        if timestamps {
            Self::Timed(Committer::new())
        } else {
            Self::Seq(SeqCommitter::new())
        }
    }
}

impl<T: Transcriber, V: SpeechDetector> RecheckEngine<T, V> {
    pub fn new(cfg: EngineConfig, transcriber: T, vad: V) -> Self {
        let commit = Commit::for_model(transcriber.has_timestamps());
        Self {
            cfg,
            transcriber,
            vad,
            buf: Vec::new(),
            buf_start: 0.0,
            total: 0,
            decoded_len: 0,
            heard_speech: false,
            was_speech: false,
            commit,
            stats: EngineStats::default(),
        }
    }

    /// Starts a new listening session (time restarts at 0).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf_start = 0.0;
        self.total = 0;
        self.decoded_len = 0;
        self.heard_speech = false;
        self.was_speech = false;
        self.commit = Commit::for_model(self.transcriber.has_timestamps());
        self.stats = EngineStats::default();
        self.vad.reset();
    }

    pub fn stats(&self) -> &EngineStats {
        &self.stats
    }

    /// Audio time at the end of everything received.
    pub fn now(&self) -> f64 {
        self.total as f64 / SR
    }

    /// Feeds captured audio. Returns words that became final.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError> {
        self.buf.extend_from_slice(samples);
        self.total += samples.len() as u64;
        self.vad.accept(samples);
        let speech = self.vad.is_speech();
        let ended = self.was_speech && !speech;
        self.was_speech = speech;
        if speech {
            self.heard_speech = true;
        }
        if !self.heard_speech {
            self.drop_leading_silence();
            return Ok(Vec::new());
        }
        if ended {
            return self.end_phrase();
        }
        if self.buf.len() - self.decoded_len >= (self.cfg.rerun_interval * SR) as usize {
            return self.recheck();
        }
        Ok(Vec::new())
    }

    /// Stops the session: transcribes what's left and commits everything.
    pub fn finish(&mut self) -> Result<Vec<Word>, AsrError> {
        let out = if self.heard_speech {
            self.end_phrase()?
        } else {
            Vec::new()
        };
        Ok(out)
    }

    fn recheck(&mut self) -> Result<Vec<Word>, AsrError> {
        let words = self.transcribe_buffer()?;
        let now = self.now();
        self.decoded_len = self.buf.len();
        let committed = match &mut self.commit {
            Commit::Timed(c) => c.insert(words, now, self.cfg.min_age),
            Commit::Seq(c) => c.insert(words),
        };
        self.note_commits(&committed, now);
        let limit = match self.commit {
            Commit::Timed(_) => self.cfg.hard_max_buffer,
            // No timestamps → no mid-buffer trims; end the phrase instead.
            Commit::Seq(_) => self.cfg.max_buffer,
        };
        if self.buf_secs() > limit {
            // Nothing agrees for a long time (or no way to trim): flush this stretch.
            let mut rest = self.end_phrase()?;
            let mut all = committed;
            all.append(&mut rest);
            return Ok(all);
        }
        if matches!(self.commit, Commit::Timed(_)) {
            self.trim_after(&committed);
        }
        Ok(committed)
    }

    fn end_phrase(&mut self) -> Result<Vec<Word>, AsrError> {
        let words = self.transcribe_buffer()?;
        let committed = match &mut self.commit {
            Commit::Timed(c) => {
                let out = c.flush(words);
                c.clear_pending();
                out
            }
            Commit::Seq(c) => {
                let out = c.flush(words);
                c.new_buffer();
                out
            }
        };
        let now = self.now();
        self.note_commits(&committed, now);
        self.buf_start += self.buf.len() as f64 / SR;
        self.buf.clear();
        self.decoded_len = 0;
        self.heard_speech = false;
        Ok(committed)
    }

    fn transcribe_buffer(&mut self) -> Result<Vec<Word>, AsrError> {
        if self.buf.is_empty() {
            return Ok(Vec::new());
        }
        self.stats.decodes += 1;
        let mut words = self.transcriber.transcribe(&self.buf)?;
        for w in &mut words {
            w.start += self.buf_start;
            w.end += self.buf_start;
        }
        Ok(words)
    }

    /// Trims the buffer at the last committed sentence end, or at the commit
    /// point when the buffer is getting long, so each run stays short.
    fn trim_after(&mut self, committed: &[Word]) {
        let cut = committed
            .iter()
            .rev()
            .find(|w| ends_sentence(&w.text))
            .map(|w| w.end)
            .or_else(|| {
                let Commit::Timed(c) = &self.commit else {
                    return None;
                };
                (self.buf_secs() > self.cfg.max_buffer).then(|| c.committed_end())
            });
        if let Some(t) = cut {
            self.cut_at(t);
        }
    }

    fn cut_at(&mut self, t: f64) {
        let n = (((t - self.buf_start) * SR).round().max(0.0) as usize).min(self.buf.len());
        if n == 0 {
            return;
        }
        self.buf.drain(..n);
        self.buf_start += n as f64 / SR;
        self.decoded_len = self.decoded_len.saturating_sub(n);
    }

    fn drop_leading_silence(&mut self) {
        let keep = (self.cfg.preroll * SR) as usize;
        if self.buf.len() > keep {
            let n = self.buf.len() - keep;
            self.buf.drain(..n);
            self.buf_start += n as f64 / SR;
        }
        self.decoded_len = 0;
    }

    fn buf_secs(&self) -> f64 {
        self.buf.len() as f64 / SR
    }

    fn note_commits(&mut self, committed: &[Word], now: f64) {
        let timed = matches!(self.commit, Commit::Timed(_));
        for w in committed {
            if !timed {
                self.stats.committed_words += 1;
                continue;
            }
            let lag = (now - w.end).max(0.0);
            self.stats.committed_words += 1;
            self.stats.total_commit_lag += lag;
            self.stats.max_commit_lag = self.stats.max_commit_lag.max(lag);
        }
    }
}

impl<T, V> super::Engine for RecheckEngine<T, V>
where
    T: Transcriber + Send,
    V: SpeechDetector + Send,
{
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError> {
        RecheckEngine::push(self, samples)
    }
    fn finish(&mut self) -> Result<Vec<Word>, AsrError> {
        RecheckEngine::finish(self)
    }
    fn reset(&mut self) {
        RecheckEngine::reset(self);
    }
    fn stats(&self) -> EngineStats {
        self.stats.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake model: a fixed script of (word, start, end) in absolute time. It
    /// "hears" a word once the buffer covers its end, and optionally mishears
    /// the newest word (as real models do at the buffer edge).
    struct Script {
        words: Vec<(&'static str, f64, f64)>,
        buf_start: std::rc::Rc<std::cell::Cell<f64>>,
        edge_garble: bool,
    }

    impl Transcriber for Script {
        fn transcribe(&mut self, samples: &[f32]) -> Result<Vec<Word>, AsrError> {
            let start = self.buf_start.get();
            let end = start + samples.len() as f64 / SR;
            let mut out: Vec<Word> = self
                .words
                .iter()
                .filter(|(_, s, e)| *s >= start - 0.01 && *e <= end)
                .map(|(t, s, e)| Word {
                    text: (*t).to_owned(),
                    start: s - start,
                    end: e - start,
                })
                .collect();
            if self.edge_garble
                && let Some(last) = out.last_mut()
                && end - (last.end + start) < 0.3
            {
                last.text = format!("{}?!", last.text);
            }
            Ok(out)
        }
    }

    /// VAD driven by a list of speech intervals in absolute time.
    struct Intervals {
        speech: Vec<(f64, f64)>,
        t: f64,
        hangover: f64,
    }

    impl SpeechDetector for Intervals {
        fn accept(&mut self, samples: &[f32]) {
            self.t += samples.len() as f64 / SR;
        }
        fn is_speech(&self) -> bool {
            self.speech
                .iter()
                .any(|(s, e)| self.t >= *s && self.t < *e + self.hangover)
        }
        fn reset(&mut self) {
            self.t = 0.0;
        }
    }

    /// Feeds `secs` of audio in 20 ms chunks; keeps the fake model's view of
    /// buf_start in sync. Returns committed words with the audio time they appeared.
    fn run(
        engine: &mut RecheckEngine<Script, Intervals>,
        buf_start: &std::rc::Rc<std::cell::Cell<f64>>,
        secs: f64,
    ) -> Vec<(String, f64)> {
        let chunk = vec![0.0f32; 320];
        let mut out = Vec::new();
        for _ in 0..(secs / 0.02) as usize {
            buf_start.set(engine.buf_start);
            // buf_start can move inside push (trim); the fake reads it at transcribe time
            let words = engine.push(&chunk).expect("fake never fails");
            for w in words {
                out.push((w.text, engine.now()));
            }
        }
        buf_start.set(engine.buf_start);
        for w in engine.finish().expect("fake never fails") {
            out.push((w.text, engine.now()));
        }
        out
    }

    fn engine(
        words: Vec<(&'static str, f64, f64)>,
        speech: Vec<(f64, f64)>,
        garble: bool,
    ) -> (
        RecheckEngine<Script, Intervals>,
        std::rc::Rc<std::cell::Cell<f64>>,
    ) {
        let bs = std::rc::Rc::new(std::cell::Cell::new(0.0));
        let e = RecheckEngine::new(
            EngineConfig::default(),
            Script {
                words,
                buf_start: bs.clone(),
                edge_garble: garble,
            },
            Intervals {
                speech,
                t: 0.0,
                hangover: 0.8,
            },
        );
        (e, bs)
    }

    #[test]
    fn types_every_word_once_in_order() {
        let script = vec![
            ("Hello", 1.0, 1.4),
            ("there,", 1.5, 1.9),
            ("how", 2.0, 2.3),
            ("are", 2.4, 2.6),
            ("you?", 2.7, 3.1),
        ];
        let (mut e, bs) = engine(script, vec![(1.0, 3.1)], true);
        let out = run(&mut e, &bs, 5.0);
        let texts: Vec<_> = out.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["Hello", "there,", "how", "are", "you?"]);
    }

    #[test]
    fn words_appear_while_speaking_not_only_at_the_end() {
        let script: Vec<_> = (0..10)
            .map(|i| {
                let s = 1.0 + i as f64 * 0.5;
                (
                    ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j."][i],
                    s,
                    s + 0.4,
                )
            })
            .collect();
        let (mut e, bs) = engine(script, vec![(1.0, 6.0)], false);
        let out = run(&mut e, &bs, 8.0);
        assert_eq!(out.len(), 10);
        // "a" ends at 1.4 s; it must be typed long before speech ends at 6 s
        let (_, t_a) = &out[0];
        assert!(*t_a < 3.0, "first word committed at {t_a}");
        // lag stays around a second
        assert!(e.stats().max_commit_lag < 1.6, "{:?}", e.stats());
    }

    #[test]
    fn pause_flushes_the_phrase_with_final_word() {
        let script = vec![("One", 1.0, 1.3), ("two.", 1.4, 1.8), ("Three.", 4.0, 4.4)];
        let (mut e, bs) = engine(script, vec![(1.0, 1.8), (4.0, 4.4)], true);
        let out = run(&mut e, &bs, 6.0);
        let texts: Vec<_> = out.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["One", "two.", "Three."]);
        // "two." is typed at the pause (1.8 + 0.8 hangover), not at the end
        assert!(out[1].1 < 3.0, "{:?}", out);
    }

    #[test]
    fn silence_only_types_nothing_and_keeps_buffer_small() {
        let (mut e, bs) = engine(vec![], vec![], false);
        let out = run(&mut e, &bs, 10.0);
        assert!(out.is_empty());
        assert!(e.buf.len() <= (0.5 * SR) as usize);
        assert_eq!(e.stats().decodes, 0);
    }

    #[test]
    fn buffer_is_trimmed_at_sentence_ends() {
        let mut script = Vec::new();
        let names = ["First.", "Second.", "Third.", "Fourth.", "Fifth."];
        for (i, n) in names.iter().enumerate() {
            let s = 1.0 + i as f64 * 2.0;
            script.push((*n, s, s + 1.0));
        }
        let (mut e, bs) = engine(script, vec![(1.0, 11.0)], false);
        let chunk = vec![0.0f32; 320];
        let mut max_buf = 0usize;
        for _ in 0..(10.5 / 0.02) as usize {
            bs.set(e.buf_start);
            e.push(&chunk).expect("fake");
            max_buf = max_buf.max(e.buf.len());
        }
        // without trimming the buffer would reach ~10 s
        assert!(max_buf < (5.0 * SR) as usize, "max buffer {max_buf}");
    }

    #[test]
    fn stop_mid_sentence_commits_everything() {
        let script = vec![("Stop", 1.0, 1.3), ("right", 1.4, 1.7), ("here", 1.8, 2.0)];
        let (mut e, bs) = engine(script, vec![(1.0, 10.0)], false);
        let out = run(&mut e, &bs, 2.1);
        let texts: Vec<_> = out.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["Stop", "right", "here"]);
    }
}
