// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
//! Build, dedup, detect, cluster, global graph, watch and `.graphiclerc`
//! changes ported from Graphify 0.9.78-0.9.88 (port unit "build"). Cases
//! follow the upstream tests named in each comment.
use graphicle::build::{
    BuildOptions, Diagnostics, backfill_origin, build, build_from_json, build_from_json_with,
    dedupe_edges, execution_wins, finalize_raw_graph_endpoints, fold_edge_aliases,
    fold_node_aliases, is_external_stub, lost_saved_semantic_nodes,
};
use graphicle::dedup::{DedupOptions, deduplicate_entities, norm, rank_path};
use graphicle::detect::{
    DetectOptions, Detector, EncodingWarnings, IgnoreState, ManifestOptions, save_manifest,
};
use graphicle::graph::Graph;
use graphicle::rcfile::{self, LanguageOverrides};
use graphicle::watch::{self, RebuildOptions};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn ids(graph: &Graph) -> BTreeSet<String> {
    graph
        .nodes()
        .iter()
        .filter_map(|n| n.id.as_str().map(str::to_owned))
        .collect()
}

fn has_edge(graph: &Graph, a: &str, b: &str) -> bool {
    !graph.edges_between(&json!(a), &json!(b)).is_empty()
}

fn edge(graph: &Graph, a: &str, b: &str) -> Map<String, Value> {
    let edges = graph.edges_between(&json!(a), &json!(b));
    edges.first().expect("edge exists").attributes.to_map()
}

fn built(mut extraction: Value, directed: bool) -> Graph {
    build_from_json(&mut extraction, directed, None, &mut Diagnostics::default()).unwrap()
}

// --- test_external_stub_endpoints / test_no_cluster_undeclared_endpoints (0.9.80) ---

#[test]
fn external_stubs_carry_no_tier_and_lose_a_stale_stamp() {
    let stub = json!({"id": "os", "label": "os", "file_type": "concept", "type": "external",
                      "external": true, "source_file": "", "_origin": "semantic"});
    assert!(is_external_stub(&stub));
    let mut stamped = stub.clone();
    backfill_origin(&mut stamped);
    assert!(stamped.get("_origin").is_none());
    // `external` must be the boolean true and the node sourceless.
    assert!(!is_external_stub(
        &json!({"external": 1, "type": "external"})
    ));
    assert!(!is_external_stub(
        &json!({"external": true, "type": "external", "source_file": "a.py"})
    ));
    let mut legacy = json!({"id": "f", "source_location": "L3"});
    backfill_origin(&mut legacy);
    assert_eq!(legacy["_origin"], "ast");
    let mut semantic = json!({"id": "c", "label": "Concept"});
    backfill_origin(&mut semantic);
    assert_eq!(semantic["_origin"], "semantic");
}

#[test]
fn raw_graph_endpoints_follow_build_from_json_rules() {
    let mut data = json!({
        "nodes": [
            {"id": "app", "label": "app.py", "source_file": "app.py"},
            {"id": "docs_guide", "label": "guide.md", "source_file": "docs/guide.md"},
            {"id": "gone", "label": "gone", "file_type": "concept", "type": "external",
             "external": true, "source_file": ""},
            {"id": "kept_by_hyperedge", "label": "x", "file_type": "concept",
             "type": "external", "external": true, "source_file": ""}
        ],
        "links": [
            {"source": "app", "target": "os", "relation": "imports"},
            {"source": "app", "target": "Docs-Guide", "relation": "references"},
            {"source": "app", "target": "missing_doc", "relation": "references"},
            {"source": "app", "target": "print", "relation": "calls"}
        ],
        "hyperedges": [{"id": "h", "nodes": ["app", "kept_by_hyperedge"]}]
    });
    finalize_raw_graph_endpoints(&mut data);
    let node_ids: BTreeSet<_> = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect();
    // The import gets its stub; an unreferenced carried stub is dropped.
    assert_eq!(
        node_ids,
        ["app", "docs_guide", "kept_by_hyperedge", "os"]
            .map(String::from)
            .into()
    );
    let links = data["links"].as_array().unwrap();
    assert_eq!(links.len(), 2, "{links:?}");
    assert_eq!(links[0]["target"], "os");
    // A normalization twin is repointed onto the declared id.
    assert_eq!(links[1]["target"], "docs_guide");
}

// --- test_type_only_import_cycles (0.9.80, #3159) ---

