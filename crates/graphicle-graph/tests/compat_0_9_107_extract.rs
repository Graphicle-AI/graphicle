// Adapted from Graphify 0.9.107, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_extract_php_closures.py
//! Extraction changes ported from Graphify 0.9.107. Cases follow the
//! upstream tests named in each comment.
use graphicle::api::{ExtractOptions, extract};
use std::collections::BTreeSet;

fn labels(name: &str, text: &str) -> BTreeSet<String> {
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join(name);
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
    r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn php_route_path_comes_only_from_arguments_before_the_closure() {
    // test_extract_php_closures.py (#5135)
    let l = labels("late_path.php", "<?php\n$app->get(function () { return 1; }, '/late');\n");
    assert!(!l.iter().any(|x| x.starts_with("GET ")), "{l:?}");
    let l = labels(
        "late_prefix.php",
        "<?php\n$app->group(function ($group) {\n    $group->get('/users', function () { return 1; });\n}, '/late');\n",
    );
    assert!(l.contains("GET /users()"), "{l:?}");
    assert!(!l.contains("GET /late/users()"), "{l:?}");
}

const UNDERSCORE_CASES: &[(&str, &str)] = &[
    ("a.c", "int _save(void) { return 1; }\nint save(void) { return _save(); }\nint caller(void) { return save(); }\n"),
    ("a.cpp", "int _save() { return 1; }\nint save() { return _save(); }\nint caller() { return save(); }\n"),
    ("a.js", "function _save() { return 1 }\nfunction save() { return _save() }\nfunction caller() { return save() }\n"),
    ("a.kt", "fun _save(): Int = 1\nfun save(): Int = _save()\nfun caller(): Int = save()\n"),
    ("a.lua", "function _save() return 1 end\nfunction save() return _save() end\nfunction caller() return save() end\n"),
    ("a.php", "<?php\nfunction _save() { return 1; }\nfunction save() { return _save(); }\nfunction caller() { return save(); }\n"),
    ("a.rb", "def _save; 1; end\ndef save; _save; end\ndef caller; save; end\n"),
    ("a.swift", "func _save() -> Int { 1 }\nfunc save() -> Int { _save() }\nfunc caller() -> Int { save() }\n"),
    ("a.ts", "function _save(): number { return 1 }\nfunction save(): number { return _save() }\nfunction caller(): number { return save() }\n"),
    ("arrow.ts", "const _save = (): number => 1\nconst save = (): number => _save()\nconst caller = (): number => save()\n"),
];

fn extract_one(name: &str, text: &str) -> serde_json::Value {
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join(name);
    std::fs::write(&path, text).unwrap();
    extract(
        &[path],
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("cache")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn underscore_twins_are_two_nodes_with_their_own_calls() {
    // test_module_level_underscore_decl_ids.py (#2810)
    for (name, text) in UNDERSCORE_CASES {
        let r = extract_one(name, text);
        let by_label = r["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| Some((n["label"].as_str()?.to_string(), n["id"].as_str()?.to_string())))
            .collect::<std::collections::BTreeMap<_, _>>();
        for label in ["_save()", "save()", "caller()"] {
            assert!(by_label.contains_key(label), "{name}: {by_label:?}");
        }
        assert_ne!(by_label["_save()"], by_label["save()"], "{name}");
        let calls = r["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls")
            .map(|e| (e["source"].as_str().unwrap().to_string(), e["target"].as_str().unwrap().to_string()))
            .collect::<BTreeSet<_>>();
        assert!(!calls.iter().any(|(a, b)| a == b), "{name}: {calls:?}");
        assert!(calls.contains(&(by_label["save()"].clone(), by_label["_save()"].clone())), "{name}: {calls:?}");
        assert!(calls.contains(&(by_label["caller()"].clone(), by_label["save()"].clone())), "{name}: {calls:?}");
    }
}

#[test]
fn registry_salts_a_same_kind_underscore_respelling() {
    use graphicle::ids::DeclIdRegistry;
    let mut taken = BTreeSet::<String>::new();
    let mut registry = DeclIdRegistry::default();
    assert_eq!(registry.resolve("a_save", "_save", "fn", |id| taken.contains(id)), "a_save");
    taken.insert("a_save".into());
    let plain = registry.resolve("a_save", "save", "fn", |id| taken.contains(id));
    assert_ne!(plain, "a_save");
    taken.insert(plain.clone());
    assert_eq!(registry.resolve("a_save", "save", "fn", |id| taken.contains(id)), plain);
    assert_eq!(registry.resolve("a_save", "_save", "fn", |id| taken.contains(id)), "a_save");
    assert_eq!(registry.lookup("save"), Some(plain));
    assert_eq!(registry.lookup("_save"), Some("a_save".to_string()));

    let mut taken = BTreeSet::<String>::new();
    let mut registry = DeclIdRegistry::default();
    assert_eq!(registry.resolve("a_foo", "Foo", "type", |id| taken.contains(id)), "a_foo");
    taken.insert("a_foo".into());
    assert_eq!(registry.resolve("a_foo", "FOO", "type", |id| taken.contains(id)), "a_foo");
}

fn ids_and_calls(name: &str, text: &str) -> (std::collections::BTreeMap<String, Vec<String>>, BTreeSet<(String, String)>) {
    let r = extract_one(name, text);
    let mut ids = std::collections::BTreeMap::<String, Vec<String>>::new();
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

fn one(ids: &std::collections::BTreeMap<String, Vec<String>>, label: &str) -> String {
    let found = ids.get(label).cloned().unwrap_or_default();
    assert_eq!(found.len(), 1, "{label}: {ids:?}");
    found[0].clone()
}

#[test]
fn php_underscore_method_twins_are_two_nodes() {
    // Graphify 0.9.107 (6a69b73d; #2810)
    let (ids, calls) = ids_and_calls(
        "b.php",
        "<?php\nclass B {\n  private function _x() { return 1; }\n  public function x() { return $this->_x(); }\n  public function y() { return $this->x(); }\n}\n",
    );
    let (ux, x, y) = (one(&ids, "._x()"), one(&ids, ".x()"), one(&ids, ".y()"));
    assert_ne!(ux, x);
    assert!(calls.contains(&(x.clone(), ux)), "{calls:?}");
    assert!(calls.contains(&(y, x.clone())), "{calls:?}");
    assert!(!calls.contains(&(x.clone(), x)), "{calls:?}");
}

#[test]
fn java_underscore_and_case_twins_keep_their_own_nodes_and_calls() {
    // test_java_underscore_ids.py (Graphify 0.9.107)
    let (ids, calls) = ids_and_calls(
        "Foo.java",
        "package demo;\n\npublic class Foo {\n    public void bar() { helper(); }\n    private void _bar() { other(); }\n    void helper() {}\n    void other() {}\n    void Run() {}\n    void run() {}\n    public void caller() { _bar(); bar(); run(); Run(); }\n}\n",
    );
    let (bar, ubar) = (one(&ids, ".bar()"), one(&ids, "._bar()"));
    let (helper, other, caller) = (one(&ids, ".helper()"), one(&ids, ".other()"), one(&ids, ".caller()"));
    assert_ne!(bar, ubar);
    assert!(bar.ends_with("_foo_bar") && helper.ends_with("_foo_helper"), "{bar} {helper}");
    for pair in [(&bar, &helper), (&ubar, &other), (&caller, &bar), (&caller, &ubar)] {
        assert!(calls.contains(&(pair.0.clone(), pair.1.clone())), "{pair:?}: {calls:?}");
    }
    assert!(!calls.contains(&(bar.clone(), other.clone())));
    let (run, big_run) = (one(&ids, ".run()"), one(&ids, ".Run()"));
    assert_ne!(run, big_run);
    assert!(calls.contains(&(caller.clone(), run)) && calls.contains(&(caller, big_run)));
}
