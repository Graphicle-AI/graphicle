// Adapted from Graphify 0.9.102 (5bab54d8), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_js_import_resolution.py
//! A JS/TS member call through a default import binds to the method of the
//! object literal the imported file default-exports (Graphify 0.9.102,
//! #4963), only for a direct, unshadowed default binding, a plain call (no
//! `new`) and a member the literal states for certain.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
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
    fn run_with(&self, paths: &[PathBuf], context: Vec<Value>) -> Value {
        extract(
            paths,
            &ExtractOptions {
                root: Some(self.root.clone()),
                cache_root: Some(self.root.join("graphify-out")),
                parallel: false,
                resolution_context_nodes: context,
                ..Default::default()
            },
        )
        .unwrap()
    }
    fn run(&self, paths: &[PathBuf]) -> Value {
        self.run_with(paths, Vec::new())
    }
}

fn items<'a>(result: &'a Value, key: &str) -> &'a [Value] {
    result[key].as_array().map(Vec::as_slice).unwrap_or_default()
}

/// The node of `file` (root-relative, `/`-separated) labelled `label`.
fn find<'a>(result: &'a Value, file: &str, label: &str) -> Option<&'a Value> {
    items(result, "nodes").iter().find(|n| {
        n["label"] == label
            && n["source_file"]
                .as_str()
                .is_some_and(|s| Path::new(s).ends_with(file))
    })
}

fn calls(result: &Value) -> Vec<&Value> {
    items(result, "edges")
        .iter()
        .filter(|e| e["relation"] == "calls")
        .collect()
}

/// Calls edges from `from_label` in `from_file` to `to_label` in `to_file`.
fn calls_between(result: &Value, from: (&str, &str), to: (&str, &str)) -> usize {
    let (Some(source), Some(target)) = (find(result, from.0, from.1), find(result, to.0, to.1))
    else {
        return 0;
    };
    calls(result)
        .into_iter()
        .filter(|e| e["source"] == source["id"] && e["target"] == target["id"])
        .count()
}

/// Calls edges into the `toast()` node of native.ts.
fn calls_into_toast(result: &Value) -> usize {
    let Some(target) = find(result, "native.ts", "toast()") else {
        return 0;
    };
    calls(result)
        .into_iter()
        .filter(|e| e["target"] == target["id"])
        .count()
}

#[test]
fn default_object_member_call_resolves() {
    for suffix in [".ts", ".js"] {
        for binding in ["native", "renamed"] {
            let f = Fixture::new();
            let native = format!("native{suffix}");
            let app = format!("app{suffix}");
            let target = f.write(&native, "export default { toast() {} };\n");
            let importer = f.write(
                &app,
                &format!(
                    "import {binding} from './native';\nexport function run() {{ {binding}.toast(); }}\n"
                ),
            );
            let result = f.run(&[target, importer]);
            assert_eq!(
                calls_between(&result, (&app, "run()"), (&native, "toast()")),
                1,
                "{suffix} {binding}: {result:#}"
            );
            let methods: Vec<&Value> = items(&result, "nodes")
                .iter()
                .filter(|n| n["label"] == "toast()")
                .collect();
            assert_eq!(methods.len(), 1, "{suffix} {binding}");
            assert_eq!(methods[0]["source_file"], native.as_str());
            assert!(!items(&result, "nodes").iter().any(|n| n["label"] == "default"));
        }
    }
}

#[test]
fn default_object_member_calls_stay_in_imported_module() {
    let f = Fixture::new();
    let left = f.write("left/native.ts", "export default { toast() {} };\n");
    let right = f.write("right/native.ts", "export default { toast() {} };\n");
    let importer = f.write(
        "app.ts",
        "import left from './left/native';\n\
         import right from './right/native';\n\
         function useLeft() { left.toast(); }\n\
         function useRight() { right.toast(); }\n",
    );
    let orders = [
        vec![left.clone(), right.clone(), importer.clone()],
        vec![importer, right, left],
    ];
    for paths in orders {
        let result = f.run(&paths);
        for (caller, target, other) in [
            ("useLeft()", "left/native.ts", "right/native.ts"),
            ("useRight()", "right/native.ts", "left/native.ts"),
        ] {
            assert_eq!(calls_between(&result, ("app.ts", caller), (target, "toast()")), 1);
            assert_eq!(calls_between(&result, ("app.ts", caller), (other, "toast()")), 0);
        }
    }
}

