// Adapted from Graphify 0.9.98, Apache-2.0; see NOTICE.md.
//! Extraction changes ported from Graphify 0.9.98, from the upstream tests
//! of that release: unique declaration ids beside the file id and the
//! cross-file binding of imported references (#4854), C# owner-qualified
//! method labels (#4883), JS imports with a query (#4860), and fixes for
//! PowerShell, Solidity, Common Lisp, Lazarus forms, SystemVerilog and Go.
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


fn labels(result: &Value) -> BTreeSet<String> {
    nodes(result)
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}
fn id_of(result: &Value, label: &str, file: &str) -> String {
    let found = nodes(result)
        .iter()
        .filter(|n| n["label"] == label && n["source_file"] == file)
        .map(|n| n["id"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1, "one {label} in {file}: {found:?}");
    found[0].clone()
}
/// `((label, file), relation, (label, file))` of every edge.
type End = (String, String);
fn edge_facts(result: &Value) -> BTreeSet<(End, String, End)> {
    let end = |id: &Value| {
        node(result, id)
            .map(|n| {
                (
                    n["label"].as_str().unwrap_or("").to_string(),
                    n["source_file"].as_str().unwrap_or("").to_string(),
                )
            })
            .unwrap_or_default()
    };
    edges(result)
        .iter()
        .map(|e| {
            (
                end(&e["source"]),
                e["relation"].as_str().unwrap_or("").to_string(),
                end(&e["target"]),
            )
        })
        .collect()
}
fn fact(a: (&str, &str), relation: &str, b: (&str, &str)) -> (End, String, End) {
    (
        (a.0.into(), a.1.into()),
        relation.into(),
        (b.0.into(), b.1.into()),
    )
}
fn assert_clean(result: &Value, expected: &[(&str, &str)]) {
    let got = nodes(result)
        .iter()
        .map(|n| {
            (
                n["label"].as_str().unwrap_or("").to_string(),
                n["source_file"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    for (label, file) in expected {
        assert!(got.contains(&(label.to_string(), file.to_string())), "missing {label} in {file}: {got:?}");
    }
    let ids = nodes(result).iter().map(|n| n["id"].clone()).collect::<Vec<_>>();
    let unique = ids.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), unique.len(), "duplicate ids");
    assert!(
        !edges(result)
            .iter()
            .any(|e| e["source"] == e["target"] && e["relation"] == "contains"),
        "self-contains edge"
    );
}

// --- A file node and a declaration never share an id (#4854) ---------------

#[test]
fn declarations_named_like_their_file_keep_their_own_nodes() {
    type Case<'a> = (
        &'a str,
        &'a str,
        &'a [(&'a str, &'a str)],
        &'a [((&'a str, &'a str), &'a str, (&'a str, &'a str))],
    );
    let cases: [Case<'_>; 6] = [
        (
            "a.js",
            "class Js { run() { return 1; } }\n",
            &[("a.js", "a.js"), ("Js", "a.js"), (".run()", "a.js")],
            &[(("a.js", "a.js"), "contains", ("Js", "a.js")), (("Js", "a.js"), "method", (".run()", "a.js"))],
        ),
        (
            "a.js",
            "function js() { return 1; }\n",
            &[("a.js", "a.js"), ("js()", "a.js")],
            &[(("a.js", "a.js"), "contains", ("js()", "a.js"))],
        ),
        (
            "a.js",
            "export const Js = 1;\n",
            &[("a.js", "a.js"), ("Js", "a.js")],
            &[(("a.js", "a.js"), "contains", ("Js", "a.js"))],
        ),
        (
            "a.js",
            "const js = () => 1;\n",
            &[("a.js", "a.js"), ("js()", "a.js")],
            &[(("a.js", "a.js"), "contains", ("js()", "a.js"))],
        ),
        (
            "m.py",
            "class Py:\n    def run(self):\n        return 1\n",
            &[("m.py", "m.py"), ("Py", "m.py"), (".run()", "m.py")],
            &[(("m.py", "m.py"), "contains", ("Py", "m.py")), (("Py", "m.py"), "method", (".run()", "m.py"))],
        ),
        (
            "a.ts",
            "class Ts {}\n@Ts class Other {}\n",
            &[("a.ts", "a.ts"), ("Ts", "a.ts"), ("Other", "a.ts")],
            &[(("a.ts", "a.ts"), "contains", ("Ts", "a.ts")), (("Other", "a.ts"), "references", ("Ts", "a.ts"))],
        ),
    ];
    for (file, text, expected, expected_edges) in cases {
        let f = Fixture::new();
        let r = f.extract_files(&[(file, text)]);
        assert_clean(&r, expected);
        let facts = edge_facts(&r);
        for (a, rel, b) in expected_edges {
            assert!(facts.contains(&fact(*a, rel, *b)), "{text}: missing {a:?} {rel} {b:?}");
        }
    }
    let f = Fixture::new();
    let r = f.extract_files(&[("a.rs", "struct Rs { x: i32 }\nfn main() {}\n")]);
    assert_clean(&r, &[("a.rs", "a.rs"), ("Rs", "a.rs"), ("main()", "a.rs")]);
    assert_ne!(id_of(&r, "Rs", "a.rs"), id_of(&r, "a.rs", "a.rs"));
}

