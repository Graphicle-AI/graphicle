//! Verified deletion ("certified forgetting") of connector documents, whole
//! connectors and projects.
//!
//! Forgetting removes the originals and every artifact that could carry their
//! information, verifies the result in the same transaction, and stores a
//! hash-chained receipt:
//! - artifacts computed from one source only (observer notes, passage
//!   vectors, single-source extraction drafts) go with that source and stay
//!   otherwise, because their input never contained the forgotten text;
//! - artifacts whose input could have included the forgotten documents (model
//!   caches, served-evidence records, outcomes and hints, reports and
//!   communities, overlay anchors, job history, model drafts built from
//!   retrieval) are removed from the moment those documents first became
//!   visible, and are recomputed on demand without them;
//! - dependency-index entries follow the artifacts they describe.
//!
//! The intended result equals a store that never ingested the documents.
//! [`state`] digests a store so the claim can be checked: fork a store before
//! the documents arrive, forget them in one copy, rerun the same work in both,
//! and compare (tests/it/forgetting.rs, tools/certify_forgetting.py). The state
//! contract is `graphicle.state.v2`: tables with no rows in the project's
//! scope are left out, so a store that has an edition's tables and one that
//! never had them digest equally when their content is equal; history is the
//! core's history tables plus those the installed extensions declare and, on
//! a store that has them, the commercial edition's team history tables ([`crate::store_compat`]),
//! so one store digests the same whichever edition reads it.
//!
//! Residue is reported, never hidden: records people wrote are not silently
//! destroyed, and the receipt is then not `verified_local`. Receipts outlive
//! projects and never store deleted identifiers, titles or text; documents are
//! named only by one-way hashes, which also stop a later sync re-importing
//! them. Backups taken earlier, provider-side retention of past model requests
//! and copies exported earlier are outside any receipt.
use crate::store::{Principal, Store, decode, one, repo, rows, s};
use crate::{Error, Result, digest, id, now, string};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params, params_from_iter};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const CONTRACT: &str = "graphicle.deletion.v1";
pub const STATE_CONTRACT: &str = "graphicle.state.v2";
const RECEIPTS: &str = "am_deletion_receipts";
const TOMBSTONES: &str = "am_forgotten_sources";
/// Whether a row's key or text belongs to the scope being forgotten.
type Matches = fn(&str, &Scope) -> bool;
const FINGERPRINTS: usize = 16;
const FINGERPRINT_BYTES: usize = 48;
const MIN_FINGERPRINT_BYTES: usize = 16;
const MAX_DOCUMENTS: usize = 1000;
/// Other sources' originals: identical text there is an independent copy.
const INDEPENDENT: &[&str] = &[
    "repo_objects",
    "repo_chunks",
    "repo_parses",
    "repo_snapshots",
    "repo_heads",
];
/// Kept by design when a project is deleted: audit trails, and the durable
/// external-cleanup queue that must name the scope until cleanup completes.
/// Installed extensions add theirs ([`retained`]).
const RETAINED: &[&str] = &["am_identity_audit", "am_projection_cleanup"];
const NOT_COVERED: &[&str] = &[
    "backups taken before the deletion",
    "model-provider retention of requests already sent",
    "copies exported from Graphicle before the deletion",
];
/// Records of work and access rather than memory. They are excluded from
/// state equality; residue scans still read them. Installed extensions add
/// theirs ([`history`]).
const HISTORY: &[&str] = &[
    "am_jobs",
    "am_model_calls",
    "am_call_reservations",
    "repo_events",
    "repo_syncs",
    "repo_runs",
    "repo_tool_events",
    "am_identity_audit",
    "am_projection_cleanup",
    TOMBSTONES,
    "am_oauth_pending",
    // What Graphicle served (`metering`): counters per day and credential.
    "am_served_usage",
];
/// Columns holding digests of capabilities minted per store (the owner's and
/// granted credentials'). Like minted identifiers, they never take part in
/// state equality.
const MINTED_DIGESTS: &[&str] = &["capability_hash"];

/// The tables kept by design: the installed extensions' (for example a team
/// audit trail) and the commercial edition's team audit trail when this store has it and no
/// extension named it ([`crate::store_compat`]), then the core's. One store
/// therefore lists the same tables in every edition.
fn retained(db: &Connection) -> Result<Vec<&'static str>> {
    let mut tables = crate::extension::installed().retained_tables();
    crate::store_compat::add_present(db, &mut tables, crate::store_compat::RETAINED_TABLES)?;
    tables.extend_from_slice(RETAINED);
    Ok(tables)
}
/// The history tables: the installed extensions' (for example sign-in
/// attempts and browser sessions) and the commercial edition's team history tables that this
/// store has and no extension named ([`crate::store_compat`]), then the
/// core's.
fn history(db: &Connection) -> Result<Vec<&'static str>> {
    let mut tables = crate::extension::installed().history_tables();
    crate::store_compat::add_present(db, &mut tables, crate::store_compat::HISTORY_TABLES)?;
    tables.extend_from_slice(HISTORY);
    Ok(tables)
}
/// Caches whose rows depend on when invalidation happened, not only on the
/// sources. Excluded from state equality; forgetting removes every entry
/// computed while the documents were visible.
const CACHES: &[&str] = &["am_model_cache", "am_query_cache"];
/// Where a project's folders are on this machine: the folder
/// aliases a local sync records, with canonical paths. Location, not
/// content: excluded from state equality, so two stores that hold the same
/// content digest equally wherever their folders are. Residue scans still
/// read them, and they go with their project.
const LOCATIONS: &[&str] = &["repo_folders"];
/// Sync counters: they count imports, not content.
const COUNTERS: &[(&str, &str)] = &[
    ("am_connectors", "generation"),
    ("am_connectors", "cursor"),
    ("am_connector_docs", "generation"),
    ("am_source_permissions", "generation"),
];
/// Artifacts whose input may include any document visible when they were
/// computed, with the column that dates them. Rows dated at or after the
/// first moment the forgotten documents were visible are removed.
const SINCE_VISIBLE: &[(&str, &str, &str)] = &[
    ("model_cache", "am_model_cache", "created_at"),
    ("preparation_steps", "am_preparation_steps", "created_at"),
    ("reports", "am_reports", "created_at"),
    ("communities", "am_communities", "created_at"),
    ("jobs", "am_jobs", "created_at"),
    ("events", "repo_events", "created_at"),
    ("alias_proposals", "am_alias_proposals", "created_at"),
    ("served", "gf_served", "created_at"),
    ("outcomes", "gf_outcomes", "created_at"),
    ("hints", "gf_hints", "updated_at"),
    ("anchors", "gf_anchor_edges", "created_at"),
    ("overlay_communities", "gf_communities", "created_at"),
    ("overlay_aliases", "gf_alias_proposals", "created_at"),
    // The export index the incremental link pass keeps; the next pass
    // rebuilds it from the graph.
    ("cross_link_state", "gf_cross_link_state", "created_at"),
];

/// What one forget request covers: every document of a connector, or named
/// documents of it. Source ids are `connector:<connector>:<document>`,
/// passage identities append `:<byte offset>`, and dependency-index ids of
/// derived artifacts prefix `<repository>:`.
#[derive(Clone, Debug)]
pub struct Scope {
    connector: String,
    documents: Option<BTreeSet<String>>,
}

impl Scope {
    pub fn connector(connector: &str) -> Self {
        Self {
            connector: connector.to_string(),
            documents: None,
        }
    }
    pub fn documents(connector: &str, documents: &Value) -> Result<Self> {
        let listed = documents
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= MAX_DOCUMENTS)
            .ok_or_else(|| Error::bad("document_ids must list 1–1,000 connector document IDs"))?;
        let mut set = BTreeSet::new();
        for value in listed {
            let document = value
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 500)
                .ok_or_else(|| Error::bad("Each document ID must be 1–500 bytes"))?;
            set.insert(document.to_string());
        }
        Ok(Self {
            connector: connector.to_string(),
            documents: Some(set),
        })
    }
    /// Named documents of a connector chosen by the service itself (a
    /// retention plan); callers bound the set to [`MAX_DOCUMENTS`].
    pub fn document_set(connector: &str, documents: BTreeSet<String>) -> Self {
        Self {
            connector: connector.to_string(),
            documents: Some(documents),
        }
    }
    fn whole(&self) -> bool {
        self.documents.is_none()
    }
    fn prefix(&self) -> String {
        format!("connector:{}:", self.connector)
    }
    /// A bare connector document id.
    pub fn document(&self, id: &str) -> bool {
        self.documents.as_ref().is_none_or(|d| d.contains(id))
    }
    /// A source id `connector:<connector>:<document>`.
    pub fn source(&self, id: &str) -> bool {
        id.strip_prefix(&self.prefix())
            .is_some_and(|document| self.document(document))
    }
    /// A passage identity `<source id>:<byte offset>`.
    pub fn passage(&self, id: &str) -> bool {
        id.rsplit_once(':').is_some_and(|(head, tail)| {
            !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) && self.source(head)
        })
    }
    fn sources(&self) -> Vec<String> {
        self.documents
            .iter()
            .flatten()
            .map(|d| format!("{}{d}", self.prefix()))
            .collect()
    }
    fn hashes(&self) -> Vec<String> {
        self.documents
            .iter()
            .flatten()
            .map(|d| tombstone(&self.connector, d))
            .collect()
    }
    fn describe(&self) -> Value {
        json!({"kind":if self.whole() {"connector"} else {"documents"},"connector":self.connector,
            "documents":self.documents.as_ref().map(BTreeSet::len)})
    }
}

