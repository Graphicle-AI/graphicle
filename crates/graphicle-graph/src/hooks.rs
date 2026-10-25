// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/hooks.py
//! Native Git hook installation, detached rebuilds, and merge-driver setup.
pub use crate::hooks_runtime::{changed_files, dispatch, launch, run_worker, unquote_git_path};
use crate::{Result, paths, process};
use serde_json::{Value, json};
use std::{
    fs::{self},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
pub const HOOK_MARKER: &str = "# graphicle-hook-start";
pub const HOOK_END: &str = "# graphicle-hook-end";
pub const CHECKOUT_MARKER: &str = "# graphicle-checkout-hook-start";
pub const CHECKOUT_END: &str = "# graphicle-checkout-hook-end";
pub const MERGE_HOOK_MARKER: &str = "# graphicle-merge-hook-start";
pub const MERGE_HOOK_END: &str = "# graphicle-merge-hook-end";
pub const REWRITE_HOOK_MARKER: &str = "# graphicle-rewrite-hook-start";
pub const REWRITE_HOOK_END: &str = "# graphicle-rewrite-hook-end";
/// The four hooks `hook install` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    /// post-commit: rebuild the files the commit changed.
    Commit,
    /// post-checkout: an unscoped rebuild after a branch switch.
    Checkout,
    /// post-merge (0.9.89, #2418): an unscoped rebuild after a merge, so a
    /// symbol the graph.json merge driver's union resurrected is dropped.
    Merge,
    /// post-rewrite (Graphify 0.9.92, #4490): an unscoped rebuild when a
    /// rebase finishes, `git pull --rebase` included. post-commit skips every
    /// replayed commit (rebase-merge/ is on disk), and git runs post-rewrite
    /// once, after the last one, with the tree in its final state.
    Rewrite,
}
impl HookKind {
    pub fn name(self) -> &'static str {
        match self {
            HookKind::Commit => "post-commit",
            HookKind::Checkout => "post-checkout",
            HookKind::Merge => "post-merge",
            HookKind::Rewrite => "post-rewrite",
        }
    }
    fn markers(self) -> (&'static str, &'static str) {
        match self {
            HookKind::Commit => (HOOK_MARKER, HOOK_END),
            HookKind::Checkout => (CHECKOUT_MARKER, CHECKOUT_END),
            HookKind::Merge => (MERGE_HOOK_MARKER, MERGE_HOOK_END),
            HookKind::Rewrite => (REWRITE_HOOK_MARKER, REWRITE_HOOK_END),
        }
    }
}
const HOOK_KINDS: [HookKind; 4] = [
    HookKind::Commit,
    HookKind::Checkout,
    HookKind::Merge,
    HookKind::Rewrite,
];
/// `_load_graphifyrc`: `<root>/.graphiclerc`, parsed by [`crate::rcfile`]
/// (Graphify 0.9.87, #2961). Kept as the hooks-side name; returns
/// `viz_node_limit` (used here) beside any other option the file sets, and a
/// malformed line is an error naming it.
pub fn load_rc(root: &Path) -> Result<Value> {
    crate::rcfile::load_rc_file(root).map(|config| config.to_json())
}
pub fn git_root(path: &Path) -> Option<PathBuf> {
    crate::extractors::resolution::resolved(path)
        .ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}
