//! B10: `query::snapshot` reads the projected code-graph episode when one is
//! recorded and falls back to the legacy `structure::graph` adapter otherwise.
//!
//! The equivalence test pins the *documented* differences between the two
//! builders on a synced and projected fixture rather than forcing identity:
//! - file set and legacy symbol set are contained in the episode's nodes;
//! - `imports` file edges are identical;
//! - legacy-only edge kinds are exactly `document_link` (markdown links);
//! - episode-only edge kinds are `calls` (legacy kept those in `symbol_calls`)
//!   and `references`;
//! - `symbol_calls` (caller → callee by path and name) are identical.

use graphicle_runtime::query::{self, Snapshot};
use graphicle_runtime::store::{Store, decode, repo, rows, s};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tempfile::TempDir;

const A: &str = "def alpha():\n    return 1\n\ndef beta():\n    return alpha() + 1\n";
const B: &str = "from a import alpha, beta\n\ndef run():\n    return alpha() + beta()\n";
const C: &str = "import b\n\ndef main():\n    return b.run()\n";
const README: &str = "# Fixture\n\nSee [b](b.py) for the entry point.\n";

#[test]
fn core_build_combines_projected_graph_and_reviewed_memory_then_retracts_stale_memory() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B), ("README.md", README)]);
    let before = f.context(json!({"question":"alpha returns one","max_tokens":4000}));
    assert_eq!(before["code_graph"]["status"], "no_episode");
    let projected = f.project();
    let note = f
        .store
        .note(
            &f.workspace,
            &f.key,
            &f.args(json!({
        "kind":"decision","title":"alpha returns one","body":"alpha returns one",
        "evidence":[{"id":"file:a.py","quote":"return 1"}]})),
        )
        .unwrap();
    assert_eq!(note["state"], "reviewed");
    let packet = f.context(json!({"question":"alpha returns one","max_tokens":4000}));
    assert_eq!(packet["mechanisms"]["graph_source"], "episode", "{packet}");
    assert_eq!(packet["mechanisms"]["graph_episode"], projected["episode"]);
    assert_eq!(packet["mechanisms"]["graph_searched"], true, "{packet}");
    assert!(
        packet["mechanisms"]["static_relationship_edges"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        packet["mechanisms"]["reviewed_memory_included"], 1,
        "{packet}"
    );
    assert!(!packet["evidence"].as_array().unwrap().is_empty());

    f.sync(&[
        ("a.py", "def alpha():\n    return 7\n"),
        ("b.py", B),
        ("README.md", README),
    ]);
    f.project();
    let updated = f.context(json!({"question":"alpha returns one","max_tokens":4000}));
    assert_eq!(
        updated["mechanisms"]["reviewed_memory_included"], 0,
        "{updated}"
    );
    assert_eq!(updated["mechanisms"]["graph_searched"], true);
}

#[test]
fn compiled_capabilities_match_available_tools_and_native_operations() {
    let pro = pro_edition();
    let capabilities = graphicle_runtime::core_capabilities::capabilities();
    assert_eq!(capabilities["change_impact"], pro);
    assert_eq!(capabilities["execution_coverage"], pro);
    let tools = graphicle_runtime::api::mcp_tools(false);
    assert_eq!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "change_impact"),
        pro
    );
    let empty = json!({"directed":true,"multigraph":false,"graph":{},"nodes":[],"links":[]});
    let result = graphicle_runtime::native("graph.impact.diff", json!({"base":empty,"head":empty}));
    assert_eq!(result.is_ok(), pro);
    if !pro {
        // Core answers every overridable operation with 501, but only after
        // authorizing the caller.
        let f = Fixture::new(&[("a.py", A)]);
        let app = graphicle_runtime::api::App::new(
            f.store.clone(),
            graphicle_runtime::model::Config::default(),
            vec![],
        )
        .unwrap();
        let args = json!({"repository_id": f.repo});
        for operation in ["impact.report", "coverage.upload", "graph.impact"] {
            assert_eq!(
                graphicle_runtime::api::command(&app, &f.workspace, &f.key, operation, &args)
                    .unwrap_err()
                    .status,
                axum::http::StatusCode::NOT_IMPLEMENTED,
                "{operation}"
            );
            assert_eq!(
                graphicle_runtime::api::command(&app, &f.workspace, "invalid", operation, &args)
                    .unwrap_err()
                    .status,
                axum::http::StatusCode::FORBIDDEN,
                "{operation}"
            );
        }
    }
}

/// Installs the edition under test before a fixture is built: nothing in this
/// Core test binary. An extension crate can re-run this suite from its own
/// test binary, whose root `edition_setup` installs its extensions (`crate::`
/// names the root of whichever test binary the suite is compiled into).
#[allow(dead_code)]
fn edition_setup() {}

