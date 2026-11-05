// Adapted from Graphify 0.9.96, Apache-2.0; see NOTICE.md.
//! Extraction changes ported from Graphify 0.9.96, from the upstream tests
//! of that release: Python member calls bind the method they spell (#4734)
//! and only Python definitions, injected fields and method results type a
//! receiver (#4726), a Rust `use` is scoped to the crate it names (#4699),
//! Zig call shapes, the unnamed Dart extension, duplicate Robot test cases,
//! VB.NET sourceless stubs (#4725), an external CommonJS `require` (#4688),
//! chained TS decorators (#4711) and the XAML codebehind order.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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
fn calls_edge<'a>(result: &'a Value, source: &str, target: &str) -> Option<&'a Value> {
    edges(result)
        .iter()
        .find(|e| e["relation"] == "calls" && e["source"] == source && e["target"] == target)
}

// --- Python member calls bind the method they spell (#4734) -------------

const WORKER: &str = "class Worker:\n    def run(self):\n        return 1\n\n    def _run(self):\n        return 2\n";

fn method_hits(r: &Value, caller: &str) -> BTreeSet<String> {
    pairs(r, "calls")
        .into_iter()
        .filter(|(s, t)| s == caller && t.starts_with('.'))
        .map(|(_, t)| t)
        .collect()
}

#[test]
fn python_member_calls_hit_the_spelled_method() {
    for (call, target) in [
        ("Worker.run(None)", ".run()"),
        ("Worker._run(None)", "._run()"),
    ] {
        let f = Fixture::new();
        let r = f.extract_files(&[
            ("worker.py", WORKER),
            (
                "main.py",
                &format!("from worker import Worker\n\n\ndef viaclass():\n    return {call}\n"),
            ),
        ]);
        assert_eq!(
            method_hits(&r, "viaclass()"),
            BTreeSet::from([target.to_string()])
        );
    }
    for (call, target) in [("w.run()", ".run()"), ("w._run()", "._run()")] {
        let f = Fixture::new();
        let r = f.extract_files(&[
            ("worker.py", WORKER),
            (
                "main.py",
                &format!(
                    "from worker import Worker\n\n\ndef typed(w: Worker):\n    return {call}\n"
                ),
            ),
        ]);
        assert_eq!(
            method_hits(&r, "typed()"),
            BTreeSet::from([target.to_string()])
        );
    }
    for (call, target) in [("self.run()", ".run()"), ("self._run()", "._run()")] {
        let f = Fixture::new();
        let r = f.extract_files(&[
            ("base.py", &WORKER.replace("Worker", "Base")),
            (
                "child.py",
                &format!(
                    "from base import Base\n\n\nclass Child(Base):\n    def start(self):\n        return {call}\n"
                ),
            ),
        ]);
        assert_eq!(
            method_hits(&r, ".start()"),
            BTreeSet::from([target.to_string()])
        );
    }
}

#[test]
fn python_member_call_matching_no_method_exactly_is_not_guessed() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("worker.py", WORKER),
        (
            "main.py",
            "from worker import Worker\n\n\ndef viaclass():\n    return Worker.Run(None)\n",
        ),
    ]);
    assert!(method_hits(&r, "viaclass()").is_empty());
    let f = Fixture::new();
    let r = f.extract_files(&[
        (
            "worker.py",
            "class Worker:\n    def run(self):\n        return 1\n",
        ),
        (
            "main.py",
            "from worker import Worker\n\n\ndef viaclass():\n    return Worker.run(None)\n",
        ),
    ]);
    assert!(pairs(&r, "calls").contains(&("viaclass()".into(), ".run()".into())));
}

#[test]
fn python_member_calls_bind_python_definitions_only() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("Inner.swift", "class Inner {\n    func go() {}\n}\n"),
        ("main.py", "def caller():\n    return Inner.go()\n"),
    ]);
    assert!(
        !pairs(&r, "calls").contains(&("caller()".into(), ".go()".into())),
        "{:?}",
        pairs(&r, "calls")
    );
}

// --- Python injected fields and method results (#4726) --------------------

const MODELS: &str = "class Task:\n    def complete(self): pass\n";
const REPOSITORY: &str = "from models import Task\nclass TaskRepository:\n    def add(self, title) -> Task: return Task()\n    def get(self, key) -> Task: return Task()\n    def all(self) -> list[Task]: return []\n";
const SERVICES: &str = "from models import Task\nfrom repository import TaskRepository\nclass TaskService:\n    def __init__(self, repository: TaskRepository):\n        self.repository = repository\n    def create_task(self, title):\n        return self.repository.add(title)\n    def complete_task(self, key):\n        task = self.repository.get(key)\n        task.complete()\n    def pending_tasks(self):\n        return self.repository.all()\n";

