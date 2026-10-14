// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/affected.py, graphify/global_graph.py, graphify/ids.py,
//           graphify/multigraph_compat.py, graphify/serve.py
//! Native attributed graph substrate for the translated Graphify modules.
//! Preserves insertion order, keyed parallel edges and node-link JSON semantics.
use crate::{
    Result,
    graph_adjacency::Neighbors,
    python_compat::{self, ScalarKey},
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
};
// Identifier indices are never iterated, so a fast non-cryptographic hasher
// cannot change any observable ordering.
#[doc(hidden)]
pub type FastState = foldhash::fast::RandomState;
type IndexMap<K, V> = hashbrown::HashMap<K, V, FastState>;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Scalar(ScalarKey),
    Tuple(Vec<Key>),
}
fn hash_scalar<H: Hasher>(key: &ScalarKey, state: &mut H) {
    match key {
        ScalarKey::Null => 0u8.hash(state),
        ScalarKey::Integer(value) => {
            1u8.hash(state);
            value.hash(state);
        }
        ScalarKey::Float(value) => {
            2u8.hash(state);
            value.hash(state);
        }
        ScalarKey::Text(value) => {
            3u8.hash(state);
            value.hash(state);
        }
    }
}
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Scalar(value) => hash_scalar(value, state),
            Self::Tuple(values) => {
                4u8.hash(state);
                values.len().hash(state);
                for value in values {
                    value.hash(state);
                }
            }
        }
    }
}
// Borrow identifiers for lookup: no String or tuple allocation on every edge.
struct KeyRef<'a>(&'a Value);
impl Hash for KeyRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.0 {
            Value::String(value) => {
                3u8.hash(state);
                value.hash(state);
            }
            Value::Array(values) => {
                4u8.hash(state);
                values.len().hash(state);
                for value in values {
                    KeyRef(value).hash(state);
                }
            }
            value => match python_compat::scalar_key(value) {
                Some(value) => hash_scalar(&value, state),
                None => 5u8.hash(state),
            },
        }
    }
}
impl hashbrown::Equivalent<Key> for KeyRef<'_> {
    fn equivalent(&self, key: &Key) -> bool {
        match (self.0, key) {
            (Value::String(a), Key::Scalar(ScalarKey::Text(b))) => a == b,
            (Value::Array(a), Key::Tuple(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| KeyRef(a).equivalent(b))
            }
            (Value::String(_) | Value::Array(_), _) => false,
            (value, Key::Scalar(key)) => python_compat::scalar_key(value).as_ref() == Some(key),
            _ => false,
        }
    }
}
fn key(value: &Value) -> Result<Key> {
    if let Some(scalar) = python_compat::scalar_key(value) {
        Ok(Key::Scalar(scalar))
    } else if let Value::Array(items) = value {
        Ok(Key::Tuple(items.iter().map(key).collect::<Result<_>>()?))
    } else {
        Err(crate::Error::graph(
            "TypeError: unhashable graph identifier",
        ))
    }
}
fn valid_identifier(value: &Value) -> bool {
    match value {
        Value::Object(_) => false,
        Value::Array(values) => values.iter().all(valid_identifier),
        _ => true,
    }
}
#[derive(Clone, Debug)]
pub struct Node {
    pub id: Value,
    pub attributes: crate::record::Attributes,
}
#[derive(Clone, Debug)]
pub struct Edge {
    pub source: usize,
    pub target: usize,
    pub key: Value,
    pub attributes: crate::record::Attributes,
}
#[derive(Clone, Debug)]
pub struct Graph {
    pub(crate) directed: bool,
    pub(crate) multigraph: bool,
    pub(crate) attributes: Value,
    pub(crate) nodes: Vec<Node>,
    node_index: IndexMap<Key, usize>,
    edges: Vec<Option<Edge>>,
    edge_index: IndexMap<(usize, usize, Key), usize>,
    adjacency: Vec<Neighbors>,
    incoming: Vec<Neighbors>,
}
impl Graph {
    pub fn new(directed: bool, multigraph: bool) -> Self {
        Self::with_capacity(directed, multigraph, 0, 0)
    }
    pub fn with_capacity(directed: bool, multigraph: bool, nodes: usize, edges: usize) -> Self {
        Self {
            directed,
            multigraph,
            attributes: json!({}),
            nodes: Vec::with_capacity(nodes),
            node_index: IndexMap::with_capacity_and_hasher(nodes, FastState::default()),
            edges: Vec::with_capacity(edges),
            edge_index: IndexMap::with_capacity_and_hasher(edges, FastState::default()),
            adjacency: Vec::with_capacity(nodes),
            incoming: Vec::with_capacity(nodes),
        }
    }
    pub fn node_index(&self, id: &Value) -> Option<usize> {
        self.node_index.get(&KeyRef(id)).copied()
    }
    /// The built graph for a query snapshot (`crate::snapshot`): records,
    /// edge slots and both adjacency tables exactly as they are in memory,
    /// in chunks that decode in parallel.
    pub(crate) fn write_snapshot<'a>(&'a self, w: &mut crate::snapshot::Writer<'a>) {
        use crate::snapshot::CHUNK;
        w.u8(u8::from(self.directed));
        w.u8(u8::from(self.multigraph));
        w.value(&self.attributes);
        w.usize(self.nodes.len());
        w.usize(self.edges.len());
        w.end_chunk();
        for part in self.nodes.chunks(CHUNK) {
            for node in part {
                w.value(&node.id);
                node.attributes.write_snapshot(w);
            }
            w.end_chunk();
        }
        for part in self.edges.chunks(CHUNK) {
            for edge in part {
                match edge {
                    None => w.u8(0),
                    Some(edge) => {
                        w.u8(1);
                        w.usize(edge.source);
                        w.usize(edge.target);
                        w.value(&edge.key);
                        edge.attributes.write_snapshot(w);
                    }
                }
            }
            w.end_chunk();
        }
        for rows in [&self.adjacency, &self.incoming] {
            for part in rows.chunks(CHUNK) {
                for row in part {
                    row.write_snapshot(w);
                }
                w.end_chunk();
            }
        }
    }
    /// Inverse of `write_snapshot`; the lookup tables are rebuilt from the
    /// records exactly as `add_node`/`add_edge` key them.
    pub(crate) fn read_snapshot(d: &mut crate::snapshot::Decoder<'_>) -> Result<Self> {
        use crate::snapshot::{CHUNK, chunk_count, corrupt};
        let (directed, multigraph, attributes, n, m) = d.chunk(|r| {
            Ok((
                r.u8()? != 0,
                r.u8()? != 0,
                r.value()?,
                r.usize()?,
                r.usize()?,
            ))
        })?;
        let part = |n: usize, i: usize| CHUNK.min(n - i * CHUNK);
        let nodes = d.chunks(chunk_count(n), |r, i| {
            (0..part(n, i))
                .map(|_| {
                    let id = r.value()?;
                    let attributes = crate::record::Attributes::read_snapshot(r)?;
                    Ok(Node { id, attributes })
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut graph = Self::with_capacity(directed, multigraph, 0, 0);
        graph.attributes = attributes;
        graph.nodes = nodes.into_iter().flatten().collect();
        graph.node_index.reserve(n);
        for (index, node) in graph.nodes.iter().enumerate() {
            if node.id.is_null() || !valid_identifier(&node.id) {
                return Err(corrupt("node identifier"));
            }
            if graph.node_index.insert(key(&node.id)?, index).is_some() {
                return Err(corrupt("duplicate node"));
            }
        }
        let edges = d.chunks(chunk_count(m), |r, i| {
            (0..part(m, i))
                .map(|_| {
                    Ok(match r.u8()? {
                        0 => None,
                        1 => Some(Edge {
                            source: r.index(n)?,
                            target: r.index(n)?,
                            key: r.value()?,
                            attributes: crate::record::Attributes::read_snapshot(r)?,
                        }),
                        _ => return Err(corrupt("edge slot")),
                    })
                })
                .collect::<Result<Vec<_>>>()
        })?;
        graph.edges = edges.into_iter().flatten().collect();
        graph.edge_index.reserve(m);
        for (index, edge) in graph.edges.iter().enumerate() {
            let Some(edge) = edge else { continue };
            let (u, v) = if !directed && edge.source > edge.target {
                (edge.target, edge.source)
            } else {
                (edge.source, edge.target)
            };
            if graph
                .edge_index
                .insert((u, v, key(&edge.key)?), index)
                .is_some()
            {
                return Err(corrupt("duplicate edge"));
            }
        }
        let mut rows = [Vec::new(), Vec::new()];
        for table in &mut rows {
            *table = d
                .chunks(chunk_count(n), |r, i| {
                    (0..part(n, i))
                        .map(|_| Neighbors::read_snapshot(r, n, m))
                        .collect::<Result<Vec<_>>>()
                })?
                .into_iter()
                .flatten()
                .collect();
        }
        let [adjacency, incoming] = rows;
        graph.adjacency = adjacency;
        graph.incoming = incoming;
        Ok(graph)
    }
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
    /// Mutable node records; identifiers must not change through this view.
    pub fn nodes_mut(&mut self) -> impl Iterator<Item = &mut Node> {
        self.nodes.iter_mut()
    }
    /// Mutable live edges; endpoints and keys must not change through this view.
    pub fn edges_mut(&mut self) -> impl Iterator<Item = &mut Edge> {
        self.edges.iter_mut().filter_map(Option::as_mut)
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_directed(&self) -> bool {
        self.directed
    }
    pub fn is_multigraph(&self) -> bool {
        self.multigraph
    }
    pub fn attributes(&self) -> &Value {
        &self.attributes
    }
    pub fn attributes_mut(&mut self) -> &mut Value {
        &mut self.attributes
    }
    pub fn add_node(
        &mut self,
        id: Value,
        attributes: impl Into<crate::record::Attributes>,
    ) -> Result<usize> {
        let attributes = attributes.into();
        if id.is_null() {
            return Err(crate::Error::graph("ValueError: None cannot be a node"));
        }
        if let Some(&index) = self.node_index.get(&KeyRef(&id)) {
            self.nodes[index].attributes.extend_record(attributes);
            return Ok(index);
        }
        let k = key(&id)?;
        let index = self.nodes.len();
        self.node_index.insert(k, index);
        self.nodes.push(Node { id, attributes });
        self.adjacency.push(Neighbors::default());
        self.incoming.push(Neighbors::default());
        Ok(index)
    }
    fn pair(&self, source: usize, target: usize) -> (usize, usize) {
        if !self.directed && source > target {
            (target, source)
        } else {
            (source, target)
        }
    }
    fn attach(&mut self, source: usize, target: usize, edge: usize) {
        self.adjacency[source].push_edge(target, edge);
    }
    pub fn add_edge(
        &mut self,
        source: Value,
        target: Value,
        explicit_key: Option<Value>,
        attributes: impl Into<crate::record::Attributes>,
    ) -> Result<Value> {
        let attributes = attributes.into();
        if self.multigraph && explicit_key.is_some() && attributes.contains_key("key") {
            return Err("TypeError: duplicate reserved edge key".into());
        }
        if source.is_null()
            || target.is_null()
            || !valid_identifier(&source)
            || !valid_identifier(&target)
        {
            return Err(crate::Error::graph(
                "Edge endpoints must be non-null, hashable graph identifiers",
            ));
        }
        if self.multigraph
            && explicit_key
                .as_ref()
                .is_some_and(|key| key.is_array() || key.is_object())
        {
            return Err(crate::Error::graph("TypeError: unhashable edge key"));
        }
        let source = self.add_node(source, Map::new())?;
        let target = self.add_node(target, Map::new())?;
        let (u, v) = self.pair(source, target);
        let edge_key = if !self.multigraph {
            Value::Null
        } else if let Some(k) = explicit_key.filter(|k| !k.is_null()) {
            if k.is_array() || k.is_object() {
                return Err("TypeError: unhashable edge key".into());
            }
            k
        } else {
            // Build the integer key directly: going through a JSON number here
            // would allocate and re-parse its text on every candidate.
            let mut candidate = self.adjacency[source]
                .get(target)
                .map_or(0, |edges| edges.len()) as u64;
            let mut lookup = (u, v, Key::Scalar(ScalarKey::Integer(candidate.into())));
            while self.edge_index.contains_key(&lookup) {
                candidate += 1;
                lookup.2 = Key::Scalar(ScalarKey::Integer(candidate.into()));
            }
            json!(candidate)
        };
        let lookup = (u, v, key(&edge_key)?);
        if let Some(&index) = self.edge_index.get(&lookup) {
            self.edges
                .get_mut(index)
                .and_then(Option::as_mut)
                .ok_or_else(|| crate::Error::Internal("Graph edge index is inconsistent".into()))?
                .attributes
                .extend(attributes);
            return Ok(edge_key);
        }
        let index = self.edges.len();
        self.edges.push(Some(Edge {
            source,
            target,
            key: edge_key.clone(),
            attributes,
        }));
        self.edge_index.insert(lookup, index);
        self.attach(source, target, index);
        self.incoming[target].push_edge(source, index);
        if !self.directed && source != target {
            self.attach(target, source, index);
        }
        Ok(edge_key)
    }
    pub fn number_of_edges(&self, source: &Value, target: &Value) -> usize {
        let (Some(u), Some(v)) = (self.node_index(source), self.node_index(target)) else {
            return 0;
        };
        self.adjacency[u].get(v).map_or(0, |edges| edges.len())
    }
    pub fn edge_count(&self) -> usize {
        self.edge_index.len()
    }
    /// Mutable attributes of the first edge between `source` and `target`
    /// (`G[u][v]` on a simple graph), or of the one with `key` on a multigraph.
    /// The endpoints and key cannot change through this view.
    pub fn edge_attributes_mut(
        &mut self,
        source: &Value,
        target: &Value,
        key: Option<&Value>,
    ) -> Option<&mut crate::record::Attributes> {
        let (u, v) = (self.node_index(source)?, self.node_index(target)?);
        let ids = self.adjacency[u].get(v)?;
        let id = match key {
            None => *ids.first()?,
            Some(key) => *ids
                .iter()
                .find(|&&id| self.edges[id].as_ref().is_some_and(|edge| &edge.key == key))?,
        };
        self.edges[id].as_mut().map(|edge| &mut edge.attributes)
    }
    pub fn edges_between(&self, source: &Value, target: &Value) -> Vec<&Edge> {
        let (Some(u), Some(v)) = (self.node_index(source), self.node_index(target)) else {
            return vec![];
        };
        self.adjacency[u]
            .get(v)
            .into_iter()
            .flatten()
            .filter_map(|&id| self.edges[id].as_ref())
            .collect()
    }
    pub fn remove_edge(
        &mut self,
        source: &Value,
        target: &Value,
        explicit_key: Option<&Value>,
    ) -> Result<()> {
        let (Some(u), Some(v)) = (self.node_index(source), self.node_index(target)) else {
            return Err("Edge not found".into());
        };
        let pair = self.pair(u, v);
        let edge_index = if self.multigraph
            && let Some(k) = explicit_key
        {
            self.edge_index.get(&(pair.0, pair.1, key(k)?)).copied()
        } else {
            self.adjacency[u]
                .get(v)
                .and_then(|edges| edges.last().copied())
        }
        .ok_or("Edge not found")?;
        let edge = self
            .edges
            .get_mut(edge_index)
            .and_then(Option::take)
            .ok_or_else(|| crate::Error::Internal("Graph edge index is inconsistent".into()))?;
        self.edge_index.remove(&(pair.0, pair.1, key(&edge.key)?));
        self.incoming[edge.target].remove_edge(edge.source, edge_index);
        self.adjacency[u].remove_edge(v, edge_index);
        if !self.directed && u != v {
            self.adjacency[v].remove_edge(u, edge_index);
        }
        Ok(())
    }
    pub fn remove_edges_from(&mut self, edges: &[(Value, Value, Option<Value>)]) {
        for (source, target, key) in edges {
            let _ = self.remove_edge(source, target, key.as_ref());
        }
    }
    /// Remove nodes without rebuilding edges: neighbor and predecessor order survive.
    pub fn remove_nodes_from(&mut self, ids: &[Value]) -> Result<usize> {
        let removed: std::collections::BTreeSet<_> =
            ids.iter().filter_map(|id| self.node_index(id)).collect();
        if removed.is_empty() {
            return Ok(0);
        }
        let mut next = 0;
        let remap: Vec<_> = (0..self.nodes.len())
            .map(|i| {
                if removed.contains(&i) {
                    None
                } else {
                    let index = next;
                    next += 1;
                    Some(index)
                }
            })
            .collect();
        let mut index = 0;
        self.nodes.retain(|_| {
            let keep = remap[index].is_some();
            index += 1;
            keep
        });
        for slot in &mut self.edges {
            if let Some(edge) = slot {
                if let (Some(source), Some(target)) = (remap[edge.source], remap[edge.target]) {
                    edge.source = source;
                    edge.target = target;
                } else {
                    *slot = None;
                }
            }
        }
        let filter = |rows: Vec<Neighbors>| -> Vec<Neighbors> {
            rows.into_iter()
                .enumerate()
                .filter(|(i, _)| remap[*i].is_some())
                .map(|(_, row)| row.remap(&remap))
                .collect()
        };
        self.adjacency = filter(std::mem::take(&mut self.adjacency));
        self.incoming = filter(std::mem::take(&mut self.incoming));
        self.node_index = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| Ok((key(&n.id)?, i)))
            .collect::<Result<_>>()?;
        self.edge_index = self
            .edges
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_ref().map(|e| (i, e)))
            .map(|(i, e)| {
                let (u, v) = self.pair(e.source, e.target);
                Ok(((u, v, key(&e.key)?), i))
            })
            .collect::<Result<_>>()?;
        Ok(removed.len())
    }
    /// The graph that `clone()` plus `remove_nodes_from` of every other node
    /// yields, built from the kept part only. Node, neighbor, predecessor and
    /// parallel-edge order are preserved; unreferenced edge slots are dropped.
    pub fn induced_subgraph(&self, keep: &[usize]) -> Result<Self> {
        self.induced_subgraph_with(keep, None)
    }
    /// `induced_subgraph`; with `only_edge_attribute`, of the topology alone:
    /// node and graph attributes are dropped and each edge keeps only that
    /// attribute (`topology_with_edge_attribute` of the induced graph).
    pub fn induced_subgraph_with(
        &self,
        keep: &[usize],
        only_edge_attribute: Option<&str>,
    ) -> Result<Self> {
        let mut kept: Vec<_> = keep
            .iter()
            .copied()
            .filter(|&i| i < self.nodes.len())
            .collect();
        kept.sort_unstable();
        kept.dedup();
        let mut remap = vec![None; self.nodes.len()];
        for (new, &old) in kept.iter().enumerate() {
            remap[old] = Some(new);
        }
        // Edge slots referenced between kept nodes, in their original order.
        let mut slots: Vec<_> = kept
            .iter()
            .flat_map(|&u| self.adjacency[u].iter().chain(self.incoming[u].iter()))
            .filter(|(v, _)| remap[*v].is_some())
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect();
        slots.sort_unstable();
        slots.dedup();
        let slot_of: IndexMap<usize, usize> = slots
            .iter()
            .enumerate()
            .map(|(new, &old)| (old, new))
            .collect();
        let endpoint = |old: usize| {
            remap[old]
                .ok_or_else(|| crate::Error::Internal("Induced edge leaves the subgraph".into()))
        };
        let edges = slots
            .iter()
            .map(|&old| {
                self.edges[old]
                    .as_ref()
                    .map(|edge| {
                        Ok(Edge {
                            source: endpoint(edge.source)?,
                            target: endpoint(edge.target)?,
                            key: edge.key.clone(),
                            attributes: match only_edge_attribute {
                                None => edge.attributes.clone(),
                                Some(name) => {
                                    let mut attributes = crate::record::Attributes::default();
                                    if let Some(value) = edge.attributes.get(name) {
                                        attributes.insert(name.to_string(), value.clone());
                                    }
                                    attributes
                                }
                            },
                        })
                    })
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let row = |r: &Neighbors| r.restricted(&remap, |id| slot_of[&id]);
        let nodes: Vec<_> = kept
            .iter()
            .map(|&i| match only_edge_attribute {
                None => self.nodes[i].clone(),
                Some(_) => Node {
                    id: self.nodes[i].id.clone(),
                    attributes: Default::default(),
                },
            })
            .collect();
        let mut graph = Self {
            directed: self.directed,
            multigraph: self.multigraph,
            attributes: if only_edge_attribute.is_some() {
                json!({})
            } else {
                self.attributes.clone()
            },
            node_index: nodes
                .iter()
                .enumerate()
                .map(|(i, n)| Ok((key(&n.id)?, i)))
                .collect::<Result<_>>()?,
            nodes,
            edges,
            edge_index: IndexMap::default(),
            adjacency: kept.iter().map(|&u| row(&self.adjacency[u])).collect(),
            incoming: kept.iter().map(|&u| row(&self.incoming[u])).collect(),
        };
        graph.edge_index = graph
            .edges
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_ref().map(|e| (i, e)))
            .map(|(i, e)| {
                let (u, v) = graph.pair(e.source, e.target);
                Ok(((u, v, key(&e.key)?), i))
            })
            .collect::<Result<_>>()?;
        Ok(graph)
    }
    /// Edges between two distinct members, each counted once as `iter_edges`
    /// yields it, visiting only the members' own adjacency rows. `members`
    /// must be distinct node indices.
    pub fn edges_within(&self, members: &[usize], is_member: impl Fn(usize) -> bool) -> usize {
        members
            .iter()
            .map(|&u| {
                self.adjacency
                    .get(u)
                    .into_iter()
                    .flat_map(Neighbors::iter)
                    .filter(|(v, _)| *v != u && (self.directed || *v > u) && is_member(*v))
                    .map(|(_, ids)| {
                        ids.iter()
                            .filter(|id| self.edges.get(**id).is_some_and(Option::is_some))
                            .count()
                    })
                    .sum::<usize>()
            })
            .sum()
    }
    /// NetworkX copy=True relabeling, including colliding node attributes and edge keys.
    pub fn relabel_copy(&self, mapping: &[(Value, Value)]) -> Result<Self> {
        let mapping: BTreeMap<_, _> = mapping
            .iter()
            .map(|(from, to)| Ok((key(from)?, to.clone())))
            .collect::<Result<_>>()?;
        let ids: Vec<_> = self
            .nodes
            .iter()
            .map(|n| Ok(mapping.get(&key(&n.id)?).cloned().unwrap_or(n.id.clone())))
            .collect::<Result<_>>()?;
        let mut out = Self::new(self.directed, self.multigraph);
        out.attributes = self.attributes.clone();
        for id in &ids {
            out.add_node(id.clone(), Map::new())?;
        }
        for (i, node) in self.nodes.iter().enumerate() {
            let target = out
                .node_index(&ids[i])
                .ok_or_else(|| crate::Error::Internal("Missing relabeled node".into()))?;
            out.nodes[target].attributes = node.attributes.clone();
        }
        for (u, v, edge) in self.iter_edges() {
            let mut edge_key = edge.key.clone();
            if self.multigraph {
                while out
                    .edges_between(&ids[u], &ids[v])
                    .iter()
                    .any(|e| key(&e.key).ok() == key(&edge_key).ok())
                {
                    edge_key = match edge_key {
                        Value::Bool(b) => json!(u8::from(b) + 1),
                        Value::Number(ref n) if n.is_f64() => {
                            let old = n.as_f64().unwrap();
                            let new = old + 1.0;
                            if new == old || !new.is_finite() {
                                return Err(
                                    "Edge key cannot be incremented within the JSON number domain"
                                        .into(),
                                );
                            }
                            json!(new)
                        }
                        Value::Number(ref n) => {
                            let old = n
                                .as_i64()
                                .map(i128::from)
                                .or_else(|| n.as_u64().map(i128::from))
                                .ok_or_else(|| {
                                    crate::Error::graph("Edge key exceeds JSON integer range")
                                })?;
                            let new = old + 1;
                            if new <= i64::MAX as i128 {
                                json!(new as i64)
                            } else {
                                json!(
                                    u64::try_from(new)
                                        .map_err(|_| "Edge key exceeds JSON integer range")?
                                )
                            }
                        }
                        _ => json!(1),
                    };
                }
            }
            out.add_edge(
                ids[u].clone(),
                ids[v].clone(),
                self.multigraph.then_some(edge_key),
                edge.attributes.clone(),
            )?;
        }
        Ok(out)
    }
    /// Directed edge views preserve first-neighbor and parallel-key insertion order.
    pub fn out_edges(&self, node: usize) -> Vec<(usize, usize, &Edge)> {
        self.adjacency
            .get(node)
            .into_iter()
            .flat_map(Neighbors::iter)
            .flat_map(|(target, edges)| {
                edges
                    .iter()
                    .filter_map(|&id| self.edges[id].as_ref().map(|edge| (node, *target, edge)))
            })
            .collect()
    }
    pub fn in_edges(&self, node: usize) -> Vec<(usize, usize, &Edge)> {
        self.incoming
            .get(node)
            .into_iter()
            .flat_map(Neighbors::iter)
            .flat_map(|(source, edges)| {
                edges
                    .iter()
                    .filter_map(|&id| self.edges[id].as_ref().map(|edge| (*source, node, edge)))
            })
            .collect()
    }
    /// Restore predecessor iteration order when importing an in-memory snapshot.
    /// Standard node-link JSON preserves successor order only. No edge or
    /// attribute may be added by this optional serialization metadata.
    pub fn restore_predecessor_order(&mut self, rows: &Value) -> Result<()> {
        for row in rows.as_array().ok_or("Expected predecessor order rows")? {
            let target = self
                .node_index(&row[0])
                .ok_or("Unknown predecessor target")?;
            let order: Vec<_> = row[1]
                .as_array()
                .ok_or("Expected predecessor list")?
                .iter()
                .map(|id| {
                    self.node_index(id)
                        .ok_or_else(|| crate::Error::invalid("Unknown predecessor"))
                })
                .collect::<Result<_>>()?;
            let expected: std::collections::BTreeSet<_> =
                self.incoming[target].iter().map(|(n, _)| *n).collect();
            if order.len() != expected.len()
                || order
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    != expected
            {
                return Err("Predecessor snapshot changes graph membership".into());
            }
            self.incoming[target].reorder(&order);
        }
        Ok(())
    }
    /// Preserve an in-memory adjacency view across node-link serialization.
    /// Undirected node-link rows alone cannot retain each endpoint's original
    /// neighbor order. This metadata may reorder existing neighbors only.
    pub fn restore_neighbor_order(&mut self, rows: &Value) -> Result<()> {
        for row in rows.as_array().ok_or("Expected neighbor order rows")? {
            let source = self.node_index(&row[0]).ok_or("Unknown adjacency source")?;
            let order: Vec<_> = row[1]
                .as_array()
                .ok_or("Expected neighbor list")?
                .iter()
                .map(|id| {
                    self.node_index(id)
                        .ok_or_else(|| crate::Error::invalid("Unknown neighbor"))
                })
                .collect::<Result<_>>()?;
            let expected: std::collections::BTreeSet<_> =
                self.adjacency[source].iter().map(|(n, _)| *n).collect();
            if order.len() != expected.len()
                || order
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    != expected
            {
                return Err("Neighbor snapshot changes graph membership".into());
            }
            self.adjacency[source].reorder(&order);
        }
        Ok(())
    }
    /// Source/neighbor/key iteration order matches the node-link writer.
    pub fn ordered_edges(&self) -> Vec<(usize, usize, &Edge)> {
        self.iter_edges().collect()
    }
    pub fn iter_edges(&self) -> impl Iterator<Item = (usize, usize, &Edge)> {
        self.adjacency
            .iter()
            .enumerate()
            .flat_map(move |(source, neighbors)| {
                neighbors
                    .iter()
                    .filter(move |(target, _)| self.directed || *target >= source)
                    .flat_map(move |(target, edges)| {
                        edges.iter().filter_map(move |id| {
                            self.edges
                                .get(*id)
                                .and_then(Option::as_ref)
                                .map(|edge| (source, *target, edge))
                        })
                    })
            })
    }
    pub fn neighbor_indices(&self, node: usize, incoming: bool) -> impl Iterator<Item = usize> {
        let rows = if incoming && self.directed {
            &self.incoming
        } else {
            &self.adjacency
        };
        rows.get(node)
            .into_iter()
            .flat_map(Neighbors::iter)
            .map(|(other, _)| *other)
    }
    /// Node `i` as a node-link record.
    pub fn node_link_node(&self, i: usize) -> Value {
        let node = &self.nodes[i];
        let mut fields = node.attributes.clone();
        fields.insert("id".into(), node.id.clone());
        Value::Object(fields.into())
    }
    /// An `iter_edges` item as a node-link record.
    pub fn node_link_edge(&self, u: usize, v: usize, edge: &Edge) -> Value {
        let mut fields = edge.attributes.clone();
        fields.insert("source".into(), self.nodes[u].id.clone());
        fields.insert("target".into(), self.nodes[v].id.clone());
        if self.multigraph {
            fields.insert("key".into(), edge.key.clone());
        }
        Value::Object(fields.into())
    }
    pub fn to_node_link(&self, edges_key: &str) -> Value {
        // Every record is built on its own; the document moves them in (`json!`
        // would copy each through the serializer), keeping the key order.
        let workers = crate::par::workers(None);
        let indices = (0..self.nodes.len()).collect::<Vec<_>>();
        let nodes = crate::par::map(&indices, workers, |i| self.node_link_node(*i));
        let edges = self.iter_edges().collect::<Vec<_>>();
        let edges = crate::par::map(&edges, workers, |(u, v, edge)| {
            self.node_link_edge(*u, *v, edge)
        });
        let mut out = serde_json::Map::new();
        out.insert("directed".into(), Value::Bool(self.directed));
        out.insert("multigraph".into(), Value::Bool(self.multigraph));
        out.insert("graph".into(), self.attributes.clone());
        out.insert("nodes".into(), Value::Array(nodes));
        let mut out = Value::Object(out);
        out[edges_key] = Value::Array(edges);
        out
    }
    pub fn from_node_link(data: &Value, edges_key: &str) -> Result<Self> {
        Self::from_node_link_marking(data, edges_key, false)
    }
    /// [`Self::from_node_link`] that keeps each link's stored direction as
    /// `_src`/`_tgt` (Graphify 0.9.77 `paths.restore_arc_direction`, #4066).
    /// graph.json is written undirected but carries direction in arc order
    /// (#563); an undirected load re-orders endpoints by node position.
    /// Markers a legacy file already carries win (#2309).
    pub fn from_node_link_with_arcs(data: &Value, edges_key: &str) -> Result<Self> {
        Self::from_node_link_marking(data, edges_key, true)
    }
    fn from_node_link_marking(data: &Value, edges_key: &str, arcs: bool) -> Result<Self> {
        if !data.is_object() || data.get("graph").is_some_and(|attrs| !attrs.is_object()) {
            return Err(crate::Error::graph(
                "Graph and graph attributes must be objects",
            ));
        }
        let mut graph = Self::with_capacity(
            data["directed"].as_bool().unwrap_or(false),
            data["multigraph"].as_bool().unwrap_or(true),
            data["nodes"].as_array().map_or(0, Vec::len),
            data[edges_key].as_array().map_or(0, Vec::len),
        );
        graph.attributes = data.get("graph").cloned().unwrap_or(json!({}));
        for (i, node) in data["nodes"]
            .as_array()
            .ok_or("Expected graph nodes")?
            .iter()
            .enumerate()
        {
            let mut attributes = node.as_object().ok_or("Expected node object")?.clone();
            let id = attributes.shift_remove("id").unwrap_or_else(|| json!(i));
            graph.add_node(id, attributes)?;
        }
        for edge in data[edges_key].as_array().ok_or("Expected graph edges")? {
            graph.add_node_link_edge(
                edge.as_object().ok_or("Expected edge object")?.clone(),
                arcs,
            )?;
        }
        Ok(graph)
    }
    /// Import a disposable JSON document by moving its attribute maps and IDs.
    /// Use `from_node_link` when the caller needs to retain the original JSON.
    pub fn from_node_link_owned(data: Value, edges_key: &str) -> Result<Self> {
        Self::from_node_link_owned_marking(data, edges_key, false)
    }
    /// [`Self::from_node_link_with_arcs`] of a disposable JSON document.
    pub fn from_node_link_owned_with_arcs(data: Value, edges_key: &str) -> Result<Self> {
        Self::from_node_link_owned_marking(data, edges_key, true)
    }
    fn from_node_link_owned_marking(data: Value, edges_key: &str, arcs: bool) -> Result<Self> {
        // A custom edge field may alias the node field. Retaining that unusual
        // shared view requires the borrowed loader's independent copies.
        if edges_key == "nodes" {
            return Self::from_node_link_marking(&data, edges_key, arcs);
        }
        if !data.is_object() || data.get("graph").is_some_and(|attrs| !attrs.is_object()) {
            return Err(crate::Error::graph(
                "Graph and graph attributes must be objects",
            ));
        }
        let mut graph = Self::with_capacity(
            data["directed"].as_bool().unwrap_or(false),
            data["multigraph"].as_bool().unwrap_or(true),
            data["nodes"].as_array().map_or(0, Vec::len),
            data[edges_key].as_array().map_or(0, Vec::len),
        );
        let Value::Object(mut fields) = data else {
            return Err(crate::Error::graph(
                "Graph and graph attributes must be objects",
            ));
        };
        graph.attributes = fields.shift_remove("graph").unwrap_or_else(|| json!({}));
        let Some(Value::Array(nodes)) = fields.shift_remove("nodes") else {
            return Err("Expected graph nodes".into());
        };
        for (index, node) in nodes.into_iter().enumerate() {
            let Value::Object(mut attributes) = node else {
                return Err("Expected node object".into());
            };
            let id = attributes
                .shift_remove("id")
                .unwrap_or_else(|| json!(index));
            graph.add_node(id, attributes)?;
        }
        let Some(Value::Array(edges)) = fields.remove(edges_key) else {
            return Err("Expected graph edges".into());
        };
        for edge in edges {
            let Value::Object(attributes) = edge else {
                return Err("Expected edge object".into());
            };
            graph.add_node_link_edge(attributes, arcs)?;
        }
        Ok(graph)
    }
    fn add_node_link_edge(&mut self, mut attributes: Map<String, Value>, arcs: bool) -> Result<()> {
        let source = attributes
            .shift_remove("source")
            .ok_or("KeyError: source")?;
        let target = attributes
            .shift_remove("target")
            .ok_or("KeyError: target")?;
        // node_link_graph converts one list level on edge endpoints; nested
        // lists remain unhashable (node ids, by contrast, recurse to tuples).
        for value in [&source, &target] {
            if value
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.is_array() || v.is_object()))
            {
                return Err("TypeError: unhashable endpoint".into());
            }
        }
        let key = if self.multigraph {
            attributes.shift_remove("key")
        } else {
            None
        };
        if arcs {
            // Upstream stamps the link before the endpoints are popped, so a new
            // marker follows every other attribute.
            for (marker, end) in [("_src", &source), ("_tgt", &target)] {
                if !attributes.contains_key(marker) {
                    attributes.insert(marker.into(), end.clone());
                }
            }
        }
        self.add_edge(source, target, key, attributes)?;
        Ok(())
    }
    /// The same graph without node or graph attributes: ids, edges (with their
    /// attributes), keys and neighbor order only. For passes such as community
    /// detection that never read node attributes and would otherwise copy them.
    pub fn without_node_attributes(&self) -> Self {
        Self {
            directed: self.directed,
            multigraph: self.multigraph,
            attributes: json!({}),
            nodes: self
                .nodes
                .iter()
                .map(|n| Node {
                    id: n.id.clone(),
                    attributes: Default::default(),
                })
                .collect(),
            node_index: self.node_index.clone(),
            edges: self.edges.clone(),
            edge_index: self.edge_index.clone(),
            adjacency: self.adjacency.clone(),
            incoming: self.incoming.clone(),
        }
    }
    /// `without_node_attributes` that also keeps only the `key` attribute of
    /// each edge: the whole topology at a fraction of the memory.
    pub fn topology_with_edge_attribute(&self, key: &str) -> Self {
        let mut out = Self {
            edges: vec![],
            ..self.without_nodes_and_edges()
        };
        out.edges = self
            .edges
            .iter()
            .map(|edge| {
                edge.as_ref().map(|edge| {
                    let mut attributes = crate::record::Attributes::default();
                    if let Some(value) = edge.attributes.get(key) {
                        attributes.insert(key.to_string(), value.clone());
                    }
                    Edge {
                        source: edge.source,
                        target: edge.target,
                        key: edge.key.clone(),
                        attributes,
                    }
                })
            })
            .collect();
        out
    }
    /// Node ids and lookup tables with no edge records (callers fill `edges`).
    fn without_nodes_and_edges(&self) -> Self {
        Self {
            directed: self.directed,
            multigraph: self.multigraph,
            attributes: json!({}),
            nodes: self
                .nodes
                .iter()
                .map(|n| Node {
                    id: n.id.clone(),
                    attributes: Default::default(),
                })
                .collect(),
            node_index: self.node_index.clone(),
            edges: vec![],
            edge_index: self.edge_index.clone(),
            adjacency: self.adjacency.clone(),
            incoming: self.incoming.clone(),
        }
    }
    pub fn to_undirected(&self) -> Result<Self> {
        let mut out = Self::new(false, self.multigraph);
        out.attributes = self.attributes.clone();
        for node in &self.nodes {
            out.add_node(node.id.clone(), node.attributes.clone())?;
        }
        for (u, v, edge) in self.iter_edges() {
            out.add_edge(
                self.nodes[u].id.clone(),
                self.nodes[v].id.clone(),
                self.multigraph.then(|| edge.key.clone()),
                edge.attributes.clone(),
            )?;
        }
        Ok(out)
    }
    pub fn undirected_view(&self) -> UndirectedView<'_> {
        UndirectedView(self)
    }
}
pub struct UndirectedView<'a>(&'a Graph);
impl UndirectedView<'_> {
    pub fn is_directed(&self) -> bool {
        false
    }
    pub fn is_multigraph(&self) -> bool {
        self.0.multigraph
    }
    pub fn node_count(&self) -> usize {
        self.0.nodes.len()
    }
}

