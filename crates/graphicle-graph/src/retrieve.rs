//! Opt-in agent retrieval: ranked, cited source evidence within a token budget.
//!
//! `query` answers with a breadth-first dump of node labels. Agents need the
//! code itself, ranked, with a citation for every piece and a hard budget.
//! `retrieve` (CLI) and the `retrieve_context` MCP tool (with `serve
//! --retrieval` or `GRAPHICLE_RETRIEVAL=1`) do that:
//!
//! 1. Lexical seeds: Graphify's own query scoring (`score_query`).
//! 2. One hop of spreading activation from the best seeds over the graph,
//!    weighted by relation (calls > references/inherits > contains > imports)
//!    and edge confidence, so the code that implements or uses a matched
//!    symbol ranks next to it.
//! 3. Files rank by the BM25 of the whole file plus half the score of their
//!    best definitions (both divided by the best file's). Test files rank at
//!    0.3 of their score unless tests are asked for (`--include-tests`):
//!    they repeat the names of the code under test and would outrank it.
//!    Each file contributes its best definitions as source snippets (the
//!    definition's lines, up to the next definition), cited as
//!    `path:Lstart-Lend`, each with why it was chosen.
//! 4. Items are packed in rank order until the token budget (characters / 4,
//!    as elsewhere in the engine) is used.
//!
//! Nothing here changes the graph or any default output.
use crate::{
    Result,
    graph::Graph,
    serve_search::{SearchIndex, label, score_query},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// `serve --retrieval`: list and answer the `retrieve_context` tool.
pub static ENABLED: AtomicBool = AtomicBool::new(false);
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed) || std::env::var("GRAPHICLE_RETRIEVAL").is_ok_and(|v| v == "1")
}

const SEEDS: usize = 40;
const HOP: f64 = 0.35;
const NODES_PER_FILE: usize = 2;
/// Share of a node's score from its name (Graphify's query scoring); the
/// rest is the BM25 of its source body.
const NAME_WEIGHT: f64 = 0.35;
/// Added when the question names the symbol outright.
const EXACT_BONUS: f64 = 1.0;
/// A file ranks by the BM25 of the whole file (divided by the best file's)
/// plus this share of its best definitions' score (divided by the best
/// file's). Tuned on 76 SWE-bench Lite instances with
/// `bench/retrieval/tune_ranking.py`, then checked unchanged on 160 others.
const DEFS_WEIGHT: f64 = 0.5;
/// Test files rank at this share of their score unless the caller asks for
/// tests: they repeat the names of the code under test, so they outrank it.
const TEST_WEIGHT: f64 = 0.3;
/// A file's definitions score: its best one plus these shares of the next.
const FILE_DECAY: [f64; 3] = [1.0, 0.5, 0.25];
/// Most files in one answer (with code, then as citations).
const MAX_FILES: usize = 20;

/// (node, score, why it was chosen).
pub type Ranked = (usize, f64, String);

pub struct Options {
    /// Budget for cited evidence, estimated as characters / 4. The response
    /// heading and JSON envelope are separate from this evidence budget.
    pub budget: usize,
    /// Most lines in one snippet.
    pub snippet_lines: usize,
    /// Scan root that `source_file` paths are relative to.
    pub root: Option<PathBuf>,
    /// Rank test files like any other file (by default they rank lower).
    pub include_tests: bool,
}

#[derive(Debug, Clone)]
pub struct Evidence {
    pub node_id: String,
    pub label: String,
    pub file: String,
    pub start: usize,
    pub end: usize,
    pub score: f64,
    pub why: String,
    pub snippet: Option<String>,
}

fn attr<'a>(g: &'a Graph, i: usize, key: &str) -> Option<&'a str> {
    g.nodes[i].attributes.get(key).and_then(Value::as_str)
}

