//! Request counts this process holds while a SQLite store is busy.
//!
//! Every authenticated HTTP request is counted against its workspace's
//! monthly budget (`product::reserve_request`) and its outcome recorded
//! after it is served (`product::finish_request`). On PostgreSQL those are
//! row-lock transactions ([`Store::counter`]). On SQLite every write needs
//! the store's one write lock, which a long writer, such as an indexing job
//! in another process, can hold for longer than the store's busy timeout,
//! and the request then failed with "database is locked" although it only
//! reads.
//!
//! On SQLite a counter write ([`write`]) now waits at most [`COUNTER_WAIT`]
//! for the lock. When the store stays busy, the request is checked against
//! its budget with a read (the stored count plus the counts held here) and
//! served, and its count is held here ([`Held`]). The next counter write of
//! this process that gets the lock writes every held count first, in its
//! own transaction; [`flush_all`] writes them when the program ends. A
//! process that is killed loses its held counts.
//!
//! **Bound.** Within one process a budget is never exceeded: a counter
//! write holds this process's tally lock from its first write to its
//! commit, and a held check adds to the stored count both the held counts
//! and every request this process wrote since its read began
//! ([`Held::written`]; one written while the read ran may be counted
//! twice, which only refuses early). It never waits for a writer. Several
//! processes serving one data folder at once do not see each other's held
//! counts or uncommitted counter writes: a budget can then be exceeded by
//! at most the requests the other processes hold or are counting at that
//! moment.
use crate::db::{Connection, TransactionBehavior, params};
use crate::{Error, Result, store::Store};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

/// How long a counter write waits for SQLite's write lock before the
/// request is counted here instead.
pub const COUNTER_WAIT: Duration = Duration::from_millis(50);

/// Counts of one workspace and UTC day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub requests: u64,
    pub errors: u64,
    pub elapsed_ms: u64,
}

/// What this process holds for one store.
#[derive(Default)]
pub struct Held {
    /// Unwritten counts by (workspace, UTC day `YYYY-MM-DD`).
    pub counts: BTreeMap<(String, String), Tally>,
    /// Requests of each workspace this process has written to the store
    /// (held counts it wrote and requests a counter write counted).
    pub written: BTreeMap<String, u64>,
    store: Option<Store>,
}

impl Held {
    /// The requests held for `workspace` on days from `start` on.
    pub fn requests_since(&self, workspace: &str, start: &str) -> u64 {
        self.counts
            .iter()
            .filter(|((w, day), _)| w == workspace && day.as_str() >= start)
            .map(|(_, tally)| tally.requests)
            .fold(0, u64::saturating_add)
    }
    /// The requests of `workspace` this process has written to the store.
    pub fn written(&self, workspace: &str) -> u64 {
        self.written.get(workspace).copied().unwrap_or(0)
    }
    /// Add `tally` to the counts of `workspace` on `day`.
    pub fn add(&mut self, workspace: &str, day: &str, tally: Tally) {
        let held = self
            .counts
            .entry((workspace.to_owned(), day.to_owned()))
            .or_default();
        held.requests = held.requests.saturating_add(tally.requests);
        held.errors = held.errors.saturating_add(tally.errors);
        held.elapsed_ms = held.elapsed_ms.saturating_add(tally.elapsed_ms);
    }
}

type Registry = BTreeMap<PathBuf, Arc<Mutex<Held>>>;

static REGISTRY: Mutex<Registry> = Mutex::new(BTreeMap::new());

/// This process's tally lock and held counts for `store`.
pub fn held(store: &Store) -> Arc<Mutex<Held>> {
    let mut registry = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let entry = registry
        .entry(store.database.as_ref().clone())
        .or_insert_with(|| {
            Arc::new(Mutex::new(Held {
                store: Some(store.clone()),
                ..Held::default()
            }))
        });
    entry.clone()
}

