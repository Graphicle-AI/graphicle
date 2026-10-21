//! Reviewed memory anchored to code symbols.
//!
//! **Links.** A memory's quotes in repository files resolve to the code
//! symbols that contain them (rung `quote`); `propose_memory` and
//! `note` can name symbols (rung `named`); a retained answer
//! keeps the code-graph facts of the windows it quoted (rung
//! `answer_window`). The links go to the memory's `data.symbols`, each with
//! the symbol's `stable_id` and `revision`, its logical identity (relpath,
//! qualified name, kind, ordinal), the episode it was read at and the
//! symbol's byte range there. `data.symbol_files` says, per cited file, how
//! its staleness is judged ([`RULE_SYMBOLS`], [`RULE_PENDING`], or a reason
//! for the file rule). A memory without code links stores neither field.
//!
//! **Staleness by symbol**. A changed file whose grounds all lie in
//! code symbols withholds its memory only when one of those symbols changed
//! (`symbol_changed:<id>`), is gone (`symbol_missing:<id>`), or cannot be
//! judged until a projection runs (`projection_pending:<id>`, or
//! `projection_pending:file:<path>` for a quote not yet pinned). An exact
//! move is followed: a symbol that left while one of
//! identical revision, kind and name tail arrived. A quote in a module
//! header or outside code symbols, a quote across several symbols (their
//! revisions do not say whether they are still adjacent), a file without
//! code symbols, a quote found more than [`MAX_OCCURRENCES`] times, and a
//! file named as a whole keep the file rule. So a file judged by its symbols
//! has every quote occurrence inside one linked symbol, whose unchanged
//! revision means unchanged bytes. The check is a read of indexed rows,
//! never of the graph: [`Check`].
//!
//! **Tables.** `gf_symbol_heads` holds the code symbols of the head set
//! `gf_symbol_state` names (one row per `stable_id`, with the lines of the
//! bytes its revision hashes and the file's sha256). A projection of the
//! current head set keeps it ([`record`], by changed file when it described
//! the previous episode, else whole), and [`ensure`] rebuilds it from the
//! resident graph when the head set returns to a recorded episode.
//! `gf_symbol_moves` holds the exact moves; `gf_symbol_pins` holds quote
//! occurrences written before their file's symbols were projected, pinned to
//! their symbols by the next projection of that content ([`pin_pending`]).
//! Pins go with their memory; the other tables with their project.
use crate::{
    Error, Result, graph_projection, now,
    store::{rows, s},
    string,
};
use graphicle::graph::Graph;
use graphicle_memory::graph::{identity::LogicalIdentity, span};
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// A quote resolved to the symbols that contain it.
pub const RUNG_QUOTE: &str = "quote";
/// A symbol a caller named by `stable_id`, or by path and qualified name.
pub const RUNG_NAMED: &str = "named";
/// A retained answer's cited window, as the code-graph arm described it.
pub const RUNG_ANSWER: &str = "answer_window";

/// `data.symbol_files`: the file's staleness follows its linked symbols.
pub const RULE_SYMBOLS: &str = "symbols";
/// Quotes in a code file whose symbols were not projected yet: they follow
/// their symbols once pinned, the file rule if a pin finds bytes outside.
pub const RULE_PENDING: &str = "pending";
/// File rule: some quoted bytes lie outside code symbols (a module header,
/// a comment or section node between symbols).
pub const RULE_OUTSIDE: &str = "outside_symbols";
/// File rule: a quote occurrence runs across several code symbols. Each
/// keeping its revision does not keep the quote: a symbol inserted between
/// them, the two reordered or moved apart leave both revisions unchanged.
pub const RULE_SPANS: &str = "spans_symbols";
/// File rule: a quote occurs more than [`MAX_OCCURRENCES`] times.
pub const RULE_REPEATED: &str = "repeated_quote";
/// File rule: the file is named as a whole (`note`'s `dependencies`,
/// `propose_memory`'s `files`).
pub const RULE_NAMED_FILE: &str = "named_file";
/// File rule: `memory.import` could not re-ground a link in this file.
pub const RULE_UNRESOLVED: &str = "unresolved";

/// Occurrences of one quote that are linked; more keep the file rule.
pub const MAX_OCCURRENCES: usize = 4;
/// Links one memory records; past it every file keeps the file rule.
pub const MAX_LINKS: usize = 200;
/// Symbols `propose_memory` and `note` accept by name.
pub const MAX_NAMED: usize = 50;
/// Exact moves followed from one link.
const MAX_MOVES: usize = 8;

/// One row of `gf_symbol_heads`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    pub stable_id: String,
    pub revision: String,
    pub identity: LogicalIdentity,
    pub node_id: String,
    pub line_start: usize,
    /// The next symbol start in the file (the slice ends before it); `None`
    /// runs to the end of the file.
    pub line_next: Option<usize>,
    /// The file's sha256 in the head set the row describes.
    pub sha256: String,
}

impl Live {
    /// The bytes the revision hashes, in `content` (the file at `sha256`).
    pub fn range(&self, starts: &[usize], len: usize) -> std::ops::Range<usize> {
        span::byte_range(starts, len, Some(self.line_start), self.line_next)
    }
    fn tail(&self) -> &str {
        tail(&self.identity.qualified_name)
    }
}

fn tail(qualified: &str) -> &str {
    qualified.rsplit('.').next().unwrap_or(qualified)
}

const COLUMNS: &str =
    "stable_id,revision,relpath,qualified_name,kind,ordinal,node_id,line_start,line_next,sha256";

fn select(db: &Connection, filter: &str, arguments: impl rusqlite::Params) -> Result<Vec<Live>> {
    let mut statement = db.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM gf_symbol_heads WHERE {filter}"
    ))?;
    let found = statement.query_map(arguments, |row| {
        Ok(Live {
            stable_id: row.get(0)?,
            revision: row.get(1)?,
            identity: LogicalIdentity {
                relpath: row.get(2)?,
                qualified_name: row.get(3)?,
                kind: row.get(4)?,
                ordinal: row.get::<_, i64>(5)?.max(0) as usize,
            },
            node_id: row.get(6)?,
            line_start: row.get::<_, i64>(7)?.max(1) as usize,
            line_next: row.get::<_, Option<i64>>(8)?.map(|n| n.max(1) as usize),
            sha256: row.get(9)?,
        })
    })?;
    Ok(found.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The live code symbols with these `stable_id`s.
pub fn by_ids(db: &Connection, repository: &str, ids: &BTreeSet<String>) -> Result<Vec<Live>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    select(
        db,
        "repository_id=?1 AND stable_id IN (SELECT value FROM json_each(?2))",
        params![repository, json!(ids).to_string()],
    )
}

/// The live code symbols of one file (`relpath`), by the indexed file column.
pub fn in_file(db: &Connection, repository: &str, relpath: &str) -> Result<Vec<Live>> {
    select(
        db,
        "repository_id=?1 AND relpath=?2",
        params![repository, relpath],
    )
}

/// The head set (episode) `gf_symbol_heads` describes, if any.
pub fn table_episode(db: &Connection, repository: &str) -> Result<Option<String>> {
    Ok(rows(
        db,
        "SELECT episode FROM gf_symbol_state WHERE repository_id=?",
        [repository],
    )?
    .first()
    .map(|row| s(row, "episode").to_string()))
}