/// Whether the edition under test is the `pro` edition (`build.info` edition).
fn pro_edition() -> bool {
    crate::edition_setup();
    graphicle_runtime::core_capabilities::capabilities()["edition"] == "pro"
}

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Graph source").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"gsrc","name":"Graph source"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        let f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
        };
        f.sync(files);
        f
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn sync(&self, files: &[(&str, &str)]) {
        let state = self
            .store
            .project_status(&self.workspace, &self.key, &self.args(json!({})))
            .unwrap();
        let begin=self.store.sync_prepare(&self.workspace,&self.key,&self.args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()}))).unwrap();
        self.store.sync_upload(&self.workspace,&self.key,&self.args(json!({"sync_id":begin["sync_id"],"files":files.iter().map(|(name,text)|json!({"path":name,"content":text})).collect::<Vec<_>>()}))).unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &self.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    fn project(&self) -> Value {
        self.store
            .graph_project(&self.workspace, &self.key, &self.repo)
            .unwrap()
    }
    fn snapshot(&self) -> Snapshot {
        self.store
            .authorized(&self.workspace, &self.key, "read", |db, p| {
                let repository = repo(db, p, &self.repo)?;
                query::snapshot(db, &repository, p)
            })
            .unwrap()
    }
    fn legacy(&self) -> Value {
        self.store
            .authorized(&self.workspace, &self.key, "read", |db, p| {
                let repository = repo(db, p, &self.repo)?;
                query::legacy_graph(db, &repository, p)
            })
            .unwrap()
    }
    fn episode(&self) -> String {
        let db = self.store.connect().unwrap();
        graphicle_runtime::graph_projection::current_episode(&db, &self.repo).unwrap()
    }
    /// `(relpath, symbol name)` for every legacy analysis symbol, plus
    /// `(relpath, "")` per analysed file.
    fn legacy_nodes(&self) -> BTreeSet<(String, String)> {
        let db = self.store.connect().unwrap();
        let mut out = BTreeSet::new();
        for mut row in rows(
            &db,
            "SELECT p.path,p.data FROM repo_parses p JOIN repo_heads h ON h.repository_id=p.repository_id AND h.path=p.path AND h.sha256=p.sha256 WHERE p.repository_id=?",
            [&self.repo],
        )
        .unwrap()
        {
            decode(&mut row, "data").unwrap();
            let path = s(&row, "path").to_string();
            out.insert((path.clone(), String::new()));
            for symbol in row["data"]["symbols"].as_array().into_iter().flatten() {
                out.insert((path.clone(), s(symbol, "name").to_string()));
            }
        }
        out
    }
    /// `(relpath, qualified name)` for every projected node anchored to a
    /// file; file nodes carry an empty name.
    fn episode_nodes(&self) -> BTreeSet<(String, String)> {
        let db = self.store.connect().unwrap();
        let episode = self.episode();
        let handle =
            graphicle_runtime::graph_projection::resident(&db, &self.repo, &episode).unwrap();
        handle
            .nodes()
            .iter()
            .filter_map(|node| {
                let path = node.attributes.get("relpath")?.as_str()?.to_string();
                let name = if node.attributes["kind"] == "file" {
                    String::new()
                } else {
                    node.attributes["qualified_name"]
                        .as_str()
                        .or(node.attributes["label"].as_str())?
                        .to_string()
                };
                Some((path, name))
            })
            .collect()
    }
    fn context(&self, args: Value) -> Value {
        self.store
            .project_context(&self.workspace, &self.key, &self.args(args))
            .unwrap()
    }
}

type FileEdge = (String, String, String);

fn file_edges(graph: &Value) -> BTreeSet<FileEdge> {
    graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                s(e, "source").to_string(),
                s(e, "target").to_string(),
                s(e, "kind").to_string(),
            )
        })
        .collect()
}

fn of_kind(edges: &BTreeSet<FileEdge>, kind: &str) -> BTreeSet<(String, String)> {
    edges
        .iter()
        .filter(|(_, _, k)| k == kind)
        .map(|(a, b, _)| (a.clone(), b.clone()))
        .collect()
}

fn symbol_calls(graph: &Value) -> BTreeSet<(String, String, String, String)> {
    graph["symbol_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                s(c, "path").to_string(),
                s(c, "caller").to_string(),
                s(c, "callee_path").to_string(),
                s(c, "callee").to_string(),
            )
        })
        .collect()
}