fn line_of(g: &Graph, i: usize) -> Option<usize> {
    attr(g, i, "source_location")?
        .strip_prefix('L')?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn relation_weight(relation: &str) -> f64 {
    match relation {
        "calls" | "indirect_call" => 1.0,
        "references" | "inherits" | "extends" | "implements" | "method" => 0.6,
        "contains" | "rationale_for" => 0.4,
        "imports" | "imports_from" | "re_exports" => 0.3,
        _ => 0.25,
    }
}

/// The scan root for a graph: `.graphicle_root` next to it, else the parent
/// of its `.graphicle` directory.
pub fn root_for(graph: &Path) -> Option<PathBuf> {
    let dir = graph.parent()?;
    if let Ok(text) = fs::read_to_string(dir.join(".graphicle_root")) {
        let p = PathBuf::from(text.trim());
        if p.is_dir() {
            return Some(p);
        }
    }
    dir.parent().map(Path::to_path_buf)
}

/// Ranked evidence for `question`, best first (not yet budgeted), and file
/// scores. With a scan root, every definition's source body and every whole
/// file are also scored (BM25), so words that only appear in code (error
/// messages, called names) count too.
pub fn rank(
    g: &Graph,
    index: &SearchIndex,
    question: &str,
    root: Option<&Path>,
) -> (Vec<Ranked>, HashMap<String, f64>) {
    let terms = crate::query_terms::query_terms(question);
    let scored = score_query(g, index, &terms, true);
    let mut ranked = scored.ranked.clone();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let top = ranked.first().map_or(1.0, |r| r.0).max(f64::MIN_POSITIVE);
    let mut base: HashMap<usize, f64> = HashMap::new();
    let mut why: HashMap<usize, String> = HashMap::new();
    let (body, files) = root
        .map(|root| body_scores(g, root, question))
        .unwrap_or_default();
    // Without code text, names are all there is.
    let name_weight = if body.is_empty() { 1.0 } else { NAME_WEIGHT };
    for (s, i) in ranked.iter().take(SEEDS * 4) {
        // Graphify's exact and prefix name tiers (>= 100x a substring hit)
        // mean the question names this symbol: it leads.
        let exact = if *s >= 0.1 * top { EXACT_BONUS } else { 0.0 };
        base.insert(*i, name_weight * s / top + exact);
        why.insert(*i, "name matches".into());
    }
    let top = body
        .values()
        .copied()
        .fold(0.0, f64::max)
        .max(f64::MIN_POSITIVE);
    for (i, s) in body {
        let e = base.entry(i).or_insert(0.0);
        *e += (1.0 - name_weight) * s / top;
        why.entry(i).or_insert_with(|| "code matches".into());
    }
    let mut seeds = base.iter().map(|(i, s)| (*s, *i)).collect::<Vec<_>>();
    seeds.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut score = base.clone();
    for (seed, i) in seeds.iter().take(SEEDS) {
        let (seed, i) = (*seed, *i);
        for (u, v, e) in g.out_edges(i).into_iter().chain(g.in_edges(i)) {
            let other = if u == i { v } else { u };
            if other == i {
                continue;
            }
            let relation = e
                .attributes
                .get("relation")
                .and_then(Value::as_str)
                .unwrap_or("");
            let confidence = e
                .attributes
                .get("confidence_score")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let add = HOP * seed * relation_weight(relation) * confidence;
            let entry = score.entry(other).or_insert(0.0);
            if *entry < add {
                let verb = if u == i {
                    relation.to_string()
                } else {
                    format!("{relation} (reverse)")
                };
                why.entry(other)
                    .or_insert_with(|| format!("{verb} {}", label(g, i)));
            }
            *entry += add;
        }
    }
    let mut out = score
        .into_iter()
        .filter(|(i, _)| attr(g, *i, "source_file").is_some_and(|f| !f.is_empty()))
        .map(|(i, s)| (i, s, why.remove(&i).unwrap_or_default()))
        .collect::<Vec<_>>();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    (out, files)
}

/// Lowercase word and identifier-part tokens: `getUserName` and
/// `get_user_name` both give `get`, `user`, `name` (and the whole word).
fn tokens(text: &str, out: &mut Vec<String>) {
    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if word.len() < 2 || word.len() > 64 {
            continue;
        }
        let lower = word.to_lowercase();
        let mut parts = vec![];
        let mut cur = String::new();
        let mut prev_lower = false;
        for c in word.chars() {
            if c == '_' || (c.is_uppercase() && prev_lower) {
                if cur.len() > 1 {
                    parts.push(cur.to_lowercase());
                }
                cur.clear();
            }
            if c != '_' {
                cur.push(c);
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
        }
        if cur.len() > 1 {
            parts.push(cur.to_lowercase());
        }
        if parts.len() > 1 {
            out.extend(parts);
        }
        out.push(lower);
    }
}

const STOP: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "from", "are", "was", "not", "but", "have", "has",
    "can", "will", "when", "what", "how", "which", "should", "would", "there", "their", "been",
    "into", "than", "then", "also", "some", "such", "any", "all", "use", "used", "using", "does",
    "self", "return", "def", "none", "true", "false", "import", "if", "else", "in", "is", "it",
    "of", "to", "a", "an", "be", "or", "on", "as", "at", "by", "we", "you", "do", "no", "so",
];

