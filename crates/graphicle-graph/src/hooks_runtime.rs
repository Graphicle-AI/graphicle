// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/hooks.py
//! Detached native hook workers. No interpreter discovery or runtime fallback.
use crate::{Result, hooks::*, paths, watch};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
/// The files the last commit changed. `-z` makes git print each name
/// verbatim, NUL-terminated: never C-quoted or octal-escaped, whatever
/// `core.quotePath` says, so non-ASCII names reach the rebuild as the files
/// on disk (#4278). Names are taken as raw bytes where the platform allows.
pub fn changed_files(root: &Path) -> Result<Vec<PathBuf>> {
    changed_files_since(root, None)
}
/// [`changed_files`], or with `base` the files changed between it and HEAD,
/// with no fallback (Graphify 0.9.92, #4490: the last step of a pick range
/// covers every commit picked since the sequence began).
fn changed_files_since(root: &Path, base: Option<&str>) -> Result<Vec<PathBuf>> {
    let diff = |args: &[&str]| {
        crate::process::capture(
            Command::new("git").arg("-C").arg(root).args(args),
            Duration::from_secs(10),
        )
        .map_err(crate::Error::from)
    };
    let output = match base {
        Some(base) => diff(&["diff", "--name-only", "-z", base, "HEAD"])?,
        None => {
            let output = diff(&["diff", "--name-only", "-z", "HEAD~1", "HEAD"])?;
            if output.success {
                output
            } else {
                diff(&["diff", "--name-only", "-z", "HEAD"])?
            }
        }
    };
    if !output.success {
        return Ok(vec![]);
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(path_from_bytes)
        .collect())
}
/// Where a post-commit sits in a cherry-pick or revert sequence (Graphify
/// 0.9.92, #4490).
#[derive(Debug, PartialEq, Eq)]
enum Sequence {
    /// No live sequence: rebuild what the commit changed.
    Outside,
    /// More steps are pending: skip, so a tracked graph.json is not rewritten
    /// between steps.
    Pending,
    /// The last step: rebuild once, from `sequencer/head` when git recorded it.
    Last(Option<String>),
}
/// git keeps CHERRY_PICK_HEAD and `sequencer/` on disk while post-commit
/// runs, so neither alone is a skip signal. `sequencer/` is trusted only when
/// the first step of its todo list is the commit just made (a pick of the
/// same subject, or its `Revert "..."`): a stale one left by `git reset
/// --hard` must not silence every later commit.
fn sequence(root: &Path) -> Sequence {
    let Ok(out) = git(root, &["rev-parse", "--git-dir"]) else {
        return Sequence::Outside;
    };
    let raw = out.stdout.trim_end_matches(['\r', '\n']);
    if out.code != 0 || raw.is_empty() {
        return Sequence::Outside;
    }
    let sequencer = root.join(raw).join("sequencer");
    let todo = sequencer.join("todo");
    if !todo.is_file() {
        return Sequence::Outside;
    }
    let text = fs::read(&todo).unwrap_or_default();
    let first = first_line(&text);
    let (verb, rest) = first
        .split_once(' ')
        .unwrap_or((first.as_str(), first.as_str()));
    let subject = rest.split_once(' ').map_or("", |(_, subject)| subject);
    let head = git(root, &["log", "-1", "--format=%s"])
        .ok()
        .filter(|out| out.code == 0)
        .map(|out| out.stdout.trim_end_matches('\n').to_owned())
        .unwrap_or_default();
    let live = match verb {
        "pick" => head == subject,
        "revert" => head == format!("Revert \"{subject}\""),
        _ => false,
    };
    if !live {
        return Sequence::Outside;
    }
    let pending = text
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .count();
    if pending > 1 {
        return Sequence::Pending;
    }
    let head_file = sequencer.join("head");
    Sequence::Last(
        head_file
            .is_file()
            .then(|| first_line(&fs::read(&head_file).unwrap_or_default()))
            .filter(|base| !base.is_empty()),
    )
}
/// The first line of `text`, as the shell's `IFS= read -r` reads it.
fn first_line(text: &[u8]) -> String {
    let line = text.split(|b| *b == b'\n').next().unwrap_or_default();
    String::from_utf8_lossy(line).into_owned()
}
/// A file name from bytes git printed: verbatim on Unix (`os.fsdecode`'s
/// surrogate escapes), UTF-8 elsewhere.
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}
/// `_unquote_git_path` (Graphify 0.9.85, #4278): a path as `git diff
/// --name-only` prints it without `-z`. Even with `core.quotePath=false`, git
/// wraps a name holding a double quote, backslash or control character in
/// double quotes and C-escapes it, spelling raw bytes as `\ooo` octal.
/// Unquoted names are returned as they are.
pub fn unquote_git_path(name: &str) -> PathBuf {
    let Some(body) = name
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|_| name.len() >= 2)
    else {
        return PathBuf::from(name);
    };
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch != '\\' || i + 1 >= chars.len() {
            let mut buf = [0; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        let octal = |c: &char| ('0'..='7').contains(c);
        if ('0'..='3').contains(&next)
            && i + 3 < chars.len()
            && chars[i + 1..i + 4].iter().all(octal)
        {
            let digits: String = chars[i + 1..i + 4].iter().collect();
            out.push(u8::from_str_radix(&digits, 8).expect("three octal digits below 0o400"));
            i += 4;
        } else if let Some(byte) = match next {
            'a' => Some(7),
            'b' => Some(8),
            't' => Some(9),
            'n' => Some(10),
            'v' => Some(11),
            'f' => Some(12),
            'r' => Some(13),
            '"' => Some(34),
            '\\' => Some(92),
            _ => None,
        } {
            out.push(byte);
            i += 2;
        } else {
            out.push(b'\\');
            i += 1;
        }
    }
    path_from_bytes(&out)
}
pub fn saved_root(cwd: &Path, out: &Path) -> (PathBuf, Option<String>) {
    let raw = fs::read_to_string(out.join(".graphicle_root")).unwrap_or_default();
    let raw = raw.trim_start_matches('\u{feff}').trim();
    if raw.is_empty() {
        return (cwd.to_path_buf(), None);
    }
    let candidate = cwd.join(raw);
    let root = crate::extractors::resolution::resolved(cwd);
    match candidate.canonicalize() {
        // `canonicalize` is verbatim (`\\?\C:\...`) on Windows and the root is not.
        Ok(p) if p.is_dir() && crate::fsutil::path_starts_with(&p, &root) => (candidate, None),
        _ => (
            cwd.to_path_buf(),
            Some(format!(
                "[graphicle hook] ignoring out-of-repo .graphicle_root: {raw}"
            )),
        ),
    }
}
/// The hook events `hook launch`/`hook run` accept. post-checkout,
/// post-merge (#2418) and post-rewrite (Graphify 0.9.92, #4490) rebuild
/// unscoped; post-commit rebuilds what it changed.
const HOOK_EVENTS: [&str; 4] = ["post-commit", "post-checkout", "post-merge", "post-rewrite"];
pub fn launch(path: &Path, event: &str) -> Result<String> {
    if !HOOK_EVENTS.contains(&event) {
        return Err(format!("Unknown hook event: {event}").into());
    }
    let root = git_root(path).ok_or("Not in a git repository.")?;
    let changed = if event == "post-commit" {
        // A range pick or revert rebuilds once, at its last step, for every
        // commit picked since the sequence began.
        match sequence(&root) {
            Sequence::Pending => return Ok(String::new()),
            Sequence::Last(Some(base)) => changed_files_since(&root, Some(&base))?,
            Sequence::Last(None) | Sequence::Outside => changed_files(&root)?,
        }
    } else {
        vec![]
    };
    if event == "post-commit"
        && (changed.is_empty()
            || changed
                .iter()
                .all(|p| p.starts_with(paths::OUT_DIR.as_str())))
    {
        return Ok(String::new());
    }
    let log = std::env::var_os("GRAPHICLE_REBUILD_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(".cache/graphicle/rebuild.log")
        });
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent).map_err(crate::Error::from)?;
    }
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log)
        .map_err(crate::Error::from)?;
    let err = file.try_clone().map_err(crate::Error::from)?;
    let mut cmd = Command::new(std::env::current_exe().map_err(crate::Error::from)?);
    cmd.args(["hook", "run", event])
        .current_dir(&root)
        .env("GRAPHICLE_CHANGED_JSON", json!(changed).to_string())
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    let child = spawn_detached(&mut cmd).map_err(crate::Error::from)?;
    #[cfg(not(windows))]
    let child = cmd.spawn().map_err(crate::Error::from)?;
    Ok(format!(
        "[graphicle hook] rebuild started (pid {}, log {})",
        child.id(),
        log.display()
    ))
}
/// Graphify's hooks.py on Windows: CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
/// breaking away from any job object when allowed (CREATE_BREAKAWAY_FROM_JOB),
/// else without it. CREATE_NO_WINDOW, not DETACHED_PROCESS, so no console
/// window shows for the whole rebuild. This process's standard handles are
/// made non-inheritable first, as Graphify's `Popen` passes only the child's
/// own: else the rebuild holds the Git hook's output pipes open and a caller
/// that reads them (an agent's shell, an IDE) waits for the whole rebuild.
#[cfg(windows)]
fn spawn_detached(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const FLAGS: u32 = 0x0800_0000 | 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    crate::win::keep_standard_handles();
    match cmd
        .creation_flags(FLAGS | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Ok(child) => Ok(child),
        Err(_) => cmd.creation_flags(FLAGS).spawn(),
    }
}
pub fn run_rebuild(
    cwd: &Path,
    event: &str,
    changed: Option<Vec<PathBuf>>,
) -> Result<watch::RebuildResult> {
    if !HOOK_EVENTS.contains(&event) {
        return Err(format!("Unknown hook event: {event}").into());
    }
    let changed = changed.unwrap_or_else(|| {
        std::env::var("GRAPHICLE_CHANGED_JSON")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| {
                // Names as `git diff --name-only` prints them: decoded when
                // git still quotes them (#4278).
                std::env::var("GRAPHICLE_CHANGED")
                    .unwrap_or_default()
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(unquote_git_path)
                    .collect()
            })
    });
    if event == "post-commit" && changed.is_empty() {
        return Ok(watch::RebuildResult {
            success: true,
            ..Default::default()
        });
    }
    let (root, warning) = saved_root(cwd, &cwd.join(paths::OUT_DIR.as_str()));
    let force = std::env::var("GRAPHICLE_FORCE")
        .is_ok_and(|s| ["1", "true", "yes"].contains(&s.to_ascii_lowercase().as_str()));
    let changed = if event == "post-commit" {
        Some(changed.into_iter().map(|p| cwd.join(p)).collect())
    } else {
        None
    };
    let mut result = watch::rebuild(
        &root,
        &watch::RebuildOptions {
            changed_paths: changed,
            force,
            ..Default::default()
        },
    )?;
    if let Some(w) = warning {
        result.messages.insert(0, w);
    }
    let out = root.join(paths::OUT_DIR.as_str());
    let memory = out.join("memory");
    if fs::read_dir(&memory).is_ok_and(|mut it| {
        it.any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|s| s == "md")))
    }) {
        let graph = out.join("graph.json");
        let _ = crate::reflect::reflect(
            &memory,
            &out.join("reflections/LESSONS.md"),
            crate::reflect::ReflectOptions {
                graph: graph.exists().then_some(graph.as_path()),
                analysis: None,
                labels: None,
                now: &crate::python_datetime::DateTime::now(),
                half_life: 30.0,
                min_corroboration: 2,
            },
        );
    }
    Ok(result)
}
/// Only the detached CLI worker acquires an exit timer or resource limits.
fn stop_descendants() {
    #[cfg(unix)]
    {
        // Native integrations start their own process groups. Enumerate only
        // this worker's descendants so its deadline cannot leave them orphaned.
        if let Ok(output) = crate::process::capture(
            Command::new("ps").args(["-axo", "pid=,ppid="]),
            Duration::from_secs(2),
        ) {
            let processes = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| {
                    let mut parts = line.split_whitespace();
                    Some((
                        parts.next()?.parse::<i32>().ok()?,
                        parts.next()?.parse::<i32>().ok()?,
                    ))
                })
                .collect::<Vec<_>>();
            let me = std::process::id() as i32;
            let mut descendants = std::collections::BTreeSet::from([me]);
            loop {
                let before = descendants.len();
                for (pid, parent) in &processes {
                    if *pid > 1 && descendants.contains(parent) {
                        descendants.insert(*pid);
                    }
                }
                if before == descendants.len() {
                    break;
                }
            }
            descendants.remove(&me);
            for pid in descendants.into_iter().rev() {
                let pid = nix::unistd::Pid::from_raw(pid);
                let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &std::process::id().to_string(), "/T", "/F"])
            .status();
    }
}
pub fn run_worker(event: &str) -> Result<watch::RebuildResult> {
    watch::apply_resource_limits();
    let timeout = std::env::var("GRAPHICLE_REBUILD_TIMEOUT")
        .unwrap_or("600".into())
        .parse::<u64>()
        .map_err(|_| "Invalid GRAPHICLE_REBUILD_TIMEOUT")?;
    let completed = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    if timeout > 0 {
        let state = completed.clone();
        std::thread::spawn(move || {
            let (lock, wake) = &*state;
            let (g, _) = wake
                .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(timeout), |done| {
                    !*done
                })
                .unwrap();
            if !*g {
                eprintln!("[graphicle hook] graphicle rebuild exceeded {timeout}s");
                stop_descendants();
                std::process::exit(1);
            }
        });
    }
    let result = run_rebuild(
        &std::env::current_dir().map_err(crate::Error::from)?,
        event,
        None,
    );
    let (lock, wake) = &*completed;
    *lock.lock().unwrap() = true;
    wake.notify_all();
    result
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |k| r.get(k).and_then(Value::as_str).unwrap_or("");
    let path = |k| Path::new(r.get(k).and_then(Value::as_str).unwrap_or("."));
    Ok(match s("action") {
        "load_rc" => load_rc(path("path"))?,
        "git_root" => json!(git_root(path("path"))),
        "hooks_dir" => json!(hooks_dir(path("path"))?),
        "user_hooks_dir" => json!(user_hooks_dir(path("path"))),
        "reject_windows_path" => {
            reject_windows_path(s("value"), s("source"))?;
            Value::Null
        }
        "rotating_prefix" => json!(rotating_prefix(s("path"))),
        "merge_attr_line" => json!(merge_attr_line(s("out"))),
        "has_merge_attr" => json!(has_merge_attr(s("content"))),
        "register_merge_driver" => json!(register_merge_driver(
            path("path"),
            r["executable"].as_str()
        )?),
        "unregister_merge_driver" => json!(unregister_merge_driver(path("path"))?),
        "merge_driver_status" => json!(merge_driver_status(path("path"))),
        "install_hook" => json!(install_hook(
            path("path"),
            s("name"),
            s("script"),
            s("marker"),
            s("end")
        )?),
        "uninstall_hook" => json!(uninstall_hook(
            path("path"),
            s("name"),
            s("marker"),
            s("end")
        )?),
        "script" => json!(if r["rewrite"].as_bool().unwrap_or(false) {
            crate::hooks::hook_script(
                crate::hooks::HookKind::Rewrite,
                s("executable"),
                r["viz_node_limit"].as_i64(),
            )
        } else if r["merge"].as_bool().unwrap_or(false) {
            crate::hooks::hook_script(
                crate::hooks::HookKind::Merge,
                s("executable"),
                r["viz_node_limit"].as_i64(),
            )
        } else {
            script(
                r["checkout"].as_bool().unwrap_or(false),
                s("executable"),
                r["viz_node_limit"].as_i64(),
            )
        }),
        "saved_root" => json!(saved_root(path("path"), path("out"))),
        "changed_files" => json!(changed_files(path("path"))?),
        "unquote_git_path" => json!(unquote_git_path(s("name"))),
        "install" => json!(install(path("path"))?),
        "uninstall" => json!(uninstall(path("path"))?),
        "status" => json!(status(path("path"))?),
        "launch" => json!(launch(path("path"), s("event"))?),
        "run" => serde_json::to_value(run_rebuild(
            path("path"),
            s("event"),
            r.get("changed_paths")
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()
                .map_err(crate::Error::from)?,
        )?)
        .map_err(crate::Error::from)?,
        x => return Err(format!("Unknown hooks action: {x}").into()),
    })
}

