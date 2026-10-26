//! Saved code graphs: each recorded episode's graph and search index, built
//! once after its projection and read by every later process instead of
//! rebuilding them from the store's rows (GitHub issue #8: a question on a
//! large project spent most of its time there).
//!
//! The saved graph is `graphicle::snapshot`'s encoding of the graph exactly as
//! [`graph_projection::load`] reads it from the store, with its search index,
//! keyed by the episode and readable by the build that wrote it only. It
//! lives in the project's graph cache folder (`gf-cache/<project>/query/`),
//! which goes with the project. Beside it, the episode's file relationships
//! (what `query::episode_graph` lists as `edges`) are kept as pairs of files,
//! so `context` reads the relationships of the files it needs without the
//! whole list ([`FileEdges`]), and the episode's query file, which
//! `context`'s arms read in place instead of the whole graph
//! ([`crate::query_file`]).
//!
//! A missing, unreadable or stale file is never an error: the graph is then
//! read from the store as before, and the next projection or sync writes it
//! again ([`ensure`]). The saved graph and the file relationships end with a
//! trailer, their length and checksum ([`TRAILER`]): a sync checks the
//! length, and the read that finds a file of this build truncated or
//! damaged removes it, so that the next sync writes it again.
use crate::{Result, graph_projection};
use graphicle::graph::Graph;
use graphicle::serve_search::SearchIndex;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Changes when what this module writes changes (v2: the saved graph and the
/// file relationships end with a trailer, [`trailer`]).
const FORMAT: &str = "graphicle.query_graph.v2";

/// Bytes of the trailer a saved graph and a file relationships file end
/// with: the length of what comes before it and its checksum
/// (`crate::query_file::checksum`), 8 bytes each.
const TRAILER: usize = 16;

/// The trailer of `content` ([`TRAILER`]).
fn trailer(content: &[u8]) -> [u8; TRAILER] {
    let mut out = [0u8; TRAILER];
    out[..8].copy_from_slice(&(content.len() as u64).to_le_bytes());
    out[8..].copy_from_slice(&crate::query_file::checksum(content).to_le_bytes());
    out
}

/// Writes the trailer of a saved graph's or a file relationships file's
/// bytes again: for tests that give a file content a damaged byte would not
/// show (a hostile file).
#[doc(hidden)]
pub fn reseal(bytes: &mut Vec<u8>) {
    bytes.truncate(bytes.len().saturating_sub(TRAILER));
    let end = trailer(bytes);
    bytes.extend_from_slice(&end);
}

/// What `bytes` hold before their trailer, when the trailer matches them:
/// their length and their checksum.
fn unsealed(bytes: &[u8]) -> Option<&[u8]> {
    let at = bytes.len().checked_sub(TRAILER)?;
    let (content, end) = bytes.split_at(at);
    (end == trailer(content)).then_some(content)
}

/// Whether the file at `path` begins as `starts` says (its first `head`
/// bytes) and ends with a trailer that gives its length: a cheap check of a
/// saved file that reads neither its content nor its checksum.
fn whole_by_trailer(path: &Path, head: usize, starts: impl Fn(&[u8]) -> bool) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Some(len) = file
        .metadata()
        .ok()
        .map(|m| m.len())
        .filter(|&len| len >= TRAILER as u64)
    else {
        return false;
    };
    let mut first = vec![0u8; head.min(len as usize)];
    let mut end = [0u8; 8];
    file.read_exact(&mut first).is_ok()
        && starts(&first)
        && file.seek(SeekFrom::Start(len - TRAILER as u64)).is_ok()
        && file.read_exact(&mut end).is_ok()
        && u64::from_le_bytes(end) == len - TRAILER as u64
}

/// What tells a file apart from one that replaced it at the same path
/// ([`write_atomic`] renames a new file into place): its device and inode
/// where the platform has them, else its length and modification time.
pub(crate) fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64);
        (metadata.len(), modified)
    }
}

/// Removes the file at `path` that a read found truncated, malformed or
/// damaged, so that the next sync writes it again; not when another file has
/// replaced it since it was read (`read`, its [`identity`]).
pub(crate) fn remove_damaged(path: &Path, read: (u64, u64)) {
    if std::fs::metadata(path).is_ok_and(|now| identity(&now) == read) {
        let _ = std::fs::remove_file(path);
    }
}

/// The folder of a project's saved graphs: `gf-cache/<project>/query`.
fn folder(db: &Connection, repository: &str) -> Option<PathBuf> {
    Some(graph_projection::cache_root(db, repository)?.join("query"))
}

