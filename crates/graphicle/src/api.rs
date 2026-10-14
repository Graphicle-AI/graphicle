use crate::{Error, Result, string};
use crate::{
    extension::McpTool,
    jobs::Worker,
    model,
    store::{Store, repo, s},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct App {
    pub worker: Worker,
    pub allowed_hosts: Arc<Vec<String>>,
    pub mcp_learning: bool,
    /// Publish the agent's work-state tools (`work_state_write`/`_read`).
    pub mcp_work_state: bool,
    /// The rest of what the MCP server publishes and fixes.
    pub mcp: McpOptions,
    pub public_origin: Option<String>,
    pub local_session: Arc<Mutex<Option<(String, Instant, Value)>>>,
    /// The secret of a local `serve`: `/health?challenge=`
    /// answers with a proof of it ([`crate::terminal::server_proof`]), so
    /// that `graphicle open` signs in only to the server of its own data
    /// folder, which keeps the secret in `server.json`.
    pub instance_secret: Option<Arc<String>>,
}
impl App {
    pub fn new(store: Store, config: model::Config, hosts: Vec<String>) -> Result<Self> {
        Ok(Self {
            worker: Worker::new(store, config)?,
            allowed_hosts: Arc::new(hosts),
            mcp_learning: false,
            mcp_work_state: false,
            mcp: McpOptions::default(),
            public_origin: None,
            local_session: Arc::new(Mutex::new(None)),
            instance_secret: None,
        })
    }
    pub fn hosted(mut self, origin: &str) -> Result<Self> {
        let parsed = model::checked_url(origin)?;
        if parsed.scheme() != "https"
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Error::bad(
                "Public origin must be an HTTPS origin without a path or query",
            ));
        }
        let origin = parsed.origin().ascii_serialization();
        let host = origin.strip_prefix("https://").unwrap().to_string();
        Arc::make_mut(&mut self.allowed_hosts).push(host);
        self.public_origin = Some(origin);
        Ok(self)
    }
    pub fn browser_ticket(&self, session: Value) -> Result<String> {
        let ticket = format!("{}{}", crate::id(), crate::id());
        *self
            .local_session
            .lock()
            .map_err(|_| Error::bad("Session lock unavailable"))? =
            Some((ticket.clone(), Instant::now(), session));
        Ok(ticket)
    }
}
/// One MCP server: what `graphicle mcp` (stdio) or
/// `graphicle serve` (the HTTP route) publishes beside the learning and
/// work-state tools, and the retrieval switches it fixes.
#[derive(Clone, Debug, Default)]
pub struct McpOptions {
    /// `--tools`: the whole server, or one half of it (two servers over one
    /// store can then serve the halves separately).
    pub tools: ToolSet,
    /// `--set`: switches of `recall_project` ([`RECALL_SWITCHES`]) fixed by
    /// the server: a request that names one
    /// with another value is refused, the packet's `mechanisms` reports them
    /// under `server_fixed`, and no published schema lists them.
    pub fixed: serde_json::Map<String, Value>,
    /// `graphicle mcp --pull-requests`: publish the engine's pull-request
    /// tools, which run `gh` in the project's folder (stdio only).
    pub pull_requests: bool,
    /// The folder `graphicle mcp` runs in: over stdio a tool's `repo` may
    /// name a synced folder, resolved from here. `None` over HTTP.
    pub cwd: Option<std::path::PathBuf>,
}

/// Which tools a server publishes (`--tools`). The two halves partition the
/// tools: every tool is in exactly one, so two servers over one store, one
/// of each, offer the same tools as one server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolSet {
    /// Both: the one integrated server (the default).
    #[default]
    All,
    /// The code-graph half: every tool that reads the typed code graph
    /// ([`reads_code_graph`]): the graph engine's tools under Graphify's names
    /// (and the resources), the graph-history tools (`graph_episodes`,
    /// `graph_as_of`, `graph_history_stats`, `graph_link`) and an edition's
    /// code-graph tools.
    Graph,
    /// The memory half: the other tools (sources, recall, memory, captured
    /// sessions and work state). `recall_project` still reads the code graph
    /// through its code-graph arm unless the server fixes `graph_retrieval`
    /// off (`--set`).
    Memory,
}

impl ToolSet {
    /// `all`, `graph` or `memory`.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "all" => Ok(Self::All),
            "graph" => Ok(Self::Graph),
            "memory" => Ok(Self::Memory),
            _ => Err(Error::bad("Use --tools all, graph or memory")),
        }
    }
    pub fn publishes(self, tool: &McpTool) -> bool {
        match self {
            Self::All => true,
            Self::Graph => reads_code_graph(tool),
            Self::Memory => !reads_code_graph(tool),
        }
    }
}

/// Whether a tool belongs to the code-graph half of the server
/// ([`ToolSet::Graph`]): its operation is in the `graph.` family (the
/// engine's navigation and pull-request tools, the graph-history tools), or
/// the `impact.` or `overlay.` families an edition serves over the code graph.
pub fn reads_code_graph(tool: &McpTool) -> bool {
    ["graph.", "impact.", "overlay."]
        .iter()
        .any(|family| tool.operation.starts_with(family))
}

/// Parse `--set switch=true|false` values into fixed switches; only the
/// switches of [`RECALL_SWITCHES`] can be fixed, each at most once.
pub fn fixed_switches(values: &[String]) -> Result<serde_json::Map<String, Value>> {
    let mut fixed = serde_json::Map::new();
    for value in values {
        let (name, setting) = value
            .split_once('=')
            .ok_or_else(|| Error::bad("Use --set <switch>=true or false"))?;
        if !RECALL_SWITCHES.contains(&name) {
            let (last, rest) = RECALL_SWITCHES.split_last().unwrap_or((&"", &[]));
            return Err(Error::bad(format!(
                "{name} cannot be fixed; the server fixes {} and {last}",
                rest.join(", ")
            )));
        }
        let setting = match setting {
            "true" => true,
            "false" => false,
            _ => return Err(Error::bad(format!("{name} is true or false"))),
        };
        if fixed.insert(name.to_string(), json!(setting)).is_some() {
            return Err(Error::bad(format!("{name} is set twice; give it once")));
        }
    }
    Ok(fixed)
}

