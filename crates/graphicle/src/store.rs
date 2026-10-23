//! Native ownership of the same source/revision tables used by the original host.
use crate::{Error, Result, digest, id, integer, native, now, string};
use rusqlite::{
    Connection, OptionalExtension, Params, Transaction, TransactionBehavior, params,
    types::ValueRef,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    panic::Location,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

/// A write transaction held longer than this is logged with its call site:
/// every other writer waits for it, up to the 10-second busy timeout.
const SLOW_HOLD: Duration = Duration::from_secs(2);
/// Slow holds kept for `runtime.status`, newest last.
const SLOW_KEPT: usize = 20;

#[derive(Default)]
struct LockStats {
    transactions: u64,
    held: Duration,
    longest: Duration,
    longest_at: String,
    slow: u64,
    recent: VecDeque<(String, f64)>,
}
static LOCKS: Mutex<LockStats> = Mutex::new(LockStats {
    transactions: 0,
    held: Duration::ZERO,
    longest: Duration::ZERO,
    longest_at: String::new(),
    slow: 0,
    recent: VecDeque::new(),
});

/// A call site's file relative to its crate (`src/jobs.rs`). Cargo gives
/// rustc absolute paths for path dependencies outside the package being
/// built (this runtime inside an edition's program, for example); lock reports
/// must not reveal the build machine's folders.
fn crate_relative(file: &str) -> &str {
    if let Some(rest) = file.strip_prefix(env!("CARGO_MANIFEST_DIR"))
        && let Some(rest) = rest.strip_prefix(['/', '\\'])
    {
        return rest;
    }
    if Path::new(file).is_absolute()
        && let Some(at) = file.rfind("/src/").or_else(|| file.rfind("\\src\\"))
    {
        return &file[at + 1..];
    }
    file
}

fn record_hold(at: &Location<'_>, held: Duration) {
    let site = format!("{}:{}", crate_relative(at.file()), at.line());
    let mut stats = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    stats.transactions += 1;
    stats.held += held;
    if held > stats.longest {
        stats.longest = held;
        stats.longest_at = site.clone();
    }
    if held >= SLOW_HOLD {
        stats.slow += 1;
        if stats.recent.len() == SLOW_KEPT {
            stats.recent.pop_front();
        }
        stats.recent.push_back((site.clone(), held.as_secs_f64()));
        drop(stats);
        // A diagnostic, not a message for people: logged
        // with the other phase timings (`GRAPHICLE_PHASE_TIMINGS=1`); the
        // counts stay in `lock_report` (`status --json`'s `write_lock`).
        if crate::phase::enabled() {
            eprintln!(
                "graphicle: the write lock was held {:.1} s at {site}; other writers waited",
                held.as_secs_f64()
            );
        }
    }
}

/// Write-lock use by this process: how many write transactions ran, how long
/// they held the lock, the longest hold and recent slow holds by call site.
pub fn lock_report() -> Value {
    let stats = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    json!({"write_transactions":stats.transactions,"held_seconds":stats.held.as_secs_f64(),
        "longest_seconds":stats.longest.as_secs_f64(),"longest_at":stats.longest_at,
        "slow_threshold_seconds":SLOW_HOLD.as_secs_f64(),"slow_holds":stats.slow,
        "recent_slow":stats.recent.iter().map(|(at,s)|json!({"at":at,"seconds":s})).collect::<Vec<_>>()})
}

#[derive(Clone)]
pub struct Store {
    pub directory: Arc<PathBuf>,
    pub database: Arc<PathBuf>,
    /// Signalled when a job is queued, so an idle worker in this process
    /// starts it at once instead of at its next poll.
    pub wake: Arc<tokio::sync::Notify>,
    /// The size limits of whoever runs this program ([`Store::with_limits`]);
    /// without them, the environment's over Graphicle Core's defaults.
    limits: Option<Arc<crate::limits::Limits>>,
}
#[derive(Clone)]
pub struct Principal {
    pub workspace: String,
    pub credential: String,
    pub hash: String,
    pub role: String,
}
/// The roles a workspace credential may have in Core, besides the owner
/// capability. An installed extension may define more
/// (`Extension::role_permits`).
pub const CREDENTIAL_ROLES: &[&str] = &["reader", "agent", "reviewer", "collector", "runner"];
impl Principal {
    /// Whether the core's role grants `permission`; false for a role the core
    /// does not define.
    pub fn permits(&self, permission: &str) -> bool {
        match self.role.as_str() {
            "owner" => true,
            "reviewer" => ["read", "write", "collect", "propose", "approve"].contains(&permission),
            "collector" => ["read", "collect", "propose"].contains(&permission),
            "runner" => ["read", "propose", "run", "verify"].contains(&permission),
            "agent" => ["read", "propose", "run"].contains(&permission),
            "reader" => permission == "read",
            _ => false,
        }
    }
}
pub fn rows(db: &Connection, sql: &str, arguments: impl Params) -> Result<Vec<Value>> {
    let mut statement = db.prepare(sql)?;
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|x| x.to_string())
        .collect();
    let mapped = statement.query_map(arguments, |row| {
        let mut value = Map::new();
        for (i, name) in names.iter().enumerate() {
            let field = match row.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
                ValueRef::Blob(_) => Value::Null,
            };
            value.insert(name.clone(), field);
        }
        Ok(Value::Object(value))
    })?;
    Ok(mapped.collect::<std::result::Result<Vec<_>, _>>()?)
}
pub fn one(db: &Connection, sql: &str, arguments: impl Params) -> Result<Option<Value>> {
    Ok(rows(db, sql, arguments)?.into_iter().next())
}
pub fn decode(value: &mut Value, key: &str) -> Result<()> {
    if let Some(raw) = value[key].as_str() {
        value[key] = serde_json::from_str(raw)?;
    }
    Ok(())
}
pub fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
pub fn path(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.len() > 1000
        || value.starts_with('/')
        || value.contains(['\\', '\0', '\r', '\n'])
        // Windows: a drive (`C:`) or a stream name never names a file
        // under the root.
        || (cfg!(windows) && value.contains(':'))
        || value
            .split('/')
            .any(|v| v.is_empty() || v == "." || v == "..")
    {
        return Err(Error::bad("Use a relative source path without traversal"));
    }
    Ok(value)
}

/// [`path`] for a path an MCP or HTTP client sends. On Windows `\` is
/// accepted as a separator and becomes `/`; traversal, drives and UNC paths
/// (`\\server\share` becomes `//server/share`) are still refused.
pub fn client_path(value: &str) -> Result<std::borrow::Cow<'_, str>> {
    #[cfg(windows)]
    if value.contains('\\') {
        let converted = value.replace('\\', "/");
        path(&converted)?;
        return Ok(std::borrow::Cow::Owned(converted));
    }
    path(value).map(std::borrow::Cow::Borrowed)
}
pub fn hash(value: &str) -> Result<&str> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Err(Error::bad("Expected lowercase SHA-256"))
    } else {
        Ok(value)
    }
}

/// One manifest entry of a committing sync: whether its content still needs
/// analysis or chunk rows.
struct SyncDocument {
    path: String,
    sha: String,
    analyze: bool,
    chunk: bool,
}

/// Documents a commit loads, analyses and chunks at a time (at most
/// [`crate::limits::REQUEST_BYTES`] of text).
const COMMIT_BATCH_FILES: usize = 512;

