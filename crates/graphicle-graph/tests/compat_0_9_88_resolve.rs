// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
//! Corpus assembly, cross-file resolution and AST cache changes of Graphify
//! 0.9.78-0.9.88, ported from the upstream tests of those releases.
//!
//! Tests that drive a whole extraction need only this engine's resolve
//! passes. Where an upstream test also needs a per-file field another part of
//! the 0.9.88 port emits (the Python receiver types, C# receiver chains, Java
//! receiver chains, Elixir module receivers, ...), the resolver is driven
//! with the per-file records that field produces, as `extract()` hands them
//! over.
use graphicle::api::{ExtractOptions, extract};
use graphicle::extractors::base::file_stem;
use graphicle::ids::make_id;
use serde_json::{Value, json};
use sha1::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// This thread's own (empty) language overrides: tests that declare some run
/// beside ones that must not see them.
fn isolate() {
    graphicle::rcfile::set_language_overrides(None);
}

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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    fn options(&self) -> ExtractOptions {
        ExtractOptions {
            root: Some(self.root.clone()),
            cache_root: Some(self.root.join("graphify-out")),
            parallel: false,
            ..Default::default()
        }
    }
    fn extract(&self, paths: &[PathBuf]) -> Value {
        extract(paths, &self.options()).unwrap()
    }
    fn extract_files(&self, files: &[(&str, &str)]) -> Value {
        let paths = files
            .iter()
            .map(|(rel, text)| self.write(rel, text))
            .collect::<Vec<_>>();
        self.extract(&paths)
    }
}

fn nodes(result: &Value) -> &Vec<Value> {
    result["nodes"].as_array().unwrap()
}
fn edges(result: &Value) -> &Vec<Value> {
    result["edges"].as_array().unwrap()
}
fn node<'a>(result: &'a Value, id: &str) -> Option<&'a Value> {
    nodes(result).iter().find(|n| n["id"] == id)
}
fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
/// Nodes labelled `label` with a source file (`_defs`).
fn defs<'a>(result: &'a Value, label: &str) -> Vec<&'a Value> {
    nodes(result)
        .iter()
        .filter(|n| n["label"] == label && !s(n, "source_file").is_empty())
        .collect()
}
/// Targets of `relation` edges labelled `label` (`_targets`).
fn targets<'a>(result: &'a Value, relation: &str, label: &str) -> Vec<&'a Value> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .filter_map(|e| node(result, s(e, "target")))
        .filter(|n| n["label"] == label)
        .collect()
}
fn pairs(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| (s(e, "source").to_string(), s(e, "target").to_string()))
        .collect()
}
fn id_of(result: &Value, label: &str) -> String {
    nodes(result)
        .iter()
        .find(|n| n["label"] == label)
        .and_then(|n| n["id"].as_str())
        .unwrap_or_else(|| panic!("no node labelled {label}"))
        .to_string()
}
fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

// ---------------------------------------------------------------------------
// Python import resolution (0.9.81 #4225, 0.9.84 #4261, #4269)
// ---------------------------------------------------------------------------

const WRAPPERS: &str = "from werkzeug.wrappers import Request as RequestBase\n\n\nclass Request(RequestBase):\n    pass\n\n\ndef wrap(req: RequestBase) -> RequestBase:\n    return RequestBase(req)\n";

fn build_pkg(fx: &Fixture, files: &[(&str, &str)]) -> Value {
    let mut all = vec![("pkg/__init__.py", "")];
    all.extend_from_slice(files);
    fx.extract_files(&all)
}

#[test]
fn external_base_aliased_in_a_same_named_module_is_not_a_self_loop() {
    isolate();
    let fx = Fixture::new();
    let r = build_pkg(&fx, &[("pkg/wrappers.py", WRAPPERS)]);
    assert!(!edges(&r).iter().any(|e| e["source"] == e["target"]));
    let inherits = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "inherits")
        .collect::<Vec<_>>();
    assert_eq!(inherits.len(), 1);
    let base = node(&r, s(inherits[0], "target")).unwrap();
    assert_eq!(base["label"], "RequestBase");
    assert!(s(base, "source_file").is_empty());
}

#[test]
fn uses_of_the_alias_do_not_point_at_the_local_class() {
    isolate();
    let fx = Fixture::new();
    let r = build_pkg(&fx, &[("pkg/wrappers.py", WRAPPERS)]);
    let local = nodes(&r)
        .iter()
        .find(|n| n["label"] == "Request" && !s(n, "source_file").is_empty())
        .unwrap();
    assert!(
        !edges(&r).iter().any(
            |e| e["target"] == local["id"] && matches!(s(e, "relation"), "uses" | "references")
        )
    );
}

#[test]
fn a_real_in_repo_import_still_resolves() {
    isolate();
    let fx = Fixture::new();
    let r = build_pkg(
        &fx,
        &[
            ("pkg/base.py", "class Request:\n    pass\n"),
            (
                "pkg/wrappers.py",
                "from pkg.base import Request as RequestBase\n\n\nclass Request(RequestBase):\n    pass\n",
            ),
        ],
    );
    let bases = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "inherits")
        .map(|e| s(node(&r, s(e, "target")).unwrap(), "source_file").to_string())
        .collect::<Vec<_>>();
    assert_eq!(bases, ["pkg/base.py"]);
}

#[test]
fn warm_rebuild_keeps_the_own_stem_guard() {
    isolate();
    let fx = Fixture::new();
    let key = |r: &Value| {
        let mut out = edges(r)
            .iter()
            .map(|e| {
                (
                    s(e, "source").to_string(),
                    s(e, "target").to_string(),
                    s(e, "relation").to_string(),
                )
            })
            .collect::<Vec<_>>();
        out.sort();
        out
    };
    let first = build_pkg(&fx, &[("pkg/wrappers.py", WRAPPERS)]);
    let warm = build_pkg(&fx, &[("pkg/wrappers.py", WRAPPERS)]);
    assert_eq!(key(&first), key(&warm));
    assert!(!edges(&warm).iter().any(|e| e["source"] == e["target"]));
}

#[test]
fn a_same_named_module_in_another_package_still_resolves() {
    isolate();
    for (i, importer) in [
        "from b.utils import Helper\n",
        "from proj.b.utils import Helper\n",
        "from installed.x.utils import Helper\n",
    ]
    .iter()
    .enumerate()
    {
        for b_first in [false, true] {
            let fx = Fixture::new();
            let local = format!("{importer}\n\nclass Local(Helper):\n    pass\n");
            let mut files = vec![
                ("proj/__init__.py", ""),
                ("proj/a/__init__.py", ""),
                ("proj/b/__init__.py", ""),
                ("proj/b/utils.py", "class Helper:\n    pass\n"),
                ("proj/a/utils.py", local.as_str()),
            ];
            if b_first {
                files.swap(3, 4);
            }
            let r = fx.extract_files(&files);
            let bases = edges(&r)
                .iter()
                .filter(|e| e["relation"] == "inherits")
                .map(|e| s(node(&r, s(e, "target")).unwrap(), "source_file").to_string())
                .collect::<Vec<_>>();
            assert_eq!(bases, ["proj/b/utils.py"], "case {i} b_first={b_first}");
        }
    }
}

#[test]
fn bare_stdlib_import_does_not_bind_to_same_stem_sibling() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("scripts/logging.py", "def configure():\n    return 1\n"),
        (
            "scripts/service.py",
            "import logging\n\ndef run():\n    return logging.getLogger(__name__)\n",
        ),
    ]);
    let imports = edges(&r)
        .iter()
        .filter(|e| e["source"] == "scripts_service" && e["relation"] == "imports")
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0]["target"], "logging");
    assert!(
        !pairs(&r, "calls")
            .iter()
            .any(|(_, t)| t.starts_with("scripts_logging"))
    );
}

#[test]
fn non_stdlib_sibling_still_repoints() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("scripts/greeter.py", "def greet():\n    return 42\n"),
        (
            "scripts/service.py",
            "import greeter\n\ndef run():\n    return greeter.greet()\n",
        ),
    ]);
    assert!(pairs(&r, "imports").contains(&pair("scripts_service", "scripts_greeter")));
    assert!(pairs(&r, "calls").contains(&pair("scripts_service_run", "scripts_greeter_greet")));
}

#[test]
fn stdlib_named_module_resolves_only_through_a_package() {
    isolate();
    use graphicle::extractors::resolution_facts::resolve_python_module_path;
    let fx = Fixture::new();
    let importer = fx.write("app/main.py", "import logging\n");
    fx.write("logging.py", "");
    assert_eq!(
        resolve_python_module_path("logging", &importer, &fx.root, 0),
        None
    );
    assert_eq!(
        resolve_python_module_path("logging.handlers", &importer, &fx.root, 0),
        None
    );
    let package = fx.write("logging/__init__.py", "");
    std::fs::remove_file(fx.root.join("logging.py")).unwrap();
    assert_eq!(
        resolve_python_module_path("logging", &importer, &fx.root, 0),
        Some(package)
    );
    // A relative import is never a stdlib import.
    fx.write("app/logging.py", "");
    assert_eq!(
        resolve_python_module_path("logging", &importer, &fx.root, 1),
        Some(fx.root.join("app/logging.py"))
    );
}

#[test]
fn qualified_annotation_does_not_bind_to_private_local_class() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "app/client.py",
            "import httpx\n\n\ndef read_body(response: httpx.Response) -> bytes:\n    return response.content\n",
        ),
        (
            "tests/test_fake.py",
            "class _Response:\n    \"\"\"Stand-in for a urllib response.\"\"\"\n\n    def read(self):\n        return b\"\"\n",
        ),
    ]);
    assert!(
        !pairs(&r, "references")
            .contains(&pair("app_client_read_body", "tests_test_fake_response"))
    );
    assert!(node(&r, "tests_test_fake_response").is_some());
}

