//! Muse Code, Meta's coding agent: Graphicle as a native Muse plugin.
//!
//! `graphicle connect muse --hooks` writes the plugin into the data folder
//! ([`PLUGIN_FOLDER`]) and installs it with `muse plugins install <folder>
//! --scope user`. It is opt-in (a preview) until it has passed a live check
//! with Muse Code: `graphicle setup` keeps Muse Code's settings entry, and
//! keeps an installed plugin's paths current instead of adding an entry
//! beside it. Muse Code reviews a plugin's hooks and MCP servers: the
//! person approves them once with `muse plugins approve graphicle`. The
//! package is a native Muse plugin (`.muse-plugin/plugin.json`, the contract
//! of Muse Code's own `create-plugin` skill) with three capability families:
//! * the MCP server: `graphicle mcp --project . --or-workspace` with the
//!   flags `connect` chose, the project of the folder Muse Code starts it
//!   in, else every project of the workspace, so it starts in any folder;
//!   it sets `"mode": "optional"` as the settings entry does; Muse Code
//!   accepts the field in a plugin's server (no diagnostics on validate,
//!   install or inspect) but does not show it, so whether it skips a plugin
//!   server that cannot start is not confirmed (`hook_probe.py` checks);
//! * the `graphicle` skill ([`SKILL`]): when to call which tool;
//! * hooks ([`HOOKS`]) that run `graphicle hook muse <event>`: the session
//!   start adds the project's repository map and its reviewed memory, a
//!   prompt adds the evidence for a code question, a batch of searches adds
//!   a pointer to the graph tools, and the end of a turn captures the
//!   session log as a source.
//!
//! The context goes in at the session's start and after each prompt, never
//! before every model call, so the start of the conversation (system
//! prompt, tools, then Graphicle's map and memory) is the same bytes for the
//! whole session and the API's prompt cache keeps serving it. The map is
//! the repository part of a cache-layout pack ([`crate::code_pack`],
//! `--prefix-share 1.0`): the same bytes for every session at a commit.
//! It is built in the background and kept in the data folder
//! ([`session_pack`]), so a session start never waits for a pack.
use crate::{Error, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The plugin's id in Muse Code.
pub const PLUGIN_ID: &str = "graphicle";
/// The folder in the data folder that holds the generated plugin.
pub const PLUGIN_FOLDER: &str = "muse-code-plugin";
/// The folder in the data folder that holds the session packs.
pub const PACK_FOLDER: &str = "muse-code-packs";
/// The `graphicle` skill the plugin carries. The plugin in
/// `packaging/muse-code-plugin` holds the same text.
pub const SKILL: &str = include_str!("muse_code_skill.md");
/// Tokens of the repository map a session starts with, unless the hook's
/// `--pack-tokens` says otherwise (0 leaves it out).
pub const SESSION_PACK_TOKENS: u64 = 8_000;
/// Most tokens `--pack-tokens` may give.
pub const SESSION_PACK_MAX: u64 = 100_000;
/// A session pack younger than this is used without a rebuild.
const PACK_FRESH: std::time::Duration = std::time::Duration::from_secs(600);

/// One hook of the plugin: its id, Muse Code's event, the argument of
/// `graphicle hook muse` and Muse Code's timeout in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Hook {
    pub id: &'static str,
    pub event: &'static str,
    pub argument: &'static str,
    pub timeout_ms: u64,
}

/// The plugin's hooks. Session start waits at most 2.5 s for its context
/// (the timeout leaves room for the system's first-launch check of an
/// updated program); the prompt hook gives up by itself after its budget
/// ([`crate::prompt_hook::budget`]); capture hands the import to a detached
/// process.
pub const HOOKS: [Hook; 6] = [
    Hook {
        id: "session-start",
        event: "SessionStart",
        argument: "session-start",
        timeout_ms: 10_000,
    },
    Hook {
        id: "prompt-context",
        event: "UserPromptSubmit",
        argument: "prompt-context",
        timeout_ms: crate::prompt_hook::HOOK_TIMEOUT * 1000,
    },
    Hook {
        id: "tool-batch",
        event: "PostToolBatch",
        argument: "tool-batch",
        timeout_ms: 5_000,
    },
    Hook {
        id: "stop",
        event: "Stop",
        argument: "stop",
        timeout_ms: 10_000,
    },
    Hook {
        id: "pre-compact",
        event: "PreCompact",
        argument: "pre-compact",
        timeout_ms: 10_000,
    },
    Hook {
        id: "session-end",
        event: "SessionEnd",
        argument: "session-end",
        timeout_ms: 10_000,
    },
];

