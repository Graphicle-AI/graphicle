// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/swift_dispatch.py
//! The `.swift` gate over [`crate::interface_dispatch`]: a call through an
//! injected protocol lands on the requirement, and every conformer's method
//! that owns the name unambiguously is linked to it with a member-level
//! `dispatches_to` edge.
use crate::{Result, extract::per_file::PerFile, resolver_registry::Rec};
use serde_json::Value;
const SWIFT_SUFFIXES: &[&str] = &[".swift"];
pub fn resolve_swift_protocol_dispatch(
    per_file: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
) -> Result<()> {
    resolve_swift_protocol_dispatch_any(per_file, nodes, edges)
}
/// [`resolve_swift_protocol_dispatch`] over the per-file results and the
/// edges in either form.
pub fn resolve_swift_protocol_dispatch_any<P: PerFile, E: Rec>(
    per_file: &[P],
    nodes: &[Value],
    edges: &mut Vec<E>,
) -> Result<()> {
    crate::interface_dispatch::resolve_interface_dispatch_any(
        per_file,
        nodes,
        edges,
        SWIFT_SUFFIXES,
    )
}
pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "swift_dispatch.resolve" => {
            crate::interface_dispatch::resolve_request(request, resolve_swift_protocol_dispatch)
        }
        _ => Err("Unknown Swift dispatch operation".into()),
    }
}