#[test]
fn bare_underscore_class_does_not_absorb_sibling_reference() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "producer.py",
            "import external\n\n\ndef build(x: external.Thing) -> None:\n    return None\n",
        ),
        ("private.py", "class _Thing:\n    pass\n"),
    ]);
    assert!(!pairs(&r, "references").contains(&pair("producer_build", "private_thing")));
}

#[test]
fn node_label_key_keeps_underscores() {
    isolate();
    use graphicle::extract_merge::node_label_key;
    assert_eq!(node_label_key("_Response", false), "_Response");
    assert_ne!(
        node_label_key("_Response", false),
        node_label_key("Response", false)
    );
    assert_eq!(node_label_key("Foo()", false), node_label_key("Foo", false));
    assert_eq!(node_label_key("Map<K, V>", true), "mapkv");
}

#[test]
fn reexported_type_reference_binds_to_single_located_def() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("config/__init__.py", ""),
        ("config/settings/__init__.py", "from ._models import Settings\n"),
        (
            "config/settings/_models.py",
            "class Settings:\n    def load(self):\n        return 1\n",
        ),
        (
            "app/service.py",
            "from config.settings import Settings\n\n\ndef run(s: Settings):\n    return s.load()\n",
        ),
    ]);
    assert!(
        !nodes(&r)
            .iter()
            .any(|n| n["label"] == "Settings" && s(n, "source_file").is_empty())
    );
    assert!(
        pairs(&r, "references")
            .contains(&pair("app_service_run", "config_settings_models_settings"))
    );
}

#[test]
fn unresolved_type_references_coalesce_to_unique_def() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("pkg/_models.py", "class Settings:\n    pass\n"),
        ("a.py", "class A(Settings):\n    pass\n"),
        ("b.py", "class B(Settings):\n    pass\n"),
    ]);
    assert!(
        !nodes(&r)
            .iter()
            .any(|n| n["label"] == "Settings" && s(n, "source_file").is_empty())
    );
    let inherits = pairs(&r, "inherits");
    assert!(inherits.contains(&pair("a_a", "pkg_models_settings")));
    assert!(inherits.contains(&pair("b_b", "pkg_models_settings")));
}

// ---------------------------------------------------------------------------
// Rust stub rewire (0.9.85 #4283)
// ---------------------------------------------------------------------------

fn thing_targets_of_make(files: &[(&str, &str)]) -> Vec<String> {
    let fx = Fixture::new();
    let mut paths = files
        .iter()
        .map(|(rel, text)| fx.write(rel, text))
        .collect::<Vec<_>>();
    paths.sort();
    let r = fx.extract(&paths);
    let make = id_of(&r, "make()");
    edges(&r)
        .iter()
        .filter(|e| e["relation"] == "references" && e["source"] == make.as_str())
        .filter_map(|e| node(&r, s(e, "target")))
        .filter(|n| n["label"] == "Thing")
        .map(|n| s(n, "source_file").to_string())
        .collect()
}

#[test]
fn rust_cross_file_reference_skips_impl_blocks_in_other_files() {
    isolate();
    let fx = Fixture::new();
    let mut paths = vec![
        fx.write("a.rs", "pub struct Thing {\n    pub n: u32,\n}\n"),
        fx.write(
            "b.rs",
            "use crate::a::Thing;\n\nimpl Thing {\n    pub fn new() -> Self {\n        Thing { n: 0 }\n    }\n}\n",
        ),
        fx.write(
            "c.rs",
            "use crate::a::Thing;\n\npub fn make(t: Thing) -> u32 {\n    t.n\n}\n",
        ),
    ];
    paths.sort();
    for _ in 0..2 {
        let r = fx.extract(&paths);
        let things = nodes(&r)
            .iter()
            .filter(|n| n["label"] == "Thing")
            .collect::<Vec<_>>();
        assert!(things.iter().all(|n| !s(n, "source_file").is_empty()));
        let declared = things
            .iter()
            .find(|n| s(n, "source_file").ends_with("a.rs"))
            .unwrap();
        let make = id_of(&r, "make()");
        assert!(pairs(&r, "references").contains(&pair(&make, s(declared, "id"))));
    }
}

#[test]
fn rust_cross_file_reference_to_twice_declared_type_keeps_its_stub() {
    isolate();
    assert_eq!(
        thing_targets_of_make(&[
            ("a.rs", "pub struct Thing;\n"),
            (
                "b.rs",
                "pub struct Thing;\n\nimpl Thing {\n    pub fn new() -> Self {\n        Thing\n    }\n}\n"
            ),
            ("c.rs", "pub fn make(t: Thing) {}\n"),
        ]),
        [""]
    );
}

#[test]
fn rust_declaration_tiebreak_keeps_stub_beside_another_language() {
    isolate();
    assert_eq!(
        thing_targets_of_make(&[
            ("a.rs", "pub struct Thing;\n"),
            ("b.ts", "export class Thing {}\n"),
            ("c.rs", "pub fn make(t: Thing) {}\n"),
        ]),
        [""]
    );
}

#[test]
fn rust_declaration_tiebreak_keeps_stub_for_declarations_folded_in_one_node() {
    isolate();
    assert_eq!(
        thing_targets_of_make(&[
            (
                "a.rs",
                "mod x {\n    pub struct Thing;\n}\nmod y {\n    pub struct Thing;\n}\n"
            ),
            ("b.rs", "impl Thing {\n    pub fn new() {}\n}\n"),
            ("c.rs", "pub fn make(t: Thing) {}\n"),
        ]),
        [""]
    );
}

// ---------------------------------------------------------------------------
// JS/TS symbol facts (0.9.80 #4143, 0.9.83 #4257, 0.9.85)
// ---------------------------------------------------------------------------

fn unresolved_reexport_id(path: &str) -> String {
    let salt = format!("{:x}", sha1::Sha1::digest(path.as_bytes()));
    make_id([
        "unresolved_reexport",
        file_stem(Path::new(path)).as_str(),
        &salt[..12],
    ])
}

#[test]
fn unresolved_ts_reexport_ids_are_portable_across_checkout_paths() {
    isolate();
    for (specifier, target) in [
        ("./missing", None),
        ("./directory", Some("directory/index.d.ts")),
        ("./type", Some("type.d.ts")),
    ] {
        for statement in [
            format!("export * from '{specifier}'\n"),
            format!("export * as values from '{specifier}'\n"),
            format!("export {{ Value }} from '{specifier}'\n"),
        ] {
            let outer = tempfile::tempdir().unwrap();
            let outer_root = outer.path().canonicalize().unwrap();
            let build = |checkout: &str| {
                let root = outer_root.join(checkout);
                if let Some(target) = target {
                    let p = root.join(target);
                    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                    std::fs::write(&p, "export interface Value { id: string }\n").unwrap();
                }
                std::fs::create_dir_all(&root).unwrap();
                let barrel = root.join("index.ts");
                std::fs::write(&barrel, &statement).unwrap();
                let r = extract(
                    std::slice::from_ref(&barrel),
                    &ExtractOptions {
                        cache_root: Some(root.clone()),
                        parallel: false,
                        ..Default::default()
                    },
                )
                .unwrap();
                let projection = json!({"nodes": r["nodes"], "edges": r["edges"]});
                let text = projection.to_string();
                assert!(
                    !text.contains(root.to_string_lossy().as_ref()),
                    "{statement}"
                );
                assert!(!text.contains(&make_id([root.to_string_lossy().as_ref()])));
                let expected = unresolved_reexport_id(specifier.trim_start_matches("./"));
                assert!(
                    edges(&r)
                        .iter()
                        .any(|e| e["relation"] == "re_exports" && e["target"] == expected.as_str()),
                    "{statement}: {projection}"
                );
                projection
            };
            assert_eq!(
                build("checkout-a"),
                build("nested/checkout-b"),
                "{statement}"
            );
        }
    }
}

#[test]
fn unresolved_ts_reexport_does_not_bind_a_colliding_context_node() {
    isolate();
    let fx = Fixture::new();
    let barrel = fx.write("repo/index.ts", "export * from './a-b/x'\n");
    let root = fx.root.join("repo");
    let context = json!({"id": make_id(["a/b/x"]), "label": "x.ts", "file_type": "code",
        "source_file": "a/b/x.ts", "source_location": "L1", "_origin": "ast"});
    let r = extract(
        &[barrel],
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root),
            parallel: false,
            resolution_context_nodes: vec![context.clone()],
            ..Default::default()
        },
    )
    .unwrap();
    let target = edges(&r)
        .iter()
        .find(|e| e["relation"] == "re_exports")
        .map(|e| s(e, "target").to_string())
        .unwrap();
    assert_eq!(target, unresolved_reexport_id("a-b/x"));
    assert_ne!(target, s(&context, "id"));
}

#[test]
fn re_export_survives_for_both_files_whose_ids_collide() {
    isolate();
    for reverse in [false, true] {
        let fx = Fixture::new();
        let mut paths = vec![
            fx.write("lib/util.ts", "export function helper() { return 1; }\n"),
            fx.write("a-b/x.ts", "export * from '../lib/util'\n"),
            fx.write("a/b/x.ts", "export * from '../../lib/util'\n"),
        ];
        if reverse {
            paths.reverse();
        }
        let r = extract(
            &paths,
            &ExtractOptions {
                cache_root: Some(fx.root.clone()),
                parallel: false,
                ..Default::default()
            },
        )
        .unwrap();
        let mut exporters = edges(&r)
            .iter()
            .filter(|e| e["relation"] == "re_exports" && e["target"] == "lib_util")
            .map(|e| s(node(&r, s(e, "source")).unwrap(), "source_file").to_string())
            .collect::<Vec<_>>();
        exporters.sort();
        assert_eq!(exporters, ["a-b/x.ts", "a/b/x.ts"], "reverse={reverse}");
    }
}

