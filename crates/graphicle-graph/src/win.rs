// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/watch.py, graphify/install.py
//! Windows-only system calls: the rebuild lock's byte-range lock (Graphify's
//! `msvcrt.locking`), ANSI colour in classic consoles and the Ctrl+C handler,
//! declared from kernel32 directly so no extra crate is needed.
use std::{fs::File, io, os::windows::io::AsRawHandle};

type Handle = *mut std::ffi::c_void;
type Bool = i32;
type CtrlHandler = unsafe extern "system" fn(u32) -> Bool;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LockFile(
        file: Handle,
        offset_low: u32,
        offset_high: u32,
        len_low: u32,
        len_high: u32,
    ) -> Bool;
    fn UnlockFile(
        file: Handle,
        offset_low: u32,
        offset_high: u32,
        len_low: u32,
        len_high: u32,
    ) -> Bool;
    fn GetStdHandle(which: u32) -> Handle;
    fn SetHandleInformation(object: Handle, mask: u32, flags: u32) -> Bool;
    fn SetConsoleMode(console: Handle, mode: u32) -> Bool;
    fn SetConsoleCtrlHandler(handler: Option<CtrlHandler>, add: Bool) -> Bool;
}

/// `ERROR_LOCK_VIOLATION`: another handle holds the byte range.
const ERROR_LOCK_VIOLATION: i32 = 33;
/// `STD_INPUT_HANDLE`, that is `(DWORD)-10`.
const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
/// `STD_OUTPUT_HANDLE`, that is `(DWORD)-11`.
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
/// `STD_ERROR_HANDLE`, that is `(DWORD)-12`.
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
/// `HANDLE_FLAG_INHERIT`.
const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

/// Lock `len` bytes at `offset` without waiting, as `msvcrt.locking(fd,
/// LK_NBLCK, len)` does after a seek. `Ok(false)` when another handle holds
/// the range.
pub fn try_lock_range(file: &File, offset: u64, len: u32) -> io::Result<bool> {
    let (low, high) = (offset as u32, (offset >> 32) as u32);
    // SAFETY: the handle is open for the lifetime of `file`.
    if unsafe { LockFile(file.as_raw_handle(), low, high, len, 0) } != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// Release a range locked by [`try_lock_range`]; closing the handle releases
/// it too, so a failure is ignored like Graphify's `LK_UNLCK` one.
pub fn unlock_range(file: &File, offset: u64, len: u32) {
    let (low, high) = (offset as u32, (offset >> 32) as u32);
    // SAFETY: the handle is open for the lifetime of `file`.
    unsafe { UnlockFile(file.as_raw_handle(), low, high, len, 0) };
}

/// Graphify's `_print_banner`: `SetConsoleMode(GetStdHandle(-11), 7)`, that
/// is processed output, wrap at end of line and virtual-terminal processing,
/// so ANSI colour renders in classic consoles. False when standard output is
/// not a console or the console refuses the mode.
pub fn enable_virtual_terminal() -> bool {
    // SAFETY: both calls take plain values; an invalid handle makes
    // SetConsoleMode fail, which is reported as false.
    unsafe { SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), 7) != 0 }
}

/// Make this process's standard input, output and error handles
/// non-inheritable. Windows starts a child with every inheritable handle of
/// its parent, whatever the child's own standard handles are, so without
/// this a detached child keeps the pipes this process's caller reads (a Git
/// hook's output) open until it exits; Graphify's `Popen` passes only the
/// child's own handles. A child started later with inherited output still
/// gets it: the standard library hands it an inheritable copy. A missing
/// handle, or one that refuses, is left as it is.
pub fn keep_standard_handles() {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain calls on this process's own standard handles; the
        // flag change does not close or replace them.
        unsafe {
            let handle = GetStdHandle(which);
            if !handle.is_null() && handle as isize != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Add `handler` for console control events (Ctrl+C, Ctrl+Break, close).
pub fn add_ctrl_handler(handler: extern "system" fn(u32) -> Bool) -> bool {
    let handler: CtrlHandler = handler;
    // SAFETY: `handler` is a plain function that lives for the whole program.
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_lock_blocks_only_its_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let first = File::create(&path).unwrap();
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(try_lock_range(&first, 4096, 1).unwrap());
        assert!(!try_lock_range(&second, 4096, 1).unwrap());
        assert!(try_lock_range(&second, 0, 1).unwrap());
        unlock_range(&second, 0, 1);
        unlock_range(&first, 4096, 1);
        assert!(try_lock_range(&second, 4096, 1).unwrap());
        unlock_range(&second, 4096, 1);
    }

    #[test]
    fn console_calls_link_and_answer() {
        // Under the test harness standard output may or may not be a console;
        // the call must answer the same way twice and never fail hard.
        assert_eq!(enable_virtual_terminal(), enable_virtual_terminal());
        extern "system" fn ignore(_: u32) -> Bool {
            0
        }
        let _ = add_ctrl_handler(ignore);
    }
}
