// Adapted from Graphify 0.9.110, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_python_aliased_call_in_method.py
//! Extraction changes ported from Graphify 0.9.110. Cases follow the
//! upstream tests named in each comment.
use graphicle::api::{ExtractOptions, extract};
use std::collections::BTreeSet;

fn calls_to_f(caller: &str) -> BTreeSet<String> {
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let pkg = root.join("pkg");
    std::fs::create_dir(&pkg).unwrap();
    std::fs::write(pkg.join("__init__.py"), "").unwrap();
    std::fs::write(pkg.join("a.py"), "def f():\n    return 1\n").unwrap();
    std::fs::write(pkg.join("b.py"), caller).unwrap();
    let r = extract(
        &[pkg.join("__init__.py"), pkg.join("a.py"), pkg.join("b.py")],
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("graphify-out")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap();
    let target = r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == "f()")
        .map(|n| n["id"].clone())
        .unwrap();
    r["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls" && e["target"] == target)
        .map(|e| e["source"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn aliased_call_in_a_method_or_staticmethod_is_linked() {
    // test_python_aliased_call_in_method.py (#5214)
    for caller in [
        "from .a import f as g\n\nclass C:\n    def use(self):\n        return g()\n",
        "from .a import f as g\n\nclass C:\n    @staticmethod\n    def use():\n        return g()\n",
    ] {
        assert_eq!(calls_to_f(caller), BTreeSet::from(["pkg_b_c_use".to_string()]), "{caller}");
    }
}

#[test]
fn go_internal_test_file_method_folds_into_the_package_type() {
    // test_go_package_type_fold.py (#5209)
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let files = [
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        ("seq/seq.go", "package seq\n\ntype Seq struct{}\n\nfunc (s *Seq) helper() int { return 1 }\n\nfunc (s *Seq) Run() int { return s.helper() }\n"),
        ("seq/extra_test.go", "// Test-only helpers.\npackage seq\n\nfunc (s *Seq) extra() int { return 2 }\n"),
        ("svc/a.go", "package svc\n\ntype Fixture struct{}\n"),
        ("svc/b_test.go", "package svc_test\n\nfunc (f *Fixture) Reset() {}\n"),
    ];
    let paths = files
        .iter()
        .map(|(rel, text)| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
            p
        })
        .filter(|p| p.extension().is_some_and(|e| e == "go"))
        .collect::<Vec<_>>();
    let r = extract(
        &paths,
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("graphify-out")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap();
    let nodes = r["nodes"].as_array().unwrap();
    let seqs = nodes.iter().filter(|n| n["label"] == "Seq").collect::<Vec<_>>();
    assert_eq!(seqs.len(), 1, "{seqs:?}");
    let methods = r["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "method" && e["source"] == seqs[0]["id"])
        .count();
    assert_eq!(methods, 3, "{r:#}");
    assert_eq!(nodes.iter().filter(|n| n["label"] == "Fixture").count(), 2);
}
