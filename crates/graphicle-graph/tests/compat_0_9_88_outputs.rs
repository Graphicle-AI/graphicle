// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
//! Output fixes ported from Graphify 0.9.78-0.9.88: graph.json provenance,
//! compact layout and the saved-semantic-node guard; graph_diff direction;
//! knowledge gaps and low-confidence hints; graph.html script escaping, theme
//! and sidebar tooltips; SVG control characters; affected `defines` and
//! overrides; hook path decoding and `.graphiclerc` parsing; installer line
//! endings, hook identity, OpenCode plugins and purge safety; the benchmark's
//! real query answer.
use graphicle::build::Diagnostics;
use graphicle::cluster::Communities;
use graphicle::graph::Graph;
use graphicle::install::Options;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Message text for comparison: on Windows the verbatim prefix and the
/// separator kind are ignored (the message joins `.opencode/...` onto a
/// resolved folder); elsewhere the text itself.
fn seps(text: &str) -> String {
    if cfg!(windows) {
        text.replace(r"\\?\", "").replace('\\', "/")
    } else {
        text.to_string()
    }
}

fn graph(directed: bool, nodes: Vec<Value>, links: Vec<Value>) -> Graph {
    let data = json!({"directed": directed, "multigraph": false, "graph": {},
                      "nodes": nodes, "links": links});
    Graph::from_node_link(&data, "links").unwrap()
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn communities(groups: &[(i64, &[&str])]) -> Communities {
    groups
        .iter()
        .map(|(cid, members)| (*cid, members.iter().map(|m| json!(m)).collect()))
        .collect()
}

// ---------------------------------------------------------------------------
// graph.json (export.to_json)
// ---------------------------------------------------------------------------

fn write_json(graph: &Graph, path: &Path, force: bool) -> (bool, Vec<String>) {
    let mut d = Diagnostics::default();
    let written = graphicle::export::to_json(
        graph,
        &vec![],
        path,
        &graphicle::export::JsonOptions {
            force,
            ..Default::default()
        },
        &mut d,
    )
    .unwrap();
    (written, d.stderr)
}

/// 0.9.78 (#4167): the persisted graph carries its schema version and the
/// release that wrote it, under the engine's key (`graphicle_version`: the
/// Graphify release it ports and its version).
#[test]
fn to_json_records_the_schema_and_writer_version() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.json");
    let g = graph(false, vec![json!({"id": "a", "label": "A"})], vec![]);
    assert!(write_json(&g, &out, false).0);
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(!text.contains("graphify"), "{text}");
    let meta = &read(&out)["graph"];
    assert_eq!(meta["schema_version"], json!(1));
    let version = meta[graphicle::export::GRAPH_VERSION_KEY].as_str().unwrap();
    assert_eq!(
        version,
        format!(
            "{}-graphicle.{}",
            graphicle::UPSTREAM_VERSION,
            graphicle::VERSION
        )
    );
    assert_eq!(graphicle::export::graphicle_version(), version);
}

/// A graph.json Graphify wrote carries its writer stamp (`graphify_version`);
/// rewriting it records the engine in that slot, so the file names one writer.
#[test]
fn to_json_replaces_an_upstream_writer_stamp_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.json");
    let with = |attributes: Value| {
        let data = json!({"directed": false, "multigraph": false, "graph": attributes,
                          "nodes": [{"id": "a", "label": "A"}], "links": []});
        Graph::from_node_link(&data, "links").unwrap()
    };
    let g = with(json!({"schema_version": 1, "graphify_version": "0.9.88", "name": "kept"}));
    assert!(write_json(&g, &out, false).0);
    let meta = read(&out)["graph"].as_object().unwrap().clone();
    let keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    assert_eq!(keys, ["schema_version", "graphicle_version", "name"]);
    assert_eq!(
        meta["graphicle_version"],
        json!(graphicle::export::graphicle_version())
    );
    // The engine's own stamp is refreshed where it stands.
    let g = with(json!({"graphicle_version": "old", "name": "kept"}));
    assert!(write_json(&g, &out, true).0);
    let meta = read(&out)["graph"].as_object().unwrap().clone();
    let keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    assert_eq!(keys, ["graphicle_version", "name", "schema_version"]);
}

/// 0.9.86 (#3413): graph.json is one compact line, the same data.
#[test]
fn to_json_writes_one_compact_line() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.json");
    let g = graph(
        true,
        vec![
            json!({"id": "a", "label": "caf\u{e9}, \"x\": y", "source_file": "a.py"}),
            json!({"id": "b", "label": "B"}),
        ],
        vec![json!({"source": "a", "target": "b", "relation": "calls"})],
    );
    assert!(write_json(&g, &out, false).0);
    let text = fs::read_to_string(&out).unwrap();
    assert!(!text.contains('\n'), "{text}");
    assert!(
        text.starts_with(r#"{"directed":true,"multigraph":false,"graph":{"#),
        "{text}"
    );
    assert!(text.contains(r#""label":"caf\u00e9, \"x\": y""#), "{text}");
    assert!(text.contains(r#","links":[{"source":"a","target":"b","relation":"calls","#));
    let data: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(data["nodes"].as_array().unwrap().len(), 2);
}

fn semantic(id: &str, label: &str) -> Value {
    json!({"id": id, "label": label, "source_file": "src/app.ts", "_origin": "semantic",
           "source_location": null, "file_type": "rationale"})
}

fn ast(id: &str, label: &str, line: u32) -> Value {
    json!({"id": id, "label": label, "source_file": "src/app.ts", "_origin": "ast",
           "source_location": format!("L{line}"), "file_type": "code"})
}

/// 0.9.87 (#2229/#4115): AST growth and a new stub must not replace a saved
/// rationale id, although the node count grows.
#[test]
fn to_json_refuses_a_lost_semantic_id_when_the_total_grows() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.json");
    let saved = vec![
        ast("app_ts", "app.ts", 1),
        ast("app_ts_main", "main", 2),
        semantic("rationale_kept", "kept"),
        semantic("rationale_lost", "lost"),
    ];
    fs::write(&out, json!({"nodes": saved, "links": []}).to_string()).unwrap();
    let g = graph(
        false,
        vec![
            ast("app_ts", "app.ts", 1),
            ast("app_ts_main", "main", 2),
            ast("app_ts_hook", "hook", 10),
            ast("app_ts_route", "route", 20),
            ast("app_ts_util", "util", 30),
            semantic("rationale_kept", "kept"),
            json!({"id": "pathlib_Path", "label": "Path", "source_file": "",
                   "source_location": null, "file_type": "concept"}),
        ],
        vec![],
    );
    let (written, stderr) = write_json(&g, &out, false);
    assert!(!written);
    let ids: Vec<Value> = read(&out)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].clone())
        .collect();
    assert!(ids.contains(&json!("rationale_lost")));
    assert!(
        stderr
            .iter()
            .any(|line| line.contains("saved semantic node(s) are missing")
                && line.contains("rationale_lost")),
        "{stderr:?}"
    );
    // force still overwrites.
    assert!(write_json(&g, &out, true).0);
}

#[test]
fn to_json_allows_ast_growth_that_keeps_semantic_ids() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.json");
    let saved = vec![
        ast("app_ts", "app.ts", 1),
        semantic("rationale_kept", "kept"),
        semantic("rationale_lost", "lost"),
    ];
    fs::write(&out, json!({"nodes": saved, "links": []}).to_string()).unwrap();
    let mut nodes = saved.clone();
    nodes.push(ast("app_ts_hook", "hook", 10));
    assert!(write_json(&graph(false, nodes, vec![]), &out, false).0);
}

