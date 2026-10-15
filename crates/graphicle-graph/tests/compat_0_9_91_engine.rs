// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
//! Generic-engine changes ported from Graphify 0.9.89-0.9.91: generic and
//! template type parameters of Kotlin, Scala, C++ and Swift are local
//! bindings (no sourceless stubs), a file-scope JS callback's own parameters
//! shadow corpus names (#4448), TS class fields on their class node, and the
//! PHP receiver table.
use graphicle::api::{ExtractOptions, extract};
use graphicle::extractors::{engine::extract_generic, engine_config::config};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn run(language: &str, name: &str, source: &str) -> Value {
    extract_generic(
        Path::new(name),
        &config(language).unwrap(),
        Some(source.as_bytes()),
    )
}

fn corpus(files: &[(&str, &str)]) -> Value {
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let paths = files
        .iter()
        .map(|(rel, text)| {
            let path = root.join(rel);
            std::fs::write(&path, text).unwrap();
            path
        })
        .collect::<Vec<PathBuf>>();
    extract(
        &paths,
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("graphify-out")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap()
}

fn array<'a>(result: &'a Value, key: &str) -> &'a Vec<Value> {
    static EMPTY: Vec<Value> = Vec::new();
    result[key].as_array().unwrap_or(&EMPTY)
}

fn sourceless(result: &Value) -> Vec<String> {
    array(result, "nodes")
        .iter()
        .filter(|n| n["source_file"].as_str().unwrap_or("").is_empty())
        .map(|n| n["label"].as_str().unwrap_or("").to_string())
        .collect()
}

fn reference_targets(result: &Value) -> BTreeSet<String> {
    let label = |id: &Value| {
        array(result, "nodes")
            .iter()
            .find(|n| n["id"] == *id)
            .and_then(|n| n["label"].as_str())
            .unwrap_or("")
            .to_string()
    };
    array(result, "edges")
        .iter()
        .filter(|e| e["relation"] == "references")
        .map(|e| label(&e["target"]))
        .collect()
}

// ── Generic type parameters (0.9.89) ────────────────────────────────────────

#[test]
fn kotlin_type_parameters_mint_no_stub() {
    let r = run(
        "kotlin",
        "K.kt",
        "class Box<T>(val v: T) {\n    fun get(): T? = v\n    fun <R> map(f: (T) -> R): R? = null\n}\n",
    );
    assert_eq!(sourceless(&r), Vec::<String>::new());
    let r = run(
        "kotlin",
        "K.kt",
        "class User(val name: String)\nclass Box<T>(val v: T)\nfun wrap(u: User): Box<User> = Box(u)\n",
    );
    let refs = reference_targets(&r);
    assert!(refs.contains("User") && refs.contains("Box"), "{refs:?}");
    assert!(!refs.contains("T"));
}

#[test]
fn scala_type_parameters_mint_no_stub() {
    let r = run(
        "scala",
        "S.scala",
        "class Box[T](v: T) {\n  def get: T = v\n  def map[R](f: T => R): R = f(v)\n}\nclass Cov[+A](a: A)\n",
    );
    assert_eq!(sourceless(&r), Vec::<String>::new());
    assert!(
        !reference_targets(&r)
            .iter()
            .any(|t| ["T", "R", "A"].contains(&t.as_str()))
    );
}

#[test]
fn cpp_template_parameters_mint_no_stub() {
    let r = run(
        "cpp",
        "b.cpp",
        "template<typename T>\nclass Box {\n    T value;\npublic:\n    T get() { return value; }\n};\ntemplate<typename K, typename V>\nV lookup(K key) { V v; return v; }\n",
    );
    assert_eq!(sourceless(&r), Vec::<String>::new());
    let r = run(
        "cpp",
        "b.cpp",
        "struct User { int id; };\ntemplate<typename T, int N>\nclass Buffer {\n    T data[N];\npublic:\n    User owner() { return User(); }\n};\n",
    );
    let refs = reference_targets(&r);
    assert!(refs.contains("User"), "{refs:?}");
    assert!(!refs.contains("T"));
}

#[test]
fn swift_type_parameters_mint_no_stub() {
    let r = run(
        "swift",
        "S.swift",
        "struct Stack<Element> {\n    var items: [Element] = []\n    func top() -> Element? { return items.last }\n    func map<R>(_ f: (Element) -> R) -> [R] { return [] }\n}\n",
    );
    assert_eq!(sourceless(&r), Vec::<String>::new());
}

// ── JS file-scope callbacks (0.9.91, #4448) ─────────────────────────────────

