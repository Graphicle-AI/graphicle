//! How old each excerpt of a `context` packet is, said in its citation.
//!
//! Imported notes and documents are plain sources: they carry no review
//! state, so the stale flags of reviewed memory never reach them. Before a
//! packet is packed, [`annotate`] gives every repository-file excerpt (with
//! `source_age: true`, or when the call leaves it out and turns
//! `notes_first` on):
//! - the date of its file's last change (`changed`, `changed_at`,
//!   `changed_basis`): the last commit that touched the file when Git
//!   tracks it and the working tree has no change to it; otherwise the
//!   file's modification time. The packer prints it in the citation.
//! - `may be stale: <file> (defines <name>) changed <date>` when the excerpt
//!   names code (a linked mention of `doc_links` written as code,
//!   [`code_shaped`]) whose file changed after the excerpt's file last did.
//!   A file's date is never older than any of its lines, so the excerpt's
//!   text is older than that change. The date is the file's: the change may
//!   not touch the named definition itself.
//!
//! Excerpts are not compared with one another: no excerpt is marked as
//! replaced by another, and the packet keeps the ranking's order.
//!
//! Dates come from the folder the project was last synced from; a project
//! with no recorded folder (one synced over HTTP) gets no dates and no
//! marks. Git runs only for the packet's files, within [`GIT_BUDGET`]; a
//! file Git has not dated by then keeps no date.
use crate::Result;
use crate::store::s;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::Path;
use std::time::{Duration, Instant};

/// The longest a packet waits for `git log` to date its files.
pub const GIT_BUDGET: Duration = Duration::from_millis(1500);
/// Code names marked per excerpt; the rest are counted.
const MAX_STALE_NAMES: usize = 3;

/// When a file last changed and how that is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileDate {
    /// Seconds since the Unix epoch.
    pub at: i64,
    /// `git` (the last commit that touched it) or `mtime`.
    pub basis: &'static str,
}

