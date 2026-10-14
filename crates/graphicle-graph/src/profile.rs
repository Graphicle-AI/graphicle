//! Opt-in phase profile (`GRAPHICLE_PROFILE=1`): wall time, CPU time of all
//! threads, the cores that implies, and resident memory for named spans, on
//! stderr as they end. Nothing else changes; disabled, a span is one cached
//! flag check.
//!
//! `GRAPHICLE_PROFILE=1` also totals the per-file stage by file suffix
//! (files, bytes, worker time), printed when the extraction run ends.

use std::{
    collections::BTreeMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var("GRAPHICLE_PROFILE").is_ok_and(|v| v == "1"));

pub fn enabled() -> bool {
    *ENABLED
}

#[cfg(unix)]
fn rusage() -> Option<nix::libc::rusage> {
    let mut usage = std::mem::MaybeUninit::<nix::libc::rusage>::zeroed();
    // SAFETY: getrusage fills the buffer when it returns 0.
    (unsafe { nix::libc::getrusage(nix::libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0)
        .then(|| unsafe { usage.assume_init() })
}

/// CPU seconds (user + system) used so far by every thread of the process.
fn cpu_seconds() -> f64 {
    #[cfg(unix)]
    {
        rusage().map_or(0.0, |u| {
            let t = |tv: nix::libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
            t(u.ru_utime) + t(u.ru_stime)
        })
    }
    #[cfg(not(unix))]
    {
        0.0
    }
}

/// Resident set now and at its peak, in MiB (0 where unavailable).
fn resident_mib() -> (f64, f64) {
    #[cfg(unix)]
    let peak = rusage().map_or(0.0, |u| {
        // ru_maxrss is bytes on macOS and KiB elsewhere.
        let raw = u.ru_maxrss as f64;
        if cfg!(target_os = "macos") {
            raw / 1048576.0
        } else {
            raw / 1024.0
        }
    });
    #[cfg(not(unix))]
    let peak = 0.0;
    #[cfg(target_os = "linux")]
    let now = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map_or(0.0, |pages| pages * 4096.0 / 1048576.0);
    #[cfg(target_os = "macos")]
    let now = {
        let mut info = std::mem::MaybeUninit::<nix::libc::rusage_info_v2>::zeroed();
        // SAFETY: the buffer is a rusage_info_v2, the flavor requested.
        let rc = unsafe {
            nix::libc::proc_pid_rusage(
                std::process::id() as i32,
                nix::libc::RUSAGE_INFO_V2,
                info.as_mut_ptr().cast(),
            )
        };
        if rc == 0 {
            // SAFETY: proc_pid_rusage filled the buffer on success.
            unsafe { info.assume_init() }.ri_resident_size as f64 / 1048576.0
        } else {
            0.0
        }
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let now = 0.0;
    (now, peak)
}

/// A named span; reports when dropped. Spans nest by naming convention
/// (`extract.assemble.python_imports`).
pub struct Span {
    name: std::borrow::Cow<'static, str>,
    start: Instant,
    cpu: f64,
}

pub fn span(name: &'static str) -> Option<Span> {
    enabled().then(|| Span {
        name: name.into(),
        start: Instant::now(),
        cpu: cpu_seconds(),
    })
}

/// A span whose name is built only when profiling is on.
pub fn span_with(name: impl FnOnce() -> String) -> Option<Span> {
    enabled().then(|| Span {
        name: name().into(),
        start: Instant::now(),
        cpu: cpu_seconds(),
    })
}

/// CPU seconds so far, for callers that time their own intervals (0 when
/// profiling is off).
pub fn cpu_now() -> f64 {
    if enabled() { cpu_seconds() } else { 0.0 }
}

/// Report an interval the caller timed itself.
pub fn report(name: &str, wall: f64, cpu: f64) {
    if !enabled() {
        return;
    }
    let (now, peak) = resident_mib();
    eprintln!(
        "[graphicle profile] {name} wall={wall:.3}s cpu={cpu:.3}s cores={:.1} rss={now:.0}MiB peak={peak:.0}MiB",
        if wall > 0.0 { cpu / wall } else { 0.0 },
    );
}

impl Drop for Span {
    fn drop(&mut self) {
        report(
            &self.name,
            self.start.elapsed().as_secs_f64(),
            cpu_seconds() - self.cpu,
        );
    }
}

#[derive(Default, Clone, Copy)]
struct FileTotals {
    files: usize,
    bytes: u64,
    busy: Duration,
}

static PER_SUFFIX: LazyLock<Mutex<BTreeMap<String, FileTotals>>> = LazyLock::new(Mutex::default);

/// Account one file of the per-file stage: its suffix, size and the worker
/// time spent on it.
pub fn file_done(path: &std::path::Path, busy: Duration) {
    if !enabled() {
        return;
    }
    let suffix = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let bytes = std::fs::metadata(path).map_or(0, |m| m.len());
    let mut totals = PER_SUFFIX.lock().unwrap_or_else(|e| e.into_inner());
    let entry = totals.entry(suffix).or_default();
    entry.files += 1;
    entry.bytes += bytes;
    entry.busy += busy;
}

/// Print and reset the per-suffix totals, largest worker time first.
pub fn report_files(stage: &str) {
    if !enabled() {
        return;
    }
    let totals = std::mem::take(&mut *PER_SUFFIX.lock().unwrap_or_else(|e| e.into_inner()));
    let mut rows = totals.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1.busy));
    for (suffix, t) in rows {
        let secs = t.busy.as_secs_f64();
        eprintln!(
            "[graphicle profile] {stage} suffix={} files={} bytes={} worker={:.3}s ms_per_file={:.2} mb_per_s={:.1}",
            if suffix.is_empty() { "(none)" } else { &suffix },
            t.files,
            t.bytes,
            secs,
            secs * 1000.0 / t.files.max(1) as f64,
            if secs > 0.0 {
                t.bytes as f64 / 1e6 / secs
            } else {
                0.0
            }
        );
    }
}
