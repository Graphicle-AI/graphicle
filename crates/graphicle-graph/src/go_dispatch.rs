// Adapted from Graphify 0.9.102, Apache-2.0; see NOTICE.md.
// Upstream: graphify/go_dispatch.py
//! Member-level interface dispatch for Go. Go types satisfy an interface
//! implicitly, so the `implements` pairs are inferred from method sets and
//! handed to [`crate::interface_dispatch`], which links each interface method
//! to the implementers' methods with `dispatches_to`; the inferred pairs are
//! not added to the graph.
//!
//! A named type implements an interface when its methods (value and pointer
//! receivers alike) include every method name the interface requires. Both
//! ends are declared in a `.go` file; an interface with no methods or with a
//! type set is skipped; one that embeds an interface outside the corpus is
//! skipped; an unexported requirement is matched only in the interface's own
//! directory; and an interface matched by more than ten types is skipped.
use crate::{
    Result,
    extract::per_file::PerFile,
    interface_dispatch::{declared_in, method_label},
    resolver_registry::{Rec, rs},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, path::Path};

const GO_SUFFIXES: &[&str] = &[".go"];
const MAX_IMPLICIT_IMPLEMENTERS: usize = 10;

fn is_go_interface(node: &Value) -> bool {
    node["metadata"]["language"] == "go" && node["metadata"]["kind"] == "interface"
}

pub fn resolve_go_interface_dispatch(
    per_file: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
) -> Result<()> {
    resolve_go_interface_dispatch_any(per_file, nodes, edges)
}

/// [`resolve_go_interface_dispatch`] over the per-file results and the edges
/// in either form.
pub fn resolve_go_interface_dispatch_any<P: PerFile, E: Rec>(
    per_file: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) -> Result<()> {
    let inferred = implicit_implements(nodes, edges);
    if inferred.is_empty() {
        return Ok(());
    }
    let base = edges.len();
    let count = inferred.len();
    edges.extend(inferred.into_iter().map(|(source, target)| {
        let mut edge = Map::new();
        edge.insert("source".into(), json!(source));
        edge.insert("target".into(), json!(target));
        edge.insert("relation".into(), json!("implements"));
        E::from_object(edge)
    }));
    let result =
        crate::interface_dispatch::resolve_interface_dispatch_any(per_file, nodes, edges, GO_SUFFIXES);
    // The inferred pairs only feed the mechanism.
    edges.drain(base..base + count);
    result
}

/// The `(implementer, interface)` pairs the method sets imply.
fn implicit_implements<E: Rec>(nodes: &[Value], edges: &[E]) -> Vec<(String, String)> {
    let interfaces = nodes
        .iter()
        .filter(|n| is_go_interface(n) && declared_in(Some(n), GO_SUFFIXES))
        .map(|n| (n["id"].as_str().unwrap_or(""), n))
        .collect::<FxHashMap<_, _>>();
    if interfaces.is_empty() {
        return vec![];
    }
    let by_id = nodes
        .iter()
        .map(|n| (n["id"].as_str().unwrap_or(""), n))
        .collect::<FxHashMap<_, _>>();
    let mut method_names: FxHashMap<&str, BTreeSet<String>> = FxHashMap::default();
    let mut owners: Vec<&str> = vec![];
    let mut embeds: FxHashMap<&str, Vec<&str>> = FxHashMap::default();
    let mut constrained: FxHashSet<&str> = FxHashSet::default();
    for e in edges {
        let (relation, source) = (rs(e, "relation"), rs(e, "source"));
        if relation == "method" {
            if let Some(method) = by_id.get(rs(e, "target"))
                && declared_in(Some(method), GO_SUFFIXES)
            {
                let label = method_label(method);
                if !label.is_empty() {
                    if !method_names.contains_key(source) {
                        owners.push(source);
                    }
                    method_names.entry(source).or_default().insert(label);
                }
            }
        } else if interfaces.contains_key(source) && relation == "embeds" {
            embeds.entry(source).or_default().push(rs(e, "target"));
        } else if interfaces.contains_key(source)
            && relation == "references"
            && rs(e, "context") == "type_constraint"
        {
            constrained.insert(source);
        }
    }
    fn required<'a>(
        interface: &'a str,
        visiting: &mut Vec<&'a str>,
        interfaces: &FxHashMap<&'a str, &Value>,
        method_names: &FxHashMap<&'a str, BTreeSet<String>>,
        embeds: &FxHashMap<&'a str, Vec<&'a str>>,
    ) -> Option<BTreeSet<String>> {
        let mut names = method_names.get(interface).cloned().unwrap_or_default();
        for &embedded in embeds.get(interface).into_iter().flatten() {
            if !interfaces.contains_key(embedded) || visiting.contains(&embedded) {
                return None;
            }
            visiting.push(interface);
            let inner = required(embedded, visiting, interfaces, method_names, embeds);
            visiting.pop();
            names.extend(inner?);
        }
        Some(names)
    }
    let directory = |node: &Value| {
        Path::new(node["source_file"].as_str().unwrap_or(""))
            .parent()
            .map(Path::to_path_buf)
    };
    let candidates = owners
        .iter()
        .copied()
        .filter(|owner| {
            !interfaces.contains_key(owner) && declared_in(by_id.get(owner).copied(), GO_SUFFIXES)
        })
        .collect::<Vec<_>>();
    let mut inferred = vec![];
    // Interfaces in node order, as upstream's dict of them.
    let mut ordered = interfaces.iter().collect::<Vec<_>>();
    let position = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n["id"].as_str().unwrap_or(""), i))
        .collect::<FxHashMap<_, _>>();
    ordered.sort_by_key(|(id, _)| position.get(*id).copied().unwrap_or(usize::MAX));
    for (&interface, &node) in ordered {
        if constrained.contains(interface)
            || method_names.get(interface).is_none_or(BTreeSet::is_empty)
        {
            continue;
        }
        let Some(needed) = required(interface, &mut vec![], &interfaces, &method_names, &embeds)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let package = directory(node);
        let same_package_only = needed
            .iter()
            .any(|name| !name.chars().next().is_some_and(char::is_uppercase));
        let implementers = candidates
            .iter()
            .copied()
            .filter(|owner| {
                needed.is_subset(&method_names[owner])
                    && (!same_package_only
                        || by_id.get(owner).map(|n| directory(n)) == Some(package.clone()))
            })
            .collect::<Vec<_>>();
        if !implementers.is_empty() && implementers.len() <= MAX_IMPLICIT_IMPLEMENTERS {
            inferred.extend(
                implementers
                    .into_iter()
                    .map(|owner| (owner.to_string(), interface.to_string())),
            );
        }
    }
    inferred
}
