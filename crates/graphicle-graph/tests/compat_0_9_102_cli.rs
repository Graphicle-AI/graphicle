// Adapted from Graphify 0.9.102 (v0.9.102), Apache-2.0; see NOTICE.md.
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
