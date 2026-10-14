// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/validate.py
//! Port of graphify/validate.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
use crate::{
    Result,
    python_compat::{repr, scalar_key, string, type_name},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const VALID_FILE_TYPES: &[&str] =
    &["code", "concept", "document", "image", "paper", "rationale"];
pub const VALID_CONFIDENCES: &[&str] = &["AMBIGUOUS", "EXTRACTED", "INFERRED"];
pub const REQUIRED_NODE_FIELDS: &[&str] = &["file_type", "id", "label", "source_file"];
pub const REQUIRED_EDGE_FIELDS: &[&str] =
    &["confidence", "relation", "source", "source_file", "target"];

fn in_string_set(value: &Value, allowed: &[&str]) -> Result<bool> {
    if scalar_key(value).is_none() {
        return Err(format!("TypeError: unhashable type: '{}'", type_name(value)).into());
    }
    Ok(value.as_str().is_some_and(|value| allowed.contains(&value)))
}

/// Preserve the reference's checks and diagnostics. Required-field diagnostics
/// use a stable order; upstream iterates an unordered Python set.
pub fn validate_extraction(data: &Value) -> Result<Vec<String>> {
    validate_extraction_with(data, true)
}
/// [`validate_extraction`] without the "does not match any node id" errors
/// when `dangling` is false (every other error, in the same order): callers
/// that discard those skip indexing the node ids and checking every endpoint.
pub fn validate_extraction_with(data: &Value, dangling: bool) -> Result<Vec<String>> {
    let Some(data) = data.as_object() else {
        return Ok(vec!["Extraction must be a JSON object".into()]);
    };
    let mut errors = Vec::new();
    let mut node_ids = BTreeSet::new();
    // Whether any node has a hashable id (Python's `bool(node_ids)`).
    let mut any_id = false;
    match data.get("nodes") {
        None => errors.push("Missing required key 'nodes'".into()),
        Some(Value::Array(nodes)) => {
            for (i, node) in nodes.iter().enumerate() {
                let Some(node) = node.as_object() else {
                    errors.push(format!("Node {i} must be an object"));
                    continue;
                };
                let unknown = json!("?");
                let id = node.get("id").unwrap_or(&unknown);
                for field in REQUIRED_NODE_FIELDS {
                    if !node.contains_key(*field) {
                        errors.push(format!(
                            "Node {i} (id={}) missing required field '{field}'",
                            repr(id)
                        ));
                    }
                }
                if let Some(id) = node.get("id") {
                    let hashable = if dangling {
                        scalar_key(id).map(|key| node_ids.insert(key)).is_some()
                    } else {
                        // A string is always hashable; skip copying it.
                        match id {
                            Value::String(_) => true,
                            other => scalar_key(other).is_some(),
                        }
                    };
                    if hashable {
                        any_id = true;
                    } else {
                        errors.push(format!(
                            "Node {i} has non-hashable id {} - id must be a string",
                            repr(id)
                        ));
                    }
                }
                if let Some(kind) = node.get("file_type")
                    && !in_string_set(kind, VALID_FILE_TYPES)?
                {
                    errors.push(format!(
                        "Node {i} (id={}) has invalid file_type '{}' - must be one of {}",
                        repr(id),
                        string(kind),
                        repr(&json!(VALID_FILE_TYPES))
                    ));
                }
            }
        }
        _ => errors.push("'nodes' must be a list".into()),
    }
    let edge_list = if data.contains_key("edges") {
        data.get("edges")
    } else {
        data.get("links")
    };
    match edge_list {
        None | Some(Value::Null) => errors.push("Missing required key 'edges'".into()),
        Some(Value::Array(edges)) => {
            for (i, edge) in edges.iter().enumerate() {
                let Some(edge) = edge.as_object() else {
                    errors.push(format!("Edge {i} must be an object"));
                    continue;
                };
                for field in REQUIRED_EDGE_FIELDS {
                    if !edge.contains_key(*field) {
                        errors.push(format!("Edge {i} missing required field '{field}'"));
                    }
                }
                if let Some(confidence) = edge.get("confidence")
                    && !in_string_set(confidence, VALID_CONFIDENCES)?
                {
                    errors.push(format!(
                        "Edge {i} has invalid confidence '{}' - must be one of {}",
                        string(confidence),
                        repr(&json!(VALID_CONFIDENCES))
                    ));
                }
                for endpoint in ["source", "target"] {
                    let Some(value) = edge.get(endpoint) else {
                        continue;
                    };
                    // Preserve Python's bool(node_ids) short-circuit: no hash
                    // lookup takes place when the ID set is empty.
                    if !any_id {
                        continue;
                    }
                    if !dangling {
                        // A string is always hashable; skip copying it.
                        let hashable = match value {
                            Value::String(_) => true,
                            other => scalar_key(other).is_some(),
                        };
                        if !hashable {
                            errors.push(format!(
                                "Edge {i} {endpoint} {} is non-hashable - must be a string",
                                repr(value)
                            ));
                        }
                        continue;
                    }
                    match scalar_key(value) {
                        None => errors.push(format!(
                            "Edge {i} {endpoint} {} is non-hashable - must be a string",
                            repr(value)
                        )),
                        Some(key) if !node_ids.contains(&key) => errors.push(format!(
                            "Edge {i} {endpoint} '{}' does not match any node id",
                            string(value)
                        )),
                        _ => {}
                    }
                }
            }
        }
        _ => errors.push("'edges' must be a list".into()),
    }
    Ok(errors)
}

pub fn assert_valid(data: &Value) -> Result<()> {
    let errors = validate_extraction(data)?;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Extraction JSON has {} error(s):\n{}",
            errors.len(),
            errors
                .iter()
                .map(|error| format!("  • {error}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_ids_do_not_hide_dangling_references() {
        let data = json!({"nodes":[{"id":"a","label":"A","file_type":"code","source_file":"a.py"},{"id":["bad"],"label":"B","file_type":"code","source_file":"b.py"}],"edges":[{"source":"a","target":"missing","relation":"calls","confidence":"EXTRACTED","source_file":"a.py"}]});
        let errors = validate_extraction(&data).unwrap();
        assert_eq!(errors.len(), 2);
        assert!(errors[0].contains("non-hashable"));
        assert!(errors[1].contains("does not match"));
    }
    #[test]
    fn links_fallback_and_python_scalar_identity_are_preserved() {
        let data = json!({"nodes":[{"id":true,"label":"A","file_type":"code","source_file":"a"}],"links":[{"source":1,"target":1.0,"relation":"calls","confidence":"EXTRACTED","source_file":"a"}]});
        assert_eq!(validate_extraction(&data).unwrap(), Vec::<String>::new());
        assert!(assert_valid(&json!({"nodes":[],"edges":null,"links":[]})).is_err());
    }
}
