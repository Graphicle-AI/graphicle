// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/base.py
//! Port of graphify/extractors/base.py.
pub use super::builtins_data::LANGUAGE_BUILTIN_GLOBALS;
use super::parse_cache::Source;
use crate::path_syntax::{posix_join, posix_name, posix_parts, suffix};
use std::{
    borrow::Cow,
    collections::HashSet,
    io,
    path::Path,
    sync::{LazyLock, Mutex},
};

pub fn make_id<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    crate::ids::make_id(parts)
}

pub fn is_language_builtin(name: &str) -> bool {
    LANGUAGE_BUILTIN_GLOBALS.binary_search(&name).is_ok()
}

pub fn file_stem(path: &Path) -> String {
    let spelling = path
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let mut parts = posix_parts(&spelling);
    let name = posix_name(&parts);
    if name.is_empty() {
        return String::new();
    }
    let name = &name[..name.len() - suffix(name).len()];
    let name = name.to_owned();
    *parts.last_mut().unwrap() = name;
    posix_join(&parts)
}

pub fn read_text(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    read_text_range(node.start_byte(), node.end_byte(), source)
}

/// Raw bytes spanned by `node`, clamped to `source` like [`read_text`].
pub fn node_bytes<'s>(node: tree_sitter::Node<'_>, source: &'s [u8]) -> &'s [u8] {
    let start = node.start_byte().min(source.len());
    let end = node.end_byte().min(source.len()).max(start);
    &source[start..end]
}

pub fn read_text_range(start: usize, end: usize, source: &[u8]) -> String {
    let start = start.min(source.len());
    let end = end.min(source.len()).max(start);
    String::from_utf8_lossy(&source[start..end]).into_owned()
}

/// Python's `cp1252` decoding table for 0x80..=0x9F; `None` marks the five
/// bytes the codec leaves undefined (it raises on them, so latin-1 is used).
const CP1252_HIGH: [Option<char>; 32] = [
    Some('\u{20AC}'),
    None,
    Some('\u{201A}'),
    Some('\u{0192}'),
    Some('\u{201E}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02C6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017D}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201C}'),
    Some('\u{201D}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02DC}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203A}'),
    Some('\u{0153}'),
    None,
    Some('\u{017E}'),
    Some('\u{0178}'),
];

/// `bytes.decode("cp1252")`, or `None` where Python's codec raises.
fn decode_cp1252(raw: &[u8]) -> Option<String> {
    raw.iter()
        .map(|&b| match b {
            0x80..=0x9F => CP1252_HIGH[usize::from(b - 0x80)],
            _ => Some(char::from(b)),
        })
        .collect()
}

/// Python's strict `"utf-16"`/`"utf-32"` codecs for input that starts with
/// the matching byte-order mark: the mark picks the byte order and is dropped;
/// a truncated unit, a lone surrogate or (UTF-32) a value outside Unicode is an
/// error, as `UnicodeDecodeError` is upstream.
fn decode_utf16_with_bom(raw: &[u8]) -> Option<String> {
    let little = raw.starts_with(b"\xff\xfe");
    let body = &raw[2..];
    if !body.len().is_multiple_of(2) {
        return None;
    }
    let units = body.chunks_exact(2).map(|u| {
        if little {
            u16::from_le_bytes([u[0], u[1]])
        } else {
            u16::from_be_bytes([u[0], u[1]])
        }
    });
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

fn decode_utf32_with_bom(raw: &[u8]) -> Option<String> {
    let little = raw.starts_with(b"\xff\xfe\x00\x00");
    let body = &raw[4..];
    if !body.len().is_multiple_of(4) {
        return None;
    }
    body.chunks_exact(4)
        .map(|u| {
            char::from_u32(if little {
                u32::from_le_bytes([u[0], u[1], u[2], u[3]])
            } else {
                u32::from_be_bytes([u[0], u[1], u[2], u[3]])
            })
        })
        .collect()
}

/// The decode `_read_source_bytes` applies to a file's raw bytes: the UTF-8
/// bytes tree-sitter parses, and the guessed encoding (`cp1252` or `latin-1`)
/// when neither UTF-8 nor a UTF-16/UTF-32 byte-order mark identified it.
///
/// Valid UTF-8, with or without a BOM, comes back unchanged (borrowed), so ids,
/// offsets and cache keys for those files do not move. A UTF-32 or UTF-16 file
/// with a BOM is decoded exactly (UTF-32 first: `FF FE 00 00` is also a UTF-16
/// little-endian BOM followed by U+0000). Anything else is read as cp1252, then
/// latin-1, which accepts every byte; line numbers hold in every case.
pub fn decode_source(raw: &[u8]) -> (Cow<'_, [u8]>, Option<&'static str>) {
    if raw.is_ascii() || std::str::from_utf8(raw).is_ok() {
        return (Cow::Borrowed(raw), None);
    }
    if (raw.starts_with(b"\xff\xfe\x00\x00") || raw.starts_with(b"\x00\x00\xfe\xff"))
        && let Some(text) = decode_utf32_with_bom(raw)
    {
        return (Cow::Owned(text.into_bytes()), None);
    }
    if (raw.starts_with(b"\xff\xfe") || raw.starts_with(b"\xfe\xff"))
        && let Some(text) = decode_utf16_with_bom(raw)
    {
        return (Cow::Owned(text.into_bytes()), None);
    }
    if let Some(text) = decode_cp1252(raw) {
        return (Cow::Owned(text.into_bytes()), Some("cp1252"));
    }
    let text: String = raw.iter().map(|&b| char::from(b)).collect();
    (Cow::Owned(text.into_bytes()), Some("latin-1"))
}

/// Paths already named by the not-UTF-8 warning in this process.
static WARNED_SOURCE_ENCODINGS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// `_read_source_bytes`: read a source file as the UTF-8 bytes tree-sitter
/// parses (see [`decode_source`]). A file read through a guessed encoding is
/// named once per process on stderr when `warn` is set; secondary passes that
/// re-read a file its extractor already read pass `warn = false`. Like
/// upstream it reads the file itself on every call, never the run cache
/// ([`super::parse_cache::read`]), whose entries outlive a rewrite of the file while
/// another extraction run in the process keeps the cache open.
pub fn read_source_bytes(path: &Path, warn: bool) -> io::Result<Source> {
    let raw = std::fs::read(path)?;
    let (decoded, guessed) = match decode_source(&raw) {
        (Cow::Borrowed(_), guessed) => (None, guessed),
        (Cow::Owned(bytes), guessed) => (Some(bytes), guessed),
    };
    if let Some(encoding) = guessed
        && warn
        && first_encoding_warning(path)
    {
        eprintln!(
            "[graphicle] WARNING: {} is not valid UTF-8; read it as {encoding}. \
             Re-save it as UTF-8 if names in the graph look wrong.",
            path.display()
        );
    }
    Ok(Source::from(decoded.unwrap_or(raw)))
}

/// Record that the not-UTF-8 warning names `path`; `false` when this process
/// already named it (`_warned_source_encodings`). Every reader that warns
/// shares the set, as the LLM prompt's source reads share upstream's.
pub(crate) fn first_encoding_warning(path: &Path) -> bool {
    WARNED_SOURCE_ENCODINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_string_lossy().into_owned())
}

