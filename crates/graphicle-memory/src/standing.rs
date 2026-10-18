//! Standing instructions and stated preferences in the user's own messages.
//!
//! "Always include version numbers when I ask about libraries" governs every
//! later answer, yet a later question rarely shares its words, so retrieval by
//! similarity to the question misses it. These statements are found lexically,
//! without a model call, in turns a conversation labels as the user's, and are
//! returned as exact byte spans so a reader can be shown the user's own words.
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Instruction,
    Preference,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Instruction => "instruction",
            Kind::Preference => "preference",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub kind: Kind,
    pub text: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// A speaker label opening a line, optionally after a bracketed tag such as
/// `[March-15-2024 | Turn 0]`.
static TURN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*(?:\[[^\]\n]{0,80}\][ \t]*)?([A-Za-z][A-Za-z .'_-]{0,40}?)[ \t]*:[ \t]")
        .unwrap()
});
static INSTRUCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:please[ ,]+)?(?:always|never|from now on|make sure (?:to|you)|whenever i ask|when i ask|remember to|do not ever|don't ever)\b").unwrap()
});
static PREFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bi(?: really| strongly| much)? prefer\b|\bi'd (?:rather|prefer)\b|\bi would (?:rather|prefer)\b|\bmy preference is\b|\bi (?:like|love|want) to keep\b").unwrap()
});
/// Trailing machine markers some transcripts append to a turn (`->-> 1,25`).
static MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*->->\s*\d+,\d+\s*$").unwrap());

fn user_label(label: &str) -> bool {
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "user" | "human" | "me"
    )
}

/// Byte ranges of the user's turns: from after each user label to the next label.
fn user_turns(text: &str) -> Vec<(usize, usize)> {
    let labels: Vec<(usize, usize, bool)> = TURN
        .captures_iter(text)
        .map(|c| {
            let whole = c.get(0).unwrap();
            (whole.start(), whole.end(), user_label(&c[1]))
        })
        .collect();
    labels
        .iter()
        .enumerate()
        .filter(|(_, (_, _, user))| *user)
        .map(|(i, (_, body, _))| (*body, labels.get(i + 1).map_or(text.len(), |next| next.0)))
        .collect()
}

/// Sentences of `text[start..end]` as byte ranges. A sentence ends at `.`, `!`
/// or `?` followed by whitespace, or at a line break, so "Flask 2.1" stays whole.
fn sentences(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut from = start;
    let mut i = start;
    while i < end {
        let b = bytes[i];
        let terminal =
            matches!(b, b'.' | b'!' | b'?') && (i + 1 == end || bytes[i + 1].is_ascii_whitespace());
        if terminal || b == b'\n' {
            let stop = if terminal { i + 1 } else { i };
            out.push((from, stop));
            from = i + 1;
        }
        i += 1;
    }
    if from < end {
        out.push((from, end));
    }
    out.into_iter()
        .filter_map(|(a, b)| {
            let slice = &text[a..b];
            let lead = slice.len() - slice.trim_start().len();
            let trail = slice.len() - slice.trim_end().len();
            (a + lead < b - trail).then_some((a + lead, b - trail))
        })
        .collect()
}

/// Standing instructions and preferences in the user's turns of one
/// conversation document, in order. Sentences shorter than four words or
/// longer than 400 bytes are skipped.
pub fn extract(text: &str) -> Vec<Statement> {
    let mut found = Vec::new();
    for (start, end) in user_turns(text) {
        let turn = &text[start..end];
        let end = start + MARKER.find(turn).map_or(turn.len(), |m| m.start());
        for (a, b) in sentences(text, start, end) {
            let sentence = &text[a..b];
            if sentence.len() > 400 || sentence.split_whitespace().count() < 4 {
                continue;
            }
            let kind = if INSTRUCTION.is_match(sentence) {
                Kind::Instruction
            } else if PREFERENCE.is_match(sentence) {
                Kind::Preference
            } else {
                continue;
            };
            found.push(Statement {
                kind,
                text: sentence.to_owned(),
                byte_start: a,
                byte_end: b,
            });
        }
    }
    found
}

