//! Every extension hook at the place the runtime calls it. One probe
//! extension implementing all hooks is installed for this whole test binary
//! (the registry is process-global), so each test installs it first.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode, header},
    routing::{get, post},
};
use graphicle_runtime::{
    Error, Result,
    api::{self, App},
    connectors::Remote,
    extension::{
        self, Access, AccessRequest, BoxFuture, CliContext, Edition, Extension, Extensions,
        JobKind, McpTool, ServeContext, Source,
    },
    jobs::{Job, Worker},
    metering::Served,
    model,
    store::{Principal, Store, one, s},
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};
use std::{
    ffi::OsString,
    sync::{Arc, Mutex, Once},
    time::Duration,
};
use tower::ServiceExt;

const SESSION_TOKEN: &str = "probe-session-token";
/// The probe's job kind that follows projections.
const FOLLOW: &str = "probe_follow";

static INSTALL: Once = Once::new();
fn install() {
    INSTALL.call_once(|| extension::install(Extensions::none().with(Probe)));
}

static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn record(event: String) {
    EVENTS.lock().unwrap().push(event);
}
fn recorded(event: &str) -> bool {
    EVENTS.lock().unwrap().iter().any(|e| e == event)
}
async fn until_recorded(event: &str) {
    while !recorded(event) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Probe;
impl Extension for Probe {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn edition(&self) -> Option<Edition> {
        Some(Edition::Pro)
    }
    fn capabilities(&self, capabilities: &mut Map<String, Value>) {
        capabilities.insert("probe".into(), json!(true));
        // It fetches provider permissions (`source_permissions` below).
        capabilities.insert("provider_permission_sync".into(), json!(true));
        // Ignored: the edition is the declared one.
        capabilities.insert("edition".into(), json!("probe"));
    }
    fn migrate(&self, db: &Connection) -> Result<()> {
        db.execute_batch("CREATE TABLE IF NOT EXISTS probe_rows (id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE);")?;
        Ok(())
    }
    fn operations(&self) -> &'static [&'static str] {
        &["probe.echo", "probe.async", "impact.report"]
    }
    fn command(
        &self,
        _app: &App,
        workspace: &str,
        _capability: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        match operation {
            "probe.echo" | "impact.report" => {
                Ok(json!({"probe":operation,"workspace":workspace,"args":args}))
            }
            _ => Err(Error::bad("Unsupported native project operation")),
        }
    }
    fn command_async<'a>(
        &'a self,
        app: &'a App,
        workspace: &'a str,
        capability: &'a str,
        operation: &'a str,
        _args: &'a Value,
    ) -> Option<BoxFuture<'a, Result<Value>>> {
        if operation != "probe.async" {
            return None;
        }
        Some(Box::pin(async move {
            app.worker.store.workspace(workspace, capability)?;
            Ok(json!({"probe":"async"}))
        }))
    }
    fn routes(&self) -> Router<App> {
        Router::new()
            .route(
                "/probe",
                get(|State(app): State<App>| async move {
                    Json(json!({"probe":true,"hosted":app.public_origin.is_some()}))
                }),
            )
            .route("/probe/form", post(session_seen))
            .route("/probe/other", post(session_seen))
    }
    fn cross_site_post(&self, path: &str) -> bool {
        path == "/probe/form"
    }
    fn mcp_tools(&self, _learning: bool, _work_state: bool) -> Vec<McpTool> {
        vec![McpTool::new(
            api::tool(
                "probe_tool",
                "Echo the arguments.",
                json!({"repo":{"type":"string"},"word":{"type":"string"}}),
                &[],
                false,
            ),
            "probe.echo",
            true,
        )]
    }
    fn native_operations(&self) -> &'static [&'static str] {
        &["graph.probe_native"]
    }
    fn native(&self, operation: &str, request: &mut Value) -> Option<Result<Value>> {
        // It would answer anything; only its declared operation reaches it.
        Some(Ok(
            json!({"native":std::mem::take(request),"operation":operation}),
        ))
    }
    fn session_credential(
        &self,
        db: &Connection,
        workspace: &str,
        hash: &str,
    ) -> Result<Option<Value>> {
        if hash != graphicle_memory::text::sha256(SESSION_TOKEN.as_bytes()) {
            return Ok(None);
        }
        one(
            db,
            "SELECT id,role,expires_at,revoked_at,capability_hash FROM credentials WHERE workspace_id=? AND label='session'",
            [workspace],
        )
    }
    fn credential_allowed(&self, db: &Connection, credential: &str) -> Result<bool> {
        let label: Option<String> = db
            .query_row(
                "SELECT label FROM credentials WHERE id=?",
                [credential],
                |r| r.get(0),
            )
            .optional()?;
        Ok(label.as_deref() != Some("blocked"))
    }
    fn role_permits(
        &self,
        _db: &Connection,
        _workspace: &str,
        role: &str,
        permission: &str,
    ) -> Result<Option<bool>> {
        Ok(match role {
            "probe-role" => Some(["read", "propose"].contains(&permission)),
            // An attempt to widen a Core role: the core never asks.
            "reader" => Some(true),
            _ => None,
        })
    }
    /// Credentials labelled `guarded…` are recorded and restricted: no
    /// `collect`, no repository whose key is `hidden`, no episodes route, no
    /// `graph.stats`, `graph.navigate` or `graph.pull_requests`, no
    /// `repository_status` tool; `probe.echo` runs with
    /// narrowed arguments, and responses of `guarded-capped` are limited.
    /// Credentials labelled `metered` get a per-call cap of 5 tokens for
    /// `probe.echo` and `work_state.write`, and a large one otherwise.
    fn authorize(
        &self,
        db: &Connection,
        principal: &Principal,
        request: &mut AccessRequest<'_>,
    ) -> Result<()> {
        if principal.role == "owner" {
            record("access:owner".into());
        }
        let label: Option<String> = db
            .query_row(
                "SELECT label FROM credentials WHERE id=?",
                [&principal.credential],
                |r| r.get(0),
            )
            .optional()?;
        if label.as_deref() == Some("metered") {
            if let Access::Operation(operation) = request.access {
                request.max_tokens = Some(match operation {
                    "probe.echo" | "work_state.write" => 5,
                    _ => 1_000_000,
                });
            }
            return Ok(());
        }
        let Some(label) = label.filter(|label| label.starts_with("guarded")) else {
            return Ok(());
        };
        record(format!("access:{label}:{:?}", request.access));
        let refuse = |what: &str| -> Result<()> {
            Err(Error {
                status: StatusCode::FORBIDDEN,
                message: format!("probe refuses {what}"),
                provider_failure: None,
            })
        };
        match request.access {
            Access::Permission("collect") => return refuse("collect"),
            Access::Repository(repository) => {
                let key: Option<String> = db
                    .query_row(
                        "SELECT project_key FROM repo_projects WHERE id=?",
                        [repository],
                        |r| r.get(0),
                    )
                    .optional()?;
                if key.as_deref() == Some("hidden") {
                    return refuse("the hidden repository");
                }
            }
            Access::Route { pattern, .. } if pattern.ends_with("/graph/episodes") => {
                return refuse("the episodes route");
            }
            Access::Operation("graph.stats") => return refuse("graph.stats"),
            Access::Operation(operation @ ("graph.navigate" | "graph.pull_requests")) => {
                return refuse(operation);
            }
            Access::Operation("probe.echo") => {
                if let Some(arguments) = request.arguments.as_mut() {
                    arguments.to_mut()["narrowed"] = json!(true);
                }
            }
            Access::Tool {
                name: "repository_status",
                ..
            } => return refuse("repository_status"),
            _ => {}
        }
        if label == "guarded-capped" {
            request.max_tokens = Some(120);
        }
        Ok(())
    }
    fn credential_revoked(&self, db: &Connection, workspace: &str, credential: &str) -> Result<()> {
        // Inside the revoking transaction: the credential is already revoked.
        let revoked: bool = db.query_row(
            "SELECT revoked_at IS NOT NULL FROM credentials WHERE id=? AND workspace_id=?",
            [credential, workspace],
            |r| r.get(0),
        )?;
        record(format!("revoked:{credential}:{revoked}"));
        Ok(())
    }
    fn request_allowance(&self, _db: &Connection, _workspace: &str) -> Result<Option<u64>> {
        Ok(Some(1000))
    }
    fn usage_details(&self, _db: &Connection, _workspace: &str, usage: &mut Value) -> Result<()> {
        usage["probe"] = json!("usage");
        Ok(())
    }
    fn served_allowed(
        &self,
        db: &Connection,
        principal: &Principal,
        served: &Served<'_>,
    ) -> Result<()> {
        // Only for credentials labelled "metered", so other tests are untouched.
        let label: Option<String> = db
            .query_row(
                "SELECT label FROM credentials WHERE id=?",
                [&principal.credential],
                |r| r.get(0),
            )
            .optional()?;
        if label.as_deref() != Some("metered") {
            return Ok(());
        }
        record(format!(
            "served:{}:{}:{}:{}:{}",
            principal.credential,
            served.channel.as_str(),
            served.kind,
            served.session,
            served.repository
        ));
        match served.kind {
            "repository_files" | "files" | "graph.cross_edges" => Err(Error {
                status: StatusCode::TOO_MANY_REQUESTS,
                message: format!("probe refuses {}", served.kind),
                provider_failure: None,
            }),
            _ => Ok(()),
        }
    }
    fn metered_operations(&self) -> &'static [&'static str] {
        &["probe.async"]
    }
    fn scrub_backup(&self, db: &Connection) -> Result<()> {
        db.execute("DELETE FROM probe_rows", [])?;
        Ok(())
    }
    fn vault_key(&self, store: &Store) -> Option<Result<[u8; 32]>> {
        // Only the stores a test marks keep their vault key in the probe.
        if store.directory.join("probe-vault-key").exists() {
            Some(Ok([7; 32]))
        } else if store.directory.join("probe-vault-down").exists() {
            Some(Err(Error::bad("Probe key service unavailable")))
        } else {
            None
        }
    }
    fn job_kinds(&self) -> &'static [JobKind] {
        const KINDS: &[JobKind] = &[
            JobKind::new("probe", "read", true),
            JobKind::new(FOLLOW, "read", false).following_projection(),
        ];
        KINDS
    }
    fn job<'a>(
        &'a self,
        _worker: &'a Worker,
        job: &'a Job,
    ) -> Option<BoxFuture<'a, Result<Value>>> {
        match job.kind.as_str() {
            "probe" => Some(Box::pin(async move { Ok(json!({"probe_job":job.id})) })),
            FOLLOW => Some(Box::pin(async move {
                Ok(json!({"followed":job.payload["episode"]}))
            })),
            _ => None,
        }
    }
    /// Queues a [`FOLLOW`] job after each projection of a project named
    /// "Probe follows"; for one named "Probe fails" it writes a row and then
    /// fails, which must roll the row back and leave the projection alone.
    fn after_projection(
        &self,
        db: &Connection,
        principal: &graphicle_runtime::store::Principal,
        repository: &str,
        episode: &str,
    ) -> Result<Vec<Value>> {
        let name: String = db.query_row(
            "SELECT name FROM repo_projects WHERE id=?",
            [repository],
            |r| r.get(0),
        )?;
        if !name.starts_with("Probe f") {
            return Ok(Vec::new());
        }
        let stamp = graphicle_runtime::now();
        let id = format!("{FOLLOW}-{episode}");
        db.execute(
            "INSERT OR IGNORE INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,priority,max_attempts,not_before,created_at,updated_at) VALUES(?,?,?,?,?,?,'queued',-1,3,?,?,?)",
            rusqlite::params![id, repository, FOLLOW, id, json!({"episode":episode}).to_string(), principal.hash, stamp, stamp, stamp],
        )?;
        if name == "Probe fails" {
            return Err(Error::bad("probe hook failed"));
        }
        Ok(vec![
            json!({"id":id,"kind":FOLLOW,"state":"queued","replayed":false}),
        ])
    }
    fn schedule(&self, _store: &Store) -> Result<usize> {
        Ok(2)
    }
    fn source_permissions<'a>(
        &'a self,
        remote: &'a mut Remote<'_>,
        _item: &'a Value,
        doc: &'a Value,
    ) -> Option<BoxFuture<'a, Result<Value>>> {
        Some(Box::pin(async move {
            let mut permissions = json!({"complete":true,"principals":[{"type":"user","id":"7"}]});
            permissions["checked_at"] = json!(graphicle_runtime::now());
            permissions["probe_doc"] = doc["id"].clone();
            permissions["connector_kind"] = remote.connector["kind"].clone();
            Ok(permissions)
        }))
    }
    fn sources(&self) -> &'static [&'static dyn Source] {
        PROBE_SOURCES
    }
    fn serve(&self, context: &mut ServeContext<'_>) -> Result<()> {
        let store = context.store.clone();
        context.background.push(Box::pin(async move {
            record(format!("background:{}", store.directory.display()));
        }));
        record(format!(
            "serve:{}",
            context.public_origin.unwrap_or("loopback")
        ));
        Ok(())
    }
    fn cli(&self, command: clap::Command) -> clap::Command {
        command.subcommand(
            clap::Command::new("probe")
                .about("Probe command")
                .arg(clap::Arg::new("word").long("word").required(true)),
        )
    }
    fn run_cli<'a>(
        &'a self,
        matches: &'a clap::ArgMatches,
        context: &'a CliContext,
    ) -> Option<BoxFuture<'a, Result<()>>> {
        let ("probe", probe) = matches.subcommand()? else {
            return None;
        };
        Some(Box::pin(async move {
            let store = context.store()?;
            record(format!(
                "cli:{}:{}",
                probe.get_one::<String>("word").unwrap(),
                store.directory.display()
            ));
            Ok(())
        }))
    }
    fn ui_script(&self) -> &'static str {
        "window.graphicleProbe = true;"
    }
    fn page(&self, path: &str) -> Option<&'static str> {
        (path == "/product").then_some("<!doctype html><title>Probe product</title>")
    }
    fn history_tables(&self) -> &'static [&'static str] {
        &["probe_rows"]
    }
    fn retained_tables(&self) -> &'static [&'static str] {
        &["probe_rows"]
    }
}

