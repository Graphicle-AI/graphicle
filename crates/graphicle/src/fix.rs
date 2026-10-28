//! `graphicle fix`: a change for an issue, made and tested on a new branch.
//!
//! One cache-aware pack focused on the issue ([`crate::code_pack`]) and a
//! plan (the `plan` command's validated JSON), then at most `--rounds` rounds
//! of edits. Each round the model returns search-and-replace blocks per file,
//! whole new files and a short explanation as structured output. They are
//! checked as a whole and applied, all or none, in a new git worktree on a new
//! branch from HEAD (never the person's working tree or branch), made once
//! the plan has come back, and the test command runs there with a timeout. A
//! failure's output tail and the diff so far go back to the model as the next
//! round, whose start (the instructions, the pack, the issue and the plan) the
//! provider's prompt cache can still hold. Every round's request is fitted to
//! the model's input limit.
//!
//! The test command comes from `--test` or the configuration's `fix.test`,
//! never from the model: its test suggestions are shown, not run. It runs
//! model-edited code with the person's privileges, without the variables
//! that hold model keys. Edits may only touch files inside the repository:
//! absolute paths, `..`, any spelling of `.git` and symlinks that lead
//! outside or into `.git` are refused. Every git command runs with hooks
//! turned off and without the variables that would point git at another
//! repository. Nothing is fetched or pushed.

use crate::long_context::{self, Request, Run, Tree};
use crate::{Error, Result, model};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What `fix` tells the model in every round: the same words each time, so
/// that the provider's prompt cache matches them and the pack after them.
pub const FIX_INSTRUCTIONS: &str = "You fix an issue in a code repository from a pack of its code. The pack starts with a header. It holds an architecture summary, a manifest of the files it holds, and the files themselves, each headed by its path, line range and why it is included; a pack in the cache layout has a repository part first and a question part after it. Code lines that start with a number and a tab carry their line number, which is not part of the file. After the pack come the issue and a plan, and after a round whose tests failed, the end of the test output and the diff so far. Return one JSON object that matches the schema: the edits, each a file's path, a search text copied exactly from the file as it is now (without the line numbers the pack adds, and long enough to occur once in the file) and its replacement; new files with their whole content; a short explanation that cites the code as path:line; and the tests you would run or add. Change only what the issue needs, keep the code's style, and use paths exactly as the pack shows them.";

/// The edits' JSON schema.
pub fn edits_schema() -> Value {
    json!({"type":"object","properties":{
        "explanation":{"type":"string"},
        "edits":{"type":"array","items":{"type":"object","properties":{
            "path":{"type":"string"},"search":{"type":"string"},"replace":{"type":"string"}}}},
        "new_files":{"type":"array","items":{"type":"object","properties":{
            "path":{"type":"string"},"content":{"type":"string"}}}},
        "tests":{"type":"array","items":{"type":"string"}}}})
}

/// Edits and new files one round may make at most.
const MAX_EDITS: usize = 100;
const MAX_NEW_FILES: usize = 20;
/// The largest file an edit or a new file may write.
const MAX_FILE_BYTES: usize = 4 << 20;
/// The end of a test's output that is kept and sent back.
const OUTPUT_TAIL: usize = 6_000;
/// The end of each output stream held while a test runs.
const OUTPUT_KEEP: usize = 64 << 10;
/// How long a test's output is still read after the command ends.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// The diff so far that a follow-up sends at most.
const DIFF_CAP: usize = 24_000;
/// Tokens a pack without `--budget` leaves free, beyond the room `ask
/// --pack` keeps, for what the rounds add after it: the plan, the test
/// output's end and the diff so far. Each round is still fitted to the
/// input limit.
const ROUND_ROOM: usize = 12_000;
/// The variable that marks a test run's processes (to find the ones that
/// left its process group).
const TEST_MARKER: &str = "GRAPHICLE_FIX_TEST_RUN";
/// The variables model keys are usually read from, besides the ones the
/// configuration names.
const KEY_VARIABLES: &[&str] = &[
    "MODEL_API_KEY",
    // Muse Code reads the Meta Model API key from it.
    "META_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "AZURE_OPENAI_API_KEY",
];

/// What `fix` is asked for.
pub struct Options {
    pub issue: String,
    /// The test command, run with `sh -c` in the worktree.
    pub test: Option<String>,
    pub rounds: usize,
    pub branch: Option<String>,
    pub commit: bool,
    pub timeout: Duration,
    /// Passages the pack takes first, whatever the issue's words rank
    /// (`--focus`: the places `graphicle pixel` located, for one): path,
    /// first and last line.
    pub focus: Vec<(String, usize, usize)>,
}

/// The most `--focus` passages.
pub const MAX_FOCUS: usize = 50;
/// Lines on each side of a `--focus PATH:LINE`.
const FOCUS_AROUND: usize = 40;
/// The last line of a `--focus PATH` (the whole file; the pack stops at its
/// end).
const WHOLE_FILE: usize = 1_000_000;

/// A `--focus` passage: `PATH`, `PATH:LINE` (the lines around it) or
/// `PATH:START-END`, with the path as the project shows it.
pub fn parse_focus(spec: &str) -> std::result::Result<(String, usize, usize), String> {
    let spec = spec.trim();
    let number = |text: &str| text.parse::<usize>().ok().filter(|&n| n >= 1);
    let (path, start, end) = match spec.rsplit_once(':') {
        Some((path, lines))
            if !lines.is_empty() && lines.bytes().all(|b| b.is_ascii_digit() || b == b'-') =>
        {
            match lines.split_once('-') {
                Some((a, b)) => match (number(a), number(b)) {
                    (Some(a), Some(b)) => (path, a.min(b), a.max(b)),
                    _ => return Err(format!("--focus {spec}: lines are START-END from 1")),
                },
                None => match number(lines) {
                    Some(line) => (
                        path,
                        line.saturating_sub(FOCUS_AROUND).max(1),
                        line + FOCUS_AROUND,
                    ),
                    None => return Err(format!("--focus {spec}: a line counts from 1")),
                },
            }
        }
        _ => (spec, 1, WHOLE_FILE),
    };
    if path.is_empty() || path.len() > 4096 {
        return Err(format!("--focus {spec}: give a path of the project"));
    }
    Ok((path.to_string(), start, end))
}

/// The pack's `focus` argument for `passages`; null when there are none.
fn focus_json(passages: &[(String, usize, usize)]) -> Value {
    if passages.is_empty() {
        return Value::Null;
    }
    json!(
        passages
            .iter()
            .map(|(path, start, end)| json!({"path":path,"start_line":start,"end_line":end}))
            .collect::<Vec<_>>()
    )
}

