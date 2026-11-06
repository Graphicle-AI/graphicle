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

/// A schema made for one piece of work and dropped with it.
pub struct Scratch {
    pub database: Arc<Database>,
    /// The tables of the archive it was loaded from ([`open_scratch`]):
    /// written back, an archive keeps the tables it had.
    pub tables: Option<BTreeSet<String>>,
}

impl Scratch {
    /// A new, empty scratch schema.
    pub fn new() -> Result<Self> {
        let settings = Settings::from_environment()?;
        let schema = format!("g_{}", &uuid::Uuid::new_v4().simple().to_string()[..24]);
        crate::db::admin(&settings.url, &format!("CREATE SCHEMA \"{schema}\""))?;
        Ok(Self {
            database: Database::open(&settings, &schema)?,
            tables: None,
        })
    }
    pub fn connect(&self) -> Result<Connection> {
        Ok(self.database.connect()?)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.database.drop_schema();
    }
}

/// A table of a schema: its name, the columns an archive carries (all but
/// generated ones) and the tables its foreign keys reference.
#[derive(Debug)]
struct Table {
    name: String,
    columns: Vec<String>,
    references: BTreeSet<String>,
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
            db.execute_batch(&format!(
                "CREATE TABLE {} (LIKE {}.{} INCLUDING ALL)",
                quote(name),
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
            quote(&table.name),
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
            quote(&table.name),
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
                quote(&table.name)
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

/// A type name an archive may give a column: letters, digits, spaces,
/// brackets and parentheses only.
fn plain_type(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " _[](),".contains(c))
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
        line.clear();
        let n = input.read_line(line)?;
        if n == 0 || n > 1 << 20 {
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
    loop {
        // The trailer is not hashed: read it raw.
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Err(invalid());
        }
        let value: Value = serde_json::from_str(&line).map_err(|_| invalid())?;
        if value["end"] == true {
            let digest = format!("{:x}", hasher.finalize());
            if value["sha256"] != digest.as_str() || value["tables"] != tables {
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
        };
        table(&section, &mut chunks)?;
        // Whatever the consumer left unread is read (and hashed) here.
        std::io::copy(&mut chunks, &mut std::io::sink())?;
        tables += 1;
    }
}

/// The chunked rows of one table, as a reader.
struct Chunks<'a> {
    input: &'a mut BufReader<std::fs::File>,
    hasher: &'a mut Sha256,
    left: usize,
    done: bool,
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
    let checked = read(path, |_, _| Ok(()))?;
    let present: BTreeMap<String, Table> = tables(db)?
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    let tx = db.transaction()?;
    // Rows are copied as they are: the projection-group triggers, which
    // mark a group stale on any other write, stay out of it.
    tx.execute_batch("SET LOCAL graphicle.projecting = 'on'")?;
    let names: Vec<String> = present.keys().map(|n| quote(n)).collect();
    if !names.is_empty() {
        tx.execute_batch(&format!("TRUNCATE {} CASCADE", names.join(",")))?;
    }
    let mut loaded = 0usize;
    read(path, |section, rows| {
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
                if section.types.len() != section.columns.len()
                    || !section.types.iter().all(|t| plain_type(t))
                {
                    return Err(Error::bad(format!(
                        "The backup has table {} that this version of Graphicle does not know",
                        section.table
                    )));
                }
                let definition: Vec<String> = section
                    .columns
                    .iter()
                    .zip(&section.types)
                    .map(|(c, t)| format!("{} {t}", quote(c)))
                    .collect();
                tx.execute_batch(&format!(
                    "CREATE TABLE {} ({})",
                    quote(&section.table),
                    definition.join(",")
                ))?;
                section.columns.iter().map(|c| quote(c)).collect()
            }
        };
        tx.copy_in(
            &format!(
                "COPY {} ({}) FROM STDIN",
                quote(&section.table),
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
                t = quote(&table)
            ),
            crate::params![quote(&table), column],
            |_| Ok(()),
        )?;
    }
    Ok(())
}

/// Load an archive into a new scratch schema with this program's tables:
/// the copy that offboarding reads and edits.
pub fn open_scratch(path: &Path) -> Result<Scratch> {
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
