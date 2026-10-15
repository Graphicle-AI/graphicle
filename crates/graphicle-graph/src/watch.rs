// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/watch.py
//! Incremental rebuilds, conservative reconciliation, and filesystem watching.
use crate::extractors::resolution::resolved;
use crate::{Result, build, cluster, detect, export, extract, paths, security, watch_paths::*};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub fn root_marker_value(path: &Path) -> Result<String> {
    root_marker_value_for(path, paths::OUT_DIR.as_str())
}

/// Opt-in per-phase rebuild timing (`GRAPHICLE_WATCH_TIMING=1`), written to
/// stderr so stdout parity with upstream is untouched.
struct PhaseTimer {
    enabled: bool,
    start: Instant,
    last: Instant,
    phases: Vec<(&'static str, Duration)>,
    // GRAPHICLE_PROFILE=1: CPU seconds at the last mark and at the start.
    cpu_start: f64,
    cpu_last: f64,
}

impl PhaseTimer {
    fn new() -> Self {
        let now = Instant::now();
        let cpu = crate::profile::cpu_now();
        Self {
            enabled: std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1"),
            start: now,
            last: now,
            phases: vec![],
            cpu_start: cpu,
            cpu_last: cpu,
        }
    }
    fn mark(&mut self, name: &'static str) {
        let profiling = crate::profile::enabled();
        if self.enabled || profiling {
            let now = Instant::now();
            if profiling {
                let cpu = crate::profile::cpu_now();
                crate::profile::report(
                    &format!("rebuild.{name}"),
                    (now - self.last).as_secs_f64(),
                    cpu - self.cpu_last,
                );
                self.cpu_last = cpu;
            }
            self.phases.push((name, now - self.last));
            self.last = now;
        }
    }
    fn report(&self, tag: &str) {
        if crate::profile::enabled() {
            crate::profile::report(
                "rebuild.total",
                self.start.elapsed().as_secs_f64(),
                crate::profile::cpu_now() - self.cpu_start,
            );
        }
        if self.enabled {
            let parts = self
                .phases
                .iter()
                .map(|(n, d)| format!("{n}={:.1}ms", d.as_secs_f64() * 1000.0))
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!(
                "[graphicle timing] {tag} total={:.1}ms {parts}",
                self.start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

/// `_graphify_root_marker_value` (upstream #3375): a shared absolute
/// `GRAPHICLE_OUT` records the resolved watch path so the marker stays
/// meaningful from any cwd; a relative one keeps the path as given.
pub fn root_marker_value_for(path: &Path, out_dir: &str) -> Result<String> {
    let marker = if Path::new(out_dir).is_absolute() {
        crate::fsutil::resolve_missing(path)?
    } else {
        path.to_path_buf()
    };
    Ok(marker.to_string_lossy().into_owned())
}

/// `.pending.lock`, opened to append. Windows also needs read access:
/// `LockFileEx` refuses a handle that may only append.
fn open_pending_lock(out: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .read(cfg!(windows))
        .open(out.join(".pending.lock"))
}
pub fn queue_pending(out: &Path, changed: &[PathBuf]) -> Result<()> {
    if changed.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(out).map_err(crate::Error::from)?;
    let lock = open_pending_lock(out).map_err(crate::Error::from)?;
    lock.lock().map_err(crate::Error::from)?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join(".pending_changes"))
        .map_err(crate::Error::from)?;
    f.write_all(
        changed
            .iter()
            .map(|p| format!("{}\n", p.display()))
            .collect::<String>()
            .as_bytes(),
    )
    .map_err(crate::Error::from)
}
pub fn drain_pending(out: &Path) -> Vec<PathBuf> {
    if !out.is_dir() {
        return vec![];
    }
    let Ok(lock) = open_pending_lock(out) else {
        return vec![];
    };
    if lock.lock().is_err() {
        return vec![];
    }
    let path = out.join(".pending_changes");
    let Ok(s) = fs::read_to_string(&path) else {
        return vec![];
    };
    let _ = fs::remove_file(path);
    let mut seen = BTreeSet::new();
    s.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && seen.insert(s.to_string()))
        .map(PathBuf::from)
        .collect()
}
/// The marker a full-corpus rebuild request leaves when it cannot run.
const PENDING_FULL_FILENAME: &str = ".pending_full";
/// `_queue_pending_full` (0.9.87, #4274): record that a full-corpus rebuild
/// was requested but could not run. Full requests have no path list for
/// `.pending_changes`, so they leave a marker the lock holder consumes.
pub fn queue_pending_full(out: &Path) -> Result<()> {
    fs::create_dir_all(out).map_err(crate::Error::from)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join(PENDING_FULL_FILENAME))
        .map(drop)
        .map_err(crate::Error::from)
}
/// `_drain_pending_full`: consume the full-rebuild marker; true when one
/// was queued.
pub fn drain_pending_full(out: &Path) -> bool {
    fs::remove_file(out.join(PENDING_FULL_FILENAME)).is_ok()
}
pub fn merge_changed(groups: &[Vec<PathBuf>]) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    groups
        .iter()
        .flatten()
        .filter(|p| seen.insert((*p).clone()))
        .cloned()
        .collect()
}
pub fn read_config(out: &Path) -> Value {
    fs::read(out.join(".graphicle_build.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object)
        .unwrap_or(json!({}))
}
pub fn write_config(out: &Path, excludes: &[String], gitignore: Option<bool>) {
    if excludes.is_empty() && gitignore.is_none() {
        return;
    }
    let mut config = read_config(out);
    if !excludes.is_empty() {
        config["excludes"] = json!(excludes);
    }
    if let Some(b) = gitignore {
        config["gitignore"] = json!(b);
    }
    let _ = fs::create_dir_all(out);
    let _ = paths::write_json_atomic(&out.join(".graphicle_build.json"), &config, None, false);
}
pub fn build_excludes(out: &Path) -> Vec<String> {
    items(&read_config(out), "excludes")
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}
pub fn build_gitignore(out: &Path) -> bool {
    read_config(out)["gitignore"].as_bool().unwrap_or(true)
}
/// `extract --docs` / `--routes` was used for this output: `update` and
/// `watch` keep doing it (opt-in, beyond the upstream port).
pub fn build_flag(out: &Path, key: &str) -> bool {
    read_config(out)[key].as_bool().unwrap_or(false)
}
pub fn build_docs(out: &Path) -> bool {
    build_flag(out, "docs")
}
pub fn write_flag_config(out: &Path, key: &str) {
    let mut config = read_config(out);
    config[key] = json!(true);
    let _ = fs::create_dir_all(out);
    let _ = paths::write_json_atomic(&out.join(".graphicle_build.json"), &config, None, false);
}
pub struct RebuildLock {
    _file: File,
}
/// Graphify's `_rebuild_lock` on Windows, where byte-range locks are
/// mandatory: lock one byte at offset 4096 so other processes can still read
/// the PID at byte 0.
#[cfg(windows)]
const WINDOWS_LOCK_OFFSET: u64 = 4096;
impl RebuildLock {
    pub fn acquire(out: &Path, blocking: bool) -> Result<Option<Self>> {
        fs::create_dir_all(out).map_err(crate::Error::from)?;
        let mut f = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(out.join(".rebuild.lock"))
            .map_err(crate::Error::from)?;
        #[cfg(windows)]
        while !crate::win::try_lock_range(&f, WINDOWS_LOCK_OFFSET, 1).map_err(crate::Error::from)? {
            if !blocking {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        #[cfg(not(windows))]
        if blocking {
            f.lock().map_err(crate::Error::from)?;
        } else {
            match f.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        let _ = f.set_len(0);
        let _ = writeln!(f, "{}", std::process::id());
        Ok(Some(Self { _file: f }))
    }
}
#[cfg(windows)]
impl Drop for RebuildLock {
    fn drop(&mut self) {
        crate::win::unlock_range(&self._file, WINDOWS_LOCK_OFFSET, 1);
    }
}
pub fn node_communities(data: &Value) -> BTreeMap<String, i64> {
    communities_by_id(
        items_ref(data, "nodes")
            .iter()
            .map(|n| (&n["id"], &n["community"])),
    )
}
/// [`node_communities`] of each node's `id` and `community` (`Null` where
/// the record has none, or is not an object).
fn communities_by_id<'a>(
    nodes: impl Iterator<Item = (&'a Value, &'a Value)>,
) -> BTreeMap<String, i64> {
    nodes
        .filter_map(|(id, community)| {
            if id.is_null() {
                return None;
            }
            let cid = community
                .as_i64()
                .or_else(|| community.as_str()?.parse().ok())?;
            Some((
                if let Some(s) = id.as_str() {
                    s.into()
                } else {
                    id.to_string()
                },
                cid,
            ))
        })
        .collect()
}
fn sort_array(data: &mut Value, key: &str) {
    if let Some(a) = data.get_mut(key).and_then(Value::as_array_mut) {
        a.sort_by_cached_key(crate::extract_normalize::canonical_bytes);
    }
}
pub fn canonical_graph(data: &Value, topology: bool) -> Value {
    crate::extract_normalize::canonical(&canonical_prepared(data.clone(), topology))
}
/// Canonical comparison key: `canonical_graph(a, t) == canonical_graph(b, t)`
/// holds exactly when the keys are equal, but the key is produced without
/// building the intermediate sorted maps.
pub fn canonical_graph_key(data: &Value, topology: bool) -> Vec<u8> {
    let Value::Object(top) = data else {
        return canonical_graph_key_owned(data.clone(), topology);
    };
    // Encoded from the borrowed graph: the fields `prepared_fields` drops or
    // rewrites are skipped or substituted per record, so no copy of a graph
    // (hundreds of MB for a large one) is made to compare it.
    let workers = crate::par::workers(None);
    let directed = json!(data["directed"].as_bool().unwrap_or(false));
    let mut keys = top
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "built_at_commit")
        .collect::<Vec<_>>();
    if !topology && !top.contains_key("directed") {
        keys.push("directed");
    }
    keys.sort_unstable();
    let mut out = Vec::with_capacity(1 << 16);
    let graph_meta = topology_graph_metadata(data, topology);
    out.push(b'{');
    for (i, k) in keys.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, k).expect("string serializes");
        out.push(b':');
        if !topology && k == "directed" {
            crate::extract_normalize::write_canonical(&directed, None, &mut out);
            continue;
        }
        match &top[k] {
            Value::Array(items) if SORTED_ARRAYS.contains(&k) => {
                let rewritten = topology && matches!(k, "nodes" | "links" | "edges");
                let mut encoded = crate::par::map(items, workers, |item| {
                    if rewritten {
                        prepared_record(item, k == "nodes")
                    } else {
                        Some(crate::extract_normalize::canonical_bytes(item))
                    }
                })
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                encoded.sort_unstable();
                out.push(b'[');
                for (j, e) in encoded.iter().enumerate() {
                    if j > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(e);
                }
                out.push(b']');
            }
            other => crate::extract_normalize::write_canonical(
                graph_meta
                    .as_ref()
                    .filter(|_| k == "graph")
                    .unwrap_or(other),
                None,
                &mut out,
            ),
        }
    }
    out.push(b'}');
    out
}
/// The stored `graph` object as the topology comparison reads it (0.9.79,
/// #4167): without the format/version provenance (`schema_version`, the
/// writer's version) that a candidate built fresh from the graph never
/// carries. `None` when nothing is dropped.
fn topology_graph_metadata(data: &Value, topology: bool) -> Option<Value> {
    if !topology {
        return None;
    }
    without_version_keys(data.get("graph")?)
}
/// A `graph` object without its [`GRAPH_VERSION_KEYS`]; `None` when it is
/// not an object or has none of them.
fn without_version_keys(graph: &Value) -> Option<Value> {
    let meta = graph.as_object()?;
    if !GRAPH_VERSION_KEYS.iter().any(|k| meta.contains_key(*k)) {
        return None;
    }
    let mut meta = meta.clone();
    for key in GRAPH_VERSION_KEYS {
        meta.shift_remove(key);
    }
    Some(Value::Object(meta))
}
/// `graph.schema_version` and the writer stamp: the engine's
/// `graphicle_version`, and `graphify_version` in a graph.json the upstream
/// tool wrote (upstream drops its own key).
const GRAPH_VERSION_KEYS: [&str; 3] = [
    "schema_version",
    crate::export::GRAPH_VERSION_KEY,
    "graphify_version",
];
/// SHA-256 standing in for [`canonical_graph_key`]: two graphs have equal
/// digests exactly when their keys are equal (up to a SHA-256 collision). The
/// records of each sorted array are hashed one by one and their digests
/// sorted, so no key (hundreds of MB for a large graph) is ever built.
pub fn canonical_graph_digest(data: &Value, topology: bool) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let Value::Object(top) = data else {
        let mut h = Sha256::new();
        h.update(canonical_graph_key_owned(data.clone(), topology));
        return h.finalize().into();
    };
    let workers = crate::par::workers(None);
    let directed = json!(data["directed"].as_bool().unwrap_or(false));
    let mut keys = top
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "built_at_commit")
        .collect::<Vec<_>>();
    if !topology && !top.contains_key("directed") {
        keys.push("directed");
    }
    keys.sort_unstable();
    let graph_meta = topology_graph_metadata(data, topology);
    digest_entries(keys.into_iter().map(|k| {
        let entry = if !topology && k == "directed" {
            DigestEntry::Value(&directed)
        } else {
            match &top[k] {
                Value::Array(items) if SORTED_ARRAYS.contains(&k) => {
                    let rewritten = topology && matches!(k, "nodes" | "links" | "edges");
                    DigestEntry::Records(record_digests(
                        items,
                        workers,
                        rewritten.then_some(k == "nodes"),
                    ))
                }
                other => DigestEntry::Value(
                    graph_meta
                        .as_ref()
                        .filter(|_| k == "graph")
                        .unwrap_or(other),
                ),
            }
        };
        (k, entry)
    }))
}
/// One top-level entry of a graph document as [`canonical_graph_digest`]
/// hashes it: the record digests of a sorted array (in any order), or a
/// value written canonically.
enum DigestEntry<'a> {
    Records(Vec<[u8; 32]>),
    Value(&'a Value),
}
/// The digest of a document's entries, given in sorted key order: the
/// object's canonical text with each sorted array's records replaced by
/// their sorted SHA-256 digests.
fn digest_entries<'a>(entries: impl IntoIterator<Item = (&'a str, DigestEntry<'a>)>) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut bytes = Vec::new();
    h.update(b"{");
    for (i, (k, entry)) in entries.into_iter().enumerate() {
        if i > 0 {
            h.update(b",");
        }
        bytes.clear();
        serde_json::to_writer(&mut bytes, k).expect("string serializes");
        bytes.push(b':');
        h.update(&bytes);
        match entry {
            DigestEntry::Records(mut digests) => {
                digests.sort_unstable();
                h.update(b"[");
                for (j, d) in digests.iter().enumerate() {
                    if j > 0 {
                        h.update(b",");
                    }
                    h.update(d);
                }
                h.update(b"]");
            }
            DigestEntry::Value(value) => {
                bytes.clear();
                crate::extract_normalize::write_canonical(value, None, &mut bytes);
                h.update(&bytes);
            }
        }
    }
    h.update(b"}");
    h.finalize().into()
}
/// The SHA-256 of each record's canonical bytes, on `workers` threads.
/// `prepared` is `Some(node)` for the nodes or edges of a topology
/// comparison ([`prepared_record`], which skips non-objects).
fn record_digests(items: &[Value], workers: usize, prepared: Option<bool>) -> Vec<[u8; 32]> {
    use sha2::{Digest, Sha256};
    crate::par::map(items, workers, |item| {
        let encoded = match prepared {
            Some(node) => prepared_record(item, node),
            None => Some(crate::extract_normalize::canonical_bytes(item)),
        };
        encoded.map(|e| <[u8; 32]>::from(Sha256::digest(&e)))
    })
    .into_iter()
    .flatten()
    .collect()
}
/// [`canonical_graph_digest`] with `topology` of the graph's node-link
/// document as a rebuild compares it (`to_node_link("links")` plus its
/// `hyperedges` attribute, or an empty list), made one record at a time:
/// the document itself, as large as the graph, is never built.
fn graph_topology_digest(graph: &crate::graph::Graph) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let workers = crate::par::workers(None);
    let digest = |record: &Value, node: bool| {
        prepared_record(record, node).map(|e| <[u8; 32]>::from(Sha256::digest(&e)))
    };
    let indices = (0..graph.nodes.len()).collect::<Vec<_>>();
    let nodes = crate::par::map(&indices, workers, |i| {
        digest(&graph.node_link_node(*i), true)
    })
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    drop(indices);
    let edges = graph.iter_edges().collect::<Vec<_>>();
    let links = crate::par::map(&edges, workers, |(u, v, edge)| {
        digest(&graph.node_link_edge(*u, *v, edge), false)
    })
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    drop(edges);
    let directed = Value::Bool(graph.directed);
    let multigraph = Value::Bool(graph.multigraph);
    let empty = json!([]);
    let hyperedges = match graph.attributes.get("hyperedges").unwrap_or(&empty) {
        Value::Array(items) => DigestEntry::Records(record_digests(items, workers, None)),
        other => DigestEntry::Value(other),
    };
    let meta = without_version_keys(&graph.attributes);
    digest_entries([
        ("directed", DigestEntry::Value(&directed)),
        (
            "graph",
            DigestEntry::Value(meta.as_ref().unwrap_or(&graph.attributes)),
        ),
        ("hyperedges", hyperedges),
        ("links", DigestEntry::Records(links)),
        ("multigraph", DigestEntry::Value(&multigraph)),
        ("nodes", DigestEntry::Records(nodes)),
    ])
}
/// Canonical bytes of one topology node or edge as `prepared_fields` leaves
/// it (`None` for the non-objects it drops): a node without its community
/// fields; an edge without `_src`/`_tgt`/`confidence_score`, with `source` and
/// `target` taken from `_src`/`_tgt` when both are set.
fn prepared_record(item: &Value, node: bool) -> Option<Vec<u8>> {
    let Value::Object(o) = item else {
        return None;
    };
    let (drop, set): (&[&str], Vec<(&str, &Value)>) = if node {
        (&["community", "community_name", "norm_label"], vec![])
    } else {
        let set = match (o.get("_src"), o.get("_tgt")) {
            (Some(a), Some(b)) if !a.is_null() && !b.is_null() => {
                vec![("source", a), ("target", b)]
            }
            _ => vec![],
        };
        (&["_src", "_tgt", "confidence_score"], set)
    };
    let mut keys = o
        .keys()
        .map(String::as_str)
        .filter(|k| !drop.contains(k) && !set.iter().any(|(s, _)| s == k))
        .collect::<Vec<_>>();
    keys.extend(set.iter().map(|(k, _)| *k));
    keys.sort_unstable();
    let mut out = Vec::with_capacity(256);
    out.push(b'{');
    for (i, k) in keys.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, k).expect("string serializes");
        out.push(b':');
        let value = set.iter().find(|(s, _)| *s == k).map_or(&o[k], |(_, v)| *v);
        crate::extract_normalize::write_canonical(value, None, &mut out);
    }
    out.push(b'}');
    Some(out)
}
/// `canonical_graph_key` of a graph the caller no longer needs: no copy.
pub fn canonical_graph_key_owned(data: Value, topology: bool) -> Vec<u8> {
    crate::extract_normalize::canonical_bytes_sorting(
        &prepared_fields(data, topology),
        &SORTED_ARRAYS,
        crate::par::workers(None),
    )
}
/// The arrays `canonical_prepared` sorts; element order within them is not
/// part of a graph's identity.
const SORTED_ARRAYS: [&str; 4] = ["nodes", "links", "edges", "hyperedges"];
fn canonical_prepared(data: Value, topology: bool) -> Value {
    let mut data = prepared_fields(data, topology);
    for k in SORTED_ARRAYS {
        sort_array(&mut data, k);
    }
    data
}
/// `canonical_prepared` before its sorting: the fields that are not part of
/// the compared identity removed.
fn prepared_fields(mut data: Value, topology: bool) -> Value {
    if let Some(o) = data.as_object_mut() {
        o.shift_remove("built_at_commit");
    }
    if !topology {
        data["directed"] = json!(data["directed"].as_bool().unwrap_or(false));
    }
    if let Some(meta) = topology_graph_metadata(&data, topology) {
        data["graph"] = meta;
    }
    if topology {
        if let Some(ns) = data.get_mut("nodes").and_then(Value::as_array_mut) {
            ns.retain(Value::is_object);
            for n in ns {
                for k in ["community", "community_name", "norm_label"] {
                    n.as_object_mut().unwrap().shift_remove(k);
                }
            }
        }
        for key in ["links", "edges"] {
            if let Some(es) = data.get_mut(key).and_then(Value::as_array_mut) {
                es.retain(Value::is_object);
                for e in es {
                    let o = e.as_object_mut().unwrap();
                    let a = o.shift_remove("_src");
                    let b = o.shift_remove("_tgt");
                    if let (Some(a), Some(b)) = (a, b)
                        && !a.is_null()
                        && !b.is_null()
                    {
                        o.insert("source".into(), a);
                        o.insert("target".into(), b);
                    }
                    o.shift_remove("confidence_score");
                }
            }
        }
    }
    data
}
#[allow(clippy::too_many_arguments)]
pub fn check_shrink(
    force: bool,
    old: &Value,
    new: &Value,
    deletions: bool,
    rebuilt: Option<&BTreeSet<String>>,
    failed: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
    messages: &mut Vec<String>,
) -> bool {
    check_shrink_with(
        force,
        &ShrinkBase::of(old),
        new,
        deletions,
        rebuilt,
        failed,
        deleted,
        messages,
    )
}
/// What [`check_shrink`] reads from the stored graph: whether it is empty,
/// each node's id (as JSON text) and source file, and the stored records the
/// saved-semantic-node check reads (see [`ShrinkBase::of`]).
#[derive(Default)]
pub struct ShrinkBase {
    empty: bool,
    nodes: Vec<(String, String)>,
    saved: Vec<Value>,
}
impl ShrinkBase {
    /// `saved` keeps, in stored order, every semantic node and every node a
    /// semantic id can be renamed onto (its stem rewrite, a `_doc` twin), so
    /// [`build::lost_saved_semantic_nodes`] reads exactly what it would read
    /// from the whole stored graph.
    pub fn of(old: &Value) -> Self {
        let stored: &[Value] = old["nodes"].as_array().map_or(&[], Vec::as_slice);
        let semantic = stored
            .iter()
            .filter(|n| n.is_object() && !build::is_ast_tier(n))
            .cloned()
            .collect::<Vec<_>>();
        let mut targets = semantic
            .iter()
            .map(|n| text(n, "id"))
            .collect::<BTreeSet<_>>();
        targets.extend(build::semantic_id_remap(&semantic, None).into_values());
        ShrinkBase {
            empty: old.as_object().is_none_or(|o| o.is_empty()),
            nodes: stored
                .iter()
                .map(|n| (n["id"].to_string(), text(n, "source_file")))
                .collect(),
            saved: stored
                .iter()
                .filter(|n| {
                    n.is_object()
                        && (!build::is_ast_tier(n) || {
                            let id = text(n, "id");
                            id.ends_with("_doc") || targets.contains(&id)
                        })
                })
                .cloned()
                .collect(),
        }
    }
}
/// `_check_shrink`: whether a candidate graph may replace the stored one.
/// A saved semantic node missing from the candidate refuses the write even
/// when the candidate is larger (0.9.87, #2229), unless its file is one of
/// `deleted` (the removed files, not the rebuilt ones). A net shrink is
/// allowed when every lost node belongs to a rebuilt file that did not fail.
#[allow(clippy::too_many_arguments)]
pub fn check_shrink_with(
    force: bool,
    old: &ShrinkBase,
    new: &Value,
    deletions: bool,
    rebuilt: Option<&BTreeSet<String>>,
    failed: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
    messages: &mut Vec<String>,
) -> bool {
    let after = items_ref(new, "nodes");
    shrink_allowed(
        force,
        old,
        ShrinkCandidate {
            nodes: after.len(),
            lost: &|deleted| {
                // Only a saved semantic node can be lost: without one, the
                // check (which indexes every new node) has nothing to find.
                if has_saved_semantic(old) {
                    build::lost_saved_semantic_nodes(&old.saved, after, deleted)
                } else {
                    vec![]
                }
            },
            ids: &|| after.iter().map(|n| n["id"].to_string()).collect(),
        },
        deletions,
        rebuilt,
        failed,
        deleted,
        messages,
    )
}
/// Whether the stored graph had a saved semantic node, the only kind the
/// shrink check can find lost.
fn has_saved_semantic(old: &ShrinkBase) -> bool {
    old.saved
        .iter()
        .any(|n| n.is_object() && !build::is_ast_tier(n))
}
/// What [`shrink_allowed`] reads of a candidate graph: its node count, the
/// saved semantic nodes it lost (given the deleted sources), and every
/// node's `id` as JSON text.
struct ShrinkCandidate<'a> {
    nodes: usize,
    lost: &'a dyn Fn(&[String]) -> Vec<Value>,
    ids: &'a dyn Fn() -> BTreeSet<String>,
}
/// [`check_shrink_with`] over what it reads of the candidate.
#[allow(clippy::too_many_arguments)]
fn shrink_allowed(
    force: bool,
    old: &ShrinkBase,
    new: ShrinkCandidate<'_>,
    deletions: bool,
    rebuilt: Option<&BTreeSet<String>>,
    failed: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
    messages: &mut Vec<String>,
) -> bool {
    if force || old.empty || deletions && rebuilt.is_none() {
        return true;
    }
    let deleted = deleted.iter().cloned().collect::<Vec<_>>();
    let missing = (new.lost)(&deleted);
    if !missing.is_empty() {
        let ids = missing
            .iter()
            .map(|n| crate::python_compat::string(n.get("id").unwrap_or(&Value::Null)))
            .collect::<Vec<_>>()
            .join(", ");
        messages.push(format!(
            "[graphicle] WARNING: Refusing to overwrite — saved semantic node(s) are missing from the new graph: {ids}."
        ));
        if new.nodes < old.nodes.len() {
            messages.push(shrink_warning(new.nodes, old.nodes.len()));
        }
        return false;
    }
    if new.nodes >= old.nodes.len() {
        return true;
    }
    if let Some(rebuilt) = rebuilt {
        let ids = (new.ids)();
        if old
            .nodes
            .iter()
            .filter(|(id, _)| !ids.contains(id))
            .all(|(_, sf)| {
                let normalized = norm(sf, None);
                (sf.is_empty() || !failed.contains(&normalized))
                    && (sf.is_empty() || rebuilt.contains(sf) || rebuilt.contains(&normalized))
            })
        {
            return true;
        }
    }
    messages.push(shrink_warning(new.nodes, old.nodes.len()));
    false
}
fn shrink_warning(new: usize, old: usize) -> String {
    format!(
        "[graphicle] WARNING: new graph has {new} nodes but existing graph.json has {old}. Refusing to overwrite — you may be missing chunk files from a previous session. Pass --force to override."
    )
}
/// The error a refused write returns: the guard's warnings.
fn refused(messages: &[String]) -> crate::Error {
    let text = messages.join("\n");
    if text.is_empty() {
        "Refusing to overwrite a smaller graph with unaccounted lost sources; pass --force to override.".into()
    } else {
        text.into()
    }
}
/// What a rebuild reads from the stored graph once the new graph is built.
struct StoredGraph {
    directed: bool,
    /// [`canonical_graph_digest`] with and without `topology`.
    topology: [u8; 32],
    full: [u8; 32],
    communities: BTreeMap<String, i64>,
    shrink: ShrinkBase,
}
impl StoredGraph {
    fn of(existing: &Value) -> Self {
        StoredGraph {
            directed: existing["directed"].as_bool().unwrap_or(false),
            topology: canonical_graph_digest(existing, true),
            full: canonical_graph_digest(existing, false),
            communities: node_communities(existing),
            shrink: ShrinkBase::of(existing),
        }
    }
}
pub fn report_for_compare(report: &str) -> String {
    regex::Regex::new(r"(?m)^- Built from commit: `[^`]+`\n?")
        .unwrap()
        .replace_all(report, "")
        .into_owned()
}
/// [`reconcile_html`] of the graph stored at `graph_path`, read only when the
/// HTML actually has to be rendered (it is stale or missing).
fn reconcile_stored_html(
    out: &Path,
    graph_path: &Path,
    messages: &mut Vec<String>,
) -> Option<String> {
    let limit = crate::exporters::html::viz_node_limit();
    let current = out.join("graph.html").exists()
        && !out.join(crate::exporters::html::HTML_STALE_MARKER).exists();
    if limit > 0 && current {
        return None;
    }
    let data = if limit > 0 {
        read_graph(graph_path).ok()?
    } else {
        json!({})
    };
    reconcile_html_owned(out, data, messages)
}
pub fn reconcile_html(out: &Path, data: &Value, messages: &mut Vec<String>) -> Option<String> {
    reconcile_html_with(out, HtmlSource::Borrowed(data), messages)
}
/// [`reconcile_html`] of a graph document the caller no longer needs: its
/// records move into the rendered graph instead of being copied.
pub fn reconcile_html_owned(out: &Path, data: Value, messages: &mut Vec<String>) -> Option<String> {
    reconcile_html_with(out, HtmlSource::Owned(data), messages)
}
enum HtmlSource<'a> {
    Borrowed(&'a Value),
    Owned(Value),
    /// The graph stored at `path`, whose nodes' facts were already read: it
    /// is loaded from the file only to render.
    Stored {
        path: &'a Path,
        nodes: Vec<NodeFacts>,
        edges_key: &'static str,
    },
}
fn reconcile_html_with(
    out: &Path,
    source: HtmlSource<'_>,
    messages: &mut Vec<String>,
) -> Option<String> {
    let target = out.join("graph.html");
    let marker = out.join(crate::exporters::html::HTML_STALE_MARKER);
    let limit = crate::exporters::html::viz_node_limit();
    if limit <= 0 {
        let changed = target.exists() || marker.exists();
        let _ = fs::remove_file(target);
        let _ = fs::remove_file(marker);
        return changed.then(|| "removed".into());
    }
    if target.exists() && !marker.exists() {
        return None;
    }
    let (communities, labels) = match &source {
        HtmlSource::Borrowed(data) => html_groups(items_ref(data, "nodes"), record_view),
        HtmlSource::Owned(data) => html_groups(items_ref(data, "nodes"), record_view),
        HtmlSource::Stored { nodes, .. } => html_groups(nodes, NodeFacts::view),
    };
    let render = || -> Result<bool> {
        let g = match source {
            HtmlSource::Borrowed(data) => paths::load_node_link_graph(data)?,
            HtmlSource::Owned(data) => paths::load_node_link_graph_owned(data)?,
            HtmlSource::Stored {
                path,
                nodes,
                edges_key,
            } => {
                let aggregated = nodes.len() as i64 > limit;
                drop(nodes);
                load_stored_graph(path, edges_key, aggregated.then_some(limit))?
            }
        };
        let mut d = build::Diagnostics::default();
        crate::exporters::html::to_html(
            &g,
            &communities,
            &target,
            &crate::exporters::html::HtmlOptions {
                community_labels: labels,
                node_limit: Some(limit),
                ..Default::default()
            },
            &mut d,
        )
    };
    match render() {
        Ok(true) if target.exists() => {
            let _ = fs::remove_file(marker);
            Some("rendered".into())
        }
        Ok(_) => None,
        Err(e) => {
            messages.push(format!("graph.html regeneration will be retried: {e}"));
            None
        }
    }
}
/// The communities (each with its member ids) and the community labels the
/// HTML renders.
type HtmlGroups = (Vec<(i64, Vec<Value>)>, BTreeMap<i64, Value>);
/// A node record's `id`, `community` and `community_name`, as
/// [`html_groups`] reads them.
fn record_view(node: &Value) -> [&Value; 3] {
    [&node["id"], &node["community"], &node["community_name"]]
}
/// The communities and labels [`reconcile_html`] renders, from each node's
/// `id`, `community` and `community_name` (`view`). Upstream groups the
/// members by iterating its node -> community dict, so communities (and the
/// aggregated view's community nodes) come in the order of their first
/// member in graph.json, each node id once.
fn html_groups<T>(nodes: &[T], view: impl Fn(&T) -> [&Value; 3]) -> HtmlGroups {
    let cids = communities_by_id(nodes.iter().map(|n| {
        let [id, community, _] = view(n);
        (id, community)
    }));
    let mut communities: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut slots = std::collections::HashMap::<i64, usize>::new();
    let mut members_seen = std::collections::HashSet::new();
    let mut labels = BTreeMap::new();
    for n in nodes {
        let [node_id, _, name] = view(n);
        let id = node_id.as_str().unwrap_or("").to_string();
        if let Some(cid) = cids.get(&id) {
            if members_seen.insert(id) {
                let slot = *slots.entry(*cid).or_insert_with(|| {
                    communities.push((*cid, Vec::new()));
                    communities.len() - 1
                });
                communities[slot].1.push(node_id.clone());
            }
            if name.as_str().is_some_and(|s| !s.is_empty()) {
                labels.entry(*cid).or_insert(name.clone());
            }
        }
    }
    (communities, labels)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RebuildOptions {
    pub changed_paths: Option<Vec<PathBuf>>,
    pub follow_symlinks: bool,
    pub force: bool,
    pub no_cluster: bool,
    pub acquire_lock: bool,
    pub block_on_lock: bool,
    pub out: Option<PathBuf>,
}
impl Default for RebuildOptions {
    fn default() -> Self {
        Self {
            changed_paths: None,
            follow_symlinks: false,
            force: false,
            no_cluster: false,
            acquire_lock: true,
            block_on_lock: false,
            out: None,
        }
    }
}
#[derive(Default, Serialize)]
pub struct RebuildResult {
    pub success: bool,
    pub changed: bool,
    pub messages: Vec<String>,
    pub nodes: usize,
    pub edges: usize,
    /// The rebuild already wrote the new graph's query snapshot.
    #[serde(skip)]
    pub prewarmed: bool,
}
pub fn rebuild(path: &Path, options: &RebuildOptions) -> Result<RebuildResult> {
    let result = rebuild_unwarmed(path, options)?;
    if result.success && result.changed && !result.prewarmed {
        let out = options
            .out
            .clone()
            .unwrap_or_else(|| path.join(paths::OUT_DIR.as_str()));
        crate::cli::prewarm_query_snapshot(&out.join("graph.json"));
    }
    Ok(result)
}
fn rebuild_unwarmed(path: &Path, options: &RebuildOptions) -> Result<RebuildResult> {
    let out = options
        .out
        .clone()
        .unwrap_or_else(|| path.join(paths::OUT_DIR.as_str()));
    let mut opts = options.clone();
    if options.acquire_lock {
        // #1059: a request must not be dropped when another rebuild is
        // running. Queue before trying the lock so a lost race still records
        // the work: incremental requests append their paths to
        // `.pending_changes`, full requests leave `.pending_full` (#4274).
        let mut queued = false;
        if !options.block_on_lock {
            match &options.changed_paths {
                None => {
                    queue_pending_full(&out)?;
                    queued = true;
                }
                Some(changed) if !changed.is_empty() => {
                    queue_pending(&out, changed)?;
                    queued = true;
                }
                Some(_) => {}
            }
        }
        let Some(_lock) = RebuildLock::acquire(&out, options.block_on_lock)? else {
            return Ok(RebuildResult {
                messages: vec![format!(
                    "Rebuild already in progress; {}.",
                    if queued {
                        "changes queued"
                    } else {
                        "nothing to queue"
                    }
                )],
                ..Default::default()
            });
        };
        // Drain what earlier contenders queued (and what was just queued
        // here). A queued full request upgrades this run to a full rebuild,
        // which supersedes the incremental work queued before it starts.
        let full = drain_pending_full(&out) || options.changed_paths.is_none();
        let pending = drain_pending(&out);
        opts.changed_paths = if full {
            None
        } else {
            options
                .changed_paths
                .as_ref()
                .map(|a| merge_changed(&[a.clone(), pending]))
        };
        opts.acquire_lock = false;
        let mut result = rebuild_inner(path, &out, &opts)?;
        // Late arrivals: even a full rebuild only saw files as they were when
        // it walked the corpus. Bounded so a storm of commits quiesces.
        for _ in 0..20 {
            let late_full = drain_pending_full(&out);
            let late = drain_pending(&out);
            if !late_full && late.is_empty() {
                break;
            }
            opts.changed_paths = (!late_full).then_some(late);
            let r = rebuild_inner(path, &out, &opts)?;
            result.success &= r.success;
            result.changed |= r.changed;
            result.nodes = r.nodes;
            result.edges = r.edges;
            result.messages.extend(r.messages);
        }
        return Ok(result);
    }
    rebuild_inner(path, &out, &opts)
}
/// `.graphicle_labels.json` as a rebuild writes it: upstream's
/// `json.dumps({str(k): v for k, v in sorted(labels.items())},
/// ensure_ascii=False, indent=2) + "\n"`, so entries are ordered by community
/// id (reused labels and hub-filled ones interleave) and the file ends in a
/// newline.
/// The sidecar beside `.graphicle_labels.json` that holds hub-fallback
/// community names (#3334).
pub const PENDING_LABELS_FILE: &str = ".graphicle_labels.pending.json";
/// `int(k)` for a labels-file key, the way Python reads one.
fn label_cid(key: &str) -> Option<i64> {
    crate::llm_config::parse_integer(key)
}
/// The labels a rebuild uses (curated, pending and hub names merged) and the
/// community ids whose name belongs in the pending sidecar (upstream
/// `_rebuild_code`, 0.9.91): tracked names first, then pending ones for
/// communities without a tracked name; a name whose community changed is
/// dropped unless the shared members are a strict majority of both the saved
/// and the current set (#2298); every community left without a name gets its
/// hub, which is pending.
fn rebuild_labels(
    graph: &crate::graph::Graph,
    communities: &cluster::Communities,
    previous: &BTreeMap<String, i64>,
    cur_sigs: &Value,
    label_file: &Path,
    sig_file: &Path,
    pending_file: &Path,
) -> (Value, BTreeSet<i64>) {
    let in_communities = |cid: i64| communities.iter().any(|(c, _)| *c == cid);
    let placeholder = |cid: i64, v: &Value| v.as_str() == Some(format!("Community {cid}").as_str());
    let read = |path: &Path| {
        fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    };
    // Tracked names; a key that is not an integer discards the whole file,
    // as upstream's comprehension does.
    let raw = read(label_file).unwrap_or(json!({}));
    let mut labels: BTreeMap<i64, Value> = BTreeMap::new();
    let mut raw_len = 0;
    if let Some(map) = raw.as_object() {
        let parsed: Option<Vec<(i64, &Value)>> = map
            .iter()
            .map(|(k, v)| label_cid(k).map(|c| (c, v)))
            .collect();
        if let Some(parsed) = parsed {
            raw_len = map.len();
            for (cid, v) in parsed {
                if in_communities(cid) && v.is_string() && !placeholder(cid, v) {
                    labels.insert(cid, v.clone());
                }
            }
        }
    }
    // Pending names, for communities without a tracked one. A bad key stops
    // the scan, keeping the names read before it.
    let mut pending_cids = BTreeSet::new();
    if let Some(map) = read(pending_file).as_ref().and_then(Value::as_object) {
        for (k, v) in map {
            let Some(cid) = label_cid(k) else {
                break;
            };
            if in_communities(cid)
                && !labels.contains_key(&cid)
                && v.is_string()
                && !placeholder(cid, v)
            {
                labels.insert(cid, v.clone());
                pending_cids.insert(cid);
            }
        }
    }
    // Saved membership signatures: string values only, and none at all when
    // a key is not an integer.
    let saved_sigs: BTreeMap<i64, String> = read(sig_file)
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|map| {
            map.iter()
                .map(|(k, v)| Ok::<_, ()>((label_cid(k).ok_or(())?, v)))
                .collect::<std::result::Result<Vec<_>, ()>>()
                .ok()
        })
        .map(|pairs| {
            pairs
                .into_iter()
                .filter_map(|(c, v)| Some((c, v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let members = |cid: i64| -> &[Value] {
        communities
            .iter()
            .find(|(c, _)| *c == cid)
            .map(|(_, m)| m.as_slice())
            .unwrap_or(&[])
    };
    let stale: Vec<i64> = if !saved_sigs.is_empty() {
        labels
            .keys()
            .copied()
            .filter(|cid| {
                !cluster::label_still_applies(
                    *cid,
                    members(*cid),
                    previous,
                    saved_sigs.get(cid).map(String::as_str),
                    cur_sigs[cid.to_string()].as_str(),
                )
            })
            .collect()
    } else if raw_len != communities.len() {
        labels
            .keys()
            .copied()
            .filter(|cid| !cluster::label_still_applies(*cid, members(*cid), previous, None, None))
            .collect()
    } else {
        vec![]
    };
    for cid in &stale {
        labels.remove(cid);
        pending_cids.remove(cid);
    }
    let missing: cluster::Communities = communities
        .iter()
        .filter(|(c, _)| !labels.contains_key(c))
        .cloned()
        .collect();
    if let Some(hubs) = cluster::label_communities_by_hub(graph, &missing).as_object() {
        for (k, v) in hubs {
            if let Some(cid) = label_cid(k) {
                labels.insert(cid, v.clone());
                pending_cids.insert(cid);
            }
        }
    }
    let labels = Value::Object(
        labels
            .into_iter()
            .map(|(cid, v)| (cid.to_string(), v))
            .collect(),
    );
    (labels, pending_cids)
}
/// `labels` split into the tracked (curated) names and the pending ones.
fn split_labels(labels: &Value, pending_cids: &BTreeSet<i64>) -> (Value, Value) {
    let (mut curated, mut pending) = (serde_json::Map::new(), serde_json::Map::new());
    for (k, v) in labels.as_object().into_iter().flatten() {
        if label_cid(k).is_some_and(|cid| pending_cids.contains(&cid)) {
            pending.insert(k.clone(), v.clone());
        } else {
            curated.insert(k.clone(), v.clone());
        }
    }
    (Value::Object(curated), Value::Object(pending))
}
fn labels_text(labels: &Value) -> Result<String> {
    let mut entries: Vec<(&String, &Value)> = labels
        .as_object()
        .map(|m| m.iter().collect())
        .unwrap_or_default();
    entries.sort_by_key(|(k, _)| k.parse::<i64>().ok());
    let sorted: serde_json::Map<String, Value> = entries
        .into_iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut text = Vec::new();
    crate::python_json::write(&mut text, &Value::Object(sorted), Some(2), false)
        .map_err(crate::Error::from)?;
    text.push(b'\n');
    String::from_utf8(text).map_err(|e| e.to_string().into())
}
fn read_graph(path: &Path) -> Result<Value> {
    security::check_graph_file_size_cap(path)?;
    let b = fs::read(path).map_err(crate::Error::from)?;
    let v = serde_json::from_slice::<Value>(&b).map_err(|e| {
        format!(
            "Cannot read {}: {e}. Refusing to overwrite; repair the graph and retry.",
            path.display()
        )
    })?;
    if !v.is_object() || !v["nodes"].is_array() {
        return Err(format!("Invalid graph at {}; refusing to overwrite", path.display()).into());
    }
    Ok(v)
}
/// One node of a graph a rebuild wrote, as the shrink check and the HTML
/// read it: its `id`, `community` and `community_name` (`Null` where the
/// record has none, as indexing it reads them).
struct NodeFacts {
    id: Value,
    community: Value,
    community_name: Value,
}
impl NodeFacts {
    fn of(node: &Value) -> Self {
        NodeFacts {
            id: node["id"].clone(),
            community: node["community"].clone(),
            community_name: node["community_name"].clone(),
        }
    }
    fn view(&self) -> [&Value; 3] {
        [&self.id, &self.community, &self.community_name]
    }
}
/// The graph.json a rebuild wrote ([`read_candidate`]), as the rest of the
/// rebuild reads it.
enum Candidate {
    /// Read one record at a time: its [`canonical_graph_digest`] without
    /// topology, its nodes' [`NodeFacts`] and its edge field. The document
    /// (about as large as the graph) is never built.
    Streamed {
        full: [u8; 32],
        nodes: Vec<NodeFacts>,
        edges_key: &'static str,
    },
    /// The whole document, as [`read_graph`] reads it.
    Whole(Value),
}
impl Candidate {
    fn full_digest(&self) -> [u8; 32] {
        match self {
            Candidate::Streamed { full, .. } => *full,
            Candidate::Whole(data) => canonical_graph_digest(data, false),
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn check_shrink(
        &self,
        force: bool,
        old: &ShrinkBase,
        deletions: bool,
        rebuilt: Option<&BTreeSet<String>>,
        failed: &BTreeSet<String>,
        deleted: &BTreeSet<String>,
        messages: &mut Vec<String>,
    ) -> bool {
        match self {
            Candidate::Whole(data) => check_shrink_with(
                force, old, data, deletions, rebuilt, failed, deleted, messages,
            ),
            // Streamed only when the stored graph has no saved semantic node
            // (`read_candidate`), so none can be lost.
            Candidate::Streamed { nodes, .. } => shrink_allowed(
                force,
                old,
                ShrinkCandidate {
                    nodes: nodes.len(),
                    lost: &|_| vec![],
                    ids: &|| nodes.iter().map(|n| n.id.to_string()).collect(),
                },
                deletions,
                rebuilt,
                failed,
                deleted,
                messages,
            ),
        }
    }
}
/// The candidate graph at `path`: [`read_graph`], streamed record by record
/// unless `whole` (the shrink check then reads whole nodes) or the document
/// is not one the stream reader takes ([`stream_candidate`]).
fn read_candidate(path: &Path, whole: bool) -> Result<Candidate> {
    if !whole {
        security::check_graph_file_size_cap(path)?;
        let bytes = fs::read(path).map_err(crate::Error::from)?;
        if let Some(candidate) = stream_candidate(&bytes) {
            return Ok(candidate);
        }
    }
    read_graph(path).map(Candidate::Whole)
}
/// [`Candidate::Streamed`] of a graph document's text; `None` where it is
/// not a JSON object, has a repeated top-level key, a `nodes`, `links`,
/// `edges` or `hyperedges` field that is not an array, or no `nodes`.
fn stream_candidate(bytes: &[u8]) -> Option<Candidate> {
    use serde::de::Deserializer as _;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let document = (&mut deserializer)
        .deserialize_map(CandidateVisitor {
            workers: crate::par::workers(None),
        })
        .ok()?;
    deserializer.end().ok()?;
    let nodes = document.nodes?;
    let has = |key: &str| document.entries.iter().any(|(k, _)| k == key);
    let edges_key = if !has("links") && has("edges") {
        "edges"
    } else {
        "links"
    };
    // `canonical_graph_digest(document, false)`: `built_at_commit` left out,
    // `directed` always present as a boolean.
    let directed = json!(
        document
            .entries
            .iter()
            .find(|(k, _)| k == "directed")
            .and_then(|(_, entry)| match entry {
                StreamEntry::Value(value) => value.as_bool(),
                StreamEntry::Records(_) => None,
            })
            .unwrap_or(false)
    );
    // The keys are distinct (a repeated one is declined above), so the map
    // holds every entry, in the sorted order the digest reads them.
    let mut entries = document
        .entries
        .into_iter()
        .filter(|(k, _)| k != "built_at_commit")
        .collect::<BTreeMap<_, _>>();
    entries.insert("directed".into(), StreamEntry::Value(directed));
    let full = digest_entries(entries.iter_mut().map(|(k, entry)| {
        (
            k.as_str(),
            match entry {
                StreamEntry::Records(digests) => DigestEntry::Records(std::mem::take(digests)),
                StreamEntry::Value(value) => DigestEntry::Value(&*value),
            },
        )
    }));
    Some(Candidate::Streamed {
        full,
        nodes,
        edges_key,
    })
}
/// A top-level field of a streamed document: a sorted array's record
/// digests, or the value itself.
enum StreamEntry {
    Records(Vec<[u8; 32]>),
    Value(Value),
}
struct StreamedDocument {
    entries: Vec<(String, StreamEntry)>,
    nodes: Option<Vec<NodeFacts>>,
}
struct CandidateVisitor {
    workers: usize,
}
impl<'de> serde::de::Visitor<'de> for CandidateVisitor {
    type Value = StreamedDocument;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a graph object")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<StreamedDocument, A::Error> {
        let mut document = StreamedDocument {
            entries: vec![],
            nodes: None,
        };
        while let Some(key) = map.next_key::<String>()? {
            if document.entries.iter().any(|(k, _)| *k == key) {
                return Err(serde::de::Error::custom("repeated key"));
            }
            let entry = if SORTED_ARRAYS.contains(&key.as_str()) {
                let mut facts = (key == "nodes").then(Vec::new);
                let digests = map.next_value_seed(RecordDigests {
                    workers: self.workers,
                    facts: facts.as_mut(),
                })?;
                if facts.is_some() {
                    document.nodes = facts;
                }
                StreamEntry::Records(digests)
            } else {
                StreamEntry::Value(map.next_value()?)
            };
            document.entries.push((key, entry));
        }
        Ok(document)
    }
}
/// The canonical digests of an array's records ([`record_digests`]), made a
/// batch at a time on the run's workers; with `facts`, each record's
/// [`NodeFacts`] too.
struct RecordDigests<'a> {
    workers: usize,
    facts: Option<&'a mut Vec<NodeFacts>>,
}
impl<'de> serde::de::DeserializeSeed<'de> for RecordDigests<'_> {
    type Value = Vec<[u8; 32]>;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}
impl<'de> serde::de::Visitor<'de> for RecordDigests<'_> {
    type Value = Vec<[u8; 32]>;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an array of records")
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        mut self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        const BATCH: usize = 8192;
        let mut digests = Vec::new();
        let mut batch = Vec::with_capacity(BATCH);
        loop {
            let item = seq.next_element::<Value>()?;
            let done = item.is_none();
            if let Some(item) = item {
                if let Some(facts) = self.facts.as_mut() {
                    facts.push(NodeFacts::of(&item));
                }
                batch.push(item);
            }
            if batch.len() == BATCH || done {
                digests.extend(record_digests(&batch, self.workers, None));
                batch.clear();
            }
            if done {
                return Ok(digests);
            }
        }
    }
}
/// The graph stored at `path` as [`paths::load_node_link_graph_owned`]
/// loads its parsed document, streamed from the text where the streaming
/// loader takes it. With `aggregate_above`, a graph with more nodes than
/// that is loaded as its topology alone: the HTML then renders the
/// aggregated community view, which reads no node or edge attribute.
fn load_stored_graph(
    path: &Path,
    edges_key: &str,
    aggregate_above: Option<i64>,
) -> Result<crate::graph::Graph> {
    let text = fs::read_to_string(path).map_err(crate::Error::from)?;
    if let Some(limit) = aggregate_above
        && let Some(graph) = crate::graph::Graph::from_node_link_json_topology(&text, edges_key)
        && graph.nodes.len() as i64 > limit
    {
        return Ok(graph);
    }
    if let Some(graph) = crate::graph::Graph::from_node_link_json_with_arcs(&text, edges_key) {
        return Ok(graph);
    }
    let data = serde_json::from_str::<Value>(&text).map_err(crate::Error::from)?;
    drop(text);
    paths::load_node_link_graph_owned(data)
}
fn stamp_manifest(
    out: &Path,
    detected: &Value,
    root: &Path,
    failed: &BTreeSet<String>,
) -> Result<()> {
    let mut files = detected["files"].as_object().cloned().unwrap_or_default();
    let scan = files
        .values()
        .flat_map(|v| v.as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let failed_abs = failed
        .iter()
        .map(|s| resolved(Path::new(s)))
        .collect::<BTreeSet<_>>();
    for list in files.values_mut() {
        if let Some(a) = list.as_array_mut() {
            a.retain(|f| {
                f.as_str().is_none_or(|s| {
                    !failed.contains(s) && !failed_abs.contains(&resolved(Path::new(s)))
                })
            });
        }
    }
    detect::save_manifest(
        &files,
        &out.join("manifest.json"),
        "ast",
        &detect::ManifestOptions {
            root: Some(root.into()),
            scan_corpus: Some(scan),
            clear_ast: Some(failed.iter().cloned().collect()),
            ..Default::default()
        },
    )
}
fn rebuild_inner(path: &Path, out: &Path, o: &RebuildOptions) -> Result<RebuildResult> {
    let watch_root = resolved(path);
    let project_root = if path.is_absolute() {
        watch_root.clone()
    } else {
        resolved(Path::new("."))
    };
    let mut result = RebuildResult::default();
    let mut timer = PhaseTimer::new();
    let excludes = build_excludes(out);
    let gitignore = build_gitignore(out);
    if build_docs(out) {
        crate::extractors::rst::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if build_flag(out, "routes") {
        crate::extractors::routes::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let mut detector = detect::Detector::default();
    let graph_path = out.join("graph.json");
    // The stored graph is parsed, and the ignore rules reconcile consults are
    // loaded (which may run `git ls-files`), while the corpus is detected:
    // none reads the others. A detection error still takes precedence, and an
    // error loading the ignore rules still surfaces after extraction.
    let (detected, existing, mut ignore_states) = std::thread::scope(|scope| {
        let loading = scope.spawn(|| {
            if graph_path.exists() {
                read_graph(&graph_path)
            } else {
                Ok(json!({}))
            }
        });
        let ignoring = scope.spawn(|| {
            let mut warning = detect::EncodingWarnings::default();
            let always = detect::IgnoreState::new(&watch_root, &excludes, false, &mut warning)?;
            let all = detect::IgnoreState::new(&watch_root, &excludes, gitignore, &mut warning)?;
            Ok::<_, crate::Error>((always, all, warning))
        });
        let detected = detector.detect(
            path,
            &detect::DetectOptions {
                follow_symlinks: Some(o.follow_symlinks),
                extra_excludes: excludes.clone(),
                gitignore,
                ..Default::default()
            },
        );
        fn join<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        }
        let existing = join(loading);
        (detected, existing, join(ignoring))
    });
    let detected = detected?;
    // The equality skips below compare parsed data, not layout: a multi-line
    // graph.json is still rewritten in the compact layout (0.9.86, #3413).
    let legacy_layout = graph_path.exists() && !paths::is_single_line(&graph_path).unwrap_or(false);
    let mut code = items(&detected["files"], "code")
        .iter()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let docs = items(&detected["files"], "document")
        .iter()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .filter(|p| extract::get_extractor(p).is_some())
        .collect::<Vec<_>>();
    code.extend(docs.clone());
    timer.mark("detect");
    let existing = existing?;
    timer.mark("load_existing");
    if code.is_empty() && !graph_path.exists() {
        result
            .messages
            .push("No code files found - nothing to rebuild.".into());
        return Ok(result);
    }
    let sp = StoredSourcePaths::new(&existing, out, &project_root, &watch_root);
    let semantic = items_ref(&existing, "nodes")
        .iter()
        .filter(|n| {
            !build::is_ast_tier(n)
                && ["document", "concept", "rationale", "paper", "code"]
                    .contains(&text(n, "file_type").as_str())
        })
        .map(|n| sp.identity(&text(n, "source_file")))
        .collect::<BTreeSet<_>>();
    let semantic_docs = docs
        .iter()
        .filter(|p| semantic.contains(&sp.absolute_identity(&p.to_string_lossy(), &project_root)))
        .map(|p| crate::detect_ignore::absolute_lexical(p))
        .collect::<BTreeSet<_>>();
    // `--routes`: a changed file that mounts routers, or one that shaped a
    // stored route's path from another file, can move routes declared in
    // unchanged files; such a change set rebuilds every file, as `update`
    // does (see `extractors::routes`).
    let changed_paths = o.changed_paths.as_ref().filter(|changed| {
        if !crate::extractors::routes::enabled() || !graph_path.exists() {
            return true;
        }
        let cwd = resolved(Path::new("."));
        let files = changed
            .iter()
            .flat_map(|raw| changed_candidates(raw, &cwd, &watch_root))
            .collect::<Vec<_>>();
        let moved =
            crate::extractors::routes::stored_mounts(&existing, |s| PathBuf::from(sp.identity(s)))
                .moved_by(&files);
        if moved {
            result
                .messages
                .push("Route mounts changed - rebuilding every file.".into());
        }
        !moved
    });
    let full = changed_paths.is_none();
    let mut deleted = BTreeSet::new();
    let mut deleted_ids = BTreeSet::new();
    let targets = if let Some(changed) = changed_paths {
        let code_set = code
            .iter()
            .map(|p| crate::detect_ignore::absolute_lexical(p))
            .collect::<BTreeSet<_>>();
        let mut wanted = vec![];
        for raw in changed {
            let cs = changed_candidates(raw, &resolved(Path::new(".")), &watch_root);
            if let Some(p) = cs.iter().find(|p| p.exists() && code_set.contains(*p)) {
                if !wanted.contains(p) && !semantic_docs.contains(p) {
                    wanted.push(p.clone());
                }
                continue;
            }
            if cs.iter().any(|p| p.exists() && p.starts_with(&watch_root)) {
                continue;
            }
            if let Some(p) = cs.iter().find(|p| p.starts_with(&watch_root)) {
                deleted_ids.insert(
                    crate::detect_ignore::absolute_lexical(p)
                        .to_string_lossy()
                        .into_owned(),
                );
                for root in [&project_root, &watch_root] {
                    deleted.insert(norm(&p.to_string_lossy(), Some(root)));
                }
            }
        }
        let wanted = crate::extractors::terraform::refresh_terraform_paths(&wanted, &code, changed);
        if wanted.is_empty() && deleted.is_empty() {
            result.success = true;
            result
                .messages
                .push("No tracked code files in change set - skipping rebuild.".into());
            return Ok(result);
        }
        wanted
    } else {
        code.iter()
            .filter(|p| !semantic_docs.contains(&crate::detect_ignore::absolute_lexical(p)))
            .cloned()
            .collect()
    };
    let mut context_nodes = vec![];
    let mut context_edges = vec![];
    if !full && graph_path.exists() {
        let live = code
            .iter()
            .map(|p| sp.absolute_identity(&p.to_string_lossy(), &project_root))
            .filter(|s| {
                !deleted_ids.contains(s)
                    && !targets
                        .iter()
                        .any(|p| sp.absolute_identity(&p.to_string_lossy(), &project_root) == *s)
            })
            .collect::<BTreeSet<_>>();
        for n in items_ref(&existing, "nodes") {
            if text(n, "id").is_empty()
                || !build::is_ast_tier(n)
                || !live.contains(&sp.identity(&text(n, "source_file")))
            {
                continue;
            }
            let mut c = json!({});
            for k in ["id", "label", "source_file", "file_type", "type"] {
                c[k] = n[k].clone();
            }
            // `--routes`: requests in re-extracted files reach stored routes.
            if text(n, "node_kind") == "route" {
                for k in ["node_kind", "route_method", "route_path"] {
                    c[k] = n[k].clone();
                }
            }
            for k in [
                "_callable",
                "_callable_class",
                "_elixir_module",
                "_rust_impl_key",
                "_rust_declaration_count",
                "_ts_public_twin",
                "_py_property",
                "_java_ret",
                "_ts_fields",
                "_ts_static_fields",
            ] {
                if build::truthy(&n[k]) {
                    c[k] = n[k].clone();
                }
            }
            let mut meta = json!({});
            for k in [
                "ruby_resolution_schema",
                "ruby_method_kind",
                "ruby_lookup_unsafe",
                "python_opaque_bases",
                "ruby_reopened",
                "ruby_external_method_owners",
                "language",
                "kind",
                "module",
                "name",
                "arity",
            ] {
                if let Some(v) = n["metadata"].get(k) {
                    meta[k] = v.clone();
                }
            }
            if !meta.as_object().unwrap().is_empty() {
                c["metadata"] = meta;
            }
            context_nodes.push(c);
        }
        for e in items_ref(
            &existing,
            if existing.get("links").is_some() {
                "links"
            } else {
                "edges"
            },
        ) {
            // Import edges come too (0.9.91): a TS field inherited from an
            // unchanged file is typed by that file's imports, as in a full build.
            if !["contains", "method", "inherits", "imports", "imports_from"]
                .contains(&text(e, "relation").as_str())
                || !build::is_ast_tier(e)
                || !live.contains(&sp.identity(&text(e, "source_file")))
            {
                continue;
            }
            let mut c = json!({});
            for k in ["source", "target", "relation", "source_file"] {
                c[k] = e[k].clone();
            }
            if e["metadata"]["ruby_superclass_ref"].is_string() {
                c["metadata"] = json!({"ruby_superclass_ref":e["metadata"]["ruby_superclass_ref"]});
                if e["metadata"]["ruby_lexical_scopes"]
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string))
                {
                    c["metadata"]["ruby_lexical_scopes"] =
                        e["metadata"]["ruby_lexical_scopes"].clone();
                }
            }
            context_edges.push(c);
        }
    }
    timer.mark("context");
    // Everything the rest of the rebuild reads from the stored graph is taken
    // now, before extraction: the comparison digests, communities and shrink
    // base once the new graph is built, and what reconcile keeps of it (its
    // eviction decisions and the records that may survive; see
    // `watch_reconcile::plan`). The stored document, about as large as the
    // new graph, is then released before the new graph is extracted.
    // `--no-cluster` compares the whole stored document below and keeps it.
    let (mut stored, plan) = std::thread::scope(|scope| {
        let stored = scope.spawn(|| StoredGraph::of(&existing));
        let plan = ignore_states.as_mut().ok().map(|(always, all, warning)| {
            crate::watch_reconcile::plan(
                &existing,
                &crate::watch_reconcile::ReconcileOptions {
                    out,
                    project_root: &project_root,
                    watch_root: &watch_root,
                    code_files: &code,
                    extract_targets: &targets,
                    full,
                    deleted_paths: &mut deleted,
                    deleted_identities: &deleted_ids,
                },
                |p, full| {
                    if full {
                        all.predicate(p, warning).unwrap_or(false)
                    } else {
                        always.predicate(p, warning).unwrap_or(false)
                    }
                },
            )
        });
        let stored = stored
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (stored, plan)
    });
    // Freed here and handed back to the system at once (`par::trim`): on a
    // small repository the whole rebuild is shorter than the allocator's
    // purge delay, and the extraction would otherwise grow on top of it.
    let existing = if o.no_cluster {
        existing
    } else {
        drop(existing);
        crate::par::trim();
        Value::Null
    };
    timer.mark("plan");
    let extraction = if targets.is_empty() {
        Ok(json!({"nodes":[],"edges":[],"hyperedges":[],"input_tokens":0,"output_tokens":0}))
    } else {
        extract::extract(
            &targets,
            &extract::ExtractOptions {
                cache_root: Some(watch_root.clone()),
                resolution_context_nodes: context_nodes,
                resolution_context_edges: context_edges,
                ..Default::default()
            },
        )
    };
    let mut extraction = extraction?;
    timer.mark("extract");
    rebase(&mut extraction, &watch_root, &project_root);
    let failed = items_ref(&extraction, "failed_sources")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    // An error loading the ignore rules surfaces here, after extraction.
    ignore_states?;
    if graph_path.exists()
        && let Some(plan) = plan
    {
        extraction = crate::watch_reconcile::finish(
            plan,
            extraction,
            &mut crate::watch_reconcile::ReconcileOptions {
                out,
                project_root: &project_root,
                watch_root: &watch_root,
                code_files: &code,
                extract_targets: &targets,
                full,
                deleted_paths: &mut deleted,
                deleted_identities: &deleted_ids,
            },
            &mut result.messages,
        )?;
    }
    timer.mark("reconcile");
    relativize(&mut extraction, &project_root, Some(&watch_root));
    let mut rebuilt = if full {
        code.iter()
            .filter_map(|p| p.strip_prefix(&project_root).ok())
            .map(|p| norm(&p.to_string_lossy(), Some(&project_root)))
            .collect::<BTreeSet<_>>()
    } else {
        targets
            .iter()
            .map(|p| norm(&p.to_string_lossy(), Some(&project_root)))
            .collect()
    };
    rebuilt.extend(deleted.iter().cloned());
    let normalized_failed = failed
        .iter()
        .map(|s| norm(s, Some(&project_root)))
        .collect::<BTreeSet<_>>();
    fs::create_dir_all(out).map_err(crate::Error::from)?;
    if o.no_cluster {
        // Normalised to "links" like the clustered path; parallel edges are
        // deduplicated (#1317). Run-only extraction keys are left out, and
        // hyperedges are always written so a first build and a reconciled
        // rebuild of one tree give the same file (0.9.80).
        let mut nodes = build::dedupe_nodes(items_ref(&extraction, "nodes"))?;
        build::disambiguate_file_labels_in_nodes(&mut nodes);
        let links = build::dedupe_edges(items_ref(&extraction, "edges"))?;
        let mut candidate = serde_json::Map::new();
        for (k, v) in extraction.as_object().into_iter().flatten() {
            if !matches!(k.as_str(), "edges" | "nodes")
                && !extract::RUN_ONLY_EXTRACTION_KEYS.contains(&k.as_str())
            {
                candidate.insert(k.clone(), v.clone());
            }
        }
        candidate.insert("nodes".into(), Value::Array(nodes));
        candidate.insert("links".into(), Value::Array(links));
        candidate.insert(
            "hyperedges".into(),
            Value::Array(items(&extraction, "hyperedges")),
        );
        // Inherit the stored graph's directed flag (#2342).
        candidate.insert(
            "directed".into(),
            json!(build::truthy(&existing["directed"])),
        );
        let mut candidate = Value::Object(candidate);
        // The raw merged extraction gets build_from_json's endpoint rules:
        // import stubs, normalization twins, no dangling edge (#4189).
        build::finalize_raw_graph_endpoints(&mut candidate);
        if legacy_layout
            || canonical_graph_key(&existing, false) != canonical_graph_key(&candidate, false)
        {
            let mut refusal = vec![];
            if !check_shrink(
                o.force,
                &existing,
                &candidate,
                !deleted.is_empty(),
                Some(&rebuilt),
                &normalized_failed,
                &deleted,
                &mut refusal,
            ) {
                return Err(refused(&refusal));
            }
            export::backup_if_protected(out, &mut build::Diagnostics::default());
            paths::write_text_atomic(
                &graph_path,
                &paths::compact_json_text(&candidate).map_err(crate::Error::from)?,
            )
            .map_err(crate::Error::from)?;
            result.changed = true;
        }
        fs::write(out.join(".graphicle_root"), root_marker_value(path)?)
            .map_err(crate::Error::from)?;
        stamp_manifest(out, &detected, &watch_root, &failed)?;
        result.success = true;
        result.nodes = items_ref(&candidate, "nodes").len();
        result.edges = items_ref(&candidate, "links").len();
        return Ok(result);
    }
    // The extraction's records move into the graph (nothing reads them
    // after the build), so the build holds one copy of them, not two.
    let graph = build::build_from_json_consuming(
        &mut extraction,
        stored.directed,
        None,
        &mut build::Diagnostics::default(),
    )?;
    // Nothing reads the extraction after the build; its records are handed
    // back before clustering allocates, as `extract` does.
    drop(extraction);
    crate::par::trim();
    timer.mark("build");
    // The digest is made from the graph's records one at a time, not from its
    // node-link document (`graph_topology_digest`).
    let topology_unchanged =
        graph_path.exists() && !legacy_layout && stored.topology == graph_topology_digest(&graph);
    timer.mark("topology_compare");
    if topology_unchanged {
        stamp_manifest(out, &detected, &watch_root, &failed)?;
        reconcile_stored_html(out, &graph_path, &mut result.messages);
        result.success = true;
        result.nodes = graph.nodes.len();
        result.edges = graph.edge_count();
        result
            .messages
            .push("No code-graph topology changes detected; outputs left untouched.".into());
        timer.mark("manifest_html");
        timer.report("rebuild(topology-unchanged)");
        return Ok(result);
    }
    let mut communities = cluster::cluster(&graph, 1.0, None, "auto")?;
    let previous = std::mem::take(&mut stored.communities);
    if !previous.is_empty() {
        let previous = previous.iter().map(|(k, v)| (json!(k), *v)).collect();
        communities = cluster::remap_communities_to_previous(&communities, &previous)?;
    }
    timer.mark("cluster");
    let cur_sigs = cluster::community_member_sigs(&communities);
    let label_file = out.join(".graphicle_labels.json");
    let sig_file = out.join(".graphicle_labels.json.sig");
    // Hub-fallback names live in their own sidecar (#3334), so the tracked
    // labels file only ever holds names someone set (manual or LLM).
    let pending_file = out.join(PENDING_LABELS_FILE);
    let (labels, pending_cids) = rebuild_labels(
        &graph,
        &communities,
        &previous,
        &cur_sigs,
        &label_file,
        &sig_file,
        &pending_file,
    );
    timer.mark("labels");
    let commit = export::git_head(Some(&watch_root));
    timer.mark("git_head");
    // The report's analyses only read the graph: they run side by side, and
    // their errors are raised in the order the sequential version met them.
    let (cohesion, gods, surprises, questions) = std::thread::scope(|scope| {
        let cohesion = scope.spawn(|| cluster::score_all(&graph, &communities));
        let gods = scope.spawn(|| crate::analyze::god_nodes(&graph, 10, None));
        let surprises = scope.spawn(|| {
            crate::analyze::surprising_connections(
                &graph,
                &communities,
                crate::analyze::SURPRISES_TOP_N,
            )
        });
        let questions = crate::analyze::suggest_questions(
            &graph,
            &communities,
            labels.as_object().unwrap(),
            crate::analyze::QUESTIONS_TOP_N,
        );
        fn join<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        }
        (join(cohesion), join(gods), join(surprises), questions)
    });
    let (gods, surprises, questions) = (gods?, surprises?, questions?);
    let report = crate::report::generate(
        &graph,
        &json!({"communities":cluster::communities_to_json(&communities),"cohesion_scores":cohesion,"community_labels":labels,"god_node_list":gods,"surprise_list":surprises,"detection_result":{"total_files":code.len(),"total_words":detected["total_words"].as_u64().unwrap_or(0),"files":{"code":code},"unclassified":detected.get("unclassified").cloned().unwrap_or_else(|| json!([]))},"token_cost":{"input":0,"output":0},"root":report_root_label(path),"suggested_questions":questions,"built_at_commit":commit,"learning":crate::report::load_learning_for_report(&graph_path)}),
    )?;
    timer.mark("report");
    let tmp = out.join(".graph.tmp.json");
    export::to_json(
        &graph,
        &communities,
        &tmp,
        &export::JsonOptions {
            force: true,
            built_at_commit: commit,
            community_labels: labels
                .as_object()
                .unwrap()
                .iter()
                .filter_map(|(k, v)| Some((k.parse().ok()?, v.clone())))
                .collect(),
            original_links: None,
        },
        &mut build::Diagnostics::default(),
    )?;
    timer.mark("export_tmp");
    // The comparisons below read JSON only; the graph is not needed again.
    let (graph_nodes, graph_edges) = (graph.nodes.len(), graph.edge_count());
    drop(communities);
    drop(graph);
    crate::par::trim();
    // Read record by record (`read_candidate`) unless the shrink check reads
    // whole nodes.
    let candidate = read_candidate(&tmp, has_saved_semantic(&stored.shrink))?;
    // The reader's batches and buffers are handed back before the query
    // snapshot and the HTML load the new graph.
    crate::par::trim();
    let old_report = fs::read_to_string(out.join("GRAPH_REPORT.md")).unwrap_or_default();
    let unchanged = graph_path.exists()
        && !legacy_layout
        && stored.full == candidate.full_digest()
        && report_for_compare(&old_report) == report_for_compare(&report);
    timer.mark("final_compare");
    if unchanged {
        let _ = fs::remove_file(&tmp);
    } else {
        let mut refusal = vec![];
        if !candidate.check_shrink(
            o.force,
            &stored.shrink,
            !deleted.is_empty(),
            Some(&rebuilt),
            &normalized_failed,
            &deleted,
            &mut refusal,
        ) {
            let _ = fs::remove_file(tmp);
            return Err(refused(&refusal));
        }
        fs::write(out.join(crate::exporters::html::HTML_STALE_MARKER), b"")
            .map_err(crate::Error::from)?;
        export::backup_if_protected(out, &mut build::Diagnostics::default());
        paths::os_replace_with_fallback(&tmp, &graph_path).map_err(crate::Error::from)?;
        paths::write_text_atomic(&out.join("GRAPH_REPORT.md"), &report)
            .map_err(crate::Error::from)?;
        // Split back into the tracked (curated) file and the pending
        // (hub-fallback) sidecar (#3334); the report and graph.json above used
        // the merged labels.
        let (curated, pending) = split_labels(&labels, &pending_cids);
        paths::write_text_atomic(&label_file, &labels_text(&curated)?)
            .map_err(crate::Error::from)?;
        if pending.as_object().is_some_and(|m| !m.is_empty()) {
            paths::write_text_atomic(&pending_file, &labels_text(&pending)?)
                .map_err(crate::Error::from)?;
        } else {
            // Every community has a curated name, or none is pending this run:
            // an emptied sidecar would otherwise stay forever.
            match fs::remove_file(&pending_file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(crate::Error::from(e));
                }
                _ => {}
            }
        }
        paths::write_json_atomic(&sig_file, &cur_sigs, None, false).map_err(crate::Error::from)?;
    }
    timer.mark("write_outputs");
    // A full rebuild writes the new graph's query snapshot (see `rebuild`)
    // while the manifest is written, instead of after it. A changed-paths
    // rebuild leaves it to `rebuild`: it may run again for changes queued
    // meanwhile. The snapshot's graph is released before the HTML loads its
    // own: two graphs are never resident at once.
    let prewarm = !unchanged && full;
    // The candidate document is read for the last time by the HTML below.
    let mut candidate = Some(candidate);
    std::thread::scope(|scope| -> Result<()> {
        let warm = prewarm.then(|| scope.spawn(|| crate::cli::prewarm_query_snapshot(&graph_path)));
        fs::write(out.join(".graphicle_root"), root_marker_value(path)?)
            .map_err(crate::Error::from)?;
        stamp_manifest(out, &detected, &watch_root, &failed)?;
        timer.mark("manifest");
        if let Some(warm) = warm {
            let _ = warm.join();
        }
        crate::par::trim();
        timer.mark("prewarm");
        if !unchanged && let Some(candidate) = candidate.take() {
            match candidate {
                Candidate::Whole(data) => reconcile_html_owned(out, data, &mut result.messages),
                Candidate::Streamed {
                    nodes, edges_key, ..
                } => reconcile_html_with(
                    out,
                    HtmlSource::Stored {
                        path: &graph_path,
                        nodes,
                        edges_key,
                    },
                    &mut result.messages,
                ),
            };
            for entry in fs::read_dir(out).map_err(crate::Error::from)?.flatten() {
                let p = entry.path();
                if p.file_name()
                    .is_some_and(|s| s.to_string_lossy().ends_with("-callflow.html"))
                    && let Err(e) = crate::callflow_html::dispatch(
                        &json!({"function":"write_callflow_html","args":[{"graph":graph_path,"report":out.join("GRAPH_REPORT.md"),"labels":label_file,"output":p,"verbose":false}]}),
                    )
                {
                    result
                        .messages
                        .push(format!("callflow HTML update skipped: {e}"));
                }
            }
        }
        timer.mark("html");
        Ok(())
    })?;
    result.prewarmed = prewarm;
    timer.report(if full {
        "rebuild(full)"
    } else {
        "rebuild(incremental)"
    });
    result.success = true;
    result.changed = !unchanged;
    result.nodes = graph_nodes;
    result.edges = graph_edges;
    Ok(result)
}
pub fn check_update(path: &Path, messages: &mut Vec<String>) -> bool {
    if path
        .join(paths::OUT_DIR.as_str())
        .join("needs_update")
        .exists()
    {
        messages.push(format!(
            "[graphicle check-update] Pending non-code changes in {}.",
            path.display()
        ));
        messages.push(
            "[graphicle check-update] Run `/graphicle --update` to apply semantic re-extraction."
                .into(),
        );
    }
    true
}
pub fn notify_only(path: &Path) -> Result<PathBuf> {
    let flag = path.join(paths::OUT_DIR.as_str()).join("needs_update");
    fs::create_dir_all(flag.parent().unwrap()).map_err(crate::Error::from)?;
    fs::write(&flag, "1").map_err(crate::Error::from)?;
    Ok(flag)
}
/// The suffix a changed path is judged by: the project's declared language
/// for its extension, if any (0.9.87, #2961).
fn watched_suffix(p: &Path) -> String {
    crate::rcfile::effective_suffix(p).to_lowercase()
}
pub fn has_non_code(batch: &[PathBuf]) -> bool {
    batch
        .iter()
        .any(|p| !crate::detect_types::contains("CODE_EXTENSIONS", &watched_suffix(p)))
}
pub fn batch_triggers_rebuild(batch: &[PathBuf]) -> bool {
    batch.iter().any(|p| {
        !p.exists() || crate::detect_types::contains("CODE_EXTENSIONS", &watched_suffix(p))
    })
}
pub fn batch_needs_llm_flag(batch: &[PathBuf]) -> bool {
    has_non_code(
        &batch
            .iter()
            .filter(|p| p.exists())
            .cloned()
            .collect::<Vec<_>>(),
    )
}
fn watched(p: &Path) -> bool {
    [
        "CODE_EXTENSIONS",
        "DOC_EXTENSIONS",
        "PAPER_EXTENSIONS",
        "IMAGE_EXTENSIONS",
    ]
    .iter()
    .any(|k| crate::detect_types::contains(k, &watched_suffix(p)))
}
fn watch_snapshot(root: &Path, ignore: &mut detect::IgnoreState) -> BTreeMap<PathBuf, (u64, u128)> {
    let mut stack = vec![root.to_path_buf()];
    let mut out = BTreeMap::new();
    let mut warnings = detect::EncodingWarnings::default();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == paths::OUT_DIR_NAME.as_str() {
                continue;
            }
            if ignore.predicate(&p, &mut warnings).unwrap_or(false) {
                continue;
            }
            if p.is_dir() {
                if !p.is_symlink() {
                    stack.push(p);
                }
            } else if watched(&p)
                && let Ok(m) = p.metadata()
            {
                let ns = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|t| t.as_nanos())
                    .unwrap_or(0);
                out.insert(p, (m.len(), ns));
            }
        }
    }
    out
}
pub fn watch(
    path: &Path,
    debounce: f64,
    stop: &AtomicBool,
    mut announce: impl FnMut(&str),
) -> Result<()> {
    if !debounce.is_finite() || debounce < 0.0 {
        return Err("debounce must be a finite nonnegative number".into());
    }
    let root = resolved(path);
    let mut ignore = detect::IgnoreState::new(
        &root,
        &[],
        build_gitignore(&root.join(paths::OUT_DIR.as_str())),
        &mut detect::EncodingWarnings::default(),
    )?;
    // The project's language declarations, reloaded whenever its rc file
    // changes; a change rebuilds and flags semantic re-extraction (0.9.87).
    let rc_path = root.join(crate::rcfile::RC_FILENAME);
    let rc_state = |path: &Path| {
        fs::metadata(path).ok().map(|m| {
            let ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|t| t.as_nanos())
                .unwrap_or(0);
            (m.len(), ns)
        })
    };
    crate::rcfile::activate_language_overrides(&root);
    let mut rc_prior = rc_state(&rc_path);
    let mut config_changed = false;
    let mut prior = watch_snapshot(&root, &mut ignore);
    let mut changed = BTreeSet::new();
    let mut last = Instant::now();
    announce(&format!(
        "Watching {} - press Ctrl+C to stop",
        root.display()
    ));
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
        let rc_next = rc_state(&rc_path);
        if rc_next != rc_prior {
            rc_prior = rc_next;
            crate::rcfile::activate_language_overrides(&root);
            config_changed = true;
            last = Instant::now();
        }
        let next = watch_snapshot(&root, &mut ignore);
        for p in prior.keys().chain(next.keys()) {
            if prior.get(p) != next.get(p) {
                changed.insert(p.clone());
                last = Instant::now();
            }
        }
        prior = next;
        if (!changed.is_empty() || config_changed) && last.elapsed().as_secs_f64() >= debounce {
            let batch = std::mem::take(&mut changed).into_iter().collect::<Vec<_>>();
            let config_event = std::mem::take(&mut config_changed);
            announce(&format!("{} file(s) changed", batch.len()));
            if config_event || batch_triggers_rebuild(&batch) {
                match rebuild(path, &RebuildOptions::default()) {
                    Ok(r) => {
                        for m in r.messages {
                            announce(&m);
                        }
                        announce(&format!("Rebuilt: {} nodes, {} edges", r.nodes, r.edges));
                    }
                    Err(e) => announce(&format!("Rebuild failed: {e}")),
                }
            }
            if config_event || batch_needs_llm_flag(&batch) {
                let flag = notify_only(path)?;
                announce(&format!(
                    "Non-code changes require semantic re-extraction; flag written to {}",
                    flag.display()
                ));
            }
        }
    }
    announce("Stopped.");
    Ok(())
}
pub fn apply_resource_limits() {
    #[cfg(unix)]
    unsafe {
        nix::libc::nice(10);
        if let Ok(mb) = std::env::var("GRAPHICLE_REBUILD_MEMORY_LIMIT_MB")
            && let Ok(mb) = mb.trim().parse::<u64>()
            && let Some(limit) = mb.checked_mul(1024 * 1024)
        {
            #[cfg(target_os = "macos")]
            let which = nix::libc::RLIMIT_DATA;
            #[cfg(not(target_os = "macos"))]
            let which = nix::libc::RLIMIT_AS;
            let mut limits = std::mem::zeroed::<nix::libc::rlimit>();
            if nix::libc::getrlimit(which, &mut limits) == 0 {
                limits.rlim_max = limits.rlim_max.min(limit as _);
                limits.rlim_cur = (limit as nix::libc::rlim_t).min(limits.rlim_max);
                nix::libc::setrlimit(which, &limits);
            }
        }
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let p = |k: &str| PathBuf::from(r[k].as_str().unwrap_or("."));
    let ps = |k: &str| {
        items(r, k)
            .iter()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    };
    let strings = |k: &str| {
        items(r, k)
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    };
    match r["action"].as_str().unwrap_or("rebuild") {
        "queue" => {
            queue_pending(&p("out"), &ps("paths"))?;
            Ok(Value::Null)
        }
        "drain" => Ok(json!(drain_pending(&p("out")))),
        "merge_changed" => {
            let groups = items(r, "groups")
                .iter()
                .map(|a| {
                    a.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from)
                        .collect()
                })
                .collect::<Vec<_>>();
            Ok(json!(merge_changed(&groups)))
        }
        "write_config" => {
            write_config(
                &p("out"),
                &strings("excludes").into_iter().collect::<Vec<_>>(),
                r["gitignore"].as_bool(),
            );
            Ok(Value::Null)
        }
        "read_excludes" => Ok(json!(build_excludes(&p("out")))),
        "read_gitignore" => Ok(json!(build_gitignore(&p("out")))),
        "report_root" => Ok(json!(report_root_label(&p("path")))),
        "candidates" => Ok(json!(changed_candidates(
            &p("path"),
            &p("change_root"),
            &p("watch_root")
        ))),
        "relativize" => {
            let mut v = r["data"].clone();
            relativize(&mut v, &p("root"), r["scope"].as_str().map(Path::new));
            Ok(v)
        }
        "rebase" => {
            let mut v = r["data"].clone();
            rebase(&mut v, &p("source_root"), &p("target_root"));
            Ok(v)
        }
        "stored_paths" => {
            let sp =
                StoredSourcePaths::new(&r["data"], &p("out"), &p("project_root"), &p("watch_root"));
            let sf = r["source"].as_str().unwrap_or("");
            let mut item = json!({"source_file":sf});
            sp.rebase_preserved(&mut item);
            Ok(
                json!({"existing_source_root":sp.existing_source_root,"legacy_watch_relative":sp.legacy_watch_relative,"normalize":sp.normalize(sf),"identity":sp.identity(sf),"in_watch_root":sp.in_watch(sf),"rebased":item}),
            )
        }
        "remote" => Ok(json!(remote(r["source"].as_str().unwrap_or("")))),
        "node_communities" => Ok(json!(node_communities(&r["data"]))),
        "canonical" => Ok(canonical_graph(&r["data"], r["topology"] == true)),
        "check_shrink" => {
            let mut messages = vec![];
            let ok = check_shrink(
                r["force"] == true,
                &r["existing"],
                &r["new"],
                r["had_explicit_deletions"] == true,
                r["rebuilt_sources"]
                    .as_array()
                    .map(|_| strings("rebuilt_sources"))
                    .as_ref(),
                &strings("failed_sources"),
                &strings("deleted_sources"),
                &mut messages,
            );
            Ok(json!({"value": ok, "messages": messages}))
        }
        "queue_full" => {
            queue_pending_full(&p("out"))?;
            Ok(Value::Null)
        }
        "drain_full" => Ok(json!(drain_pending_full(&p("out")))),
        "report_compare" => Ok(json!(report_for_compare(r["text"].as_str().unwrap_or("")))),
        "has_non_code" => Ok(json!(has_non_code(&ps("paths")))),
        "triggers" => Ok(json!(batch_triggers_rebuild(&ps("paths")))),
        "needs_llm" => Ok(json!(batch_needs_llm_flag(&ps("paths")))),
        "notify" => {
            notify_only(&p("path"))?;
            Ok(Value::Null)
        }
        "check_update" => Ok(json!(check_update(&p("path"), &mut vec![]))),
        "pending_update" => Ok(json!(
            p("path")
                .join(paths::OUT_DIR.as_str())
                .join("needs_update")
                .exists()
        )),
        "reconcile" => {
            let mut deleted = strings("deleted_paths");
            let ids = strings("deleted_source_identities");
            let code = ps("code_files");
            let targets = ps("extract_targets");
            let out = p("out");
            let project = p("project_root");
            let watch = p("watch_root");
            let ignore = strings("ignored_always");
            let fullignore = strings("ignored_full");
            let mut messages = vec![];
            let result = crate::watch_reconcile::reconcile(
                &r["existing"],
                r["result"].clone(),
                &mut crate::watch_reconcile::ReconcileOptions {
                    out: &out,
                    project_root: &project,
                    watch_root: &watch,
                    code_files: &code,
                    extract_targets: &targets,
                    full: r["full_rebuild"] == true,
                    deleted_paths: &mut deleted,
                    deleted_identities: &ids,
                },
                |p, full| {
                    ignore.contains(p.to_string_lossy().as_ref())
                        || full && fullignore.contains(p.to_string_lossy().as_ref())
                },
                &mut messages,
            )?;
            Ok(json!({"result":result,"deleted_paths":deleted,"messages":messages}))
        }
        "html" => {
            let mut messages = vec![];
            let action = reconcile_html(&p("out"), &r["data"], &mut messages);
            Ok(json!({"action":action,"messages":messages}))
        }
        "rebuild" => {
            let o =
                serde_json::from_value::<RebuildOptions>(r.clone()).map_err(crate::Error::from)?;
            Ok(serde_json::to_value(rebuild(&p("path"), &o)?).unwrap())
        }
        x => Err(format!("Unknown watch action: {x}").into()),
    }
}

