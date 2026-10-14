// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
//! Build, export, detect and merge fixes ported from Graphify 0.9.77.
use graphicle::build::{
    BuildOptions, Diagnostics, MergeOptions, build, build_from_json_dedup, build_merge,
};
use graphicle::detect::{DetectOptions, Detector, ManifestOptions, save_manifest};
use graphicle::graph::Graph;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn node_ids(graph: &Graph) -> BTreeSet<String> {
    graph
        .nodes()
        .iter()
        .filter_map(|n| n.id.as_str().map(str::to_string))
        .collect()
}

fn has_edge(graph: &Graph, a: &str, b: &str) -> bool {
    !graph.edges_between(&json!(a), &json!(b)).is_empty()
}

// 3f712fe: with dedup disabled, distinct located nodes keep their own ids.
fn located_nodes() -> Vec<Value> {
    vec![
        json!({"id": "pkg_name", "label": "name", "file_type": "code",
               "source_file": "pkg/plugin.json", "source_location": "L2"}),
        json!({"id": "pkg_author_name", "label": "name", "file_type": "code",
               "source_file": "pkg/plugin.json", "source_location": "L5"}),
        json!({"id": "doc_s1_notes", "label": "Notas", "file_type": "document",
               "source_file": "docs/plan.md", "source_location": "L10"}),
        json!({"id": "doc_s2_notes", "label": "Notas", "file_type": "document",
               "source_file": "docs/plan.md", "source_location": "L80"}),
        json!({"id": "other_name", "label": "name", "file_type": "code",
               "source_file": "pkg/other.json", "source_location": "L2"}),
    ]
}

fn ids_of(nodes: &[Value]) -> BTreeSet<String> {
    nodes
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect()
}

fn build_no_dedup(mut extraction: Value) -> Graph {
    build_from_json_dedup(
        &mut extraction,
        false,
        false,
        false,
        None,
        &mut Diagnostics::default(),
    )
    .unwrap()
}

#[test]
fn distinct_same_file_locations_preserve_ids_and_edges() {
    for reverse in [false, true] {
        let mut nodes = located_nodes();
        if reverse {
            nodes.reverse();
        }
        let graph = build_no_dedup(json!({
            "nodes": nodes,
            "edges": [
                {"source": "pkg_name", "target": "doc_s1_notes", "relation": "references"},
                {"source": "pkg_author_name", "target": "doc_s2_notes", "relation": "references"},
            ],
            "hyperedges": [],
        }));
        assert_eq!(node_ids(&graph), ids_of(&nodes), "reverse={reverse}");
        assert!(has_edge(&graph, "pkg_name", "doc_s1_notes"));
        assert!(has_edge(&graph, "pkg_author_name", "doc_s2_notes"));
    }
}

fn merge_no_dedup(chunk: Value, existing: Value) -> Graph {
    let dir = tempfile::tempdir().unwrap();
    let graph_path = dir.path().join("graph.json");
    fs::write(&graph_path, existing.to_string()).unwrap();
    let options = MergeOptions {
        graph_path: Some(graph_path),
        dedup: false,
        ..Default::default()
    };
    build_merge(&[chunk], &options, &mut Diagnostics::default(), None).unwrap()
}

#[test]
fn empty_no_dedup_merge_keeps_located_graph() {
    let nodes = located_nodes();
    let graph = merge_no_dedup(
        json!({"nodes": [], "edges": [], "hyperedges": []}),
        json!({"nodes": nodes, "edges": [], "hyperedges": []}),
    );
    assert_eq!(node_ids(&graph), ids_of(&nodes));
}

#[test]
fn disabled_dedup_keeps_distinct_semantic_ids_at_identical_locations() {
    let nodes: Vec<Value> = [
        ("a_first", "L2"),
        ("b_second", "L5"),
        ("z_first", "L2"),
        ("z_second", "L5"),
    ]
    .iter()
    .map(|(id, location)| {
        json!({"id": id, "label": "Notes", "file_type": "document", "_origin": "semantic",
               "source_file": "plan.md", "source_location": location})
    })
    .collect();
    let graph = build_no_dedup(json!({
        "nodes": nodes,
        "edges": [{"source": "z_first", "target": "z_second", "relation": "references"}],
        "hyperedges": [],
    }));
    assert_eq!(node_ids(&graph), ids_of(&nodes));
    assert!(has_edge(&graph, "z_first", "z_second"));
}

