// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/csharp_dispatch.py
//! The `.cs` gate over [`crate::interface_dispatch`] (Graphify 0bd22d4).
pub use crate::interface_dispatch::{DISPATCH_RELATION, method_label};
use crate::{Result, resolver_registry::LanguageResolver};
use serde_json::{Value, json};
use std::sync::Arc;
const CSHARP_SUFFIXES: &[&str] = &[".cs"];
pub fn is_csharp(node: &Value) -> bool {
    crate::interface_dispatch::declared_in(Some(node), CSHARP_SUFFIXES)
}
pub fn resolve_csharp_interface_dispatch(
    per_file: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
) -> Result<()> {
    crate::interface_dispatch::resolve_interface_dispatch(per_file, nodes, edges, CSHARP_SUFFIXES)
}
pub fn resolver() -> LanguageResolver {
    LanguageResolver {
        name: "csharp_interface_dispatch".into(),
        suffixes: [".cs".into()].into(),
        resolve: Arc::new(|per_file, nodes, edges| {
            resolve_csharp_interface_dispatch(per_file, nodes, edges)
        }),
    }
}
pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "csharp_dispatch.is_csharp" => Ok(json!(is_csharp(&request["node"]))),
        "csharp_dispatch.method_label" => Ok(json!(method_label(&request["node"]))),
        "csharp_dispatch.resolve" => {
            crate::interface_dispatch::resolve_request(request, resolve_csharp_interface_dispatch)
        }
        _ => Err("Unknown C# dispatch operation".into()),
    }
}
