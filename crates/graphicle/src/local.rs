use crate::platform::CanonicalExt;
use crate::store::{Store, s};
use crate::{Error, Result, id, now};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub workspace: String,
    pub capability: String,
    pub project: Option<String>,
}
pub fn data_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("GRAPHICLE_DATA_DIR") {
        return value.into();
    }
    default_data_dir()
}
/// The platform's data folder, ignoring `GRAPHICLE_DATA_DIR`: the store a
/// client finds when it starts `graphicle` without `--data-dir` and without
/// the user's environment.
pub fn default_data_dir() -> PathBuf {
    // Windows: `%LOCALAPPDATA%\Graphicle`.
    #[cfg(windows)]
    {
        crate::platform::local_app_data()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Graphicle")
    }
    #[cfg(not(windows))]
    let user_dir = crate::platform::home_dir().unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    {
        user_dir.join("Library/Application Support/Graphicle")
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| user_dir.join(".local/share"))
            .join("graphicle")
    }
}
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.is_symlink() {
        return Err(Error::bad("Refusing to replace a symlink"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::bad("File has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".graphicle-{}", id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}
pub fn save_session(store: &Store, session: &Session) -> Result<()> {
    write_private(
        &store.directory.join("local-session.json"),
        &serde_json::to_vec_pretty(session)?,
    )
}
pub fn session(store: &Store, create: bool) -> Result<Session> {
    let path = store.directory.join("local-session.json");
    if path.exists() {
        if path.is_symlink() {
            return Err(Error::bad("Local session file cannot be a symlink"));
        }
        let bytes = std::fs::read(path)?;
        if bytes.len() > 8192 {
            return Err(Error::bad("Invalid local session"));
        }
        let value: Session = serde_json::from_slice(&bytes)?;
        store.workspace(&value.workspace, &value.capability)?;
        return Ok(value);
    }
    if !create {
        return Err(Error::bad("Run graphicle init, or graphicle login, first"));
    }
    let created = store.create_workspace("My workspace")?;
    let session = Session {
        workspace: s(&created, "id").into(),
        capability: s(&created, "capability").into(),
        project: None,
    };
    save_session(store, &session)?;
    Ok(session)
}
/// The project a command acts on: `--project` (an id, a key
/// or a folder), else the project of this process's working directory, else
/// the saved default project. See [`Store::resolve_project`].
pub fn resolve(
    store: &Store,
    session: &Session,
    requested: Option<&str>,
) -> Result<crate::folders::Resolved> {
    let cwd = std::env::current_dir().ok();
    store.resolve_project(
        &session.workspace,
        &session.capability,
        requested,
        cwd.as_deref(),
        session.project.as_deref(),
    )
}
/// The id of [`resolve`]'s project.
pub fn project(store: &Store, session: &Session, requested: Option<&str>) -> Result<String> {
    Ok(resolve(store, session, requested)?.id)
}
/// [`resolve`] without `--project`, or `None` when neither the working
/// directory nor the saved default names a project.
pub fn find(store: &Store, session: &Session) -> Result<Option<crate::folders::Resolved>> {
    let cwd = std::env::current_dir().ok();
    store.find_project(
        &session.workspace,
        &session.capability,
        None,
        cwd.as_deref(),
        session.project.as_deref(),
    )
}
/// Folder and file names a folder sync never reads, at any depth.
pub(crate) const SKIPPED_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    ".venv",
    "venv",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    "__pycache__",
    ".ssh",
    ".aws",
    ".azure",
    ".codex",
    ".graphicle",
    ".graphicle-graph",
    "graphify-out",
];

/// Files a folder sync skips by name: credentials, keys and the session file.
pub(crate) fn sensitive(name: &str) -> bool {
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || [
            "credentials",
            "credentials.json",
            "secrets.json",
            ".netrc",
            ".npmrc",
            ".pypirc",
            "local-session.json",
        ]
        .contains(&name)
}

/// Files a folder sync never reads, by their folder-relative path: the
/// backups setup and connect keep (`<file>.graphicle-bak`)
/// and Claude Code's personal settings (`.claude/settings.local.json`,
/// which may hold tokens and holds the capture hooks' paths).
pub(crate) fn skipped_file(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    name.ends_with(crate::agent_files::BACKUP_SUFFIX)
        || relative == ".claude/settings.local.json"
        || relative.ends_with("/.claude/settings.local.json")
}

/// The walk of a folder sync: `.gitignore` (also outside a git repository),
/// the global and the repository's excludes, hidden files, no symlinks
/// followed, and [`SKIPPED_NAMES`] and [`skipped_file`]'s files left out.
/// The freshness scan ([`crate::fresh`]) walks a folder the same way.
pub(crate) fn folder_walk(root: &Path) -> ignore::WalkBuilder {
    let mut walk = ignore::WalkBuilder::new(root);
    walk.hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false);
    let top = root.to_path_buf();
    walk.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        let name = entry.file_name().to_string_lossy();
        if SKIPPED_NAMES.contains(&name.as_ref()) {
            return false;
        }
        !entry
            .path()
            .strip_prefix(&top)
            .is_ok_and(|relative| skipped_file(&relative.to_string_lossy().replace('\\', "/")))
    });
    walk
}

/// The environment variable that turns reading the tracked files an
/// ignore pattern matches ([`tracked_ignored`]) off (`0`, `false`, `off` or
/// `no`) or on (any other value); it wins over the configuration file's
/// `sync.tracked_ignored` ([`tracked_ignored_setting`]).
pub const TRACKED_IGNORED_VARIABLE: &str = "GRAPHICLE_SYNC_TRACKED_IGNORED";

/// The configuration file's `sync.tracked_ignored`, installed by the command
/// line after it loads the file ([`install_tracked_ignored`]).
static TRACKED_IGNORED_SETTING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Make the configuration's `sync.tracked_ignored` this process's setting.
pub fn install_tracked_ignored(config: &crate::model::Config) {
    if let Some(on) = config.sync.as_ref().and_then(|sync| sync.tracked_ignored) {
        let _ = TRACKED_IGNORED_SETTING.set(on);
    }
}