/// The server flags of the package in `packaging/`: the whole server, with
/// the memory proposals a person reviews (`connect muse --hooks
/// --allow-learning`).
pub const PACKAGED_SERVER_FLAGS: [&str; 1] = ["--allow-learning"];

/// The plugin's manifest. `program` and `data` are the program and data
/// folder its commands run with: the absolute paths `connect muse --hooks`
/// writes, or `graphicle` and `None` for the package in `packaging/`, which
/// runs the `graphicle` on `PATH` with its default data folder.
/// `pack_tokens` sizes the session start's repository map; `server` holds
/// the server's flags after `mcp --project . --or-workspace` (`--tools`,
/// `--allow-learning`, `--work-state`, as `connect` chose them). The server
/// sets `"mode": "optional"`, as the settings entry does, so that Muse Code
/// can skip it when it cannot start (Graphicle uninstalled, its data folder
/// gone). Muse Code accepts the field for a plugin's server but does not show
/// it, so that is not confirmed: `graphicle disconnect muse` removes the
/// plugin before an uninstall.
pub fn manifest(program: &str, data: Option<&Path>, pack_tokens: u64, server: &[&str]) -> Value {
    let prefix = |rest: &[&str]| -> Vec<String> {
        let mut argv = vec![program.to_string()];
        if let Some(data) = data {
            argv.push("--data-dir".into());
            argv.push(data.to_string_lossy().into_owned());
        }
        argv.extend(rest.iter().map(|arg| arg.to_string()));
        argv
    };
    let mut server_args = vec!["mcp", "--project", ".", "--or-workspace"];
    server_args.extend_from_slice(server);
    let hooks: Vec<Value> = HOOKS
        .iter()
        .map(|hook| {
            let mut argv = prefix(&["hook", "muse", hook.argument]);
            if hook.argument == "session-start" && pack_tokens != SESSION_PACK_TOKENS {
                argv.extend(["--pack-tokens".into(), pack_tokens.to_string()]);
            }
            json!({"id":hook.id,"event":hook.event,"command":argv,"timeoutMs":hook.timeout_ms})
        })
        .collect();
    json!({
        "schemaVersion": 1,
        "name": PLUGIN_ID,
        "displayName": "Graphicle",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Graphicle's code graph, whole-repository packs and reviewed project memory for Muse Code, with the context added at session start and per prompt.",
        "compat": {"source": "native", "manifestDir": ".muse-plugin"},
        "capabilities": {
            "skills": [{"id": PLUGIN_ID, "path": "skills/graphicle/SKILL.md", "enabledDefault": true}],
            "commands": [],
            "hooks": hooks,
            "mcpServers": [{
                "id": PLUGIN_ID,
                "transport": "stdio",
                "command": prefix(&server_args),
                "mode": "optional",
            }],
            "reminders": [],
        },
    })
}

/// Write the plugin package into `folder`: the manifest and the skill.
/// Files of the package that are already the same bytes are left alone.
pub fn write_package(folder: &Path, manifest: &Value) -> Result<bool> {
    let files = [
        (
            folder.join(".muse-plugin/plugin.json"),
            format!("{}\n", serde_json::to_string_pretty(manifest)?),
        ),
        (folder.join("skills/graphicle/SKILL.md"), SKILL.to_string()),
    ];
    let mut changed = false;
    for (path, text) in files {
        if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)?;
        changed = true;
    }
    Ok(changed)
}

/// The `muse` program: on `PATH`, else where Muse Code's installer puts it
/// (`~/.local/bin/muse`).
pub fn program() -> Option<PathBuf> {
    let env = crate::agent_detect::Environment::current();
    env.program("muse").or_else(|| {
        crate::platform::home_dir()
            .map(|home| home.join(".local/bin/muse"))
            .filter(|path| path.is_file())
    })
}