#[test]
fn type_only_survives_only_when_every_folded_import_is_type_only() {
    let both = dedupe_edges(&[
        json!({"source": "a", "target": "b", "relation": "imports", "type_only": true}),
        json!({"source": "a", "target": "b", "relation": "imports", "type_only": true}),
    ])
    .unwrap();
    assert_eq!(both.len(), 1);
    assert_eq!(both[0]["type_only"], true);
    let mixed = dedupe_edges(&[
        json!({"source": "a", "target": "b", "relation": "imports", "type_only": true}),
        json!({"source": "a", "target": "b", "relation": "imports"}),
        json!({"source": "a", "target": "b", "relation": "imports", "type_only": true}),
    ])
    .unwrap();
    assert_eq!(mixed.len(), 1);
    assert!(mixed[0].get("type_only").is_none());
    let (mut a, mut b) = (json!({"type_only": true}), json!({"type_only": false}));
    execution_wins(&mut a, &mut b);
    assert!(a.get("type_only").is_none() && b.get("type_only").is_none());

    let nodes = json!([
        {"id": "a", "label": "a.ts", "file_type": "code", "source_file": "a.ts"},
        {"id": "b", "label": "b.ts", "file_type": "code", "source_file": "b.ts"}
    ]);
    for directed in [false, true] {
        let graph = built(
            json!({"nodes": nodes, "edges": [
                {"source": "a", "target": "b", "relation": "imports_from", "type_only": true,
                 "confidence": "EXTRACTED", "source_file": "a.ts"},
                {"source": "a", "target": "b", "relation": "imports_from",
                 "confidence": "EXTRACTED", "source_file": "a.ts"}
            ]}),
            directed,
        );
        assert!(
            !edge(&graph, "a", "b").contains_key("type_only"),
            "{directed}"
        );
    }
    // Undirected: b's runtime import of a is another file's import, not a
    // second leg of a's type-only import; the surviving edge keeps its marker.
    let graph = built(
        json!({"nodes": nodes, "edges": [
            {"source": "a", "target": "b", "relation": "imports_from", "type_only": true,
             "confidence": "EXTRACTED", "source_file": "a.ts"},
            {"source": "b", "target": "a", "relation": "imports_from",
             "confidence": "EXTRACTED", "source_file": "b.ts"}
        ]}),
        false,
    );
    let kept = edge(&graph, "a", "b");
    assert_eq!(kept["_src"], "a");
    assert_eq!(kept["type_only"], true);
}

// --- test_relation_collapse_precedence / test_csharp_object_creation (0.9.83, #4256) ---

#[test]
fn membership_edge_beats_the_opposite_call_on_an_undirected_graph() {
    for (owner, member) in [("report", "report_fault"), ("zz_report", "aa_fault")] {
        let nodes = json!([
            {"id": owner, "label": "Report", "file_type": "code", "source_file": "r.cs"},
            {"id": member, "label": ".Fault()", "file_type": "code", "source_file": "r.cs"}
        ]);
        for reverse in [false, true] {
            let mut edges = vec![
                json!({"source": owner, "target": member, "relation": "method",
                       "confidence": "EXTRACTED", "source_file": "r.cs"}),
                json!({"source": member, "target": owner, "relation": "calls",
                       "confidence": "EXTRACTED", "source_file": "r.cs"}),
            ];
            if reverse {
                edges.reverse();
            }
            let graph = built(json!({"nodes": nodes, "edges": edges}), false);
            let kept = edge(&graph, owner, member);
            assert_eq!(kept["relation"], "method", "{owner} {reverse}");
            assert_eq!(kept["_src"], owner);
            // Directed graphs keep both arcs.
            let graph = built(json!({"nodes": nodes, "edges": edges}), true);
            assert_eq!(edge(&graph, owner, member)["relation"], "method");
            assert_eq!(edge(&graph, member, owner)["relation"], "calls");
        }
    }
}

// --- #4261 (0.9.84): a stdlib-named module never claims the bare-stem alias ---

#[test]
fn stdlib_named_python_module_does_not_capture_the_stdlib_import() {
    let graph = built(
        json!({"nodes": [
            {"id": "scripts_logging", "label": "logging.py", "file_type": "code",
             "source_file": "scripts/logging.py", "_origin": "ast"},
            {"id": "scripts_helpers", "label": "helpers.py", "file_type": "code",
             "source_file": "scripts/helpers.py", "_origin": "ast"},
            {"id": "app", "label": "app.py", "file_type": "code", "source_file": "app.py",
             "_origin": "ast"}
        ], "edges": [
            {"source": "app", "target": "logging", "relation": "imports",
             "confidence": "EXTRACTED", "source_file": "app.py"},
            {"source": "app", "target": "helpers", "relation": "imports",
             "confidence": "EXTRACTED", "source_file": "app.py"}
        ]}),
        false,
    );
    assert!(has_edge(&graph, "app", "logging"));
    assert!(!has_edge(&graph, "app", "scripts_logging"));
    let stub = graph.node_index(&json!("logging")).unwrap();
    assert_eq!(
        graph.nodes()[stub].attributes.get("external"),
        Some(&json!(true))
    );
    // A non-stdlib stem still heals through its bare alias.
    assert!(has_edge(&graph, "app", "scripts_helpers"));
}

// --- test_lost_saved_semantic_nodes (0.9.87/0.9.88, #4115/#4389) ---

fn sem(id: &str, label: &str, sf: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "code", "source_file": sf,
           "source_location": null, "_origin": "semantic"})
}

fn ast(id: &str, label: &str, sf: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "code", "source_file": sf,
           "source_location": "L10", "_origin": "ast"})
}

