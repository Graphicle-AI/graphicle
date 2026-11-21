// Adapted from Graphify 0.9.105, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_affected_member_seed.py, tests/test_tree_html.py,
// tests/test_serve.py
//! Affected, tree and query changes ported from Graphify 0.9.105. Cases
//! follow the upstream tests named in each comment.
use serde_json::json;

#[test]
fn file_affected_reaches_nested_method_callers() {
    // test_affected_member_seed.py (#5094)
    let data = json!({"directed": true, "multigraph": false, "graph": {},
        "nodes": [
            {"id": "file_p", "label": "processor.py"},
            {"id": "proc", "label": "Processor"},
            {"id": "proc_call", "label": ".call()"},
            {"id": "runner", "label": "Runner"},
            {"id": "runner_run", "label": ".run()"},
        ],
        "links": [
            {"source": "file_p", "target": "proc", "relation": "contains"},
            {"source": "proc", "target": "proc_call", "relation": "method"},
            {"source": "runner", "target": "runner_run", "relation": "method"},
            {"source": "runner_run", "target": "proc_call", "relation": "calls"},
        ]});
    let g = graphicle::paths::load_node_link_graph(&data).unwrap();
    let relations: Vec<String> = graphicle::affected::DEFAULT_AFFECTED_RELATIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let hits = graphicle::affected::affected_nodes(&g, "file_p", &relations, 2)
        .unwrap()
        .into_iter()
        .map(|h| h.node_id)
        .collect::<Vec<_>>();
    assert!(hits.contains(&"runner_run".to_string()), "{hits:?}");
}