/// The search chunks of `content` at `path`: forty lines each, indexed with
/// the path's words, the text and the text's words.
fn chunk_rows(path: &str, content: &str) -> Vec<(usize, usize, String)> {
    let path_words = graphicle_memory::text::words(path).join(" ");
    content
        .split_inclusive('\n')
        .collect::<Vec<_>>()
        .chunks(40)
        .enumerate()
        .map(|(number, chunk)| {
            let line = number * 40 + 1;
            let text = chunk.join("");
            let indexed = format!(
                "{path_words}\n{text}\n{}",
                graphicle_memory::text::words(&text).join(" ")
            );
            (line, line + chunk.len() - 1, indexed)
        })
        .collect()
}

/// The `repo_parses` row of `content` at `path`: its revision and analysis,
/// within the native `analyze` request's `limits`. Limits never fail an
/// analysis. An analysis that fails otherwise is recorded as skipped, with
/// the reason, and the file stays searchable as text. Limited and skipped
/// analyses carry `revisions[1]` (the limited revision), so a change of the
/// limits redoes them; complete ones carry `revisions[0]`.
///
/// A failure that is not about the file is not cached as the file's: a
/// parser that is missing or incompatible fails the commit, and a parse that
/// did not finish (a timeout under load) or a caught panic is recorded under
/// a revision the next sync treats as outdated, so each sync retries it once.
fn analysis_row(
    path: &str,
    content: &str,
    limits: &Value,
    revisions: [&str; 2],
) -> Result<(String, String)> {
    let analysis = match native(
        "analyze",
        json!({"path":path,"content":content,"limits":limits}),
    ) {
        Ok(analysis) => analysis,
        Err(error) if parser_unavailable(&error.message) => {
            return Err(Error::bad(format!(
                "Could not analyse {path}: {}. Reinstall Graphicle",
                error.message
            )));
        }
        Err(error) => {
            let revision = if transient_failure(&error.message) {
                format!("{}+retry", revisions[1])
            } else {
                revisions[1].to_string()
            };
            let skipped = graphicle_memory::structure::skipped(&error.message);
            return Ok((revision, skipped.to_string()));
        }
    };
    let revision = if analysis.get("limited").is_some() || analysis.get("skipped").is_some() {
        revisions[1]
    } else {
        revisions[0]
    };
    Ok((revision.to_string(), analysis.to_string()))
}

/// An analysis error that is about this installation, not the file.
fn parser_unavailable(message: &str) -> bool {
    message.starts_with("Bundled parser unavailable")
        || message.starts_with("Parser grammar is incompatible")
}

/// An analysis error that may not recur: a parse that did not finish (a
/// timeout under load) or a caught panic.
fn transient_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    message.starts_with("Source parsing did not complete")
        || message.starts_with("Native operation failed")
        || lower.contains("timeout")
        || lower.contains("timed out")
}

/// The files of `repository`'s current heads whose structure analysis was
/// limited (`[{path, limit, reason}]`) or skipped (`[{path, reason}]`), in
/// path order. Complete analyses carry `structure::REVISION`, so only the
/// others are read.
pub fn analysis_report(db: &Connection, repository: &str) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut statement = db.prepare_cached(
        "SELECT h.path,p.data FROM repo_heads h JOIN repo_parses p ON p.repository_id=h.repository_id AND p.path=h.path AND p.sha256=h.sha256 WHERE h.repository_id=? AND p.revision!=? ORDER BY h.path",
    )?;
    let rows = statement.query_map(
        params![repository, graphicle_memory::structure::REVISION],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?;
    let (mut limited, mut skipped) = (Vec::new(), Vec::new());
    for row in rows {
        let (path, data) = row?;
        let analysis: Value = serde_json::from_str(&data)?;
        if analysis.get("skipped").is_some() {
            skipped.push(json!({"path":path,"reason":analysis["issues"][0]}));
        } else if let Some(limit) = analysis["limited"].as_str() {
            let reason = graphicle_memory::structure::limit_issues(&analysis).join("; ");
            limited.push(json!({"path":path,"limit":limit,"reason":reason}));
        }
    }
    Ok((limited, skipped))
}