pub fn check_scenario(request: &Value) -> Result<Value> {
    let mut graph = if request.get("data").is_some() {
        Graph::from_node_link(
            &request["data"],
            request["edges_key"].as_str().unwrap_or("links"),
        )?
    } else {
        Graph::new(
            request["directed"].as_bool().unwrap_or(true),
            request["multigraph"].as_bool().unwrap_or(true),
        )
    };
    let mut errors = Vec::new();
    for op in request["actions"].as_array().into_iter().flatten() {
        let attrs = op["attributes"].as_object().cloned().unwrap_or_default();
        let result = match op["kind"].as_str() {
            Some("node") => graph.add_node(op["id"].clone(), attrs).map(|_| ()),
            Some("edge") => graph
                .add_edge(
                    op["source"].clone(),
                    op["target"].clone(),
                    op.get("key").cloned(),
                    attrs,
                )
                .map(|_| ()),
            Some("remove") => {
                let _ = graph.remove_edge(&op["source"], &op["target"], op.get("key"));
                Ok(())
            }
            Some("undirected") => graph.to_undirected().map(|value| graph = value),
            Some("remove_nodes") => graph
                .remove_nodes_from(op["ids"].as_array().ok_or("Expected node ids")?)
                .map(|_| ()),
            Some("relabel") => {
                let mapping: Vec<(Value, Value)> = serde_json::from_value(op["mapping"].clone())
                    .map_err(|_| "Expected relabel pairs")?;
                graph.relabel_copy(&mapping).map(|value| graph = value)
            }
            _ => Err("Unknown graph action".into()),
        };
        if let Err(error) = result {
            errors.push(error);
        }
    }
    Ok(json!({"graph":graph.to_node_link("links"),"errors":errors,"edge_count":graph.edge_count()}))
}

