//! Store archives: the file `graphicle backup` writes and `graphicle
//! restore` reads, and the scratch schemas that backups, restores,
//! offboarding and `graphicle import-sqlite` work in.
//!
//! An archive is one file, written front to back and read the same way:
//!
//! ```text
//! GRAPHICLE-STORE-ARCHIVE 1\n
//! {"format":"graphicle.store-archive.v1","version":…,"created_at":…}\n
//! for each table:
//!   {"table":"…","columns":["…",…]}\n
//!   chunks of the table's rows in COPY text format: "<bytes>\n" then the
//!   bytes; a chunk of 0 bytes ("0\n") ends the table
//! {"end":true,"tables":N,"rows":N,"sha256":"…"}\n
//! ```
//!
//! The trailer's `sha256` is the digest of every byte before it, so a
//! truncated or altered archive is refused before anything is loaded
//! ([`verify`]). Generated columns are left out and recomputed on load;
//! identity sequences are moved past the loaded rows.

use crate::db::{Connection, Database, Settings};
use crate::{Error, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;

const MAGIC: &str = "GRAPHICLE-STORE-ARCHIVE 1\n";
pub const FORMAT: &str = "graphicle.store-archive.v1";
const CHUNK: usize = 1 << 20;

/// A schema made for one piece of work and dropped with it (or, for a
/// SQLite backup file, a private copy of the file, removed with it).
pub struct Scratch {
    pub database: Arc<Database>,
    /// The tables of the archive it was loaded from ([`open_scratch`]):
    /// written back, an archive keeps the tables it had.
    pub tables: Option<BTreeSet<String>>,
    /// The folder of a SQLite copy.
    copy: Option<tempfile::TempDir>,
}

impl Scratch {
    /// A new, empty scratch schema.
    pub fn new() -> Result<Self> {
        let settings = Settings::from_environment()?;
        let schema = format!("g_{}", &uuid::Uuid::new_v4().simple().to_string()[..24]);
        // Marked as scratch with its creation time, so that one a crash
        // left behind (a copy of a store that later deletions would never
        // reach) is dropped by the next scratch made a day later.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        crate::db::admin(
            &settings,
            &format!(
                "CREATE SCHEMA \"{schema}\"; COMMENT ON SCHEMA \"{schema}\" IS 'graphicle scratch {now}'"
            ),
        )?;
        let database = Database::open(&settings, &schema)?;
        let _ = drop_stale_scratch(&database.connect()?, now);
        Ok(Self {
            database,
            tables: None,
            copy: None,
        })
    }
    pub fn connect(&self) -> Result<Connection> {
        Ok(self.database.connect()?)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.copy.is_none() {
            let _ = self.database.drop_schema();
        }
    }
}

/// Drop the scratch schemas of this role made more than a day before `now`.
fn drop_stale_scratch(db: &Connection, now: u64) -> Result<()> {
    let mut statement = db.prepare(
        "SELECT n.nspname, d.description FROM pg_namespace n
         JOIN pg_description d ON d.objoid=n.oid AND d.classoid='pg_namespace'::regclass
         WHERE d.description LIKE 'graphicle scratch %' AND n.nspowner=(SELECT oid FROM pg_roles WHERE rolname=current_user)",
    )?;
    let found =
        statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in found {
        let (schema, description) = row?;
        let made = description
            .rsplit(' ')
            .next()
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        if crate::db::valid_schema(&schema) && made.saturating_add(86_400) < now {
            db.execute_batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))?;
        }
    }
    Ok(())
}

/// A table of a schema: its name, the columns an archive carries (all but
/// generated ones) and the tables its foreign keys reference.
#[derive(Debug)]
struct Table {
    name: String,
    columns: Vec<String>,
    references: BTreeSet<String>,
}

/// `name` quoted as one SQL identifier.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `table` of the connection's own schema, qualified: an unqualified name
/// would resolve to a `pg_catalog` table of that name first.
fn qualified(db: &Connection, table: &str) -> String {
    format!("{}.{}", quote(&db.database().schema), quote(table))
}

/// A table or column name an archive may carry: a plain identifier, and
/// never one of PostgreSQL's own (`pg_…`).
fn plain_name(name: &str) -> bool {
    crate::db::valid_identifier(name) && !name.to_ascii_lowercase().starts_with("pg_")
}

