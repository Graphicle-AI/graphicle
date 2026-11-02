//! `graphicle fix`: a change for an issue, made and tested on a new branch.
//!
//! One cache-aware pack focused on the issue ([`crate::code_pack`]) and a
//! plan (the `plan` command's validated JSON), then at most `--rounds` rounds
//! of edits. Each round the model returns search-and-replace blocks per file,
//! whole new files and a short explanation as structured output. They are
//! checked as a whole and applied, all or none, in a new git worktree on a new
//! branch from HEAD (never the person's working tree or branch), made once
//! the plan has come back, and the test command runs there with a timeout. A
//! failure's output (when it is long, its lines that report failures and its
//! end: [`digest`]), the diff so far, and the files that the failure and the
//! model name but the pack does not hold whole ([`next_context`]) go back to
//! the model as the next round, whose start (the instructions, the pack, the
//! issue and the plan) the provider's prompt cache can still hold. Every
//! round's request is fitted to the model's input limit.
//!
//! The test command comes from `--test` or the configuration's `fix.test`,
//! never from the model: its test suggestions are shown, not run. It runs
//! model-edited code with the person's privileges, without the variables
//! that hold model keys. Edits may only touch files inside the repository:
//! absolute paths, `..`, any spelling of `.git` and symlinks that lead
//! outside or into `.git` are refused. Every git command runs with hooks
//! turned off and without the variables that would point git at another
//! repository. Nothing is fetched or pushed.

use crate::code_pack::Counter;
use crate::long_context::{self, Request, Run, Tree};
use crate::{Error, Result, model};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// What `fix` tells the model in every round: the same words each time, so
/// that the provider's prompt cache matches them and the pack after them.
pub const FIX_INSTRUCTIONS: &str = "You fix an issue in a code repository from a pack of its code. The pack starts with a header. It holds an architecture summary, a manifest of the files it holds, and the files themselves, each headed by its path, line range and why it is included; a pack in the cache layout has a repository part first and a question part after it. Code lines that start with a number and a tab carry their line number, which is not part of the file. After the pack come the issue and a plan; after a round that failed, the test output (when it is long, its lines that report failures, then its end), the diff so far, and the files that the output, the round's edits or your explanation named, by path or by a symbol they define, when the pack does not hold them whole. Return one JSON object that matches the schema: the edits, each a file's path, a search text copied exactly from the file as it is now (without the line numbers the pack adds, and long enough to occur once in the file) and its replacement; new files with their whole content; a short explanation that cites the code as path:line; and the tests you would run or add. To see a file the pack does not show, name its path, or a symbol it defines, in the explanation: if the round fails, the next round adds that file. Change only what the issue needs, keep the code's style, and use paths exactly as the pack shows them.";

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
/// What a round shows of a test's output, at most: all of it, or its lines
/// that report failures and its end ([`digest`]).
const OUTPUT_TAIL: usize = 6_000;
/// The bytes of [`OUTPUT_TAIL`] that the lines reporting failures, from the
/// part of a long output that is cut, may take at most.
const FAILURE_SHARE: usize = 2_400;
/// A line that reports a failure is cut to this many bytes.
const FAILURE_LINE: usize = 400;
/// The lines reporting failures that each output stream keeps from the part
/// it drops ([`OUTPUT_KEEP`]), at most; a line longer than
/// [`FAILURE_KEEP_LINE`] bytes is not kept.
const FAILURE_KEEP: usize = 64;
const FAILURE_KEEP_LINE: usize = 8 << 10;
/// How far before its cut an output stream looks for a line's end, so that
/// it drops whole lines.
const LINE_SLACK: usize = 8 << 10;
/// The end of each output stream held while a test runs: a stream holds up
/// to twice this, then drops all but about this much of its end ([`Kept`]).
const OUTPUT_KEEP: usize = 64 << 10;
/// How long a test's output is still read after the command ends.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// The diff so far that a follow-up sends at most.
const DIFF_CAP: usize = 24_000;
/// The files a failed round adds to the next round's request at most, and
/// their tokens at most, counted as the pack counts ([`next_context`]).
const CONTEXT_FILES: usize = 6;
const CONTEXT_TOKENS: usize = 24_000;
/// One added file's tokens at most: a larger file goes in as the lines
/// around the line named in it, when one is named.
const CONTEXT_FILE_TOKENS: usize = 12_000;
/// The lines before and after the line named that such an excerpt holds
/// (half as many when that is still too large).
const EXCERPT_BEFORE: usize = 30;
const EXCERPT_AFTER: usize = 170;
/// A file larger than this is not read for a round.
const CONTEXT_READ_BYTES: u64 = 1 << 20;
/// The code names of an explanation looked up in the code graph, at most.
const CONTEXT_NAMES: usize = 24;
/// A name that more files than this define is too common to add them.
const CONTEXT_DEFINITIONS: usize = 3;
/// The entries of a round's `context.left_out`, at most.
const LEFT_OUT: usize = 20;
/// The paths read from one text, at most.
const PATHS_READ: usize = 400;
/// Tokens a pack without `--budget` leaves free, beyond the room `ask
/// --pack` keeps, for what the rounds add after it: the plan, the test
/// output's end and the diff so far; the files a failed round adds take
/// only what is left ([`files_room`]). Each round is still fitted to the
/// input limit.
const ROUND_ROOM: usize = 12_000;
/// What the files a failed round adds come after in the next request.
const FILES_HEADING: &str = "Files for this round that the pack does not hold whole, which the last round's output or edits or your explanation named (by path, or by a symbol they define), as the worktree has them now:\n\n";
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
    /// What a round shows of its output (standard output, then standard
    /// error): all of it, or its lines that report failures and its end
    /// ([`digest`]).
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

/// `text` cut to at most `max` bytes at a character boundary, ending in `…`
/// when cut.
fn shortened(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// A line of a test's output that reports a failure, in the usual runners'
/// words: FAIL, FAILED, FAILS or FAILURES; a failed test's mark (×, ✗, ✕,
/// ✘, ❌, ●); an error or exception with its message (`AssertionError`,
/// `TypeError:`, `Error:`); a panic or a traceback; a compiler's `error:`,
/// `error[E0425]:` or `error TS2304:` and where it is (`--> src/lib.rs:3:5`);
/// a count of failures (`2 failed`, `1 failing`, `3 errors`); an assertion
/// and its `expected`, `received`, `left` and `right`; pytest's `E` and `>`
/// lines; and a Go test's `x_test.go:12:` message.
static FAILURE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r"\bFAIL(?:ED|S|URES?)?\b",
        r"|[×✗✕✘❌●]",
        r"|\b[A-Za-z]*(?:Error|Exception)\b:|\bAssertionError\b",
        r"|\bpanic(?:ked)?\b|\bTraceback\b",
        r"|\berror(?:\[E\d+\]| TS\d+)?:|^\s*--> ",
        r"|\b[1-9]\d* (?:failed|failing|failures?|errors?)\b",
        r"|(?i:\bassert(?:ion)?\b|\bexpected\b|\breceived\b)",
        r"|^\s*(?:left|right):|^E\s|^>\s",
        r"|_test\.go:\d+:",
    ))
    .expect("the failure rule")
});

/// Whether `line` (without the terminal's escape sequences) reports a
/// failure ([`FAILURE`]).
fn reports_failure(line: &str) -> bool {
    FAILURE.is_match(line)
}

/// A test's output as a round shows it: the secrets hidden, the terminal's
/// escape sequences and other control characters but newlines and tabs
/// removed ([`printable`]), then the secrets hidden again, in case a colour
/// code split one.
fn clean(scrub: &Scrub, text: &str) -> String {
    scrub.redact(&printable(&scrub.redact(text)))
}