fn evidence_ids(packet: &Value) -> BTreeSet<String> {
    packet["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| s(e, "id").to_string())
        .collect()
}

#[test]
fn legacy_adapter_and_projected_episode_differ_only_as_documented() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B), ("c.py", C), ("README.md", README)]);
    f.project();
    let legacy = f.legacy();
    let projected = f.snapshot().graph;
    assert_eq!(legacy["graph_source"], "legacy");
    assert_eq!(projected["graph_source"], "episode", "{projected}");
    assert_eq!(projected["episode"], json!(f.episode()));

    // Nodes: every analysed file and every legacy symbol is a projected node.
    let legacy_nodes = f.legacy_nodes();
    let episode_nodes = f.episode_nodes();
    let missing: Vec<_> = legacy_nodes.difference(&episode_nodes).collect();
    assert!(
        missing.is_empty(),
        "legacy nodes absent from the episode: {missing:?}\nlegacy={legacy_nodes:?}\nepisode={episode_nodes:?}"
    );
    let legacy_files: BTreeSet<_> = legacy_nodes
        .iter()
        .filter(|(_, n)| n.is_empty())
        .cloned()
        .collect();
    let episode_files: BTreeSet<_> = episode_nodes
        .iter()
        .filter(|(_, n)| n.is_empty())
        .cloned()
        .collect();
    assert_eq!(legacy_files, episode_files, "file node sets differ");
    assert_eq!(
        legacy["coverage"]["files"], projected["coverage"]["files"],
        "coverage.files: legacy={} projected={}",
        legacy["coverage"], projected["coverage"]
    );

    // Edges by (source, target, kind).
    let legacy_edges = file_edges(&legacy);
    let episode_edges = file_edges(&projected);
    assert_eq!(
        of_kind(&legacy_edges, "imports"),
        of_kind(&episode_edges, "imports"),
        "import edges must agree\nlegacy={legacy_edges:?}\nepisode={episode_edges:?}"
    );
    let legacy_only_kinds: BTreeSet<_> = legacy_edges
        .difference(&episode_edges)
        .map(|(_, _, k)| k.clone())
        .collect();
    assert!(
        legacy_only_kinds.is_subset(&BTreeSet::from(["document_link".to_string()])),
        "undocumented legacy-only edge kinds: {legacy_only_kinds:?}\nlegacy={legacy_edges:?}\nepisode={episode_edges:?}"
    );
    assert!(
        legacy_only_kinds.contains("document_link"),
        "the fixture's README link is the documented legacy-only edge: {legacy_edges:?}"
    );
    let episode_only_kinds: BTreeSet<_> = episode_edges
        .difference(&legacy_edges)
        .map(|(_, _, k)| k.clone())
        .collect();
    assert!(
        episode_only_kinds.is_subset(&BTreeSet::from([
            "calls".to_string(),
            "references".to_string()
        ])),
        "undocumented episode-only edge kinds: {episode_only_kinds:?}\nlegacy={legacy_edges:?}\nepisode={episode_edges:?}"
    );
    // The episode's file-level `calls` edges are exactly the cross-file pairs
    // the legacy adapter reported under `symbol_calls`.
    let legacy_cross_file_calls: BTreeSet<_> = symbol_calls(&legacy)
        .into_iter()
        .filter(|(from, _, to, _)| from != to)
        .map(|(from, _, to, _)| (from, to))
        .collect();
    assert_eq!(
        of_kind(&episode_edges, "calls"),
        legacy_cross_file_calls,
        "episode call edges must be the legacy cross-file symbol calls"
    );
    for edge in projected["edges"].as_array().unwrap() {
        assert_eq!(edge["basis"], query::EPISODE_BASIS, "{edge}");
        assert!(edge["line"].as_u64().is_some_and(|l| l >= 1), "{edge}");
        assert!(edge["derivation"].is_string(), "{edge}");
    }

    // Symbol calls agree exactly on this fixture.
    assert_eq!(
        symbol_calls(&legacy),
        symbol_calls(&projected),
        "symbol_calls differ\nlegacy={}\nprojected={}",
        legacy["symbol_calls"],
        projected["symbol_calls"]
    );
    assert_eq!(
        legacy["coverage"]["symbols"], projected["coverage"]["symbols"],
        "symbol counts: legacy={} projected={}",
        legacy["coverage"], projected["coverage"]
    );
}