pub fn router(app: App) -> Router {
    Router::new()
        .merge(crate::product::routes())
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/settings.js", get(settings_script))
        .route("/extensions.js", get(extensions_script))
        .route("/style.css", get(style))
        .route("/fonts/inter-latin.woff2", get(font_inter))
        .route("/fonts/space-grotesk-latin.woff2", get(font_space_grotesk))
        .route("/health", get(health))
        .route("/api/workspaces", post(create_workspace))
        .route("/api/local-session", post(local_session))
        .route("/api/workspaces/{workspace}", get(workspace))
        .route(
            "/api/workspaces/{workspace}/browser-session",
            post(browser_session),
        )
        .route("/api/workspaces/{workspace}/repositories", get(projects))
        .route(
            "/api/workspaces/{workspace}/repositories/command",
            post(http_command),
        )
        .route(
            "/api/workspaces/{workspace}/repositories/{repository}/graph/project",
            post(graph_project),
        )
        .route(
            "/api/workspaces/{workspace}/repositories/{repository}/graph/episodes",
            get(graph_episodes),
        )
        .route(
            "/api/workspaces/{workspace}/repositories/{repository}/impact",
            post(graph_impact),
        )
        .route(
            "/api/workspaces/{workspace}/repositories/{repository}/graph/as_of",
            post(graph_as_of),
        )
        .route("/api/workspaces/{workspace}/graph/link", post(graph_link))
        .route(
            "/api/workspaces/{workspace}/repositories/{repository}/graph/cross_edges",
            get(graph_cross_edges),
        )
        .route(
            "/api/workspaces/{workspace}/repositories/{repository}/graph/stats",
            get(graph_stats),
        )
        .route(
            "/api/workspaces/{workspace}/repositories/{repository}/access",
            post(graph_access),
        )
        .route(
            "/v1/workspaces/{workspace}/repos/{repo}/impact/report",
            post(impact_report),
        )
        .route(
            "/v1/workspaces/{workspace}/repos/{repo}/coverage",
            post(coverage_upload),
        )
        .route(
            "/v1/workspaces/{workspace}/repos/{repo}/graph/labels",
            post(graph_label),
        )
        .route("/api/workspaces/{workspace}/credentials", post(grant))
        .route(
            "/api/workspaces/{workspace}/credentials/{credential}/revoke",
            post(revoke),
        )
        .route("/api/models", get(models))
        .route("/api/workspaces/{workspace}/models", get(workspace_models))
        .route("/api/workspaces/{workspace}/oauth/start", post(oauth_start))
        .route("/oauth/callback/{provider}", get(oauth_callback))
        .route("/mcp/{workspace}", post(mcp_http_workspace))
        .route("/mcp/{workspace}/{repository}", post(mcp_http))
        // Inside the layers below: extension routes keep the body limit, the
        // request meter and the host, origin and browser-session boundary.
        .merge(crate::extension::installed().routes())
        .layer(DefaultBodyLimit::max(24 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            app.clone(),
            crate::product::meter,
        ))
        .layer(middleware::from_fn_with_state(app.clone(), boundary))
        .with_state(app)
}
async fn boundary(State(app): State<App>, mut request: Request<Body>, next: Next) -> Response {
    // This header is internal only; untrusted HTTP clients cannot supply session authority.
    request.headers_mut().remove("x-graphicle-browser-session");
    let cookie_name = if app.public_origin.is_some() {
        "__Host-graphicle_session"
    } else {
        "graphicle_session"
    };
    if let Some(cookie) = cookie(request.headers(), cookie_name).map(str::to_string) {
        let explicit = request.headers().get(header::AUTHORIZATION).is_some()
            || request
                .headers()
                .get("x-workspace-capability")
                .is_some_and(|v| !v.is_empty());
        if !explicit
            && !matches!(
                *request.method(),
                axum::http::Method::GET | axum::http::Method::HEAD
            )
            && request
                .headers()
                .get("x-graphicle-request")
                .is_none_or(|v| v != "1")
        {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"Browser mutation requires a same-origin request"})),
            )
                .into_response();
        }
        if let Ok(value) = header::HeaderValue::from_str(&cookie) {
            request
                .headers_mut()
                .insert("x-graphicle-browser-session", value);
        }
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !app.allowed_hosts.iter().any(|allowed| allowed == host) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"Unexpected server host"})),
        )
            .into_response();
    }
    if let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        && !app.public_origin.as_ref().map_or_else(
            || origin == format!("http://{host}") || origin == format!("https://{host}"),
            |public| origin == public && public == &format!("https://{host}"),
        )
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"Cross-origin access denied"})),
        )
            .into_response();
    }
    let mut result = next.run(request).await;
    let headers = result.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "referrer-policy",
        header::HeaderValue::from_static("no-referrer"),
    );
    headers.insert("content-security-policy",header::HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    result
}
/// The value of the one cookie named `name`; none when it is absent, empty,
/// longer than 200 bytes or sent more than once.
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let values: Vec<_> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|v| v.trim().split_once('='))
        .filter(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .collect();
    (values.len() == 1)
        .then(|| values[0])
        .filter(|v| !v.is_empty() && v.len() <= 200)
}
/// The request's credential: `X-Workspace-Capability`, else a bearer token,
/// else the browser session the boundary middleware mapped from its cookie.
pub fn token(headers: &HeaderMap) -> Result<&str> {
    headers
        .get("x-workspace-capability")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        })
        .or_else(|| {
            headers
                .get("x-graphicle-browser-session")
                .and_then(|v| v.to_str().ok())
        })
        .filter(|v| !v.is_empty() && v.len() <= 512)
        .ok_or_else(Error::denied)
}
async fn index() -> impl IntoResponse {
    (
        [("content-type", "text/html; charset=utf-8")],
        include_str!("../web/index.html"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}
async fn settings_script() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        include_str!("../web/settings.js"),
    )
}
/// The installed extensions' scripts (`Extension::ui_script`); empty in Core.
async fn extensions_script() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        crate::extension::installed().ui_script(),
    )
}
async fn style() -> impl IntoResponse {
    (
        [("content-type", "text/css; charset=utf-8")],
        include_str!("../web/style.css"),
    )
}
async fn font_inter() -> impl IntoResponse {
    (
        [("content-type", "font/woff2")],
        include_bytes!("../web/fonts/inter-latin.woff2").as_slice(),
    )
}
async fn font_space_grotesk() -> impl IntoResponse {
    (
        [("content-type", "font/woff2")],
        include_bytes!("../web/fonts/space-grotesk-latin.woff2").as_slice(),
    )
}
async fn health(
    State(app): State<App>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let mut health = json!({"product":"Graphicle","version":env!("CARGO_PKG_VERSION"),"runtime":"rust","status":"ok","hosted":app.public_origin.is_some(),"public_origin":app.public_origin,"self_service_workspace_creation":app.public_origin.is_none()});
    // Only when asked: a proof that this server holds its data folder's
    // secret (the secret itself never leaves the server).
    if let (Some(secret), Some(challenge)) = (&app.instance_secret, query.get("challenge"))
        && !challenge.is_empty()
        && challenge.len() <= 128
    {
        health["proof"] = json!(crate::terminal::server_proof(secret, challenge));
    }
    Json(health)
}
async fn local_session(State(app): State<App>, Json(args): Json<Value>) -> Result<Json<Value>> {
    use subtle::ConstantTimeEq;
    let ticket = string(&args, "ticket", 100)?;
    let mut pending = app.local_session.lock().map_err(|_| Error::denied())?;
    if !pending.as_ref().is_some_and(|(key, time, _)| {
        time.elapsed() < Duration::from_secs(180)
            && bool::from(key.as_bytes().ct_eq(ticket.as_bytes()))
    }) {
        return Err(Error::denied());
    }
    let session = pending.take().unwrap().2;
    app.worker
        .store
        .workspace(s(&session, "workspace"), s(&session, "capability"))?;
    Ok(Json(session))
}
async fn browser_session(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    let capability = token(&headers)?;
    app.worker.store.workspace(&workspace, capability)?;
    let project = args.get("project").and_then(Value::as_str);
    if let Some(project) = project {
        let projects = app.worker.store.projects(&workspace, capability)?;
        if !projects
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| s(row, "id") == project))
        {
            return Err(Error::denied());
        }
    }
    let ticket = app
        .browser_ticket(json!({"workspace":workspace,"capability":capability,"project":project}))?;
    Ok(Json(json!({"ticket":ticket,"expires_in_seconds":180})))
}
async fn models(State(app): State<App>) -> Json<Value> {
    Json(app.worker.config.status())
}
async fn workspace_models(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(crate::settings::status(
        &app.worker.store,
        &workspace,
        token(&headers)?,
        &app.worker.config,
    )?))
}
async fn create_workspace(State(app): State<App>, Json(args): Json<Value>) -> Result<Json<Value>> {
    if app.public_origin.is_some() {
        return Err(Error::bad(
            "This server uses administrator-created workspaces. Ask your workspace owner for access.",
        ));
    }
    Ok(Json(
        app.worker
            .store
            .create_workspace(string(&args, "name", 100)?)?,
    ))
}
async fn workspace(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let mut value = app.worker.store.workspace(&workspace, token(&headers)?)?;
    // A local app's MCP snippet passes `--data-dir` when this
    // store is not in the platform's default folder. Only the owner sees the
    // store's path, and a hosted service never shows its folders.
    if app.public_origin.is_none()
        && value["credential_role"] == "owner"
        && let Ok(directory) = app.worker.store.directory.canonicalize()
        && crate::local::default_data_dir().canonicalize().ok() != Some(directory.clone())
    {
        value["local_mcp"] = json!({"data_dir": directory});
    }
    Ok(Json(value))
}
async fn projects(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(
        app.worker.store.projects(&workspace, token(&headers)?)?,
    ))
}
async fn grant(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    Ok(Json(app.worker.store.grant(
        &workspace,
        token(&headers)?,
        &args,
    )?))
}
async fn revoke(
    State(app): State<App>,
    Path((workspace, credential)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(app.worker.store.revoke(
        &workspace,
        token(&headers)?,
        &credential,
    )?))
}
async fn graph_project(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let token = token(&headers)?.to_string();
    let store = app.worker.store.clone();
    // Extraction is CPU-bound; keep it off the async executor like document.import.
    tokio::task::spawn_blocking(move || store.graph_project(&workspace, &token, &repository))
        .await
        .map(|result| result.map(Json))
        .map_err(|_| Error::bad("Graph projection interrupted"))?
}
async fn graph_episodes(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(app.worker.store.graph_episodes(
        &workspace,
        token(&headers)?,
        &repository,
    )?))
}
/// `graph.impact`: base-versus-head impact between two graph episodes.
async fn graph_impact(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    overridable(
        app,
        &headers,
        (workspace, repository),
        "graph.impact",
        args,
        "Impact analysis interrupted",
    )
    .await
}
async fn graph_link(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let token = token(&headers)?.to_string();
    let store = app.worker.store.clone();
    tokio::task::spawn_blocking(move || store.graph_link(&workspace, &token))
        .await
        .map(|result| result.map(Json))
        .map_err(|_| Error::bad("Cross-repository linking interrupted"))?
}
async fn graph_cross_edges(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(app.worker.store.graph_cross_edges(
        &workspace,
        token(&headers)?,
        &repository,
    )?))
}
async fn graph_stats(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(app.worker.store.graph_stats(
        &workspace,
        token(&headers)?,
        &repository,
    )?))
}
async fn graph_access(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
    Json(mut args): Json<Value>,
) -> Result<Json<Value>> {
    args["repository_id"] = json!(repository);
    Ok(Json(app.worker.store.graph_access(
        &workspace,
        token(&headers)?,
        &args,
    )?))
}
async fn graph_as_of(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    let token = token(&headers)?.to_string();
    let store = app.worker.store.clone();
    tokio::task::spawn_blocking(move || store.graph_as_of(&workspace, &token, &repository, &args))
        .await
        .map(|result| result.map(Json))
        .map_err(|_| Error::bad("Episode reconstruction interrupted"))?
}
/// `impact.report`: the PR change-impact report.
async fn impact_report(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    overridable(
        app,
        &headers,
        (workspace, repository),
        "impact.report",
        args,
        "Impact report interrupted",
    )
    .await
}
/// `coverage.upload`: store an execution-coverage run.
async fn coverage_upload(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    overridable(
        app,
        &headers,
        (workspace, repository),
        "coverage.upload",
        args,
        "Coverage upload interrupted",
    )
    .await
}
/// A repository route for an overridable operation
/// (`extension::OVERRIDABLE_OPERATIONS`), run through [`command`]: an installed
/// extension serves it, and without one it answers 501 after authorization.
/// The repository comes from the path. The work can be CPU-bound (projecting
/// the head set first, parsing large coverage runs), so it runs off the
/// executor.
async fn overridable(
    app: App,
    headers: &HeaderMap,
    (workspace, repository): (String, String),
    operation: &'static str,
    mut args: Value,
    interrupted: &'static str,
) -> Result<Json<Value>> {
    let token = token(headers)?.to_string();
    if let Some(map) = args.as_object_mut() {
        map.insert("repository_id".into(), json!(repository));
    }
    tokio::task::spawn_blocking(move || command(&app, &workspace, &token, operation, &args))
        .await
        .map(|result| result.map(Json))
        .map_err(|_| Error::bad(interrupted))?
}
async fn graph_label(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    if !args.is_object() {
        return Err(Error::bad("Label arguments must be an object"));
    }
    Ok(Json(app.worker.store.graph_label(
        &workspace,
        token(&headers)?,
        &repository,
        string(&args, "label", 128)?,
        string(&args, "episode", 128)?,
    )?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    operation: String,
    #[serde(default)]
    arguments: Value,
}
async fn http_command(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Command>,
) -> Result<Json<Value>> {
    Ok(Json(
        command_async(
            &app,
            &workspace,
            token(&headers)?,
            &body.operation,
            &body.arguments,
        )
        .await?,
    ))
}
pub async fn command_async(
    app: &App,
    workspace: &str,
    capability: &str,
    operation: &str,
    args: &Value,
) -> Result<Value> {
    // An extension operation without an async form runs through `command`
    // on a blocking thread (the fallback arm below).
    if let Some(run) = crate::extension::installed()
        .operation(operation)
        .and_then(|extension| extension.command_async(app, workspace, capability, operation, args))
    {
        return run.await;
    }
    match operation {
        "document.import" => {
            let store = app.worker.store.clone();
            let workspace = workspace.to_string();
            let capability = capability.to_string();
            let args = args.clone();
            tokio::task::spawn_blocking(move || store.import_binary(&workspace, &capability, &args))
                .await
                .map_err(|_| Error::bad("Document converter interrupted"))?
        }
        "oauth.refresh" => {
            crate::oauth::refresh(
                &app.worker.store,
                &app.worker.client,
                workspace,
                capability,
                string(args, "account_id", 100)?,
            )
            .await
        }
        "oauth.resources" => {
            crate::oauth::resources(
                &app.worker.store,
                &app.worker.client,
                workspace,
                capability,
                args,
            )
            .await
        }
        "graph.neo4j.publish" | "graph.neo4j.search" => {
            crate::graph::neo4j(
                &app.worker.store,
                &app.worker.client,
                workspace,
                capability,
                args,
                operation == "graph.neo4j.publish",
            )
            .await
        }
        "vector.publish" => {
            crate::vector::publish(
                &app.worker.store,
                &app.worker.client,
                workspace,
                capability,
                args,
            )
            .await
        }
        _ => {
            // SQLite, graph traversal and optional CPU inference are
            // synchronous. They must not occupy an async HTTP worker.
            let app = app.clone();
            let workspace = workspace.to_owned();
            let capability = capability.to_owned();
            let operation = operation.to_owned();
            let args = args.clone();
            tokio::task::spawn_blocking(move || {
                command(&app, &workspace, &capability, &operation, &args)
            })
            .await
            .map_err(|_| Error::bad("Command interrupted"))?
        }
    }
}
async fn oauth_start(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    let provider = string(&args, "provider", 20)?;
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(Error::denied)?;
    let origin = app
        .public_origin
        .clone()
        .unwrap_or_else(|| format!("http://{host}"));
    let redirect = format!("{origin}/oauth/callback/{provider}");
    Ok(Json(crate::oauth::start(
        &app.worker.store,
        &workspace,
        token(&headers)?,
        provider,
        &redirect,
    )?))
}
async fn oauth_callback(
    State(app): State<App>,
    Path(provider): Path<String>,
    Query(args): Query<std::collections::BTreeMap<String, String>>,
) -> Result<impl IntoResponse> {
    if args.contains_key("error") {
        return Err(Error::bad("Provider declined account connection"));
    }
    crate::oauth::callback(
        &app.worker.store,
        &app.worker.client,
        &provider,
        args.get("state").map(String::as_str).unwrap_or(""),
        args.get("code").map(String::as_str).unwrap_or(""),
    )
    .await?;
    Ok((
        [("content-type", "text/html; charset=utf-8")],
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>Account connected — Graphicle</title><h1>Account connected</h1><p>Your credential is encrypted in this workspace.</p><p><a href=/>Return to Graphicle</a></p></html>",
    ))
}
/// Every operation `command` and `command_async` serve themselves. An
/// extension may not claim one of these, except the overridable ones
/// (`extension::OVERRIDABLE_OPERATIONS`); a test keeps the list in step with
/// the match arms below.
pub const CORE_OPERATIONS: &[&str] = &[
    "build.info",
    "claim.history",
    "connector.add",
    "connector.delete",
    "connector.import",
    "connector.list",
    "connector.observe",
    "connector.schedule",
    "connector.sharing",
    "connector.state",
    "context",
    "coverage.upload",
    "credential.list",
    "credential.remove",
    "credential.store",
    "deletion.receipts",
    "deletion.state",
    "deletion.verify",
    "document.import",
    "document.original",
    "document.versions",
    "evidence.read",
    "files",
    "graph.access",
    "graph.as_of",
    "graph.cross_edges",
    "graph.doc_links",
    "graph.episodes",
    "graph.impact",
    "graph.link",
    "graph.navigate",
    "graph.neo4j.configure",
    "graph.neo4j.publish",
    "graph.neo4j.search",
    "graph.project",
    "graph.query",
    "graph.source",
    "graph.stats",
    "history",
    "impact.report",
    "interpretation.prepare",
    "interpretation.propose",
    "learning.refresh.prepare",
    "learning.refresh.propose",
    "learning.review",
    "memories",
    "memory.delete",
    "memory.export",
    "memory.history",
    "memory.import",
    "memory.propose",
    "memory.search",
    "memory.state",
    "model.configure",
    "model.status",
    "note",
    "oauth.configure",
    "oauth.disconnect",
    "oauth.refresh",
    "oauth.resources",
    "oauth.status",
    "observer.context",
    "observer.status",
    "project.delete",
    "project.list",
    "projection.retry",
    "projection.status",
    "read",
    "register",
    "retention.apply",
    "retention.preview",
    "runtime.status",
    "session.forget",
    "session.timeline",
    "source.forget",
    "status",
    "storage.prune",
    "sync.abort",
    "sync.commit",
    "sync.prepare",
    "sync.upload",
    "vector.configure",
    "vector.publish",
    "work_state.read",
    "work_state.write",
    "worker.configure",
    "workflow.cancel",
    "workflow.enqueue",
    "workflow.retry",
    "workflow.status",
    "workspace.members",
];
pub fn command(
    app: &App,
    workspace: &str,
    capability: &str,
    operation: &str,
    args: &Value,
) -> Result<Value> {
    if !args.is_object() {
        return Err(Error::bad("Command arguments must be an object"));
    }
    if let Some(extension) = crate::extension::installed().operation(operation) {
        return extension.command(app, workspace, capability, operation, args);
    }
    let store = &app.worker.store;
    match operation {
        "build.info" => {
            store.workspace(workspace, capability)?;
            Ok(crate::core_capabilities::capabilities())
        }
        "register" => store.register(workspace, capability, args),
        "status" | "runtime.status" => store.project_status(workspace, capability, args),
        "sync.prepare" => store.sync_prepare(workspace, capability, args),
        "sync.upload" => store.sync_upload(workspace, capability, args),
        "sync.commit" => store.sync_commit(workspace, capability, args),
        "read" | "evidence.read" => {
            let mut arguments = args.clone();
            if let Some(id) = args.get("evidence_id") {
                arguments["source_id"] = id.clone();
            }
            if let Some(hash) = args.get("expected_sha256") {
                arguments["sha256"] = hash.clone();
            }
            store.read_project_source(workspace, capability, &arguments)
        }
        "context" => {
            // An unsupported option is refused before any profile it names is
            // resolved (see crate::archived).
            crate::archived::check(crate::archived::Surface::Context, args)?;
            if args.get("local_embedding_profile").is_none_or(Value::is_null)
                && args.get("local_reranker_profile").is_none_or(Value::is_null) {
                store.project_context(workspace, capability, args)
            } else {
                let configuration=crate::settings::configuration(store,workspace,&app.worker.config)?;
                let selected=|field:&str|->Result<Option<&crate::model::Profile>> {
                    if args.get(field).is_none_or(Value::is_null) { Ok(None) }
                    else { configuration.profile(Some(crate::string(args,field,100)?)).map(|(_,p)|Some(p)) }
                };
                store.project_context_local(workspace,capability,args,
                    selected("local_embedding_profile")?,selected("local_reranker_profile")?)
            }
        },
        "note" => store.note(workspace, capability, args),
        "memory.search" => store.memory_search(workspace, capability, args),
        "learning.refresh.prepare" => store.refresh_context(workspace, capability, args),
        "learning.review" => store.review_memory(workspace, capability, args),
        "claim.history" => store.claim_history(workspace, capability, args),
        "workflow.enqueue" => store.enqueue(workspace, capability, args, &app.worker.config),
        "workflow.status" => store.job_status(workspace, capability, args),
        // Change impact and execution coverage, which an installed
        // extension serves (above); without one: 501 after authorization.
        "impact.report" | "coverage.upload" | "graph.impact" => {
            crate::core_capabilities::not_included(
                store,
                workspace,
                capability,
                crate::string(args, "repository_id", 100)?,
            )
        }
        "graph.source" => store.graph_source(workspace, capability, args),
        "graph.project" => store.graph_project(workspace, capability, string(args, "repository_id", 100)?),
        "graph.episodes" => {
            store.episode_listing(workspace, capability, crate::string(args, "repository_id", 100)?)
        }
        "graph.as_of" => {
            store.graph_as_of(workspace, capability, crate::string(args, "repository_id", 100)?, args)
        }
        "graph.link" => store.graph_link(workspace, capability),
        // Documents' mention links to code, read
        // as the caller may read them now.
        "graph.doc_links" => crate::doc_links::read(store, workspace, capability, args),
        "graph.access" => store.graph_access(workspace, capability, args),
        "graph.cross_edges" => {
            store.graph_cross_edges(workspace, capability, crate::string(args, "repository_id", 100)?)
        }
        "graph.stats" => {
            store.graph_stats(workspace, capability, crate::string(args, "repository_id", 100)?)
        }
        // The engine's navigation tools on a recorded episode.
        "graph.navigate" => crate::graph_tools::call(store, workspace, capability, args),
        "retention.preview" => store.retention_preview(workspace, capability, args),
        // Captured agent sessions (connector documents): browse and forget.
        "session.timeline" => store.session_timeline(workspace, capability, args),
        "session.forget" => store.forget_session(workspace, capability, args),
        "work_state.write" => store.work_state_write(workspace, capability, args),
        "work_state.read" => store.work_state_read(workspace, capability, args),
        "workflow.cancel" => store.cancel_job(workspace, capability, args),
        "workflow.retry" => store.retry_job(workspace, capability, args, &app.worker.config),
        "observer.status" => store.authorized_read(workspace, capability, "read", |db, p| {
            let repo = crate::store::repo(db, p, crate::string(args, "repository_id", 100)?)?;
            let documents = crate::memory::documents(db, &repo, p)?;
            crate::observer::status(db, crate::store::s(&repo, "id"), &documents, args)
        }),
        "observer.context" => {
            // Optional local embedding profile for the ranked view, resolved
            // as `context` resolves `local_embedding_profile`.
            let embedding = if args.get("local_embedding_profile").is_none_or(Value::is_null) {
                None
            } else {
                let configuration = crate::settings::configuration(store, workspace, &app.worker.config)?;
                let (_, profile) = configuration.profile(Some(crate::string(args, "local_embedding_profile", 100)?))?;
                if profile.protocol != "local_onnx" {
                    return Err(crate::Error::bad("Local retrieval requires local_onnx profiles"));
                }
                Some(profile.clone())
            };
            store.authorized_read(workspace, capability, "read", |db, p| {
                let repo = crate::store::repo(db, p, crate::string(args, "repository_id", 100)?)?;
                let documents = crate::memory::documents(db, &repo, p)?;
                crate::observer::context_with(db, crate::store::s(&repo, "id"), &documents, args, embedding.as_ref())
            })
        }
        "deletion.verify" => crate::deletion::verify(store, workspace, capability, args),
        "deletion.receipts" => crate::deletion::list(store, workspace, capability),
        "deletion.state" => crate::deletion::state_api(store, workspace, capability, args),
        "connector.add" => store.connector_add(workspace, capability, args),
        "connector.list" => store.connector_list(workspace, capability, args),
        "document.versions" => store.document_versions(workspace, capability, args),
        "connector.sharing" => store.connector_sharing(workspace, capability, args),
        "connector.import" => store.import_documents(workspace, capability, args).map(|mut imported| {
            // A connector with an observe policy may now be over its threshold.
            if let Some(decision) = store
                .auto_observe(&app.worker.config, args["connector_id"].as_str())
                .ok()
                .and_then(|d| d.into_iter().next())
            {
                imported["observe"] = decision;
            }
            imported
        }),
        "connector.observe" => store.connector_observe(workspace, capability, args),
        "document.import" => store.import_binary(workspace, capability, args),
        "document.original" => store.original_document(workspace, capability, args),
        "credential.store" => store.put_credential(workspace, capability, args),
        "model.configure" => {
            crate::settings::configure(store, workspace, capability, &app.worker.config, args)
        }
        "model.status" => crate::settings::status(store, workspace, capability, &app.worker.config),
        "project.list" => store.project_list(workspace, capability, args),
        "projection.status" => crate::projection::status(store,workspace,capability),
        "projection.retry" => crate::projection::retry(store,workspace,capability,args),
        "workspace.members" => store.authorized(workspace,capability,"admin",|db,_|Ok(json!({"members":crate::store::rows(db,"SELECT id,label,role,created_at,expires_at,revoked_at FROM credentials WHERE workspace_id=? ORDER BY created_at",[workspace])?}))),
        "credential.list" => store.list_credentials(workspace, capability),
        "credential.remove" => store.remove_credential(workspace, capability, args),
        "oauth.configure" => crate::oauth::configure(store, workspace, capability, args),
        "graph.query" => crate::graph::read(store, workspace, capability, args),
        "graph.neo4j.configure" => {
            crate::graph::configure_neo4j(store, workspace, capability, args)
        }
        "worker.configure" => crate::distributed::configure(store, workspace, capability, args),
        "vector.configure" => crate::vector::configure(store, workspace, capability, args),
        "oauth.status" => crate::oauth::status(store, workspace, capability),
        "oauth.disconnect" => crate::oauth::disconnect(
            store,
            workspace,
            capability,
            string(args, "account_id", 100)?,
        ),
        "connector.schedule" => store.connector_schedule(workspace, capability, args),
        "connector.state" | "connector.delete" | "source.forget" | "project.delete"
        | "storage.prune" | "retention.apply" => store.maintain(workspace, capability, operation, args),
        "files" | "memories" | "memory.history" | "memory.export" | "history" => {
            store.project_read(workspace, capability, operation, args)
        }
        "sync.abort"
        | "memory.state"
        | "memory.delete"
        | "memory.import"
        | "memory.propose"
        | "learning.refresh.propose" => {
            store.project_mutate(workspace, capability, operation, args)
        }
        "interpretation.prepare" => store.interpretation(workspace, capability, args, false),
        "interpretation.propose" => store.interpretation(workspace, capability, args, true),
        _ => Err(Error::bad("Unsupported native project operation")),
    }
}
/// An MCP tool contract: an object input schema that admits no other
/// properties, and read-only or write annotations.
pub fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    write: bool,
) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":!write,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}
pub fn mcp_tools(learning: bool) -> Value {
    mcp_tools_with(learning, false)
}
/// The tools a project connection of a server with default options
/// publishes (its `tools/list`): the core tools, then the installed
/// extensions' tools, each as [`McpTool::confined`] gives it.
pub fn mcp_tools_with(learning: bool, work_state: bool) -> Value {
    mcp_tools_for(learning, work_state, &McpOptions::default())
}
/// The tools a project connection publishes for `options`,
/// as its `tools/list` lists them (each as [`McpTool::confined`] gives it).
pub fn mcp_tools_for(learning: bool, work_state: bool, options: &McpOptions) -> Value {
    let tools: Vec<Value> = published_tools(learning, work_state, options)
        .into_iter()
        .filter_map(McpTool::confined)
        .map(|tool| tool.spec)
        .collect();
    json!({"tools":tools})
}
/// The published tools: the core tools (the service's, then the graph
/// engine's navigation tools), the engine's pull-request tools when
/// `options` asks for them, then the installed extensions' tools; only those
/// of `options.tools`.
fn published_tools(learning: bool, work_state: bool, options: &McpOptions) -> Vec<McpTool> {
    let mut tools = core_mcp_tools(learning, work_state);
    if options.pull_requests {
        tools.extend(
            crate::graph_tools::tools(true)
                .into_iter()
                .filter(|tool| tool.operation == crate::graph_tools::PULL_REQUEST),
        );
    }
    tools.extend(crate::extension::installed().mcp_tools(learning, work_state));
    tools.retain(|tool| options.tools.publishes(tool));
    tools
}
/// The core tools: the read tools always, the write tools with `learning`,
/// and the agent's work-state tools with `work_state`, then the graph
/// engine's navigation tools under Graphify's names (one
/// server). Each names the operation `tools/call` runs and whether its
/// `repo` argument may select another repository of the workspace.
pub fn core_mcp_tools(learning: bool, work_state: bool) -> Vec<McpTool> {
    let mut tools = vec![
        McpTool::new(
            tool(
                "recall_project",
                "Find original source evidence and current reviewed memory. Optional local model profiles select explicitly configured CPU embedding and reranking; leave the reranker unset for a project holding one conversation's sessions, where it did not raise held-out recall and took about 20 s per question. Continue incomplete sources before making decisions.",
                json!({"question":{"type":"string"},"max_tokens":{"type":"integer","default":3000},"max_chars":{"type":"integer","default":12000},"mode":{"type":"string","default":"auto"},"needs":{"type":"array","items":{"type":"string"}},"local_embedding_profile":{"type":"string"},"local_reranker_profile":{"type":"string"},
                "pack_order":{"type":"string","enum":["rank","chronological"],"description":"chronological presents the selected excerpts by date, with markers where a source's text between two excerpts is not shown; selection stays by rank"},
                "reading_guidance":{"type":"boolean","description":"add guidance on weighing what users said against what assistants suggested"},
                "skip_shown":{"type":"boolean","description":"answer excerpts this connection already returned with one-line references and spend their budget on further hits"},
                "include":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"string","minLength":1,"maxLength":300},"description":"search only sources whose id starts with one of these prefixes: file: for the project's files, file:docs/ for a folder, connector:<id>: for one connector's documents"},
                "shown":{"type":"array","maxItems":200,"description":"source spans already in your context; covered hits return as references","items":{"type":"object","properties":{"id":{"type":"string"},"sha256":{"type":"string"},"byte_start":{"type":"integer","minimum":0},"byte_end":{"type":"integer","minimum":1}},"required":["id","byte_start","byte_end"],"additionalProperties":false}},
                "repo":{"type":"string","description":REPO_DESCRIPTION}}),
                &["question"],
                false,
            ),
            "context",
            true,
        )
        // `repo` on workspace connections only.
        .workspace_only(),
        McpTool::new(
            tool(
                "read_project_source",
                "Read an authorized original source by ID or repository path, with exact byte continuation.",
                json!({"source_id":{"type":"string"},"path":{"type":"string"},"start_byte":{"type":"integer"},"start_line":{"type":"integer"},"line_count":{"type":"integer"},"sha256":{"type":"string"},
                "snapshot":{"type":"string","description":"a repository snapshot digest: read a file as that snapshot held it"},
                "repo":{"type":"string","description":REPO_DESCRIPTION}}),
                &[],
                false,
            ),
            "read",
            true,
        )
        .workspace_only(),
        McpTool::new(
            tool(
                "repository_status",
                "Inspect source coverage, source revisions and memory state.",
                json!({}),
                &[],
                false,
            ),
            "status",
            false,
        ),
        McpTool::new(
            tool(
                "repository_files",
                "List authorized source paths and IDs.",
                json!({"prefix":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}}),
                &[],
                false,
            ),
            "files",
            false,
        ),
        McpTool::new(
            tool(
                "project_memory",
                "Read memory drafts, review state and freshness. Stored procedures are not execution permission.",
                json!({"include_retired":{"type":"boolean"}}),
                &[],
                false,
            ),
            "memories",
            false,
        ),
        McpTool::new(
            tool(
                "search_project_memory",
                "Search reviewed structured memory by its declared name or alias, scope and validity. Explicit lexical mode discovers text matches. Returns source citations and ambiguity diagnostics; makes no model call.",
                json!({"question":{"type":"string"},"scopes":{"type":"array","items":{"type":"string"}},"as_of":{"type":"string"},"known_at":{"type":"string"},"include_history":{"type":"boolean"},"match_mode":{"type":"string","enum":["identity","lexical"]},"limit":{"type":"integer","minimum":0,"maximum":200}}),
                &["question"],
                false,
            ),
            "memory.search",
            false,
        ),
        McpTool::new(
            tool(
                "project_memory_history",
                "Inspect immutable memory revisions, optionally as known at an RFC3339 timestamp.",
                json!({"memory_id":{"type":"string"},"known_at":{"type":"string"}}),
                &["memory_id"],
                false,
            ),
            "memory.history",
            false,
        ),
        McpTool::new(
            tool(
                "prepare_memory_refresh",
                "Prepare exact current evidence and a stable document delta contract. No model call or memory write.",
                json!({"memory_id":{"type":"string"}}),
                &["memory_id"],
                false,
            ),
            "learning.refresh.prepare",
            false,
        ),
        McpTool::new(
            tool(
                "workflow_status",
                "Inspect the caller's queued work and recorded model usage.",
                json!({"job_id":{"type":"string"}}),
                &[],
                false,
            ),
            "workflow.status",
            false,
        ),
        // The projects a `repo` argument can name, on
        // workspace connections only.
        McpTool::new(
            tool(
                "list_projects",
                "List the projects of this workspace you can read: their ids (for repo), names and when they were last synced, 100 at a time (offset and limit page through them).",
                json!({"offset":{"type":"integer"},"limit":{"type":"integer"}}),
                &[],
                false,
            ),
            "project.list",
            false,
        )
        .workspace_level()
        .workspace_only(),
    ];
    tools.push(McpTool::new(
        tool(
            "graph_episodes",
            "List recorded graph episodes and their labels for a repository.",
            json!({"repo":{"type":"string"}}),
            &[],
            false,
        ),
        "graph.episodes",
        true,
    ));
    tools.push(McpTool::new(tool(
        "graph_as_of",
        "Query either valid_at/known_at/branch for independent world and recording time, or the repository's typed code graph at an episode selector (`as_of`: episode id, commit sha label, `latest` or `working`), reconstructed from recorded history without re-extraction. Narrow with `node` (exact id), `query` (id substring) or `relations`; includes parent episode, labels and cardinality-one contradictions recorded at that episode.",
        json!({"repo":{"type":"string"},"as_of":{"type":"string"},"valid_at":{"type":"string"},"known_at":{"type":"string"},"branch":{"type":"string"},"canonical":{"type":"boolean"},"node":{"type":"string"},"query":{"type":"string"},"relations":{"type":"array","items":{"type":"string"}},"limit":{"type":"integer","minimum":1,"maximum":2000}}),
        &[],
        false,
    ), "graph.as_of", true));
    tools.push(McpTool::new(tool(
        "graph_link",
        "Resolve imports between the independently indexed repositories of this workspace (Python, TypeScript/JavaScript, Go) by package identity and persist the cross-repository edges with both episodes; re-running after new episodes closes stale edges. Then recompute and persist the links from documents to the code symbols they name. Only repositories the caller may read take part.",
        json!({}),
        &[],
        // It persists edges, so it is not read-only.
        true,
    ), "graph.link", false).workspace_level());
    // The service's own statistics tool; the engine's `graph_stats` keeps
    // Graphify's name in the same list.
    tools.push(McpTool::new(tool(
        "graph_history_stats",
        "Storage and timing summary for a repository's graph history: row counts and content bytes per table (nodes, edges, edge_sources, coverage, cross_edges, unresolved_calls), episode count, recent coverage uploads with their format and granularity, the latest change-impact report's wall-clock time, and the retention defaults in force.",
        json!({"repo":{"type":"string"}}),
        &[],
        false,
    ), "graph.stats", true));
    tools.push(McpTool::new(tool("prepare_observations","Prepare current source excerpts, existing claim targets and the interpretation contract. Use your own model; no provider call.",json!({"source_id":{"type":"string"},"start_byte":{"type":"integer"},"limit":{"type":"integer"}}),&[],false), "interpretation.prepare", false));
    tools.push(McpTool::new(tool(
        "session_timeline",
        "Browse captured coding-agent turns (Claude Code, Codex) of this project: the latest session by default, or the session of an anchor turn, with turns before and after it and the recent sessions. Returns titles, times and source ids; read_project_source returns a whole turn.",
        json!({"session_id":{"type":"string"},"anchor_source_id":{"type":"string"},"before":{"type":"integer","minimum":0,"maximum":20},"after":{"type":"integer","minimum":0,"maximum":20}}),
        &[],
        false,
    ), "session.timeline", false));
    if learning {
        tools.push(McpTool::new(tool("propose_observations","Validate source citations and save proposed facts, corrections or retractions as drafts. Separate review is required.",json!({"source_id":{"type":"string"},"start_byte":{"type":"integer"},"limit":{"type":"integer"},"context_sha256":{"type":"string"},"claims":graphicle_memory::semantic::schema()}),&["context_sha256","claims"],true), "interpretation.propose", false));
        tools.push(McpTool::new(tool("propose_memory_refresh","Save a source-checked draft. A separate maintainer reviews it. Cannot approve or execute.",json!({"memory_id":{"type":"string"},"context_sha256":{"type":"string"},"operations":crate::native("document.contract",json!({})).unwrap_or(json!({})),"evidence":crate::workflows::evidence_schema()}),&["memory_id","context_sha256","operations","evidence"],true), "learning.refresh.propose", false));
        tools.push(McpTool::new(tool(
            "propose_memory",
            "Propose a decision, preference, procedure, task state or note as a draft memory. Every evidence quote must appear verbatim in a current source (for example a captured turn or a file). A quote that lies inside one code symbol anchors the memory to that symbol: an edit elsewhere in the file leaves the memory current, an edit inside the symbol withholds it. symbols names code symbols the memory is about; files are whole-file dependencies (any change to them withholds the memory). A member reviews the draft; it is never served as reviewed memory before that. Reusing the idempotency key with the same proposal replays it.",
            json!({"kind":{"type":"string","enum":["decision","preference","procedure","task_state","note"]},"title":{"type":"string","maxLength":500},"body":{"type":"string","maxLength":64000},"reason":{"type":"string","maxLength":2000},"files":{"type":"array","maxItems":50,"items":{"type":"string"},"description":"project files the memory depends on as a whole"},"symbols":{"type":"array","maxItems":50,"description":"code symbols the memory is about: a stable_id from the code graph, or a path and qualified_name (with kind or ordinal when several symbols share them)","items":{"type":"object","properties":{"stable_id":{"type":"string"},"path":{"type":"string"},"qualified_name":{"type":"string"},"kind":{"type":"string"},"ordinal":{"type":"integer","minimum":0}},"additionalProperties":false}},"evidence":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"object","properties":{"source_id":{"type":"string"},"quote":{"type":"string","minLength":8}},"required":["source_id","quote"],"additionalProperties":false}},"idempotency_key":{"type":"string","maxLength":200},"recall":{"type":"object","properties":{"name":{"type":"string"},"aliases":{"type":"array","items":{"type":"string"},"maxItems":32},"scopes":{"type":"array","items":{"type":"string"},"maxItems":32},"state":{"type":"string","enum":["active","historical","resolved","open_question"]},"valid_from":{"type":["string","null"]},"valid_to":{"type":["string","null"]},"superseded_by":{"type":["string","null"]}},"required":["name"],"additionalProperties":false}}),
            &["kind", "title", "body", "evidence", "idempotency_key"],
            true,
        ), "memory.propose", false));
    }
    if work_state {
        let mut write = tool(
            "work_state_write",
            "Record progress on a to-do list or other tracked work in this project. Name the list and only the fields that change: with a \"task\" field the write updates that item (\"status\": \"todo\", \"doing\", \"done\" or \"dropped\", plus any details), otherwise the list's own state. The latest value of each key wins and null clears it; \"done\" or \"dropped\" closes the item or the list. Open items are shown at the start of later sessions. String values are stored with private spans dropped and credentials redacted.",
            json!({"list":{"type":"string","minLength":1,"maxLength":crate::work_state::MAX_LIST_CHARS},
                "fields":{"type":"object","minProperties":1,"additionalProperties":{"type":["string","number","boolean","null"]}}}),
            &["list", "fields"],
            true,
        );
        // Each write appends an entry; repeating one refreshes its time.
        write["annotations"]["idempotentHint"] = json!(false);
        tools.push(McpTool::new(write, "work_state.write", false));
        tools.push(McpTool::new(tool(
            "work_state_read",
            "This project's work-state lists, the most recently written first: open items only unless include_closed. Name a list to read only that one.",
            json!({"list":{"type":"string","maxLength":crate::work_state::MAX_LIST_CHARS},"include_closed":{"type":"boolean","default":false}}),
            &[],
            false,
        ), "work_state.read", false));
    }
    tools.extend(crate::graph_tools::tools(false));
    tools
}
/// The switches `recall_project` accepts beside its published contract
///: each turns one retrieval
/// mechanism off or on for the call, and the packet's `mechanisms` reports
/// it. `doc_links: true` follows documents'
/// mention links to code (left out, they are counted, not followed;
/// `false` reads none). A server can fix each (`--set`).
pub const RECALL_SWITCHES: [&str; 3] = ["graph_retrieval", "memory_retrieval", "doc_links"];
/// `recall_project`'s scope switch, accepted beside the
/// published contract like [`RECALL_SWITCHES`]: `project`
/// or `workspace`. A workspace connection recalls over the workspace unless
/// the call names a project with `repo`; a project connection reads its own
/// project and refuses `workspace`.
pub const RECALL_SCOPE: &str = "scope";
/// The `repo` argument of `recall_project` and `read_project_source`, which
/// workspace connections publish.
const REPO_DESCRIPTION: &str = "The project to read, by id or key (list_projects lists the ids). Without it, recall_project searches every project of the workspace you can read, and read_project_source needs it.";
/// What one MCP connection has been given, for `recall_project`'s opt-in
/// `skip_shown` (Mastra's navigable recall, a86dc4240): the source spans of
/// its recent recall packets, newest last, at most 200.
#[derive(Default)]
pub struct McpSession {
    shown: std::collections::VecDeque<Value>,
}
impl McpSession {
    const LIMIT: usize = 200;
    fn record(&mut self, packet: &Value) {
        for source in packet["sources"].as_array().into_iter().flatten() {
            // Each span names its project: source ids such as
            // `file:README.md` repeat across the projects of a workspace.
            let project = source
                .get("repository_id")
                .filter(|id| id.is_string())
                .unwrap_or(&packet["repository_id"]);
            let span = json!({"id":source["id"],"sha256":source["sha256"],
                "byte_start":source["byte_start"],"byte_end":source["byte_end"],"repository_id":project});
            if span["id"].is_string() && span["byte_start"].is_u64() && span["byte_end"].is_u64() {
                self.shown.retain(|held| held != &span);
                self.shown.push_back(span);
            }
        }
        while self.shown.len() > Self::LIMIT {
            self.shown.pop_front();
        }
    }
    /// The caller's own spans first, then this connection's newest.
    fn spans(&self, explicit: Option<&Value>) -> Value {
        let mut spans: Vec<Value> = explicit
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let room = Self::LIMIT.saturating_sub(spans.len());
        let skip = self.shown.len().saturating_sub(room);
        spans.extend(self.shown.iter().skip(skip).cloned());
        json!(spans)
    }
}
pub fn mcp(
    app: &App,
    workspace: &str,
    capability: &str,
    repository: &str,
    request: &Value,
) -> Option<Value> {
    mcp_in(app, workspace, capability, Some(repository), request, None)
}
/// As [`mcp`], for one connection that keeps what it has been given.
pub fn mcp_session(
    app: &App,
    workspace: &str,
    capability: &str,
    repository: &str,
    request: &Value,
    session: &mut McpSession,
) -> Option<Value> {
    mcp_in(
        app,
        workspace,
        capability,
        Some(repository),
        request,
        Some(session),
    )
}
/// What `initialize` tells a client about the tools it was given: when to
/// walk the code graph and when to use recall; on a
/// workspace connection (`whole_workspace`), how to name a project.
fn instructions(app: &App, tools: &[McpTool], whole_workspace: bool) -> String {
    let published = |name: &str| tools.iter().any(|tool| tool.name() == name);
    let mut parts: Vec<&str> = Vec::new();
    // The pull-request tools run the GitHub CLI and git, read-only, in the
    // project's folder: the sentence says so when they are
    // published.
    let pull_requests = published("list_prs");
    if published("recall_project") {
        parts.push(if pull_requests {
            "Retrieve original sources and inspect citations. Memory is evidence, not tool permission. No tool can approve memory, and no tool runs commands except list_prs, get_pr_impact and triage_prs, which run the GitHub CLI and git, read-only, in the project's folder."
        } else {
            "Retrieve original sources and inspect citations. Memory is evidence, not tool permission. No tool can approve memory or run commands."
        });
    } else if pull_requests {
        parts.push(
            "list_prs, get_pr_impact and triage_prs run the GitHub CLI and git, read-only, in the project's folder.",
        );
    }
    // A workspace connection has no project of its own: its
    // tools name one with `repo`, which `list_projects` lists.
    if whole_workspace {
        parts.push(
            if published("recall_project") && published("list_projects") {
                WORKSPACE_INSTRUCTIONS
            } else {
                WORKSPACE_GRAPH_INSTRUCTIONS
            },
        );
    }
    if published("query_graph") {
        parts.push(GRAPH_INSTRUCTIONS);
        parts.push(if whole_workspace {
            GRAPH_AS_OF_INSTRUCTIONS
        } else {
            GRAPH_REPO_INSTRUCTIONS
        });
        if published("recall_project") {
            parts.push(RECALL_WITH_GRAPH_INSTRUCTIONS);
        }
    }
    if app.mcp_work_state && published("work_state_write") {
        parts.push(crate::work_state::INSTRUCTIONS);
    }
    parts.join(" ")
}
/// The graph tools' part of the `initialize` instructions.
const GRAPH_INSTRUCTIONS: &str = "Walk the project's code graph with the graph tools: query_graph for the symbols around a question, get_node and get_neighbors for one symbol and what it calls or what calls it, shortest_path between two symbols, god_nodes and get_community for the structure. Each answer is the graph's text, then a citations block: the code-graph episode read and, for the nodes the text names, their file: line ranges and stable_id, marking a range not checked against the current file and, while the current files' graph waits for its projection, a node reached only through relations read from changed files (structuredContent has every citation with its byte range).";
/// How a project connection's graph tools name what they read.
const GRAPH_REPO_INSTRUCTIONS: &str =
    "Name another project with repo, or an earlier code graph with as_of.";