/// One-way name of a forgotten document: blocks re-import without keeping
/// its identifier.
fn tombstone(connector: &str, document: &str) -> String {
    digest(&json!({"connector":connector,"document":document}))
}

/// The one-way name receipts give a connector document.
pub fn document_hash(connector: &str, document: &str) -> String {
    tombstone(connector, document)
}

/// Most documents one forget request (and one receipt) names.
pub const MAX_SCOPE_DOCUMENTS: usize = MAX_DOCUMENTS;

/// Whether an incoming connector document was forgotten earlier.
pub(crate) fn forgotten(db: &Connection, connector: &str, document: &str) -> Result<bool> {
    let session = crate::capture::document_session_marker(document)
        .map(|marker| tombstone(connector, &marker));
    Ok(one(
        db,
        "SELECT 1 AS yes FROM am_forgotten_sources WHERE connector_id=? AND id_hash IN (?,?) LIMIT 1",
        params![connector, tombstone(connector, document), session],
    )?
    .is_some())
}

/// Block a connector document id before it exists (the later turns of a
/// forgotten agent session) with the same one-way tombstone a forgotten
/// document leaves, so no identifier is kept.
pub(crate) fn block(db: &Connection, connector: &str, document: &str) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO am_forgotten_sources VALUES(?,?,?)",
        params![connector, tombstone(connector, document), now()],
    )?;
    Ok(())
}

struct Table {
    name: String,
    text: Vec<String>,
    columns: BTreeSet<String>,
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn count(db: &Connection, sql: &str, arguments: impl rusqlite::Params) -> Result<usize> {
    Ok(db.query_row(sql, arguments, |r| r.get::<_, i64>(0))? as usize)
}

/// Rowids of the rows a query returns (`SELECT rowid, value …`) whose value
/// satisfies `keep`.
fn select_rowids(
    db: &Connection,
    sql: &str,
    arguments: impl rusqlite::Params,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<i64>> {
    let mut statement = db.prepare(sql)?;
    let mut found = statement.query(arguments)?;
    let mut out = Vec::new();
    while let Some(row) = found.next()? {
        let value: Option<String> = row.get(1)?;
        if value.as_deref().is_some_and(&keep) {
            out.push(row.get(0)?);
        }
    }
    Ok(out)
}

fn delete_rowids(db: &Connection, table: &str, rowids: &[i64]) -> Result<usize> {
    for rowid in rowids {
        db.execute(&format!("DELETE FROM {table} WHERE rowid=?"), [rowid])?;
    }
    Ok(rowids.len())
}

/// Every ordinary table with its text columns. FTS indexes (derived from
/// repository files only) and the receipts themselves are excluded.
fn tables(db: &Connection) -> Result<Vec<Table>> {
    let all = rows(
        db,
        "SELECT name,coalesce(sql,'') AS sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        [],
    )?;
    let virtual_tables: Vec<&str> = all
        .iter()
        .filter(|t| {
            s(t, "sql")
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CREATE VIRTUAL TABLE")
        })
        .map(|t| s(t, "name"))
        .collect();
    let mut out = Vec::new();
    for table in &all {
        let name = s(table, "name");
        if name == RECEIPTS
            || virtual_tables
                .iter()
                .any(|v| name == *v || name.starts_with(&format!("{v}_")))
        {
            continue;
        }
        let mut entry = Table {
            name: name.to_string(),
            text: vec![],
            columns: BTreeSet::new(),
        };
        for column in rows(db, "SELECT name,type FROM pragma_table_info(?)", [name])? {
            let kind = s(&column, "type").to_ascii_uppercase();
            if kind.is_empty()
                || ["TEXT", "CHAR", "CLOB", "JSON"]
                    .iter()
                    .any(|k| kind.contains(k))
            {
                entry.text.push(s(&column, "name").to_string());
            }
            entry.columns.insert(s(&column, "name").to_string());
        }
        out.push(entry);
    }
    Ok(out)
}

/// The rows of `table` a scan may read: one repository, else one workspace.
fn filter<'a>(
    table: &Table,
    repository: &'a str,
    workspace: &'a str,
) -> Option<(&'static str, &'a str)> {
    if table.columns.contains("repository_id") {
        Some(("repository_id", repository))
    } else if table.columns.contains("workspace_id") {
        Some(("workspace_id", workspace))
    } else if table.columns.contains("workspace") {
        Some(("workspace", workspace))
    } else {
        None
    }
}

/// `SELECT … WHERE (instr(col, pattern) > 0 OR …) [AND scope]` for a table's
/// text columns.
fn instr_query(
    table: &Table,
    select: &str,
    patterns: &[String],
    filter: Option<(&str, &str)>,
) -> (String, Vec<String>) {
    let terms: Vec<String> = table
        .text
        .iter()
        .flat_map(|c| (1..=patterns.len()).map(move |i| format!("instr({},?{i})>0", quote(c))))
        .collect();
    let mut sql = format!(
        "SELECT {select} FROM {} WHERE ({})",
        quote(&table.name),
        terms.join(" OR ")
    );
    let mut values: Vec<String> = patterns.to_vec();
    if let Some((column, value)) = filter {
        sql.push_str(&format!(" AND {}=?{}", quote(column), patterns.len() + 1));
        values.push(value.to_string());
    }
    (sql, values)
}

/// Rows whose text columns contain any pattern.
fn matching(
    db: &Connection,
    table: &Table,
    patterns: &[String],
    filter: Option<(&str, &str)>,
) -> Result<usize> {
    if table.text.is_empty() || patterns.is_empty() {
        return Ok(0);
    }
    let (sql, values) = instr_query(table, "count(*)", patterns, filter);
    count(db, &sql, params_from_iter(values))
}

/// Exact references to deleted ids in every table in scope. Tables holding
/// connectors' originals are checked structurally instead.
fn references(
    db: &Connection,
    tables: &[Table],
    patterns: &[String],
    (repository, workspace): (&str, &str),
    skip: &[&str],
    skip_repository_tables: bool,
) -> Result<Value> {
    let mut scanned = 0;
    let mut matches = Vec::new();
    for table in tables {
        if skip.contains(&table.name.as_str())
            || table.columns.contains("connector_id")
            || (skip_repository_tables && table.columns.contains("repository_id"))
        {
            continue;
        }
        scanned += 1;
        let rows = matching(db, table, patterns, filter(table, repository, workspace))?;
        if rows > 0 {
            matches.push(json!({"table":table.name,"rows":rows}));
        }
    }
    Ok(json!({"tables_scanned":scanned,"matches":matches}))
}

/// References to forgotten document ids. A substring hit counts unless it is
/// the start of a longer id of a document that still exists (`a` inside `a:b`).
fn document_references(
    db: &Connection,
    tables: &[Table],
    scope: &Scope,
    existing: &BTreeSet<String>,
    (repository, workspace): (&str, &str),
) -> Result<Value> {
    let ids = scope.sources();
    let mut scanned = 0;
    let mut matches = Vec::new();
    for table in tables {
        if table.columns.contains("connector_id") || table.text.is_empty() {
            continue;
        }
        scanned += 1;
        let joined = table
            .text
            .iter()
            .map(|c| format!("coalesce(CAST({} AS TEXT),'')", quote(c)))
            .collect::<Vec<_>>()
            .join("||char(10)||");
        let (sql, values) = instr_query(table, &joined, &ids, filter(table, repository, workspace));
        let mut statement = db.prepare(&sql)?;
        let mut found = statement.query(params_from_iter(values))?;
        let mut rows = 0;
        while let Some(row) = found.next()? {
            let text: String = row.get(0)?;
            if names_any(&text, &ids, existing) {
                rows += 1;
            }
        }
        if rows > 0 {
            matches.push(json!({"table":table.name,"rows":rows}));
        }
    }
    Ok(json!({"tables_scanned":scanned,"matches":matches}))
}

fn names_any(text: &str, ids: &[String], existing: &BTreeSet<String>) -> bool {
    ids.iter().any(|id| {
        text.match_indices(id.as_str()).any(|(at, _)| {
            !existing
                .range(id.clone()..)
                .take_while(|e| e.starts_with(id.as_str()))
                .any(|e| e.len() > id.len() && text[at..].starts_with(e.as_str()))
        })
    })
}

/// Verbatim excerpts of deleted text in this project's derived tables.
fn verbatim(
    db: &Connection,
    tables: &[Table],
    fingerprints: &[String],
    repository: &str,
) -> Result<Value> {
    let mut scanned = 0;
    let mut matches = Vec::new();
    for table in tables {
        if !table.columns.contains("repository_id")
            || table.columns.contains("connector_id")
            || INDEPENDENT.contains(&table.name.as_str())
        {
            continue;
        }
        scanned += 1;
        let rows = matching(db, table, fingerprints, Some(("repository_id", repository)))?;
        if rows > 0 {
            matches.push(json!({"table":table.name,"rows":rows}));
        }
    }
    Ok(json!({"fingerprints":fingerprints.len(),"tables_scanned":scanned,"matches":matches}))
}

/// A middle window of the longest run of characters that no JSON encoder
/// escapes, so a copy is found inside serialized columns too.
fn excerpt(content: &str) -> Option<String> {
    let safe = |c: char| c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\';
    let mut best = (0, 0);
    let mut start = None;
    for (i, c) in content.char_indices().chain([(content.len(), '\n')]) {
        match (safe(c), start) {
            (true, None) => start = Some(i),
            (false, Some(a)) => {
                if i - a > best.1 - best.0 {
                    best = (a, i);
                }
                start = None;
            }
            _ => {}
        }
    }
    let run = content[best.0..best.1].trim();
    let offset = run.len().saturating_sub(FINGERPRINT_BYTES) / 2;
    let window = run[offset..(offset + FINGERPRINT_BYTES).min(run.len())].trim();
    (window.len() >= MIN_FINGERPRINT_BYTES
        && window.chars().filter(char::is_ascii_alphanumeric).count() >= 8)
        .then(|| window.to_string())
}

fn secure_delete(db: &Connection) -> Result<bool> {
    Ok(db.query_row("PRAGMA secure_delete", [], |r| r.get::<_, i64>(0))? == 1)
}

fn external(db: &Connection, repository: &str) -> Result<Vec<Value>> {
    rows(
        db,
        "SELECT id,kind,revision,state FROM am_projection_cleanup WHERE scope=? ORDER BY kind,id",
        [repository],
    )
}

/// A captured external cleanup is done once its row completed at (or after)
/// the captured revision; revisions only grow.
fn external_now(db: &Connection, captured: &Value) -> Result<(Vec<Value>, bool)> {
    let mut pending = false;
    let mut out = Vec::new();
    for cleanup in captured.as_array().into_iter().flatten() {
        let row = one(
            db,
            "SELECT revision,state,error FROM am_projection_cleanup WHERE id=?",
            [s(cleanup, "id")],
        )?;
        let done = row.as_ref().is_some_and(|r| {
            r["state"] == "complete" && r["revision"].as_i64() >= cleanup["revision"].as_i64()
        });
        pending |= !done;
        out.push(json!({"id":cleanup["id"],"kind":cleanup["kind"],"captured_revision":cleanup["revision"],
            "state":row.as_ref().map_or(json!("unknown"),|r|r["state"].clone()),"error":row.map_or(Value::Null,|r|r["error"].clone()),"complete":done}));
    }
    Ok((out, pending))
}

fn empty(scan: &Value) -> bool {
    scan["matches"].as_array().is_some_and(Vec::is_empty)
}

fn cites(data: &str, scope: &Scope) -> bool {
    serde_json::from_str::<Value>(data).is_ok_and(|d| {
        d["native_sources"]
            .as_object()
            .is_some_and(|m| m.keys().any(|k| scope.source(k)))
    })
}

fn lists(list: &str, scope: &Scope) -> bool {
    serde_json::from_str::<Vec<Value>>(list).is_ok_and(|v| {
        v.iter()
            .any(|x| x.as_str().is_some_and(|i| scope.source(i)))
    })
}

/// A dependency-index id `<repository>:<…>` of an artifact derived from a
/// forgotten document: its observer note or one of its passage embeddings.
fn derived(repository: &str, scope: &Scope, kind: &str, id: &str) -> bool {
    let Some(rest) = id
        .strip_prefix(repository)
        .and_then(|r| r.strip_prefix(':'))
    else {
        return false;
    };
    match kind {
        crate::observer::NOTE => scope.source(rest),
        crate::invalidation::EMBEDDING => scope.passage(rest),
        _ => false,
    }
}

fn index_edges(
    db: &Connection,
    workspace: &str,
    repository: &str,
    scope: &Scope,
) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for row in rows(
        db,
        "SELECT rowid,dependent_kind,dependent_id,source_kind,source_id FROM gf_dependencies WHERE workspace=?",
        [workspace],
    )? {
        if (row["source_kind"] == crate::invalidation::DOCUMENT
            && scope.source(s(&row, "source_id")))
            || derived(
                repository,
                scope,
                s(&row, "dependent_kind"),
                s(&row, "dependent_id"),
            )
        {
            out.push(row["rowid"].as_i64().unwrap_or_default());
        }
    }
    Ok(out)
}