/// An unstamped integer id still in the graph is not lost; a missing one is.
#[test]
fn to_json_checks_integer_ids() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("racecar");
    let nodes = vec![json!({"id": 1}), json!({"id": 2}), json!({"id": 3})];
    let links = vec![
        json!({"source": 1, "target": 2}),
        json!({"source": 1, "target": 3}),
        json!({"source": 2, "target": 3}),
    ];
    let g = graph(false, nodes.clone(), links);
    assert!(write_json(&g, &out, false).0);
    assert!(write_json(&g, &out, false).0);
    let out = dir.path().join("graph.json");
    fs::write(&out, r#"{"nodes": [{"id": 4}], "links": []}"#).unwrap();
    assert!(!write_json(&graph(false, nodes, vec![]), &out, false).0);
    assert_eq!(read(&out)["nodes"], json!([{"id": 4}]));
}

/// 0.9.87: only a `nodes` list counts toward the shrink guard; another type
/// reads as no saved nodes, where it used to count its length.
#[test]
fn to_json_counts_only_a_nodes_list() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.json");
    fs::write(&out, r#"{"nodes": {"a": 1, "b": 2, "c": 3}}"#).unwrap();
    let g = graph(false, vec![json!({"id": "a"})], vec![]);
    assert!(write_json(&g, &out, false).0);
    fs::write(&out, "[1, 2, 3]").unwrap();
    assert!(
        !write_json(&g, &out, false).0,
        "a non-object document is unreadable"
    );
}

// ---------------------------------------------------------------------------
// graph.html and tree.html
// ---------------------------------------------------------------------------

const DANGEROUS: [&str; 3] = [
    "foo <!-- bar components the panel",
    "code:vue (<script setup>)",
    "end </script> tail",
];

fn no_breakout(html: &str) {
    assert!(!html.contains("<!--"));
    assert!(!html.contains("<script setup"));
    assert!(!html.contains("</script> tail"));
    assert!(html.contains("\\u003c"));
}

fn html_page(nodes: Vec<Value>, links: Vec<Value>, groups: &[(i64, &[&str])]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph.html");
    let mut d = Diagnostics::default();
    let options = graphicle::exporters::html::HtmlOptions {
        community_labels: BTreeMap::from([(0, json!("Core"))]),
        ..Default::default()
    };
    assert!(
        graphicle::exporters::html::to_html(
            &graph(false, nodes, links),
            &communities(groups),
            &out,
            &options,
            &mut d
        )
        .unwrap()
    );
    fs::read_to_string(out).unwrap()
}