/// BM25 of each definition's source body (its lines up to the next
/// definition in the file, at most 80) and of each whole file against the
/// question: (definition scores, file scores).
fn body_scores(
    g: &Graph,
    root: &Path,
    question: &str,
) -> (HashMap<usize, f64>, HashMap<String, f64>) {
    let mut q = vec![];
    tokens(question, &mut q);
    let mut query: HashMap<String, usize> = HashMap::new();
    for t in q.into_iter().filter(|t| !STOP.contains(&t.as_str())) {
        *query.entry(t).or_default() += 1;
    }
    if query.is_empty() {
        return Default::default();
    }
    let mut by_file: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
    for i in 0..g.nodes.len() {
        if let (Some(f), Some(l)) = (attr(g, i, "source_file"), line_of(g, i)) {
            by_file.entry(f).or_default().push((l, i));
        }
    }
    let mut docs: Vec<(usize, HashMap<String, u32>, usize)> = vec![];
    let mut files: Vec<(String, HashMap<String, u32>, usize)> = vec![];
    let keep = |toks: Vec<String>| {
        let len = toks.len();
        let mut tf: HashMap<String, u32> = HashMap::new();
        for t in toks {
            if query.contains_key(&t) {
                *tf.entry(t).or_default() += 1;
            }
        }
        (tf, len)
    };
    for (file, mut defs) in by_file {
        let Some(lines) = read_source(root, file) else {
            continue;
        };
        // The file's path words belong to each of its definitions.
        let mut path_tokens = vec![];
        tokens(&file.replace(['/', '.'], " "), &mut path_tokens);
        let mut whole = path_tokens.clone();
        for l in &lines {
            tokens(l, &mut whole);
        }
        let (tf, len) = keep(whole);
        files.push((file.to_string(), tf, len));
        defs.sort_unstable();
        for (k, (start, i)) in defs.iter().enumerate() {
            let next = defs[k + 1..]
                .iter()
                .find(|(l, _)| l > start)
                .map(|(l, _)| *l);
            let end = next
                .map_or(lines.len(), |n| n - 1)
                .min(start.saturating_add(79))
                .min(lines.len());
            if *start == 0 || *start > end {
                continue;
            }
            let mut toks = path_tokens.clone();
            for l in &lines[start - 1..end] {
                tokens(l, &mut toks);
            }
            let (tf, len) = keep(toks);
            docs.push((*i, tf, len));
        }
    }
    let defs = bm25(&docs, &query).into_iter().collect();
    let files = bm25(&files, &query).into_iter().collect();
    (defs, files)
}

fn bm25<K: Clone>(
    docs: &[(K, HashMap<String, u32>, usize)],
    query: &HashMap<String, usize>,
) -> Vec<(K, f64)> {
    let mut df: HashMap<&str, usize> = HashMap::new();
    for (_, tf, _) in docs {
        for t in tf.keys() {
            if let Some((k, _)) = query.get_key_value(t) {
                *df.entry(k.as_str()).or_default() += 1;
            }
        }
    }
    let n = docs.len().max(1) as f64;
    let avg = docs.iter().map(|d| d.2).sum::<usize>() as f64 / n;
    let (k1, b) = (1.2, 0.75);
    let mut out = vec![];
    for (key, tf, len) in docs {
        // Sum in term order: floating-point addition is not associative, and a
        // map's iteration order changes from run to run, so equal documents
        // could otherwise score one ulp apart and swap places.
        let mut terms = tf.iter().collect::<Vec<_>>();
        terms.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut s = 0.0;
        for (t, f) in terms {
            let d = *df.get(t.as_str()).unwrap_or(&0) as f64;
            let idf = ((n - d + 0.5) / (d + 0.5) + 1.0).ln();
            let f = f64::from(*f);
            s += idf * f * (k1 + 1.0) / (f + k1 * (1.0 - b + b * *len as f64 / avg.max(1.0)));
        }
        if s > 0.0 {
            out.push((key.clone(), s));
        }
    }
    out
}