fn index_markers(
    db: &Connection,
    workspace: &str,
    repository: &str,
    scope: &Scope,
) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for row in rows(
        db,
        "SELECT rowid,kind,id FROM gf_stale WHERE workspace=?",
        [workspace],
    )? {
        if derived(repository, scope, s(&row, "kind"), s(&row, "id")) {
            out.push(row["rowid"].as_i64().unwrap_or_default());
        }
    }
    Ok(out)
}

/// Rows derived from or naming the forgotten documents, by kind.
fn source_rows(
    db: &Connection,
    workspace: &str,
    repository: &str,
    scope: &Scope,
) -> Result<BTreeMap<&'static str, usize>> {
    let connector = scope.connector.as_str();
    let mut out = BTreeMap::new();
    if scope.whole() {
        out.insert(
            "connector",
            count(
                db,
                "SELECT count(*) FROM am_connectors WHERE id=?",
                [connector],
            )?,
        );
    }
    for (name, table, column) in [
        ("documents", "am_connector_docs", "id"),
        ("versions", "am_connector_versions", "id"),
        ("staged", "am_connector_stage", "id"),
        ("permissions", "am_source_permissions", "source_id"),
    ] {
        let found = select_rowids(
            db,
            &format!("SELECT rowid,{column} FROM {table} WHERE connector_id=?"),
            [connector],
            |v| scope.document(v),
        )?;
        out.insert(name, found.len());
    }
    let per_repository: [(&'static str, &str, Matches); 12] = [
        (
            "originals",
            "SELECT rowid,source_id FROM am_original_documents WHERE repository_id=?",
            |v, sc| sc.source(v),
        ),
        (
            "memories",
            "SELECT rowid,data FROM repo_memories WHERE repository_id=?",
            cites,
        ),
        (
            "observer_notes",
            "SELECT rowid,source_id FROM am_observer_notes WHERE repository_id=?",
            |v, sc| sc.source(v),
        ),
        (
            "observer_reflections",
            "SELECT rowid,sources FROM am_observer_reflections WHERE repository_id=?",
            lists,
        ),
        (
            "vectors",
            "SELECT rowid,identity FROM am_vectors WHERE repository_id=?",
            |v, sc| sc.passage(v),
        ),
        (
            "document_reads",
            "SELECT rowid,source_id FROM am_document_reads WHERE repository_id=?",
            |v, sc| sc.source(v),
        ),
        (
            "anchors",
            "SELECT rowid,json_extract(attributes,'$.document_id') FROM gf_anchor_edges WHERE repository_id=?",
            |v, sc| sc.source(v),
        ),
        (
            "hints",
            "SELECT rowid,target FROM gf_hints WHERE repository_id=?",
            |v, sc| sc.source(v),
        ),
        // A forgotten document's mention links and what they
        // were computed from.
        (
            "doc_links",
            "SELECT rowid,document_id FROM gf_doc_links WHERE repository_id=?",
            |v, sc| sc.source(v),
        ),
        (
            "doc_link_sources",
            "SELECT rowid,document_id FROM gf_doc_link_sources WHERE repository_id=?",
            |v, sc| sc.source(v),
        ),
        (
            "served",
            "SELECT rowid,served FROM gf_served WHERE repository_id=?",
            lists,
        ),
        (
            "outcomes",
            "SELECT rowid,targets FROM gf_outcomes WHERE repository_id=?",
            lists,
        ),
    ];
    for (name, sql, keep) in per_repository {
        let found = select_rowids(db, sql, [repository], |v| keep(v, scope))?;
        out.insert(name, found.len());
    }
    out.insert(
        "dependency_edges",
        index_edges(db, workspace, repository, scope)?.len(),
    );
    out.insert(
        "stale_markers",
        index_markers(db, workspace, repository, scope)?.len(),
    );
    Ok(out)
}

/// Rows of the artifacts listed in [`SINCE_VISIBLE`] dated from `since` (and
/// before `until`, when given).
fn since_visible(
    db: &Connection,
    repository: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<BTreeMap<&'static str, usize>> {
    let mut out = BTreeMap::new();
    for (name, table, column) in SINCE_VISIBLE {
        let n = match since {
            None => 0,
            Some(since) => count(
                db,
                &format!(
                    "SELECT count(*) FROM {table} WHERE repository_id=?1 AND {column}>=?2 AND (?3 IS NULL OR {column}<?3)"
                ),
                params![repository, since, until],
            )?,
        };
        out.insert(*name, n);
    }
    Ok(out)
}

/// What a forget request must cover, captured before anything is removed.
pub struct Plan {
    scope: Scope,
    since: Option<String>,
    sources: usize,
    sources_digest: String,
    texts: usize,
    fingerprints: Vec<String>,
    rows: BTreeMap<&'static str, usize>,
    visible_since: BTreeMap<&'static str, usize>,
}

impl Plan {
    /// When the forgotten documents first became visible (their first
    /// imported version); `None` if they never were.
    pub fn since(&self) -> Option<&str> {
        self.since.as_deref()
    }
    pub fn sources(&self) -> usize {
        self.sources
    }
    /// Rows forgetting would remove, per table (originals and single-source artifacts).
    pub fn rows(&self) -> &BTreeMap<&'static str, usize> {
        &self.rows
    }
    /// Derived rows computed while the documents were visible, per kind.
    pub fn visible_since(&self) -> &BTreeMap<&'static str, usize> {
        &self.visible_since
    }
}