#[cfg(test)]
mod induced_tests {
    use super::Graph;
    use rand::{Rng, SeedableRng};
    use rand_xorshift::XorShiftRng;
    use serde_json::{Value, json};

    /// Random graph with string/integer ids, self-loops, parallel edges,
    /// attributes and removed edges (empty slots).
    fn random_graph(rng: &mut XorShiftRng, directed: bool, multigraph: bool) -> Graph {
        let mut graph = Graph::new(directed, multigraph);
        let n = rng.gen_range(1..40);
        let id = |i: usize| {
            if i.is_multiple_of(3) {
                json!(i)
            } else {
                json!(format!("n{i}"))
            }
        };
        for i in 0..n {
            let attributes = json!({"label": format!("L{i}"), "w": i % 5});
            graph
                .add_node(id(i), attributes.as_object().unwrap().clone())
                .unwrap();
        }
        let mut added = Vec::new();
        for e in 0..rng.gen_range(0..n * 4) {
            let (u, v) = (rng.gen_range(0..n), rng.gen_range(0..n));
            let attributes = json!({"relation": format!("r{}", e % 4), "e": e});
            let key = graph
                .add_edge(id(u), id(v), None, attributes.as_object().unwrap().clone())
                .unwrap();
            added.push((id(u), id(v), key));
        }
        for (u, v, key) in added {
            if rng.gen_bool(0.15) {
                // Simple graphs merge repeated pairs, so a second removal may miss.
                let _ = graph.remove_edge(&u, &v, Some(&key));
            }
        }
        graph
    }