/// One candidate file: its rank score and the parts it came from.
#[derive(Debug, Clone)]
pub struct FileRank<'g> {
    pub file: &'g str,
    pub score: f64,
    /// BM25 of the whole file, divided by the best file's.
    pub whole: f64,
    /// The file's best definitions (FILE_DECAY shares), before combining.
    pub defs: f64,
    /// A test file (`paths::is_test_path`).
    pub test: bool,
    /// Its ranked definitions, best first.
    pub nodes: Vec<Ranked>,
}

/// Candidate files for `question`, best first.
pub fn rank_files<'g>(
    g: &'g Graph,
    index: &SearchIndex,
    question: &str,
    root: Option<&Path>,
    include_tests: bool,
) -> Vec<FileRank<'g>> {
    let (ranked, file_scores) = rank(g, index, question, root);
    let mut per_file: HashMap<&'g str, Vec<Ranked>> = HashMap::new();
    for (i, s, why) in ranked {
        let f = attr(g, i, "source_file").unwrap_or("");
        per_file.entry(f).or_default().push((i, s, why));
    }
    let file_top = file_scores
        .values()
        .copied()
        .fold(0.0, f64::max)
        .max(f64::MIN_POSITIVE);
    let mut files: Vec<FileRank<'g>> = per_file
        .into_iter()
        .map(|(file, nodes)| {
            let defs = nodes
                .iter()
                .zip(FILE_DECAY)
                .map(|(n, w)| n.1 * w)
                .sum::<f64>();
            let whole = file_scores.get(file).copied().unwrap_or(0.0) / file_top;
            FileRank {
                file,
                score: 0.0,
                whole,
                defs,
                test: crate::paths::is_test_path(file),
                nodes,
            }
        })
        .collect();
    let defs_top = files
        .iter()
        .map(|f| f.defs)
        .fold(0.0, f64::max)
        .max(f64::MIN_POSITIVE);
    for f in &mut files {
        let defs = f.defs / defs_top;
        // Without source text (no root) the definitions are all there is.
        let score = if file_scores.is_empty() {
            defs
        } else {
            f.whole + DEFS_WEIGHT * defs
        };
        f.score = if f.test && !include_tests {
            score * TEST_WEIGHT
        } else {
            score
        };
    }
    files.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.file.cmp(b.file)));
    files
}

/// Evidence items packed into `options.budget` tokens.
pub fn retrieve(
    g: &Graph,
    index: &SearchIndex,
    question: &str,
    options: &Options,
) -> Vec<Evidence> {
    retrieve_ranked(g, index, question, options).0
}

