// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/install.py
use crate::{Result, install::*, install_fs as f};
use serde_json::{Value, json};
use std::{fs, path::Path};
fn hook_path(name: &str, o: &Options) -> std::path::PathBuf {
    o.project_dir.join(match name {
        "codex" => ".codex/hooks.json",
        "gemini" => ".gemini/settings.json",
        "codebuddy" => ".codebuddy/settings.json",
        _ => ".claude/settings.json",
    })
}
/// A hook command this engine wrote: it runs the `hook-guard` or
/// `hook-check` subcommand. The executable's name is not compared, so a
/// command other programs installed (the Graphicle runtime's session hooks,
/// for one) is not the engine's, whatever it is called.
fn is_engine_hook_command(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|word| word == "hook-guard" || word == "hook-check")
}
/// `_is_graphify_hook_entry` (#4359): one element of a group's `hooks` list
/// that is an engine command entry. Anything that is not a command object is
/// never the engine's.
fn is_engine_hook_entry(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("command")
        && entry
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(is_engine_hook_command)
}
/// `_strip_graphify_hooks` (#4359): the hook groups with the engine's own
/// entries removed, or `None` when no group holds one. Only those entries go:
/// a user command sharing the engine's group survives, a group is dropped
/// only once nothing is left in it, and a group that is not an object or whose
/// `hooks` is not a list is passed through untouched.
fn strip_engine_hooks(groups: &[Value]) -> Option<Vec<Value>> {
    let mut changed = false;
    let mut kept = Vec::with_capacity(groups.len());
    for group in groups {
        let Some(entries) = group.get("hooks").and_then(Value::as_array) else {
            kept.push(group.clone());
            continue;
        };
        let remaining: Vec<Value> = entries
            .iter()
            .filter(|entry| !is_engine_hook_entry(entry))
            .cloned()
            .collect();
        if remaining.len() == entries.len() {
            kept.push(group.clone());
            continue;
        }
        changed = true;
        if !remaining.is_empty() {
            let mut group = group.clone();
            group["hooks"] = Value::Array(remaining);
            kept.push(group);
        }
    }
    changed.then_some(kept)
}
fn hook_file(name: &str, o: &Options, path: &Path, remove: bool) -> Result<()> {
    if remove && !path.exists() {
        return Ok(());
    }
    let mut cfg = match f::settings(path) {
        Ok(v) => v,
        Err(_) if remove => return Ok(()),
        Err(e) => return Err(e),
    };
    let event = if name == "gemini" {
        "BeforeTool"
    } else {
        "PreToolUse"
    };
    let obj = cfg.as_object_mut().unwrap();
    if remove {
        // A missing or non-list section leaves the file as it is; a changed
        // one is written through the rolling backup (#4359).
        let Some(groups) = obj
            .get_mut("hooks")
            .and_then(Value::as_object_mut)
            .and_then(|hooks| hooks.get_mut(event))
            .and_then(Value::as_array_mut)
        else {
            return Ok(());
        };
        let Some(kept) = strip_engine_hooks(groups) else {
            return Ok(());
        };
        *groups = kept;
        f::write_settings(path, &cfg, true)?;
        return Ok(());
    }
    let hooks = obj
        .entry("hooks")
        .or_insert(json!({}))
        .as_object_mut()
        .ok_or_else(|| f::refuse(path))?;
    let groups = hooks
        .entry(event)
        .or_insert(json!([]))
        .as_array_mut()
        .ok_or_else(|| f::refuse(path))?;
    if let Some(kept) = strip_engine_hooks(groups) {
        *groups = kept;
    }
    groups.extend(match name {
        "gemini" => vec![gemini_hook(o)],
        "codex" => vec![
            json!({"matcher":"Bash","hooks":[{"type":"command","command":format!("{} hook-check",command_executable(o))}]}),
        ],
        _ => claude_hooks(o).as_array().unwrap().clone(),
    });
    f::write_settings(path, &cfg, true)?;
    Ok(())
}
pub fn hook(name: &str, o: &Options, remove: bool) -> Result<()> {
    hook_file(name, o, &hook_path(name, o), remove)?;
    if remove && ["claude", "windows"].contains(&name) {
        hook_file(
            name,
            o,
            &o.project_dir.join(".claude/settings.local.json"),
            true,
        )?;
    }
    Ok(())
}
/// Write (or remove) a project's plugin file. The OpenCode plugin is written
/// atomically into `.opencode/plugins/`, where OpenCode discovers it, and the
/// bare-path entry older releases put in opencode.json is dropped, never
/// written (0.9.84, #3554/#3732). Returns the lines asking for a manual edit.
pub fn plugin(name: &str, project: &Path, remove: bool) -> Result<Vec<String>> {
    if name == "opencode" {
        let file = project.join(OPENCODE_PLUGIN_PATH);
        if remove {
            f::remove(&file)?;
        } else {
            f::write(&file, &constant("_OPENCODE_PLUGIN_JS"))?;
        }
        let (_, by_hand) = drop_opencode_config_entry(project)?;
        return Ok(by_hand.into_iter().collect());
    }
    if name != "kilo" {
        return Err(format!("Unknown plugin: {name}").into());
    }
    let file = project.join(".kilo/plugins/graphicle.js");
    let plain = project.join(".kilo/kilo.json");
    let read_path = if !plain.exists() && plain.with_extension("jsonc").exists() {
        plain.with_extension("jsonc")
    } else {
        plain.clone()
    };
    let entry = reqwest::Url::from_file_path(crate::extractors::resolution::resolved(&file))
        .map_err(|_| "Invalid plugin path")?
        .to_string();
    if remove {
        f::remove(&file)?;
        if !read_path.exists() {
            return Ok(vec![]);
        }
    } else {
        f::write(&file, &constant("_KILO_PLUGIN_JS"))?;
    }
    let mut cfg = f::json_like(&read_path);
    let mut plugins = cfg["plugin"].as_array().cloned().unwrap_or_default();
    if remove {
        let n = plugins.len();
        plugins.retain(|s| s != &entry);
        if n == plugins.len() {
            return Ok(vec![]);
        }
        if plugins.is_empty() {
            cfg.as_object_mut().unwrap().shift_remove("plugin");
        } else {
            cfg["plugin"] = json!(plugins);
        }
    } else {
        if plugins.contains(&json!(entry)) {
            return Ok(vec![]);
        }
        plugins.push(json!(entry));
        cfg["plugin"] = json!(plugins);
    }
    f::write_settings(&plain, &cfg, false)?;
    Ok(vec![])
}
fn md(path: &Path, block: &str, remove: bool) -> Result<()> {
    if remove {
        f::strip_section(path, "## graphicle", "## ")?;
    } else {
        f::update_section(path, block)?;
    }
    Ok(())
}
fn standalone(path: &Path, text: &str, remove: bool) -> Result<()> {
    if remove {
        f::remove(path)?;
    } else {
        f::write(path, text)?;
    }
    Ok(())
}
pub fn configure(name: &str, o: &Options, remove: bool) -> Result<Vec<String>> {
    let name = canonical(name);
    let p = &o.project_dir;
    let mut messages = vec![];
    match name {
        "claude" | "windows" => {
            md(&p.join("CLAUDE.md"), &always_on("claude-md")?, remove)?;
            if remove {
                for rel in ["CLAUDE.local.md", ".claude/CLAUDE.local.md"] {
                    md(&p.join(rel), "", true)?;
                }
            }
            hook(name, o, remove)?;
        }
        "codebuddy" => {
            md(&p.join("CODEBUDDY.md"), &always_on("claude-md")?, remove)?;
            hook(name, o, remove)?;
        }
        "gemini" => {
            md(&p.join("GEMINI.md"), &always_on("gemini-md")?, remove)?;
            hook(name, o, remove)?;
        }
        "vscode" => {
            md(
                &p.join(".github/copilot-instructions.md"),
                &always_on("vscode-instructions")?,
                remove,
            )?;
        }
        "cursor" => standalone(
            &p.join(".cursor/rules/graphicle.mdc"),
            &constant("_CURSOR_RULE"),
            remove,
        )?,
        "kiro" => standalone(
            &p.join(".kiro/steering/graphicle.md"),
            &always_on("kiro-steering")?,
            remove,
        )?,
        "devin" => standalone(
            &p.join(".windsurf/rules/graphicle.md"),
            &constant("_DEVIN_RULES"),
            remove,
        )?,
        "antigravity" | "antigravity-windows" => {
            standalone(
                &p.join(".agents/rules/graphicle.md"),
                &always_on("antigravity-rules")?,
                remove,
            )?;
            standalone(
                &p.join(".agents/workflows/graphicle.md"),
                &constant("_ANTIGRAVITY_WORKFLOW"),
                remove,
            )?;
        }
        "aider" | "amp" | "agents" | "codex" | "opencode" | "kilo" | "claw" | "droid" | "trae"
        | "trae-cn" | "hermes" => {
            md(&p.join("AGENTS.md"), &always_on("agents-md")?, remove)?;
            if name == "codex" {
                hook(name, o, remove)?;
            }
            if ["opencode", "kilo"].contains(&name) {
                messages.extend(plugin(name, p, remove)?);
            }
        }
        "copilot" | "pi" | "kimi" => {}
        _ => return Err(format!("Unknown platform: {name}").into()),
    }
    messages.push(format!(
        "{} graphicle configuration for {name} in {}",
        if remove { "Removed" } else { "Updated" },
        p.display()
    ));
    Ok(messages)
}
/// The Graphicle wordmark, shown where upstream prints its `_BANNER_LOGO`
/// (0.9.71, #3892).
const BANNER_LOGO: [&str; 7] = [
    " ▄█████▄                           ██                  ██",
    "██▀   ▀▀▀ ██▄███  ██████▄ ██▄████▄ ██▄████▄ ▄▄ ▄██████ ██ ▄██████▄",
    "██  █████ ██▀     ▄▄▄▄▄██ ██▀  ▀██ ██▀  ▀██ ██ ██▀     ██ ██▄▄▄▄██",
    "██▄   ▄██ ██     ██▀▀▀▀██ ██▄  ▄██ ██    ██ ██ ██▄     ██ ██▀▀▀▀▀▀",
    " ▀████▀██ ██     ▀██████▀ ██▀████▀ ██    ██ ██ ▀██████ ██ ▀██████▀",
    "                          ██",
    "                          ██",
];
/// `_print_banner`: the green wordmark on `install` and `--help`, only to a
/// terminal; output errors are ignored like the Python `except Exception: pass`.
pub(crate) fn print_banner() {
    use std::io::{IsTerminal, Write};
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return;
    }
    #[cfg(windows)]
    let _ = crate::win::enable_virtual_terminal();
    let (green, dim, reset) = ("\x1b[38;2;17;141;79m", "\x1b[2m", "\x1b[0m");
    let mut lines: Vec<String> = BANNER_LOGO
        .iter()
        .map(|line| format!("  {green}{line}{reset}"))
        .collect();
    // The version sits after the wordmark baseline.
    lines[4].push_str(&format!("  {dim}{}{reset}", crate::VERSION));
    let _ = write!(stdout, "\n{}\n\n", lines.join("\n"));
    let _ = stdout.flush();
}
pub fn install(name: &str, o: &Options) -> Result<Vec<String>> {
    print_banner();
    let name = canonical(name);
    if name == "cursor" {
        return configure(name, o, false);
    }
    let mut actual = o.clone();
    if name == "kiro" {
        actual.project = true;
    }
    if name == "amp" && !o.project {
        let _ = f::remove(&o.home.join(".amp/skills/graphicle"));
    }
    let dst = copy_skill(name, &actual)?;
    let mut messages = vec![format!("Skill installed: {}", dst.display())];
    if ["claude", "windows"].contains(&name) {
        let target = if o.project {
            o.project_dir.join(".claude/CLAUDE.md")
        } else {
            o.claude_config_dir
                .clone()
                .unwrap_or_else(|| o.home.join(".claude"))
                .join("CLAUDE.md")
        };
        let reference = if o.project {
            ".claude/skills/graphicle/SKILL.md".into()
        } else if o.claude_config_dir.is_some() {
            dst.to_string_lossy().into_owned()
        } else {
            "~/.claude/skills/graphicle/SKILL.md".into()
        };
        if let Err(e) = f::register_block(&target, &skill_registration(&reference)) {
            messages.push(format!(
                "Skill installed; registration in {} failed: {e}",
                target.display()
            ));
        }
    }
    if name == "codebuddy"
        && !o.project
        && let Err(e) = f::register_block(
            &o.home.join(".codebuddy/CODEBUDDY.md"),
            &skill_registration("~/.codebuddy/skills/graphicle/SKILL.md"),
        )
    {
        messages.push(e.to_string());
    }
    if name == "kilo" {
        let command = o.home.join(".config/kilo/command/graphicle.md");
        f::write(
            &command,
            &crate::cli::program_text(
                "---\ndescription: Build or query the Graphicle knowledge graph\n---\nFollow the installed graphicle skill. Use graphicle-graph extract . for a build and graphicle-graph query for focused questions.\n",
            ),
        )?;
    }
    if o.project
        || [
            "gemini",
            "vscode",
            "kiro",
            "antigravity",
            "antigravity-windows",
            "amp",
            "agents",
        ]
        .contains(&name)
    {
        // Generic project-only skill installs match the upstream destinations.
        if !(o.project && ["copilot", "pi", "kimi", "agents", "kilo", "codebuddy"].contains(&name))
        {
            messages.extend(configure(name, &actual, false)?);
        }
    } else if name == "opencode" {
        messages.extend(plugin(name, &o.project_dir, false)?);
    }
    if o.project {
        messages.push(format!(
            "Project-scoped install. Add {} to version control.",
            scope_root(&dst, &o.project_dir).display()
        ));
    }
    Ok(messages)
}
pub fn uninstall(name: &str, o: &Options) -> Result<Vec<String>> {
    let name = canonical(name);
    let mut actual = o.clone();
    if name == "kiro" {
        actual.project = true;
    }
    let mut messages = vec![];
    if name != "cursor" && remove_skill(name, &actual)? {
        messages.push(format!(
            "Skill removed: {}",
            destination(name, &actual)?.display()
        ));
    }
    if ["claude", "windows"].contains(&name) && o.project {
        f::strip_section(
            &o.project_dir.join(".claude/CLAUDE.md"),
            "# graphicle",
            "# ",
        )?;
    }
    if name == "kilo" && !o.project {
        let p = o.home.join(".config/kilo/command/graphicle.md");
        f::remove(&p)?;
        if let Some(parent) = p.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    if !(o.project && ["copilot", "pi", "kimi", "agents"].contains(&name)) {
        messages.extend(configure(name, o, true)?);
    }
    Ok(messages)
}
/// `_unsafe_purge_reason` (0.9.86, #4329): why deleting the output folder
/// for `--purge` must not happen, or `None` when it is safe. The folder may
/// legitimately live outside the project (an absolute GRAPHICLE_OUT), so only
/// a target whose deletion takes the project, the home folder or a file
/// system root with it is refused.
pub fn unsafe_purge_reason(out: &Path, project_dir: &Path, home: &Path) -> Option<&'static str> {
    let resolve = |p: &Path| crate::fsutil::resolve_missing(p).unwrap_or_else(|_| p.to_path_buf());
    let (target, project, home) = (resolve(out), resolve(project_dir), resolve(home));
    if project.ancestors().any(|p| p == target) {
        return Some("the project folder or one of its parents");
    }
    if home.ancestors().any(|p| p == target) {
        return Some("your home folder or one of its parents");
    }
    if target.parent().is_none() {
        return Some("a filesystem root");
    }
    None
}
pub fn uninstall_all(o: &Options, purge: bool) -> Result<Vec<String>> {
    let out = o.project_dir.join(crate::paths::OUT_DIR.as_str());
    // Checked before any uninstaller runs, so a refused purge changes nothing.
    if purge && let Some(reason) = unsafe_purge_reason(&out, &o.project_dir, &o.home) {
        let shown = crate::fsutil::resolve_missing(&out).unwrap_or_else(|_| out.clone());
        return Err(format!(
            "refusing --purge: {} is {reason}. Nothing was changed. Check the GRAPHICLE_OUT environment variable.",
            shown.display()
        )
        .into());
    }
    let mut messages = vec![];
    for name in platforms() {
        messages.extend(uninstall(&name, o)?);
    }
    if !o.project
        && let Ok(result) = crate::hooks::uninstall(&o.project_dir)
    {
        messages.push(result);
    }
    if purge && f::remove(&out)? {
        messages.push(format!("Deleted {} (--purge)", out.display()));
    }
    Ok(messages)
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    /// Hook entries are the engine's when they run its `hook-guard` or
    /// `hook-check` subcommand, whatever the executable is called and under
    /// whatever matcher; a hook of another program whose name contains
    /// "graphicle" is not, and neither is anything but a command object.
    #[test]
    fn engine_hooks_are_found_by_subcommand_not_by_name() {
        let entry = |command: &str| json!({"type": "command", "command": command});
        assert!(super::is_engine_hook_entry(&entry(
            "'/opt/bin/embedding-cli' hook-guard search"
        )));
        assert!(super::is_engine_hook_entry(&entry(
            "graphicle-graph hook-check"
        )));
        assert!(!super::is_engine_hook_entry(&entry(
            "graphicle hook session-start"
        )));
        assert!(!super::is_engine_hook_entry(&json!(
            "graphicle-graph hook-check"
        )));
        assert!(!super::is_engine_hook_entry(
            &json!({"type": "prompt", "command": "graphicle-graph hook-check"})
        ));
    }

    /// `_strip_graphify_hooks` (#4359): only the engine's own entries go; a
    /// group is dropped once nothing is left in it, malformed groups pass
    /// through, and no change at all reads as `None`.
    #[test]
    fn strip_drops_only_emptied_groups_and_passes_malformed_through() {
        let groups = vec![
            json!({"matcher": "Bash|Grep", "hooks": [
                {"type": "command", "command": "graphicle-graph hook-guard search"},
                {"type": "command", "command": "my-own-command"}]}),
            json!({"matcher": "Read|Glob", "hooks": [
                {"type": "command", "command": "graphicle-graph hook-guard read"}]}),
            json!("not-a-dict"),
            json!({"matcher": "Bash", "hooks": "not-a-list"}),
        ];
        assert_eq!(
            super::strip_engine_hooks(&groups),
            Some(vec![
                json!({"matcher": "Bash|Grep", "hooks": [{"type": "command", "command": "my-own-command"}]}),
                json!("not-a-dict"),
                json!({"matcher": "Bash", "hooks": "not-a-list"}),
            ])
        );
        assert_eq!(super::strip_engine_hooks(&groups[2..]), None);
    }

    /// Graphify 0.9.94 (#4662): every hook command quotes an executable path
    /// with a space, the Codex PreToolUse hook included, so a shell does not
    /// split the command at the space; the quoted entry is still the engine's.
    #[test]
    fn hook_commands_quote_a_spaced_executable() {
        let project = tempfile::tempdir().unwrap();
        for (windows, quoted) in [
            (false, "'/opt/Program Files/graphicle-graph'"),
            (true, "\"/opt/Program Files/graphicle-graph\""),
        ] {
            let o = crate::install::Options {
                project_dir: project.path().to_path_buf(),
                windows,
                executable: Some("/opt/Program Files/graphicle-graph".into()),
                ..Default::default()
            };
            super::hook("codex", &o, false).unwrap();
            let written =
                std::fs::read_to_string(project.path().join(".codex/hooks.json")).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&written).unwrap();
            let entry = &doc["hooks"]["PreToolUse"][0]["hooks"][0];
            assert_eq!(
                entry["command"],
                format!("{quoted} hook-check"),
                "{written}"
            );
            assert!(super::is_engine_hook_entry(entry));
            for group in crate::install::claude_hooks(&o).as_array().unwrap() {
                let command = group["hooks"][0]["command"].as_str().unwrap();
                assert!(command.starts_with(quoted), "{command}");
            }
            let gemini = crate::install::gemini_hook(&o);
            assert!(
                gemini["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .starts_with(quoted)
            );
        }
    }
}
