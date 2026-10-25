// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
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

// The executable's allocator is mimalloc (main.rs). libmimalloc-sys declares
// `mi_collect` only with its `extended` feature, but the C library it links
// always defines it; the `use` keeps that library linked into builds of this
// crate alone (its tests).
use libmimalloc_sys as _;
unsafe extern "C" {
    /// Free the calling thread's empty mimalloc pages and, with `force`,
    /// return every page already handed back to mimalloc's arenas (by any
    /// thread) to the operating system now instead of after the purge delay
    /// (mimalloc v3, the version the lock file builds). Safe at any time on
    /// any thread: it only touches memory no live allocation uses, and it
    /// returns at once on a thread that never allocated through mimalloc (so
    /// under an embedder's own global allocator it does nothing).
    safe fn mi_collect(force: bool);
}

/// Return the memory that freed allocations left behind to the operating
/// system now. mimalloc keeps freed pages for its purge delay before it
/// returns them, so that they can be reused: [`PURGE_DELAY_MS`] while a run is
/// frugal (see [`configure_allocator`]), a second after that. `extract` calls
/// this once, between the build and clustering: by then the parse workers'
/// scratch memory, assembly's tables and the extraction records the build
/// consumed are all freed, and clustering and the export would otherwise
/// allocate on top of them (tokio with the one-second delay: 251 to 203 MiB at
/// the peak). Where a stage takes longer than the delay (Kubernetes) the
/// pages are gone or reused by the time the next peak comes, so a call costs
/// time (up to 0.3 s there) without lowering any peak; calls at the other
/// stage boundaries were measured and dropped for that reason. Output never
/// depends on it.
pub fn trim() {
    let _profiled = crate::profile::span("alloc.trim");
    mi_collect(true);
}

// mimalloc's option number for the purge delay, from `mi_option_e`
// (include/mimalloc.h) of the mimalloc that Cargo.lock builds, 3.3.2
// (libmimalloc-sys 0.1.49 without its `v2` feature). libmimalloc-sys declares
// the option functions only with its `extended` feature; the C library always
// defines them. `allocator_options_are_mimallocs` checks the number against
// the library's default.
const MI_OPTION_PURGE_DELAY: std::ffi::c_int = 15;

unsafe extern "C" {
    /// A mimalloc option's value: its MIMALLOC_<NAME> environment variable,
    /// read once, else its default.
    safe fn mi_option_get(option: std::ffi::c_int) -> std::ffi::c_long;
    /// Change a mimalloc option's value unless its MIMALLOC_<NAME>
    /// environment variable set it. mimalloc reads the purge delay each time
    /// it frees pages, so a change applies from then on.
    safe fn mi_option_set_default(option: std::ffi::c_int, value: std::ffi::c_long);
}

/// The purge delay of a frugal run, in milliseconds: how long memory handed
/// back to mimalloc's arenas stays resident before it returns to the
/// operating system. mimalloc's own default is 1,000 ms. A small repository's
/// run is a few phases of tens or hundreds of milliseconds, each freeing what
/// the one before built, so with the default a phase allocates on top of the
/// previous phases' pages and the peak counts several phases at once.
pub const PURGE_DELAY_MS: std::ffi::c_long = 10;

/// The peak resident set at which a run stops being frugal (1 GiB).
pub const FRUGAL_UNTIL_BYTES: u64 = 1 << 30;

/// Make the executable's allocator frugal until the process has grown to
/// [`FRUGAL_UNTIL_BYTES`]; main.rs calls this first, before any work. Output
/// never depends on it.
///
/// Frugal means two things:
/// - mimalloc's purge delay is [`PURGE_DELAY_MS`] instead of its 1,000 ms;
/// - on Linux, no transparent huge pages. mimalloc asks for them on its arenas
///   (`MADV_HUGEPAGE`), so each 2 MiB stretch a thread touches becomes resident
///   whole: on a small repository most of the resident set was such pages.
///
/// Measured on wave C with both settings for the whole run (8 workers, peak
/// RSS in MiB, extract / priming update / unchanged update): flask 115 / 136 /
/// 109 to 60 / 70 / 60, ripgrep 140 / 219 / 183 to 75 / 110 / 94, tokio 214 /
/// 320 / 317 to 137 / 260 / 220, prometheus 451 / 634 / 630 to 367 / 546 /
/// 551, for 3-22% more wall time. Fewer worker threads saved at most a few
/// MiB more there, at 10-80% more time, so the pool keeps its size.
///
/// Both cost time where they save little memory: a large repository reuses
/// what it frees, and its peak is live data that huge pages map with fewer
/// faults (Kubernetes with both for the whole run: 3,404 to 3,309 MiB, 20%
/// slower). So a thread waits for the process's peak resident set to reach
/// [`FRUGAL_UNTIL_BYTES`], then restores mimalloc's purge delay and allows huge
/// pages again (for what is faulted in from then on), and ends. Kubernetes
/// passes the limit while it parses.
///
/// A setting the environment makes (MIMALLOC_PURGE_DELAY, MIMALLOC_ALLOW_THP)
/// is left alone, so a run can still be compared or tuned without a rebuild.
/// Other systems than Linux keep mimalloc's defaults.
pub fn configure_allocator() {
    let frugal = Frugal::from_env();
    if !(frugal.purge || frugal.huge_pages) {
        return;
    }
    frugal.set(true);
    let _ = std::thread::Builder::new()
        .name("graphicle-frugal".into())
        .spawn(move || {
            // Quick checks first (a large repository passes the limit within
            // seconds), then backing off to one a second.
            let mut wait = std::time::Duration::from_millis(10);
            while crate::profile::peak_resident_bytes() < FRUGAL_UNTIL_BYTES {
                std::thread::sleep(wait);
                wait = (wait * 5 / 4).min(std::time::Duration::from_secs(1));
            }
            frugal.set(false);
        });
}

