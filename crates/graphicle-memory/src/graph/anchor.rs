//! Semantic document chunks anchored to code (plan item G06).
//!
//! A chunk of prose (a design note, a README paragraph, a ticket comment) is
//! tied to the AST it talks about through a fixed ladder; the first rung that
//! yields a result wins and later rungs are not consulted:
//!
//! 1. **Span containment** – an AST node from the same file whose span
//!    contains the chunk's byte range.
//! 2. **Lexical, tier ≤ 2** – a lexical hit whose score is close to the best
//!    hit *and* whose name literally appears in the chunk (tier 1: exact
//!    token; tier 2: case- and underscore-insensitive).
//! 3. **Community pseudo-node** – two or more near-best lexical hits fall in
//!    the same community, so the chunk describes that neighbourhood.
//! 4. **Unanchored** – a legitimate, labelled state. Nothing is fabricated.
//!
//! Chunk ids are idempotent: the same repository, path, index and text always
//! produce the same id, so re-anchoring after a re-index is a no-op.

use crate::graph::{
    Result,
    graph::Graph,
    retrieval::lexical_candidates,
    serve_search::{SearchIndex, text as value_text},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// One chunk of a semantic document, addressed by repository-relative path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentChunk {
    pub repo: String,
    pub relpath: String,
    pub index: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
}

/// Which rung of the ladder produced the anchor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rung", rename_all = "snake_case")]
pub enum AnchorDecision {
    Span {
        node_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        stable_id: Option<String>,
    },
    Lexical {
        node_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        stable_id: Option<String>,
        tier: u8,
    },
    Community {
        community_id: String,
    },
    Unanchored,
}

impl AnchorDecision {
    /// Edge confidence score by rung; `None` for unanchored chunks.
    pub fn confidence_score(&self) -> Option<f64> {
        match self {
            Self::Span { .. } => Some(0.95),
            Self::Lexical { tier: 1, .. } => Some(0.8),
            Self::Lexical { .. } => Some(0.65),
            Self::Community { .. } => Some(0.5),
            Self::Unanchored => None,
        }
    }

    /// Graph identifier of the anchor target (community targets are
    /// `community:<id>`).
    pub fn target(&self) -> Option<String> {
        match self {
            Self::Span { node_id, .. } | Self::Lexical { node_id, .. } => Some(node_id.clone()),
            Self::Community { community_id } => Some(format!("community:{community_id}")),
            Self::Unanchored => None,
        }
    }
}

/// A lexical hit that survived the score gate, with the facts used to judge it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnchorCandidate {
    pub node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    pub label: String,
    pub score: f64,
    /// `1` or `2` when the label literally appears in the chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub chunk_id: String,
    pub decision: AnchorDecision,
    pub candidates: Vec<AnchorCandidate>,
}

/// Ladder parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct AnchorOptions {
    /// Lexical hits examined per chunk.
    pub lexical_limit: usize,
    /// A hit is considered only if `score >= lexical_ratio * best_score`.
    pub lexical_ratio: f64,
    /// Repository-relative path → file text. Needed to compare a chunk's byte
    /// range against nodes that only carry line spans (`source_location`
    /// `L12-L30`, `span.start_line`); nodes with byte spans need no source.
    pub sources: BTreeMap<String, String>,
}

impl Default for AnchorOptions {
    fn default() -> Self {
        Self {
            lexical_limit: 10,
            lexical_ratio: 0.6,
            sources: BTreeMap::new(),
        }
    }
}

impl AnchorOptions {
    pub fn source(mut self, relpath: impl Into<String>, text: impl Into<String>) -> Self {
        self.sources
            .insert(normalize_path(&relpath.into()), text.into());
        self
    }
}

/// Edge material for an anchored chunk: `source` is the chunk id, `target` a
/// node id or `community:<id>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnchorEdge {
    pub source: String,
    pub target: String,
    pub attributes: Map<String, Value>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

/// `{repo}_{relpath_hash}_{index}_{sha256(text)}`; stable across runs.
pub fn chunk_id(chunk: &DocumentChunk) -> String {
    let relpath_hash = &sha256_hex(normalize_path(&chunk.relpath).as_bytes())[..16];
    format!(
        "{}_{}_{}_{}",
        chunk.repo,
        relpath_hash,
        chunk.index,
        sha256_hex(chunk.text.as_bytes())
    )
}

