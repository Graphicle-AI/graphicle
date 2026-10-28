//! The files Graphicle changes outside its data folder for agents (`setup`,
//! `connect`). Every change is:
//! - marked, with comment markers or a named entry;
//! - preceded by a backup of the file as `<file>.graphicle-bak`, taken once:
//!   a later change keeps the first backup;
//! - idempotent: a run that would change nothing writes nothing;
//! - recorded in the data folder's `agent-changes.json`, so that `graphicle
//!   disconnect` and `graphicle uninstall` undo it ([`Change`]).
//!
//! Undoing a change puts the backup back, bytes and permission bits, when
//! the file is then the same as its backup; otherwise only Graphicle's part
//! is removed and the backup is kept.
//!
//! A backup inside a Git working tree is kept out of Git: the repository's
//! `.git/info/exclude` gets a marked `*.graphicle-bak` line (recorded, and
//! removed by `graphicle disconnect` once no backup is left), so a copy of
//! an ignored file such as `.claude/settings.local.json` never shows in
//! `git status`. A `.claude/settings.local.json` that setup creates, whose
//! hooks name the program and the data folder, gets its own line in the
//! same block. Folder syncs never read backups either
//! ([`crate::local::folder_walk`]).
use crate::platform::CanonicalExt;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

/// What a backup's name adds to the file's.
pub const BACKUP_SUFFIX: &str = ".graphicle-bak";
/// The record of changes, in the data folder.
const RECORD: &str = "agent-changes.json";

/// `<file>.graphicle-bak`, beside `path`.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(BACKUP_SUFFIX);
    path.with_file_name(name)
}

/// Back up `path` before Graphicle changes it: copy it, with its permission
/// bits, to [`backup_path`] unless a backup is there already (the first one
/// is kept). Returns the backup when there is one; a file that does not
/// exist, or is not a regular file, has none.
pub fn backup_once(path: &Path) -> Result<Option<PathBuf>> {
    let backup = backup_path(path);
    if std::fs::symlink_metadata(&backup).is_ok() {
        return Ok(Some(backup));
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(None);
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&backup)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    set_mode(&backup, mode_of(&meta))?;
    Ok(Some(backup))
}

/// [`backup_once`], and when the backup is new and lies in a Git working
/// tree, the repository's `.git/info/exclude` keeps it out of Git
/// ([`exclude_backups`]); the change is recorded in the data folder `data`.
pub fn backup_in(data: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let existed = std::fs::symlink_metadata(backup_path(path)).is_ok();
    let backup = backup_once(path)?;
    if !existed && let Some(backup) = &backup {
        exclude_backups(data, backup)?;
    }
    Ok(backup)
}

/// The first line of the block Graphicle adds to `.git/info/exclude`.
pub const EXCLUDE_START: &str =
    "# graphicle-backups-start (added by graphicle setup; graphicle disconnect removes it)";
/// The block's last line.
pub const EXCLUDE_END: &str = "# graphicle-backups-end";
/// The pattern the block holds.
const EXCLUDE_PATTERN: &str = "*.graphicle-bak";

/// `git -C folder <args>`'s one line of output, with the variables a Git
/// hook sets removed; `None` when it fails.
fn git_line(folder: &Path, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new("git");
    // Git refuses Windows' verbatim `\\?\` paths.
    command
        .arg("-C")
        .arg(crate::platform::simplified(folder.to_path_buf()))
        .args(args);
    for variable in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX"] {
        command.env_remove(variable);
    }
    let output = command
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim_end_matches(['\n', '\r']);
    (output.status.success() && !text.is_empty() && !text.contains('\n')).then(|| text.to_string())
}

/// The working tree's top level and the exclude file of the Git repository
/// whose working tree holds `folder`; `None` outside one (and inside a Git
/// folder itself). Git is run only when a `.git` lies at or above `folder`.
pub(crate) fn work_tree(folder: &Path) -> Option<(PathBuf, PathBuf)> {
    folder.ancestors().find(|dir| dir.join(".git").exists())?;
    if git_line(folder, &["rev-parse", "--is-inside-work-tree"])? != "true" {
        return None;
    }
    let root = PathBuf::from(git_line(folder, &["rev-parse", "--show-toplevel"])?);
    let exclude = git_line(&root, &["rev-parse", "--git-path", "info/exclude"])?;
    let exclude = root.join(exclude);
    Some((root.canonical().unwrap_or(root), exclude))
}

