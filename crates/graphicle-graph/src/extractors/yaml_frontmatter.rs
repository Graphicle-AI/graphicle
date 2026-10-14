// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/markdown.py
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
    scanner::{Scanner, TScalarStyle, TokenType},
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
                        let value = scalar(&text, &kind, implicit)?;
                        self.add(Node::Scalar(value), anchor);
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
                digits.parse::<f64>().map_err(|_| "Non-JSON YAML float")?
            };
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| "Non-JSON YAML float".into())
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
pub fn parse(text: &str) -> Result<Value> {
    let mut anchors = std::collections::HashSet::new();
    for token in Scanner::new(text.chars()) {
        if let TokenType::Anchor(name) = token.1
            && !anchors.insert(name)
        {
            return Err("Duplicate YAML anchor".into());
        }
    }
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
