//! Retention: 25 projected episodes with coverage and unresolved
//! calls at each; after `retention.apply` (defaults: coverage 20 episodes,
//! unresolved calls current + previous) the old rows are gone, every episode
//! and its graph survive, and the current change-impact report is unchanged.
//! Also covers `graph.stats` and the `gf_unresolved_calls` table taking over
//! from `gf_episodes.attributes`.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use graphicle_runtime::{
    api::{App, router},
    graph_projection, jobs, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

/// `Box.size` and `Ring.size` make `x.size()` an ambiguous receiver call:
/// every episode of this file records `unresolved_calls`.
fn shapes(version: usize) -> String {
    format!(
        "class Box:\n    def size(self):\n        return {version}\n\n\nclass Ring:\n    def size(self):\n        return {version} * 2\n\n\ndef measure(x):\n    return x.size()\n"
    )
}
const TEST_SHAPES: &str =
    "from shapes import measure, Box\n\ndef test_measure():\n    assert measure(Box()) > 0\n";

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
        let ws = store.create_workspace("Retention").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"retention","name":"Retention"}),
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
    /// Full prepare/upload/commit of `files` under commit `sha`, then run
    /// the queued projection (and link) jobs. Returns the new episode id.
    fn commit(&self, files: &[(&str, &str)], sha: &str) -> String {
        let begin = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &self.args(json!({"expected_snapshot":self.snapshot(),"metadata":{"commit":sha},"manifest":Self::manifest(files)})),
            )
            .unwrap();
        assert!(!begin["sync_id"].is_null(), "{begin}");
        self.store.sync_upload(&self.workspace,&self.key,&self.args(json!({"sync_id":begin["sync_id"],"files":files.iter().map(|(name,text)|json!({"path":name,"content":text})).collect::<Vec<_>>()}))).unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &self.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
        self.drain_jobs();
        let db = self.store.connect().unwrap();
        graph_projection::resolve_episode(&db, &self.workspace, &self.repo, sha).unwrap()
    }
    fn drain_jobs(&self) {
        while let Some(job) = self.store.lease_job().unwrap() {
            let result = match job.kind.as_str() {
                "project" => jobs::run_projection(&self.store, &job),
                "link" => jobs::run_link(&self.store, &job),
                other => panic!("unexpected job kind {other}"),
            };
            self.store.finish_job(&job, result).unwrap();
        }
    }
    fn upload_coverage(&self, version: usize) -> Value {
        let revision = sha(version);
        let test_id = "tests/test_shapes.py::test_measure";
        let coverage = json!({
            "revision": revision, "format": "coverage_py",
            "granularity": "test", "per_test_available": true,
            "entries": {test_id: {"shapes.py": [[2, 3], [12, 13]]}},
            "measured": {"shapes.py": [[2, 3], [12, 13]]},
            "inventory": {
                "revision": revision,
                "source_revisions": {
                    "shapes.py": graphicle_memory::text::sha256(shapes(version).as_bytes()),
                    "tests/test_shapes.py": graphicle_memory::text::sha256(TEST_SHAPES.as_bytes())
                },
                "files": {"tests/test_shapes.py": [test_id]}
            }
        });
        self.command(
            "coverage.upload",
            json!({"commit": revision, "data": coverage}),
        )
    }
    /// An operation through the operation table, as HTTP, MCP and `graphicle
    /// call` run it: an edition's operations are served by an installed extension.
    fn command(&self, operation: &str, args: Value) -> Value {
        let app = App::new(self.store.clone(), model::Config::default(), vec![]).unwrap();
        graphicle_runtime::api::command(
            &app,
            &self.workspace,
            &self.key,
            operation,
            &self.args(args),
        )
        .unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        self.store
            .connect()
            .unwrap()
            .query_row(sql, [&self.repo], |r| r.get(0))
            .unwrap()
    }
    fn report(&self, base: &str, head: &str) -> Value {
        self.command(
            "impact.report",
            json!({"base": base, "head": head, "changed_files": ["shapes.py"]}),
        )
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
}

