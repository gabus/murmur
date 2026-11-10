//! "Local agreement" word committer (as in whisper_streaming).
//!
//! The re-check engine transcribes the growing buffer every ~0.5 s. A word is
//! committed (typed) once two consecutive hypotheses agree on it, word for word,
//! including punctuation and case, and it is at least `min_age` old. Committed
//! words are final: murmur never deletes typed text.

use super::words::Word;

/// Tolerance when matching new hypothesis words against the committed end time.
const TIME_SLACK: f64 = 0.1;
/// Max n-gram length checked when removing words repeated at the commit boundary.
const MAX_OVERLAP: usize = 5;

#[derive(Debug, Default)]
pub struct Committer {
    /// End time of the last committed word (absolute seconds).
    committed_end: f64,
    /// Most recent committed words (for overlap removal), newest last.
    recent: Vec<String>,
    /// Uncommitted tail of the previous hypothesis.
    pending: Vec<Word>,
}

impl Committer {
    pub fn new() -> Self {
        Self::default()
    }

    /// End time of the last committed word.
    pub fn committed_end(&self) -> f64 {
        self.committed_end
    }

    /// Feeds a new full hypothesis of the current buffer. Returns words that
    /// became final. `now` is the audio time at the end of the buffer.
    /// Words newer than `now - min_age` are never committed yet.
    pub fn insert(&mut self, hypothesis: Vec<Word>, now: f64, min_age: f64) -> Vec<Word> {
        let new = self.uncommitted_part(hypothesis);
        let agreed = self
            .pending
            .iter()
            .zip(&new)
            .take_while(|(a, b)| a.text == b.text && b.end <= now - min_age)
            .count();
        let committed: Vec<Word> = new[..agreed].to_vec();
        self.pending = new[agreed..].to_vec();
        self.record(&committed);
        committed
    }

    /// Commits everything new in a final hypothesis (end of phrase or stop).
    pub fn flush(&mut self, hypothesis: Vec<Word>) -> Vec<Word> {
        let committed = self.uncommitted_part(hypothesis);
        self.pending.clear();
        self.record(&committed);
        committed
    }

    /// Forgets the pending hypothesis (e.g. after a phrase was flushed).
    /// Committed history is kept for overlap removal.
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Words of `hyp` after the committed point, with a repeated boundary n-gram removed.
    fn uncommitted_part(&self, hyp: Vec<Word>) -> Vec<Word> {
        let mut new: Vec<Word> = hyp
            .into_iter()
            .filter(|w| w.start > self.committed_end - TIME_SLACK)
            .collect();
        // The model may re-emit the last committed word(s) when timestamps wobble.
        if let Some(first) = new.first()
            && (first.start - self.committed_end).abs() < 1.0
        {
            let max_n = MAX_OVERLAP.min(self.recent.len()).min(new.len());
            for n in (1..=max_n).rev() {
                let tail = &self.recent[self.recent.len() - n..];
                if new[..n].iter().map(|w| &w.text).eq(tail.iter()) {
                    new.drain(..n);
                    break;
                }
            }
        }
        new
    }

    fn record(&mut self, committed: &[Word]) {
        if let Some(last) = committed.last() {
            self.committed_end = self.committed_end.max(last.end);
        }
        self.recent.extend(committed.iter().map(|w| w.text.clone()));
        if self.recent.len() > MAX_OVERLAP {
            let excess = self.recent.len() - MAX_OVERLAP;
            self.recent.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words spaced 0.5 s apart starting at `t0`.
    fn hyp(t0: f64, text: &str) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| Word {
                text: w.to_owned(),
                start: t0 + i as f64 * 0.5,
                end: t0 + i as f64 * 0.5 + 0.4,
            })
            .collect()
    }

    fn texts(w: &[Word]) -> Vec<&str> {
        w.iter().map(|w| w.text.as_str()).collect()
    }

