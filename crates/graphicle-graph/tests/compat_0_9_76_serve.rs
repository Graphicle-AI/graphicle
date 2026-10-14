// Parity tests for Graphify 0.9.76 path endpoint resolution (Apache-2.0; see NOTICE.md).
use serde_json::{Value, json};
use std::{fs, path::Path};

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn node_link(nodes: &[(&str, &str, &str, &str)], links: &[(&str, &str, &str)]) -> Value {
    json!({
        "directed": false, "multigraph": false, "graph": {},
        "nodes": nodes.iter().map(|(id, label, sf, loc)| json!({
            "id": id, "label": label, "source_file": sf, "source_location": loc,
        })).collect::<Vec<_>>(),
        "links": links.iter().map(|(a, b, rel)| json!({
            "source": a, "target": b, "relation": rel, "confidence": "EXTRACTED",
        })).collect::<Vec<_>>(),
    })
}

/// `.nonce()` defined on a class in each of two files, both reachable from run().
fn twin_method_graph() -> Value {
    node_link(
        &[
            ("src_main", "main.ts", "src/main.ts", "L1"),
            ("src_main_run", "run()", "src/main.ts", "L3"),
            ("src_a", "a.ts", "src/a.ts", "L1"),
            ("src_a_alphaclient", "AlphaClient", "src/a.ts", "L1"),
            ("src_a_alphaclient_nonce", ".nonce()", "src/a.ts", "L1"),
            ("src_b", "b.ts", "src/b.ts", "L1"),
            ("src_b_betaclient", "BetaClient", "src/b.ts", "L1"),
            ("src_b_betaclient_nonce", ".nonce()", "src/b.ts", "L1"),
        ],
        &[
            ("src_main", "src_main_run", "contains"),
            ("src_main_run", "src_a_alphaclient", "calls"),
            ("src_main_run", "src_b_betaclient", "calls"),
            ("src_a", "src_a_alphaclient", "contains"),
            ("src_b", "src_b_betaclient", "contains"),
            ("src_a_alphaclient", "src_a_alphaclient_nonce", "method"),
            ("src_b_betaclient", "src_b_betaclient_nonce", "method"),
        ],
    )
}

/// `.nonce()` on two classes of one file, both reachable from run().
fn same_file_twin_graph() -> Value {
    node_link(
        &[
            ("src_x", "x.ts", "src/x.ts", "L1"),
            ("src_x_run", "run()", "src/x.ts", "L3"),
            ("src_x_alphaclient", "AlphaClient", "src/x.ts", "L5"),
            ("src_x_alphaclient_nonce", ".nonce()", "src/x.ts", "L6"),
            ("src_x_betaclient", "BetaClient", "src/x.ts", "L9"),
            ("src_x_betaclient_nonce", ".nonce()", "src/x.ts", "L10"),
        ],
        &[
            ("src_x", "src_x_run", "contains"),
            ("src_x_run", "src_x_alphaclient", "calls"),
            ("src_x_run", "src_x_betaclient", "calls"),
            ("src_x_alphaclient", "src_x_alphaclient_nonce", "method"),
            ("src_x_betaclient", "src_x_betaclient_nonce", "method"),
        ],
    )
}

fn index_foo_graph() -> Value {
    node_link(
        &[
            ("a_index_foo", "foo()", "a/index.ts", "L3"),
            ("b_index_foo", "foo()", "b/index.ts", "L3"),
        ],
        &[],
    )
}

fn write(dir: &Path, graph: &Value) -> String {
    let gp = dir.join("graph.json");
    fs::write(&gp, graph.to_string()).unwrap();
    gp.to_str().unwrap().to_string()
}

fn cli(graph: &Value, args: &[&str]) -> graphicle::cli::Output {
    let dir = tempfile::tempdir().unwrap();
    let gp = write(dir.path(), graph);
    let mut argv = args.to_vec();
    argv.extend(["--graph", gp.as_str()]);
    run(&argv)
}

fn tool(graph: &Value, source: &str, target: &str) -> String {
    let mut graph = graph.clone();
    graph["directed"] = json!(true);
    let out = graphicle::dispatch(&json!({
        "operation": "serve",
        "action": "shortest_path_text",
        "graph": graph,
        "arguments": {"source": source, "target": target},
    }))
    .unwrap();
    out.as_str().unwrap().to_string()
}

const ROUTE_TO_BETA: &str =
    "run() --calls [EXTRACTED]--> BetaClient --method [EXTRACTED]--> .nonce()";

