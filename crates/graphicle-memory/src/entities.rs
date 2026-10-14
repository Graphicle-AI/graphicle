//! Specific terms a question names: proper names, quoted phrases, technical
//! identifiers and quantities, and whether any source mentions them at all.
//!
//! Candidate rules adapted from mem0's rule-based entity candidates
//! (mem0ai/mem0@94c3fe9f, `mem0/utils/entity_extraction.py`:
//! `_add_quoted_candidates`, `_add_technical_identifier_candidates`,
//! `_add_proper_name_candidates`, `_resolve_candidates`; Apache-2.0). Upstream
//! reads spaCy part-of-speech tags; here capitalization and word lists stand
//! in for them, and quantities are added because a changed amount ("30-gallon"
//! for a 20-gallon tank) is the commonest false premise in conversational
//! memory questions. Nothing is inferred or stored: a term either occurs in a
//! source or it does not.
use crate::{Result, string};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Most terms reported per question; the rest are dropped in question order.
pub const MAX_TERMS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// `proper`, `quoted`, `identifier` or `quantity`.
    pub kind: &'static str,
    /// The term as written in the question.
    pub text: String,
    tokens: Vec<String>,
}

impl Term {
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }
}

/// Function words, question words and imperative openers that are capitalized
/// only because they start a sentence, plus role words mem0 rejects as names.
const COMMON: &[&str] = &[
    "a",
    "about",
    "after",
    "again",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "been",
    "before",
    "being",
    "both",
    "but",
    "by",
    "can",
    "could",
    "did",
    "do",
    "does",
    "during",
    "each",
    "either",
    "even",
    "ever",
    "every",
    "few",
    "for",
    "from",
    "give",
    "had",
    "has",
    "have",
    "having",
    "he",
    "help",
    "her",
    "here",
    "hers",
    "him",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "just",
    "last",
    "let",
    "list",
    "many",
    "may",
    "me",
    "might",
    "mine",
    "more",
    "most",
    "much",
    "must",
    "my",
    "name",
    "neither",
    "next",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "on",
    "once",
    "one",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "out",
    "over",
    "please",
    "recommend",
    "remind",
    "said",
    "same",
    "say",
    "she",
    "should",
    "show",
    "since",
    "so",
    "some",
    "suggest",
    "summarize",
    "tell",
    "than",
    "thanks",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "though",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "us",
    "was",
    "we",
    "were",
    "what",
    "whatever",
    "when",
    "where",
    "whether",
    "which",
    "while",
    "who",
    "whom",
    "whose",
    "why",
    "will",
    "with",
    "would",
    "yes",
    "yet",
    "you",
    "your",
    "yours",
    "hi",
    "hello",
    "hey",
    "ok",
    "okay",
    // mem0 `_GENERIC_SINGLE_ENTITY_TERMS` and `_GENERIC_CAPS`
    "user",
    "assistant",
    "agent",
    "customer",
    "client",
    "person",
    "people",
    "human",
    "memory",
    "message",
    "conversation",
    "chat",
    "session",
    "system",
    "top",
    "works",
    "items",
    "things",
    "stuff",
    "resources",
    "options",
    "tips",
    "ideas",
    "steps",
    "ways",
    "methods",
    "tools",
    "features",
    "benefits",
    "examples",
    "details",
    "notes",
    "instructions",
    "guidelines",
    "recommendations",
    "suggestions",
    "overview",
    "summary",
    "conclusion",
    "introduction",
    "pros",
    "cons",
    "advantages",
    "disadvantages",
];

/// Calendar words are time expressions, not names; sources often write dates
/// numerically, so their absence proves nothing.
const CALENDAR: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "jan",
    "feb",
    "mar",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "sept",
    "oct",
    "nov",
    "dec",
    "today",
    "yesterday",
    "tomorrow",
    "weekend",
    "day",
    "days",
    "week",
    "weeks",
    "month",
    "months",
    "year",
    "years",
    "eve",
    "christmas",
    "easter",
    "valentine",
    "valentine's",
    "valentine’s",
    "halloween",
    "thanksgiving",
    "hanukkah",
    "diwali",
    "ramadan",
    "eid",
];

/// mem0 lets these join name tokens inside one span ("Bank of America"). Its
/// "at" and "in" are left out: in questions they mostly join two separate
/// names ("the Airbnb in Sacramento"), which sources rarely write verbatim.
const CONNECTORS: &[&str] = &["of", "the", "for"];

/// A period after these does not end a sentence ("Dr. Johnson").
const ABBREVIATIONS: &[&str] = &[
    "dr", "mr", "mrs", "ms", "mx", "prof", "st", "mt", "jr", "sr", "vs", "etc", "no", "inc", "ltd",
    "co", "corp", "dept", "univ", "ave", "rd", "blvd",
];

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid pattern"))
}

