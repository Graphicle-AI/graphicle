//! `graphicle pack`: a project's code for one question, filled to a token
//! budget and laid out for a long-context model.
//!
//! The ranking is `context`'s own: the fused order of the passage index, the
//! identifier arm and the code-graph arm, read whole
//! ([`crate::query::PACK_RANKING`]) instead of as the packet's top excerpts.
//! The saved query file ([`crate::query_file`]) gives the symbol start lines
//! that cut large files into excerpts, the call edges of the call paths and
//! the tests, and the degrees that order the breadth fill. File text comes
//! from the project's synced snapshot, the text the ranking and the code graph
//! read, so every line range in a pack is a range of that text.
//!
//! Sections, filled in this order, each within its share of the budget:
//! architecture (question-independent, cached per code-graph episode), focus
//! (the ranked files: small ones whole, large ones as symbol excerpts with
//! overlapping ranges merged), call paths (callers and callees of the focus
//! symbols, two steps along call edges), tests (from the graph's edges, else
//! by path and name), docs (ranked documentation passages and the nearest
//! README), breadth (the next ranked files, files in the focus folders, then
//! the rest by centrality). A header and a manifest frame them.
//!
//! The cache layout ([`Layout::Cache`]) splits a pack for a provider's
//! prompt cache, which matches the longest identical start of a prompt. Its
//! repository part comes first and depends on nothing but the project, its
//! commit and the settings: a header without the question, the architecture,
//! a map of the folders, the files with the most connections in the code
//! graph (whole, or the first line of each symbol) and the top docs, within
//! the prefix share of the budget. The question part follows: a manifest,
//! focus, call paths and tests, sized like a pack of the room left, which
//! never repeats lines the repository part holds and lists the ranked files
//! it holds instead. The question itself goes after the pack.
//!
//! The budget is a hard limit. Tokens are counted exactly with a Hugging Face
//! `tokenizer.json` (the `tokenizers` crate, pure Rust), else estimated from bytes
//! per token by language ([`estimate`]), with a margin. The text of a pack is
//! the concatenation of its chunks' text, in order, and the same inputs give
//! the same bytes.

use crate::store::{Store, repo, rows, s};
use crate::{Error, Result};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// The JSON format's name.
pub const FORMAT: &str = "graphicle.pack.v1";
/// `--budget` when a call gives none.
pub const DEFAULT_BUDGET: usize = 128_000;
/// The largest budget.
pub const MAX_BUDGET: usize = 2_000_000;
/// The smallest budget: room for the header and a little code.
pub const MIN_BUDGET: usize = 512;
/// The MCP tool's default and largest budgets.
pub const MCP_DEFAULT_BUDGET: usize = 32_000;
pub const MCP_MAX_BUDGET: usize = 200_000;

/// The token estimator's constants, in one place: a calibration against a
/// model's tokenizer changes them here and nowhere else.
pub mod estimate {
    /// Bytes per token by file extension: the Muse Spark Open tokenizer's
    /// own bytes per token over 1.03, measured on kubernetes and
    /// home-assistant (up to 400 files per extension and repository), so
    /// that an estimate runs about 3% above an exact count. Extensions those
    /// repositories hardly hold keep a lower rate (about 1.10 times the
    /// first, conservative one), which runs further above it.
    pub const BYTES_PER_TOKEN: &[(&str, f64)] = &[
        ("c", 3.4),
        ("cc", 3.4),
        ("cpp", 3.4),
        ("cs", 3.7),
        ("css", 3.3),
        ("go", 3.63),
        ("h", 3.5),
        ("hpp", 3.4),
        ("html", 3.3),
        ("java", 3.85),
        ("js", 3.5),
        ("json", 4.1),
        ("jsx", 3.4),
        ("kt", 3.6),
        ("md", 3.8),
        ("php", 3.5),
        ("proto", 4.05),
        ("py", 4.34),
        ("rb", 3.6),
        ("rs", 3.5),
        ("rst", 3.8),
        ("scala", 3.6),
        ("sh", 3.59),
        ("swift", 3.6),
        ("toml", 3.39),
        ("ts", 3.5),
        ("tsx", 3.4),
        ("txt", 2.9),
        ("xml", 3.2),
        ("yaml", 3.64),
        ("yml", 3.64),
    ];
    /// Any other file.
    pub const DEFAULT_BYTES_PER_TOKEN: f64 = 3.0;
    /// The pack's own text: headings, the manifest, the architecture and
    /// the call paths (3.61 bytes per token in Muse Spark Open's count of
    /// 22 kubernetes packs, over 1.03).
    pub const PACK_TEXT_BYTES_PER_TOKEN: f64 = 3.5;
    /// The share of the budget held back when counting by estimate.
    pub const MARGIN: f64 = 0.03;
    /// The share held back with an exact tokenizer (chunks are counted one
    /// by one while filling, the whole pack once at the end).
    pub const EXACT_MARGIN: f64 = 0.002;

    /// Bytes per token for a file with this extension.
    pub fn rate(extension: &str) -> f64 {
        BYTES_PER_TOKEN
            .binary_search_by(|(name, _)| name.cmp(&extension))
            .map_or(DEFAULT_BYTES_PER_TOKEN, |i| BYTES_PER_TOKEN[i].1)
    }

    /// The tokens of a line-number prefix (the number and a tab), in tenths:
    /// 1.6 up to four digits (Muse Spark Open adds 1.3 to 1.8 tokens a line,
    /// 1.5 on average), one more past them. A chunk's prefixes count the sum,
    /// rounded up ([`prefix_tokens`]).
    pub fn prefix_tenths(line: usize) -> usize {
        if line < 10_000 { 16 } else { 26 }
    }

    /// The tokens of the line-number prefixes of lines `from..=to`.
    pub fn prefix_tokens(from: usize, to: usize) -> usize {
        (from..=to).map(prefix_tenths).sum::<usize>().div_ceil(10)
    }

    /// The estimate for `bytes` of text at `rate` bytes per token.
    pub fn tokens(bytes: usize, rate: f64) -> usize {
        (bytes as f64 / rate).ceil() as usize
    }
}

/// How tokens are counted.
pub enum Counter {
    /// Bytes per token by language ([`estimate`]).
    Estimate,
    /// UTF-8 bytes divided by one rate for all text, with no margin
    /// (`--bytes-per-token`): the rule another tool sizes its own text by,
    /// so that both are matched.
    Uniform(f64),
    /// A Hugging Face tokenizer.
    Exact {
        name: String,
        tokenizer: Box<tokenizers::Tokenizer>,
    },
}

impl Counter {
    /// The estimator, one rate of bytes per token, or the tokenizer at
    /// `path`.
    pub fn open(path: Option<&str>, bytes_per_token: Option<f64>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(match bytes_per_token {
                Some(rate) => Self::Uniform(rate),
                None => Self::Estimate,
            });
        };
        if bytes_per_token.is_some() {
            return Err(Error::bad(
                "Choose --tokenizer or --bytes-per-token, not both",
            ));
        }
        let tokenizer = tokenizers::Tokenizer::from_file(path)
            .map_err(|error| Error::bad(format!("Cannot read the tokenizer {path}: {error}")))?;
        Ok(Self::Exact {
            name: tokenizer_name(path),
            tokenizer: Box::new(tokenizer),
        })
    }

    /// `estimate`, or the tokenizer's name.
    pub fn name(&self) -> String {
        match self {
            Self::Estimate | Self::Uniform(_) => "estimate".into(),
            Self::Exact { name, .. } => name.clone(),
        }
    }

    pub fn exact(&self) -> bool {
        !matches!(self, Self::Estimate | Self::Uniform(_))
    }

    /// The tokens of `text`, whose `prefix_bytes` of line-number prefixes
    /// cost `prefix_tokens`, the rest `rate` bytes per token.
    fn count(&self, text: &str, rate: f64, prefix_bytes: usize, prefix_tokens: usize) -> usize {
        match self {
            Self::Estimate => {
                estimate::tokens(text.len().saturating_sub(prefix_bytes), rate) + prefix_tokens
            }
            Self::Uniform(uniform) => {
                let _ = (rate, prefix_bytes, prefix_tokens);
                estimate::tokens(text.len(), *uniform)
            }
            Self::Exact { tokenizer, .. } => {
                let _ = (rate, prefix_bytes, prefix_tokens);
                exact_count(tokenizer, text)
            }
        }
    }

    /// The tokens of the whole pack's text: the sum of its chunks by
    /// estimate, one count of the text with a tokenizer.
    fn total(&self, text: &str, sum: usize) -> usize {
        match self {
            Self::Estimate | Self::Uniform(_) => {
                let _ = text;
                sum
            }
            Self::Exact { tokenizer, .. } => {
                let _ = sum;
                exact_count(tokenizer, text)
            }
        }
    }

    fn margin(&self) -> f64 {
        match self {
            Self::Estimate => estimate::MARGIN,
            Self::Uniform(_) => 0.0,
            Self::Exact { .. } => estimate::EXACT_MARGIN,
        }
    }

    /// How the header says tokens were counted.
    fn counted(&self) -> String {
        match self {
            Self::Estimate => "estimated from bytes per token by language, with a margin".into(),
            Self::Uniform(rate) => format!("estimated as UTF-8 bytes / {rate}, without a margin"),
            Self::Exact { name, .. } => format!("counted exactly with the tokenizer {name}"),
        }
    }
}

fn exact_count(tokenizer: &tokenizers::Tokenizer, text: &str) -> usize {
    tokenizer
        .encode_fast(text, false)
        .map(|encoding| encoding.len())
        // A text the tokenizer refuses: the estimate, doubled.
        .unwrap_or_else(|_| 2 * estimate::tokens(text.len(), estimate::DEFAULT_BYTES_PER_TOKEN))
}

/// A tokenizer's name: its folder's for a `tokenizer.json`, else its file's.
pub fn tokenizer_name(path: &str) -> String {
    let path = Path::new(path);
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match path.parent().and_then(Path::file_name) {
        Some(folder) if file == "tokenizer.json" => folder.to_string_lossy().into_owned(),
        _ => file,
    }
}

/// A pack's sections, by their JSON names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    Header,
    Manifest,
    Architecture,
    /// The cache layout's map of the folders.
    Map,
    Focus,
    Paths,
    Tests,
    Docs,
    Breadth,
}

impl Section {
    pub const CHOOSABLE: [Section; 8] = [
        Section::Manifest,
        Section::Architecture,
        Section::Map,
        Section::Focus,
        Section::Paths,
        Section::Tests,
        Section::Docs,
        Section::Breadth,
    ];

    const ALL: [Section; 9] = [
        Section::Header,
        Section::Manifest,
        Section::Architecture,
        Section::Map,
        Section::Focus,
        Section::Paths,
        Section::Tests,
        Section::Docs,
        Section::Breadth,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Manifest => "manifest",
            Self::Architecture => "architecture",
            Self::Map => "map",
            Self::Focus => "focus",
            Self::Paths => "paths",
            Self::Tests => "tests",
            Self::Docs => "docs",
            Self::Breadth => "breadth",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Header => "Code pack",
            Self::Manifest => "Manifest",
            Self::Architecture => "Architecture summary",
            Self::Map => "Repository map",
            Self::Focus => "Focus code",
            Self::Paths => "Call paths",
            Self::Tests => "Tests",
            Self::Docs => "Documentation",
            Self::Breadth => "Breadth",
        }
    }

    /// A `--sections` name: a section's JSON name, or `arch`.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "arch" | "architecture" => Self::Architecture,
            "map" => Self::Map,
            "manifest" => Self::Manifest,
            "focus" => Self::Focus,
            "paths" => Self::Paths,
            "tests" => Self::Tests,
            "docs" => Self::Docs,
            "breadth" => Self::Breadth,
            _ => return None,
        })
    }
}

/// Where the most relevant code goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// Header, manifest, architecture, breadth (least relevant first), docs,
    /// tests, call paths, focus (most relevant last).
    RelevantLast,
    /// Header, manifest, architecture, focus (most relevant first), call
    /// paths, tests, docs, breadth.
    RelevantFirst,
}

impl Order {
    pub fn name(self) -> &'static str {
        match self {
            Self::RelevantLast => "relevant-last",
            Self::RelevantFirst => "relevant-first",
        }
    }

    fn sections(self) -> [Section; 8] {
        use Section::*;
        match self {
            Self::RelevantLast => [
                Header,
                Manifest,
                Architecture,
                Breadth,
                Docs,
                Tests,
                Paths,
                Focus,
            ],
            Self::RelevantFirst => [
                Header,
                Manifest,
                Architecture,
                Focus,
                Paths,
                Tests,
                Docs,
                Breadth,
            ],
        }
    }
}

/// How a pack's text is split for a provider's prompt cache, which matches
/// the longest identical start of a prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// One question-specific pack: its header names the question, so no
    /// part of it is the same for another question.
    Standard,
    /// A repository part first (header, architecture, map, breadth by
    /// centrality, top docs), the same bytes for every question at a
    /// commit, budget and settings; then a question part (manifest, focus,
    /// call paths, tests) that does not repeat what the first part holds.
    Cache,
}

impl Layout {
    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Cache => "cache",
        }
    }
}

/// The share of the budget the cache layout's repository part takes at
/// most, when a call gives none.
pub const DEFAULT_PREFIX_SHARE: f64 = 0.9;
/// The smallest prefix share.
pub const MIN_PREFIX_SHARE: f64 = 0.1;

/// Which chunks carry line numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Numbers {
    Focus,
    All,
    None,
}

/// The text a pack is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Markdown,
    Xml,
}

/// What a pack is asked for.
#[derive(Clone, Debug)]
pub struct Options {
    pub question: String,
    pub budget: usize,
    pub order: Order,
    pub numbers: Numbers,
    pub sections: BTreeSet<Section>,
    pub style: Style,
    pub tokenizer: Option<String>,
    /// One rate of UTF-8 bytes per token for all text, instead of the
    /// per-language estimate.
    pub bytes_per_token: Option<f64>,
    pub layout: Layout,
    /// The cache layout's repository part: this share of the budget at
    /// most (1.0: a stable pack, only the question after it changes).
    pub prefix_share: f64,
    /// The fixed budget the cache layout's repository part is sized from,
    /// when the budget itself moves between calls (a calibrated budget): the
    /// part then stays the same bytes ([`prefix_basis`]).
    pub prefix_budget: Option<usize>,
}

impl Options {
    /// From a call's arguments: `question`, `budget`, `order`,
    /// `line_numbers`, `sections` (a list or a comma-separated string),
    /// `style` (`markdown` or `xml`), `tokenizer` (a path), `layout`
    /// (`standard` or `cache`) and `prefix_share` (which alone chooses the
    /// cache layout).
    pub fn parse(args: &Value) -> Result<Self> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if question.len() > 1024 * 1024 {
            return Err(Error::bad("The question is longer than 1 MiB"));
        }
        let budget = match args.get("budget") {
            None | Some(Value::Null) => DEFAULT_BUDGET,
            Some(value) => value
                .as_u64()
                .map(|n| n as usize)
                .ok_or_else(|| Error::bad("budget must be a number of tokens"))?,
        };
        if !(MIN_BUDGET..=MAX_BUDGET).contains(&budget) {
            return Err(Error::bad(format!(
                "budget must be from {MIN_BUDGET} to {MAX_BUDGET} tokens"
            )));
        }
        let pick = |key: &str, default: &str| -> String {
            args.get(key)
                .and_then(Value::as_str)
                .unwrap_or(default)
                .to_string()
        };
        let order = match pick("order", "relevant-last").as_str() {
            "relevant-last" => Order::RelevantLast,
            "relevant-first" => Order::RelevantFirst,
            _ => return Err(Error::bad("order must be relevant-last or relevant-first")),
        };
        let numbers = match pick("line_numbers", "focus").as_str() {
            "focus" => Numbers::Focus,
            "all" => Numbers::All,
            "none" => Numbers::None,
            _ => return Err(Error::bad("line_numbers must be focus, all or none")),
        };
        let style = match pick("style", "markdown").as_str() {
            "markdown" => Style::Markdown,
            "xml" => Style::Xml,
            _ => return Err(Error::bad("style must be markdown or xml")),
        };
        let names: Vec<String> = match args.get("sections") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(list)) => list
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect(),
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            Some(_) => return Err(Error::bad("sections must be a list of section names")),
        };
        let sections = if names.is_empty() {
            Section::CHOOSABLE.into_iter().collect()
        } else {
            let mut chosen = BTreeSet::new();
            for name in &names {
                chosen.insert(Section::parse(name).ok_or_else(|| {
                    Error::bad(format!(
                        "Unknown section {name}: choose from arch, map, focus, paths, tests, docs, breadth, manifest"
                    ))
                })?);
            }
            chosen
        };
        let tokenizer = args
            .get("tokenizer")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        let bytes_per_token = match args.get("bytes_per_token") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_f64()
                    .filter(|rate| (1.0..=16.0).contains(rate))
                    .ok_or_else(|| Error::bad("bytes_per_token must be a number from 1 to 16"))?,
            ),
        };
        let prefix_share = match args.get("prefix_share") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_f64()
                    .filter(|share| (MIN_PREFIX_SHARE..=1.0).contains(share))
                    .ok_or_else(|| {
                        Error::bad(format!(
                            "prefix_share must be a number from {MIN_PREFIX_SHARE} to 1"
                        ))
                    })?,
            ),
        };
        let prefix_budget = match args.get("prefix_budget") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .map(|n| n as usize)
                    .filter(|n| (MIN_BUDGET..=MAX_BUDGET).contains(n))
                    .ok_or_else(|| {
                        Error::bad(format!(
                            "prefix_budget must be from {MIN_BUDGET} to {MAX_BUDGET} tokens"
                        ))
                    })?,
            ),
        };
        let layout = match args.get("layout").and_then(Value::as_str) {
            None if prefix_share.is_some() => Layout::Cache,
            None | Some("standard") => {
                if prefix_share.is_some() {
                    return Err(Error::bad(
                        "prefix_share sizes the cache layout's repository part: choose layout cache, or leave prefix_share out",
                    ));
                }
                Layout::Standard
            }
            Some("cache") => Layout::Cache,
            Some(_) => return Err(Error::bad("layout must be standard or cache")),
        };
        Ok(Self {
            question,
            budget,
            order,
            numbers,
            sections,
            style,
            tokenizer,
            bytes_per_token,
            layout,
            prefix_share: prefix_share.unwrap_or(DEFAULT_PREFIX_SHARE),
            prefix_budget,
        })
    }

    fn on(&self, section: Section) -> bool {
        section == Section::Header || self.sections.contains(&section)
    }
}

/// A code symbol of the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub node: usize,
    pub name: String,
    pub path: String,
    pub line: usize,
    pub degree: usize,
}

/// One edge of a node: the node at its other end, its relation, and whether
/// it points at the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub other: usize,
    pub relation: String,
    pub incoming: bool,
}

type EdgesFn<'a> = Box<dyn Fn(usize) -> Vec<Edge> + 'a>;
/// How a corpus reads a file's text by its path.
pub type FetchFn<'a> = Box<dyn FnMut(&str) -> Option<String> + 'a>;
type NodesFn<'a> = Box<dyn Fn(usize) -> Vec<usize> + 'a>;
type StartsFn<'a> = Box<dyn Fn(&str) -> Vec<usize> + 'a>;

/// What a pack reads of a code graph: its symbols, each node's edges, the
/// nodes each node points at (any relation), and each file's node start
/// lines (a node's range runs to the next start).
pub struct GraphView<'a> {
    symbols: Vec<Symbol>,
    by_path: HashMap<String, (usize, usize)>,
    by_node: HashMap<usize, usize>,
    edges: EdgesFn<'a>,
    dependencies: NodesFn<'a>,
    starts: StartsFn<'a>,
    memo: RefCell<HashMap<usize, Rc<Vec<Edge>>>>,
}

/// Edges read of one node at most (a hub's thousands are cut).
const MAX_EDGES_READ: usize = 2_000;

impl<'a> GraphView<'a> {
    pub fn new(
        mut symbols: Vec<Symbol>,
        edges: EdgesFn<'a>,
        dependencies: NodesFn<'a>,
        starts: StartsFn<'a>,
    ) -> Self {
        symbols.sort_by(|a, b| {
            (a.path.as_str(), a.line, a.name.as_str(), a.node).cmp(&(
                b.path.as_str(),
                b.line,
                b.name.as_str(),
                b.node,
            ))
        });
        let mut by_path: HashMap<String, (usize, usize)> = HashMap::new();
        let mut by_node = HashMap::new();
        for (i, symbol) in symbols.iter().enumerate() {
            by_path
                .entry(symbol.path.clone())
                .and_modify(|range| range.1 = i + 1)
                .or_insert((i, i + 1));
            by_node.entry(symbol.node).or_insert(i);
        }
        Self {
            symbols,
            by_path,
            by_node,
            edges,
            dependencies,
            starts,
            memo: RefCell::new(HashMap::new()),
        }
    }

    /// The graph of a saved query file.
    pub fn of_query_file(file: Arc<crate::query_file::QueryFile>) -> Option<GraphView<'static>> {
        use graphicle::serve_search::ScoreSource;
        let mut symbols = Vec::with_capacity(file.symbol_count());
        for position in 0..file.symbol_count() {
            let (node, name, path, line) = file.symbol(position).ok()?;
            let Some(line) = line else {
                continue;
            };
            symbols.push(Symbol {
                node,
                name: name.to_string(),
                path: path.to_string(),
                line,
                degree: file.degree(node),
            });
        }
        let edges_file = file.clone();
        let edges: EdgesFn<'static> = Box::new(move |node| {
            let Ok(adjacency) = edges_file.adjacency(node) else {
                return Vec::new();
            };
            let mut found = Vec::new();
            for id in adjacency.all().into_iter().take(MAX_EDGES_READ) {
                let Ok(edge) = edges_file.edge(id) else {
                    continue;
                };
                let relation = edge
                    .relation
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if edge.target == node && edge.source != node {
                    found.push(Edge {
                        other: edge.source,
                        relation,
                        incoming: true,
                    });
                } else if edge.source == node && edge.target != node {
                    found.push(Edge {
                        other: edge.target,
                        relation,
                        incoming: false,
                    });
                }
            }
            found
        });
        let deps_file = file.clone();
        let dependencies: NodesFn<'static> = Box::new(move |node| {
            let Ok(adjacency) = deps_file.adjacency(node) else {
                return Vec::new();
            };
            adjacency
                .neighbours()
                .into_iter()
                .filter(|&v| v != node && !adjacency.out_to(v).is_empty())
                .collect()
        });
        let starts: StartsFn<'static> =
            Box::new(move |path: &str| file.starts(path).ok().flatten().unwrap_or_default());
        Some(GraphView::new(symbols, edges, dependencies, starts))
    }

    /// The symbols of `path`, by start line.
    pub fn symbols(&self, path: &str) -> &[Symbol] {
        self.by_path
            .get(path)
            .map_or(&[], |&(from, to)| &self.symbols[from..to])
    }

    pub fn symbol_of(&self, node: usize) -> Option<&Symbol> {
        self.by_node.get(&node).map(|&i| &self.symbols[i])
    }

    fn edges(&self, node: usize) -> Rc<Vec<Edge>> {
        if let Some(hit) = self.memo.borrow().get(&node) {
            return hit.clone();
        }
        let mut found = (self.edges)(node);
        found.sort_by(|a, b| {
            (a.other, a.incoming, a.relation.as_str()).cmp(&(
                b.other,
                b.incoming,
                b.relation.as_str(),
            ))
        });
        found.dedup();
        let found = Rc::new(found);
        self.memo.borrow_mut().insert(node, found.clone());
        found
    }

    /// The node start lines of `path`, else its symbols' lines.
    fn starts(&self, path: &str) -> Vec<usize> {
        let mut starts = (self.starts)(path);
        if starts.is_empty() {
            starts = self.symbols(path).iter().map(|s| s.line).collect();
        }
        starts.sort_unstable();
        starts.dedup();
        starts
    }
}

/// What a pack is built from.
pub struct Corpus<'a> {
    pub project: String,
    pub commit: Option<String>,
    /// Every file of the project with its size in bytes.
    pub files: BTreeMap<String, usize>,
    /// The ranking's passages, best first: (path, first line, last line).
    pub ranking: Vec<(String, usize, usize)>,
    pub graph: Option<GraphView<'a>>,
    /// The question-independent architecture, when cached.
    pub modules: Option<Arc<Modules>>,
    fetch: FetchFn<'a>,
    cache: HashMap<String, Option<Arc<str>>>,
}

impl<'a> Corpus<'a> {
    pub fn new(
        project: String,
        commit: Option<String>,
        files: BTreeMap<String, usize>,
        ranking: Vec<(String, usize, usize)>,
        graph: Option<GraphView<'a>>,
        fetch: FetchFn<'a>,
    ) -> Self {
        Self {
            project,
            commit,
            files,
            ranking,
            graph,
            modules: None,
            fetch,
            cache: HashMap::new(),
        }
    }

    fn text(&mut self, path: &str) -> Option<Arc<str>> {
        if let Some(hit) = self.cache.get(path) {
            return hit.clone();
        }
        let read: Option<Arc<str>> = (self.fetch)(path).map(Arc::from);
        self.cache.insert(path.to_string(), read.clone());
        read
    }
}

fn extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    }
}

fn rate_of(path: &str) -> f64 {
    estimate::rate(&extension(path).to_ascii_lowercase())
}

fn language(path: &str) -> &'static str {
    match extension(path).to_ascii_lowercase().as_str() {
        "go" => "go",
        "py" => "python",
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "java" => "java",
        "kt" => "kotlin",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "scala" => "scala",
        "sh" | "bash" => "bash",
        "md" => "markdown",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "proto" => "protobuf",
        "sql" => "sql",
        "html" => "html",
        "css" => "css",
        _ => "",
    }
}

const CODE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cs", "cxx", "go", "h", "hh", "hpp", "java", "js", "jsx", "kt", "m", "mm",
    "php", "proto", "py", "rb", "rs", "scala", "sh", "swift", "ts", "tsx",
];

fn is_code(path: &str) -> bool {
    CODE_EXTENSIONS.contains(&extension(path).to_ascii_lowercase().as_str())
}

/// Generated code, which the centrality fill leaves out (a ranked generated
/// file still goes in).
fn generated(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.contains("generated")
        || name.ends_with(".pb.go")
        || name.ends_with("_pb2.py")
        || name.ends_with(".min.js")
        || path.contains("/testdata/")
        || path.starts_with("testdata/")
}

fn class(path: &str) -> crate::code_question::PathClass {
    crate::code_question::path_class(path)
}

fn est_file(path: &str, bytes: usize) -> usize {
    estimate::tokens(bytes, rate_of(path))
}

