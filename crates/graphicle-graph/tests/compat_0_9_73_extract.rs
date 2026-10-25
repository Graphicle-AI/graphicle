//! Mirrors the 0.9.73 additions to upstream extraction tests:
//! `tests/test_manifest_ingest.py` (#3910 virtual workspace roots),
//! `tests/test_multilang.py` / `tests/test_languages.py` (enum members as
//! `case_of` nodes for Rust, Zig, C++ and Scala 3),
//! `tests/test_cpp_nested_and_cli.py` (#3939 nested C++ enums),
//! `tests/test_java_member_calls.py` (inherited and `super` member calls),
//! `tests/test_solidity_extractor.py` (free functions),
//! `tests/test_vbnet_extractor.py` (qualified calls) and
//! `tests/test_astro_extraction.py` (frontmatter / script-only parsing).
use graphicle::api::{ExtractOptions, extract};
use graphicle::cache::{CacheOptions, ExtractionCache};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    fn extract(&self, paths: &[PathBuf]) -> Value {
        let options = ExtractOptions {
            root: Some(self.root.clone()),
            cache_root: Some(self.root.clone()),
            parallel: false,
            ..Default::default()
        };
        extract(paths, &options).unwrap()
    }
}

fn labels(result: &Value) -> BTreeSet<String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}

/// `_edge_labels`: `(source label, target label)` for every `relation` edge.
fn edge_labels(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    let by_id: HashMap<&str, &str> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n["label"].as_str()?)))
        .collect();
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == relation)
        .filter_map(|e| {
            Some((
                by_id.get(e["source"].as_str()?)?.to_string(),
                by_id.get(e["target"].as_str()?)?.to_string(),
            ))
        })
        .collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

fn load_cached(path: &Path, root: &Path) -> Option<Value> {
    ExtractionCache::default()
        .load(
            path,
            &CacheOptions {
                root: root.to_path_buf(),
                cache_root: Some(root.to_path_buf()),
                ..Default::default()
            },
        )
        .unwrap()
}

// test_manifest_ingest.py::test_cargo_virtual_workspace_manifest_emits_no_package
#[test]
fn cargo_virtual_workspace_manifest_is_skipped_and_cached() {
    let fx = Fixture::new();
    let p = fx.write("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n");
    let r = graphicle::manifest_ingest::extract_package_manifest(&p).unwrap();
    assert_eq!(r["nodes"], serde_json::json!([]));
    assert_eq!(r["skipped"], "virtual workspace root Cargo.toml");

    let result = fx.extract(std::slice::from_ref(&p));
    assert_eq!(result["failed_sources"], serde_json::json!([]));

    let cached = load_cached(&p, &fx.root).expect("skipped manifest result is cached");
    assert_eq!(cached["skipped"], "virtual workspace root Cargo.toml");
    assert_eq!(cached["nodes"], serde_json::json!([]));
}

// A package manifest that declares a real package is unaffected.
#[test]
fn cargo_package_manifest_is_not_skipped() {
    let fx = Fixture::new();
    let p = fx.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\"]\n[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
    );
    let r = graphicle::manifest_ingest::extract_package_manifest(&p).unwrap();
    assert!(r.get("skipped").is_none());
    assert!(labels(&r).contains("root"));
}

fn ids_to_labels(result: &Value) -> HashMap<String, String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?.into(), n["label"].as_str()?.into())))
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// test_cpp_nested_and_cli.py::test_cpp_enum_specifier_members_are_extracted
#[test]
fn cpp_enum_specifier_members_are_extracted() {
    let fx = Fixture::new();
    let p = fx.write(
        "colors.hpp",
        "enum class Color { Red, Green = 2, Blue };\n\
         enum Old { X, Y };\n\
         typedef enum { A, B } Flag;\n",
    );
    let r = graphicle::extract_languages::extract_language(&p, "cpp");
    assert!(r.get("parse_errors").is_none_or(Value::is_null));
    let labels = labels(&r);
    assert!(set(&["Color", "Red", "Green", "Blue", "Old", "X", "Y"]).is_subset(&labels));
    let case_of = edge_labels(&r, "case_of");
    for (a, b) in [
        ("Color", "Red"),
        ("Color", "Green"),
        ("Color", "Blue"),
        ("Old", "X"),
        ("Old", "Y"),
    ] {
        assert!(case_of.contains(&pair(a, b)), "missing case_of {a} -> {b}");
    }
    assert!(!labels.contains("A"));
    // Graphify 0.9.95 (#4580): the typedef name `Flag` is a type alias node of
    // its own, contained by the file, with no members.
    assert!(!labels.contains(""));
    assert!(edge_labels(&r, "contains").contains(&pair("colors.hpp", "Flag")));
    assert!(!case_of.iter().any(|(source, _)| source == "Flag"));
}