/// [`retrieve`] plus the file ranking it packed from (for `--explain`).
pub fn retrieve_ranked<'g>(
    g: &'g Graph,
    index: &SearchIndex,
    question: &str,
    options: &Options,
) -> (Vec<Evidence>, Vec<FileRank<'g>>) {
    let ranking = rank_files(
        g,
        index,
        question,
        options.root.as_deref(),
        options.include_tests,
    );
    // Definition start lines per file, to end each snippet at the next one.
    let mut starts: HashMap<&str, Vec<usize>> = HashMap::new();
    for i in 0..g.nodes.len() {
        if let (Some(f), Some(l)) = (attr(g, i, "source_file"), line_of(g, i)) {
            starts.entry(f).or_default().push(l);
        }
    }
    for v in starts.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    // Each file contributes up to NODES_PER_FILE definitions.
    let files = ranking
        .iter()
        .map(|r| {
            let mut nodes = r.nodes.clone();
            nodes.truncate(NODES_PER_FILE);
            (r.file, nodes)
        })
        .collect::<Vec<_>>();
    let mut out = vec![];
    let mut used = 0usize;
    let mut citations_only = false;
    let mut sources: HashMap<&str, Option<Vec<String>>> = HashMap::new();
    'files: for (file, nodes) in files.into_iter().take(MAX_FILES) {
        for (i, s, why) in nodes {
            let start = line_of(g, i).unwrap_or(1).max(1);
            let next = starts
                .get(file)
                .and_then(|v| v.iter().find(|l| **l > start).copied());
            let last = start.saturating_add(options.snippet_lines.max(1) - 1);
            let mut end = next.map_or(last, |n| n - 1).clamp(start, last);
            let snippet = if citations_only {
                None
            } else {
                let lines = sources.entry(file).or_insert_with(|| {
                    let root = options.root.as_deref()?;
                    read_source(root, file)
                });
                lines.as_ref().and_then(|l| {
                    if start > l.len() {
                        return None;
                    }
                    let hi = end.min(l.len());
                    end = hi;
                    Some(l[start - 1..hi].join("\n"))
                })
            };
            let mut item = Evidence {
                node_id: crate::serve_search::id(g, i),
                label: label(g, i),
                file: file.to_string(),
                start,
                end,
                score: (s * 1000.0).round() / 1000.0,
                why,
                snippet,
            };
            let remaining = options.budget.saturating_mul(4).saturating_sub(used);
            let mut cost = render_item(&item).chars().count();
            if cost > remaining && !citations_only {
                if out.is_empty() {
                    // Citation and fence overhead use the same budget as the
                    // snippet. A budget too small for the citation yields no
                    // evidence instead of silently exceeding the limit.
                    if let Some(mut code) = item.snippet.take() {
                        item.snippet = Some(String::new());
                        let overhead = render_item(&item).chars().count();
                        let keep = remaining.saturating_sub(overhead);
                        let cut = code.char_indices().nth(keep).map_or(code.len(), |(b, _)| b);
                        code.truncate(cut);
                        item.end = item.start.saturating_add(code.lines().count().max(1) - 1);
                        item.snippet = (!code.is_empty()).then_some(code);
                    }
                    if render_item(&item).chars().count() <= remaining {
                        out.push(item);
                    }
                    break 'files;
                }
                // Out of room for code: list the remaining files as citations.
                citations_only = true;
                item.snippet = None;
                cost = render_item(&item).chars().count();
            }
            if cost > remaining {
                break 'files;
            }
            used += cost;
            out.push(item);
            if citations_only {
                // One citation per remaining file.
                continue 'files;
            }
        }
    }
    (out, ranking)
}

/// Reads `rel` under `root`, refusing paths that leave it.
fn read_source(root: &Path, rel: &str) -> Option<Vec<String>> {
    let root = root.canonicalize().ok()?;
    let path = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    let path = path.canonicalize().ok()?;
    if !path.starts_with(&root) {
        return None;
    }
    let meta = fs::metadata(&path).ok()?;
    if meta.len() > 8 << 20 {
        return None;
    }
    let text = String::from_utf8_lossy(&fs::read(&path).ok()?).into_owned();
    // Code is quoted verbatim except for control characters (terminal
    // escapes); tabs stay.
    Some(
        text.lines()
            .map(|l| {
                l.chars()
                    .filter(|c| *c == '\t' || (*c > '\x1f' && *c != '\x7f'))
                    .collect()
            })
            .collect(),
    )
}

fn render_item(e: &Evidence) -> String {
    let mut s = format!(
        "## {}:L{}-L{} {} (score {:.3}; {})\n",
        e.file, e.start, e.end, e.label, e.score, e.why
    );
    if let Some(code) = &e.snippet {
        s.push_str("```\n");
        s.push_str(code);
        s.push_str("\n```\n");
    }
    s
}

pub fn render_text(question: &str, items: &[Evidence], budget: usize) -> String {
    if items.is_empty() {
        return "No matching nodes found.".into();
    }
    let shown: String = question.chars().take(120).collect();
    let ellipsis = if shown.len() < question.len() {
        "…"
    } else {
        ""
    };
    let mut out = format!(
        "Evidence for: {shown}{ellipsis} | {} items, budget {budget} tokens\n\n",
        items.len()
    );
    for e in items {
        out.push_str(&render_item(e));
        out.push('\n');
    }
    out.trim_end().to_string()
}

