//! WS-M: dependency index, transitive staleness, and freshness reporting
//! through the commit, connector, and revocation paths.
use graphicle_runtime::{
    invalidation::{self, CHUNK, DOCUMENT, EMBEDDING, FILE, GRAPH_NODE, MEMORY, REPORT, SUMMARY},
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const A_V1: &str = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n";
const A_V2: &str = "def alpha():\n    return 1\n";
const B: &str = "from a import alpha, beta\n\ndef run():\n    return alpha() + beta()\n";
const C: &str = "def gamma():\n    return 3\n";

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
        let ws = store.create_workspace("Invalidation").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"inv","name":"Invalidated sources"}),
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
    fn sync(&self, files: &[(&str, &str)]) -> Value {
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
            .unwrap()
    }
    fn db(&self) -> rusqlite::Connection {
        self.store.connect().unwrap()
    }
    fn stale(&self, kind: &str) -> Vec<String> {
        let db = self.db();
        let mut statement = db
            .prepare("SELECT id FROM gf_stale WHERE workspace=? AND kind=? ORDER BY id")
            .unwrap();
        statement
            .query_map(rusqlite::params![self.workspace, kind], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    fn freshness(&self) -> invalidation::Freshness {
        invalidation::freshness(&self.db(), &self.workspace).unwrap()
    }
    /// `graph.impact` through the operation table, as HTTP, MCP and
    /// `graphicle call` run it: served by an installed extension (an extension),
    /// otherwise 501 after authorization (Core).
    fn graph_impact(&self, mut args: Value) -> graphicle_runtime::Result<Value> {
        args["repository_id"] = json!(self.repo);
        let app = graphicle_runtime::api::App::new(
            self.store.clone(),
            graphicle_runtime::model::Config::default(),
            vec![],
        )
        .unwrap();
        graphicle_runtime::api::command(&app, &self.workspace, &self.key, "graph.impact", &args)
    }
}

#[test]
fn recorded_dependencies_invalidate_transitively_and_report_freshness() {
    let f = Fixture::new(&[("c.py", C)]);
    let db = f.db();
    let ws = f.workspace.as_str();
    // A synthetic file: committed files already own chunk dependencies.
    let file = invalidation::file_id(&f.repo, "virtual.py");
    record(&db, ws, (CHUNK, "chunk-1"), (FILE, &file));
    record(&db, ws, (EMBEDDING, "emb-1"), (CHUNK, "chunk-1"));
    record(&db, ws, (GRAPH_NODE, "node-1"), (FILE, &file));
    record(&db, ws, (SUMMARY, "sum-1"), (GRAPH_NODE, "node-1"));
    record(&db, ws, (REPORT, "rep-1"), (SUMMARY, "sum-1"));
    record(&db, ws, (REPORT, "rep-1"), (EMBEDDING, "emb-1"));
    // Unrelated artifact in the same workspace and a same-named one elsewhere.
    record(&db, ws, (CHUNK, "chunk-other"), (FILE, "other:file"));
    record(&db, "other-workspace", (CHUNK, "chunk-1"), (FILE, &file));

    assert!(f.freshness().is_fresh());
    let marked = invalidation::invalidate_source(&db, ws, (FILE, &file), "edited").unwrap();
    let mut kinds: Vec<_> = marked.iter().map(|(k, _)| k.as_str()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec![CHUNK, EMBEDDING, GRAPH_NODE, REPORT, SUMMARY]);
    assert!(invalidation::is_stale(&db, ws, REPORT, "rep-1").unwrap());
    assert!(!invalidation::is_stale(&db, ws, CHUNK, "chunk-other").unwrap());
    assert!(!invalidation::is_stale(&db, "other-workspace", CHUNK, "chunk-1").unwrap());
    assert!(!invalidation::is_stale(&db, ws, FILE, &file).unwrap());

    let freshness = f.freshness();
    assert_eq!(freshness.stale_count, 5);
    assert_eq!(freshness.by_kind[REPORT], 1);
    assert_eq!(freshness.by_kind[CHUNK], 1);
    assert!(freshness.oldest_since.is_some());
    let serialized = serde_json::to_value(&freshness).unwrap();
    assert_eq!(serialized["stale_count"], 5);
    assert!(serialized["by_kind"].is_object());
    assert!(serialized["oldest_since"].is_string());

    // Recomputing clears one marker; the rest stay.
    assert!(invalidation::clear_stale(&db, ws, SUMMARY, "sum-1").unwrap());
    assert!(!invalidation::clear_stale(&db, ws, SUMMARY, "sum-1").unwrap());
    assert_eq!(f.freshness().stale_count, 4);
    assert!(
        invalidation::freshness(&db, "other-workspace")
            .unwrap()
            .is_fresh()
    );
}

#[test]
fn committing_a_file_marks_its_graph_nodes_stale_and_context_reports_it() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    // Chunks record their file dependency at commit time.
    let chunk_deps: i64 = f
        .db()
        .query_row(
            "SELECT count(*) FROM gf_dependencies WHERE workspace=? AND dependent_kind='chunk' AND source_kind='file'",
            [&f.workspace],
            |r| r.get(0),
        )
        .unwrap();
    assert!(chunk_deps >= 2, "{chunk_deps}");

    let projection = f
        .store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    assert_eq!(projection["replayed"], false);
    let node_deps: Vec<(String, String)> = {
        let db = f.db();
        let mut statement = db
            .prepare("SELECT dependent_id,source_id FROM gf_dependencies WHERE workspace=? AND dependent_kind='graph_node' AND source_kind='file'")
            .unwrap();
        statement
            .query_map([&f.workspace], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert!(
        !node_deps.is_empty(),
        "graph nodes must depend on their files"
    );
    let a_file = invalidation::file_id(&f.repo, "a.py");
    let a_nodes: Vec<_> = node_deps
        .iter()
        .filter(|(_, source)| *source == a_file)
        .map(|(node, _)| node.clone())
        .collect();
    let b_nodes: Vec<_> = node_deps
        .iter()
        .filter(|(_, source)| *source == invalidation::file_id(&f.repo, "b.py"))
        .map(|(node, _)| node.clone())
        .collect();
    assert!(!a_nodes.is_empty() && !b_nodes.is_empty(), "{node_deps:?}");
    assert!(f.freshness().is_fresh(), "{:?}", f.freshness());

    let status = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    assert_eq!(status["freshness"]["stale_count"], 0);

    // Edit a.py through the commit path: only a.py's derived nodes go stale.
    let commit = f.sync(&[("a.py", A_V2), ("b.py", B)]);
    assert_eq!(commit["changed"], json!(["a.py"]));
    let stale_nodes = f.stale(GRAPH_NODE);
    assert!(!stale_nodes.is_empty(), "no graph nodes went stale");
    for node in &a_nodes {
        assert!(stale_nodes.contains(node), "{node} should be stale");
    }
    for node in &b_nodes {
        assert!(!stale_nodes.contains(node), "{node} must stay fresh");
    }
    // Chunks of the edited file were rebuilt in the same commit: fresh again.
    assert!(f.stale(CHUNK).is_empty(), "{:?}", f.stale(CHUNK));

    let context = f
        .store
        .project_context(
            &f.workspace,
            &f.key,
            &f.args(json!({"question":"What does alpha return?"})),
        )
        .unwrap();
    let freshness = &context["freshness"];
    assert!(
        freshness["stale_count"].as_u64().unwrap() >= a_nodes.len() as u64,
        "{freshness}"
    );
    assert_eq!(
        freshness["by_kind"][GRAPH_NODE].as_u64().unwrap() as usize,
        stale_nodes.len()
    );
    assert!(freshness["oldest_since"].is_string());
    let status = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    assert_eq!(status["freshness"], *freshness);

    // Re-projecting the new head set is the recomputation: nodes clear.
    let second = f
        .store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    assert_ne!(second["episode"], projection["episode"]);
    assert!(f.stale(GRAPH_NODE).is_empty());
    if pro_edition() {
        let report = f
            .graph_impact(
                json!({"base_episode":projection["episode"],"head_episode":second["episode"]}),
            )
            .unwrap();
        assert_eq!(
            report["freshness"]["stale_count"], 0,
            "{}",
            report["freshness"]
        );
    }
    assert!(f.freshness().is_fresh());
}

/// A file edited and then restored brings back a head set whose episode is
/// already recorded, so no projection follows: the commit itself clears the
/// graph nodes it marked, and a replayed projection clears what is left, so
/// `context` never reports stale sources that `graphicle sync` cannot
/// refresh.
#[test]
fn restoring_a_projected_head_set_leaves_no_stale_graph_nodes() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let first = f
        .store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    assert!(!f.stale(GRAPH_NODE).is_empty());
    f.store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    assert!(f.stale(GRAPH_NODE).is_empty());
    let restored = f.sync(&[("a.py", A_V1), ("b.py", B)]);
    assert_eq!(restored["changed"], json!(["a.py"]));
    assert!(f.stale(GRAPH_NODE).is_empty(), "{:?}", f.stale(GRAPH_NODE));
    let replayed = f
        .store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    assert_eq!(replayed["episode"], first["episode"]);
    assert_eq!(replayed["replayed"], true);
    let context = f
        .store
        .project_context(
            &f.workspace,
            &f.key,
            &f.args(json!({"question":"What does alpha return?"})),
        )
        .unwrap();
    assert_eq!(
        context["freshness"]["by_kind"].get(GRAPH_NODE),
        None,
        "{}",
        context["freshness"]
    );
}

#[test]
fn removing_a_file_marks_dependents_stale() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    f.store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    let commit = f.sync(&[("a.py", A_V1)]);
    assert_eq!(commit["removed"], json!(["b.py"]));
    let stale = f.stale(GRAPH_NODE);
    assert!(!stale.is_empty());
    let b_file = invalidation::file_id(&f.repo, "b.py");
    let db = f.db();
    for node in &stale {
        let source: String = db
            .query_row(
                "SELECT source_id FROM gf_dependencies WHERE workspace=? AND dependent_kind='graph_node' AND dependent_id=?",
                rusqlite::params![f.workspace, node],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(source, b_file);
    }
}

#[test]
fn connector_document_updates_and_deletes_invalidate_dependents() {
    let f = Fixture::new(&[("c.py", C)]);
    let connector = f
        .store
        .connector_add(
            &f.workspace,
            &f.key,
            &f.args(json!({"name":"Fixture","kind":"jsonl","config":{"acl":{"workspace_readers":true,"credentials":[]}}})),
        )
        .unwrap();
    let cid = s(&connector, "id").to_string();
    let import = |docs: Value, deleted: Value| {
        f.store
            .import_documents(
                &f.workspace,
                &f.key,
                &f.args(json!({"connector_id":cid,"mode":"delta","documents":docs,"deleted_ids":deleted})),
            )
            .unwrap()
    };
    import(
        json!([{"id":"plan","title":"Plan","content":"v1"},{"id":"notes","title":"Notes","content":"n1"}]),
        json!([]),
    );
    let plan = invalidation::connector_document_id(&cid, "plan");
    let notes = invalidation::connector_document_id(&cid, "notes");
    let db = f.db();
    record(
        &db,
        &f.workspace,
        (SUMMARY, "plan-summary"),
        (DOCUMENT, &plan),
    );
    record(
        &db,
        &f.workspace,
        (SUMMARY, "notes-summary"),
        (DOCUMENT, &notes),
    );
    record(&db, &f.workspace, (CHUNK, "plan-chunk"), (DOCUMENT, &plan));
    drop(db);

    // Re-importing identical content is not a change.
    import(
        json!([{"id":"plan","title":"Plan","content":"v1"}]),
        json!([]),
    );
    assert!(f.freshness().is_fresh(), "{:?}", f.freshness());

    import(
        json!([{"id":"plan","title":"Plan","content":"v2"}]),
        json!([]),
    );
    assert_eq!(f.stale(SUMMARY), vec!["plan-summary"]);
    assert_eq!(f.stale(CHUNK), vec!["plan-chunk"]);

    import(json!([]), json!(["notes"]));
    assert_eq!(f.stale(SUMMARY), vec!["notes-summary", "plan-summary"]);
    let freshness = f.freshness();
    assert_eq!(freshness.stale_count, 3);
    assert_eq!(freshness.by_kind[SUMMARY], 2);
}

#[test]
fn memories_record_their_sources_and_go_stale_when_sources_change() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let memory = f
        .store
        .note(
            &f.workspace,
            &f.key,
            &f.args(json!({"kind":"decision","title":"Alpha returns one","body":"alpha returns 1","evidence":[{"id":"file:a.py","quote":"return 1"}]})),
        )
        .unwrap();
    let memory_id = s(&memory, "id").to_string();
    let db = f.db();
    let sources: Vec<String> = {
        let mut statement = db
            .prepare("SELECT source_id FROM gf_dependencies WHERE workspace=? AND dependent_kind=? AND dependent_id=?")
            .unwrap();
        statement
            .query_map(rusqlite::params![f.workspace, MEMORY, memory_id], |r| {
                r.get(0)
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(sources, vec![invalidation::file_id(&f.repo, "a.py")]);
    drop(db);
    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    assert_eq!(f.stale(MEMORY), vec![memory_id]);
}

#[test]
fn revocation_invalidates_answers_cached_for_the_principal() {
    let f = Fixture::new(&[("c.py", C)]);
    let credential = f
        .store
        .grant(
            &f.workspace,
            &f.key,
            &json!({"label":"Ada","role":"reader"}),
        )
        .unwrap();
    let hash = graphicle_memory::text::sha256(s(&credential, "capability").as_bytes());
    let db = f.db();
    record(
        &db,
        &f.workspace,
        (invalidation::ANSWER, "answer-1"),
        (invalidation::PRINCIPAL, &hash),
    );
    record(
        &db,
        &f.workspace,
        (invalidation::ANSWER, "answer-owner"),
        (invalidation::PRINCIPAL, "someone-else"),
    );
    drop(db);
    f.store
        .revoke(&f.workspace, &f.key, s(&credential, "id"))
        .unwrap();
    assert_eq!(f.stale(invalidation::ANSWER), vec!["answer-1"]);
    let status = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    assert_eq!(status["freshness"]["by_kind"]["answer"], 1);
}

fn record(db: &rusqlite::Connection, ws: &str, dependent: (&str, &str), source: (&str, &str)) {
    invalidation::record_dependency(db, ws, dependent, source).unwrap();
}

#[test]
fn re_projection_keeps_dependencies_of_unchanged_nodes_and_drops_closed_ones() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    f.store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    let deps = |f: &Fixture| -> Vec<(String, String)> {
        let db = f.db();
        let mut statement = db
            .prepare("SELECT dependent_id,source_id FROM gf_dependencies WHERE workspace=? AND dependent_kind='graph_node' AND source_kind='file' ORDER BY dependent_id,source_id")
            .unwrap();
        statement
            .query_map([&f.workspace], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let before = deps(&f);
    let b_file = invalidation::file_id(&f.repo, "b.py");
    let b_before: Vec<_> = before.iter().filter(|(_, s)| *s == b_file).collect();
    assert!(!b_before.is_empty());

    // Remove `beta` from a.py: its node closes, b.py's nodes are untouched.
    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    f.store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    let after = deps(&f);
    let b_after: Vec<_> = after.iter().filter(|(_, s)| *s == b_file).collect();
    assert_eq!(
        b_before, b_after,
        "unchanged nodes keep their file dependency"
    );
    let mut unique = after.clone();
    unique.dedup();
    assert_eq!(unique.len(), after.len(), "no duplicate dependency rows");
    let a_file = invalidation::file_id(&f.repo, "a.py");
    let beta_rows = after
        .iter()
        .filter(|(node, s)| *s == a_file && node.contains("beta"))
        .count();
    assert_eq!(
        beta_rows, 0,
        "closed nodes lose their dependency rows: {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|(node, s)| *s == a_file && node.contains("alpha")),
        "{after:?}"
    );

    // Dependencies preserved for b.py still drive invalidation.
    f.sync(&[("a.py", A_V2), ("b.py", "def run():\n    return 0\n")]);
    let stale = f.stale(GRAPH_NODE);
    for (node, _) in &b_after {
        assert!(stale.contains(node), "{node} should be stale: {stale:?}");
    }
}
