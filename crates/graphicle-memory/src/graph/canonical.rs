//! Scoped canonicalization (plan item G07).
//!
//! Two graph entities that name the same thing (`parsePayload` in one file,
//! `parse_payload` in another; a re-exported symbol and its definition) are
//! nominated as aliases through the Graphiti ladder with code thresholds:
//!
//! 1. **Exact** – identical normalised names (case, `_`, `-` and camel-case
//!    boundaries folded).
//! 2. **Entropy gate** – names whose normalised form carries less than
//!    `min_entropy` bits of Shannon entropy (`x`, `tmp`, `data`) never merge on
//!    any rung; they are too common to be evidence of identity.
//! 3. **MinHash** – estimated Jaccard of character 3-gram shingles ≥ 0.9.
//! 4. **Embedding** – cosine ≥ 0.85 *and* at least one shared neighbour.
//! 5. **LLM** – external; this module never produces it (see [`Rung::Llm`]).
//!
//! **Scope rule**: a proposal never crosses a repository or a namespace
//! boundary, whatever the similarity. Proposals are reviewable data; applying
//! them rewrites edge endpoints through a union-find [`AliasSet`] while keeping
//! every node, so a single alias can be reverted.
//!
//! Portions adapted from Graphiti (<https://github.com/getzep/graphiti>), Copyright 2024, Zep Software, Inc., Apache-2.0; changes are described in NOTICE.md.

use crate::graph::{Result, graph::Graph, serve_search::text as value_text};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A node record as the canonicalizer sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub stable_id: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub neighbours: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

