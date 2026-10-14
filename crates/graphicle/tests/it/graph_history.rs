//! G03 history: multigraph persistence (B5), commit-sha labels on identical-tree
//! replays (B7), `as_of` by sha, edge provenance, the cardinality-one
//! contradiction rule and parent episodes (B9).
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use graphicle_runtime::{
    api::{App, router},
    graph_projection::{self, resolve_episode},
    jobs, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

/// A nested function is both contained by and called from `outer`: two
/// relations between the same ordered pair.
const NESTED: &str = "def outer():\n    def inner():\n        return 1\n    return inner()\n";
const NESTED_V2: &str = "def outer():\n    def inner():\n        return 2\n    return inner()\n\n\ndef spare():\n    return 0\n";

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
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Graph history").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"history","name":"History"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
        }
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn snapshot(&self) -> Value {
        self.store
            .project_status(&self.workspace, &self.key, &self.args(json!({})))
            .unwrap()["repository"]["snapshot"]
            .clone()
    }
    fn manifest(files: &[(&str, &str)]) -> Vec<Value> {
        files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect()
    }
    /// `sync_prepare` with commit metadata; returns the prepare result.
    fn prepare(&self, files: &[(&str, &str)], sha: &str) -> Value {
        self.store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &self.args(json!({"expected_snapshot":self.snapshot(),"metadata":{"commit":sha},"manifest":Self::manifest(files)})),
            )
            .unwrap()
    }
    /// Full prepare/upload/commit of `files` under commit `sha`.
    fn commit(&self, files: &[(&str, &str)], sha: &str) -> Value {
        let begin = self.prepare(files, sha);
        if begin["sync_id"].is_null() {
            return begin;
        }
        self.store.sync_upload(&self.workspace,&self.key,&self.args(json!({"sync_id":begin["sync_id"],"files":files.iter().map(|(name,text)|json!({"path":name,"content":text})).collect::<Vec<_>>()}))).unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &self.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap()
    }
    /// Lease and run every queued projection job synchronously.
    fn drain_jobs(&self) -> Vec<Value> {
        let mut results = Vec::new();
        while let Some(job) = self.store.lease_job().unwrap() {
            // Every projection queues a cross-repo `link` job; run it too but
            // report only the projection results.
            if job.kind == "link" {
                let result = jobs::run_link(&self.store, &job);
                self.store.finish_job(&job, result).unwrap();
                continue;
            }
            assert_eq!(job.kind, "project", "unexpected job kind {}", job.kind);
            let result = jobs::run_projection(&self.store, &job);
            let value = result.as_ref().ok().cloned().unwrap_or(Value::Null);
            self.store.finish_job(&job, result).unwrap();
            results.push(value);
        }
        results
    }
    fn project(&self) -> Value {
        self.store
            .graph_project(&self.workspace, &self.key, &self.repo)
            .unwrap()
    }
    /// `graph.impact` through the operation table, as HTTP, MCP and
    /// `graphicle call` run it: served by an installed extension (an extension),
    /// otherwise 501 after authorization (Core).
    fn graph_impact(&self, mut args: Value) -> graphicle_runtime::Result<Value> {
        args["repository_id"] = json!(self.repo);
        let app = App::new(self.store.clone(), model::Config::default(), vec![]).unwrap();
        graphicle_runtime::api::command(&app, &self.workspace, &self.key, "graph.impact", &args)
    }
    fn db(&self) -> rusqlite::Connection {
        self.store.connect().unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        self.db()
            .query_row(sql, [&self.repo], |r| r.get(0))
            .unwrap()
    }
    fn app(&self) -> Router {
        router(
            App::new(
                self.store.clone(),
                model::Config::default(),
                vec!["localhost".into()],
            )
            .unwrap(),
        )
    }
    fn url(&self, suffix: &str) -> String {
        format!(
            "/api/workspaces/{}/repositories/{}/{suffix}",
            self.workspace, self.repo
        )
    }
}

