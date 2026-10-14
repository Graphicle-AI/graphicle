// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/pascal_resolution.py
//! Native translation of graphify/pascal_resolution.py.
use crate::{
    Result,
    python_compat::{ScalarKey, scalar_key, string},
    resolver_registry::LanguageResolver,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};
pub const PASCAL_SUFFIXES: &[&str] = &[".pas", ".pp", ".dpr", ".dpk", ".inc"];
fn key(value: &Value) -> Result<ScalarKey> {
    scalar_key(value).ok_or_else(|| "unhashable identifier".into())
}
fn text(node: &Value, field: &str) -> String {
    node.get(field).map(string).unwrap_or_default()
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
pub fn pascal_raw_calls(per_file: &[Value]) -> Result<Vec<&Value>> {
    let mut calls = vec![];
    for result in per_file.iter().filter(|v| v.is_object()) {
        let Some(value) = result.get("raw_calls") else {
            continue;
        };
        let Some(raw) = value.as_array() else {
            if value.is_string() || value.is_object() {
                continue;
            }
            return Err("raw_calls is not iterable".into());
        };
        calls.extend(raw.iter().filter(|v| {
            v.is_object()
                && PASCAL_SUFFIXES
                    .iter()
                    .any(|suffix| text(v, "source_file").ends_with(suffix))
        }));
    }
    Ok(calls)
}
pub fn resolve_pascal_inherited_calls(
    per_file: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
) -> Result<()> {
    let by_id: BTreeMap<_, _> = nodes
        .iter()
        .map(|n| Ok((key(&n["id"])?, n)))
        .collect::<Result<_>>()?;
    let mut bases: BTreeMap<ScalarKey, Vec<Value>> = BTreeMap::new();
    let mut owners = BTreeMap::new();
    let mut methods: BTreeMap<ScalarKey, BTreeMap<String, Vec<Value>>> = BTreeMap::new();
    for edge in edges.iter() {
        if edge["relation"] == "inherits" {
            let source = edge
                .get("source")
                .ok_or("inherits edge is missing source")?;
            let target = edge
                .get("target")
                .ok_or("inherits edge is missing target")?;
            bases.entry(key(source)?).or_default().push(target.clone());
        } else if edge["relation"] == "method" {
            let owner = key(&edge["source"])?;
            let method = key(&edge["target"])?;
            owners.insert(method.clone(), edge["source"].clone());
            let Some(node) = by_id.get(&method) else {
                continue;
            };
            let label = text(node, "label");
            let name = label.strip_suffix("()").unwrap_or(&label).to_lowercase();
            let bucket = methods.entry(owner).or_default().entry(name).or_default();
            if !bucket
                .iter()
                .any(|v| scalar_key(v).as_ref() == Some(&method))
            {
                bucket.push(edge["target"].clone());
            }
        }
    }
    let mut existing: BTreeSet<_> = edges
        .iter()
        .map(|e| Ok((key(&e["source"])?, key(&e["target"])?)))
        .collect::<Result<_>>()?;
    for call in pascal_raw_calls(per_file)? {
        let caller = &call["caller_nid"];
        let name = &call["callee"];
        if !truthy(caller) || !truthy(name) {
            continue;
        }
        let Some(owner) = owners.get(&key(caller)?).filter(|v| truthy(v)) else {
            continue;
        };
        let owner = ScalarKey::Text(string(owner));
        let name = string(name);
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<_> = bases.get(&owner).cloned().unwrap_or_default().into();
        let mut target = None;
        while let Some(base) = queue.pop_front() {
            let base = key(&base)?;
            if !seen.insert(base.clone()) {
                continue;
            }
            if let Some(candidates) = methods
                .get(&base)
                .and_then(|m| m.get(&name))
                .filter(|v| !v.is_empty())
            {
                if candidates.len() == 1 {
                    target = Some(candidates[0].clone());
                }
                break;
            }
            queue.extend(bases.get(&base).into_iter().flatten().cloned());
        }
        let Some(target) = target.filter(truthy) else {
            continue;
        };
        let pair = (key(caller)?, key(&target)?);
        if pair.0 == pair.1 || !existing.insert(pair) {
            continue;
        }
        edges.push(json!({"source":caller,"target":target,"relation":"calls","context":"call","confidence":"EXTRACTED","confidence_score":1.0,
            "source_file":call.get("source_file").cloned().unwrap_or(json!("")),"source_location":call["source_location"],"weight":1.0}));
    }
    Ok(())
}
pub fn resolver() -> LanguageResolver {
    LanguageResolver {
        name: "pascal_inherited_calls".into(),
        suffixes: PASCAL_SUFFIXES.iter().map(|s| s.to_string()).collect(),
        resolve: Arc::new(|p, n, e| resolve_pascal_inherited_calls(p, n, e)),
    }
}
pub fn dispatch(request: &Value) -> Result<Value> {
    let per_file = request["per_file"]
        .as_array()
        .ok_or("Expected per-file results")?;
    if request["operation"] == "pascal_resolution.raw_calls" {
        return Ok(json!(pascal_raw_calls(per_file)?));
    }
    let nodes = request["nodes"].as_array().ok_or("Expected nodes")?;
    let mut edges = request["edges"].as_array().ok_or("Expected edges")?.clone();
    resolve_pascal_inherited_calls(per_file, nodes, &mut edges)?;
    Ok(json!({"nodes":nodes,"edges":edges}))
}
