//! A SQLite store's write-ahead log stays bounded while readers are always
//! in flight. SQLite's own checkpoint after each commit is PASSIVE: it
//! copies back only the pages no reader still needs, and the log restarts
//! from its beginning only when a checkpoint has copied everything and no
//! reader is using it. With a writer committing back to back and readers
//! always in flight (an indexing job and many MCP clients), that moment
//! rarely comes, so the log grows by everything written: the 8.3 GB log of
//! the 2026-11-03 outage. The store now checkpoints and truncates the log
//! once it passes a limit (`Connection::bound_wal`).

use graphicle_runtime::store::Store;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Rows of 128 KiB committed one per transaction while four readers keep
/// read transactions open back to back; the largest the log grew, in
/// bytes, with the store's log limit set to `limit` (or none). With `pin`,
/// a fifth reader also holds one read transaction open from before the
/// first commit to the end, so SQLite's own checkpoint can never restart
/// the log: without it, a moment when all four readers are between
/// transactions (more likely on a loaded machine) lets the log restart and
/// makes the unbounded growth this test compares against vary from run to
/// run.
fn largest_log(limit: Option<u64>, commits: usize, pin: bool) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.pool.set_wal_limit(limit);
    store
        .transaction(|db| {
            db.execute_batch("CREATE TABLE wal_probe(id INTEGER PRIMARY KEY, body BLOB)")?;
            Ok(())
        })
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4u64)
        .map(|i| {
            let store = store.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(i * 2));
                while !stop.load(Ordering::Relaxed) {
                    store
                        .read(|db| {
                            let _: i64 =
                                db.query_row("SELECT count(*) FROM wal_probe", [], |r| r.get(0))?;
                            std::thread::sleep(Duration::from_millis(5));
                            Ok(())
                        })
                        .unwrap();
                }
            })
        })
        .collect();
    let pinned = pin.then(|| {
        let store = store.clone();
        let stop = stop.clone();
        let (started, snapshot) = std::sync::mpsc::channel();
        let pinned = std::thread::spawn(move || {
            store
                .read(|db| {
                    let _: i64 =
                        db.query_row("SELECT count(*) FROM wal_probe", [], |r| r.get(0))?;
                    started.send(()).unwrap();
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(())
                })
                .unwrap();
        });
        snapshot.recv().unwrap();
        pinned
    });
    let log = dir.path().join("reviews.sqlite3-wal");
    let body = vec![7u8; 128 << 10];
    let mut largest = 0;
    for _ in 0..commits {
        store
            .transaction(|db| {
                db.execute(
                    "INSERT INTO wal_probe(body) VALUES(?)",
                    graphicle_runtime::params![body],
                )?;
                Ok(())
            })
            .unwrap();
        largest = largest.max(std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0));
    }
    stop.store(true, Ordering::Relaxed);
    for reader in readers.into_iter().chain(pinned) {
        reader.join().unwrap();
    }
    largest
}

#[test]
fn the_write_ahead_log_stays_bounded_under_constant_reads() {
    if graphicle_runtime::db::postgres_configured() {
        eprintln!("skipped: a SQLite store's log; GRAPHICLE_DATABASE_URL is set");
        return;
    }
    // 240 commits of 128 KiB: 30 MiB written.
    let commits = 240;
    let limit: u64 = 4 << 20;
    let unbounded = largest_log(None, commits, true);
    let bounded = largest_log(Some(limit), commits, false);
    eprintln!(
        "largest log: {unbounded} bytes without the limit, {bounded} with a {limit}-byte limit"
    );
    // Without the limit the log keeps most of what was written (a pinned
    // reader keeps SQLite's own checkpoint from restarting it).
    assert!(
        unbounded > 16 << 20,
        "the log did not grow without the limit: {unbounded} bytes"
    );
    // With it, the log never passes the limit by more than the commits
    // made while a checkpoint waited for readers.
    assert!(
        bounded <= limit + (6 << 20),
        "the log passed its limit: {bounded} bytes"
    );
}
