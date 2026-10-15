// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/scip_ingest.py
//! Native port of Graphify's simplified SCIP JSON ingestion, not SCIP protobuf.
use crate::security;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};

pub type PerDocumentIndex = HashMap<(String, String), String>;
pub type GlobalIndex = HashMap<String, Vec<String>>;
type EdgeKey = (String, String, String, String);

pub fn coerce_str<'a>(value: Option<&'a Value>, default: Option<&'a Value>) -> &'a str {
    value
        .and_then(Value::as_str)
        .or_else(|| default.and_then(Value::as_str))
        .unwrap_or("")
}
pub fn make_scip_node_id(symbol: &str, source_file: &str) -> String {
    let raw = format!("{source_file}:{symbol}");
    let hash = format!("{:x}", Sha1::digest(raw.as_bytes()));
    let suffix: String = symbol
        .rsplit('#')
        .next()
        .unwrap_or(symbol)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let suffix = suffix.trim_matches('_').to_ascii_lowercase();
    if suffix.is_empty() {
        format!("scip_{}", &hash[..12])
    } else {
        format!("scip_{suffix}_{}", &hash[..12])
    }
}
pub fn scip_kind_to_file_type(_kind: &str) -> &'static str {
    "code"
}
pub fn build_scip_metadata(symbol: &str, kind: &str, description: &str) -> Value {
    let mut out = json!({"scip_symbol":symbol,"scip_kind":kind});
    if !description.is_empty() {
        out["scip_description"] = json!(description);
    }
    out
}
pub fn is_true(value: Option<&Value>) -> bool {
    value == Some(&Value::Bool(true))
}
pub fn relation_for(rel: &Value) -> &'static str {
    if is_true(rel.get("is_implementation")) {
        "scip_impl"
    } else if is_true(rel.get("is_type_definition")) {
        "scip_typed"
    } else if is_true(rel.get("is_definition")) {
        "scip_def"
    } else {
        "scip_ref"
    }
}
pub fn first_occurrence_line(occurrences: Option<&Value>) -> Value {
    let line = occurrences
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|o| o.as_object())
        .and_then(|o| o.get("range"))
        .and_then(Value::as_array)
        .and_then(|a| a.first());
    if let Some(Value::Number(n)) = line {
        // Keep arbitrary-size Python integers without narrowing them to machine words.
        let raw = n.to_string();
        if raw.bytes().all(|b| b.is_ascii_digit()) {
            return Value::Number(n.clone());
        }
    }
    json!(0)
}
fn location(raw: &Value) -> String {
    let line = first_occurrence_line(raw.get("occurrences"));
    if line == json!(0) {
        String::new()
    } else {
        format!("L{line}")
    }
}
pub fn resolve_relationship_target(
    target: &str,
    source_doc: &str,
    per_doc: &PerDocumentIndex,
    global: &GlobalIndex,
) -> Option<String> {
    if let Some(id) = per_doc.get(&(target.into(), source_doc.into())) {
        return Some(id.clone());
    }
    global
        .get(target)
        .filter(|c| c.len() == 1)
        .map(|c| c[0].clone())
}
struct Record<'a> {
    node_id: String,
    symbol_id: &'a str,
    doc_path: &'a str,
    raw: &'a Value,
}
fn emit_symbol_node(record: &Record<'_>, nodes: &mut Vec<Value>, seen: &mut HashSet<String>) {
    if !seen.insert(record.node_id.clone()) {
        return;
    }
    let raw = record.raw;
    let kind = raw.get("kind").and_then(Value::as_str).unwrap_or("unknown");
    let display = raw
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let description = raw
        .get("documentation")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or("");
    let suffix = record
        .symbol_id
        .rsplit('#')
        .next()
        .unwrap_or(record.symbol_id);
    let label = if !display.is_empty() {
        display
    } else if !suffix.is_empty() {
        suffix
    } else {
        record.symbol_id
    };
    nodes.push(json!({"id":record.node_id,"label":label,"file_type":scip_kind_to_file_type(kind),"source_file":record.doc_path,"source_location":location(raw),
        "metadata":security::sanitize_metadata(Some(&build_scip_metadata(record.symbol_id,kind,description)))}));
}
fn emit_relationships(
    record: &Record<'_>,
    per_doc: &PerDocumentIndex,
    global: &GlobalIndex,
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
    seen_nodes: &mut HashSet<String>,
    seen_edges: &mut HashSet<EdgeKey>,
) {
    let Some(relationships) = record.raw.get("relationships").and_then(Value::as_array) else {
        return;
    };
    let loc = location(record.raw);
    for rel in relationships.iter().filter(|v| v.is_object()) {
        let target = rel.get("symbol").and_then(Value::as_str).unwrap_or("");
        if target.is_empty() {
            continue;
        }
        let target_id = if let Some(id) =
            resolve_relationship_target(target, record.doc_path, per_doc, global)
        {
            id
        } else {
            let id = make_scip_node_id(target, record.doc_path);
            if seen_nodes.insert(id.clone()) {
                let suffix = target.rsplit('#').next().unwrap_or(target);
                nodes.push(json!({"id":id,"label":if suffix.is_empty(){target}else{suffix},"file_type":"code","source_file":record.doc_path,"source_location":"",
                    "metadata":security::sanitize_metadata(Some(&build_scip_metadata(target,"external","")))}));
            }
            id
        };
        let relation = relation_for(rel);
        if !seen_edges.insert((
            record.node_id.clone(),
            target_id.clone(),
            relation.into(),
            loc.clone(),
        )) {
            continue;
        }
        edges.push(json!({"source":record.node_id,"target":target_id,"relation":relation,"confidence":"EXTRACTED","confidence_score":1.0,
            "source_file":record.doc_path,"source_location":loc,"weight":1.0,"context":"scip",
            "metadata":security::sanitize_metadata(Some(&json!({"scip_relationship":rel})))}));
    }
}
pub fn ingest_scip_json(doc: &Value, source_file: &Value, language: &Value) -> Value {
    let (mut nodes, mut edges) = (vec![], vec![]);
    let Some(documents) = doc
        .as_object()
        .and_then(|d| d.get("documents"))
        .and_then(Value::as_array)
    else {
        return json!({"nodes":nodes,"edges":edges});
    };
    let mut per_doc = PerDocumentIndex::new();
    let mut global = GlobalIndex::new();
    let mut records = vec![];
    for document in documents.iter().filter(|d| d.is_object()) {
        let doc_path = coerce_str(document.get("relative_path"), Some(source_file));
        // Upstream records the language for later consumers but does not emit it.
        let _doc_language = coerce_str(document.get("language"), Some(language));
        let Some(symbols) = document.get("symbols").and_then(Value::as_array) else {
            continue;
        };
        for symbol in symbols.iter().filter(|s| s.is_object()) {
            let symbol_id = symbol.get("symbol").and_then(Value::as_str).unwrap_or("");
            if symbol_id.is_empty() {
                continue;
            }
            let node_id = make_scip_node_id(symbol_id, doc_path);
            per_doc
                .entry((symbol_id.into(), doc_path.into()))
                .or_insert_with(|| node_id.clone());
            let candidates = global.entry(symbol_id.into()).or_default();
            if !candidates.contains(&node_id) {
                candidates.push(node_id.clone());
            }
            records.push(Record {
                node_id,
                symbol_id,
                doc_path,
                raw: symbol,
            });
        }
    }
    let (mut seen_nodes, mut seen_edges) = (HashSet::new(), HashSet::new());
    for record in records {
        emit_symbol_node(&record, &mut nodes, &mut seen_nodes);
        emit_relationships(
            &record,
            &per_doc,
            &global,
            &mut nodes,
            &mut edges,
            &mut seen_nodes,
            &mut seen_edges,
        );
    }
    json!({"nodes":nodes,"edges":edges})
}
