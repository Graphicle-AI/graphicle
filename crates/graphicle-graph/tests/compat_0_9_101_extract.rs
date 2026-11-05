// Adapted from Graphify 0.9.101, Apache-2.0; see NOTICE.md.
//! Extraction changes ported from Graphify 0.9.101, from the upstream tests
//! of that release: Rust module-qualified calls and macro-argument calls,
//! Python namespace packages under `src/` and bare calls in methods, Go
//! method values through fields and package-var type references, and C++
//! receivers typed from parameters and fields (#3215) and local callables.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        graphicle::rcfile::set_language_overrides(None);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    fn run(&self, paths: &[PathBuf]) -> Value {
        extract(
            paths,
            &ExtractOptions {
                root: Some(self.root.clone()),
                cache_root: Some(self.root.join("graphify-out")),
                parallel: false,
                ..Default::default()
            },
        )
        .unwrap()
    }
    fn extract_files(&self, files: &[(&str, &str)]) -> Value {
        let paths = files
            .iter()
            .map(|(rel, text)| self.write(rel, text))
            .collect::<Vec<_>>();
        self.run(&paths)
    }
}

fn nodes(result: &Value) -> &Vec<Value> {
    result["nodes"].as_array().unwrap()
}
fn edges(result: &Value) -> &Vec<Value> {
    result["edges"].as_array().unwrap()
}
fn node<'a>(result: &'a Value, id: &Value) -> Option<&'a Value> {
    nodes(result).iter().find(|n| n["id"] == *id)
}
fn label_of(result: &Value, id: &Value) -> String {
    node(result, id)
        .and_then(|n| n["label"].as_str())
        .unwrap_or("")
        .to_string()
}
/// `(source label, target label)` of every edge of `relation`.
fn pairs(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| {
            (
                label_of(result, &e["source"]),
                label_of(result, &e["target"]),
            )
        })
        .collect()
}


/// `(source label, target label, confidence)` of every call edge, labels
/// without their parens.
fn calls_with_confidence(result: &Value) -> BTreeSet<(String, String, String)> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| {
            (
                label_of(result, &e["source"]).trim_matches(['(', ')']).trim_start_matches('.').to_string(),
                label_of(result, &e["target"]).trim_matches(['(', ')']).trim_start_matches('.').to_string(),
                e["confidence"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

// --- Rust module-qualified calls and macro arguments ------------------------

#[test]
fn rust_module_qualified_call_binds_the_named_module() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("src/a.rs", "pub fn run() { b::helper(); }\n"),
        ("src/b.rs", "pub fn helper() {}\n"),
        ("src/c.rs", "pub fn helper() {}\n"),
    ]);
    let calls = calls_with_confidence(&r);
    assert!(calls.contains(&("run".into(), "helper".into(), "EXTRACTED".into())), "{calls:?}");
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("src/a.rs", "pub fn run() { util::helper(); }\n"),
        ("src/util/mod.rs", "pub fn helper() {}\n"),
    ]);
    assert!(calls_with_confidence(&r).contains(&("run".into(), "helper".into(), "EXTRACTED".into())));
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("src/a.rs", "pub fn run() { b::helper(); }\n"),
        ("src/b.rs", "pub fn helper() {}\n"),
        ("src/b/mod.rs", "pub fn helper() {}\n"),
    ]);
    assert!(!calls_with_confidence(&r).iter().any(|(s, t, _)| s == "run" && t == "helper"));
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("src/a.rs", "use std::fs;\npub fn run() { fs::read(); }\n"),
        ("src/fs.rs", "pub fn read() {}\n"),
    ]);
    assert!(!calls_with_confidence(&r).iter().any(|(s, t, _)| s == "run" && t == "read"));
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("src/a.rs", "pub fn helper() {}\npub fn run() { crate::b::helper(); }\n"),
        ("src/b.rs", "pub fn helper() {}\n"),
    ]);
    let targets = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls" && label_of(&r, &e["source"]) == "run()")
        .filter_map(|e| node(&r, &e["target"]))
        .map(|n| n["source_file"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert!(targets[0].ends_with("b.rs"));
}

#[test]
fn rust_calls_inside_macro_arguments_are_linked() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "src/lib.rs",
        "fn compute(x: i32) -> i32 { x }\nfn run() { println!(\"{}\", compute(1)); assert_eq!(compute(2), 2); }\n",
    )]);
    let found = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls" && label_of(&r, &e["source"]) == "run()" && label_of(&r, &e["target"]) == "compute()")
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["context"], "macro_argument_call");
    assert_eq!(found[0]["confidence"], "INFERRED");
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "src/lib.rs",
        "fn compute(x: i32) -> i32 { x }\nfn run() { compute(0); println!(\"{}\", compute(1)); }\n",
    )]);
    let found = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls" && label_of(&r, &e["target"]) == "compute()")
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["confidence"], "EXTRACTED");
}