fn attr(attributes: &impl graphicle::record::Properties, key: &str) -> Option<String> {
    match attributes.get(key)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

fn relpath(attributes: &impl graphicle::record::Properties) -> Option<String> {
    attr(attributes, "relpath")
        .or_else(|| attr(attributes, "source_file"))
        .map(|p| p.replace('\\', "/"))
}

/// The code symbols of `graph` (restricted to the files in `owners`), with
/// each file's sha256 from `heads`. The slice ends where `identity` ends it:
/// at the next start line of any node of the file other than the file node.
fn rows_of(
    graph: &Graph,
    heads: &BTreeMap<String, String>,
    owners: Option<&BTreeSet<String>>,
) -> Vec<Live> {
    let identities = graphicle_memory::graph::identity::logical_identities(graph);
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
    let mut out = Vec::new();
    for (index, node) in graph.nodes().iter().enumerate() {
        let attributes = &node.attributes;
        if !crate::code_graph_arm::code_symbol(attributes) {
            continue;
        }
        let Some(path) = relpath(attributes) else {
            continue;
        };
        if owners.is_some_and(|owners| !owners.contains(&path)) {
            continue;
        }
        let (Some(sha), Some(line), Some(stable_id), Some(revision), Some(identity)) = (
            heads.get(&path),
            span::start_line(attributes),
            attr(attributes, "stable_id"),
            attr(attributes, "revision"),
            identities[index].clone(),
        ) else {
            continue;
        };
        if identity.relpath != path {
            continue;
        }
        out.push(Live {
            stable_id,
            revision,
            identity,
            node_id: match &node.id {
                Value::String(id) => id.clone(),
                other => other.to_string(),
            },
            line_start: line.max(1),
            line_next: starts
                .get(&path)
                .and_then(|list| span::next_start(list, line)),
            sha256: sha.clone(),
        });
    }
    // A collapsed id would break the key; keep the first in node order.
    let mut seen = HashSet::new();
    out.retain(|row| seen.insert(row.stable_id.clone()));
    out
}

/// After `persist_with_report` recorded `episode` for the current head set
/// `heads`: the table follows it. When the table described the latest
/// episode before this one (`parent`), only the files the projection
/// rewrote (`dirty`, the delta's owner groups) are replaced; otherwise the
/// whole table is rebuilt from `graph`.
pub(crate) fn record(
    db: &Connection,
    repository: &str,
    episode: &str,
    graph: &Graph,
    heads: &BTreeMap<String, String>,
    parent: Option<&str>,
    dirty: &BTreeSet<String>,
) -> Result<()> {
    let incremental = parent.is_some() && table_episode(db, repository)?.as_deref() == parent;
    let owners = incremental.then_some(dirty);
    replace(
        db,
        repository,
        episode,
        rows_of(graph, heads, owners),
        owners,
    )
}

/// Make the table describe `episode`, a recorded episode of the current head
/// set, when it describes another one: a sync that returned to a recorded
/// head set, a replayed projection, or a store written before symbol links.
/// Reads the
/// resident graph. Runs in a savepoint, so a failure leaves the table as it
/// was; callers on a sync path ignore the error (the table then lags, and a
/// changed linked file reads `projection_pending`).
pub fn ensure(db: &Connection, repository: &str, episode: &str) -> Result<bool> {
    if table_episode(db, repository)?.as_deref() == Some(episode) {
        return Ok(false);
    }
    let heads = crate::store::heads(db, repository)?;
    if graph_projection::episode_of(&heads) != episode
        || !graph_projection::is_recorded(db, repository, episode)?
    {
        return Ok(false);
    }
    savepoint(db, || {
        let handle = graph_projection::resident(db, repository, episode)?;
        let graph: &Graph = &handle;
        replace(db, repository, episode, rows_of(graph, &heads, None), None)
    })?;
    Ok(true)
}

/// [`ensure`] for the current head set, when it is recorded.
pub fn ensure_current(db: &Connection, repository: &str) -> Result<bool> {
    let episode = graph_projection::current_episode(db, repository)?;
    ensure(db, repository, &episode)
}

fn savepoint<T>(db: &Connection, work: impl FnOnce() -> Result<T>) -> Result<T> {
    db.execute_batch("SAVEPOINT graphicle_symbols")?;
    match work() {
        Ok(value) => {
            db.execute_batch("RELEASE graphicle_symbols")?;
            Ok(value)
        }
        Err(error) => {
            let _ = db.execute_batch("ROLLBACK TO graphicle_symbols; RELEASE graphicle_symbols");
            Err(error)
        }
    }
}

/// Replace the rows of `owners` (all rows when `None`) with `new`, record
/// the exact moves between the old and new rows, and mark the table as
/// describing `episode`.
fn replace(
    db: &Connection,
    repository: &str,
    episode: &str,
    new: Vec<Live>,
    owners: Option<&BTreeSet<String>>,
) -> Result<()> {
    let old = match owners {
        Some(owners) => {
            let owners = json!(owners).to_string();
            let old = select(
                db,
                "repository_id=?1 AND relpath IN (SELECT value FROM json_each(?2))",
                params![repository, owners],
            )?;
            db.execute(
                "DELETE FROM gf_symbol_heads WHERE repository_id=?1 AND relpath IN (SELECT value FROM json_each(?2))",
                params![repository, owners],
            )?;
            old
        }
        None => {
            let old = select(db, "repository_id=?1", params![repository])?;
            db.execute(
                "DELETE FROM gf_symbol_heads WHERE repository_id=?",
                [repository],
            )?;
            old
        }
    };
    {
        let mut insert = db.prepare_cached(
            "INSERT OR REPLACE INTO gf_symbol_heads(repository_id,stable_id,revision,relpath,qualified_name,kind,ordinal,node_id,line_start,line_next,sha256) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
        )?;
        for row in &new {
            insert.execute(params![
                repository,
                row.stable_id,
                row.revision,
                row.identity.relpath,
                row.identity.qualified_name,
                row.identity.kind,
                row.identity.ordinal as i64,
                row.node_id,
                row.line_start as i64,
                row.line_next.map(|n| n as i64),
                row.sha256
            ])?;
        }
    }
    record_moves(db, repository, episode, &old, &new)?;
    db.execute(
        "INSERT INTO gf_symbol_state(repository_id,episode,updated_at) VALUES(?,?,?) ON CONFLICT(repository_id) DO UPDATE SET episode=excluded.episode,updated_at=excluded.updated_at",
        params![repository, episode, now()],
    )?;
    pin_pending(db, repository)?;
    Ok(())
}

/// Exact moves: a symbol that left the table and one that entered it
/// with the same revision, kind and name tail, when exactly one of each
/// shares that key. A moved-from id that is live again loses its move.
fn record_moves(
    db: &Connection,
    repository: &str,
    episode: &str,
    old: &[Live],
    new: &[Live],
) -> Result<()> {
    let old_ids: HashSet<&str> = old.iter().map(|r| r.stable_id.as_str()).collect();
    let new_ids: HashSet<&str> = new.iter().map(|r| r.stable_id.as_str()).collect();
    type Key<'a> = (&'a str, &'a str, &'a str);
    let mut left: HashMap<Key<'_>, Vec<&Live>> = HashMap::new();
    for row in old
        .iter()
        .filter(|r| !new_ids.contains(r.stable_id.as_str()))
    {
        left.entry((&row.revision, &row.identity.kind, row.tail()))
            .or_default()
            .push(row);
    }
    let mut arrived: HashMap<Key<'_>, Vec<&Live>> = HashMap::new();
    for row in new
        .iter()
        .filter(|r| !old_ids.contains(r.stable_id.as_str()))
    {
        arrived
            .entry((&row.revision, &row.identity.kind, row.tail()))
            .or_default()
            .push(row);
    }
    let stamp = now();
    let mut insert = db.prepare_cached(
        "INSERT OR REPLACE INTO gf_symbol_moves(repository_id,from_stable_id,to_stable_id,revision,episode,created_at) VALUES(?,?,?,?,?,?)",
    )?;
    for (key, from) in &left {
        if let (1, Some(to)) = (from.len(), arrived.get(key).filter(|to| to.len() == 1)) {
            insert.execute(params![
                repository,
                from[0].stable_id,
                to[0].stable_id,
                from[0].revision,
                episode,
                stamp
            ])?;
        }
    }
    db.execute(
        "DELETE FROM gf_symbol_moves WHERE repository_id=?1 AND from_stable_id IN (SELECT stable_id FROM gf_symbol_heads WHERE repository_id=?1)",
        [repository],
    )?;
    Ok(())
}

/// Whether the bytes `start..end` of `content` lie wholly in the slices of
/// `symbols` (the file's live rows at `content`'s sha256), and the symbols
/// they touch.
pub fn cover<'a>(
    content: &str,
    symbols: &'a [Live],
    start: usize,
    end: usize,
) -> (bool, Vec<(&'a Live, std::ops::Range<usize>)>) {
    let starts = span::line_starts(content);
    let mut ranges: Vec<(std::ops::Range<usize>, &Live)> = symbols
        .iter()
        .map(|row| (row.range(&starts, content.len()), row))
        .filter(|(range, _)| range.start < range.end)
        .collect();
    ranges.sort_by_key(|(range, row)| (range.start, range.end, row.stable_id.clone()));
    let mut hits = Vec::new();
    let mut reached = start;
    let mut gap = false;
    for (range, row) in ranges {
        if range.end <= start || range.start >= end {
            continue;
        }
        if range.start > reached {
            gap = true;
        }
        reached = reached.max(range.end);
        hits.push((row, range));
    }
    (!gap && reached >= end && end > start, hits)
}

/// How one quote occurrence is judged, from [`cover`]: inside one code
/// symbol, by that symbol ([`RULE_SYMBOLS`]); across several
/// ([`RULE_SPANS`]) or partly outside them ([`RULE_OUTSIDE`]), by the file.
fn occurrence_rule(covered: bool, symbols: usize) -> &'static str {
    match (covered, symbols) {
        (true, 1) => RULE_SYMBOLS,
        (true, _) => RULE_SPANS,
        _ => RULE_OUTSIDE,
    }
}

