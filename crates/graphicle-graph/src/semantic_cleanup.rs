// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/semantic_cleanup.py
//! Native translation of graphify/semantic_cleanup.py.
use crate::{
    Result, hyperedge_members,
    python_compat::{ScalarKey, scalar_key},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    path::Path,
    sync::LazyLock,
};

pub const RATIONALE_MIN_CHARS: usize = 80;
pub const RATIONALE_MIN_WORDS: usize = 8;
pub const MAX_SEMANTIC_FRAGMENT_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_SEMANTIC_FRAGMENT_NODES: usize = 10_000;
pub const MAX_SEMANTIC_FRAGMENT_EDGES: usize = 100_000;
pub const MAX_SEMANTIC_FRAGMENT_HYPEREDGES: usize = 10_000;
pub const MAX_SEMANTIC_HYPEREDGE_NODES: usize = 256;
pub const MAX_SEMANTIC_ID_LENGTH: usize = 256;
pub const VALID_SEMANTIC_FILE_TYPES: &[&str] =
    &["code", "document", "paper", "image", "rationale", "concept"];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    pub bytes: usize,
    pub nodes: usize,
    pub edges: usize,
    pub hyperedges: usize,
    pub members: usize,
    pub id: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: MAX_SEMANTIC_FRAGMENT_BYTES,
            nodes: MAX_SEMANTIC_FRAGMENT_NODES,
            edges: MAX_SEMANTIC_FRAGMENT_EDGES,
            hyperedges: MAX_SEMANTIC_FRAGMENT_HYPEREDGES,
            members: MAX_SEMANTIC_HYPEREDGE_NODES,
            id: MAX_SEMANTIC_ID_LENGTH,
        }
    }
}
#[derive(Default, Debug, Serialize)]
pub struct Validation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}
struct Count(usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
static ID_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::regex_compat::compile(r"^[\w.:-]+\z"));

