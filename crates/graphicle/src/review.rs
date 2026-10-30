//! `graphicle review`: review comments on a change, from a cache-aware pack.
//!
//! The change is what `--head` adds since its merge base with `--base`, as a
//! pull request shows it, read with git in the project's folder (`fix`'s git:
//! hooks off, no variables that point it elsewhere). One model call:
//! - the instructions, then a pack ([`crate::code_pack`]) whose repository
//!   part comes first in the cache layout, and whose question part has the
//!   changed hunks pinned at the top of its ranking
//!   ([`crate::code_pack::pinned`]): its focus is the changed code, its call
//!   paths are the changed symbols' callers and callees along the code
//!   graph's call edges, and its tests are the tests that touch them;
//! - then the change's own text: the lines of the pack's architecture for the
//!   modules the change touches, the changed hunks, and the diff.
//!
//! The model returns structured comments: path, line, severity, body and the
//! `path:line` evidence each relies on. A comment is kept only on a line of
//! the head inside one of the diff's changed hunks, the lines a pull request
//! review can comment on; the others are dropped, with the reason. Its
//! evidence is checked against the working tree, and a comment none of whose
//! evidence holds is flagged: shown, never posted. Output is markdown for a
//! terminal, the report as JSON, or the body of GitHub's "create a review"
//! call (`--format github`), always a plain `COMMENT` review.
//!
//! The pack reads the project's synced files, so `--head` must be the commit
//! checked out in the project's folder. The call goes through the spend log
//! and its caps ([`crate::spend`]) like `ask --pack`. Nothing is fetched,
//! pushed or posted: the GitHub Action posts the review.

use crate::fix::Git;
use crate::long_context::{self, Request, Run, Tree};
use crate::{Error, Result, model};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

pub const REVIEW_INSTRUCTIONS: &str = "You review a change to a code repository from a pack of its code and the change's diff. The pack starts with a header. It holds an architecture summary, a manifest of the files it holds, and the files themselves, each headed by its path, line range and why it is included. Its repository part comes first; its change part holds the changed code, the callers and callees of the changed symbols, and the tests that touch them. Code lines that start with a number and a tab carry their line number, which is not part of the file, and the pack shows the code as the change leaves it. After the pack come the architecture of the modules the change touches, the changed hunks with their line ranges, and the diff. Return one JSON object that matches the schema: a short summary of the change and of what you found, and the comments. Comment only on what a careful reviewer would raise: bugs, callers or contracts the change breaks, missing or wrong tests for changed behaviour, security and data-loss risks, and clear performance traps. Skip style and naming unless they hide a bug, and do not praise. Each comment gives a path exactly as the diff shows it and one line inside a changed hunk, numbered as in the code after the change; a severity: blocker, major, minor or nit; a body that says what is wrong and what to do about it; and its evidence: the path:line or path:start-end citations of the code the comment relies on, from the pack or the diff. Leave out any comment you cannot support with evidence, and return no comments when the change has no problems.";

/// Severities, most severe first.
pub const SEVERITIES: [&str; 4] = ["blocker", "major", "minor", "nit"];

/// The comments' JSON schema.
pub fn review_schema() -> Value {
    json!({"type":"object","properties":{
        "summary":{"type":"string"},
        "comments":{"type":"array","items":{"type":"object","properties":{
            "path":{"type":"string"},"line":{"type":"integer"},
            "severity":{"type":"string","enum":SEVERITIES},
            "body":{"type":"string"},
            "evidence":{"type":"array","items":{"type":"string"}}}}}}})
}

/// The diff's bytes a request holds at most; past them it is cut, and the
/// list of changed hunks still names every file.
const DIFF_CAP: usize = 400_000;
/// Room for the change's own text the repository part's size leaves, so that
/// it stays the same bytes for changes of up to about this many tokens.
const REVIEW_ROOM: usize = 24_000;
/// Room for the affected modules' lines of the architecture.
const MODULES_ROOM: usize = 4_000;
/// The affected modules' lines at most.
const MODULE_LINES: usize = 60;
/// A comment's body, in characters, at most.
const BODY_CAP: usize = 4_000;
/// Evidence entries read per comment at most.
const EVIDENCE_CAP: usize = 12;
/// The question the pack is ranked for, in bytes at most.
const QUESTION_CAP: usize = 2_000;
/// The summary, in characters, at most.
const SUMMARY_CAP: usize = 8_000;

/// `graphicle review`'s arguments.
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// The base to review against: a commit, branch or tag. The review covers
    /// what the head adds since their merge base, as a pull request shows it
    /// [default: origin/HEAD, else origin/main, origin/master, main or
    /// master].
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,
    /// The head: the commit checked out in the project's folder, synced
    /// after checkout [default: HEAD].
    #[arg(long, value_name = "REF")]
    pub head: Option<String>,
    /// markdown, json (the report) or github (the body of GitHub's
    /// create-a-review call, for the Action) [default: markdown at a
    /// terminal, else json].
    #[arg(long, value_parser = ["markdown", "json", "github"])]
    pub format: Option<String>,
    /// Comments to keep at most, the most severe first.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u64).range(0..=100))]
    pub max_comments: u64,
    /// The project: an id, a key or a synced folder (default: this folder's
    /// project, else the last synced one).
    #[arg(long)]
    pub project: Option<String>,
    /// The pack's budget in tokens [default: the model's input limit less
    /// room for the diff].
    #[arg(long, value_name = "TOKENS")]
    pub budget: Option<usize>,
    /// The configured model profile to ask [default: the answer model].
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,
    /// The pack's layout, standard or cache [default: cache].
    #[arg(long, value_parser = ["standard", "cache"])]
    pub layout: Option<String>,
    /// With the cache layout: the share of the budget the repository part
    /// takes at most, from 0.1 to 1 [default: 0.9].
    #[arg(long, value_name = "F")]
    pub prefix_share: Option<f64>,
}

impl Args {
    /// Whether the output is for a program: json or github.
    pub fn machine(&self) -> bool {
        matches!(self.format.as_deref(), Some("json" | "github"))
    }
}

/// The output's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Json,
    Github,
}

impl Format {
    /// `--format`, else markdown at a terminal and JSON otherwise.
    pub fn of(asked: Option<&str>) -> Self {
        match asked {
            Some("markdown") => Self::Markdown,
            Some("json") => Self::Json,
            Some("github") => Self::Github,
            _ if crate::terminal::human(false) => Self::Markdown,
            _ => Self::Json,
        }
    }
}