/// Whether the boundary mapped a browser session into the request.
async fn session_seen(headers: HeaderMap) -> Json<Value> {
    Json(json!({"session":headers.contains_key("x-graphicle-browser-session")}))
}

/// A connector kind the probe extension adds: two listing pages of one
/// document each, read from `docs/{id}`.
struct ProbeDocs;
static PROBE_SOURCES: &[&dyn Source] = &[&ProbeDocs];
impl Source for ProbeDocs {
    fn kind(&self) -> &'static str {
        "probe_docs"
    }
    fn oauth_provider(&self) -> &'static str {
        "probe_accounts"
    }
    fn oauth_identity(&self, identity: &Value) -> Option<(String, String)> {
        Some((identity["sub"].as_str()?.to_string(), String::new()))
    }
    fn fields(&self) -> &'static [&'static str] {
        &["topic"]
    }
    fn validate(&self, config: &Value) -> Result<()> {
        graphicle_runtime::string(config, "topic", 50).map(|_| ())
    }
    fn base_url(&self, _config: &Value) -> Result<String> {
        Ok("https://probe-docs.invalid".into())
    }
    fn listing<'a>(
        &'a self,
        _remote: &'a mut Remote<'_>,
        cursor: &'a Value,
    ) -> BoxFuture<'a, Result<(Vec<Value>, Value)>> {
        Box::pin(async move {
            Ok(if cursor.is_null() {
                (vec![json!({"id":"d1"})], json!({"page":2}))
            } else {
                (vec![json!({"id":"d2"})], Value::Null)
            })
        })
    }
    fn document<'a>(
        &'a self,
        remote: &'a mut Remote<'_>,
        item: &'a Value,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            let id = s(item, "id").to_string();
            let doc = remote.get(&format!("docs/{id}"), vec![]).await?;
            Ok(Some(
                json!({"id":id,"title":doc["title"],"content":doc["text"],
                "url":null,"source_timestamp":null,"metadata":{}}),
            ))
        })
    }
    fn probe<'a>(&'a self, remote: &'a mut Remote<'_>) -> BoxFuture<'a, Result<(Value, Value)>> {
        Box::pin(async move {
            let who = remote.get("whoami", vec![]).await?;
            Ok((who, json!({"topic":remote.connector["config"]["topic"]})))
        })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    workspace: String,
    owner: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        install();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let created = store.create_workspace("Extensions").unwrap();
        let workspace = s(&created, "id").to_string();
        let owner = s(&created, "capability").to_string();
        let project = store
            .register(
                &workspace,
                &owner,
                &json!({"project_key":"probe","name":"Probe"}),
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
    fn app(&self) -> App {
        App::new(
            self.store.clone(),
            model::Config::default(),
            vec!["localhost".into()],
        )
        .unwrap()
    }
    fn grant(&self, label: &str) -> Value {
        self.store
            .grant(
                &self.workspace,
                &self.owner,
                &json!({"label":label,"role":"reader"}),
            )
            .unwrap()
    }
}