/// Lock `held`, whatever a panicking holder left.
pub fn lock(held: &Mutex<Held>) -> MutexGuard<'_, Held> {
    held.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The requests this process holds for `workspace` from `start` on.
pub fn requests_since(store: &Store, workspace: &str, start: &str) -> u64 {
    let held = REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(store.database.as_ref())
        .cloned();
    held.map_or(0, |held| lock(&held).requests_since(workspace, start))
}

/// SQLite: a counter write waiting at most `wait` for the write lock.
/// `Ok(None)` when the store stayed busy; nothing was written then.
/// Otherwise every count this process holds for the store is written
/// first, then `f` runs, in one transaction, under the tally lock; the
/// held counts are dropped once it commits. `f` names the workspace whose
/// request it counted, if any.
pub fn write<T>(
    store: &Store,
    wait: Duration,
    f: impl FnOnce(&Connection) -> Result<(T, Option<String>)>,
) -> Result<Option<T>> {
    let mut db = store.connect()?;
    db.busy_timeout(wait)?;
    let transaction = match db.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(transaction) => transaction,
        Err(error) => {
            let error = Error::from(error);
            if crate::jobs::busy(&error) {
                return Ok(None);
            }
            return Err(error);
        }
    };
    transaction.busy_timeout(crate::db::DEFAULT_BUSY)?;
    let held = held(store);
    let mut held = lock(&held);
    write_held(&transaction, &held.counts)?;
    let (value, counted) = f(&transaction)?;
    transaction.commit()?;
    let counts = std::mem::take(&mut held.counts);
    let written = counts
        .into_iter()
        .map(|((workspace, _), tally)| (workspace, tally.requests))
        .chain(counted.map(|workspace| (workspace, 1)));
    for (workspace, requests) in written.collect::<Vec<_>>() {
        let total = held.written.entry(workspace).or_default();
        *total = total.saturating_add(requests);
    }
    drop(held);
    let _ = db.bound_wal();
    Ok(Some(value))
}

/// Add held counts to the stored counters; counts of a workspace deleted
/// since are dropped.
fn write_held(db: &Connection, counts: &BTreeMap<(String, String), Tally>) -> Result<()> {
    if counts.is_empty() {
        return Ok(());
    }
    let mut insert = db.prepare(
        "INSERT INTO am_usage(workspace_id,day,requests,errors,elapsed_ms)
         SELECT ?1,?2,?3,?4,?5 WHERE EXISTS(SELECT 1 FROM workspaces WHERE id=?1)
         ON CONFLICT(workspace_id,day) DO UPDATE SET requests=am_usage.requests+excluded.requests,
         errors=am_usage.errors+excluded.errors,elapsed_ms=am_usage.elapsed_ms+excluded.elapsed_ms",
    )?;
    for ((workspace, day), tally) in counts {
        let clamp = |n: u64| n.min(i64::MAX as u64) as i64;
        insert.execute(params![
            workspace,
            day,
            clamp(tally.requests),
            clamp(tally.errors),
            clamp(tally.elapsed_ms)
        ])?;
    }
    Ok(())
}

/// When the program ends: write every store's held counts, waiting at most
/// `wait` for each store's write lock. What stays unwritten is reported on
/// standard error.
pub fn flush_all(wait: Duration) {
    let stores: Vec<Store> = REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .filter_map(|held| {
            let held = lock(held);
            (!held.counts.is_empty())
                .then(|| held.store.clone())
                .flatten()
        })
        .collect();
    for store in stores {
        let left = |store: &Store| {
            lock(&held(store))
                .counts
                .values()
                .map(|t| t.requests)
                .fold(0, u64::saturating_add)
        };
        match write(&store, wait, |_| Ok(((), None))) {
            Ok(Some(())) => {}
            Ok(None) => eprintln!(
                "graphicle: {} request counts were not recorded: the store stayed busy",
                left(&store)
            ),
            Err(error) => eprintln!(
                "graphicle: {} request counts were not recorded: {}",
                left(&store),
                error.message
            ),
        }
    }
}