/// One hunk of a diff: its ranges in the base and the head, and git's
/// function context after its `@@`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub context: String,
}

impl Hunk {
    /// The head's lines the hunk shows (added and unchanged), if any.
    pub fn head_range(&self) -> Option<(usize, usize)> {
        (self.new_lines > 0 && self.new_start > 0)
            .then(|| (self.new_start, self.new_start + self.new_lines - 1))
    }
}

/// One file of a diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// Its path in the head; none for a deleted file.
    pub path: Option<String>,
    /// Its path in the base; none for an added file.
    pub old_path: Option<String>,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    pub added: usize,
    pub removed: usize,
}

impl FileDiff {
    pub fn status(&self) -> &'static str {
        match (&self.path, &self.old_path) {
            (Some(_), None) => "added",
            (None, _) => "deleted",
            (Some(new), Some(old)) if new != old => "renamed",
            _ => "modified",
        }
    }

    /// The path a reader knows it by.
    pub fn name(&self) -> &str {
        self.path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }

    /// Whether the head's `line` is inside one of the hunks.
    pub fn in_hunks(&self, line: usize) -> bool {
        self.hunks
            .iter()
            .filter_map(Hunk::head_range)
            .any(|(a, b)| a <= line && line <= b)
    }
}

static HUNK: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)$").unwrap()
});

/// Git's C-style quoting undone (`"a\tb\303\251"`); none when it is not
/// quoting.
fn unquote(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next()? {
            b'n' => bytes.push(b'\n'),
            b't' => bytes.push(b'\t'),
            b'r' => bytes.push(b'\r'),
            b'a' => bytes.push(7),
            b'b' => bytes.push(8),
            b'f' => bytes.push(12),
            b'v' => bytes.push(11),
            b'\\' => bytes.push(b'\\'),
            b'"' => bytes.push(b'"'),
            first @ b'0'..=b'7' => {
                let mut value = u32::from(first - b'0');
                for _ in 0..2 {
                    match it.peek() {
                        Some(&digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            it.next();
                        }
                        _ => break,
                    }
                }
                bytes.push((value & 0xff) as u8);
            }
            _ => return None,
        }
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A path as a diff's header prints it: quoted or not.
fn plain(text: &str) -> String {
    let text = text.strip_suffix('\t').unwrap_or(text);
    unquote(text).unwrap_or_else(|| text.to_string())
}

/// The path of a `---` or `+++` line after its `a/` or `b/`; none for
/// `/dev/null`.
fn side_path(text: &str, side: &str) -> Option<String> {
    let text = plain(text);
    if text == "/dev/null" {
        return None;
    }
    Some(text.strip_prefix(side).unwrap_or(&text).to_string())
}

/// The two paths of a `diff --git a/X b/Y` line, for the files without
/// `---` and `+++` lines (binary files, mode changes).
fn header_paths(rest: &str) -> Option<(String, String)> {
    if let Some(quoted) = rest.strip_prefix('"') {
        let close = quoted.find("\" ")?;
        let old = unquote(&rest[..close + 2])?;
        let new = plain(&quoted[close + 2..]);
        return Some((
            old.strip_prefix("a/")?.to_string(),
            new.strip_prefix("b/")?.to_string(),
        ));
    }
    // `a/P b/P`, the same path twice, is the usual case; split in the middle.
    let n = rest.len();
    if n >= 5 && (n - 5).is_multiple_of(2) {
        let p = (n - 5) / 2;
        if rest.is_char_boundary(2 + p)
            && rest.is_char_boundary(5 + p)
            && rest.starts_with("a/")
            && &rest[2 + p..5 + p] == " b/"
            && rest[2..2 + p] == rest[5 + p..]
        {
            return Some((rest[2..2 + p].to_string(), rest[5 + p..].to_string()));
        }
    }
    let (old, new) = rest.split_once(" b/")?;
    Some((old.strip_prefix("a/")?.to_string(), plain(new)))
}

/// The files of a unified diff as `git diff` prints it, with the `a/` and
/// `b/` prefixes.
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    // The base's and the head's lines the current hunk has still to show.
    let (mut old_left, mut new_left) = (0usize, 0usize);
    for line in text.split('\n') {
        if old_left > 0 || new_left > 0 {
            let file = files.last_mut();
            match line.as_bytes().first() {
                Some(b'+') => {
                    new_left = new_left.saturating_sub(1);
                    if let Some(file) = file {
                        file.added += 1;
                    }
                    continue;
                }
                Some(b'-') => {
                    old_left = old_left.saturating_sub(1);
                    if let Some(file) = file {
                        file.removed += 1;
                    }
                    continue;
                }
                // An unchanged line; `diff.suppressBlankEmpty` prints an empty
                // one without its space.
                Some(b' ') | None => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    continue;
                }
                Some(b'\\') => continue,
                // A hunk that ends early: read the line as a header.
                _ => (old_left, new_left) = (0, 0),
            }
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let mut file = FileDiff::default();
            if let Some((old, new)) = header_paths(rest) {
                file.old_path = Some(old);
                file.path = Some(new);
            }
            files.push(file);
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("--- ") {
            file.old_path = side_path(rest, "a/");
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            file.path = side_path(rest, "b/");
        } else if line.starts_with("new file mode ") {
            file.old_path = None;
        } else if line.starts_with("deleted file mode ") {
            file.path = None;
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            file.old_path = Some(plain(rest));
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            file.path = Some(plain(rest));
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        } else if let Some(c) = HUNK.captures(line) {
            let n = |i: usize, default: usize| {
                c.get(i)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(default)
            };
            let hunk = Hunk {
                old_start: n(1, 0),
                old_lines: n(2, 1),
                new_start: n(3, 0),
                new_lines: n(4, 1),
                context: c.get(5).map_or("", |m| m.as_str()).trim().to_string(),
            };
            (old_left, new_left) = (hunk.old_lines, hunk.new_lines);
            file.hunks.push(hunk);
        }
    }
    files
}

/// The first `cap` bytes of `text` at a character boundary.
fn clip(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn short(sha: &str) -> &str {
    &sha[..12.min(sha.len())]
}

/// A severity of [`SEVERITIES`] for what the model wrote.
fn severity_of(text: Option<&str>) -> &'static str {
    match text.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "blocker" | "critical" => "blocker",
        "major" | "high" | "error" => "major",
        "nit" | "info" | "style" | "suggestion" | "trivial" => "nit",
        _ => "minor",
    }
}

