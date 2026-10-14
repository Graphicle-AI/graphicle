//! Test support: no child process may hold a copy of a lock file that a test
//! relies on.
//!
//! An `flock` lock belongs to an open file description, which every copy of
//! its descriptor shares. A child process starts with a copy of every
//! descriptor the test process has open at that moment, close-on-exec ones
//! included, and keeps them until it execs or exits; a forked child that
//! never execs keeps them until it exits. While a child holds such a copy, a
//! lock its test has already closed stays held, so the test's next
//! `try_lock` of that file fails (`graph_materialize::prepare` then falls
//! back to an isolated temporary tree).
//!
//! The two guards below exclude each other:
//! - a test that creates a process holds [`forking`] from before the child
//!   exists until it is reaped (`fork` to `waitpid`);
//! - a test whose result depends on flock exclusivity holds
//!   [`flock_sensitive`] for its whole body, taken before it opens anything,
//!   so every file it opens is closed again before the guard is released.
//!
//! A child therefore never exists while such a test has a descriptor open,
//! and it cannot copy one opened after it was created. Tests that rely on
//! flock exclusivity still run in parallel with each other.
//!
//! Users: the library tests' only child processes, `embedding_cache::tests`'
//! probe (a fork that never execs) and the git commands of
//! `agent_files::tests::a_created_settings_file_is_kept_out_of_git_with_the_block`,
//! and every `graph_materialize` test, each of which calls `prepare` or
//! `purge` with a cache folder. The integration tests have their own copy,
//! `tests/it/fork_guard.rs`.
use std::sync::{PoisonError, RwLock, RwLockReadGuard};

static PROCESSES: RwLock<()> = RwLock::new(());

/// Held for the whole body of a test whose result depends on flock
/// exclusivity: no child process exists while it is held.
pub(crate) fn flock_sensitive() -> RwLockReadGuard<'static, ()> {
    PROCESSES.read().unwrap_or_else(PoisonError::into_inner)
}

/// Held from before a child process is created until it is reaped: no test
/// that relies on flock exclusivity runs meanwhile.
#[cfg(unix)]
pub(crate) fn forking() -> std::sync::RwLockWriteGuard<'static, ()> {
    PROCESSES.write().unwrap_or_else(PoisonError::into_inner)
}
