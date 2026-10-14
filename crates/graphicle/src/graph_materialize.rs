//! Reuse a verified source tree between graph projections.
//!
//! SQLite remains authoritative. Cache files are hashed before reuse; changed
//! blobs are verified and atomically replaced, and deleted paths disappear
//! before extraction. The lock stays alive for the entire graph build. A busy
//! cache uses an isolated temporary tree instead of racing another process.
use crate::{Error, Result, store::path};
use rusqlite::{Connection, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tempfile::TempDir;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub cached: bool,
    pub written: usize,
    pub reused: usize,
    pub removed: usize,
    pub bytes_written: usize,
}

pub struct SourceTree {
    root: PathBuf,
    pub report: Report,
    _lock: Option<File>,
    _temporary: Option<TempDir>,
}

impl SourceTree {
    pub fn path(&self) -> &Path {
        &self.root
    }
}

fn files(root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut pending = vec![root.to_path_buf()];
    let (mut files, mut directories) = (Vec::new(), Vec::new());
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
                return Err(Error::conflict(
                    "Graph source cache contains a non-regular entry",
                ));
            }
            if kind.is_dir() {
                pending.push(entry.path());
                directories.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    directories.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    Ok((files, directories))
}

fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn sync(
    db: &Connection,
    repository: &str,
    heads: &BTreeMap<String, String>,
    tree: &mut SourceTree,
) -> Result<()> {
    let wanted = heads
        .keys()
        .map(|relative| {
            path(relative)?;
            Ok(tree.root.join(relative))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let (present, directories) = files(&tree.root)?;
    for file in present {
        if !wanted.contains(&file) {
            fs::remove_file(file)?;
            tree.report.removed += 1;
        }
    }
    // Also handles directory-to-file and file-to-directory transitions.
    for directory in directories {
        if fs::read_dir(&directory)?.next().is_none() {
            fs::remove_dir(directory)?;
        }
    }
    let mut query =
        db.prepare_cached("SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?")?;
    for (relative, expected) in heads {
        let target = tree.root.join(relative);
        if target.is_file() && file_hash(&target)? == *expected {
            tree.report.reused += 1;
            continue;
        }
        let content: String = query
            .query_row(params![repository, expected], |row| row.get(0))
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::conflict("Head references a missing source object")
                }
                other => other.into(),
            })?;
        if format!("{:x}", Sha256::digest(content.as_bytes())) != *expected {
            return Err(Error::conflict(
                "Source object does not match its content digest",
            ));
        }
        let parent = target
            .parent()
            .ok_or_else(|| Error::bad("Source path has no parent"))?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(content.as_bytes())?;
        temporary
            .persist(target)
            .map_err(|error| Error::from(error.error))?;
        tree.report.written += 1;
        tree.report.bytes_written += content.len();
    }
    Ok(())
}

pub fn prepare(
    db: &Connection,
    repository: &str,
    heads: &BTreeMap<String, String>,
    cache: Option<&Path>,
) -> Result<SourceTree> {
    let cached = cache
        .map(|cache| -> Result<_> {
            fs::create_dir_all(cache)?;
            let root = cache.join("sources");
            if root
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                return Err(Error::conflict("Graph source cache root is a symlink"));
            }
            let lock_path = cache.join("sources.lock");
            if lock_path
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                return Err(Error::conflict("Graph source cache lock is a symlink"));
            }
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(lock_path)?;
            if lock.try_lock().is_err() {
                return Ok(None);
            }
            fs::create_dir_all(&root)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
            }
            Ok(Some(SourceTree {
                root,
                report: Report {
                    cached: true,
                    ..Report::default()
                },
                _lock: Some(lock),
                _temporary: None,
            }))
        })
        .transpose()?
        .flatten();
    let mut tree = match cached {
        Some(tree) => tree,
        None => {
            let temporary = TempDir::new()?;
            SourceTree {
                root: temporary.path().to_path_buf(),
                report: Report::default(),
                _lock: None,
                _temporary: Some(temporary),
            }
        }
    };
    sync(db, repository, heads, &mut tree)?;
    Ok(tree)
}

