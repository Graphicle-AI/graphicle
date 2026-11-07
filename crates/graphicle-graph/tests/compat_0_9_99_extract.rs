// Adapted from Graphify 0.9.99, Apache-2.0; see NOTICE.md.
//! Extraction changes ported from Graphify 0.9.99, from the upstream tests
//! of that release: Go package-level var initialisers (#2360), method
//! values passed as arguments (#1566), receiver fields typed from a sibling
//! file (#3368), and Python dispatch tables of `module.func` values (#1566).
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

fn id_by_label(result: &Value, label: &str, suffix: &str) -> String {
    let found = nodes(result)
        .iter()
        .filter(|n| n["label"].as_str().unwrap_or("").trim_end_matches("()") == label)
        .filter(|n| {
            n["source_file"]
                .as_str()
                .unwrap_or("")
                .replace('\\', "/")
                .ends_with(suffix)
        })
        .map(|n| n["id"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1, "{label} {suffix}: {found:?}");
    found[0].clone()
}
fn id_pairs(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| {
            (
                e["source"].as_str().unwrap_or("").to_string(),
                e["target"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

// --- Python dispatch tables of `module.func` values (#1566) -----------------

const PAYMENTS: [(&str, &str); 3] = [
    ("shop/payments/__init__.py", ""),
    (
        "shop/payments/card.py",
        "def card_charge(amount):\n    return amount\n\n\nclass CardGateway:\n    pass\n",
    ),
    (
        "shop/payments/cash.py",
        "def cash_charge(amount):\n    return amount\n",
    ),
];

#[test]
fn python_dispatch_table_of_module_attributes_links_each_function() {
    let f = Fixture::new();
    let mut files = PAYMENTS.to_vec();
    files.push((
        "shop/checkout.py",
        "from payments import card\nimport payments.cash as cash\n\n\ndef pay(provider, amount):\n    table = {\"card\": card.card_charge, \"cash\": cash.cash_charge}\n    return table[provider](amount)\n",
    ));
    let r = f.extract_files(&files);
    let pay = id_by_label(&r, "pay", "checkout.py");
    let card = id_by_label(&r, "card_charge", "card.py");
    let cash = id_by_label(&r, "cash_charge", "cash.py");
    let indirect = id_pairs(&r, "indirect_call");
    assert!(
        indirect.contains(&(pay.clone(), card.clone())),
        "{indirect:?}"
    );
    assert!(indirect.contains(&(pay.clone(), cash.clone())));
    let calls = id_pairs(&r, "calls");
    assert!(!calls.contains(&(pay.clone(), card)) && !calls.contains(&(pay.clone(), cash)));
    for e in edges(&r)
        .iter()
        .filter(|e| e["relation"] == "indirect_call" && e["source"] == pay.as_str())
    {
        assert_eq!(
            (e["confidence"].as_str(), e["context"].as_str()),
            (Some("INFERRED"), Some("collection"))
        );
        assert_eq!(e["confidence_score"], 0.85);
    }
}

#[test]
fn python_module_level_registry_links_from_the_file() {
    let f = Fixture::new();
    let mut files = PAYMENTS.to_vec();
    files.push((
        "shop/registry.py",
        "from payments import card, cash\n\nPROVIDERS = [card.card_charge, cash.cash_charge]\n",
    ));
    let r = f.extract_files(&files);
    let registry = nodes(&r)
        .iter()
        .find(|n| n["label"] == "registry.py")
        .map(|n| n["id"].as_str().unwrap().to_string())
        .unwrap();
    let indirect = id_pairs(&r, "indirect_call");
    assert!(
        indirect.contains(&(registry.clone(), id_by_label(&r, "card_charge", "card.py"))),
        "{indirect:?}"
    );
    assert!(indirect.contains(&(registry, id_by_label(&r, "cash_charge", "cash.py"))));
}

// --- Go: package vars, method values and sibling-file struct fields --------

#[test]
fn go_package_level_var_initialiser_calls_are_the_files() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "svc/svc.go",
        "package svc\n\nfunc newRegistry() int { return 1 }\n\nvar (\n\tregistry = newRegistry()\n\tother    = 2\n)\n",
    )]);
    assert!(
        pairs(&r, "calls").contains(&("svc.go".into(), "newRegistry()".into())),
        "{:?}",
        pairs(&r, "calls")
    );
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "svc/svc.go",
        "package svc\n\nfunc helper() int { return 1 }\n\nfunc run() {\n\tvar x = helper()\n\t_ = x\n}\n",
    )]);
    let calls = pairs(&r, "calls");
    assert!(
        calls.contains(&("run()".into(), "helper()".into())),
        "{calls:?}"
    );
    assert!(!calls.contains(&("svc.go".into(), "helper()".into())));
}

#[test]
fn go_method_value_argument_is_an_indirect_call() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "svc/server.go",
        "package svc\n\ntype Server struct{}\n\nfunc (s *Server) Health() {}\n\nfunc wrap(f func()) func() { return f }\n\nfunc (s *Server) Routes() {\n\twrap(s.Health)\n}\n",
    )]);
    let indirect = pairs(&r, "indirect_call");
    assert!(
        indirect.contains(&(".Routes()".into(), ".Health()".into())),
        "{indirect:?}"
    );
}

#[test]
fn go_receiver_field_typed_from_a_sibling_file() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        (
            "svc/types.go",
            "package svc\n\ntype Logger struct{}\n\nfunc (l *Logger) Log() {}\n\ntype Server struct {\n\tlogger *Logger\n}\n",
        ),
        (
            "svc/run.go",
            "package svc\n\nfunc (s *Server) Run() {\n\ts.logger.Log()\n}\n",
        ),
    ]);
    let calls = pairs(&r, "calls");
    assert!(
        calls.contains(&(".Run()".into(), ".Log()".into())),
        "{calls:?}"
    );
    assert!(
        nodes(&r)
            .iter()
            .all(|n| n.get("_go_fields").is_none() || n["label"] == "types.go")
    );
}
