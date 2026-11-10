//! Turning model tokens into timed words.

/// A recognised word with absolute times in seconds since listening started.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    /// Word text including attached punctuation (e.g. `"you,"`).
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// Fallback token length when the model gives no durations (Parakeet frame = 80 ms).
const DEFAULT_TOKEN_SECS: f64 = 0.08;

/// Groups sentencepiece-style tokens into words. A token that starts with a
/// space (or `▁`) begins a new word. Other tokens, including punctuation,
/// attach to the current word. `offset` is added to every timestamp.
pub fn tokens_to_words(
    tokens: &[String],
    timestamps: &[f32],
    durations: &[f32],
    offset: f64,
) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    // A bare space token (sentencepiece `▁` on its own, e.g. before digits or
    // symbols the vocabulary has no `▁x` piece for) starts the *next* word.
    let mut boundary = false;
    for (i, tok) in tokens.iter().enumerate() {
        let start = timestamps.get(i).map_or(offset, |t| offset + f64::from(*t));
        let end = match durations.get(i) {
            Some(d) if *d > 0.0 => start + f64::from(*d),
            _ => timestamps
                .get(i + 1)
                .map_or(start + DEFAULT_TOKEN_SECS, |t| offset + f64::from(*t)),
        };
        let (new_word, piece) = match tok.strip_prefix(' ').or_else(|| tok.strip_prefix('▁')) {
            Some(rest) => (true, rest),
            None => (false, tok.as_str()),
        };
        if piece.is_empty() {
            boundary |= new_word;
            continue;
        }
        let new_word = new_word || std::mem::take(&mut boundary);
        match words.last_mut() {
            Some(w) if !new_word => {
                w.text.push_str(piece);
                w.end = end.max(w.end);
            }
            _ => words.push(Word {
                text: piece.to_owned(),
                start,
                end,
            }),
        }
    }
    words
}

/// True if the word ends a sentence (`.`, `?`, `!`, `…`, optionally followed by
/// closing quotes or brackets).
pub fn ends_sentence(word: &str) -> bool {
    let trimmed = word.trim_end_matches(['"', '\'', ')', ']', '»', '”', '’']);
    trimmed.ends_with(['.', '?', '!', '…'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn groups_subwords_and_punctuation() {
        let t = toks(&[" A", "sk", " not", ",", " co", "un", "try", "."]);
        let ts = [0.0, 0.24, 0.40, 0.64, 0.80, 0.88, 0.96, 1.20];
        let du = [0.24, 0.16, 0.24, 0.08, 0.08, 0.08, 0.24, 0.08];
        let w = tokens_to_words(&t, &ts, &du, 10.0);
        let texts: Vec<_> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Ask", "not,", "country."]);
        assert!((w[0].start - 10.0).abs() < 1e-6);
        assert!((w[0].end - 10.40).abs() < 1e-6);
        assert!((w[2].start - 10.80).abs() < 1e-6);
        assert!((w[2].end - 11.28).abs() < 1e-6);
    }

    #[test]
    fn first_token_without_space_starts_a_word() {
        let w = tokens_to_words(&toks(&["Hi", " there"]), &[0.0, 0.3], &[], 0.0);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].text, "Hi");
        // no durations: end = next token start
        assert!((w[0].end - 0.3).abs() < 1e-6);
        assert!((w[1].end - (0.3 + DEFAULT_TOKEN_SECS)).abs() < 1e-6);
    }

    #[test]
    fn sentencepiece_marker_and_empty_tokens() {
        let w = tokens_to_words(
            &toks(&["▁Labas", " ", "▁rytas"]),
            &[0.0, 0.1, 0.2],
            &[],
            0.0,
        );
        let texts: Vec<_> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Labas", "rytas"]);
    }

    #[test]
    fn bare_space_token_starts_the_next_word() {
        // Parakeet v3 has no "▁1"…"▁9" pieces: " 1030" comes as [" ", "1", "0", "3", "0"].
        let t = toks(&[
            " at", " ", "1", "0", "3", "0", " on", " Ma", "y", " ", "4", "th", ".",
        ]);
        let ts: Vec<f32> = (0..t.len()).map(|i| i as f32 * 0.1).collect();
        let w = tokens_to_words(&t, &ts, &[], 0.0);
        let texts: Vec<_> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["at", "1030", "on", "May", "4th."]);
        // the number starts at its first digit, not at the space token
        assert!((w[1].start - 0.2).abs() < 1e-6);
    }

    #[test]
    fn sentence_end_detection() {
        assert!(ends_sentence("country."));
        assert!(ends_sentence("why?"));
        assert!(ends_sentence("stop!\""));
        assert!(ends_sentence("(see this.)"));
        assert!(!ends_sentence("you,"));
        assert!(!ends_sentence("Mr"));
    }
}
