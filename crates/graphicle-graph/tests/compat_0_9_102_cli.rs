// Adapted from Graphify 0.9.102 (59b5ab11), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_prs.py, tests/test_cli_export.py,
// tests/test_hyperedge_member_shapes.py, tests/test_dedup.py,
// tests/test_query_cli.py, tests/test_explain_cli.py, tests/test_cli_help.py,
// tests/test_install.py, tests/test_serve.py
//! CLI, query, install, export, build and dedup changes ported from
//! Graphify 0.9.96-0.9.102 (port part b). Cases follow the upstream tests
//! named in each comment.
use serde_json::{Value, json};

#[test]
fn unfinished_check_run_statuses_are_pending() {
    // test_prs.py (Graphify 0.9.96): every CheckStatusState but COMPLETED is
    // pending, alone or beside a passing sibling.
    for status in ["REQUESTED", "QUEUED", "IN_PROGRESS", "WAITING", "PENDING"] {
        let rollup = [json!({"__typename": "CheckRun", "name": "deploy", "status": status, "conclusion": ""})];
        assert_eq!(graphicle::prs::parse_ci(&rollup), "PENDING", "{status}");
    }
    for status in ["REQUESTED", "WAITING", "PENDING"] {
        let rollup = [
            json!({"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS"}),
            json!({"__typename": "CheckRun", "name": "deploy", "status": status, "conclusion": ""}),
        ];
        assert_eq!(graphicle::prs::parse_ci(&rollup), "PENDING", "{status}");
    }
}

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

