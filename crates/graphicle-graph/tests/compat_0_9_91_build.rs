// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_detect_jinja.py, tests/test_csharp_interface_dispatch.py,
// tests/test_swift_protocol_dispatch.py, tests/test_labeling.py,
// tests/test_merge_sweeps_reextract_orphans.py, tests/test_dedup.py,
// tests/test_watch.py, tests/test_chunking.py
//! Build, dedup, detect, cluster, watch and dispatch changes ported from
//! Graphify 0.9.89-0.9.91 (port part b). Cases follow the upstream tests
//! named in each comment.
use graphicle::build::{Diagnostics, MergeOptions, build_merge};
use graphicle::cluster::label_still_applies;
use graphicle::dedup::{DedupOptions, deduplicate_entities};
use graphicle::interface_dispatch::{dispatch_confidence, resolve_interface_dispatch};
use graphicle::watch::{self, RebuildOptions};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[test]
fn jinja_templates_are_documents_and_split_as_text() {
    // test_detect_jinja.py (#3427): .j2, .jinja and .jinja2 are documents.
    for name in ["page.j2", "base.jinja", "mail.jinja2"] {
        assert_eq!(
            graphicle::detect::classify_file(Path::new(name)),
            Some(graphicle::detect::FileType::Document),
            "{name}"
        );
        let suffix = &name[name.find('.').unwrap()..];
        assert!(graphicle::file_slice::SPLITTABLE_TEXT_SUFFIXES.contains(&suffix));
    }
}

fn dispatch_graph(implementers: &[(&str, &str)]) -> (Vec<Value>, Vec<Value>) {
    let mut nodes = vec![
        json!({"id": "ireport", "label": "IReport", "source_file": "IReport.cs"}),
        json!({"id": "ireport_build", "label": ".Build()", "source_file": "IReport.cs"}),
    ];
    let mut edges =
        vec![json!({"source": "ireport", "target": "ireport_build", "relation": "method"})];
    for (id, file) in implementers {
        nodes.push(json!({"id": id, "label": id, "source_file": file}));
        let method = format!("{id}_build");
        nodes.push(json!({"id": method, "label": ".Build()", "source_file": file}));
        edges.push(json!({"source": id, "target": "ireport", "relation": "implements"}));
        edges.push(json!({"source": id, "target": method, "relation": "method"}));
    }
    (nodes, edges)
}

