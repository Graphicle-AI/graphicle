//! Dependency index and staleness markers for derived artifacts.
//!
//! Producers of derived data (chunks, embeddings, graph nodes, summaries, cached
//! answers) record what they were derived from with [`record_dependency`].  When a
//! source changes, [`invalidate_source`] walks the dependency graph transitively and
//! marks every dependent stale in `gf_stale`.  Readers report the workspace-wide
//! [`Freshness`] so callers can see when an answer may rest on outdated artifacts.
use crate::{Result, now};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Artifact kinds used as `kind` in `gf_dependencies` and `gf_stale`.
pub const FILE: &str = "file";
pub const CHUNK: &str = "chunk";
pub const EMBEDDING: &str = "embedding";
pub const GRAPH_NODE: &str = "graph_node";
pub const GRAPH_EDGE: &str = "graph_edge";
pub const COMMUNITY: &str = "community";
pub const SUMMARY: &str = "summary";
pub const REPORT: &str = "report";
pub const DOCUMENT: &str = "document";
pub const MEMORY: &str = "memory";
pub const ANSWER: &str = "answer";
pub const PRINCIPAL: &str = "principal";
pub const SOURCE: &str = "source";
/// One anchored document chunk of the semantic overlay (`gf_anchor_edges`).
pub const ANCHOR: &str = "anchor";

/// Upper bound on the number of artifacts marked in one transitive walk.
const WALK_LIMIT: usize = 100_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Freshness {
    pub stale_count: u64,
    pub by_kind: BTreeMap<String, u64>,
    pub oldest_since: Option<String>,
}

impl Freshness {
    pub fn is_fresh(&self) -> bool {
        self.stale_count == 0
    }
}

/// Build the composite identifier used for a repository-scoped file artifact.
pub fn file_id(repository: &str, path: &str) -> String {
    format!("{repository}:{path}")
}
/// Build the composite identifier used for a repository-scoped graph node.
pub fn node_id(repository: &str, node: &str) -> String {
    format!("{repository}:{node}")
}
/// Build the composite identifier of one anchored chunk ([`ANCHOR`]).
pub fn anchor_id(repository: &str, chunk: &str) -> String {
    format!("{repository}:{chunk}")
}
/// Build the composite identifier used for a repository-scoped chunk.
pub fn chunk_id(repository: &str, path: &str, line: i64) -> String {
    format!("{repository}:{path}:{line}")
}

/// Record that `dependent` was derived from `source`.  Idempotent.
pub fn record_dependency(
    db: &Connection,
    workspace: &str,
    dependent: (&str, &str),
    source: (&str, &str),
) -> Result<()> {
    if dependent == source {
        return Ok(());
    }
    db.execute(
        "INSERT OR IGNORE INTO gf_dependencies(workspace,dependent_kind,dependent_id,source_kind,source_id,recorded_at) VALUES(?,?,?,?,?,?)",
        params![workspace, dependent.0, dependent.1, source.0, source.1, now()],
    )?;
    Ok(())
}

/// Remove every dependency edge recorded for `dependent` (before re-recording).
pub fn forget_dependent(db: &Connection, workspace: &str, dependent: (&str, &str)) -> Result<()> {
    db.execute(
        "DELETE FROM gf_dependencies WHERE workspace=? AND dependent_kind=? AND dependent_id=?",
        params![workspace, dependent.0, dependent.1],
    )?;
    Ok(())
}

/// Mark a single artifact stale without walking dependents.
pub fn mark_stale(
    db: &Connection,
    workspace: &str,
    kind: &str,
    id: &str,
    reason: &str,
) -> Result<()> {
    db.prepare_cached(
        "INSERT INTO gf_stale(workspace,kind,id,reason,since) VALUES(?,?,?,?,?) ON CONFLICT(workspace,kind,id) DO UPDATE SET reason=excluded.reason",
    )?
    .execute(params![workspace, kind, id, reason, now()])?;
    Ok(())
}

