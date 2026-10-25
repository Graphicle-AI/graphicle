// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/install.py
//! Native platform skill installation and project configuration.
pub use crate::install_platforms::{
    configure, install, uninstall, uninstall_all, unsafe_purge_reason,
};
use crate::{Result, install_fs as f, paths};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};
pub static DATA: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("assets/install/constants.json")).unwrap());
pub fn platforms() -> Vec<String> {
    DATA["platforms"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .chain(["gemini", "cursor", "vscode"].map(str::to_string))
        .collect()
}
/// The Pi guard package (0.9.89): a Pi / Oh My Pi extension that hands each
/// search and read tool call to the installed `hook-guard` (index.ts) and the
/// package manifest Pi reads (package.json).
pub const PI_GUARD_FILES: [(&str, &str); 2] = [
    ("index.ts", include_str!("assets/install/pi/index.ts")),
    (
        "package.json",
        include_str!("assets/install/pi/package.json"),
    ),
];
/// Where `pi guard` keeps the package: `~/.graphicle-graph/pi`. Upstream
/// ships it inside the wheel; a native build writes it here, naming the
/// running program, and Pi registers this folder.
pub fn pi_guard_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(crate::paths::CONFIG_DIR)
        .join("pi")
}
/// Writes the Pi guard package to [`pi_guard_dir`] (unchanged files are left
/// alone) and returns its folder.
pub fn write_pi_guard() -> Result<PathBuf> {
    let dir = pi_guard_dir();
    fs::create_dir_all(&dir).map_err(crate::Error::from)?;
    for (name, text) in PI_GUARD_FILES {
        let text = crate::cli::program_text(text);
        let path = dir.join(name);
        if fs::read_to_string(&path).ok().as_deref() != Some(text.as_ref()) {
            crate::paths::write_text_atomic(&path, &text).map_err(crate::Error::from)?;
        }
    }
    Ok(dir)
}
/// `pi guard install|uninstall|path` (0.9.89): `path` prints the package
/// folder; `install`/`uninstall` hand it to Pi (`pi install|remove <folder>`),
/// which owns the registration. Without `pi` on PATH nothing is written.
/// Returns the lines to print, the error lines and the exit code.
pub fn pi_guard(action: &str) -> Result<(Vec<String>, Vec<String>, i32)> {
    if action == "path" {
        let dir = write_pi_guard()?;
        return Ok((vec![dir.to_string_lossy().into_owned()], vec![], 0));
    }
    let env: crate::llm_config::Environment = std::env::vars().collect();
    let Some(pi) = crate::llm_config::executable("pi", &env) else {
        return Ok((
            vec![],
            vec!["error: install Pi (pi) and add it to PATH first".into()],
            1,
        ));
    };
    let dir = write_pi_guard()?;
    let verb = if action == "install" {
        "install"
    } else {
        "remove"
    };
    let status = std::process::Command::new(pi)
        .arg(verb)
        .arg(&dir)
        .status()
        .map_err(crate::Error::from)?;
    Ok((vec![], vec![], status.code().unwrap_or(1)))
}
pub fn canonical(name: &str) -> &str {
    if name == "skills" { "agents" } else { name }
}
/// An always-on instruction block, naming the running program.
pub fn always_on(name: &str) -> Result<String> {
    Ok(DATA["always_on"][name]
        .as_str()
        .map(|text| crate::cli::program_text(text).into_owned())
        .ok_or_else(|| format!("Missing always-on block: {name}"))?)
}
/// An installed-file text (rule, plugin, workflow), naming the running program.
pub fn constant(name: &str) -> String {
    crate::cli::program_text(DATA[name].as_str().unwrap_or("")).into_owned()
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub home: PathBuf,
    pub project_dir: PathBuf,
    pub project: bool,
    pub windows: bool,
    pub claude_config_dir: Option<PathBuf>,
    pub local_appdata: Option<PathBuf>,
    pub executable: Option<String>,
    pub strict: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            home: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
            project_dir: PathBuf::from("."),
            project: false,
            windows: cfg!(windows),
            claude_config_dir: std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
            local_appdata: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            executable: None,
            strict: false,
        }
    }
}
pub fn destination(name: &str, o: &Options) -> Result<PathBuf> {
    let name = canonical(name);
    let root = if o.project { &o.project_dir } else { &o.home };
    let rel = match name {
        "gemini" => {
            if !o.project && o.windows {
                ".agents/skills/graphicle/SKILL.md"
            } else {
                ".gemini/skills/graphicle/SKILL.md"
            }
        }
        "opencode" => {
            if o.project {
                ".opencode/skills/graphicle/SKILL.md"
            } else {
                ".config/opencode/skills/graphicle/SKILL.md"
            }
        }
        "hermes" => {
            if !o.project && o.windows {
                return Ok(o
                    .local_appdata
                    .clone()
                    .unwrap_or_else(|| o.home.join("AppData/Local"))
                    .join("hermes/skills/graphicle/SKILL.md"));
            }
            ".hermes/skills/graphicle/SKILL.md"
        }
        "devin" => {
            if o.project {
                ".devin/skills/graphicle/SKILL.md"
            } else {
                ".config/devin/skills/graphicle/SKILL.md"
            }
        }
        "amp" => {
            if o.project {
                ".agents/skills/graphicle/SKILL.md"
            } else {
                ".config/agents/skills/graphicle/SKILL.md"
            }
        }
        "agents" => ".agents/skills/graphicle/SKILL.md",
        "antigravity" | "antigravity-windows" => {
            if o.project {
                ".agents/skills/graphicle/SKILL.md"
            } else {
                ".gemini/config/skills/graphicle/SKILL.md"
            }
        }
        "vscode" => ".copilot/skills/graphicle/SKILL.md",
        _ => {
            if !o.project
                && ["claude", "windows"].contains(&name)
                && let Some(dir) = &o.claude_config_dir
            {
                return Ok(dir.join("skills/graphicle/SKILL.md"));
            }
            DATA["platforms"][name]["skill_dst"]
                .as_str()
                .ok_or_else(|| format!("Unknown platform: {name}"))?
        }
    };
    Ok(root.join(rel))
}
pub fn resolve_executable(o: &Options) -> String {
    if let Some(exe) = &o.executable {
        return exe.replace('\\', "/");
    }
    if o.project {
        return crate::cli::program().into();
    }
    crate::hooks::pinned_executable().replace('\\', "/")
}
pub fn command_executable(o: &Options) -> String {
    let exe = resolve_executable(o);
    if exe
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/_-.:".contains(c))
    {
        exe
    } else if o.windows {
        format!("\"{}\"", exe.replace('"', "\"\""))
    } else {
        crate::hooks::shell_quote(&exe)
    }
}
pub fn claude_hooks(o: &Options) -> Value {
    let exe = command_executable(o);
    json!([{"matcher":"Bash|Grep","hooks":[{"type":"command","command":format!("{exe} hook-guard search"),"timeout":10}]},{"matcher":"Read|Glob","hooks":[{"type":"command","command":format!("{exe} hook-guard read{}",if o.strict{" --strict"}else{""}),"timeout":10}]}])
}
pub fn gemini_hook(o: &Options) -> Value {
    json!({"matcher":"read_file|list_directory","hooks":[{"type":"command","command":format!("{} hook-guard gemini",command_executable(o))}]})
}
pub fn skill_registration(path: &str) -> String {
    format!(
        "\n# graphicle\n- **graphicle** (`{path}`) - any input to knowledge graph. Trigger: `/graphicle`\nWhen the user types `/graphicle`, use the installed graphicle skill or instructions before doing anything else.\n"
    )
}
pub fn write_version(dst: &Path, version: &str) -> Result<()> {
    write_version_named(dst, ".graphicle_version", version)
}
/// The mode bits of a file.
#[cfg(unix)]
fn mode_bits(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}
/// Elsewhere files have no mode bits; nothing reads this value.
#[cfg(not(unix))]
fn mode_bits(_: &fs::Metadata) -> u32 {
    0o600
}
/// The mode [`paths::replace_file`] gives a file that replaces `path`: an
/// existing regular file's own, else `fallback` (also in place of a link).
fn kept_mode(path: &Path, fallback: paths::NewMode) -> paths::NewMode {
    fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map_or(fallback, |m| paths::NewMode::Bits(mode_bits(&m)))
}
/// Graphify `_write_version_stamp` (#3286, #4513): the stamp `name` beside
/// `dst` is written to a private temporary file renamed over the stamp, so a
/// symlink there (or a dangling one) is replaced, never written through. A
/// new stamp, or one in place of a link, keeps the temporary file's 0600, as
/// upstream's `mkstemp` leaves it; an existing regular stamp keeps its mode.
pub fn write_version_named(dst: &Path, name: &str, version: &str) -> Result<()> {
    let stamp = dst.parent().ok_or("Invalid skill path")?.join(name);
    let mode = kept_mode(&stamp, paths::NewMode::Bits(0o600));
    paths::replace_file(&stamp, &format!("{name}."), version.as_bytes(), mode)
        .map_err(crate::Error::from)
}
pub fn copy_skill(name: &str, o: &Options) -> Result<PathBuf> {
    copy_skill_reporting(name, o).map(|(dst, _)| dst)
}
/// [`copy_skill`], also returning where a SKILL.md that differed from the
/// packaged one was kept (`SKILL.md.bak`, #3144).
pub fn copy_skill_reporting(name: &str, o: &Options) -> Result<(PathBuf, Option<PathBuf>)> {
    let dst = destination(name, o)?;
    let packaged = include_str!("assets/install/SKILL.md");
    let content = crate::cli::program_text(packaged);
    let dir = dst.parent().ok_or("Invalid skill path")?;
    // 0.9.72: every writer of a skill directory holds its lock (install and
    // uninstall wait for it; unavailable locking proceeds unlocked).
    let _lock = skill_lock(dir, &o.home, true);
    // Every write is staged under a random name (Graphify 0.9.95, #4513):
    // the references in a private temporary directory, SKILL.md and the
    // version stamp in exclusive temporary files, so a symlink pre-created at
    // a predictable name redirects nothing. SKILL.md and the stamp are then
    // renamed over their own names, as upstream's `os.replace` does: a
    // symlink there (a cloned repository can commit one) is replaced, its
    // target left alone.
    let staging = f::references_staging(dir)?;
    let staged = staging.path().join("references");
    f::write(
        &staged.join("workflows.md"),
        &crate::cli::program_text(include_str!("assets/install/workflows.md")),
    )?;
    f::swap_in_references(&staged, &dir.join("references"))?;
    let mut backup = None;
    // A copy is kept only when it differs from the packaged skill as written
    // for some program: a copy another executable embedding this command line
    // installed differs from this one's only in the command name.
    let modified = |bytes: &[u8]| {
        !std::str::from_utf8(bytes).is_ok_and(|s| crate::cli::same_but_program(s, packaged))
    };
    // A linked SKILL.md is replaced below without touching the file it names,
    // so nothing of that file is copied: the link could name any readable
    // file, and the copy would land in the skill folder (a deliberate
    // difference: upstream's `shutil.copy2` copies it into SKILL.md.bak). The
    // copy itself is renamed over SKILL.md.bak, so a link there is replaced
    // rather than written through (upstream's copy follows it).
    // Nothing is read through a link either (it could name a device).
    let linked = fs::symlink_metadata(&dst).is_ok_and(|m| m.file_type().is_symlink());
    let previous = if linked { None } else { fs::read(&dst).ok() };
    if let Some(bytes) = previous.filter(|b| modified(b.as_slice())) {
        let path = dst.with_extension("md.bak");
        let mode = fs::metadata(&dst).map_or(0o644, |m| mode_bits(&m));
        paths::replace_file(&path, ".SKILL.md.bak.", &bytes, paths::NewMode::Bits(mode))
            .map_err(crate::Error::from)?;
        backup = Some(path);
    }
    let mode = kept_mode(&dst, paths::NewMode::Default);
    paths::replace_file(&dst, ".SKILL.md.", content.as_bytes(), mode)
        .map_err(crate::Error::from)?;
    write_version(&dst, crate::VERSION)?;
    Ok((dst, backup))
}
pub fn remove_skill(name: &str, o: &Options) -> Result<bool> {
    let dst = destination(name, o)?;
    let dir = dst.parent().ok_or("Invalid skill path")?;
    let _lock = skill_lock(dir, &o.home, true);
    let mut removed = f::remove(&dst)?;
    for rel in [".graphicle_version", "references"] {
        removed |= f::remove(&dir.join(rel))?;
    }
    for d in dir.ancestors().take(3) {
        if fs::remove_dir(d).is_err() {
            break;
        }
    }
    Ok(removed)
}
/// The lock file for a skill directory, outside it (Graphify 0.9.72
/// `_skill_lock_path`): `~/.cache/graphicle/skill-locks/` named by the first
/// 16 hex digits of the SHA-256 of the resolved directory path, so every
/// process writing one directory takes the same lock.
pub fn skill_lock_path(dir: &Path, home: &Path) -> PathBuf {
    let real = crate::fsutil::resolve_missing(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf());
    let text = real.to_string_lossy();
    // os.path.normcase: Windows folds case and separators; POSIX keeps the text.
    let text = if cfg!(windows) {
        text.to_lowercase().replace('/', "\\")
    } else {
        text.into_owned()
    };
    let digest = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes())
    );
    home.join(".cache/graphicle/skill-locks")
        .join(format!("{}.lock", &digest[..16]))
}
/// What [`skill_lock`] got: the lock, a busy lock (only without waiting), or
/// no locking at all (no writable cache directory, a file system that
/// refuses locks), where install and uninstall go ahead unlocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillLockState {
    Owned,
    Busy,
    Unavailable,
}
/// An OS file lock (flock / LockFileEx) serializing every writer of one skill
/// directory, released on drop; the OS drops it with a crashed holder, and
/// the lock file is never deleted. Re-entrant per thread.
pub struct SkillLock {
    pub state: SkillLockState,
    held: Option<(PathBuf, fs::File)>,
}
thread_local! {
    static HELD_SKILL_LOCKS: std::cell::RefCell<std::collections::HashSet<PathBuf>> =
        std::cell::RefCell::default();
}
impl Drop for SkillLock {
    fn drop(&mut self) {
        if let Some((path, file)) = self.held.take() {
            HELD_SKILL_LOCKS.with(|held| held.borrow_mut().remove(&path));
            let _ = file.unlock();
        }
    }
}
pub fn skill_lock(dir: &Path, home: &Path, wait: bool) -> SkillLock {
    let lock = |state| SkillLock { state, held: None };
    let path = skill_lock_path(dir, home);
    if HELD_SKILL_LOCKS.with(|held| held.borrow().contains(&path)) {
        return lock(SkillLockState::Owned);
    }
    let file = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| {
            fs::OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(&path)
        });
    let Ok(file) = file else {
        return lock(SkillLockState::Unavailable);
    };
    let state = if wait {
        match file.lock() {
            Ok(()) => SkillLockState::Owned,
            Err(_) => SkillLockState::Unavailable,
        }
    } else {
        match file.try_lock() {
            Ok(()) => SkillLockState::Owned,
            Err(fs::TryLockError::WouldBlock) => SkillLockState::Busy,
            Err(fs::TryLockError::Error(_)) => SkillLockState::Unavailable,
        }
    };
    if state != SkillLockState::Owned {
        return lock(state);
    }
    HELD_SKILL_LOCKS.with(|held| held.borrow_mut().insert(path.clone()));
    SkillLock {
        state,
        held: Some((path, file)),
    }
}
/// The version stamped beside `dst` when it is older than this executable's.
fn stale_stamp(dst: &Path) -> Option<String> {
    let dir = dst.parent()?;
    let stamp = dir.join(".graphicle_version");
    if !(stamp.is_file() && dst.is_file()) {
        return None;
    }
    let installed = fs::read_to_string(&stamp).ok()?.trim().to_string();
    if installed == crate::VERSION || version_tuple(&installed) > version_tuple(crate::VERSION) {
        return None;
    }
    Some(installed)
}
/// Graphify 0.9.72 `_refresh_stale_skills` (#1805): before a command runs,
/// re-copy each user-scope skill left at an older version stamp, returning
/// the stderr lines. Upstream's limits hold: nothing is installed that was
/// not there, a newer stamp is never downgraded, a directory with more than
/// one writer (resolved path; the VS Code copy shares copilot's) is left to
/// the version warning, and a directory another process holds is skipped.
/// `GRAPHICLE_NO_AUTO_REFRESH=1` (or `true`/`yes`) turns it off.
pub fn refresh_stale_skills(o: &Options) -> Vec<String> {
    if auto_refresh_disabled() {
        return vec![];
    }
    let user = Options {
        project: false,
        ..o.clone()
    };
    // The claude/windows and antigravity pairs write one directory and differ
    // only in the host shell: keep the variant a plain install picks here.
    let other_os: [&str; 2] = if o.windows {
        ["claude", "antigravity"]
    } else {
        ["windows", "antigravity-windows"]
    };
    let mut names = vec!["vscode".to_string()];
    // Every platform with a user-scope skill directory, gemini included.
    for name in DATA["platforms"]
        .as_object()
        .into_iter()
        .flat_map(|platforms| platforms.keys().map(String::as_str))
        .chain(["gemini"])
    {
        if !other_os.contains(&name) && !names[1..].iter().any(|seen| seen == name) {
            names.push(name.to_string());
        }
    }
    let mut writers: Vec<(PathBuf, Vec<(String, PathBuf)>)> = vec![];
    for name in names {
        let Ok(dst) = destination(&name, &user) else {
            continue;
        };
        let real = crate::fsutil::resolve_missing(&dst).unwrap_or_else(|_| dst.clone());
        match writers.iter_mut().find(|(path, _)| *path == real) {
            Some((_, entries)) => entries.push((name, dst)),
            None => writers.push((real, vec![(name, dst)])),
        }
    }
    let mut messages = vec![];
    for (_, entries) in writers {
        let [(name, dst)] = entries.as_slice() else {
            continue;
        };
        let Some(dir) = dst.parent() else {
            continue;
        };
        if name == "vscode" || stale_stamp(dst).is_none() {
            continue;
        }
        // Without waiting; the stamp is read again under the lock, since a
        // process that held it may have refreshed or removed the copy.
        let lock = skill_lock(dir, &o.home, false);
        let Some(installed) = (lock.state == SkillLockState::Owned)
            .then(|| stale_stamp(dst))
            .flatten()
        else {
            continue;
        };
        let (dir, version) = (dir.display(), crate::VERSION);
        match copy_skill_reporting(name, &user) {
            Ok((_, backup)) => {
                messages.push(format!(
                    "graphicle: refreshed skill at {dir} ({installed} -> {version}); set GRAPHICLE_NO_AUTO_REFRESH=1 to disable"
                ));
                if let Some(backup) = backup {
                    messages.push(format!(
                        "  previous copy    ->  {} (differed from the packaged skill)",
                        backup.display()
                    ));
                }
            }
            // The stamp stays old, so the version warning names the fix.
            Err(e) => messages.push(format!(
                "graphicle: could not refresh skill at {dir} ({installed} -> {version}): {e}"
            )),
        }
    }
    messages
}
/// `GRAPHICLE_NO_AUTO_REFRESH=1` (or `true`/`yes`): the CLI leaves stale
/// skill copies and old OpenCode plugins alone.
fn auto_refresh_disabled() -> bool {
    let opt_out = std::env::var("GRAPHICLE_NO_AUTO_REFRESH").unwrap_or_default();
    matches!(opt_out.trim().to_lowercase().as_str(), "1" | "true" | "yes")
}
/// The OpenCode plugin, relative to the project (or home) it was installed
/// in, and the config file older releases registered it in.
pub const OPENCODE_PLUGIN_PATH: &str = ".opencode/plugins/graphicle.js";
pub const OPENCODE_CONFIG_PATH: &str = ".opencode/opencode.json";
/// The first line of every OpenCode plugin the engine has written.
pub const OPENCODE_PLUGIN_HEADER: &str = "// Graphicle OpenCode plugin";
/// `_opencode_plugin_is_v1_only`: a plugin written before the OpenCode 2
/// entrypoint. Those exported a named function and nothing else; OpenCode 2
/// needs a default export and rejects the file ("Plugin must export a default
/// definition", #3554/#3732).
pub fn opencode_plugin_is_v1_only(body: &str) -> bool {
    body.starts_with(OPENCODE_PLUGIN_HEADER) && !body.contains("export default")
}
/// `_opencode_config_names_plugin`: opencode.json still mentions the bare
/// plugin path.
pub fn opencode_config_names_plugin(project_dir: &Path) -> bool {
    fs::read_to_string(project_dir.join(OPENCODE_CONFIG_PATH))
        .is_ok_and(|raw| raw.contains(&format!("\"{OPENCODE_PLUGIN_PATH}\"")))
}
/// `_drop_opencode_config_entry`: remove the plugin entry older releases
/// wrote into opencode.json (`plugin` or `plugins`). OpenCode loads every
/// file in `.opencode/plugins/` on its own, so the entry was never needed,
/// and OpenCode 2 reads a bare path as an npm package name and fails to
/// install it (#3732). Returns whether the config changed, and, for a file
/// that is not plain JSON (OpenCode accepts comments, a rewrite would drop
/// them) but still names the plugin, the line asking for the edit by hand.
pub fn drop_opencode_config_entry(project_dir: &Path) -> Result<(bool, Option<String>)> {
    let config_file = project_dir.join(OPENCODE_CONFIG_PATH);
    let Ok(raw) = fs::read_to_string(&config_file) else {
        return Ok((false, None));
    };
    let Ok(mut config) = serde_json::from_str::<Value>(&raw) else {
        let named = raw.contains(&format!("\"{OPENCODE_PLUGIN_PATH}\""));
        return Ok((false, named.then(|| format!(
            "  {OPENCODE_CONFIG_PATH}  ->  not plain JSON, left as is; remove \"{OPENCODE_PLUGIN_PATH}\" from its plugin list by hand"
        ))));
    };
    let Some(object) = config.as_object_mut() else {
        return Ok((false, None));
    };
    let mut changed = false;
    for key in ["plugin", "plugins"] {
        let Some(plugins) = object.get(key).and_then(Value::as_array) else {
            continue;
        };
        if !plugins.iter().any(|p| p == OPENCODE_PLUGIN_PATH) {
            continue;
        }
        let kept: Vec<Value> = plugins
            .iter()
            .filter(|p| *p != OPENCODE_PLUGIN_PATH)
            .cloned()
            .collect();
        if kept.is_empty() {
            object.shift_remove(key);
        } else {
            object.insert(key.into(), Value::Array(kept));
        }
        changed = true;
    }
    if changed {
        // Not ASCII-escaped: the user's non-ASCII values stay readable.
        let mut text = vec![];
        crate::python_json::write(&mut text, &config, Some(2), false)
            .map_err(crate::Error::from)?;
        f::write(
            &config_file,
            &String::from_utf8(text).map_err(crate::Error::from)?,
        )?;
    }
    Ok((changed, None))
}
/// `_opencode_loads_current_plugin` on `opencode --version` output: the
/// plugin the engine writes now is a default export, which OpenCode reads from
/// 1.3.4 on. No version in the output counts as no.
pub fn opencode_version_loads_current_plugin(output: &str) -> bool {
    static VERSION: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(\d+)\.(\d+)\.(\d+)").unwrap());
    VERSION
        .captures(output)
        .and_then(|c| {
            let part = |i: usize| c[i].parse::<u64>().ok();
            Some((part(1)?, part(2)?, part(3)?))
        })
        .is_some_and(|version| version >= (1, 3, 4))
}
/// `opencode` on PATH, as `shutil.which` finds it.
fn opencode_executable() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["opencode.exe", "opencode.cmd", "opencode.bat", "opencode"]
    } else {
        &["opencode"]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                path.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                path.is_file()
            }
        })
}
/// `_opencode_loads_current_plugin`: whether the `opencode` on PATH can load
/// the plugin the engine writes now. Older OpenCode 1 releases call every
/// export as a function and fail on it, while the plugin they have keeps
/// working, so it is left alone. Unknown (no `opencode`, no answer within
/// 10 s, no version) counts as no: `opencode install` still rewrites it.
pub fn opencode_loads_current_plugin() -> bool {
    let Some(exe) = opencode_executable() else {
        return false;
    };
    let mut command = std::process::Command::new(exe);
    command.arg("--version");
    crate::process::capture(&mut command, std::time::Duration::from_secs(10)).is_ok_and(|out| {
        opencode_version_loads_current_plugin(&String::from_utf8_lossy(&out.stdout))
    })
}
/// Graphify 0.9.84 `_refresh_v1_opencode_plugins` (#3554, #3732): before a
/// command runs, rewrite OpenCode plugins an older release left behind, in
/// the working directory (where the install writes it) and the home folder
/// (where it lands when the install ran from there). Returns the stderr lines.
/// `GRAPHICLE_NO_AUTO_REFRESH=1` (or `true`/`yes`) turns it off.
pub fn refresh_v1_opencode_plugins(o: &Options) -> Vec<String> {
    if auto_refresh_disabled() {
        return vec![];
    }
    let Ok(cwd) = std::env::current_dir() else {
        return vec![];
    };
    refresh_opencode_plugins_in(
        &[cwd, o.home.clone()],
        &o.home,
        opencode_loads_current_plugin,
    )
}
/// [`refresh_v1_opencode_plugins`] over `directories` (each resolved, checked
/// once), with `loads_current` deciding whether the installed OpenCode can
/// load the current plugin; it is asked once, and only when an old plugin is
/// found. Only a plugin the engine wrote is touched. An old plugin is replaced
/// only when OpenCode can load the new one; the stale opencode.json entry is
/// dropped beside a current plugin too (a checkout or a failed write can bring
/// it back alone). Each directory is handled under its lock, without waiting:
/// one another process holds is left to it. A failure is reported and never
/// stops the command.
pub fn refresh_opencode_plugins_in(
    directories: &[PathBuf],
    home: &Path,
    loads_current: impl Fn() -> bool,
) -> Vec<String> {
    let mut unique: Vec<PathBuf> = vec![];
    for directory in directories {
        let Ok(directory) = crate::fsutil::resolve_missing(directory) else {
            return vec![];
        };
        if !unique.contains(&directory) {
            unique.push(directory);
        }
    }
    let mut loadable = None;
    let mut messages = vec![];
    for directory in unique {
        let plugin_file = directory.join(OPENCODE_PLUGIN_PATH);
        // Some(true) for an old plugin, Some(false) for a stale entry only.
        let pending = || -> Option<bool> {
            let body = fs::read_to_string(&plugin_file).ok()?;
            if !body.starts_with(OPENCODE_PLUGIN_HEADER) {
                return None;
            }
            if opencode_plugin_is_v1_only(&body) {
                return Some(true);
            }
            opencode_config_names_plugin(&directory).then_some(false)
        };
        let Some(stale) = pending() else {
            continue;
        };
        if stale && !*loadable.get_or_insert_with(&loads_current) {
            continue;
        }
        let Some(dir) = plugin_file.parent() else {
            continue;
        };
        let lock = skill_lock(dir, home, false);
        // Look again under the lock: a process that held it may have finished.
        let Some(stale) = (lock.state == SkillLockState::Owned)
            .then(pending)
            .flatten()
        else {
            continue;
        };
        let result = if stale {
            crate::install_platforms::plugin("opencode", &directory, false)
                .map(|lines| (false, lines))
        } else {
            drop_opencode_config_entry(&directory).map(|(dropped, _)| (dropped, vec![]))
        };
        drop(lock);
        match result {
            // A refresh must never take the user's actual command down with it.
            Err(e) => messages.push(format!(
                "graphicle: could not refresh {}: {e}",
                plugin_file.display()
            )),
            Ok((_, lines)) if stale => {
                messages.push(format!(
                    "graphicle: refreshed {} so OpenCode 2 can load it; set GRAPHICLE_NO_AUTO_REFRESH=1 to disable",
                    plugin_file.display()
                ));
                // Of the install's own lines, only the one asking for a manual edit.
                messages.extend(lines.into_iter().filter(|line| line.contains("by hand")));
            }
            Ok((true, _)) => messages.push(format!(
                "graphicle: removed the stale graphicle entry from {}; set GRAPHICLE_NO_AUTO_REFRESH=1 to disable",
                directory.join(OPENCODE_CONFIG_PATH).display()
            )),
            Ok(_) => {}
        }
    }
    messages
}
pub fn scope_root(path: &Path, project: &Path) -> PathBuf {
    path.strip_prefix(project)
        .ok()
        .and_then(|p| p.components().next())
        .map(|p| project.join(p))
        .unwrap_or_else(|| path.into())
}
pub fn version_tuple(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|s| {
            s.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}
pub fn check_skill_version(dst: &Path, names: &[String]) -> Vec<String> {
    let mut messages = vec![];
    let Some(dir) = dst.parent() else {
        return messages;
    };
    let Ok(installed) = f::read(&dir.join(".graphicle_version")) else {
        return messages;
    };
    if !dst.exists() {
        return vec![format!(
            "Skill directory exists but SKILL.md is missing; run {} install to repair.",
            crate::cli::program()
        )];
    }
    if f::read(dst).is_ok_and(|s| s.contains("references/")) && !dir.join("references").exists() {
        messages.push(format!(
            "Skill references/ sidecar is missing; run {} install to repair.",
            crate::cli::program()
        ));
    }
    if installed.trim() != crate::VERSION {
        let action = if version_tuple(installed.trim()) > version_tuple(crate::VERSION) {
            "Upgrade the native executable before reinstalling this skill.".into()
        } else {
            format!(
                "Run {} install{} to update it.",
                crate::cli::program(),
                names
                    .first()
                    .map(|s| format!(" --platform {s}"))
                    .unwrap_or_default()
            )
        };
        messages.push(format!(
            "Skill at {} is from graphicle {}; executable implements {}. {action}",
            dir.display(),
            installed.trim(),
            crate::VERSION
        ));
    }
    messages
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let o: Options = serde_json::from_value(r.get("options").cloned().unwrap_or(json!({})))
        .map_err(crate::Error::from)?;
    let s = |k| r.get(k).and_then(Value::as_str).unwrap_or("");
    let p = |k| Path::new(r.get(k).and_then(Value::as_str).unwrap_or("."));
    Ok(match s("action") {
        "platforms" => json!(platforms()),
        "canonical" => json!(canonical(s("platform"))),
        "destination" => json!(destination(s("platform"), &o)?),
        "always_on" => json!(always_on(s("name"))?),
        "constant" => json!(constant(s("name"))),
        "version_tuple" => json!(version_tuple(s("version"))),
        "check_version" => json!(check_skill_version(
            p("path"),
            &serde_json::from_value::<Vec<String>>(
                r.get("platform_names").cloned().unwrap_or(json!([]))
            )
            .map_err(crate::Error::from)?
        )),
        "replace_section" => json!(f::replace_section(
            s("content"),
            s("marker"),
            s("new_section")
        )),
        "remove_section" => json!(f::remove_section(
            s("content"),
            s("marker"),
            r["boundary"].as_str().unwrap_or("## ")
        )),
        "strip_section" => json!(f::strip_section(
            p("path"),
            r["marker"].as_str().unwrap_or("## graphicle"),
            r["boundary"].as_str().unwrap_or("## ")
        )?),
        "strip_json_comments" => json!(f::strip_json_comments(s("raw"))),
        "load_json_like" => f::json_like(p("path")),
        "settings" => f::settings(p("path"))?,
        "write_settings" => {
            f::write_settings(p("path"), &r["settings"], true)?;
            Value::Null
        }
        "register_block" => json!(f::register_block(p("path"), s("registration"))?),
        "scope_root" => json!(scope_root(p("path"), p("project_dir"))),
        "copy_skill" => json!(copy_skill(s("platform"), &o)?),
        "remove_skill" => json!(remove_skill(s("platform"), &o)?),
        "write_version" => {
            write_version_named(
                p("path"),
                r["name"].as_str().unwrap_or(".graphicle_version"),
                s("version"),
            )?;
            Value::Null
        }
        "install_refs" => {
            f::install_refs(p("path"), p("source"))?;
            Value::Null
        }
        "claude_hooks" => claude_hooks(&o),
        "gemini_hook" => gemini_hook(&o),
        "skill_registration" => json!(skill_registration(
            r["path"]
                .as_str()
                .unwrap_or("~/.claude/skills/graphicle/SKILL.md")
        )),
        "resolve_executable" => json!(resolve_executable(&o)),
        "install" => json!(install(s("platform"), &o)?),
        "uninstall" => json!(uninstall(s("platform"), &o)?),
        "uninstall_all" => json!(uninstall_all(&o, r["purge"].as_bool().unwrap_or(false))?),
        "configure" => json!(configure(
            s("platform"),
            &o,
            r["remove"].as_bool().unwrap_or(false)
        )?),
        "hook" => {
            crate::install_platforms::hook(
                s("platform"),
                &o,
                r["remove"].as_bool().unwrap_or(false),
            )?;
            Value::Null
        }
        "plugin" => json!(crate::install_platforms::plugin(
            s("platform"),
            &o.project_dir,
            r["remove"].as_bool().unwrap_or(false),
        )?),
        "unsafe_purge_reason" => json!(crate::install_platforms::unsafe_purge_reason(
            p("out"),
            p("project_dir"),
            &o.home
        )),
        x => return Err(format!("Unknown install action: {x}").into()),
    })
}