fn episode_name(episode: &str) -> Option<&str> {
    (!episode.is_empty()
        && episode
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(episode)
}

/// The file of `episode` with `extension` in the project's saved graphs
/// folder.
pub(crate) fn file_of(
    db: &Connection,
    repository: &str,
    episode: &str,
    extension: &str,
) -> Option<PathBuf> {
    Some(folder(db, repository)?.join(format!("{}.{extension}", episode_name(episode)?)))
}

fn graph_file(db: &Connection, repository: &str, episode: &str) -> Option<PathBuf> {
    file_of(db, repository, episode, "graph")
}

fn edges_file(db: &Connection, repository: &str, episode: &str) -> Option<PathBuf> {
    file_of(db, repository, episode, "files")
}

/// The key a saved graph and its file relationships carry: the format, the
/// builds of the memory library and of this program that read the graph
/// from the store (hashes of their sources and, for this program, of its
/// locked dependencies; the snapshot adds the engine's own), the project and
/// the episode. A file another build wrote is never read.
fn key(repository: &str, episode: &str) -> Vec<u8> {
    format!(
        "{FORMAT}\0{}\0{}\0{repository}\0{episode}",
        graphicle_memory::build_code(),
        env!("GRAPHICLE_RUNTIME_CODE")
    )
    .into_bytes()
}

/// Whether `head`, the first bytes of a saved graph, carries `key` (the
/// snapshot's own key check; it decodes nothing of the graph).
fn graph_key(head: &[u8], key: &[u8]) -> bool {
    graphicle::snapshot::from_bytes(&head[..head.len().min(4096)], key).is_some()
}

/// The saved graph and index of `episode`, when this build wrote one and it
/// is whole: a file with this build's key whose trailer or snapshot does not
/// hold (truncated or damaged) is removed, so that the next sync writes it
/// again, and the graph is read from the store.
pub fn read(db: &Connection, repository: &str, episode: &str) -> Option<(Graph, SearchIndex)> {
    if !enabled() {
        return None;
    }
    let path = graph_file(db, repository, episode)?;
    let file = std::fs::File::open(&path).ok()?;
    let read = identity(&file.metadata().ok()?);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::BufReader::new(file), &mut bytes).ok()?;
    let key = key(repository, episode);
    if !graph_key(&bytes, &key) {
        return None;
    }
    match unsealed(&bytes).and_then(|content| graphicle::snapshot::from_bytes(content, &key)) {
        Some(Ok(parts)) => Some(parts),
        _ => {
            remove_damaged(&path, read);
            None
        }
    }
}