// test_cpp_nested_and_cli.py::test_cpp_enum_nested_in_class_and_namespace_is_extracted
#[test]
fn cpp_enum_nested_in_class_and_namespace_is_extracted() {
    let fx = Fixture::new();
    let p = fx.write(
        "nested_enum.hpp",
        "class Widget {\n\
         public:\n    enum class State { Idle, Active };\n\
         };\n\
         namespace net {\n    enum Proto { Tcp, Udp };\n\
         }\n",
    );
    let r = graphicle::extract_languages::extract_language(&p, "cpp");
    assert!(r.get("parse_errors").is_none_or(Value::is_null));
    let labels = labels(&r);
    assert!(set(&["Widget", "State", "Idle", "Active", "Proto", "Tcp", "Udp"]).is_subset(&labels));
    assert!(edge_labels(&r, "contains").contains(&pair("Widget", "State")));
    let case_of = edge_labels(&r, "case_of");
    for (a, b) in [
        ("State", "Idle"),
        ("State", "Active"),
        ("Proto", "Tcp"),
        ("Proto", "Udp"),
    ] {
        assert!(case_of.contains(&pair(a, b)), "missing case_of {a} -> {b}");
    }
}

// test_languages.py::test_scala_enum_definition_cases_and_methods
#[test]
fn scala_enum_definition_cases_and_methods() {
    let fx = Fixture::new();
    let p = fx.write(
        "color.scala",
        "enum Color { case Red, Green, Blue }\n\
         enum Op[A] {\n  case Add(x: Int, y: Int)\n  case Noop\n  def run: A = ???\n}\n",
    );
    let r = graphicle::extract_languages::extract_language(&p, "scala");
    let contains = edge_labels(&r, "contains");
    assert!(contains.contains(&pair("color.scala", "Color")));
    assert!(contains.contains(&pair("color.scala", "Op")));
    let case_of = edge_labels(&r, "case_of");
    for (a, b) in [
        ("Color", "Red"),
        ("Color", "Green"),
        ("Color", "Blue"),
        ("Op", "Add"),
        ("Op", "Noop"),
    ] {
        assert!(case_of.contains(&pair(a, b)), "missing case_of {a} -> {b}");
    }
    assert!(
        labels(&r)
            .iter()
            .any(|l| l.trim_start_matches('.').trim_end_matches("()") == "run")
    );
}

/// `_calls`: extract the files together and collect `calls` edges by id.
fn java_calls(files: &[(&str, &str)]) -> (BTreeSet<(String, String)>, Value) {
    let fx = Fixture::new();
    let paths = files
        .iter()
        .map(|(rel, text)| fx.write(rel, text))
        .collect::<Vec<_>>();
    let result = fx.extract(&paths);
    let calls = result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_string(),
                e["target"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    (calls, result)
}

/// `_find`: the id of the node with `label` whose id contains `id_contains`.
fn find(result: &Value, label: &str, id_contains: &str) -> String {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == label && n["id"].as_str().unwrap().contains(id_contains))
        .and_then(|n| n["id"].as_str())
        .unwrap_or_else(|| panic!("no node {label} ~ {id_contains}"))
        .to_string()
}

// test_java_member_calls.py::test_inherited_method_resolves_for_this_field_and_parameter_receivers
#[test]
fn java_inherited_method_resolves_for_this_field_and_parameter_receivers() {
    let (calls, r) = java_calls(&[
        ("Base.java", "class Base { void start() {} }\n"),
        (
            "Worker.java",
            "class Worker extends Base {\n    void viaThis() { this.start(); }\n}\n",
        ),
        (
            "Service.java",
            "class Service {\n    Worker worker;\n    void viaField() { worker.start(); }\n    \
             void viaParam(Worker w) { w.start(); }\n}\n",
        ),
    ]);
    let start = find(&r, ".start()", "base_start");
    for (label, id) in [
        (".viaThis()", "worker_viathis"),
        (".viaField()", "service_viafield"),
        (".viaParam()", "service_viaparam"),
    ] {
        let caller = find(&r, label, id);
        assert!(calls.contains(&(caller, start.clone())), "{label} -> start");
    }
}

