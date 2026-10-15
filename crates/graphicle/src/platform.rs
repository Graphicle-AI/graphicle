//! Platform helpers for the few places where Windows differs from macOS and
//! Linux. On macOS and Linux every function here does exactly what the code
//! it replaced did; the Windows branches sit behind `cfg(windows)`.
use std::path::{Path, PathBuf};
use std::process::Command;

/// `fs::canonicalize` without Windows' verbatim prefix (see [`simplified`]),
/// so the path can go to Git, into client entries, hooks and folder keys.
pub trait CanonicalExt {
    fn canonical(&self) -> std::io::Result<PathBuf>;
}

impl CanonicalExt for Path {
    fn canonical(&self) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(self).map(simplified)
    }
}

/// `path` unchanged on macOS and Linux. On Windows a verbatim path
/// (`\\?\C:\x`, `\\?\UNC\server\share\x`) becomes its plain form (`C:\x`,
/// `\\server\share\x`) when that names the same file, as the `dunce` crate
/// decides: every component a valid, non-reserved name and the whole at most
/// 260 characters; otherwise the verbatim path is kept.
#[cfg(not(windows))]
pub fn simplified(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
pub fn simplified(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(strip_verbatim) {
        Some(plain) => PathBuf::from(plain),
        None => path,
    }
}

/// [`simplified`]'s rule on a string: the plain form of a verbatim path, or
/// `None` when `path` is not verbatim or the plain form would differ.
#[cfg(any(windows, test))]
fn strip_verbatim(path: &str) -> Option<String> {
    let (plain, names) = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        let mut parts = rest.splitn(3, '\\');
        let (server, share) = (parts.next()?, parts.next()?);
        if !valid_name(server) || !valid_name(share) {
            return None;
        }
        (format!(r"\\{rest}"), parts.next().unwrap_or(""))
    } else {
        let rest = path.strip_prefix(r"\\?\")?;
        let bytes = rest.as_bytes();
        if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
            return None;
        }
        let tail = &rest[2..];
        if !tail.is_empty() && !tail.starts_with('\\') {
            return None;
        }
        (rest.to_string(), tail.strip_prefix('\\').unwrap_or(tail))
    };
    let mut components: Vec<&str> = names.split('\\').collect();
    if components.last() == Some(&"") {
        components.pop();
    }
    if plain.len() > 260
        || components
            .iter()
            .any(|name| !valid_name(name) || reserved_name(name))
    {
        return None;
    }
    Some(plain)
}

/// A name Windows accepts outside a verbatim path ("." and ".." are taken
/// literally in a verbatim path, so they are not).
#[cfg(any(windows, test))]
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.encode_utf16().count() <= 255
        && !name.bytes().any(|c| {
            matches!(
                c,
                0..=31 | b'<' | b'>' | b':' | b'"' | b'/' | b'\\' | b'|' | b'?' | b'*'
            )
        })
        && !name.ends_with([' ', '.'])
}

/// A DOS device name (`con`, `NUL.txt`, `com1 .log`).
#[cfg(any(windows, test))]
fn reserved_name(name: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "AUX", "NUL", "PRN", "CON", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '.']);
    stem.len() <= 4 && RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r))
}

/// The user's home folder: `HOME` on macOS and Linux, as before. On Windows
/// `std::env::home_dir()` (`USERPROFILE`, then the profile API), as the
/// engine reads it; `HOME`, which Git Bash sets, is not used.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::home_dir().filter(|p| p.is_absolute())
    }
}

/// `%LOCALAPPDATA%`, else `AppData\Local` in the home folder.
#[cfg(windows)]
pub fn local_app_data() -> Option<PathBuf> {
    app_data_folder("LOCALAPPDATA", r"AppData\Local")
}

/// `%APPDATA%`, else `AppData\Roaming` in the home folder.
#[cfg(windows)]
pub fn roaming_app_data() -> Option<PathBuf> {
    app_data_folder("APPDATA", r"AppData\Roaming")
}

#[cfg(windows)]
fn app_data_folder(variable: &str, under_home: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(under_home)))
}

/// Claude Desktop's folder on Windows: the Microsoft Store build's
/// redirected copy (`%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude`)
/// when one exists, since that build reads its own copy first; else
/// `%APPDATA%\Claude`.
#[cfg(windows)]
pub fn claude_desktop_dir(roaming: Option<&Path>, local: Option<&Path>) -> Option<PathBuf> {
    let store = local.and_then(|local| {
        let mut packages: Vec<PathBuf> = std::fs::read_dir(local.join("Packages"))
            .ok()?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .starts_with("claude_")
            })
            .map(|entry| entry.path().join(r"LocalCache\Roaming\Claude"))
            .filter(|folder| folder.is_dir())
            .collect();
        packages.sort();
        packages.into_iter().next()
    });
    store.or_else(|| roaming.map(|roaming| roaming.join("Claude")))
}

