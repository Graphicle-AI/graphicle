//! Mirrors upstream `tests/test_unmapped_at_alias_resolution.py` (#3357).
//!
//! When no tsconfig.json or jsconfig.json exists, `@/...` is an internal
//! project-root convention alias resolving against `anchor/src/<sub>` (if
//! `src/` exists) else `anchor/<sub>`.
use graphicle::api::{ExtractOptions, extract};
use graphicle::extractors::base::make_id;
use serde_json::Value;
use std::path::{Path, PathBuf};

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
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }
    fn extract(&self, paths: &[PathBuf]) -> Value {
        let options = ExtractOptions {
            root: Some(self.root.clone()),
            cache_root: Some(self.root.clone()),
            parallel: false,
            use_cache: false,
            ..Default::default()
        };
        extract(paths, &options).unwrap()
    }
}

fn file_id(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap().with_extension("");
    make_id([rel.to_string_lossy().as_ref()])
}

fn edges<'a>(res: &'a Value, relation: &str, source: &str) -> Vec<&'a Value> {
    res["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == relation && e["source"] == source)
        .collect()
}

fn calls_from_label<'a>(res: &'a Value, label: &str) -> Vec<&'a Value> {
    let nodes: std::collections::HashMap<&str, &Value> = res["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n))
        .collect();
    res["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["relation"] == "calls"
                && nodes
                    .get(e["source"].as_str().unwrap_or(""))
                    .is_some_and(|n| n["label"] == label)
        })
        .collect()
}

#[test]
fn test_3357_minimal_reproduction_no_config_emits_calls_edge() {
    let fx = Fixture::new();
    let adapter = fx.write(
        "adapter.js",
        "export function enableBackgroundBle() { return 1; }\n",
    );
    let caller = fx.write(
        "caller.js",
        "import { enableBackgroundBle } from \"@/adapter.js\";\nfunction run() { enableBackgroundBle(); }\n",
    );
    let res = fx.extract(&[adapter.clone(), caller]);
    let adapter_nid = file_id(&fx.root, &adapter);
    let fn_nid = make_id(["adapter", "enableBackgroundBle"]);

    let imports_from = edges(&res, "imports_from", "caller");
    assert_eq!(imports_from.len(), 1);
    assert_eq!(imports_from[0]["target"], adapter_nid);

    let imports = edges(&res, "imports", "caller");
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0]["target"], fn_nid);

    let calls = calls_from_label(&res, "run()");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["target"], fn_nid);
    assert_eq!(calls[0]["confidence"], "EXTRACTED");
}

#[test]
fn test_nested_importer_src_layout_resolves_to_src() {
    let fx = Fixture::new();
    fx.write("package.json", "{\"name\": \"test-pkg\"}\n");
    let adapter = fx.write(
        "src/adapter.js",
        "export function doThing() { return 2; }\n",
    );
    let caller = fx.write(
        "src/features/caller.js",
        "import { doThing } from \"@/adapter.js\";\nexport function run() { doThing(); }\n",
    );
    let res = fx.extract(&[adapter, caller]);
    let fn_nid = make_id(["src_adapter", "doThing"]);
    let calls = calls_from_label(&res, "run()");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["target"], fn_nid);
    assert_eq!(calls[0]["confidence"], "EXTRACTED");
}

#[test]
fn test_nested_importer_flat_layout_resolves_to_root() {
    let fx = Fixture::new();
    fx.write("package.json", "{\"name\": \"flat-pkg\"}\n");
    let adapter = fx.write("adapter.js", "export function rootHelper() { return 3; }\n");
    let caller = fx.write(
        "features/deep/caller.js",
        "import { rootHelper } from \"@/adapter.js\";\nexport function run() { rootHelper(); }\n",
    );
    let res = fx.extract(&[adapter, caller]);
    let fn_nid = make_id(["adapter", "rootHelper"]);
    let calls = calls_from_label(&res, "run()");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["target"], fn_nid);
    assert_eq!(calls[0]["confidence"], "EXTRACTED");
}