/// Whether a folder sync reads the tracked files an ignore pattern matches:
/// [`TRACKED_IGNORED_VARIABLE`] when it is set, else the configuration
/// file's `sync.tracked_ignored` (`config`, or the installed one), else yes.
pub fn tracked_ignored_setting(config: Option<&crate::model::Config>) -> bool {
    if let Ok(value) = std::env::var(TRACKED_IGNORED_VARIABLE) {
        return !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        );
    }
    config
        .and_then(|config| config.sync.as_ref())
        .and_then(|sync| sync.tracked_ignored)
        .or_else(|| TRACKED_IGNORED_SETTING.get().copied())
        .unwrap_or(true)
}

/// The files under `root` that its Git repository tracks although a
/// `.gitignore` (or `.git/info/exclude`, or the global excludes) pattern
/// matches them (`git ls-files --cached --ignored --exclude-standard`),
/// which [`folder_walk`] leaves out: Git still versions them, so they are
/// the project's sources (a repository that ignores `jsonata.js` for its
/// build output and tracks `src/jsonata.js`). A file an `.ignore` file
/// leaves out stays out, as in the walk; so do [`SKIPPED_NAMES`] and
/// [`skipped_file`]'s files, and only regular files count. Empty outside a
/// Git work tree, when Git fails, or when [`tracked_ignored_setting`] says
/// no.
pub(crate) fn tracked_ignored(root: &Path) -> Vec<PathBuf> {
    if !tracked_ignored_setting(None)
        || !root.ancestors().any(|folder| folder.join(".git").exists())
    {
        return Vec::new();
    }
    let Some(listed) = crate::fresh::git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--ignored",
            "--exclude-standard",
        ],
    ) else {
        return Vec::new();
    };
    let mut dot_ignore = DotIgnore::new(root);
    listed
        .split(|byte| *byte == 0)
        .filter_map(|path| std::str::from_utf8(path).ok())
        .filter(|relative| {
            !relative.is_empty()
                && !relative
                    .split('/')
                    .any(|part| SKIPPED_NAMES.contains(&part))
                && !skipped_file(relative)
        })
        .filter(|relative| !dot_ignore.ignores(relative))
        .map(|relative| root.join(relative))
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()))
        .collect()
}

/// The `.ignore` files of a folder, read once each: the walk honours them
/// (as ripgrep does), so the tracked files they leave out stay out too.
struct DotIgnore {
    root: PathBuf,
    folders: std::collections::HashMap<PathBuf, Option<ignore::gitignore::Gitignore>>,
}

impl DotIgnore {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            folders: std::collections::HashMap::new(),
        }
    }

    /// Whether the `.ignore` files from `relative`'s folder up to the root
    /// leave it out: the deepest one that matches decides (`!pattern`
    /// takes it back in).
    fn ignores(&mut self, relative: &str) -> bool {
        let path = self.root.join(relative);
        let mut folder = path.parent().map(Path::to_path_buf);
        while let Some(current) = folder {
            if !current.starts_with(&self.root) {
                break;
            }
            let matcher = self.folders.entry(current.clone()).or_insert_with(|| {
                let file = current.join(".ignore");
                file.is_file().then(|| {
                    let mut builder = ignore::gitignore::GitignoreBuilder::new(&current);
                    builder.add(&file);
                    builder.build().ok()
                })?
            });
            if let Some(matcher) = matcher {
                let found = matcher.matched_path_or_any_parents(&path, false);
                if found.is_ignore() {
                    return true;
                }
                if found.is_whitelist() {
                    return false;
                }
            }
            folder = current.parent().map(Path::to_path_buf);
        }
        false
    }
}

/// [`folder_walk`]'s entries, then one for each of [`tracked_ignored`]'s
/// files: what a folder sync and the freshness scan read. The folder itself
/// is the first entry.
pub(crate) fn folder_entries(
    root: &Path,
) -> impl Iterator<Item = std::result::Result<ignore::DirEntry, ignore::Error>> {
    let tracked = tracked_ignored(root);
    folder_walk(root)
        .build()
        .chain(tracked.into_iter().flat_map(|path| {
            ignore::WalkBuilder::new(path)
                .standard_filters(false)
                .follow_links(false)
                .max_depth(Some(0))
                .build()
        }))
}

/// Graphicle's own writes are not the project's evidence:
/// the Graphicle section of an AGENTS.md or CLAUDE.md reads as blank lines,
/// so the file's other lines keep their numbers, and a file left blank is
/// not read; a client MCP file holding only Graphicle's entries
/// (`.mcp.json`, `.vscode/mcp.json`) is not read either. `None`: leave the
/// file out.
fn without_graphicle(relative: &str, content: String) -> Option<String> {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    if matches!(name, "AGENTS.md" | "CLAUDE.md") && crate::agent_instructions::present(&content) {
        let read = crate::agent_instructions::for_sync(&content);
        return (!read.trim().is_empty()).then_some(read);
    }
    let vscode = relative == ".vscode/mcp.json" || relative.ends_with("/.vscode/mcp.json");
    if (name == ".mcp.json" || vscode)
        && let Some(document) = crate::json_edit::parse(&content)
        && let Some(object) = document.as_object()
    {
        let group = if vscode { "servers" } else { "mcpServers" };
        let only_ours = object.keys().all(|key| key == group)
            && object[group].as_object().is_some_and(|servers| {
                !servers.is_empty() && servers.values().all(crate::agent_files::is_graphicle_entry)
            });
        if only_ours {
            return None;
        }
    }
    Some(content)
}