pub fn validate_id(errors: &mut Vec<String>, field: &str, value: &Value, max: usize) {
    let Some(value) = value.as_str() else {
        errors.push(format!("{field} must be a string"));
        return;
    };
    if value.is_empty() {
        errors.push(format!("{field} must not be empty"));
        return;
    }
    let n = value.chars().count();
    if n > max {
        errors.push(format!("{field} is {n} chars; max is {max}"));
    }
    if value.contains('/') || value.contains('\\') || value.contains("..") {
        errors.push(format!("{field} must not contain path separators or '..'"));
    }
    if !ID_RE.is_match(value) {
        errors.push(format!("{field} contains unsupported characters"));
    }
}
pub fn validate_semantic_fragment(fragment: &mut Value) -> Validation {
    validate_semantic_fragment_with(fragment, &Limits::default())
}
pub fn validate_semantic_fragment_with(fragment: &mut Value, limits: &Limits) -> Validation {
    let mut report = Validation::default();
    if !fragment.is_object() {
        report.errors.push("fragment must be a JSON object".into());
        return report;
    }
    let mut size = Count(0);
    if let Err(error) = crate::python_json::write(&mut size, fragment, None, false) {
        report
            .errors
            .push(format!("fragment is not JSON-serializable: {error}"));
        return report;
    }
    if size.0 > limits.bytes {
        report.errors.push(format!(
            "payload is {} bytes; max is {}",
            size.0, limits.bytes
        ));
    }
    for (field, max) in [("nodes", limits.nodes), ("edges", limits.edges)] {
        if let Some(value) = fragment.get(field) {
            match value.as_array() {
                None => report.errors.push(format!("{field} must be a list")),
                Some(rows) if rows.len() > max => report
                    .errors
                    .push(format!("{field} has {} entries; max is {max}", rows.len())),
                _ => (),
            }
        }
    }
    for field in ["nodes", "edges"] {
        for (i, row) in fragment
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if !row.is_object() {
                report
                    .errors
                    .push(format!("{field}[{i}] must be an object"));
                continue;
            }
            let keys: &[&str] = if field == "nodes" {
                &["id"]
            } else {
                &["source", "target"]
            };
            for key in keys {
                validate_id(
                    &mut report.errors,
                    &format!("{field}[{i}].{key}"),
                    &row[*key],
                    limits.id,
                );
            }
        }
    }
    if let Some(value) = fragment.get_mut("hyperedges").filter(|v| !v.is_null()) {
        if let Some(rows) = value.as_array_mut() {
            if rows.len() > limits.hyperedges {
                report.errors.push(format!(
                    "hyperedges has {} entries; max is {}",
                    rows.len(),
                    limits.hyperedges
                ));
            }
            for (i, he) in rows.iter_mut().enumerate() {
                if !he.is_object() {
                    report
                        .errors
                        .push(format!("hyperedges[{i}] must be an object"));
                    continue;
                }
                report.warnings.extend(hyperedge_members::normalize(he));
                validate_id(
                    &mut report.errors,
                    &format!("hyperedges[{i}].id"),
                    &he["id"],
                    limits.id,
                );
                let Some(members) = he["nodes"].as_array() else {
                    report
                        .errors
                        .push(format!("hyperedges[{i}].nodes must be a list"));
                    continue;
                };
                if members.len() > limits.members {
                    report.errors.push(format!(
                        "hyperedges[{i}].nodes has {} entries; max is {}",
                        members.len(),
                        limits.members
                    ));
                }
                for (j, member) in members.iter().enumerate() {
                    validate_id(
                        &mut report.errors,
                        &format!("hyperedges[{i}].nodes[{j}]"),
                        member,
                        limits.id,
                    );
                }
            }
        } else {
            report.errors.push("hyperedges must be a list".into());
        }
    }
    report
}
pub fn load_validated_semantic_fragment(
    path: &Path,
    limits: &Limits,
) -> Result<(Option<Value>, Validation)> {
    let error_result = |message| {
        Ok((
            None,
            Validation {
                errors: vec![message],
                warnings: vec![],
            },
        ))
    };
    let size = match path.metadata() {
        Ok(m) => m.len(),
        Err(e) => return error_result(format!("could not stat {}: {e}", path.display())),
    };
    if size > limits.bytes as u64 {
        return error_result(format!("payload is {size} bytes; max is {}", limits.bytes));
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return error_result(format!("could not read {}: {e}", path.display())),
    };
    let text = std::str::from_utf8(&bytes).map_err(|e| format!("invalid UTF-8: {e}"))?;
    let mut fragment = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => return error_result(format!("invalid JSON: {e}")),
    };
    let validation = validate_semantic_fragment_with(&mut fragment, limits);
    Ok((validation.errors.is_empty().then_some(fragment), validation))
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
fn py_space(c: char) -> bool {
    matches!(c, '\t'..='\r' | '\x1c'..='\x20' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}
pub fn is_sentence_like_rationale_label(label: &Value) -> Result<bool> {
    if !truthy(label) {
        return Ok(false);
    }
    let label = label
        .as_str()
        .ok_or("rationale label must be a string")?
        .trim_matches(py_space);
    if label.chars().count() < RATIONALE_MIN_CHARS
        && label.split(py_space).filter(|w| !w.is_empty()).count() < RATIONALE_MIN_WORDS
    {
        return Ok(false);
    }
    Ok(label.contains(['.', '!', '?', ':']))
}
pub fn append_rationale_attr(node: &mut Value, texts: &[String]) -> Result<()> {
    let existing = node.get("rationale").unwrap_or(&Value::Null);
    let text = texts.join("\n\n");
    let text = text.trim_matches(py_space);
    let value = if truthy(existing) {
        format!(
            "{}\n\n{text}",
            existing
                .as_str()
                .ok_or("existing rationale must be a string")?
        )
    } else {
        text.to_string()
    };
    node.as_object_mut()
        .ok_or("rationale target must be an object")?
        .insert("rationale".into(), value.into());
    Ok(())
}
fn id(row: &Value, key: &str) -> Result<ScalarKey> {
    match row.get(key) {
        Some(value) => scalar_key(value).ok_or_else(|| "unhashable identifier".into()),
        None => Ok(ScalarKey::Text(String::new())),
    }
}
fn rows(fragment: &Value, key: &str) -> Result<Vec<Value>> {
    match fragment.get(key) {
        None => Ok(vec![]),
        Some(value) => Ok(value
            .as_array()
            .cloned()
            .ok_or_else(|| format!("{key} must be a list"))?),
    }
}
pub fn sanitize_semantic_fragment(fragment: &mut Value) -> Result<Vec<String>> {
    if !fragment.is_object() {
        return Err("fragment must be an object".into());
    }
    let mut nodes = rows(fragment, "nodes")?;
    let edges = rows(fragment, "edges")?;
    let hyperedges = if fragment.get("hyperedges").is_some_and(truthy) {
        rows(fragment, "hyperedges")?
    } else {
        vec![]
    };
    let mut node_by_id = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        if !node.is_object() {
            return Err("node must be an object".into());
        }
        if truthy(&node["id"]) {
            node_by_id.insert(id(node, "id")?, i);
        }
    }
    let mut sources = BTreeSet::new();
    for edge in &edges {
        if !edge.is_object() {
            return Err("edge must be an object".into());
        }
        if edge["relation"] == "rationale_for" && truthy(&edge["source"]) {
            sources.insert(id(edge, "source")?);
        }
    }
    let mut candidates = vec![];
    let mut removed = BTreeSet::new();
    let mut kept = vec![];
    for (i, node) in nodes.iter().enumerate() {
        if !truthy(&node["id"]) {
            continue;
        }
        let nid = id(node, "id")?;
        if matches!(node["file_type"].as_str(), Some("rationale" | "concept")) {
            if is_sentence_like_rationale_label(&node["label"])? {
                candidates.push(i);
            }
            removed.insert(nid);
        } else if sources.contains(&nid) && is_sentence_like_rationale_label(&node["label"])? {
            candidates.push(i);
            removed.insert(nid);
        } else {
            kept.push(i);
        }
    }
    let mut attrs: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for i in candidates {
        let source = id(&nodes[i], "id")?;
        let text = nodes[i]["label"].as_str().unwrap().trim_matches(py_space);
        for edge in &edges {
            if edge["relation"] != "rationale_for"
                || scalar_key(&edge["source"]).as_ref() != Some(&source)
            {
                continue;
            }
            let target = scalar_key(&edge["target"]).ok_or("unhashable identifier")?;
            if let Some(&index) = node_by_id
                .get(&target)
                .filter(|_| !removed.contains(&target))
            {
                attrs.entry(index).or_default().push(text.into());
            }
        }
    }
    for (index, texts) in attrs {
        append_rationale_attr(&mut nodes[index], &texts)?;
    }
    let kept_nodes: Vec<_> = kept.into_iter().map(|i| nodes[i].clone()).collect();
    let mut kept_edges = vec![];
    for edge in edges {
        if !removed.contains(&id(&edge, "source")?) && !removed.contains(&id(&edge, "target")?) {
            kept_edges.push(edge);
        }
    }
    let surviving: BTreeSet<_> = kept_nodes
        .iter()
        .map(|n| id(n, "id"))
        .collect::<Result<_>>()?;
    let mut kept_hyperedges = vec![];
    let mut warnings = vec![];
    for mut he in hyperedges {
        if !he.is_object() {
            continue;
        }
        warnings.extend(hyperedge_members::normalize(&mut he));
        let Some(members) = he["nodes"].as_array() else {
            continue;
        };
        let filtered: Vec<_> = members
            .iter()
            .filter(|v| v.is_string() && scalar_key(v).is_some_and(|k| surviving.contains(&k)))
            .cloned()
            .collect();
        if filtered.len() < 2 {
            continue;
        }
        he["nodes"] = filtered.into();
        kept_hyperedges.push(he);
    }
    fragment["nodes"] = kept_nodes.into();
    fragment["edges"] = kept_edges.into();
    fragment["hyperedges"] = kept_hyperedges.into();
    Ok(warnings)
}
pub fn dispatch(request: &Value) -> Result<Value> {
    let limits = request
        .get("limits")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(crate::Error::from)?
        .unwrap_or_default();
    let mut fragment = request["fragment"].clone();
    match request["operation"].as_str().unwrap_or("") {
        "semantic.validate" => {
            let result = validate_semantic_fragment_with(&mut fragment, &limits);
            Ok(json!({"fragment": fragment, "errors": result.errors, "warnings": result.warnings}))
        }
        "semantic.load" => {
            let (fragment, result) = load_validated_semantic_fragment(
                Path::new(request["path"].as_str().ok_or("Expected path")?),
                &limits,
            )?;
            Ok(json!({"fragment": fragment, "errors": result.errors, "warnings": result.warnings}))
        }
        "semantic.sanitize" => {
            let warnings = sanitize_semantic_fragment(&mut fragment)?;
            Ok(json!({"fragment": fragment, "warnings": warnings}))
        }
        "semantic.sentence" => Ok(json!(is_sentence_like_rationale_label(&request["label"])?)),
        "semantic.append" => {
            let mut node = request["node"].clone();
            let texts = serde_json::from_value::<Vec<String>>(request["texts"].clone())
                .map_err(crate::Error::from)?;
            append_rationale_attr(&mut node, &texts)?;
            Ok(node)
        }
        "semantic.normalize_members" => {
            let mut he = request["hyperedge"].clone();
            let warnings = hyperedge_members::normalize(&mut he);
            Ok(json!({"hyperedge": he, "warnings": warnings}))
        }
        _ => Err("Unknown semantic cleanup operation".into()),
    }
}
