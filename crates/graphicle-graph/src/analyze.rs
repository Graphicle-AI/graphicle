// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/analyze.py
//! Port of graphify/analyze.py: ranked entities, structural questions, changes,
//! and file-level import cycles. Source attribution remains on original nodes.

#[derive(Clone, Copy)]
pub struct SurpriseContext<'a> {
    pub node_community: &'a [Option<i64>],
    pub u_source: &'a str,
    pub v_source: &'a str,
    pub degrees: Option<&'a [usize]>,
}
use crate::{
    Result,
    analyze_data::*,
    build_labels,
    cluster::{self, Communities, Partition},
    graph::{Edge, Graph},
    graph_algorithms, path_syntax, python_compat,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
/// Python `==` between two JSON values: numbers and booleans by value
/// (`1 == 1.0 == True`), lists and dicts element by element.
fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| py_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, a)| y.get(k).is_some_and(|b| py_eq(a, b)))
        }
        _ => python_compat::scalar_key(a)
            .zip(python_compat::scalar_key(b))
            .is_some_and(|(a, b)| a == b),
    }
}
fn get(attrs: &impl crate::record::Properties, name: &str, default: &str) -> Value {
    attrs.get(name).cloned().unwrap_or_else(|| json!(default))
}
fn label(graph: &Graph, node: usize) -> Value {
    graph.nodes[node]
        .attributes
        .get("label")
        .unwrap_or(&graph.nodes[node].id)
        .clone()
}
fn source(graph: &Graph, node: usize) -> Value {
    get(&graph.nodes[node].attributes, "source_file", "")
}
fn as_text(value: &Value) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| "TypeError: expected string attribute".into())
}
fn text_or_empty(value: &Value) -> Result<&str> {
    if truthy(value) {
        as_text(value)
    } else {
        Ok("")
    }
}
fn display(value: &Value) -> String {
    python_compat::string(value)
}
pub fn degree(graph: &Graph, node: usize) -> usize {
    if graph.directed {
        graph.out_edges(node).len() + graph.in_edges(node).len()
    } else {
        graph.out_edges(node).len()
            + graph.number_of_edges(&graph.nodes[node].id, &graph.nodes[node].id)
    }
}
fn take(mut rows: Vec<Value>, top_n: i64) -> Vec<Value> {
    let length = if top_n < 0 {
        rows.len().saturating_sub(top_n.unsigned_abs() as usize)
    } else {
        (top_n as usize).min(rows.len())
    };
    rows.truncate(length);
    rows
}
pub fn cross_language(a: &str, b: &str) -> bool {
    fn family(path: &str) -> Option<&'static str> {
        let parts = path_syntax::posix_parts(path);
        let ext = path_syntax::suffix(path_syntax::posix_name(&parts)).to_lowercase();
        match ext.as_str() {
            ".py" | ".pyw" => Some("python"),
            ".js" | ".jsx" | ".mjs" | ".cjs" | ".ejs" | ".ts" | ".tsx" | ".mts" | ".cts"
            | ".vue" | ".svelte" => Some("js"),
            ".go" => Some("go"),
            ".rs" => Some("rust"),
            ".java" | ".kt" | ".kts" | ".scala" => Some("jvm"),
            ".c" | ".h" | ".cpp" | ".cc" | ".cxx" | ".hpp" => Some("c"),
            ".rb" | ".rake" => Some("ruby"),
            ".swift" => Some("swift"),
            ".cs" | ".vb" => Some("dotnet"),
            ".php" => Some("php"),
            ".r" => Some("r"),
            // Graphify 0.9.66 language families (additive; only reachable for
            // files with these extensions).
            ".cbl" | ".cob" | ".cobol" | ".cpy" => Some("cobol"),
            ".sol" => Some("solidity"),
            ".erl" | ".hrl" | ".escript" => Some("erlang"),
            _ => None,
        }
    }
    matches!((family(a),family(b)),(Some(a),Some(b)) if a!=b)
}
pub fn file_category(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, s)| format!(".{}", s.to_lowercase()))
        .unwrap_or_default();
    if CODE_EXTENSIONS.contains(&ext.as_str()) {
        "code"
    } else if PAPER_EXTENSIONS.contains(&ext.as_str()) {
        "paper"
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        "image"
    } else {
        "doc"
    }
}
/// First path component, used to detect cross-repo edges. A root-level file
/// (no separator) is in the root itself, `"."`, which keeps it distinct from an
/// out-of-root absolute path (first component `""`). Backslashes count as
/// separators and leading `./` segments are ignored.
pub fn top_level_dir(path: &str) -> &str {
    let mut rest = path;
    while let Some(stripped) = rest.strip_prefix("./").or_else(|| rest.strip_prefix(".\\")) {
        rest = stripped;
    }
    match rest.find(['/', '\\']) {
        Some(end) => &rest[..end],
        None => ".",
    }
}
/// Whether two nodes share `G.nodes[n].get("repo")`: `merge-graphs` / global
/// add tag nodes with their repo, so it is part of the cross-repo key (absent,
/// i.e. `None`, on a single-repo graph).
fn same_repo(graph: &Graph, u: usize, v: usize) -> bool {
    let repo = |node: usize| {
        graph.nodes()[node]
            .attributes
            .get("repo")
            .filter(|value| !value.is_null())
    };
    match (repo(u), repo(v)) {
        (None, None) => true,
        (Some(a), Some(b)) => match (
            crate::python_compat::scalar_key(a),
            crate::python_compat::scalar_key(b),
        ) {
            (Some(x), Some(y)) => x == y,
            _ => a == b,
        },
        _ => false,
    }
}
pub fn node_community_map(communities: &Communities) -> Partition {
    let mut registry = Graph::new(false, false);
    let mut assignments = Vec::new();
    for (cid, nodes) in communities {
        for node in nodes {
            if let Some(i) = registry.node_index(node) {
                assignments[i] = (*cid, node.clone());
            } else if let Ok(i) = registry.add_node(node.clone(), Map::new()) {
                debug_assert_eq!(i, assignments.len());
                assignments.push((*cid, node.clone()));
            }
        }
    }
    assignments.into_iter().map(|(c, n)| (n, c)).collect()
}
fn assignments(graph: &Graph, communities: &Communities) -> Vec<Option<i64>> {
    let mut result = vec![None; graph.nodes.len()];
    for (c, nodes) in communities {
        for node in nodes {
            if let Some(i) = graph.node_index(node) {
                result[i] = Some(*c);
            }
        }
    }
    result
}
pub fn is_file_node(graph: &Graph, node: usize) -> Result<bool> {
    let attrs = &graph.nodes[node].attributes;
    let label = get(attrs, "label", "");
    if !truthy(&label) {
        return Ok(false);
    }
    let source = get(attrs, "source_file", "");
    if truthy(&source) && build_labels::is_file_node_label(&label, &source) {
        return Ok(true);
    }
    let label = as_text(&label)?;
    Ok((label.starts_with('.') && label.ends_with("()"))
        || (label.ends_with("()") && degree(graph, node) <= 1))
}
pub fn is_concept_node(graph: &Graph, node: usize) -> Result<bool> {
    let source = source(graph, node);
    if !truthy(&source) {
        return Ok(true);
    }
    Ok(!as_text(&source)?
        .rsplit('/')
        .next()
        .unwrap_or("")
        .contains('.'))
}
/// AST declarations (type aliases, enum members, local consts, JSON keys)
/// whose only edge is the structural `contains` from their declaring file are
/// working as designed, not documentation gaps (#4205). The edge is read the
/// way `G.edges(node)` lists it: every incident edge of an undirected graph,
/// the out-edges of a directed one.
pub fn is_contains_only_ast_node(graph: &Graph, node: usize) -> bool {
    let attrs = &graph.nodes[node].attributes;
    let mut tier = Map::new();
    for key in ["_origin", "source_location"] {
        if let Some(value) = attrs.get(key) {
            tier.insert(key.into(), value.clone());
        }
    }
    if !crate::build::is_ast_tier(&Value::Object(tier)) || degree(graph, node) != 1 {
        return false;
    }
    let edges = graph.out_edges(node);
    edges.len() == 1 && edges[0].2.attributes.get("relation") == Some(&json!("contains"))
}
pub fn is_json_key_node(graph: &Graph, node: usize) -> Result<bool> {
    let src = source(graph, node);
    if !text_or_empty(&src)?.to_lowercase().ends_with(".json") {
        return Ok(false);
    }
    let label = get(&graph.nodes[node].attributes, "label", "");
    let name = text_or_empty(&label)?
        .trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
        .to_lowercase();
    Ok(JSON_NOISE_LABELS.contains(&name.as_str()))
}
pub fn god_nodes(
    graph: &Graph,
    top_n: i64,
    exclude_hubs_percentile: Option<f64>,
) -> Result<Vec<Value>> {
    let degrees: Vec<_> = (0..graph.nodes.len()).map(|i| degree(graph, i)).collect();
    let mut threshold = None;
    if let Some(percent) = exclude_hubs_percentile
        && !degrees.is_empty()
    {
        if !percent.is_finite() {
            return Err("ValueError: cannot convert nonfinite percentile to integer".into());
        }
        let mut sorted = degrees.clone();
        sorted.sort();
        let i = ((sorted.len() as f64 * percent / 100.0).trunc() - 1.0).max(0.0) as usize;
        threshold = Some(*sorted.get(i).ok_or("IndexError: list index out of range")?);
    }
    let mut nodes: Vec<_> = (0..graph.nodes.len()).collect();
    nodes.sort_by_key(|i| std::cmp::Reverse(degrees[*i]));
    let mut result = Vec::new();
    for node in nodes {
        if threshold.is_some_and(|t| degrees[node] > t)
            || is_file_node(graph, node)?
            || is_concept_node(graph, node)?
            || is_json_key_node(graph, node)?
        {
            continue;
        }
        let name = get(&graph.nodes[node].attributes, "label", "");
        if name.is_array() || name.is_object() {
            return Err("TypeError: unhashable label".into());
        }
        if name
            .as_str()
            .is_some_and(|n| BUILTIN_NOISE_LABELS.contains(&n))
        {
            continue;
        }
        result.push(
            json!({"id":graph.nodes[node].id,"label":label(graph,node),"degree":degrees[node]}),
        );
        if result.len() as i64 >= top_n {
            break;
        }
    }
    Ok(result)
}
pub fn surprise_score(
    graph: &Graph,
    u: usize,
    v: usize,
    data: &impl crate::record::Properties,
    context: SurpriseContext<'_>,
) -> (i64, Vec<String>) {
    let SurpriseContext {
        node_community,
        u_source,
        v_source,
        degrees,
    } = context;

    let confidence = get(data, "confidence", "EXTRACTED");
    let relation = get(data, "relation", "");
    let conf = confidence.as_str().unwrap_or("");
    let relation = relation.as_str().unwrap_or("");
    let (cat_u, cat_v) = (file_category(u_source), file_category(v_source));
    let suppress = conf == "INFERRED"
        && matches!(relation, "calls" | "uses")
        && (cross_language(u_source, v_source)
            || matches!((cat_u, cat_v), ("code", "doc") | ("doc", "code")));
    let mut score = if suppress {
        0
    } else {
        match conf {
            "AMBIGUOUS" => 3,
            "INFERRED" => 2,
            _ => 1,
        }
    };
    let mut reasons = Vec::new();
    if matches!(conf, "AMBIGUOUS" | "INFERRED") {
        reasons.push(format!(
            "{} connection - not explicitly stated in source",
            conf.to_lowercase()
        ));
    }
    if cat_u != cat_v && !suppress {
        score += 2;
        reasons.push(format!("crosses file types ({cat_u} ↔ {cat_v})"));
    }
    let same_top = same_repo(graph, u, v) && top_level_dir(u_source) == top_level_dir(v_source);
    if !same_top && !suppress {
        score += 2;
        reasons.push("connects across different repos/directories".into());
    }
    if node_community[u].is_some()
        && node_community[v].is_some()
        && node_community[u] != node_community[v]
        && !suppress
    {
        score += 1;
        reasons.push("bridges separate communities".into());
    }
    if relation == "semantically_similar_to" {
        score = score * 3 / 2;
        reasons.push("semantically similar concepts with no structural link".into());
    }
    let a = degrees.map_or_else(|| degree(graph, u), |d| d[u]);
    let b = degrees.map_or_else(|| degree(graph, v), |d| d[v]);
    if a.min(b) <= 2 && a.max(b) >= 5 {
        score += 1;
        let (peripheral, hub) = if a <= 2 { (u, v) } else { (v, u) };
        reasons.push(format!(
            "peripheral node `{}` unexpectedly reaches hub `{}`",
            display(&label(graph, peripheral)),
            display(&label(graph, hub))
        ));
    }
    (score, reasons)
}
fn endpoints(graph: &Graph, u: usize, v: usize, edge: &Edge) -> (usize, usize) {
    (
        edge.attributes
            .get("_src")
            .and_then(|n| graph.node_index(n))
            .unwrap_or(u),
        edge.attributes
            .get("_tgt")
            .and_then(|n| graph.node_index(n))
            .unwrap_or(v),
    )
}
fn edge_result(graph: &Graph, u: usize, v: usize, data: &impl crate::record::Properties) -> Value {
    json!({"source":label(graph,u),"target":label(graph,v),"source_files":[source(graph,u),source(graph,v)],"confidence":get(data,"confidence","EXTRACTED"),"relation":get(data,"relation","")})
}
pub fn cross_file_surprises(
    graph: &Graph,
    communities: &Communities,
    top_n: i64,
) -> Result<Vec<Value>> {
    let assignment = assignments(graph, communities);
    let degrees: Vec<_> = (0..graph.nodes.len()).map(|i| degree(graph, i)).collect();
    let mut candidates = Vec::new();
    for (u, v, edge) in graph.iter_edges() {
        if matches!(
            edge.attributes.get("relation").and_then(Value::as_str),
            Some("imports" | "imports_from" | "contains" | "method")
        ) {
            continue;
        }
        if is_concept_node(graph, u)?
            || is_concept_node(graph, v)?
            || is_file_node(graph, u)?
            || is_file_node(graph, v)?
        {
            continue;
        }
        let (u_source, v_source) = (source(graph, u), source(graph, v));
        if !truthy(&u_source) || !truthy(&v_source) || u_source == v_source {
            continue;
        }
        let (score, reasons) = surprise_score(
            graph,
            u,
            v,
            &edge.attributes,
            SurpriseContext {
                node_community: &assignment,
                u_source: as_text(&u_source)?,
                v_source: as_text(&v_source)?,
                degrees: Some(&degrees),
            },
        );
        let (src, tgt) = endpoints(graph, u, v, edge);
        let mut record = edge_result(graph, src, tgt, &edge.attributes);
        record["why"] = json!(if reasons.is_empty() {
            "cross-file semantic connection".into()
        } else {
            reasons.join("; ")
        });
        candidates.push((score, record));
    }
    candidates.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    if candidates.is_empty() {
        return cross_community_surprises(graph, communities, top_n);
    }
    Ok(take(
        candidates.into_iter().map(|(_, r)| r).collect(),
        top_n,
    ))
}
pub fn cross_community_surprises(
    graph: &Graph,
    communities: &Communities,
    top_n: i64,
) -> Result<Vec<Value>> {
    if communities.is_empty() {
        if graph.edge_count() == 0 || graph.nodes.len() > 5000 {
            return Ok(vec![]);
        }
        let (_, mut scores) = graph_algorithms::centrality(graph, None, true);
        scores.sort_by(|a, b| b.2.total_cmp(&a.2));
        let keep = if top_n < 0 {
            scores.len().saturating_sub(top_n.unsigned_abs() as usize)
        } else {
            (top_n as usize).min(scores.len())
        };
        scores.truncate(keep);
        // Upstream unpacks (u,v) from the keyed MultiGraph BC result. Retain its
        // explicit failure rather than silently dropping keys and claiming parity.
        if graph.multigraph && !scores.is_empty() {
            return Err("ValueError: too many values to unpack (expected 2)".into());
        }
        let mut result = Vec::new();
        for (u, v, score) in scores {
            let edge = graph.edges_between(&graph.nodes[u].id, &graph.nodes[v].id)[0];
            let mut row = edge_result(graph, u, v, &edge.attributes);
            row["note"] = json!(format!("Bridges graph structure (betweenness={score:.3})"));
            result.push(row);
        }
        return Ok(result);
    }
    let assignment = assignments(graph, communities);
    let mut surprises = Vec::new();
    for (u, v, edge) in graph.iter_edges() {
        let (Some(a), Some(b)) = (assignment[u], assignment[v]) else {
            continue;
        };
        if a == b {
            continue;
        }
        if is_file_node(graph, u)? || is_file_node(graph, v)? {
            continue;
        }
        if matches!(
            edge.attributes.get("relation").and_then(Value::as_str),
            Some("imports" | "imports_from" | "contains" | "method")
        ) {
            continue;
        }
        let (src, tgt) = endpoints(graph, u, v, edge);
        let mut row = edge_result(graph, src, tgt, &edge.attributes);
        row["note"] = json!(format!("Bridges community {a} → community {b}"));
        let order = match row["confidence"].as_str() {
            Some("AMBIGUOUS") => 0,
            Some("INFERRED") => 1,
            Some("EXTRACTED") => 2,
            _ => 3,
        };
        surprises.push((order, (a.min(b), a.max(b)), row));
    }
    surprises.sort_by_key(|(order, _, _)| *order);
    let mut seen = HashSet::new();
    Ok(take(
        surprises
            .into_iter()
            .filter_map(|(_, pair, row)| seen.insert(pair).then_some(row))
            .collect(),
        top_n,
    ))
}
/// Upstream's default `top_n` for [`surprising_connections`]. `extract`,
/// `update`/watch rebuilds and `cluster-only` call it with the default; only
/// the MCP server asks for 10.
pub const SURPRISES_TOP_N: i64 = 5;
/// Upstream's default `top_n` for [`suggest_questions`] (the rebuild and
/// `cluster-only` reports; the MCP server asks for 10).
pub const QUESTIONS_TOP_N: i64 = 7;
pub fn surprising_connections(
    graph: &Graph,
    communities: &Communities,
    top_n: i64,
) -> Result<Vec<Value>> {
    let mut sources = BTreeSet::new();
    for i in 0..graph.nodes.len() {
        let s = source(graph, i);
        if truthy(&s) {
            sources.insert(display(&s));
        }
    }
    if sources.len() > 1 {
        cross_file_surprises(graph, communities, top_n)
    } else {
        cross_community_surprises(graph, communities, top_n)
    }
}
fn community_label(labels: &Map<String, Value>, cid: Option<i64>) -> Value {
    let name = cid.map(|c| c.to_string()).unwrap_or_else(|| "None".into());
    labels
        .get(&name)
        .cloned()
        .unwrap_or_else(|| json!(format!("Community {name}")))
}
fn quoted_names(values: impl Iterator<Item = Value>) -> String {
    values
        .map(|v| format!("`{}`", display(&v)))
        .collect::<Vec<_>>()
        .join(", ")
}
pub fn suggest_questions(
    graph: &Graph,
    communities: &Communities,
    labels: &Map<String, Value>,
    top_n: i64,
) -> Result<Vec<Value>> {
    let assignment = assignments(graph, communities);
    let mut questions = Vec::new();
    // Tagged low_confidence (#4199): the report lists these under their own
    // "Low-confidence Hints" heading, away from EXTRACTED/INFERRED bridges.
    for (u, v, edge) in graph.iter_edges() {
        if edge.attributes.get("confidence") == Some(&json!("AMBIGUOUS")) {
            questions.push(json!({"type":"ambiguous_edge","question":format!("What is the exact relationship between `{}` and `{}`?",display(&label(graph,u)),display(&label(graph,v))),"why":format!("Edge tagged AMBIGUOUS (relation: {}) - confidence is low.",display(&get(&edge.attributes,"relation","related to"))),"low_confidence":true}));
        }
    }
    if graph.edge_count() > 0 {
        let sample = (graph.nodes.len() > 1000).then_some(100);
        let (between, _) = graph_algorithms::centrality(graph, sample, false);
        let mut bridges = Vec::new();
        for (i, &score) in between.iter().enumerate() {
            if !is_file_node(graph, i)? && !is_concept_node(graph, i)? && score > 0.0 {
                bridges.push((i, score));
            }
        }
        bridges.sort_by(|a, b| b.1.total_cmp(&a.1));
        bridges.truncate(3);
        for (node, score) in bridges {
            let cid = assignment[node];
            let own = if cid.is_some() {
                community_label(labels, cid)
            } else {
                json!("unknown")
            };
            // Upstream lists the neighbouring communities in the order of the
            // `set` it collects them in (ints and None, so a fixed order).
            let other = python_compat::int_set_order(
                graph
                    .out_edges(node)
                    .into_iter()
                    .map(|(_, neighbor, _)| assignment[neighbor])
                    .filter(|c| *c != cid),
            );
            if !other.is_empty() {
                questions.push(json!({"type":"bridge_node","question":format!("Why does `{}` connect `{}` to {}?",display(&label(graph,node)),display(&own),quoted_names(other.into_iter().map(|c|community_label(labels,c)))),"why":format!("High betweenness centrality ({score:.3}) - this node is a cross-community bridge.")}));
            }
        }
    }
    let mut top = Vec::new();
    for node in 0..graph.nodes.len() {
        if !is_file_node(graph, node)? {
            top.push((node, degree(graph, node)));
        }
    }
    top.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    top.truncate(5);
    for (node, _) in top {
        let inferred: Vec<_> = graph
            .out_edges(node)
            .into_iter()
            .filter(|(_, _, e)| e.attributes.get("confidence") == Some(&json!("INFERRED")))
            .collect();
        if inferred.len() >= 2 {
            let mut others = Vec::new();
            for (u, v, edge) in inferred.iter().take(2) {
                let (src, tgt) = endpoints(graph, *u, *v, edge);
                others.push(label(graph, if src == node { tgt } else { src }));
            }
            let name = display(&label(graph, node));
            let count = inferred.len();
            questions.push(json!({"type":"verify_inferred","question":format!("Are the {count} inferred relationships involving `{name}` (e.g. with `{}` and `{}`) actually correct?",display(&others[0]),display(&others[1])),"why":format!("`{name}` has {count} INFERRED edges - model-reasoned connections that need verification.")}));
        }
    }
    let mut isolated = Vec::new();
    for i in 0..graph.nodes.len() {
        if degree(graph, i) <= 1
            && !is_file_node(graph, i)?
            && !is_concept_node(graph, i)?
            && !is_contains_only_ast_node(graph, i)
            && graph.nodes[i].attributes.get("file_type") != Some(&json!("rationale"))
        {
            isolated.push(i);
        }
    }
    if !isolated.is_empty() {
        questions.push(json!({"type":"isolated_nodes","question":format!("What connects {} to the rest of the system?",quoted_names(isolated.iter().take(3).map(|i|label(graph,*i)))),"why":format!("{} weakly-connected nodes found - possible documentation gaps or missing edges.",isolated.len())}));
    }
    for (cid, nodes) in communities {
        let score = cluster::cohesion_score(graph, nodes);
        if score < 0.15 && nodes.len() >= 5 {
            questions.push(json!({"type":"low_cohesion","question":format!("Should `{}` be split into smaller, more focused modules?",display(&community_label(labels,Some(*cid)))),"why":format!("Cohesion score {} - nodes in this community are weakly interconnected.",display(&json!(score)))}));
        }
    }
    if questions.is_empty() {
        return Ok(vec![
            json!({"type":"no_signal","question":null,"why":"Not enough signal to generate questions. This usually means the corpus has no AMBIGUOUS edges, no bridge nodes, no INFERRED relationships, and all communities are tightly cohesive. Add more files or run with --mode deep to extract richer edges."}),
        ]);
    }
    Ok(take(diversify_by_type(questions), top_n))
}