/// What a folder sync would read of the text file `relative` under `root`
/// (canonical) now: `Some(None)` when it leaves the file out (Graphicle's
/// own files, an absent, sensitive, binary or too large file),
/// `Some(Some(text))` for its text, `None` when only a sync can tell (a
/// document it converts, a symbolic link, an unreadable file). Setup uses
/// it to see whether the files it wrote change what the project reads.
pub(crate) fn sync_view(root: &Path, relative: &str) -> Option<Option<String>> {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    if skipped_file(relative)
        || relative
            .split('/')
            .any(|part| SKIPPED_NAMES.contains(&part))
        || sensitive(name)
    {
        return Some(None);
    }
    let path = root.join(relative);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(None),
        Err(_) => return None,
    };
    if meta.file_type().is_symlink() || crate::documents::supported(name) {
        return None;
    }
    if !meta.is_file() || meta.len() > crate::limits::current().file_bytes as u64 {
        return Some(None);
    }
    let bytes = std::fs::read(&path).ok()?;
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Some(without_graphicle(relative, text)),
        _ => Some(None),
    }
}

/// Raise when a document converter changes what it extracts (a parser
/// upgrade, a new option), so that the next sync converts every document
/// again instead of keeping the text of an unchanged original
/// ([`converter`]).
const CONVERSIONS: u32 = 1;

/// What identifies this build's document conversions in a folder's
/// freshness record: the program's version, [`CONVERSIONS`], whether the
/// native document helper (OCR, legacy formats) is installed, and the limits
/// a conversion applies (the document's size, its text, an archive's size
/// and entries).
pub(crate) fn converter() -> String {
    let limits = crate::limits::current();
    format!(
        "{}+{CONVERSIONS}{}+{}/{}/{}/{}",
        env!("CARGO_PKG_VERSION"),
        if crate::documents::helper_installed() {
            "+helper"
        } else {
            ""
        },
        limits.document_bytes,
        limits.document_text_bytes,
        limits.archive_bytes,
        limits.archive_entries
    )
}

/// A document the snapshot holds as converted text.
struct Converted {
    /// The SHA-256 of the original file.
    original: String,
    text_sha256: String,
    parser: String,
    text: String,
}

/// The documents of `repository`'s snapshot by path, when the last sync of
/// `root` (canonical) left that snapshot with this build's conversions
/// ([`crate::fresh::converted_here`]): converting an unchanged original again
/// gives the text the snapshot holds, so a sync keeps it instead. Empty
/// otherwise (a first sync, a snapshot from another folder or an earlier
/// build).
fn converted(
    store: &Store,
    session: &Session,
    repository: &str,
    root: &Path,
) -> Result<std::collections::HashMap<String, Converted>> {
    store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
        let project = crate::store::repo(db, p, repository)?;
        let mut found = std::collections::HashMap::new();
        if !crate::fresh::converted_here(db, &p.workspace, root, s(&project, "snapshot"))? {
            return Ok(found);
        }
        let mut rows = db.prepare(
            "SELECT b.path,b.original_sha,b.source_sha,b.parser,o.content FROM am_binary_heads b \
             JOIN repo_heads h ON h.repository_id=b.repository_id AND h.path=b.path AND h.sha256=b.source_sha \
             JOIN repo_objects o ON o.repository_id=b.repository_id AND o.sha256=b.source_sha \
             WHERE b.repository_id=?",
        )?;
        let mut rows = rows.query([s(&project, "id")])?;
        while let Some(row) = rows.next()? {
            found.insert(
                row.get::<_, String>(0)?,
                Converted {
                    original: row.get(1)?,
                    text_sha256: row.get(2)?,
                    parser: row.get(3)?,
                    text: row.get(4)?,
                },
            );
        }
        Ok(found)
    })
}

/// Sync the folder `root` into its project: `requested` (an id, a key, a
/// recorded folder key or a registered folder itself), else the project
/// registered for exactly this folder, else a new one under its `local:` key.
/// A sync that completes records the folder as an alias of the project it
/// went into, in its last transaction, so a sync that fails
/// moves no alias; it also makes that project the saved default.
///
/// A folder over a project limit of whoever runs Graphicle
/// ([`crate::limits`]) is refused, with a message that names the count, the
/// limit, the setting that raises it and how to leave paths out. No single
/// file fails the sync: the result lists a file left out for its size or a
/// failed conversion, or not analysed, under `skipped`, and a file analysed
/// in part under `limited`, each with the reason, when there are any. The
/// manifest and the content go in batches under the request caps.
pub fn sync(
    store: &Store,
    session: &mut Session,
    root: &Path,
    requested: Option<&str>,
) -> Result<Value> {
    let synced = sync_folder(store, session, root, requested)?;
    session.project = Some(s(&synced, "repository_id").to_string());
    save_session(store, session)?;
    Ok(synced)
}