#[cfg(test)]
mod rebuild_lock_tests {
    use super::RebuildLock;

    #[test]
    fn rebuild_lock_serializes_nonblocking_acquirers_cross_platform() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join(".graphicle");
        let held = RebuildLock::acquire(&out, true).unwrap().unwrap();
        let contents = std::fs::read_to_string(out.join(".rebuild.lock")).unwrap();
        assert_eq!(contents, format!("{}\n", std::process::id()));
        assert!(RebuildLock::acquire(&out, false).unwrap().is_none());
        drop(held);
        // The lock belongs to the open file description. A process another
        // test spawns at this moment holds a copy of the descriptor until it
        // execs (the descriptor is close-on-exec), so the release is observed
        // within a bounded wait rather than instantly.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let reacquired = loop {
            if let Some(lock) = RebuildLock::acquire(&out, false).unwrap() {
                break Some(lock);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(reacquired.is_some());
    }

    /// Windows: the lock covers one byte at offset 4096, as Graphify's
    /// `msvcrt.locking` does, so another handle can still read the PID.
    #[cfg(windows)]
    #[test]
    fn rebuild_lock_leaves_the_pid_readable_on_windows() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join(".graphicle");
        let held = RebuildLock::acquire(&out, false).unwrap().unwrap();
        let other = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(out.join(".rebuild.lock"))
            .unwrap();
        assert!(!crate::win::try_lock_range(&other, 4096, 1).unwrap());
        assert!(crate::win::try_lock_range(&other, 0, 1).unwrap());
        crate::win::unlock_range(&other, 0, 1);
        let contents = std::fs::read_to_string(out.join(".rebuild.lock")).unwrap();
        assert_eq!(contents, format!("{}\n", std::process::id()));
        drop(held);
        assert!(crate::win::try_lock_range(&other, 4096, 1).unwrap());
    }

    /// The pending queue round-trips under its lock (on Windows the lock
    /// needs a handle with read access).
    #[test]
    fn pending_changes_queue_then_drain() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join(".graphicle");
        let a = std::path::PathBuf::from("a.py");
        let b = std::path::PathBuf::from("b.py");
        super::queue_pending(&out, &[a.clone(), b.clone(), a.clone()]).unwrap();
        assert_eq!(super::drain_pending(&out), vec![a, b]);
        assert!(super::drain_pending(&out).is_empty());
    }
}

