//! No child process may hold a copy of a lock file that a test relies on.
//! This is `src/fork_guard.rs` (the library tests' guard) for this test
//! binary and for the commercial edition's `tests/core_suites.rs`, which compiles this
//! file too.
//!
//! An `flock` lock belongs to an open file description, which every copy of
//! its descriptor shares. A child process starts with a copy of every
//! descriptor the test process has open at that moment, close-on-exec ones
//! included, until it execs or exits (`Command::spawn` creates it with
//! `posix_spawn` or `fork`). While a child holds such a copy, a lock its test
//! has already closed stays held, so the test's next `try_lock` of that file
//! fails: a graph build then uses a temporary tree instead of its cache.
//!
//! The two guards below exclude each other:
//! - every test that starts a process does it through [`spawn`] or under
//!   [`forking`], which is held until the child exists;
//! - a test whose result depends on flock exclusivity holds
//!   [`flock_sensitive`] for its whole body, taken before it opens anything.
//!
//! A child therefore never starts while such a test has a descriptor open,
//! and it cannot copy one opened after it started. Nothing else starts a
//! process in these binaries: the runtime library starts one only on paths
//! the suites never take in process (the document worker only in a program
//! named `graphicle`; the hook's session import, `open`, `codex mcp add` and
//! the engine of `graphicle graph` only in the command-line program; `git
//! status` for the freshness check only in a folder inside a git
//! repository, which no suite syncs in process; `git rev-parse` only when a
//! backup lands in a folder with a `.git` above it, which only setup and
//! connect make), and the commercial edition's `git` calls belong
//! to its `pr` command, which no re-run suite calls.
use std::process::{Child, Command};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

static PROCESSES: RwLock<()> = RwLock::new(());

/// Held for the whole body of a test whose result depends on flock
/// exclusivity: no child process starts while it is held.
pub fn flock_sensitive() -> RwLockReadGuard<'static, ()> {
    PROCESSES.read().unwrap_or_else(PoisonError::into_inner)
}

/// Held while a child process is created: no test that relies on flock
/// exclusivity runs meanwhile.
pub fn forking() -> RwLockWriteGuard<'static, ()> {
    PROCESSES.write().unwrap_or_else(PoisonError::into_inner)
}

/// `command.spawn()` under [`forking`].
pub fn spawn(command: &mut Command) -> std::io::Result<Child> {
    let _forking = forking();
    command.spawn()
}
