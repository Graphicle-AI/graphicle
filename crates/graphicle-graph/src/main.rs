// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py
//! Native command-line entry point and backwards-compatible JSON Lines API.
use std::io::{self, IsTerminal, Write};

// Keep the allocator choice in the executable: applications embedding the
// library retain control of their own process-wide allocator.
// Not on macOS: mimalloc 3.3.2 can crash there on Apple silicon (microsoft/mimalloc#1306).
#[cfg(not(target_os = "macos"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn ts_malloc(size: usize) -> *mut std::ffi::c_void {
    unsafe { libmimalloc_sys::mi_malloc(size) }
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn ts_calloc(count: usize, size: usize) -> *mut std::ffi::c_void {
    match count.checked_mul(size) {
        Some(total) => unsafe { libmimalloc_sys::mi_zalloc(total) },
        None => std::ptr::null_mut(),
    }
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn ts_realloc(ptr: *mut std::ffi::c_void, size: usize) -> *mut std::ffi::c_void {
    unsafe { libmimalloc_sys::mi_realloc(ptr, size) }
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn ts_free(ptr: *mut std::ffi::c_void) {
    unsafe { libmimalloc_sys::mi_free(ptr) }
}

/// tree-sitter's C runtime allocates a node per token through libc malloc by
/// default; with many parser threads the system allocator's locks cap the
/// speedup (1.5x more parse throughput at 12 threads with mimalloc). Parse
/// results are unchanged: only where the bytes live differs.
#[cfg(not(target_os = "macos"))]
fn use_mimalloc_for_tree_sitter() {
    // SAFETY: called once, first thing in `main`, before any tree-sitter
    // object exists; the four functions are one allocator family (mimalloc)
    // and return malloc-aligned memory (null only on exhaustion). The binding
    // also frees the strings the runtime returns with `ts_free` from here on.
    // This is the tree-sitter 0.25 signature: the executable is built from
    // this crate's Cargo.lock (`--locked`), which pins the 0.25.10 runtime.
    unsafe {
        tree_sitter::set_allocator(
            Some(ts_malloc),
            Some(ts_calloc),
            Some(ts_realloc),
            Some(ts_free),
        );
    }
}

fn main() -> io::Result<()> {
    // mimalloc's options for a run of short phases (`par::configure_allocator`).
    graphicle::par::configure_allocator();
    #[cfg(not(target_os = "macos"))]
    use_mimalloc_for_tree_sitter();
    struct FlushCaches;
    impl Drop for FlushCaches {
        fn drop(&mut self) {
            graphicle::cache_api::flush();
            graphicle::detect::flush();
        }
    }
    let _flush_caches = FlushCaches;
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) != Some("--jsonl")
        && (!args.is_empty() || io::stdin().is_terminal())
    {
        let result = graphicle::cli::run(&args);
        let write = io::stdout().lock().write_all(result.stdout.as_bytes());
        if matches!(&write, Err(e) if e.kind() == io::ErrorKind::BrokenPipe) {
            return Ok(());
        }
        write?;
        let _ = io::stderr().lock().write_all(result.stderr.as_bytes());
        graphicle::cache_api::flush();
        graphicle::detect::flush();
        if result.exit_code != 0 {
            std::process::exit(result.exit_code);
        }
        return Ok(());
    }
    // Graph requests run to megabytes; a large read buffer keeps one request
    // from costing hundreds of small stdin reads.
    graphicle::protocol::jsonl(
        &mut io::BufReader::with_capacity(1 << 20, io::stdin().lock()),
        &mut io::BufWriter::with_capacity(1 << 20, io::stdout().lock()),
        |bytes| graphicle::protocol::encode(graphicle::dispatch_slice(bytes)),
    )
}