fn lost(saved: &[Value], new: &[Value], deleted: &[&str]) -> Vec<String> {
    let deleted = deleted.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    lost_saved_semantic_nodes(saved, new, &deleted)
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn lost_saved_semantic_nodes_exempts_ast_twins_and_deleted_files() {
    let saved = [sem("ghost_bar", "bar", "src/foo.py")];
    assert!(
        lost(
            &[sem("foo_bar", "bar", "src/foo.py")],
            &[sem("foo_bar", "bar", "src/foo.py")],
            &[]
        )
        .is_empty()
    );
    assert!(lost(&saved, &[ast("foo_py_bar", "bar", "src/foo.py")], &[]).is_empty());
    assert_eq!(
        lost(&saved, &[sem("other_bar", "bar", "src/foo.py")], &[]),
        ["ghost_bar"]
    );
    assert_eq!(
        lost(
            &saved,
            &[
                ast("foo_py_bar_1", "bar", "src/foo.py"),
                ast("foo_py_bar_2", "bar", "src\\foo.py")
            ],
            &[]
        ),
        ["ghost_bar"]
    );
    assert!(
        lost(
            &[sem("ghost_bar", "bar", "src\\foo.py")],
            &[ast("foo_py_bar", "bar", "src/foo.py")],
            &[]
        )
        .is_empty()
    );
    assert_eq!(
        lost(
            &saved,
            &[
                ast("foo_py_baz", "baz", "src/foo.py"),
                ast("other_py_bar", "bar", "src/other.py")
            ],
            &[]
        ),
        ["ghost_bar"]
    );
    assert!(lost(&saved, &[], &["src/foo.py"]).is_empty());
    assert_eq!(lost(&saved, &[], &["src/other.py"]), ["ghost_bar"]);
    let malformed = [json!({"id": "s1", "label": "S1", "source_file": 123, "_origin": "semantic"})];
    assert_eq!(
        lost(
            &malformed,
            &[json!({"id": "n1", "label": "N1", "source_file": true})],
            &[]
        ),
        ["s1"]
    );
    // Import stubs are rebuilt from this run's import edges.
    let stub = json!({"id": "os", "type": "external", "external": true, "source_file": ""});
    assert!(lost(&[stub], &[], &[]).is_empty());
}

#[test]
fn shrink_guard_refuses_a_lost_semantic_node_even_when_the_graph_grows() {
    let existing = json!({"nodes": [sem("design_note", "Design note", "docs/a.md")]});
    let grown = json!({"nodes": [ast("a", "a", "a.py"), ast("b", "b", "b.py")]});
    let none = BTreeSet::new();
    let mut messages = vec![];
    assert!(!watch::check_shrink(
        false,
        &existing,
        &grown,
        false,
        None,
        &none,
        &none,
        &mut messages
    ));
    assert!(
        messages[0].contains("saved semantic node(s) are missing from the new graph: design_note.")
    );
    let deleted = BTreeSet::from(["docs/a.md".to_owned()]);
    assert!(watch::check_shrink(
        false,
        &existing,
        &grown,
        false,
        None,
        &none,
        &deleted,
        &mut vec![]
    ));
    // A ghost folded into its AST twin is not lost (#4389).
    let ghost = sem("src_models_user_ts_user", "User", "src/models/user.ts");
    let existing = json!({"nodes": [ghost]});
    let graph = built(
        json!({"nodes": [ghost, ast("user_user", "User", "src/models/user.ts")], "edges": []}),
        false,
    );
    assert_eq!(ids(&graph), BTreeSet::from(["user_user".to_owned()]));
    let new = graph.to_node_link("links");
    assert!(watch::check_shrink(
        false,
        &existing,
        &new,
        false,
        None,
        &none,
        &none,
        &mut vec![]
    ));
}

// --- test_alias_folding (0.9.88, #4391) ---

#[test]
fn node_file_aliases_and_edge_relationship_fold_onto_canonical_keys() {
    for alias in ["file", "path", "filepath"] {
        let mut node = json!({"id": "a", "label": "A", alias: "foo.ts"});
        fold_node_aliases(&mut node);
        assert_eq!(node["source_file"], "foo.ts", "{alias}");
        assert!(node.get(alias).is_none());
    }
    let mut node = json!({"id": "a", "label": "A", "source_file": "real.py", "file": "other.py"});
    fold_node_aliases(&mut node);
    assert_eq!(node["source_file"], "real.py");
    // The first non-blank alias wins.
    let mut node = json!({"id": "a", "file": "  ", "path": "p.py", "filepath": "f.py"});
    fold_node_aliases(&mut node);
    assert_eq!(node["source_file"], "p.py");
    assert_eq!(node["file"], "  ");
    let mut folded = json!({"source": "a", "target": "b", "relationship": "calls"});
    fold_edge_aliases(&mut folded);
    assert_eq!(folded["relation"], "calls");
    assert!(folded.get("relationship").is_none());
    let mut folded =
        json!({"source": "a", "target": "b", "relation": "imports", "relationship": "calls"});
    fold_edge_aliases(&mut folded);
    assert_eq!(folded["relation"], "imports");

    let mut messages = Diagnostics::default();
    let graph = build_from_json_with(
        &mut json!({"nodes": [
            {"id": "a", "label": "A", "file_type": "code", "file": "foo.ts"},
            {"id": "b", "label": "B", "file_type": "code", "path": "bar.py"}
        ], "edges": [
            {"source": "a", "target": "b", "relationship": "calls", "confidence": "EXTRACTED",
             "source_file": "foo.ts"}
        ]}),
        false,
        false,
        None,
        &mut messages,
    )
    .unwrap();
    assert!(
        !messages
            .stderr
            .iter()
            .any(|m| m.contains("missing required field"))
    );
    assert_eq!(edge(&graph, "a", "b")["relation"], "calls");
}

// --- test_dedup.py (0.9.85, 0.9.86, 0.9.88; #4281, #4317, #4326, #4362) ---

fn dedup(nodes: Vec<Value>, edges: Vec<Value>) -> (Vec<Value>, Vec<Value>, Vec<String>) {
    let out = deduplicate_entities(&nodes, &edges, &DedupOptions::default(), None, None).unwrap();
    (out.nodes, out.edges, out.stderr)
}

fn doc(id: &str, label: &str, sf: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "document", "source_file": sf})
}

