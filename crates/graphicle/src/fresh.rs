//! Freshness: answers come from what is in
//! the folder now. Before `ask`, `context`, `status` and every MCP tool call
//! on a folder-backed project, [`ensure`] runs a cheap change check. A few
//! changed files ([`REFRESH_LIMIT`]) are synced before the answer, through
//! the same sync `graphicle sync` runs, and the code graph follows as it
//! does after `graphicle sync`; more changes, or a folder whose sync is slow, give a
//! stale note instead. `graphicle sync . --quiet` ([`sync_quiet`]), which the
//! git hooks run, does nothing when nothing changed.
//!
//! The check ([`check`]):
//! - every completed folder sync records, with its folder
//!   (`repo_folders.freshness`), the snapshot it produced, the time its scan
//!   started and, in a git repository, a signature of the folder's git
//!   state: HEAD and `git status --porcelain` with each listed file's size
//!   and times ([`mark`], [`record`]);
//! - in a git repository whose signature still matches, nothing changed:
//!   one `git status`;
//! - otherwise a bounded scan with the sync's own walk counts the files
//!   whose inode changed after that time (the change time, which every write
//!   moves and no tool can set back) and the synced files that are gone.
//!
//! Conversation-only projects, projects created over HTTP and PR scratch
//! projects have no folder and are never touched. `freshness` in the
//! configuration (`auto`, the default, or `off`) or `GRAPHICLE_FRESHNESS`
//! turns it off. Machine output gains a `folder_freshness` object only when
//! a refresh synced something or the data is stale. (`freshness` was taken:
//! `context`, `status` and the graph replies already carry the reviewed
//! memory's staleness under that name.)
use crate::platform::CanonicalExt;
use crate::{
    Error, Result,
    local::{self, Session},
    model,
    store::{Store, one, s},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::Metadata,
    path::{Path, PathBuf},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// At most this many changed files are synced before an answer.
pub const REFRESH_LIMIT: usize = 200;
/// What a refresh may take, the code graph included, before the answer
/// goes on without waiting for it.
pub const BUDGET: Duration = Duration::from_secs(2);
/// A folder whose last sync spent longer than this reading its files is
/// not refreshed before an answer (its refresh would not fit [`BUDGET`]).
pub const SLOW_SCAN: Duration = Duration::from_millis(1500);
/// A check is reused this long within one process (MCP tool calls).
pub const CACHE_TTL: Duration = Duration::from_secs(2);
/// The freshness scan stops after this many entries, or after
/// [`SCAN_TIME`], and reports the changes as unknown.
const MAX_SCAN: usize = 50_000;
const SCAN_TIME: Duration = Duration::from_secs(2);
/// `git status` is given this long.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// File times may lag the clock read before a scan by the kernel's tick
/// (Linux records coarse times): a file whose inode changed less than this
/// before a sync started may have changed after it, so the next check
/// compares its content with the synced one.
const MARGIN_NS: i64 = 20_000_000;
/// `graphicle sync --quiet` waits this long for another process syncing the
/// same folder before it leaves the work to that process; a foreground sync
/// waits as long before it syncs anyway ([`locked`]).
const QUIET_WAIT: Duration = Duration::from_secs(120);
/// A folder whose last sync predates the record of its sync time is
/// refreshed before an answer only up to this many synced files; a larger
/// one gets the stale note until a sync records its time.
const UNTIMED_FILES: usize = 2000;
/// Changed paths kept as examples.
const SAMPLE: usize = 10;

/// `freshness` in the configuration: `auto` checks and refreshes before
/// answers, `off` never checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Auto,
    Off,
}

impl Mode {
    /// `auto` or `off`, in any case.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "off" => Ok(Self::Off),
            _ => Err(Error::bad("freshness is auto or off")),
        }
    }
}

/// The mode in effect: `GRAPHICLE_FRESHNESS` (`auto` or `off`), else the
/// configuration's `freshness`, else `auto`.
pub fn mode(config: &model::Config) -> Mode {
    std::env::var("GRAPHICLE_FRESHNESS")
        .ok()
        .and_then(|value| Mode::parse(&value).ok())
        .or(config.freshness)
        .unwrap_or_default()
}

/// Adds the `freshness` column that holds each folder's record. Called when
/// a store opens.
pub(crate) fn migrate(db: &Connection) -> Result<()> {
    crate::store::ensure_column(db, "repo_folders", "freshness", "TEXT")
}

static BACKGROUND_BUILDS: AtomicBool = AtomicBool::new(false);

/// Lets this process start `graphicle build-graph` for a refreshed project
/// ([`crate::cli::main_with`] calls it): only the program itself, never a
/// test binary running the command line in-process.
pub fn allow_background_builds() {
    BACKGROUND_BUILDS.store(true, Ordering::Relaxed);
}

/// What the next check compares with, taken when a sync starts reading its
/// folder ([`mark`]) and stored when it completes ([`record`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    v: u32,
    /// The snapshot the sync produced: the record holds only while the
    /// project still has it.
    snapshot: String,
    /// Wall-clock nanoseconds when the sync started reading the folder.
    since_ns: i64,
    /// The git signature then ([`git_state`]), in a git repository.
    #[serde(default)]
    git: Option<String>,
    /// How long that sync spent reading the folder.
    #[serde(default)]
    scan_ms: Option<u64>,
    /// The build whose conversions the snapshot's documents hold
    /// ([`local::converter`]); missing in records of earlier builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    converter: Option<String>,
}

/// A sync's view of its folder before it reads it. See [`mark`].
#[derive(Debug)]
pub struct Mark {
    since_ns: i64,
    git: Option<String>,
    started: Instant,
    scan_ms: Option<u64>,
}

/// The mark a folder sync takes before it reads `folder` (canonical): the
/// time, then the git signature. `store` is the data directory, left out of
/// the signature when it is inside the folder.
pub fn mark(folder: &Path, store: &Path) -> Mark {
    let since_ns = now_ns();
    let git = git_state(folder, store).map(|state| state.signature);
    Mark {
        since_ns,
        git,
        started: Instant::now(),
        scan_ms: None,
    }
}

impl Mark {
    /// The sync has read and digested its files.
    pub(crate) fn scanned(&mut self) {
        self.scan_ms = Some(self.started.elapsed().as_millis() as u64);
    }
}

/// Store `mark` as the record of `folder`'s completed sync into `snapshot`,
/// in the sync's last transaction, after the folder's alias is recorded.
pub(crate) fn record(
    db: &Connection,
    workspace: &str,
    folder: &Path,
    mark: &Mark,
    snapshot: &str,
) -> Result<()> {
    let record = Record {
        v: 1,
        snapshot: snapshot.to_string(),
        since_ns: mark.since_ns,
        git: mark.git.clone(),
        scan_ms: mark.scan_ms,
        // Every document of the snapshot was converted by this build, or
        // kept from a sync whose record named it ([`converted_here`]).
        converter: Some(local::converter()),
    };
    write_record(db, workspace, folder, &record)
}

