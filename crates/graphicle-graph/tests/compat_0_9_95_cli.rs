// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_install_references.py, tests/test_atomic_version_stamp.py,
// tests/test_cli_help.py, tests/test_hooks.py, tests/test_lookup_exit_codes.py,
// tests/test_serve.py, tests/test_backend_opt_in.py, tests/test_binary_media_words.py,
// tests/test_tree_html.py, tests/test_alias_folding.py,
// tests/test_no_cluster_undeclared_endpoints.py, tests/test_build.py,
// tests/test_affected_path_file_node.py, tests/test_prs.py,
// tests/test_manifest_ingest.py, tests/test_atomic_writes.py
//! Install, CLI, hook, query, LLM selection, export and build changes ported
//! from Graphify 0.9.92-0.9.95 (port part b). Cases follow the upstream tests
//! named in each comment.
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
#[test]
fn skill_install_stages_through_random_names_and_follows_no_symlink() {
    // test_install_references.py / test_atomic_version_stamp.py (#4513): a
    // symlink pre-created at a predictable staging name redirects nothing,
    // and the staging directories are gone afterwards.
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let outside = home.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("sentinel.txt"), "untouched\n").unwrap();
    let options = graphicle::install::Options {
        home: home.path().to_path_buf(),
        project_dir: home.path().to_path_buf(),
        claude_config_dir: None,
        ..Default::default()
    };
    let dst = graphicle::install::destination("claude", &options).unwrap();
    let dir = dst.parent().unwrap().to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("references.tmp")).unwrap();
    std::os::unix::fs::symlink(outside.join("sentinel.txt"), dir.join("SKILL.md.tmp")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("sentinel.txt"),
        dir.join(".graphicle_version.tmp"),
    )
    .unwrap();
    graphicle::install::copy_skill("claude", &options).unwrap();
    assert_eq!(
        fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
        "untouched\n"
    );
    let entries = fs::read_dir(&outside)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from("sentinel.txt")]);
    assert!(dir.join("references/workflows.md").is_file());
    assert!(dst.is_file());
    let leftovers = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".references.") || name.starts_with(".gfy-"))
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    // A new version stamp keeps mkstemp's private mode; an existing one keeps
    // its own on the next install.
    let stamp = dir.join(".graphicle_version");
    assert_eq!(
        fs::metadata(&stamp).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::set_permissions(&stamp, fs::Permissions::from_mode(0o644)).unwrap();
    graphicle::install::copy_skill("claude", &options).unwrap();
    assert_eq!(
        fs::metadata(&stamp).unwrap().permissions().mode() & 0o777,
        0o644
    );
    // A fixed-name staging directory an older release left behind is removed.
    fs::remove_file(dir.join("references.tmp")).unwrap();
    fs::create_dir_all(dir.join("references.tmp/stale")).unwrap();
    graphicle::install::copy_skill("claude", &options).unwrap();
    assert!(!dir.join("references.tmp").exists());
}

#[test]
fn help_lists_every_install_platform_from_the_installer_table() {
    // test_cli_help.py (#4655): the `install [--platform P]` line names every
    // value the installer accepts.
    let out = run(&["--help"]);
    let line = out
        .stdout
        .split("install [--platform P]")
        .nth(1)
        .expect("install line");
    let listed = &line[line.find('(').unwrap() + 1..line.find(')').unwrap()];
    let listed = listed.split('|').collect::<Vec<_>>();
    for platform in graphicle::install::platforms() {
        assert!(
            listed.contains(&platform.as_str()),
            "{platform}: {listed:?}"
        );
    }
    for platform in ["amp", "copilot", "kilo", "kimi", "antigravity-windows"] {
        assert!(listed.contains(&platform), "{platform}");
    }
    assert!(out.stdout.contains("post-rewrite"), "{}", out.stdout);
}