#[test]
fn files_differing_only_by_extension_keep_separate_ids() {
    let nodes = vec![
        json!({"id": "firmware_lib_portal", "label": "portal.py", "file_type": "code",
               "source_file": "firmware/lib/portal.py"}),
        json!({"id": "firmware_lib_portal", "label": "Brick provisioning portal (portal.html)",
               "file_type": "document", "source_file": "firmware/lib/portal.html"}),
    ];
    let edges = vec![
        json!({"source": "firmware_lib_portal", "target": "firmware_lib_portal",
                            "relation": "serves", "source_file": "firmware/lib/portal.html"}),
    ];
    for order in [false, true] {
        let mut nodes = nodes.clone();
        if order {
            nodes.reverse();
        }
        let (nodes, edges, stderr) = dedup(nodes, edges.clone());
        assert_eq!(nodes.len(), 2);
        let id_of = |sf: &str| nodes.iter().find(|n| n["source_file"] == sf).unwrap()["id"].clone();
        assert_eq!(id_of("firmware/lib/portal.py"), "firmware_lib_portal");
        assert_eq!(
            id_of("firmware/lib/portal.html"),
            "firmware_lib_portal_html"
        );
        assert_eq!(edges[0]["source"], "firmware_lib_portal_html");
        assert_eq!(edges[0]["target"], "firmware_lib_portal_html");
        assert!(!stderr.iter().any(|m| m.contains("WARNING")), "{stderr:?}");
    }
}

#[test]
fn bare_ids_that_dropped_the_directory_are_path_qualified_per_file() {
    let (nodes, _, stderr) = dedup(
        vec![
            doc("protocol", "protocol", "docs/protocol.md"),
            doc(
                "protocol",
                "protocol (top-level)",
                "docs/firmware/protocol.md",
            ),
        ],
        vec![],
    );
    let ids: BTreeSet<_> = nodes
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        ["docs_firmware_protocol", "docs_protocol"]
            .map(String::from)
            .into()
    );
    assert!(
        !stderr
            .iter()
            .any(|m| m.contains("minted by two different files"))
    );

    // protocol.md exists: the bare id may name it, so the group collapses.
    let (nodes, _, stderr) = dedup(
        vec![
            doc("protocol", "protocol", "protocol.md"),
            doc("protocol", "protocol (docs)", "docs/protocol.md"),
            doc(
                "protocol",
                "protocol (firmware)",
                "docs/firmware/protocol.md",
            ),
        ],
        vec![],
    );
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["source_file"], "protocol.md");
    assert!(stderr.iter().any(|m| m.contains("WARNING")));

    // Each file's edges follow its own renamed node.
    let path_dropped = || {
        (
            vec![
                doc("x", "X A", "a/x.md"),
                doc("x_intro", "Intro A", "a/x.md"),
                doc("x", "X B", "b/x.md"),
                doc("x_intro", "Intro B", "b/x.md"),
            ],
            vec![
                json!({"source": "x", "target": "x_intro", "relation": "references",
                       "confidence": "EXTRACTED", "source_file": "a/x.md"}),
                json!({"source": "x", "target": "x_intro", "relation": "references",
                       "confidence": "EXTRACTED", "source_file": "b/x.md"}),
            ],
        )
    };
    let (nodes, edges) = path_dropped();
    let (nodes, edges, stderr) = dedup(nodes, edges);
    let ids: BTreeSet<_> = nodes
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        ["a_x", "a_x_intro", "b_x", "b_x_intro"]
            .map(String::from)
            .into()
    );
    for e in &edges {
        let prefix = if e["source_file"] == "a/x.md" {
            "a"
        } else {
            "b"
        };
        assert_eq!(e["source"], format!("{prefix}_x"));
        assert_eq!(e["target"], format!("{prefix}_x_intro"));
    }
    assert!(
        !stderr
            .iter()
            .any(|m| m.contains("minted by two different files"))
    );

    // A reference from another file cannot be attributed: left alone, one warning.
    let (mut nodes, mut edges) = path_dropped();
    nodes.push(doc("c_note", "Note C", "c.md"));
    edges.push(
        json!({"source": "c_note", "target": "x", "relation": "references",
                      "confidence": "INFERRED", "source_file": "c.md"}),
    );
    let (_, edges, stderr) = dedup(nodes, edges);
    let foreign = edges.iter().find(|e| e["source_file"] == "c.md").unwrap();
    assert_eq!(foreign["target"], "x");
    let warning = "[graphicle] WARNING: node 'x' was minted by several files and renamed per file; 1 reference(s) from other files could not be attributed and were left unresolved.";
    assert_eq!(
        stderr.iter().filter(|m| *m == warning).count(),
        1,
        "{stderr:?}"
    );

    // An absolute path is never baked into an id.
    let (nodes, _, stderr) = dedup(
        vec![
            doc("x", "X A", "/srv/ckout/a/x.md"),
            doc("x", "X B", "/srv/ckout/b/x.md"),
        ],
        vec![],
    );
    assert!(
        !nodes
            .iter()
            .any(|n| n["id"].as_str().unwrap().contains("ckout"))
    );
    assert!(
        stderr
            .iter()
            .any(|m| m.contains("minted by two different files"))
    );

    // A lone path-dropped id is left to build_from_json's remap.
    let (nodes, _, _) = dedup(
        vec![doc("x", "X A", "a/x.md"), doc("other", "Other", "a/y.md")],
        vec![],
    );
    let ids: BTreeSet<_> = nodes
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["other", "x"].map(String::from).into());
}