/// The file names a program called `name` may have: `name` itself on macOS
/// and Linux. On Windows `name` with each `PATHEXT` extension (`.COM`,
/// `.EXE`, `.BAT`, `.CMD` by default), or `name` alone when it already ends
/// in one of them, compared without case.
pub fn program_names(name: &str) -> Vec<String> {
    #[cfg(not(windows))]
    {
        vec![format!("{name}{}", std::env::consts::EXE_SUFFIX)]
    }
    #[cfg(windows)]
    {
        program_names_with(name, std::env::var("PATHEXT").ok().as_deref())
    }
}

#[cfg(any(windows, test))]
fn program_names_with(name: &str, pathext: Option<&str>) -> Vec<String> {
    let extensions: Vec<String> = pathext
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .map(|ext| ext.trim().to_ascii_lowercase())
        .filter(|ext| ext.starts_with('.') && ext.len() > 1)
        .collect();
    let lower = name.to_ascii_lowercase();
    if extensions.iter().any(|ext| lower.ends_with(ext.as_str())) {
        return vec![name.to_string()];
    }
    extensions
        .iter()
        .map(|ext| format!("{name}{ext}"))
        .collect()
}

/// `Command::new(name)` on macOS and Linux. On Windows the program is first
/// found on `PATH` with [`program_names`], so npm's `codex.cmd` shims run:
/// the standard library starts a `.cmd` or `.bat` file through
/// `cmd.exe /d /c` with its batch-file escaping, and refuses an argument it
/// cannot quote safely.
pub fn command(name: &str) -> Command {
    #[cfg(windows)]
    {
        if let Some(found) = find_program(name) {
            return Command::new(found);
        }
    }
    Command::new(name)
}

/// The first file named [`program_names`]`(name)` in `PATH`'s absolute
/// folders.
#[cfg(windows)]
pub fn find_program(name: &str) -> Option<PathBuf> {
    let names = program_names(name);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|folder| folder.is_absolute())
        .flat_map(|folder| names.iter().map(move |name| folder.join(name)))
        .find(|candidate| candidate.is_file())
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// No console window for a child started from a program without one.
#[cfg(windows)]
pub fn no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Start `command` detached from this process: its own process group on
/// Unix, as before. On Windows the engine's flags (its `hooks_runtime`,
/// after Graphify's hooks.py): a new process group with a hidden console,
/// outside this process's job when the job allows it (the start is retried
/// without leaving the job). Not DETACHED_PROCESS: with it CREATE_NO_WINDOW
/// is ignored, and each console program the child starts (git) would open a
/// window. Windows starts a child with every inheritable handle of its
/// parent, whatever its own standard handles are, so this process's
/// standard handles are made non-inheritable first: else the detached child
/// holds the pipes a hook's caller reads (Claude Code, Git) open, and the
/// caller waits for the child instead of this process. Children started
/// later with inherited output still get it (the standard library hands
/// them their own inheritable copy).
pub fn spawn_detached(command: &mut Command) -> std::io::Result<std::process::Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        win::keep_standard_handles();
        command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = command.spawn() {
            return Ok(child);
        }
        command.creation_flags(flags);
    }
    command.spawn()
}

/// The command that opens an `http` or `https` address in the default
/// browser on Windows: `cmd /d /c start "" "<url>"`, the address quoted so
/// `&` stays in it. Anything else, or a quote or control character, is
/// refused, so `start` never runs a program; so are `%` and `!`, which cmd
/// expands inside quotes (`%VAR%`, and `!VAR!` with delayed expansion).
/// The caller prints a refused address instead.
#[cfg(windows)]
pub fn open_url_command(url: &str) -> Option<Command> {
    use std::os::windows::process::CommandExt;
    let web = url.starts_with("http://") || url.starts_with("https://");
    let refused = |c: char| matches!(c, '"' | '%' | '!') || c.is_control();
    if !web || url.chars().any(refused) {
        return None;
    }
    let mut command = Command::new("cmd");
    command.raw_arg(format!("/d /c start \"\" \"{url}\""));
    no_window(&mut command);
    Some(command)
}