/// One line of an archive, at most 1 MiB, hashed unless it is the trailer.
fn read_line_bounded(input: &mut BufReader<std::fs::File>, line: &mut String) -> Result<usize> {
    line.clear();
    let n = input.by_ref().take(1 << 20).read_line(line)?;
    if n >= 1 << 20 && !line.ends_with('\n') {
        return Err(Error::bad("This is not a valid Graphicle store backup"));
    }
    Ok(n)
}

/// The tables of the connection's schema, parents before the tables whose
/// foreign keys reference them.
fn tables(db: &Connection) -> Result<Vec<Table>> {
    let mut statement = db.prepare(
        "SELECT c.relname,
                coalesce((SELECT string_agg(a.attname, ',' ORDER BY a.attnum) FROM pg_attribute a
                          WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped AND a.attgenerated=''),'') AS columns,
                coalesce((SELECT string_agg(DISTINCT r.relname, ',') FROM pg_constraint k JOIN pg_class r ON r.oid=k.confrelid
                          WHERE k.conrelid=c.oid AND k.contype='f' AND r.oid<>c.oid),'') AS refs
         FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=current_schema() AND c.relkind='r' ORDER BY c.relname",
    )?;
    let found = statement.query_map([], |r| {
        let split = |s: String| -> Vec<String> {
            s.split(',')
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect()
        };
        Ok(Table {
            name: r.get(0)?,
            columns: split(r.get(1)?),
            references: split(r.get(2)?).into_iter().collect(),
        })
    })?;
    let mut pending: BTreeMap<String, Table> = BTreeMap::new();
    for table in found {
        let table = table?;
        pending.insert(table.name.clone(), table);
    }
    let names: BTreeSet<String> = pending.keys().cloned().collect();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    while !pending.is_empty() {
        let ready: Vec<String> = pending
            .values()
            .filter(|t| {
                t.references
                    .iter()
                    .all(|r| done.contains(r) || !names.contains(r))
            })
            .map(|t| t.name.clone())
            .collect();
        if ready.is_empty() {
            return Err(Error::bad("The store's foreign keys form a cycle"));
        }
        for name in ready {
            if let Some(table) = pending.remove(&name) {
                done.insert(name);
                out.push(table);
            }
        }
    }
    Ok(out)
}

/// The tables of the connection's schema, parents first, each with the
/// columns an archive carries and their types (`bigint`, `text`, `bytea`…).
/// A table and its archived columns with their types.
pub type TableColumns = (String, Vec<(String, String)>);

