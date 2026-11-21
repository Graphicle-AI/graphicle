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