async fn get(app: &Router, path: &str, token: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header("host", "localhost")
                .header("x-workspace-capability", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn sha(n: usize) -> String {
    format!("{n:040x}")
}

#[test]
fn unresolved_calls_live_in_their_own_table_and_reload() {
    let f = Fixture::new();
    let e1 = f.commit(
        &[
            ("shapes.py", &shapes(1)),
            ("tests/test_shapes.py", TEST_SHAPES),
        ],
        &sha(1),
    );
    let db = f.store.connect().unwrap();
    let attributes: String = db
        .query_row(
            "SELECT attributes FROM gf_episodes WHERE repository_id=? AND episode=?",
            [&f.repo, &e1],
            |r| r.get(0),
        )
        .unwrap();
    let attributes: Value = serde_json::from_str(&attributes).unwrap();
    assert!(
        attributes.get("unresolved_calls").is_none(),
        "{attributes:#}"
    );
    let stored = f.count("SELECT count(*) FROM gf_unresolved_calls WHERE repository_id=? AND valid_to_episode IS NULL");
    assert!(stored > 0, "fixture must produce ambiguous receiver calls");
    assert_eq!(attributes["unresolved_calls_count"], stored);
    let graph = graph_projection::load(&db, &f.repo, &e1).unwrap();
    let calls = graph.attributes()["unresolved_calls"].as_array().unwrap();
    assert_eq!(calls.len() as i64, stored);
    assert!(calls.iter().all(|c| c["caller"].is_string()), "{calls:#?}");

    // The next episode closes the previous rows: current + previous open.
    let e2 = f.commit(
        &[
            ("shapes.py", &shapes(2)),
            ("tests/test_shapes.py", TEST_SHAPES),
        ],
        &sha(2),
    );
    assert_ne!(e1, e2);
    let closed = f.count("SELECT count(*) FROM gf_unresolved_calls WHERE repository_id=? AND valid_to_episode IS NOT NULL");
    assert_eq!(closed, stored);
    let previous = graph_projection::load(&db, &f.repo, &e1).unwrap();
    assert_eq!(
        previous.attributes()["unresolved_calls"]
            .as_array()
            .unwrap()
            .len() as i64,
        stored
    );
}

#[tokio::test]
async fn twenty_five_episodes_are_trimmed_to_the_retention_window() {
    let f = Fixture::new();
    let pro = pro_edition();
    let mut episodes = Vec::new();
    for n in 1..=25 {
        let episode = f.commit(
            &[
                ("shapes.py", &shapes(n)),
                ("tests/test_shapes.py", TEST_SHAPES),
            ],
            &sha(n),
        );
        if pro {
            f.upload_coverage(n);
        }
        episodes.push(episode);
    }
    assert_eq!(
        f.count("SELECT count(*) FROM gf_episodes WHERE repository_id=?"),
        25
    );
    assert_eq!(
        f.count("SELECT count(*) FROM gf_test_coverage WHERE repository_id=?") > 0,
        pro
    );
    let before = if pro {
        f.report(&sha(24), &sha(25))
    } else {
        Value::Null
    };
    if pro {
        assert_eq!(before["coverage_status"], "available", "{before:#}");
        assert!(
            before["counts"]["tests_by_evidence"]["executed"]
                .as_u64()
                .unwrap()
                >= 1,
            "{before:#}"
        );
    }

    // Explicit run with the defaults: nothing left older than the window.
    let applied = f
        .store
        .maintain(&f.workspace, &f.key, "retention.apply", &f.args(json!({})))
        .unwrap();
    assert_eq!(applied["retention"]["coverage_episodes"], 20, "{applied}");
    assert_eq!(applied["retention"]["unresolved_episodes"], 2, "{applied}");
    // Projection already trimmed to the defaults, so this run removed
    // nothing and signs nothing; the automatic trims were receipted.
    assert!(applied.get("receipt").is_none(), "{applied:#}");
    let receipts = graphicle_runtime::deletion::list(&f.store, &f.workspace, &f.key).unwrap();
    let automatic: Vec<&Value> = receipts["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["operation"] == "retention.graph")
        .collect();
    assert!(!automatic.is_empty(), "{receipts:#}");
    assert!(automatic.iter().all(|r| r["verified_local"] == true));
    assert_eq!(receipts["chain"]["intact"], true);

    let covered_episodes =
        f.count("SELECT count(DISTINCT episode) FROM gf_test_coverage WHERE repository_id=?");
    assert_eq!(covered_episodes, if pro { 20 } else { 0 });
    let upload_episodes =
        f.count("SELECT count(DISTINCT episode) FROM gf_coverage_uploads WHERE repository_id=?");
    assert_eq!(upload_episodes, if pro { 20 } else { 0 });
    assert_eq!(
        f.count("SELECT count(*) FROM gf_coverage_inventory WHERE repository_id=?"),
        if pro { 20 } else { 0 }
    );
    let unresolved_episodes =
        f.count("SELECT count(DISTINCT episode) FROM gf_unresolved_calls WHERE repository_id=?");
    assert_eq!(unresolved_episodes, 2);
    let db = f.store.connect().unwrap();
    for (index, episode) in episodes.iter().enumerate() {
        let n = index + 1;
        let coverage: i64 = db
            .query_row(
                "SELECT count(*) FROM gf_test_coverage WHERE repository_id=? AND episode=?",
                [&f.repo, episode],
                |r| r.get(0),
            )
            .unwrap();
        let unresolved: i64 = db
            .query_row(
                "SELECT count(*) FROM gf_unresolved_calls WHERE repository_id=? AND episode=?",
                [&f.repo, episode],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            coverage > 0,
            pro && n > 5,
            "episode {n} coverage {coverage}"
        );
        let inventory: i64 = db
            .query_row(
                "SELECT count(*) FROM gf_coverage_inventory WHERE repository_id=? AND episode=?",
                [&f.repo, episode],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inventory, i64::from(pro && n > 5), "episode {n} inventory");
        assert_eq!(
            unresolved > 0,
            n > 23,
            "episode {n} unresolved {unresolved}"
        );
        // Graph history itself is never trimmed.
        let graph = graph_projection::load(&db, &f.repo, episode).unwrap();
        assert!(graph.node_count() > 0);
    }

    // A tighter explicit window trims further and is reported.
    let tight = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "retention.apply",
            &f.args(json!({"retention": {"coverage_episodes": 3, "unresolved_episodes": 1}})),
        )
        .unwrap();
    assert!(
        (tight["deleted"]["coverage"].as_i64().unwrap() > 0) == pro,
        "{tight}"
    );
    // No silent deletion: the explicit trim is receipted and re-counted.
    let receipt = &tight["receipt"];
    assert_eq!(receipt["operation"], "retention.graph", "{tight:#}");
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(receipt["policy"]["coverage_episodes"], 3);
    assert_eq!(
        receipt["removed"]["rows"]["coverage_inventory"],
        if pro { 17 } else { 0 }
    );
    assert_eq!(
        receipt["removed"]["rows"]["coverage"].as_u64().unwrap() > 0,
        pro
    );
    assert!(
        receipt["remaining"]
            .as_object()
            .unwrap()
            .values()
            .all(|n| n == 0),
        "{receipt:#}"
    );
    let verified = graphicle_runtime::deletion::verify(
        &f.store,
        &f.workspace,
        &f.key,
        &json!({"receipt_id": receipt["id"]}),
    )
    .unwrap();
    assert_eq!(verified["complete"], true, "{verified:#}");
    assert_eq!(
        f.count("SELECT count(DISTINCT episode) FROM gf_test_coverage WHERE repository_id=?"),
        if pro { 3 } else { 0 }
    );
    assert_eq!(
        f.count("SELECT count(DISTINCT episode) FROM gf_unresolved_calls WHERE repository_id=?"),
        1
    );

    if pro {
        // The current report is unchanged by trimming history it never used.
        let after = f.report(&sha(24), &sha(25));
        assert_eq!(before, after);
        // ... and an old report that lost its coverage degrades to `none`.
        // (`coverage_status` is omitted from the JSON when it is `none`.)
        let old = f.report(&sha(1), &sha(2));
        assert!(
            old.get("coverage_status").is_none_or(|v| v == "none"),
            "{old:#}"
        );
        assert!(old.get("uncovered_changed").is_none(), "{old:#}");
    }
    // The previous episode lost its unresolved calls: the loaded graph says so.
    let stale = graph_projection::load(&db, &f.repo, &episodes[23]).unwrap();
    assert_eq!(
        stale.attributes()["unresolved_calls"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );

    // graph.stats: bytes by table, episode count, last report timing.
    let stats = f.store.graph_stats(&f.workspace, &f.key, &f.repo).unwrap();
    assert_eq!(stats["episodes"], 25, "{stats:#}");
    assert_eq!(
        stats["tables"]["coverage_inventory"]["rows"],
        if pro { 3 } else { 0 }
    );
    for table in [
        "nodes",
        "edges",
        "edge_sources",
        "coverage",
        "cross_edges",
        "unresolved_calls",
    ] {
        assert!(
            stats["tables"][table]["rows"].is_i64(),
            "{table}: {stats:#}"
        );
        assert!(
            stats["tables"][table]["content_bytes"].is_i64(),
            "{table}: {stats:#}"
        );
    }
    assert!(stats["tables"]["nodes"]["rows"].as_i64().unwrap() > 0);
    assert_eq!(
        stats["tables"]["coverage"]["rows"].as_i64().unwrap() > 0,
        pro
    );
    assert_eq!(stats["tables"]["cross_edges"]["rows"], 0);
    assert!(stats["content_bytes"].as_i64().unwrap() > 0);
    assert!(stats["database_bytes_all_repositories"].as_i64().unwrap() > 0);
    assert_eq!(stats["impact_reports"], if pro { 3 } else { 0 });
    if pro {
        assert!(
            stats["last_impact_report"]["elapsed_ms"].is_i64(),
            "{stats:#}"
        );
    }
    assert_eq!(
        stats["coverage_uploads"].as_array().unwrap().len(),
        if pro { 3 } else { 0 }
    );
    if pro {
        assert_eq!(stats["coverage_uploads"][0]["granularity"], "test");
    }
    assert_eq!(stats["retention_defaults"]["coverage_episodes"], 20);

    // Same document over HTTP.
    let app = f.app();
    let (status, body) = get(
        &app,
        &format!(
            "/api/workspaces/{}/repositories/{}/graph/stats",
            f.workspace, f.repo
        ),
        &f.key,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["episodes"], 25);
    assert_eq!(
        body["tables"]["coverage"]["rows"],
        stats["tables"]["coverage"]["rows"]
    );
}