fn severity_rank(severity: &str) -> usize {
    SEVERITIES
        .iter()
        .position(|s| *s == severity)
        .unwrap_or(SEVERITIES.len())
}

/// What became of the model's comments.
#[derive(Debug, Default)]
pub struct Checked {
    /// On a changed line, with evidence that holds: shown and posted.
    pub kept: Vec<Value>,
    /// On a changed line, but none of its evidence holds: shown, not posted.
    pub flagged: Vec<Value>,
    /// Elsewhere, empty, a repeat, or over the cap, with the reason.
    pub dropped: Vec<Value>,
}

/// The comments of the model's `reply` checked against the change's `files`
/// and the working `tree`: at most `max` kept, the most severe first.
pub fn check(reply: &Value, files: &[FileDiff], tree: &mut Tree, max: usize) -> Checked {
    let by_path: HashMap<&str, &FileDiff> = files
        .iter()
        .filter_map(|f| Some((f.path.as_deref()?, f)))
        .collect();
    let mut checked = Checked::default();
    let mut seen: HashSet<(String, usize, String)> = HashSet::new();
    let mut held: Vec<Value> = Vec::new();
    for comment in reply["comments"].as_array().into_iter().flatten() {
        let written = comment["path"].as_str().unwrap_or("").trim();
        let mut path = written.trim_start_matches("./").to_string();
        if !by_path.contains_key(path.as_str())
            && let Some(stripped) = path.strip_prefix("b/").or(path.strip_prefix("a/"))
            && by_path.contains_key(stripped)
        {
            path = stripped.to_string();
        }
        let line = comment["line"]
            .as_u64()
            .or_else(|| comment["line"].as_str().and_then(|s| s.trim().parse().ok()))
            .unwrap_or(0) as usize;
        let severity = severity_of(comment["severity"].as_str());
        let body = clip(comment["body"].as_str().unwrap_or("").trim(), BODY_CAP).to_string();
        let mut out = json!({"path":path,"line":line,"severity":severity,"body":body});
        let problem = if body.is_empty() {
            Some("the comment is empty".to_string())
        } else if line == 0 {
            Some("the comment names no line".to_string())
        } else {
            match by_path.get(path.as_str()) {
                None => Some(format!(
                    "{} is not a file the change adds or modifies",
                    clip(written, 200)
                )),
                Some(file) if !file.in_hunks(line) => Some(format!(
                    "line {line} is outside the changed hunks of {path}"
                )),
                Some(_) => None,
            }
        };
        if let Some(problem) = problem {
            out["reason"] = json!(problem);
            checked.dropped.push(out);
            continue;
        }
        if !seen.insert((path.clone(), line, body.clone())) {
            out["reason"] = json!("a repeat of an earlier comment");
            checked.dropped.push(out);
            continue;
        }
        let mut evidence: Vec<Value> = Vec::new();
        for item in comment["evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(EVIDENCE_CAP)
        {
            let cites = long_context::citations(item);
            if cites.is_empty() {
                evidence.push(json!({"cite":clip(item.trim(), 200),"valid":false,
                    "problem":"not a path:line citation"}));
            }
            for c in cites {
                let cite = if c.end == c.line {
                    format!("{}:{}", c.path, c.line)
                } else {
                    format!("{}:{}-{}", c.path, c.line, c.end)
                };
                let problem = tree.problem(&c.path, c.line, c.end);
                evidence.push(json!({"cite":cite,"valid":problem.is_none(),"problem":problem}));
            }
        }
        let holds = evidence.iter().any(|e| e["valid"] == true);
        out["evidence"] = json!(evidence);
        if holds {
            held.push(out);
        } else {
            out["reason"] = json!("none of its evidence holds in the working tree");
            checked.flagged.push(out);
        }
    }
    held.sort_by(|a, b| {
        let key = |c: &Value| {
            (
                severity_rank(c["severity"].as_str().unwrap_or("")),
                c["path"].as_str().unwrap_or("").to_string(),
                c["line"].as_u64().unwrap_or(0),
            )
        };
        key(a).cmp(&key(b))
    });
    for mut comment in held {
        if checked.kept.len() < max {
            checked.kept.push(comment);
        } else {
            comment["reason"] = json!("over --max-comments");
            checked.dropped.push(comment);
        }
    }
    checked
}

/// The lines of the pack's architecture for the modules that hold the
/// changed `paths`, and the dependencies between folders that involve them.
pub fn affected_modules(pack: &Value, paths: &[&str]) -> Vec<String> {
    let architecture: String = pack["sections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["name"] == "architecture")
        .flat_map(|s| s["chunks"].as_array().into_iter().flatten())
        .filter_map(|c| c["text"].as_str())
        .collect();
    let holds = |module: &str, path: &str| {
        if module.is_empty() {
            !path.contains('/')
        } else {
            path.starts_with(module) && path[module.len()..].starts_with('/')
        }
    };
    let mut modules: BTreeSet<&str> = BTreeSet::new();
    let mut lines: Vec<String> = Vec::new();
    for line in architecture.lines() {
        if let Some(rest) = line.strip_prefix("- ")
            && let Some((module, _)) = rest.split_once("/: ")
            && paths.iter().any(|path| holds(module, path))
        {
            modules.insert(module);
            lines.push(line.to_string());
        }
    }
    for line in architecture.lines() {
        if let Some(rest) = line.strip_prefix("- ")
            && let Some((from, tail)) = rest.split_once("/ -> ")
            && let Some((to, _)) = tail.split_once("/ (")
            && (modules.contains(from) || modules.contains(to))
        {
            lines.push(line.to_string());
        }
    }
    lines.truncate(MODULE_LINES);
    lines
}

/// The question the pack's ranking reads: the changed files and the
/// function context of their hunks.
fn ranking_question(files: &[FileDiff]) -> String {
    let mut question = String::from("Review a change to ");
    for (i, file) in files.iter().filter(|f| f.path.is_some()).enumerate() {
        if i > 0 {
            question.push_str(", ");
        }
        question.push_str(file.name());
        let contexts: BTreeSet<&str> = file
            .hunks
            .iter()
            .map(|h| h.context.as_str())
            .filter(|c| !c.is_empty())
            .collect();
        if !contexts.is_empty() {
            let shown: Vec<&str> = contexts.into_iter().take(3).collect();
            question.push_str(&format!(" ({})", shown.join("; ")));
        }
        if question.len() > QUESTION_CAP {
            break;
        }
    }
    clip(&question, QUESTION_CAP).to_string()
}

/// The changed hunks, one line per file, for the request and the report.
fn hunk_lines(files: &[FileDiff]) -> String {
    let mut out = String::new();
    for file in files {
        let ranges: Vec<String> = file
            .hunks
            .iter()
            .filter_map(|h| {
                let (a, b) = h.head_range()?;
                Some(if h.context.is_empty() {
                    format!("{a}-{b}")
                } else {
                    format!("{a}-{b} ({})", clip(&h.context, 120))
                })
            })
            .collect();
        let status = match (file.status(), &file.old_path) {
            ("renamed", Some(old)) => format!("renamed from {old}"),
            (status, _) => status.to_string(),
        };
        let binary = if file.binary { ", binary" } else { "" };
        out.push_str(&format!(
            "- {} ({status}{binary}, +{} -{}): {}\n",
            file.name(),
            file.added,
            file.removed,
            if ranges.is_empty() {
                "no lines to comment on".to_string()
            } else {
                ranges.join(", ")
            }
        ));
    }
    out
}

/// The changed hunks the pack pins at the top of its ranking.
fn focus_of(files: &[FileDiff]) -> Value {
    let passages: Vec<Value> = files
        .iter()
        .filter_map(|f| Some((f.path.as_deref()?, f)))
        .flat_map(|(path, f)| {
            f.hunks.iter().filter_map(move |h| {
                let (a, b) = h.head_range()?;
                Some(json!({"path":path,"start_line":a,"end_line":b}))
            })
        })
        .take(crate::code_pack::MAX_PINNED)
        .collect();
    json!(passages)
}

/// What the pack's change part holds: the changed symbols its focus names,
/// the call-path lines, the callers' and callees' excerpts and the tests.
fn context_of(pack: &Value, changed: &[&str]) -> Value {
    let mut symbols: BTreeSet<String> = BTreeSet::new();
    let mut call_lines = 0;
    let mut neighbours: Vec<Value> = Vec::new();
    let mut tests: BTreeSet<String> = BTreeSet::new();
    for section in pack["sections"].as_array().into_iter().flatten() {
        for chunk in section["chunks"].as_array().into_iter().flatten() {
            let why = chunk["why"].as_str().unwrap_or("");
            let path = chunk["path"].as_str();
            match section["name"].as_str().unwrap_or("") {
                "focus" if path.is_some_and(|p| changed.contains(&p)) => {
                    if let Some((_, names)) = why.split_once("defines ") {
                        symbols.extend(names.split(", ").map(|n| n.trim().to_string()));
                    }
                }
                "paths" => match path {
                    None => {
                        call_lines += chunk["text"]
                            .as_str()
                            .unwrap_or("")
                            .lines()
                            .filter(|l| l.contains(" -> "))
                            .count();
                    }
                    Some(path) => {
                        neighbours.push(json!({"path":path,"start_line":chunk["start_line"],
                            "end_line":chunk["end_line"],"why":why}));
                    }
                },
                "tests" => {
                    if let Some(path) = path {
                        tests.insert(path.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    json!({"changed_symbols":symbols,"call_path_lines":call_lines,
        "callers_and_callees":neighbours,"tests":tests})
}

/// `ref`'s commit in `folder`.
fn commit_of(git: &Git, folder: &Path, reference: &str) -> Result<String> {
    if reference.is_empty() || reference.starts_with('-') {
        return Err(Error::bad(format!("{reference:?} is not a git reference")));
    }
    let spec = format!("{reference}^{{commit}}");
    git.run(folder, &["rev-parse", "--verify", "--quiet", &spec])
        .map(|sha| sha.trim().to_string())
        .map_err(|_| Error::bad(format!("No commit {reference} in {}", folder.display())))
}

/// `--base` when it is left out: the remote's default branch, else a local
/// main or master.
fn default_base(git: &Git, folder: &Path) -> Result<(String, String)> {
    for candidate in [
        "origin/HEAD",
        "origin/main",
        "origin/master",
        "main",
        "master",
    ] {
        if let Ok(sha) = commit_of(git, folder, candidate) {
            return Ok((candidate.to_string(), sha));
        }
    }
    Err(Error::bad(
        "Name the base to review against with --base REF (no origin/HEAD, main or master here)",
    ))
}

/// GitHub turns `@name` into a mention that notifies; the model's text never
/// mentions anyone (a word joiner after each `@` that starts a name).
fn quiet_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '@' && chars.peek().is_some_and(|n| n.is_ascii_alphanumeric()) {
            out.push('\u{2060}');
        }
    }
    out
}

/// `text` with `&`, `<` and `>` as HTML entities outside Markdown code, which
/// stays as it is: fenced code blocks (``` or ~~~) and code spans (a run of
/// backticks up to the next run of the same length in its paragraph).
/// GitHub renders raw HTML in a comment (an `<img>`, a `<!--` that hides the
/// rest), so the model's text shows as it was written.
fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if (i == 0 || text.as_bytes()[i - 1] == b'\n')
            && let Some(end) = fence_end(text, i)
        {
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        if c == '`' {
            let run = backticks(&text[i..]);
            let block = paragraph_end(text, i);
            // A run with no closing run of its length is literal backticks.
            let mut end = i + run;
            let mut from = i + run;
            while let Some(at) = text[from..block].find('`').map(|at| from + at) {
                let len = backticks(&text[at..]);
                if len == run {
                    end = at + len;
                    break;
                }
                from = at + len;
            }
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
        i += c.len_utf8();
    }
    out
}

fn backticks(text: &str) -> usize {
    text.bytes().take_while(|&b| b == b'`').count()
}

/// Where the paragraph that holds `from` ends: at the line break before the
/// next blank line, else at the end of `text`.
fn paragraph_end(text: &str, from: usize) -> usize {
    let mut at = from;
    while let Some(n) = text[at..].find('\n').map(|n| at + n) {
        let next = &text[n + 1..];
        if next[..next.find('\n').unwrap_or(next.len())]
            .trim()
            .is_empty()
        {
            return n;
        }
        at = n + 1;
    }
    text.len()
}

/// The end of the fenced code block that opens on the line at `start` (up to
/// three spaces, then three or more backticks or tildes): after its closing
/// fence, else the end of `text`. `None` when no fence opens there.
fn fence_end(text: &str, start: usize) -> Option<usize> {
    let line_end = |from: usize| text[from..].find('\n').map_or(text.len(), |n| from + n + 1);
    // The run of `mark` that a line opens with, and where it ends.
    let fence = |line: &str, mark: char| -> Option<(usize, usize)> {
        let line = line.trim_end_matches(['\n', '\r']);
        let indent = line.len() - line.trim_start_matches(' ').len();
        let rest = &line[indent..];
        let run = rest.len() - rest.trim_start_matches(mark).len();
        (indent <= 3 && run >= 3).then_some((run, indent + run))
    };
    let first = &text[start..line_end(start)];
    let mark = first.trim_start_matches(' ').chars().next()?;
    if mark != '`' && mark != '~' {
        return None;
    }
    let (run, info) = fence(first, mark)?;
    // A backtick fence's info string holds no backtick (that is a code span).
    if mark == '`' && first[info..].contains('`') {
        return None;
    }
    let mut at = line_end(start);
    while at < text.len() {
        let end = line_end(at);
        let line = &text[at..end];
        if let Some((close, after)) = fence(line, mark)
            && close >= run
            && line[after..].trim().is_empty()
        {
            return Some(end);
        }
        at = end;
    }
    Some(text.len())
}

fn cite_list(comment: &Value, valid: bool) -> Vec<String> {
    comment["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| (e["valid"] == true) == valid)
        .filter_map(|e| e["cite"].as_str())
        .map(str::to_string)
        .collect()
}

/// A kept comment as GitHub shows it.
fn github_comment_body(comment: &Value) -> String {
    let cites: Vec<String> = cite_list(comment, true)
        .into_iter()
        .map(|c| format!("`{c}`"))
        .collect();
    quiet_mentions(&escape_markup(&format!(
        "**{}**: {}\n\nEvidence: {}",
        comment["severity"].as_str().unwrap_or("minor"),
        comment["body"].as_str().unwrap_or(""),
        cites.join(", ")
    )))
}

/// The body of GitHub's create-a-review call: a `COMMENT` review at the
/// head, its comments at `prefix` (the project's folder in its repository)
/// plus their paths.
pub fn github_review(report: &Value, prefix: &str) -> Value {
    let comments: Vec<Value> = report["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            json!({"path":format!("{prefix}{}", c["path"].as_str().unwrap_or("")),
                "line":c["line"],"side":"RIGHT","body":github_comment_body(c)})
        })
        .collect();
    let count = |key: &str| report[key].as_array().map_or(0, Vec::len);
    let mut body = format!(
        "**Graphicle review** of `{}..{}`",
        short(report["merge_base"].as_str().unwrap_or("")),
        short(report["head"].as_str().unwrap_or(""))
    );
    if let Some(model) = report["model"].as_str() {
        body.push_str(&format!(" with {model}"));
    }
    body.push_str(&format!(
        ".\n\n{}",
        report["summary"].as_str().unwrap_or("")
    ));
    if report["result"] == "reviewed" {
        body.push_str(&format!(
            "\n\n{} inline comment(s); {} withheld because none of their evidence holds; {} dropped (outside the changed lines, repeated, or over the cap).",
            comments.len(),
            count("flagged"),
            count("dropped")
        ));
    }
    if let Some(line) = report["usage_line"].as_str() {
        body.push_str(&format!("\n\nUsage: {line}"));
    }
    json!({"commit_id":report["head"],"event":"COMMENT",
        "body":clip(&quiet_mentions(&escape_markup(&body)), 60_000),"comments":comments})
}

/// The report for a terminal.
pub fn markdown(report: &Value) -> String {
    let mut out = format!(
        "# Review of {}..{}\n\n",
        short(report["merge_base"].as_str().unwrap_or("")),
        short(report["head"].as_str().unwrap_or(""))
    );
    if let Some(files) = report["files"].as_array() {
        out.push_str(&format!("{} file(s) changed.\n\n", files.len()));
    }
    if let Some(summary) = report["summary"].as_str().filter(|s| !s.is_empty()) {
        out.push_str(summary);
        out.push_str("\n\n");
    }
    let show = |out: &mut String, title: &str, list: &Value, why: bool| {
        let Some(list) = list.as_array().filter(|l| !l.is_empty()) else {
            return;
        };
        out.push_str(&format!("## {title} ({})\n\n", list.len()));
        for c in list {
            out.push_str(&format!(
                "- **{}** `{}:{}`: {}\n",
                c["severity"].as_str().unwrap_or("minor"),
                c["path"].as_str().unwrap_or(""),
                c["line"],
                c["body"].as_str().unwrap_or("").replace('\n', "\n  ")
            ));
            let valid = cite_list(c, true);
            if !valid.is_empty() {
                out.push_str(&format!("  Evidence: {}\n", valid.join(", ")));
            }
            let invalid = cite_list(c, false);
            if !invalid.is_empty() {
                out.push_str(&format!(
                    "  Evidence that does not hold: {}\n",
                    invalid.join(", ")
                ));
            }
            if why && let Some(reason) = c["reason"].as_str() {
                out.push_str(&format!("  ({reason})\n"));
            }
        }
        out.push('\n');
    };
    show(&mut out, "Comments", &report["comments"], false);
    show(
        &mut out,
        "Flagged: no evidence that holds, not posted",
        &report["flagged"],
        false,
    );
    show(&mut out, "Dropped", &report["dropped"], true);
    if report["comments"].as_array().is_some_and(Vec::is_empty) && report["result"] == "reviewed" {
        out.push_str("No comments to post.\n\n");
    }
    if let Some(line) = report["usage_line"].as_str() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn files_json(files: &[FileDiff]) -> Value {
    json!(
        files
            .iter()
            .map(|f| json!({"path":f.path,"old_path":f.old_path,"status":f.status(),
                "binary":f.binary,"added":f.added,"removed":f.removed,
                "hunks":f.hunks.iter().filter_map(Hunk::head_range).map(|(a, b)| json!([a, b])).collect::<Vec<_>>()}))
            .collect::<Vec<_>>()
    )
}

fn output(report: &Value, format: Format, prefix: &str) -> Result<()> {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(report)?),
        Format::Github => println!(
            "{}",
            serde_json::to_string_pretty(&github_review(report, prefix))?
        ),
        Format::Markdown => print!("{}", markdown(report)),
    }
    Ok(())
}

/// `graphicle review`.
pub async fn review(run: Run<'_>, args: Args) -> Result<()> {
    let format = Format::of(args.format.as_deref());
    let human = format == Format::Markdown && crate::terminal::human(run.json);
    let (name, profile) = long_context::profile(&run)?;
    let folder = run.folder.clone().ok_or_else(|| {
        Error::bad("review reads a project's own folder, a git working tree: sync one first")
    })?;
    let git = Git::new(&run.store.directory)?;
    let head_ref = args.head.as_deref().unwrap_or("HEAD");
    let head = commit_of(&git, &folder, head_ref)?;
    let checked_out = commit_of(&git, &folder, "HEAD")?;
    if head != checked_out {
        return Err(Error::bad(format!(
            "review reads the project's synced files, which are the folder's HEAD ({}): check out {head_ref} there and sync it, then review",
            short(&checked_out)
        )));
    }
    let (base_ref, base) = match &args.base {
        Some(reference) => (reference.clone(), commit_of(&git, &folder, reference)?),
        None => default_base(&git, &folder)?,
    };
    let merge_base = git
        .run(&folder, &["merge-base", &base, &head])
        .map_err(|_| {
            Error::bad(format!(
                "{base_ref} and {head_ref} have no merge base here: fetch the base's history (in a GitHub workflow, actions/checkout with fetch-depth: 0)"
            ))
        })?
        .trim()
        .to_string();
    let prefix = git
        .run(&folder, &["rev-parse", "--show-prefix"])?
        .trim()
        .to_string();
    let diff = git.run(
        &folder,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--relative",
            "--find-renames",
            "--unified=3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &merge_base,
            &head,
            "--",
        ],
    )?;
    let files = parse_diff(&diff);
    if human
        && git
            .run(&folder, &["status", "--porcelain", "--untracked-files=no"])
            .is_ok_and(|s| !s.trim().is_empty())
    {
        eprintln!(
            "graphicle: the folder has uncommitted changes; the diff reads commits, the pack the synced files"
        );
    }
    let mut report = json!({"result":"reviewed","base":base_ref,"base_commit":base,
        "merge_base":merge_base,"head":head,"files":files_json(&files),"summary":"",
        "comments":[],"flagged":[],"dropped":[]});
    if files.is_empty() {
        report["result"] = json!("no changes");
        report["summary"] = json!(format!(
            "No changes between {} and {}.",
            short(&merge_base),
            short(&head)
        ));
        return output(&report, format, &prefix);
    }
    // No head line to comment on (deleted, renamed or binary files only): no call.
    let focus = focus_of(&files);
    if focus.as_array().is_none_or(Vec::is_empty) {
        report["result"] = json!("nothing to review");
        report["summary"] = json!(
            "The change adds or modifies no lines a review can comment on: it deletes, renames or changes binary files only."
        );
        return output(&report, format, &prefix);
    }
    // The change's own text, after the pack: the affected modules (from the
    // pack's architecture), the changed hunks and the diff.
    let cut = diff.len() > DIFF_CAP;
    let shown = clip(&diff, DIFF_CAP);
    let rest = format!(
        "Changed hunks of {}..{} (path, status, lines added and removed, then the head's line ranges a comment may name):\n{}\n<diff>\n{shown}{}</diff>\n{}Review the change.\n",
        short(&merge_base),
        short(&head),
        hunk_lines(&files),
        if shown.ends_with('\n') { "" } else { "\n" },
        if cut {
            format!(
                "The diff is cut at {DIFF_CAP} of its {} bytes; the hunks above list every file.\n",
                diff.len()
            )
        } else {
            String::new()
        }
    );
    let changed: Vec<&str> = files.iter().filter_map(|f| f.path.as_deref()).collect();
    let user_of = |pack: &Value| {
        let modules = affected_modules(pack, &changed);
        let modules = if modules.is_empty() {
            String::new()
        } else {
            format!(
                "Architecture of the modules the change touches:\n{}\n\n",
                modules.join("\n")
            )
        };
        format!("{}\n{modules}{rest}", crate::code_pack::text(pack))
    };
    let question = ranking_question(&files);
    let schema = review_schema();
    let request = Request {
        question: &question,
        rest: &rest,
        instructions: REVIEW_INSTRUCTIONS,
        user_of: &user_of,
        schema: Some(&schema),
    };
    let ratio = long_context::calibration(run.store, &name, &profile);
    let rest_tokens = crate::code_pack::estimate::tokens(
        rest.len(),
        crate::code_pack::estimate::PACK_TEXT_BYTES_PER_TOKEN,
    ) + MODULES_ROOM;
    let room = |budget: usize, less: usize| {
        if run.budget.is_none() {
            budget
                .saturating_sub(less)
                .max(crate::code_pack::MIN_BUDGET)
        } else {
            budget
        }
    };
    let budget = room(
        long_context::budget_with(&profile, run.budget, ratio)?,
        rest_tokens,
    );
    let nominal = room(long_context::budget_for(&profile, run.budget)?, REVIEW_ROOM);
    let repack = |budget: usize| {
        let mut args = long_context::pack_args(&run, &profile, &question, budget, Some(nominal));
        args["focus"] = focus.clone();
        run.store
            .project_pack(&run.session.workspace, &run.session.capability, &args)
    };
    let pack = repack(budget)?;
    let (pack, _, counted) =
        long_context::fit_with(&profile, &request, pack, budget, ratio, human, &repack).await?;
    if human {
        long_context::tell_packed(&pack, &name, &profile);
    }
    let (result, _) = long_context::send(
        &run,
        "review",
        &name,
        &profile,
        &pack,
        &request,
        counted,
        ratio,
        &mut |_| {},
    )
    .await?;
    let done = result?;
    let reply = long_context::json_in(&done.text)?;
    if !reply.is_object() {
        return Err(Error::bad("The model's review is not a JSON object"));
    }
    let mut tree = Tree::new(Some(folder.clone()));
    let checked = check(&reply, &files, &mut tree, args.max_comments as usize);
    let usage = long_context::usage_json(&done, profile.price.as_ref());
    report["summary"] = json!(clip(
        reply["summary"].as_str().unwrap_or("").trim(),
        SUMMARY_CAP
    ));
    report["comments"] = json!(checked.kept);
    report["flagged"] = json!(checked.flagged);
    report["dropped"] = json!(checked.dropped);
    report["model"] = json!(done.model);
    report["profile"] = json!(name);
    report["usage_line"] = json!(crate::spend::line(&usage));
    report["usage"] = usage;
    report["prompt_cache_key"] =
        json!(model::prompt_cache_on(&profile).then(|| long_context::cache_key(&pack)));
    report["input_tokens_counted"] = json!(counted);
    report["diff_cut"] = json!(cut);
    report["context"] = context_of(&pack, &changed);
    report["pack"] = crate::code_pack::summary(&pack);
    output(&report, format, &prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/answer.go b/src/answer.go
index 1111111..2222222 100644
--- a/src/answer.go
+++ b/src/answer.go
@@ -3,4 +3,5 @@ package answer
 // Answer is the answer.
 func Answer() int {
-\treturn 41
+\tvalue := 42
+\treturn value
 }
@@ -20,2 +21,3 @@ func Other() int {
 \ta := 1
+-- not a header
 \treturn a
diff --git a/docs/new.md b/docs/new.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/docs/new.md
@@ -0,0 +1,2 @@
+# New
+--- a list item, not a header
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 4444444..0000000
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
--- gone
\\ No newline at end of file
diff --git a/a b.rs b/c d.rs
similarity index 90%
rename from a b.rs
rename to c d.rs
index 5555555..6666666 100644
--- a/a b.rs\t
+++ b/c d.rs\t
@@ -1 +1 @@
-fn a() {}
+fn c() {}
diff --git a/logo.png b/logo.png
index 7777777..8888888 100644
Binary files a/logo.png and b/logo.png differ
diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"
index 9999999..aaaaaaa 100644
--- \"a/caf\\303\\251.txt\"
+++ \"b/caf\\303\\251.txt\"
@@ -1 +1,2 @@
 one
+two
";

    #[test]
    fn a_diff_parses_into_files_hunks_and_counts() {
        let files = parse_diff(DIFF);
        let names: Vec<&str> = files.iter().map(FileDiff::name).collect();
        assert_eq!(
            names,
            [
                "src/answer.go",
                "docs/new.md",
                "old.txt",
                "c d.rs",
                "logo.png",
                "café.txt"
            ]
        );
        let statuses: Vec<&str> = files.iter().map(FileDiff::status).collect();
        assert_eq!(
            statuses,
            [
                "modified", "added", "deleted", "renamed", "modified", "modified"
            ]
        );
        let answer = &files[0];
        assert_eq!(answer.hunks.len(), 2);
        assert_eq!(answer.hunks[0].head_range(), Some((3, 7)));
        assert_eq!(answer.hunks[0].context, "package answer");
        assert_eq!(answer.hunks[1].head_range(), Some((21, 23)));
        // "-- not a header" and "--- a list item" are content, not headers.
        assert_eq!((answer.added, answer.removed), (3, 1));
        assert_eq!((files[1].added, files[1].removed), (2, 0));
        assert_eq!(files[1].old_path, None);
        assert_eq!(files[2].path, None);
        // A removed line "-- gone" prints as "--- gone": content, not a header.
        assert_eq!((files[2].added, files[2].removed), (0, 1));
        assert_eq!(files[2].hunks[0].head_range(), None);
        assert_eq!(files[3].old_path.as_deref(), Some("a b.rs"));
        assert!(files[4].binary && files[4].hunks.is_empty());
        assert!(answer.in_hunks(3) && answer.in_hunks(7) && answer.in_hunks(22));
        assert!(!answer.in_hunks(2) && !answer.in_hunks(8) && !answer.in_hunks(24));
    }

    #[test]
    fn quoting_and_headers_without_paths_lines() {
        assert_eq!(unquote("\"a\\tb\\\\c\\\"\"").as_deref(), Some("a\tb\\c\""));
        assert_eq!(unquote("plain"), None);
        assert_eq!(
            header_paths("a/x y.rs b/x y.rs"),
            Some(("x y.rs".into(), "x y.rs".into()))
        );
        assert_eq!(
            header_paths("\"a/\\303\\251\" \"b/\\303\\251\""),
            Some(("é".into(), "é".into()))
        );
    }

    fn tree_with(files: &[(&str, usize)]) -> (tempfile::TempDir, Tree) {
        let dir = tempfile::TempDir::new().unwrap();
        for (path, lines) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, "x\n".repeat(*lines)).unwrap();
        }
        let tree = Tree::new(Some(dir.path().to_path_buf()));
        (dir, tree)
    }

    #[test]
    fn comments_are_kept_flagged_or_dropped() {
        let files = parse_diff(DIFF);
        let (_dir, mut tree) = tree_with(&[("src/answer.go", 30), ("src/use.go", 10)]);
        let reply = json!({"summary":"s","comments":[
            {"path":"src/answer.go","line":5,"severity":"minor","body":"Name the constant.",
             "evidence":["src/answer.go:4-6"]},
            {"path":"b/src/answer.go","line":6,"severity":"BLOCKER","body":"Callers expect 41.",
             "evidence":["src/use.go:3", "nowhere.go:1"]},
            {"path":"src/answer.go","line":5,"severity":"minor","body":"Name the constant.",
             "evidence":["src/answer.go:4"]},
            {"path":"src/answer.go","line":12,"severity":"major","body":"Outside.","evidence":["src/answer.go:12"]},
            {"path":"old.txt","line":1,"severity":"major","body":"Deleted.","evidence":["src/answer.go:1"]},
            {"path":"src/answer.go","line":22,"severity":"nit","body":"Unsupported.",
             "evidence":["src/answer.go:99","see the docs"]},
            {"path":"src/answer.go","line":4,"severity":"major","body":"","evidence":[]}
        ]});
        let checked = check(&reply, &files, &mut tree, 20);
        let kept: Vec<(&str, u64, &str)> = checked
            .kept
            .iter()
            .map(|c| {
                (
                    c["path"].as_str().unwrap(),
                    c["line"].as_u64().unwrap(),
                    c["severity"].as_str().unwrap(),
                )
            })
            .collect();
        // The blocker first; `b/` taken off its path; one invalid citation
        // beside a valid one keeps it.
        assert_eq!(
            kept,
            [
                ("src/answer.go", 6, "blocker"),
                ("src/answer.go", 5, "minor")
            ]
        );
        assert_eq!(checked.kept[0]["evidence"][1]["valid"], false);
        assert_eq!(checked.flagged.len(), 1);
        assert_eq!(checked.flagged[0]["line"], 22);
        let reasons: Vec<&str> = checked
            .dropped
            .iter()
            .map(|c| c["reason"].as_str().unwrap())
            .collect();
        assert_eq!(reasons.len(), 4, "{reasons:?}");
        assert!(reasons.contains(&"a repeat of an earlier comment"));
        assert!(reasons.contains(&"line 12 is outside the changed hunks of src/answer.go"));
        assert!(reasons.contains(&"old.txt is not a file the change adds or modifies"));
        assert!(reasons.contains(&"the comment is empty"));
        // The cap keeps the most severe.
        let capped = check(&reply, &files, &mut tree, 1);
        assert_eq!(capped.kept.len(), 1);
        assert_eq!(capped.kept[0]["severity"], "blocker");
        assert!(
            capped
                .dropped
                .iter()
                .any(|c| c["reason"] == "over --max-comments")
        );
    }

    #[test]
    fn the_github_review_comments_on_the_head_and_mentions_no_one() {
        let report = json!({"result":"reviewed","merge_base":"0123456789abcdef","head":"fedcba9876543210ffff",
            "model":"muse-spark-1.3","summary":"Looks risky, cc @octocat.",
            "comments":[{"path":"src/answer.go","line":6,"severity":"major","body":"Ask @admin first.",
                "evidence":[{"cite":"src/use.go:3","valid":true},{"cite":"x.go:1","valid":false}]}],
            "flagged":[{"path":"src/answer.go","line":22}],"dropped":[],
            "usage_line":"1,000 tokens in, 10 out; 1.0 s."});
        let review = github_review(&report, "services/api/");
        assert_eq!(review["event"], "COMMENT");
        assert_eq!(review["commit_id"], "fedcba9876543210ffff");
        let comment = &review["comments"][0];
        assert_eq!(comment["path"], "services/api/src/answer.go");
        assert_eq!(comment["line"], 6);
        assert_eq!(comment["side"], "RIGHT");
        let body = comment["body"].as_str().unwrap();
        assert!(
            body.starts_with("**major**: Ask @\u{2060}admin first."),
            "{body}"
        );
        assert!(body.contains("`src/use.go:3`") && !body.contains("x.go:1"));
        let summary = review["body"].as_str().unwrap();
        assert!(summary.contains("@\u{2060}octocat") && !summary.contains("@octocat"));
        assert!(summary.contains("1 inline comment(s); 1 withheld"));
    }

    /// The review's body, its comments and so the step summary (the body)
    /// show the model's markup as text: `&`, `<` and `>` are escaped outside
    /// code spans, which stay as they are.
    #[test]
    fn the_github_review_escapes_markup_outside_code_spans() {
        let report = json!({"result":"reviewed","merge_base":"0123456789abcdef","head":"fedcba9876543210ffff",
            "summary":"<img src=x><!--h--> and `a < b && c > d`.",
            "comments":[{"path":"web/page.html","line":6,"severity":"major",
                "body":"<img src=x><!--h--> R&D: keep ``Vec<u8> & `x` `` and `if a<b {}`; a lone ` <b>",
                "evidence":[{"cite":"web/page.html:3","valid":true}]}],
            "flagged":[],"dropped":[]});
        let review = github_review(&report, "");
        assert_eq!(
            review["comments"][0]["body"],
            "**major**: &lt;img src=x&gt;&lt;!--h--&gt; R&amp;D: keep ``Vec<u8> & `x` `` and `if a<b {}`; a lone ` &lt;b&gt;\n\nEvidence: `web/page.html:3`"
        );
        let summary = review["body"].as_str().unwrap();
        assert!(
            summary.contains(".\n\n&lt;img src=x&gt;&lt;!--h--&gt; and `a < b && c > d`."),
            "{summary}"
        );
        assert_eq!(
            escape_markup("<img src=x><!--h-->"),
            "&lt;img src=x&gt;&lt;!--h--&gt;"
        );
        assert_eq!(escape_markup("`<b>` & ```<i>```"), "`<b>` &amp; ```<i>```");
        // A code span ends with its paragraph; a fenced block may hold blank lines.
        assert_eq!(
            escape_markup("a ` b <i>\n\n`c` <u>"),
            "a ` b &lt;i&gt;\n\n`c` &lt;u&gt;"
        );
        assert_eq!(
            escape_markup("```suggestion\nif a < b {\n\n    c >> 1;\n}\n```\n<b>"),
            "```suggestion\nif a < b {\n\n    c >> 1;\n}\n```\n&lt;b&gt;"
        );
        assert_eq!(escape_markup("~~~\n<x>\n"), "~~~\n<x>\n");
    }

    #[test]
    fn affected_modules_come_from_the_architecture() {
        let pack = json!({"sections":[{"name":"architecture","part":"prefix","chunks":[{"text":
            "3 files.\n\n- src/: 2 files, ~100 tokens, Go. The answer.\n- docs/: 1 files, ~10 tokens\n- /: 1 files, ~5 tokens\n\nDependencies between folders (edges from the key symbols, most first):\n- src/ -> lib/ (4)\n- docs/ -> lib/ (1)\n"}]}]});
        let lines = affected_modules(&pack, &["src/answer.go", "README.md"]);
        assert_eq!(
            lines,
            [
                "- src/: 2 files, ~100 tokens, Go. The answer.",
                "- /: 1 files, ~5 tokens",
                "- src/ -> lib/ (4)"
            ]
        );
    }

    #[test]
    fn the_ranking_question_names_the_changed_files() {
        let question = ranking_question(&parse_diff(DIFF));
        assert!(
            question.starts_with(
                "Review a change to src/answer.go (func Other() int {; package answer)"
            ),
            "{question}"
        );
        assert!(!question.contains("old.txt"));
        let focus = focus_of(&parse_diff(DIFF));
        assert_eq!(
            focus[0],
            json!({"path":"src/answer.go","start_line":3,"end_line":7})
        );
        assert!(
            focus
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["path"] != "old.txt")
        );
    }
}
