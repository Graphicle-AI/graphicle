// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
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

pub fn queue_pending(out: &Path, changed: &[PathBuf]) -> Result<()> {
    if changed.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(out).map_err(crate::Error::from)?;
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join(".pending.lock"))
        .map_err(crate::Error::from)?;
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
    let Ok(lock) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join(".pending.lock"))
    else {
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
pub fn node_communities(data: &Value) -> BTreeMap<String, i64> {
    items(data, "nodes")
        .iter()
        .filter_map(|n| {
            let id = n.get("id")?;
            if id.is_null() {
                return None;
            }
            let cid = n["community"]
                .as_i64()
                .or_else(|| n["community"].as_str()?.parse().ok())?;
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
    let meta = data.get("graph")?.as_object()?;
    if !topology || !GRAPH_VERSION_KEYS.iter().any(|k| meta.contains_key(*k)) {
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
    let mut h = Sha256::new();
    let Value::Object(top) = data else {
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
    let mut bytes = Vec::new();
    let graph_meta = topology_graph_metadata(data, topology);
    h.update(b"{");
    for (i, k) in keys.into_iter().enumerate() {
        if i > 0 {
            h.update(b",");
        }
        bytes.clear();
        serde_json::to_writer(&mut bytes, k).expect("string serializes");
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
                let mut digests = crate::par::map(items, workers, |item| {
                    let encoded = if rewritten {
                        prepared_record(item, k == "nodes")
                    } else {
                        Some(crate::extract_normalize::canonical_bytes(item))
                    };
                    encoded.map(|e| <[u8; 32]>::from(Sha256::digest(&e)))
                })
                .into_iter()
                .flatten()
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
    if force || old.empty || deletions && rebuilt.is_none() {
        return true;
    }
    let after = items(new, "nodes");
    let deleted = deleted.iter().cloned().collect::<Vec<_>>();
    let missing = build::lost_saved_semantic_nodes(&old.saved, &after, &deleted);
    if !missing.is_empty() {
        let ids = missing
            .iter()
            .map(|n| crate::python_compat::string(n.get("id").unwrap_or(&Value::Null)))
            .collect::<Vec<_>>()
            .join(", ");
        messages.push(format!(
            "[graphicle] WARNING: Refusing to overwrite — saved semantic node(s) are missing from the new graph: {ids}."
        ));
        if after.len() < old.nodes.len() {
            messages.push(shrink_warning(after.len(), old.nodes.len()));
        }
        return false;
    }
    if after.len() >= old.nodes.len() {
        return true;
    }
    if let Some(rebuilt) = rebuilt {
        let ids = after
            .iter()
            .map(|n| n["id"].to_string())
            .collect::<BTreeSet<_>>();
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
    messages.push(shrink_warning(after.len(), old.nodes.len()));
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
}
fn reconcile_html_with(
    out: &Path,
    source: HtmlSource<'_>,
    messages: &mut Vec<String>,
) -> Option<String> {
    let data = match &source {
        HtmlSource::Borrowed(data) => *data,
        HtmlSource::Owned(data) => data,
    };
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
    let cids = node_communities(data);
    // Upstream groups the members by iterating its node -> community dict,
    // so communities (and the aggregated view's community nodes) come in the
    // order of their first member in graph.json, each node id once.
    let mut communities: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut slots = std::collections::HashMap::<i64, usize>::new();
    let mut members_seen = std::collections::HashSet::new();
    let mut labels = BTreeMap::new();
    for n in items(data, "nodes") {
        let id = text(&n, "id");
        if let Some(cid) = cids.get(&id) {
            if members_seen.insert(id) {
                let slot = *slots.entry(*cid).or_insert_with(|| {
                    communities.push((*cid, Vec::new()));
                    communities.len() - 1
                });
                communities[slot].1.push(n["id"].clone());
            }
            if n["community_name"].as_str().is_some_and(|s| !s.is_empty()) {
                labels.entry(*cid).or_insert(n["community_name"].clone());
            }
        }
    }
    let render = || -> Result<bool> {
        let g = match source {
            HtmlSource::Borrowed(data) => paths::load_node_link_graph(data)?,
            HtmlSource::Owned(data) => paths::load_node_link_graph_owned(data)?,
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
    // The stored graph is parsed while the corpus is detected: neither reads
    // the other, and a detection error still takes precedence.
    let (detected, existing) = std::thread::scope(|scope| {
        let loading = scope.spawn(|| {
            if graph_path.exists() {
                read_graph(&graph_path)
            } else {
                Ok(json!({}))
            }
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
        let existing = loading
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (detected, existing)
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
    let existing = std::sync::Arc::new(existing?);
    timer.mark("load_existing");
    if code.is_empty() && !graph_path.exists() {
        result
            .messages
            .push("No code files found - nothing to rebuild.".into());
        return Ok(result);
    }
    let sp = StoredSourcePaths::new(&existing, out, &project_root, &watch_root);
    let semantic = items(&existing, "nodes")
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
    // What the rest of the rebuild reads from the stored graph once the new
    // graph is built (its comparison digests, communities and shrink base) is
    // taken beside extraction, so that the stored document (about as large as
    // the new graph) can be released before the new graph is built.
    let stored_job = {
        let existing = existing.clone();
        std::thread::spawn(move || StoredGraph::of(&existing))
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
        for n in items(&existing, "nodes") {
            if text(&n, "id").is_empty()
                || !build::is_ast_tier(&n)
                || !live.contains(&sp.identity(&text(&n, "source_file")))
            {
                continue;
            }
            let mut c = json!({});
            for k in ["id", "label", "source_file", "file_type", "type"] {
                c[k] = n[k].clone();
            }
            // `--routes`: requests in re-extracted files reach stored routes.
            if text(&n, "node_kind") == "route" {
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
        for e in items(
            &existing,
            if existing.get("links").is_some() {
                "links"
            } else {
                "edges"
            },
        ) {
            if !["contains", "method", "inherits"].contains(&text(&e, "relation").as_str())
                || !build::is_ast_tier(&e)
                || !live.contains(&sp.identity(&text(&e, "source_file")))
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
    // The ignore rules reconcile consults (loading them may run `git
    // ls-files`) are read while extraction runs; an error loading them still
    // surfaces after extraction, as before.
    let (extraction, ignore_states) = std::thread::scope(|scope| {
        let ignore_states = scope.spawn(|| {
            let mut warning = detect::EncodingWarnings::default();
            let always = detect::IgnoreState::new(&watch_root, &excludes, false, &mut warning)?;
            let all = detect::IgnoreState::new(&watch_root, &excludes, gitignore, &mut warning)?;
            Ok::<_, crate::Error>((always, all, warning))
        });
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
        let ignore_states = ignore_states
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (extraction, ignore_states)
    });
    let mut extraction = extraction?;
    timer.mark("extract");
    rebase(&mut extraction, &watch_root, &project_root);
    let failed = items(&extraction, "failed_sources")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let (mut always, mut all, mut warning) = ignore_states?;
    if graph_path.exists() {
        extraction = crate::watch_reconcile::reconcile(
            &existing,
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
            |p, full| {
                if full {
                    all.predicate(p, &mut warning).unwrap_or(false)
                } else {
                    always.predicate(p, &mut warning).unwrap_or(false)
                }
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
        let mut nodes = build::dedupe_nodes(&items(&extraction, "nodes"))?;
        build::disambiguate_file_labels_in_nodes(&mut nodes);
        let links = build::dedupe_edges(&items(&extraction, "edges"))?;
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
        result.nodes = items(&candidate, "nodes").len();
        result.edges = items(&candidate, "links").len();
        return Ok(result);
    }
    let mut stored = stored_job
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    crate::par::drop_later(existing);
    let graph = build::build_from_json(
        &mut extraction,
        stored.directed,
        None,
        &mut build::Diagnostics::default(),
    )?;
    // Nothing reads the extraction after the build.
    drop(extraction);
    timer.mark("build");
    let mut topology = graph.to_node_link("links");
    topology["hyperedges"] = graph
        .attributes
        .get("hyperedges")
        .cloned()
        .unwrap_or(json!([]));
    let topology_unchanged = graph_path.exists()
        && !legacy_layout
        && stored.topology == canonical_graph_digest(&topology, true);
    crate::par::drop_later(topology);
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
        let previous = previous.into_iter().map(|(k, v)| (json!(k), v)).collect();
        communities = cluster::remap_communities_to_previous(&communities, &previous)?;
    }
    timer.mark("cluster");
    let cur_sigs = cluster::community_member_sigs(&communities);
    let label_file = out.join(".graphicle_labels.json");
    let sig_file = out.join(".graphicle_labels.json.sig");
    let raw = fs::read(&label_file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(json!({}));
    let saved = fs::read(&sig_file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(json!({}));
    let mut labels = json!({});
    let mut missing = vec![];
    for (cid, members) in &communities {
        let key = cid.to_string();
        let fresh = if saved.as_object().is_some_and(|m| !m.is_empty()) {
            saved[&key] == cur_sigs[&key]
        } else {
            raw.as_object()
                .is_some_and(|m| m.len() == communities.len())
        };
        if fresh
            && raw[&key]
                .as_str()
                .is_some_and(|s| s != format!("Community {cid}"))
        {
            labels[&key] = raw[&key].clone();
        } else {
            missing.push((*cid, members.clone()));
        }
    }
    if let Some(m) = cluster::label_communities_by_hub(&graph, &missing).as_object() {
        labels.as_object_mut().unwrap().extend(m.clone());
    }
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
    let candidate = read_graph(&tmp)?;
    let old_report = fs::read_to_string(out.join("GRAPH_REPORT.md")).unwrap_or_default();
    let unchanged = graph_path.exists()
        && !legacy_layout
        && stored.full == canonical_graph_digest(&candidate, false)
        && report_for_compare(&old_report) == report_for_compare(&report);
    timer.mark("final_compare");
    if unchanged {
        let _ = fs::remove_file(&tmp);
    } else {
        let mut refusal = vec![];
        if !check_shrink_with(
            o.force,
            &stored.shrink,
            &candidate,
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
        paths::write_text_atomic(&label_file, &labels_text(&labels)?)
            .map_err(crate::Error::from)?;
        paths::write_json_atomic(&sig_file, &cur_sigs, None, false).map_err(crate::Error::from)?;
    }
    timer.mark("write_outputs");
    // A full rebuild writes the new graph's query snapshot (see `rebuild`)
    // while the manifest and HTML are written, instead of after them. A
    // changed-paths rebuild leaves it to `rebuild`: it may run again for
    // changes queued meanwhile.
    let prewarm = !unchanged && full;
    // The candidate document is read for the last time by the HTML below.
    let mut candidate = Some(candidate);
    std::thread::scope(|scope| -> Result<()> {
        let warm = prewarm.then(|| scope.spawn(|| crate::cli::prewarm_query_snapshot(&graph_path)));
        fs::write(out.join(".graphicle_root"), root_marker_value(path)?)
            .map_err(crate::Error::from)?;
        stamp_manifest(out, &detected, &watch_root, &failed)?;
        timer.mark("manifest");
        if !unchanged && let Some(candidate) = candidate.take() {
            reconcile_html_owned(out, candidate, &mut result.messages);
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
        if let Some(warm) = warm {
            let _ = warm.join();
        }
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
