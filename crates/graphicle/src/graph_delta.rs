//! Exact file-group fingerprints for incremental projection persistence.
//!
//! The complete assembled graph is still the authority. Hash every final
//! record, including global normalization effects, then load/compare SQLite
//! records only for groups that differ. SQL triggers invalidate fingerprints
//! on every row mutation, so this is a cache rather than a second authority.
use crate::{Result, graph_projection::CanonicalRecord, store::ensure_column};
use graphicle::{graph::Graph, record::Attributes};
use rusqlite::{Connection, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
};

pub fn owner(attributes: &Attributes) -> &str {
    attributes
        .get("relpath")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            attributes
                .get("source_file")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("")
}

pub fn migrate(db: &Connection) -> Result<()> {
    let transaction = db.unchecked_transaction()?;
    let db = &*transaction;
    let existed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='gf_projection_groups')", [], |r| r.get(0))?;
    for table in ["gf_nodes", "gf_edges"] {
        ensure_column(db, table, "owner", "TEXT NOT NULL DEFAULT ''")?;
    }
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS gf_projection_groups (
            repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
            owner TEXT NOT NULL, digest BLOB,
            PRIMARY KEY(repository_id,owner));
         CREATE INDEX IF NOT EXISTS gf_nodes_owner ON gf_nodes(repository_id,owner) WHERE valid_to_episode IS NULL;
         CREATE INDEX IF NOT EXISTS gf_edges_owner ON gf_edges(repository_id,owner) WHERE valid_to_episode IS NULL;"
    )?;
    if !existed {
        for table in ["gf_nodes", "gf_edges"] {
            db.execute_batch(&format!(
                "UPDATE {table} SET owner=CASE
                   WHEN json_type(attributes,'$.relpath')='text' THEN json_extract(attributes,'$.relpath')
                   WHEN json_type(attributes,'$.source_file')='text' THEN json_extract(attributes,'$.source_file')
                   ELSE '' END;
                 INSERT OR IGNORE INTO gf_projection_groups(repository_id,owner)
                   SELECT DISTINCT repository_id,owner FROM {table} WHERE valid_to_episode IS NULL;"))?;
        }
    }
    install_triggers(db)?;
    transaction.commit()?;
    Ok(())
}

fn install_triggers(db: &Connection) -> Result<()> {
    for table in ["gf_nodes", "gf_edges"] {
        db.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS {table}_group_insert AFTER INSERT ON {table} BEGIN
                 INSERT INTO gf_projection_groups(repository_id,owner,digest) VALUES(NEW.repository_id,NEW.owner,NULL)
                   ON CONFLICT(repository_id,owner) DO UPDATE SET digest=NULL WHERE digest IS NOT NULL;
             END;
             CREATE TRIGGER IF NOT EXISTS {table}_group_update AFTER UPDATE ON {table} BEGIN
                 UPDATE gf_projection_groups SET digest=NULL WHERE repository_id=OLD.repository_id AND owner=OLD.owner AND digest IS NOT NULL;
                 INSERT INTO gf_projection_groups(repository_id,owner,digest) VALUES(NEW.repository_id,NEW.owner,NULL)
                   ON CONFLICT(repository_id,owner) DO UPDATE SET digest=NULL WHERE digest IS NOT NULL;
             END;
             CREATE TRIGGER IF NOT EXISTS {table}_group_delete AFTER DELETE ON {table} BEGIN
                 UPDATE gf_projection_groups SET digest=NULL WHERE repository_id=OLD.repository_id AND owner=OLD.owner AND digest IS NOT NULL;
             END;"))?;
    }
    Ok(())
}

/// Keep the authoritative projection and its cache publication atomic.
///
/// Row triggers impose a statement journal on every insert. The projector
/// already knows every changed group, so temporarily remove its cache
/// triggers inside a savepoint. SQLite serializes writers and transactionally
/// restores the schema on rollback; other connections never see a committed
/// state without the triggers. Unrelated SQL writes retain normal invalidation.
pub struct ProjectionWrite<'a> {
    db: &'a Connection,
    committed: bool,
}

impl<'a> ProjectionWrite<'a> {
    pub fn begin(db: &'a Connection) -> Result<Self> {
        db.execute_batch("SAVEPOINT graphicle_graph_projection")?;
        let write = Self {
            db,
            committed: false,
        };
        for table in ["gf_nodes", "gf_edges"] {
            for action in ["insert", "update", "delete"] {
                db.execute_batch(&format!("DROP TRIGGER IF EXISTS {table}_group_{action}"))?;
            }
        }
        Ok(write)
    }

