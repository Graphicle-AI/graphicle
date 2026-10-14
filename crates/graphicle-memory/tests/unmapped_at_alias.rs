//! Mirrors upstream Graphify `tests/test_unmapped_at_alias_resolution.py`
//! (issue #3357) against the in-memory manifest graph: an unmapped `@/x`
//! import resolves through the project-root convention only when no
//! `tsconfig.json`/`jsconfig.json` governs the importer.
use graphicle_memory::structure::{analyze, graph};
use serde_json::{Map, Value, json};

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

fn call(a: &str, b: &str, c: &str, d: &str) -> Vec<Vec<String>> {
    vec![vec![a.into(), b.into(), c.into(), d.into()]]
}

#[test]
fn test_3357_minimal_reproduction_no_config_emits_calls_edge() {
    let raw = build(&[
        ("adapter.js", "export function enableBackgroundBle() {}\n"),
        (
            "caller.js",
            "import { enableBackgroundBle } from '@/adapter';\nexport function run() { enableBackgroundBle(); }\n",
        ),
    ]);
    assert_eq!(
        calls(&raw),
        call("caller.js", "run", "adapter.js", "enableBackgroundBle")
    );
}

#[test]
fn test_nested_importer_src_layout_resolves_to_src() {
    let raw = build(&[
        ("package.json", "{\"name\": \"p\"}"),
        ("src/adapter.js", "export function doThing() {}\n"),
        (
            "src/features/caller.js",
            "import { doThing } from '@/adapter';\nexport function run() { doThing(); }\n",
        ),
    ]);
    assert_eq!(
        calls(&raw),
        call("src/features/caller.js", "run", "src/adapter.js", "doThing")
    );
}

#[test]
fn test_nested_importer_flat_layout_resolves_to_root() {
    let raw = build(&[
        ("package.json", "{\"name\": \"p\"}"),
        ("adapter.js", "export function rootHelper() {}\n"),
        (
            "features/deep/caller.js",
            "import { rootHelper } from '@/adapter';\nexport function run() { rootHelper(); }\n",
        ),
    ]);
    assert_eq!(
        calls(&raw),
        call("features/deep/caller.js", "run", "adapter.js", "rootHelper")
    );
}

#[test]
fn test_tsconfig_without_paths_leaves_at_alias_unresolved() {
    let raw = build(&[
        ("tsconfig.json", "{\"compilerOptions\": {}}"),
        ("adapter.js", "export function enableBackgroundBle() {}\n"),
        (
            "caller.js",
            "import { enableBackgroundBle } from '@/adapter';\nexport function run() { enableBackgroundBle(); }\n",
        ),
    ]);
    assert!(calls(&raw).is_empty());
}

#[test]
fn test_explicit_tsconfig_alias_takes_precedence_over_convention() {
    let raw = build(&[
        (
            "tsconfig.json",
            "{\"compilerOptions\": {\"paths\": {\"@/*\": [\"./custom/*\"]}}}",
        ),
        ("adapter.js", "export function targetFunc() {}\n"),
        ("custom/adapter.js", "export function targetFunc() {}\n"),
        (
            "caller.js",
            "import { targetFunc } from '@/adapter';\nexport function run() { targetFunc(); }\n",
        ),
    ]);
    assert_eq!(
        calls(&raw),
        call("caller.js", "run", "custom/adapter.js", "targetFunc")
    );
}

#[test]
fn test_missing_at_target_uses_ref_fallback() {
    let raw = build(&[(
        "caller.js",
        "import { ghost } from '@/nothing';\nexport function run() { ghost(); }\n",
    )]);
    assert!(calls(&raw).is_empty());
    assert!(
        raw["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["module"] == json!("@/nothing"))
    );
}

#[test]
fn test_scoped_package_import_remains_external() {
    let raw = build(&[(
        "caller.js",
        "import { thing } from '@scope/pkg';\nexport function run() { thing(); }\n",
    )]);
    assert!(calls(&raw).is_empty());
}

#[test]
fn test_unresolved_at_alias_does_not_infer_call_to_unrelated_definition() {
    let raw = build(&[
        ("unrelated.js", "export function ghost() {}\n"),
        (
            "caller.js",
            "import { ghost } from '@/nothing';\nexport function run() { ghost(); }\n",
        ),
    ]);
    assert!(calls(&raw).is_empty());
}
