// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/swift_dispatch.py
//! The `.swift` gate over [`crate::interface_dispatch`]: a call through an
//! injected protocol lands on the requirement, and the single conformer's
//! method is linked to it with a member-level `dispatches_to` edge.
use crate::Result;
use serde_json::Value;
const SWIFT_SUFFIXES: &[&str] = &[".swift"];
pub fn resolve_swift_protocol_dispatch(
    per_file: &[Value],
    nodes: &[Value],
    edges: &mut Vec<Value>,
) -> Result<()> {
    crate::interface_dispatch::resolve_interface_dispatch(per_file, nodes, edges, SWIFT_SUFFIXES)
}
pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "swift_dispatch.resolve" => {
            crate::interface_dispatch::resolve_request(request, resolve_swift_protocol_dispatch)
        }
        _ => Err("Unknown Swift dispatch operation".into()),
    }
}
