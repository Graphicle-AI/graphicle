// Adapted from Graphify 0.9.105, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_affected_member_seed.py, tests/test_tree_html.py,
// tests/test_serve.py
//! Affected, tree and query changes ported from Graphify 0.9.105. Cases
//! follow the upstream tests named in each comment.
use serde_json::json;

#[test]
fn file_affected_reaches_nested_method_callers() {
    // test_affected_member_seed.py (#5094)
    let data = json!({"directed": true, "multigraph": false, "graph": {},
        "nodes": [
            {"id": "file_p", "label": "processor.py"},
            {"id": "proc", "label": "Processor"},
            {"id": "proc_call", "label": ".call()"},
            {"id": "runner", "label": "Runner"},
            {"id": "runner_run", "label": ".run()"},
        ],
        "links": [
            {"source": "file_p", "target": "proc", "relation": "contains"},
            {"source": "proc", "target": "proc_call", "relation": "method"},
            {"source": "runner", "target": "runner_run", "relation": "method"},
            {"source": "runner_run", "target": "proc_call", "relation": "calls"},
        ]});
    let g = graphicle::paths::load_node_link_graph(&data).unwrap();
    let relations: Vec<String> = graphicle::affected::DEFAULT_AFFECTED_RELATIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let hits = graphicle::affected::affected_nodes(&g, "file_p", &relations, 2)
        .unwrap()
        .into_iter()
        .map(|h| h.node_id)
        .collect::<Vec<_>>();
    assert!(hits.contains(&"runner_run".to_string()), "{hits:?}");
}