/// Same file, tolerating an absolute prefix on either side.
fn same_file(source_file: &str, relpath: &str) -> bool {
    let (a, b) = (normalize_path(source_file), normalize_path(relpath));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

fn attr_text(attributes: &impl crate::graph::record::Properties, key: &str) -> Option<String> {
    attributes
        .get(key)
        .filter(|v| !v.is_null())
        .map(value_text)
        .filter(|s| !s.is_empty())
}

fn attr_usize(attributes: &impl crate::graph::record::Properties, key: &str) -> Option<usize> {
    attributes
        .get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
}

fn is_code_node(attributes: &impl crate::graph::record::Properties) -> bool {
    attr_text(attributes, "file_type").is_none_or(|kind| kind == "code")
}

/// Inclusive byte span `[start, end]` if the node declares one.
fn byte_span(attributes: &impl crate::graph::record::Properties) -> Option<(usize, usize)> {
    let span = attributes.get("span").and_then(Value::as_object);
    let start = span
        .and_then(|s| attr_usize(s, "start_byte"))
        .or_else(|| attr_usize(attributes, "byte_start"))?;
    let end = span
        .and_then(|s| attr_usize(s, "end_byte"))
        .or_else(|| attr_usize(attributes, "byte_end"))?;
    Some((start, end))
}

/// Inclusive 1-based line span if the node declares one (`span`, `line` /
/// `end_line`, or `source_location` `L12-L30` / `L12`).
fn line_span(attributes: &impl crate::graph::record::Properties) -> Option<(usize, usize)> {
    if let Some(span) = attributes.get("span").and_then(Value::as_object)
        && let Some(start) = attr_usize(span, "start_line")
    {
        return Some((start, attr_usize(span, "end_line").unwrap_or(start)));
    }
    if let Some(start) = attr_usize(attributes, "line") {
        return Some((start, attr_usize(attributes, "end_line").unwrap_or(start)));
    }
    let location = attr_text(attributes, "source_location")?;
    let mut parts = location.split('-');
    let start: usize = parts.next()?.trim().strip_prefix('L')?.parse().ok()?;
    let end = parts
        .next()
        .and_then(|p| p.trim().strip_prefix('L').unwrap_or(p.trim()).parse().ok())
        .unwrap_or(start);
    Some((start, end))
}

/// 1-based line containing byte offset `offset` (clamped to the text).
fn line_at(source: &str, offset: usize) -> usize {
    let offset = offset.min(source.len());
    source.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

fn source_for<'a>(options: &'a AnchorOptions, relpath: &str) -> Option<&'a str> {
    let wanted = normalize_path(relpath);
    options
        .sources
        .iter()
        .find(|(path, _)| same_file(path, &wanted))
        .map(|(_, text)| text.as_str())
}

/// The tightest code node whose span contains the chunk range.
fn span_anchor(graph: &Graph, chunk: &DocumentChunk, options: &AnchorOptions) -> Option<usize> {
    if chunk.byte_end < chunk.byte_start {
        return None;
    }
    let source = source_for(options, &chunk.relpath);
    let chunk_lines = source.map(|text| {
        let end = chunk.byte_end.saturating_sub(1).max(chunk.byte_start);
        (line_at(text, chunk.byte_start), line_at(text, end))
    });
    let mut best: Option<(usize, usize)> = None;
    for (index, node) in graph.nodes().iter().enumerate() {
        let attributes = &node.attributes;
        if !is_code_node(attributes) {
            continue;
        }
        let Some(source_file) = attr_text(attributes, "source_file") else {
            continue;
        };
        if !same_file(&source_file, &chunk.relpath) {
            continue;
        }
        let width = if let Some((start, end)) = byte_span(attributes) {
            (start <= chunk.byte_start && chunk.byte_end <= end).then(|| end - start)
        } else if let (Some((start, end)), Some((first, last))) =
            (line_span(attributes), chunk_lines)
        {
            (start <= first && last <= end).then(|| end - start)
        } else {
            None
        };
        if let Some(width) = width
            && best.is_none_or(|(w, i)| {
                width < w || (width == w && value_text(&node.id) < value_text(&graph.nodes()[i].id))
            })
        {
            best = Some((width, index));
        }
    }
    best.map(|(_, index)| index)
}