    #[test]
    fn commits_only_after_two_runs_agree() {
        let mut c = Committer::new();
        assert!(c.insert(hyp(0.0, "Ask not what"), 10.0, 0.0).is_empty());
        let out = c.insert(hyp(0.0, "Ask not what your"), 10.0, 0.0);
        assert_eq!(texts(&out), ["Ask", "not", "what"]);
        let out = c.insert(hyp(0.0, "Ask not what your country"), 10.0, 0.0);
        assert_eq!(texts(&out), ["your"]);
    }

    #[test]
    fn disagreement_stops_the_prefix() {
        let mut c = Committer::new();
        c.insert(hyp(0.0, "I scream is"), 10.0, 0.0);
        // model revises "I scream" -> "Ice cream": nothing agrees at position 0
        assert!(c.insert(hyp(0.0, "Ice cream is"), 10.0, 0.0).is_empty());
        let out = c.insert(hyp(0.0, "Ice cream is great"), 10.0, 0.0);
        assert_eq!(texts(&out), ["Ice", "cream", "is"]);
    }

    #[test]
    fn punctuation_change_blocks_commit() {
        let mut c = Committer::new();
        c.insert(hyp(0.0, "for you. Ask"), 10.0, 0.0);
        let out = c.insert(hyp(0.0, "for you, ask what"), 10.0, 0.0);
        assert_eq!(texts(&out), ["for"]);
    }

    #[test]
    fn min_age_holds_back_recent_words() {
        let mut c = Committer::new();
        c.insert(hyp(0.0, "one two three four"), 2.0, 0.0);
        // words end at 0.4, 0.9, 1.4, 1.9; with now=2.0 and min_age=1.0 only <=1.0 qualify
        let out = c.insert(hyp(0.0, "one two three four"), 2.0, 1.0);
        assert_eq!(texts(&out), ["one", "two"]);
        let out = c.insert(hyp(0.0, "one two three four"), 3.0, 1.0);
        assert_eq!(texts(&out), ["three", "four"]);
    }

    #[test]
    fn flush_commits_remaining_and_skips_committed() {
        let mut c = Committer::new();
        c.insert(hyp(0.0, "hello big"), 10.0, 0.0);
        c.insert(hyp(0.0, "hello big world"), 10.0, 0.0);
        let out = c.flush(hyp(0.0, "hello big world."));
        assert_eq!(texts(&out), ["world."]);
        assert!(c.flush(hyp(0.0, "hello big world.")).is_empty());
    }

    #[test]
    fn removes_repeated_boundary_words() {
        let mut c = Committer::new();
        c.insert(hyp(0.0, "the cat sat"), 10.0, 0.0);
        c.insert(hyp(0.0, "the cat sat"), 10.0, 0.0); // commits all three (end 1.4)
        // timestamps shifted: "sat" re-appears starting after committed_end
        let shifted = vec![
            Word {
                text: "cat".into(),
                start: 1.35,
                end: 1.5,
            },
            Word {
                text: "sat".into(),
                start: 1.55,
                end: 1.7,
            },
            Word {
                text: "down".into(),
                start: 1.8,
                end: 2.0,
            },
        ];
        c.insert(shifted.clone(), 10.0, 0.0);
        let out = c.insert(shifted, 10.0, 0.0);
        assert_eq!(texts(&out), ["down"]);
    }

    #[test]
    fn trimmed_buffer_with_absolute_times_keeps_working() {
        let mut c = Committer::new();
        c.insert(hyp(0.0, "First sentence."), 10.0, 0.0);
        let out = c.insert(hyp(0.0, "First sentence. Second"), 10.0, 0.0);
        assert_eq!(texts(&out), ["First", "sentence."]);
        // engine trims audio at 0.9 s: new hypotheses only cover later audio,
        // and "Second" (pending from the previous run) is confirmed
        let out = c.insert(hyp(1.0, "Second one"), 10.0, 0.0);
        assert_eq!(texts(&out), ["Second"]);
        let out = c.insert(hyp(1.0, "Second one here"), 10.0, 0.0);
        assert_eq!(texts(&out), ["one"]);
    }
}