#[test]
fn build_tree_single_file_common_root_is_directory() {
    // test_tree_html.py test_build_tree_single_file_common_root_is_directory (#5092)
    let graph = json!({"directed": false, "links": [],
        "nodes": [{"id": "n0", "label": "func_a", "source_file": "pkg/a.py"}]});
    let tree = graphicle::tree_html::build_tree(&graph, None, 200, None).unwrap();
    assert_eq!(tree["name"], "pkg", "{tree:#}");
    let names = |v: &serde_json::Value| {
        v["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&tree), ["a.py"]);
    assert_eq!(names(&tree["children"][0]), ["func_a"]);
}

#[test]
fn constructor_call_resolves_through_destructured_barrel_require() {
    // test_js_cjs_barrel_require_call.py (#5034)
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let files = [
        ("source.js", "class SourceCode {}\nmodule.exports = SourceCode;\n"),
        ("index.js", "const SourceCode = require('./source');\nmodule.exports = { SourceCode };\n"),
        ("consumer.js", "const { SourceCode } = require('./index');\nfunction create() { return new SourceCode(); }\n"),
    ];
    let paths = files
        .iter()
        .map(|(name, body)| {
            std::fs::write(root.join(name), body).unwrap();
            root.join(name)
        })
        .collect::<Vec<_>>();
    let r = graphicle::api::extract(
        &paths,
        &graphicle::api::ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("cache")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap();
    let node = |id: &serde_json::Value| r["nodes"].as_array().unwrap().iter().find(|n| n["id"] == *id);
    let calls = r["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls")
        .filter_map(|e| {
            let (s, t) = (node(&e["source"])?, node(&e["target"])?);
            Some((
                s["label"].as_str()?.to_string(),
                t["label"].as_str()?.to_string(),
                std::path::Path::new(t["source_file"].as_str()?)
                    .file_name()?
                    .to_string_lossy()
                    .into_owned(),
            ))
        })
        .collect::<Vec<_>>();
    assert!(
        calls.contains(&("create()".into(), "SourceCode".into(), "source.js".into())),
        "{calls:?}"
    );
    // The internal keys never reach the result.
    let text = r.to_string();
    assert!(!text.contains("_require_target_file") && !text.contains("_require_imported_name"));
}

#[test]
fn racily_clean_window_holds_when_seen_lands_before_mtime() {
    // test_detect*.py test_a_row_stamped_just_before_the_mtime_still_hashes and
    // ..._well_before_the_mtime_keeps_the_fastpath (#5036, #5090)
    let mtime_may_hide_a_rewrite = |current: f64, stored: &serde_json::Value| {
        graphicle::dispatch(&json!({"operation": "detect", "function": "_mtime_may_hide_a_rewrite",
            "current_mtime": current, "stored": stored}))
        .unwrap()
            == json!(true)
    };
    let mtime = 1_700_000_000.5;
    assert!(mtime_may_hide_a_rewrite(mtime, &json!({"seen": mtime - 0.000_000_24})));
    assert!(!mtime_may_hide_a_rewrite(mtime, &json!({"seen": mtime - 1.0})));
    assert!(mtime_may_hide_a_rewrite(mtime, &json!({"seen": mtime + 0.01})));
}

fn serve(action: &str, request: serde_json::Value) -> serde_json::Value {
    let mut request = request;
    request["operation"] = json!("serve");
    request["action"] = json!(action);
    graphicle::dispatch(&request).unwrap()
}

#[test]
fn qualified_label_beats_a_whole_label_token_collision() {
    // test_serve.py (#5080)
    let mut nodes = vec![];
    for i in 0..4 {
        nodes.push(json!({"id": format!("migrations/04{i}_widget_sweep.sql::public.sweep_widget_state()"),
            "label": "public.sweep_widget_state()", "norm_label": "public.sweep_widget_state()",
            "source_file": format!("migrations/04{i}_widget_sweep.sql"), "community": 0}));
    }
    nodes.push(json!({"id": "consts_sweep", "label": "SWEEP", "norm_label": "sweep", "source_file": "src/consts.py", "community": 0}));
    nodes.push(json!({"id": "consts_widget", "label": "WIDGET", "norm_label": "widget", "source_file": "src/consts.py", "community": 0}));
    nodes.push(json!({"id": "types_state", "label": "State", "norm_label": "state", "source_file": "src/types.py", "community": 0}));
    for i in 0..6 {
        nodes.push(json!({"id": format!("noise_{i}"), "label": format!("unrelated_{i}"), "source_file": format!("src/noise{i}.py"), "community": 1}));
    }
    let g = json!({"directed": false, "multigraph": false, "graph": {}, "nodes": nodes, "links": []});
    let scored = serve("score_nodes", json!({"graph": g, "terms": ["sweep_widget_state"]}));
    let scored = scored.as_array().unwrap();
    for row in &scored[..4] {
        assert!(row[1].as_str().unwrap().starts_with("migrations/"), "{scored:?}");
    }
    assert!(scored[4][0].as_f64().unwrap() < scored[0][0].as_f64().unwrap(), "{scored:?}");

    let g = json!({"directed": false, "multigraph": false, "graph": {}, "links": [], "nodes": [
        {"id": "qualified", "label": "public.sweep()", "norm_label": "public.sweep()", "source_file": "m.sql"},
        {"id": "plain", "label": "sweep", "norm_label": "sweep", "source_file": "s.py"},
    ]});
    let scored = serve("score_nodes", json!({"graph": g, "terms": ["sweep"]}));
    assert_eq!(scored[0][1], "plain", "{scored}");
}

#[test]
fn qualified_tail_split() {
    use graphicle::serve_search::qualified_tail;
    let some = |n: usize, s: &str| Some((n, s.to_string()));
    assert_eq!(qualified_tail("public.sweep_widget_state()"), some(1, "sweep widget state"));
    assert_eq!(qualified_tail("Index.ClientLive"), some(1, "clientlive"));
    assert_eq!(qualified_tail("fmt.Println"), some(1, "println"));
    assert_eq!(qualified_tail("Widget::sweep"), some(1, "sweep"));
    assert_eq!(qualified_tail("lib/foo/bar.py"), some(2, "bar"));
    for none in ["3.14", "HTTPClient", "sweep_widget_state", ""] {
        assert_eq!(qualified_tail(none), None, "{none}");
    }
}

#[test]
fn path_refuses_endpoint_that_names_no_node() {
    // test_path_cli.py test_path_refuses_endpoint_that_names_no_node (#5079)
    let node = |id: &str, label: &str, sf: &str| json!({"id": id, "label": label, "source_file": sf, "source_location": "L1"});
    let link = |a: &str, b: &str, r: &str| json!({"source": a, "target": b, "relation": r, "confidence": "EXTRACTED"});
    let g = json!({"directed": false, "multigraph": false, "graph": {}, "nodes": [
        node("src_a_ts", "a.ts", "src/a.ts"), node("src_a_ts_parse", "parse()", "src/a.ts"),
        node("src_b_ts", "b.ts", "src/b.ts"), node("src_b_ts_helper", "no_such_symbol_helper()", "src/b.ts"),
        node("src_b_ts_format", "format()", "src/b.ts"), node("src_c_ts", "c.ts", "src/c.ts"),
        node("src_c_ts_format", "format()", "src/c.ts"),
    ], "links": [
        link("src_a_ts", "src_a_ts_parse", "contains"), link("src_a_ts_parse", "src_b_ts_helper", "calls"),
        link("src_b_ts", "src_b_ts_helper", "contains"), link("src_b_ts", "src_b_ts_format", "contains"),
        link("src_c_ts", "src_c_ts_format", "contains"), link("src_b_ts_helper", "src_b_ts_format", "calls"),
    ]});
    let dir = tempfile::tempdir().unwrap();
    let gp = dir.path().join("graph.json");
    std::fs::write(&gp, g.to_string()).unwrap();
    let args = ["path", "zzqqxx_no_such_symbol_9137", "src/b.ts::format()", "--graph", gp.to_str().unwrap()]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let out = graphicle::cli::run(&args);
    assert_ne!(out.exit_code, 0, "{} {}", out.stdout, out.stderr);
    assert!(!out.stdout.contains("Shortest path"), "{}", out.stdout);
    assert!(!out.stdout.contains("no_such_symbol_helper"), "{}", out.stdout);
}

#[test]
fn markdown_keeps_uppercase_namespace_classes_and_called_members() {
    // test_markdown_code_spans.py (#5093)
    use graphicle::extractors::markdown::code_span_mention;
    let parts = |v: &[&str]| Some((None, v.iter().map(|s| s.to_string()).collect::<Vec<_>>()));
    assert_eq!(code_span_mention("App\\Services\\DB"), parts(&["App", "Services", "DB"]));
    assert_eq!(code_span_mention("App\\API"), parts(&["App", "API"]));
    assert_eq!(code_span_mention("Http::GET()"), parts(&["Http", "GET"]));
    assert_eq!(code_span_mention("Foo::CONST"), None);
    assert_eq!(code_span_mention("Foo::class"), None);
}

#[test]
fn structured_enum_fields_are_reported_and_folded() {
    // test_build.py / test_validate.py (#5086, #5088)
    let data = json!({
        "nodes": [
            {"id": "a", "label": "A", "file_type": ["code"], "source_file": "a.py"},
            {"id": "b", "label": "B", "file_type": "code", "source_file": "b.py"},
        ],
        "edges": [{"source": "a", "target": "b", "relation": "calls", "confidence": {"x": 1}, "source_file": "a.py"}],
    });
    let errors = graphicle::validate::validate_extraction(&data).unwrap();
    assert!(errors.iter().any(|e| e.contains("invalid file_type")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("invalid confidence")), "{errors:?}");
    let mut data = data;
    data["hyperedges"] = json!([]);
    let mut diagnostics = graphicle::build::Diagnostics::default();
    let g = graphicle::build_from_json(&mut data, true, None, &mut diagnostics).unwrap();
    let out = g.to_node_link("links");
    let a = out["nodes"].as_array().unwrap().iter().find(|n| n["id"] == "a").unwrap();
    assert_eq!(a["file_type"], "concept", "{out:#}");
    assert_eq!(out["links"][0]["confidence"], "INFERRED", "{out:#}");
}
