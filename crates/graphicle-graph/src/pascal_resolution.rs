// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/pascal_resolution.py
//! Native translation of graphify/pascal_resolution.py.
//!
//! The pass reads the edges and the per-file raw calls in whichever form
//! assembly holds them ([`Rec`], [`PerFile`]).
use crate::{
    Result,
    extract::per_file::PerFile,
    python_compat::{ScalarKey, scalar_key, string},
    resolver_registry::{LanguageResolver, Rec, append_edges, at, edge_record},
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
    pascal_raw_calls_any(per_file)
}
/// The raw calls of Pascal sources, in result order, from the per-file
/// results in either form (see `ruby_resolution::ruby_raw_calls_any`).
pub fn pascal_raw_calls_any<P: PerFile>(per_file: &[P]) -> Result<Vec<&P::R>> {
    let mut calls = vec![];
    for result in per_file {
        let Some(raw) = result.raw_calls() else {
            match result.raw_calls_other() {
                Some(value) if !value.is_string() && !value.is_object() => {
                    return Err("raw_calls is not iterable".into());
                }
                _ => continue,
            }
        };
        calls.extend(raw.iter().filter(|v| {
            let source = v.rec_get("source_file").map(string).unwrap_or_default();
            PASCAL_SUFFIXES
                .iter()
                .any(|suffix| source.ends_with(suffix))
        }));
    }
    Ok(calls)
}
pub fn resolve_pascal_inherited_calls(
    per_file: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
) -> Result<()> {
    resolve_pascal_inherited_calls_any(per_file, nodes, edges)
}
/// [`resolve_pascal_inherited_calls`] over the per-file results and the
/// edges in either form. New edges are appended after the pass, which reads
/// none of them; when it stops at an error, the edges it added before stay,
/// as in Python.
pub fn resolve_pascal_inherited_calls_any<P: PerFile, E: Rec>(
    per_file: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) -> Result<()> {
    let mut added = vec![];
    let outcome = inherited_calls(per_file, nodes, edges, &mut added);
    append_edges(edges, added);
    outcome
}
/// The pass over `edges`, collecting its new edges in `added`.
fn inherited_calls<P: PerFile, E: Rec>(
    per_file: &[P],
    nodes: &[Value],
    edges: &[E],
    added: &mut Vec<E>,
) -> Result<()> {
    let by_id: BTreeMap<_, _> = nodes
        .iter()
        .map(|n| Ok((key(&n["id"])?, n)))
        .collect::<Result<_>>()?;
    let mut bases: BTreeMap<ScalarKey, Vec<Value>> = BTreeMap::new();
    let mut owners = BTreeMap::new();
    let mut methods: BTreeMap<ScalarKey, BTreeMap<String, Vec<Value>>> = BTreeMap::new();
    for edge in edges.iter() {
        if at(edge, "relation") == "inherits" {
            let source = edge
                .rec_get("source")
                .ok_or("inherits edge is missing source")?;
            let target = edge
                .rec_get("target")
                .ok_or("inherits edge is missing target")?;
            bases.entry(key(source)?).or_default().push(target.clone());
        } else if at(edge, "relation") == "method" {
            let owner = key(at(edge, "source"))?;
            let method = key(at(edge, "target"))?;
            owners.insert(method.clone(), at(edge, "source").clone());
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
                bucket.push(at(edge, "target").clone());
            }
        }
    }
    let mut existing: BTreeSet<_> = edges
        .iter()
        .map(|e| Ok((key(at(e, "source"))?, key(at(e, "target"))?)))
        .collect::<Result<_>>()?;
    for call in pascal_raw_calls_any(per_file)? {
        let caller = at(call, "caller_nid");
        let name = at(call, "callee");
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
        added.push(edge_record(json!({"source":caller,"target":target,"relation":"calls","context":"call","confidence":"EXTRACTED","confidence_score":1.0,
            "source_file":call.rec_get("source_file").cloned().unwrap_or(json!("")),"source_location":at(call, "source_location"),"weight":1.0})));
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