pub fn table_order(db: &Connection) -> Result<Vec<TableColumns>> {
    let mut out = Vec::new();
    for table in tables(db)? {
        let mut statement = db.prepare_cached(
            "SELECT a.attname, format_type(a.atttypid, NULL) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace
             WHERE n.nspname=current_schema() AND c.relname=? AND a.attnum>0 AND NOT a.attisdropped AND a.attgenerated='' ORDER BY a.attnum",
        )?;
        let columns = statement.query_map([&table.name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        out.push((
            table.name.clone(),
            columns.collect::<std::result::Result<Vec<_>, _>>()?,
        ));
    }
    Ok(out)
}

/// Copy every row of `from`'s schema into the empty schema of `to`, which
/// gets the same tables (without foreign keys) first. Runs in one snapshot
/// of `from`.
pub fn snapshot_into(from: &mut Connection, to: &Scratch) -> Result<()> {
    // The scratch schema gets this program's tables, foreign keys included,
    // so that what is copied in parents-first order is written out in that
    // order too and loads back the same way.
    crate::store::migrate_pool(&to.database)?;
    let target = to.database.schema.clone();
    // Tables this program does not create (an edition's, which an
    // extension installed in another program created) are copied too, as
    // they are, without foreign keys.
    {
        let source: String = from.query_row("SELECT current_schema()", [], |r| r.get(0))?;
        let theirs: BTreeSet<String> = tables(from)?.into_iter().map(|t| t.name).collect();
        let ours: BTreeSet<String> = tables(&to.connect()?)?
            .into_iter()
            .map(|t| t.name)
            .collect();
        let db = to.connect()?;
        for name in theirs.difference(&ours) {
            if !plain_name(name) {
                continue;
            }
            db.execute_batch(&format!(
                "CREATE TABLE {} (LIKE {}.{} INCLUDING ALL)",
                qualified(&db, name),
                quote(&source),
                quote(name)
            ))?;
        }
    }
    // One snapshot of every table, without the write lock: writers go on.
    let tx = from.snapshot_transaction()?;
    // Rows are copied as they are: the projection-group triggers, which
    // mark a group stale on any other write, stay out of it.
    tx.execute_batch("SET LOCAL graphicle.projecting = 'on'")?;
    let list = tables(&tx)?;
    let names: Vec<String> = list
        .iter()
        .map(|t| format!("{}.{}", quote(&target), quote(&t.name)))
        .collect();
    if !names.is_empty() {
        tx.execute_batch(&format!("TRUNCATE {} CASCADE", names.join(",")))?;
    }
    for table in &list {
        let columns: Vec<String> = table.columns.iter().map(|c| quote(c)).collect();
        tx.execute_batch(&format!(
            "INSERT INTO {}.{} ({cols}) OVERRIDING SYSTEM VALUE SELECT {cols} FROM {}",
            quote(&target),
            quote(&table.name),
            qualified(&tx, &table.name),
            cols = columns.join(",")
        ))?;
    }
    tx.commit()?;
    reset_identities(&to.connect()?)?;
    Ok(())
}

/// Make `to` (a store at this program's schema, such as one
/// [`crate::store::Store::open`] just created) an exact copy of `from` as it
/// is now: one snapshot of every table of `from`, ids included. What `to`
/// held is replaced.
pub fn copy_store(from: &crate::store::Store, to: &crate::store::Store) -> Result<()> {
    let target = to.schema().to_string();
    let mut db = from.connect()?;
    let tx = db.snapshot_transaction()?;
    // Rows are copied as they are: the projection-group triggers, which
    // mark a group stale on any other write, stay out of it.
    tx.execute_batch("SET LOCAL graphicle.projecting = 'on'")?;
    let list = tables(&tx)?;
    let names: Vec<String> = list
        .iter()
        .map(|t| format!("{}.{}", quote(&target), quote(&t.name)))
        .collect();
    if !names.is_empty() {
        tx.execute_batch(&format!("TRUNCATE {} CASCADE", names.join(",")))?;
    }
    for table in &list {
        let columns: Vec<String> = table.columns.iter().map(|c| quote(c)).collect();
        tx.execute_batch(&format!(
            "INSERT INTO {}.{} ({cols}) OVERRIDING SYSTEM VALUE SELECT {cols} FROM {}",
            quote(&target),
            quote(&table.name),
            qualified(&tx, &table.name),
            cols = columns.join(",")
        ))?;
    }
    tx.commit()?;
    reset_identities(&to.connect()?)?;
    Ok(())
}

/// A writer that frames what it is given into chunks and hashes every byte
/// it writes to the file.
struct Framed<'a> {
    out: &'a mut (dyn Write + Send),
    hasher: &'a mut Sha256,
    buffer: Vec<u8>,
    rows: u64,
}

impl Framed<'_> {
    fn emit(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.hasher.update(bytes);
        self.out.write_all(bytes)
    }
    fn flush_chunk(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let buffer = std::mem::take(&mut self.buffer);
        self.rows += buffer.iter().filter(|b| **b == b'\n').count() as u64;
        self.emit(format!("{}\n", buffer.len()).as_bytes())?;
        self.emit(&buffer)?;
        self.buffer = buffer;
        self.buffer.clear();
        Ok(())
    }
}

impl Write for Framed<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= CHUNK {
            self.flush_chunk()?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_chunk()
    }
}

/// Write every table of `db`'s schema to a new archive at `path` (created,
/// never replaced, mode 0600). Returns the table and row counts.
pub fn write(db: &Connection, path: &Path) -> Result<Value> {
    write_tables(db, path, None)
}

/// [`write`] of the tables `only` names, when it names them.
pub fn write_tables(
    db: &Connection,
    path: &Path,
    only: Option<&BTreeSet<String>>,
) -> Result<Value> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    let outcome = write_to(db, file, only);
    if outcome.is_err() {
        let _ = std::fs::remove_file(path);
    }
    outcome
}