#[test]
fn export_default_string_is_not_a_module_specifier() {
    isolate();
    let fx = Fixture::new();
    fx.write("tsconfig.json", r#"{"compilerOptions": {"baseUrl": "."}}"#);
    let long = format!(
        "export default 'data:image/png;base64,{}';\n",
        "A".repeat(5000)
    );
    let r = fx.extract_files(&[
        ("logo.js", "export default 'data:image/png;base64,AAAA';\n"),
        ("src/long.js", &long),
        (
            "src/main.js",
            &format!("import x from '{}';\n", "a".repeat(5000)),
        ),
    ]);
    // The symbol facts read no module from `export default '...'` (the
    // per-file import edges are the engine's part of the fix).
    assert!(edges(&r).iter().all(|e| e["relation"] != "re_exports"));
}

#[test]
fn js_import_paths_follow_declared_languages_and_ejs() {
    isolate();
    use graphicle::extractors::resolution_js::resolve_js_import_path;
    let fx = Fixture::new();
    let main = fx.write("src/util.ts", "export const a = 1;\n");
    assert_eq!(resolve_js_import_path(&fx.root.join("src/util")), main);
    let declared = fx.write("src/view.tpl", "export const b = 1;\n");
    let candidate = fx.root.join("src/view");
    assert_eq!(resolve_js_import_path(&candidate), candidate);
    let index = fx.write("src/widgets/index.tpl", "export const c = 1;\n");
    graphicle::rcfile::set_language_overrides(Some(&BTreeMap::from([(
        ".tpl".to_string(),
        ".ts".to_string(),
    )])));
    assert_eq!(resolve_js_import_path(&candidate), declared);
    assert_eq!(resolve_js_import_path(&fx.root.join("src/widgets")), index);
    // A native suffix remapped away from JS is no longer a JS candidate.
    graphicle::rcfile::set_language_overrides(Some(&BTreeMap::from([(
        ".ts".to_string(),
        ".php".to_string(),
    )])));
    assert_eq!(
        resolve_js_import_path(&fx.root.join("src/util")),
        fx.root.join("src/util")
    );
    graphicle::rcfile::set_language_overrides(None);
    // `.ejs` resolves once it is a JS-family suffix (models, 0.9.88 #3811).
    if graphicle::extractors::models::JS_CACHE_BYPASS_SUFFIXES.contains(&".ejs") {
        let page = fx.write("views/page.ejs", "<% include('x') %>\n");
        assert_eq!(resolve_js_import_path(&fx.root.join("views/page")), page);
    }
}

// ---------------------------------------------------------------------------
// C# type resolution (0.9.81 #4204, 0.9.83 #4235, 0.9.84 #4247)
// ---------------------------------------------------------------------------

fn resolved<'a>(result: &'a Value, relation: &str, label: &str) -> Vec<&'a Value> {
    targets(result, relation, label)
        .into_iter()
        .filter(|n| !s(n, "source_file").is_empty())
        .collect()
}

fn assert_enclosing_namespace_binds(r: &Value) {
    let widget = defs(r, "Widget")[0];
    let base = defs(r, "Base")[0];
    let inherits = resolved(r, "inherits", "Base");
    assert!(!inherits.is_empty() && inherits[0]["id"] == base["id"]);
    let refs = resolved(r, "references", "Widget");
    assert!(!refs.is_empty() && refs[0]["id"] == widget["id"]);
    let make = id_of(r, ".Make()");
    assert!(pairs(r, "calls").contains(&pair(&make, s(widget, "id"))));
    assert!(
        !nodes(r)
            .iter()
            .any(|n| matches!(s(n, "label"), "Widget" | "Base") && s(n, "source_file").is_empty())
    );
}

#[test]
fn csharp_enclosing_namespace_dotted() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "Demo.cs",
            "namespace Demo {\n    public class Base {}\n    public class Widget {}\n}\n",
        ),
        (
            "Inner.cs",
            "namespace Demo.Inner {\n    public class Registry {\n        public Widget Item;\n    }\n    public class Factory : Base {\n        public object Make() {\n            return new Widget();\n        }\n    }\n}\n",
        ),
    ]);
    assert_eq!(defs(&r, "Widget").len(), 1);
    assert_eq!(defs(&r, "Base").len(), 1);
    assert_enclosing_namespace_binds(&r);
}

#[test]
fn csharp_enclosing_namespace_nested_blocks() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[(
        "nested.cs",
        "namespace Demo {\n    public class Base {}\n    public class Widget {}\n    namespace Inner {\n        public class Registry {\n            public Widget Item;\n        }\n        public class Factory : Base {\n            public object Make() {\n                return new Widget();\n            }\n        }\n    }\n}\n",
    )]);
    assert_enclosing_namespace_binds(&r);
}

#[test]
fn csharp_enclosing_namespace_file_scoped() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "Demo.cs",
            "namespace Demo;\npublic class Base {}\npublic class Widget {}\n",
        ),
        (
            "Scoped.cs",
            "namespace Demo.Scoped;\npublic class Registry {\n    public Widget Item;\n}\npublic class Factory : Base {\n    public object Make() {\n        return new Widget();\n    }\n}\n",
        ),
    ]);
    assert_enclosing_namespace_binds(&r);
}

#[test]
fn csharp_enclosing_namespace_multilevel_shadowing() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "outer.cs",
            "namespace Demo {\n    public class Widget { public int Level = 1; }\n}\n",
        ),
        (
            "inner.cs",
            "namespace Demo.Inner {\n    public class Widget { public int Level = 2; }\n}\n",
        ),
        (
            "deep.cs",
            "namespace Demo.Inner.Deep {\n    public class Consumer {\n        public Widget Item;\n    }\n}\n",
        ),
    ]);
    let inner = defs(&r, "Widget")
        .into_iter()
        .find(|n| s(n, "source_file").contains("inner.cs"))
        .unwrap();
    let refs = resolved(&r, "references", "Widget");
    assert!(!refs.is_empty());
    assert!(refs.iter().all(|n| n["id"] == inner["id"]));
}

#[test]
fn csharp_enclosing_namespace_regression_controls() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[(
        "types.cs",
        "namespace Demo {\n    public class OuterType {}\n    public class CollidingType {}\n}\nnamespace Other {\n    public class ImportedType {}\n}\nnamespace Demo.Inner {\n    using Other;\n    public class CollidingType {}\n    public class Tester {\n        public CollidingType Local;\n        public ImportedType Imported;\n        public UnknownType Missing;\n    }\n}\n",
    )]);
    let local = defs(&r, "CollidingType")
        .into_iter()
        .find(|n| n["metadata"]["namespace"] == "Demo.Inner")
        .unwrap();
    let imported = defs(&r, "ImportedType")[0];
    let colliding = resolved(&r, "references", "CollidingType");
    assert!(!colliding.is_empty() && colliding.iter().all(|n| n["id"] == local["id"]));
    let imports = resolved(&r, "references", "ImportedType");
    assert!(!imports.is_empty() && imports.iter().all(|n| n["id"] == imported["id"]));
    let missing = targets(&r, "references", "UnknownType");
    assert!(!missing.is_empty() && missing.iter().all(|n| s(n, "source_file").is_empty()));
}

#[test]
fn csharp_cross_namespace_refs_still_do_not_misbind() {
    isolate();
    for source in [
        "namespace B { class Use : T {} } namespace C { class T {} }\n",
        "namespace B { class T {} } namespace C { class Use : T {} }\n",
    ] {
        let fx = Fixture::new();
        let r = fx.extract_files(&[("x.cs", source)]);
        assert!(resolved(&r, "inherits", "T").is_empty(), "{source}");
    }
    let fx = Fixture::new();
    let r = fx.extract_files(&[(
        "y.cs",
        "namespace Game { class Damage {} class Use : Game {} }\n",
    )]);
    let namespaces = nodes(&r)
        .iter()
        .filter(|n| n["type"] == "namespace")
        .map(|n| s(n, "id").to_string())
        .collect::<BTreeSet<_>>();
    assert!(
        !edges(&r)
            .iter()
            .any(|e| e["relation"] == "inherits" && namespaces.contains(s(e, "target")))
    );
}

const LAYER: &str =
    "namespace Demo\n{\n    public class Layer\n    {\n        public class Record {}\n    }\n}\n";

#[test]
fn csharp_using_static_resolves_nested_type_reference() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("layer.cs", LAYER),
        (
            "consumer.cs",
            "using static Demo.Layer;\nnamespace Consumer\n{\n    public class Service\n    {\n        public Record value;\n    }\n}\n",
        ),
    ]);
    let record = defs(&r, "Record");
    assert_eq!(record.len(), 1);
    let refs = resolved(&r, "references", "Record");
    assert!(!refs.is_empty() && refs[0]["id"] == record[0]["id"]);
}

#[test]
fn csharp_using_static_resolves_nested_constructor_call() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("layer.cs", LAYER),
        (
            "consumer.cs",
            "using static Demo.Layer;\nnamespace Consumer\n{\n    public class Service\n    {\n        public void Run()\n        {\n            var r = new Record();\n        }\n    }\n}\n",
        ),
    ]);
    let record = defs(&r, "Record");
    assert_eq!(record.len(), 1);
    let calls = resolved(&r, "calls", "Record");
    assert!(!calls.is_empty() && calls[0]["id"] == record[0]["id"]);
}

#[test]
fn csharp_using_static_multi_level_nested_type() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "print.cs",
            "namespace Demo\n{\n    public class Print\n    {\n        public class Engine\n        {\n            public class Info {}\n        }\n    }\n}\n",
        ),
        (
            "consumer.cs",
            "using static Demo.Print.Engine;\nnamespace Consumer\n{\n    public class App\n    {\n        public Info value;\n    }\n}\n",
        ),
    ]);
    let info = defs(&r, "Info");
    assert_eq!(info.len(), 1);
    let refs = resolved(&r, "references", "Info");
    assert!(!refs.is_empty() && refs[0]["id"] == info[0]["id"]);
}