#[test]
fn hook_install_adds_post_rewrite_and_anchors_every_hook() {
    // test_hooks.py (#4463, #4490, #3258).
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q"]);
    let hooks = repo.join(".git/hooks");
    git(repo, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let user = hooks.join("post-rewrite");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(&user, "#!/bin/sh\necho 'user rewrite hook'\n").unwrap();
    let installed = graphicle::hooks::install(repo).unwrap();
    assert!(
        installed.contains("\npost-rewrite: appended to existing post-rewrite hook"),
        "{installed}"
    );
    let read = |name: &str| fs::read_to_string(hooks.join(name)).unwrap();
    let rewrite = read("post-rewrite");
    assert!(rewrite.contains("echo 'user rewrite hook'"));
    assert!(rewrite.contains("# graphicle-rewrite-hook-start"));
    assert!(rewrite.contains("hook launch post-rewrite"));
    assert!(rewrite.contains("[ -t 0 ] || cat >/dev/null"));
    assert!(rewrite.contains("[ \"${1:-}\" = \"rebase\" ] || exit 0"));
    // git keeps rebase-merge/ while post-rewrite runs: not a skip signal.
    assert!(!rewrite.contains("rebase-merge"), "{rewrite}");
    let commit = read("post-commit");
    assert!(!commit.contains("CHERRY_PICK_HEAD"), "{commit}");
    assert!(commit.contains("MERGE_HEAD"), "{commit}");
    let merge = read("post-merge");
    assert!(!merge.contains("MERGE_HEAD"), "{merge}");
    // test_checkout_hook_skips_same_head_noop_at_runtime (#2421): a checkout
    // that keeps HEAD (`git checkout -b`) stops at the guard. The emitted
    // script runs under sh up to the guard, then a sentinel.
    let checkout = read("post-checkout");
    let guard = "[ \"${1:-}\" = \"${2:-}\" ] && exit 0";
    assert!(checkout.contains(guard), "{checkout}");
    #[cfg(unix)]
    {
        let prefix = format!(
            "{}{guard}\necho RAN\n)\n",
            checkout.split(guard).next().unwrap()
        );
        let ran = |prev: &str, new: &str, flag: &str| {
            let out = Command::new("sh")
                .args(["-c", &prefix, "hook", prev, new, flag])
                .current_dir(repo)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).contains("RAN")
        };
        assert!(!ran("abc123", "abc123", "1"));
        assert!(ran("abc123", "def456", "1"));
        assert!(!ran("abc123", "def456", "0"));
    }
    for name in ["post-commit", "post-checkout", "post-merge", "post-rewrite"] {
        let script = read(name);
        let guard = script
            .find("[ \"$_GCL_GITDIR\" != \"$_GCL_COMMONDIR\" ]")
            .unwrap();
        let anchor = script
            .find("[ -n \"$_GCL_TOP\" ] && cd \"$_GCL_TOP\" || exit 0")
            .unwrap();
        let launch = script.find("hook launch").unwrap();
        assert!(guard < anchor && anchor < launch, "{name}: {script}");
        assert_eq!(script.matches("_GCL_TOP=${_GCL_COMMONDIR%/*}").count(), 1);
    }
    let status = graphicle::hooks::status(repo).unwrap();
    assert!(status.contains("post-rewrite: installed\n"), "{status}");
    let removed = graphicle::hooks::uninstall(repo).unwrap();
    assert!(removed.contains("post-rewrite: "), "{removed}");
    let kept = read("post-rewrite");
    assert!(kept.contains("echo 'user rewrite hook'"));
    assert!(!kept.contains("graphicle-rewrite-hook-start"));
    let status = graphicle::hooks::status(repo).unwrap();
    assert!(status.contains("post-rewrite: not installed"), "{status}");
}