/// A symbol's name as a pack shows it: the code graph's qualified name
/// without its file (`pkg/x.go.Type.Method` is `Type.Method`), at most its
/// last two dotted segments; empty for a file's own node.
fn short(name: &str) -> &str {
    let tail = name.rsplit('/').next().unwrap_or(name);
    let mut parts = tail.splitn(3, '.');
    let tail = match (parts.next(), parts.next(), parts.next()) {
        (Some(stem), Some(ext), rest) if !stem.is_empty() && is_code(&format!("x.{ext}")) => {
            rest.unwrap_or("")
        }
        _ => tail,
    };
    let mut dots = tail.rmatch_indices('.');
    match (dots.next(), dots.next()) {
        (Some(_), Some((i, _))) => &tail[i + 1..],
        _ => tail,
    }
}

fn attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// A fence longer than any run of backticks in `body`.
fn fence(body: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// Line ranges (first, last; from 1) merged where they overlap or are at
/// most [`MERGE_GAP`] lines apart.
pub fn merge(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.retain(|&(a, b)| a >= 1 && a <= b);
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 1 + MERGE_GAP => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

/// Lines between two ranges that merging closes.
pub const MERGE_GAP: usize = 2;

/// `ranges` less the lines `covered` holds (both merged).
fn uncovered(ranges: &[(usize, usize)], covered: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for &(a, b) in ranges {
        let mut start = a;
        for &(c, d) in covered {
            if d < start || c > b {
                continue;
            }
            if c > start {
                out.push((start, c - 1));
            }
            start = start.max(d + 1);
            if start > b {
                break;
            }
        }
        if start <= b {
            out.push((start, b));
        }
    }
    out
}

/// One piece of a pack's text.
#[derive(Clone, Debug)]
struct Chunk {
    path: Option<String>,
    start: usize,
    end: usize,
    whole: bool,
    why: String,
    text: String,
    tokens: usize,
    /// The order its file went in (relevance), for the section's layout.
    order: usize,
}

impl Chunk {
    fn json(&self) -> Value {
        let lines = |n: usize| {
            if self.path.is_some() {
                json!(n)
            } else {
                Value::Null
            }
        };
        json!({"path":self.path,"start_line":lines(self.start),"end_line":lines(self.end),
            "whole":self.whole,"tokens":self.tokens,"why":self.why,"text":self.text})
    }
}

/// A file's text rendered as a chunk.
struct FilePiece<'t> {
    path: &'t str,
    lines: &'t [&'t str],
    start: usize,
    end: usize,
    whole: bool,
    numbered: bool,
}

fn render_file(
    style: Style,
    piece: &FilePiece<'_>,
    why: &str,
    counter: &Counter,
) -> (String, usize) {
    let mut body = String::new();
    let mut prefix_bytes = 0;
    let mut prefix_tenths = 0;
    for (i, line) in piece.lines[piece.start - 1..piece.end].iter().enumerate() {
        if piece.numbered {
            let n = piece.start + i;
            let prefix = format!("{n}\t");
            prefix_bytes += prefix.len();
            prefix_tenths += estimate::prefix_tenths(n);
            body.push_str(&prefix);
        }
        body.push_str(line);
        body.push('\n');
    }
    let range = format!("{}-{}", piece.start, piece.end);
    let text = match style {
        Style::Markdown => {
            let fence = fence(&body);
            format!(
                "### {} lines {range}{}\nwhy: {why}\n{fence}{}\n{body}{fence}\n\n",
                piece.path,
                if piece.whole { " (whole file)" } else { "" },
                language(piece.path)
            )
        }
        Style::Xml => format!(
            "<file path=\"{}\" lines=\"{range}\" why=\"{}\">\n{body}</file>\n",
            attr(piece.path),
            attr(why)
        ),
    };
    let tokens = counter.count(
        &text,
        rate_of(piece.path),
        prefix_bytes,
        prefix_tenths.div_ceil(10),
    );
    (text, tokens)
}

fn section_open(style: Style, section: Section) -> String {
    match style {
        Style::Markdown if section == Section::Header => String::new(),
        Style::Markdown => format!("## {}\n\n", section.title()),
        Style::Xml => format!("<section name=\"{}\">\n", section.name()),
    }
}

fn section_close(style: Style) -> String {
    match style {
        Style::Markdown => String::new(),
        Style::Xml => "</section>\n".into(),
    }
}

/// An included file.
#[derive(Clone, Debug)]
struct Included {
    tokens: usize,
    rank: Option<usize>,
    section: Section,
    /// The manifest line's cost charged when it went in.
    charged: usize,
}

/// The filling state: chunks by section, what each file holds, the tokens
/// used against the limit.
struct Fill<'o> {
    options: &'o Options,
    counter: &'o Counter,
    limit: usize,
    used: usize,
    sections: BTreeMap<Section, Vec<Chunk>>,
    spent: BTreeMap<Section, usize>,
    files: BTreeMap<String, Included>,
    covered: HashMap<String, Vec<(usize, usize)>>,
    next_order: usize,
    /// The cost of each section's opening and closing chunks.
    frame: BTreeMap<Section, usize>,
    /// Whether each new file is charged its manifest line.
    manifest: bool,
}

impl<'o> Fill<'o> {
    fn new(
        options: &'o Options,
        counter: &'o Counter,
        limit: usize,
        frame: BTreeMap<Section, usize>,
        manifest: bool,
    ) -> Self {
        Self {
            options,
            counter,
            limit,
            used: 0,
            sections: BTreeMap::new(),
            spent: BTreeMap::new(),
            files: BTreeMap::new(),
            covered: HashMap::new(),
            next_order: 0,
            frame,
            manifest,
        }
    }

    fn numbered(&self, section: Section) -> bool {
        match self.options.numbers {
            Numbers::All => true,
            Numbers::Focus => section == Section::Focus,
            Numbers::None => false,
        }
    }

    fn manifest_cost(&self, path: &str, rank: Option<usize>, section: Section) -> usize {
        if !self.manifest {
            return 0;
        }
        let line = manifest_line(path, 9_999_999, rank, section);
        self.counter
            .count(&line, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0)
    }

    fn room(&self, section: Section, cap: usize) -> usize {
        let frame = if self.sections.contains_key(&section) {
            0
        } else {
            self.frame[&section]
        };
        let by_cap = cap.saturating_sub(self.spent.get(&section).copied().unwrap_or(0) + frame);
        let by_limit = self.limit.saturating_sub(self.used + frame);
        by_cap.min(by_limit)
    }

    /// Add `chunk` to `section` if it fits the section's cap and the limit.
    fn add(&mut self, section: Section, cap: usize, mut chunk: Chunk, rank: Option<usize>) -> bool {
        let manifest = match &chunk.path {
            Some(path) if !self.files.contains_key(path) => self.manifest_cost(path, rank, section),
            _ => 0,
        };
        if chunk.tokens + manifest > self.room(section, cap) {
            return false;
        }
        if !self.sections.contains_key(&section) {
            self.used += self.frame[&section];
            *self.spent.entry(section).or_default() += self.frame[&section];
        }
        self.used += chunk.tokens + manifest;
        *self.spent.entry(section).or_default() += chunk.tokens;
        if let Some(path) = &chunk.path {
            let entry = self.files.entry(path.clone()).or_insert(Included {
                tokens: 0,
                rank,
                section,
                charged: manifest,
            });
            entry.tokens += chunk.tokens;
            let covered = self.covered.entry(path.clone()).or_default();
            covered.push((chunk.start, chunk.end));
            *covered = merge_exact(std::mem::take(covered));
        }
        if chunk.order == usize::MAX {
            chunk.order = self.next_order;
        }
        self.next_order += 1;
        self.sections.entry(section).or_default().push(chunk);
        true
    }

    /// Remove the last chunk of `section` (the budget check at the end).
    fn pop(&mut self, section: Section) -> bool {
        let Some(list) = self.sections.get_mut(&section) else {
            return false;
        };
        let Some(chunk) = list.pop() else {
            return false;
        };
        let empty = list.is_empty();
        self.used -= chunk.tokens;
        if let Some(path) = &chunk.path {
            let mut gone = false;
            if let Some(entry) = self.files.get_mut(path) {
                entry.tokens -= chunk.tokens;
                if entry.tokens == 0 {
                    self.used -= entry.charged;
                    gone = true;
                }
            }
            if gone {
                self.files.remove(path);
                self.covered.remove(path);
            }
        }
        if empty {
            self.sections.remove(&section);
            self.used -= self.frame[&section];
        }
        true
    }

    fn covered(&self, path: &str) -> &[(usize, usize)] {
        self.covered.get(path).map_or(&[], Vec::as_slice)
    }

    /// The chunks of a file's `ranges` (uncovered parts only), each with
    /// `why`; false when the first does not fit.
    #[allow(clippy::too_many_arguments)]
    fn add_ranges(
        &mut self,
        section: Section,
        cap: usize,
        path: &str,
        lines: &[&str],
        ranges: &[(usize, usize)],
        why: &dyn Fn(usize, usize) -> String,
        rank: Option<usize>,
        order: usize,
    ) -> usize {
        let total = lines.len();
        let ranges: Vec<(usize, usize)> = ranges
            .iter()
            .map(|&(a, b)| (a.max(1), b.min(total)))
            .filter(|&(a, b)| a <= b)
            .collect();
        let ranges = uncovered(&merge(ranges), self.covered(path));
        let mut added = 0;
        for (a, b) in ranges {
            let whole = a == 1 && b == total && self.covered(path).is_empty();
            let piece = FilePiece {
                path,
                lines,
                start: a,
                end: b,
                whole,
                numbered: self.numbered(section),
            };
            let reason = why(a, b);
            let (text, tokens) = render_file(self.options.style, &piece, &reason, self.counter);
            let chunk = Chunk {
                path: Some(path.to_string()),
                start: a,
                end: b,
                whole,
                why: reason,
                text,
                tokens,
                order,
            };
            if self.add(section, cap, chunk, rank) {
                added += 1;
            }
        }
        added
    }
}

/// Ranges merged only where they overlap or touch (what a file holds).
fn merge_exact(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

fn manifest_line(path: &str, tokens: usize, rank: Option<usize>, section: Section) -> String {
    match rank {
        Some(rank) => format!(
            "- {path}: {tokens} tokens, {}, rank {rank}\n",
            section.name()
        ),
        None => format!("- {path}: {tokens} tokens, {}\n", section.name()),
    }
}

/// A ranked file: its rank (from 1), its passages, best first, and why it
/// was put in the ranking when the retrieval did not rank it.
#[derive(Clone, Debug)]
struct Ranked {
    path: String,
    rank: usize,
    passages: Vec<(usize, usize)>,
    added: Option<&'static str>,
}

/// Folder names too general to route a question to.
const GENERAL_FOLDERS: &[&str] = &[
    "api",
    "app",
    "apps",
    "build",
    "cmd",
    "common",
    "components",
    "config",
    "core",
    "doc",
    "docs",
    "example",
    "examples",
    "hack",
    "helpers",
    "internal",
    "lib",
    "libs",
    "pkg",
    "plugin",
    "plugins",
    "script",
    "scripts",
    "server",
    "src",
    "staging",
    "test",
    "tests",
    "testdata",
    "third_party",
    "tools",
    "util",
    "utils",
    "vendor",
    "web",
];
/// Folder segments whose folders are never routed to: tests and vendored code.
const UNROUTED_SEGMENTS: &[&str] = &[
    "test",
    "tests",
    "testdata",
    "e2e",
    "fixtures",
    "vendor",
    "third_party",
    "node_modules",
];
/// A folder the question names is routed to when it holds at most this
/// many files.
const ROUTE_MAX_FILES: usize = 80;
/// Top-ranked files that stay ahead of the named folders' files.
const ROUTE_AFTER: usize = 5;
/// Ranked files that stay ahead of the definition files.
const DEFINITIONS_AFTER: usize = 15;
/// Top files whose imports lead to definitions, and the definition files
/// added at most.
const DEFINITION_SOURCES: usize = 8;
const DEFINITION_FILES: usize = 12;

/// The folders a question names: a folder whose last name is one, two or
/// three consecutive words of the question joined by `_` or `-` ("Alexa
/// Devices" names `alexa_devices`; the segments of a path, a URL or a
/// dotted module name are words too), that holds at most
/// [`ROUTE_MAX_FILES`] files, is not a test or vendored folder, and shares
/// its name with at most one other such folder. The longest name wins where
/// names overlap. In the order the question names them, at most three.
fn named_folders(question: &str, files: &BTreeMap<String, usize>) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for path in files.keys() {
        let mut end = path.len();
        while let Some(i) = path[..end].rfind('/') {
            *counts.entry(&path[..i]).or_default() += 1;
            end = i;
        }
    }
    let mut by_leaf: HashMap<&str, Vec<&str>> = HashMap::new();
    for (&folder, &count) in &counts {
        let routable =
            count <= ROUTE_MAX_FILES && !folder.split('/').any(|s| UNROUTED_SEGMENTS.contains(&s));
        if routable {
            let leaf = folder.rsplit('/').next().unwrap_or(folder);
            by_leaf.entry(leaf).or_default().push(folder);
        }
    }
    let lower = question.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let mut used = vec![false; words.len()];
    let mut out: Vec<String> = Vec::new();
    'words: for i in 0..words.len() {
        for n in (1..=3).rev() {
            if i + n > words.len() || used[i..i + n].iter().any(|u| *u) {
                continue;
            }
            for separator in ["_", "-"] {
                if n == 1 && separator == "-" {
                    continue;
                }
                let name = words[i..i + n].join(separator);
                if name.len() < 3 || GENERAL_FOLDERS.contains(&name.as_str()) {
                    continue;
                }
                let Some(folders) = by_leaf.get(name.as_str()) else {
                    continue;
                };
                if folders.len() > 2 {
                    continue;
                }
                let mut folders = folders.clone();
                folders.sort_unstable();
                for folder in folders {
                    if !out.iter().any(|f| f == folder) {
                        out.push(folder.to_string());
                    }
                }
                used[i..i + n].iter_mut().for_each(|u| *u = true);
                if out.len() >= 3 {
                    break 'words;
                }
                continue 'words;
            }
        }
    }
    out.truncate(3);
    out
}

static GO_IMPORT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"^\s*(?:import\s+)?(?:([A-Za-z_][A-Za-z0-9_]*)\s+)?"([^"]+)"\s*$"#).unwrap()
});
static GO_SELECTOR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\b([a-z][A-Za-z0-9_]*)\.([A-Z][A-Za-z0-9_]{2,})\b").unwrap()
});
static PY_FROM: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?m)^\s*from\s+(\.*)([A-Za-z0-9_.]*)\s+import\s").unwrap()
});

/// A Go file's imports: package name (its alias, else the path's last
/// segment) to import path.
fn go_imports(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut block = false;
    for line in text.lines().take(400) {
        let trimmed = line.trim();
        if trimmed.starts_with("import (") {
            block = true;
            continue;
        }
        if block && trimmed.starts_with(')') {
            block = false;
            continue;
        }
        if !(block || trimmed.starts_with("import ")) {
            continue;
        }
        if let Some(found) = GO_IMPORT.captures(trimmed) {
            let path = found[2].to_string();
            let name = match found.get(1) {
                Some(alias) => alias.as_str().to_string(),
                None => path.rsplit('/').next().unwrap_or(&path).to_string(),
            };
            out.insert(name, path);
        }
    }
    out
}

/// The project folder an import path names: the longest tail of it (two
/// segments at least) that is a folder of the project, as is or under
/// `staging/src/`.
fn import_folder(path: &str, folders: &BTreeSet<String>) -> Option<String> {
    let staged = format!("staging/src/{path}");
    if folders.contains(&staged) {
        return Some(staged);
    }
    let segments: Vec<&str> = path.split('/').collect();
    (0..segments.len().saturating_sub(1))
        .map(|i| segments[i..].join("/"))
        .find(|tail| folders.contains(tail))
}

/// Whether `text` defines `name` (Go): a declaration line that starts with
/// it (in a const, var or type block), or `func`, `type`, `var` or `const`
/// before it.
fn go_defines(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        let after = |prefix: &str| -> bool {
            trimmed.strip_prefix(prefix).is_some_and(|rest| {
                let rest = rest.trim_start();
                let rest = if prefix == "func " && rest.starts_with('(') {
                    rest.split_once(')').map_or("", |(_, r)| r.trim_start())
                } else {
                    rest
                };
                rest.strip_prefix(name)
                    .is_some_and(|tail| tail.starts_with([' ', '\t', '=', '(', '[']))
            })
        };
        after("") && line.starts_with(['\t', ' '])
            || after("func ")
            || after("type ")
            || after("var ")
            || after("const ")
    })
}

/// TypeScript and JavaScript files.
const TS_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];
/// C, C++ and Objective-C files.
const C_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx", "inl", "ipp", "m", "mm",
];
/// tsconfig files read for path aliases at most.
const MAX_TSCONFIGS: usize = 40;

static TS_FROM: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?m)^[ \t]*(?:import|export)\b([^;'"`]*?)\bfrom[ \t]*['"]([^'"\n]+)['"]"#)
        .unwrap()
});
static TS_BARE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?m)^[ \t]*import[ \t]*['"]([^'"\n]+)['"]|\b(?:require|import)[ \t]*\([ \t]*['"]([^'"\n]+)['"][ \t]*\)"#,
    )
    .unwrap()
});
static TS_PATHS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#""paths"\s*:\s*\{([^{}]*)\}"#).unwrap());
static TS_PATH_ENTRY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#""([^"]+)"\s*:\s*\[([^\]]*)\]"#).unwrap());
static TS_BASE_URL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#""baseUrl"\s*:\s*"([^"]*)""#).unwrap());
static QUOTED: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#""([^"]*)""#).unwrap());
static IDENTIFIER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());

/// `folder/relative` with `.` and `..` resolved; `None` when it climbs out
/// of the project.
fn joined(folder: &str, relative: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for part in folder.split('/').chain(relative.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            part => out.push(part),
        }
    }
    Some(out.join("/"))
}

/// The project file a module path names, as TypeScript resolves it: the
/// path itself, with a TypeScript or JavaScript extension, or its folder's
/// index file (`./x.js` in an ES module names `x.ts`).
fn ts_file(base: &str, files: &BTreeMap<String, usize>) -> Option<String> {
    let base = ["js", "mjs", "cjs", "jsx"]
        .iter()
        .find_map(|e| base.strip_suffix(&format!(".{e}")))
        .unwrap_or(base);
    if base.is_empty() {
        return None;
    }
    let mut candidates = vec![base.to_string()];
    for e in ["ts", "tsx", "d.ts", "js", "jsx", "mts", "mjs", "cts", "cjs"] {
        candidates.push(format!("{base}.{e}"));
    }
    for e in ["ts", "tsx", "js", "jsx"] {
        candidates.push(format!("{base}/index.{e}"));
    }
    candidates.into_iter().find(|c| {
        files.contains_key(c) && TS_EXTENSIONS.contains(&extension(c).to_ascii_lowercase().as_str())
    })
}

/// A tsconfig `paths` entry: `@scope/*` to `packages/*`, from the project
/// root.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TsAlias {
    pattern: String,
    targets: Vec<String>,
}

/// The `paths` aliases of a tsconfig file at `path` (comments allowed),
/// with their targets resolved from its `baseUrl` (else its folder) to
/// project paths. Targets outside the project or in `node_modules` are
/// left out.
fn ts_aliases_of(path: &str, text: &str) -> Vec<TsAlias> {
    let folder = path.rsplit_once('/').map_or("", |(f, _)| f);
    let base = TS_BASE_URL
        .captures(text)
        .and_then(|c| joined(folder, &c[1]))
        .unwrap_or_else(|| folder.to_string());
    let Some(block) = TS_PATHS.captures(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in TS_PATH_ENTRY.captures_iter(&block[1]) {
        let targets: Vec<String> = QUOTED
            .captures_iter(&entry[2])
            .filter_map(|t| joined(&base, &t[1]))
            .filter(|t| !t.is_empty() && !t.split('/').any(|s| s == "node_modules"))
            .collect();
        if !targets.is_empty() {
            out.push(TsAlias {
                pattern: entry[1].to_string(),
                targets,
            });
        }
    }
    out
}

/// The project's tsconfig path aliases: those of its `tsconfig*.json`
/// files at most three folders deep, outside tests and vendored code.
fn ts_aliases(corpus: &mut Corpus<'_>) -> Vec<TsAlias> {
    let configs: Vec<String> = corpus
        .files
        .keys()
        .filter(|p| {
            let name = p.rsplit('/').next().unwrap_or(p);
            name.starts_with("tsconfig")
                && name.ends_with(".json")
                && p.matches('/').count() <= 3
                && !is_test(p)
                && class(p) != crate::code_question::PathClass::Vendored
        })
        .take(MAX_TSCONFIGS)
        .cloned()
        .collect();
    let mut out: Vec<TsAlias> = Vec::new();
    for config in configs {
        if let Some(text) = corpus.text(&config) {
            for alias in ts_aliases_of(&config, &text) {
                if !out.contains(&alias) {
                    out.push(alias);
                }
            }
        }
    }
    out
}

/// The project file a TypeScript or JavaScript import names: a relative
/// path from `here`, or a tsconfig alias. Package imports outside the
/// project resolve to nothing.
fn ts_resolve(
    spec: &str,
    here: &str,
    files: &BTreeMap<String, usize>,
    aliases: &[TsAlias],
) -> Option<String> {
    if spec.starts_with('.') {
        return ts_file(&joined(here, spec)?, files);
    }
    for alias in aliases {
        let star = match alias.pattern.split_once('*') {
            Some((prefix, suffix)) => {
                let Some(middle) = spec
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix))
                else {
                    continue;
                };
                Some(middle)
            }
            None if alias.pattern == spec => None,
            None => continue,
        };
        for target in &alias.targets {
            let base = match star {
                Some(middle) => target.replacen('*', middle, 1),
                None => target.clone(),
            };
            if let Some(file) = ts_file(&base, files) {
                return Some(file);
            }
        }
    }
    None
}

/// The modules a TypeScript or JavaScript file imports or re-exports, each
/// with the number of names it takes from it (at least one).
fn ts_imports(text: &str) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for found in TS_FROM.captures_iter(text) {
        let clause = &found[1];
        let names = match clause.split_once('{') {
            Some((head, rest)) => {
                let inner = rest.split('}').next().unwrap_or("");
                let listed = inner
                    .split(',')
                    .filter(|n| {
                        n.split_whitespace()
                            .any(|w| w != "type" && IDENTIFIER.is_match(w))
                    })
                    .count();
                // `import D, {...}` takes D too; `import type {...}` does not.
                let default = head
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .any(|w| w != "type" && IDENTIFIER.is_match(w));
                listed + usize::from(default)
            }
            None => 1,
        };
        out.push((found[2].to_string(), names.max(1)));
    }
    for found in TS_BARE.captures_iter(text) {
        if let Some(spec) = found.get(1).or(found.get(2)) {
            out.push((spec.as_str().to_string(), 1));
        }
    }
    out
}

/// Whether a module is a barrel (`index.ts`, `public_api.ts`) that passes
/// other modules' names on.
fn ts_barrel(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name);
    matches!(stem, "index" | "public_api" | "public-api")
}

/// The files that define what the top ranked files use: for Go, the
/// identifiers they take from imported project packages (`features.X`
/// leads to the file of `pkg/features` that declares `X`); for Python, the
/// project modules they import from their own folder; for TypeScript and
/// JavaScript, the modules they import by a relative path or a tsconfig
/// alias, weighed by the names they take. Most used first.
fn definitions(corpus: &mut Corpus<'_>, top: &[Ranked]) -> Vec<String> {
    let mut folders: BTreeSet<String> = BTreeSet::new();
    for path in corpus.files.keys() {
        if let Some((folder, _)) = path.rsplit_once('/') {
            folders.insert(folder.to_string());
        }
    }
    let skip = |path: &str| {
        is_test(path) || generated(path) || class(path) == crate::code_question::PathClass::Vendored
    };
    let mut score: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut order = 0usize;
    let mut credit = |file: String, weight: usize, score: &mut BTreeMap<String, (usize, usize)>| {
        let entry = score.entry(file).or_insert((0, order));
        entry.0 += weight;
        order += 1;
    };
    // Read when a TypeScript or JavaScript source needs them.
    let mut aliases: Option<Vec<TsAlias>> = None;
    let in_top = |file: &str| top.iter().any(|r| r.path == file);
    for source in top
        .iter()
        .filter(|r| !skip(&r.path))
        .take(DEFINITION_SOURCES)
    {
        let Some(text) = corpus.text(&source.path) else {
            continue;
        };
        let here = source
            .path
            .rsplit_once('/')
            .map_or("", |(f, _)| f)
            .to_string();
        match extension(&source.path) {
            "go" => {
                let imports = go_imports(&text);
                let mut used: BTreeSet<(String, String)> = BTreeSet::new();
                for found in GO_SELECTOR.captures_iter(&text) {
                    if imports.contains_key(&found[1]) {
                        used.insert((found[1].to_string(), found[2].to_string()));
                    }
                }
                for (package, name) in used.into_iter().take(200) {
                    let Some(folder) = import_folder(&imports[&package], &folders) else {
                        continue;
                    };
                    if folder == here {
                        continue;
                    }
                    let candidates: Vec<String> = corpus
                        .files
                        .keys()
                        .filter(|p| {
                            p.rsplit_once('/').is_some_and(|(f, _)| f == folder)
                                && p.ends_with(".go")
                                && !skip(p)
                        })
                        .cloned()
                        .collect();
                    for candidate in candidates {
                        if corpus
                            .text(&candidate)
                            .is_some_and(|t| go_defines(&t, &name))
                        {
                            credit(candidate, 1, &mut score);
                            break;
                        }
                    }
                }
            }
            "py" => {
                for found in PY_FROM.captures_iter(&text) {
                    let module = found[2].replace('.', "/");
                    let base = if found[1].is_empty() {
                        // Absolute: only modules of the importing file's own folder tree.
                        module.clone()
                    } else {
                        let mut base = here.clone();
                        for _ in 1..found[1].len() {
                            base = base
                                .rsplit_once('/')
                                .map_or(String::new(), |(f, _)| f.to_string());
                        }
                        if module.is_empty() {
                            base
                        } else if base.is_empty() {
                            module.clone()
                        } else {
                            format!("{base}/{module}")
                        }
                    };
                    for file in [format!("{base}.py"), format!("{base}/__init__.py")] {
                        let near = file.starts_with(&format!("{here}/"))
                            || here.starts_with(base.as_str());
                        if near
                            && corpus.files.contains_key(&file)
                            && !skip(&file)
                            && file != source.path
                        {
                            credit(file, 1, &mut score);
                            break;
                        }
                    }
                }
            }
            ext if TS_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) => {
                if aliases.is_none() {
                    aliases = Some(ts_aliases(corpus));
                }
                let aliases = aliases.as_deref().unwrap_or_default();
                for (spec, names) in ts_imports(&text) {
                    let Some(file) = ts_resolve(&spec, &here, &corpus.files, aliases) else {
                        continue;
                    };
                    // A package's barrel only passes its modules' names on.
                    if !spec.starts_with('.') && ts_barrel(&file) {
                        continue;
                    }
                    if file != source.path && !skip(&file) && !in_top(&file) {
                        credit(file, names, &mut score);
                    }
                }
            }
            _ => {}
        }
    }
    let mut files: Vec<(String, (usize, usize))> = score.into_iter().collect();
    files.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.1.1.cmp(&b.1.1)));
    files
        .into_iter()
        .map(|(f, _)| f)
        .take(DEFINITION_FILES)
        .collect()
}

