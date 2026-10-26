//! G04 · Typed code-graph arm of the fusion pipeline.
//!
//! Lexical candidates on the resident graph of the repository's recorded
//! current episode, expanded one hop, are anchored to source byte ranges so
//! `rank` fuses them with the other arms and `pack` cites them with
//! `stable_id`, `revision` and `derivation`. The arm never builds an episode
//! inside an interactive query: without a recorded episode it is skipped and
//! the response says so. While the current head set waits for its
//! projection, the arm reads the latest recorded episode, cites only the
//! files whose bytes that episode recorded, and follows only the edges read
//! from those files (the lag fallback).
use crate::{
    Result, graph_projection,
    query::Snapshot,
    store::{one, s},
};
use graphicle::graph::Graph;
use graphicle_memory::graph::{
    retrieval::{ExpandOptions, expand, expand_seeds, lexical_candidates, resident_index},
    span,
};
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

pub const ARM: &str = "code_graph";
/// Per-arm candidate cap.
pub const CAP: usize = 300;
/// Expansion depth around the lexical seeds.
pub const DEPTH: i64 = 1;
/// Synthetic evidence items for nodes without a chunk, per response.
pub const UNCHUNKED_LIMIT: usize = 6;

/// One graph node anchored to a byte range of a current source document.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub document: String,
    pub byte_start: usize,
    pub byte_end: usize,
    /// `stable_id`, `revision`, `derivation`, `relpath`, line range, ...
    pub facts: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Arm {
    /// Anchored hits in arm order: lexical candidates by score, then
    /// one-hop neighbours; at most [`CAP`].
    pub hits: Vec<Hit>,
    /// `{"status": "fused" | "no_episode" | "no_match" | "error", ...}`.
    /// `no_episode` says why ([`skip_reason`]). `fused` and `no_match` give
    /// the episode read and its `episode_lag`: 0 on the current head set, else
    /// the number of files the lag fallback could not cite ([`Reading`]).
    pub diagnostics: Value,
}

/// The episode the arm reads: the current head set's when it
/// is recorded, else, under the lag fallback, the latest recorded
/// episode, citing only the files whose current head still has the bytes
/// that episode recorded.
#[derive(Clone, Debug)]
pub struct Reading {
    pub episode: String,
    /// Set under the lag fallback: the current head set is not recorded yet.
    pub lag: Option<Lag>,
}

/// What the lag fallback may cite.
#[derive(Clone, Debug)]
pub struct Lag {
    /// The current head set's episode, still to be projected.
    pub current: String,
    /// Files whose current head differs from the episode read: added,
    /// changed or removed. The packet's `episode_lag`.
    pub changed: usize,
    /// Files with the same sha256 in the current heads and the episode read:
    /// only their nodes are cited, and only the edges read from them are
    /// followed (any node may still be a seed).
    pub unchanged: BTreeSet<String>,
}

/// The episode the arm reads for the head set `heads` (digest `current`),
/// or `None` when it is skipped: no episode is recorded, or the latest was
/// recorded without its source manifest (older stores), so no file can be
/// checked. "Latest" is the most recently recorded episode, always a head
/// set of this project: `graphicle pr` records commit trees in a separate PR
/// scratch project.
pub fn reading(
    db: &Connection,
    repository: &str,
    heads: &BTreeMap<String, String>,
    current: &str,
) -> Result<Option<Reading>> {
    if graph_projection::is_recorded(db, repository, current)? {
        return Ok(Some(Reading {
            episode: current.to_string(),
            lag: None,
        }));
    }
    let Some(latest) = one(
        db,
        "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
        [repository],
    )?
    else {
        return Ok(None);
    };
    let episode = s(&latest, "episode").to_string();
    let Some(recorded) = graph_projection::stored_episode_heads(db, repository, &episode)? else {
        return Ok(None);
    };
    let unchanged: BTreeSet<String> = heads
        .iter()
        .filter(|(path, sha)| recorded.get(*path) == Some(*sha))
        .map(|(path, _)| path.clone())
        .collect();
    let changed =
        heads.len() - unchanged.len() + recorded.keys().filter(|p| !heads.contains_key(*p)).count();
    Ok(Some(Reading {
        episode,
        lag: Some(Lag {
            current: current.to_string(),
            changed,
            unchanged,
        }),
    }))
}

impl Arm {
    pub fn skipped(status: &str) -> Self {
        Self {
            hits: Vec::new(),
            diagnostics: json!({"status": status}),
        }
    }
}

fn attr(attributes: &impl graphicle::record::Properties, key: &str) -> Option<String> {
    value_text(attributes.get(key))
}

/// [`attr`] of an attribute's value: `None` when it is missing, null or an
/// empty string; a string as it is; any other value as its JSON text.
fn value_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// `L31` → (31, None); `L31-L40` / `L31-40` → (31, Some(40)): the start line
/// as identity reads it, and the explicit end a few extractors record.
fn lines(attributes: &impl graphicle::record::Properties) -> Option<(usize, Option<usize>)> {
    Some((span::start_line(attributes)?, span::end_line(attributes)))
}

/// The project-relative path of a node's file: identity's `relpath`, else
/// the engine's `source_file`.
pub fn relpath(attributes: &impl graphicle::record::Properties) -> Option<String> {
    attr(attributes, "relpath")
        .or_else(|| attr(attributes, "source_file"))
        .map(|p| p.replace('\\', "/"))
}

/// Symbol start lines per file of an episode's graph (file nodes left out,
/// as `identity` collects them): a node's range runs to the next symbol
/// start in its file, the bytes its `revision` hashes
/// (`graphicle_memory::graph::span`). Shared by the arm and the unified
/// server's graph tools, so both cite the same ranges.
pub fn symbol_starts(graph: &Graph) -> HashMap<String, Vec<usize>> {
    let mut starts: HashMap<String, Vec<usize>> = HashMap::new();
    for node in graph.nodes() {
        if attr(&node.attributes, "kind").as_deref() == Some("file") {
            continue;
        }
        if let (Some(path), Some(line)) = (
            relpath(&node.attributes),
            span::start_line(&node.attributes),
        ) {
            starts.entry(path).or_default().push(line);
        }
    }
    for list in starts.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    starts
}

/// `(line, end_line, byte_start, byte_end)` of a node in the current text of
/// its file `path`, whose line starts are `offsets` and length
/// `content_len`: from its start line to its explicit end (test identities)
/// or else to the next symbol start of [`symbol_starts`]; a node without a
/// line is the whole file. A start past the end of the file gives an empty
/// range, which the caller leaves unanchored.
pub fn node_span(
    attributes: &impl graphicle::record::Properties,
    path: &str,
    starts: &HashMap<String, Vec<usize>>,
    offsets: &[usize],
    content_len: usize,
) -> (usize, usize, usize, usize) {
    let total_lines = offsets.len();
    match lines(attributes) {
        Some((start, explicit_end)) => {
            let start = start.max(1);
            // An explicit end (test identities) wins over the next start.
            let next = explicit_end.map(|e| e + 1).or_else(|| {
                starts
                    .get(path)
                    .and_then(|list| span::next_start(list, start))
            });
            let end_line = next.map_or(total_lines, |n| (n - 1).max(start));
            let bytes = span::byte_range(offsets, content_len, Some(start), next);
            (start, end_line, bytes.start, bytes.end)
        }
        None => (1, total_lines, 0, content_len),
    }
}

