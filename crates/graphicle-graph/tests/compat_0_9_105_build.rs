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

#[test]
fn build_tree_single_file_common_root_is_directory() {
    // test_tree_html.py test_build_tree_single_file_common_root_is_directory (#5092)
    let graph = json!({"directed": false, "links": [],
        "nodes": [{"id": "n0", "label": "func_a", "source_file": "pkg/a.py"}]});
    let tree = graphicle::tree_html::build_tree(&graph, None, 200, None).unwrap();
    assert_eq!(tree["name"], "pkg", "{tree:#}");
    let names = |v: &serde_json::Value| {
        v["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&tree), ["a.py"]);
    assert_eq!(names(&tree["children"][0]), ["func_a"]);
}