/// What a round shows of a test's output (`text`, cleaned) in `cap` bytes:
/// all of it when it fits. Else the lines that report failures
/// ([`reports_failure`]) from the part that is cut, after `early` (those of
/// what the output streams dropped while the test ran, cleaned): each once,
/// cut to [`FAILURE_LINE`] bytes, at most [`FAILURE_SHARE`] bytes of them;
/// then a marker and the output's end, all within `cap`. With no such line,
/// the end alone after `...`, as before.
fn digest(text: &str, early: &[String], cap: usize) -> String {
    const HEAD: &str = "[the lines that report failures, from the part of the output cut below]\n";
    const CUT: &str = "[...]\n";
    if text.len() <= cap && early.is_empty() {
        return text.to_string();
    }
    // The end shown at least: the lines in it are not repeated above it.
    let least = cap.saturating_sub(HEAD.len() + FAILURE_SHARE + CUT.len() + 64);
    let mut from = text.len().saturating_sub(least);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut lines: Vec<String> = Vec::new();
    let (mut used, mut more) = (0, 0);
    for line in early.iter().map(String::as_str).chain(text[..from].lines()) {
        let line = line.trim();
        if line.is_empty() || !reports_failure(line) || !seen.insert(line) {
            continue;
        }
        let shown = shortened(line, FAILURE_LINE);
        if used + shown.len() + 1 > FAILURE_SHARE {
            more += 1;
            continue;
        }
        used += shown.len() + 1;
        lines.push(shown);
    }
    if lines.is_empty() {
        return tail(text, cap);
    }
    let mut out = String::from(HEAD);
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    if more > 0 {
        out.push_str(&format!("[{more} more such lines left out]\n"));
    }
    out.push_str(CUT);
    let room = cap.saturating_sub(out.len());
    let mut start = text.len().saturating_sub(room);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    out.push_str(&text[start..]);
    out
}

/// The end of an output stream, and the lines that report failures in what
/// it dropped: it holds up to twice [`OUTPUT_KEEP`] bytes, then drops all
/// but about the last [`OUTPUT_KEEP`].
#[derive(Default)]
struct Kept {
    bytes: Vec<u8>,
    cut: bool,
    /// The lines that report failures in the part dropped, as read (their
    /// secrets are hidden later), each once, at most [`FAILURE_KEEP`].
    failures: Vec<String>,
}

impl Kept {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > 2 * OUTPUT_KEEP {
            let mut drop = self.bytes.len() - OUTPUT_KEEP;
            // Whole lines go: up to a line's end shortly before the cut, else
            // shortly after it, so that what is kept starts at a line's start
            // (and a key value in a line is not cut in two there). Only a
            // line longer than twice [`LINE_SLACK`] can be cut.
            let from = drop.saturating_sub(LINE_SLACK);
            let to = (drop + LINE_SLACK).min(self.bytes.len());
            if let Some(end) = self.bytes[from..drop].iter().rposition(|&b| b == b'\n') {
                drop = from + end + 1;
            } else if let Some(end) = self.bytes[drop..to].iter().position(|&b| b == b'\n') {
                drop += end + 1;
            }
            self.note_failures(drop);
            self.bytes.drain(..drop);
            self.cut = true;
        }
    }

    /// Keep the lines that report failures among the first `end` bytes.
    fn note_failures(&mut self, end: usize) {
        for line in self.bytes[..end].split(|&b| b == b'\n') {
            if self.failures.len() >= FAILURE_KEEP {
                return;
            }
            if line.is_empty() || line.len() > FAILURE_KEEP_LINE {
                continue;
            }
            let line = String::from_utf8_lossy(line);
            if reports_failure(&printable(&line))
                && !self.failures.iter().any(|kept| kept.as_str() == line)
            {
                self.failures.push(line.into_owned());
            }
        }
    }

    /// All that the stream holds, so that a line that reports a failure in
    /// a part never dropped is read too ([`digest`] keeps such lines from
    /// the part it cuts): an output of one to two times [`OUTPUT_KEEP`]
    /// keeps the failure it printed first.
    fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.cut {
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
    let (text, early) = {
        let out = out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let err = err
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let early: Vec<String> = out
            .failures
            .iter()
            .chain(&err.failures)
            .map(|line| clean(scrub, line))
            .collect();
        (
            clean(scrub, &format!("{}{}", out.text(), err.text())),
            early,
        )
    };
    TestRun {
        passed: status.is_some_and(|s| s.success()) && !timed_out,
        code: status.and_then(|s| s.code()),
        seconds,
        timed_out,
        tail: digest(&text, &early, OUTPUT_TAIL),
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

/// A folder in `parent` that nothing uses yet: `stem`, else `stem-2`, `stem-3`
/// and so on, as for the branch. The stem's time stamp is in seconds, and two
/// runs on one issue can start within the same second.
fn free_folder(parent: &Path, stem: &str) -> Result<PathBuf> {
    (1..100)
        .map(|n| {
            if n == 1 {
                parent.join(stem)
            } else {
                parent.join(format!("{stem}-{n}"))
            }
        })
        .find(|folder| folder.symlink_metadata().is_err())
        .ok_or_else(|| Error::bad("No free worktree folder for this issue"))
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
    std::fs::create_dir_all(data.join("fix"))?;
    let root = free_folder(
        &data.join("fix"),
        &format!("{}-{}-{stamp}", slug(&name), slug(issue)),
    )?;
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

/// `round` with what its request added after the pack (`context`), when it
/// added or left out any file.
fn with_context(mut round: Value, context: Option<&Value>) -> Value {
    if let Some(context) = context {
        round["context"] = context.clone();
    }
    round
}

/// A path as a test's output or a model writes it (an optional leading
/// slash, folders, a file name with an extension), and the line of a
/// `path:LINE`.
static WRITTEN_PATH: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(/?(?:[A-Za-z0-9_.@+~-]+/)*[A-Za-z0-9_@+~-][A-Za-z0-9_.@+~-]*\.[A-Za-z][A-Za-z0-9]{0,9})\b(?::(\d+))?",
    )
    .expect("the path rule")
});

/// The project's files as its last sync holds them: the files a pack reads,
/// so none that the sync leaves out (ignored, secret or skipped files), by
/// path and by file name; and the worktree's project folder, which absolute
/// paths in a test's output start with.
struct ProjectFiles {
    paths: BTreeSet<String>,
    by_name: HashMap<String, Vec<String>>,
    roots: Vec<String>,
}

impl ProjectFiles {
    fn new(paths: BTreeSet<String>, roots: Vec<String>) -> Self {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for path in &paths {
            let name = path.rsplit('/').next().unwrap_or(path);
            by_name
                .entry(name.to_string())
                .or_default()
                .push(path.clone());
        }
        Self {
            paths,
            by_name,
            roots,
        }
    }

