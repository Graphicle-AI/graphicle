//! Browser workspace, versioned HTTP entry points, and an atomic ingress budget.
//! Model-call budgets remain the responsibility of the workflow engine.
use crate::{
    Error, Result,
    api::{self, App},
    store::{Store, one, repo, rows, s},
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode, header::HeaderValue},
    middleware::Next,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{Datelike, Utc};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Instant;

const BODY_LIMIT: usize = 24 * 1024 * 1024;
const MAX_BUDGET: u64 = 1_000_000_000;

pub fn routes() -> Router<App> {
    Router::new()
        .route(
            "/workspace.js",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    include_str!("../web/workspace.js"),
                )
            }),
        )
        .route(
            "/workspace.css",
            get(|| async {
                (
                    [("content-type", "text/css; charset=utf-8")],
                    include_str!("../web/workspace.css"),
                )
            }),
        )
        .route(
            "/product",
            // An installed edition may serve its own product page.
            get(|| async {
                Html(
                    crate::extension::installed()
                        .page("/product")
                        .unwrap_or(include_str!("../web/product.html")),
                )
            }),
        )
        .route(
            "/docs",
            get(|| async { Html(include_str!("../web/docs.html")) }),
        )
        .route("/v1/openapi.json", get(|| async { Json(openapi()) }))
        .route(
            "/v1/workspaces/{workspace}/repos/{repository}/commands",
            post(commands),
        )
        .route(
            "/v1/workspaces/{workspace}/repos/{repository}/overview",
            get(overview),
        )
        .route("/v1/workspaces/{workspace}/usage", get(usage))
        .route("/v1/workspaces/{workspace}/budget", post(budget))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    operation: String,
    #[serde(default = "empty")]
    arguments: Value,
}
fn empty() -> Value {
    json!({})
}

async fn commands(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
    Json(mut body): Json<Command>,
) -> Result<Json<Value>> {
    let capability = api::token(&headers)?.to_owned();
    if !body.arguments.is_object() {
        return Err(Error::bad("arguments must be an object"));
    }
    // The URL binds the project; a body cannot silently change its authority.
    if body.arguments.get("repository_id").is_some() || body.arguments.get("workspace_id").is_some()
    {
        return Err(Error::bad("Project and workspace are selected by the URL"));
    }
    // Workspace administration stays on its existing, separately authorized routes.
    if body.operation == "register" {
        return Err(Error::bad(
            "Register projects through the workspace endpoint",
        ));
    }
    body.arguments["repository_id"] = json!(repository);
    // Validate the binding even for commands whose implementations are workspace-wide.
    let store = app.worker.store.clone();
    let w = workspace.clone();
    let c = capability.clone();
    blocking(move || {
        store.authorized_read(&w, &c, "read", |db, p| repo(db, p, &repository).map(|_| ()))
    })
    .await?;
    Ok(Json(
        api::command_async(
            &app,
            &workspace,
            &capability,
            &body.operation,
            &body.arguments,
        )
        .await?,
    ))
}

pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Error::bad("Workspace request interrupted"))?
}

