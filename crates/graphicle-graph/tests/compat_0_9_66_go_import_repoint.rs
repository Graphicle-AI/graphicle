//! Mirrors upstream `tests/test_go_import_repoint.py` (0.9.66, #3746): intra-
//! module Go `imports_from` edges are repointed from the `go_pkg_*` sink onto
//! the imported package's file nodes.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn write_module(root: &Path) {
    std::fs::write(root.join("go.mod"), "module example.com/m\n\ngo 1.22\n").unwrap();
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    std::fs::write(b.join("b.go"), "package b\n\nfunc F() int { return 1 }\n").unwrap();
    std::fs::write(b.join("c.go"), "package b\n\nfunc G() int { return 2 }\n").unwrap();
    std::fs::write(
        a.join("a.go"),
        "package a\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/m/b\"\n)\n\n\
         func Run() int {\n\tfmt.Println(b.G())\n\treturn b.F()\n}\n",
    )
    .unwrap();
}

fn extract_module(root: &Path) -> Value {
    let paths: Vec<PathBuf> = vec![
        root.join("a/a.go"),
        root.join("b/b.go"),
        root.join("b/c.go"),
    ];
    let options = ExtractOptions {
        root: Some(root.to_path_buf()),
        cache_root: Some(root.to_path_buf()),
        parallel: false,
        ..Default::default()
    };
    extract(&paths, &options).unwrap()
}

fn file_node<'a>(result: &'a Value, suffix: &str) -> &'a Value {
    let name = Path::new(suffix).file_name().unwrap().to_str().unwrap();
    let hits: Vec<&Value> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["source_file"].as_str().unwrap_or("").ends_with(suffix) && n["label"] == name)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected one file node for {suffix}, got {hits:?}"
    );
    hits[0]
}

fn edges<'a>(result: &'a Value, source_id: &str, relation: &str) -> Vec<&'a Value> {
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["source"] == source_id && e["relation"] == relation)
        .collect()
}

fn ids_with_label(result: &Value, label: &str) -> BTreeSet<String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["label"] == label)
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn intra_module_import_reaches_every_file_of_the_package() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_module(&root);
    let result = extract_module(&root);

    let a_go = file_node(&result, "a/a.go");
    let b_go = file_node(&result, "b/b.go");
    let c_go = file_node(&result, "b/c.go");

    let imports = edges(&result, a_go["id"].as_str().unwrap(), "imports_from");
    let targets: BTreeSet<&str> = imports
        .iter()
        .map(|e| e["target"].as_str().unwrap())
        .collect();
    assert!(
        targets.contains(b_go["id"].as_str().unwrap()),
        "{imports:?}"
    );
    assert!(
        targets.contains(c_go["id"].as_str().unwrap()),
        "{imports:?}"
    );
    assert!(!targets.contains("go_pkg_example_com_m_b"), "{imports:?}");
    assert!(
        !result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["target"] == "go_pkg_example_com_m_b")
    );
}

#[test]
fn repoint_keeps_the_call_edge_and_adds_no_self_import() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_module(&root);
    let result = extract_module(&root);

    let a_go = file_node(&result, "a/a.go");
    let run_ids = ids_with_label(&result, "Run()");
    let f_ids = ids_with_label(&result, "F()");
    assert!(!run_ids.is_empty() && !f_ids.is_empty());
    let all = result["edges"].as_array().unwrap();
    assert!(
        all.iter().any(|e| {
            run_ids.contains(e["source"].as_str().unwrap_or(""))
                && f_ids.contains(e["target"].as_str().unwrap_or(""))
                && e["relation"] == "calls"
        }),
        "the qualified call edge must survive the import repoint"
    );
    assert!(
        !all.iter()
            .any(|e| e["source"] == a_go["id"] && e["target"] == a_go["id"])
    );
}

#[test]
fn stdlib_and_external_imports_stay_sinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_module(&root);
    let result = extract_module(&root);

    let a_go = file_node(&result, "a/a.go");
    let imports = edges(&result, a_go["id"].as_str().unwrap(), "imports_from");
    let fmt_targets: Vec<&str> = imports
        .iter()
        .map(|e| e["target"].as_str().unwrap())
        .filter(|t| t.starts_with("go_pkg_fmt"))
        .collect();
    assert_eq!(fmt_targets, ["go_pkg_fmt"], "{imports:?}");
    for n in result["nodes"].as_array().unwrap() {
        if n["id"] == "go_pkg_fmt" {
            assert!(n["source_file"].as_str().unwrap_or("").is_empty());
        }
    }
}
