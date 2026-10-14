// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/multigraph_compat.py
//! Port of graphify/multigraph_compat.py; probes the actual Rust graph backend.
use crate::{Result, graph::Graph};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, sync::LazyLock};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MultigraphCapabilityResult {
    pub runtime: String,
    pub backend: String,
    pub checks: Vec<CapabilityCheck>,
}
impl MultigraphCapabilityResult {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
    pub fn failed(&self) -> Vec<&CapabilityCheck> {
        self.checks.iter().filter(|c| !c.ok).collect()
    }
    pub fn error_message(&self) -> String {
        if self.ok() {
            format!(
                "Graphicle MultiDiGraph capability probe passed ({}, {}).",
                self.runtime, self.backend
            )
        } else {
            format!(
                "error: --multigraph requires keyed MultiDiGraph node-link round-trip support. Detected {}, {}. Failed capability check(s): {}. Default simple graph mode remains available.",
                self.runtime,
                self.backend,
                self.failed()
                    .iter()
                    .map(|c| format!("{}: {}", c.name, c.detail))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        }
    }
    pub fn json(&self) -> Value {
        let mut value = json!(self);
        value["ok"] = json!(self.ok());
        value["failed"] = json!(self.failed());
        value["message"] = json!(self.error_message());
        value
    }
}
pub fn check(name: &str, func: impl FnOnce() -> Result<bool>) -> CapabilityCheck {
    match func() {
        Ok(true) => CapabilityCheck {
            name: name.into(),
            ok: true,
            detail: "ok".into(),
        },
        Ok(false) => CapabilityCheck {
            name: name.into(),
            ok: false,
            detail: "unexpected result False".into(),
        },
        Err(error) => CapabilityCheck {
            name: name.into(),
            ok: false,
            detail: error.to_string(),
        },
    }
}
pub fn build_probe_graph() -> Result<Graph> {
    let mut graph = Graph::new(true, true);
    graph.add_node(
        json!("a"),
        json!({"label":"A"}).as_object().unwrap().clone(),
    )?;
    graph.add_node(
        json!("b"),
        json!({"label":"B"}).as_object().unwrap().clone(),
    )?;
    for (key, relation) in [("calls:a.py:L1", "calls"), ("imports:a.py:L2", "imports")] {
        graph.add_edge(
            json!("a"),
            json!("b"),
            Some(json!(key)),
            json!({"relation":relation,"source_file":"a.py"})
                .as_object()
                .unwrap()
                .clone(),
        )?;
    }
    Ok(graph)
}
fn keys(graph: &Graph) -> BTreeSet<String> {
    graph
        .edges_between(&json!("a"), &json!("b"))
        .iter()
        .map(|edge| edge.key.as_str().unwrap_or("").into())
        .collect()
}
pub fn probe_keyed_parallel_edges() -> Result<bool> {
    let graph = build_probe_graph()?;
    if !graph.multigraph || !graph.directed {
        return Err("probe graph is not a directed multigraph".into());
    }
    if graph.number_of_edges(&json!("a"), &json!("b")) != 2 {
        return Err("expected 2 keyed parallel edges".into());
    }
    Ok(keys(&graph) == BTreeSet::from(["calls:a.py:L1".into(), "imports:a.py:L2".into()]))
}
pub fn probe_node_link_round_trip() -> Result<bool> {
    let graph = build_probe_graph()?;
    let data = graph.to_node_link("links");
    if data["directed"] != true
        || data["multigraph"] != true
        || data["links"].as_array().map(Vec::len) != Some(2)
    {
        return Err("invalid serialized graph flags or links".into());
    }
    let loaded = Graph::from_node_link(&data, "links")?;
    Ok(loaded.directed
        && loaded.multigraph
        && loaded.edge_count() == 2
        && keys(&loaded) == keys(&graph))
}
pub fn probe_duplicate_key_overwrite_semantics() -> Result<bool> {
    let mut graph = Graph::new(true, true);
    for marker in ["first", "second"] {
        graph.add_edge(
            json!("x"),
            json!("y"),
            Some(json!("same")),
            json!({"marker":marker}).as_object().unwrap().clone(),
        )?;
    }
    Ok(graph.edge_count() == 1
        && graph.edges_between(&json!("x"), &json!("y"))[0].attributes["marker"] == "second")
}
pub fn probe_reserved_key_attr_rejected() -> Result<bool> {
    let mut graph = Graph::new(true, true);
    Ok(graph
        .add_edge(
            json!("a"),
            json!("b"),
            Some(json!("schema-key")),
            json!({"key":"attr-key","relation":"calls"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .is_err())
}
pub fn probe_remove_edges_from_two_tuple_semantics() -> Result<bool> {
    let mut graph = Graph::new(true, true);
    for key in ["one", "two"] {
        graph.add_edge(json!("a"), json!("b"), Some(json!(key)), Map::new())?;
    }
    graph.remove_edges_from(&[(json!("a"), json!("b"), None)]);
    Ok(graph.number_of_edges(&json!("a"), &json!("b")) == 1)
}
pub fn probe_to_undirected_preserves_multigraph_type() -> Result<bool> {
    let graph = build_probe_graph()?;
    let undirected = graph.to_undirected()?;
    let view = graph.undirected_view();
    Ok(
        !undirected.directed
            && undirected.multigraph
            && !view.is_directed()
            && view.is_multigraph(),
    )
}
static PROBE: LazyLock<MultigraphCapabilityResult> = LazyLock::new(|| MultigraphCapabilityResult {
    runtime: "Rust".into(),
    backend: "graphicle native attributed graph 0.1.0".into(),
    checks: vec![
        check("keyed_parallel_edges", probe_keyed_parallel_edges),
        check(
            "node_link_edges_links_round_trip",
            probe_node_link_round_trip,
        ),
        check(
            "duplicate_key_overwrite_semantics",
            probe_duplicate_key_overwrite_semantics,
        ),
        check(
            "reserved_key_attr_rejected",
            probe_reserved_key_attr_rejected,
        ),
        check(
            "remove_edges_from_two_tuple_semantics",
            probe_remove_edges_from_two_tuple_semantics,
        ),
        check(
            "to_undirected_preserves_multigraph_type",
            probe_to_undirected_preserves_multigraph_type,
        ),
    ],
});
pub fn probe_multigraph_capabilities() -> &'static MultigraphCapabilityResult {
    &PROBE
}
pub fn require_multigraph_capabilities() -> Result<&'static MultigraphCapabilityResult> {
    let result = probe_multigraph_capabilities();
    if result.ok() {
        Ok(result)
    } else {
        Err((result.error_message()).into())
    }
}