impl Store {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        if directory.is_symlink() {
            return Err(Error::bad("Data directory cannot be a symlink"));
        }
        std::fs::create_dir_all(directory)?;
        let database = directory.join("reviews.sqlite3");
        if database.is_symlink() {
            return Err(Error::bad("Database cannot be a symlink"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let store = Self {
            directory: Arc::new(directory.to_path_buf()),
            database: Arc::new(database),
            wake: Arc::new(tokio::sync::Notify::new()),
            limits: None,
        };
        let db = store.connect()?;
        // A store that needs a newer program is refused before anything in
        // it changes.
        check_reader(&db)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(READERS)?;
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("workflow_schema.sql"))?;
        db.execute_batch(include_str!("expansion_schema.sql"))?;
        db.execute_batch(include_str!("projection_schema.sql"))?;
        ensure_column(&db, "gf_episodes", "parent_episode", "TEXT")?;
        db.execute_batch(include_str!("temporal_schema.sql"))?;
        crate::temporal_graph::migrate(&db)?;
        crate::graph_delta::migrate(&db)?;
        db.execute_batch(include_str!("product_schema.sql"))?;
        crate::work_state::migrate(&db)?;
        // Each folder's freshness record.
        crate::fresh::migrate(&db)?;
        // Documents' mention links to code symbols.
        crate::doc_links::migrate(&db)?;
        // Each project's passage index, which `context` reads.
        crate::passage_index::migrate(&db)?;
        ensure_column(
            &db,
            "gf_cross_unresolved",
            "exporter_episodes",
            "TEXT NOT NULL DEFAULT '{}'",
        )?;
        // Coverage rows carry the granularity of the upload that produced
        // them (`test` or `file`); rows written before the column existed
        // were all per-test.
        ensure_column(
            &db,
            "gf_test_coverage",
            "granularity",
            "TEXT NOT NULL DEFAULT 'test'",
        )?;
        // Projects earlier `graphicle pr` runs registered are PR scratch
        // copies: they no longer make imports ambiguous.
        crate::graph_projection::mark_legacy_pr_projects(&db)?;
        // Extension tables last: they may reference core tables. Stores
        // keep every table, so an edition without an extension ignores its
        // tables.
        crate::extension::installed().migrate(&db)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                store.database.as_ref(),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }
        Ok(store)
    }
    /// This store with the size limits of the running program's
    /// configuration file and environment ([`crate::limits`]).
    pub fn with_limits(mut self, limits: crate::limits::Limits) -> Self {
        self.limits = Some(Arc::new(limits));
        self
    }
    /// The size limits in effect: those set with [`Store::with_limits`],
    /// else the environment's over Graphicle Core's defaults.
    pub fn limits(&self) -> Result<crate::limits::Limits> {
        match &self.limits {
            Some(limits) => Ok(**limits),
            None => crate::limits::Limits::from_environment(&BTreeMap::new()),
        }
    }
    pub fn connect(&self) -> Result<Connection> {
        let db = Connection::open(self.database.as_ref())?;
        db.busy_timeout(Duration::from_secs(10))?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.pragma_update(None, "secure_delete", true)?;
        Ok(db)
    }
    /// A write transaction (`BEGIN IMMEDIATE`): it takes SQLite's single write
    /// lock up front, so a read-then-write never fails to upgrade. Keep the
    /// work inside short; long holds are logged with the caller's location.
    #[track_caller]
    pub fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let at = Location::caller();
        let mut db = self.connect()?;
        let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let started = Instant::now();
        let outcome = (move || {
            let value = f(&transaction)?;
            transaction.commit()?;
            Ok(value)
        })();
        record_hold(at, started.elapsed());
        outcome
    }
    /// A read-only snapshot (`BEGIN DEFERRED` with `query_only`). In WAL mode
    /// it neither takes nor waits for the write lock, so long reads run beside
    /// writers and each other. A write inside it fails at once instead of
    /// upgrading the transaction and failing only under concurrency.
    pub fn read<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut db = self.connect()?;
        db.pragma_update(None, "query_only", true)?;
        let transaction = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let value = f(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }
    #[track_caller]
    pub fn authorized<T>(
        &self,
        workspace: &str,
        token: &str,
        permission: &str,
        f: impl FnOnce(&Transaction<'_>, &Principal) -> Result<T>,
    ) -> Result<T> {
        if token.len() > 512 {
            return Err(Error::denied());
        }
        self.transaction(|db| {
            let principal = authorize_hash(
                db,
                workspace,
                &graphicle_memory::text::sha256(token.as_bytes()),
                permission,
            )?;
            f(db, &principal)
        })
    }
    /// `authorized` for operations that only read: a read-only snapshot.
    pub fn authorized_read<T>(
        &self,
        workspace: &str,
        token: &str,
        permission: &str,
        f: impl FnOnce(&Transaction<'_>, &Principal) -> Result<T>,
    ) -> Result<T> {
        if token.len() > 512 {
            return Err(Error::denied());
        }
        self.read(|db| {
            let principal = authorize_hash(
                db,
                workspace,
                &graphicle_memory::text::sha256(token.as_bytes()),
                permission,
            )?;
            f(db, &principal)
        })
    }
    pub fn create_workspace(&self, name: &str) -> Result<Value> {
        string(&json!({"name":name}), "name", 100)?;
        self.transaction(|db| {
            let count:i64=db.query_row("SELECT count(*) FROM workspaces",[],|r|r.get(0))?;
            if count>=100{return Err(Error::bad("Workspace limit reached"));}
            let identifier=id();let token=format!("{}{}",id(),id());let stamp=now();
            db.execute("INSERT INTO workspaces VALUES(?,?,?,?,0)",params![identifier,graphicle_memory::text::sha256(token.as_bytes()),name.trim(),stamp])?;
            Ok(json!({"id":identifier,"name":name.trim(),"capability":token,"version":0,"created_at":stamp}))
        })
    }
    pub fn workspace(&self, workspace: &str, token: &str) -> Result<Value> {
        self.authorized_read(workspace, token, "read", |db, p| {
            let mut row = one(
                db,
                "SELECT id,name,created_at,version FROM workspaces WHERE id=?",
                [workspace],
            )?
            .ok_or_else(Error::denied)?;
            row["credential_role"] = json!(p.role);
            Ok(row)
        })
    }
    pub fn grant(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,_| {
            let role=string(args,"role",30)?;
            if !CREDENTIAL_ROLES.contains(&role) {
                // A role an installed extension defines, which it may refuse
                // a new credential on.
                let extensions=crate::extension::installed();
                if extensions.role_permits(db,workspace,role,"read")?.is_none() {return Err(Error::bad("Invalid credential role"));}
                extensions.grant_role(db,workspace,role)?;
            }
            let label=string(args,"label",100)?; let token=format!("{}{}",id(),id()); let identifier=id();
            let expiry=args.get("expires_at").and_then(Value::as_str);
            if let Some(value)=expiry && graphicle_memory::temporal::instant(value).map_err(Error::bad)?<=chrono::Utc::now().fixed_offset() {return Err(Error::bad("Expiry must be in the future"));}
            db.execute("INSERT INTO credentials VALUES (?,?,?,?,?,?,?,NULL)",params![identifier,workspace,graphicle_memory::text::sha256(token.as_bytes()),role,label,now(),expiry])?;
            Ok(json!({"id":identifier,"capability":token,"role":role,"label":label,"expires_at":expiry}))
        })
    }
    pub fn revoke(&self, workspace: &str, token: &str, credential: &str) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,_| {
            if db.execute("UPDATE credentials SET revoked_at=? WHERE id=? AND workspace_id=?",params![now(),credential,workspace])?!=1 {return Err(Error::bad("Credential not found"));}
            // A member linked to the credential is deactivated too, on any
            // store that has the team tables, so the revocation holds in
            // every edition that opens the store.
            crate::store_compat::credential_revoked(db,workspace,credential)?;
            crate::extension::installed().credential_revoked(db,workspace,credential)?;
            // A queued job never retains the authority of a revoked credential.
            db.execute("UPDATE am_jobs SET state='cancelled',updated_at=? WHERE principal_hash IN (SELECT capability_hash FROM credentials WHERE id=?) AND state IN ('queued','retry','running')",params![now(),credential])?;
            // Cached answers produced under the revoked credential are no longer current.
            let hash:String=db.query_row("SELECT capability_hash FROM credentials WHERE id=?",[credential],|r|r.get(0))?;
            let revoked_sources=credential_sources(db,workspace,credential)?;
            crate::invalidation::permissions_changed(db,workspace,Some(&hash),&revoked_sources,"credential_revoked")?;
            Ok(json!({"revoked":true,"sources":revoked_sources}))
        })
    }
    pub fn register(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"collect",|db,p| {
            let key=string(args,"project_key",200)?; let name=string(args,"name",150)?;
            if let Some(row)=one(db,"SELECT id FROM repo_projects WHERE workspace_id=? AND project_key=?",params![workspace,key])? {repo(db,p,s(&row,"id"))?;return Ok(json!({"id":row["id"],"replayed":true}));}
            let identifier=id();let stamp=now();
            db.execute("INSERT INTO repo_projects(id,workspace_id,project_key,name,created_at,updated_at) VALUES(?,?,?,?,?,?)",params![identifier,workspace,key,name,stamp,stamp])?;
            Ok(json!({"id":identifier,"replayed":false}))
        })
    }
    /// Mark a project as a PR scratch copy (`pr_scratch`): the
    /// project `graphicle pr` registers for a commit range. Linking skips it
    /// while another project exports its packages. Idempotent.
    pub fn mark_pr_scratch(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        self.authorized(workspace, token, "collect", |db, p| {
            let repository = repo(db, p, repository)?;
            db.execute(
                "INSERT OR IGNORE INTO repo_project_roles(repository_id,role,created_at) VALUES(?,?,?)",
                params![s(&repository, "id"), crate::graph_projection::PR_SCRATCH, now()],
            )?;
            Ok(json!({"id":repository["id"],"role":crate::graph_projection::PR_SCRATCH}))
        })
    }
    /// Start a sync (`sync.prepare`) with the snapshot's manifest,
    /// `[{path, sha256, bytes}]`. A request carries at most
    /// [`crate::limits::REQUEST_FILES`] entries, so a larger manifest comes in
    /// parts: every part but the last has `manifest_complete: false`, and the
    /// parts after the first name the `sync_id` the first one returned. The
    /// manifest is held to the project limits of whoever runs Graphicle
    /// ([`crate::limits`]); its last part returns the `snapshot` and the paths
    /// whose content is `missing`, or `replayed: true` for an unchanged tree.
    /// A failed request changes nothing; `sync.abort` drops an unfinished sync.
    pub fn sync_prepare(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let limits = self.limits()?;
        let manifest = args["manifest"]
            .as_array()
            .ok_or_else(|| Error::bad("Expected manifest"))?;
        if manifest.len() > crate::limits::REQUEST_FILES {
            return Err(Error::bad(
                "At most 5,000 manifest entries per request; send a larger manifest in parts with manifest_complete: false",
            ));
        }
        let complete = match args.get("manifest_complete") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(complete)) => *complete,
            Some(_) => return Err(Error::bad("Expected manifest_complete: true or false")),
        };
        let mut part = BTreeMap::new();
        for item in manifest {
            let name = path(string(item, "path", 1000)?)?;
            let sha = hash(string(item, "sha256", 64)?)?;
            let size = integer(item, "bytes", 0, limits.file_bytes)?;
            if part
                .insert(name.to_string(), json!({"sha256":sha,"bytes":size}))
                .is_some()
            {
                return Err(Error::bad("Duplicate source path"));
            }
        }
        let continued = match args.get("sync_id") {
            None | Some(Value::Null) => None,
            Some(_) => Some(string(args, "sync_id", 100)?),
        };
        let metadata = args.get("metadata").cloned().unwrap_or(json!({}));
        if !metadata.is_object() || metadata.to_string().len() > 65536 {
            return Err(Error::bad("Invalid bounded metadata"));
        }
        let limited = graphicle_memory::structure::limited_revision(&limits.analysis());
        self.authorized(workspace,token,"collect",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;
            let identifier=s(&repository,"id");
            // Earlier parts' entries, the snapshot the sync started from, and its metadata.
            let (mut files,expected,metadata)=match continued {
                Some(sync_id)=>{
                    let sync=one(db,"SELECT * FROM repo_syncs WHERE repository_id=? AND id=?",params![identifier,sync_id])?.ok_or_else(||Error::bad("Sync not found"))?;
                    if s(&sync,"state")!="preparing" {return Err(Error::conflict("Sync is not waiting for more of its manifest"));}
                    (serde_json::from_str::<BTreeMap<String,Value>>(s(&sync,"manifest"))?,s(&sync,"expected_snapshot").to_string(),s(&sync,"metadata").to_string())
                }
                None=>{
                    if repository["snapshot"]!=args["expected_snapshot"] {return Err(Error::conflict("Repository changed during scanning"));}
                    (BTreeMap::new(),s(&repository,"snapshot").to_string(),metadata.to_string())
                }
            };
            for (name,item) in part {
                if files.insert(name,item).is_some() {return Err(Error::bad("Duplicate source path"));}
            }
            let bytes:usize=files.values().map(|item|item["bytes"].as_u64().unwrap_or(0) as usize).sum();
            if files.len()>limits.project_files || bytes>limits.project_bytes {
                return Err(crate::limits::project_refusal(files.len(),bytes,false,&limits));
            }
            let unfinished=|db:&Connection|->Result<()>{
                let count:i64=db.query_row("SELECT count(*) FROM repo_syncs WHERE repository_id=? AND state IN ('pending','preparing')",[identifier],|r|r.get(0))?;
                if count>=20{return Err(Error::conflict("Abort an unfinished sync before adding another"));}
                Ok(())
            };
            if !complete {
                let manifest=graphicle_memory::text::canonical(&json!(files));
                let sync_id=match continued {
                    Some(sync_id)=>{db.execute("UPDATE repo_syncs SET manifest=? WHERE id=?",params![manifest,sync_id])?;sync_id.to_string()}
                    None=>{unfinished(db)?;let sync_id=id();db.execute("INSERT INTO repo_syncs VALUES(?,?,?,?,?,?,?,?)",params![sync_id,identifier,expected,"",manifest,metadata,"preparing",now()])?;sync_id}
                };
                return Ok(json!({"sync_id":sync_id,"manifest_complete":false,"files":files.len()}));
            }
            if s(&repository,"snapshot")!=expected {return Err(Error::conflict("Repository changed during scanning"));}
            let files=json!(files);
            let snapshot=digest(&files);
            let outdated:i64=db.query_row("SELECT count(*) FROM repo_heads h LEFT JOIN repo_parses p ON p.repository_id=h.repository_id AND p.path=h.path AND p.sha256=h.sha256 WHERE h.repository_id=? AND (p.revision IS NULL OR p.revision NOT IN (?,?))",params![identifier,graphicle_memory::structure::REVISION,limited],|r|r.get(0))?;
            if s(&repository,"snapshot")==snapshot && outdated==0 {
                if let Some(sync_id)=continued {db.execute("DELETE FROM repo_syncs WHERE id=?",[sync_id])?;}
                // Identical tree, possibly a new commit identity: the existing
                // episode still gains the sha from this sync's metadata.
                let metadata:Value=serde_json::from_str(&metadata)?;
                let projection=crate::jobs::enqueue_projection(db,identifier,p,&snapshot,&metadata)?;
                return Ok(json!({"snapshot":snapshot,"sync_id":null,"missing":[],"replayed":true,"projection_job":projection}));
            }
            let mut missing=Vec::new();
            let mut exists=db.prepare_cached("SELECT EXISTS(SELECT 1 FROM repo_objects WHERE repository_id=? AND sha256=? AND bytes=?)")?;
            for (name,item) in files.as_object().unwrap() {
                if !exists.query_row(params![identifier,s(item,"sha256"),item["bytes"].as_i64()],|r|r.get::<_,bool>(0))? {missing.push(name.clone());}
            }
            drop(exists);
            let manifest=graphicle_memory::text::canonical(&files);
            let sync_id=match continued {
                Some(sync_id)=>{db.execute("UPDATE repo_syncs SET snapshot=?,manifest=?,state='pending' WHERE id=?",params![snapshot,manifest,sync_id])?;sync_id.to_string()}
                None=>{unfinished(db)?;let sync_id=id();db.execute("INSERT INTO repo_syncs VALUES(?,?,?,?,?,?,?,?)",params![sync_id,identifier,expected,snapshot,manifest,metadata,"pending",now()])?;sync_id}
            };
            Ok(json!({"snapshot":snapshot,"sync_id":sync_id,"missing":missing,"replayed":false}))
        })
    }
    /// Send content for a pending sync (`sync.upload`): 1 to
    /// [`crate::limits::UPLOAD_FILES`] files whose text matches the manifest,
    /// at most [`crate::limits::REQUEST_BYTES`] of text unless it is one file.
    pub fn sync_upload(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let limits = self.limits()?;
        self.authorized(workspace, token, "collect", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let identifier = s(&repository, "id");
            let sync = one(
                db,
                "SELECT * FROM repo_syncs WHERE repository_id=? AND id=?",
                params![identifier, string(args, "sync_id", 100)?],
            )?
            .ok_or_else(|| Error::bad("Sync not found"))?;
            if s(&sync, "state") != "pending" {
                return Err(Error::conflict("Sync is not pending"));
            }
            let manifest: Value = serde_json::from_str(s(&sync, "manifest"))?;
            let files = args["files"]
                .as_array()
                .filter(|v| !v.is_empty() && v.len() <= crate::limits::UPLOAD_FILES)
                .ok_or_else(|| Error::bad("Upload 1–1,000 files"))?;
            let mut accepted = Vec::new();
            let mut sent = 0usize;
            for file in files {
                let name = path(string(file, "path", 1000)?)?;
                let content = file["content"]
                    .as_str()
                    .filter(|v| v.len() <= limits.file_bytes && !v.contains('\0'))
                    .ok_or_else(|| Error::bad("Invalid source text"))?;
                sent += content.len();
                if files.len() > 1 && sent > crate::limits::REQUEST_BYTES {
                    return Err(Error::bad(
                        "Upload at most 64 MiB of text per request, or a single file",
                    ));
                }
                let sha = graphicle_memory::text::sha256(content.as_bytes());
                if s(&manifest[name], "sha256") != sha
                    || manifest[name]["bytes"].as_u64() != Some(content.len() as u64)
                {
                    return Err(Error::conflict("Source does not match its manifest"));
                }
                db.execute(
                    "INSERT OR IGNORE INTO repo_objects VALUES(?,?,?,?)",
                    params![identifier, sha, content, content.len()],
                )?;
                accepted.push(name.to_string());
            }
            let bytes: i64 = db.query_row(
                "SELECT coalesce(sum(bytes),0) FROM repo_objects WHERE repository_id=?",
                [identifier],
                |r| r.get(0),
            )?;
            let retained = limits.retained_bytes();
            if usize::try_from(bytes).unwrap_or(usize::MAX) > retained {
                return Err(Error::bad(format!(
                    "Retained sources exceed {}, earlier versions included; prune earlier versions with storage.prune, or raise limits.project_bytes",
                    crate::limits::size(retained)
                )));
            }
            Ok(json!({"accepted":accepted,"snapshot_activated":false}))
        })
    }
    /// Activate a pending sync's snapshot (`sync.commit`). New content is
    /// analysed and chunked in batches outside the write lock; those rows are
    /// keyed by content, so they stay invisible until the heads move in the
    /// last transaction, and a commit that fails before it changes nothing a
    /// reader sees. Analysis limits never fail it: a file past one is listed
    /// under `limited`, and a file whose analysis fails under `skipped`, both
    /// still searchable as text. Storage, integrity and authorization errors
    /// fail it.
    pub fn sync_commit(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let limits = self.limits()?;
        let analysis_limits = limits.analysis_request();
        let limited = graphicle_memory::structure::limited_revision(&limits.analysis());
        let current = [graphicle_memory::structure::REVISION, limited.as_str()];
        // Plan the commit from a complete authorized snapshot.
        let (sync, documents) = self.authorized(workspace, token, "collect", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let sync = one(
                db,
                "SELECT * FROM repo_syncs WHERE repository_id=? AND id=?",
                params![s(&repository, "id"), string(args, "sync_id", 100)?],
            )?
            .ok_or_else(|| Error::bad("Sync not found"))?;
            match s(&sync, "state") {
                "aborted" => return Err(Error::conflict("Sync was aborted")),
                "preparing" => {
                    return Err(Error::conflict(
                        "Send the rest of the manifest before committing",
                    ));
                }
                _ => {}
            }
            let manifest: Value = serde_json::from_str(s(&sync, "manifest"))?;
            let identifier = s(&repository, "id");
            // Analysis and chunking are pure functions of `(path, content)`
            // and keyed by `(path, sha256)` in their tables, so an object
            // whose rows already exist at the current revision is carried
            // over; only new content is verified, loaded and parsed.
            let mut documents: Vec<SyncDocument> = Vec::new();
            let mut parsed = db.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM repo_parses WHERE repository_id=? AND path=? AND sha256=? AND revision IN (?,?))",
            )?;
            let mut chunked = db.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM repo_chunks WHERE repository_id=? AND path=? AND sha256=?)",
            )?;
            let mut exists = db.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM repo_objects WHERE repository_id=? AND sha256=?)",
            )?;
            for (path, item) in manifest
                .as_object()
                .ok_or_else(|| Error::bad("Invalid stored manifest"))?
            {
                let sha = s(item, "sha256");
                if !exists.query_row(params![identifier, sha], |r| r.get::<_, bool>(0))? {
                    return Err(Error::conflict("Upload missing sources before committing"));
                }
                let analyzed: bool = parsed.query_row(
                    params![identifier, path, sha, current[0], current[1]],
                    |r| r.get(0),
                )?;
                let has_chunks: bool =
                    chunked.query_row(params![identifier, path, sha], |r| r.get(0))?;
                documents.push(SyncDocument {
                    path: path.clone(),
                    sha: sha.to_string(),
                    analyze: !analyzed,
                    chunk: !has_chunks,
                });
            }
            Ok((sync, documents))
        })?;
        let work: Vec<&SyncDocument> = documents.iter().filter(|d| d.analyze || d.chunk).collect();
        let mut start = 0;
        while start < work.len() {
            // Load a batch (integrity checked), then analyse and chunk it
            // without holding the write lock.
            let contents = self.authorized_read(workspace, token, "collect", |db, p| {
                let repository = repo(db, p, string(args, "repository_id", 100)?)?;
                let mut load = db.prepare_cached(
                    "SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?",
                )?;
                let (mut contents, mut bytes) = (Vec::new(), 0);
                for document in &work[start..] {
                    if contents.len() == COMMIT_BATCH_FILES || bytes >= crate::limits::REQUEST_BYTES
                    {
                        break;
                    }
                    let content: String = load
                        .query_row(params![s(&repository, "id"), document.sha], |r| r.get(0))
                        .optional()?
                        .ok_or_else(|| {
                            Error::conflict("Upload missing sources before committing")
                        })?;
                    if graphicle_memory::text::sha256(content.as_bytes()) != document.sha {
                        return Err(Error::bad("Source integrity check failed"));
                    }
                    bytes += content.len();
                    contents.push(content);
                }
                Ok(contents)
            })?;
            let batch = &work[start..start + contents.len()];
            let rows: Vec<_> = batch
                .iter()
                .zip(&contents)
                .map(|(document, content)| -> Result<_> {
                    let analysis = if document.analyze {
                        Some(analysis_row(
                            &document.path,
                            content,
                            &analysis_limits,
                            current,
                        )?)
                    } else {
                        None
                    };
                    let chunks = if document.chunk {
                        chunk_rows(&document.path, content)
                    } else {
                        Vec::new()
                    };
                    Ok((analysis, chunks))
                })
                .collect::<Result<_>>()?;
            drop(contents);
            self.authorized(workspace, token, "collect", |db, p| {
                let repository = repo(db, p, string(args, "repository_id", 100)?)?;
                let identifier = s(&repository, "id");
                let state: String = db.query_row(
                    "SELECT state FROM repo_syncs WHERE id=? AND repository_id=?",
                    params![s(&sync, "id"), identifier],
                    |r| r.get(0),
                )?;
                if state == "aborted" {
                    return Err(Error::conflict("Sync was aborted"));
                }
                let mut insert_parse = db.prepare_cached("INSERT OR REPLACE INTO repo_parses VALUES(?,?,?,?,?)")?;
                let mut insert_chunk = db.prepare_cached("INSERT OR IGNORE INTO repo_chunks(repository_id,path,sha256,line,end_line,index_text) VALUES(?,?,?,?,?,?)")?;
                for (document, (analysis, chunks)) in batch.iter().zip(&rows) {
                    if let Some((revision, analysis)) = analysis {
                        insert_parse.execute(params![identifier, document.path, document.sha, revision, analysis])?;
                    }
                    for (line, end_line, text) in chunks {
                        insert_chunk.execute(params![identifier, document.path, document.sha, line, end_line, text])?;
                    }
                }
                Ok(())
            })?;
            start += batch.len();
        }
        self.authorized(workspace,token,"collect",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;let identifier=s(&repository,"id");
            let state:String=db.query_row("SELECT state FROM repo_syncs WHERE id=? AND repository_id=?",params![s(&sync,"id"),identifier],|r|r.get(0))?;
            if state=="complete" {return Ok(json!({"snapshot":sync["snapshot"],"current_snapshot":repository["snapshot"],"replayed":true}));}
            if state!="pending" || repository["snapshot"]!=sync["expected_snapshot"] {return Err(Error::conflict("A newer sync finished first"));}
            let before=heads(db,identifier)?;
            let changed:Vec<String>=documents.iter().filter(|d|before.get(&d.path)!=Some(&d.sha)).map(|d|d.path.clone()).collect();
            let present:BTreeSet<&str>=documents.iter().map(|d|d.path.as_str()).collect();
            let removed:Vec<_>=before.keys().filter(|path|!present.contains(path.as_str())).cloned().collect();
            db.execute("DELETE FROM repo_heads WHERE repository_id=?",[identifier])?;
            let mut insert_head=db.prepare_cached("INSERT INTO repo_heads VALUES(?,?,?)")?;
            for document in &documents {insert_head.execute(params![identifier,document.path,document.sha])?;}
            drop(insert_head);
            db.execute("INSERT OR IGNORE INTO repo_snapshots VALUES(?,?,?,?,?)",params![identifier,s(&sync,"snapshot"),s(&sync,"manifest"),s(&sync,"metadata"),now()])?;
            db.execute("UPDATE repo_projects SET snapshot=?,generation=generation+1,metadata=?,updated_at=? WHERE id=?",params![s(&sync,"snapshot"),s(&sync,"metadata"),now(),identifier])?;
            db.execute("UPDATE repo_syncs SET state='complete' WHERE id=?",[s(&sync,"id")])?;
            invalidate(db,identifier)?;
            for path in changed.iter().chain(&removed) {crate::invalidation::file_changed(db,&p.workspace,identifier,path,"repository_commit")?;crate::observer::forget_source(db,&p.workspace,identifier,&format!("file:{path}"))?;}
            let changed_paths:BTreeSet<&str>=changed.iter().map(String::as_str).collect();
            for document in documents.iter().filter(|d|changed_paths.contains(d.path.as_str())) {crate::invalidation::record_file_chunks(db,&p.workspace,identifier,&document.path,&document.sha)?;}
            event(db,identifier,"synced",json!({"snapshot":sync["snapshot"],"changed":changed,"removed":removed}))?;
            // Every commit yields a graph episode labelled with its commit identity.
            let metadata:Value=serde_json::from_str(s(&sync,"metadata")).unwrap_or(Value::Null);
            let projection=crate::jobs::enqueue_projection(db,identifier,p,s(&sync,"snapshot"),&metadata)?;
            // A head set that was already projected (a file edited, then restored) gets no new projection to clear the markers above.
            crate::graph_projection::clear_stale_when_current(db,identifier)?;
            let mut result=json!({"snapshot":sync["snapshot"],"changed":changed,"removed":removed,"files":documents.len(),"replayed":false,"projection_job":projection});
            let (limited,skipped)=analysis_report(db,identifier)?;
            if !limited.is_empty() {result["limited"]=json!(limited);}
            if !skipped.is_empty() {result["skipped"]=json!(skipped);}
            Ok(result)
        })
    }
}