// test_java_member_calls.py::test_nearest_declaration_on_the_superclass_chain_wins
#[test]
fn java_nearest_declaration_on_the_superclass_chain_wins() {
    let (calls, r) = java_calls(&[(
        "Chain.java",
        "class Root { void start() {} void stop() {} }\n\
         class Middle extends Root { void stop() {} }\n\
         class Leaf extends Middle {}\n\
         class Client { void run(Leaf leaf) { leaf.start(); leaf.stop(); } }\n",
    )]);
    let run = find(&r, ".run()", "client_run");
    assert!(calls.contains(&(run.clone(), find(&r, ".start()", "root_start"))));
    assert!(calls.contains(&(run.clone(), find(&r, ".stop()", "middle_stop"))));
    assert!(!calls.contains(&(run, find(&r, ".stop()", "root_stop"))));
}

// test_java_member_calls.py::test_super_call_resolves_to_the_superclass_declaration
#[test]
fn java_super_call_resolves_to_the_superclass_declaration() {
    let (calls, r) = java_calls(&[
        ("Base.java", "class Base { void close() {} }\n"),
        (
            "Worker.java",
            "class Worker extends Base {\n    void close() { super.close(); }\n}\n",
        ),
    ]);
    let worker_close = find(&r, ".close()", "worker_close");
    let base_close = find(&r, ".close()", "base_close");
    assert!(calls.contains(&(worker_close.clone(), base_close.clone())));
    assert!(r["edges"].as_array().unwrap().iter().any(|e| {
        e["relation"] == "calls"
            && e["source"] == worker_close.as_str()
            && e["target"] == base_close.as_str()
            && e["confidence"] == "EXTRACTED"
    }));
}

// test_java_member_calls.py::test_ancestor_outside_the_corpus_leaves_inherited_call_unresolved
#[test]
fn java_ancestor_outside_the_corpus_leaves_inherited_call_unresolved() {
    let (calls, r) = java_calls(&[
        ("Named.java", "interface Named { String name(); }\n"),
        (
            "Entity.java",
            "import org.example.Identified;\ninterface Entity extends Identified, Named {}\n",
        ),
        (
            "Client.java",
            "class Client { void run(Entity e) { e.name(); } }\n",
        ),
    ]);
    let run = find(&r, ".run()", "client_run");
    assert!(!calls.iter().any(|(source, _)| *source == run));
}

// test_java_member_calls.py::test_super_call_is_not_bound_back_to_the_calling_class
#[test]
fn java_super_call_is_not_bound_back_to_the_calling_class() {
    let (calls, r) = java_calls(&[
        (
            "lib/Handler.java",
            "package lib;\npublic class Handler { public void handle() {} }\n",
        ),
        (
            "app/Handler.java",
            "package app;\npublic class Handler extends lib.Handler {\n    \
             public void handle() {}\n    public void retry() { super.handle(); }\n}\n",
        ),
    ]);
    let retry = find(&r, ".retry()", "handler_retry");
    assert!(!calls.iter().any(|(source, _)| *source == retry));
}

// test_java_member_calls.py::test_inherited_call_is_not_bound_to_another_language
#[test]
fn java_inherited_call_is_not_bound_to_another_language() {
    let (calls, r) = java_calls(&[
        (
            "Worker.java",
            "import org.lib.*;\n\
             class Worker extends Base { void run() { this.start(); super.start(); } }\n",
        ),
        (
            "tools/base.py",
            "class Base:\n    def start(self):\n        pass\n",
        ),
    ]);
    let run = find(&r, ".run()", "worker_run");
    assert!(!calls.iter().any(|(source, _)| *source == run));
}

/// `_labels` in `test_astro_extraction.py`: label -> source_location for every
/// node except the file node (the first node).
fn astro_labels(result: &Value) -> HashMap<String, String> {
    let nodes = result["nodes"].as_array().unwrap();
    let file_id = nodes
        .first()
        .map(|n| n["id"].clone())
        .unwrap_or(Value::Null);
    nodes
        .iter()
        .filter(|n| n["id"] != file_id)
        .map(|n| {
            let text = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_string);
            (text(&n["label"]), text(&n["source_location"]))
        })
        .collect()
}

