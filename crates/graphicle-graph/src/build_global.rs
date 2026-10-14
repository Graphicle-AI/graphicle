// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/build.py, graphify/global_graph.py
//! build.py global-store dependencies. The rest of build.py remains on the migration ledger.
use crate::{
    Result,
    graph::Graph,
    path_syntax,
    python_compat::{self, scalar_key},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
fn id_string(v: &Value) -> String {
    if let Value::Array(a) = v {
        format!(
            "({}{})",
            a.iter()
                .map(|v| if v.is_array() {
                    id_string(v)
                } else {
                    python_compat::repr(v)
                })
                .collect::<Vec<_>>()
                .join(", "),
            if a.len() == 1 { "," } else { "" }
        )
    } else {
        python_compat::string(v)
    }
}
pub fn prefix_graph_for_global(graph: &Graph, tag: &str, offset: i64) -> Result<Graph> {
    let mapping: Vec<_> = graph
        .nodes
        .iter()
        .filter(|n| !n.attributes.get("external").is_some_and(truthy))
        .map(|n| (n.id.clone(), json!(format!("{tag}::{}", id_string(&n.id)))))
        .collect();
    let mut out = graph.relabel_copy(&mapping)?;
    for node in &mut out.nodes {
        if node.attributes.get("external").is_some_and(truthy) {
            continue;
        }
        node.attributes.insert("repo".into(), json!(tag));
        let local = node
            .id
            .as_str()
            .and_then(|s| s.split_once("::"))
            .map(|(_, s)| s)
            .ok_or("Prefixed node identifier must contain ::")?;
        node.attributes.entry("local_id").or_insert(json!(local));
        if offset != 0
            && let Some(cid) = node
                .attributes
                .get("community")
                .filter(|v| v.is_boolean() || v.as_number().is_some_and(|n| !n.is_f64()))
                .cloned()
        {
            let old = cid
                .as_i64()
                .map(i128::from)
                .or_else(|| cid.as_u64().map(i128::from))
                .or_else(|| cid.as_bool().map(i128::from))
                .unwrap();
            let new = old + i128::from(offset);
            let new = if let Ok(n) = i64::try_from(new) {
                json!(n)
            } else {
                json!(
                    u64::try_from(new)
                        .map_err(|_| "Community identifier exceeds JSON integer range")?
                )
            };
            node.attributes.insert("local_community".into(), cid);
            node.attributes.insert("community".into(), new);
        }
    }
    // Preserve graph traversal order while updating edge attributes through the existing keys.
    let edges = out
        .ordered_edges()
        .iter()
        .map(|(u, v, e)| {
            (
                out.nodes[*u].id.clone(),
                out.nodes[*v].id.clone(),
                e.key.clone(),
                e.attributes.clone(),
            )
        })
        .collect::<Vec<_>>();
    for (u, v, k, mut attrs) in edges {
        for name in ["_src", "_tgt"] {
            if let Some(value) = attrs.get(name) {
                let key =
                    scalar_key(value).ok_or("TypeError: unhashable directional edge identifier")?;
                if let Some((_, id)) = mapping
                    .iter()
                    .find(|(old, _)| scalar_key(old).as_ref() == Some(&key))
                {
                    attrs.insert(name.into(), id.clone());
                }
            }
        }
        out.add_edge(u, v, out.multigraph.then_some(k), attrs)?;
    }
    if let Some(hyperedges) = out
        .attributes
        .get_mut("hyperedges")
        .and_then(Value::as_array_mut)
    {
        for he in hyperedges {
            if let Some(he) = he.as_object_mut() {
                if let Some(members) = he.get_mut("nodes").and_then(Value::as_array_mut) {
                    for member in members {
                        if let Some(key) = scalar_key(member)
                            && let Some((_, new)) = mapping
                                .iter()
                                .find(|(old, _)| scalar_key(old).as_ref() == Some(&key))
                        {
                            *member = new.clone();
                        }
                    }
                }
                if let Some(id) = he.get_mut("id").filter(|v| truthy(v)) {
                    *id = json!(format!("{tag}::{}", python_compat::string(id)));
                }
            }
        }
    }
    Ok(out)
}
pub fn prune_repo_from_graph(graph: &mut Graph, tag: &str) -> Result<usize> {
    let ids = graph
        .nodes
        .iter()
        .filter(|n| n.attributes.get("repo").and_then(Value::as_str) == Some(tag))
        .map(|n| n.id.clone())
        .collect::<Vec<_>>();
    graph.remove_nodes_from(&ids)
}
pub fn distinct_repo_tags(paths: &[String]) -> Vec<String> {
    let dirs = paths
        .iter()
        .map(|p| {
            let mut parts = path_syntax::posix_parts(p);
            for _ in 0..2 {
                if !path_syntax::posix_name(&parts).is_empty() {
                    parts.pop();
                }
            }
            parts
        })
        .collect::<Vec<_>>();
    let mut tags = dirs
        .iter()
        .map(|p| {
            let n = path_syntax::posix_name(p);
            if n.is_empty() {
                "repo".into()
            } else {
                n.into()
            }
        })
        .collect::<Vec<String>>();
    if tags.iter().collect::<BTreeSet<_>>().len() != tags.len() {
        tags = dirs
            .iter()
            .map(|p| {
                let name = path_syntax::posix_name(p);
                let mut parent = p.clone();
                if !name.is_empty() {
                    parent.pop();
                }
                let parent = path_syntax::posix_name(&parent);
                if !parent.is_empty() && !name.is_empty() {
                    format!("{parent}_{name}")
                } else if name.is_empty() {
                    "repo".into()
                } else {
                    name.into()
                }
            })
            .collect();
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    tags.into_iter()
        .map(|tag| {
            let count = seen.entry(tag.clone()).or_default();
            *count += 1;
            if *count == 1 {
                tag
            } else {
                format!("{tag}-{count}")
            }
        })
        .collect()
}
