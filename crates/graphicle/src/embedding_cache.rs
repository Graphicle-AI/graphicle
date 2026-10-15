//! Opt-in passage-embedding cache shared between stores.
//!
//! `GRAPHICLE_EMBEDDING_CACHE` names a SQLite file that maps
//! `(model key, SHA-256 of the embedded text)` to the vector the model
//! returned. A store that embeds a passage another store already embedded
//! with the same model reuses the vector instead of calling the model again.
//! Vectors are kept as little-endian `f64`, so a hit is bit-identical to the
//! model's answer.
//!
//! The key is [`crate::model::key`], which covers the provider, the model and,
//! for local models, the manifest fingerprint, so a changed model or prefix
//! never matches an old entry. Remote providers are assumed to serve fixed
//! weights for a model name. Share one file only within one trust domain: a
//! hit reveals that the same text was embedded before.
use crate::platform::CanonicalExt;
use crate::{Error, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, types::ValueRef};
use std::path::Path;
use std::time::Duration;

pub const ENV: &str = "GRAPHICLE_EMBEDDING_CACHE";

pub struct Cache {
    // Field order matters: SQLite must close before the last permission guard.
    db: Connection,
    namespace: String,
    #[cfg(unix)]
    _private_files: PrivateFiles,
}

impl Cache {
    /// The cache named by [`ENV`] for passages embedded under `model_key`,
    /// or `None` when the variable is unset or empty.
    pub fn from_env(model_key: &str) -> Result<Option<Self>> {
        match std::env::var_os(ENV) {
            Some(path) if !path.is_empty() => Self::open(Path::new(&path), model_key).map(Some),
            _ => Ok(None),
        }
    }

    pub fn open(path: &Path, model_key: &str) -> Result<Self> {
        if path.is_symlink() {
            return Err(Error::bad("Embedding cache cannot be a symlink"));
        }
        // SQLite's NOFOLLOW rejects links in ancestors too (for example
        // macOS /var -> /private/var). Resolve the parent, never the final
        // component, so ordinary cache paths work without following a file link.
        let name = path
            .file_name()
            .ok_or_else(|| Error::bad("Embedding cache must name a file"))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonical()?;
        let path = parent.join(name);
        // SQLite inherits the database mode when it creates WAL/SHM files.
        // Restrict the main file before SQLite can write, and repair existing
        // sidecars too: chmod on the database alone leaves their data exposed.
        #[cfg(unix)]
        let private_files = PrivateFiles::acquire(&path)?;
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        // Several embedding jobs may share one file; writes are short.
        db.busy_timeout(Duration::from_secs(60))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS am_embedding_cache(
                model_key TEXT NOT NULL,
                text_sha256 TEXT NOT NULL,
                vector BLOB NOT NULL,
                PRIMARY KEY(model_key, text_sha256)
            ) WITHOUT ROWID",
        )?;
        Ok(Self {
            db,
            namespace: format!("passage:{model_key}"),
            #[cfg(unix)]
            _private_files: private_files,
        })
    }

    /// The cached vector for `text`, if any. A malformed entry is a miss.
    pub fn get(&self, text: &str) -> Result<Option<Vec<f64>>> {
        let vector = self
            .db
            .prepare_cached(
                "SELECT vector FROM am_embedding_cache WHERE model_key=? AND text_sha256=?",
            )?
            .query_row(params![self.namespace, sha(text)], |row| {
                Ok(match row.get_ref(0)? {
                    ValueRef::Blob(blob) => decode(blob),
                    _ => None,
                })
            })
            .optional()?;
        Ok(vector.flatten())
    }

    /// Record vectors for texts; an existing entry is kept.
    pub fn put<'a>(&self, entries: impl IntoIterator<Item = (&'a str, &'a [f64])>) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        {
            let mut insert =
                tx.prepare_cached("INSERT OR IGNORE INTO am_embedding_cache VALUES(?,?,?)")?;
            for (text, vector) in entries {
                if vector.is_empty() || !vector.iter().all(|x| x.is_finite()) {
                    continue;
                }
                insert.execute(params![self.namespace, sha(text), encode(vector)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// Restricts the files before the first live connection, and holds that
/// permission check valid until the last connection closes. POSIX record
/// locks belong to the process, and closing any
/// descriptor of a file releases all of them, including those SQLite holds for
/// a live connection (sqlite.org/howtocorrupt.html, section 2.2). Without its
/// WAL-index lock, another process takes the connection for gone and resets
/// the shared index under it, which corrupts the file when several jobs share
/// it. Only harden when no cache connection is alive; after all of them close,
/// a cleared or recreated file at the same path needs a new permission check.
#[cfg(unix)]
struct PrivateFiles(std::path::PathBuf);

#[cfg(unix)]
fn private_handles()
-> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, usize>> {
    use std::sync::{Mutex, OnceLock};
    static HANDLES: OnceLock<Mutex<std::collections::HashMap<std::path::PathBuf, usize>>> =
        OnceLock::new();
    HANDLES.get_or_init(Default::default)
}

#[cfg(unix)]
impl PrivateFiles {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        let mut handles = private_handles()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !handles.contains_key(path) {
            private_files(path)?;
        }
        *handles.entry(path.to_path_buf()).or_default() += 1;
        Ok(Self(path.to_path_buf()))
    }
}

#[cfg(unix)]
impl Drop for PrivateFiles {
    fn drop(&mut self) {
        let mut handles = private_handles()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = handles.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                handles.remove(&self.0);
            }
        }
    }
}