/// Whether the last completed sync of `folder` (canonical) left `snapshot`
/// with this build's document conversions ([`local::converter`]): a
/// document whose original is unchanged would convert to the text that
/// snapshot holds for it.
pub(crate) fn converted_here(
    db: &Connection,
    workspace: &str,
    folder: &Path,
    snapshot: &str,
) -> Result<bool> {
    let record: Option<String> = db
        .query_row(
            "SELECT freshness FROM repo_folders WHERE workspace_id=? AND folder_key=?",
            params![workspace, crate::folders::folder_key(folder)],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    Ok(record
        .and_then(|text| serde_json::from_str::<Record>(&text).ok())
        .is_some_and(|record| {
            record.v == 1
                && !snapshot.is_empty()
                && record.snapshot == snapshot
                && record.converter.as_deref() == Some(local::converter().as_str())
        }))
}

fn write_record(db: &Connection, workspace: &str, folder: &Path, record: &Record) -> Result<()> {
    db.execute(
        "UPDATE repo_folders SET freshness=? WHERE workspace_id=? AND folder_key=?",
        params![
            serde_json::to_string(record)?,
            workspace,
            crate::folders::folder_key(folder)
        ],
    )?;
    Ok(())
}

/// Where a project stands against its folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No folder: a conversation-only project, one created over HTTP, or a
    /// PR scratch project. Never touched.
    NotFolder,
    /// The folder it was last synced from is gone.
    Missing,
    /// Nothing changed since the last sync.
    Fresh,
    /// Files changed since the last sync.
    Changed,
}

/// A change check of one project's folder.
#[derive(Clone, Debug)]
pub struct Check {
    pub project: String,
    /// The folder checked: the one the project was last synced from.
    pub folder: Option<PathBuf>,
    pub state: State,
    /// Files changed, added or removed since the last sync (0 when fresh);
    /// `None` when the folder was too large or too slow to scan.
    pub changed: Option<usize>,
    /// Up to ten of those files, folder-relative.
    pub sample: Vec<String>,
    /// `git` when the git signature decided, `scan` when the folder was
    /// scanned, empty without a folder.
    pub method: &'static str,
    /// When the folder's last sync completed (RFC 3339).
    pub last_sync: Option<String>,
    /// What the check itself took.
    pub elapsed: Duration,
    /// How long the last sync spent reading the folder, when recorded.
    scan_ms: Option<u64>,
    /// How many files the project has from its last sync (scans only).
    files: usize,
    /// The record, when it still describes the project's snapshot.
    record: Option<Record>,
    /// The time this check's scan started and the git signature it saw.
    since_ns: i64,
    git: Option<String>,
}

impl Check {
    fn without(project: &str, state: State, folder: Option<PathBuf>) -> Self {
        Self {
            project: project.to_string(),
            folder,
            state,
            changed: None,
            sample: Vec::new(),
            method: "",
            last_sync: None,
            elapsed: Duration::ZERO,
            scan_ms: None,
            files: 0,
            record: None,
            since_ns: 0,
            git: None,
        }
    }

    /// The check as `status` shows it: `{state, folder, last_sync, changed,
    /// sample, method, seconds}`, `state` being `not_folder`, `missing`,
    /// `fresh` or `changed`.
    pub fn to_json(&self) -> Value {
        json!({
            "state": match self.state {
                State::NotFolder => "not_folder",
                State::Missing => "missing",
                State::Fresh => "fresh",
                State::Changed => "changed",
            },
            "folder": self.folder,
            "last_sync": self.last_sync,
            "changed": self.changed,
            "sample": self.sample,
            "method": self.method,
            "seconds": self.elapsed.as_secs_f64(),
        })
    }
}

/// The folder row a check reads.
struct Baseline {
    folder: PathBuf,
    synced_at: String,
    record: Option<Record>,
    snapshot: String,
    /// Files the last sync left out as binary (a `.DS_Store`, an image):
    /// rewriting them changes nothing a sync reads.
    binaries: BTreeSet<String>,
}

/// Check `project` against the folder it was last synced from. Reads only;
/// see the module documentation for how.
pub fn check(store: &Store, session: &Session, project: &str) -> Result<Check> {
    check_at(store, session, project, None)
}

/// [`check`] against `folder` (canonical), one of the project's folders,
/// instead of the one it was last synced from.
pub fn check_folder(
    store: &Store,
    session: &Session,
    project: &str,
    folder: &Path,
) -> Result<Check> {
    check_at(store, session, project, Some(folder))
}

fn check_at(
    store: &Store,
    session: &Session,
    project: &str,
    folder: Option<&Path>,
) -> Result<Check> {
    let started = Instant::now();
    let baseline = store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
        let repository = crate::store::repo(db, p, project)?;
        if crate::folders::is_pr_scratch(db, project)? {
            return Ok(None);
        }
        let row = match folder {
            Some(folder) => one(
                db,
                "SELECT path,updated_at,freshness FROM repo_folders WHERE workspace_id=? AND folder_key=? AND repository_id=? AND path IS NOT NULL",
                params![p.workspace, crate::folders::folder_key(folder), project],
            )?,
            None => one(
                db,
                "SELECT path,updated_at,freshness FROM repo_folders WHERE repository_id=? AND path IS NOT NULL ORDER BY updated_at DESC,path LIMIT 1",
                [project],
            )?,
        };
        Ok(row.map(|row| Baseline {
            folder: PathBuf::from(s(&row, "path")),
            synced_at: s(&row, "updated_at").to_string(),
            record: row["freshness"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Record>(text).ok())
                .filter(|record| record.v == 1),
            snapshot: s(&repository, "snapshot").to_string(),
            binaries: repository["metadata"]["excluded"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["reason"] == "unsupported_binary")
                .map(|item| s(item, "path").to_string())
                .collect(),
        }))
    })?;
    let Some(baseline) = baseline else {
        return Ok(Check::without(project, State::NotFolder, None));
    };
    let mut check = Check::without(project, State::Changed, Some(baseline.folder.clone()));
    check.last_sync = Some(baseline.synced_at.clone());
    check.scan_ms = baseline.record.as_ref().and_then(|record| record.scan_ms);
    if !baseline.folder.is_dir() {
        check.state = State::Missing;
        check.elapsed = started.elapsed();
        return Ok(check);
    }
    let record = baseline
        .record
        .filter(|record| record.snapshot == baseline.snapshot);
    check.since_ns = now_ns();
    check.git = git_state(&baseline.folder, &store.directory).map(|state| state.signature);
    if let (Some(record), Some(now)) = (&record, &check.git)
        && record.git.as_deref() == Some(now.as_str())
    {
        check.state = State::Fresh;
        check.changed = Some(0);
        check.method = "git";
        check.record = Some(record.clone());
        check.elapsed = started.elapsed();
        return Ok(check);
    }
    // Changed since: the record's scan, else (a folder synced before
    // records existed, or a project synced from elsewhere since) the
    // folder's last completed sync.
    let since = match &record {
        Some(record) => record.since_ns,
        None => instant_ns(&baseline.synced_at).unwrap_or(0),
    };
    let heads =
        store.authorized_read(&session.workspace, &session.capability, "read", |db, _| {
            crate::store::heads(db, project)
        })?;
    check.method = "scan";
    check.record = record;
    check.files = heads.len();
    match scan(
        &baseline.folder,
        &store.directory,
        since,
        &heads,
        &baseline.binaries,
    ) {
        Some(changed) => {
            check.changed = Some(changed.len());
            check.sample = changed.iter().take(SAMPLE).cloned().collect();
            if changed.is_empty() {
                check.state = State::Fresh;
            }
        }
        None => check.changed = None,
    }
    check.elapsed = started.elapsed();
    Ok(check)
}