/// The opening sentence of each user turn, as exact byte ranges in order: what
/// the user asked for, turn by turn. An outline of these answers "in what order
/// did I bring up …" without a model call. Openings shorter than four words
/// are skipped; one longer than 300 bytes ends at the last word that fits.
pub fn openings(text: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (start, end) in user_turns(text) {
        let turn = &text[start..end];
        let end = start + MARKER.find(turn).map_or(turn.len(), |m| m.start());
        let Some(&(a, b)) = sentences(text, start, end).first() else {
            continue;
        };
        if text[a..b].split_whitespace().count() < 4 {
            continue;
        }
        let mut b = b;
        if b - a > 300 {
            // The first 300 bytes, back to a character boundary: byte 300
            // can fall inside a multi-byte character.
            let mut cut = a + 300;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            b = text[a..cut].rfind(' ').map_or(cut, |space| a + space);
        }
        found.push((a, b));
    }
    found
}

/// A yes/no question about the user's own past: "Have I …?", "Did we …?".
static OWN_PAST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:have|has|did|do|does|am|was|were|had)\s+(?:i|we)\b").unwrap()
});
/// A statement that the user never did something: "I've never …".
static DENIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:i(?:'ve| have)? never|i have not ever|i've not ever|i haven't ever|never have i)\b").unwrap()
});
/// A sentence in the speaker's own voice.
static FIRST_PERSON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:i|i'm|i've|i'd|i'll|my|we|we've|our)\b").unwrap());
/// A line of code pasted into a turn, not a statement about what the user did.
/// Prose verbs such as "return" or "import" alone do not make a sentence code.
static CODE_LIKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[{}=;<>]|\(\)|->|::|^\s*(?:from\s+\S+\s+)?import\s|\bdef\s+\w+\s*\(").unwrap()
});
/// Words too common to tie two statements to one subject.
const FUNCTION_WORDS: &[&str] = &[
    "and", "the", "for", "with", "from", "are", "was", "were", "been", "you", "this", "that",
    "what", "which", "who", "how", "does", "did", "can", "could", "would", "should", "will",
    "about", "into", "than", "then", "but", "not", "have", "has", "had", "any", "never", "ever",
    "also", "just",
];

/// Porter stems of a text's content words: three or more characters, not a
/// function word.
fn content(text: &str) -> BTreeSet<String> {
    crate::text::words(text)
        .into_iter()
        .filter(|w| w.chars().count() >= 3 && !FUNCTION_WORDS.contains(&w.as_str()))
        .map(|w| crate::stem::stem(&w))
        .collect()
}

/// One sentence of the user's own words in one of several documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Said {
    pub doc: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    /// The sentence says the user never did what the question asks about.
    pub denial: bool,
}