#[cfg(unix)]
fn private_files(path: &Path) -> std::io::Result<()> {
    use std::fs::{OpenOptions, Permissions};
    use std::io::ErrorKind;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(suffix.is_empty())
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(Path::new(&name));
        match file {
            Ok(file) => file.set_permissions(Permissions::from_mode(0o600))?,
            Err(error) if !suffix.is_empty() && error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn sha(text: &str) -> String {
    graphicle_memory::text::sha256(text.as_bytes())
}

fn encode(vector: &[f64]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn decode(blob: &[u8]) -> Option<Vec<f64>> {
    if blob.is_empty() || !blob.len().is_multiple_of(8) {
        return None;
    }
    let vector: Vec<f64> = blob
        .chunks_exact(8)
        .map(|bytes| f64::from_le_bytes(bytes.try_into().expect("8-byte chunk")))
        .collect();
    vector.iter().all(|x| x.is_finite()).then_some(vector)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_exact_vectors_per_model_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let cache = Cache::open(&path, "model-a").unwrap();
        let vector = vec![0.1, -2.5e-300, 1.0 / 3.0];
        assert_eq!(cache.get("title\ntext").unwrap(), None);
        cache.put([("title\ntext", vector.as_slice())]).unwrap();
        assert_eq!(cache.get("title\ntext").unwrap(), Some(vector.clone()));
        assert_eq!(cache.get("title\nother").unwrap(), None);
        // Another model key never sees the entry; a second handle on the
        // same file does, and an existing entry is not overwritten.
        let other = Cache::open(&path, "model-b").unwrap();
        assert_eq!(other.get("title\ntext").unwrap(), None);
        let again = Cache::open(&path, "model-a").unwrap();
        again.put([("title\ntext", [9.0].as_slice())]).unwrap();
        assert_eq!(again.get("title\ntext").unwrap(), Some(vector));
    }

    #[test]
    fn skips_non_finite_and_rejects_malformed_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("cache.sqlite"), "m").unwrap();
        cache
            .put([("nan", [f64::NAN].as_slice()), ("empty", [].as_slice())])
            .unwrap();
        assert_eq!(cache.get("nan").unwrap(), None);
        assert_eq!(cache.get("empty").unwrap(), None);
        assert_eq!(decode(&[0u8; 7]), None);
        assert_eq!(decode(&f64::INFINITY.to_le_bytes()), None);
    }

    /// Whether another process sees a read lock on byte `offset` of `path`.
    /// A child probes it, so this process never opens (and closes) the file.
    /// The child is a fork that never execs: it holds a copy of every
    /// descriptor of this test process until it exits, so it is created and
    /// reaped under [`crate::fork_guard::forking`], while no test that relies
    /// on flock exclusivity runs.
    #[cfg(unix)]
    fn read_locked_elsewhere(path: &Path, offset: i64) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let _forking = crate::fork_guard::forking();
        // SAFETY: the child calls only async-signal-safe functions, then _exit.
        unsafe {
            match libc::fork() {
                -1 => panic!("fork failed"),
                0 => {
                    let fd = libc::open(name.as_ptr(), libc::O_RDWR);
                    let mut lock: libc::flock = std::mem::zeroed();
                    lock.l_type = libc::F_WRLCK as _;
                    lock.l_whence = libc::SEEK_SET as _;
                    lock.l_start = offset as _;
                    lock.l_len = 1;
                    let held = fd >= 0
                        && libc::fcntl(fd, libc::F_GETLK, &mut lock) == 0
                        && lock.l_type == libc::F_RDLCK as libc::c_short;
                    libc::_exit(if held { 0 } else { 1 })
                }
                child => {
                    let mut status = 0;
                    assert_eq!(libc::waitpid(child, &mut status, 0), child);
                    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn another_handle_keeps_the_live_ones_locks() {
        // Every process with a live WAL connection read-locks byte 128 of the
        // -shm file (SQLite's UNIX_SHM_DMS); a process without it may reset
        // the shared WAL index under that connection.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let shm = dir.path().canonical().unwrap().join("cache.sqlite-shm");
        let first = Cache::open(&path, "m").unwrap();
        first.put([("a", [1.0].as_slice())]).unwrap();
        assert!(read_locked_elsewhere(&shm, 128));
        let second = Cache::open(&path, "m").unwrap();
        assert!(
            read_locked_elsewhere(&shm, 128),
            "opening a second handle released the first one's locks"
        );
        drop(first);
        second.put([("b", [2.0].as_slice())]).unwrap();
        assert!(read_locked_elsewhere(&shm, 128));
        assert_eq!(second.get("a").unwrap(), Some(vec![1.0]));
    }

    #[cfg(unix)]
    #[test]
    fn keeps_cache_files_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let cache = Cache::open(&path, "m").unwrap();
        cache.put([("a", [1.0].as_slice())]).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let file = dir.path().join(format!("cache.sqlite{suffix}"));
            let mode = std::fs::symlink_metadata(&file)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{suffix} is {mode:o}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rehardens_recreated_cache_after_last_handle_closes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        drop(Cache::open(&path, "m").unwrap());
        // A running service can clear and recreate a cache at the same path.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, []).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let cache = Cache::open(&path, "m").unwrap();
        cache.put([("private passage", [1.0].as_slice())]).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let file = dir.path().join(format!("cache.sqlite{suffix}"));
            let mode = std::fs::symlink_metadata(file)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "recreated {suffix} is {mode:o}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_cache() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.sqlite");
        std::fs::write(&target, b"").unwrap();
        let link = dir.path().join("link.sqlite");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(Cache::open(&link, "m").is_err());
    }
}