#[test]
fn astro_template_is_not_a_parse_error() {
    let fx = Fixture::new();
    let page = fx.write(
        "src/pages/results.astro",
        "---
import Layout from '../layouts/Layout.astro';
interface Props { year: number }
const { year } = Astro.props;
function rank(scores: number[]): number[] {
  return [...scores].sort((a, b) => b - a);
}
---

<Layout title={`Results ${year}`}>
  <ol>{rank([3, 1, 2]).map((s) => <li class=\"score\">{s}</li>)}</ol>
</Layout>
<script type=\"application/ld+json\">{ \"@type\": \"Event\", \"name\": \"x\" }</script>
<script>
  function toggle(el: HTMLElement) { el.classList.toggle('open'); }
  document.querySelectorAll('ol').forEach((el) => toggle(el));
</script>
",
    );
    let result = graphicle::extract_languages::extract_language(&page, "astro");
    assert!(result.get("parse_errors").is_none(), "{result}");
    let labels = astro_labels(&result);
    // Masking keeps offsets, so locations still point at the original lines.
    assert_eq!(
        labels.get("rank()").map(String::as_str),
        Some("L5"),
        "{labels:?}"
    );
    assert_eq!(
        labels.get("toggle()").map(String::as_str),
        Some("L15"),
        "{labels:?}"
    );
}

#[test]
fn astro_scripts_on_one_line_do_not_merge() {
    let fx = Fixture::new();
    let page = fx.write(
        "src/pages/inline.astro",
        "<script>const a = 1</script><script>function b() {}</script>\n",
    );
    let result = graphicle::extract_languages::extract_language(&page, "astro");
    assert!(result.get("parse_errors").is_none(), "{result}");
    assert!(astro_labels(&result).contains_key("b()"), "{result}");
}

/// Pinned 0.9.73 strings, with the explicitly reviewed quoted-type correction.
/// Since 0.9.77 (674338b, #4072) each script body ends with U+2028 before its
/// `;`, so a trailing `//` comment cannot run into the next script.
#[test]
fn astro_mask_preserves_locations_with_reviewed_script_type_fix() {
    use graphicle::extract_languages::astro_mask_non_script as mask;
    let sp = |n: usize| " ".repeat(n);
    assert_eq!(
        mask("---\nconst a = 1;\n---\n<p>x</p>\n"),
        format!("{}\nconst a = 1;\n{}\n{}\n", sp(3), sp(3), sp(8))
    );
    assert_eq!(
        mask("<script>a()</script><script>b()</script>"),
        format!("{}a()\u{2028};{}b()\u{2028};{}", sp(8), sp(13), sp(5))
    );
    assert_eq!(
        mask("<SCRIPT>x</SCRIPT>\r\n"),
        format!("{}x\u{2028};{}\r\n", sp(8), sp(5))
    );
    // Upstream's optional quote backtracks and blanks this executable body.
    // The adaptation preserves it, like the equivalent unquoted attribute.
    assert_eq!(
        mask("<script type=\"module\">m()</script>"),
        format!("{}m()\u{2028};{}", sp(22), sp(5))
    );
    assert_eq!(
        mask("<script type=module>m()</script>"),
        format!("{}m()\u{2028};{}", sp(20), sp(5))
    );
    assert_eq!(mask("<script type=module_x>m()</script>"), sp(34));
    assert_eq!(
        mask("<script lang=\"ts\">m()</script>"),
        format!("{}m()\u{2028};{}", sp(18), sp(5))
    );
    // A script inside the frontmatter is not a second region.
    assert_eq!(
        mask("---\n<script>s()</script>\n---\n"),
        format!("{}\n<script>s()</script>\n{}\n", sp(3), sp(3))
    );
    // Empty frontmatter and empty script bodies; non-ASCII becomes one space.
    assert_eq!(mask("---\n\n---\n"), format!("{}\n\n{}\n", sp(3), sp(3)));
    assert_eq!(
        mask("<script></script>\u{e9}"),
        format!("{}\u{2028};{}", sp(8), sp(6))
    );
}

// test_multilang.py::test_rust_enum_variants_emit_case_of_nodes (the enum of
// upstream's tests/fixtures/sample.rs).
#[test]
fn rust_enum_variants_emit_case_of_nodes() {
    let fx = Fixture::new();
    let p = fx.write(
        "sample.rs",
        "pub struct Graph;\n\
         pub struct DataProcessor;\n\
         enum GraphEvent {\n    NodeAdded(Graph),\n    Processed { proc: DataProcessor },\n}\n",
    );
    let r = graphicle::extractors::rust::extract_rust(&p);
    let labels = labels(&r);
    assert!(labels.contains("NodeAdded"));
    assert!(labels.contains("Processed"));
    let case_of = edge_labels(&r, "case_of");
    assert!(case_of.contains(&pair("GraphEvent", "NodeAdded")));
    assert!(case_of.contains(&pair("GraphEvent", "Processed")));
    // The variant payload references are still emitted from the enum.
    let refs = edge_labels(&r, "references");
    assert!(refs.contains(&pair("GraphEvent", "Graph")));
    assert!(refs.contains(&pair("GraphEvent", "DataProcessor")));
}

// test_languages.py::test_zig_enum_members_emit_case_of_nodes
#[test]
fn zig_enum_members_emit_case_of_nodes() {
    let fx = Fixture::new();
    let p = fx.write(
        "colors.zig",
        "const Color = enum { red, green, blue };\n\
         const Dir = enum(u8) { north = 0, south };\n\
         const Point = struct { x: i32, y: i32 };\n",
    );
    let r = graphicle::extractors::zig::extract_zig(&p);
    assert!(r.get("error").is_none());
    let labels = labels(&r);
    assert!(set(&["red", "green", "blue", "north", "south"]).is_subset(&labels));
    let case_of = edge_labels(&r, "case_of");
    for (a, b) in [
        ("Color", "red"),
        ("Color", "green"),
        ("Color", "blue"),
        ("Dir", "north"),
        ("Dir", "south"),
    ] {
        assert!(case_of.contains(&pair(a, b)), "missing case_of {a} -> {b}");
    }
    // Struct fields share the container_field shape but are not enum members.
    assert!(!labels.contains("x"));
    assert!(!case_of.iter().any(|(source, _)| source == "Point"));
}

// test_solidity_extractor.py::test_solidity_free_functions_and_their_calls_are_extracted
#[test]
fn solidity_free_functions_and_their_calls_are_extracted() {
    let fx = Fixture::new();
    let p = fx.write(
        "Utils.sol",
        "pragma solidity ^0.8.0;\n\
         function halve(uint x) pure returns (uint) { return x / 2; }\n\
         function quarter(uint x) pure returns (uint) { return halve(halve(x)); }\n",
    );
    let result = fx.extract(std::slice::from_ref(&p));
    assert!(set(&["halve()", "quarter()"]).is_subset(&labels(&result)));
    assert!(edge_labels(&result, "calls").contains(&pair("quarter()", "halve()")));
}

// Free functions are contained by the file, callable, and member calls from
// them (`Lib.f()`) are left as raw calls for cross-file resolution.
#[test]
fn solidity_free_function_shape_and_member_calls() {
    let fx = Fixture::new();
    let p = fx.write(
        "Free.sol",
        "pragma solidity ^0.8.0;\n\
         function a(uint x, address y) pure returns (uint) { return Lib.f(x); }\n\
         function b() pure {}\n\
         function b(uint x) pure {}\n\
         function c() pure { b(); b(1); missing(); }\n",
    );
    let r = graphicle::extractors::solidity::extract_solidity(&p);
    let ids = ids_to_labels(&r);
    let a = r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == "a()")
        .unwrap();
    assert_eq!(a["_callable"], true);
    assert_eq!(a["metadata"]["kind"], "function");
    assert_eq!(a["source_location"], "L2");
    assert!(edge_labels(&r, "contains").contains(&pair("Free.sol", "a()")));
    let raw = r["raw_calls"].as_array().unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0]["callee"], "f");
    assert_eq!(ids[raw[0]["caller_nid"].as_str().unwrap()], "a()");
    assert_eq!(raw[0]["is_member_call"], true);
    // Overloads are told apart by arity, so both `b` calls resolve.
    let calls = r["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls")
        .count();
    assert_eq!(calls, 2);
}

