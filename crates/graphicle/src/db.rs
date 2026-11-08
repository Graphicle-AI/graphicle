//! The store's database: SQLite by default, or PostgreSQL when
//! `GRAPHICLE_DATABASE_URL` is set, through one small connection layer
//! shaped like the parts of `rusqlite` the store was written against, so
//! that the code reading and writing the store keeps its shape.
//!
//! - **Which.** Without `GRAPHICLE_DATABASE_URL` a data folder's store is
//!   the SQLite file `reviews.sqlite3` in it, as in every release before
//!   PostgreSQL support: no server, nothing to configure
//!   ([`Engine::Sqlite`]). With it, the store is in PostgreSQL
//!   ([`Engine::Postgres`]). A data folder that has `postgres.json` is
//!   always a PostgreSQL store, and one with `reviews.sqlite3` is copied
//!   into PostgreSQL once with `graphicle import-sqlite`.
//! - **Where (PostgreSQL).** `GRAPHICLE_DATABASE_URL` names the server (a
//!   libpq connection string or a `postgresql://` URL). Each data folder
//!   owns one schema of that database, named in the folder's
//!   `postgres.json` when the store is created
//!   ([`Database::for_directory`]); two data folders (two instances) never
//!   share tables, and moving a data folder keeps its schema. A server that
//!   is not on this machine is reached over TLS with its certificate and
//!   host name checked ([`Settings`]).
//! - **Transactions.** [`Connection::transaction`] serializes writers on the
//!   schema's advisory lock, as SQLite's single write lock did, so a
//!   read-then-write never races another writer; reads use a read-only
//!   `REPEATABLE READ` snapshot and never wait for writers
//!   ([`Connection::read_transaction`]). A writer that waits longer than the
//!   connection's busy timeout fails with "database is locked", as before.
//! - **Values.** Parameters and results carry SQLite's five storage classes
//!   ([`Value`]); a parameter is encoded as the type the server expects for
//!   it (an integer bound to a `text` column is sent as its decimal text, as
//!   SQLite's type affinity stored it), and every result type the schema uses
//!   decodes to one of the five.
//! - **SQL.** Statements are written to run on both. On PostgreSQL, `?`
//!   and `?NNN` placeholders are rewritten to `$1`, `$2`… outside quotes and
//!   comments ([`translate`]) and SQLite table definitions take
//!   PostgreSQL's types ([`sqlite_ddl`]); on SQLite, PostgreSQL casts
//!   (`?1::text`), `LIMIT ALL` and `chr()` are written SQLite's way
//!   ([`sqlite_sql`]). The few statements that differ more are chosen by
//!   [`Connection::engine`] where they are.

use postgres::types::{FromSql, IsNull, ToSql as PgToSql, Type, to_sql_checked};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::Duration,
};

/// The settings this module reads, with their defaults.
pub const URL_ENV: &str = "GRAPHICLE_DATABASE_URL";
pub const POOL_ENV: &str = "GRAPHICLE_DATABASE_POOL";
/// Set to `1` to allow a connection without TLS to a server that is not on
/// this machine (`sslmode=disable` with a remote host). Never needed for a
/// Unix socket or localhost.
pub const PLAINTEXT_ENV: &str = "GRAPHICLE_DATABASE_ALLOW_PLAINTEXT";
/// A PEM file of the certificate authorities to trust for the server's
/// certificate, in place of the system's (a private CA). Also read from
/// libpq's `PGSSLROOTCERT`.
pub const ROOT_CERT_ENV: &str = "GRAPHICLE_DATABASE_SSLROOTCERT";
/// The longest one statement may run, in seconds (`statement_timeout`);
/// 0 turns the limit off.
pub const STATEMENT_TIMEOUT_ENV: &str = "GRAPHICLE_DATABASE_STATEMENT_TIMEOUT";
pub const DEFAULT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(600);
/// A session left idle inside a transaction this long is ended by the
/// server, so a stuck client cannot hold back vacuum or hold locks.
pub const IDLE_IN_TRANSACTION: Duration = Duration::from_secs(600);
/// A SQLite store's file in its data folder.
pub const SQLITE_FILE: &str = "reviews.sqlite3";
/// A SQLite store's write-ahead log is checkpointed and truncated once it
/// passes this size ([`Connection::bound_wal`]).
pub const DEFAULT_WAL_LIMIT: u64 = 64 << 20;
/// Connections each process keeps to one store at most.
pub const DEFAULT_POOL: u32 = 32;
/// The file in a data folder that names its schema.
pub const SCHEMA_FILE: &str = "postgres.json";
/// How long a writer waits for the write lock by default (SQLite's busy
/// timeout in the code this replaces).
pub const DEFAULT_BUSY: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Errors

/// A database error. Its text is what [`crate::Error`] reports after
/// "Storage operation failed: ".
#[derive(Debug)]
pub enum Error {
    /// `query_row` found no row (`OptionalExtension::optional` makes it `None`).
    QueryReturnedNoRows,
    /// A column could not be read as the type asked for.
    InvalidColumnType(usize, String, &'static str),
    /// A column index or name that the statement does not have.
    InvalidColumn(String),
    /// A parameter that cannot be sent (a `u64` past `i64::MAX`).
    ToSqlConversionFailure(String),
    /// The server refused or failed the statement.
    Server { code: String, message: String },
    /// SQLite refused or failed the statement: its extended result code,
    /// and `rusqlite`'s text for the error.
    Sqlite { code: Option<i32>, message: String },
    /// No connection could be made or taken from the pool.
    Connect(String),
    /// Anything else, with its text.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::QueryReturnedNoRows => f.write_str("Query returned no rows"),
            Error::InvalidColumnType(index, name, wanted) => {
                write!(
                    f,
                    "Invalid column type at index {index} ({name}): expected {wanted}"
                )
            }
            Error::InvalidColumn(name) => write!(f, "Invalid column: {name}"),
            Error::ToSqlConversionFailure(message) => write!(f, "Invalid parameter: {message}"),
            // Lock waits keep SQLite's words, which callers and tests match
            // (`crate::jobs::busy`).
            Error::Server { code, message } if is_busy_code(code) => {
                write!(f, "database is locked ({message})")
            }
            // A write in a read-only snapshot keeps SQLite's words too.
            Error::Server { code, message } if code == "25006" => {
                write!(f, "attempt to write a readonly database ({message})")
            }
            Error::Server { message, .. } => f.write_str(message),
            Error::Sqlite { message, .. } => f.write_str(message),
            Error::Connect(message) => write!(f, "Could not reach the database: {message}"),
            Error::Other(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for Error {}

fn is_busy_code(code: &str) -> bool {
    // lock_not_available, deadlock_detected, serialization_failure
    matches!(code, "55P03" | "40P01" | "40001")
}

impl Error {
    /// The server's SQLSTATE, when the server refused the statement.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Server { code, .. } => Some(code),
            _ => None,
        }
    }
    /// Whether a unique or primary-key constraint refused the write.
    pub fn is_unique_violation(&self) -> bool {
        // SQLITE_CONSTRAINT_PRIMARYKEY and SQLITE_CONSTRAINT_UNIQUE.
        self.code() == Some("23505")
            || matches!(
                self,
                Error::Sqlite {
                    code: Some(1555 | 2067),
                    ..
                }
            )
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::QueryReturnedNoRows => Error::QueryReturnedNoRows,
            rusqlite::Error::InvalidColumnType(index, name, _) => {
                Error::InvalidColumnType(index, name, "the column's type")
            }
            rusqlite::Error::InvalidColumnName(name) => Error::InvalidColumn(name),
            rusqlite::Error::InvalidColumnIndex(index) => Error::InvalidColumn(index.to_string()),
            other => Error::Sqlite {
                code: other.sqlite_error().map(|e| e.extended_code),
                message: other.to_string(),
            },
        }
    }
}

impl From<postgres::Error> for Error {
    fn from(error: postgres::Error) -> Self {
        if let Some(db) = error.as_db_error() {
            let mut message = db.message().to_string();
            if let Some(detail) = db.detail() {
                message.push_str(": ");
                message.push_str(detail);
            }
            return Error::Server {
                code: db.code().code().to_string(),
                message,
            };
        }
        if error.is_closed() {
            return Error::Connect("the connection was closed".into());
        }
        Error::Other(error.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `rusqlite`'s `OptionalExtension`: no row is `None`, not an error.
pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}
impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Values

/// One value, in SQLite's five storage classes.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// A borrowed [`Value`], as rows hand out (`Row::get_ref`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueRef<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'a [u8]),
    Blob(&'a [u8]),
}

impl Value {
    fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Value::Null => ValueRef::Null,
            Value::Integer(v) => ValueRef::Integer(*v),
            Value::Real(v) => ValueRef::Real(*v),
            Value::Text(v) => ValueRef::Text(v.as_bytes()),
            Value::Blob(v) => ValueRef::Blob(v),
        }
    }
}

/// Why a value could not be read as the type asked for.
#[derive(Debug)]
pub enum FromSqlError {
    InvalidType,
    OutOfRange(i64),
    Other(String),
}
impl fmt::Display for FromSqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FromSqlError::InvalidType => f.write_str("Invalid type"),
            FromSqlError::OutOfRange(v) => write!(f, "Value {v} out of range"),
            FromSqlError::Other(m) => f.write_str(m),
        }
    }
}
impl std::error::Error for FromSqlError {}
impl From<FromSqlError> for Error {
    fn from(error: FromSqlError) -> Self {
        Error::Other(error.to_string())
    }
}
pub type FromSqlResult<T> = std::result::Result<T, FromSqlError>;

impl<'a> ValueRef<'a> {
    pub fn as_str(&self) -> FromSqlResult<&'a str> {
        match self {
            ValueRef::Text(t) => {
                std::str::from_utf8(t).map_err(|e| FromSqlError::Other(e.to_string()))
            }
            _ => Err(FromSqlError::InvalidType),
        }
    }
    pub fn as_str_or_null(&self) -> FromSqlResult<Option<&'a str>> {
        match self {
            ValueRef::Null => Ok(None),
            _ => self.as_str().map(Some),
        }
    }
    pub fn as_i64(&self) -> FromSqlResult<i64> {
        match self {
            ValueRef::Integer(v) => Ok(*v),
            _ => Err(FromSqlError::InvalidType),
        }
    }
    pub fn as_f64(&self) -> FromSqlResult<f64> {
        match self {
            ValueRef::Real(v) => Ok(*v),
            ValueRef::Integer(v) => Ok(*v as f64),
            // A number that a JSON function returned as text.
            ValueRef::Text(t) => std::str::from_utf8(t)
                .ok()
                .and_then(|t| t.parse::<f64>().ok())
                .ok_or(FromSqlError::InvalidType),
            _ => Err(FromSqlError::InvalidType),
        }
    }
    pub fn as_blob(&self) -> FromSqlResult<&'a [u8]> {
        match self {
            ValueRef::Blob(b) => Ok(b),
            _ => Err(FromSqlError::InvalidType),
        }
    }
    pub fn as_bytes(&self) -> FromSqlResult<&'a [u8]> {
        match self {
            ValueRef::Text(b) | ValueRef::Blob(b) => Ok(b),
            _ => Err(FromSqlError::InvalidType),
        }
    }
    pub fn data_type(&self) -> &'static str {
        match self {
            ValueRef::Null => "null",
            ValueRef::Integer(_) => "integer",
            ValueRef::Real(_) => "real",
            ValueRef::Text(_) => "text",
            ValueRef::Blob(_) => "blob",
        }
    }
}

/// A value that can be sent as a parameter.
pub trait ToSql {
    fn to_sql(&self) -> Result<Value>;
}

macro_rules! to_sql_integer {
    ($($t:ty),*) => {$(
        impl ToSql for $t {
            fn to_sql(&self) -> Result<Value> {
                i64::try_from(*self)
                    .map(Value::Integer)
                    .map_err(|_| Error::ToSqlConversionFailure(format!("{} does not fit a 64-bit integer", self)))
            }
        }
    )*};
}
to_sql_integer!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl ToSql for bool {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Integer(i64::from(*self)))
    }
}
impl ToSql for f64 {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Real(*self))
    }
}
impl ToSql for f32 {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Real(f64::from(*self)))
    }
}
impl ToSql for str {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Text(self.to_string()))
    }
}
impl ToSql for String {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Text(self.clone()))
    }
}
impl ToSql for Cow<'_, str> {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Text(self.to_string()))
    }
}
impl ToSql for [u8] {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Blob(self.to_vec()))
    }
}
impl ToSql for Vec<u8> {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Blob(self.clone()))
    }
}
impl<const N: usize> ToSql for [u8; N] {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Blob(self.to_vec()))
    }
}
impl ToSql for Value {
    fn to_sql(&self) -> Result<Value> {
        Ok(self.clone())
    }
}
impl ToSql for ValueRef<'_> {
    fn to_sql(&self) -> Result<Value> {
        Ok(match self {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(v) => Value::Integer(*v),
            ValueRef::Real(v) => Value::Real(*v),
            ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
        })
    }
}
/// SQL `NULL` (`rusqlite::types::Null`).
#[derive(Clone, Copy, Debug)]
pub struct Null;
impl ToSql for Null {
    fn to_sql(&self) -> Result<Value> {
        Ok(Value::Null)
    }
}
impl<T: ToSql> ToSql for Option<T> {
    fn to_sql(&self) -> Result<Value> {
        match self {
            Some(value) => value.to_sql(),
            None => Ok(Value::Null),
        }
    }
}
impl<T: ToSql + ?Sized> ToSql for &T {
    fn to_sql(&self) -> Result<Value> {
        (**self).to_sql()
    }
}
impl<T: ToSql + ?Sized> ToSql for Box<T> {
    fn to_sql(&self) -> Result<Value> {
        (**self).to_sql()
    }
}
impl<T: ToSql + ?Sized> ToSql for Arc<T> {
    fn to_sql(&self) -> Result<Value> {
        (**self).to_sql()
    }
}

/// A value that can be read from a column.
pub trait FromSqlValue: Sized {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self>;
}

macro_rules! from_sql_integer {
    ($($t:ty),*) => {$(
        impl FromSqlValue for $t {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let v = match value {
                    ValueRef::Integer(v) => v,
                    ValueRef::Real(v) if v.fract() == 0.0 && v.abs() < 9.2e18 => v as i64,
                    // A number that a JSON function returned as text.
                    ValueRef::Text(t) => match std::str::from_utf8(t).ok().and_then(|t| t.parse::<i64>().ok()) {
                        Some(v) => v,
                        None => return Err(FromSqlError::InvalidType),
                    },
                    _ => return Err(FromSqlError::InvalidType),
                };
                <$t>::try_from(v).map_err(|_| FromSqlError::OutOfRange(v))
            }
        }
    )*};
}
from_sql_integer!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl FromSqlValue for bool {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Integer(v) => Ok(v != 0),
            ValueRef::Text(b"1") => Ok(true),
            ValueRef::Text(b"0") => Ok(false),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}
