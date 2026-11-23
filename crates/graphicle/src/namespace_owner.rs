//! Owner-only namespace lifecycle. Exports are deterministic, SHA-256 verified
//! archives of every private row. Restores run in a single transaction and
//! refuse conflicts. A credential is never exported, so rotated credentials
//! continue to refer to the same owner-controlled namespace.
use crate::db::{Connection, Value as DbValue, params};
use crate::store::{self, Principal, Store, one, rows};
use crate::{Error, Result, digest, now};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Parents first; delete walks it in reverse.
use crate::memory_scope::TABLES;

fn authorize(p: &Principal, namespace: &str) -> Result<()> {
    if p.role != "owner" {
        return Err(Error::denied());
    }
    crate::memory_scope::Namespace::parse(namespace).map_err(Error::bad)?;
    Ok(())
}
fn exists(db: &Connection, workspace: &str, namespace: &str) -> Result<Value> {
    one(db, "SELECT self_review,created_at,last_read_at,last_write_at FROM am_memory_namespaces WHERE workspace_id=? AND name=?",
        params![workspace,namespace])?.ok_or_else(||Error::bad("Memory namespace unavailable"))
}
fn predicate(table: &str, column: &str) -> String {
    let inner = match column {
        "memory_id" => {
            "SELECT m.id FROM repo_memories m JOIN repo_projects p ON p.id=m.repository_id WHERE p.workspace_id=?"
        }
        "run_id" => {
            "SELECT r.id FROM repo_runs r JOIN repo_projects p ON p.id=r.repository_id WHERE p.workspace_id=?"
        }
        "job_id" => {
            "SELECT j.id FROM am_jobs j JOIN repo_projects p ON p.id=j.repository_id WHERE p.workspace_id=?"
        }
        _ => "SELECT id FROM repo_projects WHERE workspace_id=?",
    };
    format!("{table}.memory_namespace=? AND {table}.{column} IN ({inner})")
}
fn contents(db: &Connection, workspace: &str, namespace: &str) -> Result<Value> {
    let mut output = serde_json::Map::new();
    for &(table, column) in TABLES {
        if !store::table_exists(db, table)? {
            continue;
        }
        let sql = format!(
            "SELECT {table}.* FROM {table} WHERE {} ORDER BY 1",
            predicate(table, column)
        );
        let mut items = rows(db, &sql, params![namespace, workspace])?;
        // Sort by canonical JSON so multi-column-PK tables (where column 1 is
        // `repository_id`) export in one deterministic order across engines.
        items.sort_by_cached_key(|row| serde_json::to_string(row).unwrap_or_default());
        if !items.is_empty() {
            output.insert(table.to_string(), json!(items));
        }
    }
    Ok(Value::Object(output))
}
fn total(rows: &Value) -> usize {
    rows.as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .map(|v| v.as_array().map_or(0, Vec::len))
        .sum()
}
fn archive(db: &Connection, workspace: &str, namespace: &str) -> Result<Value> {
    let policy = exists(db, workspace, namespace)?;
    let payload = json!({
        "format":"graphicle.namespace.v1", "workspace_id":workspace,
        "memory_namespace":namespace, "self_review":policy["self_review"],
        "tables":contents(db,workspace,namespace)?,
    });
    // Every row is represented as a machine-readable record, and the digest
    // is of Graphicle's canonical JSON, not the transport's whitespace.
    Ok(json!({"archive":payload,"sha256":digest(&payload),"rows":total(&payload["tables"])}))
}
fn stats(db: &Connection, workspace: &str, namespace: &str) -> Result<Value> {
    let policy = exists(db, workspace, namespace)?;
    let tables = contents(db, workspace, namespace)?;
    let mut by_table = serde_json::Map::new();
    let mut bytes = 0usize;
    for (name, values) in tables.as_object().into_iter().flat_map(|m| m.iter()) {
        let items = values.as_array().expect("only table arrays");
        let size: usize = items
            .iter()
            .map(|r| serde_json::to_vec(r).map_or(0, |v| v.len()))
            .sum();
        by_table.insert(name.clone(), json!({"rows":items.len(),"bytes":size}));
        bytes += size;
    }
    Ok(
        json!({"memory_namespace":namespace,"rows":total(&tables),"bytes":bytes,
        "tables":by_table,"self_review":policy["self_review"],
        "last_read_at":policy["last_read_at"],"last_write_at":policy["last_write_at"]}),
    )
}