#[test]
fn ast_twin_remains_canonical_despite_semantic_location_drift() {
    let graph = build_no_dedup(json!({
        "nodes": [
            {"id": "src_render", "label": "render", "file_type": "code", "_origin": "ast",
             "source_file": "src/view.py", "source_location": "L2"},
            {"id": "view_render", "label": "render", "file_type": "code", "_origin": "semantic",
             "source_file": "src/view.py", "source_location": "L5"},
        ],
        "edges": [],
        "hyperedges": [],
    }));
    assert_eq!(node_ids(&graph), BTreeSet::from(["src_render".to_string()]));
}

#[test]
fn full_build_honors_disabled_semantic_dedup() {
    let nodes = located_nodes();
    let options = BuildOptions {
        dedup: false,
        ..Default::default()
    };
    let graph = build(
        &[json!({"nodes": nodes, "edges": [], "hyperedges": []})],
        &options,
        &mut Diagnostics::default(),
        None,
    )
    .unwrap();
    assert_eq!(node_ids(&graph), ids_of(&nodes));
}

#[test]
fn fresh_merge_honors_disabled_semantic_dedup() {
    let nodes = located_nodes();
    let graph = merge_no_dedup(
        json!({"nodes": nodes, "edges": [], "hyperedges": []}),
        json!({"nodes": [], "edges": [], "hyperedges": []}),
    );
    assert_eq!(node_ids(&graph), ids_of(&nodes));
}

// 5eeb037 (#4066): reloading graph.json keeps each edge's stored direction.
struct Stored {
    _dir: tempfile::TempDir,
    root: PathBuf,
    graph: PathBuf,
}

fn graph_json() -> Stored {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let out = root.join(".graphicle");
    fs::create_dir_all(&out).unwrap();
    let node = |id: &str| {
        json!({"id": id, "label": format!("{id}()"), "file_type": "code",
               "source_file": "a.py", "community": 0})
    };
    let nodes = ["callee", "caller", "helper", "util", "legacy_b", "legacy_a"].map(node);
    let data = json!({
        "directed": false,
        "multigraph": false,
        "graph": {},
        "nodes": nodes,
        "links": [
            {"source": "caller", "target": "callee", "relation": "calls",
             "confidence": "EXTRACTED"},
            {"source": "helper", "target": "util", "relation": "calls",
             "confidence": "EXTRACTED"},
            {"source": "legacy_b", "target": "legacy_a", "_src": "legacy_a",
             "_tgt": "legacy_b", "relation": "calls", "confidence": "EXTRACTED"},
        ],
    });
    let graph = out.join("graph.json");
    fs::write(&graph, data.to_string()).unwrap();
    fs::write(
        out.join(".graphicle_analysis.json"),
        json!({"communities": {"0": ["callee", "caller", "helper", "util", "legacy_a", "legacy_b"]}})
            .to_string(),
    )
    .unwrap();
    Stored {
        _dir: dir,
        root,
        graph,
    }
}

fn true_arcs() -> BTreeSet<(String, String)> {
    [
        ("caller", "callee"),
        ("helper", "util"),
        ("legacy_a", "legacy_b"),
    ]
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .into()
}

