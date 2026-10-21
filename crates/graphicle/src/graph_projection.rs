//! Projection of a repository head set into the typed code graph and its
//! bi-temporal `gf_*` tables. One resident graph per `(repository_id, episode)`.
use crate::platform::CanonicalExt;
use crate::{
    Error, Result, digest, now,
    store::{Principal, Store, one, repo, rows, s},
    string,
};
use graphicle::{
    build::{BuildOptions, Diagnostics, build_owned},
    extract::ExtractOptions,
    graph::Graph,
    session::{GraphHandle, registry},
};
use graphicle_memory::graph::identity::{self, IdentityOptions};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Mutex, OnceLock, PoisonError},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectionInfo {
    pub episode: String,
    pub nodes: usize,
    pub edges: usize,
    pub graph_ref: String,
    pub replayed: bool,
}

pub fn graph_ref(repository: &str, episode: &str) -> String {
    format!("{repository}:{episode}")
}

/// Deterministic digest of the repository head set (`path → sha256`).
pub fn episode_of(heads: &BTreeMap<String, String>) -> String {
    digest(&json!(heads))
}

pub fn current_episode(db: &Connection, repository: &str) -> Result<String> {
    Ok(episode_of(&crate::store::heads(db, repository)?))
}

/// Durable per-repository extraction cache next to the SQLite file. Entries
/// are keyed by content and repository-relative path, so they hit across the
/// fresh materialization directory of every build. In-memory databases have
/// no directory and build without a cache.
pub fn cache_root(db: &Connection, repository: &str) -> Option<PathBuf> {
    let database = db.path().filter(|p| !p.is_empty())?;
    let directory = std::path::Path::new(database).parent()?;
    if !repository
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(directory.join("gf-cache").join(repository))
}

/// A resolver cache must never survive a change of engine implementation.
/// The executable's size, modification time and its first and last 4 MiB
/// identify the build: the Mach-O UUID or ELF build ID sits in the headers and
/// changes with any code change. Hashing a whole debug build (hundreds of
/// megabytes) took longer than the store's busy timeout.
fn engine_fingerprint() -> Option<&'static str> {
    static FINGERPRINT: OnceLock<Option<String>> = OnceLock::new();
    FINGERPRINT
        .get_or_init(|| {
            use sha2::{Digest, Sha256};
            use std::io::{Read, Seek, SeekFrom};
            const SPAN: u64 = 4 << 20;
            let path = std::env::current_exe().ok()?;
            let mut file = std::fs::File::open(&path).ok()?;
            let meta = file.metadata().ok()?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            let mut sha = Sha256::new();
            sha.update(meta.len().to_le_bytes());
            sha.update(modified.to_le_bytes());
            let mut buffer = vec![0u8; SPAN as usize];
            for start in [0, meta.len().saturating_sub(SPAN)] {
                file.seek(SeekFrom::Start(start)).ok()?;
                let mut read = 0;
                while read < buffer.len() {
                    let n = file.read(&mut buffer[read..]).ok()?;
                    if n == 0 {
                        break;
                    }
                    read += n;
                }
                sha.update(&buffer[..read]);
            }
            Some(format!("{:x}", sha.finalize()))
        })
        .as_deref()
}

/// Extract and build the typed graph for the current head set.
pub fn build_graph(db: &Connection, repository: &str) -> Result<(String, Graph)> {
    let (episode, graph, _) = build_graph_with_report(db, repository)?;
    Ok((episode, graph))
}

/// `build_graph` plus the extraction cache report of this run
/// (`{cached, changed, unchanged, unsupported}`). The report describes the
/// run, not the graph, so it is stored with the episode rather than inside
/// the graph attributes.
pub fn build_graph_with_report(
    db: &Connection,
    repository: &str,
) -> Result<(String, Graph, Value)> {
    let heads = crate::store::heads(db, repository)?;
    let episode = episode_of(&heads);
    // Keep the materialization root stable so the file-description and
    // root-sensitive resolver caches can actually be reused. Serialize before
    // preparing it: another build must not mutate files during extraction.
    static EXTRACTION: Mutex<()> = Mutex::new(());
    let _serial = EXTRACTION.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache_root(db, repository);
    let dir = crate::phase::phase("graph_materialize", || {
        crate::graph_materialize::prepare(db, repository, &heads, cache.as_deref())
    })?;
    // Canonical root: node identities derive from paths relative to it, and a
    // root that crosses a symlink would make cached and fresh records diverge.
    let root = dir.path().canonical()?;
    let files: Vec<PathBuf> = heads.keys().map(|p| root.join(p)).collect();
    // A contended cross-process cache gets an isolated uncached tree.
    let cache = cache.filter(|_| dir.report.cached);
    if let Some(cache) = &cache {
        std::fs::create_dir_all(cache)?;
    }
    let (extraction, report) = crate::phase::phase("graph_extract", || {
        graphicle::incremental::extract(
            &files,
            &ExtractOptions {
                root: Some(root.clone()),
                use_cache: cache.is_some(),
                cache_root: cache.clone(),
                // Receiver-aware method resolution: additive `calls`
                // edges labelled `derivation: inferred` + `receiver_evidence`;
                // ambiguous receivers land in `attributes.unresolved_calls`.
                receiver_resolution: true,
                test_identities: true,
                ..Default::default()
            },
            cache
                .as_ref()
                .filter(|_| engine_fingerprint().is_some())
                .map(|p| p.join("resolution"))
                .as_deref(),
            engine_fingerprint().unwrap_or("uncached"),
        )
    })?;
    let extraction_summary = json!({
        "cached": cache.is_some(),
        "materialization": dir.report,
        "changed": report.parsing.changed.len(),
        "unchanged": report.parsing.unchanged + report.skipped_parsing,
        "unsupported": report.parsing.unsupported,
        "resolution": {"dirty_files":report.dirty_files,"resolved_files":report.resolved_files,
            "context_files":report.context_files,"reused_files":report.reused_files,
            "skipped_parsing":report.skipped_parsing,
            "components":report.components,"largest_component":report.largest_component,
            "fallback":report.fallback},
    });
    drop(report);
    drop(files);
    // Multigraph keyed by relation: the extractors emit parallel edges with
    // distinct relations between one pair (`calls`+`contains` for a nested
    // function, `inherits`+`uses`, `imports`+`imports_from`); the parity
    // non-multigraph build folds them into one edge with mixed attributes.
    // `build_owned` consumes the extraction so the records are moved into the
    // graph instead of being copied (the borrowing `build` holds both).
    let mut graph = crate::phase::phase("graph_assemble", || {
        build_owned(
            vec![extraction],
            &BuildOptions {
                directed: true,
                root: Some(root.clone()),
                multigraph: true,
                ..Default::default()
            },
            &mut Diagnostics::default(),
            None,
        )
    })?;
    // Durable identity: every persisted node carries a content-independent
    // `stable_id` and a content `revision`; every edge its `derivation`.
    // A PR scratch project's ids are scoped by its folder project, so a symbol has one `stable_id` in both; its nodes still name the
    // scratch project as their `repo`, where their episode is recorded.
    let scope = identity_scope(db, repository)?;
    crate::phase::phase("graph_identity", || {
        identity::annotate(
            &mut graph,
            &IdentityOptions {
                repo: scope.clone(),
                root: Some(root.clone()),
                strip_prefix: Some(root.to_string_lossy().into_owned()),
                sources: BTreeMap::new(),
            },
        )
    })?;
    if scope != repository {
        for node in graph.nodes_mut() {
            node.attributes.insert("repo".into(), json!(repository));
        }
    }
    Ok((episode, graph, extraction_summary))
}

/// The identity scope of `repository`'s `stable_id`s: its own
/// id, or for a PR scratch project linked to a folder project
/// ([`crate::folders::scratch_of`]) that project's id.
pub fn identity_scope(db: &Connection, repository: &str) -> Result<String> {
    Ok(crate::folders::scratch_of(db, repository)?.unwrap_or_else(|| repository.to_string()))
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn edge_key_value(text: &str) -> Value {
    if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text).unwrap_or_else(|_| json!(text))
    }
}

pub fn episode_row(db: &Connection, repository: &str, episode: &str) -> Result<Option<Value>> {
    one(
        db,
        "SELECT episode,sequence,created_at,node_count,edge_count,directed,multigraph,attributes,parent_episode FROM gf_episodes WHERE repository_id=? AND episode=?",
        params![repository, episode],
    )
}

/// Whether `episode` has been recorded for this repository (G04 arm gate).
pub fn is_recorded(db: &Connection, repository: &str, episode: &str) -> Result<bool> {
    Ok(episode_row(db, repository, episode)?.is_some())
}

/// Persist `graph` as `episode`: close live rows that disappeared or changed,
/// insert new ones, never delete. Idempotent for an already recorded episode.
pub fn persist(db: &Connection, repository: &str, episode: &str, graph: &Graph) -> Result<bool> {
    persist_with_report(db, repository, episode, graph, None)
}

/// Episode attribute key holding the extraction run report; stripped on `load`.
pub const EXTRACTION_RUN: &str = "extraction_run";
/// Complete source manifest of the episode, including documents and files
/// the AST extractor does not understand. This is storage metadata, not a
/// graph attribute returned by `load`.
const SOURCE_HEADS: &str = "source_heads";

/// The source manifest stored with a recorded episode, or `None` for an
/// episode recorded without one (older stores, or a graph persisted for a
/// head set other than the current one). One row read: unlike
/// [`episode_heads`], it never searches the retained sync manifests, so a
/// query path can afford it.
pub fn stored_episode_heads(
    db: &Connection,
    repository: &str,
    episode: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    let row = episode_row(db, repository, episode)?
        .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
    let attributes: Value = serde_json::from_str(s(&row, "attributes"))?;
    Ok(match attributes.get(SOURCE_HEADS) {
        Some(value) => Some(serde_json::from_value(value.clone())?),
        None => None,
    })
}

/// Exact source revisions for a recorded episode. Older stores can recover
/// them from retained sync manifests; no current file is substituted for a
/// historical file merely because the paths match.
pub fn episode_heads(
    db: &Connection,
    repository: &str,
    episode: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    if let Some(heads) = stored_episode_heads(db, repository, episode)? {
        return Ok(Some(heads));
    }
    let current = crate::store::heads(db, repository)?;
    if episode_of(&current) == episode {
        return Ok(Some(current));
    }
    for snapshot in rows(
        db,
        "SELECT manifest FROM repo_snapshots WHERE repository_id=? ORDER BY created_at DESC",
        [repository],
    )? {
        let manifest: BTreeMap<String, Value> = serde_json::from_str(s(&snapshot, "manifest"))?;
        let heads: BTreeMap<String, String> = manifest
            .into_iter()
            .map(|(path, entry)| (path, s(&entry, "sha256").to_string()))
            .collect();
        if episode_of(&heads) == episode {
            return Ok(Some(heads));
        }
    }
    Ok(None)
}

