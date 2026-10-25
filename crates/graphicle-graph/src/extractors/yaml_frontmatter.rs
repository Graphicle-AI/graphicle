// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/markdown.py, graphify/extractors/yaml_config.py
//! YAML frontmatter with the reference's YAML-1.1 scalar and merge semantics.
//! Uses a Rust parser, preserving scalar style before resolving types.
//! Scalar grammar follows PyYAML (MIT; see licenses/PyYAML-LICENSE).
use crate::{Result, python_compat};
use base64::Engine;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::LazyLock};
use yaml_rust2::{
    parser::{Event, EventReceiver, Parser, Tag},
    scanner::{Scanner, TScalarStyle, Token, TokenType},
};

static INT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"^[-+]?(?:0b[0-1_]+|0[0-7_]+|(?:0|[1-9][0-9_]*)|0x[0-9a-fA-F_]+|[1-9][0-9_]*(?::[0-5]?[0-9])+)\z").unwrap()
});
static FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5][0-9]|:[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))\z").unwrap()
});
static DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"^(\d{4})-(\d{1,2})-(\d{1,2})(?:(?:[Tt]|[ \t]+)(\d{1,2}):(\d{2}):(\d{2})(?:\.(\d*))?(?:[ \t]*(Z|([-+])(\d{1,2})(?::(\d{2}))?))?)?\z").unwrap()
});

