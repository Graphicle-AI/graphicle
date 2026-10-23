//! The identifier arm of `context`: the identifiers and file paths a code
//! question names ([`named`]), found in the project's passages by a
//! case-sensitive whole-word match and, for identifiers, at their
//! definitions in the code graph, ranked by file and fused into `rank` as
//! the arm `identifiers` ([`ARM`]). The identifier route answers a question
//! that only asks where names are defined or used; any other code question
//! that names identifiers keeps them here, so the files that define or use
//! them rank with the rest of the retrieval (the route's `asks_more` exit
//! used to drop them).
//!
//! A file's score sums, over the names it holds, each name's weight
//! `ln(1 + files / files holding it)`, doubled where the file defines the
//! name; a file whose path the question names adds twice the weight of a
//! name only it holds. Files come in score order, each with its two best
//! passages. The switch is `identifier_arm`: `"auto"` (left out) for a code
//! question ([`crate::code_question::is_code_question`]), `true` for any
//! question that names identifiers or paths, `false` for none.
use crate::code_graph_arm::{self, Reading};
use crate::query::Passages;
use crate::store::s;
use crate::{Result, graph_projection};
use graphicle::graph::Graph;
use graphicle::session::GraphHandle;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

/// The arm's name in `rank`'s fusion, `arm_weights` and the packet's
/// `arms`.
pub const ARM: &str = "identifiers";
/// Names one question may look for; the most specific are kept.
pub const MAX_NAMES: usize = 40;
/// Names and paths read from one question, in order of mention.
pub const MAX_CANDIDATES: usize = 160;
/// A name with more definitions than this is too common to weigh its
/// definitions.
pub const MAX_DEFINITIONS: usize = 20;
/// Passages the arm ranks.
pub const RANKED: usize = 200;
/// Passages one file gives the arm.
pub const PER_FILE: usize = 2;

/// The identifiers and file paths a text names ([`named`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Named {
    pub names: Vec<String>,
    pub paths: Vec<String>,
}

/// Extensions a named file path may have.
const PATH_EXTENSIONS: &str = "c h cc cpp cxx hpp hh hxx inl ipp tpp cu cuh rs java kt kts \
    scala cs fs vb js jsx mjs cjs ts tsx mts cts go py pyi pyx rb php swift m mm lua ex exs \
    erl hs ml mli dart jl r clj cljs zig vue svelte groovy pl pm sh bash ps1 sql proto toml \
    json yaml yml xml gradle cmake csproj props ini cfg conf md rst txt html css scss less";

/// Common English words and keywords of programming languages: a word of a
/// code block or span that is one of these is not a name.
const COMMON: &str = "the and for with that this from into are was were has have had not \
    can about there their they them then than its our your you all any did should would will \
    what which where when does how why who use used using also just like get set new old one \
    two may might must need needs make makes made see more most some such other only same each \
    true false null none nil void self super let var const mut pub impl struct enum trait mod \
    crate return returns else match while loop break continue static final public private \
    protected class interface extends implements import package include define ifdef ifndef \
    endif typedef unsigned signed int long short char float double bool boolean string str \
    async await function export default delete try catch throw throws yield def elif lambda \
    pass raise except finally assert global nonlocal print error errors warning info debug \
    line lines file files value values type types name names test tests example output input";

fn common(word: &str) -> bool {
    static WORDS: LazyLock<BTreeSet<&'static str>> =
        LazyLock::new(|| COMMON.split_whitespace().collect());
    WORDS.contains(word.to_ascii_lowercase().as_str())
}

fn path_extension(extension: &str) -> bool {
    static EXTENSIONS: LazyLock<BTreeSet<&'static str>> =
        LazyLock::new(|| PATH_EXTENSIONS.split_whitespace().collect());
    EXTENSIONS.contains(extension.to_ascii_lowercase().as_str())
}

/// A word written as code: with an underscore inside (`parse_config`,
/// `MAX_NAMES`); camelCase with two lowercase letters before its first
/// capital, digits between allowed (`parseConfig`, `utf8Decoder`; not
/// `iPhone` or `eBay`); PascalCase with a capital inside after a lowercase
/// letter or a digit (`ParseConfig`, `IsNull`); or a run of capitals before
/// a lowercase letter (`HTTPServer`).
fn code_like(word: &str) -> bool {
    static CAPITAL: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^[a-z]{2}[a-z0-9]*[A-Z]|^[A-Z][A-Za-z0-9]*[a-z0-9][A-Z]|[A-Z]{2}[a-z]")
            .expect("the inner capital rule")
    });
    word.trim_matches('_').contains('_') || CAPITAL.is_match(word)
}

