// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
//! Python external-module call resolution ported from Graphify 0.9.76
//! (6868fff, 33edd89; `tests/test_python_import_resolution.py` and
//! `tests/test_extract.py`). `module.func()` on a plain `import module` that
//! no parsed file defines records one `calls` edge to the module stub; any
//! way the receiver could be rebound fails closed.
use graphicle::api::{ExtractOptions, extract};
use graphicle::extract_members::python_members;
use serde_json::{Value, json};
use std::path::PathBuf;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }
    /// On Windows the root without the verbatim prefix, as the command line
    /// resolves it; elsewhere [`Fixture::new`]'s.
    fn plain() -> Self {
        let fx = Self::new();
        match fx.root.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            Some(plain) if cfg!(windows) && plain.as_bytes().get(1) == Some(&b':') => Self {
                root: plain.into(),
                ..fx
            },
            _ => fx,
        }
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    fn extract(&self, paths: &[PathBuf]) -> Value {
        let options = ExtractOptions {
            root: Some(self.root.clone()),
            cache_root: Some(self.root.clone()),
            parallel: false,
            ..Default::default()
        };
        extract(paths, &options).unwrap()
    }
}

fn extract_caller(source: &str) -> Value {
    let fx = Fixture::new();
    let path = fx.write("caller.py", source);
    fx.extract(&[path])
}

fn calls_to<'a>(result: &'a Value, target: &str) -> Vec<&'a Value> {
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls" && e["target"] == target)
        .collect()
}

fn node_id(result: &Value, label: &str, source_file: &str) -> String {
    let matches = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["label"] == label && n["source_file"] == source_file)
        .filter_map(|n| n["id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "{label} in {source_file}: {matches:?}");
    matches[0].to_string()
}

/// `(import line, receiver, module stub)`.
const POSITIVE: [(&str, &str, &str); 9] = [
    ("import requests", "requests", "requests"),
    ("import requests as rq", "rq", "requests"),
    ("import requests as Requests", "Requests", "requests"),
    (
        "import requests\nfrom requests import Session",
        "requests",
        "requests",
    ),
    (
        "from requests import Session; import requests",
        "requests",
        "requests",
    ),
    (
        "from other import value; import requests as rq",
        "rq",
        "requests",
    ),
    (
        "import requests as Requests\nfrom requests import Session",
        "Requests",
        "requests",
    ),
    ("import os", "os", "os"),
    ("import pkg.sub as sub", "sub", "pkg_sub"),
];

#[test]
fn external_plain_import_member_call_targets_module() {
    for (import_line, receiver, target) in POSITIVE {
        let result = extract_caller(&format!(
            "{import_line}\n\ndef fetch():\n    return {receiver}.get('/data')\n\n\
             def unrelated():\n    return 1\n"
        ));
        let caller = node_id(&result, "fetch()", "caller.py");
        let calls = calls_to(&result, target);
        assert_eq!(calls.len(), 1, "{import_line:?}: {calls:?}");
        let call = calls[0];
        assert_eq!(call["source"], caller.as_str(), "{import_line:?}");
        assert_eq!(call["source_file"], "caller.py", "{import_line:?}");
        let line = format!("L{}", import_line.lines().count() + 3);
        assert_eq!(
            [
                &call["confidence"],
                &call["confidence_score"],
                &call["context"],
                &call["source_location"],
                &call["weight"],
            ],
            [
                &json!("EXTRACTED"),
                &json!(1.0),
                &json!("call"),
                &json!(line),
                &json!(1.0)
            ],
            "{import_line:?}"
        );
        assert!(
            !result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["label"] == "get()"),
            "{import_line:?}"
        );
        let text = result.to_string();
        assert!(
            !text.contains("_python_receiver_shadowed"),
            "{import_line:?}"
        );
        assert!(!text.contains("_python_plain_import"), "{import_line:?}");
    }
}