pub fn persist_with_report(
    db: &Connection,
    repository: &str,
    episode: &str,
    graph: &Graph,
    extraction: Option<&Value>,
) -> Result<bool> {
    if episode_row(db, repository, episode)?.is_some() {
        return Ok(false);
    }
    let write = crate::graph_delta::ProjectionWrite::begin(db)?;
    let stamp = now();
    let heads = crate::store::heads(db, repository)?;
    let delta = crate::graph_delta::Plan::new(db, repository, graph)?;
    let mut attributes = episode_attributes(graph, extraction);
    if let Some(report) = attributes
        .get_mut(EXTRACTION_RUN)
        .and_then(Value::as_object_mut)
    {
        report.insert("persistence".into(), serde_json::to_value(&delta.stats)?);
    }
    let head_set = episode_of(&heads) == episode;
    if head_set {
        attributes[SOURCE_HEADS] = json!(heads);
    }
    let sequence: i64 = db.query_row(
        "SELECT coalesce(max(sequence),0)+1 FROM gf_episodes WHERE repository_id=?",
        [repository],
        |r| r.get(0),
    )?;
    // History is a DAG over tree digests: the parent is whatever was `latest`
    // when this episode was recorded (merges are not reconstructed here).
    let parent: Option<String> = one(
        db,
        "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
        [repository],
    )?
    .map(|row| s(&row, "episode").to_string());
    db.execute(
        "INSERT INTO gf_episodes(repository_id,episode,sequence,created_at,node_count,edge_count,directed,multigraph,attributes,parent_episode) VALUES(?,?,?,?,?,?,?,?,?,?)",
        params![
            repository,
            episode,
            sequence,
            stamp,
            graph.node_count() as i64,
            graph.edge_count() as i64,
            graph.is_directed(),
            graph.is_multigraph(),
            graphicle_memory::text::canonical(&attributes),
            parent,
        ],
    )?;
    let recorded = crate::temporal_graph::instant(&stamp)?;
    crate::temporal_graph::observe(
        db,
        repository,
        episode,
        "default",
        recorded,
        recorded,
        "projection",
    )?;
    persist_unresolved_calls(db, repository, episode, graph, &stamp)?;
    // A projection writes hundreds of thousands of rows in one transaction;
    // the default 2 MiB page cache spills dirty pages to the WAL mid-way and
    // reads them back. The cache is a cap, released with this connection.
    db.pragma_update(None, "cache_size", -262_144)?;

    // Live rows are compared by a digest of their canonical attributes so the
    // diff holds one small entry per row instead of every attribute blob.
    let mut live_nodes: HashMap<String, (i64, Digest)> = HashMap::new();
    {
        let mut select = db.prepare_cached(
            "SELECT id,node_id,attributes FROM gf_nodes WHERE repository_id=? AND valid_to_episode IS NULL AND owner IN (SELECT value FROM json_each(?))",
        )?;
        let mut live = select.query(params![repository, delta.dirty_json])?;
        while let Some(row) = live.next()? {
            let id: i64 = row.get(0)?;
            let node_id: String = row.get(1)?;
            let attributes = column_str(row, 2)?;
            live_nodes.insert(node_id, (id, digest_of(attributes.as_bytes())));
        }
    }
    let mut close_node =
        db.prepare_cached("UPDATE gf_nodes SET valid_to_episode=?,expired_at=? WHERE id=?")?;
    let mut insert_node = db.prepare_cached(
        "INSERT INTO gf_nodes(repository_id,node_id,attributes,valid_from_episode,created_at,owner) VALUES(?,?,?,?,?,?)",
    )?;
    let nodes = graph.nodes();
    let node_ids: Vec<String> = nodes.iter().map(|node| text(&node.id)).collect();
    // Dependency bookkeeping for the nodes this episode inserted or closed;
    // unchanged nodes keep the `graph_node → file` rows of their own episode.
    let mut inserted_nodes: Vec<(usize, &str)> = Vec::new();
    let mut attributes = String::new();
    for (index, node) in nodes.iter().enumerate() {
        if !delta.changed(&node.attributes) {
            continue;
        }
        canonical_into(&mut attributes, &node.attributes);
        match live_nodes.remove(&node_ids[index]) {
            Some((_, existing)) if existing == digest_of(attributes.as_bytes()) => {}
            other => {
                if let Some((id, _)) = other {
                    close_node.execute(params![episode, stamp, id])?;
                }
                insert_node.execute(params![
                    repository,
                    node_ids[index],
                    attributes,
                    episode,
                    stamp,
                    crate::graph_delta::owner(&node.attributes)
                ])?;
                inserted_nodes.push((index, node_ids[index].as_str()));
            }
        }
    }
    for (id, _) in live_nodes.values() {
        close_node.execute(params![episode, stamp, id])?;
    }
    let closed_nodes: Vec<String> = live_nodes.into_keys().collect();

    let mut live_edges: HashMap<Vec<u8>, (i64, Digest)> = HashMap::new();
    {
        let mut select = db.prepare_cached(
            "SELECT id,source,target,relation,key,attributes FROM gf_edges WHERE repository_id=? AND valid_to_episode IS NULL AND owner IN (SELECT value FROM json_each(?))",
        )?;
        let mut live = select.query(params![repository, delta.dirty_json])?;
        let mut identity = Vec::new();
        while let Some(row) = live.next()? {
            let id: i64 = row.get(0)?;
            edge_identity(
                &mut identity,
                column_str(row, 1)?,
                column_str(row, 2)?,
                column_str(row, 3)?,
                column_str(row, 4)?,
            );
            let attributes = column_str(row, 5)?;
            live_edges.insert(identity.clone(), (id, digest_of(attributes.as_bytes())));
        }
    }
    let mut close_edge =
        db.prepare_cached("UPDATE gf_edges SET valid_to_episode=?,expired_at=? WHERE id=?")?;
    let mut insert_edge = db.prepare_cached(
        "INSERT INTO gf_edges(repository_id,source,target,relation,key,attributes,valid_from_episode,created_at,owner) VALUES(?,?,?,?,?,?,?,?,?)",
    )?;
    // Plain INSERT: the edge row id is fresh, so there is nothing to replace,
    // and `OR REPLACE` would make SQLite open a statement journal per row.
    let mut insert_source = db.prepare_cached(
        "INSERT INTO gf_edge_sources(repository_id,edge_row_id,relpath,source_location,derivation) VALUES(?,?,?,?,?)",
    )?;
    // Persisted row id of every graph edge, in `iter_edges` order, for the
    // contradiction check of the edges closed below.
    let mut row_ids: Vec<Option<i64>> = Vec::with_capacity(graph.edge_count());
    let mut identity = Vec::new();
    let mut key = String::new();
    for (source, target, edge) in graph.iter_edges() {
        if !delta.changed(&edge.attributes) {
            row_ids.push(None);
            continue;
        }
        let relation = edge
            .attributes
            .get("relation")
            .map(text)
            .unwrap_or_default();
        key.clear();
        if !edge.key.is_null() {
            key.push_str(&edge.key.to_string());
        }
        edge_identity(
            &mut identity,
            &node_ids[source],
            &node_ids[target],
            &relation,
            &key,
        );
        canonical_into(&mut attributes, &edge.attributes);
        let row_id = match live_edges.remove(&identity) {
            Some((id, existing)) if existing == digest_of(attributes.as_bytes()) => id,
            other => {
                if let Some((id, _)) = other {
                    close_edge.execute(params![episode, stamp, id])?;
                }
                insert_edge.execute(params![
                    repository,
                    node_ids[source],
                    node_ids[target],
                    relation,
                    key,
                    attributes,
                    episode,
                    stamp,
                    crate::graph_delta::owner(&edge.attributes)
                ])?;
                let id = db.last_insert_rowid();
                let field = |key: &str| {
                    edge.attributes
                        .get(key)
                        .filter(|v| !v.is_null())
                        .map(text)
                        .unwrap_or_default()
                };
                insert_source.execute(params![
                    repository,
                    id,
                    field("source_file"),
                    field("source_location"),
                    field("derivation")
                ])?;
                id
            }
        };
        row_ids.push(Some(row_id));
    }
    if !live_edges.is_empty() {
        // Cardinality-one relations in the new graph: (relation, subject) →
        // the subject's targets with their persisted row ids. Only needed
        // when an edge disappeared.
        let mut single_now: HashMap<(String, String), Vec<(String, i64)>> = HashMap::new();
        let mut find_row = db.prepare_cached("SELECT id FROM gf_edges WHERE repository_id=? AND source=? AND target=? AND relation=? AND key=? AND valid_to_episode IS NULL")?;
        for (row_id, (source, target, edge)) in row_ids.iter().zip(graph.iter_edges()) {
            let relation = edge
                .attributes
                .get("relation")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some((subject, other)) = single_valued(
                relation,
                &node_ids[source],
                &node_ids[target],
                &edge.attributes,
            ) {
                let id = match row_id {
                    Some(id) => *id,
                    None => {
                        let key = if edge.key.is_null() {
                            String::new()
                        } else {
                            edge.key.to_string()
                        };
                        find_row.query_row(
                            params![
                                repository,
                                node_ids[source],
                                node_ids[target],
                                relation,
                                key
                            ],
                            |row| row.get(0),
                        )?
                    }
                };
                single_now
                    .entry((relation.to_string(), subject))
                    .or_default()
                    .push((other, id));
            }
        }
        let mut closed: Vec<i64> = live_edges.into_values().map(|(id, _)| id).collect();
        closed.sort_unstable();
        let mut old_edge =
            db.prepare_cached("SELECT source,target,relation,attributes FROM gf_edges WHERE id=?")?;
        for id in &closed {
            close_edge.execute(params![episode, stamp, id])?;
            let (source, target, relation, attributes): (String, String, String, String) =
                old_edge.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            record_contradiction(
                db,
                repository,
                episode,
                &stamp,
                graph,
                &single_now,
                (&source, &target, &relation, *id, &attributes),
            )?;
        }
    }
    drop(row_ids);
    drop(close_node);
    drop(insert_node);
    drop(close_edge);
    drop(insert_edge);
    drop(insert_source);
    // Live nodes are fresh derivations of their source files.
    let workspace = crate::invalidation::workspace_of(db, repository)?;
    crate::invalidation::record_graph_nodes(
        db,
        &workspace,
        repository,
        inserted_nodes.iter().map(|(index, node_id)| {
            (
                *node_id,
                nodes[*index]
                    .attributes
                    .get("relpath")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty()),
            )
        }),
        closed_nodes.iter().map(String::as_str),
        inserted_nodes.len() < nodes.len(),
    )?;
    delta.finish(db, repository)?;
    // The live-symbol table follows the current head set's
    // episode, by the files this projection rewrote when it described the
    // previous one, and pins quotes that waited for this content.
    if head_set {
        crate::symbols::record(
            db,
            repository,
            episode,
            graph,
            &heads,
            parent.as_deref(),
            delta.dirty(),
        )?;
    }
    write.commit()?;
    Ok(true)
}

type Digest = [u8; 16];

/// Borrow a TEXT column without copying it out of the row.
fn column_str<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> rusqlite::Result<&'a str> {
    row.get_ref(index)?.as_str().map_err(rusqlite::Error::from)
}

/// Borrow a nullable TEXT column.
fn column_opt_str<'a>(
    row: &'a rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<&'a str>> {
    row.get_ref(index)?
        .as_str_or_null()
        .map_err(rusqlite::Error::from)
}

/// Truncated SHA-256 of an attribute blob: enough to compare live rows with
/// the new graph without holding every blob in memory.
fn digest_of(bytes: &[u8]) -> Digest {
    let hash = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 16];
    out.copy_from_slice(&hash.as_ref()[..16]);
    out
}

/// One allocation per lookup key, with length-prefixed columns: JSON strings
/// can contain any delimiter, including decoded NUL characters.
fn edge_identity(out: &mut Vec<u8>, source: &str, target: &str, relation: &str, key: &str) {
    out.clear();
    for part in [source, target, relation, key] {
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part.as_bytes());
    }
}

/// `serde_json::Value` serialized with object keys in byte order: the same
/// bytes as `graphicle_memory::text::canonical`, without cloning the tree.
struct Canonical<'a>(&'a Value);