    pub fn commit(mut self) -> Result<()> {
        install_triggers(self.db)?;
        self.db
            .execute_batch("RELEASE graphicle_graph_projection")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for ProjectionWrite<'_> {
    fn drop(&mut self) {
        if !self.committed && !self.db.is_autocommit() {
            // A SQL error or panic must not leave partial rows or disabled
            // triggers behind, even if the caller commits its outer transaction.
            let _ = self.db.execute_batch(
                "ROLLBACK TO graphicle_graph_projection; RELEASE graphicle_graph_projection",
            );
        }
    }
}

struct HashWriter<'a>(&'a mut Sha256);
impl io::Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub groups: usize,
    pub dirty_groups: usize,
    pub nodes_compared: usize,
    pub edges_compared: usize,
    pub total_nodes: usize,
    pub total_edges: usize,
}

pub struct Plan {
    pub stats: Stats,
    pub dirty_json: String,
    dirty: BTreeSet<String>,
    next: BTreeMap<String, [u8; 32]>,
}

impl Plan {
    pub fn new(db: &Connection, repository: &str, graph: &Graph) -> Result<Self> {
        let mut groups: HashMap<&str, (Sha256, usize, usize)> = HashMap::new();
        for node in graph.nodes() {
            let group = groups.entry(owner(&node.attributes)).or_default();
            serde_json::to_writer(
                HashWriter(&mut group.0),
                &(0, &node.id, CanonicalRecord(&node.attributes)),
            )?;
            group.1 += 1;
        }
        for (source, target, edge) in graph.iter_edges() {
            let group = groups.entry(owner(&edge.attributes)).or_default();
            serde_json::to_writer(
                HashWriter(&mut group.0),
                &(
                    1,
                    &graph.nodes()[source].id,
                    &graph.nodes()[target].id,
                    &edge.key,
                    CanonicalRecord(&edge.attributes),
                ),
            )?;
            group.2 += 1;
        }
        let mut previous = BTreeMap::new();
        let mut query = db.prepare_cached(
            "SELECT owner,digest FROM gf_projection_groups WHERE repository_id=?",
        )?;
        let mut rows = query.query([repository])?;
        while let Some(row) = rows.next()? {
            previous.insert(row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?);
        }
        let mut next = BTreeMap::new();
        let mut dirty = BTreeSet::new();
        let mut stats = Stats {
            groups: groups.len(),
            dirty_groups: 0,
            nodes_compared: 0,
            edges_compared: 0,
            total_nodes: graph.node_count(),
            total_edges: graph.edge_count(),
        };
        for (owner, (hash, nodes, edges)) in groups {
            let digest: [u8; 32] = hash.finalize().into();
            if previous.remove(owner).flatten().as_deref() != Some(digest.as_slice()) {
                dirty.insert(owner.to_owned());
                stats.nodes_compared += nodes;
                stats.edges_compared += edges;
            }
            next.insert(owner.to_owned(), digest);
        }
        dirty.extend(previous.into_keys());
        stats.dirty_groups = dirty.len();
        Ok(Self {
            dirty_json: serde_json::to_string(&dirty)?,
            stats,
            dirty,
            next,
        })
    }

    pub fn changed(&self, attributes: &Attributes) -> bool {
        self.dirty.contains(owner(attributes))
    }

    /// The owner groups (files) this projection rewrites, removed ones
    /// included.
    pub fn dirty(&self) -> &BTreeSet<String> {
        &self.dirty
    }

    /// Publish the cache only after all authoritative rows have been written.
    /// The caller keeps the plan and row updates in the same transaction.
    pub fn finish(&self, db: &Connection, repository: &str) -> Result<()> {
        let mut update = db.prepare_cached(
            "INSERT INTO gf_projection_groups(repository_id,owner,digest) VALUES(?,?,?)
            ON CONFLICT(repository_id,owner) DO UPDATE SET digest=excluded.digest",
        )?;
        let mut delete = db
            .prepare_cached("DELETE FROM gf_projection_groups WHERE repository_id=? AND owner=?")?;
        for owner in &self.dirty {
            if let Some(digest) = self.next.get(owner) {
                update.execute(params![repository, owner, digest.as_slice()])?;
            } else {
                delete.execute(params![repository, owner])?;
            }
        }
        Ok(())
    }
}