pub fn reject_windows_path(value: &str, source: &str) -> Result<()> {
    #[cfg(windows)]
    let _ = (value, source);
    #[cfg(not(windows))]
    if value.contains('\\')
        || regex::Regex::new(r"^[A-Za-z]:[\\/]")
            .unwrap()
            .is_match(value)
    {
        return Err(format!(
            "git hooks path from {source} looks like a Windows path: {}. On WSL/POSIX this can't resolve to a real directory. Unset it with `git config --local --unset core.hooksPath`, or set a POSIX path.",
            crate::python_compat::repr(&json!(value))
        ).into());
    }
    Ok(())
}
pub(crate) struct GitOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}
pub(crate) fn git(root: &Path, args: &[&str]) -> Result<GitOutput> {
    let out = process::capture(
        Command::new("git").arg("-C").arg(root).args(args),
        Duration::from_secs(10),
    )
    .map_err(crate::Error::from)?;
    Ok(GitOutput {
        code: out.code.unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}
fn is_within(child: &Path, parent: &Path) -> bool {
    use crate::extractors::resolution::resolved;
    resolved(child).starts_with(resolved(parent))
}
/// One `git rev-parse` path flag, anchored on `root` and resolved.
fn rev_parse_path(root: &Path, flag: &str) -> Option<PathBuf> {
    let out = git(root, &["rev-parse", flag]).ok()?;
    let raw = out.stdout.trim();
    if out.code != 0 || raw.is_empty() || raw.contains(['\n', '\r', '\0']) {
        return None;
    }
    Some(crate::extractors::resolution::resolved(&root.join(raw)))
}
/// Git's own hooks directory, ignoring `core.hooksPath`; derived from the
/// common git dir so a linked worktree gets the main repo's `.git/hooks`.
fn builtin_hooks_dir(root: &Path) -> Option<PathBuf> {
    ["--git-common-dir", "--git-dir"]
        .iter()
        .find_map(|flag| rev_parse_path(root, flag))
        .map(|git_dir| crate::extractors::resolution::resolved(&git_dir.join("hooks")))
}
/// In-repo hook dirs (Husky) and git's own hooks dir only: a `core.hooksPath`
/// resolving anywhere else would make `hook install` write outside the repo.
fn hooks_path_allowed(root: &Path, candidate: &Path) -> bool {
    is_within(candidate, root)
        || builtin_hooks_dir(root)
            .is_some_and(|builtin| crate::extractors::resolution::resolved(candidate) == builtin)
}
pub fn hooks_dir(root: &Path) -> Result<PathBuf> {
    if let Ok(out) = git(root, &["rev-parse", "--git-path", "hooks"]) {
        let raw = out.stdout.trim();
        if out.code != 0 {
            let err = out.stderr.trim();
            let err = if err.is_empty() {
                format!("git exited with code {}", out.code)
            } else {
                err.to_string()
            };
            eprintln!(
                "[graphicle hooks] git could not resolve the hooks path for {}: {err}",
                root.display()
            );
        } else if !raw.is_empty() && !raw.contains(['\n', '\r', '\0']) {
            reject_windows_path(raw, "git rev-parse --git-path hooks")?;
            let path = crate::extractors::resolution::resolved(&root.join(raw));
            // Upstream treats an OSError while creating the directory like any
            // other git failure and falls back to `.git/hooks`.
            if hooks_path_allowed(root, &path) {
                if fs::create_dir_all(&path).is_ok() {
                    return Ok(path);
                }
            } else {
                eprintln!(
                    "[graphicle hooks] refusing hooks path {}: it is outside {}. Installing into the default git hooks directory instead (#3869).",
                    path.display(),
                    crate::extractors::resolution::resolved(root).display()
                );
                if let Some(default) = builtin_hooks_dir(root)
                    && fs::create_dir_all(&default).is_ok()
                {
                    return Ok(default);
                }
            }
        }
    }
    let path = root.join(".git/hooks");
    fs::create_dir_all(&path).map_err(crate::Error::from)?;
    Ok(path)
}
pub fn user_hooks_dir(path: &Path) -> PathBuf {
    if path.file_name().and_then(|s| s.to_str()) == Some("_") {
        path.parent().unwrap_or(path).into()
    } else {
        path.into()
    }
}
fn read_hook(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
        .map_err(crate::Error::from)
}
fn write(path: &Path, s: &str) -> Result<()> {
    fs::write(path, s).map_err(crate::Error::from)
}
/// Write `s` to `path` as a regular file only: a symbolic link there is
/// refused (O_NOFOLLOW on Unix), never followed.
fn write_regular(path: &Path, s: &str) -> Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(crate::Error::from)?;
    file.write_all(s.as_bytes()).map_err(crate::Error::from)
}
pub fn install_hook(
    dir: &Path,
    name: &str,
    script: &str,
    marker: &str,
    end: &str,
) -> Result<String> {
    let path = dir.join(name);
    if path.exists() {
        let content = read_hook(&path)?;
        if content.contains(marker) {
            if !end.is_empty()
                && let (Some(a), Some(b)) = (content.find(marker), content.find(end))
                && b >= a
            {
                let updated = format!(
                    "{}{}{}",
                    &content[..a],
                    script.trim_end(),
                    &content[b + end.len()..]
                );
                if updated == content {
                    return Ok(format!("already installed at {}", path.display()));
                }
                write(&path, &updated)?;
                return Ok(format!(
                    "updated existing {name} hook at {}",
                    path.display()
                ));
            }
            return Ok(format!("already installed at {}", path.display()));
        }
        write(&path, &format!("{}\n\n{script}", content.trim_end()))?;
        return Ok(format!(
            "appended to existing {name} hook at {}",
            path.display()
        ));
    }
    write(&path, &format!("#!/bin/sh\n{script}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .map_err(crate::Error::from)?;
    }
    Ok(format!("installed at {}", path.display()))
}
pub fn uninstall_hook(dir: &Path, name: &str, marker: &str, end: &str) -> Result<String> {
    let path = dir.join(name);
    if !path.exists() {
        return Ok(format!("no {name} hook found - nothing to remove."));
    }
    let content = read_hook(&path)?;
    if !content.contains(marker) {
        return Ok(format!(
            "graphicle hook not found in {name} - nothing to remove."
        ));
    }
    let re = regex::Regex::new(&format!(
        "(?s){}.*?{}\n?",
        regex::escape(marker),
        regex::escape(end)
    ))
    .map_err(crate::Error::from)?;
    let text = re.replace_all(&content, "").trim().to_string();
    if text.is_empty() || ["#!/bin/bash", "#!/bin/sh"].contains(&text.as_str()) {
        fs::remove_file(path.clone()).map_err(crate::Error::from)?;
        Ok(format!("removed {name} hook at {}", path.display()))
    } else {
        write(&path, &format!("{text}\n"))?;
        Ok(format!(
            "graphicle removed from {name} at {} (other hook content preserved)",
            path.display()
        ))
    }
}
pub fn rotating_prefix(path: &str) -> bool {
    regex::Regex::new(r"/snap/[^/]+/(\d+|current)/")
        .unwrap()
        .is_match(&path.replace('\\', "/"))
}
pub fn pinned_executable() -> String {
    std::env::current_exe()
        .ok()
        .filter(|p| !rotating_prefix(&p.to_string_lossy()))
        .filter(|p| !p.to_string_lossy().contains(['\n', '\r', '\0']))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}
pub fn merge_attr_line(out: &str) -> String {
    let out = if out.is_empty() || Path::new(out).is_absolute() || out.contains('\\') {
        paths::DEFAULT_OUT_DIR
    } else {
        out
    };
    format!("{}/graph.json merge=graphicle", out.trim_end_matches('/'))
}
pub fn has_merge_attr(content: &str) -> bool {
    content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .any(|l| {
            let f = l.split_whitespace().collect::<Vec<_>>();
            f.first().is_some_and(|s| s.ends_with("graph.json"))
                && f[1..].contains(&"merge=graphicle")
        })
}
pub fn register_merge_driver(root: &Path, executable: Option<&str>) -> Result<String> {
    let pinned = executable
        .map(str::to_string)
        .unwrap_or_else(pinned_executable);
    let driver = format!(
        "{} merge-driver %O %A %B",
        if pinned.is_empty() {
            crate::cli::program().into()
        } else {
            shell_quote(&pinned)
        }
    );
    for (k, v) in [
        ("merge.graphicle.name", "graphicle graph.json union merge"),
        ("merge.graphicle.driver", driver.as_str()),
    ] {
        let result = git(root, &["config", k, v]);
        if !result.as_ref().is_ok_and(|r| r.code == 0) {
            return Ok(format!(
                "not registered (git config failed: {})",
                result.map(|r| r.stderr).unwrap_or_else(|e| e.to_string())
            ));
        }
    }
    let line = merge_attr_line(paths::OUT_DIR.as_str());
    let path = root.join(".gitattributes");
    // A linked .gitattributes is neither read nor written: git does not read
    // an in-tree .gitattributes that is a symbolic link (since 2.32), and an
    // append through it would land in whatever file it names (a repository
    // can commit such a link). A deliberate difference: upstream appends
    // through the link.
    if path.is_symlink() {
        return Ok(format!(
            "partially registered: .gitattributes is a symbolic link, left alone; add `{line}` to a regular .gitattributes by hand"
        ));
    }
    let mut content = if path.exists() {
        read_hook(&path)?
    } else {
        String::new()
    };
    if has_merge_attr(&content) {
        return Ok(format!("already registered ({line})"));
    }
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&line);
    content.push('\n');
    write_regular(&path, &content)?;
    Ok(format!("registered ({line})"))
}
pub fn unregister_merge_driver(root: &Path) -> Result<String> {
    for key in ["merge.graphicle.name", "merge.graphicle.driver"] {
        let _ = git(root, &["config", "--unset", key]);
    }
    let path = root.join(".gitattributes");
    // Install never writes into a linked .gitattributes; uninstall leaves one alone too.
    if path.is_symlink() {
        return Ok(".gitattributes is a symbolic link, left alone.".into());
    }
    if !path.exists() {
        return Ok("not registered - nothing to remove.".into());
    }
    let s = read_hook(&path)?;
    let kept = s
        .lines()
        .filter(|line| !has_merge_attr(line))
        .collect::<Vec<_>>();
    if kept.len() == s.lines().count() {
        return Ok("gitattributes entry not found - nothing to remove.".into());
    }
    if kept.is_empty() {
        fs::remove_file(path).map_err(crate::Error::from)?;
        Ok("removed (.gitattributes deleted - no other entries)".into())
    } else {
        write_regular(&path, &format!("{}\n", kept.join("\n")))?;
        Ok("removed from .gitattributes (other entries preserved)".into())
    }
}
pub fn merge_driver_status(root: &Path) -> String {
    let config = git(root, &["config", "--get", "merge.graphicle.driver"])
        .is_ok_and(|r| r.code == 0 && !r.stdout.trim().is_empty());
    // A linked .gitattributes is not read: git ignores it.
    let attributes = root.join(".gitattributes");
    let attr = !attributes.is_symlink() && read_hook(&attributes).is_ok_and(|s| has_merge_attr(&s));
    match (config, attr) {
        (true, true) => "registered",
        (true, false) => "partially registered (git config set, .gitattributes line missing)",
        (false, true) => "partially registered (.gitattributes line set, git config missing)",
        _ => "not registered",
    }
    .into()
}
pub fn script(checkout: bool, pinned: &str, viz: Option<i64>) -> String {
    let kind = if checkout {
        HookKind::Checkout
    } else {
        HookKind::Commit
    };
    script_for(kind, pinned, viz, crate::cli::program())
}
/// The hook block of `kind`.
pub fn hook_script(kind: HookKind, pinned: &str, viz: Option<i64>) -> String {
    script_for(kind, pinned, viz, crate::cli::program())
}
/// [`hook_script`] naming the executable `program`.
fn script_for(kind: HookKind, pinned: &str, viz: Option<i64>, program: &str) -> String {
    let (marker, end) = kind.markers();
    let export = viz
        .map(|n| {
            format!("export GRAPHICLE_VIZ_NODE_LIMIT=\"${{GRAPHICLE_VIZ_NODE_LIMIT:-{n}}}\"\n")
        })
        .unwrap_or_default();
    // A branch checkout, a completed merge and a finished rebase rebuild only
    // a graph that was built before; post-checkout also skips file checkouts
    // and a checkout that keeps HEAD (`git checkout -b`, Graphify #2421).
    // post-rewrite drains the old/new pairs git writes to its stdin, so git
    // never blocks on a full pipe, and skips an amend: post-commit rebuilt it.
    let event = match kind {
        HookKind::Commit => "",
        HookKind::Checkout => {
            "[ \"${3:-0}\" = \"1\" ] || exit 0\n[ \"${1:-}\" = \"${2:-}\" ] && exit 0\n[ -d \"${GRAPHICLE_OUT:-.graphicle}\" ] || exit 0\n"
        }
        HookKind::Merge => "[ -d \"${GRAPHICLE_OUT:-.graphicle}\" ] || exit 0\n",
        HookKind::Rewrite => {
            "[ -t 0 ] || cat >/dev/null\n[ \"${1:-}\" = \"rebase\" ] || exit 0\n[ -d \"${GRAPHICLE_OUT:-.graphicle}\" ] || exit 0\n"
        }
    };
    // Skip during a rebase, merge or cherry-pick to avoid blocking
    // `--continue` with unstaged changes. git keeps each signal on disk while
    // the hook that completes the operation runs, so that hook must not test
    // it (Graphify 0.9.92): post-merge runs beside MERGE_HEAD (#4463),
    // post-commit beside a pick's CHERRY_PICK_HEAD (#4490; the sequence rule
    // is in `hook launch`), and post-rewrite beside rebase-merge/.
    let skips: &[&str] = match kind {
        HookKind::Commit => &["-d rebase-merge", "-d rebase-apply", "-f MERGE_HEAD"],
        HookKind::Checkout => &[
            "-d rebase-merge",
            "-d rebase-apply",
            "-f MERGE_HEAD",
            "-f CHERRY_PICK_HEAD",
        ],
        HookKind::Merge => &["-d rebase-merge", "-d rebase-apply", "-f CHERRY_PICK_HEAD"],
        HookKind::Rewrite => &[],
    };
    let skips = skips
        .iter()
        .map(|test| {
            let (flag, name) = test.split_once(' ').unwrap();
            format!("[ {flag} \"$_GCL_GITDIR/{name}\" ] && exit 0\n")
        })
        .collect::<String>();
    let mode = kind.name();
    // The rebuild runs from the checkout's top level (Graphify 0.9.95, #3258):
    // with GIT_DIR exported and no GIT_WORK_TREE, git starts a hook in the
    // caller's directory, and `--show-toplevel` answers with that directory.
    // In the standard layout the top is the parent of the common .git dir;
    // GIT_WORK_TREE and --separate-git-dir layouts keep git's own answer.
    // GIT_DIR is rewritten to its absolute form so git still finds it.
    format!(
        r#"{marker}
# Rebuild the knowledge graph using the {program} executable.
(
{export}{event}[ "${{GRAPHICLE_SKIP_HOOK:-0}}" = "1" ] && exit 0
_GCL_GITDIR=$(git rev-parse --git-dir 2>/dev/null) || exit 0
{skips}_GCL_GITDIR=$(cd "$_GCL_GITDIR" 2>/dev/null && pwd)
_GCL_COMMONDIR=$(cd "$(git rev-parse --git-common-dir 2>/dev/null)" 2>/dev/null && pwd)
[ -n "$_GCL_COMMONDIR" ] && [ "$_GCL_GITDIR" != "$_GCL_COMMONDIR" ] && exit 0
if [ -z "${{GIT_WORK_TREE:-}}" ] && [ "${{_GCL_COMMONDIR##*/}}" = ".git" ]; then
    _GCL_TOP=${{_GCL_COMMONDIR%/*}}
else
    _GCL_TOP=$(git rev-parse --show-toplevel 2>/dev/null)
fi
[ -n "$_GCL_TOP" ] && cd "$_GCL_TOP" || exit 0
[ -n "$_GCL_GITDIR" ] && GIT_DIR=$_GCL_GITDIR
_GCL_BIN={pinned}
if [ ! -x "$_GCL_BIN" ]; then
    _GCL_BIN=$(command -v {program} 2>/dev/null)
fi
if [ -z "$_GCL_BIN" ]; then
    echo "[graphicle hook] Executable not found; re-run {program} hook install." >&2
    exit 0
fi
"$_GCL_BIN" hook launch {mode}
)
{end}
"#,
        pinned = shell_quote(pinned)
    )
}
pub fn install(path: &Path) -> Result<String> {
    let root = git_root(path).ok_or_else(|| {
        format!(
            "No git repository found at or above {}",
            crate::extractors::resolution::resolved(path).display()
        )
    })?;
    let dir = user_hooks_dir(&hooks_dir(&root)?);
    let cfg = load_rc(&root)?;
    let pinned = pinned_executable();
    let viz = cfg["viz_node_limit"].as_i64();
    let mut output = String::new();
    for kind in HOOK_KINDS {
        let (marker, end) = kind.markers();
        let message = install_hook(
            &dir,
            kind.name(),
            &hook_script(kind, &pinned, viz),
            marker,
            end,
        )?;
        output.push_str(&format!("{}: {message}\n", kind.name()));
    }
    let merge = register_merge_driver(&root, Some(&pinned))?;
    Ok(format!("{output}merge driver: {merge}"))
}
pub fn uninstall(path: &Path) -> Result<String> {
    let root = git_root(path).ok_or_else(|| {
        format!(
            "No git repository found at or above {}",
            crate::extractors::resolution::resolved(path).display()
        )
    })?;
    let dir = user_hooks_dir(&hooks_dir(&root)?);
    let mut output = String::new();
    for kind in HOOK_KINDS {
        let (marker, end) = kind.markers();
        let message = uninstall_hook(&dir, kind.name(), marker, end)?;
        output.push_str(&format!("{}: {message}\n", kind.name()));
    }
    let merge = unregister_merge_driver(&root)?;
    Ok(format!("{output}merge driver: {merge}"))
}
/// `_comparable_block` (0.9.73, #3771): the graphicle block in `text` with its
/// per-install values blanked out, or `None` when the block has no end marker
/// (install cannot rewrite those either). The native block pins its executable
/// on the `_GCL_BIN=` line (shell-quoted, so any quoting form), where upstream
/// matches `_PINNED='…'`.
fn comparable_block(text: &str, marker: &str, end: &str) -> Option<String> {
    static PINNED: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^_GCL_BIN=.*$").unwrap());
    static VIZ_EXPORT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?m)^export GRAPHICLE_VIZ_NODE_LIMIT=.*\n").unwrap()
    });
    let start = text.find(marker)?;
    let stop = start + text[start..].find(end)?;
    let block = &text[start..stop + end.len()];
    let block = PINNED.replace_all(block, "_GCL_BIN=''");
    Some(VIZ_EXPORT.replace_all(&block, "").into_owned())
}
pub fn status(path: &Path) -> Result<String> {
    let Some(root) = git_root(path) else {
        return Ok("Not in a git repository.".into());
    };
    let dir = user_hooks_dir(&hooks_dir(&root)?);
    let cfg = load_rc(&root).unwrap_or(json!({}));
    let limit = cfg["viz_node_limit"].as_i64();
    let check = |kind: HookKind| {
        let (marker, end) = kind.markers();
        let p = dir.join(kind.name());
        if !p.exists() {
            return "not installed".into();
        }
        let text = read_hook(&p).unwrap_or_default();
        if !text.contains(marker) {
            return "not installed (hook exists but graphicle not found)".into();
        }
        if let Some(limit) = limit {
            let re=regex::Regex::new(r#"export GRAPHICLE_VIZ_NODE_LIMIT="(?:\$\{GRAPHICLE_VIZ_NODE_LIMIT:-(\d+)\}|(\d+))""#).unwrap();
            let installed = re
                .captures(&text)
                .and_then(|c| c.get(1).or(c.get(2)))
                .and_then(|m| m.as_str().parse::<i64>().ok());
            if installed != Some(limit) {
                return format!(
                    "installed (out of date: hook has limit {}, .graphiclerc has {limit})",
                    installed.map(|n| n.to_string()).unwrap_or("unset".into())
                );
            }
        }
        // Upgrading does not touch hooks already on disk, so a block from an
        // older release keeps running its old script (#3771). The block names
        // the executable that installed it; one installed by another program
        // embedding this command line is current when nothing else differs.
        let installed = comparable_block(&text, marker, end);
        let current = comparable_block(
            &script_for(kind, "", None, crate::cli::PROGRAM),
            marker,
            end,
        );
        if let (Some(installed), Some(current)) = (&installed, &current)
            && !crate::cli::same_but_program(installed, current)
        {
            return format!(
                "installed (out of date: run `{} hook install` to refresh it)",
                crate::cli::program()
            );
        }
        "installed".into()
    };
    let mut output = String::new();
    for kind in HOOK_KINDS {
        output.push_str(&format!("{}: {}\n", kind.name(), check(kind)));
    }
    output.push_str(&format!("merge driver: {}", merge_driver_status(&root)));
    if let Some(limit) = limit {
        output.push_str(&format!("\nviz node limit: {limit}"));
    }
    Ok(output)
}
#[cfg(test)]
mod tests {
    use super::{install, status};
    use crate::cli::with_program;
    use std::{path::Path, process::Command};

    fn git_repo(root: &Path) {
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        // Pin the hooks dir locally so a global core.hooksPath never leaks in.
        let hooks = root.join(".git/hooks");
        git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
    }

    /// Hook blocks name the executable that installed them. A block another
    /// program embedding this command line installed is current for this one,
    /// and the other way round; an edited block is still out of date.
    #[test]
    fn hooks_from_another_program_name_are_installed_not_out_of_date() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_repo(repo);
        with_program("embedding-cli", || install(repo)).unwrap();
        let hook = repo.join(".git/hooks/post-commit");
        let text = std::fs::read_to_string(&hook).unwrap();
        assert!(
            text.contains("command -v embedding-cli 2>/dev/null"),
            "{text}"
        );
        let current = "post-commit: installed\npost-checkout: installed\n";
        assert!(status(repo).unwrap().starts_with(current));
        install(repo).unwrap();
        let text = std::fs::read_to_string(&hook).unwrap();
        assert!(
            text.contains("command -v graphicle-graph 2>/dev/null"),
            "{text}"
        );
        let other = with_program("embedding-cli", || status(repo)).unwrap();
        assert!(other.starts_with(current), "{other}");
        std::fs::write(&hook, text.replace("hook launch", "hook-launch")).unwrap();
        let edited = with_program("embedding-cli", || status(repo)).unwrap();
        assert!(
            edited.starts_with(
                "post-commit: installed (out of date: run `embedding-cli hook install` to refresh it)\n"
            ),
            "{edited}"
        );
    }
}