fn task_project(models: &str, repository: &str, services: &str) -> Value {
    let f = Fixture::new();
    f.extract_files(&[
        ("models.py", models),
        ("repository.py", repository),
        ("services.py", services),
    ])
}

#[test]
fn python_injected_parameter_types_reach_member_calls() {
    let r = task_project(MODELS, REPOSITORY, SERVICES);
    for (caller, callee, line) in [
        ("create_task", "add", "L7"),
        ("complete_task", "get", "L9"),
        ("pending_tasks", "all", "L12"),
    ] {
        let e = calls_edge(
            &r,
            &format!("services_taskservice_{caller}"),
            &format!("repository_taskrepository_{callee}"),
        )
        .unwrap_or_else(|| panic!("{caller} -> {callee}"));
        assert_eq!(e["confidence"], "INFERRED");
        assert_eq!(e["source_location"], line);
    }
    let e = calls_edge(
        &r,
        "services_taskservice_complete_task",
        "models_task_complete",
    )
    .expect("the annotated method result types the local");
    assert_eq!(e["confidence"], "INFERRED");
    assert_eq!(e["source_location"], "L10");
}

#[test]
fn python_unknown_rebound_and_shadowed_results_do_not_guess() {
    let head = SERVICES.split("    def complete_task").next().unwrap();
    for body in [
        "        task = self.repository.get(key)\n        task = unknown()\n        task.complete()\n",
        "        task = wrap(self.repository.get(key))\n        task.complete()\n",
        "        task = unknown.get(self.repository.get(key))\n        task.complete()\n",
        "        task = self.repository.get(key)\n        return lambda task: task.complete()\n",
        "        task = self.repository.get(key)\n        task = self.repository.get(key)\n        task.complete()\n",
    ] {
        let services = format!("{head}    def complete_task(self, key):\n{body}");
        let r = task_project(MODELS, REPOSITORY, &services);
        assert!(
            calls_edge(
                &r,
                "services_taskservice_complete_task",
                "models_task_complete"
            )
            .is_none(),
            "{body}"
        );
    }
    for annotation in [
        "",
        " -> list[Task]",
        " -> Task | Other",
        " -> Task | int",
        " -> Task | Unknown",
    ] {
        let repository = REPOSITORY.replace(
            "    def get(self, key) -> Task:",
            &format!("    def get(self, key){annotation}:"),
        ) + "class Other:\n    def complete(self): pass\n";
        let r = task_project(MODELS, &repository, SERVICES);
        assert!(
            calls_edge(
                &r,
                "services_taskservice_complete_task",
                "models_task_complete"
            )
            .is_none(),
            "{annotation:?}"
        );
    }
    let untyped = SERVICES.replace("repository: TaskRepository", "repository");
    let r = task_project(MODELS, REPOSITORY, &untyped);
    assert!(
        calls_edge(
            &r,
            "services_taskservice_create_task",
            "repository_taskrepository_add"
        )
        .is_none()
    );
    let rebound = SERVICES.replace(
        "        self.repository = repository",
        "        repository = unknown()\n        self.repository = repository",
    );
    let r = task_project(MODELS, REPOSITORY, &rebound);
    assert!(
        calls_edge(
            &r,
            "services_taskservice_create_task",
            "repository_taskrepository_add"
        )
        .is_none()
    );
}

// --- Rust `use` scoped to the crate it names (#4699) -----------------------

const ENUM_A: &str = "pub enum Field {\n    Value(String),\n}\n";
const STRUCT_VALUE: &str = "pub struct Value;\n";
const USE_VALUE: &str = "pub fn get(v: &Value) -> Option<&Value> {\n    v.get(\"x\")\n}\n";

