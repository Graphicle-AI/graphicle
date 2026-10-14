//! Mirrors the `build.py` tests added upstream in Graphify 0.9.66
//! (`tests/test_build.py::test_method_ghost_*_3705`, #3703).
use graphicle::graph::Graph;
use serde_json::{Value, json};

fn build(extraction: Value) -> Graph {
    let mut data = extraction;
    let mut diagnostics = graphicle::build::Diagnostics::default();
    graphicle::build_from_json(&mut data, true, None, &mut diagnostics).unwrap()
}

fn has_node(graph: &Graph, id: &str) -> bool {
    graph.node_index(&json!(id)).is_some()
}

fn has_edge(graph: &Graph, source: &str, target: &str) -> bool {
    !graph
        .edges_between(&json!(source), &json!(target))
        .is_empty()
}

fn method_ghost_extraction() -> Value {
    json!({
        "nodes": [
            {
                "id": "broker_mock_tda_mocktdaaccount_save_state",
                "label": ".save_state()",
                "file_type": "code",
                "source_file": "broker/mock_tda.py",
                "source_location": "L220",
                "_origin": "ast"
            },
            {
                "id": "broker_mock_tda_save_state",
                "label": "save_state()",
                "file_type": "code",
                "source_file": "broker/mock_tda.py"
            },
            {
                "id": "doc_summary",
                "label": "Doc Summary",
                "file_type": "document",
                "source_file": "docs/summary.md"
            }
        ],
        "edges": [
            {
                "source": "doc_summary",
                "target": "broker_mock_tda_save_state",
                "relation": "references",
                "confidence": "INFERRED",
                "confidence_score": 0.85,
                "source_file": "docs/summary.md"
            }
        ]
    })
}

#[test]
fn method_ghost_dedupe_and_alias_resolution_3705() {
    {
        let graph = build(method_ghost_extraction());
        assert!(!has_node(&graph, "broker_mock_tda_save_state"));
        assert!(has_node(
            &graph,
            "broker_mock_tda_mocktdaaccount_save_state"
        ));
        assert!(has_edge(
            &graph,
            "doc_summary",
            "broker_mock_tda_mocktdaaccount_save_state"
        ));
    }
}

#[test]
fn method_ghost_alias_remaps_dangling_stem_method_edge_3705() {
    // Alias index: `<file_stem>_<method>` (stem = make_id of the path without
    // extension) resolves an edge endpoint that never existed as a node onto the
    // single AST `.name()` node in that file. Verified against Python 0.9.66.
    let extraction = json!({
        "nodes": [
            {"id": "broker_mock_tda_mocktdaaccount_save_state", "label": ".save_state()",
             "file_type": "code", "source_file": "broker/mock_tda.py",
             "source_location": "L220", "_origin": "ast"},
            {"id": "doc_summary", "label": "Doc Summary", "file_type": "document",
             "source_file": "docs/summary.md"}
        ],
        "edges": [
            {"source": "doc_summary", "target": "broker_mock_tda_save_state", "relation": "references",
             "confidence": "INFERRED", "confidence_score": 0.85, "source_file": "docs/summary.md"}
        ]
    });
    {
        let graph = build(extraction);
        assert!(has_edge(
            &graph,
            "doc_summary",
            "broker_mock_tda_mocktdaaccount_save_state"
        ));
    }
}

#[test]
fn method_ghost_ambiguous_same_name_in_one_file_is_not_merged_3705() {
    let extraction = json!({
        "nodes": [
            {"id": "svc_a_save_state", "label": ".save_state()", "file_type": "code",
             "source_file": "svc.py", "source_location": "L10", "_origin": "ast"},
            {"id": "svc_b_save_state", "label": ".save_state()", "file_type": "code",
             "source_file": "svc.py", "source_location": "L40", "_origin": "ast"},
            {"id": "svc_save_state", "label": "save_state()", "file_type": "code",
             "source_file": "svc.py"},
            {"id": "doc", "label": "Doc", "file_type": "document", "source_file": "d.md"}
        ],
        "edges": [
            {"source": "doc", "target": "svc_save_state", "relation": "references",
             "confidence": "INFERRED", "confidence_score": 0.85, "source_file": "d.md"}
        ]
    });
    {
        let graph = build(extraction);
        assert!(has_node(&graph, "svc_a_save_state") && has_node(&graph, "svc_b_save_state"));
        assert!(
            has_node(&graph, "svc_save_state"),
            "ambiguous ghost must be left intact, not remapped"
        );
        assert!(has_edge(&graph, "doc", "svc_save_state"));
        assert!(!has_edge(&graph, "doc", "svc_a_save_state"));
        assert!(!has_edge(&graph, "doc", "svc_b_save_state"));
    }
}

#[test]
fn dotfile_label_is_not_a_method_ghost_candidate_3705() {
    // `.env` has the leading dot but no `()` suffix: never indexed as a method.
    let extraction = json!({
        "nodes": [
            {"id": "cfg_env_file", "label": ".env", "file_type": "code",
             "source_file": "cfg/.env", "source_location": "L1", "_origin": "ast"},
            {"id": "cfg_env", "label": "env", "file_type": "code", "source_file": "cfg/.env"}
        ],
        "edges": []
    });
    {
        let graph = build(extraction);
        assert!(has_node(&graph, "cfg_env_file"));
        assert!(has_node(&graph, "cfg_env"));
    }
}

/// Mirrors `test_relation_collapse_precedence.py::
/// test_same_relation_collision_preserves_extracted_over_inferred` (#3703).
#[test]
fn same_relation_collision_preserves_extracted_over_inferred_3703() {
    let edge = |confidence: &str, score: f64, file: &str, location: Value| {
        json!({
            "source": "a", "target": "b", "relation": "calls",
            "confidence": confidence, "confidence_score": score,
            "source_file": file, "source_location": location,
        })
    };
    let extracted = edge("EXTRACTED", 1.0, "a.py", json!("L724"));
    let inferred = edge("INFERRED", 0.95, "note.md", Value::Null);
    for order in [
        vec![extracted.clone(), inferred.clone()],
        vec![inferred.clone(), extracted.clone()],
    ] {
        let extraction = json!({
            "nodes": [
                {"id": "a", "label": "a()", "file_type": "code", "source_file": "a.py"},
                {"id": "b", "label": "b()", "file_type": "code", "source_file": "b.py"}
            ],
            "edges": order,
            "hyperedges": []
        });
        {
            let graph = build(extraction);
            let edges = graph.edges_between(&json!("a"), &json!("b"));
            assert_eq!(edges.len(), 1);
            let attrs = &edges[0].attributes;
            assert_eq!(attrs.get("relation"), Some(&json!("calls")));
            assert_eq!(attrs.get("confidence"), Some(&json!("EXTRACTED")));
            assert_eq!(attrs.get("confidence_score"), Some(&json!(1.0)));
            assert_eq!(attrs.get("source_file"), Some(&json!("a.py")));
            assert_eq!(attrs.get("source_location"), Some(&json!("L724")));
        }
    }
}
