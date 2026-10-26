//! Opt-in phase instrumentation: `GRAPHICLE_PHASE_TIMINGS=1` logs the
//! wall-clock and resident memory of the projection phases to stderr. Off by
//! default; standard output is never touched.
use std::{sync::OnceLock, time::Instant};

/// Whether `GRAPHICLE_PHASE_TIMINGS` asks for diagnostics on stderr.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GRAPHICLE_PHASE_TIMINGS").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// Peak resident set size of this process in bytes (0 when unavailable, as
/// on Windows, where the engine's profile also reports 0).
#[cfg(not(unix))]
pub fn peak_rss() -> u64 {
    0
}

/// Peak resident set size of this process in bytes (0 when unavailable).
#[cfg(unix)]
pub fn peak_rss() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return 0;
    }
    let raw = usage.ru_maxrss.max(0) as u64;
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    }
}

/// Current resident set size in bytes (0 when unavailable).
pub fn current_rss() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let pages: u64 = statm
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        return pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64;
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::mach_task_basic_info = unsafe { std::mem::zeroed() };
        let mut count = (std::mem::size_of::<libc::mach_task_basic_info>()
            / std::mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        #[allow(deprecated)]
        let status = unsafe {
            libc::task_info(
                libc::mach_task_self(),
                libc::MACH_TASK_BASIC_INFO,
                (&mut info as *mut libc::mach_task_basic_info).cast(),
                &mut count,
            )
        };
        if status == libc::KERN_SUCCESS {
            return info.resident_size;
        }
        return 0;
    }
    #[allow(unreachable_code)]
    0
}

/// Run `f` as a named phase, logging its duration and memory when enabled.
pub fn phase<T>(name: &str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let started = Instant::now();
    let result = f();
    eprintln!(
        "phase={name} ms={} rss_mb={:.1} peak_rss_mb={:.1}",
        started.elapsed().as_millis(),
        current_rss() as f64 / (1024.0 * 1024.0),
        peak_rss() as f64 / (1024.0 * 1024.0),
    );
    result
}

/// Log `name` as a marker with `detail` (what a stretch of code found, such
/// as how much of a file it read) and no duration, when enabled.
pub fn note(name: &str, detail: &str) {
    if enabled() {
        eprintln!("phase={name} ms=0 {detail}");
    }
}

/// Log `name` as a phase that started at `started` (for a stretch of code
/// that is not one closure), when enabled.
pub fn since(name: &str, started: Instant) {
    if enabled() {
        eprintln!(
            "phase={name} ms={} rss_mb={:.1} peak_rss_mb={:.1}",
            started.elapsed().as_millis(),
            current_rss() as f64 / (1024.0 * 1024.0),
            peak_rss() as f64 / (1024.0 * 1024.0),
        );
    }
}
