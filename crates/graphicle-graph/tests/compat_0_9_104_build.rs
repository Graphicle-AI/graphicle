// Adapted from Graphify 0.9.104, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_build.py, tests/test_global_manifest_shape.py,
// tests/test_learning_overlay_input.py, tests/test_tree_html.py
//! Build, global manifest, reflect and tree changes ported from Graphify
//! 0.9.104. Cases follow the upstream tests named in each comment.
use graphicle::build::{graph_has_legacy_ids, graph_has_legacy_ids_with, legacy_id_source_prefix};
use serde_json::json;

#[test]
fn graph_has_legacy_ids_honors_the_scan_root_prefix() {
    // test_build.py test_graph_has_legacy_ids_honors_the_scan_root_prefix (#5028)
    let fresh = [json!({"id": "pkg_mod", "source_file": "sub/pkg/mod.py", "type": "code", "source_location": "L1"})];
    assert!(graph_has_legacy_ids(&fresh, None, 300)); // the false positive
    assert!(!graph_has_legacy_ids_with(&fresh, None, 300, Some("sub")));
    assert!(!graph_has_legacy_ids_with(&fresh, None, 300, Some("sub/")));
    // A genuinely pre-#1504 id under the same prefix is still caught.
    let old = [json!({"id": "v1_readme", "source_file": "sub/docs/v1/README.md", "type": "document", "source_location": "L1"})];
    assert!(graph_has_legacy_ids_with(&old, None, 300, Some("sub")));
    // A source_file outside the prefix is judged as stored.
    let other = [json!({"id": "docs_v1_readme", "source_file": "docs/v1/README.md", "type": "document", "source_location": "L1"})];
    assert!(!graph_has_legacy_ids_with(&other, None, 300, Some("sub")));
}

#[test]
fn legacy_id_source_prefix_reads_the_relative_marker() {
    // test_build.py test_legacy_id_source_prefix_reads_the_relative_marker
    for (marker, expected) in [
        ("sub", Some("sub")),
        ("./a/b/", Some("a/b")),
        ("a\\b", Some("a/b")),
        (".", None),
        ("", None),
        ("../elsewhere", None),
        ("/abs/scan/root", None),
        ("C:\\scan\\root", None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("graphify-out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join(".graphify_root"), marker).unwrap();
        assert_eq!(
            legacy_id_source_prefix(&out.join("graph.json")).as_deref(),
            expected,
            "{marker}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(legacy_id_source_prefix(&dir.path().join("graph.json")), None);
}

#[test]
fn learning_overlay_ignores_non_object_root() {
    // test_learning_overlay_input.py test_learning_overlay_ignores_non_object_root (#5019)
    use graphicle::reflect::{LEARNING_SIDECAR_NAME, load_learning_overlay};
    for payload in [json!(null), json!([]), json!(7), json!("unexpected")] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LEARNING_SIDECAR_NAME), payload.to_string()).unwrap();
        assert_eq!(
            load_learning_overlay(&dir.path().join("graph.json")).unwrap(),
            json!({}),
            "{payload}"
        );
    }
}

#[test]
fn learning_overlay_keeps_valid_entries_beside_malformed_source() {
    // test_learning_overlay_input.py (#5019, #5024)
    use graphicle::reflect::{LEARNING_SIDECAR_NAME, load_learning_overlay};
    let dir = tempfile::tempdir().unwrap();
    let payload = json!({"nodes": {
        "bad": {"source_file": ["app.py"], "status": "preferred"},
        "good": {"status": "preferred", "score": 2},
        "legacy": {"source_file": null, "status": "preferred"},
        "gone": {"source_file": "deleted.py", "status": "preferred"},
    }});
    std::fs::write(dir.path().join(LEARNING_SIDECAR_NAME), payload.to_string()).unwrap();
    assert_eq!(
        load_learning_overlay(&dir.path().join("graph.json")).unwrap(),
        json!({
            "good": {"status": "preferred", "score": 2, "stale": false},
            "legacy": {"source_file": null, "status": "preferred", "stale": false},
            "gone": {"source_file": "deleted.py", "status": "preferred", "stale": true},
        })
    );
}

#[test]
fn global_list_backs_up_wrong_manifest_shape() {
    // test_global_manifest_shape.py test_global_list_backs_up_wrong_manifest_shape
    for payload in [json!(null), json!([]), json!({}), json!({"repos": []})] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = graphicle::global_graph::GlobalStore::new(dir.path().to_path_buf());
        std::fs::write(&store.manifest_path, payload.to_string()).unwrap();
        assert_eq!(store.list().unwrap(), json!({}), "{payload}");
        assert!(!store.manifest_path.exists());
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("global-manifest.json.corrupt.")
            })
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(backups[0].path()).unwrap(),
            payload.to_string()
        );
        assert!(store.warnings.iter().any(|w| w.contains("moved to")), "{:?}", store.warnings);
    }
}

