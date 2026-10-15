// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/dedup.py
//! Label comparisons for entity deduplication, using the pinned Unicode tables.
use crate::{
    decimal_data::DECIMAL,
    minhash::MinHash,
    python_compat,
    unicode15::{CASEFOLD, MARK_RANGES, WORD_RANGES},
};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    sync::LazyLock,
};
use unicode_normalization::UnicodeNormalization;

pub fn casefold(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        match CASEFOLD.binary_search_by_key(&c, |e| e.0) {
            Ok(i) => out.push_str(CASEFOLD[i].1),
            Err(_) => out.push(c),
        }
    }
    out
}
fn in_ranges(ranges: &[(u32, u32)], c: char) -> bool {
    let i = ranges.partition_point(|&(lo, _)| lo <= c as u32);
    i > 0 && c as u32 <= ranges[i - 1].1
}
/// `_is_label_char` (0.9.86, #4317): letters and digits (`str.isalnum`), plus
/// combining marks (category M*), which `\w` does not match: without them
/// every vowel sign and virama of an Indic word was a separator.
pub fn is_label_char(c: char) -> bool {
    (c != '_' && in_ranges(WORD_RANGES, c)) || in_ranges(MARK_RANGES, c)
}
/// `_norm`: NFKC, casefold, and every run of non-label characters (see
/// [`is_label_char`]) collapsed to one space, none at either end.
pub fn norm(value: &Value) -> String {
    let value = if value.is_null() {
        String::new()
    } else {
        python_compat::string(value)
    };
    let value = casefold(&value.nfkc().collect::<String>());
    let mut out = String::new();
    let mut pending = false;
    for c in value.chars() {
        if is_label_char(c) {
            if pending {
                out.push(' ');
                pending = false;
            }
            out.push(c);
        } else {
            pending = !out.is_empty();
        }
    }
    out
}
pub fn entropy(value: &Value) -> f64 {
    let s = norm(value);
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = Vec::<usize>::new();
    let mut indices = HashMap::new();
    let mut len = 0;
    for c in s.chars() {
        let next = counts.len();
        let i = *indices.entry(c).or_insert_with(|| {
            counts.push(0);
            next
        });
        counts[i] += 1;
        len += 1;
    }
    let (mut sum, mut correction) = (0.0_f64, 0.0_f64);
    for count in counts {
        let p = count as f64 / len as f64;
        let v = p * p.log2();
        let next = sum + v;
        correction += if sum.abs() >= v.abs() {
            (sum - next) + v
        } else {
            (v - next) + sum
        };
        sum = next;
    }
    -(sum + correction)
}
pub fn shingles(text: &str, k: i64) -> BTreeSet<String> {
    let c: Vec<_> = text.chars().collect();
    let n = c.len() as i128;
    if n < k as i128 {
        return BTreeSet::from([text.to_owned()]);
    }
    let mut out = BTreeSet::new();
    // Callers use positive trigram lengths. Handle zero/negative Python slice
    // indices too, without signed arithmetic overflow.
    for i in 0..(n - k as i128 + 1) {
        let end = i + k as i128;
        let end = if end < 0 {
            (n + end).max(0)
        } else {
            end.min(n)
        } as usize;
        let start = i.min(n) as usize;
        out.insert(if end > start {
            c[start..end].iter().collect()
        } else {
            String::new()
        });
    }
    out
}
pub fn make_minhash(text: &str, num_perm: usize) -> MinHash {
    let mut sketch = MinHash::new(num_perm);
    for shingle in shingles(&text.replace(' ', ""), 3) {
        sketch.update(shingle.as_bytes());
    }
    sketch
}
static VARIANT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*[a-z])([0-9]+[a-z]*|[a-z]{2,})(?:\n)?\z").unwrap());
pub fn is_variant_pair(a: &str, b: &str) -> bool {
    if a == b || a.chars().count().max(b.chars().count()) >= 12 {
        return false;
    }
    match (VARIANT.captures(a), VARIANT.captures(b)) {
        (Some(a), Some(b)) => a[1] == b[1] && a[2] != b[2],
        _ => false,
    }
}
fn same_length_one_edit(a: &[char], b: &[char]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut first = None;
    let mut second = None;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x == y {
            continue;
        }
        if first.is_none() {
            first = Some(i);
        } else if second.is_none() {
            second = Some(i);
        } else {
            return false;
        }
    }
    match (first, second) {
        (_, None) => true,
        (Some(i), Some(j)) => j == i + 1 && a[i] == b[j] && a[j] == b[i],
        _ => false,
    }
}
pub fn short_label_blocked(a: &str, b: &str, score: f64) -> bool {
    let a: Vec<_> = a.chars().collect();
    let b: Vec<_> = b.chars().collect();
    a.len().max(b.len()) < 12 && !(score >= 97.0 && same_length_one_edit(&a, &b))
}
pub fn decimal(c: char) -> bool {
    let i = DECIMAL.partition_point(|&(lo, _)| lo <= c as u32);
    i > 0 && c as u32 <= DECIMAL[i - 1].1
}
fn digit_runs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in text.chars().chain(std::iter::once('\0')) {
        if decimal(c) {
            current.push(c);
        } else if !current.is_empty() {
            let stripped = current.trim_start_matches('0');
            out.push(if stripped.is_empty() {
                "0".into()
            } else {
                stripped.into()
            });
            current.clear();
        }
    }
    out.sort();
    out
}
pub fn numeric_tokens_differ(a: &str, b: &str) -> bool {
    a != b && digit_runs(a) != digit_runs(b)
}
pub fn jaro(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let a: Vec<_> = a.chars().collect();
    let b: Vec<_> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let radius = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut aa = vec![false; a.len()];
    let mut bb = vec![false; b.len()];
    let mut matches = 0_usize;
    for i in 0..a.len() {
        for j in i.saturating_sub(radius)..(i + radius + 1).min(b.len()) {
            if !bb[j] && a[i] == b[j] {
                aa[i] = true;
                bb[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let x = a.iter().zip(aa).filter_map(|(c, yes)| yes.then_some(c));
    let y = b.iter().zip(bb).filter_map(|(c, yes)| yes.then_some(c));
    let transpositions = (x.zip(y).filter(|(x, y)| x != y).count() / 2) as f64;
    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions) / m) / 3.0
}
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let mut s = jaro(a, b);
    if s > 0.7 {
        let prefix = a
            .chars()
            .zip(b.chars())
            .take(4)
            .take_while(|(a, b)| a == b)
            .count();
        s += prefix as f64 * 0.1 * (1.0 - s);
    }
    s
}
pub fn same_word_variant(a: &str, b: &str) -> bool {
    let x: Vec<_> = a.chars().collect();
    let y: Vec<_> = b.chars().collect();
    if same_length_one_edit(&x, &y) {
        return true;
    }
    x.len().min(y.len()) >= 6 && jaro_winkler(a, b) * 100.0 >= 92.0
}
pub fn whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
pub fn content_token_swap(a: &str, b: &str) -> bool {
    const STOP: &[&str] = &[
        "a", "an", "the", "and", "or", "of", "for", "to", "in", "on", "at", "by", "with", "from",
        "as", "is", "are", "be", "this", "that", "its",
    ];
    let a: Vec<_> = a.split(whitespace).filter(|s| !s.is_empty()).collect();
    let b: Vec<_> = b.split(whitespace).filter(|s| !s.is_empty()).collect();
    a.len() == b.len()
        && a.iter().zip(&b).any(|(x, y)| {
            x != y && !STOP.contains(x) && !STOP.contains(y) && !same_word_variant(x, y)
        })
}