/// Normalized word tokens used for matching: lowercase alphanumeric runs,
/// with possessive or contraction `s` dropped and a plural `s` stripped, so
/// "30-gallon", "30 gallons" and "Sarah's" meet their plain forms.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Apostrophes inside words go first, so "Hawai'i" and "Sarah's" stay whole.
    let text: String = text
        .chars()
        .filter(|c| !matches!(c, '\'' | '’' | 'ʻ' | '‘'))
        .collect();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        // An acronym's plural ("EPs", "TVs") meets its singular.
        let acronym = word.len() > 2
            && word.ends_with('s')
            && word[..word.len() - 1]
                .chars()
                .all(|c| c.is_uppercase() || c.is_ascii_digit());
        let mut word = word.to_lowercase();
        if acronym || word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
            word.pop();
        }
        out.push(word);
    }
    out
}

/// Whether `words` mention `term`: a single word also matches with a demonym
/// or adjective ending ("Hawaii" in "Hawaiian"), and a longer term matches
/// with up to `gap - 1` words between its parts ("Samsung TV" in "Samsung
/// 55-inch TV"; quantities use a gap of 1, so their parts stay adjacent).
fn mentions(words: &[String], term: &[String], gap: usize) -> bool {
    const ENDINGS: &[&str] = &["n", "an", "ian", "ese", "ish", "er", "i"];
    match term {
        [] => false,
        [only] => words.iter().any(|w| {
            w == only
                || only.chars().count() >= 5
                    && w.strip_prefix(only.as_str())
                        .is_some_and(|rest| ENDINGS.contains(&rest))
        }),
        [first, rest @ ..] => words.iter().enumerate().any(|(i, w)| {
            if w != first {
                return false;
            }
            let mut at = i;
            rest.iter().all(|part| {
                let window = &words[at + 1..(at + 1 + gap).min(words.len())];
                match window.iter().position(|w| w == part) {
                    Some(offset) => {
                        at += offset + 1;
                        true
                    }
                    None => false,
                }
            })
        }),
    }
}

/// Most words, plus one, allowed between consecutive parts of a name.
const GAP: usize = 4;

struct Candidate {
    kind: &'static str,
    text: String,
    span: Option<(usize, usize)>,
    priority: u8,
}

fn common(word: &str) -> bool {
    let lower = word.to_lowercase();
    COMMON.contains(&lower.as_str()) || CALENDAR.contains(&lower.as_str())
}

/// A capitalized token that is not a common word; at a sentence start it also
/// needs an inner capital or digit, which stands in for mem0's proper-noun tag.
fn name_like(word: &str, sentence_start: bool) -> bool {
    let Some(first) = word.chars().next() else {
        return false;
    };
    if !first.is_uppercase() || !word.chars().any(char::is_alphabetic) || common(word) {
        return false;
    }
    !sentence_start
        || word
            .chars()
            .skip(1)
            .any(|c| c.is_uppercase() || c.is_ascii_digit())
}

/// Specific terms named by `text`, in question order, at most `MAX_TERMS`.
pub fn terms(text: &str) -> Vec<Term> {
    terms_up_to(text, MAX_TERMS)
}

