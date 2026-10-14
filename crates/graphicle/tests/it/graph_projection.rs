use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use graphicle_runtime::{
    api::{App, router},
    graph_projection::{self, graph_ref},
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const A_V1: &str = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n";
const A_V2: &str = "def alpha():\n    return 1\n";
const B: &str = "from a import alpha, beta\n\ndef run():\n    return alpha() + beta()\n";

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
        let ws = store.create_workspace("Graph projection").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"graph","name":"Projected sources"}),
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
    fn count(&self, table: &str) -> i64 {
        self.db()
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE repository_id=?"),
                [&self.repo],
                |r| r.get(0),
            )
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

fn sorted_node_link(graph: &graphicle::graph::Graph) -> Value {
    let mut data = graph.to_node_link("links");
    for key in ["nodes", "links"] {
        let mut items = data[key].as_array().unwrap().clone();
        items.sort_by_key(graphicle_memory::text::canonical);
        data[key] = json!(items);
    }
    data
}

fn ids(items: &Value, key: &str) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|v| s(v, key).to_string())
        .collect()
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    if let Some(token) = token {
        request = request.header("x-workspace-capability", token);
    }
    if !body.is_null() {
        request = request.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
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

#[test]
fn impact_between_episodes_reports_removed_symbol_and_dependents() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let first = f.project();
    assert_eq!(first["replayed"], false);
    assert!(first["nodes"].as_u64().unwrap() >= 5, "{first}");
    let episode1 = s(&first, "episode").to_string();
    assert_eq!(first["graph_ref"], json!(graph_ref(&f.repo, &episode1)));

    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    let second = f.project();
    let episode2 = s(&second, "episode").to_string();
    assert_ne!(episode1, episode2);
    assert_eq!(
        second["nodes"].as_u64().unwrap() + 1,
        first["nodes"].as_u64().unwrap()
    );

    let report = f.graph_impact(json!({"base_episode":episode1,"head_episode":episode2}));
    if !pro_edition() {
        // Core: the overridable operation answers 501 after authorization.
        assert_eq!(report.unwrap_err().status, StatusCode::NOT_IMPLEMENTED);
        return;
    }
    let report = report.unwrap();
    assert_eq!(report["base_episode"], json!(episode1));
    assert_eq!(report["head_episode"], json!(episode2));
    let removed: Vec<_> = report["changed_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["change"] == "removed")
        .map(|n| s(n, "node_id").to_string())
        .collect();
    assert_eq!(removed, vec!["a_beta".to_string()], "{report}");
    let affected = ids(&report["affected"], "node_id");
    assert!(affected.contains(&"b_run".to_string()), "{report}");
    assert!(affected.contains(&"b".to_string()), "{report}");
    assert_eq!(report["summary"]["removed_nodes"], 1);
    assert_eq!(report["summary"]["added_nodes"], 0);
    assert_eq!(report["summary"]["affected"], json!(affected.len()));
    assert!(report["summary"]["removed_edges"].as_u64().unwrap() >= 3);
    assert!(
        report["changed_edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"] == "b_run" && e["target"] == "a_beta" && e["change"] == "removed")
    );

    // Without head_episode the current projected head is used.
    let implicit = f.graph_impact(json!({"base_episode":episode1})).unwrap();
    assert_eq!(implicit["head_episode"], json!(episode2));
    assert_eq!(implicit["summary"], report["summary"]);

    // A head set that changed since the last projection is projected first.
    f.sync(&[("a.py", A_V1), ("b.py", B)]);
    let fresh = f.graph_impact(json!({"base_episode":episode2})).unwrap();
    assert_eq!(fresh["head_episode"], json!(episode1));
    assert_eq!(fresh["summary"]["added_nodes"], 1);
    assert_eq!(f.count("gf_episodes"), 2);
}

