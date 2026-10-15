// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/exporters/graphdb.py, graphify/exporters/html.py
use crate::{Result, cluster::Communities, graph::Graph, python_compat, python_json};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn communities(r: &Value) -> Result<Communities> {
    crate::cluster::communities_from_json(r.get("communities").unwrap_or(&serde_json::json!({})))
}
pub fn labels(value: &Value) -> Result<BTreeMap<i64, Value>> {
    match value.as_object() {
        None if value.is_null() => Ok(BTreeMap::new()),
        None => Err("Expected community labels object".into()),
        Some(v) => v
            .iter()
            .map(|(k, v)| {
                Ok((
                    k.parse::<i64>()
                        .map_err(|_| "Expected integer community ID")?,
                    v.clone(),
                ))
            })
            .collect(),
    }
}
pub fn assignments(graph: &Graph, communities: &Communities) -> Vec<Option<i64>> {
    let mut out = vec![None; graph.nodes.len()];
    for (cid, members) in communities {
        for member in members {
            if let Some(i) = graph.node_index(member) {
                out[i] = Some(*cid);
            }
        }
    }
    out
}
pub fn community_name(cid: Option<i64>, labels: &BTreeMap<i64, Value>) -> Value {
    cid.and_then(|c| labels.get(&c))
        .cloned()
        .unwrap_or_else(|| {
            Value::String(format!(
                "Community {}",
                cid.map(|c| c.to_string()).unwrap_or_else(|| "None".into())
            ))
        })
}
pub fn string(v: &Value) -> String {
    python_compat::string(v)
}
pub fn json_text(v: &Value, indent: Option<usize>, ascii: bool) -> Result<String> {
    let mut buf = Vec::new();
    python_json::write(&mut buf, v, indent, ascii).map_err(crate::Error::from)?;
    String::from_utf8(buf).map_err(crate::Error::from)
}
/// JSON for a `<script>` element: every `<` is escaped as `\u003c`, which
/// `JSON.parse` reads back unchanged. Escaping only `</` is not enough: an
/// unclosed `<!--` and a later `<script` in a label leave the tokenizer in a
/// state where the real `</script>` no longer closes the element (#4124).
pub fn js_safe(v: &Value) -> Result<String> {
    Ok(json_text(v, None, true)?.replace('<', "\\u003c"))
}
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
pub fn truth(v: &Value) -> bool {
    crate::build_normalize::truthy(v)
}
