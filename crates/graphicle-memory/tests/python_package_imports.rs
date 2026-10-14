//! Mirrors the package/module collision cases of upstream Graphify
//! `tests/test_python_import_resolution.py` (issue #3777) against the manifest
//! graph, plus the importlib rules they depend on: a regular package shadows a
//! same-named sibling module file, and `from pkg import sub` binds the
//! submodule unless `pkg/__init__.py` binds `sub` itself.
use graphicle_memory::structure::{analyze, graph};
use serde_json::{Map, Value};

fn build(files: &[(&str, &str)]) -> Value {
    let mut analyses = Map::new();
    for (path, content) in files {
        analyses.insert((*path).to_string(), analyze(path, content).unwrap());
    }
    graph(&Value::Object(analyses)).unwrap()
}

fn calls(raw: &Value) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = raw["symbol_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            ["path", "caller", "callee_path", "callee"]
                .iter()
                .map(|k| r[k].as_str().unwrap().to_string())
                .collect()
        })
        .collect();
    out.sort();
    out
}

fn call(a: &str, b: &str, c: &str, d: &str) -> Vec<String> {
    vec![a.into(), b.into(), c.into(), d.into()]
}

fn imports(raw: &Value, source: &str, target: &str) -> bool {
    raw["edges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "imports" && e["source"] == source && e["target"] == target)
}

fn unresolved_reason(raw: &Value, callee: &str) -> String {
    raw["unresolved_calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["callee"] == callee)
        .and_then(|r| r["reason"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn test_issue_3777_nested_module_package_collision_resolves_to_submodule() {
    let raw = build(&[
        (
            "pkg/runner.py",
            "from pkg.runner.step import run\n\ndef start():\n    run()\n",
        ),
        ("pkg/runner/__init__.py", ""),
        (
            "pkg/runner/step.py",
            "from pkg.runner import helper\n\ndef run():\n    helper.work()\n",
        ),
        ("pkg/runner/helper.py", "def work():\n    pass\n"),
    ]);
    assert!(imports(&raw, "pkg/runner/step.py", "pkg/runner/helper.py"));
    assert!(!imports(&raw, "pkg/runner/step.py", "pkg/runner.py"));
    assert_eq!(
        calls(&raw),
        vec![
            call("pkg/runner.py", "start", "pkg/runner/step.py", "run"),
            call("pkg/runner/step.py", "run", "pkg/runner/helper.py", "work"),
        ]
    );
}

#[test]
fn test_issue_3777_package_module_collision_has_no_phantom_edge() {
    let raw = build(&[
        (
            "nettacker.py",
            "from nettacker.main import run\n\ndef cli():\n    run()\n",
        ),
        ("nettacker/__init__.py", ""),
        (
            "nettacker/main.py",
            "from nettacker.core.app import Nettacker\n\ndef run():\n    return Nettacker()\n",
        ),
        (
            "nettacker/core/app.py",
            "from nettacker import logger\n\nclass Nettacker:\n    def start(self):\n        logger.log_info('start')\n",
        ),
        (
            "nettacker/logger.py",
            "def log_info(msg):\n    print(msg)\n",
        ),
    ]);
    assert!(imports(
        &raw,
        "nettacker/core/app.py",
        "nettacker/logger.py"
    ));
    assert!(!imports(&raw, "nettacker/core/app.py", "nettacker.py"));
    // Graphify 0.9.69 omits the method edge: its module-call arm (#1883) maps
    // a caller to its file through `contains` edges only, which methods lack.
    assert_eq!(
        calls(&raw),
        vec![
            call("nettacker.py", "cli", "nettacker/main.py", "run"),
            call(
                "nettacker/core/app.py",
                "Nettacker.start",
                "nettacker/logger.py",
                "log_info"
            ),
            call(
                "nettacker/main.py",
                "run",
                "nettacker/core/app.py",
                "Nettacker"
            ),
        ]
    );
}

#[test]
fn test_issue_3777_namespace_package_submodule_import() {
    let raw = build(&[
        ("ns/sub.py", "def helper():\n    pass\n"),
        ("ns/other.py", "def util():\n    pass\n"),
        (
            "ns/consumer.py",
            "from ns import sub, other\n\ndef run():\n    sub.helper()\n    other.util()\n",
        ),
    ]);
    assert!(imports(&raw, "ns/consumer.py", "ns/sub.py"));
    assert!(imports(&raw, "ns/consumer.py", "ns/other.py"));
    assert_eq!(
        calls(&raw),
        vec![
            call("ns/consumer.py", "run", "ns/other.py", "util"),
            call("ns/consumer.py", "run", "ns/sub.py", "helper"),
        ]
    );
}

#[test]
fn test_issue_3777_standalone_module_import_unaffected() {
    let raw = build(&[
        ("standalone.py", "def fn():\n    return 42\n"),
        (
            "consumer.py",
            "from standalone import fn\n\ndef run():\n    fn()\n",
        ),
    ]);
    assert!(imports(&raw, "consumer.py", "standalone.py"));
    assert_eq!(
        calls(&raw),
        vec![call("consumer.py", "run", "standalone.py", "fn")]
    );
}

#[test]
fn package_binding_takes_precedence_over_submodule() {
    let raw = build(&[
        ("pkg/__init__.py", "def helper():\n    pass\n"),
        ("pkg/helper.py", "def work():\n    pass\n"),
        (
            "user.py",
            "from pkg import helper\n\ndef run():\n    helper()\n    helper.work()\n",
        ),
    ]);
    assert!(!imports(&raw, "user.py", "pkg/helper.py"));
    assert_eq!(
        calls(&raw),
        vec![call("user.py", "run", "pkg/__init__.py", "helper")]
    );
    assert_eq!(
        unresolved_reason(&raw, "helper.work"),
        "external_or_dynamic_call"
    );
}

#[test]
fn relative_submodule_import_in_init_keeps_the_submodule_binding() {
    let raw = build(&[
        ("pkg/__init__.py", "from . import helper\n"),
        ("pkg/helper.py", "def work():\n    pass\n"),
        (
            "user.py",
            "from pkg import helper as h\n\ndef run():\n    h.work()\n",
        ),
    ]);
    assert!(imports(&raw, "pkg/__init__.py", "pkg/helper.py"));
    assert_eq!(
        calls(&raw),
        vec![call("user.py", "run", "pkg/helper.py", "work")]
    );
}

#[test]
fn shadowed_submodule_binding_stays_unresolved() {
    let raw = build(&[
        ("pkg/__init__.py", ""),
        ("pkg/helper.py", "def work():\n    pass\n"),
        (
            "user.py",
            "from pkg import helper\n\ndef run(helper):\n    helper.work()\n",
        ),
    ]);
    assert!(calls(&raw).is_empty());
    assert_eq!(
        unresolved_reason(&raw, "helper.work"),
        "shadowed_or_dynamic_binding"
    );
}

#[test]
fn incomplete_package_init_does_not_bind_submodule() {
    let raw = build(&[
        ("pkg/__init__.py", "from .other import *\n"),
        ("pkg/other.py", "def helper():\n    pass\n"),
        ("pkg/helper.py", "def work():\n    pass\n"),
        (
            "user.py",
            "from pkg import helper\n\ndef run():\n    helper.work()\n",
        ),
    ]);
    assert!(!imports(&raw, "user.py", "pkg/helper.py"));
    assert!(calls(&raw).is_empty());
}