#[test]
fn loaded_episode_equals_built_graph() {
    let f = Fixture::new(&[
        ("a.py", A_V1),
        ("b.py", B),
        ("docs/notes.md", "# Notes\n\nSee a.py\n"),
    ]);
    let db = f.db();
    let (episode, built) = graph_projection::build_graph(&db, &f.repo).unwrap();
    assert!(built.node_count() >= 5);
    assert!(built.is_directed());
    let projected = f.project();
    assert_eq!(projected["episode"], json!(episode));
    assert_eq!(projected["nodes"], json!(built.node_count()));
    assert_eq!(projected["edges"], json!(built.edge_count()));
    let loaded = graph_projection::load(&db, &f.repo, &episode).unwrap();
    assert_eq!(sorted_node_link(&loaded), sorted_node_link(&built));
    // The registry is process-wide and holds 8 graphs: tests running beside
    // this one can evict the projection's handle before it is read (seen on a
    // 96-core Linux machine), so an evicted handle is reopened from
    // the tables, as every reader of a resident graph does.
    let resident = graph_projection::resident(&db, &f.repo, &episode).unwrap();
    assert_eq!(sorted_node_link(&resident), sorted_node_link(&built));
    assert!(
        loaded
            .nodes()
            .iter()
            .filter_map(|n| n.attributes.get("source_file"))
            .filter_map(Value::as_str)
            .all(|file| !file.starts_with('/'))
    );
}

#[test]
fn extraction_cache_persists_across_builds_and_leaves_the_graph_unchanged() {
    // Each build uses the cache only when it gets the cache's lock.
    let _flock = crate::fork_guard::flock_sensitive();
    let f = Fixture::new(&[
        ("a.py", A_V1),
        ("b.py", B),
        ("docs/notes.md", "# Notes\n\nSee a.py\n"),
    ]);
    let db = f.db();
    let cache = graph_projection::cache_root(&db, &f.repo).unwrap();
    assert!(cache.starts_with(std::fs::canonicalize(f._dir.path()).unwrap()));
    let (episode, cold, summary) = graph_projection::build_graph_with_report(&db, &f.repo).unwrap();
    assert_eq!(summary["cached"], true);
    assert_eq!(summary["changed"], 3);
    assert_eq!(summary["unchanged"], 0);
    assert!(cache.is_dir());
    // A stable materialization also reuses dependency descriptions, without
    // loading parser records. The graph stays byte-identical.
    let (again, warm, warm_summary) =
        graph_projection::build_graph_with_report(&db, &f.repo).unwrap();
    assert_eq!(again, episode);
    assert_eq!(warm_summary["changed"], 0);
    assert_eq!(warm_summary["unchanged"], 3);
    assert_eq!(warm_summary["resolution"]["skipped_parsing"], 3);
    assert_eq!(warm_summary["materialization"]["written"], 0);
    assert_eq!(warm_summary["materialization"]["reused"], 3);
    assert_eq!(sorted_node_link(&warm), sorted_node_link(&cold));
    // One edited file re-extracts alone; the report is persisted with the episode.
    f.sync(&[
        ("a.py", A_V2),
        ("b.py", B),
        ("docs/notes.md", "# Notes\n\nSee a.py\n"),
    ]);
    let (edited, graph, edited_summary) =
        graph_projection::build_graph_with_report(&db, &f.repo).unwrap();
    assert_ne!(edited, episode);
    assert_eq!(edited_summary["changed"], 1);
    assert_eq!(edited_summary["unchanged"], 2);
    assert_eq!(edited_summary["materialization"]["written"], 1);
    assert!(graph.attributes().get("extraction_run").is_none());
    let projected = f.project();
    assert_eq!(projected["episode"], json!(edited));
    let listing = graph_projection::episodes(&db, &f.repo).unwrap();
    let recorded = listing
        .iter()
        .find(|e| e["episode"] == json!(edited))
        .unwrap();
    assert_eq!(recorded["extraction"]["cached"], true);
    assert_eq!(recorded["extraction"]["unchanged"], 3);
    assert_eq!(recorded["extraction"]["changed"], 0);
    // The run report never leaks into the reconstructed graph.
    let loaded = graph_projection::load(&db, &f.repo, &edited).unwrap();
    assert_eq!(sorted_node_link(&loaded), sorted_node_link(&graph));
}