#[test]
fn install_happens_once_per_process() {
    install();
    assert_eq!(extension::installed().names(), ["probe"]);
    assert!(std::panic::catch_unwind(|| extension::install(Extensions::none())).is_err());
    assert_eq!(extension::installed().names(), ["probe"]);
}

#[test]
fn build_info_reports_the_declared_edition_and_extension_capabilities() {
    let f = Fixture::new();
    let info = api::command(&f.app(), &f.workspace, &f.owner, "build.info", &json!({})).unwrap();
    // The probe declares the "pro" edition; its capabilities hook cannot rename it.
    assert_eq!(info["edition"], "pro");
    assert_eq!(graphicle_runtime::core_capabilities::edition(), "pro");
    assert_eq!(info["probe"], true);
    assert_eq!(info["provider_permission_sync"], true);
}

#[tokio::test]
async fn an_extension_may_replace_the_product_page() {
    let f = Fixture::new();
    let reply = api::router(f.app())
        .oneshot(
            Request::get("/product")
                .header(header::HOST, "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    let body = to_bytes(reply.into_body(), 4096).await.unwrap();
    assert_eq!(&body[..], b"<!doctype html><title>Probe product</title>");
}

#[test]
fn deletion_honours_the_extension_history_and_retained_tables() {
    let f = Fixture::new();
    f.store
        .connect()
        .unwrap()
        .execute("INSERT INTO probe_rows VALUES('kept',?)", [&f.workspace])
        .unwrap();
    let args = json!({"repository_id":f.repo});
    let state = api::command(&f.app(), &f.workspace, &f.owner, "deletion.state", &args).unwrap();
    assert_eq!(state["contract"], "graphicle.state.v2");
    assert_eq!(state["excluded"]["history"][0], "probe_rows", "{state}");
    assert!(
        state["tables"].get("probe_rows").is_none(),
        "history is excluded"
    );
    let snapshot = f
        .store
        .project_status(&f.workspace, &f.owner, &args)
        .unwrap()["repository"]["snapshot"]
        .clone();
    let deleted = f
        .store
        .maintain(
            &f.workspace,
            &f.owner,
            "project.delete",
            &json!({"repository_id":f.repo,"expected_snapshot":snapshot}),
        )
        .unwrap();
    assert_eq!(
        deleted["receipt"]["residue"]["retained_by_design"][0], "probe_rows",
        "{deleted}"
    );
}

#[test]
fn store_open_runs_extension_migrations_after_the_core_schema() {
    let f = Fixture::new();
    let tables: i64 = f
        .store
        .connect()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('probe_rows','workspaces')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 2);
    // Reopening is idempotent.
    Store::open(f.store.directory.as_ref()).unwrap();
}

#[tokio::test]
async fn operations_go_to_the_extension_before_the_core() {
    let f = Fixture::new();
    let app = f.app();
    let args = json!({"repository_id":f.repo});
    let echo = api::command(&app, &f.workspace, &f.owner, "probe.echo", &args).unwrap();
    assert_eq!(echo["probe"], "probe.echo");
    assert_eq!(echo["args"]["repository_id"], json!(f.repo));
    // An overridable core operation answers through the extension.
    let report = api::command(&app, &f.workspace, &f.owner, "impact.report", &args).unwrap();
    assert_eq!(report["probe"], "impact.report");
    // An async-only operation is unsupported on the synchronous path, as the
    // core's own async operations are.
    let error = api::command(&app, &f.workspace, &f.owner, "probe.async", &args).unwrap_err();
    assert_eq!(error.message, "Unsupported native project operation");
    let reply = api::command_async(&app, &f.workspace, &f.owner, "probe.async", &args)
        .await
        .unwrap();
    assert_eq!(reply["probe"], "async");
    assert!(
        api::command_async(&app, &f.workspace, "not-a-key", "probe.async", &args)
            .await
            .is_err()
    );
    // Without an async form, the synchronous one runs on a blocking thread.
    let echo = api::command_async(&app, &f.workspace, &f.owner, "probe.echo", &args)
        .await
        .unwrap();
    assert_eq!(echo["probe"], "probe.echo");
    // Arguments are still checked first, and unknown names stay unknown.
    assert!(api::command(&app, &f.workspace, &f.owner, "probe.echo", &json!([])).is_err());
    let unknown = api::command(&app, &f.workspace, &f.owner, "probe.none", &args).unwrap_err();
    assert_eq!(unknown.message, "Unsupported native project operation");
}

#[tokio::test]
async fn extension_routes_sit_inside_the_host_boundary() {
    let f = Fixture::new();
    let get = |path: &str, host: &str| {
        Request::get(path)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    };
    let router = api::router(f.app());
    let reply = router
        .clone()
        .oneshot(get("/probe", "localhost"))
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(
        reply.headers()[header::CACHE_CONTROL],
        "no-store",
        "boundary headers apply"
    );
    let body = to_bytes(reply.into_body(), 4096).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"probe":true,"hosted":false})
    );
    let foreign = router
        .clone()
        .oneshot(get("/probe", "attacker.example"))
        .await
        .unwrap();
    assert_eq!(foreign.status(), StatusCode::FORBIDDEN);
    let script = router
        .oneshot(get("/extensions.js", "localhost"))
        .await
        .unwrap();
    assert_eq!(script.status(), StatusCode::OK);
    assert!(
        script.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/javascript")
    );
    let body = to_bytes(script.into_body(), 4096).await.unwrap();
    assert_eq!(&body[..], b"window.graphicleProbe = true;");
}

