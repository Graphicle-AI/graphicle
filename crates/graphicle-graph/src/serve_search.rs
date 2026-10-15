// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Indexed, multilingual graph search and context-aware seed selection.
use crate::{graph::Graph, python_compat};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{LazyLock, Mutex},
};
use unicode_normalization::{UnicodeNormalization, char::canonical_combining_class};
pub static CONSTANTS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("assets/serve/constants.json")).unwrap());
pub fn field<'a>(g: &'a Graph, i: usize, k: &str) -> &'a Value {
    g.nodes[i].attributes.get(k).unwrap_or(&Value::Null)
}
pub fn string(v: &Value) -> String {
    python_compat::string(v)
}
pub fn text(v: &Value) -> String {
    if v.is_null() {
        String::new()
    } else {
        string(v)
    }
}
pub fn id(g: &Graph, i: usize) -> String {
    text(&g.nodes[i].id)
}
pub fn label(g: &Graph, i: usize) -> String {
    g.nodes[i]
        .attributes
        .get("label")
        .map(string)
        .unwrap_or_else(|| id(g, i))
}
/// `sanitize_label(G.nodes[n].get("label", n))`: a node's label as the tool
/// text prints it. A null label prints as nothing (`sanitize_label(None)`),
/// not as `None`.
pub fn label_text(g: &Graph, i: usize) -> String {
    match g.nodes[i].attributes.get("label") {
        Some(label) => crate::security::sanitize_label(label),
        None => crate::security::sanitize_label(&Value::String(id(g, i))),
    }
}
pub fn fold(s: &str) -> String {
    s.nfkd()
        .filter(|c| canonical_combining_class(*c) == 0)
        .collect()
}
pub fn tokens(s: &str) -> Vec<String> {
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"[^\W_]+").unwrap());
    RE.find_iter(&fold(s).to_lowercase())
        .map(|m| m.as_str().to_string())
        .collect()
}
pub fn norm_label(g: &Graph, i: usize) -> String {
    let norm = text(field(g, i, "norm_label"));
    if norm.is_empty() {
        fold(&text(field(g, i, "label"))).to_lowercase()
    } else {
        norm
    }
}
pub fn degrees(g: &Graph) -> Vec<usize> {
    let mut d = vec![0; g.nodes.len()];
    for (u, v, _) in g.iter_edges() {
        d[u] += 1;
        d[v] += 1;
    }
    d
}
pub fn neighbors(g: &Graph, i: usize, incoming: bool) -> Vec<usize> {
    g.neighbor_indices(i, incoming).collect()
}
pub fn flatten(v: &Value, parts: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                parts.push(k.clone());
                flatten(v, parts);
            }
        }
        Value::Array(a) => {
            for v in a {
                flatten(v, parts);
            }
        }
        Value::Null => {}
        _ => parts.push(string(v)),
    }
}
pub fn attributes_text(data: &impl crate::record::Properties) -> (String, String) {
    let Some(v) = data.get("attributes").filter(|v| v.is_object()) else {
        return (String::new(), String::new());
    };
    let mut parts = vec![];
    flatten(v, &mut parts);
    let norm = fold(&parts.join(" ")).to_lowercase();
    let tok = tokens(&norm).join(" ");
    (norm, tok)
}
pub fn rationale_text(data: &impl crate::record::Properties) -> String {
    let Some(v) = data.get("rationale").filter(|v| crate::build::truthy(v)) else {
        return String::new();
    };
    let s = if let Some(a) = v.as_array() {
        a.iter()
            .filter(|v| crate::build::truthy(v))
            .map(string)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        string(v)
    };
    fold(&s).to_lowercase()
}
pub fn search_text(g: &Graph, i: usize) -> String {
    let nid = id(g, i);
    let lower = nid.to_lowercase();
    let source = text(field(g, i, "source_file"));
    let mut fields = vec![
        norm_label(g, i),
        tokens(&text(field(g, i, "label"))).join(" "),
        lower.clone(),
        source.to_lowercase(),
        tokens(&source).join(" "),
    ];
    if !lower.is_ascii() {
        let norm = fold(&nid).to_lowercase();
        if norm != lower {
            fields.push(norm);
        }
    }
    let rationale = rationale_text(&g.nodes[i].attributes);
    if !rationale.is_empty() {
        fields.push(rationale);
    }
    let (a, t) = attributes_text(&g.nodes[i].attributes);
    if !a.is_empty() {
        fields.extend([a, t]);
    }
    fields.join("\0")
}
pub fn trigrams(s: &str) -> BTreeSet<String> {
    let cs = s.chars().collect::<Vec<_>>();
    if cs.len() < 3 {
        if s.is_empty() {
            BTreeSet::new()
        } else {
            BTreeSet::from([s.to_string()])
        }
    } else {
        cs.windows(3).map(|w| w.iter().collect()).collect()
    }
}
/// Three chars packed into 63 bits, 21 per Unicode scalar value, so trigram
/// postings avoid one heap string per gram. Order is irrelevant: postings are
/// only probed by key, and `postings()` re-sorts by string for inspection.
fn pack3(w: &[char]) -> u64 {
    (u64::from(u32::from(w[0])) << 42)
        | (u64::from(u32::from(w[1])) << 21)
        | u64::from(u32::from(w[2]))
}
fn unpack3(key: u64) -> String {
    [key >> 42, (key >> 21) & 0x1f_ffff, key & 0x1f_ffff]
        .into_iter()
        .filter_map(|v| char::from_u32(v as u32))
        .collect()
}
/// A node's distinct trigram keys, or the whole text when it is shorter than
/// three chars (`trigrams` keeps such a text as its only gram).
fn node_grams(text: &str) -> (Vec<u64>, Option<String>) {
    let cs = text.chars().collect::<Vec<_>>();
    if cs.len() < 3 {
        return (vec![], (!text.is_empty()).then(|| text.to_string()));
    }
    let mut keys = cs.windows(3).map(pack3).collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();
    (keys, None)
}
type Postings = hashbrown::HashMap<u64, Vec<usize>, crate::graph::FastState>;
/// Per-node strings that scoring derives from the graph, computed once per
/// index instead of once per node per query.
pub struct NodeText {
    pub norm: String,
    norm_lower: String,
    pub label: String,
    label_tokens: String,
    src_lower: String,
    rationale: String,
    attr_norm: String,
    attr_tokens: String,
    pub id: String,
    id_lower: String,
    /// Label length in chars, or the id's when the label is empty.
    len: usize,
}
/// The node's scoring strings and its `search_text`, sharing each piece.
fn node_text(g: &Graph, i: usize) -> (NodeText, String) {
    let nid = id(g, i);
    let id_lower = nid.to_lowercase();
    let source = text(field(g, i, "source_file"));
    let label = text(field(g, i, "label"));
    let norm = norm_label(g, i);
    let label_tokens = tokens(&label).join(" ");
    let rationale = rationale_text(&g.nodes[i].attributes);
    let (attr_norm, attr_tokens) = attributes_text(&g.nodes[i].attributes);
    let src_lower = source.to_lowercase();
    // Exactly `search_text(g, i)`, assembled from the shared pieces.
    let mut fields = vec![
        norm.clone(),
        label_tokens.clone(),
        id_lower.clone(),
        src_lower.clone(),
        tokens(&source).join(" "),
    ];
    if !id_lower.is_ascii() {
        let folded = fold(&nid).to_lowercase();
        if folded != id_lower {
            fields.push(folded);
        }
    }
    if !rationale.is_empty() {
        fields.push(rationale.clone());
    }
    if !attr_norm.is_empty() {
        fields.extend([attr_norm.clone(), attr_tokens.clone()]);
    }
    let len = if label.is_empty() {
        nid.chars().count()
    } else {
        label.chars().count()
    };
    (
        NodeText {
            norm_lower: norm.to_lowercase(),
            norm,
            label,
            label_tokens,
            src_lower,
            rationale,
            attr_norm,
            attr_tokens,
            id: nid,
            id_lower,
            len,
        },
        fields.join("\0"),
    )
}
/// The successor and predecessor lists `shortest_path` searches.
pub type PathAdjacency = (Vec<Vec<usize>>, Vec<Vec<usize>>);
pub struct SearchIndex {
    grams: Postings,
    short: BTreeMap<String, Vec<usize>>,
    texts: Vec<NodeText>,
    traversal: Mutex<Option<std::sync::Arc<Graph>>>,
    /// `shortest_path` adjacency, directed and undirected, built on first use.
    paths: Mutex<[Option<std::sync::Arc<PathAdjacency>>; 2]>,
    pub size: usize,
    idf: Mutex<BTreeMap<String, f64>>,
}
impl SearchIndex {
    pub fn new(g: &Graph) -> Self {
        let n = g.nodes.len();
        // Search texts are independent per node; build them on all cores and
        // insert in node order so every posting list stays sorted ascending.
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(n.div_ceil(512).max(1));
        let chunk = n.div_ceil(workers.max(1)).max(1);
        let per_node = std::thread::scope(|scope| {
            (0..n)
                .step_by(chunk)
                .map(|start| {
                    scope.spawn(move || {
                        (start..(start + chunk).min(n))
                            .map(|i| {
                                let (texts, search) = node_text(g, i);
                                (texts, node_grams(&search))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|handle| handle.join().expect("search index worker panicked"))
                .collect::<Vec<_>>()
        });
        let mut grams = Postings::default();
        let mut short = BTreeMap::<String, Vec<usize>>::new();
        let mut texts = Vec::with_capacity(n);
        for (i, (node, (keys, whole))) in per_node.into_iter().enumerate() {
            for key in keys {
                grams.entry(key).or_default().push(i);
            }
            if let Some(whole) = whole {
                short.entry(whole).or_default().push(i);
            }
            texts.push(node);
        }
        Self {
            grams,
            short,
            texts,
            traversal: Mutex::new(None),
            paths: Mutex::new([None, None]),
            size: n,
            idf: Mutex::new(BTreeMap::new()),
        }
    }
    /// Cached scoring strings for node `i` of the indexed graph.
    pub fn text(&self, i: usize) -> &NodeText {
        &self.texts[i]
    }
    /// The index of a graph derived from the indexed one by removing nodes
    /// (`remap[old]` is the new position of each survivor, in the same order)
    /// and appending nodes from `first_new` on. Equal to `SearchIndex::new` on
    /// that graph when surviving nodes kept their attributes.
    pub fn patched(&self, g: &Graph, remap: &[Option<usize>], first_new: usize) -> Self {
        let mut grams = Postings::with_capacity_and_hasher(self.grams.len(), Default::default());
        for (key, list) in &self.grams {
            let moved = list.iter().filter_map(|i| remap[*i]).collect::<Vec<_>>();
            if !moved.is_empty() {
                grams.insert(*key, moved);
            }
        }
        let mut short = BTreeMap::<String, Vec<usize>>::new();
        for (key, list) in &self.short {
            let moved = list.iter().filter_map(|i| remap[*i]).collect::<Vec<_>>();
            if !moved.is_empty() {
                short.insert(key.clone(), moved);
            }
        }
        let mut texts = Vec::with_capacity(g.nodes.len());
        for (old, text) in self.texts.iter().enumerate() {
            if remap[old].is_some() {
                texts.push(NodeText {
                    norm: text.norm.clone(),
                    norm_lower: text.norm_lower.clone(),
                    label: text.label.clone(),
                    label_tokens: text.label_tokens.clone(),
                    src_lower: text.src_lower.clone(),
                    rationale: text.rationale.clone(),
                    attr_norm: text.attr_norm.clone(),
                    attr_tokens: text.attr_tokens.clone(),
                    id: text.id.clone(),
                    id_lower: text.id_lower.clone(),
                    len: text.len,
                });
            }
        }
        for i in first_new..g.nodes.len() {
            let (node, search) = node_text(g, i);
            let (keys, whole) = node_grams(&search);
            for key in keys {
                grams.entry(key).or_default().push(i);
            }
            if let Some(whole) = whole {
                short.entry(whole).or_default().push(i);
            }
            texts.push(node);
        }
        Self {
            grams,
            short,
            texts,
            traversal: Mutex::new(None),
            paths: Mutex::new([None, None]),
            size: g.nodes.len(),
            idf: Mutex::new(BTreeMap::new()),
        }
    }
    /// `traversal_view(g)` of the indexed graph, built on first use.
    pub fn traversal_view(&self, g: &Graph) -> crate::Result<std::sync::Arc<Graph>> {
        let mut slot = self.traversal.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(view) = slot.as_ref() {
            return Ok(view.clone());
        }
        let view = std::sync::Arc::new(crate::serve_text::traversal_view(g)?);
        *slot = Some(view.clone());
        Ok(view)
    }
    /// The adjacency `shortest_path` searches in the indexed graph, built by
    /// `build` on first use per direction mode and shared afterwards (Graphify
    /// 0.9.80, #4192: rebuilding it from every edge was nearly all of a call).
    pub fn path_adjacency(
        &self,
        undirected: bool,
        build: impl FnOnce() -> PathAdjacency,
    ) -> std::sync::Arc<PathAdjacency> {
        let mut slots = self.paths.lock().unwrap_or_else(|e| e.into_inner());
        slots[usize::from(undirected)]
            .get_or_insert_with(|| std::sync::Arc::new(build()))
            .clone()
    }
    /// For an undirected `graph`, `traversal_view` is an exact copy of it:
    /// use the graph itself instead of building that copy.
    pub fn share_traversal(&self, graph: &std::sync::Arc<Graph>) {
        if !graph.is_directed() {
            *self.traversal.lock().unwrap_or_else(|e| e.into_inner()) = Some(graph.clone());
        }
    }
    /// Query snapshot encoding (`crate::snapshot`). The idf cache and the
    /// traversal view are rebuilt on demand, as after `new`.
    pub(crate) fn write_snapshot<'a>(&'a self, w: &mut crate::snapshot::Writer<'a>) {
        w.usize(self.size);
        w.usize(self.texts.len());
        w.end_chunk();
        for part in self.texts.chunks(crate::snapshot::CHUNK) {
            for t in part {
                for s in [
                    &t.norm,
                    &t.norm_lower,
                    &t.label,
                    &t.label_tokens,
                    &t.src_lower,
                    &t.rationale,
                    &t.attr_norm,
                    &t.attr_tokens,
                    &t.id,
                    &t.id_lower,
                ] {
                    w.str(s);
                }
                w.usize(t.len);
            }
            w.end_chunk();
        }
        let mut grams = self.grams.iter().collect::<Vec<_>>();
        grams.sort_unstable_by_key(|(k, _)| **k);
        w.usize(grams.len());
        for (k, list) in grams {
            w.uvar(*k);
            w.indices(list);
        }
        w.end_chunk();
        w.usize(self.short.len());
        for (k, list) in &self.short {
            w.str(k);
            w.indices(list);
        }
    }
    pub(crate) fn read_snapshot(
        d: &mut crate::snapshot::Decoder<'_>,
        nodes: usize,
    ) -> crate::Result<Self> {
        use crate::snapshot::{CHUNK, chunk_count, corrupt};
        let (size, n) = d.chunk(|r| Ok((r.usize()?, r.usize()?)))?;
        if size != nodes || n != nodes {
            return Err(corrupt("index size"));
        }
        let texts = d.chunks(chunk_count(n), |r, i| {
            (0..CHUNK.min(n - i * CHUNK))
                .map(|_| {
                    Ok(NodeText {
                        norm: r.string()?,
                        norm_lower: r.string()?,
                        label: r.string()?,
                        label_tokens: r.string()?,
                        src_lower: r.string()?,
                        rationale: r.string()?,
                        attr_norm: r.string()?,
                        attr_tokens: r.string()?,
                        id: r.string()?,
                        id_lower: r.string()?,
                        len: r.usize()?,
                    })
                })
                .collect::<crate::Result<Vec<_>>>()
        })?;
        let grams = d.chunk(|r| {
            let count = r.len(2)?;
            let mut grams = Postings::with_capacity_and_hasher(count, Default::default());
            for _ in 0..count {
                let key = r.uvar()?;
                let list = r.indices(nodes)?;
                if grams.insert(key, list).is_some() {
                    return Err(corrupt("duplicate gram"));
                }
            }
            Ok(grams)
        })?;
        let short = d.chunk(|r| {
            let count = r.len(2)?;
            let mut short = BTreeMap::new();
            for _ in 0..count {
                let key = r.string()?;
                let list = r.indices(nodes)?;
                short.insert(key, list);
            }
            Ok(short)
        })?;
        Ok(Self {
            grams,
            short,
            texts: texts.into_iter().flatten().collect(),
            traversal: Mutex::new(None),
            paths: Mutex::new([None, None]),
            size,
            idf: Mutex::new(BTreeMap::new()),
        })
    }
    /// Postings keyed by gram text, as the reference implementation stores them.
    pub fn postings(&self) -> BTreeMap<String, Vec<usize>> {
        let mut out = self.short.clone();
        out.extend(self.grams.iter().map(|(k, v)| (unpack3(*k), v.clone())));
        out
    }
    pub fn candidates(&self, needles: &[String], guard: f64) -> Option<Vec<usize>> {
        if self.size == 0 {
            return Some(vec![]);
        }
        let needles = needles.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let threshold = (self.size as f64 * guard).trunc();
        let mut keyed = Vec::with_capacity(needles.len());
        for s in &needles {
            // A needle shorter than three chars yields a short gram: no index use.
            let (keys, whole) = node_grams(s);
            if whole.is_some() || keys.is_empty() {
                return None;
            }
            if keys
                .iter()
                .filter_map(|k| self.grams.get(k).map(Vec::len))
                .min()
                .is_some_and(|n| n as f64 > threshold)
            {
                return None;
            }
            keyed.push(keys);
        }
        let mut candidates = BTreeSet::new();
        for keys in keyed {
            let Some(mut sets) = keys
                .iter()
                .map(|k| self.grams.get(k))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            sets.sort_by_key(|s| s.len());
            let Some(first) = sets.first() else {
                continue;
            };
            let mut hits = first.iter().copied().collect::<BTreeSet<_>>();
            for other in sets.iter().skip(1) {
                hits.retain(|i| other.binary_search(i).is_ok());
            }
            candidates.extend(hits);
        }
        Some(candidates.into_iter().collect())
    }
    pub fn idf(&self, g: &Graph, terms: &[String]) -> BTreeMap<String, f64> {
        let mut cache = self.idf.lock().unwrap_or_else(|e| e.into_inner());
        let n = g.nodes.len().max(1) as f64;
        for term in terms {
            cache.entry(term.clone()).or_insert_with(|| {
                let df = self
                    .texts
                    .iter()
                    .filter(|t| t.norm_lower.contains(term.as_str()))
                    .count();
                (1.0 + n / (1.0 + df as f64)).ln()
            });
        }
        terms.iter().map(|t| (t.clone(), cache[t])).collect()
    }
}
#[derive(Default)]
pub struct QueryScores {
    pub ranked: Vec<(f64, usize)>,
    pub best_seed_by_term: BTreeMap<String, usize>,
    /// The terms of `best_seed_by_term`, strongest per-term match first
    /// (Graphify 0.9.87, #4357; see [`pick_seeds_in_order`]).
    pub term_order: Vec<String>,
}
pub fn score_query(
    g: &Graph,
    index: &SearchIndex,
    terms: &[String],
    per_term: bool,
) -> QueryScores {
    let mut seen = BTreeSet::new();
    let terms = terms
        .iter()
        .flat_map(|s| tokens(s))
        .filter(|s| seen.insert(s.clone()))
        .collect::<Vec<_>>();
    let weights = index.idf(g, &terms);
    let joined = terms.join(" ");
    let joined_w = weights.values().copied().reduce(f64::max).unwrap_or(1.0);
    let mut needles = terms.clone();
    if !joined.is_empty() {
        needles.push(joined.clone());
    }
    let candidates = index
        .candidates(&needles, 0.1)
        .unwrap_or_else(|| (0..g.nodes.len()).collect());
    let ds = degrees(g);
    let mut result = QueryScores::default();
    let mut best = BTreeMap::<String, (f64, usize, usize, String, usize)>::new();
    for i in candidates {
        let node = index.text(i);
        let norm = node.norm.as_str();
        let bare = norm.trim_end_matches(['(', ')']);
        let lt = node.label_tokens.as_str();
        let src = node.src_lower.as_str();
        let rationale = node.rationale.as_str();
        let (an, at) = (node.attr_norm.as_str(), node.attr_tokens.as_str());
        let nid = node.id_lower.as_str();
        let mut score = 0.0;
        if !joined.is_empty() {
            if [norm, bare, lt, nid].contains(&joined.as_str()) {
                score += 10000.0 * joined_w;
            } else if [norm, bare, lt].iter().any(|s| s.starts_with(&joined)) {
                score += 1000.0 * joined_w;
            }
        }
        let (mut matched, mut tiered) = (0, 0.0);
        for t in &terms {
            let w = weights[t];
            let (mut tier, mut substr, mut source, mut rationale_value, mut attr) =
                (0.0, 0.0, 0.0, 0.0, 0.0);
            if t == norm || t == bare {
                tier = 1000.0 * w;
                matched += 1;
            } else if norm.starts_with(t.as_str()) || bare.starts_with(t.as_str()) {
                tier = 100.0 * w;
                matched += 1;
            } else if norm.contains(t.as_str()) {
                substr = w;
                score += substr;
                matched += 1;
            }
            if src.contains(t.as_str()) {
                source = 0.5 * w;
                score += source;
            }
            if !rationale.is_empty() && rationale.contains(t.as_str()) {
                rationale_value = 0.75 * w;
                score += rationale_value;
            }
            if !an.is_empty() && (an.contains(t.as_str()) || at.contains(t.as_str())) {
                attr = 0.75 * w;
                score += attr;
            }
            tiered += tier;
            if per_term {
                let mut singleton = if [norm, bare, lt, nid].contains(&t.as_str()) {
                    10000.0 * w
                } else if [norm, bare, lt].iter().any(|s| s.starts_with(t.as_str())) {
                    1000.0 * w
                } else {
                    0.0
                };
                singleton += tier + substr + source + rationale_value + attr;
                let len = node.len;
                if singleton > 0.0
                    && best.get(t).is_none_or(|old| {
                        singleton > old.0
                            || singleton == old.0
                                && (ds[i] > old.1
                                    || ds[i] == old.1
                                        && (len < old.2 || len == old.2 && node.id < old.3))
                    })
                {
                    best.insert(t.clone(), (singleton, ds[i], len, node.id.clone(), i));
                }
            }
        }
        if tiered != 0.0 {
            score += tiered * (matched as f64 / terms.len() as f64).powi(2);
        }
        if score > 0.0 {
            result.ranked.push((score, i));
        }
    }
    result.ranked.sort_by(|(a, i), (b, j)| {
        b.total_cmp(a)
            .then_with(|| index.text(*i).len.cmp(&index.text(*j).len))
            .then_with(|| index.text(*i).id.cmp(&index.text(*j).id))
    });
    // Strongest per-term winner first: the key is (-singleton score, -degree,
    // label length, node id), and the singleton score already carries the
    // term's IDF weight, so a specific term's exact match leads and a common
    // term's substring hit trails. The term breaks remaining ties.
    let mut order = best.iter().collect::<Vec<_>>();
    order.sort_by(|(ta, a), (tb, b)| {
        b.0.total_cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.cmp(&b.3))
            .then_with(|| ta.cmp(tb))
    });
    result.term_order = order.into_iter().map(|(t, _)| t.clone()).collect();
    result.best_seed_by_term = best.into_iter().map(|(t, k)| (t, k.4)).collect();
    result
}
pub fn pick_endpoint(g: &Graph, ranked: &[(f64, usize)], query: &str) -> Option<usize> {
    let qt = tokens(query).into_iter().collect::<BTreeSet<_>>();
    if qt.is_empty() {
        return ranked.first().map(|(_, i)| *i);
    }
    ranked
        .iter()
        .find(|(_, i)| qt.is_subset(&tokens(&label(g, *i)).into_iter().collect()))
        .or_else(|| ranked.first())
        .map(|(_, i)| *i)
}
/// [`pick_seeds_in_order`] with the per-term seeds in sorted term order (no
/// `term_order`).
pub fn pick_seeds(
    g: Option<&Graph>,
    ranked: &[(f64, usize)],
    max: usize,
    gap: f64,
    best: &BTreeMap<String, usize>,
) -> Vec<usize> {
    pick_seeds_in_order(g, ranked, max, gap, best, &[])
}
/// Seeds from the gap window of `ranked`, then one per term of `best`. With
/// `term_order` (from [`score_query`]) the per-term seeds follow the strength
/// of each term's best match, strongest first, so the seeds rendered first are
/// the strongest; terms it does not name follow in sorted order (Graphify
/// 0.9.87, #4357).
pub fn pick_seeds_in_order(
    g: Option<&Graph>,
    ranked: &[(f64, usize)],
    max: usize,
    gap: f64,
    best: &BTreeMap<String, usize>,
    term_order: &[String],
) -> Vec<usize> {
    let Some((top, _)) = ranked.first() else {
        return vec![];
    };
    let key = |i: usize| {
        g.map(|g| {
            let s = norm_label(g, i);
            if s.is_empty() { id(g, i) } else { s }
        })
        .unwrap_or_else(|| i.to_string())
    };
    let mut labels = BTreeSet::new();
    let mut seeds = vec![];
    for (score, i) in ranked {
        if seeds.len() >= max || !seeds.is_empty() && *score < top * gap {
            break;
        }
        if labels.insert(key(*i)) {
            seeds.push(*i);
        }
    }
    if g.is_some() {
        let mut ordered = term_order
            .iter()
            .filter(|t| best.contains_key(*t))
            .collect::<Vec<_>>();
        let named = ordered.iter().copied().collect::<BTreeSet<_>>();
        ordered.extend(best.keys().filter(|t| !named.contains(t)));
        for term in ordered {
            let i = &best[term];
            if !seeds.contains(i) && labels.insert(key(*i)) {
                seeds.push(*i);
            }
        }
    }
    seeds
}
pub fn normalize_contexts(filters: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    filters
        .iter()
        .map(|s| fold(s).trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .map(|s| {
            CONSTANTS["_CONTEXT_FILTER_ALIASES"][&s]
                .as_str()
                .unwrap_or(&s)
                .to_string()
        })
        .filter(|s| seen.insert(s.clone()))
        .collect()
}
pub fn infer_contexts(question: &str) -> Vec<String> {
    let lowered = question
        .replace(['?', ','], " ")
        .split_whitespace()
        .map(|s| fold(s).to_lowercase())
        .collect::<BTreeSet<_>>();
    CONSTANTS["_CONTEXT_HINTS"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row[1]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| lowered.contains(h.as_str().unwrap()))
        })
        .map(|row| row[0].as_str().unwrap().into())
        .collect()
}
pub fn resolve_contexts(q: &str, filters: &[String]) -> (Vec<String>, Option<&'static str>) {
    let f = normalize_contexts(filters);
    if !f.is_empty() {
        return (f, Some("explicit"));
    }
    let f = infer_contexts(q);
    let kind = (!f.is_empty()).then_some("heuristic");
    (f, kind)
}
pub fn score_json(g: &Graph, q: &QueryScores) -> Value {
    json!({"ranked":q.ranked.iter().map(|(s,i)|json!([s,g.nodes[*i].id])).collect::<Vec<_>>(),"best_seed_by_term":q.best_seed_by_term.iter().map(|(t,i)|(t.clone(),g.nodes[*i].id.clone())).collect::<Map<_,_>>(),"term_order":q.term_order})
}

#[cfg(test)]
mod index_tests {
    use super::{SearchIndex, search_text, trigrams};
    use crate::graph::Graph;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    /// The string-keyed index and candidate search as first ported.
    fn reference(g: &Graph) -> BTreeMap<String, Vec<usize>> {
        let mut postings = BTreeMap::<String, Vec<usize>>::new();
        for i in 0..g.nodes.len() {
            for gram in trigrams(&search_text(g, i)) {
                postings.entry(gram).or_default().push(i);
            }
        }
        postings
    }
    fn reference_candidates(
        postings: &BTreeMap<String, Vec<usize>>,
        size: usize,
        needles: &[String],
        guard: f64,
    ) -> Option<Vec<usize>> {
        if size == 0 {
            return Some(vec![]);
        }
        let needles = needles.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let threshold = (size as f64 * guard).trunc();
        for s in &needles {
            let grams = trigrams(s);
            if grams.is_empty() || grams.iter().any(|g| g.chars().count() < 3) {
                return None;
            }
            if grams
                .iter()
                .filter_map(|g| postings.get(g).map(Vec::len))
                .min()
                .is_some_and(|n| n as f64 > threshold)
            {
                return None;
            }
        }
        let mut out = BTreeSet::new();
        for s in needles {
            let Some(mut sets) = trigrams(s)
                .iter()
                .map(|g| postings.get(g))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            sets.sort_by_key(|s| s.len());
            let Some(first) = sets.first() else { continue };
            let mut hits = first.iter().copied().collect::<BTreeSet<_>>();
            for other in sets.iter().skip(1) {
                hits.retain(|i| other.binary_search(i).is_ok());
            }
            out.extend(hits);
        }
        Some(out.into_iter().collect())
    }

    fn sample(n: usize) -> Graph {
        let mut g = Graph::new(true, false);
        let labels = [
            "parse_request()",
            "Résumé",
            "ab",
            "x",
            "",
            "日本語テキスト",
            "HTTPClient.send",
            "émoji 🚀 launch",
            "get_user_by_id",
            "__init__.py",
            "a_b",
        ];
        for i in 0..n {
            let label = labels[i % labels.len()];
            let attrs = json!({"label": format!("{label}{}", if i % 3 == 0 { String::new() } else { i.to_string() }),
                "source_file": format!("pkg/mod{}/file_{}.py", i % 7, i % 13),
                "rationale": if i % 5 == 0 { json!(["Handles retries", "Ünïcode"]) } else { json!(null) }});
            g.add_node(json!(format!("n{i}")), attrs.as_object().unwrap().clone())
                .unwrap();
        }
        g
    }

    #[test]
    fn packed_index_matches_the_string_index() {
        for n in [0, 1, 7, 64, 700, 3000] {
            let g = sample(n);
            for i in 0..n {
                assert_eq!(super::node_text(&g, i).1, search_text(&g, i), "node {i}");
            }
            let index = SearchIndex::new(&g);
            let expected = reference(&g);
            assert_eq!(index.postings(), expected, "n={n}");
            for needles in [
                vec!["parse"],
                vec!["ab"],
                vec!["résumé"],
                vec!["日本語"],
                vec!["get_user", "http"],
                vec!["zzz"],
                vec!["file_1", "mod3"],
                vec![""],
                vec!["🚀 l"],
                vec!["x", "parse"],
            ] {
                let needles: Vec<String> = needles.into_iter().map(String::from).collect();
                for guard in [0.0, 0.1, 1.0] {
                    assert_eq!(
                        index.candidates(&needles, guard),
                        reference_candidates(&expected, g.nodes.len(), &needles, guard),
                        "n={n} needles={needles:?} guard={guard}"
                    );
                }
            }
        }
    }
}