fn rust_run(f: &Fixture, files: &[(&str, &str)], manifests: bool) -> Value {
    for (rel, text) in files {
        f.write(rel, text);
    }
    if manifests {
        for rel in files
            .iter()
            .map(|(rel, _)| *rel)
            .filter(|r| r.ends_with(".rs"))
        {
            let krate = rel.split('/').next().unwrap();
            let manifest = f.root.join(krate).join("Cargo.toml");
            if !manifest.exists() {
                std::fs::write(&manifest, format!("[package]\nname = \"{krate}\"\n")).unwrap();
            }
        }
    }
    let mut paths = vec![];
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    walk(&f.root, &mut paths);
    paths.sort();
    f.run(&paths)
}
fn crate_of(file: Option<&str>) -> Option<String> {
    let file = file.filter(|f| !f.is_empty())?;
    Some(file.replace('\\', "/").split('/').next()?.to_string())
}
fn between<'a>(r: &'a Value, src: &str, dst: &str) -> Vec<&'a Value> {
    edges(r)
        .iter()
        .filter(|e| {
            !["contains", "case_of", "method"].contains(&e["relation"].as_str().unwrap_or(""))
        })
        .filter(|e| {
            crate_of(node(r, &e["source"]).and_then(|n| n["source_file"].as_str())).as_deref()
                == Some(src)
                && crate_of(node(r, &e["target"]).and_then(|n| n["source_file"].as_str()))
                    .as_deref()
                    == Some(dst)
        })
        .collect()
}

#[test]
fn rust_external_crate_use_does_not_reach_a_local_crate() {
    for decl in [ENUM_A, STRUCT_VALUE] {
        let f = Fixture::new();
        let r = rust_run(
            &f,
            &[
                ("a/src/lib.rs", decl),
                (
                    "b/src/lib.rs",
                    &format!("use serde_json::Value;\n\n{USE_VALUE}"),
                ),
            ],
            true,
        );
        assert!(between(&r, "b", "a").is_empty());
    }
    let f = Fixture::new();
    let r = rust_run(
        &f,
        &[("a/src/lib.rs", ENUM_A), ("b/src/lib.rs", USE_VALUE)],
        true,
    );
    assert!(between(&r, "b", "a").is_empty());
    let f = Fixture::new();
    let r = rust_run(
        &f,
        &[
            ("a/src/lib.rs", "pub struct HashMap;\n"),
            (
                "b/src/lib.rs",
                "use std::collections::HashMap;\n\npub fn f(m: &HashMap<u8, u8>) {}\n",
            ),
        ],
        true,
    );
    assert!(between(&r, "b", "a").is_empty());
}

#[test]
fn rust_workspace_crate_use_reaches_the_named_crate() {
    let f = Fixture::new();
    let r = rust_run(
        &f,
        &[
            ("a/src/lib.rs", STRUCT_VALUE),
            ("c/src/lib.rs", STRUCT_VALUE),
            ("b/src/lib.rs", &format!("use a::Value;\n\n{USE_VALUE}")),
        ],
        true,
    );
    assert!(!between(&r, "b", "a").is_empty());
    assert!(between(&r, "b", "c").is_empty());
    let f = Fixture::new();
    let r = rust_run(
        &f,
        &[
            ("osiris-types/src/lib.rs", "pub struct Config;\n"),
            ("server/src/lib.rs", "pub struct Config;\n"),
            (
                "plugin/src/lib.rs",
                "use osiris_types::Config;\n\npub fn f(c: &Config) {}\n",
            ),
        ],
        true,
    );
    assert!(!between(&r, "plugin", "osiris-types").is_empty());
    assert!(between(&r, "plugin", "server").is_empty());
}

#[test]
fn rust_crate_less_binding_is_inferred_and_a_scoped_one_extracted() {
    let f = Fixture::new();
    let r = rust_run(
        &f,
        &[
            ("a/src/lib.rs", STRUCT_VALUE),
            (
                "b/src/lib.rs",
                &format!("use serde_json::Value;\n\n{USE_VALUE}"),
            ),
        ],
        false,
    );
    let found = between(&r, "b", "a");
    assert!(!found.is_empty());
    assert!(found.iter().all(|e| e["confidence"] == "INFERRED"));
    let f = Fixture::new();
    let r = rust_run(
        &f,
        &[
            (
                "b/src/lib.rs",
                &format!("mod model;\nuse crate::model::Value;\n\n{USE_VALUE}"),
            ),
            ("b/src/model.rs", STRUCT_VALUE),
        ],
        true,
    );
    let found = between(&r, "b", "b");
    assert!(!found.is_empty());
    assert!(found.iter().all(|e| e["confidence"] == "EXTRACTED"));
}

