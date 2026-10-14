use graphicle_memory::structure::{analyze, graph};
use serde_json::{Value, json};

fn relationships(files: &[(&str, &str)]) -> Value {
    let analyses: serde_json::Map<String, Value> = files
        .iter()
        .map(|(path, text)| ((*path).to_string(), analyze(path, text).unwrap()))
        .collect();
    graph(&Value::Object(analyses)).unwrap()
}

#[test]
fn imported_aliases_resolve_without_cross_file_name_guessing() {
    let result = relationships(&[
        (
            "main.py",
            "from util import transform as convert\nimport util as tools\ndef run():\n    convert()\n    tools.transform()\n",
        ),
        ("util.py", "def transform():\n    return 1\n"),
        ("unrelated.py", "def transform():\n    return 2\n"),
    ]);
    let calls = result["symbol_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{result}");
    assert!(calls.iter().all(|v| v["callee_path"] == "util.py"
        && v["callee"] == "transform"
        && v["runtime_verified"] == false));
}

#[test]
fn parameters_assignments_and_unrelated_nested_definitions_are_not_imported_calls() {
    let result = relationships(&[
        (
            "main.py",
            "from util import transform\ndef parameter(transform):\n    transform()\ndef assignment(value):\n    transform = value\n    transform()\ndef outer():\n    def hidden():\n        pass\ndef outside():\n    hidden()\n",
        ),
        ("util.py", "def transform():\n    return 1\n"),
    ]);
    assert_eq!(result["symbol_calls"], json!([]), "{result}");
    assert_eq!(result["unresolved_calls"].as_array().unwrap().len(), 3);
}

#[test]
fn typescript_aliases_namespaces_and_parameter_shadowing_are_distinct() {
    let result = relationships(&[
        (
            "main.ts",
            "import { transform as convert } from './util.js';\nimport * as tools from './util';\nexport function run() { convert(); tools.transform(); }\nexport function shadow(convert: () => number) { return convert(); }\n",
        ),
        ("util.ts", "export function transform() { return 1; }"),
    ]);
    let calls = result["symbol_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{result}");
    assert!(
        calls
            .iter()
            .all(|v| v["callee_path"] == "util.ts" && v["caller"] == "run")
    );
    assert_eq!(
        result["unresolved_calls"][0]["reason"],
        "shadowed_or_dynamic_binding"
    );
}

#[test]
fn jsonc_paths_inherit_relative_to_the_config_and_reject_manifest_escapes() {
    let result = relationships(&[
        (
            "config/base.json",
            "{\"compilerOptions\": {\"baseUrl\": \"../src\", \"paths\": {\"@app/*\": [\"*\"]}}}",
        ),
        (
            "tsconfig.json",
            "{ // original comment\n\"extends\": \"./config/base.json\", /* keep paths */ }",
        ),
        (
            "main.ts",
            "import { transform } from '@app/util';\nexport function run() { return transform(); }\n",
        ),
        ("src/util.ts", "export function transform() { return 1; }"),
    ]);
    assert_eq!(
        result["symbol_calls"][0]["callee_path"], "src/util.ts",
        "{result}"
    );
    let outside = relationships(&[
        (
            "tsconfig.json",
            "{\"compilerOptions\":{\"paths\":{\"@private/*\":[\"../private/*\"]}}}",
        ),
        (
            "main.ts",
            "import { transform } from '@private/util';\nexport function run() { return transform(); }\n",
        ),
        (
            "private/util.ts",
            "export function transform() { return 1; }",
        ),
    ]);
    assert_eq!(outside["symbol_calls"], json!([]), "{outside}");
}

#[test]
fn rust_explicit_use_binds_the_callable_inside_its_module() {
    let result = relationships(&[
        (
            "src/main.rs",
            "mod util;\nuse crate::util::transform as convert;\nfn run() -> i32 { convert() }",
        ),
        ("src/util.rs", "pub fn transform() -> i32 { 4 }"),
    ]);
    assert_eq!(
        result["symbol_calls"][0]["callee_path"], "src/util.rs",
        "{result}"
    );
    assert_eq!(result["symbol_calls"][0]["callee"], "transform");
}