/// The largest namespace archive `import` restores.
pub const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;

/// The body limit of the import route: an archive at [`ARCHIVE_LIMIT`] and
/// its envelope (the server's default limit is smaller).
pub const IMPORT_BODY_LIMIT: usize = ARCHIVE_LIMIT + 1024 * 1024;

fn restore(db: &Connection, workspace: &str, namespace: &str, input: &Value) -> Result<Value> {
    let policy = exists(db, workspace, namespace)?;
    store::lock_namespace_for_erase(db, workspace, namespace)?;
    let payload = input
        .get("archive")
        .ok_or_else(|| Error::bad("Missing archive"))?;
    if payload["format"] != "graphicle.namespace.v1"
        || payload["workspace_id"] != workspace
        || payload["memory_namespace"] != namespace
        || payload["self_review"] != policy["self_review"]
        || input["sha256"].as_str() != Some(digest(payload).as_str())
    {
        return Err(Error::bad("Namespace archive identity or SHA-256 mismatch"));
    }
    let archived = payload["tables"]
        .as_object()
        .ok_or_else(|| Error::bad("Invalid archive tables"))?;
    if archived
        .keys()
        .any(|key| !TABLES.iter().any(|(name, _)| *name == key))
    {
        return Err(Error::bad("Unknown namespace archive table"));
    }
    if serde_json::to_vec(archived)?.len() > ARCHIVE_LIMIT {
        return Err(Error::bad("Archive exceeds 64 MiB"));
    }
    // A restore is deliberately empty-target only. Never merge or silently
    // skip conflicts; a partial restore could lose remembered revisions.
    if total(&contents(db, workspace, namespace)?) != 0 {
        return Err(Error::conflict("Namespace must be empty before import"));
    }
    for &(table, column) in TABLES {
        let Some(values) = archived.get(table) else {
            continue;
        };
        let rows = values
            .as_array()
            .ok_or_else(|| Error::bad("Archive table must be a list"))?;
        for row in rows {
            let object = row
                .as_object()
                .ok_or_else(|| Error::bad("Archive row must be an object"))?;
            if row["memory_namespace"] != namespace {
                return Err(Error::bad("Archive contains foreign-namespace data"));
            }
            let id = row[column]
                .as_str()
                .ok_or_else(|| Error::bad("Archive row has no parent"))?;
            let sql = match column {
                "memory_id" => {
                    "SELECT 1 FROM repo_memories m JOIN repo_projects p ON p.id=m.repository_id WHERE m.id=? AND p.workspace_id=? AND m.memory_namespace=?"
                }
                "run_id" => {
                    "SELECT 1 FROM repo_runs r JOIN repo_projects p ON p.id=r.repository_id WHERE r.id=? AND p.workspace_id=? AND r.memory_namespace=?"
                }
                "job_id" => {
                    "SELECT 1 FROM am_jobs j JOIN repo_projects p ON p.id=j.repository_id WHERE j.id=? AND p.workspace_id=? AND j.memory_namespace=?"
                }
                _ => "SELECT 1 FROM repo_projects WHERE id=? AND workspace_id=?",
            };
            // Parents must be in this workspace. Later tables can refer to
            // parents restored earlier in this same transaction.
            let parent = if column == "memory_id" || column == "run_id" || column == "job_id" {
                one(db, sql, params![id, workspace, namespace])?
            } else {
                one(db, sql, params![id, workspace])?
            };
            if parent.is_none() {
                return Err(Error::bad(
                    "Archive references an unavailable project or parent",
                ));
            }
            // A namespace's passage-index rows are its memories' passages
            // only: a source passage is shared, and is never the namespace's
            // to restore. Its memory is in this archive, restored before it
            // (parents first), in the same project.
            if table == "am_vectors" {
                let memory = row["identity"]
                    .as_str()
                    .and_then(|identity| identity.strip_prefix("memory:"))
                    .ok_or_else(|| {
                        Error::bad("Archive has a vector of no memory of the namespace")
                    })?;
                let restored = one(
                    db,
                    "SELECT 1 FROM repo_memories WHERE id=? AND repository_id=? AND memory_namespace=?",
                    params![memory, id, namespace],
                )?;
                if restored.is_none() {
                    return Err(Error::bad(
                        "Archive has a vector of no memory of the namespace",
                    ));
                }
            }
            if table == "am_claims" && !row["memory_id"].is_null() {
                let memory = row["memory_id"]
                    .as_str()
                    .ok_or_else(|| Error::bad("Archive claim has an invalid memory"))?;
                if one(
                    db,
                    "SELECT 1 FROM repo_memories WHERE id=? AND repository_id=? AND memory_namespace=?",
                    params![memory, id, namespace],
                )?
                .is_none()
                {
                    return Err(Error::bad(
                        "Archive claim references an unavailable memory",
                    ));
                }
            }
            if table == "am_entity_links" {
                let claim = row["claim_id"]
                    .as_str()
                    .ok_or_else(|| Error::bad("Archive link has no claim"))?;
                let subject = row["subject_id"]
                    .as_str()
                    .ok_or_else(|| Error::bad("Archive link has no subject"))?;
                let object_id = row["object_id"]
                    .as_str()
                    .ok_or_else(|| Error::bad("Archive link has no object"))?;
                if one(
                    db,
                    "SELECT 1 FROM am_claims WHERE id=? AND repository_id=? AND memory_namespace=?",
                    params![claim, id, namespace],
                )?
                .is_none()
                    || one(
                        db,
                        "SELECT 1 FROM am_entities WHERE id=? AND repository_id=? AND memory_namespace=?",
                        params![subject, id, namespace],
                    )?
                    .is_none()
                    || one(
                        db,
                        "SELECT 1 FROM am_entities WHERE id=? AND repository_id=? AND memory_namespace=?",
                        params![object_id, id, namespace],
                    )?
                    .is_none()
                {
                    return Err(Error::bad(
                        "Archive entity link references an unavailable parent",
                    ));
                }
            }
            if table == "am_alias_proposals" {
                let source = row["source_id"]
                    .as_str()
                    .ok_or_else(|| Error::bad("Archive alias has no source"))?;
                let target = row["target_id"]
                    .as_str()
                    .ok_or_else(|| Error::bad("Archive alias has no target"))?;
                if one(
                    db,
                    "SELECT 1 FROM am_entities WHERE id=? AND repository_id=? AND memory_namespace=?",
                    params![source, id, namespace],
                )?
                .is_none()
                    || one(
                        db,
                        "SELECT 1 FROM am_entities WHERE id=? AND repository_id=? AND memory_namespace=?",
                        params![target, id, namespace],
                    )?
                    .is_none()
                {
                    return Err(Error::bad(
                        "Archive alias references an unavailable entity",
                    ));
                }
            }
            if table == "am_claim_events" {
                let claim = row["claim_id"]
                    .as_str()
                    .ok_or_else(|| Error::bad("Archive claim event has no claim"))?;
                if one(
                    db,
                    "SELECT 1 FROM am_claims WHERE id=? AND repository_id=? AND memory_namespace=?",
                    params![claim, id, namespace],
                )?
                .is_none()
                {
                    return Err(Error::bad(
                        "Archive claim event references an unavailable claim",
                    ));
                }
            }
            if table == "am_model_calls" && !row["job_id"].is_null() {
                let job = row["job_id"]
                    .as_str()
                    .ok_or_else(|| Error::bad("Archive model call has an invalid job"))?;
                if one(
                    db,
                    "SELECT 1 FROM am_jobs WHERE id=? AND repository_id=? AND memory_namespace=?",
                    params![job, id, namespace],
                )?
                .is_none()
                {
                    return Err(Error::bad(
                        "Archive model call references an unavailable job",
                    ));
                }
            }
            if object.keys().any(|k| {
                k.eq_ignore_ascii_case("rowid")
                    || k.eq_ignore_ascii_case("_rowid_")
                    || k.eq_ignore_ascii_case("oid")
            }) {
                return Err(Error::bad("Archive must not override database rowids"));
            }
            let keys: Vec<&String> = object.keys().collect();
            // Columns, though archived, must be identifiers from the actual
            // database schema rather than untrusted SQL fragments.
            if keys
                .iter()
                .any(|k| !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            {
                return Err(Error::bad("Archive contains an invalid column name"));
            }
            let columns = keys
                .iter()
                .map(|k| format!("\"{k}\""))
                .collect::<Vec<_>>()
                .join(",");
            let marks = std::iter::repeat_n("?", keys.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("INSERT INTO {table}({columns}) VALUES({marks})");
            let values: Vec<DbValue> = keys
                .iter()
                .map(|key| match &object[*key] {
                    Value::Null => Ok(DbValue::Null),
                    Value::String(s) => Ok(DbValue::Text(s.clone())),
                    Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            Ok(DbValue::Integer(i))
                        } else if let Some(f) = n.as_f64() {
                            Ok(DbValue::Real(f))
                        } else {
                            Err(Error::bad("Archive has an unsupported SQL value"))
                        }
                    }
                    _ => Err(Error::bad("Archive has an unsupported SQL value")),
                })
                .collect::<Result<_>>()?;
            if db.execute(&sql, values)? != 1 {
                return Err(Error::conflict("Archive insertion was incomplete"));
            }
        }
    }
    // Re-register restored memories' dependency edges and symbol pins, which
    // are derived indexes outside `TABLES` and were removed on erase.
    for memory in archived
        .get("repo_memories")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let repository = store::s(memory, "repository_id");
        let identifier = store::s(memory, "id");
        let data: Value = serde_json::from_str(store::s(memory, "data")).unwrap_or(Value::Null);
        crate::invalidation::record_memory(db, workspace, repository, identifier, &data)?;
        crate::symbols::record_pins(db, repository, identifier, &data)?;
    }
    if db.engine() == crate::db::Engine::Postgres {
        crate::archive::reset_identities(db)?;
    }
    db.execute(
        "UPDATE am_memory_namespaces SET last_write_at=? WHERE workspace_id=? AND name=?",
        params![now(), workspace, namespace],
    )?;
    Ok(json!({"restored":total(&contents(db,workspace,namespace)?),"sha256":input["sha256"]}))
}
/// Delete everything the namespace holds, in every project of the
/// workspace: its rows ([`TABLES`]), what still names its memories outside
/// them (served and outcome records, hints, the dependency index), its
/// vectors in the projects' vector backends (a full clear, rebuilt at the next
/// publication) and its key space in the embedding cache; and revoke its keys.
/// The namespace itself stays, with its `self_review` policy, so that its
/// archive can be imported again; new keys for it are the owner's to make.
fn erase(db: &Connection, p: &Principal, namespace: &str) -> Result<(Value, BTreeSet<String>)> {
    exists(db, &p.workspace, namespace)?;
    // On PostgreSQL, serialize against any in-flight write in `namespace` so
    // no READ COMMITTED transaction that authorized earlier can commit a row
    // after `erase` deletes the namespace and revokes its keys.
    store::lock_namespace_for_erase(db, &p.workspace, namespace)?;
    let records = contents(db, &p.workspace, namespace)?;
    let mut keys: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for memory in records["repo_memories"].as_array().into_iter().flatten() {
        keys.entry(store::s(memory, "repository_id").to_string())
            .or_default()
            .push(store::s(memory, "id").to_string());
    }
    // Every project the namespace wrote to may hold its vectors.
    let projects: BTreeSet<String> = records
        .as_object()
        .into_iter()
        .flat_map(|tables| tables.values())
        .flat_map(|rows| rows.as_array().into_iter().flatten())
        .filter_map(|row| row["repository_id"].as_str().map(str::to_string))
        .collect();
    let mut removed: BTreeMap<String, usize> = BTreeMap::new();
    for (repository, ids) in &keys {
        crate::deletion::strip_memory_references(
            db,
            &p.workspace,
            repository,
            ids,
            Some(Some(namespace)),
            &mut removed,
        )?;
    }
    // Remove dependency-index entries and stale markers for the namespace's
    // cached model answers and its wiki articles across the workspace.
    for cached in records["am_model_cache"].as_array().into_iter().flatten() {
        let key = format!(
            "{}:{}",
            store::s(cached, "repository_id"),
            store::s(cached, "request_hash")
        );
        crate::invalidation::forget_dependent(
            db,
            &p.workspace,
            (crate::invalidation::ANSWER, &key),
        )?;
        crate::invalidation::clear_stale(db, &p.workspace, crate::invalidation::ANSWER, &key)?;
    }
    for project in rows(
        db,
        "SELECT id FROM repo_projects WHERE workspace_id=? ORDER BY id",
        [&p.workspace],
    )? {
        let wiki_prefix = format!("wiki:{}:namespace:{namespace}:", store::s(&project, "id"));
        db.execute(
            "DELETE FROM gf_dependencies WHERE workspace=?1 AND dependent_kind=?2 AND substr(dependent_id,1,length(?3))=?3",
            params![&p.workspace, crate::invalidation::SUMMARY, &wiki_prefix],
        )?;
        crate::invalidation::clear_stale_prefix(
            db,
            &p.workspace,
            crate::invalidation::SUMMARY,
            &wiki_prefix,
        )?;
    }
    for &(table, column) in TABLES.iter().rev() {
        if !store::table_exists(db, table)? {
            continue;
        }
        let sql = format!("DELETE FROM {table} WHERE {}", predicate(table, column));
        let count = db.execute(&sql, params![namespace, &p.workspace])?;
        if count > 0 {
            *removed.entry(table.to_string()).or_default() += count;
        }
    }
    let remaining = contents(db, &p.workspace, namespace)?;
    let remaining_count = total(&remaining);
    if remaining_count != 0 {
        return Err(Error::conflict("Namespace deletion left database residue"));
    }
    // The namespace's keys go with its memory: a key left valid would write
    // into the emptied namespace, or read an import of it.
    let credentials = rows(
        db,
        "SELECT id FROM credentials WHERE workspace_id=? AND memory_namespace=? AND revoked_at IS NULL ORDER BY id",
        params![p.workspace, namespace],
    )?;
    for credential in &credentials {
        store::revoke_credential(db, &p.workspace, store::s(credential, "id"))?;
    }
    for repository in &projects {
        crate::projection::enqueue(db, repository, "memory_namespace_deleted")?;
    }
    // Outside this transaction (another file, or another schema): deleted
    // first, so that a failure here leaves the namespace as it was.
    let cache = crate::embedding_cache::forget_namespace(&p.workspace, namespace)?;
    let receipt = crate::deletion::namespace_receipt(
        db,
        p,
        namespace,
        json!({"rule":"owner_namespace_cascade","sha256":digest(&records)}),
        json!(removed),
        &keys,
        json!({"namespace_rows":remaining_count}),
        &projects,
        json!({"credentials_revoked":credentials.len(),
            "embedding_cache":cache.map_or(json!("not_configured"), |n| json!({"entries_removed":n}))}),
    )?;
    db.execute(
        "UPDATE am_memory_namespaces SET last_write_at=? WHERE workspace_id=? AND name=?",
        params![now(), p.workspace, namespace],
    )?;
    Ok((
        json!({"deleted":total(&records),"remaining":remaining_count,
        "credentials_revoked":credentials.len(),"receipt":receipt}),
        projects,
    ))
}