async fn overview(
    State(app): State<App>,
    Path((workspace, repository)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let cap = api::token(&headers)?.to_owned();
    Ok(Json(
        blocking(move || {
            app.worker
                .store
                .product_overview(&workspace, &cap, &repository)
        })
        .await?,
    ))
}
async fn usage(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let cap = api::token(&headers)?.to_owned();
    Ok(Json(
        blocking(move || app.worker.store.product_usage(&workspace, &cap)).await?,
    ))
}
async fn budget(
    State(app): State<App>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>> {
    let cap = api::token(&headers)?.to_owned();
    Ok(Json(
        blocking(move || app.worker.store.product_budget(&workspace, &cap, &args)).await?,
    ))
}

fn month() -> String {
    Utc::now().format("%Y-%m").to_string()
}
fn day() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

fn limits(
    db: &rusqlite::Connection,
    workspace: &str,
) -> Result<(Option<u64>, Option<u64>, Option<u64>)> {
    let budget = one(
        db,
        "SELECT monthly_request_budget AS value FROM am_product_policy WHERE workspace_id=?",
        [workspace],
    )?
    .and_then(|r| r["value"].as_u64());
    // An installed extension may set an allowance (a plan, for example);
    // the lowest of them and the owner's own budget applies.
    let allocation = crate::extension::installed().request_allowance(db, workspace)?;
    Ok((
        budget,
        allocation,
        match (budget, allocation) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        },
    ))
}

impl Store {
    /// Read the retained original cited by a graph episode, never today's same-named file.
    pub fn graph_source(&self, workspace: &str, cap: &str, args: &Value) -> Result<Value> {
        self.authorized_read(workspace, cap, "read", |db, p| {
            let repository = repo(db, p, crate::string(args, "repository_id", 100)?)?;
            let identifier = s(&repository, "id");
            let path = crate::store::path(crate::string(args, "path", 4096)?)?;
            let episode = crate::graph_projection::resolve_episode(
                db,
                workspace,
                identifier,
                crate::string(args, "episode", 128)?,
            )?;
            let heads = crate::graph_projection::episode_heads(db, identifier, &episode)?
                .ok_or_else(|| Error::bad("The cited episode has no retained source manifest"))?;
            let sha = heads
                .get(path)
                .ok_or_else(|| Error::bad("File absent from the cited graph episode"))?;
            let original = one(
                db,
                "SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?",
                params![identifier, sha],
            )?
            .ok_or_else(|| Error::bad("The cited original was removed"))?;
            let content = s(&original, "content");
            if graphicle_memory::text::sha256(content.as_bytes()) != *sha {
                return Err(Error::bad("Historical source integrity check failed"));
            }
            let source = json!({"id":format!("file:{path}"),"path":path,"title":path,
                "sha256":sha,"version":sha,"episode":episode,"source_kind":"repository_file",
                "content":content,"authority":"historical_original_source"});
            let mut result = crate::memory::read_source(&source, args)?;
            result["episode"] = json!(episode);
            Ok(result)
        })
    }

    pub fn product_overview(&self, workspace: &str, cap: &str, repository: &str) -> Result<Value> {
        // Source and memory counts use the same ACL-filtered snapshot as retrieval.
        let status = self.project_status(workspace, cap, &json!({"repository_id":repository}))?;
        self.authorized_read(workspace, cap, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            let latest = one(db,
                "SELECT episode,sequence,created_at,node_count,edge_count FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
                [identifier])?;
            let current_episode = crate::graph_projection::current_episode(db, identifier)?;
            let recorded = crate::graph_projection::is_recorded(db, identifier, &current_episode)?;
            let current = one(db, "SELECT episode,sequence,created_at,node_count,edge_count FROM gf_episodes WHERE repository_id=? AND episode=?",
                params![identifier,current_episode])?;
            let counts = one(db,
                "SELECT count(*) AS episodes FROM gf_episodes WHERE repository_id=?", [identifier])?;
            let mut value = json!({
                "repository": repository, "status":status,
                "graph":{"latest":latest,"current":current,"current_episode":current_episode,"current_recorded":recorded,"counts":counts},
                "capabilities":{"mcp":true,"http":true,"source_citations":true,"version_history":true,
                    "memory_review":true,"x4_enabled":false},
                "model_calls":0,
                "generated_at":crate::now(),
            });
            // Workspace-wide operational metadata is visible only to its owner.
            if p.role == "owner" {
                value["jobs"] = json!(rows(db,
                    "SELECT state,count(*) AS count FROM am_jobs WHERE repository_id=? GROUP BY state", [identifier])?);
                value["connectors"] = json!(rows(db,
                    "SELECT id,name,kind,state,last_sync FROM am_connectors WHERE repository_id=? ORDER BY name LIMIT 100", [identifier])?);
            }
            Ok(value)
        })
    }

    pub fn product_budget(&self, workspace: &str, cap: &str, args: &Value) -> Result<Value> {
        let object = args
            .as_object()
            .ok_or_else(|| Error::bad("Expected a budget object"))?;
        if object.len() != 1 || !object.contains_key("monthly_requests") {
            return Err(Error::bad(
                "Supply monthly_requests, or null to remove your budget",
            ));
        }
        let limit = if args["monthly_requests"].is_null() {
            None
        } else {
            Some(
                args["monthly_requests"]
                    .as_u64()
                    .filter(|n| *n <= MAX_BUDGET)
                    .ok_or_else(|| {
                        Error::bad(
                            "monthly_requests must be an integer from 0 to 1000000000, or null",
                        )
                    })?,
            )
        };
        self.authorized(workspace, cap, "admin", |db, p| {
            // This is a customer-selected cap, not a billing entitlement.
            db.execute("INSERT INTO am_product_policy(workspace_id,monthly_request_budget,updated_at) VALUES(?,?,?)
                ON CONFLICT(workspace_id) DO UPDATE SET monthly_request_budget=excluded.monthly_request_budget,updated_at=excluded.updated_at",
                params![p.workspace,limit,crate::now()])?;
            Ok(json!({"monthly_requests":limit,"effective_limit":limits(db,&p.workspace)?.2}))
        })
    }

    pub fn product_usage(&self, workspace: &str, cap: &str) -> Result<Value> {
        self.authorized_read(workspace, cap, "admin", |db, p| {
            let period = month();
            let start = format!("{period}-01");
            let daily = rows(db, "SELECT day,requests,errors,elapsed_ms FROM am_usage WHERE workspace_id=? AND day>=? ORDER BY day",
                params![p.workspace,start])?;
            let requests:u64 = daily.iter().filter_map(|d| d["requests"].as_u64()).sum();
            let errors:u64 = daily.iter().filter_map(|d| d["errors"].as_u64()).sum();
            let (budget, allowance, effective) = limits(db, &p.workspace)?;
            let models = one(db,
                "SELECT count(*) AS calls,coalesce(sum(input_tokens),0) AS input_tokens,coalesce(sum(output_tokens),0) AS output_tokens
                 FROM am_model_calls c JOIN repo_projects r ON r.id=c.repository_id
                 WHERE r.workspace_id=? AND c.cached=0 AND c.created_at>=?", params![p.workspace,start])?;
            let unknown = one(db,
                "SELECT count(*) AS count FROM am_call_reservations c JOIN am_jobs j ON j.id=c.job_id
                 JOIN repo_projects r ON r.id=j.repository_id WHERE r.workspace_id=?
                 AND c.usage_reported=0 AND c.state!='released' AND c.created_at>=?", params![p.workspace,start])?;
            let mut usage = json!({
                "period":period,"timezone":"UTC","requests":requests,"http_errors":errors,"daily":daily,
                "budget":budget,"plan_allowance":allowance,"effective_limit":effective,
                "remaining":effective.map(|n| n.saturating_sub(requests)),
                "model_usage":models,"unreported_reservations":unknown.map(|r|r["count"].clone()),
                "scope":"Authenticated HTTP requests, including MCP tool calls. Failed requests consume the request budget. Sign-in, navigation, usage, billing, export, deletion and cancellation are exempt. Local CLI and background model calls are separate.",
                "cost":null,"cost_note":"Provider charges are not estimated. Model budgets are enforced per workflow; missing usage is shown separately."
            });
            crate::extension::installed().usage_details(db, &p.workspace, &mut usage)?;
            Ok(usage)
        })
    }

    pub fn reserve_request(&self, workspace: &str, cap: &str) -> Result<(String, Option<u64>)> {
        self.authorized(workspace, cap, "read", |db, p| {
            let date = day();
            let start = format!("{}-01", &date[..7]);
            let used:i64 = db.query_row("SELECT coalesce(sum(requests),0) FROM am_usage WHERE workspace_id=? AND day>=?",
                params![p.workspace,start], |r| r.get(0))?;
            let limit = limits(db, &p.workspace)?.2;
            if limit.is_some_and(|n| used as u64 >= n) {
                return Err(Error { status:StatusCode::TOO_MANY_REQUESTS,
                    message:"Workspace request budget reached. The owner can review Usage; export, deletion and cancellation remain available.".into(),
                    provider_failure:None });
            }
            db.execute("INSERT INTO am_usage(workspace_id,day,requests) VALUES(?,?,1)
                ON CONFLICT(workspace_id,day) DO UPDATE SET requests=requests+1",params![p.workspace,date])?;
            // Daily counters retain 25 months; they never grow with individual requests.
            let cutoff = (Utc::now() - chrono::Duration::days(760)).format("%Y-%m-%d").to_string();
            db.execute("DELETE FROM am_usage WHERE workspace_id=? AND day<?", params![p.workspace,cutoff])?;
            Ok((date,limit.map(|n| n.saturating_sub(used as u64 + 1))))
        })
    }
    fn finish_request(
        &self,
        workspace: &str,
        date: &str,
        failed: bool,
        elapsed: u64,
    ) -> Result<()> {
        self.transaction(|db| {
            db.execute("UPDATE am_usage SET errors=errors+?,elapsed_ms=elapsed_ms+? WHERE workspace_id=? AND day=?",
                params![i64::from(failed),elapsed.min(i64::MAX as u64),workspace,date])?;
            Ok(())
        })
    }
}

/// Commands that stay available when the request budget is used up: sign-in
/// (`build.info`, which the web app reads to open a workspace), status,
/// export, deletion and cancellation.
fn exempt_operation(value: &Value, mcp: bool) -> bool {
    if mcp {
        return matches!(
            value["method"].as_str(),
            Some(
                "initialize"
                    | "ping"
                    | "tools/list"
                    | "resources/list"
                    | "resources/templates/list"
                    | "notifications/initialized"
            )
        ) || (value["method"] == "tools/call"
            && matches!(
                value["params"]["name"].as_str(),
                Some("repository_status" | "list_projects")
            ));
    }
    matches!(
        value["operation"].as_str(),
        Some(
            "build.info"
                | "status"
                | "runtime.status"
                | "memory.export"
                | "source.forget"
                | "project.delete"
                | "document.original"
                | "memory.delete"
                | "connector.delete"
                | "session.forget"
                | "storage.prune"
                | "retention.apply"
                | "deletion.verify"
                | "deletion.receipts"
                | "deletion.state"
                | "workflow.cancel"
                | "credential.remove"
        )
    )
}

/// Runs inside the host/origin/session boundary so cookies cannot bypass accounting.
pub async fn meter(State(app): State<App>, request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    let (workspace, mcp) = match parts.as_slice() {
        ["api" | "v1", "workspaces", workspace, ..] => ((*workspace).to_owned(), false),
        // A project connection, and a workspace connection.
        ["mcp", workspace, _] | ["mcp", workspace] => ((*workspace).to_owned(), true),
        _ => return next.run(request).await,
    };
    // Control and navigation must remain usable when a data-plane budget is exhausted.
    if (!mcp && parts.len() == 3)
        || (!mcp
            && parts.len() == 4
            && matches!(
                parts[3],
                "usage"
                    | "budget"
                    | "billing"
                    | "browser-session"
                    | "repositories"
                    | "models"
                    | "credentials"
                    | "licence"
            ))
        || (!mcp
            && parts
                .get(3)
                .is_some_and(|v| matches!(*v, "billing" | "credentials")))
    {
        return next.run(request).await;
    }
    let cap = match api::token(request.headers()) {
        Ok(cap) => cap.to_owned(),
        Err(error) => return error.into_response(),
    };
    let mut request = request;
    if mcp || matches!(parts.last(), Some(&"command" | &"commands")) {
        let (head, body) = request.into_parts();
        let bytes = match to_bytes(body, BODY_LIMIT).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({"error":"Request body exceeds 24 MiB"})),
                )
                    .into_response();
            }
        };
        let exempt = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .is_some_and(|v| exempt_operation(&v, mcp));
        request = Request::from_parts(head, Body::from(bytes));
        if exempt {
            return next.run(request).await;
        }
    }
    let store = app.worker.store.clone();
    let w = workspace.clone();
    let reservation = blocking(move || store.reserve_request(&w, &cap)).await;
    let (date, remaining) = match reservation {
        Ok(value) => value,
        Err(error) => {
            let limited = error.status == StatusCode::TOO_MANY_REQUESTS;
            let mut response = error.into_response();
            if limited {
                let now = Utc::now();
                let (year, month) = if now.month() == 12 {
                    (now.year() + 1, 1)
                } else {
                    (now.year(), now.month() + 1)
                };
                let reset = chrono::NaiveDate::from_ymd_opt(year, month, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc();
                response.headers_mut().insert(
                    "retry-after",
                    HeaderValue::from_str(&(reset - now).num_seconds().max(1).to_string()).unwrap(),
                );
            }
            return response;
        }
    };
    let started = Instant::now();
    let mut response = next.run(request).await;
    let failed = response.status().is_client_error() || response.status().is_server_error();
    let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    if let Err(error) = blocking(move || {
        app.worker
            .store
            .finish_request(&workspace, &date, failed, elapsed)
    })
    .await
    {
        eprintln!("graphicle usage completion: {}", error.message);
    }
    if let Some(remaining) = remaining {
        response.headers_mut().insert(
            "x-graphicle-requests-remaining",
            HeaderValue::from_str(&remaining.to_string()).unwrap(),
        );
    }
    response
}

