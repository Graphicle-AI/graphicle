//! End-to-end cross-repository import linkage: two
//! independently indexed repositories in one workspace, linked by package
//! identity, and hidden again when the principal loses read access to a
//! repository. How the PR change-impact report surfaces these links is tested
//! with the edition that serves that report.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use graphicle_runtime::{
    api::{App, router},
    jobs, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const A_PYPROJECT: &str = "[project]\nname = \"alib\"\nversion = \"1.2.0\"\n";
const A_CORE_V1: &str =
    "def process(items):\n    return [i for i in items]\n\ndef helper():\n    return 1\n";
const A_CORE_V2: &str = "def process(items):\n    total = 0\n    for i in items:\n        total += i\n    return total\n";
const B_PYPROJECT: &str =
    "[project]\nname = \"bapp\"\nversion = \"0.1.0\"\ndependencies = [\"alib>=1.0\"]\n";
const B_INGEST: &str = "from alib.core import process, helper\n\ndef ingest(rows):\n    helper()\n    return process(rows)\n";
const A_SHA1: &str = "aaa1110000000000000000000000000000000000";
const A_SHA2: &str = "aaa2220000000000000000000000000000000000";
const B_SHA1: &str = "bbb1110000000000000000000000000000000000";

fn a_files(core: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        ("pyproject.toml", A_PYPROJECT),
        ("alib/__init__.py", ""),
        ("alib/core.py", core),
    ]
}
fn b_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pyproject.toml", B_PYPROJECT),
        ("bapp/__init__.py", ""),
        ("bapp/ingest.py", B_INGEST),
    ]
}

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    a: String,
    b: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Cross repo").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let register = |project_key: &str, name: &str| {
            s(
                &store
                    .register(
                        &workspace,
                        &key,
                        &json!({"project_key":project_key,"name":name}),
                    )
                    .unwrap(),
                "id",
            )
            .to_string()
        };
        let a = register("alib", "Library A");
        let b = register("bapp", "Application B");
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            a,
            b,
        }
    }
    fn commit(&self, repo: &str, files: &[(&str, &str)], sha: &str) {
        let args = |mut v: Value| {
            v["repository_id"] = json!(repo);
            v
        };
        let state = self
            .store
            .project_status(&self.workspace, &self.key, &args(json!({})))
            .unwrap();
        let begin = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &args(json!({
                    "expected_snapshot": state["repository"]["snapshot"],
                    "metadata": {"commit": sha},
                    "manifest": files.iter().map(|(name, text)| json!({
                        "path": name,
                        "sha256": graphicle_memory::text::sha256(text.as_bytes()),
                        "bytes": text.len(),
                    })).collect::<Vec<_>>(),
                })),
            )
            .unwrap();
        self.store
            .sync_upload(
                &self.workspace,
                &self.key,
                &args(json!({
                    "sync_id": begin["sync_id"],
                    "files": files.iter().map(|(name, text)| json!({"path": name, "content": text})).collect::<Vec<_>>(),
                })),
            )
            .unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &args(json!({"sync_id": begin["sync_id"]})),
            )
            .unwrap();
    }
    /// Run every queued job: projections first (each one queues a workspace
    /// `link` job), then the link jobs they left behind. Returns
    /// `(kind, result)` in execution order.
    fn drain_jobs(&self) -> Vec<(String, Value)> {
        let mut results = Vec::new();
        while let Some(job) = self.store.lease_job().unwrap() {
            let result = match job.kind.as_str() {
                "project" => jobs::run_projection(&self.store, &job),
                "link" => jobs::run_link(&self.store, &job),
                other => panic!("unexpected job kind {other}"),
            };
            let value = result.as_ref().ok().cloned().unwrap_or(Value::Null);
            self.store.finish_job(&job, result).unwrap();
            results.push((job.kind.clone(), value));
        }
        results
    }
    fn episode(&self, repo: &str) -> String {
        let listing = self
            .store
            .graph_episodes(&self.workspace, &self.key, repo)
            .unwrap();
        s(&listing, "current_episode").to_string()
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

async fn call(
    app: Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("host", "localhost");
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let request = request
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[test]
fn linking_persists_edges_with_both_episodes_and_relinks_after_a_new_episode() {
    let f = Fixture::new();
    f.commit(&f.a, &a_files(A_CORE_V1), A_SHA1);
    f.commit(&f.b, &b_files(), B_SHA1);
    let results = f.drain_jobs();
    // Two projections, then exactly one `link` job: the second projection
    // found the first one's link job still queued and replayed it.
    let kinds: Vec<&str> = results.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, vec!["project", "project", "link"], "{kinds:?}");
    assert_eq!(results[0].1["cross_repo_link"]["replayed"], false);
    assert_eq!(results[1].1["cross_repo_link"]["replayed"], true);
    assert_eq!(
        results[0].1["cross_repo_link"]["id"],
        results[1].1["cross_repo_link"]["id"]
    );
    // The link job saw both repositories and linked them.
    let link = &results[2].1;
    assert!(link["edges"].as_u64().unwrap() >= 2, "{link}");
    assert_eq!(link["repositories"], 2);

    let a1 = f.episode(&f.a);
    let b1 = f.episode(&f.b);
    let edges = f
        .store
        .graph_cross_edges(&f.workspace, &f.key, &f.a)
        .unwrap();
    let incoming = edges["incoming"].as_array().unwrap();
    let targets: Vec<&str> = incoming.iter().map(|e| s(e, "target_node_id")).collect();
    assert!(targets.contains(&"alib_core_process"), "{targets:?}");
    assert!(targets.contains(&"alib_core_helper"), "{targets:?}");
    for edge in incoming {
        assert_eq!(s(edge, "source_repository_id"), f.b);
        assert_eq!(s(edge, "source_episode"), b1);
        assert_eq!(s(edge, "target_episode"), a1);
        assert_eq!(s(edge, "source_node_id"), "bapp_ingest");
        assert_eq!(s(edge, "relation"), "imports");
        assert_eq!(s(edge, "confidence"), "EXTRACTED");
        assert_eq!(s(edge, "version_constraint"), ">=1.0");
        assert_eq!(s(edge, "exporter_version"), "1.2.0");
        assert_eq!(s(edge, "version_status"), "satisfied");
    }
    assert!(edges["unresolved"].as_array().unwrap().is_empty());

    // Re-running on unchanged episodes is idempotent.
    let again = f.store.graph_link(&f.workspace, &f.key).unwrap();
    assert!(
        again["pairs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["unchanged"] == true),
        "{again}"
    );
    let count = f
        .store
        .graph_cross_edges(&f.workspace, &f.key, &f.a)
        .unwrap()["incoming"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(count, incoming.len());

    // A new episode of A: `helper` disappears, `process` changes.
    f.commit(&f.a, &a_files(A_CORE_V2), A_SHA2);
    f.drain_jobs();
    let a2 = f.episode(&f.a);
    assert_ne!(a1, a2);
    let edges = f
        .store
        .graph_cross_edges(&f.workspace, &f.key, &f.a)
        .unwrap();
    let live: Vec<&Value> = edges["incoming"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["valid_to_episode"].is_null())
        .collect();
    // The surviving symbol edge plus the module-level fallback edge that the
    // missing `helper` import leaves behind (INFERRED, to the file node).
    assert_eq!(live.len(), 2, "{edges}");
    let symbol_edges: Vec<&&Value> = live
        .iter()
        .filter(|e| s(e, "relation") == "imports")
        .collect();
    assert_eq!(symbol_edges.len(), 1);
    assert_eq!(s(symbol_edges[0], "target_node_id"), "alib_core_process");
    for edge in &live {
        assert_eq!(s(edge, "target_episode"), a2);
        assert_eq!(s(edge, "source_episode"), b1);
    }
    let module_edge = live
        .iter()
        .find(|e| s(e, "relation") == "imports_from")
        .unwrap();
    assert_eq!(s(module_edge, "target_node_id"), "alib_core");
    assert_eq!(s(module_edge, "confidence"), "INFERRED");
    // The import of the vanished symbol is now an unresolved cross import naming A.
    let unresolved = edges["unresolved"].as_array().unwrap();
    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    assert_eq!(s(&unresolved[0], "reason"), "symbol_not_exported");
    assert_eq!(s(&unresolved[0], "symbol"), "helper");
    assert!(s(&unresolved[0], "candidates").contains(&f.a));
}

#[test]
fn repository_revocation_blocks_direct_reads_and_outgoing_links() {
    let f = Fixture::new();
    f.commit(&f.a, &a_files(A_CORE_V1), A_SHA1);
    f.commit(&f.b, &b_files(), B_SHA1);
    f.drain_jobs();
    let reader = f
        .store
        .grant(
            &f.workspace,
            &f.key,
            &json!({"role":"reader","label":"restricted"}),
        )
        .unwrap();
    let token = s(&reader, "capability");
    f.store
        .graph_access(
            &f.workspace,
            &f.key,
            &json!({"repository_id": f.a, "credential_id": reader["id"], "state":"revoked"}),
        )
        .unwrap();

    assert!(
        f.store.graph_episodes(&f.workspace, token, &f.a).is_err(),
        "revoked repository remains directly readable"
    );
    assert!(
        f.store
            .project_read(&f.workspace, token, "files", &json!({"repository_id":f.a}))
            .is_err()
    );
    let visible = f.store.projects(&f.workspace, token).unwrap();
    assert!(visible.as_array().unwrap().iter().all(|r| r["id"] != f.a));
    let edges = f
        .store
        .graph_cross_edges(&f.workspace, token, &f.b)
        .unwrap();
    assert_eq!(
        edges["outgoing"],
        json!([]),
        "hidden exporter leaked: {edges}"
    );
    let original = f
        .store
        .graph_cross_edges(&f.workspace, &f.key, &f.b)
        .unwrap();
    let restricted = f.store.graph_link(&f.workspace, token).unwrap();
    assert_eq!(restricted["persisted"], false);
    assert_eq!(
        f.store
            .graph_cross_edges(&f.workspace, &f.key, &f.b)
            .unwrap(),
        original,
        "a restricted link pass overwrote shared graph evidence"
    );
    assert!(f.store.graph_episodes(&f.workspace, &f.key, &f.a).is_ok());
}

#[tokio::test]
async fn repository_revocation_is_enforced_over_http_and_mcp() {
    let f = Fixture::new();
    f.commit(&f.a, &a_files(A_CORE_V1), A_SHA1);
    f.drain_jobs();
    let reader = f
        .store
        .grant(
            &f.workspace,
            &f.key,
            &json!({"role":"reader","label":"restricted"}),
        )
        .unwrap();
    let token = s(&reader, "capability");
    f.store
        .graph_access(
            &f.workspace,
            &f.key,
            &json!({"repository_id": f.a, "credential_id":reader["id"], "state":"revoked"}),
        )
        .unwrap();
    let (status, _) = call(
        f.app(),
        "GET",
        &format!(
            "/api/workspaces/{}/repositories/{}/graph/stats",
            f.workspace, f.a
        ),
        token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        f.app(),
        "POST",
        &format!("/mcp/{}/{}", f.workspace, f.a),
        token,
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"graph_episodes","arguments":{}}})),
    )
    .await;
    assert!(
        status == StatusCode::FORBIDDEN
            || body["result"]["isError"] == true
            || body["error"].is_object(),
        "MCP did not enforce the revoked repository: {status} {body}"
    );
}

