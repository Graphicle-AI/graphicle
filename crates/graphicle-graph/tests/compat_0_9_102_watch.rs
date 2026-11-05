// Adapted from Graphify 0.9.102 (v0.9.102), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_watch.py, tests/test_update_incremental.py,
// tests/test_update_lock_contention.py, tests/test_update_no_change_gate.py
//! Watch, update and incremental changes ported from Graphify 0.9.96-0.9.102
//! (port part b). Cases follow the upstream tests named in each comment.
use graphicle::watch::{self, RebuildOptions};
use serde_json::{Value, json};
use std::{fs, path::Path};

fn graph(dir: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(dir.join(".graphicle/graph.json")).unwrap()).unwrap()
}

fn nodes_of(data: &Value, file: &str) -> Vec<String> {
    let mut ids: Vec<String> = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| {
            Path::new(n["source_file"].as_str().unwrap_or_default())
                .file_name()
                .is_some_and(|f| f == file)
        })
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn paper_classified_markdown_gets_the_quick_scan() {
    // test_watch.py (Graphify 0.9.96, #4750): a .md that detect() calls a
    // paper still gets its heading structure on an AST-only rebuild.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("app.py"), "def f():\n    return 1\n").unwrap();
    fs::write(
        dir.path().join("paper_notes.md"),
        "# Notes on a paper\n\narXiv 2501.00001\n\n## Abstract\n\nWe study X.\n",
    )
    .unwrap();
    let options = RebuildOptions {
        acquire_lock: false,
        no_cluster: true,
        ..Default::default()
    };
    let result = watch::rebuild(dir.path(), &options).unwrap();
    assert!(result.success, "{:?}", result.messages);
    assert!(!nodes_of(&graph(dir.path()), "paper_notes.md").is_empty());
}

#[test]
fn semantic_backed_paper_markdown_gets_no_ast_layer() {
    // test_watch.py (Graphify 0.9.96, #4750 x #1915): a paper .md that already
    // has a semantic layer keeps it as its sole representation, on a full
    // and on an incremental rebuild.
    for incremental in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("app.py"), "def f():\n    return 1\n").unwrap();
        let options = RebuildOptions {
            acquire_lock: false,
            no_cluster: true,
            ..Default::default()
        };
        assert!(watch::rebuild(dir.path(), &options).unwrap().success);
        let paper = dir.path().join("paper_notes.md");
        fs::write(
            &paper,
            "# Notes on a paper\n\narXiv 2501.00001\n\n## Abstract\n\nWe study X.\n",
        )
        .unwrap();
        let mut data = graph(dir.path());
        data["nodes"].as_array_mut().unwrap().extend([
            json!({"id": "paper_notes_paper", "label": "Notes on a paper", "file_type": "paper", "source_file": "paper_notes.md"}),
            json!({"id": "paper_notes_method", "label": "Method", "file_type": "concept", "source_file": "paper_notes.md"}),
        ]);
        data["links"].as_array_mut().unwrap().push(json!({"source": "paper_notes_paper", "target": "paper_notes_method", "relation": "describes", "confidence": "INFERRED", "source_file": "paper_notes.md"}));
        fs::write(dir.path().join(".graphicle/graph.json"), data.to_string()).unwrap();
        let before = data["nodes"].as_array().unwrap().len();
        let options = RebuildOptions {
            changed_paths: incremental.then(|| vec![paper.clone()]),
            ..options
        };
        watch::rebuild(dir.path(), &options).unwrap();
        let after = graph(dir.path());
        assert_eq!(
            nodes_of(&after, "paper_notes.md"),
            vec!["paper_notes_method", "paper_notes_paper"],
            "incremental={incremental}"
        );
        assert_eq!(after["nodes"].as_array().unwrap().len(), before);
    }
}