/// 0.9.85 (#4124): every `<` in the embedded JSON is escaped, so no label can
/// close or reopen the script element; `<` still round-trips through JSON.
#[test]
fn labels_cannot_break_out_of_the_graph_view_script() {
    let nodes = DANGEROUS
        .iter()
        .enumerate()
        .map(|(i, label)| json!({"id": format!("n{i}"), "label": label, "file_type": "code", "source_file": format!("f{i}.py")}))
        .collect();
    let html = html_page(
        nodes,
        vec![json!({"source": "n0", "target": "n1", "relation": "imports"})],
        &[(0, &["n0", "n1", "n2"])],
    );
    no_breakout(&html);
    assert_eq!(
        html.matches("<script").count(),
        html.matches("</script").count()
    );
    let clean = html_page(
        vec![
            json!({"id": "a", "label": "plain_label", "file_type": "code", "source_file": "a.py"}),
            json!({"id": "b", "label": "Vector<T>", "file_type": "code", "source_file": "b.ts"}),
        ],
        vec![json!({"source": "a", "target": "b", "relation": "imports"})],
        &[(0, &["a", "b"])],
    );
    assert!(clean.contains("plain_label"));
    assert!(clean.contains("Vector\\u003cT>"));
    let decoded: String = serde_json::from_str(r#""Vector<T>""#).unwrap();
    assert_eq!(decoded, "Vector<T>");
}

#[test]
fn labels_cannot_break_out_of_the_tree_view_script() {
    let tree = json!({"name": "root", "children": DANGEROUS.iter().map(|l| json!({"name": l, "children": []})).collect::<Vec<_>>()});
    no_breakout(&graphicle::tree_html::emit_html(
        &tree, "t", "h", 6000, 8000,
    ));
}

fn css_block(html: &str, selector: &str) -> BTreeMap<String, String> {
    let start = html.find(&format!("{selector} {{")).expect(selector);
    let body = &html[start + selector.len() + 2..];
    let body = &body[..body.find('}').unwrap()];
    body.split(';')
        .filter_map(|decl| decl.trim().split_once(':'))
        .filter(|(name, _)| name.starts_with("--"))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// 0.9.87 (#3894): light/dark/system theme toggle; the dark default keeps the
/// colors the page always had, and the saved choice applies before paint.
#[test]
fn graph_view_theme_toggle() {
    let html = html_page(
        vec![
            json!({"id": "a", "label": "Alpha", "source_file": "a.py", "file_type": "code"}),
            json!({"id": "b", "label": "Beta", "source_file": "b.py", "file_type": "code"}),
        ],
        vec![json!({"source": "a", "target": "b", "relation": "calls", "confidence": "EXTRACTED"})],
        &[(0, &["a", "b"])],
    );
    let dark = css_block(&html, ":root");
    for (name, value) in [
        ("--bg", "#0f0f1a"),
        ("--panel", "#1a1a2e"),
        ("--border", "#2a2a4e"),
        ("--input-border", "#3a3a5e"),
        ("--text", "#e0e0e0"),
        ("--text-2", "#aaa"),
        ("--text-3", "#ccc"),
        ("--muted", "#555"),
        ("--count", "#666"),
        ("--neighbor-border", "#333"),
        ("--node-label", "#ffffff"),
    ] {
        assert_eq!(dark.get(name).map(String::as_str), Some(value), "{name}");
    }
    let light = css_block(&html, "[data-theme=\"light\"]");
    assert_eq!(
        light.keys().collect::<Vec<_>>(),
        dark.keys().collect::<Vec<_>>()
    );
    assert_ne!(light["--bg"], dark["--bg"]);
    for choice in ["light", "dark", "system"] {
        assert!(html.contains(&format!("data-theme-choice=\"{choice}\"")));
    }
    let head = html.split("</head>").next().unwrap();
    assert!(head.contains("try { pref = localStorage.getItem('graphicle-theme'); } catch (e) {}"));
    assert!(head.contains("setAttribute('data-theme'") && head.contains(": 'dark';"));
    assert!(html.contains("try { localStorage.setItem(THEME_KEY, pref); } catch (e) {}"));
    assert!(html.contains("addEventListener('change'"));
}

/// 0.9.88 (#4286): sidebar tooltips use one shared `.vis-tooltip`, filled with
/// textContent only.
#[test]
fn graph_view_sidebar_tooltips_use_text_only() {
    let html = html_page(
        vec![json!({"id": "a", "label": "a", "file_type": "code", "source_file": "a.py"})],
        vec![],
        &[(0, &["a"])],
    );
    assert!(html.contains("sidebarTip.className = 'vis-tooltip';"));
    assert!(html.contains("max-width: 360px; overflow-wrap: break-word"));
    assert!(html.contains("sidebarTip.textContent = text;"));
    assert!(!html.contains("sidebarTip.innerHTML"));
}

// ---------------------------------------------------------------------------
// SVG
// ---------------------------------------------------------------------------

/// 0.9.82 (#4244): XML-illegal control characters in labels and legend
/// entries are stripped; the readable rest of the text stays.
#[test]
fn svg_strips_control_characters_from_labels_and_the_legend() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("g.svg");
    let g = graph(
        false,
        vec![
            json!({"id": "a", "label": "Build \u{1b}[31mlog\u{1b}[0m capture", "file_type": "document", "source_file": "d.md"}),
            json!({"id": "b", "label": "plain", "file_type": "code", "source_file": "b.py"}),
        ],
        vec![json!({"source": "a", "target": "b", "relation": "references"})],
    );
    let labels = BTreeMap::from([(0, json!("Core \u{7}module"))]);
    graphicle::export::to_svg(
        &g,
        &communities(&[(0, &["a", "b"])]),
        &out,
        &labels,
        (20.0, 14.0),
    )
    .unwrap();
    let svg = fs::read_to_string(&out).unwrap();
    roxmltree::Document::parse(&svg).unwrap();
    assert!(svg.contains("Build [31mlog[0m capture"));
    assert!(svg.contains("Core module (2)"));
    assert!(!svg.contains('\u{1b}') && !svg.contains('\u{7}'));
}

// ---------------------------------------------------------------------------
// analyze and report
// ---------------------------------------------------------------------------

fn marked(src: &str, tgt: &str) -> Graph {
    graph(
        false,
        vec![
            json!({"id": "n1", "label": "Alpha"}),
            json!({"id": "n2", "label": "Beta"}),
        ],
        vec![
            json!({"source": src, "target": tgt, "relation": "calls", "confidence": "EXTRACTED", "_src": src, "_tgt": tgt}),
        ],
    )
}

/// 0.9.78 (#4067/#4170): a reversed edge of an undirected graph is reported
/// as removed and added, in its stored direction, when both sides mark it.
#[test]
fn graph_diff_reports_a_reversed_edge() {
    let pairs = |rows: &Value| -> Vec<(String, String)> {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["source"].as_str().unwrap().into(),
                    e["target"].as_str().unwrap().into(),
                )
            })
            .collect()
    };
    let diff = graphicle::analyze::graph_diff(&marked("n1", "n2"), &marked("n2", "n1")).unwrap();
    assert_eq!(pairs(&diff["new_edges"]), [("n2".into(), "n1".into())]);
    assert_eq!(pairs(&diff["removed_edges"]), [("n1".into(), "n2".into())]);
    assert_eq!(diff["summary"], "1 new edge, 1 edge removed");
    let same = graphicle::analyze::graph_diff(&marked("n2", "n1"), &marked("n2", "n1")).unwrap();
    assert_eq!(same["summary"], "no changes");
    let plain = |src: &str, tgt: &str| {
        graph(
            false,
            vec![
                json!({"id": "n1", "label": "Alpha"}),
                json!({"id": "n2", "label": "Beta"}),
            ],
            vec![
                json!({"source": src, "target": tgt, "relation": "calls", "confidence": "EXTRACTED"}),
            ],
        )
    };
    let unmarked = graphicle::analyze::graph_diff(&plain("n1", "n2"), &plain("n2", "n1")).unwrap();
    assert_eq!(unmarked["summary"], "no changes");
    let one_side = graphicle::analyze::graph_diff(&plain("n1", "n2"), &marked("n2", "n1")).unwrap();
    assert_eq!(one_side["summary"], "no changes");
}

fn report(g: &Graph, groups: &[(i64, &[&str])], questions: Value) -> String {
    let comm: Map<String, Value> = groups
        .iter()
        .map(|(cid, members)| (cid.to_string(), json!(members)))
        .collect();
    graphicle::report::generate(
        g,
        &json!({"communities": comm, "cohesion_scores": {}, "community_labels": {},
                "god_node_list": [], "surprise_list": [],
                "detection_result": {"total_files": 1, "total_words": 10},
                "token_cost": {}, "root": "proj", "min_community_size": 1,
                "suggested_questions": questions, "today": "2026-10-13"}),
    )
    .unwrap()
}

fn gaps(text: &str) -> &str {
    text.split("## Knowledge Gaps")
        .nth(1)
        .map(|rest| rest.split("## ").next().unwrap())
        .unwrap_or("")
}