#[test]
fn projection_keeps_edge_identities_distinct_with_embedded_nul() {
    let f = Fixture::new(&[("a.py", A_V1)]);
    let db = f.db();
    let graph = |links: Value| {
        graphicle::graph::Graph::from_node_link(
            &json!({
                "directed": true, "multigraph": true,
                "nodes": [{"id":"a\u{0000}b"},{"id":"c"},{"id":"a"},{"id":"b\u{0000}c"}],
                "links": links
            }),
            "links",
        )
        .unwrap()
    };
    let first_edge = json!({"source":"a\u{0000}b","target":"c","relation":"calls","key":"calls"});
    let other_edge = json!({"source":"a","target":"b\u{0000}c","relation":"calls","key":"calls"});
    let first = graph(json!([first_edge, other_edge]));
    let second = graph(json!([first_edge]));
    let third = graph(json!([]));
    for (episode, current) in [("first", &first), ("second", &second), ("third", &third)] {
        graph_projection::persist(&db, &f.repo, episode, current).unwrap();
        assert_eq!(
            sorted_node_link(&graph_projection::load(&db, &f.repo, episode).unwrap()),
            sorted_node_link(current),
            "current episode {episode}"
        );
    }
    for (episode, expected) in [("first", &first), ("second", &second)] {
        assert_eq!(
            sorted_node_link(&graph_projection::load(&db, &f.repo, episode).unwrap()),
            sorted_node_link(expected),
            "historical episode {episode}"
        );
    }
}

#[test]
fn reprojecting_identical_content_is_idempotent() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let first = f.project();
    let counts = (
        f.count("gf_episodes"),
        f.count("gf_nodes"),
        f.count("gf_edges"),
    );
    assert_eq!(counts.0, 1);
    assert_eq!(counts.1, first["nodes"].as_i64().unwrap());
    assert_eq!(counts.2, first["edges"].as_i64().unwrap());
    let again = f.project();
    assert_eq!(again["replayed"], true);
    assert_eq!(again["episode"], first["episode"]);
    assert_eq!(again["graph_ref"], first["graph_ref"]);
    assert_eq!(
        (
            f.count("gf_episodes"),
            f.count("gf_nodes"),
            f.count("gf_edges")
        ),
        counts
    );
    // Reverting to an already projected head set replays that episode.
    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    let second = f.project();
    assert_eq!(second["replayed"], false);
    assert_eq!(f.count("gf_episodes"), 2);
    let rows = (f.count("gf_nodes"), f.count("gf_edges"));
    f.sync(&[("a.py", A_V1), ("b.py", B)]);
    let third = f.project();
    assert_eq!(third["replayed"], true);
    assert_eq!(third["episode"], first["episode"]);
    assert_eq!(f.count("gf_episodes"), 2);
    assert_eq!((f.count("gf_nodes"), f.count("gf_edges")), rows);
}

