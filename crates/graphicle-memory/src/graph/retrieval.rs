//! Pure, in-process retrieval entry points (`retrieval.candidates` and
//! `retrieval.expand`).
//!
//! The engine's `serve` fuses lexical scoring, seed selection, traversal and text
//! rendering into one MCP tool call. These entry points expose the first
//! three stages as typed data so a host can fuse them with dense retrieval,
//! rerank, and pack context under its own budget. They compute exactly what
//! the parity `serve` path computes (`score_query`, `pick_seeds`,
//! `traverse` over the context-filtered undirected traversal view) but work
//! on resident graphs without copying them, and cache the trigram index per
//! resident graph.

use crate::graph::{
    Error, Result,
    graph::Graph,
    serve_search::{
        CONSTANTS, SearchIndex, id, label, pick_seeds_in_order, resolve_contexts, score_query, text,
    },
    session::{self, GraphHandle},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    sync::{Arc, LazyLock, Mutex},
};

/// One lexical hit with the facts a host needs to cite it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub node_id: String,
    pub score: f64,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Ranked lexical candidates plus the parity seed selection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Candidates {
    /// Query terms after the parity term expansion.
    pub terms: Vec<String>,
    /// Best-first hits; at most `limit`.
    pub candidates: Vec<Candidate>,
    /// Node ids `serve` would start traversal from.
    pub seeds: Vec<String>,
    /// Total nodes with a positive score before the limit was applied.
    pub matched: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Bfs,
    Dfs,
}

/// Traversal parameters for [`expand`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpandOptions {
    pub depth: i64,
    pub mode: Mode,
    /// Explicit context filters (`code`, `docs`, ...), resolved the way
    /// `serve` resolves them, with `question` supplying inferred ones.
    pub filters: Vec<String>,
    pub question: String,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        Self {
            depth: 3,
            mode: Mode::Bfs,
            filters: vec![],
            question: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpandedNode {
    pub node_id: String,
    pub label: String,
    pub seed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpandedEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub confidence: String,
}

/// The neighbourhood `serve` would render for a set of seeds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Expansion {
    pub depth: i64,
    pub mode: Mode,
    pub filters: Vec<String>,
    pub nodes: Vec<ExpandedNode>,
    pub edges: Vec<ExpandedEdge>,
    /// Seed ids that were not found in the graph.
    pub unknown_seeds: Vec<String>,
}

fn attr(graph: &Graph, index: usize, key: &str) -> Option<String> {
    graph.nodes()[index]
        .attributes
        .get(key)
        .filter(|v| !v.is_null())
        .map(text)
        .filter(|s| !s.is_empty())
}

fn candidate(graph: &Graph, index: usize, score: f64) -> Candidate {
    Candidate {
        node_id: id(graph, index),
        score,
        label: label(graph, index),
        source_file: attr(graph, index, "source_file"),
        source_location: attr(graph, index, "source_location"),
        stable_id: attr(graph, index, "stable_id"),
        revision: attr(graph, index, "revision"),
        kind: attr(graph, index, "kind"),
    }
}