/// Why the arm has no episode to read: the state of the
/// projection of the current head set (`none`, `queued`, `running` or
/// `failed`, over its job and any retry of it: `jobs::projection_state`),
/// the latest recorded episode, and its lag, the number of files whose
/// current head differs from that episode (added, changed or removed).
/// `heads` is the head set the caller read; `episode` is its digest.
///
/// Since the lag fallback a recorded episode with a source manifest is
/// read, so the arm is skipped only without one: the lag is `null` without
/// an episode, and for an episode recorded without its source manifest
/// (older stores): a query never searches the retained sync manifests for
/// it. The job is reported by state only. Whoever synced queued it, and job
/// listings show a job only to its principal and owners.
pub fn skip_reason(
    db: &Connection,
    repository: &str,
    heads: &BTreeMap<String, String>,
    episode: &str,
) -> Result<Value> {
    let projection = crate::jobs::projection_state(db, repository, episode)?;
    // The latest recorded episode. Every episode of a project is one of its
    // head sets: `graphicle pr` records commit trees in a separate PR scratch
    // project, so no
    // commit episode can become "latest" here.
    let latest = one(
        db,
        "SELECT episode,sequence,created_at FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
        [repository],
    )?;
    let lag = match &latest {
        Some(row) => graph_projection::stored_episode_heads(db, repository, s(row, "episode"))?
            .map(|recorded| {
                let changed = heads
                    .iter()
                    .filter(|(path, sha)| recorded.get(*path) != Some(*sha))
                    .count();
                changed + recorded.keys().filter(|p| !heads.contains_key(*p)).count()
            }),
        None => None,
    };
    Ok(
        json!({"status":"no_episode","episode":episode,"projection":projection,"head_files":heads.len(),
        "latest_episode":latest.map(|row| json!({"episode":row["episode"],"sequence":row["sequence"],"recorded_at":row["created_at"]})),
        "episode_lag":lag}),
    )
}

/// The derivation of one connecting edge, in either direction of the
/// undirected traversal view; `unknown` when the graph has no such edge.
fn edge_derivation(graph: &Graph, a: &Value, b: &Value) -> String {
    graph
        .edges_between(a, b)
        .into_iter()
        .chain(graph.edges_between(b, a))
        .find_map(|edge| attr(&edge.attributes, "derivation"))
        .unwrap_or_else(|| "unknown".to_string())
}

/// The files an edge was read from: the `source_file`s of its provenance
/// (project-relative paths, as identity records them), else its own
/// `source_file`. Empty when the edge names none. Shared with the unified
/// server's graph tools, which flag the relations a lagging
/// answer reaches only through edges read from changed files.
pub fn edge_files(attributes: &impl graphicle::record::Properties) -> Vec<String> {
    let mut files: Vec<String> = attributes
        .get("provenance")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("source_file").and_then(Value::as_str))
        .filter(|file| !file.is_empty())
        .map(|file| file.replace('\\', "/"))
        .collect();
    if files.is_empty() {
        files.extend(attr(attributes, "source_file").map(|file| file.replace('\\', "/")));
    }
    files
}

/// Under the lag fallback, the relation and derivation of
/// an edge between `a` and `b`, in either direction, that the traversal view
/// keeps (`filters`, matched against the edge's `context` as `expand` does)
/// and that was read from a file whose bytes the episode recorded: that
/// file still states the relationship. `None` when every connecting edge
/// was read from a changed or removed file, or names no file: the
/// relationship may no longer exist.
fn current_edge(
    graph: &Graph,
    a: &str,
    b: &str,
    filters: &[String],
    unchanged: &BTreeSet<String>,
) -> Option<(String, String)> {
    let (a, b) = (json!(a), json!(b));
    graph
        .edges_between(&a, &b)
        .into_iter()
        .chain(graph.edges_between(&b, &a))
        .filter(|edge| {
            filters.is_empty()
                || filters.contains(&attr(&edge.attributes, "context").unwrap_or_default())
        })
        .find(|edge| {
            edge_files(&edge.attributes)
                .iter()
                .any(|file| unchanged.contains(file))
        })
        .map(|edge| {
            (
                attr(&edge.attributes, "relation").unwrap_or_default(),
                attr(&edge.attributes, "derivation").unwrap_or_else(|| "unknown".to_string()),
            )
        })
}

/// A node of a recorded episode, located in the current text of its file as
/// the arm anchors its hits (the cross-repository step of
/// workspace recall reads and cites the nodes that `gf_cross_edges` join).
#[derive(Clone, Debug)]
pub struct Located {
    /// `file:<relpath>`, the document the range is in.
    pub document: String,
    pub relpath: String,
    /// A file (module) node rather than a symbol.
    pub file: bool,
    pub line: usize,
    pub end_line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    /// `node_id`, `label`, `kind`, `qualified_name`, `stable_id`, `revision`,
    /// `relpath`, `line`, `end_line`, `byte_start` and `byte_end`.
    pub facts: Value,
}

/// The file of `node_id` in `graph` (its `relpath`, else `source_file`),
/// without locating its lines.
pub fn relpath_of(graph: &Graph, node_id: &str) -> Option<String> {
    let index = graph.node_index(&json!(node_id))?;
    relpath(&graph.nodes()[index].attributes)
}

/// Locate `node_id` of `graph` in `documents` (the caller's authorized view
/// of the node's project, by id): `None` when the node is unknown, names no
/// file, its file is not in the view, or its range is empty. `starts` comes
/// from [`symbol_starts`] over the same graph. The caller checks that the
/// file still has the bytes the node's episode recorded.
pub fn locate(
    graph: &Graph,
    starts: &HashMap<String, Vec<usize>>,
    node_id: &str,
    documents: &HashMap<&str, &Value>,
) -> Option<Located> {
    let index = graph.node_index(&json!(node_id))?;
    locate_index(graph, starts, index, documents)
}

