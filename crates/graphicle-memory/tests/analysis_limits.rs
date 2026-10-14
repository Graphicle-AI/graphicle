//! Per-file analysis limits: no limit fails an analysis. A
//! file past one is analysed as far as the limit allows, `complete: false`,
//! with an issue that names the limit and `limited` naming its kind; a file
//! within every limit is analysed as before, with no `limited`.
use graphicle_memory::dispatch;
use graphicle_memory::structure::{
    Limits, MAX_PATH_BYTES, REVISION, analyze, analyze_with, limit_issues, limited_revision,
    skipped,
};
use serde_json::{Value, json};

fn limits(calls: usize, symbols: usize, nodes: usize) -> Limits {
    Limits {
        bytes: 4 << 20,
        calls,
        symbols,
        nodes,
    }
}

fn issues(analysis: &Value) -> Vec<&str> {
    analysis["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

fn count(analysis: &Value, list: &str) -> usize {
    analysis[list].as_array().unwrap().len()
}

#[test]
fn the_defaults_are_graphicle_cores() {
    assert_eq!(
        Limits::default(),
        Limits {
            bytes: 4 * 1024 * 1024,
            calls: 50_000,
            symbols: 16_000,
            nodes: 2_000_000,
        }
    );
}

#[test]
fn a_file_over_the_size_limit_is_text_only() {
    let content = "def f():\n    g()\n".repeat(100);
    let small = Limits {
        bytes: 64,
        ..Limits::default()
    };
    let analysis = analyze_with("big.py", &content, &small).unwrap();
    assert_eq!(analysis["limited"], "size");
    assert_eq!(analysis["complete"], false);
    assert_eq!(analysis["revision"], REVISION);
    for list in ["symbols", "imports", "calls", "links"] {
        assert_eq!(analysis[list], json!([]), "{list}");
    }
    assert_eq!(
        issues(&analysis),
        ["Too large to analyse (1,700 bytes; limit 64); searchable as text only"]
    );
    assert_eq!(limit_issues(&analysis), issues(&analysis));
    // Within the limit the same file is analysed in full.
    let full = analyze("big.py", &content).unwrap();
    assert!(full.get("limited").is_none());
    assert_eq!(count(&full, "symbols"), 100);
    assert_eq!(count(&full, "calls"), 100);
    assert!(limit_issues(&full).is_empty());
    // A path past the path limit is text only too.
    let path = format!("{}x.py", "a/".repeat(MAX_PATH_BYTES / 2));
    let long = analyze(&path, "def f():\n    pass\n").unwrap();
    assert_eq!(long["limited"], "size");
    assert!(issues(&long)[0].starts_with("Path too long to analyse (2,004 bytes; limit 2,000)"));
}

#[test]
fn calls_past_the_limit_are_truncated_with_a_note() {
    let content = format!("def main():\n{}", "    work()\n".repeat(5));
    let analysis = analyze_with("calls.py", &content, &limits(3, 100, 1000)).unwrap();
    assert_eq!(count(&analysis, "calls"), 3);
    assert_eq!(analysis["limited"], "calls");
    assert_eq!(analysis["complete"], false);
    assert_eq!(limit_issues(&analysis), ["Call list truncated at 3 calls"]);
    assert_eq!(analysis["symbols"][0]["name"], "main");
    // Earlier builds kept 2,000 calls of a file; now it keeps them all.
    let many = format!("def main():\n{}", "    work()\n".repeat(2500));
    let analysis = analyze("many.py", &many).unwrap();
    assert_eq!(count(&analysis, "calls"), 2500);
    assert!(analysis.get("limited").is_none());
    assert_eq!(analysis["complete"], true);
}

#[test]
fn the_walk_stops_at_the_node_limit_with_a_note() {
    let content = "def f():\n    return 1\n".repeat(50);
    let analysis = analyze_with("nodes.py", &content, &limits(100, 100, 20)).unwrap();
    assert_eq!(analysis["limited"], "nodes");
    assert_eq!(
        limit_issues(&analysis),
        ["Analysis stopped at 20 syntax nodes"]
    );
    assert!(count(&analysis, "symbols") < 50);
}

#[test]
fn declarations_imports_and_links_past_the_limit_are_truncated_with_a_note() {
    let content: String = (0..10)
        .map(|i| format!("import m{i}\ndef f{i}():\n    pass\n"))
        .collect();
    let analysis = analyze_with("defs.py", &content, &limits(100, 4, 100_000)).unwrap();
    let names: Vec<_> = analysis["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["f0", "f1", "f2", "f3"]);
    assert_eq!(count(&analysis, "imports"), 4);
    assert_eq!(analysis["limited"], "symbols");
    assert_eq!(
        limit_issues(&analysis),
        [
            "Declaration list truncated at 4 declarations",
            "Import list truncated at 4 imports"
        ]
    );
    let markdown: String = (0..6)
        .map(|i| format!("See [part {i}](part{i}.md).\n"))
        .collect();
    let analysis = analyze_with("index.md", &markdown, &limits(100, 2, 100_000)).unwrap();
    assert_eq!(count(&analysis, "links"), 2);
    assert_eq!(analysis["limited"], "symbols");
    assert_eq!(limit_issues(&analysis), ["Link list truncated at 2 links"]);
}

/// The languages of the language pack used to fail the whole analysis past
/// 2,000 declarations or calls or 200,000 nodes; now they degrade the same way.
#[test]
fn language_pack_languages_degrade_instead_of_failing() {
    let go: String = std::iter::once("package main\n\n".to_string())
        .chain((0..6).map(|i| format!("func f{i}() {{\n\twork()\n\twork()\n}}\n\n")))
        .collect();
    let full = analyze("main.go", &go).unwrap();
    assert!(full.get("limited").is_none(), "{full}");
    assert_eq!(count(&full, "calls"), 12);
    let symbols = analyze_with("main.go", &go, &limits(1000, 3, 100_000)).unwrap();
    assert_eq!(count(&symbols, "symbols"), 3);
    assert_eq!(symbols["limited"], "symbols");
    assert_eq!(
        limit_issues(&symbols),
        ["Declaration list truncated at 3 declarations"]
    );
    let calls = analyze_with("main.go", &go, &limits(5, 1000, 100_000)).unwrap();
    assert_eq!(count(&calls, "calls"), 5);
    assert_eq!(calls["limited"], "calls");
    // Each call keeps its innermost enclosing function as the caller: `f<i>`
    // opens on line 3 + 5i, its calls are on the next two lines.
    let mut callers: Vec<_> = calls["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| {
            let line = call["line"].as_u64().unwrap();
            let caller = call["caller"].as_str().unwrap();
            assert!(caller.ends_with(&format!("f{}", (line - 4) / 5)), "{call}");
            caller
        })
        .collect();
    callers.dedup();
    assert!(callers.len() >= 3, "{callers:?}");
    let nodes = analyze_with("main.go", &go, &limits(1000, 1000, 10)).unwrap();
    assert_eq!(nodes["limited"], "nodes");
    assert_eq!(
        limit_issues(&nodes),
        ["Analysis stopped at 10 syntax nodes"]
    );
    // 2,500 calls: past earlier builds' failure point, within the default.
    let busy = format!(
        "package main\n\nfunc main() {{\n{}}}\n",
        "\twork()\n".repeat(2500)
    );
    let analysis = analyze("busy.go", &busy).unwrap();
    assert_eq!(count(&analysis, "calls"), 2500);
    assert!(analysis.get("limited").is_none());
}

#[test]
fn a_skipped_file_is_recorded_text_only_with_its_reason() {
    let analysis = skipped("Source analysis failed: parse cancelled");
    assert_eq!(
        analysis["skipped"],
        "Source analysis failed: parse cancelled"
    );
    assert_eq!(analysis["complete"], false);
    assert!(analysis.get("limited").is_none());
    assert_eq!(
        issues(&analysis),
        ["Not analysed: Source analysis failed: parse cancelled; searchable as text only"]
    );
    for list in ["symbols", "imports", "calls", "links"] {
        assert_eq!(analysis[list], json!([]), "{list}");
    }
}

#[test]
fn limited_analyses_are_cached_under_their_limits() {
    let defaults = limited_revision(&Limits::default());
    assert!(defaults.starts_with(REVISION));
    assert_ne!(defaults, REVISION);
    assert_ne!(defaults, limited_revision(&limits(1, 16_000, 2_000_000)));
}

#[test]
fn the_native_analyze_operation_takes_limits() {
    let content = format!("def main():\n{}", "    work()\n".repeat(5));
    let analysis = dispatch(
        json!({"operation":"analyze","path":"a.py","content":content,"limits":{"calls":2}}),
    )
    .unwrap();
    assert_eq!(count(&analysis, "calls"), 2);
    assert_eq!(analysis["limited"], "calls");
    let analysis =
        dispatch(json!({"operation":"analyze","path":"a.py","content":content})).unwrap();
    assert_eq!(count(&analysis, "calls"), 5);
    for bad in [
        json!({"calls":0}),
        json!({"calls":"2"}),
        json!({"depth":2}),
        json!([1]),
    ] {
        assert!(
            dispatch(json!({"operation":"analyze","path":"a.py","content":"x","limits":bad}))
                .is_err(),
            "{bad}"
        );
    }
}