fn captured_arcs(text: &str, pattern: &str) -> BTreeSet<(String, String)> {
    regex::Regex::new(pattern)
        .unwrap()
        .captures_iter(text)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

fn html_arcs(path: &Path) -> BTreeSet<(String, String)> {
    captured_arcs(
        &fs::read_to_string(path).unwrap(),
        r#""from": "([^"]+)", "to": "([^"]+)""#,
    )
}

fn stored_arcs(graph: &Graph) -> BTreeSet<(String, String)> {
    graph
        .iter_edges()
        .map(|(u, v, e)| {
            let end = |marker: &str, i: usize| {
                e.attributes
                    .get(marker)
                    .unwrap_or(&graph.nodes()[i].id)
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            (end("_src", u), end("_tgt", v))
        })
        .collect()
}

fn loaded(stored: &Stored) -> Graph {
    graphicle::paths::load_node_link_graph(&json!(stored.graph.to_string_lossy())).unwrap()
}

#[test]
fn load_node_link_graph_stamps_stored_direction() {
    let stored = graph_json();
    assert_eq!(stored_arcs(&loaded(&stored)), true_arcs());
}

#[test]
fn export_html_draws_arrows_in_stored_direction() {
    let stored = graph_json();
    let graph = stored.graph.to_string_lossy().into_owned();
    let r = run(&["export", "html", "--graph", &graph]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    assert_eq!(
        html_arcs(&stored.root.join(".graphicle/graph.html")),
        true_arcs()
    );
}

#[test]
fn export_cypher_merges_edges_in_stored_direction() {
    let stored = graph_json();
    let graph = stored.graph.to_string_lossy().into_owned();
    let r = run(&["export", "neo4j", "--graph", &graph]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    let cypher = fs::read_to_string(stored.root.join(".graphicle/cypher.txt")).unwrap();
    assert_eq!(
        captured_arcs(
            &cypher,
            r"MATCH \(a \{id: '([^']+)'\}\), \(b \{id: '([^']+)'\}\)"
        ),
        true_arcs()
    );
}

fn plan_arcs(plan: &[graphicle::exporters::graphdb::Statement]) -> BTreeSet<(String, String)> {
    plan.iter()
        .filter(|s| s.params.get("src").is_some())
        .map(|s| {
            (
                s.params["src"].as_str().unwrap().to_string(),
                s.params["tgt"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn graphdb_push_plans_send_stored_direction() {
    use graphicle::exporters::graphdb::{falkordb_query_plan, query_plan};
    let stored = graph_json();
    let graph = loaded(&stored);
    let communities = vec![(0, graph.nodes().iter().map(|n| n.id.clone()).collect())];
    assert_eq!(
        plan_arcs(&query_plan(&graph, &communities).unwrap()),
        true_arcs()
    );
    assert_eq!(
        plan_arcs(&falkordb_query_plan(&graph, &communities).unwrap()),
        true_arcs()
    );
}

#[test]
fn export_obsidian_canvas_edges_in_stored_direction() {
    let stored = graph_json();
    let graph = stored.graph.to_string_lossy().into_owned();
    let vault = stored.root.join(".graphicle/obsidian");
    let vault_arg = vault.to_string_lossy().into_owned();
    let r = run(&["export", "obsidian", "--graph", &graph, "--dir", &vault_arg]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    let canvas = read(&vault.join("graph.canvas"));
    let arcs: BTreeSet<(String, String)> = canvas["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["fromNode"].as_str().unwrap()[2..].to_string(),
                e["toNode"].as_str().unwrap()[2..].to_string(),
            )
        })
        .collect();
    assert_eq!(arcs, true_arcs());
}

#[test]
fn watch_html_reconcile_draws_stored_direction() {
    let stored = graph_json();
    let out = stored.graph.parent().unwrap();
    let mut messages = Vec::new();
    let status = graphicle::watch::reconcile_html(out, &read(&stored.graph), &mut messages);
    assert_eq!(status.as_deref(), Some("rendered"), "{messages:?}");
    assert_eq!(html_arcs(&out.join("graph.html")), true_arcs());
}

#[test]
fn merge_driver_keeps_stored_direction_in_arc_order() {
    let stored = graph_json();
    let text = fs::read_to_string(&stored.graph).unwrap();
    let [base, cur, other] = ["base.json", "cur.json", "oth.json"].map(|n| stored.root.join(n));
    for path in [&base, &cur, &other] {
        fs::write(path, &text).unwrap();
    }
    let arg = |p: &Path| p.to_string_lossy().into_owned();
    let r = run(&["merge-driver", &arg(&base), &arg(&cur), &arg(&other)]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    let merged = read(&cur);
    let links = merged["links"].as_array().unwrap();
    let arcs: BTreeSet<(String, String)> = links
        .iter()
        .map(|l| {
            (
                l["source"].as_str().unwrap().to_string(),
                l["target"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(arcs, true_arcs());
    assert!(
        !links
            .iter()
            .any(|l| l.get("_src").is_some() || l.get("_tgt").is_some())
    );
}

// 79b7a84 (#3785): a subfolder run ignores other folders' manifest entries.
struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
    sub_a: PathBuf,
    sub_b: PathBuf,
    manifest: PathBuf,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().unwrap();
    // A non-hidden root: tempfile names its directories `.tmpXXXX`.
    let root = dir.path().canonicalize().unwrap().join("workspace");
    let (sub_a, sub_b) = (root.join("workflows"), root.join("experts"));
    fs::create_dir_all(&sub_a).unwrap();
    fs::create_dir_all(&sub_b).unwrap();
    fs::write(sub_a.join("workflow.py"), "def run(): pass\n").unwrap();
    fs::write(sub_b.join("expert.py"), "def consult(): pass\n").unwrap();
    Workspace {
        manifest: root.join(".graphicle/manifest.json"),
        _dir: dir,
        root,
        sub_a,
        sub_b,
    }
}

fn save(files: &Value, manifest: &Path, root: &Path) {
    let options = ManifestOptions {
        root: Some(root.to_path_buf()),
        ..Default::default()
    };
    save_manifest(files.as_object().unwrap(), manifest, "both", &options).unwrap();
}

fn detect(root: &Path) -> Value {
    Detector::default()
        .detect(root, &DetectOptions::default())
        .unwrap()
}

fn incremental(root: &Path, manifest: &Path) -> Value {
    Detector::default()
        .detect_incremental(root, manifest, "semantic", &DetectOptions::default())
        .unwrap()
}

#[test]
fn detect_incremental_subfolder_ignores_out_of_root_manifest_entries() {
    let w = workspace();
    save(&detect(&w.root)["files"], &w.manifest, &w.root);
    let inc_a = incremental(&w.sub_a, &w.manifest);
    assert_eq!(inc_a["deleted_files"], json!([]), "false deletions");
    assert_eq!(inc_a["excluded_files"], json!([]), "false exclusions");
}

#[test]
fn detect_incremental_cross_subfolder_shared_manifest() {
    let w = workspace();
    save(&detect(&w.sub_b)["files"], &w.manifest, &w.sub_b);
    let inc_a = incremental(&w.sub_a, &w.manifest);
    assert_eq!(inc_a["deleted_files"], json!([]));
    assert_eq!(inc_a["excluded_files"], json!([]));
    save(&inc_a["files"], &w.manifest, &w.sub_a);
    let inc_b = incremental(&w.sub_b, &w.manifest);
    assert_eq!(inc_b["deleted_files"], json!([]));
    assert_eq!(inc_b["excluded_files"], json!([]));
    assert_eq!(inc_b["new_files"]["code"], json!([]));
}

// af83bc0 (#4078): --previous restores metadata only for nodes still present.
fn write_with_communities(path: &Path, nodes: &[(&str, i64)]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let nodes: Vec<Value> = nodes
        .iter()
        .map(|(id, community)| json!({"id": id, "community": community}))
        .collect();
    let graph =
        json!({"directed": false, "multigraph": false, "graph": {}, "nodes": nodes, "links": []});
    fs::write(path, graph.to_string()).unwrap();
}

#[test]
fn merge_graphs_previous_does_not_resurrect_removed_node() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let a = root.join("alpha/.graphicle/graph.json");
    let b = root.join("beta/.graphicle/graph.json");
    write_with_communities(&a, &[("a0", 0)]);
    write_with_communities(&b, &[("b0", 0)]);
    let previous = root.join("previous-merged.json");
    write_with_communities(
        &previous,
        &[("alpha::a0", 7), ("beta::b0", 12), ("beta::removed", 99)],
    );
    let out = root.join("merged.json");
    let arg = |p: &Path| p.to_string_lossy().into_owned();
    let r = run(&[
        "merge-graphs",
        &arg(&a),
        &arg(&b),
        "--out",
        &arg(&out),
        "--previous",
        &arg(&previous),
    ]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    let data = read(&out);
    let by_id = |id: &str| {
        data["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .cloned()
    };
    assert!(by_id("beta::removed").is_none());
    assert_eq!(by_id("alpha::a0").unwrap()["community"], 7);
    assert_eq!(by_id("beta::b0").unwrap()["community"], 12);
}