impl FromSqlValue for f64 {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_f64()
    }
}
impl FromSqlValue for f32 {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_f64().map(|v| v as f32)
    }
}
impl FromSqlValue for String {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Text(t) => {
                String::from_utf8(t.to_vec()).map_err(|e| FromSqlError::Other(e.to_string()))
            }
            _ => Err(FromSqlError::InvalidType),
        }
    }
}
impl FromSqlValue for Vec<u8> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_bytes().map(<[u8]>::to_vec)
    }
}
impl FromSqlValue for Value {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(match value {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(v) => Value::Integer(v),
            ValueRef::Real(v) => Value::Real(v),
            ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
        })
    }
}
impl<T: FromSqlValue> FromSqlValue for Option<T> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value {
            ValueRef::Null => Ok(None),
            other => T::column_result(other).map(Some),
        }
    }
}

// The wire encoding of a parameter, by the type the server expects.
impl PgToSql for Value {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut bytes::BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        use bytes::BufMut;
        if matches!(self, Value::Null) {
            return Ok(IsNull::Yes);
        }
        let integer = || -> std::result::Result<i64, Box<dyn std::error::Error + Sync + Send>> {
            match self {
                Value::Integer(v) => Ok(*v),
                Value::Real(v) if v.fract() == 0.0 => Ok(*v as i64),
                Value::Text(t) => Ok(t.trim().parse::<i64>()?),
                _ => Err(format!(
                    "cannot send a {} parameter as {ty}",
                    self.as_ref().data_type()
                )
                .into()),
            }
        };
        let real = || -> std::result::Result<f64, Box<dyn std::error::Error + Sync + Send>> {
            match self {
                Value::Integer(v) => Ok(*v as f64),
                Value::Real(v) => Ok(*v),
                Value::Text(t) => Ok(t.trim().parse::<f64>()?),
                _ => Err(format!(
                    "cannot send a {} parameter as {ty}",
                    self.as_ref().data_type()
                )
                .into()),
            }
        };
        let text = |out: &mut bytes::BytesMut| -> std::result::Result<(), Box<dyn std::error::Error + Sync + Send>> {
            match self {
                Value::Integer(v) => out.put_slice(v.to_string().as_bytes()),
                Value::Real(v) => out.put_slice(real_text(*v).as_bytes()),
                Value::Text(t) if needs_escape(t) => out.put_slice(escape_nul(t).as_bytes()),
                Value::Text(t) => out.put_slice(t.as_bytes()),
                Value::Blob(b) => match std::str::from_utf8(b) {
                    Ok(t) => out.put_slice(t.as_bytes()),
                    Err(_) => return Err(format!("cannot send binary data as {ty}").into()),
                },
                Value::Null => {}
            }
            Ok(())
        };
        match *ty {
            Type::INT8 => out.put_i64(integer()?),
            Type::INT4 => out.put_i32(i32::try_from(integer()?)?),
            Type::INT2 => out.put_i16(i16::try_from(integer()?)?),
            Type::OID => out.put_u32(u32::try_from(integer()?)?),
            Type::FLOAT8 => out.put_f64(real()?),
            Type::FLOAT4 => out.put_f32(real()? as f32),
            Type::BOOL => out.put_u8(u8::from(match self {
                Value::Text(t) => matches!(t.as_str(), "1" | "t" | "true"),
                _ => real()? != 0.0,
            })),
            Type::BYTEA => match self {
                Value::Blob(b) => out.put_slice(b),
                Value::Text(t) => out.put_slice(t.as_bytes()),
                _ => {
                    return Err(format!(
                        "cannot send a {} parameter as bytea",
                        self.as_ref().data_type()
                    )
                    .into());
                }
            },
            Type::JSONB => {
                out.put_u8(1);
                text(out)?;
            }
            Type::NUMERIC => {
                // Sent as text would need the text format; numbers the store
                // writes are integers or doubles, never numeric columns.
                return Err("numeric parameters are not supported; cast the parameter".into());
            }
            _ => text(out)?,
        }
        Ok(IsNull::No)
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    to_sql_checked!();
}

/// PostgreSQL text holds no NUL character, which SQLite text could: a NUL
/// is stored as U+10FFFF and `0`, and U+10FFFF itself (a noncharacter) as
/// two of it, so that text with either reads back as it was written and
/// equal texts stay equal.
const NUL_ESCAPE: char = '\u{10FFFF}';

fn needs_escape(text: &str) -> bool {
    text.contains('\0') || text.contains(NUL_ESCAPE)
}

fn escape_nul(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\0' => {
                out.push(NUL_ESCAPE);
                out.push('0');
            }
            NUL_ESCAPE => {
                out.push(NUL_ESCAPE);
                out.push(NUL_ESCAPE);
            }
            c => out.push(c),
        }
    }
    out
}

fn unescape_nul(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == NUL_ESCAPE {
            match chars.next() {
                Some('0') => out.push('\0'),
                Some(NUL_ESCAPE) => out.push(NUL_ESCAPE),
                Some(other) => {
                    out.push(NUL_ESCAPE);
                    out.push(other);
                }
                None => out.push(NUL_ESCAPE),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// SQLite's text for a REAL (`%!.15g`): `1.0`, `0.5`, `1e+20`.
fn real_text(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// A result column, decoded into a [`Value`] whatever its type.
struct Raw(Value);
impl<'a> FromSql<'a> for Raw {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let be =
            |n: usize| -> std::result::Result<&[u8], Box<dyn std::error::Error + Sync + Send>> {
                raw.get(..n).ok_or_else(|| "short value".into())
            };
        Ok(Raw(match *ty {
            Type::INT8 => Value::Integer(i64::from_be_bytes(be(8)?.try_into()?)),
            Type::INT4 => Value::Integer(i64::from(i32::from_be_bytes(be(4)?.try_into()?))),
            Type::INT2 => Value::Integer(i64::from(i16::from_be_bytes(be(2)?.try_into()?))),
            Type::OID => Value::Integer(i64::from(u32::from_be_bytes(be(4)?.try_into()?))),
            Type::BOOL => Value::Integer(i64::from(be(1)?[0] != 0)),
            Type::FLOAT8 => Value::Real(f64::from_be_bytes(be(8)?.try_into()?)),
            Type::FLOAT4 => Value::Real(f64::from(f32::from_be_bytes(be(4)?.try_into()?))),
            Type::NUMERIC => numeric(raw)?,
            Type::BYTEA => Value::Blob(raw.to_vec()),
            Type::JSONB => Value::Text(String::from_utf8(
                raw.get(1..).unwrap_or_default().to_vec(),
            )?),
            Type::VOID => Value::Null,
            _ => {
                let text = String::from_utf8(raw.to_vec())?;
                if text.contains(NUL_ESCAPE) {
                    Value::Text(unescape_nul(&text))
                } else {
                    Value::Text(text)
                }
            }
        }))
    }
    fn from_sql_null(
        _: &Type,
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(Value::Null))
    }
    fn accepts(ty: &Type) -> bool {
        // Text-like types are read as text (their binary form is their UTF-8
        // text); the others are listed in `from_sql`.
        !matches!(
            *ty,
            Type::TIMESTAMP
                | Type::TIMESTAMPTZ
                | Type::DATE
                | Type::TIME
                | Type::INTERVAL
                | Type::TS_VECTOR
                | Type::TSQUERY
                | Type::UUID
        )
    }
}

/// PostgreSQL's binary `numeric`: an integer when it has no fraction and
/// fits, else a double (as SQLite's arithmetic gave).
fn numeric(raw: &[u8]) -> std::result::Result<Value, Box<dyn std::error::Error + Sync + Send>> {
    let word = |i: usize| -> std::result::Result<u16, Box<dyn std::error::Error + Sync + Send>> {
        Ok(u16::from_be_bytes(
            raw.get(i..i + 2).ok_or("short numeric")?.try_into()?,
        ))
    };
    let digits = word(0)? as usize;
    let weight = word(2)? as i16 as i32;
    let sign = word(4)?;
    let scale = word(6)?;
    if sign == 0xC000 {
        return Ok(Value::Real(f64::NAN));
    }
    let mut integer: Option<i128> = Some(0);
    let mut real = 0f64;
    for i in 0..digits {
        let d = word(8 + 2 * i)? as i128;
        let power = weight - i as i32;
        real += d as f64 * 10000f64.powi(power);
        integer = match integer {
            Some(acc) if power >= 0 => (0..power)
                .try_fold(d, |v, _| v.checked_mul(10000))
                .and_then(|v| acc.checked_add(v)),
            Some(acc) if d == 0 => Some(acc),
            _ => None,
        };
    }
    let negative = sign == 0x4000;
    if scale == 0
        && let Some(v) = integer
        && let Ok(v) = i64::try_from(if negative { -v } else { v })
    {
        return Ok(Value::Integer(v));
    }
    Ok(Value::Real(if negative { -real } else { real }))
}

// ---------------------------------------------------------------------------
// Parameters

/// A statement's parameters (`rusqlite::Params`).
pub trait Params {
    fn values(self) -> Result<Vec<Value>>;
}
impl Params for () {
    fn values(self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
}
impl Params for &[&dyn ToSql] {
    fn values(self) -> Result<Vec<Value>> {
        self.iter().map(|v| v.to_sql()).collect()
    }
}
impl Params for &[&(dyn ToSql + Send + Sync)] {
    fn values(self) -> Result<Vec<Value>> {
        self.iter().map(|v| v.to_sql()).collect()
    }
}
impl Params for [&(dyn ToSql + Send + Sync); 0] {
    fn values(self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
}
impl Params for &Vec<Value> {
    fn values(self) -> Result<Vec<Value>> {
        Ok(self.clone())
    }
}
impl Params for Vec<Value> {
    fn values(self) -> Result<Vec<Value>> {
        Ok(self)
    }
}
macro_rules! params_array {
    ($($n:literal)*) => {$(
        impl<T: ToSql> Params for [T; $n] {
            fn values(self) -> Result<Vec<Value>> {
                self.iter().map(|v| v.to_sql()).collect()
            }
        }
        impl<T: ToSql> Params for &[T; $n] {
            fn values(self) -> Result<Vec<Value>> {
                self.iter().map(|v| v.to_sql()).collect()
            }
        }
    )*};
}
params_array!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
macro_rules! params_tuple {
    ($(($($name:ident $index:tt),+))*) => {$(
        impl<$($name: ToSql),+> Params for ($($name,)+) {
            fn values(self) -> Result<Vec<Value>> {
                Ok(vec![$(self.$index.to_sql()?),+])
            }
        }
    )*};
}
params_tuple!(
    (A 0)
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
    (A 0, B 1, C 2, D 3, E 4)
    (A 0, B 1, C 2, D 3, E 4, F 5)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7)
);

/// Parameters from an iterator (`rusqlite::params_from_iter`).
pub struct ParamsFromIter<I>(I);
pub fn params_from_iter<I>(iter: I) -> ParamsFromIter<I>
where
    I: IntoIterator,
    I::Item: ToSql,
{
    ParamsFromIter(iter)
}
impl<I> Params for ParamsFromIter<I>
where
    I: IntoIterator,
    I::Item: ToSql,
{
    fn values(self) -> Result<Vec<Value>> {
        self.0.into_iter().map(|v| v.to_sql()).collect()
    }
}

/// `rusqlite::params!`: a slice of parameters of mixed types.
#[macro_export]
macro_rules! params {
    () => {
        &[] as &[&dyn $crate::db::ToSql]
    };
    ($($param:expr),+ $(,)?) => {
        &[$(&$param as &dyn $crate::db::ToSql),+] as &[&dyn $crate::db::ToSql]
    };
}
pub use crate::params;

// ---------------------------------------------------------------------------
// SQL

/// [`translate`], with placeholders renumbered when the statement leaves
/// parameters out (`?2` alone): PostgreSQL cannot type a parameter no
/// placeholder names. Returns, for each `$n`, the given parameter it takes
/// (0-based), or nothing when they are in order.
pub fn translate_ordered(sql: &str) -> (String, Vec<usize>) {
    let translated = translate(sql).into_owned();
    // The `$n` the translation wrote, outside quotes (they are the only `$`
    // followed by digits that translate emits; dollar quotes are never
    // followed by a digit).
    let mut used = std::collections::BTreeSet::new();
    let bytes = translated.as_bytes();
    let mut i = 0;
    let mut in_quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match in_quote {
            Some(q) if b == q => in_quote = None,
            Some(_) => {}
            None if b == b'\'' || b == b'"' => in_quote = Some(b),
            None if b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if let Ok(n) = translated[i + 1..j].parse::<usize>() {
                    used.insert(n);
                }
                i = j;
                continue;
            }
            None => {}
        }
        i += 1;
    }
    let max = used.iter().next_back().copied().unwrap_or(0);
    if used.len() == max {
        return (translated, Vec::new());
    }
    let renumber: HashMap<usize, usize> =
        used.iter().enumerate().map(|(k, &n)| (n, k + 1)).collect();
    let order: Vec<usize> = used.iter().map(|&n| n - 1).collect();
    let mut out = String::with_capacity(translated.len());
    let mut i = 0;
    let mut in_quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match in_quote {
            Some(q) if b == q => in_quote = None,
            Some(_) => {}
            None if b == b'\'' || b == b'"' => in_quote = Some(b),
            None if b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let n = translated[i + 1..j].parse::<usize>().unwrap_or(0);
                out.push('$');
                out.push_str(&renumber.get(&n).copied().unwrap_or(n).to_string());
                i = j;
                continue;
            }
            None => {}
        }
        out.push(b as char);
        i += 1;
    }
    // Pushing bytes as chars is right only for ASCII; rebuild from slices
    // when the statement has other characters.
    if !translated.is_ascii() {
        let mut out = String::with_capacity(translated.len());
        let mut last = 0;
        let mut i = 0;
        let mut in_quote: Option<u8> = None;
        while i < bytes.len() {
            let b = bytes[i];
            match in_quote {
                Some(q) if b == q => in_quote = None,
                Some(_) => {}
                None if b == b'\'' || b == b'"' => in_quote = Some(b),
                None if b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                    let mut j = i + 1;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    let n = translated[i + 1..j].parse::<usize>().unwrap_or(0);
                    out.push_str(&translated[last..i]);
                    out.push('$');
                    out.push_str(&renumber.get(&n).copied().unwrap_or(n).to_string());
                    last = j;
                    i = j;
                    continue;
                }
                None => {}
            }
            i += 1;
        }
        out.push_str(&translated[last..]);
        return (out, order);
    }
    (out, order)
}

