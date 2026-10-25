// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
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
/// `_add`: the `references` edge of a resolved mention, unless the graph has
/// it already; either way the document's citation of `target` is recorded.
fn add_mention<R: Rec, E: Rec>(
    call: &R,
    target: Option<String>,
    (confidence, score): (&str, f64),
    existing: &mut HashSet<(String, String)>,
    added: &mut Vec<E>,
    cited: &mut HashMap<String, HashSet<String>>,
) {
    let caller = rtext(call, "caller_nid");
    let Some(target) = target.filter(|target| *target != caller) else {
        return;
    };
    let document = posix(&rtext(call, "source_file"));
    if existing.insert((caller.clone(), target.clone())) {
        added.push(edge_record(json!({"source":caller,"target":target,"relation":"references",
            "context":call.rec_get("context").unwrap_or(&json!("")),"confidence":confidence,"confidence_score":score,
            "source_file":call.rec_get("source_file").unwrap_or(&json!("")),"source_location":call.rec_get("source_location"),"weight":1.0})));
    }
    cited.entry(document).or_default().insert(target);
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
    // `_method_owner`: the class that owns a node when it is a method, not a
    // function. Parents are a set upstream; the smallest id decides a tie.
    let method_owner = |id: &str| -> Option<String> {
        parents
            .get(id)
            .into_iter()
            .flatten()
            .filter(|parent| {
                nodes_by_id
                    .get(*parent)
                    .is_some_and(|n| truthy(&n["_callable_class"]))
            })
            .min()
            .cloned()
    };
    // `_target`: the node a mention resolves to, and the confidence of the match.
    let resolve = |call: &P::R, names: &[String]| -> (Option<String>, &'static str, f64) {
        if truthy(at(call, "path")) {
            let Some(file) = match_cited_file(
                &rtext(call, "path"),
                &posix(&rtext(call, "source_file")),
                &source_files,
            ) else {
                return (None, "EXTRACTED", 1.0);
            };
            return (
                resolve_chain(&by_file[&file], names, &parents),
                "EXTRACTED",
                1.0,
            );
        }
        let callee = &names[names.len() - 1];
        if LANGUAGE_BUILTIN_GLOBALS.contains(&callee.as_str()) {
            return (None, "INFERRED", 0.95);
        }
        let candidates: Vec<&String> = by_label
            .get(callee)
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
    // Targets each document already cites: owner evidence for a bare method
    // mention in the same document (Graphify 0.9.94, #4609).
    let mut cited: HashMap<String, HashSet<String>> = HashMap::new();
    // New edges are appended after the pass; the loop reads none of them.
    let mut added: Vec<E> = vec![];
    // Pass 1: explicit evidence resolves on its own (a cited path, a dotted
    // qualifier chain, or a trailing `()`). Pass 2 takes the bare,
    // parenthesis-less, single-name mentions, where the word alone (`redis`,
    // `data`) is as often prose as a reference.
    let mut bare = vec![];
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
        names.push(callee);
        let span = call
            .rec_get("context")
            .filter(|context| truthy(context))
            .map(python_compat::string)
            .unwrap_or_default();
        if truthy(at(call, "path")) || names.len() > 1 || span.contains('(') {
            let (target, confidence, score) = resolve(call, &names);
            add_mention(
                call,
                target,
                (confidence, score),
                &mut existing,
                &mut added,
                &mut cited,
            );
        } else {
            bare.push((call, names));
        }
    }
    // Pass 2a: bare mentions that name no method (a class or free function is
    // specific enough), so the classes a method mention needs are on record.
    let mut methods = vec![];
    for (call, names) in bare {
        let (target, confidence, score) = resolve(call, &names);
        match target.as_deref().and_then(method_owner) {
            Some(owner) => methods.push((call, target, owner, confidence, score)),
            None => add_mention(
                call,
                target,
                (confidence, score),
                &mut existing,
                &mut added,
                &mut cited,
            ),
        }
    }
    // Pass 2b: a bare method name is evidence only when the same document
    // already cites the class that owns it.
    for (call, target, owner, confidence, score) in methods {
        let document = posix(&rtext(call, "source_file"));
        if cited
            .get(&document)
            .is_some_and(|targets| targets.contains(&owner))
        {
            add_mention(
                call,
                target,
                (confidence, score),
                &mut existing,
                &mut added,
                &mut cited,
            );
        }
    }
    append_edges(all_edges, added);
}