pub fn plan(db: &Connection, workspace: &str, repository: &str, scope: &Scope) -> Result<Plan> {
    let prefix = scope.prefix();
    let mut sources = BTreeSet::new();
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    for table in [
        "am_connector_docs",
        "am_connector_versions",
        "am_connector_stage",
    ] {
        for row in rows(
            db,
            &format!("SELECT id,sha256,content FROM {table} WHERE connector_id=?"),
            [&scope.connector],
        )? {
            if !scope.document(s(&row, "id")) {
                continue;
            }
            sources.insert((
                format!("{prefix}{}", s(&row, "id")),
                s(&row, "sha256").to_string(),
            ));
            texts
                .entry(s(&row, "sha256").to_string())
                .or_insert_with(|| s(&row, "content").to_string());
        }
    }
    for row in rows(
        db,
        "SELECT source_id,sha256 FROM am_original_documents WHERE repository_id=?",
        [repository],
    )? {
        if scope.source(s(&row, "source_id")) {
            sources.insert((
                s(&row, "source_id").to_string(),
                s(&row, "sha256").to_string(),
            ));
        }
    }
    let mut since: Option<String> = None;
    for row in rows(
        db,
        "SELECT id,created_at FROM am_connector_versions WHERE connector_id=?",
        [&scope.connector],
    )? {
        let stamp = s(&row, "created_at");
        if scope.document(s(&row, "id")) && since.as_deref().is_none_or(|first| stamp < first) {
            since = Some(stamp.to_string());
        }
    }
    // Content hashes order the sample, so it is deterministic but unbiased.
    let fingerprints: Vec<String> = texts
        .values()
        .filter_map(|t| excerpt(t))
        .take(FINGERPRINTS)
        .collect();
    Ok(Plan {
        scope: scope.clone(),
        sources: sources.len(),
        sources_digest: digest(&json!(sources)),
        texts: texts.len(),
        fingerprints,
        rows: source_rows(db, workspace, repository, scope)?,
        visible_since: since_visible(db, repository, since.as_deref(), None)?,
        since,
    })
}

/// A model draft written while the forgotten documents were visible, whatever
/// sources it cites. An agent's unreviewed proposal (`memory.propose`) counts
/// as a model draft: the agent may have read the forgotten documents through
/// recall. (Single-source drafts of other sources that compact extraction
/// wrote are kept; compact extraction is not supported, unit
/// `compact-extraction`.)
fn drafted_while_visible(record: &Value, since: &str) -> bool {
    s(record, "created_at") >= since
        && !["reviewed", "retired"].contains(&s(record, "state"))
        && ["unreviewed_model_interpretation", "agent_proposed_draft"]
            .contains(&s(&record["data"], "authority"))
}

/// Memories citing the documents in any revision, model drafts written while
/// they were visible, and memories that depend on either.
fn forget_memories(
    db: &Connection,
    workspace: &str,
    repository: &str,
    scope: &Scope,
    since: Option<&str>,
) -> Result<BTreeSet<String>> {
    use crate::invalidation::{self, EMBEDDING, MEMORY};
    let mut records = rows(
        db,
        "SELECT * FROM repo_memories WHERE repository_id=?",
        [repository],
    )?;
    let mut removed = BTreeSet::new();
    let direct = |data: &Value| {
        data["native_sources"]
            .as_object()
            .is_some_and(|d| d.keys().any(|k| scope.source(k)))
    };
    for record in &mut records {
        decode(record, "data")?;
        let identifier = s(record, "id").to_string();
        if direct(&record["data"]) || since.is_some_and(|t| drafted_while_visible(record, t)) {
            removed.insert(identifier.clone());
        }
        for revision in rows(
            db,
            "SELECT data FROM repo_memory_revisions WHERE memory_id=?",
            [&identifier],
        )? {
            let mut prior: Value = serde_json::from_str(s(&revision, "data"))?;
            decode(&mut prior, "data")?;
            if direct(&prior["data"]) {
                removed.insert(identifier.clone());
            }
        }
    }
    loop {
        let before = removed.len();
        for record in &records {
            let data = &record["data"];
            if data["notes"]
                .as_object()
                .is_some_and(|d| d.keys().any(|k| removed.contains(k)))
                || removed.contains(s(&data["historical_target"], "memory_id"))
            {
                removed.insert(s(record, "id").to_string());
            }
        }
        if removed.len() == before {
            break;
        }
    }
    for identifier in &removed {
        db.execute(
            "DELETE FROM repo_memories WHERE repository_id=? AND id=?",
            params![repository, identifier],
        )?;
        invalidation::forget_dependent(db, workspace, (MEMORY, identifier))?;
        invalidation::clear_stale(db, workspace, MEMORY, identifier)?;
        let passage = format!("memory:{identifier}");
        db.execute(
            "DELETE FROM am_vectors WHERE repository_id=? AND identity=?",
            params![repository, passage],
        )?;
        let key = format!("{repository}:{passage}");
        invalidation::forget_dependent(db, workspace, (EMBEDDING, &key))?;
        invalidation::clear_stale(db, workspace, EMBEDDING, &key)?;
    }
    Ok(removed)
}

/// Derived rows that may name a memory by id: served-evidence records,
/// outcomes, hints and caches. [`delete_memories`] removes the rows naming a
/// deleted memory; the rest of each table stays.
const NAMES_MEMORIES: &[&str] = &[
    "gf_served",
    "gf_outcomes",
    "gf_hints",
    "am_model_cache",
    "am_query_cache",
];

/// Delete `memory_id` and every visible memory that depends on it — notes
/// naming it, historical versions targeting it, transitively — with their
/// revisions and claims (cascaded), vectors, dependency-index entries and
/// the derived rows ([`NAMES_MEMORIES`]) that name them. Returns the removed
/// ids and per-table counts; the caller signs [`rows_receipt`].
pub fn delete_memories(
    db: &Connection,
    workspace: &str,
    repository: &str,
    memories: &[Value],
    memory_id: &str,
) -> Result<(BTreeSet<String>, Value)> {
    use crate::invalidation::{self, EMBEDDING, MEMORY};
    let mut removed = BTreeSet::from([memory_id.to_string()]);
    loop {
        let before = removed.len();
        for record in memories {
            let data = &record["data"];
            if data["notes"]
                .as_object()
                .is_some_and(|d| d.keys().any(|k| removed.contains(k)))
                || removed.contains(s(&data["historical_target"], "memory_id"))
            {
                removed.insert(s(record, "id").to_string());
            }
        }
        if removed.len() == before {
            break;
        }
    }
    let keys: Vec<String> = removed.iter().cloned().collect();
    let mut counts = BTreeMap::new();
    let mut revisions = 0usize;
    let mut claims = 0usize;
    let mut vectors = 0usize;
    let mut pins = 0usize;
    for identifier in &removed {
        revisions += count(
            db,
            "SELECT count(*) FROM repo_memory_revisions WHERE memory_id=?",
            [identifier],
        )?;
        // The memory's pinned or pending quotes go with it.
        pins += count(
            db,
            "SELECT count(*) FROM gf_symbol_pins WHERE repository_id=? AND memory_id=?",
            params![repository, identifier],
        )?;
        claims += count(
            db,
            "SELECT count(*) FROM am_claims WHERE repository_id=? AND memory_id=?",
            params![repository, identifier],
        )?;
        db.execute(
            "DELETE FROM repo_memories WHERE repository_id=? AND id=?",
            params![repository, identifier],
        )?;
        invalidation::forget_dependent(db, workspace, (MEMORY, identifier))?;
        invalidation::clear_stale(db, workspace, MEMORY, identifier)?;
        let passage = format!("memory:{identifier}");
        vectors += db.execute(
            "DELETE FROM am_vectors WHERE repository_id=? AND identity=?",
            params![repository, passage],
        )?;
        let key = format!("{repository}:{passage}");
        invalidation::forget_dependent(db, workspace, (EMBEDDING, &key))?;
        invalidation::clear_stale(db, workspace, EMBEDDING, &key)?;
    }
    counts.insert("repo_memories".to_string(), removed.len());
    counts.insert("repo_memory_revisions".to_string(), revisions);
    counts.insert("am_claims".to_string(), claims);
    counts.insert("am_vectors".to_string(), vectors);
    if pins > 0 {
        counts.insert("gf_symbol_pins".to_string(), pins);
    }
    let catalog = tables(db)?;
    for name in NAMES_MEMORIES {
        let Some(table) = catalog.iter().find(|t| t.name == *name) else {
            continue;
        };
        if table.text.is_empty() {
            continue;
        }
        let (sql, values) =
            instr_query(table, "rowid", &keys, filter(table, repository, workspace));
        let rowids: Vec<i64> = {
            let mut statement = db.prepare(&sql)?;
            let found = statement.query_map(params_from_iter(values), |r| r.get::<_, i64>(0))?;
            found.collect::<std::result::Result<_, _>>()?
        };
        let deleted = delete_rowids(db, &quote(name), &rowids)?;
        if deleted > 0 {
            counts.insert(name.to_string(), deleted);
        }
    }
    Ok((removed, json!(counts)))
}

/// Rows still holding any of the deleted memories, per table (all zero after
/// [`delete_memories`]).
pub fn memory_rows(db: &Connection, repository: &str, ids: &[String]) -> Result<Value> {
    let mut out = BTreeMap::new();
    for (table, sql) in [
        (
            "repo_memories",
            "SELECT count(*) FROM repo_memories WHERE repository_id=?1 AND id=?2",
        ),
        (
            "repo_memory_revisions",
            "SELECT count(*) FROM repo_memory_revisions WHERE memory_id=?2",
        ),
        (
            "am_claims",
            "SELECT count(*) FROM am_claims WHERE repository_id=?1 AND memory_id=?2",
        ),
        (
            "am_vectors",
            "SELECT count(*) FROM am_vectors WHERE repository_id=?1 AND identity='memory:'||?2",
        ),
        (
            "gf_symbol_pins",
            "SELECT count(*) FROM gf_symbol_pins WHERE repository_id=?1 AND memory_id=?2",
        ),
    ] {
        let mut total = 0;
        for id in ids {
            total += count(db, sql, params![repository, id])?;
        }
        // Listed only when a deleted memory's pin remains.
        if table == "gf_symbol_pins" && total == 0 {
            continue;
        }
        out.insert(table.to_string(), total);
    }
    Ok(json!(out))
}