#[test]
fn alias_updates_and_same_name_targets_do_not_reuse_stale_resolution() {
    let files = [
        (
            "tsconfig.json",
            "{\"compilerOptions\":{\"paths\":{\"@app/*\":[\"a/*\"]}}}",
        ),
        (
            "main.ts",
            "import { transform } from '@app/util';\nexport function run() { return transform(); }",
        ),
        ("a/util.ts", "export function transform() { return 1; }"),
        ("b/util.ts", "export function transform() { return 2; }"),
    ];
    assert_eq!(
        relationships(&files)["symbol_calls"][0]["callee_path"],
        "a/util.ts"
    );
    let mut changed = files;
    changed[0].1 = "{\"compilerOptions\":{\"paths\":{\"@app/*\":[\"b/*\"]}}}";
    assert_eq!(
        relationships(&changed)["symbol_calls"][0]["callee_path"],
        "b/util.ts"
    );
    let ambiguous = relationships(&[
        (
            "main.ts",
            "import { transform } from './util';\nfunction run() { transform(); }",
        ),
        ("util.ts", "export function transform() { return 1; }"),
        ("util.js", "export function transform() { return 2; }"),
    ]);
    assert_eq!(ambiguous["symbol_calls"], json!([]));
    assert!(
        ambiguous["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["reason"] == "ambiguous")
    );
}

#[test]
fn typescript_type_only_imports_keep_file_dependencies_without_runtime_calls() {
    let result = relationships(&[
        (
            "main.ts",
            "import type { transform as onlyType } from './util';\nimport { type transform as alsoType, transform as value } from './util';\nimport type * as types from './util';\nfunction run() { onlyType(); alsoType(); types.transform(); value(); }",
        ),
        ("util.ts", "export function transform() { return 1; }"),
    ]);
    let calls = result["symbol_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{result}");
    assert_eq!(calls[0]["callee_path"], "util.ts");
    assert_eq!(result["unresolved_calls"].as_array().unwrap().len(), 3);
    assert_eq!(result["edges"].as_array().unwrap().len(), 3);
}

#[test]
fn destructuring_reassignments_and_mutated_namespaces_are_not_static_import_calls() {
    let result = relationships(&[
        (
            "main.ts",
            "import { transform } from './util';\nimport * as tools from './util';\nfunction unpack(input: any) { const { transform } = input; transform(); }\nfunction replace(input: any) { transform = input; transform(); }\nfunction mutate(input: any) { tools.transform = input; tools.transform(); }",
        ),
        ("util.ts", "export function transform() { return 1; }"),
        (
            "main.py",
            "import util as tools\ndef mutate(value):\n    tools.transform = value\n    tools.transform()\n",
        ),
        ("util.py", "def transform():\n    return 1\n"),
        (
            "src/main.rs",
            "use crate::util::transform;\nfn run(value: fn()) { let transform = value; transform(); }",
        ),
        ("src/util.rs", "pub fn transform() {}"),
    ]);
    assert_eq!(result["symbol_calls"], json!([]), "{result}");
    assert_eq!(result["unresolved_calls"].as_array().unwrap().len(), 5);
}

#[test]
fn named_package_reexports_resolve_to_the_original_callable() {
    let result = relationships(&[
        ("pkg/foo.py", "def Foo():\n    return 1\n"),
        ("pkg/__init__.py", "from .foo import Foo as PublicFoo\n"),
        (
            "app.py",
            "from pkg import PublicFoo\ndef run():\n    return PublicFoo()\n",
        ),
        ("origin.ts", "export function clean() { return 1; }"),
        (
            "barrel.ts",
            "export { clean as publicClean } from './origin';",
        ),
        (
            "app.ts",
            "import { publicClean as clean } from './barrel';\nfunction run() { return clean(); }",
        ),
    ]);
    let calls = result["symbol_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "{result}");
    assert!(
        calls
            .iter()
            .any(|v| v["callee_path"] == "pkg/foo.py" && v["callee"] == "Foo")
    );
    assert!(
        calls
            .iter()
            .any(|v| v["callee_path"] == "origin.ts" && v["callee"] == "clean")
    );
}

#[test]
fn cyclic_shadowed_and_conflicting_exports_remain_unresolved() {
    let result = relationships(&[
        ("origin.ts", "export function clean() {}"),
        ("second.ts", "export function clean() {}"),
        (
            "conflict.ts",
            "export { clean } from './origin'; export { clean } from './second';",
        ),
        ("a.ts", "export { clean } from './b';"),
        ("b.ts", "export { clean } from './a';"),
        (
            "app.ts",
            "import { clean as conflict } from './conflict'; import { clean as cycle } from './a';\nfunction run() { conflict(); cycle(); }",
        ),
        ("pkg/foo.py", "def Foo():\n    pass\n"),
        ("pkg/__init__.py", "from .foo import Foo\nFoo = 123\n"),
        ("app.py", "from pkg import Foo\ndef run():\n    Foo()\n"),
    ]);
    assert_eq!(result["symbol_calls"], json!([]), "{result}");
    assert_eq!(result["unresolved_calls"].as_array().unwrap().len(), 3);
}