impl Serialize for Canonical<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(fields) => CanonicalMap(fields).serialize(serializer),
            Value::Array(items) => serializer.collect_seq(items.iter().map(Canonical)),
            other => other.serialize(serializer),
        }
    }
}

struct CanonicalMap<'a>(&'a Map<String, Value>);

impl Serialize for CanonicalMap<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut sorted: Vec<(&String, &Value)> = self.0.iter().collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut map = serializer.serialize_map(Some(sorted.len()))?;
        for (key, value) in sorted {
            map.serialize_entry(key, &Canonical(value))?;
        }
        map.end()
    }
}

pub(crate) struct CanonicalRecord<'a>(pub(crate) &'a graphicle::record::Attributes);
impl Serialize for CanonicalRecord<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut sorted = self.0.iter().collect::<Vec<_>>();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut map = serializer.serialize_map(Some(sorted.len()))?;
        for (key, value) in sorted {
            map.serialize_entry(key, &Canonical(value))?;
        }
        map.end()
    }
}

/// Canonical JSON of `fields` written into `out` (cleared first).
fn canonical_into(out: &mut String, fields: &graphicle::record::Attributes) {
    out.clear();
    // serde_json only emits valid UTF-8.
    let mut bytes = std::mem::take(out).into_bytes();
    serde_json::to_writer(&mut bytes, &CanonicalRecord(fields))
        .expect("JSON values are serializable");
    *out = String::from_utf8(bytes).expect("serde_json emits UTF-8");
}

/// File extensions of languages whose classes extend at most one class.
/// Python, C++, Perl, Scala (`with`) and Lisp allow several and stay out.
const SINGLE_INHERITANCE: &[&str] = &[
    "java", "kt", "kts", "cs", "swift", "dart", "ts", "tsx", "js", "jsx", "mjs", "cjs", "php",
    "rb", "m", "mm", "cls", "trigger", "groovy", "vb",
];

/// The (subject, other) pair of an edge whose relation has declared
/// cardinality one on the subject side; `None` for multi-valued relations
/// (`calls`, `imports`, `references`, ...) which never auto-close except by
/// disappearance. `contains` is one-valued on the child, so the subject is
/// the target; `inherits`/`extends` only count in single-inheritance languages.
fn single_valued(
    relation: &str,
    source: &str,
    target: &str,
    attributes: &impl graphicle::record::Properties,
) -> Option<(String, String)> {
    match relation {
        "defined_in" => Some((source.to_string(), target.to_string())),
        "contains" => Some((target.to_string(), source.to_string())),
        "inherits" | "extends" => {
            let file = attributes.get("source_file").and_then(Value::as_str)?;
            let extension = std::path::Path::new(file)
                .extension()
                .and_then(|e| e.to_str())?
                .to_ascii_lowercase();
            SINGLE_INHERITANCE
                .contains(&extension.as_str())
                .then(|| (source.to_string(), target.to_string()))
        }
        _ => None,
    }
}

/// Interface-like declarations may extend several parents even in
/// single-inheritance languages; their `inherits` edges are not one-valued.
fn interface_like(graph: &Graph, node_id: &str) -> bool {
    graph
        .node_index(&json!(node_id))
        .and_then(|index| graph.nodes()[index].attributes.get("label"))
        .and_then(Value::as_str)
        .is_some_and(|label| {
            let label = label.to_ascii_lowercase();
            ["interface", "protocol", "trait", "mixin"]
                .iter()
                .any(|kind| label.contains(kind))
        })
}

