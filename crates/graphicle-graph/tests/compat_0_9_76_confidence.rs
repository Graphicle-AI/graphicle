// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
//! Graphify 0.9.76 (b8fcd29): every INFERRED edge carries one of the documented
//! discrete scores. The cross-repo resolvers and the member-call resolvers used
//! to hardcode 0.8 or 0.9, neither of which is on the rubric.
use graphicle::api::{ExtractOptions, extract};
use graphicle::graph::Graph;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const RUBRIC: [f64; 5] = [0.55, 0.65, 0.75, 0.85, 0.95];

fn on_rubric(score: f64) -> bool {
    RUBRIC.iter().any(|r| (r - score).abs() < 1e-9)
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }
    fn extract(&self, files: &[(&str, &str)]) -> Value {
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|(rel, text)| {
                let path = self.root.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, text).unwrap();
                path
            })
            .collect();
        let options = ExtractOptions {
            root: Some(self.root.clone()),
            cache_root: Some(self.root.join("cache")),
            parallel: false,
            ..Default::default()
        };
        extract(&paths, &options).unwrap()
    }
}

fn edges(result: &Value) -> &[Value] {
    result["edges"].as_array().unwrap()
}

fn label_of(result: &Value) -> HashMap<String, String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?.into(), n["label"].as_str()?.into())))
        .collect()
}

fn assert_inferred_on_rubric(result: &Value) {
    for edge in edges(result) {
        if edge["confidence"] == "INFERRED" {
            let score = edge["confidence_score"].as_f64();
            assert!(
                score.is_some_and(on_rubric),
                "INFERRED edge off the rubric: {edge}"
            );
        }
    }
}

// --- source guard -----------------------------------------------------------