#[test]
fn projection_fingerprints_skip_clean_groups_and_preserve_history() {
    use graphicle::graph::Graph;
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let db = f.db();
    let make = |label: &str, target: &str, include_b: bool| {
        let mut nodes = vec![
            json!({"id":"a", "relpath":"a.py", "label":label}),
            json!({"id":"c", "relpath":"c.py"}),
        ];
        let mut edges =
            vec![json!({"source":"a", "target":"c", "source_file":"a.py", "relation":"calls"})];
        if include_b {
            nodes.push(json!({"id":"b", "relpath":"b.py", "nested":{"z":1,"a":2}}));
            edges.push(
                json!({"source":"b", "target":target, "source_file":"b.py", "relation":"calls"}),
            );
        }
        Graph::from_node_link(
            &json!({"directed":true,"multigraph":false,"nodes":nodes,"links":edges}),
            "links",
        )
        .unwrap()
    };
    let stats = |episode: &str| -> Value {
        let raw: String = db.query_row("SELECT json_extract(attributes,'$.extraction_run.persistence') FROM gf_episodes WHERE repository_id=? AND episode=?",
            rusqlite::params![f.repo, episode], |r| r.get(0)).unwrap();
        serde_json::from_str(&raw).unwrap()
    };
    let first = make("first", "c", true);
    let changed = make("second", "c", true);
    let endpoint = make("second", "a", true);
    let removed = make("second", "a", false);
    for (episode, graph) in [
        ("one", &first),
        ("same", &first),
        ("edit", &changed),
        ("endpoint", &endpoint),
        ("remove", &removed),
    ] {
        graph_projection::persist_with_report(&db, &f.repo, episode, graph, Some(&json!({})))
            .unwrap();
        assert_eq!(
            sorted_node_link(&graph_projection::load(&db, &f.repo, episode).unwrap()),
            sorted_node_link(graph)
        );
    }
    assert_eq!(stats("same")["dirty_groups"], 0);
    assert_eq!(stats("same")["nodes_compared"], 0);
    assert_eq!(stats("same")["edges_compared"], 0);
    assert_eq!(stats("edit")["dirty_groups"], 1);
    assert_eq!(stats("edit")["nodes_compared"], 1);
    assert_eq!(stats("endpoint")["dirty_groups"], 1);
    assert_eq!(stats("remove")["dirty_groups"], 1);
    assert_eq!(
        sorted_node_link(&graph_projection::load(&db, &f.repo, "one").unwrap()),
        sorted_node_link(&first)
    );
    // Writes outside the projector invalidate fingerprints. An unchanged
    // authoritative graph must restore the altered live row.
    db.execute("UPDATE gf_nodes SET attributes=json_set(attributes,'$.label','tampered') WHERE repository_id=? AND node_id='a' AND valid_to_episode IS NULL", [&f.repo]).unwrap();
    graph_projection::persist_with_report(&db, &f.repo, "repair", &removed, Some(&json!({})))
        .unwrap();
    assert_eq!(stats("repair")["dirty_groups"], 1);
    assert_eq!(
        sorted_node_link(&graph_projection::load(&db, &f.repo, "repair").unwrap()),
        sorted_node_link(&removed)
    );
    // Reopening an existing store preserves valid cache entries.
    drop(db);
    let reopened = Store::open(f._dir.path()).unwrap().connect().unwrap();
    let valid: i64 = reopened
        .query_row(
            "SELECT count(*) FROM gf_projection_groups WHERE repository_id=? AND length(digest)=32",
            [&f.repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(valid, 2);
}

#[test]
fn failed_projection_rolls_back_rows_and_restores_cache_invalidation() {
    use graphicle::graph::Graph;
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let db = f.db();
    let make = |label: &str, target: &str| {
        Graph::from_node_link(
            &json!({"directed":true,"multigraph":false,"nodes":[
                {"id":"a","relpath":"a.py","label":label},
                {"id":"b","relpath":"b.py"}
            ],"links":[{"source":"a","target":target,"relation":"calls","source_file":"a.py"}]}),
            "links",
        )
        .unwrap()
    };
    let original = make("original", "b");
    graph_projection::persist_with_report(&db, &f.repo, "one", &original, Some(&json!({})))
        .unwrap();
    assert!(db.is_autocommit());
    db.execute_batch(
        "CREATE TABLE rollback_marker(value INTEGER);
         CREATE TRIGGER reject_projection BEFORE INSERT ON gf_edges
         WHEN NEW.valid_from_episode='failed' BEGIN
             SELECT RAISE(ABORT,'forced projection failure');
         END;",
    )
    .unwrap();
    let outer = db.unchecked_transaction().unwrap();
    outer
        .execute("INSERT INTO rollback_marker VALUES(1)", [])
        .unwrap();
    let error = graph_projection::persist_with_report(
        &outer,
        &f.repo,
        "failed",
        &make("changed", "a"),
        Some(&json!({})),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("forced projection failure"),
        "{error}"
    );
    outer.commit().unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM rollback_marker", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM gf_episodes WHERE episode='failed'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM gf_nodes WHERE repository_id=? AND valid_to_episode IS NULL",
            [&f.repo],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        sorted_node_link(&graph_projection::load(&db, &f.repo, "one").unwrap()),
        sorted_node_link(&original),
    );
    // The failed projection must restore invalidation before an outer commit.
    db.execute("UPDATE gf_nodes SET attributes=json_set(attributes,'$.label','tampered') WHERE repository_id=? AND node_id='a' AND valid_to_episode IS NULL", [&f.repo]).unwrap();
    graph_projection::persist_with_report(&db, &f.repo, "repair", &original, Some(&json!({})))
        .unwrap();
    assert_eq!(
        sorted_node_link(&graph_projection::load(&db, &f.repo, "repair").unwrap()),
        sorted_node_link(&original),
    );
}

#[test]
fn earlier_episode_is_reconstructed_as_of_after_later_projection() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let db = f.db();
    let episode1 = s(&f.project(), "episode").to_string();
    let before = sorted_node_link(&graph_projection::load(&db, &f.repo, &episode1).unwrap());
    let nodes_before = f.count("gf_nodes");
    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    let episode2 = s(&f.project(), "episode").to_string();
    // Rows are closed, never deleted.
    assert!(f.count("gf_nodes") >= nodes_before);
    let closed: i64 = db
        .query_row(
            "SELECT count(*) FROM gf_nodes WHERE repository_id=? AND node_id='a_beta' AND valid_to_episode=? AND expired_at IS NOT NULL",
            rusqlite::params![f.repo, episode2],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(closed, 1);
    let after = sorted_node_link(&graph_projection::load(&db, &f.repo, &episode1).unwrap());
    assert_eq!(after, before);
    let head = graph_projection::load(&db, &f.repo, &episode2).unwrap();
    assert!(head.node_index(&json!("a_beta")).is_none());
    assert_eq!(
        head.node_count() + 1,
        before["nodes"].as_array().unwrap().len()
    );
    let listed = f
        .store
        .graph_episodes(&f.workspace, &f.key, &f.repo)
        .unwrap();
    assert_eq!(
        ids(&listed["episodes"], "episode"),
        vec![episode1, episode2.clone()]
    );
    assert_eq!(listed["current_episode"], json!(episode2));
    assert_eq!(listed["episodes"][0]["sequence"], 1);
    assert_eq!(listed["episodes"][1]["sequence"], 2);
    assert!(graph_projection::load(&db, &f.repo, "missing").is_err());
}

#[tokio::test]
async fn http_routes_project_list_and_diff_and_reject_unauthorized_principals() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let app = f.app();
    let other = f.store.create_workspace("Other").unwrap();
    let stranger = s(&other, "capability");
    let reader = s(
        &f.store
            .grant(
                &f.workspace,
                &f.key,
                &json!({"role":"reader","label":"read-only"}),
            )
            .unwrap(),
        "capability",
    )
    .to_string();
    for (method, suffix, body) in [
        ("POST", "graph/project", json!({})),
        ("GET", "graph/episodes", Value::Null),
        ("POST", "impact", json!({"base_episode":"x"})),
    ] {
        let (status, _) = call(&app, method, &f.url(suffix), None, body.clone()).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {suffix} without token"
        );
        let (status, _) = call(&app, method, &f.url(suffix), Some(stranger), body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {suffix} stranger");
        let (status, _) = call(&app, method, &f.url(suffix), Some("bogus"), body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {suffix} bogus");
    }
    assert_eq!(f.count("gf_episodes"), 0);

    let (status, first) = call(
        &app,
        "POST",
        &f.url("graph/project"),
        Some(&f.key),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let episode1 = s(&first, "episode").to_string();
    assert_eq!(first["graph_ref"], json!(graph_ref(&f.repo, &episode1)));

    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    let (status, second) = call(
        &app,
        "POST",
        &f.url("graph/project"),
        Some(&reader),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let episode2 = s(&second, "episode").to_string();

    let (status, listed) = call(
        &app,
        "GET",
        &f.url("graph/episodes"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ids(&listed["episodes"], "episode"),
        vec![episode1.clone(), episode2.clone()]
    );

    if pro_edition() {
        let (status, report) = call(
            &app,
            "POST",
            &f.url("impact"),
            Some(&f.key),
            json!({"base_episode":episode1,"head_episode":episode2,"depth":2,"relations":["calls","imports"],"changed_files":["a.py"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["depth"], 2);
        assert_eq!(report["relations"], json!(["calls", "imports"]));
        assert!(
            ids(&report["affected"], "node_id").contains(&"b_run".to_string()),
            "{report}"
        );
        assert!(
            report["changed_nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["node_id"] == "a" && n["change"] == "modified"),
            "{report}"
        );

        let (status, error) = call(
            &app,
            "POST",
            &f.url("impact"),
            Some(&f.key),
            json!({"base_episode":"unknown"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    } else {
        // Core keeps the route: 501 after authorization, never 404.
        let (status, error) = call(
            &app,
            "POST",
            &f.url("impact"),
            Some(&f.key),
            json!({"base_episode":episode1,"head_episode":episode2}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{error}");
    }
    let missing = format!(
        "/api/workspaces/{}/repositories/nope/graph/episodes",
        f.workspace
    );
    let (status, _) = call(&app, "GET", &missing, Some(&f.key), Value::Null).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