#[test]
fn csharp_using_static_scoped_to_namespace_block() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("layer.cs", LAYER),
        (
            "consumer.cs",
            "namespace Good\n{\n    using static Demo.Layer;\n    public class Service\n    {\n        public Record goodValue;\n    }\n}\nnamespace Bad\n{\n    public class OtherService\n    {\n        public Record badValue;\n    }\n}\n",
        ),
    ]);
    let record = defs(&r, "Record");
    assert_eq!(record.len(), 1);
    let (mut good, mut bad) = (false, false);
    for e in edges(&r).iter().filter(|e| e["relation"] == "references") {
        let Some(target) = node(&r, s(e, "target")).filter(|n| n["label"] == "Record") else {
            continue;
        };
        match node(&r, s(e, "source")).map_or("", |n| s(n, "label")) {
            "Service" => {
                good = true;
                assert_eq!(target["id"], record[0]["id"]);
            }
            "OtherService" => {
                bad = true;
                assert!(s(target, "source_file").is_empty());
            }
            _ => {}
        }
    }
    assert!(good && bad);
}

#[test]
fn csharp_unimported_ambiguous_or_conflicting_nested_types_dangle() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("layer.cs", LAYER),
        (
            "consumer.cs",
            "namespace Consumer\n{\n    public class Service\n    {\n        public Record value;\n    }\n}\n",
        ),
    ]);
    assert!(resolved(&r, "references", "Record").is_empty());
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "layer_a.cs",
            "namespace DemoA\n{\n    public class LayerA\n    {\n        public class Record {}\n    }\n}\n",
        ),
        (
            "layer_b.cs",
            "namespace DemoB\n{\n    public class LayerB\n    {\n        public class Record {}\n    }\n}\n",
        ),
        (
            "consumer.cs",
            "using static DemoA.LayerA;\nusing static DemoB.LayerB;\nnamespace Consumer\n{\n    public class Service\n    {\n        public Record value;\n    }\n}\n",
        ),
    ]);
    assert!(resolved(&r, "references", "Record").is_empty());
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        ("other.cs", "namespace Other\n{\n    public class Record {}\n}\n"),
        ("layer.cs", LAYER),
        (
            "consumer.cs",
            "using Other;\nusing static Demo.Layer;\nnamespace Consumer\n{\n    public class Service\n    {\n        public Record value;\n    }\n}\n",
        ),
    ]);
    assert!(resolved(&r, "references", "Record").is_empty());
}

#[test]
fn csharp_external_type_shares_one_placeholder_across_files() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "A.cs",
            "using System.Collections.Generic;\nnamespace Demo { public class A { public List<int> Items; } }\n",
        ),
        (
            "B.cs",
            "using System.Collections.Generic;\nnamespace Demo { public class B { public List<string> Names; } }\n",
        ),
    ]);
    let stubs = nodes(&r)
        .iter()
        .filter(|n| n["label"] == "List" && s(n, "source_file").is_empty())
        .collect::<Vec<_>>();
    assert_eq!(stubs.len(), 1, "{stubs:?}");
    let files = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "references" && e["target"] == stubs[0]["id"])
        .map(|e| s(e, "source_file").rsplit('/').next().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(files.len(), 2, "{files:?}");
}

#[test]
fn csharp_bare_call_to_a_delegate_field_does_not_bind_to_an_enum_case() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[(
        "Eval.cs",
        "namespace Demo {\n    public delegate float Curve(float t);\n    public class Eval {\n        public enum Kind { Linear, Delegate }\n        public Kind Mode;\n        public Curve Delegate;\n        public float Evaluate(float t) {\n            switch (Mode) {\n                case Kind.Delegate: return Delegate != null ? Delegate(t) : t;\n                default: return t;\n            }\n        }\n    }\n}\n",
    )]);
    let kind = id_of(&r, "Kind");
    let member = pairs(&r, "case_of")
        .into_iter()
        .find(|(source, target)| {
            *source == kind
                && node(&r, target)
                    .is_some_and(|n| s(n, "label").trim_matches(['.', '(', ')']) == "Delegate")
        })
        .map(|(_, target)| target)
        .unwrap();
    assert!(!pairs(&r, "calls").iter().any(|(_, t)| *t == member));
}

#[test]
fn csharp_bare_calls_never_bind_enum_members_or_fields() {
    isolate();
    // The shared bare-name pass: a C# raw call (not only `new X()`) never
    // lands on an enum member or a field/property (#4245).
    let nodes = vec![
        n("eval", "Eval", "Eval.cs"),
        n("eval_kind", "Kind", "Eval.cs"),
        n("eval_kind_delegate", "Delegate", "Eval.cs"),
        n("eval_curve", "Curve", "Eval.cs"),
        n("other", "Other", "Other.cs"),
        n("other_run", ".Run()", "Other.cs"),
    ];
    let edges = vec![
        e("eval_kind", "eval_kind_delegate", "case_of", "Eval.cs"),
        e("eval", "eval_curve", "defines", "Eval.cs"),
    ];
    for (callee, lang) in [
        ("Delegate", "csharp"),
        ("Curve", "csharp"),
        ("Delegate", "java"),
    ] {
        let rc = json!({"caller_nid": "other_run", "callee": callee, "lang": lang,
            "source_file": "Other.cs", "source_location": "L2"});
        let mut out = edges.clone();
        graphicle::extract::bind_raw_calls(&[&rc], &nodes, &mut out, &[], Path::new("/"));
        let bound = out.len() > edges.len();
        assert_eq!(bound, lang != "csharp", "{callee} from {lang}");
    }
}

#[test]
fn csharp_bare_call_prefers_the_single_own_type_candidate() {
    isolate();
    // Two same-named methods: the caller's own type's binds (merged partial
    // halves live in different files), with import-level confidence.
    let nodes = vec![
        n("svc", "Svc", "a.cs"),
        n("svc_run", ".Run()", "a.cs"),
        n("svc_save", ".Save()", "b.cs"),
        n("repo", "Repo", "c.cs"),
        n("repo_save", ".Save()", "c.cs"),
    ];
    let edges = vec![
        e("svc", "svc_run", "method", "a.cs"),
        e("svc", "svc_save", "method", "b.cs"),
        e("repo", "repo_save", "method", "c.cs"),
    ];
    let rc = json!({"caller_nid": "svc_run", "callee": "Save", "lang": "csharp",
        "source_file": "a.cs", "source_location": "L2"});
    let mut out = edges.clone();
    graphicle::extract::bind_raw_calls(&[&rc], &nodes, &mut out, &[], Path::new("/"));
    assert_eq!(
        found(&out[edges.len()..]),
        BTreeSet::from([quad("svc_run", "svc_save", "calls", "EXTRACTED")])
    );
}

#[test]
fn csharp_element_access_member_call_still_binds_to_the_real_method() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[(
        "Handles.cs",
        "namespace Demo {\n    public enum State { Pause, Play }\n    public class Handle { public void Pause() { } }\n    public class Injector {\n        public Handle[] Handles;\n        public void StopAll() {\n            for (int i = 0; i < Handles.Length; i++) { Handles[i].Pause(); }\n        }\n    }\n}\n",
    )]);
    let state = id_of(&r, "State");
    let member = pairs(&r, "case_of")
        .into_iter()
        .find(|(source, _)| *source == state)
        .map(|(_, target)| target)
        .unwrap();
    let pause = id_of(&r, ".Pause()");
    let stop_all = id_of(&r, ".StopAll()");
    let calls = pairs(&r, "calls");
    assert!(calls.contains(&pair(&stop_all, &pause)));
    assert!(!calls.contains(&pair(&stop_all, &member)));
}

#[test]
fn csharp_property_receiver_shadows_same_named_type() {
    isolate();
    let fx = Fixture::new();
    let r = fx.extract_files(&[
        (
            "Store.cs",
            "public interface IStore { void Save(); }\npublic class Store : IStore { public void Save() { } }\n",
        ),
        (
            "Editor.cs",
            "public class Editor {\n    object _obj;\n    IStore Store => (IStore)_obj;\n    public void Run() { Store.Save(); }\n}\n",
        ),
    ]);
    let run = nodes(&r)
        .iter()
        .find(|n| n["label"] == ".Run()" && s(n, "id").contains("editor"))
        .map(|n| s(n, "id").to_string())
        .unwrap();
    let istore_save = nodes(&r)
        .iter()
        .find(|n| n["label"] == ".Save()" && s(n, "id").contains("istore"))
        .map(|n| s(n, "id").to_string())
        .unwrap();
    let store_save = nodes(&r)
        .iter()
        .find(|n| n["label"] == ".Save()" && n["id"] != istore_save.as_str())
        .map(|n| s(n, "id").to_string())
        .unwrap();
    let calls = pairs(&r, "calls");
    assert!(calls.contains(&pair(&run, &istore_save)), "{calls:?}");
    assert!(!calls.contains(&pair(&run, &store_save)));
}

// ---------------------------------------------------------------------------
// Member-call resolvers over the per-file records the 0.9.88 extractors emit
// ---------------------------------------------------------------------------