/// Connector documents whose sharing policy names `credential` exactly: an
/// entry of the document's ACL credential list, or a provider-side principal
/// (`am_source_permissions`) that maps to one of the credential's verified
/// provider identities (of the kind's identity provider,
/// `connectors::identity_provider`). Both are joins on the identifier, never
/// text search over the JSON, so `cred-a` does not match `cred-ab`.
pub fn credential_sources(
    db: &Connection,
    workspace: &str,
    credential: &str,
) -> Result<Vec<String>> {
    let mut sources = BTreeSet::new();
    for row in rows(
        db,
        "SELECT d.connector_id,d.id FROM am_connector_docs d \
         JOIN am_connectors c ON c.id=d.connector_id \
         JOIN repo_projects r ON r.id=c.repository_id \
         JOIN json_each(d.acl,'$.credentials') reader \
         WHERE r.workspace_id=? AND d.present=1 AND reader.value=?",
        params![workspace, credential],
    )? {
        sources.insert(crate::invalidation::connector_document_id(
            s(&row, "connector_id"),
            s(&row, "id"),
        ));
    }
    for row in rows(
        db,
        "SELECT DISTINCT d.connector_id,d.id FROM am_connector_docs d \
         JOIN am_connectors c ON c.id=d.connector_id \
         JOIN repo_projects r ON r.id=c.repository_id \
         JOIN am_source_permissions p ON p.connector_id=d.connector_id AND p.source_id=d.id \
         JOIN json_each(p.principals) grant_ \
         JOIN am_provider_identities i ON i.workspace_id=r.workspace_id AND i.provider=coalesce(json_extract(?,'$.\"'||c.kind||'\"'),c.kind) AND i.credential_id=? \
         LEFT JOIN am_provider_emails e ON e.workspace_id=i.workspace_id AND e.provider=i.provider AND e.external_id=i.external_id \
         LEFT JOIN am_oauth_accounts o ON o.workspace_id=i.workspace_id AND o.provider=i.provider AND o.external_id=i.external_id AND o.state='active' \
         WHERE r.workspace_id=? AND d.present=1 AND ( \
            (json_extract(grant_.value,'$.type')='user' AND json_extract(grant_.value,'$.id')=i.external_id) OR \
            (json_extract(grant_.value,'$.type')='email' AND coalesce(json_extract(grant_.value,'$.email'),'')!='' \
             AND lower(json_extract(grant_.value,'$.email')) IN (lower(coalesce(e.email,'')),lower(coalesce(o.email,'')))))",
        params![
            crate::connectors::identity_providers().to_string(),
            credential,
            workspace
        ],
    )? {
        sources.insert(crate::invalidation::connector_document_id(
            s(&row, "connector_id"),
            s(&row, "id"),
        ));
    }
    Ok(sources.into_iter().collect())
}

