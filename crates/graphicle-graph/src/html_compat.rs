// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/csharp.py
//! Native HTML5 unescaping compatible with CPython 3.12 html.unescape.
//! Tables/rules originate in CPython; retained PSF license is in licenses/.
use std::{collections::BTreeMap, sync::LazyLock};
static ENTITIES: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("html_entities.json")).expect("generated entity data")
});
static REFERENCES: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"&(#[0-9]+;?|#[xX][0-9a-fA-F]+;?|[^\t\n\x0c <&#;]{1,32};?)").unwrap()
});
fn numeric(n: u32) -> String {
    if n == 0 {
        return "\u{fffd}".into();
    }
    if (0x80..=0x9f).contains(&n) {
        const C1: [char; 32] = [
            '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
            '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ',
            '\u{9d}', 'ž', 'Ÿ',
        ];
        return C1[(n - 0x80) as usize].into();
    }
    if (0xd800..=0xdfff).contains(&n) || n > 0x10ffff {
        return "\u{fffd}".into();
    }
    if matches!(n,1..=8 | 11 | 14..=31 | 0x7f | 0xfdd0..=0xfdef) || n & 0xffff >= 0xfffe {
        return String::new();
    }
    char::from_u32(n).unwrap().into()
}
pub fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.into();
    }
    REFERENCES
        .replace_all(text, |captures: &regex::Captures<'_>| {
            let value = &captures[1];
            if let Some(value) = value.strip_prefix('#') {
                let value = value.trim_end_matches(';');
                let (digits, radix) = if value.starts_with(['x', 'X']) {
                    (&value[1..], 16)
                } else {
                    (value, 10)
                };
                numeric(u32::from_str_radix(digits, radix).unwrap_or(0x110000))
            } else if let Some(replacement) = ENTITIES.get(value) {
                replacement.clone()
            } else {
                for (offset, _) in value.char_indices().rev() {
                    if offset < 2 {
                        continue;
                    }
                    if let Some(replacement) = ENTITIES.get(&value[..offset]) {
                        return format!("{replacement}{}", &value[offset..]);
                    }
                }
                format!("&{value}")
            }
        })
        .into_owned()
}