/// Each body follows `import requests`; every one may rebind `requests`.
const SHADOWS: [&str; 42] = [
    "def fetch(requests):\n    return requests.get()\n",
    "def fetch(*, requests=None):\n    return requests.get()\n",
    "def fetch():\n    requests = object()\n    return requests.get()\n",
    "def fetch():\n    requests, x = pair\n    return requests.get()\n",
    "def fetch():\n    requests += 1\n    return requests.get()\n",
    "def fetch():\n    (requests := object())\n    return requests.get()\n",
    "def fetch():\n    for requests in xs: pass\n    return requests.get()\n",
    "def fetch():\n    xs = [requests.get() for requests in things]\n    return requests.get()\n",
    "def fetch():\n    with open('x') as requests: pass\n    return requests.get()\n",
    "def fetch():\n    try: pass\n    except Exception as requests: pass\n    return requests.get()\n",
    "def fetch():\n    def requests(): pass\n    return requests.get()\n",
    "def fetch():\n    class requests: pass\n    return requests.get()\n",
    "def fetch():\n    del requests\n    return requests.get()\n",
    "def fetch():\n    requests.value = 1\n    return requests.get()\n",
    "def fetch():\n    import requests\n    return requests.get()\n",
    "def fetch():\n    global requests\n    return requests.get()\n",
    "def fetch():\n    nonlocal requests\n    return requests.get()\n",
    "def fetch():\n    match x:\n        case requests: pass\n    return requests.get()\n",
    "def fetch():\n    return (lambda requests: requests.get())(object())\n",
    "def fetch():\n    return (lambda *, requests=None: requests.get())()\n",
    "def fetch():\n    return (lambda **requests: requests.get())()\n",
    "requests = object()\ndef fetch():\n    return requests.get()\n",
    "type requests = object\ndef fetch():\n    return requests.get()\n",
    "def fetch():\n    type requests[T] = list[T]\n    return requests.get()\n",
    "def fetch[requests]():\n    return requests.get()\n",
    "import builtins\nbuiltins.exec('requests = object()')\ndef fetch():\n    return requests.get()\n",
    "import builtins as bi\nbi.eval('globals().update(requests=object())')\ndef fetch():\n    return requests.get()\n",
    "from builtins import exec as run\nrun('requests = object()')\ndef fetch():\n    return requests.get()\n",
    "import builtins\nrun = builtins.exec\nrun('requests = object()')\ndef fetch():\n    return requests.get()\n",
    "run = exec\nrun('requests = object()')\ndef fetch():\n    return requests.get()\n",
    "def outer():\n    requests = object()\n    def fetch():\n        return requests.get()\n",
    "if flag:\n    import requests\ndef fetch():\n    return requests.get()\n",
    "from elsewhere import *\ndef fetch():\n    return requests.get()\n",
    "def fetch():\n    exec('requests = other')\n    return requests.get()\n",
    "def fetch():\n    eval('globals().__setitem__(\\\"requests\\\", other)')\n    return requests.get()\n",
    "def fetch():\n    globals()['requests'] = other\n    return requests.get()\n",
    "def fetch():\n    locals()['requests'] = other\n    return requests.get()\n",
    "def fetch():\n    setattr(requests, 'get', other)\n    return requests.get()\n",
    "def fetch():\n    delattr(requests, 'get')\n    return requests.get()\n",
    "def mutate():\n    global requests\n    requests = other\ndef fetch():\n    return requests.get()\n",
    "import requests\ndef fetch():\n    return requests.get()\n",
    "import other as requests\ndef fetch():\n    return requests.get()\n",
];

#[test]
fn external_module_fallback_rejects_receiver_shadow() {
    for body in SHADOWS {
        let result = extract_caller(&format!("import requests\n{body}"));
        assert!(calls_to(&result, "requests").is_empty(), "{body:?}");
    }
}

const FROM_ONLY: [&str; 2] = [
    "from requests import Session",
    "from other import value as requests",
];

#[test]
fn external_module_fallback_rejects_from_only_import() {
    for declaration in FROM_ONLY {
        let result = extract_caller(&format!(
            "{declaration}\ndef fetch():\n    return requests.get()\n"
        ));
        assert!(calls_to(&result, "requests").is_empty(), "{declaration:?}");
        assert!(calls_to(&result, "other").is_empty(), "{declaration:?}");
    }
}