pub fn render_json(question: &str, items: &[Evidence], budget: usize) -> Value {
    json!({
        "question": question,
        "budget": budget,
        "items": items.iter().map(|e| json!({
            "node_id": e.node_id, "label": e.label,
            "citation": format!("{}:L{}-L{}", e.file, e.start, e.end),
            "file": e.file, "start_line": e.start, "end_line": e.end,
            "score": e.score, "why": e.why, "snippet": e.snippet,
        })).collect::<Vec<_>>(),
    })
}

/// The first `limit` ranked files with the parts of their score
/// (`retrieve --json --explain`).
pub fn ranking_json(ranking: &[FileRank], limit: usize) -> Value {
    let round = |x: f64| (x * 1e4).round() / 1e4;
    Value::Array(
        ranking
            .iter()
            .take(limit)
            .map(|r| {
                json!({
                    "file": r.file, "score": round(r.score), "whole": round(r.whole),
                    "defs": round(r.defs), "test": r.test,
                    "best": r.nodes.first().map(|n| round(n.1)),
                })
            })
            .collect(),
    )
}

/// MCP tool description for `tools/list` (opt-in).
pub fn tool() -> Value {
    json!({
        "name": "retrieve_context",
        "description": "Ranked source evidence for a question: matching definitions and the code that calls or uses them, as cited snippets (path:Lstart-Lend) packed into a token budget.",
        "inputSchema": {"type": "object", "properties": {
            "question": {"type": "string", "description": "Natural language question or keywords"},
            "token_budget": {"type": "integer", "default": 2000, "description": "Maximum output tokens"},
            "include_tests": {"type": "boolean", "default": false, "description": "Rank test files like other files (by default they rank lower)"},
            "format": {"type": "string", "enum": ["text", "json"], "default": "text"}
        }, "required": ["question"]}
    })
}

