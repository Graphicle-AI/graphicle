// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/build.py, graphify/ruby_resolution.py
//! File-label predicate from graphify/build.py; the build module remains pending.
use crate::python_compat::string;
use serde_json::Value;
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
pub fn is_file_node_label(label: &Value, source_file: &Value) -> bool {
    if !truthy(label) || !truthy(source_file) {
        return false;
    }
    let source = string(source_file).replace('\\', "/");
    let label = string(label);
    label == source.rsplit('/').next().unwrap_or("")
        || (label.contains('/') && (source == label || source.ends_with(&format!("/{label}"))))
}
/// [`is_file_node_label`] of two strings, without copying them.
pub fn is_file_node_label_str(label: &str, source_file: &str) -> bool {
    if label.is_empty() || source_file.is_empty() {
        return false;
    }
    // The last `/` component once `\` reads as `/`.
    if label == source_file.rsplit(['/', '\\']).next().unwrap_or("") {
        return true;
    }
    if !label.contains('/') {
        return false;
    }
    let source = source_file.replace('\\', "/");
    source == label
        || (source.len() > label.len()
            && source.ends_with(label)
            && source.as_bytes()[source.len() - label.len() - 1] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_predicate_matches_the_value_one() {
        let labels = [
            "", "a.py", "b/a.py", "x\\a.py", "pkg/", "/a.py", "a", "c/b/a.py", "b\\a.py", "é/ü.py",
        ];
        let sources = [
            "",
            "a.py",
            "b/a.py",
            "c/b/a.py",
            "c\\b\\a.py",
            "cb/a.py",
            "a",
            "/a.py",
            "b\\a.py",
            "x/é/ü.py",
            "pkg/",
            "c/b\\a.py",
        ];
        for label in labels {
            for source in sources {
                assert_eq!(
                    is_file_node_label_str(label, source),
                    is_file_node_label(&Value::from(label), &Value::from(source)),
                    "{label:?} {source:?}"
                );
            }
        }
    }
}