/// The lines `start..end` of the backups block in an exclude file's lines.
fn exclude_block(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines.iter().position(|line| line.trim() == EXCLUDE_START)?;
    let end = (start + 1..lines.len()).find(|i| lines[*i].trim() == EXCLUDE_END)?;
    Some((start, end + 1))
}

/// Keep `backup` out of Git when it lies in a working tree: a marked
/// `*.graphicle-bak` block in the repository's exclude file (backed up
/// first, recorded in `data`), unless the file has that pattern already.
pub fn exclude_backups(data: &Path, backup: &Path) -> Result<()> {
    exclude_in_block(data, backup, EXCLUDE_PATTERN)
}

/// Keep a file Graphicle created in a working tree out of Git, such as
/// Claude Code's personal `.claude/settings.local.json`, whose hooks name the
/// program and the data folder by absolute path: its path from the working
/// tree's top level joins the marked block, which `graphicle disconnect`
/// removes.
pub fn exclude_created(data: &Path, file: &Path) -> Result<()> {
    let Some((root, _)) = file.parent().and_then(work_tree) else {
        return Ok(());
    };
    let file = std::path::absolute(file)?;
    let file = match (file.parent(), file.file_name()) {
        (Some(parent), Some(name)) => parent.canonical()?.join(name),
        _ => return Ok(()),
    };
    let Ok(relative) = file.strip_prefix(&root) else {
        return Ok(());
    };
    let pattern = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
    exclude_in_block(data, &file, &pattern)
}

/// Why a write failed when the file system refused it (a read-only folder,
/// Claude Code's or Codex's sandbox): `not writable: <the OS error>`; `None`
/// for any other error.
pub fn write_failure(error: &Error) -> Option<String> {
    error
        .message
        .strip_prefix("Local I/O failed: ")
        .map(|os| format!("not writable: {os}"))
}

/// `error`, from a write of `path`, naming the file when the file system
/// refused the write: `<path>: not writable: <the OS error>`.
pub fn naming(path: &Path, error: Error) -> Error {
    match write_failure(&error) {
        Some(reason) => Error::bad(format!("{}: {reason}", path.display())),
        None => error,
    }
}

/// A line Graphicle could not add to a repository's exclude file.
#[derive(Clone, Debug, PartialEq)]
pub struct Unexcluded {
    /// The exclude file.
    pub file: PathBuf,
    /// The line it lacks: `*.graphicle-bak`, or a created file's path.
    pub pattern: String,
    /// Why: `not writable: <the OS error>`.
    pub reason: String,
}

thread_local! {
    /// The lines this thread could not add to exclude files, until
    /// [`take_unexcluded`] takes them.
    static UNEXCLUDED: std::cell::RefCell<Vec<Unexcluded>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The lines this thread could not add to an exclude file since the last
/// call, each once: setup's summary gives them to add by hand.
pub fn take_unexcluded() -> Vec<Unexcluded> {
    UNEXCLUDED.with(|list| std::mem::take(&mut *list.borrow_mut()))
}

/// Add `pattern` to the marked block of the exclude file of the working tree
/// that holds `file` (the block is created, after a backup of the file, when
/// it is missing; nothing is added when the file has the pattern outside the
/// block), and record the change in `data`. An exclude file that cannot be
/// written (a read-only `.git`, as in Codex's sandbox) is left as it is:
/// the line only keeps a file out of `git status`, so the change it goes
/// with is still made, and [`take_unexcluded`] has the line.
fn exclude_in_block(data: &Path, file: &Path, pattern: &str) -> Result<()> {
    let Some(folder) = file.parent() else {
        return Ok(());
    };
    let Some((root, exclude)) = work_tree(folder) else {
        return Ok(());
    };
    if exclude.is_symlink() {
        return Ok(());
    }
    let existed = exclude.exists();
    let text = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut lines: Vec<&str> = text.split('\n').collect();
    let written = match exclude_block(&lines) {
        Some((start, end)) => {
            if lines[start..end].iter().any(|line| line.trim() == pattern) {
                Ok(())
            } else {
                lines.insert(end - 1, pattern);
                write(&exclude, lines.join("\n").as_bytes(), 0o644)
            }
        }
        None if lines.iter().any(|line| line.trim() == pattern) => return Ok(()),
        None => {
            let mut updated = text.clone();
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(&format!("{EXCLUDE_START}\n{pattern}\n{EXCLUDE_END}\n"));
            backup_once(&exclude).and_then(|_| write(&exclude, updated.as_bytes(), 0o644))
        }
    };
    if let Err(error) = written {
        let Some(reason) = write_failure(&error) else {
            return Err(error);
        };
        let line = Unexcluded {
            file: exclude,
            pattern: pattern.to_string(),
            reason,
        };
        UNEXCLUDED.with(|list| {
            let mut list = list.borrow_mut();
            if !list
                .iter()
                .any(|known| known.file == line.file && known.pattern == line.pattern)
            {
                list.push(line);
            }
        });
        return Ok(());
    }
    record(
        data,
        Change {
            path: std::path::absolute(&exclude)?,
            kind: "exclude".into(),
            folder: Some(root),
            created: !existed,
            ..Change::default()
        },
    )
}

/// `text` (an exclude file) without the backups block; `None` when it has
/// none.
pub fn remove_exclude(text: &str) -> Option<String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    let (start, end) = exclude_block(&lines)?;
    lines.drain(start..end);
    Some(lines.join("\n"))
}