fn n(id: &str, label: &str, file: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "code", "source_file": file, "source_location": "L1"})
}
fn e(source: &str, target: &str, relation: &str, file: &str) -> Value {
    json!({"source": source, "target": target, "relation": relation, "context": relation,
        "confidence": "EXTRACTED", "source_file": file, "source_location": "L1", "weight": 1.0})
}
fn call(caller: &str, callee: &str, receiver: &str, file: &str, extra: Value) -> Value {
    let mut rc = json!({"caller_nid": caller, "callee": callee, "receiver": receiver,
        "is_member_call": true, "source_file": file, "source_location": "L9"});
    for (k, v) in extra.as_object().unwrap() {
        rc[k] = v.clone();
    }
    rc
}
fn resolve(
    lang: &str,
    raw: Vec<Value>,
    mut nodes: Vec<Value>,
    mut edges: Vec<Value>,
) -> Vec<Value> {
    let before = edges.len();
    let per = vec![json!({"nodes": [], "edges": [], "raw_calls": raw})];
    graphicle::extract_members::resolve_members(lang, &per, &mut nodes, &mut edges);
    edges.split_off(before)
}
fn found(added: &[Value]) -> BTreeSet<(String, String, String, String)> {
    added
        .iter()
        .map(|e| {
            (
                s(e, "source").to_string(),
                s(e, "target").to_string(),
                s(e, "relation").to_string(),
                s(e, "confidence").to_string(),
            )
        })
        .collect()
}
fn quad(a: &str, b: &str, rel: &str, conf: &str) -> (String, String, String, String) {
    (a.into(), b.into(), rel.into(), conf.into())
}

/// `client.py` defines `Client.read()`; `app.py` (importing it) has `run()`.
fn python_corpus() -> (Vec<Value>, Vec<Value>) {
    let nodes = vec![
        n("client", "client.py", "client.py"),
        n("client_client", "Client", "client.py"),
        n("client_client_read", ".read()", "client.py"),
        n("app", "app.py", "app.py"),
        n("app_run", "run()", "app.py"),
        n("app_handler", "Handler", "app.py"),
        n("app_handler_handle", ".handle()", "app.py"),
        n("app_handler_handle_inner", "inner()", "app.py"),
    ];
    let edges = vec![
        e("client", "client_client", "contains", "client.py"),
        e("client_client", "client_client_read", "method", "client.py"),
        e("app", "app_run", "contains", "app.py"),
        e("app", "app_handler", "contains", "app.py"),
        e("app_handler", "app_handler_handle", "method", "app.py"),
        e(
            "app_handler_handle",
            "app_handler_handle_inner",
            "contains",
            "app.py",
        ),
    ];
    (nodes, edges)
}

#[test]
fn python_typed_receiver_arm_needs_the_class_to_be_visible() {
    isolate();
    let (nodes, mut edges) = python_corpus();
    let typed = |caller: &str| {
        call(
            caller,
            "read",
            "request",
            "app.py",
            json!({"receiver_type": "Client"}),
        )
    };
    // Not imported: no edge.
    assert!(
        resolve(
            "python",
            vec![typed("app_run")],
            nodes.clone(),
            edges.clone()
        )
        .is_empty()
    );
    edges.push(e("app", "client_client", "imports", "app.py"));
    let added = resolve(
        "python",
        vec![
            typed("app_run"),
            // A nested function finds its file by climbing its parents (#4227).
            typed("app_handler_handle_inner"),
            typed("app_handler_handle"),
        ],
        nodes.clone(),
        edges.clone(),
    );
    assert_eq!(
        found(&added),
        BTreeSet::from([
            quad("app_run", "client_client_read", "calls", "INFERRED"),
            quad(
                "app_handler_handle_inner",
                "client_client_read",
                "calls",
                "INFERRED"
            ),
            quad(
                "app_handler_handle",
                "client_client_read",
                "calls",
                "INFERRED"
            ),
        ])
    );
    assert!(added.iter().all(|e| e["confidence_score"] == 0.85));
    // A capitalised receiver is the class arm, not the typed arm.
    let class_arm = resolve(
        "python",
        vec![call(
            "app_run",
            "read",
            "Client",
            "app.py",
            json!({"receiver_type": "Other"}),
        )],
        nodes,
        edges,
    );
    assert_eq!(
        found(&class_arm),
        BTreeSet::from([quad("app_run", "client_client_read", "calls", "EXTRACTED")])
    );
}

#[test]
fn python_self_attr_arm_binds_a_unique_imported_class() {
    isolate();
    let (nodes, mut edges) = python_corpus();
    let raw = || {
        vec![call(
            "app_handler_handle",
            "read",
            "self.client",
            "app.py",
            json!({"_python_self_attr_type": "Client"}),
        )]
    };
    assert!(resolve("python", raw(), nodes.clone(), edges.clone()).is_empty());
    edges.push(e("app", "client_client", "imports_from", "app.py"));
    assert_eq!(
        found(&resolve("python", raw(), nodes, edges)),
        BTreeSet::from([quad(
            "app_handler_handle",
            "client_client_read",
            "calls",
            "INFERRED"
        )])
    );
}

/// `base.py` defines `Base.greet()` and `Base.items` (a property);
/// `child.py` imports Base and defines `Child(Base).run()`.
fn inheritance_corpus(base_target: &str) -> (Vec<Value>, Vec<Value>) {
    let mut items = n("base_base_items", ".items()", "base.py");
    items["_py_property"] = json!(true);
    let nodes = vec![
        n("base", "base.py", "base.py"),
        n("base_base", "Base", "base.py"),
        n("base_base_greet", ".greet()", "base.py"),
        items,
        n("child", "child.py", "child.py"),
        n("child_child", "Child", "child.py"),
        n("child_child_run", ".run()", "child.py"),
        json!({"id": "base_stub", "label": "Base", "file_type": "code", "source_file": "", "source_location": ""}),
    ];
    let edges = vec![
        e("base", "base_base", "contains", "base.py"),
        e("base_base", "base_base_greet", "method", "base.py"),
        e("base_base", "base_base_items", "method", "base.py"),
        e("child", "child_child", "contains", "child.py"),
        e("child_child", "child_child_run", "method", "child.py"),
        e("child_child", base_target, "inherits", "child.py"),
        e("child", "base_base", "imports", "child.py"),
    ];
    (nodes, edges)
}

#[test]
fn python_inherited_self_cls_and_super_calls_cross_files() {
    isolate();
    for base_target in ["base_base", "base_stub"] {
        let (nodes, edges) = inheritance_corpus(base_target);
        let added = resolve(
            "python",
            vec![
                call("child_child_run", "greet", "self", "child.py", json!({})),
                call(
                    "child_child_run",
                    "greet",
                    "super",
                    "child.py",
                    json!({"_python_super_args": true}),
                ),
            ],
            nodes.clone(),
            edges.clone(),
        );
        assert_eq!(
            found(&added),
            BTreeSet::from([quad(
                "child_child_run",
                "base_base_greet",
                "calls",
                "EXTRACTED"
            )]),
            "{base_target}"
        );
        // Opaque bases stop the walk.
        let mut opaque = nodes.clone();
        opaque[5]["metadata"] = json!({"python_opaque_bases": ["mod.Base"]});
        assert!(
            resolve(
                "python",
                vec![call(
                    "child_child_run",
                    "greet",
                    "cls",
                    "child.py",
                    json!({})
                )],
                opaque,
                edges.clone(),
            )
            .is_empty()
        );
    }
    // Without import evidence the first hop is refused.
    let (nodes, mut edges) = inheritance_corpus("base_base");
    edges.pop();
    assert!(
        resolve(
            "python",
            vec![call(
                "child_child_run",
                "greet",
                "self",
                "child.py",
                json!({})
            )],
            nodes,
            edges,
        )
        .is_empty()
    );
}

#[test]
fn python_property_reads_bind_only_to_a_getter_of_that_name() {
    isolate();
    let (nodes, edges) = inheritance_corpus("base_base");
    let read = |callee: &str| {
        call(
            "child_child_run",
            callee,
            "self",
            "child.py",
            json!({"_python_attr_read": true}),
        )
    };
    let added = resolve(
        "python",
        vec![read("items"), read("_items"), read("greet")],
        nodes,
        edges,
    );
    assert_eq!(
        found(&added),
        BTreeSet::from([quad(
            "child_child_run",
            "base_base_items",
            "references",
            "EXTRACTED"
        )])
    );
    assert_eq!(added[0]["context"], "property_read");
}

#[test]
fn typescript_picks_the_visible_definition_and_respects_private_twins() {
    isolate();
    let nodes = vec![
        n("ctx_a", "a.ts", "a.ts"),
        n("ctx_a_context", "Context", "a.ts"),
        n("ctx_a_context_get", ".get()", "a.ts"),
        n("ctx_b", "b.ts", "b.ts"),
        n("ctx_b_context", "Context", "b.ts"),
        n("ctx_b_context_get", ".#get()", "b.ts"),
        n("main", "main.ts", "main.ts"),
        n("main_run", "run()", "main.ts"),
    ];
    let edges = vec![
        e("ctx_a", "ctx_a_context", "contains", "a.ts"),
        e("ctx_a_context", "ctx_a_context_get", "method", "a.ts"),
        e("ctx_b", "ctx_b_context", "contains", "b.ts"),
        e("ctx_b_context", "ctx_b_context_get", "method", "b.ts"),
        e("main", "main_run", "contains", "main.ts"),
    ];
    let per = |file: &str| {
        vec![
            json!({"ts_type_table": {"path": file, "table": {"c": "Context"}},
            "raw_calls": [call("main_run", "get", "c", file, json!({}))]}),
        ]
    };
    let run = |edges: &Vec<Value>, nodes: &Vec<Value>| {
        let (mut nodes, mut edges) = (nodes.clone(), edges.clone());
        let before = edges.len();
        graphicle::extract_members::resolve_members(
            "typescript",
            &per("main.ts"),
            &mut nodes,
            &mut edges,
        );
        found(&edges[before..])
    };
    // Two same-named types, neither imported: nothing.
    assert!(run(&edges, &nodes).is_empty());
    let mut with_a = edges.clone();
    with_a.push(e("main", "ctx_a_context", "imports", "main.ts"));
    assert_eq!(
        run(&with_a, &nodes),
        BTreeSet::from([quad("main_run", "ctx_a_context_get", "calls", "INFERRED")])
    );
    // Through the module import of b.ts: a public call never reaches `#get`
    // unless the node is marked as shared with a public `get`.
    let mut with_b = edges.clone();
    with_b.push(e("main", "ctx_b", "imports_from", "main.ts"));
    assert!(run(&with_b, &nodes).is_empty());
    let mut twin = nodes.clone();
    twin[5]["_ts_public_twin"] = json!(true);
    assert_eq!(
        run(&with_b, &twin),
        BTreeSet::from([quad("main_run", "ctx_b_context_get", "calls", "INFERRED")])
    );
    // Both visible: ambiguous again.
    let mut both = with_a.clone();
    both.push(e("main", "ctx_b", "imports_from", "main.ts"));
    assert!(run(&both, &twin).is_empty());
}

