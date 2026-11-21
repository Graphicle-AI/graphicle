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

const VARIADIC_CASES: &[(&str, &str, &str, &str)] = &[
    ("csharp", "x.cs", "class Opt {}\nclass X {\n  void Plain(Opt o) {}\n  void Vari(params Opt[] os) {}\n}\n", ".Vari()"),
    ("go", "x.go", "package p\n\ntype Opt struct{}\n\nfunc Plain(o Opt) {}\nfunc Vari(os ...Opt) {}\n", "Vari()"),
    ("java", "X.java", "class Opt {}\nclass X {\n  void plain(Opt o) {}\n  void vari(Opt... os) {}\n}\n", ".vari()"),
    ("java-generic", "Y.java", "import java.util.List;\nclass Opt {}\nclass Y {\n  void vari(List<Opt>... os) {}\n}\n", ".vari()"),
    ("php", "x.php", "<?php\nclass Opt {}\nfunction plain(Opt $o) {}\nfunction vari(Opt ...$os) {}\n", "vari()"),
    ("scala", "x.scala", "class Opt\nobject X {\n  def plain(o: Opt): Unit = ()\n  def vari(os: Opt*): Unit = ()\n}\n", ".vari()"),
    ("scala-by-name", "y.scala", "class Opt\nobject Y {\n  def lazyp(o: => Opt): Unit = ()\n}\n", ".lazyp()"),
];

#[test]
fn a_variadic_parameter_references_its_type() {
    // test_variadic_param_refs.py (#4638)
    for (case, name, src, func) in VARIADIC_CASES {
        graphicle::rcfile::set_language_overrides(None);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join(name);
        std::fs::write(&path, src).unwrap();
        let r = extract(
            &[path],
            &ExtractOptions {
                root: Some(root.clone()),
                cache_root: Some(root.join("graphify-out")),
                parallel: false,
                ..Default::default()
            },
        )
        .unwrap();
        let nodes = r["nodes"].as_array().unwrap();
        let opt = nodes.iter().filter(|n| n["label"] == "Opt").collect::<Vec<_>>();
        assert_eq!(opt.len(), 1, "{case}: {r:#}");
        let label = |id: &serde_json::Value| {
            nodes
                .iter()
                .find(|n| n["id"] == *id)
                .and_then(|n| n["label"].as_str())
                .unwrap_or("")
                .to_string()
        };
        let refs = r["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "references" && e["target"] == opt[0]["id"])
            .map(|e| (label(&e["source"]), e["context"].as_str().unwrap_or("").to_string()))
            .collect::<BTreeMap<_, _>>();
        let expected = if *case == "java-generic" { "generic_arg" } else { "parameter_type" };
        assert_eq!(refs.get(*func).map(String::as_str), Some(expected), "{case}: {refs:?}");
    }
}

#[test]
fn dart_legacy_typedef_returning_a_function_type_is_named_by_its_own_name() {
    // test_dart_typedef.py (#4641)
    let (ids, _) = ids_and_calls(
        "lib/t.dart",
        "typedef int Function(int) Make();\ntypedef void Function() Callback(String s);\ntypedef int Function(int)? MaybeMake();\ntypedef Map<String, int> Function(int a, String b) Build();\n",
    );
    for name in ["Make", "Callback", "MaybeMake", "Build"] {
        assert!(ids.contains_key(name), "{name}: {ids:?}");
    }
    assert!(!ids.contains_key("Function"), "{ids:?}");
}

#[cfg(unix)]
#[test]
fn same_basename_files_stay_distinct_under_a_symlinked_root() {
    // test_symlinked_root_source_file.py (#4624)
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let root = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &root).unwrap();
    let page = "export default function ListPage() {\n  return helper();\n}\nfunction helper() { return 1; }\n";
    let paths = ["web/app/a/page.tsx", "web/app/b/page.tsx"]
        .iter()
        .map(|rel| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, page).unwrap();
            p
        })
        .collect::<Vec<_>>();
    let r = extract(
        &paths,
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(dir.path().join("cache")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap();
    let files = r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["source_file"].as_str().map(str::to_string))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        files,
        ["web/app/a/page.tsx".to_string(), "web/app/b/page.tsx".to_string()].into_iter().collect(),
        "{r:#}"
    );
}