/// Whether saved graphs are read and written: `GRAPHICLE_SAVED_GRAPH=0`
/// turns them off (the graph is then read from the store every time).
pub fn enabled() -> bool {
    !std::env::var("GRAPHICLE_SAVED_GRAPH").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Whether `episode` has a saved graph this build can read, without reading
/// it: its key, and its trailer's length against the file's (a truncated
/// file is not saved). A damaged byte within is found by [`read`].
pub fn saved(db: &Connection, repository: &str, episode: &str) -> bool {
    enabled()
        && graph_file(db, repository, episode).is_some_and(|path| {
            let key = key(repository, episode);
            whole_by_trailer(&path, 4096, |head| graph_key(head, &key))
        })
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_parts(path, &[bytes])
}

/// [`write_atomic`] of `parts` one after the other (a file and its
/// [`trailer`], without copying the file).
fn write_atomic_parts(path: &Path, parts: &[&[u8]]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent folder"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    for part in parts {
        tmp.write_all(part)?;
    }
    tmp.flush()?;
    // On disk before the name points at it: a crash leaves the old file or
    // the whole new one.
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// The episode `context` reads for `repository`: its current head set's,
/// when that is recorded, else the latest recorded one (under the lag
/// fallback, `crate::code_graph_arm::reading`).
pub fn read_episode(db: &Connection, repository: &str) -> Result<Option<String>> {
    let heads = crate::store::heads(db, repository)?;
    let current = graph_projection::episode_of(&heads);
    if graph_projection::is_recorded(db, repository, &current)? {
        return Ok(Some(current));
    }
    Ok(crate::store::one(
        db,
        "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
        [repository],
    )?
    .map(|row| crate::store::s(&row, "episode").to_string()))
}

/// Saved files of episodes other than the one `context` reads that are kept:
/// the one it read before (a branch switched back to finds its own). A sync
/// marks the files of the episode it finds saved as the folder's newest
/// ([`touch`]), so "before" counts the episodes synced, not only the ones
/// written.
const KEPT_EPISODES: usize = 1;
/// A temporary file of an interrupted write is removed after this long.
const STALE_TEMPORARY: std::time::Duration = std::time::Duration::from_secs(3600);

/// The record of a query file that could not be written for `episode`:
/// `<episode>.query-failed`, which holds the key it was written for and the
/// [`Failure`].
fn failed_file(db: &Connection, repository: &str, episode: &str) -> Option<PathBuf> {
    file_of(db, repository, episode, "query-failed")
}

/// A query file write that failed for an episode and build.
#[derive(Clone, Debug, PartialEq)]
struct Failure {
    /// The failure cannot pass by trying again: a code graph the file cannot
    /// hold (an error of kind `Unsupported`). Only the next code graph build,
    /// or another build of the program, tries again.
    permanent: bool,
    /// Failed writes in a row for this episode and build.
    attempts: u32,
    /// When a sync may try again, in seconds since 1970 ([`retry_wait`]);
    /// `graphicle sync`, `setup` and `doctor --fix` try again at once.
    retry_at: u64,
    error: String,
}

/// How long syncs wait to try a failed write again (an error that may pass,
/// such as a full disk): 10 minutes, doubled for each failure in a row, at
/// most 6 hours. A sync with nothing to build thus never writes the file at
/// each call, and a disk that is fixed gets its file without a hand.
fn retry_wait(attempts: u32) -> u64 {
    const FIRST: u64 = 600;
    const MOST: u64 = 6 * 3600;
    FIRST
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(16))
        .min(MOST)
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Failure {
    /// The failure of a write that failed with `error`, after `before` (the
    /// failure recorded for the same episode and build, if any), at `now`.
    fn of(error: &std::io::Error, before: Option<&Failure>, now: u64) -> Self {
        let permanent = error.kind() == std::io::ErrorKind::Unsupported;
        let attempts = before
            .filter(|before| !permanent && !before.permanent)
            .map_or(1, |before| before.attempts.saturating_add(1));
        Self {
            permanent,
            attempts,
            retry_at: if permanent {
                u64::MAX
            } else {
                now.saturating_add(retry_wait(attempts))
            },
            error: error.to_string(),
        }
    }

    /// The record's bytes: the key, `\n`, `permanent` or `retry <retry_at>
    /// <attempts>`, `\n`, the error.
    fn record(&self, key: &[u8]) -> Vec<u8> {
        let mut out = key.to_vec();
        out.push(b'\n');
        if self.permanent {
            out.extend_from_slice(b"permanent");
        } else {
            out.extend_from_slice(format!("retry {} {}", self.retry_at, self.attempts).as_bytes());
        }
        out.push(b'\n');
        out.extend_from_slice(self.error.as_bytes());
        out
    }

    /// The failure the record at `path` holds for `key`; none when it holds
    /// another key or does not read as a record.
    fn read(path: &Path, key: &[u8]) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let rest = bytes.strip_prefix(key)?.strip_prefix(b"\n")?;
        let text = String::from_utf8_lossy(rest);
        let (state, error) = text.split_once('\n')?;
        let mut words = state.split(' ');
        let (permanent, retry_at, attempts) = match words.next()? {
            "permanent" => (true, u64::MAX, 1),
            "retry" => (
                false,
                words.next()?.parse().ok()?,
                words.next()?.parse().ok()?,
            ),
            _ => return None,
        };
        Some(Self {
            permanent,
            attempts,
            retry_at,
            error: error.to_string(),
        })
    }

    /// Whether this failure holds a write back at `now`: always when it is
    /// permanent, else until `retry_at` unless `retry`.
    fn holds(&self, retry: bool, now: u64) -> bool {
        self.permanent || (!retry && now < self.retry_at)
    }

    /// When a sync may try again, as people read it.
    fn retry_time(&self) -> String {
        chrono::DateTime::from_timestamp(i64::try_from(self.retry_at).unwrap_or(i64::MAX), 0)
            .map_or_else(String::new, |time| {
                time.format("%Y-%m-%d %H:%M UTC").to_string()
            })
    }

    /// What a sync's report says of the query file this failure holds back.
    fn note(&self) -> String {
        if self.permanent {
            format!("not written: {}", self.error)
        } else {
            format!(
                "not written: {}; a sync tries again after {}, and graphicle sync at once",
                self.error,
                self.retry_time()
            )
        }
    }
}