/// The `*.graphicle-bak` files under `root` (Git's folder, dependency and
/// build folders skipped), at most `limit` entries looked at.
pub fn backups_under(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut walk = ignore::WalkBuilder::new(root);
    walk.hidden(false)
        .follow_links(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false);
    walk.filter_entry(|entry| {
        entry.depth() == 0
            || !crate::local::SKIPPED_NAMES.contains(&entry.file_name().to_string_lossy().as_ref())
    });
    walk.build()
        .take(limit)
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(BACKUP_SUFFIX))
        .map(|entry| entry.into_path())
        .collect()
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}
#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> u32 {
    0o644
}
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}

/// Replace `path` with `bytes` through a temporary file in its folder, with
/// the permission bits `mode`. A symlink is never replaced.
fn replace(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if path.is_symlink() {
        return Err(Error::bad(format!(
            "Refusing to replace the symlink {}",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::bad("File has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".graphicle-{}", crate::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        set_mode(&temporary, mode)?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Write `bytes` to `path`, keeping the permission bits of the file it
/// replaces (`mode` for a new file).
pub fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mode = std::fs::metadata(path)
        .map(|meta| mode_of(&meta))
        .unwrap_or(mode);
    replace(path, bytes, mode)
}

/// Write a client's JSON file after a change that leaves `document`: the
/// in-place edit `edited` of the file's text `original` when it reads as
/// `document` (every other byte kept: indentation, compact arrays,
/// comments, the final newline), else `document` in the file's style
/// ([`crate::json_edit::render`]). A file with comments is never rewritten
/// whole: that is an error. The file keeps its permission bits (a new one
/// is private).
pub fn write_json(
    path: &Path,
    original: Option<&str>,
    edited: Option<String>,
    document: &Value,
) -> Result<()> {
    let text = match edited {
        Some(text) if crate::json_edit::parse(&text).as_ref() == Some(document) => text,
        _ => {
            if original.is_some_and(crate::json_edit::has_comments) {
                return Err(Error::bad(format!(
                    "{} has comments that a rewrite would drop, so Graphicle left it alone",
                    path.display()
                )));
            }
            crate::json_edit::render(document, original)
        }
    };
    write(path, text.as_bytes(), 0o600)
}

/// Put `backup` back as `path`, its bytes and permission bits, and remove it.
pub fn restore(path: &Path, backup: &Path) -> Result<()> {
    let bytes = std::fs::read(backup)?;
    let mode = mode_of(&std::fs::metadata(backup)?);
    replace(path, &bytes, mode)?;
    std::fs::remove_file(backup)?;
    Ok(())
}

/// One change Graphicle made outside its data folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// The file changed, absolute.
    pub path: PathBuf,
    /// What the change is:
    /// - `mcp`: an entry in a JSON MCP configuration (`group` holds it);
    /// - `codex`: a `[mcp_servers.<entry>]` table of Codex's `config.toml`;
    /// - `hooks`: session-capture hooks in a JSON hooks file;
    /// - `instructions`: the marked Graphicle section of AGENTS.md or
    ///   CLAUDE.md;
    /// - `git-hook`: the marked block of a Git hook that keeps `folder`
    ///   fresh;
    /// - `removed-mcp`, `removed-codex`: a Graphify entry a switch removed,
    ///   put back on disconnect (`replaced` holds it);
    /// - `exclude`: the `*.graphicle-bak` block of a repository's
    ///   `.git/info/exclude` (`folder` is the working tree);
    /// - `muse-plugin`: Graphicle's Muse Code plugin (`path` is its manifest
    ///   in the data folder), installed for every project.
    pub kind: String,
    /// The agent client, for entries and capture hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// The project folder the change belongs to, when it belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// The project an entry names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The entry's name (`mcp`, `codex`, and the `removed-*` kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// The JSON object holding the entry (`mcpServers` or `servers`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Graphicle created the file.
    #[serde(default)]
    pub created: bool,
    /// What the change replaced, put back when it is undone: Graphify's
    /// section (`{"text": …}`), entry (`{"value": …}`) or Codex table
    /// (`{"text": …}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced: Option<Value>,
}

impl Change {
    /// The fields that identify a change: one record per file, kind, entry
    /// and folder.
    fn key(&self) -> (&Path, &str, Option<&str>, Option<&Path>) {
        (
            &self.path,
            &self.kind,
            self.entry.as_deref(),
            self.folder.as_deref(),
        )
    }
}

/// The record of changes in a data folder.
#[derive(Default, Serialize, Deserialize)]
pub struct Changes {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub changes: Vec<Change>,
}

/// The changes recorded in the data folder `directory`: none when there is
/// no record; an error when the record cannot be read, so that nothing
/// overwrites it (it holds which files Graphicle created).
pub fn load(directory: &Path) -> Result<Changes> {
    let path = directory.join(RECORD);
    let unreadable = || {
        Error::bad(format!(
            "The record of the files Graphicle changed, {}, cannot be read; fix it or move it away, then run the command again",
            path.display()
        ))
    };
    if path.is_symlink() {
        return Err(unreadable());
    }
    match std::fs::read(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Changes::default()),
        Err(_) => Err(unreadable()),
        Ok(bytes) if bytes.len() > 4 * 1024 * 1024 => Err(unreadable()),
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| unreadable()),
    }
}

/// [`load`] for reading only: none when the record is missing or cannot be
/// read.
pub fn recorded(directory: &Path) -> Vec<Change> {
    load(directory)
        .map(|changes| changes.changes)
        .unwrap_or_default()
}

/// The record's lock (`agent-changes.lock` in the data folder), held until
/// the returned file is dropped: two processes never change the record at
/// once.
fn lock(directory: &Path) -> Result<std::fs::File> {
    let path = directory.join("agent-changes.lock");
    if path.is_symlink() {
        return Err(Error::bad("The record's lock file cannot be a symlink"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    file.lock()?;
    Ok(file)
}

/// Change the record of the data folder `directory` under its lock: `change`
/// gets the record as it is now and edits it; the result is saved.
pub fn update<T>(directory: &Path, change: impl FnOnce(&mut Changes) -> T) -> Result<T> {
    let _lock = lock(directory)?;
    let mut changes = load(directory)?;
    let result = change(&mut changes);
    save(directory, &changes)?;
    Ok(result)
}

/// Save `changes` in the data folder `directory`; an empty record removes
/// the file.
fn save(directory: &Path, changes: &Changes) -> Result<()> {
    let path = directory.join(RECORD);
    if changes.changes.is_empty() {
        if std::fs::symlink_metadata(&path).is_ok() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    let record = Changes {
        version: 1,
        changes: changes.changes.clone(),
    };
    crate::local::write_private(&path, &serde_json::to_vec_pretty(&record)?)
}

/// Record `change` in the data folder `directory`, unless the same change
/// (file, kind, entry and folder) is recorded: the first record keeps what
/// was there before Graphicle (`created`, `replaced`).
pub fn record(directory: &Path, change: Change) -> Result<()> {
    if load(directory)?.changes.iter().any(|existing| {
        existing.key() == change.key() && (existing.replaced.is_some() || change.replaced.is_none())
    }) {
        return Ok(());
    }
    update(directory, |changes| {
        match changes
            .changes
            .iter_mut()
            .find(|existing| existing.key() == change.key())
        {
            // A later change may name what it replaced when the first did not.
            Some(existing) => {
                if existing.replaced.is_none() && change.replaced.is_some() {
                    existing.replaced = change.replaced;
                }
            }
            None => changes.changes.push(change),
        }
    })
}

/// Whether `entry`, a client's MCP server entry, runs a Graphicle program's
/// MCP server: its command is a program named `graphicle…` and its
/// arguments include `mcp`.
pub fn is_graphicle_entry(entry: &Value) -> bool {
    let command = entry["command"].as_str().unwrap_or_default();
    let name = Path::new(command)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.starts_with("graphicle")
        && entry["args"]
            .as_array()
            .is_some_and(|args| args.iter().any(|arg| arg == "mcp"))
}

/// The arguments of an MCP entry, as text.
fn arguments(entry: &Value) -> Vec<&str> {
    entry["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// The data folder a `graphicle` command line opens: its `--data-dir`, else
/// the platform's default folder, made canonical when it exists.
pub fn store_of<'a>(args: impl IntoIterator<Item = &'a str>) -> PathBuf {
    let args: Vec<&str> = args.into_iter().collect();
    let named = args
        .iter()
        .position(|arg| *arg == "--data-dir")
        .and_then(|at| args.get(at + 1))
        .map(PathBuf::from)
        .unwrap_or_else(crate::local::default_data_dir);
    named.canonical().unwrap_or(named)
}

/// Whether `data` (a data folder) is the store a `graphicle` command line
/// with `args` opens.
pub fn same_store<'a>(args: impl IntoIterator<Item = &'a str>, data: &Path) -> bool {
    let data = data.canonical().unwrap_or_else(|_| data.to_path_buf());
    store_of(args) == data
}

/// Whether an MCP entry serves the data folder `data`.
pub fn entry_uses_store(entry: &Value, data: &Path) -> bool {
    same_store(arguments(entry), data)
}

/// The project an MCP entry's `--project` names: `None` for an entry that
/// follows the folder its client starts it in (`.` or VS Code's
/// `${workspaceFolder}`) or names none.
pub fn entry_project(entry: &Value) -> Option<String> {
    let args = arguments(entry);
    let named = args
        .iter()
        .position(|arg| *arg == "--project")
        .and_then(|at| args.get(at + 1))?;
    (!matches!(*named, "." | "${workspaceFolder}")).then(|| named.to_string())
}

/// Whether a Graphicle MCP entry belongs to `project` of the store `data`:
/// it opens that store, and names that project or follows its folder.
pub fn entry_serves(entry: &Value, data: &Path, project: Option<&str>) -> bool {
    is_graphicle_entry(entry)
        && entry_uses_store(entry, data)
        && project.is_none_or(|project| entry_project(entry).is_none_or(|named| named == project))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `.claude/settings.local.json` that setup created holds absolute
    /// paths: its line joins the marked block once, beside the backups'
    /// pattern, and goes with the block.
    #[test]
    fn a_created_settings_file_is_kept_out_of_git_with_the_block() {
        // Its git children must not copy a lock file that a
        // flock-sensitive test has open (see fork_guard.rs).
        #[cfg(unix)]
        let _forking = crate::fork_guard::forking();
        let dir = tempfile::tempdir().unwrap();
        let initialized = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(dir.path())
            .status()
            .is_ok_and(|status| status.success());
        if !initialized {
            return; // no Git here
        }
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let settings = dir.path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"{}\n").unwrap();
        exclude_created(&data, &settings).unwrap();
        exclude_created(&data, &settings).unwrap();
        let backup = dir.path().join("a.json.graphicle-bak");
        std::fs::write(&backup, b"{}\n").unwrap();
        exclude_backups(&data, &backup).unwrap();
        let exclude = std::fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
        assert_eq!(
            exclude.matches("\n/.claude/settings.local.json\n").count(),
            1,
            "{exclude}"
        );
        assert!(exclude.contains("\n*.graphicle-bak\n"), "{exclude}");
        let removed = remove_exclude(&exclude).unwrap();
        assert!(!removed.contains("settings.local.json"), "{removed}");
        assert!(!removed.contains(EXCLUDE_START), "{removed}");
    }

    #[test]
    fn the_first_backup_is_kept_and_restored_with_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        assert_eq!(backup_once(&file).unwrap(), None, "nothing to back up");
        std::fs::write(&file, b"{\"a\":1}\n").unwrap();
        set_mode(&file, 0o640).unwrap();
        let backup = backup_once(&file).unwrap().unwrap();
        assert_eq!(backup, dir.path().join("settings.json.graphicle-bak"));
        write(&file, b"{\"a\":2}\n", 0o600).unwrap();
        assert_eq!(backup_once(&file).unwrap(), Some(backup.clone()));
        assert_eq!(std::fs::read(&backup).unwrap(), b"{\"a\":1}\n");
        #[cfg(unix)]
        assert_eq!(
            mode_of(&std::fs::metadata(&file).unwrap()),
            0o640,
            "a write keeps the file's mode"
        );
        set_mode(&file, 0o600).unwrap();
        restore(&file, &backup).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"{\"a\":1}\n");
        #[cfg(unix)]
        assert_eq!(mode_of(&std::fs::metadata(&file).unwrap()), 0o640);
        assert!(!backup.exists());
    }

    #[test]
    fn a_change_is_recorded_once_and_an_empty_record_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let change = Change {
            path: dir.path().join(".mcp.json"),
            kind: "mcp".into(),
            client: Some("claude-code".into()),
            entry: Some("graphicle".into()),
            group: Some("mcpServers".into()),
            created: true,
            ..Change::default()
        };
        record(dir.path(), change.clone()).unwrap();
        record(
            dir.path(),
            Change {
                created: false,
                ..change.clone()
            },
        )
        .unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.changes, vec![change]);
        update(dir.path(), |changes| changes.changes.clear()).unwrap();
        assert!(!dir.path().join(RECORD).exists());
        // A record that cannot be read is never overwritten.
        std::fs::write(dir.path().join(RECORD), b"{not json").unwrap();
        let refused = record(
            dir.path(),
            Change {
                path: dir.path().join("AGENTS.md"),
                kind: "instructions".into(),
                ..Change::default()
            },
        )
        .unwrap_err();
        assert!(
            refused.message.contains("cannot be read"),
            "{}",
            refused.message
        );
        assert_eq!(
            std::fs::read(dir.path().join(RECORD)).unwrap(),
            b"{not json"
        );
        assert!(recorded(dir.path()).is_empty());
    }

    #[test]
    fn entries_belong_to_their_store_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().canonical().unwrap();
        let named = |store: &Path, project: &str| json!({"command":"/bin/graphicle","args":["--data-dir",store,"mcp","--project",project]});
        let entry = named(&data, "p1");
        assert!(entry_uses_store(&entry, &data));
        assert!(entry_serves(&entry, &data, Some("p1")));
        assert!(!entry_serves(&entry, &data, Some("p2")));
        assert!(entry_serves(&entry, &data, None));
        assert!(!entry_serves(
            &named(Path::new("/elsewhere"), "p1"),
            &data,
            Some("p1")
        ));
        let follows =
            json!({"command":"graphicle","args":["--data-dir",data,"mcp","--project","."]});
        assert_eq!(entry_project(&follows), None);
        assert!(entry_serves(&follows, &data, Some("p1")));
        assert_eq!(entry_project(&entry).as_deref(), Some("p1"));
    }

    #[test]
    fn graphicle_entries_run_a_graphicle_programs_mcp_server() {
        assert!(is_graphicle_entry(
            &json!({"command":"/opt/homebrew/bin/graphicle","args":["mcp","--project","."]})
        ));
        assert!(is_graphicle_entry(
            &json!({"command":"/usr/local/bin/graphicle-0.1.0","args":["--data-dir","/d","mcp"]})
        ));
        assert!(!is_graphicle_entry(
            &json!({"command":"graphicle","args":["conflicting"]})
        ));
        assert!(!is_graphicle_entry(
            &json!({"command":"npx","args":["graphicle-mcp","mcp"]})
        ));
    }
}
