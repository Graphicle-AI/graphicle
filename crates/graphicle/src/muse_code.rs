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
//!
//! Muse Code reads none of a hook's output past [`HOOK_OUTPUT_CEILING`]
//! bytes: it stops the hook, and the session starts without its context. The
//! session start keeps its output within [`HOOK_OUTPUT_BYTES`]
//! ([`session_start`]), leaving out the least important parts first and
//! saying so.
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
/// `--pack-tokens` says otherwise (0 leaves it out). Muse Code takes at most
/// [`HOOK_OUTPUT_CEILING`] bytes of a hook's output. Measured on 31 October
/// 2026, a map of 2,500 tokens takes 8,464 bytes in JSON for the program's
/// own Rust sources and 10,580 for Graphicle's Python tools, which
/// leaves room for the project's memory in [`HOOK_OUTPUT_BYTES`]; at 8,000
/// tokens, as in 0.1.9 and 0.1.10, the maps took 27,551 and 33,570 bytes.
pub const SESSION_PACK_TOKENS: u64 = 2_500;
/// Most tokens `--pack-tokens` may give: a larger map would leave no room
/// for the project's memory.
pub const SESSION_PACK_MAX: u64 = 3_500;
/// A session pack younger than this is used without a rebuild.
const PACK_FRESH: std::time::Duration = std::time::Duration::from_secs(600);
/// Muse Code's limit on what a plugin's hook prints, in bytes. Muse Code
/// 1.4.14 stops a hook whose standard output passes it and reads none of
/// that output (`output_too_large`), so the session starts without the
/// hook's context.
pub const HOOK_OUTPUT_CEILING: usize = 16_384;
/// The most bytes the session start prints for Muse Code, its newline
/// included: [`HOOK_OUTPUT_CEILING`] less room to spare.
pub const HOOK_OUTPUT_BYTES: usize = 14_000;
const _: () = assert!(HOOK_OUTPUT_BYTES < HOOK_OUTPUT_CEILING);

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
/// every question at a commit), written whole and renamed into place. It is
/// laid out relevant-first, the most connected files first, the order in
/// which a session start that has to cut it keeps them ([`session_start`]).
/// One build at a time per pack: a second one finds the lock and leaves.
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
                "layout":"cache","prefix_share":1.0,"style":"markdown",
                "order":"relevant-first"}),
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

/// The output of `graphicle hook <client> session-start` for `context`, the
/// same for every client: one line of JSON.
pub fn start_output(context: &str) -> Value {
    json!({"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":context}})
}

/// The bytes the session start prints for `context`: [`start_output`] and
/// its newline.
fn printed_bytes(context: &str) -> usize {
    start_output(context).to_string().len() + 1
}

/// The bytes `text` takes inside a JSON string, as [`start_output`] writes
/// it (six a byte at most).
fn escaped_bytes(text: &str) -> usize {
    serde_json::to_string(text).map_or(text.len() * 6, |quoted| quoted.len() - 2)
}

/// What a session start left out of its map.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MapCut {
    /// Nothing: the map is whole, or there is none.
    Nothing,
    /// Some of its parts.
    Part,
    /// All of it.
    Whole,
}

/// The last line of a session start that left something out: what went,
/// and which tools return it. `lines` memory lines went too.
fn left_out(map: MapCut, lines: usize) -> String {
    let memory = match lines {
        1 => "the last line of project memory".to_string(),
        lines => format!("the last {lines} lines of project memory"),
    };
    let what = match (map, lines) {
        (MapCut::Part, _) => "part of the repository map".to_string(),
        (MapCut::Whole, 0) => "the repository map".to_string(),
        (MapCut::Whole, _) => format!("the repository map and {memory}"),
        (MapCut::Nothing, _) => memory,
    };
    let tools = match (map == MapCut::Nothing, lines == 0) {
        (false, true) => "the pack tool returns the repository's code",
        (true, _) => "project_memory and session_timeline return the rest",
        (false, false) => {
            "the pack tool returns the repository's code, project_memory and session_timeline the rest"
        }
    };
    format!(
        "Graphicle left out {what} to stay within Muse Code's limit on a hook's output (16,384 bytes); {tools}."
    )
}