/// For a yes/no question about the user's own past ("Have I worked with Flask
/// routes?"): the user's "I've never X" sentence that best matches the
/// question, then up to `partners` of their other sentences about X, so a
/// reader sees a record that says both. Partners are the user's own prose
/// (first person, not pasted code) sharing at least two content stems with X,
/// best per stem first; everything is returned in document then reading
/// order. Empty for any other question, or when no denial matches it in at
/// least two content stems. `documents` are conversation texts in episode order.
pub fn conflicts(documents: &[&str], question: &str, partners: usize) -> Vec<Said> {
    if !OWN_PAST.is_match(question) {
        return Vec::new();
    }
    let mut said: Vec<(usize, usize, usize)> = Vec::new();
    for (doc, text) in documents.iter().enumerate() {
        for (start, end) in user_turns(text) {
            let turn = &text[start..end];
            let end = start + MARKER.find(turn).map_or(turn.len(), |m| m.start());
            for (a, b) in sentences(text, start, end) {
                let words = text[a..b].split_whitespace().count();
                if (5..=80).contains(&words) {
                    said.push((doc, a, b));
                }
            }
        }
    }
    let want = content(question);
    let mut best: Option<(f64, usize, BTreeSet<String>)> = None;
    for (i, &(doc, a, b)) in said.iter().enumerate() {
        let sentence = &documents[doc][a..b];
        let Some(m) = DENIAL.find(sentence) else {
            continue;
        };
        let claim = content(&sentence[m.end()..]);
        let hits = claim.intersection(&want).count();
        let score = hits as f64 / (claim.len().max(1) as f64).sqrt();
        if hits >= 2 && best.as_ref().is_none_or(|(top, _, _)| score > *top) {
            best = Some((score, i, claim));
        }
    }
    let Some((_, denial, claim)) = best else {
        return Vec::new();
    };
    let mut scored: Vec<(f64, usize)> = said
        .iter()
        .enumerate()
        .filter(|&(j, _)| j != denial)
        .filter_map(|(j, &(doc, a, b))| {
            let sentence = &documents[doc][a..b];
            if DENIAL.is_match(sentence)
                || CODE_LIKE.is_match(sentence)
                || !FIRST_PERSON.is_match(sentence)
            {
                return None;
            }
            let bag = content(sentence);
            let shared = claim.intersection(&bag).count();
            let asked = bag.intersection(&want).count();
            (shared >= 2).then(|| {
                (
                    (shared as f64 + 0.5 * asked as f64) / (bag.len() as f64).sqrt(),
                    j,
                )
            })
        })
        .collect();
    scored.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
    let mut keep: Vec<usize> = std::iter::once(denial)
        .chain(scored.into_iter().take(partners).map(|(_, j)| j))
        .collect();
    if keep.len() == 1 {
        return Vec::new();
    }
    keep.sort_unstable();
    keep.into_iter()
        .map(|i| {
            let (doc, byte_start, byte_end) = said[i];
            Said {
                doc,
                byte_start,
                byte_end,
                denial: i == denial,
            }
        })
        .collect()
}

/// The `standing.conflicts` operation: `{"documents": [text, …], "question",
/// "partners"?}` (at most 1,000 documents; partners 0–10, default 3).
pub fn conflicts_operation(request: &Value) -> crate::Result<Value> {
    let question = request
        .get("question")
        .and_then(Value::as_str)
        .ok_or("Expected question")?;
    let documents: Vec<&str> = request
        .get("documents")
        .and_then(Value::as_array)
        .filter(|d| d.len() <= 1000)
        .ok_or("Expected at most 1,000 documents")?
        .iter()
        .map(|d| d.as_str().ok_or("Documents must be strings"))
        .collect::<std::result::Result<_, _>>()?;
    let partners = match request.get("partners") {
        None | Some(Value::Null) => 3,
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= 10)
            .ok_or("partners must be 0 to 10")? as usize,
    };
    let items: Vec<Value> = conflicts(&documents, question, partners)
        .into_iter()
        .map(|s| {
            json!({"doc":s.doc,"byte_start":s.byte_start,"byte_end":s.byte_end,"denial":s.denial,
            "text":&documents[s.doc][s.byte_start..s.byte_end]})
        })
        .collect();
    Ok(json!({"items":items}))
}

