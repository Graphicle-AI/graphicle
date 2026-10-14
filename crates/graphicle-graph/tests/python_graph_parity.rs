//! Graph details the per-file oracle comparisons cannot see, because they sort
//! nodes and edges before comparing: the order in which an extractor emits
//! edges decides which attributes a repeated edge keeps in the graph (the
//! later one) and the order of graph.json. Expected values are Python
//! Graphify 0.9.76's graph.json for the same sources.
use serde_json::Value;
use std::{fs, path::Path};

fn extract(files: &[(&str, &str)]) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let root = graphicle::extractors::resolution::resolved(dir.path());
    for (name, text) in files {
        fs::write(root.join(name), text).unwrap();
    }
    let out = graphicle::cli::run(&[
        "extract".to_string(),
        root.to_string_lossy().into_owned(),
        "--code-only".to_string(),
    ]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
    serde_json::from_str(
        &fs::read_to_string(Path::new(&root).join(".graphicle/graph.json")).unwrap(),
    )
    .unwrap()
}

/// (source, target, relation, context) of every link, in graph.json order.
fn links(graph: &Value) -> Vec<(String, String, String, Option<String>)> {
    graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let text = |key: &str| l[key].as_str().unwrap().to_string();
            (
                text("source"),
                text("target"),
                text("relation"),
                l["context"].as_str().map(str::to_string),
            )
        })
        .collect()
}

fn link(
    source: &str,
    target: &str,
    relation: &str,
    context: Option<&str>,
) -> (String, String, String, Option<String>) {
    (
        source.into(),
        target.into(),
        relation.into(),
        context.map(str::to_string),
    )
}

#[test]
fn c_and_cpp_reference_the_return_type_before_the_parameters() {
    // A type that is both the return type and a parameter type: upstream
    // emits the return-type reference first, so the graph keeps the
    // parameter one.
    let c = extract(&[(
        "a.c",
        "typedef struct { int k; } jv;\n\njv binop_divide(jv a, jv b) {\n  return a;\n}\n",
    )]);
    assert_eq!(
        links(&c),
        [
            link("a_binop_divide", "jv", "references", Some("parameter_type")),
            link("a", "a_binop_divide", "contains", None),
        ]
    );
    let cpp = extract(&[(
        "a.cpp",
        "struct Item { int k; };\n\nItem run(Item x) {\n  return x;\n}\n\nclass Worker {\n public:\n  Item *make(const Item &seed) { return nullptr; }\n};\n",
    )]);
    assert_eq!(
        links(&cpp),
        [
            link("a_item", "a_item_k", "defines", Some("field")),
            link("a_run", "a_item", "references", Some("parameter_type")),
            link(
                "a_worker_make",
                "a_item",
                "references",
                Some("parameter_type")
            ),
            link("a", "a_item", "contains", None),
            link("a", "a_run", "contains", None),
            link("a", "a_worker", "contains", None),
            link("a_worker", "a_worker_make", "method", None),
        ]
    );
}

fn label(graph: &Value, id: &str) -> String {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == id)
        .and_then(|n| n["label"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn csharp_base_list_keeps_a_self_type_argument_and_using_metadata_order() {
    let graph = extract(&[(
        "a.cs",
        "using System.Collections.Generic;\nusing Convert = System.Convert;\n\nnamespace App;\n\ninternal class ObjectDetails : IComparer<ObjectDetails>\n{\n    public int Compare(ObjectDetails x, ObjectDetails y) => 0;\n}\n",
    )]);
    assert_eq!(
        links(&graph),
        [
            link(
                "a_app_objectdetails",
                "a_app_objectdetails",
                "references",
                Some("generic_arg")
            ),
            link("a", "system_convert", "imports", Some("import")),
            link("a", "system_collections_generic", "imports", Some("import")),
            link("a_app_objectdetails", "icomparer", "implements", None),
            link("a", "csharp_namespace:fc4a695f02a8a53a", "contains", None),
            link("a", "a_app_objectdetails", "contains", None),
            link(
                "a_app_objectdetails",
                "a_app_objectdetails_compare",
                "method",
                None
            ),
        ]
    );
    let alias = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["target"] == "system_convert")
        .unwrap();
    let keys: Vec<&str> = alias["metadata"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["using_kind", "alias", "target_fqn", "scope_kind"]);
}

#[test]
fn swift_member_calls_resolve_by_name_and_escaped_cases_keep_backticks() {
    // Upstream records a Swift call's receiver on the raw call only; unlike
    // other languages, a capitalized receiver does not defer the call.
    let swift = extract(&[(
        "a.swift",
        "import Foundation\n\nextension NotificationCenter {\n    func postNotification(named name: String) {}\n}\n\nfinal class Notifier {\n    func didResume() {\n        NotificationCenter.default.postNotification(named: \"x\")\n    }\n}\n\nenum ConfigurationType {\n    case `default`, ephemeral\n}\n",
    )]);
    assert_eq!(
        links(&swift)[0],
        link(
            "a_notifier_didresume",
            "a_notificationcenter_postnotification",
            "calls",
            Some("call")
        )
    );
    assert_eq!(label(&swift, "a_configurationtype_default"), "`default`");
    let kotlin = extract(&[(
        "a.kt",
        "package app\n\nclass Version(val major: Int) : Comparable<Version> {\n    override fun compareTo(other: Version): Int = major - other.major\n}\n\nenum class Mode { `default`, fast }\n",
    )]);
    assert_eq!(
        links(&kotlin)[0],
        link("a_version", "a_version", "references", Some("generic_arg"))
    );
    assert_eq!(label(&kotlin, "a_mode_default"), "`default`");
}