/// The parts of the Markdown session pack `map`, in order: its header, then
/// each section (`## `) and chunk (`### `), cut where those headings start
/// outside fenced blocks.
fn map_parts(map: &str) -> Vec<&str> {
    let mut cuts = vec![0];
    // The open fence's length in backticks (a pack's fence is longer than
    // any run of backticks in its body).
    let mut fence: Option<usize> = None;
    let mut offset = 0;
    for line in map.split_inclusive('\n') {
        let text = line.trim_end();
        let ticks = text.len() - text.trim_start_matches('`').len();
        match fence {
            Some(length) if ticks >= length && text[ticks..].trim().is_empty() => fence = None,
            Some(_) => {}
            None if offset > 0 && (text.starts_with("## ") || text.starts_with("### ")) => {
                cuts.push(offset);
            }
            None if ticks >= 3 => fence = Some(ticks),
            None => {}
        }
        offset += line.len();
    }
    cuts.push(map.len());
    cuts.windows(2).map(|pair| &map[pair[0]..pair[1]]).collect()
}

/// The sections of a session map from the one a pack over its budget keeps
/// longest to the one it drops first ([`crate::code_pack`]'s repository
/// part): parts of a section further down this list go first.
const MAP_KEEPS: [crate::code_pack::Section; 4] = [
    crate::code_pack::Section::Architecture,
    crate::code_pack::Section::Map,
    crate::code_pack::Section::Docs,
    crate::code_pack::Section::Breadth,
];

/// What fits of `map` in `room` bytes of JSON, in the map's own order: its
/// header, then its parts ([`map_parts`]) taken by [`MAP_KEEPS`], each
/// section's in the map's order (the files from the most connected), while
/// they fit. A section's heading goes in with the first part of it that
/// does. `None` when nothing past the header fits.
fn map_fit(map: &str, room: usize) -> Option<String> {
    let parts = map_parts(map);
    // Each part's section: the index of the `## ` part it is in.
    let mut section = Vec::with_capacity(parts.len());
    let mut current = None;
    for (index, part) in parts.iter().enumerate() {
        if part.starts_with("## ") {
            current = Some(index);
        }
        section.push(current);
    }
    let heading_only = |part: &str| {
        part.starts_with("## ") && part.lines().skip(1).all(|line| line.trim().is_empty())
    };
    let rank = |index: usize| {
        let Some(at) = section[index] else {
            return 0;
        };
        let title = parts[at].lines().next().unwrap_or("")[3..].trim();
        MAP_KEEPS
            .iter()
            .position(|kept| kept.title() == title)
            .map_or(MAP_KEEPS.len() + 1, |position| position + 1)
    };
    let mut order: Vec<usize> = (0..parts.len())
        .filter(|&index| !heading_only(parts[index]))
        .collect();
    order.sort_by_key(|&index| (rank(index), index));
    let mut chosen = vec![false; parts.len()];
    let mut used = 0;
    for index in order {
        let heading = section[index].filter(|&at| at != index && !chosen[at]);
        let bytes = escaped_bytes(parts[index]) + heading.map_or(0, |at| escaped_bytes(parts[at]));
        if used + bytes > room {
            if index == 0 {
                return None;
            }
            continue;
        }
        used += bytes;
        chosen[index] = true;
        if let Some(at) = heading {
            chosen[at] = true;
        }
    }
    if !chosen.iter().skip(1).any(|&kept| kept) {
        return None;
    }
    let kept: String = parts
        .iter()
        .zip(&chosen)
        .filter(|(_, kept)| **kept)
        .map(|(part, _)| *part)
        .collect();
    Some(kept.trim_end().to_string())
}