/// The identifiers and file paths `text` names, each once, in order of
/// mention, at most [`MAX_CANDIDATES`]:
/// - paths: a file name or path with a known extension (`parse.rs`,
///   `src/config/parse.rs`, `globals_api.h#L172` gives `globals_api.h`);
/// - in code (a fenced block or a backtick span): every identifier of three
///   or more characters that is not a common English word or keyword, and
///   a qualified name whole (`Store::open`) with its parts;
/// - in prose: `snake_case`, `SCREAMING_CASE`, camelCase and PascalCase
///   words with a capital inside, calls (`name(`), and qualified names
///   (`a.b`, `a::b`, `a->b`, `a#b`) with a part written as code or
///   capitalized, whole and by those parts.
pub fn named(text: &str) -> Named {
    static FENCE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?s)```.*?(?:```|\z)|~~~.*?(?:~~~|\z)").expect("the fence rule")
    });
    static SPAN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"`[^`\n]+`").expect("the span rule"));
    static PATH: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?:[A-Za-z0-9_.+-]+/)*[A-Za-z0-9_+-][A-Za-z0-9_.+-]*\.([A-Za-z][A-Za-z0-9+]{0,6})\b",
        )
        .expect("the path rule")
    });
    static QUALIFIED: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\.|->|#)[A-Za-z_][A-Za-z0-9_]*)*")
            .expect("the qualified name rule")
    });
    static CALL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\(").expect("the call rule"));
    let mut named = Named::default();
    // Each once (a set, not a scan of the list), at most MAX_CANDIDATES.
    let mut seen_paths: HashSet<&str> = HashSet::new();
    for path in PATH.captures_iter(text) {
        if named.paths.len() >= MAX_CANDIDATES {
            break;
        }
        let whole = path.get(0).expect("a match").as_str();
        if path_extension(&path[1]) && whole.chars().any(|c| c.is_ascii_alphabetic()) {
            let whole = whole.trim_start_matches("./").trim_start_matches('/');
            if !whole.is_empty() && seen_paths.insert(whole) {
                named.paths.push(whole.to_string());
            }
        }
    }
    // Code keeps its offsets; prose is the text with code blanked out, in
    // place (code spans become spaces of the same length), in one pass.
    let mut blanked = text.as_bytes().to_vec();
    let mut code: Vec<(usize, String)> = Vec::new();
    for rule in [&*FENCE, &*SPAN] {
        let current = String::from_utf8_lossy(&blanked).into_owned();
        for found in rule.find_iter(&current) {
            let (a, b) = (found.start(), found.end());
            // A fenced block's first line names its language.
            let block = found.as_str();
            let skip = if block.starts_with("```") || block.starts_with("~~~") {
                block.find('\n').unwrap_or(block.len())
            } else {
                0
            };
            code.push((a + skip, block[skip..].to_string()));
            blanked[a..b].fill(b' ');
        }
    }
    let prose = String::from_utf8_lossy(&blanked).into_owned();
    code.sort();
    let mut mentions: Vec<(usize, String)> = Vec::new();
    // A qualified name: whole unless it is a file name, and its parts that
    // read as code (in code: any part of three or more characters).
    let qualified = |at: usize, token: &str, in_code: bool, out: &mut Vec<(usize, String)>| {
        let parts: Vec<&str> = token
            .split(['.', ':', '#', '-', '>'])
            .filter(|p| !p.is_empty())
            .collect();
        // `globals_api.h#L172` names a file, not a symbol.
        let file = parts.iter().skip(1).any(|part| path_extension(part));
        let anchor = |part: &str| {
            part.strip_prefix('L')
                .is_some_and(|line| !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()))
        };
        let worded = |part: &str| {
            // A file name's extension is no name.
            let extension = file && path_extension(part);
            part.len() >= 3
                && !common(part)
                && !anchor(part)
                && !extension
                && (in_code
                    || code_like(part)
                    || part.starts_with(|c: char| c.is_ascii_uppercase()))
        };
        if parts.len() > 1 {
            let shows_code = parts
                .iter()
                .any(|p| code_like(p) || p.starts_with(|c: char| c.is_ascii_uppercase()))
                && !parts
                    .iter()
                    .all(|p| p.chars().all(|c| c.is_ascii_lowercase()));
            if !(in_code || shows_code) {
                return;
            }
            if !file {
                out.push((at, token.to_string()));
            }
        }
        for part in &parts {
            if worded(part) && (parts.len() > 1 || in_code || code_like(part)) {
                out.push((at, (*part).to_string()));
            }
        }
    };
    for (at, block) in &code {
        for token in QUALIFIED.find_iter(block) {
            qualified(at + token.start(), token.as_str(), true, &mut mentions);
        }
    }
    for token in QUALIFIED.find_iter(&prose) {
        qualified(token.start(), token.as_str(), false, &mut mentions);
    }
    for call in CALL.captures_iter(&prose) {
        let name = &call[1];
        if name.len() >= 3 && !common(name) {
            mentions.push((call.get(1).expect("a name").start(), name.to_string()));
        }
    }
    mentions.sort_by_key(|(at, _)| *at);
    let mut seen_names: HashSet<String> = HashSet::new();
    for (_, name) in mentions {
        if named.names.len() + named.paths.len() >= MAX_CANDIDATES {
            break;
        }
        if !name.is_empty() && seen_names.insert(name.clone()) {
            named.names.push(name);
        }
    }
    named
}