/// The files under `folder` (the sync's walk, [`local::folder_entries`]) whose inode changed after
/// `since_ns`, and the synced files (`heads`, path to SHA-256) that are gone;
/// `None` when the folder holds more than [`MAX_SCAN`] entries or the scan
/// takes longer than [`SCAN_TIME`]. A file whose inode changed within
/// [`MARGIN_NS`] before `since_ns` counts when its content differs from the
/// synced one. Files the sync never reads (symlinks, credentials, and the
/// `binaries` it left out last time) count only when they replaced a synced
/// file; the data directory, when it is inside the folder, never counts.
fn scan(
    folder: &Path,
    store: &Path,
    since_ns: i64,
    heads: &BTreeMap<String, String>,
    binaries: &BTreeSet<String>,
) -> Option<BTreeSet<String>> {
    let started = Instant::now();
    let store = store.canonical().ok();
    let inside = store
        .as_deref()
        .and_then(|dir| dir.strip_prefix(folder).ok());
    let threshold = since_ns.saturating_sub(MARGIN_NS);
    let mut seen = BTreeSet::new();
    let mut changed = BTreeSet::new();
    for (count, entry) in local::folder_entries(folder).enumerate() {
        if count > MAX_SCAN || started.elapsed() > SCAN_TIME {
            return None;
        }
        // An unreadable entry fails the sync itself; a check passes over it.
        let Ok(entry) = entry else { continue };
        let Some(kind) = entry.file_type() else {
            continue;
        };
        // The folder itself or a directory; a tracked file the walk left
        // out comes at depth 0 too, and is scanned.
        if entry.path() == folder || kind.is_dir() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(folder) else {
            continue;
        };
        if inside.is_some_and(|inside| relative.starts_with(inside)) {
            continue;
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        let name = entry.file_name().to_str().unwrap_or("");
        let read = !kind.is_symlink() && !local::sensitive(name) && !binaries.contains(&relative);
        if read || heads.contains_key(&relative) {
            let newer = match std::fs::symlink_metadata(entry.path()) {
                Ok(meta) => {
                    let at = changed_ns(&meta);
                    at > since_ns
                        || (at > threshold
                            && !same_content(folder, &relative, name, heads.get(&relative)))
                }
                Err(_) => true,
            };
            if newer || !read {
                changed.insert(relative.clone());
            }
        }
        seen.insert(relative);
    }
    for head in heads.keys() {
        if !seen.contains(head) && !inside.is_some_and(|inside| Path::new(head).starts_with(inside))
        {
            changed.insert(head.clone());
        }
    }
    Some(changed)
}

/// Whether the file `relative` under `folder` reads, as a sync reads it
/// ([`local::sync_view`]: Graphicle's own lines and files left out), as
/// what was synced as `head` (its SHA-256), or is left out and was never
/// synced. A document (whose synced text is extracted), a file over the
/// sync's limit and an unreadable file never do.
fn same_content(folder: &Path, relative: &str, name: &str, head: Option<&String>) -> bool {
    if crate::documents::supported(name) {
        return false;
    }
    match (local::sync_view(folder, relative), head) {
        (Some(None), None) => true,
        (Some(Some(text)), Some(head)) => graphicle_memory::text::sha256(text.as_bytes()) == *head,
        _ => false,
    }
}

/// A folder's git state.
struct GitState {
    signature: String,
}

/// The git signature of `folder` (canonical): its commit, and each entry of
/// `git status --porcelain=v2` inside the folder with the listed file's size,
/// times and inode (a nested repository's or a submodule's files one by
/// one), leaving out what the sync never reads and the data directory
/// `store`. `None` outside a git repository, in a folder the repository
/// ignores (git lists none of its files, the sync reads them all), or when
/// git is missing, fails or takes longer than five seconds: the check then
/// scans.
fn git_state(folder: &Path, store: &Path) -> Option<GitState> {
    let root = folder
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())?;
    let prefix = folder
        .strip_prefix(root)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    let output = git(
        folder,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--ignored=matching",
            "--no-renames",
            "--ignore-submodules=none",
            "--",
            ".",
        ],
    )?;
    let (head, entries, ignored) = porcelain(&output, &prefix);
    if ignored {
        return None;
    }
    let inside = store
        .canonical()
        .ok()
        .and_then(|dir| dir.strip_prefix(folder).ok().map(Path::to_path_buf));
    let mut digest = Sha256::new();
    digest.update(head.as_bytes());
    digest.update([0]);
    let mut listed = 0usize;
    for (relative, record) in &entries {
        if inside
            .as_deref()
            .is_some_and(|inside| Path::new(relative).starts_with(inside))
        {
            continue;
        }
        digest.update(record);
        digest.update([0]);
        let path = folder.join(relative);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                // A nested repository or a submodule: git lists the folder,
                // the sync reads its files.
                for entry in local::folder_walk(&path).build().flatten() {
                    listed += 1;
                    if listed > MAX_SCAN {
                        return None;
                    }
                    if entry.file_type().is_some_and(|kind| !kind.is_dir())
                        && let Ok(meta) = std::fs::symlink_metadata(entry.path())
                    {
                        digest.update(entry.path().to_string_lossy().as_bytes());
                        digest.update(stat(&meta).as_bytes());
                    }
                }
            }
            Ok(meta) => digest.update(stat(&meta).as_bytes()),
            Err(_) => digest.update(b"-"),
        }
        digest.update([0]);
    }
    Some(GitState {
        signature: format!("{:x}", digest.finalize()),
    })
}

