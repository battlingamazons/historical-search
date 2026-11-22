//! Turning a typed phrase into the numbers CLIP's text model reads.
//!
//! CLIP splits words into pieces with "byte pair encoding": text is lowered,
//! cut into runs of letters, single digits and runs of punctuation, and each
//! run is built up by joining the pairs of pieces that the model's list of
//! merges ranks highest. The list and the numbering of the pieces (49,408 of
//! them) are OpenAI's, shipped here as two small text files.
//!
//! The answers are checked against the Python tokenizer the model was
//! published with (`tests/fixtures/clip/tokens.json`).

use std::collections::HashMap;
use std::sync::OnceLock;

/// Every phrase is exactly this many numbers long: the model reads a fixed window.
pub const CONTEXT: usize = 77;
const START: i64 = 49406;
const END: i64 = 49407;

const VOCAB_JSON: &str = include_str!("../clip/vocab.json");
const MERGES: &str = include_str!("../clip/merges.txt");

struct Tables {
    /// Piece (as CLIP spells it, bytes mapped to printable characters) -> number.
    vocab: HashMap<String, i64>,
    /// Pair of pieces -> how early the model merges it (lower is earlier).
    ranks: HashMap<(String, String), usize>,
    /// Byte -> the printable character CLIP uses for it.
    byte_char: [char; 256],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let vocab: HashMap<String, i64> = serde_json::from_str(VOCAB_JSON).unwrap_or_default();
        let ranks = MERGES
            .lines()
            .skip(1) // "#version" line
            .filter_map(|l| l.split_once(' '))
            .enumerate()
            .map(|(i, (a, b))| ((a.to_string(), b.to_string()), i))
            .collect();
        Tables { vocab, ranks, byte_char: byte_chars() }
    })
}

/// OpenAI's byte -> character table: printable bytes stand for themselves and
/// the rest are moved up past 255, so no piece contains a space or a control
/// character.
fn byte_chars() -> [char; 256] {
    let mut table = ['\0'; 256];
    let mut extra = 0u32;
    for b in 0..=255u32 {
        let printable = (33..=126).contains(&b) || (161..=172).contains(&b) || (174..=255).contains(&b);
        table[b as usize] = if printable {
            char::from_u32(b).unwrap_or('\0')
        } else {
            let c = char::from_u32(256 + extra).unwrap_or('\0');
            extra += 1;
            c
        };
    }
    table
}

/// Cuts text into the runs CLIP's pattern describes: an apostrophe ending
/// ('s 't 're 've 'm 'll 'd), a run of letters, a single digit, or a run of
/// anything else that is not a space.
fn runs(text: &str) -> Vec<String> {
    const ENDINGS: [&str; 7] = ["'s", "'t", "'re", "'ve", "'m", "'ll", "'d"];
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '\'' {
            let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
            if let Some(e) = ENDINGS.iter().find(|e| rest.starts_with(**e)) {
                out.push((*e).to_string());
                i += e.chars().count();
                continue;
            }
        }
        let start = i;
        if c.is_alphabetic() {
            while i < chars.len() && chars[i].is_alphabetic() {
                i += 1;
            }
        } else if c.is_numeric() {
            i += 1;
        } else {
            // Anything else, up to a space, a letter or a digit. (The pattern
            // does not stop at an apostrophe, so neither does this.)
            while i < chars.len() && !chars[i].is_whitespace() && !chars[i].is_alphanumeric() {
                i += 1;
            }
        }
        out.push(chars[start..i].iter().collect());
    }
    out
}

/// One run built up into pieces by the model's merges.
fn bpe(run: &str, t: &Tables) -> Vec<String> {
    let mut word: Vec<String> = run.bytes().map(|b| t.byte_char[b as usize].to_string()).collect();
    if let Some(last) = word.last_mut() {
        last.push_str("</w>"); // marks the end of a word
    }
    while word.len() > 1 {
        let best = word
            .windows(2)
            .enumerate()
            .filter_map(|(i, p)| t.ranks.get(&(p[0].clone(), p[1].clone())).map(|r| (*r, i)))
            .min();
        let Some((_, at)) = best else { break };
        let joined = format!("{}{}", word[at], word[at + 1]);
        word[at] = joined;
        word.remove(at + 1);
    }
    word
}

/// The numbers for a phrase: start, the pieces, end, then end again to fill
/// the window (as the published tokenizer pads). Phrases too long are cut and
/// closed with the end marker.
pub fn tokenize(text: &str) -> [i64; CONTEXT] {
    let t = tables();
    // The model was trained on lower-case text with spaces collapsed.
    let lowered = text.to_lowercase();
    let mut ids: Vec<i64> = vec![START];
    for run in runs(&lowered) {
        for piece in bpe(&run, t) {
            if let Some(id) = t.vocab.get(&piece) {
                ids.push(*id);
            }
        }
    }
    ids.truncate(CONTEXT - 1);
    ids.push(END);
    let mut out = [END; CONTEXT];
    out[..ids.len()].copy_from_slice(&ids);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_tokenizer_on_every_phrase() {
        let reference: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/clip/tokens.json")).unwrap();
        let cases = reference.as_array().unwrap();
        assert!(cases.len() >= 15);
        for case in cases {
            let text = case["text"].as_str().unwrap();
            let want: Vec<i64> = case["ids"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
            assert_eq!(tokenize(text).to_vec(), want, "phrase {text:?}");
        }
    }

    #[test]
    fn a_phrase_always_fills_the_window_and_ends_properly() {
        let ids = tokenize("a very small photograph");
        assert_eq!(ids.len(), CONTEXT);
        assert_eq!(ids[0], START);
        assert_eq!(ids[CONTEXT - 1], END);
    }
}