/// Extensions the engine classifies as code that hold data, a project or
/// solution description, or a form or map: data JSON (config JSON is a
/// package manifest), MSBuild projects and solutions, Terraform variable
/// values, PowerShell data, addon tables of contents, Delphi and Lazarus
/// forms and packages, BYOND maps, icons and interfaces, XAML markup.
const DATA_EXTENSIONS: &[&str] = &[
    "json", "csproj", "fsproj", "vbproj", "sln", "slnx", "tfvars", "psd1", "toc", "dfm", "lfm",
    "lpk", "dmm", "dmi", "dmf", "xaml",
];

/// Whether a file could hold code symbols the projection has not read yet:
/// a code file by the engine's classification of its name, except data and
/// package manifests ([`DATA_EXTENSIONS`], `pyproject.toml`, `go.mod`, …),
/// whose quotes keep the file rule with nothing recorded until a projection
/// shows code symbols there. The name is classified under `/dev/null`, a
/// root nothing can exist below, so the engine never reads a file of the
/// working directory (it reads a document to tell a paper apart, and an
/// extensionless file for its shebang).
fn is_code_path(path: &str) -> bool {
    let path = std::path::Path::new(path);
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    !DATA_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        && !graphicle::manifest_ingest::is_package_manifest_path(path)
        && graphicle::detect::classify_file(&std::path::Path::new("/dev/null").join(path))
            == Some(graphicle::detect::FileType::Code)
}

/// Write-side view of the table for one memory producer.
struct Resolver<'a> {
    db: &'a Connection,
    repository: &'a str,
    episode: Option<String>,
    manifest: Option<Option<BTreeMap<String, String>>>,
}

impl<'a> Resolver<'a> {
    fn new(db: &'a Connection, repository: &'a str) -> Result<Self> {
        // An upgraded store, or a head set that returned to a recorded
        // episode: describe the current head set before resolving. Never
        // fatal; the quotes are then pending.
        let _ = ensure_current(db, repository);
        Ok(Self {
            db,
            repository,
            episode: table_episode(db, repository)?,
            manifest: None,
        })
    }

    fn episode(&self) -> &str {
        self.episode.as_deref().unwrap_or("")
    }

    /// The live rows of `path` when the table describes it at `sha`: its
    /// code symbols (none for a file without any). `None` while the table
    /// describes other content of the file, or nothing.
    fn describe(&mut self, path: &str, sha: &str) -> Result<Option<Vec<Live>>> {
        describe(
            self.db,
            self.repository,
            self.episode.as_deref(),
            &mut self.manifest,
            path,
            sha,
        )
    }
}

fn describe(
    db: &Connection,
    repository: &str,
    episode: Option<&str>,
    manifest: &mut Option<Option<BTreeMap<String, String>>>,
    path: &str,
    sha: &str,
) -> Result<Option<Vec<Live>>> {
    let rows = select(
        db,
        "repository_id=?1 AND relpath=?2",
        params![repository, path],
    )?;
    if let Some(first) = rows.first() {
        return Ok((first.sha256 == sha).then_some(rows));
    }
    let Some(episode) = episode else {
        return Ok(None);
    };
    let heads = manifest.get_or_insert_with(|| {
        graph_projection::stored_episode_heads(db, repository, episode)
            .ok()
            .flatten()
    });
    Ok(heads
        .as_ref()
        .and_then(|heads| heads.get(path))
        .filter(|recorded| recorded.as_str() == sha)
        .map(|_| Vec::new()))
}

fn link(row: &Live, rung: &str, episode: &str, range: std::ops::Range<usize>) -> Value {
    json!({"state":"resolved","stable_id":row.stable_id,"revision":row.revision,
        "identity":row.identity,"episode":episode,"byte_start":range.start,"byte_end":range.end,
        "rung":rung,"source":format!("file:{}",row.identity.relpath)})
}

/// The file a link or pending occurrence is in.
fn entry_path(entry: &Value) -> &str {
    entry["identity"]["relpath"]
        .as_str()
        .or_else(|| entry["relpath"].as_str())
        .unwrap_or("")
}

fn entries(data: &Value) -> impl Iterator<Item = &Value> {
    data["symbols"].as_array().into_iter().flatten()
}

fn push(data: &mut Value, entry: Value) {
    if !data["symbols"].is_array() {
        data["symbols"] = json!([]);
    }
    let list = data["symbols"].as_array_mut().unwrap();
    let same = |other: &Value| match s(&entry, "state") {
        "pending" => {
            other["state"] == "pending"
                && other["relpath"] == entry["relpath"]
                && other["sha256"] == entry["sha256"]
                && other["byte_start"] == entry["byte_start"]
                && other["byte_end"] == entry["byte_end"]
        }
        // One link per symbol and rung: a retained answer keeps both the
        // symbol its quote resolved to and the window it quoted.
        _ => {
            other["state"] != "pending"
                && other["stable_id"] == entry["stable_id"]
                && other["rung"] == entry["rung"]
        }
    };
    if !list.iter().any(same) {
        list.push(entry);
    }
}

/// Precedence of `symbol_files` values: a reason for the file rule wins
/// over `pending`, which wins over `symbols`.
fn weight(rule: &str) -> u8 {
    match rule {
        RULE_SYMBOLS => 0,
        RULE_PENDING => 1,
        _ => 2,
    }
}

fn set_rule(data: &mut Value, path: &str, rule: &str) {
    let current = data["symbol_files"][path].as_str().map(weight);
    if current.is_none_or(|current| weight(rule) > current) {
        data["symbol_files"][path] = json!(rule);
    }
}

/// Keep `symbol_files` only for files that hold links or pending quotes,
/// and drop both fields when nothing is linked: a memory without code links
/// stores what it stored before symbol links. Past [`MAX_LINKS`] nothing is
/// linked and every file keeps the file rule.
pub fn tidy(data: &mut Value) {
    if entries(data).count() > MAX_LINKS
        && let Some(object) = data.as_object_mut()
    {
        object.remove("symbols");
        object.remove("symbol_files");
        return;
    }
    let linked: BTreeSet<String> = entries(data).map(|e| entry_path(e).to_string()).collect();
    if let Some(files) = data["symbol_files"].as_object_mut() {
        files.retain(|path, _| linked.contains(path));
    }
    let Some(object) = data.as_object_mut() else {
        return;
    };
    if linked.is_empty() {
        object.remove("symbols");
        object.remove("symbol_files");
    } else if object
        .get("symbol_files")
        .is_some_and(|files| files.as_object().is_none_or(Map::is_empty))
    {
        object.remove("symbol_files");
    }
}

/// Resolve every quote of `citations` that is in a repository file to
/// the code symbols containing each of its occurrences, and say per file how
/// staleness is judged. `memory::dependencies`, the one place every memory
/// producer passes through, calls this after checking the quotes.
pub fn anchor(
    db: &Connection,
    repository: &str,
    citations: &[Value],
    documents: &[Value],
    data: &mut Value,
) -> Result<()> {
    let mut by_file: BTreeMap<&str, (&Value, Vec<&str>)> = BTreeMap::new();
    for citation in citations {
        let Some(doc) = documents
            .iter()
            .find(|d| d["id"] == citation["id"] && d["source_kind"] == "repository_file")
        else {
            continue;
        };
        let quote = s(citation, "quote");
        if !quote.is_empty() {
            by_file
                .entry(s(doc, "path"))
                .or_insert_with(|| (doc, Vec::new()))
                .1
                .push(quote);
        }
    }
    if by_file.is_empty() {
        return Ok(());
    }
    let mut resolver = Resolver::new(db, repository)?;
    for (path, (doc, quotes)) in by_file {
        let content = s(doc, "content");
        let sha = s(doc, "sha256");
        let described = resolver.describe(path, sha)?;
        if described.as_ref().is_some_and(Vec::is_empty)
            || (described.is_none() && !is_code_path(path))
        {
            // No code symbols there: the file rule, recorded as before.
            continue;
        }
        for quote in quotes {
            let occurrences: Vec<usize> = content
                .match_indices(quote)
                .map(|(at, _)| at)
                .take(MAX_OCCURRENCES + 1)
                .collect();
            if occurrences.len() > MAX_OCCURRENCES {
                set_rule(data, path, RULE_REPEATED);
                continue;
            }
            for start in occurrences {
                let end = start + quote.len();
                match &described {
                    Some(rows) => {
                        let (covered, hits) = cover(content, rows, start, end);
                        let rule = occurrence_rule(covered, hits.len());
                        for (row, range) in hits {
                            push(data, link(row, RUNG_QUOTE, resolver.episode(), range));
                        }
                        set_rule(data, path, rule);
                    }
                    None => {
                        push(
                            data,
                            json!({"state":"pending","relpath":path,"sha256":sha,"byte_start":start,
                                "byte_end":end,"rung":RUNG_QUOTE,"source":format!("file:{path}")}),
                        );
                        set_rule(data, path, RULE_PENDING);
                    }
                }
            }
        }
    }
    tidy(data);
    Ok(())
}

