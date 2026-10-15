// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/affected.py, graphify/ids.py
//! Native translation of graphify/affected.py: reverse dependency traversal.
use crate::{Result, graph::Graph, path_syntax, python_compat::string, unicode15::CASEFOLD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::Path,
};
use unicode_normalization::UnicodeNormalization;

pub const DEFAULT_AFFECTED_RELATIONS: &[&str] = &[
    "calls",
    "indirect_call",
    "references",
    "imports",
    "imports_from",
    "dynamic_import",
    "re_exports",
    "inherits",
    "extends",
    "implements",
    "uses",
    "mixes_in",
    "embeds",
    // `defines`: a file declaring a named symbol. In languages where a
    // symbol's only incoming edge is its file's `defines` (Dart, Python, Go,
    // ...), every other relation targets the file node, so the reverse walk
    // follows `defines` back to the file and then its importers (#4288).
    "defines",
    "requires",
];
/// Edges from a type to the types it derives from. A method declared again in
/// a subtype overrides the supertype's same-named member.
const SUPERTYPE_RELATIONS: [&str; 3] = ["inherits", "extends", "implements"];
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AffectedHit {
    pub node_id: String,
    pub depth: i64,
    pub via_relation: String,
    #[serde(default)]
    pub via_file: Option<String>,
    #[serde(default)]
    pub via_location: Option<String>,
}
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
fn text(data: &impl crate::record::Properties, key: &str) -> String {
    data.get(key).map(string).unwrap_or_default()
}
fn nonempty(data: &impl crate::record::Properties, key: &str) -> Option<String> {
    data.get(key).filter(|v| truthy(v)).map(string)
}
pub fn normalize_label(label: &str) -> String {
    let mut result = String::new();
    for c in label.nfc() {
        match CASEFOLD.binary_search_by_key(&c, |entry| entry.0) {
            Ok(i) => result.push_str(CASEFOLD[i].1),
            Err(_) => result.push(c),
        }
    }
    result
}
pub fn bare_name(label: &str) -> String {
    let normalized = normalize_label(label);
    normalized.strip_suffix("()").unwrap_or(&normalized).into()
}
pub fn as_repo_relative(query: &str, root: Option<&Path>) -> Result<String> {
    // Windows: `C:\repo\a.py` (or `\\?\C:\...`) is absolute as well; it is
    // compared in the engine's `/` spelling (`C:/repo/a.py`).
    #[cfg(windows)]
    let spelled = if Path::new(query).is_absolute() {
        path_syntax::windows_posix(query)
    } else {
        query.to_string()
    };
    #[cfg(windows)]
    let query = spelled.as_str();
    let path = path_syntax::posix_parts(query);
    if query.starts_with('/') || (cfg!(windows) && Path::new(query).is_absolute()) {
        let cwd;
        let anchor = match root {
            Some(path) => path,
            None => {
                cwd = std::env::current_dir().map_err(crate::Error::from)?;
                &cwd
            }
        };
        if !anchor.is_absolute() {
            return Ok(query.into());
        }
        let anchor = path_syntax::posix_parts(&crate::fsutil::path_string(anchor));
        if path.starts_with(&anchor) {
            return Ok(path_syntax::posix_join(&path[anchor.len()..]));
        }
        return Ok(query.into());
    }
    Ok(path_syntax::posix_join(&path))
}
pub fn prefer_file_node(graph: &Graph, ids: &[String], query: &str) -> Option<String> {
    let parts = path_syntax::posix_parts(query);
    let basename = normalize_label(path_syntax::posix_name(&parts));
    for pass in 0..3 {
        let candidates: Vec<_> = ids
            .iter()
            .filter(|id| {
                let Some(index) = graph.node_index(&json!(id)) else {
                    return false;
                };
                let data = &graph.nodes[index].attributes;
                let l1 = text(data, "source_location") == "L1";
                let named = normalize_label(&text(data, "label")) == basename;
                match pass {
                    0 => l1 && named,
                    1 => l1,
                    _ => named,
                }
            })
            .collect();
        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }
    }
    None
}
pub fn resolve_seed(graph: &Graph, query: &str, root: Option<&Path>) -> Result<Option<String>> {
    let trimmed = query.trim_end_matches(['/', '\\']);
    let query = if trimmed.is_empty() { query } else { trimmed };
    if graph.node_index(&json!(query)).is_some() {
        return Ok(Some(query.into()));
    }
    let lower = normalize_label(query);
    let bare = bare_name(&lower);
    let mut exact = vec![];
    let mut undecorated = vec![];
    for node in &graph.nodes {
        let label = text(&node.attributes, "label");
        if normalize_label(&label) == lower {
            exact.push(string(&node.id));
        }
        if bare_name(&label) == bare {
            undecorated.push(string(&node.id));
        }
    }
    if exact.len() == 1 {
        return Ok(exact.pop());
    }
    if undecorated.len() == 1 {
        return Ok(undecorated.pop());
    }
    let path = as_repo_relative(query, root)?;
    let path_lower = normalize_label(&path);
    let sources: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| {
            let source = normalize_label(&text(&node.attributes, "source_file"));
            source == lower || source == path_lower
        })
        .map(|node| string(&node.id))
        .collect();
    if sources.len() == 1 {
        return Ok(Some(sources[0].clone()));
    }
    if !sources.is_empty()
        && let Some(id) = prefer_file_node(graph, &sources, &path)
    {
        return Ok(Some(id));
    }
    let contained: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| normalize_label(&text(&node.attributes, "label")).contains(&lower))
        .map(|node| string(&node.id))
        .collect();
    Ok((contained.len() == 1).then(|| contained[0].clone()))
}
/// `G.out_edges(id)` for a node named by its `str()` spelling.
fn out_edges_of<'g>(graph: &'g Graph, id: &str) -> Vec<(usize, usize, &'g crate::graph::Edge)> {
    graph
        .node_index(&json!(id))
        .map(|i| graph.out_edges(i))
        .unwrap_or_default()
}
/// Every type `type_id` derives from, nearest first (cycles cut).
fn supertypes(
    graph: &Graph,
    type_id: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> Vec<String> {
    if let Some(found) = cache.get(type_id) {
        return found.clone();
    }
    let mut found = vec![];
    let mut seen = HashSet::from([type_id.to_string()]);
    let mut frontier = vec![type_id.to_string()];
    while !frontier.is_empty() {
        let mut next = vec![];
        for t in &frontier {
            for (_, base, edge) in out_edges_of(graph, t) {
                let base = string(&graph.nodes[base].id);
                if SUPERTYPE_RELATIONS.contains(&text(&edge.attributes, "relation").as_str())
                    && seen.insert(base.clone())
                {
                    found.push(base.clone());
                    next.push(base);
                }
            }
        }
        frontier = next;
    }
    cache.insert(type_id.into(), found.clone());
    found
}
/// A node's `label` as `dict.get` reads it (absent and null are both None),
/// compared with Python's `==`.
fn same_label(a: Option<&Value>, b: Option<&Value>) -> bool {
    let (a, b) = (a.filter(|v| !v.is_null()), b.filter(|v| !v.is_null()));
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match (
            crate::python_compat::scalar_key(a),
            crate::python_compat::scalar_key(b),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => a == b,
        },
        _ => false,
    }
}
/// The supertype members `node` overrides: same-named methods of the types
/// its class derives from. A call bound to one of them can run `node` at
/// runtime (`visitor.head()` typed `NodeVisitor` runs `FormattingVisitor.head`).
fn overridden_members(
    graph: &Graph,
    node: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> Vec<String> {
    let Some(index) = graph.node_index(&json!(node)) else {
        return vec![];
    };
    let owners: Vec<String> = graph
        .in_edges(index)
        .into_iter()
        .filter(|(_, _, edge)| text(&edge.attributes, "relation") == "method")
        .map(|(owner, _, _)| string(&graph.nodes[owner].id))
        .collect();
    if owners.is_empty() {
        return vec![];
    }
    let label = graph.nodes[index].attributes.get("label");
    let mut found = vec![];
    for owner in owners {
        for base in supertypes(graph, &owner, cache) {
            for (_, member, edge) in out_edges_of(graph, &base) {
                let id = string(&graph.nodes[member].id);
                if text(&edge.attributes, "relation") == "method"
                    && id != node
                    && same_label(graph.nodes[member].attributes.get("label"), label)
                {
                    found.push(id);
                }
            }
        }
    }
    found
}
pub fn affected_nodes(
    graph: &Graph,
    seed: &str,
    relations: &[String],
    depth: i64,
) -> Result<Vec<AffectedHit>> {
    let mut seen = BTreeSet::from([seed.to_string()]);
    let mut queue = VecDeque::from([(seed.to_string(), 0)]);
    let mut hits = vec![];
    let seed_index = graph.node_index(&json!(seed));
    let member_edges = if graph.directed {
        seed_index.map(|i| graph.out_edges(i)).unwrap_or_default()
    } else {
        graph
            .iter_edges()
            .filter(|(source, _, _)| Some(*source) == seed_index)
            .collect()
    };
    for (_, member, edge) in member_edges {
        if !matches!(
            text(&edge.attributes, "relation").as_str(),
            "method" | "contains"
        ) {
            continue;
        }
        let member = string(&graph.nodes[member].id);
        if seen.insert(member.clone()) {
            queue.push_back((member, 0));
        }
    }
    let mut supertype_cache = HashMap::new();
    // Overridden members walked without being reported; one that a real edge
    // also reaches (an overload calling the abstract one) is reported then.
    let mut stepping_stones = HashSet::new();
    while let Some((current, current_depth)) = queue.pop_front() {
        if current_depth >= depth {
            continue;
        }
        // A caller bound to the supertype member a method overrides depends on
        // the override too, so that member's callers are walked at the same
        // depth (#4330). The member itself did not change: like the seed's
        // members above, it is a stepping stone, not a hit. Needs edge
        // direction, so directed graphs only.
        if graph.directed {
            for overridden in overridden_members(graph, &current, &mut supertype_cache) {
                if seen.insert(overridden.clone()) {
                    stepping_stones.insert(overridden.clone());
                    // Front of the queue: same depth as `current`, so it is
                    // walked before the deeper nodes already queued.
                    queue.push_front((overridden, current_depth));
                }
            }
        }
        let index = graph.node_index(&json!(current));
        let incoming = if graph.directed {
            index.map(|i| graph.in_edges(i)).unwrap_or_default()
        } else {
            graph
                .iter_edges()
                .filter(|(_, target, _)| Some(*target) == index)
                .collect()
        };
        for (source, _, edge) in incoming {
            let relation = text(&edge.attributes, "relation");
            if !relations.contains(&relation) {
                continue;
            }
            let source = string(&graph.nodes[source].id);
            // A stepping stone a real edge reaches is reported now; it is
            // already queued.
            let queued = stepping_stones.remove(&source);
            if !queued && !seen.insert(source.clone()) {
                continue;
            }
            hits.push(AffectedHit {
                node_id: source.clone(),
                depth: current_depth + 1,
                via_relation: relation,
                via_file: nonempty(&edge.attributes, "source_file"),
                via_location: nonempty(&edge.attributes, "source_location"),
            });
            if !queued {
                queue.push_back((source, current_depth + 1));
            }
        }
    }
    Ok(hits)
}
pub fn node_label(graph: &Graph, id: &str) -> Result<String> {
    let index = graph
        .node_index(&json!(id))
        .ok_or_else(|| format!("Node not found: {id}"))?;
    Ok(nonempty(&graph.nodes[index].attributes, "label").unwrap_or_else(|| id.into()))
}
pub fn format_location(data: &impl crate::record::Properties) -> String {
    let file = nonempty(data, "source_file").unwrap_or_else(|| "-".into());
    match nonempty(data, "source_location") {
        Some(location) => format!("{file}:{location}"),
        None => file,
    }
}
pub fn format_affected(
    graph: &Graph,
    query: &str,
    relations: &[String],
    depth: i64,
    root: Option<&Path>,
) -> Result<String> {
    let Some(seed) = resolve_seed(graph, query, root)? else {
        return Ok(format!("No unique node match for {query}"));
    };
    let hits = affected_nodes(graph, &seed, relations, depth)?;
    let mut lines = vec![
        format!("Affected nodes for {}", node_label(graph, &seed)?),
        format!("Relations: {}", relations.join(", ")),
        format!("Depth: {depth}"),
    ];
    if hits.is_empty() {
        lines.push("No affected nodes found.".into());
    }
    for hit in hits {
        let index = graph
            .node_index(&json!(hit.node_id))
            .ok_or("Affected node no longer exists")?;
        let data = &graph.nodes[index].attributes;
        let location = if let Some(location) = hit.via_location {
            format!(
                "{}:{location}",
                hit.via_file
                    .or_else(|| nonempty(data, "source_file"))
                    .unwrap_or_else(|| "-".into())
            )
        } else {
            format_location(data)
        };
        lines.push(format!(
            "- {} [{}] {location}",
            node_label(graph, &hit.node_id)?,
            hit.via_relation
        ));
    }
    Ok(lines.join("\n"))
}
pub fn load_graph(path: &Path) -> Result<Graph> {
    let parse = || -> Result<Value> {
        let bytes = std::fs::read(path).map_err(crate::Error::from)?;
        serde_json::from_slice(&bytes).map_err(crate::Error::from)
    };
    let mut data = parse().map_err(|e| {
        format!(
            "Cannot read graph file {}: {e}. Re-run '{} extract' to regenerate it.",
            path.display(),
            crate::cli::program()
        )
    })?;
    let object = data
        .as_object_mut()
        .ok_or("Graph input must be an object")?;
    object.insert("directed".into(), true.into());
    if !object.contains_key("links")
        && let Some(edges) = object.shift_remove("edges")
    {
        object.insert("links".into(), edges);
    }
    Graph::from_node_link_owned(data, "links")
}
pub fn dispatch(request: &Value) -> Result<Value> {
    let operation = request["operation"].as_str().unwrap_or("");
    let root = request["root"].as_str().map(Path::new);
    if operation == "affected.normalize" {
        return Ok(json!(normalize_label(
            request["label"].as_str().ok_or("Expected label")?
        )));
    }
    if operation == "affected.bare" {
        return Ok(json!(bare_name(
            request["label"].as_str().ok_or("Expected label")?
        )));
    }
    if operation == "affected.relative" {
        return Ok(json!(as_repo_relative(
            request["query"].as_str().ok_or("Expected query")?,
            root
        )?));
    }
    if operation == "affected.location" {
        return Ok(json!(format_location(
            request["data"].as_object().ok_or("Expected attributes")?
        )));
    }
    if operation == "affected.load" {
        return Ok(
            load_graph(Path::new(request["path"].as_str().ok_or("Expected path")?))?
                .to_node_link("links"),
        );
    }
    crate::run_borrowed_graph_operation(request, "links")
}

pub(crate) fn dispatch_graph(request: &Value, graph: &mut Graph) -> Result<Value> {
    prepare_graph(request, graph)?;
    query_graph(request, graph)
}

/// Whether a request asks for graph mutations before its query runs.
pub(crate) fn mutates(request: &Value) -> bool {
    request.get("predecessor_order").is_some()
        || request
            .get("actions")
            .and_then(Value::as_array)
            .is_some_and(|actions| !actions.is_empty())
}

/// Apply a request's optional mutations (`predecessor_order`, `actions`).
pub(crate) fn prepare_graph(request: &Value, graph: &mut Graph) -> Result<()> {
    if let Some(order) = request.get("predecessor_order") {
        graph.restore_predecessor_order(order)?;
    }
    // Optional native mutations exercise traversal on a graph built in memory.
    for action in request["actions"].as_array().into_iter().flatten() {
        match action["kind"].as_str() {
            Some("edge") => {
                graph.add_edge(
                    action["source"].clone(),
                    action["target"].clone(),
                    action.get("key").cloned(),
                    action["attributes"]
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                )?;
            }
            Some("remove") => {
                let _ = graph.remove_edge(&action["source"], &action["target"], action.get("key"));
            }
            _ => return Err("Unknown graph mutation".into()),
        }
    }
    Ok(())
}

/// Run a request's query against a prepared graph.
pub(crate) fn query_graph(request: &Value, graph: &Graph) -> Result<Value> {
    let operation = request["operation"].as_str().unwrap_or("");
    let root = request["root"].as_str().map(Path::new);
    let query = request["query"].as_str().unwrap_or("");
    let relations = request
        .get("relations")
        .map(|v| serde_json::from_value::<Vec<String>>(v.clone()))
        .transpose()
        .map_err(crate::Error::from)?
        .unwrap_or_else(|| {
            DEFAULT_AFFECTED_RELATIONS
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    let depth = request["depth"].as_i64().unwrap_or(2);
    match operation {
        "affected.resolve" => Ok(json!(resolve_seed(graph, query, root)?)),
        "affected.hits" => Ok(json!(affected_nodes(
            graph,
            request["seed"].as_str().ok_or("Expected seed")?,
            &relations,
            depth
        )?)),
        "affected.format" => Ok(json!(format_affected(
            graph, query, &relations, depth, root
        )?)),
        "affected.prefer" => {
            let ids = serde_json::from_value::<Vec<String>>(request["ids"].clone())
                .map_err(crate::Error::from)?;
            Ok(json!(prefer_file_node(graph, &ids, query)))
        }
        "affected.label" => Ok(json!(node_label(
            graph,
            request["seed"].as_str().ok_or("Expected seed")?
        )?)),
        _ => Err("Unknown affected operation".into()),
    }
}