fn locate_index(
    graph: &Graph,
    starts: &HashMap<String, Vec<usize>>,
    index: usize,
    documents: &HashMap<&str, &Value>,
) -> Option<Located> {
    let node = &graph.nodes()[index];
    let node_id = match &node.id {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    locate_record(node_id, &node.attributes, starts, documents)
}

/// [`locate`] for a node given by its id and attributes (a query file's
/// record, [`crate::query_file`]).
pub(crate) fn locate_record(
    node_id: String,
    attributes: &impl graphicle::record::Properties,
    starts: &HashMap<String, Vec<usize>>,
    documents: &HashMap<&str, &Value>,
) -> Option<Located> {
    let path = relpath(attributes)?;
    let document = format!("file:{path}");
    let content = s(documents.get(document.as_str())?, "content");
    let offsets = span::line_starts(content);
    let (line, end_line, byte_start, byte_end) =
        node_span(attributes, &path, starts, &offsets, content.len());
    if byte_end <= byte_start {
        return None;
    }
    let file = attr(attributes, "kind").as_deref() == Some("file");
    let facts = json!({
        "node_id": node_id,
        "label": attributes.get("label").cloned().unwrap_or(Value::Null),
        "kind": attributes.get("kind").cloned().unwrap_or(Value::Null),
        "qualified_name": attributes.get("qualified_name").cloned().unwrap_or(Value::Null),
        "stable_id": attributes.get("stable_id").cloned().unwrap_or(Value::Null),
        "revision": attributes.get("revision").cloned().unwrap_or(Value::Null),
        "relpath": path,
        "line": line,
        "end_line": end_line,
        "byte_start": byte_start,
        "byte_end": byte_end,
    });
    Some(Located {
        document,
        relpath: path,
        file,
        line,
        end_line,
        byte_start,
        byte_end,
        facts,
    })
}

/// The symbols (non-file nodes with a start line) of `relpath` in `graph`,
/// located in `documents`, in line order.
pub fn symbols_in(
    graph: &Graph,
    starts: &HashMap<String, Vec<usize>>,
    relpath: &str,
    documents: &HashMap<&str, &Value>,
) -> Vec<Located> {
    let mut found: Vec<Located> = graph
        .nodes()
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            attr(&node.attributes, "kind").as_deref() != Some("file")
                && span::start_line(&node.attributes).is_some()
                && self::relpath(&node.attributes).as_deref() == Some(relpath)
        })
        .filter_map(|(index, _)| locate_index(graph, starts, index, documents))
        .collect();
    found.sort_by_key(|located| (located.line, located.byte_start));
    found
}

/// Lexical candidates and their one-hop neighbourhood on the episode the arm
/// reads ([`reading`]), anchored to the caller's authorized source view.
pub fn arm(db: &Connection, repository: &str, snapshot: &Snapshot, question: &str) -> Result<Arm> {
    arm_with(db, repository, snapshot, question, true)
}

/// A document node of an episode: a Markdown or reStructuredText page or
/// one of its sections.
pub fn document_node(attributes: &impl graphicle::record::Properties) -> bool {
    attr(attributes, "file_type").as_deref() == Some("document")
        || attr(attributes, "kind").as_deref() == Some("document")
}