/// The commit and the entries of `git status --porcelain=v2 -z --branch
/// --ignored=matching` output, as (folder-relative path, record), and whether
/// the repository ignores the folder itself or a folder above it. Paths in
/// the output are relative to the repository; `prefix` is the folder's own
/// path in it ("" at its top level). Entries outside the folder, ignored
/// entries (the sync reads the same ignore files) and names the sync never
/// reads are left out.
fn porcelain(output: &[u8], prefix: &str) -> (String, Vec<(String, Vec<u8>)>, bool) {
    let mut head = String::new();
    let mut entries = Vec::new();
    let mut ignored = false;
    let mut records = output.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(oid) = record.strip_prefix(b"# branch.oid ") {
            head = String::from_utf8_lossy(oid).into_owned();
            continue;
        }
        // Ordinary (1), renamed (2: the original path follows as a record
        // of its own), unmerged (u), untracked (?) and ignored (!) entries.
        let fields = match record[0] {
            b'1' => 9,
            b'2' => 10,
            b'u' => 11,
            b'?' | b'!' => 2,
            _ => continue,
        };
        if record[0] == b'2' {
            records.next();
        }
        let Some(path) = record.splitn(fields, |byte| *byte == b' ').nth(fields - 1) else {
            continue;
        };
        let path = String::from_utf8_lossy(path);
        if record[0] == b'!' {
            let path = path.trim_end_matches('/');
            ignored |= !prefix.is_empty()
                && (prefix == path
                    || prefix
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('/')));
            continue;
        }
        let relative = if prefix.is_empty() {
            path.as_ref()
        } else {
            match path
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/'))
            {
                Some(rest) => rest,
                None => continue,
            }
        }
        .trim_end_matches('/');
        if relative.is_empty()
            || relative
                .split('/')
                .any(|part| local::SKIPPED_NAMES.contains(&part))
            || local::skipped_file(relative)
        {
            continue;
        }
        entries.push((relative.to_string(), record.to_vec()));
    }
    (head, entries, ignored)
}

/// Run git in `folder` and return its standard output, or `None` when it
/// cannot start, fails or runs past [`GIT_TIMEOUT`] (it is then killed). The
/// variables a git hook sets are removed, so git finds the folder's own
/// repository, and optional locks are off, so the check never writes the
/// index another git process may be using.
pub(crate) fn git(folder: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = std::process::Command::new("git");
    // Git refuses Windows' verbatim `\\?\` paths.
    command
        .arg("-C")
        .arg(crate::platform::simplified(folder.to_path_buf()))
        .args(args);
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ] {
        command.env_remove(variable);
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let child = command.spawn().ok()?;
    let pid = child.id();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(child.wait_with_output());
    });
    match receive.recv_timeout(GIT_TIMEOUT) {
        Ok(Ok(output)) if output.status.success() => Some(output.stdout),
        Ok(_) => None,
        Err(_) => {
            #[cfg(unix)]
            // SAFETY: a plain signal to the child this function started; it
            // has not been waited for yet, so the pid is still its own.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            #[cfg(not(unix))]
            let _ = pid;
            None
        }
    }
}

/// Size, modification and change times and inode, as the signature reads
/// them.
fn stat(meta: &Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}.{}:{}.{}:{}",
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
            meta.ino()
        )
    }
    #[cfg(not(unix))]
    {
        format!("{}:{}", meta.len(), changed_ns(meta))
    }
}

/// When the file's inode last changed, in nanoseconds: every write moves
/// it, and unlike the modification time no tool can set it back.
fn changed_ns(meta: &Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.ctime()
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.ctime_nsec())
    }
    #[cfg(not(unix))]
    {
        meta.modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(i64::MAX, |since| since.as_nanos() as i64)
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as i64)
}

/// An RFC 3339 instant in nanoseconds since the epoch.
fn instant_ns(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .timestamp_nanos_opt()
}

/// What [`ensure`] does with a refreshed project's code graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphPolicy {
    /// Build it in a `graphicle build-graph` process (as `sync
    /// --background` does) and wait for it within [`BUDGET`]; past that the
    /// answer reads the latest code graph with the changed files withheld
    /// (the lag fallback) while the process finishes.
    Wait,
    /// The caller builds it (`ask` builds a waiting graph before it answers).
    Skip,
}

/// How [`ensure`] runs.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub mode: Mode,
    pub graph: GraphPolicy,
}

impl Options {
    /// `context`, `status` and MCP tool calls.
    pub fn answer(config: &model::Config) -> Self {
        Self {
            mode: mode(config),
            graph: GraphPolicy::Wait,
        }
    }
    /// `ask`, which builds the code graph itself.
    pub fn ask(config: &model::Config) -> Self {
        Self {
            mode: mode(config),
            graph: GraphPolicy::Skip,
        }
    }
}

/// The key of the object [`Outcome::attach`] adds to a reply.
pub const KEY: &str = "folder_freshness";

/// What [`ensure`] did, for the answer: nothing to report, or a
/// `folder_freshness` object with a note.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    report: Option<Value>,
}