/// The `symbols` argument of `propose_memory` and `note`.
/// Each item is a `stable_id`, or a `path` and `qualified_name` (with `kind`
/// and `ordinal` when several symbols share them), resolved against the
/// live-symbol table for the file's current content. The file becomes a
/// dependency judged by its symbols. An unknown or ambiguous symbol, or one
/// whose file's code graph is not built for its current content, is refused.
pub fn name(
    db: &Connection,
    repository: &str,
    items: &[Value],
    documents: &[Value],
    data: &mut Value,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    if items.len() > MAX_NAMED {
        return Err(Error::bad(format!("Name at most {MAX_NAMED} symbols")));
    }
    let resolver = Resolver::new(db, repository)?;
    for item in items {
        if !item.is_object() {
            return Err(Error::bad(
                "Each symbol is an object with stable_id, or path and qualified_name",
            ));
        }
        let candidates = if item.get("stable_id").is_some_and(|v| !v.is_null()) {
            let id = string(item, "stable_id", 128)?;
            select(
                db,
                "repository_id=?1 AND stable_id=?2",
                params![repository, id],
            )?
        } else {
            let path = string(item, "path", 1500)?;
            let qualified = string(item, "qualified_name", 4000)?;
            let mut rows = select(
                db,
                "repository_id=?1 AND relpath=?2 AND qualified_name=?3",
                params![repository, path, qualified],
            )?;
            if let Some(kind) = item["kind"].as_str() {
                rows.retain(|r| r.identity.kind == kind);
            }
            if let Some(ordinal) = item["ordinal"].as_u64() {
                rows.retain(|r| r.identity.ordinal as u64 == ordinal);
            }
            rows
        };
        let label = item["stable_id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} in {}", s(item, "qualified_name"), s(item, "path")));
        let row = match candidates.as_slice() {
            [row] => row,
            [] => {
                return Err(Error::bad(format!(
                    "No code symbol {label} in the project's code graph"
                )));
            }
            many => {
                return Err(Error::bad(format!(
                    "{} code symbols match {label}; add its kind or ordinal",
                    many.len()
                )));
            }
        };
        let path = &row.identity.relpath;
        let doc = documents
            .iter()
            .find(|d| s(d, "id") == format!("file:{path}"))
            .ok_or_else(|| Error::bad(format!("{path} is not in the current sources")))?;
        if s(doc, "sha256") != row.sha256 {
            return Err(Error::conflict(format!(
                "The code graph of {path} is not built for its current content yet; sync the folder (or wait for its projection) and try again"
            )));
        }
        let content = s(doc, "content");
        let range = row.range(&span::line_starts(content), content.len());
        data["dependencies"][path.as_str()] = doc["sha256"].clone();
        push(data, link(row, RUNG_NAMED, resolver.episode(), range));
        set_rule(data, path, RULE_SYMBOLS);
    }
    tidy(data);
    Ok(())
}

/// Files a memory names as a whole keep the file rule, whatever their
/// quotes resolve to.
pub fn name_files(data: &mut Value, paths: &[String]) {
    if data.get("symbols").is_none() {
        return;
    }
    for path in paths {
        set_rule(data, path, RULE_NAMED_FILE);
    }
    tidy(data);
}

/// A retained answer keeps the code-graph facts of the windows its
/// quotes were read from, with no new resolution (rung `answer_window`).
/// `windows` are the cited windows of the retrievals the answer read:
/// `{id, byte_start, byte_end, code_graph}`. Only a window of a code symbol
/// the live-symbol table holds at the window's revision is kept, with the
/// table's logical identity: staleness reads that table, so a file node, a
/// rationale or another node it never holds would read `symbol_missing`
/// after any edit of its file.
pub fn carry_windows(
    db: &Connection,
    repository: &str,
    citations: &[Value],
    windows: &[Value],
    documents: &[Value],
    data: &mut Value,
) -> Result<()> {
    let mut found = Vec::new();
    for citation in citations {
        let id = s(citation, "id");
        let quote = s(citation, "quote");
        let Some(doc) = documents.iter().find(|d| s(d, "id") == id) else {
            continue;
        };
        if quote.is_empty() {
            continue;
        }
        let occurrences: Vec<usize> = s(doc, "content")
            .match_indices(quote)
            .map(|(at, _)| at)
            .take(MAX_OCCURRENCES)
            .collect();
        for window in windows.iter().filter(|w| s(w, "id") == id) {
            let facts = &window[crate::code_graph_arm::ARM];
            let (Some(a), Some(b)) = (window["byte_start"].as_u64(), window["byte_end"].as_u64())
            else {
                continue;
            };
            if facts["stable_id"].as_str().is_none_or(str::is_empty)
                || facts["revision"].as_str().is_none_or(str::is_empty)
            {
                continue;
            }
            if occurrences
                .iter()
                .any(|&at| at as u64 >= a && (at + quote.len()) as u64 <= b)
            {
                found.push(facts.clone());
            }
        }
    }
    if found.is_empty() {
        return Ok(());
    }
    let ids: BTreeSet<String> = found
        .iter()
        .map(|f| s(f, "stable_id").to_string())
        .collect();
    let live: HashMap<String, Live> = by_ids(db, repository, &ids)?
        .into_iter()
        .map(|row| (row.stable_id.clone(), row))
        .collect();
    for facts in found {
        let Some(row) = live
            .get(s(&facts, "stable_id"))
            .filter(|row| row.revision == s(&facts, "revision"))
        else {
            continue;
        };
        push(
            data,
            json!({"state":"resolved","stable_id":row.stable_id,"revision":row.revision,
                "identity":row.identity,"episode":facts["episode"],"byte_start":facts["byte_start"],
                "byte_end":facts["byte_end"],"rung":RUNG_ANSWER,"source":format!("file:{}",row.identity.relpath)}),
        );
    }
    tidy(data);
    Ok(())
}

/// `propose_refresh`: the links and rules of the files a refresh draft keeps
/// from the memory it refreshes (`paths`, the dependencies its own quotes do
/// not cite).
pub fn carry(data: &mut Value, old: &Value, paths: &BTreeSet<String>) {
    for entry in entries(old) {
        if paths.contains(entry_path(entry)) {
            push(data, entry.clone());
        }
    }
    if let Some(files) = old["symbol_files"].as_object() {
        for (path, rule) in files {
            if paths.contains(path)
                && let Some(rule) = rule.as_str()
            {
                set_rule(data, path, rule);
            }
        }
    }
    tidy(data);
}

/// After `memory::save` wrote a revision: queue its pending quote
/// occurrences for pinning, and pin what can be pinned now.
pub fn record_pins(db: &Connection, repository: &str, memory: &str, data: &Value) -> Result<()> {
    let pending: Vec<&Value> = entries(data).filter(|e| e["state"] == "pending").collect();
    if pending.is_empty() {
        return Ok(());
    }
    let stamp = now();
    for entry in pending {
        db.execute(
            "INSERT OR IGNORE INTO gf_symbol_pins(repository_id,memory_id,relpath,sha256,byte_start,byte_end,state,created_at) VALUES(?,?,?,?,?,?,'pending',?)",
            params![
                repository,
                memory,
                s(entry, "relpath"),
                s(entry, "sha256"),
                entry["byte_start"].as_i64(),
                entry["byte_end"].as_i64(),
                stamp
            ],
        )?;
    }
    pin_pending(db, repository)?;
    Ok(())
}

