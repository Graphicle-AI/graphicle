//! Owner-only namespace lifecycle. Exports are deterministic, SHA-256 verified
//! archives of every private row. Restores run in a single transaction and
//! refuse conflicts. A credential is never exported, so rotated credentials
//! continue to refer to the same owner-controlled namespace.
use crate::db::{Connection, Value as DbValue, params};
use crate::store::{self, Principal, Store, one, rows};
use crate::{Error, Result, digest, now};
use serde_json::{Value, json};

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
        let items = rows(db, &sql, params![namespace, workspace])?;
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

fn restore(db: &Connection, workspace: &str, namespace: &str, input: &Value) -> Result<Value> {
    exists(db, workspace, namespace)?;
    let payload = input
        .get("archive")
        .ok_or_else(|| Error::bad("Missing archive"))?;
    if payload["format"] != "graphicle.namespace.v1"
        || payload["workspace_id"] != workspace
        || payload["memory_namespace"] != namespace
        || payload["self_review"] != exists(db, workspace, namespace)?["self_review"]
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
    if serde_json::to_vec(archived)?.len() > 64 * 1024 * 1024 {
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
            if object.contains_key("rowid") {
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
                    Value::Number(n) if n.is_i64() => Ok(DbValue::Integer(n.as_i64().unwrap())),
                    Value::Number(n) if n.is_f64() => Ok(DbValue::Real(n.as_f64().unwrap())),
                    _ => Err(Error::bad("Archive has an unsupported SQL value")),
                })
                .collect::<Result<_>>()?;
            if db.execute(&sql, values)? != 1 {
                return Err(Error::conflict("Archive insertion was incomplete"));
            }
        }
    }
    Ok(json!({"restored":total(&contents(db,workspace,namespace)?),"sha256":input["sha256"]}))
}
fn erase(db: &Connection, p: &Principal, namespace: &str) -> Result<Value> {
    let mut removed = serde_json::Map::new();
    let records = contents(db, &p.workspace, namespace)?;
    let keys = records["repo_memories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    for &(table, column) in TABLES.iter().rev() {
        if !store::table_exists(db, table)? {
            continue;
        }
        let sql = format!("DELETE FROM {table} WHERE {}", predicate(table, column));
        let count = db.execute(&sql, params![namespace, &p.workspace])?;
        if count > 0 {
            removed.insert(table.to_string(), json!(count));
        }
    }
    let remaining = contents(db, &p.workspace, namespace)?;
    let remaining_count = total(&remaining);
    if remaining_count != 0 {
        return Err(Error::conflict("Namespace deletion left database residue"));
    }
    // Attach the existing hash-chained deletion receipt to the workspace.
    // A placeholder repository identifier is valid for a namespace spanning
    // several projects: receipts do not FK to a repository and outlive it.
    let receipt = crate::deletion::rows_receipt(
        db,
        p,
        "namespace",
        "memory.namespace.delete",
        namespace,
        json!({"rule":"owner_namespace_cascade","sha256":digest(&records)}),
        json!(removed),
        &keys,
        json!({"namespace_rows":0}),
        false,
    )?;
    db.execute(
        "UPDATE am_memory_namespaces SET last_write_at=? WHERE workspace_id=? AND name=?",
        params![now(), p.workspace, namespace],
    )?;
    Ok(json!({"deleted":total(&records),"remaining":remaining_count,"receipt":receipt}))
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
        let mut result = self.authorized(workspace, token, "admin", |db, p| {
            authorize(p, namespace)?;
            erase(db, p, namespace)
        })?;
        result["storage"] = crate::deletion::checkpoint(self)?;
        Ok(result)
    }
}
