// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/ids.py
//! Port of graphify/ids.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
//! Copyright notices of the upstream work: NOTICE; modified for Rust by KarLex AI, Inc.
use crate::unicode15::{CASEFOLD, WORD_RANGES};
use unicode_normalization::UnicodeNormalization;

fn casefold(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match CASEFOLD.binary_search_by_key(&c, |entry| entry.0) {
            Ok(index) => out.push_str(CASEFOLD[index].1),
            Err(_) => out.push(c),
        }
    }
    out
}

fn python_word(c: char) -> bool {
    let cp = c as u32;
    let index = WORD_RANGES.partition_point(|&(start, _)| start <= cp);
    index > 0 && cp <= WORD_RANGES[index - 1].1
}

/// Full Unicode casefold then NFKC, iterated to the upstream six-pass bound.
/// Python's Unicode word predicate excludes combining marks; Rust regex `\w`
/// has a broader definition and must not be substituted here.
pub fn normalize_id(value: &str) -> String {
    // ASCII is already NFKC-normalized; its casefold and word classes are
    // exactly these byte operations. Keep the Unicode path for all other text.
    if value.is_ascii() {
        let mut out = String::with_capacity(value.len());
        let mut separator = false;
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() {
                if separator {
                    out.push('_');
                    separator = false;
                }
                out.push(byte.to_ascii_lowercase() as char);
            } else {
                separator = !out.is_empty();
            }
        }
        return out;
    }
    let mut current = value.to_owned();
    for _ in 0..6 {
        let next: String = casefold(&current).nfkc().collect();
        if next == current {
            break;
        }
        current = next;
        // `current` is NFKC output; when casefolding would leave it as it is,
        // the next pass returns it unchanged, so it is already the result.
        if current
            .chars()
            .all(|c| CASEFOLD.binary_search_by_key(&c, |entry| entry.0).is_err())
        {
            break;
        }
    }
    let mut out = String::with_capacity(current.len());
    let mut separator = false;
    for c in current.chars() {
        if c == '_' || !python_word(c) {
            separator = !out.is_empty();
        } else {
            if separator {
                out.push('_');
                separator = false;
            }
            out.push(c);
        }
    }
    out
}

pub fn make_id<I, S>(parts: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut joined = String::new();
    let mut first = true;
    for part in parts {
        let value = part.as_ref();
        if value.is_empty() {
            continue;
        }
        if !first {
            joined.push('_');
        }
        first = false;
        joined.push_str(value.trim_matches(['_', '.']));
    }
    normalize_id(&joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_graph_identity_across_unicode_and_punctuation() {
        assert_eq!(unicode_normalization::UNICODE_VERSION, (15, 0, 0));
        assert_eq!(normalize_id("cafe\u{301}"), normalize_id("café"));
        assert_eq!(normalize_id("Große"), "grosse");
        assert_eq!(make_id(["islem", "İslemYap"]), "islem_i_slemyap");
        assert_ne!(normalize_id("クラスА"), normalize_id("クラスB"));
        assert_eq!(
            make_id(["..src__", "__main.py..", "", "__A"]),
            "src_main_py_a"
        );
    }

    #[test]
    fn normalization_is_stable_for_combining_mark_sequences() {
        for value in [
            "\u{345}\u{300}",
            "\u{345}\u{301}",
            "a\u{345}\u{300}b",
            "\u{1f0}\u{f35}\u{345}",
        ] {
            let normalized = normalize_id(value);
            assert_eq!(normalized, normalize_id(&normalized));
            assert_eq!(normalized, normalize_id(&casefold(value)));
        }
    }

    #[test]
    fn ascii_identifiers_keep_only_letters_and_digits_separated_by_underscores() {
        let all_ascii: String = (0..=127).map(char::from).collect();
        assert_eq!(
            normalize_id(&all_ascii),
            "0123456789_abcdefghijklmnopqrstuvwxyz_abcdefghijklmnopqrstuvwxyz"
        );
        assert_eq!(
            normalize_id("__HTTP/2.0__Request---ID__"),
            "http_2_0_request_id"
        );
        assert_eq!(make_id(["__", "...", "", "A", "..B__"]), "a_b");
        assert_eq!(normalize_id("\0\x1f\x7f"), "");
    }

    /// The pass loop as it was before stopping once casefolding is a no-op.
    fn reference_normalize(value: &str) -> String {
        let mut current = value.to_owned();
        for _ in 0..6 {
            let next: String = casefold(&current).nfkc().collect();
            if next == current {
                break;
            }
            current = next;
        }
        let mut out = String::new();
        let mut separator = false;
        for c in current.chars() {
            if c == '_' || !python_word(c) {
                separator = !out.is_empty();
            } else {
                if separator {
                    out.push('_');
                    separator = false;
                }
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn early_stop_matches_the_full_pass_loop() {
        let mut samples = vec![
            "Große Straße".to_string(),
            "ǅemal ǈubljana ǋ".into(),
            "ﬁle ﬀ ﬃ ﬄ".into(),
            "ΣΊΣΥΦΟΣ ς".into(),
            "İstanbul ı".into(),
            "①②③ ⅨⅪ ㎏ ㍿".into(),
            "Ǆ ǅ ǆ ᾈ ᾼ ῼ".into(),
            "\u{345}\u{300}\u{1f0}\u{f35}".into(),
            "日本語 テキスト ｶﾀｶﾅ".into(),
            "Ångström Ω K".into(),
        ];
        // Every code point up to U+2FFFF in short runs, mixed with ASCII.
        let mut run = String::new();
        for cp in (0x80u32..0x30000).step_by(7) {
            if let Some(c) = char::from_u32(cp) {
                run.push(c);
                if run.chars().count() == 9 {
                    samples.push(format!("A{run}z"));
                    run.clear();
                }
            }
        }
        for sample in &samples {
            assert_eq!(
                normalize_id(sample),
                reference_normalize(sample),
                "{sample:?}"
            );
        }
    }
}