#[test]
fn default_object_unknown_or_uncertain_member_has_no_call() {
    let certain = "export default { toast() {} };";
    for exported in [
        certain,
        "const key = 'toast'; export default { [key]() {} };",
        "export default { ['toast']() {} };",
        "const extra = {}; export default { toast() {}, ...extra };",
        "const extra = {}; export default { ...extra, toast() {} };",
        "export default { toast() {}, toast: 1 };",
        "export default { get toast() { return () => {}; } };",
        "export default { nested: { toast() {} } };",
    ] {
        let f = Fixture::new();
        let target = f.write("native.ts", &format!("{exported}\n"));
        let importer = f.write(
            "app.ts",
            "import native from './native';\nfunction run() { native.missing(); native.toast(); }\n",
        );
        let unrelated = f.write("other.ts", "export function missing() {}\n");
        let result = f.run(&[target, importer, unrelated]);
        if exported == certain {
            assert_eq!(calls_between(&result, ("app.ts", "run()"), ("native.ts", "toast()")), 1);
            assert_eq!(calls(&result).len(), 1, "{exported}");
        } else {
            assert!(calls(&result).is_empty(), "{exported}: {:?}", calls(&result));
        }
    }
}

#[test]
fn default_object_member_requires_direct_default_binding() {
    for statement in [
        "import * as native from './native'; function run() { native.toast(); }",
        "import { native } from './native'; function run() { native.toast(); }",
        "import native from './native'; const alias = native; function run() { alias.toast(); }",
        "import native from './native'; function run(native: any) { native.toast(); }",
        "import native from './native'; function run() { const native = {}; native.toast(); }",
        "import native from './native'; function run() { native['toast'](); }",
    ] {
        let f = Fixture::new();
        let target = f.write("native.ts", "export default { toast() {} };\n");
        let importer = f.write("app.ts", &format!("{statement}\n"));
        let result = f.run(&[target, importer]);
        assert!(calls(&result).is_empty(), "{statement}: {:?}", calls(&result));
    }
}

#[test]
fn default_object_member_requires_unshadowed_method_call() {
    for body in [
        "function run(native: any) { function inner() { native.toast(); } inner(); }",
        "function run() { const native = {}; function inner() { native.toast(); } inner(); }",
        "function run() { function native() {} native.toast(); }",
        "function run() { class native {} native.toast(); }",
        "function run() { const inner = function native() { native.toast(); }; inner(); }",
        "function run() { new native.toast(); }",
    ] {
        for incremental in [false, true] {
            let f = Fixture::new();
            let target = f.write("native.ts", "export default { toast() {} };\n");
            let importer = f.write("app.ts", &format!("import native from './native';\n{body}\n"));
            let result = if incremental {
                let baseline = f.run(&[target]);
                let context = items(&baseline, "nodes").to_vec();
                let mut result = f.run_with(&[importer], context);
                // The target node lives in the baseline: count calls into it.
                let toast = find(&baseline, "native.ts", "toast()").cloned();
                if let (Some(toast), Some(nodes)) = (toast, result["nodes"].as_array_mut()) {
                    nodes.push(toast);
                }
                result
            } else {
                f.run(&[target, importer])
            };
            assert_eq!(calls_into_toast(&result), 0, "{body} incremental={incremental}");
        }
    }
}

#[test]
fn default_object_member_keeps_calls_outside_shadow_scope() {
    for body in [
        "function run() { function inner() { native.toast(); } inner(); }",
        "function run() { { const native = {}; } function inner() { native.toast(); } inner(); }",
        "function run() { { class native {} } native.toast(); }",
    ] {
        let f = Fixture::new();
        let target = f.write("native.ts", "export default { toast() {} };\n");
        let importer = f.write("app.ts", &format!("import native from './native';\n{body}\n"));
        let result = f.run(&[target, importer]);
        assert_eq!(calls_into_toast(&result), 1, "{body}");
    }
}

#[test]
fn default_object_member_call_from_class() {
    for method in ["toast", "'toast'", "\"toast\""] {
        let f = Fixture::new();
        let target = f.write("native.ts", &format!("export default {{ {method}() {{}} }};\n"));
        let importer = f.write(
            "app.ts",
            "import renamed from './native';\nclass App { run() { renamed.toast(); } }\n",
        );
        let result = f.run(&[target, importer]);
        let found = calls(&result);
        assert_eq!(found.len(), 1, "{method}: {found:?}");
        let node = |id: &Value| {
            items(&result, "nodes")
                .iter()
                .find(|n| n["id"] == *id)
                .unwrap()
                .clone()
        };
        assert_eq!(node(&found[0]["source"])["label"], ".run()");
        let target = node(&found[0]["target"]);
        assert_eq!(target["source_file"], "native.ts");
        assert_eq!(target["label"], format!("{method}()"));
        assert_eq!(found[0]["confidence"], "EXTRACTED");
    }
}

#[test]
fn default_object_changed_caller_resolves_with_context() {
    let f = Fixture::new();
    let target = f.write("native.ts", "export default { toast() {} };\n");
    let baseline = f.run(&[target]);
    let toast = find(&baseline, "native.ts", "toast()").unwrap()["id"].clone();
    let importer = f.write(
        "app.ts",
        "import native from './native';\nfunction run() { native.toast(); }\n",
    );
    let result = f.run_with(&[importer], items(&baseline, "nodes").to_vec());
    let run = find(&result, "app.ts", "run()").unwrap()["id"].clone();
    assert!(
        calls(&result)
            .iter()
            .any(|e| e["source"] == run && e["target"] == toast),
        "{result:#}"
    );
}