/// A name or file stem without case and separators: `MeshInstance3D`,
/// `mesh_instance_3d` and `mesh-instance-3d` are all `meshinstance3d`.
fn folded(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a name reads as one of code's compound names: camelCase or
/// PascalCase with a capital after a lowercase letter or a digit
/// (`TextEdit`, `MeshInstance3D`, `formControl`), or words joined by `_`.
fn compound(name: &str) -> bool {
    let bytes = name.as_bytes();
    let inner_capital = bytes
        .windows(2)
        .any(|w| (w[0].is_ascii_lowercase() || w[0].is_ascii_digit()) && w[1].is_ascii_uppercase());
    let joined = name
        .trim_matches('_')
        .split('_')
        .filter(|p| !p.is_empty())
        .count()
        > 1;
    inner_capital || joined
}

/// Files a question names by a code name at most, per name.
const NAMED_FILES_PER_NAME: usize = 4;
/// Code names of a question routed to files at most.
const NAMED_FILE_NAMES: usize = 4;
/// Files routed by name at most.
const NAMED_FILES_MAX: usize = 8;
/// Words of a question read for names of two or three words.
const NAMED_WORDS: usize = 600;

static WORD: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"[A-Za-z0-9]+").unwrap());

/// The files a question names by a code name: a compound name of the
/// question (a part of `Class::method` or `a.b.C` too), or two or three
/// consecutive words of it, whose folded form is a code file's folded stem
/// (`TextEdit` and "the text edit" name `text_edit.cpp` and `text_edit.h`,
/// `FormControl` names `form_control.ts`, `UserModel` names
/// `UserModel.java`, `FooComponent` names `foo.component.ts`, "FileSystem
/// dock" names `filesystem_dock.cpp`), when at most
/// [`NAMED_FILES_PER_NAME`] code files outside tests carry that stem. In
/// the order the question names them, for at most [`NAMED_FILE_NAMES`]
/// names and [`NAMED_FILES_MAX`] files.
fn named_files(question: &str, files: &BTreeMap<String, usize>) -> Vec<String> {
    // (where the question names it, folded name)
    let mut keys: Vec<(usize, String)> = Vec::new();
    for name in crate::identifier_arm::named(question).names {
        let at = question.find(name.as_str()).unwrap_or(usize::MAX);
        for part in name.split(['.', ':', '#', '-', '>']) {
            let key = folded(part);
            if compound(part) && key.len() >= 6 {
                keys.push((at, key));
            }
        }
    }
    keys.sort_by_key(|(at, _)| *at);
    // Code names first, then runs of two or three words.
    let compound_keys = keys.len();
    let words: Vec<(usize, String)> = WORD
        .find_iter(question)
        .take(NAMED_WORDS)
        .map(|m| (m.start(), m.as_str().to_ascii_lowercase()))
        .collect();
    let mut runs: Vec<(usize, String)> = Vec::new();
    for n in 2..=3 {
        for run in words.windows(n) {
            let key: String = run.iter().map(|(_, w)| w.as_str()).collect();
            if key.len() >= 6 {
                runs.push((run[0].0, key));
            }
        }
    }
    runs.sort_by_key(|(at, _)| *at);
    keys.extend(runs);
    let mut wanted: Vec<(String, bool)> = Vec::new();
    let mut known: HashSet<String> = HashSet::new();
    for (i, (_, key)) in keys.into_iter().enumerate() {
        if known.insert(key.clone()) {
            wanted.push((key, i < compound_keys));
        }
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut by_stem: HashMap<String, Vec<&String>> = HashMap::new();
    for path in files.keys() {
        let name = path.rsplit('/').next().unwrap_or(path);
        let Some((stem, _)) = name.rsplit_once('.') else {
            continue;
        };
        let key = folded(stem);
        if known.contains(&key) {
            by_stem.entry(key).or_default().push(path);
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut names = 0;
    for (key, code_name) in &wanted {
        if out.len() >= NAMED_FILES_MAX {
            break;
        }
        let Some(paths) = by_stem.get(key) else {
            continue;
        };
        // Words name a file only in the languages that name files after
        // their classes.
        let class_files = |p: &str| {
            let ext = extension(p).to_ascii_lowercase();
            ext == "java"
                || TS_EXTENSIONS.contains(&ext.as_str())
                || C_EXTENSIONS.contains(&ext.as_str())
        };
        let code: Vec<&&String> = paths
            .iter()
            .filter(|p| {
                (is_code(p) || TS_EXTENSIONS.contains(&extension(p).to_ascii_lowercase().as_str()))
                    && (*code_name || class_files(p))
                    && !is_test(p)
                    && !generated(p)
                    && class(p) != crate::code_question::PathClass::Vendored
            })
            .collect();
        if code.is_empty() || code.len() > NAMED_FILES_PER_NAME {
            continue;
        }
        for path in code {
            if !out.contains(*path) {
                out.push((*path).clone());
            }
        }
        names += 1;
        if names >= NAMED_FILE_NAMES {
            break;
        }
    }
    out.truncate(NAMED_FILES_MAX);
    out
}

/// The ranking a pack fills by: the retrieval's, with the files the
/// question names by a code name and the files of the folders it names
/// right after its top files, and the files that define what the top files
/// use after the first [`DEFINITIONS_AFTER`]. Ranks follow the new order.
fn augment(corpus: &mut Corpus<'_>, question: &str, ranked: Vec<Ranked>) -> Vec<Ranked> {
    let folders = named_folders(question, &corpus.files);
    let mut routed: Vec<Ranked> = Vec::new();
    for path in named_files(question, &corpus.files) {
        let passages = ranked
            .iter()
            .find(|r| r.path == path)
            .map(|r| r.passages.clone())
            .unwrap_or_default();
        routed.push(Ranked {
            path,
            rank: 0,
            passages,
            added: Some("named in the question"),
        });
    }
    for folder in &folders {
        let prefix = format!("{folder}/");
        let mut files: Vec<(&String, usize)> = corpus
            .files
            .iter()
            .filter(|(p, _)| p.starts_with(&prefix) && !is_test(p) && !generated(p))
            .map(|(p, b)| (p, *b))
            .collect();
        // Files the retrieval ranked first, by rank; then the rest, smallest first.
        let rank_of: HashMap<&str, usize> =
            ranked.iter().map(|r| (r.path.as_str(), r.rank)).collect();
        files.sort_by(|a, b| {
            let ra = rank_of.get(a.0.as_str()).copied().unwrap_or(usize::MAX);
            let rb = rank_of.get(b.0.as_str()).copied().unwrap_or(usize::MAX);
            ra.cmp(&rb).then(a.1.cmp(&b.1)).then(a.0.cmp(b.0))
        });
        for (path, _) in files {
            let passages = ranked
                .iter()
                .find(|r| &r.path == path)
                .map(|r| r.passages.clone())
                .unwrap_or_default();
            routed.push(Ranked {
                path: path.clone(),
                rank: 0,
                passages,
                added: Some("in a folder the question names"),
            });
        }
    }
    let mut order: Vec<Ranked> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut push = |r: Ranked, order: &mut Vec<Ranked>| {
        if seen.insert(r.path.clone()) {
            order.push(r);
        }
    };
    let mut rest = ranked.into_iter();
    for r in rest.by_ref().take(ROUTE_AFTER) {
        push(r, &mut order);
    }
    for r in routed {
        push(r, &mut order);
    }
    let mut tail: Vec<Ranked> = rest.collect();
    // Definitions come after the first ranks, computed from the top files.
    let head_len = DEFINITIONS_AFTER.saturating_sub(order.len());
    let more: Vec<Ranked> = tail.drain(..head_len.min(tail.len())).collect();
    for r in more {
        push(r, &mut order);
    }
    let top: Vec<Ranked> = order.iter().take(DEFINITION_SOURCES * 2).cloned().collect();
    for file in definitions(corpus, &top) {
        push(
            Ranked {
                path: file,
                rank: 0,
                passages: Vec::new(),
                added: Some("defines what the top files use"),
            },
            &mut order,
        );
    }
    for r in tail {
        push(r, &mut order);
    }
    for (i, r) in order.iter_mut().enumerate() {
        r.rank = i + 1;
    }
    order
}

fn ranked_files(corpus: &Corpus<'_>) -> Vec<Ranked> {
    let mut order: Vec<Ranked> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    for (path, a, b) in &corpus.ranking {
        if !corpus.files.contains_key(path) {
            continue;
        }
        let i = *at.entry(path.as_str()).or_insert_with(|| {
            order.push(Ranked {
                path: path.clone(),
                rank: order.len() + 1,
                passages: Vec::new(),
                added: None,
            });
            order.len() - 1
        });
        order[i].passages.push(((*a).max(1), (*b).max(*a).max(1)));
    }
    order
}

/// The question-independent view of a project: its folders as modules,
/// and the dependencies between them.
#[derive(Clone, Debug, Default)]
pub struct Modules {
    pub files: usize,
    pub tokens: usize,
    pub modules: Vec<Module>,
    /// (from, to, relations), most first.
    pub edges: Vec<(String, String, usize)>,
}

#[derive(Clone, Debug, Default)]
pub struct Module {
    pub path: String,
    pub files: usize,
    pub tokens: usize,
    pub language: String,
    pub purpose: String,
    pub key: Vec<String>,
    pub entries: Vec<String>,
}

/// Folders a summary splits the project into at most.
const MAX_MODULES: usize = 48;

fn folder_of(path: &str, depth: usize) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= depth {
        // A file directly in the folder of depth - 1.
        return parts[..parts.len() - 1].join("/");
    }
    parts[..depth].join("/")
}

/// The modules of `files`: top-level folders, the largest split into their
/// subfolders while there are fewer than [`MAX_MODULES`].
fn partition(files: &BTreeMap<String, usize>) -> BTreeMap<String, Vec<String>> {
    // module prefix → (depth, files)
    let mut modules: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    for path in files.keys() {
        let folder = folder_of(path, 1);
        modules
            .entry(folder)
            .or_insert((1, Vec::new()))
            .1
            .push(path.clone());
    }
    let tokens = |list: &[String]| -> usize { list.iter().map(|p| est_file(p, files[p])).sum() };
    let total: usize = files.iter().map(|(p, b)| est_file(p, *b)).sum();
    let mut sealed: BTreeSet<String> = BTreeSet::new();
    loop {
        if modules.len() >= MAX_MODULES {
            break;
        }
        let largest = modules
            .iter()
            .filter(|(prefix, _)| !sealed.contains(*prefix))
            .map(|(prefix, (_, list))| (tokens(list), prefix.clone()))
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let Some((size, prefix)) = largest else {
            break;
        };
        if size * 16 < total {
            break;
        }
        let (depth, list) = modules[&prefix].clone();
        let vendored = list
            .first()
            .is_some_and(|p| class(p) == crate::code_question::PathClass::Vendored);
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for path in &list {
            children
                .entry(folder_of(path, depth + 1))
                .or_default()
                .push(path.clone());
        }
        if vendored || children.len() < 2 || modules.len() - 1 + children.len() > MAX_MODULES {
            sealed.insert(prefix);
            continue;
        }
        modules.remove(&prefix);
        for (child, list) in children {
            if child == prefix {
                // The files directly in the folder stay one module, sealed.
                sealed.insert(child.clone());
            }
            modules.insert(child, (depth + 1, list));
        }
    }
    modules.into_iter().map(|(k, (_, v))| (k, v)).collect()
}

/// The first line of prose in a README or a package comment.
fn purpose_of(text: &str) -> Option<String> {
    for line in text.lines().take(60) {
        let line = line.trim();
        let line = line
            .strip_prefix("//")
            .or_else(|| line.strip_prefix('#').filter(|_| false))
            .unwrap_or(line)
            .trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with('[')
            || line.starts_with('!')
            || line.starts_with('<')
            || line.starts_with("```")
            || line.starts_with("+build")
            || line.starts_with("go:build")
            || line.starts_with("Copyright")
            || line.starts_with("SPDX")
            || line.starts_with('=')
            || line.starts_with('-')
            || line.starts_with("package ")
            || line.starts_with("/*")
            || line.starts_with('*')
        {
            continue;
        }
        let mut purpose: String = line.chars().take(140).collect();
        if purpose.len() < line.len() {
            purpose.push_str("...");
        }
        return Some(purpose);
    }
    None
}

const ENTRY_NAMES: &[&str] = &[
    "main.go",
    "main.py",
    "__main__.py",
    "main.rs",
    "lib.rs",
    "index.ts",
    "index.js",
    "app.py",
    "manage.py",
    "Main.java",
    "main.c",
    "main.cc",
    "main.cpp",
];

fn modules_of(corpus: &mut Corpus<'_>) -> Modules {
    let parts = partition(&corpus.files);
    let mut module_of: HashMap<&str, usize> = HashMap::new();
    let names: Vec<String> = parts.keys().cloned().collect();
    for (i, list) in parts.values().enumerate() {
        for path in list {
            module_of.insert(path.as_str(), i);
        }
    }
    let mut modules = Vec::new();
    let mut keys: Vec<Vec<(usize, usize)>> = vec![Vec::new(); parts.len()];
    if let Some(graph) = &corpus.graph {
        for symbol in &graph.symbols {
            if let Some(&m) = module_of.get(symbol.path.as_str()) {
                keys[m].push((symbol.degree, graph.by_node[&symbol.node]));
            }
        }
    }
    let mut edges: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    let mut readmes = Vec::new();
    for (i, (prefix, list)) in parts.iter().enumerate() {
        let tokens: usize = list.iter().map(|p| est_file(p, corpus.files[p])).sum();
        let mut languages: BTreeMap<&'static str, usize> = BTreeMap::new();
        for path in list {
            let language = language(path);
            if !language.is_empty() && language != "markdown" {
                *languages.entry(language).or_default() += corpus.files[path];
            }
        }
        let language = languages
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(l, _)| l.to_string())
            .unwrap_or_default();
        let mut key = std::mem::take(&mut keys[i]);
        key.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut seen = BTreeSet::new();
        let mut key_names = Vec::new();
        let mut key_nodes = Vec::new();
        if let Some(graph) = &corpus.graph {
            for &(_, index) in &key {
                let symbol = &graph.symbols[index];
                if !short(&symbol.name).is_empty() && seen.insert(short(&symbol.name).to_string()) {
                    key_names.push(short(&symbol.name).to_string());
                    key_nodes.push(symbol.node);
                }
                if key_names.len() == 5 {
                    break;
                }
            }
            for &node in key_nodes.iter().take(3) {
                for target in (graph.dependencies)(node).into_iter().take(MAX_EDGES_READ) {
                    let Some(other) = graph.symbol_of(target) else {
                        continue;
                    };
                    if let Some(&m) = module_of.get(other.path.as_str())
                        && m != i
                    {
                        *edges.entry((i, m)).or_default() += 1;
                    }
                }
            }
        }
        let entries: Vec<String> = list
            .iter()
            .filter(|p| {
                let name = p.rsplit('/').next().unwrap_or(p);
                ENTRY_NAMES.contains(&name)
            })
            .take(3)
            .cloned()
            .collect();
        let readme = ["README.md", "README", "README.rst", "doc.go"]
            .iter()
            .map(|name| {
                if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}/{name}")
                }
            })
            .find(|p| corpus.files.contains_key(p));
        readmes.push(readme);
        modules.push(Module {
            path: if prefix.is_empty() {
                ".".into()
            } else {
                prefix.clone()
            },
            files: list.len(),
            tokens,
            language,
            purpose: String::new(),
            key: key_names,
            entries,
        });
    }
    for (module, readme) in modules.iter_mut().zip(readmes) {
        if let Some(path) = readme
            && let Some(text) = corpus.text(&path)
        {
            module.purpose = purpose_of(&text).unwrap_or_default();
        }
    }
    let mut edges: Vec<(String, String, usize)> = edges
        .into_iter()
        .map(|((a, b), n)| (names_or_dot(&names[a]), names_or_dot(&names[b]), n))
        .collect();
    edges.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    Modules {
        files: corpus.files.len(),
        tokens: corpus.files.iter().map(|(p, b)| est_file(p, *b)).sum(),
        modules,
        edges,
    }
}

fn names_or_dot(name: &str) -> String {
    if name.is_empty() {
        ".".into()
    } else {
        name.into()
    }
}

/// The architecture section's text, within `room` tokens.
fn architecture_text(modules: &Modules, room: usize, counter: &Counter) -> String {
    let mut text = format!(
        "{} files, about {} tokens (estimated), in {} folders. Each line: folder, files, tokens, main language, purpose, key symbols by connections in the code graph, entry points.\n\n",
        modules.files,
        modules.tokens,
        modules.modules.len()
    );
    let cost = |t: &str| counter.count(t, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0);
    if cost(&text) > room {
        return String::new();
    }
    let mut ordered: Vec<&Module> = modules.modules.iter().collect();
    ordered.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.path.cmp(&b.path)));
    for module in ordered {
        let mut line = format!(
            "- {}/: {} files, ~{} tokens",
            module.path, module.files, module.tokens
        );
        if !module.language.is_empty() {
            line.push_str(&format!(", {}", module.language));
        }
        if !module.purpose.is_empty() {
            line.push_str(&format!(". {}", module.purpose));
        }
        if !module.key.is_empty() {
            line.push_str(&format!(" Key: {}.", module.key.join(", ")));
        }
        if !module.entries.is_empty() {
            line.push_str(&format!(" Entry: {}.", module.entries.join(", ")));
        }
        line.push('\n');
        let next = format!("{text}{line}");
        if cost(&next) > room {
            return text;
        }
        text = next;
    }
    if !modules.edges.is_empty() {
        let head = "\nDependencies between folders (edges from the key symbols, most first):\n";
        let next = format!("{text}{head}");
        if cost(&next) > room {
            return text;
        }
        text = next;
        for (from, to, n) in modules.edges.iter().take(60) {
            let next = format!("{text}- {from}/ -> {to}/ ({n})\n");
            if cost(&next) > room {
                return text;
            }
            text = next;
        }
    }
    text
}

/// The architecture cache: one entry per project and code-graph episode.
static ARCHITECTURE: Mutex<Option<(String, Arc<Modules>)>> = Mutex::new(None);

fn cached_modules(key: &str) -> Option<Arc<Modules>> {
    let held = ARCHITECTURE.lock().unwrap_or_else(PoisonError::into_inner);
    held.as_ref()
        .filter(|(k, _)| k == key)
        .map(|(_, m)| m.clone())
}

fn cache_modules(key: String, modules: Arc<Modules>) {
    *ARCHITECTURE.lock().unwrap_or_else(PoisonError::into_inner) = Some((key, modules));
}

/// A symbol's range in a file of `total` lines whose node starts are
/// `starts`: from its line to the line before the next start.
fn span(line: usize, starts: &[usize], total: usize) -> (usize, usize) {
    let next = starts
        .iter()
        .find(|&&s| s > line)
        .map_or(total, |&s| s.saturating_sub(1));
    (line, next.max(line).min(total.max(line)))
}

/// Lines of one excerpt at most; a longer symbol is cut around its passage,
/// with its first line kept.
const MAX_EXCERPT: usize = 300;
/// Lines kept on each side of a passage in a cut symbol.
const CONTEXT: usize = 40;
/// Budgets up to this spend breadth's share on compact excerpts of the
/// ranks past [`FULL_RANKS`], as far down the ranking as the budget goes.
const SMALL_BUDGET: usize = 256_000;
const FULL_RANKS: usize = 15;
/// The cache layout's question part gives full excerpts to one ranked file
/// per this many tokens of its room ([`Sizing::question_part`]).
const QUESTION_FULL_TOKENS: usize = 8_000;
/// A compactly excerpted file goes in whole at most this size.
const COMPACT_WHOLE: usize = 1_500;
/// Lines around a passage in a compact excerpt.
const COMPACT_CONTEXT: usize = 3;
/// Top-ranked files left out that the manifest lists at most.
const MANIFEST_OMITTED: usize = 100;

/// A file's excerpt ranges for its passages, best passage first, within
/// `cap` tokens (estimated from the lines' bytes at `rate`): each passage
/// widened to its symbols ([`excerpt_ranges`]), or with `compact` only a few
/// lines around it and the first line of each symbol it overlaps. The
/// first passage always goes in, cut to the cap.
fn capped_ranges(
    passages: &[(usize, usize)],
    symbols: &[Symbol],
    starts: &[usize],
    lines: &[&str],
    rate: f64,
    cap: usize,
    compact: bool,
) -> Vec<(usize, usize)> {
    let total = lines.len();
    let cost = |ranges: &[(usize, usize)]| -> usize {
        ranges
            .iter()
            .map(|&(a, b)| {
                let bytes: usize = lines[a - 1..b].iter().map(|l| l.len() + 1).sum();
                estimate::tokens(bytes, rate) + (b - a + 1)
            })
            .sum()
    };
    let mut kept: Vec<(usize, usize)> = Vec::new();
    for &(a, b) in passages {
        let (a, b) = (a.clamp(1, total.max(1)), b.clamp(1, total.max(1)));
        if total == 0 {
            break;
        }
        let pieces = if compact {
            let mut pieces = vec![(
                a.saturating_sub(COMPACT_CONTEXT).max(1),
                (b + COMPACT_CONTEXT).min(total),
            )];
            for symbol in symbols {
                let (s, e) = span(symbol.line, starts, total);
                if s <= b && e >= a && s <= total {
                    pieces.push((s, s));
                }
            }
            pieces
        } else {
            excerpt_ranges(&[(a, b)], symbols, starts, total)
        };
        let mut next = kept.clone();
        next.extend(pieces);
        let next = merge(next);
        if cost(&next) <= cap {
            kept = next;
        } else if kept.is_empty() {
            // The first passage alone, cut to the cap.
            let mut end = a;
            while end < b && cost(&[(a, end + 1)]) <= cap {
                end += 1;
            }
            kept = vec![(a, end)];
        }
    }
    kept
}

/// The excerpt ranges of a large file for its passages: each passage widened
/// to the symbols it overlaps.
fn excerpt_ranges(
    passages: &[(usize, usize)],
    symbols: &[Symbol],
    starts: &[usize],
    total: usize,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for &(a, b) in passages {
        let (a, b) = (a.min(total.max(1)), b.min(total.max(1)));
        let mut from = a;
        let mut to = b;
        let mut first = None;
        for symbol in symbols {
            let (s, e) = span(symbol.line, starts, total);
            if s <= b && e >= a {
                from = from.min(s);
                to = to.max(e);
                first.get_or_insert(s);
            }
        }
        if to - from + 1 > MAX_EXCERPT {
            let cut = (a.saturating_sub(CONTEXT).max(from), (b + CONTEXT).min(to));
            if let Some(s) = first
                && s < cut.0
            {
                ranges.push((s, s));
            }
            ranges.push(cut);
        } else {
            ranges.push((from, to));
        }
    }
    merge(ranges)
}

/// The names of the symbols that start in `a..=b`, most connected first.
fn names_in(symbols: &[Symbol], a: usize, b: usize) -> Vec<String> {
    let mut inside: Vec<&Symbol> = symbols
        .iter()
        .filter(|s| s.line >= a && s.line <= b)
        .collect();
    inside.sort_by(|x, y| y.degree.cmp(&x.degree).then(x.line.cmp(&y.line)));
    let mut names = Vec::new();
    for symbol in inside {
        let name = short(&symbol.name).to_string();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
        if names.len() == 3 {
            break;
        }
    }
    names
}

fn defines(names: &[String]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!(", defines {}", names.join(", "))
    }
}

/// Test file names that go with `path`, by language convention.
fn test_names(path: &str) -> Vec<String> {
    let (folder, name) = match path.rsplit_once('/') {
        Some((folder, name)) => (format!("{folder}/"), name),
        None => (String::new(), path),
    };
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return Vec::new();
    };
    match ext {
        "go" => vec![format!("{folder}{stem}_test.go")],
        "py" => vec![
            format!("{folder}test_{stem}.py"),
            format!("{folder}{stem}_test.py"),
            format!("{folder}tests/test_{stem}.py"),
        ],
        "ts" | "tsx" | "js" | "jsx" => vec![
            format!("{folder}{stem}.test.{ext}"),
            format!("{folder}{stem}.spec.{ext}"),
            format!("{folder}__tests__/{stem}.test.{ext}"),
        ],
        "rs" => vec![format!("{folder}tests.rs"), format!("tests/{stem}.rs")],
        "java" => vec![format!(
            "{}{stem}Test.java",
            folder.replace("/main/", "/test/")
        )],
        _ => Vec::new(),
    }
}

fn is_test(path: &str) -> bool {
    class(path) == crate::code_question::PathClass::Test
}

/// Relations a test's reference to a symbol travels.
const REFERENCES: &[&str] = &[
    "calls",
    "references",
    "uses",
    "inherits",
    "implements",
    "extends",
];

/// Build the pack of `corpus` for `options`, as the JSON format without
/// `seconds`.
pub fn build(corpus: &mut Corpus<'_>, options: &Options, counter: &Counter) -> Result<Value> {
    match options.layout {
        Layout::Standard => build_standard(corpus, options, counter),
        Layout::Cache => build_cache(corpus, options, counter),
    }
}

/// The cost of each section's opening and closing chunks, counted as the
/// two chunks they are.
fn frames(style: Style, counter: &Counter) -> BTreeMap<Section, usize> {
    let cost = |text: &str| {
        if text.is_empty() {
            0
        } else {
            counter.count(text, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0)
        }
    };
    Section::ALL
        .into_iter()
        .map(|section| {
            (
                section,
                cost(&section_open(style, section)) + cost(&section_close(style)),
            )
        })
        .collect()
}

/// What the question's sections are sized by: the budget they share.
struct Sizing {
    budget: usize,
    /// At small budgets the ranks past [`FULL_RANKS`] get compact excerpts
    /// (their passages and the first lines of the symbols around them), and
    /// breadth fills by rank only.
    small: bool,
    /// The most one focus file takes.
    per_file: usize,
    /// A focus file goes in whole up to this size.
    whole_limit: usize,
    /// The ranks that get full excerpts at small budgets; the ranks past
    /// them get compact ones.
    full_ranks: usize,
}