/// `sql` with `?` and `?NNN` placeholders numbered for PostgreSQL (`$1`…),
/// leaving quoted text, quoted identifiers, dollar-quoted bodies and comments
/// as they are.
pub fn translate(sql: &str) -> Cow<'_, str> {
    if !sql.contains('?') {
        return Cow::Borrowed(sql);
    }
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let (mut i, mut next) = (0usize, 1usize);
    let mut start = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        if bytes.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'$' => {
                // A dollar quote: $$…$$ or $tag$…$tag$.
                let tag_end = sql[i + 1..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .map(|n| i + 1 + n);
                match tag_end {
                    Some(end) if bytes[end] == b'$' && !bytes[i + 1].is_ascii_digit() => {
                        let tag = &sql[i..=end];
                        match sql[end + 1..].find(tag) {
                            Some(close) => i = end + 1 + close + tag.len(),
                            None => i = bytes.len(),
                        }
                    }
                    _ => i += 1,
                }
            }
            b'?' => {
                out.push_str(&sql[start..i]);
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 {
                    out.push('$');
                    out.push_str(&sql[i + 1..j]);
                    if let Ok(n) = sql[i + 1..j].parse::<usize>() {
                        next = next.max(n + 1);
                    }
                } else {
                    out.push('$');
                    out.push_str(&next.to_string());
                    next += 1;
                }
                i = j;
                start = j;
            }
            _ => i += 1,
        }
    }
    out.push_str(&sql[start.min(sql.len())..]);
    Cow::Owned(out)
}

/// Whether `sql` changes tables (prepared statements made before it may
/// return other columns).
fn changes_schema(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    [
        "ALTER TABLE",
        "DROP TABLE",
        "CREATE TABLE",
        "ADD COLUMN",
        "GRAPHICLE_ADD_ROWIDS",
        "DROP SCHEMA",
    ]
    .iter()
    .any(|k| upper.contains(k))
}

/// A table or column name Graphicle writes into SQL itself: letters,
/// digits and underscores, not starting with a digit.
pub fn valid_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `sql`, written for both engines, as SQLite takes it: PostgreSQL casts
/// (`?1::text`, `x::bigint[]`) left out, `LIMIT ALL` as `LIMIT -1` and
/// `chr(n)` as `char(n)`, outside quoted text and comments. Anything else
/// is left as it is.
pub fn sqlite_sql(sql: &str) -> Cow<'_, str> {
    let lower = sql.to_ascii_lowercase();
    if !(sql.contains("::") || lower.contains("limit all") || lower.contains("chr(")) {
        return Cow::Borrowed(sql);
    }
    static PATTERNS: OnceLock<[(regex::Regex, &'static str); 3]> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let re = |p: &str| {
            regex::Regex::new(p).unwrap_or_else(|_| regex::Regex::new("$^").expect("regex"))
        };
        [
            (
                re(r"(?i)::\s*(double\s+precision|[a-z_][a-z0-9_]*)(\s*\[\])?"),
                "",
            ),
            (re(r"(?i)\bLIMIT\s+ALL\b"), "LIMIT -1"),
            (re(r"(?i)\bchr\("), "char("),
        ]
    });
    map_plain(sql, |part| {
        let mut part = part.to_string();
        for (pattern, with) in patterns {
            part = pattern.replace_all(&part, *with).into_owned();
        }
        part
    })
}

/// `sql` with `f` applied to the text outside quoted strings, quoted
/// identifiers and `--` comments.
fn map_plain(sql: &str, f: impl Fn(&str) -> String) -> Cow<'_, str> {
    let mut out = String::with_capacity(sql.len() + 16);
    let mut plain = String::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                out.push_str(&f(&std::mem::take(&mut plain)));
                out.push(c);
                while let Some(q) = chars.next() {
                    out.push(q);
                    if q == c {
                        if chars.peek() == Some(&c) {
                            out.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                out.push_str(&f(&std::mem::take(&mut plain)));
                out.push(c);
                for q in chars.by_ref() {
                    out.push(q);
                    if q == '\n' {
                        break;
                    }
                }
            }
            c => plain.push(c),
        }
    }
    out.push_str(&f(&plain));
    Cow::Owned(out)
}

/// A [`Value`] as a SQLite parameter.
struct Lite<'a>(&'a Value);
impl rusqlite::ToSql for Lite<'_> {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::ValueRef as R;
        Ok(rusqlite::types::ToSqlOutput::Borrowed(match self.0 {
            Value::Null => R::Null,
            Value::Integer(v) => R::Integer(*v),
            Value::Real(v) => R::Real(*v),
            Value::Text(v) => R::Text(v.as_bytes()),
            Value::Blob(v) => R::Blob(v),
        }))
    }
}

/// A SQLite result value as a [`Value`]. Text that is not UTF-8 (SQLite
/// stores what it is given) is kept as a blob rather than altered.
fn from_lite(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef as R;
    match value {
        R::Null => Value::Null,
        R::Integer(v) => Value::Integer(v),
        R::Real(v) => Value::Real(v),
        R::Text(t) => match std::str::from_utf8(t) {
            Ok(text) => Value::Text(text.to_string()),
            Err(_) => Value::Blob(t.to_vec()),
        },
        R::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// How long a SQLite write-ahead-log checkpoint waits for readers.
pub const CHECKPOINT_WAIT: Duration = Duration::from_secs(2);
/// After a checkpoint that found readers still on an older snapshot, how
/// long this process leaves the log alone. A checkpoint that cuts the log
/// back holds the write lock while it waits for readers, so with readers
/// always in flight back to back attempts after every commit kept the lock
/// almost all the time and starved other writers (an indexing job failed
/// with "database is locked"). With this pause a process holds the lock
/// for checkpoints at most [`CHECKPOINT_WAIT`] in every
/// `CHECKPOINT_WAIT + CHECKPOINT_PAUSE`.
pub const CHECKPOINT_PAUSE: Duration = Duration::from_secs(8);

/// One checkpoint at a time per store in this process, and a pause after
/// one that found readers ([`CHECKPOINT_PAUSE`]).
#[derive(Default)]
struct CheckpointGate {
    running: std::sync::atomic::AtomicBool,
    /// Milliseconds since the Unix epoch before which no checkpoint starts.
    not_before_ms: std::sync::atomic::AtomicU64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
static WAL_CHECKPOINTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WAL_CHECKPOINTS_BUSY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// This process's SQLite write-ahead-log checkpoints
/// ([`Connection::bound_wal`]): how many ran, and how many found readers
/// still on an older snapshot after [`CHECKPOINT_WAIT`].
pub fn wal_report() -> serde_json::Value {
    serde_json::json!({
        "wal_checkpoints": WAL_CHECKPOINTS.load(std::sync::atomic::Ordering::Relaxed),
        "wal_checkpoints_busy": WAL_CHECKPOINTS_BUSY.load(std::sync::atomic::Ordering::Relaxed),
        "wal_limit_bytes": DEFAULT_WAL_LIMIT,
    })
}

/// Table definitions written for SQLite (an extension's tables, an older
/// test's) in PostgreSQL's types: `INTEGER` as `BIGINT` (SQLite integers
/// are 64-bit), `REAL` as `DOUBLE PRECISION`, `BLOB` as `BYTEA`,
/// `INTEGER PRIMARY KEY [AUTOINCREMENT]` as an identity, and no
/// `WITHOUT ROWID`. Statements without `CREATE TABLE` or `ADD COLUMN` are
/// left as they are.
pub fn sqlite_ddl(sql: &str) -> Cow<'_, str> {
    let upper = sql.to_ascii_uppercase();
    // Function bodies (dollar-quoted) are PostgreSQL's own.
    if !(upper.contains("CREATE TABLE") || upper.contains("ADD COLUMN")) || sql.contains("$$") {
        return Cow::Borrowed(sql);
    }
    static PATTERNS: OnceLock<[(regex::Regex, &'static str); 5]> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let re = |p: &str| {
            regex::Regex::new(p).unwrap_or_else(|_| regex::Regex::new("$^").expect("regex"))
        };
        [
            (
                re(r"(?i)\bINTEGER\s+PRIMARY\s+KEY(\s+AUTOINCREMENT)?\b"),
                "BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY",
            ),
            (re(r"(?i)\bINTEGER\b"), "BIGINT"),
            (re(r"(?i)\bREAL\b"), "DOUBLE PRECISION"),
            (re(r"(?i)\bBLOB\b"), "BYTEA"),
            (re(r"(?i)\)\s*WITHOUT\s+ROWID"), ")"),
        ]
    });
    // Only outside quoted text and comments: a default of 'INTEGER' stays as
    // it is, and a comment's apostrophe opens no quote.
    let mut out = String::with_capacity(sql.len() + 16);
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut String| {
        let mut part = std::mem::take(plain);
        for (pattern, with) in patterns {
            part = pattern.replace_all(&part, *with).into_owned();
        }
        out.push_str(&part);
    };
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                flush(&mut plain, &mut out);
                out.push(c);
                while let Some(q) = chars.next() {
                    out.push(q);
                    if q == '\'' {
                        if chars.peek() == Some(&'\'') {
                            out.push('\'');
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                flush(&mut plain, &mut out);
                out.push(c);
                for q in chars.by_ref() {
                    out.push(q);
                    if q == '\n' {
                        break;
                    }
                }
            }
            c => plain.push(c),
        }
    }
    flush(&mut plain, &mut out);
    Cow::Owned(out)
}

// ---------------------------------------------------------------------------
// The database of a data folder, and its pool

/// Which database a store is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// A SQLite file (`reviews.sqlite3`): the default.
    Sqlite,
    /// A schema of a PostgreSQL database (`GRAPHICLE_DATABASE_URL`).
    Postgres,
}

/// Whether `GRAPHICLE_DATABASE_URL` is set: new stores go to PostgreSQL.
pub fn postgres_configured() -> bool {
    std::env::var_os(URL_ENV).is_some_and(|v| !v.to_string_lossy().trim().is_empty())
}

/// The server address, pool size and connection policy, from the
/// environment. Its `Debug` never shows the address, which may hold a
/// password.
#[derive(Clone)]
pub struct Settings {
    pub url: String,
    pub pool: u32,
    /// `GRAPHICLE_DATABASE_ALLOW_PLAINTEXT=1`: a remote server may be
    /// reached without TLS when the address asks for `sslmode=disable`.
    pub allow_plaintext: bool,
    /// Certificate authorities to trust instead of the system's.
    pub root_cert: Option<PathBuf>,
    /// `statement_timeout` of every session; zero for none.
    pub statement_timeout: Duration,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("url", &"(not shown)")
            .field("pool", &self.pool)
            .field("allow_plaintext", &self.allow_plaintext)
            .field("root_cert", &self.root_cert)
            .field("statement_timeout", &self.statement_timeout)
            .finish()
    }
}

impl Settings {
    pub fn from_environment() -> Result<Self> {
        let url = std::env::var(URL_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                Error::Other(format!(
                    "This store is in PostgreSQL; set {URL_ENV} to its server (see docs/configuration.md)"
                ))
            })?;
        let pool = match std::env::var(POOL_ENV) {
            Ok(v) if !v.trim().is_empty() => v
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|n| (2..=1000).contains(n))
                .ok_or_else(|| {
                    Error::Other(format!(
                        "{POOL_ENV} takes a number of connections from 2 to 1000"
                    ))
                })?,
            _ => DEFAULT_POOL,
        };
        let statement_timeout = match std::env::var(STATEMENT_TIMEOUT_ENV) {
            Ok(v) if !v.trim().is_empty() => v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|n| *n <= 86_400)
                .map(Duration::from_secs)
                .ok_or_else(|| {
                    Error::Other(format!(
                        "{STATEMENT_TIMEOUT_ENV} takes a number of seconds from 0 to 86400"
                    ))
                })?,
            _ => DEFAULT_STATEMENT_TIMEOUT,
        };
        let root_cert = [ROOT_CERT_ENV, "PGSSLROOTCERT"]
            .iter()
            .find_map(|name| std::env::var_os(name).filter(|v| !v.is_empty()))
            .map(PathBuf::from);
        Ok(Self {
            url,
            pool,
            allow_plaintext: std::env::var(PLAINTEXT_ENV).is_ok_and(|v| v.trim() == "1"),
            root_cert,
            statement_timeout,
        })
    }

    /// The parsed address, with the TLS this policy requires of it.
    /// libpq's `sslmode=verify-full` and `verify-ca` are accepted and read
    /// as `require`: a certificate is always checked against its host name.
    fn config(&self) -> Result<(postgres::Config, bool)> {
        let url = self
            .url
            .replace("sslmode=verify-full", "sslmode=require")
            .replace("sslmode=verify-ca", "sslmode=require");
        let mut config: postgres::Config = url
            .parse()
            .map_err(|e: postgres::Error| Error::Connect(redact(&e.to_string(), &self.url)))?;
        let tls = tls_policy(&mut config, self.allow_plaintext)?;
        if config.get_password().is_none()
            && let Some(password) = pgpass_password(&config)
        {
            config.password(password);
        }
        Ok((config, tls))
    }
}

/// The password for `config` from the libpq password file (`PGPASSFILE`,
/// or `~/.pgpass`), read as libpq reads it: `host:port:database:user:password`
/// lines, `*` matching anything, `\:` and `\\` escaped, `localhost` for a
/// Unix socket, and the file ignored unless only its owner can read it.
fn pgpass_password(config: &postgres::Config) -> Option<String> {
    let path = std::env::var_os("PGPASSFILE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pgpass")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return None;
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let host = match config.get_hosts().first() {
        Some(postgres::config::Host::Tcp(name)) => name.clone(),
        _ => "localhost".to_string(),
    };
    let port = config
        .get_ports()
        .first()
        .copied()
        .unwrap_or(5432)
        .to_string();
    let user = config.get_user()?.to_string();
    let database = config.get_dbname().unwrap_or(&user).to_string();
    pgpass_lookup(&text, &[host, port, database, user])
}

/// The password a password file's text gives for `wanted` (host, port,
/// database, user).
fn pgpass_lookup(text: &str, wanted: &[String; 4]) -> Option<String> {
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        field.push(next);
                    }
                }
                ':' if fields.len() < 4 => fields.push(std::mem::take(&mut field)),
                c => field.push(c),
            }
        }
        fields.push(field);
        if fields.len() == 5
            && fields[..4]
                .iter()
                .zip(wanted)
                .all(|(have, want)| have == "*" || have == want)
        {
            return fields.pop();
        }
    }
    None
}