fn indirect(a_js: &str) -> BTreeSet<(String, String)> {
    let r = corpus(&[
        ("a.js", a_js),
        ("b.js", "function value(x) { return x; }\n"),
    ]);
    let label = |id: &Value| {
        array(&r, "nodes")
            .iter()
            .find(|n| n["id"] == *id)
            .and_then(|n| n["label"].as_str())
            .unwrap_or("")
            .to_string()
    };
    array(&r, "edges")
        .iter()
        .filter(|e| e["relation"] == "indirect_call")
        .map(|e| (label(&e["source"]), label(&e["target"])))
        .collect()
}

#[test]
fn file_scope_callback_parameters_shadow_corpus_names() {
    for src in [
        "items.forEach(function (value) { s.on(value); });\n",
        "items.forEach(function (value) { emit(value); });\n",
        "m.c(function (s) { items.forEach(function (value) { s.on(value); }); });\n",
        "items.forEach(function () { var value = 1; emit(value); });\n",
    ] {
        assert_eq!(indirect(src), BTreeSet::new(), "{src}");
    }
    let unbound = BTreeSet::from([("a.js".to_string(), "value()".to_string())]);
    assert_eq!(
        indirect("items.forEach(function (x) { emit(value); });\n"),
        unbound
    );
    assert_eq!(
        indirect("m.c(function (value) { emit(1); }, function () { emit(value); });\n"),
        unbound
    );
}

// ── TS class fields (0.9.91) ────────────────────────────────────────────────

#[test]
fn ts_class_fields_ride_on_their_class_node() {
    let r = run(
        "ts",
        "svc.ts",
        "class A {\n  repo: Repo;\n  static shared = new Cache();\n  constructor(private log: Logger, other: Other) { this.other = other; }\n  get store(): Store { return this.s; }\n  run() { this.repo.save(); }\n}\nclass B {\n  repo: Other;\n}\n",
    );
    let node = |label: &str| {
        array(&r, "nodes")
            .iter()
            .find(|n| n["label"] == label)
            .unwrap()
            .clone()
    };
    assert_eq!(
        node("A")["_ts_fields"],
        serde_json::json!({"log": "Logger", "other": "Other", "repo": "Repo", "store": "Store"})
    );
    assert_eq!(
        node("A")["_ts_static_fields"],
        serde_json::json!({"shared": "Cache"})
    );
    assert_eq!(
        node("B")["_ts_fields"],
        serde_json::json!({"repo": "Other"})
    );
    // Fields are no longer in the file-wide table.
    let table = &r["ts_type_table"]["table"];
    assert!(table.get("repo").is_none(), "{table}");
    let call = array(&r, "raw_calls")
        .iter()
        .find(|c| c["callee"] == "save")
        .unwrap();
    assert_eq!(call["ts_this_field"], true);
    assert_eq!(call["receiver"], "repo");
}

#[test]
fn ts_this_destructuring_reads_the_field() {
    let r = run(
        "ts",
        "svc.ts",
        "class A {\n  creator: Creator;\n  run() { const { creator } = this; creator.make(); }\n  static go() { const { creator: c } = this; c.make(); }\n}\n",
    );
    let calls = array(&r, "raw_calls")
        .iter()
        .filter(|c| c["callee"] == "make")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls
            .iter()
            .all(|c| c["receiver"] == "creator" && c["ts_this_field"] == true)
    );
    assert_eq!(
        calls.iter().filter(|c| c["ts_static_this"] == true).count(),
        1
    );
}

// ── PHP receiver table (0.9.90) ─────────────────────────────────────────────

#[test]
fn php_receiver_table_and_member_receivers() {
    let r = run(
        "php",
        "App.php",
        "<?php\nclass App {\n    private Greeter $g;\n    private ?Logger $log;\n    private A|B $u;\n    public function __construct(private Store $store) {}\n    public function run(Client $c, $x) {\n        $n = new Notifier();\n        $this->g->greet();\n        $c->send();\n        $this->a->b->c();\n        Helper::format();\n    }\n}\n",
    );
    assert_eq!(
        r["php_type_table"]["table"],
        serde_json::json!({"c": "Client", "g": "Greeter", "log": "Logger", "n": "Notifier", "store": "Store"})
    );
    let call = |callee: &str| {
        array(&r, "raw_calls")
            .iter()
            .find(|c| c["callee"] == callee)
            .unwrap_or_else(|| panic!("{callee}: {r:#}"))
            .clone()
    };
    assert_eq!(call("greet")["receiver"], "g");
    assert_eq!(call("greet")["php_scoped"], false);
    assert_eq!(call("send")["receiver"], "c");
    assert!(call("c")["receiver"].is_null());
    assert_eq!(call("format")["receiver"], "Helper");
    assert_eq!(call("format")["php_scoped"], true);
}