/// A closed edge of a cardinality-one relation whose subject now points at
/// exactly one different target is a contradiction, not a disappearance.
fn record_contradiction(
    db: &Connection,
    repository: &str,
    episode: &str,
    stamp: &str,
    graph: &Graph,
    single_now: &HashMap<(String, String), Vec<(String, i64)>>,
    closed: (&str, &str, &str, i64, &str),
) -> Result<()> {
    let (source, target, relation, old_row_id, attributes) = closed;
    let attributes: Map<String, Value> = serde_json::from_str(attributes).unwrap_or_default();
    let Some((subject, old_other)) = single_valued(relation, source, target, &attributes) else {
        return Ok(());
    };
    let Some(current) = single_now.get(&(relation.to_string(), subject.clone())) else {
        return Ok(());
    };
    // Several targets in the new graph mean the relation is not one-valued
    // for this subject in practice; nothing is marked.
    let [(new_other, new_row_id)] = current.as_slice() else {
        return Ok(());
    };
    if *new_other == old_other || interface_like(graph, &subject) {
        return Ok(());
    }
    // Node ids are not durable identity: a file node can be re-keyed by an
    // unrelated change (`base` ↔ `base_java_base`). The same entity under a
    // new id is a re-identification, never a contradiction.
    let new_stable = graph
        .node_index(&json!(new_other))
        .and_then(|index| graph.nodes()[index].attributes.get("stable_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let old_stable = one(
        db,
        "SELECT json_extract(attributes,'$.stable_id') AS stable_id FROM gf_nodes WHERE repository_id=? AND node_id=? ORDER BY id DESC LIMIT 1",
        params![repository, old_other],
    )?
    .and_then(|row| row["stable_id"].as_str().map(str::to_string));
    if let (Some(new_stable), Some(old_stable)) = (&new_stable, &old_stable)
        && new_stable == old_stable
    {
        return Ok(());
    }
    db.execute(
        "INSERT INTO gf_contradictions(repository_id,episode,relation,subject,old_target,new_target,old_edge_row_id,new_edge_row_id,recorded_at) VALUES(?,?,?,?,?,?,?,?,?)",
        params![
            repository,
            episode,
            relation,
            subject,
            old_other,
            new_other,
            old_row_id,
            new_row_id,
            stamp
        ],
    )?;
    Ok(())
}

/// Contradiction markers recorded for `episode` (or every episode when `None`).
pub fn contradictions(
    db: &Connection,
    repository: &str,
    episode: Option<&str>,
) -> Result<Vec<Value>> {
    match episode {
        Some(episode) => rows(
            db,
            "SELECT id,episode,relation,subject,old_target,new_target,old_edge_row_id,new_edge_row_id,recorded_at FROM gf_contradictions WHERE repository_id=? AND episode=? ORDER BY id",
            params![repository, episode],
        ),
        None => rows(
            db,
            "SELECT id,episode,relation,subject,old_target,new_target,old_edge_row_id,new_edge_row_id,recorded_at FROM gf_contradictions WHERE repository_id=? ORDER BY id",
            [repository],
        ),
    }
}

/// Reconstruct the graph exactly as it was at `episode`.
pub fn load(db: &Connection, repository: &str, episode: &str) -> Result<Graph> {
    let row = episode_row(db, repository, episode)?
        .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
    let sequence = row["sequence"].as_i64().unwrap_or(0);
    let multigraph = row["multigraph"] != 0;
    let mut attributes: Value = serde_json::from_str(s(&row, "attributes"))?;
    if let Value::Object(fields) = &mut attributes {
        fields.remove(EXTRACTION_RUN);
        fields.remove(SOURCE_HEADS);
        if fields.remove("unresolved_calls_count").is_some() {
            fields.insert(
                UNRESOLVED_CALLS.into(),
                Value::Array(unresolved_calls(db, repository, episode)?),
            );
        }
    }
    // As-of filter resolved in memory: the episode table is small, and the
    // per-row join against it cost two index lookups for every node and edge.
    let mut sequences: HashMap<String, i64> = HashMap::new();
    {
        let mut select =
            db.prepare_cached("SELECT episode,sequence FROM gf_episodes WHERE repository_id=?")?;
        let mut episodes = select.query([repository])?;
        while let Some(episode) = episodes.next()? {
            sequences.insert(episode.get(0)?, episode.get(1)?);
        }
    }
    let as_of = |from: &str, to: Option<&str>| -> bool {
        sequences.get(from).is_some_and(|&f| f <= sequence)
            && to.is_none_or(|to| sequences.get(to).is_some_and(|&t| t > sequence))
    };
    // Rows stream straight into the graph: no node-link document in between.
    let mut graph = Graph::with_capacity(
        row["directed"] != 0,
        multigraph,
        row["node_count"].as_u64().unwrap_or(0) as usize,
        row["edge_count"].as_u64().unwrap_or(0) as usize,
    );
    *graph.attributes_mut() = if attributes.is_object() {
        attributes
    } else {
        json!({})
    };
    let mut select = db.prepare_cached(
        "SELECT node_id,attributes,valid_from_episode,valid_to_episode FROM gf_nodes WHERE repository_id=? ORDER BY id",
    )?;
    let mut nodes = select.query([repository])?;
    while let Some(node) = nodes.next()? {
        if !as_of(column_str(node, 2)?, column_opt_str(node, 3)?) {
            continue;
        }
        let mut fields: Map<String, Value> = serde_json::from_str(column_str(node, 1)?)?;
        fields.shift_remove("id");
        graph.add_node(json!(column_str(node, 0)?), fields)?;
    }
    drop(nodes);
    let mut select = db.prepare_cached(
        "SELECT source,target,key,attributes,valid_from_episode,valid_to_episode FROM gf_edges WHERE repository_id=? ORDER BY id",
    )?;
    let mut edges = select.query([repository])?;
    while let Some(edge) = edges.next()? {
        if !as_of(column_str(edge, 4)?, column_opt_str(edge, 5)?) {
            continue;
        }
        let mut fields: Map<String, Value> = serde_json::from_str(column_str(edge, 3)?)?;
        fields.shift_remove("source");
        fields.shift_remove("target");
        let key = if multigraph {
            fields.shift_remove("key");
            Some(edge_key_value(column_str(edge, 2)?))
        } else {
            None
        };
        graph.add_edge(
            json!(column_str(edge, 0)?),
            json!(column_str(edge, 1)?),
            key,
            fields,
        )?;
    }
    Ok(graph)
}

/// The resident handle for a recorded episode, reopened from the tables when
/// the registry evicted it.
pub fn resident(db: &Connection, repository: &str, episode: &str) -> Result<GraphHandle> {
    let reference = graph_ref(repository, episode);
    if let Ok(handle) = registry().get(&reference) {
        return Ok(handle);
    }
    let handle = GraphHandle::new(load(db, repository, episode)?);
    registry().open(handle.clone(), Some(&reference));
    Ok(handle)
}

pub fn episodes(db: &Connection, repository: &str) -> Result<Vec<Value>> {
    let mut episodes = rows(
        db,
        "SELECT episode,sequence,created_at,node_count,edge_count,attributes,parent_episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence",
        [repository],
    )?;
    for episode in &mut episodes {
        let attributes: Value =
            serde_json::from_str(s(episode, "attributes")).unwrap_or(Value::Null);
        let extraction = attributes
            .get(EXTRACTION_RUN)
            .cloned()
            .unwrap_or(Value::Null);
        if let Value::Object(fields) = episode {
            fields.remove("attributes");
            fields.insert("extraction".into(), extraction);
        }
    }
    Ok(episodes)
}

/// Graph attribute holding the receiver-resolution leftovers; stored in
/// `gf_unresolved_calls` (one row per call, closed when the next episode is
/// recorded) instead of the episode's attribute blob so retention can drop it.
pub const UNRESOLVED_CALLS: &str = "unresolved_calls";

fn episode_attributes(graph: &Graph, extraction: Option<&Value>) -> Value {
    let mut attributes = graph.attributes().clone();
    if let Value::Object(fields) = &mut attributes {
        if let Some(calls) = fields.remove(UNRESOLVED_CALLS) {
            let count = calls.as_array().map_or(0, Vec::len);
            fields.insert("unresolved_calls_count".into(), json!(count));
        }
        if let Some(extraction) = extraction {
            fields.insert(EXTRACTION_RUN.into(), extraction.clone());
        }
    }
    attributes
}

/// Write the episode's `unresolved_calls` rows and close the rows of the
/// previous episode (`valid_to_episode`), so only current + previous stay
/// open for the report's receiver check and retention.
fn persist_unresolved_calls(
    db: &Connection,
    repository: &str,
    episode: &str,
    graph: &Graph,
    stamp: &str,
) -> Result<()> {
    db.execute(
        "UPDATE gf_unresolved_calls SET valid_to_episode=?2 WHERE repository_id=?1 AND valid_to_episode IS NULL AND episode<>?2",
        params![repository, episode],
    )?;
    let Some(calls) = graph.attributes()[UNRESOLVED_CALLS].as_array() else {
        return Ok(());
    };
    let mut insert = db.prepare_cached(
        "INSERT INTO gf_unresolved_calls(repository_id,episode,caller,callee,call,created_at) VALUES(?,?,?,?,?,?)",
    )?;
    for call in calls {
        let caller = call["caller"].as_str().unwrap_or("");
        let callee = call["callee"].as_str().unwrap_or("");
        insert.execute(params![
            repository,
            episode,
            caller,
            callee,
            graphicle_memory::text::canonical(call),
            stamp,
        ])?;
    }
    Ok(())
}

/// The episode's `unresolved_calls` as stored in `gf_unresolved_calls`
/// (empty once retention has dropped them).
pub fn unresolved_calls(db: &Connection, repository: &str, episode: &str) -> Result<Vec<Value>> {
    rows(
        db,
        "SELECT call FROM gf_unresolved_calls WHERE repository_id=? AND episode=? ORDER BY id",
        params![repository, episode],
    )?
    .iter()
    .map(|row| Ok(serde_json::from_str(s(row, "call"))?))
    .collect()
}

/// Attach `label` (a commit sha, "main", "base", ...) to a recorded episode.
/// Re-labelling moves the label; labelling the same episode again is a no-op.
pub fn label_episode(
    db: &Connection,
    workspace: &str,
    repository: &str,
    label: &str,
    episode: &str,
) -> Result<()> {
    if label.is_empty() || label.len() > 128 || label == "latest" || label == "working" {
        return Err(Error::bad("Invalid episode label"));
    }
    if episode_row(db, repository, episode)?.is_none() {
        return Err(Error::bad("Unknown graph episode for this repository"));
    }
    if let Some(verified) = one(
        db,
        "SELECT episode FROM gf_commits WHERE repository_id=? AND oid=?",
        params![repository, label],
    )? && s(&verified, "episode") != episode
    {
        return Err(Error::conflict(
            "A verified Git commit cannot be relabelled to a different episode",
        ));
    }
    db.execute(
        "INSERT INTO gf_episode_labels(workspace,repo,label,episode_id,created_at) VALUES(?,?,?,?,?) \
         ON CONFLICT(workspace,repo,label) DO UPDATE SET episode_id=excluded.episode_id,created_at=excluded.created_at",
        params![workspace, repository, label, episode, now()],
    )?;
    Ok(())
}

pub fn labels(db: &Connection, workspace: &str, repository: &str) -> Result<Vec<Value>> {
    rows(
        db,
        "SELECT label,episode_id,created_at FROM gf_episode_labels WHERE workspace=? AND repo=? ORDER BY label",
        params![workspace, repository],
    )
}

/// Resolve an episode selector: a recorded episode id, a label, or
/// `latest` (most recently recorded) / `working` (the current head set,
/// which must already be recorded).
pub fn resolve_episode(
    db: &Connection,
    workspace: &str,
    repository: &str,
    selector: &str,
) -> Result<String> {
    if selector.is_empty() || selector.len() > 128 {
        return Err(Error::bad("Invalid episode selector"));
    }
    match selector {
        "working" => {
            let current = current_episode(db, repository)?;
            if episode_row(db, repository, &current)?.is_none() {
                return Err(Error::bad(
                    "The working head set has not been projected yet",
                ));
            }
            return Ok(current);
        }
        "latest" => {
            return one(
                db,
                "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
                [repository],
            )?
            .map(|row| s(&row, "episode").to_string())
            .ok_or_else(|| Error::bad("No graph episode recorded for this repository"));
        }
        _ => {}
    }
    if episode_row(db, repository, selector)?.is_some() {
        return Ok(selector.to_string());
    }
    one(
        db,
        "SELECT episode_id FROM gf_episode_labels WHERE workspace=? AND repo=? AND label=?",
        params![workspace, repository, selector],
    )?
    .map(|row| s(&row, "episode_id").to_string())
    .ok_or_else(|| Error::bad("Unknown episode or label for this repository"))
}

fn info(
    db: &Connection,
    repository: &str,
    episode: &str,
    replayed: bool,
) -> Result<ProjectionInfo> {
    let row = episode_row(db, repository, episode)?
        .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
    Ok(ProjectionInfo {
        episode: episode.to_string(),
        nodes: row["node_count"].as_u64().unwrap_or(0) as usize,
        edges: row["edge_count"].as_u64().unwrap_or(0) as usize,
        graph_ref: graph_ref(repository, episode),
        replayed,
    })
}

/// Whether the current head set's episode is recorded; if so, its graph is a
/// projection of exactly the files in the store, so no graph node is stale:
/// clear the repository's `graph_node` markers. A sync leaves markers that no
/// new projection would clear when its head set was already projected (a
/// file edited and then restored) or when a projection job replays a
/// recorded episode. Returns how many markers were cleared.
pub fn clear_stale_when_current(db: &Connection, repository: &str) -> Result<usize> {
    let episode = current_episode(db, repository)?;
    if episode_row(db, repository, &episode)?.is_none() {
        return Ok(0);
    }
    let workspace = crate::invalidation::workspace_of(db, repository)?;
    crate::invalidation::clear_stale_prefix(
        db,
        &workspace,
        crate::invalidation::GRAPH_NODE,
        &format!("{repository}:"),
    )
}

/// Build, persist and open the current head set inside one authorized transaction.
pub fn project(
    db: &Connection,
    repository: &str,
    _principal: &Principal,
) -> Result<ProjectionInfo> {
    let episode = current_episode(db, repository)?;
    if episode_row(db, repository, &episode)?.is_some() {
        resident(db, repository, &episode)?;
        // A replayed head set's symbols become the live ones.
        crate::symbols::ensure(db, repository, &episode)?;
        clear_stale_when_current(db, repository)?;
        return info(db, repository, &episode, true);
    }
    let (episode, graph, extraction) =
        crate::phase::phase("graph_build", || build_graph_with_report(db, repository))?;
    crate::phase::phase("graph_persist", || {
        persist_with_report(db, repository, &episode, &graph, Some(&extraction))
    })?;
    registry().open(
        GraphHandle::new(graph),
        Some(&graph_ref(repository, &episode)),
    );
    info(db, repository, &episode, false)
}

/// A graph built outside any write transaction: `(episode, graph, report)`.
pub type Built = (String, Graph, Value);

/// Phase one of a projection, for a read-only snapshot: `None` when the
/// current head set is already recorded (and now resident), else the built
/// graph. Extraction reads blobs and a file cache only, never SQLite writes,
/// so it must not run while holding the write lock: a large repository takes
/// longer than the store's busy timeout.
pub fn prebuild(db: &Connection, repository: &str) -> Result<Option<Built>> {
    let episode = current_episode(db, repository)?;
    if episode_row(db, repository, &episode)?.is_some() {
        resident(db, repository, &episode)?;
        return Ok(None);
    }
    Ok(Some(crate::phase::phase("graph_build", || {
        build_graph_with_report(db, repository)
    })?))
}

/// Phase two, inside the write transaction: record a graph from `prebuild`
/// when it still matches the current head set and nobody recorded it first.
/// If the head set moved in between, build here rather than record a stale
/// graph (the rare race keeps the old single-phase behaviour).
pub fn project_prebuilt(
    db: &Connection,
    repository: &str,
    principal: &Principal,
    built: Option<Built>,
) -> Result<ProjectionInfo> {
    let episode = current_episode(db, repository)?;
    if episode_row(db, repository, &episode)?.is_some() {
        resident(db, repository, &episode)?;
        crate::symbols::ensure(db, repository, &episode)?;
        clear_stale_when_current(db, repository)?;
        return info(db, repository, &episode, true);
    }
    match built {
        Some((built_episode, graph, extraction)) if built_episode == episode => {
            crate::phase::phase("graph_persist", || {
                persist_with_report(db, repository, &episode, &graph, Some(&extraction))
            })?;
            registry().open(
                GraphHandle::new(graph),
                Some(&graph_ref(repository, &episode)),
            );
            info(db, repository, &episode, false)
        }
        _ => project(db, repository, principal),
    }
}

/// Read the head set under the caller's authority, build outside SQLite, then
/// persist if the head set is still the same and nobody recorded it first.
pub fn project_store(
    store: &Store,
    workspace: &str,
    token: &str,
    repository: &str,
) -> Result<ProjectionInfo> {
    let (identifier, built) = store.authorized_read(workspace, token, "read", |db, p| {
        let repository = repo(db, p, repository)?;
        let identifier = s(&repository, "id").to_string();
        let episode = current_episode(db, &identifier)?;
        if episode_row(db, &identifier, &episode)?.is_some() {
            resident(db, &identifier, &episode)?;
            return Ok((identifier, None));
        }
        let built =
            crate::phase::phase("graph_build", || build_graph_with_report(db, &identifier))?;
        Ok((identifier, Some(built)))
    })?;
    store.authorized(workspace, token, "read", |db, p| {
        let repository = repo(db, p, &identifier)?;
        let identifier = s(&repository, "id");
        let current = current_episode(db, identifier)?;
        match built {
            Some((episode, graph, extraction)) if episode == current => {
                let recorded = crate::phase::phase("graph_persist", || {
                    persist_with_report(db, identifier, &episode, &graph, Some(&extraction))
                })?;
                if recorded {
                    registry().open(
                        GraphHandle::new(graph),
                        Some(&graph_ref(identifier, &episode)),
                    );
                } else {
                    resident(db, identifier, &episode)?;
                    clear_stale_when_current(db, identifier)?;
                }
                info(db, identifier, &episode, !recorded)
            }
            Some(_) => Err(Error::conflict("Repository changed while projecting")),
            None => {
                crate::symbols::ensure(db, identifier, &current)?;
                clear_stale_when_current(db, identifier)?;
                info(db, identifier, &current, true)
            }
        }
    })
}

impl Store {
    /// Episodes and labels of a repository, for the `graph_episodes` MCP tool.
    pub fn episode_listing(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        let mut listing = self.graph_episodes(workspace, token, repository)?;
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            listing["repo"] = json!(s(&repository, "name"));
            listing["count"] = json!(episodes(db, s(&repository, "id"))?.len());
            Ok(())
        })?;
        Ok(listing)
    }

    pub fn graph_project(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        Ok(json!(project_store(self, workspace, token, repository)?))
    }
    pub fn graph_episodes(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            let mut listed = episodes(db, identifier)?;
            let labels = labels(db, workspace, identifier)?;
            for episode in &mut listed {
                let mine: Vec<Value> = labels
                    .iter()
                    .filter(|l| l["episode_id"] == episode["episode"])
                    .map(|l| l["label"].clone())
                    .collect();
                episode["labels"] = json!(mine);
            }
            Ok(json!({
                "current_episode": current_episode(db, identifier)?,
                "episodes": listed,
                "labels": labels,
            }))
        })
    }
    /// Label a recorded episode (`episode` may itself be a selector).
    pub fn graph_label(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
        label: &str,
        episode: &str,
    ) -> Result<Value> {
        self.authorized(workspace, token, "collect", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            let resolved = resolve_episode(db, workspace, identifier, episode)?;
            label_episode(db, workspace, identifier, label, &resolved)?;
            Ok(json!({"label": label, "episode": resolved}))
        })
    }
    /// The typed graph as it stood at an episode selector (`as_of`: episode
    /// id, commit-sha label, `latest`, `working`), reconstructed from the
    /// bi-temporal tables without re-extraction. Optional `node` (exact id),
    /// `query` (substring of node id) and `relations` narrow the view;
    /// `limit` bounds nodes. Edges are those between returned nodes.
    pub fn graph_as_of(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
        args: &Value,
    ) -> Result<Value> {
        if !args.is_object() {
            return Err(Error::bad("as_of arguments must be an object"));
        }
        let selector = match args.get("as_of") {
            None | Some(Value::Null) => "latest".to_string(),
            Some(_) => string(args, "as_of", 128)?.to_string(),
        };
        let limit = crate::integer(args, "limit", 200, 2000)?.max(1);
        let edge_limit = crate::integer(args, "edge_limit", 10000, 100000)?.max(1);
        let query = args["query"].as_str().unwrap_or("").to_lowercase();
        let node = args["node"].as_str().filter(|n| !n.is_empty());
        let relations: Option<Vec<String>> = match args.get("relations").filter(|v| !v.is_null()) {
            None => None,
            Some(list) => Some(
                list.as_array()
                    .filter(|l| l.len() <= 64)
                    .ok_or_else(|| Error::bad("Expected relations array"))?
                    .iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| Error::bad("Expected relation names"))?,
            ),
        };
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            let view = crate::temporal_graph::select(db, workspace, identifier, args)?;
            let episode = view.episode;
            let row = episode_row(db, identifier, &episode)?
                .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
            let mut graph = resident(db, identifier, &episode)?;
            if args["canonical"] == true {
                let aliases = crate::temporal_graph::aliases_at(db, identifier, view.valid, view.known)?;
                let mut owned = graph.into_graph();
                graphicle_memory::graph::canonical::apply(&mut owned, &aliases)?;
                graph = GraphHandle::new(owned);
            }
            let nodes = graph.nodes();
            let text_of = |value: &Value| text(value);
            let keep_edge = |relation: &str| {
                relations
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|r| r == relation))
            };
            let mut selected: Vec<usize> = Vec::new();
            if let Some(node) = node {
                let index = graph
                    .node_index(&json!(node))
                    .ok_or_else(|| Error::bad("Node unavailable at this episode"))?;
                selected.push(index);
                for (source, target, edge) in graph.iter_edges() {
                    let relation = edge.attributes.get("relation").map(text).unwrap_or_default();
                    if (source == index || target == index) && keep_edge(&relation) {
                        selected.push(if source == index { target } else { source });
                    }
                }
                selected.sort_unstable();
                selected.dedup();
                // Keep the requested anchor even when its neighbor list is truncated.
                selected.retain(|candidate| *candidate != index);
                selected.insert(0, index);
            } else {
                selected = (0..nodes.len())
                    .filter(|&i| query.is_empty() || text_of(&nodes[i].id).to_lowercase().contains(&query))
                    .collect();
            }
            let truncated = selected.len() > limit;
            selected.truncate(limit);
            let chosen: std::collections::HashSet<usize> = selected.iter().copied().collect();
            let listed: Vec<Value> = selected
                .iter()
                .map(|&i| {
                    let mut fields = nodes[i].attributes.clone();
                    fields.insert("id".into(), nodes[i].id.clone());
                    Value::Object(fields.into())
                })
                .collect();
            let mut edges = Vec::new();
            let mut edges_truncated = false;
            for (source, target, edge) in graph.iter_edges() {
                if !chosen.contains(&source) || !chosen.contains(&target) {
                    continue;
                }
                let relation = edge.attributes.get("relation").map(text).unwrap_or_default();
                if !keep_edge(&relation) {
                    continue;
                }
                if edges.len() >= edge_limit {
                    edges_truncated = true;
                    continue;
                }
                let mut fields = edge.attributes.clone();
                fields.insert("source".into(), nodes[source].id.clone());
                fields.insert("target".into(), nodes[target].id.clone());
                fields.insert("key".into(), edge.key.to_value());
                edges.push(Value::Object(fields.into()));
            }
            let labels: Vec<Value> = labels(db, workspace, identifier)?
                .into_iter()
                .filter(|l| l["episode_id"] == json!(episode))
                .map(|l| l["label"].clone())
                .collect();
            let (hyperedges,hyperedges_truncated)=crate::temporal_graph::hyperedges_at(db, identifier, view.valid, view.known, &graph)?;
            Ok(json!({
                "as_of": if view.metadata["selection"]=="episode" {json!(selector)} else {Value::Null},
                "temporal": view.metadata,
                "episode": episode,
                "sequence": row["sequence"],
                "parent_episode": row["parent_episode"],
                "created_at": row["created_at"],
                "labels": labels,
                "node_count": graph.node_count(),
                "edge_count": graph.edge_count(),
                "canonical": args["canonical"] == true,
                "multigraph": row["multigraph"] != 0,
                "nodes": listed,
                "edges": edges,
                "edges_truncated": edges_truncated,
                "truncated": truncated,
                "contradictions": contradictions(db, identifier, Some(&episode))?,
                "hyperedges": hyperedges,"hyperedges_truncated":hyperedges_truncated,
                "freshness": crate::invalidation::repository_freshness(db, &p.workspace, identifier)?,
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// Cross-repository import linkage. Every repository of the
// workspace is indexed on its own; `link_workspace` resolves imports between
// the resident graphs by package identity and persists the result with both
// episodes in `gf_cross_edges` / `gf_cross_unresolved`.
// ---------------------------------------------------------------------------

use graphicle_memory::graph::cross_repo_imports::{self as xrepo, ExportIndex, PackageManifest};

/// `via` marker on cross-repository report items.
pub const VIA_CROSS_REPO_IMPORT: &str = "cross_repo_import";
const MANIFEST_FILES: &[&str] = &["pyproject.toml", "setup.cfg", "package.json", "go.mod"];
const MANIFEST_SKIP_DIRS: &[&str] = &["node_modules/", "vendor/", ".venv/", "dist/", "build/"];

/// Repository-level read access. Workspace membership grants access to every
/// repository unless an explicit `revoked` row in `gf_repo_access` names the
/// principal's credential; the workspace owner always reads everything. A
/// PR scratch project linked to a folder project is readable only while the
/// folder project is. An installed extension may narrow the repositories of
/// a credential (`extension::Access::Repository`).
pub fn repo_readable(db: &Connection, p: &Principal, repository: &str) -> Result<bool> {
    if p.role == "owner" {
        return Ok(true);
    }
    // A PR scratch project holds copies of its folder project's code:
    // it is readable only while that project is.
    let revoked = one(
        db,
        "SELECT 1 AS revoked FROM gf_repo_access WHERE credential_id=?1 AND state='revoked' \
         AND (repository_id=?2 OR repository_id IN (SELECT project_id FROM repo_pr_scratch WHERE repository_id=?2))",
        params![p.credential, repository],
    )?;
    if revoked.is_some() {
        return Ok(false);
    }
    // An installed extension may narrow the credential's repositories. A PR
    // scratch project is asked about as its folder project, whose code it
    // holds.
    let extensions = crate::extension::installed();
    if extensions.is_empty() {
        return Ok(true);
    }
    let scope = crate::folders::scratch_of(db, repository)?;
    Ok(extensions
        .authorize(
            db,
            p,
            &mut crate::extension::AccessRequest::new(crate::extension::Access::Repository(
                scope.as_deref().unwrap_or(repository),
            )),
        )
        .is_ok())
}

/// Record `granted` / `revoked` repository access for a credential.
pub fn set_repo_access(
    db: &Connection,
    workspace: &str,
    repository: &str,
    credential: &str,
    state: &str,
) -> Result<()> {
    if !["granted", "revoked"].contains(&state) {
        return Err(Error::bad("Expected state granted or revoked"));
    }
    db.execute(
        "INSERT INTO gf_repo_access(workspace_id,repository_id,credential_id,state,created_at) \
         VALUES(?,?,?,?,?) ON CONFLICT(repository_id,credential_id) DO UPDATE SET state=excluded.state,created_at=excluded.created_at",
        params![workspace, repository, credential, state, now()],
    )?;
    Ok(())
}

fn blob(db: &Connection, repository: &str, sha: &str) -> Result<Option<String>> {
    Ok(one(
        db,
        "SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?",
        params![repository, sha],
    )?
    .map(|row| s(&row, "content").to_string()))
}

fn is_manifest(relpath: &str) -> bool {
    let name = relpath.rsplit('/').next().unwrap_or(relpath);
    MANIFEST_FILES.contains(&name)
        && !MANIFEST_SKIP_DIRS
            .iter()
            .any(|dir| relpath.starts_with(dir) || relpath.contains(&format!("/{dir}")))
}

/// Package manifests of the repository's current head set.
pub fn manifests(
    db: &Connection,
    repository: &str,
    heads: &BTreeMap<String, String>,
) -> Result<Vec<PackageManifest>> {
    let mut out = Vec::new();
    for (relpath, sha) in heads {
        if !is_manifest(relpath) {
            continue;
        }
        if let Some(text) = blob(db, repository, sha)? {
            out.push(PackageManifest::new(relpath.clone(), text));
        }
    }
    Ok(out)
}

/// The episode a repository takes part in linking with: the current head
/// set when it is recorded, else the latest recorded episode. `None` when
/// the repository has never been projected.
fn linkable_episode(db: &Connection, repository: &str) -> Result<Option<(String, bool)>> {
    let current = current_episode(db, repository)?;
    if episode_row(db, repository, &current)?.is_some() {
        return Ok(Some((current, true)));
    }
    Ok(one(
        db,
        "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
        [repository],
    )?
    .map(|row| (s(&row, "episode").to_string(), false)))
}

struct Linkable {
    id: String,
    name: String,
    episode: String,
    /// Whether `episode` is the current head set (sources then match the graph).
    current: bool,
    /// The resident graph, once this pass needed it: to build the export
    /// index, or to link the project's own imports.
    graph: Option<GraphHandle>,
    heads: BTreeMap<String, String>,
    manifests: Vec<PackageManifest>,
    index: ExportIndex,
    /// The export index as built from the graph, before the nested-project
    /// filter, when this pass built it (it is then stored for later passes).
    built: Option<String>,
}

/// Version of the stored link state (`gf_cross_link_state`). Another value,
/// as after an upgrade, rebuilds every export index and relinks every
/// project once.
pub const LINK_STATE_FORMAT: &str = concat!("xrepo-link-1/", env!("CARGO_PKG_VERSION"));

/// The export index stored for `repository` at `episode`, if any.
fn stored_export_index(
    db: &Connection,
    repository: &str,
    episode: &str,
) -> Result<Option<ExportIndex>> {
    let Some(row) = one(
        db,
        "SELECT export_index FROM gf_cross_link_state WHERE repository_id=? AND episode=? AND format=?",
        params![repository, episode, LINK_STATE_FORMAT],
    )?
    else {
        return Ok(None);
    };
    Ok(serde_json::from_str(s(&row, "export_index")).ok())
}

/// What other projects can import from `index`: its digest without the
/// episode, so a new episode that exports the same modules and symbols
/// (same node ids) has the same surface.
fn export_surface(index: &ExportIndex) -> String {
    let mut value = json!(index);
    if let Some(map) = value.as_object_mut() {
        map.remove("episode");
    }
    digest(&value)
}

/// The live link rows of `repository` as its import report: what its last
/// link wrote. Only the persisted fields are filled; the persistence of
/// [`link_workspace`] reads nothing else.
fn previous_report(db: &Connection, repository: &str, episode: &str) -> Result<xrepo::LinkReport> {
    let text = |row: &Value, field: &str| row[field].as_str().map(str::to_string);
    let line = |row: &Value| row["line"].as_u64().map(|line| line as usize);
    let edges = rows(
        db,
        "SELECT * FROM gf_cross_edges WHERE source_repository_id=? AND valid_to_episode IS NULL ORDER BY id",
        [repository],
    )?
    .iter()
    .map(|row| xrepo::CrossEdge {
        source: xrepo::EdgeEnd {
            repo: repository.to_string(),
            node_id: s(row, "source_node_id").to_string(),
            stable_id: text(row, "source_stable_id"),
            ..Default::default()
        },
        target: xrepo::EdgeEnd {
            repo: s(row, "target_repository_id").to_string(),
            node_id: s(row, "target_node_id").to_string(),
            stable_id: text(row, "target_stable_id"),
            ..Default::default()
        },
        relation: s(row, "relation").to_string(),
        confidence: s(row, "confidence").to_string(),
        derivation: s(row, "derivation").to_string(),
        module_path: s(row, "module_path").to_string(),
        symbol: text(row, "symbol"),
        version_constraint: text(row, "version_constraint"),
        exporter_version: text(row, "exporter_version"),
        version_status: s(row, "version_status").to_string(),
        line: line(row),
    })
    .collect();
    let unresolved = rows(
        db,
        "SELECT * FROM gf_cross_unresolved WHERE source_repository_id=? AND valid_to_episode IS NULL ORDER BY id",
        [repository],
    )?
    .iter()
    .map(|row| {
        let relpath = text(row, "relpath");
        xrepo::UnresolvedImport {
            repo: repository.to_string(),
            node_id: s(row, "source_node_id").to_string(),
            stable_id: text(row, "source_stable_id"),
            // Not stored; the persistence does not read it.
            ecosystem: relpath
                .as_deref()
                .and_then(xrepo::Ecosystem::of_file)
                .unwrap_or(xrepo::Ecosystem::Python),
            relpath,
            module_path: s(row, "module_path").to_string(),
            symbol: text(row, "symbol"),
            package: s(row, "package").to_string(),
            reason: s(row, "reason").to_string(),
            candidates: serde_json::from_str(s(row, "candidates")).unwrap_or_default(),
            version_constraint: text(row, "version_constraint"),
            exporter_version: text(row, "exporter_version"),
            line: line(row),
        }
    })
    .collect();
    Ok(xrepo::LinkReport {
        repo: repository.to_string(),
        episode: Some(episode.to_string()),
        edges,
        unresolved,
        ..Default::default()
    })
}

fn live_pair_episodes(
    db: &Connection,
    source: &str,
    target: &str,
) -> Result<Vec<(String, String)>> {
    Ok(rows(
        db,
        "SELECT DISTINCT source_episode,target_episode FROM gf_cross_edges \
         WHERE source_repository_id=? AND target_repository_id=? AND valid_to_episode IS NULL",
        params![source, target],
    )?
    .iter()
    .map(|r| {
        (
            s(r, "source_episode").to_string(),
            s(r, "target_episode").to_string(),
        )
    })
    .collect())
}

fn same_cross_edges(
    db: &Connection,
    source: &str,
    target: &str,
    edges: &[&xrepo::CrossEdge],
) -> Result<bool> {
    const FIELDS: &[&str] = &[
        "source_node_id",
        "source_stable_id",
        "target_node_id",
        "target_stable_id",
        "relation",
        "confidence",
        "derivation",
        "module_path",
        "symbol",
        "line",
        "version_constraint",
        "exporter_version",
        "version_status",
    ];
    let mut previous: Vec<String> = rows(db,
        "SELECT source_node_id,source_stable_id,target_node_id,target_stable_id,relation,confidence,derivation,\
         module_path,symbol,line,version_constraint,exporter_version,version_status FROM gf_cross_edges \
         WHERE source_repository_id=? AND target_repository_id=? AND valid_to_episode IS NULL",
        params![source, target],
    )?.iter().map(|row| json!(FIELDS.iter().map(|field| &row[*field]).collect::<Vec<_>>()).to_string()).collect();
    let mut next: Vec<String> = edges
        .iter()
        .map(|edge| {
            json!([
                edge.source.node_id,
                edge.source.stable_id,
                edge.target.node_id,
                edge.target.stable_id,
                edge.relation,
                edge.confidence,
                edge.derivation,
                edge.module_path,
                edge.symbol,
                edge.line,
                edge.version_constraint,
                edge.exporter_version,
                edge.version_status,
            ])
            .to_string()
        })
        .collect();
    previous.sort_unstable();
    next.sort_unstable();
    Ok(previous == next)
}

fn record_packages(db: &Connection, workspace: &str, repo: &Linkable) -> Result<()> {
    for package in &repo.index.packages {
        db.execute(
            "INSERT OR REPLACE INTO gf_cross_packages(workspace_id,repository_id,episode,ecosystem,name,version,created_at) VALUES(?,?,?,?,?,?,?)",
            params![
                workspace,
                repo.id,
                repo.episode,
                package.ecosystem.as_str(),
                package.name,
                package.version,
                now()
            ],
        )?;
    }
    Ok(())
}

/// Export indexes by `(repository, episode)`: an episode's graph never
/// changes, so its index is computed once per process and reused by every
/// later link run (a small bounded map; the newest entries win).
fn export_index(
    graph: &Graph,
    repository: &str,
    episode: &str,
    manifests: &[PackageManifest],
) -> ExportIndex {
    static INDEXES: Mutex<Option<HashMap<(String, String), ExportIndex>>> = Mutex::new(None);
    const CAPACITY: usize = 64;
    let key = (repository.to_string(), episode.to_string());
    if let Some(index) = INDEXES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|cache| cache.get(&key))
    {
        return index.clone();
    }
    let index = xrepo::exports(graph, repository, Some(episode), manifests);
    let mut guard = INDEXES.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = guard.get_or_insert_with(HashMap::new);
    if cache.len() >= CAPACITY {
        cache.clear();
    }
    cache.insert(key, index.clone());
    index
}

/// Link the readable, projected repositories of the principal's workspace
/// against each other and persist the result. A `pr_scratch` project (a PR
/// copy of a checkout, [`PR_SCRATCH`]) takes part only while no other
/// linkable project, and no newer `pr_scratch` project, exports one of its
/// import roots; when one does, the copy is skipped, its live cross rows are
/// closed, and the summary lists it under `pr_scratch_skipped`. A project
/// synced from a folder inside another project's folder owns the packages
/// under it. Idempotent for unchanged episode pairs; when either
/// side moved to a new episode the stale rows are closed with
/// `valid_to_episode` (the new episode of the side that changed) and the pair
/// is re-linked.
///
/// The pass is incremental. A project's own imports are linked again only
/// when its episode, the export surface of another taking part (what it
/// exports, without the episode) or the set of projects taking part changed
/// since its last link, or when it no longer has as many live rows as that
/// link left (rows are only ever closed or deleted, never edited in place);
/// otherwise its live rows stand for the link and go through
/// the same persistence, so the rows are those of a full pass
/// ([`link_workspace_full`]). Export indexes are stored per project
/// (`gf_cross_link_state`), so an unchanged project's graph is not loaded.
/// The summary lists the projects linked again (`relinked`) and those whose
/// links were reused (`reused`).
pub fn link_workspace(db: &Connection, p: &Principal) -> Result<Value> {
    link_workspace_with(db, p, true)
}

/// [`link_workspace`] that rebuilds every export index from the graphs and
/// links every project's imports again, whatever changed.
pub fn link_workspace_full(db: &Connection, p: &Principal) -> Result<Value> {
    link_workspace_with(db, p, false)
}

fn link_workspace_with(db: &Connection, p: &Principal, incremental: bool) -> Result<Value> {
    let workspace = p.workspace.as_str();
    let mut hidden = 0usize;
    let mut unprojected = Vec::new();
    let mut candidates: Vec<(String, String, String, bool)> = Vec::new();
    for row in rows(
        db,
        "SELECT id,name FROM repo_projects WHERE workspace_id=? ORDER BY created_at,id",
        [workspace],
    )? {
        let id = s(&row, "id").to_string();
        if !repo_readable(db, p, &id)? {
            hidden += 1;
            continue;
        }
        let Some((episode, current)) = linkable_episode(db, &id)? else {
            unprojected.push(id);
            continue;
        };
        candidates.push((id, s(&row, "name").to_string(), episode, current));
    }
    // Cross-repository linkage needs two sides: with fewer than two linkable
    // repositories there is no pair to link and nothing is recorded.
    if candidates.len() < 2 {
        return Ok(json!({
            "repositories": candidates.iter().map(|(id, name, episode, current)| json!({
                "id": id, "name": name, "episode": episode, "current": current,
                "packages": [], "exported_modules": 0,
            })).collect::<Vec<_>>(),
            "pairs": [],
            "edges": 0,
            "unresolved": 0,
            "hidden_repositories": hidden,
            "persisted": hidden == 0,
            "unprojected_repositories": unprojected,
            "skipped": "fewer_than_two_repositories",
        }));
    }
    let mut repos: Vec<Linkable> = Vec::new();
    for (id, name, episode, current) in candidates {
        let heads = episode_heads(db, &id, &episode)?.unwrap_or_default();
        let manifests = manifests(db, &id, &heads)?;
        let stored = if incremental {
            stored_export_index(db, &id, &episode)?
        } else {
            None
        };
        let (graph, index, built) = match stored {
            Some(index) => (None, index, None),
            None => {
                let graph = resident(db, &id, &episode)?;
                let index = export_index(&graph, &id, &episode, &manifests);
                let built = json!(index).to_string();
                (Some(graph), index, Some(built))
            }
        };
        repos.push(Linkable {
            id,
            name,
            episode,
            current,
            graph,
            heads,
            manifests,
            index,
            built,
        });
    }
    // Nested projects: a project synced from a folder inside
    // another linkable project's folder owns the packages and modules under
    // it, so the outer project stops exporting them and the two never export
    // the same import root (which made every import of it ambiguous).
    let nested = nested_prefixes(db, &repos)?;
    for repo in &mut repos {
        if let Some(prefixes) = nested.get(&repo.id) {
            let mut index = repo.index.clone();
            index
                .packages
                .retain(|package| !prefixes.iter().any(|prefix| below(&package.root, prefix)));
            index
                .modules
                .retain(|_, module| !prefixes.iter().any(|prefix| below(&module.relpath, prefix)));
            repo.index = index;
        }
    }
    // PR scratch copies: a `pr_scratch` project takes part only
    // while no other linkable project exports one of its import roots. A
    // second exporter of the same package would make every import of it from
    // a third project unresolved as `ambiguous_exporters`. A skipped scratch
    // project's live cross rows are closed at its episode (never deleted).
    let scratch = pr_scratch_projects(db, workspace)?;
    let roots = |repo: &Linkable| -> Vec<(xrepo::Ecosystem, String)> {
        repo.index
            .packages
            .iter()
            .flat_map(|package| {
                package
                    .import_roots
                    .iter()
                    .map(|root| (package.ecosystem, root.clone()))
            })
            .collect()
    };
    let mut claimed: BTreeSet<(xrepo::Ecosystem, String)> = repos
        .iter()
        .filter(|repo| !scratch.contains(&repo.id))
        .flat_map(roots)
        .collect();
    // Among PR scratch copies of one package, the newest one exports it
    //: `graphicle pr` keeps a scratch project per folder
    // project now, beside any `git:` copy an earlier run registered.
    // `repos` is in creation order. As with the PR scratch rule, the skip is per
    // project: an older copy that shares any import root with a newer one
    // is skipped whole, its other roots included.
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    for repo in repos.iter().rev().filter(|repo| scratch.contains(&repo.id)) {
        let own = roots(repo);
        if own.iter().any(|root| claimed.contains(root)) {
            skipped.insert(repo.id.clone());
        } else {
            claimed.extend(own);
        }
    }
    let (shadowed, repos): (Vec<Linkable>, Vec<Linkable>) = repos
        .into_iter()
        .partition(|repo| skipped.contains(&repo.id));
    if hidden == 0 {
        for copy in &shadowed {
            db.execute(
                "UPDATE gf_cross_edges SET valid_to_episode=?1,valid_to_repository_id=?2 \
                 WHERE (source_repository_id=?2 OR target_repository_id=?2) AND valid_to_episode IS NULL",
                params![copy.episode, copy.id],
            )?;
            db.execute(
                "UPDATE gf_cross_unresolved SET valid_to_episode=?1,valid_to_repository_id=?2 \
                 WHERE source_repository_id=?2 AND valid_to_episode IS NULL",
                params![copy.episode, copy.id],
            )?;
            // Its rows no longer describe its last link.
            db.execute(
                "UPDATE gf_cross_link_state SET inputs=NULL WHERE repository_id=?",
                [&copy.id],
            )?;
        }
    }
    let skipped_scratch: Vec<&str> = shadowed.iter().map(|copy| copy.id.as_str()).collect();
    if repos.len() < 2 {
        let mut summary = json!({
            "repositories": repos.iter().map(|r| json!({
                "id": r.id, "name": r.name, "episode": r.episode, "current": r.current,
                "packages": [], "exported_modules": 0,
            })).collect::<Vec<_>>(),
            "pairs": [],
            "edges": 0,
            "unresolved": 0,
            "hidden_repositories": hidden,
            "persisted": hidden == 0,
            "unprojected_repositories": unprojected,
            "skipped": "fewer_than_two_repositories",
        });
        summary["pr_scratch_skipped"] = json!(skipped_scratch);
        return Ok(summary);
    }
    for repo in &repos {
        if hidden == 0 {
            record_packages(db, workspace, repo)?;
        }
    }
    // Link state is stored only from a complete view of a workspace with
    // something to link: a workspace whose projects export nothing (one of
    // conversations only, for example) gains no rows.
    let keep_state = hidden == 0
        && repos
            .iter()
            .any(|repo| !repo.index.packages.is_empty() || !repo.index.modules.is_empty());
    if keep_state {
        for repo in &repos {
            if let Some(built) = &repo.built {
                db.execute(
                    "INSERT INTO gf_cross_link_state(repository_id,workspace_id,episode,format,export_index,created_at) \
                     VALUES(?,?,?,?,?,?) ON CONFLICT(repository_id) DO UPDATE SET workspace_id=excluded.workspace_id,\
                     episode=excluded.episode,format=excluded.format,export_index=excluded.export_index,\
                     created_at=excluded.created_at",
                    params![repo.id, workspace, repo.episode, LINK_STATE_FORMAT, built, now()],
                )?;
            }
        }
    }
    let surfaces: Vec<String> = repos
        .iter()
        .map(|repo| export_surface(&repo.index))
        .collect();
    let stamp = now();
    let mut pairs = Vec::new();
    let mut edges_total = 0usize;
    let mut unresolved_total = 0usize;
    let mut relinked = Vec::new();
    let mut reused = Vec::new();
    for (i, importer) in repos.iter().enumerate() {
        // Everything the importer's link reads besides its own episode: the
        // other projects taking part, in order, with their export surfaces.
        let inputs = digest(&json!([
            LINK_STATE_FORMAT,
            importer.id,
            importer.episode,
            repos
                .iter()
                .zip(&surfaces)
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, (repo, surface))| json!([repo.id, surface]))
                .collect::<Vec<_>>(),
        ]));
        let reuse = incremental && keep_state && unchanged_link(db, &importer.id, &inputs)?;
        let report = if reuse {
            reused.push(importer.id.as_str());
            previous_report(db, &importer.id, &importer.episode)?
        } else {
            relinked.push(importer.id.as_str());
            let others: Vec<ExportIndex> = repos
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, r)| r.index.clone())
                .collect();
            let graph = match &importer.graph {
                Some(graph) => graph.clone(),
                None => resident(db, &importer.id, &importer.episode)?,
            };
            let loader = |relpath: &str| -> Option<String> {
                let sha = importer.heads.get(relpath)?;
                blob(db, &importer.id, sha).ok().flatten()
            };
            xrepo::link(
                &graph,
                &importer.id,
                Some(&importer.episode),
                &importer.manifests,
                &others,
                &loader,
            )
        };
        if hidden > 0 {
            // A restricted view cannot establish workspace-wide uniqueness of
            // an exporter. It must not overwrite shared links or unresolved
            // evidence produced with the complete workspace view.
            edges_total += report.edges.len();
            unresolved_total += report.unresolved.len();
            continue;
        }
        let exporter_episodes = json!(
            repos
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, other)| (other.index.repo.clone(), other.index.episode.clone()))
                .collect::<BTreeMap<_, _>>()
        )
        .to_string();
        let mut rewritten_any = false;
        for (j, exporter) in repos.iter().enumerate() {
            if j == i {
                continue;
            }
            let live = live_pair_episodes(db, &importer.id, &exporter.id)?;
            let current_pair = (importer.episode.clone(), exporter.episode.clone());
            let edges: Vec<&xrepo::CrossEdge> = report
                .edges
                .iter()
                .filter(|e| e.target.repo == exporter.id)
                .collect();
            let unchanged = live.len() == 1
                && live[0] == current_pair
                && same_cross_edges(db, &importer.id, &exporter.id, &edges)?;
            let nothing_recorded = live.is_empty();
            if unchanged || (nothing_recorded && edges.is_empty()) {
                pairs.push(json!({
                    "source": importer.id, "target": exporter.id,
                    "source_episode": importer.episode, "target_episode": exporter.episode,
                    "edges": edges.len(), "inserted": 0, "closed": 0, "unchanged": true,
                }));
                edges_total += edges.len();
                continue;
            }
            let mut closed = 0usize;
            for (source_episode, _) in &live {
                let (to_repo, to_episode) = if source_episode != &importer.episode {
                    (&importer.id, &importer.episode)
                } else {
                    (&exporter.id, &exporter.episode)
                };
                closed += db.execute(
                    "UPDATE gf_cross_edges SET valid_to_episode=?,valid_to_repository_id=? \
                     WHERE source_repository_id=? AND target_repository_id=? AND source_episode=? AND valid_to_episode IS NULL",
                    params![to_episode, to_repo, importer.id, exporter.id, source_episode],
                )?;
            }
            for edge in &edges {
                db.execute(
                    "INSERT INTO gf_cross_edges(workspace_id,source_repository_id,source_episode,source_node_id,source_stable_id,\
                     target_repository_id,target_episode,target_node_id,target_stable_id,relation,confidence,derivation,\
                     module_path,symbol,line,version_constraint,exporter_version,version_status,created_at) \
                     VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        workspace,
                        importer.id,
                        importer.episode,
                        edge.source.node_id,
                        edge.source.stable_id,
                        exporter.id,
                        exporter.episode,
                        edge.target.node_id,
                        edge.target.stable_id,
                        edge.relation,
                        edge.confidence,
                        edge.derivation,
                        edge.module_path,
                        edge.symbol,
                        edge.line.map(|l| l as i64),
                        edge.version_constraint,
                        edge.exporter_version,
                        edge.version_status,
                        stamp
                    ],
                )?;
            }
            rewritten_any = true;
            edges_total += edges.len();
            pairs.push(json!({
                "source": importer.id, "target": exporter.id,
                "source_episode": importer.episode, "target_episode": exporter.episode,
                "edges": edges.len(), "inserted": edges.len(), "closed": closed, "unchanged": false,
            }));
        }
        // Unresolved imports are per importing repository; rewrite them when
        // the importer moved or any partner pair was re-linked.
        let live_unresolved = rows(
            db,
            "SELECT DISTINCT source_episode,exporter_episodes FROM gf_cross_unresolved WHERE source_repository_id=? AND valid_to_episode IS NULL",
            [&importer.id],
        )?;
        let stale = live_unresolved.iter().any(|r| {
            s(r, "source_episode") != importer.episode
                || s(r, "exporter_episodes") != exporter_episodes
        });
        let first_time = live_unresolved.is_empty();
        if rewritten_any || stale || (first_time && !report.unresolved.is_empty()) {
            db.execute(
                "UPDATE gf_cross_unresolved SET valid_to_episode=?,valid_to_repository_id=? \
                 WHERE source_repository_id=? AND valid_to_episode IS NULL",
                params![importer.episode, importer.id, importer.id],
            )?;
            for u in &report.unresolved {
                db.execute(
                    "INSERT INTO gf_cross_unresolved(workspace_id,source_repository_id,source_episode,source_node_id,source_stable_id,\
                     relpath,line,module_path,symbol,package,reason,candidates,exporter_episodes,version_constraint,exporter_version,created_at) \
                     VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        workspace,
                        importer.id,
                        importer.episode,
                        u.node_id,
                        u.stable_id,
                        u.relpath,
                        u.line.map(|l| l as i64),
                        u.module_path,
                        u.symbol,
                        u.package,
                        u.reason,
                        json!(u.candidates).to_string(),
                        exporter_episodes,
                        u.version_constraint,
                        u.exporter_version,
                        stamp
                    ],
                )?;
            }
        }
        // What this link read and wrote, for the next pass to compare. It
        // updates a stored row only, so a workspace without one gains none.
        db.execute(
            "UPDATE gf_cross_link_state SET inputs=?1,\
             edges=(SELECT count(*) FROM gf_cross_edges WHERE source_repository_id=?2 AND valid_to_episode IS NULL),\
             unresolved=(SELECT count(*) FROM gf_cross_unresolved WHERE source_repository_id=?2 AND valid_to_episode IS NULL) \
             WHERE repository_id=?2",
            params![inputs, importer.id],
        )?;
        unresolved_total += report.unresolved.len();
    }
    let mut summary = json!({
        "repositories": repos.iter().map(|r| json!({
            "id": r.id, "name": r.name, "episode": r.episode, "current": r.current,
            "packages": r.index.packages.iter().map(|p| json!({
                "ecosystem": p.ecosystem.as_str(), "name": p.name, "version": p.version,
            })).collect::<Vec<_>>(),
            "exported_modules": r.index.modules.len(),
        })).collect::<Vec<_>>(),
        "pairs": pairs,
        "edges": edges_total,
        "unresolved": unresolved_total,
        "hidden_repositories": hidden,
        "persisted": hidden == 0,
        "unprojected_repositories": unprojected,
        "incremental": incremental,
        "relinked": relinked,
        "reused": reused,
    });
    if !skipped_scratch.is_empty() {
        summary["pr_scratch_skipped"] = json!(skipped_scratch);
    }
    Ok(summary)
}

