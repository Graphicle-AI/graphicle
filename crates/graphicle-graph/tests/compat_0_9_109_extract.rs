// Adapted from Graphify 0.9.109, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_kotlin_underscore_ids.py
//! Extraction changes ported from Graphify 0.9.109. Cases follow the
//! upstream tests named in each comment.
use graphicle::api::{ExtractOptions, extract};
use std::collections::{BTreeMap, BTreeSet};

fn ids_and_calls(name: &str, text: &str) -> (BTreeMap<String, Vec<String>>, BTreeSet<(String, String)>) {
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    let r = extract(
        &[path],
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("cache")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap();
    let mut ids = BTreeMap::<String, Vec<String>>::new();
    for n in r["nodes"].as_array().unwrap() {
        ids.entry(n["label"].as_str().unwrap_or("").to_string())
            .or_default()
            .push(n["id"].as_str().unwrap_or("").to_string());
    }
    let calls = r["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| (e["source"].as_str().unwrap().to_string(), e["target"].as_str().unwrap().to_string()))
        .collect();
    (ids, calls)
}

fn one(ids: &BTreeMap<String, Vec<String>>, label: &str) -> String {
    let found = ids.get(label).cloned().unwrap_or_default();
    assert_eq!(found.len(), 1, "{label}: {ids:?}");
    found[0].clone()
}

#[test]
fn kotlin_underscore_twin_keeps_its_own_node_and_calls() {
    // test_kotlin_underscore_ids.py
    let (ids, calls) = ids_and_calls(
        "demo/Foo.kt",
        "package demo\n\nclass Foo {\n    fun bar() { helper() }\n    fun _bar() { other() }\n    fun helper() {}\n    fun other() {}\n    fun caller() { _bar(); bar(); run(); Run() }\n    companion object {\n        fun run() { helper() }\n        fun Run() {}\n    }\n}\n",
    );
    let (bar, ubar) = (one(&ids, ".bar()"), one(&ids, "._bar()"));
    let (helper, other) = (one(&ids, ".helper()"), one(&ids, ".other()"));
    assert_ne!(bar, ubar);
    assert!(bar.ends_with("_foo_bar") && helper.ends_with("_foo_helper"), "{bar} {helper}");
    assert!(calls.contains(&(bar, helper)), "{calls:?}");
    assert!(calls.contains(&(ubar, other)), "{calls:?}");
    assert_ne!(one(&ids, ".run()"), one(&ids, ".Run()"));
}

#[test]
fn dart_underscore_twins_keep_their_own_nodes() {
    // test_dart_underscore_ids.py
    let (ids, _) = ids_and_calls(
        "lib/foo.dart",
        "class Foo {\n  void bar() { ref.watch(helperProvider); }\n  void _bar() { ref.watch(otherProvider); }\n  void helper() {}\n}\nclass Other {\n  void _helper() { context.push('/home'); }\n}\n",
    );
    let (bar, ubar) = (one(&ids, "bar"), one(&ids, "_bar"));
    assert_ne!(bar, ubar);
    assert_eq!(bar, "lib_foo_bar");
    assert_ne!(one(&ids, "helper"), one(&ids, "_helper"));
}