/// What [`arm`] gives one project's `context` call.
pub struct Identifiers {
    /// The arm's ranking of passage identities, when it ran.
    pub ranking: Option<Vec<String>>,
    /// The files (`file:` origins) that define a code name the question
    /// names when no source file defines it (a test helper, a fixture, an
    /// example in a document): the code-first prior leaves their weight
    /// ([`crate::code_question::DEFINING`]).
    pub defining: Vec<String>,
    /// The packet's `identifier_arm` report.
    pub report: Value,
}

/// The arm for one project's `context` call ([`Identifiers`]).
pub fn arm(
    db: &Connection,
    repository: &str,
    passages: &Passages,
    args: &Value,
    question: &str,
    query: &str,
) -> Result<Identifiers> {
    let (mode, defaulted) = match args.get("identifier_arm") {
        None | Some(Value::Null) => (None, true),
        Some(Value::Bool(on)) => (Some(*on), false),
        Some(Value::String(mode)) if mode == "auto" => (None, false),
        Some(_) => {
            return Err(crate::Error::bad(
                "identifier_arm must be true, false or auto",
            ));
        }
    };
    let setting = mode.map_or(json!("auto"), |on| json!(on));
    let mut report = json!({"identifier_arm": setting, "defaulted": defaulted});
    let on = mode.unwrap_or_else(|| crate::code_question::is_code_question(question));
    let skipped = |report: Value| Identifiers {
        ranking: None,
        defining: Vec::new(),
        report,
    };
    if !on {
        report["status"] = json!("off");
        return Ok(skipped(report));
    }
    let named = named(query);
    if named.names.is_empty() && named.paths.is_empty() {
        report["status"] = json!("no_names");
        return Ok(skipped(report));
    }
    let graph = if args["graph_retrieval"] == false {
        None
    } else {
        crate::phase::phase("identifiers_graph", || code_graph(db, repository))
    };
    let ranked = crate::phase::phase("identifiers_rank", || {
        rank(&passages.rows, &named, graph.as_ref())
    });
    report["status"] = json!(if ranked.ranking.is_empty() {
        "no_match"
    } else {
        "fused"
    });
    report["names"] = json!(ranked.names);
    report["paths"] = json!(ranked.paths);
    report["defined"] = json!(ranked.defined);
    report["files"] = json!(ranked.files);
    report["candidates"] = json!(ranked.ranking.len());
    report["code_graph"] = json!(graph.is_some());
    report["defining_files_outside_source"] = json!(ranked.defining.len());
    Ok(Identifiers {
        ranking: (!ranked.ranking.is_empty()).then_some(ranked.ranking),
        defining: ranked.defining,
        report,
    })
}

/// The code graph the code-graph arm reads, with its symbols; `None` when
/// the project has no recorded episode or it cannot be opened (the arm then
/// matches text alone).
struct CodeGraph {
    handle: GraphHandle,
    /// Code symbols: (node index, dotted name, file).
    symbols: Vec<(usize, String, String)>,
    /// Symbols by the last segment of their dotted name.
    by_last: HashMap<String, Vec<usize>>,
    /// Files the lag fallback may cite (`None`: all).
    unchanged: Option<BTreeSet<String>>,
}

fn code_graph(db: &Connection, repository: &str) -> Option<CodeGraph> {
    let heads = crate::store::heads(db, repository).ok()?;
    let current = graph_projection::episode_of(&heads);
    let Reading { episode, lag } = code_graph_arm::reading(db, repository, &heads, &current)
        .ok()
        .flatten()?;
    let handle = graph_projection::resident(db, repository, &episode).ok()?;
    let graph: &Graph = &handle;
    let symbols: Vec<(usize, String, String)> = graph
        .nodes()
        .iter()
        .enumerate()
        .filter(|(_, node)| code_graph_arm::code_symbol(&node.attributes))
        .filter_map(|(index, node)| {
            Some((
                index,
                crate::identifier_route::symbol_name(&node.attributes)?,
                code_graph_arm::relpath(&node.attributes)?,
            ))
        })
        .collect();
    let mut by_last: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (_, name, _)) in symbols.iter().enumerate() {
        let last = name.rsplit('.').next().unwrap_or(name);
        by_last.entry(last.to_string()).or_default().push(i);
    }
    Some(CodeGraph {
        handle,
        symbols,
        by_last,
        unchanged: lag.map(|lag| lag.unchanged),
    })
}