/// Ladder rung that produced a proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rung {
    Exact,
    MinHash,
    Embedding,
    /// External: an LLM-nominated alias recorded by the host. Never produced
    /// by [`propose_aliases`]; present so reviewed proposals from every
    /// source share one type.
    Llm,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AliasProposal {
    /// `stable_id` of the lexicographically smaller entity.
    pub left: String,
    pub right: String,
    pub rung: Rung,
    pub score: f64,
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalOptions {
    /// Minimum Shannon entropy (bits per character) of a normalised name.
    pub min_entropy: f64,
    /// MinHash-estimated Jaccard threshold on character 3-gram shingles.
    pub minhash_threshold: f64,
    /// Cosine threshold on embeddings.
    pub embedding_threshold: f64,
    /// Hash permutations used by the MinHash estimate.
    pub num_perm: usize,
    /// Only entities of the same `kind` may alias (a class never aliases a
    /// function of the same name).
    pub require_same_kind: bool,
}

impl Default for CanonicalOptions {
    fn default() -> Self {
        Self {
            min_entropy: 2.0,
            minhash_threshold: 0.9,
            embedding_threshold: 0.85,
            num_perm: 64,
            require_same_kind: true,
        }
    }
}

/// Fold case, separators and camel-case boundaries: `parse_payload`,
/// `parsePayload`, `Parse-Payload` and `.parsePayload()` all become
/// `parsepayload`.
pub fn normalize_name(label: &str) -> String {
    let trimmed = label.trim();
    let trimmed = trimmed.strip_prefix('.').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix("()").unwrap_or(trimmed);
    trimmed
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' ' | '\t'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Shannon entropy in bits over the characters of `s`.
pub fn shannon_entropy(s: &str) -> f64 {
    let mut counts: BTreeMap<char, usize> = BTreeMap::new();
    for c in s.chars() {
        *counts.entry(c).or_default() += 1;
    }
    let total = s.chars().count() as f64;
    if total == 0.0 {
        return 0.0;
    }
    counts
        .values()
        .map(|&n| {
            let p = n as f64 / total;
            -p * p.log2()
        })
        .sum()
}

fn shingles(s: &str) -> BTreeSet<Vec<u8>> {
    let bytes: Vec<u8> = s.bytes().collect();
    if bytes.len() <= 3 {
        return std::iter::once(bytes).collect();
    }
    bytes.windows(3).map(<[u8]>::to_vec).collect()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// MinHash signature of a shingle set: `num_perm` independent hash functions,
/// each realised as splitmix64 over the shingle hash XOR a per-permutation seed.
pub fn minhash_signature(shingles: &BTreeSet<Vec<u8>>, num_perm: usize) -> Vec<u64> {
    let hashes: Vec<u64> = shingles.iter().map(|s| fnv1a(s)).collect();
    (0..num_perm)
        .map(|i| {
            let seed = splitmix64(i as u64);
            hashes
                .iter()
                .map(|&h| splitmix64(h ^ seed))
                .min()
                .unwrap_or(u64::MAX)
        })
        .collect()
}

/// Estimated Jaccard similarity from two signatures of equal length.
pub fn minhash_similarity(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).filter(|(x, y)| x == y).count() as f64 / a.len() as f64
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

struct Prepared<'a> {
    entity: &'a Entity,
    name: String,
    signature: Vec<u64>,
    neighbours: BTreeSet<&'a str>,
}

fn scope_key(entity: &Entity, options: &CanonicalOptions) -> (String, Option<String>, String) {
    (
        entity.repo.clone(),
        entity.namespace.clone(),
        if options.require_same_kind {
            entity.kind.clone()
        } else {
            String::new()
        },
    )
}

fn judge(
    a: &Prepared<'_>,
    b: &Prepared<'_>,
    options: &CanonicalOptions,
) -> Option<(Rung, f64, String)> {
    if a.name == b.name {
        return Some((Rung::Exact, 1.0, format!("normalised name `{}`", a.name)));
    }
    let estimate = minhash_similarity(&a.signature, &b.signature);
    if estimate >= options.minhash_threshold {
        return Some((
            Rung::MinHash,
            estimate,
            format!(
                "minhash jaccard {estimate:.3} over 3-gram shingles of `{}` / `{}`",
                a.name, b.name
            ),
        ));
    }
    let (Some(ea), Some(eb)) = (&a.entity.embedding, &b.entity.embedding) else {
        return None;
    };
    let similarity = cosine(ea, eb)?;
    if similarity < options.embedding_threshold {
        return None;
    }
    let shared = a.neighbours.intersection(&b.neighbours).next()?;
    Some((
        Rung::Embedding,
        similarity,
        format!("cosine {similarity:.3} with shared neighbour `{shared}`"),
    ))
}

/// Nominate aliases within each `(repo, namespace[, kind])` scope. Output is
/// sorted by `(left, right)`; one proposal per pair, from the first rung that
/// fires. Never crosses repositories or namespaces.
pub fn propose_aliases(entities: &[Entity], options: &CanonicalOptions) -> Vec<AliasProposal> {
    let mut scopes: BTreeMap<(String, Option<String>, String), Vec<Prepared<'_>>> = BTreeMap::new();
    for entity in entities {
        let name = normalize_name(&entity.label);
        if name.is_empty() || shannon_entropy(&name) < options.min_entropy {
            continue;
        }
        let signature = minhash_signature(&shingles(&name), options.num_perm);
        scopes
            .entry(scope_key(entity, options))
            .or_default()
            .push(Prepared {
                entity,
                name,
                signature,
                neighbours: entity.neighbours.iter().map(String::as_str).collect(),
            });
    }
    let mut proposals = Vec::new();
    for members in scopes.values_mut() {
        members.sort_by(|a, b| a.entity.stable_id.cmp(&b.entity.stable_id));
        for (i, a) in members.iter().enumerate() {
            for b in &members[i + 1..] {
                if a.entity.stable_id == b.entity.stable_id {
                    continue;
                }
                if let Some((rung, score, evidence)) = judge(a, b, options) {
                    proposals.push(AliasProposal {
                        left: a.entity.stable_id.clone(),
                        right: b.entity.stable_id.clone(),
                        rung,
                        score,
                        evidence,
                    });
                }
            }
        }
    }
    proposals.sort_by(|a, b| {
        a.left
            .cmp(&b.left)
            .then_with(|| a.right.cmp(&b.right))
            .then_with(|| a.rung.cmp(&b.rung))
    });
    proposals
}

/// Union-find over stable ids. The canonical member of a set is its
/// lexicographically smallest id, so resolution does not depend on the order
/// in which aliases were added.
#[derive(Clone, Debug, Default)]
pub struct AliasSet {
    ids: Vec<String>,
    index: HashMap<String, usize>,
    parent: Vec<usize>,
    /// Index of the smallest id in each root's set.
    smallest: Vec<usize>,
}

impl AliasSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from reviewed proposals.
    pub fn from_proposals<'a>(proposals: impl IntoIterator<Item = &'a AliasProposal>) -> Self {
        let mut set = Self::new();
        for proposal in proposals {
            set.union(&proposal.left, &proposal.right);
        }
        set
    }

    fn intern(&mut self, id: &str) -> usize {
        if let Some(&i) = self.index.get(id) {
            return i;
        }
        let i = self.ids.len();
        self.ids.push(id.to_string());
        self.index.insert(id.to_string(), i);
        self.parent.push(i);
        self.smallest.push(i);
        i
    }

    fn root(&self, mut i: usize) -> usize {
        while self.parent[i] != i {
            i = self.parent[i];
        }
        i
    }

    /// Declare `a` and `b` aliases of one another.
    pub fn union(&mut self, a: &str, b: &str) {
        let (a, b) = (self.intern(a), self.intern(b));
        let (ra, rb) = (self.root(a), self.root(b));
        if ra == rb {
            return;
        }
        self.parent[rb] = ra;
        let (sa, sb) = (self.smallest[ra], self.smallest[rb]);
        if self.ids[sb] < self.ids[sa] {
            self.smallest[ra] = sb;
        }
    }

    /// Canonical id of `id`, or `None` if `id` was never mentioned.
    pub fn find(&self, id: &str) -> Option<&str> {
        let i = *self.index.get(id)?;
        Some(&self.ids[self.smallest[self.root(i)]])
    }

    /// Canonical id of `id`; an unknown id is its own canonical form.
    pub fn canonical<'a>(&'a self, id: &'a str) -> &'a str {
        self.find(id).unwrap_or(id)
    }

    /// Whether `a` and `b` resolve to the same canonical id.
    pub fn same(&self, a: &str, b: &str) -> bool {
        self.canonical(a) == self.canonical(b)
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
}

/// What [`apply`] changed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplySummary {
    pub nodes_aliased: usize,
    pub edges_rewritten: usize,
    /// Aliases whose canonical node is absent from the graph; left untouched.
    pub missing_canonical: Vec<String>,
}