impl Sizing {
    fn new(budget: usize, overview: bool) -> Self {
        let small = budget <= SMALL_BUDGET && !overview;
        Self {
            budget,
            small,
            per_file: (budget / if small { 10 } else { 8 }).max(2_000),
            whole_limit: (budget / 16).clamp(1_500, 16_000),
            full_ranks: FULL_RANKS,
        }
    }

    /// The cache layout's question part, sized by its `room`: full excerpts
    /// for one ranked file per [`QUESTION_FULL_TOKENS`] of the room (3 to
    /// [`FULL_RANKS`]), together at most two fifths of it, and compact ones
    /// past them, so that a small question part still reaches far down the
    /// ranking.
    fn question_part(room: usize) -> Self {
        let mut sizing = Self::new(room, false);
        sizing.full_ranks = (room / QUESTION_FULL_TOKENS).clamp(3, FULL_RANKS);
        sizing.per_file = (room * 2 / 5 / sizing.full_ranks).max(2_000);
        sizing
    }

    fn share(&self, part: f64) -> usize {
        ((self.budget as f64) * part) as usize
    }
}

/// The ranked files for the question: none without one.
fn question_ranking(corpus: &mut Corpus<'_>, options: &Options) -> Vec<Ranked> {
    if options.question.is_empty() {
        Vec::new()
    } else {
        let base = ranked_files(corpus);
        augment(corpus, &ranking_question(&options.question), base)
    }
}

static HEADING: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^ {0,3}(#{1,6})[ \t]+(.*?)[ \t]*#*[ \t]*$").unwrap());
/// A heading's parenthesized note, as in "Container runtime (CRI) and
/// version (if applicable)".
static HEADING_NOTE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\([^()]*\)").unwrap());
/// A short heading that labels the environment, read in lower case without
/// its notes and punctuation: the system information, the environment, a
/// `<name> version` or the tested versions, the cloud provider, install
/// tools, the container runtime, related plugins, the operating system,
/// hardware or the type of installation, or several of them ("Container
/// runtime and version").
static ENVIRONMENT_LABEL: LazyLock<regex::Regex> = LazyLock::new(|| {
    let label = concat!(
        r"(?:system (?:information|info|details)",
        r"|(?:[\w+/-]+ )?environment(?: (?:information|info|details))?",
        r"|(?:[\w+/-]+ ){0,3}versions?",
        r"|versions? (?:affected|used|tested|info|information|details)",
        r"|cloud provider|install tools|container runtime|related plugins",
        r"|operating system|os|hardware|type of installation)"
    );
    regex::Regex::new(&format!(r"^{label}(?:(?: and | & |, ?| ?/ ?){label})*$")).unwrap()
});
/// An issue form's question for the environment, as a heading in lower
/// case: "What version of Home Assistant Core has the issue?", "Please
/// provide the environment you discovered this bug in". It must also name
/// an `ENVIRONMENT_WORD`.
static ENVIRONMENT_QUESTION: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^(?:what|which)\b.*\?$|\bplease (?:provide|specify|share|list|include)\b")
        .unwrap()
});
/// A word that names the environment, for `ENVIRONMENT_QUESTION`.
static ENVIRONMENT_WORD: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"\b(?:versions?|environment|operating system|os|type of installation|installation type)\b",
    )
    .unwrap()
});
/// Words that make a short heading or a version line a sentence, not a
/// label: "Broken in the new version", "In the new version: it fails".
const SENTENCE_WORDS: [&str; 14] = [
    "in", "on", "at", "to", "from", "after", "since", "with", "for", "by", "before", "when", "is",
    "not",
];

/// Whether a markdown heading opens an environment section: a short label
/// (five words or fewer without its parenthesized notes) such as "System
/// information", "Tested versions", "Godot version", "OS" or "Container
/// runtime (CRI) and version (if applicable)", or an issue form's question
/// for the environment. A heading that merely names a version ("Upgrade to
/// version 2 breaks login") does not, and a level-1 heading never does: it
/// is the issue's title.
fn environment_heading(level: usize, heading: &str) -> bool {
    if level < 2 {
        return false;
    }
    let heading = heading.to_lowercase();
    let label: String = HEADING_NOTE
        .replace_all(&heading, " ")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "+/-&,".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    let label = label.trim_matches(|c: char| " ,-&/".contains(c));
    let words = || label.split(' ').filter(|word| !word.is_empty());
    if words().count() <= 5
        && ENVIRONMENT_LABEL.is_match(label)
        && !words().any(|word| SENTENCE_WORDS.contains(&word))
    {
        return true;
    }
    let asked = heading.replace(['*', '`'], "");
    let asked = asked.trim();
    ENVIRONMENT_QUESTION.is_match(asked) && ENVIRONMENT_WORD.is_match(asked)
}

/// A `<label> version: <value>` line: at most three words of a label at the
/// start of the line, the word version or versions (or "version affected",
/// ...) and a colon, with a list marker and emphasis around the label
/// allowed. Group 1 is the label's words.
static VERSION_LINE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"(?i)^[ \t]*(?:[-*+][ \t]+)?(?:\*\*|__|\*|_)?((?:[A-Za-z0-9./+_-]+[ \t]+){0,3})",
        r"versions?(?:[ \t]+(?:affected|used|tested|info|information|details))?",
        r"[ \t]*(?:\*\*|__|\*|_)?[ \t]*:"
    ))
    .unwrap()
});

/// Whether a line is a `<label> version: <value>` line ("- VS Code Version:
/// 1.95", "**Godot version:** 4.2"), not prose ("Version check fails: ...",
/// "In the new version: ...") or code (`def version(self):`).
fn version_line(line: &str) -> bool {
    VERSION_LINE.captures(line).is_some_and(|line| {
        !line[1]
            .split_whitespace()
            .any(|word| SENTENCE_WORDS.contains(&word.to_lowercase().as_str()))
    })
}

/// The question as the ranking reads it: an issue without its environment
/// sections (a heading below the title that labels the system information,
/// the tested versions or a `<name> version`, the environment, the cloud
/// provider, install tools, the container runtime, related plugins, the
/// operating system, hardware or the type of installation, see
/// `environment_heading`, down to the next heading of its level) and
/// without its `<name> version:` lines. They say where the reporter ran the
/// program, not what the code does, and their words (graphics drivers,
/// display servers, package versions) pull unrelated files up. A level-1
/// heading (the title, or a `# comment` line of a code block) never opens
/// an environment section. A question without environment sections or
/// version lines is returned as it is.
fn ranking_question(question: &str) -> std::borrow::Cow<'_, str> {
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping: Option<usize> = None;
    let mut changed = false;
    for line in question.split('\n') {
        // A line read without its `\r` (Windows line ends), kept as it is.
        let read = line.strip_suffix('\r').unwrap_or(line);
        if let Some(heading) = HEADING.captures(read) {
            let level = heading[1].len();
            if skipping.is_some_and(|at| level <= at) {
                skipping = None;
            }
            if skipping.is_none() && environment_heading(level, &heading[2]) {
                skipping = Some(level);
                changed = true;
                continue;
            }
        }
        if skipping.is_some() || version_line(read) {
            changed = true;
            continue;
        }
        kept.push(line);
    }
    if changed {
        let kept = kept.join("\n");
        if kept.trim().is_empty() {
            // Every line was in an environment section or a version line:
            // rank on the question as written.
            return std::borrow::Cow::Borrowed(question);
        }
        std::borrow::Cow::Owned(kept)
    } else {
        std::borrow::Cow::Borrowed(question)
    }
}

/// The top ranked code files (the tests and READMEs that go with them).
fn top_code_of(ranked: &[Ranked]) -> Vec<Ranked> {
    ranked
        .iter()
        .filter(|r| class(&r.path) != crate::code_question::PathClass::Doc)
        .take(10)
        .cloned()
        .collect()
}

/// The focus symbols: those the top ranked code files' passages overlap
/// (the call paths and tests start from them, whatever the focus section
/// holds).
fn focus_symbols_of(corpus: &mut Corpus<'_>, ranked: &[Ranked]) -> Vec<usize> {
    let mut focus_symbols: Vec<usize> = Vec::new();
    if corpus.graph.is_some() {
        for file in ranked
            .iter()
            .filter(|r| class(&r.path) != crate::code_question::PathClass::Doc)
        {
            if focus_symbols.len() >= MAX_FOCUS_SYMBOLS {
                break;
            }
            let Some(text) = corpus.text(&file.path) else {
                continue;
            };
            let total = text.lines().count();
            let Some(graph) = corpus.graph.as_ref() else {
                break;
            };
            let starts = graph.starts(&file.path);
            for symbol in graph.symbols(&file.path) {
                let (s, e) = span(symbol.line, &starts, total);
                if file.passages.iter().any(|&(a, b)| s <= b && e >= a)
                    && !short(&symbol.name).is_empty()
                    && !focus_symbols.contains(&symbol.node)
                    && focus_symbols.len() < MAX_FOCUS_SYMBOLS
                {
                    focus_symbols.push(symbol.node);
                }
            }
        }
    }
    focus_symbols
}

/// The architecture section within `cap`.
fn add_architecture(fill: &mut Fill<'_>, corpus: &mut Corpus<'_>, cap: usize) {
    let counter = fill.counter;
    let room = fill.room(Section::Architecture, cap);
    let modules = match corpus.modules.clone() {
        Some(modules) => modules,
        None => {
            let built = Arc::new(modules_of(corpus));
            corpus.modules = Some(built.clone());
            built
        }
    };
    let body = architecture_text(&modules, room, counter);
    if !body.is_empty() {
        let tokens = counter.count(&body, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0);
        fill.add(
            Section::Architecture,
            cap,
            Chunk {
                path: None,
                start: 0,
                end: 0,
                whole: false,
                why: "architecture".into(),
                text: body,
                tokens,
                order: usize::MAX,
            },
            None,
        );
    }
}

/// The sum of the connections of each file's symbols in the code graph.
fn centrality_of(corpus: &Corpus<'_>) -> HashMap<String, usize> {
    match &corpus.graph {
        Some(graph) => {
            let mut sums: HashMap<String, usize> = HashMap::new();
            for (path, &(from, to)) in &graph.by_path {
                sums.insert(
                    path.clone(),
                    graph.symbols[from..to].iter().map(|s| s.degree).sum(),
                );
            }
            sums
        }
        None => HashMap::new(),
    }
}