/// `.method()` → `method`, `Class.member` → `member`; otherwise verbatim.
fn name_part(label: &str) -> &str {
    let trimmed = label.trim();
    let trimmed = trimmed.strip_prefix('.').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix("()").unwrap_or(trimmed);
    trimmed.rsplit('.').next().unwrap_or(trimmed)
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Whole-token, case-sensitive occurrence.
fn contains_token(text: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find(token) {
        let start = from + offset;
        let end = start + token.len();
        let before = start == 0 || !is_word_byte(bytes[start - 1]);
        let after = end == bytes.len() || !is_word_byte(bytes[end]);
        if before && after {
            return true;
        }
        from = start + token.len().max(1);
        if from >= text.len() {
            break;
        }
    }
    false
}

fn fold_name(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Case- and underscore-insensitive occurrence, checked per token of the
/// chunk so `parsePayload` matches `parse_payload` but `payload` does not
/// match `parse_payload`.
fn contains_folded(text: &str, token: &str) -> bool {
    let wanted = fold_name(token);
    if wanted.is_empty() {
        return false;
    }
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .any(|word| fold_name(word) == wanted)
}

/// Split an identifier at camel-case and separator boundaries:
/// `parsePayload` → `["parse", "payload"]`, `HTTP_Server` → `["http", "server"]`.
fn split_identifier(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = word.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            continue;
        }
        let boundary = i > 0
            && c.is_uppercase()
            && (chars[i - 1].is_lowercase()
                || chars[i - 1].is_ascii_digit()
                || chars.get(i + 1).is_some_and(|n| n.is_lowercase())
                    && chars[i - 1].is_uppercase());
        if boundary && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// The chunk text plus the split form of every identifier-like word, so the
/// parity lexical scorer (which keys on whole tokens) can reach a
/// `parse_payload` node from prose that says `parsePayload`.
fn lexical_query(text: &str) -> String {
    let mut query = text.to_string();
    let mut seen = std::collections::BTreeSet::new();
    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')) {
        let parts = split_identifier(word);
        if parts.len() > 1 {
            for part in parts {
                if part.chars().count() > 1 && seen.insert(part.clone()) {
                    query.push(' ');
                    query.push_str(&part);
                }
            }
        }
    }
    query
}

fn literal_tier(text: &str, label: &str) -> Option<u8> {
    let name = name_part(label);
    if contains_token(text, name) {
        Some(1)
    } else if contains_folded(text, name) {
        Some(2)
    } else {
        None
    }
}

fn community_of(communities: Option<&[(String, Vec<String>)]>, node_id: &str) -> Option<String> {
    communities?
        .iter()
        .find(|(_, members)| members.iter().any(|m| m == node_id))
        .map(|(id, _)| id.clone())
}

/// Anchor one chunk with a prebuilt lexical index (see [`anchor_all`]).
pub fn anchor_with_index(
    graph: &Graph,
    index: &SearchIndex,
    chunk: &DocumentChunk,
    communities: Option<&[(String, Vec<String>)]>,
    options: &AnchorOptions,
) -> Anchor {
    let chunk_id = chunk_id(chunk);
    if let Some(node) = span_anchor(graph, chunk, options) {
        let node = &graph.nodes()[node];
        return Anchor {
            chunk_id,
            decision: AnchorDecision::Span {
                node_id: value_text(&node.id),
                stable_id: attr_text(&node.attributes, "stable_id"),
            },
            candidates: vec![],
        };
    }
    let query = lexical_query(&chunk.text);
    let hits = lexical_candidates(graph, index, &query, options.lexical_limit).candidates;
    let best = hits.first().map_or(0.0, |hit| hit.score);
    let candidates: Vec<AnchorCandidate> = hits
        .iter()
        .filter(|hit| best > 0.0 && hit.score >= options.lexical_ratio * best)
        .filter(|hit| {
            graph
                .index_of(&hit.node_id)
                .is_none_or(|i| is_code_node(&graph.nodes()[i].attributes))
        })
        .map(|hit| AnchorCandidate {
            node_id: hit.node_id.clone(),
            stable_id: hit.stable_id.clone(),
            label: hit.label.clone(),
            score: hit.score,
            tier: literal_tier(&chunk.text, &hit.label),
            community_id: community_of(communities, &hit.node_id),
        })
        .collect();
    if let Some(hit) = candidates.iter().find(|c| c.tier.is_some()) {
        return Anchor {
            chunk_id,
            decision: AnchorDecision::Lexical {
                node_id: hit.node_id.clone(),
                stable_id: hit.stable_id.clone(),
                tier: hit.tier.unwrap_or(2),
            },
            candidates,
        };
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for community in candidates.iter().filter_map(|c| c.community_id.as_deref()) {
        *counts.entry(community).or_default() += 1;
    }
    // Most hits first; the smallest id breaks ties, so the choice is stable.
    let community = counts
        .iter()
        .filter(|(_, count)| **count >= 2)
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(id, _)| (*id).to_string());
    let decision = match community {
        Some(community_id) => AnchorDecision::Community { community_id },
        None => AnchorDecision::Unanchored,
    };
    Anchor {
        chunk_id,
        decision,
        candidates,
    }
}

/// Anchor one chunk. Builds the lexical index for `graph`; prefer
/// [`anchor_all`] for many chunks against the same graph.
pub fn anchor(
    graph: &Graph,
    chunk: &DocumentChunk,
    communities: Option<&[(String, Vec<String>)]>,
    options: &AnchorOptions,
) -> Anchor {
    let index = SearchIndex::new(graph);
    anchor_with_index(graph, &index, chunk, communities, options)
}

/// Anchor every chunk, building the lexical index once.
pub fn anchor_all(
    graph: &Graph,
    chunks: &[DocumentChunk],
    communities: Option<&[(String, Vec<String>)]>,
    options: &AnchorOptions,
) -> Vec<Anchor> {
    let index = SearchIndex::new(graph);
    chunks
        .iter()
        .map(|chunk| anchor_with_index(graph, &index, chunk, communities, options))
        .collect()
}

/// `describes` edges for anchored chunks only; unanchored chunks emit nothing.
pub fn anchor_edges(anchors: &[Anchor]) -> Vec<AnchorEdge> {
    anchors
        .iter()
        .filter_map(|anchor| {
            let target = anchor.decision.target()?;
            let score = anchor.decision.confidence_score()?;
            let rung = match &anchor.decision {
                AnchorDecision::Span { .. } => "span",
                AnchorDecision::Lexical { .. } => "lexical",
                AnchorDecision::Community { .. } => "community",
                AnchorDecision::Unanchored => return None,
            };
            let mut attributes = Map::new();
            attributes.insert("relation".into(), json!("describes"));
            attributes.insert("confidence".into(), json!("INFERRED"));
            attributes.insert("confidence_score".into(), json!(score));
            attributes.insert("derivation".into(), json!("semantic"));
            attributes.insert("anchor_rung".into(), json!(rung));
            if let AnchorDecision::Lexical { tier, .. } = &anchor.decision {
                attributes.insert("anchor_tier".into(), json!(tier));
            }
            Some(AnchorEdge {
                source: anchor.chunk_id.clone(),
                target,
                attributes,
            })
        })
        .collect()
}

/// Add the chunk nodes and `describes` edges of [`anchor_edges`] to `graph`.
/// Community targets become `community:<id>` pseudo-nodes. Returns the number
/// of edges added.
pub fn attach(graph: &mut Graph, anchors: &[Anchor], chunks: &[DocumentChunk]) -> Result<usize> {
    let by_id: BTreeMap<String, &DocumentChunk> = chunks.iter().map(|c| (chunk_id(c), c)).collect();
    let edges = anchor_edges(anchors);
    for edge in &edges {
        let mut attributes = Map::new();
        attributes.insert("file_type".into(), json!("docs"));
        attributes.insert("kind".into(), json!("chunk"));
        if let Some(chunk) = by_id.get(&edge.source) {
            attributes.insert(
                "label".into(),
                json!(format!("{}#{}", chunk.relpath, chunk.index)),
            );
            attributes.insert("source_file".into(), json!(chunk.relpath));
            attributes.insert("repo".into(), json!(chunk.repo));
            attributes.insert("byte_start".into(), json!(chunk.byte_start));
            attributes.insert("byte_end".into(), json!(chunk.byte_end));
        }
        graph.add_node(json!(edge.source), attributes)?;
        if let Some(community) = edge.target.strip_prefix("community:") {
            let mut pseudo = Map::new();
            pseudo.insert("kind".into(), json!("community"));
            pseudo.insert("label".into(), json!(format!("community {community}")));
            graph.add_node(json!(edge.target), pseudo)?;
        }
        graph.add_edge(
            json!(edge.source),
            json!(edge.target),
            None,
            edge.attributes.clone(),
        )?;
    }
    Ok(edges.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::attributes;

    fn graph() -> Graph {
        let mut graph = Graph::new(true, false);
        let node = |g: &mut Graph, id: &str, label: &str, file: &str, loc: &str| {
            g.node(
                id,
                attributes([
                    ("label", json!(label)),
                    ("source_file", json!(file)),
                    ("source_location", json!(loc)),
                    ("file_type", json!("code")),
                    ("stable_id", json!(format!("stable:{id}"))),
                ]),
            )
            .unwrap();
        };
        node(&mut graph, "app.py", "app.py", "src/app.py", "L1-L40");
        node(
            &mut graph,
            "handler",
            "request_handler()",
            "src/app.py",
            "L10-L20",
        );
        node(
            &mut graph,
            "helper",
            "parse_payload()",
            "src/util.py",
            "L3-L9",
        );
        node(&mut graph, "config", "Config", "src/config.py", "L1-L5");
        node(
            &mut graph,
            "loader",
            "load_settings()",
            "src/config.py",
            "L7-L12",
        );
        graph
            .node(
                "doc",
                attributes([
                    ("label", json!("parse_payload notes")),
                    ("source_file", json!("docs/design.md")),
                    ("file_type", json!("docs")),
                ]),
            )
            .unwrap();
        for (s, t, r) in [
            ("app.py", "handler", "contains"),
            ("handler", "helper", "calls"),
            ("helper", "config", "references"),
            ("config", "loader", "contains"),
        ] {
            graph
                .relate(s, t, r, attributes([("confidence", json!("EXTRACTED"))]))
                .unwrap();
        }
        graph
    }

    fn chunk(relpath: &str, index: usize, range: (usize, usize), text: &str) -> DocumentChunk {
        DocumentChunk {
            repo: "repo".into(),
            relpath: relpath.into(),
            index,
            byte_start: range.0,
            byte_end: range.1,
            text: text.into(),
        }
    }

    #[test]
    fn chunk_ids_are_idempotent_and_content_addressed() {
        let a = chunk("docs/x.md", 0, (0, 5), "hello");
        let b = chunk("docs/x.md", 0, (0, 5), "hello");
        let c = chunk("docs/x.md", 0, (0, 5), "hello!");
        let d = chunk("docs/y.md", 0, (0, 5), "hello");
        assert_eq!(chunk_id(&a), chunk_id(&b));
        assert_ne!(chunk_id(&a), chunk_id(&c));
        assert_ne!(chunk_id(&a), chunk_id(&d));
        assert!(chunk_id(&a).starts_with("repo_"));
        assert!(chunk_id(&a).contains("_0_"));
        assert!(chunk_id(&a).ends_with(&sha256_hex(b"hello")));
    }

    #[test]
    fn span_containment_picks_the_tightest_node_and_tolerates_absolute_paths() {
        let graph = graph();
        // 40 lines of 10 bytes each; lines 12-13 sit inside `handler`.
        let source: String = (1..=40).map(|i| format!("line{i:04}\n")).collect();
        let options = AnchorOptions::default().source("src/app.py", source);
        let docstring = chunk("/abs/checkout/src/app.py", 0, (110, 130), "docstring");
        let anchor = anchor(&graph, &docstring, None, &options);
        assert_eq!(
            anchor.decision,
            AnchorDecision::Span {
                node_id: "handler".into(),
                stable_id: Some("stable:handler".into()),
            }
        );
        // A range spilling past the function falls back to the file node.
        let wide = chunk("src/app.py", 1, (110, 300), "docstring");
        let anchor = super::anchor(&graph, &wide, None, &options);
        assert!(
            matches!(anchor.decision, AnchorDecision::Span { ref node_id, .. } if node_id == "app.py")
        );
        // Byte spans need no source text.
        let mut g = graph;
        g.node(
            "bytes",
            attributes([
                ("label", json!("byte_span()")),
                ("source_file", json!("src/other.py")),
                ("span", json!({"start_byte": 100, "end_byte": 200})),
            ]),
        )
        .unwrap();
        let inside = chunk("src/other.py", 0, (120, 150), "anything");
        let anchor = super::anchor(&g, &inside, None, &AnchorOptions::default());
        assert!(
            matches!(anchor.decision, AnchorDecision::Span { ref node_id, .. } if node_id == "bytes")
        );
    }

    #[test]
    fn span_rung_is_skipped_without_line_evidence() {
        let graph = graph();
        // No source text → line spans cannot be compared → ladder continues.
        let c = chunk("src/app.py", 0, (110, 130), "unrelated prose");
        let anchor = anchor(&graph, &c, None, &AnchorOptions::default());
        assert_eq!(anchor.decision, AnchorDecision::Unanchored);
    }

    #[test]
    fn lexical_tier_one_requires_the_exact_token() {
        let graph = graph();
        let c = chunk(
            "docs/design.md",
            0,
            (0, 10),
            "We call parse_payload before validation.",
        );
        let anchor = anchor(&graph, &c, None, &AnchorOptions::default());
        assert_eq!(
            anchor.decision,
            AnchorDecision::Lexical {
                node_id: "helper".into(),
                stable_id: Some("stable:helper".into()),
                tier: 1,
            }
        );
        assert!(anchor.candidates.iter().all(|c| c.node_id != "doc"));
    }

    #[test]
    fn lexical_tier_two_folds_case_and_underscores() {
        let graph = graph();
        let c = chunk("docs/design.md", 1, (0, 10), "ParsePayload runs first.");
        let anchor = anchor(&graph, &c, None, &AnchorOptions::default());
        assert_eq!(
            anchor.decision,
            AnchorDecision::Lexical {
                node_id: "helper".into(),
                stable_id: Some("stable:helper".into()),
                tier: 2,
            }
        );
    }

    #[test]
    fn lexical_hits_far_below_the_best_are_ignored() {
        let graph = graph();
        let c = chunk("docs/design.md", 2, (0, 10), "request_handler and Config");
        let strict = AnchorOptions {
            lexical_ratio: 1.0,
            ..AnchorOptions::default()
        };
        let anchor = anchor(&graph, &c, None, &strict);
        assert_eq!(anchor.candidates.len(), 1, "{:?}", anchor.candidates);
        assert!(matches!(anchor.decision, AnchorDecision::Lexical { .. }));
    }

    #[test]
    fn community_rung_needs_two_gated_hits_in_one_community() {
        let graph = graph();
        let communities = vec![
            (
                "c0".to_string(),
                vec!["config".to_string(), "loader".to_string()],
            ),
            (
                "c1".to_string(),
                vec!["handler".to_string(), "helper".to_string()],
            ),
        ];
        // Terms overlap the labels lexically (trigram score) without either
        // label appearing literally, so tiers fail and communities decide.
        let c = chunk("docs/ops.md", 0, (0, 10), "load conf");
        let anchor = anchor(&graph, &c, Some(&communities), &AnchorOptions::default());
        assert_eq!(
            anchor.decision,
            AnchorDecision::Community {
                community_id: "c0".into()
            },
            "{:?}",
            anchor.candidates
        );
        // Without communities the same chunk is an honest unanchored state.
        let anchor = super::anchor(&graph, &c, None, &AnchorOptions::default());
        assert_eq!(anchor.decision, AnchorDecision::Unanchored);
        assert!(!anchor.candidates.is_empty());
    }

    #[test]
    fn unanchored_chunks_emit_no_edges() {
        let graph = graph();
        let chunks = vec![
            chunk("docs/design.md", 0, (0, 10), "We call parse_payload here."),
            chunk(
                "docs/design.md",
                1,
                (10, 20),
                "Nothing about the code at all.",
            ),
        ];
        let anchors = anchor_all(&graph, &chunks, None, &AnchorOptions::default());
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[1].decision, AnchorDecision::Unanchored);
        let edges = anchor_edges(&anchors);
        assert_eq!(edges.len(), 1);
        let edge = &edges[0];
        assert_eq!(edge.source, anchors[0].chunk_id);
        assert_eq!(edge.target, "helper");
        assert_eq!(edge.attributes["relation"], json!("describes"));
        assert_eq!(edge.attributes["confidence"], json!("INFERRED"));
        assert_eq!(edge.attributes["confidence_score"], json!(0.8));
        assert_eq!(edge.attributes["derivation"], json!("semantic"));
    }

    #[test]
    fn identifier_splitting_feeds_the_lexical_query() {
        assert_eq!(split_identifier("parsePayload"), vec!["parse", "payload"]);
        assert_eq!(split_identifier("HTTP_Server"), vec!["http", "server"]);
        assert_eq!(split_identifier("HTTPServer"), vec!["http", "server"]);
        assert_eq!(split_identifier("plain"), vec!["plain"]);
        assert_eq!(split_identifier("v2Api"), vec!["v2", "api"]);
        let query = lexical_query("ParsePayload runs first.");
        assert!(query.starts_with("ParsePayload runs first."));
        assert!(query.ends_with(" parse payload"));
        assert_eq!(lexical_query("plain words only"), "plain words only");
    }

    #[test]
    fn confidence_scores_follow_the_rung() {
        let span = AnchorDecision::Span {
            node_id: "a".into(),
            stable_id: None,
        };
        let t1 = AnchorDecision::Lexical {
            node_id: "a".into(),
            stable_id: None,
            tier: 1,
        };
        let t2 = AnchorDecision::Lexical {
            node_id: "a".into(),
            stable_id: None,
            tier: 2,
        };
        let community = AnchorDecision::Community {
            community_id: "c0".into(),
        };
        assert_eq!(span.confidence_score(), Some(0.95));
        assert_eq!(t1.confidence_score(), Some(0.8));
        assert_eq!(t2.confidence_score(), Some(0.65));
        assert_eq!(community.confidence_score(), Some(0.5));
        assert_eq!(AnchorDecision::Unanchored.confidence_score(), None);
        assert_eq!(community.target().as_deref(), Some("community:c0"));
    }

    #[test]
    fn attach_adds_chunk_nodes_and_pseudo_nodes_without_touching_existing_edges() {
        let mut graph = graph();
        let before = graph.edge_count();
        let communities = vec![(
            "c0".to_string(),
            vec!["config".to_string(), "loader".to_string()],
        )];
        let chunks = vec![
            chunk("docs/design.md", 0, (0, 10), "We call parse_payload here."),
            chunk("docs/ops.md", 0, (0, 10), "load conf"),
            chunk("docs/ops.md", 1, (0, 10), "nothing relevant"),
        ];
        let anchors = anchor_all(
            &graph,
            &chunks,
            Some(&communities),
            &AnchorOptions::default(),
        );
        let added = attach(&mut graph, &anchors, &chunks).unwrap();
        assert_eq!(added, 2);
        assert_eq!(graph.edge_count(), before + 2);
        assert!(graph.index_of("community:c0").is_some());
        let chunk_node = graph.index_of(&anchors[0].chunk_id).unwrap();
        assert_eq!(
            graph.nodes()[chunk_node].attributes["file_type"],
            json!("docs")
        );
        assert_eq!(
            graph.nodes()[chunk_node].attributes["label"],
            json!("docs/design.md#0")
        );
        assert!(graph.index_of(&anchors[2].chunk_id).is_none());
    }
}
