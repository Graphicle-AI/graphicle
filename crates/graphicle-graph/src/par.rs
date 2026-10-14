// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/rcfile.py
//! Order-preserving parallel map for per-file work (hashing, classification,
//! cache reads). Results come back in input order, so callers that apply side
//! effects afterwards (index updates, counters, lists) do so exactly as a
//! sequential loop would.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Worker threads for per-file work: `requested`, else `GRAPHICLE_MAX_WORKERS`,
/// else the available parallelism; at least 1.
pub fn workers(requested: Option<usize>) -> usize {
    requested
        .or_else(|| {
            std::env::var("GRAPHICLE_MAX_WORKERS")
                .ok()
                .and_then(|s| s.trim().parse::<usize>().ok())
                .filter(|n| *n > 0)
        })
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
        })
        .max(1)
}

/// Worker threads for work dominated by opening and reading files rather
/// than computing on them (hashing every file for the manifest). On macOS,
/// opens from more than about six threads contend in the kernel: measured on
/// an M3 Pro, reading and hashing 11k files took 0.44 s on 4-6 threads and
/// 0.67 s on 12-16. Elsewhere this is `workers`. `GRAPHICLE_MAX_WORKERS` still
/// caps it; results never depend on it.
pub fn io_workers() -> usize {
    let workers = workers(None);
    if cfg!(target_os = "macos") {
        workers.min(6)
    } else {
        workers
    }
}

/// Apply `f` to every item on up to `workers` threads and return the results
/// in item order. Runs inline for one worker or a handful of items. Items are
/// claimed in small batches from a shared counter, so uneven items (a large
/// file among small ones) do not leave threads idle; each thread keeps its
/// results locally, so no lock is taken per item.
pub fn map<T: Sync, R: Send>(items: &[T], workers: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    map_with(items, workers, || (), |_, item| f(item))
}

/// [`map`] with scratch state per thread (`init` runs once on each), for memo
/// tables that only speed up a computation whose result does not depend on
/// them.
pub fn map_with<T: Sync, S, R: Send>(
    items: &[T],
    workers: usize,
    init: impl Fn() -> S + Sync,
    f: impl Fn(&mut S, &T) -> R + Sync,
) -> Vec<R> {
    let workers = workers.clamp(1, items.len().max(1));
    if workers == 1 || items.len() < 16 {
        let mut state = init();
        return items.iter().map(|item| f(&mut state, item)).collect();
    }
    let batch = (items.len() / (workers * 8)).clamp(1, 64);
    let next = AtomicUsize::new(0);
    // Workers see the caller's `.graphiclerc` language overrides, as the
    // reference's pool initializer hands its workers the parent's.
    let overrides = crate::rcfile::snapshot();
    let parts = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| {
                let (next, init, f, overrides) = (&next, &init, &f, &overrides);
                scope.spawn(move || {
                    overrides.install();
                    let mut state = init();
                    let mut out = Vec::new();
                    loop {
                        let start = next.fetch_add(batch, Ordering::Relaxed);
                        if start >= items.len() {
                            break;
                        }
                        let end = (start + batch).min(items.len());
                        for (offset, item) in items[start..end].iter().enumerate() {
                            out.push((start + offset, f(&mut state, item)));
                        }
                    }
                    out
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect::<Vec<_>>()
    });
    let mut slots = (0..items.len()).map(|_| None).collect::<Vec<Option<R>>>();
    for (i, r) in parts.into_iter().flatten() {
        slots[i] = Some(r);
    }
    slots
        .into_iter()
        .map(|r| r.expect("every item is mapped once"))
        .collect()
}

/// Free `value` on a background thread: releasing a corpus worth of small
/// allocations takes tens of milliseconds that nothing needs to wait for.
/// (If the thread cannot be started, the value is freed here.)
pub fn drop_later<T: Send + 'static>(value: T) {
    let _ = std::thread::Builder::new()
        .name("graphicle-release".into())
        .spawn(move || drop(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_input_order_for_any_worker_count() {
        let items = (0..1000).collect::<Vec<u64>>();
        for workers in [1, 2, 7, 64] {
            let out = map(&items, workers, |n| n * n);
            assert_eq!(out, items.iter().map(|n| n * n).collect::<Vec<_>>());
        }
        assert!(map(&Vec::<u8>::new(), 8, |b| *b).is_empty());
    }
}