#[test]
fn elixir_qualified_calls_bind_to_the_named_module() {
    isolate();
    let mut accounts = n("accounts", "App.Accounts", "accounts.ex");
    accounts["_elixir_module"] = json!(true);
    let mut web = n("web", "App.Web", "web.ex");
    web["_elixir_module"] = json!(true);
    let nodes = vec![
        accounts,
        n("accounts_get_user", "get_user()", "accounts.ex"),
        web,
        n("web_show", "show()", "web.ex"),
    ];
    let mut edges = vec![
        e("accounts", "accounts_get_user", "method", "accounts.ex"),
        e("web", "web_show", "method", "web.ex"),
    ];
    let raw = vec![
        json!({"caller_nid": "web_show", "callee": "get_user", "elixir_module": "App.Accounts",
            "source_file": "web.ex", "source_location": "L3"}),
        json!({"caller_nid": "web_show", "callee": "all", "elixir_module": "Repo",
            "source_file": "web.ex", "source_location": "L4"}),
    ];
    let per = vec![json!({"raw_calls": raw})];
    graphicle::extract_members::elixir_qualified_calls(&per, &nodes, &mut edges);
    assert_eq!(
        found(&edges[2..]),
        BTreeSet::from([quad("web_show", "accounts_get_user", "calls", "EXTRACTED")])
    );
}

#[test]
fn php_static_calls_bind_to_the_scope_class() {
    isolate();
    let nodes = vec![
        n("helper", "helper.php", "helper.php"),
        n("helper_helper", "Helper", "helper.php"),
        n("helper_helper_format", ".format()", "helper.php"),
        n("app", "app.php", "app.php"),
        n("app_child", "Child", "app.php"),
        n("app_child_run", ".run()", "app.php"),
        n("app_child_go", ".go()", "app.php"),
    ];
    let edges = vec![
        e("helper", "helper_helper", "contains", "helper.php"),
        e(
            "helper_helper",
            "helper_helper_format",
            "method",
            "helper.php",
        ),
        e("app", "app_child", "contains", "app.php"),
        e("app_child", "app_child_run", "method", "app.php"),
        e("app_child", "app_child_go", "method", "app.php"),
        e("app_child", "helper_helper", "inherits", "app.php"),
    ];
    // Scoped calls (`X::m()`) carry `php_scoped` since Graphify 0.9.89.
    let scoped = || json!({"php_scoped": true});
    let raw = vec![
        call(
            "app_child_run",
            "FORMAT",
            "\\App\\Helper",
            "app.php",
            scoped(),
        ),
        call("app_child_run", "go", "static", "app.php", scoped()),
        call("app_child_go", "format", "parent", "app.php", scoped()),
        call("app_child_go", "x", "$obj", "app.php", scoped()),
        call("app_child_go", "y", "Missing", "app.php", scoped()),
    ];
    let mut all_nodes = nodes.clone();
    let mut all_edges = edges.clone();
    let per = vec![json!({"raw_calls": raw})];
    graphicle::extract_members::resolve_members("php", &per, &mut all_nodes, &mut all_edges);
    assert_eq!(
        found(&all_edges[edges.len()..]),
        BTreeSet::from([
            quad(
                "app_child_run",
                "helper_helper_format",
                "calls",
                "EXTRACTED"
            ),
            quad("app_child_run", "app_child_go", "calls", "EXTRACTED"),
            quad("app_child_go", "helper_helper_format", "calls", "EXTRACTED"),
        ])
    );
    let go = all_nodes
        .iter()
        .find(|n| n["id"] == "app_child_go")
        .unwrap();
    // A parked call records its call-site file (Graphify 0.9.94, #4668).
    assert_eq!(
        go["metadata"]["unresolved_calls"],
        json!([{"callee": "y", "receiver_type": "Missing", "lang": "php", "line": "L9",
                "file": "app.php"}])
    );
}

/// `Service` holds `Repo Store` and `Repo[] Many`; `Repo.Save()` returns
/// `Result`; `Factory.Make<T>()` returns its type parameter.
fn csharp_corpus() -> (Vec<Value>, Vec<Value>, Value) {
    let mut nodes = vec![
        n("f", "a.cs", "a.cs"),
        n("service", "Service", "a.cs"),
        n("service_run", ".Run()", "a.cs"),
        n("repo", "Repo", "a.cs"),
        n("repo_save", ".Save()", "a.cs"),
        n("result", "Result", "a.cs"),
        n("result_ok", ".Ok()", "a.cs"),
        n("factory", "Factory", "a.cs"),
        n("factory_make", ".Make()", "a.cs"),
        n("base", "Base", "a.cs"),
        n("base_init", ".Init()", "a.cs"),
    ];
    for n in &mut nodes {
        n["metadata"] = json!({"namespace": ""});
    }
    let mut edges = vec![];
    for (owner, member) in [
        ("service", "service_run"),
        ("repo", "repo_save"),
        ("result", "result_ok"),
        ("factory", "factory_make"),
        ("base", "base_init"),
    ] {
        edges.push(e("f", owner, "contains", "a.cs"));
        edges.push(e(owner, member, "method", "a.cs"));
    }
    edges.push(e("service", "base", "inherits", "a.cs"));
    let tables = json!([
        {"lang": "csharp", "class_label": "Service", "source_file": "a.cs",
            "fields": {"Store": "Repo", "Many": "Repo", "Many[]": "Repo"}},
        {"lang": "csharp_returns", "class_label": "Repo", "source_file": "a.cs",
            "fields": {"Save": [["Result", 0]]}},
        {"lang": "csharp_returns", "class_label": "Factory", "source_file": "a.cs",
            "fields": {"Make": [["#0", 1]]}},
    ]);
    (nodes, edges, tables)
}

#[test]
fn csharp_member_chains_constructors_and_var_return_types() {
    isolate();
    let (mut nodes, mut edges, tables) = csharp_corpus();
    let before = edges.len();
    let cs = |caller: &str, callee: &str, receiver: &str, extra: Value| {
        let mut rc = call(caller, callee, receiver, "a.cs", extra);
        rc["lang"] = json!("csharp");
        rc
    };
    let raw = vec![
        // this.Store.Save() -> [type of this, "Store"]
        cs(
            "service_run",
            "Save",
            "",
            json!({"receiver_chain": ["Service", "Store"]}),
        ),
        // Many[i].Save(): an array-typed field is not its element.
        cs(
            "service_run",
            "Save",
            "",
            json!({"receiver_chain": ["Service", "Many"]}),
        ),
        // var r = Store.Save(); r.Ok()
        cs(
            "service_run",
            "Ok",
            "r",
            json!({"receiver_call": ["Repo", "Save", []]}),
        ),
        // var x = Factory.Make<Result>(); x.Ok()
        cs(
            "service_run",
            "Ok",
            "x",
            json!({"receiver_call": ["Factory", "Make", ["Result"]]}),
        ),
        // In a constructor body the caller is the type itself.
        cs("service", "Init", "base", json!({})),
    ];
    let per = vec![json!({"raw_calls": raw, "member_field_tables": tables})];
    graphicle::extract_members::resolve_members("csharp", &per, &mut nodes, &mut edges);
    assert_eq!(
        found(&edges[before..]),
        BTreeSet::from([
            quad("service_run", "repo_save", "calls", "INFERRED"),
            quad("service_run", "result_ok", "calls", "INFERRED"),
            quad("service", "base_init", "calls", "EXTRACTED"),
        ])
    );
}

#[test]
fn csharp_type_receiver_prefers_the_declared_type_of_a_same_named_member() {
    isolate();
    let (mut nodes, mut edges, _) = csharp_corpus();
    nodes.push(n("store", "Store", "a.cs"));
    nodes.push(n("store_save", ".Save()", "a.cs"));
    edges.push(e("f", "store", "contains", "a.cs"));
    edges.push(e("store", "store_save", "method", "a.cs"));
    let before = edges.len();
    let mut rc = call(
        "service_run",
        "Save",
        "Store",
        "a.cs",
        json!({"receiver_type": "Repo"}),
    );
    rc["lang"] = json!("csharp");
    let per = vec![json!({"raw_calls": [rc]})];
    graphicle::extract_members::resolve_members("csharp", &per, &mut nodes, &mut edges);
    assert_eq!(
        found(&edges[before..]),
        BTreeSet::from([quad("service_run", "repo_save", "calls", "EXTRACTED")])
    );
}