/// 0.9.81 (#4205): an AST declaration whose only edge is its file's
/// `contains` is not a gap; a semantic one, or an AST node with another
/// relation, still is.
#[test]
fn contains_only_ast_declarations_are_not_knowledge_gaps() {
    let mut nodes = vec![
        json!({"id": "tabs", "label": "tabs.ts", "file_type": "code", "source_file": "src/tabs.ts"}),
    ];
    let mut links = vec![];
    for name in ["TabRow", "ContributionRow", "TabRecognition"] {
        nodes.push(
            json!({"id": name, "label": name, "file_type": "code", "source_file": "src/tabs.ts",
                          "source_location": "L1", "_origin": "ast"}),
        );
        links.push(json!({"source": "tabs", "target": name, "relation": "contains", "confidence": "EXTRACTED"}));
    }
    let g = graph(false, nodes, links);
    let members: &[&str] = &["tabs", "TabRow", "ContributionRow", "TabRecognition"];
    let text = report(&g, &[(0, members)], json!([]));
    assert!(!gaps(&text).contains("TabRow") && !gaps(&text).contains("ContributionRow"));
    let index = g
        .nodes()
        .iter()
        .position(|n| n.id == json!("TabRow"))
        .unwrap();
    assert!(graphicle::analyze::is_contains_only_ast_node(&g, index));
    let questions =
        graphicle::analyze::suggest_questions(&g, &communities(&[(0, members)]), &Map::new(), 7)
            .unwrap();
    assert!(questions.iter().all(|q| q["type"] != "isolated_nodes"));

    let semantic = graph(
        false,
        vec![
            json!({"id": "doc", "label": "guide.md", "file_type": "document", "source_file": "docs/guide.md"}),
            json!({"id": "concept", "label": "OnboardingChecklist", "file_type": "document",
                   "source_file": "docs/guide.md", "source_location": null, "_origin": "semantic"}),
        ],
        vec![
            json!({"source": "doc", "target": "concept", "relation": "contains", "confidence": "EXTRACTED"}),
        ],
    );
    assert!(
        gaps(&report(&semantic, &[(0, &["doc", "concept"])], json!([])))
            .contains("OnboardingChecklist")
    );

    let loose = graph(
        false,
        vec![
            json!({"id": "a", "label": "a.ts", "file_type": "code", "source_file": "src/a.ts"}),
            json!({"id": "loose", "label": "LooseHelper", "file_type": "code", "source_file": "src/a.ts",
                   "source_location": "L1", "_origin": "ast"}),
        ],
        vec![
            json!({"source": "a", "target": "loose", "relation": "calls", "confidence": "AMBIGUOUS"}),
        ],
    );
    assert!(gaps(&report(&loose, &[(0, &["a", "loose"])], json!([]))).contains("LooseHelper"));
}

/// 0.9.81 (#4199): AMBIGUOUS-edge questions are tagged low_confidence, other
/// types never are, and the report lists them under their own heading.
#[test]
fn ambiguous_questions_are_low_confidence_hints() {
    let mut nodes = vec![];
    let mut links = vec![];
    for side in ["A", "B"] {
        for i in 0..5 {
            nodes.push(json!({"id": format!("{side}{i}"), "label": format!("{side}{i}")}));
            for j in i + 1..5 {
                links.push(
                    json!({"source": format!("{side}{i}"), "target": format!("{side}{j}"),
                                  "confidence": "EXTRACTED", "weight": 1.0}),
                );
            }
        }
    }
    for k in 0..5 {
        nodes.push(json!({"id": format!("amb_{k}"), "label": format!("amb_{k}")}));
        for hub in ["A0", "B0"] {
            links.push(
                json!({"source": hub, "target": format!("amb_{k}"), "confidence": "AMBIGUOUS",
                              "weight": 0.3, "relation": "conceptually_related_to"}),
            );
        }
    }
    let g = graph(false, nodes, links);
    let a: Vec<String> = (0..5).map(|i| format!("A{i}")).collect();
    let b: Vec<String> = (0..5).map(|i| format!("B{i}")).collect();
    let amb: Vec<String> = (0..5).map(|k| format!("amb_{k}")).collect();
    let comm: Communities = vec![
        (0, a.iter().map(|s| json!(s)).collect()),
        (1, b.iter().map(|s| json!(s)).collect()),
        (2, amb.iter().map(|s| json!(s)).collect()),
    ];
    let labels: Map<String, Value> = [("0", "A clique"), ("1", "B clique"), ("2", "Noise")]
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
    let questions = graphicle::analyze::suggest_questions(&g, &comm, &labels, 50).unwrap();
    let ambiguous: Vec<_> = questions
        .iter()
        .filter(|q| q["type"] == "ambiguous_edge")
        .collect();
    assert!(!ambiguous.is_empty());
    assert!(ambiguous.iter().all(|q| q["low_confidence"] == json!(true)));
    assert!(
        questions
            .iter()
            .filter(|q| q["type"] != "ambiguous_edge")
            .all(|q| q.get("low_confidence").is_none())
    );
    let all: Vec<&str> = a.iter().chain(&b).chain(&amb).map(String::as_str).collect();
    let text = report(&g, &[(0, all.as_slice())], json!(questions));
    let (main, hints) = text
        .split_once("### Low-confidence Hints")
        .expect("low-confidence section");
    assert!(hints.starts_with(
        "\n_AMBIGUOUS edges — the extractor was unsure. Verify before acting on these._\n\n- **What is the exact relationship"
    ));
    assert!(
        !main
            .split("## Suggested Questions")
            .nth(1)
            .unwrap()
            .contains("exact relationship")
    );
}

// ---------------------------------------------------------------------------
// affected
// ---------------------------------------------------------------------------

fn digraph(edges: &[(&str, &str, &str)]) -> Graph {
    let mut ids: Vec<&str> = vec![];
    for (s, t, _) in edges {
        for id in [s, t] {
            if !ids.contains(id) {
                ids.push(id);
            }
        }
    }
    let nodes = ids
        .iter()
        .map(|id| {
            let label = match id.rsplit_once('.') {
                Some((_, name)) => format!(".{name}()"),
                None => id.to_string(),
            };
            json!({"id": id, "label": label})
        })
        .collect();
    let links = edges
        .iter()
        .map(|(s, t, r)| json!({"source": s, "target": t, "relation": r}))
        .collect();
    graph(true, nodes, links)
}

fn hits(g: &Graph, seed: &str, depth: i64) -> BTreeMap<String, i64> {
    let relations: Vec<String> = graphicle::affected::DEFAULT_AFFECTED_RELATIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    graphicle::affected::affected_nodes(g, seed, &relations, depth)
        .unwrap()
        .into_iter()
        .map(|hit| (hit.node_id, hit.depth))
        .collect()
}