fn node_key(node: &crate::graph::graph::Node) -> String {
    node.attributes
        .get("stable_id")
        .filter(|v| !v.is_null())
        .map(value_text)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| value_text(&node.id))
}

/// Rewrite edge endpoints of aliased nodes to their canonical node. Nodes are
/// never deleted: an aliased node gets `alias_of = <canonical node id>` and
/// every moved edge records the original endpoint in `aliased_source` /
/// `aliased_target`, so [`revert`] can undo one alias in isolation. In a plain
/// graph a move that collides with an existing edge is folded into the
/// aliased node's [`FOLDED`] record instead of merging attributes.
pub fn apply(graph: &mut Graph, aliases: &AliasSet) -> Result<ApplySummary> {
    let mut summary = ApplySummary::default();
    // stable id → node index, for canonical lookups.
    let by_key: HashMap<String, usize> = graph
        .nodes()
        .iter()
        .enumerate()
        .map(|(i, node)| (node_key(node), i))
        .collect();
    // aliased node index → canonical node index.
    let mut redirect: BTreeMap<usize, usize> = BTreeMap::new();
    for (i, node) in graph.nodes().iter().enumerate() {
        let key = node_key(node);
        let canonical = aliases.canonical(&key);
        if canonical == key {
            continue;
        }
        match by_key.get(canonical) {
            Some(&target) if target != i => {
                redirect.insert(i, target);
            }
            Some(_) => {}
            None => summary.missing_canonical.push(key),
        }
    }
    if redirect.is_empty() {
        return Ok(summary);
    }
    let ids: Vec<Value> = graph.nodes().iter().map(|n| n.id.clone()).collect();
    let multigraph = graph.is_multigraph();
    let mut moved = Vec::new();
    for (u, v, edge) in graph.iter_edges() {
        let (nu, nv) = (redirect.get(&u).copied(), redirect.get(&v).copied());
        if nu.is_none() && nv.is_none() {
            continue;
        }
        let mut attributes = edge.attributes.clone();
        if nu.is_some() {
            attributes.insert("aliased_source".into(), ids[u].clone());
        }
        if nv.is_some() {
            attributes.insert("aliased_target".into(), ids[v].clone());
        }
        // The aliased node that owns a fold record if the move collides.
        let owner = if nu.is_some() { u } else { v };
        moved.push((
            ids[u].clone(),
            ids[v].clone(),
            multigraph.then(|| edge.key.clone()),
            ids[nu.unwrap_or(u)].clone(),
            ids[nv.unwrap_or(v)].clone(),
            attributes,
            owner,
        ));
    }
    let mut folded: BTreeMap<usize, Vec<Value>> = BTreeMap::new();
    for (source, target, key, new_source, new_target, mut attributes, owner) in moved {
        graph.remove_edge(&source, &target, key.as_ref())?;
        let collides = if multigraph {
            // A multigraph keys parallel edges by relation; only the same key
            // between the new endpoints collides.
            graph
                .edges_between(&new_source, &new_target)
                .iter()
                .any(|edge| Some(&edge.key) == key.as_ref())
        } else {
            graph.number_of_edges(&new_source, &new_target) > 0
        };
        if collides {
            // The graph cannot hold both edges. The surviving edge keeps its
            // own attributes; the displaced one is stored on the aliased node
            // so `revert` can recreate it exactly.
            attributes.remove("aliased_source");
            attributes.remove("aliased_target");
            folded.entry(owner).or_default().push(json!({
                "source": source,
                "target": target,
                "key": key.clone().unwrap_or(Value::Null),
                "attributes": Value::Object(attributes.into()),
            }));
        } else {
            // Keep the original key so the move (and its revert) is exact.
            graph.add_edge(new_source, new_target, key, attributes)?;
        }
        summary.edges_rewritten += 1;
    }
    for (&i, &target) in &redirect {
        let canonical_id = ids[target].clone();
        if let Some(node) = graph.nodes_mut().nth(i) {
            node.attributes.insert("alias_of".into(), canonical_id);
            if let Some(records) = folded.remove(&i) {
                let existing = node
                    .attributes
                    .remove(FOLDED)
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default();
                node.attributes.insert(
                    FOLDED.into(),
                    Value::Array(existing.into_iter().chain(records).collect()),
                );
            }
            summary.nodes_aliased += 1;
        }
    }
    Ok(summary)
}