    /// The project's file that `written` names: the path as the project has
    /// it; an absolute path inside the worktree's project folder; or a path
    /// that is the end of exactly one of the project's paths, or ends with
    /// exactly one. Never a path with `..` or through a folder the sync
    /// skips (such as `node_modules`), nor an absolute path elsewhere.
    fn resolve(&self, written: &str) -> Option<String> {
        let written = written.replace('\\', "/");
        let relative = if written.starts_with('/') {
            self.roots.iter().find_map(|root| {
                written
                    .strip_prefix(root.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
            })?
        } else {
            written.trim_start_matches("./")
        };
        if self.paths.contains(relative) {
            return Some(relative.to_string());
        }
        if relative.is_empty()
            || relative
                .split('/')
                .any(|part| part == ".." || crate::local::SKIPPED_NAMES.contains(&part))
        {
            return None;
        }
        let name = relative.rsplit('/').next()?;
        let (shorter, longer) = (format!("/{relative}"), relative);
        let mut found = self
            .by_name
            .get(name)?
            .iter()
            .filter(|path| path.ends_with(&shorter) || longer.ends_with(&format!("/{path}")));
        match (found.next(), found.next()) {
            (Some(path), None) => Some(path.clone()),
            _ => None,
        }
    }
}

/// The project's files that `text` names ([`ProjectFiles::resolve`]), each
/// once, in order of mention, with the first line named in each.
fn paths_named(text: &str, files: &ProjectFiles) -> Vec<(String, Option<usize>)> {
    let mut found: Vec<(String, Option<usize>)> = Vec::new();
    for capture in WRITTEN_PATH.captures_iter(text).take(PATHS_READ) {
        let Some(path) = files.resolve(&capture[1]) else {
            continue;
        };
        let line = capture
            .get(2)
            .and_then(|m| m.as_str().parse::<usize>().ok())
            .filter(|&n| n >= 1);
        match found.iter_mut().find(|(known, _)| *known == path) {
            Some(known) => {
                if known.1.is_none() {
                    known.1 = line;
                }
            }
            None => found.push((path, line)),
        }
    }
    found
}

/// The code names `explanation` gives ([`crate::identifier_arm::named`]:
/// backticked, or written as code), less the folder and file names of the
/// paths it writes, in order, at most [`CONTEXT_NAMES`].
fn names_in(explanation: &str) -> Vec<String> {
    let mut words: HashSet<&str> = HashSet::new();
    for capture in WRITTEN_PATH.captures_iter(explanation).take(PATHS_READ) {
        let path = capture.get(1).map_or("", |m| m.as_str());
        for part in path.split('/') {
            words.insert(part);
            if let Some((stem, _)) = part.split_once('.') {
                words.insert(stem);
            }
        }
    }
    crate::identifier_arm::named(explanation)
        .names
        .into_iter()
        .filter(|name| !words.contains(name.as_str()))
        .take(CONTEXT_NAMES)
        .collect()
}

/// A file a failed round names for the next one: its path in the project,
/// the line named in it, and why.
#[derive(Clone, Debug, PartialEq)]
struct Wanted {
    path: String,
    line: Option<usize>,
    why: Vec<String>,
}

/// Note a file or a name the next round leaves out, and why, at most
/// [`LEFT_OUT`] of them.
fn leave(left_out: &mut Vec<Value>, key: &str, name: &str, why: impl Into<String>) {
    if left_out.len() < LEFT_OUT {
        left_out.push(json!({key: name, "why": why.into()}));
    }
}

/// The files failed round `round` names, in the order the next round takes
/// them: those its output names (`output`: the text and what it is, the
/// test's output or why the edits were refused), and those the model names
/// (the paths its explanation writes and its edits give, then the files
/// that define the code names its explanation gives, found by `defines`),
/// by turns; then the files the round itself had added (`last`). Each file
/// once, with every reason. A name that more than [`CONTEXT_DEFINITIONS`]
/// files define goes to `left_out`.
fn candidates(
    round: usize,
    output: Option<(&str, &str)>,
    reply: Option<&Value>,
    files: &ProjectFiles,
    defines: &mut dyn FnMut(&[String]) -> crate::identifier_arm::Definitions,
    last: &[String],
    left_out: &mut Vec<Value>,
) -> Vec<Wanted> {
    let wanted = |path: String, line: Option<usize>, why: String| Wanted {
        path,
        line,
        why: vec![why],
    };
    let mut by_output: Vec<Wanted> = Vec::new();
    if let Some((text, what)) = output {
        for (path, line) in paths_named(text, files) {
            by_output.push(wanted(path, line, format!("{what} names it")));
        }
    }
    let mut by_model: Vec<Wanted> = Vec::new();
    let explanation = reply
        .and_then(|reply| reply["explanation"].as_str())
        .unwrap_or("");
    for (path, line) in paths_named(explanation, files) {
        by_model.push(wanted(
            path,
            line,
            format!("round {round}'s explanation names it"),
        ));
    }
    for edit in reply
        .and_then(|reply| reply["edits"].as_array())
        .into_iter()
        .flatten()
    {
        if let Some(path) = edit["path"].as_str().and_then(|p| files.resolve(p)) {
            by_model.push(wanted(
                path,
                None,
                format!("an edit of round {round} names it"),
            ));
        }
    }
    let names = names_in(explanation);
    if !names.is_empty() {
        for (name, found) in names.iter().zip(defines(&names)) {
            if found.len() > CONTEXT_DEFINITIONS {
                leave(
                    left_out,
                    "symbol",
                    name,
                    format!("more than {CONTEXT_DEFINITIONS} files define it"),
                );
                continue;
            }
            for (path, line) in found {
                if files.paths.contains(&path) {
                    by_model.push(wanted(
                        path,
                        line,
                        format!("it defines `{name}`, which round {round}'s explanation names"),
                    ));
                }
            }
        }
    }
    let (mut a, mut b) = (by_output.into_iter(), by_model.into_iter());
    let mut order: Vec<Wanted> = Vec::new();
    loop {
        let (x, y) = (a.next(), b.next());
        if x.is_none() && y.is_none() {
            break;
        }
        order.extend(x);
        order.extend(y);
    }
    for path in last {
        order.push(wanted(
            path.clone(),
            None,
            format!("round {round} had it added"),
        ));
    }
    let mut merged: Vec<Wanted> = Vec::new();
    for one in order {
        match merged.iter_mut().find(|known| known.path == one.path) {
            Some(known) => {
                for why in one.why {
                    if !known.why.contains(&why) {
                        known.why.push(why);
                    }
                }
                if known.line.is_none() {
                    known.line = one.line;
                }
            }
            None => merged.push(one),
        }
    }
    merged
}

/// The lines a pack holds of each file: whether one of its chunks holds the
/// whole file, and its chunks' line ranges, merged.
struct Held(HashMap<String, (bool, Vec<(usize, usize)>)>);

impl Held {
    fn of(pack: &Value) -> Self {
        let mut held: HashMap<String, (bool, Vec<(usize, usize)>)> = HashMap::new();
        for section in pack["sections"].as_array().into_iter().flatten() {
            for chunk in section["chunks"].as_array().into_iter().flatten() {
                let Some(path) = chunk["path"].as_str() else {
                    continue;
                };
                let entry = held.entry(path.to_string()).or_default();
                entry.0 |= chunk["whole"] == true;
                if let (Some(a), Some(b)) =
                    (chunk["start_line"].as_u64(), chunk["end_line"].as_u64())
                {
                    entry.1.push((a as usize, b as usize));
                }
            }
        }
        for (_, ranges) in held.values_mut() {
            ranges.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::new();
            for &(a, b) in ranges.iter() {
                match merged.last_mut() {
                    Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
                    _ => merged.push((a, b)),
                }
            }
            *ranges = merged;
        }
        Self(held)
    }

