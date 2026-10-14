use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
};
use graphicle_runtime::{
    answer_review, atlassian, graph,
    jobs::Worker,
    model, projection,
    store::{self, Store, s},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    ws: String,
    token: String,
    project: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Release fixture").unwrap();
        let token = s(&ws, "capability").to_string();
        let ws = s(&ws, "id").to_string();
        let r = store
            .register(
                &ws,
                &token,
                &json!({"project_key":"fixture","name":"Fixture"}),
            )
            .unwrap();
        Self {
            _dir: dir,
            store,
            ws,
            token,
            project: s(&r, "id").into(),
        }
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.project);
        args
    }
    fn secret(&self, value: Value) -> String {
        s(
            &self
                .store
                .put_credential(
                    &self.ws,
                    &self.token,
                    &json!({"name":"public-test-credential","value":value.to_string()}),
                )
                .unwrap(),
            "credential_ref",
        )
        .into()
    }
    async fn run(&self, cfg: &model::Config, args: Value) -> Value {
        let j = self
            .store
            .enqueue(&self.ws, &self.token, &self.args(args), cfg)
            .unwrap();
        Worker::new(self.store.clone(), cfg.clone())
            .unwrap()
            .once()
            .await
            .unwrap();
        self.store
            .job_status(&self.ws, &self.token, &self.args(json!({"job_id":j["id"]})))
            .unwrap()
    }
}
#[derive(Clone)]
struct RemoteState {
    requests: Arc<Mutex<Vec<Value>>>,
    fail: Arc<AtomicBool>,
    rate_limited: Arc<AtomicBool>,
    invalidate: Arc<AtomicBool>,
    store: Store,
    project: String,
}
async fn remote(State(state): State<RemoteState>, request: Request) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let method = request.method().to_string();
    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.requests.lock().unwrap().push(json!({"path":path,"query":query,"method":method,"body":body,"authorization":authorization}));
    if state.rate_limited.load(Ordering::SeqCst) {
        // GitHub's form of an exhausted primary rate limit.
        let reset = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 120)
            .to_string();
        return (
            StatusCode::FORBIDDEN,
            [
                ("x-ratelimit-remaining", "0".to_string()),
                ("x-ratelimit-reset", reset),
            ],
            Json(json!({"message":"API rate limit exceeded"})),
        )
            .into_response();
    }
    if state.fail.load(Ordering::SeqCst) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"fixture revoked"})),
        )
            .into_response();
    }
    if path.ends_with("/tx/commit") {
        if body["statements"].as_array().unwrap().len() > 1
            && state.invalidate.swap(false, Ordering::SeqCst)
        {
            state
                .store
                .transaction(|db| {
                    store::invalidate(db, &state.project)?;
                    db.execute("UPDATE am_projection_cleanup SET state='complete'", [])?;
                    Ok(())
                })
                .unwrap();
        }
        return Json(json!({"results":[],"errors":[]})).into_response();
    }
    let value = match path.as_str() {
        "/rest/api/3/myself" => json!({"accountId":"author-1","displayName":"Mira"}),
        "/rest/api/3/search/jql" => {
            if body["nextPageToken"] == "page-2" {
                json!({"issues":[],"isLast":true})
            } else {
                json!({"issues":[{"id":"101","key":"P-1","fields":{"summary":"Conditional release","created":"2026-09-01T10:00:00Z","updated":"2026-09-03T10:00:00Z","reporter":{"accountId":"author-1","displayName":"Mira"},"status":{"name":"Waiting"},"description":{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Release only after both checks pass."}]}]}}}],"isLast":false,"nextPageToken":"page-2"})
            }
        }
        "/rest/api/3/issue/P-1/comment" => {
            if query.contains("startAt=1") {
                json!({"startAt":1,"maxResults":1,"total":2,"comments":[{"id":"c2","author":{"accountId":"author-2","displayName":"Arun"},"created":"2026-09-03T10:00:00Z","body":{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Correction: the access check has not passed."}]}]}}]})
            } else {
                json!({"startAt":0,"maxResults":1,"total":2,"comments":[{"id":"c1","author":{"accountId":"author-1","displayName":"Mira"},"created":"2026-09-02T10:00:00Z","body":{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"The build passed."}]}]}}]})
            }
        }
        "/wiki/api/v2/spaces" => json!({"results":[{"id":"10","key":"TEAM","name":"Team"}]}),
        "/wiki/api/v2/spaces/10/pages" => {
            if query.contains("cursor=end") {
                json!({"results":[],"_links":{}})
            } else {
                json!({"results":[{"id":"20"}],"_links":{"next":"/wiki/api/v2/spaces/10/pages?cursor=end"}})
            }
        }
        "/wiki/api/v2/pages/20" => {
            json!({"id":"20","title":"Catering","status":"current","createdAt":"2026-09-01T10:00:00Z","version":{"createdAt":"2026-09-03T10:00:00Z","authorId":"author-2"},"body":{"storage":{"value":"<h1>Catering</h1><p>Mira is vegetarian. Arun cannot have dairy.</p>"}},"_links":{"webui":"/spaces/TEAM/pages/20"}})
        }
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"unknown fixture path"})),
            )
                .into_response();
        }
    };
    Json(value).into_response()
}
async fn server(f: &Fixture) -> (String, RemoteState, tokio::task::JoinHandle<()>) {
    let state = RemoteState {
        requests: Arc::new(Mutex::new(vec![])),
        fail: Arc::new(AtomicBool::new(false)),
        rate_limited: Arc::new(AtomicBool::new(false)),
        invalidate: Arc::new(AtomicBool::new(false)),
        store: f.store.clone(),
        project: f.project.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(remote).with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, state, task)
}
#[test]
fn answer_review_rejects_missing_coverage_and_invented_quotes() {
    let sources = vec![json!({"id":"s1","text":"Mira is vegetarian. Arun cannot have dairy."})];
    let mut value = json!({"reviewed_answer":{"answer":"Mira: vegetarian; Arun: dairy free.","abstain":false,"evidence":[{"id":"s1","quote":"Mira is vegetarian. Arun cannot have dairy."}],"unresolved":[]},"coverage":[{"requested_part":"Food restrictions","status":"answered","evidence":[{"id":"s1","quote":"Mira is vegetarian. Arun cannot have dairy."}]}]});
    answer_review::validate(&value, &sources).unwrap();
    value["coverage"][0]["evidence"][0]["quote"] = json!("Mira is dairy free.");
    assert!(answer_review::validate(&value, &sources).is_err());
    value["coverage"] = json!([]);
    assert!(answer_review::validate(&value, &sources).is_err());
}
#[test]
fn atlassian_pagination_and_rich_text_are_bounded_and_scoped() {
    let (path, q) = atlassian::next_page(
        "https://api.atlassian.com/ex/confluence/site-id",
        "/wiki/api/v2/pages?cursor=abc",
    )
    .unwrap();
    assert_eq!(path, "wiki/api/v2/pages");
    assert_eq!(q[0], ("cursor".into(), "abc".into()));
    for bad in [
        "https://evil.test/steal",
        "/ex/confluence/another-site/wiki/api/v2/pages",
        "//evil.test/a",
        "/wiki/../../ex/jira/x",
    ] {
        assert!(
            atlassian::next_page("https://api.atlassian.com/ex/confluence/site-id", bad).is_err(),
            "{bad}"
        );
    }
    // A Jira document's text may be as long as a document's
    // (limits.document_text_bytes, 8 MiB; earlier builds 512 KiB), in a real
    // ADF document with block nodes.
    let paragraph =
        |text: String| json!({"type":"paragraph","content":[{"type":"text","text":text}]});
    let long = json!({"type":"doc","content":[paragraph("x".repeat(600 * 1024)),paragraph("tail".into())]});
    let text = atlassian::adf_text(&long).unwrap();
    assert!(text.len() > 600 * 1024 && text.ends_with("tail\n"));
    let too_long = json!({"type":"doc","content":[
        paragraph("x".repeat(graphicle_runtime::documents::max_text() - 10)),
        paragraph("y".repeat(20)),
    ]});
    let refused = atlassian::adf_text(&too_long).unwrap_err().message;
    assert!(
        refused.starts_with("Source document too large: a Jira document has"),
        "{refused}"
    );
    assert!(refused.contains("limits.document_text_bytes"), "{refused}");
    let text=atlassian::adf_text(&json!({"type":"doc","content":[{"type":"paragraph","content":[{"type":"mention","attrs":{"text":"@Mira"}},{"type":"text","text":" may go only if approved."}]}]})).unwrap();
    assert!(text.contains("@Mira may go only if approved."));
}
#[tokio::test]
async fn atlassian_full_scan_keeps_comments_dates_and_handles_revocation() {
    let f = Fixture::new();
    let (base, state, task) = server(&f).await;
    let credential = f.secret(
        json!({"username":"public-fixture@example.test","password":"public-fixture-token"}),
    );
    let cfg = model::Config::default();
    for kind in ["jira", "confluence"] {
        let mut config = json!({"base_url":base,"site_url":base,"auth_type":"basic","acl":{"workspace_readers":false,"credentials":[]}});
        config[if kind == "jira" { "jql" } else { "space_key" }] = json!(if kind == "jira" {
            "project = P"
        } else {
            "TEAM"
        });
        let c = f
            .store
            .connector_add(
                &f.ws,
                &f.token,
                &f.args(
                    json!({"kind":kind,"name":kind,"credential_ref":credential,"config":config}),
                ),
            )
            .unwrap();
        let done = f
            .run(
                &cfg,
                json!({"kind":"connector","connector_id":c["id"],"max_requests":16}),
            )
            .await;
        assert_eq!(done["state"], "complete", "{done}");
        assert_eq!(done["result"]["complete"], true, "{done}");
        let probe = f
            .run(
                &cfg,
                json!({"kind":"connector_probe","connector_id":c["id"]}),
            )
            .await;
        assert_eq!(probe["state"], "complete", "{probe}");
        assert_eq!(probe["result"]["evidence"], "local_fixture_response");
        let db = f.store.connect().unwrap();
        let texts = serde_json::to_string(
            &store::rows(
                &db,
                "SELECT content FROM am_connector_docs WHERE connector_id=?",
                [s(&c, "id")],
            )
            .unwrap(),
        )
        .unwrap();
        if kind == "jira" {
            assert!(texts.contains("access check has not passed"));
            assert!(texts.contains("Arun"));
            assert!(texts.contains("2026-09-03"));
        } else {
            assert!(texts.contains("Mira is vegetarian"));
            assert!(texts.contains("author-2"));
        }
        let last_sync = |db: &rusqlite::Connection| -> Option<String> {
            db.query_row(
                "SELECT last_sync FROM am_connectors WHERE id=?",
                [s(&c, "id")],
                |r| r.get(0),
            )
            .unwrap()
        };
        // A rate limit is not a denial: the synced documents stay available.
        state.rate_limited.store(true, Ordering::SeqCst);
        let limited = f
            .run(
                &cfg,
                json!({"kind":"connector_probe","connector_id":c["id"],"max_attempts":1}),
            )
            .await;
        assert_ne!(limited["state"], "complete");
        assert!(s(&limited, "error").contains("rate limited"), "{limited}");
        assert!(last_sync(&db).is_some());
        state.rate_limited.store(false, Ordering::SeqCst);
        state.fail.store(true, Ordering::SeqCst);
        let failed = f
            .run(
                &cfg,
                json!({"kind":"connector_probe","connector_id":c["id"],"max_attempts":1}),
            )
            .await;
        assert_ne!(failed["state"], "complete");
        assert!(last_sync(&db).is_none());
        state.fail.store(false, Ordering::SeqCst);
    }
    assert!(
        state
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| s(r, "authorization").starts_with("Basic "))
    );
    task.abort();
}
#[tokio::test]
async fn external_cleanup_survives_project_deletion_and_restart_and_keeps_scope() {
    let f = Fixture::new();
    let (base, state, task) = server(&f).await;
    let credential = f.secret(json!({"username":"neo4j","password":"public-fixture-password"}));
    graph::configure_neo4j(
        &f.store,
        &f.ws,
        &f.token,
        &f.args(json!({"endpoint":base,"credential_ref":credential})),
    )
    .unwrap();
    f.store
        .maintain(
            &f.ws,
            &f.token,
            "project.delete",
            &f.args(json!({"expected_snapshot":f.store.project_status(&f.ws,&f.token,&f.args(json!({}))).unwrap()["repository"]["snapshot"]})),
        )
        .unwrap();
    let reopened = Store::open(f._dir.path()).unwrap();
    state.fail.store(true, Ordering::SeqCst);
    assert!(
        projection::once(&reopened, &model::client().unwrap())
            .await
            .unwrap()
    );
    let row = projection::status(&reopened, &f.ws, &f.token).unwrap()["cleanups"][0].clone();
    assert_eq!(row["state"], "pending");
    assert!(row["error"].is_string());
    state.fail.store(false, Ordering::SeqCst);
    projection::retry(&reopened, &f.ws, &f.token, &json!({"cleanup_id":row["id"]})).unwrap();
    assert!(
        projection::once(&reopened, &model::client().unwrap())
            .await
            .unwrap()
    );
    assert_eq!(
        projection::status(&reopened, &f.ws, &f.token).unwrap()["cleanups"][0]["state"],
        "complete"
    );
    let req = state.requests.lock().unwrap();
    for r in req.iter() {
        assert_eq!(r["body"]["statements"][0]["parameters"]["scope"], f.project);
        assert!(s(&r["body"]["statements"][0], "statement").contains("Graphicle {scope:$scope}"));
    }
    task.abort();
}
#[tokio::test]
async fn completed_cleanup_during_publication_cannot_be_overwritten() {
    let f = Fixture::new();
    let (base, state, task) = server(&f).await;
    let credential = f.secret(json!({"username":"neo4j","password":"public-fixture-password"}));
    graph::configure_neo4j(
        &f.store,
        &f.ws,
        &f.token,
        &f.args(json!({"endpoint":base,"credential_ref":credential})),
    )
    .unwrap();
    state.invalidate.store(true, Ordering::SeqCst);
    assert!(
        graph::neo4j(
            &f.store,
            &model::client().unwrap(),
            &f.ws,
            &f.token,
            &f.args(json!({})),
            true
        )
        .await
        .is_err()
    );
    assert_eq!(
        projection::status(&f.store, &f.ws, &f.token).unwrap()["cleanups"][0]["state"],
        "pending"
    );
    task.abort();
}