fn map(rows: &[(&str, i64)]) -> BTreeMap<String, i64> {
    rows.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// 0.9.85 (#4288): a symbol reached only through its file's `defines` edge
/// leads to the file and on to its importers.
#[test]
fn affected_follows_defines_to_the_defining_file() {
    assert!(graphicle::affected::DEFAULT_AFFECTED_RELATIONS.contains(&"defines"));
    let g = digraph(&[
        ("db_file", "db_class", "defines"),
        ("consumer_file", "db_file", "imports"),
    ]);
    assert_eq!(
        hits(&g, "db_class", 2),
        map(&[("db_file", 1), ("consumer_file", 2)])
    );
}

/// 0.9.86 (#4330): the callers of a supertype member an override overrides
/// are walked, the member itself as a stepping stone.
#[test]
fn affected_walks_callers_of_overridden_members() {
    let g = digraph(&[
        ("Visitor", "Visitor.head", "method"),
        ("Impl", "Impl.head", "method"),
        ("Impl", "Visitor", "implements"),
        ("walk", "Visitor.head", "calls"),
    ]);
    let found = hits(&g, "Impl.head", 2);
    assert_eq!(found.get("walk"), Some(&1));
    assert!(!found.contains_key("Visitor.head"));

    let chain = digraph(&[
        ("Root", "Root.close", "method"),
        ("Mid", "Root", "inherits"),
        ("Leaf", "Mid", "inherits"),
        ("Leaf", "Leaf.close", "method"),
        ("client", "Root.close", "calls"),
        ("outer", "client", "calls"),
    ]);
    assert_eq!(
        hits(&chain, "Leaf.close", 2),
        map(&[("client", 1), ("outer", 2)])
    );

    let overload = digraph(&[
        ("Base", "Base.read", "method"),
        ("Base.read", "Base.read_all", "calls"),
        ("Base", "Base.read_all", "method"),
        ("Impl", "Base", "inherits"),
        ("Impl", "Impl.read", "method"),
        ("Base.read_all", "Base.read", "calls"),
    ]);
    assert_eq!(
        hits(&overload, "Impl.read", 2).get("Base.read_all"),
        Some(&1)
    );

    let recursive = digraph(&[
        ("Base", "Base.m", "method"),
        ("Base.m", "Base.m", "calls"),
        ("Impl", "Base", "inherits"),
        ("Impl", "Impl.m", "method"),
    ]);
    assert_eq!(hits(&recursive, "Impl.m", 2), map(&[("Base.m", 1)]));

    let shallow = digraph(&[
        ("Iface", "Iface.run", "method"),
        ("Impl", "Iface", "implements"),
        ("Impl", "Impl.run", "method"),
        ("a", "Iface.run", "calls"),
        ("b", "a", "calls"),
    ]);
    assert_eq!(hits(&shallow, "Impl.run", 1), map(&[("a", 1)]));

    let unrelated = digraph(&[
        ("Other", "Other.head", "method"),
        ("Impl", "Impl.head", "method"),
        ("walk", "Other.head", "calls"),
    ]);
    assert!(!hits(&unrelated, "Impl.head", 2).contains_key("walk"));

    let cycle = digraph(&[
        ("A", "B", "inherits"),
        ("B", "A", "inherits"),
        ("A", "A.m", "method"),
        ("B", "B.m", "method"),
        ("x", "B.m", "calls"),
    ]);
    assert_eq!(hits(&cycle, "A.m", 2), map(&[("x", 1)]));

    // Without edge direction a subtype cannot be told from a supertype.
    let undirected = graph(
        false,
        g.nodes()
            .iter()
            .map(|n| json!({"id": n.id, "label": n.attributes.get("label")}))
            .collect(),
        vec![
            json!({"source": "Visitor", "target": "Visitor.head", "relation": "method"}),
            json!({"source": "Impl", "target": "Impl.head", "relation": "method"}),
            json!({"source": "Impl", "target": "Visitor", "relation": "implements"}),
            json!({"source": "walk", "target": "Visitor.head", "relation": "calls"}),
        ],
    );
    assert!(!hits(&undirected, "Impl.head", 2).contains_key("walk"));
}

// ---------------------------------------------------------------------------
// git hooks and .graphiclerc
// ---------------------------------------------------------------------------

/// 0.9.85 (#4278): names git C-quotes in `--name-only` output decode back.
#[test]
fn unquote_git_path_decodes_git_c_quoting() {
    for (raw, expected) in [
        ("pkg/plain.py", "pkg/plain.py"),
        ("ünïcödé/naïve.py", "ünïcödé/naïve.py"),
        (
            r#""\303\274n\303\257c\303\266d\303\251/na\303\257ve.py""#,
            "ünïcödé/naïve.py",
        ),
        (r#""\344\270\255\346\226\207/a.py""#, "中文/a.py"),
        (r#""say \"hi\".py""#, r#"say "hi".py"#),
        (r#""tab\there.py""#, "tab\there.py"),
        (r#""back\\slash.py""#, r"back\slash.py"),
        ("\"", "\""),
    ] {
        assert_eq!(
            graphicle::hooks::unquote_git_path(raw),
            PathBuf::from(expected),
            "{raw}"
        );
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every changed path the post-commit hook collects for a commit touching
/// non-ASCII and quote-bearing names is a file on disk.
#[test]
fn changed_files_are_the_files_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "."]);
    git(root, &["config", "user.email", "t@t.co"]);
    git(root, &["config", "user.name", "t"]);
    git(root, &["config", "core.quotePath", "true"]);
    fs::write(root.join("seed.txt"), "x").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let mut names = vec!["plain.py", "ünïcödé/naïve.py", "中文/用户.py"];
    if cfg!(unix) {
        names.push("say \"hi\".py");
    }
    for name in &names {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "def f():\n    return 1\n").unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "edit"]);
    let mut changed = graphicle::hooks::changed_files(root).unwrap();
    changed.sort();
    let mut expected: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
    expected.sort();
    assert_eq!(changed, expected);
    assert!(changed.iter().all(|p| root.join(p).is_file()));
}

/// 0.9.87 (#2961): the hooks read `.graphiclerc` through the shared parser,
/// so a malformed `language.` line is an error and other options come back.
#[test]
fn hooks_read_the_rc_file_through_the_shared_parser() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".graphiclerc");
    fs::write(&rc, "# options\nviz_node_limit=250\nlanguage.inc=php\n").unwrap();
    assert_eq!(
        graphicle::hooks::load_rc(dir.path()).unwrap(),
        json!({"viz_node_limit": 250, "languages": {".inc": ".php"}})
    );
    fs::write(&rc, "language.inc=klingon\n").unwrap();
    let error = graphicle::hooks::load_rc(dir.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("Invalid language.inc"), "{error}");
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

fn options(home: &Path, project: &Path) -> Options {
    Options {
        home: home.to_path_buf(),
        project_dir: project.to_path_buf(),
        project: true,
        windows: false,
        claude_config_dir: None,
        local_appdata: None,
        executable: Some("/native bin/graphicle-graph".into()),
        strict: false,
    }
}

fn seed(path: &Path, newline: &str) -> Vec<u8> {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body = ["# My rules", "", "some notes"].join(newline) + newline;
    fs::write(path, &body).unwrap();
    body.into_bytes()
}

fn single_ending(data: &[u8], newline: &str) {
    let lf = data.iter().filter(|b| **b == b'\n').count();
    let crlf = data.windows(2).filter(|w| w == b"\r\n").count();
    if newline == "\r\n" {
        assert_eq!(lf, crlf, "file is not wholly CRLF");
    } else {
        assert_eq!(crlf, 0, "an LF file gained CRLF endings");
    }
}

/// 0.9.88 (#4360): every instructions writer keeps the file's own line
/// endings, and install then uninstall restores the exact bytes.
#[test]
fn instruction_writers_keep_line_endings() {
    for newline in ["\n", "\r\n"] {
        for (platform, rel) in [
            ("claude", "CLAUDE.md"),
            ("codex", "AGENTS.md"),
            ("gemini", "GEMINI.md"),
            ("codebuddy", "CODEBUDDY.md"),
            ("vscode", ".github/copilot-instructions.md"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (home, project) = (dir.path().join("home"), dir.path().join("proj"));
            fs::create_dir_all(&home).unwrap();
            let target = project.join(rel);
            let original = seed(&target, newline);
            let o = options(&home, &project);
            graphicle::install::configure(platform, &o, false).unwrap();
            let installed = fs::read(&target).unwrap();
            assert!(String::from_utf8_lossy(&installed).contains("## graphicle"));
            single_ending(&installed, newline);
            graphicle::install::configure(platform, &o, true).unwrap();
            assert_eq!(
                fs::read(&target).unwrap(),
                original,
                "{platform} {newline:?}"
            );
        }
    }
}

#[test]
fn skill_registration_and_local_sections_keep_line_endings() {
    for newline in ["\n", "\r\n"] {
        let dir = tempfile::tempdir().unwrap();
        let (home, project) = (dir.path().join("home"), dir.path().join("proj"));
        fs::create_dir_all(&home).unwrap();
        let registration = project.join(".claude/CLAUDE.md");
        let original = seed(&registration, newline);
        let o = options(&home, &project);
        graphicle::install::install("claude", &o).unwrap();
        single_ending(&fs::read(&registration).unwrap(), newline);
        assert!(
            fs::read_to_string(&registration)
                .unwrap()
                .contains("# graphicle")
        );
        graphicle::install::uninstall("claude", &o).unwrap();
        assert_eq!(fs::read(&registration).unwrap(), original);

        let local = project.join("CLAUDE.local.md");
        let text = [
            "# local",
            "",
            "## graphicle",
            "- managed line",
            "",
            "## other",
            "keep me",
        ]
        .join(newline)
            + newline;
        fs::write(&local, &text).unwrap();
        graphicle::install::configure("claude", &o, true).unwrap();
        let data = fs::read(&local).unwrap();
        single_ending(&data, newline);
        let left = String::from_utf8(data).unwrap();
        assert!(
            !left.contains("## graphicle") && left.contains("keep me") && left.contains("local")
        );
    }
}

#[test]
fn a_new_instructions_file_is_written_lf() {
    let dir = tempfile::tempdir().unwrap();
    let (home, project) = (dir.path().join("home"), dir.path().join("proj"));
    fs::create_dir_all(&home).unwrap();
    graphicle::install::configure("vscode", &options(&home, &project), false).unwrap();
    let data = fs::read(project.join(".github/copilot-instructions.md")).unwrap();
    assert!(!data.is_empty() && !data.windows(2).any(|w| w == b"\r\n"));
}

/// 0.9.86: the always-on texts expect a large graph.json diff in reviews.
#[test]
fn always_on_texts_expect_whole_graph_rewrites() {
    for name in ["agents-md", "claude-md", "gemini-md"] {
        assert!(graphicle::install::always_on(name).unwrap().contains(
            "- A committed .graphicle/graph.json is rewritten whole on every build, so a large diff to it in a pull request is expected."
        ));
    }
}

fn settings_hook(platform: &str, o: &Options, remove: bool) {
    graphicle::install::dispatch(&json!({
        "action": "hook", "platform": platform, "remove": remove,
        "options": serde_json::to_value(o).unwrap()
    }))
    .unwrap();
}

/// 0.9.88 (#4359): uninstall removes only the engine's own hook entries; a
/// user command in the same group and an unrelated group survive, a
/// non-object entry is kept, and the file is backed up.
#[test]
fn hook_uninstall_removes_only_the_engines_entries() {
    for (platform, rel, section) in [
        ("claude", ".claude/settings.json", "PreToolUse"),
        ("codebuddy", ".codebuddy/settings.json", "PreToolUse"),
        ("codex", ".codex/hooks.json", "PreToolUse"),
        ("gemini", ".gemini/settings.json", "BeforeTool"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (home, project) = (dir.path().join("home"), dir.path().join("proj"));
        fs::create_dir_all(&home).unwrap();
        let o = options(&home, &project);
        settings_hook(platform, &o, false);
        let path = project.join(rel);
        let mut settings = read(&path);
        let groups = settings["hooks"][section].as_array_mut().unwrap();
        groups[0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "python /tools/my-linter.py"}));
        groups.push(json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "python /tools/graphicle-notes/check.py"}]}));
        groups.push(json!("a-legacy-string-entry"));
        fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
        settings_hook(platform, &o, true);
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("my-linter"), "{platform}");
        assert!(after.contains("graphicle-notes"), "{platform}");
        assert!(
            !after.contains("hook-guard") && !after.contains("hook-check"),
            "{platform}"
        );
        assert!(
            read(&path)["hooks"][section]
                .as_array()
                .unwrap()
                .contains(&json!("a-legacy-string-entry"))
        );
        assert!(
            path.with_file_name(format!(
                "{}.graphicle-bak",
                path.file_name().unwrap().to_string_lossy()
            ))
            .exists(),
            "{platform}: uninstall wrote no backup"
        );
        // A re-install is idempotent: one set of engine groups.
        settings_hook(platform, &o, false);
        settings_hook(platform, &o, false);
        let text = fs::read_to_string(&path).unwrap();
        let expected = if platform == "claude" || platform == "codebuddy" {
            2
        } else {
            1
        };
        assert_eq!(
            text.matches("hook-guard").count() + text.matches("hook-check").count(),
            expected,
            "{platform}"
        );
    }
}