impl Store {
    pub fn namespace_stats(&self, workspace: &str, token: &str, namespace: &str) -> Result<Value> {
        self.authorized_read(workspace, token, "admin", |db, p| {
            authorize(p, namespace)?;
            stats(db, workspace, namespace)
        })
    }
    pub fn namespace_export(&self, workspace: &str, token: &str, namespace: &str) -> Result<Value> {
        self.authorized_read(workspace, token, "admin", |db, p| {
            authorize(p, namespace)?;
            archive(db, workspace, namespace)
        })
    }
    pub fn namespace_import(
        &self,
        workspace: &str,
        token: &str,
        namespace: &str,
        args: &Value,
    ) -> Result<Value> {
        self.authorized(workspace, token, "admin", |db, p| {
            authorize(p, namespace)?;
            restore(db, workspace, namespace, args)
        })
    }
    pub fn namespace_delete(&self, workspace: &str, token: &str, namespace: &str) -> Result<Value> {
        let (mut result, projects) = self.authorized(workspace, token, "admin", |db, p| {
            authorize(p, namespace)?;
            erase(db, p, namespace)
        })?;
        for repository in &projects {
            graphicle_memory::cache::clear(repository).map_err(Error::bad)?;
        }
        result["storage"] = crate::deletion::checkpoint(self)?;
        Ok(result)
    }
}