#[cfg(test)]
mod canonical_key_tests {
    use super::{canonical_graph_key_owned, canonical_prepared};
    use serde_json::json;

    #[test]
    fn keys_equal_sorting_then_encoding() {
        let node =
            |id: &str, c: i64| json!({"id": id, "label": id, "community": c, "norm_label": id});
        let edge = |s: &str, t: &str| {
            json!({"_src": s, "_tgt": t, "source": "x", "target": "y", "relation": "calls",
                   "confidence_score": 1.0, "meta": {"b": [2, 1], "a": null}})
        };
        let graphs = [
            json!({"directed": true, "built_at_commit": "abc", "graph": {"z": 1, "a": [3, {"k": "v"}]},
                   "nodes": [node("b", 1), node("a", 2), node("b", 1), "stray"],
                   "links": [edge("b", "a"), edge("a", "b"), edge("a", "b")],
                   "hyperedges": [{"id": "h2"}, {"id": "h1", "nodes": ["a", "b"]}]}),
            json!({"nodes": [], "edges": [edge("a", "b")]}),
            json!({"directed": "yes", "nodes": [{"id": "n", "community_name": "c", "x": {"b": 1, "a": [2]}}, 7],
                   "links": [{"_src": null, "_tgt": "t", "source": "s", "target": "t"},
                             {"_src": "s2", "source": "s", "target": "t", "confidence_score": 0.5},
                             {"_tgt": "t3", "relation": "r"}, [1, 2]],
                   "edges": [edge("c", "d")], "hyperedges": "none", "extra": [3, 1, 2]}),
            json!(null),
        ];
        for graph in graphs {
            for topology in [false, true] {
                let expected = crate::extract_normalize::canonical_bytes(&canonical_prepared(
                    graph.clone(),
                    topology,
                ));
                assert_eq!(
                    canonical_graph_key_owned(graph.clone(), topology),
                    expected,
                    "{graph} topology={topology}"
                );
                assert_eq!(
                    super::canonical_graph_key(&graph, topology),
                    expected,
                    "borrowed {graph} topology={topology}"
                );
            }
        }
    }

