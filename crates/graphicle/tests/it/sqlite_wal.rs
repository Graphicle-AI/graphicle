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
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long each snapshot of the relay reader lasts: well under the 2
/// seconds a checkpoint waits for readers.
const SNAPSHOT: Duration = Duration::from_millis(50);

/// One read snapshot of the store at `path`, held until `COMMIT`.
fn snapshot(path: &Path) -> rusqlite::Connection {
    let reader = rusqlite::Connection::open(path).unwrap();
    reader.busy_timeout(Duration::from_secs(10)).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM wal_probe", [], |r| r.get(0))
        .unwrap();
    reader
}

/// Rows of 128 KiB committed one per transaction while readers are always
/// in flight; the largest the log grew, in bytes, with the store's log
/// limit set to `limit` (or none). The load is the same with and without
/// the limit: four readers keep short read transactions open back to back,
/// and a relay reader always holds a snapshot, each for [`SNAPSHOT`],
/// opening the next before it ends the last. There is never a moment
/// without a reader on an older snapshot, so SQLite's own checkpoint can
/// never restart the log, on any machine: without the limit the log grows
/// by everything written. Every snapshot ends within the 2 seconds a
/// checkpoint waits, so with the limit the store's checkpoint can always
/// cut the log back.
fn largest_log(limit: Option<u64>, commits: usize) -> u64 {
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
    let relay = {
        let path = dir.path().join("reviews.sqlite3");
        let stop = stop.clone();
        let (started, snapshot_held) = std::sync::mpsc::channel();
        let relay = std::thread::spawn(move || {
            let mut held = snapshot(&path);
            started.send(()).unwrap();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(SNAPSHOT);
                let next = snapshot(&path);
                held.execute_batch("COMMIT").unwrap();
                held = next;
            }
            held.execute_batch("COMMIT").unwrap();
        });
        snapshot_held.recv().unwrap();
        relay
    };
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
    for reader in readers.into_iter().chain([relay]) {
        reader.join().unwrap();
    }
    largest
}

fn checkpoints() -> u64 {
    graphicle_runtime::db::wal_report()["wal_checkpoints"]
        .as_u64()
        .unwrap_or(0)
}

fn busy_checkpoints() -> u64 {
    graphicle_runtime::db::wal_report()["wal_checkpoints_busy"]
        .as_u64()
        .unwrap_or(0)
}

#[test]
fn the_write_ahead_log_stays_bounded_under_constant_reads() {
    if graphicle_runtime::db::postgres_configured() {
        eprintln!("skipped: a SQLite store's log; GRAPHICLE_DATABASE_URL is set");
        return;
    }
    // 200 commits of 128 KiB: 25 MiB written, under the same reads.
    let commits = 200;
    let limit: u64 = 4 << 20;
    let unbounded = largest_log(None, commits);
    let before = checkpoints();
    let busy_before = busy_checkpoints();
    let bounded = largest_log(Some(limit), commits);
    let ran = checkpoints() - before;
    let busy = busy_checkpoints() - busy_before;
    eprintln!(
        "largest log: {unbounded} bytes without the limit, {bounded} with a {limit}-byte limit ({ran} checkpoints, {busy} of them found readers)"
    );
    // Without the limit the log keeps what was written: SQLite's own
    // checkpoint never restarts it while the relay holds a snapshot.
    assert!(
        unbounded > 16 << 20,
        "the log did not grow without the limit: {unbounded} bytes"
    );
    // With it, under the same load, the store's checkpoint cut it back
    // (no other test's store passes the default 64 MiB limit), and the log
    // never passed the limit by more than the commits made while a
    // checkpoint waited for readers.
    assert!(ran > 0, "no checkpoint ran");
    assert!(
        bounded <= limit + (6 << 20),
        "the log passed its limit: {bounded} bytes ({ran} checkpoints, {busy} of them found readers)"
    );
}