    fn fingerprint(graph: &Graph) -> Value {
        let id = |i: usize| graph.nodes[i].id.clone();
        let rows: Vec<_> = (0..graph.node_count())
            .map(|i| {
                let out: Vec<_> = graph
                    .out_edges(i)
                    .into_iter()
                    .map(|(_, v, e)| json!([id(v), e.key]))
                    .collect();
                let incoming: Vec<_> = graph
                    .in_edges(i)
                    .into_iter()
                    .map(|(u, _, e)| json!([id(u), e.key]))
                    .collect();
                let neighbors: Vec<_> = graph.neighbor_indices(i, false).map(id).collect();
                let predecessors: Vec<_> = graph.neighbor_indices(i, true).map(id).collect();
                let between: Vec<_> = (0..graph.node_count())
                    .map(|j| graph.number_of_edges(&id(i), &id(j)))
                    .collect();
                json!([id(i), out, incoming, neighbors, predecessors, between])
            })
            .collect();
        json!({"link": graph.to_node_link("links"), "rows": rows, "edges": graph.edge_count()})
    }

    #[test]
    fn induced_subgraph_matches_clone_and_remove() {
        let mut rng = XorShiftRng::seed_from_u64(3777);
        for round in 0..400 {
            let (directed, multigraph) = (round % 2 == 0, round % 4 < 2);
            let graph = random_graph(&mut rng, directed, multigraph);
            let keep: Vec<_> = (0..graph.node_count())
                .filter(|_| rng.gen_bool(0.6))
                .collect();
            let removed: Vec<_> = (0..graph.node_count())
                .filter(|i| !keep.contains(i))
                .map(|i| graph.nodes[i].id.clone())
                .collect();
            let mut expected = graph.clone();
            expected.remove_nodes_from(&removed).unwrap();
            let actual = graph.induced_subgraph(&keep).unwrap();
            assert_eq!(
                fingerprint(&actual),
                fingerprint(&expected),
                "round {round}"
            );
            // Both stay usable: further edits land on the same keys.
            let (mut a, mut b) = (actual, expected);
            if a.node_count() > 1 {
                let (u, v) = (a.nodes[0].id.clone(), a.nodes[1].id.clone());
                let attributes = json!({"extra": true}).as_object().unwrap().clone();
                assert_eq!(
                    a.add_edge(u.clone(), v.clone(), None, attributes.clone())
                        .unwrap(),
                    b.add_edge(u, v, None, attributes).unwrap()
                );
                assert_eq!(fingerprint(&a), fingerprint(&b), "round {round} edit");
            }
        }
    }

