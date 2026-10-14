use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
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
        json!(f.store.directory.canonicalize().unwrap()),
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