/// Called while deleting a project, after its authorization and snapshot
/// checks. Wait for an in-flight builder before removing derived cache data.
pub fn purge(cache: &Path) -> Result<()> {
    if !cache.exists() {
        return Ok(());
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cache.join("sources.lock"))?;
    lock.lock()?;
    fs::remove_dir_all(cache)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every test here calls `prepare` or `purge` with a cache folder, and
    // `prepare` uses the cache only when `try_lock` succeeds. Each holds
    // `crate::fork_guard::flock_sensitive` for its whole body, so no child
    // process holds a copy of a lock file it has closed.

    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE repo_objects(repository_id TEXT,sha256 TEXT,content TEXT)")
            .unwrap();
        db
    }

    fn heads(db: &Connection, files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files
            .iter()
            .map(|(path, content)| {
                let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
                db.execute(
                    "INSERT INTO repo_objects VALUES('repo',?,?)",
                    params![hash, content],
                )
                .unwrap();
                ((*path).into(), hash)
            })
            .collect()
    }

    #[test]
    fn reuse_updates_removals_and_tampering_match_authoritative_blobs() {
        let _flock = crate::fork_guard::flock_sensitive();
        let db = database();
        let cache = TempDir::new().unwrap();
        let first = heads(&db, &[("a.py", "a = 1\n"), ("dir/b.py", "b = 2\n")]);
        let tree = prepare(&db, "repo", &first, Some(cache.path())).unwrap();
        let root = tree.path().to_path_buf();
        assert_eq!(tree.report.written, 2);
        drop(tree);
        let reused = prepare(&db, "repo", &first, Some(cache.path())).unwrap();
        assert_eq!(reused.path(), root);
        assert_eq!((reused.report.written, reused.report.reused), (0, 2));
        drop(reused);
        fs::write(root.join("a.py"), "a = 9\n").unwrap();
        let repaired = prepare(&db, "repo", &first, Some(cache.path())).unwrap();
        assert_eq!((repaired.report.written, repaired.report.reused), (1, 1));
        assert_eq!(fs::read_to_string(root.join("a.py")).unwrap(), "a = 1\n");
        drop(repaired);
        // Rename, deletion, and replacing a directory with a file.
        let second = heads(&db, &[("c.py", "a = 1\n"), ("dir", "content\n")]);
        let changed = prepare(&db, "repo", &second, Some(cache.path())).unwrap();
        assert_eq!(changed.report.removed, 2);
        assert!(!root.join("a.py").exists());
        assert_eq!(fs::read_to_string(root.join("dir")).unwrap(), "content\n");
        drop(changed);
        purge(cache.path()).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn concurrent_builder_gets_an_isolated_tree_and_bad_blobs_fail() {
        let _flock = crate::fork_guard::flock_sensitive();
        let db = database();
        let cache = TempDir::new().unwrap();
        let sources = heads(&db, &[("a.py", "value = 1\n")]);
        let first = prepare(&db, "repo", &sources, Some(cache.path())).unwrap();
        let second = prepare(&db, "repo", &sources, Some(cache.path())).unwrap();
        assert!(!second.report.cached);
        assert_ne!(first.path(), second.path());
        assert_eq!(
            fs::read_to_string(second.path().join("a.py")).unwrap(),
            "value = 1\n"
        );
        drop((first, second));
        db.execute("UPDATE repo_objects SET content='corrupt'", [])
            .unwrap();
        fs::remove_file(cache.path().join("sources/a.py")).unwrap();
        assert!(prepare(&db, "repo", &sources, Some(cache.path())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_redirect_cache_writes() {
        let _flock = crate::fork_guard::flock_sensitive();
        let db = database();
        let cache = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret"), "untouched").unwrap();
        fs::create_dir_all(cache.path().join("sources")).unwrap();
        std::os::unix::fs::symlink(outside.path(), cache.path().join("sources/escape")).unwrap();
        let sources = heads(&db, &[("escape/secret", "overwrite")]);
        assert!(prepare(&db, "repo", &sources, Some(cache.path())).is_err());
        assert_eq!(
            fs::read_to_string(outside.path().join("secret")).unwrap(),
            "untouched"
        );
    }
}