/// The state of `episode`'s query file: `ready`, `missing`, `failed` (with
/// the error, whether it is `permanent`, and when a sync tries again,
/// `retry_after`) or `off`.
pub fn query_file_state(db: &Connection, repository: &str, episode: &str) -> Value {
    if !crate::query_file::enabled() {
        return serde_json::json!({"state":"off"});
    }
    let key = crate::query_file::key(repository, episode);
    if crate::query_file::path(db, repository, episode)
        .is_some_and(|path| crate::query_file::current(&path, &key))
    {
        return serde_json::json!({"state":"ready"});
    }
    match failed_file(db, repository, episode).and_then(|path| Failure::read(&path, &key)) {
        Some(failure) => serde_json::json!({"state":"failed","error":failure.error,
            "permanent":failure.permanent,
            "retry_after":(!failure.permanent).then(|| failure.retry_time())}),
        None => serde_json::json!({"state":"missing"}),
    }
}

/// Save the graph and the query file of the episode `context` reads
/// ([`read_episode`]) unless they are saved already, and remove the files
/// of the project's other episodes but the one synced before it, and
/// temporary files an interrupted write left. A query file write that
/// failed for this episode and build is tried again only when its failure
/// can pass, after its wait ([`retry_wait`]) or at once with `retry` (a
/// sync a person asked for: `graphicle sync`, `setup`, `doctor --fix`).
/// Returns what it did, for the sync's report.
pub fn ensure(db: &Connection, repository: &str, retry: bool) -> Result<Value> {
    if !enabled() {
        return Ok(serde_json::json!({"status":"off"}));
    }
    let Some(episode) = read_episode(db, repository)? else {
        return Ok(serde_json::json!({"status":"no_episode"}));
    };
    let (Some(graph_path), Some(edges_path), Some(query_path), Some(failed_path)) = (
        graph_file(db, repository, &episode),
        edges_file(db, repository, &episode),
        crate::query_file::path(db, repository, &episode),
        failed_file(db, repository, &episode),
    ) else {
        return Ok(serde_json::json!({"status":"no_folder"}));
    };
    let graph_key = key(repository, &episode);
    let query_key = crate::query_file::key(repository, &episode);
    // Whole by their keys and trailers (a damaged byte within is found by
    // the read that reaches it, which removes the file).
    let graph_saved = saved(db, repository, &episode)
        && whole_by_trailer(&edges_path, graph_key.len() + 10, |head| {
            FileEdges::starts_with_key(head, &graph_key)
        });
    // A query file is wanted unless it is current or turned off, or a
    // failure recorded for this episode and build holds it back.
    let query_current =
        crate::query_file::enabled() && crate::query_file::current(&query_path, &query_key);
    if query_current && failed_path.exists() {
        let _ = std::fs::remove_file(&failed_path);
    }
    let failure = (!query_current)
        .then(|| Failure::read(&failed_path, &query_key))
        .flatten();
    let held = failure
        .as_ref()
        .filter(|failure| failure.holds(retry, now_seconds()));
    let query_wanted = crate::query_file::enabled() && !query_current && held.is_none();
    if graph_saved && !query_wanted {
        touch(&[&graph_path, &edges_path, &query_path, &failed_path]);
        tidy(&graph_path, &episode);
        let mut report = serde_json::json!({"status":"saved","episode":episode});
        if let Some(failure) = held {
            report["query_file"] = serde_json::json!(failure.note());
        }
        return Ok(report);
    }
    let started = std::time::Instant::now();
    let mut bytes = 0;
    let (graph, index) = match graph_saved
        .then(|| read(db, repository, &episode))
        .flatten()
    {
        // Only the query file is missing (a store an earlier version
        // synced): it is made from the saved graph.
        Some(parts) => parts,
        None => {
            // The graph as every other process reads it from the store.
            let graph = graph_projection::load(db, repository, &episode)?;
            let index = SearchIndex::new(&graph);
            let saved = graphicle::snapshot::to_bytes(&graph_key, &graph, &index);
            let edges = FileEdges::of(&graph).encode(&graph_key);
            write_atomic_parts(&graph_path, &[&saved, &trailer(&saved)])?;
            write_atomic_parts(&edges_path, &[&edges, &trailer(&edges)])?;
            bytes += saved.len() + edges.len() + 2 * TRAILER;
            (graph, index)
        }
    };
    // The query file `context` reads (crate::query_file); a failure leaves
    // questions reading the saved graph, and is recorded: for good when it
    // cannot pass (a code graph the file cannot hold), else with the wait
    // before syncs try again.
    let query_file = if !query_wanted {
        match held {
            Some(failure) => serde_json::json!(failure.note()),
            None if crate::query_file::enabled() => serde_json::json!("saved"),
            None => serde_json::json!("off"),
        }
    } else {
        match crate::query_file::write(&query_path, repository, &episode, &graph, &index) {
            Ok(written) => {
                bytes += written;
                let _ = std::fs::remove_file(&failed_path);
                serde_json::json!("written")
            }
            Err(error) => {
                let failed = Failure::of(&error, failure.as_ref(), now_seconds());
                let _ = write_atomic(&failed_path, &failed.record(&query_key));
                serde_json::json!(format!("error: {error}"))
            }
        }
    };
    tidy(&graph_path, &episode);
    Ok(serde_json::json!({"status":"written","episode":episode,
        "bytes":bytes,"query_file":query_file,
        "seconds":(started.elapsed().as_secs_f64() * 10.0).round() / 10.0}))
}