/// The store format this program reads ([`require_reader`]). Format 1 is
/// Graphicle 0.1.3's: the first program that checks the format when it opens
/// a store, and one that honours the `credential_requirements` contract
/// ([`required_capabilities`]).
pub const READER_FORMAT: i64 = 1;

/// The formats a store requires of the programs that open it, by reason.
const READERS: &str = "CREATE TABLE IF NOT EXISTS store_readers (
 reason TEXT PRIMARY KEY, format INTEGER NOT NULL, version TEXT NOT NULL,
 updated_at TEXT NOT NULL);";

/// Record, under `reason`, that a program must read store format `format`
/// (first released in Graphicle `version`) to honour what this store holds,
/// for example restrictions an older program would not enforce. From
/// Graphicle 0.1.3 on, a program that reads only an older format refuses to
/// open the store ([`Store::open`]) instead of ignoring them; programs before
/// 0.1.3 do not check. Writes only when the record changes.
pub fn require_reader(db: &Connection, reason: &str, format: i64, version: &str) -> Result<()> {
    let current: Option<(i64, String)> = db
        .query_row(
            "SELECT format,version FROM store_readers WHERE reason=?",
            [reason],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if current
        .as_ref()
        .is_some_and(|(f, v)| *f == format && v == version)
    {
        return Ok(());
    }
    db.execute(
        "INSERT INTO store_readers(reason,format,version,updated_at) VALUES(?,?,?,?)
         ON CONFLICT(reason) DO UPDATE SET format=excluded.format,version=excluded.version,updated_at=excluded.updated_at",
        params![reason, format, version, now()],
    )?;
    Ok(())
}

/// Refuse a store that requires a newer store format than this program reads
/// ([`require_reader`]).
fn check_reader(db: &Connection) -> Result<()> {
    let declared: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='store_readers')",
        [],
        |r| r.get(0),
    )?;
    if !declared {
        return Ok(());
    }
    let needed: Option<(i64, String)> = db
        .query_row(
            "SELECT format,version FROM store_readers ORDER BY format DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match needed {
        Some((format, version)) if format > READER_FORMAT => Err(Error::bad(format!(
            "This store needs Graphicle {version} or newer (store format {format}); this is Graphicle {} (store format {READER_FORMAT}). Open it with Graphicle {version} or newer.",
            env!("CARGO_PKG_VERSION")
        ))),
        _ => Ok(()),
    }
}

