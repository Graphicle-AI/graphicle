// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py, graphify/extractors/julia.py
//! Shared edge helpers from engine.py; the complete generic engine is still pending.
use crate::{Result, python_compat};
use serde_json::{Value, json};
use sha1::Digest;
pub const REFERENCE_CONTEXTS: &[&str] = &[
    "field",
    "parameter_type",
    "return_type",
    "generic_arg",
    "attribute",
    "value",
    "type",
];
pub fn csharp_namespace_id(name: &str) -> String {
    format!(
        "csharp_namespace:{}",
        &format!("{:x}", sha1::Sha1::digest(name.as_bytes()))[..16]
    )
}
pub fn source_location(line: &Value) -> Value {
    if line.is_null() {
        return Value::Null;
    }
    if let Some(s) = line.as_str()
        && s.starts_with('L')
    {
        return line.clone();
    }
    json!(format!("L{}", python_compat::string(line)))
}
pub fn semantic_reference_edge(
    source: &str,
    target: &str,
    context: &str,
    file: &str,
    line: &Value,
) -> Result<Value> {
    if !REFERENCE_CONTEXTS.contains(&context) {
        return Err(format!("ValueError: unknown reference context: {context}").into());
    }
    Ok(
        json!({"source":source,"target":target,"relation":"references","context":context,"confidence":"EXTRACTED","source_file":file,"source_location":source_location(line),"weight":1.0}),
    )
}