    /// The digest the rebuild compares stands in for the key: equal exactly
    /// when the keys are, on graphs that differ only in record order, in
    /// ignored fields, and in one field of one record.
    #[test]
    fn digests_agree_with_keys() {
        let base = json!({"directed": true, "built_at_commit": "abc",
            "nodes": [{"id": "a", "label": "a", "community": 1}, {"id": "b", "label": "b"}],
            "links": [{"_src": "a", "_tgt": "b", "source": "x", "target": "y", "relation": "calls",
                       "confidence_score": 1.0}],
            "hyperedges": [{"id": "h", "nodes": ["a", "b"]}]});
        let mut reordered = base.clone();
        reordered["nodes"].as_array_mut().unwrap().reverse();
        reordered["built_at_commit"] = json!("def");
        let mut regrouped = base.clone();
        regrouped["nodes"][0]["community"] = json!(7);
        let mut changed = base.clone();
        changed["links"][0]["relation"] = json!("imports");
        let mut undirected = base.clone();
        undirected.as_object_mut().unwrap().shift_remove("directed");
        let graphs = [
            base,
            reordered,
            regrouped,
            changed,
            undirected,
            json!({}),
            json!(null),
        ];
        for topology in [false, true] {
            for a in &graphs {
                for b in &graphs {
                    assert_eq!(
                        super::canonical_graph_digest(a, topology)
                            == super::canonical_graph_digest(b, topology),
                        super::canonical_graph_key(a, topology)
                            == super::canonical_graph_key(b, topology),
                        "{a} vs {b} topology={topology}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod labels_file_tests {
    use serde_json::json;

    #[test]
    fn labels_are_sorted_by_community_id_and_end_in_a_newline() {
        // Expected bytes from Python 3.12: json.dumps({str(k): v for k, v in
        // sorted(labels.items())}, ensure_ascii=False, indent=2) + "\n".
        let labels = json!({"10": "Zeta", "2": "Große", "1": "a\"b\\c", "0": "日本 \u{2028}"});
        assert_eq!(
            super::labels_text(&labels).unwrap(),
            "{\n  \"0\": \"日本 \u{2028}\",\n  \"1\": \"a\\\"b\\\\c\",\n  \"2\": \"Große\",\n  \"10\": \"Zeta\"\n}\n"
        );
        assert_eq!(super::labels_text(&json!({})).unwrap(), "{}\n");
    }
}

#[cfg(test)]
mod digest_tests {
    use super::{
        Candidate, GRAPH_VERSION_KEYS, SORTED_ARRAYS, canonical_graph_digest,
        canonical_graph_key_owned, graph_topology_digest, prepared_record, record_view,
        stream_candidate, topology_graph_metadata,
    };
    use serde_json::{Value, json};

    /// `canonical_graph_digest` as written before its entries were hashed
    /// through `digest_entries`: the oracle for its exact value.
    fn digest_oracle(data: &Value, topology: bool) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        let Value::Object(top) = data else {
            h.update(canonical_graph_key_owned(data.clone(), topology));
            return h.finalize().into();
        };
        let directed = json!(data["directed"].as_bool().unwrap_or(false));
        let mut keys = top
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "built_at_commit")
            .collect::<Vec<_>>();
        if !topology && !top.contains_key("directed") {
            keys.push("directed");
        }
        keys.sort_unstable();
        let mut bytes = Vec::new();
        let graph_meta = topology_graph_metadata(data, topology);
        h.update(b"{");
        for (i, k) in keys.into_iter().enumerate() {
            if i > 0 {
                h.update(b",");
            }
            bytes.clear();
            serde_json::to_writer(&mut bytes, k).unwrap();
            bytes.push(b':');
            h.update(&bytes);
            if !topology && k == "directed" {
                bytes.clear();
                crate::extract_normalize::write_canonical(&directed, None, &mut bytes);
                h.update(&bytes);
                continue;
            }
            match &top[k] {
                Value::Array(items) if SORTED_ARRAYS.contains(&k) => {
                    let rewritten = topology && matches!(k, "nodes" | "links" | "edges");
                    let mut digests = items
                        .iter()
                        .filter_map(|item| {
                            let encoded = if rewritten {
                                prepared_record(item, k == "nodes")
                            } else {
                                Some(crate::extract_normalize::canonical_bytes(item))
                            };
                            encoded.map(|e| <[u8; 32]>::from(Sha256::digest(&e)))
                        })
                        .collect::<Vec<_>>();
                    digests.sort_unstable();
                    h.update(b"[");
                    for (j, d) in digests.iter().enumerate() {
                        if j > 0 {
                            h.update(b",");
                        }
                        h.update(d);
                    }
                    h.update(b"]");
                }
                other => {
                    bytes.clear();
                    crate::extract_normalize::write_canonical(
                        graph_meta
                            .as_ref()
                            .filter(|_| k == "graph")
                            .unwrap_or(other),
                        None,
                        &mut bytes,
                    );
                    h.update(&bytes);
                }
            }
        }
        h.update(b"}");
        h.finalize().into()
    }

    /// Graph documents as a rebuild writes and compares them, plus the
    /// unusual shapes the digest still has to encode.
    fn documents() -> Vec<Value> {
        let node = |id: &str, c: i64| {
            json!({"label": id, "file_type": "code", "source_file": "a.rs",
                   "source_location": "L1", "community": c, "community_name": format!("C{c}"),
                   "norm_label": id, "_origin": "ast", "id": id})
        };
        let edge = |s: &str, t: &str, r: &str| {
            json!({"relation": r, "confidence": "EXTRACTED", "source_file": "a.rs",
                   "_src": s, "_tgt": t, "confidence_score": 1.0, "weight": 1.0,
                   "source": t, "target": s})
        };
        assert_eq!(crate::export::GRAPH_VERSION_KEY, "graphicle_version");
        vec![
            json!({"directed": false, "multigraph": false,
                   "graph": {"hyperedges": [{"id": "h", "nodes": ["a", "b"]}],
                             "graphicle_version": "0.1.0", "schema_version": 2},
                   "nodes": [node("a", 0), node("b", 1), node("c", 0)],
                   "links": [edge("a", "b", "calls"), edge("b", "c", "imports"), edge("a", "b", "calls")],
                   "hyperedges": [{"id": "h", "nodes": ["a", "b"]}], "built_at_commit": "abc"}),
            json!({"directed": true, "multigraph": true, "graph": {},
                   "nodes": [node("x", 3), {"id": 5, "label": 1.50}, "stray", {"label": "no id"}],
                   "links": [{"source": "x", "target": 5, "key": 0, "relation": "r"},
                             {"source": 5, "target": "x", "key": "k", "w": [2, {"b": 1, "a": null}]}],
                   "hyperedges": []}),
            json!({"nodes": [], "edges": [edge("a", "b", "calls")], "directed": 1}),
            json!({"nodes": [node("a", 1)], "links": [], "hyperedges": "none", "extra": [3, 1, 2]}),
            json!({"graph": {"graphify_version": "0.9.88"}, "nodes": [node("q", 2)], "links": []}),
        ]
    }

    #[test]
    fn digest_keeps_its_former_value() {
        let mut documents = documents();
        documents.extend([json!({}), json!(null)]);
        for document in &documents {
            for topology in [false, true] {
                assert_eq!(
                    canonical_graph_digest(document, topology),
                    digest_oracle(document, topology),
                    "{document} topology={topology}"
                );
            }
        }
    }

    /// The rebuild's topology digest of a built graph equals the digest of
    /// the node-link document it used to build for the comparison.
    #[test]
    fn topology_digest_of_a_graph_matches_its_document() {
        for document in documents() {
            let edges_key = if document.get("links").is_some() {
                "links"
            } else {
                "edges"
            };
            let Ok(graph) =
                crate::graph::Graph::from_node_link_owned_with_arcs(document.clone(), edges_key)
            else {
                continue;
            };
            let mut topology = graph.to_node_link("links");
            topology["hyperedges"] = graph
                .attributes
                .get("hyperedges")
                .cloned()
                .unwrap_or(json!([]));
            assert_eq!(
                graph_topology_digest(&graph),
                canonical_graph_digest(&topology, true),
                "{document}"
            );
            assert_eq!(
                graph_topology_digest(&graph),
                digest_oracle(&topology, true),
                "{document}"
            );
        }
        let mut graph = crate::graph::Graph::new(false, true);
        graph.attributes = json!("not an object");
        assert_eq!(
            graph_topology_digest(&graph),
            digest_oracle(
                &{
                    let mut t = graph.to_node_link("links");
                    t["hyperedges"] = json!([]);
                    t
                },
                true
            )
        );
        assert!(GRAPH_VERSION_KEYS.contains(&crate::export::GRAPH_VERSION_KEY));
    }

    /// A streamed candidate has the whole document's digest and, per node,
    /// what the shrink check and the HTML read; documents it does not take
    /// whole-record-wise are declined.
    #[test]
    fn streamed_candidate_matches_the_document() {
        for document in documents() {
            let text = serde_json::to_vec(&document).unwrap();
            let Some(Candidate::Streamed {
                full,
                nodes,
                edges_key,
            }) = stream_candidate(&text)
            else {
                assert!(
                    document["hyperedges"].is_string() || !document["nodes"].is_array(),
                    "{document}"
                );
                continue;
            };
            assert_eq!(full, digest_oracle(&document, false), "{document}");
            let records = document["nodes"].as_array().unwrap();
            assert_eq!(nodes.len(), records.len());
            for (facts, record) in nodes.iter().zip(records) {
                assert_eq!(facts.view(), record_view(record));
                assert_eq!(facts.id.to_string(), record["id"].to_string());
            }
            let expected = if document.get("links").is_none() && document.get("edges").is_some() {
                "edges"
            } else {
                "links"
            };
            assert_eq!(edges_key, expected);
        }
        for text in [
            r#"{"nodes":[],"links":[],"links":[]}"#,
            r#"{"nodes":{},"links":[]}"#,
            r#"{"links":[]}"#,
            r#"[]"#,
            r#"{"nodes":[],"links":[]} trailing"#,
            r#"{"nodes":[],"hyperedges":null}"#,
        ] {
            assert!(stream_candidate(text.as_bytes()).is_none(), "{text}");
        }
    }
}