#[tokio::test]
async fn http_route_and_mcp_tool_expose_linking() {
    let f = Fixture::new();
    f.commit(&f.a, &a_files(A_CORE_V1), A_SHA1);
    f.commit(&f.b, &b_files(), B_SHA1);
    f.drain_jobs();
    let (status, body) = call(
        f.app(),
        "POST",
        &format!("/api/workspaces/{}/graph/link", f.workspace),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["edges"].as_u64().unwrap() >= 2, "{body}");
    assert_eq!(body["repositories"].as_array().unwrap().len(), 2);

    let (status, body) = call(
        f.app(),
        "GET",
        &format!(
            "/api/workspaces/{}/repositories/{}/graph/cross_edges",
            f.workspace, f.a
        ),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["incoming"].as_array().unwrap().is_empty());

    let (status, body) = call(
        f.app(),
        "POST",
        &format!("/mcp/{}/{}", f.workspace, f.a),
        &f.key,
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"graph_link"), "{names:?}");
    // Linking persists edges, so the tool is not read-only.
    let link_tool = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "graph_link")
        .unwrap();
    assert_eq!(
        link_tool["annotations"]["readOnlyHint"], false,
        "{link_tool}"
    );
    let (status, body) = call(
        f.app(),
        "POST",
        &format!("/mcp/{}/{}", f.workspace, f.a),
        &f.key,
        Some(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"graph_link","arguments":{}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], false, "{body}");
    assert!(
        body["result"]["structuredContent"]["edges"]
            .as_u64()
            .unwrap()
            >= 2
    );
}

