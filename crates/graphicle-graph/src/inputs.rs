//! The files and folders extraction reads besides the scanned files, for
//! the early stop of a full rebuild (`watch_gate`).
//!
//! A full rebuild that may record (`watch_gate::Snapshot::begin`) opens a
//! [`Session`]. While one is open, every reader that probes or reads a path
//! the scan may not cover goes through this module first: a quoted C
//! include resolved beside its file, a Python, JavaScript, Lua or shell
//! module probed by name, a Sphinx `conf.py` in the folders above a `.txt`
//! page, a folder listed for project markers, a resolved `target_file`. The
//! module notes the path and its stats (or its absence) before the reader
//! looks, so a change between the two shows as a difference when the
//! rebuild records (`watch_gate::record`). The record keeps the stats of
//! every noted path the scan does not cover, and the next rebuild stops
//! early only when each still has them.
//!
//! A reader whose inputs cannot be listed this way (a walk of a whole tree
//! without the ignore rules, a result kept for the life of the process)
//! marks the session [`uncovered`]: the rebuild then records nothing.
use crate::watch_gate::FileStat;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// How a reader used a path.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    /// Whether it is a file (`is_file`). The scan lists every file in a
    /// folder it walks, so a file probed in such a folder is covered.
    File,
    /// A folder's entries (`read_dir`). A folder the scan walks is covered.
    Listing,
    /// Whether it exists at all, or is a folder (`exists`, `is_dir`): an
    /// empty folder is in no scan.
    Any,
    /// Its content. Covered only when the file is scanned.
    Content,
}

/// What a session noted about one path.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The strongest use of the path.
    pub kind: Kind,
    /// Its stats when first noted, before any reader looked, or `None`
    /// when it did not exist.
    pub stat: Option<FileStat>,
    /// Noted after a reader looked, at least once (a `target_file` stamp in
    /// a result): a change between the look and the note shows only in its
    /// folder's times.
    pub after: bool,
}

#[derive(Default)]
struct Log {
    sessions: usize,
    overlapped: bool,
    uncovered: Option<&'static str>,
    scanned: Arc<HashSet<String>>,
    seen: HashMap<PathBuf, Entry>,
}

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static LOG: Mutex<Option<Log>> = Mutex::new(None);

fn with_log<T>(f: impl FnOnce(&mut Log) -> T) -> T {
    let mut guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Log::default))
}

/// A full rebuild's log of what extraction read. Two rebuilds in one
/// process at once share the log, and neither records.
pub struct Session(());

/// What a session noted, when the rebuild records.
pub struct Noted {
    /// Another rebuild ran in this process at the same time.
    pub overlapped: bool,
    /// A reader whose inputs cannot be listed ran.
    pub uncovered: Option<&'static str>,
    pub entries: Vec<(PathBuf, Entry)>,
}

impl Session {
    /// Open a session; `scanned` are the scanned files, whose stats the
    /// snapshot holds already (they are not noted again).
    pub fn begin(scanned: &[String]) -> Session {
        with_log(|log| {
            log.sessions += 1;
            if log.sessions > 1 {
                log.overlapped = true;
            } else {
                log.scanned = Arc::new(scanned.iter().cloned().collect());
            }
        });
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        Session(())
    }
    /// What was noted so far.
    pub fn noted(&self) -> Noted {
        with_log(|log| {
            let mut entries = log
                .seen
                .iter()
                .map(|(path, entry)| (path.clone(), entry.clone()))
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Noted {
                overlapped: log.overlapped,
                uncovered: log.uncovered,
                entries,
            }
        })
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
        with_log(|log| {
            log.sessions = log.sessions.saturating_sub(1);
            if log.sessions == 0 {
                *log = Log::default();
            }
        });
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn note(path: &Path, kind: Kind, after: bool) {
    if ACTIVE.load(Ordering::Relaxed) == 0 || path.as_os_str().is_empty() {
        return;
    }
    let path = absolute(path);
    let known = with_log(|log| {
        if log.scanned.contains(&*path.to_string_lossy()) {
            return true;
        }
        match log.seen.get_mut(&path) {
            Some(entry) => {
                entry.kind = entry.kind.max(kind);
                entry.after |= after;
                true
            }
            None => false,
        }
    });
    if known {
        return;
    }
    // Stat outside the lock; the first note of a path keeps its stats.
    let stat = FileStat::of(&path);
    with_log(|log| {
        let entry = log.seen.entry(path).or_insert(Entry { kind, stat, after });
        entry.kind = entry.kind.max(kind);
        entry.after |= after;
    });
}

/// `path.is_file()`, noted.
pub fn is_file(path: &Path) -> bool {
    note(path, Kind::File, false);
    path.is_file()
}
/// `path.exists()`, noted.
pub fn exists(path: &Path) -> bool {
    note(path, Kind::Any, false);
    path.exists()
}
/// `path.is_dir()`, noted.
pub fn is_dir(path: &Path) -> bool {
    note(path, Kind::Any, false);
    path.is_dir()
}
/// Note a file's content before it is read.
pub fn content(path: &Path) {
    note(path, Kind::Content, false);
}
/// Note a folder's entries before they are listed.
pub fn listing(path: &Path) {
    note(path, Kind::Listing, false);
}
/// Note a file a result names as resolved (`target_file`), after the
/// reader looked.
pub fn resolved(path: &Path) {
    note(path, Kind::File, true);
}
/// A reader whose inputs this module cannot list ran: the rebuild records
/// nothing.
pub fn uncovered(reader: &'static str) {
    if ACTIVE.load(Ordering::Relaxed) == 0 {
        return;
    }
    with_log(|log| {
        log.uncovered.get_or_insert(reader);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, since sessions share process state (other tests' rebuilds
    /// may note paths of their own: only paths under this test's folder
    /// are checked).
    #[test]
    fn a_session_notes_paths_before_readers_look() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (scanned, header, missing) = (root.join("a.c"), root.join("b.h"), root.join("c.h"));
        std::fs::write(&scanned, "").unwrap();
        std::fs::write(&header, "").unwrap();
        let mine = |session: &Session| {
            session
                .noted()
                .entries
                .into_iter()
                .filter(|(p, _)| p.starts_with(&root))
                .collect::<Vec<_>>()
        };
        let session = Session::begin(&[scanned.to_string_lossy().into_owned()]);
        assert!(mine(&session).is_empty());
        assert!(is_file(&scanned));
        assert!(is_file(&header));
        assert!(!is_file(&missing));
        content(&header);
        let noted = mine(&session);
        let paths = noted.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
        assert_eq!(paths, [header.clone(), missing.clone()]);
        assert_eq!(noted[0].1.kind, Kind::Content);
        assert!(noted[0].1.stat.is_some() && !noted[0].1.after);
        assert!(noted[1].1.stat.is_none());
        // A second session at once marks both.
        let other = Session::begin(&[]);
        assert!(session.noted().overlapped);
        drop(other);
        uncovered("a reader");
        assert!(session.noted().uncovered.is_some());
    }
}
