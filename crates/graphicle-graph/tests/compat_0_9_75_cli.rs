// Parity tests for Graphify 0.9.75 CLI and cache fixes (Apache-2.0; see NOTICE.md).
use serde_json::{Value, json};
use std::{fs, path::Path};

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn write_graph(path: &Path, ids: &[&str]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let nodes = ids
        .iter()
        .enumerate()
        .map(|(i, id)| json!({"id": id, "label": id, "community": i, "source_file": "x.py"}))
        .collect::<Vec<_>>();
    let graph =
        json!({"directed": false, "multigraph": false, "graph": {}, "nodes": nodes, "links": []});
    fs::write(path, serde_json::to_string(&graph).unwrap()).unwrap();
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

struct Repos {
    _dir: tempfile::TempDir,
    a: String,
    b: String,
    out: String,
    root: std::path::PathBuf,
}

fn repos() -> Repos {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let a = root.join("alpha/.graphicle/graph.json");
    let b = root.join("beta/.graphicle/graph.json");
    write_graph(&a, &["one", "two"]);
    write_graph(&b, &["three"]);
    Repos {
        a: a.to_string_lossy().into_owned(),
        b: b.to_string_lossy().into_owned(),
        out: root.join("merged.json").to_string_lossy().into_owned(),
        root,
        _dir: dir,
    }
}

#[test]
fn merge_graphs_previous_restores_the_merged_community_by_node_id() {
    // Graphify 9ec7df0 (#3858).
    let r = repos();
    let first = run(&["merge-graphs", &r.a, &r.b, "--out", &r.out]);
    assert_eq!(first.exit_code, 0, "{}", first.stderr);
    let merged = read(Path::new(&r.out));
    let ids = merged["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 3);
    // The previous merged clustering knows two of the three surviving nodes.
    let previous = r.root.join("previous.json");
    let prev_nodes = json!([
        {"id": ids[0], "community": 41},
        {"id": ids[2], "community": 7},
        {"id": "gone::node", "community": 3},
        {"id": ids[1], "community": "not-an-int"},
    ]);
    fs::write(&previous, json!({"nodes": prev_nodes}).to_string()).unwrap();
    let out = run(&[
        "merge-graphs",
        &r.a,
        &r.b,
        "--out",
        &r.out,
        "--previous",
        previous.to_str().unwrap(),
    ]);
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    assert!(
        out.stdout
            .contains("restored the previous merged community on 2 node(s)"),
        "{}",
        out.stdout
    );
    let merged = read(Path::new(&r.out));
    let community = |id: &str| {
        merged["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .unwrap()
            .get("community")
            .cloned()
    };
    assert_eq!(community(&ids[0]), Some(json!(41)));
    assert_eq!(community(&ids[2]), Some(json!(7)));
    assert_eq!(community(&ids[1]), None);
}

#[test]
fn merge_graphs_previous_missing_file_errors() {
    let r = repos();
    let missing = r.root.join("nope.json");
    let out = run(&[
        "merge-graphs",
        &r.a,
        &r.b,
        "--out",
        &r.out,
        "--previous",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(out.exit_code, 1);
    assert!(
        out.stderr.contains("--previous file not found"),
        "{}",
        out.stderr
    );
}

#[test]
fn merge_graphs_previous_malformed_json_errors_gracefully() {
    // Graphify b523898: a clear message and exit 1, never a traceback.
    let r = repos();
    let previous = r.root.join("previous.json");
    fs::write(&previous, "{ not valid json ,,, ").unwrap();
    let out = run(&[
        "merge-graphs",
        &r.a,
        &r.b,
        "--out",
        &r.out,
        "--previous",
        previous.to_str().unwrap(),
    ]);
    assert_eq!(out.exit_code, 1);
    assert!(
        out.stderr.contains("--previous file is not readable JSON"),
        "{}",
        out.stderr
    );
    assert!(!Path::new(&r.out).exists() || read(Path::new(&r.out))["nodes"].is_array());
}

fn path_text(nodes: &[&str], links: Value, source: &str, target: &str) -> String {
    let nodes = nodes
        .iter()
        .map(|n| json!({"id": n, "label": n}))
        .collect::<Vec<_>>();
    let graph =
        json!({"directed": true, "multigraph": false, "graph": {}, "nodes": nodes, "links": links});
    let out = graphicle::dispatch(&json!({
        "operation": "serve",
        "action": "shortest_path_text",
        "graph": graph,
        "arguments": {"source": source, "target": target},
    }))
    .unwrap();
    out.as_str().unwrap().to_string()
}

#[test]
fn path_routes_through_a_contains_edge_to_reach_the_file() {
    // Graphify e61c5fa (#3878): a.py imports a symbol b.py contains.
    let out = path_text(
        &["a.py", "b.py", "helper()"],
        json!([
            {"source": "a.py", "target": "helper()", "relation": "imports", "confidence": "EXTRACTED"},
            {"source": "b.py", "target": "helper()", "relation": "contains", "confidence": "EXTRACTED"},
        ]),
        "a.py",
        "b.py",
    );
    assert!(out.contains("Shortest path (2 hops):"), "{out}");
    assert!(
        out.contains("a.py --imports [EXTRACTED]--> helper() <--contains [EXTRACTED]-- b.py"),
        "{out}"
    );
    assert!(!out.contains("No directed path found"), "{out}");
}

#[test]
fn shortest_path_tool_routes_through_a_contains_edge() {
    // Graphify e47d633 (serve mirror).
    let out = path_text(
        &["a.py", "b.py", "helper_a", "helper_b"],
        json!([
            {"source": "a.py", "target": "helper_a", "relation": "contains"},
            {"source": "b.py", "target": "helper_b", "relation": "contains"},
            {"source": "helper_a", "target": "helper_b", "relation": "calls"},
        ]),
        "helper_a",
        "b.py",
    );
    assert!(!out.contains("No directed path"), "{out}");
    assert!(out.contains("Shortest path (2 hops)"), "{out}");
    assert!(out.contains("--calls-->"), "{out}");
    assert!(out.contains("<--contains--"), "{out}");
}

#[test]
fn contains_is_still_not_walked_backwards_for_other_relations() {
    // Only `contains` gains an implied reverse hop.
    let out = path_text(
        &["a.py", "b.py"],
        json!([{"source": "b.py", "target": "a.py", "relation": "imports"}]),
        "a.py",
        "b.py",
    );
    assert!(out.contains("No directed path found"), "{out}");
}

fn cluster_only(graph: Value) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let out = root.join(".graphicle");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("graph.json"), graph.to_string()).unwrap();
    let r = run(&[
        "cluster-only",
        root.to_str().unwrap(),
        "--no-viz",
        "--no-label",
    ]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    read(&out.join("graph.json"))
}

fn link_triples(data: &Value) -> Vec<(String, String, String)> {
    let mut pairs = data["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let s = |k: &str| l[k].as_str().unwrap().to_string();
            (s("source"), s("target"), s("relation"))
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
}

fn triples(items: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
    items
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect()
}

#[test]
fn cluster_only_preserves_parallel_edges() {
    // Graphify 7527e73 (#3999).
    let code =
        |id: &str| json!({"id": id, "label": id, "file_type": "code", "source_file": "a.py"});
    let data = cluster_only(json!({
        "directed": false, "multigraph": false, "graph": {},
        "nodes": [code("a"), code("b"), code("c")],
        "links": [
            {"source": "a", "target": "b", "relation": "imports", "confidence": "EXTRACTED"},
            {"source": "a", "target": "b", "relation": "calls", "confidence": "EXTRACTED"},
            {"source": "b", "target": "c", "relation": "calls", "confidence": "EXTRACTED"},
        ],
    }));
    assert_eq!(
        link_triples(&data),
        triples(&[
            ("a", "b", "calls"),
            ("a", "b", "imports"),
            ("b", "c", "calls")
        ])
    );
}

#[test]
fn to_json_original_links_drops_only_a_dangling_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.json");
    let link = |t: &str, r: &str| json!({"source": "a", "target": t, "relation": r, "confidence": "EXTRACTED"});
    let code =
        |id: &str| json!({"id": id, "label": id, "file_type": "code", "source_file": "a.py"});
    let mut extraction = json!({
        "directed": false,
        "nodes": [code("a"), code("b")],
        "links": [link("b", "imports"), link("b", "calls")],
    });
    let mut d = graphicle::build::Diagnostics::default();
    let g = graphicle::build::build_from_json(&mut extraction, false, None, &mut d).unwrap();
    let communities = graphicle::cluster::cluster(&g, 1.0, None, "auto").unwrap();
    let options = graphicle::export::JsonOptions {
        original_links: Some(vec![
            link("b", "imports"),
            link("b", "calls"),
            link("gone", "calls"),
        ]),
        ..Default::default()
    };
    assert!(graphicle::export::to_json(&g, &communities, &out, &options, &mut d).unwrap());
    let data = read(&out);
    assert_eq!(
        link_triples(&data),
        triples(&[("a", "b", "calls"), ("a", "b", "imports")])
    );
    assert!(data["links"][0]["confidence_score"].is_number());
}

#[test]
fn cluster_only_preserves_parallel_edges_across_an_id_remap() {
    // Graphify e3ac168, as corrected by b523898: both endpoints remap
    // (claude_alpha -> claude_claude_alpha, beta -> pkg_beta) and the written
    // links track them.
    let data = cluster_only(json!({
        "directed": false, "multigraph": false, "graph": {},
        "nodes": [
            {"id": "claude_alpha", "label": "alpha", "file_type": "concept",
             "source_file": ".claude/CLAUDE.md", "_origin": "semantic"},
            {"id": "beta", "label": "beta", "file_type": "code",
             "source_file": "pkg/beta.py", "_origin": "semantic"},
        ],
        "links": [
            {"source": "claude_alpha", "target": "beta", "relation": "references", "confidence": "EXTRACTED"},
            {"source": "claude_alpha", "target": "beta", "relation": "mentions", "confidence": "EXTRACTED"},
        ],
    }));
    let ids = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"claude_claude_alpha".to_string()), "{ids:?}");
    assert!(!ids.contains(&"claude_alpha".to_string()));
    assert_eq!(
        link_triples(&data),
        triples(&[
            ("claude_claude_alpha", "pkg_beta", "mentions"),
            ("claude_claude_alpha", "pkg_beta", "references"),
        ])
    );
}

// Graphify 0.9.75 (daee93f, e8b5d27): a complete extract force-writes, so a
// graph that dedup made smaller is refused unless --allow-dedup-shrink; a
// deleted file still writes, and the counters never reach graph.json (#3774).
// Since 0.9.87 (#2843) semantic results are stamped `_origin: semantic` before
// the merge, so the notes carry the stamp a saved graph has (as upstream's
// fixture does).
fn note(id: &str, label: &str, source: &str, line: &str) -> Value {
    json!({"id": id, "label": label, "file_type": "document", "source_file": source, "source_location": line, "_origin": "semantic"})
}

fn decision_notes(readme: &str) -> Vec<Value> {
    vec![
        note("readme_decisions_a", "Decisions", readme, "L1"),
        note("readme_decisions_b", "Decisions", readme, "L2"),
        note("readme_overview", "Overview", readme, "L3"),
    ]
}

struct ShrinkCorpus {
    _dir: tempfile::TempDir,
    corpus: std::path::PathBuf,
    out: std::path::PathBuf,
    graph: std::path::PathBuf,
}

/// A corpus whose README semantic result is already cached (so the extract
/// never reaches the backend) beside a saved graph.json. `nodes` receives the
/// README path and the corpus and returns `(cached README nodes, saved nodes)`.
fn shrink_corpus(nodes: impl Fn(&str, &Path) -> (Vec<Value>, Vec<Value>)) -> ShrinkCorpus {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let corpus = root.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    let readme = corpus.join("README.md");
    fs::write(&readme, "# Notes\nThe entry point overview.\n").unwrap();
    let (cached, saved) = nodes(&readme.to_string_lossy(), &corpus);
    let out = root.join("out");
    let graph = out.join(".graphicle").join("graph.json");
    fs::create_dir_all(graph.parent().unwrap()).unwrap();
    fs::write(&graph, json!({"nodes": saved, "links": []}).to_string()).unwrap();
    let options = graphicle::cache::SemanticOptions {
        root: corpus.clone(),
        cache_root: Some(out.clone()),
        prompt: Some(graphicle::cache::Prompt::Text(
            graphicle::llm::extraction_system(false),
        )),
        ..Default::default()
    };
    let entries = graphicle::cache::ExtractionCache::default()
        .save_semantic(&cached, &[], &[], &options)
        .unwrap();
    assert_eq!(entries, 1);
    ShrinkCorpus {
        _dir: dir,
        corpus,
        out,
        graph,
    }
}

fn shrink_extract(c: &ShrinkCorpus, extra: &[&str]) -> graphicle::cli::Output {
    let corpus = c.corpus.to_string_lossy().into_owned();
    let out = c.out.to_string_lossy().into_owned();
    let mut args = vec!["extract", &corpus, "--backend", "ollama", "--out", &out];
    args.extend_from_slice(extra);
    run(&args)
}

fn node_ids(path: &Path) -> Vec<String> {
    let mut ids = read(path)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

#[test]
fn extract_refuses_a_same_file_dedup_shrink() {
    let c = shrink_corpus(|readme, _| (decision_notes(readme), decision_notes(readme)));
    let before = fs::read_to_string(&c.graph).unwrap();
    let output = shrink_extract(&c, &[]);
    assert_eq!(output.exit_code, 1, "{}", output.stderr);
    assert_eq!(fs::read_to_string(&c.graph).unwrap(), before);
    let refusal = format!(
        "[graphicle extract] error: dedup merged 1 node(s); writing would shrink the graph from 3 nodes to 2 nodes. Refusing to overwrite {}. Pass --allow-dedup-shrink to write the smaller graph.",
        c.graph.display()
    );
    assert!(output.stderr.contains(&refusal), "{}", output.stderr);
}

#[test]
fn allow_dedup_shrink_writes_the_smaller_graph_without_the_counters() {
    let c = shrink_corpus(|readme, _| (decision_notes(readme), decision_notes(readme)));
    let output = shrink_extract(&c, &["--allow-dedup-shrink"]);
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    let ids = node_ids(&c.graph);
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&"readme_overview".to_string()));
    let raw = fs::read_to_string(&c.graph).unwrap();
    assert!(!raw.contains("_dedup_collapsed") && !raw.contains("_pruned_node_count"));
}

#[test]
fn no_dedup_never_meets_the_dedup_shrink_refusal() {
    // Without dedup nothing is deduplicated, so the write proceeds; since
    // 0.9.77 (3f712fe) the same-label notes also keep their own nodes.
    let c = shrink_corpus(|readme, _| (decision_notes(readme), decision_notes(readme)));
    let output = shrink_extract(&c, &["--no-dedup"]);
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(!output.stderr.contains("--allow-dedup-shrink"));
    assert_eq!(
        node_ids(&c.graph),
        [
            "readme_decisions_a",
            "readme_decisions_b",
            "readme_overview"
        ]
    );
}

#[test]
fn a_deleted_file_still_writes() {
    let c = shrink_corpus(|readme, corpus| {
        let gone = corpus.join("gone.md").to_string_lossy().into_owned();
        let overview = note("readme_overview", "Overview", readme, "L1");
        let saved = vec![
            overview.clone(),
            note("gone_notes", "Gone notes", &gone, "L1"),
        ];
        (vec![overview], saved)
    });
    let output = shrink_extract(&c, &[]);
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(node_ids(&c.graph), ["readme_overview"]);
}

#[test]
fn a_deletion_excuses_only_its_own_share_of_a_dedup_shrink() {
    // Saved: one merged decision, the overview and a note from a deleted file.
    // Dedup merges the fresh pair back into one (collapsed 1) and the prune
    // removes the saved note, so 3 -> 2 is exactly the deletion: it writes.
    let c = shrink_corpus(|readme, corpus| {
        let gone = corpus.join("gone.md").to_string_lossy().into_owned();
        let saved = vec![
            note("readme_decisions_a", "Decisions", readme, "L1"),
            note("readme_overview", "Overview", readme, "L3"),
            note("gone_notes", "Gone notes", &gone, "L1"),
        ];
        (decision_notes(readme), saved)
    });
    let output = shrink_extract(&c, &[]);
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(node_ids(&c.graph).len(), 2);
}

#[test]
fn build_records_the_dedup_collapse_and_to_json_never_stores_it() {
    let dir = tempfile::tempdir().unwrap();
    let readme = dir.path().join("README.md").to_string_lossy().into_owned();
    let extraction = json!({"nodes": decision_notes(&readme), "edges": [], "hyperedges": []});
    let mut d = graphicle::build::Diagnostics::default();
    let mut g = graphicle::build::build(&[extraction], &Default::default(), &mut d, None).unwrap();
    assert_eq!(g.attributes()["_dedup_collapsed"], json!(1));
    let out = dir.path().join("graph.json");
    let options = graphicle::export::JsonOptions {
        force: true,
        ..Default::default()
    };
    let communities: graphicle::cluster::Communities = vec![];
    assert!(graphicle::export::to_json(&g, &communities, &out, &options, &mut d).unwrap());
    let raw = fs::read_to_string(&out).unwrap();
    assert!(!raw.contains("_dedup_collapsed") && !raw.contains("_pruned_node_count"));
    assert_eq!(graphicle::build::take_shrink_accounting(&mut g), (1, 0));
    assert!(g.attributes().get("_dedup_collapsed").is_none());
    assert_eq!(graphicle::build::take_shrink_accounting(&mut g), (0, 0));
}

#[test]
fn pruned_node_count_ignores_a_stub_created_this_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let doc = |id: &str, label: &str, source: &str| json!({"id": id, "label": label, "file_type": "document", "source_file": source});
    let graph_path = root.join(".graphicle").join("graph.json");
    fs::create_dir_all(graph_path.parent().unwrap()).unwrap();
    let disk = json!([
        doc("kept", "Overview", "README.md"),
        doc("gone", "Gone", "gone.md")
    ]);
    fs::write(&graph_path, json!({"nodes": disk, "links": []}).to_string()).unwrap();
    let chunk = json!({
        "nodes": [doc("kept", "Overview", "README.md"), {"id": "fresh_stub", "label": "Path", "file_type": "code"}],
        "edges": [{"source": "gone", "target": "fresh_stub", "source_file": "gone.md"}],
        "hyperedges": [],
    });
    let options = graphicle::build::MergeOptions {
        graph_path: Some(graph_path),
        prune_sources: vec!["gone.md".into()],
        dedup: false,
        root: Some(root),
        ..Default::default()
    };
    let mut d = graphicle::build::Diagnostics::default();
    let g = graphicle::build::build_merge(&[chunk], &options, &mut d, None).unwrap();
    let ids = g
        .nodes()
        .iter()
        .map(|n| n.id.to_string())
        .collect::<Vec<_>>();
    assert!(
        !ids.iter()
            .any(|id| id.contains("gone") || id.contains("fresh_stub")),
        "{ids:?}"
    );
    assert_eq!(g.attributes()["_pruned_node_count"], json!(1));
}