/// Additive column migration for tables created by an earlier schema:
/// `CREATE TABLE IF NOT EXISTS` never alters an existing table, so a column
/// added later is appended here when `pragma_table_info` lacks it.
pub fn ensure_column(db: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    let present = rows(db, &format!("PRAGMA table_info({table})"), [])?
        .iter()
        .any(|row| s(row, "name") == column);
    if !present {
        db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"))?;
    }
    Ok(())
}

/// The principal `hash` authenticates in `workspace`, whatever its role
/// grants: the owner capability, a usable workspace credential (not revoked,
/// expired or refused by its member's sign-in rules or the store's
/// credential requirements), or a token an installed extension knows, such as
/// a browser session. No permission is checked: admission and metering resolve their
/// caller with this, so a credential is checked and counted whichever
/// permissions its role holds ([`authorize_hash`] checks one).
pub fn authenticate_hash(db: &Connection, workspace: &str, hash: &str) -> Result<Principal> {
    let owner: String = db
        .query_row(
            "SELECT capability_hash FROM workspaces WHERE id=?",
            [workspace],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(Error::denied)?;
    let principal = if owner.as_bytes().ct_eq(hash.as_bytes()).into() {
        Principal {
            workspace: workspace.into(),
            hash: hash.into(),
            credential: "owner".into(),
            role: "owner".into(),
        }
    } else {
        let row = match one(
            db,
            "SELECT id,role,expires_at,revoked_at,capability_hash FROM credentials WHERE workspace_id=? AND capability_hash=?",
            params![workspace, hash],
        )? {
            Some(row) => row,
            // Not a workspace credential: an installed extension may know the
            // token, such as a browser session.
            None => crate::extension::installed()
                .session_credential(db, workspace, hash)?
                .ok_or_else(Error::denied)?,
        };
        if !row["revoked_at"].is_null() {
            return Err(Error::denied());
        }
        // A member's credential follows the member and team sign-in on any
        // store that has the team tables, whichever edition reads it, and a
        // credential the store requires capabilities for needs a program
        // that has them ([`required_capabilities`]).
        if !crate::store_compat::credential_allowed(db, s(&row, "id"))?
            || !crate::extension::installed().credential_allowed(db, s(&row, "id"))?
            || !requirements_met(db, s(&row, "id"))?
        {
            return Err(Error::denied());
        }
        if let Some(expiry) = row["expires_at"].as_str()
            && graphicle_memory::temporal::instant(expiry).map_err(Error::bad)?
                <= chrono::Utc::now().fixed_offset()
        {
            return Err(Error::denied());
        }
        Principal {
            workspace: workspace.into(),
            hash: s(&row, "capability_hash").into(),
            credential: s(&row, "id").into(),
            role: s(&row, "role").into(),
        }
    };
    Ok(principal)
}
/// The principal `hash` authenticates in `workspace` ([`authenticate_hash`]),
/// once its role grants `permission` and every installed extension allows it.
pub fn authorize_hash(
    db: &Connection,
    workspace: &str,
    hash: &str,
    permission: &str,
) -> Result<Principal> {
    let principal = authenticate_hash(db, workspace, hash)?;
    if !role_grants(db, &principal, permission)? {
        return Err(Error::denied());
    }
    extensions_allow(db, &principal, permission)?;
    Ok(principal)
}
/// Whether the role of `p` grants `permission`: a core role as
/// [`Principal::permits`] says, and a role the core does not define only as
/// the installed extension that defines it says (`Extension::role_permits`).
pub fn role_grants(db: &Connection, p: &Principal, permission: &str) -> Result<bool> {
    if p.permits(permission) {
        return Ok(true);
    }
    if p.role == "owner" || CREDENTIAL_ROLES.contains(&p.role.as_str()) {
        return Ok(false);
    }
    Ok(
        crate::extension::installed().role_permits(db, &p.workspace, &p.role, permission)?
            == Some(true),
    )
}
/// Every installed extension allows `p` the permission
/// (`Extension::authorize`); the owner capability is never asked about.
fn extensions_allow(db: &Connection, p: &Principal, permission: &str) -> Result<()> {
    if p.role == "owner" {
        return Ok(());
    }
    crate::extension::installed().authorize(
        db,
        p,
        &mut crate::extension::AccessRequest::new(crate::extension::Access::Permission(permission)),
    )
}
/// Whether `p` holds `permission` as [`authorize_hash`] decides it, for checks
/// inside an authorized transaction that need a further permission.
pub fn permitted(db: &Connection, p: &Principal, permission: &str) -> Result<bool> {
    Ok(role_grants(db, p, permission)? && extensions_allow(db, p, permission).is_ok())
}
/// The `build.info` capabilities a program needs to honour `credential`,
/// sorted. A store may hold a table or view `credential_requirements`
/// (`credential_id`, `capability`), which an installed extension keeps for
/// the credentials it restricts in ways the core cannot enforce, each with a
/// capability that extension sets. A program without the capability refuses
/// the credential ([`authorize_hash`]) instead of granting its role in full,
/// so the restriction holds, failing closed, wherever the store or a
/// portable backup of it is opened; a program with it leaves the credential
/// to its extensions' hooks. Empty when the store declares none.
pub fn required_capabilities(db: &Connection, credential: &str) -> Result<Vec<String>> {
    let declared: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name='credential_requirements')",
        [],
        |r| r.get(0),
    )?;
    if !declared {
        return Ok(Vec::new());
    }
    Ok(rows(
        db,
        "SELECT DISTINCT capability FROM credential_requirements WHERE credential_id=? ORDER BY capability",
        [credential],
    )?
    .iter()
    .map(|row| s(row, "capability").to_string())
    .collect())
}
/// Whether this program has every capability `credential` requires
/// ([`required_capabilities`]).
fn requirements_met(db: &Connection, credential: &str) -> Result<bool> {
    let required = required_capabilities(db, credential)?;
    if required.is_empty() {
        return Ok(true);
    }
    let capabilities = crate::core_capabilities::capabilities();
    Ok(required
        .iter()
        .all(|capability| capabilities[capability.as_str()] == true))
}
pub fn repo(db: &Connection, p: &Principal, identifier: &str) -> Result<Value> {
    let mut row = one(
        db,
        "SELECT * FROM repo_projects WHERE id=? AND workspace_id=?",
        params![identifier, p.workspace],
    )?
    .ok_or_else(|| Error::bad("Repository not found in this workspace"))?;
    if !crate::graph_projection::repo_readable(db, p, identifier)? {
        return Err(Error::denied());
    }
    decode(&mut row, "metadata")?;
    Ok(row)
}
pub fn heads(db: &Connection, repository: &str) -> Result<BTreeMap<String, String>> {
    Ok(rows(
        db,
        "SELECT path,sha256 FROM repo_heads WHERE repository_id=?",
        [repository],
    )?
    .iter()
    .map(|v| (s(v, "path").into(), s(v, "sha256").into()))
    .collect())
}
pub fn event(db: &Connection, repository: &str, kind: &str, value: Value) -> Result<()> {
    db.execute(
        "INSERT INTO repo_events(repository_id,kind,data,created_at) VALUES(?,?,?,?)",
        params![repository, kind, value.to_string(), now()],
    )?;
    Ok(())
}
pub fn invalidate(db: &Connection, repository: &str) -> Result<()> {
    crate::projection::enqueue(db, repository, "source_or_permission_state_changed")?;
    graphicle_memory::cache::clear(repository).map_err(Error::bad)?;
    db.execute(
        "DELETE FROM am_query_cache WHERE repository_id=?",
        [repository],
    )?;
    Ok(())
}

