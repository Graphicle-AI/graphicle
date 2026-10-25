// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/affected.py, graphify/global_graph.py, graphify/multigraph_compat.py
//! Native attributed graph substrate for the translated Graphify modules.
//! Preserves insertion order, keyed parallel edges and node-link JSON semantics.
use crate::{
    Result,
    graph_adjacency::{GROUP, Neighbors},
    python_compat::{self, ScalarKey},
    vtab::{self, VId},
};
use hashbrown::HashTable;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    fmt,
    hash::{BuildHasher, Hash, Hasher},
    ops::Deref,
};
// Identifier indices are never iterated, so a fast non-cryptographic hasher
// cannot change any observable ordering.
#[doc(hidden)]
pub type FastState = foldhash::fast::RandomState;
type IndexMap<K, V> = hashbrown::HashMap<K, V, FastState>;

/// An owned identifier key (Python hash semantics: `1 == 1.0 == True`), for
/// the ordered maps of `relabel_copy`. The indices hash and compare the
/// stored identifiers themselves (`KeyRef`, `same_key`).
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
/// A scalar key hashed exactly as `KeyRef` hashes a value with that key.
struct ScalarRef<'a>(&'a ScalarKey);
impl Hash for ScalarRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_scalar(self.0, state);
    }
}
/// `key(a) == key(b)` for identifiers, without building either key: equal
/// strings, element-wise equal tuples, or equal scalars (`1 == 1.0 == True`).
fn same_key(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_key(a, b))
        }
        (Value::String(_) | Value::Array(_), _) | (_, Value::String(_) | Value::Array(_)) => false,
        (a, b) => python_compat::scalar_key(a)
            .is_some_and(|a| python_compat::scalar_key(b).as_ref() == Some(&a)),
    }
}
/// Whether `key(value)` succeeds, without building the key.
fn hashable(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Array(items) => items.iter().all(hashable),
        value => python_compat::scalar_key(value).is_some(),
    }
}
fn unhashable() -> crate::Error {
    crate::Error::graph("TypeError: unhashable graph identifier")
}
fn pair_of(directed: bool, source: usize, target: usize) -> (usize, usize) {
    if !directed && source > target {
        (target, source)
    } else {
        (source, target)
    }
}
fn node_hash(state: &FastState, id: &Value) -> u64 {
    state.hash_one(KeyRef(id))
}
fn edge_hash(state: &FastState, (u, v): (usize, usize), key: &Value) -> u64 {
    state.hash_one((u, v, KeyRef(key)))
}
/// The `edge_hash` of a live edge slot (0 for an empty slot, which the
/// index never holds).
fn slot_hash(state: &FastState, directed: bool, edges: &[Option<Edge>], slot: u32) -> u64 {
    edges[slot as usize].as_ref().map_or(0, |edge| {
        edge_hash(
            state,
            pair_of(directed, edge.source, edge.target),
            &edge.key,
        )
    })
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
/// An edge's key: null on a simple graph; on a multigraph the integer the
/// graph chose or the scalar the caller gave. Four bytes: the value lives
/// once in the process-wide table (`crate::vtab`), against 72 bytes for an
/// inline `Value`. It reads as that `&Value` (`Deref`), compares, formats and
/// serializes exactly as the value does, and two keys are equal exactly when
/// their values are (the table keeps one id per kind and exact text).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EdgeKey(VId);
impl EdgeKey {
    /// The key of every simple-graph edge.
    pub const NULL: Self = Self(VId::NULL);
    /// The key holding the scalar `value`. Arrays and objects cannot be
    /// edge keys and come back unchanged as the error.
    pub fn new(value: Value) -> std::result::Result<Self, Value> {
        vtab::intern(value).map(Self)
    }
    /// The key equal to the scalar `value` (cloned only when it is new to
    /// the table); `None` for an array or an object.
    pub fn of(value: &Value) -> Option<Self> {
        vtab::intern_ref(value).map(Self)
    }
    /// The key's value, valid for the life of the process.
    pub fn value(self) -> &'static Value {
        self.0.value()
    }
    /// An owned copy of the key's value.
    pub fn to_value(self) -> Value {
        self.value().clone()
    }
}
impl Deref for EdgeKey {
    type Target = Value;
    fn deref(&self) -> &Value {
        self.value()
    }
}
impl PartialEq<Value> for EdgeKey {
    fn eq(&self, other: &Value) -> bool {
        self.value() == other
    }
}
impl PartialEq<EdgeKey> for Value {
    fn eq(&self, other: &EdgeKey) -> bool {
        self == other.value()
    }
}
impl fmt::Debug for EdgeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.value(), f)
    }
}
impl fmt::Display for EdgeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.value(), f)
    }
}
impl Serialize for EdgeKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.value().serialize(serializer)
    }
}
impl From<EdgeKey> for Value {
    fn from(key: EdgeKey) -> Self {
        key.to_value()
    }
}
#[derive(Clone, Debug)]
pub struct Edge {
    pub source: usize,
    pub target: usize,
    pub key: EdgeKey,
    pub attributes: crate::record::Attributes,
}
#[derive(Clone, Debug)]
pub struct Graph {
    pub(crate) directed: bool,
    pub(crate) multigraph: bool,
    pub(crate) attributes: Value,
    pub(crate) nodes: Vec<Node>,
    /// Seeds the hashes of `node_index` and `edge_index`.
    state: FastState,
    /// Position of every node, hashed and compared through `nodes[i].id`
    /// (`KeyRef`, `same_key`), so no copy of an identifier is kept.
    node_index: HashTable<u32>,
    edges: Vec<Option<Edge>>,
    /// Number of live edge slots.
    live: usize,
    /// Multigraphs only: the slot of every live edge, by `(pair, key)`. A
    /// simple graph has one edge per pair and finds it in an adjacency row.
    edge_index: HashTable<u32>,
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
            state: FastState::default(),
            node_index: HashTable::with_capacity(nodes),
            edges: Vec::with_capacity(edges),
            live: 0,
            edge_index: HashTable::with_capacity(if multigraph { edges } else { 0 }),
            adjacency: Vec::with_capacity(nodes),
            incoming: Vec::with_capacity(nodes),
        }
    }
    /// Room for `additional` more edges, so that a build that knows how many
    /// edges it adds grows the edge list once, exactly, instead of doubling.
    pub fn reserve_edges(&mut self, additional: usize) {
        self.edges.reserve_exact(additional);
        if self.multigraph {
            let Self {
                state,
                directed,
                edges,
                edge_index,
                ..
            } = self;
            edge_index.reserve(additional, |&slot| slot_hash(state, *directed, edges, slot));
        }
    }
    /// Releases spare capacity of the node and edge lists, the indices and
    /// the adjacency rows of a complete graph. Order and contents are
    /// unchanged.
    pub fn shrink_to_fit(&mut self) {
        let Self {
            directed,
            nodes,
            state,
            node_index,
            edges,
            edge_index,
            adjacency,
            incoming,
            ..
        } = self;
        nodes.shrink_to_fit();
        node_index.shrink_to(0, |&i| node_hash(state, &nodes[i as usize].id));
        edges.shrink_to_fit();
        edge_index.shrink_to(0, |&slot| slot_hash(state, *directed, edges, slot));
        for row in adjacency.iter_mut().chain(incoming.iter_mut()) {
            row.shrink_to_fit();
        }
        adjacency.shrink_to_fit();
        incoming.shrink_to_fit();
    }
    pub fn node_index(&self, id: &Value) -> Option<usize> {
        let nodes = &self.nodes;
        self.node_index
            .find(node_hash(&self.state, id), |&i| {
                same_key(id, &nodes[i as usize].id)
            })
            .map(|&i| i as usize)
    }
    /// Indexes `nodes[index]`, which is not indexed yet.
    fn index_node(&mut self, index: usize) {
        let Self {
            nodes,
            state,
            node_index,
            ..
        } = self;
        node_index.insert_unique(node_hash(state, &nodes[index].id), index as u32, |&i| {
            node_hash(state, &nodes[i as usize].id)
        });
    }
    /// `node_index` rebuilt for the current node list.
    fn reindex_nodes(&mut self) {
        self.node_index = HashTable::with_capacity(self.nodes.len());
        for index in 0..self.nodes.len() {
            self.index_node(index);
        }
    }
    /// Recounts the live edges and rebuilds a multigraph's `edge_index`.
    /// With `check` (snapshot loads) every key must be hashable and every
    /// `(pair, key)` unique, on simple graphs too.
    fn reindex_edges(&mut self, check: bool) -> Result<()> {
        self.live = self.edges.iter().filter(|edge| edge.is_some()).count();
        if !self.multigraph && !check {
            self.edge_index = HashTable::new();
            return Ok(());
        }
        let Self {
            directed,
            state,
            edges,
            ..
        } = &*self;
        let mut index = HashTable::with_capacity(self.live);
        for (slot, edge) in edges.iter().enumerate() {
            let Some(edge) = edge else { continue };
            if check && !hashable(&edge.key) {
                return Err(unhashable());
            }
            let pair = pair_of(*directed, edge.source, edge.target);
            let hash = edge_hash(state, pair, &edge.key);
            if check
                && index
                    .find(hash, |&other: &u32| {
                        edges[other as usize].as_ref().is_some_and(|e| {
                            pair_of(*directed, e.source, e.target) == pair
                                && same_key(&e.key, &edge.key)
                        })
                    })
                    .is_some()
            {
                return Err(crate::snapshot::corrupt("duplicate edge"));
            }
            index.insert_unique(hash, slot as u32, |&other| {
                slot_hash(state, *directed, edges, other)
            });
        }
        // A simple graph keeps no edge index; the check was all it was for.
        self.edge_index = if self.multigraph {
            index
        } else {
            HashTable::new()
        };
        Ok(())
    }
    /// The live edge `(pair, key)` of a multigraph.
    fn find_keyed(&self, pair: (usize, usize), key: &Value) -> Option<usize> {
        let (directed, edges) = (self.directed, &self.edges);
        self.edge_index
            .find(edge_hash(&self.state, pair, key), |&slot| {
                edges[slot as usize].as_ref().is_some_and(|edge| {
                    pair_of(directed, edge.source, edge.target) == pair && same_key(&edge.key, key)
                })
            })
            .map(|&slot| slot as usize)
    }
    /// Whether a multigraph has a live edge `(pair, candidate)`.
    fn has_integer_key(&self, pair: (usize, usize), candidate: u64) -> bool {
        let key = ScalarKey::Integer(candidate.into());
        let hash = self.state.hash_one((pair.0, pair.1, ScalarRef(&key)));
        let (directed, edges) = (self.directed, &self.edges);
        self.edge_index
            .find(hash, |&slot| {
                edges[slot as usize].as_ref().is_some_and(|edge| {
                    pair_of(directed, edge.source, edge.target) == pair
                        && !matches!(*edge.key, Value::String(_) | Value::Array(_))
                        && python_compat::scalar_key(&edge.key).as_ref() == Some(&key)
                })
            })
            .is_some()
    }
    /// Adds the live edge `slot` to a multigraph's `edge_index`.
    fn index_edge(&mut self, slot: usize) {
        let Self {
            directed,
            state,
            edges,
            edge_index,
            ..
        } = self;
        edge_index.insert_unique(
            slot_hash(state, *directed, edges, slot as u32),
            slot as u32,
            |&other| slot_hash(state, *directed, edges, other),
        );
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
        // Rows store node ids as u32 and edge ids below `GROUP`.
        if u32::try_from(n).is_err() || m > GROUP as usize {
            return Err(corrupt("graph size"));
        }
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
        graph.node_index = HashTable::with_capacity(graph.nodes.len());
        for index in 0..graph.nodes.len() {
            let id = &graph.nodes[index].id;
            if id.is_null() || !valid_identifier(id) {
                return Err(corrupt("node identifier"));
            }
            if !hashable(id) {
                return Err(unhashable());
            }
            if graph.node_index(id).is_some() {
                return Err(corrupt("duplicate node"));
            }
            graph.index_node(index);
        }
        let edges = d.chunks(chunk_count(m), |r, i| {
            (0..part(m, i))
                .map(|_| {
                    Ok(match r.u8()? {
                        0 => None,
                        1 => Some(Edge {
                            source: r.index(n)?,
                            target: r.index(n)?,
                            // The writer only ever holds scalar keys.
                            key: EdgeKey::new(r.value()?).map_err(|key| {
                                if key.is_object() {
                                    unhashable()
                                } else {
                                    corrupt("edge key")
                                }
                            })?,
                            attributes: crate::record::Attributes::read_snapshot(r)?,
                        }),
                        _ => return Err(corrupt("edge slot")),
                    })
                })
                .collect::<Result<Vec<_>>>()
        })?;
        graph.edges = edges.into_iter().flatten().collect();
        graph.reindex_edges(true)?;
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
        if let Some(index) = self.node_index(&id) {
            self.nodes[index].attributes.extend_record(attributes);
            return Ok(index);
        }
        if !hashable(&id) {
            return Err(unhashable());
        }
        let index = self.nodes.len();
        if u32::try_from(index).is_err() {
            return Err(crate::Error::graph("Graph node limit reached"));
        }
        self.nodes.push(Node { id, attributes });
        self.index_node(index);
        self.adjacency.push(Neighbors::default());
        self.incoming.push(Neighbors::default());
        Ok(index)
    }
    fn pair(&self, source: usize, target: usize) -> (usize, usize) {
        pair_of(self.directed, source, target)
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
            while self.has_integer_key((u, v), candidate) {
                candidate += 1;
            }
            json!(candidate)
        };
        // A simple graph keeps one edge per pair, the one its row lists.
        let existing = if self.multigraph {
            self.find_keyed((u, v), &edge_key)
        } else {
            self.adjacency[source]
                .get(target)
                .and_then(|ids| ids.first())
                .map(|&id| id as usize)
        };
        if let Some(index) = existing {
            self.edges
                .get_mut(index)
                .and_then(Option::as_mut)
                .ok_or_else(|| crate::Error::Internal("Graph edge index is inconsistent".into()))?
                .attributes
                .extend(attributes);
            return Ok(edge_key);
        }
        let index = self.edges.len();
        if index >= GROUP as usize {
            return Err(crate::Error::graph("Graph edge limit reached"));
        }
        // Simple-graph keys are null; a multigraph key is a checked scalar.
        let key = EdgeKey::of(&edge_key).ok_or_else(unhashable)?;
        self.edges.push(Some(Edge {
            source,
            target,
            key,
            attributes,
        }));
        self.live += 1;
        if self.multigraph {
            self.index_edge(index);
        }
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
        self.live
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
            Some(key) => *ids.iter().find(|&&id| {
                self.edges[id as usize]
                    .as_ref()
                    .is_some_and(|edge| &edge.key == key)
            })?,
        };
        self.edges[id as usize]
            .as_mut()
            .map(|edge| &mut edge.attributes)
    }
    pub fn edges_between(&self, source: &Value, target: &Value) -> Vec<&Edge> {
        let (Some(u), Some(v)) = (self.node_index(source), self.node_index(target)) else {
            return vec![];
        };
        self.adjacency[u]
            .get(v)
            .into_iter()
            .flatten()
            .filter_map(|&id| self.edges[id as usize].as_ref())
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
            if !hashable(k) {
                return Err(unhashable());
            }
            self.find_keyed(pair, k)
        } else {
            self.adjacency[u]
                .get(v)
                .and_then(|edges| edges.last())
                .map(|&id| id as usize)
        }
        .ok_or("Edge not found")?;
        let edge = self
            .edges
            .get_mut(edge_index)
            .and_then(Option::take)
            .ok_or_else(|| crate::Error::Internal("Graph edge index is inconsistent".into()))?;
        self.live -= 1;
        if self.multigraph
            && let Ok(entry) = self
                .edge_index
                .find_entry(edge_hash(&self.state, pair, &edge.key), |&slot| {
                    slot as usize == edge_index
                })
        {
            entry.remove();
        }
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
        self.reindex_nodes();
        self.reindex_edges(false)?;
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
            .flat_map(|(_, ids)| ids.iter().map(|&id| id as usize))
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
                            key: edge.key,
                            attributes: match only_edge_attribute {
                                None => edge.attributes.clone(),
                                Some(name) => edge
                                    .attributes
                                    .get(name)
                                    .map_or_else(Default::default, |value| {
                                        crate::record::Attributes::single(name, value.clone())
                                    }),
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
            nodes,
            state: self.state.clone(),
            node_index: HashTable::new(),
            edges,
            live: 0,
            edge_index: HashTable::new(),
            adjacency: kept.iter().map(|&u| row(&self.adjacency[u])).collect(),
            incoming: kept.iter().map(|&u| row(&self.incoming[u])).collect(),
        };
        graph.reindex_nodes();
        graph.reindex_edges(false)?;
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
                            .filter(|&&id| self.edges.get(id as usize).is_some_and(Option::is_some))
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
            let mut edge_key = edge.key.to_value();
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
                edges.iter().filter_map(move |&id| {
                    self.edges[id as usize]
                        .as_ref()
                        .map(|edge| (node, target, edge))
                })
            })
            .collect()
    }
    pub fn in_edges(&self, node: usize) -> Vec<(usize, usize, &Edge)> {
        self.incoming
            .get(node)
            .into_iter()
            .flat_map(Neighbors::iter)
            .flat_map(|(source, edges)| {
                edges.iter().filter_map(move |&id| {
                    self.edges[id as usize]
                        .as_ref()
                        .map(|edge| (source, node, edge))
                })
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
                self.incoming[target].iter().map(|(n, _)| n).collect();
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
                self.adjacency[source].iter().map(|(n, _)| n).collect();
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
                                .get(*id as usize)
                                .and_then(Option::as_ref)
                                .map(|edge| (source, target, edge))
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
            .map(|(other, _)| other)
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
            fields.insert("key".into(), edge.key.to_value());
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
            state: self.state.clone(),
            node_index: self.node_index.clone(),
            edges: self.edges.clone(),
            live: self.live,
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
                    let attributes = edge
                        .attributes
                        .get(key)
                        .map_or_else(Default::default, |value| {
                            crate::record::Attributes::single(key, value.clone())
                        });
                    Edge {
                        source: edge.source,
                        target: edge.target,
                        key: edge.key,
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
            state: self.state.clone(),
            node_index: self.node_index.clone(),
            edges: vec![],
            // The caller fills `edges` slot for slot from this graph.
            live: self.live,
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
                self.multigraph.then(|| edge.key.to_value()),
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
#[path = "graph_parity_tests.rs"]
mod parity_tests;

#[cfg(test)]
mod edge_key_tests {
    use super::{Edge, EdgeKey, Graph};
    use serde_json::{Value, json};

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn an_edge_slot_is_56_bytes() {
        // Endpoints 2 x 8, key 4, attributes 32: the key no longer costs a
        // 72-byte Value, and an empty slot needs no extra tag.
        assert_eq!(std::mem::size_of::<EdgeKey>(), 4);
        assert_eq!(std::mem::size_of::<Option<Edge>>(), 56);
    }

    #[test]
    fn a_key_reads_compares_formats_and_serializes_as_its_value() {
        // Numbers keep their exact text: `1`, `1.0`, `1e-7` and `1E-7` differ.
        let values: Vec<Value> = serde_json::from_str(
            r#"[null, true, false, 0, 1, 1.0, -0.0, 1e-7, 1E-7, 0.55,
                18446744073709551616, "1", "k", "", "é"]"#,
        )
        .unwrap();
        let keys: Vec<EdgeKey> = values
            .iter()
            .map(|v| EdgeKey::new(v.clone()).unwrap())
            .collect();
        for (i, (value, key)) in values.iter().zip(&keys).enumerate() {
            assert_eq!(&**key, value);
            assert_eq!(*key, *value);
            assert_eq!(*value, *key);
            assert_eq!(EdgeKey::of(value), Some(*key));
            assert_eq!(key.to_value(), *value);
            assert_eq!(Value::from(*key), *value);
            assert_eq!(key.to_string(), value.to_string());
            assert_eq!(format!("{key:?}"), format!("{value:?}"));
            assert_eq!(format!("{key:#}"), format!("{value:#}"));
            assert_eq!(
                serde_json::to_string(key).unwrap(),
                serde_json::to_string(value).unwrap()
            );
            assert_eq!(json!([key]), json!([value]));
            for (other_value, other_key) in values.iter().zip(&keys).skip(i + 1) {
                assert_eq!(
                    key == other_key,
                    value == other_value,
                    "{value} {other_value}"
                );
            }
        }
        assert_eq!(keys[0], EdgeKey::NULL);
        for complex in [json!([1]), json!(["k"]), json!({"k": 1}), json!([])] {
            assert_eq!(EdgeKey::new(complex.clone()), Err(complex.clone()));
            assert_eq!(EdgeKey::of(&complex), None);
        }
    }

    #[test]
    fn edges_keep_their_keys_through_every_copy() {
        for multigraph in [false, true] {
            let mut graph = Graph::new(true, multigraph);
            for (key, relation) in [(None, "calls"), (Some(json!("k")), "uses"), (None, "calls")] {
                let fields = json!({"relation": relation});
                graph
                    .add_edge(
                        json!("a"),
                        json!("b"),
                        key,
                        fields.as_object().unwrap().clone(),
                    )
                    .unwrap();
            }
            let keys = |g: &Graph| -> Vec<Value> {
                g.iter_edges().map(|(_, _, e)| e.key.to_value()).collect()
            };
            let expected = if multigraph {
                vec![json!(0), json!("k"), json!(2)]
            } else {
                vec![Value::Null]
            };
            assert_eq!(keys(&graph), expected);
            assert!(
                format!("{:?}", graph.edges_between(&json!("a"), &json!("b"))[0])
                    .contains(&format!("key: {:?}", expected[0]))
            );
            for copy in [
                graph.clone(),
                graph.without_node_attributes(),
                graph.topology_with_edge_attribute("relation"),
                graph.induced_subgraph(&[0, 1]).unwrap(),
                graph.to_undirected().unwrap(),
                graph.relabel_copy(&[]).unwrap(),
                Graph::from_node_link(&graph.to_node_link("links"), "links").unwrap(),
            ] {
                assert_eq!(keys(&copy), expected, "multigraph {multigraph}");
            }
            let link = graph.to_node_link("links");
            let link_keys: Vec<_> = link["links"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.get("key").cloned())
                .collect();
            if multigraph {
                assert_eq!(
                    link_keys,
                    expected.into_iter().map(Some).collect::<Vec<_>>()
                );
            } else {
                assert_eq!(link_keys, vec![None]);
            }
        }
    }
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
    fn reserving_and_shrinking_change_nothing() {
        let mut rng = XorShiftRng::seed_from_u64(515);
        for round in 0..200 {
            let graph = random_graph(&mut rng, round % 2 == 0, round % 4 < 2);
            let mut reserved = graph.clone();
            reserved.reserve_edges(rng.gen_range(0..50));
            let mut shrunk = graph.clone();
            shrunk.shrink_to_fit();
            let mut edited = graph.clone();
            for copy in [&mut reserved, &mut shrunk] {
                assert_eq!(fingerprint(copy), fingerprint(&graph), "round {round}");
                if copy.node_count() > 1 {
                    let (u, v) = (copy.nodes[0].id.clone(), copy.nodes[1].id.clone());
                    let attributes = json!({"extra": true}).as_object().unwrap().clone();
                    let key = copy.add_edge(u.clone(), v.clone(), None, attributes.clone());
                    let expected = edited.add_edge(u.clone(), v.clone(), None, attributes);
                    assert_eq!(key.unwrap(), expected.unwrap(), "round {round}");
                    copy.remove_edge(&u, &v, None).unwrap();
                    edited.remove_edge(&u, &v, None).unwrap();
                    assert_eq!(fingerprint(copy), fingerprint(&edited), "round {round}");
                    edited = graph.clone();
                }
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
                            .map(|(_, v, e)| (v, e.key))
                            .collect();
                        let inc: Vec<_> = g
                            .in_edges(i)
                            .into_iter()
                            .map(|(u, _, e)| (u, e.key))
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