/// [`lexical_candidates`] over the episode without its document nodes:
/// they neither rank nor seed, so no `references` edge from a document
/// leads the walk to code (`doc_links: false`). A
/// term whose best seed was a document node loses that seed rather than
/// gaining the next one.
fn code_candidates(
    graph: &Graph,
    index: &graphicle::serve_search::SearchIndex,
    question: &str,
) -> graphicle_memory::graph::Candidates {
    use graphicle::serve_search::{CONSTANTS, id, label, pick_seeds, score_query};
    let terms = graphicle::query_terms::query_terms(question);
    let mut scored = score_query(graph, index, &terms, true);
    let document = |i: usize| document_node(&graph.nodes()[i].attributes);
    scored.ranked.retain(|(_, i)| !document(*i));
    scored.best_seed_by_term.retain(|_, i| !document(*i));
    let intent = CONSTANTS["_RELATIONAL_INTENT_TERMS"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if terms.iter().any(|t| !intent.contains(&json!(t))) {
        scored
            .best_seed_by_term
            .retain(|t, _| !intent.contains(&json!(t)));
    }
    let seeds = pick_seeds(
        Some(graph),
        &scored.ranked,
        3,
        0.2,
        &scored.best_seed_by_term,
    );
    let text = |i: usize, key: &str| attr(&graph.nodes()[i].attributes, key);
    graphicle_memory::graph::Candidates {
        terms,
        matched: scored.ranked.len(),
        candidates: scored
            .ranked
            .iter()
            .take(CAP)
            .map(|&(score, i)| graphicle_memory::graph::Candidate {
                node_id: id(graph, i),
                score,
                label: label(graph, i),
                source_file: text(i, "source_file"),
                source_location: text(i, "source_location"),
                stable_id: text(i, "stable_id"),
                revision: text(i, "revision"),
                kind: text(i, "kind"),
            })
            .collect(),
        seeds: seeds.iter().map(|&i| id(graph, i)).collect(),
    }
}

/// [`arm`]; with `documents` false (`doc_links: false`) the episode's
/// document nodes take no part: they are not candidates, seeds or cited
/// neighbours ([`code_candidates`]).
pub fn arm_with(
    db: &Connection,
    repository: &str,
    snapshot: &Snapshot,
    question: &str,
    documents: bool,
) -> Result<Arm> {
    let plan = plan(db, repository, question, documents)?;
    let by_id: HashMap<&str, &Value> = snapshot.documents.iter().map(|d| (s(d, "id"), d)).collect();
    plan.anchor(db, repository, &by_id)
}

/// A node the arm cites, as [`plan`] found it: its id, hop (0 for a
/// lexical candidate, 1 for a neighbour) and score, the attributes the
/// citation reads, how the walk reached it, and the derivation of the edge it
/// was reached through.
#[derive(Clone, Debug)]
pub struct Planned {
    pub node_id: String,
    pub hop: u8,
    pub score: Option<f64>,
    /// `None`: the node is not in the episode the arm reads.
    pub attributes: Option<Map<String, Value>>,
    /// (seed it was reached from, relation, the derivation under the lag
    /// fallback).
    pub via: Option<(String, String, Option<String>)>,
    /// The derivation of a connecting edge on the current episode
    /// (`edge_derivation`), for a neighbour reached without one.
    pub derivation: Option<String>,
}

/// The graph half of the arm: the nodes it cites, in arm order, before
/// they are anchored to the current text of their files ([`Plan::anchor`]).
pub struct Plan {
    /// Set when the arm is skipped or found nothing: its diagnostics.
    pub skipped: Option<Value>,
    pub episode: String,
    pub lag: Option<Lag>,
    pub documents: bool,
    pub terms: Vec<String>,
    pub matched: usize,
    pub candidates: usize,
    pub seeds: Vec<String>,
    pub expanded: usize,
    pub nodes: Vec<Planned>,
    /// Symbol start lines of the files of `nodes` ([`symbol_starts`]).
    pub starts: HashMap<String, Vec<usize>>,
}

impl Plan {
    /// The project-relative paths of the files the planned nodes are in:
    /// the documents [`Plan::anchor`] reads.
    pub fn files(&self) -> BTreeSet<String> {
        self.nodes
            .iter()
            .filter_map(|node| node.attributes.as_ref().and_then(relpath))
            .collect()
    }

    /// Anchor the planned nodes to `documents` (the caller's authorized view
    /// of the project, by id, with the text of [`Plan::files`]).
    pub fn anchor(
        &self,
        db: &Connection,
        repository: &str,
        documents: &HashMap<&str, &Value>,
    ) -> Result<Arm> {
        let lagging = |diagnostics: &mut Value, withheld: usize, stale: usize| {
            if let Some(lag) = &self.lag {
                diagnostics["episode_lag"] = json!(lag.changed);
                diagnostics["current_episode"] = json!(lag.current);
                diagnostics["projection"] =
                    json!(crate::jobs::projection_state(db, repository, &lag.current)?);
                diagnostics["withheld_changed"] = json!(withheld);
                diagnostics["withheld_stale_edges"] = json!(stale);
            }
            Ok::<_, crate::Error>(())
        };
        if let Some(skipped) = &self.skipped {
            let mut diagnostics = skipped.clone();
            if diagnostics["status"] == "no_match" {
                lagging(&mut diagnostics, 0, 0)?;
                if !self.documents {
                    diagnostics["document_nodes"] = json!("excluded");
                }
            }
            return Ok(Arm {
                hits: Vec::new(),
                diagnostics,
            });
        }
        let citable = |path: &str| {
            self.lag
                .as_ref()
                .is_none_or(|lag| lag.unchanged.contains(path))
        };
        let mut offsets: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut hits = Vec::new();
        let mut unanchored = 0usize;
        let mut skipped_files = 0usize;
        let mut withheld = 0usize;
        let mut withheld_stale = 0usize;
        for node in &self.nodes {
            let Some(attributes) = &node.attributes else {
                continue;
            };
            let Some(path) = relpath(attributes) else {
                unanchored += 1;
                continue;
            };
            if !citable(&path) {
                withheld += 1;
                continue;
            }
            if node.hop != 0 && self.lag.is_some() && node.via.is_none() {
                // Reached only through edges read from changed or removed files.
                withheld_stale += 1;
                continue;
            }
            let document_id = format!("file:{path}");
            let Some(document) = documents.get(document_id.as_str()) else {
                unanchored += 1;
                continue;
            };
            let content = s(document, "content");
            if lines(attributes).is_none() && node.hop != 0 {
                // A whole file reached only by expansion is not a citation.
                skipped_files += 1;
                continue;
            }
            let offsets = offsets
                .entry(s(document, "id"))
                .or_insert_with(|| span::line_starts(content));
            let (line, end_line, byte_start, byte_end) =
                node_span(attributes, &path, &self.starts, offsets, content.len());
            // A start past the end of the current file is an empty range, so the
            // node goes unanchored instead of citing the last line.
            if byte_end <= byte_start {
                unanchored += 1;
                continue;
            }
            let derivation = match &node.via {
                Some((_, _, Some(derivation))) => derivation.clone(),
                Some((_, _, None)) => node
                    .derivation
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                None => "lexical_match".to_string(),
            };
            let mut facts = json!({
                "node_id": node.node_id,
                "label": attributes.get("label").cloned().unwrap_or(Value::Null),
                "kind": attributes.get("kind").cloned().unwrap_or(Value::Null),
                "qualified_name": attributes.get("qualified_name").cloned().unwrap_or(Value::Null),
                "stable_id": attributes.get("stable_id").cloned().unwrap_or(Value::Null),
                "revision": attributes.get("revision").cloned().unwrap_or(Value::Null),
                "relpath": path,
                "line": line,
                "end_line": end_line,
                "byte_start": byte_start,
                "byte_end": byte_end,
                "derivation": derivation,
                "hop": node.hop,
                "episode": self.episode,
            });
            if let Some(score) = node.score {
                facts["score"] = json!(score);
            }
            if let Some((from, relation, _)) = &node.via {
                facts["via"] = json!({"node_id": from, "relation": relation});
            }
            hits.push(Hit {
                document: document_id,
                byte_start,
                byte_end,
                facts,
            });
        }
        let mut diagnostics = json!({
            "status": "fused",
            "episode": self.episode,
            "episode_lag": 0,
            "terms": self.terms,
            "matched": self.matched,
            "candidates": self.candidates,
            "seeds": self.seeds,
            "expanded": self.expanded,
            "anchored": hits.len(),
            "unanchored": unanchored,
            "skipped_files": skipped_files,
            "depth": DEPTH,
            "cap": CAP,
        });
        lagging(&mut diagnostics, withheld, withheld_stale)?;
        if !self.documents {
            diagnostics["document_nodes"] = json!("excluded");
        }
        Ok(Arm { diagnostics, hits })
    }
}

/// The arm's graph half on the episode it reads: lexical candidates and their
/// one-hop neighbourhood ([`Plan`]). With `documents` false
/// (`doc_links: false`) the episode's document nodes take no part. The
/// episode's query file ([`crate::query_file`]) answers when this build
/// wrote one; else the resident graph does, with the same result.
pub fn plan(db: &Connection, repository: &str, question: &str, documents: bool) -> Result<Plan> {
    // The head set is read once: its digest names the current episode, and
    // the lag fallback and a skipped arm measure the latest episode against
    // it.
    let heads = crate::store::heads(db, repository)?;
    let current = graph_projection::episode_of(&heads);
    let Some(Reading { episode, lag }) = reading(db, repository, &heads, &current)? else {
        let mut plan = Plan::empty(String::new(), documents);
        plan.skipped = Some(skip_reason(db, repository, &heads, &current)?);
        return Ok(plan);
    };
    if let Some(file) = crate::phase::phase("arm_query_file", || {
        crate::query_file::open(db, repository, &episode)
    }) {
        let saved = Saved::new(file);
        let planned = plan_on(&saved, episode.clone(), lag.clone(), question, documents);
        saved.file.note("arm_query_file_read");
        if !saved.corrupt() {
            return planned;
        }
        // A query file that does not decode: the resident graph answers.
    }
    let resident = Resident::open(db, repository, &episode)?;
    plan_on(&resident, episode, lag, question, documents)
}

/// What [`plan`] reads of an episode's graph: the resident graph
/// ([`Resident`]) or the episode's query file ([`Saved`]), which answer
/// each read alike.
trait Episode {
    /// The lexical candidates as [`lexical_candidates`] finds them (or
    /// [`code_candidates`], without `documents`): node ids and scores, best
    /// first, at most [`CAP`].
    fn candidates(&self, question: &str, documents: bool) -> Found;
    /// The seeds' neighbourhood as [`expand_seeds`] finds it.
    fn expand(&self, seeds: &[String], question: &str) -> Result<Expanded>;
    /// [`current_edge`] between `a` and `b`.
    fn current_edge(
        &self,
        a: &str,
        b: &str,
        filters: &[String],
        unchanged: &BTreeSet<String>,
    ) -> Option<(String, String)>;
    /// Whether node `id` is a document node ([`document_node`]).
    fn is_document(&self, id: &str) -> bool;
    /// Node `id`'s attributes.
    fn attributes(&self, id: &str) -> Option<Map<String, Value>>;
    /// [`edge_derivation`] between `a` and `b`.
    fn derivation(&self, a: &str, b: &str) -> String;
    /// The symbol start lines of `path` ([`symbol_starts`]).
    fn starts(&self, path: &str) -> Option<Vec<usize>>;
}

/// The lexical candidates of a question.
struct Found {
    terms: Vec<String>,
    matched: usize,
    /// (node id, score), best first.
    candidates: Vec<(String, f64)>,
    seeds: Vec<String>,
}

/// The seeds' neighbourhood.
struct Expanded {
    /// (node id, seed), in the traversal's order.
    nodes: Vec<(String, bool)>,
    /// (source, target, relation) from a seed to a node that is not one.
    edges: Vec<(String, String, String)>,
    filters: Vec<String>,
}

/// The episode's resident graph and its search index.
struct Resident {
    handle: graphicle::session::GraphHandle,
    index: std::sync::Arc<graphicle::serve_search::SearchIndex>,
    starts: std::sync::OnceLock<HashMap<String, Vec<usize>>>,
}

impl Resident {
    fn open(db: &Connection, repository: &str, episode: &str) -> Result<Self> {
        let handle = crate::phase::phase("arm_resident", || {
            graph_projection::resident(db, repository, episode)
        })?;
        let reference = graph_projection::graph_ref(repository, episode);
        let index = crate::phase::phase("arm_index", || resident_index(&reference, &handle));
        Ok(Self {
            handle,
            index,
            starts: std::sync::OnceLock::new(),
        })
    }
}

impl Episode for Resident {
    fn candidates(&self, question: &str, documents: bool) -> Found {
        let graph: &Graph = &self.handle;
        let found = if documents {
            lexical_candidates(graph, &self.index, question, CAP)
        } else {
            code_candidates(graph, &self.index, question)
        };
        Found {
            terms: found.terms,
            matched: found.matched,
            candidates: found
                .candidates
                .into_iter()
                .map(|candidate| (candidate.node_id, candidate.score))
                .collect(),
            seeds: found.seeds,
        }
    }
    fn expand(&self, seeds: &[String], question: &str) -> Result<Expanded> {
        let graph: &Graph = &self.handle;
        let seeds: Vec<Value> = seeds.iter().map(|id| json!(id)).collect();
        // Depth 1 from the seeds: their neighbours, without the traversal
        // view of the whole graph (`expand_seeds`, equal to `expand` there).
        let expansion = if DEPTH == 1 {
            expand_seeds(graph, &seeds, question)
        } else {
            expand(
                graph,
                &seeds,
                &ExpandOptions {
                    depth: DEPTH,
                    question: question.to_string(),
                    ..Default::default()
                },
            )
        }?;
        Ok(Expanded {
            nodes: expansion
                .nodes
                .into_iter()
                .map(|node| (node.node_id, node.seed))
                .collect(),
            edges: expansion
                .edges
                .into_iter()
                .map(|edge| (edge.source, edge.target, edge.relation))
                .collect(),
            filters: expansion.filters,
        })
    }
    fn current_edge(
        &self,
        a: &str,
        b: &str,
        filters: &[String],
        unchanged: &BTreeSet<String>,
    ) -> Option<(String, String)> {
        current_edge(&self.handle, a, b, filters, unchanged)
    }
    fn is_document(&self, id: &str) -> bool {
        let graph: &Graph = &self.handle;
        graph
            .node_index(&json!(id))
            .is_some_and(|index| document_node(&graph.nodes()[index].attributes))
    }
    fn attributes(&self, id: &str) -> Option<Map<String, Value>> {
        let graph: &Graph = &self.handle;
        graph
            .node_index(&json!(id))
            .map(|index| graph.nodes()[index].attributes.to_map())
    }
    fn derivation(&self, a: &str, b: &str) -> String {
        edge_derivation(&self.handle, &json!(a), &json!(b))
    }
    fn starts(&self, path: &str) -> Option<Vec<usize>> {
        self.starts
            .get_or_init(|| {
                crate::phase::phase("arm_symbol_starts", || symbol_starts(&self.handle))
            })
            .get(path)
            .cloned()
    }
}

/// The episode's query file. A read that fails marks the file corrupt
/// ([`crate::query_file::QueryFile::corrupt`]), and [`plan`] then asks the
/// resident graph instead.
struct Saved {
    file: std::sync::Arc<crate::query_file::QueryFile>,
    /// Adjacency records decoded so far, by node.
    adjacency: RefCell<HashMap<usize, Rc<crate::query_file::Adjacency>>>,
}

impl Saved {
    fn new(file: std::sync::Arc<crate::query_file::QueryFile>) -> Self {
        Self {
            file,
            adjacency: RefCell::new(HashMap::new()),
        }
    }
    /// The value of a read; `None` when it failed (the file is then
    /// marked corrupt).
    fn ok<T>(&self, result: std::result::Result<T, crate::query_file::Corrupt>) -> Option<T> {
        result.ok()
    }
    fn corrupt(&self) -> bool {
        self.file.corrupt()
    }
    /// Node `i`'s id ("" when the read failed).
    fn id(&self, i: usize) -> String {
        self.ok(self.file.id(i)).unwrap_or_default().to_string()
    }
    fn node_index(&self, id: &str) -> Option<usize> {
        self.ok(self.file.node_index(id)).flatten()
    }
    fn is_document(&self, i: usize) -> bool {
        self.ok(self.file.is_document(i)).unwrap_or(false)
    }
    fn adjacency(&self, i: usize) -> Option<Rc<crate::query_file::Adjacency>> {
        if let Some(found) = self.adjacency.borrow().get(&i) {
            return Some(found.clone());
        }
        let decoded = Rc::new(self.ok(self.file.adjacency(i))?);
        self.adjacency.borrow_mut().insert(i, decoded.clone());
        Some(decoded)
    }
    /// The edges between `a` and `b` in both directions, as
    /// `edges_between(a, b)` then `edges_between(b, a)` list them.
    fn both_ways(&self, a: &str, b: &str) -> Option<Vec<crate::query_file::EdgeRecord>> {
        let (a, b) = (self.node_index(a)?, self.node_index(b)?);
        let adjacency = self.adjacency(a)?;
        let mut ids = adjacency.out_to(b).to_vec();
        ids.extend_from_slice(adjacency.in_from(b));
        self.ok(self.file.edges(&ids))
    }
}

impl Episode for Saved {
    fn candidates(&self, question: &str, documents: bool) -> Found {
        use graphicle::serve_search::{CONSTANTS, pick_seeds_on, score_on};
        let file = &*self.file;
        let terms = graphicle::query_terms::query_terms(question);
        let mut scored = score_on(file, &terms, true);
        // Without documents, as `code_candidates` leaves them out.
        if !documents {
            scored.ranked.retain(|(_, i)| !self.is_document(*i));
            scored
                .best_seed_by_term
                .retain(|_, i| !self.is_document(*i));
        }
        let intent = CONSTANTS["_RELATIONAL_INTENT_TERMS"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if terms.iter().any(|t| !intent.contains(&json!(t))) {
            scored
                .best_seed_by_term
                .retain(|t, _| !intent.contains(&json!(t)));
        }
        // `code_candidates` picks its per-term seeds in term order.
        let order: &[String] = if documents { &scored.term_order } else { &[] };
        let seeds = pick_seeds_on(
            file,
            &scored.ranked,
            3,
            0.2,
            &scored.best_seed_by_term,
            order,
        );
        Found {
            terms,
            matched: scored.ranked.len(),
            candidates: scored
                .ranked
                .iter()
                .take(CAP)
                .map(|&(score, i)| (self.id(i), score))
                .collect(),
            seeds: seeds.iter().map(|&i| self.id(i)).collect(),
        }
    }
    fn expand(&self, seeds: &[String], question: &str) -> Result<Expanded> {
        use graphicle::serve_search::text;
        let file = &*self.file;
        let mut indices: Vec<usize> = Vec::new();
        for seed in seeds {
            if let Some(i) = self.node_index(seed)
                && !indices.contains(&i)
            {
                indices.push(i);
            }
        }
        let (filters, _) = graphicle::serve_search::resolve_contexts(question, &[]);
        let kept = |context: &Option<Value>| {
            filters.is_empty() || filters.contains(&context.as_ref().map(text).unwrap_or_default())
        };
        let seedset: BTreeSet<usize> = indices.iter().copied().collect();
        let mut visited = seedset.clone();
        let mut edges = Vec::new();
        for &i in &seedset {
            let Some(adjacency) = self.adjacency(i) else {
                continue;
            };
            for nb in adjacency.neighbours() {
                if seedset.contains(&nb) {
                    continue;
                }
                // `edges_between(low, high)`, then on a directed graph
                // `edges_between(high, low)`, from the seed's own record.
                let (forward, backward) = (adjacency.out_to(nb), adjacency.in_from(nb));
                let (first, second) = if i <= nb {
                    (forward, backward)
                } else {
                    (backward, forward)
                };
                let mut ids = first.to_vec();
                if file.directed {
                    ids.extend_from_slice(second);
                }
                let list = self.ok(file.edges(&ids)).unwrap_or_default();
                // A plain graph's view keeps one merged entry per pair, kept
                // when its last context passes; a multigraph's keeps every arc.
                let surviving = if file.multigraph {
                    list.into_iter()
                        .filter(|edge| kept(&edge.context))
                        .collect()
                } else {
                    let mut context = None;
                    for edge in &list {
                        if let Some(value) = &edge.context {
                            context = Some(text(value));
                        }
                    }
                    if filters.is_empty() || filters.contains(&context.unwrap_or_default()) {
                        list
                    } else {
                        Vec::new()
                    }
                };
                if surviving.is_empty() {
                    continue;
                }
                let mut relation = String::new();
                for edge in &surviving {
                    if let Some(value) = &edge.relation {
                        relation = text(value);
                    }
                }
                visited.insert(nb);
                edges.push((i, nb, relation));
            }
        }
        Ok(Expanded {
            nodes: visited
                .iter()
                .map(|&i| (self.id(i), seedset.contains(&i)))
                .collect(),
            edges: edges
                .into_iter()
                .map(|(u, v, relation)| (self.id(u), self.id(v), relation))
                .collect(),
            filters,
        })
    }
    fn current_edge(
        &self,
        a: &str,
        b: &str,
        filters: &[String],
        unchanged: &BTreeSet<String>,
    ) -> Option<(String, String)> {
        self.both_ways(a, b)?
            .into_iter()
            .filter(|edge| {
                filters.is_empty()
                    || filters.contains(&value_text(edge.context.as_ref()).unwrap_or_default())
            })
            .find(|edge| edge.files.iter().any(|file| unchanged.contains(file)))
            .map(|edge| {
                (
                    value_text(edge.relation.as_ref()).unwrap_or_default(),
                    value_text(edge.derivation.as_ref()).unwrap_or_else(|| "unknown".to_string()),
                )
            })
    }
    fn is_document(&self, id: &str) -> bool {
        self.node_index(id).is_some_and(|i| self.is_document(i))
    }
    fn attributes(&self, id: &str) -> Option<Map<String, Value>> {
        let i = self.node_index(id)?;
        self.ok(self.file.attributes(i))
    }
    fn derivation(&self, a: &str, b: &str) -> String {
        self.both_ways(a, b)
            .and_then(|list| {
                list.into_iter()
                    .find_map(|edge| value_text(edge.derivation.as_ref()))
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
    fn starts(&self, path: &str) -> Option<Vec<usize>> {
        self.ok(self.file.starts(path)).flatten()
    }
}

impl Plan {
    fn empty(episode: String, documents: bool) -> Self {
        Self {
            skipped: None,
            episode,
            lag: None,
            documents,
            terms: Vec::new(),
            matched: 0,
            candidates: 0,
            seeds: Vec::new(),
            expanded: 0,
            nodes: Vec::new(),
            starts: HashMap::new(),
        }
    }
}

/// [`plan`] on the episode `source` reads.
fn plan_on(
    source: &dyn Episode,
    episode: String,
    lag: Option<Lag>,
    question: &str,
    documents: bool,
) -> Result<Plan> {
    let mut plan = Plan::empty(episode.clone(), documents);
    let candidates =
        crate::phase::phase("arm_candidates", || source.candidates(question, documents));
    plan.terms = candidates.terms.clone();
    if candidates.candidates.is_empty() {
        let mut skipped = json!({"status":"no_match"});
        skipped["episode"] = json!(episode);
        skipped["episode_lag"] = json!(0);
        skipped["terms"] = json!(candidates.terms);
        plan.skipped = Some(skipped);
        plan.lag = lag;
        return Ok(plan);
    }
    let expansion =
        crate::phase::phase("arm_expand", || source.expand(&candidates.seeds, question))?;
    // Neighbour → the seed that reached it, the relation, and under the lag
    // fallback the derivation of the current edge it was reached through
    // (first edge wins; on the current episode the derivation is read when
    // the neighbour is cited). Under the lag fallback a neighbour that only
    // edges read from changed or removed files reach is left out.
    let mut reached_via: HashMap<&str, (&str, String, Option<String>)> = HashMap::new();
    for (source_id, target_id, relation) in &expansion.edges {
        let seed_side = |id: &str| candidates.seeds.iter().any(|seed| seed == id);
        let (from, to) = if seed_side(source_id) && !seed_side(target_id) {
            (source_id.as_str(), target_id.as_str())
        } else if seed_side(target_id) && !seed_side(source_id) {
            (target_id.as_str(), source_id.as_str())
        } else {
            continue;
        };
        if reached_via.contains_key(to) {
            continue;
        }
        match &lag {
            None => {
                reached_via.insert(to, (from, relation.clone(), None));
            }
            Some(lag) => {
                if let Some((relation, derivation)) =
                    source.current_edge(from, to, &expansion.filters, &lag.unchanged)
                {
                    reached_via.insert(to, (from, relation, Some(derivation)));
                }
            }
        }
    }
    // Node ids in arm order, deduplicated, capped.
    let mut ordered: Vec<(String, u8, Option<f64>)> = Vec::new();
    let mut seen = BTreeSet::new();
    for (node_id, score) in &candidates.candidates {
        if seen.insert(node_id.clone()) {
            ordered.push((node_id.clone(), 0, Some(*score)));
        }
    }
    for (node_id, seed) in &expansion.nodes {
        if ordered.len() >= CAP {
            break;
        }
        // Without documents, a page or section reached through its
        // `references` edge is no neighbour either.
        if !documents && source.is_document(node_id) {
            continue;
        }
        if !seed && seen.insert(node_id.clone()) {
            ordered.push((node_id.clone(), 1, None));
        }
    }
    ordered.truncate(CAP);
    // Symbol starts per file: a node's range runs to the next symbol start in
    // the same file, the bytes its `revision` hashes.
    for (node_id, hop, score) in ordered {
        let attributes = source.attributes(&node_id);
        let via = reached_via
            .get(node_id.as_str())
            .map(|(from, relation, derivation)| {
                (from.to_string(), relation.clone(), derivation.clone())
            });
        let derivation = match &via {
            Some((from, _, None)) if attributes.is_some() => {
                Some(source.derivation(from, &node_id))
            }
            _ => None,
        };
        if let Some(path) = attributes.as_ref().and_then(relpath)
            && !plan.starts.contains_key(&path)
            && let Some(lines) = source.starts(&path)
        {
            plan.starts.insert(path, lines);
        }
        plan.nodes.push(Planned {
            node_id,
            hop,
            score,
            attributes,
            via,
            derivation,
        });
    }
    plan.lag = lag;
    plan.matched = candidates.matched;
    plan.candidates = candidates.candidates.len();
    plan.seeds = candidates.seeds;
    plan.expanded = expansion.nodes.len();
    Ok(plan)
}

/// Whether a `basic`
/// answer reads the code graph and reviewed memory by default. It does when
/// the episode the arm reads ([`reading`]) holds at least one code symbol: a
/// node in a file whose kind is not a file, document or concept node
/// ([`code_symbol`]). A conversation-only project's episode holds none, so
/// its answers stay source-only, as before. A
/// second trigger, a current reviewed memory that carries symbol links,
/// is read by `basic` from its snapshot (`symbols::current_links`,
/// reason `memory_symbol_links`).
///
/// Returns `{"integrated": bool, "reason": "code_symbols" |
/// "no_code_symbols" | "no_episode", "episode": <read> | null}`.
pub fn integration_default(db: &Connection, repository: &str) -> Result<Value> {
    let heads = crate::store::heads(db, repository)?;
    let current = graph_projection::episode_of(&heads);
    let Some(Reading { episode, .. }) = reading(db, repository, &heads, &current)? else {
        return Ok(json!({"integrated": false, "reason": "no_episode", "episode": null}));
    };
    let handle = graph_projection::resident(db, repository, &episode)?;
    let graph: &Graph = &handle;
    let integrated = graph
        .nodes()
        .iter()
        .any(|node| code_symbol(&node.attributes) && !yaml_node(&node.attributes));
    Ok(json!({"integrated": integrated,
        "reason": if integrated { "code_symbols" } else { "no_code_symbols" },
        "episode": episode}))
}

/// A node of a YAML file. The engine reads YAML configuration as keys since
/// Graphify 0.9.95 (#2214); the code graph leaves them out for now
/// (`graph_projection`'s `YAML_IN_CODE_GRAPH`), and where it has them a
/// project whose only symbols are YAML keys is not a code project for
/// [`integration_default`].
fn yaml_node(attributes: &impl graphicle::record::Properties) -> bool {
    relpath(attributes).is_some_and(|path| {
        std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
    })
}

/// A code symbol of an episode: a node anchored to a file (`relpath`) that
/// is not the file itself, a document section, a paper, an image, a
/// rationale comment or a concept (externals). Identity writes `kind`: `file`
/// for a file node, else the engine's `file_type`; a node recorded before
/// identity has only `file_type`, and the engine names a file node after its
/// file.
pub fn code_symbol(attributes: &impl graphicle::record::Properties) -> bool {
    let Some(path) = relpath(attributes) else {
        return false;
    };
    match attr(attributes, "kind").as_deref() {
        Some(kind) => !matches!(
            kind,
            "file" | "document" | "paper" | "image" | "video" | "rationale" | "concept"
        ),
        None => {
            attr(attributes, "file_type").as_deref() == Some("code")
                && attr(attributes, "label").as_deref() != path.rsplit('/').next()
        }
    }
}

/// Map anchored hits onto the candidate id space of `rank`. Returns the arm
/// ranking, the facts per candidate identity, and synthetic hits for anchors
/// no chunk covers (a passage-limited document) so `pack` can still cite them.
pub fn fuse(
    arm: &Arm,
    rows: &[Value],
    documents: &[Value],
) -> (Vec<String>, Map<String, Value>, Vec<Value>) {
    let mut chunks: HashMap<&str, Vec<(usize, usize, &str)>> = HashMap::new();
    for row in rows.iter().filter(|r| r["kind"] == "source") {
        let source = &row["source"];
        if let (Some(a), Some(b)) = (source["byte_start"].as_u64(), source["byte_end"].as_u64()) {
            chunks.entry(s(source, "id")).or_default().push((
                a as usize,
                b as usize,
                s(row, "identity"),
            ));
        }
    }
    for list in chunks.values_mut() {
        list.sort_unstable();
    }
    let docs: HashMap<&str, &Value> = documents.iter().map(|d| (s(d, "id"), d)).collect();
    let mut ranking = Vec::new();
    let mut facts = Map::new();
    let mut unchunked = Vec::new();
    let mut seen = BTreeSet::new();
    for hit in &arm.hits {
        if ranking.len() >= CAP {
            break;
        }
        let covering: Vec<&str> = chunks
            .get(hit.document.as_str())
            .into_iter()
            .flatten()
            .filter(|(a, b, _)| *a < hit.byte_end && *b > hit.byte_start)
            .map(|(_, _, identity)| *identity)
            .collect();
        if covering.is_empty() {
            let Some(document) = docs.get(hit.document.as_str()) else {
                continue;
            };
            let content = s(document, "content");
            let Some(text) = content.get(hit.byte_start..hit.byte_end) else {
                continue;
            };
            let identity = format!("{}:{}#{ARM}", hit.document, hit.byte_start);
            if text.trim().is_empty()
                || unchunked.len() >= UNCHUNKED_LIMIT
                || !seen.insert(identity.clone())
            {
                continue;
            }
            let mut source = document.as_object().cloned().unwrap_or_default();
            source.remove("content");
            source.insert("byte_start".into(), json!(hit.byte_start));
            source.insert("byte_end".into(), json!(hit.byte_end));
            source.insert("line".into(), hit.facts["line"].clone());
            source.insert("end_line".into(), hit.facts["end_line"].clone());
            source.insert("total_bytes".into(), json!(content.len()));
            source.insert(
                "complete".into(),
                json!(hit.byte_start == 0 && hit.byte_end == content.len()),
            );
            source.insert("truncated".into(), json!(hit.byte_end < content.len()));
            source.insert(ARM.into(), hit.facts.clone());
            unchunked.push(
                json!({"identity": identity, "kind": "source", "origin": hit.document,
                "title": document["title"], "text": text, "source_sha": document["sha256"],
                "source": source}),
            );
            continue;
        }
        for identity in covering {
            if seen.insert(identity.to_string()) {
                ranking.push(identity.to_string());
            }
            match facts.get_mut(identity) {
                Some(existing) => {
                    let count = existing["nodes"].as_u64().unwrap_or(1) + 1;
                    existing["nodes"] = json!(count);
                }
                None => {
                    let mut record = hit.facts.clone();
                    record["nodes"] = json!(1);
                    facts.insert(identity.to_string(), record);
                }
            }
        }
    }
    ranking.truncate(CAP);
    (ranking, facts, unchunked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_locations() {
        let mut attributes = Map::new();
        attributes.insert("source_location".into(), json!("L31"));
        assert_eq!(lines(&attributes), Some((31, None)));
        attributes.insert("source_location".into(), json!("L3-L9"));
        assert_eq!(lines(&attributes), Some((3, Some(9))));
        attributes.insert("source_location".into(), json!("L3-9"));
        assert_eq!(lines(&attributes), Some((3, Some(9))));
        attributes.insert("source_location".into(), Value::Null);
        assert_eq!(lines(&attributes), None);
    }

    /// YAML keys are code symbols of the graph, but a project with only YAML
    /// is not a code project for `basic`'s default.
    #[test]
    fn yaml_keys_alone_do_not_make_a_code_project() {
        let node = |fields: Value| fields.as_object().cloned().unwrap();
        let key = node(json!({"kind":"code","relpath":"deploy/app.yaml","label":"replicas"}));
        assert!(code_symbol(&key));
        assert!(yaml_node(&key));
        assert!(yaml_node(&node(
            json!({"file_type":"code","source_file":".github/workflows/ci.YML","label":"jobs"})
        )));
        assert!(!yaml_node(&node(
            json!({"kind":"code","relpath":"a.py","label":"alpha()"})
        )));
        assert!(!yaml_node(&node(json!({"kind":"concept","label":"yaml"}))));
    }

    /// What makes an episode a code
    /// project's for `basic`'s default.
    #[test]
    fn code_symbols_are_file_anchored_nodes_other_than_files_and_documents() {
        let node = |fields: Value| fields.as_object().cloned().unwrap();
        for (fields, symbol) in [
            (
                json!({"kind":"code","relpath":"a.py","label":"alpha()"}),
                true,
            ),
            (
                json!({"kind":"file","relpath":"a.py","label":"a.py"}),
                false,
            ),
            (
                json!({"kind":"document","relpath":"notes.md","label":"Notes"}),
                false,
            ),
            (
                json!({"kind":"rationale","relpath":"a.py","label":"NOTE"}),
                false,
            ),
            (json!({"kind":"concept","label":"requests"}), false),
            (json!({"kind":"code","label":"requests"}), false),
            // Recorded before identity: `file_type` only, and the engine names
            // a file node after its file.
            (
                json!({"file_type":"code","source_file":"pkg/a.py","label":"alpha()"}),
                true,
            ),
            (
                json!({"file_type":"code","source_file":"pkg/a.py","label":"a.py"}),
                false,
            ),
            (
                json!({"file_type":"document","source_file":"notes.md","label":"Notes"}),
                false,
            ),
        ] {
            assert_eq!(code_symbol(&node(fields.clone())), symbol, "{fields}");
        }
    }

    /// The file an edge was read from, which the lag
    /// fallback checks before it follows the edge.
    #[test]
    fn an_edge_is_read_from_its_provenance_files_else_its_source_file() {
        let edge = |fields: Value| fields.as_object().cloned().unwrap();
        assert_eq!(
            edge_files(&edge(json!({"source_file":"/abs/ignored.py",
                "provenance":[{"source_file":"pkg\\a.py"},{"source_file":"b.py"},{"source_file":""}]}))),
            ["pkg/a.py", "b.py"]
        );
        assert_eq!(edge_files(&edge(json!({"source_file":"c.py"}))), ["c.py"]);
        assert_eq!(
            edge_files(&edge(json!({"provenance":[],"source_file":"c.py"}))),
            ["c.py"]
        );
        assert!(edge_files(&edge(json!({"relation":"calls"}))).is_empty());
    }

    #[test]
    fn line_offsets_are_one_based_starts() {
        assert_eq!(span::line_starts("a\nbb\n"), vec![0, 2, 5]);
        assert_eq!(span::line_starts(""), vec![0]);
    }

    #[test]
    fn fuse_maps_ranges_to_chunks_and_synthesizes_uncovered() {
        let doc = json!({"id":"file:a.py","path":"a.py","title":"a.py","sha256":"x","content":"def a():\n    pass\n\ndef b():\n    pass\n","source_kind":"repository_file"});
        let rows = vec![
            json!({"identity":"file:a.py:0","kind":"source","source":{"id":"file:a.py","byte_start":0,"byte_end":18}}),
        ];
        let arm = Arm {
            hits: vec![
                Hit {
                    document: "file:a.py".into(),
                    byte_start: 0,
                    byte_end: 18,
                    facts: json!({"stable_id":"s1","line":1,"end_line":2}),
                },
                Hit {
                    document: "file:a.py".into(),
                    byte_start: 19,
                    byte_end: 37,
                    facts: json!({"stable_id":"s2","line":4,"end_line":5}),
                },
            ],
            diagnostics: json!({}),
        };
        let (ranking, facts, unchunked) = fuse(&arm, &rows, &[doc]);
        assert_eq!(ranking, vec!["file:a.py:0".to_string()]);
        assert_eq!(facts["file:a.py:0"]["stable_id"], "s1");
        assert_eq!(unchunked.len(), 1);
        assert_eq!(unchunked[0]["source"]["code_graph"]["stable_id"], "s2");
        assert_eq!(unchunked[0]["source"]["byte_start"], 19);
        assert_eq!(unchunked[0]["text"], "def b():\n    pass\n");
    }

    /// The query file plans exactly as the resident graph does: the same
    /// candidates, seeds, neighbours, attributes, derivations and start
    /// lines, with and without documents and the lag fallback.
    #[test]
    fn the_query_file_plans_as_the_resident_graph() {
        for directed in [true, false] {
            plans_alike(crate::query_file::tests::sample_of(directed));
        }
    }

    fn plans_alike(graph: Graph) {
        use graphicle::serve_search::SearchIndex;
        let dir = tempfile::TempDir::new().unwrap();
        let file = std::sync::Arc::new(crate::query_file::tests::written(&graph, dir.path()));
        let index = std::sync::Arc::new(SearchIndex::new(&graph));
        let resident = Resident {
            handle: graphicle::session::GraphHandle::new(graph),
            index,
            starts: std::sync::OnceLock::new(),
        };
        let saved = Saved::new(file);
        let describe = |plan: &Plan| {
            let mut starts: Vec<_> = plan.starts.iter().collect();
            starts.sort();
            format!(
                "{:?} {:?} {} {} {:?} {} {:?} {:?} {:?}",
                plan.skipped,
                plan.terms,
                plan.matched,
                plan.candidates,
                plan.seeds,
                plan.expanded,
                plan.nodes,
                starts,
                plan.lag.as_ref().map(|lag| (&lag.current, lag.changed))
            )
        };
        let lags = [
            None,
            Some(Lag {
                current: "next".into(),
                changed: 2,
                unchanged: BTreeSet::from(["src/query.rs".to_string()]),
            }),
            Some(Lag {
                current: "next".into(),
                changed: 1,
                unchanged: BTreeSet::from(["src/query.rs".to_string(), "src/store.rs".to_string()]),
            }),
        ];
        for question in [
            "Where does Store.open run?",
            "Which code calls store open and close in the tests?",
            "run_query plan helper_12",
            "What does the store guide say about the write ahead log?",
            "Ünïcode störe",
            "helper",
            "nothing matches zzqqxx",
            "",
        ] {
            for documents in [true, false] {
                for lag in &lags {
                    let a =
                        plan_on(&resident, "e".into(), lag.clone(), question, documents).unwrap();
                    let b = plan_on(&saved, "e".into(), lag.clone(), question, documents).unwrap();
                    assert_eq!(
                        describe(&a),
                        describe(&b),
                        "{question:?} documents {documents}"
                    );
                }
            }
        }
        assert!(!saved.corrupt());
    }
}
