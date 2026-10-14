//! Typed in-process API.
//!
//! The JSON boundary ([`crate::dispatch_slice`], [`crate::dispatch_owned`])
//! exists for the differential checks against the pinned Python and for
//! out-of-process callers. Code linked into the same process pays for none of
//! that: it builds or loads a [`Graph`] once, keeps it in a [`GraphHandle`],
//! and calls the traversal and clustering functions with plain Rust values.
//! Results are the same structs the boundary serialises, so a caller can
//! check its own typed result against the JSON path when it wants to.
//!
//! ```no_run
//! use graphicle::api::{AffectedQuery, GraphHandle};
//! # fn main() -> graphicle::Result<()> {
//! let graph = GraphHandle::from_path(std::path::Path::new(".graphicle/graph.json"))?;
//! let hits = AffectedQuery::new("api_handler").depth(3).run(&graph)?;
//! for hit in hits {
//!     println!("{} via {} at depth {}", hit.node_id, hit.via_relation, hit.depth);
//! }
//! # Ok(())
//! # }
//! ```
use crate::Result;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub use crate::affected::{
    AffectedHit, DEFAULT_AFFECTED_RELATIONS, affected_nodes, bare_name, format_affected,
    load_graph, node_label, normalize_label, prefer_file_node, resolve_seed,
};
pub use crate::cluster::{
    Communities, Partition, cluster as communities, cohesion_score, label_communities_by_hub,
    native_leiden, partition, score_all, split_community,
};
pub use crate::extract::{ExtractOptions, ExtractReport, extract, extract_with_report};
pub use crate::graph::{Edge, Graph, Node};
pub use crate::serve_search::SearchIndex as LexicalIndex;
pub use crate::session::{GraphHandle, GraphInfo, GraphRegistry, registry};

/// Reverse-dependency traversal parameters, mirroring the `affected` command.
#[derive(Clone, Debug, PartialEq)]
pub struct AffectedQuery {
    /// Node id, label or repository path to start from.
    pub query: String,
    /// Edge relations to follow; [`DEFAULT_AFFECTED_RELATIONS`] by default.
    pub relations: Vec<String>,
    /// Maximum traversal depth (default 2).
    pub depth: i64,
    /// Repository root used to make path queries repository-relative.
    pub root: Option<PathBuf>,
}

impl AffectedQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            relations: DEFAULT_AFFECTED_RELATIONS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            depth: 2,
            root: None,
        }
    }
    pub fn relations<I, S>(mut self, relations: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.relations = relations.into_iter().map(Into::into).collect();
        self
    }
    pub fn depth(mut self, depth: i64) -> Self {
        self.depth = depth;
        self
    }
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }
    fn root_path(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The unique node the query resolves to, if any.
    pub fn seed(&self, graph: &Graph) -> Result<Option<String>> {
        resolve_seed(graph, &self.query, self.root_path())
    }

    /// Every node that depends on the resolved seed, or nothing when the
    /// query does not resolve to exactly one node.
    pub fn run(&self, graph: &Graph) -> Result<Vec<AffectedHit>> {
        match self.seed(graph)? {
            Some(seed) => affected_nodes(graph, &seed, &self.relations, self.depth),
            None => Ok(Vec::new()),
        }
    }

    /// Dependents of an already resolved seed node id.
    pub fn from_seed(&self, graph: &Graph, seed: &str) -> Result<Vec<AffectedHit>> {
        affected_nodes(graph, seed, &self.relations, self.depth)
    }

    /// The human-readable report the `affected` command prints.
    pub fn report(&self, graph: &Graph) -> Result<String> {
        format_affected(
            graph,
            &self.query,
            &self.relations,
            self.depth,
            self.root_path(),
        )
    }
}

/// Community detection parameters, mirroring `graphify.cluster.cluster`.
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterOptions {
    pub resolution: f64,
    pub exclude_hubs_percentile: Option<f64>,
    /// `auto`, `leiden` or `louvain`.
    pub backend: String,
}

impl Default for ClusterOptions {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            exclude_hubs_percentile: None,
            backend: "auto".into(),
        }
    }
}