#[test]
fn a_mention_of_another_document_is_not_a_second_document() {
    for (docs_label, mention_label) in [
        ("Protocol", "protocol (top-level)"),
        ("Protocol overview", "protocol"),
    ] {
        let fw = "docs/firmware/protocol.md";
        let extraction = json!({
            "nodes": [doc("protocol", docs_label, "docs/protocol.md"),
                      doc("docs_firmware_protocol", "Firmware protocol", fw),
                      doc("protocol", mention_label, fw)],
            "edges": [{"source": "docs_firmware_protocol", "target": "protocol",
                       "relation": "references", "confidence": "EXTRACTED", "source_file": fw}]
        });
        let graph = build(
            &[extraction],
            &BuildOptions::default(),
            &mut Diagnostics::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            ids(&graph),
            ["docs_firmware_protocol", "docs_protocol"]
                .map(String::from)
                .into(),
            "{docs_label}"
        );
        assert!(has_edge(&graph, "docs_firmware_protocol", "docs_protocol"));
        assert_eq!(graph.edge_count(), 1);
    }
}

#[test]
fn label_norm_keeps_combining_marks_and_relative_rank_paths_stay_relative() {
    assert_eq!(norm(&json!("हिंदी")), "हिंदी");
    assert_eq!(norm(&json!("தமிழ்")), "தமிழ்");
    assert_eq!(norm(&json!("हिंदी - भाषा_नोट")), "हिंदी भाषा नोट");
    assert_eq!(norm(&json!("Graph_Builder -- v2")), "graph builder v2");
    let (nodes, _, _) = dedup(
        vec![
            json!({"id": "h1", "label": "कि", "source_file": "a.md", "file_type": "concept"}),
            json!({"id": "h2", "label": "की", "source_file": "a.md", "file_type": "concept"}),
            json!({"id": "h3", "label": "के", "source_file": "a.md", "file_type": "concept"}),
        ],
        vec![],
    );
    assert_eq!(nodes.len(), 3);
    // #4326: a relative stored path is already root-relative.
    let root = std::env::current_dir().unwrap().join("elsewhere");
    assert_eq!(rank_path("lib/portal.py", Some(&root)), "lib/portal.py");
    assert_eq!(
        rank_path(&root.join("lib/portal.py").to_string_lossy(), Some(&root)),
        "lib/portal.py"
    );
}

// --- test_detect.py (0.9.85 #4242, 0.9.86 #4335, 0.9.80 #3581) ---

fn touch(root: &Path, rels: &[&str]) {
    for rel in rels {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x = 1\n").unwrap();
    }
}

fn ignored_among(ignore_text: &str, ignored: &[&str], kept: &[&str]) -> BTreeSet<String> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    touch(&root, ignored);
    touch(&root, kept);
    fs::write(root.join(".graphicleignore"), ignore_text).unwrap();
    let mut rules = IgnoreState::new(&root, &[], true, &mut EncodingWarnings::default()).unwrap();
    ignored
        .iter()
        .chain(kept)
        .filter(|rel| rules.is_scan_ignored(&root.join(rel)))
        .map(|rel| rel.to_string())
        .collect()
}

#[test]
fn an_unanchored_pattern_matches_a_name_not_a_path() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        (
            "test_*.py\n",
            &["test_one.py", "pkg/test_two.py"],
            &["test_helpers/util.py", "test_helpers/sub/deep.py"],
        ),
        (
            "doc*.md\n",
            &["doc.md", "pkg/docs.md"],
            &["docs/api.md", "docs/guide/readme.md"],
        ),
        (
            "s*c.py\n",
            &["sc.py", "pkg/spec.py"],
            &["src/c.py", "src/a/c.py"],
        ),
        (
            "*.py\n!vendor\n",
            &["c.py", "vendor/a.py", "vendor/sub/b.py"],
            &["vendor/notes.txt"],
        ),
        (
            "*.py\n!test_*.py\n",
            &["util.py", "test_helpers/util.py"],
            &["test_one.py", "pkg/test_two.py"],
        ),
        (
            "build\n",
            &["build/out.py", "pkg/build/deep/out.py"],
            &["build.py", "builder/ok.py", "notbuild/ok.py"],
        ),
        (
            "build/\n",
            &["build/out.py", "pkg/build/deep/out.py"],
            &["build.py", "builder/ok.py", "notbuild/ok.py"],
        ),
        // A re-included directory brings its files back.
        (
            "tags\n!tags/\n",
            &["tags"],
            &["src/tags/handler.py", "src/tags/sub/deep.py"],
        ),
        (
            "cache*\n!cache_keep/\n",
            &["cache.db", "cache_old/a.txt"],
            &["cache_keep/a.txt"],
        ),
    ];
    for (text, ignored, kept) in cases {
        assert_eq!(
            ignored_among(text, ignored, kept),
            ignored.iter().map(|s| s.to_string()).collect(),
            "{text:?}"
        );
    }
}

fn code_files(detected: &Value) -> Vec<String> {
    detected["files"]["code"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().replace('\\', "/"))
        .collect()
}