#[derive(Clone)]
enum Node {
    Scalar(Value),
    /// A float JSON cannot hold (`.inf`, `-.inf`, `.nan`, `1e999`), kept only
    /// when the builder allows it ([`parse_all`]); [`expand`] refuses it.
    NonFinite(f64),
    Merge,
    Sequence(Vec<usize>, String),
    Mapping(Vec<usize>, String),
    Alias(usize),
}
#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
    stack: Vec<usize>,
    anchors: HashMap<usize, usize>,
    roots: Vec<usize>,
    error: Option<String>,
    /// Keep non-finite floats as [`Node::NonFinite`] instead of failing.
    nonfinite: bool,
}
impl Builder {
    fn add(&mut self, node: Node, anchor: usize) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        if anchor > 0 {
            self.anchors.insert(anchor, id);
        }
        if let Some(&parent) = self.stack.last() {
            match &mut self.nodes[parent] {
                Node::Mapping(children, _) | Node::Sequence(children, _) => children.push(id),
                _ => unreachable!(),
            }
        } else {
            self.roots.push(id);
        }
        id
    }
}
fn tag_name(tag: Option<Tag>, default: &str) -> Result<String> {
    match tag {
        None => Ok(default.into()),
        Some(tag) if tag.handle == "tag:yaml.org,2002:" => Ok(tag.suffix),
        Some(tag) if tag.handle.is_empty() && tag.suffix == "!" => Ok(default.into()),
        _ => Err("Unsupported safe YAML tag".into()),
    }
}
impl EventReceiver for Builder {
    fn on_event(&mut self, event: Event) {
        if self.error.is_some() {
            return;
        }
        let result: Result<()> = (|| {
            match event {
                Event::Scalar(text, style, anchor, tag) => {
                    let implicit = style == TScalarStyle::Plain && tag.is_none();
                    let kind = tag_name(tag, "str")?;
                    if kind == "merge" || (implicit && text == "<<") {
                        self.add(Node::Merge, anchor);
                    } else {
                        match scalar(&text, &kind, implicit) {
                            Ok(value) => {
                                self.add(Node::Scalar(value), anchor);
                            }
                            Err(error) if self.nonfinite && error.to_string() == NON_JSON_FLOAT => {
                                let value = python_float(&text).ok_or(error)?;
                                self.add(Node::NonFinite(value), anchor);
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
                Event::SequenceStart(anchor, tag) => {
                    let kind = tag_name(tag, "seq")?;
                    if !["seq", "omap", "pairs"].contains(&kind.as_str()) {
                        return Err("Invalid YAML sequence tag".into());
                    }
                    let id = self.add(Node::Sequence(vec![], kind), anchor);
                    self.stack.push(id);
                }
                Event::MappingStart(anchor, tag) => {
                    let kind = tag_name(tag, "map")?;
                    if !["map", "set"].contains(&kind.as_str()) {
                        return Err("Invalid YAML mapping tag".into());
                    }
                    let id = self.add(Node::Mapping(vec![], kind), anchor);
                    self.stack.push(id);
                }
                Event::SequenceEnd | Event::MappingEnd => {
                    self.stack.pop();
                }
                Event::Alias(anchor) => {
                    let id = *self.anchors.get(&anchor).ok_or("Unknown YAML alias")?;
                    self.add(Node::Alias(id), 0);
                }
                _ => (),
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(error.to_string());
        }
    }
}
fn integer(text: &str) -> Result<Value> {
    let text = text.replace('_', "");
    let negative = text.starts_with('-');
    let digits = text.trim_start_matches(['-', '+']);
    let value = if let Some(bits) = digits.strip_prefix("0b") {
        i128::from_str_radix(bits, 2).ok()
    } else if let Some(hex) = digits.strip_prefix("0x") {
        i128::from_str_radix(hex, 16).ok()
    } else if digits.starts_with('0') {
        i128::from_str_radix(digits, 8).ok()
    } else if digits.contains(':') {
        digits.split(':').try_fold(0i128, |acc, digit| {
            acc.checked_mul(60)?.checked_add(digit.parse().ok()?)
        })
    } else {
        digits.parse().ok()
    }
    .ok_or("Invalid YAML integer")?;
    let value = if negative { -value } else { value };
    if let Ok(value) = i64::try_from(value) {
        Ok(json!(value))
    } else if let Ok(value) = u64::try_from(value) {
        Ok(json!(value))
    } else {
        Err("YAML integer exceeds native JSON range".into())
    }
}
fn timestamp(text: &str) -> Result<Value> {
    let c = DATE.captures(text).ok_or("Invalid YAML timestamp")?;
    let num = |i: usize| {
        c.get(i)
            .map_or(Ok(0), |m| m.as_str().parse::<i32>())
            .map_err(|_| "Invalid timestamp".to_string())
    };
    let (year, month, day) = (num(1)?, num(2)?, num(3)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    if year == 0 || day < 1 || day > days {
        return Err("Invalid YAML date".into());
    }
    let mut out = format!("{year:04}-{month:02}-{day:02}");
    if c.get(4).is_some() {
        let (hour, minute, second) = (num(4)?, num(5)?, num(6)?);
        if hour > 23 || minute > 59 || second > 59 {
            return Err("Invalid YAML time".into());
        }
        out.push_str(&format!(" {hour:02}:{minute:02}:{second:02}"));
        if let Some(fraction) = c.get(7) {
            let mut fraction = fraction.as_str().chars().take(6).collect::<String>();
            while fraction.len() < 6 {
                fraction.push('0');
            }
            if fraction != "000000" {
                out.push('.');
                out.push_str(&fraction);
            }
        }
        if let Some(zone) = c.get(8) {
            let mut minutes = num(10)? * 60 + num(11)?;
            if minutes >= 1440 {
                return Err("Invalid YAML timezone".into());
            }
            if c.get(9).is_some_and(|m| m.as_str() == "-") {
                minutes = -minutes;
            }
            if zone.as_str() == "Z" || minutes == 0 {
                out.push_str("+00:00");
            } else {
                out.push_str(&format!(
                    "{}{:02}:{:02}",
                    if minutes < 0 { '-' } else { '+' },
                    minutes.abs() / 60,
                    minutes.abs() % 60
                ));
            }
        }
    }
    Ok(json!(out))
}
fn bytes_repr(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = format!("b{}", quote as char);
    for &byte in bytes {
        if byte == quote || byte == b'\\' {
            out.push('\\');
            out.push(byte as char);
        } else {
            match byte {
                b'\n' => out.push_str("\\n"),
                b'\r' => out.push_str("\\r"),
                b'\t' => out.push_str("\\t"),
                32..=126 => out.push(byte as char),
                _ => out.push_str(&format!("\\x{byte:02x}")),
            }
        }
    }
    out.push(quote as char);
    out
}
const NON_JSON_FLOAT: &str = "Non-JSON YAML float";
/// `construct_yaml_float` for the floats JSON cannot hold: `.inf` with its
/// sign, `.nan`, or a literal past the f64 range (`1.0e+999`).
fn python_float(text: &str) -> Option<f64> {
    let value = text.replace('_', "").to_lowercase();
    let (sign, rest) = match value.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, value.strip_prefix('+').unwrap_or(&value)),
    };
    let magnitude = match rest {
        ".inf" => f64::INFINITY,
        ".nan" => f64::NAN,
        _ => rest.parse::<f64>().ok()?,
    };
    Some(sign * magnitude).filter(|v| !v.is_finite())
}
/// Python's `str()` of a non-finite float.
fn python_float_text(value: f64) -> &'static str {
    if value.is_nan() {
        "nan"
    } else if value > 0.0 {
        "inf"
    } else {
        "-inf"
    }
}
fn scalar(text: &str, explicit: &str, implicit: bool) -> Result<Value> {
    let mut kind = explicit;
    if implicit {
        kind = if [
            "yes", "Yes", "YES", "no", "No", "NO", "true", "True", "TRUE", "false", "False",
            "FALSE", "on", "On", "ON", "off", "Off", "OFF",
        ]
        .contains(&text)
        {
            "bool"
        } else if ["", "~", "null", "Null", "NULL"].contains(&text) {
            "null"
        } else if FLOAT.is_match(text) {
            "float"
        } else if INT.is_match(text) {
            "int"
        } else if DATE.is_match(text) && (text.len() == 10 || text.contains(['T', 't', ' ', '\t']))
        {
            "timestamp"
        } else {
            "str"
        };
    }
    match kind {
        "str" | "value" => Ok(json!(text)),
        "null" => Ok(Value::Null),
        "bool" => match text.to_lowercase().as_str() {
            "yes" | "true" | "on" => Ok(json!(true)),
            "no" | "false" | "off" => Ok(json!(false)),
            _ => Err("Invalid YAML bool".into()),
        },
        "int" => integer(text),
        "float" => {
            let digits = text.replace('_', "").to_lowercase();
            let value = if digits.contains(':') {
                let negative = digits.starts_with('-');
                let value = digits
                    .trim_start_matches(['+', '-'])
                    .split(':')
                    .try_fold(0.0, |acc, digit| {
                        digit.parse::<f64>().ok().map(|n| acc * 60.0 + n)
                    })
                    .ok_or("Invalid YAML float")?;
                if negative { -value } else { value }
            } else {
                digits.parse::<f64>().map_err(|_| NON_JSON_FLOAT)?
            };
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| NON_JSON_FLOAT.into())
        }
        "timestamp" => timestamp(text),
        "binary" => {
            if !text.is_ascii() {
                return Err("Non-ASCII base64".into());
            }
            let bytes = text
                .bytes()
                .filter(|b| b.is_ascii_alphanumeric() || b"+/=".contains(b))
                .collect::<Vec<_>>();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(bytes)
                .map_err(|_| "Invalid base64")?;
            Ok(json!(bytes_repr(&bytes)))
        }
        _ => Err("Unsupported safe YAML scalar tag".into()),
    }
}
fn expand(nodes: &[Node], id: usize, depth: usize) -> Result<Value> {
    // Refuse recursive aliases before materializing JSON; the Python exporter
    // cannot serialize these either. The caller uses its flat fallback.
    if depth > 128 {
        return Err("Recursive or excessively nested YAML".into());
    }
    match &nodes[id] {
        Node::Scalar(value) => Ok(value.clone()),
        Node::NonFinite(_) => Err(NON_JSON_FLOAT.into()),
        Node::Merge => Ok(json!("<<")),
        Node::Alias(id) => expand(nodes, *id, depth + 1),
        Node::Sequence(children, kind) => {
            let mut values = Vec::new();
            for &child in children {
                let value = expand(nodes, child, depth + 1)?;
                if kind == "seq" {
                    values.push(value);
                } else {
                    let map = value
                        .as_object()
                        .filter(|map| map.len() == 1)
                        .ok_or("Invalid YAML pairs")?;
                    let (key, value) = map.iter().next().unwrap();
                    values.push(json!([key, value]));
                }
            }
            Ok(Value::Array(values))
        }
        Node::Mapping(children, kind) => {
            let mut entries: Vec<(Value, Value)> = Vec::new();
            let mut inherited = Map::new();
            for pair in children.chunks_exact(2) {
                let key = expand(nodes, pair[0], depth + 1)?;
                let value = expand(nodes, pair[1], depth + 1)?;
                if matches!(nodes[pair[0]], Node::Merge) {
                    if let Value::Object(map) = value {
                        inherited.extend(map);
                    } else if let Value::Array(values) = value {
                        for value in values.into_iter().rev() {
                            inherited.extend(
                                value
                                    .as_object()
                                    .ok_or("Invalid YAML merge sequence")?
                                    .clone(),
                            );
                        }
                    } else {
                        return Err("Invalid YAML merge".into());
                    }
                } else {
                    let scalar = python_compat::scalar_key(&key).ok_or("Unhashable YAML key")?;
                    if let Some((_, old)) = entries
                        .iter_mut()
                        .find(|(k, _)| python_compat::scalar_key(k) == Some(scalar.clone()))
                    {
                        *old = value;
                    } else {
                        entries.push((key, value));
                    }
                }
            }
            if kind == "set" {
                // Set iteration is unspecified upstream. Stable spelling avoids
                // process-hash-dependent graph output in the native interface.
                let mut keys = entries
                    .iter()
                    .map(|(key, _)| python_compat::repr(key))
                    .collect::<Vec<_>>();
                keys.sort();
                return Ok(json!(if keys.is_empty() {
                    "set()".into()
                } else {
                    format!("{{{}}}", keys.join(", "))
                }));
            }
            for (key, value) in entries {
                inherited.insert(python_compat::string(&key), value);
            }
            Ok(Value::Object(inherited))
        }
    }
}
/// One scan of `text`: refuses duplicate anchors and, when the scan stops at an
/// indentation error inside a flow collection, returns `text` with the rejected
/// line moved right of the outermost open bracket.
///
/// PyYAML ignores indentation in flow context (`Scanner.unwind_indent`: "In the
/// flow context, indentation is ignored. We make the scanner less restrictive
/// then specification requires."); yaml-rust2 does not. Front matter such as
/// Hugo's
///
/// ```yaml
/// aliases: [
///   '/templates/base/',
/// ]
/// ```
///
/// (the closing bracket at the key's column) is therefore YAML upstream but a
/// scan error here, and the flat fallback stored `"aliases": "["` and string
/// scalars. Leading spaces inside a flow collection carry no content, so the
/// repaired text has the reference's value. The scanner hands over a bracket
/// only once it knows that the bracket does not start a key, so a flow
/// collection that opens a line or follows `- ` is not repaired. Any other scan
/// error is left for the parser to report.
fn scan_repairing_flow_indentation(text: &str) -> Result<Option<String>> {
    let mut anchors = std::collections::HashSet::new();
    let mut flows = vec![];
    let mut scanner = Scanner::new(text.chars());
    for Token(mark, token) in &mut scanner {
        match token {
            // PyYAML's composer forgets anchors at each document start.
            TokenType::DocumentStart => anchors.clear(),
            TokenType::Anchor(name) if anchors.contains(&name) => {
                return Err("Duplicate YAML anchor".into());
            }
            TokenType::Anchor(name) => {
                anchors.insert(name);
            }
            TokenType::FlowSequenceStart | TokenType::FlowMappingStart => flows.push(mark),
            TokenType::FlowSequenceEnd | TokenType::FlowMappingEnd => {
                flows.pop();
            }
            _ => (),
        }
    }
    let (Some(error), Some(outer)) = (scanner.get_error(), flows.first()) else {
        return Ok(None);
    };
    if !error.info().contains("indentation") {
        return Ok(None);
    }
    let (col, target) = (error.marker().col(), outer.col() + 1);
    let Some(index) = error.marker().line().checked_sub(1) else {
        return Ok(None);
    };
    let lines = text.split('\n').collect::<Vec<_>>();
    // Only rightwards, and only a line whose first character after its leading
    // spaces is where the scan stopped.
    match lines.get(index) {
        Some(line) if col < target && line.len() - line.trim_start_matches(' ').len() == col => {}
        _ => return Ok(None),
    }
    let mut out = String::with_capacity(text.len() + target);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if i == index {
            out.push_str(&" ".repeat(target - col));
        }
        out.push_str(line);
    }
    Ok(Some(out))
}
/// Bytes that the rescans of one `parse` may read in all. Each moved line costs
/// one more scan of the whole text, so a text with very many of them would take
/// quadratic time; front matter (`markdown::FRONTMATTER_MAX_LINES` lines at
/// most) and manifests of ordinary size stay far below this.
const FLOW_REPAIR_READ_LIMIT: usize = 16 << 20;
/// `text` scanned again after each line that `scan_repairing_flow_indentation`
/// moves, until a scan moves none. A repaired line clears every flow indent, so
/// each line is moved at most once; past `limit` bytes read by the rescans, the
/// text is refused and the caller keeps its flat fallback.
fn repair_flow_indentation(text: &str, limit: usize) -> Result<std::borrow::Cow<'_, str>> {
    let mut text = std::borrow::Cow::Borrowed(text);
    let mut read = 0;
    while let Some(repaired) = scan_repairing_flow_indentation(&text)? {
        read += text.len();
        if read > limit {
            return Err("Too many YAML flow collection lines to move".into());
        }
        text = std::borrow::Cow::Owned(repaired);
    }
    Ok(text)
}
pub fn parse(text: &str) -> Result<Value> {
    let text = repair_flow_indentation(text, FLOW_REPAIR_READ_LIMIT)?;
    let text = text.as_ref();
    let mut builder = Builder::default();
    Parser::new_from_str(text)
        .load(&mut builder, true)
        .map_err(crate::Error::from)?;
    if let Some(error) = builder.error {
        return Err((error).into());
    }
    if builder.roots.len() > 1 {
        return Err("Expected one YAML document".into());
    }
    match builder.roots.first() {
        Some(&root) => expand(&builder.nodes, root, 0),
        None => Ok(Value::Null),
    }
}
/// A value as `yaml.safe_load_all` constructs it, for a structural walk
/// (`extractors::yaml_config`, Graphify 0.9.95): a mapping keeps its keys as
/// the Python objects PyYAML makes, in insertion order, with Python `dict`
/// semantics (keys equal under `==`, such as `1`, `1.0` and `true`, are one
/// entry that keeps its first spelling and its last value), so `1` and `"1"`
/// stay two keys. Merge keys (`<<`) are flattened as PyYAML's
/// `flatten_mapping` does.
#[derive(Clone, Debug)]
pub enum Loaded {
    /// A scalar, a `!!set` or anything else a walk does not enter.
    Other(Value),
    /// A list. The tuples of `!!omap`/`!!pairs` are items a walk never enters.
    Seq(Vec<Loaded>),
    /// A dict: `(key, value)` in insertion order.
    Map(Vec<(Value, Loaded)>),
}
fn expand_loaded(nodes: &[Node], id: usize, depth: usize) -> Result<Loaded> {
    if depth > 128 {
        return Err("Recursive or excessively nested YAML".into());
    }
    match &nodes[id] {
        Node::Alias(target) => expand_loaded(nodes, *target, depth + 1),
        Node::Sequence(children, kind) if kind == "seq" => Ok(Loaded::Seq(
            children
                .iter()
                .map(|&child| expand_loaded(nodes, child, depth + 1))
                .collect::<Result<_>>()?,
        )),
        Node::Sequence(children, _) => {
            // `!!omap`/`!!pairs`: validated as the single-pair mappings they
            // must be; each becomes a tuple.
            expand(nodes, id, depth)?;
            Ok(Loaded::Seq(vec![
                Loaded::Other(Value::Null);
                children.len()
            ]))
        }
        Node::Mapping(_, kind) if kind == "set" => Ok(Loaded::Other(expand(nodes, id, depth)?)),
        Node::Mapping(children, _) => {
            let mut merged: Vec<(Value, Loaded)> = Vec::new();
            let mut own: Vec<(Value, Loaded)> = Vec::new();
            for pair in children.chunks_exact(2) {
                if matches!(nodes[pair[0]], Node::Merge) {
                    match expand_loaded(nodes, pair[1], depth + 1)? {
                        Loaded::Map(entries) => merged.extend(entries),
                        Loaded::Seq(values) => {
                            // PyYAML merges a sequence's mappings last to first.
                            for value in values.into_iter().rev() {
                                let Loaded::Map(entries) = value else {
                                    return Err("Invalid YAML merge sequence".into());
                                };
                                merged.extend(entries);
                            }
                        }
                        Loaded::Other(_) => return Err("Invalid YAML merge".into()),
                    }
                } else {
                    let key = loaded_key(nodes, pair[0], depth + 1)?;
                    own.push((key, expand_loaded(nodes, pair[1], depth + 1)?));
                }
            }
            let mut entries: Vec<(Value, Loaded)> = Vec::new();
            let mut index: std::collections::BTreeMap<python_compat::ScalarKey, usize> =
                std::collections::BTreeMap::new();
            for (key, value) in merged.into_iter().chain(own) {
                // A NaN key equals nothing, itself included: always a new entry.
                let Some(scalar) = loaded_key_identity(&key)? else {
                    entries.push((key, value));
                    continue;
                };
                match index.get(&scalar) {
                    Some(&at) => entries[at].1 = value,
                    None => {
                        index.insert(scalar, entries.len());
                        entries.push((key, value));
                    }
                }
            }
            Ok(Loaded::Map(entries))
        }
        // Only its type matters to a walk: neither a string nor a collection.
        Node::NonFinite(_) => Ok(Loaded::Other(Value::Null)),
        Node::Scalar(_) | Node::Merge => Ok(Loaded::Other(expand(nodes, id, depth)?)),
    }
}
/// A mapping key of [`expand_loaded`]. A non-finite float key becomes the
/// text Python's `str()` gives it, tagged so that it stays apart from the
/// string of the same spelling ([`loaded_key_identity`]).
fn loaded_key(nodes: &[Node], id: usize, depth: usize) -> Result<Value> {
    match &nodes[id] {
        Node::Alias(target) if depth <= 128 => loaded_key(nodes, *target, depth + 1),
        Node::NonFinite(value) => {
            let mut key = Map::new();
            key.insert(NONFINITE_KEY.into(), json!(python_float_text(*value)));
            Ok(Value::Object(key))
        }
        _ => expand(nodes, id, depth),
    }
}
const NONFINITE_KEY: &str = "\u{0}float";
/// The dict identity of a [`loaded_key`]: `None` for a NaN key.
fn loaded_key_identity(key: &Value) -> Result<Option<python_compat::ScalarKey>> {
    if let Some(text) = nonfinite_key(key) {
        return Ok(match text {
            "inf" => Some(python_compat::ScalarKey::Float(f64::INFINITY.to_bits())),
            "-inf" => Some(python_compat::ScalarKey::Float(f64::NEG_INFINITY.to_bits())),
            _ => None,
        });
    }
    python_compat::scalar_key(key)
        .map(Some)
        .ok_or_else(|| "Unhashable YAML key".into())
}
/// `str(key)` of a non-finite float key made by [`loaded_key`].
pub fn nonfinite_key(key: &Value) -> Option<&str> {
    key.as_object()
        .filter(|map| map.len() == 1)
        .and_then(|map| map.get(NONFINITE_KEY))
        .and_then(Value::as_str)
}
/// `yaml.safe_load_all(text)`: every document of the stream, in order.
pub fn parse_all(text: &str) -> Result<Vec<Loaded>> {
    let text = repair_flow_indentation(text, FLOW_REPAIR_READ_LIMIT)?;
    let text = text.as_ref();
    let mut builder = Builder {
        nonfinite: true,
        ..Default::default()
    };
    Parser::new_from_str(text)
        .load(&mut builder, true)
        .map_err(crate::Error::from)?;
    if let Some(error) = builder.error {
        return Err((error).into());
    }
    builder
        .roots
        .iter()
        .map(|&root| expand_loaded(&builder.nodes, root, 0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{parse, repair_flow_indentation};
    use serde_json::json;

    // gohugoio/hugo 3fff6fb5, docs/content/en/templates/types.md: the closing
    // bracket of a flow sequence at the key's column. Expected values are
    // PyYAML 6.0.3's yaml.safe_load, which Graphify 0.9.91 uses.
    const HUGO: [&str; 16] = [
        "title: Template types",
        "description: Create templates of different types to render your content, resources, and data.",
        "categories: []",
        "keywords: []",
        "weight: 30",
        "aliases: [",
        "  '/templates/base/',",
        "  '/templates/content-view/',",
        "  '/templates/home/',",
        "  '/templates/lists/',",
        "  '/templates/partial/',",
        "  '/templates/section/',",
        "  '/templates/single/',",
        "  '/templates/taxonomy/',",
        "  '/templates/term/',",
        "]",
    ];

    #[test]
    fn flow_collection_lines_at_the_block_indent_parse_as_pyyaml_does() {
        assert_eq!(
            parse(&HUGO.join("\n")).unwrap(),
            json!({"title": "Template types",
                "description": "Create templates of different types to render your content, resources, and data.",
                "categories": [], "keywords": [], "weight": 30,
                "aliases": ["/templates/base/", "/templates/content-view/", "/templates/home/",
                    "/templates/lists/", "/templates/partial/", "/templates/section/",
                    "/templates/single/", "/templates/taxonomy/", "/templates/term/"]})
        );
        for (text, expected) in [
            ("- [\n  a,\n]", json!([["a"]])),
            ("a:\n  b: [\n  x,\n  ]", json!({"a": {"b": ["x"]}})),
            ("m: {\n  k: 1,\n}", json!({"m": {"k": 1}})),
            (
                "tags: [\ngo,\nyaml\n]\nweight: 5",
                json!({"tags": ["go", "yaml"], "weight": 5}),
            ),
            ("a: [[\n1,\n],\n2]", json!({"a": [[1], 2]})),
            (
                "a: {\n'k': 'v',\n'l': [\n'w'\n]\n}",
                json!({"a": {"k": "v", "l": ["w"]}}),
            ),
            ("- key: [\n'v',\n]", json!([{"key": ["v"]}])),
            ("a: &x [\n'1'\n]\nb: *x", json!({"a": ["1"], "b": ["1"]})),
            ("a: [\r\nb\r\n]", json!({"a": ["b"]})),
        ] {
            assert_eq!(parse(text).unwrap(), expected, "{text:?}");
        }
    }

    #[test]
    fn other_scan_errors_still_reach_the_fallback() {
        // PyYAML rejects both; the caller keeps its flat fallback for them.
        assert!(parse("a: [\n  x").is_err());
        assert!(parse("a:\n  b: 1\n c: 2").is_err());
        // The text is scanned again after a moved line, so a duplicate anchor
        // after it is still refused, as PyYAML refuses it.
        assert!(parse("a: &x [\n'1'\n]\nb: &x 2").is_err());
    }

    #[test]
    fn rescans_stop_at_their_read_limit() {
        // 101 moved lines: each entry and the closing bracket.
        let text = format!("a: [\n{}]", "'x',\n".repeat(100));
        assert_eq!(
            parse(&text).unwrap()["a"].as_array().map(Vec::len),
            Some(100)
        );
        assert!(repair_flow_indentation(&text, 10 * text.len()).is_err());
    }

    #[test]
    fn hugo_front_matter_keeps_its_types_on_the_page_node() {
        let fm = crate::extractors::markdown::parse_frontmatter(&HUGO);
        assert_eq!(fm["weight"], json!(30));
        assert_eq!(fm["categories"], json!([]));
        assert_eq!(fm["aliases"].as_array().map(Vec::len), Some(9));
    }
}
