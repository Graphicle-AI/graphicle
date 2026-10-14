// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
//! Deduplication fix ported from Graphify 0.9.76 (7e7de7d, #3094): the exact
//! pass calls the cross-file block for every pair, as the fuzzy pass does.
use graphicle::build::{Diagnostics, MergeOptions, build_merge};
use graphicle::dedup::{DedupOptions, deduplicate_entities};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn doc(id: &str, label: &str, source_file: &str, file_type: &str) -> Value {
    json!({"id": id, "label": label, "file_type": file_type,
           "source_file": source_file, "source_location": null})
}

fn dedup(nodes: &[Value]) -> Vec<Value> {
    deduplicate_entities(nodes, &[], &DedupOptions::default(), None, None)
        .unwrap()
        .nodes
}

#[test]
fn exact_pass_keeps_unstamped_documents_across_files() {
    let dir = tempfile::tempdir().unwrap();
    let extraction = json!({"nodes": [
        doc("a_decisions", "Decisions", "Sessions/session-A.md", "document"),
        doc("b_decisions", "Decisions", "Sessions/session-B.md", "document"),
        doc("c_decisions", "Decisions", "Sessions/session-C.md", "document"),
        doc("a_open_items", "Open items", "Sessions/session-A.md", "document"),
        doc("b_open_items", "Open items", "Sessions/session-B.md", "document"),
    ], "edges": [], "hyperedges": [], "input_tokens": 0, "output_tokens": 0});
    let options = MergeOptions {
        graph_path: Some(dir.path().join("missing.json")),
        dedup: true,
        ..Default::default()
    };
    let mut messages = Diagnostics::default();
    let graph = build_merge(&[extraction], &options, &mut messages, None).unwrap();
    let files = graph
        .nodes()
        .iter()
        .filter_map(|n| {
            n.attributes
                .get("source_file")?
                .as_str()
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(graph.node_count(), 5);
    assert_eq!(
        files,
        BTreeSet::from(
            [
                "Sessions/session-A.md",
                "Sessions/session-B.md",
                "Sessions/session-C.md"
            ]
            .map(str::to_string)
        )
    );
    assert!(
        !messages.stdout.iter().any(|m| m.contains("Deduplicated")),
        "{:?}",
        messages.stdout
    );
}

#[test]
fn crossfile_document_entity_variants_stay_one_node_per_file() {
    let nodes = ["@cyrilXBT", "@cyrilxbt", "cyrilXBT"]
        .iter()
        .enumerate()
        .map(|(i, variant)| {
            doc(
                &format!("journal_2024_03_0{}_cyrilxbt", i + 1),
                variant,
                &format!("journal/2024-03-0{}.md", i + 1),
                "document",
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(dedup(&nodes).len(), 3);
}

#[test]
fn crossfile_rationale_entity_variants_stay_one_node_per_file() {
    let nodes = [
        doc(
            "svc_alpha_py_retention_window",
            "Retention Window",
            "svc/alpha.py",
            "rationale",
        ),
        doc(
            "svc_beta_py_retention_window",
            "retention window",
            "svc/beta.py",
            "rationale",
        ),
    ];
    assert_eq!(dedup(&nodes).len(), 2);
}

#[test]
fn exact_pass_still_merges_same_file_document_duplicate() {
    let nodes = [
        doc("a1", "Decisions", "Sessions/session-A.md", "document"),
        doc("a2", "Decisions", "Sessions/session-A.md", "document"),
    ];
    assert_eq!(dedup(&nodes).len(), 1);
}

#[test]
fn exact_pass_still_merges_crossfile_concepts() {
    let nodes = [
        doc("c1", "Decisions", "Sessions/session-A.md", "concept"),
        doc("c2", "Decisions", "Sessions/session-B.md", "concept"),
    ];
    assert_eq!(dedup(&nodes).len(), 1);
}

#[test]
fn exact_pass_merges_concepts_beside_a_blocked_document() {
    let nodes = [
        doc("a", "Decisions", "Sessions/session-A.md", "document"),
        doc("c1", "Decisions", "Sessions/session-B.md", "concept"),
        doc("c2", "Decisions", "Sessions/session-C.md", "concept"),
    ];
    let out = dedup(&nodes);
    let mut kinds = out
        .iter()
        .map(|n| n["file_type"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    kinds.sort();
    assert_eq!(out.len(), 2);
    assert_eq!(kinds, ["concept", "document"]);
    assert!(out.iter().any(|n| n["id"] == "a"));
    assert!(out.iter().any(|n| n["id"] == "c1"));
}

#[test]
fn incoming_concepts_join_each_other_when_the_protected_winner_is_blocked() {
    let nodes = [
        doc("a", "Decisions", "Sessions/session-A.md", "document"),
        doc("c1", "Decisions", "Sessions/session-B.md", "concept"),
        doc("c2", "Decisions", "Sessions/session-C.md", "concept"),
    ];
    let options = DedupOptions {
        protected_ids: ["a".to_string()].into(),
        ..Default::default()
    };
    let out = deduplicate_entities(&nodes, &[], &options, None, None)
        .unwrap()
        .nodes;
    let mut ids = out
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, ["a", "c1"], "{out:?}");
}
