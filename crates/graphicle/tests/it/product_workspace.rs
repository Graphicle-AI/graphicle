use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use graphicle_runtime::platform::CanonicalExt;
use graphicle_runtime::{
    api::{App, router},
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

/// Installs the edition under test before a fixture is built: nothing in this
/// Core test binary. An extension crate can re-run this suite from its own
/// test binary, whose root `edition_setup` installs its extensions (`crate::`
/// names the root of whichever test binary the suite is compiled into).
#[allow(dead_code)]
fn edition_setup() {}

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    owner: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Product fixture").unwrap();
        let workspace = s(&ws, "id").to_string();
        let owner = s(&ws, "capability").to_string();
        let project = store
            .register(
                &workspace,
                &owner,
                &json!({"project_key":"fixture","name":"Fixture"}),
            )
            .unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            owner,
            repo: s(&project, "id").to_string(),
        }
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
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
    fn base(&self) -> String {
        format!("/v1/workspaces/{}", self.workspace)
    }
    fn command_url(&self) -> String {
        format!("{}/repos/{}/commands", self.base(), self.repo)
    }
    fn sync(&self, content: &str) -> Value {
        let snapshot = self
            .store
            .project_status(&self.workspace, &self.owner, &self.args(json!({})))
            .unwrap()["repository"]["snapshot"]
            .clone();
        let begin=self.store.sync_prepare(&self.workspace,&self.owner,&self.args(json!({
            "expected_snapshot":snapshot,"manifest":[{"path":"auth.py","sha256":graphicle_memory::text::sha256(content.as_bytes()),"bytes":content.len()}]
        }))).unwrap();
        self.store.sync_upload(&self.workspace,&self.owner,&self.args(json!({"sync_id":begin["sync_id"],"files":[{"path":"auth.py","content":content}]}))).unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.owner,
                &self.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
        self.store
            .graph_project(&self.workspace, &self.owner, &self.repo)
            .unwrap()
    }
}
async fn call(
    app: &Router,
    path: &str,
    cap: &str,
    body: Option<Value>,
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut req = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json");
    if !cap.is_empty() {
        req = req.header("x-workspace-capability", cap);
    }
    let response = app
        .clone()
        .oneshot(
            req.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        headers,
    )
}