/// The same on a workspace connection, whose tools all name their project.
const GRAPH_AS_OF_INSTRUCTIONS: &str = "Name an earlier code graph with as_of.";
/// A workspace connection's sentence.
const WORKSPACE_INSTRUCTIONS: &str = "This connection serves every project of the workspace you can read: list_projects names them, recall_project searches them all unless repo names one, and the other tools need repo.";
/// The same when the connection publishes the code-graph half only
/// (`--tools graph`), without `list_projects` and `recall_project`.
const WORKSPACE_GRAPH_INSTRUCTIONS: &str = "This connection serves every project of the workspace you can read: name one with repo (a project id or key).";
/// How recall and the graph tools fit together, when both are published.
const RECALL_WITH_GRAPH_INSTRUCTIONS: &str = "Use recall_project for what the code, the documents and reviewed memory say together about a question: it cites the same stable_id for code. read_project_source reads any cited window exactly.";
/// A workspace connection (`POST /mcp/{workspace}`,
/// `graphicle mcp --workspace`): no project of its own. It publishes the
/// tools that take `repo` and the workspace-level tools; `recall_project`
/// searches every project the caller may read unless `repo` names one, and
/// the other `repo` tools need it.
pub fn mcp_workspace(
    app: &App,
    workspace: &str,
    capability: &str,
    request: &Value,
) -> Option<Value> {
    mcp_in(app, workspace, capability, None, request, None)
}
/// As [`mcp_workspace`], for one connection that keeps what it has been
/// given.
pub fn mcp_workspace_session(
    app: &App,
    workspace: &str,
    capability: &str,
    request: &Value,
    session: &mut McpSession,
) -> Option<Value> {
    mcp_in(app, workspace, capability, None, request, Some(session))
}
/// The refusal of a project connection's call that reaches beyond its
/// project.
fn confined(what: &str) -> Error {
    Error::bad(format!(
        "{what} needs a workspace connection (graphicle mcp --workspace, or POST /mcp/WORKSPACE_ID): this connection reads its own project"
    ))
}
/// The tools a connection publishes, of those the server's options give
/// (`--tools`, `--pull-requests`). A project connection
/// publishes each tool as [`McpTool::confined`] gives it, so it reads its own
/// project only: no `repo` on `recall_project` and `read_project_source`, no
/// `list_projects`. A workspace connection (`repository` is `None`) publishes
/// the tools that take `repo` (the graph engine's navigation tools among
/// them) and the workspace-level ones.
fn connection_tools(app: &App, repository: Option<&str>) -> Vec<McpTool> {
    let tools = published_tools(app.mcp_learning, app.mcp_work_state, &app.mcp);
    match repository {
        Some(_) => tools.into_iter().filter_map(McpTool::confined).collect(),
        None => tools
            .into_iter()
            .filter(|tool| tool.repo_arg || tool.workspace)
            .collect(),
    }
}
fn mcp_in(
    app: &App,
    workspace: &str,
    capability: &str,
    repository: Option<&str>,
    request: &Value,
    mut session: Option<&mut McpSession>,
) -> Option<Value> {
    if request["jsonrpc"] != "2.0" {
        return Some(
            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32600,"message":"Invalid JSON-RPC request"}}),
        );
    }
    request.get("id")?;
    let result: Result<Value> = (|| {
        match repository {
            Some(repository) => {
                app.worker
                    .store
                    .authorized(workspace, capability, "read", |db, p| {
                        repo(db, p, repository)?;
                        Ok(())
                    })?
            }
            None => {
                app.worker.store.workspace(workspace, capability)?;
            }
        }
        let tools = connection_tools(app, repository);
        // The engine's resources come with its tools, on
        // a project connection: they describe one project's code graph, and
        // a workspace connection has no project of its own.
        let resources = repository.filter(|_| tools.iter().any(crate::graph_tools::is_graph_tool));
        match s(request, "method") {
            "initialize" => {
                let capabilities = if resources.is_some() {
                    json!({"tools":{},"resources":{}})
                } else {
                    json!({"tools":{}})
                };
                // The work-state tools are a project's: a workspace
                // connection publishes neither them nor their sentence.
                let text = instructions(app, &tools, repository.is_none());
                Ok(
                    json!({"protocolVersion":request["params"]["protocolVersion"].as_str().filter(|v|["2024-11-05","2025-03-26","2025-06-18","2025-11-25"].contains(v)).unwrap_or("2025-11-25"),"capabilities":capabilities,"serverInfo":{"name":"Graphicle","version":env!("CARGO_PKG_VERSION")},"instructions":text}),
                )
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                Ok(json!({"tools":tools.into_iter().map(|tool| tool.spec).collect::<Vec<_>>()}))
            }
            "resources/list" if resources.is_some() => {
                Ok(json!({"resources":crate::graph_tools::resources()}))
            }
            "resources/templates/list" if resources.is_some() => {
                Ok(json!({"resourceTemplates":[]}))
            }
            "resources/read" if resources.is_some() => {
                let uri = string(&request["params"], "uri", 200)?;
                let text = crate::graph_tools::read_resource(
                    &app.worker.store,
                    workspace,
                    capability,
                    resources.unwrap_or_default(),
                    uri,
                )?;
                let kind = if uri.starts_with("graphicle-graph://report") {
                    "text/markdown"
                } else {
                    "text/plain"
                };
                Ok(json!({"contents":[{"uri":uri,"mimeType":kind,"text":text}]}))
            }
            "tools/call" => {
                let name = string(&request["params"], "name", 100)?;
                let tool = tools
                    .iter()
                    .find(|tool| tool.name() == name)
                    .ok_or_else(|| Error::bad("Tool unavailable"))?;
                let operation = tool.operation;
                let graph_tool = crate::graph_tools::is_graph_tool(tool);
                let mut args = request["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                if !args.is_object() {
                    return Err(Error::bad("Tool arguments must be an object"));
                }
                if args.get("repository_id").is_some() || args.get("workspace_id").is_some() {
                    return Err(Error::denied());
                }
                // The engine's own server reads any graph file a path names;
                // this one reads the projects of its store.
                if graph_tool && args.get("project_path").is_some() {
                    return Err(Error::bad(
                        "This server reads the projects of its Graphicle store: name another project with repo (its id or key, or with graphicle mcp a synced folder), not project_path",
                    ));
                }
                // An unsupported `context` field is refused with its unit,
                // or left out at an inert value, before the published
                // contract (which no longer lists it) is checked.
                if operation == "context" {
                    crate::archived::check(crate::archived::Surface::Context, &args)?;
                    args = crate::archived::without_inert(crate::archived::Surface::Context, &args);
                }
                // `recall_project` honours the `graph_retrieval`,
                // `memory_retrieval` and `doc_links` switches, which its
                // published contract leaves out, to keep the ablation
                // switches out of the schemas agents read. They are taken out
                // before the contract is checked and passed on. A switch the
                // server fixes (`--set`) takes its fixed value, and a request
                // for another value is refused.
                let mut switches = Vec::new();
                if name == "recall_project" {
                    for field in RECALL_SWITCHES {
                        let asked = args.as_object_mut().and_then(|a| a.remove(field));
                        if asked.as_ref().is_some_and(|value| !value.is_boolean()) {
                            return Err(Error::bad(format!("{field} must be true or false")));
                        }
                        match (app.mcp.fixed.get(field), asked) {
                            (Some(fixed), Some(asked)) if *fixed != asked => {
                                return Err(Error::bad(format!(
                                    "{field} is fixed to {fixed} by this server"
                                )));
                            }
                            (Some(fixed), _) => switches.push((field, fixed.clone())),
                            (None, Some(asked)) => switches.push((field, asked)),
                            (None, None) => {}
                        }
                    }
                    // The scope switch likewise. A project
                    // connection reads its own project only: the workspace
                    // takes a workspace connection.
                    if let Some(value) = args.as_object_mut().and_then(|a| a.remove(RECALL_SCOPE)) {
                        match value.as_str() {
                            Some("project") => {}
                            Some("workspace") if repository.is_none() => {}
                            Some("workspace") => return Err(confined("scope workspace")),
                            _ => return Err(Error::bad("scope must be project or workspace")),
                        }
                        switches.push((RECALL_SCOPE, value));
                    }
                }
                // Likewise `repo`, which a project connection does not
                // publish on these tools: say why rather than only that the
                // contract was not met.
                if repository.is_some() && tool.workspace_only && args.get("repo").is_some() {
                    return Err(confined("repo"));
                }
                let validator = jsonschema::validator_for(&tool.spec["inputSchema"])
                    .map_err(|_| Error::bad("Invalid tool contract"))?;
                if !validator.is_valid(&args) {
                    return Err(Error::bad(
                        "Tool arguments do not match the published contract",
                    ));
                }
                for (field, value) in switches {
                    args[field] = value;
                }
                if name == "recall_project"
                    && args
                        .as_object_mut()
                        .and_then(|a| a.remove("skip_shown"))
                        .is_some_and(|v| v == true)
                    && let Some(session) = session.as_deref()
                {
                    args["shown"] = session.spans(args.get("shown"));
                }
                // Tools with a `repo` argument may name another project of the
                // same workspace (resolved below); `repo()` re-authorizes
                // it under the calling principal, as for every project read
                // (recall and source reads too, on workspace
                // connections).
                let named = args["repo"]
                    .as_str()
                    .is_some_and(|r| tool.repo_arg && !r.is_empty());
                match (named, repository) {
                    (true, _) => {}
                    (false, Some(repository)) => args["repository_id"] = json!(repository),
                    // A workspace connection: recall searches the workspace
                    // unless the call asks for one project, which it must name.
                    (false, None) if name == "recall_project" => {
                        if args[RECALL_SCOPE] == "project" {
                            return Err(Error::bad(
                                "Name the project with repo: this connection serves the whole workspace (list_projects lists its projects)",
                            ));
                        }
                        args[RECALL_SCOPE] = json!("workspace");
                    }
                    (false, None) if tool.workspace => {}
                    (false, None) => {
                        return Err(Error::bad(
                            "Name the project with repo: this connection serves the whole workspace (list_projects lists its projects)",
                        ));
                    }
                }
                if graph_tool && operation == crate::graph_tools::NAVIGATE {
                    args["tool"] = json!(name);
                }
                let result = (|| {
                    // Graph tools may name another project of the workspace by
                    // its id or key, and over stdio by its folder; it is
                    // authorized again for the caller.
                    if tool.repo_arg
                        && let Some(named) = args["repo"].as_str().filter(|r| !r.is_empty())
                    {
                        let named = app.worker.store.authorized_read(
                            workspace,
                            capability,
                            "read",
                            |db, p| crate::folders::named(db, p, named, app.mcp.cwd.as_deref()),
                        )?;
                        args["repository_id"] = json!(named);
                    }
                    if operation == crate::graph_tools::PULL_REQUEST {
                        let repository = repository.ok_or_else(|| {
                            Error::bad("The pull-request tools serve a project connection")
                        })?;
                        crate::graph_tools::pull_request(
                            &app.worker.store,
                            workspace,
                            capability,
                            repository,
                            name,
                            &args,
                        )
                    } else {
                        command(app, workspace, capability, operation, &args)
                    }
                })();
                let result = result.map(|mut packet| {
                    if name == "recall_project"
                        && !app.mcp.fixed.is_empty()
                        && packet["mechanisms"].is_object()
                    {
                        packet["mechanisms"]["server_fixed"] = Value::Object(app.mcp.fixed.clone());
                    }
                    packet
                });
                if name == "recall_project"
                    && let (Ok(packet), Some(session)) = (&result, session.as_deref_mut())
                {
                    session.record(packet);
                }
                Ok(match result {
                    // The engine's text, as its own server answers, then (for
                    // the navigation tools) a short citations block, which
                    // clients that show the model `content` only also pass on;
                    // `structuredContent` has the episode and the
                    // nodes the text names.
                    Ok(value) if graph_tool => {
                        let mut content = vec![json!({"type":"text","text":value["text"]})];
                        if value["nodes"].is_array() {
                            content.push(json!({"type":"text","text":crate::graph_tools::citations_text(&value)}));
                        }
                        json!({"content":content,"structuredContent":value,"isError":false})
                    }
                    Ok(value) => {
                        json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":if value.is_object(){value}else{json!({"items":value})},"isError":false})
                    }
                    Err(error) => {
                        json!({"content":[{"type":"text","text":error.message}],"isError":true})
                    }
                })
            }
            _ => Err(Error::bad("Unsupported MCP method")),
        }
    })();
    Some(match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":request["id"],"result":value}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32602,"message":error.message}})
        }
    })
}
async fn mcp_http(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Response> {
    let token = token(&headers)?.to_owned();
    let reply = tokio::task::spawn_blocking(move || {
        // Keep transport-level authentication (including notifications) in
        // the blocking job too: it may wait on a SQLite transaction.
        app.worker.store.workspace(&workspace, &token)?;
        // The easy flow's freshness before every tool call, as in `graphicle mcp`.
        let fresh = (request["method"] == "tools/call")
            .then(|| mcp_freshness(&app, &workspace, &token, &repository))
            .flatten();
        let mut reply = mcp(&app, &workspace, &token, &repository, &request);
        if let (Some(fresh), Some(reply)) = (&fresh, reply.as_mut()) {
            fresh.attach_mcp(reply);
        }
        Ok::<_, Error>(reply)
    })
    .await
    .map_err(|_| Error::bad("MCP request interrupted"))??;
    Ok(match reply {
        Some(value) => Json(value).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    })
}
/// `POST /mcp/{workspace}`: a workspace connection ([`mcp_workspace`]).
async fn mcp_http_workspace(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Response> {
    let token = token(&headers)?.to_owned();
    let reply = tokio::task::spawn_blocking(move || {
        app.worker.store.workspace(&workspace, &token)?;
        Ok::<_, Error>(mcp_workspace(&app, &workspace, &token, &request))
    })
    .await
    .map_err(|_| Error::bad("MCP request interrupted"))??;
    Ok(match reply {
        Some(value) => Json(value).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    })
}

/// The freshness of an HTTP MCP tool call: what `graphicle mcp` does before
/// each one ([`crate::fresh::ensure`], with its limits: a few changed files
/// synced within its time budget, else a stale note). The sync runs as this
/// data folder's local session, the owner folder syncs on this computer run
/// as, as with `graphicle mcp`; so only a caller in that owner's workspace
/// who may read the project gets it. `None` for any other call.
fn mcp_freshness(
    app: &App,
    workspace: &str,
    token: &str,
    repository: &str,
) -> Option<crate::fresh::Outcome> {
    let store = &app.worker.store;
    let owner = crate::local::session(store, false)
        .ok()
        .filter(|owner| owner.workspace == workspace)?;
    store
        .authorized_read(workspace, token, "read", |db, p| {
            repo(db, p, repository).map(|_| ())
        })
        .ok()?;
    Some(crate::fresh::ensure(
        store,
        &owner,
        repository,
        crate::fresh::Options::answer(&app.worker.config),
    ))
}

#[cfg(test)]
mod operation_table_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The operation names of the match arms of `command_async` and `command`.
    fn match_arm_names() -> BTreeSet<String> {
        let source = include_str!("api.rs");
        let body = |start: &str, end: &str| {
            let from = source.find(start).expect(start);
            let to = from + source[from..].find(end).expect(end);
            &source[from..to]
        };
        // A quoted name before `=>` or `|`, or after `|`.
        let arm =
            regex::Regex::new(r#""([a-z_][a-z0-9_.]*)"\s*(?:=>|\|)|\|\s*"([a-z_][a-z0-9_.]*)""#)
                .unwrap();
        let mut names = BTreeSet::new();
        for text in [
            body("pub async fn command_async(", "\nasync fn oauth_start("),
            body("pub fn command(", "\npub fn tool("),
        ] {
            for found in arm.captures_iter(text) {
                let name = found.get(1).or_else(|| found.get(2)).unwrap();
                names.insert(name.as_str().to_string());
            }
        }
        names
    }

    #[test]
    fn core_operations_list_every_match_arm() {
        let listed: BTreeSet<String> = CORE_OPERATIONS.iter().map(|s| s.to_string()).collect();
        assert_eq!(listed.len(), CORE_OPERATIONS.len(), "no duplicates");
        assert_eq!(listed, match_arm_names());
    }

    #[tokio::test]
    async fn every_listed_core_operation_is_dispatched() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let created = store.create_workspace("Operations").unwrap();
        let (workspace, owner) = (s(&created, "id"), s(&created, "capability"));
        let app = App::new(store.clone(), model::Config::default(), vec![]).unwrap();
        let unsupported = "Unsupported native project operation";
        for operation in CORE_OPERATIONS {
            if let Err(error) = command_async(&app, workspace, owner, operation, &json!({})).await {
                assert_ne!(error.message, unsupported, "{operation}");
            }
        }
        let error = command_async(&app, workspace, owner, "zz.unknown", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(error.message, unsupported);
    }
}