#[test]
fn cross_file_decorator_targets_the_declared_class() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("a.ts", "export class Ts {}\n"),
        ("b.ts", "import { Ts } from \"./a\";\n@Ts class Other {}\n"),
    ]);
    let class = id_of(&r, "Ts", "a.ts");
    let other = id_of(&r, "Other", "b.ts");
    let file = id_of(&r, "b.ts", "b.ts");
    let targets = edges(&r)
        .iter()
        .filter(|e| e["source"] == other.as_str())
        .map(|e| e["target"].as_str().unwrap_or("").to_string())
        .collect::<BTreeSet<_>>();
    assert!(targets.contains(&class), "{targets:?}");
    assert!(!targets.contains(&file));
}

#[test]
fn decl_registry_salts_and_reuses() {
    use graphicle::ids::{DeclIdRegistry, decl_salt, make_id};
    let mut registry = DeclIdRegistry::new(["a_js"]);
    let taken = |id: &str| id == "a_js";
    let class = registry.resolve("a_js", "Js", "type", taken);
    assert_eq!(class, make_id(["a_js", decl_salt("Js").as_str()]));
    assert_eq!(registry.resolve("a_js", "Js", "type", taken), class);
    assert_eq!(registry.resolve("a_other", "Other", "type", |_| false), "a_other");
    assert_eq!(registry.lookup("Js"), Some(class));
    assert_eq!(decl_salt("Js").len(), 12);
}

// --- C# owner-qualified method labels (#4883) --------------------------------

#[test]
fn csharp_method_labels_round_trip_through_the_bare_form() {
    use graphicle::extractors::csharp::{
        bare_csharp_method_label, bare_csharp_method_labels_in_nodes,
        csharp_method_label_reassignments,
    };
    use serde_json::json;
    let cs = json!("src/A.cs");
    assert_eq!(bare_csharp_method_label(&json!("Widget.Build()"), &cs).as_deref(), Some(".Build()"));
    assert_eq!(bare_csharp_method_label(&json!(".Build()"), &cs).as_deref(), Some(".Build()"));
    assert_eq!(bare_csharp_method_label(&json!("A.B.Build()"), &cs), None);
    assert_eq!(bare_csharp_method_label(&json!("Widget.Build()"), &json!("a.py")), None);
    let nodes = vec![
        json!({"id":"w","label":"Widget","source_file":"src/A.cs"}),
        json!({"id":"m","label":".Build()","source_file":"src/A.cs"}),
    ];
    let moved = csharp_method_label_reassignments(
        nodes.iter().map(|n| (n["id"].as_str().unwrap(), n)),
        [("w", "m")],
    );
    assert_eq!(moved.get("m").map(String::as_str), Some("Widget.Build()"));
    let qualified = vec![json!({"id":"m","label":"Widget.Build()","source_file":"src/A.cs"})];
    let bare = bare_csharp_method_labels_in_nodes(&qualified).unwrap();
    assert_eq!(bare[0]["label"], ".Build()");
    assert!(bare_csharp_method_labels_in_nodes(&nodes).is_none());
}

// --- Single-language fixes ------------------------------------------------------

