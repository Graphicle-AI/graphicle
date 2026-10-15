// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/markdown_resolution.py
//! Port of graphify/markdown_resolution.py; emits only uniquely supported references.
//!
//! The pass reads the edges and the per-file raw calls in whichever form
//! assembly holds them ([`Rec`], [`PerFile`]), so neither list converts for
//! it; the JSON forms give what they always gave.
use crate::{
    extract::per_file::PerFile,
    extractors::base::LANGUAGE_BUILTIN_GLOBALS,
    python_compat,
    resolver_registry::{Rec, append_edges, at, edge_record},
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub const MARKDOWN_MENTION_SUFFIXES: &[&str] = &[".md", ".mdx", ".qmd", ".skill"];
pub const CONTAINMENT_RELATIONS: &[&str] = &["contains", "method"];
type Labels = HashMap<String, Vec<String>>;
pub type Parents = HashMap<String, HashSet<String>>;

fn text(node: &Value, field: &str) -> String {
    node.get(field)
        .map(python_compat::string)
        .unwrap_or_default()
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
pub fn symbol_label(node: &Value) -> String {
    text(node, "label")
        .trim_matches(['(', ')'])
        .trim_start_matches('.')
        .into()
}
pub fn is_file_node(node: &Value) -> bool {
    let source = text(node, "source_file");
    !source.is_empty() && text(node, "label") == source.rsplit('/').next().unwrap_or("")
}
pub fn posix(path: &str) -> String {
    path.replace('\\', "/")
}

fn normpath(path: &str) -> String {
    let mut parts = Vec::<String>::new();
    for part in crate::path_syntax::posix_parts(path) {
        if part == ".." {
            if parts
                .last()
                .is_some_and(|p| !["..", "/", "//"].contains(&p.as_str()))
            {
                parts.pop();
            } else if parts
                .first()
                .is_none_or(|p| !["/", "//"].contains(&p.as_str()))
            {
                parts.push(part);
            }
        } else {
            parts.push(part);
        }
    }
    crate::path_syntax::posix_join(&parts)
}
pub fn match_cited_file(
    cited: &str,
    doc_file: &str,
    source_files: &HashSet<String>,
) -> Option<String> {
    let cited = posix(cited);
    let head = doc_file.rfind('/').map_or("", |i| &doc_file[..i + 1]);
    let dir = if head.chars().all(|c| c == '/') {
        head
    } else {
        head.trim_end_matches('/')
    };
    let joined = if cited.starts_with('/') || dir.is_empty() {
        cited.clone()
    } else {
        format!("{dir}{}{cited}", if dir.ends_with('/') { "" } else { "/" })
    };
    let relative = posix(&normpath(&joined));
    if source_files.contains(&relative) {
        return Some(relative);
    }
    if cited.starts_with("./") || cited.starts_with("../") {
        return None;
    }
    if source_files.contains(&cited) {
        return Some(cited);
    }
    let mut stripped = cited.as_str();
    while let Some(rest) = stripped
        .strip_prefix("./")
        .or_else(|| stripped.strip_prefix("../"))
    {
        stripped = rest;
    }
    let suffix = format!("/{stripped}");
    let mut matches = source_files
        .iter()
        .filter(|sf| sf.as_str() == stripped || sf.ends_with(&suffix));
    let first = matches.next()?;
    matches.next().is_none().then(|| first.clone())
}
pub fn evidence(
    node_id: &str,
    nodes_by_id: &HashMap<String, &Value>,
    parents: &Parents,
) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut frontier = vec![node_id.to_owned()];
    let mut seen = HashSet::new();
    while let Some(id) = frontier.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        for owner in parents.get(&id).into_iter().flatten() {
            if let Some(node) = nodes_by_id.get(owner) {
                found.insert(symbol_label(node));
            }
            frontier.push(owner.clone());
        }
    }
    if let Some(node) = nodes_by_id.get(node_id) {
        for segment in posix(&text(node, "source_file"))
            .split('/')
            .filter(|s| !s.is_empty())
        {
            found.insert(segment.into());
            // os.path.splitext treats a run of leading dots as part of the stem.
            let stem = segment
                .rfind('.')
                .filter(|&at| segment[..at].chars().any(|c| c != '.'))
                .map_or(segment, |at| &segment[..at]);
            found.insert(stem.into());
        }
    }
    found
}
pub fn markdown_raw_calls(per_file: &[Value]) -> Vec<&Value> {
    markdown_raw_calls_any(per_file)
}
/// The Markdown mentions among the raw calls, in result order, from the
/// per-file results in either form. A call that is no object has no
/// `language` and is skipped.
pub fn markdown_raw_calls_any<P: PerFile>(per_file: &[P]) -> Vec<&P::R> {
    per_file
        .iter()
        .filter_map(PerFile::raw_calls)
        .flatten()
        .filter(|call| at(*call, "language").as_str() == Some("markdown"))
        .collect()
}
/// [`text`] of a raw-call record.
fn rtext<R: Rec>(record: &R, field: &str) -> String {
    record
        .rec_get(field)
        .map(python_compat::string)
        .unwrap_or_default()
}
fn resolve_chain(labels: &Labels, names: &[String], parents: &Parents) -> Option<String> {
    let mut candidates = labels.get(names.last()?).cloned().unwrap_or_default();
    for (index, owner_name) in names[..names.len() - 1].iter().rev().enumerate() {
        let owners: HashSet<_> = labels
            .get(owner_name)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        candidates.retain(|candidate| {
            let mut chain = HashSet::from([candidate.clone()]);
            for _ in 0..=index {
                chain = chain
                    .iter()
                    .flat_map(|node| parents.get(node).into_iter().flatten().cloned())
                    .collect();
            }
            !chain.is_disjoint(&owners)
        });
    }
    (candidates.len() == 1).then(|| candidates.remove(0))
}
pub fn resolve_markdown_mentions(
    per_file: &[Value],
    all_nodes: &[Value],
    all_edges: &mut Vec<Value>,
) {
    resolve_markdown_mentions_any(per_file, all_nodes, all_edges);
}
/// [`resolve_markdown_mentions`] over the per-file results and the edges in
/// either form.
pub fn resolve_markdown_mentions_any<P: PerFile, E: Rec>(
    per_file: &[P],
    all_nodes: &[Value],
    all_edges: &mut Vec<E>,
) {
    let mentions = markdown_raw_calls_any(per_file);
    if mentions.is_empty() {
        return;
    }
    let mut by_label = Labels::new();
    let mut by_file = HashMap::<String, Labels>::new();
    for node in all_nodes.iter().filter(|node| {
        truthy(&node["id"]) && node["file_type"].as_str() == Some("code") && !is_file_node(node)
    }) {
        let label = symbol_label(node);
        if label.is_empty() {
            continue;
        }
        let id = text(node, "id");
        if truthy(&node["_callable"]) {
            by_label.entry(label.clone()).or_default().push(id.clone());
        }
        let file = posix(&text(node, "source_file"));
        if !file.is_empty() {
            by_file
                .entry(file)
                .or_default()
                .entry(label)
                .or_default()
                .push(id);
        }
    }
    let source_files = by_file.keys().cloned().collect();
    let mut parents = Parents::new();
    for edge in all_edges.iter() {
        if CONTAINMENT_RELATIONS.contains(&at(edge, "relation").as_str().unwrap_or("")) {
            parents
                .entry(python_compat::string(at(edge, "target")))
                .or_default()
                .insert(python_compat::string(at(edge, "source")));
        }
    }
    let nodes_by_id: HashMap<String, &Value> = all_nodes
        .iter()
        .filter(|node| truthy(&node["id"]))
        .map(|node| (text(node, "id"), node))
        .collect();
    let mut existing: HashSet<(String, String)> = all_edges
        .iter()
        .filter(|edge| at(*edge, "relation").as_str() == Some("references"))
        .map(|edge| {
            (
                python_compat::string(at(edge, "source")),
                python_compat::string(at(edge, "target")),
            )
        })
        .collect();
    // New edges are appended after the pass; the loop reads none of them.
    let mut added: Vec<E> = vec![];
    for call in mentions {
        let caller = rtext(call, "caller_nid");
        let callee = rtext(call, "callee")
            .trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
            .to_owned();
        if caller.is_empty() || callee.is_empty() || !nodes_by_id.contains_key(&caller) {
            continue;
        }
        let mut names: Vec<String> = at(call, "qualifiers")
            .as_array()
            .into_iter()
            .flatten()
            .map(python_compat::string)
            .collect();
        names.push(callee.clone());
        let (target, confidence, score) = if truthy(at(call, "path")) {
            let Some(file) = match_cited_file(
                &rtext(call, "path"),
                &posix(&rtext(call, "source_file")),
                &source_files,
            ) else {
                continue;
            };
            (
                resolve_chain(&by_file[&file], &names, &parents),
                "EXTRACTED",
                1.0,
            )
        } else {
            if LANGUAGE_BUILTIN_GLOBALS.contains(&callee.as_str()) {
                continue;
            }
            let candidates: Vec<&String> = by_label
                .get(&callee)
                .into_iter()
                .flatten()
                .filter(|id| {
                    let found = evidence(id, &nodes_by_id, &parents);
                    names[..names.len() - 1]
                        .iter()
                        .all(|qualifier| found.contains(qualifier))
                })
                .collect();
            (
                (candidates.len() == 1).then(|| candidates[0].clone()),
                "INFERRED",
                0.95,
            )
        };
        let Some(target) = target else {
            continue;
        };
        if target == caller || !existing.insert((caller.clone(), target.clone())) {
            continue;
        }
        added.push(edge_record(json!({"source":caller,"target":target,"relation":"references",
            "context":call.rec_get("context").unwrap_or(&json!("")),"confidence":confidence,"confidence_score":score,
            "source_file":call.rec_get("source_file").unwrap_or(&json!("")),"source_location":call.rec_get("source_location"),"weight":1.0})));
    }
    append_edges(all_edges, added);
}