#[test]
fn path_refuses_ambiguous_endpoint() {
    // Graphify ea1f158 (#3913): a bare label naming symbols in two files is
    // refused with the candidate ids, on either end of the path; since 0.9.86
    // (#4320) with the lookup exit code for an ambiguous name.
    for ends in [["run()", ".nonce()"], [".nonce()", "run()"]] {
        let out = cli(&twin_method_graph(), &["path", ends[0], ends[1]]);
        assert_eq!(out.exit_code, 3, "{ends:?}: {}", out.stdout);
        assert!(!out.stdout.contains("Shortest path"), "{}", out.stdout);
        assert!(
            out.stderr
                .contains("Ambiguous: '.nonce()' matches 2 nodes in different files."),
            "{}",
            out.stderr
        );
        assert!(
            out.stderr.contains("src_a_alphaclient_nonce"),
            "{}",
            out.stderr
        );
        assert!(
            out.stderr.contains("src_b_betaclient_nonce"),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn path_endpoint_selects_named_node() {
    // The two retry forms `explain` suggests end the route at that method, not
    // at its file (the path half of `path::symbol`) or its containing class.
    for target in ["src/b.ts::.nonce()", "src_b_betaclient_nonce"] {
        let out = cli(&twin_method_graph(), &["path", "run()", target]);
        assert_eq!(out.exit_code, 0, "{target}: {}", out.stderr);
        assert!(
            out.stdout.contains("Shortest path (2 hops):"),
            "{}",
            out.stdout
        );
        assert!(out.stdout.contains(ROUTE_TO_BETA), "{}", out.stdout);
    }
}

#[test]
fn shortest_path_tool_refuses_ambiguous_endpoint() {
    let g = twin_method_graph();
    let expected = "Ambiguous: '.nonce()' matches 2 nodes in different files.";
    assert!(tool(&g, "run()", ".nonce()").starts_with(expected));
    assert!(tool(&g, ".nonce()", "run()").starts_with(expected));
    let out = tool(&g, "run()", "src/b.ts::.nonce()");
    assert!(out.contains("Shortest path (2 hops):"), "{out}");
    assert!(
        out.contains("BetaClient --method [EXTRACTED]--> .nonce()"),
        "{out}"
    );
}

#[test]
fn path_refuses_same_file_duplicate_endpoint() {
    // Two same-named symbols of one file were split by graph order behind a
    // score-tie warning; only the node id can tell them apart.
    for target in [".nonce()", "src/x.ts::.nonce()"] {
        let out = cli(&same_file_twin_graph(), &["path", "run()", target]);
        assert_eq!(out.exit_code, 3, "{target}: {}", out.stdout);
        assert!(!out.stdout.contains("Shortest path"), "{}", out.stdout);
        let header = format!("Ambiguous: '{target}' matches 2 nodes in src/x.ts.");
        assert!(out.stderr.contains(&header), "{}", out.stderr);
        assert!(
            out.stderr.contains("  L6\n    id: src_x_alphaclient_nonce"),
            "{}",
            out.stderr
        );
        assert!(
            out.stderr.contains("  L10\n    id: src_x_betaclient_nonce"),
            "{}",
            out.stderr
        );
        assert!(
            out.stderr.contains("Retry with the full node id."),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn path_same_file_endpoints_still_resolve() {
    // The node id picks one twin, and a file-path query (file node plus its
    // members, one file) stays a file lookup rather than an ambiguity.
    let g = same_file_twin_graph();
    let out = cli(&g, &["path", "run()", "src_x_betaclient_nonce"]);
    assert!(
        out.stdout.contains(ROUTE_TO_BETA),
        "{}{}",
        out.stdout,
        out.stderr
    );
    let out = cli(&g, &["path", "src/x.ts", "src_x_betaclient_nonce"]);
    assert!(
        out.stdout.contains("Shortest path (3 hops):"),
        "{}{}",
        out.stdout,
        out.stderr
    );
}

#[test]
fn ambiguity_hint_names_symbol_not_query() {
    // `index.ts::foo()` matches two files; the hint shows `<path>::foo()`, not
    // the query nested behind another path.
    let g = index_foo_graph();
    let out = tool(&g, "index.ts::foo()", "a_index_foo");
    assert!(out.contains("(e.g. <path>::foo())"), "{out}");
    assert!(!out.contains("<path>::index.ts::"), "{out}");
    let out = cli(&g, &["explain", "index.ts::foo()"]);
    assert_eq!(out.exit_code, 3, "{}", out.stderr);
    assert!(
        out.stdout.contains("(e.g. <path>::foo())"),
        "{}",
        out.stdout
    );
    assert!(!out.stdout.contains("<path>::index.ts::"), "{}", out.stdout);
}

#[test]
fn get_node_hint_names_symbol_not_query() {
    // get_node / get_neighbors share the message (`_resolve_single_node`).
    let mut g = index_foo_graph();
    g["directed"] = json!(true);
    let out = graphicle::dispatch(&json!({
        "operation": "serve",
        "action": "resolve",
        "graph": g,
        "label": "index.ts::foo()",
    }))
    .unwrap();
    let text = out.to_string();
    assert!(text.contains("(e.g. <path>::foo())"), "{text}");
    assert!(!text.contains("<path>::index.ts::"), "{text}");
}

#[test]
fn exact_endpoints_skip_score_tie_warnings() {
    // An exact hit is used as-is, so no score-tie warning is attached to it.
    let out = cli(
        &same_file_twin_graph(),
        &["path", "run()", "src_x_alphaclient_nonce"],
    );
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    assert!(!out.stderr.contains("warning:"), "{}", out.stderr);
    assert!(
        out.stdout
            .contains("run() --calls [EXTRACTED]--> AlphaClient --method [EXTRACTED]--> .nonce()"),
        "{}",
        out.stdout
    );
}