impl Outcome {
    /// The `folder_freshness` object: `{status: "refreshed", changed, seconds,
    /// graph, note}` or `{status: "stale", changed, reason, note}` (with
    /// `error` when the refresh failed). `None` when there is nothing to
    /// report: the project is fresh, has no folder, or freshness is off.
    pub fn report(&self) -> Option<&Value> {
        self.report.as_ref()
    }
    /// The note, for a person: what was synced, or what is stale and the
    /// command that syncs it.
    pub fn note(&self) -> Option<&str> {
        self.report.as_ref()?.get("note")?.as_str()
    }
    /// Whether the data is stale.
    pub fn stale(&self) -> bool {
        self.report
            .as_ref()
            .is_some_and(|report| report["status"] == "stale")
    }
    /// Add the `folder_freshness` object to a JSON reply that is an object.
    pub fn attach(&self, value: &mut Value) {
        if let (Some(report), Some(object)) = (&self.report, value.as_object_mut()) {
            object.insert(KEY.into(), report.clone());
        }
    }
    /// Add the `folder_freshness` object to an MCP `tools/call` reply: into its
    /// structured content and into the JSON text the agent reads, else as a
    /// text item of its own. Error replies are left as they are.
    pub fn attach_mcp(&self, reply: &mut Value) {
        let Some(report) = &self.report else { return };
        let Some(result) = reply.get_mut("result").and_then(Value::as_object_mut) else {
            return;
        };
        if result.get("isError") == Some(&Value::Bool(true)) {
            return;
        }
        if let Some(structured) = result
            .get_mut("structuredContent")
            .and_then(Value::as_object_mut)
        {
            structured.insert(KEY.into(), report.clone());
        }
        let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else {
            return;
        };
        let annotated = content.first_mut().is_some_and(|first| {
            let parsed = first["text"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            match parsed {
                Some(Value::Object(mut object)) => {
                    object.insert(KEY.into(), report.clone());
                    first["text"] = json!(Value::Object(object).to_string());
                    true
                }
                _ => false,
            }
        });
        if !annotated && let Some(note) = report["note"].as_str() {
            content.push(json!({"type":"text","text":format!("graphicle: {note}")}));
        }
    }
    /// Print the note on standard error when it is a terminal.
    pub fn tell(&self) {
        use std::io::IsTerminal;
        if let Some(note) = self.note()
            && std::io::stderr().is_terminal()
        {
            eprintln!("graphicle: {note}");
        }
    }
}

/// Checks reused within [`CACHE_TTL`], by data directory and project: the
/// stale outcome again, or nothing to report (fresh, or refreshed then).
static RECENT: Mutex<Option<Recent>> = Mutex::new(None);
type Recent = HashMap<(PathBuf, String), (Instant, Outcome)>;

/// Before an answer from `project`: check its folder and, within
/// [`REFRESH_LIMIT`] changed files and a quick sync, sync them (the code
/// graph as `options.graph` says); otherwise report the data as stale. Never
/// fails: an error is reported in the outcome, and a check that cannot run
/// reports nothing. Two processes never refresh one folder at once: the
/// second waits up to [`BUDGET`] for the first, then checks again.
pub fn ensure(store: &Store, session: &Session, project: &str, options: Options) -> Outcome {
    if options.mode == Mode::Off {
        return Outcome::default();
    }
    let key = (store.directory.to_path_buf(), project.to_string());
    {
        let mut recent = RECENT.lock().unwrap_or_else(PoisonError::into_inner);
        let recent = recent.get_or_insert_with(HashMap::new);
        recent.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if let Some((_, outcome)) = recent.get(&key) {
            return outcome.clone();
        }
    }
    // A check or lock that fails is reported, never passed off as fresh.
    let outcome = refresh(store, session, project, options).unwrap_or_else(|error| {
        let mut check = Check::without(project, State::Changed, None);
        check.changed = None;
        stale(&check, "error", Some(error.message))
    });
    let kept = if outcome.stale() {
        outcome.clone()
    } else {
        Outcome::default()
    };
    RECENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(key, (Instant::now(), kept));
    outcome
}

fn refresh(store: &Store, session: &Session, project: &str, options: Options) -> Result<Outcome> {
    let started = Instant::now();
    let first = check(store, session, project)?;
    let Some(folder) = refreshable(&first) else {
        return Ok(Outcome::default());
    };
    if let Some(stale) = too_much(&first) {
        return Ok(stale);
    }
    let Some(lock) = lock(&store.directory, &folder, BUDGET)? else {
        return Ok(stale(&first, "busy", None));
    };
    // Another process may have synced the folder while this one waited.
    let check = check(store, session, project)?;
    if refreshable(&check).as_deref() != Some(folder.as_path()) {
        return Ok(Outcome::default());
    }
    if let Some(stale) = too_much(&check) {
        return Ok(stale);
    }
    let synced = match local::sync_folder(store, session, &folder, Some(project)) {
        Ok(synced) => synced,
        Err(error) => return Ok(stale(&check, "error", Some(error.message))),
    };
    drop(lock);
    // Nothing differed after all (a file touched, or written just before
    // the last sync): the record moved on, and there is nothing to report.
    if synced["replayed"] == true {
        return Ok(Outcome::default());
    }
    let graph = match options.graph {
        GraphPolicy::Skip => "skipped",
        GraphPolicy::Wait => graph_within(
            store,
            session,
            project,
            BUDGET.saturating_sub(started.elapsed()),
        ),
    };
    let changed = check.changed.unwrap_or(0);
    let seconds = started.elapsed().as_secs_f64();
    let mut note = format!(
        "synced {changed} changed {} before answering ({seconds:.1} s)",
        files(changed)
    );
    if graph == "background" {
        note.push_str("; the code graph is being rebuilt in the background");
    }
    Ok(Outcome {
        report: Some(
            json!({"status":"refreshed","changed":changed,"seconds":seconds,
            "graph":graph,"sample":check.sample,"note":note}),
        ),
    })
}

/// The folder to refresh when `check` found changes.
fn refreshable(check: &Check) -> Option<PathBuf> {
    (check.state == State::Changed)
        .then(|| check.folder.clone())
        .flatten()
}

/// The stale outcome when a refresh would not fit: too many changes, a
/// folder too large to count them, or a slow sync (a sync whose time was
/// never recorded counts as slow on a large folder).
fn too_much(check: &Check) -> Option<Outcome> {
    match check.changed {
        None => Some(stale(check, "too_large", None)),
        Some(changed) if changed > REFRESH_LIMIT => Some(stale(check, "too_many_changes", None)),
        _ if check
            .scan_ms
            .is_some_and(|ms| ms > SLOW_SCAN.as_millis() as u64) =>
        {
            Some(stale(check, "slow_sync", None))
        }
        _ if check.scan_ms.is_none() && check.files > UNTIMED_FILES => {
            Some(stale(check, "untimed_sync", None))
        }
        _ => None,
    }
}

fn stale(check: &Check, reason: &str, error: Option<String>) -> Outcome {
    let sync = sync_command(check.folder.as_deref());
    let what = match check.changed {
        Some(changed) => format!("{changed} {} changed since the last sync", files(changed)),
        None => "this folder changed since the last sync".to_string(),
    };
    let note = match (reason, &error) {
        ("busy", _) => format!("{what}; another graphicle process is syncing them"),
        ("slow_sync", _) => format!(
            "{what}; run {sync} (a sync of this folder takes over {:.1} s, too long to run before an answer)",
            SLOW_SCAN.as_secs_f64()
        ),
        ("untimed_sync", _) => format!(
            "{what}; run {sync} (this folder has {} files and how long its sync takes is not known yet; after that sync, a few changed files are synced before each answer)",
            check.files
        ),
        ("error", Some(error)) if check.folder.is_none() => format!(
            "this project's folder could not be checked for changes ({error}); run graphicle sync if files changed"
        ),
        (_, Some(error)) => format!("{what}; the automatic sync failed ({error}); run {sync}"),
        _ => format!("{what}; run {sync}"),
    };
    let mut report = json!({"status":"stale","changed":check.changed,"reason":reason,
        "sample":check.sample,"note":note});
    if let Some(error) = error {
        report["error"] = json!(error);
    }
    Outcome {
        report: Some(report),
    }
}

fn files(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

/// `graphicle sync` in the folder itself, else with the folder named.
fn sync_command(folder: Option<&Path>) -> String {
    let here = std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.canonical().ok());
    match folder {
        Some(folder) if !here.is_some_and(|here| here.starts_with(folder)) => {
            let text = folder.to_string_lossy();
            if text.contains(|c: char| !(c.is_ascii_alphanumeric() || "/._-+".contains(c))) {
                format!("graphicle sync '{}'", text.replace('\'', r"'\''"))
            } else {
                format!("graphicle sync {text}")
            }
        }
        _ => "graphicle sync".to_string(),
    }
}

/// The refreshed project's code graph within `wait`: `current` when no
/// projection was needed, `built` when it finished in time, `background`
/// when a `graphicle build-graph` process (started here, or another
/// process's) is still at it, `queued` when nothing may build it here (the
/// next `ask`, `sync` or `graphicle mcp` does), `failed`, or `none`.
fn graph_within(store: &Store, session: &Session, project: &str, wait: Duration) -> &'static str {
    let report = |status: &str| -> &'static str {
        match status {
            "current" => "current",
            "running" => "background",
            "failed" => "failed",
            "queued" => "queued",
            _ => "none",
        }
    };
    let Ok(first) = local::graph_report(store, session, project) else {
        return "queued";
    };
    match s(&first, "status") {
        "queued" if BACKGROUND_BUILDS.load(Ordering::Relaxed) => {
            if local::spawn_graph_build(&store.directory, project).is_err() {
                return "queued";
            }
        }
        "running" => {}
        status => return report(status),
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        match local::graph_report(store, session, project) {
            Ok(now) if now["status"] == "current" => return "built",
            Ok(now) if now["status"] == "failed" => return "failed",
            Ok(_) => {}
            Err(_) => return "queued",
        }
    }
    "background"
}