    #[test]
    fn edges_within_matches_a_full_edge_scan() {
        let mut rng = XorShiftRng::seed_from_u64(2107);
        for round in 0..400 {
            let graph = random_graph(&mut rng, round % 2 == 0, round % 4 < 2);
            let mut members: Vec<_> = (0..graph.node_count())
                .filter(|_| rng.gen_bool(0.5))
                .collect();
            members.dedup();
            let expected = graph
                .iter_edges()
                .filter(|(u, v, _)| u != v && members.contains(u) && members.contains(v))
                .count();
            let actual = graph.edges_within(&members, |v| members.contains(&v));
            assert_eq!(actual, expected, "round {round}");
        }
    }

    #[test]
    fn query_snapshot_roundtrip_is_exact() {
        use crate::serve_search::SearchIndex;
        let mut rng = XorShiftRng::seed_from_u64(462);
        for round in 0..300 {
            let mut graph = random_graph(&mut rng, round % 2 == 0, round % 4 < 2);
            if round % 5 == 0 && graph.node_count() > 1 {
                // A hub (indexed adjacency) and a wide record (indexed attributes).
                let hub = graph.nodes[0].id.clone();
                for i in 0..40 {
                    let attributes = json!({"relation": "hub", "i": i, "w": 1.0});
                    graph
                        .add_edge(
                            hub.clone(),
                            json!(format!("spoke{i}")),
                            None,
                            attributes.as_object().unwrap().clone(),
                        )
                        .unwrap();
                }
                let wide = (0..70)
                    .map(|i| (format!("k{i}"), json!({"n": [i, 1.5, "é"], "t": null})))
                    .collect::<serde_json::Map<_, _>>();
                graph.add_node(hub, wide).unwrap();
                graph.attributes =
                    json!({"hyperedges": [{"id": "h", "nodes": ["a", 1]}], "x": 1e5});
            }
            let index = SearchIndex::new(&graph);
            let bytes = crate::snapshot::encode(b"", &graph, &index);
            let (back, back_index) = crate::snapshot::decode(&bytes[8..]).unwrap();
            assert_eq!(fingerprint(&back), fingerprint(&graph), "round {round}");
            assert_eq!(
                serde_json::to_string(&back.to_node_link("links")).unwrap(),
                serde_json::to_string(&graph.to_node_link("links")).unwrap(),
                "round {round}"
            );
            assert_eq!(back_index.postings(), index.postings(), "round {round}");
            for i in 0..graph.node_count() {
                let (a, b) = (index.text(i), back_index.text(i));
                assert_eq!((&a.norm, &a.label, &a.id), (&b.norm, &b.label, &b.id));
            }
            // Both stay usable: further edits land on the same keys.
            let (mut a, mut b) = (graph, back);
            if a.node_count() > 1 {
                let (u, v) = (a.nodes[0].id.clone(), a.nodes[1].id.clone());
                let attributes = json!({"extra": true}).as_object().unwrap().clone();
                assert_eq!(
                    a.add_edge(u.clone(), v.clone(), None, attributes.clone())
                        .unwrap(),
                    b.add_edge(u.clone(), v.clone(), None, attributes).unwrap()
                );
                let _ = a.remove_edge(&u, &v, None);
                let _ = b.remove_edge(&u, &v, None);
                assert_eq!(fingerprint(&a), fingerprint(&b), "round {round} edit");
            }
        }
    }