/// [`sync`] without making the project the saved default: the sync a
/// freshness refresh runs before an answer ([`crate::fresh`]). It also
/// records, with the folder, what the next freshness check compares with.
pub fn sync_folder(
    store: &Store,
    session: &Session,
    root: &Path,
    requested: Option<&str>,
) -> Result<Value> {
    let limits = store.limits()?;
    let root = root.canonical()?;
    if !root.is_dir() {
        return Err(Error::bad("Choose a project folder"));
    }
    // Only the project folder itself must be readable; an entry that cannot
    // be read is listed under `skipped` and the sync goes on.
    std::fs::read_dir(&root)?;
    // Taken before the folder is read: a file that changes from here on is
    // newer than the record this sync leaves.
    let read_started = std::time::Instant::now();
    let mut mark = crate::phase::phase("sync_read_mark", || {
        crate::fresh::mark(&root, &store.directory)
    });
    let mut name = root
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("Project");
    if name.len() > 150 {
        let mut end = 150;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name = &name[..end];
    }
    let cwd = std::env::current_dir().ok();
    let (repository, _) = store.folder_project(
        &session.workspace,
        &session.capability,
        &root,
        requested,
        cwd.as_deref(),
        name,
    )?;
    // The snapshot this sync replaces (`sync_prepare` refuses a sync whose
    // project moved meanwhile), read alone: the project's status view also
    // assembles its documents, memories and graph.
    let expected =
        store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
            Ok(crate::store::repo(db, p, &repository)?["snapshot"].clone())
        })?;
    let mut converted = converted(store, session, &repository, &root)?;
    // Document conversions run in this sync, the time they took, and the
    // documents whose text was kept (`GRAPHICLE_PHASE_TIMINGS`).
    let (mut conversions, mut converting, mut kept) = (0usize, std::time::Duration::ZERO, 0usize);
    let mut files = BTreeMap::new();
    // Documents converted: their paths and conversions; their originals go
    // to the store in batches (`originals`), so a folder's documents are
    // never all held at once.
    let mut binaries: Vec<(String, Value)> = Vec::new();
    let mut originals: Vec<(String, Vec<u8>, Value)> = Vec::new();
    let mut original_bytes = 0usize;
    let mut binary_bytes = 0usize;
    // Left out by rule (secrets, links, binaries) or size: counted, and the
    // first 300 recorded with the snapshot.
    let mut excluded = Vec::new();
    // Left out for size or a failed conversion: listed in the result.
    let mut skipped = Vec::new();
    let mut bytes = 0;
    // Past a project limit: the files and text counted so far, the rest
    // counted without reading it.
    let mut over: Option<(usize, usize, bool)> = None;
    // The tracked files an ignore pattern matches, which the walk is
    // followed by: counted when read (`tracked_ignored` in the result).
    let tracked: std::collections::HashSet<PathBuf> = tracked_ignored(&root).into_iter().collect();
    let tracked_entries = tracked.clone().into_iter().flat_map(|path| {
        ignore::WalkBuilder::new(path)
            .standard_filters(false)
            .follow_links(false)
            .max_depth(Some(0))
            .build()
    });
    let mut tracked_read = 0usize;
    for entry in folder_walk(&root).build().chain(tracked_entries) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let path = error_path(&error)
                    .and_then(|path| path.strip_prefix(&root).ok())
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                skipped.push(json!({"path":path,"reason":format!("Could not read: {}",io_reason(&error.to_string()))}));
                continue;
            }
        };
        // The folder, a directory, or (depth 0 too) a tracked file the walk
        // left out, which is read.
        if entry.path() == root.as_path() || entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| Error::bad("Source escaped project root"))?
            .to_string_lossy()
            .replace('\\', "/");
        let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
        if entry.file_type().is_some_and(|t| t.is_symlink()) || sensitive(name) {
            excluded.push(json!({"path":relative,"reason":"sensitive_or_symlink"}));
            continue;
        }
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) => {
                skipped.push(json!({"path":relative,"reason":unreadable(&error)}));
                continue;
            }
        };
        let is_document = crate::documents::supported(name);
        let limit = if is_document {
            limits.document_bytes
        } else {
            limits.file_bytes
        };
        if !meta.is_file() || meta.len() > limit as u64 {
            excluded.push(json!({"path":relative,"reason":"file_size_or_type"}));
            // Listed only when it would have been indexed: a document, or a
            // file whose start reads as text (other binaries are counted
            // under `excluded` only).
            if meta.is_file() && (is_document || starts_as_text(path)) {
                skipped
                    .push(json!({"path":relative,"reason":too_large(meta.len() as usize, limit)}));
            }
            continue;
        }
        if let Some((count, size, about)) = &mut over {
            *count += 1;
            if !is_document {
                *size += meta.len() as usize;
            }
            *about = true;
            continue;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        use std::io::Read;
        let mut contents = Vec::new();
        if let Err(error) = options
            .open(path)
            .and_then(|file| file.take(limit as u64 + 1).read_to_end(&mut contents))
        {
            skipped.push(json!({"path":relative,"reason":unreadable(&error)}));
            continue;
        }
        if contents.len() > limit {
            return Err(Error::conflict(
                "A source grew during scanning; retry the sync",
            ));
        }
        let content = if is_document {
            // An unchanged original keeps the text this build extracted
            // from it (still within the text limit); others are converted.
            // Taken out of the map, so its text is not held twice.
            let same = converted.remove(&relative).filter(|document| {
                document.text.len() <= crate::documents::max_text()
                    && document.original == graphicle_memory::text::sha256(&contents)
                    && document.text_sha256
                        == graphicle_memory::text::sha256(document.text.as_bytes())
            });
            let parsed = match same {
                Some(document) => {
                    kept += 1;
                    json!({"text":document.text,"text_sha256":document.text_sha256,
                        "original_sha256":document.original,"parser":document.parser})
                }
                None => {
                    let started = std::time::Instant::now();
                    let parsed = crate::documents::extract(name, &contents);
                    conversions += 1;
                    converting += started.elapsed();
                    match parsed {
                        Ok(v) => v,
                        Err(error) => {
                            skipped.push(json!({"path":relative,"reason":format!("Document not converted: {}",error.message)}));
                            excluded.push(json!({"path":relative,"reason":"document_conversion","detail":error.message}));
                            continue;
                        }
                    }
                }
            };
            let text = s(&parsed, "text").to_string();
            if text.len() > limits.file_bytes {
                skipped.push(
                    json!({"path":relative,"reason":too_large(text.len(), limits.file_bytes)}),
                );
                excluded.push(json!({"path":relative,"reason":"file_size_or_type"}));
                continue;
            }
            binary_bytes += contents.len();
            if binary_bytes > limits.folder_document_bytes {
                return Err(Error::bad(format!(
                    "This folder's documents exceed {} (PDF, Office, EPUB, HTML and image files together). Raise the limit with {}, or leave untracked ones out with .gitignore or .git/info/exclude (they cover only files Git does not track; graphicle config set sync.tracked_ignored false, or GRAPHICLE_SYNC_TRACKED_IGNORED=0, leaves out the tracked files they match too), or set up a subfolder: graphicle setup path/to/subfolder",
                    crate::limits::size(limits.folder_document_bytes),
                    crate::limits::raise("folder_document_bytes")
                )));
            }
            // Past a project limit nothing more is stored: the sync is
            // refused, and its originals would only wait for storage.prune.
            if files.len() >= limits.project_files || bytes + text.len() > limits.project_bytes {
                over = Some((files.len() + 1, bytes + text.len(), false));
                continue;
            }
            // Only what the binary head needs; the text goes in `files`.
            let parsed = json!({"text_sha256":parsed["text_sha256"],
                "original_sha256":parsed["original_sha256"],"parser":parsed["parser"]});
            binaries.push((relative.clone(), parsed.clone()));
            original_bytes += contents.len();
            originals.push((relative.clone(), contents, parsed));
            if original_bytes >= crate::limits::REQUEST_BYTES {
                store_originals(store, session, &repository, &originals)?;
                originals.clear();
                original_bytes = 0;
            }
            text
        } else {
            match String::from_utf8(contents) {
                Ok(value) if !value.contains('\0') => match without_graphicle(&relative, value) {
                    Some(value) => value,
                    None => continue,
                },
                _ => {
                    excluded.push(json!({"path":relative,"reason":"unsupported_binary"}));
                    continue;
                }
            }
        };
        if files.len() >= limits.project_files || bytes + content.len() > limits.project_bytes {
            over = Some((files.len() + 1, bytes + content.len(), false));
            continue;
        }
        bytes += content.len();
        if tracked.contains(path) {
            tracked_read += 1;
        }
        files.insert(relative, content);
    }
    if let Some((count, size, about)) = over {
        return Err(crate::limits::project_refusal(count, size, about, &limits));
    }
    store_originals(store, session, &repository, &originals)?;
    drop(originals);
    let file_count = files.len();
    if crate::phase::enabled() {
        eprintln!(
            "phase=sync_read_documents ms={} converted={conversions} kept={kept}",
            converting.as_millis()
        );
    }
    let manifest: Vec<_> = crate::phase::phase("sync_read_manifest", || {
        files.iter().map(|(path,content)|json!({"path":path,"sha256":graphicle_memory::text::sha256(content.as_bytes()),"bytes":content.len()})).collect()
    });
    let metadata = json!({"scan":"native_local_folder","scanned_at":now(),"excluded":excluded.iter().take(300).collect::<Vec<_>>(),"excluded_count":excluded.len(),"scope_complete":excluded.is_empty()});
    mark.scanned();
    crate::phase::since("sync_read", read_started);
    let prepared = send_snapshot(
        store,
        session,
        &repository,
        &expected,
        &manifest,
        metadata,
        &mut files,
    )?;
    drop(files);
    store.authorized(
        &session.workspace,
        &session.capability,
        "collect",
        |db, p| {
            let current = crate::store::repo(db, p, &repository)?;
            if current["snapshot"] != prepared["snapshot"] {
                return Err(Error::conflict(
                    "Folder changed before binary originals could be attached; sync again",
                ));
            }
            db.execute(
                "DELETE FROM am_binary_heads WHERE repository_id=?",
                [&repository],
            )?;
            for (path, parsed) in &binaries {
                db.execute(
                    "INSERT INTO am_binary_heads VALUES(?,?,?,?,?)",
                    rusqlite::params![
                        repository,
                        path,
                        s(parsed, "text_sha256"),
                        s(parsed, "original_sha256"),
                        s(parsed, "parser")
                    ],
                )?;
            }
            // The folder is this project's from now on.
            crate::folders::record(db, &p.workspace, &repository, &root)?;
            crate::fresh::record(db, &p.workspace, &root, &mark, s(&current, "snapshot"))?;
            Ok(())
        },
    )?;
    let (limited, not_analysed) =
        store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
            let current = crate::store::repo(db, p, &repository)?;
            crate::store::analysis_report(db, s(&current, "id"))
        })?;
    skipped.extend(not_analysed);
    skipped.sort_by(|a, b| s(a, "path").cmp(s(b, "path")));
    let mut result = json!({"repository_id":repository,"files":file_count,"binary_documents":binaries.len(),"original_bytes":binary_bytes,"bytes":bytes,"excluded":excluded.len(),"snapshot":prepared["snapshot"],"replayed":prepared["replayed"]});
    // Files read although an ignore pattern matches them, because Git tracks
    // them (`sync.tracked_ignored`).
    if tracked_read > 0 {
        result["tracked_ignored"] = json!(tracked_read);
    }
    if !limited.is_empty() {
        result["limited"] = json!(limited);
    }
    if !skipped.is_empty() {
        result["skipped"] = json!(skipped);
    }
    if let Some(note) = crate::limits::question_note(file_count, bytes) {
        result["question_limits"] = json!(note);
    }
    Ok(result)
}