pub fn openapi() -> Value {
    json!({
        "openapi":"3.1.0",
        "info":{"title":"Graphicle API","version":env!("CARGO_PKG_VERSION"),
            "description":"Evidence, project memory, graph history and bounded workflows. The URL binds repository authority. Command argument/result shapes follow /docs and the MCP tools/list schemas."},
        "servers":[{"url":"/"}],
        "security":[{"capability":[]}],
        "components":{"securitySchemes":{"capability":{"type":"apiKey","in":"header","name":"X-Workspace-Capability"}},
            "schemas":{"Command":{"type":"object","required":["operation"],"additionalProperties":false,
                "properties":{"operation":{"type":"string"},"arguments":{"type":"object","default":{}}}},
                "Error":{"type":"object","required":["error"],"properties":{"error":{"type":"string"}}}}},
        "paths":{
            "/v1/workspaces/{workspace}/repos/{repository}/commands":{
                "parameters":[{"name":"workspace","in":"path","required":true,"schema":{"type":"string"}},{"name":"repository","in":"path","required":true,"schema":{"type":"string"}}],
                "post":{"operationId":"projectCommand","summary":"Run a project operation",
                    "requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Command"}}}},
                    "responses":{"200":{"description":"Operation result"},"400":{"description":"Invalid arguments"},"403":{"description":"Workspace or project unavailable"},"409":{"description":"Stale revision"},"429":{"description":"Monthly HTTP budget reached; Retry-After reports the reset"}}}},
            "/v1/workspaces/{workspace}/repos/{repository}/overview":{
                "parameters":[{"name":"workspace","in":"path","required":true,"schema":{"type":"string"}},{"name":"repository","in":"path","required":true,"schema":{"type":"string"}}],
                "get":{"operationId":"projectOverview","responses":{"200":{"description":"ACL-filtered source/memory counts and graph readiness"},"403":{"description":"Access denied"}}}},
            "/v1/workspaces/{workspace}/usage":{
                "parameters":[{"name":"workspace","in":"path","required":true,"schema":{"type":"string"}}],
                "get":{"operationId":"workspaceUsage","responses":{"200":{"description":"Owner-only UTC counters, limits and recorded model usage"},"403":{"description":"Owner access required"}}}},
            "/v1/workspaces/{workspace}/budget":{
                "parameters":[{"name":"workspace","in":"path","required":true,"schema":{"type":"string"}}],
                "post":{"operationId":"setRequestBudget","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","required":["monthly_requests"],"additionalProperties":false,"properties":{"monthly_requests":{"type":["integer","null"],"minimum":0,"maximum":MAX_BUDGET}}}}}},
                    "responses":{"200":{"description":"Budget and effective plan limit"},"400":{"description":"Invalid budget"},"403":{"description":"Owner access required"}}}}
        }
    })
}