fn lookup_graph(dir: &Path) -> String {
    let graph = json!({
        "directed": false, "multigraph": false, "graph": {},
        "nodes": [
            {"id": "a_nonce", "label": ".nonce()", "source_file": "a.py", "source_location": "L3"},
            {"id": "b_nonce", "label": ".nonce()", "source_file": "b.py", "source_location": "L7"},
            {"id": "emailcontroller", "label": "EmailController", "source_file": "mail.py", "source_location": "L1"},
            {"id": "a_client", "label": "AlphaClient", "source_file": "a.py", "source_location": "L9"},
            {"id": "b_client", "label": "BetaClient", "source_file": "b.py", "source_location": "L9"}
        ],
        "links": [
            {"source": "a_nonce", "target": "a_client", "relation": "calls", "confidence": "EXTRACTED"},
            {"source": "b_nonce", "target": "b_client", "relation": "calls", "confidence": "EXTRACTED"}
        ]
    });
    let path = dir.join("graph.json");
    fs::write(&path, graph.to_string()).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn query_refuses_an_exact_name_of_several_nodes() {
    // test_lookup_exit_codes.py (#4511, #4517): exit 3 with the candidates;
    // an unresolved --seed exits 4; a keyword that only matches part of
    // several labels still searches.
    let dir = tempfile::tempdir().unwrap();
    let graph = lookup_graph(dir.path());
    let out = run(&["query", ".nonce()", "--graph", &graph]);
    assert_eq!(out.exit_code, 3, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout
            .starts_with("Ambiguous: '.nonce()' matches 2 nodes"),
        "{}",
        out.stdout
    );
    let out = run(&["query", "anything", "--seed", ".nonce()", "--graph", &graph]);
    assert_eq!(out.exit_code, 3, "{}{}", out.stdout, out.stderr);
    let out = run(&["query", "anything", "--seed", "zzqqxx", "--graph", &graph]);
    assert_eq!(out.exit_code, 4, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout
            .starts_with("No unique node match for --seed: zzqqxx")
    );
    let out = run(&["query", "client", "--graph", &graph]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
}

#[test]
fn explain_reports_a_substring_near_miss() {
    // test_serve.py (#3917): a substring-only hit is a near miss, not the
    // answer; a prefix still resolves.
    let dir = tempfile::tempdir().unwrap();
    let graph = lookup_graph(dir.path());
    let out = run(&["explain", "mailcontroller", "--graph", &graph]);
    assert_eq!(out.exit_code, 4, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout.starts_with(
            "No exact match for 'mailcontroller'. These names contain it, but none is called that:\n  EmailController  mail.py L1\n"
        ),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("(e.g. mail.py::EmailController)"));
    let out = run(&["explain", "Email", "--graph", &graph]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
}

#[test]
fn ambiguity_names_the_repo_when_one_path_is_in_two_repos() {
    // test_serve.py (#4512): a merged graph's same-path files stay two files.
    let dir = tempfile::tempdir().unwrap();
    let graph = json!({
        "directed": false, "multigraph": false, "graph": {},
        "nodes": [
            {"id": "repo_a::dup_run", "label": "run()", "source_file": "dup.py", "repo": "repo_a"},
            {"id": "repo_b::dup_run", "label": "run()", "source_file": "dup.py", "repo": "repo_b"}
        ],
        "links": []
    });
    let path = dir.path().join("graph.json");
    fs::write(&path, graph.to_string()).unwrap();
    let out = run(&["explain", "run()", "--graph", path.to_str().unwrap()]);
    assert_eq!(out.exit_code, 3, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout
            .contains("  repo_a/dup.py\n    id: repo_a::dup_run"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("  repo_b/dup.py\n    id: repo_b::dup_run")
    );
    assert!(
        out.stdout
            .contains("Retry with the full node id: the same path exists in more than one repo")
    );
}

#[test]
fn parallel_edges_survive_the_mcp_load_and_render_one_line_each() {
    // test_serve.py (#4500).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.json");
    fs::write(
        &path,
        json!({
            "directed": true, "multigraph": false, "graph": {},
            "nodes": [{"id": "app", "label": "App()"}, {"id": "seed", "label": "applySeed()"}],
            "links": [
                {"source": "app", "target": "seed", "relation": "contains", "confidence": "EXTRACTED",
                 "source_file": "src/App.tsx", "source_location": "80"},
                {"source": "app", "target": "seed", "relation": "calls", "confidence": "EXTRACTED",
                 "source_file": "src/App.tsx", "source_location": "163"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let g = graphicle::serve::load_graph(&path).unwrap();
    assert!(g.is_multigraph());
    assert_eq!(g.iter_edges().count(), 2);
    let i = g.node_index(&json!("app")).unwrap();
    let lines = graphicle::serve::neighbor_lines(&g, i, "");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("[contains]") && l.contains("App.tsx:80"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("[calls]") && l.contains("App.tsx:163"))
    );
    assert_eq!(graphicle::serve::neighbor_lines(&g, i, "calls").len(), 1);
    let links = [
        json!({"source": "a", "target": "b"}),
        json!({"source": "b", "target": "a"}),
    ];
    assert!(!graphicle::build::has_parallel_links(&links, true));
    assert!(graphicle::build::has_parallel_links(&links, false));
}

#[test]
fn every_reader_loads_an_old_graph_file_like_a_new_one() {
    // Graphicle's own default: the MCP loader's repair (#4500) applies to every
    // reader of a saved graph.json, so the same links read the same whichever
    // release wrote the file (old writers: parallel links under `false`).
    let dir = tempfile::tempdir().unwrap();
    let document = |multigraph: bool| {
        json!({
            "directed": true, "multigraph": multigraph, "graph": {},
            "nodes": [{"id": "app", "label": "App()"}, {"id": "seed", "label": "applySeed()"}],
            "links": [
                {"source": "app", "target": "seed", "relation": "contains", "confidence": "EXTRACTED"},
                {"source": "app", "target": "seed", "relation": "calls", "confidence": "EXTRACTED"}
            ]
        })
    };
    let relations = |g: &graphicle::graph::Graph| {
        let mut found: Vec<String> = g
            .iter_edges()
            .map(|(_, _, e)| {
                e.attributes
                    .get("relation")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        found.sort();
        found
    };
    let both = vec!["calls".to_owned(), "contains".to_owned()];
    for (name, multigraph) in [("old", false), ("new", true)] {
        let folder = dir.path().join(name);
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("graph.json");
        fs::write(&path, document(multigraph).to_string()).unwrap();
        for (reader, graph) in [
            ("affected", graphicle::affected::load_graph(&path).unwrap()),
            ("cli", graphicle::cli::load(&path, None).unwrap()),
            ("serve", graphicle::serve::load_graph(&path).unwrap()),
            (
                "paths",
                graphicle::paths::load_node_link_graph(&json!(path)).unwrap(),
            ),
        ] {
            assert!(graph.is_multigraph(), "{name} file, {reader} reader");
            assert_eq!(relations(&graph), both, "{name} file, {reader} reader");
        }
        // `export` (every format reads the file once) and `merge-driver`.
        let graphml = folder.join("graph.graphml");
        let (graph_arg, graphml_arg) = (path.to_str().unwrap(), graphml.to_str().unwrap());
        let out = run(&[
            "export",
            "graphml",
            "--graph",
            graph_arg,
            "--output",
            graphml_arg,
        ]);
        assert_eq!(out.exit_code, 0, "{}", out.stderr);
        let text = fs::read_to_string(&graphml).unwrap();
        assert_eq!(text.matches("<edge ").count(), 2, "{name} file, export");
        let sides: Vec<_> = ["base", "current", "other"]
            .iter()
            .map(|side| {
                let side = folder.join(format!("{side}.json"));
                fs::write(&side, document(multigraph).to_string()).unwrap();
                side.to_str().unwrap().to_owned()
            })
            .collect();
        let out = run(&["merge-driver", &sides[0], &sides[1], &sides[2]]);
        assert_eq!(out.exit_code, 0, "{}", out.stderr);
        let merged: Value = serde_json::from_str(&fs::read_to_string(&sides[1]).unwrap()).unwrap();
        assert_eq!(
            merged["links"].as_array().unwrap().len(),
            2,
            "{name} file, merge-driver"
        );
        fs::write(
            folder.join("global-graph.json"),
            document(multigraph).to_string(),
        )
        .unwrap();
        let global = graphicle::global_graph::GlobalStore::new(folder.clone())
            .load_graph()
            .unwrap();
        assert_eq!(relations(&global), both, "{name} file, global reader");
    }
    // A forced flag still wins in the command-line loader (callflow's view).
    let old = dir.path().join("old/graph.json");
    let simple = graphicle::cli::load_as(&old, None, Some(false)).unwrap();
    assert!(!simple.is_multigraph());
    assert_eq!(simple.iter_edges().count(), 1);
}

#[test]
fn graphicle_backend_selects_or_disables_llm_work() {
    // test_backend_opt_in.py (#4361).
    let llm = |function: &str, env: Value, extra: Value| {
        let mut request = json!({"operation": "llm", "function": function, "env": env});
        for (k, v) in extra.as_object().unwrap() {
            request[k] = v.clone();
        }
        graphicle::dispatch(&request).unwrap()
    };
    let out = llm(
        "detect_backend_with_source",
        json!({"GEMINI_API_KEY": "ambient"}),
        json!({}),
    );
    assert_eq!(
        out["value"],
        json!(["gemini", "auto-detected from GEMINI_API_KEY"])
    );
    let out = llm(
        "detect_backend_with_source",
        json!({"GEMINI_API_KEY": "ambient", "GRAPHICLE_BACKEND": "openai"}),
        json!({}),
    );
    assert_eq!(out["value"], json!(["openai", "GRAPHICLE_BACKEND=openai"]));
    for sentinel in ["none", "off", "disable", "Disabled"] {
        let env = json!({"GEMINI_API_KEY": "ambient", "GRAPHICLE_BACKEND": sentinel});
        let out = llm("detect_backend_with_source", env.clone(), json!({}));
        assert_eq!(
            out["value"],
            json!([null, format!("GRAPHICLE_BACKEND={sentinel}")])
        );
        assert_eq!(
            llm("backend_disabled_by_env", env, json!({}))["value"],
            true
        );
    }
    let out = llm(
        "extract_files_direct",
        json!({"GRAPHICLE_BACKEND": "none"}),
        json!({"files": [], "responses": []}),
    );
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("GRAPHICLE_BACKEND=none"),
        "{out}"
    );
    // The `claude -p` labeling fallback is skipped too.
    let bin = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let claude = bin.path().join("claude");
        fs::write(&claude, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = llm(
        "generate_community_labels",
        json!({"GRAPHICLE_BACKEND": "none", "PATH": bin.path()}),
        json!({
            "G": {"directed": false, "multigraph": false, "graph": {},
                  "nodes": [{"id": "a"}, {"id": "b"}], "edges": []},
            "communities": {"0": ["a"], "1": ["b"]},
            "quiet": true,
            "responses": []
        }),
    );
    assert_eq!(
        out["value"],
        json!([{"0": "Community 0", "1": "Community 1"}, "placeholder"]),
        "{out}"
    );
    assert!(out["calls"].as_array().is_none_or(Vec::is_empty), "{out}");
}

#[test]
fn binary_media_is_listed_but_not_counted() {
    // test_binary_media_words.py (#4548).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["a.png", "b.JPG", "c.mp4", "d.MP3", "e.webp"] {
        fs::write(
            root.join(name),
            b"\x89\x00\xff imaginary words in binary media",
        )
        .unwrap();
    }
    fs::write(
        root.join("diagram.SVG"),
        "<svg><text>real words</text></svg>",
    )
    .unwrap();
    fs::write(root.join("notes.txt"), "three real words").unwrap();
    let mut detector = graphicle::detect::Detector::default();
    let result = detector
        .detect(root, &graphicle::detect::DetectOptions::default())
        .unwrap();
    assert_eq!(result["total_files"], 7, "{result}");
    assert_eq!(result["total_words"], 5, "{result}");
}

#[test]
fn a_merged_graph_tree_has_one_root_per_repo() {
    // test_tree_html.py (#4524).
    let graph = json!({"nodes": [
        {"id": "a1", "label": "EngineA", "source_file": "src/engine.py", "repo": "repo-a"},
        {"id": "b1", "label": "EngineB", "source_file": "src/engine.py", "repo": "repo-b"},
        {"id": "x", "label": "X", "source_file": "x.py"}
    ]});
    let tree = graphicle::tree_html::build_tree(&graph, None, 200, None).unwrap();
    assert_eq!(tree["name"], "(merged graph)");
    let names = tree["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["(no repo)", "repo-a", "repo-b"]);
    let engine = &tree["children"][1]["children"][0]["children"][0];
    assert_eq!(engine["name"], "engine.py");
    assert_eq!(engine["children"][0]["name"], "EngineA");
    let labelled = graphicle::tree_html::build_tree(&graph, None, 200, Some("world")).unwrap();
    assert_eq!(labelled["name"], "world");
    let error = graphicle::tree_html::build_tree(&graph, Some("/abs/elsewhere"), 200, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("matched 0 of 3 source files"), "{error}");
    // One repo tag (plus untagged nodes) is not a merged graph.
    let single = json!({"nodes": [
        {"id": "a", "label": "A", "source_file": "pkg/a.py", "repo": "only-repo"},
        {"id": "b", "label": "B", "source_file": "pkg/b.py"}
    ]});
    let tree = graphicle::tree_html::build_tree(&single, None, 200, None).unwrap();
    assert_eq!(tree["name"], "pkg");
}

#[test]
fn drifted_confidences_fold_onto_the_enum() {
    // test_alias_folding.py (#4495, #4529).
    let fold = |confidence: Value, score: Option<f64>| {
        let mut edge =
            json!({"source": "a", "target": "b", "relation": "calls", "confidence": confidence});
        if let Some(score) = score {
            edge["confidence_score"] = json!(score);
        }
        graphicle::build::fold_edge_aliases(&mut edge);
        (edge["confidence"].clone(), edge["confidence_score"].clone())
    };
    assert_eq!(fold(json!("high"), None), (json!("INFERRED"), Value::Null));
    assert_eq!(fold(json!("0.85"), None), (json!("INFERRED"), json!(0.85)));
    assert_eq!(
        fold(json!(" 1_0e-1 "), None),
        (json!("INFERRED"), json!(1.0))
    );
    assert_eq!(
        fold(json!("0.85"), Some(0.5)),
        (json!("INFERRED"), json!(0.5))
    );
    assert_eq!(
        fold(json!("inferred"), None),
        (json!("INFERRED"), Value::Null)
    );
    assert_eq!(
        fold(json!("extracted"), Some(1.0)),
        (json!("EXTRACTED"), json!(1.0))
    );
    assert_eq!(fold(json!(0.9), None), (json!("INFERRED"), json!(0.9)));
    assert_eq!(
        fold(json!("AMBIGUOUS"), None),
        (json!("AMBIGUOUS"), Value::Null)
    );
    let mut node = json!({"id": "n", "label": "N", "confidence": "confirmed"});
    graphicle::build::fold_node_aliases(&mut node);
    assert_eq!(node["confidence"], "INFERRED");
    let mut plain = json!({"id": "n", "label": "N"});
    graphicle::build::fold_node_aliases(&mut plain);
    assert_eq!(plain, json!({"id": "n", "label": "N"}));
}

#[test]
fn raw_build_resolves_imports_through_the_alias_index() {
    // test_no_cluster_undeclared_endpoints.py (#4670): a framework import
    // written with the bare stem lands on the real header, as the clustered
    // build resolves it; a genuinely external import still gets its stub.
    let mut data = json!({
        "nodes": [
            {"id": "kit_event_toolevent", "label": "ToolEvent.h", "source_file": "Kit/Event/ToolEvent.h"},
            {"id": "kit_holder", "label": "Holder.m", "source_file": "Kit/Holder.m"}
        ],
        "links": [
            {"source": "kit_holder", "target": "toolevent", "relation": "imports"},
            {"source": "kit_holder", "target": "foundation", "relation": "imports"}
        ]
    });
    graphicle::build::finalize_raw_graph_endpoints(&mut data);
    let targets = data["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["target"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(targets, vec!["kit_event_toolevent", "foundation"]);
    let ids = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(!ids.contains(&"toolevent"), "{ids:?}");
    assert!(ids.contains(&"foundation"), "{ids:?}");
}

#[test]
fn affected_resolves_a_disambiguated_file_label() {
    // test_affected_path_file_node.py (#4698): the file node labelled with
    // its shortest unique path wins over a docstring or definition on L1;
    // two nodes claiming a path form of the file are still not guessed between.
    let graph = |extra: Value| {
        let mut nodes = vec![
            json!({"id": "app_alpha_hooks", "label": "alpha/hooks.py", "source_file": "app/alpha/hooks.py", "source_location": "L1", "file_type": "code"}),
            json!({"id": "app_alpha_hooks_check", "label": "check()", "source_file": "app/alpha/hooks.py", "source_location": "L4", "file_type": "code"}),
        ];
        nodes.push(extra);
        graphicle::graph::Graph::from_node_link(
            &json!({"directed": true, "multigraph": false, "graph": {}, "nodes": nodes, "links": []}),
            "links",
        )
        .unwrap()
    };
    for extra in [
        json!({"id": "app_alpha_hooks_rationale_1", "label": "Checks a request before it runs.", "source_file": "app/alpha/hooks.py", "source_location": "L1", "file_type": "rationale"}),
        json!({"id": "app_alpha_hooks_check2", "label": "check2()", "source_file": "app/alpha/hooks.py", "source_location": "L1", "file_type": "code"}),
    ] {
        let g = graph(extra);
        let seed =
            graphicle::affected::resolve_seed(&g, "app/alpha/hooks.py", Some(Path::new("/repo")))
                .unwrap();
        assert_eq!(seed.as_deref(), Some("app_alpha_hooks"));
        let seed = graphicle::affected::resolve_seed(
            &g,
            "/repo/app/alpha/hooks.py",
            Some(Path::new("/repo")),
        )
        .unwrap();
        assert_eq!(seed.as_deref(), Some("app_alpha_hooks"));
    }
    let twins = graph(
        json!({"id": "two", "label": "alpha/hooks.py", "source_file": "app/alpha/hooks.py", "source_location": "L1"}),
    );
    let seed =
        graphicle::affected::resolve_seed(&twins, "app/alpha/hooks.py", Some(Path::new("/repo")))
            .unwrap();
    assert_eq!(seed, None);
}

#[test]
fn pr_ci_reads_status_contexts_and_slashed_default_branches_survive() {
    // test_prs.py (0.9.93, #4533).
    let ci = |rollup: Value| graphicle::prs::parse_ci(rollup.as_array().unwrap());
    assert_eq!(ci(json!([{"state": "FAILURE"}])), "FAILURE");
    assert_eq!(ci(json!([{"state": "ERROR"}])), "FAILURE");
    assert_eq!(
        ci(json!([{"conclusion": "SUCCESS", "status": "COMPLETED"}, {"state": "FAILURE"}])),
        "FAILURE"
    );
    assert_eq!(ci(json!([{"state": "PENDING"}])), "PENDING");
    assert_eq!(ci(json!([{"state": "EXPECTED"}])), "PENDING");
    assert_eq!(ci(json!([{"state": "SUCCESS"}])), "SUCCESS");
    assert_eq!(
        ci(json!([{"conclusion": "", "state": "SUCCESS"}])),
        "SUCCESS"
    );
    assert_eq!(ci(json!([])), "NONE");
    // test_prs.py `test_git_default_branch_with_slash_is_kept_whole` (#4534):
    // with no answer from `gh`, the origin HEAD ref loses only its prefix.
    let branch = |refname: &str| {
        graphicle::dispatch(&json!({
            "operation": "prs", "function": "_detect_default_branch", "env": {}, "repo": null,
            "command_responses": [
                {"program": "gh", "stdout": "null"},
                {"program": "git", "stdout": refname}
            ]
        }))
        .unwrap()["value"]
            .clone()
    };
    assert_eq!(branch("refs/remotes/origin/release/2.x\n"), "release/2.x");
    assert_eq!(branch("refs/remotes/origin/develop\n"), "develop");
}

#[test]
fn maven_poms_contribute_only_their_own_dependencies() {
    // test_manifest_ingest.py (0.9.94, #4596).
    let pom = r#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <groupId>org.example</groupId><artifactId>app</artifactId><version>1.0</version>
  <dependencyManagement><dependencies>
    <dependency><groupId>org.pinned</groupId><artifactId>managed</artifactId></dependency>
  </dependencies></dependencyManagement>
  <dependencies>
    <dependency><groupId>org.real</groupId><artifactId>direct</artifactId></dependency>
  </dependencies>
  <build><plugins><plugin><artifactId>tool</artifactId><dependencies>
    <dependency><groupId>org.tool</groupId><artifactId>plugin-dep</artifactId></dependency>
  </dependencies></plugin></plugins></build>
  <profiles><profile><dependencies>
    <dependency><groupId>org.profile</groupId><artifactId>extra</artifactId></dependency>
  </dependencies></profile></profiles>
</project>"#;
    let parsed = graphicle::manifest_ingest::parse_manifest("maven", pom).unwrap();
    assert_eq!(
        parsed["deps"],
        json!(["org.real:direct", "org.profile:extra"])
    );
}

#[cfg(unix)]
#[test]
fn skill_install_replaces_links_at_the_skill_files() {
    // test_atomic_version_stamp.py
    // `test_write_version_stamp_replaces_symlink_instead_of_following` (#3286)
    // at the real names of a project-scope install, where a repository can
    // commit SKILL.md and .graphicle_version as links: each link is replaced
    // by a regular file and the file it names is left alone; a dangling link
    // creates nothing. A linked SKILL.md is not copied into SKILL.md.bak, and
    // a link at SKILL.md.bak (or at a settings backup) is replaced, not
    // written through (deliberate differences: upstream copies with copy2).
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let victim = |name: &str, text: &str| {
        let path = outside.join(name);
        fs::write(&path, text).unwrap();
        path
    };
    let project = root.path().join("project");
    let options = graphicle::install::Options {
        home: root.path().join("home"),
        project_dir: project.clone(),
        project: true,
        claude_config_dir: None,
        ..Default::default()
    };
    let dst = graphicle::install::destination("claude", &options).unwrap();
    let dir = dst.parent().unwrap().to_path_buf();
    let stamp = dir.join(".graphicle_version");
    fs::create_dir_all(&dir).unwrap();
    let skill_victim = victim("victim-skill.txt", "precious file A\n");
    let stamp_victim = victim("victim-stamp.txt", "precious file B\n");
    symlink(&skill_victim, &dst).unwrap();
    symlink(&stamp_victim, &stamp).unwrap();
    graphicle::install::copy_skill("claude", &options).unwrap();
    assert_eq!(
        fs::read_to_string(&skill_victim).unwrap(),
        "precious file A\n"
    );
    assert_eq!(
        fs::read_to_string(&stamp_victim).unwrap(),
        "precious file B\n"
    );
    assert!(!dst.is_symlink() && !stamp.is_symlink());
    assert!(fs::read_to_string(&dst).unwrap().contains("graphicle"));
    assert_eq!(fs::read_to_string(&stamp).unwrap(), graphicle::VERSION);
    assert_eq!(
        fs::metadata(&stamp).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!dir.join("SKILL.md.bak").exists());
    // Dangling links: nothing is created where they point.
    for name in ["SKILL.md", ".graphicle_version"] {
        fs::remove_file(dir.join(name)).unwrap();
        symlink(outside.join(format!("created-{name}")), dir.join(name)).unwrap();
    }
    graphicle::install::copy_skill("claude", &options).unwrap();
    for name in ["SKILL.md", ".graphicle_version"] {
        assert!(!outside.join(format!("created-{name}")).exists(), "{name}");
        assert!(!dir.join(name).is_symlink(), "{name}");
    }
    // A locally edited SKILL.md is kept in SKILL.md.bak; a link there is
    // replaced by the copy.
    let bak_victim = victim("victim-bak.txt", "precious file C\n");
    let bak = dir.join("SKILL.md.bak");
    symlink(&bak_victim, &bak).unwrap();
    fs::write(&dst, "local edit\n").unwrap();
    let (_, backup) = graphicle::install::copy_skill_reporting("claude", &options).unwrap();
    assert_eq!(backup.as_deref(), Some(bak.as_path()));
    assert_eq!(
        fs::read_to_string(&bak_victim).unwrap(),
        "precious file C\n"
    );
    assert!(!bak.is_symlink());
    assert_eq!(fs::read_to_string(&bak).unwrap(), "local edit\n");
    // The settings backup is renamed over its own name as well.
    let settings = project.join(".claude/settings.json");
    fs::write(&settings, "{\"other\": true}").unwrap();
    let settings_victim = victim("victim-settings.txt", "precious file D\n");
    let settings_bak = project.join(".claude/settings.json.graphicle-bak");
    symlink(&settings_victim, &settings_bak).unwrap();
    graphicle::dispatch(&json!({
        "operation": "install", "action": "write_settings",
        "path": settings, "settings": {"other": false}
    }))
    .unwrap();
    assert_eq!(
        fs::read_to_string(&settings_victim).unwrap(),
        "precious file D\n"
    );
    assert!(!settings_bak.is_symlink());
    assert_eq!(
        fs::read_to_string(&settings_bak).unwrap(),
        "{\"other\": true}"
    );
}

#[cfg(unix)]
#[test]
fn graph_json_links_are_replaced_by_rebuilds_and_followed_by_exports() {
    // Upstream's `_rebuild_code` renames its new graph over graph.json
    // (`os_replace_with_fallback(graph_tmp, existing_graph)`), so a link
    // there is replaced, with or without clustering; `to_json` (extract,
    // cluster-only) writes through a link, as `write_json_atomic` does for
    // shared output folders (test_atomic_writes.py). GraphML is renamed over
    // its output.
    use graphicle::watch::{self, RebuildOptions};
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("app.py"), "def alpha():\n    return 1\n").unwrap();
    let graph = root.join(".graphicle/graph.json");
    for (round, no_cluster) in [true, false].into_iter().enumerate() {
        let options = RebuildOptions {
            acquire_lock: false,
            force: true,
            no_cluster,
            ..Default::default()
        };
        let result = watch::rebuild(&root, &options).unwrap();
        assert!(result.success, "{:?}", result.messages);
        // Move the graph outside and leave a link to it in its place.
        let target = outside.join(format!("graph-{round}.json"));
        fs::rename(&graph, &target).unwrap();
        symlink(&target, &graph).unwrap();
        let before = fs::read_to_string(&target).unwrap();
        fs::write(
            root.join("app.py"),
            format!("def alpha():\n    return 1\n\ndef beta_{round}():\n    return 2\n"),
        )
        .unwrap();
        let result = watch::rebuild(&root, &options).unwrap();
        assert!(result.success, "{:?}", result.messages);
        assert!(!graph.is_symlink(), "no_cluster={no_cluster}");
        assert!(
            fs::read_to_string(&graph)
                .unwrap()
                .contains(&format!("beta_{round}"))
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            before,
            "no_cluster={no_cluster}"
        );
    }
    // cluster-only writes through the link (upstream `to_json`): the link
    // stays and the file it names is replaced.
    use std::os::unix::fs::MetadataExt;
    let target = outside.join("shared.json");
    fs::rename(&graph, &target).unwrap();
    symlink(&target, &graph).unwrap();
    let inode = fs::metadata(&target).unwrap().ino();
    let r = run(&[
        "cluster-only",
        root.to_str().unwrap(),
        "--no-viz",
        "--no-label",
    ]);
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    assert!(graph.is_symlink());
    assert_ne!(fs::metadata(&target).unwrap().ino(), inode);
    // GraphML is renamed over its output: a link there is replaced.
    let g = graphicle::graph::Graph::from_node_link(
        &json!({"directed": false, "multigraph": false, "graph": {},
                "nodes": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}],
                "links": [{"source": "a", "target": "b", "relation": "calls"}]}),
        "links",
    )
    .unwrap();
    let victim = outside.join("victim.graphml");
    fs::write(&victim, "precious\n").unwrap();
    let output = root.join(".graphicle/graph.graphml");
    symlink(&victim, &output).unwrap();
    graphicle::export::to_graphml(&g, &vec![(0, vec![json!("a"), json!("b")])], &output).unwrap();
    assert_eq!(fs::read_to_string(&victim).unwrap(), "precious\n");
    assert!(!output.is_symlink());
    assert!(fs::read_to_string(&output).unwrap().contains("<graphml"));
}

#[cfg(unix)]
#[test]
fn links_at_the_lock_the_graph_temp_and_gitattributes_are_not_followed() {
    // Deliberate differences from upstream, which follows a link at each of
    // these names (a repository can commit one): the rebuild lock is
    // recreated as a regular file, the rebuild's graph goes through a random
    // temporary name, and hook install leaves a linked .gitattributes alone.
    use graphicle::watch::{self, RebuildOptions};
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(root.join(".graphicle")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("app.py"), "def alpha():\n    return 1\n").unwrap();
    let victim = |name: &str| {
        let path = outside.join(name);
        fs::write(&path, "precious\n").unwrap();
        path
    };
    let lock_victim = victim("lock");
    let temp_victim = victim("graph-temp");
    symlink(&lock_victim, root.join(".graphicle/.rebuild.lock")).unwrap();
    symlink(&temp_victim, root.join(".graphicle/.graph.tmp.json")).unwrap();
    let options = RebuildOptions {
        acquire_lock: true,
        force: true,
        ..Default::default()
    };
    let result = watch::rebuild(&root, &options).unwrap();
    assert!(result.success, "{:?}", result.messages);
    assert_eq!(fs::read_to_string(&lock_victim).unwrap(), "precious\n");
    assert_eq!(fs::read_to_string(&temp_victim).unwrap(), "precious\n");
    let graph = root.join(".graphicle/graph.json");
    assert!(!graph.is_symlink());
    let data: Value = serde_json::from_str(&fs::read_to_string(&graph).unwrap()).unwrap();
    assert!(
        data["nodes"]
            .as_array()
            .is_some_and(|nodes| !nodes.is_empty())
    );
    let leftovers = fs::read_dir(root.join(".graphicle"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with(".graph.") && name.ends_with(".tmp.json") && *name != ".graph.tmp.json"
        })
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    // hook install neither reads nor writes a linked .gitattributes.
    git(&root, &["init", "-q"]);
    let attributes_victim = victim("attributes");
    symlink(&attributes_victim, root.join(".gitattributes")).unwrap();
    let installed = graphicle::hooks::install(&root).unwrap();
    assert!(
        installed.contains("symbolic link, left alone"),
        "{installed}"
    );
    assert_eq!(
        fs::read_to_string(&attributes_victim).unwrap(),
        "precious\n"
    );
    assert!(root.join(".gitattributes").is_symlink());
    let status = graphicle::hooks::status(&root).unwrap();
    assert!(
        status.contains("partially registered (git config set, .gitattributes line missing)"),
        "{status}"
    );
    let removed = graphicle::hooks::uninstall(&root).unwrap();
    assert!(removed.contains("left alone"), "{removed}");
    assert_eq!(
        fs::read_to_string(&attributes_victim).unwrap(),
        "precious\n"
    );
}
