//! G13: every read path must enforce workspace ACL after a credential is
//! revoked, a connector ACL is narrowed, or a foreign workspace is used.
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

const SECRET: &str = "Copperfern";
const REPO_SECRET: &str = "Silverbranch";
const A: &str = "def alpha():\n    # Silverbranch is the repository codename\n    return 1\n";
const B: &str = "from a import alpha\n\ndef run():\n    return alpha()\n";

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
    owner: String,
    repo: String,
    connector: String,
}
impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Revocation").unwrap();
        let workspace = s(&ws, "id").to_string();
        let owner = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &owner,
                    &json!({"project_key":"private","name":"Private"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        let f = Self {
            _dir: dir,
            store,
            workspace,
            owner,
            repo,
            connector: String::new(),
        };
        f.sync(&[("a.py", A), ("b.py", B)]);
        let connector = f
            .store
            .connector_add(
                &f.workspace,
                &f.owner,
                &f.args(json!({"name":"Fixture","kind":"jsonl","config":{"acl":{"workspace_readers":true,"credentials":[]}}})),
            )
            .unwrap();
        let connector = s(&connector, "id").to_string();
        f.store
            .import_documents(
                &f.workspace,
                &f.owner,
                &f.args(json!({"connector_id":connector,"mode":"delta","documents":[{"id":"plan","title":"Secret rollout","content":format!("The launch codename is {SECRET}.")}]})),
            )
            .unwrap();
        Self { connector, ..f }
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn sync(&self, files: &[(&str, &str)]) {
        let state = self
            .store
            .project_status(&self.workspace, &self.owner, &self.args(json!({})))
            .unwrap();
        let begin=self.store.sync_prepare(&self.workspace,&self.owner,&self.args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()}))).unwrap();
        self.store.sync_upload(&self.workspace,&self.owner,&self.args(json!({"sync_id":begin["sync_id"],"files":files.iter().map(|(name,text)|json!({"path":name,"content":text})).collect::<Vec<_>>()}))).unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.owner,
                &self.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    fn grant(&self, role: &str) -> (String, String) {
        let credential = self
            .store
            .grant(
                &self.workspace,
                &self.owner,
                &json!({"label":"Ada","role":role}),
            )
            .unwrap();
        (
            s(&credential, "id").to_string(),
            s(&credential, "capability").to_string(),
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
    fn url(&self, suffix: &str) -> String {
        format!(
            "/api/workspaces/{}/repositories/{}/{suffix}",
            self.workspace, self.repo
        )
    }
    /// `graph.impact` on this repository through the operation table, as
    /// HTTP, MCP and `graphicle call` run it: served by an installed extension
    /// (an extension), otherwise 501 after authorization (Core). Either way the caller
    /// is authorized first.
    fn graph_impact(
        &self,
        workspace: &str,
        token: &str,
        mut args: Value,
    ) -> graphicle_runtime::Result<Value> {
        args["repository_id"] = json!(self.repo);
        let app = App::new(self.store.clone(), model::Config::default(), vec![]).unwrap();
        graphicle_runtime::api::command(&app, workspace, token, "graph.impact", &args)
    }
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

/// Every read operation reachable through the command surface, with the
/// arguments needed to make it return data when authorized.
fn read_operations(f: &Fixture) -> Vec<(&'static str, Value)> {
    vec![
        ("status", f.args(json!({}))),
        (
            "context",
            f.args(json!({"question":"What is the launch codename?"})),
        ),
        ("read", f.args(json!({"path":"a.py"}))),
        ("files", f.args(json!({}))),
        ("memories", f.args(json!({}))),
        ("history", f.args(json!({}))),
        ("graph.query", f.args(json!({"query":"alpha"}))),
    ]
}

fn command(f: &Fixture, token: &str, operation: &str, args: &Value) -> Result<Value, StatusCode> {
    let app = App::new(
        f.store.clone(),
        model::Config::default(),
        vec!["localhost".into()],
    )
    .unwrap();
    graphicle_runtime::api::command(&app, &f.workspace, token, operation, args)
        .map_err(|e| e.status)
}

fn leaks(value: &Value) -> bool {
    let text = value.to_string();
    text.contains(SECRET) || text.contains(REPO_SECRET)
}
fn leaks_connector(value: &Value) -> bool {
    value.to_string().contains(SECRET)
}

#[test]
fn reader_sees_data_before_revocation_and_nothing_after() {
    let f = Fixture::new();
    let (credential, reader) = f.grant("reader");
    // Warm every cache a reader could populate.
    for (operation, args) in read_operations(&f) {
        let value = command(&f, &reader, operation, &args)
            .unwrap_or_else(|e| panic!("{operation} failed before revocation: {e}"));
        if operation == "context" {
            assert!(
                leaks(&value),
                "{operation} should show data before revocation: {value}"
            );
        }
        if operation == "read" {
            assert!(value.to_string().contains(REPO_SECRET), "{value}");
        }
    }
    f.store
        .graph_project(&f.workspace, &reader, &f.repo)
        .unwrap();
    let episodes = f
        .store
        .graph_episodes(&f.workspace, &reader, &f.repo)
        .unwrap();
    let episode = s(&episodes, "current_episode").to_string();
    if pro_edition() {
        f.graph_impact(
            &f.workspace,
            &reader,
            json!({"base_episode":episode,"head_episode":episode}),
        )
        .unwrap();
    }

    f.store.revoke(&f.workspace, &f.owner, &credential).unwrap();

    for (operation, args) in read_operations(&f) {
        match command(&f, &reader, operation, &args) {
            Ok(value) => panic!("{operation} leaked after revocation: {value}"),
            Err(status) => assert_eq!(status, StatusCode::FORBIDDEN, "{operation}"),
        }
    }
    assert_eq!(
        f.store
            .graph_project(&f.workspace, &reader, &f.repo)
            .unwrap_err()
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.store
            .graph_episodes(&f.workspace, &reader, &f.repo)
            .unwrap_err()
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.graph_impact(
            &f.workspace,
            &reader,
            json!({"base_episode":episode,"head_episode":episode}),
        )
        .unwrap_err()
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.store.workspace(&f.workspace, &reader).unwrap_err().status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.store.projects(&f.workspace, &reader).unwrap_err().status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn http_and_mcp_surfaces_deny_revoked_tokens() {
    let f = Fixture::new();
    let (credential, reader) = f.grant("reader");
    let app = f.app();
    let (status, value) = call(
        &app,
        "POST",
        &format!("/api/workspaces/{}/repositories/command", f.workspace),
        Some(&reader),
        json!({"operation":"context","arguments":f.args(json!({"question":"codename?"}))}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert!(leaks(&value));
    let (status, _) = call(
        &app,
        "POST",
        &f.url("graph/project"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, episodes) = call(
        &app,
        "GET",
        &f.url("graph/episodes"),
        Some(&reader),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let episode = s(&episodes, "current_episode").to_string();

    f.store.revoke(&f.workspace, &f.owner, &credential).unwrap();

    for (method, path, body) in [
        (
            "POST",
            format!("/api/workspaces/{}/repositories/command", f.workspace),
            json!({"operation":"context","arguments":f.args(json!({"question":"codename?"}))}),
        ),
        (
            "POST",
            format!("/api/workspaces/{}/repositories/command", f.workspace),
            json!({"operation":"read","arguments":f.args(json!({"path":"a.py"}))}),
        ),
        (
            "GET",
            format!("/api/workspaces/{}/repositories", f.workspace),
            Value::Null,
        ),
        (
            "GET",
            format!("/api/workspaces/{}", f.workspace),
            Value::Null,
        ),
        ("POST", f.url("graph/project"), Value::Null),
        ("GET", f.url("graph/episodes"), Value::Null),
        (
            "POST",
            f.url("impact"),
            json!({"base_episode":episode,"head_episode":episode}),
        ),
    ] {
        let (status, value) = call(&app, method, &path, Some(&reader), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {value}");
        assert!(!leaks(&value), "{method} {path} leaked: {value}");
    }
    // MCP tools/call with a revoked token.
    let (status, value) = call(
        &app,
        "POST",
        &format!("/mcp/{}/{}", f.workspace, f.repo),
        Some(&reader),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"context","arguments":{"question":"codename?"}}}),
    )
    .await;
    assert!(
        status == StatusCode::FORBIDDEN
            || status == StatusCode::UNAUTHORIZED
            || value["error"].is_object()
            || value["result"]["isError"] == true,
        "mcp status {status}: {value}"
    );
    assert!(!leaks(&value), "mcp leaked: {value}");
}

#[test]
fn foreign_workspace_credentials_cannot_read_this_repository() {
    let f = Fixture::new();
    let other = f.store.create_workspace("Other").unwrap();
    let other_ws = s(&other, "id").to_string();
    let other_key = s(&other, "capability").to_string();
    // Same repository id through the other workspace: must not resolve.
    for (operation, args) in read_operations(&f) {
        let app = App::new(
            f.store.clone(),
            model::Config::default(),
            vec!["localhost".into()],
        )
        .unwrap();
        match graphicle_runtime::api::command(&app, &other_ws, &other_key, operation, &args) {
            Ok(value) => assert!(
                !leaks(&value),
                "{operation} leaked across workspaces: {value}"
            ),
            Err(e) => assert!(
                e.status == StatusCode::BAD_REQUEST || e.status == StatusCode::FORBIDDEN,
                "{operation}: {}",
                e.message
            ),
        }
    }
    for result in [
        f.store.graph_project(&other_ws, &other_key, &f.repo),
        f.store.graph_episodes(&other_ws, &other_key, &f.repo),
        f.graph_impact(&other_ws, &other_key, json!({"base_episode":"x"})),
    ] {
        let error = result.expect_err("foreign workspace resolved a private repository");
        assert!(!leaks(&json!(error.message)));
    }
    // The owner token of the other workspace against this workspace id.
    for (operation, args) in read_operations(&f) {
        let error =
            command(&f, &other_key, operation, &args).expect_err("foreign owner token accepted");
        assert_eq!(error, StatusCode::FORBIDDEN, "{operation}");
    }
}

#[test]
fn narrowed_connector_acl_hides_documents_from_existing_readers() {
    let f = Fixture::new();
    let (credential, reader) = f.grant("reader");
    let question = f.args(json!({"question":"What is the launch codename?"}));
    let before = command(&f, &reader, "context", &question).unwrap();
    assert!(leaks_connector(&before), "{before}");
    let files = command(&f, &reader, "files", &f.args(json!({}))).unwrap();
    assert!(
        files["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["source_kind"] == "connector_source"),
        "{files}"
    );
    // Restrict the connector to a credential that is not the reader.
    let (other_credential, _) = f.grant("reader");
    f.store
        .connector_sharing(
            &f.workspace,
            &f.owner,
            &f.args(json!({"connector_id":f.connector,"acl":{"workspace_readers":false,"credentials":[other_credential]}})),
        )
        .unwrap();
    let after = command(&f, &reader, "context", &question).unwrap();
    assert!(
        !leaks_connector(&after),
        "connector document leaked after ACL narrowed: {after}"
    );
    let files = command(&f, &reader, "files", &f.args(json!({}))).unwrap();
    assert!(
        !files["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["source_kind"] == "connector_source"),
        "{files}"
    );
    let read = command(
        &f,
        &reader,
        "read",
        &f.args(json!({"source_id":format!("connector:{}:plan",f.connector)})),
    );
    assert!(read.is_err() || !leaks_connector(read.as_ref().unwrap()));
    let _ = credential;
}

/// B6: documents scoped to a revoked credential are found by joining on the
/// identifier, so a credential whose id is a prefix of another's revokes only
/// its own sources; provider-side grants join through verified identities.
#[test]
fn revocation_matches_credential_ids_exactly_not_as_substrings() {
    let f = Fixture::new();
    let db = f.store.connect().unwrap();
    // Two credentials whose ids nest: `cred-a` is a prefix of `cred-ab`.
    for (id, token) in [("cred-a", "token-a"), ("cred-ab", "token-ab")] {
        db.execute(
            "INSERT INTO credentials VALUES(?,?,?,?,?,?,NULL,NULL)",
            rusqlite::params![
                id,
                f.workspace,
                graphicle_memory::text::sha256(token.as_bytes()),
                "reader",
                id,
                graphicle_runtime::now()
            ],
        )
        .unwrap();
    }
    let mut connectors = Vec::new();
    for (name, reader) in [("Only A", "cred-a"), ("Only AB", "cred-ab")] {
        let connector = f
            .store
            .connector_add(
                &f.workspace,
                &f.owner,
                &f.args(json!({"name":name,"kind":"jsonl","config":{"acl":{"workspace_readers":false,"credentials":[reader]}}})),
            )
            .unwrap();
        let connector = s(&connector, "id").to_string();
        f.store
            .import_documents(
                &f.workspace,
                &f.owner,
                &f.args(json!({"connector_id":connector,"mode":"delta","documents":[{"id":"doc","title":name,"content":format!("Visible to {reader} only.")}]})),
            )
            .unwrap();
        connectors.push(connector);
    }
    // A provider-side grant on the shared fixture document names `cred-ab`'s
    // verified identity, not `cred-a`'s.
    let generation: i64 = db
        .query_row(
            "SELECT generation FROM am_connector_docs WHERE connector_id=? AND id='plan'",
            [&f.connector],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "INSERT INTO am_source_permissions VALUES(?,?,?,?,1,?)",
        rusqlite::params![
            f.connector,
            "plan",
            json!([{"type":"user","id":"U-AB"}]).to_string(),
            graphicle_runtime::now(),
            generation
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO am_provider_identities VALUES(?,?,?,?,?)",
        rusqlite::params![
            f.workspace,
            "jsonl",
            "U-AB",
            "cred-ab",
            graphicle_runtime::now()
        ],
    )
    .unwrap();

    let expected = |connector: &str, doc: &str| format!("connector:{connector}:{doc}");
    assert_eq!(
        graphicle_runtime::store::credential_sources(&db, &f.workspace, "cred-a").unwrap(),
        vec![expected(&connectors[0], "doc")]
    );
    assert_eq!(
        graphicle_runtime::store::credential_sources(&db, &f.workspace, "cred-ab").unwrap(),
        {
            let mut both = vec![
                expected(&connectors[1], "doc"),
                expected(&f.connector, "plan"),
            ];
            both.sort();
            both
        }
    );
    drop(db);

    let revoked = f.store.revoke(&f.workspace, &f.owner, "cred-a").unwrap();
    assert_eq!(revoked["revoked"], true);
    assert_eq!(revoked["sources"], json!([expected(&connectors[0], "doc")]));
    let db = f.store.connect().unwrap();
    let still_active: i64 = db
        .query_row(
            "SELECT count(*) FROM credentials WHERE workspace_id=? AND id='cred-ab' AND revoked_at IS NULL",
            [&f.workspace],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(still_active, 1);
    // The other credential's document remains readable to it.
    let visible = command(
        &f,
        "token-ab",
        "read",
        &f.args(json!({"source_id":expected(&connectors[1], "doc")})),
    )
    .unwrap();
    assert!(visible.to_string().contains("cred-ab only"), "{visible}");
    assert_eq!(
        command(
            &f,
            "token-a",
            "read",
            &f.args(json!({"source_id":expected(&connectors[0], "doc")})),
        )
        .unwrap_err(),
        StatusCode::FORBIDDEN
    );
}
