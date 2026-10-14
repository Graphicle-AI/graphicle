// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/interface_dispatch.py
//! Single-implementer interface dispatch, shared by the C# and Swift passes.
//!
//! A call through an injected dependency lands on the interface's member, so
//! the implementation sits unreachable from the call site. When an interface
//! has exactly one implementer, each uniquely named member is linked to the
//! implementer's same-named member. `suffixes` is the language gate: both ends
//! must be declared in a file with one of these extensions.
use crate::{
    Result,
    python_compat::{ScalarKey, scalar_key, string},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub const DISPATCH_RELATION: &str = "dispatches_to";
fn key(value: &Value) -> Result<ScalarKey> {
    scalar_key(value).ok_or_else(|| "unhashable identifier".into())
}
fn text(node: &Value, field: &str) -> String {
    node.get(field).map(string).unwrap_or_default()
}
/// Whether `node` is declared in a file with one of `suffixes`, by its
/// effective suffix, so a `.graphiclerc` language declaration counts (#2961).
pub fn declared_in(node: Option<&Value>, suffixes: &[&str]) -> bool {
    node.and_then(|n| n.get("source_file"))
        .filter(|v| crate::build_normalize::truthy(v))
        .is_some_and(|v| suffixes.contains(&crate::rcfile::effective_suffix(string(v)).as_str()))
}
pub fn method_label(node: &Value) -> String {
    let text = text(node, "label");
    let text = text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    text.strip_prefix('.')
        .unwrap_or(text)
        .split('(')
        .next()
        .unwrap_or("")
        .into()
}
fn insert_unique(values: &mut Vec<Value>, value: &Value) -> Result<()> {
    let id = key(value)?;
    if !values.iter().any(|v| scalar_key(v).as_ref() == Some(&id)) {
        values.push(value.clone());
    }
    Ok(())
}
pub fn resolve_interface_dispatch(
    per_file: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
    suffixes: &[&str],
) -> Result<()> {
    let present = per_file
        .iter()
        .filter(|v| v.is_object())
        .chain(nodes)
        .any(|n| declared_in(Some(n), suffixes));
    if !present {
        return Ok(());
    }
    let by_id: BTreeMap<_, _> = nodes
        .iter()
        .map(|n| Ok((key(&n["id"])?, n)))
        .collect::<Result<_>>()?;
    let mut implementers: Vec<(ScalarKey, Vec<Value>)> = vec![];
    let mut methods: BTreeMap<ScalarKey, Vec<(String, Vec<Value>)>> = BTreeMap::new();
    for edge in edges.iter() {
        if edge["relation"] == "implements" {
            let target = key(&edge["target"])?;
            let index = implementers
                .iter()
                .position(|(id, _)| id == &target)
                .unwrap_or_else(|| {
                    implementers.push((target, vec![]));
                    implementers.len() - 1
                });
            insert_unique(&mut implementers[index].1, &edge["source"])?;
        } else if edge["relation"] == "method" {
            let owner = key(&edge["source"])?;
            let method = key(&edge["target"])?;
            let Some(node) = by_id
                .get(&method)
                .filter(|n| declared_in(Some(n), suffixes))
            else {
                continue;
            };
            let name = method_label(node);
            if name.is_empty() {
                continue;
            }
            let names = methods.entry(owner).or_default();
            let index = names
                .iter()
                .position(|(s, _)| s == &name)
                .unwrap_or_else(|| {
                    names.push((name, vec![]));
                    names.len() - 1
                });
            insert_unique(&mut names[index].1, &edge["target"])?;
        }
    }
    let mut pairs: BTreeSet<_> = edges
        .iter()
        .filter(|e| e["relation"] == DISPATCH_RELATION)
        .map(|e| Ok((key(&e["source"])?, key(&e["target"])?)))
        .collect::<Result<_>>()?;
    for (interface, impls) in implementers {
        if impls.len() != 1 {
            continue;
        }
        let implementation = key(&impls[0])?;
        let (Some(interface_node), Some(impl_node)) =
            (by_id.get(&interface), by_id.get(&implementation))
        else {
            continue;
        };
        if !declared_in(Some(interface_node), suffixes) || !declared_in(Some(impl_node), suffixes) {
            continue;
        }
        let Some(declared_names) = methods.get(&interface) else {
            continue;
        };
        for (name, declared) in declared_names {
            if declared.len() != 1 {
                continue;
            }
            let Some(candidates) = methods
                .get(&implementation)
                .and_then(|names| names.iter().find(|(n, _)| n == name))
                .map(|(_, v)| v)
                .filter(|v| v.len() == 1)
            else {
                continue;
            };
            let source = &declared[0];
            let target = &candidates[0];
            let pair = (key(source)?, key(target)?);
            if pair.0 == pair.1 || !pairs.insert(pair) {
                continue;
            }
            edges.push(json!({"source":source,"target":target,"relation":DISPATCH_RELATION,"context":"call","confidence":"INFERRED","confidence_score":0.85,
                "source_file":text(impl_node,"source_file"),"source_location":impl_node["source_location"],"weight":1.0}));
        }
    }
    Ok(())
}
fn suffixes(request: &Value) -> Result<Vec<String>> {
    request["suffixes"]
        .as_array()
        .ok_or("Expected suffixes")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "Expected string suffixes".into())
        })
        .collect()
}
pub(crate) fn resolve_request(
    request: &Value,
    resolve: impl FnOnce(&[Value], &[Value], &mut Vec<Value>) -> Result<()>,
) -> Result<Value> {
    let per_file = request["per_file"]
        .as_array()
        .ok_or("Expected per-file results")?;
    let nodes = request["nodes"].as_array().ok_or("Expected nodes")?;
    let mut edges = request["edges"].as_array().ok_or("Expected edges")?.clone();
    resolve(per_file, nodes, &mut edges)?;
    Ok(json!({"nodes":nodes,"edges":edges}))
}
pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "interface_dispatch.declared_in" => {
            let owned = suffixes(request)?;
            let gate: Vec<&str> = owned.iter().map(String::as_str).collect();
            let node = Some(&request["node"]).filter(|n| !n.is_null());
            Ok(json!(declared_in(node, &gate)))
        }
        "interface_dispatch.method_label" => Ok(json!(method_label(&request["node"]))),
        "interface_dispatch.resolve" => {
            let owned = suffixes(request)?;
            let gate: Vec<&str> = owned.iter().map(String::as_str).collect();
            resolve_request(request, |per_file, nodes, edges| {
                resolve_interface_dispatch(per_file, nodes, edges, &gate)
            })
        }
        _ => Err("Unknown interface dispatch operation".into()),
    }
}
