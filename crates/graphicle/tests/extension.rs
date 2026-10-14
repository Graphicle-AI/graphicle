//! Every extension hook at the place the runtime calls it. One probe
//! extension implementing all hooks is installed for this whole test binary
//! (the registry is process-global), so each test installs it first.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode, header},
    routing::get,
};
use graphicle_runtime::{
    Error, Result,
    api::{self, App},
    connectors::Remote,
    extension::{
        self, BoxFuture, CliContext, Edition, Extension, Extensions, JobKind, McpTool, ServeContext,
    },
    jobs::{Job, Worker},
    model,
    store::{Store, one, s},
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
        Router::new().route(
            "/probe",
            get(|State(app): State<App>| async move {
                Json(json!({"probe":true,"hosted":app.public_origin.is_some()}))
            }),
        )
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
    fn scrub_backup(&self, db: &Connection) -> Result<()> {
        db.execute("DELETE FROM probe_rows", [])?;
        Ok(())
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