#[test]
fn git_core_excludes_file_applies_inside_a_repo_only() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    git(&["init", "-q"]);
    let excludes = dir.path().join("my-ignore");
    fs::write(&excludes, "secret.py\n").unwrap();
    git(&["config", "core.excludesFile", &excludes.to_string_lossy()]);
    fs::write(repo.join("a.py"), "x = 1").unwrap();
    fs::write(repo.join("secret.py"), "SECRET = 'abc'").unwrap();
    let mut detector = Detector::default();
    let code = code_files(&detector.detect(&repo, &DetectOptions::default()).unwrap());
    assert!(code.iter().any(|f| f.ends_with("/a.py")));
    assert!(!code.iter().any(|f| f.ends_with("/secret.py")), "{code:?}");
    // A project re-include beats the user-wide file; --no-gitignore drops it.
    fs::write(repo.join(".gitignore"), "!secret.py\n").unwrap();
    let code = code_files(&detector.detect(&repo, &DetectOptions::default()).unwrap());
    assert!(code.iter().any(|f| f.ends_with("/secret.py")));
    fs::remove_file(repo.join(".gitignore")).unwrap();
    let options = DetectOptions {
        gitignore: false,
        ..Default::default()
    };
    let code = code_files(&detector.detect(&repo, &options).unwrap());
    assert!(code.iter().any(|f| f.ends_with("/secret.py")));
}

fn checkout_files(checkout: &Path) -> Vec<PathBuf> {
    let files = vec![
        checkout.join("a.py"),
        checkout.join("b.py"),
        checkout.join("src/c.py"),
    ];
    fs::create_dir_all(checkout.join("src")).unwrap();
    for (file, text) in files.iter().zip(["a = 1\n", "b = 2\n", "c = 3\n"]) {
        fs::write(file, text).unwrap();
    }
    files
}

fn save_checkout(checkout: &Path, files: &[PathBuf], root: Option<&Path>, scan: bool) -> Value {
    let manifest = checkout.join(".graphicle/manifest.json");
    let list = files
        .iter()
        .map(|p| json!(p.to_string_lossy()))
        .collect::<Vec<_>>();
    let mut groups = Map::new();
    groups.insert("code".into(), Value::Array(list));
    save_manifest(
        &groups,
        &manifest,
        "both",
        &ManifestOptions {
            root: root.map(Path::to_path_buf),
            scan_corpus: scan.then(|| {
                files
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect()
            }),
            ..Default::default()
        },
    )
    .unwrap();
    serde_json::from_str(&fs::read_to_string(manifest).unwrap()).unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn a_full_save_drops_the_previous_checkouts_absolute_keys() {
    for scan in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (a, b) = (base.join("checkout-a"), base.join("checkout-b"));
        let files_a = checkout_files(&a);
        save_checkout(&a, &files_a, None, false);
        copy_tree(&a, &b);
        let files_b = ["a.py", "b.py", "src/c.py"].map(|f| b.join(f)).to_vec();
        let raw = save_checkout(&b, &files_b, Some(&b), scan);
        let keys = raw
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let old = keys
            .iter()
            .filter(|k| k.starts_with(&*a.to_string_lossy()))
            .count();
        assert!(keys.contains("a.py") && keys.contains("b.py") && keys.contains("src/c.py"));
        // A subset save (no scan corpus) keeps every row it was not given.
        assert_eq!(old, if scan { 0 } else { 3 }, "{keys:?}");
    }
    // One include row that shares a basename with a project file stays.
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (root, outside) = (base.join("proj"), base.join("proj-include"));
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let (a, include) = (root.join("a.py"), outside.join("a.py"));
    fs::write(&a, "x = 1\n").unwrap();
    fs::write(&include, "z = 3\n").unwrap();
    save_checkout(&root, &[a.clone(), include.clone()], Some(&root), false);
    let raw = save_checkout(&root, &[a], Some(&root), true);
    assert!(raw.get("a.py").is_some());
    assert!(raw.get(&*include.to_string_lossy()).is_some(), "{raw}");
}

// --- test_language_overrides / test_language_override_lifecycle (0.9.87, #2961) ---

#[test]
fn a_declared_extension_is_classified_stamped_and_invalidated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("view.tpl"), "export const view = () => 1;\n").unwrap();
    fs::write(
        root.join("lib.inc"),
        "<?php function helper() { return 1; }\n",
    )
    .unwrap();
    let mut detector = Detector::default();
    let undeclared = detector.detect(&root, &DetectOptions::default()).unwrap();
    assert!(
        !code_files(&undeclared)
            .iter()
            .any(|f| f.ends_with("view.tpl"))
    );
    fs::write(
        root.join(".graphiclerc"),
        "language.tpl=typescript\nlanguage.inc=php\n",
    )
    .unwrap();
    let declared = detector.detect(&root, &DetectOptions::default()).unwrap();
    let code = code_files(&declared);
    assert!(code.iter().any(|f| f.ends_with("view.tpl")), "{code:?}");
    assert_eq!(rcfile::effective_suffix(root.join("lib.inc")), ".php");

    // The manifest records the declared language; a later change of the
    // declaration re-queues the file even though its bytes did not change.
    let manifest = root.join(".graphicle/manifest.json");
    let options = ManifestOptions {
        root: Some(root.clone()),
        ..Default::default()
    };
    save_manifest(
        declared["files"].as_object().unwrap(),
        &manifest,
        "both",
        &options,
    )
    .unwrap();
    let rows: Value = serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(rows["lib.inc"]["language"], "language=.php");
    let unchanged = detector
        .detect_incremental(&root, &manifest, "ast", &DetectOptions::default())
        .unwrap();
    assert!(
        !code_files(&json!({"files": unchanged["new_files"]}))
            .iter()
            .any(|f| f.ends_with("lib.inc"))
    );
    fs::write(root.join(".graphiclerc"), "language.tpl=typescript\n").unwrap();
    let changed = detector
        .detect_incremental(&root, &manifest, "ast", &DetectOptions::default())
        .unwrap();
    assert!(
        code_files(&json!({"files": changed["new_files"]}))
            .iter()
            .any(|f| f.ends_with("lib.inc"))
    );
    rcfile::set_language_overrides(None);
}