/// Which allocator settings a frugal run changes: those the environment does
/// not set.
#[derive(Clone, Copy)]
struct Frugal {
    purge: bool,
    huge_pages: bool,
    default_delay: std::ffi::c_long,
}

impl Frugal {
    /// Linux only: that is where it was measured, and where the peak
    /// resident set is read as a run goes.
    fn from_env() -> Self {
        let linux = cfg!(target_os = "linux");
        let unset = |var: &str| std::env::var_os(var).is_none();
        Self {
            purge: linux && unset("MIMALLOC_PURGE_DELAY"),
            huge_pages: linux && unset("MIMALLOC_ALLOW_THP"),
            default_delay: mi_option_get(MI_OPTION_PURGE_DELAY),
        }
    }

    fn set(self, frugal: bool) {
        if self.purge {
            let delay = if frugal {
                PURGE_DELAY_MS
            } else {
                self.default_delay
            };
            mi_option_set_default(MI_OPTION_PURGE_DELAY, delay);
        }
        if self.huge_pages {
            disable_huge_pages(frugal);
        }
    }
}

/// Turn this process's "no transparent huge pages" flag on or off. It covers
/// every mapping, mimalloc's arenas included, and child processes inherit it.
#[cfg(target_os = "linux")]
fn disable_huge_pages(disable: bool) {
    let (flag, unused): (nix::libc::c_ulong, nix::libc::c_ulong) = (disable.into(), 0);
    // SAFETY: PR_SET_THP_DISABLE only sets a flag of this process; it reads
    // and writes no memory of ours.
    unsafe { nix::libc::prctl(nix::libc::PR_SET_THP_DISABLE, flag, unused, unused, unused) };
}

#[cfg(not(target_os = "linux"))]
fn disable_huge_pages(_: bool) {}

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

    #[test]
    fn trim_runs_on_any_thread() {
        let freed = (0..10_000).map(|n| vec![n as u8; 64]).collect::<Vec<_>>();
        drop(freed);
        trim();
        std::thread::spawn(trim).join().unwrap();
        drop_later(vec![0u8; 1 << 20]);
    }

    #[test]
    fn allocator_options_are_mimallocs() {
        // mimalloc 3's default purge delay, 1,000 ms, identifies the option
        // number. Only this test changes allocator settings.
        if std::env::var_os("MIMALLOC_PURGE_DELAY").is_none() {
            assert_eq!(mi_option_get(MI_OPTION_PURGE_DELAY), 1000);
        }
        let frugal = Frugal::from_env();
        let delay = mi_option_get(MI_OPTION_PURGE_DELAY);
        #[cfg(target_os = "linux")]
        let huge_pages_off = || {
            let unused: nix::libc::c_ulong = 0;
            // SAFETY: PR_GET_THP_DISABLE only reads a flag of this process.
            unsafe {
                nix::libc::prctl(
                    nix::libc::PR_GET_THP_DISABLE,
                    unused,
                    unused,
                    unused,
                    unused,
                )
            }
        };
        for on in [true, false] {
            frugal.set(on);
            let expected = if frugal.purge && on {
                PURGE_DELAY_MS
            } else {
                delay
            };
            assert_eq!(mi_option_get(MI_OPTION_PURGE_DELAY), expected);
            #[cfg(target_os = "linux")]
            if frugal.huge_pages {
                assert_eq!(huge_pages_off(), i32::from(on));
            }
        }
    }
}