/// Whether `repository`'s last link read `inputs` and its live rows are
/// still the ones that link wrote (as many edges and unresolved imports).
fn unchanged_link(db: &Connection, repository: &str, inputs: &str) -> Result<bool> {
    Ok(one(
        db,
        "SELECT 1 AS same FROM gf_cross_link_state WHERE repository_id=?1 AND inputs=?2 \
         AND edges=(SELECT count(*) FROM gf_cross_edges WHERE source_repository_id=?1 AND valid_to_episode IS NULL) \
         AND unresolved=(SELECT count(*) FROM gf_cross_unresolved WHERE source_repository_id=?1 AND valid_to_episode IS NULL)",
        params![repository, inputs],
    )?
    .is_some())
}

/// Whether the repository-relative `path` is `prefix` or lies under it.
fn below(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// For each linkable project, the current folders of the other linkable
/// projects that lie inside its own current folder, relative to it
/// (`sub/dir`): the nested-project rule. A project's current
/// folder is the one it was last synced from
/// ([`crate::folders::current_folder`]); its older aliases (a moved
/// folder's old path, or a folder whose content a later `--project` sync of
/// another folder replaced) do not describe its files and take no part,
/// nor do projects without recorded folders (synced by an earlier build,
/// or over HTTP).
fn nested_prefixes(db: &Connection, repos: &[Linkable]) -> Result<HashMap<String, Vec<String>>> {
    let mut folders: Vec<(&str, PathBuf)> = Vec::with_capacity(repos.len());
    for repo in repos {
        if let Some(folder) = crate::folders::current_folder(db, &repo.id)? {
            folders.push((repo.id.as_str(), folder));
        }
    }
    let mut nested: HashMap<String, Vec<String>> = HashMap::new();
    for (outer, outer_folder) in &folders {
        for (inner, inner_folder) in &folders {
            if outer == inner {
                continue;
            }
            if let Ok(relative) = inner_folder.strip_prefix(outer_folder)
                && !relative.as_os_str().is_empty()
            {
                nested
                    .entry(outer.to_string())
                    .or_default()
                    .push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(nested)
}

/// Project role that marks a PR scratch project: the projects
/// `graphicle pr` registered under a `git:` key for a commit range. Linking
/// skips one while another project exports its packages; workspace recall
/// and `list_projects` skip them (`crate::workspace`).
pub const PR_SCRATCH: &str = "pr_scratch";

/// Ids of the workspace's `pr_scratch` projects.
pub fn pr_scratch_projects(db: &Connection, workspace: &str) -> Result<BTreeSet<String>> {
    Ok(rows(
        db,
        "SELECT r.repository_id FROM repo_project_roles r JOIN repo_projects p ON p.id=r.repository_id \
         WHERE p.workspace_id=? AND r.role=?",
        params![workspace, PR_SCRATCH],
    )?
    .iter()
    .map(|row| s(row, "repository_id").to_string())
    .collect())
}

/// Mark the projects earlier `graphicle pr` runs registered as `pr_scratch`:
/// their key is `git:` and the sha256 of the checkout path. Idempotent; no
/// other data changes.
pub fn mark_legacy_pr_projects(db: &Connection) -> Result<usize> {
    let pattern = format!("git:{}", "[0-9a-f]".repeat(64));
    Ok(db.execute(
        "INSERT OR IGNORE INTO repo_project_roles(repository_id,role,created_at) \
         SELECT id,?1,?2 FROM repo_projects WHERE project_key GLOB ?3",
        params![PR_SCRATCH, now(), pattern],
    )?)
}

/// Cross-repository edges targeting the requested base/head revisions, or
/// live edges when no pair is supplied. Later exporter revisions never
/// substitute for an older report's evidence.
pub fn cross_edges_into(
    db: &Connection,
    p: &Principal,
    repository: &str,
    episodes: Option<(&str, &str)>,
) -> Result<Vec<Value>> {
    let (base, head) = episodes.unwrap_or(("", ""));
    let mut out = Vec::new();
    let mut readable: HashMap<String, bool> = HashMap::new();
    for row in rows(
        db,
        "SELECT source_repository_id,source_episode,source_node_id,source_stable_id,target_episode,target_node_id,\
         relation,confidence,derivation,module_path,symbol,line,version_constraint,exporter_version,version_status,valid_to_episode \
         FROM gf_cross_edges WHERE workspace_id=?1 AND target_repository_id=?2 \
         AND ((?3='' AND valid_to_episode IS NULL) OR (?3<>'' AND target_episode IN (?3,?4))) ORDER BY id",
        params![p.workspace, repository, base, head],
    )? {
        let source = s(&row, "source_repository_id").to_string();
        let allowed = match readable.get(&source) {
            Some(allowed) => *allowed,
            None => {
                let allowed = repo_readable(db, p, &source)?;
                readable.insert(source.clone(), allowed);
                allowed
            }
        };
        if allowed {
            out.push(row);
        }
    }
    Ok(out)
}

/// Which side of a cross-repository edge a repository is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossDirection {
    /// The repository imports from another one (it is the edge's source).
    Outgoing,
    /// Another repository imports from it (it is the edge's target).
    Incoming,
}

/// The live cross-repository edges (`valid_to_episode IS NULL`) touching
/// `repository` in one direction, every column, in row order, kept only when
/// the principal may read the repository on the other side (the one
/// permission-filtered reader of both directions that `graph.cross_edges`
/// and workspace recall's cross-repository step share).
/// The caller has authorized `repository` itself.
pub fn live_cross_edges(
    db: &Connection,
    p: &Principal,
    repository: &str,
    direction: CrossDirection,
) -> Result<Vec<Value>> {
    let (own, other) = match direction {
        CrossDirection::Outgoing => ("source_repository_id", "target_repository_id"),
        CrossDirection::Incoming => ("target_repository_id", "source_repository_id"),
    };
    let mut readable: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
    for edge in rows(
        db,
        &format!(
            "SELECT * FROM gf_cross_edges WHERE workspace_id=?1 AND {own}=?2 AND valid_to_episode IS NULL ORDER BY id"
        ),
        params![p.workspace, repository],
    )? {
        let side = s(&edge, other).to_string();
        let allowed = match readable.get(&side) {
            Some(allowed) => *allowed,
            None => {
                let allowed = repo_readable(db, p, &side)?;
                readable.insert(side, allowed);
                allowed
            }
        };
        if allowed {
            out.push(edge);
        }
    }
    Ok(out)
}

/// Live unresolved cross-repository imports that name `repository` as a
/// candidate exporter or one of its declared packages, from importing
/// repositories the principal may read.
pub fn cross_unresolved_toward(
    db: &Connection,
    p: &Principal,
    repository: &str,
    head_episode: Option<&str>,
) -> Result<Vec<Value>> {
    let packages: Vec<String> = rows(
        db,
        "SELECT DISTINCT name FROM gf_cross_packages WHERE repository_id=?1 AND (?2 IS NULL OR episode=?2)",
        params![repository, head_episode],
    )?
    .iter()
    .map(|r| s(r, "name").to_string())
    .collect();
    let mut out = Vec::new();
    for mut row in rows(
        db,
        "SELECT source_repository_id,source_episode,source_node_id,source_stable_id,relpath,line,module_path,symbol,\
         package,reason,candidates,version_constraint,exporter_version \
         FROM gf_cross_unresolved u WHERE workspace_id=?1 AND source_repository_id<>?2 \
         AND ((?3 IS NULL AND valid_to_episode IS NULL) OR \
              (?3 IS NOT NULL AND EXISTS (SELECT 1 FROM json_each(u.exporter_episodes) e WHERE e.key=?2 AND e.value=?3))) ORDER BY id",
        params![p.workspace, repository, head_episode],
    )? {
        let candidates: Vec<String> =
            serde_json::from_str(s(&row, "candidates")).unwrap_or_default();
        let names_repo = candidates.iter().any(|c| c == repository);
        let package = s(&row, "package");
        let names_package = packages.iter().any(|name| {
            name == package
                || name.starts_with(&format!("{package}/"))
                || package.starts_with(&format!("{name}/"))
        });
        if !(names_repo || names_package) {
            continue;
        }
        if !repo_readable(db, p, s(&row, "source_repository_id"))? {
            continue;
        }
        let mut visible = Vec::new();
        for candidate in candidates {
            if repo_readable(db, p, &candidate)? {
                visible.push(candidate);
            }
        }
        row["candidates"] = json!(json!(visible).to_string());
        out.push(row);
    }
    Ok(out)
}

impl Store {
    /// Link imports across all repositories of the workspace (`graph.link`),
    /// then run the `link` job's document step now
    /// ([`crate::doc_links::relink`]): its summary, or its error, is the
    /// result's `documents`.
    pub fn graph_link(&self, workspace: &str, token: &str) -> Result<Value> {
        self.graph_link_with(workspace, token, link_workspace)
    }
    /// [`Store::graph_link`] with a full pass ([`link_workspace_full`]):
    /// every project's imports are linked again, whatever changed.
    pub fn graph_link_full(&self, workspace: &str, token: &str) -> Result<Value> {
        self.graph_link_with(workspace, token, link_workspace_full)
    }
    fn graph_link_with(
        &self,
        workspace: &str,
        token: &str,
        pass: fn(&Connection, &Principal) -> Result<Value>,
    ) -> Result<Value> {
        let (mut result, principal) = self.authorized(workspace, token, "read", |db, p| {
            Ok((pass(db, p)?, p.hash.clone()))
        })?;
        result["documents"] =
            match crate::doc_links::relink(self, workspace, &principal, |_| Ok(())) {
                Ok(summary) => summary,
                Err(error) => json!({"error": error.message}),
            };
        Ok(result)
    }
    /// Grant or revoke a credential's read access to one repository
    /// (`graph.access`; admin only).
    pub fn graph_access(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "admin", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let identifier = s(&repository, "id");
            let credential = string(args, "credential_id", 100)?;
            if one(
                db,
                "SELECT id FROM credentials WHERE id=? AND workspace_id=?",
                params![credential, workspace],
            )?
            .is_none()
            {
                return Err(Error::bad("Unknown credential in this workspace"));
            }
            let state = string(args, "state", 20)?;
            set_repo_access(db, workspace, identifier, credential, state)?;
            Ok(json!({"repository_id": identifier, "credential_id": credential, "state": state}))
        })
    }
    /// Live cross-repository edges and unresolved imports touching a repository.
    pub fn graph_cross_edges(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
    ) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            Ok(json!({
                "incoming": cross_edges_into(db, p, identifier, None)?,
                "outgoing": live_cross_edges(db, p, identifier, CrossDirection::Outgoing)?,
                "unresolved": cross_unresolved_toward(db, p, identifier, None)?,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_serializer_matches_text_canonical() {
        let value = json!({
            "z": {"b": [3, {"y": null, "a": "é\"\\\n"}], "a": 1.5},
            "m": [],
            "a": {},
            "k": "text",
            "n": -7,
            "big": 18446744073709551615u64,
        });
        let Value::Object(fields) = &value else {
            unreachable!()
        };
        let mut out = String::new();
        canonical_into(&mut out, &fields.clone().into());
        assert_eq!(out, graphicle_memory::text::canonical(&value));
        canonical_into(&mut out, &graphicle::record::Attributes::new());
        assert_eq!(out, "{}");
    }

    #[test]
    fn edge_identity_keeps_column_boundaries() {
        let mut first = Vec::new();
        let mut second = Vec::new();
        edge_identity(&mut first, "a\0b", "c", "calls", "");
        edge_identity(&mut second, "a", "b\0c", "calls", "");
        assert_ne!(first, second);
        edge_identity(&mut first, "a", "b", "calls\0", "");
        edge_identity(&mut second, "a", "b", "calls", "\0");
        assert_ne!(first, second);
    }
}