/// The one line `graphicle sync` prints (on standard error) when a sync's
/// result lists files under `limited` or `skipped`: how many, and where the
/// result lists them with the reasons. `None` when it lists none.
pub fn limits_note(synced: &Value) -> Option<String> {
    let count = |key: &str| synced[key].as_array().map_or(0, Vec::len);
    let files = |n: usize| {
        if n == 1 {
            "1 file".to_string()
        } else {
            format!("{} files", crate::limits::grouped(n))
        }
    };
    let (limited, skipped) = (count("limited"), count("skipped"));
    let (counts, keys) = match (limited, skipped) {
        (0, 0) => return None,
        (limited, 0) => (
            format!("{} analysed in part", files(limited)),
            "\"limited\" in the result lists",
        ),
        (0, skipped) => (
            format!("{} skipped", files(skipped)),
            "\"skipped\" in the result lists",
        ),
        (limited, skipped) => (
            format!(
                "{} analysed in part and {} skipped",
                files(limited),
                files(skipped)
            ),
            "\"limited\" and \"skipped\" in the result list",
        ),
    };
    Some(format!(
        "graphicle: {counts}; {keys} each file with the reason"
    ))
}

/// Reserve storage for document originals before a new text snapshot is
/// exposed. They stay unreachable until a matching binary head is activated
/// after the text commit, and `storage.prune` removes those that never are.
fn store_originals(
    store: &Store,
    session: &Session,
    repository: &str,
    originals: &[(String, Vec<u8>, Value)],
) -> Result<()> {
    store.authorized(&session.workspace,&session.capability,"collect",|db,p|{
        crate::store::repo(db,p,repository)?;
        for (path,original,parsed) in originals {
            db.execute("INSERT OR IGNORE INTO am_original_documents VALUES(?,NULL,?,?,?,?,?,?,?)",rusqlite::params![repository,format!("file:{path}"),s(parsed,"original_sha256"),path,s(parsed,"parser"),s(parsed,"text_sha256"),original,now()])?;
        }
        let size:i64=db.query_row("SELECT coalesce(sum(length(bytes)),0) FROM am_original_documents WHERE repository_id=?",[repository],|r|r.get(0))?;
        let cap=store.limits()?.retained_original_bytes;
        if usize::try_from(size).unwrap_or(usize::MAX)>cap{return Err(Error::bad(format!("Retained document originals exceed {}; prune earlier versions with storage.prune, or raise the limit with {}",crate::limits::size(cap),crate::limits::raise("retained_original_bytes"))));}Ok(())
    })
}