/// Mark every artifact that (transitively) depends on `source` as stale.
///
/// The source itself is not marked; returns the `(kind, id)` pairs marked, in
/// breadth-first order, without duplicates.
pub fn invalidate_source(
    db: &Connection,
    workspace: &str,
    source: (&str, &str),
    reason: &str,
) -> Result<Vec<(String, String)>> {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    seen.insert((source.0.to_string(), source.1.to_string()));
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    queue.push_back((source.0.to_string(), source.1.to_string()));
    let mut marked = Vec::new();
    let mut statement = db.prepare_cached(
        "SELECT dependent_kind,dependent_id FROM gf_dependencies WHERE workspace=? AND source_kind=? AND source_id=?",
    )?;
    while let Some((kind, id)) = queue.pop_front() {
        let dependents = statement
            .query_map(params![workspace, kind, id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for dependent in dependents {
            if !seen.insert(dependent.clone()) {
                continue;
            }
            mark_stale(db, workspace, &dependent.0, &dependent.1, reason)?;
            marked.push(dependent.clone());
            if marked.len() >= WALK_LIMIT {
                return Ok(marked);
            }
            queue.push_back(dependent);
        }
    }
    Ok(marked)
}

pub fn is_stale(db: &Connection, workspace: &str, kind: &str, id: &str) -> Result<bool> {
    Ok(db
        .query_row(
            "SELECT 1 FROM gf_stale WHERE workspace=? AND kind=? AND id=?",
            params![workspace, kind, id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Clear the stale marker for an artifact once it has been recomputed.
pub fn clear_stale(db: &Connection, workspace: &str, kind: &str, id: &str) -> Result<bool> {
    Ok(db.execute(
        "DELETE FROM gf_stale WHERE workspace=? AND kind=? AND id=?",
        params![workspace, kind, id],
    )? > 0)
}

/// Clear every stale marker of one kind whose id starts with `prefix`
/// (e.g. every graph node of a repository after a fresh projection).
pub fn clear_stale_prefix(
    db: &Connection,
    workspace: &str,
    kind: &str,
    prefix: &str,
) -> Result<usize> {
    let pattern = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
    Ok(db.execute(
        "DELETE FROM gf_stale WHERE workspace=? AND kind=? AND id LIKE ? ESCAPE '\\'",
        params![workspace, kind, pattern],
    )?)
}

/// Workspace-wide staleness summary.
pub fn freshness(db: &Connection, workspace: &str) -> Result<Freshness> {
    let mut statement = db
        .prepare("SELECT kind,count(*),min(since) FROM gf_stale WHERE workspace=? GROUP BY kind")?;
    let mut freshness = Freshness::default();
    for row in statement.query_map([workspace], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (kind, count, since) = row?;
        let count = u64::try_from(count).unwrap_or(0);
        freshness.stale_count += count;
        freshness.by_kind.insert(kind, count);
        match &freshness.oldest_since {
            Some(oldest) if *oldest <= since => {}
            _ => freshness.oldest_since = Some(since),
        }
    }
    Ok(freshness)
}

/// Freshness restricted to artifacts belonging to one repository (ids prefixed
/// with `repository:`), plus workspace-level artifacts (answers, memories).
pub fn repository_freshness(
    db: &Connection,
    workspace: &str,
    repository: &str,
) -> Result<Freshness> {
    let pattern = format!("{}:%", repository.replace('%', "\\%").replace('_', "\\_"));
    let mut statement = db.prepare(
        "SELECT kind,count(*),min(since) FROM gf_stale WHERE workspace=? AND (id LIKE ? ESCAPE '\\' OR kind IN ('answer','memory','principal')) GROUP BY kind",
    )?;
    let mut freshness = Freshness::default();
    for row in statement.query_map(params![workspace, pattern], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (kind, count, since) = row?;
        let count = u64::try_from(count).unwrap_or(0);
        freshness.stale_count += count;
        freshness.by_kind.insert(kind, count);
        match &freshness.oldest_since {
            Some(oldest) if *oldest <= since => {}
            _ => freshness.oldest_since = Some(since),
        }
    }
    Ok(freshness)
}

/// Map an evidence document id (`file:<path>`, `connector:<id>:<doc>`, or a
/// workspace-source link id) to the `(kind, id)` source it derives from.
pub fn origin_source(repository: &str, origin: &str) -> (&'static str, String) {
    match origin.strip_prefix("file:") {
        Some(path) => (FILE, file_id(repository, path)),
        None => (DOCUMENT, origin.to_string()),
    }
}

/// Workspace that owns a repository; empty when the repository is unknown.
pub fn workspace_of(db: &Connection, repository: &str) -> Result<String> {
    Ok(db
        .query_row(
            "SELECT workspace_id FROM repo_projects WHERE id=?",
            [repository],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_default())
}

/// Connector document identifier as it appears in evidence documents.
pub fn connector_document_id(connector: &str, document: &str) -> String {
    format!("connector:{connector}:{document}")
}

/// Record a memory's dependencies from its saved `data` (repository file
/// paths under `dependencies`, workspace links under `evidence_links`,
/// connector documents under `native_sources`) and clear its stale marker.
pub fn record_memory(
    db: &Connection,
    workspace: &str,
    repository: &str,
    memory: &str,
    data: &serde_json::Value,
) -> Result<()> {
    let dependent = (MEMORY, memory);
    forget_dependent(db, workspace, dependent)?;
    for path in data["dependencies"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k)
    {
        record_dependency(db, workspace, dependent, (FILE, &file_id(repository, path)))?;
    }
    for key in ["evidence_links", "native_sources"] {
        for origin in data[key].as_object().into_iter().flatten().map(|(k, _)| k) {
            let (kind, id) = origin_source(repository, origin);
            record_dependency(db, workspace, dependent, (kind, &id))?;
        }
    }
    clear_stale(db, workspace, MEMORY, memory)?;
    Ok(())
}

/// Record that a derived artifact depends on each evidence document it used
/// (by document id as produced by `memory::documents`).
pub fn record_document_dependencies<'a>(
    db: &Connection,
    workspace: &str,
    repository: &str,
    dependent: (&str, &str),
    origins: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    for origin in origins {
        let (kind, id) = origin_source(repository, origin);
        record_dependency(db, workspace, dependent, (kind, &id))?;
    }
    Ok(())
}

/// Record `graph_node → file` dependencies for every live node of a freshly
/// persisted episode and clear their stale markers: the projection is the
/// recomputation. Nodes whose `relpath` attribute is empty are skipped.
pub fn record_graph_episode(db: &Connection, workspace: &str, repository: &str) -> Result<usize> {
    let mut statement = db.prepare(
        "SELECT node_id,attributes FROM gf_nodes WHERE repository_id=? AND valid_to_episode IS NULL",
    )?;
    let nodes = statement
        .query_map([repository], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    clear_stale_prefix(db, workspace, GRAPH_NODE, &format!("{repository}:"))?;
    let mut recorded = 0;
    for (node, attributes) in nodes {
        let attributes: serde_json::Value = serde_json::from_str(&attributes).unwrap_or_default();
        let Some(relpath) = attributes["relpath"].as_str().filter(|p| !p.is_empty()) else {
            continue;
        };
        let dependent = node_id(repository, &node);
        forget_dependent(db, workspace, (GRAPH_NODE, &dependent))?;
        record_dependency(
            db,
            workspace,
            (GRAPH_NODE, &dependent),
            (FILE, &file_id(repository, relpath)),
        )?;
        recorded += 1;
    }
    Ok(recorded)
}

/// Incremental form of [`record_graph_episode`] for a persisted episode diff:
/// `inserted` are the `(node_id, relpath)` pairs of the nodes the episode
/// inserted, `closed` the node ids it closed, `unchanged_live` whether any
/// live node was carried over. Unchanged live nodes keep the dependency rows
/// recorded when they were inserted, so the cost follows the diff rather than
/// the repository. Stores whose live nodes predate the dependency index are
/// backfilled once through the full pass.
pub fn record_graph_nodes<'a>(
    db: &Connection,
    workspace: &str,
    repository: &str,
    inserted: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    closed: impl IntoIterator<Item = &'a str>,
    unchanged_live: bool,
) -> Result<usize> {
    let prefix = format!("{repository}:");
    if unchanged_live {
        let mut upper = prefix.clone();
        upper.pop();
        upper.push(';');
        let indexed: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM gf_dependencies WHERE workspace=? AND dependent_kind=? AND dependent_id>=? AND dependent_id<?)",
            params![workspace, GRAPH_NODE, prefix, upper],
            |r| r.get(0),
        )?;
        if !indexed {
            return record_graph_episode(db, workspace, repository);
        }
    }
    clear_stale_prefix(db, workspace, GRAPH_NODE, &prefix)?;
    let stamp = now();
    let mut forget = db.prepare_cached(
        "DELETE FROM gf_dependencies WHERE workspace=? AND dependent_kind=? AND dependent_id=?",
    )?;
    let mut record = db.prepare_cached(
        "INSERT OR IGNORE INTO gf_dependencies(workspace,dependent_kind,dependent_id,source_kind,source_id,recorded_at) VALUES(?,?,?,?,?,?)",
    )?;
    for node in closed {
        forget.execute(params![workspace, GRAPH_NODE, node_id(repository, node)])?;
    }
    let mut recorded = 0;
    for (node, relpath) in inserted {
        let dependent = node_id(repository, node);
        forget.execute(params![workspace, GRAPH_NODE, dependent])?;
        let Some(relpath) = relpath else {
            continue;
        };
        record.execute(params![
            workspace,
            GRAPH_NODE,
            dependent,
            FILE,
            file_id(repository, relpath),
            stamp
        ])?;
        recorded += 1;
    }
    Ok(recorded)
}

/// Record `chunk → file` dependencies for the chunks of one committed file and
/// clear their stale markers (the chunks were just rebuilt).
pub fn record_file_chunks(
    db: &Connection,
    workspace: &str,
    repository: &str,
    path: &str,
    sha256: &str,
) -> Result<usize> {
    let mut statement =
        db.prepare("SELECT line FROM repo_chunks WHERE repository_id=? AND path=? AND sha256=?")?;
    let lines = statement
        .query_map(params![repository, path, sha256], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let file = file_id(repository, path);
    for line in &lines {
        let chunk = chunk_id(repository, path, *line);
        record_dependency(db, workspace, (CHUNK, &chunk), (FILE, &file))?;
        clear_stale(db, workspace, CHUNK, &chunk)?;
    }
    Ok(lines.len())
}

/// Invalidate everything derived from a repository file that changed or was
/// removed through the sync/commit path.  Records nothing; the producers of the
/// derived artifacts own their dependency edges.
pub fn file_changed(
    db: &Connection,
    workspace: &str,
    repository: &str,
    path: &str,
    reason: &str,
) -> Result<Vec<(String, String)>> {
    let id = file_id(repository, path);
    invalidate_source(db, workspace, (FILE, &id), reason)
}

/// Invalidate everything derived from a connector document.
pub fn document_changed(
    db: &Connection,
    workspace: &str,
    document: &str,
    reason: &str,
) -> Result<Vec<(String, String)>> {
    invalidate_source(db, workspace, (DOCUMENT, document), reason)
}

/// A principal's permissions changed: invalidate cached answers/context recorded
/// against that principal and mark every artifact scoped to the revoked
/// sources stale.
pub fn permissions_changed(
    db: &Connection,
    workspace: &str,
    principal: Option<&str>,
    revoked_sources: &[String],
    reason: &str,
) -> Result<Vec<(String, String)>> {
    let mut marked = match principal {
        Some(principal) => invalidate_source(db, workspace, (PRINCIPAL, principal), reason)?,
        None => Vec::new(),
    };
    for source in revoked_sources {
        marked.extend(invalidate_source(db, workspace, (SOURCE, source), reason)?);
    }
    marked.sort();
    marked.dedup();
    Ok(marked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("schema.sql")).unwrap();
        db
    }

    #[test]
    fn transitive_invalidation_marks_every_dependent_once() {
        let db = db();
        record_dependency(&db, "w", (CHUNK, "c1"), (FILE, "f")).unwrap();
        record_dependency(&db, "w", (EMBEDDING, "e1"), (CHUNK, "c1")).unwrap();
        record_dependency(&db, "w", (GRAPH_NODE, "n1"), (FILE, "f")).unwrap();
        record_dependency(&db, "w", (SUMMARY, "s1"), (GRAPH_NODE, "n1")).unwrap();
        record_dependency(&db, "w", (SUMMARY, "s1"), (EMBEDDING, "e1")).unwrap();
        // A cycle must terminate.
        record_dependency(&db, "w", (FILE, "f"), (SUMMARY, "s1")).unwrap();
        let marked = invalidate_source(&db, "w", (FILE, "f"), "edited").unwrap();
        assert_eq!(marked.len(), 4);
        assert!(is_stale(&db, "w", SUMMARY, "s1").unwrap());
        assert!(!is_stale(&db, "w", FILE, "f").unwrap());
        assert!(!is_stale(&db, "other", SUMMARY, "s1").unwrap());
        let f = freshness(&db, "w").unwrap();
        assert_eq!(f.stale_count, 4);
        assert_eq!(f.by_kind[SUMMARY], 1);
        assert!(f.oldest_since.is_some());
        assert!(clear_stale(&db, "w", SUMMARY, "s1").unwrap());
        assert_eq!(freshness(&db, "w").unwrap().stale_count, 3);
        assert!(freshness(&db, "other").unwrap().is_fresh());
    }

    #[test]
    fn prefix_clearing_is_repository_scoped() {
        let db = db();
        mark_stale(&db, "w", GRAPH_NODE, "r1:a", "x").unwrap();
        mark_stale(&db, "w", GRAPH_NODE, "r1:b", "x").unwrap();
        mark_stale(&db, "w", GRAPH_NODE, "r2:a", "x").unwrap();
        assert_eq!(clear_stale_prefix(&db, "w", GRAPH_NODE, "r1:").unwrap(), 2);
        assert_eq!(freshness(&db, "w").unwrap().stale_count, 1);
        assert_eq!(repository_freshness(&db, "w", "r2").unwrap().stale_count, 1);
        assert_eq!(repository_freshness(&db, "w", "r1").unwrap().stale_count, 0);
    }
}