/// [`terms`] with another cap, for reading passages rather than questions.
pub fn terms_up_to(text: &str, limit: usize) -> Vec<Term> {
    static QUOTED_DOUBLE: OnceLock<Regex> = OnceLock::new();
    static QUOTED_SINGLE: OnceLock<Regex> = OnceLock::new();
    static IDENTIFIER: OnceLock<Regex> = OnceLock::new();
    static QUANTITY: OnceLock<Regex> = OnceLock::new();
    static WORD: OnceLock<Regex> = OnceLock::new();
    let mut candidates = Vec::new();
    for pattern in [
        regex(&QUOTED_DOUBLE, r#"["“]([^"“”]+)["”]"#),
        regex(
            &QUOTED_SINGLE,
            r"(?:^|[\s(\[{,;])['‘]([^'‘’]+)['’](?:[\s.,;:!?)\]]|$)",
        ),
    ] {
        for found in pattern.captures_iter(text) {
            let inner = found.get(1).unwrap();
            if inner.as_str().trim().chars().count() > 2 {
                candidates.push(Candidate {
                    kind: "quoted",
                    text: inner.as_str().trim().to_string(),
                    // Upstream quoted candidates carry no span: they never
                    // displace the names inside them.
                    span: None,
                    priority: 3,
                });
            }
        }
    }
    let identifier = regex(&IDENTIFIER, r"\b[A-Za-z_][\w-]*(?:\.[A-Za-z_][\w-]*)+\b");
    for found in identifier.find_iter(text) {
        candidates.push(Candidate {
            kind: "identifier",
            text: found.as_str().to_string(),
            span: Some((found.start(), found.end())),
            priority: 1,
        });
    }
    // Amounts with a unit joined by a hyphen, currency, percentages and years.
    let quantity = regex(
        &QUANTITY,
        r"(?:[$£€¥]\s?\d[\d,]*(?:\.\d+)?)|(?:\b\d[\d,]*(?:\.\d+)?\s?%)|(?:\b\d[\d,]*(?:\.\d+)?-[A-Za-z]+\b)|(?:\b(?:1[89]|20)\d{2}\b)",
    );
    for found in quantity.find_iter(text) {
        candidates.push(Candidate {
            kind: "quantity",
            text: found.as_str().to_string(),
            span: Some((found.start(), found.end())),
            priority: 1,
        });
    }
    // Proper-name spans: consecutive name-like words, allowing an inner
    // connector between two of them.
    let words: Vec<_> = regex(&WORD, r"[\p{L}\p{N}][\p{L}\p{N}'’&-]*")
        .find_iter(text)
        .collect();
    let starts_sentence = |i: usize| {
        let before = text[..words[i].start()].trim_end();
        if let Some(head) = before.strip_suffix('.') {
            // Initials and titles keep the sentence going.
            let last = head
                .rsplit(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or("");
            if last.chars().count() == 1 || ABBREVIATIONS.contains(&last.to_lowercase().as_str()) {
                return false;
            }
        }
        before.is_empty() || before.ends_with(['.', '!', '?', ':', '"', '“', '(', '-', '*', '#'])
    };
    let mut i = 0;
    while i < words.len() {
        if !name_like(words[i].as_str(), starts_sentence(i)) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < words.len() {
            let between = &text[words[j - 1].end()..words[j].start()];
            if !between.chars().all(|c| c == ' ') {
                break;
            }
            if name_like(words[j].as_str(), false) {
                j += 1;
            } else if CONNECTORS.contains(&words[j].as_str().to_lowercase().as_str())
                && j + 1 < words.len()
                && text[words[j].end()..words[j + 1].start()]
                    .chars()
                    .all(|c| c == ' ')
                && name_like(words[j + 1].as_str(), false)
            {
                j += 2;
            } else {
                break;
            }
        }
        let (start, end) = (words[i].start(), words[j - 1].end());
        let span = text[start..end].trim_end_matches(['\'', '’', '-']);
        let span = span
            .strip_suffix("'s")
            .or_else(|| span.strip_suffix("’s"))
            .unwrap_or(span);
        if span.chars().count() > 2 {
            candidates.push(Candidate {
                kind: "proper",
                text: span.to_string(),
                span: Some((start, start + span.len())),
                priority: 2,
            });
        }
        i = j;
    }
    resolve(candidates, limit)
}

/// mem0's `_resolve_candidates`: one candidate per normalized text, stronger
/// sources first, and no two overlapping spans; then question order.
fn resolve(candidates: Vec<Candidate>, limit: usize) -> Vec<Term> {
    let mut ordered: Vec<(usize, Candidate)> = candidates.into_iter().enumerate().collect();
    ordered.sort_by_key(|(i, c)| {
        let width = c.span.map_or(0, |(a, b)| b - a);
        (c.priority, std::cmp::Reverse(width), *i)
    });
    let mut seen = BTreeSet::new();
    let mut accepted: Vec<(usize, Candidate)> = Vec::new();
    for (index, candidate) in ordered {
        let key = tokens(&candidate.text);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        let overlaps = |other: &Candidate| match (candidate.span, other.span) {
            (Some((a, b)), Some((x, y))) => a < y && x < b,
            _ => false,
        };
        if accepted.iter().any(|(_, other)| overlaps(other)) {
            continue;
        }
        accepted.push((index, candidate));
    }
    accepted.sort_by_key(|(index, c)| (c.span.map_or(usize::MAX, |(a, _)| a), *index));
    accepted
        .into_iter()
        .take(limit)
        .map(|(_, c)| Term {
            kind: c.kind,
            tokens: tokens(&c.text),
            text: c.text,
        })
        .collect()
}

/// Terms that none of the `sources` mention (see `mentions`).
pub fn absent<'t, 'a>(
    terms: &'t [Term],
    sources: impl IntoIterator<Item = &'a str>,
) -> Vec<&'t Term> {
    let mut missing: Vec<&Term> = terms.iter().filter(|t| !t.tokens.is_empty()).collect();
    for source in sources {
        if missing.is_empty() {
            break;
        }
        let words = tokens(source);
        missing.retain(|term| {
            let gap = if term.kind == "quantity" { 1 } else { GAP };
            !mentions(&words, &term.tokens, gap)
        });
    }
    missing
}

fn listed<'a>(items: impl IntoIterator<Item = &'a Term>) -> Vec<Value> {
    items
        .into_iter()
        .map(|t| json!({"kind":t.kind,"text":t.text}))
        .collect()
}

/// `entities.terms`: `{"text"}` → the terms; with `"sources"` also the terms
/// no source mentions.
pub fn operation(request: &Value) -> Result<Value> {
    let text = string(request, "text")?;
    if text.len() > 64 * 1024 {
        return Err("Question text is too long".into());
    }
    let found = terms(text);
    let mut result = json!({"terms":listed(&found)});
    if let Some(sources) = request.get("sources") {
        let sources = sources.as_array().ok_or("Expected source texts")?;
        if sources.len() > 100_000 {
            return Err("Source limit exceeded".into());
        }
        let mut texts = Vec::with_capacity(sources.len());
        for source in sources {
            texts.push(source.as_str().ok_or("Expected source text")?);
        }
        result["absent"] = json!(listed(absent(&found, texts)));
    }
    Ok(result)
}
