// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/interface_dispatch.py
//! Interface dispatch to every implementer, shared by the C# and Swift passes.
//!
//! A call through an injected dependency lands on the interface's member, so
//! the implementation sits unreachable from the call site. Each uniquely named
//! member of an interface is linked to the same-named member of every
//! implementer that owns exactly one member of that name (#3800); an
//! implementer that overloads the name is skipped on its own. `suffixes` is
//! the language gate: the interface and each implementer must be declared in a
//! file with one of these extensions, and implementers that fail it do not
//! count toward the confidence (#4396).
//!
//! The pass reads the edges and the per-file results in whichever form
//! assembly holds them ([`Rec`], [`PerFile`]).
use crate::{
    Result,
    extract::per_file::PerFile,
    python_compat::{ScalarKey, scalar_key, string},
    resolver_registry::{Rec, append_edges, at, edge_record},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub const DISPATCH_RELATION: &str = "dispatches_to";
/// INFERRED confidence for a dispatch edge, given how many gated implementers
/// exist: 0.85 for one (the target is forced), 0.75 for two, and a 0.65 floor
/// from three on, the extraction rubric's own tiers.
pub fn dispatch_confidence(implementer_count: usize) -> f64 {
    match implementer_count {
        1 => 0.85,
        2 => 0.75,
        _ => 0.65,
    }
}
fn key(value: &Value) -> Result<ScalarKey> {
    scalar_key(value).ok_or_else(|| "unhashable identifier".into())
}
fn text(node: &Value, field: &str) -> String {
    node.get(field).map(string).unwrap_or_default()
}
/// Whether `node` is declared in a file with one of `suffixes`, by its
/// effective suffix, so a `.graphiclerc` language declaration counts (#2961).
pub fn declared_in(node: Option<&Value>, suffixes: &[&str]) -> bool {
    declared_at(node.and_then(|n| n.get("source_file")), suffixes)
}
/// [`declared_in`] for the `source_file` value itself.
fn declared_at(source_file: Option<&Value>, suffixes: &[&str]) -> bool {
    source_file
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
    resolve_interface_dispatch_any(per_file, nodes, edges, suffixes)
}
/// [`resolve_interface_dispatch`] over the per-file results and the edges in
/// either form. New edges are appended once after the pass, which reads none
/// of them; when it stops at an error it adds none, as Python 0.9.91 extends
/// the edge list only at the end.
pub fn resolve_interface_dispatch_any<P: PerFile, E: Rec>(
    per_file: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
    suffixes: &[&str],
) -> Result<()> {
    let mut added = vec![];
    dispatch_edges(per_file, nodes, edges, suffixes, &mut added)?;
    append_edges(edges, added);
    Ok(())
}
/// The pass over `edges`, collecting its new edges in `added`.
fn dispatch_edges<P: PerFile, E: Rec>(
    per_file: &[P],
    nodes: &[Value],
    edges: &[E],
    suffixes: &[&str],
    added: &mut Vec<E>,
) -> Result<()> {
    // A result that is no object has no `source_file`.
    let present = per_file
        .iter()
        .any(|r| declared_at(r.key("source_file"), suffixes))
        || nodes.iter().any(|n| declared_in(Some(n), suffixes));
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
        if at(edge, "relation") == "implements" {
            let target = key(at(edge, "target"))?;
            let index = implementers
                .iter()
                .position(|(id, _)| id == &target)
                .unwrap_or_else(|| {
                    implementers.push((target, vec![]));
                    implementers.len() - 1
                });
            insert_unique(&mut implementers[index].1, at(edge, "source"))?;
        } else if at(edge, "relation") == "method" {
            let owner = key(at(edge, "source"))?;
            let method = key(at(edge, "target"))?;
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
            insert_unique(&mut names[index].1, at(edge, "target"))?;
        }
    }
    let mut pairs: BTreeSet<_> = edges
        .iter()
        .filter(|e| at(*e, "relation") == DISPATCH_RELATION)
        .map(|e| Ok((key(at(e, "source"))?, key(at(e, "target"))?)))
        .collect::<Result<_>>()?;
    for (interface, impls) in implementers {
        let Some(interface_node) = by_id.get(&interface) else {
            continue;
        };
        if !declared_in(Some(interface_node), suffixes) {
            continue;
        }
        let Some(declared_names) = methods.get(&interface).filter(|names| !names.is_empty()) else {
            continue;
        };
        // Gate first, then count: a sourceless stub or a cross-language name
        // collision never gets an edge, so it must not lower the confidence of
        // the implementers that do (#4396).
        let mut gated = vec![];
        for implementation in &impls {
            let implementation = key(implementation)?;
            if let Some(impl_node) = by_id
                .get(&implementation)
                .filter(|n| declared_in(Some(n), suffixes))
            {
                gated.push((implementation, *impl_node));
            }
        }
        if gated.is_empty() {
            continue;
        }
        let score = dispatch_confidence(gated.len());
        for (implementation, impl_node) in gated {
            for (name, declared) in declared_names {
                if declared.len() != 1 {
                    continue;
                }
                // Ambiguity is per implementer: one implementer overloading a
                // name does not disqualify its siblings.
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
                added.push(edge_record(json!({"source":source,"target":target,"relation":DISPATCH_RELATION,"context":"call","confidence":"INFERRED","confidence_score":score,
                    "source_file":text(impl_node,"source_file"),"source_location":impl_node["source_location"],"weight":1.0})));
            }
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