fn dispatched(edges: &[Value]) -> BTreeMap<String, f64> {
    edges
        .iter()
        .filter(|e| e["relation"] == "dispatches_to")
        .map(|e| {
            assert_eq!(e["source"], "ireport_build");
            (
                e["target"].as_str().unwrap().to_owned(),
                e["confidence_score"].as_f64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn interface_members_fan_out_to_every_implementer_with_tiered_confidence() {
    // test_csharp_interface_dispatch.py (#3800): every implementer gets an
    // edge; 0.85 for one, 0.75 for two, a 0.65 floor from three on.
    assert_eq!(dispatch_confidence(1), 0.85);
    assert_eq!(dispatch_confidence(2), 0.75);
    assert_eq!(dispatch_confidence(3), 0.65);
    assert_eq!(dispatch_confidence(7), 0.65);
    for (count, score) in [(1, 0.85), (2, 0.75), (3, 0.65)] {
        let impls: Vec<(String, &str)> = (0..count)
            .map(|i| (format!("report{i}"), "Reports.cs"))
            .collect();
        let refs: Vec<(&str, &str)> = impls.iter().map(|(a, b)| (a.as_str(), *b)).collect();
        let (nodes, mut edges) = dispatch_graph(&refs);
        resolve_interface_dispatch(&[], &nodes, &mut edges, &[".cs"]).unwrap();
        let got = dispatched(&edges);
        assert_eq!(got.len(), count, "{got:?}");
        assert!(got.values().all(|s| *s == score), "{got:?}");
    }
}

#[test]
fn gate_failing_implementers_do_not_lower_confidence() {
    // #4396: a sourceless stub and a Java class implementing the same name
    // never get an edge, so the one C# implementer keeps 0.85.
    let (mut nodes, mut edges) =
        dispatch_graph(&[("report", "Report.cs"), ("legacy", "Legacy.java")]);
    nodes.push(json!({"id": "stub", "label": "Stub", "source_file": ""}));
    edges.push(json!({"source": "stub", "target": "ireport", "relation": "implements"}));
    resolve_interface_dispatch(&[], &nodes, &mut edges, &[".cs"]).unwrap();
    assert_eq!(
        dispatched(&edges),
        BTreeMap::from([("report_build".to_owned(), 0.85)])
    );
}

#[test]
fn an_overloading_implementer_is_skipped_alone() {
    // Ambiguity is per implementer: the clean sibling still gets its edge.
    let (mut nodes, mut edges) = dispatch_graph(&[("a", "A.cs"), ("b", "B.cs")]);
    nodes.push(json!({"id": "a_build2", "label": ".Build()", "source_file": "A.cs"}));
    edges.push(json!({"source": "a", "target": "a_build2", "relation": "method"}));
    resolve_interface_dispatch(&[], &nodes, &mut edges, &[".cs"]).unwrap();
    assert_eq!(
        dispatched(&edges),
        BTreeMap::from([("b_build".to_owned(), 0.75)])
    );
}

#[test]
fn swift_two_conformers_both_dispatch() {
    // test_swift_protocol_dispatch.py::test_two_conformers_both_get_an_edge_at_lower_confidence.
    let nodes = vec![
        json!({"id": "s_shape", "label": "Shape", "source_file": "S.swift"}),
        json!({"id": "s_shape_area", "label": ".area()", "source_file": "S.swift"}),
        json!({"id": "s_square", "label": "Square", "source_file": "S.swift"}),
        json!({"id": "s_square_area", "label": ".area()", "source_file": "S.swift"}),
        json!({"id": "s_circle", "label": "Circle", "source_file": "S.swift"}),
        json!({"id": "s_circle_area", "label": ".area()", "source_file": "S.swift"}),
    ];
    let mut edges = vec![
        json!({"source": "s_shape", "target": "s_shape_area", "relation": "method"}),
        json!({"source": "s_square", "target": "s_shape", "relation": "implements"}),
        json!({"source": "s_square", "target": "s_square_area", "relation": "method"}),
        json!({"source": "s_circle", "target": "s_shape", "relation": "implements"}),
        json!({"source": "s_circle", "target": "s_circle_area", "relation": "method"}),
    ];
    graphicle::swift_dispatch::resolve_swift_protocol_dispatch(&[], &nodes, &mut edges).unwrap();
    let got: BTreeSet<_> = edges
        .iter()
        .filter(|e| e["relation"] == "dispatches_to")
        .map(|e| {
            assert_eq!(e["confidence_score"], 0.75);
            (
                e["source"].as_str().unwrap().to_owned(),
                e["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("s_shape_area", "s_circle_area"),
            ("s_shape_area", "s_square_area")
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .into()
    );
}

fn previous(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
    pairs.iter().map(|(n, c)| (n.to_string(), *c)).collect()
}

fn members(ids: &[&str]) -> Vec<Value> {
    ids.iter().map(|s| json!(s)).collect()
}

#[test]
fn a_label_needs_a_strict_majority_of_both_member_sets() {
    // test_labeling.py (#2298).
    let prev = previous(&[("a", 0), ("b", 0), ("c", 0), ("d", 0), ("x", 1)]);
    // Equal signatures keep the label whatever the overlap.
    assert!(label_still_applies(
        0,
        &members(&["z"]),
        &prev,
        Some("s"),
        Some("s")
    ));
    // 3 of 4 saved and 3 of 4 current: kept.
    assert!(label_still_applies(
        0,
        &members(&["a", "b", "c", "e"]),
        &prev,
        Some("s"),
        Some("t")
    ));
    // A one-node fragment of a large community keeps the id but not the name.
    assert!(!label_still_applies(
        0,
        &members(&["a"]),
        &prev,
        Some("s"),
        Some("t")
    ));
    // Half is not a strict majority.
    assert!(!label_still_applies(
        0,
        &members(&["a", "b", "e", "f"]),
        &prev,
        None,
        None
    ));
    // No saved members, or no current members.
    assert!(!label_still_applies(5, &members(&["a"]), &prev, None, None));
    assert!(!label_still_applies(0, &[], &prev, None, None));
    // Only the saved signature: the overlap decides.
    assert!(label_still_applies(
        1,
        &members(&["x"]),
        &prev,
        Some("s"),
        None
    ));
}

fn write_graph(path: &Path, nodes: Value, links: Value, hyperedges: Value) {
    fs::write(
        path,
        json!({"directed": false, "multigraph": false,
               "graph": {"hyperedges": hyperedges}, "nodes": nodes, "links": links})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn merge_sweeps_placeholders_stranded_by_reextraction() {
    // test_merge_sweeps_reextract_orphans.py (#4399): a type stub whose last
    // reference was edited away goes; an isolate already in the stored graph,
    // a hyperedge member and a node the fresh chunk emits without an edge stay.
    let dir = tempfile::tempdir().unwrap();
    let graph_path = dir.path().join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id": "a_run", "label": "run()", "file_type": "code", "source_file": "a.py", "_origin": "ast"},
            {"id": "helper", "label": "Helper", "file_type": "code", "source_file": ""},
            {"id": "old_isolate", "label": "Old", "file_type": "code", "source_file": ""},
            {"id": "member", "label": "Member", "file_type": "code", "source_file": ""},
        ]),
        json!([
            {"source": "a_run", "target": "helper", "relation": "references", "confidence": "EXTRACTED",
             "source_file": "a.py", "_origin": "ast"},
        ]),
        json!([{"id": "he", "label": "group", "nodes": ["member", "a_run"], "source_file": "notes.md"}]),
    );
    let chunk = json!({
        "nodes": [
            {"id": "a_run", "label": "run()", "file_type": "code", "source_file": "a.py"},
            {"id": "fresh_isolate", "label": "Fresh", "file_type": "code", "source_file": ""},
        ],
        "edges": [],
        "extracted_sources": ["a.py"],
    });
    let mut d = Diagnostics::default();
    let graph = build_merge(
        &[chunk],
        &MergeOptions {
            graph_path: Some(graph_path.clone()),
            root: Some(dir.path().to_path_buf()),
            ..Default::default()
        },
        &mut d,
        None,
    )
    .unwrap();
    let ids: BTreeSet<String> = graph
        .nodes()
        .iter()
        .filter_map(|n| n.id.as_str().map(str::to_owned))
        .collect();
    assert!(!ids.contains("helper"), "{ids:?}");
    for kept in ["a_run", "old_isolate", "member", "fresh_isolate"] {
        assert!(ids.contains(kept), "{kept}: {ids:?}");
    }
    assert!(
        d.stderr
            .iter()
            .any(|m| m
                .contains("Removed 1 source-less node(s) left with no edges by re-extraction.")),
        "{:?}",
        d.stderr
    );
}

fn mention(id: &str, label: &str, sf: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "document", "source_file": sf})
}

fn dedup(nodes: Vec<Value>, edges: Vec<Value>) -> (Vec<Value>, Vec<Value>) {
    let out = deduplicate_entities(&nodes, &edges, &DedupOptions::default(), None, None).unwrap();
    (out.nodes, out.edges)
}

fn id_files(nodes: &[Value]) -> BTreeSet<(String, String)> {
    nodes
        .iter()
        .map(|n| {
            (
                n["id"].as_str().unwrap().to_owned(),
                n["source_file"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn a_reference_to_the_top_level_file_still_collapses() {
    // test_dedup.py::test_genuine_reference_to_top_level_file_still_collapses
    // and ..._without_an_edge_... (#4281).
    for edge in [true, false] {
        let nodes = vec![
            mention("docs_protocol", "Docs protocol", "docs/protocol.md"),
            mention("protocol", "protocol", "docs/protocol.md"),
            mention("protocol", "Protocol", "protocol.md"),
        ];
        let edges = if edge {
            vec![
                json!({"source": "docs_protocol", "target": "protocol", "relation": "references",
                        "confidence": "EXTRACTED", "source_file": "docs/protocol.md"}),
            ]
        } else {
            vec![]
        };
        let (kept, kept_edges) = dedup(nodes, edges);
        assert_eq!(
            id_files(&kept),
            [
                ("docs_protocol", "docs/protocol.md"),
                ("protocol", "protocol.md")
            ]
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .into()
        );
        assert_eq!(kept_edges.len(), usize::from(edge));
    }
}

#[test]
fn the_top_level_file_survives_whatever_the_labels_are() {
    // test_dedup.py::test_top_level_file_survives_without_a_references_edge:
    // a bare slice from a file with its own page ranks as a reference.
    let nodes = vec![
        mention("docs_protocol", "Docs protocol", "docs/protocol.md"),
        mention("protocol", "Protocol", "docs/protocol.md"),
        mention("protocol", "Protocol (top-level)", "protocol.md"),
    ];
    for order in [nodes.clone(), nodes.into_iter().rev().collect()] {
        let (kept, _) = dedup(order, vec![]);
        let survivor = kept.iter().find(|n| n["id"] == "protocol").unwrap();
        assert_eq!(survivor["label"], "Protocol (top-level)");
        assert_eq!(survivor["source_file"], "protocol.md");
    }
}

#[test]
fn a_dropped_folder_doc_with_its_own_stem_entity_falls_back_to_collapse() {
    // test_dedup.py::test_dropped_folder_doc_with_own_stem_entity_falls_back_to_collapse.
    let (kept, _) = dedup(
        vec![
            mention("protocol", "protocol", "docs/protocol.md"),
            mention("docs_protocol_header", "Header", "docs/protocol.md"),
            mention("protocol", "Protocol", "protocol.md"),
        ],
        vec![],
    );
    assert_eq!(
        id_files(&kept),
        [
            ("docs_protocol_header", "docs/protocol.md"),
            ("protocol", "protocol.md")
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .into()
    );
}

fn cache(function: &str, arguments: Value) -> Value {
    graphicle::dispatch(
        &json!({"operation": "cache", "function": function, "arguments": arguments}),
    )
    .unwrap()
}

#[test]
fn scoping_keeps_edges_between_kept_nodes_from_another_file() {
    // test_chunking.py (#2276): an edge attributed to an undispatched file
    // stays when both of its endpoints are kept nodes.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    let result = json!({
        "nodes": [
            {"id": "a", "label": "A", "source_file": "a.md"},
            {"id": "b", "label": "B", "source_file": "a.md"},
            {"id": "c", "label": "C", "source_file": "other.md"},
        ],
        "edges": [
            {"source": "a", "target": "b", "relation": "references", "source_file": "other.md"},
            {"source": "a", "target": "c", "relation": "references", "source_file": "other.md"},
        ],
        "hyperedges": [],
    });
    let out = cache(
        "scope_semantic_result",
        json!({"result": result, "root": root, "allowed_source_files": ["a.md"]}),
    );
    let value = &out["value"];
    let edges = value["result"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{value}");
    assert_eq!(edges[0]["target"], "b");
    assert_eq!(value["dropped_item_count"], 2, "{value}");
}

#[test]
fn watch_rebuild_keeps_hub_names_in_the_pending_sidecar() {
    // test_watch.py (#3334): hub-fallback names go to
    // .graphicle_labels.pending.json; the tracked labels file holds only
    // curated names, and a curated name stays there.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("app.py"),
        "def alpha():\n    return beta()\n\ndef beta():\n    return 1\n",
    )
    .unwrap();
    let options = RebuildOptions {
        acquire_lock: false,
        force: true,
        ..Default::default()
    };
    let result = watch::rebuild(dir.path(), &options).unwrap();
    assert!(result.success, "{:?}", result.messages);
    let out = dir.path().join(".graphicle");
    let read = |name: &str| -> Value {
        serde_json::from_str(&fs::read_to_string(out.join(name)).unwrap()).unwrap()
    };
    assert_eq!(read(".graphicle_labels.json"), json!({}));
    let pending = read(".graphicle_labels.pending.json");
    assert!(!pending.as_object().unwrap().is_empty(), "{pending}");
    // Curating every name empties the sidecar, which is then removed.
    let curated: serde_json::Map<String, Value> = pending
        .as_object()
        .unwrap()
        .keys()
        .map(|k| (k.clone(), json!(format!("Curated {k}"))))
        .collect();
    fs::write(
        out.join(".graphicle_labels.json"),
        Value::Object(curated.clone()).to_string(),
    )
    .unwrap();
    fs::write(dir.path().join("app.py"), "def alpha():\n    return beta()\n\ndef beta():\n    return 2\n\ndef gamma():\n    return alpha()\n").unwrap();
    let result = watch::rebuild(dir.path(), &options).unwrap();
    assert!(result.success, "{:?}", result.messages);
    let labels = read(".graphicle_labels.json");
    for (k, v) in &curated {
        if labels.get(k).is_some() {
            assert_eq!(&labels[k], v);
        }
    }
    let leftover = out.join(".graphicle_labels.pending.json");
    if leftover.exists() {
        let pending = read(".graphicle_labels.pending.json");
        assert!(
            pending
                .as_object()
                .unwrap()
                .keys()
                .all(|k| labels.get(k).is_none()),
            "{pending} {labels}"
        );
    }
}