/// Round-robin question types (in first-seen order) so an abundant early type
/// cannot crowd later signals out of the `top_n` cut; order within a type is kept.
fn diversify_by_type(questions: Vec<Value>) -> Vec<Value> {
    let mut by_type: Vec<(Value, std::collections::VecDeque<Value>)> = vec![];
    for question in questions {
        let kind = question.get("type").cloned().unwrap_or(Value::Null);
        match by_type.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, rows)) => rows.push_back(question),
            None => by_type.push((kind, [question].into())),
        }
    }
    let mut out = vec![];
    while by_type.iter().any(|(_, rows)| !rows.is_empty()) {
        for (_, rows) in &mut by_type {
            out.extend(rows.pop_front());
        }
    }
    out
}
pub fn graph_diff(old: &Graph, new: &Graph) -> Result<Value> {
    let added_nodes: Vec<_> = new
        .nodes
        .iter()
        .filter(|n| old.node_index(&n.id).is_none())
        .map(|n| json!({"id":n.id,"label":n.attributes.get("label").unwrap_or(&n.id)}))
        .collect();
    let removed_nodes: Vec<_> = old
        .nodes
        .iter()
        .filter(|n| new.node_index(&n.id).is_none())
        .map(|n| json!({"id":n.id,"label":n.attributes.get("label").unwrap_or(&n.id)}))
        .collect();
    let mut registry = Graph::new(false, false);
    fn edge_keys(graph: &Graph, registry: &mut Graph) -> Result<Vec<usize>> {
        graph
            .iter_edges()
            .map(|(u, v, e)| {
                let mut pair = vec![graph.nodes[u].id.clone(), graph.nodes[v].id.clone()];
                if !graph.directed {
                    pair = cluster::sorted_ids(&pair)?;
                }
                let relation = get(&e.attributes, "relation", "");
                if relation.is_array() || relation.is_object() {
                    return Err("TypeError: unhashable edge relation".into());
                }
                registry.add_node(json!([pair[0], pair[1], relation]), Map::new())
            })
            .collect()
    }
    let old_keys = edge_keys(old, &mut registry)?;
    let new_keys = edge_keys(new, &mut registry)?;
    // Stored direction of an undirected edge (`_src`/`_tgt`, both present),
    // per key; the last edge with a key sets it, as the upstream dict does.
    fn edge_dirs<'a>(graph: &'a Graph, keys: &[usize]) -> HashMap<usize, Option<[&'a Value; 2]>> {
        graph
            .iter_edges()
            .zip(keys)
            .map(|((_, _, e), key)| {
                let dir = e
                    .attributes
                    .get("_src")
                    .zip(e.attributes.get("_tgt"))
                    .map(|(s, t)| [s, t]);
                (*key, dir)
            })
            .collect()
    }
    let old_dirs = edge_dirs(old, &old_keys);
    let new_dirs = edge_dirs(new, &new_keys);
    // A reversed edge (a->b became b->a) keeps its undirected key. Report it as
    // removed + added only when both snapshots carry _src/_tgt and they differ,
    // so graphs without the markers diff exactly as before (#4067).
    let reversed: HashSet<usize> = old_dirs
        .iter()
        .filter_map(|(key, old_dir)| {
            let (a, b) = ((*old_dir)?, (*new_dirs.get(key)?)?);
            (!(py_eq(a[0], b[0]) && py_eq(a[1], b[1]))).then_some(*key)
        })
        .collect();
    // Keys kept by both snapshots (reversed ones aside) are the unchanged edges.
    let unchanged =
        |key: &usize, other: &HashMap<usize, _>| other.contains_key(key) && !reversed.contains(key);
    fn changes(graph: &Graph, keys: &[usize], unchanged: impl Fn(&usize) -> bool) -> Vec<Value> {
        graph.iter_edges().zip(keys).filter(|(_,key)|!unchanged(key)).map(|((u,v,e),_)|json!({"source":e.attributes.get("_src").unwrap_or(&graph.nodes[u].id),"target":e.attributes.get("_tgt").unwrap_or(&graph.nodes[v].id),"relation":get(&e.attributes,"relation",""),"confidence":get(&e.attributes,"confidence","")})).collect()
    }
    let added_edges = changes(new, &new_keys, |key| unchanged(key, &old_dirs));
    let removed_edges = changes(old, &old_keys, |key| unchanged(key, &new_dirs));
    let mut parts = Vec::new();
    for (count, noun, suffix) in [
        (added_nodes.len(), "new node", ""),
        (added_edges.len(), "new edge", ""),
        (removed_nodes.len(), "node", " removed"),
        (removed_edges.len(), "edge", " removed"),
    ] {
        if count > 0 {
            parts.push(format!(
                "{count} {noun}{}{suffix}",
                if count == 1 { "" } else { "s" }
            ));
        }
    }
    Ok(
        json!({"new_nodes":added_nodes,"removed_nodes":removed_nodes,"new_edges":added_edges,"removed_edges":removed_edges,"summary":if parts.is_empty(){"no changes".into()}else{parts.join(", ")}}),
    )
}
pub fn find_import_cycles(graph: &Graph, max_cycle_length: i64, top_n: i64) -> Result<Vec<Value>> {
    let mut files = Graph::new(true, false);
    for (u, v, edge) in graph.iter_edges() {
        let a = &edge.attributes;
        if !matches!(
            a.get("relation").and_then(Value::as_str),
            Some("imports_from" | "re_exports")
        ) || a.get("deferred").is_some_and(truthy)
            || a.get("type_only").is_some_and(truthy)
        {
            continue;
        }
        let Some(src) = a
            .get("source_file")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let uf = graph.nodes[u]
            .attributes
            .get("source_file")
            .and_then(Value::as_str)
            .unwrap_or("");
        let vf = graph.nodes[v]
            .attributes
            .get("source_file")
            .and_then(Value::as_str)
            .unwrap_or("");
        let target = if uf == src {
            vf
        } else if vf == src {
            uf
        } else if !vf.is_empty() && vf != src {
            vf
        } else {
            uf
        };
        if !target.is_empty() {
            files.add_edge(json!(src), json!(target), None, Map::new())?;
        }
    }
    if files.edge_count() == 0 {
        return Ok(vec![]);
    }
    if max_cycle_length < 0 {
        return Err("ValueError: length bound must be non-negative".into());
    }
    let cap = top_n.saturating_mul(10).max(1) as usize;
    // networkx picks pivots through sets of these paths; upstream's CLI
    // hashes them with PYTHONHASHSEED=0 (see `directed_cycles`).
    let hashes: Vec<i64> = files
        .nodes
        .iter()
        .map(|node| python_compat::str_hash_seed0(node.id.as_str().unwrap_or_default()))
        .collect();
    let mut cycles = graph_algorithms::directed_cycles(
        &graph_algorithms::neighbors(&files),
        &hashes,
        max_cycle_length as usize,
        cap,
    );
    cycles.sort_by_key(Vec::len);
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for cycle in cycles {
        let mut names: Vec<_> = cycle
            .iter()
            .map(|i| files.nodes[*i].id.as_str().unwrap().to_string())
            .collect();
        let start = names.iter().enumerate().min_by_key(|(_, s)| *s).unwrap().0;
        names.rotate_left(start);
        if seen.insert(names.clone()) {
            result.push(json!({"cycle":names,"length":names.len(),"why":"circular dependency"}));
            if result.len() as i64 >= top_n {
                break;
            }
        }
    }
    Ok(result)
}
pub fn dispatch(request: &Value) -> Result<Value> {
    dispatch_with_graph(request, None)
}
pub(crate) fn dispatch_owned(mut request: Value) -> Result<Value> {
    let graph = request.get_mut("graph").map(Value::take);
    dispatch_with_graph(&request, graph)
}
fn dispatch_with_graph(request: &Value, graph: Option<Value>) -> Result<Value> {
    let operation = request["operation"].as_str().ok_or("Expected operation")?;
    if operation == "analyze.file_category" {
        return Ok(json!(file_category(
            request["path"].as_str().ok_or("Expected path")?
        )));
    }
    if operation == "analyze.top_level_dir" {
        return Ok(json!(top_level_dir(
            request["path"].as_str().ok_or("Expected path")?
        )));
    }
    if operation == "analyze.cross_language" {
        return Ok(json!(cross_language(
            request["a"].as_str().ok_or("Expected path")?,
            request["b"].as_str().ok_or("Expected path")?
        )));
    }
    let communities =
        cluster::communities_from_json(request.get("communities").unwrap_or(&json!({})))?;
    if operation == "analyze.node_community_map" {
        return Ok(json!(node_community_map(&communities)));
    }
    let mut graph = match graph {
        Some(data) => Graph::from_node_link_owned(data, "edges")?,
        None => Graph::from_node_link(&request["graph"], "edges")?,
    };
    if let Some(order) = request.get("neighbor_order") {
        graph.restore_neighbor_order(order)?;
    }
    let top = |default| {
        request
            .get("top_n")
            .and_then(Value::as_i64)
            .unwrap_or(default)
    };
    match operation {
        "analyze.god_nodes" => Ok(json!(god_nodes(
            &graph,
            top(10),
            request
                .get("exclude_hubs_percentile")
                .and_then(Value::as_f64)
        )?)),
        "analyze.is_file" | "analyze.is_concept" | "analyze.is_json_key" => {
            let node = graph
                .node_index(&request["node"])
                .ok_or("KeyError: node not found")?;
            Ok(json!(match operation {
                "analyze.is_file" => is_file_node(&graph, node)?,
                "analyze.is_concept" => is_concept_node(&graph, node)?,
                _ => is_json_key_node(&graph, node)?,
            }))
        }
        "analyze.surprises" => Ok(json!(surprising_connections(
            &graph,
            &communities,
            top(SURPRISES_TOP_N)
        )?)),
        "analyze.cross_file" => Ok(json!(cross_file_surprises(&graph, &communities, top(5))?)),
        "analyze.cross_community" => Ok(json!(cross_community_surprises(
            &graph,
            &communities,
            top(5)
        )?)),
        "analyze.surprise_score" => {
            let u = graph
                .node_index(&request["u"])
                .ok_or("Expected source node")?;
            let v = graph
                .node_index(&request["v"])
                .ok_or("Expected target node")?;
            let mut assignment = vec![None; graph.nodes.len()];
            let mapping: Partition = serde_json::from_value(request["node_community"].clone())
                .map_err(crate::Error::from)?;
            for (n, c) in mapping {
                if let Some(i) = graph.node_index(&n) {
                    assignment[i] = Some(c);
                }
            }
            let mut supplied = vec![0; graph.nodes.len()];
            let use_degrees = request.get("degrees").filter(|v| !v.is_null());
            if let Some(rows) = use_degrees {
                for row in rows.as_array().ok_or("Expected degree entries")? {
                    if let Some(i) = graph.node_index(&row[0]) {
                        supplied[i] = row[1].as_u64().ok_or("Expected degree")? as usize;
                    }
                }
            }
            Ok(json!(surprise_score(
                &graph,
                u,
                v,
                request["data"].as_object().ok_or("Expected edge data")?,
                SurpriseContext {
                    node_community: &assignment,
                    u_source: request["u_source"].as_str().ok_or("Expected source file")?,
                    v_source: request["v_source"].as_str().ok_or("Expected target file")?,
                    degrees: use_degrees.map(|_| supplied.as_slice()),
                }
            )))
        }
        "analyze.questions" => Ok(json!(suggest_questions(
            &graph,
            &communities,
            request["community_labels"]
                .as_object()
                .ok_or("Expected labels")?,
            top(QUESTIONS_TOP_N)
        )?)),
        "analyze.diff" => graph_diff(
            &graph,
            &Graph::from_node_link(&request["new_graph"], "edges")?,
        ),
        "analyze.cycles" => Ok(json!(find_import_cycles(
            &graph,
            request
                .get("max_cycle_length")
                .and_then(Value::as_i64)
                .unwrap_or(5),
            top(20)
        )?)),
        "analyze.centrality" => {
            let (nodes, edges) = graph_algorithms::centrality(
                &graph,
                request
                    .get("sample")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize),
                true,
            );
            Ok(
                json!({"nodes":nodes.iter().enumerate().map(|(i,v)|json!([graph.nodes[i].id,v])).collect::<Vec<_>>(),"edges":edges.iter().map(|(u,v,s)|json!([graph.nodes[*u].id,graph.nodes[*v].id,s])).collect::<Vec<_>>()}),
            )
        }
        _ => Err("Unsupported graph analysis operation".into()),
    }
}