/// `Base` (base.java) declares `helper()`, `Widget field` and `Builder
/// builder()`; `Iface` declares a default `dflt()`; `Child extends Base
/// implements Iface` (child.java).
fn java_corpus() -> (Vec<Value>, Vec<Value>) {
    let mut builder_method = n("base_builder", ".builder()", "Base.java");
    builder_method["_java_ret"] = json!("Builder");
    let mut size = n("builder_size", ".size()", "Builder.java");
    size["_java_ret"] = json!("Builder");
    let nodes = vec![
        n("base_file", "Base.java", "Base.java"),
        n("base", "Base", "Base.java"),
        n("base_helper", ".helper()", "Base.java"),
        builder_method,
        n("iface_file", "Iface.java", "Iface.java"),
        n("iface", "Iface", "Iface.java"),
        n("iface_dflt", ".dflt()", "Iface.java"),
        n("widget_file", "Widget.java", "Widget.java"),
        n("widget", "Widget", "Widget.java"),
        n("widget_paint", ".paint()", "Widget.java"),
        n("builder_file", "Builder.java", "Builder.java"),
        n("builder", "Builder", "Builder.java"),
        size,
        n("builder_build", ".build()", "Builder.java"),
        n("child_file", "Child.java", "Child.java"),
        n("child", "Child", "Child.java"),
        n("child_run", ".run()", "Child.java"),
        n("base_ctor", ".Base()", "Base.java"),
    ];
    let mut edges = vec![];
    for (file, owner, members) in [
        (
            "base_file",
            "base",
            vec!["base_helper", "base_builder", "base_ctor"],
        ),
        ("iface_file", "iface", vec!["iface_dflt"]),
        ("widget_file", "widget", vec!["widget_paint"]),
        (
            "builder_file",
            "builder",
            vec!["builder_size", "builder_build"],
        ),
        ("child_file", "child", vec!["child_run"]),
    ] {
        edges.push(e(file, owner, "contains", "x.java"));
        for m in members {
            edges.push(e(owner, m, "method", "x.java"));
        }
    }
    edges.push(e("child", "base", "inherits", "Child.java"));
    edges.push(e("child", "iface", "implements", "Child.java"));
    (nodes, edges)
}

#[test]
fn java_inherited_bare_calls_fields_and_receiver_chains() {
    isolate();
    let (mut nodes, mut edges) = java_corpus();
    let before = edges.len();
    let j = |callee: &str, receiver: &str, member: bool, extra: Value| {
        let mut rc = call("child_run", callee, receiver, "Child.java", extra);
        rc["lang"] = json!("java");
        rc["is_member_call"] = json!(member);
        rc
    };
    let raw = vec![
        j("helper", "", false, json!({})),
        j("dflt", "", false, json!({})),
        // `new Base()` names a constructor, which is not inherited.
        j("Base", "", false, json!({})),
        j("paint", "widget", true, json!({"receiver_unbound": true})),
        j(
            "build",
            "",
            true,
            json!({"java_receiver_chain": ["call", "size", ["call", "builder", null]]}),
        ),
    ];
    let tables = json!([{"lang": "java", "class_label": "Base", "source_file": "Base.java",
        "fields": {"widget": "Widget"}}]);
    let per = vec![json!({"raw_calls": raw, "member_field_tables": tables})];
    graphicle::extract_members::resolve_members("java", &per, &mut nodes, &mut edges);
    assert_eq!(
        found(&edges[before..]),
        BTreeSet::from([
            quad("child_run", "base_helper", "calls", "EXTRACTED"),
            quad("child_run", "iface_dflt", "calls", "EXTRACTED"),
            quad("child_run", "widget_paint", "calls", "INFERRED"),
            quad("child_run", "builder_build", "calls", "INFERRED"),
        ])
    );
}

#[test]
fn java_superclass_wins_over_an_interface_default_and_ties_bind_nothing() {
    isolate();
    let (mut nodes, mut edges) = java_corpus();
    // Base and Iface both declare `shared()`: the superclass chain wins.
    nodes.push(n("base_shared", ".shared()", "Base.java"));
    nodes.push(n("iface_shared", ".shared()", "Iface.java"));
    edges.push(e("base", "base_shared", "method", "x.java"));
    edges.push(e("iface", "iface_shared", "method", "x.java"));
    // Two interfaces on one level both declaring `twice()`: a tie.
    nodes.push(n("other", "Other", "Other.java"));
    nodes.push(n("other_twice", ".twice()", "Other.java"));
    nodes.push(n("iface_twice", ".twice()", "Iface.java"));
    edges.push(e("other", "other_twice", "method", "x.java"));
    edges.push(e("iface", "iface_twice", "method", "x.java"));
    edges.push(e("child", "other", "implements", "Child.java"));
    let before = edges.len();
    let mut shared = call(
        "child_run",
        "shared",
        "",
        "Child.java",
        json!({"is_member_call": false}),
    );
    shared["lang"] = json!("java");
    let mut twice = call(
        "child_run",
        "twice",
        "",
        "Child.java",
        json!({"is_member_call": false}),
    );
    twice["lang"] = json!("java");
    let per = vec![json!({"raw_calls": [shared, twice]})];
    graphicle::extract_members::resolve_members("java", &per, &mut nodes, &mut edges);
    assert_eq!(
        found(&edges[before..]),
        BTreeSet::from([quad("child_run", "base_shared", "calls", "EXTRACTED")])
    );
}

// ---------------------------------------------------------------------------
// Corpus assembly passes (0.9.86 #4135, 0.9.87 #4363, 0.9.88 #4354)
// ---------------------------------------------------------------------------

fn include_edge(source: &str, raw: &str, file: &str) -> Value {
    let mut edge = e(source, &make_id([raw]), "imports", file);
    edge["metadata"] = json!({"raw_include": raw, "include_kind": "quoted"});
    edge["target_file"] = json!("ignored");
    edge
}

#[test]
fn quoted_includes_bind_to_a_unique_header_by_path_suffix() {
    isolate();
    let header = |id: &str, file: &str| {
        n(
            id,
            Path::new(file).file_name().unwrap().to_str().unwrap(),
            file,
        )
    };
    let nodes = [
        header("lib_util_h", "lib/util.h"),
        header("src_core_util_h", "src/core/util.h"),
        header("src_net_util_h", "src/net/util.h"),
        header("inc_config_h", "inc/Config.h"),
    ];
    let refs = nodes.iter().collect::<Vec<_>>();
    let mut edges = vec![
        include_edge("app_main_c", "lib/util.h", "app/main.c"),
        include_edge("src_core_a_c", "util.h", "src/core/a.c"),
        include_edge("top_c", "util.h", "top.c"),
        include_edge("app_main_c", "config.h", "app/main.c"),
        include_edge("app_main_c", "../lib/util.h", "app/main.c"),
        include_edge("app_main_c", "./util.h", "app/main.c"),
    ];
    graphicle::extract_merge::resolve_c_include_edges(&mut edges, &refs, None);
    let targets = edges
        .iter()
        .map(|e| s(e, "target").to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        [
            "lib_util_h",
            "src_core_util_h",
            make_id(["util.h"]).as_str(),
            "inc_config_h",
            make_id(["../lib/util.h"]).as_str(),
            make_id(["./util.h"]).as_str(),
        ]
    );
    assert!(edges[0].get("target_file").is_none());
    assert!(edges[2].get("target_file").is_some());
}

#[test]
fn shadowed_partial_base_placeholder_is_dropped_but_qualified_or_aliased_ones_stay() {
    isolate();
    let mut half = n("svc", "Svc", "a.cs");
    half["metadata"] = json!({"is_partial": true});
    let nodes = vec![
        half,
        n("ifoo", "IFoo", "i.cs"),
        json!({"id": "ifoo_stub", "label": "IFoo", "file_type": "code", "source_file": "", "source_location": ""}),
        n("b_cs", "b.cs", "b.cs"),
    ];
    let mut qualified = e("svc", "ifoo_stub", "implements", "b.cs");
    qualified["metadata"] = json!({"qualified": true});
    let mut alias = e("b_cs", "x", "imports", "c.cs");
    alias["metadata"] = json!({"using_kind": "alias", "alias": "IFoo"});
    let base = vec![
        e("svc", "ifoo", "implements", "a.cs"),
        e("svc", "ifoo_stub", "implements", "b.cs"),
    ];
    let mut edges = base.clone();
    graphicle::extract_merge::drop_shadowed_csharp_partial_bases(&nodes, &mut edges);
    assert_eq!(edges, base[..1]);
    let mut edges = vec![base[0].clone(), qualified.clone()];
    graphicle::extract_merge::drop_shadowed_csharp_partial_bases(&nodes, &mut edges);
    assert_eq!(edges.len(), 2);
    let mut aliased = base[1].clone();
    aliased["source_file"] = json!("b.cs");
    let mut edges = vec![base[0].clone(), aliased, alias];
    edges[2]["source_file"] = json!("b.cs");
    graphicle::extract_merge::drop_shadowed_csharp_partial_bases(&nodes, &mut edges);
    assert_eq!(edges.len(), 3);
}

#[test]
fn rust_use_groups_are_dropped_and_binding_edges_are_not_import_evidence() {
    isolate();
    let fx = Fixture::new();
    let a = fx.write("a.rs", "pub fn helper() -> u32 { 1 }\n");
    let b = fx.write("b.rs", "pub fn helper() -> u32 { 2 }\n");
    let c = fx.write(
        "c.rs",
        "use crate::a::helper;\npub fn run() -> u32 { helper() }\n",
    );
    let paths = vec![a.clone(), b.clone(), c.clone()];
    let mut per = paths
        .iter()
        .map(|p| graphicle::extract::extract_file(p, Some(&fx.root)))
        .collect::<Vec<_>>();
    // What the 0.9.86 extractor adds for `use crate::{a, b}`: a group edge and
    // one binding edge per name.
    let file_c = per[2]["nodes"][0]["id"].as_str().unwrap().to_string();
    let edges_c = per[2]["edges"].as_array_mut().unwrap();
    edges_c.retain(|e| !matches!(e["relation"].as_str(), Some("imports" | "imports_from")));
    let mut group = e(&file_c, "group_target", "imports_from", c.to_str().unwrap());
    group["_rust_use_group"] = json!(true);
    let mut binding = e(&file_c, "b", "imports_from", c.to_str().unwrap());
    binding["_rust_use_binding"] = json!(true);
    edges_c.extend([group, binding]);
    let r = graphicle::extract::assemble(&paths, per, &fx.root, &fx.options()).unwrap();
    let text = r.to_string();
    assert!(!text.contains("_rust_use_group") && !text.contains("_rust_use_binding"));
    assert!(!edges(&r).iter().any(|e| e["target"] == "group_target"));
    // The binding edge itself stays, under the file's portable id.
    assert!(pairs(&r, "imports_from").contains(&pair("c", "b")), "{r}");
    // The binding edge onto b.rs is no module-import evidence for the
    // ambiguous `helper()`.
    assert!(
        !edges(&r).iter().any(|e| e["relation"] == "calls"
            && e["target"] == "b_helper"
            && e["confidence"] == "EXTRACTED"),
        "{r}"
    );
}