    /// Whether the pack holds lines `start..=end` of `path`.
    fn holds(&self, path: &str, start: usize, end: usize) -> bool {
        self.0.get(path).is_some_and(|(whole, ranges)| {
            *whole || ranges.iter().any(|&(a, b)| a <= start && end <= b)
        })
    }
}

/// A file added to a round's request: its lines, as a pack shows a file.
struct Added {
    path: String,
    start: usize,
    end: usize,
    whole: bool,
    tokens: usize,
    why: String,
    text: String,
}

/// The files of `wanted` that the next round takes, in order: the ones the
/// pack (`held`) does not hold whole, that `read` gives (the project's text
/// files in the worktree), whole when they fit in [`CONTEXT_FILE_TOKENS`],
/// else the lines around the line named in them; at most [`CONTEXT_FILES`]
/// files and [`CONTEXT_TOKENS`] tokens, counted with `counter` as the pack
/// counts. The others go to `left_out`, with why.
fn select(
    wanted: Vec<Wanted>,
    held: &Held,
    read: &mut dyn FnMut(&str) -> std::result::Result<String, String>,
    counter: &Counter,
    space: usize,
    left_out: &mut Vec<Value>,
) -> Vec<Added> {
    let mut added: Vec<Added> = Vec::new();
    let mut tokens = 0;
    // The sections' bytes so far, within `space`.
    let mut bytes = 0;
    for wanted in wanted {
        if added.len() >= CONTEXT_FILES {
            leave(
                left_out,
                "path",
                &wanted.path,
                format!("over the {CONTEXT_FILES} files a round adds"),
            );
            continue;
        }
        let text = match read(&wanted.path) {
            Ok(text) => text,
            Err(why) => {
                leave(left_out, "path", &wanted.path, why);
                continue;
            }
        };
        let lines = text.lines().count();
        if lines == 0 {
            leave(left_out, "path", &wanted.path, "an empty file");
            continue;
        }
        if held.holds(&wanted.path, 1, lines) {
            leave(left_out, "path", &wanted.path, "the pack holds it whole");
            continue;
        }
        let why = wanted.why.join("; ");
        let room = CONTEXT_FILE_TOKENS.min(CONTEXT_TOKENS.saturating_sub(tokens));
        let mut tries = vec![(1, lines)];
        if let Some(line) = wanted.line.filter(|&line| line <= lines) {
            for (before, after) in [
                (EXCERPT_BEFORE, EXCERPT_AFTER),
                (EXCERPT_BEFORE / 2, EXCERPT_AFTER / 2),
            ] {
                tries.push((
                    line.saturating_sub(before).max(1),
                    (line + after).min(lines),
                ));
            }
        }
        let mut fitted = None;
        let mut reason = if room < CONTEXT_FILE_TOKENS {
            format!("over the {CONTEXT_TOKENS} tokens a round adds")
        } else if wanted.line.is_some() {
            "too large, even the lines around the line named".to_string()
        } else {
            "too large to add whole, and no line of it is named".to_string()
        };
        for (start, end) in tries {
            if held.holds(&wanted.path, start, end) {
                reason = "the pack holds the lines named".to_string();
                break;
            }
            let (section, count) =
                crate::code_pack::file_section(&wanted.path, &text, start, end, &why, counter);
            if count <= room && section.len() > space.saturating_sub(bytes) {
                reason = "over the room the round's request has left".to_string();
            } else if count <= room {
                fitted = Some((start, end, section, count));
                break;
            }
        }
        match fitted {
            Some((start, end, section, count)) => {
                tokens += count;
                bytes += section.len();
                added.push(Added {
                    path: wanted.path,
                    start,
                    end,
                    whole: start == 1 && end == lines,
                    tokens: count,
                    why,
                    text: section,
                });
            }
            None => leave(left_out, "path", &wanted.path, reason),
        }
    }
    added
}

/// The text of the project's file `path` in the worktree's project folder,
/// read as the pack reads files: a file the project's last sync holds (so
/// not ignored, secret or skipped), not a credential or key by its name,
/// inside the project (no `..`, no symlink at the file or at a folder on its
/// path, nothing in `.git`), UTF-8 text of at most [`CONTEXT_READ_BYTES`].
fn read_project_file(
    project: &Path,
    files: &ProjectFiles,
    path: &str,
) -> std::result::Result<String, String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if !files.paths.contains(path)
        || crate::local::sensitive(name)
        || crate::local::skipped_file(path)
        || path
            .split('/')
            .any(|part| crate::local::SKIPPED_NAMES.contains(&part))
    {
        return Err("not a file the project's sync reads".into());
    }
    let full = safe_path(project, path).map_err(|why| format!("refused, {why}"))?;
    // The sync reads no symlink, so a round reads none either, even one that
    // stays inside the project: the worktree can have a link where the sync
    // read a file (to `.env`, or to an ignored file).
    let relative = full
        .strip_prefix(project)
        .map_err(|_| "refused, outside the project".to_string())?;
    let mut walked = project.to_path_buf();
    for part in relative.components() {
        walked.push(part);
        if walked
            .symlink_metadata()
            .is_ok_and(|found| found.file_type().is_symlink())
        {
            return Err("refused, a symlink".into());
        }
    }
    let metadata = std::fs::metadata(&full).map_err(|_| "not in the worktree".to_string())?;
    if !metadata.is_file() {
        return Err("not a file in the worktree".into());
    }
    if metadata.len() > CONTEXT_READ_BYTES {
        return Err("larger than 1 MiB".into());
    }
    let bytes = std::fs::read(&full).map_err(|_| "cannot be read".to_string())?;
    String::from_utf8(bytes).map_err(|_| "not UTF-8 text".into())
}

/// What `fix` keeps to add files to a round: the project's files and the
/// pack's counter, read at the first failed round, and the files the last
/// round added.
#[derive(Default)]
struct Context {
    files: Option<ProjectFiles>,
    counter: Option<Counter>,
    last: Vec<String>,
}

/// The bytes of the next round's request that the files a failed round adds
/// may take, for a pack made without `--budget`: what the model's input
/// limit, less a margin, leaves after the request's estimate without them
/// (`rest`: its text after the pack, with [`FILES_HEADING`]), as
/// [`long_context::fit_with`] estimates a request, with the calibration
/// `ratio`. Such a round fits with the same pack, so its start can still be
/// in the provider's prompt cache.
fn files_room(profile: &model::Profile, pack: &Value, ratio: Option<f64>, rest: &str) -> usize {
    let ratio = ratio.filter(|r| r.is_finite() && *r > 0.0).unwrap_or(1.0);
    let used = long_context::request_estimate(pack, FIX_INSTRUCTIONS, rest) as f64 * ratio;
    let left = profile.input_limit as f64 * (1.0 - crate::code_pack::estimate::MARGIN) - used;
    (left.max(0.0) / ratio * crate::code_pack::estimate::PACK_TEXT_BYTES_PER_TOKEN) as usize
}