/// Whether every host `config` names is this machine: a Unix socket, or
/// `localhost` and the loopback addresses.
fn local_only(config: &postgres::Config) -> bool {
    let loopback = |name: &str| {
        name.eq_ignore_ascii_case("localhost")
            || name
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    };
    let hosts_local = config.get_hosts().iter().all(|host| match host {
        postgres::config::Host::Tcp(name) => loopback(name),
        #[cfg(unix)]
        postgres::config::Host::Unix(_) => true,
    });
    let addresses_local = config.get_hostaddrs().iter().all(|ip| ip.is_loopback());
    hosts_local && addresses_local
}

/// Decide whether the connection uses TLS: required (with the certificate
/// and host name checked) for any host that is not this machine, unless the
/// address says `sslmode=disable` and plaintext was explicitly allowed;
/// optional on a Unix socket or localhost, where `sslmode=require` still
/// asks for it.
fn tls_policy(config: &mut postgres::Config, allow_plaintext: bool) -> Result<bool> {
    use postgres::config::SslMode;
    let mode = config.get_ssl_mode();
    if local_only(config) {
        return Ok(matches!(mode, SslMode::Require));
    }
    if matches!(mode, SslMode::Disable) {
        if allow_plaintext {
            return Ok(false);
        }
        return Err(Error::Connect(format!(
            "the database server is not on this machine and the address asks for no TLS (sslmode=disable); remove it, or set {PLAINTEXT_ENV}=1 to send the store unencrypted"
        )));
    }
    config.ssl_mode(SslMode::Require);
    Ok(true)
}

/// `message` with the password of `url`, if it has one, replaced: a
/// connection error never carries the secret it was given.
pub fn redact(message: &str, url: &str) -> String {
    let mut out = message.to_string();
    let parsed: Option<postgres::Config> = url.parse().ok();
    let mut secrets: Vec<String> = Vec::new();
    if let Some(password) = parsed.as_ref().and_then(|c| c.get_password()) {
        secrets.push(String::from_utf8_lossy(password).into_owned());
    }
    // A URL that does not parse may still hold `:password@`.
    if let Some(rest) = url.split("://").nth(1)
        && let Some(userinfo) = rest.split('@').next().filter(|_| rest.contains('@'))
        && let Some((_, password)) = userinfo.split_once(':')
    {
        secrets.push(password.to_string());
    }
    for secret in secrets.into_iter().filter(|s| s.len() >= 2) {
        out = out.replace(&secret, "***");
    }
    if out.contains(url) && url.len() > 8 {
        out = out.replace(url, "(the database address)");
    }
    out
}

/// What `postgres.json` in a data folder holds.
#[derive(serde::Serialize, serde::Deserialize)]
struct SchemaFile {
    schema: String,
    /// A store that `import-sqlite` or `restore` is still copying into, or
    /// that one of them left when it was stopped: every opener refuses it
    /// ([`Database::create_unfinished`], [`Database::finish`]).
    #[serde(default, skip_serializing_if = "is_false")]
    incomplete: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// What the file that names a store holds, as read.
enum Named {
    /// No such file.
    Missing,
    /// An empty file: one that the process that created it is still
    /// writing, or that a process stopped while writing left.
    Empty,
    File(SchemaFile),
}

/// How long an opener waits for the process that created an empty
/// `postgres.json` to write it.
const SCHEMA_FILE_WAIT: Duration = Duration::from_secs(2);

/// The refusal of a store that an import or restore has not finished.
fn unfinished(path: &Path) -> Error {
    Error::Other(format!(
        "{} names a store that is not finished: an import-sqlite or restore is still copying into it, or was stopped before it finished. Run graphicle import-sqlite again to replace an unfinished import, or remove the data folder and restore again",
        path.display()
    ))
}

/// Where a store's rows are.
enum Backend {
    /// This process's connections to the PostgreSQL server.
    Postgres(r2d2::Pool<Manager>),
    /// A SQLite file; `store` when it is a data folder's store (foreign
    /// keys and `secure_delete` on, as every release set them), otherwise
    /// a cache file opened as it is, never through a symlink.
    Sqlite { path: PathBuf, store: bool },
}

/// One store: a schema of the configured database, or a SQLite file, and
/// this process's connections to it.
pub struct Database {
    /// The PostgreSQL schema (`main` for SQLite).
    pub schema: String,
    /// The file that names the schema (`postgres.json` of a data folder, or
    /// a cache file), or the SQLite file.
    pub file: Option<PathBuf>,
    /// A schema made for one connection ([`Connection::open_in_memory`]),
    /// dropped with it.
    temporary: bool,
    /// The schema's advisory-lock key, which write transactions take.
    pub lock_key: i64,
    /// Bumped by every schema change this process makes to the store
    /// (DDL), which outdates every session's prepared statements for it.
    generation: Arc<std::sync::atomic::AtomicU64>,
    /// A SQLite store's write-ahead log limit in bytes (0: none).
    wal_limit: std::sync::atomic::AtomicU64,
    /// Whether this process is checkpointing the log, and until when a
    /// checkpoint that found readers keeps others away ([`Connection::bound_wal`]).
    checkpoint: CheckpointGate,
    backend: Backend,
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("engine", &self.engine())
            .field("schema", &self.schema)
            .finish()
    }
}

/// A schema name Graphicle creates: `g_` and 24 lowercase hex digits.
pub fn valid_schema(name: &str) -> bool {
    name.len() == 26
        && name.starts_with("g_")
        && name[2..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// This process's stores, by server address and schema.
type Registry = Mutex<HashMap<(String, String), Arc<Database>>>;
static DATABASES: OnceLock<Registry> = OnceLock::new();

impl Database {
    /// Which database this store is in.
    pub fn engine(&self) -> Engine {
        match self.backend {
            Backend::Postgres(_) => Engine::Postgres,
            Backend::Sqlite { .. } => Engine::Sqlite,
        }
    }

    /// A SQLite file as a store (`store`) or a cache.
    pub fn sqlite(path: &Path, store: bool) -> Arc<Database> {
        Arc::new(Database {
            schema: "main".into(),
            file: Some(path.to_path_buf()),
            temporary: false,
            lock_key: 0,
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_limit: std::sync::atomic::AtomicU64::new(DEFAULT_WAL_LIMIT),
            checkpoint: CheckpointGate::default(),
            backend: Backend::Sqlite {
                path: path.to_path_buf(),
                store,
            },
        })
    }

    /// Set (or, with `None`, turn off) the size at which a SQLite store's
    /// write-ahead log is checkpointed and truncated. For tests and tools.
    pub fn set_wal_limit(&self, bytes: Option<u64>) {
        self.wal_limit
            .store(bytes.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    }

    /// The schema named in `directory`'s `postgres.json`, when the folder
    /// has a store.
    pub fn schema_of(directory: &Path) -> Result<Option<String>> {
        Self::schema_in(&directory.join(SCHEMA_FILE))
    }

    /// The schema the file at `path` names, when it exists. A store that an
    /// import or restore has not finished is refused, and so is an empty
    /// file that its creator has not written within [`SCHEMA_FILE_WAIT`]:
    /// only the process that created the file ever writes it.
    pub fn schema_in(path: &Path) -> Result<Option<String>> {
        match Self::named_in(path)? {
            Named::Missing => Ok(None),
            Named::File(file) if !file.incomplete => Ok(Some(file.schema)),
            Named::Empty | Named::File(_) => Err(unfinished(path)),
        }
    }

    /// The file at `path` as read; an empty one is read again until its
    /// creator has written it or [`SCHEMA_FILE_WAIT`] has passed.
    fn named_in(path: &Path) -> Result<Named> {
        let deadline = std::time::Instant::now() + SCHEMA_FILE_WAIT;
        loop {
            let named = Self::read_named(path)?;
            if !matches!(named, Named::Empty) || std::time::Instant::now() >= deadline {
                return Ok(named);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn read_named(path: &Path) -> Result<Named> {
        if path.is_symlink() {
            return Err(Error::Other(format!("{SCHEMA_FILE} cannot be a symlink")));
        }
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Named::Missing);
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "Could not read {}: {error}",
                    path.display()
                )));
            }
        };
        if bytes.is_empty() {
            return Ok(Named::Empty);
        }
        let file: SchemaFile = serde_json::from_slice(&bytes).map_err(|_| {
            Error::Other(format!("{} is not a Graphicle store file", path.display()))
        })?;
        if !valid_schema(&file.schema) {
            return Err(Error::Other(format!(
                "{} names an invalid schema",
                path.display()
            )));
        }
        Ok(Named::File(file))
    }

    /// A new store, named by a new file at `path` (which must not exist)
    /// and marked unfinished: every opener refuses it until [`Database::finish`].
    /// What `import-sqlite` and `restore` copy into, so that one stopped
    /// before its copy committed never leaves a store that opens empty.
    pub fn create_unfinished(path: &Path) -> Result<Arc<Database>> {
        let settings = Settings::from_environment()?;
        let schema = format!("g_{}", &uuid::Uuid::new_v4().simple().to_string()[..24]);
        admin(
            &settings,
            &format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""),
        )?;
        let file = serde_json::to_vec_pretty(&SchemaFile {
            schema: schema.clone(),
            incomplete: true,
        })
        .unwrap_or_default();
        if let Err(error) = write_private(path, &file) {
            let _ = admin(
                &settings,
                &format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"),
            );
            return Err(error);
        }
        Self::named_by(&settings, &schema, path)
    }

    /// Mark this store, made by [`Database::create_unfinished`], finished:
    /// its file is replaced, in one rename, by one without the mark.
    pub fn finish(&self) -> Result<()> {
        let Some(path) = &self.file else {
            return Err(Error::Other("Not a PostgreSQL store".into()));
        };
        match Self::read_named(path)? {
            Named::File(file) if file.schema == self.schema => {}
            _ => {
                return Err(Error::Other(format!(
                    "{} no longer names this store",
                    path.display()
                )));
            }
        }
        let bytes = serde_json::to_vec_pretty(&SchemaFile {
            schema: self.schema.clone(),
            incomplete: false,
        })
        .unwrap_or_default();
        let temporary =
            path.with_file_name(format!(".{SCHEMA_FILE}.{}", uuid::Uuid::new_v4().simple()));
        write_private(&temporary, &bytes)?;
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(Error::Other(format!(
                "Could not write {}: {error}",
                path.display()
            )));
        }
        #[cfg(unix)]
        if let Some(folder) = path.parent() {
            let _ = std::fs::File::open(folder).and_then(|folder| folder.sync_all());
        }
        Ok(())
    }

    /// Drop this store's schema, and remove its file when the file still
    /// names it (an import or restore that failed).
    pub fn discard(&self) -> Result<()> {
        if let Some(path) = &self.file
            && matches!(Self::read_named(path), Ok(Named::File(file)) if file.schema == self.schema)
        {
            let _ = std::fs::remove_file(path);
        }
        self.drop_schema()
    }

    /// Remove the store that the file at `path` names when an import or
    /// restore did not finish it (or an empty file its creator never
    /// wrote): its schema is dropped and the file removed. Returns whether
    /// there was such a store; a finished store is never touched.
    pub fn discard_unfinished(path: &Path) -> Result<bool> {
        match Self::named_in(path)? {
            Named::Missing => Ok(false),
            Named::File(file) if !file.incomplete => Ok(false),
            Named::Empty => {
                std::fs::remove_file(path).map_err(|e| {
                    Error::Other(format!("Could not remove {}: {e}", path.display()))
                })?;
                Ok(true)
            }
            Named::File(file) => {
                let settings = Settings::from_environment()?;
                admin(
                    &settings,
                    &format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", file.schema),
                )?;
                std::fs::remove_file(path).map_err(|e| {
                    Error::Other(format!("Could not remove {}: {e}", path.display()))
                })?;
                Ok(true)
            }
        }
    }

    /// Whether `directory` has a store: its `postgres.json`, or its SQLite
    /// file.
    pub fn exists(directory: &Path) -> bool {
        directory.join(SCHEMA_FILE).is_file() || directory.join(SQLITE_FILE).is_file()
    }

    /// Which database `directory`'s store is in, or would be created in:
    /// PostgreSQL when the folder has `postgres.json`, or has no SQLite
    /// store and `GRAPHICLE_DATABASE_URL` is set; SQLite otherwise.
    pub fn engine_of(directory: &Path) -> Engine {
        if directory.join(SCHEMA_FILE).is_file()
            || (postgres_configured() && !directory.join(SQLITE_FILE).is_file())
        {
            Engine::Postgres
        } else {
            Engine::Sqlite
        }
    }

    /// The store of `directory`, created (schema and `postgres.json`, or
    /// the SQLite file) when `create` and the folder has none yet.
    pub fn for_directory(directory: &Path, create: bool) -> Result<Arc<Database>> {
        match Self::engine_of(directory) {
            Engine::Postgres => Self::for_file(&directory.join(SCHEMA_FILE), create),
            Engine::Sqlite => {
                let path = directory.join(SQLITE_FILE);
                if path.is_symlink() {
                    return Err(Error::Other("Database cannot be a symlink".into()));
                }
                if !create && !path.is_file() {
                    return Err(Error::Other("This data folder has no store".into()));
                }
                Ok(Self::sqlite(&path, true))
            }
        }
    }

    /// The store that the file at `path` names (a data folder's
    /// `postgres.json`, or a cache file such as `GRAPHICLE_EMBEDDING_CACHE`),
    /// created with a new schema when `create` and the file does not exist.
    /// Without `GRAPHICLE_DATABASE_URL`, a cache file is a SQLite file.
    pub fn for_file(path: &Path, create: bool) -> Result<Arc<Database>> {
        let named = path
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new(SCHEMA_FILE));
        if !named && !postgres_configured() {
            if path.is_symlink() {
                return Err(Error::Other(format!(
                    "{} cannot be a symlink",
                    path.display()
                )));
            }
            if !create && !path.is_file() {
                return Err(Error::Other(format!("{} does not exist", path.display())));
            }
            return Ok(Self::sqlite(path, false));
        }
        let settings = Settings::from_environment()?;
        let schema = match Self::schema_in(path)? {
            Some(schema) => schema,
            None if create => {
                let schema = format!("g_{}", &uuid::Uuid::new_v4().simple().to_string()[..24]);
                admin(
                    &settings,
                    &format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""),
                )?;
                let file = serde_json::to_vec_pretty(&SchemaFile {
                    schema: schema.clone(),
                    incomplete: false,
                })
                .unwrap_or_default();
                if let Err(error) = write_private(path, &file) {
                    // Another process created it first: use its schema,
                    // once that process has written it.
                    let _ = admin(
                        &settings,
                        &format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"),
                    );
                    match Self::schema_in(path)? {
                        Some(theirs) => theirs,
                        None => return Err(error),
                    }
                } else {
                    schema
                }
            }
            None => return Err(Error::Other("This data folder has no store".into())),
        };
        Self::named_by(&settings, &schema, path)
    }

    /// The store in `schema`, named by the file at `path`.
    fn named_by(settings: &Settings, schema: &str, path: &Path) -> Result<Arc<Database>> {
        let database = Self::open(settings, schema)?;
        if database.file.as_deref() != Some(path) {
            // The same schema reached through another path (a moved data
            // folder): keep one pool, and remember the latest path.
            let mut all = DATABASES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Backend::Postgres(pool) = &database.backend else {
                return Err(Error::Other("Not a PostgreSQL store".into()));
            };
            let replaced = Arc::new(Database {
                schema: database.schema.clone(),
                file: Some(path.to_path_buf()),
                temporary: false,
                lock_key: database.lock_key,
                generation: database.generation.clone(),
                wal_limit: std::sync::atomic::AtomicU64::new(0),
                checkpoint: CheckpointGate::default(),
                backend: Backend::Postgres(pool.clone()),
            });
            all.insert((settings.url.clone(), schema.to_string()), replaced.clone());
            return Ok(replaced);
        }
        Ok(database)
    }

    /// The store in `schema`, with this process's pool for it.
    pub fn open(settings: &Settings, schema: &str) -> Result<Arc<Database>> {
        if !valid_schema(schema) {
            return Err(Error::Other("Invalid store schema".into()));
        }
        let key = (settings.url.clone(), schema.to_string());
        let mut all = DATABASES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(database) = all.get(&key) {
            return Ok(database.clone());
        }
        let pool = shared_pool(settings)?;
        let database = Arc::new(Database {
            schema: schema.to_string(),
            file: None,
            temporary: false,
            lock_key: lock_key(schema),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_limit: std::sync::atomic::AtomicU64::new(0),
            checkpoint: CheckpointGate::default(),
            backend: Backend::Postgres(pool),
        });
        all.insert(key, database.clone());
        Ok(database)
    }

    /// A connection, with the default busy timeout: from the pool for
    /// PostgreSQL, or a new one to the SQLite file.
    pub fn connect(self: &Arc<Self>) -> Result<Connection> {
        let pool = match &self.backend {
            Backend::Postgres(pool) => pool,
            Backend::Sqlite { path, store } => {
                let lite = if *store {
                    let lite = rusqlite::Connection::open(path)?;
                    lite.busy_timeout(DEFAULT_BUSY)?;
                    lite.pragma_update(None, "foreign_keys", true)?;
                    lite.pragma_update(None, "secure_delete", true)?;
                    // A write-ahead log that a checkpoint reset is cut back
                    // to this size rather than kept at its largest.
                    lite.pragma_update(None, "journal_size_limit", DEFAULT_WAL_LIMIT as i64)?;
                    lite
                } else {
                    rusqlite::Connection::open_with_flags(
                        path,
                        rusqlite::OpenFlags::default() | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
                    )?
                };
                return Ok(Connection::sqlite(self.clone(), lite));
            }
        };
        let session = blocking(|| pool.get()).map_err(|e| Error::Connect(e.to_string()))?;
        let connection = Connection {
            session: RefCell::new(Some(session)),
            lite: None,
            database: self.clone(),
            in_transaction: Cell::new(false),
            implicit: RefCell::new(None),
            savepoints: Cell::new(0),
            changes: Cell::new(0),
            busy: Cell::new(DEFAULT_BUSY),
            query_only: Cell::new(false),
            allocated: RefCell::new(HashMap::new()),
        };
        let schema = self.schema.clone();
        let generation = self.generation.load(std::sync::atomic::Ordering::Acquire);
        connection.with(move |session| {
            if session.schema != schema {
                session
                    .client
                    .batch_execute(&format!("SET search_path TO \"{schema}\""))?;
                // Statements were prepared against the other schema's tables.
                session.statements.clear();
                session.schema = schema;
            } else if session.generation != generation {
                // A schema change since: prepare again.
                session.statements.clear();
            }
            session.generation = generation;
            Ok(())
        })?;
        connection.apply_busy()?;
        Ok(connection)
    }

    /// Drop the schema and everything in it (`graphicle offboard` of a whole
    /// store, tests). PostgreSQL stores only.
    pub fn drop_schema(&self) -> Result<()> {
        if self.engine() != Engine::Postgres {
            return Err(Error::Other("Not a PostgreSQL store".into()));
        }
        let settings = Settings::from_environment()?;
        admin(
            &settings,
            &format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema),
        )
    }
}