#[test]
fn linking_a_single_repository_workspace_is_skipped_and_records_nothing() {
    let f = Fixture::new();
    f.commit(&f.a, &a_files(A_CORE_V1), A_SHA1);
    let results = f.drain_jobs();
    let kinds: Vec<&str> = results.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, vec!["project", "link"], "{kinds:?}");
    let link = &results[1].1;
    assert_eq!(link["skipped"], "fewer_than_two_repositories", "{link}");
    assert_eq!(link["pairs"], 0);
    assert_eq!(link["edges"], 0);
    assert_eq!(link["repositories"], 1);
    let direct = f.store.graph_link(&f.workspace, &f.key).unwrap();
    assert_eq!(direct["skipped"], "fewer_than_two_repositories", "{direct}");
    assert_eq!(direct["pairs"], json!([]));
    assert_eq!(direct["repositories"].as_array().unwrap().len(), 1);
    let db = f.store.connect().unwrap();
    for table in ["gf_cross_edges", "gf_cross_packages", "gf_cross_unresolved"] {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE workspace_id=?"),
                [&f.workspace],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "{table} must stay empty for one repository");
    }
    // Once a second repository is projected the pair links as before.
    f.commit(&f.b, &b_files(), B_SHA1);
    f.drain_jobs();
    let edges = f
        .store
        .graph_cross_edges(&f.workspace, &f.key, &f.a)
        .unwrap();
    assert!(edges["incoming"].as_array().unwrap().len() >= 2, "{edges}");
}

