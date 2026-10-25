// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/build.py, graphify/semantic_cleanup.py
//! Native member-normalization dependency from graphify/build.py (187–258).
//! The remainder of build.py is tracked separately and is not yet ported.
use crate::python_compat::{repr, scalar_key, string};
use serde_json::Value;
use std::collections::BTreeSet;

pub fn normalize(he: &mut Value) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(object) = he.as_object_mut() else {
        return warnings;
    };
    let id = object.get("id").map(string).unwrap_or_else(|| "?".into());
    let field = ["nodes", "members", "node_ids"]
        .into_iter()
        .find(|key| object.get(*key).is_some_and(Value::is_array));
    if let Some(field) = field {
        let mut seen = BTreeSet::new();
        let mut members = Vec::new();
        for original in object[field].as_array().unwrap() {
            let mut member = original.clone();
            if let Some(obj) = original.as_object() {
                member = obj.get("id").cloned().unwrap_or(Value::Null);
                if member.is_number() {
                    member = Value::String(string(&member));
                }
                if member.is_null() || member.as_str() == Some("") || scalar_key(&member).is_none()
                {
                    warnings.push(format!("[graphicle] WARNING: hyperedge '{id}' has a member object with no usable 'id' ({}); dropping that member.", repr(original)));
                    continue;
                }
            } else if scalar_key(&member).is_none() {
                warnings.push(format!("[graphicle] WARNING: hyperedge '{id}' has an unusable member reference {}; dropping that member.", repr(original)));
                continue;
            }
            if seen.insert(scalar_key(&member).unwrap()) {
                members.push(member);
            }
        }
        object.insert("nodes".into(), Value::Array(members));
        if field != "nodes" {
            warnings.push(format!("[graphicle] WARNING: hyperedge '{id}' uses field '{field}' instead of 'nodes'; normalizing."));
        }
    }
    object.shift_remove("members");
    object.shift_remove("node_ids");
    warnings
}