/// The path a walk error names, when it names one.
fn error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::Loop { child, .. } => Some(child),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        _ => None,
    }
}

/// The reason of a walk error without the absolute path it repeats.
fn io_reason(message: &str) -> String {
    message
        .rsplit_once(": ")
        .map(|(_, reason)| reason)
        .unwrap_or(message)
        .to_string()
}

/// The note for an entry that could not be read.
fn unreadable(error: &std::io::Error) -> String {
    format!("Could not read: {}", io_reason(&error.to_string()))
}

/// Whether the first 8 KiB of a file read as text (no NUL byte), so a file
/// left out for its size is one that would have been indexed.
fn starts_as_text(path: &Path) -> bool {
    use std::io::Read;
    let mut start = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(8192).read_to_end(&mut start))
        .is_ok_and(|_| !start.contains(&0))
}

/// The note for a file left out of indexing for its size.
fn too_large(bytes: usize, limit: usize) -> String {
    format!(
        "Too large to index ({} bytes; limit {}); left out",
        crate::limits::grouped(bytes),
        crate::limits::grouped(limit)
    )
}

/// Send a snapshot (`manifest` entries `{path, sha256, bytes}`, `files` the
/// text by path) to `repository` through the sync protocol in batches under
/// its request caps: the manifest in parts of at most
/// [`crate::limits::REQUEST_FILES`] entries, the missing content in uploads
/// of at most [`crate::limits::UPLOAD_FILES`] files and
/// [`crate::limits::REQUEST_BYTES`] of text, then the commit. `expected` is
/// the repository's snapshot the scan started from. Uploaded text is taken
/// out of `files`. A step that fails aborts the unfinished sync, so nothing
/// is committed; a retry starts a new sync and reuses the content and
/// analyses already stored. Returns `sync.prepare`'s last reply.
pub fn send_snapshot(
    store: &Store,
    session: &Session,
    repository: &str,
    expected: &Value,
    manifest: &[Value],
    metadata: Value,
    files: &mut BTreeMap<String, String>,
) -> Result<Value> {
    let mut started = None;
    let sent = send_batches(
        store,
        session,
        repository,
        expected,
        manifest,
        metadata,
        files,
        &mut started,
    );
    if sent.is_err()
        && let Some(sync_id) = started
    {
        let _ = store.project_mutate(
            &session.workspace,
            &session.capability,
            "sync.abort",
            &json!({"repository_id":repository,"sync_id":sync_id}),
        );
    }
    sent
}

#[allow(clippy::too_many_arguments)]
fn send_batches(
    store: &Store,
    session: &Session,
    repository: &str,
    expected: &Value,
    manifest: &[Value],
    metadata: Value,
    files: &mut BTreeMap<String, String>,
    started: &mut Option<String>,
) -> Result<Value> {
    let (workspace, capability) = (session.workspace.as_str(), session.capability.as_str());
    let parts: Vec<&[Value]> = if manifest.is_empty() {
        vec![manifest]
    } else {
        manifest.chunks(crate::limits::REQUEST_FILES).collect()
    };
    let mut prepared = Value::Null;
    for (index, part) in parts.iter().enumerate() {
        let mut args = json!({"repository_id":repository,"manifest":part});
        if index + 1 < parts.len() {
            args["manifest_complete"] = json!(false);
        }
        match started.as_deref() {
            Some(sync_id) => args["sync_id"] = json!(sync_id),
            None => {
                args["expected_snapshot"] = expected.clone();
                args["metadata"] = metadata.clone();
            }
        }
        prepared = crate::phase::phase("sync_prepare", || {
            store.sync_prepare(workspace, capability, &args)
        })?;
        if let Some(sync_id) = prepared["sync_id"].as_str() {
            *started = Some(sync_id.to_string());
        }
    }
    if prepared["replayed"] == true {
        return Ok(prepared);
    }
    let upload = |batch: Vec<Value>| {
        store.sync_upload(
            workspace,
            capability,
            &json!({"repository_id":repository,"sync_id":prepared["sync_id"],"files":batch}),
        )
    };
    let (mut batch, mut batch_bytes) = (Vec::new(), 0);
    for name in prepared["missing"].as_array().into_iter().flatten() {
        let name = name.as_str().unwrap_or_default();
        let content = files
            .remove(name)
            .ok_or_else(|| Error::bad("The sync asked for a file the folder did not send"))?;
        if !batch.is_empty()
            && (batch.len() == crate::limits::UPLOAD_FILES
                || batch_bytes + content.len() > crate::limits::REQUEST_BYTES)
        {
            upload(std::mem::take(&mut batch))?;
            batch_bytes = 0;
        }
        batch_bytes += content.len();
        batch.push(json!({"path":name,"content":content}));
    }
    if !batch.is_empty() {
        upload(batch)?;
    }
    crate::phase::phase("sync_commit", || {
        store.sync_commit(
            workspace,
            capability,
            &json!({"repository_id":repository,"sync_id":prepared["sync_id"]}),
        )
    })?;
    Ok(prepared)
}