/// Remove every derived artifact that could carry the documents' information
/// (see the module documentation). Originals are removed by [`erase`].
pub fn forget(
    db: &Connection,
    workspace: &str,
    repository: &str,
    scope: &Scope,
    since: Option<&str>,
) -> Result<Value> {
    use crate::invalidation;
    let claims = || -> Result<BTreeSet<String>> {
        Ok(rows(
            db,
            "SELECT id FROM am_claims WHERE repository_id=?",
            [repository],
        )?
        .iter()
        .map(|r| s(r, "id").to_string())
        .collect())
    };
    let claims_before = claims()?;
    let pins = || {
        count(
            db,
            "SELECT count(*) FROM gf_symbol_pins WHERE repository_id=?",
            [repository],
        )
    };
    let pins_before = pins()?;
    let memories = forget_memories(db, workspace, repository, scope, since)?;
    // The forgotten memories' symbol pins went with them.
    let symbol_pins = pins_before.saturating_sub(pins()?);
    let lost: Vec<String> = claims_before
        .difference(&claims()?)
        .cloned()
        .chain(memories.iter().cloned())
        .collect();
    // A reflection consumed many sources; any forgotten one removes it.
    let reflections = select_rowids(
        db,
        "SELECT rowid,sources FROM am_observer_reflections WHERE repository_id=?",
        [repository],
        |v| lists(v, scope),
    )?;
    let reflections = delete_rowids(db, "am_observer_reflections", &reflections)?;
    // Single-source artifacts of the forgotten documents.
    let mut notes = 0;
    for row in rows(
        db,
        "SELECT source_id FROM am_observer_notes WHERE repository_id=?",
        [repository],
    )? {
        if scope.source(s(&row, "source_id")) {
            notes +=
                crate::observer::forget_source(db, workspace, repository, s(&row, "source_id"))?;
        }
    }
    let vectors = select_rowids(
        db,
        "SELECT rowid,identity FROM am_vectors WHERE repository_id=?",
        [repository],
        |v| scope.passage(v),
    )?;
    let vectors = delete_rowids(db, "am_vectors", &vectors)?;
    // An extraction read whose drafts were just removed must run again.
    let reads = select_rowids(
        db,
        "SELECT rowid,source_id||char(10)||claim_ids FROM am_document_reads WHERE repository_id=?",
        [repository],
        |v| {
            let (source, claims) = v.split_once('\n').unwrap_or((v, ""));
            scope.source(source) || lost.iter().any(|id| claims.contains(id.as_str()))
        },
    )?;
    let reads = delete_rowids(db, "am_document_reads", &reads)?;
    // Artifacts computed while the documents were visible.
    let mut visible_since = BTreeMap::new();
    if let Some(since) = since {
        // Cached answers: their index entries go with them.
        for row in rows(
            db,
            "SELECT dependent_id FROM gf_dependencies WHERE workspace=?1 AND dependent_kind=?2 AND substr(dependent_id,1,length(?3))=?3 AND recorded_at>=?4",
            params![
                workspace,
                invalidation::ANSWER,
                format!("{repository}:"),
                since
            ],
        )? {
            let key = s(&row, "dependent_id");
            invalidation::forget_dependent(db, workspace, (invalidation::ANSWER, key))?;
            invalidation::clear_stale(db, workspace, invalidation::ANSWER, key)?;
        }
        for (kind, table) in [
            (invalidation::REPORT, "am_reports"),
            (invalidation::COMMUNITY, "am_communities"),
        ] {
            for row in rows(
                db,
                &format!("SELECT id FROM {table} WHERE repository_id=? AND created_at>=?"),
                params![repository, since],
            )? {
                let key = format!("{repository}:{}", s(&row, "id"));
                invalidation::forget_dependent(db, workspace, (kind, &key))?;
                invalidation::clear_stale(db, workspace, kind, &key)?;
            }
        }
        for row in rows(
            db,
            "SELECT chunk_id FROM gf_anchor_edges WHERE repository_id=? AND created_at>=?",
            params![repository, since],
        )? {
            let key = invalidation::anchor_id(repository, s(&row, "chunk_id"));
            invalidation::forget_dependent(db, workspace, (invalidation::ANCHOR, &key))?;
            invalidation::clear_stale(db, workspace, invalidation::ANCHOR, &key)?;
        }
        for (name, table, column) in SINCE_VISIBLE {
            let n = db.execute(
                &format!("DELETE FROM {table} WHERE repository_id=? AND {column}>=?"),
                params![repository, since],
            )?;
            visible_since.insert(*name, n);
        }
    }
    // References in older rows (none unless written outside the normal flow).
    let mut anchors = 0;
    for row in rows(
        db,
        "SELECT rowid,chunk_id,json_extract(attributes,'$.document_id') AS document FROM gf_anchor_edges WHERE repository_id=?",
        [repository],
    )? {
        if scope.source(s(&row, "document")) {
            let key = invalidation::anchor_id(repository, s(&row, "chunk_id"));
            invalidation::forget_dependent(db, workspace, (invalidation::ANCHOR, &key))?;
            invalidation::clear_stale(db, workspace, invalidation::ANCHOR, &key)?;
            anchors += db.execute(
                "DELETE FROM gf_anchor_edges WHERE rowid=?",
                [row["rowid"].as_i64()],
            )?;
        }
    }
    let hints = select_rowids(
        db,
        "SELECT rowid,target FROM gf_hints WHERE repository_id=?",
        [repository],
        |v| scope.source(v),
    )?;
    let hints = delete_rowids(db, "gf_hints", &hints)?;
    // The documents' mention links and their inputs.
    let mut doc_links = 0;
    for table in ["gf_doc_links", "gf_doc_link_sources"] {
        let found = select_rowids(
            db,
            &format!("SELECT rowid,document_id FROM {table} WHERE repository_id=?"),
            [repository],
            |v| scope.source(v),
        )?;
        doc_links += delete_rowids(db, table, &found)?;
    }
    let mut stripped = BTreeMap::new();
    for (table, column) in [("gf_served", "served"), ("gf_outcomes", "targets")] {
        let mut changed = 0;
        for row in rows(
            db,
            &format!("SELECT rowid,{column} AS list FROM {table} WHERE repository_id=?"),
            [repository],
        )? {
            let list: Vec<Value> = serde_json::from_str(s(&row, "list")).unwrap_or_default();
            let kept: Vec<&Value> = list
                .iter()
                .filter(|v| !v.as_str().is_some_and(|i| scope.source(i)))
                .collect();
            if kept.len() == list.len() {
                continue;
            }
            changed += 1;
            if kept.is_empty() {
                db.execute(
                    &format!("DELETE FROM {table} WHERE rowid=?"),
                    [row["rowid"].as_i64()],
                )?;
            } else {
                db.execute(
                    &format!("UPDATE {table} SET {column}=? WHERE rowid=?"),
                    params![json!(kept).to_string(), row["rowid"].as_i64()],
                )?;
            }
        }
        stripped.insert(table, changed);
    }
    // The dependency index follows the artifacts.
    let edges = delete_rowids(
        db,
        "gf_dependencies",
        &index_edges(db, workspace, repository, scope)?,
    )?;
    let markers = delete_rowids(
        db,
        "gf_stale",
        &index_markers(db, workspace, repository, scope)?,
    )?;
    db.execute("DELETE FROM am_entities WHERE repository_id=? AND id NOT IN (SELECT subject_id FROM am_entity_links UNION SELECT object_id FROM am_entity_links)",[repository])?;
    crate::store::invalidate(db, repository)?;
    let mut removed = json!({"memories":memories.len(),"observer_notes":notes,"observer_reflections":reflections,"vectors":vectors,"document_reads":reads,
        "since_visible":visible_since,"anchors":anchors,"hints":hints,"doc_links":doc_links,"served_records":stripped["gf_served"],
        "outcome_records":stripped["gf_outcomes"],"dependency_edges":edges,"stale_markers":markers});
    if symbol_pins > 0 {
        removed["symbol_pins"] = json!(symbol_pins);
    }
    Ok(removed)
}

/// Remove the originals in scope. Forgotten documents keep a one-way
/// tombstone so a later sync or import skips them.
pub fn erase(db: &Connection, repository: &str, scope: &Scope) -> Result<Value> {
    let connector = scope.connector.as_str();
    if scope.whole() {
        let n = db.execute("DELETE FROM am_connectors WHERE id=?", [connector])?;
        return Ok(json!({"connector":n}));
    }
    let mut out = BTreeMap::new();
    for (name, table, column) in [
        ("documents", "am_connector_docs", "id"),
        ("versions", "am_connector_versions", "id"),
        ("staged", "am_connector_stage", "id"),
        ("permissions", "am_source_permissions", "source_id"),
    ] {
        let found = select_rowids(
            db,
            &format!("SELECT rowid,{column} FROM {table} WHERE connector_id=?"),
            [connector],
            |v| scope.document(v),
        )?;
        out.insert(name, delete_rowids(db, table, &found)?);
    }
    let originals = select_rowids(
        db,
        "SELECT rowid,source_id FROM am_original_documents WHERE repository_id=?",
        [repository],
        |v| scope.source(v),
    )?;
    out.insert(
        "originals",
        delete_rowids(db, "am_original_documents", &originals)?,
    );
    let stamp = now();
    for hash in scope.hashes() {
        db.execute(
            "INSERT OR IGNORE INTO am_forgotten_sources VALUES(?,?,?)",
            params![connector, hash, stamp],
        )?;
    }
    Ok(json!(out))
}