impl CodeGraph {
    /// The symbols `name` defines, as the identifier route finds them
    /// (`crate::identifier_route`'s longest matching suffix), by index into
    /// `symbols`.
    fn definitions(&self, name: &str) -> Vec<usize> {
        let dotted = name
            .replace("::", ".")
            .replace(['#'], ".")
            .replace("->", ".");
        let mut segments: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
        while segments.len() > 1 && ["self", "this", "crate", "super"].contains(&segments[0]) {
            segments.remove(0);
        }
        let Some(last) = segments.last() else {
            return Vec::new();
        };
        let Some(bucket) = self.by_last.get(*last) else {
            return Vec::new();
        };
        let ends_with = |qualified: &str, path: &str| {
            qualified == path
                || qualified
                    .strip_suffix(path)
                    .is_some_and(|head| head.ends_with('.'))
        };
        for k in 0..segments.len() {
            let suffix = segments[k..].join(".");
            let last_only = k + 1 == segments.len() && segments.len() > 1;
            let found: Vec<usize> = bucket
                .iter()
                .copied()
                .filter(|&i| {
                    let (_, qualified, relpath) = &self.symbols[i];
                    ends_with(qualified, &suffix)
                        && (!last_only
                            || relpath
                                .split(['/', '.'])
                                .any(|part| segments[..k].contains(&part)))
                })
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }
}

/// What [`rank`] found.
struct Ranked {
    ranking: Vec<String>,
    names: Vec<String>,
    paths: Vec<String>,
    defined: usize,
    files: usize,
    /// The files (`file:` origins) that define one of the code names no
    /// source file defines ([`defining_outside_source`]).
    defining: Vec<String>,
}

/// The arm's passage order over `rows` (the query's passages) for `named`.
fn rank(rows: &[Value], named: &Named, graph: Option<&CodeGraph>) -> Ranked {
    // Passages of project files, by file in first-seen order.
    let mut files: Vec<&str> = Vec::new();
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let Some(origin) = row["origin"].as_str().filter(|o| o.starts_with("file:")) else {
            continue;
        };
        if row["kind"] != "source" {
            continue;
        }
        by_file
            .entry(origin)
            .or_insert_with(|| {
                files.push(origin);
                Vec::new()
            })
            .push(i);
    }
    let total = files.len().max(1) as f64;
    // Whole-word, case-sensitive matches of every candidate name, longest
    // first so `Store::open` is found before `Store`.
    let mut candidates: Vec<&String> = named.names.iter().collect();
    candidates.sort_by_key(|name| std::cmp::Reverse(name.len()));
    let mut counts: Vec<Vec<(usize, u32)>> = vec![Vec::new(); rows.len()];
    if !candidates.is_empty() {
        let pattern = candidates
            .iter()
            .map(|name| regex::escape(name))
            .collect::<Vec<_>>()
            .join("|");
        if let Ok(matcher) = regex::Regex::new(&format!(r"(?-u:\b)(?:{pattern})(?-u:\b)")) {
            let index: HashMap<&str, usize> = named
                .names
                .iter()
                .enumerate()
                .map(|(i, name)| (name.as_str(), i))
                .collect();
            for list in by_file.values() {
                for &i in list {
                    let mut found: BTreeMap<usize, u32> = BTreeMap::new();
                    for hit in matcher.find_iter(s(&rows[i], "text")) {
                        if let Some(&name) = index.get(hit.as_str()) {
                            *found.entry(name).or_default() += 1;
                        }
                    }
                    counts[i] = found.into_iter().collect();
                }
            }
        }
    }
    // Files holding each name, by text and by definition.
    let mut holders: Vec<BTreeSet<&str>> = vec![BTreeSet::new(); named.names.len()];
    for (&file, list) in &by_file {
        for &i in list {
            for &(name, _) in &counts[i] {
                holders[name].insert(file);
            }
        }
    }
    // Definitions, for names specific enough: per name, the files and
    // start lines that define it.
    let mut defined: Vec<Vec<(String, usize)>> = vec![Vec::new(); named.names.len()];
    if let Some(graph) = graph {
        for (n, name) in named.names.iter().enumerate() {
            let found = graph.definitions(name);
            if found.is_empty() || found.len() > MAX_DEFINITIONS {
                continue;
            }
            for i in found {
                let (index, _, relpath) = &graph.symbols[i];
                if graph
                    .unchanged
                    .as_ref()
                    .is_some_and(|unchanged| !unchanged.contains(relpath))
                {
                    continue;
                }
                let origin = format!("file:{relpath}");
                let Some((&file, _)) = by_file.get_key_value(origin.as_str()) else {
                    continue;
                };
                let line = graphicle_memory::graph::span::start_line(
                    &graph.handle.nodes()[*index].attributes,
                );
                defined[n].push((file.to_string(), line.unwrap_or(0)));
                holders[n].insert(file);
            }
        }
    }
    // The most specific names: fewest holding files first.
    let mut kept: Vec<usize> = (0..named.names.len())
        .filter(|&n| !holders[n].is_empty())
        .collect();
    kept.sort_by_key(|&n| (holders[n].len(), n));
    kept.truncate(MAX_NAMES);
    // In `kept` order, so every sum adds in the same order.
    let weights: Vec<(usize, f64)> = kept
        .iter()
        .map(|&n| (n, (1.0 + total / holders[n].len() as f64).ln()))
        .collect();
    // Named paths: the files whose path ends with one, or that one ends with.
    let path_weight = 2.0 * (1.0 + total).ln();
    let mut path_bonus: HashMap<&str, f64> = HashMap::new();
    let mut paths_found = Vec::new();
    for path in &named.paths {
        let path = path.replace('\\', "/");
        let matching: Vec<&str> = files
            .iter()
            .copied()
            .filter(|origin| {
                let file = &origin[5..];
                file == path
                    || file.ends_with(&format!("/{path}"))
                    || path.ends_with(&format!("/{file}"))
            })
            .collect();
        if matching.is_empty() {
            continue;
        }
        paths_found.push(path.clone());
        for &file in &matching {
            *path_bonus.entry(file).or_default() += path_weight / matching.len() as f64;
        }
    }
    // A file or passage holding a name's definition counts the name twice.
    let defines = |n: usize, file: &str| defined[n].iter().any(|(f, _)| f == file);
    let passage_defines = |n: usize, row: &Value| {
        let file = s(row, "origin");
        let (start, end) = (
            row["source"]["line"].as_u64().unwrap_or(0) as usize,
            row["source"]["end_line"].as_u64().unwrap_or(0) as usize,
        );
        defined[n]
            .iter()
            .any(|(f, line)| f == file && *line >= start.max(1) && (end == 0 || *line <= end))
    };
    let mut scored: Vec<(f64, u32, &str)> = Vec::new();
    for &file in &files {
        let list = &by_file[file];
        let mut score = path_bonus.get(file).copied().unwrap_or(0.0);
        let mut hits = 0u32;
        for &(n, w) in &weights {
            let in_text: u32 = list
                .iter()
                .flat_map(|&i| counts[i].iter())
                .filter(|(name, _)| *name == n)
                .map(|(_, c)| *c)
                .sum();
            let here = defines(n, file);
            if in_text > 0 || here {
                score += if here { 2.0 * w } else { w };
                hits += in_text;
            }
        }
        if score > 0.0 {
            scored.push((score, hits, file));
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
    let mut ranking = Vec::new();
    for (_, _, file) in &scored {
        let list = &by_file[file];
        let mut passages: Vec<(f64, usize)> = list
            .iter()
            .map(|&i| {
                let mut score = 0.0;
                for &(n, w) in &weights {
                    let in_text = counts[i].iter().any(|(name, _)| *name == n);
                    let here = passage_defines(n, &rows[i]);
                    if in_text || here {
                        score += if here { 2.0 * w } else { w };
                    }
                }
                (score, i)
            })
            .collect();
        passages.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let best = passages.first().map_or(0.0, |p| p.0);
        for (score, i) in passages.into_iter().take(PER_FILE) {
            // A file the question names by path gives its first passage
            // even when no name is in it.
            if score <= 0.0 && best > 0.0 {
                break;
            }
            ranking.push(s(&rows[i], "identity").to_string());
            if best <= 0.0 {
                break;
            }
        }
        if ranking.len() >= RANKED {
            break;
        }
    }
    ranking.truncate(RANKED);
    let mut names: Vec<String> = kept.iter().map(|&n| named.names[n].clone()).collect();
    names.sort();
    let defining = defining_outside_source(&named.names, &kept, &defined);
    Ranked {
        ranking,
        names,
        paths: paths_found,
        defined: kept.iter().filter(|&&n| !defined[n].is_empty()).count(),
        files: scored.len(),
        defining,
    }
}

/// The text of the passages of some files of a passage index, by (file,
/// passage): what [`arm_indexed`] matches names with separators in.
pub type PassageTexts<'a> =
    dyn FnMut(&BTreeSet<usize>) -> Result<HashMap<(usize, u32), String>> + 'a;

/// [`arm`] over a project's passage index ([`crate::passage_index`])
/// instead of its chunked passages: the same ranking, from the index's
/// counts of each name. A name with a separator (`Store::open`, `a.b`) is
/// counted by matching the text of the passages that hold its words, with
/// every name, as [`rank`] matches it; `text` gives those passages' text
/// (file, passage), by index.
pub fn arm_indexed(
    db: &Connection,
    repository: &str,
    index: &crate::passage_index::Index,
    text: &mut PassageTexts<'_>,
    args: &Value,
    question: &str,
    query: &str,
) -> Result<Identifiers> {
    let (mode, defaulted) = match args.get("identifier_arm") {
        None | Some(Value::Null) => (None, true),
        Some(Value::Bool(on)) => (Some(*on), false),
        Some(Value::String(mode)) if mode == "auto" => (None, false),
        Some(_) => {
            return Err(crate::Error::bad(
                "identifier_arm must be true, false or auto",
            ));
        }
    };
    let setting = mode.map_or(json!("auto"), |on| json!(on));
    let mut report = json!({"identifier_arm": setting, "defaulted": defaulted});
    let on = mode.unwrap_or_else(|| crate::code_question::is_code_question(question));
    let skipped = |report: Value| Identifiers {
        ranking: None,
        defining: Vec::new(),
        report,
    };
    if !on {
        report["status"] = json!("off");
        return Ok(skipped(report));
    }
    let named = named(query);
    if named.names.is_empty() && named.paths.is_empty() {
        report["status"] = json!("no_names");
        return Ok(skipped(report));
    }
    let graph = if args["graph_retrieval"] == false {
        None
    } else {
        crate::phase::phase("identifiers_graph", || code_graph(db, repository))
    };
    let ranked = crate::phase::phase("identifiers_rank", || {
        rank_indexed(db, index, text, &named, graph.as_ref())
    })?;
    report["status"] = json!(if ranked.ranking.is_empty() {
        "no_match"
    } else {
        "fused"
    });
    report["names"] = json!(ranked.names);
    report["paths"] = json!(ranked.paths);
    report["defined"] = json!(ranked.defined);
    report["files"] = json!(ranked.files);
    report["candidates"] = json!(ranked.ranking.len());
    report["code_graph"] = json!(graph.is_some());
    report["defining_files_outside_source"] = json!(ranked.defining.len());
    Ok(Identifiers {
        ranking: (!ranked.ranking.is_empty()).then_some(ranked.ranking),
        defining: ranked.defining,
        report,
    })
}

/// Whether `name` is one ASCII identifier, which the index counts whole.
fn simple_name(name: &str) -> bool {
    (3..=128).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// [`rank`] over the passage index: the files in path order (the order the
/// passages of a project's files come in), each name's counts per passage.
fn rank_indexed(
    db: &Connection,
    index: &crate::passage_index::Index,
    text: &mut PassageTexts<'_>,
    named: &Named,
    graph: Option<&CodeGraph>,
) -> Result<Ranked> {
    // Files with passages, as `rank` reads them from the rows.
    let files: Vec<usize> = (0..index.files.len())
        .filter(|&f| !index.files[f].passages.is_empty())
        .collect();
    let origin = |f: usize| format!("file:{}", index.files[f].path);
    let total = files.len().max(1) as f64;
    // (file, passage) → [(name, count)] by name, as `rank` counts them.
    let mut counts: BTreeMap<(usize, u32), BTreeMap<usize, u32>> = BTreeMap::new();
    let mut candidates: Vec<&String> = named.names.iter().collect();
    candidates.sort_by_key(|name| std::cmp::Reverse(name.len()));
    if !candidates.is_empty() {
        let pattern = candidates
            .iter()
            .map(|name| regex::escape(name))
            .collect::<Vec<_>>()
            .join("|");
        if let Ok(matcher) = regex::Regex::new(&format!(r"(?-u:\b)(?:{pattern})(?-u:\b)")) {
            let mut verify: BTreeSet<(usize, u32)> = BTreeSet::new();
            let mut stemmer = graphicle_memory::stem::Stemmer::default();
            for (n, name) in named.names.iter().enumerate() {
                if simple_name(name) {
                    let term = format!("{}{name}", crate::passage_index::IDENT);
                    for (file, passage, count) in index.postings(db, &term)? {
                        counts.entry((file, passage)).or_default().insert(n, count);
                    }
                    continue;
                }
                // A name with separators: the passages that hold all its
                // words, matched below.
                let words = graphicle_memory::text::words(name);
                let mut holding: Option<BTreeSet<(usize, u32)>> = None;
                for word in words {
                    let found: BTreeSet<(usize, u32)> = index
                        .postings(db, &stemmer.stem(&word))?
                        .into_iter()
                        .map(|(file, passage, _)| (file, passage))
                        .collect();
                    holding = Some(match holding {
                        None => found,
                        Some(before) => before.intersection(&found).copied().collect(),
                    });
                }
                verify.extend(holding.unwrap_or_default());
            }
            if !verify.is_empty() {
                let wanted: BTreeSet<usize> = verify.iter().map(|(file, _)| *file).collect();
                let texts = text(&wanted)?;
                let position: HashMap<&str, usize> = named
                    .names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (name.as_str(), i))
                    .collect();
                for key in verify {
                    let Some(passage_text) = texts.get(&key) else {
                        continue;
                    };
                    let mut found: BTreeMap<usize, u32> = BTreeMap::new();
                    for hit in matcher.find_iter(passage_text) {
                        if let Some(&name) = position.get(hit.as_str()) {
                            *found.entry(name).or_default() += 1;
                        }
                    }
                    if found.is_empty() {
                        counts.remove(&key);
                    } else {
                        counts.insert(key, found);
                    }
                }
            }
        }
    }
    // Files holding each name, by text and by definition.
    let mut holders: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); named.names.len()];
    for ((file, _), found) in &counts {
        for &name in found.keys() {
            holders[name].insert(*file);
        }
    }
    let mut defined: Vec<Vec<(usize, usize)>> = vec![Vec::new(); named.names.len()];
    if let Some(graph) = graph {
        for (n, name) in named.names.iter().enumerate() {
            let found = graph.definitions(name);
            if found.is_empty() || found.len() > MAX_DEFINITIONS {
                continue;
            }
            for i in found {
                let (node, _, relpath) = &graph.symbols[i];
                if graph
                    .unchanged
                    .as_ref()
                    .is_some_and(|unchanged| !unchanged.contains(relpath))
                {
                    continue;
                }
                let Some(file) = index
                    .file(relpath)
                    .filter(|&f| !index.files[f].passages.is_empty())
                else {
                    continue;
                };
                let line = graphicle_memory::graph::span::start_line(
                    &graph.handle.nodes()[*node].attributes,
                );
                defined[n].push((file, line.unwrap_or(0)));
                holders[n].insert(file);
            }
        }
    }
    let mut kept: Vec<usize> = (0..named.names.len())
        .filter(|&n| !holders[n].is_empty())
        .collect();
    kept.sort_by_key(|&n| (holders[n].len(), n));
    kept.truncate(MAX_NAMES);
    let weights: Vec<(usize, f64)> = kept
        .iter()
        .map(|&n| (n, (1.0 + total / holders[n].len() as f64).ln()))
        .collect();
    let path_weight = 2.0 * (1.0 + total).ln();
    let mut path_bonus: HashMap<usize, f64> = HashMap::new();
    let mut paths_found = Vec::new();
    for path in &named.paths {
        let path = path.replace('\\', "/");
        let matching: Vec<usize> = files
            .iter()
            .copied()
            .filter(|&f| {
                let file = index.files[f].path.as_str();
                file == path
                    || file.ends_with(&format!("/{path}"))
                    || path.ends_with(&format!("/{file}"))
            })
            .collect();
        if matching.is_empty() {
            continue;
        }
        paths_found.push(path.clone());
        for &file in &matching {
            *path_bonus.entry(file).or_default() += path_weight / matching.len() as f64;
        }
    }
    let defines = |n: usize, file: usize| defined[n].iter().any(|(f, _)| *f == file);
    let passage_defines = |n: usize, file: usize, passage: &crate::passage_index::Passage| {
        let (start, end) = (passage.line, passage.end_line);
        defined[n]
            .iter()
            .any(|(f, line)| *f == file && *line >= start.max(1) && (end == 0 || *line <= end))
    };
    // Per file, each name's count over its passages.
    let mut in_file: BTreeMap<usize, BTreeMap<usize, u32>> = BTreeMap::new();
    for ((file, _), found) in &counts {
        let entry = in_file.entry(*file).or_default();
        for (&name, &count) in found {
            *entry.entry(name).or_default() += count;
        }
    }
    let mut considered: BTreeSet<usize> = in_file.keys().copied().collect();
    considered.extend(path_bonus.keys().copied());
    for list in &defined {
        considered.extend(list.iter().map(|(file, _)| *file));
    }
    let mut scored: Vec<(f64, u32, usize)> = Vec::new();
    for &file in &considered {
        let mut score = path_bonus.get(&file).copied().unwrap_or(0.0);
        let mut hits = 0u32;
        for &(n, w) in &weights {
            let in_text = in_file
                .get(&file)
                .and_then(|names| names.get(&n))
                .copied()
                .unwrap_or(0);
            let here = defines(n, file);
            if in_text > 0 || here {
                score += if here { 2.0 * w } else { w };
                hits += in_text;
            }
        }
        if score > 0.0 {
            scored.push((score, hits, file));
        }
    }
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(origin(a.2).cmp(&origin(b.2)))
    });
    let mut ranking = Vec::new();
    for (_, _, file) in &scored {
        let entry = &index.files[*file];
        let mut passages: Vec<(f64, usize)> = entry
            .passages
            .iter()
            .enumerate()
            .map(|(p, passage)| {
                let found = counts.get(&(*file, p as u32));
                let mut score = 0.0;
                for &(n, w) in &weights {
                    let in_text = found.is_some_and(|names| names.contains_key(&n));
                    let here = passage_defines(n, *file, passage);
                    if in_text || here {
                        score += if here { 2.0 * w } else { w };
                    }
                }
                (score, p)
            })
            .collect();
        passages.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let best = passages.first().map_or(0.0, |p| p.0);
        for (score, p) in passages.into_iter().take(PER_FILE) {
            if score <= 0.0 && best > 0.0 {
                break;
            }
            ranking.push(format!("file:{}:{}", entry.path, entry.passages[p].start));
            if best <= 0.0 {
                break;
            }
        }
        if ranking.len() >= RANKED {
            break;
        }
    }
    ranking.truncate(RANKED);
    let mut names: Vec<String> = kept.iter().map(|&n| named.names[n].clone()).collect();
    names.sort();
    let defined_paths: Vec<Vec<(String, usize)>> = defined
        .iter()
        .map(|list| list.iter().map(|(f, line)| (origin(*f), *line)).collect())
        .collect();
    let defining = defining_outside_source(&named.names, &kept, &defined_paths);
    Ok(Ranked {
        ranking,
        names,
        paths: paths_found,
        defined: kept.iter().filter(|&&n| !defined[n].is_empty()).count(),
        files: scored.len(),
        defining,
    })
}

