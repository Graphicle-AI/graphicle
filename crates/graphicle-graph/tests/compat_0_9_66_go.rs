//! Mirrors the two tests added to upstream `tests/test_go_interface_methods.py`
//! in 0.9.66 (#3672 interface method signature references, #3737 case-only
//! interface method owners).
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::write(&path, text).unwrap();
        path
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).unwrap()
    }
    fn extract(&self) -> Value {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&self.root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "go"))
            .collect();
        paths.sort();
        let options = ExtractOptions {
            root: Some(self.root.clone()),
            cache_root: Some(self.root.clone()),
            parallel: false,
            ..Default::default()
        };
        extract(&paths, &options).unwrap()
    }
}

fn by_id(result: &Value) -> HashMap<&str, &Value> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n))
        .collect()
}

/// `_signature_refs`: the single method owned by `owner_label` and its
/// `references` edges, each augmented with `target_label`.
fn signature_refs(result: &Value, owner_label: &str) -> (String, Vec<Value>) {
    let nodes = by_id(result);
    let edges = result["edges"].as_array().unwrap();
    let method_ids: BTreeSet<&str> = edges
        .iter()
        .filter(|e| e["relation"] == "method")
        .filter(|e| nodes[e["source"].as_str().unwrap()]["label"] == owner_label)
        .map(|e| e["target"].as_str().unwrap())
        .collect();
    assert_eq!(method_ids.len(), 1, "owner {owner_label}: {method_ids:?}");
    let method_id = method_ids.into_iter().next().unwrap().to_string();
    let refs = edges
        .iter()
        .filter(|e| e["relation"] == "references" && e["source"] == method_id.as_str())
        .map(|e| {
            let mut e = e.clone();
            e["target_label"] = nodes[e["target"].as_str().unwrap()]["label"].clone();
            e
        })
        .collect();
    (method_id, refs)
}