/// Run `muse plugins <args> --json`; the reply is its exit status, and its
/// JSON (or its text) from standard output and error.
fn plugins(muse: &Path, args: &[&str]) -> Result<(bool, Value)> {
    let output = std::process::Command::new(muse)
        .arg("plugins")
        .args(args)
        .arg("--json")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| Error::bad(format!("Cannot run {}: {error}", muse.display())))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let reply = serde_json::from_str::<Value>(text.trim()).unwrap_or_else(|_| {
        let mut both = text.trim().to_string();
        let error = String::from_utf8_lossy(&output.stderr);
        if !error.trim().is_empty() {
            both.push_str(if both.is_empty() { "" } else { "\n" });
            both.push_str(error.trim());
        }
        let cut: String = both.chars().take(2000).collect();
        Value::String(cut)
    });
    Ok((output.status.success(), reply))
}

/// Install the package in `folder` for the user, or refresh an earlier
/// install from it (`muse plugins update`). Muse Code keeps its own record
/// of installed plugins; `muse plugins remove graphicle` undoes this.
pub fn install(muse: &Path, folder: &Path) -> Result<Value> {
    let path = folder.to_string_lossy().into_owned();
    let (installed, reply) = plugins(muse, &["install", &path, "--scope", "user"])?;
    if installed {
        return Ok(json!({"state":"installed","muse":reply}));
    }
    // Installed before: refresh it from the folder.
    let (updated, again) = plugins(muse, &["update", PLUGIN_ID])?;
    if updated {
        return Ok(json!({"state":"updated","muse":again}));
    }
    Err(Error::bad(format!(
        "muse plugins install {path} --scope user failed: {}",
        match &reply {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    )))
}

/// Refresh the installed plugin from its folder (`muse plugins update
/// graphicle`), after its package changed.
pub fn update(muse: &Path) -> Result<Value> {
    let (updated, reply) = plugins(muse, &["update", PLUGIN_ID])?;
    if updated {
        return Ok(reply);
    }
    Err(Error::bad(format!(
        "muse plugins update {PLUGIN_ID} failed: {}",
        match &reply {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    )))
}

/// Remove the plugin from Muse Code (`muse plugins remove graphicle`).
pub fn remove(muse: &Path) -> Result<Value> {
    let (removed, reply) = plugins(muse, &["remove", PLUGIN_ID])?;
    Ok(json!({"removed":removed,"muse":reply}))
}

/// Undo the plugin for `graphicle disconnect`: `muse plugins remove
/// graphicle`, then the package Graphicle wrote in the data folder `data`
/// (whose manifest is `manifest`). Without the `muse` program, Muse Code's
/// record stays and the outcome says how to remove it.
pub fn undo(manifest: &Path, data: &Path) -> Result<Value> {
    let package = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Error::bad("Not a plugin manifest"))?;
    let ours = |path: &Path| {
        use crate::platform::CanonicalExt;
        let expected = data.join(PLUGIN_FOLDER);
        path.canonical()
            .ok()
            .is_some_and(|found| Some(found) == expected.as_path().canonical().ok())
    };
    if !ours(package) {
        return Err(Error::bad(format!(
            "{} is not Graphicle's Muse Code plugin; remove it with muse plugins remove {PLUGIN_ID}",
            package.display()
        )));
    }
    let mut removed = Vec::new();
    let mut outcome = json!({"file":manifest,"state":"deleted"});
    match program() {
        Some(muse) => {
            let reply = remove(&muse)?;
            if reply["removed"] == true {
                removed.push(format!("removed the {PLUGIN_ID} plugin from Muse Code"));
            } else {
                outcome["next"] = json!(format!(
                    "Muse Code did not remove the plugin; run muse plugins remove {PLUGIN_ID}"
                ));
            }
            outcome["muse"] = reply["muse"].clone();
        }
        None => {
            outcome["next"] = json!(format!(
                "The muse program was not found: run muse plugins remove {PLUGIN_ID}"
            ));
        }
    }
    std::fs::remove_dir_all(package)?;
    removed.push("deleted the plugin package".to_string());
    outcome["removed"] = json!(removed);
    Ok(outcome)
}

/// Muse Code's session root: `$XDG_DATA_HOME/muse/sessions`, else
/// `~/.local/share/muse/sessions`.
pub fn sessions_root() -> Option<PathBuf> {
    crate::capture::default_root(crate::capture::Client::Muse)
}

/// The session log a hook's payload belongs to: its `transcript_path`, else
/// `MUSE_CURRENT_SESSION_LOG`, else the log of its session id (the
/// payload's `session_id`, else `MUSE_SESSION_ID`) under the session root:
/// `YYYY/MM/DD/<session>/session.jsonl`, sub-agents' logs left out.
pub fn session_log(payload: &Value) -> Option<PathBuf> {
    let given = payload["transcript_path"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("MUSE_CURRENT_SESSION_LOG").map(PathBuf::from));
    if let Some(path) = given.filter(|path| path.is_file()) {
        return Some(path);
    }
    let session = payload["session_id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| std::env::var("MUSE_SESSION_ID").ok())?;
    if session.is_empty()
        || session.len() > 100
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    find_session_log(&sessions_root()?, &session)
}

/// `<root>/YYYY/MM/DD/<session>/session.jsonl`, newest day first.
pub fn find_session_log(root: &Path, session: &str) -> Option<PathBuf> {
    let sorted = |folder: &Path| -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        found.sort();
        found.reverse();
        found
    };
    for year in sorted(root) {
        for month in sorted(&year) {
            for day in sorted(&month) {
                let log = day.join(session).join("session.jsonl");
                if log.is_file() {
                    return Some(log);
                }
            }
        }
    }
    None
}

/// Where the session pack of `project` at `tokens` is kept.
fn pack_path(directory: &Path, project: &str, tokens: u64) -> PathBuf {
    let safe: String = project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(80)
        .collect();
    directory
        .join(PACK_FOLDER)
        .join(format!("{safe}-{tokens}.md"))
}

/// The repository map a session of `project` starts with: the kept session
/// pack, when there is one. A missing pack, or one older than ten minutes,
/// is rebuilt by a detached `graphicle hook muse pack` ([`build_pack`]), so
/// the next session has it; this one is never held up.
pub fn session_pack(directory: &Path, project: &str, tokens: u64) -> Option<String> {
    if tokens == 0 {
        return None;
    }
    let path = pack_path(directory, project, tokens);
    let kept = std::fs::metadata(&path).ok();
    let fresh = kept
        .as_ref()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < PACK_FRESH);
    if !fresh && let Ok(executable) = std::env::current_exe() {
        let mut command = std::process::Command::new(executable);
        command
            .arg("--data-dir")
            .arg(directory)
            .args(["hook", "muse", "pack", "--project", project])
            .args(["--pack-tokens", &tokens.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = crate::platform::spawn_detached(&mut command);
    }
    kept.and_then(|_| std::fs::read_to_string(&path).ok())
        .filter(|text| !text.trim().is_empty())
}

/// Build and keep the session pack of `project` at `tokens`: the repository
/// part of a cache-layout pack with prefix share 1.0 (the same bytes for
/// every question at a commit), written whole and renamed into place. One
/// build at a time per pack: a second one finds the lock and leaves.
pub fn build_pack(store: &crate::store::Store, project: &str, tokens: u64) -> Result<PathBuf> {
    let session = crate::local::session(store, false)?;
    let path = pack_path(&store.directory, project, tokens);
    let folder = path
        .parent()
        .ok_or_else(|| Error::bad("No pack folder"))?
        .to_path_buf();
    std::fs::create_dir_all(&folder)?;
    let lock = path.with_extension("lock");
    // A lock left by a build that died is taken over after ten minutes.
    if std::fs::metadata(&lock)
        .and_then(|meta| meta.modified())
        .is_ok_and(|time| time.elapsed().unwrap_or_default() > PACK_FRESH)
    {
        let _ = std::fs::remove_file(&lock);
    }
    let Ok(_held) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    else {
        return Ok(path);
    };
    let built = (|| -> Result<()> {
        let pack = store.project_pack(
            &session.workspace,
            &session.capability,
            &json!({"repository_id":project,"question":"","budget":tokens,
                "layout":"cache","prefix_share":1.0,"style":"markdown"}),
        )?;
        let text = crate::code_pack::text(&pack);
        let mut file = tempfile::NamedTempFile::new_in(&folder)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        file.persist(&path).map_err(|error| error.error)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&lock);
    built.map(|()| path)
}

/// The tool calls a PostToolBatch payload names: each object with a tool
/// name (`tool_name` or `name`) and its input (`tool_input`, `input`,
/// `args` or `arguments`), at any depth.
fn batch_calls(payload: &Value) -> Vec<(String, Value)> {
    let mut found = Vec::new();
    let mut stack = vec![payload];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                let name = map
                    .get("tool_name")
                    .or(map.get("name"))
                    .and_then(Value::as_str);
                let input = ["tool_input", "input", "args", "arguments"]
                    .iter()
                    .find_map(|key| map.get(*key));
                if let (Some(name), Some(input)) = (name, input) {
                    let input = input
                        .as_str()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok())
                        .unwrap_or_else(|| input.clone());
                    found.push((name.to_string(), input));
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
        if found.len() > 256 {
            break;
        }
    }
    found
}

/// How many calls of a batch searched the repository's text: a search tool
/// (`grep`, `search`, `glob`, `find`) or a shell command that runs one.
pub fn searches(payload: &Value) -> usize {
    batch_calls(payload)
        .iter()
        .filter(|(name, input)| {
            let short = name
                .rsplit(['.', '/'])
                .next()
                .unwrap_or(name)
                .rsplit("__")
                .next()
                .unwrap_or(name)
                .to_ascii_lowercase();
            if short.contains("graphicle") || name.contains("graphicle") {
                return false;
            }
            let shell = ["bash", "shell", "exec_command", "run_shell", "terminal"]
                .iter()
                .any(|tool| short == *tool);
            if shell {
                let command = input["command"]
                    .as_str()
                    .or(input["cmd"].as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        input["command"].as_array().map(|parts| {
                            parts
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                    })
                    .unwrap_or_default();
                return crate::agent_setup::runs_a_search(&command);
            }
            ["grep", "search", "glob", "find"]
                .iter()
                .any(|word| short.contains(word))
        })
        .count()
}

/// The note a batch of searches gets: where Graphicle's tools answer in
/// one call.
pub const SEARCH_NOTE: &str = "Graphicle: this folder is a Graphicle project with a code graph. For a question about how the repository works, one call of graphicle's pack tool returns the relevant code with path:line citations within a token budget; query_graph, get_node and get_neighbors give definitions, callers and callees. Searching the files is fine for an exact string.";

/// The PostToolBatch hook's output for `payload`, if any: [`SEARCH_NOTE`]
/// after a batch with a search, within the nudge budget of the session
/// ([`crate::agent_setup::nudge_allowed`]).
pub fn tool_batch_output(directory: &Path, payload: &Value) -> Option<Value> {
    if searches(payload) == 0 {
        return None;
    }
    if !crate::agent_setup::nudge_allowed(directory, payload, crate::agent_setup::nudge_cap()) {
        return None;
    }
    Some(
        json!({"hookSpecificOutput":{"hookEventName":"PostToolBatch","additionalContext":SEARCH_NOTE}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_a_native_plugin_with_three_families() {
        let manifest = manifest(
            "graphicle",
            None,
            SESSION_PACK_TOKENS,
            &PACKAGED_SERVER_FLAGS,
        );
        assert_eq!(manifest["compat"]["manifestDir"], ".muse-plugin");
        assert_eq!(manifest["name"], PLUGIN_ID);
        let capabilities = &manifest["capabilities"];
        assert_eq!(
            capabilities["mcpServers"][0]["command"],
            json!([
                "graphicle",
                "mcp",
                "--project",
                ".",
                "--or-workspace",
                "--allow-learning"
            ])
        );
        // Optional, as the settings entry is (accepted by Muse Code's
        // validator; whether it skips a failing plugin server is unconfirmed).
        assert_eq!(capabilities["mcpServers"][0]["mode"], "optional");
        let hooks = capabilities["hooks"].as_array().unwrap();
        let events: Vec<&str> = hooks.iter().map(|h| h["event"].as_str().unwrap()).collect();
        assert_eq!(
            events,
            [
                "SessionStart",
                "UserPromptSubmit",
                "PostToolBatch",
                "Stop",
                "PreCompact",
                "SessionEnd"
            ]
        );
        assert_eq!(
            hooks[0]["command"],
            json!(["graphicle", "hook", "muse", "session-start"])
        );
        // Ids follow Muse's portable grammar; argv only, no shell strings.
        for hook in hooks {
            let id = hook["id"].as_str().unwrap();
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-._".contains(c))
            );
            assert!(hook["command"].is_array());
        }
        assert_eq!(
            capabilities["skills"][0]["path"],
            "skills/graphicle/SKILL.md"
        );
        let with_data = manifest_with_data();
        assert_eq!(
            with_data["capabilities"]["mcpServers"][0]["command"],
            json!([
                "/opt/g/graphicle",
                "--data-dir",
                "/d",
                "mcp",
                "--project",
                ".",
                "--or-workspace",
                "--tools",
                "essentials"
            ])
        );
        assert_eq!(
            with_data["capabilities"]["hooks"][0]["command"],
            json!([
                "/opt/g/graphicle",
                "--data-dir",
                "/d",
                "hook",
                "muse",
                "session-start",
                "--pack-tokens",
                "0"
            ])
        );
    }

    fn manifest_with_data() -> Value {
        manifest(
            "/opt/g/graphicle",
            Some(Path::new("/d")),
            0,
            &["--tools", "essentials"],
        )
    }

    #[test]
    fn skill_has_frontmatter_and_names_the_tools() {
        assert!(SKILL.starts_with("---\nname: graphicle\ndescription: "));
        for tool in ["pack", "query_graph", "recall_project", "propose_memory"] {
            assert!(SKILL.contains(tool), "{tool}");
        }
    }

    #[test]
    fn packaging_copy_matches_the_skill_and_manifest() {
        // The package in packaging/ (private tree, then public export).
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        for root in [
            crate_dir.join("../graphicle/public-root/packaging/muse-code-plugin"),
            crate_dir.join("../..").join("packaging/muse-code-plugin"),
        ] {
            if !root.is_dir() {
                continue;
            }
            let skill = std::fs::read_to_string(root.join("skills/graphicle/SKILL.md")).unwrap();
            assert_eq!(skill, SKILL);
            let text = std::fs::read_to_string(root.join(".muse-plugin/plugin.json")).unwrap();
            let mut packaged: Value = serde_json::from_str(&text).unwrap();
            let mut ours = manifest(
                "graphicle",
                None,
                SESSION_PACK_TOKENS,
                &PACKAGED_SERVER_FLAGS,
            );
            // The packaged copy names the release it ships in; a written
            // one names the version of the program that wrote it.
            assert!(packaged["version"].is_string());
            packaged.as_object_mut().unwrap().remove("version");
            ours.as_object_mut().unwrap().remove("version");
            assert_eq!(packaged, ours);
            return;
        }
    }

    #[test]
    fn searches_counts_search_tools_and_shell_searches() {
        let payload = json!({"hook_event_name":"PostToolBatch","session_id":"s",
            "tool_calls":[
                {"tool_name":"grep","tool_input":{"pattern":"Foo"}},
                {"name":"exec_command","args":"{\"cmd\":\"rg -n Foo src\"}"},
                {"name":"shell","input":{"command":["bash","-lc","ls src"]}},
                {"name":"read_file","args":{"path":"a.rs"}},
                {"name":"mcp__graphicle__query_graph","args":{"query":"Foo"}}]});
        assert_eq!(searches(&payload), 2);
        assert_eq!(searches(&json!({"tool_calls":[]})), 0);
    }

    #[test]
    fn finds_a_session_log_by_id_and_skips_others() {
        let root = tempfile::tempdir().unwrap();
        let day = root.path().join("2026/10/27/abc-1");
        std::fs::create_dir_all(day.join("subagent/x")).unwrap();
        std::fs::write(day.join("session.jsonl"), "{}\n").unwrap();
        std::fs::write(day.join("subagent/x/session.jsonl"), "{}\n").unwrap();
        assert_eq!(
            find_session_log(root.path(), "abc-1"),
            Some(day.join("session.jsonl"))
        );
        assert_eq!(find_session_log(root.path(), "x"), None);
        let given = json!({"transcript_path": day.join("session.jsonl")});
        assert_eq!(session_log(&given), Some(day.join("session.jsonl")));
        assert_eq!(session_log(&json!({"session_id":"../etc"})), None);
    }

    #[test]
    fn writes_the_package_once() {
        let folder = tempfile::tempdir().unwrap();
        let manifest = manifest_with_data();
        assert!(write_package(folder.path(), &manifest).unwrap());
        assert!(!write_package(folder.path(), &manifest).unwrap());
        let written: Value = serde_json::from_str(
            &std::fs::read_to_string(folder.path().join(".muse-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written, manifest);
        assert_eq!(
            std::fs::read_to_string(folder.path().join("skills/graphicle/SKILL.md")).unwrap(),
            SKILL
        );
    }
}
