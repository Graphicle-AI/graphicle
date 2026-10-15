// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/csharp_dispatch.py
//! The `.cs` gate over [`crate::interface_dispatch`] (Graphify 0bd22d4).
pub use crate::interface_dispatch::{DISPATCH_RELATION, method_label};
use crate::{
    Result,
    extract::per_file::PerFile,
    resolver_registry::{LanguageResolver, Rec},
};
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
    resolve_csharp_interface_dispatch_any(per_file, nodes, edges)
}
/// [`resolve_csharp_interface_dispatch`] over the per-file results and the
/// edges in either form.
pub fn resolve_csharp_interface_dispatch_any<P: PerFile, E: Rec>(
    per_file: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) -> Result<()> {
    crate::interface_dispatch::resolve_interface_dispatch_any(
        per_file,
        nodes,
        edges,
        CSHARP_SUFFIXES,
    )
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
