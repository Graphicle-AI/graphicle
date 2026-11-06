//! `graphicle import-sqlite`: copy a SQLite store (`reviews.sqlite3`, from
//! Graphicle 0.1.12 or earlier) into the data folder's PostgreSQL store,
//! once. The only code that reads SQLite (feature `sqlite-import`).
//!
//! The data folder must not have a PostgreSQL store yet. The SQLite file is
//! opened read-only and checked (`PRAGMA quick_check`); the store is created
//! at this program's schema; then every table both have is copied in one
//! transaction, parents before children, each row's `rowid` kept, and the
//! identity sequences are moved past the copied ids. A copy that fails
//! leaves no store behind. The SQLite file is never changed or removed.

use crate::store::Store;
use crate::{Error, Result};
use rusqlite::types::ValueRef;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// SQLite tables that have no PostgreSQL counterpart, with the reason.
fn skipped_reason(table: &str) -> Option<&'static str> {
    if table == "repo_search" || table.starts_with("repo_search_") {
        return Some("the FTS5 index nothing queried; PostgreSQL stores have none");
    }
    if table.starts_with("sqlite_") {
        return Some("SQLite's own table");
    }
    None
}

/// One value in PostgreSQL's COPY text format.
fn copy_text(out: &mut Vec<u8>, value: ValueRef<'_>, column_type: &str) -> Result<()> {
    match value {
        ValueRef::Null => out.extend_from_slice(b"\\N"),
        ValueRef::Integer(v) => match column_type {
            "boolean" => out.extend_from_slice(if v != 0 { b"t" } else { b"f" }),
            _ => out.extend_from_slice(v.to_string().as_bytes()),
        },
        ValueRef::Real(v) => out.extend_from_slice(format!("{v:?}").as_bytes()),
        ValueRef::Text(t) => {
            if column_type == "bytea" {
                hex(out, t);
            } else {
                escape(out, t);
            }
        }
        ValueRef::Blob(b) => {
            if column_type == "bytea" {
                hex(out, b);
            } else {
                // A BLOB in a TEXT column (SQLite allows it): its UTF-8, or
                // refused when it has none.
                let text = std::str::from_utf8(b).map_err(|_| {
                    Error::bad(
                        "A binary value is stored in a text column; it cannot be copied as text",
                    )
                })?;
                escape(out, text.as_bytes());
            }
        }
    }
    Ok(())
}

fn escape(out: &mut Vec<u8>, text: &[u8]) {
    for &b in text {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0 => {}
            _ => out.push(b),
        }
    }
}

fn hex(out: &mut Vec<u8>, bytes: &[u8]) {
    // bytea's hex input, its backslash escaped for COPY: \\x…
    out.extend_from_slice(b"\\\\x");
    for b in bytes {
        out.extend_from_slice(format!("{b:02x}").as_bytes());
    }
}

/// Copy the SQLite store at `source` into a new PostgreSQL store for the
/// data folder `directory`.
pub fn import(source: &Path, directory: &Path) -> Result<Value> {
    if crate::db::Database::exists(directory) {
        return Err(Error::bad(
            "This data folder already has a PostgreSQL store; import into a new --data-dir",
        ));
    }
    if !source.is_file() || source.is_symlink() {
        return Err(Error::bad(format!(
            "{} is not a SQLite store file",
            source.display()
        )));
    }
    let sqlite = rusqlite::Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| Error::bad(format!("Could not open {}: {e}", source.display())))?;
    let sqlite_error =
        |e: rusqlite::Error| Error::bad(format!("Reading the SQLite store failed: {e}"));
    let check: String = sqlite
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(sqlite_error)?;
    if check != "ok" {
        return Err(Error::bad(format!(
            "The SQLite store failed its integrity check: {check}"
        )));
    }
    let mut source_tables: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut statement = sqlite
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .map_err(sqlite_error)?;
        let names: Vec<String> = statement
            .query_map([], |r| r.get(0))
            .map_err(sqlite_error)?
            .collect::<std::result::Result<_, _>>()
            .map_err(sqlite_error)?;
        for name in names {
            let mut columns = sqlite
                .prepare("SELECT name FROM pragma_table_info(?) ORDER BY cid")
                .map_err(sqlite_error)?;
            let list: Vec<String> = columns
                .query_map([&name], |r| r.get(0))
                .map_err(sqlite_error)?
                .collect::<std::result::Result<_, _>>()
                .map_err(sqlite_error)?;
            source_tables.insert(name, list);
        }
    }
    if !source_tables.contains_key("repo_projects") {
        return Err(Error::bad(format!(
            "{} is not a Graphicle store",
            source.display()
        )));
    }
    std::fs::create_dir_all(directory)?;
    let store = Store::open(directory)?;
    let outcome = copy_all(&sqlite, &source_tables, &store);
    if outcome.is_err() {
        let _ = store.pool.drop_schema();
        let _ = std::fs::remove_file(crate::db::schema_file(directory));
    }
    outcome
}

