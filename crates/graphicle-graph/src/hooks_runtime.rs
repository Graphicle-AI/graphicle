// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
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
    let diff = |args: &[&str]| {
        crate::process::capture(
            Command::new("git").arg("-C").arg(root).args(args),
            Duration::from_secs(10),
        )
        .map_err(crate::Error::from)
    };
    let mut output = diff(&["diff", "--name-only", "-z", "HEAD~1", "HEAD"])?;
    if !output.success {
        output = diff(&["diff", "--name-only", "-z", "HEAD"])?;
    }
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
        Ok(p) if p.is_dir() && p.starts_with(&root) => (candidate, None),
        _ => (
            cwd.to_path_buf(),
            Some(format!(
                "[graphicle hook] ignoring out-of-repo .graphicle_root: {raw}"
            )),
        ),
    }
}
pub fn launch(path: &Path, event: &str) -> Result<String> {
    if !["post-commit", "post-checkout"].contains(&event) {
        return Err(format!("Unknown hook event: {event}").into());
    }
    let root = git_root(path).ok_or("Not in a git repository.")?;
    let changed = if event == "post-commit" {
        changed_files(&root)?
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
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000008);
    }
    let child = cmd.spawn().map_err(crate::Error::from)?;
    Ok(format!(
        "[graphicle hook] rebuild started (pid {}, log {})",
        child.id(),
        log.display()
    ))
}
pub fn run_rebuild(
    cwd: &Path,
    event: &str,
    changed: Option<Vec<PathBuf>>,
) -> Result<watch::RebuildResult> {
    if !["post-commit", "post-checkout"].contains(&event) {
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
        "script" => json!(script(
            r["checkout"].as_bool().unwrap_or(false),
            s("executable"),
            r["viz_node_limit"].as_i64()
        )),
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