/// Pin the pending quote occurrences of `repository` that can be pinned:
/// from another memory's pin of the same bytes, or against the live-symbol
/// table once it describes the file at the quoted content. Runs after every
/// change of the table (in the projection's own transaction) and after a
/// memory with pending quotes is saved.
pub fn pin_pending(db: &Connection, repository: &str) -> Result<usize> {
    let pending = rows(
        db,
        "SELECT memory_id,relpath,sha256,byte_start,byte_end FROM gf_symbol_pins WHERE repository_id=? AND state='pending'",
        [repository],
    )?;
    if pending.is_empty() {
        return Ok(0);
    }
    let episode = table_episode(db, repository)?;
    let mut manifest = None;
    // (path, sha256) → the content there and its live rows, when described.
    type Described = Option<(String, Vec<Live>)>;
    let mut files: HashMap<(String, String), Described> = HashMap::new();
    let stamp = now();
    let mut pinned = 0;
    let update = "UPDATE gf_symbol_pins SET state='pinned',covered=?7,symbols=?8,episode=?9,pinned_at=?10 WHERE repository_id=?1 AND memory_id=?2 AND relpath=?3 AND sha256=?4 AND byte_start=?5 AND byte_end=?6 AND state='pending'";
    for row in &pending {
        let (memory, path, sha) = (s(row, "memory_id"), s(row, "relpath"), s(row, "sha256"));
        let (start, end) = (
            row["byte_start"].as_u64().unwrap_or(0) as usize,
            row["byte_end"].as_u64().unwrap_or(0) as usize,
        );
        // The same bytes pinned for another memory (a refresh draft, a
        // later revision applied to another id) need no new resolution.
        if let Some(other) = rows(
            db,
            "SELECT covered,symbols,episode FROM gf_symbol_pins WHERE repository_id=? AND relpath=? AND sha256=? AND byte_start=? AND byte_end=? AND state='pinned' LIMIT 1",
            params![repository, path, sha, start as i64, end as i64],
        )?
        .pop()
        {
            pinned += db.execute(
                update,
                params![
                    repository,
                    memory,
                    path,
                    sha,
                    start as i64,
                    end as i64,
                    other["covered"].as_i64(),
                    other["symbols"].as_str(),
                    other["episode"].as_str(),
                    stamp
                ],
            )?;
            continue;
        }
        let file = match files.entry((path.to_string(), sha.to_string())) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let described =
                    describe(db, repository, episode.as_deref(), &mut manifest, path, sha)?;
                let content = match described {
                    Some(_) => rows(
                        db,
                        "SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?",
                        params![repository, sha],
                    )?
                    .first()
                    .map(|r| s(r, "content").to_string()),
                    None => None,
                };
                entry.insert(
                    described
                        .zip(content)
                        .map(|(rows, content)| (content, rows)),
                )
            }
        };
        let Some((content, live)) = file else {
            continue;
        };
        if content.get(start..end).is_none() {
            continue;
        }
        let (covered, hits) = cover(content, live, start, end);
        // Judged by its symbol only inside one symbol, as `anchor` judges.
        let covered = occurrence_rule(covered, hits.len()) == RULE_SYMBOLS;
        let symbols: Vec<Value> = hits
            .iter()
            .map(|(row, range)| {
                link(
                    row,
                    RUNG_QUOTE,
                    episode.as_deref().unwrap_or(""),
                    range.clone(),
                )
            })
            .collect();
        pinned += db.execute(
            update,
            params![
                repository,
                memory,
                path,
                sha,
                start as i64,
                end as i64,
                i64::from(covered),
                json!(symbols).to_string(),
                episode,
                stamp
            ],
        )?;
    }
    Ok(pinned)
}