/// Marks the saved files of the episode a sync found saved as the newest of
/// their folder (their modification time, which [`tidy`] orders by), so that
/// when the next episode is written, these are the ones kept beside it.
fn touch(paths: &[&Path]) {
    let now = std::time::SystemTime::now();
    for path in paths {
        if let Ok(file) = std::fs::File::options().write(true).open(path) {
            let _ = file.set_modified(now);
        }
    }
}

/// In the folder of `graph_path`, the saved files of `episode` and of the
/// [`KEPT_EPISODES`] most recently synced other episodes stay; the others
/// go, with temporary files older than [`STALE_TEMPORARY`].
fn tidy(graph_path: &Path, episode: &str) {
    let Some(folder) = graph_path.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut others: HashMap<String, (std::time::SystemTime, Vec<PathBuf>)> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(now);
        if name.starts_with(".tmp") {
            if now.duration_since(modified).unwrap_or_default() > STALE_TEMPORARY {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem == episode
            || !matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("graph" | "files" | "query" | "query-failed")
            )
        {
            continue;
        }
        let slot = others
            .entry(stem.to_string())
            .or_insert((std::time::UNIX_EPOCH, Vec::new()));
        slot.0 = slot.0.max(modified);
        slot.1.push(path);
    }
    let mut others: Vec<(std::time::SystemTime, Vec<PathBuf>)> = others.into_values().collect();
    others.sort_by_key(|other| std::cmp::Reverse(other.0));
    for (_, paths) in others.into_iter().skip(KEPT_EPISODES) {
        for path in paths {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The file relationships of an episode, as `query::episode_graph` lists
/// them (`edges`): pairs of project files with the relation kind, and how
/// many edges that list holds before and after folding by kind.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileEdges {
    pub paths: Vec<String>,
    /// (source, target, kind) by index into `paths` and `kinds`, distinct,
    /// in first-listed order.
    pub pairs: Vec<(u32, u32, u16)>,
    /// How many of `episode_graph`'s edges each pair holds (one per line).
    pub lines: Vec<u32>,
    pub kinds: Vec<String>,
    /// `episode_graph`'s edge count (one per source, target, line and kind).
    pub listed: usize,
}

fn node_text<'a>(attributes: &'a impl graphicle::record::Properties, key: &str) -> Option<&'a str> {
    attributes
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}