/// Literal scores at emission sites: object keys inside `json!`, index
/// assignments, map inserts and both arms of an `if` expression.
fn emitted_scores(source: &str) -> Vec<(usize, f64)> {
    let num = r"(\d+(?:\.\d+)?)";
    let patterns = [
        format!(
            r#""confidence_score"\s*:\s*if\b[^{{]*\{{\s*{num}\s*\}}\s*else\s*\{{\s*{num}\s*\}}"#
        ),
        format!(r#""confidence_score"\s*:\s*{num}"#),
        format!(r#"\["confidence_score"\]\s*=\s*json!\(\s*{num}\s*\)"#),
        format!(r#"insert\(\s*"confidence_score"[^,]*,\s*json!\(\s*{num}\s*\)"#),
    ]
    .map(|p| Regex::new(&p).unwrap());
    let mut out = vec![];
    for (i, line) in source.lines().enumerate() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        for re in &patterns {
            for caps in re.captures_iter(code) {
                for m in caps.iter().skip(1).flatten() {
                    out.push((i + 1, m.as_str().parse().unwrap()));
                }
            }
        }
    }
    out
}

/// Drop trailing `#[cfg(test)] mod tests { .. }` blocks: fixtures there are
/// test data, not emission sites.
fn production_code(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let cut = (0..lines.len())
        .find(|&i| {
            lines[i].trim() == "#[cfg(test)]"
                && lines[i + 1..]
                    .iter()
                    .find(|l| !l.trim().is_empty())
                    .is_some_and(|l| l.trim_start().starts_with("mod "))
        })
        .unwrap_or(lines.len());
    lines[..cut].join("\n")
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_source_file_hardcodes_an_off_rubric_score() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![];
    rust_sources(&src, &mut files);
    files.sort();
    let mut violations = vec![];
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        for (line, score) in emitted_scores(&production_code(&text)) {
            let extracted_or_ambiguous = score == 1.0 || score == 0.2;
            if !on_rubric(score) && !extracted_or_ambiguous {
                let rel = path.strip_prefix(&src).unwrap().display();
                violations.push(format!("{rel}:{line}: {score}"));
            }
        }
    }
    assert!(files.len() > 50, "scanned too few files: {}", files.len());
    assert!(
        violations.is_empty(),
        "Off-rubric emitted scores:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_guard_recognizes_every_rust_spelling() {
    for source in [
        r#"json!({"relation": "calls", "confidence_score": 0.8})"#,
        r#"json!({"confidence_score":0.8,"weight":1.0})"#,
        r#"edge["confidence_score"] = json!(0.8);"#,
        r#"attrs.insert("confidence_score".into(), json!(0.8));"#,
        r#""confidence_score":if exact{1.0}else{0.8},"#,
        r#""confidence_score": if exact { 0.8 } else { 0.85 },"#,
    ] {
        let scores: Vec<f64> = emitted_scores(source).into_iter().map(|(_, s)| s).collect();
        assert!(scores.contains(&0.8), "{source} -> {scores:?}");
    }
}

#[test]
fn the_guard_ignores_comments_and_score_consumers() {
    let source = concat!(
        "// \"confidence_score\": 0.8\n",
        "let s = e.get(\"confidence_score\").unwrap_or(&json!(0.5));\n",
        "let keep = float(&e[\"confidence_score\"], 1.0) >= 0.85;\n",
    );
    assert_eq!(emitted_scores(source), vec![]);
    let module = "fn f() {}\n#[cfg(test)]\nmod tests {\n    // {\"confidence_score\": 0.5}\n}\n";
    assert_eq!(production_code(module), "fn f() {}");
}

// --- export default ---------------------------------------------------------

#[test]
fn an_unscored_inferred_edge_exports_the_rubric_default() {
    let fx = Fixture::new();
    let out = fx.root.join("graph.json");
    let node =
        |id: &str| json!({"id": id, "label": id, "file_type": "code", "source_file": "a.py"});
    let link = |source: &str, target: &str, relation: &str, c: &str| {
        json!({"source": source, "target": target, "relation": relation, "confidence": c,
               "source_file": "a.py"})
    };
    let mut extraction = json!({
        "directed": true,
        "nodes": [node("a"), node("b"), node("c")],
        "links": [link("a", "b", "calls", "INFERRED"), link("b", "c", "imports", "EXTRACTED"),
                  link("c", "a", "uses", "AMBIGUOUS")],
    });
    let mut d = graphicle::build::Diagnostics::default();
    let g = graphicle::build::build_from_json(&mut extraction, true, None, &mut d).unwrap();
    let communities: graphicle::cluster::Communities = vec![];
    let options = graphicle::export::JsonOptions::default();
    assert!(graphicle::export::to_json(&g, &communities, &out, &options, &mut d).unwrap());
    let data: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let score = |confidence: &str| {
        data["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["confidence"] == confidence)
            .map(|e| e["confidence_score"].as_f64().unwrap())
            .unwrap()
    };
    assert_eq!(score("INFERRED"), 0.55);
    assert_eq!(score("EXTRACTED"), 1.0);
    assert_eq!(score("AMBIGUOUS"), 0.2);
}

// --- extraction ---------------------------------------------------------------

#[test]
fn swift_cross_file_member_calls_score_on_the_rubric() {
    let fx = Fixture::new();
    let result = fx.extract(&[
        (
            "src/Models/SessionViewModel.swift",
            "class SessionViewModel {\n    func update() {}\n}\n",
        ),
        (
            "src/Services/NetworkService.swift",
            "class NetworkService {\n    func fetch() {}\n}\n",
        ),
        (
            "src/Core/SessionType.swift",
            "enum SessionType {\n    static func staticMethod() {}\n}\n",
        ),
        (
            "src/Core/Singleton.swift",
            "class Singleton {\n    static let shared = Singleton()\n    func method() {}\n}\n",
        ),
        (
            "src/Views/HomeView.swift",
            concat!(
                "class HomeView {\n",
                "    let vm = SessionViewModel()\n",
                "    var svc: NetworkService\n\n",
                "    func go() {\n",
                "        vm.update()\n",
                "        SessionType.staticMethod()\n",
                "        Singleton.shared.method()\n",
                "        self.svc.fetch()\n",
                "    }\n",
                "}\n",
            ),
        ),
    ]);
    let labels = label_of(&result);
    let label = |v: &Value| labels.get(v.as_str().unwrap()).cloned().unwrap_or_default();
    let mut seen = vec![];
    for edge in edges(&result) {
        if edge["relation"] != "calls" {
            continue;
        }
        let target = label(&edge["target"]);
        match target.as_str() {
            ".update()" | ".fetch()" => {
                assert_eq!(edge["confidence"], "INFERRED", "{edge}");
                assert_eq!(edge["confidence_score"], json!(0.85), "{edge}");
                seen.push(target);
            }
            ".staticMethod()" | ".method()" => {
                assert_eq!(edge["confidence"], "EXTRACTED", "{edge}");
                assert_eq!(edge["confidence_score"], json!(1.0), "{edge}");
                seen.push(target);
            }
            _ => {}
        }
    }
    seen.sort();
    assert_eq!(
        seen,
        [".fetch()", ".method()", ".staticMethod()", ".update()"]
    );
    assert_inferred_on_rubric(&result);
}

#[test]
fn swift_protocol_dispatch_edges_score_on_the_rubric() {
    let fx = Fixture::new();
    let result = fx.extract(&[
        (
            "Core/Store.swift",
            "protocol Store {\n    func fetch() async throws -> [Item]\n}\nstruct Item { let id: Int }\n",
        ),
        (
            "Services/RemoteStore.swift",
            concat!(
                "struct RemoteStore: Store {\n",
                "    func fetch() async throws -> [Item] { try decode() }\n",
                "    func decode() throws -> [Item] { [] }\n",
                "}\n",
            ),
        ),
        (
            "App/ItemModel.swift",
            concat!(
                "final class ItemModel {\n",
                "    private let store: Store\n",
                "    init(store: Store) { self.store = store }\n",
                "    func load() async throws -> [Item] { try await store.fetch() }\n",
                "}\n",
            ),
        ),
    ]);
    let dispatch: Vec<&Value> = edges(&result)
        .iter()
        .filter(|e| e["relation"] == "dispatches_to")
        .collect();
    assert!(!dispatch.is_empty());
    for edge in dispatch {
        assert_eq!(edge["confidence"], "INFERRED");
        assert_eq!(edge["confidence_score"], json!(0.85));
    }
    assert_inferred_on_rubric(&result);
}

#[test]
fn csharp_interface_dispatch_edges_score_on_the_rubric() {
    let fx = Fixture::new();
    let result = fx.extract(&[
        ("IReport.cs", "public interface IReport { void Build(); }\n"),
        (
            "Report.cs",
            concat!(
                "public class Report : IReport {\n",
                "    public void Build() { Format(); }\n",
                "    public void Format() { }\n",
                "}\n",
            ),
        ),
        (
            "Runner.cs",
            concat!(
                "public class Runner {\n",
                "    private readonly IReport _report;\n",
                "    public Runner(IReport report) { _report = report; }\n",
                "    public void Go() { _report.Build(); }\n",
                "}\n",
            ),
        ),
    ]);
    let dispatch: Vec<&Value> = edges(&result)
        .iter()
        .filter(|e| e["relation"] == "dispatches_to")
        .collect();
    assert!(!dispatch.is_empty());
    for edge in dispatch {
        assert_eq!(edge["confidence"], "INFERRED");
        assert_eq!(edge["confidence_score"], json!(0.85));
    }
    assert_inferred_on_rubric(&result);
}

#[test]
fn member_calls_in_every_resolver_language_score_on_the_rubric() {
    let fx = Fixture::new();
    let result = fx.extract(&[
        ("ts/greeter.ts", "export class Greeter {\n  greet(): void {}\n}\n"),
        (
            "ts/app.ts",
            "import { Greeter } from './greeter';\nfunction run(g: Greeter) {\n  g.greet();\n}\n",
        ),
        (
            "java/Greeter.java",
            "public class Greeter {\n    public void greet() {}\n}\n",
        ),
        (
            "java/App.java",
            "public class App {\n    void run(Greeter g) {\n        g.greet();\n    }\n}\n",
        ),
        (
            "cs/Greeter.cs",
            "public class Greeter {\n    public void Greet() {}\n}\n",
        ),
        (
            "cs/App.cs",
            "public class App {\n    void Run(Greeter g) {\n        g.Greet();\n    }\n}\n",
        ),
        (
            "py/m.py",
            "def handler():\n    return 1\n\ndef register(cb):\n    return cb\n\ndef wire():\n    return register(handler)\n",
        ),
    ]);
    assert_inferred_on_rubric(&result);
    for edge in edges(&result) {
        assert!(
            edge["confidence"] != "INFERRED" || edge.get("confidence_score").is_some(),
            "edge would inherit the default instead of stating a score: {edge}"
        );
        if edge["confidence"] == "EXTRACTED"
            && let Some(score) = edge.get("confidence_score")
        {
            assert_eq!(score, &json!(1.0), "{edge}");
        }
    }
}

// --- merge-graphs passes -----------------------------------------------------

fn merged(nodes: Value, links: Value) -> Graph {
    let data = json!({"directed": false, "multigraph": false, "graph": {},
                      "nodes": nodes, "links": links});
    Graph::from_node_link(&data, "links").unwrap()
}

fn link_between<'a>(data: &'a Value, a: &str, b: &str) -> &'a Value {
    data["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| (e["source"] == a && e["target"] == b) || (e["source"] == b && e["target"] == a))
        .unwrap_or_else(|| panic!("no link between {a} and {b}"))
}

#[test]
fn a_cross_repo_member_call_scores_on_the_rubric() {
    let parked =
        json!([{"callee": "greet", "receiver_type": "Greeter", "lang": "java", "line": "L10"}]);
    let mut g = merged(
        json!([
            {"id": "a::app_run", "label": ".run()", "source_file": "src/App.java", "repo": "a",
             "metadata": {"unresolved_calls": parked}},
            {"id": "b::greeter", "label": "Greeter", "repo": "b", "_callable_class": true,
             "_callable": true, "source_file": "src/Greeter.java"},
            {"id": "b::greeter_greet", "label": ".greet()", "repo": "b",
             "source_file": "src/Greeter.java", "_callable": true},
        ]),
        json!([{"source": "b::greeter", "target": "b::greeter_greet", "relation": "method"}]),
    );
    assert_eq!(
        graphicle::cross_repo_calls::link_cross_repo_member_calls(&mut g).unwrap(),
        1
    );
    let data = g.to_node_link("links");
    let edge = link_between(&data, "a::app_run", "b::greeter_greet");
    assert_eq!(edge["relation"], "calls");
    assert_eq!(edge["confidence"], "INFERRED");
    assert_eq!(edge["confidence_score"], json!(0.85));
    assert_eq!(edge["context"], "cross_repo");
    assert_eq!(edge["source_location"], "L10");
}

#[test]
fn a_shared_type_link_scores_on_the_rubric() {
    let node = |repo: &str| {
        json!({"id": format!("{repo}::evt"), "label": "OrderPlaced", "source_file": "a.cs",
               "_callable_class": true, "_callable": true, "repo": repo,
               "metadata": {"namespace": "Contracts.Events"}})
    };
    let mut g = merged(json!([node("svc_a"), node("svc_b")]), json!([]));
    assert_eq!(
        graphicle::cross_repo_types::link_shared_type_declarations(&mut g).unwrap(),
        1
    );
    let data = g.to_node_link("links");
    let edge = link_between(&data, "svc_a::evt", "svc_b::evt");
    assert_eq!(edge["relation"], "same_type_as");
    assert_eq!(edge["confidence"], "INFERRED");
    assert_eq!(edge["confidence_score"], json!(0.85));
}