/// The files that define one of the `kept` names written as code
/// ([`code_like`]) when no source file defines that name. A name a source
/// file defines points at that source, so a test or a document that defines
/// it too (a mock, a copy in an example) stays lowered; a plain word a test
/// defines (`repos`, `Book`) says little about that test. A test helper or a
/// fixture only tests define (`make_client`) keeps its test's weight.
fn defining_outside_source(
    names: &[String],
    kept: &[usize],
    defined: &[Vec<(String, usize)>],
) -> Vec<String> {
    let source = |origin: &str| {
        crate::code_question::path_class(origin.strip_prefix("file:").unwrap_or(origin))
            == crate::code_question::PathClass::Source
    };
    let files: BTreeSet<&String> = kept
        .iter()
        .filter(|&&n| code_like(&names[n]))
        .filter(|&&n| !defined[n].iter().any(|(file, _)| source(file)))
        .flat_map(|&n| defined[n].iter().map(|(file, _)| file))
        .collect();
    files.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_paths_are_read_from_code_and_prose() {
        let text = "`noinline` breaks in core/lib/globals_api.h#L172 and Store::open \
            calls parse_config() with HTTPServer.\n```\nlet header = read_header(buf);\n```\n\
            See the e.g. docs and version 1.2.";
        let named = named(text);
        assert_eq!(named.paths, ["core/lib/globals_api.h"]);
        for name in [
            "noinline",
            "globals_api",
            "Store::open",
            "Store",
            "parse_config",
            "HTTPServer",
            "header",
            "read_header",
            "buf",
        ] {
            assert!(named.names.contains(&name.to_string()), "{name}: {named:?}");
        }
        for word in ["breaks", "calls", "docs", "let", "open", "version"] {
            assert!(
                !named.names.contains(&word.to_string()),
                "{word}: {named:?}"
            );
        }
        for word in [
            "parseConfig",
            "ParseConfig",
            "utf8Decoder",
            "int64Value",
            "HTTPServer",
            "IsNull",
            "MyModel",
        ] {
            assert!(code_like(word), "{word}");
        }
        for word in ["iPhone", "eBay", "iPad", "iOS", "x86", "Parse"] {
            assert!(!code_like(word), "{word}");
        }
    }

    fn row(identity: &str, path: &str, text: &str) -> Value {
        json!({"identity":identity,"kind":"source","origin":format!("file:{path}"),"text":text,
            "source":{"id":format!("file:{path}"),"path":path,"line":1,"end_line":text.lines().count()}})
    }

    #[test]
    fn files_rank_by_the_specific_names_they_hold_and_named_paths_first() {
        let rows = vec![
            row("a0", "src/a.rs", "fn other() {}\nlet x = common_name;"),
            row("b0", "src/b.rs", "fn parse_config() { common_name }"),
            row("b1", "src/b.rs", "fn unrelated() {}"),
            row(
                "c0",
                "src/c.rs",
                "parse_config(); common_name; parse_config();",
            ),
            row("d0", "docs/guide.md", "nothing here"),
            row("e0", "src/e.rs", "common_name"),
        ];
        let named = Named {
            names: vec!["parse_config".into(), "common_name".into()],
            paths: vec!["docs/guide.md".into()],
        };
        let ranked = rank(&rows, &named, None);
        // The named path first; then the files with the rarer name, the one
        // with more matches first; then the common name alone.
        assert_eq!(ranked.ranking, ["d0", "c0", "b0", "a0", "e0"]);
        assert_eq!(ranked.files, 5);
        assert_eq!(ranked.paths, ["docs/guide.md"]);
        // No name or path in the project: nothing ranked.
        let none = Named {
            names: vec!["absent_name".into()],
            paths: vec![],
        };
        assert!(rank(&rows, &none, None).ranking.is_empty());
    }

    #[test]
    fn only_code_names_no_source_file_defines_keep_their_definers_weight() {
        let at = |file: &str| (format!("file:{file}"), 1);
        let names: Vec<String> = [
            "run_agent",
            "make_client",
            "exampleConfig",
            "repos",
            "absent_name",
        ]
        .iter()
        .map(|name| name.to_string())
        .collect();
        let defined = vec![
            // A source file and a test define it: the source is what the
            // question names, and the test stays lowered.
            vec![at("src/agent.rs"), at("tests/agent.rs")],
            // Only a test defines it (a fixture): that test keeps its weight.
            vec![at("tests/conftest.py")],
            // Only a document defines it: so does the document.
            vec![at("docs/example.md")],
            // A plain word only a test defines: the test stays lowered.
            vec![at("tests/workspace.test.ts")],
            // Nothing defines it.
            vec![],
        ];
        assert_eq!(
            defining_outside_source(&names, &[0, 1, 2, 3, 4], &defined),
            ["file:docs/example.md", "file:tests/conftest.py"]
        );
        assert!(defining_outside_source(&names, &[0, 3, 4], &defined).is_empty());
    }
}
