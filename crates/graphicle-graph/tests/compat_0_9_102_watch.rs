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

// --- test_update_incremental.py, test_update_lock_contention.py (Graphify
// 0.9.102, #4740) ---

/// Four code files, one of which imports across files.
fn corpus(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    for name in ["a", "b", "c"] {
        fs::write(
            root.join(format!("src/{name}.py")),
            format!("def f{name}():\n    return 1\n"),
        )
        .unwrap();
    }
    fs::write(
        root.join("src/uses.py"),
        "from src.a import fa\n\ndef use():\n    return fa()\n",
    )
    .unwrap();
}

fn update(root: &Path, force: bool) -> watch::RebuildResult {
    let result = watch::rebuild(
        root,
        &RebuildOptions {
            incremental: true,
            force,
            no_cluster: true,
            acquire_lock: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.success, "{:?}", result.messages);
    result
}

fn incremental_line(result: &watch::RebuildResult) -> Option<&str> {
    result
        .messages
        .iter()
        .map(String::as_str)
        .find(|m| m.starts_with("Incremental:"))
}

/// Node ids and `(source, target, relation)` of a stored graph.
fn shape(root: &Path) -> (Vec<String>, Vec<(String, String, String)>) {
    let data = graph(root);
    let mut nodes: Vec<String> = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect();
    nodes.sort();
    let mut edges: Vec<(String, String, String)> = data["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["source"].as_str().unwrap_or_default().to_string(),
                e["target"].as_str().unwrap_or_default().to_string(),
                e["relation"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    edges.sort();
    (nodes, edges)
}

#[test]
fn update_re_extracts_only_changed_files_and_matches_a_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    corpus(root);
    // The first update has no manifest: the full rebuild.
    assert_eq!(incremental_line(&update(root, false)), None);
    fs::write(
        root.join("src/b.py"),
        "def fb():\n    return 2\n\ndef fb2():\n    return fb()\n",
    )
    .unwrap();
    let r = update(root, false);
    assert_eq!(
        incremental_line(&r),
        Some("Incremental: re-extracting 1 of 4 scanned files that changed since the last build.")
    );
    let incremental = shape(root);
    // A forced update is the full rebuild, and writes the same graph.
    assert_eq!(incremental_line(&update(root, true)), None);
    assert_eq!(shape(root), incremental);
}

#[test]
fn update_skips_a_file_whose_content_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    corpus(root);
    update(root, false);
    let before = fs::read(root.join(".graphicle/graph.json")).unwrap();
    // Same bytes, a new modification time.
    let text = fs::read_to_string(root.join("src/c.py")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(root.join("src/c.py"), text).unwrap();
    let r = update(root, false);
    if let Some(line) = incremental_line(&r) {
        assert_eq!(
            line,
            "Incremental: none of the 4 scanned files differ from the last build."
        );
    }
    assert_eq!(fs::read(root.join(".graphicle/graph.json")).unwrap(), before);
}

#[test]
fn update_handles_added_and_deleted_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    corpus(root);
    update(root, false);
    fs::write(root.join("src/d.py"), "def fd():\n    return 4\n").unwrap();
    fs::remove_file(root.join("src/c.py")).unwrap();
    let r = update(root, false);
    assert_eq!(
        incremental_line(&r),
        Some("Incremental: re-extracting 2 of 4 scanned files that changed since the last build.")
    );
    let data = graph(root);
    assert!(nodes_of(&data, "c.py").is_empty());
    assert!(!nodes_of(&data, "d.py").is_empty());
    let incremental = shape(root);
    update(root, true);
    assert_eq!(shape(root), incremental);
}

#[test]
fn update_without_a_trusted_baseline_is_the_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    corpus(root);
    update(root, false);
    fs::write(root.join("src/a.py"), "def fa():\n    return 3\n").unwrap();
    // A manifest another command wrote (its stats differ from the baseline).
    let manifest = root.join(".graphicle/manifest.json");
    let text = fs::read_to_string(&manifest).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&manifest, format!("{text}\n")).unwrap();
    assert_eq!(incremental_line(&update(root, false)), None);
    // No manifest at all.
    fs::write(root.join("src/a.py"), "def fa():\n    return 4\n").unwrap();
    fs::remove_file(&manifest).unwrap();
    assert_eq!(incremental_line(&update(root, false)), None);
    // --force.
    fs::write(root.join("src/a.py"), "def fa():\n    return 5\n").unwrap();
    assert_eq!(incremental_line(&update(root, true)), None);
}

#[test]
fn explicit_changed_paths_win_over_incremental() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    corpus(root);
    update(root, false);
    fs::write(root.join("src/a.py"), "def fa():\n    return 9\n").unwrap();
    let r = watch::rebuild(
        root,
        &RebuildOptions {
            incremental: true,
            changed_paths: Some(vec![root.join("src/a.py")]),
            no_cluster: true,
            acquire_lock: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(r.success, "{:?}", r.messages);
    assert_eq!(incremental_line(&r), None);
}

#[test]
fn a_contended_incremental_request_is_recorded_not_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    corpus(root);
    update(root, false);
    let out = root.join(".graphicle");
    let lock = watch::RebuildLock::acquire(&out, true).unwrap().unwrap();
    let busy = watch::rebuild(
        root,
        &RebuildOptions {
            incremental: true,
            no_cluster: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!busy.success);
    assert!(busy.messages[0].contains("changes queued"), "{:?}", busy.messages);
    assert!(out.join(".pending_incremental").exists());
    assert!(!out.join(".pending_full").exists());
    drop(lock);
    // The next holder drains it and runs an incremental pass.
    fs::write(root.join("src/b.py"), "def fb():\n    return 7\n").unwrap();
    let done = watch::rebuild(
        root,
        &RebuildOptions {
            no_cluster: true,
            block_on_lock: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(done.success, "{:?}", done.messages);
    assert!(!out.join(".pending_incremental").exists());
    assert_eq!(
        incremental_line(&done),
        Some("Incremental: re-extracting 1 of 4 scanned files that changed since the last build.")
    );
}

#[test]
fn a_code_only_graph_stays_code_only_on_rebuilds() {
    // test_code_only_scope_4923.py (Graphify 0.9.101, #4923): the scope of
    // `extract --code-only` is recorded and kept by update.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("app.py"), "def f():\n    return 1\n").unwrap();
    fs::write(root.join("README.md"), "# Notes\n\n## Usage\n\nRun it.\n").unwrap();
    let args: Vec<String> = ["extract", root.to_str().unwrap(), "--code-only", "--no-cluster"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let r = graphicle::cli::run(&args);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    assert!(watch::build_flag(&root.join(".graphicle"), "code_only"));
    let result = watch::rebuild(
        root,
        &RebuildOptions {
            force: true,
            no_cluster: true,
            acquire_lock: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.success, "{:?}", result.messages);
    assert!(nodes_of(&graph(root), "README.md").is_empty());
    assert!(!nodes_of(&graph(root), "app.py").is_empty());
}