/// [`write`] over a file that replaces `path` once it is complete (the
/// rewrite of a backup after offboarding).
pub fn replace(db: &Connection, path: &Path, only: Option<&BTreeSet<String>>) -> Result<Value> {
    let directory = path
        .parent()
        .ok_or_else(|| Error::bad("Backup path has no folder"))?;
    let temporary = directory.join(format!(".{}.partial", uuid::Uuid::new_v4().simple()));
    let outcome = write_tables(db, &temporary, only);
    match outcome {
        Ok(value) => {
            std::fs::rename(&temporary, path)?;
            Ok(value)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn write_to(
    db: &Connection,
    file: std::fs::File,
    only: Option<&BTreeSet<String>>,
) -> Result<Value> {
    let mut out = std::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let emit = |out: &mut std::io::BufWriter<std::fs::File>,
                hasher: &mut Sha256,
                bytes: &[u8]|
     -> Result<()> {
        hasher.update(bytes);
        out.write_all(bytes)?;
        Ok(())
    };
    emit(&mut out, &mut hasher, MAGIC.as_bytes())?;
    let header =
        json!({"format":FORMAT,"version":env!("CARGO_PKG_VERSION"),"created_at":crate::now()});
    emit(&mut out, &mut hasher, format!("{header}\n").as_bytes())?;
    let list: Vec<Table> = tables(db)?
        .into_iter()
        .filter(|t| only.is_none_or(|only| only.contains(&t.name)))
        .collect();
    let mut total_rows = 0u64;
    for table in &list {
        let types = column_types(db, &table.name)?;
        let line = json!({"table":table.name,"columns":table.columns,"types":types});
        emit(&mut out, &mut hasher, format!("{line}\n").as_bytes())?;
        let columns: Vec<String> = table.columns.iter().map(|c| quote(c)).collect();
        let mut framed = Framed {
            out: &mut out,
            hasher: &mut hasher,
            buffer: Vec::with_capacity(CHUNK),
            rows: 0,
        };
        db.copy_out(
            &format!(
                "COPY (SELECT {} FROM {} ORDER BY 1) TO STDOUT",
                columns.join(","),
                qualified(db, &table.name)
            ),
            &mut framed,
        )?;
        framed.flush_chunk()?;
        total_rows += framed.rows;
        emit(&mut out, &mut hasher, b"0\n")?;
    }
    let digest = format!("{:x}", hasher.finalize());
    let trailer = json!({"end":true,"tables":list.len(),"rows":total_rows,"sha256":digest});
    out.write_all(format!("{trailer}\n").as_bytes())?;
    out.flush()?;
    out.get_ref().sync_all()?;
    Ok(json!({"format":FORMAT,"tables":list.len(),"rows":total_rows,"sha256":digest}))
}

/// One table of an archive as it is read.
pub struct Section {
    pub table: String,
    pub columns: Vec<String>,
    /// The columns' types (`bigint`, `text`…), in the same order.
    pub types: Vec<String>,
}

/// The types of `table`'s archived columns, in order.
fn column_types(db: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = db.prepare_cached(
        "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=current_schema() AND c.relname=? AND a.attnum>0 AND NOT a.attisdropped AND a.attgenerated='' ORDER BY a.attnum",
    )?;
    let rows = statement.query_map([table], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The column types an archive may give a table this program does not
/// create: those of this program's own tables (`bigint`, `text`, `double
/// precision`, `bytea`) and other plain built-in ones. A type is the
/// column's whole definition in `CREATE TABLE`, so anything else, such as a
/// `CHECK`, `DEFAULT` or `GENERATED` clause, which would run functions the
/// archive chose while it loads, is refused.
const ALLOWED_TYPES: &[&str] = &[
    "bigint",
    "integer",
    "smallint",
    "int",
    "int2",
    "int4",
    "int8",
    "double precision",
    "real",
    "float4",
    "float8",
    "numeric",
    "boolean",
    "bool",
    "text",
    "character varying",
    "varchar",
    "bytea",
    "json",
    "jsonb",
    "uuid",
    "date",
    "timestamp",
    "timestamp without time zone",
    "timestamp with time zone",
    "timestamptz",
];

/// The type an archive gives a column, as it is written into `CREATE
/// TABLE`: one of [`ALLOWED_TYPES`], in any case and with any spacing,
/// optionally an array (`[]`); `None` for anything else.
fn allowed_type(name: &str) -> Option<String> {
    let normal = name
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let (base, array) = match normal.strip_suffix("[]") {
        Some(base) => (base.trim_end(), true),
        None => (normal.as_str(), false),
    };
    if !ALLOWED_TYPES.contains(&base) {
        return None;
    }
    Some(if array {
        format!("{base}[]")
    } else {
        base.to_string()
    })
}

/// Whether one table of an archive is one this program may load: plain
/// table and column names, never PostgreSQL's own, and an allowed type for
/// every column whose type it gives.
fn plain_section(section: &Section) -> bool {
    plain_name(&section.table)
        && section.columns.iter().all(|c| plain_name(c))
        && (section.types.is_empty() || section.types.len() == section.columns.len())
        && section.types.iter().all(|t| allowed_type(t).is_some())
}

/// Check the whole archive at `path` before anything is created from it:
/// its digest, and that every table has plain names and allowed column
/// types ([`ALLOWED_TYPES`]).
pub fn verify(path: &Path) -> Result<Value> {
    read(path, |section, _| {
        if !plain_section(section) {
            return Err(Error::bad(format!(
                "The backup's table {} has a name or column type this version of Graphicle does not load",
                section.table
            )));
        }
        Ok(())
    })
}

/// Read an archive front to back, handing each table's rows to `table` as
/// a reader of COPY text; verifies the trailer's digest at the end. With a
/// no-op `table`, this is the integrity check.
pub fn read(
    path: &Path,
    mut table: impl FnMut(&Section, &mut (dyn Read + Send)) -> Result<()>,
) -> Result<Value> {
    let file = std::fs::File::open(path)?;
    let mut input = BufReader::new(file);
    let mut hasher = Sha256::new();
    let invalid = || Error::bad("This is not a valid Graphicle store backup");
    let mut line = String::new();
    let read_line = |input: &mut BufReader<std::fs::File>,
                     hasher: &mut Sha256,
                     line: &mut String|
     -> Result<()> {
        if read_line_bounded(input, line)? == 0 {
            return Err(invalid());
        }
        hasher.update(line.as_bytes());
        Ok(())
    };
    read_line(&mut input, &mut hasher, &mut line)?;
    if line != MAGIC {
        return Err(invalid());
    }
    read_line(&mut input, &mut hasher, &mut line)?;
    let header: Value = serde_json::from_str(&line).map_err(|_| invalid())?;
    if header["format"] != FORMAT {
        return Err(invalid());
    }
    let mut tables = 0usize;
    let mut rows = 0u64;
    loop {
        // The trailer is not hashed: read it raw.
        if read_line_bounded(&mut input, &mut line)? == 0 {
            return Err(invalid());
        }
        let value: Value = serde_json::from_str(&line).map_err(|_| invalid())?;
        if value["end"] == true {
            let digest = format!("{:x}", hasher.finalize());
            let mut trailing = [0u8; 1];
            if value["sha256"] != digest.as_str()
                || value["tables"] != tables
                || value["rows"].as_u64() != Some(rows)
                || input.read(&mut trailing)? != 0
            {
                return Err(invalid());
            }
            return Ok(
                json!({"format":FORMAT,"tables":tables,"rows":value["rows"],"sha256":digest,"header":header}),
            );
        }
        hasher.update(line.as_bytes());
        let section = Section {
            table: value["table"].as_str().ok_or_else(invalid)?.to_string(),
            columns: value["columns"]
                .as_array()
                .ok_or_else(invalid)?
                .iter()
                .map(|c| c.as_str().map(str::to_string).ok_or_else(invalid))
                .collect::<Result<_>>()?,
            types: value["types"]
                .as_array()
                .map(|types| {
                    types
                        .iter()
                        .map(|t| t.as_str().unwrap_or("text").to_string())
                        .collect()
                })
                .unwrap_or_default(),
        };
        let mut chunks = Chunks {
            input: &mut input,
            hasher: &mut hasher,
            left: 0,
            done: false,
            rows: 0,
        };
        table(&section, &mut chunks)?;
        // Whatever the consumer left unread is read (and hashed) here.
        std::io::copy(&mut chunks, &mut std::io::sink())?;
        let table_rows = chunks.rows;
        drop(chunks);
        rows = rows.checked_add(table_rows).ok_or_else(invalid)?;
        tables += 1;
    }
}

/// The chunked rows of one table, as a reader.
struct Chunks<'a> {
    input: &'a mut BufReader<std::fs::File>,
    hasher: &'a mut Sha256,
    left: usize,
    done: bool,
    rows: u64,
}

impl Read for Chunks<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 || line.len() > 24 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated archive",
                ));
            }
            self.hasher.update(line.as_bytes());
            self.left = line
                .trim_end()
                .parse::<usize>()
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk"))?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let n = self.left.min(out.len());
        self.input.read_exact(&mut out[..n])?;
        self.hasher.update(&out[..n]);
        let row_breaks = out[..n].iter().filter(|byte| **byte == b'\n').count() as u64;
        self.rows = self
            .rows
            .checked_add(row_breaks)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "too many rows"))?;
        self.left -= n;
        Ok(n)
    }
}

