// Graphify 0.9.74 build parity: hyperedges carried across `--update`.
use graphicle::build::{Diagnostics, MergeOptions, build_merge};
use serde_json::{Value, json};

fn merged_labels(hyperedges: Value, chunk: Value) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("corpus");
    std::fs::create_dir_all(&root).unwrap();
    let graph_path = dir.path().join("graph.json");
    let nodes = json!([
        {"id": "a1", "label": "a1", "file_type": "document", "source_file": "a.md"},
        {"id": "b1", "label": "b1", "file_type": "document", "source_file": "b.md"},
    ]);
    std::fs::write(
        &graph_path,
        json!({"nodes": nodes, "edges": [], "hyperedges": hyperedges}).to_string(),
    )
    .unwrap();
    let chunk = chunk.to_string().replace("{root}", &root.to_string_lossy());
    let options = MergeOptions {
        graph_path: Some(graph_path),
        dedup: false,
        root: Some(root),
        ..Default::default()
    };
    let graph = build_merge(
        &[serde_json::from_str(&chunk).unwrap()],
        &options,
        &mut Diagnostics::default(),
        None,
    )
    .unwrap();
    let mut labels: Vec<String> = graph.attributes()["hyperedges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["label"].as_str().unwrap().to_string())
        .collect();
    labels.sort();
    labels
}

#[test]
fn update_keeps_an_unchanged_files_hyperedge_that_shares_an_id() {
    let labels = merged_labels(
        json!([
            {"id": "shared_flow", "label": "flow A", "source_file": "a.md", "nodes": ["a1"]},
            {"id": "shared_flow", "label": "flow B", "source_file": "b.md", "nodes": ["b1"]},
        ]),
        json!({
            "nodes": [{"id": "b1", "label": "b1", "file_type": "document", "source_file": "b.md"}],
            "edges": [],
            "hyperedges": [{"id": "shared_flow", "label": "flow B v2", "source_file": "b.md",
                            "nodes": ["b1"]}],
        }),
    );
    assert_eq!(labels, ["flow A", "flow B v2"]);
}

#[test]
fn update_still_drops_a_hyperedge_the_new_chunk_re_emits() {
    let labels = merged_labels(
        json!([
            {"id": "he_a", "label": "flow A", "source_file": "a.md", "nodes": ["a1"]},
            {"id": "he_b", "label": "flow B", "source_file": "b.md", "nodes": ["b1"]},
            {"id": "he_global", "label": "cross-file flow", "nodes": ["a1", "b1"]},
        ]),
        json!({
            "nodes": [{"id": "b1", "label": "b1", "file_type": "document", "source_file": "b.md"}],
            "edges": [],
            "hyperedges": [
                {"id": "he_a", "label": "flow A v2", "source_file": "{root}/a.md", "nodes": ["a1"]},
                {"id": "he_global", "label": "cross-file flow v2", "source_file": "",
                 "nodes": ["a1", "b1"]},
            ],
        }),
    );
    assert_eq!(labels, ["cross-file flow v2", "flow A v2"]);
}