#[test]
fn parallel_workers_see_the_callers_declaration() {
    rcfile::set_language_overrides(Some(&LanguageOverrides::from([(
        ".tpl".into(),
        ".ts".into(),
    )])));
    let paths = (0..200)
        .map(|i| PathBuf::from(format!("v{i}.tpl")))
        .collect::<Vec<_>>();
    let suffixes = graphicle::par::map(&paths, 8, |p| rcfile::effective_suffix(p));
    assert!(suffixes.iter().all(|s| s == ".ts"));
    rcfile::set_language_overrides(None);
}

// --- test_cluster_ambiguous_scale (0.9.81, #4199) ---

fn two_cliques() -> Graph {
    let mut links = vec![];
    for side in ["L", "R"] {
        for i in 0..4 {
            for j in i + 1..4 {
                links.push(
                    json!({"source": format!("{side}{i}"), "target": format!("{side}{j}"),
                                  "weight": 1.0, "confidence": "EXTRACTED"}),
                );
            }
        }
    }
    for i in 0..4 {
        links.push(
            json!({"source": format!("L{i}"), "target": format!("R{i}"), "weight": 1.0,
                          "confidence": "AMBIGUOUS"}),
        );
    }
    links.push(json!({"source": "L0", "target": "R1", "weight": 1.0, "confidence": "AMBIGUOUS"}));
    let nodes = ["L0", "L1", "L2", "L3", "R0", "R1", "R2", "R3"].map(|id| json!({"id": id}));
    Graph::from_node_link(
        &json!({"directed": false, "multigraph": false, "nodes": nodes, "links": links}),
        "links",
    )
    .unwrap()
}

#[test]
fn ambiguous_edges_are_scaled_on_a_copy_before_partitioning() {
    let graph = two_cliques();
    let before = graph.to_node_link("links");
    for scale in [0.0, 0.1, 0.5, 1.0] {
        let communities =
            graphicle::cluster::cluster_with(&graph, 1.0, None, "auto", scale).unwrap();
        let covered = communities
            .iter()
            .map(|(_, members)| members.len())
            .sum::<usize>();
        assert_eq!(covered, 8, "{scale}");
        if scale == 0.0 {
            let side = |id: &str| {
                communities
                    .iter()
                    .find(|(_, members)| members.contains(&json!(id)))
                    .unwrap()
                    .0
            };
            assert_ne!(side("L0"), side("R0"));
        }
    }
    // The caller's graph keeps the raw AMBIGUOUS weight.
    assert_eq!(graph.to_node_link("links"), before);
}

// --- test_compact_graph_json / paths (0.9.86, #3413, #4329) ---

#[test]
fn graph_json_writers_are_compact_and_out_dir_rejects_the_project_itself() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.json");
    graphicle::paths::write_json_atomic_compact(&path, &json!({"a": [1, {"b": "é"}]}), true)
        .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{\"a\":[1,{\"b\":\"\\u00e9\"}]}"
    );
    assert!(graphicle::paths::is_single_line(&path).unwrap());
    assert_eq!(
        graphicle::paths::compact_json_text(&json!({"a": "é"})).unwrap(),
        "{\"a\":\"é\"}\n"
    );
    fs::write(&path, "{\n  \"a\": 1\n}").unwrap();
    assert!(!graphicle::paths::is_single_line(&path).unwrap());
    use graphicle::security::{DEFAULT_OUT_DIR, out_dir_from};
    for raw in [None, Some(""), Some("   ")] {
        assert_eq!(out_dir_from(raw), (DEFAULT_OUT_DIR.to_owned(), None));
    }
    for raw in [".", "./", "..", "../..", "a/../.."] {
        let (dir, warning) = out_dir_from(Some(raw));
        assert_eq!(dir, DEFAULT_OUT_DIR, "{raw}");
        assert!(
            warning
                .unwrap()
                .contains("points at the project or a parent of it")
        );
    }
    assert_eq!(out_dir_from(Some(" out ")), ("out".to_owned(), None));
    assert_eq!(
        out_dir_from(Some("../graphs")),
        ("../graphs".to_owned(), None)
    );
}

// --- test_export_direction (global add, 0.9.86, #4066) ---