/// Load an archive into `db`'s schema, whose tables already exist (a store
/// [`crate::store::Store::open`] just created, or a scratch schema of
/// [`open_scratch`]). Tables are emptied first; a table the
/// archive has and the schema lacks is refused.
pub fn load(db: &mut Connection, path: &Path) -> Result<Value> {
    // Check the whole file before changing anything.
    let checked = verify(path)?;
    let present: BTreeMap<String, Table> = tables(db)?
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    let tx = db.transaction()?;
    // Rows are copied as they are: the projection-group triggers, which
    // mark a group stale on any other write, stay out of it.
    tx.execute_batch("SET LOCAL graphicle.projecting = 'on'")?;
    let names: Vec<String> = present.keys().map(|n| qualified(&tx, n)).collect();
    if !names.is_empty() {
        tx.execute_batch(&format!("TRUNCATE {} CASCADE", names.join(",")))?;
    }
    let mut loaded = 0usize;
    read(path, |section, rows| {
        // Names come from the file: only plain identifiers, never
        // PostgreSQL's own, and always in this schema.
        if !plain_name(&section.table) || !section.columns.iter().all(|c| plain_name(c)) {
            return Err(Error::bad("This is not a valid Graphicle store backup"));
        }
        let columns: Vec<String> = match present.get(&section.table) {
            Some(table) => {
                let columns: Vec<String> = section
                    .columns
                    .iter()
                    .filter(|c| table.columns.contains(c))
                    .map(|c| quote(c))
                    .collect();
                if columns.len() != section.columns.len() {
                    return Err(Error::bad(format!(
                        "The backup's table {} has columns this store lacks",
                        section.table
                    )));
                }
                columns
            }
            // A table this program does not create (an edition's): kept,
            // as a store keeps every table, with the columns and types
            // the backup gives.
            None => {
                let types: Vec<String> = section
                    .types
                    .iter()
                    .filter_map(|t| allowed_type(t))
                    .collect();
                if section.types.len() != section.columns.len()
                    || types.len() != section.types.len()
                {
                    return Err(Error::bad(format!(
                        "The backup has table {} that this version of Graphicle does not know",
                        section.table
                    )));
                }
                // Only the allowed type's own name reaches the statement.
                let definition: Vec<String> = section
                    .columns
                    .iter()
                    .zip(&types)
                    .map(|(c, t)| format!("{} {t}", quote(c)))
                    .collect();
                tx.execute_batch(&format!(
                    "CREATE TABLE {} ({})",
                    qualified(&tx, &section.table),
                    definition.join(",")
                ))?;
                section.columns.iter().map(|c| quote(c)).collect()
            }
        };
        tx.copy_in(
            &format!(
                "COPY {} ({}) FROM STDIN",
                qualified(&tx, &section.table),
                columns.join(",")
            ),
            rows,
        )?;
        loaded += 1;
        Ok(())
    })?;
    reset_identities(&tx)?;
    tx.commit()?;
    Ok(json!({"tables":loaded,"rows":checked["rows"],"sha256":checked["sha256"]}))
}