/// `_read_source_text`: [`read_source_bytes`] as text, with the universal
/// newline translation `path.read_text(encoding="utf-8")` applies (`\r\n` and a
/// lone `\r` become `\n`; a BOM is kept), so for valid UTF-8 it is exactly the
/// text the regex extractors always saw.
pub fn read_source_text(path: &Path, warn: bool) -> io::Result<String> {
    let bytes = read_source_bytes(path, warn)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.into_owned()
    })
}

/// `index` in the integer type a tree-sitter `Node` method takes, which
/// differs between runtimes (`named_child` takes `usize` in 0.25 and `u32` in
/// 0.27, `field_name_for_child` the other way round), so the extractors build
/// with either; `None` if it does not fit.
pub(crate) fn ts_index<T: TryFrom<I>, I>(index: I) -> Option<T> {
    T::try_from(index).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_relative_path_survives_stem_creation() {
        assert_eq!(
            file_stem(Path::new("docs/v2/api/README.md")),
            "docs/v2/api/README"
        );
        assert_eq!(file_stem(Path::new("src//./x.test.ts")), "src/x.test");
        assert_eq!(file_stem(Path::new(".")), "");
        assert_eq!(file_stem(Path::new("a.")), "a.");
    }

    fn decoded(raw: &[u8]) -> (String, Option<&'static str>) {
        let (bytes, guessed) = decode_source(raw);
        (String::from_utf8(bytes.into_owned()).unwrap(), guessed)
    }

    #[test]
    fn source_decoding_matches_the_upstream_codecs() {
        // Valid UTF-8 (with a BOM too) is returned as is.
        assert!(matches!(
            decode_source(b"caf\xc3\xa9"),
            (Cow::Borrowed(_), None)
        ));
        assert!(matches!(
            decode_source(b"\xef\xbb\xbfx"),
            (Cow::Borrowed(_), None)
        ));
        // UTF-16 with either BOM, UTF-32 with either BOM; the mark is dropped.
        assert_eq!(decoded(b"\xff\xfeC\x00\xe9\x00"), ("Cé".into(), None));
        assert_eq!(decoded(b"\xfe\xff\x00C\x00\xe9"), ("Cé".into(), None));
        assert_eq!(
            decoded(b"\xff\xfe\x00\x00C\x00\x00\x00"),
            ("C".into(), None)
        );
        assert_eq!(
            decoded(b"\x00\x00\xfe\xff\x00\x00\x00C"),
            ("C".into(), None)
        );
        // `FF FE 00 00` that is not UTF-32 is UTF-16-LE: BOM, U+0000, ...
        assert_eq!(decoded(b"\xff\xfe\x00\x00A\x00"), ("\0A".into(), None));
        // A truncated or lone-surrogate UTF-16 body falls back to cp1252.
        assert_eq!(decoded(b"\xff\xfeA").1, Some("cp1252"));
        assert_eq!(decoded(b"\xff\xfe\x00\xd8A\x00").1, Some("cp1252"));
        // cp1252, then latin-1 where cp1252 leaves a byte undefined.
        assert_eq!(decoded(b"Caf\xe9\x80"), ("Café€".into(), Some("cp1252")));
        assert_eq!(decoded(b"x\x81\xe9"), ("x\u{81}é".into(), Some("latin-1")));
    }

    #[test]
    fn source_text_folds_newlines_and_keeps_the_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"\xef\xbb\xbfa\r\nb\rc\xe9").unwrap();
        // Not UTF-8 (the trailing 0xE9), so cp1252 decides; BOM bytes become text.
        assert_eq!(
            read_source_text(&path, false).unwrap(),
            "\u{ef}\u{bb}\u{bf}a\nb\ncé"
        );
        std::fs::write(&path, b"\xef\xbb\xbfa\r\nb\rc").unwrap();
        assert_eq!(read_source_text(&path, false).unwrap(), "\u{feff}a\nb\nc");
    }
}