/// Whether one part of a path names `.git` on some file system: in any
/// case, with the trailing dots or spaces Windows drops, or as a Windows
/// short name such as `GIT~1`.
fn is_git_name(part: &str) -> bool {
    let folded = part.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    if folded == ".git" {
        return true;
    }
    folded
        .strip_prefix("git~")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The parts of `path` as the model wrote it, empty and `.` parts dropped,
/// or why it is refused.
fn path_parts(path: &str) -> std::result::Result<Vec<&str>, String> {
    if path.is_empty() || path.len() > 4096 {
        return Err("an empty or overlong path".into());
    }
    if path.contains(['\\', '\0', ':']) {
        return Err("a path with a backslash, colon or NUL".into());
    }
    if path.starts_with('/') {
        return Err("an absolute path".into());
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err("a path that leaves the repository (..)".into()),
            part if is_git_name(part) => return Err("a path inside .git".into()),
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err("an empty path".into());
    }
    Ok(parts)
}

/// The file `path` (as the model wrote it) names inside `root`, or why it is
/// refused: an empty, absolute or Windows path, `..`, any spelling of
/// `.git`, or a symlink (on the way or at the end) that leads outside `root`
/// or into a `.git`.
pub fn safe_path(root: &Path, path: &str) -> std::result::Result<PathBuf, String> {
    let parts = path_parts(path)?;
    let real_root = root
        .canonicalize()
        .map_err(|e| format!("the worktree cannot be read ({e})"))?;
    let full = parts
        .iter()
        .fold(root.to_path_buf(), |p, part| p.join(part));
    // The deepest part of the path that exists, the file itself included,
    // must resolve inside the repository and outside its .git.
    let mut probe = full.clone();
    loop {
        if probe.symlink_metadata().is_ok() {
            let real = probe
                .canonicalize()
                .map_err(|_| "a symlink that leads nowhere".to_string())?;
            let Ok(inside) = real.strip_prefix(&real_root) else {
                return Err("a symlink that leads outside the repository".into());
            };
            if inside.components().any(|c| match c {
                Component::Normal(part) => is_git_name(&part.to_string_lossy()),
                _ => false,
            }) {
                return Err("a path inside .git".into());
            }
            break;
        }
        if !probe.pop() {
            break;
        }
    }
    Ok(full)
}

/// The text with a line-number prefix (`123<tab>`) taken off each line, when
/// every line has one: the pack's numbering copied into a search text.
fn unnumbered(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut any = false;
    for line in text.split_inclusive('\n') {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || line.as_bytes().get(digits) != Some(&b'\t') {
            return None;
        }
        out.push_str(&line[digits + 1..]);
        any = true;
    }
    any.then_some(out)
}

/// Whether the file system under `root` ignores case: the root under its
/// own name with the case of its letters swapped is found too.
fn case_insensitive(root: &Path) -> bool {
    let Some(name) = root.file_name().and_then(|n| n.to_str()) else {
        return cfg!(any(windows, target_os = "macos"));
    };
    let swapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    if swapped == name {
        return cfg!(any(windows, target_os = "macos"));
    }
    root.with_file_name(swapped).exists()
}

/// The key a file is staged under: an existing file's identity on disk, so
/// that every spelling of it is one file; else its path as normalised,
/// case-folded where the file system ignores case.
fn file_key(full: &Path, normalised: &str, fold: bool) -> String {
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(full) {
        use std::os::unix::fs::MetadataExt;
        return format!("file:{}:{}", meta.dev(), meta.ino());
    }
    #[cfg(not(unix))]
    let _ = full;
    if fold {
        format!("path:{}", normalised.to_lowercase())
    } else {
        format!("path:{normalised}")
    }
}

/// The files a round wrote: changed and created, as paths in the project.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Applied {
    pub changed: Vec<String>,
    pub created: Vec<String>,
}

/// One file of a round, checked and ready to write.
struct Staged {
    /// The path as normalised.
    path: String,
    full: PathBuf,
    text: String,
    /// Its bytes before the round, for an edit.
    before: Option<Vec<u8>>,
}