#[tokio::test]
async fn a_declared_cross_site_form_post_skips_only_the_origin_checks() {
    let f = Fixture::new();
    let router = api::router(f.app());
    let post = |path: &str, host: &str, origin: Option<&str>, cookie: bool| {
        let mut request = Request::post(path).header(header::HOST, host);
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if cookie {
            request = request.header(header::COOKIE, format!("graphicle_session={SESSION_TOKEN}"));
        }
        request.body(Body::empty()).unwrap()
    };
    let seen = |reply: axum::response::Response| async move {
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(
            reply.headers()[header::CACHE_CONTROL],
            "no-store",
            "boundary headers apply"
        );
        let body = to_bytes(reply.into_body(), 4096).await.unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()["session"].clone()
    };
    // The declared path takes a post from another origin, even with a
    // session cookie and no request header, and is given no session.
    let reply = router
        .clone()
        .oneshot(post(
            "/probe/form",
            "localhost",
            Some("https://idp.example"),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(seen(reply).await, false);
    // Its host check stays.
    let foreign_host = router
        .clone()
        .oneshot(post(
            "/probe/form",
            "attacker.example",
            Some("https://idp.example"),
            false,
        ))
        .await
        .unwrap();
    assert_eq!(foreign_host.status(), StatusCode::FORBIDDEN);
    // A GET to the declared path, and a post to any other path, keep the
    // origin and browser-mutation checks.
    let get = Request::get("/probe/form")
        .header(header::HOST, "localhost")
        .header(header::ORIGIN, "https://idp.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.clone().oneshot(get).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    for (origin, cookie) in [(Some("https://idp.example"), false), (None, true)] {
        let reply = router
            .clone()
            .oneshot(post("/probe/other", "localhost", origin, cookie))
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::FORBIDDEN, "{origin:?} {cookie}");
    }
    let same_origin = router
        .clone()
        .oneshot(post(
            "/probe/other",
            "localhost",
            Some("http://localhost"),
            false,
        ))
        .await
        .unwrap();
    assert_eq!(seen(same_origin).await, false);
    let mut browser = post("/probe/other", "localhost", None, true);
    browser
        .headers_mut()
        .insert("x-graphicle-request", "1".parse().unwrap());
    assert_eq!(seen(router.oneshot(browser).await.unwrap()).await, true);
}

#[test]
fn mcp_tools_follow_the_core_tools_and_run_their_operation() {
    let f = Fixture::new();
    for (learning, work_state) in [(false, false), (true, true)] {
        let tools = api::mcp_tools_with(learning, work_state);
        let names: Vec<&str> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        // A project connection's list: the core tools as it publishes them
        // (`list_projects` only on workspace connections).
        let core: Vec<McpTool> = api::core_mcp_tools(learning, work_state)
            .into_iter()
            .filter_map(McpTool::confined)
            .collect();
        assert_eq!(names.len(), core.len() + 1);
        assert_eq!(names.last(), Some(&"probe_tool"));
    }
    let other = f
        .store
        .register(
            &f.workspace,
            &f.owner,
            &json!({"project_key":"other","name":"Other"}),
        )
        .unwrap();
    let call = |arguments: Value| {
        api::mcp(
            &f.app(),
            &f.workspace,
            &f.owner,
            &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"probe_tool","arguments":arguments}}),
        )
        .unwrap()
    };
    let reply = call(json!({"word":"hello"}));
    let result = &reply["result"]["structuredContent"];
    assert_eq!(result["probe"], "probe.echo", "{reply}");
    assert_eq!(result["args"]["repository_id"], json!(f.repo));
    assert_eq!(result["args"]["word"], "hello");
    // `repo_arg`: the tool may name another repository of the workspace.
    let reply = call(json!({"repo":other["id"]}));
    assert_eq!(
        reply["result"]["structuredContent"]["args"]["repository_id"],
        other["id"]
    );
    // The published contract is enforced.
    let reply = call(json!({"unknown":true}));
    assert!(
        s(&reply["error"], "message").contains("published contract"),
        "{reply}"
    );
}

#[test]
fn declared_native_operations_reach_the_extension_first() {
    install();
    assert_eq!(
        graphicle_runtime::native("graph.probe_native", json!({"x":1})).unwrap(),
        json!({"native":{"x":1},"operation":"graph.probe_native"})
    );
    // Undeclared operations, including other graph ones, reach the memory
    // engine, never the extension.
    assert!(graphicle_runtime::native("info", json!({})).unwrap()["abi"].is_number());
    assert!(graphicle_runtime::native("graph.probe_other", json!({})).is_err());
}

#[test]
fn credential_hooks_take_part_in_every_authorization() {
    let f = Fixture::new();
    // An extension session token maps to a workspace credential.
    assert!(f.store.workspace(&f.workspace, SESSION_TOKEN).is_err());
    let session = f.grant("session");
    let row = f.store.workspace(&f.workspace, SESSION_TOKEN).unwrap();
    assert_eq!(row["credential_role"], "reader");
    // A credential the extension disallows is denied; others are not.
    let blocked = f.grant("blocked");
    let error = f
        .store
        .workspace(&f.workspace, s(&blocked, "capability"))
        .unwrap_err();
    assert_eq!(error.status, StatusCode::FORBIDDEN);
    let reader = f.grant("reader");
    f.store
        .workspace(&f.workspace, s(&reader, "capability"))
        .unwrap();
    // Revocation is reported inside the revoking transaction.
    f.store
        .revoke(&f.workspace, &f.owner, s(&session, "id"))
        .unwrap();
    assert!(recorded(&format!("revoked:{}:true", s(&session, "id"))));
    assert!(f.store.workspace(&f.workspace, SESSION_TOKEN).is_err());
}

#[test]
fn usage_reports_the_extension_allowance_and_details() {
    let f = Fixture::new();
    let usage = f.store.product_usage(&f.workspace, &f.owner).unwrap();
    assert_eq!(usage["plan_allowance"], 1000);
    assert_eq!(usage["effective_limit"], 1000);
    assert_eq!(usage["probe"], "usage");
    let budget = f
        .store
        .product_budget(&f.workspace, &f.owner, &json!({"monthly_requests":10}))
        .unwrap();
    assert_eq!(budget["effective_limit"], 10, "the lowest limit applies");
}