/// Verify a forget request inside its transaction and store the receipt.
/// `started` is when the request began; artifacts dated after it are new work.
#[allow(clippy::too_many_arguments)]
pub fn receipt(
    db: &Connection,
    p: &Principal,
    repository: &Value,
    operation: &str,
    plan: Plan,
    started: &str,
    forgotten: Value,
    erased: Value,
) -> Result<Value> {
    policy_receipt(
        db, p, repository, operation, plan, started, forgotten, erased, None,
    )
}

/// [`receipt`] for a deletion a retention policy chose: the receipt also
/// carries the policy and its digest.
#[allow(clippy::too_many_arguments)]
pub fn policy_receipt(
    db: &Connection,
    p: &Principal,
    repository: &Value,
    operation: &str,
    plan: Plan,
    started: &str,
    forgotten: Value,
    erased: Value,
    policy: Option<&Value>,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let scope = &plan.scope;
    let remaining = source_rows(db, &p.workspace, identifier, scope)?;
    let tainted = since_visible(db, identifier, plan.since(), Some(started))?;
    let catalog = tables(db)?;
    let ids = if scope.whole() {
        references(
            db,
            &catalog,
            &[scope.prefix()],
            (identifier, &p.workspace),
            &[],
            false,
        )?
    } else {
        let existing: BTreeSet<String> = rows(
            db,
            "SELECT id FROM am_connector_docs WHERE connector_id=?",
            [&scope.connector],
        )?
        .iter()
        .map(|r| format!("{}{}", scope.prefix(), s(r, "id")))
        .collect();
        document_references(db, &catalog, scope, &existing, (identifier, &p.workspace))?
    };
    let text = verbatim(db, &catalog, &plan.fingerprints, identifier)?;
    let visible = crate::memory::documents(db, repository, p)?
        .iter()
        .filter(|d| scope.source(s(d, "id")))
        .count();
    let snapshot = state(db, &p.workspace, identifier)?;
    let cleanups = external(db, identifier)?;
    let clean = remaining.values().chain(tainted.values()).all(|n| *n == 0)
        && empty(&ids)
        && empty(&text)
        && visible == 0;
    let pending = cleanups.iter().any(|c| c["state"] != "complete");
    let recompute: Vec<&str> = plan
        .visible_since
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, _)| *k)
        .collect();
    let mut receipt = json!({
        "contract":CONTRACT,"operation":operation,"workspace_id":p.workspace,
        "repository_id":identifier,"target":scope.connector,"scope":scope.describe(),
        "requested_by":p.credential,"deleted_at":now(),
        "sources":{"count":plan.sources,"digest":plan.sources_digest,"identifiers_retained":false,
            "document_hashes":scope.hashes()},
        "removed":{"rows":plan.rows,"since_visible":plan.visible_since,"derived":forgotten,"originals":erased},
        "remaining":{"rows":remaining,"since_visible":tainted},
        "recomputed_on_demand":recompute,
        "residue":{"source_ids":ids,"verbatim_text":json!({"texts":plan.texts,"sampled":text["fingerprints"],
            "tables_scanned":text["tables_scanned"],"matches":text["matches"]})},
        "serving_probe":{"visible_documents":visible},
        "state":{"contract":STATE_CONTRACT,"digest":snapshot["digest"],
            "tables":snapshot["tables"].as_object().map_or(0, |t| t.len())},
        "reimport_blocked":!scope.whole(),
        "external_cleanup":cleanups,
        "verified_local":clean,"external_pending":pending,"complete":clean && !pending,
        "not_covered":NOT_COVERED,"storage":{"secure_delete":secure_delete(db)?}
    });
    if let Some(policy) = policy {
        receipt["policy"] = policy.clone();
        receipt["policy_digest"] = json!(digest(policy));
    }
    append(
        db,
        &p.workspace,
        identifier,
        operation,
        &scope.connector,
        receipt,
    )
}

/// One-way name of a removed row key (a memory id, a pruned object).
pub fn key_hash(operation: &str, key: &str) -> String {
    digest(&json!({"operation":operation,"key":key}))
}

/// Receipt for a deletion of named rows rather than a source scope — a
/// memory (`memory.delete`), graph by-products past their retention
/// (`retention.graph`) or unreferenced storage (`storage.prune`). Removed
/// rows are counted per table; their keys are kept only as one-way hashes
/// (`removed.keys`). `remaining` is the caller's re-count of removed keys
/// still present, per table. With `scan`, every table is searched for the
/// keys themselves: matches in work history are reported as such, any other
/// match is residue and the receipt is not `verified_local`.
#[allow(clippy::too_many_arguments)]
pub fn rows_receipt(
    db: &Connection,
    p: &Principal,
    repository: &str,
    operation: &str,
    target: &str,
    policy: Value,
    removed: Value,
    keys: &[String],
    remaining: Value,
    scan: bool,
) -> Result<Value> {
    let mut hashes: Vec<String> = keys.iter().map(|k| key_hash(operation, k)).collect();
    hashes.sort();
    hashes.dedup();
    // Large prunes keep one digest over the hashes instead of the list.
    const LISTED_HASHES: usize = 1000;
    let hashes_digest = digest(&json!(hashes));
    let listed = if hashes.len() <= LISTED_HASHES {
        json!(hashes)
    } else {
        json!([])
    };
    let (residue, history) = if scan && !keys.is_empty() {
        let catalog = tables(db)?;
        let patterns: Vec<String> = keys.to_vec();
        let found = references(
            db,
            &catalog,
            &patterns,
            (repository, &p.workspace),
            &retained(db)?,
            false,
        )?;
        let history_tables = history(db)?;
        let (history, other): (Vec<Value>, Vec<Value>) = found["matches"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .partition(|m| history_tables.contains(&s(m, "table")));
        (
            json!({"tables_scanned":found["tables_scanned"],"matches":other}),
            json!(history),
        )
    } else {
        (json!({"tables_scanned":0,"matches":[]}), json!([]))
    };
    let clean = remaining
        .as_object()
        .is_none_or(|counts| counts.values().all(|n| n.as_u64() == Some(0)))
        && residue["matches"].as_array().is_none_or(Vec::is_empty);
    let receipt = json!({
        "contract":CONTRACT,"operation":operation,"workspace_id":p.workspace,
        "repository_id":repository,"target":target,"requested_by":p.credential,"deleted_at":now(),
        "policy":policy,"policy_digest":digest(&policy),
        "removed":{"rows":removed,"keys":{"count":hashes.len(),"hashes":listed,"hashes_digest":hashes_digest,
            "hashes_listed":hashes.len() <= LISTED_HASHES,"identifiers_retained":false}},
        "remaining":remaining,
        "residue":{"references":residue,"history_references":history},
        "verified_local":clean,"external_pending":false,"complete":clean,
        "not_covered":NOT_COVERED,"storage":{"secure_delete":secure_delete(db)?}
    });
    append(db, &p.workspace, repository, operation, target, receipt)
}

/// Replace identifiers minted per store and clock readings, so two stores
/// that hold the same content digest equally.
fn mask(value: &str) -> String {
    use std::sync::OnceLock;
    static IDS: OnceLock<regex::Regex> = OnceLock::new();
    static CLOCK: OnceLock<regex::Regex> = OnceLock::new();
    let ids = IDS.get_or_init(|| regex::Regex::new(r"\b[0-9a-f]{32}\b").unwrap());
    let clock = CLOCK.get_or_init(|| {
        regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})")
            .unwrap()
    });
    clock
        .replace_all(&ids.replace_all(value, "<id>"), "<time>")
        .into_owned()
}