fn copy_all(
    sqlite: &rusqlite::Connection,
    source_tables: &BTreeMap<String, Vec<String>>,
    store: &Store,
) -> Result<Value> {
    let sqlite_error =
        |e: rusqlite::Error| Error::bad(format!("Reading the SQLite store failed: {e}"));
    let mut db = store.connect()?;
    let targets = crate::archive::table_order(&db)?;
    let tx = db.transaction()?;
    // Rows are copied as they are: the projection-group triggers, which
    // mark a group stale on any other write, stay out of it.
    tx.execute_batch("SET LOCAL graphicle.projecting = 'on'")?;
    let names: Vec<String> = targets.iter().map(|(t, _)| format!("\"{t}\"")).collect();
    tx.execute_batch(&format!("TRUNCATE {} CASCADE", names.join(",")))?;
    let mut copied = BTreeMap::new();
    let mut not_copied = BTreeMap::new();
    let target_names: BTreeSet<&str> = targets.iter().map(|(t, _)| t.as_str()).collect();
    for (table, reason) in source_tables
        .keys()
        .filter_map(|t| skipped_reason(t).map(|r| (t, r)))
    {
        not_copied.insert(table.clone(), json!(reason));
    }
    for name in source_tables.keys() {
        if skipped_reason(name).is_none() && !target_names.contains(name.as_str()) {
            return Err(Error::bad(format!(
                "The SQLite store has table {name}, which this version of Graphicle does not know; import it with the version that wrote it"
            )));
        }
    }
    for (table, columns) in &targets {
        let Some(present) = source_tables.get(table) else {
            continue;
        };
        // The columns both have, and the SQLite rowid for the hidden
        // `rowid` identity (tables whose `rowid` is their `id` recompute it).
        let mut copied_columns: Vec<(String, String)> = Vec::new();
        for (column, kind) in columns {
            if column == "rowid" || present.contains(column) {
                copied_columns.push((column.clone(), kind.clone()));
            }
        }
        if copied_columns.is_empty() {
            continue;
        }
        let select: Vec<String> = copied_columns
            .iter()
            .map(|(c, _)| {
                if c == "rowid" {
                    "rowid".to_string()
                } else {
                    format!("\"{c}\"")
                }
            })
            .collect();
        let mut statement = sqlite
            .prepare(&format!(
                "SELECT {} FROM \"{table}\" ORDER BY rowid",
                select.join(",")
            ))
            .map_err(sqlite_error)?;
        let mut rows = statement.query([]).map_err(sqlite_error)?;
        let mut data = Vec::new();
        let mut count = 0u64;
        let flush = |data: &mut Vec<u8>| -> Result<()> {
            if !data.is_empty() {
                let list: Vec<String> = copied_columns
                    .iter()
                    .map(|(c, _)| format!("\"{c}\""))
                    .collect();
                tx.copy_in(
                    &format!("COPY \"{table}\" ({}) FROM STDIN", list.join(",")),
                    &mut data.as_slice(),
                )
                .map_err(|e| Error::bad(format!("Copying table {table} failed: {e}")))?;
                data.clear();
            }
            Ok(())
        };
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            for (i, (column, kind)) in copied_columns.iter().enumerate() {
                if i > 0 {
                    data.push(b'\t');
                }
                let value = row.get_ref(i).map_err(sqlite_error)?;
                copy_text(&mut data, value, kind).map_err(|e| {
                    Error::bad(format!("Table {table}, column {column}: {}", e.message))
                })?;
            }
            data.push(b'\n');
            count += 1;
            if data.len() >= 8 << 20 {
                flush(&mut data)?;
            }
        }
        flush(&mut data)?;
        copied.insert(table.clone(), count);
    }
    crate::archive::reset_identities(&tx)?;
    tx.commit()?;
    let rows: u64 = copied.values().sum();
    Ok(
        json!({"imported":true,"tables":copied.len(),"rows":rows,"copied":copied,"not_copied":not_copied,
        "data_dir":store.directory.as_ref(),"schema":store.schema(),
        "next":"Check the projects with graphicle status; the SQLite file is unchanged, and Graphicle no longer reads it."}),
    )
}
