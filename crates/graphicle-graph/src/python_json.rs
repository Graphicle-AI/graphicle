// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/paths.py
//! Streaming JSON layout compatible with the pinned json.dump defaults.
use serde::Serialize;
use serde_json::ser::Formatter;
use std::io::{self, Write};

struct Format {
    indent: Option<usize>,
    ascii: bool,
    /// `separators=(",", ":")`: no padding after either separator.
    compact: bool,
    nonempty: Vec<bool>,
}
/// Indentation is written from here in slices rather than one space at a time.
const SPACES: &[u8] = &[b' '; 128];
impl Format {
    fn newline<W: Write + ?Sized>(&self, w: &mut W) -> io::Result<()> {
        if let Some(indent) = self.indent {
            w.write_all(b"\n")?;
            let mut n = self.nonempty.len() * indent;
            while n > 0 {
                let k = n.min(SPACES.len());
                w.write_all(&SPACES[..k])?;
                n -= k;
            }
        }
        Ok(())
    }
    fn before<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if !first {
            w.write_all(if self.indent.is_some() || self.compact {
                b","
            } else {
                b", "
            })?;
        }
        self.newline(w)?;
        if let Some(value) = self.nonempty.last_mut() {
            *value = true;
        }
        Ok(())
    }
}
impl Formatter for Format {
    fn begin_array<W: Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        self.nonempty.push(false);
        w.write_all(b"[")
    }
    fn end_array<W: Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        let any = self.nonempty.pop().unwrap_or(false);
        if any {
            self.newline(w)?;
        }
        w.write_all(b"]")
    }
    fn begin_array_value<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.before(w, first)
    }
    fn begin_object<W: Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        self.nonempty.push(false);
        w.write_all(b"{")
    }
    fn end_object<W: Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        let any = self.nonempty.pop().unwrap_or(false);
        if any {
            self.newline(w)?;
        }
        w.write_all(b"}")
    }
    fn begin_object_key<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.before(w, first)
    }
    fn begin_object_value<W: Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(if self.compact { b":" } else { b": " })
    }
    fn write_string_fragment<W: Write + ?Sized>(&mut self, w: &mut W, s: &str) -> io::Result<()> {
        if !self.ascii {
            return w.write_all(s.as_bytes());
        }
        // Runs below DEL are written as they are, in one call; every other
        // character (DEL and all non-ASCII) is escaped on its own.
        let bytes = s.as_bytes();
        let mut start = 0;
        while start < bytes.len() {
            let run = bytes[start..]
                .iter()
                .position(|b| *b >= 0x7f)
                .unwrap_or(bytes.len() - start);
            if run > 0 {
                w.write_all(&bytes[start..start + run])?;
                start += run;
                continue;
            }
            let c = s[start..]
                .chars()
                .next()
                .expect("a character at a boundary");
            start += c.len_utf8();
            let cp = c as u32;
            if cp <= 0xffff {
                write!(w, "\\u{cp:04x}")?;
            } else {
                let cp = cp - 0x10000;
                write!(
                    w,
                    "\\u{:04x}\\u{:04x}",
                    0xd800 + (cp >> 10),
                    0xdc00 + (cp & 0x3ff)
                )?;
            }
        }
        Ok(())
    }
    fn write_f64<W: Write + ?Sized>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        w.write_all(crate::python_compat::repr(&serde_json::json!(value)).as_bytes())
    }
}
pub fn write<W: Write, T: Serialize>(
    writer: W,
    value: &T,
    indent: Option<usize>,
    ensure_ascii: bool,
) -> io::Result<()> {
    write_at_depth(writer, value, indent, ensure_ascii, 0)
}
/// [`write`] for a value that sits `depth` containers deep in a larger
/// document: its lines are indented as that document would indent them, so
/// separately rendered parts can be concatenated into the same bytes.
pub fn write_at_depth<W: Write, T: Serialize>(
    writer: W,
    value: &T,
    indent: Option<usize>,
    ensure_ascii: bool,
    depth: usize,
) -> io::Result<()> {
    write_formatted(writer, value, indent, ensure_ascii, false, depth)
}
/// `json.dump(value, separators=(",", ":"))`: one line without padding
/// (`paths.write_json_atomic(compact=True)`, Graphify 0.9.86, #3413).
pub fn write_compact<W: Write, T: Serialize>(
    writer: W,
    value: &T,
    ensure_ascii: bool,
) -> io::Result<()> {
    write_formatted(writer, value, None, ensure_ascii, true, 0)
}
/// [`write`] with `compact` choosing `separators=(",", ":")`: no padding after
/// a key; with an indent the item separator is `","` either way.
pub fn write_with<W: Write, T: Serialize>(
    writer: W,
    value: &T,
    indent: Option<usize>,
    ensure_ascii: bool,
    compact: bool,
) -> io::Result<()> {
    write_formatted(writer, value, indent, ensure_ascii, compact, 0)
}
fn write_formatted<W: Write, T: Serialize>(
    writer: W,
    value: &T,
    indent: Option<usize>,
    ensure_ascii: bool,
    compact: bool,
    depth: usize,
) -> io::Result<()> {
    let formatter = Format {
        indent,
        ascii: ensure_ascii,
        compact,
        nonempty: vec![true; depth],
    };
    value
        .serialize(&mut serde_json::Serializer::with_formatter(
            writer, formatter,
        ))
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The formatter before its fast paths, kept as the reference.
    fn reference(value: &serde_json::Value, indent: Option<usize>, ascii: bool) -> String {
        fn text(v: &serde_json::Value, indent: Option<usize>, ascii: bool, depth: usize) -> String {
            let pad = |d: usize| indent.map(|i| format!("\n{}", " ".repeat(d * i)));
            let sep = if indent.is_some() { "," } else { ", " };
            let string = |s: &str| {
                let quoted = serde_json::to_string(s).unwrap();
                if !ascii {
                    return quoted;
                }
                let mut out = String::new();
                for c in quoted.chars() {
                    let cp = c as u32;
                    if cp < 0x7f {
                        out.push(c);
                    } else if cp <= 0xffff {
                        out.push_str(&format!("\\u{cp:04x}"));
                    } else {
                        let cp = cp - 0x10000;
                        out.push_str(&format!(
                            "\\u{:04x}\\u{:04x}",
                            0xd800 + (cp >> 10),
                            0xdc00 + (cp & 0x3ff)
                        ));
                    }
                }
                out
            };
            match v {
                serde_json::Value::Array(items) if !items.is_empty() => {
                    let inner = items
                        .iter()
                        .map(|x| {
                            format!(
                                "{}{}",
                                pad(depth + 1).unwrap_or_default(),
                                text(x, indent, ascii, depth + 1)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(sep);
                    format!("[{inner}{}]", pad(depth).unwrap_or_default())
                }
                serde_json::Value::Object(map) if !map.is_empty() => {
                    let inner = map
                        .iter()
                        .map(|(k, x)| {
                            format!(
                                "{}{}: {}",
                                pad(depth + 1).unwrap_or_default(),
                                string(k),
                                text(x, indent, ascii, depth + 1)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(sep);
                    format!("{{{inner}{}}}", pad(depth).unwrap_or_default())
                }
                serde_json::Value::Array(_) => "[]".into(),
                serde_json::Value::Object(_) => "{}".into(),
                serde_json::Value::String(s) => string(s),
                other => other.to_string(),
            }
        }
        text(value, indent, ascii, 0)
    }

    #[test]
    fn fast_paths_write_the_reference_bytes() {
        let value = serde_json::json!({
            "plain": "abc", "mixed": "caf\u{e9} \u{7f} \u{1f600} x\"y\\z\n\t",
            "deep": {"a": [1, [2, {"b": []}], {}], "c": "\u{2603}"},
            "wide": "x".repeat(300), "empty": [], "n": -1.5e-7
        });
        for indent in [None, Some(2), Some(40)] {
            for ascii in [false, true] {
                let mut out = vec![];
                write(&mut out, &value, indent, ascii).unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    reference(&value, indent, ascii)
                );
            }
        }
    }

    #[test]
    fn compact_layout_has_no_padding_and_one_line() {
        let value = serde_json::json!({"a": [1, {"b": "caf\u{e9}"}], "c": {}, "d": []});
        let mut out = vec![];
        write_compact(&mut out, &value, false).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"a\":[1,{\"b\":\"caf\u{e9}\"}],\"c\":{},\"d\":[]}"
        );
        let mut out = vec![];
        write_compact(&mut out, &value, true).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"a\":[1,{\"b\":\"caf\\u00e9\"}],\"c\":{},\"d\":[]}"
        );
    }

    #[test]
    fn a_part_rendered_at_its_depth_splices_into_the_document() {
        let part = serde_json::json!({"id": "x", "tags": ["a", {"k": 1}]});
        let doc = serde_json::json!({"outer": {"list": [part.clone(), part.clone()]}});
        let mut whole = vec![];
        write(&mut whole, &doc, Some(2), true).unwrap();
        let mut spliced = b"{\n  \"outer\": {\n    \"list\": [".to_vec();
        for i in 0..2 {
            spliced.extend_from_slice(if i == 0 { b"\n      " } else { b",\n      " });
            write_at_depth(&mut spliced, &part, Some(2), true, 3).unwrap();
        }
        spliced.extend_from_slice(b"\n    ]\n  }\n}");
        assert_eq!(
            String::from_utf8(spliced).unwrap(),
            String::from_utf8(whole).unwrap()
        );
    }
}