/// Create the file at `path`, readable by this user only, with `bytes`.
/// A file that exists already, even an empty one, is never written: only
/// the process that created a file names a store in it.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| Error::Other(format!("Could not create {}: {e}", path.display())))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| Error::Other(format!("Could not write {}: {e}", path.display())))
}

/// The advisory-lock key of a schema: a stable 64-bit hash of its name.
fn lock_key(schema: &str) -> i64 {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("graphicle.write.{schema}").as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

/// Run `sql` on a connection of its own, outside any store's schema
/// (creating and dropping schemas).
pub fn admin(settings: &Settings, sql: &str) -> Result<()> {
    blocking(|| {
        let mut client = connect_raw(settings)?;
        client.batch_execute(sql)?;
        Ok(())
    })
}

/// The `application_name` of this process's connections: `graphicle` and
/// its command (`graphicle serve`, `graphicle sync`), so that
/// `pg_stat_activity` and the server's lock-wait log tell the processes
/// sharing a store apart.
fn application_name() -> String {
    // The first argument that is neither an option nor the value of a
    // global option (`--data-dir DIR`, `--config FILE`).
    let mut arguments = std::env::args().skip(1);
    let mut command = None;
    while let Some(argument) = arguments.next() {
        if argument == "--data-dir" || argument == "--config" {
            arguments.next();
        } else if !argument.starts_with('-') {
            command = Some(argument);
            break;
        }
    }
    let command = command
        .filter(|a| a.len() <= 32 && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    match command {
        Some(command) => format!("graphicle {command}"),
        None => "graphicle".to_string(),
    }
}

/// `values` as a PostgreSQL array literal (`{"a","b"}`), for a parameter
/// cast to `text[]`: `col = ANY(CAST(?::text AS text[]))` looks each value up
/// in an index on `col`.
pub fn text_array<S: AsRef<str>>(values: &[S]) -> String {
    let mut out = String::from("{");
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for c in value.as_ref().chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    }
    out.push('}');
    out
}

/// A connection outside any store's schema. Blocks: call it through
/// [`blocking`] from async code.
fn connect_raw(settings: &Settings) -> Result<postgres::Client> {
    Manager::new(settings)?.client()
}

impl Drop for Database {
    fn drop(&mut self) {
        if self.temporary
            && let Ok(settings) = Settings::from_environment()
        {
            let _ = admin(
                &settings,
                &format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema),
            );
        }
    }
}

/// The pool's connections: a client with its search path set to the store's
/// schema, and its prepared statements.
pub struct Session {
    client: postgres::Client,
    /// The store schema the session's search path names now, and the
    /// schema generation its prepared statements were made at.
    schema: String,
    generation: u64,
    statements: HashMap<String, (postgres::Statement, Arc<[usize]>)>,
    /// The session's `lock_timeout`, in milliseconds, as last set.
    lock_timeout_ms: u128,
    /// Whether foreign keys are off for the session
    /// (`pragma_update(None, "foreign_keys", false)`).
    replica: bool,
}

impl Session {
    /// The prepared statement for `sql`, prepared once per session.
    /// With it, the parameter each placeholder takes (empty: in order).
    fn statement(&mut self, sql: &str) -> Result<(postgres::Statement, Arc<[usize]>)> {
        if let Some(found) = self.statements.get(sql) {
            return Ok(found.clone());
        }
        let (translated, order) = translate_ordered(&sqlite_ddl(sql));
        let statement = match self.client.prepare(&translated) {
            Ok(statement) => statement,
            Err(error) => {
                let error = Error::from(error);
                if std::env::var_os("GRAPHICLE_SQL_DEBUG").is_some() {
                    eprintln!("graphicle: SQL refused ({error}): {translated}");
                }
                return Err(error);
            }
        };
        if self.statements.len() >= STATEMENT_CACHE {
            self.statements.clear();
        }
        let order: Arc<[usize]> = order.into();
        self.statements
            .insert(sql.to_string(), (statement.clone(), order.clone()));
        Ok((statement, order))
    }
}

/// The parameters in the order a statement's placeholders take them.
fn ordered<'a>(values: &'a [Value], order: &[usize]) -> Result<Vec<&'a (dyn PgToSql + Sync)>> {
    if order.is_empty() {
        return Ok(values.iter().map(|v| v as &(dyn PgToSql + Sync)).collect());
    }
    order
        .iter()
        .map(|&i| {
            values
                .get(i)
                .map(|v| v as &(dyn PgToSql + Sync))
                .ok_or_else(|| {
                    Error::Other(format!(
                        "The statement needs parameter {} that was not given",
                        i + 1
                    ))
                })
        })
        .collect()
}

/// Run blocking database work where the synchronous client may block: on
/// this thread outside an async runtime, through `block_in_place` on a
/// multi-thread runtime's worker, or on a scoped thread from a
/// current-thread runtime (the client drives its own runtime, which cannot
/// start inside another).
pub fn blocking<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    match tokio::runtime::Handle::try_current() {
        Err(_) => f(),
        Ok(handle) => match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
            _ => std::thread::scope(|scope| match scope.spawn(f).join() {
                Ok(value) => value,
                Err(panic) => std::panic::resume_unwind(panic),
            }),
        },
    }
}

struct Manager {
    config: postgres::Config,
    /// TLS for this server ([`tls_policy`]): the client configuration that
    /// checks its certificate and host name.
    tls: Option<crate::db_tls::MakeRustls>,
    statement_timeout: Duration,
    /// The address as given, to keep its password out of error text.
    url: String,
}

impl Manager {
    fn new(settings: &Settings) -> Result<Self> {
        let (mut config, tls) = settings.config()?;
        if config.get_application_name().is_none() {
            config.application_name(&application_name());
        }
        if config.get_user().is_none()
            && let Some(user) = std::env::var_os("USER").or_else(|| std::env::var_os("LOGNAME"))
        {
            config.user(&user.to_string_lossy());
        }
        if config.get_password().is_none()
            && let Some(password) = pgpass_password(&config)
        {
            config.password(password);
        }
        let tls = if tls {
            Some(crate::db_tls::MakeRustls::new(
                settings.root_cert.as_deref(),
            )?)
        } else {
            None
        };
        Ok(Self {
            config,
            tls,
            statement_timeout: settings.statement_timeout,
            url: settings.url.clone(),
        })
    }

    /// A new client, with this store's session limits.
    fn client(&self) -> Result<postgres::Client> {
        let connected = match &self.tls {
            Some(tls) => self.config.connect(tls.clone()),
            None => self.config.connect(postgres::NoTls),
        };
        let mut client =
            connected.map_err(|e| Error::Connect(redact(&e.to_string(), &self.url)))?;
        client.batch_execute(&format!(
            "SET lock_timeout = {}; SET statement_timeout = {}; SET idle_in_transaction_session_timeout = {}",
            DEFAULT_BUSY.as_millis(),
            self.statement_timeout.as_millis(),
            IDLE_IN_TRANSACTION.as_millis()
        ))?;
        Ok(client)
    }
}

static POOLS: OnceLock<Mutex<HashMap<String, r2d2::Pool<Manager>>>> = OnceLock::new();

/// This process's connections to the server at `settings.url`, shared by
/// every store (schema) the process opens: a connection is moved to a
/// store's schema when it is handed out ([`Database::connect`]), so the
/// number of connections is bounded by `GRAPHICLE_DATABASE_POOL` however
/// many stores a process uses.
fn shared_pool(settings: &Settings) -> Result<r2d2::Pool<Manager>> {
    let mut pools = POOLS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(pool) = pools.get(&settings.url) {
        return Ok(pool.clone());
    }
    let manager = Manager::new(settings)?;
    let pool = r2d2::Pool::builder()
        .max_size(settings.pool)
        .min_idle(Some(0))
        .test_on_check_out(false)
        .idle_timeout(Some(Duration::from_secs(60)))
        .connection_timeout(Duration::from_secs(30))
        .build_unchecked(manager);
    pools.insert(settings.url.clone(), pool.clone());
    Ok(pool)
}

impl r2d2::ManageConnection for Manager {
    type Connection = Session;
    type Error = Error;
    fn connect(&self) -> Result<Session> {
        let client = self.client()?;
        Ok(Session {
            client,
            schema: String::new(),
            generation: 0,
            statements: HashMap::new(),
            lock_timeout_ms: DEFAULT_BUSY.as_millis(),
            replica: false,
        })
    }
    fn is_valid(&self, session: &mut Session) -> Result<()> {
        session
            .client
            .simple_query("")
            .map(|_| ())
            .map_err(Error::from)
    }
    fn has_broken(&self, session: &mut Session) -> bool {
        session.client.is_closed()
    }
}

// ---------------------------------------------------------------------------
// Connections and transactions

/// How a transaction begins (`rusqlite::TransactionBehavior`). In this
/// store every write transaction takes the write lock when it begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionBehavior {
    Deferred,
    Immediate,
    Exclusive,
}

/// One connection to a store's schema, taken from its pool and returned to
/// it when dropped.
pub struct Connection {
    session: RefCell<Option<r2d2::PooledConnection<Manager>>>,
    /// The SQLite connection, for a SQLite store.
    lite: Option<rusqlite::Connection>,
    database: Arc<Database>,
    in_transaction: Cell<bool>,
    /// The savepoint that began the open transaction, when a `SAVEPOINT`
    /// outside one began it (as SQLite does).
    implicit: RefCell<Option<String>>,
    savepoints: Cell<u32>,
    changes: Cell<u64>,
    busy: Cell<Duration>,
    /// SQLite: `query_only` is on for the open read transaction.
    query_only: Cell<bool>,
    /// SQLite: the last id [`Connection::next_ids`] handed out, by table.
    allocated: RefCell<HashMap<String, i64>>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("schema", &self.database.schema)
            .finish()
    }
}

/// Cached statements kept per connection.
const STATEMENT_CACHE: usize = 512;

impl Connection {
    /// A connection to a new, empty schema of its own, dropped with the
    /// connection (`rusqlite`'s in-memory database, for tests and scratch
    /// work).
    pub fn open_in_memory() -> Result<Connection> {
        if !postgres_configured() {
            let database = Database::sqlite(Path::new(":memory:"), false);
            return Ok(Connection::sqlite(
                database,
                rusqlite::Connection::open_in_memory()?,
            ));
        }
        let settings = Settings::from_environment()?;
        let schema = format!("g_{}", &uuid::Uuid::new_v4().simple().to_string()[..24]);
        admin(&settings, &format!("CREATE SCHEMA \"{schema}\""))?;
        let pool = shared_pool(&settings)?;
        let database = Arc::new(Database {
            lock_key: lock_key(&schema),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            wal_limit: std::sync::atomic::AtomicU64::new(0),
            checkpoint: CheckpointGate::default(),
            schema,
            file: None,
            temporary: true,
            backend: Backend::Postgres(pool),
        });
        database.connect()
    }