#[test]
fn external_aliased_import_fabricates_no_member_node() {
    let fx = Fixture::new();
    let caller = fx.write(
        "app.py",
        "import numpy as np\nfrom os import path as p\n\n\
         def build(rows):\n    p.join('a', 'b')\n    return np.array(rows)\n",
    );
    let result = fx.extract(&[caller]);
    let label = |id: &Value| {
        result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| &n["id"] == id)
            .and_then(|n| n["label"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let fabricated = result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls" || e["relation"] == "indirect_call")
        .filter(|e| {
            let target = label(&e["target"]);
            target.contains("array") || target.contains("join")
        })
        .collect::<Vec<_>>();
    assert!(fabricated.is_empty(), "{fabricated:?}");
}

fn raw_call(receiver: &str, shadowed: Option<Value>) -> Value {
    let mut raw = json!({
        "caller_nid": "caller_fetch", "callee": "get", "receiver": receiver,
        "is_member_call": true, "source_file": "caller.py", "source_location": "L4",
    });
    if let Some(shadowed) = shadowed {
        raw["_python_receiver_shadowed"] = shadowed;
    }
    raw
}

fn caller_nodes() -> Vec<Value> {
    vec![
        json!({"id": "caller", "label": "caller.py", "source_file": "caller.py"}),
        json!({"id": "caller_fetch", "label": "fetch()", "source_file": "caller.py"}),
    ]
}

fn contains_fetch() -> Value {
    json!({"source": "caller", "target": "caller_fetch", "relation": "contains"})
}

fn resolve(raw: Value, nodes: &mut [Value], edges: &mut Vec<Value>) -> Vec<Value> {
    python_members(&[json!({"raw_calls": [raw]})], nodes, edges);
    edges
        .iter()
        .filter(|e| e["relation"] == "calls")
        .cloned()
        .collect()
}

#[test]
fn external_resolver_emits_for_unique_plain_import_and_false_fact() {
    let mut nodes = caller_nodes();
    let mut edges = vec![
        contains_fetch(),
        json!({"source": "caller", "target": "requests", "relation": "imports",
               "_python_plain_import": true}),
    ];
    let calls = resolve(
        raw_call("requests", Some(json!(false))),
        &mut nodes,
        &mut edges,
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["source"], "caller_fetch");
    assert_eq!(calls[0]["target"], "requests");
}

#[test]
fn external_resolver_requires_explicit_false_shadow_fact() {
    for hint in [None, Some(json!(0)), Some(json!(1)), Some(json!("false"))] {
        let mut nodes = caller_nodes();
        let mut edges = vec![
            contains_fetch(),
            json!({"source": "caller", "target": "requests", "relation": "imports",
                   "_python_plain_import": true}),
        ];
        let calls = resolve(raw_call("requests", hint.clone()), &mut nodes, &mut edges);
        assert!(calls.is_empty(), "{hint:?}: {calls:?}");
    }
}

/// (receiver, plain imports as (target, alias), producer hint)
type ImportCase<'a> = (&'a str, &'a [(&'a str, Option<&'a str>)], Option<Value>);

#[test]
fn external_resolver_requires_unique_exact_plain_import() {
    let cases: [ImportCase<'_>; 8] = [
        (
            "Requests",
            &[("requests", Some("requests"))],
            Some(json!(true)),
        ),
        ("Request", &[("requests", None)], Some(json!(true))),
        ("requests", &[], Some(json!(true))),
        (
            "requests",
            &[("requests", None), ("other", Some("requests"))],
            Some(json!(true)),
        ),
        ("requests", &[("requests", None)], None),
        ("requests", &[("requests", None)], Some(json!(false))),
        ("requests", &[("requests", None)], Some(json!(1))),
        ("requests", &[("requests", None)], Some(json!("true"))),
    ];
    for (receiver, imports, producer_hint) in cases {
        let mut nodes = caller_nodes();
        let mut edges = vec![contains_fetch()];
        for (target, alias) in imports {
            let mut edge = json!({"source": "caller", "target": target, "relation": "imports",
                                  "source_file": "caller.py", "source_location": "L1"});
            if let Some(hint) = &producer_hint {
                edge["_python_plain_import"] = hint.clone();
            }
            if let Some(alias) = alias {
                edge["local_alias"] = json!(alias);
            }
            edges.push(edge);
        }
        let calls = resolve(
            raw_call(receiver, Some(json!(false))),
            &mut nodes,
            &mut edges,
        );
        assert!(
            calls.is_empty(),
            "{receiver} {imports:?} {producer_hint:?}: {calls:?}"
        );
    }
}

#[test]
fn parsed_module_never_falls_back_to_coarse_call() {
    for member_count in [0, 2] {
        let mut nodes = caller_nodes();
        nodes.push(json!({"id": "helper", "label": "helper.py", "source_file": "helper.py"}));
        let mut edges = vec![
            contains_fetch(),
            json!({"source": "caller", "target": "helper", "relation": "imports",
                   "_python_plain_import": true}),
        ];
        for number in 0..member_count {
            let id = format!("helper_get_{number}");
            nodes.push(json!({"id": id, "label": "get()", "source_file": "helper.py"}));
            edges.push(json!({"source": "helper", "target": id, "relation": "contains"}));
        }
        let calls = resolve(
            raw_call("helper", Some(json!(false))),
            &mut nodes,
            &mut edges,
        );
        assert!(calls.is_empty(), "{member_count}: {calls:?}");
    }
}

#[test]
fn external_module_call_survives_cache() {
    let fx = Fixture::new();
    let source = fx.write(
        "caller.py",
        "import requests\n\ndef fetch():\n    return requests.get('/data')\n",
    );
    let cold = fx.extract(std::slice::from_ref(&source));
    let warm = fx.extract(std::slice::from_ref(&source));
    let (cold_calls, warm_calls) = (calls_to(&cold, "requests"), calls_to(&warm, "requests"));
    assert_eq!(cold_calls.len(), 1, "{cold_calls:?}");
    assert_eq!(cold_calls, warm_calls);
    let mut data = cold.clone();
    let mut diagnostics = graphicle::build::Diagnostics::default();
    let graph = graphicle::build_from_json(&mut data, true, None, &mut diagnostics).unwrap();
    let stub = graph.node_index(&json!("requests")).expect("requests stub");
    assert_eq!(
        graph.nodes()[stub].attributes.get("external"),
        Some(&json!(true))
    );
    let edges = graph.edges_between(&json!("caller_fetch"), &json!("requests"));
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].attributes.get("relation"), Some(&json!("calls")));
    let shadow = fx.write(
        "shadow.py",
        "import requests\n\ndef fetch(requests):\n    return requests.get('/data')\n",
    );
    for _ in 0..2 {
        let result = fx.extract(std::slice::from_ref(&shadow));
        assert!(calls_to(&result, "requests").is_empty());
    }
}