#[test]
fn obsidian_note_lists_every_parallel_relation() {
    // test_cli_export.py (Graphify 0.9.96): a pair joined by both `imports`
    // and `calls` keeps both bullets, sorted.
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    std::fs::write(
        &graph,
        json!({
            "directed": false, "multigraph": true, "graph": {},
            "nodes": [
                {"id": "a.py", "label": "a.py", "file_type": "code", "community": 0},
                {"id": "b.py", "label": "b.py", "file_type": "code", "community": 0}
            ],
            "links": [
                {"source": "a.py", "target": "b.py", "relation": "imports", "confidence": "EXTRACTED"},
                {"source": "a.py", "target": "b.py", "relation": "calls", "confidence": "INFERRED"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let vault = dir.path().join("vault");
    let r = run(&[
        "export",
        "obsidian",
        "--graph",
        graph.to_str().unwrap(),
        "--dir",
        vault.to_str().unwrap(),
    ]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    let note = std::fs::read_to_string(vault.join("a.py.md")).unwrap();
    let connections: Vec<&str> = note.lines().filter(|l| l.starts_with("- [[")).collect();
    assert_eq!(
        connections,
        ["- [[b.py]] - `calls` [INFERRED]", "- [[b.py]] - `imports` [EXTRACTED]"]
    );
}

#[test]
fn numeric_hyperedge_members_are_coerced_under_every_alias() {
    // test_hyperedge_member_shapes.py (Graphify 0.9.96): `members` and
    // `node_ids` coerce numeric ids like `nodes`, so the hyperedge survives.
    for alias in ["nodes", "members", "node_ids"] {
        let mut extraction = json!({
            "nodes": [
                {"id": 1, "label": "Alpha", "file_type": "concept", "source_file": "a.md"},
                {"id": 2, "label": "Beta", "file_type": "concept", "source_file": "a.md"},
                {"id": 3, "label": "Gamma", "file_type": "concept", "source_file": "a.md"}
            ],
            "edges": [],
            "hyperedges": [{"id": "h_num", "relation": "collab", alias: [1, 2, 3], "source_file": "a.md"}]
        });
        let mut d = graphicle::build::Diagnostics::default();
        let g = graphicle::build::build_from_json_dedup(&mut extraction, true, false, false, None, &mut d)
            .unwrap();
        let hyperedges = &g.attributes()["hyperedges"];
        assert_eq!(hyperedges.as_array().map(Vec::len), Some(1), "{alias}: {hyperedges}");
        assert_eq!(hyperedges[0]["id"], "h_num", "{alias}");
        assert_eq!(hyperedges[0]["nodes"], json!(["1", "2", "3"]), "{alias}");
    }
}

fn scanned(root: &std::path::Path) -> Vec<String> {
    let d = graphicle::detect::Detector::default()
        .detect(root, &graphicle::detect::DetectOptions::default())
        .unwrap();
    d["files"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| v.as_array().unwrap().iter())
        .map(|f| f.as_str().unwrap().replace(\, "/"))
        .collect()
}

fn write(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn msbuild_obj_and_bin_are_pruned_only_on_msbuild_evidence() {
    // test_msbuild_dir_evidence.py (Graphify 0.9.96).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("App.csproj"), "<Project Sdk=\"Microsoft.NET.Sdk.Web\" />\n");
    write(&root.join("Program.cs"), "public class Program { }\n");
    write(&root.join("appsettings.json"), "{}\n");
    write(&root.join("obj/project.assets.json"), "{}\n");
    write(&root.join("obj/Debug/net8.0/App.GlobalUsings.g.cs"), "global using global::System;\n");
    write(&root.join("bin/Release/net8.0/linux-x64/App.runtimeconfig.json"), "{}\n");
    write(&root.join("bin/Release/net8.0/linux-x64/appsettings.json"), "{}\n");
    let files = scanned(root);
    assert!(files.iter().any(|f| f.ends_with("/Program.cs")), "{files:?}");
    assert!(!files.iter().any(|f| f.contains("/obj/") || f.contains("/bin/")), "{files:?}");
    // A Rust src/bin/ with no MSBuild artifact stays source.
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("src/bin/tool.rs"), "fn main() {}\n");
    let files = scanned(dir.path());
    assert!(files.iter().any(|f| f.ends_with("src/bin/tool.rs")), "{files:?}");
}

#[test]
fn query_terms_drop_get_question_filler() {
    // test_serve.py (Graphify 0.9.97, #4684).
    use graphicle::query_terms::query_terms;
    assert_eq!(query_terms("how does payroll.approve get authorized"), ["payroll", "approve", "authorized"]);
    for verb in ["gets", "got", "getting"] {
        assert_eq!(query_terms(&format!("payroll {verb} authorized")), ["payroll", "authorized"]);
    }
    assert_eq!(query_terms("get"), ["get"]);
    assert_eq!(query_terms("how do I get it"), ["how", "get"]);
}

/// A repository `app` with a linked worktree `app-feature` beside it, laid
/// out as `git worktree add` leaves them (no git process needed).
fn workspace_with_worktree(ws: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let app = ws.join("app");
    let wt = ws.join("app-feature");
    write(&app.join("src/a.py"), "x = 1\n");
    write(&app.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&app.join(".git/worktrees/app-feature/commondir"), "../..\n");
    write(&wt.join("src/a.py"), "x = 1\n");
    write(
        &wt.join(".git"),
        &format!("gitdir: {}\n", app.join(".git/worktrees/app-feature").display()),
    );
    (app, wt)
}

#[test]
fn sibling_linked_worktree_is_pruned() {
    // test_detect.py (Graphify 0.9.97, #4683): a second copy of a scanned
    // repository is pruned; scanning the worktree itself still indexes it.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let (_app, wt) = workspace_with_worktree(&ws);
    let files = scanned(&ws);
    assert!(files.iter().any(|f| f.ends_with("app/src/a.py")), "{files:?}");
    assert!(!files.iter().any(|f| f.contains("app-feature/")), "{files:?}");
    let files = scanned(&wt);
    assert!(files.iter().any(|f| f.ends_with("src/a.py")), "{files:?}");
    // A worktree whose main checkout is outside the root stays indexed.
    let files = scanned(&wt.parent().unwrap().join("app-feature"));
    assert!(!files.is_empty());
}

#[test]
fn lone_help_after_query_or_explain_prints_usage() {
    // test_cli_help.py (Graphify 0.9.97, #4830).
    for command in ["query", "explain"] {
        for flag in ["--help", "-h"] {
            let r = run(&[command, flag]);
            assert_eq!(r.exit_code, 0, "{}", r.stderr);
            assert!(r.stdout.contains(&format!("Usage: ")) && r.stdout.contains(command), "{}", r.stdout);
            assert!(!r.stdout.contains("No node matching"), "{}", r.stdout);
        }
    }
}

fn validators_graph(dir: &std::path::Path) -> String {
    let graph = dir.join("graph.json");
    std::fs::write(
        &graph,
        json!({
            "directed": false, "multigraph": false, "graph": {},
            "nodes": [
                {"id": "src", "label": "validate_username()", "source_file": "pkg/auth/validators.py", "source_location": "L10"},
                {"id": "tst", "label": "validate_username()", "source_file": "tests/auth/test_validators.py", "source_location": "L5"},
                {"id": "doc", "label": "validate_username", "source_file": "docs/auth.md", "source_location": "L1"},
                {"id": "cfg", "label": "username", "source_file": "pkg/settings.yml", "source_location": "L1"},
                {"id": "ext", "label": "username"}
            ],
            "links": [{"source": "tst", "target": "src", "relation": "calls", "confidence": "EXTRACTED"}]
        })
        .to_string(),
    )
    .unwrap();
    graph.to_string_lossy().into_owned()
}

fn node_sources(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with("NODE "))
        .map(|l| {
            l.split(" [src=")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn query_answer_leads_with_source_not_test() {
    // test_query_source_first.py (Graphify 0.9.102): tests, docs, config
    // and source-less nodes weigh less unless the question asks for them.
    let dir = tempfile::tempdir().unwrap();
    let graph = validators_graph(dir.path());
    let r = run(&[
        "query",
        "validate_username allows a trailing newline",
        "--graph",
        &graph,
        "--depth",
        "1",
    ]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    assert_eq!(
        node_sources(&r.stdout)[0],
        "pkg/auth/validators.py",
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("depth=1"), "{}", r.stdout);
}

#[test]
fn query_depth_and_budget_come_from_options_then_project_settings() {
    // test_query_cli.py (Graphify 0.9.102): --depth 1-6, the project
    // .graphiclerc query_depth/query_budget, then 2 and 2000.
    let q = "how does validate_username work";
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(".graphicle");
    std::fs::create_dir_all(&out).unwrap();
    let graph = validators_graph(&out);
    let r = run(&["query", q, "--graph", &graph]);
    assert!(r.stdout.contains("depth=2"), "{}", r.stdout);
    std::fs::write(
        dir.path().join(".graphiclerc"),
        "query_depth=3\nquery_budget=50\n",
    )
    .unwrap();
    let r = run(&["query", q, "--graph", &graph]);
    assert!(r.stdout.contains("depth=3"), "{}", r.stdout);
    let r = run(&["query", q, "--graph", &graph, "--depth=4"]);
    assert!(r.stdout.contains("depth=4"), "{}", r.stdout);
    for bad in ["0", "7", "x"] {
        let r = run(&["query", q, "--graph", &graph, "--depth", bad]);
        assert_ne!(r.exit_code, 0, "{bad}");
        assert!(
            r.stderr.contains("--depth must be an integer from 1 to 6"),
            "{}",
            r.stderr
        );
    }
    std::fs::write(dir.path().join(".graphiclerc"), "query_depth=9\n").unwrap();
    let r = run(&["query", q, "--graph", &graph]);
    assert!(r.stderr.contains("ignoring project settings"), "{}", r.stderr);
    assert!(r.stdout.contains("depth=2"), "{}", r.stdout);
}

#[test]
fn path_reports_its_weakest_hop_and_scored_tags() {
    // test_path_confidence.py and test_confidence_score_output.py (Graphify
    // 0.9.98-0.9.99, #4885, #4882): a path is as sure as its weakest hop, and
    // a scored non-EXTRACTED tag prints its score.
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    std::fs::write(
        &graph,
        json!({
            "directed": true, "multigraph": false, "graph": {},
            "nodes": [
                {"id": "alpha", "label": "alpha()", "source_file": "a.py"},
                {"id": "beta", "label": "beta()", "source_file": "b.py"},
                {"id": "gamma", "label": "gamma()", "source_file": "c.py"}
            ],
            "links": [
                {"source": "alpha", "target": "beta", "relation": "calls", "confidence": "INFERRED", "confidence_score": 0.6},
                {"source": "beta", "target": "gamma", "relation": "calls", "confidence": "EXTRACTED", "confidence_score": 1.0}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let r = run(&["path", "alpha()", "gamma()", "--graph", graph.to_str().unwrap()]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("--calls [INFERRED 0.60]--> beta()"), "{}", r.stdout);
    assert!(r.stdout.contains("--calls [EXTRACTED]--> gamma()"), "{}", r.stdout);
    assert!(
        r.stdout
            .trim_end()
            .ends_with("Path confidence: INFERRED (weakest hop 1: calls, score 0.6)"),
        "{}",
        r.stdout
    );
}