    /// A connection over an open SQLite connection.
    fn sqlite(database: Arc<Database>, lite: rusqlite::Connection) -> Connection {
        Connection {
            session: RefCell::new(None),
            lite: Some(lite),
            database,
            in_transaction: Cell::new(false),
            implicit: RefCell::new(None),
            savepoints: Cell::new(0),
            changes: Cell::new(0),
            busy: Cell::new(DEFAULT_BUSY),
            query_only: Cell::new(false),
            allocated: RefCell::new(HashMap::new()),
        }
    }

    /// A connection over a SQLite connection the caller opened (a backup
    /// file opened read-only or immutable), for the store's code to read.
    pub fn from_sqlite(lite: rusqlite::Connection, path: &Path) -> Connection {
        Connection::sqlite(Database::sqlite(path, false), lite)
    }

    /// Which database this connection's store is in.
    pub fn engine(&self) -> Engine {
        if self.lite.is_some() {
            Engine::Sqlite
        } else {
            Engine::Postgres
        }
    }

    /// The SQLite connection under this one, for a SQLite store (the
    /// backup API, functions only SQLite has).
    pub fn sqlite_connection(&self) -> Option<&rusqlite::Connection> {
        self.lite.as_ref()
    }

    /// Run transaction-control or other parameterless SQL on either engine.
    fn control(&self, sql: &str) -> Result<()> {
        if let Some(lite) = &self.lite {
            return Ok(lite.execute_batch(sql)?);
        }
        self.with(|session| Ok(session.client.batch_execute(sql)?))
    }

    /// SQLite: run `sql` with `values`, mapping each row.
    fn lite_query(
        lite: &rusqlite::Connection,
        sql: &str,
        values: &[Value],
    ) -> Result<(Arc<Vec<String>>, Vec<Row>)> {
        let sql = sqlite_sql(sql);
        let mut statement = lite.prepare_cached(&sql)?;
        let columns: Arc<Vec<String>> = Arc::new(
            statement
                .column_names()
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        let width = columns.len();
        let mut rows = statement.query(rusqlite::params_from_iter(values.iter().map(Lite)))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut values = Vec::with_capacity(width);
            for i in 0..width {
                values.push(from_lite(row.get_ref(i)?));
            }
            out.push(Row {
                columns: columns.clone(),
                values,
            });
        }
        Ok((columns, out))
    }

    /// SQLite: when this store's write-ahead log has passed its limit,
    /// checkpoint it and cut it back (`wal_checkpoint(TRUNCATE)`), waiting at
    /// most [`CHECKPOINT_WAIT`] for readers to move to the latest snapshot.
    /// Run after a write transaction commits. With readers always in
    /// flight, SQLite's own automatic checkpoint (PASSIVE) can copy pages
    /// back but never reset the log, which then grows by everything
    /// written. One checkpoint runs at a time per store in this process,
    /// and none for [`CHECKPOINT_PAUSE`] after one that found readers.
    /// Returns whether a checkpoint ran and finished.
    pub fn bound_wal(&self) -> Result<Option<bool>> {
        let Some(lite) = &self.lite else {
            return Ok(None);
        };
        let limit = self
            .database
            .wal_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        let Some(file) = &self.database.file else {
            return Ok(None);
        };
        if limit == 0 || !self.is_autocommit() {
            return Ok(None);
        }
        let mut wal = file.clone().into_os_string();
        wal.push("-wal");
        let size = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        if size <= limit {
            return Ok(None);
        }
        let gate = &self.database.checkpoint;
        if now_ms()
            < gate
                .not_before_ms
                .load(std::sync::atomic::Ordering::Relaxed)
            || gate.running.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Ok(None);
        }
        let outcome = lite.busy_timeout(CHECKPOINT_WAIT).and_then(|()| {
            lite.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            })
        });
        let restored = lite.busy_timeout(self.busy.get());
        let found_readers = !matches!(outcome, Ok(0));
        if found_readers {
            gate.not_before_ms.store(
                now_ms() + CHECKPOINT_PAUSE.as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        gate.running
            .store(false, std::sync::atomic::Ordering::Release);
        restored?;
        let busy = outcome?;
        WAL_CHECKPOINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if busy != 0 {
            WAL_CHECKPOINTS_BUSY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Some(busy == 0))
    }

    /// The store this connection belongs to.
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    /// The file that names this connection's store (`rusqlite`'s
    /// `Connection::path`): a data folder's `postgres.json`, whose folder
    /// holds the store's other files.
    pub fn path(&self) -> Option<&str> {
        if let Some(lite) = &self.lite {
            return lite.path().filter(|p| !p.is_empty());
        }
        self.database.file.as_deref().and_then(Path::to_str)
    }

    /// Run `f` on this connection's session, off any async runtime's
    /// thread ([`blocking`]).
    fn with<T: Send>(&self, f: impl FnOnce(&mut Session) -> Result<T> + Send) -> Result<T> {
        let mut guard = self.session.try_borrow_mut().map_err(|_| {
            Error::Other("A statement is already running on this connection".into())
        })?;
        let session: &mut Session = guard
            .as_mut()
            .ok_or_else(|| Error::Other("Connection closed".into()))?;
        blocking(move || f(session))
    }

    /// Run `sql` once, with one retry outside a transaction when a schema
    /// change outdated the cached statement.
    fn run<T>(
        &self,
        sql: &str,
        values: &[Value],
        f: impl Fn(
            &mut postgres::Client,
            &postgres::Statement,
            &[&(dyn PgToSql + Sync)],
        ) -> std::result::Result<T, postgres::Error>
        + Send,
    ) -> Result<T>
    where
        T: Send,
    {
        let in_transaction = self.in_transaction.get();
        self.with(move |session| {
            let (statement, order) = session.statement(sql)?;
            let refs = ordered(values, &order)?;
            match f(&mut session.client, &statement, &refs) {
                Ok(value) => Ok(value),
                Err(error) => {
                    let error = Error::from(error);
                    if std::env::var_os("GRAPHICLE_SQL_DEBUG").is_some() {
                        eprintln!("graphicle: SQL failed ({error}): {}", translate(sql));
                    }
                    // A cached plan outdated by a schema change: prepare it
                    // again (only outside a transaction, which the error
                    // has not aborted).
                    if error.code() == Some("0A000") || error.code() == Some("26000") {
                        session.statements.remove(sql);
                        if !in_transaction {
                            let (statement, order) = session.statement(sql)?;
                            let refs = ordered(values, &order)?;
                            return f(&mut session.client, &statement, &refs).map_err(Error::from);
                        }
                    }
                    Err(error)
                }
            }
        })
    }

    pub fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize> {
        let values = params.values()?;
        if let Some(lite) = &self.lite {
            let sql = sqlite_sql(sql);
            let mut statement = lite.prepare_cached(&sql)?;
            let n = statement.execute(rusqlite::params_from_iter(values.iter().map(Lite)))?;
            self.changes.set(n as u64);
            return Ok(n);
        }
        let n = self.run(sql, &values, |client, statement, refs| {
            client.execute(statement, refs)
        })?;
        self.changes.set(n);
        Ok(usize::try_from(n).unwrap_or(usize::MAX))
    }

    /// Run statements with no parameters, separated by semicolons.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        if let Some(lite) = &self.lite {
            lite.execute_batch(&sqlite_sql(sql))?;
            self.in_transaction.set(!lite.is_autocommit());
            return Ok(());
        }
        let trimmed = sql.trim().trim_end_matches(';').trim();
        // SQLite's `BEGIN IMMEDIATE`: a transaction that holds the write lock.
        if trimmed.eq_ignore_ascii_case("BEGIN IMMEDIATE")
            || trimmed.eq_ignore_ascii_case("BEGIN EXCLUSIVE")
        {
            return self.begin(TransactionBehavior::Immediate, false);
        }
        if trimmed.eq_ignore_ascii_case("BEGIN") || trimmed.eq_ignore_ascii_case("BEGIN DEFERRED") {
            return self.begin(TransactionBehavior::Deferred, false);
        }
        // SQLite's SAVEPOINT outside a transaction begins one, which the
        // release of that savepoint commits (and a rollback to it, then its
        // release, ends with nothing written).
        let words: Vec<String> = trimmed
            .split_whitespace()
            .map(|w| w.trim_end_matches(';').to_ascii_lowercase())
            .collect();
        if !self.in_transaction.get() && words.len() == 2 && words[0] == "savepoint" {
            self.begin(TransactionBehavior::Deferred, false)?;
            *self.implicit.borrow_mut() = Some(words[1].clone());
            return self.with(|session| Ok(session.client.batch_execute(sql)?));
        }
        let implicit = self.implicit.borrow().clone();
        if let Some(name) = implicit
            && words.len() >= 2
            && words[words.len() - 2] == "release"
            && words[words.len() - 1] == name
        {
            let undone = words.iter().any(|w| w == "rollback");
            *self.implicit.borrow_mut() = None;
            let end = if undone { "ROLLBACK" } else { "COMMIT" };
            let outcome = self.with(|session| {
                session.client.batch_execute(sql)?;
                Ok(session.client.batch_execute(end)?)
            });
            if outcome.is_err() {
                let _ = self.with(|session| Ok(session.client.batch_execute("ROLLBACK")?));
            }
            self.in_transaction.set(false);
            return outcome;
        }
        let ends = trimmed.eq_ignore_ascii_case("COMMIT")
            || trimmed.eq_ignore_ascii_case("ROLLBACK")
            || trimmed.eq_ignore_ascii_case("END");
        let sql = &*sqlite_ddl(sql);
        let ddl = changes_schema(sql);
        let generation = self.database.generation.clone();
        self.with(move |session| {
            session.client.batch_execute(sql)?;
            if ddl {
                session.statements.clear();
                session.generation =
                    generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            }
            Ok(())
        })?;
        if ends {
            self.in_transaction.set(false);
            *self.implicit.borrow_mut() = None;
        }
        Ok(())
    }

    pub fn query_row<T, P: Params, F>(&self, sql: &str, params: P, f: F) -> Result<T>
    where
        F: FnOnce(&Row) -> Result<T>,
    {
        let rows = self.query_all(sql, params)?;
        let row = rows
            .rows
            .into_iter()
            .next()
            .ok_or(Error::QueryReturnedNoRows)?;
        f(&row)
    }

    fn query_all<P: Params>(&self, sql: &str, params: P) -> Result<Rows> {
        let values = params.values()?;
        if let Some(lite) = &self.lite {
            let (_, rows) = Self::lite_query(lite, sql, &values)?;
            return Ok(Rows {
                rows,
                next: 0,
                current: None,
            });
        }
        let (columns, rows) = self.run(sql, &values, |client, statement, refs| {
            let rows = client.query(statement, refs)?;
            Ok((
                statement
                    .columns()
                    .iter()
                    .map(|c| c.name().to_string())
                    .collect::<Vec<_>>(),
                rows,
            ))
        })?;
        // Every table has a `rowid` column (as every SQLite table had a
        // rowid), which `SELECT *` lists in PostgreSQL but never did in
        // SQLite: it is left out unless the statement names it.
        let hidden: Vec<bool> = if sql.to_ascii_lowercase().contains("rowid") {
            vec![false; columns.len()]
        } else {
            columns.iter().map(|c| c == "rowid").collect()
        };
        let columns = Arc::new(
            columns
                .into_iter()
                .zip(&hidden)
                .filter(|(_, h)| !**h)
                .map(|(c, _)| c)
                .collect::<Vec<_>>(),
        );
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut values = Vec::with_capacity(row.len());
            for (i, hide) in hidden.iter().enumerate().take(row.len()) {
                if *hide {
                    continue;
                }
                let raw: Raw = row.try_get(i).map_err(Error::from)?;
                values.push(raw.0);
            }
            out.push(Row {
                columns: columns.clone(),
                values,
            });
        }
        Ok(Rows {
            rows: out,
            next: 0,
            current: None,
        })
    }

    /// A statement (`prepare` and `prepare_cached` are the same: every
    /// connection caches its prepared statements).
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        if let Some(lite) = &self.lite {
            let columns = lite
                .prepare_cached(&sqlite_sql(sql))?
                .column_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            return Ok(Statement {
                connection: self,
                sql: sql.to_string(),
                columns,
            });
        }
        let named = sql.to_ascii_lowercase().contains("rowid");
        let columns = self.with(|session| {
            let (statement, _) = session.statement(sql)?;
            Ok(statement
                .columns()
                .iter()
                .map(|c| c.name().to_string())
                .filter(|c| named || c != "rowid")
                .collect::<Vec<_>>())
        })?;
        Ok(Statement {
            connection: self,
            sql: sql.to_string(),
            columns,
        })
    }
    pub fn prepare_cached(&self, sql: &str) -> Result<Statement<'_>> {
        self.prepare(sql)
    }

    /// Rows changed by the last `execute`.
    pub fn changes(&self) -> u64 {
        if let Some(lite) = &self.lite {
            return lite.changes();
        }
        self.changes.get()
    }
    pub fn is_autocommit(&self) -> bool {
        if let Some(lite) = &self.lite {
            return lite.is_autocommit();
        }
        !self.in_transaction.get()
    }

    /// How long a writer waits for the write lock (SQLite's busy timeout).
    pub fn busy_timeout(&self, timeout: Duration) -> Result<()> {
        self.busy.set(timeout);
        self.apply_busy()
    }
    fn apply_busy(&self) -> Result<()> {
        if let Some(lite) = &self.lite {
            return Ok(lite.busy_timeout(self.busy.get())?);
        }
        let ms = self.busy.get().as_millis().clamp(1, i32::MAX as u128);
        self.with(|session| {
            // Foreign keys are on for every connection handed out.
            if session.replica {
                session
                    .client
                    .batch_execute("SET session_replication_role = origin")?;
                session.replica = false;
            }
            // Pooled sessions keep their setting: set it only when it
            // changes, not on every checkout.
            if session.lock_timeout_ms != ms {
                session
                    .client
                    .batch_execute(&format!("SET lock_timeout = {ms}"))
                    .map_err(Error::from)?;
                session.lock_timeout_ms = ms;
            }
            Ok(())
        })
    }

    /// SQLite pragmas this store used to set. They have no PostgreSQL
    /// counterpart (the server owns journaling, foreign keys are always on,
    /// a read-only snapshot is [`Connection::read_transaction`]) and are
    /// accepted and ignored; any other pragma is refused.
    pub fn pragma_update<V: ToSql>(
        &self,
        schema: Option<&str>,
        name: &str,
        value: V,
    ) -> Result<()> {
        if let Some(lite) = &self.lite {
            let value = value.to_sql()?;
            return Ok(lite.pragma_update(schema, name, Lite(&value))?);
        }
        if name == "foreign_keys" {
            // Off (tests that write a store as an older program left it):
            // the session stops enforcing foreign keys until it is handed
            // out again. Needs a role that may set session_replication_role.
            let on = !matches!(value.to_sql()?, Value::Integer(0))
                && !matches!(value.to_sql()?, Value::Text(ref t) if t.eq_ignore_ascii_case("off") || t == "0" || t.eq_ignore_ascii_case("false"));
            let sql = if on {
                "SET session_replication_role = origin"
            } else {
                "SET session_replication_role = replica"
            };
            return self.with(move |session| {
                session.client.batch_execute(sql)?;
                session.replica = !on;
                Ok(())
            });
        }
        match name {
            "journal_mode" | "secure_delete" | "synchronous" | "query_only" | "busy_timeout"
            | "cache_size" | "mmap_size" | "temp_store" | "wal_autocheckpoint"
            | "journal_size_limit" => Ok(()),
            _ => Err(Error::Other(format!(
                "PRAGMA {name} is not supported on PostgreSQL"
            ))),
        }
    }

    fn begin(&self, behavior: TransactionBehavior, read_only: bool) -> Result<()> {
        if self.in_transaction.get() {
            return Err(Error::Other(
                "cannot start a transaction within a transaction".into(),
            ));
        }
        if let Some(lite) = &self.lite {
            if read_only {
                lite.execute_batch("PRAGMA query_only=ON")?;
                self.query_only.set(true);
            }
            let begin = match (read_only, behavior) {
                (true, _) | (false, TransactionBehavior::Deferred) => "BEGIN DEFERRED",
                (false, TransactionBehavior::Immediate) => "BEGIN IMMEDIATE",
                (false, TransactionBehavior::Exclusive) => "BEGIN EXCLUSIVE",
            };
            if let Err(error) = lite.execute_batch(begin) {
                self.end_query_only();
                return Err(error.into());
            }
            self.in_transaction.set(true);
            return Ok(());
        }
        let key = self.database.lock_key;
        self.with(|session| {
            if read_only {
                session
                    .client
                    .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")?;
            } else {
                session.client.batch_execute("BEGIN")?;
                // Writers serialize on the store's lock, as SQLite's
                // single write lock did; it is released at commit or
                // rollback. A deferred transaction takes it too: this
                // store does not upgrade a reader into a writer.
                let _ = behavior;
                if let Err(error) = session
                    .client
                    .execute("SELECT pg_advisory_xact_lock($1)", &[&key])
                {
                    let _ = session.client.batch_execute("ROLLBACK");
                    return Err(Error::from(error));
                }
            }
            Ok(())
        })?;
        self.in_transaction.set(true);
        Ok(())
    }

    /// A write transaction: it holds the store's write lock until it ends.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.transaction_with_behavior(TransactionBehavior::Deferred)
    }
    pub fn transaction_with_behavior(
        &mut self,
        behavior: TransactionBehavior,
    ) -> Result<Transaction<'_>> {
        self.begin_or_nest(behavior)
    }
    /// `rusqlite`'s `unchecked_transaction`: a write transaction from a
    /// shared reference (a savepoint inside an open transaction).
    pub fn unchecked_transaction(&self) -> Result<Transaction<'_>> {
        self.begin_or_nest(TransactionBehavior::Deferred)
    }
    fn begin_or_nest(&self, behavior: TransactionBehavior) -> Result<Transaction<'_>> {
        if self.in_transaction.get() {
            let depth = self.savepoints.get() + 1;
            self.savepoints.set(depth);
            let name = format!("graphicle_nested_{depth}");
            self.control(&format!("SAVEPOINT {name}"))?;
            return Ok(Transaction {
                connection: self,
                finished: false,
                savepoint: Some(name),
            });
        }
        self.begin(behavior, false)?;
        Ok(Transaction {
            connection: self,
            finished: false,
            savepoint: None,
        })
    }
    /// A snapshot that may write elsewhere (copying a store into a scratch
    /// schema): `REPEATABLE READ`, without the write lock.
    pub fn snapshot_transaction(&mut self) -> Result<Transaction<'_>> {
        if self.in_transaction.get() {
            return Err(Error::Other(
                "cannot start a transaction within a transaction".into(),
            ));
        }
        if self.lite.is_some() {
            self.control("BEGIN DEFERRED")?;
        } else {
            self.control("BEGIN ISOLATION LEVEL REPEATABLE READ")?;
        }
        self.in_transaction.set(true);
        Ok(Transaction {
            connection: self,
            finished: false,
            savepoint: None,
        })
    }
    /// A write transaction that does not take the store's write lock
    /// (`READ COMMITTED`): for a counter row, whose own row lock orders
    /// concurrent writers. Only for writes that never depend on what other
    /// writers do outside the rows they lock.
    pub fn row_lock_transaction(&mut self) -> Result<Transaction<'_>> {
        if self.in_transaction.get() {
            return Err(Error::Other(
                "cannot start a transaction within a transaction".into(),
            ));
        }
        // SQLite has one write lock: the counter takes it, as every write
        // did before.
        if self.lite.is_some() {
            self.control("BEGIN IMMEDIATE")?;
        } else {
            self.control("BEGIN ISOLATION LEVEL READ COMMITTED")?;
        }
        self.in_transaction.set(true);
        Ok(Transaction {
            connection: self,
            finished: false,
            savepoint: None,
        })
    }
    /// A read-only snapshot: it sees the store as of its first statement,
    /// never waits for writers and refuses to write.
    pub fn read_transaction(&mut self) -> Result<Transaction<'_>> {
        self.begin(TransactionBehavior::Deferred, true)?;
        Ok(Transaction {
            connection: self,
            finished: false,
            savepoint: None,
        })
    }

    /// SQLite: turn `query_only` off after a read transaction.
    fn end_query_only(&self) {
        if self.query_only.replace(false)
            && let Some(lite) = &self.lite
        {
            let _ = lite.execute_batch("PRAGMA query_only=OFF");
        }
    }

    /// A writer that sends rows in multi-row statements ([`Batch`]).
    pub fn batch<'c>(&'c self, head: &str, width: usize, tail: &str) -> Batch<'c> {
        Batch::new(self, head, width, tail)
    }

    /// `n` fresh values of `table`'s identity column `column`, in one round
    /// trip (ids for rows a [`Batch`] will insert with their ids).
    pub fn next_ids(&self, table: &str, column: &str, n: usize) -> Result<Vec<i64>> {
        if self.lite.is_some() {
            // SQLite has no sequence to take ids from: inside the write
            // transaction, the ids after both the table's largest and its
            // AUTOINCREMENT record, and after any this connection handed out
            // that are not written yet.
            if !valid_identifier(table) || !valid_identifier(column) {
                return Err(Error::Other(format!("Invalid table {table}")));
            }
            let found: i64 = self.query_row(
                &format!(
                    "SELECT max(coalesce((SELECT max({column}) FROM {table}),0),coalesce((SELECT seq FROM sqlite_sequence WHERE name=?),0))"
                ),
                [table],
                |r| r.get(0),
            )?;
            let mut allocated = self.allocated.borrow_mut();
            let last = allocated.entry(table.to_string()).or_insert(0);
            let start = found.max(*last);
            *last = start + n as i64;
            return Ok((1..=n as i64).map(|i| start + i).collect());
        }
        let rows = self.query_all(
            "SELECT nextval(pg_get_serial_sequence($1, $2)) FROM generate_series(1, $3)",
            params![table, column, n as i64],
        )?;
        rows.rows.iter().map(|r| r.get(0)).collect()
    }

    /// Bulk-copy rows in (`COPY … FROM STDIN`) from `data`.
    pub fn copy_in(&self, sql: &str, data: &mut (dyn std::io::Read + Send)) -> Result<u64> {
        if self.lite.is_some() {
            return Err(Error::Other("COPY needs a PostgreSQL store".into()));
        }
        self.with(|session| {
            let mut writer = session.client.copy_in(sql)?;
            std::io::copy(data, &mut writer).map_err(|e| Error::Other(e.to_string()))?;
            Ok(writer.finish()?)
        })
    }
    /// Bulk-copy rows out (`COPY … TO STDOUT`) into `out`.
    pub fn copy_out(&self, sql: &str, out: &mut (dyn std::io::Write + Send)) -> Result<()> {
        if self.lite.is_some() {
            return Err(Error::Other("COPY needs a PostgreSQL store".into()));
        }
        self.with(|session| {
            let mut reader = session.client.copy_out(sql)?;
            std::io::copy(&mut reader, out).map_err(|e| Error::Other(e.to_string()))?;
            Ok(())
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Never hand a connection with an open transaction back to the pool.
        // A rollback that fails leaves a closed client, which the pool
        // discards (`Manager::has_broken`).
        if self.in_transaction.get()
            && let Some(session) = self.session.get_mut().as_mut()
        {
            let _ = session.client.batch_execute("ROLLBACK");
        }
    }
}

/// An open transaction; dropping it rolls it back. One begun inside
/// another (a migration that opens its own) is a savepoint of it.
pub struct Transaction<'c> {
    connection: &'c Connection,
    finished: bool,
    savepoint: Option<String>,
}