async fn call(app: &Router, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("host", "localhost")
                .header("x-workspace-capability", token)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn relations(graph: &graphicle::graph::Graph, source: &str, target: &str) -> Vec<String> {
    let mut out: Vec<String> = graph
        .edges_between(&json!(source), &json!(target))
        .iter()
        .map(|e| s(&Value::Object(e.attributes.to_map()), "relation").to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn parallel_relations_between_one_pair_are_persisted_and_reloaded() {
    let f = Fixture::new();
    f.commit(&[("m.py", NESTED)], "aaa111");
    let first = f.project();
    let episode = s(&first, "episode").to_string();
    let db = f.db();
    let (_, built) = graph_projection::build_graph(&db, &f.repo).unwrap();
    assert!(built.is_multigraph());
    assert_eq!(
        relations(&built, "m_outer", "m_outer_inner"),
        vec!["calls".to_string(), "contains".to_string()],
        "{}",
        built.to_node_link("links")
    );
    // Both relations are separate rows keyed by relation, not one folded edge.
    let keys: Vec<String> = graphicle_runtime::store::rows(
        &db,
        "SELECT key FROM gf_edges WHERE repository_id=? AND source='m_outer' AND target='m_outer_inner' AND valid_to_episode IS NULL ORDER BY key",
        [&f.repo],
    )
    .unwrap()
    .iter()
    .map(|r| s(r, "key").to_string())
    .collect();
    assert_eq!(
        keys,
        vec!["\"calls\"".to_string(), "\"contains\"".to_string()]
    );
    assert_eq!(
        f.count("SELECT multigraph FROM gf_episodes WHERE repository_id=?"),
        1
    );
    // The reconstructed episode carries both relations and equals the build.
    let loaded = graph_projection::load(&db, &f.repo, &episode).unwrap();
    assert!(loaded.is_multigraph());
    assert_eq!(
        relations(&loaded, "m_outer", "m_outer_inner"),
        vec!["calls".to_string(), "contains".to_string()]
    );
    assert_eq!(loaded.edge_count(), built.edge_count());
    if pro_edition() {
        // Impact keyed by relation sees the `calls` edge disappear independently
        // of the `contains` edge when the call is removed.
        f.commit(
            &[(
                "m.py",
                "def outer():\n    def inner():\n        return 1\n    return 1\n",
            )],
            "bbb222",
        );
        let report = f
            .graph_impact(json!({"base_episode":episode,"relations":["calls","contains"]}))
            .unwrap();
        let removed: Vec<(String, String, String)> = report["changed_edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["change"] == "removed")
            .map(|e| {
                (
                    s(e, "source").to_string(),
                    s(e, "target").to_string(),
                    s(e, "relation").to_string(),
                )
            })
            .collect();
        assert!(
            removed.contains(&("m_outer".into(), "m_outer_inner".into(), "calls".into())),
            "{report}"
        );
        assert!(
            !removed.contains(&("m_outer".into(), "m_outer_inner".into(), "contains".into())),
            "{report}"
        );
    }
}

#[test]
fn every_persisted_edge_is_traceable_to_its_origin() {
    let f = Fixture::new();
    f.commit(
        &[
            ("m.py", NESTED),
            (
                "n.py",
                "from m import outer\n\ndef go():\n    return outer()\n",
            ),
        ],
        "aaa111",
    );
    f.project();
    let edges = f.count("SELECT count(*) FROM gf_edges WHERE repository_id=?");
    assert!(edges >= 3);
    let traced = f.count(
        "SELECT count(*) FROM gf_edges e JOIN gf_edge_sources x ON x.repository_id=e.repository_id AND x.edge_row_id=e.id WHERE e.repository_id=? AND x.relpath!='' AND x.derivation!=''",
    );
    assert_eq!(traced, edges);
    let calls = graphicle_runtime::store::one(
        &f.db(),
        "SELECT x.relpath,x.source_location,x.derivation FROM gf_edges e JOIN gf_edge_sources x ON x.edge_row_id=e.id WHERE e.repository_id=? AND e.source='n_go' AND e.relation='calls'",
        [&f.repo],
    )
    .unwrap()
    .unwrap();
    assert_eq!(calls["relpath"], "n.py");
    assert!(s(&calls, "source_location").starts_with('L'), "{calls}");
    // Edge rows persist after their episode is superseded; provenance stays.
    f.commit(&[("m.py", NESTED)], "bbb222");
    f.project();
    let total = f.count("SELECT count(*) FROM gf_edges WHERE repository_id=?");
    assert!(total >= edges);
    assert_eq!(
        f.count("SELECT count(*) FROM gf_edge_sources WHERE repository_id=?"),
        total
    );
}

#[test]
fn identical_tree_under_a_new_sha_labels_the_existing_episode() {
    let f = Fixture::new();
    let committed = f.commit(&[("m.py", NESTED)], "aaa111");
    assert_eq!(committed["replayed"], false);
    assert_eq!(f.drain_jobs().len(), 1);
    let db = f.db();
    let episode = resolve_episode(&db, &f.workspace, &f.repo, "aaa111").unwrap();
    // Same tree, new commit: the sync layer replays without a sync id and the
    // existing episode gains the new sha without any explicit label call.
    let replay = f.prepare(&[("m.py", NESTED)], "ccc333");
    assert_eq!(replay["replayed"], true, "{replay}");
    assert!(replay["sync_id"].is_null());
    assert_eq!(replay["projection_job"]["replayed"], true, "{replay}");
    assert!(f.drain_jobs().is_empty());
    assert_eq!(
        resolve_episode(&db, &f.workspace, &f.repo, "ccc333").unwrap(),
        episode
    );
    assert_eq!(
        f.count("SELECT count(*) FROM gf_episodes WHERE repository_id=?"),
        1
    );
    // A different tree, then the original tree again under yet another sha:
    // the reverted commit is a new sync whose episode already exists.
    f.commit(&[("m.py", NESTED_V2)], "ddd444");
    f.drain_jobs();
    let back = f.commit(&[("m.py", NESTED)], "eee555");
    assert_eq!(back["replayed"], false);
    f.drain_jobs();
    assert_eq!(
        resolve_episode(&db, &f.workspace, &f.repo, "eee555").unwrap(),
        episode
    );
    assert_eq!(
        f.count("SELECT count(*) FROM gf_episodes WHERE repository_id=?"),
        2
    );
    // Every sha that was synced resolves.
    for sha in ["aaa111", "ccc333", "ddd444", "eee555"] {
        resolve_episode(&db, &f.workspace, &f.repo, sha)
            .unwrap_or_else(|e| panic!("{sha}: {}", e.message));
    }
}

#[test]
fn replay_before_projection_requeues_a_skipped_projection_job() {
    let f = Fixture::new();
    f.commit(&[("m.py", NESTED)], "aaa111");
    // The head set moves on before the job runs: the job completes as a skip.
    f.commit(&[("m.py", NESTED_V2)], "bbb222");
    let results = f.drain_jobs();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results.iter().any(|r| r["skipped"] == true), "{results:?}");
    // Reverting to the skipped tree re-commits; the earlier job is replayed
    // and re-queued rather than left complete without an episode.
    let back = f.commit(&[("m.py", NESTED)], "ccc333");
    assert_eq!(back["projection_job"]["replayed"], true, "{back}");
    assert_eq!(back["projection_job"]["state"], "queued", "{back}");
    let results = f.drain_jobs();
    assert_eq!(results.len(), 1, "{results:?}");
    let db = f.db();
    let episode = resolve_episode(&db, &f.workspace, &f.repo, "ccc333").unwrap();
    assert_eq!(
        resolve_episode(&db, &f.workspace, &f.repo, "aaa111").unwrap(),
        episode
    );
}

#[tokio::test]
async fn as_of_by_commit_sha_reconstructs_history_and_impact_accepts_selectors() {
    let f = Fixture::new();
    f.commit(&[("m.py", NESTED)], "aaa111");
    f.drain_jobs();
    f.commit(&[("m.py", NESTED_V2)], "bbb222");
    f.drain_jobs();
    let db = f.db();
    let first = resolve_episode(&db, &f.workspace, &f.repo, "aaa111").unwrap();
    let second = resolve_episode(&db, &f.workspace, &f.repo, "bbb222").unwrap();
    assert_ne!(first, second);

    let old = f
        .store
        .graph_as_of(&f.workspace, &f.key, &f.repo, &json!({"as_of":"aaa111"}))
        .unwrap();
    assert_eq!(old["episode"], json!(first));
    assert_eq!(old["sequence"], 1);
    assert!(old["parent_episode"].is_null());
    assert!(old["labels"].as_array().unwrap().contains(&json!("aaa111")));
    let ids = |v: &Value| -> Vec<String> {
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| s(n, "id").to_string())
            .collect()
    };
    assert!(!ids(&old).contains(&"m_spare".to_string()), "{old}");
    let new = f
        .store
        .graph_as_of(&f.workspace, &f.key, &f.repo, &json!({"as_of":"bbb222"}))
        .unwrap();
    assert_eq!(new["episode"], json!(second));
    assert_eq!(new["parent_episode"], json!(first));
    assert!(ids(&new).contains(&"m_spare".to_string()), "{new}");
    let latest = f
        .store
        .graph_as_of(&f.workspace, &f.key, &f.repo, &json!({}))
        .unwrap();
    assert_eq!(latest["episode"], json!(second));
    // Node-centred view with relation filter over the multigraph.
    let focused = f
        .store
        .graph_as_of(
            &f.workspace,
            &f.key,
            &f.repo,
            &json!({"as_of":"aaa111","node":"m_outer","relations":["calls"]}),
        )
        .unwrap();
    assert_eq!(
        ids(&focused),
        vec!["m_outer".to_string(), "m_outer_inner".to_string()]
    );
    assert_eq!(focused["edges"].as_array().unwrap().len(), 1);
    assert_eq!(focused["edges"][0]["relation"], "calls");
    assert_eq!(focused["edges"][0]["key"], "calls");
    assert!(
        f.store
            .graph_as_of(&f.workspace, &f.key, &f.repo, &json!({"as_of":"nope"}))
            .is_err()
    );

    if pro_edition() {
        // Impact accepts sha labels as well as ids.
        let report = f
            .graph_impact(json!({"base_episode":"aaa111","head_episode":"bbb222"}))
            .unwrap();
        assert_eq!(report["base_episode"], json!(first));
        assert_eq!(report["head_episode"], json!(second));
        assert_eq!(report["summary"]["added_nodes"], 1, "{report}");
        let bad = f.graph_impact(json!({"base_episode":"nope"})).unwrap_err();
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    }
    // HTTP and MCP surfaces.
    let app = f.app();
    let (status, body) = call(
        &app,
        &f.url("graph/as_of"),
        &f.key,
        json!({"as_of":"aaa111"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["episode"], json!(first));
    let stranger = s(&f.store.create_workspace("Other").unwrap(), "capability").to_string();
    let (status, _) = call(
        &app,
        &f.url("graph/as_of"),
        &stranger,
        json!({"as_of":"aaa111"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, mcp) = call(
        &app,
        &format!("/mcp/{}/{}", f.workspace, f.repo),
        &f.key,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"graph_as_of","arguments":{"as_of":"bbb222","node":"m_spare"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{mcp}");
    assert_eq!(mcp["result"]["isError"], false, "{mcp}");
    assert_eq!(mcp["result"]["structuredContent"]["episode"], json!(second));
}

#[test]
fn parent_episode_links_history_as_recorded() {
    let f = Fixture::new();
    f.commit(&[("m.py", NESTED)], "aaa111");
    let e1 = s(&f.project(), "episode").to_string();
    f.commit(&[("m.py", NESTED_V2)], "bbb222");
    let e2 = s(&f.project(), "episode").to_string();
    // Reverting replays e1: no new episode, parents unchanged.
    f.commit(&[("m.py", NESTED)], "ccc333");
    assert_eq!(f.project()["replayed"], true);
    f.commit(
        &[("m.py", NESTED), ("n.py", "def x():\n    return 1\n")],
        "ddd444",
    );
    let e3 = s(&f.project(), "episode").to_string();
    let listed = graph_projection::episodes(&f.db(), &f.repo).unwrap();
    let parents: Vec<(String, Value)> = listed
        .iter()
        .map(|e| (s(e, "episode").to_string(), e["parent_episode"].clone()))
        .collect();
    assert_eq!(
        parents,
        vec![
            (e1.clone(), Value::Null),
            (e2.clone(), json!(e1)),
            // Recorded while e2 was still `latest`: history follows record order.
            (e3, json!(e2)),
        ]
    );
}

const JAVA_BASE: &str = "public class Base {}\n";
const JAVA_OTHER: &str = "public class Other {}\n";

#[test]
fn single_valued_inheritance_change_records_a_contradiction() {
    let f = Fixture::new();
    let child = |parent: &str| format!("public class Child extends {parent} {{}}\n");
    f.commit(
        &[
            ("Base.java", JAVA_BASE),
            ("Other.java", JAVA_OTHER),
            ("Child.java", &child("Base")),
        ],
        "aaa111",
    );
    let e1 = s(&f.project(), "episode").to_string();
    let db = f.db();
    let g1 = graph_projection::load(&db, &f.repo, &e1).unwrap();
    let child_id = g1
        .nodes()
        .iter()
        .find(|n| n.attributes.get("label").and_then(Value::as_str) == Some("Child"))
        .map(|n| n.id.clone())
        .unwrap_or_else(|| panic!("{}", g1.to_node_link("links")));
    assert_eq!(
        graph_projection::contradictions(&db, &f.repo, None).unwrap(),
        Vec::<Value>::new()
    );

    f.commit(
        &[
            ("Base.java", JAVA_BASE),
            ("Other.java", JAVA_OTHER),
            ("Child.java", &child("Other")),
        ],
        "bbb222",
    );
    let e2 = s(&f.project(), "episode").to_string();
    let marks = graph_projection::contradictions(&db, &f.repo, Some(&e2)).unwrap();
    assert_eq!(marks.len(), 1, "{marks:?}");
    let mark = &marks[0];
    assert_eq!(mark["relation"], "inherits");
    assert_eq!(mark["subject"], child_id);
    assert!(s(mark, "old_target").ends_with("base"), "{mark}");
    assert!(s(mark, "new_target").ends_with("other"), "{mark}");
    // The old edge row was closed by this episode and the new one opened.
    let old: Value = graphicle_runtime::store::one(
        &db,
        "SELECT valid_to_episode FROM gf_edges WHERE id=?",
        [mark["old_edge_row_id"].as_i64().unwrap()],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old["valid_to_episode"], json!(e2));
    let new: Value = graphicle_runtime::store::one(
        &db,
        "SELECT valid_from_episode,valid_to_episode FROM gf_edges WHERE id=?",
        [mark["new_edge_row_id"].as_i64().unwrap()],
    )
    .unwrap()
    .unwrap();
    assert_eq!(new["valid_from_episode"], json!(e2));
    assert!(new["valid_to_episode"].is_null());
    let view = f
        .store
        .graph_as_of(&f.workspace, &f.key, &f.repo, &json!({"as_of":e2}))
        .unwrap();
    assert_eq!(view["contradictions"].as_array().unwrap().len(), 1);
}

#[test]
fn multi_valued_relations_never_record_contradictions() {
    let f = Fixture::new();
    // Python allows several bases: a changed base is not a contradiction.
    // A changed call target is never one.
    f.commit(
        &[
            ("m.py", "class Base:\n    pass\n\nclass Other:\n    pass\n\nclass Child(Base):\n    pass\n\ndef a():\n    return 1\n\ndef b():\n    return 2\n\ndef run():\n    return a()\n"),
        ],
        "aaa111",
    );
    f.project();
    f.commit(
        &[
            ("m.py", "class Base:\n    pass\n\nclass Other:\n    pass\n\nclass Child(Other):\n    pass\n\ndef a():\n    return 1\n\ndef b():\n    return 2\n\ndef run():\n    return b()\n"),
        ],
        "bbb222",
    );
    let e2 = s(&f.project(), "episode").to_string();
    let db = f.db();
    // Both old edges were closed by disappearance...
    let closed = f.count(&format!(
        "SELECT count(*) FROM gf_edges WHERE repository_id=? AND valid_to_episode='{e2}' AND relation IN ('inherits','calls') AND source IN ('m_child','m_run')"
    ));
    assert_eq!(closed, 2);
    // ...and nothing was marked as contradicted.
    assert_eq!(
        graph_projection::contradictions(&db, &f.repo, None).unwrap(),
        Vec::<Value>::new()
    );
}