// --- Python -------------------------------------------------------------------

#[test]
fn python_bare_call_in_a_method_never_binds_a_method() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("pkg/api.py", "def get_build_status(x):\n    return x\n"),
        (
            "pkg/client.py",
            "from pkg.api import get_build_status\n\n\nclass AsyncTemplate:\n    def get_build_status(self, x):\n        return get_build_status(x)\n",
        ),
    ]);
    assert!(
        !edges(&r).iter().any(|e| e["relation"] == "calls" && e["source"] == e["target"]),
        "self loop"
    );
    let method = nodes(&r)
        .iter()
        .find(|n| n["label"] == ".get_build_status()")
        .unwrap();
    let targets = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls" && e["source"] == method["id"])
        .filter_map(|e| node(&r, &e["target"]))
        .map(|n| n["source_file"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert!(targets.iter().any(|f| f.ends_with("api.py")), "{targets:?}");
}

#[test]
fn python_namespace_package_under_src_resolves_imports() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("pkgsql/src/airflow/sql/hooks/sql.py", "class DbApiHook:\n    def run(self):\n        return 1\n"),
        (
            "pkgpg/src/airflow/pg/hooks/pg.py",
            "from airflow.sql.hooks.sql import DbApiHook\n\n\nclass PgHook(DbApiHook):\n    def go(self):\n        return self.run()\n",
        ),
    ]);
    assert!(pairs(&r, "inherits").contains(&("PgHook".into(), "DbApiHook".into())), "{:?}", pairs(&r, "inherits"));
    assert!(pairs(&r, "calls").contains(&(".go()".into(), ".run()".into())), "{:?}", pairs(&r, "calls"));
}

// --- Go -------------------------------------------------------------------------

#[test]
fn go_method_value_through_a_receiver_field() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "svc/server.go",
        "package svc\n\ntype Handlers struct{}\n\nfunc (h *Handlers) Health() {}\n\ntype Server struct {\n\th *Handlers\n}\n\nfunc wrap(f func()) {}\n\nfunc (s *Server) Routes() {\n\twrap(s.h.Health)\n}\n",
    )]);
    assert!(pairs(&r, "indirect_call").contains(&(".Routes()".into(), ".Health()".into())), "{:?}", pairs(&r, "indirect_call"));
}

#[test]
fn go_package_var_references_the_types_it_names() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "wire/set.go",
        "package wire\n\ntype Iface interface{}\ntype Impl struct{}\n\nfunc Bind(a, b interface{}) int { return 0 }\n\nvar Set = Bind(new(Iface), new(*Impl))\n",
    )]);
    let refs = pairs(&r, "references");
    assert!(refs.contains(&("set.go".into(), "Iface".into())) && refs.contains(&("set.go".into(), "Impl".into())), "{refs:?}");
}

// --- C++ ----------------------------------------------------------------------

#[test]
fn cpp_receivers_typed_from_parameters_and_fields() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("thing.h", "class Thing {\npublic:\n  void Touch();\n};\n"),
        (
            "use.cpp",
            "#include \"thing.h\"\nclass Holder {\n  Thing Inner;\npublic:\n  void Poke() { Inner.Touch(); }\n};\nvoid ParamRef(const Thing& t) { t.Touch(); }\n",
        ),
    ]);
    let calls = pairs(&r, "calls");
    assert!(calls.contains(&("ParamRef()".into(), ".Touch()".into())), "{calls:?}");
    assert!(calls.contains(&(".Poke()".into(), ".Touch()".into())), "{calls:?}");
}

#[test]
fn cpp_local_callable_value_stays_local() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "cb.cpp",
        "void callback() {}\nvoid run(void (*callback)()) { callback(); }\n",
    )]);
    assert!(!pairs(&r, "calls").contains(&("run()".into(), "callback()".into())), "{:?}", pairs(&r, "calls"));
}
