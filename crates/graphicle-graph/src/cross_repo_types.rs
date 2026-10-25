// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/cross_repo_types.py
//! Native translation of graphify/cross_repo_types.py.
use crate::{
    Result,
    graph::Graph,
    python_compat::{scalar_key, string},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const SHARED_TYPE_RELATION: &str = "same_type_as";
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
fn value<'a>(data: &'a impl crate::record::Properties, key: &str) -> &'a Value {
    data.get(key).unwrap_or(&Value::Null)
}
pub fn link_shared_type_declarations(merged: &mut Graph) -> Result<usize> {
    let mut lookup = BTreeMap::new();
    let mut groups: Vec<Vec<usize>> = vec![];
    for (index, node) in merged.nodes.iter().enumerate() {
        let data = &node.attributes;
        if !truthy(value(data, "_callable_class")) || !truthy(value(data, "source_file")) {
            continue;
        }
        let metadata = value(data, "metadata");
        let namespace = if truthy(metadata) {
            metadata
                .as_object()
                .ok_or("metadata must be an object")?
                .get("namespace")
                .unwrap_or(&Value::Null)
        } else {
            &Value::Null
        };
        let label = value(data, "label");
        if !truthy(namespace) || !truthy(label) || !truthy(value(data, "repo")) {
            continue;
        }
        let key = (string(namespace), string(label));
        let group = *lookup.entry(key).or_insert_with(|| {
            groups.push(vec![]);
            groups.len() - 1
        });
        groups[group].push(index);
    }
    let mut added = 0;
    for nodes in groups {
        if nodes.len() < 2 {
            continue;
        }
        let repos: BTreeSet<_> = nodes
            .iter()
            .map(|&i| {
                scalar_key(value(&merged.nodes[i].attributes, "repo"))
                    .ok_or_else(|| "unhashable repository tag".into())
            })
            .collect::<Result<_>>()?;
        if repos.len() < 2 {
            continue;
        }
        for (i, &left) in nodes.iter().enumerate() {
            for &right in &nodes[i + 1..] {
                let lhs = &merged.nodes[left];
                let rhs = &merged.nodes[right];
                if scalar_key(value(&lhs.attributes, "repo"))
                    == scalar_key(value(&rhs.attributes, "repo"))
                    || merged.number_of_edges(&lhs.id, &rhs.id) > 0
                {
                    continue;
                }
                let attrs = json!({"relation":SHARED_TYPE_RELATION,"context":"cross_repo","confidence":"INFERRED","confidence_score":0.85,
                    "source_file":string(value(&lhs.attributes,"source_file")),"weight":1.0,"_src":lhs.id,"_tgt":rhs.id});
                merged.add_edge(
                    lhs.id.clone(),
                    rhs.id.clone(),
                    None,
                    attrs.as_object().unwrap().clone(),
                )?;
                added += 1;
            }
        }
    }
    Ok(added)
}
pub fn dispatch(request: &Value) -> Result<Value> {
    let mut graph = Graph::from_node_link(&request["graph"], "links")?;
    let added = link_shared_type_declarations(&mut graph)?;
    Ok(json!({"added":added,"graph":graph.to_node_link("links")}))
}
