// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/diagnostics.py
use crate::{Result, build_normalize::truthy, python_compat};
use serde_json::{Value, json};

pub fn format_json(summary: &Value) -> Value {
    let mut core = summary.clone();
    if let Some(m) = core.as_object_mut() {
        m.shift_remove("examples");
        m.shift_remove("producer_suppression");
    }
    json!({"schema_version":1,"summary":core,
        "examples":summary.get("examples").cloned().unwrap_or(json!([])),
        "producer_suppression":summary.get("producer_suppression").cloned().unwrap_or(json!({})),
        "notes":["Diagnostics are read-only.","A normal graph.json is already post-build and cannot recover raw producer edges.","Producer suppression sites are heuristic source-code evidence."]})
}
fn text(v: &Value, key: &str, default: &str) -> String {
    v.get(key)
        .map(python_compat::string)
        .unwrap_or_else(|| default.into())
}
pub fn format_report(summary: &Value) -> Result<String> {
    let suppression = &summary["producer_suppression"];
    let mut lines = vec![
        "[graphicle] MultiDiGraph edge-collapse diagnostic".into(),
        format!("input: {}", text(summary, "input_path", "<in-memory>")),
        "input_stage: provided JSON (normal graph.json is post-build)".into(),
        format!(
            "effective_directed: {}",
            text(summary, "effective_directed", "<direct-call>")
        ),
    ];
    for (label, key) in [
        ("nodes", "node_count"),
        ("unverified_code_nodes", "unverified_node_count"),
        ("raw_edges", "raw_edge_count"),
        ("valid_candidate_edges", "valid_candidate_edges"),
        ("missing_endpoint_edges", "missing_endpoint_edges"),
        ("dangling_endpoint_edges", "dangling_endpoint_edges"),
        ("external_reference_edges", "external_reference_edges"),
        ("self_loop_edges", "self_loop_edges"),
        ("exact_duplicate_edges", "exact_duplicate_edges"),
        (
            "directed_unique_endpoint_pairs",
            "directed_unique_endpoint_pairs",
        ),
        (
            "directed_same_endpoint_collapsed_edges",
            "directed_same_endpoint_collapsed_edges",
        ),
        (
            "undirected_unique_endpoint_pairs",
            "undirected_unique_endpoint_pairs",
        ),
        (
            "undirected_same_endpoint_collapsed_edges",
            "undirected_same_endpoint_collapsed_edges",
        ),
        ("same_endpoint_group_count", "same_endpoint_group_count"),
        ("relation_variant_groups", "relation_variant_groups"),
        ("source_file_variant_groups", "source_file_variant_groups"),
        (
            "source_location_variant_groups",
            "source_location_variant_groups",
        ),
        ("context_variant_groups", "context_variant_groups"),
        ("post_build_graph_type", "post_build_graph_type"),
        ("post_build_edges", "post_build_edge_count"),
    ] {
        if summary.get(key).is_none()
            && !["unverified_node_count", "external_reference_edges"].contains(&key)
        {
            return Err(format!("Missing diagnostic field: {key}").into());
        }
        lines.push(format!("{label}: {}", text(summary, key, "0")));
    }
    lines.push(format!(
        "producer_suppression_sites: {}",
        text(suppression, "total_sites", "0")
    ));
    for (label, value) in [
        ("post_build_error", &summary["post_build_error"]),
        ("producer_suppression_error", &suppression["error"]),
    ] {
        if truthy(value) {
            lines.push(format!("{label}: {}", python_compat::string(value)));
        }
    }
    if let Some(sites) = suppression["sites"].as_array().filter(|a| !a.is_empty()) {
        lines.push("producer_suppression_examples:".into());
        for site in sites.iter().take(8) {
            let arity = if truthy(&site["tuple_arity"]) {
                python_compat::string(&site["tuple_arity"])
            } else {
                "unknown".into()
            };
            lines.push(format!(
                "  - L{} {} arity={arity}",
                text(site, "line", ""),
                text(site, "name", "")
            ));
        }
    }
    if let Some(examples) = summary["examples"].as_array().filter(|a| !a.is_empty()) {
        lines.push("examples:".into());
        for example in examples {
            lines.push(format!(
                "  - {} -> {} edges={} relations={} locations={} contexts={}",
                text(example, "source", ""),
                text(example, "target", ""),
                text(example, "edge_count", ""),
                python_compat::repr(&example["relations"]),
                python_compat::repr(&example["source_locations"]),
                python_compat::repr(&example["contexts"])
            ));
        }
    }
    lines.push(
        "note: normal graph.json is post-build; raw producer loss must be measured earlier.".into(),
    );
    Ok(lines.join("\n"))
}
