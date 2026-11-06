//! Exact file-group fingerprints for incremental projection persistence.
//!
//! The complete assembled graph is still the authority. Hash every final
//! record, including global normalization effects, then load/compare SQLite
//! records only for groups that differ. SQL triggers invalidate fingerprints
//! on every row mutation, so this is a cache rather than a second authority.
use crate::db::{Connection, params};
use crate::{Result, graph_projection::CanonicalRecord, store::ensure_column};
use graphicle::{graph::Graph, record::Attributes};
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
    let existed = crate::store::table_exists(db, "gf_projection_groups")?;
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
                 INSERT INTO gf_projection_groups(repository_id,owner)
                   SELECT DISTINCT repository_id,owner FROM {table} WHERE valid_to_episode IS NULL ON CONFLICT DO NOTHING;"))?;
        }
    }
    install_triggers(db)?;
    transaction.commit()?;
    Ok(())
}

/// SQLite's cache triggers, three per table, which a projection drops
/// inside its savepoint and installs again.
fn install_sqlite_triggers(db: &Connection) -> Result<()> {
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

fn install_triggers(db: &Connection) -> Result<()> {
    if db.engine() == crate::db::Engine::Sqlite {
        return install_sqlite_triggers(db);
    }
    // One trigger function for both tables: a row written, changed or
    // removed outside a projection marks its group's cached digest stale.
    // A projection, which knows every group it changes, writes with
    // `graphicle.projecting` set and is not slowed by them
    // ([`ProjectionWrite`]).
    db.execute_batch(
        "CREATE OR REPLACE FUNCTION graphicle_group_dirty() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF coalesce(current_setting('graphicle.projecting', true), '') = 'on' THEN
             RETURN NULL;
           END IF;
           IF TG_OP IN ('UPDATE', 'DELETE') THEN
             UPDATE gf_projection_groups SET digest = NULL
              WHERE repository_id = OLD.repository_id AND owner = OLD.owner AND digest IS NOT NULL;
           END IF;
           IF TG_OP IN ('INSERT', 'UPDATE') THEN
             INSERT INTO gf_projection_groups(repository_id, owner, digest) VALUES (NEW.repository_id, NEW.owner, NULL)
               ON CONFLICT (repository_id, owner) DO UPDATE SET digest = NULL
               WHERE gf_projection_groups.digest IS NOT NULL;
           END IF;
           RETURN NULL;
         END $$",
    )?;
    for table in ["gf_nodes", "gf_edges"] {
        db.execute_batch(&format!(
            "CREATE OR REPLACE TRIGGER {table}_group AFTER INSERT OR UPDATE OR DELETE ON {table}
               FOR EACH ROW EXECUTE FUNCTION graphicle_group_dirty()"
        ))?;
    }
    Ok(())
}

/// Keep the authoritative projection and its cache publication atomic.
///
/// On SQLite, row triggers impose a statement journal on every insert: the
/// projector, which already knows every changed group, removes its cache
/// triggers inside a savepoint. SQLite serializes writers and
/// transactionally restores the schema on rollback; other connections never
/// see a committed state without the triggers.
///
/// On PostgreSQL, row triggers would cost a statement per inserted row. The projector
/// already knows every changed group, so its writes run in a savepoint with
/// `graphicle.projecting` set for the transaction, which the triggers skip.
/// The setting ends with the savepoint (released or rolled back); other
/// connections never see it. Unrelated SQL writes retain normal
/// invalidation.
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
        if db.engine() == crate::db::Engine::Sqlite {
            for table in ["gf_nodes", "gf_edges"] {
                for action in ["insert", "update", "delete"] {
                    db.execute_batch(&format!("DROP TRIGGER IF EXISTS {table}_group_{action}"))?;
                }
            }
        } else {
            db.execute_batch("SET LOCAL graphicle.projecting = 'on'")?;
        }
        Ok(write)
    }

    pub fn commit(mut self) -> Result<()> {
        if self.db.engine() == crate::db::Engine::Sqlite {
            install_sqlite_triggers(self.db)?;
            self.db
                .execute_batch("RELEASE graphicle_graph_projection")?;
        } else {
            self.db.execute_batch(
                "SET LOCAL graphicle.projecting = 'off'; RELEASE graphicle_graph_projection",
            )?;
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for ProjectionWrite<'_> {
    fn drop(&mut self) {
        if !self.committed && !self.db.is_autocommit() {
            // A SQL error or panic must not leave partial rows or skipped
            // triggers behind, even if the caller commits its outer
            // transaction (the rollback also ends the setting).
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