impl FileEdges {
    /// `episode_graph`'s edges of `graph`, folded to pairs of files.
    pub fn of(graph: &Graph) -> Self {
        let nodes = graph.nodes();
        let relpath = |index: usize| {
            let attributes = &nodes[index].attributes;
            node_text(attributes, "relpath")
                .or_else(|| node_text(attributes, "source_file"))
                .map(|p| p.replace('\\', "/"))
        };
        let node_line = |index: usize| crate::query::node_line_of(&nodes[index].attributes);
        let mut paths: Vec<String> = Vec::new();
        let mut path_ids: HashMap<String, u32> = HashMap::new();
        let mut kinds: Vec<String> = Vec::new();
        let mut kind_ids: HashMap<String, u16> = HashMap::new();
        let mut seen = BTreeSet::new();
        let mut pairs_seen: HashMap<(u32, u32, u16), usize> = HashMap::new();
        let mut pairs = Vec::new();
        let mut lines: Vec<u32> = Vec::new();
        let mut listed = 0usize;
        for (source, target, edge) in graph.iter_edges() {
            let relation = node_text(&edge.attributes, "relation").unwrap_or("");
            if relation.is_empty()
                || graphicle::markdown_resolution::CONTAINMENT_RELATIONS.contains(&relation)
            {
                continue;
            }
            let (Some(from), Some(to)) = (relpath(source), relpath(target)) else {
                continue;
            };
            if from == to {
                continue;
            }
            let line = edge
                .attributes
                .get("source_location")
                .and_then(Value::as_str)
                .and_then(|l| l.strip_prefix('L'))
                .and_then(|l| l.split(['-', ':']).next()?.trim().parse::<u64>().ok())
                .or_else(|| node_line(source))
                .unwrap_or(1);
            let kind = match relation {
                "imports" | "imports_from" => "imports",
                other => other,
            };
            if !seen.insert((from.clone(), to.clone(), line, kind.to_string())) {
                continue;
            }
            listed += 1;
            let mut id = |path: String| -> u32 {
                if let Some(&id) = path_ids.get(&path) {
                    return id;
                }
                let id = paths.len() as u32;
                paths.push(path.clone());
                path_ids.insert(path, id);
                id
            };
            let (a, b) = (id(from), id(to));
            let k = *kind_ids.entry(kind.to_string()).or_insert_with(|| {
                kinds.push(kind.to_string());
                (kinds.len() - 1) as u16
            });
            match pairs_seen.get(&(a, b, k)) {
                Some(&at) => lines[at] += 1,
                None => {
                    pairs_seen.insert((a, b, k), pairs.len());
                    pairs.push((a, b, k));
                    lines.push(1);
                }
            }
        }
        Self {
            paths,
            pairs,
            lines,
            kinds,
            listed,
        }
    }