/// `retrieve_context` for the MCP server.
pub fn call(
    g: &Graph,
    index: &SearchIndex,
    args: &Value,
    graph_path: Option<&Path>,
) -> Result<String> {
    let question = args["question"]
        .as_str()
        .ok_or("missing required string: question")?;
    let budget = args["token_budget"]
        .as_u64()
        .unwrap_or(2000)
        .clamp(100, 100_000) as usize;
    let options = Options {
        budget,
        snippet_lines: 20,
        root: graph_path.and_then(root_for),
        include_tests: args["include_tests"] == true,
    };
    let items = retrieve(g, index, question, &options);
    Ok(if args["format"] == "json" {
        render_json(question, &items, budget).to_string()
    } else {
        render_text(question, &items, budget)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ranks_callers_next_to_matches_and_respects_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("auth.py"),
            "def check_password(user, pw):\n    return user.pw == pw\n\n\ndef login(user, pw):\n    if check_password(user, pw):\n        return True\n    return False\n",
        )
        .unwrap();
        std::fs::write(root.join("other.py"), "def unrelated():\n    return 1\n").unwrap();
        let mut g = Graph::new(false, false);
        for (id, label, file, line) in [
            ("auth_check", "check_password()", "auth.py", "L1"),
            ("auth_login", "login()", "auth.py", "L5"),
            ("other_x", "unrelated()", "other.py", "L1"),
        ] {
            g.add_node(
                json!(id),
                json!({"label": label, "source_file": file, "source_location": line})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        }
        g.add_edge(
            json!("auth_login"),
            json!("auth_check"),
            None,
            json!({"relation": "calls", "confidence_score": 1.0})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        let index = SearchIndex::new(&g);
        let options = Options {
            budget: 2000,
            snippet_lines: 20,
            root: Some(root.to_path_buf()),
            include_tests: false,
        };
        let items = retrieve(&g, &index, "check password", &options);
        assert_eq!(items[0].label, "check_password()");
        assert_eq!((items[0].start, items[0].end), (1, 4));
        assert!(
            items[0]
                .snippet
                .as_deref()
                .unwrap()
                .contains("return user.pw == pw")
        );
        assert!(items.iter().any(|e| e.label == "login()"));
        assert!(!items.iter().any(|e| e.label == "unrelated()"));
        let tiny = retrieve(
            &g,
            &index,
            "check password",
            &Options {
                budget: 10,
                root: options.root.clone(),
                ..options
            },
        );
        assert!(tiny.is_empty(), "the citation alone exceeds ten tokens");
        for budget in [0, 30, 40, 100] {
            let packed = retrieve(
                &g,
                &index,
                "check password",
                &Options {
                    budget,
                    snippet_lines: 0,
                    root: options.root.clone(),
                    include_tests: false,
                },
            );
            let characters: usize = packed
                .iter()
                .map(|item| render_item(item).chars().count())
                .sum();
            assert!(
                characters <= budget * 4,
                "{characters} characters for {budget} tokens"
            );
        }
        let text = render_text("check password", &items, 2000);
        assert!(text.contains("auth.py:L1-L4 check_password()"));
        g.nodes[0]
            .attributes
            .insert("source_location".into(), json!(format!("L{}", usize::MAX)));
        let index = SearchIndex::new(&g);
        let items = retrieve(&g, &index, "check password", &options);
        assert!(
            items
                .iter()
                .filter(|item| item.node_id == "auth_check")
                .all(|item| item.snippet.is_none()),
            "an out-of-file location has no source excerpt"
        );
    }

    #[test]
    fn test_files_rank_below_the_code_they_test_unless_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("tests")).unwrap();
        std::fs::write(
            root.join("parser.py"),
            "def parse_header(raw):\n    return raw.split(':')\n",
        )
        .unwrap();
        // The test names the function three times; the code once.
        std::fs::write(
            root.join("tests/test_parser.py"),
            "def test_parse_header():\n    assert parse_header('a:b') == ['a', 'b']\n    parse_header('x')\n    parse_header('y')\n",
        )
        .unwrap();
        let mut g = Graph::new(false, false);
        for (id, label, file) in [
            ("parser_parse_header", "parse_header()", "parser.py"),
            (
                "test_parse_header",
                "test_parse_header()",
                "tests/test_parser.py",
            ),
        ] {
            g.add_node(
                json!(id),
                json!({"label": label, "source_file": file, "source_location": "L1"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        }
        let index = SearchIndex::new(&g);
        let options = Options {
            budget: 2000,
            snippet_lines: 20,
            root: Some(root.to_path_buf()),
            include_tests: false,
        };
        let files = |opts: &Options| {
            rank_files(
                &g,
                &index,
                "parse_header splits wrong",
                opts.root.as_deref(),
                opts.include_tests,
            )
            .into_iter()
            .map(|f| (f.file.to_string(), f.score))
            .collect::<Vec<_>>()
        };
        let demoted = files(&options);
        assert_eq!(demoted[0].0, "parser.py");
        let with_tests = Options {
            include_tests: true,
            ..options
        };
        let full = files(&with_tests);
        let score = |v: &[(String, f64)], f: &str| v.iter().find(|(x, _)| x == f).unwrap().1;
        let (a, b) = (
            score(&demoted, "tests/test_parser.py"),
            score(&full, "tests/test_parser.py"),
        );
        assert!((a - b * TEST_WEIGHT).abs() < 1e-9, "{a} vs {b}");
        assert_eq!(score(&demoted, "parser.py"), score(&full, "parser.py"));
    }

    #[test]
    fn equal_documents_get_bit_identical_scores() {
        // Each map has its own hash seed, so without a fixed summation order
        // equal documents usually differ in the last bits.
        let words = (0..40).map(|i| format!("term{i}")).collect::<Vec<_>>();
        let query = words
            .iter()
            .map(|w| (w.clone(), 1))
            .collect::<HashMap<_, _>>();
        let doc = |key: usize| {
            let tf = words
                .iter()
                .enumerate()
                .map(|(i, w)| (w.clone(), 1 + (i as u32 * 7) % 5))
                .collect::<HashMap<_, _>>();
            (key, tf, 300)
        };
        let docs = (0..50).map(doc).collect::<Vec<_>>();
        let scores = bm25(&docs, &query);
        assert_eq!(scores.len(), 50);
        assert!(
            scores
                .iter()
                .all(|(_, s)| s.to_bits() == scores[0].1.to_bits())
        );
    }
}
