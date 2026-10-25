// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/pg_introspect.py
//! PostgreSQL catalog → DDL → native SQL extraction. The optional `postgres`
//! feature uses libpq directly, preserving its DSN, environment and TLS support.
use crate::{Result, extractors::sql, path_syntax};
use serde_json::{Map, Value, json};
use std::path::Path;
pub const TRANSACTION: &str = include_str!("assets/postgres/transaction.sql");
pub const QUERIES: [&str; 4] = [
    include_str!("assets/postgres/tables.sql"),
    include_str!("assets/postgres/views.sql"),
    include_str!("assets/postgres/routines.sql"),
    include_str!("assets/postgres/foreign_keys.sql"),
];
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
pub fn connection_error(message: &str) -> String {
    format!(
        "could not connect to PostgreSQL: {}",
        message.split('\n').next().unwrap_or("")
    )
}
pub trait CatalogConnection {
    fn begin_readonly(&mut self) -> Result<()>;
    fn query(&mut self, sql: &str) -> Result<Vec<Vec<Value>>>;
}
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Catalog {
    pub tables: Vec<Vec<Value>>,
    pub views: Vec<Vec<Value>>,
    pub routines: Vec<Vec<Value>>,
    pub fks: Vec<Vec<Value>>,
}
pub fn read_catalog(connection: &mut impl CatalogConnection) -> Result<Catalog> {
    connection.begin_readonly()?;
    Ok(Catalog {
        tables: connection.query(QUERIES[0])?,
        views: connection.query(QUERIES[1])?,
        routines: connection.query(QUERIES[2])?,
        fks: connection.query(QUERIES[3])?,
    })
}
fn text(v: &Value) -> Result<&str> {
    v.as_str()
        .ok_or_else(|| "TypeError: expected catalog text".into())
}
fn row(row: &[Value], length: usize) -> Result<&[Value]> {
    if row.len() != length {
        Err(format!(
            "ValueError: expected {length} catalog columns, got {}",
            row.len()
        )
        .into())
    } else {
        Ok(row)
    }
}
fn nonempty(v: &Value) -> bool {
    !v.is_null() && v != &json!("")
}
pub fn reconstruct_ddl(c: &Catalog) -> Result<String> {
    let mut ddl = Vec::new();
    for r in &c.tables {
        let r = row(r, 3)?;
        if r[2] == "BASE TABLE" {
            ddl.push(format!(
                "CREATE TABLE {}.{} (id INT);",
                quote_ident(text(&r[0])?),
                quote_ident(text(&r[1])?)
            ));
        }
    }
    for r in &c.views {
        let r = row(r, 3)?;
        let body = if nonempty(&r[2]) {
            text(&r[2])?
        } else {
            "SELECT 1"
        };
        ddl.push(format!(
            "CREATE VIEW {}.{} AS {body};",
            quote_ident(text(&r[0])?),
            quote_ident(text(&r[1])?)
        ));
    }
    for r in &c.fks {
        let r = row(r, 7)?;
        let cols = |v: &Value| -> Result<String> {
            v.as_array()
                .ok_or("TypeError: expected foreign key columns")?
                .iter()
                .map(|v| Ok(quote_ident(text(v)?)))
                .collect::<Result<Vec<_>>>()
                .map(|v| v.join(", "))
        };
        ddl.push(format!(
            "ALTER TABLE {}.{} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}.{}({});",
            quote_ident(text(&r[1])?),
            quote_ident(text(&r[2])?),
            quote_ident(text(&r[0])?),
            cols(&r[3])?,
            quote_ident(text(&r[4])?),
            quote_ident(text(&r[5])?),
            cols(&r[6])?
        ));
    }
    for r in &c.routines {
        let r = row(r, 5)?;
        let language = if nonempty(&r[4]) {
            text(&r[4])?
        } else {
            "plpgsql"
        }
        .to_lowercase();
        if r[2] == "FUNCTION" || r[2] == "PROCEDURE" {
            let body = if nonempty(&r[3]) {
                text(&r[3])?
            } else {
                "BEGIN SELECT 1; END;"
            };
            ddl.push(format!(
                "CREATE FUNCTION {}.{}() RETURNS void AS $gfx$ {body} $gfx$ LANGUAGE {language};",
                quote_ident(text(&r[0])?),
                quote_ident(text(&r[1])?)
            ));
        }
    }
    Ok(ddl.join("\n"))
}
pub fn extract_catalog(c: &Catalog, info: &Map<String, Value>) -> Result<Value> {
    let host = info
        .get("host")
        .map(text)
        .transpose()?
        .unwrap_or("localhost");
    let db = info.get("dbname").map(text).transpose()?.unwrap_or("db");
    let virtual_path = path_syntax::posix_join(&path_syntax::posix_parts(&format!(
        "postgresql://{host}/{db}"
    )));
    let ddl = reconstruct_ddl(c)?;
    let result = sql::extract_sql(Path::new(&virtual_path), Some(ddl.as_bytes()))?;
    if let Some(error) = result.get("error").filter(|v| nonempty(v)) {
        return Err(format!(
            "{} (required by PostgreSQL introspection)",
            crate::python_compat::string(error)
        )
        .into());
    }
    Ok(result)
}