// test_vbnet_extractor.py::test_vbnet_module_qualified_call_resolves_to_the_module_method
#[test]
fn vbnet_module_qualified_call_resolves_to_the_module_method() {
    let fx = Fixture::new();
    let p = fx.write(
        "App.vb",
        "Namespace Demo\n\
         \x20Public Module Helpers\n\
         \x20 Public Sub Log()\n\
         \x20 End Sub\n\
         \x20End Module\n\
         \x20Public Class Service\n\
         \x20 Public Sub Run()\n\
         \x20  Helpers.Log()\n\
         \x20 End Sub\n\
         \x20End Class\n\
         End Namespace\n",
    );
    let result = fx.extract(std::slice::from_ref(&p));
    assert!(edge_labels(&result, "calls").contains(&pair("Run()", "Log()")));
}

// test_vbnet_extractor.py::test_vbnet_call_through_a_value_receiver_stays_unresolved
#[test]
fn vbnet_call_through_a_value_receiver_stays_unresolved() {
    let fx = Fixture::new();
    let p = fx.write(
        "App.vb",
        "Public Class Other\n\
         \x20Public Sub Save()\n\
         \x20End Sub\n\
         End Class\n\
         Public Class Service\n\
         \x20Public Sub Run()\n\
         \x20 obj.Save()\n\
         \x20End Sub\n\
         End Class\n",
    );
    let result = fx.extract(std::slice::from_ref(&p));
    assert!(!edge_labels(&result, "calls").contains(&pair("Run()", "Save()")));
}