/// Rank nodes lexically for `query`, exactly as `serve` ranks them before
/// traversal, and report the seeds it would pick.
pub fn lexical_candidates(
    graph: &Graph,
    index: &SearchIndex,
    query: &str,
    limit: usize,
) -> Candidates {
    let terms = crate::graph::query_terms::query_terms(query);
    let mut scored = score_query(graph, index, &terms, true);
    let intent = CONSTANTS["_RELATIONAL_INTENT_TERMS"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if terms.iter().any(|t| !intent.contains(&json!(t))) {
        scored
            .best_seed_by_term
            .retain(|t, _| !intent.contains(&json!(t)));
    }
    let seeds = pick_seeds_in_order(
        Some(graph),
        &scored.ranked,
        3,
        0.2,
        &scored.best_seed_by_term,
        &scored.term_order,
    );
    Candidates {
        terms,
        matched: scored.ranked.len(),
        candidates: scored
            .ranked
            .iter()
            .take(limit)
            .map(|&(score, i)| candidate(graph, i, score))
            .collect(),
        seeds: seeds.iter().map(|&i| id(graph, i)).collect(),
    }
}

/// The undirected, context-filtered traversal view as adjacency lists, built
/// without copying node or edge attributes. Neighbour order and degrees are
/// those of `filter_contexts(traversal_view(graph))`, so traversal results
/// match the parity path exactly.
struct View {
    adjacency: Vec<Vec<usize>>,
    degree: Vec<usize>,
    /// `(u, v)` with `u <= v` → merged `(relation, confidence)`.
    labels: HashMap<(usize, usize), (String, String)>,
}

impl View {
    fn new(graph: &Graph, filters: &[String]) -> Self {
        // One entry per surviving edge of `traversal_view(graph)`: a plain
        // graph folds parallel and opposite arcs into one edge whose attribute
        // dict is merged (later keys overwrite earlier ones); a multigraph
        // keeps every arc as its own undirected edge.
        type Attrs = (Option<String>, Option<String>, Option<String>);
        let count = graph.nodes().len();
        let mut order: Vec<Vec<usize>> = vec![vec![]; count];
        let mut entries: HashMap<(usize, usize), Vec<Attrs>> = HashMap::new();
        for (u, v, edge) in graph.iter_edges() {
            let key = if u <= v { (u, v) } else { (v, u) };
            let list = entries.entry(key).or_default();
            if list.is_empty() {
                order[u].push(v);
                if u != v {
                    order[v].push(u);
                }
            }
            if graph.is_multigraph() || list.is_empty() {
                list.push((None, None, None));
            }
            let entry = list.last_mut().expect("entry pushed above");
            if let Some(context) = edge.attributes.get("context") {
                entry.0 = Some(text(context));
            }
            if let Some(relation) = edge.attributes.get("relation") {
                entry.1 = Some(text(relation));
            }
            if let Some(confidence) = edge.attributes.get("confidence") {
                entry.2 = Some(text(confidence));
            }
        }
        let kept = |attrs: &Attrs| {
            filters.is_empty() || filters.contains(&attrs.0.clone().unwrap_or_default())
        };
        let mut adjacency: Vec<Vec<usize>> = vec![vec![]; count];
        let mut degree = vec![0usize; count];
        let mut labels = HashMap::new();
        // `filter_contexts` rebuilds the graph from the view's edge iteration
        // (source ascending, targets in insertion order, target >= source),
        // which fixes the neighbour order; an empty filter returns the view
        // itself, whose neighbour order is the insertion order above.
        for (s, targets) in order.iter().enumerate() {
            for &t in targets {
                if t < s {
                    continue;
                }
                let key = (s, t);
                let Some(list) = entries.get(&key) else {
                    continue;
                };
                let surviving = list.iter().filter(|attrs| kept(attrs)).count();
                if surviving == 0 {
                    continue;
                }
                if filters.is_empty() {
                    // Keep the view's own neighbour order.
                } else {
                    adjacency[s].push(t);
                    if s != t {
                        adjacency[t].push(s);
                    }
                }
                degree[s] += surviving;
                degree[t] += surviving;
                let mut relation = String::new();
                let mut confidence = String::new();
                for attrs in list.iter().filter(|attrs| kept(attrs)) {
                    if let Some(value) = &attrs.1 {
                        relation.clone_from(value);
                    }
                    if let Some(value) = &attrs.2 {
                        confidence.clone_from(value);
                    }
                }
                labels.insert(key, (relation, confidence));
            }
        }
        if filters.is_empty() {
            adjacency = order;
        }
        Self {
            adjacency,
            degree,
            labels,
        }
    }

    /// `serve_text::traverse` over this view.
    fn traverse(
        &self,
        seeds: &[usize],
        depth: i64,
        dfs: bool,
    ) -> (BTreeSet<usize>, Vec<(usize, usize)>) {
        let mut sorted = self.degree.clone();
        sorted.sort_unstable();
        let threshold = sorted
            .get((sorted.len() as f64 * 0.99) as usize)
            .copied()
            .unwrap_or(50)
            .max(50);
        let seedset: BTreeSet<usize> = seeds.iter().copied().collect();
        let mut visited = BTreeSet::new();
        let mut edges = vec![];
        if dfs {
            let mut stack: Vec<(usize, i64)> = seeds.iter().rev().map(|&i| (i, 0)).collect();
            while let Some((i, d)) = stack.pop() {
                if visited.contains(&i) || d > depth {
                    continue;
                }
                visited.insert(i);
                if !seedset.contains(&i) && self.degree[i] >= threshold {
                    continue;
                }
                for &nb in &self.adjacency[i] {
                    if !visited.contains(&nb) {
                        stack.push((nb, d + 1));
                        edges.push((i, nb));
                    }
                }
            }
        } else {
            visited = seedset.clone();
            let mut frontier = seedset.clone();
            for _ in 0..depth.max(0) {
                let mut next = BTreeSet::new();
                for i in frontier {
                    if !seedset.contains(&i) && self.degree[i] >= threshold {
                        continue;
                    }
                    for &nb in &self.adjacency[i] {
                        if !visited.contains(&nb) {
                            next.insert(nb);
                            edges.push((i, nb));
                        }
                    }
                }
                visited.extend(&next);
                frontier = next;
            }
        }
        self.complete_induced(&visited, &mut edges);
        (visited, edges)
    }

    fn complete_induced(&self, visited: &BTreeSet<usize>, edges: &mut Vec<(usize, usize)>) {
        let key = |u: usize, v: usize| if u < v { (u, v) } else { (v, u) };
        let mut seen: BTreeSet<(usize, usize)> = edges.iter().map(|&(u, v)| key(u, v)).collect();
        let mut examined = BTreeSet::new();
        for &u in visited {
            for &v in &self.adjacency[u] {
                if examined.contains(&v) {
                    continue;
                }
                if u != v && visited.contains(&v) && seen.insert(key(u, v)) {
                    edges.push((u, v));
                }
            }
            examined.insert(u);
        }
    }
}

/// Expand `seed_ids` the way `serve` expands its picked seeds: over the
/// undirected traversal view, restricted to the resolved context filters,
/// with hub suppression and induced-edge completion.
pub fn expand(graph: &Graph, seed_ids: &[Value], options: &ExpandOptions) -> Result<Expansion> {
    if options.depth < 0 {
        return Err(Error::invalid(
            "Expected depth to be a non-negative integer",
        ));
    }
    let mut seeds = vec![];
    let mut unknown = vec![];
    for seed in seed_ids {
        match graph.node_index(seed) {
            Some(i) if !seeds.contains(&i) => seeds.push(i),
            Some(_) => {}
            None => unknown.push(crate::graph::python_compat::string(seed)),
        }
    }
    let (filters, _) = resolve_contexts(&options.question, &options.filters);
    let view = View::new(graph, &filters);
    let (visited, edges) = view.traverse(&seeds, options.depth, options.mode == Mode::Dfs);
    let seedset: BTreeSet<usize> = seeds.iter().copied().collect();
    let nodes = visited
        .iter()
        .map(|&i| ExpandedNode {
            node_id: id(graph, i),
            label: label(graph, i),
            seed: seedset.contains(&i),
            source_file: attr(graph, i, "source_file"),
            source_location: attr(graph, i, "source_location"),
            stable_id: attr(graph, i, "stable_id"),
            kind: attr(graph, i, "kind"),
        })
        .collect();
    let edges = edges
        .into_iter()
        .map(|(u, v)| {
            let key = if u <= v { (u, v) } else { (v, u) };
            let (relation, confidence) = view.labels.get(&key).cloned().unwrap_or_default();
            ExpandedEdge {
                source: id(graph, u),
                target: id(graph, v),
                relation,
                confidence,
            }
        })
        .collect();
    Ok(Expansion {
        depth: options.depth,
        mode: options.mode,
        filters,
        nodes,
        edges,
        unknown_seeds: unknown,
    })
}

// ---------------------------------------------------------------------------
// Resident-graph index cache and the JSON boundary.

struct CachedIndex {
    graph: std::sync::Weak<Graph>,
    index: Arc<SearchIndex>,
}

static INDEXES: LazyLock<Mutex<HashMap<String, CachedIndex>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The trigram index for a resident graph, built once per graph instance.
/// A graph re-opened under the same name gets a fresh index.
pub fn resident_index(graph_ref: &str, handle: &GraphHandle) -> Arc<SearchIndex> {
    // Release a cached index as soon as its reference is closed.
    static FORGET_ON_CLOSE: std::sync::Once = std::sync::Once::new();
    FORGET_ON_CLOSE.call_once(|| session::on_close(forget_index));
    let graph = handle.downgrade();
    let mut cache = INDEXES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = cache.get(graph_ref)
        && entry.graph.ptr_eq(&graph)
    {
        return Arc::clone(&entry.index);
    }
    // Drop indexes for graphs the registry has since closed or replaced.
    cache.retain(|name, entry| {
        session::registry()
            .peek(name)
            .is_some_and(|h| entry.graph.ptr_eq(&h.downgrade()))
    });
    let index = Arc::new(SearchIndex::new(handle));
    cache.insert(
        graph_ref.to_string(),
        CachedIndex {
            graph,
            index: Arc::clone(&index),
        },
    );
    index
}

/// Forget the cached index of a closed graph.
pub fn forget_index(graph_ref: &str) {
    INDEXES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(graph_ref);
}

enum Input {
    Resident(String, GraphHandle),
    Owned(Box<Graph>),
}

impl Input {
    fn graph(&self) -> &Graph {
        match self {
            Input::Resident(_, handle) => handle,
            Input::Owned(graph) => graph,
        }
    }
    fn index(&self) -> Arc<SearchIndex> {
        match self {
            Input::Resident(name, handle) => resident_index(name, handle),
            Input::Owned(graph) => Arc::new(SearchIndex::new(graph)),
        }
    }
}

fn input(request: &Value) -> Result<Input> {
    let reference = request.get("graph_ref").filter(|v| !v.is_null());
    let inline = request.get("graph").filter(|v| !v.is_null());
    let path = request.get("graph_path").filter(|v| !v.is_null());
    match (reference, inline, path) {
        (Some(Value::String(reference)), None, None) if !reference.is_empty() => Ok(
            Input::Resident(reference.clone(), session::registry().get(reference)?),
        ),
        (Some(_), None, None) => Err(Error::invalid(
            "Expected graph_ref to be a non-empty string",
        )),
        (None, Some(graph), None) => Ok(Input::Owned(Box::new(Graph::from_node_link(
            graph,
            session::edges_key(request)?,
        )?))),
        (None, None, Some(Value::String(path))) => Ok(Input::Owned(Box::new(
            crate::graph::affected::load_graph(Path::new(path))?,
        ))),
        (None, None, Some(_)) => Err(Error::invalid("Expected graph_path to be a string")),
        (None, None, None) => Err(Error::invalid("Expected graph, graph_path or graph_ref")),
        _ => Err(Error::invalid(
            "Provide exactly one of graph, graph_path or graph_ref",
        )),
    }
}

fn strings(request: &Value, key: &str) -> Result<Vec<String>> {
    match request.get(key).filter(|v| !v.is_null()) {
        None => Ok(vec![]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    Error::invalid(format!("Expected {key} to be a list of strings"))
                })
            })
            .collect(),
        Some(_) => Err(Error::invalid(format!(
            "Expected {key} to be a list of strings"
        ))),
    }
}