/// A changed caller shares an external stub with an unchanged caller.
#[test]
fn external_calls_survive_real_incremental_context() {
    let fx = Fixture::plain();
    fx.write(
        "ext_a.py",
        "import requests\n\ndef fetch_a():\n    return requests.get('/a')\n",
    );
    fx.write(
        "ext_b.py",
        "import requests as rq\n\ndef fetch_b():\n    return rq.post('/b')\n",
    );
    fx.write("helper.py", "def other():\n    return 1\n");
    fx.write(
        "local_caller.py",
        "import helper\n\ndef fetch_local():\n    return helper.get()\n",
    );
    let run = || {
        let out = graphicle::cli::run(&[
            "extract".to_string(),
            fx.root.to_string_lossy().into_owned(),
            "--code-only".to_string(),
        ]);
        assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
        let text = std::fs::read_to_string(fx.root.join(".graphicle/graph.json")).unwrap();
        assert!(!text.contains("_python_receiver_shadowed"));
        assert!(!text.contains("_python_plain_import"));
        let graph: Value = serde_json::from_str(&text).unwrap();
        let calls = graph["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls")
            .collect::<Vec<_>>();
        assert!(
            !calls
                .iter()
                .any(|e| e["source"] == "local_caller_fetch_local")
        );
        let stub = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "requests")
            .expect("requests stub");
        assert_eq!(stub["external"], true);
        assert_eq!(stub["source_file"], "");
        let mut rows = calls
            .iter()
            .map(|e| {
                [&e["source"], &e["target"], &e["source_location"]]
                    .map(|v| v.as_str().unwrap_or_default().to_string())
            })
            .collect::<Vec<_>>();
        rows.sort();
        (out.stdout, rows)
    };
    let row = |s: &str, t: &str, l: &str| [s, t, l].map(str::to_string);
    let (full_stdout, full_calls) = run();
    assert!(
        full_stdout.contains("4 code and 0 semantic files found"),
        "{full_stdout}"
    );
    assert_eq!(
        full_calls,
        [
            row("ext_a_fetch_a", "requests", "L4"),
            row("ext_b_fetch_b", "requests", "L4")
        ]
    );
    fx.write(
        "ext_a.py",
        "import requests\n\ndef fetch_a():\n    # changed caller\n    return requests.get('/a')\n",
    );
    fx.write(
        "local_caller.py",
        "import helper\n\ndef fetch_local():\n    # changed local consumer\n    return helper.get()\n",
    );
    let (incremental_stdout, incremental_calls) = run();
    assert!(
        incremental_stdout.contains("2 code and 0 semantic files changed (incremental)"),
        "{incremental_stdout}"
    );
    assert_eq!(
        incremental_calls,
        [
            row("ext_a_fetch_a", "requests", "L5"),
            row("ext_b_fetch_b", "requests", "L4")
        ]
    );
}