/// `2026-10-15 07:22 UTC`.
pub fn display(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

fn rfc3339(at: i64) -> Value {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| json!(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
        .unwrap_or(Value::Null)
}

fn git(root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["--literal-pathspecs", "-c", "core.quotePath=false"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// The paths `git <args> -- <paths>` prints, NUL-separated; `None` when Git
/// fails.
fn listed(root: &Path, args: &[&str], paths: &BTreeSet<String>) -> Option<BTreeSet<String>> {
    let output = git(root).args(args).arg("--").args(paths).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .filter_map(|p| std::str::from_utf8(p).ok())
            .map(str::to_string)
            .collect(),
    )
}

/// The last commit date of each of `paths` (relative to `root`, tracked and
/// unchanged in the working tree): one `git log` walk, stopped once every
/// path is dated or `budget` has passed.
fn commit_dates(root: &Path, paths: &BTreeSet<String>, budget: Duration) -> BTreeMap<String, i64> {
    let mut found = BTreeMap::new();
    // No pathspec would make `git log` list every file of every commit.
    if paths.is_empty() {
        return found;
    }
    let Ok(mut child) = git(root)
        .args([
            "log",
            "--relative",
            "--no-renames",
            "--format=%x01%ct",
            "--name-only",
            "--",
        ])
        .args(paths)
        .stdout(std::process::Stdio::piped())
        .spawn()
    else {
        return found;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return found;
    };
    let (sender, receiver) = std::sync::mpsc::channel::<(String, i64)>();
    let reader = std::thread::spawn(move || {
        let mut at = None;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(stamp) = line.strip_prefix('\u{1}') {
                at = stamp.trim().parse::<i64>().ok();
            } else if let Some(at) = at
                && !line.is_empty()
                && sender.send((line, at)).is_err()
            {
                break;
            }
        }
    });
    let deadline = Instant::now() + budget;
    while found.len() < paths.len() {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match receiver.recv_timeout(left) {
            Ok((path, at)) => {
                if paths.contains(&path) {
                    found.entry(path).or_insert(at);
                }
            }
            Err(_) => break,
        }
    }
    drop(receiver);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    found
}

/// When each of `paths` (relative to `root`) last changed: Git's last
/// commit date for a tracked file the working tree has not changed, else
/// the file's modification time. A path with neither is left out.
pub fn file_dates(
    root: &Path,
    paths: &BTreeSet<String>,
    budget: Duration,
) -> BTreeMap<String, FileDate> {
    let mut dates = BTreeMap::new();
    if paths.is_empty() {
        return dates;
    }
    let mut git_dated = BTreeSet::new();
    let has_head = git(root)
        .args(["rev-parse", "-q", "--verify", "HEAD"])
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if has_head
        && let Some(tracked) = listed(root, &["ls-files", "-z"], paths)
        && let Some(changed) = listed(
            root,
            &["diff", "--relative", "--name-only", "-z", "HEAD"],
            paths,
        )
    {
        let clean: BTreeSet<String> = tracked.difference(&changed).cloned().collect();
        for (path, at) in commit_dates(root, &clean, budget) {
            dates.insert(path.clone(), FileDate { at, basis: "git" });
            git_dated.insert(path);
        }
        // A tracked, unchanged file Git did not date in time keeps no date:
        // its modification time says when it was checked out, not changed.
        for path in clean {
            git_dated.insert(path);
        }
    }
    for path in paths {
        if git_dated.contains(path) {
            continue;
        }
        let modified = std::fs::symlink_metadata(root.join(path))
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        if let Some(since) = modified {
            dates.insert(
                path.clone(),
                FileDate {
                    at: since.as_secs() as i64,
                    basis: "mtime",
                },
            );
        }
    }
    dates
}

/// A mention that reads as code rather than a word: four characters or more
/// with `_`, `::`, `.`, `/` or `(`, or a capital inside it (`parseHeader`,
/// `DocLinks`). `ask` or `Context` may name a definition, but read as words.
fn code_shaped(mention: &str) -> bool {
    let inner_capital = mention
        .chars()
        .zip(mention.chars().skip(1))
        .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
    mention.chars().count() >= 4
        && (inner_capital || mention.contains(['_', '.', '/', '(']) || mention.contains("::"))
}

/// Date the packet's repository-file `hits` and mark those that name code
/// whose file changed later. `links` reads the project's document links and
/// names its folder. Returns the packet's `source_age` report.
pub fn annotate(links: &crate::doc_links::Lookup<'_>, hits: &mut [Value]) -> Result<Value> {
    let Some(root) = links.folder().filter(|r| r.is_dir()) else {
        return Ok(json!({"status":"no_folder"}));
    };
    let started = Instant::now();
    let files = |hit: &Value| {
        (hit["source"]["source_kind"] == "repository_file")
            .then(|| s(&hit["source"], "path").to_string())
            .filter(|p| !p.is_empty())
    };
    // The code each excerpt names, read before the dates so one Git walk
    // dates the excerpts' files and the named code's files together.
    let mut named: Vec<Vec<(String, String)>> = Vec::with_capacity(hits.len());
    for hit in hits.iter() {
        let source = &hit["source"];
        let mentions = match (
            files(hit),
            source["byte_start"].as_u64(),
            source["byte_end"].as_u64(),
        ) {
            (Some(_), Some(a), Some(b)) => {
                links.code_named_in(s(source, "id"), s(source, "sha256"), a as usize, b as usize)?
            }
            _ => Vec::new(),
        };
        named.push(mentions);
    }
    let mut paths: BTreeSet<String> = hits.iter().filter_map(files).collect();
    for mentions in &named {
        paths.extend(mentions.iter().map(|(_, path)| path.clone()));
    }
    let dates = file_dates(root, &paths, GIT_BUDGET);
    let mut by_basis: BTreeMap<&str, u64> = BTreeMap::new();
    let mut stale_marks = 0u64;
    for (hit, mentions) in hits.iter_mut().zip(&named) {
        let Some(path) = files(hit) else { continue };
        let Some(date) = dates.get(&path) else {
            continue;
        };
        *by_basis.entry(date.basis).or_default() += 1;
        let source = &mut hit["source"];
        source["changed"] = json!(display(date.at));
        source["changed_at"] = rfc3339(date.at);
        source["changed_basis"] = json!(date.basis);
        let mut marks = Vec::new();
        let mut seen = BTreeSet::new();
        for (mention, target) in mentions {
            if target == &path || !code_shaped(mention) || !seen.insert(mention.as_str()) {
                continue;
            }
            if let Some(code) = dates.get(target)
                && code.at > date.at
            {
                // The date is the defining file's, not the definition's.
                marks.push(json!(format!(
                    "may be stale: {target} (defines `{mention}`) changed {}",
                    display(code.at)
                )));
            }
        }
        let over = marks.len().saturating_sub(MAX_STALE_NAMES);
        marks.truncate(MAX_STALE_NAMES);
        if over > 0 {
            marks.push(json!(format!(
                "may be stale: {over} more names of code whose files changed since"
            )));
        }
        stale_marks += marks.len() as u64;
        if !marks.is_empty() {
            source["age_marks"] = json!(marks);
        }
    }
    let report = json!({"status":"dated","dated":by_basis.values().sum::<u64>(),"basis":by_basis,
        "stale_code_marks":stale_marks,"elapsed_ms":started.elapsed().as_millis() as u64});
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_code_shaped_mentions_are_marked() {
        for (mention, shaped) in [
            ("ask", false),
            ("Context", false),
            ("header", false),
            ("a_b", false),
            ("parse_header", true),
            ("parseHeader", true),
            ("DocLinks", true),
            ("wire.py::parse", true),
            ("reply()", true),
        ] {
            assert_eq!(code_shaped(mention), shaped, "{mention}");
        }
    }

    #[test]
    fn git_dates_tracked_files_and_mtime_dates_the_rest() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path();
        let run = |args: &[&str], date: &str| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        let commit = |message: &str, date: &str| {
            run(&["add", "-A"], date);
            run(
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-q",
                    "-m",
                    message,
                ],
                date,
            );
        };
        run(&["init", "-q"], "@0 +0000");
        std::fs::create_dir_all(root.join("notes")).unwrap();
        std::fs::write(root.join("notes/old.md"), "old\n").unwrap();
        std::fs::write(root.join("edited.md"), "one\n").unwrap();
        commit("first", "@1700000000 +0000");
        std::fs::write(root.join("new.md"), "new\n").unwrap();
        commit("second", "@1700003600 +0000");
        std::fs::write(root.join("edited.md"), "two\n").unwrap();
        std::fs::write(root.join("untracked.md"), "loose\n").unwrap();
        let paths: BTreeSet<String> = [
            "notes/old.md",
            "new.md",
            "edited.md",
            "untracked.md",
            "gone.md",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let dates = file_dates(root, &paths, GIT_BUDGET);
        assert_eq!(
            dates["notes/old.md"],
            FileDate {
                at: 1_700_000_000,
                basis: "git"
            }
        );
        assert_eq!(
            dates["new.md"],
            FileDate {
                at: 1_700_003_600,
                basis: "git"
            }
        );
        // Changed in the working tree, or never committed: the file's time.
        assert_eq!(dates["edited.md"].basis, "mtime");
        assert_eq!(dates["untracked.md"].basis, "mtime");
        assert!(!dates.contains_key("gone.md"));
        // Outside Git, every file has its modification time.
        let plain = tempfile::tempdir().unwrap();
        std::fs::write(plain.path().join("a.md"), "a\n").unwrap();
        let one: BTreeSet<String> = [String::from("a.md")].into();
        assert_eq!(
            file_dates(plain.path(), &one, GIT_BUDGET)["a.md"].basis,
            "mtime"
        );
    }

    #[test]
    fn dates_display_in_utc_minutes() {
        assert_eq!(display(1_760_513_520), "2025-10-15 07:32 UTC");
        assert_eq!(rfc3339(0), json!("1970-01-01T00:00:00Z"));
    }
}