/// Move every identity sequence of the schema past its column's largest
/// value, after rows were loaded with their ids.
pub fn reset_identities(db: &Connection) -> Result<()> {
    let identities: Vec<(String, String)> = {
        let mut statement = db.prepare(
            "SELECT table_name, column_name FROM information_schema.columns WHERE table_schema=current_schema() AND is_identity='YES'",
        )?;
        let rows =
            statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (table, column) in identities {
        db.query_row(
            &format!(
                "SELECT setval(pg_get_serial_sequence(?, ?), coalesce((SELECT max({c}) FROM {t}), 0) + 1, false)",
                c = quote(&column),
                t = qualified(db, &table)
            ),
            crate::params![qualified(db, &table), column],
            |_| Ok(()),
        )?;
    }
    Ok(())
}

/// The tables a backup holds: a SQLite backup's, or an archive's.
pub fn backup_tables(path: &Path) -> Result<BTreeSet<String>> {
    if crate::db::is_sqlite_file(path) {
        let db = Database::sqlite(path, false).connect()?;
        return Ok(crate::store::tables(&db)?.into_iter().collect());
    }
    let mut names = BTreeSet::new();
    read(path, |section, _| {
        names.insert(section.table.clone());
        Ok(())
    })?;
    Ok(names)
}

/// Load an archive into a new scratch schema with this program's tables:
/// the copy that offboarding reads and edits.
pub fn open_scratch(path: &Path) -> Result<Scratch> {
    if crate::db::is_sqlite_file(path) {
        // A SQLite backup: a private copy of the file.
        let folder = tempfile::tempdir()?;
        let copy = folder.path().join("backup.sqlite3");
        std::fs::copy(path, &copy)?;
        let database = Database::sqlite(&copy, false);
        let names: BTreeSet<String> = crate::store::tables(&database.connect()?)?
            .into_iter()
            .collect();
        return Ok(Scratch {
            database,
            tables: Some(names),
            copy: Some(folder),
        });
    }
    let mut scratch = Scratch::new()?;
    crate::store::migrate_pool(&scratch.database)?;
    load(&mut scratch.connect()?, path)?;
    let mut names = BTreeSet::new();
    read(path, |section, _| {
        names.insert(section.table.clone());
        Ok(())
    })?;
    scratch.tables = Some(names);
    Ok(scratch)
}

/// Rewrite the archive at `path` with only the tables `keep` accepts (an
/// archive of an older store, which had fewer tables, in tests).
pub fn retain_tables(path: &Path, keep: impl Fn(&str) -> bool) -> Result<()> {
    if crate::db::is_sqlite_file(path) {
        let db = Database::sqlite(path, false).connect()?;
        for table in crate::store::tables(&db)? {
            if !keep(&table) && crate::db::valid_identifier(&table) {
                db.execute_batch(&format!("DROP TABLE {table}"))?;
            }
        }
        db.execute_batch("VACUUM")?;
        return Ok(());
    }
    let scratch = open_scratch(path)?;
    let only: BTreeSet<String> = scratch
        .tables
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| keep(t))
        .collect();
    replace(&scratch.connect()?, path, Some(&only))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(rows: u64, payload: &[u8], trailing: &[u8]) -> tempfile::NamedTempFile {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC.as_bytes());
        bytes.extend_from_slice(
            format!(
                "{}\n",
                json!({"format":FORMAT,"version":"0.1.14","created_at":"2026-11-11T00:00:00Z"})
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(
            format!(
                "{}\n",
                json!({"table":"sample","columns":["value"],"types":["text"]})
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(format!("{}\n", payload.len()).as_bytes());
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(b"0\n");
        let digest = format!("{:x}", Sha256::digest(&bytes));
        bytes.extend_from_slice(
            format!(
                "{}\n",
                json!({"end":true,"tables":1,"rows":rows,"sha256":digest})
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(trailing);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        file
    }

    #[test]
    fn verify_rejects_bytes_after_the_trailer() {
        let file = archive(1, b"row\n", b"unexpected");
        assert!(verify(file.path()).is_err());
    }

    #[test]
    fn verify_checks_the_trailer_row_count() {
        let file = archive(2, b"row\n", b"");
        assert!(verify(file.path()).is_err());
    }

    #[test]
    fn verify_accepts_an_exact_archive() {
        let file = archive(1, b"row\n", b"");
        assert!(verify(file.path()).is_ok());
    }
}