#[cfg(test)]
mod analysis_row_tests {
    use super::analysis_row;
    use serde_json::{Value, json};

    const REVISIONS: [&str; 2] = ["complete-revision", "limited-revision"];

    #[test]
    fn a_complete_analysis_keeps_the_structure_revision() {
        let limits = crate::limits::Limits::default().analysis_request();
        let (revision, data) =
            analysis_row("main.py", "def main():\n    work()\n", &limits, REVISIONS).unwrap();
        let analysis: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(revision, REVISIONS[0]);
        assert_eq!(analysis["complete"], true, "{analysis}");
        assert!(analysis.get("limited").is_none());
    }

    #[test]
    fn a_limited_analysis_is_kept_under_the_limited_revision() {
        let (revision, data) = analysis_row(
            "main.py",
            "def main():\n    one()\n    two()\n",
            &json!({"calls":1}),
            REVISIONS,
        )
        .unwrap();
        let analysis: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(revision, REVISIONS[1]);
        assert_eq!(analysis["limited"], "calls");
        assert_eq!(analysis["calls"].as_array().unwrap().len(), 1);
    }

    /// Any other failure of a file's analysis records the file as skipped,
    /// with the reason, instead of failing the sync (here the analysis
    /// refuses its request).
    #[test]
    fn a_failed_analysis_is_recorded_as_skipped_with_its_reason() {
        let (revision, data) = analysis_row(
            "main.py",
            "def main():\n    work()\n",
            &json!({"calls":0}),
            REVISIONS,
        )
        .unwrap();
        let analysis: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(revision, REVISIONS[1]);
        assert_eq!(
            analysis["skipped"],
            "Analysis limit calls must be a positive integer"
        );
        assert_eq!(analysis["complete"], false);
        assert_eq!(analysis["symbols"], json!([]));
        assert_eq!(
            analysis["issues"][0],
            "Not analysed: Analysis limit calls must be a positive integer; searchable as text only"
        );
    }