/// `memory.import` re-grounds a memory's links in the
/// target project through their logical identity: the `stable_id` is
/// recomputed in the target's identity scope and must name a live symbol
/// there with the same revision. Re-grounded links take the target's ids,
/// episode and byte ranges, and a file whose links all re-grounded, and
/// whose quotes (the memory's own `evidence`) each lie in one of them, is
/// judged by its symbols at the target's content. A file of pending quotes
/// keeps them only when every occurrence of its quotes there has one. A
/// link that does not re-ground is kept, marked `unresolved` with a reason,
/// and its file keeps the file rule; so does a file whose quotes the links
/// or pending entries do not cover, since imported links are data, not a
/// resolution made here. More than [`MAX_LINKS`] links are dropped unread,
/// as [`tidy`] drops them. Returns `{regrounded, unresolved}`.
pub fn reground(
    db: &Connection,
    repository: &str,
    data: &mut Value,
    documents: &[Value],
) -> Result<Value> {
    let count = data["symbols"].as_array().map_or(0, Vec::len);
    if count > MAX_LINKS
        && let Some(object) = data.as_object_mut()
    {
        object.remove("symbols");
        object.remove("symbol_files");
        return Ok(json!({"regrounded":0,"unresolved":count}));
    }
    let Some(list) = data["symbols"]
        .as_array()
        .cloned()
        .filter(|l| !l.is_empty())
    else {
        tidy(data);
        return Ok(json!({"regrounded":0,"unresolved":0}));
    };
    let resolver = Resolver::new(db, repository)?;
    let current = resolver.episode.as_deref()
        == Some(graph_projection::current_episode(db, repository)?.as_str());
    let scope = graph_projection::identity_scope(db, repository)?;
    let docs: HashMap<&str, &Value> = documents.iter().map(|d| (s(d, "id"), d)).collect();
    // The memory's own quotes, per file: what its links must cover.
    let mut quotes: HashMap<String, Vec<String>> = HashMap::new();
    for citation in data["evidence"].as_array().into_iter().flatten() {
        if let Some(path) = s(citation, "id").strip_prefix("file:") {
            quotes
                .entry(path.to_string())
                .or_default()
                .push(s(citation, "quote").to_string());
        }
    }
    let mut out = Vec::with_capacity(list.len());
    let mut regrounded = 0;
    let mut unresolved: BTreeSet<String> = BTreeSet::new();
    let mut rows_by_file: HashMap<String, Vec<Live>> = HashMap::new();
    let mut failed = 0;
    for entry in list {
        let path = entry_path(&entry).to_string();
        let doc = docs.get(format!("file:{path}").as_str());
        let mut fail = |mut entry: Value, reason: &str| {
            entry["state"] = json!("unresolved");
            entry["reason"] = json!(reason);
            unresolved.insert(path.clone());
            failed += 1;
            entry
        };
        match s(&entry, "state") {
            "pending" => {
                // The same content here, and the bytes it names are one of
                // the memory's quotes.
                let quoted = doc.is_some_and(|d| {
                    d["sha256"] == entry["sha256"]
                        && s(d, "content")
                            .get(
                                entry["byte_start"].as_u64().unwrap_or(0) as usize
                                    ..entry["byte_end"].as_u64().unwrap_or(0) as usize,
                            )
                            .is_some_and(|text| {
                                quotes
                                    .get(&path)
                                    .is_some_and(|q| q.iter().any(|q| q == text))
                            })
                });
                if quoted {
                    out.push(entry);
                } else {
                    out.push(fail(entry, "source_differs"));
                }
            }
            "resolved" => {
                let Ok(identity) =
                    serde_json::from_value::<LogicalIdentity>(entry["identity"].clone())
                else {
                    out.push(fail(entry, "identity_incomplete"));
                    continue;
                };
                let Some(doc) = doc else {
                    out.push(fail(entry, "file_missing"));
                    continue;
                };
                let id = identity.stable_id(&scope);
                let row = select(
                    db,
                    "repository_id=?1 AND stable_id=?2",
                    params![repository, id],
                )?
                .pop();
                match row {
                    Some(row) if row.sha256 != s(doc, "sha256") => {
                        out.push(fail(entry, "projection_pending"))
                    }
                    Some(row) if row.revision == s(&entry, "revision") => {
                        let content = s(doc, "content");
                        let range = row.range(&span::line_starts(content), content.len());
                        out.push(link(&row, s(&entry, "rung"), resolver.episode(), range));
                        rows_by_file.entry(path.clone()).or_default().push(row);
                        regrounded += 1;
                    }
                    Some(_) => out.push(fail(entry, "revision_differs")),
                    None if !current => out.push(fail(entry, "projection_pending")),
                    None => out.push(fail(entry, "symbol_missing")),
                }
            }
            _ => {
                unresolved.insert(path.clone());
                out.push(entry);
            }
        }
    }
    // The pending occurrences kept (same content here, bytes of one of the
    // memory's quotes), per file.
    let mut kept: HashMap<String, BTreeSet<(usize, usize)>> = HashMap::new();
    for entry in out.iter().filter(|e| e["state"] == "pending") {
        if let (Some(a), Some(b)) = (entry["byte_start"].as_u64(), entry["byte_end"].as_u64()) {
            kept.entry(entry_path(entry).to_string())
                .or_default()
                .insert((a as usize, b as usize));
        }
    }
    let linked: BTreeSet<String> = out.iter().map(|e| entry_path(e).to_string()).collect();
    data["symbols"] = json!(out);
    // Only files that hold links keep a rule (`tidy`), so only theirs are read.
    let rules: Vec<(String, String)> = data["symbol_files"]
        .as_object()
        .map(|files| {
            files
                .iter()
                .filter(|(path, _)| linked.contains(path.as_str()))
                .map(|(path, rule)| (path.clone(), rule.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    for (path, rule) in rules {
        let doc = docs.get(format!("file:{path}").as_str());
        // Every occurrence of the memory's quotes in the file here, when each
        // quote occurs, and at most `MAX_OCCURRENCES` times, as `anchor` links.
        let occurrences = || -> Option<Vec<(usize, usize)>> {
            let content = s(doc?, "content");
            let mut all = Vec::new();
            for quote in quotes.get(&path).into_iter().flatten() {
                let found: Vec<usize> = content
                    .match_indices(quote.as_str())
                    .map(|(at, _)| at)
                    .take(MAX_OCCURRENCES + 1)
                    .collect();
                if found.is_empty() || found.len() > MAX_OCCURRENCES {
                    return None;
                }
                all.extend(found.into_iter().map(|at| (at, at + quote.len())));
            }
            Some(all)
        };
        let judged = match rule.as_str() {
            // Each occurrence inside one re-grounded symbol.
            RULE_SYMBOLS => rows_by_file.get(&path).is_some_and(|rows| {
                let content = doc.map_or("", |d| s(d, "content"));
                occurrences().is_some_and(|all| {
                    all.iter().all(|&(a, b)| {
                        let (covered, hits) = cover(content, rows, a, b);
                        occurrence_rule(covered, hits.len()) == RULE_SYMBOLS
                    })
                })
            }),
            // Each occurrence a kept pending entry, to be pinned here.
            RULE_PENDING => occurrences().is_some_and(|all| {
                all.iter()
                    .all(|range| kept.get(&path).is_some_and(|k| k.contains(range)))
            }),
            _ => true,
        };
        if unresolved.contains(&path) || !judged {
            set_rule(data, &path, RULE_UNRESOLVED);
        } else if rule == RULE_SYMBOLS
            && let Some(doc) = doc
        {
            // Every link of the file names identical bytes here, and each of
            // the memory's quotes there lies in one of them.
            data["dependencies"][path.as_str()] = doc["sha256"].clone();
        }
    }
    tidy(data);
    Ok(json!({"regrounded":regrounded,"unresolved":failed}))
}

/// A knowledge pack's anchor names no symbol of this project.
pub const LAND_MISSING: &str = "symbol_missing";
/// Several symbols here could be the anchor's.
pub const LAND_AMBIGUOUS: &str = "ambiguous";
/// The anchor's symbol is not built for its file's current content yet.
pub const LAND_PENDING: &str = "projection_pending";
/// The anchor carries no complete logical identity.
pub const LAND_INCOMPLETE: &str = "identity_incomplete";

/// A knowledge pack's symbol anchor, landed on a live symbol here.
#[derive(Clone, Debug)]
pub struct Landed {
    /// The file the anchor named in the pack.
    pub from: String,
    pub row: Live,
    /// Not at the anchor's relpath and ordinal: found by qualified name and
    /// kind elsewhere.
    pub moved: bool,
}

/// Land a knowledge pack's symbol anchors (logical identities: relpath,
/// qualified name, kind, ordinal) on this project's live symbols: the one
/// live symbol of the same identity, else, as a move, the only live symbol
/// of that qualified name and kind anywhere in the project. The symbol's
/// file must be a current source whose code graph is built for its content.
/// Revisions are not compared: the code is another project's, and the
/// import is a draft a member reviews against this code. Per anchor, the
/// landed symbol or the reason it was dropped.
pub fn land(
    db: &Connection,
    repository: &str,
    anchors: &[Value],
    documents: &[Value],
) -> Result<Vec<std::result::Result<Landed, &'static str>>> {
    Lander::new(db, repository, documents).land(anchors)
}

/// [`land`] for the anchors of many memories (one knowledge pack): the
/// project's symbols are brought up to date once, the map of its live
/// symbols by qualified name and kind is read at most once, and its
/// documents are looked up by id.
pub struct Lander<'a> {
    db: &'a Connection,
    repository: &'a str,
    documents: HashMap<&'a str, &'a Value>,
    by_name: Option<HashMap<(String, String), Vec<Live>>>,
}

impl<'a> Lander<'a> {
    pub fn new(db: &'a Connection, repository: &'a str, documents: &'a [Value]) -> Self {
        let _ = ensure_current(db, repository);
        Self {
            db,
            repository,
            documents: documents.iter().map(|d| (s(d, "id"), d)).collect(),
            by_name: None,
        }
    }
    /// Per anchor, the landed symbol or the reason it was dropped ([`land`]).
    pub fn land(
        &mut self,
        anchors: &[Value],
    ) -> Result<Vec<std::result::Result<Landed, &'static str>>> {
        let (db, repository) = (self.db, self.repository);
        let mut out = Vec::with_capacity(anchors.len());
        for anchor in anchors {
            let Ok(identity) = serde_json::from_value::<LogicalIdentity>(anchor.clone()) else {
                out.push(Err(LAND_INCOMPLETE));
                continue;
            };
            let mut rows = select(
                db,
                "repository_id=?1 AND relpath=?2 AND qualified_name=?3",
                params![repository, identity.relpath, identity.qualified_name],
            )?;
            rows.retain(|r| {
                r.identity.kind == identity.kind && r.identity.ordinal == identity.ordinal
            });
            let (candidates, moved) = if rows.len() == 1 {
                (rows, false)
            } else {
                // Every live symbol by qualified name and kind, read once
                // and only when an anchor is not where the pack had it.
                if self.by_name.is_none() {
                    let mut all: HashMap<(String, String), Vec<Live>> = HashMap::new();
                    for row in select(db, "repository_id=?1", params![repository])? {
                        all.entry((
                            row.identity.qualified_name.clone(),
                            row.identity.kind.clone(),
                        ))
                        .or_default()
                        .push(row);
                    }
                    self.by_name = Some(all);
                }
                let key = (identity.qualified_name.clone(), identity.kind.clone());
                let found = self.by_name.as_ref().and_then(|all| all.get(&key));
                (found.cloned().unwrap_or_default(), true)
            };
            let row = match candidates.as_slice() {
                [row] => row.clone(),
                [] => {
                    out.push(Err(LAND_MISSING));
                    continue;
                }
                _ => {
                    out.push(Err(LAND_AMBIGUOUS));
                    continue;
                }
            };
            let file = format!("file:{}", row.identity.relpath);
            match self.documents.get(file.as_str()) {
                None => out.push(Err(LAND_MISSING)),
                Some(doc) if s(doc, "sha256") != row.sha256 => out.push(Err(LAND_PENDING)),
                Some(_) => out.push(Ok(Landed {
                    from: identity.relpath,
                    row,
                    moved,
                })),
            }
        }
        Ok(out)
    }
}

/// Link landed symbols as `named` links at this project's ids, revisions
/// and byte ranges; a symbol a quote already linked keeps that link. Their
/// files become dependencies judged by their symbols.
pub fn link_landed(
    db: &Connection,
    repository: &str,
    landed: &[&Landed],
    documents: &[Value],
    data: &mut Value,
) -> Result<()> {
    if landed.is_empty() {
        return Ok(());
    }
    let resolver = Resolver::new(db, repository)?;
    for landed in landed {
        let row = &landed.row;
        if entries(data).any(|e| e["stable_id"] == row.stable_id.as_str()) {
            continue;
        }
        let path = &row.identity.relpath;
        let Some(doc) = documents
            .iter()
            .find(|d| s(d, "id") == format!("file:{path}"))
        else {
            continue;
        };
        let content = s(doc, "content");
        let range = row.range(&span::line_starts(content), content.len());
        data["dependencies"][path.as_str()] = doc["sha256"].clone();
        push(data, link(row, RUNG_NAMED, resolver.episode(), range));
        set_rule(data, path, RULE_SYMBOLS);
    }
    tidy(data);
    Ok(())
}

/// A memory whose state, freshness and links make it a trigger for
/// `basic` (the second trigger): reviewed, current, with symbol links.
pub fn current_links(memories: &[Value]) -> bool {
    memories.iter().any(|m| {
        m["state"] == "reviewed"
            && m["stale"] != true
            && m["symbol_links"]["links"].as_u64().is_some_and(|n| n > 0)
    })
}

/// How a link reads against the live-symbol table.
#[derive(Debug)]
enum Status<'a> {
    Current { followed: bool, row: &'a Live },
    Changed,
    Missing,
    Pending,
}

/// A pin as read from `gf_symbol_pins`.
struct Pin {
    pinned: bool,
    covered: bool,
    symbols: Vec<(String, String)>,
}

/// One file's outcome under the symbol rule.
#[derive(Debug, Default)]
pub struct FileOutcome {
    /// Withholding reasons; empty when every linked symbol is current.
    pub reasons: Vec<String>,
    pub links: usize,
    pub followed: usize,
    pub pending: usize,
    /// `(document id, byte range)` of the current bytes of the file's linked
    /// symbols, where a moved symbol now is.
    pub locations: Vec<(String, std::ops::Range<usize>)>,
}

type PinKey = (String, String, String, u64, u64);

/// Read-side check of symbol links for one memory listing: what the
/// links of every changed, symbol-judged file need, prefetched in a few
/// indexed reads; nothing is read when no such file changed.
#[derive(Default)]
pub struct Check {
    heads: BTreeMap<String, String>,
    current: bool,
    rows: HashMap<String, Live>,
    moves: HashMap<String, String>,
    shifted: HashSet<String>,
    pins: HashMap<PinKey, Pin>,
    contents: HashMap<String, (String, Vec<usize>)>,
}

fn governed(data: &Value, path: &str) -> bool {
    matches!(
        data["symbol_files"][path].as_str(),
        Some(RULE_SYMBOLS | RULE_PENDING)
    )
}

fn pin_key(memory: &str, entry: &Value) -> PinKey {
    (
        memory.to_string(),
        s(entry, "relpath").to_string(),
        s(entry, "sha256").to_string(),
        entry["byte_start"].as_u64().unwrap_or(0),
        entry["byte_end"].as_u64().unwrap_or(0),
    )
}

impl Check {
    /// Prefetch for `memories` (`(id, data)`; the id may be absent for a
    /// record not saved under one). `live` maps source ids to their current
    /// sha256; `documents` give the current file contents.
    pub fn prepare(
        db: &Connection,
        repository: &str,
        memories: &[(Option<&str>, &Value)],
        live: &BTreeMap<&str, &str>,
        documents: &[Value],
    ) -> Result<Self> {
        let mut check = Self::default();
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        let mut pin_memories: BTreeSet<&str> = BTreeSet::new();
        // (stable_id, revision) of every link of a changed file.
        let mut changed_links: Vec<(String, String)> = Vec::new();
        for (id, data) in memories {
            if data.get("symbols").is_none() {
                continue;
            }
            for entry in entries(data) {
                if entry["state"] == "pending"
                    && let Some(id) = id
                {
                    pin_memories.insert(id);
                }
            }
            let Some(files) = data["symbol_files"].as_object() else {
                continue;
            };
            for path in files.keys().filter(|p| governed(data, p)) {
                let changed = live.get(format!("file:{path}").as_str()).copied()
                    != data["dependencies"][path.as_str()].as_str();
                if !changed {
                    continue;
                }
                for entry in entries(data).filter(|e| entry_path(e) == path) {
                    if entry["state"] == "resolved"
                        && let Some(stable) = entry["stable_id"].as_str()
                    {
                        wanted.insert(stable.to_string());
                        changed_links.push((stable.to_string(), s(entry, "revision").to_string()));
                    }
                }
            }
        }
        if wanted.is_empty() && pin_memories.is_empty() {
            return Ok(check);
        }
        if !pin_memories.is_empty() {
            for row in rows(
                db,
                "SELECT memory_id,relpath,sha256,byte_start,byte_end,state,covered,symbols FROM gf_symbol_pins WHERE repository_id=?1 AND memory_id IN (SELECT value FROM json_each(?2))",
                params![repository, json!(pin_memories).to_string()],
            )? {
                let symbols: Vec<Value> =
                    serde_json::from_str(row["symbols"].as_str().unwrap_or("[]"))?;
                let pin = Pin {
                    pinned: row["state"] == "pinned",
                    covered: row["covered"].as_i64() == Some(1),
                    symbols: symbols
                        .iter()
                        .map(|l| (s(l, "stable_id").to_string(), s(l, "revision").to_string()))
                        .collect(),
                };
                check.pins.insert(pin_key(s(&row, "memory_id"), &row), pin);
            }
        }
        // Pinned symbols of changed files are checked like resolved links.
        for (id, data) in memories {
            let Some(id) = id else { continue };
            for entry in entries(data).filter(|e| e["state"] == "pending") {
                let path = entry_path(entry);
                let changed = live.get(format!("file:{path}").as_str()).copied()
                    != data["dependencies"][path].as_str();
                if !changed || !governed(data, path) {
                    continue;
                }
                if let Some(pin) = check.pins.get(&pin_key(id, entry))
                    && pin.pinned
                    && pin.covered
                {
                    for (stable, revision) in &pin.symbols {
                        wanted.insert(stable.clone());
                        changed_links.push((stable.clone(), revision.clone()));
                    }
                }
            }
        }
        check.heads = crate::store::heads(db, repository)?;
        check.current = table_episode(db, repository)?.as_deref()
            == Some(graph_projection::episode_of(&check.heads).as_str());
        for row in by_ids(db, repository, &wanted)? {
            check.rows.insert(row.stable_id.clone(), row);
        }
        // Follow exact moves of ids that left the table.
        let mut missing: BTreeSet<String> = wanted
            .iter()
            .filter(|id| !check.rows.contains_key(id.as_str()))
            .cloned()
            .collect();
        for _ in 0..MAX_MOVES {
            if missing.is_empty() || !check.current {
                break;
            }
            let mut next = BTreeSet::new();
            for row in rows(
                db,
                "SELECT from_stable_id,to_stable_id FROM gf_symbol_moves WHERE repository_id=?1 AND from_stable_id IN (SELECT value FROM json_each(?2))",
                params![repository, json!(missing).to_string()],
            )? {
                let to = s(&row, "to_stable_id").to_string();
                check
                    .moves
                    .insert(s(&row, "from_stable_id").to_string(), to.clone());
                if !check.rows.contains_key(&to) && !check.moves.contains_key(&to) {
                    next.insert(to);
                }
            }
            for row in by_ids(db, repository, &next)? {
                next.remove(&row.stable_id);
                check.rows.insert(row.stable_id.clone(), row);
            }
            missing = next;
        }
        // A link whose id now names another symbol while its own bytes live
        // on under the same name (a same-name symbol inserted above shifted
        // the ordinals) reads as missing, not changed.
        let mut shifted = db.prepare_cached(
            "SELECT 1 FROM gf_symbol_heads WHERE repository_id=?1 AND relpath=?2 AND qualified_name=?3 AND kind=?4 AND revision=?5 AND stable_id<>?6 LIMIT 1",
        )?;
        for (stable, revision) in &changed_links {
            let Some(row) = check.rows.get(stable) else {
                continue;
            };
            if &row.revision != revision
                && shifted.exists(params![
                    repository,
                    row.identity.relpath,
                    row.identity.qualified_name,
                    row.identity.kind,
                    revision,
                    stable
                ])?
            {
                check.shifted.insert(stable.clone());
            }
        }
        let docs: HashMap<&str, &Value> = documents.iter().map(|d| (s(d, "id"), d)).collect();
        for row in check.rows.values() {
            let id = format!("file:{}", row.identity.relpath);
            if check.contents.contains_key(&id) {
                continue;
            }
            if let Some(doc) = docs.get(id.as_str()) {
                let content = s(doc, "content").to_string();
                let starts = span::line_starts(&content);
                check.contents.insert(id, (content, starts));
            }
        }
        Ok(check)
    }

    fn status(&self, stable: &str, revision: &str) -> Status<'_> {
        let mut cursor = stable;
        let mut followed = false;
        for _ in 0..=MAX_MOVES {
            if let Some(row) = self.rows.get(cursor) {
                if self.heads.get(&row.identity.relpath) != Some(&row.sha256) {
                    return Status::Pending;
                }
                if row.revision == revision {
                    return Status::Current { followed, row };
                }
                if !followed && self.shifted.contains(cursor) {
                    return Status::Missing;
                }
                return Status::Changed;
            }
            if !self.current {
                return Status::Pending;
            }
            match self.moves.get(cursor) {
                Some(next) => {
                    cursor = next;
                    followed = true;
                }
                None => return Status::Missing,
            }
        }
        Status::Missing
    }

    /// The outcome for one changed file of a memory under the symbol rule,
    /// or `None` when the file rule applies to it.
    pub fn file(&self, memory: Option<&str>, data: &Value, path: &str) -> Option<FileOutcome> {
        if !governed(data, path) {
            return None;
        }
        let mut links: Vec<(String, String)> = Vec::new();
        let mut outcome = FileOutcome::default();
        for entry in entries(data).filter(|e| entry_path(e) == path) {
            match s(entry, "state") {
                "resolved" => links.push((
                    s(entry, "stable_id").to_string(),
                    s(entry, "revision").to_string(),
                )),
                "pending" => {
                    let pin = memory.and_then(|id| self.pins.get(&pin_key(id, entry)));
                    match pin {
                        Some(pin) if pin.pinned && pin.covered => {
                            links.extend(pin.symbols.iter().cloned())
                        }
                        // Pinned bytes outside code symbols: the file rule.
                        Some(pin) if pin.pinned => return None,
                        // The table describes the current head set and still
                        // could not pin it: the quoted content was never
                        // projected, so the file rule.
                        _ if self.current => return None,
                        _ => outcome.pending += 1,
                    }
                }
                _ => return None,
            }
        }
        if links.is_empty() && outcome.pending == 0 {
            return None;
        }
        // A symbol linked under several rungs is checked once.
        let mut seen = HashSet::new();
        links.retain(|(stable, _)| seen.insert(stable.clone()));
        outcome.links = links.len();
        for (stable, revision) in &links {
            match self.status(stable, revision) {
                Status::Current { followed, row } => {
                    outcome.followed += usize::from(followed);
                    let id = format!("file:{}", row.identity.relpath);
                    if let Some((content, starts)) = self.contents.get(&id) {
                        outcome
                            .locations
                            .push((id, row.range(starts, content.len())));
                    }
                }
                Status::Changed => outcome.reasons.push(format!("symbol_changed:{stable}")),
                Status::Missing => outcome.reasons.push(format!("symbol_missing:{stable}")),
                Status::Pending => outcome.reasons.push(format!("projection_pending:{stable}")),
            }
        }
        if outcome.pending > 0 {
            outcome
                .reasons
                .push(format!("projection_pending:file:{path}"));
        }
        Some(outcome)
    }

    /// `symbol_links` of a memory row with links: how many links it holds,
    /// how many followed a move, how many wait for a projection, and, per
    /// changed file judged by its symbols and still current, where the
    /// linked bytes are now (`locations`, read by `memory_evidence`).
    pub fn summary(
        &self,
        memory: Option<&str>,
        data: &Value,
        live: &BTreeMap<&str, &str>,
    ) -> Option<Value> {
        data.get("symbols")?;
        let mut links = 0usize;
        let mut followed = 0usize;
        let mut pending = 0usize;
        let mut locations = Map::new();
        let paths: BTreeSet<&str> = entries(data).map(entry_path).collect();
        for path in paths {
            let changed = live.get(format!("file:{path}").as_str()).copied()
                != data["dependencies"][path].as_str();
            let outcome = changed.then(|| self.file(memory, data, path)).flatten();
            if let Some(outcome) = outcome {
                links += outcome.links;
                followed += outcome.followed;
                // Unpinned quotes, and links whose file the table has not
                // caught up with.
                pending += outcome.pending
                    + outcome
                        .reasons
                        .iter()
                        .filter(|r| {
                            r.starts_with("projection_pending:")
                                && !r.starts_with("projection_pending:file:")
                        })
                        .count();
                if outcome.reasons.is_empty() {
                    locations.insert(format!("file:{path}"), json!(merged(outcome.locations)));
                }
                continue;
            }
            let mut symbols: BTreeSet<&str> = BTreeSet::new();
            for entry in entries(data).filter(|e| entry_path(e) == path) {
                match s(entry, "state") {
                    "resolved" => {
                        symbols.insert(s(entry, "stable_id"));
                    }
                    "pending" => match memory.and_then(|id| self.pins.get(&pin_key(id, entry))) {
                        Some(pin) if pin.pinned => {
                            symbols.extend(pin.symbols.iter().map(|(id, _)| id.as_str()))
                        }
                        _ => pending += 1,
                    },
                    _ => {}
                }
            }
            links += symbols.len();
        }
        let mut out = json!({"links":links,"followed_move":followed,"pending":pending});
        if !locations.is_empty() {
            out["locations"] = Value::Object(locations);
        }
        Some(out)
    }
}