/// Canonical digest of everything a store holds for one project: originals,
/// derived memory and indexes (`graphicle.state.v2`). History,
/// invalidation-timed caches and folder locations are excluded, and so are
/// tables with no rows in the project's scope; identifiers and capability
/// digests minted per store, clock readings and sync counters are masked. Two stores digest equally
/// when they hold the same content, whichever edition created them.
pub fn state(db: &Connection, workspace: &str, repository: &str) -> Result<Value> {
    let mut tables_out = BTreeMap::new();
    let history = history(db)?;
    let all = rows(
        db,
        "SELECT name,coalesce(sql,'') AS sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        [],
    )?;
    let virtual_tables: Vec<String> = all
        .iter()
        .filter(|t| {
            s(t, "sql")
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CREATE VIRTUAL TABLE")
        })
        .map(|t| s(t, "name").to_string())
        .collect();
    for table in &all {
        let name = s(table, "name");
        if name == RECEIPTS
            || history.contains(&name)
            || CACHES.contains(&name)
            || LOCATIONS.contains(&name)
            || virtual_tables
                .iter()
                .any(|v| name == v || name.starts_with(&format!("{v}_")))
        {
            continue;
        }
        let columns = rows(db, "SELECT name,type,pk FROM pragma_table_info(?)", [name])?;
        let has = |c: &str| columns.iter().any(|col| s(col, "name") == c);
        let cross: Vec<&str> = ["source_repository_id", "target_repository_id"]
            .into_iter()
            .filter(|c| has(c))
            .collect();
        let cross_scope = format!("?1 IN ({})", cross.join(","));
        let (scope, value) = if name == "repo_projects" {
            ("id=?", repository)
        } else if name == "workspaces" {
            ("id=?", workspace)
        } else if has("repository_id") {
            ("repository_id=?", repository)
        } else if !cross.is_empty() {
            (cross_scope.as_str(), repository)
        } else if has("connector_id") {
            (
                "connector_id IN (SELECT id FROM am_connectors WHERE repository_id=?)",
                repository,
            )
        } else if has("memory_id") {
            (
                "memory_id IN (SELECT id FROM repo_memories WHERE repository_id=?)",
                repository,
            )
        } else if has("workspace_id") {
            ("workspace_id=?", workspace)
        } else if has("workspace") {
            ("workspace=?", workspace)
        } else {
            continue;
        };
        let selected: Vec<String> = columns
            .iter()
            .filter(|c| {
                let rowid_alias = c["pk"] == 1 && s(c, "type").eq_ignore_ascii_case("INTEGER");
                // Request keys digest the whole snapshot, including per-store
                // memory ids; the rows' content is compared instead.
                !rowid_alias
                    && s(c, "name") != "request_hash"
                    && !MINTED_DIGESTS.contains(&s(c, "name"))
                    && !COUNTERS.contains(&(name, s(c, "name")))
            })
            .map(|c| s(c, "name").to_string())
            .collect();
        if selected.is_empty() {
            continue;
        }
        // The model cache's dependency-index entries belong to the cache.
        let cache_index = match name {
            "gf_dependencies" => selected.iter().position(|c| c == "dependent_kind"),
            "gf_stale" => selected.iter().position(|c| c == "kind"),
            _ => None,
        };
        let quoted: Vec<String> = selected.iter().map(|c| quote(c)).collect();
        let sql = format!(
            "SELECT {} FROM {} WHERE {scope}",
            quoted.join(","),
            quote(name)
        );
        let mut statement = db.prepare(&sql)?;
        let mut found = statement.query([value])?;
        let mut canonical = Vec::new();
        while let Some(row) = found.next()? {
            let mut values = Vec::with_capacity(selected.len());
            for i in 0..selected.len() {
                values.push(match row.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => json!(n),
                    ValueRef::Real(n) => json!(n),
                    ValueRef::Text(t) => json!(mask(&String::from_utf8_lossy(t))),
                    ValueRef::Blob(b) => {
                        json!(format!("blob:{}", graphicle_memory::text::sha256(b)))
                    }
                });
            }
            if cache_index.is_some_and(|i| values[i] == crate::invalidation::ANSWER) {
                continue;
            }
            canonical.push(Value::Array(values).to_string());
        }
        // An empty table says nothing about content (v2): whether a store has
        // an edition's tables at all must not change its digest.
        if canonical.is_empty() {
            continue;
        }
        canonical.sort();
        tables_out.insert(
            name.to_string(),
            json!({"rows":canonical.len(),"digest":graphicle_memory::text::sha256(canonical.join("\n").as_bytes())}),
        );
    }
    Ok(
        json!({"contract":STATE_CONTRACT,"digest":digest(&json!(tables_out)),"tables":tables_out,
        "excluded":{"history":history,"caches":CACHES,"locations":LOCATIONS,"empty_tables":true},
        "masked":["identifiers minted per store","capability digests minted per store","clock readings","sync counters"]}),
    )
}

/// `deletion.state`: the canonical digest of one project, for comparing a
/// store after forgetting with one rebuilt without the documents.
pub fn state_api(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    store.authorized_read(workspace, token, "admin", |db, p| {
        let project = repo(db, p, string(args, "repository_id", 100)?)?;
        state(db, &p.workspace, s(&project, "id"))
    })
}

/// Dependency-index entries and episode labels of a deleted project. They
/// are keyed by workspace, so the project's foreign-key cascade does not
/// reach them.
pub fn forget_project_references(
    db: &Connection,
    workspace: &str,
    repository: &str,
    connectors: &[String],
) -> Result<Value> {
    let mut edges = 0;
    let mut stale = 0;
    for prefix in project_patterns(repository, connectors) {
        edges += db.execute("DELETE FROM gf_dependencies WHERE workspace=?1 AND (substr(source_id,1,length(?2))=?2 OR substr(dependent_id,1,length(?2))=?2)", params![workspace, prefix])?;
        stale += db.execute(
            "DELETE FROM gf_stale WHERE workspace=?1 AND substr(id,1,length(?2))=?2",
            params![workspace, prefix],
        )?;
    }
    // The names (commit, snapshot) of its recorded episodes (every
    // projected project has them, and they named its id).
    let labels = db.execute(
        "DELETE FROM gf_episode_labels WHERE workspace=?1 AND repo=?2",
        params![workspace, repository],
    )?;
    Ok(json!({"dependency_edges":edges,"stale_markers":stale,"episode_labels":labels}))
}

fn project_patterns(repository: &str, connectors: &[String]) -> Vec<String> {
    std::iter::once(format!("{repository}:"))
        .chain(connectors.iter().map(|c| format!("connector:{c}:")))
        .collect()
}

/// Rows still keyed to a deleted project or to one of its connectors.
fn project_rows(
    db: &Connection,
    catalog: &[Table],
    repository: &str,
    connectors: &[String],
) -> Result<Value> {
    let mut checked = 0;
    let mut matches = Vec::new();
    let retained = retained(db)?;
    for table in catalog
        .iter()
        .filter(|t| !retained.contains(&t.name.as_str()))
    {
        for column in [
            "repository_id",
            "source_repository_id",
            "target_repository_id",
        ] {
            if table.columns.contains(column) {
                checked += 1;
                let n = count(
                    db,
                    &format!(
                        "SELECT count(*) FROM {} WHERE {}=?",
                        quote(&table.name),
                        quote(column)
                    ),
                    [repository],
                )?;
                if n > 0 {
                    matches.push(json!({"table":table.name,"column":column,"rows":n}));
                }
            }
        }
        if table.columns.contains("connector_id") {
            for connector in connectors {
                checked += 1;
                let n = count(
                    db,
                    &format!(
                        "SELECT count(*) FROM {} WHERE connector_id=?",
                        quote(&table.name)
                    ),
                    [connector],
                )?;
                if n > 0 {
                    matches.push(json!({"table":table.name,"column":"connector_id","rows":n}));
                }
            }
        }
    }
    Ok(json!({"columns_checked":checked,"matches":matches}))
}

/// The receipt of `project.delete` for `repository`. `removed` holds what
/// the deletion removed besides the project's own rows: `references`
/// (dependency-index entries) and, when the project had any,
/// `pr_scratch_projects` (the PR scratch projects deleted with it, each with
/// its own receipt).
pub fn project_receipt(
    db: &Connection,
    p: &Principal,
    repository: &str,
    connectors: &[String],
    removed: Value,
) -> Result<Value> {
    let catalog = tables(db)?;
    let remaining = project_rows(db, &catalog, repository, connectors)?;
    let patterns = std::iter::once(repository.to_string())
        .chain(connectors.iter().map(|c| format!("connector:{c}:")))
        .collect::<Vec<_>>();
    let retained = retained(db)?;
    let ids = references(
        db,
        &catalog,
        &patterns,
        (repository, &p.workspace),
        &retained,
        true,
    )?;
    let cleanups = external(db, repository)?;
    let clean = empty(&remaining) && empty(&ids);
    let pending = cleanups.iter().any(|c| c["state"] != "complete");
    let receipt = json!({
        "contract":CONTRACT,"operation":"project.delete","workspace_id":p.workspace,
        "repository_id":repository,"target":repository,"connectors":connectors,"requested_by":p.credential,"deleted_at":now(),
        "removed":removed,
        "remaining":{"project_rows":remaining},
        "residue":{"source_ids":ids,"retained_by_design":retained},
        "external_cleanup":cleanups,
        "verified_local":clean,"external_pending":pending,"complete":clean && !pending,
        "not_covered":NOT_COVERED,"storage":{"secure_delete":secure_delete(db)?}
    });
    append(
        db,
        &p.workspace,
        repository,
        "project.delete",
        repository,
        receipt,
    )
}

/// Link a receipt into the workspace chain and store it.
fn append(
    db: &Connection,
    workspace: &str,
    repository: &str,
    operation: &str,
    target: &str,
    mut receipt: Value,
) -> Result<Value> {
    let last = one(
        db,
        "SELECT sequence,digest FROM am_deletion_receipts WHERE workspace_id=? ORDER BY sequence DESC LIMIT 1",
        [workspace],
    )?;
    let sequence = last
        .as_ref()
        .and_then(|r| r["sequence"].as_i64())
        .unwrap_or(0)
        + 1;
    receipt["id"] = json!(id());
    receipt["sequence"] = json!(sequence);
    receipt["previous"] = last.map_or(Value::Null, |r| r["digest"].clone());
    let hash = digest(&receipt);
    db.execute(
        "INSERT INTO am_deletion_receipts(id,workspace_id,sequence,repository_id,operation,target,body,digest,previous,created_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
        params![s(&receipt, "id"), workspace, sequence, repository, operation, target,
            graphicle_memory::text::canonical(&receipt), hash, receipt["previous"].as_str(), now()],
    )?;
    receipt["digest"] = json!(hash);
    Ok(receipt)
}

/// Recompute every digest and link of the workspace's receipt chain.
fn chain(db: &Connection, workspace: &str) -> Result<Value> {
    let mut previous = Value::Null;
    let mut intact = true;
    let mut length = 0i64;
    for row in rows(
        db,
        "SELECT sequence,body,digest FROM am_deletion_receipts WHERE workspace_id=? ORDER BY sequence",
        [workspace],
    )? {
        length += 1;
        let body: Value = serde_json::from_str(s(&row, "body"))?;
        intact &= row["sequence"] == json!(length)
            && body["sequence"] == row["sequence"]
            && body["previous"] == previous
            && digest(&body) == s(&row, "digest");
        previous = row["digest"].clone();
    }
    Ok(json!({"length":length,"intact":intact,"head":previous}))
}