    /// A failure that is not about the file is not cached as the file's.
    #[test]
    fn installation_failures_fail_and_transient_ones_are_retried() {
        assert!(super::parser_unavailable(
            "Bundled parser unavailable: rust"
        ));
        assert!(super::parser_unavailable("Parser grammar is incompatible"));
        assert!(!super::parser_unavailable("Source contains syntax errors"));
        assert!(super::transient_failure("Source parsing did not complete"));
        assert!(super::transient_failure(
            "Native operation failed; no result accepted"
        ));
        assert!(super::transient_failure(
            "Source analysis failed: parse timeout exceeded"
        ));
        assert!(!super::transient_failure(
            "Analysis limit calls must be a positive integer"
        ));
    }
}

#[cfg(test)]
mod lock_site_tests {
    use super::crate_relative;

    #[test]
    fn lock_report_sites_are_relative_to_their_crate() {
        // Paths as rustc gives them on this system: from `/`, or `C:\`.
        let separator = std::path::MAIN_SEPARATOR_STR;
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        let absolute = |parts: &[&str]| format!("{root}{}", parts.join(separator));
        let relative = |parts: &[&str]| parts.join(separator);
        let manifest = format!("{}/src/jobs.rs", env!("CARGO_MANIFEST_DIR"));
        assert_eq!(crate_relative(&manifest), "src/jobs.rs");
        assert_eq!(crate_relative("src/jobs.rs"), "src/jobs.rs");
        assert_eq!(
            crate_relative(&absolute(&[
                "builds", "pro", "src", "service", "team", "oidc.rs"
            ])),
            relative(&["src", "service", "team", "oidc.rs"])
        );
        let registry = [
            "home",
            "ci",
            ".cargo",
            "registry",
            "src",
            "index.example-1",
            "dep-1.0",
            "src",
            "lib.rs",
        ];
        assert_eq!(
            crate_relative(&absolute(&registry)),
            relative(&["src", "lib.rs"])
        );
    }
}

#[cfg(test)]
mod client_path_tests {
    use super::client_path;

    #[test]
    fn client_paths_keep_the_relative_path_rules() {
        assert_eq!(client_path("src/app.py").unwrap(), "src/app.py");
        assert!(client_path("../x").is_err() && client_path("/etc/passwd").is_err());
        #[cfg(windows)]
        {
            assert_eq!(client_path(r"src\app.py").unwrap(), "src/app.py");
            for refused in [r"..\x", r"C:\x", "C:x", r"\\server\share\x", r"\x", r"a\\b"] {
                assert!(client_path(refused).is_err(), "{refused}");
            }
        }
        #[cfg(not(windows))]
        {
            assert!(client_path(r"src\app.py").is_err());
            assert_eq!(client_path("C:x").unwrap(), "C:x");
        }
    }
}

#[cfg(test)]
mod reader_format_tests {
    use super::{READER_FORMAT, Store, require_reader};

    /// A store that requires a newer store format is refused at open,
    /// before anything in it changes; one this program reads opens.
    #[test]
    fn a_store_that_needs_a_newer_program_is_refused_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let db = store.connect().unwrap();
        require_reader(&db, "restrictions", READER_FORMAT, "0.1.3").unwrap();
        drop(db);
        assert!(Store::open(dir.path()).is_ok());
        let db = store.connect().unwrap();
        require_reader(&db, "later", READER_FORMAT + 1, "9.9.9").unwrap();
        let changed: i64 = db
            .query_row("SELECT total_changes()", [], |r| r.get(0))
            .unwrap();
        // Unchanged records are not rewritten.
        require_reader(&db, "later", READER_FORMAT + 1, "9.9.9").unwrap();
        let again: i64 = db
            .query_row("SELECT total_changes()", [], |r| r.get(0))
            .unwrap();
        assert_eq!(changed, again);
        drop(db);
        let error = Store::open(dir.path()).err().expect("refused");
        assert!(
            error
                .message
                .starts_with("This store needs Graphicle 9.9.9 or newer (store format 2)"),
            "{}",
            error.message
        );
        store
            .connect()
            .unwrap()
            .execute("DELETE FROM store_readers WHERE reason='later'", [])
            .unwrap();
        assert!(Store::open(dir.path()).is_ok());
    }
}

#[cfg(test)]
mod credential_requirement_tests {
    use super::{Store, required_capabilities, s};
    use serde_json::json;

    /// A credential the store requires a capability for is refused by a
    /// program without it (this Core build), also for a permission its role
    /// grants; one this program has leaves it usable, as are credentials the
    /// store names no requirement for and the owner capability.
    #[test]
    fn a_credential_needs_the_capabilities_the_store_requires_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let created = store.create_workspace("Requirements").unwrap();
        let (workspace, owner) = (s(&created, "id"), s(&created, "capability"));
        let grant = |label: &str| {
            let granted = store
                .grant(workspace, owner, &json!({"label":label,"role":"agent"}))
                .unwrap();
            (
                s(&granted, "id").to_string(),
                s(&granted, "capability").to_string(),
            )
        };
        let (restricted, restricted_token) = grant("restricted");
        let (_, plain_token) = grant("plain");
        let usable = |token: &str, permission: &str| {
            store
                .authorized(workspace, token, permission, |_, _| Ok(()))
                .is_ok()
        };
        let db = store.connect().unwrap();
        assert!(required_capabilities(&db, &restricted).unwrap().is_empty());
        db.execute_batch(
            "CREATE TABLE credential_requirements(credential_id TEXT, capability TEXT)",
        )
        .unwrap();
        // A capability every build has.
        db.execute(
            "INSERT INTO credential_requirements VALUES(?,'memory')",
            [&restricted],
        )
        .unwrap();
        assert!(usable(&restricted_token, "propose"));
        // One no installed extension sets.
        db.execute(
            "INSERT INTO credential_requirements VALUES(?,'unknown_restriction')",
            [&restricted],
        )
        .unwrap();
        assert_eq!(
            required_capabilities(&db, &restricted).unwrap(),
            ["memory", "unknown_restriction"]
        );
        assert!(!usable(&restricted_token, "read"));
        assert!(!usable(&restricted_token, "propose"));
        assert!(usable(&plain_token, "propose"));
        assert!(usable(owner, "admin"));
        db.execute("DELETE FROM credential_requirements", [])
            .unwrap();
        assert!(usable(&restricted_token, "propose"));
    }
}