#[test]
fn global_add_keeps_stored_direction_and_rewires_deduplicated_externals() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, graph: Value| {
        let path = dir.path().join(name);
        fs::write(&path, graph.to_string()).unwrap();
        path
    };
    let first = write(
        "first.json",
        json!({"directed": false, "nodes": [
        {"id": "requests", "label": "requests", "source_file": ""}
    ], "links": []}),
    );
    let second = write(
        "second.json",
        json!({"directed": false, "nodes": [
            {"id": "a", "label": "a", "source_file": "a.py"},
            {"id": "b", "label": "b", "source_file": "b.py"},
            {"id": "requests", "label": "requests", "source_file": ""}
        ], "links": [
            {"source": "b", "target": "a", "relation": "calls"},
            {"source": "a", "target": "requests", "relation": "imports"}
        ]}),
    );
    let mut store = graphicle::global_graph::GlobalStore::new(dir.path().join("store"));
    store.add(&first, "one").unwrap();
    store.add(&second, "two").unwrap();
    let text = fs::read_to_string(&store.graph_path).unwrap();
    assert_eq!(
        text.lines().count(),
        1,
        "the global graph is written compact"
    );
    let saved: Value = serde_json::from_str(&text).unwrap();
    let links = saved["links"].as_array().unwrap();
    let call = links.iter().find(|l| l["relation"] == "calls").unwrap();
    assert_eq!(
        (call["_src"].as_str(), call["_tgt"].as_str()),
        (Some("two::b"), Some("two::a"))
    );
    let import = links.iter().find(|l| l["relation"] == "imports").unwrap();
    assert_eq!(import["_tgt"], "one::requests");
}

// --- test_watch.py (0.9.87, #4274), test_incremental_unscanned_placeholders (0.9.78, #4161) ---

#[test]
fn full_rebuild_requests_queue_a_marker_under_contention() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(".graphicle");
    watch::queue_pending_full(&out).unwrap();
    assert!(watch::drain_pending_full(&out));
    assert!(!watch::drain_pending_full(&out));

    fs::write(dir.path().join("app.py"), "def alpha():\n    return 1\n").unwrap();
    let lock = watch::RebuildLock::acquire(&out, true).unwrap().unwrap();
    let busy = watch::rebuild(
        dir.path(),
        &RebuildOptions {
            no_cluster: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!busy.success);
    assert!(
        busy.messages[0].contains("changes queued"),
        "{:?}",
        busy.messages
    );
    assert!(out.join(".pending_full").exists());
    drop(lock);
    let done = watch::rebuild(
        dir.path(),
        &RebuildOptions {
            no_cluster: true,
            changed_paths: Some(vec![]),
            block_on_lock: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(done.success);
    assert!(!out.join(".pending_full").exists());
    assert!(
        out.join("graph.json").exists(),
        "the queued full request ran"
    );
}

fn graph_of(corpus: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(corpus.join(".graphicle/graph.json")).unwrap())
        .unwrap()
}

fn rebuild_raw(corpus: &Path, changed: Option<Vec<PathBuf>>) {
    let result = watch::rebuild(
        corpus,
        &RebuildOptions {
            changed_paths: changed,
            no_cluster: true,
            acquire_lock: false,
            force: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.success, "{:?}", result.messages);
}

#[test]
fn raw_rebuilds_write_one_compact_line_and_rewrite_a_legacy_layout() {
    let dir = tempfile::tempdir().unwrap();
    let corpus = dir.path().canonicalize().unwrap();
    fs::write(
        corpus.join("app.py"),
        "import os\n\ndef alpha():\n    return beta()\n\n\ndef beta():\n    return os.getcwd()\n",
    )
    .unwrap();
    rebuild_raw(&corpus, None);
    let path = corpus.join(".graphicle/graph.json");
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.ends_with('\n') && !text.contains("\": ") && !text.contains("}, "));
    let data: Value = serde_json::from_str(&text).unwrap();
    assert!(data.get("extracted_sources").is_none() && data.get("failed_sources").is_none());
    assert!(data["hyperedges"].is_array());
    let declared = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].clone())
        .collect::<Vec<_>>();
    for link in data["links"].as_array().unwrap() {
        assert!(
            declared.contains(&link["source"]) && declared.contains(&link["target"]),
            "{link}"
        );
    }
    // Unchanged data is left alone, but a multi-line file is rewritten compact.
    let stamp = fs::metadata(&path).unwrap().modified().unwrap();
    rebuild_raw(&corpus, None);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), stamp);
    fs::write(&path, serde_json::to_string_pretty(&data).unwrap()).unwrap();
    rebuild_raw(&corpus, None);
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 1);
}

#[test]
fn an_unscanned_import_placeholder_survives_an_unrelated_change() {
    let dir = tempfile::tempdir().unwrap();
    let corpus = dir.path().canonicalize().unwrap();
    fs::write(
        corpus.join("app.ts"),
        "export async function load() {\n  return import('./queue.js');\n}\n",
    )
    .unwrap();
    let other = corpus.join("other.ts");
    fs::write(&other, "export function other() {\n  return 1;\n}\n").unwrap();
    rebuild_raw(&corpus, None);
    let placeholders = |graph: &Value| {
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| {
                n["source_file"]
                    .as_str()
                    .is_some_and(|s| s.ends_with("queue.js"))
            })
            .map(|n| n["id"].as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>()
    };
    let before = placeholders(&graph_of(&corpus));
    assert!(
        !before.is_empty(),
        "the fixture mints a placeholder for ./queue.js"
    );
    fs::write(&other, "export function other() {\n  return 2;\n}\n").unwrap();
    rebuild_raw(&corpus, Some(vec![other.clone()]));
    let incremental = graph_of(&corpus);
    assert_eq!(placeholders(&incremental), before);
    assert!(
        incremental["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["target"].as_str().is_some_and(|t| before.contains(t)))
    );
    // Without manifest evidence the old eviction is kept.
    fs::remove_file(corpus.join(".graphicle/manifest.json")).unwrap();
    fs::write(&other, "export function other() {\n  return 3;\n}\n").unwrap();
    rebuild_raw(&corpus, Some(vec![other]));
    assert!(placeholders(&graph_of(&corpus)).is_empty());
}
