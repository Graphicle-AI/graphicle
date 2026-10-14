// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/benchmark.py, graphify/serve.py
//! Query tokenization shared by the native benchmark and serving pipeline.
use crate::{dedup_strings::whitespace, regex_compat};
use regex::Regex;
use std::{collections::BTreeSet, sync::LazyLock};

static STOPWORDS: LazyLock<BTreeSet<String>> =
    LazyLock::new(|| serde_json::from_str(include_str!("assets/query/stopwords.json")).unwrap());
static WORDS: LazyLock<Regex> = LazyLock::new(|| regex_compat::compile(r"\w+"));
pub fn has_chinese(text: &str) -> bool {
    text.chars().any(|c| ('一'..='鿿').contains(&c))
}
pub fn is_searchable(term: &str) -> bool {
    !term.chars().all(|c| c.is_ascii_lowercase()) || term.chars().count() > 2
}
pub fn segment_chinese_bigrams(text: &str) -> Vec<String> {
    let chars: Vec<_> = text.chars().collect();
    let mut segments: Vec<String> = chars.windows(2).map(|w| w.iter().collect()).collect();
    if segments.is_empty() {
        segments.push(text.into());
    }
    if chars.len() > 1 && !segments.iter().any(|s| s == text) {
        segments.push(text.into());
    }
    segments
}
pub fn segment_chinese(text: &str) -> Vec<String> {
    #[cfg(feature = "chinese")]
    {
        static JIEBA: LazyLock<jieba_rs::Jieba> = LazyLock::new(|| {
            let mut segmenter = jieba_rs::Jieba::new();
            let mut data =
                std::io::Cursor::new(include_bytes!("assets/query/jieba-py-0.46.12.hmm"));
            segmenter
                .set_hmm_model(jieba_rs::HmmModel::load(&mut data).expect("pinned HMM parameters"));
            segmenter
        });
        static HMM_SKIP: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"[a-zA-Z0-9]+(?:\.\d+)?%?").unwrap());
        let mut result = Vec::new();
        for token in JIEBA.cut(text, true) {
            let word = token.word;
            if word.trim_matches(whitespace).is_empty() {
                continue;
            }
            // jieba-rs 0.11 groups more identifier connectors than the pinned
            // Python finalseg. Restore that model's ASCII token boundaries.
            if word.is_ascii() && !JIEBA.has_word(word) {
                let mut end = 0;
                for m in HMM_SKIP.find_iter(word) {
                    if m.start() > end {
                        result.push(word[end..m.start()].into());
                    }
                    result.push(m.as_str().into());
                    end = m.end();
                }
                if end < word.len() {
                    result.push(word[end..].into());
                }
            } else {
                result.push(word.into());
            }
        }
        if text.chars().count() > 1 && !result.iter().any(|s| s == text) {
            result.push(text.into());
        }
        result
    }
    #[cfg(not(feature = "chinese"))]
    segment_chinese_bigrams(text)
}
pub fn query_terms(question: &str) -> Vec<String> {
    query_terms_with(question, true)
}
pub fn query_terms_with(question: &str, dictionary: bool) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in question.split(whitespace).filter(|s| !s.is_empty()) {
        if has_chinese(raw) {
            let raw = raw.to_lowercase();
            let raw = raw.trim_matches(whitespace);
            let segments = if dictionary {
                segment_chinese(raw)
            } else {
                segment_chinese_bigrams(raw)
            };
            for word in segments {
                let word = word.trim_matches(whitespace);
                if !word.is_empty() && is_searchable(word) {
                    terms.push(word.into());
                }
            }
        } else {
            terms.extend(
                WORDS
                    .find_iter(&raw.to_lowercase())
                    .map(|m| m.as_str().to_owned())
                    .filter(|w| is_searchable(w)),
            );
        }
    }
    let content: Vec<_> = terms
        .iter()
        .filter(|w| !STOPWORDS.contains(w.as_str()))
        .cloned()
        .collect();
    if content.is_empty() { terms } else { content }
}
