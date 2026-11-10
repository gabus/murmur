//! "Local agreement" word committer (as in whisper_streaming).
//!
//! The re-check engine transcribes the growing buffer every ~0.5 s. A word is
//! committed (typed) once two consecutive hypotheses agree on it, word for word,
//! including punctuation and case, and it is at least `min_age` old. Committed
//! words are final: murmur never deletes typed text.
//!
//! Finding where the *uncommitted* part of a new hypothesis starts is the
//! delicate bit. Model timestamps wobble by a few 80 ms frames between runs,
//! and committed words may come back with different case or punctuation. So
//! we first locate the committed tail (last few words, normalised) inside the
//! hypothesis, near the commit time. Only if that fails do we fall back to time.

use super::words::Word;

/// How many committed words are remembered for locating the commit point.
const TAIL: usize = 5;
/// A tail match must lie within this distance of the last committed word (s).
const MATCH_WINDOW: f64 = 0.6;

#[derive(Debug, Default)]
pub struct Committer {
    /// End time of the last committed word (absolute seconds).
    committed_end: f64,
    /// Most recent committed words, oldest first.
    recent: Vec<Word>,
    /// Uncommitted tail of the previous hypothesis.
    pending: Vec<Word>,
}

/// Lowercase alphanumerics only: "So," and "so" compare equal.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
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
    /// Committed history is kept to locate the commit point.
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// The words of `hyp` that come after what has already been committed.
    fn uncommitted_part(&self, hyp: Vec<Word>) -> Vec<Word> {
        if self.recent.is_empty() {
            return hyp;
        }
        let hn: Vec<String> = hyp.iter().map(|w| norm(&w.text)).collect();
        let rn: Vec<String> = self.recent.iter().map(|w| norm(&w.text)).collect();
        // Distance of a hypothesis word to the last committed word. Either
        // boundary may move a lot when the model merges or splits words
        // ("leaves" + "tomorrow?" → "leaves?"), so take the closer one.
        let Some(last) = self.recent.last() else {
            return hyp;
        };
        let dist = |w: &Word| (w.start - last.start).abs().min((w.end - last.end).abs());
        // Longest committed tail found in the hypothesis close to the commit time.
        for n in (1..=rn.len().min(hyp.len())).rev() {
            let tail = &rn[rn.len() - n..];
            if tail.iter().all(String::is_empty) {
                continue;
            }
            let best = (0..=hyp.len() - n)
                .filter(|&j| hn[j..j + n] == *tail)
                .map(|j| (j, dist(&hyp[j + n - 1])))
                .filter(|(_, dist)| *dist <= MATCH_WINDOW)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((j, _)) = best {
                return hyp[j + n..].to_vec();
            }
        }
        // The committed words aren't in this hypothesis (buffer trimmed, or the
        // model changed them): keep words whose centre is after the commit point.
        hyp.into_iter()
            .filter(|w| (w.start + w.end) / 2.0 > self.committed_end)
            .collect()
    }

    fn record(&mut self, committed: &[Word]) {
        if let Some(last) = committed.last() {
            self.committed_end = self.committed_end.max(last.end);
        }
        self.recent.extend_from_slice(committed);
        if self.recent.len() > TAIL {
            let excess = self.recent.len() - TAIL;
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

    /// Words with explicit (text, start, end).
    fn timed(v: &[(&str, f64, f64)]) -> Vec<Word> {
        v.iter()
            .map(|(t, s, e)| Word {
                text: (*t).to_owned(),
                start: *s,
                end: *e,
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
    fn re_emitted_committed_words_with_shifted_times_are_not_typed_again() {
        let mut c = Committer::new();
        c.insert(hyp(0.0, "the cat sat"), 10.0, 0.0);
        c.insert(hyp(0.0, "the cat sat"), 10.0, 0.0); // commits all three (end 1.4)
        let shifted = timed(&[("cat", 1.35, 1.5), ("sat", 1.55, 1.7), ("down", 1.8, 2.0)]);
        c.insert(shifted.clone(), 10.0, 0.0);
        let out = c.insert(shifted, 10.0, 0.0);
        assert_eq!(texts(&out), ["down"]);
    }

    #[test]
    fn genuinely_repeated_words_are_kept() {
        // "I think that that is fine": the second "that" is real speech.
        let mut c = Committer::new();
        let h1 = timed(&[
            ("I", 0.0, 0.2),
            ("think", 0.3, 0.6),
            ("that", 0.7, 0.9),
            ("that", 1.0, 1.2),
        ]);
        c.insert(h1.clone(), 10.0, 0.0);
        let mut h2 = h1.clone();
        h2.push(Word {
            text: "is".into(),
            start: 1.3,
            end: 1.5,
        });
        let out = c.insert(h2, 10.0, 0.0);
        assert_eq!(texts(&out), ["I", "think", "that", "that"]);

        // committed only up to the first "that" (agreement stopped there)
        let mut c = Committer::new();
        c.insert(
            timed(&[
                ("I", 0.0, 0.2),
                ("think", 0.3, 0.6),
                ("that", 0.7, 0.9),
                ("is", 1.0, 1.2),
            ]),
            10.0,
            0.0,
        );
        let out = c.insert(
            timed(&[
                ("I", 0.0, 0.2),
                ("think", 0.3, 0.6),
                ("that", 0.7, 0.9),
                ("that", 1.0, 1.2),
            ]),
            10.0,
            0.0,
        );
        assert_eq!(texts(&out), ["I", "think", "that"]);
        let full = timed(&[
            ("I", 0.0, 0.2),
            ("think", 0.3, 0.6),
            ("that", 0.7, 0.9),
            ("that", 1.0, 1.2),
            ("is", 1.3, 1.5),
            ("fine.", 1.6, 1.9),
        ]);
        let mid = c.insert(full.clone(), 10.0, 0.0);
        let out = c.flush(full);
        let mut all: Vec<&str> = vec!["I", "think", "that"];
        all.extend(texts(&mid));
        all.extend(texts(&out));
        assert_eq!(all, ["I", "think", "that", "that", "is", "fine."]);
    }

    #[test]
    fn very_very_with_wobbling_timestamps() {
        // Real dump: "very" committed with end 2.96; the next run shifted it.
        let mut c = Committer::new();
        let h1 = timed(&[("see", 2.0, 2.3), ("you", 2.4, 2.6), ("very", 2.7, 2.96)]);
        c.insert(h1.clone(), 10.0, 0.0);
        let out = c.insert(h1, 10.0, 0.0);
        assert_eq!(texts(&out), ["see", "you", "very"]);
        let h = timed(&[
            ("see", 2.0, 2.3),
            ("you", 2.4, 2.6),
            ("very", 2.80, 3.12),
            ("very", 3.20, 3.52),
            ("soon.", 3.6, 3.9),
        ]);
        c.insert(h.clone(), 10.0, 0.0);
        let out = c.flush(h);
        assert_eq!(texts(&out), ["very", "soon."]);
    }

    #[test]
    fn committed_word_changing_punctuation_is_not_retyped() {
        // "long" committed; next run says "long," with shifted times.
        let mut c = Committer::new();
        let h1 = timed(&[("a", 4.5, 4.7), ("long", 4.96, 5.12)]);
        c.insert(h1.clone(), 10.0, 0.0);
        c.insert(h1, 10.0, 0.0);
        let h = timed(&[
            ("a", 4.5, 4.7),
            ("long,", 5.28, 5.60),
            ("long", 5.7, 6.0),
            ("day.", 6.1, 6.4),
        ]);
        let out = c.flush(h);
        assert_eq!(texts(&out), ["long", "day."]);
    }

    #[test]
    fn next_word_starting_slightly_early_is_not_lost() {
        let mut c = Committer::new();
        let h1 = timed(&[
            ("Hello", 0.0, 0.4),
            ("there,", 0.5, 0.9),
            ("how", 0.95, 1.2),
        ]);
        c.insert(h1.clone(), 10.0, 0.0);
        c.insert(
            timed(&[("Hello", 0.0, 0.4), ("there,", 0.5, 0.9)]),
            10.0,
            0.0,
        );
        // "how" now starts two frames before the committed end (0.9)
        let h = timed(&[
            ("Hello", 0.0, 0.4),
            ("there,", 0.5, 0.9),
            ("how", 0.74, 1.0),
            ("are", 1.1, 1.3),
            ("you?", 1.4, 1.7),
        ]);
        let out = c.flush(h);
        assert_eq!(texts(&out), ["how", "are", "you?"]);
    }

    #[test]
    fn committed_word_absorbing_the_next_one_is_not_retyped() {
        // Real dump: "leaves" committed (3.52-4.16); later runs merged
        // "leaves tomorrow?" into one "leaves?" spanning 3.60-5.28.
        let mut c = Committer::new();
        let h1 = timed(&[
            ("train", 3.12, 3.52),
            ("leaves", 3.52, 4.16),
            ("tomorrow?", 4.16, 5.0),
        ]);
        c.insert(h1.clone(), 10.0, 0.0);
        let out = c.insert(h1, 10.0, 0.0);
        assert_eq!(
            texts(&out),
            ["train", "leaves", "tomorrow?"][..3]
                .iter()
                .copied()
                .take(3)
                .collect::<Vec<_>>()
        );
        let mut c = Committer::new();
        let h1 = timed(&[
            ("train", 3.12, 3.52),
            ("leaves", 3.52, 4.16),
            ("tomorrow?", 4.16, 5.0),
        ]);
        c.insert(h1.clone(), 4.5, 0.0);
        let out = c.insert(h1, 4.5, 0.0);
        assert_eq!(texts(&out), ["train", "leaves"]);
        let merged = timed(&[
            ("train", 3.12, 3.60),
            ("leaves?", 3.60, 5.28),
            ("I", 5.28, 5.50),
            ("think", 5.5, 5.8),
        ]);
        let out = c.flush(merged);
        assert_eq!(texts(&out), ["I", "think"]);
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

    #[test]
    fn new_phrase_starting_with_last_committed_word_is_typed() {
        // Previous phrase ended with "no." at 2.0; after a pause the user says "No way."
        let mut c = Committer::new();
        let h = timed(&[("Oh", 1.0, 1.4), ("no.", 1.6, 2.0)]);
        c.flush(h);
        c.clear_pending();
        let out = c.flush(timed(&[("No", 3.5, 3.8), ("way.", 3.9, 4.2)]));
        assert_eq!(texts(&out), ["No", "way."]);
    }
}