/// `served_allowed` is asked before every metered call of a credential other
/// than the owner capability, with the call's channel, tool or operation,
/// session and project. Its refusal reaches the caller instead of the result
/// (a 429 with `Retry-After` over HTTP), and a read's result larger than its
/// per-call cap is refused, not served or counted; a write's result is served.
/// Over HTTP every operation a published tool runs is metered, and so are
/// the other text reads (Core's and the extension's `metered_operations`),
/// through the command routes and the graph routes alike.
#[tokio::test]
async fn metered_calls_are_admitted_and_capped_by_the_extension() {
    let f = Fixture::new();
    let key = f.grant("metered");
    let (token, id) = (s(&key, "capability").to_string(), s(&key, "id").to_string());
    let agent = f
        .store
        .grant(
            &f.workspace,
            &f.owner,
            &json!({"label":"metered","role":"agent"}),
        )
        .unwrap();
    let (agent_token, agent_id) = (
        s(&agent, "capability").to_string(),
        s(&agent, "id").to_string(),
    );
    let mut app = f.app();
    app.mcp_work_state = true;
    let tool = |token: &str, name: &str, arguments: Value| {
        let reply = api::mcp(
            &app,
            &f.workspace,
            token,
            &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )
        .unwrap();
        (
            reply["result"]["isError"] == true,
            s(&reply["result"]["content"][0], "text").to_string(),
        )
    };
    let (failed, text) = tool(&token, "repository_status", json!({}));
    assert!(!failed, "{text}");
    assert!(recorded(&format!(
        "served:{id}:mcp:repository_status::{}",
        f.repo
    )));
    let (failed, text) = tool(&token, "repository_files", json!({}));
    assert!(failed);
    assert_eq!(text, "probe refuses repository_files");
    let (failed, text) = tool(&token, "probe_tool", json!({"word":"hello"}));
    assert!(failed);
    assert!(
        text.contains("this credential may receive at most 5 per call"),
        "{text}"
    );
    // A write tool's result is served whatever the cap: it is saved by then.
    let (failed, text) = tool(
        &agent_token,
        "work_state_write",
        json!({"list":"release","fields":{"status":"doing"}}),
    );
    assert!(!failed, "{text}");
    assert!(graphicle_memory::text::count(&text) > 5, "{text}");
    assert!(recorded(&format!(
        "served:{agent_id}:mcp:work_state_write::{}",
        f.repo
    )));
    // The owner capability is never asked.
    let (failed, text) = tool(&f.owner, "repository_files", json!({}));
    assert!(!failed, "{text}");
    // Over HTTP, metered operations are asked with their operation and
    // channel; the extension's refusal keeps its status.
    let router = api::router(app.clone());
    let send = |request: Request<Body>| {
        let router = router.clone();
        async move { router.oneshot(request).await.unwrap() }
    };
    let post = |token: &str, body: Value| {
        Request::post(format!(
            "/api/workspaces/{}/repositories/command",
            f.workspace
        ))
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-workspace-capability", token)
        .body(Body::from(body.to_string()))
        .unwrap()
    };
    let get = |path: &str| {
        Request::get(format!(
            "/api/workspaces/{}/repositories/{}/{path}",
            f.workspace, f.repo
        ))
        .header(header::HOST, "localhost")
        .header("x-workspace-capability", token.as_str())
        .body(Body::empty())
        .unwrap()
    };
    // A read a tool runs, a read no tool runs, and the extension's own
    // metered read are asked.
    for operation in ["memory.search", "memories", "memory.export", "probe.async"] {
        let reply = send(post(
            &token,
            json!({"operation":operation,"arguments":{"repository_id":f.repo,"question":"probe"}}),
        ))
        .await;
        assert_eq!(reply.status(), StatusCode::OK, "{operation}");
        assert!(
            recorded(&format!("served:{id}:http:{operation}::{}", f.repo)),
            "{operation}"
        );
    }
    // An operation that returns no project text is not metered, nor asked.
    let reply = send(post(
        &token,
        json!({"operation":"observer.status","arguments":{"repository_id":f.repo,"include":["file:"]}}),
    ))
    .await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert!(!recorded(&format!(
        "served:{id}:http:observer.status::{}",
        f.repo
    )));
    // A refusal with 429 says when to retry: the next UTC month.
    let reply = send(post(
        &token,
        json!({"operation":"files","arguments":{"repository_id":f.repo}}),
    ))
    .await;
    assert_eq!(reply.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: i64 = reply.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=31 * 86_400).contains(&retry), "{retry}");
    // A write the agent's tool runs is served over HTTP whatever the cap.
    let reply = send(post(
        &agent_token,
        json!({"operation":"work_state.write","arguments":{"repository_id":f.repo,"list":"release","fields":{"owner":"ada"}}}),
    ))
    .await;
    assert_eq!(reply.status(), StatusCode::OK);
    let body = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
    assert!(graphicle_memory::text::count(&String::from_utf8_lossy(&body)) > 5);
    // The graph routes are metered as the operations they serve.
    let reply = send(get("graph/stats")).await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert!(recorded(&format!(
        "served:{id}:http:graph.stats::{}",
        f.repo
    )));
    let reply = send(get("graph/cross_edges")).await;
    assert_eq!(reply.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(reply.headers().contains_key("retry-after"));
    // Only what was served is counted (recorded first: a usage report does
    // not wait for results still being counted).
    graphicle_runtime::metering::flush(&f.store).unwrap();
    let usage = f.store.product_usage(&f.workspace, &f.owner).unwrap();
    let mut kinds: Vec<(String, String, u64)> = usage["served"]["by_kind"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                s(row, "channel").to_string(),
                s(row, "kind").to_string(),
                row["calls"].as_u64().unwrap(),
            )
        })
        .collect();
    kinds.sort();
    let expected: Vec<(String, String, u64)> = [
        ("http", "graph.stats"),
        ("http", "memories"),
        ("http", "memory.export"),
        ("http", "memory.search"),
        ("http", "probe.async"),
        ("http", "work_state.write"),
        ("mcp", "repository_files"),
        ("mcp", "repository_status"),
        ("mcp", "work_state_write"),
    ]
    .into_iter()
    .map(|(channel, kind)| (channel.to_string(), kind.to_string(), 1))
    .collect();
    assert_eq!(
        kinds, expected,
        "the owner's repository_files call and nothing refused"
    );
    assert_eq!(usage["cost"], Value::Null);
}