#[test]
fn js_relative_import_with_query_or_fragment_resolves() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("lib/foo.js", "export const foo = 1;\n"),
        ("main.js", "import { foo } from './lib/foo.js?v=134';\nimport { foo as bar } from './lib/foo.js#frag';\n"),
    ]);
    let hit = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "imports_from")
        .filter_map(|e| node(&r, &e["target"]))
        .any(|n| n["source_file"].as_str().unwrap_or("").ends_with("lib/foo.js"));
    assert!(hit, "{:?}", pairs(&r, "imports_from"));
    assert!(!labels(&r).iter().any(|l| l.contains("v=134") || l.contains("134")));
}

#[test]
fn powershell_using_module_strips_known_extensions_only() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "run.ps1",
        "using module ./Helpers.psm1\nusing assembly System.Data.dll\nusing module Az.Accounts\nusing namespace System.IO\n",
    )]);
    let targets = pairs(&r, "imports_from")
        .into_iter()
        .map(|(_, t)| t)
        .collect::<BTreeSet<_>>();
    let ids = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "imports_from")
        .map(|e| e["target"].as_str().unwrap_or("").to_string())
        .collect::<BTreeSet<_>>();
    for id in ["helpers", "system_data", "az_accounts", "io"] {
        assert!(ids.contains(id), "{id}: {ids:?} {targets:?}");
    }
    assert!(!ids.contains("psm1") && !ids.contains("dll"));
}

#[test]
fn solidity_free_function_using_set_emits_uses_edges() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "Lib.sol",
        "pragma solidity ^0.8.13;\nfunction add(uint a, uint b) pure returns (uint) { return a + b; }\nfunction sub(uint a, uint b) pure returns (uint) { return a - b; }\ncontract C {\n    using {add, sub} for uint;\n}\n",
    )]);
    let used = pairs(&r, "uses")
        .into_iter()
        .filter(|(s, _)| s == "C")
        .map(|(_, t)| t)
        .collect::<BTreeSet<_>>();
    assert!(used.iter().any(|t| t.starts_with("add")) && used.iter().any(|t| t.starts_with("sub")), "{used:?}");
}

#[test]
fn commonlisp_forward_superclass_binds_its_local_node() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "shapes.lisp",
        "(defclass circle (shape) ((r :initarg :r)))\n(defclass shape () ())\n",
    )]);
    let inherits = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "inherits")
        .filter_map(|e| node(&r, &e["target"]))
        .map(|n| n["source_file"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(inherits.len(), 1, "{inherits:?}");
    assert!(inherits[0].ends_with("shapes.lisp"), "{inherits:?}");
}

#[test]
fn lazarus_inherited_and_inline_component_blocks() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "child.lfm",
        "inherited ChildForm: TChildForm\n  Caption = 'x'\n  inline Frame1: TFrame1\n  end\n  object Button1: TButton\n    OnClick = Button1Click\n  end\nend\n",
    )]);
    let l = labels(&r);
    assert!(l.contains("TChildForm") && l.contains("TFrame1") && l.contains("TButton"), "{l:?}");
}

#[test]
fn systemverilog_constructor_without_return_type() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "pkt.sv",
        "class Cfg;\nendclass\nclass Pkt;\n  function new(Cfg c);\n  endfunction\nendclass\n",
    )]);
    assert!(pairs(&r, "method").contains(&("Pkt".into(), "new".into())), "{:?}", pairs(&r, "method"));
    assert!(pairs(&r, "references").iter().any(|(s, t)| s == "new" && t == "Cfg"));
}

#[test]
fn go_var_with_composite_literal_types_the_receiver() {
    let greeter = "package svc\n\ntype Greeter struct{}\n\nfunc (g Greeter) Greet() string { return \"hi\" }\n";
    for (app, expect) in [
        ("package svc\n\nfunc Run() {\n\tvar g = Greeter{}\n\tg.Greet()\n}\n", true),
        ("package svc\n\nfunc Run() {\n\tvar g = &Greeter{}\n\tg.Greet()\n}\n", true),
    ] {
        let f = Fixture::new();
        let r = f.extract_files(&[("svc/greeter.go", greeter), ("svc/app.go", app)]);
        let hit = pairs(&r, "calls").contains(&("Run()".into(), ".Greet()".into()));
        assert_eq!(hit, expect, "{app}: {:?}", pairs(&r, "calls"));
    }
}