/// Whether ANSI escapes reach standard output: always on macOS and Linux;
/// on Windows once virtual-terminal processing is on for it (enabled on the
/// first call), else the caller writes plain text.
pub fn ansi_output() -> bool {
    #[cfg(not(windows))]
    {
        true
    }
    #[cfg(windows)]
    {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| win::enable_virtual_terminal(win::STD_OUTPUT_HANDLE))
    }
}

/// The bytes this user may still write on the volume holding the folder
/// `folder` (`GetDiskFreeSpaceExW`'s caller-available count, which honours
/// disk quotas), or `None` when Windows cannot say.
#[cfg(windows)]
pub fn free_bytes(folder: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    // A UNC folder must end in a separator; any other folder may.
    let mut wide: Vec<u16> = folder.as_os_str().encode_wide().collect();
    if !matches!(wide.last(), Some(&c) if c == u16::from(b'\\') || c == u16::from(b'/')) {
        wide.push(u16::from(b'\\'));
    }
    wide.push(0);
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the function
    // writes only the one u64 it is given (ULARGE_INTEGER) and skips the
    // two null outputs.
    let ok = unsafe {
        win::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

/// While alive, this process ignores Ctrl+C and Ctrl+Break on Windows, so a
/// child sharing the console (the engine) ends first and its exit status is
/// reported. Child processes keep their own handling: a handler routine is
/// not inherited. Nothing on macOS and Linux.
pub struct IgnoreInterrupts {
    #[cfg(windows)]
    installed: bool,
}

impl IgnoreInterrupts {
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            // SAFETY: registers a handler with the signature the API expects.
            let installed =
                unsafe { win::SetConsoleCtrlHandler(Some(win::ignore_interrupt), 1) } != 0;
            Self { installed }
        }
        #[cfg(not(windows))]
        {
            Self {}
        }
    }
}

impl Default for IgnoreInterrupts {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        if self.installed {
            // SAFETY: removes the handler `new` registered.
            unsafe { win::SetConsoleCtrlHandler(Some(win::ignore_interrupt), 0) };
        }
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
        fn SetHandleInformation(object: *mut c_void, mask: u32, flags: u32) -> i32;
        pub fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
        pub fn GetDiskFreeSpaceExW(
            directory: *const u16,
            available_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }

    /// Turn on ANSI processing for a console handle; false when the handle
    /// is not a console or the console cannot do it.
    pub fn enable_virtual_terminal(which: u32) -> bool {
        // SAFETY: plain Win32 calls on a handle this process owns; a
        // missing or redirected handle makes GetConsoleMode fail.
        unsafe {
            let handle = GetStdHandle(which);
            if handle.is_null() || handle as isize == -1 {
                return false;
            }
            let mut mode = 0u32;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return false;
            }
            mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
                || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }

    /// Make this process's standard input, output and error handles
    /// non-inheritable, so a child started next does not get them unless
    /// it is given them; a missing handle, or one that refuses, is left.
    pub fn keep_standard_handles() {
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain Win32 calls on this process's own standard
            // handles; the flag change does not close or replace them.
            unsafe {
                let handle = GetStdHandle(which);
                if !handle.is_null() && handle as isize != -1 {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }

    pub unsafe extern "system" fn ignore_interrupt(kind: u32) -> i32 {
        i32::from(kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_paths_lose_their_prefix_when_safe() {
        assert_eq!(
            strip_verbatim(r"\\?\C:\Users\me\repo").as_deref(),
            Some(r"C:\Users\me\repo")
        );
        assert_eq!(strip_verbatim(r"\\?\C:\").as_deref(), Some(r"C:\"));
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\repo").as_deref(),
            Some(r"\\server\share\repo")
        );
        assert_eq!(
            strip_verbatim(r"\\?\UNC\wsl.localhost\Ubuntu\home").as_deref(),
            Some(r"\\wsl.localhost\Ubuntu\home")
        );
        // Not verbatim, or not safe to write plainly.
        assert_eq!(strip_verbatim(r"C:\Users\me"), None);
        assert_eq!(strip_verbatim("/home/me/repo"), None);
        assert_eq!(strip_verbatim(r"\\?\C:\repo\con.txt"), None);
        assert_eq!(strip_verbatim(r"\\?\C:\repo\name."), None);
        assert_eq!(strip_verbatim(r"\\?\C:\repo\..\x"), None);
        assert_eq!(strip_verbatim(r"\\?\C:\a:b"), None);
        assert_eq!(strip_verbatim(r"\\?\GLOBALROOT\Device"), None);
        assert_eq!(strip_verbatim(r"\\?\Volume{1234}\x"), None);
        assert_eq!(strip_verbatim(r"\\?\UNC\server"), None);
        let long = format!(r"\\?\C:\{}", "a\\".repeat(140));
        assert_eq!(strip_verbatim(&long), None);
    }

    #[test]
    fn reserved_names_follow_dos() {
        assert!(reserved_name("CON"));
        assert!(reserved_name("nul.txt"));
        assert!(reserved_name("com1 .log"));
        assert!(!reserved_name("console"));
        assert!(!reserved_name(".con"));
    }

    #[test]
    fn program_names_use_pathext() {
        assert_eq!(
            program_names_with("codex", Some(".COM;.EXE;.BAT;.CMD")),
            ["codex.com", "codex.exe", "codex.bat", "codex.cmd"]
        );
        assert_eq!(program_names_with("git.EXE", None), ["git.EXE"]);
        assert_eq!(program_names_with("claude", Some(" ")).len(), 4);
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_helpers_are_unchanged() {
        assert_eq!(program_names("codex"), ["codex"]);
        let path = PathBuf::from(r"\\?\C:\x");
        assert_eq!(simplified(path.clone()), path);
        assert!(ansi_output());
        let _guard = IgnoreInterrupts::new();
    }

    #[cfg(windows)]
    #[test]
    fn windows_canonical_paths_are_plain() {
        let dir = std::env::temp_dir();
        let canonical = dir.canonical().unwrap();
        assert!(!canonical.to_string_lossy().starts_with(r"\\?\"));
        assert!(canonical.is_absolute());
        assert_eq!(
            simplified(PathBuf::from(r"\\?\C:\Windows")),
            PathBuf::from(r"C:\Windows")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_programs_are_found_with_pathext() {
        let names = program_names("cmd");
        assert!(
            names
                .iter()
                .any(|name| name.eq_ignore_ascii_case("cmd.exe"))
        );
        let found = find_program("cmd").expect("cmd.exe on PATH");
        assert!(
            found
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("cmd.exe")
        );
        let output = command("cmd")
            .args(["/d", "/c", "echo", "ok"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("ok"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_home_and_app_data() {
        assert!(home_dir().is_some_and(|home| home.is_absolute()));
        assert!(local_app_data().is_some_and(|folder| folder.is_absolute()));
        assert!(roaming_app_data().is_some_and(|folder| folder.is_absolute()));
        let fixture =
            std::env::temp_dir().join(format!("graphicle-desktop-{}", std::process::id()));
        let roaming = fixture.join("Roaming");
        let local = fixture.join("Local");
        assert_eq!(
            claude_desktop_dir(Some(&roaming), Some(&local)),
            Some(roaming.join("Claude"))
        );
        let store = local.join(r"Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude");
        std::fs::create_dir_all(&store).unwrap();
        assert_eq!(
            claude_desktop_dir(Some(&roaming), Some(&local)),
            Some(store)
        );
        std::fs::remove_dir_all(&fixture).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_data_folder_and_doctor_lookup() {
        let data = crate::local::default_data_dir();
        assert!(
            data.ends_with("Graphicle") && data.is_absolute(),
            "{}",
            data.display()
        );
        assert!(data.starts_with(local_app_data().unwrap()));
        let found = crate::terminal::resolve_program("cmd").expect("cmd.exe on PATH");
        assert!(
            found
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("cmd.exe")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_detached_children_and_urls() {
        let mut child = Command::new("cmd");
        child.args(["/d", "/c", "exit", "0"]);
        let mut started = spawn_detached(&mut child).unwrap();
        assert!(started.wait().unwrap().success());
        assert!(open_url_command("file:///C:/Windows/notepad.exe").is_none());
        assert!(open_url_command("http://127.0.0.1:1/\"&calc").is_none());
        assert!(open_url_command("http://127.0.0.1:1/%PATH%").is_none());
        assert!(open_url_command("http://127.0.0.1:1/!PATH!").is_none());
        assert!(open_url_command("http://127.0.0.1:1/#a&b").is_some());
        let _ = ansi_output();
        let guard = IgnoreInterrupts::new();
        assert!(guard.installed);
        drop(guard);
    }

    #[cfg(windows)]
    #[test]
    fn windows_free_space_is_known() {
        let temp = std::env::temp_dir();
        let free = free_bytes(&temp).expect("free space of the temporary folder");
        assert!(free > 0);
        let drive = temp.ancestors().last().unwrap().to_path_buf();
        assert!(free_bytes(&drive).is_some(), "{}", drive.display());
        assert!(crate::terminal::free_bytes(&temp.join("not-made-yet")).is_some());
    }
}