fn build_standard(corpus: &mut Corpus<'_>, options: &Options, counter: &Counter) -> Result<Value> {
    let budget = options.budget;
    let limit = budget - ((budget as f64) * counter.margin()).ceil() as usize;
    let mut fill = Fill::new(
        options,
        counter,
        limit,
        frames(options.style, counter),
        options.on(Section::Manifest),
    );
    let overview = options.question.is_empty();
    let ranked = question_ranking(corpus, options);
    let rank_of: HashMap<String, usize> = ranked.iter().map(|r| (r.path.clone(), r.rank)).collect();
    // The header's room, from its text at the largest numbers.
    let header_reserve = counter.count(
        &header_text(corpus, options, counter, budget, &rank_of),
        estimate::PACK_TEXT_BYTES_PER_TOKEN,
        0,
        0,
    ) + fill.frame[&Section::Header]
        + 8;
    fill.used += header_reserve;
    // The manifest's frame, title line and left-out list.
    let omitted_reserve = if options.on(Section::Manifest) {
        let lines = (budget / 600).clamp(3, MANIFEST_OMITTED);
        fill.frame[&Section::Manifest] + 40 + lines * 24
    } else {
        0
    };
    fill.used += omitted_reserve;
    if fill.used > limit {
        return Err(Error::bad(format!(
            "The budget of {budget} tokens leaves no room after the header"
        )));
    }
    let sizing = Sizing::new(budget, overview);
    let share = |part: f64| sizing.share(part);

    // Architecture.
    if options.on(Section::Architecture) {
        add_architecture(&mut fill, corpus, share(if overview { 0.20 } else { 0.04 }));
    }

    let top_code = top_code_of(&ranked);
    let focus_symbols = focus_symbols_of(corpus, &ranked);

    // Focus.
    if options.on(Section::Focus) && !ranked.is_empty() {
        let reserved: usize = [
            (Section::Paths, 0.10),
            (Section::Tests, 0.10),
            (Section::Docs, 0.05),
        ]
        .iter()
        .filter(|(s, _)| options.on(*s))
        .map(|(_, p)| share(*p))
        .sum();
        let left = limit.saturating_sub(fill.used);
        let cap = left.saturating_sub(reserved).max(share(0.5).min(left));
        fill_focus(&mut fill, corpus, &ranked, cap, &sizing, None);
    }

    // Call paths.
    if options.on(Section::Paths) && !focus_symbols.is_empty() && corpus.graph.is_some() {
        fill_paths(&mut fill, corpus, &focus_symbols, &rank_of, share(0.10));
    }

    // Tests.
    if options.on(Section::Tests) && !ranked.is_empty() {
        fill_tests(
            &mut fill,
            corpus,
            &focus_symbols,
            &top_code,
            &rank_of,
            share(0.10),
            sizing.whole_limit,
        );
    }

    // Docs.
    if options.on(Section::Docs) && !ranked.is_empty() {
        let cap = share(0.05);
        for file in &ranked {
            if class(&file.path) != crate::code_question::PathClass::Doc {
                continue;
            }
            if fill.room(Section::Docs, cap) < 64 {
                break;
            }
            let Some(text) = corpus.text(&file.path) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            let ranges: Vec<(usize, usize)> = file
                .passages
                .iter()
                .map(|&(a, b)| (a, b.min(a + 119)))
                .collect();
            let rank = file.rank;
            let why = move |_: usize, _: usize| format!("docs: rank {rank}");
            fill.add_ranges(
                Section::Docs,
                cap,
                &file.path,
                &lines,
                &ranges,
                &why,
                Some(rank),
                usize::MAX,
            );
        }
        // The nearest README of the top ranked code files.
        let tops: Vec<String> = top_code.iter().take(3).map(|r| r.path.clone()).collect();
        let mut seen = BTreeSet::new();
        for top in tops {
            let mut folder = top.rsplit_once('/').map(|(f, _)| f.to_string());
            while let Some(current) = folder {
                let candidate = format!("{current}/README.md");
                if corpus.files.contains_key(&candidate) {
                    if seen.insert(candidate.clone())
                        && fill.room(Section::Docs, cap) >= 64
                        && let Some(text) = corpus.text(&candidate)
                    {
                        let lines: Vec<&str> = text.lines().collect();
                        let why = format!("docs: README of {current}/");
                        let why = move |_: usize, _: usize| why.clone();
                        let end = lines.len().min(80);
                        let rank = rank_of.get(&candidate).copied();
                        fill.add_ranges(
                            Section::Docs,
                            cap,
                            &candidate,
                            &lines,
                            &[(1, end)],
                            &why,
                            rank,
                            usize::MAX,
                        );
                    }
                    break;
                }
                folder = current.rsplit_once('/').map(|(f, _)| f.to_string());
            }
        }
    }

    // Breadth.
    if options.on(Section::Breadth) {
        let small = sizing.small;
        let centrality = centrality_of(corpus);
        let mut candidates: Vec<(String, String)> = Vec::new(); // (path, why)
        let mut queued: BTreeSet<String> = BTreeSet::new();
        for file in &ranked {
            if queued.insert(file.path.clone()) {
                candidates.push((file.path.clone(), format!("breadth: rank {}", file.rank)));
            }
        }
        // Small budgets fill by rank only; larger ones then take the top
        // files' folders and the rest of the project by centrality.
        let mut folders: Vec<String> = Vec::new();
        for file in ranked.iter().take(if small { 0 } else { 10 }) {
            let folder = file
                .path
                .rsplit_once('/')
                .map_or("", |(f, _)| f)
                .to_string();
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        let by_centrality = |paths: &mut Vec<&String>| {
            paths.sort_by(|a, b| {
                let ca = centrality.get(a.as_str()).copied().unwrap_or(0);
                let cb = centrality.get(b.as_str()).copied().unwrap_or(0);
                cb.cmp(&ca).then(a.cmp(b))
            });
        };
        for folder in &folders {
            let mut near: Vec<&String> = corpus
                .files
                .keys()
                .filter(|p| p.rsplit_once('/').map_or("", |(f, _)| f) == folder.as_str())
                .filter(|p| is_code(p) && !is_test(p) && !generated(p))
                .collect();
            by_centrality(&mut near);
            for path in near {
                if queued.insert(path.clone()) {
                    let shown = if folder.is_empty() {
                        "."
                    } else {
                        folder.as_str()
                    };
                    candidates.push((
                        path.clone(),
                        format!("breadth: same folder as the focus ({shown}/)"),
                    ));
                }
            }
        }
        let mut rest: Vec<&String> = corpus
            .files
            .keys()
            .filter(|p| {
                !small
                    && is_code(p)
                    && !generated(p)
                    && matches!(class(p), crate::code_question::PathClass::Source)
            })
            .collect();
        by_centrality(&mut rest);
        for path in rest {
            if queued.insert(path.clone()) {
                let c = centrality.get(path.as_str()).copied().unwrap_or(0);
                candidates.push((path.clone(), format!("breadth: centrality {c}")));
            }
        }
        fill_breadth(
            &mut fill,
            corpus,
            candidates,
            &rank_of,
            sizing.whole_limit * 2,
        );
    }

    finish(
        corpus,
        options,
        counter,
        fill,
        &ranked,
        &rank_of,
        header_reserve,
    )
}

/// The focus section within `cap`: the ranked code files, small ones whole,
/// large ones as excerpts of their passages' symbols. A file that
/// `in_prefix` holds whole (the cache layout's repository part, by path
/// with its line count) is not repeated: the section's first chunk refers
/// to it.
fn fill_focus(
    fill: &mut Fill<'_>,
    corpus: &mut Corpus<'_>,
    ranked: &[Ranked],
    cap: usize,
    sizing: &Sizing,
    in_prefix: Option<&BTreeMap<String, usize>>,
) {
    let (small, per_file, whole_limit) = (sizing.small, sizing.per_file, sizing.whole_limit);
    let full_ranks = sizing.full_ranks;
    let mut references = References::default();
    let mut misses = 0;
    for file in ranked {
        if class(&file.path) == crate::code_question::PathClass::Doc {
            continue;
        }
        if fill.room(Section::Focus, cap) < 64 || misses >= 40 {
            break;
        }
        if let Some(&lines) = in_prefix.and_then(|held| held.get(&file.path)) {
            references.note(fill, cap, &file.path, lines, file.rank);
            continue;
        }
        let Some(text) = corpus.text(&file.path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() || minified(text.len(), lines.len()) {
            continue;
        }
        let total = lines.len();
        let symbols: Vec<Symbol> = corpus
            .graph
            .as_ref()
            .map(|g| g.symbols(&file.path).to_vec())
            .unwrap_or_default();
        let starts = corpus
            .graph
            .as_ref()
            .map(|g| g.starts(&file.path))
            .unwrap_or_default();
        let rank = file.rank;
        let path = file.path.clone();
        let est = est_file(&file.path, text.len());
        let compact = small && rank > full_ranks;
        let note = file.added.map_or(String::new(), |why| format!(" ({why})"));
        let whole_cap = if compact { COMPACT_WHOLE } else { whole_limit };
        let mut added = 0;
        if est <= whole_cap.min(per_file) {
            let names = names_in(&symbols, 1, total);
            let note = note.clone();
            let why =
                move |_: usize, _: usize| format!("focus: rank {rank}{note}{}", defines(&names));
            added = fill.add_ranges(
                Section::Focus,
                cap,
                &path,
                &lines,
                &[(1, total)],
                &why,
                Some(rank),
                usize::MAX,
            );
        }
        if added == 0 {
            // A file the ranking added has no passages: its head.
            let passages = if file.passages.is_empty() {
                vec![(1, total.min(60))]
            } else {
                file.passages.clone()
            };
            let ranges = capped_ranges(
                &passages,
                &symbols,
                &starts,
                &lines,
                rate_of(&path),
                per_file,
                compact,
            );
            let symbols_ref = &symbols;
            let why = move |a: usize, b: usize| {
                format!(
                    "focus: rank {rank}{note}{}",
                    defines(&names_in(symbols_ref, a, b))
                )
            };
            added = fill.add_ranges(
                Section::Focus,
                cap,
                &path,
                &lines,
                &ranges,
                &why,
                Some(rank),
                usize::MAX,
            );
            if added == 0 {
                // The bare passages, when their symbols do not fit.
                added = fill.add_ranges(
                    Section::Focus,
                    cap,
                    &path,
                    &lines,
                    &passages,
                    &why,
                    Some(rank),
                    usize::MAX,
                );
            }
        }
        if added == 0 {
            misses += 1;
        } else {
            misses = 0;
        }
    }
    references.finish(fill, cap);
}

/// References at most, in the focus section, to ranked files the
/// repository part holds whole.
const MAX_REFERENCES: usize = 60;
const REFERENCES_HEAD: &str =
    "Ranked files the repository part already holds whole (not repeated here):\n";

/// The focus section's references to the repository part, charged against
/// the budget as they are noted and added as one chunk at the end.
#[derive(Default)]
struct References {
    lines: Vec<String>,
    charged: usize,
}

impl References {
    fn note(&mut self, fill: &mut Fill<'_>, cap: usize, path: &str, lines: usize, rank: usize) {
        if self.lines.len() >= MAX_REFERENCES {
            return;
        }
        let line = format!("- in the prefix: {path} lines 1-{lines} (rank {rank})\n");
        let count = |text: &str| {
            fill.counter
                .count(text, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0)
        };
        let mut cost = count(&line);
        if self.lines.is_empty() {
            // The head line and the blank line after the list.
            cost += count(REFERENCES_HEAD) + 1;
        }
        if cost > fill.room(Section::Focus, cap) {
            return;
        }
        fill.used += cost;
        *fill.spent.entry(Section::Focus).or_default() += cost;
        self.charged += cost;
        self.lines.push(line);
    }

    fn finish(self, fill: &mut Fill<'_>, cap: usize) {
        if self.lines.is_empty() {
            return;
        }
        fill.used -= self.charged;
        *fill.spent.entry(Section::Focus).or_default() -= self.charged;
        let text = format!("{REFERENCES_HEAD}{}\n", self.lines.concat());
        let tokens = fill
            .counter
            .count(&text, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0);
        fill.add(
            Section::Focus,
            cap,
            Chunk {
                path: None,
                start: 0,
                end: 0,
                whole: false,
                why: "in the prefix".into(),
                text,
                tokens,
                order: 0,
            },
            None,
        );
    }
}

/// The call paths section within `cap`: the lines, then the bodies of the
/// key hops not already in the pack.
fn fill_paths(
    fill: &mut Fill<'_>,
    corpus: &mut Corpus<'_>,
    focus_symbols: &[usize],
    rank_of: &HashMap<String, usize>,
    cap: usize,
) {
    let counter = fill.counter;
    let (lines, hops) = call_paths(corpus.graph.as_ref().unwrap(), focus_symbols, rank_of);
    if lines.is_empty() {
        return;
    }
    let mut body = String::from(
        "Callers and callees of the focus symbols along call edges, two steps at most: a (path:line) -> b (path:line).\n\n",
    );
    let cost = |t: &str| counter.count(t, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0);
    let room = fill.room(Section::Paths, cap);
    let mut kept = 0;
    for line in &lines {
        let next = format!("{body}{line}\n");
        if cost(&next) > room / 2 {
            break;
        }
        body = next;
        kept += 1;
    }
    if kept == 0 {
        return;
    }
    body.push('\n');
    let tokens = cost(&body);
    fill.add(
        Section::Paths,
        cap,
        Chunk {
            path: None,
            start: 0,
            end: 0,
            whole: false,
            why: "call paths".into(),
            text: body,
            tokens,
            order: 0,
        },
        None,
    );
    // The bodies of the key hops not already in the pack.
    for (node, why) in hops {
        if fill.room(Section::Paths, cap) < 64 {
            break;
        }
        let Some(symbol) = corpus.graph.as_ref().unwrap().symbol_of(node).cloned() else {
            continue;
        };
        let Some(text) = corpus.text(&symbol.path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let starts = corpus.graph.as_ref().unwrap().starts(&symbol.path);
        let (a, b) = span(symbol.line, &starts, lines.len());
        let (a, b) = (a, b.min(a + MAX_EXCERPT - 1));
        let rank = rank_of.get(&symbol.path).copied();
        let why = move |_: usize, _: usize| why.clone();
        fill.add_ranges(
            Section::Paths,
            cap,
            &symbol.path,
            &lines,
            &[(a, b)],
            &why,
            rank,
            usize::MAX,
        );
    }
}

/// The tests section within `cap`: tests that reach the focus symbols in
/// the code graph, then the test files named after the top code files.
#[allow(clippy::too_many_arguments)]
fn fill_tests(
    fill: &mut Fill<'_>,
    corpus: &mut Corpus<'_>,
    focus_symbols: &[usize],
    top_code: &[Ranked],
    rank_of: &HashMap<String, usize>,
    cap: usize,
    whole_limit: usize,
) {
    let mut tests: Vec<(usize, Vec<String>)> = Vec::new(); // (node, focus names)
    if let Some(graph) = &corpus.graph {
        let mut by_test: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for &focus in focus_symbols {
            let Some(target) = graph.symbol_of(focus) else {
                continue;
            };
            for edge in graph.edges(focus).iter() {
                if !edge.incoming || !REFERENCES.contains(&edge.relation.as_str()) {
                    continue;
                }
                if let Some(test) = graph.symbol_of(edge.other)
                    && is_test(&test.path)
                {
                    let names = by_test.entry(edge.other).or_default();
                    let name = short(&target.name).to_string();
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        tests = by_test.into_iter().collect();
        tests.sort_by(|a, b| {
            let (sa, sb) = (graph.symbol_of(a.0).unwrap(), graph.symbol_of(b.0).unwrap());
            b.1.len()
                .cmp(&a.1.len())
                .then((sa.path.as_str(), sa.line).cmp(&(sb.path.as_str(), sb.line)))
        });
    }
    for (node, names) in tests {
        if fill.room(Section::Tests, cap) < 64 {
            break;
        }
        let Some(symbol) = corpus.graph.as_ref().unwrap().symbol_of(node).cloned() else {
            continue;
        };
        let Some(text) = corpus.text(&symbol.path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let starts = corpus.graph.as_ref().unwrap().starts(&symbol.path);
        let (a, b) = span(symbol.line, &starts, lines.len());
        let (a, b) = (a, b.min(a + MAX_EXCERPT - 1));
        let rank = rank_of.get(&symbol.path).copied();
        let why = format!("test of {}", names.join(", "));
        let why = move |_: usize, _: usize| why.clone();
        fill.add_ranges(
            Section::Tests,
            cap,
            &symbol.path,
            &lines,
            &[(a, b)],
            &why,
            rank,
            usize::MAX,
        );
    }
    // By name: the test files that go with the top ranked code files.
    for file in top_code {
        if fill.room(Section::Tests, cap) < 64 {
            break;
        }
        for candidate in test_names(&file.path) {
            if !corpus.files.contains_key(&candidate) || !fill.covered(&candidate).is_empty() {
                continue;
            }
            let Some(text) = corpus.text(&candidate) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            if lines.is_empty() {
                continue;
            }
            let est = est_file(&candidate, text.len());
            let names: Vec<String> = corpus
                .graph
                .as_ref()
                .map(|g| {
                    g.symbols(&file.path)
                        .iter()
                        .map(|s| s.name.rsplit('.').next().unwrap_or(&s.name).to_string())
                        .collect()
                })
                .unwrap_or_default();
            let ranges: Vec<(usize, usize)> = if est <= whole_limit / 2 {
                vec![(1, lines.len())]
            } else if let Some(graph) = &corpus.graph {
                let starts = graph.starts(&candidate);
                graph
                    .symbols(&candidate)
                    .iter()
                    .filter(|t| {
                        names
                            .iter()
                            .any(|n| n.len() > 2 && t.name.contains(n.as_str()))
                    })
                    .map(|t| {
                        let (a, b) = span(t.line, &starts, lines.len());
                        (a, b.min(a + MAX_EXCERPT - 1))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            if ranges.is_empty() {
                continue;
            }
            let why = format!("test of {} (by file name)", file.path);
            let why = move |_: usize, _: usize| why.clone();
            let rank = rank_of.get(&candidate).copied();
            fill.add_ranges(
                Section::Tests,
                cap,
                &candidate,
                &lines,
                &ranges,
                &why,
                rank,
                usize::MAX,
            );
        }
    }
}

/// Breadth: `candidates` (path, why) in order, each whole when it is at
/// most `breadth_whole` tokens and fits, else an outline (the first line of
/// each symbol), until the budget is spent.
fn fill_breadth(
    fill: &mut Fill<'_>,
    corpus: &mut Corpus<'_>,
    candidates: Vec<(String, String)>,
    rank_of: &HashMap<String, usize>,
    breadth_whole: usize,
) {
    let cap = fill.limit;
    let mut misses = 0;
    for (path, why) in candidates {
        let room = fill.room(Section::Breadth, cap);
        if room < 48 || misses >= 400 {
            break;
        }
        if !fill.covered(&path).is_empty() {
            continue;
        }
        let bytes = corpus.files[&path];
        let est = est_file(&path, bytes);
        let outline_only = est > breadth_whole || est > room;
        if outline_only
            && corpus
                .graph
                .as_ref()
                .is_none_or(|g| g.symbols(&path).is_empty())
        {
            misses += 1;
            continue;
        }
        let Some(text) = corpus.text(&path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() || minified(text.len(), lines.len()) {
            continue;
        }
        let rank = rank_of.get(&path).copied();
        let added = if outline_only {
            let graph = corpus.graph.as_ref().unwrap();
            let ranges: Vec<(usize, usize)> = graph
                .symbols(&path)
                .iter()
                .map(|s| (s.line, s.line))
                .collect();
            let reason = format!("{why} (outline: the first line of each symbol)");
            let ranges = merge_exact(ranges);
            let piece_ranges: Vec<(usize, usize)> = ranges
                .into_iter()
                .map(|(a, b)| (a.min(lines.len()), b.min(lines.len())))
                .filter(|&(a, b)| a >= 1 && a <= b)
                .collect();
            outline(fill, &path, &lines, &piece_ranges, &reason, rank)
        } else {
            let reason = why.clone();
            let f = move |_: usize, _: usize| reason.clone();
            fill.add_ranges(
                Section::Breadth,
                cap,
                &path,
                &lines,
                &[(1, lines.len())],
                &f,
                rank,
                usize::MAX,
            ) > 0
        };
        if added {
            misses = 0;
        } else {
            misses += 1;
        }
    }
}

/// The cache layout's repository part: its sections with their chunks
/// (opening and closing ones included), its text and tokens, and what it
/// holds.
struct Prefix {
    sections: Vec<(Section, Vec<Chunk>)>,
    text: String,
    tokens: usize,
    /// The sum of its chunks' tokens.
    sum: usize,
    files: BTreeMap<String, Included>,
    /// The files it holds whole, with their line counts.
    whole: BTreeMap<String, usize>,
    /// The lines it holds by file, outlines left out: the question part
    /// does not repeat them.
    held: HashMap<String, Vec<(usize, usize)>>,
}

/// The repository part's sections, in their order.
const PREFIX_SECTIONS: [Section; 5] = [
    Section::Header,
    Section::Architecture,
    Section::Map,
    Section::Breadth,
    Section::Docs,
];
/// Lines of each top document the repository part takes at most.
const TOP_DOC_LINES: usize = 200;

/// The share as a person reads it: 0.9 is 90, 0.925 is 92.5.
fn percent(share: f64) -> String {
    format!("{}", (share * 1000.0).round() / 10.0)
}

/// The cache layout: the repository part, the same bytes for every
/// question at this commit, budget and settings, then the question part.
fn build_cache(corpus: &mut Corpus<'_>, options: &Options, counter: &Counter) -> Result<Value> {
    let budget = options.budget;
    let frame = frames(options.style, counter);
    let prefix = repository_part(corpus, options, counter, &frame)?;
    let ranked = question_ranking(corpus, options);
    let rank_of: HashMap<String, usize> = ranked.iter().map(|r| (r.path.clone(), r.rank)).collect();
    let limit = budget - ((budget as f64) * counter.margin()).ceil() as usize;
    // The question part takes what the repository part leaves.
    let room = budget.saturating_sub(prefix.tokens);
    let question_part = options.prefix_share < 1.0 && !options.question.is_empty();
    let mut fill = Fill::new(
        options,
        counter,
        limit.saturating_sub(prefix.tokens),
        frame,
        question_part && options.on(Section::Manifest),
    );
    fill.covered = prefix.held.clone();
    if question_part {
        if fill.manifest {
            let lines = (room / 600).clamp(3, MANIFEST_OMITTED);
            fill.used += fill.frame[&Section::Manifest] + 60 + lines * 24;
        }
        let sizing = Sizing::question_part(room);
        let top_code = top_code_of(&ranked);
        let focus_symbols = focus_symbols_of(corpus, &ranked);
        if options.on(Section::Focus) && !ranked.is_empty() {
            let reserved: usize = [(Section::Paths, 0.10), (Section::Tests, 0.10)]
                .iter()
                .filter(|(s, _)| options.on(*s))
                .map(|(_, p)| sizing.share(*p))
                .sum();
            let left = fill.limit.saturating_sub(fill.used);
            let cap = left
                .saturating_sub(reserved)
                .max(sizing.share(0.5).min(left));
            fill_focus(
                &mut fill,
                corpus,
                &ranked,
                cap,
                &sizing,
                Some(&prefix.whole),
            );
        }
        if options.on(Section::Paths) && !focus_symbols.is_empty() && corpus.graph.is_some() {
            fill_paths(
                &mut fill,
                corpus,
                &focus_symbols,
                &rank_of,
                sizing.share(0.10),
            );
        }
        if options.on(Section::Tests) && !ranked.is_empty() {
            fill_tests(
                &mut fill,
                corpus,
                &focus_symbols,
                &top_code,
                &rank_of,
                sizing.share(0.10),
                sizing.whole_limit,
            );
        }
    }
    finish_cache(
        corpus, options, counter, fill, &prefix, &ranked, &rank_of, room,
    )
}

/// Each step down from a fixed `prefix_budget` whose share does not fit
/// the budget keeps this much of the last ([`prefix_basis`]).
const BASIS_STEP: f64 = 0.8;

/// The budget the cache layout's repository part is sized from. A call
/// whose budget moves with calibration names a fixed `prefix_budget`: the
/// part is sized from it when its share fits in the budget, else from the
/// first of its steps down by [`BASIS_STEP`] (0.8, 0.64, ... of it) whose
/// share fits. The basis then moves only when the budget crosses a step, so
/// the repository part stays the same bytes from call to call. Without a
/// `prefix_budget`, or below [`MIN_BUDGET`], the budget itself.
fn prefix_basis(options: &Options) -> usize {
    let Some(mut basis) = options.prefix_budget else {
        return options.budget;
    };
    let fits = |basis: usize| ((basis as f64) * options.prefix_share) as usize <= options.budget;
    while !fits(basis) {
        basis = ((basis as f64) * BASIS_STEP) as usize;
        if basis < MIN_BUDGET {
            return options.budget;
        }
    }
    basis
}

/// The repository part: header, architecture, the folder map, the top
/// documentation, then breadth by centrality, within the prefix share of
/// the budget it is sized from ([`prefix_basis`]); nothing in it depends on
/// the question.
fn repository_part(
    corpus: &mut Corpus<'_>,
    options: &Options,
    counter: &Counter,
    frame: &BTreeMap<Section, usize>,
) -> Result<Prefix> {
    let budget = prefix_basis(options);
    let size = ((budget as f64) * options.prefix_share).floor() as usize;
    let limit = size.saturating_sub(((size as f64) * counter.margin()).ceil() as usize);
    let mut fill = Fill::new(options, counter, limit, frame.clone(), false);
    let header = text_chunk(
        "header",
        cache_header_text(corpus, options, counter),
        counter,
    );
    fill.used += header.tokens + fill.frame[&Section::Header];
    if fill.used > limit {
        return Err(Error::bad(format!(
            "The budget of {budget} tokens at a prefix share of {} leaves no room after the header",
            options.prefix_share
        )));
    }
    let share = |part: f64| ((budget as f64) * part) as usize;
    if options.on(Section::Architecture) {
        add_architecture(&mut fill, corpus, share(0.04));
    }
    if options.on(Section::Map) {
        let cap = share(0.03);
        let room = fill.room(Section::Map, cap);
        let body = map_text(corpus, room, counter);
        if !body.is_empty() {
            let tokens = counter.count(&body, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0);
            fill.add(
                Section::Map,
                cap,
                Chunk {
                    path: None,
                    start: 0,
                    end: 0,
                    whole: false,
                    why: "map".into(),
                    text: body,
                    tokens,
                    order: usize::MAX,
                },
                None,
            );
        }
    }
    if options.on(Section::Docs) {
        let cap = share(0.03);
        for (path, why) in top_docs(corpus) {
            if fill.room(Section::Docs, cap) < 64 {
                break;
            }
            let Some(text) = corpus.text(&path) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            if lines.is_empty() {
                continue;
            }
            let end = lines.len().min(TOP_DOC_LINES);
            let why = move |_: usize, _: usize| why.to_string();
            fill.add_ranges(
                Section::Docs,
                cap,
                &path,
                &lines,
                &[(1, end)],
                &why,
                None,
                usize::MAX,
            );
        }
    }
    if options.on(Section::Breadth) {
        let centrality = centrality_of(corpus);
        let mut rest: Vec<&String> = corpus
            .files
            .keys()
            .filter(|p| {
                is_code(p)
                    && !generated(p)
                    && matches!(class(p), crate::code_question::PathClass::Source)
            })
            .collect();
        rest.sort_by(|a, b| {
            let ca = centrality.get(a.as_str()).copied().unwrap_or(0);
            let cb = centrality.get(b.as_str()).copied().unwrap_or(0);
            cb.cmp(&ca).then(a.cmp(b))
        });
        let candidates: Vec<(String, String)> = rest
            .into_iter()
            .map(|path| {
                let c = centrality.get(path.as_str()).copied().unwrap_or(0);
                (path.clone(), format!("breadth: centrality {c}"))
            })
            .collect();
        let whole_limit = (budget / 16).clamp(1_500, 16_000);
        fill_breadth(
            &mut fill,
            corpus,
            candidates,
            &HashMap::new(),
            whole_limit * 2,
        );
    }
    loop {
        let mut sections: Vec<(Section, Vec<Chunk>)> = Vec::new();
        for section in PREFIX_SECTIONS {
            let mut chunks: Vec<Chunk> = match section {
                Section::Header => vec![header.clone()],
                _ => match fill.sections.get(&section) {
                    Some(chunks) => layout(chunks, options.order),
                    None => continue,
                },
            };
            let open = section_open(options.style, section);
            if !open.is_empty() {
                chunks.insert(0, text_chunk("section", open, counter));
            }
            let close = section_close(options.style);
            if !close.is_empty() {
                chunks.push(text_chunk("section", close, counter));
            }
            sections.push((section, chunks));
        }
        let text: String = sections
            .iter()
            .flat_map(|(_, chunks)| chunks.iter().map(|c| c.text.as_str()))
            .collect();
        let sum: usize = sections
            .iter()
            .flat_map(|(_, chunks)| chunks.iter().map(|c| c.tokens))
            .sum();
        let tokens = counter.total(&text, sum);
        if tokens > size {
            let drop_order = [
                Section::Breadth,
                Section::Docs,
                Section::Map,
                Section::Architecture,
            ];
            if drop_order.iter().any(|&s| fill.pop(s)) {
                continue;
            }
            return Err(Error::bad(format!(
                "The budget of {budget} tokens at a prefix share of {} leaves no room after the header",
                options.prefix_share
            )));
        }
        let mut whole = BTreeMap::new();
        let mut held: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        for (section, chunks) in &sections {
            for chunk in chunks {
                let Some(path) = &chunk.path else {
                    continue;
                };
                if chunk.whole {
                    whole.insert(path.clone(), chunk.end);
                }
                if chunk.whole || *section == Section::Docs {
                    held.entry(path.clone())
                        .or_default()
                        .push((chunk.start, chunk.end));
                }
            }
        }
        for ranges in held.values_mut() {
            *ranges = merge_exact(std::mem::take(ranges));
        }
        return Ok(Prefix {
            sections,
            text,
            tokens,
            sum,
            files: fill.files,
            whole,
            held,
        });
    }
}

/// The repository part's header: no question, time or count that changes
/// with the question.
fn cache_header_text(corpus: &Corpus<'_>, options: &Options, counter: &Counter) -> String {
    let counted = counter.counted();
    let part = if options.prefix_share >= 1.0 {
        "This pack is one repository part, the same text for every question about this commit with these settings; only the question after it changes.".to_string()
    } else {
        format!(
            "This pack starts with a repository part, the same text for every question about this commit with these settings (at most {}% of the budget), then a question part chosen for the question after the pack.",
            percent(options.prefix_share)
        )
    };
    let order = match options.order {
        Order::RelevantLast => {
            "relevant-last: in the question part, the most relevant code at the end"
        }
        Order::RelevantFirst => {
            "relevant-first: in the question part, the most relevant code first"
        }
    };
    let numbers = match options.numbers {
        Numbers::Focus => "Focus code lines start with their line number and a tab. ",
        Numbers::All => "Code lines start with their line number and a tab. ",
        Numbers::None => "",
    };
    let body = format!(
        "Repository: {} at commit {}\nTokens: {counted}.\n{part}\nThe repository part holds the architecture summary, a map of the folders, the files with the most connections in the code graph (whole, or the first line of each symbol) and the top documentation.\nOrder: {order}.\n{numbers}Each file chunk names its path, line range and why it is here. Cite code as path:line.\n",
        corpus.project,
        corpus.commit.as_deref().unwrap_or("unknown")
    );
    match options.style {
        Style::Markdown => format!("# Code pack: {}\n\n{body}\n", corpus.project),
        Style::Xml => body,
    }
}

/// The repository's README and its architecture or design documents,
/// shallowest first, each with why it is here.
fn top_docs(corpus: &Corpus<'_>) -> Vec<(String, &'static str)> {
    let mut docs: Vec<(String, &'static str)> = Vec::new();
    if let Some(readme) = [
        "README.md",
        "README.rst",
        "README.txt",
        "README",
        "readme.md",
    ]
    .iter()
    .find(|name| corpus.files.contains_key(**name))
    {
        docs.push((readme.to_string(), "docs: the repository's README"));
    }
    let mut others: Vec<&String> = corpus
        .files
        .keys()
        .filter(|path| {
            let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
            let text = [".md", ".rst", ".adoc", ".txt"]
                .iter()
                .any(|ext| name.ends_with(ext));
            text && path.matches('/').count() <= 2
                && class(path) != crate::code_question::PathClass::Vendored
                && (name.contains("architecture")
                    || name.starts_with("design")
                    || name.starts_with("overview"))
        })
        .collect();
    others.sort_by(|a, b| (a.matches('/').count(), *a).cmp(&(b.matches('/').count(), *b)));
    for path in others.into_iter().take(4) {
        docs.push((path.clone(), "docs: architecture"));
    }
    docs
}

/// The repository map within `room`: every folder that holds files (other
/// than vendored ones), with its files, their estimated tokens and its most
/// connected symbols, in path order; when they do not all fit, the most
/// connected and largest folders.
fn map_text(corpus: &Corpus<'_>, room: usize, counter: &Counter) -> String {
    #[derive(Default)]
    struct Folder {
        files: usize,
        tokens: usize,
        degree: usize,
        /// (degree, symbol index) of its symbols.
        symbols: Vec<(usize, usize)>,
    }
    let folder_of = |path: &str| -> String {
        path.rsplit_once('/')
            .map_or(String::new(), |(f, _)| f.to_string())
    };
    let mut folders: BTreeMap<String, Folder> = BTreeMap::new();
    let mut vendored = (0usize, 0usize, 0usize); // folders, files, tokens
    let mut vendored_folders: BTreeSet<String> = BTreeSet::new();
    for (path, &bytes) in &corpus.files {
        let folder = folder_of(path);
        if class(path) == crate::code_question::PathClass::Vendored {
            vendored.1 += 1;
            vendored.2 += est_file(path, bytes);
            vendored_folders.insert(folder);
            continue;
        }
        let entry = folders.entry(folder).or_default();
        entry.files += 1;
        entry.tokens += est_file(path, bytes);
    }
    vendored.0 = vendored_folders.len();
    if let Some(graph) = &corpus.graph {
        for (i, symbol) in graph.symbols.iter().enumerate() {
            if short(&symbol.name).is_empty() {
                continue;
            }
            if let Some(entry) = folders.get_mut(&folder_of(&symbol.path)) {
                entry.degree += symbol.degree;
                entry.symbols.push((symbol.degree, i));
            }
        }
    }
    let line_of = |folder: &str, entry: &Folder| -> String {
        let shown = if folder.is_empty() { "." } else { folder };
        let files = if entry.files == 1 { "file" } else { "files" };
        let mut line = format!(
            "- {shown}/: {} {files}, ~{} tokens",
            entry.files, entry.tokens
        );
        if let Some(graph) = &corpus.graph {
            let mut best = entry.symbols.clone();
            best.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let mut names: Vec<&str> = Vec::new();
            for (_, i) in best {
                let name = short(&graph.symbols[i].name);
                if !names.contains(&name) {
                    names.push(name);
                }
                if names.len() == 3 {
                    break;
                }
            }
            if !names.is_empty() {
                line.push_str(&format!(". Key: {}", names.join(", ")));
            }
        }
        line.push_str(".\n");
        line
    };
    let cost = |text: &str| counter.count(text, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0);
    let lines: Vec<(&String, String)> = folders
        .iter()
        .map(|(folder, entry)| (folder, line_of(folder, entry)))
        .collect();
    let tail = if vendored.1 > 0 {
        format!(
            "Vendored code, not listed: {} folders, {} files, ~{} tokens.\n",
            vendored.0, vendored.1, vendored.2
        )
    } else {
        String::new()
    };
    let head = |shown: usize| -> String {
        if shown == lines.len() {
            format!(
                "{} folders that hold files, in path order. Each line: the folder, the files directly in it, their tokens (estimated) and its most connected symbols in the code graph.\n\n",
                lines.len()
            )
        } else {
            format!(
                "{shown} of the {} folders that hold files (the most connected and largest), in path order. Each line: the folder, the files directly in it, their tokens (estimated) and its most connected symbols in the code graph.\n\n",
                lines.len()
            )
        }
    };
    // The folders by weight: connections, then tokens, then path.
    let mut by_weight: Vec<usize> = (0..lines.len()).collect();
    by_weight.sort_by(|&a, &b| {
        let (fa, fb) = (&folders[lines[a].0], &folders[lines[b].0]);
        (fb.degree, fb.tokens)
            .cmp(&(fa.degree, fa.tokens))
            .then(lines[a].0.cmp(lines[b].0))
    });
    let fixed = cost(&head(lines.len())) + cost(&tail) + 1;
    let mut chosen: Vec<usize> = Vec::new();
    let mut spent = fixed;
    for &i in &by_weight {
        let c = cost(&lines[i].1);
        if spent + c > room {
            break;
        }
        spent += c;
        chosen.push(i);
    }
    loop {
        if chosen.is_empty() {
            return String::new();
        }
        let mut shown = chosen.clone();
        shown.sort_unstable();
        let mut text = head(shown.len());
        for &i in &shown {
            text.push_str(&lines[i].1);
        }
        text.push_str(&tail);
        text.push('\n');
        if cost(&text) <= room {
            return text;
        }
        // An exact count over the whole text: drop the lightest tenth.
        let keep = chosen.len() - chosen.len().div_ceil(10);
        chosen.truncate(keep);
    }
}

/// The cache layout's manifest: the question part's files, how many the
/// repository part holds, and the top ranked files left out of both.
fn cache_manifest(
    corpus: &Corpus<'_>,
    fill: &Fill<'_>,
    prefix: &Prefix,
    ranked: &[Ranked],
    room: usize,
) -> String {
    let mut included: Vec<(&String, &Included)> = fill.files.iter().collect();
    included.sort_by(|a, b| {
        (a.1.rank.unwrap_or(usize::MAX), a.0).cmp(&(b.1.rank.unwrap_or(usize::MAX), b.0))
    });
    let mut text = format!(
        "The question part starts here: {} files (tokens each, the section of their first chunk, and their rank in the retrieval where ranked). The repository part above holds {} files.\n",
        included.len(),
        prefix.files.len()
    );
    for (path, entry) in &included {
        text.push_str(&manifest_line(
            path,
            entry.tokens,
            entry.rank,
            entry.section,
        ));
    }
    let lines = (room / 600).clamp(3, MANIFEST_OMITTED);
    let omitted: Vec<String> = ranked
        .iter()
        .filter(|f| !fill.files.contains_key(&f.path) && !prefix.files.contains_key(&f.path))
        .take(lines)
        .map(|f| {
            format!(
                "- {}: ~{} tokens, rank {}\n",
                f.path,
                est_file(&f.path, corpus.files[&f.path]),
                f.rank
            )
        })
        .collect();
    if !omitted.is_empty() {
        text.push_str("Top ranked files left out (estimated tokens, rank):\n");
        for line in omitted {
            text.push_str(&line);
        }
    }
    text.push('\n');
    text
}

/// The cache layout's question part assembled after the repository part,
/// within the budget, and the pack's JSON.
#[allow(clippy::too_many_arguments)]
fn finish_cache(
    corpus: &Corpus<'_>,
    options: &Options,
    counter: &Counter,
    mut fill: Fill<'_>,
    prefix: &Prefix,
    ranked: &[Ranked],
    rank_of: &HashMap<String, usize>,
    room: usize,
) -> Result<Value> {
    let style = options.style;
    let order = match options.order {
        Order::RelevantLast => [
            Section::Manifest,
            Section::Tests,
            Section::Paths,
            Section::Focus,
        ],
        Order::RelevantFirst => [
            Section::Manifest,
            Section::Focus,
            Section::Paths,
            Section::Tests,
        ],
    };
    let mut with_manifest = fill.manifest;
    loop {
        let manifest = with_manifest.then(|| cache_manifest(corpus, &fill, prefix, ranked, room));
        let mut suffix: Vec<(Section, Vec<Chunk>)> = Vec::new();
        for section in order {
            let mut chunks: Vec<Chunk> = match section {
                Section::Manifest => match &manifest {
                    Some(text) => vec![text_chunk("manifest", text.clone(), counter)],
                    None => continue,
                },
                _ => match fill.sections.get(&section) {
                    Some(chunks) => layout(chunks, options.order),
                    None => continue,
                },
            };
            let open = section_open(style, section);
            if !open.is_empty() {
                chunks.insert(0, text_chunk("section", open, counter));
            }
            let close = section_close(style);
            if !close.is_empty() {
                chunks.push(text_chunk("section", close, counter));
            }
            suffix.push((section, chunks));
        }
        let suffix_text: String = suffix
            .iter()
            .flat_map(|(_, chunks)| chunks.iter().map(|c| c.text.as_str()))
            .collect();
        let suffix_sum: usize = suffix
            .iter()
            .flat_map(|(_, chunks)| chunks.iter().map(|c| c.tokens))
            .sum();
        let total = if suffix.is_empty() {
            prefix.tokens
        } else {
            counter.total(
                &format!("{}{suffix_text}", prefix.text),
                prefix.sum + suffix_sum,
            )
        };
        if total > options.budget {
            if [Section::Tests, Section::Paths, Section::Focus]
                .iter()
                .any(|&s| fill.pop(s))
            {
                continue;
            }
            if with_manifest {
                with_manifest = false;
                continue;
            }
            return Err(Error::bad(format!(
                "The budget of {} tokens leaves no room after the repository part",
                options.budget
            )));
        }
        let mut tokens_of: BTreeMap<&String, usize> = BTreeMap::new();
        for (path, entry) in prefix.files.iter().chain(fill.files.iter()) {
            *tokens_of.entry(path).or_default() += entry.tokens;
        }
        let mut included: Vec<(&String, usize, Option<usize>)> = tokens_of
            .into_iter()
            .map(|(path, tokens)| (path, tokens, rank_of.get(path.as_str()).copied()))
            .collect();
        included.sort_by(|a, b| {
            (a.2.unwrap_or(usize::MAX), a.0).cmp(&(b.2.unwrap_or(usize::MAX), b.0))
        });
        let omitted: Vec<Value> = ranked
            .iter()
            .filter(|r| !prefix.files.contains_key(&r.path) && !fill.files.contains_key(&r.path))
            .take(50)
            .map(|r| json!({"path":r.path,"rank":r.rank,"tokens":est_file(&r.path, corpus.files[&r.path])}))
            .collect();
        let sections: Vec<Value> = prefix
            .sections
            .iter()
            .map(|s| (s, "prefix"))
            .chain(suffix.iter().map(|s| (s, "suffix")))
            .map(|((section, chunks), part)| {
                json!({
                    "name": section.name(),
                    "part": part,
                    "tokens": chunks.iter().map(|c| c.tokens).sum::<usize>(),
                    "chunks": chunks.iter().map(Chunk::json).collect::<Vec<_>>(),
                })
            })
            .collect();
        return Ok(json!({
            "format": FORMAT,
            "project": corpus.project,
            "commit": corpus.commit,
            "question": options.question,
            "budget": options.budget,
            "tokens": total,
            "tokenizer": counter.name(),
            "order": options.order.name(),
            "layout": Layout::Cache.name(),
            "prefix_share": options.prefix_share,
            "prefix_tokens": prefix.tokens,
            "prefix_sha256": sha256_hex(&prefix.text),
            "sections": sections,
            "included": included.iter().map(|(path, tokens, rank)| json!({"path":path,"tokens":tokens,"rank":rank})).collect::<Vec<_>>(),
            "omitted": omitted,
        }));
    }
}

/// The SHA-256 of `text`'s bytes, in hex.
pub fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Focus symbols the call paths and tests start from at most.
const MAX_FOCUS_SYMBOLS: usize = 16;

/// A minified or data file: lines of 300 bytes or more on average.
fn minified(bytes: usize, lines: usize) -> bool {
    lines > 0 && bytes / lines >= 300
}

/// An outline: one chunk of a file's symbol lines (not numbered unless all
/// chunks are), as one piece with gaps marked.
fn outline(
    fill: &mut Fill<'_>,
    path: &str,
    lines: &[&str],
    ranges: &[(usize, usize)],
    why: &str,
    rank: Option<usize>,
) -> bool {
    if ranges.is_empty() {
        return false;
    }
    let mut body = String::new();
    let mut prefix_bytes = 0;
    let mut prefix_tenths = 0;
    for &(a, b) in ranges {
        for n in a..=b {
            // Outlines always carry line numbers: their lines are apart.
            let prefix = format!("{n}\t");
            prefix_bytes += prefix.len();
            prefix_tenths += estimate::prefix_tenths(n);
            body.push_str(&prefix);
            body.push_str(lines[n - 1]);
            body.push('\n');
        }
    }
    let (start, end) = (ranges[0].0, ranges[ranges.len() - 1].1);
    let range = format!("{start}-{end}");
    let text = match fill.options.style {
        Style::Markdown => {
            let fence = fence(&body);
            format!(
                "### {path} lines {range} (outline)\nwhy: {why}\n{fence}{}\n{body}{fence}\n\n",
                language(path)
            )
        }
        Style::Xml => format!(
            "<file path=\"{}\" lines=\"{range}\" why=\"{}\">\n{body}</file>\n",
            attr(path),
            attr(why)
        ),
    };
    let tokens = fill.counter.count(
        &text,
        rate_of(path),
        prefix_bytes,
        prefix_tenths.div_ceil(10),
    );
    let chunk = Chunk {
        path: Some(path.to_string()),
        start,
        end,
        whole: false,
        why: why.to_string(),
        text,
        tokens,
        order: usize::MAX,
    };
    fill.add(Section::Breadth, fill.limit, chunk, rank)
}

/// The call paths of `focus` (lines) and the hops whose bodies matter
/// (node, why), most connected first.
fn call_paths(
    graph: &GraphView<'_>,
    focus: &[usize],
    rank_of: &HashMap<String, usize>,
) -> (Vec<String>, Vec<(usize, String)>) {
    let show = |node: usize| -> Option<String> {
        let s = graph.symbol_of(node)?;
        Some(format!("{} ({}:{})", short(&s.name), s.path, s.line))
    };
    let order = |nodes: &mut Vec<usize>| {
        nodes.sort_by(|&a, &b| {
            let key = |n: usize| {
                let s = graph.symbol_of(n);
                (
                    s.and_then(|s| rank_of.get(&s.path).copied())
                        .unwrap_or(usize::MAX),
                    s.map_or("", |s| s.path.as_str()).to_string(),
                    s.map_or(0, |s| s.line),
                )
            };
            key(a).cmp(&key(b))
        });
        nodes.dedup();
    };
    let neighbours = |node: usize, callers: bool| -> Vec<usize> {
        let mut found: Vec<usize> = graph
            .edges(node)
            .iter()
            .filter(|e| e.relation == "calls" && e.incoming == callers)
            .map(|e| e.other)
            // Tests have their own section; generated code is left out.
            .filter(|&n| {
                graph.symbol_of(n).is_some_and(|s| {
                    !is_test(&s.path) && !generated(&s.path) && !short(&s.name).is_empty()
                })
            })
            .collect();
        order(&mut found);
        found
    };
    let mut lines: Vec<String> = Vec::new();
    let mut seen_lines: BTreeSet<String> = BTreeSet::new();
    let mut hops: Vec<(usize, String)> = Vec::new();
    let mut hop_count: HashMap<usize, usize> = HashMap::new();
    let mut push = |line: String, lines: &mut Vec<String>| {
        if seen_lines.insert(line.clone()) {
            lines.push(line);
        }
    };
    for &f in focus {
        let Some(fs) = show(f) else {
            continue;
        };
        let fname = short(&graph.symbol_of(f).unwrap().name).to_string();
        for c1 in neighbours(f, true).into_iter().take(5) {
            let Some(c1s) = show(c1) else { continue };
            let hub = graph.symbol_of(c1).is_some_and(|s| s.degree > 500);
            let c2s = if hub {
                Vec::new()
            } else {
                neighbours(c1, true)
            };
            if c2s.is_empty() {
                push(format!("- {c1s} -> {fs}"), &mut lines);
            }
            for c2 in c2s.into_iter().take(2) {
                if let Some(c2s) = show(c2) {
                    push(format!("- {c2s} -> {c1s} -> {fs}"), &mut lines);
                }
            }
            if !focus.contains(&c1) {
                *hop_count.entry(c1).or_default() += 1;
                if !hops.iter().any(|(n, _)| *n == c1) {
                    hops.push((c1, format!("caller of {fname}")));
                }
            }
        }
        for d1 in neighbours(f, false).into_iter().take(5) {
            let Some(d1s) = show(d1) else { continue };
            let hub = graph.symbol_of(d1).is_some_and(|s| s.degree > 500);
            let d2s = if hub {
                Vec::new()
            } else {
                neighbours(d1, false)
            };
            if d2s.is_empty() {
                push(format!("- {fs} -> {d1s}"), &mut lines);
            }
            for d2 in d2s.into_iter().take(2) {
                if let Some(d2s) = show(d2) {
                    push(format!("- {fs} -> {d1s} -> {d2s}"), &mut lines);
                }
            }
            if !focus.contains(&d1) {
                *hop_count.entry(d1).or_default() += 1;
                if !hops.iter().any(|(n, _)| *n == d1) {
                    hops.push((d1, format!("callee of {fname}")));
                }
            }
        }
    }
    // Most connected first; ties keep the order they were found in.
    let position: HashMap<usize, usize> =
        hops.iter().enumerate().map(|(i, (n, _))| (*n, i)).collect();
    hops.sort_by(|a, b| {
        hop_count[&b.0]
            .cmp(&hop_count[&a.0])
            .then(position[&a.0].cmp(&position[&b.0]))
    });
    (lines, hops)
}

fn header_text(
    corpus: &Corpus<'_>,
    options: &Options,
    counter: &Counter,
    tokens: usize,
    rank_of: &HashMap<String, usize>,
) -> String {
    let question = if options.question.is_empty() {
        "none (a whole-repository overview)".to_string()
    } else {
        let mut q: String = options.question.chars().take(300).collect();
        if q.len() < options.question.len() {
            q.push_str("...");
        }
        q.replace('\n', " ")
    };
    let counted = counter.counted();
    let chosen = if options.question.is_empty() {
        "no question: folders from the code graph and files by centrality".to_string()
    } else if rank_of.is_empty() {
        "the retrieval ranking found no files for the question; the rest by folder and centrality"
            .to_string()
    } else {
        "Graphicle's retrieval ranking for the question (passage index, identifier arm, code-graph arm); call paths and tests from the code graph; the rest by rank, folder and centrality".to_string()
    };
    let order = match options.order {
        Order::RelevantLast => {
            "relevant-last: least relevant first, the most relevant code at the end"
        }
        Order::RelevantFirst => "relevant-first: the most relevant code first",
    };
    let numbers = match options.numbers {
        Numbers::Focus => "Focus code lines start with their line number and a tab. ",
        Numbers::All => "Code lines start with their line number and a tab. ",
        Numbers::None => "",
    };
    let body = format!(
        "Repository: {} at commit {}\nQuestion: {question}\nBudget: {} tokens; this pack: {tokens} tokens, {counted}.\nChosen by: {chosen}.\nOrder: {order}.\n{numbers}Each file chunk names its path, line range and why it is here. Cite code as path:line.\n",
        corpus.project,
        corpus.commit.as_deref().unwrap_or("unknown"),
        options.budget
    );
    match options.style {
        Style::Markdown => format!("# Code pack: {}\n\n{body}\n", corpus.project),
        Style::Xml => body,
    }
}

fn finish(
    corpus: &Corpus<'_>,
    options: &Options,
    counter: &Counter,
    mut fill: Fill<'_>,
    ranked: &[Ranked],
    rank_of: &HashMap<String, usize>,
    header_reserve: usize,
) -> Result<Value> {
    let style = options.style;
    let drop_order = [
        Section::Breadth,
        Section::Docs,
        Section::Tests,
        Section::Paths,
        Section::Architecture,
        Section::Focus,
    ];
    loop {
        // The manifest from what is in.
        let manifest = if options.on(Section::Manifest) {
            let mut included: Vec<(&String, &Included)> = fill.files.iter().collect();
            included.sort_by(|a, b| {
                (a.1.rank.unwrap_or(usize::MAX), a.0).cmp(&(b.1.rank.unwrap_or(usize::MAX), b.0))
            });
            let mut text = format!(
                "{} files in this pack (tokens each, the section of their first chunk, and their rank in the retrieval where ranked):\n",
                included.len()
            );
            for (path, entry) in &included {
                text.push_str(&manifest_line(
                    path,
                    entry.tokens,
                    entry.rank,
                    entry.section,
                ));
            }
            let lines = (options.budget / 600).clamp(3, MANIFEST_OMITTED);
            let mut omitted: Vec<String> = Vec::new();
            for file in ranked {
                if omitted.len() == lines {
                    break;
                }
                if !fill.files.contains_key(&file.path) {
                    omitted.push(format!(
                        "- {}: ~{} tokens, rank {}\n",
                        file.path,
                        est_file(&file.path, corpus.files[&file.path]),
                        file.rank
                    ));
                }
            }
            if !omitted.is_empty() {
                text.push_str("Top ranked files left out (estimated tokens, rank):\n");
                for line in omitted {
                    text.push_str(&line);
                }
            }
            text.push('\n');
            Some(text)
        } else {
            None
        };
        let manifest_tokens = manifest.as_ref().map_or(0, |t| {
            counter.count(t, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0)
        });
        // Tokens without the header and manifest.
        let body_tokens: usize = fill
            .sections
            .iter()
            .map(|(section, chunks)| {
                fill.frame[section] + chunks.iter().map(|c| c.tokens).sum::<usize>()
            })
            .sum();
        let manifest_total = manifest
            .as_ref()
            .map_or(0, |_| fill.frame[&Section::Manifest] + manifest_tokens);
        // The header at its final numbers (its own tokens included).
        let mut header_tokens = header_reserve;
        let mut header = String::new();
        for _ in 0..3 {
            let total = body_tokens + manifest_total + header_tokens;
            header = header_text(corpus, options, counter, total, rank_of);
            let counted = counter.count(&header, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0)
                + fill.frame[&Section::Header];
            if counted == header_tokens {
                break;
            }
            header_tokens = counted;
        }
        // Assemble in order.
        let mut sections_out: Vec<(Section, Vec<Chunk>)> = Vec::new();
        for section in options.order.sections() {
            let mut chunks: Vec<Chunk> = match section {
                Section::Header => vec![text_chunk("header", header.clone(), counter)],
                Section::Manifest => match &manifest {
                    Some(text) => vec![text_chunk("manifest", text.clone(), counter)],
                    None => continue,
                },
                _ => match fill.sections.get(&section) {
                    Some(chunks) => layout(chunks, options.order),
                    None => continue,
                },
            };
            let open = section_open(style, section);
            if !open.is_empty() {
                chunks.insert(0, text_chunk("section", open, counter));
            }
            let close = section_close(style);
            if !close.is_empty() {
                chunks.push(text_chunk("section", close, counter));
            }
            sections_out.push((section, chunks));
        }
        let text: String = sections_out
            .iter()
            .flat_map(|(_, chunks)| chunks.iter().map(|c| c.text.as_str()))
            .collect();
        let sum: usize = sections_out
            .iter()
            .flat_map(|(_, chunks)| chunks.iter().map(|c| c.tokens))
            .sum();
        let total = counter.total(&text, sum);
        if total > options.budget {
            if drop_order.iter().any(|&s| fill.pop(s)) {
                continue;
            }
            return Err(Error::bad(format!(
                "The budget of {} tokens leaves no room after the header",
                options.budget
            )));
        }
        let mut included: Vec<(&String, &Included)> = fill.files.iter().collect();
        included.sort_by(|a, b| {
            (a.1.rank.unwrap_or(usize::MAX), a.0).cmp(&(b.1.rank.unwrap_or(usize::MAX), b.0))
        });
        let omitted: Vec<Value> = ranked
            .iter()
            .filter(|r| !fill.files.contains_key(&r.path))
            .take(50)
            .map(|r| json!({"path":r.path,"rank":r.rank,"tokens":est_file(&r.path, corpus.files[&r.path])}))
            .collect();
        // The standard layout's prefix is its leading sections that do not
        // depend on the question. The header names the question, so a pack
        // for a question has none; an overview is all prefix.
        let overview = options.question.is_empty();
        let part = if overview { "prefix" } else { "suffix" };
        return Ok(json!({
            "format": FORMAT,
            "project": corpus.project,
            "commit": corpus.commit,
            "question": options.question,
            "budget": options.budget,
            "tokens": total,
            "tokenizer": counter.name(),
            "order": options.order.name(),
            "layout": Layout::Standard.name(),
            "prefix_tokens": if overview { total } else { 0 },
            "prefix_sha256": sha256_hex(if overview { text.as_str() } else { "" }),
            "sections": sections_out.iter().map(|(section, chunks)| json!({
                "name": section.name(),
                "part": part,
                "tokens": chunks.iter().map(|c| c.tokens).sum::<usize>(),
                "chunks": chunks.iter().map(Chunk::json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "included": included.iter().map(|(path, entry)| json!({"path":path,"tokens":entry.tokens,"rank":entry.rank})).collect::<Vec<_>>(),
            "omitted": omitted,
        }));
    }
}

fn text_chunk(why: &str, text: String, counter: &Counter) -> Chunk {
    let tokens = counter.count(&text, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0);
    Chunk {
        path: None,
        start: 0,
        end: 0,
        whole: false,
        why: why.into(),
        text,
        tokens,
        order: 0,
    }
}

/// A section's chunks in output order: by the order their files went in
/// (most relevant first, or last under relevant-last), each file's chunks
/// by line; text chunks first.
fn layout(chunks: &[Chunk], order: Order) -> Vec<Chunk> {
    // A file's place is its first chunk's.
    let mut first: HashMap<&str, usize> = HashMap::new();
    for chunk in chunks {
        if let Some(path) = &chunk.path {
            let entry = first.entry(path.as_str()).or_insert(chunk.order);
            *entry = (*entry).min(chunk.order);
        }
    }
    let mut out: Vec<Chunk> = chunks.to_vec();
    out.sort_by(|a, b| {
        let place = |c: &Chunk| -> (usize, usize, usize) {
            match &c.path {
                None => (0, c.order, 0),
                Some(path) => {
                    let p = first[path.as_str()];
                    let p = match order {
                        Order::RelevantFirst => p,
                        Order::RelevantLast => usize::MAX - p,
                    };
                    (1, p, c.start)
                }
            }
        };
        place(a).cmp(&place(b))
    });
    out
}

/// The text of a pack in the JSON format: its chunks' text in order.
pub fn text(pack: &Value) -> String {
    let mut out = String::new();
    for section in pack["sections"].as_array().into_iter().flatten() {
        for chunk in section["chunks"].as_array().into_iter().flatten() {
            out.push_str(chunk["text"].as_str().unwrap_or(""));
        }
    }
    out
}

/// The summary fields of a pack: the JSON format without its sections.
pub fn summary(pack: &Value) -> Value {
    let mut summary = pack.clone();
    if let Some(fields) = summary.as_object_mut() {
        fields.remove("sections");
    }
    summary
}

/// A git checkout's HEAD commit, read from its files.
pub fn git_head(folder: &Path) -> Option<String> {
    let mut git = folder.join(".git");
    if git.is_file() {
        let text = std::fs::read_to_string(&git).ok()?;
        let dir = text.trim().strip_prefix("gitdir:")?.trim().to_string();
        git = folder.join(dir);
    }
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let sha = match head.strip_prefix("ref:") {
        None => head.to_string(),
        Some(name) => {
            let name = name.trim();
            let common = std::fs::read_to_string(git.join("commondir"))
                .ok()
                .map(|c| git.join(c.trim()))
                .unwrap_or_else(|| git.clone());
            match std::fs::read_to_string(git.join(name))
                .or_else(|_| std::fs::read_to_string(common.join(name)))
            {
                Ok(sha) => sha.trim().to_string(),
                Err(_) => {
                    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
                    packed
                        .lines()
                        .filter_map(|line| line.split_once(' '))
                        .find(|(_, r)| *r == name)
                        .map(|(sha, _)| sha.to_string())?
                }
            }
        }
    };
    let hex = sha.chars().all(|c| c.is_ascii_hexdigit());
    (hex && (sha.len() == 40 || sha.len() == 64)).then_some(sha)
}

impl Store {
    /// The pack of a project for `args` (`repository_id`, `question`,
    /// `budget`, `order`, `line_numbers`, `sections`, `style`, `tokenizer`,
    /// and `focus`, passages ranked ahead of the question's: [`pinned`]),
    /// in the JSON format.
    pub fn project_pack(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let started = std::time::Instant::now();
        let options = Options::parse(args)?;
        let pinned = pinned(args)?;
        let counter = Counter::open(options.tokenizer.as_deref(), options.bytes_per_token)?;
        self.refresh_query_file(workspace, token, crate::string(args, "repository_id", 100)?)?;
        let mut pack = self.authorized_read(workspace, token, "read", |db, p| {
            let id = crate::string(args, "repository_id", 100)?;
            let repository = repo(db, p, id)?;
            let ranked = if options.question.is_empty() {
                Vec::new()
            } else {
                rank(db, p, &repository, &options.question)?
            };
            let ranking: Vec<(String, usize, usize)> =
                pinned.iter().cloned().chain(ranked).collect();
            let mut files = BTreeMap::new();
            for row in rows(
                db,
                "SELECT h.path,o.bytes FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 WHERE h.repository_id=? ORDER BY h.path",
                [id],
            )? {
                files.insert(s(&row, "path").to_string(), row["bytes"].as_u64().unwrap_or(0) as usize);
            }
            let episode = crate::graph_projection::current_episode(db, id)?;
            let graph = crate::query_file::open(db, id, &episode).and_then(GraphView::of_query_file);
            let commit = crate::folders::current_folder(db, id)?.and_then(|folder| git_head(&folder));
            let mut statement = db.prepare(
                "SELECT o.content FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 WHERE h.repository_id=?1 AND h.path=?2",
            )?;
            let fetch = move |path: &str| {
                statement
                    .query_row(rusqlite::params![id, path], |row| row.get::<_, String>(0))
                    .ok()
            };
            let name = s(&repository, "name").to_string();
            let key = format!("{id}:{episode}");
            let mut corpus = Corpus::new(name, commit, files, ranking, graph, Box::new(fetch));
            corpus.modules = cached_modules(&key);
            let pack = build(&mut corpus, &options, &counter)?;
            if let Some(modules) = corpus.modules.take() {
                cache_modules(key, modules);
            }
            Ok(pack)
        })?;
        pack["seconds"] = json!((started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0);
        Ok(pack)
    }
}

impl Store {
    /// Write the project's query file once when this build has none
    /// ([`crate::query_graph::query_file_state`]: missing, or written by
    /// another build), so that this and later packs read it instead of the
    /// whole saved graph. Says so on standard error.
    fn refresh_query_file(&self, workspace: &str, token: &str, id: &str) -> Result<()> {
        let state = self.authorized_read(workspace, token, "read", |db, p| {
            let repository = repo(db, p, id)?;
            let episode = crate::graph_projection::current_episode(db, s(&repository, "id"))?;
            Ok(crate::query_graph::query_file_state(
                db,
                s(&repository, "id"),
                &episode,
            ))
        })?;
        if state["state"] != "missing" {
            return Ok(());
        }
        let saved = self.authorized_read(workspace, token, "read", |db, p| {
            let repository = repo(db, p, id)?;
            crate::query_graph::ensure(db, s(&repository, "id"), true)
        })?;
        if saved["status"] == "written" && saved["query_file"] == "written" {
            eprintln!(
                "graphicle: wrote the project's query file for this build ({} s); later packs read it",
                saved["seconds"]
            );
        }
        Ok(())
    }
}

/// The ranking's passages for `question`: `context`'s fused order.
fn rank(
    db: &rusqlite::Connection,
    p: &crate::store::Principal,
    repository: &Value,
    question: &str,
) -> Result<Vec<(String, usize, usize)>> {
    let question = ranking_question(question);
    let question = crate::code_question::shorten(&question)
        .map(|(q, _)| q)
        .unwrap_or_else(|| question.to_string());
    let mut args = json!({"repository_id":s(repository,"id"),"question":question,"max_tokens":1000,"max_chars":4000,"more_files":0});
    args[crate::query::PACK_RANKING] = json!(true);
    let (_, packet) = crate::query::project_packet(db, p, repository, &args, None, None)?;
    Ok(packet[crate::query::PACK_RANKING]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some((
                row[0].as_str()?.to_string(),
                row[1].as_u64().unwrap_or(1) as usize,
                row[2].as_u64().or(row[1].as_u64()).unwrap_or(1) as usize,
            ))
        })
        .collect())
}

/// Passages a pack takes as its first ranked ones whatever the question
/// (`review` pins the changed hunks there, so that the focus is the changed
/// code, its call paths the changed symbols' callers and callees, and its
/// tests theirs): `focus` in a call's arguments, a list of objects with
/// `path`, `start_line` and `end_line`, at most [`MAX_PINNED`]. A path the
/// project does not hold is left out, as a ranked one is.
pub fn pinned(args: &Value) -> Result<Vec<(String, usize, usize)>> {
    let list = match args.get("focus") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(list)) if list.len() <= MAX_PINNED => list,
        Some(Value::Array(_)) => {
            return Err(Error::bad(format!(
                "focus holds at most {MAX_PINNED} passages"
            )));
        }
        Some(_) => return Err(Error::bad("focus must be a list of passages")),
    };
    let mut passages = Vec::with_capacity(list.len());
    for passage in list {
        let path = passage["path"]
            .as_str()
            .filter(|path| !path.is_empty() && path.len() <= 4096)
            .ok_or_else(|| Error::bad("each focus passage needs a path"))?;
        let line = |key: &str| {
            passage[key]
                .as_u64()
                .filter(|&n| n >= 1)
                .map(|n| n as usize)
                .ok_or_else(|| Error::bad(format!("each focus passage needs a {key} from 1")))
        };
        let (start, end) = (line("start_line")?, line("end_line")?);
        passages.push((path.to_string(), start.min(end), start.max(end)));
    }
    Ok(passages)
}

/// The most passages `focus` pins.
pub const MAX_PINNED: usize = 4_000;

/// The MCP tool's reply: the pack's markdown, and its summary as structured
/// content.
pub fn mcp_reply(pack: Value) -> Value {
    json!({"content":[{"type":"text","text":text(&pack)}],"structuredContent":summary(&pack),"isError":false})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project of `n` Go files in a few folders, each with symbols every
    /// 20 lines, calls between neighbours, and a test file for the first.
    type Project = (BTreeMap<String, String>, Vec<Symbol>, Vec<(usize, usize)>);

    fn project(n: usize) -> Project {
        let mut files = BTreeMap::new();
        let mut symbols = Vec::new();
        let mut calls = Vec::new();
        let mut node = 0;
        for i in 0..n {
            let folder = ["pkg/a", "pkg/b", "cmd/tool", "internal/c"][i % 4];
            let path = format!("{folder}/file{i}.go");
            let functions = 2 + (i % 7) * 6;
            let mut text = String::from("package x\n\n");
            for f in 0..functions {
                let line = text.lines().count() + 1;
                text.push_str(&format!("// F{i}x{f} does a thing.\nfunc F{i}x{f}() {{\n"));
                for k in 0..16 {
                    text.push_str(&format!("\tvalue := compute{k}(\"word{i} widget{f}\")\n"));
                }
                text.push_str("}\n");
                symbols.push(Symbol {
                    node,
                    name: format!("x.F{i}x{f}"),
                    path: path.clone(),
                    line: line + 1,
                    degree: (n - i) + f,
                });
                if node > 0 {
                    calls.push((node - 1, node));
                }
                node += 1;
            }
            files.insert(path, text);
        }
        let test = "pkg/a/file0_test.go".to_string();
        let mut text = String::from("package x\n\n");
        text.push_str("func TestF0x0(t *testing.T) {\n\tF0x0()\n}\n");
        symbols.push(Symbol {
            node,
            name: "x.TestF0x0".into(),
            path: test.clone(),
            line: 3,
            degree: 1,
        });
        calls.push((node, 0));
        files.insert(test, text);
        files.insert(
            "pkg/a/README.md".into(),
            "# Package a\n\nPackage a computes widgets.\n".into(),
        );
        (files, symbols, calls)
    }

    fn corpus<'a>(
        files: &'a BTreeMap<String, String>,
        symbols: &[Symbol],
        calls: &'a [(usize, usize)],
        ranking: Vec<(String, usize, usize)>,
    ) -> Corpus<'a> {
        let sizes = files.iter().map(|(p, t)| (p.clone(), t.len())).collect();
        let edges: EdgesFn<'a> = Box::new(move |node| {
            let mut out = Vec::new();
            for &(a, b) in calls {
                if a == node {
                    out.push(Edge {
                        other: b,
                        relation: "calls".into(),
                        incoming: false,
                    });
                }
                if b == node {
                    out.push(Edge {
                        other: a,
                        relation: "calls".into(),
                        incoming: true,
                    });
                }
            }
            out
        });
        let deps: NodesFn<'a> = Box::new(move |node| {
            calls
                .iter()
                .filter(|(a, _)| *a == node)
                .map(|(_, b)| *b)
                .collect()
        });
        let starts: StartsFn<'a> = Box::new(|_: &str| Vec::new());
        let graph = GraphView::new(symbols.to_vec(), edges, deps, starts);
        Corpus::new(
            "demo".into(),
            Some("0123456789abcdef0123456789abcdef01234567".into()),
            sizes,
            ranking,
            Some(graph),
            Box::new(move |path: &str| files.get(path).cloned()),
        )
    }

    fn ranking() -> Vec<(String, usize, usize)> {
        vec![
            ("pkg/a/file4.go".into(), 30, 40),
            ("pkg/b/file1.go".into(), 3, 10),
            ("pkg/a/file4.go".into(), 200, 215),
            ("pkg/a/README.md".into(), 1, 3),
            ("cmd/tool/file2.go".into(), 5, 12),
            ("pkg/a/file0.go".into(), 3, 8),
        ]
        .into_iter()
        .chain([5, 9, 13, 17, 21, 25, 29, 33].map(|i| (format!("pkg/b/file{i}.go"), 3, 10)))
        .collect()
    }

    fn pack(question: &str, budget: usize, extra: Value) -> Value {
        let (files, symbols, calls) = project(40);
        let mut args = json!({"question":question,"budget":budget});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let options = Options::parse(&args).unwrap();
        let counter = Counter::open(options.tokenizer.as_deref(), options.bytes_per_token).unwrap();
        let mut corpus = corpus(&files, &symbols, &calls, ranking());
        build(&mut corpus, &options, &counter).unwrap()
    }

    fn recount(pack: &Value) -> usize {
        pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["chunks"].as_array().unwrap())
            .map(|c| c["tokens"].as_u64().unwrap() as usize)
            .sum()
    }

    #[test]
    fn the_budget_is_never_exceeded() {
        for budget in [MIN_BUDGET, 900, 2_000, 8_000, 30_000, 200_000, MAX_BUDGET] {
            for question in ["", "how does F4x3 compute widgets"] {
                let pack = pack(question, budget, json!({}));
                let tokens = pack["tokens"].as_u64().unwrap() as usize;
                assert!(tokens <= budget, "{tokens} > {budget}");
                assert_eq!(tokens, recount(&pack));
                // Every section's tokens are its chunks'.
                for section in pack["sections"].as_array().unwrap() {
                    let sum: u64 = section["chunks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|c| c["tokens"].as_u64().unwrap())
                        .sum();
                    assert_eq!(section["tokens"].as_u64().unwrap(), sum);
                }
            }
        }
    }

    /// `focus` passages go ahead of the question's ranking: the pinned file
    /// is rank 1 and the call paths are its symbols' callers and callees,
    /// whatever the question asks about.
    #[test]
    fn pinned_passages_lead_the_focus_and_its_call_paths() {
        assert!(pinned(&json!({})).unwrap().is_empty());
        let one = pinned(
            &json!({"focus":[{"path":"cmd/tool/file38.go","start_line":100,"end_line":99}]}),
        )
        .unwrap();
        assert_eq!(one, [("cmd/tool/file38.go".to_string(), 99, 100)]);
        for bad in [
            json!({"focus":"cmd/tool/file38.go"}),
            json!({"focus":[{"start_line":1,"end_line":2}]}),
            json!({"focus":[{"path":"a.go","start_line":0,"end_line":2}]}),
            json!({"focus":vec![json!({"path":"a.go","start_line":1,"end_line":1}); MAX_PINNED + 1]}),
        ] {
            assert!(pinned(&bad).is_err(), "{bad}");
        }
        let (files, symbols, calls) = project(40);
        let ranked: Vec<(String, usize, usize)> = one.into_iter().chain(ranking()).collect();
        let options =
            Options::parse(&json!({"question":"how does F4x3 compute widgets","budget":20_000}))
                .unwrap();
        let counter = Counter::open(None, None).unwrap();
        let mut corpus = corpus(&files, &symbols, &calls, ranked);
        let pack = build(&mut corpus, &options, &counter).unwrap();
        let first = pack["included"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["rank"] == 1)
            .unwrap();
        assert_eq!(first["path"], "cmd/tool/file38.go");
        let paths: String = pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["name"] == "paths")
            .flat_map(|s| s["chunks"].as_array().unwrap())
            .map(|c| c["text"].as_str().unwrap())
            .collect();
        assert!(
            paths.contains("F38x4") && paths.contains("F38x6"),
            "{paths}"
        );
    }

    #[test]
    fn a_large_budget_takes_the_whole_project() {
        let pack = pack("how does F4x3 compute widgets", MAX_BUDGET, json!({}));
        let (files, _, _) = project(40);
        let included: BTreeSet<&str> = pack["included"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        for path in files
            .keys()
            .filter(|p| p.ends_with(".go") && !p.ends_with("_test.go"))
        {
            assert!(included.contains(path.as_str()), "{path} left out");
        }
    }

    #[test]
    fn the_same_inputs_give_the_same_bytes() {
        for format in ["markdown", "xml"] {
            let a = pack(
                "how does F4x3 compute widgets",
                20_000,
                json!({"style":format}),
            );
            let b = pack(
                "how does F4x3 compute widgets",
                20_000,
                json!({"style":format}),
            );
            assert_eq!(a.to_string(), b.to_string());
            assert_eq!(text(&a), text(&b));
        }
    }

    #[test]
    fn small_files_go_in_whole_and_large_ones_as_excerpts() {
        let pack = pack("how does F4x3 compute widgets", 30_000, json!({}));
        let focus = pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "focus")
            .unwrap();
        let chunks: Vec<&Value> = focus["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| !c["path"].is_null())
            .collect();
        // file1.go (8 functions) is small: whole; file4.go (26) is cut.
        let whole = chunks
            .iter()
            .find(|c| c["path"] == "pkg/b/file1.go")
            .unwrap();
        assert_eq!(whole["whole"], true);
        assert_eq!(whole["start_line"], 1);
        let large: Vec<&&Value> = chunks
            .iter()
            .filter(|c| c["path"] == "pkg/a/file4.go")
            .collect();
        assert!(!large.is_empty());
        for chunk in &large {
            assert_eq!(chunk["whole"], false);
            let (a, b) = (
                chunk["start_line"].as_u64().unwrap(),
                chunk["end_line"].as_u64().unwrap(),
            );
            assert!(b - a < 60, "an excerpt of {a}-{b}");
        }
        // The excerpts hold the passages' symbols, numbered.
        let first = large[0]["text"].as_str().unwrap();
        assert!(first.contains("\tfunc F4x"), "{first}");
        let why = large[0]["why"].as_str().unwrap();
        assert!(why.starts_with("focus: rank 1, defines x.F4x"), "{why}");
    }

    #[test]
    fn overlapping_ranges_merge() {
        assert_eq!(
            merge(vec![(10, 20), (15, 30), (40, 50), (52, 60)]),
            vec![(10, 30), (40, 60)]
        );
        assert_eq!(merge(vec![(5, 5), (1, 3)]), vec![(1, 5)]);
        assert_eq!(merge(vec![(1, 3), (8, 9)]), vec![(1, 3), (8, 9)]);
        assert_eq!(
            uncovered(&[(1, 100)], &[(10, 20), (50, 60)]),
            vec![(1, 9), (21, 49), (61, 100)]
        );
        assert_eq!(
            uncovered(&[(10, 20)], &[(1, 30)]),
            Vec::<(usize, usize)>::new()
        );
        // Two passages in one symbol: one excerpt.
        let symbols: Vec<Symbol> = [1, 20, 40]
            .iter()
            .enumerate()
            .map(|(i, &line)| Symbol {
                node: i,
                name: format!("f{i}"),
                path: "a.go".into(),
                line,
                degree: 1,
            })
            .collect();
        let starts = vec![1, 20, 40];
        assert_eq!(
            excerpt_ranges(&[(22, 24), (30, 33)], &symbols, &starts, 60),
            vec![(20, 39)]
        );
        assert_eq!(
            excerpt_ranges(&[(22, 24), (45, 46)], &symbols, &starts, 60),
            vec![(20, 60)]
        );
    }

    #[test]
    fn the_manifest_lists_what_is_in_and_the_top_left_out() {
        let pack = pack("how does F4x3 compute widgets", 2_500, json!({}));
        let manifest = pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "manifest")
            .unwrap();
        let text: String = manifest["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["text"].as_str().unwrap())
            .collect();
        for file in pack["included"].as_array().unwrap() {
            assert!(text.contains(&format!(
                "- {}: {} tokens",
                file["path"].as_str().unwrap(),
                file["tokens"]
            )));
        }
        assert!(!pack["omitted"].as_array().unwrap().is_empty());
        let first = &pack["omitted"][0];
        assert!(text.contains(&format!("- {}: ~", first["path"].as_str().unwrap())));
    }

    #[test]
    fn each_section_turns_on_and_off() {
        let all = pack("how does F4x3 compute widgets", 300_000, json!({}));
        let names: Vec<&str> = all["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "header",
                "manifest",
                "architecture",
                "breadth",
                "docs",
                "tests",
                "paths",
                "focus"
            ]
        );
        for section in Section::CHOOSABLE {
            let others: Vec<&str> = Section::CHOOSABLE
                .iter()
                .filter(|s| **s != section)
                .map(|s| s.name())
                .collect();
            let without = pack(
                "how does F4x3 compute widgets",
                300_000,
                json!({"sections":others.join(",")}),
            );
            assert!(
                without["sections"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|s| s["name"] != section.name()),
                "{} still in",
                section.name()
            );
            let only = pack(
                "how does F4x3 compute widgets",
                300_000,
                json!({"sections":section.name()}),
            );
            let names: Vec<&str> = only["sections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["name"].as_str().unwrap())
                .collect();
            if section == Section::Map {
                // The map is the cache layout's (tested there).
                assert_eq!(names, ["header"]);
            } else {
                assert_eq!(names, ["header", section.name()]);
            }
        }
        let first = pack(
            "how does F4x3 compute widgets",
            300_000,
            json!({"order":"relevant-first"}),
        );
        let names: Vec<&str> = first["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "header",
                "manifest",
                "architecture",
                "focus",
                "paths",
                "tests",
                "docs",
                "breadth"
            ]
        );
    }

    #[test]
    fn no_question_gives_an_overview() {
        let pack = pack("", 20_000, json!({}));
        let names: Vec<&str> = pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["header", "manifest", "architecture", "breadth"]);
        assert!(text(&pack).contains("none (a whole-repository overview)"));
        assert!(
            pack["included"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["rank"].is_null())
        );
    }

    #[test]
    fn a_tiny_budget_still_fits() {
        let pack = pack("how does F4x3 compute widgets", MIN_BUDGET, json!({}));
        assert!(pack["tokens"].as_u64().unwrap() as usize <= MIN_BUDGET);
        assert_eq!(pack["sections"][0]["name"], "header");
        let refused = Options::parse(&json!({"budget":MIN_BUDGET - 1}));
        assert!(refused.is_err());
        let refused = Options::parse(&json!({"budget":MAX_BUDGET + 1}));
        assert!(refused.is_err());
    }

    #[test]
    fn the_json_contract_holds_and_the_text_is_its_chunks() {
        for style in ["markdown", "xml"] {
            let pack = pack(
                "how does F4x3 compute widgets",
                40_000,
                json!({"style":style}),
            );
            let fields: BTreeSet<&str> = pack
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                fields,
                BTreeSet::from([
                    "format",
                    "project",
                    "commit",
                    "question",
                    "budget",
                    "tokens",
                    "tokenizer",
                    "order",
                    "layout",
                    "prefix_tokens",
                    "prefix_sha256",
                    "sections",
                    "included",
                    "omitted"
                ])
            );
            assert_eq!(pack["format"], FORMAT);
            assert_eq!(pack["tokenizer"], "estimate");
            assert_eq!(pack["order"], "relevant-last");
            assert_eq!(pack["layout"], "standard");
            for section in pack["sections"].as_array().unwrap() {
                assert_eq!(section["part"], "suffix");
                for chunk in section["chunks"].as_array().unwrap() {
                    let keys: BTreeSet<&str> = chunk
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect();
                    assert_eq!(
                        keys,
                        BTreeSet::from([
                            "path",
                            "start_line",
                            "end_line",
                            "whole",
                            "tokens",
                            "why",
                            "text"
                        ])
                    );
                    if let Some(path) = chunk["path"].as_str() {
                        let marker = if style == "xml" {
                            format!(
                                "<file path=\"{path}\" lines=\"{}-{}\"",
                                chunk["start_line"], chunk["end_line"]
                            )
                        } else {
                            format!(
                                "### {path} lines {}-{}",
                                chunk["start_line"], chunk["end_line"]
                            )
                        };
                        assert!(chunk["text"].as_str().unwrap().starts_with(&marker));
                    }
                }
            }
            for file in pack["included"].as_array().unwrap() {
                let path = file["path"].as_str().unwrap();
                let in_chunks = pack["sections"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|s| s["chunks"].as_array().unwrap())
                    .any(|c| c["path"] == path);
                assert!(in_chunks, "{path}");
            }
            let text = text(&pack);
            if style == "xml" {
                assert!(text.starts_with("<section name=\"header\">\n"));
                assert!(text.ends_with("</section>\n"));
            } else {
                assert!(text.starts_with("# Code pack: demo\n"));
            }
            // The focus code comes last under relevant-last, the top file last.
            let last_file = pack["sections"].as_array().unwrap().last().unwrap()["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|c| !c["path"].is_null())
                .unwrap()
                .clone();
            assert_eq!(last_file["path"], "pkg/a/file4.go");
        }
    }

    #[test]
    fn call_paths_and_tests_come_from_the_graph() {
        let pack = pack("how does F0x0 compute widgets", 60_000, json!({}));
        let all = text(&pack);
        assert!(all.contains("## Call paths"));
        assert!(all.contains(" -> "));
        let tests = pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "tests")
            .unwrap();
        assert!(
            tests["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"] == "pkg/a/file0_test.go")
        );
    }

    #[test]
    fn git_heads_read_refs_and_detached_heads() {
        let dir = std::env::temp_dir().join(format!("graphicle-pack-git-{}", std::process::id()));
        let git = dir.join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(git.join("refs/heads/main"), format!("{sha}\n")).unwrap();
        assert_eq!(git_head(&dir).as_deref(), Some(sha));
        std::fs::remove_file(git.join("refs/heads/main")).unwrap();
        std::fs::write(
            git.join("packed-refs"),
            format!("# pack-refs\n{sha} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(git_head(&dir).as_deref(), Some(sha));
        std::fs::write(git.join("HEAD"), format!("{sha}\n")).unwrap();
        assert_eq!(git_head(&dir).as_deref(), Some(sha));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(git_head(&dir), None);
    }

    /// With a tokenizer, every chunk and the whole pack are counted exactly,
    /// and the pack stays inside the budget.
    #[test]
    fn an_exact_tokenizer_counts_and_keeps_the_budget() {
        let dir = std::env::temp_dir().join(format!("graphicle-pack-tok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokenizer.json");
        // Every word and every punctuation run is one token.
        std::fs::write(
            &path,
            r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,
            "pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,
            "model":{"type":"WordLevel","vocab":{"[UNK]":0},"unk_token":"[UNK]"}}"#,
        )
        .unwrap();
        let counter = Counter::open(Some(path.to_str().unwrap()), None).unwrap();
        assert!(counter.exact());
        assert_eq!(counter.name(), dir.file_name().unwrap().to_string_lossy());
        let (files, symbols, calls) = project(40);
        for budget in [MIN_BUDGET, 5_000, 60_000] {
            let options = options_for("how does F4x3 compute widgets", budget);
            let mut corpus = corpus(&files, &symbols, &calls, ranking());
            let pack = build(&mut corpus, &options, &counter).unwrap();
            let tokens = pack["tokens"].as_u64().unwrap() as usize;
            assert!(tokens <= budget, "{tokens} > {budget}");
            let Counter::Exact { tokenizer, .. } = &counter else {
                unreachable!()
            };
            assert_eq!(tokens, exact_count(tokenizer, &text(&pack)));
            assert_eq!(pack["tokenizer"], counter.name());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn options_for(question: &str, budget: usize) -> Options {
        Options::parse(&json!({"question":question,"budget":budget})).unwrap()
    }

    /// `bytes_per_token`: every chunk is its UTF-8 bytes over the rate, with
    /// no margin, and the whole text over the rate stays inside the budget.
    #[test]
    fn one_rate_of_bytes_per_token_counts_every_byte_alike() {
        for budget in [MIN_BUDGET, 6_000, 90_000] {
            let pack = pack(
                "how does F4x3 compute widgets",
                budget,
                json!({"bytes_per_token":4}),
            );
            for section in pack["sections"].as_array().unwrap() {
                for chunk in section["chunks"].as_array().unwrap() {
                    let bytes = chunk["text"].as_str().unwrap().len();
                    assert_eq!(
                        chunk["tokens"].as_u64().unwrap() as usize,
                        bytes.div_ceil(4)
                    );
                }
            }
            let tokens = pack["tokens"].as_u64().unwrap() as usize;
            assert!(tokens <= budget && text(&pack).len().div_ceil(4) <= budget);
            assert_eq!(pack["tokenizer"], "estimate");
            assert!(text(&pack).contains("estimated as UTF-8 bytes / 4, without a margin"));
        }
        assert!(Options::parse(&json!({"bytes_per_token":0.5})).is_err());
        assert!(Counter::open(Some("t.json"), Some(4.0)).is_err());
    }

    #[test]
    fn a_question_routes_to_the_folders_it_names() {
        let files: BTreeMap<String, usize> = [
            "homeassistant/components/alexa_devices/__init__.py",
            "homeassistant/components/alexa_devices/manifest.json",
            "homeassistant/components/light/__init__.py",
            "homeassistant/components/alexa/__init__.py",
            "homeassistant/helpers/entity.py",
            "tests/components/alexa_devices/test_init.py",
            "pkg/controller/job/job_controller.go",
            "pkg/registry/batch/job/strategy.go",
            "test/e2e/apps/job/job.go",
            "a/x/one.go",
            "b/x/two.go",
            "c/x/three.go",
        ]
        .iter()
        .map(|p| (p.to_string(), 100))
        .collect();
        let named = named_folders(
            "Integration causing the issue: Alexa Devices. See https://www.home-assistant.io/integrations/alexa_devices/ and the helpers.",
            &files,
        );
        assert_eq!(named, ["homeassistant/components/alexa_devices"]);
        // Two folders of one name are both routed; three or more, none.
        let named = named_folders("the job counts twice; x marks", &files);
        assert_eq!(named, ["pkg/controller/job", "pkg/registry/batch/job"]);
        assert!(named_folders("general words: helpers, tests, pkg", &files).is_empty());
        // The longest name wins: "Alexa Devices" is alexa_devices, not alexa.
        let named = named_folders("Alexa Devices stops at login", &files);
        assert_eq!(named, ["homeassistant/components/alexa_devices"]);
    }

    #[test]
    fn go_imports_lead_to_the_file_that_defines_a_name() {
        let text = "package job\n\nimport (\n\t\"fmt\"\n\tutilfeature \"k8s.io/apiserver/pkg/util/feature\"\n\t\"k8s.io/kubernetes/pkg/features\"\n)\n\nfunc f() { if utilfeature.Enabled(features.JobPodReplacementPolicy) {} }\n";
        let imports = go_imports(text);
        assert_eq!(imports["features"], "k8s.io/kubernetes/pkg/features");
        assert_eq!(imports["utilfeature"], "k8s.io/apiserver/pkg/util/feature");
        let folders: BTreeSet<String> = [
            "pkg/features",
            "staging/src/k8s.io/apiserver/pkg/util/feature",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            import_folder("k8s.io/kubernetes/pkg/features", &folders).as_deref(),
            Some("pkg/features")
        );
        assert_eq!(
            import_folder("k8s.io/apiserver/pkg/util/feature", &folders).as_deref(),
            Some("staging/src/k8s.io/apiserver/pkg/util/feature")
        );
        assert_eq!(import_folder("fmt", &folders), None);
        let defining = "package features\n\nconst (\n\t// a gate\n\tJobPodReplacementPolicy featuregate.Feature = \"JobPodReplacementPolicy\"\n)\n\nfunc (g *Gate) Enabled(f Feature) bool { return true }\n";
        assert!(go_defines(defining, "JobPodReplacementPolicy"));
        assert!(go_defines(defining, "Enabled"));
        assert!(!go_defines(defining, "Feature"));
        assert!(!go_defines(
            "x := JobPodReplacementPolicy\n",
            "JobPodReplacementPolicy"
        ));
    }

    fn file_map(paths: &[&str]) -> BTreeMap<String, usize> {
        paths.iter().map(|p| (p.to_string(), 100)).collect()
    }

    #[test]
    fn typescript_imports_resolve_relative_paths_and_tsconfig_aliases() {
        let files = file_map(&[
            "packages/core/index.ts",
            "packages/core/src/render3/instructions/shared.ts",
            "packages/core/src/render3/interfaces/view.ts",
            "packages/core/src/util/assert.ts",
            "packages/forms/src/model/form_control.ts",
            "src/vs/base/common/event.ts",
            "src/vs/base/common/lifecycle.ts",
            "src/vs/editor/common/model.ts",
            "packages/tsconfig.json",
        ]);
        let tsconfig = "{\n  \"compilerOptions\": {\n    // resolve from the root\n    \"baseUrl\": \"..\",\n    \"paths\": {\n      \"rxjs/*\": [\"./node_modules/rxjs/*\"],\n      \"@angular/*\": [\"./packages/*\"]\n    }\n  }\n}\n";
        let aliases = ts_aliases_of("packages/tsconfig.json", tsconfig);
        assert_eq!(
            aliases,
            [TsAlias {
                pattern: "@angular/*".into(),
                targets: vec!["packages/*".into()],
            }]
        );
        let text = "import {assertDefined, assertEqual} from '../../util/assert';\nimport {\n  LView,\n  type TView,\n} from '../interfaces/view';\nimport {Injector} from '@angular/core';\nimport {FormControl} from '@angular/forms/src/model/form_control';\nimport {of} from 'rxjs';\nexport * from './missing';\n";
        let imports = ts_imports(text);
        assert_eq!(
            imports,
            [
                ("../../util/assert".to_string(), 2),
                ("../interfaces/view".to_string(), 2),
                ("@angular/core".to_string(), 1),
                ("@angular/forms/src/model/form_control".to_string(), 1),
                ("rxjs".to_string(), 1),
                ("./missing".to_string(), 1),
            ]
        );
        let here = "packages/core/src/render3/instructions";
        let resolved: Vec<Option<String>> = imports
            .iter()
            .map(|(spec, _)| ts_resolve(spec, here, &files, &aliases))
            .collect();
        assert_eq!(
            resolved,
            [
                Some("packages/core/src/util/assert.ts".to_string()),
                Some("packages/core/src/render3/interfaces/view.ts".to_string()),
                Some("packages/core/index.ts".to_string()),
                Some("packages/forms/src/model/form_control.ts".to_string()),
                None,
                None,
            ]
        );
        assert_eq!(
            ts_imports("import type {A, B} from './a';\nimport D, {e} from './d';\n"),
            [("./a".to_string(), 2), ("./d".to_string(), 2)]
        );
        assert!(ts_barrel("packages/core/index.ts"));
        assert!(!ts_barrel("packages/core/src/util/assert.ts"));
        // An ES module names the compiled file: `./event.js` is `event.ts`.
        let text = "import { Emitter } from '../../base/common/event.js';\nimport { Disposable } from '../../base/common/lifecycle.js';\nconst x = require('./model');\n";
        let resolved: Vec<Option<String>> = ts_imports(text)
            .iter()
            .map(|(spec, _)| ts_resolve(spec, "src/vs/editor/common", &files, &[]))
            .collect();
        assert_eq!(
            resolved,
            [
                Some("src/vs/base/common/event.ts".to_string()),
                Some("src/vs/base/common/lifecycle.ts".to_string()),
                Some("src/vs/editor/common/model.ts".to_string()),
            ]
        );
        assert_eq!(joined("a/b", "../../../c"), None);
    }

    #[test]
    fn a_question_names_files_by_their_code_names_and_words() {
        let files = file_map(&[
            "scene/gui/text_edit.cpp",
            "scene/gui/text_edit.h",
            "tests/scene/test_text_edit.h",
            "editor/plugins/text_editor.cpp",
            "packages/forms/src/model/form_control.ts",
            "packages/core/src/foo.component.ts",
            "services/src/main/java/org/keycloak/models/UserModel.java",
            "a/node.cpp",
            "homeassistant/helpers/update_coordinator.py",
            "pkg/controller/job/job_controller.go",
        ]);
        let named = named_files(
            "TextEdit crashes when a FormControl holds a `UserModel`; FooComponent::render too. Node is fine.",
            &files,
        );
        assert_eq!(
            named,
            [
                "scene/gui/text_edit.cpp",
                "scene/gui/text_edit.h",
                "packages/forms/src/model/form_control.ts",
                "services/src/main/java/org/keycloak/models/UserModel.java",
                "packages/core/src/foo.component.ts",
            ]
        );
        assert!(named_files("the node crashes", &files).is_empty());
        // Two or three words of the question name a file of a language that
        // names its files after their classes.
        assert_eq!(
            named_files("Typing in the text edit freezes the editor", &files),
            ["scene/gui/text_edit.cpp", "scene/gui/text_edit.h"]
        );
        // Words do not name a Python or Go module; a code name does.
        assert!(
            named_files(
                "the update coordinator and the job controller stall",
                &files
            )
            .is_empty()
        );
        assert_eq!(
            named_files("JobController stalls", &files),
            ["pkg/controller/job/job_controller.go"]
        );
        assert!(compound("MeshInstance3D") && compound("text_edit") && !compound("Node"));
        assert_eq!(folded("mesh_instance_3d"), folded("MeshInstance3D"));
    }

    #[test]
    fn the_ranking_reads_an_issue_without_its_environment_sections() {
        let issue = "### Tested versions\n\n4.7.2 stable\n\n### System information\n\nGodot v4.8 - Wayland - Vulkan (Forward+)\n\n### Issue description\n\nThe FileSystem dock freezes.\n\n#### Kubernetes version\n\nkubectl version output\n\n### Steps to reproduce\n\n- VS Code Version: 1.95\n- Click twice\n";
        let read = ranking_question(issue);
        for gone in ["4.7.2", "Wayland", "kubectl", "VS Code Version"] {
            assert!(!read.contains(gone), "{gone}");
        }
        for kept in [
            "### Issue description",
            "The FileSystem dock freezes.",
            "### Steps to reproduce",
            "- Click twice",
        ] {
            assert!(read.contains(kept), "{kept}");
        }
        assert!(matches!(
            ranking_question("how does F4x3 compute widgets"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(
            ranking_question("### Godot version\r\n4.0\r\n### Issue description\r\nCrash\r\n"),
            "### Issue description\r\nCrash\r\n"
        );
    }

    #[test]
    fn an_issue_whose_title_names_a_version_ranks_as_written() {
        let issue = "# Flipt Configuration Lacks a Metadata Section for Version Check Preferences\n\n## Description\n\nThe config has no metadata section.\n\n## Expected\n\nA `meta.check_for_updates` key.\n";
        assert!(matches!(
            ranking_question(issue),
            std::borrow::Cow::Borrowed(read) if read == issue
        ));
    }

    #[test]
    fn a_comment_in_a_code_block_is_not_an_environment_section() {
        // A `# comment` line that names a version reads as a level-1 heading,
        // which never opens an environment section: the issue's sections
        // after the block stay in the question.
        let issue = "Saving fails.\n\n```python\n# works on version 2.3, fails on 2.4\nsave()\n```\n\n### Traceback\n\nKeyError: 'name'\n\n### Expected\n\nThe file is saved.\n";
        assert!(matches!(
            ranking_question(issue),
            std::borrow::Cow::Borrowed(read) if read == issue
        ));
    }

    #[test]
    fn a_heading_that_merely_names_a_version_is_not_an_environment_section() {
        // A real title after a one-line preamble, a level-1 heading and short
        // headings that are sentences keep the issue's body.
        for issue in [
            "Reported by a customer.\n\n## Upgrade to version 2 breaks login\n\nThe login form rejects every password.\n\n### Steps\n\nLog in.\n",
            "Seen on Linux.\n# Crash in version 4.2\n\nThe editor closes.\n",
            "Seen on Linux.\n## Environment variables are not expanded\n\n`$HOME` stays literal.\n",
            "Seen on Linux.\n### Broken in new version\n\nThe dock is empty.\n",
        ] {
            assert!(
                matches!(
                    ranking_question(issue),
                    std::borrow::Cow::Borrowed(read) if read == issue
                ),
                "{issue}"
            );
        }
        // Short labels and an issue form's questions for the environment
        // still open environment sections.
        let issue = "### The problem\n\nThe light stays off.\n\n### What version of Home Assistant Core has the issue?\n\ncore-2026.9.4\n\n### Container runtime (CRI) and version (if applicable)\n\ncontainerd 2.1\n\n### OS\n\nLinux\n\n### Anything else?\n\nNo.\n";
        assert_eq!(
            ranking_question(issue),
            "### The problem\n\nThe light stays off.\n\n### Anything else?\n\nNo.\n"
        );
    }

    #[test]
    fn only_a_label_line_is_a_version_line() {
        for line in [
            "Version check fails: the updater crashes offline.",
            "def version(self):",
            "Upgrading to version 2: the login breaks.",
            "In the new version: the dock is empty.",
        ] {
            let issue = format!("The dock is empty.\n{line}\n");
            assert_eq!(ranking_question(&issue), issue, "{line}");
        }
        for line in [
            "- VS Code Version: 1.95",
            "**Godot version:** 4.2.1",
            "* Kubernetes version: v1.37.0",
            "Versions affected: 2.1",
        ] {
            assert_eq!(
                ranking_question(&format!("The dock is empty.\n{line}\n")),
                "The dock is empty.\n",
                "{line}"
            );
        }
    }

    #[test]
    fn excerpts_keep_to_a_cap_best_passage_first() {
        let lines: Vec<String> = (1..=400)
            .map(|i| format!("line number {i} of a long file"))
            .collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let symbols: Vec<Symbol> = [1, 100, 200, 300]
            .iter()
            .enumerate()
            .map(|(i, &line)| Symbol {
                node: i,
                name: format!("f{i}"),
                path: "a.go".into(),
                line,
                degree: 1,
            })
            .collect();
        let starts = vec![1, 100, 200, 300];
        let full = capped_ranges(
            &[(150, 152), (250, 252)],
            &symbols,
            &starts,
            &lines,
            3.0,
            100_000,
            false,
        );
        assert_eq!(full, vec![(100, 299)]);
        let capped = capped_ranges(
            &[(150, 152), (250, 252)],
            &symbols,
            &starts,
            &lines,
            3.0,
            1_200,
            false,
        );
        assert_eq!(capped, vec![(100, 199)]);
        let compact = capped_ranges(
            &[(150, 152), (250, 252)],
            &symbols,
            &starts,
            &lines,
            3.0,
            100_000,
            true,
        );
        assert_eq!(
            compact,
            vec![(100, 100), (147, 155), (200, 200), (247, 255)]
        );
        let tiny = capped_ranges(&[(150, 190)], &symbols, &starts, &lines, 3.0, 40, false);
        assert_eq!(tiny.len(), 1);
        assert!(tiny[0].0 == 150 && tiny[0].1 < 190);
    }

    #[test]
    fn a_named_folder_joins_the_ranking_after_the_top_files() {
        let pack = pack(
            "why does the tool command fail to compute widgets",
            300_000,
            json!({}),
        );
        let routed: Vec<&Value> = pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["chunks"].as_array().unwrap())
            .filter(|c| {
                c["why"]
                    .as_str()
                    .unwrap_or("")
                    .contains("in a folder the question names")
            })
            .collect();
        assert!(!routed.is_empty());
        assert!(
            routed
                .iter()
                .all(|c| c["path"].as_str().unwrap().starts_with("cmd/tool/"))
        );
        // Ranks follow the new order: the top five, then the folder.
        let ranks: Vec<u64> = routed
            .iter()
            .filter_map(|c| {
                let path = c["path"].as_str().unwrap();
                pack["included"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|f| f["path"] == path)?["rank"]
                    .as_u64()
            })
            .collect();
        assert!(ranks.iter().all(|&r| r > ROUTE_AFTER as u64), "{ranks:?}");
    }

    #[test]
    fn symbols_show_without_their_file() {
        assert_eq!(
            short("staging/src/k8s.io/api/apps/v1/types.go.StatefulSet"),
            "StatefulSet"
        );
        assert_eq!(short("pkg/x.go.Cache.Get"), "Cache.Get");
        assert_eq!(short("pkg/x.go.Cache.Get.inner"), "Get.inner");
        assert_eq!(short("listers/apps/v1/statefulset.go"), "");
        assert_eq!(short("kubelet.Kubelet.syncPod"), "Kubelet.syncPod");
        assert_eq!(short("getOrdinal"), "getOrdinal");
        assert_eq!(short("x.F4x1"), "x.F4x1");
    }

    #[test]
    fn the_estimator_rates_are_sorted_and_conservative() {
        let names: Vec<&str> = estimate::BYTES_PER_TOKEN.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(estimate::rate("go"), 3.63);
        assert_eq!(estimate::rate("unknown"), estimate::DEFAULT_BYTES_PER_TOKEN);
        assert_eq!(estimate::prefix_tokens(7, 7), 2);
        assert_eq!(estimate::prefix_tokens(1, 10), 16);
        assert_eq!(estimate::prefix_tokens(9_999, 10_000), 5);
        assert_eq!(
            tokenizer_name("/m/Muse-Spark-Open/tokenizer.json"),
            "Muse-Spark-Open"
        );
        assert_eq!(tokenizer_name("/m/custom.json"), "custom.json");
    }
    /// The pack JSON without the fields the layouts added.
    fn without_layout_fields(pack: &Value) -> Value {
        let mut out = serde_json::Map::new();
        for (key, value) in pack.as_object().unwrap() {
            if ["layout", "prefix_share", "prefix_tokens", "prefix_sha256"].contains(&key.as_str())
            {
                continue;
            }
            if key == "sections" {
                let sections = value
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|section| {
                        let mut kept = serde_json::Map::new();
                        for (k, v) in section.as_object().unwrap() {
                            if k != "part" {
                                kept.insert(k.clone(), v.clone());
                            }
                        }
                        Value::Object(kept)
                    })
                    .collect();
                out.insert(key.clone(), Value::Array(sections));
            } else {
                out.insert(key.clone(), value.clone());
            }
        }
        Value::Object(out)
    }

    /// The standard layout gives the bytes and JSON it gave before the
    /// layouts (hashes taken at 50ebde1), plus the new fields.
    #[test]
    fn the_standard_layout_is_unchanged_but_for_the_new_fields() {
        let goldens: [(&str, usize, Value, &str, &str); 6] = [
            (
                "how does F4x3 compute widgets",
                20_000,
                json!({}),
                "a3fe4f1b8a832d51ebdbd3c82a6f5688bc4abdc9120931ff2f6bdb50decbd6de",
                "ec0fcd4c739a584a137d16819673adc52b8213c7e7a392702e9a345cbcee6bd7",
            ),
            (
                "how does F4x3 compute widgets",
                40_000,
                json!({"style":"xml"}),
                "100f9cbb8c03e9b5d8a817f9fa988b828c9eb7baa2324d274b8a1ae234790e20",
                "94f0b3b5dc3555149fe54a620b609f0be0f28c2dde5df27c2b68c4cd5d2f0466",
            ),
            (
                "",
                20_000,
                json!({}),
                "a7c0f0cd8f5bf5df83cbf487f553bbf545631dd558990c92adb01d65862ce70f",
                "602698368d8c9ea4dc3eb19938f7ce13c0cceaf922af9c92cfd718df65805cc8",
            ),
            (
                "how does F0x0 compute widgets",
                300_000,
                json!({"order":"relevant-first"}),
                "f21d87fad3a784701b24c523434126fbd40ff1658efe6be56c62dd5068c9f632",
                "3b1abe7ac364f823d0aea60fad5d8e2c0c32718fbfe976df19b5cd018fceea8e",
            ),
            (
                "how does F4x3 compute widgets",
                MIN_BUDGET,
                json!({}),
                "f73aaf24790512eafddbb91558fcd15db347b63e50c5ef590417b6dd5971bf3e",
                "42ba6be2990fef1ccd8cd9dcd1b8e4e6cbc4c82fff621351ca465fd539af2f7a",
            ),
            (
                "how does F4x3 compute widgets",
                8_000,
                json!({"line_numbers":"all","sections":"focus,tests,manifest"}),
                "10246944f0adcfd023961ba3f8ac1d6c4a344e16a170b73d827e8e1a75efa0a0",
                "4bec267dbd2d7136f65bea6a76722470c3d5f16f0e51fd4167fc40fcbd592d81",
            ),
        ];
        for (question, budget, extra, text_sha, json_sha) in goldens {
            let pack = pack(question, budget, extra.clone());
            assert_eq!(
                sha256_hex(&text(&pack)),
                text_sha,
                "{question:?} {budget} {extra}"
            );
            assert_eq!(
                sha256_hex(&without_layout_fields(&pack).to_string()),
                json_sha,
                "{question:?} {budget} {extra}"
            );
            assert_eq!(pack["layout"], "standard");
            assert!(pack.get("prefix_share").is_none());
            // The header names the question: a pack for one has no prefix.
            if question.is_empty() {
                assert_eq!(pack["prefix_tokens"], pack["tokens"]);
                assert_eq!(pack["prefix_sha256"], sha256_hex(&text(&pack)));
            } else {
                assert_eq!(pack["prefix_tokens"], 0);
                assert_eq!(pack["prefix_sha256"], sha256_hex(""));
            }
        }
    }

    /// Another question's ranking: other files, other passages.
    fn other_ranking() -> Vec<(String, usize, usize)> {
        vec![
            ("internal/c/file3.go".into(), 20, 30),
            ("cmd/tool/file6.go".into(), 5, 12),
            ("pkg/b/file13.go".into(), 40, 60),
            ("internal/c/file7.go".into(), 3, 9),
            ("pkg/a/file0.go".into(), 60, 70),
        ]
    }

    /// The demo project with a root README and an architecture document.
    fn documented(n: usize) -> Project {
        let (mut files, symbols, calls) = project(n);
        files.insert(
            "README.md".into(),
            "# Demo\n\nDemo computes widgets for tools.\n".into(),
        );
        files.insert(
            "docs/ARCHITECTURE.md".into(),
            "# Architecture\n\nThe tool calls pkg/a, which calls pkg/b.\n".into(),
        );
        (files, symbols, calls)
    }

    fn cache_pack(
        question: &str,
        ranking: Vec<(String, usize, usize)>,
        budget: usize,
        extra: Value,
    ) -> Value {
        let (files, symbols, calls) = documented(40);
        let mut args = json!({"question":question,"budget":budget,"layout":"cache"});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let options = Options::parse(&args).unwrap();
        let counter = Counter::open(options.tokenizer.as_deref(), options.bytes_per_token).unwrap();
        let mut corpus = corpus(&files, &symbols, &calls, ranking);
        build(&mut corpus, &options, &counter).unwrap()
    }

    /// The text of a pack's prefix sections, in order.
    fn prefix_text(pack: &Value) -> String {
        pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["part"] == "prefix")
            .flat_map(|s| s["chunks"].as_array().unwrap())
            .map(|c| c["text"].as_str().unwrap())
            .collect()
    }

    fn section_names(pack: &Value) -> Vec<String> {
        pack["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn two_questions_share_the_repository_part_byte_for_byte() {
        for style in ["markdown", "xml"] {
            for share in [0.5, 0.9, 0.95, 1.0] {
                for budget in [8_000, 60_000] {
                    let extra = json!({"style":style,"prefix_share":share});
                    let a = cache_pack(
                        "how does F4x3 compute widgets",
                        ranking(),
                        budget,
                        extra.clone(),
                    );
                    let b = cache_pack(
                        "where do tools read file3",
                        other_ranking(),
                        budget,
                        extra.clone(),
                    );
                    let (pa, pb) = (prefix_text(&a), prefix_text(&b));
                    assert!(!pa.is_empty());
                    assert_eq!(pa, pb, "{style} {share} {budget}");
                    assert_eq!(a["prefix_sha256"], b["prefix_sha256"]);
                    assert_eq!(a["prefix_sha256"], sha256_hex(&pa));
                    assert_eq!(a["prefix_tokens"], b["prefix_tokens"]);
                    assert_eq!(a["layout"], "cache");
                    assert_eq!(a["prefix_share"], share);
                    // Neither question is in the repository part.
                    assert!(!pa.contains("F4x3 compute") && !pa.contains("read file3"));
                    if share < 1.0 {
                        assert_ne!(text(&a), text(&b), "{style} {share} {budget}");
                    } else {
                        assert_eq!(text(&a), text(&b));
                    }
                }
            }
        }
    }

    #[test]
    fn the_prefix_is_a_byte_prefix_of_the_text_and_leads_it() {
        for share in [0.5, 0.9, 1.0] {
            let pack = cache_pack(
                "how does F4x3 compute widgets",
                ranking(),
                30_000,
                json!({"prefix_share":share}),
            );
            let all = text(&pack);
            let prefix = prefix_text(&pack);
            assert!(all.starts_with(&prefix));
            assert_eq!(pack["prefix_sha256"], sha256_hex(&all[..prefix.len()]));
            // Every prefix section comes before every suffix section.
            let parts: Vec<&str> = pack["sections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["part"].as_str().unwrap())
                .collect();
            let first_suffix = parts.iter().position(|p| *p == "suffix");
            if let Some(i) = first_suffix {
                assert!(parts[i..].iter().all(|p| *p == "suffix"), "{parts:?}");
            }
            let names = section_names(&pack);
            assert_eq!(&names[..4], ["header", "architecture", "map", "breadth"]);
            if share < 1.0 {
                assert!(names.contains(&"manifest".to_string()));
                assert!(names.contains(&"focus".to_string()));
                assert_eq!(names.last().unwrap(), "focus");
            }
        }
    }

    #[test]
    fn the_cache_layout_keeps_the_budget_and_its_split() {
        for budget in [MIN_BUDGET, 900, 2_000, 8_000, 30_000, 200_000, MAX_BUDGET] {
            for share in [0.5, 0.9, 0.95, 1.0] {
                for question in ["", "how does F4x3 compute widgets"] {
                    let pack =
                        cache_pack(question, ranking(), budget, json!({"prefix_share":share}));
                    let tokens = pack["tokens"].as_u64().unwrap() as usize;
                    let prefix = pack["prefix_tokens"].as_u64().unwrap() as usize;
                    assert!(tokens <= budget, "{tokens} > {budget}");
                    assert!(
                        prefix <= ((budget as f64) * share) as usize,
                        "{prefix} over {share} of {budget}"
                    );
                    assert!(prefix <= tokens);
                    assert_eq!(tokens, recount(&pack));
                    let in_prefix: u64 = pack["sections"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|s| s["part"] == "prefix")
                        .map(|s| s["tokens"].as_u64().unwrap())
                        .sum();
                    assert_eq!(in_prefix as usize, prefix);
                    for section in pack["sections"].as_array().unwrap() {
                        let sum: u64 = section["chunks"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|c| c["tokens"].as_u64().unwrap())
                            .sum();
                        assert_eq!(section["tokens"].as_u64().unwrap(), sum);
                    }
                }
            }
        }
    }

    #[test]
    fn a_stable_pack_is_only_the_repository_part() {
        let a = cache_pack(
            "how does F4x3 compute widgets",
            ranking(),
            30_000,
            json!({"prefix_share":1.0}),
        );
        let b = cache_pack("", Vec::new(), 30_000, json!({"prefix_share":1.0}));
        assert_eq!(text(&a), text(&b));
        assert_eq!(a["prefix_tokens"], a["tokens"]);
        assert!(
            a["sections"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["part"] == "prefix")
        );
        let names = section_names(&a);
        assert_eq!(names, ["header", "architecture", "map", "breadth", "docs"]);
        let all = text(&a);
        assert!(all.contains("only the question after it changes"));
        assert!(all.contains("### README.md lines 1-3 (whole file)"));
        assert!(all.contains("docs/ARCHITECTURE.md"));
        // The prefix share alone chooses the cache layout.
        let options = Options::parse(&json!({"prefix_share":1.0})).unwrap();
        assert_eq!(options.layout, Layout::Cache);
    }

    #[test]
    fn the_question_part_refers_to_the_repository_part_instead_of_repeating_it() {
        // internal/c/file3.go is among the most connected files (whole in the
        // repository part) and ranked first for this question.
        let pack = cache_pack(
            "where do tools read file3",
            other_ranking(),
            30_000,
            json!({"prefix_share":0.5}),
        );
        let sections = pack["sections"].as_array().unwrap();
        let mut whole_in_prefix: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for section in sections.iter().filter(|s| s["part"] == "prefix") {
            for chunk in section["chunks"].as_array().unwrap() {
                if chunk["whole"] == true {
                    whole_in_prefix.insert(
                        chunk["path"].as_str().unwrap().to_string(),
                        (
                            chunk["start_line"].as_u64().unwrap(),
                            chunk["end_line"].as_u64().unwrap(),
                        ),
                    );
                }
            }
        }
        assert!(
            whole_in_prefix.contains_key("internal/c/file3.go"),
            "{whole_in_prefix:?}"
        );
        assert!(!whole_in_prefix.contains_key("cmd/tool/file6.go"));
        let mut suffix_text = String::new();
        for section in sections.iter().filter(|s| s["part"] == "suffix") {
            for chunk in section["chunks"].as_array().unwrap() {
                suffix_text.push_str(chunk["text"].as_str().unwrap());
                if let Some(path) = chunk["path"].as_str() {
                    assert!(
                        !whole_in_prefix.contains_key(path),
                        "{path} repeated in the question part"
                    );
                }
            }
        }
        let (_, end) = whole_in_prefix["internal/c/file3.go"];
        assert!(
            suffix_text.contains(&format!(
                "- in the prefix: internal/c/file3.go lines 1-{end} (rank 1)"
            )),
            "{suffix_text}"
        );
        // The second file, not whole in the repository part, is in focus.
        assert!(suffix_text.contains("### cmd/tool/file6.go lines "));
        // `included` covers both parts, with the question's ranks.
        let included: BTreeMap<&str, &Value> = pack["included"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| (f["path"].as_str().unwrap(), &f["rank"]))
            .collect();
        assert_eq!(included["internal/c/file3.go"], &json!(1));
        assert_eq!(included["cmd/tool/file6.go"], &json!(2));
    }

    #[test]
    fn the_map_lists_the_folders_and_turns_off() {
        let pack = cache_pack(
            "how does F4x3 compute widgets",
            ranking(),
            30_000,
            json!({}),
        );
        let all = text(&pack);
        assert!(all.contains("## Repository map\n"));
        assert!(all.contains("- pkg/a/: 12 files, ~"), "{}", &all[..2000]);
        assert!(all.contains("- ./: 1 file, ~"));
        assert!(all.contains(". Key: x.F4x25, x.F4x24, x.F12x31.\n"));
        let without = cache_pack(
            "how does F4x3 compute widgets",
            ranking(),
            30_000,
            json!({"sections":"arch,focus,paths,tests,docs,breadth,manifest"}),
        );
        assert!(!section_names(&without).contains(&"map".to_string()));
        // A small room keeps the most connected folders, in path order.
        let (files, symbols, calls) = documented(40);
        let corpus = corpus(&files, &symbols, &calls, ranking());
        let full = map_text(&corpus, 100_000, &Counter::Estimate);
        let small = map_text(&corpus, 90, &Counter::Estimate);
        assert!(full.starts_with("6 folders that hold files"), "{full}");
        assert!(small.starts_with("1 of the 6 folders"), "{small}");
        assert!(small.contains("- cmd/tool/:"), "{small}");
        assert!(Counter::Estimate.count(&small, estimate::PACK_TEXT_BYTES_PER_TOKEN, 0, 0) <= 90);
    }

    #[test]
    fn options_choose_the_layout_and_check_the_share() {
        let standard = Options::parse(&json!({})).unwrap();
        assert_eq!(standard.layout, Layout::Standard);
        let cache = Options::parse(&json!({"layout":"cache"})).unwrap();
        assert_eq!(cache.layout, Layout::Cache);
        assert_eq!(cache.prefix_share, DEFAULT_PREFIX_SHARE);
        let stable = Options::parse(&json!({"prefix_share":1})).unwrap();
        assert_eq!((stable.layout, stable.prefix_share), (Layout::Cache, 1.0));
        for bad in [
            json!({"layout":"standard","prefix_share":0.9}),
            json!({"prefix_share":0.05}),
            json!({"prefix_share":1.5}),
            json!({"prefix_share":"most"}),
            json!({"layout":"cached"}),
        ] {
            assert!(Options::parse(&bad).is_err(), "{bad}");
        }
    }

    /// With a tokenizer, the cache layout's parts are counted exactly and
    /// stay inside the budget and the split.
    #[test]
    fn an_exact_tokenizer_counts_the_cache_layout() {
        let dir = std::env::temp_dir().join(format!("graphicle-pack-tokc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokenizer.json");
        std::fs::write(
            &path,
            r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,
            "pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,
            "model":{"type":"WordLevel","vocab":{"[UNK]":0},"unk_token":"[UNK]"}}"#,
        )
        .unwrap();
        let tokenizer = path.to_str().unwrap().to_string();
        for budget in [MIN_BUDGET, 5_000, 60_000] {
            for share in [0.9, 1.0] {
                let pack = cache_pack(
                    "how does F4x3 compute widgets",
                    ranking(),
                    budget,
                    json!({"prefix_share":share,"tokenizer":tokenizer}),
                );
                let counter = Counter::open(Some(&tokenizer), None).unwrap();
                let Counter::Exact {
                    tokenizer: exact, ..
                } = &counter
                else {
                    unreachable!()
                };
                let tokens = pack["tokens"].as_u64().unwrap() as usize;
                assert!(tokens <= budget, "{tokens} > {budget}");
                assert_eq!(tokens, exact_count(exact, &text(&pack)));
                let prefix = pack["prefix_tokens"].as_u64().unwrap() as usize;
                assert_eq!(prefix, exact_count(exact, &prefix_text(&pack)));
                assert!(prefix <= ((budget as f64) * share) as usize);
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn a_small_question_part_gives_full_excerpts_to_fewer_ranks() {
        let ranks = |room: usize| Sizing::question_part(room).full_ranks;
        assert_eq!(
            [
                ranks(10_000),
                ranks(25_000),
                ranks(49_000),
                ranks(92_000),
                ranks(300_000)
            ],
            [3, 3, 6, 11, 15]
        );
        assert_eq!(Sizing::new(25_000, false).full_ranks, FULL_RANKS);
        // The full excerpts take at most two fifths of the room together.
        for room in [25_000, 49_000, 92_000, 139_000, 500_000] {
            let sizing = Sizing::question_part(room);
            assert!(
                sizing.full_ranks * sizing.per_file <= room * 2 / 5,
                "{room}"
            );
        }
    }
    /// A calibrated budget moves from call to call; a fixed `prefix_budget`
    /// keeps the repository part the same bytes, and the header names no
    /// budget.
    #[test]
    fn a_fixed_prefix_budget_keeps_the_repository_part_when_the_budget_moves() {
        let a = cache_pack(
            "how does F4x3 compute widgets",
            ranking(),
            56_000,
            json!({"prefix_budget":56_000}),
        );
        let b = cache_pack(
            "where do tools read file3",
            other_ranking(),
            60_794,
            json!({"prefix_budget":56_000}),
        );
        assert_eq!(a["prefix_sha256"], b["prefix_sha256"]);
        assert_ne!(a["budget"], b["budget"]);
        assert!(!prefix_text(&a).contains("Budget:"));
        // Without it, the budget sizes the part.
        let c = cache_pack(
            "how does F4x3 compute widgets",
            ranking(),
            60_794,
            json!({}),
        );
        assert_ne!(a["prefix_sha256"], c["prefix_sha256"]);
        // A fixed budget whose share does not fit steps down by 0.8 until it
        // does: 56,000, 44,800, 35,840, then 28,672 at the share 0.9.
        let d = cache_pack(
            "how does F4x3 compute widgets",
            ranking(),
            30_000,
            json!({"prefix_budget":56_000}),
        );
        let e = cache_pack(
            "how does F4x3 compute widgets",
            ranking(),
            28_672,
            json!({}),
        );
        assert_eq!(d["prefix_sha256"], e["prefix_sha256"]);
        assert!(d["tokens"].as_u64().unwrap() <= 30_000);
        assert!(Options::parse(&json!({"prefix_budget":100})).is_err());
    }
    /// When the provider counts more than the estimate, the calibrated
    /// budget falls below the fixed one's share: the repository part is then
    /// sized from a fixed step down, the same while the budget moves within
    /// the step.
    #[test]
    fn a_budget_below_the_fixed_share_steps_down_and_holds() {
        let nominal = json!({"prefix_budget":56_000});
        for (share, budgets) in [(0.9, [46_000, 49_500]), (1.0, [45_500, 48_000])] {
            let mut extra = nominal.clone();
            extra["prefix_share"] = json!(share);
            let packs: Vec<Value> = budgets
                .iter()
                .map(|&budget| {
                    cache_pack(
                        "how does F4x3 compute widgets",
                        ranking(),
                        budget,
                        extra.clone(),
                    )
                })
                .collect();
            assert_eq!(
                packs[0]["prefix_sha256"], packs[1]["prefix_sha256"],
                "{share}"
            );
            for (pack, budget) in packs.iter().zip(budgets) {
                assert!(pack["tokens"].as_u64().unwrap() <= budget as u64);
                assert!(pack["prefix_tokens"].as_u64().unwrap() <= (44_800.0 * share) as u64);
            }
            // The step's own budget gives the same part.
            let stepped = cache_pack(
                "how does F4x3 compute widgets",
                ranking(),
                44_800,
                json!({"prefix_share":share}),
            );
            assert_eq!(
                stepped["prefix_sha256"], packs[0]["prefix_sha256"],
                "{share}"
            );
        }
    }
}