/// Locations per document, sorted, with touching ranges joined.
fn merged(mut locations: Vec<(String, std::ops::Range<usize>)>) -> Vec<Value> {
    locations.sort_by(|a, b| (&a.0, a.1.start, a.1.end).cmp(&(&b.0, b.1.start, b.1.end)));
    let mut out: Vec<(String, std::ops::Range<usize>)> = Vec::new();
    for (id, range) in locations {
        match out.last_mut() {
            Some((last, joined)) if *last == id && range.start <= joined.end => {
                joined.end = joined.end.max(range.end);
            }
            _ => out.push((id, range)),
        }
    }
    out.into_iter()
        .map(|(id, range)| json!({"id":id,"byte_start":range.start,"byte_end":range.end}))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, start: usize, next: Option<usize>) -> Live {
        Live {
            stable_id: id.into(),
            revision: format!("r-{id}"),
            identity: LogicalIdentity {
                relpath: "a.py".into(),
                qualified_name: id.into(),
                kind: "code".into(),
                ordinal: 0,
            },
            node_id: id.into(),
            line_start: start,
            line_next: next,
            sha256: "sha".into(),
        }
    }

    #[test]
    fn a_quote_is_covered_only_when_every_byte_lies_in_code_symbols() {
        // Line 1 is the module header; alpha runs lines 3-5, beta 6-7.
        let content = "import os\n\ndef alpha():\n    return 1\n\ndef beta():\n    return 2\n";
        let rows = [row("alpha", 3, Some(6)), row("beta", 6, None)];
        let at = |text: &str| content.find(text).unwrap();
        let ids = |hits: &[(&Live, std::ops::Range<usize>)]| {
            hits.iter()
                .map(|(r, _)| r.stable_id.clone())
                .collect::<Vec<_>>()
        };
        let quote = "return 1";
        let (covered, hits) = cover(content, &rows, at(quote), at(quote) + quote.len());
        assert!(covered);
        assert_eq!(ids(&hits), ["alpha"]);
        assert_eq!(occurrence_rule(covered, hits.len()), RULE_SYMBOLS);
        // A quote across two symbols records both, and keeps the file rule:
        // the two revisions do not say the symbols are still adjacent.
        let quote = "return 1\n\ndef beta";
        let (covered, hits) = cover(content, &rows, at(quote), at(quote) + quote.len());
        assert!(covered);
        assert_eq!(ids(&hits), ["alpha", "beta"]);
        assert_eq!(occurrence_rule(covered, hits.len()), RULE_SPANS);
        // The module header is outside every symbol.
        let quote = "import os";
        let (covered, hits) = cover(content, &rows, at(quote), at(quote) + quote.len());
        assert!(!covered && hits.is_empty());
        assert_eq!(occurrence_rule(covered, hits.len()), RULE_OUTSIDE);
        // Half in the header, half in alpha: not covered, alpha touched.
        let quote = "os\n\ndef alpha";
        let (covered, hits) = cover(content, &rows, at(quote), at(quote) + quote.len());
        assert!(!covered);
        assert_eq!(ids(&hits), ["alpha"]);
        assert_eq!(occurrence_rule(covered, hits.len()), RULE_OUTSIDE);
        // A gap between symbols (a node that is not a code symbol between
        // them) breaks coverage.
        let gapped = [row("alpha", 3, Some(5)), row("beta", 6, None)];
        let quote = "return 1\n\ndef beta";
        let (covered, _) = cover(content, &gapped, at(quote), at(quote) + quote.len());
        assert!(!covered);
    }

    #[test]
    fn touching_locations_join_per_document() {
        let at = |id: &str, a: usize, b: usize| (id.to_string(), a..b);
        assert_eq!(
            merged(vec![
                at("file:b.py", 0, 4),
                at("file:a.py", 10, 20),
                at("file:a.py", 0, 10),
                at("file:a.py", 30, 40)
            ]),
            vec![
                json!({"id":"file:a.py","byte_start":0,"byte_end":20}),
                json!({"id":"file:a.py","byte_start":30,"byte_end":40}),
                json!({"id":"file:b.py","byte_start":0,"byte_end":4}),
            ]
        );
    }

    #[test]
    fn rules_keep_the_file_rule_over_pending_over_symbols() {
        let mut data = json!({"symbols":[{"state":"resolved","stable_id":"x","revision":"r","identity":{"relpath":"a.py"}}]});
        set_rule(&mut data, "a.py", RULE_SYMBOLS);
        set_rule(&mut data, "a.py", RULE_PENDING);
        assert_eq!(data["symbol_files"]["a.py"], RULE_PENDING);
        set_rule(&mut data, "a.py", RULE_OUTSIDE);
        set_rule(&mut data, "a.py", RULE_SYMBOLS);
        assert_eq!(data["symbol_files"]["a.py"], RULE_OUTSIDE);
        // A file without links loses its rule; no links at all, no fields.
        set_rule(&mut data, "b.py", RULE_SYMBOLS);
        tidy(&mut data);
        assert!(data["symbol_files"].get("b.py").is_none());
        let mut empty = json!({"symbols":[],"symbol_files":{"a.py":"symbols"},"dependencies":{}});
        tidy(&mut empty);
        assert_eq!(empty, json!({"dependencies":{}}));
    }

    #[test]
    fn a_current_reviewed_memory_with_links_is_d9s_second_trigger() {
        let memory = |state: &str, stale: bool, links: u64| json!({"state":state,"stale":stale,"symbol_links":{"links":links}});
        assert!(current_links(&[memory("reviewed", false, 1)]));
        assert!(!current_links(&[memory("reviewed", true, 1)]));
        assert!(!current_links(&[memory("draft", false, 1)]));
        assert!(!current_links(&[memory("reviewed", false, 0)]));
        assert!(!current_links(&[json!({"state":"reviewed","stale":false})]));
    }

    #[test]
    fn code_paths_are_known_by_extension_without_reading_the_disk() {
        assert!(is_code_path("pkg/a.py"));
        assert!(is_code_path("src/lib.rs"));
        assert!(is_code_path("web/App.TSX"));
        assert!(!is_code_path("notes.md"));
        assert!(!is_code_path("session-1.txt"));
        assert!(!is_code_path("Makefile"));
        // Data and package manifests, which the engine reads as code: their
        // quotes keep the file rule while the projection is pending.
        for data in [
            "sessions/session-1.json",
            "package.json",
            "Data.JSON",
            "pyproject.toml",
            "crates/x/Cargo.toml",
            "go.mod",
            "pom.xml",
            "apm.yml",
            "App.csproj",
            "infra/prod.tfvars",
        ] {
            assert!(!is_code_path(data), "{data}");
        }
    }
}