// ---------------------------------------------------------------------------
// Language overrides (0.9.87 #2961)
// ---------------------------------------------------------------------------

#[test]
fn declared_python_suffix_resolves_modules_and_packages() {
    isolate();
    use graphicle::extractors::engine_config::probe_python_module_candidate;
    let fx = Fixture::new();
    let module = fx.write("pkg/helpers.pyx", "def f():\n    pass\n");
    let init = fx.write("pkg/__init__.pyx", "");
    assert_eq!(
        probe_python_module_candidate(&fx.root.join("pkg/helpers")),
        None
    );
    graphicle::rcfile::set_language_overrides(Some(&BTreeMap::from([(
        ".pyx".to_string(),
        ".py".to_string(),
    )])));
    assert_eq!(
        probe_python_module_candidate(&fx.root.join("pkg/helpers")),
        Some(module)
    );
    assert_eq!(
        probe_python_module_candidate(&fx.root.join("pkg")),
        Some(init)
    );
    assert!(
        graphicle::extractors::resolution_paths::is_python_package_dir(
            &fx.root.join("pkg"),
            &[".py"]
        )
    );
    // A native `.py` remapped away is no longer Python.
    let native = fx.write("other.py", "");
    graphicle::rcfile::set_language_overrides(Some(&BTreeMap::from([(
        ".py".to_string(),
        ".pas".to_string(),
    )])));
    assert_eq!(probe_python_module_candidate(&fx.root.join("other")), None);
    assert_eq!(probe_python_module_candidate(&native), Some(native.clone()));
    graphicle::rcfile::set_language_overrides(None);
}

#[test]
fn remapped_extension_dispatches_to_the_declared_extractor() {
    isolate();
    let fx = Fixture::new();
    let inc = fx.write("lib/util.inc", "<?php\nfunction helper() { return 1; }\n");
    fx.write(graphicle::rcfile::RC_FILENAME, "language.inc=php\n");
    let r = fx.extract(std::slice::from_ref(&inc));
    assert_eq!(
        graphicle::extract::get_extractor(&inc).as_deref(),
        Some("php")
    );
    assert!(nodes(&r).iter().any(|n| n["label"] == "helper()"), "{r}");
    std::fs::remove_file(fx.root.join(graphicle::rcfile::RC_FILENAME)).unwrap();
    graphicle::rcfile::activate_language_overrides(&fx.root);
    assert_ne!(
        graphicle::extract::get_extractor(&inc).as_deref(),
        Some("php")
    );
}

// ---------------------------------------------------------------------------
// AST cache (0.9.80 #4185, 0.9.83 #4236, 0.9.87 #2961)
// ---------------------------------------------------------------------------

#[test]
fn cache_entry_whose_import_target_vanished_is_a_miss() {
    isolate();
    use graphicle::cache::{CacheOptions, ExtractionCache};
    let fx = Fixture::new();
    let source = fx.write("a.py", "import b\n");
    let target = fx.write("b.py", "x = 1\n");
    let options = CacheOptions {
        root: fx.root.clone(),
        cache_root: Some(fx.root.join("out")),
        ..CacheOptions::default()
    };
    let record = json!({"nodes": [{"id": "a", "source_file": source}],
        "edges": [{"source": "a", "target": "b", "relation": "imports", "target_file": target},
                  {"source": "a", "target": "gone", "relation": "imports", "target_file": fx.root.join("gone.py")}]});
    let mut cache = ExtractionCache::default();
    cache.save(&source, &record, &options).unwrap();
    let hit = cache.load(&source, &options).unwrap().expect("a fresh hit");
    assert!(hit.get(graphicle::cache::CACHED_TARGETS_KEY).is_none());
    std::fs::remove_file(&target).unwrap();
    assert!(cache.load(&source, &options).unwrap().is_none());
}

#[test]
fn ast_cache_directory_carries_the_grammar_fingerprint() {
    isolate();
    use graphicle::cache::{ExtractionCache, grammar_fingerprint, grammar_fingerprint_of};
    let fx = Fixture::new();
    let mut cache = ExtractionCache::default();
    let dir = cache.cache_dir(&fx.root, "ast", None).unwrap();
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with(&format!("v{}-s{}-g", cache.extractor_version, cache.schema)));
    assert!(name.ends_with(grammar_fingerprint()));
    assert_eq!(grammar_fingerprint().len(), 12);
    let old = grammar_fingerprint_of([("tree-sitter-swift".into(), "0.7.2".into())]);
    let new = grammar_fingerprint_of([("tree-sitter-swift".into(), "0.7.4".into())]);
    assert_ne!(old, new);
    assert_eq!(
        grammar_fingerprint_of([("a".into(), "1".into()), ("b".into(), "2".into())]),
        grammar_fingerprint_of([("b".into(), "2".into()), ("a".into(), "1".into())])
    );
    assert_eq!(grammar_fingerprint_of([]), "none");
    // A different grammar set is a different directory, and the old one goes.
    let mut upgraded = ExtractionCache {
        grammar_fingerprint: Some(new),
        ..ExtractionCache::default()
    };
    let moved = upgraded.cache_dir(&fx.root, "ast", None).unwrap();
    assert_ne!(moved, dir);
    assert!(!dir.exists());
}

#[test]
fn salted_cache_keys_keep_remapped_files_apart() {
    isolate();
    use graphicle::cache::{CacheOptions, ExtractionCache, salted_key};
    assert_eq!(salted_key("abc", None), "abc");
    assert_eq!(salted_key("abc", Some("")), "abc");
    assert_ne!(salted_key("abc", Some("language=.php")), "abc");
    let fx = Fixture::new();
    let source = fx.write("util.inc", "x\n");
    let plain = CacheOptions {
        root: fx.root.clone(),
        ..CacheOptions::default()
    };
    let salted = CacheOptions {
        salt: Some("language=.php".into()),
        ..plain.clone()
    };
    let mut cache = ExtractionCache::default();
    cache
        .save(
            &source,
            &json!({"nodes": [{"id": "pascal"}], "edges": []}),
            &plain,
        )
        .unwrap();
    assert!(cache.load(&source, &plain).unwrap().is_some());
    assert!(cache.load(&source, &salted).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Source re-reads of the resolve passes (0.9.78): `_read_source_bytes` /
// `_read_source_text` with warn=False, so a file that is not UTF-8 reads as
// its extractor read it
// ---------------------------------------------------------------------------

#[test]
fn python_import_aliases_read_a_non_utf8_source_as_its_extractor_did() {
    let dir = tempfile::tempdir().unwrap();
    let cp1252 = dir.path().join("cp.py");
    std::fs::write(
        &cp1252,
        b"from mod import caf\xe9 as c\nfrom pkg.other import thing\n",
    )
    .unwrap();
    let aliases = graphicle::symbol_resolution::parse_python_import_aliases(&cp1252);
    let c = &aliases["c"];
    assert_eq!(
        (
            c.imported_name.as_str(),
            c.module_stem.as_str(),
            c.source_location.as_str()
        ),
        ("café", "mod", "L1")
    );
    assert_eq!(aliases["thing"].module_stem, "other");
    // A UTF-8 BOM stays text, which the reference's ast.parse rejects.
    let bom = dir.path().join("bom.py");
    std::fs::write(&bom, b"\xef\xbb\xbffrom mod import x as y\n").unwrap();
    assert!(graphicle::symbol_resolution::parse_python_import_aliases(&bom).is_empty());
}

#[test]
fn vue_script_is_masked_from_the_decoded_source() {
    let dir = tempfile::tempdir().unwrap();
    let vue = dir.path().join("a.vue");
    std::fs::write(
        &vue,
        b"<template><p>x</p></template>\n<script>\nexport const caf\xe9 = 1;\n</script>\n",
    )
    .unwrap();
    let tree = graphicle::extractors::resolution_facts::parse_js_tree(&vue).unwrap();
    let text = String::from_utf8(tree.source.clone()).unwrap();
    assert!(text.contains("export const café = 1;"), "{text}");
}

#[test]
fn a_cross_file_parse_reads_the_decoded_source() {
    let dir = tempfile::tempdir().unwrap();
    let py = dir.path().join("m.py");
    std::fs::write(&py, b"def caf\xe9():\n    return 1\n").unwrap();
    let tree = graphicle::extractors::resolution_facts::parse_python_tree(&py).unwrap();
    assert_eq!(
        String::from_utf8(tree.source.clone()).unwrap(),
        "def café():\n    return 1\n"
    );
    assert!(!tree.tree.root_node().has_error());
}

/// Watch rebuilds pass `resolution_context_nodes=context or None` (the
/// changed-files rebuild tests of 0.9.81-0.9.88 drive it): `null` is no
/// context, not an error.
#[test]
fn null_resolution_context_is_no_context() {
    let options: ExtractOptions = serde_json::from_value(json!({
        "resolution_context_nodes": null,
        "resolution_context_edges": null,
        "parallel": false,
    }))
    .unwrap();
    assert!(options.resolution_context_nodes.is_empty());
    assert!(options.resolution_context_edges.is_empty());
    assert!(!options.parallel);
    let options: ExtractOptions =
        serde_json::from_value(json!({"resolution_context_nodes": [{"id": "a"}]})).unwrap();
    assert_eq!(options.resolution_context_nodes.len(), 1);
}