/// 0.9.86 (#4329): `--purge` refuses a target that would take the project,
/// home or a file system root with it, before any uninstaller runs.
#[test]
fn purge_refuses_unsafe_targets() {
    let dir = tempfile::tempdir().unwrap();
    let (home, project) = (dir.path().join("home"), dir.path().join("parent/project"));
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let reason = graphicle::install::unsafe_purge_reason;
    assert_eq!(
        reason(&project, &project, &home),
        Some("the project folder or one of its parents")
    );
    assert_eq!(
        reason(project.parent().unwrap(), &project, &home),
        Some("the project folder or one of its parents")
    );
    assert_eq!(
        reason(&home, &project, &home),
        Some("your home folder or one of its parents")
    );
    assert!(reason(Path::new("/"), &project, &home).is_some());
    assert_eq!(reason(&project.join(".graphicle"), &project, &home), None);
    assert_eq!(
        reason(&dir.path().join("shared-out"), &project, &home),
        None
    );

    // An output folder that resolves to home: nothing is touched.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&home, project.join(".graphicle")).unwrap();
        fs::write(home.join("keep.txt"), "x").unwrap();
        let claude = project.join("CLAUDE.md");
        fs::write(&claude, "## graphicle\nmanaged\n").unwrap();
        let error = graphicle::install::uninstall_all(&options(&home, &project), true)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("refusing --purge: "), "{error}");
        assert!(error.contains("your home folder or one of its parents. Nothing was changed."));
        assert!(home.join("keep.txt").is_file());
        assert!(fs::read_to_string(&claude).unwrap().contains("managed"));
    }
}