impl ClusterOptions {
    pub fn resolution(mut self, resolution: f64) -> Self {
        self.resolution = resolution;
        self
    }
    pub fn exclude_hubs_percentile(mut self, percentile: Option<f64>) -> Self {
        self.exclude_hubs_percentile = percentile;
        self
    }
    pub fn backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = backend.into();
        self
    }
    /// Communities as `(community id, member node ids)`.
    pub fn run(&self, graph: &Graph) -> Result<Communities> {
        communities(
            graph,
            self.resolution,
            self.exclude_hubs_percentile,
            &self.backend,
        )
    }
    /// Node-to-community assignment.
    pub fn partition(&self, graph: &Graph) -> Result<Partition> {
        partition(graph, self.resolution, &self.backend)
    }
}

/// Node attributes given as `(name, value)` pairs.
pub fn attributes<I, K>(pairs: I) -> Map<String, Value>
where
    I: IntoIterator<Item = (K, Value)>,
    K: Into<String>,
{
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

/// Typed construction helpers for graphs whose identifiers are strings, which
/// is every graph the engine builds.
impl Graph {
    /// Add or update a string-identified node.
    pub fn node(&mut self, id: &str, attributes: Map<String, Value>) -> Result<usize> {
        self.add_node(json!(id), attributes)
    }

    /// Add a `relation` edge between string-identified nodes, creating the
    /// endpoints if needed. Returns the edge key (`null` for simple graphs).
    pub fn relate(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        mut attributes: Map<String, Value>,
    ) -> Result<Value> {
        attributes.insert("relation".into(), json!(relation));
        self.add_edge(json!(source), json!(target), None, attributes)
    }

    /// Index of a string-identified node.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.node_index(&json!(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_owned;

    fn sample() -> Graph {
        let mut graph = Graph::new(true, false);
        graph
            .node("app.py", attributes([("label", json!("app.py"))]))
            .unwrap();
        graph
            .node("handler", attributes([("label", json!("handler"))]))
            .unwrap();
        graph
            .node("helper", attributes([("label", json!("helper"))]))
            .unwrap();
        graph
            .node("util", attributes([("label", json!("util"))]))
            .unwrap();
        graph
            .relate("app.py", "handler", "contains", Map::new())
            .unwrap();
        graph
            .relate(
                "handler",
                "helper",
                "calls",
                attributes([("source_file", json!("app.py"))]),
            )
            .unwrap();
        graph.relate("helper", "util", "calls", Map::new()).unwrap();
        graph
    }

    #[test]
    fn typed_affected_matches_the_json_boundary() {
        let graph = GraphHandle::new(sample());
        let query = AffectedQuery::new("util").depth(2);
        assert_eq!(query.seed(&graph).unwrap().as_deref(), Some("util"));
        let hits = query.run(&graph).unwrap();
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.node_id.as_str(), hit.depth))
                .collect::<Vec<_>>(),
            [("helper", 1), ("handler", 2)]
        );
        let boundary = dispatch_owned(json!({
            "operation": "affected.hits", "seed": "util", "depth": 2,
            "graph": graph.to_node_link("links"),
        }))
        .unwrap();
        assert_eq!(boundary, json!(hits));
        assert_eq!(
            query.report(&graph).unwrap(),
            dispatch_owned(json!({
                "operation": "affected.format", "query": "util", "depth": 2,
                "graph": graph.to_node_link("links"),
            }))
            .unwrap()
        );
        assert!(AffectedQuery::new("nope").run(&graph).unwrap().is_empty());
    }

    #[test]
    fn typed_clustering_matches_the_json_boundary() {
        let graph = sample();
        let options = ClusterOptions::default().backend("leiden");
        let communities = options.run(&graph).unwrap();
        let boundary = dispatch_owned(json!({
            "operation": "cluster.run", "backend": "leiden",
            "graph": graph.to_node_link("edges"),
        }))
        .unwrap();
        assert_eq!(boundary, crate::cluster::communities_to_json(&communities));
        assert_eq!(options.partition(&graph).unwrap().len(), graph.node_count());
    }

    #[test]
    fn graph_helpers_use_string_identifiers() {
        let graph = sample();
        assert_eq!(graph.index_of("handler"), Some(1));
        assert_eq!(graph.index_of("missing"), None);
        assert_eq!(graph.edge_count(), 3);
    }
}
