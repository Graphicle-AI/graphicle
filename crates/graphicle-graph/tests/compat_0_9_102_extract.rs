// Adapted from Graphify 0.9.102, Apache-2.0; see NOTICE.md.
//! Extraction changes ported from Graphify 0.9.102: Go lexical receiver
//! bindings (#4962), package-qualified receivers (#3368) and implicit
//! interface dispatch, Kotlin overloads and init blocks (#4935), Python
//! module instances, Solidity parent-relative imports and TypeScript typed
//! tagged templates.
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


// --- Go ----------------------------------------------------------------------

#[test]
fn go_same_file_member_calls_bind_by_receiver_type() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "svc/svc.go",
        "package svc\n\ntype A struct{}\n\nfunc (a *A) Run() {}\n\ntype B struct{}\n\nfunc (b *B) Run() {}\n\nfunc use(x *A) {\n\tx.Run()\n}\n",
    )]);
    let targets = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls" && label_of(&r, &e["source"]) == "use()")
        .map(|e| e["target"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert!(targets[0].contains("_a_"), "{targets:?}");
}

#[test]
fn go_local_binding_shadows_a_package_variable() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "svc/svc.go",
        "package svc\n\ntype A struct{}\n\nfunc (a *A) Run() {}\n\nvar x = &A{}\n\nfunc use() {\n\tx := make()\n\tx.Run()\n}\n\nfunc other() {\n\tx.Run()\n}\n",
    )]);
    let calls = pairs(&r, "calls");
    assert!(!calls.contains(&("use()".into(), ".Run()".into())), "{calls:?}");
    assert!(calls.contains(&("other()".into(), ".Run()".into())), "{calls:?}");
}

#[test]
fn go_qualified_receiver_binds_through_the_import_path() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("go.mod", "module example.com/m\n\ngo 1.22\n"),
        ("plugins/client.go", "package plugins\n\ntype Client struct{}\n\nfunc (c *Client) Get() {}\n"),
        ("other/client.go", "package other\n\ntype Client struct{}\n\nfunc (c *Client) Get() {}\n"),
        (
            "app/app.go",
            "package app\n\nimport \"example.com/m/plugins\"\n\nfunc Run(c *plugins.Client) {\n\tc.Get()\n}\n",
        ),
    ]);
    let targets = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls" && label_of(&r, &e["source"]) == "Run()")
        .filter_map(|e| node(&r, &e["target"]))
        .map(|n| n["source_file"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert!(targets[0].ends_with("plugins/client.go"), "{targets:?}");
}

#[test]
fn go_interface_method_dispatches_to_implicit_implementers() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "svc/svc.go",
        "package svc\n\ntype Provider interface {\n\tGet() string\n}\n\ntype Real struct{}\n\nfunc (r *Real) Get() string { return \"\" }\n\nfunc use(p Provider) {\n\tp.Get()\n}\n",
    )]);
    let dispatch = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "dispatches_to")
        .filter_map(|e| node(&r, &e["target"]))
        .map(|n| n["id"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert!(dispatch.iter().any(|id| id.contains("real")), "{dispatch:?}");
    assert!(!edges(&r).iter().any(|e| e["relation"] == "implements"));
}

// --- Kotlin, Python, Solidity, TS ------------------------------------------------

#[test]
fn kotlin_overloads_and_init_blocks() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Util.kt", "class Util {\n    fun log(x: Int) {}\n    fun log(x: String) {}\n}\n"),
        (
            "App.kt",
            "class App {\n    val u = Util()\n    init {\n        helper()\n    }\n    fun go() { u.log(1) }\n}\nfun helper() {}\n",
        ),
    ]);
    let calls = pairs(&r, "calls");
    assert!(calls.contains(&("App".into(), "helper()".into())), "{calls:?}");
}

#[test]
fn python_module_instance_method_calls_resolve() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("pkg/client.py", "class Client:\n    def send(self):\n        return 1\n"),
        (
            "pkg/app.py",
            "from pkg.client import Client\n\nclient = Client()\n\n\ndef run():\n    return client.send()\n",
        ),
    ]);
    assert!(pairs(&r, "calls").contains(&("run()".into(), ".send()".into())), "{:?}", pairs(&r, "calls"));
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("pkg/client.py", "class Client:\n    def send(self):\n        return 1\n"),
        (
            "pkg/app.py",
            "from pkg.client import Client\n\nclient = Client()\nclient = other()\n\n\ndef run():\n    return client.send()\n",
        ),
    ]);
    assert!(!pairs(&r, "calls").contains(&("run()".into(), ".send()".into())));
}

#[test]
fn solidity_parent_relative_import_names_the_real_file() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("contracts/utils/Errors.sol", "pragma solidity ^0.8.0;\nlibrary Errors {}\n"),
        ("contracts/proxy/Proxy.sol", "pragma solidity ^0.8.0;\nimport \"../utils/Errors.sol\";\ncontract Proxy {}\n"),
    ]);
    let targets = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "imports_from" || e["relation"] == "imports")
        .map(|e| e["target"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert!(!targets.iter().any(|t| t.contains("proxy_utils")), "{targets:?}");
}

#[test]
fn ts_typed_tagged_template_keeps_later_declarations() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "q.ts",
        "declare function sql<T>(s: TemplateStringsArray): T;\nconst rows = sql<{ id: number }[]>`select id from t`;\nexport function after() { return rows; }\n",
    )]);
    assert!(nodes(&r).iter().any(|n| n["label"] == "after()"), "{:?}", nodes(&r).iter().map(|n| &n["label"]).collect::<Vec<_>>());
}