#[test]
fn test_tsconfig_without_paths_leaves_at_alias_unresolved() {
    let fx = Fixture::new();
    fx.write(
        "tsconfig.json",
        "{\"compilerOptions\": {\"target\": \"es2020\"}}",
    );
    let adapter = fx.write(
        "adapter.js",
        "export function enableBackgroundBle() { return 1; }\n",
    );
    let caller = fx.write(
        "caller.js",
        "import { enableBackgroundBle } from \"@/adapter.js\";\nfunction run() { enableBackgroundBle(); }\n",
    );
    let res = fx.extract(&[adapter, caller]);
    let ref_target = make_id(["ref", "@/adapter.js"]);
    let imports_from = edges(&res, "imports_from", "caller");
    assert_eq!(imports_from.len(), 1);
    assert_eq!(imports_from[0]["target"], ref_target);
    assert!(edges(&res, "imports", "caller").is_empty());
    assert!(
        !res["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["relation"] == "calls")
    );
}

#[test]
fn test_explicit_tsconfig_alias_takes_precedence_over_convention() {
    let fx = Fixture::new();
    fx.write(
        "tsconfig.json",
        "{\"compilerOptions\": {\"baseUrl\": \".\", \"paths\": {\"@/*\": [\"custom/*\"]}}}",
    );
    let custom_adapter = fx.write(
        "custom/adapter.js",
        "export function targetFunc() { return 'custom'; }\n",
    );
    let root_adapter = fx.write(
        "adapter.js",
        "export function targetFunc() { return 'root'; }\n",
    );
    let caller = fx.write(
        "caller.js",
        "import { targetFunc } from \"@/adapter.js\";\nfunction run() { targetFunc(); }\n",
    );
    let res = fx.extract(&[custom_adapter, root_adapter, caller]);
    let custom_fn_nid = make_id(["custom_adapter", "targetFunc"]);
    let calls = calls_from_label(&res, "run()");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["target"], custom_fn_nid);
}

#[test]
fn test_missing_at_target_uses_ref_fallback() {
    let fx = Fixture::new();
    let caller = fx.write(
        "caller.js",
        "import { missingFn } from \"@/does-not-exist.js\";\nfunction run() { missingFn(); }\n",
    );
    let res = fx.extract(&[caller]);
    let ref_target = make_id(["ref", "@/does-not-exist.js"]);
    let imports_from = edges(&res, "imports_from", "caller");
    assert_eq!(imports_from.len(), 1);
    assert_eq!(imports_from[0]["target"], ref_target);
    let all = res["edges"].as_array().unwrap();
    assert!(!all.iter().any(|e| e["relation"] == "imports"));
    assert!(!all.iter().any(|e| e["relation"] == "calls"));
}

#[test]
fn test_scoped_package_import_remains_external() {
    let fx = Fixture::new();
    let caller = fx.write(
        "caller.js",
        "import { something } from \"@scope/pkg\";\nfunction run() { something(); }\n",
    );
    let res = fx.extract(&[caller]);
    let ref_target = make_id(["ref", "@scope/pkg"]);
    let imports_from = edges(&res, "imports_from", "caller");
    assert_eq!(imports_from.len(), 1);
    assert_eq!(imports_from[0]["target"], ref_target);
}

#[test]
fn test_regex_rescue_unmapped_at_alias_resolves() {
    let fx = Fixture::new();
    fx.write("package.json", "{\"name\": \"test-app\"}\n");
    let lib = fx.write("lib.js", "export const helper = 42;\n");
    let svelte = fx.write(
        "App.svelte",
        "<script>\nimport { helper } from \"@/lib.js\";\n</script>\n<h1>Hello</h1>\n",
    );
    let resolution = graphicle::extract_languages::rescued_specifier(&svelte, "@/lib.js");
    let (_node_id, _stub_sf, resolved_file, is_external) = resolution.expect("rescue resolves @/");
    assert_eq!(resolved_file.as_deref(), Some(lib.as_path()));
    assert!(!is_external);
}

#[test]
fn test_unresolved_at_alias_does_not_infer_call_to_unrelated_definition() {
    let fx = Fixture::new();
    let caller = fx.write(
        "caller.js",
        "import { missingFn } from \"@/does-not-exist.js\";\nfunction run() { missingFn(); }\n",
    );
    let unrelated = fx.write(
        "unrelated.js",
        "export function missingFn() { return 'unrelated'; }\n",
    );
    let res = fx.extract(&[caller, unrelated]);
    let ref_target = make_id(["ref", "@/does-not-exist.js"]);
    let imports_from = edges(&res, "imports_from", "caller");
    assert_eq!(imports_from.len(), 1);
    assert_eq!(imports_from[0]["target"], ref_target);
    let unrelated_fn_nid = make_id(["unrelated", "missingFn"]);
    let phantom: Vec<_> = res["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls" && e["target"] == unrelated_fn_nid)
        .collect();
    assert!(phantom.is_empty(), "phantom calls edge: {phantom:?}");
}
