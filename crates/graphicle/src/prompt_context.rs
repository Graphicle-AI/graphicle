//! Bounded auxiliary context in model prompts.
//!
//! Adapted from Hindsight (vectorize-io/hindsight 017b3f5, "truncate memory
//! context in internal prompts", #5086; MIT): a caller can store a long
//! auxiliary context on every item, and repeated once per item it bloats the
//! prompt. Here that context is a source's title (up to 1,000 bytes from a
//! connector, the whole path for a repository file): a prompt shows at most
//! [`MAX_CHARS`] characters of it plus `...`. Source text, quotes, source
//! headers, memory titles and API output are never cut, and
//! titles that already fit are unchanged, so their request digests are too.
use serde_json::Value;
use std::borrow::Cow;

/// Characters of a title shown in a prompt (Hindsight's
/// `PROMPT_CONTEXT_MAX_CHARS`).
pub const MAX_CHARS: usize = 200;

/// `text[:MAX_CHARS] + "..."` when `text` has more than [`MAX_CHARS`]
/// characters; otherwise `text` itself.
pub fn truncate(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(MAX_CHARS) {
        Some((end, _)) => Cow::Owned(format!("{}...", &text[..end])),
        None => Cow::Borrowed(text),
    }
}

/// A title as a prompt shows it. Values other than strings are unchanged.
pub fn title(value: &Value) -> Value {
    match value.as_str() {
        Some(text) => match truncate(text) {
            Cow::Owned(cut) => Value::String(cut),
            Cow::Borrowed(_) => value.clone(),
        },
        None => value.clone(),
    }
}

/// Bounds the `title` of each source window in place.
pub fn bound(items: &mut [Value]) {
    for item in items {
        if let Some(value) = item.get_mut("title") {
            *value = title(value);
        }
    }
}

/// Copies of source windows whose titles are bounded for a prompt; the
/// originals stay intact for citation checks and API output.
pub fn sources(items: &[Value]) -> Vec<Value> {
    let mut copies = items.to_vec();
    bound(&mut copies);
    copies
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cuts_long_titles_like_hindsight() {
        let long = "x".repeat(MAX_CHARS + 50);
        assert_eq!(truncate(&long), format!("{}...", "x".repeat(MAX_CHARS)));
        let exact = "x".repeat(MAX_CHARS);
        assert_eq!(truncate(&exact), exact);
        assert!(matches!(truncate(&exact), Cow::Borrowed(_)));
        assert_eq!(truncate("chat"), "chat");
        assert_eq!(truncate(""), "");
    }

    #[test]
    fn counts_characters_not_bytes() {
        let long = "é".repeat(MAX_CHARS + 1);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), MAX_CHARS + 3);
        assert!(cut.starts_with(&"é".repeat(MAX_CHARS)) && cut.ends_with("..."));
        assert_eq!(truncate(&"é".repeat(MAX_CHARS)), "é".repeat(MAX_CHARS));
    }

    #[test]
    fn bounds_only_titles_of_copies() {
        let long = "t".repeat(MAX_CHARS * 3);
        let text = "w".repeat(MAX_CHARS * 3);
        let originals = vec![
            json!({"id":"a","title":long,"text":text,"source_context":{"text":long}}),
            json!({"id":"b","title":"short","text":"body"}),
            json!({"id":"c","title":null,"text":"untitled"}),
            json!({"id":"d","text":"no title field"}),
        ];
        let shown = sources(&originals);
        assert_eq!(shown[0]["title"], format!("{}...", "t".repeat(MAX_CHARS)));
        assert_eq!(shown[0]["text"], text);
        assert_eq!(shown[0]["source_context"]["text"], long);
        assert_eq!(shown[1], originals[1]);
        assert_eq!(shown[2], originals[2]);
        assert_eq!(shown[3], originals[3]);
        assert_eq!(originals[0]["title"], long);
        assert_eq!(title(&json!(7)), json!(7));
    }
}