impl Transaction<'_> {
    pub fn commit(mut self) -> Result<()> {
        self.finished = true;
        let sql = match &self.savepoint {
            Some(name) => format!("RELEASE SAVEPOINT {name}"),
            None => "COMMIT".to_string(),
        };
        let outcome = self.connection.control(&sql);
        if self.savepoint.is_none() {
            if outcome.is_err() && self.connection.lite.is_some() {
                // A commit SQLite refused (busy) leaves the transaction
                // open: roll it back, as rusqlite's did on drop.
                let _ = self.connection.control("ROLLBACK");
            }
            self.connection.in_transaction.set(false);
            self.connection.end_query_only();
        }
        outcome
    }
    pub fn rollback(mut self) -> Result<()> {
        self.finished = true;
        self.undo()
    }
    fn undo(&self) -> Result<()> {
        let sql = match &self.savepoint {
            Some(name) => format!("ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"),
            None => "ROLLBACK".to_string(),
        };
        let outcome = self.connection.control(&sql);
        if self.savepoint.is_none() {
            self.connection.in_transaction.set(false);
            self.connection.end_query_only();
        }
        outcome
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.finished && self.connection.in_transaction.get() {
            let _ = self.undo();
        }
    }
}

impl std::ops::Deref for Transaction<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.connection
    }
}

// ---------------------------------------------------------------------------
// Batched writes

/// Rows sent as multi-row statements: `head` (`INSERT INTO t(a,b) VALUES`,
/// or `UPDATE … FROM (VALUES`) followed by the rows' value lists, then
/// `tail`. A round trip carries up to [`Batch::ROWS`] rows instead of one,
/// which keeps a large projection's write transaction, and so the store's
/// write lock, short. Rows are sent in the order pushed; [`Batch::flush`]
/// sends what is left (and must run before anything reads the rows back).
pub struct Batch<'c> {
    connection: &'c Connection,
    head: String,
    tail: String,
    width: usize,
    values: Vec<Value>,
}

impl<'c> Batch<'c> {
    /// Rows per statement at most (and never more than PostgreSQL's 65,535
    /// parameters).
    pub const ROWS: usize = 1000;

    fn new(connection: &'c Connection, head: &str, width: usize, tail: &str) -> Self {
        Self {
            connection,
            head: head.to_string(),
            tail: tail.to_string(),
            width: width.max(1),
            values: Vec::new(),
        }
    }

    fn per(&self) -> usize {
        Self::ROWS.min(65_535 / self.width)
    }

    /// Add one row (`width` values).
    pub fn push<P: Params>(&mut self, row: P) -> Result<()> {
        let values = row.values()?;
        if values.len() != self.width {
            return Err(Error::Other(format!(
                "A batch row has {} values, not {}",
                values.len(),
                self.width
            )));
        }
        self.values.extend(values);
        if self.values.len() >= self.per() * self.width {
            self.flush()?;
        }
        Ok(())
    }

    /// Whether the next row pushed sends the batch (so that a batch whose
    /// rows refer to another's can send that one first).
    pub fn full_after_one(&self) -> bool {
        self.values.len() + self.width >= self.per() * self.width
    }

    /// Send the rows pushed so far.
    pub fn flush(&mut self) -> Result<()> {
        if self.values.is_empty() {
            return Ok(());
        }
        let rows = self.values.len() / self.width;
        let sqlite = self.connection.engine() == Engine::Sqlite;
        let mut sql =
            String::with_capacity(self.head.len() + rows * self.width * 6 + self.tail.len());
        sql.push_str(&self.head);
        let mut n = 1;
        for row in 0..rows {
            sql.push_str(if row == 0 { " (" } else { ",(" });
            for column in 0..self.width {
                if column > 0 {
                    sql.push(',');
                }
                if sqlite {
                    sql.push('?');
                } else {
                    sql.push('$');
                    sql.push_str(&n.to_string());
                }
                n += 1;
            }
            sql.push(')');
        }
        sql.push(' ');
        sql.push_str(&self.tail);
        let values = std::mem::take(&mut self.values);
        self.connection.execute(&sql, values)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Statements and rows

/// A prepared statement.
pub struct Statement<'c> {
    connection: &'c Connection,
    sql: String,
    columns: Vec<String>,
}

impl Statement<'_> {
    pub fn execute<P: Params>(&mut self, params: P) -> Result<usize> {
        self.connection.execute(&self.sql, params)
    }
    pub fn query<P: Params>(&mut self, params: P) -> Result<Rows> {
        self.connection.query_all(&self.sql, params)
    }
    pub fn query_row<T, P: Params, F>(&mut self, params: P, f: F) -> Result<T>
    where
        F: FnOnce(&Row) -> Result<T>,
    {
        self.connection.query_row(&self.sql, params, f)
    }
    pub fn query_map<T, P: Params, F>(&mut self, params: P, mut f: F) -> Result<MappedRows<T>>
    where
        F: FnMut(&Row) -> Result<T>,
    {
        let rows = self.connection.query_all(&self.sql, params)?;
        let mapped: Vec<Result<T>> = rows.rows.iter().map(&mut f).collect();
        Ok(MappedRows {
            rows: mapped.into_iter(),
        })
    }
    pub fn exists<P: Params>(&mut self, params: P) -> Result<bool> {
        Ok(!self
            .connection
            .query_all(&self.sql, params)?
            .rows
            .is_empty())
    }
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(String::as_str).collect()
    }
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }
}