// ---------------------------------------------------------------------------
// OpenCode plugin (0.9.84, #3554/#3732)
// ---------------------------------------------------------------------------

const V1_PLUGIN: &str = "// Graphicle OpenCode plugin\n// Injects a knowledge graph reminder before bash tool calls when the graph exists.\nimport { existsSync } from \"fs\";\n\nexport const GraphiclePlugin = async ({ directory }) => ({});\n";
const ENTRY: &str = ".opencode/plugins/graphicle.js";

fn current_plugin() -> String {
    graphicle::install::constant("_OPENCODE_PLUGIN_JS")
}

fn old_install(directory: &Path, body: &str, entry: bool) -> (PathBuf, PathBuf) {
    let plugin = directory.join(ENTRY);
    fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    fs::write(&plugin, body).unwrap();
    let config = directory.join(".opencode/opencode.json");
    let mut settings = json!({"model": "some/model"});
    if entry {
        settings["plugin"] = json!([ENTRY]);
    }
    fs::write(&config, settings.to_string()).unwrap();
    (plugin, config)
}

fn refresh(dir: &Path, home: &Path, loads: bool) -> Vec<String> {
    graphicle::install::refresh_opencode_plugins_in(&[dir.to_path_buf()], home, || loads)
}

#[test]
fn the_plugin_serves_opencode_1_and_2() {
    let plugin = current_plugin();
    assert!(plugin.starts_with(graphicle::install::OPENCODE_PLUGIN_HEADER));
    assert!(plugin.contains("export default {"));
    assert!(plugin.contains("async setup(ctx)") && plugin.contains("async server({ directory })"));
    assert!(plugin.contains("id: `graphicle-${location.toString(16)}`"));
    assert!(plugin.contains("join(directory, \".graphicle\", \"graph.json\")"));
    assert!(graphicle::install::opencode_plugin_is_v1_only(V1_PLUGIN));
    assert!(!graphicle::install::opencode_plugin_is_v1_only(&plugin));
}

/// The install writes the plugin and drops the bare-path entry older releases
/// registered; other plugins and settings stay; a commented config is left as
/// it is with a line asking for the edit by hand.
#[test]
fn opencode_install_drops_the_stale_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (home, project) = (dir.path().join("home"), dir.path().join("proj"));
    fs::create_dir_all(&home).unwrap();
    let config = project.join(".opencode/opencode.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        json!({"model": "m", "plugin": [ENTRY, "other-plugin"], "plugins": [ENTRY]}).to_string(),
    )
    .unwrap();
    let o = options(&home, &project);
    graphicle::install::configure("opencode", &o, false).unwrap();
    assert_eq!(
        fs::read_to_string(project.join(ENTRY)).unwrap(),
        current_plugin()
    );
    assert_eq!(
        read(&config),
        json!({"model": "m", "plugin": ["other-plugin"]})
    );
    // A fresh project gets no opencode.json at all.
    let fresh = dir.path().join("fresh");
    graphicle::install::configure("opencode", &options(&home, &fresh), false).unwrap();
    assert!(fresh.join(ENTRY).is_file() && !fresh.join(".opencode/opencode.json").exists());

    let commented = format!("{{ // mine\n  \"plugin\": [\"{ENTRY}\"]\n}}");
    fs::write(&config, &commented).unwrap();
    let messages = graphicle::install::configure("opencode", &o, false).unwrap();
    assert_eq!(fs::read_to_string(&config).unwrap(), commented);
    assert!(
        messages
            .iter()
            .any(|m| m.contains(&format!("remove \"{ENTRY}\" from its plugin list by hand")))
    );

    graphicle::install::configure("opencode", &o, true).unwrap();
    assert!(!project.join(ENTRY).exists());
}

#[test]
fn a_v1_plugin_is_rewritten_once() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let project = dir.path().join("proj");
    fs::create_dir_all(&home).unwrap();
    let (plugin, config) = old_install(&project, V1_PLUGIN, true);
    let messages = refresh(&project, &home, true);
    assert_eq!(fs::read_to_string(&plugin).unwrap(), current_plugin());
    assert_eq!(read(&config), json!({"model": "some/model"}));
    let resolved = fs::canonicalize(&plugin).unwrap();
    assert_eq!(messages.len(), 1, "{messages:?}");
    let expected = format!("graphicle: refreshed {}", resolved.display());
    assert!(seps(&messages[0]).starts_with(&seps(&expected)));
    assert!(messages[0].contains("GRAPHICLE_NO_AUTO_REFRESH=1"));
    assert!(
        refresh(&project, &home, true).is_empty(),
        "a second run is silent"
    );
}

#[test]
fn a_stale_entry_beside_a_current_plugin_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let project = dir.path().join("proj");
    let (plugin, config) = old_install(&project, &current_plugin(), true);
    let before = fs::metadata(&plugin).unwrap().modified().unwrap();
    // OpenCode is only asked when an old plugin is found.
    let messages = graphicle::install::refresh_opencode_plugins_in(
        std::slice::from_ref(&project),
        &home,
        || panic!("no old plugin here, the version does not matter"),
    );
    assert_eq!(fs::metadata(&plugin).unwrap().modified().unwrap(), before);
    assert_eq!(read(&config), json!({"model": "some/model"}));
    let resolved = fs::canonicalize(&config).unwrap();
    assert_eq!(
        messages
            .iter()
            .map(String::as_str)
            .map(seps)
            .collect::<Vec<_>>(),
        [seps(&format!(
            "graphicle: removed the stale graphicle entry from {}; set GRAPHICLE_NO_AUTO_REFRESH=1 to disable",
            resolved.display()
        ))]
    );
    assert!(refresh(&project, &home, true).is_empty());
}

#[test]
fn a_commented_config_gets_the_hint_once() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let project = dir.path().join("proj");
    let (plugin, config) = old_install(&project, V1_PLUGIN, true);
    let commented = format!("{{ // mine\n  \"plugin\": [\"{ENTRY}\"]\n}}");
    fs::write(&config, &commented).unwrap();
    let messages = refresh(&project, &home, true);
    assert_eq!(fs::read_to_string(&plugin).unwrap(), current_plugin());
    assert_eq!(fs::read_to_string(&config).unwrap(), commented);
    assert!(messages[0].starts_with("graphicle: refreshed"));
    assert!(messages[1].contains(&format!("remove \"{ENTRY}\" from its plugin list by hand")));
    assert!(
        refresh(&project, &home, true).is_empty(),
        "no reminder on later runs"
    );
}