#[cfg(test)]
mod sequence_tests {
    use super::{Sequence, sequence};
    use std::{fs, path::Path, process::Command};

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t.co"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo_with_commits(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]);
        for name in names {
            fs::write(repo.join(name), format!("def {name}(): pass\n")).unwrap();
            git(repo, &["add", name]);
            git(repo, &["commit", "-q", "-m", name]);
        }
        dir
    }

    fn sequencer(repo: &Path, todo: &str, head: Option<&str>) {
        let dir = repo.join(".git/sequencer");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("todo"), todo).unwrap();
        if let Some(head) = head {
            fs::write(dir.join("head"), format!("{head}\n")).unwrap();
        }
    }

    /// Graphify 0.9.92 (#4490): a post-commit inside a pick or revert range
    /// is skipped while more steps are pending, and the last step rebuilds
    /// from `sequencer/head`; a todo whose first step is not the commit just
    /// made (a stale one, an unknown verb, an empty file) is not a sequence.
    #[test]
    fn post_commit_follows_a_live_pick_or_revert_sequence() {
        let dir = repo_with_commits(&["a.py", "b.py"]);
        let repo = dir.path();
        assert_eq!(sequence(repo), Sequence::Outside);
        sequencer(
            repo,
            "pick aaaaaaa b.py\npick bbbbbbb c.py\n",
            Some("0123abc"),
        );
        assert_eq!(sequence(repo), Sequence::Pending);
        sequencer(repo, "pick aaaaaaa b.py\n", Some("0123abc"));
        assert_eq!(sequence(repo), Sequence::Last(Some("0123abc".into())));
        fs::remove_file(repo.join(".git/sequencer/head")).unwrap();
        assert_eq!(sequence(repo), Sequence::Last(None));
        for todo in [
            "pick aaaaaaa some other commit\npick bbbbbbb c.py\n",
            "revert aaaaaaa b.py\nrevert bbbbbbb c.py\n",
            "squash aaaaaaa b.py\npick bbbbbbb c.py\n",
            "",
        ] {
            sequencer(repo, todo, Some("0123abc"));
            assert_eq!(sequence(repo), Sequence::Outside, "{todo:?}");
        }
        fs::remove_dir_all(repo.join(".git/sequencer")).unwrap();
        fs::write(repo.join("x.py"), "def x(): pass\n").unwrap();
        git(repo, &["add", "x.py"]);
        git(repo, &["commit", "-q", "-m", "Revert \"x\""]);
        sequencer(repo, "revert aaaaaaa x\nrevert bbbbbbb y\n", None);
        assert_eq!(sequence(repo), Sequence::Pending);
        sequencer(repo, "revert aaaaaaa x", None);
        assert_eq!(sequence(repo), Sequence::Last(None));
    }

    /// The last step of a range covers every commit since the sequence began.
    #[test]
    fn the_last_step_lists_every_file_since_the_sequence_base() {
        let dir = repo_with_commits(&["a.py", "b.py", "c.py"]);
        let repo = dir.path();
        let base = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD~2"])
            .output()
            .unwrap();
        let base = String::from_utf8(base.stdout).unwrap();
        let mut changed = super::changed_files_since(repo, Some(base.trim())).unwrap();
        changed.sort();
        assert_eq!(changed, vec![Path::new("b.py"), Path::new("c.py")]);
        assert_eq!(super::changed_files(repo).unwrap(), vec![Path::new("c.py")]);
        assert!(
            super::changed_files_since(repo, Some("not-a-commit"))
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(all(test, windows))]
mod windows_spawn_tests {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn detached_child_runs_without_a_console() {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "exit 7"]);
        let status = super::spawn_detached(&mut cmd).unwrap().wait().unwrap();
        assert_eq!(status.code(), Some(7));
    }

    /// The caller's half of [`detached_child_leaves_the_callers_pipes`]: run
    /// in a child of that test whose output is two pipes, it starts a
    /// 20-second program detached with its own output (as `launch` does)
    /// and returns. Run by itself it does nothing.
    #[test]
    #[allow(clippy::zombie_processes)] // the detached child outlives this test by design
    fn detached_child_pipe_caller() {
        if std::env::var_os("HOOK_LAUNCH_PIPE_CALLER").is_none() {
            return;
        }
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "21", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        super::spawn_detached(&mut cmd).unwrap();
    }

    /// A Git hook's caller reads the hook's output to its end: that end must
    /// come when `hook launch` exits, not when the detached rebuild does.
    #[test]
    fn detached_child_leaves_the_callers_pipes() {
        let start = Instant::now();
        let mut caller = Command::new(std::env::current_exe().unwrap())
            .args([
                "hooks_runtime::windows_spawn_tests::detached_child_pipe_caller",
                "--exact",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("HOOK_LAUNCH_PIPE_CALLER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut errors = caller.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            errors.read_to_string(&mut text).map(|_| text)
        });
        let mut output = String::new();
        caller
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        let errors = reader.join().unwrap().unwrap();
        let waited = start.elapsed();
        assert!(caller.wait().unwrap().success(), "{output}{errors}");
        assert!(output.contains("1 passed"), "{output}{errors}");
        assert!(
            waited < Duration::from_secs(12),
            "the detached child held the caller's pipes for {waited:?}"
        );
    }
}