/// A `pr_scratch` copy of the library (the project an
/// earlier `graphicle pr` registered for the same checkout) no longer makes
/// the application's imports ambiguous. It is skipped while the synced
/// library exports the same package, listed with its role, and old stores
/// mark such `git:` projects when they open. Without the role the same
/// workspace leaves the imports unresolved as `ambiguous_exporters`.
#[test]
fn a_pr_scratch_copy_does_not_make_imports_ambiguous() {
    let f = Fixture::new();
    f.commit(&f.a, &a_files(A_CORE_V1), A_SHA1);
    f.commit(&f.b, &b_files(), B_SHA1);
    // The project key `graphicle pr` used: `git:` and a sha256.
    let copy = s(
        &f.store
            .register(
                &f.workspace,
                &f.key,
                &json!({"project_key":format!("git:{}", "ab".repeat(32)),"name":"alib (pr)"}),
            )
            .unwrap(),
        "id",
    )
    .to_string();
    f.commit(&copy, &a_files(A_CORE_V1), A_SHA1);
    f.drain_jobs();

    // Unmarked (a store written before PR scratch marks): the copy is a second
    // exporter of `alib`, so B's imports of it are ambiguous.
    let db = f.store.connect().unwrap();
    db.execute("DELETE FROM repo_project_roles", []).unwrap();
    let ambiguous = f.store.graph_link(&f.workspace, &f.key).unwrap();
    assert_eq!(ambiguous["edges"], 0, "{ambiguous}");
    let reasons: Vec<String> = db
        .prepare("SELECT reason FROM gf_cross_unresolved WHERE source_repository_id=? AND valid_to_episode IS NULL")
        .unwrap()
        .query_map([&f.b], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        reasons.iter().any(|r| r == "ambiguous_exporters"),
        "{reasons:?}"
    );

    // Reopening the store marks the legacy `git:` project as `pr_scratch`.
    let reopened = Store::open(f._dir.path()).unwrap();
    let projects = reopened.projects(&f.workspace, &f.key).unwrap();
    let role = |id: &str| {
        projects
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .map(|p| p.get("role").cloned())
    };
    assert_eq!(role(&copy), Some(Some(json!("pr_scratch"))), "{projects}");
    assert_eq!(role(&f.a), Some(None), "{projects}");

    let linked = reopened.graph_link(&f.workspace, &f.key).unwrap();
    assert_eq!(linked["pr_scratch_skipped"], json!([copy]), "{linked}");
    assert!(linked["edges"].as_u64().unwrap() >= 2, "{linked}");
    let targets: Vec<String> = db
        .prepare("SELECT DISTINCT target_repository_id FROM gf_cross_edges WHERE source_repository_id=? AND valid_to_episode IS NULL")
        .unwrap()
        .query_map([&f.b], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(targets, vec![f.a.clone()], "{linked}");
    let live_ambiguous: i64 = db
        .query_row(
            "SELECT count(*) FROM gf_cross_unresolved WHERE source_repository_id=? AND valid_to_episode IS NULL AND reason='ambiguous_exporters'",
            [&f.b],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live_ambiguous, 0);
    let copy_live: i64 = db
        .query_row(
            "SELECT count(*) FROM gf_cross_edges WHERE (source_repository_id=?1 OR target_repository_id=?1) AND valid_to_episode IS NULL",
            [&copy],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(copy_live, 0);
    // Nothing was deleted: the copy and its graph stay reachable.
    assert!(reopened.graph_episodes(&f.workspace, &f.key, &copy).is_ok());

    // A scratch project alone (its folder never synced) still links: B's
    // imports resolve into it while no other project exports `alib`.
    let alone = Fixture::new();
    alone.commit(&alone.b, &b_files(), B_SHA1);
    alone.commit(&alone.a, &a_files(A_CORE_V1), A_SHA1);
    alone
        .store
        .mark_pr_scratch(&alone.workspace, &alone.key, &alone.a)
        .unwrap();
    alone.drain_jobs();
    let linked = alone
        .store
        .graph_link(&alone.workspace, &alone.key)
        .unwrap();
    assert!(linked.get("pr_scratch_skipped").is_none(), "{linked}");
    assert!(linked["edges"].as_u64().unwrap() >= 2, "{linked}");
}