/// Apply a round's `reply` (edits and new files) under `root`, all or none:
/// every path is checked, every search text found exactly once, and the new
/// files checked against each other and the folders on disk, before anything
/// is written; a write that fails puts back every file the round touched.
/// Else the problems, one per refused item.
pub fn apply(root: &Path, reply: &Value) -> std::result::Result<Applied, Vec<String>> {
    let edits = reply["edits"].as_array().cloned().unwrap_or_default();
    let new_files = reply["new_files"].as_array().cloned().unwrap_or_default();
    if edits.len() > MAX_EDITS || new_files.len() > MAX_NEW_FILES {
        return Err(vec![format!(
            "too many changes in one round (at most {MAX_EDITS} edits and {MAX_NEW_FILES} new files)"
        )]);
    }
    let fold = case_insensitive(root);
    let mut problems = Vec::new();
    // key -> the file, staged
    let mut staged: BTreeMap<String, Staged> = BTreeMap::new();
    for edit in &edits {
        let spelled = edit["path"].as_str().unwrap_or("");
        let search = edit["search"].as_str().unwrap_or("");
        let replace = edit["replace"].as_str().unwrap_or("");
        let full = match safe_path(root, spelled) {
            Ok(full) => full,
            Err(why) => {
                problems.push(format!("{spelled}: refused, {why}"));
                continue;
            }
        };
        let path = path_parts(spelled).unwrap_or_default().join("/");
        let key = file_key(&full, &path, fold);
        let current = match staged.get(&key) {
            Some(file) if file.before.is_none() => {
                problems.push(format!(
                    "{path}: a new file of this round (write it whole in new_files)"
                ));
                continue;
            }
            Some(file) => file.text.clone(),
            None if full.is_file() => match std::fs::read(&full) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(_) => {
                        problems.push(format!("{path}: not a text file"));
                        continue;
                    }
                },
                Err(_) => {
                    problems.push(format!("{path}: cannot be read"));
                    continue;
                }
            },
            None => {
                problems.push(format!(
                    "{path}: no such file (a new file goes in new_files)"
                ));
                continue;
            }
        };
        if search.is_empty() {
            problems.push(format!("{path}: an empty search text"));
            continue;
        }
        let (search, replace) = match current.matches(search).count() {
            0 => match (unnumbered(search), unnumbered(replace)) {
                (Some(s), r) if current.matches(&s).count() == 1 => {
                    (s, r.unwrap_or_else(|| replace.to_string()))
                }
                _ => {
                    problems.push(format!(
                        "{path}: the search text is not in the file as it is now"
                    ));
                    continue;
                }
            },
            1 => (search.to_string(), replace.to_string()),
            n => {
                problems.push(format!(
                    "{path}: the search text occurs {n} times; give a longer one"
                ));
                continue;
            }
        };
        let text = current.replacen(&search, &replace, 1);
        if text.len() > MAX_FILE_BYTES {
            problems.push(format!("{path}: the edited file is too large"));
            continue;
        }
        match staged.get_mut(&key) {
            Some(file) => file.text = text,
            None => {
                let before = Some(current.into_bytes());
                staged.insert(
                    key,
                    Staged {
                        path,
                        full,
                        text,
                        before,
                    },
                );
            }
        }
    }
    // New files: the paths, then the folders they need, checked as a whole.
    let mut new_paths: BTreeSet<String> = BTreeSet::new();
    let fold_path = |p: &str| {
        if fold {
            p.to_lowercase()
        } else {
            p.to_string()
        }
    };
    for file in &new_files {
        let spelled = file["path"].as_str().unwrap_or("");
        let content = file["content"].as_str().unwrap_or("");
        let full = match safe_path(root, spelled) {
            Ok(full) => full,
            Err(why) => {
                problems.push(format!("{spelled}: refused, {why}"));
                continue;
            }
        };
        let path = path_parts(spelled).unwrap_or_default().join("/");
        let key = file_key(&full, &path, fold);
        if full.symlink_metadata().is_ok() || staged.contains_key(&key) {
            problems.push(format!("{path}: exists already (edit it instead)"));
            continue;
        }
        if content.len() > MAX_FILE_BYTES {
            problems.push(format!("{path}: the new file is too large"));
            continue;
        }
        new_paths.insert(fold_path(&path));
        staged.insert(
            key,
            Staged {
                path,
                full,
                text: content.to_string(),
                before: None,
            },
        );
    }
    for file in staged.values().filter(|f| f.before.is_none()) {
        let parts: Vec<&str> = file.path.split('/').collect();
        for depth in 1..parts.len() {
            let folder = parts[..depth].join("/");
            if new_paths.contains(&fold_path(&folder)) {
                problems.push(format!(
                    "{}: its folder {folder} is a new file of this round",
                    file.path
                ));
                break;
            }
            let on_disk = root.join(&folder);
            if on_disk.symlink_metadata().is_ok() && !on_disk.is_dir() {
                problems.push(format!(
                    "{}: its folder {folder} is a file in the repository",
                    file.path
                ));
                break;
            }
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    write_all(staged.into_values().collect())
}

/// Write a round's files; on a failed write, put back every file written so
/// far and remove the files and folders the round made.
fn write_all(files: Vec<Staged>) -> std::result::Result<Applied, Vec<String>> {
    let mut written: Vec<&Staged> = Vec::new();
    let mut folders: Vec<PathBuf> = Vec::new();
    let mut failure = None;
    for file in &files {
        if file.before.is_none()
            && let Some(parent) = file.full.parent()
        {
            let mut missing = Vec::new();
            let mut probe = parent.to_path_buf();
            while probe.symlink_metadata().is_err() {
                missing.push(probe.clone());
                if !probe.pop() {
                    break;
                }
            }
            for folder in missing.into_iter().rev() {
                match std::fs::create_dir(&folder) {
                    Ok(()) => folders.push(folder),
                    Err(error) => {
                        failure =
                            Some(format!("{}: cannot create its folder ({error})", file.path));
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
        }
        let result = if file.before.is_some() {
            std::fs::write(&file.full, &file.text)
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file.full)
                .and_then(|mut handle| {
                    use std::io::Write;
                    handle.write_all(file.text.as_bytes())
                })
        };
        match result {
            Ok(()) => written.push(file),
            Err(error) => {
                written.push(file);
                failure = Some(format!("{}: cannot write it ({error})", file.path));
                break;
            }
        }
    }
    if let Some(failure) = failure {
        for file in written.iter().rev() {
            match &file.before {
                Some(before) => {
                    let _ = std::fs::write(&file.full, before);
                }
                None => {
                    let _ = std::fs::remove_file(&file.full);
                }
            }
        }
        for folder in folders.iter().rev() {
            let _ = std::fs::remove_dir(folder);
        }
        return Err(vec![format!(
            "{failure}; the files of this round were put back"
        )]);
    }
    let mut applied = Applied::default();
    for file in &files {
        if file.before.is_some() {
            applied.changed.push(file.path.clone());
        } else {
            applied.created.push(file.path.clone());
        }
    }
    Ok(applied)
}

/// What the test command's environment leaves out and its output hides:
/// the variables model keys are read from, and their values.
#[derive(Clone, Debug, Default)]
pub struct Scrub {
    pub remove: Vec<String>,
    pub secrets: Vec<String>,
}

impl Scrub {
    /// The model keys of `config`: the variables its profiles read their
    /// keys from and the providers' usual ones, and the values set here.
    /// `fix` passes the merged configuration, with the profiles stored in
    /// the workspace.
    pub fn of(config: &model::Config) -> Self {
        let mut names: BTreeSet<String> = KEY_VARIABLES.iter().map(|n| n.to_string()).collect();
        for profile in config.models.values() {
            if let Some(name) = &profile.key_env
                && !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                names.insert(name.clone());
            }
        }
        let secrets = names
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .filter(|value| value.len() >= 8)
            .collect();
        Self {
            remove: names.into_iter().collect(),
            secrets,
        }
    }

    fn redact(&self, text: &str) -> String {
        let mut text = text.to_string();
        for secret in &self.secrets {
            text = text.replace(secret.as_str(), "[redacted]");
        }
        text
    }
}

/// A test run's result.
#[derive(Clone, Debug)]
pub struct TestRun {
    pub passed: bool,
    pub code: Option<i32>,
    pub seconds: f64,
    pub timed_out: bool,
    /// The end of its output (standard output, then standard error).
    pub tail: String,
}

/// The last `cap` bytes of `text`, cut at a character boundary.
fn tail(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("...\n{}", &text[start..])
}

/// The end of an output stream, at most [`OUTPUT_KEEP`] bytes of it.
#[derive(Default)]
struct Kept {
    bytes: Vec<u8>,
    cut: bool,
}

impl Kept {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > 2 * OUTPUT_KEEP {
            let drop = self.bytes.len() - OUTPUT_KEEP;
            self.bytes.drain(..drop);
            self.cut = true;
        }
    }

    fn text(&self) -> String {
        let start = self.bytes.len().saturating_sub(OUTPUT_KEEP);
        let text = String::from_utf8_lossy(&self.bytes[start..]).into_owned();
        if self.cut || start > 0 {
            format!("...\n{text}")
        } else {
            text
        }
    }
}

/// Read `stream` into `kept` until it closes; say so on `done`.
fn keep(
    mut stream: impl std::io::Read + Send + 'static,
    kept: Arc<Mutex<Kept>>,
    done: std::sync::mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(n) = stream.read(&mut buffer) {
            if n == 0 {
                break;
            }
            kept.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(&buffer[..n]);
        }
        let _ = done.send(());
    });
}