/// The refresh lock of `folder` in the data directory `store`
/// (`fresh/<digest>.lock`, an advisory lock the system releases when the
/// process ends), waiting up to `wait`; `None` when another process holds
/// it all that time.
fn lock(store: &Path, folder: &Path, wait: Duration) -> Result<Option<std::fs::File>> {
    let directory = store.join("fresh");
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let digest = crate::digest(&json!(folder.to_string_lossy()));
    let path = directory.join(format!("{}.lock", &digest[..32]));
    if path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(Error::conflict("A freshness lock is a symlink"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `graphicle sync PATH --quiet`, which the git hooks run: nothing when the
/// folder is not a project's (a hook never creates one) or has not changed
/// since its last sync; otherwise the sync and the code graph `graphicle
/// sync` runs (`--no-graph` and `--background` as there), printing nothing
/// and leaving the saved default project as it is. It waits for another
/// process syncing the same folder (at most two minutes), then checks
/// again, so two hooks never sync one folder at once. When git alone moved
/// (a commit, a staged file) and no file changed, it records the new git
/// state, so the next check is one `git status` again.
pub fn sync_quiet(
    store: &Store,
    session: &Session,
    path: &Path,
    requested: Option<&str>,
    no_graph: bool,
    background: bool,
) -> Result<()> {
    let folder = path.canonical()?;
    if !folder.is_dir() {
        return Err(Error::bad("Choose a project folder"));
    }
    let Some(project) = folder_project(store, session, &folder, requested)? else {
        return Ok(());
    };
    let Some(lock) = lock(&store.directory, &folder, QUIET_WAIT)? else {
        // Another process has been syncing this folder all along; its
        // sync covers this change too, or the next hook's does.
        return Ok(());
    };
    let check = check_folder(store, session, &project, &folder)?;
    if check.state == State::Fresh {
        rebaseline(store, session, &folder, &check)?;
        return Ok(());
    }
    let synced = local::sync_folder(store, session, &folder, Some(&project))?;
    drop(lock);
    let repository = s(&synced, "repository_id").to_string();
    if no_graph {
        return Ok(());
    }
    if background {
        if local::graph_report(store, session, &repository)?["status"] == "queued" {
            local::spawn_graph_build(&store.directory, &repository)?;
        }
        return Ok(());
    }
    local::build_graph(store, session, &repository, false)?;
    Ok(())
}

/// Run a sync of `folder` (`sync`, `setup`, `doctor --fix`, `open PATH`)
/// under the folder's lock, the one the Git hooks' `sync --quiet` and the
/// refresh before an answer take: it waits (at most two minutes) for
/// another process syncing the folder, then syncs, which replays when that
/// process left nothing to do. A sync that still finds a newer one finished
/// first (a sync of the same project from another path) runs once more.
pub fn locked<T>(store: &Store, folder: &Path, mut sync: impl FnMut() -> Result<T>) -> Result<T> {
    let folder = folder.canonical()?;
    let mut held = lock(&store.directory, &folder, Duration::ZERO)
        .ok()
        .flatten();
    if held.is_none() {
        use std::io::IsTerminal;
        if std::io::stderr().is_terminal() {
            eprintln!(
                "graphicle: another graphicle process is syncing {}; waiting for it...",
                crate::terminal::tilde(&folder)
            );
        }
        // Past the wait, sync anyway: the store's own check keeps it
        // correct.
        held = lock(&store.directory, &folder, QUIET_WAIT).ok().flatten();
    }
    let _lock = held;
    match sync() {
        Err(error)
            if error.status == axum::http::StatusCode::CONFLICT
                && error.message.contains("newer sync finished first") =>
        {
            sync()
        }
        other => other,
    }
}

/// The project registered for exactly `folder`, the one a sync of it goes
/// into: `requested` when it names that same project, else the folder's
/// own. `None` for a folder never synced or synced into another project.
fn folder_project(
    store: &Store,
    session: &Session,
    folder: &Path,
    requested: Option<&str>,
) -> Result<Option<String>> {
    let cwd = std::env::current_dir().ok();
    store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
        let Some(row) = one(
            db,
            "SELECT repository_id FROM repo_folders WHERE workspace_id=? AND folder_key=?",
            params![p.workspace, crate::folders::folder_key(folder)],
        )?
        else {
            return Ok(None);
        };
        let own = s(&row, "repository_id").to_string();
        if let Some(value) = requested
            && crate::folders::requested(db, p, value, cwd.as_deref(), true)? != own
        {
            return Ok(None);
        }
        Ok(crate::graph_projection::repo_readable(db, p, &own)?.then_some(own))
    })
}

/// After `graphicle setup` wrote its own files into `folder` (canonical):
/// when every file changed since the project's last sync is one of
/// `written` (folder-relative) and a sync would read each as the snapshot
/// holds it (left out, or without Graphicle's lines;
/// [`local::sync_view`]), record the folder's new state over the same
/// snapshot and return true, so no sync is needed and the next answer finds
/// the folder up to date. Otherwise false: the caller syncs.
pub fn absorb_own_writes(
    store: &Store,
    session: &Session,
    project: &str,
    folder: &Path,
    written: &[String],
) -> Result<bool> {
    // The record's time comes after the margin a scan re-reads files in.
    std::thread::sleep(Duration::from_nanos(MARGIN_NS as u64 + 5_000_000));
    let check = check_folder(store, session, project, folder)?;
    match check.state {
        State::Fresh => return Ok(true),
        State::Changed if check.record.is_some() && check.method == "scan" => {}
        _ => return Ok(false),
    }
    // The sample is every changed file only when there are few.
    let Some(changed) = check.changed else {
        return Ok(false);
    };
    if changed > check.sample.len() {
        return Ok(false);
    }
    let heads =
        store.authorized_read(&session.workspace, &session.capability, "read", |db, _| {
            crate::store::heads(db, project)
        })?;
    for relative in &check.sample {
        if !written.contains(relative) {
            return Ok(false);
        }
        let same = match (local::sync_view(folder, relative), heads.get(relative)) {
            (Some(None), None) => true,
            (Some(Some(text)), Some(sha)) => {
                graphicle_memory::text::sha256(text.as_bytes()) == *sha
            }
            _ => false,
        };
        if !same {
            return Ok(false);
        }
    }
    rebaseline(store, session, folder, &check)?;
    Ok(true)
}

/// After a fresh scan whose git signature moved (a commit, a checkout of the
/// same files, a staged file): record the new signature and the scan's time
/// with the same snapshot. Only over a record of this snapshot, whose time
/// is a sync's start, never over a folder's last sync time.
fn rebaseline(store: &Store, session: &Session, folder: &Path, check: &Check) -> Result<()> {
    let Some(record) = &check.record else {
        return Ok(());
    };
    if check.method != "scan" {
        return Ok(());
    }
    let updated = Record {
        since_ns: check.since_ns,
        git: check.git.clone(),
        ..record.clone()
    };
    store.authorized(
        &session.workspace,
        &session.capability,
        "collect",
        |db, p| {
            // Only while the project still has the snapshot the scan compared.
            let current: Option<String> = db
                .query_row(
                    "SELECT snapshot FROM repo_projects WHERE id=?",
                    [check.project.as_str()],
                    |row| row.get(0),
                )
                .ok();
            if current.as_deref() == Some(record.snapshot.as_str()) {
                write_record(db, &p.workspace, folder, &updated)?;
            }
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_lists_the_folders_entries_with_their_paths() {
        let output = [
            "# branch.oid 0123456789abcdef0123456789abcdef01234567",
            "# branch.head main",
            "1 .M N... 100644 100644 100644 aaaa bbbb app/a.py",
            "1 M. N... 100644 100644 100644 aaaa bbbb app/with space.py",
            "2 R. N... 100644 100644 100644 aaaa bbbb R100 app/new.py",
            "app/old.py",
            "u UU N... 100644 100644 100644 100644 aaaa bbbb cccc app/conflict.py",
            "? app/notes.md",
            "? app/nested/",
            "? app/node_modules/x.js",
            "? app/build/out.py",
            "? other/b.py",
            "! app/ignored.log",
        ]
        .join("\0")
            + "\0";
        let (head, entries, ignored) = porcelain(output.as_bytes(), "app");
        assert_eq!(head, "0123456789abcdef0123456789abcdef01234567");
        assert!(!ignored);
        let paths: Vec<&str> = entries.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "a.py",
                "with space.py",
                "new.py",
                "conflict.py",
                "notes.md",
                "nested"
            ]
        );
        assert!(entries[0].1.starts_with(b"1 .M"));
        // At the top level every path is the folder's.
        let (_, top, _) = porcelain(output.as_bytes(), "");
        assert!(top.iter().any(|(path, _)| path == "other/b.py"));
        assert!(top.iter().any(|(path, _)| path == "app/a.py"));
        assert!(!top.iter().any(|(path, _)| path.contains("node_modules")));
        // A new repository has no commit.
        let (head, entries, _) = porcelain(b"# branch.oid (initial)\0? a.py\0", "");
        assert_eq!(head, "(initial)");
        assert_eq!(entries.len(), 1);
        // A folder the repository ignores, itself or through a folder above.
        for listed in ["! scratch/\0", "! scratch/notes/\0"] {
            let (_, entries, ignored) = porcelain(listed.as_bytes(), "scratch/notes");
            assert!(ignored && entries.is_empty(), "{listed}");
        }
        let (_, _, ignored) =
            porcelain(b"! scratch-old/\0! scratch/notes/x.log\0", "scratch/notes");
        assert!(!ignored);
    }

    #[test]
    fn modes_parse_and_the_environment_wins() {
        assert_eq!(Mode::parse("auto").unwrap(), Mode::Auto);
        assert_eq!(Mode::parse(" OFF ").unwrap(), Mode::Off);
        assert!(Mode::parse("sometimes").is_err());
        let config: model::Config = serde_json::from_value(json!({"freshness":"off"})).unwrap();
        assert_eq!(config.freshness, Some(Mode::Off));
        assert!(serde_json::from_value::<model::Config>(json!({"freshness":"never"})).is_err());
        // Unset, the configuration is written as before.
        let plain = serde_json::to_value(model::Config::default()).unwrap();
        assert!(plain.get("freshness").is_none());
        if std::env::var_os("GRAPHICLE_FRESHNESS").is_none() {
            assert_eq!(mode(&config), Mode::Off);
            assert_eq!(mode(&model::Config::default()), Mode::Auto);
        }
    }

    #[test]
    fn a_scan_counts_files_changed_after_its_time_and_synced_files_gone() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().canonical().unwrap();
        let store = folder.join("data");
        std::fs::create_dir_all(store.join("gf-cache")).unwrap();
        std::fs::write(folder.join("old.py"), "x = 1\n").unwrap();
        std::fs::write(folder.join(".env"), "TOKEN=1\n").unwrap();
        std::fs::write(folder.join("README.md"), "# Readme\n").unwrap();
        std::fs::create_dir_all(folder.join("node_modules")).unwrap();
        std::fs::write(folder.join("node_modules/dep.js"), "1\n").unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let since = now_ns();
        std::thread::sleep(Duration::from_millis(60));
        std::fs::write(folder.join("new.py"), "y = 2\n").unwrap();
        std::fs::write(folder.join("data/reviews.sqlite3"), "store").unwrap();
        std::fs::write(folder.join("node_modules/dep.js"), "2\n").unwrap();
        std::fs::write(folder.join(".env"), "TOKEN=2\n").unwrap();
        let sha = |text: &str| graphicle_memory::text::sha256(text.as_bytes());
        let heads: BTreeMap<String, String> = [
            ("old.py", sha("x = 1\n")),
            ("README.md", sha("# Readme\n")),
            ("gone.py", sha("z = 0\n")),
            ("data/models.json", sha("{}")),
        ]
        .into_iter()
        .map(|(path, sha)| (path.to_string(), sha))
        .collect();
        let changed = scan(&folder, &store, since, &heads, &BTreeSet::new()).unwrap();
        assert_eq!(
            changed.into_iter().collect::<Vec<_>>(),
            ["gone.py", "new.py"],
            "the data directory, credentials and skipped folders never count"
        );
        // Nothing after now; within the margin the content decides.
        let later = now_ns() + 2 * MARGIN_NS;
        let mut heads: BTreeMap<String, String> = [
            ("old.py", sha("x = 1\n")),
            ("README.md", sha("# Readme\n")),
            ("new.py", sha("y = 2\n")),
        ]
        .into_iter()
        .map(|(path, sha)| (path.to_string(), sha))
        .collect();
        assert!(
            scan(&folder, &store, later, &heads, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        // A clock read just after new.py was written.
        let written = changed_ns(&std::fs::symlink_metadata(folder.join("new.py")).unwrap());
        let just_after = written + MARGIN_NS / 2;
        assert!(
            scan(&folder, &store, just_after, &heads, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        // A binary the last sync left out is rewritten: nothing to sync.
        let binaries: BTreeSet<String> = ["new.py".to_string()].into();
        let synced: BTreeMap<String, String> = heads
            .iter()
            .filter(|(path, _)| *path != "new.py")
            .map(|(path, sha)| (path.clone(), sha.clone()))
            .collect();
        assert!(
            scan(&folder, &store, since, &synced, &binaries)
                .unwrap()
                .is_empty()
        );
        heads.insert("new.py".into(), sha("y = 1\n"));
        assert_eq!(
            scan(&folder, &store, just_after, &heads, &BTreeSet::new())
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["new.py"]
        );
    }

    #[test]
    fn notes_say_what_happened_and_the_command_that_syncs() {
        let mut check = Check::without("p", State::Changed, Some(PathBuf::from("/nowhere/app")));
        check.changed = Some(250);
        let outcome = stale(&check, "too_many_changes", None);
        let report = outcome.report().unwrap();
        assert_eq!(report["status"], "stale");
        assert_eq!(report["changed"], 250);
        assert_eq!(
            outcome.note().unwrap(),
            "250 files changed since the last sync; run graphicle sync /nowhere/app"
        );
        check.changed = Some(1);
        check.folder = Some(PathBuf::from("/nowhere/my app"));
        let busy = stale(&check, "error", Some("store busy".into()));
        assert_eq!(
            busy.note().unwrap(),
            "1 file changed since the last sync; the automatic sync failed (store busy); run graphicle sync '/nowhere/my app'"
        );
        assert_eq!(busy.report().unwrap()["error"], "store busy");
        check.changed = None;
        assert!(
            stale(&check, "too_large", None)
                .note()
                .unwrap()
                .starts_with("this folder changed since the last sync; run graphicle sync")
        );
        // A folder whose sync time was never recorded: refreshed when small,
        // a stale note with the reason when large.
        let mut untimed = Check::without("p", State::Changed, Some(PathBuf::from("/nowhere/app")));
        untimed.changed = Some(3);
        untimed.files = UNTIMED_FILES;
        assert!(too_much(&untimed).is_none());
        untimed.files = UNTIMED_FILES + 1;
        let note = too_much(&untimed).unwrap();
        assert_eq!(note.report().unwrap()["reason"], "untimed_sync");
        assert!(
            note.note().unwrap().contains("this folder has 2001 files"),
            "{note:?}"
        );
        untimed.scan_ms = Some(200);
        assert!(
            too_much(&untimed).is_none(),
            "a recorded quick sync refreshes"
        );
        // A check that failed before it knew the folder says so.
        let failed = stale(
            &Check::without("p", State::Changed, None),
            "error",
            Some("disk full".into()),
        );
        assert_eq!(
            failed.note().unwrap(),
            "this project's folder could not be checked for changes (disk full); run graphicle sync if files changed"
        );
        // Beside the packet's own `freshness` (reviewed memory), never over it.
        let mut value = json!({"answer":1,"freshness":{"stale_count":0}});
        busy.attach(&mut value);
        assert_eq!(value["folder_freshness"]["reason"], "error");
        assert_eq!(value["freshness"], json!({"stale_count":0}));
        Outcome::default().attach(&mut value);
        assert_eq!(value["folder_freshness"]["reason"], "error");
    }

    #[test]
    fn mcp_replies_carry_the_report_in_both_forms() {
        let check = Check {
            changed: Some(3),
            ..Check::without("p", State::Changed, None)
        };
        let outcome = stale(&check, "too_many_changes", None);
        let packet = json!({"evidence":[],"freshness":{"stale_count":0}});
        let mut reply = json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":packet.to_string()}],"structuredContent":packet,"isError":false}});
        outcome.attach_mcp(&mut reply);
        assert_eq!(
            reply["result"]["structuredContent"]["folder_freshness"]["changed"],
            3
        );
        let text: Value =
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["folder_freshness"]["status"], "stale");
        assert_eq!(text["evidence"], json!([]));
        assert_eq!(text["freshness"], json!({"stale_count":0}));
        // A list result gets the note as a text item of its own.
        let mut listed = json!({"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"[1,2]"}],"structuredContent":{"items":[1,2]},"isError":false}});
        outcome.attach_mcp(&mut listed);
        assert_eq!(listed["result"]["content"][0]["text"], "[1,2]");
        assert!(
            listed["result"]["content"][1]["text"]
                .as_str()
                .unwrap()
                .starts_with("graphicle: 3 files changed")
        );
        // Errors and quiet outcomes leave the reply as it was.
        let error = json!({"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"no"}],"isError":true}});
        let mut unchanged = error.clone();
        outcome.attach_mcp(&mut unchanged);
        assert_eq!(unchanged, error);
        let mut quiet = reply.clone();
        Outcome::default().attach_mcp(&mut quiet);
        assert_eq!(quiet, reply);
    }

    #[test]
    fn instants_read_as_nanoseconds() {
        assert_eq!(
            instant_ns("1970-01-01T00:00:01.000002Z"),
            Some(1_000_002_000)
        );
        assert_eq!(instant_ns("yesterday"), None);
    }
}