fn label_context(refs: &[Value]) -> BTreeSet<(String, String)> {
    refs.iter()
        .map(|e| {
            (
                e["target_label"].as_str().unwrap().to_string(),
                e["context"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

const SIGNATURES: &[(&str, &str)] = &[
    ("Handle(Request) Response", "return Response{}"),
    (
        "Handle(req *Request) (res *Response, err error)",
        "return nil, nil",
    ),
];

fn contract_source(signature: &str, body: &str) -> String {
    format!(
        "package contracts\n\n\
         type Request struct{{}}\n\
         type Response struct{{}}\n\
         type Service interface {{\n    {signature}\n}}\n\n\
         type Impl struct{{}}\n\
         func (Impl) {signature} {{ {body} }}\n"
    )
}

#[test]
fn interface_method_signature_keeps_type_references_3672() {
    for (signature, body) in SIGNATURES {
        let fx = Fixture::new();
        fx.write("contract.go", &contract_source(signature, body));
        {
            let result = fx.extract();
            let (interface_id, interface_refs) = signature_refs(&result, "Service");
            let (concrete_id, concrete_refs) = signature_refs(&result, "Impl");
            let expected: BTreeSet<(String, String)> = [
                ("Request".to_string(), "parameter_type".to_string()),
                ("Response".to_string(), "return_type".to_string()),
            ]
            .into_iter()
            .collect();
            assert_eq!(label_context(&concrete_refs), expected, "{signature}");
            assert_eq!(label_context(&interface_refs), expected, "{signature}");
            assert_eq!(interface_refs.len(), 2);
            assert_ne!(interface_id, concrete_id);
            assert!(
                !result["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["label"] == "error")
            );
            for edge in &interface_refs {
                assert_eq!(edge["source_file"], "contract.go");
                assert_eq!(edge["source_location"], "L6");
                assert_eq!(edge["confidence"], "EXTRACTED");
            }
            // A warm extraction replays the same source-owned signature edges.
            let warm = fx.extract();
            assert_eq!(
                signature_refs(&warm, "Service"),
                (interface_id, interface_refs)
            );
            assert_eq!(signature_refs(&warm, "Impl"), (concrete_id, concrete_refs));
        }
    }
}

const NAME_PAIRS: &[(&str, &str)] = &[
    ("Run", "run"),
    ("run", "Run"),
    ("Run", "RUN"),
    ("RUN", "Run"),
];

fn owned_refs(result: &Value, names: (&str, &str)) -> (BTreeMap<String, String>, String) {
    let nodes = by_id(result);
    let edges = result["edges"].as_array().unwrap();
    let methods: BTreeMap<String, String> = edges
        .iter()
        .filter(|e| e["relation"] == "method")
        .filter(|e| nodes[e["source"].as_str().unwrap()]["label"] == "Service")
        .map(|e| {
            let target = e["target"].as_str().unwrap();
            (
                nodes[target]["label"].as_str().unwrap().to_string(),
                target.to_string(),
            )
        })
        .collect();
    let expected_labels: BTreeSet<String> = [names.0, names.1]
        .iter()
        .map(|n| format!(".{n}()"))
        .collect();
    assert_eq!(
        methods.keys().cloned().collect::<BTreeSet<_>>(),
        expected_labels
    );
    assert_eq!(methods.values().collect::<BTreeSet<_>>().len(), 2);
    for (name, expected) in [(names.0, "InputA"), (names.1, "InputB")] {
        let method_id = &methods[&format!(".{name}()")];
        let refs: Vec<&Value> = edges
            .iter()
            .filter(|e| e["relation"] == "references" && e["source"] == method_id.as_str())
            .collect();
        assert_eq!(refs.len(), 1, "{name}");
        assert_eq!(
            nodes[refs[0]["target"].as_str().unwrap()]["label"],
            expected
        );
        assert_eq!(refs[0]["context"], "parameter_type");
    }
    let (other_id, other_refs) = signature_refs(result, "Other");
    assert!(!methods.values().any(|v| *v == other_id));
    assert_eq!(
        other_refs
            .iter()
            .map(|e| e["target_label"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["InputA"]
    );
    (methods, other_id)
}

#[test]
fn case_only_interface_methods_keep_separate_signature_owners_3737() {
    for names in NAME_PAIRS.iter().copied() {
        let fx = Fixture::new();
        let first = format!("    {}(InputA)\n    {}(InputB)\n", names.0, names.1);
        let swapped = format!("    {}(InputB)\n    {}(InputA)\n", names.1, names.0);
        fx.write(
            "contract.go",
            &format!(
                "package contracts\n\n\
                 type InputA struct{{}}\n\
                 type InputB struct{{}}\n\
                 type Service interface {{\n{first}}}\n\
                 type Other interface {{ Run(InputA) }}\n"
            ),
        );
        {
            let result = fx.extract();
            let (methods, other_id) = owned_refs(&result, names);
            assert_eq!(
                owned_refs(&fx.extract(), names),
                (methods.clone(), other_id.clone())
            );

            // Declaration order cannot choose the canonical owner of a collision.
            let source = fx.read("contract.go").replace(&first, &swapped);
            fx.write("contract.go", &source);
            assert_eq!(
                owned_refs(&fx.extract(), names),
                (methods.clone(), other_id.clone())
            );

            // Removing the private sibling preserves the exported ID and other owners.
            if names.0 == "run" || names.1 == "run" {
                let source = fx.read("contract.go");
                let kept: Vec<&str> = source
                    .lines()
                    .filter(|line| !line.contains("    run("))
                    .collect();
                fx.write("contract.go", &kept.join("\n"));
                let without_private = fx.extract();
                let (exported_id, _) = signature_refs(&without_private, "Service");
                assert_eq!(exported_id, methods[".Run()"]);
                assert_eq!(signature_refs(&without_private, "Other").0, other_id);
            }
        }
    }
}