/// Node attribute holding edges a plain graph could not keep after aliasing.
pub const FOLDED: &str = "alias_folded_edges";

/// Undo the alias of one node: edges that were moved away from it return,
/// and its `alias_of` marker is removed. Returns the number of edges restored.
pub fn revert(graph: &mut Graph, id: &str) -> Result<usize> {
    let node_id = json!(id);
    let Some(index) = graph.node_index(&node_id) else {
        return Err(crate::graph::Error::invalid(format!("Unknown node `{id}`")));
    };
    let ids: Vec<Value> = graph.nodes().iter().map(|n| n.id.clone()).collect();
    let multigraph = graph.is_multigraph();
    let mut moved = Vec::new();
    for (u, v, edge) in graph.iter_edges() {
        let from_here = edge.attributes.get("aliased_source") == Some(&node_id);
        let to_here = edge.attributes.get("aliased_target") == Some(&node_id);
        if !from_here && !to_here {
            continue;
        }
        let mut attributes = edge.attributes.clone();
        let new_source = if from_here {
            attributes.remove("aliased_source");
            node_id.clone()
        } else {
            ids[u].clone()
        };
        let new_target = if to_here {
            attributes.remove("aliased_target");
            node_id.clone()
        } else {
            ids[v].clone()
        };
        moved.push((
            ids[u].clone(),
            ids[v].clone(),
            multigraph.then(|| edge.key.clone()),
            new_source,
            new_target,
            attributes,
        ));
    }
    let mut restored = moved.len();
    for (source, target, key, new_source, new_target, attributes) in moved {
        graph.remove_edge(&source, &target, key.as_ref())?;
        graph.add_edge(new_source, new_target, key, attributes)?;
    }
    let folded = graph
        .nodes_mut()
        .nth(index)
        .and_then(|node| {
            node.attributes.remove("alias_of");
            node.attributes.remove(FOLDED)
        })
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    for record in folded {
        let (Some(source), Some(target)) = (record.get("source"), record.get("target")) else {
            continue;
        };
        let attributes = record
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // Fold records written before keys were preserved carry no key; a
        // null key lets the graph assign one as before.
        let key = record.get("key").filter(|k| !k.is_null()).cloned();
        graph.add_edge(source.clone(), target.clone(), key, attributes)?;
        restored += 1;
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::attributes;
    use serde_json::Map;

    fn entity(stable_id: &str, repo: &str, namespace: Option<&str>, label: &str) -> Entity {
        Entity {
            id: stable_id.to_string(),
            stable_id: stable_id.to_string(),
            repo: repo.to_string(),
            namespace: namespace.map(str::to_string),
            label: label.to_string(),
            kind: "function".to_string(),
            neighbours: vec![],
            embedding: None,
        }
    }

    #[test]
    fn exact_rung_folds_case_separators_and_call_syntax() {
        let entities = vec![
            entity("b", "r", None, "parsePayload"),
            entity("a", "r", None, ".parse_payload()"),
            entity("c", "r", None, "Parse-Payload"),
        ];
        let proposals = propose_aliases(&entities, &CanonicalOptions::default());
        let pairs: Vec<(&str, &str, Rung)> = proposals
            .iter()
            .map(|p| (p.left.as_str(), p.right.as_str(), p.rung))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("a", "b", Rung::Exact),
                ("a", "c", Rung::Exact),
                ("b", "c", Rung::Exact)
            ]
        );
        assert!(proposals.iter().all(|p| p.score == 1.0));
    }

    #[test]
    fn entropy_gate_blocks_short_generic_names_on_every_rung() {
        for name in ["x", "tmp", "data", "i", "aaaa"] {
            let entities = vec![entity("a", "r", None, name), entity("b", "r", None, name)];
            assert!(
                propose_aliases(&entities, &CanonicalOptions::default()).is_empty(),
                "{name} merged"
            );
            assert!(shannon_entropy(&normalize_name(name)) < 2.0, "{name}");
        }
        // `user` sits exactly at 2.0 bits and passes.
        let entities = vec![
            entity("a", "r", None, "user"),
            entity("b", "r", None, "User"),
        ];
        assert_eq!(
            propose_aliases(&entities, &CanonicalOptions::default()).len(),
            1
        );
    }

    #[test]
    fn minhash_rung_catches_near_identical_names() {
        let entities = vec![
            entity("a", "r", None, "buildCustomerInvoiceReportHandler"),
            entity("b", "r", None, "buildCustomerInvoiceReportHandlers"),
            entity("c", "r", None, "renderLoginPage"),
        ];
        let proposals = propose_aliases(&entities, &CanonicalOptions::default());
        assert_eq!(proposals.len(), 1, "{proposals:?}");
        assert_eq!(proposals[0].rung, Rung::MinHash);
        assert_eq!(
            (&proposals[0].left[..], &proposals[0].right[..]),
            ("a", "b")
        );
        assert!(proposals[0].score >= 0.9);
        // The estimate tracks exact Jaccard for identical and disjoint sets.
        let s = shingles("parsepayload");
        let sig = minhash_signature(&s, 64);
        assert_eq!(minhash_similarity(&sig, &sig), 1.0);
        let other = minhash_signature(&shingles("zzzzqqqq"), 64);
        assert!(minhash_similarity(&sig, &other) < 0.2);
    }

    #[test]
    fn embedding_rung_requires_threshold_and_a_shared_neighbour() {
        let mut a = entity("a", "r", None, "fetch_customer_orders");
        let mut b = entity("b", "r", None, "load_client_purchases");
        a.embedding = Some(vec![1.0, 0.2, 0.0]);
        b.embedding = Some(vec![1.0, 0.25, 0.05]);
        // High cosine, no shared neighbour → nothing.
        a.neighbours = vec!["orders_repo".into()];
        b.neighbours = vec!["purchase_cache".into()];
        assert!(propose_aliases(&[a.clone(), b.clone()], &CanonicalOptions::default()).is_empty());
        // Shared neighbour → embedding rung.
        b.neighbours.push("orders_repo".into());
        let proposals = propose_aliases(&[a.clone(), b.clone()], &CanonicalOptions::default());
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].rung, Rung::Embedding);
        assert!(proposals[0].score >= 0.85);
        assert!(proposals[0].evidence.contains("orders_repo"));
        // Below the cosine threshold → nothing even with the shared neighbour.
        b.embedding = Some(vec![0.0, 1.0, 0.0]);
        assert!(propose_aliases(&[a, b], &CanonicalOptions::default()).is_empty());
    }

    #[test]
    fn never_proposes_across_repositories_or_namespaces() {
        let same = |left: Entity, right: Entity| {
            propose_aliases(&[left, right], &CanonicalOptions::default())
        };
        assert!(
            same(
                entity("a", "repo-one", None, "parse_payload"),
                entity("b", "repo-two", None, "parse_payload")
            )
            .is_empty()
        );
        assert!(
            same(
                entity("a", "r", Some("billing"), "parse_payload"),
                entity("b", "r", Some("auth"), "parse_payload")
            )
            .is_empty()
        );
        assert!(
            same(
                entity("a", "r", Some("billing"), "parse_payload"),
                entity("b", "r", None, "parse_payload")
            )
            .is_empty()
        );
        assert_eq!(
            same(
                entity("a", "r", Some("billing"), "parse_payload"),
                entity("b", "r", Some("billing"), "parsePayload")
            )
            .len(),
            1
        );
        // Kinds are a scope too by default.
        let mut class = entity("b", "r", None, "parse_payload");
        class.kind = "class".into();
        assert!(same(entity("a", "r", None, "parse_payload"), class.clone()).is_empty());
        let relaxed = CanonicalOptions {
            require_same_kind: false,
            ..CanonicalOptions::default()
        };
        assert_eq!(
            propose_aliases(&[entity("a", "r", None, "parse_payload"), class], &relaxed).len(),
            1
        );
    }

    #[test]
    fn proposals_are_deterministic_regardless_of_input_order() {
        let mut entities = vec![
            entity("z", "r", None, "parsePayload"),
            entity("m", "r", None, "parse_payload"),
            entity("a", "r", None, "buildReportHandler"),
            entity("k", "r", None, "buildReportHandlers"),
        ];
        let first = propose_aliases(&entities, &CanonicalOptions::default());
        entities.reverse();
        let second = propose_aliases(&entities, &CanonicalOptions::default());
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert!(
            first
                .windows(2)
                .all(|w| (&w[0].left, &w[0].right) <= (&w[1].left, &w[1].right))
        );
    }

    #[test]
    fn alias_set_resolves_to_the_smallest_id() {
        let mut set = AliasSet::new();
        set.union("m", "z");
        set.union("z", "b");
        set.union("q", "q");
        assert_eq!(set.canonical("z"), "b");
        assert_eq!(set.canonical("m"), "b");
        assert_eq!(set.canonical("b"), "b");
        assert_eq!(set.canonical("q"), "q");
        assert_eq!(set.canonical("unknown"), "unknown");
        assert_eq!(set.find("unknown"), None);
        assert!(set.same("m", "z"));
        assert!(!set.same("m", "q"));
        assert_eq!(set.len(), 4);
        let proposals = vec![AliasProposal {
            left: "a".into(),
            right: "c".into(),
            rung: Rung::Llm,
            score: 0.7,
            evidence: "reviewed".into(),
        }];
        let from = AliasSet::from_proposals(&proposals);
        assert_eq!(from.canonical("c"), "a");
    }

    fn graph() -> Graph {
        let mut graph = Graph::new(true, false);
        for (id, stable) in [
            ("canon", "s:canon"),
            ("dup", "s:dup"),
            ("caller", "s:caller"),
            ("callee", "s:callee"),
        ] {
            graph
                .node(
                    id,
                    attributes([("stable_id", json!(stable)), ("label", json!(id))]),
                )
                .unwrap();
        }
        graph
            .relate(
                "caller",
                "dup",
                "calls",
                attributes([("confidence", json!("EXTRACTED"))]),
            )
            .unwrap();
        graph
            .relate(
                "dup",
                "callee",
                "calls",
                attributes([("confidence", json!("EXTRACTED"))]),
            )
            .unwrap();
        graph
            .relate("caller", "canon", "imports", Map::new())
            .unwrap();
        graph
    }

    #[test]
    fn apply_rewrites_endpoints_keeps_nodes_and_revert_restores_them() {
        let mut graph = graph();
        let mut aliases = AliasSet::new();
        aliases.union("s:dup", "s:canon");
        let summary = apply(&mut graph, &aliases).unwrap();
        assert_eq!(summary.nodes_aliased, 1);
        assert_eq!(summary.edges_rewritten, 2);
        assert!(summary.missing_canonical.is_empty());
        assert_eq!(graph.node_count(), 4, "nodes are never deleted");
        let dup = graph.index_of("dup").unwrap();
        assert_eq!(graph.nodes()[dup].attributes["alias_of"], json!("canon"));
        assert_eq!(graph.number_of_edges(&json!("caller"), &json!("dup")), 0);
        assert_eq!(graph.number_of_edges(&json!("caller"), &json!("canon")), 1);
        assert_eq!(graph.number_of_edges(&json!("canon"), &json!("callee")), 1);
        let moved = &graph.edges_between(&json!("canon"), &json!("callee"))[0];
        assert_eq!(moved.attributes["aliased_source"], json!("dup"));
        assert_eq!(moved.attributes["relation"], json!("calls"));
        // `caller -> dup` collided with the existing `caller -> canon`
        // (`imports`): the survivor is untouched, the displaced edge is folded.
        let kept = &graph.edges_between(&json!("caller"), &json!("canon"))[0];
        assert_eq!(kept.attributes["relation"], json!("imports"));
        assert!(!kept.attributes.contains_key("aliased_target"));
        let folded = graph.nodes()[dup].attributes[FOLDED].as_array().unwrap();
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0]["attributes"]["relation"], json!("calls"));
        // Applying again is a no-op.
        let again = apply(&mut graph, &aliases).unwrap();
        assert_eq!(again.edges_rewritten, 0);

        let restored = revert(&mut graph, "dup").unwrap();
        assert_eq!(restored, 2);
        assert!(!graph.nodes()[dup].attributes.contains_key("alias_of"));
        assert!(!graph.nodes()[dup].attributes.contains_key(FOLDED));
        assert_eq!(graph.number_of_edges(&json!("caller"), &json!("dup")), 1);
        let back = &graph.edges_between(&json!("caller"), &json!("dup"))[0];
        assert_eq!(back.attributes["relation"], json!("calls"));
        assert_eq!(graph.number_of_edges(&json!("dup"), &json!("callee")), 1);
        assert_eq!(graph.number_of_edges(&json!("canon"), &json!("callee")), 0);
        // The untouched edge survives, and markers are gone.
        assert_eq!(graph.number_of_edges(&json!("caller"), &json!("canon")), 1);
        assert_eq!(
            graph.edges_between(&json!("caller"), &json!("canon"))[0].attributes["relation"],
            json!("imports")
        );
        let back = &graph.edges_between(&json!("dup"), &json!("callee"))[0];
        assert!(!back.attributes.contains_key("aliased_source"));
        assert!(revert(&mut graph, "ghost").is_err());
    }

    #[test]
    fn apply_reports_aliases_whose_canonical_is_missing() {
        let mut graph = graph();
        let mut aliases = AliasSet::new();
        aliases.union("s:dup", "s:absent-but-smaller");
        let summary = apply(&mut graph, &aliases).unwrap();
        assert_eq!(summary.edges_rewritten, 0);
        assert_eq!(summary.missing_canonical, vec!["s:dup".to_string()]);
        assert_eq!(graph.number_of_edges(&json!("caller"), &json!("dup")), 1);
    }
}
