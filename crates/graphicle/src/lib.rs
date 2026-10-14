pub mod agent_detect;
pub mod agent_files;
pub mod agent_instructions;
pub mod agent_setup;
pub mod answer_review;
pub mod api;
pub mod archived;
pub mod atlassian;
pub mod auto_observe;
pub mod capture;
pub mod claims;
pub mod cli;
pub mod code_graph_arm;
pub mod codex_config;
pub mod connectors;
pub mod core_capabilities;
pub mod deletion;
pub mod disconnect;
pub mod distributed;
pub mod doc_links;
pub mod documents;
pub mod embedding_cache;
pub mod evidence;
pub mod extension;
pub mod folders;
pub mod fresh;
pub mod git_hooks;
pub mod graph;
pub mod graph_command;
mod graph_delta;
mod graph_materialize;
pub mod graph_projection;
pub mod graph_tools;
pub mod ingestion;
pub mod invalidation;
pub mod jobs;
pub mod json_edit;
pub mod limits;
pub mod local;
pub mod local_inference;
pub mod maintenance;
pub mod memory;
pub mod memory_evidence;
pub mod model;
pub mod oauth;
pub mod observer;
pub mod operations;
pub mod permissions;
pub mod phase;
pub mod product;
pub mod projection;
pub mod prompt_context;
pub mod query;
pub mod releases;
pub mod retention;
pub mod search;
pub mod settings;
pub mod setup;
pub mod store;
pub mod store_compat;
pub mod structured_memory;
pub mod symbols;
pub mod temporal_graph;
pub mod terminal;
pub mod vault;
pub mod vector;
pub mod work_state;
mod workflow_retrieval;
pub mod workflows;
pub mod workspace;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[derive(Debug)]
pub struct Error {
    pub status: StatusCode,
    pub message: String,
    pub provider_failure: Option<model::Failure>,
}
pub type Result<T> = std::result::Result<T, Error>;
impl Error {
    pub fn bad(message: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.to_string(),
            provider_failure: None,
        }
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
            provider_failure: None,
        }
    }
    pub fn denied() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: "Workspace access denied".into(),
            provider_failure: None,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Error {}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::bad(format!("Storage operation failed: {e}"))
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::bad("Invalid JSON data")
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::bad(format!("Local I/O failed: {e}"))
    }
}
impl From<String> for Error {
    fn from(e: String) -> Self {
        Self::bad(e)
    }
}
impl From<graphicle_memory::Error> for Error {
    fn from(error: graphicle_memory::Error) -> Self {
        use graphicle_memory::ErrorKind;
        let status = match error.kind() {
            ErrorKind::LimitExceeded => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorKind::Io | ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::External | ErrorKind::Model => StatusCode::BAD_GATEWAY,
            _ => StatusCode::BAD_REQUEST,
        };
        Self {
            status,
            message: error.to_string(),
            provider_failure: None,
        }
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error":self.message}))).into_response()
    }
}

pub fn string<'a>(v: &'a Value, key: &str, max: usize) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= max)
        .ok_or_else(|| Error::bad(format!("Expected {key}, 1–{max} bytes")))
}
pub fn integer(v: &Value, key: &str, default: usize, max: usize) -> Result<usize> {
    match v.get(key) {
        None => Ok(default),
        Some(n) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= max)
            .ok_or_else(|| Error::bad(format!("Invalid {key}"))),
    }
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
pub fn id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
pub fn digest(v: &Value) -> String {
    graphicle_memory::text::sha256(graphicle_memory::text::canonical(v).as_bytes())
}
/// Run a native (memory engine) operation. An operation an installed
/// extension declares (`Extension::native_operations`, always in the
/// `graph.` namespace) is served by that extension instead; the core's own
/// native calls never use that namespace, so no extension changes them.
pub fn native(operation: &str, mut request: Value) -> Result<Value> {
    if !request.is_object() {
        return Err(Error::bad("Expected a native request object"));
    }
    if let Some(result) = extension::installed().native(operation, &mut request) {
        return result;
    }
    request["operation"] = json!(operation);
    graphicle_memory::dispatch(request).map_err(Error::from)
}

#[cfg(test)]
mod native_boundary_tests {
    use super::*;
    /// Extensions may only declare native operations in the `graph.`
    /// namespace (`extension::install`), so the core's own calls stay with
    /// the memory engine as long as none of them uses that namespace.
    #[test]
    fn the_core_never_calls_a_native_graph_operation() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(source).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let compact: String = text.split_whitespace().collect();
                assert!(
                    !compact.contains("native(\"graph."),
                    "{} calls a native graph. operation",
                    path.display()
                );
            }
        }
    }
    #[test]
    fn malformed_native_request_is_a_client_error_without_unwinding() {
        for value in [Value::Null, json!([]), json!(1)] {
            assert_eq!(
                native("info", value).unwrap_err().status,
                StatusCode::BAD_REQUEST
            );
        }
    }
    #[test]
    fn native_error_kinds_determine_http_status() {
        assert_eq!(
            Error::from(graphicle_memory::Error::invalid("bad data")).status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            Error::from(graphicle_memory::Error::Internal(
                "unexpected failure".into()
            ))
            .status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            Error::from(graphicle_memory::Error::LimitExceeded {
                resource: "response",
                limit: 1
            })
            .status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}

#[cfg(test)]
mod fork_guard;