/// The comparison key that makes repeated statements one: lowercase words.
pub fn key(statement: &Statement) -> String {
    statement
        .text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_and_preferences_come_only_from_user_turns() {
        let text = "Conversation date: 2024/03/15 (Fri) 00:00\n\n\
            [March-15-2024 | Turn 0] User: Always include version numbers when I ask about libraries. Thanks!\n\n\
            [Turn 1] Assistant: Always happy to help. I prefer short answers myself.\n\n\
            user: I'm keeping the app small, so I prefer minimal dependencies on Flask 2.1 and SQLite. ->-> 1,25\n";
        let found = extract(text);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].kind, Kind::Instruction);
        assert_eq!(
            found[0].text,
            "Always include version numbers when I ask about libraries."
        );
        assert_eq!(&text[found[0].byte_start..found[0].byte_end], found[0].text);
        assert_eq!(found[1].kind, Kind::Preference);
        assert_eq!(
            found[1].text,
            "I'm keeping the app small, so I prefer minimal dependencies on Flask 2.1 and SQLite."
        );
    }

    #[test]
    fn named_speakers_and_short_fragments_are_not_statements() {
        let text = "Caroline: I always prefer hiking in spring.\nuser: Never mind.\nuser: please never use tabs in my Python code\n";
        let found = extract(text);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].text, "please never use tabs in my Python code");
        assert_eq!(found[0].kind, Kind::Instruction);
    }

    #[test]
    fn openings_are_each_user_turns_first_sentence() {
        let text = "[Turn 0] User: I'm trying to set up a Flask 2.3 app with Bootstrap. It should be fast.\n\
            [Turn 1] Assistant: Sure, here is how.\n\
            [Turn 2] User: Thanks!\n\
            [Turn 3] User: hmm, how do I test the update function properly?\n";
        let spans: Vec<&str> = openings(text)
            .into_iter()
            .map(|(a, b)| &text[a..b])
            .collect();
        assert_eq!(
            spans,
            [
                "I'm trying to set up a Flask 2.3 app with Bootstrap.",
                "hmm, how do I test the update function properly?"
            ]
        );
    }

    #[test]
    fn a_long_opening_ends_on_a_character_boundary() {
        // Byte 300 of the opening falls inside an "é" (two bytes).
        let text = format!("User: I need help with {}\n", "\u{e9}".repeat(200));
        let spans = openings(&text);
        assert_eq!(spans.len(), 1, "{spans:?}");
        let (a, b) = spans[0];
        assert_eq!(&text[a..b], "I need help with");
        // Without a space in the first 300 bytes the cut is at the last
        // whole character.
        let text = format!("User: x{} b c d\n", "\u{4f60}".repeat(120));
        let spans = openings(&text);
        assert_eq!(spans.len(), 1, "{spans:?}");
        let (a, b) = spans[0];
        assert_eq!(text[a..b], format!("x{}", "\u{4f60}".repeat(99)));
    }

    #[test]
    fn conflicts_pair_a_denial_with_the_users_own_statements_on_its_subject() {
        let early = "Conversation date: 2024/03/10 (Sun) 00:00\n\n\
            [Turn 24] User: I'm trying to implement the basic homepage route with Flask, and I've managed to return static HTML.\n\
            [Turn 25] Assistant: I've never seen such a fast Flask route, nice work handling HTTP requests.\n";
        let late = "Conversation date: 2024/03/20 (Wed) 00:00\n\n\
            [Turn 58] User: I've never written any Flask routes or handled HTTP requests in this project, so I'm starting from scratch.\n\
            [Turn 60] User: from flask import Flask, request, jsonify route handlers\n\
            [Turn 62] User: My weekend plans involve hiking with friends near the lake.\n";
        let question = "Have I worked with Flask routes and handled HTTP requests in this project?";
        let found = conflicts(&[early, late], question, 3);
        let texts: Vec<&str> = found
            .iter()
            .map(|s| &[early, late][s.doc][s.byte_start..s.byte_end])
            .collect();
        assert_eq!(
            texts,
            [
                "I'm trying to implement the basic homepage route with Flask, and I've managed to return static HTML.",
                "I've never written any Flask routes or handled HTTP requests in this project, so I'm starting from scratch."
            ]
        );
        assert_eq!(
            found.iter().map(|s| s.denial).collect::<Vec<_>>(),
            [false, true]
        );
        // Only yes/no questions about the user's own past, and only with a matching denial.
        assert!(conflicts(&[early, late], "What framework did I use for routes?", 3).is_empty());
        assert!(conflicts(&[early], question, 3).is_empty());
    }

    #[test]
    fn repeated_statements_share_a_key() {
        let a = Statement {
            kind: Kind::Instruction,
            text: "Always use type hints.".into(),
            byte_start: 0,
            byte_end: 0,
        };
        let b = Statement {
            kind: Kind::Instruction,
            text: "always use  type-hints".into(),
            byte_start: 9,
            byte_end: 9,
        };
        assert_eq!(key(&a), key(&b));
    }
}