pub(crate) fn expand_options_from(request: &Value) -> Result<ExpandOptions> {
    let mut options = ExpandOptions::default();
    if let Some(depth) = request.get("depth").filter(|v| !v.is_null()) {
        options.depth = depth
            .as_i64()
            .ok_or_else(|| Error::invalid("Expected depth to be an integer"))?;
    }
    if let Some(mode) = request.get("mode").filter(|v| !v.is_null()) {
        options.mode = match mode.as_str() {
            Some("bfs") => Mode::Bfs,
            Some("dfs") => Mode::Dfs,
            _ => return Err(Error::invalid("Expected mode to be bfs or dfs")),
        };
    }
    options.filters = strings(request, "filters")?;
    if let Some(question) = request.get("question").filter(|v| !v.is_null()) {
        options.question = question
            .as_str()
            .ok_or_else(|| Error::invalid("Expected question to be a string"))?
            .to_string();
    }
    Ok(options)
}

pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "retrieval.candidates" => {
            let query = request
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::invalid("Expected query to be a string"))?;
            let limit = match request.get("limit").filter(|v| !v.is_null()) {
                None => 10,
                Some(v) => v
                    .as_u64()
                    .ok_or_else(|| Error::invalid("Expected limit to be a non-negative integer"))?
                    as usize,
            };
            let input = input(request)?;
            let index = input.index();
            Ok(json!(lexical_candidates(
                input.graph(),
                &index,
                query,
                limit
            )))
        }
        "retrieval.expand" => {
            let seeds = request
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::invalid("Expected nodes to be a list of node ids"))?;
            let options = expand_options_from(request)?;
            let input = input(request)?;
            Ok(json!(expand(input.graph(), seeds, &options)?))
        }
        _ => Err(Error::UnsupportedOperation(
            "Unsupported retrieval operation".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::attributes;
    use crate::graph::serve_text::{filter_contexts, traversal_view, traverse};

    fn sample() -> Graph {
        sample_graph(true)
    }

    fn sample_graph(multigraph: bool) -> Graph {
        let mut graph = Graph::new(true, multigraph);
        let node = |graph: &mut Graph, id: &str, label: &str, file: &str| {
            graph
                .add_node(
                    json!(id),
                    attributes([
                        ("label", json!(label)),
                        ("source_file", json!(file)),
                        ("file_type", json!("code")),
                    ]),
                )
                .unwrap();
        };
        node(&mut graph, "app.py", "app.py", "src/app.py");
        node(&mut graph, "handler", "request_handler()", "src/app.py");
        node(&mut graph, "helper", "parse_payload()", "src/util.py");
        node(&mut graph, "config", "Config", "src/config.py");
        node(&mut graph, "doc", "Design notes", "docs/design.md");
        let edge = |graph: &mut Graph, s: &str, t: &str, relation: &str, context: &str| {
            graph
                .add_edge(
                    json!(s),
                    json!(t),
                    None,
                    attributes([
                        ("relation", json!(relation)),
                        ("confidence", json!("EXTRACTED")),
                        ("context", json!(context)),
                    ]),
                )
                .unwrap();
        };
        edge(&mut graph, "app.py", "handler", "contains", "code");
        edge(&mut graph, "handler", "helper", "calls", "code");
        edge(&mut graph, "helper", "config", "references", "code");
        edge(&mut graph, "doc", "handler", "mentions", "docs");
        edge(&mut graph, "helper", "handler", "calls", "code");
        // An opposite arc with no context: a plain graph folds it into the
        // `helper -> config` edge (dropping it under a `code` filter), a
        // multigraph keeps it as its own edge.
        graph
            .add_edge(
                json!("config"),
                json!("helper"),
                None,
                attributes([("relation", json!("configures"))]),
            )
            .unwrap();
        graph
    }

    #[test]
    fn candidates_rank_and_seed_like_serve() {
        let graph = sample();
        let index = SearchIndex::new(&graph);
        let result = lexical_candidates(&graph, &index, "parse_payload", 5);
        assert_eq!(result.candidates[0].node_id, "helper");
        assert_eq!(result.candidates[0].label, "parse_payload()");
        assert_eq!(
            result.candidates[0].source_file.as_deref(),
            Some("src/util.py")
        );
        assert_eq!(result.seeds, vec!["helper"]);
        assert!(result.matched >= 1);
        let limited = lexical_candidates(&graph, &index, "parse_payload handler", 1);
        assert_eq!(limited.candidates.len(), 1);
        assert!(limited.matched >= 2);
    }

    #[test]
    fn expansion_matches_the_parity_traversal_for_every_filter_and_mode() {
        for graph in [sample_graph(false), sample_graph(true)] {
            expansion_matches_parity(&graph);
        }
    }

    fn expansion_matches_parity(graph: &Graph) {
        for filters in [vec![], vec!["code".to_string()], vec!["docs".to_string()]] {
            let parity_graph = filter_contexts(&traversal_view(graph).unwrap(), &filters).unwrap();
            let view = View::new(graph, &filters);
            assert_eq!(
                view.degree,
                crate::graph::serve_search::degrees(&parity_graph),
                "{filters:?}"
            );
            for (node, neighbors) in view.adjacency.iter().enumerate() {
                let expected: Vec<usize> = parity_graph.neighbor_indices(node, false).collect();
                assert_eq!(neighbors, &expected, "{filters:?} {node}");
            }
            for mode in [Mode::Bfs, Mode::Dfs] {
                for depth in 0..4 {
                    let seeds = [json!("handler"), json!("missing")];
                    let options = ExpandOptions {
                        depth,
                        mode,
                        filters: filters.clone(),
                        question: String::new(),
                    };
                    let expansion = expand(graph, &seeds, &options).unwrap();
                    let (nodes, edges) = traverse(&parity_graph, &[1], depth, mode == Mode::Dfs);
                    let expected_nodes: Vec<String> = nodes.iter().map(|&i| id(graph, i)).collect();
                    let actual_nodes: Vec<String> =
                        expansion.nodes.iter().map(|n| n.node_id.clone()).collect();
                    assert_eq!(actual_nodes, expected_nodes, "{filters:?} {mode:?} {depth}");
                    let mut expected_edges: Vec<(String, String)> = edges
                        .iter()
                        .map(|&(u, v)| (id(graph, u), id(graph, v)))
                        .collect();
                    let mut actual_edges: Vec<(String, String)> = expansion
                        .edges
                        .iter()
                        .map(|e| (e.source.clone(), e.target.clone()))
                        .collect();
                    expected_edges.sort();
                    actual_edges.sort();
                    assert_eq!(actual_edges, expected_edges, "{filters:?} {mode:?} {depth}");
                    assert_eq!(expansion.unknown_seeds, vec!["missing"]);
                    assert!(
                        expansion
                            .nodes
                            .iter()
                            .any(|n| n.seed && n.node_id == "handler")
                    );
                }
            }
        }
    }

    #[test]
    fn expansion_labels_edges_from_the_merged_view() {
        let helper_config = |graph: &Graph, filters: Vec<String>| {
            let expansion = expand(
                graph,
                &[json!("helper")],
                &ExpandOptions {
                    depth: 1,
                    filters,
                    ..ExpandOptions::default()
                },
            )
            .unwrap();
            expansion
                .edges
                .iter()
                .find(|e| {
                    (e.source == "helper" && e.target == "config")
                        || (e.source == "config" && e.target == "helper")
                })
                .map(|e| (e.relation.clone(), e.confidence.clone()))
        };
        for graph in [sample_graph(false), sample_graph(true)] {
            // Unfiltered: the later `configures` arc overwrites the relation,
            // the confidence it lacks survives from the earlier arc.
            assert_eq!(
                helper_config(&graph, vec![]),
                Some(("configures".into(), "EXTRACTED".into()))
            );
        }
        // Filtered to `code`: the plain graph's folded edge inherits no
        // context from the later arc, so it survives with the merged label;
        // the multigraph drops the context-less arc and keeps `references`.
        assert_eq!(
            helper_config(&sample_graph(false), vec!["code".into()]),
            Some(("configures".into(), "EXTRACTED".into()))
        );
        assert_eq!(
            helper_config(&sample_graph(true), vec!["code".into()]),
            Some(("references".into(), "EXTRACTED".into()))
        );
    }

    #[test]
    fn boundary_serves_inline_and_resident_graphs_with_a_cached_index() {
        let graph = sample();
        let inline = dispatch(&json!({
            "operation": "retrieval.candidates", "query": "request handler",
            "graph": graph.to_node_link("links"), "limit": 3,
        }))
        .unwrap();
        assert_eq!(inline["candidates"][0]["node_id"], json!("handler"));
        let info =
            session::registry().open(GraphHandle::new(graph.clone()), Some("retrieval-sample"));
        let first = dispatch(&json!({
            "operation": "retrieval.candidates", "query": "request handler",
            "graph_ref": info.graph_ref, "limit": 3,
        }))
        .unwrap();
        assert_eq!(first, inline);
        let handle = session::registry().get("retrieval-sample").unwrap();
        let a = resident_index("retrieval-sample", &handle);
        let b = resident_index("retrieval-sample", &handle);
        assert!(Arc::ptr_eq(&a, &b));
        let expanded = dispatch(&json!({
            "operation": "retrieval.expand", "nodes": ["handler"], "depth": 1,
            "graph_ref": "retrieval-sample", "filters": ["code"],
        }))
        .unwrap();
        assert_eq!(expanded["filters"], json!(["code"]));
        assert!(expanded["nodes"].as_array().unwrap().len() >= 3);
        assert!(session::registry().close("retrieval-sample"));
        forget_index("retrieval-sample");
    }

    #[test]
    fn boundary_rejects_bad_arguments() {
        let graph = sample().to_node_link("links");
        let err = |request: Value| dispatch(&request).unwrap_err().to_string();
        assert!(
            err(json!({"operation": "retrieval.candidates", "graph": graph})).contains("query")
        );
        assert!(
            err(json!({"operation": "retrieval.candidates", "query": "x", "graph": graph, "limit": -1}))
                .contains("limit")
        );
        assert!(err(json!({"operation": "retrieval.expand", "graph": graph})).contains("nodes"));
        assert!(
            err(json!({"operation": "retrieval.expand", "graph": graph, "nodes": [], "mode": "random"}))
                .contains("mode")
        );
        assert!(
            err(json!({"operation": "retrieval.expand", "graph": graph, "nodes": [], "depth": -1}))
                .contains("depth")
        );
        assert!(
            err(json!({"operation": "retrieval.expand", "graph_ref": "nope", "nodes": []}))
                .contains("Unknown graph_ref")
        );
    }
}