#[tokio::test]
async fn v1_binds_authority_and_usage_is_owner_only() {
    let f = Fixture::new();
    let app = f.app();
    let reader = f
        .store
        .grant(
            &f.workspace,
            &f.owner,
            &json!({"role":"reader","label":"Reader"}),
        )
        .unwrap();
    let other = f.store.create_workspace("Other").unwrap();
    let url = format!("{}/repos/{}/overview", f.base(), f.repo);
    assert_eq!(
        call(&app, &url, s(&other, "capability"), None).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, overview, _) = call(&app, &url, s(&reader, "capability"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(overview.get("connectors").is_none());
    assert_eq!(
        call(
            &app,
            &format!("{}/usage", f.base()),
            s(&reader, "capability"),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            &f.command_url(),
            &f.owner,
            Some(json!({"operation":"status","arguments":{"repository_id":"another"}}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    f.store
        .graph_access(
            &f.workspace,
            &f.owner,
            &f.args(json!({"credential_id":reader["id"],"state":"revoked"})),
        )
        .unwrap();
    assert_eq!(
        call(&app, &url, s(&reader, "capability"), None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            &f.command_url(),
            s(&reader, "capability"),
            Some(json!({"operation":"status"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

/// One request with optional extra headers: status, headers and the raw body
/// (the exact text the server served).
async fn raw(
    app: &Router,
    path: &str,
    cap: &str,
    body: Value,
    extra: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("x-workspace-capability", cap);
    for (name, value) in extra {
        req = req.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

/// What Graphicle serves is metered (`graphicle_runtime::metering`): the
/// tokens of the text served, counted with cl100k_base, per credential, agent
/// session, channel and tool or operation. The owner's usage report lists
/// them beside the recorded model tokens, with no cost. An MCP HTTP client's
/// agent session is the `Mcp-Session-Id` issued and signed at `initialize`;
/// an id the server did not sign shares one label. Requests and results
/// themselves are never stored.
#[tokio::test]
async fn served_tokens_are_reported_per_credential_and_agent_session() {
    let f = Fixture::new();
    let app = f.app();
    f.sync("def authenticate():\n    return 'rotate keys every Friday'\n");
    let agent = f
        .store
        .grant(
            &f.workspace,
            &f.owner,
            &json!({"role":"agent","label":"Build agent"}),
        )
        .unwrap();
    let key = s(&agent, "capability");
    let question = "How does authenticate rotate keys?";
    let (status, _, body) = raw(
        &app,
        &f.command_url(),
        key,
        json!({"operation":"context","arguments":{"question":question}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let http_tokens = graphicle_memory::text::count(&body) as u64;
    // `memories` returns the project's memory rows: metered, as the
    // `project_memory` tool that runs it.
    let (status, _, body) = raw(
        &app,
        &f.command_url(),
        key,
        json!({"operation":"memories"}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let memories_tokens = graphicle_memory::text::count(&body) as u64;
    // An operation that returns only counts is not metered.
    let (status, _, _) = raw(
        &app,
        &f.command_url(),
        key,
        json!({"operation":"observer.status","arguments":{"include":["file:"]}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mcp = format!("/mcp/{}/{}", f.workspace, f.repo);
    let (status, headers, body) = raw(
        &app,
        &mcp,
        key,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"Test Agent","version":"1.0"}}}),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let session = headers["mcp-session-id"].to_str().unwrap().to_string();
    let (signed, signature) = session.rsplit_once('.').unwrap();
    let random = signed.strip_prefix("testagent.").unwrap();
    assert_eq!((random.len(), signature.len()), (32, 64), "{session}");
    let (status, _, body) = raw(
        &app,
        &mcp,
        key,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":question}}}),
        &[("mcp-session-id", &session)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let mcp_tokens =
        graphicle_memory::text::count(s(&reply["result"]["content"][0], "text")) as u64;
    assert!(http_tokens > 0 && mcp_tokens > 0 && memories_tokens > 0);
    // A session id the server did not sign: one shared label, whatever the
    // client sends, so it cannot add a counter row per call.
    for forged in [
        "testagent.0123456789abcdef0123456789abcdef",
        "my-own-session",
    ] {
        let (status, _, body) = raw(
            &app,
            &mcp,
            key,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"repository_status","arguments":{}}}),
            &[("mcp-session-id", forged)],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    // Results are counted on threads of their own, and a usage report does
    // not wait for them: record them first.
    graphicle_runtime::metering::flush(&f.store).unwrap();
    let (status, usage, _) = call(&app, &format!("{}/usage", f.base()), &f.owner, None).await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["cost"], Value::Null, "no price table: no cost");
    let served = &usage["served"];
    assert_eq!(served["counted_with"], "cl100k_base");
    assert_eq!(served["calls"], 5, "{served:#}");
    assert_eq!(served["unrecorded_tokens"], 0);
    let mut kinds: Vec<(String, String, u64)> = served["by_kind"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                s(row, "channel").to_string(),
                s(row, "kind").to_string(),
                row["tokens"].as_u64().unwrap(),
            )
        })
        .collect();
    kinds.sort();
    let status_tokens = kinds
        .iter()
        .find(|(_, kind, _)| kind == "repository_status")
        .map(|row| row.2)
        .unwrap_or_default();
    assert!(status_tokens > 0, "{kinds:?}");
    assert_eq!(
        served["tokens"].as_u64(),
        Some(http_tokens + memories_tokens + mcp_tokens + status_tokens)
    );
    assert_eq!(
        kinds,
        [
            ("http".to_string(), "context".to_string(), http_tokens),
            ("http".to_string(), "memories".to_string(), memories_tokens),
            ("mcp".to_string(), "recall_project".to_string(), mcp_tokens),
            (
                "mcp".to_string(),
                "repository_status".to_string(),
                status_tokens
            ),
        ]
    );
    let credentials = served["by_credential"].as_array().unwrap();
    assert_eq!(credentials.len(), 1, "{credentials:?}");
    assert_eq!(credentials[0]["credential_id"], agent["id"]);
    assert_eq!(credentials[0]["label"], "Build agent");
    assert_eq!(credentials[0]["role"], "agent");
    let mut sessions: Vec<(String, u64, u64)> = served["by_session"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert_eq!(row["credential_id"], agent["id"]);
            (
                s(row, "session").to_string(),
                row["calls"].as_u64().unwrap(),
                row["tokens"].as_u64().unwrap(),
            )
        })
        .collect();
    sessions.sort();
    assert_eq!(
        sessions,
        [
            ("mcp:unverified".to_string(), 2, status_tokens),
            (format!("testagent:{}", &random[..12]), 1, mcp_tokens),
        ]
    );
    assert_eq!(served["by_project"][0]["repository_id"], json!(f.repo));
    assert!(usage["model_usage_by_credential"].is_array());
    assert!(usage["model_usage_by_model"].is_array());
    // Counters only: never the question, a result or the client's session id.
    let stored = graphicle_runtime::store::rows(
        &f.store.connect().unwrap(),
        "SELECT * FROM am_served_usage",
        [],
    )
    .unwrap();
    let stored = json!(stored).to_string();
    assert!(
        !stored.contains("authenticate") && !stored.contains(&session),
        "{stored}"
    );
    // Graphicle's own model calls, by the credential that queued the job and
    // by model: tokens, and still no cost.
    let db = f.store.connect().unwrap();
    let stamp = graphicle_runtime::now();
    db.execute(
        "INSERT INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,not_before,created_at,updated_at)
         VALUES('job-1',?,'answer','job-1','{}',?,'complete',?,?,?)",
        rusqlite::params![
            f.repo,
            graphicle_memory::text::sha256(key.as_bytes()),
            stamp,
            stamp,
            stamp
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO am_model_calls VALUES('call-1',?,'job-1','openai','gpt-test','h',1200,300,0,0,?)",
        rusqlite::params![f.repo, stamp],
    )
    .unwrap();
    let (_, usage, _) = call(&app, &format!("{}/usage", f.base()), &f.owner, None).await;
    assert_eq!(
        usage["model_usage_by_credential"],
        json!([{"credential_id":agent["id"],"calls":1,"input_tokens":1200,"output_tokens":300}])
    );
    assert_eq!(
        usage["model_usage_by_model"],
        json!([{"provider":"openai","model":"gpt-test","calls":1,"input_tokens":1200,"output_tokens":300}])
    );
    assert_eq!(usage["cost"], Value::Null);
}

#[tokio::test]
async fn quota_covers_v1_legacy_and_mcp_but_preserves_recovery() {
    let f = Fixture::new();
    let app = f.app();
    f.store
        .product_budget(&f.workspace, &f.owner, &json!({"monthly_requests":1}))
        .unwrap();
    let (status, _, headers) = call(
        &app,
        &f.command_url(),
        &f.owner,
        Some(json!({"operation":"memories"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-graphicle-requests-remaining"], "0");
    let legacy = format!("/api/workspaces/{}/repositories/command", f.workspace);
    let (status, _, headers) = call(
        &app,
        &legacy,
        &f.owner,
        Some(json!({"operation":"memories","arguments":{"repository_id":f.repo}})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get("retry-after").is_some());
    assert!(headers.get("content-security-policy").is_some());
    let mcp = format!("/mcp/{}/{}", f.workspace, f.repo);
    assert_eq!(call(&app,&mcp,&f.owner,Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":"auth"}}}))).await.0,StatusCode::TOO_MANY_REQUESTS);
    // The workspace connection is metered the same way, and
    // its navigation (`list_projects`) stays available.
    let workspace_mcp = format!("/mcp/{}", f.workspace);
    assert_eq!(call(&app,&workspace_mcp,&f.owner,Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":"auth"}}}))).await.0,StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(call(&app,&workspace_mcp,&f.owner,Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_projects","arguments":{}}}))).await.0,StatusCode::OK);
    for operation in ["status", "memory.export"] {
        assert_eq!(
            call(
                &app,
                &f.command_url(),
                &f.owner,
                Some(json!({"operation":operation}))
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(
            &app,
            &mcp,
            &f.owner,
            Some(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, &format!("{}/usage", f.base()), &f.owner, None)
            .await
            .1["requests"],
        1
    );
    assert_eq!(
        call(
            &app,
            &format!("{}/budget", f.base()),
            &f.owner,
            Some(json!({"monthly_requests":null}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            &f.command_url(),
            &f.owner,
            Some(json!({"operation":"memories"}))
        )
        .await
        .0,
        StatusCode::OK
    );
}

/// The web app's sign-in (`web/app.js` `login()`) reads the workspace, the
/// edition (`build.info`), the projects and the models. None of them is
/// metered, so an owner whose budget is used up or set to 0 (the documented
/// way to pause requests) can still sign in and reach Usage.
#[tokio::test]
async fn web_sign_in_requests_stay_available_at_an_exhausted_budget() {
    let f = Fixture::new();
    let app = f.app();
    f.store
        .product_budget(&f.workspace, &f.owner, &json!({"monthly_requests":0}))
        .unwrap();
    let base = format!("/api/workspaces/{}", f.workspace);
    let command = format!("{base}/repositories/command");
    assert_eq!(
        call(
            &app,
            &command,
            &f.owner,
            Some(json!({"operation":"memories","arguments":{"repository_id":f.repo}}))
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS,
        "metered requests are refused"
    );
    assert_eq!(call(&app, &base, &f.owner, None).await.0, StatusCode::OK);
    let (status, build, _) = call(
        &app,
        &command,
        &f.owner,
        Some(json!({"operation":"build.info","arguments":{}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{build}");
    assert!(build["edition"].is_string(), "{build}");
    for path in [format!("{base}/repositories"), format!("{base}/models")] {
        assert_eq!(
            call(&app, &path, &f.owner, None).await.0,
            StatusCode::OK,
            "{path}"
        );
    }
    let usage = call(&app, &format!("{}/usage", f.base()), &f.owner, None).await;
    assert_eq!(usage.0, StatusCode::OK);
    assert_eq!(usage.1["requests"], 0, "sign-in used no budget");
    // The web app asks for build.info right after the workspace.
    let script = include_str!("../../web/app.js");
    let login = script
        .lines()
        .find(|line| line.starts_with("async function login("))
        .unwrap();
    assert!(
        login.contains("workspaceCommand(\"build.info\")"),
        "{login}"
    );
}

/// A local app gives its owner the store's folder for the MCP
/// snippet (`local_mcp.data_dir`, when the store is not in the default
/// folder) and the web app's commands name it. No other credential, such
/// as an agent's key, is told the store's path.
#[tokio::test]
async fn the_store_folder_is_shown_to_the_owner_only() {
    let f = Fixture::new();
    let app = f.app();
    let base = format!("/api/workspaces/{}", f.workspace);
    let (status, seen, _) = call(&app, &base, &f.owner, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        seen["local_mcp"]["data_dir"],
        json!(f.store.directory.canonical().unwrap()),
        "{seen}"
    );
    for role in ["reader", "agent"] {
        let key = f
            .store
            .grant(&f.workspace, &f.owner, &json!({"role":role,"label":role}))
            .unwrap();
        let (status, seen, _) = call(&app, &base, s(&key, "capability"), None).await;
        assert_eq!(status, StatusCode::OK, "{role}: {seen}");
        assert!(seen.get("local_mcp").is_none(), "{role}: {seen}");
    }
    let script = include_str!("../../web/app.js");
    let refresh = script
        .lines()
        .find(|line| line.starts_with("async function refresh("))
        .unwrap();
    assert!(
        refresh.contains("graphicle ${dataDir}mcp --project"),
        "{refresh}"
    );
}

#[test]
fn concurrent_requests_cannot_overspend_a_budget() {
    let f = Fixture::new();
    f.store
        .product_budget(&f.workspace, &f.owner, &json!({"monthly_requests":5}))
        .unwrap();
    let threads: Vec<_> = (0..12)
        .map(|_| {
            let store = f.store.clone();
            let w = f.workspace.clone();
            let c = f.owner.clone();
            std::thread::spawn(move || store.reserve_request(&w, &c).is_ok())
        })
        .collect();
    let admitted = threads
        .into_iter()
        .filter_map(|t| t.join().ok())
        .filter(|ok| *ok)
        .count();
    assert_eq!(admitted, 5);
    assert_eq!(
        f.store.product_usage(&f.workspace, &f.owner).unwrap()["requests"],
        5
    );
}

#[test]
fn graph_citation_opens_the_original_even_after_source_changes() {
    let f = Fixture::new();
    let old = "def authenticate():\n    return 'before-rotation'\n";
    let first = f.sync(old);
    f.sync("def authenticate():\n    return 'after-rotation'\n");
    let source = f
        .store
        .graph_source(
            &f.workspace,
            &f.owner,
            &f.args(json!({"episode":first["episode"],"path":"auth.py","start_line":1})),
        )
        .unwrap();
    assert!(s(&source, "quote").contains("before-rotation"), "{source}");
    assert!(!s(&source, "quote").contains("after-rotation"));
    assert_eq!(
        source["sha256"],
        graphicle_memory::text::sha256(old.as_bytes())
    );
    assert!(
        f.store
            .graph_source(
                &f.workspace,
                &f.owner,
                &f.args(json!({"episode":first["episode"],"path":"missing.py"}))
            )
            .is_err()
    );
    assert!(
        f.store
            .graph_source(
                &f.workspace,
                &f.owner,
                &f.args(json!({"episode":first["episode"],"path":"../auth.py"}))
            )
            .is_err()
    );
    let summary = f
        .store
        .product_overview(&f.workspace, &f.owner, &f.repo)
        .unwrap();
    assert_eq!(summary["graph"]["current_recorded"], true);
}

#[tokio::test]
async fn overview_respects_connector_acl_and_context_records_nothing() {
    let f = Fixture::new();
    let app = f.app();
    let connector=f.store.connector_add(&f.workspace,&f.owner,&f.args(json!({"name":"Private notes","kind":"jsonl","config":{"acl":{"workspace_readers":false,"credentials":[]}}}))).unwrap();
    f.store.import_documents(&f.workspace,&f.owner,&f.args(json!({"connector_id":connector["id"],"mode":"delta","documents":[{"id":"auth","title":"Auth","content":"The owner rotates access keys every Friday."}]}))).unwrap();
    let reader = f
        .store
        .grant(
            &f.workspace,
            &f.owner,
            &json!({"role":"reader","label":"Reader"}),
        )
        .unwrap();
    assert_eq!(
        f.store
            .product_overview(&f.workspace, s(&reader, "capability"), &f.repo)
            .unwrap()["status"]["evidence_sources"],
        0
    );
    assert_eq!(
        f.store
            .product_overview(&f.workspace, &f.owner, &f.repo)
            .unwrap()["status"]["evidence_sources"],
        1
    );
    let packet=call(&app,&f.command_url(),&f.owner,Some(json!({"operation":"context","arguments":{"question":"When do we rotate access keys?"}}))).await.1;
    assert!(
        packet["evidence"].as_array().is_some_and(|e| !e.is_empty()),
        "{packet}"
    );
    // A context call stores nothing about the request: the served record
    // and its request_hash went with learning and hints
    // (unit `learning-and-hints`).
    assert!(packet.get("request_hash").is_none(), "{packet}");
    let served: i64 = f
        .store
        .connect()
        .unwrap()
        .query_row("SELECT count(*) FROM gf_served", [], |row| row.get(0))
        .unwrap();
    assert_eq!(served, 0);
}

#[tokio::test]
async fn public_assets_and_openapi_are_available_without_exposing_a_workspace() {
    let f = Fixture::new();
    let app = f.app();
    for path in [
        "/product",
        "/docs",
        "/workspace.js",
        "/workspace.css",
        "/theme.js",
        "/v1/openapi.json",
    ] {
        let (status, _, headers) = call(&app, path, "", None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("script-src 'self'")
        );
    }
    let spec = call(&app, "/v1/openapi.json", "", None).await.1;
    assert_eq!(spec["openapi"], "3.1.0");
    assert!(!spec.to_string().contains(&f.owner));
}

/// Every page sets its colour theme before the first paint: `/theme.js`
/// (the Content-Security-Policy allows no inline script) loads, without
/// defer, before the stylesheets, and the page declares both schemes.
#[test]
fn every_page_sets_its_theme_before_the_first_paint() {
    for (name, page) in [
        ("index.html", include_str!("../../web/index.html")),
        ("docs.html", include_str!("../../web/docs.html")),
        ("product.html", include_str!("../../web/product.html")),
    ] {
        let theme = page
            .find(r#"<script src="/theme.js"></script>"#)
            .unwrap_or_else(|| panic!("{name} loads /theme.js without defer"));
        let styles = page.find(r#"<link rel="stylesheet""#).unwrap();
        assert!(theme < styles, "{name}: /theme.js before the stylesheets");
        assert!(
            page.contains(r#"<meta name="color-scheme" content="light dark">"#),
            "{name}"
        );
        for scheme in ["light", "dark"] {
            let meta = format!(r#"media="(prefers-color-scheme: {scheme})">"#);
            assert!(page.contains(&meta), "{name}: theme-color for {scheme}");
        }
        for script in page.split("<script").skip(1) {
            assert!(
                script.trim_start().starts_with("src="),
                "{name}: inline script"
            );
        }
    }
    // The control in Settings offers the three choices; the script keeps the
    // choice under the graph export page's key.
    let index = include_str!("../../web/index.html");
    for choice in ["system", "light", "dark"] {
        assert!(
            index.contains(&format!(r#"data-theme-choice="{choice}""#)),
            "{choice}"
        );
    }
    let script = include_str!("../../web/theme.js");
    assert!(script.contains(r#"const KEY = "graphicle-theme";"#));
    assert!(script.contains("localStorage") && script.contains("catch"));
}

/// The light theme only gives other values to the dark theme's tokens, and
/// the stylesheets name no colour outside the theme blocks, so a new rule
/// cannot come out dark-only.
#[test]
fn the_light_theme_overrides_tokens_and_no_colour_is_hard_coded() {
    let style = include_str!("../../web/style.css");
    let block = |selector: &str| {
        let start = style.find(&format!("{selector} {{")).expect(selector);
        let end = start + style[start..].find("\n}").unwrap();
        &style[start..end]
    };
    let names = |text: &str| -> std::collections::BTreeSet<String> {
        text.lines()
            .filter_map(|line| line.trim().strip_prefix("--"))
            .filter_map(|line| line.split_once(':').map(|(name, _)| name.to_string()))
            .collect()
    };
    let (dark, light) = (block("\n:root"), block(r#":root[data-theme="light"]"#));
    assert!(light.contains("color-scheme: light;"));
    let missing: Vec<_> = names(light).difference(&names(dark)).cloned().collect();
    assert!(missing.is_empty(), "light-only tokens: {missing:?}");
    let colour = regex::Regex::new(r"#[0-9a-fA-F]{3,8}\b|rgba?\((?:[^v)]|v[^a])").unwrap();
    for (name, css) in [
        ("style.css", style),
        ("workspace.css", include_str!("../../web/workspace.css")),
    ] {
        let mut theme_block = false;
        for (number, line) in css.lines().enumerate() {
            let text = line.trim();
            if text.starts_with(":root") {
                theme_block = true;
            }
            if theme_block {
                theme_block = text != "}";
                continue;
            }
            // Masks use only the alpha channel.
            if text.contains("url(\"data:") || text.contains("mask-image:") {
                continue;
            }
            assert!(!colour.is_match(text), "{name}:{}: {text}", number + 1);
        }
    }
}

/// Both editions serve this `/docs` page, so it states the request budget
/// rules that hold in each, and describes no other edition.
#[test]
fn the_docs_page_states_the_request_budget_rules_of_both_editions() {
    let page = include_str!("../../web/docs.html");
    for sentence in [
        "Null removes your cap.",
        "Sign-in, navigation, usage, export, cancellation and deletion remain available.",
        "A commercial edition for teams is in private preview.",
    ] {
        assert!(page.contains(sentence), "{sentence}");
    }
    assert!(!page.contains("Graphicle Pro"));
}

/// A deployment configured for hosted billing or the storefront needs the
/// edition that reads that configuration. Core refuses to serve it instead
/// of serving every workspace with no subscription allowance or usage
/// reports; the commercial edition reads it (here: fails on the missing file).
#[test]
fn serve_refuses_configuration_only_graphicle_pro_reads() {
    crate::edition_setup();
    let pro = graphicle_runtime::core_capabilities::capabilities()["edition"] == "pro";
    let dir = TempDir::new().unwrap();
    for name in ["GRAPHICLE_BILLING_CONFIG", "GRAPHICLE_COMMERCE_CONFIG"] {
        let mut child = crate::fork_guard::spawn(
            std::process::Command::new(env!("CARGO_BIN_EXE_graphicle"))
                .arg("--data-dir")
                .arg(dir.path().join("data"))
                .args(["serve", "--port", "0"])
                .env("HOME", dir.path())
                .env_remove("GRAPHICLE_BILLING_CONFIG")
                .env_remove("GRAPHICLE_COMMERCE_CONFIG")
                .env(name, dir.path().join("missing.json"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped()),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > std::time::Duration::from_secs(120) {
                child.kill().unwrap();
                panic!("{name}: the server started");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
        assert!(!status.success(), "{name}: {stderr}");
        assert_eq!(
            stderr.contains(&format!(
                "{name} configures a feature that is not part of Graphicle Core"
            )),
            !pro,
            "{name}: {stderr}"
        );
    }
}

/// Team sign-in, SCIM, commerce, the deployment licence and provider
/// identities are the commercial edition's: Core answers 404 for their routes, does
/// not know their operations and creates none of their tables. That edition
/// serves them (its own service tests check them in depth).
#[tokio::test]
async fn team_commerce_and_identity_surface_follows_the_edition() {
    let f = Fixture::new();
    let app = f.app();
    let pro = graphicle_runtime::core_capabilities::capabilities()["edition"] == "pro";
    let base = f.base();
    for path in [
        "/pricing".to_string(),
        "/pricing.js".into(),
        "/team.js".into(),
        "/api/commerce/catalog".into(),
        "/api/auth/providers".into(),
        format!("/scim/v2/{}/Users", f.workspace),
        format!("{base}/licence"),
        format!("{base}/billing"),
    ] {
        let status = call(&app, &path, &f.owner, None).await.0;
        assert_eq!(status == StatusCode::NOT_FOUND, !pro, "{path}: {status}");
    }
    for operation in [
        "team.configure",
        "team.status",
        "team.member.save",
        "team.scim.rotate",
        "provider.identity.list",
        "provider.identity.map",
    ] {
        let reply = call(
            &app,
            &f.command_url(),
            &f.owner,
            Some(json!({"operation":operation,"arguments":{}})),
        )
        .await
        .1;
        assert_eq!(
            reply["error"] == "Unsupported native project operation",
            !pro,
            "{operation}: {reply}"
        );
    }
    let db = f.store.connect().unwrap();
    let table = |name: &str| -> bool {
        db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    };
    for name in [
        "am_members",
        "am_sso",
        "am_browser_sessions",
        "am_scim_tokens",
        "am_subscriptions",
        "am_checkout_intents",
        "am_licence",
        "am_commerce_orders",
    ] {
        assert_eq!(table(name), pro, "{name}");
    }
    assert!(table("am_projection_cleanup"), "a core table");
}

#[test]
fn bounded_graph_neighborhood_keeps_its_requested_anchor() {
    let f = Fixture::new();
    let mut source = String::new();
    for i in 0..14 {
        source.push_str(&format!("def caller_{i}():\n    return target()\n\n"));
    }
    source.push_str("def target():\n    return 1\n");
    f.sync(&source);
    let view = f
        .store
        .graph_as_of(
            &f.workspace,
            &f.owner,
            &f.repo,
            &json!({"node":"auth_target","limit":2,"edge_limit":1}),
        )
        .unwrap();
    assert_eq!(view["nodes"].as_array().unwrap().len(), 2);
    assert!(
        view["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == "auth_target"),
        "{view}"
    );
    assert_eq!(view["truncated"], true);
    let edges = f
        .store
        .graph_as_of(
            &f.workspace,
            &f.owner,
            &f.repo,
            &json!({"node":"auth_target","limit":80,"edge_limit":1}),
        )
        .unwrap();
    assert_eq!(edges["edges"].as_array().unwrap().len(), 1);
    assert_eq!(edges["edges_truncated"], true);
}