#[cfg(feature = "postgres")]
mod driver {
    use super::*;
    use pq_sys as pq;
    use std::{
        ffi::{CStr, CString},
        ptr::NonNull,
    };
    fn cstring(s: &str) -> Result<CString> {
        CString::new(s).map_err(|_| "embedded null byte".into())
    }
    unsafe fn message(p: *const std::ffi::c_char) -> String {
        if p.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }
    struct PgResult(NonNull<pq::PGresult>);
    impl Drop for PgResult {
        fn drop(&mut self) {
            unsafe { pq::PQclear(self.0.as_ptr()) }
        }
    }
    pub struct Connection(NonNull<pq::PGconn>);
    impl Drop for Connection {
        fn drop(&mut self) {
            unsafe { pq::PQfinish(self.0.as_ptr()) }
        }
    }
    impl Connection {
        pub fn connect(dsn: &str) -> Result<Self> {
            let dsn = cstring(dsn)?;
            let pointer = NonNull::new(unsafe { pq::PQconnectdb(dsn.as_ptr()) })
                .ok_or("PostgreSQL connection allocation failed")?;
            let c = Self(pointer);
            if unsafe { pq::PQstatus(c.0.as_ptr()) } != pq::ConnStatusType::CONNECTION_OK {
                return Err((connection_error(&unsafe {
                    message(pq::PQerrorMessage(c.0.as_ptr()))
                }))
                .into());
            }
            let encoding = cstring("UTF8")?;
            if unsafe { pq::PQsetClientEncoding(c.0.as_ptr(), encoding.as_ptr()) } != 0 {
                return Err("PostgreSQL client could not select UTF-8".into());
            }
            Ok(c)
        }
        fn execute(&mut self, query: &str) -> Result<Vec<Vec<Value>>> {
            let query = cstring(query)?;
            let pointer = NonNull::new(unsafe { pq::PQexec(self.0.as_ptr(), query.as_ptr()) })
                .ok_or("PostgreSQL result allocation failed")?;
            let result = PgResult(pointer);
            let p = result.0.as_ptr();
            let status = unsafe { pq::PQresultStatus(p) };
            if status != pq::ExecStatusType::PGRES_TUPLES_OK
                && status != pq::ExecStatusType::PGRES_COMMAND_OK
            {
                return Err(unsafe { (message(pq::PQresultErrorMessage(p))).into() });
            }
            let mut rows = Vec::new();
            for i in 0..unsafe { pq::PQntuples(p) } {
                let mut row = Vec::new();
                for j in 0..unsafe { pq::PQnfields(p) } {
                    if unsafe { pq::PQgetisnull(p, i, j) } != 0 {
                        row.push(Value::Null);
                    } else {
                        let value = unsafe { message(pq::PQgetvalue(p, i, j)) };
                        let kind = unsafe { pq::PQftype(p, j) };
                        row.push(if kind == 1003 || kind == 1009 {
                            json!(parse_text_array(&value)?)
                        } else {
                            json!(value)
                        });
                    }
                }
                rows.push(row);
            }
            Ok(rows)
        }
    }
    impl CatalogConnection for Connection {
        fn begin_readonly(&mut self) -> Result<()> {
            self.execute("BEGIN")?;
            self.execute(TRANSACTION)?;
            Ok(())
        }
        fn query(&mut self, q: &str) -> Result<Vec<Vec<Value>>> {
            self.execute(q)
        }
    }
    pub fn conninfo(dsn: &str) -> Result<Map<String, Value>> {
        let dsn = cstring(dsn)?;
        let mut error = std::ptr::null_mut();
        let options = unsafe { pq::PQconninfoParse(dsn.as_ptr(), &mut error) };
        if options.is_null() {
            let msg = unsafe { message(error) };
            if !error.is_null() {
                unsafe { pq::PQfreemem(error.cast()) };
            }
            return Err((msg).into());
        }
        let mut map = Map::new();
        let mut index = 0;
        loop {
            let option = unsafe { &*options.add(index) };
            if option.keyword.is_null() {
                break;
            }
            if !option.val.is_null() {
                map.insert(
                    unsafe { message(option.keyword) },
                    json!(unsafe { message(option.val) }),
                );
            }
            index += 1;
        }
        unsafe { pq::PQconninfoFree(options) };
        Ok(map)
    }
}
pub fn parse_text_array(raw: &str) -> Result<Vec<Option<String>>> {
    let raw = if raw.starts_with('[') {
        raw.split_once('=')
            .map(|(_, s)| s)
            .ok_or("Invalid PostgreSQL array bounds")?
    } else {
        raw
    };
    let inner = raw
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or("Invalid PostgreSQL text array")?;
    if inner.is_empty() {
        return Ok(vec![]);
    }
    let mut chars = inner.chars().peekable();
    let mut out = Vec::new();
    loop {
        let mut value = String::new();
        let quoted = chars.peek() == Some(&'"');
        if quoted {
            chars.next();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '"' {
                    closed = true;
                    break;
                }
                if c == '\\' {
                    value.push(chars.next().ok_or("Invalid array escape")?);
                } else {
                    value.push(c);
                }
            }
            if !closed {
                return Err("Unclosed array quote".into());
            }
        } else {
            while chars.peek().is_some_and(|c| *c != ',') {
                let c = chars.next().unwrap();
                if c == '\\' {
                    value.push(chars.next().ok_or("Invalid array escape")?);
                } else if c == '{' || c == '}' {
                    return Err("Expected one-dimensional catalog array".into());
                } else {
                    value.push(c);
                }
            }
        }
        out.push(if !quoted && value == "NULL" {
            None
        } else {
            Some(value)
        });
        match chars.next() {
            Some(',') => {}
            None => break,
            _ => return Err("Invalid array separator".into()),
        }
    }
    Ok(out)
}
pub fn introspect_postgres(dsn: Option<&str>) -> Result<Value> {
    #[cfg(feature = "postgres")]
    {
        let dsn = dsn.unwrap_or("");
        let mut conn = driver::Connection::connect(dsn)?;
        let catalog = read_catalog(&mut conn)?;
        drop(conn);
        extract_catalog(&catalog, &driver::conninfo(dsn)?)
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = dsn;
        Err("PostgreSQL support requires the native `postgres` Cargo feature (libpq).".into())
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    match r["operation"].as_str().unwrap_or("") {
        "postgres.quote" => Ok(json!(quote_ident(text(&r["name"])?))),
        "postgres.connection_error" => Ok(json!(connection_error(text(&r["message"])?))),
        "postgres.array" => Ok(json!(parse_text_array(text(&r["text"])?)?)),
        "postgres.catalog" | "postgres.ddl" => {
            let catalog: serde_json::Value = r.get("catalog").cloned().unwrap_or_else(|| r.clone());
            let c: Catalog = serde_json::from_value(catalog).map_err(crate::Error::from)?;
            if r["operation"] == "postgres.ddl" {
                Ok(json!(reconstruct_ddl(&c)?))
            } else {
                extract_catalog(&c, r["info"].as_object().ok_or("Expected connection info")?)
            }
        }
        "postgres.queries" => Ok(json!({"transaction":TRANSACTION,"queries":QUERIES})),
        "postgres.introspect" => introspect_postgres(r["dsn"].as_str()),
        #[cfg(feature = "postgres")]
        "postgres.conninfo" => Ok(json!(driver::conninfo(r["dsn"].as_str().unwrap_or(""))?)),
        _ => Err("Unknown PostgreSQL operation".into()),
    }
}