#[test]
fn rust_reexports_reach_the_defining_crate_only_when_public() {
    let f = Fixture::new();
    let r = rust_run(
        &f,
        &[
            ("corelib/src/lib.rs", "pub struct FromRequestParts;\n"),
            ("other/src/lib.rs", "pub struct FromRequestParts;\n"),
            ("api/src/lib.rs", "pub use corelib::FromRequestParts;\n"),
            (
                "plugin/src/lib.rs",
                "use api::FromRequestParts;\n\npub fn f(x: &FromRequestParts) {}\n",
            ),
        ],
        true,
    );
    assert!(!between(&r, "plugin", "corelib").is_empty());
    assert!(between(&r, "plugin", "other").is_empty());
    let f = Fixture::new();
    let r = rust_run(
        &f,
        &[
            ("corelib/src/lib.rs", "pub struct FromRequestParts;\n"),
            (
                "api/src/lib.rs",
                "pub(crate) use corelib::FromRequestParts;\n",
            ),
            (
                "plugin/src/lib.rs",
                "use api::FromRequestParts;\n\npub fn f(x: &FromRequestParts) {}\n",
            ),
        ],
        true,
    );
    assert!(between(&r, "plugin", "corelib").is_empty());
    assert!(
        nodes(&r).iter().all(|n| n.get("_rust_use_roots").is_none())
            && edges(&r).iter().all(|e| e.get("_rust_pub_use").is_none())
    );
}

// --- Single-language fixes --------------------------------------------------

#[test]
fn zig_call_shape_picks_method_or_free_function() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "main.zig",
        "const S = struct {\n    fn helper(self: S) void { _ = self; }\n};\nfn helper() void {}\nfn caller() void {\n    helper();\n}\n",
    )]);
    let calls = pairs(&r, "calls");
    assert!(
        calls.contains(&("caller()".into(), "helper()".into())),
        "{calls:?}"
    );
    assert!(
        !calls.contains(&("caller()".into(), ".helper()".into())),
        "{calls:?}"
    );
}

#[test]
fn dart_unnamed_extension_is_detected() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "ext.dart",
        "extension on String {\n  int get twice => length * 2;\n}\n",
    )]);
    assert!(
        nodes(&r)
            .iter()
            .any(|n| n["label"] == "Extension on String"),
        "{:?}",
        nodes(&r).iter().map(|n| &n["label"]).collect::<Vec<_>>()
    );
}

#[test]
fn robot_duplicate_test_cases_get_their_own_nodes() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "suite.robot",
        "*** Test Cases ***\nSame Name\n    Log    one\nSame Name\n    Log    two\nOther\n    Log    three\n",
    )]);
    let same = nodes(&r)
        .iter()
        .filter(|n| n["label"] == "Same Name")
        .collect::<Vec<_>>();
    assert_eq!(same.len(), 2);
    assert_ne!(same[0]["id"], same[1]["id"]);
    assert!(nodes(&r).iter().any(|n| n["id"] == "suite_other"));
}

#[test]
fn vbnet_external_namespaces_are_sourceless_stubs() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        (
            "Caller.vb",
            "Imports System.Collections.Generic\nPublic Class Caller\n Inherits ExternalBase\n Public Function Run() As String\n  Return \"x\"\n End Function\nEnd Class\n",
        ),
        (
            "Other.vb",
            "Imports System.Collections.Generic\nPublic Class Other\n Inherits ExternalBase\nEnd Class\n",
        ),
    ]);
    let namespaces = nodes(&r)
        .iter()
        .filter(|n| n["label"] == "System.Collections.Generic")
        .collect::<Vec<_>>();
    assert_eq!(
        namespaces
            .iter()
            .filter_map(|n| n["id"].as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        1
    );
    assert!(
        namespaces
            .iter()
            .all(|n| n["source_file"] == "" && n["source_location"] == "")
    );
}

#[test]
fn js_require_of_an_external_package_keeps_its_binding_node() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "app.js",
        "const Plugin = require('some-plugin');\nclass Mine extends Plugin {}\nmodule.exports = { Mine };\n",
    )]);
    assert!(
        edges(&r)
            .iter()
            .any(|e| e["relation"] == "inherits" && label_of(&r, &e["source"]) == "Mine"),
        "{:?}",
        pairs(&r, "inherits")
    );
}

#[test]
fn ts_parenthesized_decorator_chain_names_the_outermost_call() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("link.ts", "export function toMany() { return () => {}; }\nexport function Link() { return { toMany }; }\n"),
        (
            "model.ts",
            "import { Link, toMany } from './link';\nexport class Model {\n  @(Link().toMany())\n  items: string[] = [];\n}\n",
        ),
    ]);
    let decorated = edges(&r)
        .iter()
        .filter(|e| label_of(&r, &e["target"]) == "toMany()")
        .filter(|e| label_of(&r, &e["source"]) == "Model")
        .count();
    assert!(decorated > 0, "{:?}", pairs(&r, "references"));
}
