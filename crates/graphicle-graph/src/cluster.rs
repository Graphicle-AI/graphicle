// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/cluster.py
//! Community policies translated from graphify/cluster.py. The Leiden kernel is
//! the very same native dependency used by the pinned Python implementation.
use crate::{Result, graph::Graph, python_compat, python_json};
use network_partitions::{leiden, network::prelude::*};
use rand::SeedableRng;
use rand_xorshift::XorShiftRng;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashSet},
};

pub type Communities = Vec<(i64, Vec<Value>)>;
pub type Partition = Vec<(Value, i64)>;

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}

fn cmp_id(a: &Value, b: &Value) -> Result<Ordering> {
    match (a, b) {
        (Value::String(a), Value::String(b)) => Ok(a.cmp(b)),
        (Value::Array(a), Value::Array(b)) => {
            for (a, b) in a.iter().zip(b) {
                let c = cmp_id(a, b)?;
                if c != Ordering::Equal {
                    return Ok(c);
                }
            }
            Ok(a.len().cmp(&b.len()))
        }
        _ => {
            let number = |v: &Value| {
                v.as_f64()
                    .or_else(|| v.as_bool().map(|x| f64::from(u8::from(x))))
            };
            match (number(a), number(b)) {
                (Some(a), Some(b)) => a
                    .partial_cmp(&b)
                    .ok_or_else(|| "Unordered node identifier".into()),
                _ if a == b => Ok(Ordering::Equal),
                _ => Err("TypeError: node identifiers do not support comparison".into()),
            }
        }
    }
}
pub(crate) fn sorted_ids(nodes: &[Value]) -> Result<Vec<Value>> {
    let mut nodes = nodes.to_vec();
    let mut failure = None;
    nodes.sort_by(|a, b| {
        cmp_id(a, b).unwrap_or_else(|error| {
            failure = Some(error);
            Ordering::Equal
        })
    });
    if let Some(error) = failure {
        Err(error)
    } else {
        Ok(nodes)
    }
}
fn strings(nodes: &[Value]) -> Vec<String> {
    let mut values: Vec<_> = nodes.iter().map(python_compat::string).collect();
    values.sort();
    values
}
fn sorted_json(value: &Value) -> Value {
    match value {
        Value::Object(values) => {
            let ordered: BTreeMap<_, _> = values.iter().collect();
            Value::Object(
                ordered
                    .into_iter()
                    .map(|(k, v)| (k.clone(), sorted_json(v)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(sorted_json).collect()),
        _ => value.clone(),
    }
}
fn stable_graph(graph: &Graph) -> Result<Graph> {
    let mut stable = Graph::new(false, false);
    // Each id's text, computed once per node rather than per comparison/row.
    let texts = graph
        .nodes
        .iter()
        .map(|n| python_compat::string(&n.id))
        .collect::<Vec<_>>();
    let mut order = (0..graph.nodes.len()).collect::<Vec<_>>();
    order.sort_by(|a, b| texts[*a].cmp(&texts[*b]));
    for i in order {
        stable.add_node(graph.nodes[i].id.clone(), Map::new())?;
    }
    let mut rows = Vec::new();
    for (u, v, edge) in graph.iter_edges() {
        let a = texts[u].clone();
        let b = texts[v].clone();
        let (low, high) = if a <= b { (a, b) } else { (b, a) };
        let mut attributes = Vec::new();
        python_json::write(
            &mut attributes,
            &sorted_json(&Value::Object(edge.attributes.to_map())),
            None,
            false,
        )
        .map_err(crate::Error::from)?;
        rows.push(((low, high, attributes), u, v, edge));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, u, v, edge) in rows {
        // Both partition backends read only `weight`; parallel rows still merge
        // it last-wins exactly as merging whole attribute records would.
        let mut attributes = crate::record::Attributes::default();
        if let Some(weight) = edge.attributes.get("weight") {
            attributes.insert("weight".into(), weight.clone());
        }
        stable.add_edge(
            graph.nodes[u].id.clone(),
            graph.nodes[v].id.clone(),
            None,
            attributes,
        )?;
    }
    Ok(stable)
}
fn weight(attributes: &impl crate::record::Properties, coerce: bool) -> Result<f64> {
    let Some(value) = attributes.get("weight") else {
        return Ok(1.0);
    };
    value
        .as_f64()
        .or_else(|| value.as_bool().map(|v| f64::from(u8::from(v))))
        .or_else(|| {
            if coerce {
                value.as_str().and_then(|s| s.trim().parse().ok())
            } else {
                None
            }
        })
        .ok_or_else(|| "TypeError: nonnumeric edge weight".into())
}

pub fn native_leiden(graph: &Graph, resolution: f64) -> Result<Option<Partition>> {
    if graph.directed || graph.multigraph {
        return Ok(None);
    }
    let texts = graph
        .nodes
        .iter()
        .map(|n| python_compat::string(&n.id))
        .collect::<Vec<_>>();
    let mut names = BTreeMap::new();
    for (node, key) in graph.nodes.iter().zip(&texts) {
        if let Some(previous) = names.insert(key.clone(), node.id.clone())
            && previous != node.id
        {
            return Ok(None);
        }
    }
    let mut edges = Vec::new();
    for (u, v, edge) in graph.iter_edges() {
        edges.push((
            texts[u].clone(),
            texts[v].clone(),
            weight(&edge.attributes, true)?,
        ));
    }
    // Binding exceptions fall through in Graphify. Validate before invoking the
    // kernel, whose invalid-parameter paths otherwise have no Python exception.
    if !resolution.is_finite() || resolution <= 0.0 || edges.is_empty() {
        return Ok(None);
    }
    if edges.iter().any(|e| !e.2.is_finite()) {
        return Ok(None);
    }
    let mut builder = LabeledNetworkBuilder::<String>::new();
    let network = builder.build(edges.into_iter(), true);
    let mut rng = XorShiftRng::seed_from_u64(42);
    let Ok((_, clustering)) = leiden::leiden(
        network.compact(),
        None,
        Some(1),
        Some(resolution),
        Some(0.001),
        &mut rng,
        true,
    ) else {
        return Ok(None);
    };
    let mut result = Vec::new();
    let mut included = HashSet::new();
    for item in &clustering {
        let name = network.label_for(item.node_id);
        if let Some(node) = names.get(name) {
            result.push((node.clone(), item.cluster as i64));
            included.insert(name.clone());
        }
    }
    let mut next = result.iter().map(|(_, c)| *c).max().unwrap_or(-1) + 1;
    for (node, text) in graph.nodes.iter().zip(&texts) {
        if !included.contains(text) {
            result.push((node.id.clone(), next));
            next += 1;
        }
    }
    Ok(Some(result))
}

// NetworkX's fallback uses CPython random.Random(42), not NumPy's seeding.
pub(crate) struct PythonRandom {
    state: [u32; 624],
    index: usize,
}
impl PythonRandom {
    pub(crate) fn new(seed: u32) -> Self {
        let mut out = Self {
            state: [0; 624],
            index: 624,
        };
        out.state[0] = 19650218;
        for i in 1..624 {
            out.state[i] = 1812433253u32
                .wrapping_mul(out.state[i - 1] ^ (out.state[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        let mut i = 1;
        for _ in 0..624 {
            out.state[i] = (out.state[i]
                ^ ((out.state[i - 1] ^ (out.state[i - 1] >> 30)).wrapping_mul(1664525)))
            .wrapping_add(seed);
            i += 1;
            if i >= 624 {
                out.state[0] = out.state[623];
                i = 1;
            }
        }
        for _ in 0..623 {
            out.state[i] = (out.state[i]
                ^ ((out.state[i - 1] ^ (out.state[i - 1] >> 30)).wrapping_mul(1566083941)))
            .wrapping_sub(i as u32);
            i += 1;
            if i >= 624 {
                out.state[0] = out.state[623];
                i = 1;
            }
        }
        out.state[0] = 0x80000000;
        out
    }
    fn next(&mut self) -> u32 {
        if self.index >= 624 {
            for i in 0..624 {
                let y = (self.state[i] & 0x80000000) | (self.state[(i + 1) % 624] & 0x7fffffff);
                self.state[i] = self.state[(i + 397) % 624]
                    ^ (y >> 1)
                    ^ if y & 1 != 0 { 0x9908b0df } else { 0 };
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c5680;
        y ^= (y << 15) & 0xefc60000;
        y ^ (y >> 18)
    }
    fn below(&mut self, n: usize) -> usize {
        let bits = usize::BITS - n.leading_zeros();
        loop {
            let value = if bits <= 32 {
                (self.next() >> (32 - bits)) as usize
            } else {
                (self.next() as usize) | (((self.next() >> (64 - bits)) as usize) << 32)
            };
            if value < n {
                return value;
            }
        }
    }
    fn shuffle<T>(&mut self, values: &mut [T]) {
        for i in (1..values.len()).rev() {
            let j = self.below(i + 1);
            values.swap(i, j);
        }
    }
    pub(crate) fn sample(&mut self, n: usize, k: usize) -> Vec<usize> {
        let mut table_size = 21;
        if k > 5 {
            let mut size = 1;
            while size < k * 3 {
                size *= 4;
            }
            table_size += size;
        }
        let mut result = Vec::with_capacity(k);
        if n <= table_size {
            let mut pool: Vec<_> = (0..n).collect();
            for i in 0..k {
                let j = self.below(n - i);
                result.push(pool[j]);
                pool[j] = pool[n - i - 1];
            }
        } else {
            let mut selected = HashSet::new();
            while result.len() < k {
                let j = self.below(n);
                if selected.insert(j) {
                    result.push(j);
                }
            }
        }
        result
    }
}
#[derive(Clone)]
struct WeightedGraph {
    neighbors: Vec<Vec<(usize, f64)>>,
    originals: Vec<BTreeSet<usize>>,
}
impl WeightedGraph {
    fn new(n: usize) -> Self {
        Self {
            neighbors: vec![vec![]; n],
            originals: (0..n).map(|i| BTreeSet::from([i])).collect(),
        }
    }
    fn add(&mut self, u: usize, v: usize, w: f64, accumulate: bool) {
        fn insert(row: &mut Vec<(usize, f64)>, v: usize, w: f64, accumulate: bool) {
            if let Some((_, old)) = row.iter_mut().find(|(n, _)| *n == v) {
                if accumulate {
                    *old += w;
                } else {
                    *old = w;
                }
            } else {
                row.push((v, w));
            }
        }
        insert(&mut self.neighbors[u], v, w, accumulate);
        if u != v {
            insert(&mut self.neighbors[v], u, w, accumulate);
        }
    }
    fn edges(&self) -> Vec<(usize, usize, f64)> {
        self.neighbors
            .iter()
            .enumerate()
            .flat_map(|(u, row)| {
                row.iter()
                    .filter(move |(v, _)| *v >= u)
                    .map(move |(v, w)| (u, *v, *w))
            })
            .collect()
    }
    fn degrees(&self) -> Vec<f64> {
        self.neighbors
            .iter()
            .enumerate()
            .map(|(u, row)| {
                row.iter().map(|(_, w)| *w).sum::<f64>()
                    + row.iter().find(|(v, _)| *v == u).map_or(0.0, |(_, w)| *w)
            })
            .collect()
    }
    fn modularity(&self, partition: &[BTreeSet<usize>], resolution: f64) -> Result<f64> {
        let degrees = self.degrees();
        let total = degrees.iter().sum::<f64>();
        if total == 0.0 {
            return Err("ZeroDivisionError: division by zero".into());
        }
        let m = total / 2.0;
        let norm = 1.0 / (total * total);
        let mut score = 0.0;
        for members in partition {
            let internal = self
                .edges()
                .into_iter()
                .filter(|(u, v, _)| members.contains(u) && members.contains(v))
                .map(|(_, _, w)| w)
                .sum::<f64>();
            let degree = members.iter().map(|i| degrees[*i]).sum::<f64>();
            score += internal / m - resolution * degree * degree * norm;
        }
        Ok(score)
    }
    fn aggregate(&self, partition: &[BTreeSet<usize>]) -> Self {
        let mut next = Self::new(partition.len());
        let mut mapping = vec![0; self.neighbors.len()];
        for (cid, members) in partition.iter().enumerate() {
            next.originals[cid].clear();
            for &node in members {
                mapping[node] = cid;
                next.originals[cid].extend(&self.originals[node]);
            }
        }
        for (u, v, w) in self.edges() {
            next.add(mapping[u], mapping[v], w, true);
        }
        next
    }
}
fn one_level(
    graph: &WeightedGraph,
    mut partition: Vec<BTreeSet<usize>>,
    m: f64,
    resolution: f64,
    rng: &mut PythonRandom,
) -> (Vec<BTreeSet<usize>>, Vec<BTreeSet<usize>>, bool) {
    let n = graph.neighbors.len();
    let mut node2com: Vec<_> = (0..n).collect();
    let mut inner: Vec<_> = (0..n).map(|i| BTreeSet::from([i])).collect();
    let degrees = graph.degrees();
    let mut total = degrees.clone();
    let gamma = resolution / 2.0;
    let mut order: Vec<_> = (0..n).collect();
    rng.shuffle(&mut order);
    let mut improved = false;
    loop {
        let mut moves = 0;
        for &u in &order {
            let current = node2com[u];
            let mut weights: Vec<(usize, f64)> = Vec::new();
            for &(v, w) in &graph.neighbors[u] {
                if v == u {
                    continue;
                }
                let cid = node2com[v];
                if let Some((_, sum)) = weights.iter_mut().find(|(c, _)| *c == cid) {
                    *sum += w;
                } else {
                    weights.push((cid, w));
                }
            }
            let degree = degrees[u];
            total[current] -= degree;
            let own = if let Some((_, w)) = weights.iter().find(|(c, _)| *c == current) {
                *w
            } else {
                weights.push((current, 0.0));
                0.0
            };
            let mut best_gain = own * m - gamma * (total[current] * degree);
            let mut best = current;
            for &(cid, w) in &weights {
                let gain = w * m - gamma * (total[cid] * degree);
                if gain > best_gain {
                    best_gain = gain;
                    best = cid;
                }
            }
            total[best] += degree;
            if best != current {
                for node in &graph.originals[u] {
                    partition[current].remove(node);
                    partition[best].insert(*node);
                }
                inner[current].remove(&u);
                inner[best].insert(u);
                improved = true;
                moves += 1;
                node2com[u] = best;
            }
        }
        if moves == 0 {
            break;
        }
    }
    (
        partition.into_iter().filter(|p| !p.is_empty()).collect(),
        inner.into_iter().filter(|p| !p.is_empty()).collect(),
        improved,
    )
}
fn louvain(graph: &Graph, resolution: f64) -> Result<Partition> {
    let n = graph.nodes.len();
    let mut network = WeightedGraph::new(n);
    for (u, v, e) in graph.iter_edges() {
        network.add(u, v, weight(&e.attributes, false)?, false);
    }
    let mut partition: Vec<_> = (0..n).map(|i| BTreeSet::from([i])).collect();
    if graph.edge_count() > 0 {
        let mut modularity = network.modularity(&partition, resolution)?;
        let m = network.degrees().iter().sum::<f64>() / 2.0;
        let mut rng = PythonRandom::new(42);
        let (p, mut inner, _) = one_level(&network, partition, m, resolution, &mut rng);
        partition = p;
        for level in 0..10 {
            if level == 9 {
                break;
            }
            let new_modularity = network.modularity(&inner, resolution)?;
            if new_modularity - modularity <= 1e-4 {
                break;
            }
            modularity = new_modularity;
            network = network.aggregate(&inner);
            let (p, i, improved) = one_level(&network, partition.clone(), m, resolution, &mut rng);
            if !improved {
                break;
            }
            partition = p;
            inner = i;
        }
    }
    Ok(partition
        .iter()
        .enumerate()
        .flat_map(|(c, members)| {
            members
                .iter()
                .map(move |i| (graph.nodes[*i].id.clone(), c as i64))
        })
        .collect())
}
pub fn partition(graph: &Graph, resolution: f64, backend: &str) -> Result<Partition> {
    let stable = stable_graph(graph)?;
    match backend {
        "auto" | "leiden" => {
            if let Some(result) = native_leiden(&stable, resolution)? {
                return Ok(result);
            }
        }
        "louvain" => {}
        _ => return Err("Expected auto, leiden, or louvain community backend".into()),
    }
    louvain(&stable, resolution)
}
/// The subgraph induced by `nodes`; with `weights_only`, only its topology
/// and edge weights (see `cluster`).
fn induced_as(graph: &Graph, nodes: &[Value], weights_only: bool) -> Result<Graph> {
    let selected: Vec<_> = nodes.iter().filter_map(|id| graph.node_index(id)).collect();
    graph.induced_subgraph_with(&selected, weights_only.then_some("weight"))
}
/// Whether every node id has its own `python_compat::string` text, so the
/// endpoint text of a simple graph's rows identifies the row.
fn ids_have_distinct_text(graph: &Graph) -> bool {
    let mut seen = HashSet::with_capacity(graph.nodes.len());
    graph
        .nodes
        .iter()
        .all(|node| seen.insert(python_compat::string(&node.id)))
}
fn degree(graph: &Graph, node: usize) -> usize {
    if graph.directed {
        graph.out_edges(node).len() + graph.in_edges(node).len()
    } else {
        graph.out_edges(node).len()
            + graph.number_of_edges(&graph.nodes[node].id, &graph.nodes[node].id)
    }
}
fn grouped(partition: Partition) -> Communities {
    let mut groups: Communities = Vec::new();
    for (node, cid) in partition {
        if let Some((_, members)) = groups.iter_mut().find(|(c, _)| *c == cid) {
            members.push(node);
        } else {
            groups.push((cid, vec![node]));
        }
    }
    groups
}
/// Python's `_split_community(G, nodes)` with the default `resolution=1.0`.
pub fn split_community(graph: &Graph, nodes: &[Value], backend: &str) -> Result<Vec<Vec<Value>>> {
    split_community_at(graph, nodes, 1.0, backend)
}
/// Second Leiden pass over a community subgraph at the caller's resolution
/// (upstream PR #2388: the follow-up splits honour `--resolution`).
pub fn split_community_at(
    graph: &Graph,
    nodes: &[Value],
    resolution: f64,
    backend: &str,
) -> Result<Vec<Vec<Value>>> {
    split_at(graph, nodes, resolution, backend, false)
}
fn split_at(
    graph: &Graph,
    nodes: &[Value],
    resolution: f64,
    backend: &str,
    weights_only: bool,
) -> Result<Vec<Vec<Value>>> {
    let sub = induced_as(graph, nodes, weights_only)?;
    if sub.edge_count() == 0 {
        return Ok(sorted_ids(nodes)?.into_iter().map(|n| vec![n]).collect());
    }
    if let Ok(partition) = partition(&sub, resolution, backend) {
        let groups = grouped(partition);
        if groups.len() > 1 {
            return groups.into_iter().map(|(_, n)| sorted_ids(&n)).collect();
        }
    }
    Ok(vec![sorted_ids(nodes)?])
}
pub fn cohesion_score(graph: &Graph, nodes: &[Value]) -> f64 {
    if nodes.len() <= 1 {
        return 1.0;
    }
    let mut members: Vec<_> = nodes.iter().filter_map(|id| graph.node_index(id)).collect();
    members.sort_unstable();
    members.dedup();
    let actual = graph.edges_within(&members, |v| members.binary_search(&v).is_ok());
    actual as f64 / (nodes.len() as f64 * (nodes.len() - 1) as f64 / 2.0)
}
/// The weight multiplier `cluster` applies to `AMBIGUOUS` edges (0.9.81, #4199).
pub const DEFAULT_AMBIGUOUS_SCALE: f64 = 0.5;
/// Python `float()` of a string: surrounding whitespace, `_` only between
/// digits, and the `inf`/`infinity`/`nan` spellings in any case.
fn python_float(text: &str) -> Option<f64> {
    let text = text.trim_matches(crate::dedup_strings::whitespace);
    let bytes = text.as_bytes();
    let misplaced_underscore = bytes.iter().enumerate().any(|(i, &b)| {
        b == b'_'
            && (i == 0
                || i + 1 == bytes.len()
                || !bytes[i - 1].is_ascii_digit()
                || !bytes[i + 1].is_ascii_digit())
    });
    if text.is_empty() || misplaced_underscore {
        return None;
    }
    text.replace('_', "").parse::<f64>().ok()
}
/// `float(d.get("weight", 1.0))` for an edge being rescaled.
fn float_weight(value: Option<&Value>) -> Result<f64> {
    match value {
        None => Ok(1.0),
        Some(Value::Number(n)) => n.as_f64().ok_or_else(|| "OverflowError: weight".into()),
        Some(Value::Bool(b)) => Ok(f64::from(u8::from(*b))),
        Some(Value::String(s)) => python_float(s).ok_or_else(|| {
            format!(
                "ValueError: could not convert string to float: {}",
                python_compat::repr_string(s)
            )
            .into()
        }),
        Some(other) => Err(format!(
            "TypeError: float() argument must be a string or a real number, not '{}'",
            python_compat::type_name(other)
        )
        .into()),
    }
}
/// The graph partitioning sees (#4199): a copy whose `AMBIGUOUS` edges have
/// their `weight` multiplied by `scale` (`GRAPHICLE_AMBIGUOUS_SCALE`, when it
/// parses as a float, overrides the argument), or `None` when nothing changes:
/// a scale of exactly 1.0 or no `AMBIGUOUS` edge. The caller's graph is never
/// modified, so path and explain output keep the raw weight.
fn ambiguous_scaled(graph: &Graph, scale: f64) -> Result<Option<Graph>> {
    let scale = std::env::var("GRAPHICLE_AMBIGUOUS_SCALE")
        .ok()
        .and_then(|raw| python_float(&raw))
        .unwrap_or(scale);
    let ambiguous = |attributes: &crate::record::Attributes| {
        attributes.get("confidence").and_then(Value::as_str) == Some("AMBIGUOUS")
    };
    if scale == 1.0
        || !graph
            .iter_edges()
            .any(|(_, _, edge)| ambiguous(&edge.attributes))
    {
        return Ok(None);
    }
    // Upstream scales the undirected graph: a reciprocal pair of a directed
    // graph is first merged into one record (the later arc's fields win), and
    // whether that record is AMBIGUOUS decides.
    let mut copy = if graph.directed {
        graph.to_undirected()?
    } else {
        graph.clone()
    };
    if !copy
        .iter_edges()
        .any(|(_, _, edge)| ambiguous(&edge.attributes))
    {
        return Ok(None);
    }
    for edge in copy.edges_mut() {
        if ambiguous(&edge.attributes) {
            let base = float_weight(edge.attributes.get("weight"))?;
            edge.attributes.insert("weight".into(), json!(base * scale));
        }
    }
    Ok(Some(copy))
}
pub fn cluster(
    graph: &Graph,
    resolution: f64,
    exclude_hubs_percentile: Option<f64>,
    backend: &str,
) -> Result<Communities> {
    cluster_with(
        graph,
        resolution,
        exclude_hubs_percentile,
        backend,
        DEFAULT_AMBIGUOUS_SCALE,
    )
}
/// [`cluster`] with the `ambiguous_scale` argument explicit (0.9.81): the
/// multiplier for `AMBIGUOUS` edge weights before partitioning; 1.0 disables
/// it and 0.0 leaves those edges out of the modularity entirely.
pub fn cluster_with(
    graph: &Graph,
    resolution: f64,
    exclude_hubs_percentile: Option<f64>,
    backend: &str,
    ambiguous_scale: f64,
) -> Result<Communities> {
    if graph.nodes.is_empty() {
        return Ok(vec![]);
    }
    let scaled = ambiguous_scaled(graph, ambiguous_scale)?;
    let graph = scaled.as_ref().unwrap_or(graph);
    // Clustering reads ids, edges and edge weights only. `stable_graph` orders
    // rows by endpoint text and then by the full edge record, so the other
    // edge attributes can only matter when two rows share endpoint text:
    // parallel edges, or distinct ids with equal text. Otherwise the original
    // undirected graph is read in place and every copy (subgraphs, splits)
    // keeps only weights; the general case copies the topology as before.
    let profiled = crate::profile::span("cluster.prepare");
    let weights_only = !graph.multigraph && ids_have_distinct_text(graph);
    let owned;
    let graph: &Graph = if weights_only && !graph.directed {
        graph
    } else {
        let topology = if weights_only {
            graph.topology_with_edge_attribute("weight")
        } else {
            graph.without_node_attributes()
        };
        owned = if topology.directed {
            topology.to_undirected()?
        } else {
            topology
        };
        &owned
    };
    if graph.edge_count() == 0 {
        return Ok(
            sorted_ids(&graph.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>())?
                .into_iter()
                .enumerate()
                .map(|(i, n)| (i as i64, vec![n]))
                .collect(),
        );
    }
    let degrees: Vec<_> = (0..graph.nodes.len()).map(|i| degree(graph, i)).collect();
    let mut hubs = HashSet::new();
    if let Some(percent) = exclude_hubs_percentile {
        if !percent.is_finite() {
            return Err("ValueError: cannot convert nonfinite percentile to integer".into());
        }
        let mut ordered = degrees.clone();
        ordered.sort();
        let index = ((ordered.len() as f64 * percent / 100.0).trunc() - 1.0).max(0.0) as usize;
        let threshold = *ordered
            .get(index)
            .ok_or("IndexError: list index out of range")?;
        hubs.extend(
            degrees
                .iter()
                .enumerate()
                .filter(|(_, d)| **d > threshold)
                .map(|(i, _)| i),
        );
    }
    // A node whose only neighbours (a self-loop is not one) are excluded hubs
    // is isolated in the partitioned subgraph purely because of the exclusion:
    // hold it out and place it with its hub(s) below.
    let stranded_by_hubs = |i: usize| {
        let mut neighbors = graph
            .out_edges(i)
            .into_iter()
            .map(|(_, neighbor, _)| neighbor)
            .filter(|&neighbor| neighbor != i)
            .peekable();
        neighbors.peek().is_some() && neighbors.all(|neighbor| hubs.contains(&neighbor))
    };
    let mut isolates = Vec::new();
    let mut connected = Vec::new();
    let mut stranded = Vec::new();
    for (i, node) in graph.nodes.iter().enumerate() {
        if hubs.contains(&i) {
            continue;
        }
        if degrees[i] == 0 {
            isolates.push(node.id.clone());
        } else if !hubs.is_empty() && stranded_by_hubs(i) {
            stranded.push(i);
        } else {
            connected.push(node.id.clone());
        }
    }
    drop(profiled);
    // The subgraph only feeds the partition; free it before the splits.
    let mut raw = {
        let profiled = crate::profile::span("cluster.induced");
        let sub = induced_as(graph, &connected, weights_only)?;
        drop(profiled);
        let _profiled = crate::profile::span("cluster.partition");
        if sub.nodes.is_empty() {
            vec![]
        } else {
            grouped(partition(&sub, resolution, backend)?)
        }
    };
    let profiled = crate::profile::span("cluster.hubs_isolates");
    let mut next = raw.iter().map(|(c, _)| *c).max().unwrap_or(-1) + 1;
    for node in isolates {
        raw.push((next, vec![node]));
        next += 1;
    }
    let hub_ids = sorted_ids(
        &hubs
            .iter()
            .map(|i| graph.nodes[*i].id.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut assignment = vec![None; graph.nodes.len()];
    for (c, nodes) in &raw {
        for node in nodes {
            assignment[graph.node_index(node).unwrap()] = Some(*c);
        }
    }
    for hub in hub_ids {
        let index = graph.node_index(&hub).unwrap();
        let mut votes = BTreeMap::<i64, usize>::new();
        let mut seen = HashSet::new();
        for (_, neighbor, _) in graph.out_edges(index) {
            if seen.insert(neighbor)
                && let Some(cid) = assignment[neighbor]
            {
                *votes.entry(cid).or_default() += 1;
            }
        }
        if let Some((&best, _)) = votes
            .iter()
            .min_by_key(|(cid, votes)| (std::cmp::Reverse(**votes), **cid))
        {
            raw.iter_mut()
                .find(|(c, _)| *c == best)
                .unwrap()
                .1
                .push(hub);
            assignment[index] = Some(best);
        } else {
            raw.push((next, vec![hub]));
            assignment[index] = Some(next);
            next += 1;
        }
    }
    // Every neighbour of a stranded node is a hub, and every hub is placed by
    // now, so the same majority vote always has a winner.
    stranded.sort_by_cached_key(|&i| crate::python_compat::string(&graph.nodes[i].id));
    for index in stranded {
        let mut votes = BTreeMap::<i64, usize>::new();
        let mut seen = HashSet::new();
        for (_, neighbor, _) in graph.out_edges(index) {
            if neighbor != index
                && seen.insert(neighbor)
                && let Some(cid) = assignment[neighbor]
            {
                *votes.entry(cid).or_default() += 1;
            }
        }
        let node = graph.nodes[index].id.clone();
        if let Some((&best, _)) = votes
            .iter()
            .min_by_key(|(cid, votes)| (std::cmp::Reverse(**votes), **cid))
        {
            raw.iter_mut()
                .find(|(c, _)| *c == best)
                .unwrap()
                .1
                .push(node);
            assignment[index] = Some(best);
        } else {
            raw.push((next, vec![node]));
            assignment[index] = Some(next);
            next += 1;
        }
    }
    drop(profiled);
    let profiled = crate::profile::span("cluster.splits");
    let max_size = 10.max(graph.nodes.len() / 4);
    let workers = crate::par::workers(None);
    // Each split is a Leiden pass over one community's own subgraph with its
    // own seeded generator, so the splits run in parallel and are collected
    // in community order, as the sequential loops did.
    let splits = crate::par::map(&raw, workers, |(_, nodes)| {
        (nodes.len() > max_size).then(|| split_at(graph, nodes, resolution, backend, weights_only))
    });
    let mut first = Vec::new();
    for ((_, nodes), split) in raw.into_iter().zip(splits) {
        match split {
            Some(split) => first.extend(split?),
            None => first.push(nodes),
        }
    }
    drop(profiled);
    let profiled = crate::profile::span("cluster.refine");
    let refined = crate::par::map(&first, workers, |nodes| {
        (nodes.len() >= 50 && cohesion_score(graph, nodes) < 0.05)
            .then(|| split_at(graph, nodes, resolution, backend, weights_only))
    });
    let mut final_groups = Vec::new();
    for (nodes, refined) in first.into_iter().zip(refined) {
        match refined {
            Some(splits) => {
                let splits = splits?;
                if splits.len() > 1 {
                    final_groups.extend(splits);
                } else {
                    final_groups.push(nodes);
                }
            }
            None => final_groups.push(nodes),
        }
    }
    drop(profiled);
    let _profiled = crate::profile::span("cluster.order");
    final_groups.sort_by_key(|nodes| (std::cmp::Reverse(nodes.len()), strings(nodes)));
    final_groups
        .into_iter()
        .enumerate()
        .map(|(i, n)| Ok((i as i64, sorted_ids(&n)?)))
        .collect()
}
pub fn label_communities_by_hub(graph: &Graph, communities: &Communities) -> Value {
    let mut labels = Map::new();
    for (cid, members) in communities {
        let hub = members
            .iter()
            .filter_map(|id| graph.node_index(id))
            .min_by_key(|i| {
                (
                    std::cmp::Reverse(degree(graph, *i)),
                    python_compat::string(&graph.nodes[*i].id),
                )
            });
        let fallback = format!("Community {cid}");
        let name = if let Some(hub) = hub {
            let node = &graph.nodes[hub];
            let label = node
                .attributes
                .get("label")
                .filter(|v| truthy(v))
                .unwrap_or(&node.id);
            let value = python_compat::string(label);
            let value =
                value.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
            value.strip_suffix("()").unwrap_or(value).to_string()
        } else {
            fallback.clone()
        };
        labels.insert(
            cid.to_string(),
            json!(if name.is_empty() { fallback } else { name }),
        );
    }
    Value::Object(labels)
}
pub fn community_member_sigs(communities: &Communities) -> Value {
    Value::Object(
        communities
            .iter()
            .map(|(c, nodes)| {
                let mut hash = Sha256::new();
                for node in strings(nodes) {
                    hash.update(node.as_bytes());
                    hash.update([0]);
                }
                (
                    c.to_string(),
                    json!(&format!("{:x}", hash.finalize())[..16]),
                )
            })
            .collect(),
    )
}
pub fn score_all(graph: &Graph, communities: &Communities) -> Value {
    Value::Object(
        communities
            .iter()
            .map(|(c, n)| (c.to_string(), json!(cohesion_score(graph, n))))
            .collect(),
    )
}
pub fn remap_communities_to_previous(
    communities: &Communities,
    previous: &Partition,
) -> Result<Communities> {
    let mut registry = Graph::new(false, false);
    let mut new_sets = BTreeMap::new();
    let mut old_sets = BTreeMap::<i64, BTreeSet<usize>>::new();
    for (c, nodes) in communities {
        let mut set = BTreeSet::new();
        for n in nodes {
            set.insert(registry.add_node(n.clone(), Map::new())?);
        }
        new_sets.insert(*c, set);
    }
    for (n, c) in previous {
        old_sets
            .entry(*c)
            .or_default()
            .insert(registry.add_node(n.clone(), Map::new())?);
    }
    let mut overlaps = Vec::new();
    for (old, old_nodes) in old_sets {
        for (new, new_nodes) in &new_sets {
            let count = old_nodes.intersection(new_nodes).count();
            if count > 0 {
                overlaps.push((std::cmp::Reverse(count), old, *new));
            }
        }
    }
    overlaps.sort();
    let mut mapping = BTreeMap::new();
    let mut used = BTreeSet::new();
    for (_, old, new) in overlaps {
        if !used.contains(&old) && !mapping.contains_key(&new) {
            mapping.insert(new, old);
            used.insert(old);
        }
    }
    let mut unmatched: Vec<_> = communities
        .iter()
        .filter(|(c, _)| !mapping.contains_key(c))
        .map(|(c, n)| Ok((*c, sorted_ids(n)?)))
        .collect::<Result<_>>()?;
    let mut failed = None;
    unmatched.sort_by(|a, b| {
        b.1.len().cmp(&a.1.len()).then_with(|| {
            cmp_id(&json!(a.1), &json!(b.1)).unwrap_or_else(|e| {
                failed = Some(e);
                Ordering::Equal
            })
        })
    });
    if let Some(error) = failed {
        return Err(error);
    }
    let mut next = 0;
    for (new, _) in unmatched {
        while used.contains(&next) {
            next += 1;
        }
        mapping.insert(new, next);
        used.insert(next);
        next += 1;
    }
    let mut remapped = communities
        .iter()
        .map(|(c, n)| Ok((mapping[c], sorted_ids(n)?)))
        .collect::<Result<Communities>>()?;
    remapped.sort_by_key(|(c, _)| *c);
    Ok(remapped)
}
pub fn communities_from_json(value: &Value) -> Result<Communities> {
    value
        .as_object()
        .ok_or("Expected communities")?
        .iter()
        .map(|(c, n)| {
            Ok((
                c.parse::<i64>()
                    .map_err(|_| "Expected integer community ID")?,
                n.as_array().ok_or("Expected members")?.clone(),
            ))
        })
        .collect()
}
pub fn communities_to_json(communities: &Communities) -> Value {
    Value::Object(
        communities
            .iter()
            .map(|(c, n)| (c.to_string(), json!(n)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{cluster, split_community_at};
    use crate::graph::Graph;
    use serde_json::{Map, json};

    /// networkx `barbell_graph(15, 4)`: two 15-cliques joined by a 4-node path.
    fn barbell() -> Graph {
        let mut graph = Graph::new(false, false);
        let mut edge = |u: usize, v: usize| {
            graph
                .add_edge(
                    json!(format!("n{u:03}")),
                    json!(format!("n{v:03}")),
                    None,
                    Map::new(),
                )
                .unwrap();
        };
        for base in [0, 19] {
            for u in base..base + 15 {
                for v in u + 1..base + 15 {
                    edge(u, v);
                }
            }
        }
        for u in 14..19 {
            edge(u, u + 1);
        }
        graph
    }

    #[test]
    fn oversized_community_splits_honour_the_callers_resolution() {
        let graph = barbell();
        let nodes: Vec<_> = graph.nodes.iter().map(|n| n.id.clone()).collect();
        assert_eq!(
            split_community_at(&graph, &nodes, 1.0, "auto")
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            split_community_at(&graph, &nodes, 0.1, "auto")
                .unwrap()
                .len(),
            2
        );
        // max_size is 10, so the whole graph is re-split; at 0.1 the coarser
        // choice survives instead of being undone by a hard-coded 1.0.
        assert_eq!(cluster(&graph, 0.1, None, "auto").unwrap().len(), 2);
        assert_eq!(cluster(&graph, 1.0, None, "auto").unwrap().len(), 3);
    }
}