#[test]
fn plugins_the_refresh_must_leave_alone() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    // A current plugin without an entry.
    let current = dir.path().join("current");
    let (plugin, config) = old_install(&current, &current_plugin(), false);
    let before = fs::read_to_string(&config).unwrap();
    assert!(refresh(&current, &home, true).is_empty());
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
    assert_eq!(fs::read_to_string(&plugin).unwrap(), current_plugin());
    // A plugin the engine did not write.
    let foreign = dir.path().join("foreign");
    let mine = "// my own plugin\nexport const Mine = async () => ({});\n";
    let (plugin, config) = old_install(&foreign, mine, true);
    let before = fs::read_to_string(&config).unwrap();
    assert!(refresh(&foreign, &home, true).is_empty());
    assert_eq!(fs::read_to_string(&plugin).unwrap(), mine);
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
    // An OpenCode too old for the new plugin keeps its plugin.
    let old = dir.path().join("old");
    let (plugin, config) = old_install(&old, V1_PLUGIN, true);
    let before = fs::read_to_string(&config).unwrap();
    assert!(refresh(&old, &home, false).is_empty());
    assert_eq!(fs::read_to_string(&plugin).unwrap(), V1_PLUGIN);
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
    // No plugin: nothing is created, not even a lock file.
    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    assert!(refresh(&empty, &home, true).is_empty());
    assert!(!empty.join(".opencode").exists());
    assert!(!home.join(".cache/graphicle/skill-locks").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_plugin_is_rewritten_through_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let shared = dir.path().join("dotfiles/graphicle.js");
    fs::create_dir_all(shared.parent().unwrap()).unwrap();
    fs::write(&shared, V1_PLUGIN).unwrap();
    let project = dir.path().join("project");
    let plugin = project.join(ENTRY);
    fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&shared, &plugin).unwrap();
    refresh(&project, &home, true);
    assert!(plugin.is_symlink());
    assert_eq!(fs::read_to_string(&shared).unwrap(), current_plugin());
}

#[test]
fn the_installed_opencode_version_decides() {
    for (output, loads) in [
        ("1.3.3\n", false),
        ("1.3.4\n", true),
        ("1.18.34\n", true),
        ("opencode v2.0.22\n", true),
        ("0.15.31\n", false),
        ("", false),
        ("command not understood\n", false),
    ] {
        assert_eq!(
            graphicle::install::opencode_version_loads_current_plugin(output),
            loads,
            "{output:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// benchmark (0.9.87, #4347)
// ---------------------------------------------------------------------------

fn write_graph(path: &Path, nodes: Vec<Value>, links: Vec<Value>) {
    fs::write(
        path,
        json!({"directed": true, "multigraph": false, "graph": {}, "nodes": nodes, "links": links})
            .to_string(),
    )
    .unwrap();
}

fn query_answer(path: &Path, question: &str, budget: i64) -> String {
    let g = graphicle::serve::load_graph(path).unwrap();
    let index = graphicle::serve_search::SearchIndex::new(&g);
    graphicle::serve_text::query_text(
        &g,
        &index,
        question,
        graphicle::serve_text::QueryOptions {
            mode: "bfs",
            depth: 3,
            budget,
            filters: &[],
            path: None,
            seeds: None,
            seed_root: None,
        },
    )
    .unwrap()
}

/// The benchmark counts the answer query_graph returns: the same text, cut at
/// the same budget; a question with no match is left out.
#[test]
fn benchmark_counts_the_real_query_answer() {
    let dir = tempfile::tempdir().unwrap();
    let small = dir.path().join("graph.json");
    write_graph(
        &small,
        vec![
            json!({"id": "auth", "label": "authentication()", "source_file": "auth.py", "source_location": "L1"}),
            json!({"id": "login", "label": "login()", "source_file": "views.py", "source_location": "L9"}),
            json!({"id": "user", "label": "get_user()", "source_file": "db.py", "source_location": "L3"}),
        ],
        vec![
            json!({"source": "login", "target": "auth", "relation": "calls", "confidence": "EXTRACTED"}),
            json!({"source": "auth", "target": "user", "relation": "calls", "confidence": "EXTRACTED"}),
        ],
    );
    let question = "how does authentication work";
    let result = graphicle::benchmark::run_benchmark(
        Some(small.as_path()),
        Some(10_000),
        Some(&[question.to_string()][..]),
        None,
    )
    .unwrap();
    assert_eq!(
        result["per_question"][0]["query_tokens"],
        json!(graphicle::benchmark::estimate_tokens(&query_answer(
            &small, question, 2000
        )))
    );
    let result = graphicle::benchmark::run_benchmark(
        Some(small.as_path()),
        Some(10_000),
        Some(&["xyzzy plugh".to_string(), "authentication".to_string()][..]),
        None,
    )
    .unwrap();
    let asked: Vec<&Value> = result["per_question"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["question"])
        .collect();
    assert_eq!(asked, [&json!("authentication")]);

    let hub = dir.path().join("hub.json");
    let mut nodes = vec![
        json!({"id": "auth", "label": "authentication()", "source_file": "auth.py", "source_location": "L1"}),
    ];
    let mut links = vec![];
    for i in 0..400 {
        nodes.push(json!({"id": format!("n{i}"), "label": format!("helper_function_number_{i}_with_a_long_name()"),
                          "source_file": format!("pkg/module_{i}.py"), "source_location": format!("L{i}")}));
        links.push(json!({"source": "auth", "target": format!("n{i}"), "relation": "calls", "confidence": "EXTRACTED"}));
    }
    write_graph(&hub, nodes, links);
    let g = graphicle::serve::load_graph(&hub).unwrap();
    let index = graphicle::serve_search::SearchIndex::new(&g);
    let tokens = |budget| {
        graphicle::benchmark::query_subgraph_tokens(&g, &index, "authentication", 3, budget)
            .unwrap()
    };
    assert!(query_answer(&hub, "authentication", 2000).contains("[!] TRUNCATED"));
    let full =
        graphicle::benchmark::estimate_tokens(&query_answer(&hub, "authentication", 1_000_000));
    assert!(tokens(2000) <= 2000 && 2000 < full);
    assert!(tokens(500) < tokens(2000));
    assert_eq!(
        graphicle::benchmark::query_subgraph_tokens(&g, &index, "xyzzy plugh", 3, 2000).unwrap(),
        0
    );
    for bad in ["missing.json", "graph.txt"] {
        assert!(
            graphicle::benchmark::run_benchmark(
                Some(dir.path().join(bad).as_path()),
                None,
                None,
                None
            )
            .is_err()
        );
    }
}