    fn encode(&self, key: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let put = |out: &mut Vec<u8>, mut v: u64| loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        };
        put(&mut out, key.len() as u64);
        out.extend_from_slice(key);
        put(&mut out, self.listed as u64);
        for table in [&self.paths, &self.kinds] {
            put(&mut out, table.len() as u64);
            for text in table.iter() {
                put(&mut out, text.len() as u64);
                out.extend_from_slice(text.as_bytes());
            }
        }
        put(&mut out, self.pairs.len() as u64);
        for ((a, b, k), n) in self.pairs.iter().zip(&self.lines) {
            put(&mut out, u64::from(*a));
            put(&mut out, u64::from(*b));
            put(&mut out, u64::from(*k));
            put(&mut out, u64::from(*n));
        }
        out
    }

    /// A varint of `bytes` at `position`, which it moves past it.
    fn varint(bytes: &[u8], position: &mut usize) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = *bytes.get(*position)?;
            *position += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }

    /// Whether `bytes`, the start of a file relationships file, carry `key`.
    fn starts_with_key(bytes: &[u8], key: &[u8]) -> bool {
        let mut position = 0usize;
        Self::varint(bytes, &mut position).is_some_and(|length| {
            length == key.len() as u64 && bytes.get(position..position + key.len()) == Some(key)
        })
    }

    /// The file relationships `bytes` hold for `key`; none when they hold
    /// another key or do not decode whole: every pair's files and kind must
    /// be in their tables, and the line counts must add up.
    fn decode(bytes: &[u8], key: &[u8]) -> Option<Self> {
        let mut position = 0usize;
        let get = |position: &mut usize| Self::varint(bytes, position);
        if !Self::starts_with_key(bytes, key) {
            return None;
        }
        get(&mut position)?;
        position = position.checked_add(key.len())?;
        let listed = usize::try_from(get(&mut position)?).ok()?;
        let mut tables: Vec<Vec<String>> = Vec::new();
        for _ in 0..2 {
            let count = usize::try_from(get(&mut position)?).ok()?;
            let mut table = Vec::with_capacity(count.min(1 << 20));
            for _ in 0..count {
                let n = usize::try_from(get(&mut position)?).ok()?;
                let text =
                    std::str::from_utf8(bytes.get(position..position.checked_add(n)?)?).ok()?;
                position += n;
                table.push(text.to_string());
            }
            tables.push(table);
        }
        let kinds = tables.pop()?;
        let paths = tables.pop()?;
        let count = usize::try_from(get(&mut position)?).ok()?;
        let mut pairs = Vec::with_capacity(count.min(1 << 24));
        let mut lines = Vec::with_capacity(count.min(1 << 24));
        for _ in 0..count {
            let a = u32::try_from(get(&mut position)?).ok()?;
            let b = u32::try_from(get(&mut position)?).ok()?;
            let k = u16::try_from(get(&mut position)?).ok()?;
            if a as usize >= paths.len() || b as usize >= paths.len() || k as usize >= kinds.len() {
                return None;
            }
            pairs.push((a, b, k));
            lines.push(u32::try_from(get(&mut position)?).ok()?);
        }
        let total = lines
            .iter()
            .try_fold(0usize, |total, n| total.checked_add(*n as usize))?;
        (position == bytes.len() && total == listed).then_some(Self {
            paths,
            pairs,
            lines,
            kinds,
            listed,
        })
    }

    /// The saved file relationships of `episode`, when this build wrote them
    /// and they are whole: a file with this build's key whose trailer does
    /// not hold or that does not decode (truncated or damaged) is removed, so
    /// that the next sync writes it again.
    pub fn read(db: &Connection, repository: &str, episode: &str) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let path = edges_file(db, repository, episode)?;
        let file = std::fs::File::open(&path).ok()?;
        let read = identity(&file.metadata().ok()?);
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::BufReader::new(file), &mut bytes).ok()?;
        let key = key(repository, episode);
        if !Self::starts_with_key(&bytes, &key) {
            return None;
        }
        match unsealed(&bytes).and_then(|content| Self::decode(content, &key)) {
            Some(edges) => Some(edges),
            None => {
                remove_damaged(&path, read);
                None
            }
        }
    }

    /// The pairs touching any of `files`, as `episode_graph`'s edges
    /// (`source`, `target` and `kind`).
    pub fn touching(&self, files: &BTreeSet<&str>) -> Vec<Value> {
        let ids: BTreeSet<u32> = self
            .paths
            .iter()
            .enumerate()
            .filter(|(_, path)| files.contains(path.as_str()))
            .map(|(i, _)| i as u32)
            .collect();
        self.pairs
            .iter()
            .filter(|(a, b, _)| ids.contains(a) || ids.contains(b))
            .map(|(a, b, k)| {
                serde_json::json!({"source":self.paths[*a as usize],"target":self.paths[*b as usize],
                    "kind":self.kinds[*k as usize]})
            })
            .collect()
    }

    /// The files any pair joins to one of `files`.
    pub fn neighbours(&self, files: &BTreeSet<&str>) -> BTreeSet<String> {
        let ids: BTreeMap<u32, ()> = self
            .paths
            .iter()
            .enumerate()
            .filter(|(_, path)| files.contains(path.as_str()))
            .map(|(i, _)| (i as u32, ()))
            .collect();
        let mut found = BTreeSet::new();
        for (a, b, _) in &self.pairs {
            if ids.contains_key(a) {
                found.insert(self.paths[*b as usize].clone());
            }
            if ids.contains_key(b) {
                found.insert(self.paths[*a as usize].clone());
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_edges_round_trip() {
        let edges = FileEdges {
            paths: vec!["a.rs".into(), "b.rs".into()],
            pairs: vec![(0, 1, 0), (1, 0, 1)],
            lines: vec![2, 1],
            kinds: vec!["imports".into(), "calls".into()],
            listed: 3,
        };
        let bytes = edges.encode(b"k");
        assert_eq!(FileEdges::decode(&bytes, b"k"), Some(edges.clone()));
        assert_eq!(FileEdges::decode(&bytes, b"other"), None);
        assert!(FileEdges::starts_with_key(&bytes, b"k"));
        assert!(!FileEdges::starts_with_key(&bytes, b"kk"));
        let a = BTreeSet::from(["a.rs"]);
        assert_eq!(edges.touching(&a).len(), 2);
        assert_eq!(edges.neighbours(&a), BTreeSet::from(["b.rs".to_string()]));
    }

    /// A pair whose file or kind is outside its table, line counts that do
    /// not add up, and a file cut short or with bytes after its end do not
    /// decode: the reader then takes the graph's own file relationships,
    /// with the same packet, instead of indexing out of the tables.
    #[test]
    fn file_edges_out_of_their_tables_do_not_decode() {
        let edges = FileEdges {
            paths: vec!["a.rs".into(), "b.rs".into()],
            pairs: vec![(0, 1, 0), (1, 0, 1)],
            lines: vec![2, 1],
            kinds: vec!["imports".into(), "calls".into()],
            listed: 3,
        };
        for (what, pairs) in [
            ("source", vec![(66, 1, 0), (1, 0, 1)]),
            ("target", vec![(0, 2, 0), (1, 0, 1)]),
            ("kind", vec![(0, 1, 0), (1, 0, 2)]),
        ] {
            let bad = FileEdges {
                pairs,
                ..edges.clone()
            };
            let bytes = bad.encode(b"k");
            assert_eq!(FileEdges::decode(&bytes, b"k"), None, "{what}");
        }
        let miscounted = FileEdges {
            listed: 4,
            ..edges.clone()
        };
        assert_eq!(FileEdges::decode(&miscounted.encode(b"k"), b"k"), None);
        let bytes = edges.encode(b"k");
        assert_eq!(FileEdges::decode(&bytes[..bytes.len() - 1], b"k"), None);
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(FileEdges::decode(&longer, b"k"), None);
        // A length past the end of the bytes.
        let mut huge = b"\x01k\x03".to_vec();
        huge.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert_eq!(FileEdges::decode(&huge, b"k"), None);
    }

    /// The trailer gives a file's length and checksum: a file cut short, one
    /// with a changed byte or one without its trailer is not whole.
    #[test]
    fn the_trailer_finds_a_cut_or_changed_file() {
        let content = (0..10_000u32)
            .map(|i| (i * 31 % 251) as u8)
            .collect::<Vec<u8>>();
        let mut sealed = content.clone();
        sealed.extend_from_slice(&trailer(&content));
        assert_eq!(unsealed(&sealed), Some(content.as_slice()));
        assert_eq!(unsealed(&sealed[..sealed.len() - 1]), None);
        assert_eq!(unsealed(&content), None);
        assert_eq!(unsealed(b"short"), None);
        let mut changed = sealed.clone();
        changed[5_000] ^= 1;
        assert_eq!(unsealed(&changed), None);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("f.files");
        std::fs::write(&path, &sealed).unwrap();
        assert!(whole_by_trailer(&path, 8, |head| head.starts_with(&content[..8])));
        assert!(!whole_by_trailer(&path, 8, |_| false));
        std::fs::write(&path, &sealed[..sealed.len() - 100]).unwrap();
        assert!(!whole_by_trailer(&path, 8, |_| true));
    }

    /// A failure that can pass waits 10 minutes, then twice as long for each
    /// failure in a row, at most 6 hours; a sync a person asks for tries
    /// again at once. A failure that cannot pass (a code graph the file
    /// cannot hold) holds every sync back. Records read back as written.
    #[test]
    fn a_failure_that_can_pass_waits_and_one_that_cannot_holds() {
        assert_eq!(retry_wait(1), 600);
        assert_eq!(retry_wait(2), 1200);
        assert_eq!(retry_wait(3), 2400);
        assert_eq!(retry_wait(7), 6 * 3600);
        assert_eq!(retry_wait(1000), 6 * 3600);
        let full = std::io::Error::new(std::io::ErrorKind::StorageFull, "no space left");
        let first = Failure::of(&full, None, 1_000);
        assert!(!first.permanent);
        assert_eq!((first.attempts, first.retry_at), (1, 1_600));
        assert!(first.holds(false, 1_599));
        assert!(!first.holds(false, 1_600));
        assert!(!first.holds(true, 1_000));
        let second = Failure::of(&full, Some(&first), 2_000);
        assert_eq!((second.attempts, second.retry_at), (2, 3_200));
        let refused =
            std::io::Error::new(std::io::ErrorKind::Unsupported, "a node id is not a string");
        let permanent = Failure::of(&refused, Some(&second), 3_000);
        assert!(permanent.permanent);
        assert!(permanent.holds(true, u64::MAX - 1));
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("e.query-failed");
        for failure in [&first, &second, &permanent] {
            std::fs::write(&path, failure.record(b"k\0e")).unwrap();
            assert_eq!(Failure::read(&path, b"k\0e").as_ref(), Some(failure));
            assert_eq!(Failure::read(&path, b"other"), None);
        }
        std::fs::write(&path, b"k\0e\nnot a record").unwrap();
        assert_eq!(Failure::read(&path, b"k\0e"), None);
        assert!(second.note().contains("graphicle sync at once"));
        assert!(!permanent.note().contains("tries again"));
    }
}
