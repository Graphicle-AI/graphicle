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