#[test]
fn backups_are_scrubbed_by_the_extension_too() {
    let f = Fixture::new();
    f.store
        .connect()
        .unwrap()
        .execute("INSERT INTO probe_rows VALUES('row',?)", [&f.workspace])
        .unwrap();
    let file = f.store.directory.join("backup.sqlite3");
    f.store.backup(&file).unwrap();
    let count = |db: &Connection| -> i64 {
        db.query_row("SELECT count(*) FROM probe_rows", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count(&Connection::open(&file).unwrap()), 0);
    assert_eq!(count(&f.store.connect().unwrap()), 1);
}

/// The vault seals with the key of the extension that holds a store's,
/// never with a device key the core makes beside it; a store the extension
/// does not hold keeps the device key file, as in Core.
#[test]
fn the_vault_key_comes_from_the_extension_that_holds_it() {
    let f = Fixture::new();
    let directory = f.store.directory.to_path_buf();
    let device = directory.join("vault.key");
    let plain = f
        .store
        .save_secret(&f.workspace, "plain", "device-sealed")
        .unwrap();
    assert_eq!(
        f.store.secret(&f.workspace, &plain).unwrap(),
        "device-sealed"
    );
    assert_eq!(std::fs::metadata(&device).unwrap().len(), 32);

    std::fs::remove_file(&device).unwrap();
    std::fs::write(directory.join("probe-vault-key"), b"").unwrap();
    let held = f
        .store
        .save_secret(&f.workspace, "held", "probe-sealed")
        .unwrap();
    assert_eq!(f.store.secret(&f.workspace, &held).unwrap(), "probe-sealed");
    assert!(!device.exists(), "no device key is made for a held store");
    // It was sealed with the probe's key: a device key does not open it.
    std::fs::remove_file(directory.join("probe-vault-key")).unwrap();
    let error = f.store.secret(&f.workspace, &held).unwrap_err();
    assert_eq!(error.message, "Credential integrity check failed");

    // An unavailable key fails the operation, with no fallback.
    std::fs::remove_file(&device).unwrap();
    std::fs::write(directory.join("probe-vault-down"), b"").unwrap();
    let error = f
        .store
        .save_secret(&f.workspace, "down", "never-sealed")
        .unwrap_err();
    assert_eq!(error.message, "Probe key service unavailable");
    assert!(!device.exists(), "no device key is made as a fallback");
    std::fs::remove_file(directory.join("probe-vault-down")).unwrap();

    // A vault.key that holds no key fails closed and is left alone: what a
    // program without the extension meets on a store whose key it holds.
    std::fs::write(&device, b"held by an extension").unwrap();
    let error = f.store.secret(&f.workspace, &plain).unwrap_err();
    assert_eq!(error.message, "Invalid local vault key");
    let error = f
        .store
        .save_secret(&f.workspace, "new", "value")
        .unwrap_err();
    assert_eq!(error.message, "Invalid local vault key");
    assert_eq!(std::fs::read(&device).unwrap(), b"held by an extension");
}

#[tokio::test]
async fn job_kinds_the_core_does_not_run_go_to_the_extension() {
    let f = Fixture::new();
    let worker = Worker::new(f.store.clone(), model::Config::default()).unwrap();
    let job = |kind: &str| Job {
        id: format!("{kind}-job"),
        repository: f.repo.clone(),
        workspace: f.workspace.clone(),
        principal_hash: graphicle_memory::text::sha256(f.owner.as_bytes()),
        lease: "lease".into(),
        kind: kind.into(),
        payload: json!({}),
    };
    let result = graphicle_runtime::workflows::execute(&worker, &job("probe"))
        .await
        .unwrap();
    assert_eq!(result, json!({"probe_job":"probe-job"}));
    let error = graphicle_runtime::workflows::execute(&worker, &job("unknown"))
        .await
        .unwrap_err();
    assert_eq!(error.message, "Unknown workflow");
}

/// A declared job kind is leased and run through the extension; a job of a
/// kind no installed extension declares (queued by an edition with more
/// extensions) stays queued, reported as waiting, and never stalls the
/// queue for the work this program can run.
#[tokio::test]
async fn declared_job_kinds_are_leased_and_unknown_kinds_wait() {
    let f = Fixture::new();
    let stamp = graphicle_runtime::now();
    let principal = graphicle_memory::text::sha256(f.owner.as_bytes());
    let db = f.store.connect().unwrap();
    // The unknown kind is first in line.
    for (id, kind, priority) in [("unknown-job", "future_kind", 5), ("probe-job", "probe", 0)] {
        db.execute(
            "INSERT INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,priority,max_attempts,not_before,created_at,updated_at) VALUES(?,?,?,?,'{}',?,'queued',?,3,?,?,?)",
            rusqlite::params![id, f.repo, kind, id, principal, priority, stamp, stamp, stamp],
        )
        .unwrap();
    }
    let worker = Worker::new(f.store.clone(), model::Config::default()).unwrap();
    assert!(worker.once().await.unwrap(), "the probe job runs");
    let status = |id: &str| {
        f.store
            .job_status(
                &f.workspace,
                &f.owner,
                &json!({"repository_id":f.repo,"job_id":id}),
            )
            .unwrap()
    };
    let probe = status("probe-job");
    assert_eq!(probe["state"], "complete", "{probe}");
    assert_eq!(probe["result"], json!({"probe_job":"probe-job"}));
    assert!(!worker.once().await.unwrap(), "nothing else is runnable");
    let unknown = status("unknown-job");
    assert_eq!(unknown["state"], "queued", "{unknown}");
    assert_eq!(unknown["attempt"], 0);
    assert!(unknown["error"].is_null());
    assert_eq!(
        unknown["waiting_for"],
        graphicle_runtime::jobs::UNKNOWN_KIND,
        "{unknown}"
    );
    // The declared kind's permission applies when it is queued too.
    assert_eq!(
        graphicle_runtime::jobs::permission("probe").unwrap(),
        "read"
    );
    assert!(graphicle_runtime::jobs::permission("future_kind").is_err());
}

#[test]
fn the_scheduler_counts_extension_jobs() {
    let f = Fixture::new();
    assert_eq!(f.store.schedule_connectors().unwrap(), 2);
}

/// The work an extension queues after a
/// projection (`after_projection`, of a kind declared to follow
/// projections) runs in the projection drain after the `project` and `link`
/// jobs. A hook that fails has its own writes rolled back and is reported in
/// the projection's result; the projection is recorded.
#[test]
fn the_projection_drain_runs_the_work_extensions_queue_after_a_projection() {
    let f = Fixture::new();
    assert_eq!(extension::installed().post_projection_kinds(), [FOLLOW]);
    let synced = |name: &str| -> String {
        let project = f
            .store
            .register(
                &f.workspace,
                &f.owner,
                &json!({"project_key":name,"name":name}),
            )
            .unwrap();
        let args = |mut value: Value| {
            value["repository_id"] = project["id"].clone();
            value
        };
        let text = "def alpha():\n    return 1\n";
        let state = f
            .store
            .project_status(&f.workspace, &f.owner, &args(json!({})))
            .unwrap();
        let begin = f
            .store
            .sync_prepare(
                &f.workspace,
                &f.owner,
                &args(json!({"expected_snapshot":state["repository"]["snapshot"],
                    "manifest":[{"path":"a.py","sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}]})),
            )
            .unwrap();
        f.store
            .sync_upload(
                &f.workspace,
                &f.owner,
                &args(json!({"sync_id":begin["sync_id"],"files":[{"path":"a.py","content":text}]})),
            )
            .unwrap();
        f.store
            .sync_commit(
                &f.workspace,
                &f.owner,
                &args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
        s(&project, "id").to_string()
    };
    let kinds = |ran: &[Value]| -> Vec<String> {
        ran.iter().map(|job| s(job, "kind").to_string()).collect()
    };
    let db = f.store.connect().unwrap();
    let follows = synced("Probe follows");
    let ran = graphicle_runtime::jobs::drain_projection(&f.store, &follows).unwrap();
    assert_eq!(kinds(&ran), ["project", "link", FOLLOW], "{ran:?}");
    assert!(ran.iter().all(|job| job["ok"] == true), "{ran:?}");
    let (state, result): (String, String) = db
        .query_row(
            "SELECT state,result FROM am_jobs WHERE repository_id=? AND kind=?",
            rusqlite::params![follows, FOLLOW],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "complete");
    let episode = graphicle_runtime::graph_projection::current_episode(&db, &follows).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap(),
        json!({"followed":episode})
    );
    assert!(
        graphicle_runtime::jobs::drain_projection(&f.store, &follows)
            .unwrap()
            .is_empty()
    );

    let fails = synced("Probe fails");
    let ran = graphicle_runtime::jobs::drain_projection(&f.store, &fails).unwrap();
    assert_eq!(kinds(&ran), ["project", "link"], "{ran:?}");
    let queued: i64 = db
        .query_row(
            "SELECT count(*) FROM am_jobs WHERE repository_id=? AND kind=?",
            rusqlite::params![fails, FOLLOW],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued, 0, "the failed hook's row was kept");
    let projected: String = db
        .query_row(
            "SELECT result FROM am_jobs WHERE repository_id=? AND kind='project'",
            [&fails],
            |r| r.get(0),
        )
        .unwrap();
    let projected: Value = serde_json::from_str(&projected).unwrap();
    assert_eq!(
        projected["after_projection"],
        json!([{"extension":"probe","error":"probe hook failed"}])
    );
    let episode = graphicle_runtime::graph_projection::current_episode(&db, &fails).unwrap();
    assert!(graphicle_runtime::graph_projection::is_recorded(&db, &fails, &episode).unwrap());
}

#[derive(Clone)]
struct Issues(Arc<Mutex<Vec<String>>>);
async fn issues(State(seen): State<Issues>) -> Json<Value> {
    seen.0.lock().unwrap().push("issues".into());
    let mut issue = json!({"number":1,"title":"Rollout","body":"Codename Copperfern."});
    issue["comments"] = json!(0);
    issue["html_url"] = json!("https://github.com/org/project/issues/1");
    Json(json!([issue]))
}

#[tokio::test]
async fn provider_permissions_of_a_sync_come_from_the_extension() {
    let f = Fixture::new();
    let credential = f
        .store
        .put_credential(
            &f.workspace,
            &f.owner,
            &json!({"name":"Fixture token","value":"fixture-only-access"}),
        )
        .unwrap();
    let config = json!({"repository":"org/project","include_files":false,
        "permission_mode":"provider","acl":{"workspace_readers":true,"credentials":[]}});
    let connector = f
        .store
        .connector_add(
            &f.workspace,
            &f.owner,
            &json!({"repository_id":f.repo,"name":"GitHub","kind":"github",
                "credential_ref":credential["credential_ref"],"config":config}),
        )
        .unwrap();
    let seen = Issues(Arc::new(Mutex::new(Vec::new())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    // Only the issue list is served: the core's own permission fetch
    // (repos/org/project/collaborators) would fail.
    let router = Router::new()
        .route("/repos/org/project/issues", get(issues))
        .with_state(seen.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut config = model::Config::default();
    config.connector_endpoints.insert("github".into(), base);
    let worker = Worker::new(f.store.clone(), config.clone()).unwrap();
    let job = f
        .store
        .enqueue(
            &f.workspace,
            &f.owner,
            &json!({"repository_id":f.repo,"kind":"connector","connector_id":connector["id"]}),
            &config,
        )
        .unwrap();
    assert!(worker.once().await.unwrap());
    let status = f
        .store
        .job_status(
            &f.workspace,
            &f.owner,
            &json!({"repository_id":f.repo,"job_id":job["id"]}),
        )
        .unwrap();
    assert_eq!(status["state"], "complete", "{status}");
    assert_eq!(seen.0.lock().unwrap().as_slice(), ["issues"]);
    let metadata: String = f
        .store
        .connect()
        .unwrap()
        .query_row(
            "SELECT metadata FROM am_connector_docs WHERE connector_id=?",
            [s(&connector, "id")],
            |r| r.get(0),
        )
        .unwrap();
    let metadata: Value = serde_json::from_str(&metadata).unwrap();
    let permissions = &metadata["provider_permissions"];
    assert_eq!(
        permissions["probe_doc"], "org/project:issue:1",
        "{metadata}"
    );
    assert_eq!(permissions["connector_kind"], "github");
    assert_eq!(permissions["principals"], json!([{"type":"user","id":"7"}]));
    server.abort();
}

/// A connector kind an extension adds is configured, probed, synced, shared
/// and connected through OAuth by the core like its own kinds.
#[tokio::test]
async fn connector_kinds_an_extension_adds_run_through_the_core() {
    let f = Fixture::new();
    let credential = f
        .store
        .put_credential(
            &f.workspace,
            &f.owner,
            &json!({"name":"Probe token","value":"fixture-only-access"}),
        )
        .unwrap();
    let acl = json!({"workspace_readers":true,"credentials":[]});
    let add = |config: Value| {
        f.store.connector_add(
            &f.workspace,
            &f.owner,
            &json!({"repository_id":f.repo,"name":"Docs","kind":"probe_docs",
                "credential_ref":credential["credential_ref"],"config":config}),
        )
    };
    assert_eq!(
        add(json!({"topic":"probes","acl":acl,"other":1}))
            .unwrap_err()
            .message,
        "Unknown connector configuration field"
    );
    assert!(
        add(json!({"acl":acl})).is_err(),
        "the source checks its fields"
    );
    assert!(
        add(json!({"topic":"probes","acl":acl,"permission_mode":"provider"})).is_err(),
        "no extension syncs this kind's provider permissions"
    );
    let connector = add(json!({"topic":"probes","acl":acl})).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            "/docs/{id}",
            get(|Path(id): Path<String>| async move {
                Json(json!({"title":format!("Doc {id}"),"text":format!("Probe document {id}.")}))
            }),
        )
        .route(
            "/whoami",
            get(|| async { Json(json!({"id":"probe-user"})) }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut config = model::Config::default();
    config.connector_endpoints.insert("probe_docs".into(), base);
    let worker = Worker::new(f.store.clone(), config.clone()).unwrap();
    for kind in ["connector_probe", "connector"] {
        let job = f
            .store
            .enqueue(
                &f.workspace,
                &f.owner,
                &json!({"repository_id":f.repo,"kind":kind,"connector_id":connector["id"]}),
                &config,
            )
            .unwrap();
        assert!(worker.once().await.unwrap());
        let status = f
            .store
            .job_status(
                &f.workspace,
                &f.owner,
                &json!({"repository_id":f.repo,"job_id":job["id"]}),
            )
            .unwrap();
        assert_eq!(status["state"], "complete", "{status}");
        if kind == "connector_probe" {
            assert_eq!(status["result"]["identity"]["id"], "probe-user");
            assert_eq!(status["result"]["target"]["topic"], "probes");
        } else {
            assert_eq!(status["result"]["listed_documents"], 2, "{status}");
            assert_eq!(status["result"]["complete"], true, "{status}");
        }
    }
    let reader = f.grant("Reader");
    let titles = f
        .store
        .authorized(&f.workspace, s(&reader, "capability"), "read", |db, p| {
            let repository = graphicle_runtime::store::repo(db, p, &f.repo)?;
            Ok(graphicle_runtime::memory::documents(db, &repository, p)?
                .iter()
                .map(|doc| s(doc, "title").to_string())
                .collect::<Vec<_>>())
        })
        .unwrap();
    assert_eq!(titles, ["Doc d1", "Doc d2"]);
    assert_eq!(
        graphicle_runtime::connectors::identity_provider("probe_docs"),
        "probe_accounts"
    );
    let oauth = |provider: &str| {
        graphicle_runtime::oauth::configure(
            &f.store,
            &f.workspace,
            &f.owner,
            &json!({"provider":provider,"config":{"client_id":"probe",
            "authorization_url":"https://login.invalid/authorize","token_url":"https://login.invalid/token",
            "identity_url":"https://login.invalid/me","scopes":["read"],"pkce":true}}),
        )
    };
    oauth("probe_accounts").unwrap();
    assert!(oauth("unknown_accounts").is_err());
    server.abort();
}

fn args(items: &[&std::ffi::OsStr]) -> Vec<OsString> {
    items.iter().map(|item| item.to_os_string()).collect()
}

#[tokio::test]
async fn extension_commands_run_first_with_a_lazily_opened_store() {
    install();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    graphicle_runtime::cli::run(args(&[
        "graphicle".as_ref(),
        "--data-dir".as_ref(),
        data.as_os_str(),
        "probe".as_ref(),
        "--word".as_ref(),
        "hello".as_ref(),
    ]))
    .await
    .unwrap();
    assert!(recorded(&format!("cli:hello:{}", data.display())));
    assert!(data.join("reviews.sqlite3").is_file());
    // Core commands still run when an extension is installed.
    let core = dir.path().join("core");
    Box::pin(graphicle_runtime::cli::run(args(&[
        "graphicle".as_ref(),
        "--data-dir".as_ref(),
        core.as_os_str(),
        "init".as_ref(),
    ])))
    .await
    .unwrap();
    assert!(core.join("local-session.json").is_file());
}

#[tokio::test]
async fn serve_prepares_extensions_and_runs_their_background_tasks() {
    install();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("serve");
    let serve = Box::pin(graphicle_runtime::cli::run(args(&[
        "graphicle".as_ref(),
        "--data-dir".as_ref(),
        data.as_os_str(),
        "serve".as_ref(),
        "--port".as_ref(),
        "0".as_ref(),
    ])));
    let background = format!("background:{}", data.display());
    tokio::select! {
        result = serve => panic!("serve stopped: {:?}", result.err().map(|e| e.message)),
        _ = until_recorded(&background) => {}
        _ = tokio::time::sleep(Duration::from_secs(120)) => panic!("no background task ran"),
    }
    assert!(recorded("serve:loopback"));
}

#[test]
fn a_role_an_extension_defines_grants_only_what_it_says() {
    let f = Fixture::new();
    // `Store::grant` accepts a role an installed extension defines, and
    // nothing else beyond the core's roles.
    let probe = f
        .store
        .grant(
            &f.workspace,
            &f.owner,
            &json!({"label":"probe role","role":"probe-role"}),
        )
        .unwrap();
    assert_eq!(probe["role"], "probe-role");
    let error = f
        .store
        .grant(
            &f.workspace,
            &f.owner,
            &json!({"label":"nobody's","role":"undefined-role"}),
        )
        .unwrap_err();
    assert_eq!(error.message, "Invalid credential role");
    let capability = s(&probe, "capability");
    let allowed = |token: &str, permission: &str| {
        f.store
            .authorized(&f.workspace, token, permission, |_, _| Ok(()))
            .is_ok()
    };
    assert!(allowed(capability, "read"));
    assert!(allowed(capability, "propose"));
    assert!(!allowed(capability, "collect"));
    assert!(!allowed(capability, "admin"));
    // The core's roles keep their meaning: the probe's answer for `reader`
    // is never asked.
    let reader = f.grant("plain reader");
    assert!(allowed(s(&reader, "capability"), "read"));
    assert!(!allowed(s(&reader, "capability"), "propose"));
    // A role no one defines is denied everything.
    f.store
        .connect()
        .unwrap()
        .execute(
            "UPDATE credentials SET role='undefined-role' WHERE id=?",
            [s(&probe, "id")],
        )
        .unwrap();
    assert!(!allowed(capability, "read"));
}

/// A credential the store requires a capability for is usable where an
/// installed extension sets the capability, and refused where none does,
/// whatever its role grants; the owner capability is never refused.
#[test]
fn a_credential_the_store_requires_a_capability_for_needs_it() {
    let f = Fixture::new();
    let restricted = f.grant("restricted");
    let token = s(&restricted, "capability");
    let allowed = |token: &str| {
        f.store
            .authorized(&f.workspace, token, "read", |_, _| Ok(()))
            .is_ok()
    };
    let db = f.store.connect().unwrap();
    db.execute_batch("CREATE TABLE credential_requirements(credential_id TEXT, capability TEXT)")
        .unwrap();
    db.execute(
        "INSERT INTO credential_requirements VALUES(?,'probe')",
        [s(&restricted, "id")],
    )
    .unwrap();
    assert_eq!(
        graphicle_runtime::store::required_capabilities(&db, s(&restricted, "id")).unwrap(),
        ["probe"]
    );
    assert!(allowed(token), "the probe sets the capability");
    db.execute(
        "INSERT INTO credential_requirements VALUES(?,'absent_capability')",
        [s(&restricted, "id")],
    )
    .unwrap();
    assert!(!allowed(token), "no installed extension sets it");
    assert!(allowed(s(&f.grant("unrestricted"), "capability")));
    assert!(allowed(&f.owner));
}

/// A credential of the probe's that `authorize` restricts, with `role`.
fn guarded(f: &Fixture, label: &str, role: &str) -> String {
    let credential = f
        .store
        .grant(&f.workspace, &f.owner, &json!({"label":label,"role":role}))
        .unwrap();
    s(&credential, "capability").to_string()
}

async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let reply = router.clone().oneshot(request).await.unwrap();
    let status = reply.status();
    let body = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn authorize_is_asked_wherever_a_credential_is_authorized() {
    let f = Fixture::new();
    let app = f.app();
    let token = guarded(&f, "guarded", "reviewer");
    let hidden = f
        .store
        .register(
            &f.workspace,
            &f.owner,
            &json!({"project_key":"hidden","name":"Hidden"}),
        )
        .unwrap();
    // Every authorization: a permission the role grants can be refused.
    let collect = f
        .store
        .authorized(&f.workspace, &token, "collect", |_, _| Ok(()))
        .unwrap_err();
    assert_eq!(collect.message, "probe refuses collect");
    assert!(recorded("access:guarded:Permission(\"collect\")"));
    // Every repository opened or listed.
    let listed = f.store.projects(&f.workspace, &token).unwrap();
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|p| s(p, "id"))
        .collect();
    assert_eq!(ids, [f.repo.as_str()]);
    let owner_listed = f.store.projects(&f.workspace, &f.owner).unwrap();
    assert_eq!(owner_listed.as_array().unwrap().len(), 2);
    assert!(
        api::command(
            &app,
            &f.workspace,
            &token,
            "status",
            &json!({"repository_id":hidden["id"]})
        )
        .is_err()
    );
    // Every operation, on the synchronous and the async path.
    let args = json!({"repository_id":f.repo});
    let stats = api::command(&app, &f.workspace, &token, "graph.stats", &args).unwrap_err();
    assert_eq!(stats.message, "probe refuses graph.stats");
    let stats = api::command_async(&app, &f.workspace, &token, "graph.stats", &args)
        .await
        .unwrap_err();
    assert_eq!(stats.message, "probe refuses graph.stats");
    api::command(&app, &f.workspace, &f.owner, "graph.stats", &args).unwrap();
    // Narrowed arguments reach the operation.
    let echo = api::command(&app, &f.workspace, &token, "probe.echo", &args).unwrap();
    assert_eq!(echo["args"]["narrowed"], true, "{echo}");
    let echo = api::command(&app, &f.workspace, &f.owner, "probe.echo", &args).unwrap();
    assert!(echo["args"].get("narrowed").is_none(), "{echo}");
    // Every MCP tool listed or called.
    let mcp = |token: &str, method: &str, params: Value| {
        api::mcp(
            &app,
            &f.workspace,
            token,
            &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .unwrap()
    };
    let names = |reply: Value| -> Vec<String> {
        reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| s(t, "name").to_string())
            .collect()
    };
    let listed = names(mcp(&token, "tools/list", json!({})));
    assert!(!listed.iter().any(|n| n == "repository_status"));
    assert!(listed.iter().any(|n| n == "recall_project"));
    assert!(
        names(mcp(&f.owner, "tools/list", json!({})))
            .iter()
            .any(|n| n == "repository_status")
    );
    let refused = mcp(
        &token,
        "tools/call",
        json!({"name":"repository_status","arguments":{}}),
    );
    assert_eq!(
        refused["error"]["message"], "probe refuses repository_status",
        "{refused}"
    );
    let stats = mcp(
        &token,
        "tools/call",
        json!({"name":"graph_history_stats","arguments":{}}),
    );
    assert_eq!(stats["result"]["isError"], true, "{stats}");
    assert_eq!(
        stats["result"]["content"][0]["text"], "probe refuses graph.stats",
        "{stats}"
    );
    assert!(recorded(
        "access:guarded:Tool { name: \"graph_history_stats\", writes: false }"
    ));
    // Resource reads are admitted as the navigation tools' operation.
    let resource = mcp(
        &token,
        "resources/read",
        json!({"uri":"graphicle-graph://stats"}),
    );
    assert_eq!(
        resource["error"]["message"], "probe refuses graph.navigate",
        "{resource}"
    );
    // The pull-request tools, which run no `api::command` operation, are
    // admitted as theirs.
    let mut pull_requests = app.clone();
    pull_requests.mcp.pull_requests = true;
    let prs = api::mcp(
        &pull_requests,
        &f.workspace,
        &token,
        &f.repo,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_prs","arguments":{}}}),
    )
    .unwrap();
    assert_eq!(
        prs["result"]["content"][0]["text"], "probe refuses graph.pull_requests",
        "{prs}"
    );
    // A workspace connection lists only the allowed tools too.
    let workspace_tools = names(
        api::mcp_workspace(
            &app,
            &f.workspace,
            &token,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
        )
        .unwrap(),
    );
    assert!(workspace_tools.iter().any(|n| n == "recall_project"));
    assert!(!workspace_tools.iter().any(|n| n == "repository_status"));
    // Every route that names a workspace, before its handler.
    let router = api::router(app.clone());
    let get = |path: String, token: &str| {
        Request::get(path)
            .header(header::HOST, "localhost")
            .header("x-workspace-capability", token)
            .body(Body::empty())
            .unwrap()
    };
    let episodes = format!(
        "/api/workspaces/{}/repositories/{}/graph/episodes",
        f.workspace, f.repo
    );
    let (status, body) = send(&router, get(episodes.clone(), &token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "probe refuses the episodes route");
    assert_eq!(
        send(&router, get(episodes, &f.owner)).await.0,
        StatusCode::OK
    );
    let (status, _) = send(
        &router,
        get(format!("/api/workspaces/{}", f.workspace), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(recorded(
        "access:guarded:Route { method: \"GET\", pattern: \"/api/workspaces/{workspace}\" }"
    ));
    // The HTTP command endpoint runs the same operation check.
    let command = Request::post(format!(
        "/api/workspaces/{}/repositories/command",
        f.workspace
    ))
    .header(header::HOST, "localhost")
    .header(header::CONTENT_TYPE, "application/json")
    .header("x-workspace-capability", &token)
    .body(Body::from(
        json!({"operation":"graph.stats","arguments":args}).to_string(),
    ))
    .unwrap();
    assert_eq!(send(&router, command).await.0, StatusCode::FORBIDDEN);
    // The owner capability is never asked about.
    assert!(!recorded("access:owner"));
}

#[tokio::test]
async fn an_extension_may_limit_the_tokens_of_a_response() {
    let f = Fixture::new();
    let app = f.app();
    let token = guarded(&f, "guarded-capped", "reader");
    let short = json!({"repository_id":f.repo});
    // Each response holds at most 120 tokens for this credential.
    api::command(&app, &f.workspace, &token, "probe.echo", &short).unwrap();
    let long = json!({"repository_id":f.repo,"word":"lorem ipsum dolor sit amet ".repeat(40)});
    let error = api::command(&app, &f.workspace, &token, "probe.echo", &long).unwrap_err();
    assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        error.message.contains("at most 120 per call"),
        "{}",
        error.message
    );
    api::command(&app, &f.workspace, &f.owner, "probe.echo", &long).unwrap();
    // Route responses too.
    for index in 0..6 {
        f.store
            .register(
                &f.workspace,
                &f.owner,
                &json!({"project_key":format!("listed-{index}"),"name":format!("Listed {index}")}),
            )
            .unwrap();
    }
    let router = api::router(app);
    let listing = |token: &str| {
        Request::get(format!("/api/workspaces/{}/repositories", f.workspace))
            .header(header::HOST, "localhost")
            .header("x-workspace-capability", token)
            .body(Body::empty())
            .unwrap()
    };
    let (status, body) = send(&router, listing(&f.owner)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        graphicle_memory::text::count(&body.to_string()) > 120,
        "{body}"
    );
    let (status, body) = send(&router, listing(&token)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(s(&body, "error").contains("at most 120 per call"), "{body}");
}