/// Stop what a test run left: its process group (the command itself when it
/// is still `running`), and on Linux any process of the run that left the
/// group (found by the run's marker in its environment). On Windows, a
/// command still running is stopped with its process tree.
fn stop_run(pid: u32, marker: &str, running: bool) {
    #[cfg(unix)]
    // SAFETY: a signal to the process group this run started (its leader's
    // id); with the leader gone, the group's other members.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(windows)]
    if running {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    let _ = running;
    #[cfg(target_os = "linux")]
    {
        let needle = format!("{TEST_MARKER}={marker}");
        let me = std::process::id();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(other) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<libc::pid_t>().ok())
                else {
                    continue;
                };
                if other as u32 == me {
                    continue;
                }
                let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                    continue;
                };
                if environ
                    .split(|b| *b == 0)
                    .any(|var| var == needle.as_bytes())
                {
                    // SAFETY: a signal to a process that carries this run's
                    // marker, so one the test command started.
                    unsafe {
                        libc::kill(other, libc::SIGKILL);
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = marker;
}

/// Run `command` with `sh -c` (`cmd /C` on Windows) in `dir`, without the
/// variables `scrub` names. The run ends when the command itself ends (or
/// after `timeout`); then whatever it left running in its process group is
/// stopped, and on Linux so are the processes that left the group. Output is
/// kept to its end, read for [`OUTPUT_GRACE`] more at most, and the values
/// `scrub` holds are hidden in it.
pub fn run_test(dir: &Path, command: &str, timeout: Duration, scrub: &Scrub) -> TestRun {
    #[cfg(unix)]
    let mut child = {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new("sh");
        c.arg("-c").arg(command).process_group(0);
        c
    };
    #[cfg(not(unix))]
    let mut child = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };
    let marker = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    child
        .current_dir(dir)
        .env(TEST_MARKER, &marker)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in &scrub.remove {
        child.env_remove(name);
    }
    // On Linux, the test cannot read this process's environment or memory
    // (which may hold a model key) through /proc/<pid>: a process that is not
    // dumpable has those files owned by root. Its children are dumpable again
    // once they exec, so the sweep in `stop_run` still reads theirs. This is
    // defence in depth, not isolation: the test runs as the user.
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_DUMPABLE changes only this process's dumpable flag.
    unsafe {
        let off: libc::c_ulong = 0;
        libc::prctl(libc::PR_SET_DUMPABLE, off, off, off, off);
    }
    let start = Instant::now();
    let mut spawned = match child.spawn() {
        Ok(spawned) => spawned,
        Err(error) => {
            return TestRun {
                passed: false,
                code: None,
                seconds: 0.0,
                timed_out: false,
                tail: format!("The test command did not start: {error}"),
            };
        }
    };
    let pid = spawned.id();
    let (done, finished) = std::sync::mpsc::channel();
    let out = Arc::new(Mutex::new(Kept::default()));
    let err = Arc::new(Mutex::new(Kept::default()));
    let mut streams = 0;
    if let Some(stdout) = spawned.stdout.take() {
        keep(stdout, out.clone(), done.clone());
        streams += 1;
    }
    if let Some(stderr) = spawned.stderr.take() {
        keep(stderr, err.clone(), done.clone());
        streams += 1;
    }
    drop(done);
    let (status, timed_out) = loop {
        match spawned.try_wait() {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if start.elapsed() >= timeout => break (None, true),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break (None, false),
        }
    };
    stop_run(pid, &marker, timed_out);
    let status = match status {
        Some(status) => Some(status),
        None => {
            let _ = spawned.kill();
            spawned.wait().ok()
        }
    };
    let seconds = (start.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    // The output read so far, and what comes within the grace period; a
    // stream still held open by a process that escaped is left behind.
    let deadline = Instant::now() + OUTPUT_GRACE;
    for _ in 0..streams {
        let left = deadline.saturating_duration_since(Instant::now());
        if finished.recv_timeout(left).is_err() {
            break;
        }
    }
    let text = format!(
        "{}{}",
        out.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .text(),
        err.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .text()
    );
    TestRun {
        passed: status.is_some_and(|s| s.success()) && !timed_out,
        code: status.and_then(|s| s.code()),
        seconds,
        timed_out,
        tail: tail(&scrub.redact(&text), OUTPUT_TAIL),
    }
}

/// Text from a model or a test, safe to print to a terminal: escape
/// sequences and control characters other than newlines and tabs removed.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: parameters, then one final byte from @ to ~.
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS and the like: to BEL or ESC \.
                Some(']' | 'P' | '^' | '_' | 'X') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// git, as `fix` runs it: with hooks turned off (an empty hooks folder),
/// without the variables that would point it at another repository or add
/// configuration, and with its error's end in the error.
pub(crate) struct Git {
    hooks: PathBuf,
}

/// Variables that point git at another repository, index or object store,
/// or add configuration on top of the files. The variables that choose the
/// user's own configuration files (`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`,
/// `GIT_CONFIG_NOSYSTEM`) are kept: they hold the commit identity, and the
/// command line's `-c core.hooksPath` wins over them.
fn redirects_git(name: &str) -> bool {
    matches!(
        name,
        "GIT_DIR"
            | "GIT_WORK_TREE"
            | "GIT_INDEX_FILE"
            | "GIT_OBJECT_DIRECTORY"
            | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
            | "GIT_COMMON_DIR"
            | "GIT_NAMESPACE"
            | "GIT_PREFIX"
            | "GIT_CEILING_DIRECTORIES"
            | "GIT_DISCOVERY_ACROSS_FILESYSTEM"
            | "GIT_CONFIG"
            | "GIT_CONFIG_PARAMETERS"
            | "GIT_CONFIG_COUNT"
            | "GIT_EXTERNAL_DIFF"
            | "GIT_DIFF_OPTS"
    ) || name.starts_with("GIT_CONFIG_KEY_")
        || name.starts_with("GIT_CONFIG_VALUE_")
}

impl Git {
    /// Hooks off through an empty folder in the data folder.
    pub(crate) fn new(data: &Path) -> Result<Self> {
        let hooks = data.join("fix").join("no-hooks");
        std::fs::create_dir_all(&hooks)?;
        Ok(Self { hooks })
    }

    /// `git -C dir ARGS`: its standard output, or its error's last lines.
    pub(crate) fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let mut command = Command::new("git");
        command
            .arg("-c")
            .arg(format!("core.hooksPath={}", self.hooks.display()))
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if let Some(name) = name.to_str()
                && redirects_git(name)
            {
                command.env_remove(name);
            }
        }
        let output = command
            .output()
            .map_err(|e| Error::bad(format!("git did not run: {e}")))?;
        if !output.status.success() {
            return Err(Error::bad(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                tail(String::from_utf8_lossy(&output.stderr).trim(), 400)
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// A branch-name word list from the issue: lower case, letters and digits
/// joined by hyphens, at most 40 characters.
pub fn slug(issue: &str) -> String {
    let mut out = String::new();
    for word in issue
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        if !out.is_empty() && out.len() + 1 + word.len() > 40 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word[..word.len().min(40)]);
    }
    if out.is_empty() { "issue".into() } else { out }
}

/// The repository of the project's folder: its top folder, the project's
/// folder inside it, and HEAD.
struct Repository {
    top: PathBuf,
    prefix: String,
    base: String,
}

impl Repository {
    fn of(git: &Git, folder: &Path) -> Result<Self> {
        let top = PathBuf::from(git.run(folder, &["rev-parse", "--show-toplevel"])?.trim());
        let prefix = git
            .run(folder, &["rev-parse", "--show-prefix"])?
            .trim()
            .to_string();
        let base = git
            .run(folder, &["rev-parse", "HEAD"])
            .map_err(|_| {
                Error::bad("fix needs a commit to start from: the repository has no HEAD")
            })?
            .trim()
            .to_string();
        Ok(Self { top, prefix, base })
    }
}

/// The new worktree: its branch, its folder, the folder of the project in it
/// (the project may be a subfolder of its repository) and the base commit.
struct Worktree {
    branch: String,
    root: PathBuf,
    project: PathBuf,
    base: String,
}

/// Make the branch and its worktree from HEAD, under the data folder.
fn new_worktree(
    git: &Git,
    repository: &Repository,
    data: &Path,
    wanted: Option<&str>,
    issue: &str,
) -> Result<Worktree> {
    let top = &repository.top;
    let exists = |name: &str| {
        git.run(
            top,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .is_ok()
    };
    let branch = match wanted {
        Some(name) => {
            if name.starts_with('-')
                || git
                    .run(top, &["check-ref-format", "--branch", name])
                    .is_err()
            {
                return Err(Error::bad(format!("{name} is not a branch name git takes")));
            }
            if exists(name) {
                return Err(Error::bad(format!(
                    "The branch {name} exists already: fix makes a new one"
                )));
            }
            name.to_string()
        }
        None => {
            let stem = format!("graphicle/fix-{}", slug(issue));
            (1..100)
                .map(|n| {
                    if n == 1 {
                        stem.clone()
                    } else {
                        format!("{stem}-{n}")
                    }
                })
                .find(|name| !exists(name))
                .ok_or_else(|| Error::bad("No free branch name for this issue"))?
        }
    };
    let name = top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repository".into());
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let root = data
        .join("fix")
        .join(format!("{}-{}-{stamp}", slug(&name), slug(issue)));
    std::fs::create_dir_all(data.join("fix"))?;
    let path = root.to_string_lossy().into_owned();
    git.run(
        top,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &branch,
            &path,
            &repository.base,
        ],
    )?;
    let project = root.join(&repository.prefix);
    Ok(Worktree {
        branch,
        root,
        project,
        base: repository.base.clone(),
    })
}

/// The diff of the worktree against its base, new files included; textual
/// diff drivers and external diff programs are not run.
fn diff(git: &Git, worktree: &Worktree) -> Result<(String, Vec<String>)> {
    git.run(&worktree.root, &["add", "-A", "-N"])?;
    let text = git.run(
        &worktree.root,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            &worktree.base,
        ],
    )?;
    let names = git
        .run(
            &worktree.root,
            &["diff", "--name-only", "--no-ext-diff", &worktree.base],
        )?
        .lines()
        .map(str::to_string)
        .collect();
    Ok((text, names))
}

/// One round as the report shows it.
fn round_json(
    round: usize,
    reply: Option<&Value>,
    applied: &std::result::Result<Applied, Vec<String>>,
    test: Option<&TestRun>,
    usage: &Value,
) -> Value {
    let (applied_json, refused) = match applied {
        Ok(applied) => (
            json!({"changed":applied.changed,"created":applied.created}),
            Value::Null,
        ),
        Err(problems) => (Value::Null, json!(problems)),
    };
    let result = match (applied, test) {
        (Err(_), _) => "not applied",
        (Ok(_), None) => "untested",
        (Ok(_), Some(t)) if t.passed => "passed",
        (Ok(_), Some(t)) if t.timed_out => "timed out",
        (Ok(_), Some(_)) => "failed",
    };
    json!({"round":round,"result":result,"applied":applied_json,"refused":refused,
        "explanation":reply.and_then(|r| r["explanation"].as_str()),
        "test":test.map(|t| json!({"passed":t.passed,"exit_code":t.code,"seconds":t.seconds,
            "timed_out":t.timed_out,"output_tail":t.tail})),
        "usage":usage})
}

/// The one-line usage of a spend log entry.
fn entry_line(entry: &Value) -> String {
    crate::spend::line(entry)
}

/// `graphicle fix`.
pub async fn fix(run: Run<'_>, options: Options) -> Result<()> {
    let (name, profile) = long_context::profile(&run)?;
    let human = crate::terminal::human(run.json);
    let folder = run.folder.clone().ok_or_else(|| {
        Error::bad("fix works on a project's own folder, a git working tree: sync one first")
    })?;
    let issue = options.issue.trim().to_string();
    if issue.is_empty() {
        return Err(Error::bad("Give the issue's text, or --issue-file FILE"));
    }
    let git = Git::new(&run.store.directory)?;
    let repository = Repository::of(&git, &folder)?;
    let dirty = git
        .run(&folder, &["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let ratio = long_context::calibration(run.store, &name, &profile);
    let room = |budget: usize| {
        if run.budget.is_none() {
            budget
                .saturating_sub(ROUND_ROOM)
                .max(crate::code_pack::MIN_BUDGET)
        } else {
            budget
        }
    };
    let budget = room(long_context::budget_with(&profile, run.budget, ratio)?);
    let nominal = room(long_context::budget_for(&profile, run.budget)?);
    // The plan: the `plan` command's call and checks, on a cache-aware pack.
    let plan_schema = long_context::plan_schema();
    let plan_user = |pack: &Value| {
        format!(
            "{}\nChange request: {issue}\n",
            crate::code_pack::text(pack)
        )
    };
    let plan_request = Request {
        question: &issue,
        rest: &issue,
        instructions: long_context::PLAN_INSTRUCTIONS,
        user_of: &plan_user,
        schema: Some(&plan_schema),
    };
    // The pack of the issue, with the --focus passages first: the same
    // arguments as every long-context pack (`long_context::pack_args`), and
    // `focus` only when there are passages.
    let focus = focus_json(&options.focus);
    let repack = |budget: usize| -> Result<Value> {
        let mut args = long_context::pack_args(&run, &profile, &issue, budget, Some(nominal));
        if !focus.is_null() {
            args["focus"] = focus.clone();
        }
        run.store
            .project_pack(&run.session.workspace, &run.session.capability, &args)
    };
    let (mut pack, mut budget, counted) = long_context::fit_with(
        &profile,
        &plan_request,
        repack(budget)?,
        budget,
        ratio,
        human,
        &repack,
    )
    .await?;
    if human {
        long_context::tell_packed(&pack, &name, &profile);
    }
    let (result, plan_entry) = long_context::send(
        &run,
        "fix",
        &name,
        &profile,
        &pack,
        &plan_request,
        counted,
        ratio,
        &mut |_| {},
    )
    .await?;
    let mut plan = long_context::json_in(&result?.text)?;
    if !plan.is_object() {
        return Err(Error::bad("The model's plan is not a JSON object"));
    }
    let mut tree = Tree::new(Some(folder.clone()));
    let plan_problems = long_context::check_plan(&mut plan, &mut tree);
    if human {
        eprintln!(
            "graphicle: planned ({}); {}",
            entry_line(&plan_entry),
            printable(plan["summary"].as_str().unwrap_or("no summary"))
        );
    }
    // The branch and its worktree, now that there is a plan.
    let worktree = new_worktree(
        &git,
        &repository,
        &run.store.directory,
        options.branch.as_deref(),
        &issue,
    )?;
    if human {
        eprintln!(
            "graphicle: made the branch {} at {} from HEAD {}{}",
            worktree.branch,
            worktree.root.display(),
            &worktree.base[..12.min(worktree.base.len())],
            if dirty {
                " (your uncommitted changes are not in it)"
            } else {
                ""
            }
        );
    }
    let named = |error: Error| {
        Error::bad(format!(
            "{}; the branch {} and its worktree {} remain",
            error.message,
            worktree.branch,
            worktree.root.display()
        ))
    };
    // The rounds: the same instructions, pack, issue and plan each time,
    // then what the last round left, each request fitted to the input limit.
    let schema = edits_schema();
    let plan_json = serde_json::to_string(&plan)?;
    let scrub = Scrub::of(&crate::settings::configuration(
        run.store,
        &run.session.workspace,
        run.config,
    )?);
    let mut follow_up = String::new();
    let mut rounds: Vec<Value> = Vec::new();
    let mut suggested: Vec<String> = Vec::new();
    let mut explanations = String::new();
    let mut outcome = "not applied";
    let mut stopped: Option<String> = None;
    for round in 1..=options.rounds {
        let rest = format!(
            "Issue: {issue}\nPlan: {plan_json}\n{follow_up}Round {round}: return the edits.\n"
        );
        let user_of = |pack: &Value| format!("{}\n{rest}", crate::code_pack::text(pack));
        let round_request = Request {
            question: &issue,
            rest: &rest,
            instructions: FIX_INSTRUCTIONS,
            user_of: &user_of,
            schema: Some(&schema),
        };
        let fitted = long_context::fit_with(
            &profile,
            &round_request,
            pack.clone(),
            budget,
            ratio,
            human,
            &repack,
        )
        .await;
        let counted = match fitted {
            Ok((fitted, fitted_budget, counted)) => {
                pack = fitted;
                budget = fitted_budget;
                counted
            }
            Err(error) => {
                rounds.push(json!({"round":round,"result":"does not fit","error":error.message}));
                outcome = "does not fit";
                stopped = Some(error.message);
                break;
            }
        };
        let sent = long_context::send(
            &run,
            "fix",
            &name,
            &profile,
            &pack,
            &round_request,
            counted,
            ratio,
            &mut |_| {},
        )
        .await;
        let (result, entry) = match sent {
            Ok(sent) => sent,
            Err(error) => {
                rounds.push(
                    json!({"round":round,"result":"refused by a cap","refused":[error.message]}),
                );
                outcome = "refused by a cap";
                stopped = Some(error.message);
                break;
            }
        };
        let done = match result {
            Ok(done) => done,
            Err(error) => {
                rounds.push(json!({"round":round,"result":"provider error","error":error.message,"usage":entry}));
                outcome = "provider error";
                stopped = Some(error.message);
                break;
            }
        };
        let reply = match long_context::json_in(&done.text) {
            Ok(reply) if reply.is_object() => reply,
            Ok(_) | Err(_) => {
                let applied: std::result::Result<Applied, Vec<String>> =
                    Err(vec!["the model's reply is not the edits' JSON".to_string()]);
                rounds.push(round_json(round, None, &applied, None, &entry));
                follow_up = format!(
                    "Round {round} failed: the reply was not one JSON object that matches the schema.\n"
                );
                continue;
            }
        };
        for test in reply["tests"].as_array().into_iter().flatten() {
            if let Some(test) = test.as_str()
                && !suggested.iter().any(|s| s == test)
            {
                suggested.push(test.to_string());
            }
        }
        if let Some(text) = reply["explanation"].as_str() {
            explanations.push_str(text);
            explanations.push('\n');
        }
        let applied = apply(&worktree.project, &reply);
        let test = match (&applied, &options.test) {
            (Ok(_), Some(command)) => Some(run_test(
                &worktree.project,
                command,
                options.timeout,
                &scrub,
            )),
            _ => None,
        };
        rounds.push(round_json(
            round,
            Some(&reply),
            &applied,
            test.as_ref(),
            &entry,
        ));
        if human {
            eprintln!(
                "graphicle: round {round}: {} ({})",
                rounds.last().unwrap()["result"].as_str().unwrap_or(""),
                entry_line(&entry)
            );
        }
        match (&applied, &test) {
            (Err(problems), _) => {
                outcome = if outcome == "failed" {
                    "failed"
                } else {
                    "not applied"
                };
                follow_up = format!(
                    "Round {round}: no edit was applied, because:\n{}\n",
                    problems.join("\n")
                );
            }
            (Ok(_), None) => {
                outcome = "untested";
                break;
            }
            (Ok(_), Some(t)) if t.passed => {
                outcome = "passed";
                break;
            }
            (Ok(_), Some(t)) => {
                outcome = "failed";
                let (diff_text, _) = diff(&git, &worktree).map_err(named)?;
                follow_up = format!(
                    "Round {round}: the edits were applied, and the test command {} ({}). The end of its output:\n```\n{}\n```\nThe diff so far:\n```diff\n{}\n```\n",
                    if t.timed_out { "timed out" } else { "failed" },
                    t.code
                        .map_or("no exit code".to_string(), |c| format!("exit code {c}")),
                    t.tail,
                    tail(&diff_text, DIFF_CAP)
                );
            }
        }
    }
    let (_, files) = diff(&git, &worktree).map_err(named)?;
    let mut committed = None;
    if options.commit && !files.is_empty() {
        let title: String = issue
            .lines()
            .next()
            .unwrap_or("Fix")
            .chars()
            .take(72)
            .collect();
        let tests = match (outcome, &options.test) {
            ("passed", Some(command)) => format!("Tests pass: {command}"),
            (_, Some(command)) => format!("Tests do not pass yet: {command}"),
            (_, None) => "Not tested.".to_string(),
        };
        // Hooks stay off: the worktree's own hooks may be files the model
        // edited.
        git.run(&worktree.root, &["add", "-A"]).map_err(named)?;
        git.run(
            &worktree.root,
            &["commit", "-q", "--no-verify", "-m", &title, "-m", &tests],
        )
        .map_err(named)?;
        committed = Some(
            git.run(&worktree.root, &["rev-parse", "HEAD"])
                .map_err(named)?
                .trim()
                .to_string(),
        );
    }
    // Citations of the plan and the explanations, checked against the tree.
    let mut cited: Vec<Value> = Vec::new();
    let mut text = explanations.clone();
    for edit in plan["edits"].as_array().into_iter().flatten() {
        text.push_str(&format!(
            "{}:{}-{}\n",
            edit["path"].as_str().unwrap_or(""),
            edit["start_line"],
            edit["end_line"]
        ));
    }
    for c in long_context::citations(&text) {
        let problem = tree.problem(&c.path, c.line, c.end);
        cited.push(json!({"path":c.path,"line":c.line,"end_line":c.end,"valid":problem.is_none(),"problem":problem}));
    }
    let cost: f64 = std::iter::once(&plan_entry)
        .chain(rounds.iter().map(|r| &r["usage"]))
        .filter_map(|u| u["cost_usd"].as_f64())
        .sum();
    let mut report = json!({"branch":worktree.branch,"worktree":worktree.root,"base":worktree.base,
        "result":outcome,"tested":options.test.is_some(),"test_command":options.test,
        "plan":plan,"plan_problems":plan_problems,"plan_usage":plan_entry,"rounds":rounds,
        "files_changed":files,"citations":cited,"suggested_tests":suggested,
        "committed":committed,"cost_usd":cost,"model":profile.model,"profile":name,
        "prompt_cache_key":model::prompt_cache_on(&profile).then(|| long_context::cache_key(&pack)),
        "pack":crate::code_pack::summary(&pack)});
    if !focus.is_null() {
        report["focus"] = focus.clone();
    }
    if !human {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", printable(&report_text(&report)));
    }
    match outcome {
        "passed" | "untested" => Ok(()),
        other => Err(Error::bad(format!(
            "fix ended with the result {other:?}{}; the changes are in {}",
            stopped.map_or(String::new(), |why| format!(" ({})", printable(&why))),
            worktree.root.display()
        ))),
    }
}

/// The report as a person reads it.
fn report_text(report: &Value) -> String {
    let mut out = format!(
        "Branch {} in {} (from {}).\n",
        report["branch"].as_str().unwrap_or(""),
        report["worktree"].as_str().unwrap_or(""),
        report["base"]
            .as_str()
            .unwrap_or("")
            .get(..12)
            .unwrap_or("")
    );
    if let Some(summary) = report["plan"]["summary"].as_str() {
        out.push_str(&format!("Plan: {summary}\n"));
    }
    out.push_str(&format!(
        "Plan call: {}\n",
        entry_line(&report["plan_usage"])
    ));
    for round in report["rounds"].as_array().into_iter().flatten() {
        let mut line = format!(
            "Round {}: {}",
            round["round"],
            round["result"].as_str().unwrap_or("")
        );
        if let Some(test) = round["test"].as_object() {
            line.push_str(&format!(
                " ({} s{})",
                test["seconds"],
                test["exit_code"]
                    .as_i64()
                    .map_or(String::new(), |c| format!(", exit code {c}"))
            ));
        }
        if let Some(error) = round["error"].as_str() {
            line.push_str(&format!("\n  {error}"));
        }
        if let Some(refused) = round["refused"].as_array() {
            for problem in refused {
                line.push_str(&format!("\n  refused: {}", problem.as_str().unwrap_or("")));
            }
        }
        if round["usage"].is_object() {
            line.push_str(&format!("\n  {}", entry_line(&round["usage"])));
        }
        out.push_str(&line);
        out.push('\n');
    }
    let files: Vec<&str> = report["files_changed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    out.push_str(&format!(
        "Files changed: {}\n",
        if files.is_empty() {
            "none".to_string()
        } else {
            files.join(", ")
        }
    ));
    let cited: Vec<String> = report["citations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let place = if c["line"] == c["end_line"] {
                format!("{}:{}", c["path"].as_str().unwrap_or(""), c["line"])
            } else {
                format!(
                    "{}:{}-{}",
                    c["path"].as_str().unwrap_or(""),
                    c["line"],
                    c["end_line"]
                )
            };
            if c["valid"] == true {
                place
            } else {
                format!("{place} (does not hold)")
            }
        })
        .collect();
    if !cited.is_empty() {
        out.push_str(&format!("Cited: {}\n", cited.join(", ")));
    }
    let suggested: Vec<&str> = report["suggested_tests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !suggested.is_empty() {
        out.push_str(&format!(
            "Tests the model suggests (not run): {}\n",
            suggested.join("; ")
        ));
    }
    if report["tested"] != true {
        out.push_str(
            "Untested: give a test command with --test, or set fix.test in the configuration.\n",
        );
    }
    out.push_str(&format!(
        "Cost: {} at the configured price.\n",
        crate::spend::usd(report["cost_usd"].as_f64().unwrap_or(0.0))
    ));
    match report["committed"].as_str() {
        Some(sha) => out.push_str(&format!(
            "Committed {} on {}. Nothing was pushed.\n",
            &sha[..12.min(sha.len())],
            report["branch"].as_str().unwrap_or("")
        )),
        None => out.push_str(&format!(
            "The changes are in the worktree, not committed: `git -C {} diff`, or run fix with --commit.\n",
            report["worktree"].as_str().unwrap_or("")
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_names_a_file_a_line_or_a_range() {
        assert_eq!(
            parse_focus("src/a.ts"),
            Ok(("src/a.ts".into(), 1, WHOLE_FILE))
        );
        assert_eq!(
            parse_focus("src/a.ts:100"),
            Ok(("src/a.ts".into(), 60, 140))
        );
        assert_eq!(parse_focus("src/a.ts:3"), Ok(("src/a.ts".into(), 1, 43)));
        assert_eq!(
            parse_focus(" src/a.ts:20-10 "),
            Ok(("src/a.ts".into(), 10, 20))
        );
        assert!(parse_focus("src/a.ts:0").is_err());
        assert!(parse_focus("src/a.ts:5-").is_err());
        assert!(parse_focus(":5").is_err());
        // A colon that is not a line range is part of the path.
        assert_eq!(
            parse_focus("src/a:b.ts"),
            Ok(("src/a:b.ts".into(), 1, WHOLE_FILE))
        );
        assert_eq!(focus_json(&[]), Value::Null);
    }

    #[test]
    fn paths_that_leave_the_repository_or_reach_git_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/a.go"), "package a\n").unwrap();
        assert!(safe_path(&root, "src/a.go").is_ok());
        assert!(safe_path(&root, "./src/new/b.go").is_ok());
        assert!(safe_path(&root, "src//a.go").is_ok());
        for bad in [
            "",
            ".",
            "/etc/passwd",
            "../outside.txt",
            "src/../../outside.txt",
            ".git/config",
            ".GIT/config",
            ".Git/hooks/pre-commit",
            ".git./config",
            ".git /config",
            "GIT~1/config",
            "src/.git/hooks/pre-commit",
            "C:\\x",
            "src\\a.go",
            "a:b",
        ] {
            assert!(safe_path(&root, bad).is_err(), "{bad}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), root.join("out")).unwrap();
            std::os::unix::fs::symlink(root.join("src/a.go"), root.join("inside")).unwrap();
            std::os::unix::fs::symlink(root.join(".git"), root.join("g")).unwrap();
            assert!(safe_path(&root, "out/x.txt").is_err());
            assert!(safe_path(&root, "inside").is_ok());
            assert!(safe_path(&root, "g/config").is_err());
            assert!(safe_path(&root, "g").is_err());
        }
    }

    #[test]
    fn edits_apply_all_or_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.go"), "func A() int {\n\treturn 41\n}\n").unwrap();
        let ok = json!({"edits":[{"path":"a.go","search":"\treturn 41\n","replace":"\treturn 42\n"}],
            "new_files":[{"path":"b/b.go","content":"package b\n"}]});
        assert_eq!(
            apply(root, &ok).unwrap(),
            Applied {
                changed: vec!["a.go".into()],
                created: vec!["b/b.go".into()]
            }
        );
        assert!(
            std::fs::read_to_string(root.join("a.go"))
                .unwrap()
                .contains("return 42")
        );
        // One bad item: nothing is written.
        let mixed = json!({"edits":[{"path":"a.go","search":"return 42","replace":"return 43"},
            {"path":"../x.go","search":"a","replace":"b"}]});
        let problems = apply(root, &mixed).unwrap_err();
        assert!(problems[0].starts_with("../x.go: refused"), "{problems:?}");
        assert!(
            std::fs::read_to_string(root.join("a.go"))
                .unwrap()
                .contains("return 42")
        );
        // The pack's line numbers copied into a search text still match.
        let numbered = json!({"edits":[{"path":"a.go","search":"2\t\treturn 42\n","replace":"2\t\treturn 43\n"}]});
        apply(root, &numbered).unwrap();
        assert!(
            std::fs::read_to_string(root.join("a.go"))
                .unwrap()
                .contains("\treturn 43\n")
        );
        // Missing and repeated search texts, and new files that exist.
        for (bad, why) in [
            (
                json!({"edits":[{"path":"a.go","search":"return 99","replace":"x"}]}),
                "not in the file",
            ),
            (
                json!({"edits":[{"path":"a.go","search":"\n","replace":"x"}]}),
                "occurs",
            ),
            (
                json!({"new_files":[{"path":"a.go","content":"x"}]}),
                "exists already",
            ),
            (
                json!({"edits":[{"path":"c.go","search":"a","replace":"b"}]}),
                "no such file",
            ),
        ] {
            let problems = apply(root, &bad).unwrap_err();
            assert!(problems[0].contains(why), "{problems:?}");
        }
    }

    /// New files `a` and `a/b`, or a new file under an existing file: the
    /// round is refused before anything is written.
    #[test]
    fn new_files_that_need_each_other_as_folders_are_refused_whole() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("keep.txt"), "x\n").unwrap();
        let reply = json!({"edits":[],"new_files":[
            {"path":"a","content":"first\n"},{"path":"a/b","content":"second\n"}]});
        let problems = apply(root, &reply).unwrap_err();
        assert!(
            problems[0].contains("its folder a is a new file of this round"),
            "{problems:?}"
        );
        assert!(!root.join("a").exists());
        let reply = json!({"new_files":[{"path":"keep.txt/inner","content":"x"}]});
        let problems = apply(root, &reply).unwrap_err();
        assert!(
            problems[0].contains("is a file in the repository"),
            "{problems:?}"
        );
    }

    /// A write that fails puts back what the round wrote before it.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_puts_the_round_back() {
        use std::os::unix::fs::PermissionsExt;
        // Root writes anyway: the check needs an ordinary user.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        std::fs::set_permissions(root.join("b.txt"), std::fs::Permissions::from_mode(0o444))
            .unwrap();
        let reply = json!({"edits":[
            {"path":"a.txt","search":"one","replace":"ONE"},
            {"path":"b.txt","search":"two","replace":"TWO"}],
            "new_files":[{"path":"new/c.txt","content":"three\n"}]});
        let problems = apply(root, &reply).unwrap_err();
        assert!(problems[0].contains("were put back"), "{problems:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "two\n"
        );
        assert!(!root.join("new").exists());
    }

    /// Two spellings of one file are one file: both edits are made.
    #[test]
    fn spellings_of_one_path_are_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();
        let reply = json!({"edits":[
            {"path":"src/lib.rs","search":"one","replace":"ONE"},
            {"path":"src//lib.rs","search":"two","replace":"TWO"},
            {"path":"./src/./lib.rs","search":"three","replace":"THREE"}],"new_files":[]});
        let applied = apply(&root, &reply).unwrap();
        assert_eq!(applied.changed, vec!["src/lib.rs".to_string()]);
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "ONE\nTWO\nTHREE\n"
        );
        // Where the file system ignores case, another case is the same file.
        if case_insensitive(&root) {
            let reply = json!({"edits":[
                {"path":"src/lib.rs","search":"ONE","replace":"one"},
                {"path":"SRC/LIB.rs","search":"TWO","replace":"two"}]});
            apply(&root, &reply).unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
                "one\ntwo\nTHREE\n"
            );
        }
    }

    #[test]
    fn tests_run_with_a_timeout_and_keep_the_end_of_their_output() {
        let dir = tempfile::tempdir().unwrap();
        let none = Scrub::default();
        let passed = run_test(dir.path(), "echo fine", Duration::from_secs(30), &none);
        assert!(passed.passed && passed.code == Some(0) && passed.tail.contains("fine"));
        let failed = run_test(
            dir.path(),
            "echo broken >&2; exit 3",
            Duration::from_secs(30),
            &none,
        );
        assert!(!failed.passed && failed.code == Some(3) && failed.tail.contains("broken"));
        #[cfg(unix)]
        {
            let slow = run_test(dir.path(), "sleep 30", Duration::from_millis(300), &none);
            assert!(slow.timed_out && !slow.passed && slow.seconds < 20.0);
            // A background child holding the output does not make the run
            // time out: it ends with the command.
            let started = Instant::now();
            let background = run_test(
                dir.path(),
                "sleep 20 & echo started",
                Duration::from_secs(10),
                &none,
            );
            assert!(background.passed && !background.timed_out, "{background:?}");
            assert!(background.tail.contains("started"));
            assert!(started.elapsed() < Duration::from_secs(8));
            // Output past what is kept: the end stays, the memory bounded.
            let loud = run_test(
                dir.path(),
                "i=0; while [ $i -lt 4000 ]; do echo line $i of a long run of output; i=$((i+1)); done",
                Duration::from_secs(60),
                &none,
            );
            assert!(loud.tail.contains("line 3999"));
            assert!(loud.tail.len() <= OUTPUT_TAIL + 8);
        }
        #[cfg(target_os = "linux")]
        {
            // A child that leaves the process group: the output read is kept.
            let started = Instant::now();
            let escaped = run_test(
                dir.path(),
                "setsid sleep 25 & echo started",
                Duration::from_secs(10),
                &none,
            );
            assert!(
                escaped.passed && escaped.tail.contains("started"),
                "{escaped:?}"
            );
            assert!(started.elapsed() < Duration::from_secs(8));
        }
        assert_eq!(
            slug("Fix: the StatefulSet ordinal 01 bug!"),
            "fix-the-statefulset-ordinal-01-bug"
        );
        assert_eq!(slug("***"), "issue");
        assert_eq!(tail("abcdef", 3), "...\ndef");
    }

    /// The test command runs without the model key variables, and their
    /// values are hidden in its output.
    #[cfg(unix)]
    #[test]
    fn the_test_command_gets_no_model_keys() {
        let dir = tempfile::tempdir().unwrap();
        let scrub = Scrub {
            remove: vec!["HOME".into()],
            secrets: vec!["s3cr3t-value-0123456789".into()],
        };
        let run = run_test(
            dir.path(),
            "echo \"home=[$HOME]\"; echo leaked s3cr3t-value-0123456789",
            Duration::from_secs(30),
            &scrub,
        );
        assert!(run.tail.contains("home=[]"), "{}", run.tail);
        assert!(run.tail.contains("leaked [redacted]"), "{}", run.tail);
        assert!(!run.tail.contains("s3cr3t"));
        let config: model::Config = serde_json::from_value(json!({"models":{"m":{
            "protocol":"responses","base_url":"https://api.meta.ai/v1","model":"m",
            "key_env":"MY_FIX_KEY","input_limit":4000,"output_limit":1000}}}))
        .unwrap();
        let scrub = Scrub::of(&config);
        assert!(scrub.remove.contains(&"MY_FIX_KEY".to_string()));
        assert!(scrub.remove.contains(&"MODEL_API_KEY".to_string()));
    }

    #[test]
    fn model_text_is_printed_without_terminal_controls() {
        let text =
            "Plan\u{1b}]0;owned\u{7}: fix\u{1b}[2J\u{1b}[31m it\u{1b}[0m\u{7}\r\n\tdone\u{9b}";
        assert_eq!(printable(text), "Plan: fix it\n\tdone");
        let shown = printable(&report_text(
            &json!({"plan":{"summary":"x\u{1b}[2Jy"},"rounds":[],
            "suggested_tests":["make \u{1b}]0;t\u{7}test"]}),
        ));
        assert!(!shown.contains('\u{1b}'));
        assert!(
            shown.contains("Plan: xy") && shown.contains("make test"),
            "{shown}"
        );
        assert_eq!(printable("plain text\n"), "plain text\n");
    }

    #[test]
    fn git_runs_without_redirecting_variables() {
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
        ] {
            assert!(redirects_git(name), "{name}");
        }
        // The user's configuration files, which hold the commit identity.
        for name in [
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_AUTHOR_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_SSH_COMMAND",
            "PATH",
        ] {
            assert!(!redirects_git(name), "{name}");
        }
    }
}
