// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/blade.py
//! Character classes matching Python 3.12's Unicode-15 regular expressions.
use crate::unicode15::WORD_RANGES;
use regex::Regex;
use std::sync::LazyLock;

static WORD: LazyLock<String> = LazyLock::new(|| {
    let mut class = String::from("[");
    for (start, end) in WORD_RANGES {
        class.push_str(&format!(r"\u{{{start:x}}}-\u{{{end:x}}}"));
    }
    class.push(']');
    class
});

/// For static ported patterns, substitute Python's word/space classes.
/// This is not a general Python-regex translator (e.g. backreferences).
pub fn compile(pattern: &str) -> Regex {
    Regex::new(
        &pattern
            .replace(r"\w", &WORD)
            .replace(r"\s", crate::fsutil::PY_WHITESPACE),
    )
    .expect("verified static extractor pattern")
}