/// Flush deleted pages out of the write-ahead log after the deletion commits;
/// `secure_delete` has already zeroed them in the database pages.
pub fn checkpoint(store: &Store) -> Result<Value> {
    let (busy, log, done) =
        store
            .connect()?
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
    Ok(
        json!({"wal_checkpoint":if busy == 0 {"truncated"} else {"busy"},"log_frames":log,"checkpointed_frames":done}),
    )
}

/// Re-check a stored receipt against the current database and its chain.
/// Source rows, id references, serving and external cleanup are re-checked;
/// verbatim text is checked only at deletion time, since the receipt keeps no
/// excerpt of what it deleted. A document receipt keeps only one-way hashes,
/// so originals and historical copies are re-checked by their id hashes.
/// A failed deletion-time text scan stays failed: its discarded excerpts
/// cannot establish that later edits removed the known residue.
pub fn verify(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    store.authorized_read(workspace, token, "admin", |db, p| {
        let row = one(
            db,
            "SELECT body,digest FROM am_deletion_receipts WHERE workspace_id=? AND id=?",
            params![workspace, string(args, "receipt_id", 100)?],
        )?
        .ok_or_else(|| Error::bad("Deletion receipt unavailable"))?;
        let mut receipt: Value = serde_json::from_str(s(&row, "body"))?;
        receipt["digest"] = row["digest"].clone();
        let repository = s(&receipt, "repository_id").to_string();
        let catalog = tables(db)?;
        let project = repo(db, p, &repository).ok();
        let current = match s(&receipt, "operation") {
            "connector.delete" => {
                let scope = Scope::connector(s(&receipt, "target"));
                let remaining = source_rows(db, workspace, &repository, &scope)?;
                let ids = references(db, &catalog, &[scope.prefix()], (&repository, workspace), &[], false)?;
                // Caches refill with unrelated work, so only the deleted
                // sources' own rows and references are re-checked.
                let visible = match &project {
                    Some(project) => Some(
                        crate::memory::documents(db, project, p)?
                            .iter()
                            .filter(|d| scope.source(s(d, "id")))
                            .count(),
                    ),
                    None => None,
                };
                let text_clean = empty(&receipt["residue"]["verbatim_text"]);
                let clean = remaining.values().all(|n| *n == 0) && empty(&ids) && visible.unwrap_or(0) == 0 && text_clean;
                json!({"verified_local":clean,"rows":remaining,"source_ids":ids,
                    "verbatim_text":{"recheck":"at_deletion_time","clean":text_clean},
                    "serving_probe":visible.map_or(json!("project_deleted"),|n|json!({"visible_documents":n}))})
            }
            "source.forget" | "retention.forget" => {
                let connector = s(&receipt, "target");
                let hashes: BTreeSet<&str> = receipt["sources"]["document_hashes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let mut reappeared = BTreeMap::new();
                for (name, table, column) in [
                    ("documents", "am_connector_docs", "id"),
                    ("versions", "am_connector_versions", "id"),
                    ("staged", "am_connector_stage", "id"),
                    ("permissions", "am_source_permissions", "source_id"),
                ] {
                    let count = rows(db, &format!("SELECT {column} AS id FROM {table} WHERE connector_id=?"), [connector])?
                        .iter().filter(|d| hashes.contains(tombstone(connector, s(d,"id")).as_str())).count();
                    reappeared.insert(name, count);
                }
                let prefix = format!("connector:{connector}:");
                let originals = rows(db, "SELECT source_id FROM am_original_documents WHERE repository_id=?", [&repository])?
                    .iter().filter(|d| s(d,"source_id").strip_prefix(&prefix)
                        .is_some_and(|id| hashes.contains(tombstone(connector,id).as_str()))).count();
                reappeared.insert("originals", originals);
                let tombstones = rows(
                    db,
                    "SELECT id_hash FROM am_forgotten_sources WHERE connector_id=?",
                    [connector],
                )?
                .iter()
                .filter(|r| hashes.contains(s(r, "id_hash")))
                .count();
                let connector_present = one(db,"SELECT id FROM am_connectors WHERE id=?",[connector])?.is_some();
                let clean = receipt["verified_local"] == true && reappeared.values().all(|n| *n == 0)
                    && (!connector_present || tombstones == hashes.len());
                json!({"verified_local":clean,"reappeared_documents":reappeared["documents"],"reappeared_rows":reappeared,
                    "deletion_time_checks_passed":receipt["verified_local"],
                    "tombstones":tombstones,"connector_present":connector_present})
            }
            "project.delete" => {
                let connectors: Vec<String> = receipt["connectors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect();
                let remaining = project_rows(db, &catalog, &repository, &connectors)?;
                let mut patterns = vec![repository.clone()];
                patterns.extend(connectors.iter().map(|c| format!("connector:{c}:")));
                let ids = references(db, &catalog, &patterns, (&repository, workspace), &retained(db)?, true)?;
                json!({"verified_local":empty(&remaining) && empty(&ids),"project_rows":remaining,"source_ids":ids})
            }
            "memory.delete" => {
                // Only one-way hashes of the ids are kept: confirm no
                // current memory carries a deleted id.
                let hashes: BTreeSet<&str> = receipt["removed"]["keys"]["hashes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let reappeared = rows(db, "SELECT id FROM repo_memories WHERE repository_id=?", [&repository])?
                    .iter()
                    .filter(|m| hashes.contains(key_hash("memory.delete", s(m, "id")).as_str()))
                    .count();
                json!({"verified_local":reappeared == 0 && receipt["verified_local"] == true,
                    "reappeared_memories":reappeared})
            }
            "storage.prune" | "retention.graph" => {
                // Pruned keys are kept only as hashes; the removal was
                // re-counted when the receipt was signed.
                json!({"verified_local":receipt["verified_local"] == true,"recheck":"at_deletion_time"})
            }
            _ => return Err(Error::bad("Unknown deletion receipt")),
        };
        let (cleanups, pending) = external_now(db, &receipt["external_cleanup"])?;
        let chain = chain(db, workspace)?;
        let verified = current["verified_local"] == true;
        Ok(json!({"receipt":receipt,"chain":chain,"current":current,"external_cleanup":cleanups,
            "verified_local":verified,"external_pending":pending,
            "complete":verified && !pending && chain["intact"] == true,"checked_at":now()}))
    })
}

/// Receipts of this workspace, newest first, with the chain status.
pub fn list(store: &Store, workspace: &str, token: &str) -> Result<Value> {
    store.authorized_read(workspace, token, "admin", |db, _| {
        let receipts: Vec<Value> = rows(
            db,
            "SELECT id,sequence,repository_id,operation,target,digest,previous,created_at,json_extract(body,'$.verified_local') AS verified_local,json_extract(body,'$.external_pending') AS external_pending FROM am_deletion_receipts WHERE workspace_id=? ORDER BY sequence DESC LIMIT 200",
            [workspace],
        )?
        .into_iter()
        .map(|mut r| {
            for key in ["verified_local", "external_pending"] {
                r[key] = json!(r[key] == 1);
            }
            r
        })
        .collect();
        Ok(json!({"receipts":receipts,"chain":chain(db, workspace)?}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_avoid_json_escaped_characters_and_short_texts() {
        assert_eq!(excerpt("Too short."), None);
        let text = "Line one \"quoted\"\nThe quarterly migration window opens on the third Friday at 09:00 UTC for all regions.\n";
        let window = excerpt(text).unwrap();
        assert!(text.contains(&window));
        assert!(window.len() >= MIN_FINGERPRINT_BYTES && window.len() <= FINGERPRINT_BYTES);
        assert!(!window.contains(['"', '\\', '\n']));
        assert!(
            json!({ "body": text }).to_string().contains(&window),
            "found inside serialized JSON"
        );
    }

    #[test]
    fn document_scopes_do_not_capture_documents_with_longer_ids() {
        let scope = Scope::documents("c1", &json!(["a", "issue"])).unwrap();
        assert!(scope.source("connector:c1:a"));
        assert!(!scope.source("connector:c1:a:b"));
        assert!(!scope.source("connector:c1:ab"));
        assert!(!scope.source("connector:c2:a"));
        assert!(scope.passage("connector:c1:a:120"));
        assert!(
            !scope.passage("connector:c1:a:b:0"),
            "passage of document a:b"
        );
        assert!(scope.passage("connector:c1:issue:0"));
        let whole = Scope::connector("c1");
        assert!(whole.source("connector:c1:anything:at:all"));
        assert!(whole.passage("connector:c1:x:7"));
        assert!(!whole.source("connector:c10:x"));
        let existing = BTreeSet::from(["connector:c1:a:b".to_string()]);
        let ids = scope.sources();
        assert!(
            !names_any(r#"["connector:c1:a:b"]"#, &ids, &existing),
            "another document"
        );
        assert!(names_any(r#"["connector:c1:a"]"#, &ids, &existing));
        assert!(
            names_any("connector:c1:a:17", &ids, &existing),
            "a passage of a"
        );
    }

    #[test]
    fn state_masking_ignores_minted_ids_and_clocks_only() {
        let masked = mask(
            "job 3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f at 2026-09-29T13:41:31.207743Z sha 3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f",
        );
        assert_eq!(
            masked,
            "job <id> at <time> sha 3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f"
        );
    }
}