/// `repository`'s code graph for its current head set, as the
/// `graph` field of `graphicle sync` reports it without building anything:
/// `status` `current` (the head set's episode is recorded, with its `nodes`
/// and `edges`), else the state of its projection (`queued`, `running`,
/// `failed` or `none`, `jobs::projection_state`), and the `episode`.
pub fn graph_report(store: &Store, session: &Session, repository: &str) -> Result<Value> {
    store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
        let repository = crate::store::repo(db, p, repository)?;
        let id = s(&repository, "id");
        let episode = crate::graph_projection::current_episode(db, id)?;
        Ok(
            match crate::graph_projection::episode_row(db, id, &episode)? {
                Some(row) => {
                    json!({"status":"current","episode":episode,"nodes":row["node_count"],"edges":row["edge_count"]})
                }
                None => {
                    json!({"status":crate::jobs::projection_state(db, id, &episode)?,"episode":episode})
                }
            },
        )
    })
}

/// The code symbols of `repository`'s current code graph: nodes anchored to
/// a file that are not the file itself, a document or a concept
/// ([`crate::code_graph_arm::code_symbol`]). `None` before the graph is
/// built. One count for `setup`'s summary and `status`.
pub fn code_symbols(store: &Store, session: &Session, repository: &str) -> Option<usize> {
    store
        .authorized_read(&session.workspace, &session.capability, "read", |db, p| {
            let row = crate::store::repo(db, p, repository)?;
            let id = s(&row, "id");
            let episode = crate::graph_projection::current_episode(db, id)?;
            if crate::graph_projection::episode_row(db, id, &episode)?.is_none() {
                return Ok(None);
            }
            let handle = crate::graph_projection::resident(db, id, &episode)?;
            let graph: &graphicle::graph::Graph = &handle;
            Ok(Some(
                graph
                    .nodes()
                    .iter()
                    .filter(|node| crate::code_graph_arm::code_symbol(&node.attributes))
                    .count(),
            ))
        })
        .ok()
        .flatten()
}

/// How long a foreground build waits for a projection another process holds
/// (`graphicle serve`'s worker, a `graphicle mcp` or another sync) before it
/// reports it as `running`.
const GRAPH_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Build `repository`'s code graph now, in
/// the foreground, as `graphicle sync` does by default and `ask` and
/// `run --wait` do before they queue: drain its projection work
/// (`jobs::drain_projection`), and wait up to five minutes for a projection
/// another process holds. Only a `queued` or `running` projection is waited
/// for; any other report is returned as it is (a sync re-queues a failed
/// projection of its head set first). With `progress`, one line on standard
/// error when a build starts and one when it ends. Returns
/// [`graph_report`]'s report, with `status` `built` when the episode was
/// recorded during the call (here, or by the process it waited for), and
/// `failed` with the `error` when the projection failed (a worker or the
/// next drain retries it). A store that stays busy with another process's
/// writes past its busy timeout is waited for like a running projection,
/// never reported as a failure. A projection that cannot run here,
/// behind other work of the project, stays `queued` for `graphicle mcp`,
/// `serve` or the next sync.
pub fn build_graph(
    store: &Store,
    session: &Session,
    repository: &str,
    progress: bool,
) -> Result<Value> {
    let mut report = graph_report(store, session, repository)?;
    if !matches!(report["status"].as_str(), Some("queued" | "running")) {
        return Ok(report);
    }
    let started = std::time::Instant::now();
    let mut waiting = false;
    let mut busy_said = false;
    if progress && report["status"] == "queued" {
        let files =
            store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
                let repository = crate::store::repo(db, p, repository)?;
                Ok(crate::store::heads(db, s(&repository, "id"))?.len())
            })?;
        eprintln!("graphicle: building the code graph of {files} files...");
    }
    let mut attempts = 0;
    loop {
        attempts += 1;
        let drained = crate::jobs::drain_projection(store, repository);
        report = graph_report(store, session, repository)?;
        if report["status"] == "current" {
            report["status"] = json!("built");
            if progress {
                eprintln!(
                    "graphicle: code graph ready: {} nodes, {} edges ({:.1} s)",
                    report["nodes"],
                    report["edges"],
                    started.elapsed().as_secs_f64()
                );
            }
            return Ok(report);
        }
        // Another process held the store's write lock past its busy
        // timeout, typically while it persists a large projection: wait as
        // for a projection it runs.
        let busy = match &drained {
            Err(error) if crate::jobs::busy(error) => true,
            Err(error) => {
                report["status"] = json!("failed");
                report["error"] = json!(error.message);
                if progress {
                    eprintln!("graphicle: the code graph was not built: {}", error.message);
                }
                return Ok(report);
            }
            Ok(_) => false,
        };
        let more = started.elapsed() < GRAPH_WAIT;
        match report["status"].as_str() {
            Some("queued" | "running") if busy && more => {
                if progress && !busy_said {
                    eprintln!(
                        "graphicle: another graphicle process is writing to the store; waiting..."
                    );
                    busy_said = true;
                }
            }
            // Another process builds it: wait for that, taking the job over
            // if its lease expires.
            Some("running") if more => {
                if progress && !waiting {
                    eprintln!(
                        "graphicle: another graphicle process is building the code graph; waiting..."
                    );
                    waiting = true;
                }
            }
            // Queued again (the head set moved during the build) or briefly
            // behind other work of the project.
            Some("queued") if more && attempts < 3 => {}
            _ => break,
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if progress {
        let state = match report["status"].as_str() {
            Some("running") => "is still being built by another graphicle process",
            Some("queued") => {
                "is waiting behind other work of this project; graphicle mcp, serve or the next sync builds it"
            }
            // A sync of these files queues a failed, cancelled or skipped
            // projection again.
            Some("failed") => "was not built: its projection failed; the next sync tries again",
            _ => "was not built; the next sync of these files builds it",
        };
        eprintln!("graphicle: the code graph {state}");
    }
    Ok(report)
}

/// Build `repository`'s code
/// graph in a detached `graphicle build-graph` process, so the command
/// returns at once. It runs in its own process group with its output
/// discarded, as the capture hooks' imports do; a build it cannot finish is
/// left queued for the next drain.
pub fn spawn_graph_build(directory: &Path, repository: &str) -> Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--data-dir")
        .arg(directory)
        .args(["build-graph", "--project", repository])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Windows will not rename or delete a folder a running process has as
    // its current folder, so the build does not keep the caller's (often
    // the project) for its whole run.
    #[cfg(windows)]
    {
        if directory.is_absolute() {
            command.current_dir(directory);
        }
    }
    crate::platform::spawn_detached(&mut command)?;
    Ok(())
}
pub fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = crate::platform::open_url_command(url)
        .ok_or_else(|| Error::bad("Could not open the app URL"))?;
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(not(target_os = "windows"))]
    command.arg(url);
    if !command.status()?.success() {
        return Err(Error::bad("Could not open the app URL"));
    }
    Ok(())
}