#[test]
fn snapshot_prefers_recorded_episode_and_reports_graph_source() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B), ("c.py", C)]);
    // No episode recorded yet: the legacy adapter feeds the snapshot.
    let before = f.snapshot().graph;
    assert_eq!(before["graph_source"], "legacy", "{before}");
    assert!(before.get("episode").is_none());
    let packet = f.context(json!({"question":"where is main defined"}));
    assert_eq!(packet["graph_source"], "legacy", "{packet}");
    assert_eq!(packet["mechanisms"]["graph_source"], "legacy");
    assert_eq!(packet["code_graph"]["status"], "no_episode");

    f.project();
    let after = f.snapshot().graph;
    assert_eq!(after["graph_source"], "episode", "{after}");
    assert_eq!(after["episode"], json!(f.episode()));
    assert!(after["edges"].as_array().is_some_and(|e| !e.is_empty()));
    let packet = f.context(json!({"question":"where is main defined"}));
    assert_eq!(packet["graph_source"], "episode", "{packet}");
    assert_eq!(packet["mechanisms"]["graph_source"], "episode");
    assert_eq!(packet["mechanisms"]["graph_episode"], json!(f.episode()));
    assert!(
        packet["mechanisms"]["static_relationship_edges"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "{}",
        packet["mechanisms"]
    );
    assert!(packet.get("graph_source_error").is_none(), "{packet}");

    // `project_status` exposes the same view.
    let status = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    assert_eq!(status["graph"]["graph_source"], "episode");
    assert_eq!(file_edges(&status["graph"]), file_edges(&after));

    // A new head set without a projection falls back until it is projected.
    f.sync(&[
        ("a.py", A),
        ("b.py", B),
        ("c.py", C),
        ("d.py", "import c\n"),
    ]);
    let stale = f.snapshot().graph;
    assert_eq!(stale["graph_source"], "legacy", "{stale}");
    f.project();
    let fresh = f.snapshot().graph;
    assert_eq!(fresh["graph_source"], "episode");
    assert!(
        file_edges(&fresh).contains(&("d.py".into(), "c.py".into(), "imports".into())),
        "{fresh}"
    );
}

#[test]
fn episode_edges_pull_neighbours_and_obey_the_graph_switch() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B), ("c.py", C)]);
    f.project();
    // "main" is lexically present only in c.py; b.py is reachable through the
    // episode's c.py → b.py edge (static expansion) and the arm's one hop.
    let on = f.context(
        json!({"question":"main","graph_retrieval":true,"limit":4,"expand_context":false}),
    );
    assert_eq!(on["graph_source"], "episode");
    assert!(
        evidence_ids(&on).contains("file:b.py"),
        "episode edge must pull the imported neighbour: {on}"
    );
    let off = f.context(
        json!({"question":"main","graph_retrieval":false,"limit":4,"expand_context":false}),
    );
    assert_eq!(off["graph_source"], "episode");
    assert_eq!(off["mechanisms"]["graph_retrieval_enabled"], false);
    assert_eq!(off["mechanisms"]["static_relationship_edges"], 0);
    assert_eq!(off["code_graph"]["status"], "disabled");
    assert!(
        !evidence_ids(&off).contains("file:b.py"),
        "graph off must remove the neighbour reached only through an edge: {off}"
    );
}

#[test]
fn static_edge_and_arm_evidence_never_duplicate_a_window() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B), ("c.py", C)]);
    f.project();
    for question in ["where is beta defined", "main", "run alpha beta"] {
        let packet = f.context(json!({"question":question,"limit":8}));
        assert_eq!(
            packet["code_graph"]["status"], "fused",
            "{}",
            packet["code_graph"]
        );
        assert!(
            packet["code_graph"]["deduplicated"].is_u64(),
            "{}",
            packet["code_graph"]
        );
        let mut windows = BTreeSet::new();
        let mut stable_ids = BTreeSet::new();
        for window in packet["evidence"].as_array().unwrap() {
            let key = (
                s(window, "id").to_string(),
                window["byte_start"].as_u64().unwrap(),
                window["byte_end"].as_u64().unwrap(),
            );
            assert!(
                windows.insert(key.clone()),
                "duplicate window {key:?}: {packet}"
            );
            if let Some(id) = window["code_graph"]["stable_id"].as_str() {
                assert!(
                    stable_ids.insert(id.to_string()),
                    "node {id} cited twice: {packet}"
                );
            }
        }
        // Windows of one document never overlap: a node reached by a static
        // edge and by the arm is packed once.
        let mut by_document: Vec<_> = windows.into_iter().collect();
        by_document.sort();
        for pair in by_document.windows(2) {
            if pair[0].0 == pair[1].0 {
                assert!(
                    pair[0].2 <= pair[1].1,
                    "overlapping windows {:?} / {:?}: {packet}",
                    pair[0],
                    pair[1]
                );
            }
        }
        let sources = packet["sources"].as_array().unwrap();
        let identities: BTreeSet<_> = sources
            .iter()
            .map(|s| s["id"].to_string() + &s["byte_start"].to_string())
            .collect();
        assert_eq!(identities.len(), sources.len(), "{packet}");
    }
}