/// Muse Code's session-start context: the repository's `map` (the kept
/// session pack, [`session_pack`]) first, then `context` (the line on when
/// to use Graphicle, then the project's memory:
/// [`crate::store::Store::session_start_context`]), within
/// [`HOOK_OUTPUT_BYTES`] once printed ([`printed_bytes`]). What does not fit
/// goes, least important first: parts of the map, in the order a pack over
/// its budget drops them ([`map_fit`]: its least connected files, then its
/// documentation, folder map and architecture summary), then the whole map,
/// then the memory's last lines (the recent turns before the memories, a
/// heading whose lines all went with them), never the line on when to use
/// Graphicle. A last line then says what was left out and which tools
/// return it. Empty only if not even that line fits.
pub fn session_start(map: Option<&str>, context: &str) -> String {
    let map = map.map(str::trim_end).filter(|map| !map.is_empty());
    let whole = match map {
        Some(map) => format!("{map}\n\n{context}"),
        None => context.to_string(),
    };
    if printed_bytes(&whole) <= HOOK_OUTPUT_BYTES {
        return whole;
    }
    // Room for the map and the memory: the note's longest form and the line
    // breaks around it are set aside (a line break is 2 bytes in JSON).
    let reserved = printed_bytes("") + escaped_bytes(&left_out(MapCut::Whole, usize::MAX)) + 4;
    let room = HOOK_OUTPUT_BYTES.saturating_sub(reserved);
    let mut lines: Vec<&str> = context.split_inclusive('\n').collect();
    let mut memory = escaped_bytes(context);
    let (kept, cut) = match map {
        None => (None, MapCut::Nothing),
        Some(map) => match map_fit(map, room.saturating_sub(memory + 4)) {
            Some(fitted) => (Some(fitted), MapCut::Part),
            None => (None, MapCut::Whole),
        },
    };
    let mut dropped = 0;
    if kept.is_none() {
        while lines.len() > 1 && memory > room {
            memory -= lines.pop().map_or(0, escaped_bytes);
            dropped += 1;
        }
        // A heading whose lines all went goes with them.
        while dropped > 0
            && lines.len() > 1
            && lines
                .last()
                .is_some_and(|line| line.trim_end().ends_with(':'))
        {
            lines.pop();
            dropped += 1;
        }
    }
    let mut out = String::new();
    if let Some(fitted) = &kept {
        out.push_str(fitted);
        out.push_str("\n\n");
    }
    out.extend(lines.iter().copied());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&left_out(cut, dropped));
    out.push('\n');
    if printed_bytes(&out) <= HOOK_OUTPUT_BYTES {
        return out;
    }
    // Not reached while the sizes above hold; the limit is kept regardless.
    lines
        .first()
        .map(|line| line.to_string())
        .filter(|line| printed_bytes(line) <= HOOK_OUTPUT_BYTES)
        .unwrap_or_default()
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

    /// A breadth chunk of a session map: 30 numbered lines with quotes, a
    /// backslash and non-ASCII text, which JSON and UTF-8 make longer.
    fn chunk(file: usize, centrality: usize) -> String {
        let mut text = format!(
            "### src/f{file}.rs lines 1-30 (whole file)\nwhy: breadth: centrality {centrality}\n```rust\n"
        );
        for line in 1..=30 {
            text.push_str(&format!("{line}\tlet v{line} = \"naïve \\\\ 東京\";\n"));
        }
        text.push_str("```\n\n");
        text
    }

    /// The README chunk of a session map: its body holds a heading and a
    /// fence of its own.
    const README: &str = "### README.md lines 1-5\nwhy: readme\n````markdown\n## Install\n```sh\nmake\n```\n````\n\n";

    /// A Markdown session pack as `build_pack` keeps it: the header, an
    /// architecture section, `files` breadth chunks, the most connected
    /// first, then the documentation.
    fn session_map(files: usize) -> String {
        let mut map = String::from(
            "# Code pack: app\n\nRepository: app at commit abc\nTokens: estimated.\n\n## Architecture summary\n\nOne service and its jobs.\n\n## Breadth\n\n",
        );
        for file in 0..files {
            map.push_str(&chunk(file, files - file));
        }
        map.push_str("## Documentation\n\n");
        map.push_str(README);
        map
    }

    fn memory_context(memories: usize) -> String {
        let titles: String = (0..memories)
            .map(|i| format!("- Décision \"{i}\" on the 東京 bucket \\ region (memory m{i})\n"))
            .collect();
        format!(
            "{}\nGraphicle project memory. Reviewed memories are current.\nReviewed memories:\n{titles}",
            crate::agent_instructions::SESSION_ROUTING
        )
    }

    #[test]
    fn a_map_splits_where_a_section_or_chunk_starts() {
        let map = session_map(2);
        let parts = map_parts(&map);
        assert_eq!(parts.concat(), map);
        let starts: Vec<&str> = parts
            .iter()
            .map(|part| part.lines().next().unwrap())
            .collect();
        // Not at the heading inside the README's fence.
        assert_eq!(
            starts,
            [
                "# Code pack: app",
                "## Architecture summary",
                "## Breadth",
                "### src/f0.rs lines 1-30 (whole file)",
                "### src/f1.rs lines 1-30 (whole file)",
                "## Documentation",
                "### README.md lines 1-5"
            ]
        );
    }

    #[test]
    fn a_session_start_that_fits_is_left_whole() {
        let context = memory_context(3);
        let map = session_map(2);
        assert_eq!(
            session_start(Some(&map), &context),
            format!("{}\n\n{context}", map.trim_end())
        );
        assert_eq!(session_start(None, &context), context);
        assert_eq!(session_start(Some(" \n"), &context), context);
    }

    /// Muse Code 1.4.14 stopped the session start of 0.1.10 for printing
    /// more than 16,384 bytes. Whatever the sizes, the output stays within
    /// HOOK_OUTPUT_BYTES and is JSON holding the line on when to use
    /// Graphicle.
    #[test]
    fn the_session_start_stays_within_muse_codes_limit() {
        for files in [0, 1, 5, 20, 60, 400] {
            for memories in [0, 10, 100, 400] {
                let map = session_map(files);
                let context = memory_context(memories);
                let out = session_start(Some(&map), &context);
                let printed = start_output(&out).to_string();
                assert!(
                    printed.len() < HOOK_OUTPUT_BYTES,
                    "{files} files, {memories} memories: {} bytes",
                    printed.len()
                );
                let parsed: Value = serde_json::from_str(&printed).unwrap();
                assert_eq!(parsed["hookSpecificOutput"]["additionalContext"], out);
                assert!(out.contains(crate::agent_instructions::SESSION_ROUTING));
                let whole = printed_bytes(&format!("{}\n\n{context}", map.trim_end()));
                assert_eq!(
                    out.contains("Graphicle left out"),
                    whole > HOOK_OUTPUT_BYTES,
                    "{files} files, {memories} memories"
                );
            }
        }
    }

    /// Too large together: the map loses parts as a pack over its budget
    /// would (its least connected files first, the documentation after
    /// them), each part whole and in the map's order; the memory stays whole,
    /// and a last line says so.
    #[test]
    fn the_map_is_cut_before_the_memory() {
        let context = memory_context(60);
        let out = session_start(Some(&session_map(200)), &context);
        let routing = format!("\n\n{}", crate::agent_instructions::SESSION_ROUTING);
        let (map, rest) = out.split_once(&routing).unwrap();
        let kept = (0..200)
            .filter(|file| map.contains(&format!("### src/f{file}.rs ")))
            .count();
        assert!(kept > 0 && kept < 200, "{map}");
        let mut expected = String::from(
            "# Code pack: app\n\nRepository: app at commit abc\nTokens: estimated.\n\n## Architecture summary\n\nOne service and its jobs.\n\n## Breadth\n\n",
        );
        for file in 0..kept {
            expected.push_str(&chunk(file, 200 - file));
        }
        expected.push_str("## Documentation\n\n");
        expected.push_str(README);
        assert_eq!(map, expected.trim_end());
        assert!(rest.contains(&context[context.find('\n').unwrap()..]));
        assert!(out.ends_with(
            "Graphicle left out part of the repository map to stay within Muse Code's limit on a hook's output (16,384 bytes); the pack tool returns the repository's code.\n"
        ));

        // A part too large for the room goes; smaller ones after it still
        // fill the room.
        let mut map = String::from("# Code pack: app\n\n## Breadth\n\n");
        map.push_str(&chunk(0, 9));
        let body: String = (1..=2000)
            .map(|line| format!("{line}\tlet w = 0;\n"))
            .collect();
        map.push_str(&format!(
            "### src/f1.rs lines 1-2000 (whole file)\nwhy: breadth: centrality 8\n```rust\n{body}```\n\n"
        ));
        for file in 2..6 {
            map.push_str(&chunk(file, 9 - file));
        }
        let out = session_start(Some(&map), &context);
        assert!(printed_bytes(&out) <= HOOK_OUTPUT_BYTES);
        assert!(out.contains("### src/f0.rs ") && out.contains(&chunk(5, 4)));
        assert!(!out.contains("### src/f1.rs "), "{out}");
    }

    /// Memory larger than the limit alone (titles in CJK script take three
    /// bytes a character): the map goes, then the memory's last lines, and a
    /// heading whose lines all went; the line on when to use Graphicle
    /// stays.
    #[test]
    fn memory_too_large_loses_its_last_lines_never_the_routing_line() {
        let routing = crate::agent_instructions::SESSION_ROUTING;
        let titles: String = (0..120)
            .map(|i| format!("- {} (memory m{i})\n", "東京".repeat(40)))
            .collect();
        let turns: String = (0..5)
            .map(|i| format!("- 2026-10-30 東京 {i} (connector:c:{i})\n"))
            .collect();
        let context = format!(
            "{routing}\nGraphicle project memory.\nReviewed memories:\n{titles}Recent captured turns (session_timeline for more):\n{turns}"
        );
        for map in [Some(session_map(20)), None] {
            let out = session_start(map.as_deref(), &context);
            assert!(printed_bytes(&out) <= HOOK_OUTPUT_BYTES);
            assert!(out.starts_with(routing), "{out}");
            assert!(out.contains("(memory m0)") && !out.contains("Recent captured"));
            let kept = out
                .lines()
                .filter(|line| line.contains("(memory m"))
                .count();
            let note = out.lines().last().unwrap();
            let left = 120 - kept + 6;
            let expected = match map {
                Some(_) => format!(
                    "Graphicle left out the repository map and the last {left} lines of project memory to stay within Muse Code's limit on a hook's output (16,384 bytes); the pack tool returns the repository's code, project_memory and session_timeline the rest."
                ),
                None => format!(
                    "Graphicle left out the last {left} lines of project memory to stay within Muse Code's limit on a hook's output (16,384 bytes); project_memory and session_timeline return the rest."
                ),
            };
            assert_eq!(note, expected);
        }
        // One line too large: it goes, and so does its heading.
        let context = format!(
            "{routing}\nGraphicle project memory.\nReviewed memories:\n- {} (memory m0)\n",
            "東京".repeat(3000)
        );
        let out = session_start(None, &context);
        assert_eq!(
            out,
            format!(
                "{routing}\nGraphicle project memory.\nGraphicle left out the last 2 lines of project memory to stay within Muse Code's limit on a hook's output (16,384 bytes); project_memory and session_timeline return the rest.\n"
            )
        );
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