#[test]
fn global_list_isolates_malformed_repo_entries() {
    // test_global_manifest_shape.py (#5020)
    let dir = tempfile::tempdir().unwrap();
    let mut store = graphicle::global_graph::GlobalStore::new(dir.path().to_path_buf());
    let good = json!({"source_path": "graph.json", "node_count": 2});
    std::fs::write(
        &store.manifest_path,
        json!({"version": 1, "repos": {"goodA": good, "bad": "oops", "goodB": good}}).to_string(),
    )
    .unwrap();
    assert_eq!(store.list().unwrap(), json!({"goodA": good, "goodB": good}));
    assert!(store.manifest_path.is_file());
    let warning = store.warnings.join("\n");
    assert!(warning.contains("dropped 1 malformed repo") && warning.contains("bad"), "{warning}");

    let dir = tempfile::tempdir().unwrap();
    let mut store = graphicle::global_graph::GlobalStore::new(dir.path().to_path_buf());
    std::fs::write(&store.manifest_path, json!({"repos": {"legacy": "invalid"}}).to_string()).unwrap();
    assert_eq!(store.list().unwrap(), json!({}));
    assert!(store.manifest_path.is_file());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn build_tree_accepts_src_and_file_aliases() {
    // test_tree_html.py test_build_tree_accepts_src_and_file_aliases (#5030)
    let graph = json!({"nodes": [
        {"id": "a", "label": "fn_a", "src": "pkg/sub/a.py"},
        {"id": "b", "label": "fn_b", "file": "pkg/sub/b.py"},
    ]});
    let tree = graphicle::tree_html::build_tree(&graph, None, 200, None).unwrap();
    assert_ne!(tree["name"], "(empty graph)", "{tree:#}");
    assert_eq!(tree["total_count"], 2, "{tree:#}");
}

// Graphify 0.9.105 ---------------------------------------------------------

fn wiki_graph(first: &str, second: &str) -> graphicle::graph::Graph {
    let mut data = json!({
        "nodes": [
            {"id": "n1", "label": first, "file_type": "code", "source_file": "a.py"},
            {"id": "n2", "label": second, "file_type": "code", "source_file": "b.py"},
        ],
        "edges": [{"source": "n1", "target": "n2", "relation": "calls", "confidence": "EXTRACTED", "weight": 1.0}],
        "hyperedges": [],
    });
    let mut diagnostics = graphicle::build::Diagnostics::default();
    graphicle::build_from_json(&mut data, true, None, &mut diagnostics).unwrap()
}

fn wiki_articles(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".md") && n != "index.md")
        .map(|n| {
            let text = std::fs::read_to_string(dir.join(&n)).unwrap();
            (n, text)
        })
        .collect()
}

#[test]
fn to_wiki_unicode_case_fold_collision_keeps_both_articles() {
    // test_wiki.py test_to_wiki_unicode_case_fold_collision_keeps_both_articles
    // and ..._repeat_export_is_stable (#5087)
    for (first, second) in [("Straße", "STRASSE"), ("Σ", "ς")] {
        let g = wiki_graph(first, second);
        let dir = tempfile::tempdir().unwrap();
        let communities = json!({"0": ["n1"], "1": ["n2"]});
        let labels = json!({"0": first, "1": second});
        let run = || {
            graphicle::to_wiki(
                &g,
                &communities,
                dir.path(),
                labels.as_object().unwrap(),
                &serde_json::Map::new(),
                &[],
            )
            .unwrap()
        };
        let (count, _) = run();
        assert_eq!(count, 2);
        let after_first = wiki_articles(dir.path());
        let headings = after_first
            .values()
            .map(|t| t.lines().next().unwrap_or("").to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            headings,
            [format!("# {first}"), format!("# {second}")].into_iter().collect(),
            "{after_first:?}"
        );
        run();
        assert_eq!(wiki_articles(dir.path()), after_first);
    }
}