/// After failed round `round`: the files that its output (`output`, the text
/// and what it is), the model's `reply` and the round's own added files name
/// ([`candidates`]) and that the pack does not hold whole, for the next
/// round ([`select`]), read from the worktree through the pack's file
/// filters ([`read_project_file`]), each code name found in the code graph
/// ([`crate::identifier_arm::defining_files`]), their sections within
/// `space` bytes ([`files_room`]). The section of the next round's request,
/// and its JSON `context` (the files added and why, and those left out),
/// when there is any.
#[allow(clippy::too_many_arguments)]
fn next_context(
    run: &Run<'_>,
    profile: &model::Profile,
    context: &mut Context,
    project: &Path,
    pack: &Value,
    round: usize,
    output: Option<(&str, &str)>,
    reply: Option<&Value>,
    space: usize,
) -> (String, Option<Value>) {
    let (workspace, token) = (
        run.session.workspace.as_str(),
        run.session.capability.as_str(),
    );
    let id = run.project.as_str();
    let files = context.files.get_or_insert_with(|| {
        let paths = run
            .store
            .authorized_read(workspace, token, "read", |db, p| {
                crate::store::repo(db, p, id)?;
                Ok(crate::store::heads(db, id)?
                    .into_keys()
                    .collect::<BTreeSet<String>>())
            })
            .unwrap_or_default();
        let mut roots = vec![project.to_string_lossy().trim_end_matches('/').to_string()];
        if let Ok(real) = project.canonicalize() {
            let real = real.to_string_lossy().trim_end_matches('/').to_string();
            if !roots.contains(&real) {
                roots.push(real);
            }
        }
        ProjectFiles::new(paths, roots)
    });
    let counter = context.counter.get_or_insert_with(|| {
        Counter::open(profile.tokenizer.as_deref(), None).unwrap_or(Counter::Estimate)
    });
    let mut defines = |names: &[String]| {
        run.store
            .authorized_read(workspace, token, "read", |db, p| {
                crate::store::repo(db, p, id)?;
                Ok(crate::identifier_arm::defining_files(
                    db,
                    id,
                    names,
                    CONTEXT_DEFINITIONS,
                ))
            })
            .unwrap_or_else(|_| vec![Vec::new(); names.len()])
    };
    let mut left_out: Vec<Value> = Vec::new();
    let wanted = candidates(
        round,
        output,
        reply,
        files,
        &mut defines,
        &context.last,
        &mut left_out,
    );
    let mut read = |path: &str| read_project_file(project, files, path);
    let added = select(
        wanted,
        &Held::of(pack),
        &mut read,
        counter,
        space,
        &mut left_out,
    );
    context.last = added.iter().map(|file| file.path.clone()).collect();
    if added.is_empty() && left_out.is_empty() {
        return (String::new(), None);
    }
    let mut text = String::new();
    if !added.is_empty() {
        text.push_str(FILES_HEADING);
        for file in &added {
            text.push_str(&file.text);
        }
    }
    let json = json!({
        "files": added.iter().map(|file| json!({"path":file.path,"start_line":file.start,
            "end_line":file.end,"whole":file.whole,"tokens":file.tokens,"why":file.why}))
            .collect::<Vec<_>>(),
        "tokens": added.iter().map(|file| file.tokens).sum::<usize>(),
        "left_out": left_out,
    });
    (text, Some(json))
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
        &run,
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
    let mut scrub = Scrub::of(&crate::settings::configuration(
        run.store,
        &run.session.workspace,
        run.config,
    )?);
    // A key saved in the app is in no variable the test could inherit; it
    // is hidden in the test's output too, as the keys in variables are.
    if long_context::saved_in_app(&profile)
        && let Ok(Some(secret)) = long_context::credential(&run, &profile)
        && secret.len() >= 8
    {
        scrub.secrets.push(secret);
    }
    // The bytes the files a failed round adds may take in the next round's
    // request (round `next`, after `follow_up`): without `--budget`, what
    // is left after the pack and the rest of the request ([`files_room`]);
    // with it, as much as the files' own bounds allow, and the round is
    // fitted to the input limit as before.
    let space = |pack: &Value, follow_up: &str, next: usize| -> usize {
        if run.budget.is_some() {
            return usize::MAX;
        }
        let rest = format!(
            "Issue: {issue}\nPlan: {plan_json}\n{follow_up}{FILES_HEADING}Round {next}: return the edits.\n"
        );
        files_room(&profile, pack, ratio, &rest)
    };
    let mut follow_up = String::new();
    let mut rounds: Vec<Value> = Vec::new();
    let mut suggested: Vec<String> = Vec::new();
    let mut explanations = String::new();
    let mut outcome = "not applied";
    let mut stopped: Option<String> = None;
    // The files a failed round names, for the next round: the section this
    // round's request adds after the follow-up, and its JSON.
    let mut context = Context::default();
    let mut section = String::new();
    let mut added: Option<Value> = None;
    for round in 1..=options.rounds {
        // The request fitted to the input limit; when it does not fit with
        // the added files, even with a smaller pack, it goes without them.
        let mut files = std::mem::take(&mut section);
        let (rest, fitted) = loop {
            let rest = format!(
                "Issue: {issue}\nPlan: {plan_json}\n{follow_up}{files}Round {round}: return the edits.\n"
            );
            let user_of = |pack: &Value| format!("{}\n{rest}", crate::code_pack::text(pack));
            let request = Request {
                question: &issue,
                rest: &rest,
                instructions: FIX_INSTRUCTIONS,
                user_of: &user_of,
                schema: Some(&schema),
            };
            let fitted = long_context::fit_with(
                &run,
                &profile,
                &request,
                pack.clone(),
                budget,
                ratio,
                human,
                &repack,
            )
            .await;
            if fitted.is_err() && !files.is_empty() {
                files.clear();
                if let Some(added) = added.as_mut() {
                    added["dropped"] = json!(
                        "the request does not fit the model's input limit with these files, so the round went without them"
                    );
                }
                // The round went without them, so the next round does not
                // add them again as files this round had added.
                context.last.clear();
                if human {
                    eprintln!(
                        "graphicle: round {round} does not fit the input limit with the files added for it; it goes without them"
                    );
                }
                continue;
            }
            break (rest, fitted);
        };
        let user_of = |pack: &Value| format!("{}\n{rest}", crate::code_pack::text(pack));
        let round_request = Request {
            question: &issue,
            rest: &rest,
            instructions: FIX_INSTRUCTIONS,
            user_of: &user_of,
            schema: Some(&schema),
        };
        let counted = match fitted {
            Ok((fitted, fitted_budget, counted)) => {
                pack = fitted;
                budget = fitted_budget;
                counted
            }
            Err(error) => {
                rounds.push(with_context(
                    json!({"round":round,"result":"does not fit","error":error.message}),
                    added.as_ref(),
                ));
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
                rounds.push(with_context(
                    json!({"round":round,"result":"refused by a cap","refused":[error.message]}),
                    added.as_ref(),
                ));
                outcome = "refused by a cap";
                stopped = Some(error.message);
                break;
            }
        };
        let done = match result {
            Ok(done) => done,
            Err(error) => {
                rounds.push(with_context(
                    json!({"round":round,"result":"provider error","error":error.message,"usage":entry}),
                    added.as_ref(),
                ));
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
                rounds.push(with_context(
                    round_json(round, None, &applied, None, &entry),
                    added.as_ref(),
                ));
                follow_up = format!(
                    "Round {round} failed: the reply was not one JSON object that matches the schema.\n"
                );
                // The files this round had added stay for the next.
                if round < options.rounds {
                    let (text, json) = next_context(
                        &run,
                        &profile,
                        &mut context,
                        &worktree.project,
                        &pack,
                        round,
                        None,
                        None,
                        space(&pack, &follow_up, round + 1),
                    );
                    section = text;
                    added = json;
                }
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
        rounds.push(with_context(
            round_json(round, Some(&reply), &applied, test.as_ref(), &entry),
            added.as_ref(),
        ));
        if human {
            eprintln!(
                "graphicle: round {round}: {} ({})",
                rounds.last().unwrap()["result"].as_str().unwrap_or(""),
                entry_line(&entry)
            );
        }
        let output = match (&applied, &test) {
            (Err(problems), _) => {
                outcome = if outcome == "failed" {
                    "failed"
                } else {
                    "not applied"
                };
                let problems = problems.join("\n");
                follow_up = format!("Round {round}: no edit was applied, because:\n{problems}\n");
                (problems, "the reasons its edits were refused")
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
                    "Round {round}: the edits were applied, and the test command {} ({}). Its output (when it is long, the lines that report failures, then its end):\n```\n{}\n```\nThe diff so far:\n```diff\n{}\n```\n",
                    if t.timed_out { "timed out" } else { "failed" },
                    t.code
                        .map_or("no exit code".to_string(), |c| format!("exit code {c}")),
                    t.tail,
                    tail(&diff_text, DIFF_CAP)
                );
                (t.tail.clone(), "the test output")
            }
        };
        // The files the failure and the model name, for the next round.
        if round < options.rounds {
            let (text, json) = next_context(
                &run,
                &profile,
                &mut context,
                &worktree.project,
                &pack,
                round,
                Some((output.0.as_str(), output.1)),
                Some(&reply),
                space(&pack, &follow_up, round + 1),
            );
            if human
                && let Some(files) = json.as_ref().and_then(|json| json["files"].as_array())
                && !files.is_empty()
            {
                let paths: Vec<&str> = files.iter().filter_map(|f| f["path"].as_str()).collect();
                eprintln!(
                    "graphicle: round {} adds {}",
                    round + 1,
                    printable(&paths.join(", "))
                );
            }
            section = text;
            added = json;
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
        // A round that went without its files says so for each of them.
        let dropped = round["context"]["dropped"].is_string();
        for file in round["context"]["files"].as_array().into_iter().flatten() {
            let path = file["path"].as_str().unwrap_or("");
            if dropped {
                line.push_str(&format!(
                    "\n  not added: {path} (the request did not fit the model's input limit with it)"
                ));
            } else {
                line.push_str(&format!(
                    "\n  added: {path} ({})",
                    file["why"].as_str().unwrap_or("")
                ));
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
    fn a_second_run_in_the_same_second_takes_the_next_folder() {
        let dir = tempfile::tempdir().unwrap();
        let stem = "project-make-it-42-20261102-063833";
        let free = || free_folder(dir.path(), stem).unwrap();
        assert_eq!(free(), dir.path().join(stem));
        std::fs::create_dir(dir.path().join(stem)).unwrap();
        assert_eq!(free(), dir.path().join(format!("{stem}-2")));
        std::fs::write(dir.path().join(format!("{stem}-2")), "").unwrap();
        assert_eq!(free(), dir.path().join(format!("{stem}-3")));
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

    fn lines_of(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    /// A vitest run as it prints a failure: the failed tests and the
    /// assertion first, then the DOM it compared (long), then where and the
    /// summary.
    fn vitest_output() -> String {
        let mut lines = lines_of(&[
            "yarn run v1.22.22",
            "$ vitest run packages/app/components/menu",
            "",
            " RUN  v3.0.6 /work/repo",
            "",
            " ❯ packages/app/components/menu/Menu.test.tsx (3 tests | 2 failed) 2609ms",
            "   ✓ Menu > closes on Escape",
            "   × Menu > closes the main menu on outside pointer events after reopening 1193ms",
            "     → expected <div data-side=\"bottom\" …(13)>…(1)</div> to be null",
            "",
            " FAIL  packages/app/components/menu/Menu.test.tsx > Menu > closes the main menu on outside pointer events after reopening",
            "AssertionError: expected <div data-side=\"bottom\" …(13)>…(1)</div> to be null",
            "",
            "- Expected: ",
            "null",
            "",
            "+ Received: ",
            "<div",
        ]);
        for i in 0..400 {
            lines.push(format!(
                "    <button class=\"menu-item\" data-testid=\"item-{i}\">Item {i}</button>"
            ));
        }
        lines.extend(lines_of(&[
            "</div>",
            "",
            " ❯ packages/app/components/menu/Menu.test.tsx:53:15",
            "     51|   expect(",
            "     53|   ).toBeNull();",
            "",
            " Test Files  1 failed (1)",
            "      Tests  2 failed | 1 passed (3)",
            "",
            "error Command failed with exit code 1.",
        ]));
        lines.join("\n") + "\n"
    }

    /// The lines that report failures survive the cut of a long output, each
    /// once, before its end, within the same bound.
    #[test]
    fn failure_lines_survive_the_trim() {
        let text = vitest_output();
        assert!(text.len() > 3 * OUTPUT_TAIL);
        // The end alone holds none of them.
        assert!(!tail(&text, OUTPUT_TAIL).contains("AssertionError"));
        let shown = digest(&text, &[], OUTPUT_TAIL);
        assert!(shown.len() <= OUTPUT_TAIL, "{}", shown.len());
        assert!(
            shown.starts_with("[the lines that report failures"),
            "{shown}"
        );
        let cut = shown.find("[...]\n").unwrap();
        for line in [
            "packages/app/components/menu/Menu.test.tsx (3 tests | 2 failed)",
            "× Menu > closes the main menu on outside pointer events after reopening 1193ms",
            "→ expected <div data-side=\"bottom\" …(13)>…(1)</div> to be null",
            "FAIL  packages/app/components/menu/Menu.test.tsx > Menu > closes the main menu",
            "AssertionError: expected <div data-side=\"bottom\" …(13)>…(1)</div> to be null",
            "- Expected:",
            "+ Received:",
        ] {
            assert!(shown[..cut].contains(line), "{line}\n{shown}");
        }
        assert_eq!(shown.matches("AssertionError").count(), 1, "{shown}");
        assert!(!shown[..cut].contains("data-testid"), "{shown}");
        assert!(!shown[..cut].contains("✓"), "{shown}");
        // The end follows the marker.
        let end = &shown[cut..];
        assert!(end.contains("Tests  2 failed | 1 passed (3)"), "{end}");
        assert!(
            end.ends_with("error Command failed with exit code 1.\n"),
            "{end}"
        );
        assert!(end.contains("data-testid=\"item-399\""), "{end}");
    }

    /// pytest's failure (the statement, its `E` lines and where), before a
    /// long captured log.
    #[test]
    fn pytest_failures_survive_the_trim() {
        let mut lines = lines_of(&[
            "============================= test session starts ==============================",
            "collected 3 items",
            "",
            "tests/test_cache.py F..                                                  [100%]",
            "",
            "=================================== FAILURES ===================================",
            "__________________________________ test_add ___________________________________",
            "",
            "    def test_add():",
            ">       assert add(1, 2) == 4",
            "E       assert 3 == 4",
            "E        +  where 3 = add(1, 2)",
            "",
            "tests/test_cache.py:5: AssertionError",
            "------------------------------ Captured log call -------------------------------",
        ]);
        for i in 0..300 {
            lines.push(format!(
                "DEBUG    cache:cache.py:{i} warmed entry {i} of the cache"
            ));
        }
        lines.extend(lines_of(&[
            "=========================== short test summary info ============================",
            "FAILED tests/test_cache.py::test_add - assert 3 == 4",
            "========================= 1 failed, 2 passed in 0.05s ==========================",
        ]));
        let text = lines.join("\n") + "\n";
        assert!(text.len() > OUTPUT_TAIL);
        let shown = digest(&text, &[], OUTPUT_TAIL);
        assert!(shown.len() <= OUTPUT_TAIL);
        let cut = shown.find("[...]\n").unwrap();
        for line in [
            "FAILURES",
            ">       assert add(1, 2) == 4",
            "E       assert 3 == 4",
            "E        +  where 3 = add(1, 2)",
            "tests/test_cache.py:5: AssertionError",
        ] {
            assert!(shown[..cut].contains(line), "{line}\n{shown}");
        }
        assert!(!shown[..cut].contains("DEBUG"), "{shown}");
        assert!(shown.ends_with("1 failed, 2 passed in 0.05s ==========================\n"));
    }

    /// An output that fits is shown as it is; one with no line that reports
    /// a failure keeps its end, as before; many such lines keep the bound.
    #[test]
    fn short_outputs_stay_and_the_bound_holds() {
        let short = "FAIL src/a.test.ts\nAssertionError: expected 1 to be 2\n";
        assert_eq!(digest(short, &[], OUTPUT_TAIL), short);
        let plain = "line of a long run of output\n".repeat(1_000);
        assert_eq!(digest(&plain, &[], OUTPUT_TAIL), tail(&plain, OUTPUT_TAIL));
        let mut many = String::new();
        for i in 0..2_000 {
            many.push_str(&format!(
                "FAIL test {i}: AssertionError: expected {i} to be {}\n",
                i + 1
            ));
        }
        let shown = digest(&many, &[], OUTPUT_TAIL);
        assert!(shown.len() <= OUTPUT_TAIL, "{}", shown.len());
        let cut = shown.find("[...]\n").unwrap();
        assert!(cut <= FAILURE_SHARE + 200, "{cut}");
        assert!(shown[..cut].contains("FAIL test 0: "), "{shown}");
        assert!(shown[..cut].contains("more such lines left out"), "{shown}");
        assert!(shown.ends_with("FAIL test 1999: AssertionError: expected 1999 to be 2000\n"));
        // A long line keeps its start, cut to the line's bound.
        let long = format!("Error: {}", "x".repeat(5_000));
        let text = format!("{long}\n{}", "filler line\n".repeat(1_000));
        let cut_line = shortened(&long, FAILURE_LINE);
        assert_eq!(cut_line.len(), FAILURE_LINE);
        assert!(digest(&text, &[], OUTPUT_TAIL).contains(&cut_line));
        // Lines the streams dropped come first, each once.
        let early = lines_of(&["FAIL early.test.ts", "FAIL early.test.ts"]);
        let shown = digest(&plain, &early, OUTPUT_TAIL);
        assert!(shown.len() <= OUTPUT_TAIL);
        assert_eq!(shown.matches("FAIL early.test.ts").count(), 1, "{shown}");
        assert!(shown.ends_with("line of a long run of output\n"));
    }

    /// A failure printed before more output than a stream keeps is still
    /// shown, without colour codes and with its secrets hidden.
    #[cfg(unix)]
    #[test]
    fn failures_the_streams_dropped_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let scrub = Scrub {
            remove: Vec::new(),
            secrets: vec!["s3cr3t-value-0123456789".into()],
        };
        let run = run_test(
            dir.path(),
            "printf '\\033[31mReferenceError\\033[39m: helper is not defined\\n'; echo 'FAIL leaked s3cr3t-value-0123456789'; i=0; while [ $i -lt 6000 ]; do echo filler line $i of a long run of output; i=$((i+1)); done; echo the end",
            Duration::from_secs(60),
            &scrub,
        );
        assert!(run.passed, "{run:?}");
        assert!(run.tail.len() <= OUTPUT_TAIL, "{}", run.tail.len());
        assert!(
            run.tail.contains("ReferenceError: helper is not defined"),
            "{}",
            run.tail
        );
        assert!(run.tail.contains("FAIL leaked [redacted]"), "{}", run.tail);
        assert!(!run.tail.contains("s3cr3t") && !run.tail.contains('\u{1b}'));
        assert!(run.tail.ends_with("the end\n"), "{}", run.tail);
        let mut kept = Kept::default();
        kept.push(b"FAIL first\n");
        kept.push(&b"filler\n".repeat(3 * OUTPUT_KEEP / 7));
        assert!(kept.cut && kept.failures == ["FAIL first"]);
        assert!(kept.bytes.len() <= 2 * OUTPUT_KEEP);
    }

    /// A failure printed before more output than a stream keeps of its end
    /// (64 KiB), but less than it holds before it drops anything (128 KiB),
    /// is still shown: here about 100 KB follow the assertion.
    #[cfg(unix)]
    #[test]
    fn a_failure_before_100_kb_of_output_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_test(
            dir.path(),
            "echo 'AssertionError: expected 41 to be 42'; i=0; while [ $i -lt 2500 ]; do echo filler line $i of a long run of output; i=$((i+1)); done; echo the end",
            Duration::from_secs(60),
            &Scrub::default(),
        );
        assert!(run.passed, "{run:?}");
        assert!(run.tail.len() <= OUTPUT_TAIL, "{}", run.tail.len());
        assert!(
            run.tail.contains("AssertionError: expected 41 to be 42"),
            "{}",
            run.tail
        );
        assert!(run.tail.ends_with("the end\n"), "{}", run.tail);
        let mut kept = Kept::default();
        kept.push(b"FAIL first\n");
        kept.push(&b"filler line of a long run of output\n".repeat(100_000 / 36));
        assert!(!kept.cut && kept.failures.is_empty());
        assert!(kept.text().starts_with("FAIL first\n"));
    }

    /// What a stream keeps starts at a line's start: a cut that falls more
    /// than [`LINE_SLACK`] into a long line moves to that line's end when it
    /// is near.
    #[test]
    fn a_stream_keeps_whole_lines_after_a_long_one() {
        let mut early = b"FAIL early\n".to_vec();
        early.extend(b"short line\n".repeat(6_515));
        let mut long = vec![b'y'; 12 * 1024 - 1];
        long.push(b'\n');
        let end = b"tail line\n".repeat(6_257);
        let mut kept = Kept::default();
        kept.push(&early);
        kept.push(&long);
        kept.push(&end);
        // The cut, 64 KiB before the end, is 9,322 bytes into the long line
        // and 2,966 bytes before its end.
        assert!(kept.cut);
        assert_eq!(kept.failures, ["FAIL early"]);
        assert!(kept.bytes == end, "{} bytes kept", kept.bytes.len());
        assert!(kept.text().starts_with("...\ntail line\n"));
    }

    /// The readable report lists a round's added files, and names the files
    /// of a round that went without them as not added.
    #[test]
    fn the_report_names_the_files_a_round_went_without() {
        let file = json!({"path":"src/big.go","why":"the test output names it"});
        let report = json!({"rounds":[
            {"round":2,"result":"failed","context":{"files":[file]}},
            {"round":3,"result":"passed","context":{"files":[file],
                "dropped":"the request does not fit the model's input limit with these files, so the round went without them"}}]});
        let text = report_text(&report);
        assert!(
            text.contains("Round 2: failed\n  added: src/big.go (the test output names it)\n"),
            "{text}"
        );
        assert!(
            text.contains("Round 3: passed\n  not added: src/big.go (the request did not fit the model's input limit with it)\n"),
            "{text}"
        );
        assert_eq!(text.matches("\n  added: ").count(), 1, "{text}");
    }

    /// A round reads only a regular file of the project's sync, inside the
    /// project: never a path the sync leaves out, a symlink at the file or at
    /// a folder on its path (even one that stays inside the project), a
    /// missing file, a folder, a file over 1 MiB or one that is not UTF-8.
    #[cfg(unix)]
    #[test]
    fn a_round_reads_only_the_projects_own_regular_files() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "outside\n").unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(project.join(".env"), "KEY=value\n").unwrap();
        std::fs::write(
            project.join("big.txt"),
            "x".repeat(CONTEXT_READ_BYTES as usize + 1),
        )
        .unwrap();
        std::fs::write(project.join("latin.txt"), [0xffu8, 0xfe, 0x41]).unwrap();
        symlink("src/a.ts", project.join("link.ts")).unwrap();
        symlink(".env", project.join("env.ts")).unwrap();
        symlink("src", project.join("linked")).unwrap();
        symlink(&outside, project.join("out.ts")).unwrap();
        let synced = [
            "src/a.ts",
            ".env",
            "big.txt",
            "latin.txt",
            "link.ts",
            "env.ts",
            "linked/a.ts",
            "out.ts",
            "missing.ts",
            "src",
            "node_modules/x/index.js",
        ];
        let files = ProjectFiles::new(
            synced.iter().map(|path| path.to_string()).collect(),
            vec![project.to_string_lossy().into_owned()],
        );
        assert_eq!(
            read_project_file(&project, &files, "src/a.ts"),
            Ok("export const a = 1;\n".to_string())
        );
        for (path, why) in [
            ("not/synced.ts", "not a file the project's sync reads"),
            (".env", "not a file the project's sync reads"),
            (
                "node_modules/x/index.js",
                "not a file the project's sync reads",
            ),
            ("link.ts", "refused, a symlink"),
            ("env.ts", "refused, a symlink"),
            ("linked/a.ts", "refused, a symlink"),
            (
                "out.ts",
                "refused, a symlink that leads outside the repository",
            ),
            ("missing.ts", "not in the worktree"),
            ("src", "not a file in the worktree"),
            ("big.txt", "larger than 1 MiB"),
            ("latin.txt", "not UTF-8 text"),
        ] {
            assert_eq!(
                read_project_file(&project, &files, path),
                Err(why.to_string()),
                "{path}"
            );
        }
    }

    /// A project of a few files, its worktree at `/work/fix/repo-1`.
    fn project_files() -> ProjectFiles {
        ProjectFiles::new(
            [
                "packages/app/hooks/useOutside.ts",
                "packages/app/components/menu/Menu.test.tsx",
                "packages/app/components/menu/MenuContent.tsx",
                "src/index.ts",
                "lib/index.ts",
                "README.md",
            ]
            .iter()
            .map(|path| path.to_string())
            .collect(),
            vec!["/work/fix/repo-1".into()],
        )
    }

    /// Paths count only when they are the project's files: as written,
    /// absolute inside the worktree, or the end of exactly one path.
    #[test]
    fn paths_named_are_the_projects_files() {
        let files = project_files();
        let text = [
            " FAIL  packages/app/components/menu/Menu.test.tsx > Menu > closes",
            " ❯ MenuContent packages/app/components/menu/MenuContent.tsx:37:24",
            " ❯ /work/fix/repo-1/packages/app/hooks/useOutside.ts:12:3",
            " ❯ /home/me/other/packages/app/hooks/useOutside.ts:9",
            " ❯ node_modules/react-dom/cjs/react-dom.development.js:9683:18",
            "see index.ts, Menu.test.tsx:5, ../outside/README.md and docs/missing.md",
        ]
        .join("\n");
        assert_eq!(
            paths_named(&text, &files),
            vec![
                (
                    "packages/app/components/menu/Menu.test.tsx".to_string(),
                    Some(5)
                ),
                (
                    "packages/app/components/menu/MenuContent.tsx".to_string(),
                    Some(37)
                ),
                ("packages/app/hooks/useOutside.ts".to_string(), Some(12)),
            ]
        );
        assert_eq!(
            files.resolve("repo-1/packages/app/hooks/useOutside.ts"),
            Some("packages/app/hooks/useOutside.ts".into())
        );
        assert_eq!(files.resolve("./README.md"), Some("README.md".into()));
        for refused in [
            "index.ts",
            "/README.md",
            "../README.md",
            "node_modules/x/README.md",
            "missing.ts",
        ] {
            assert_eq!(files.resolve(refused), None, "{refused}");
        }
    }

    /// The files of a failed round, in order: the output's and the model's
    /// by turns (paths its explanation writes, its edits', then the files
    /// that define the names it gives), then the round's own; a name too
    /// many files define is left out; the pack's whole files are not added.
    #[test]
    fn names_lead_to_the_files_that_define_them() {
        let files = project_files();
        let reply = json!({
            "explanation": "The listener in `useOutside` (packages/app/components/menu/MenuContent.tsx:41-56) never runs; MenuContent and `renderMenu` are fine, and `handler` is everywhere.",
            "edits": [{"path": "src/index.ts", "search": "a", "replace": "b"}],
        });
        let mut asked: Vec<String> = Vec::new();
        let mut defines = |names: &[String]| -> crate::identifier_arm::Definitions {
            asked.extend(names.iter().cloned());
            names
                .iter()
                .map(|name| match name.as_str() {
                    "useOutside" => vec![("packages/app/hooks/useOutside.ts".to_string(), Some(3))],
                    "handler" => (0..5).map(|i| (format!("src/h{i}.ts"), Some(1))).collect(),
                    _ => Vec::new(),
                })
                .collect()
        };
        let mut left_out = Vec::new();
        let wanted = candidates(
            2,
            Some((
                " FAIL  packages/app/components/menu/Menu.test.tsx > Menu",
                "the test output",
            )),
            Some(&reply),
            &files,
            &mut defines,
            &["README.md".to_string()],
            &mut left_out,
        );
        assert_eq!(asked, ["useOutside", "renderMenu", "handler"]);
        let paths: Vec<&str> = wanted.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "packages/app/components/menu/Menu.test.tsx",
                "packages/app/components/menu/MenuContent.tsx",
                "src/index.ts",
                "packages/app/hooks/useOutside.ts",
                "README.md",
            ]
        );
        assert_eq!(wanted[1].line, Some(41));
        assert_eq!(wanted[3].line, Some(3));
        assert_eq!(
            wanted[3].why,
            ["it defines `useOutside`, which round 2's explanation names"]
        );
        assert_eq!(
            left_out,
            [json!({"symbol":"handler","why":"more than 3 files define it"})]
        );
        // MenuContent.tsx is whole in the pack, and so is README.md (its two
        // lines are all of it); the rest are read and added.
        let pack = json!({"sections":[{"chunks":[
            {"path":"packages/app/components/menu/MenuContent.tsx","start_line":1,"end_line":80,"whole":true},
            {"path":"README.md","start_line":1,"end_line":1,"whole":false},
            {"path":"README.md","start_line":2,"end_line":2,"whole":false}]}]});
        let mut read = |path: &str| -> std::result::Result<String, String> {
            match path {
                "README.md" => Ok("# App\nIt draws menus.\n".to_string()),
                "packages/app/hooks/useOutside.ts" => {
                    Ok("export function useOutside() {\n  return null;\n}\n".to_string())
                }
                _ => Ok(format!("// {path}\n")),
            }
        };
        let mut left_out = Vec::new();
        let added = select(
            wanted,
            &Held::of(&pack),
            &mut read,
            &Counter::Estimate,
            usize::MAX,
            &mut left_out,
        );
        let paths: Vec<&str> = added.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "packages/app/components/menu/Menu.test.tsx",
                "src/index.ts",
                "packages/app/hooks/useOutside.ts"
            ]
        );
        let hook = &added[2];
        assert!(hook.whole && hook.start == 1 && hook.end == 3);
        assert!(
            hook.text.starts_with("### packages/app/hooks/useOutside.ts lines 1-3 (whole file)\nwhy: it defines `useOutside`"),
            "{}",
            hook.text
        );
        assert!(hook.text.contains("1\texport function useOutside() {\n"));
        assert_eq!(
            left_out,
            [
                json!({"path":"packages/app/components/menu/MenuContent.tsx","why":"the pack holds it whole"}),
                json!({"path":"README.md","why":"the pack holds it whole"}),
            ]
        );
    }

    /// A round adds six files and 24,000 tokens at most; a file too large to
    /// add whole goes in as the lines around the line named, else not.
    #[test]
    fn a_round_adds_six_files_and_24000_tokens_at_most() {
        let named = |path: &str, line: Option<usize>| Wanted {
            path: path.to_string(),
            line,
            why: vec!["named".into()],
        };
        let small = "package f\n".repeat(20);
        let mut read = |_: &str| -> std::result::Result<String, String> { Ok(small.clone()) };
        let mut left_out = Vec::new();
        let wanted: Vec<Wanted> = (0..9)
            .map(|i| named(&format!("src/f{i}.go"), None))
            .collect();
        let added = select(
            wanted,
            &Held::of(&json!({})),
            &mut read,
            &Counter::Estimate,
            usize::MAX,
            &mut left_out,
        );
        assert_eq!(added.len(), CONTEXT_FILES);
        assert_eq!(left_out.len(), 3);
        assert!(
            left_out
                .iter()
                .all(|l| l["why"] == "over the 6 files a round adds")
        );
        // Files of about 10,000 tokens: two fit, a third does not, a small
        // one after it still does.
        let big = "\tvalue := compute(\"a word of the file\")\n".repeat(800);
        let (_, one) = crate::code_pack::file_section(
            "src/big0.go",
            &big,
            1,
            800,
            "named",
            &Counter::Estimate,
        );
        assert!(
            one <= CONTEXT_FILE_TOKENS && 2 * one <= CONTEXT_TOKENS && 3 * one > CONTEXT_TOKENS,
            "{one}"
        );
        let mut read = |path: &str| -> std::result::Result<String, String> {
            Ok(if path.starts_with("src/big") {
                big.clone()
            } else {
                small.clone()
            })
        };
        let mut left_out = Vec::new();
        let wanted = vec![
            named("src/big0.go", None),
            named("src/big1.go", None),
            named("src/big2.go", None),
            named("src/small.go", None),
        ];
        let added = select(
            wanted,
            &Held::of(&json!({})),
            &mut read,
            &Counter::Estimate,
            usize::MAX,
            &mut left_out,
        );
        let paths: Vec<&str> = added.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(paths, ["src/big0.go", "src/big1.go", "src/small.go"]);
        assert!(added.iter().map(|a| a.tokens).sum::<usize>() <= CONTEXT_TOKENS);
        assert_eq!(
            left_out,
            [json!({"path":"src/big2.go","why":"over the 24000 tokens a round adds"})]
        );
        // Too large whole: the lines around the line named, else left out;
        // lines the pack holds are not added again.
        let huge = "\tvalue := compute(\"a word of the file\")\n".repeat(2_000);
        let mut read = |_: &str| -> std::result::Result<String, String> { Ok(huge.clone()) };
        let mut left_out = Vec::new();
        let pack = json!({"sections":[{"chunks":[
            {"path":"src/held.go","start_line":1,"end_line":1_000,"whole":false}]}]});
        let added = select(
            vec![
                named("src/huge.go", Some(1_500)),
                named("src/huge2.go", None),
                named("src/held.go", Some(500)),
            ],
            &Held::of(&pack),
            &mut read,
            &Counter::Estimate,
            usize::MAX,
            &mut left_out,
        );
        assert_eq!(added.len(), 1);
        assert_eq!(
            (added[0].start, added[0].end, added[0].whole),
            (1_470, 1_670, false)
        );
        assert!(
            added[0]
                .text
                .starts_with("### src/huge.go lines 1470-1670\n")
        );
        assert_eq!(
            left_out,
            [
                json!({"path":"src/huge2.go","why":"too large to add whole, and no line of it is named"}),
                json!({"path":"src/held.go","why":"the pack holds the lines named"}),
            ]
        );
        // Within the room the round's request has left (12,000 bytes here):
        // the lines around the line named go in, and a file that fits in no
        // form after them is left out for that room.
        let mut left_out = Vec::new();
        let added = select(
            vec![
                named("src/huge.go", Some(1_500)),
                named("src/huge3.go", Some(500)),
            ],
            &Held::of(&json!({})),
            &mut read,
            &Counter::Estimate,
            12_000,
            &mut left_out,
        );
        assert_eq!(added.len(), 1);
        assert_eq!((added[0].start, added[0].end), (1_470, 1_670));
        assert!(added[0].text.len() <= 12_000, "{}", added[0].text.len());
        assert_eq!(
            left_out,
            [json!({"path":"src/huge3.go","why":"over the room the round's request has left"})]
        );
    }
}