    #[test]
    fn query_snapshot_roundtrip_spans_chunks() {
        use crate::serve_search::SearchIndex;
        let mut rng = XorShiftRng::seed_from_u64(1313);
        for (directed, multigraph) in [(false, false), (true, true)] {
            let mut graph = Graph::new(directed, multigraph);
            let n = 3 * crate::snapshot::CHUNK + 17;
            for i in 0..n {
                let attributes =
                    json!({"label": format!("node {i}"), "source_file": format!("f{}.py", i % 97)});
                graph
                    .add_node(
                        json!(format!("n{i}")),
                        attributes.as_object().unwrap().clone(),
                    )
                    .unwrap();
            }
            for e in 0..3 * n {
                let (u, v) = (rng.gen_range(0..n), rng.gen_range(0..n));
                let attributes = json!({"relation": "calls", "weight": 1.0, "e": e});
                graph
                    .add_edge(
                        json!(format!("n{u}")),
                        json!(format!("n{v}")),
                        None,
                        attributes.as_object().unwrap().clone(),
                    )
                    .unwrap();
            }
            let index = SearchIndex::new(&graph);
            let bytes = crate::snapshot::encode(b"", &graph, &index);
            let (back, back_index) = crate::snapshot::decode(&bytes[8..]).unwrap();
            // Linear-size fingerprint (the pairwise one is quadratic).
            let rows = |g: &Graph| {
                (0..g.node_count())
                    .map(|i| {
                        let out: Vec<_> = g
                            .out_edges(i)
                            .into_iter()
                            .map(|(_, v, e)| (v, e.key.clone()))
                            .collect();
                        let inc: Vec<_> = g
                            .in_edges(i)
                            .into_iter()
                            .map(|(u, _, e)| (u, e.key.clone()))
                            .collect();
                        let nb: Vec<_> = g.neighbor_indices(i, false).collect();
                        let pr: Vec<_> = g.neighbor_indices(i, true).collect();
                        format!("{out:?}{inc:?}{nb:?}{pr:?}")
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(rows(&back), rows(&graph));
            assert_eq!(
                serde_json::to_string(&back.to_node_link("links")).unwrap(),
                serde_json::to_string(&graph.to_node_link("links")).unwrap()
            );
            assert_eq!(back.edge_count(), graph.edge_count());
            assert_eq!(back_index.postings(), index.postings());
        }
    }

    #[test]
    fn corrupt_query_snapshots_are_rejected() {
        let mut rng = XorShiftRng::seed_from_u64(1206);
        let graph = random_graph(&mut rng, false, false);
        let index = crate::serve_search::SearchIndex::new(&graph);
        let bytes = crate::snapshot::encode(b"", &graph, &index);
        let body = &bytes[8..];
        for cut in (0..body.len()).step_by(7) {
            assert!(crate::snapshot::decode(&body[..cut]).is_err(), "cut {cut}");
        }
        let mut flipped = body.to_vec();
        for i in (0..flipped.len()).step_by(3) {
            flipped[i] ^= 0x5a;
            let _ = crate::snapshot::decode(&flipped);
            flipped[i] ^= 0x5a;
        }
    }
}