pub fn restore(backup: &std::path::Path, directory: &std::path::Path) -> Result<Value> {
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if directory.join("reviews.sqlite3").exists() {
        return Err(Error::bad(
            "Restore into a new --data-dir; an existing store cannot be replaced",
        ));
    }
    let source =
        rusqlite::Connection::open_with_flags(backup, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = source.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let schema: bool = source.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='repo_projects')",
        [],
        |r| r.get(0),
    )?;
    if integrity != "ok" || !schema {
        return Err(Error::bad("This is not a valid Graphicle store backup"));
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(directory)?;
    if std::fs::symlink_metadata(directory)?
        .file_type()
        .is_symlink()
    {
        return Err(Error::bad("Store directory must not be a symlink"));
    }
    let destination = directory.join("reviews.sqlite3");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(&destination)?;
    if let Err(error) = source.backup(rusqlite::MAIN_DB, &destination, None) {
        let _ = std::fs::remove_file(&destination);
        return Err(error.into());
    }
    Ok(
        json!({"restored":true,"data_dir":directory,"next":"Use graphicle login with your saved workspace access key. Reconnect source accounts and model API keys in Connections; device credentials are excluded from portable backups."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A file Git tracks although `.gitignore` matches it is read by the
    /// sync's walk; an untracked file the pattern matches is not.
    #[test]
    fn a_tracked_file_that_gitignore_matches_is_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "jsonata.js\n").unwrap();
        std::fs::write(root.join("src/jsonata.js"), "function jsonata() {}\n").unwrap();
        std::fs::write(root.join("jsonata.js"), "built output\n").unwrap();
        std::fs::write(root.join("a.js"), "a\n").unwrap();
        git(&root, &["add", ".gitignore", "a.js"]);
        git(&root, &["add", "-f", "src/jsonata.js"]);
        assert_eq!(tracked_ignored(&root), [root.join("src/jsonata.js")]);
        let read: Vec<String> = folder_entries(&root)
            .flatten()
            .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
            .map(|entry| {
                let path = entry.path().strip_prefix(&root).unwrap();
                path.to_string_lossy().replace('\\', "/")
            })
            .collect();
        assert!(read.contains(&"src/jsonata.js".to_string()), "{read:?}");
        assert!(read.contains(&"a.js".to_string()), "{read:?}");
        assert!(!read.contains(&"jsonata.js".to_string()), "{read:?}");
        // A tracked file an `.ignore` file leaves out stays out, as in the
        // walk; `!pattern` in a deeper one takes a file back in.
        std::fs::create_dir_all(root.join("gen/keep")).unwrap();
        std::fs::write(root.join(".gitignore"), "jsonata.js\ngen/\n").unwrap();
        std::fs::write(root.join("gen/out.js"), "out\n").unwrap();
        std::fs::write(root.join("gen/keep/kept.js"), "kept\n").unwrap();
        git(&root, &["add", "-f", "gen/out.js", "gen/keep/kept.js"]);
        std::fs::write(root.join(".ignore"), "gen/\n").unwrap();
        std::fs::write(root.join("gen/keep/.ignore"), "!kept.js\n").unwrap();
        let tracked = tracked_ignored(&root);
        assert!(
            tracked.contains(&root.join("src/jsonata.js")),
            "{tracked:?}"
        );
        assert!(!tracked.contains(&root.join("gen/out.js")), "{tracked:?}");
        assert!(
            tracked.contains(&root.join("gen/keep/kept.js")),
            "{tracked:?}"
        );
        // Outside a Git work tree nothing is added.
        let plain = tempfile::TempDir::new().unwrap();
        assert!(tracked_ignored(plain.path()).is_empty());
        // The configuration file's sync.tracked_ignored turns it off (the
        // environment variable, unset here, would win over it).
        if std::env::var_os(TRACKED_IGNORED_VARIABLE).is_none() {
            let off: crate::model::Config =
                serde_json::from_value(serde_json::json!({"sync":{"tracked_ignored":false}}))
                    .unwrap();
            assert!(!tracked_ignored_setting(Some(&off)));
            assert!(tracked_ignored_setting(Some(
                &crate::model::Config::default()
            )));
        }
    }
}