/// Rows of a query, read as `rusqlite::Rows` are: `while let Some(row) =
/// rows.next()?`.
pub struct Rows {
    rows: Vec<Row>,
    next: usize,
    current: Option<usize>,
}

impl Rows {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<&Row>> {
        if self.next >= self.rows.len() {
            self.current = None;
            return Ok(None);
        }
        self.current = Some(self.next);
        self.next += 1;
        Ok(self.current.map(|i| &self.rows[i]))
    }
    /// Map each remaining row (`rusqlite`'s `Rows::mapped`).
    pub fn mapped<T, F: FnMut(&Row) -> Result<T>>(self, f: F) -> MappedRows<T> {
        let rows: Vec<Result<T>> = self.rows[self.next..].iter().map(f).collect();
        MappedRows {
            rows: rows.into_iter(),
        }
    }
}

/// The results of `query_map`.
pub struct MappedRows<T> {
    rows: std::vec::IntoIter<Result<T>>,
}
impl<T> Iterator for MappedRows<T> {
    type Item = Result<T>;
    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next()
    }
}

/// One row of a result.
pub struct Row {
    columns: Arc<Vec<String>>,
    values: Vec<Value>,
}

/// A column by position or name.
pub trait RowIndex {
    fn index(&self, row: &Row) -> Result<usize>;
}
impl RowIndex for usize {
    fn index(&self, row: &Row) -> Result<usize> {
        if *self < row.values.len() {
            Ok(*self)
        } else {
            Err(Error::InvalidColumn(self.to_string()))
        }
    }
}
impl RowIndex for &str {
    fn index(&self, row: &Row) -> Result<usize> {
        row.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(self))
            .ok_or_else(|| Error::InvalidColumn((*self).to_string()))
    }
}

impl Row {
    pub fn get<I: RowIndex, T: FromSqlValue>(&self, index: I) -> Result<T> {
        let i = index.index(self)?;
        T::column_result(self.values[i].as_ref()).map_err(|error| match error {
            FromSqlError::InvalidType => {
                Error::InvalidColumnType(i, self.columns[i].clone(), std::any::type_name::<T>())
            }
            other => Error::Other(format!("column {}: {other}", self.columns[i])),
        })
    }
    pub fn get_ref<I: RowIndex>(&self, index: I) -> Result<ValueRef<'_>> {
        let i = index.index(self)?;
        Ok(self.values[i].as_ref())
    }
    pub fn get_unwrap<I: RowIndex, T: FromSqlValue>(&self, index: I) -> T {
        match self.get(index) {
            Ok(value) => value,
            Err(error) => panic!("{error}"),
        }
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(String::as_str).collect()
    }
}

/// Types under `rusqlite::types`.
pub mod types {
    pub use super::FromSqlValue as FromSql;
    pub use super::{FromSqlError, FromSqlResult, Null, Value, ValueRef};
}

/// A digest of what the connection's schema holds: per table, its row count
/// and the newest row version (`xmin`). Any committed insert, update or
/// delete changes it (SQLite's `PRAGMA data_version`, for tests that check
/// that an operation wrote nothing).
pub fn data_version(db: &Connection) -> Result<String> {
    if db.engine() == Engine::Sqlite {
        let version: i64 = db.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        return Ok(version.to_string());
    }
    let tables: Vec<String> = {
        let mut statement = db.prepare(
            "SELECT table_name FROM information_schema.tables WHERE table_schema=current_schema() AND table_type='BASE TABLE' ORDER BY table_name",
        )?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_>>()?
    };
    let mut out = String::new();
    for table in tables {
        let (count, newest): (i64, i64) = db.query_row(
            &format!(
                "SELECT count(*), coalesce(max(xmin::text::bigint),0) FROM \"{}\"",
                table.replace('"', "\"\"")
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        out.push_str(&format!("{table}:{count}:{newest};"));
    }
    Ok(out)
}

/// For tests and tools: the store of `directory`, which must exist.
pub fn open_existing(directory: impl AsRef<Path>) -> Result<Connection> {
    Database::for_directory(directory.as_ref(), false)?.connect()
}

/// Whether the file at `path` is a SQLite database (its 16-byte header).
pub fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read;
    let mut header = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok()
        && &header == b"SQLite format 3\0"
}

/// A data folder's `postgres.json`, which names its PostgreSQL schema.
pub fn schema_file(directory: &Path) -> PathBuf {
    directory.join(SCHEMA_FILE)
}

/// The data folder's store file, for messages and the files that travel
/// with a data folder: `reviews.sqlite3`, or `postgres.json` for a
/// PostgreSQL store ([`Database::engine_of`]).
pub fn store_file(directory: &Path) -> PathBuf {
    match Database::engine_of(directory) {
        Engine::Postgres => directory.join(SCHEMA_FILE),
        Engine::Sqlite => directory.join(SQLITE_FILE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_arrays_quote_their_values() {
        assert_eq!(text_array::<&str>(&[]), "{}");
        assert_eq!(
            text_array(&["a", "b c", "q\"uote", "back\\slash", "{x,y}", "NULL"]),
            r#"{"a","b c","q\"uote","back\\slash","{x,y}","NULL"}"#
        );
    }

    #[test]
    fn placeholders_are_numbered_outside_quotes_and_comments() {
        assert_eq!(translate("SELECT 1"), "SELECT 1");
        assert_eq!(
            translate("SELECT * FROM t WHERE a=? AND b=?"),
            "SELECT * FROM t WHERE a=$1 AND b=$2"
        );
        assert_eq!(
            translate("SELECT '?', \"?\" FROM t WHERE a=?"),
            "SELECT '?', \"?\" FROM t WHERE a=$1"
        );
        assert_eq!(
            translate("SELECT 'it''s ?' , ? -- what?\n, ?"),
            "SELECT 'it''s ?' , $1 -- what?\n, $2"
        );
        assert_eq!(translate("SELECT ?2, ?1, ?"), "SELECT $2, $1, $3");
        assert_eq!(translate("/* ? */ SELECT ?"), "/* ? */ SELECT $1");
        assert_eq!(translate("SELECT $$?$$, ?"), "SELECT $$?$$, $1");
    }

    #[test]
    fn parameters_left_out_are_renumbered() {
        assert_eq!(
            translate_ordered("SELECT ?1, ?2"),
            ("SELECT $1, $2".to_string(), vec![])
        );
        assert_eq!(
            translate_ordered("SELECT count(*) FROM t WHERE a=?2"),
            ("SELECT count(*) FROM t WHERE a=$1".to_string(), vec![1])
        );
        assert_eq!(
            translate_ordered("SELECT '$2', ?3, ?1"),
            ("SELECT '$2', $2, $1".to_string(), vec![0, 2])
        );
        assert_eq!(
            translate_ordered("SELECT 'é', ?2"),
            ("SELECT 'é', $1".to_string(), vec![1])
        );
    }

    #[test]
    fn nul_characters_round_trip() {
        for text in [
            "a\0b",
            "\u{10FFFF}",
            "\u{10FFFF}0",
            "\0\u{10FFFF}\0",
            "plain",
        ] {
            assert_eq!(unescape_nul(&escape_nul(text)), text);
            assert!(!escape_nul(text).contains('\0'));
        }
        assert_ne!(escape_nul("a\0"), escape_nul("a\u{10FFFF}0"));
    }

    #[test]
    fn sqlite_table_definitions_take_postgresql_types() {
        assert_eq!(
            sqlite_ddl(
                "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, n integer, r REAL, b BLOB) WITHOUT ROWID"
            ),
            "CREATE TABLE t(id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, n BIGINT, r DOUBLE PRECISION, b BYTEA)"
        );
        assert_eq!(sqlite_ddl("SELECT 1"), "SELECT 1");
        assert_eq!(
            sqlite_ddl("CREATE TABLE t(k TEXT DEFAULT 'INTEGER')"),
            "CREATE TABLE t(k TEXT DEFAULT 'INTEGER')"
        );
        assert_eq!(
            sqlite_ddl(
                "-- the edition's tables\nCREATE TABLE t(n INTEGER); -- it's\nCREATE TABLE u(m INTEGER)"
            ),
            "-- the edition's tables\nCREATE TABLE t(n BIGINT); -- it's\nCREATE TABLE u(m BIGINT)"
        );
    }

    #[test]
    fn numeric_decodes_integers_and_fractions() {
        // 12345678 = digits [1234, 5678], weight 1
        let raw = [0u8, 2, 0, 1, 0, 0, 0, 0, 0x04, 0xD2, 0x16, 0x2E];
        assert_eq!(numeric(&raw).unwrap(), Value::Integer(12_345_678));
        // -1.5 = digits [1, 5000], weight 0, scale 1
        let raw = [0u8, 2, 0, 0, 0x40, 0, 0, 1, 0, 1, 0x13, 0x88];
        assert_eq!(numeric(&raw).unwrap(), Value::Real(-1.5));
        // 0
        let raw = [0u8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(numeric(&raw).unwrap(), Value::Integer(0));
    }

    #[test]
    fn schema_names_are_checked() {
        assert!(valid_schema("g_0123456789abcdef01234567"));
        assert!(!valid_schema("g_0123456789ABCDEF01234567"));
        assert!(!valid_schema("public"));
        assert!(!valid_schema("g_x\"; DROP SCHEMA public; --"));
    }

    fn policy(url: &str, allow_plaintext: bool) -> Result<bool> {
        let mut config: postgres::Config = url.parse().unwrap();
        tls_policy(&mut config, allow_plaintext)
    }

    #[test]
    fn a_remote_server_needs_tls() {
        // This machine: TLS only when the address asks for it.
        assert!(!policy("host=/var/run/postgresql dbname=g", false).unwrap());
        assert!(!policy("host=localhost dbname=g", false).unwrap());
        assert!(!policy("postgresql://u@127.0.0.1/g", false).unwrap());
        assert!(!policy("host=::1 dbname=g", false).unwrap());
        assert!(policy("host=localhost sslmode=require dbname=g", false).unwrap());
        // Another machine: TLS, checked, whatever the address prefers.
        assert!(policy("host=db.example.com dbname=g", false).unwrap());
        assert!(policy("host=db.example.com sslmode=prefer dbname=g", false).unwrap());
        assert!(policy("postgresql://u:p@10.1.2.3:5432/g", false).unwrap());
        // A local name with a remote address is remote.
        assert!(policy("host=localhost hostaddr=10.1.2.3 dbname=g", false).unwrap());
        // No TLS to another machine only when explicitly allowed.
        let refused =
            policy("host=db.example.com sslmode=disable dbname=g", false).expect_err("refused");
        assert!(refused.to_string().contains(PLAINTEXT_ENV), "{refused}");
        assert!(!policy("host=db.example.com sslmode=disable dbname=g", true).unwrap());
    }

    #[test]
    fn verify_full_is_read_as_require() {
        let settings = Settings {
            url: "host=db.example.com sslmode=verify-full dbname=g".into(),
            pool: 2,
            allow_plaintext: false,
            root_cert: None,
            statement_timeout: DEFAULT_STATEMENT_TIMEOUT,
        };
        let (config, tls) = settings.config().unwrap();
        assert!(tls);
        assert!(matches!(
            config.get_ssl_mode(),
            postgres::config::SslMode::Require
        ));
    }

    #[test]
    fn passwords_never_reach_error_text_or_debug_output() {
        let url = "postgresql://graphicle:s3cret-pass@db.example.com/g";
        let message = format!("could not connect with {url}: password s3cret-pass refused");
        let redacted = redact(&message, url);
        assert!(!redacted.contains("s3cret-pass"), "{redacted}");
        let keyword = "host=db.example.com password=hunter22x user=g";
        assert!(!redact("bad password hunter22x", keyword).contains("hunter22x"));
        let settings = Settings {
            url: url.into(),
            pool: 2,
            allow_plaintext: false,
            root_cert: None,
            statement_timeout: DEFAULT_STATEMENT_TIMEOUT,
        };
        let shown = format!("{settings:?}");
        assert!(
            !shown.contains("s3cret-pass") && !shown.contains("db.example.com"),
            "{shown}"
        );
    }

    #[test]
    fn sqlite_statements_drop_postgresql_casts() {
        assert_eq!(
            sqlite_sql("SELECT x FROM t WHERE (?2::text IS NULL OR a=?2) LIMIT ALL OFFSET 5"),
            "SELECT x FROM t WHERE (?2 IS NULL OR a=?2) LIMIT -1 OFFSET 5"
        );
        assert_eq!(sqlite_sql("SELECT a||chr(10)||b"), "SELECT a||char(10)||b");
        // Quoted text is left as it is.
        assert_eq!(
            sqlite_sql("SELECT '::text', 'LIMIT ALL'"),
            "SELECT '::text', 'LIMIT ALL'"
        );
        assert_eq!(sqlite_sql("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn password_files_are_read_as_libpq_reads_them() {
        let text = "# comment\nother:5432:*:g:nope\ndb.example.com:*:graphicle:g:pa\\:ss\n*:*:*:*:fallback\n";
        let wanted = |host: &str| [host, "5432", "graphicle", "g"].map(str::to_string);
        assert_eq!(
            pgpass_lookup(text, &wanted("db.example.com")).as_deref(),
            Some("pa:ss")
        );
        assert_eq!(
            pgpass_lookup(text, &wanted("elsewhere")).as_deref(),
            Some("fallback")
        );
        assert_eq!(pgpass_lookup("other:5432:*:g:nope\n", &wanted("db")), None);
    }

    #[test]
    fn plain_identifiers_only() {
        assert!(valid_identifier("gf_edges"));
        assert!(!valid_identifier("gf_edges; DROP TABLE x"));
        assert!(!valid_identifier("1abc"));
        assert!(!valid_identifier(""));
    }

    #[test]
    fn without_a_database_url_a_store_is_a_sqlite_file() {
        if postgres_configured() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Database::engine_of(dir.path()), Engine::Sqlite);
        let database = Database::for_directory(dir.path(), true).unwrap();
        assert_eq!(database.engine(), Engine::Sqlite);
        let db = database.connect().unwrap();
        db.execute_batch("CREATE TABLE t(a INTEGER)").unwrap();
        assert!(dir.path().join(SQLITE_FILE).is_file());
        assert!(!dir.path().join(SCHEMA_FILE).exists());
        // A folder with postgres.json is a PostgreSQL store, which needs the
        // server's address.
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join(SCHEMA_FILE), b"").unwrap();
        assert_eq!(Database::engine_of(other.path()), Engine::Postgres);
        let refused = Database::for_directory(other.path(), true).expect_err("refused");
        assert!(refused.to_string().contains(URL_ENV), "{refused}");
    }
}
