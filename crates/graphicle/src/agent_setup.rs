//! One-command MCP connection, preserving unrelated client configuration.
//! Every file it changes is backed up once, left alone when nothing would
//! change, and recorded for `graphicle disconnect` ([`crate::agent_files`]).
use crate::platform::CanonicalExt;
use crate::{Error, Result, agent_files, api::ToolSet, codex_config, local, store::Store};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// What `graphicle connect` was asked for.
pub struct Connect<'a> {
    /// `claude`, `claude-code`, `cursor`, `vscode` or `codex`.
    pub client: &'a str,
    /// The client configuration file to edit instead of the client's own.
    pub config_path: Option<&'a Path>,
    /// `--project`: an id, a key or a folder.
    pub project: Option<&'a str>,
    /// `--name`: the entry's name.
    pub name: Option<&'a str>,
    pub replace: bool,
    pub hooks: bool,
    pub work_state: bool,
    /// `--graph`: set the client up for the code graph too, as setup does.
    /// An entry that serves the whole server (or its memory half) lets the
    /// agent propose memory (`--graph` implies `--allow-learning` there);
    /// the essentials stay five tools.
    pub graph: bool,
    /// `--allow-learning`: the entry publishes the memory-proposal tools
    /// (with the essentials, `propose_memory`).
    pub allow_learning: bool,
    /// `--tools`: the tools the entry publishes; `None` is the client's
    /// default ([`default_tools`]).
    pub tools: Option<ToolSet>,
    /// `--guards`: Claude Code gets guard hooks that point searches and
    /// source reads in a synced folder to the server's graph and recall
    /// tools. Off unless asked for.
    pub guards: bool,
    /// The folder `connect` runs in: it decides the project, and the
    /// project-scope clients' files are written there.
    pub cwd: &'a Path,
}

/// The tools a client's entry publishes unless `--tools` says otherwise:
/// Claude Code, which shows every request the names of the tools and the
/// server's instructions and loads a tool only when the agent searches for
/// it, gets the five essential tools ([`ToolSet::Essentials`]); the other
/// clients get the whole server, as in 0.1.0.
pub fn default_tools(client: &str) -> ToolSet {
    if client == "claude-code" {
        ToolSet::Essentials
    } else {
        ToolSet::All
    }
}

/// How `connect` treats an entry of the same name that differs.
/// `graphicle connect <client>` refuses it without `--replace`;
/// `setup` and `connect` without a client bring a Graphicle entry
/// ([`agent_files::is_graphicle_entry`]) and Graphicle's own hooks up to
/// date, after an upgrade moved the program for example, and still refuse
/// anything else.
#[derive(Clone, Copy, Debug, Default)]
pub struct Policy {
    pub refresh_ours: bool,
    /// Setup and `connect` without a client: Claude Code's guard hooks of
    /// this store, which 0.1.0's setup installed by default, are removed
    /// when `--guards` is not asked for, so a second run leaves what the
    /// first one would have.
    pub drop_guards: bool,
}

/// The connection `setup` and `connect` without a client make for `client`
/// in `folder`, built in this one place. Capture hooks go to the clients
/// that have them (Claude Code, Codex) when `capture` is set. The entry
/// runs the one MCP server, set up for the code graph as `connect --graph`
/// does: `tools` (else the client's default, [`default_tools`]), where the
/// whole server lets the agent propose memory drafts, which a member
/// reviews; Claude Code gets the guard hooks with `guards`.
pub fn default_connection<'a>(
    client: &'a str,
    folder: &'a Path,
    capture: bool,
    guards: bool,
    tools: Option<ToolSet>,
) -> Connect<'a> {
    Connect {
        client,
        config_path: None,
        project: None,
        name: None,
        replace: false,
        hooks: capture && has_hooks(client),
        work_state: false,
        graph: true,
        allow_learning: false,
        tools,
        guards,
        cwd: folder,
    }
}

/// An entry's arguments without `--tools <set>` and `--allow-learning`.
fn without_tool_set(args: &Value) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    let mut set = false;
    for arg in args.as_array()? {
        let arg = arg.as_str()?;
        if std::mem::take(&mut set) {
            continue;
        }
        match arg {
            "--tools" => set = true,
            "--allow-learning" => {}
            _ => out.push(arg),
        }
    }
    Some(out)
}

/// Whether the entry `old` is the entry `asked` but for its tool set
/// (`--tools`, `--allow-learning`): the same command, server, store and
/// project, and no other field.
fn same_but_tool_set(old: &Value, asked: &Value) -> bool {
    let fields = |entry: &Value| {
        entry.as_object().map(|object| {
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "args")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>()
        })
    };
    fields(old).is_some()
        && fields(old) == fields(asked)
        && without_tool_set(&old["args"]).is_some()
        && without_tool_set(&old["args"]) == without_tool_set(&asked["args"])
}

/// The tool set an entry's arguments name: `--tools <set>`, else the whole
/// server.
fn entry_tools(entry: &Value) -> ToolSet {
    entry["args"]
        .as_array()
        .and_then(|args| {
            let at = args.iter().position(|arg| arg == "--tools")?;
            ToolSet::parse(args.get(at + 1)?.as_str()?).ok()
        })
        .unwrap_or(ToolSet::All)
}

/// Whether `client` takes session-capture hooks.
pub fn has_hooks(client: &str) -> bool {
    matches!(client, "claude-code" | "codex")
}

/// Clients whose MCP file belongs to one folder (Claude Code's `.mcp.json`,
/// VS Code's `.vscode/mcp.json`); the others hold one file per user.
fn project_scope(client: &str) -> bool {
    matches!(client, "claude-code" | "vscode")
}

/// A configuration entry name for a project: `graphicle-` and its name in
/// lower case, every run of other characters one `-`, at most 40 of them.
pub fn entry_name(project: &str) -> String {
    let mut slug = String::new();
    for c in project.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    format!(
        "graphicle-{}",
        if slug.is_empty() { "project" } else { slug }
    )
}

/// `graphicle` when the first `graphicle` on `PATH` is this program (whose
/// canonical path is `executable`), else `program`.
fn command_name(program: &Path, executable: &Path) -> PathBuf {
    let name = format!("graphicle{}", std::env::consts::EXE_SUFFIX);
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .filter(|folder| folder.is_absolute())
            .map(|folder| folder.join(&name))
            .find(|candidate| candidate.is_file())
    });
    match on_path.and_then(|found| found.canonical().ok()) {
        Some(found) if found == executable => PathBuf::from("graphicle"),
        _ => program.to_path_buf(),
    }
}

/// This program's path for the files that outlive this run (client
/// entries, capture hooks, Git hooks): the link on `PATH` that leads to it
/// when there is one, such as Homebrew's `bin/graphicle`, which an upgrade
/// keeps while it removes the versioned folder the link pointed into; else
/// its canonical path.
pub fn stable_program() -> Result<PathBuf> {
    let executable = std::env::current_exe()?.canonical()?;
    if let (Some(name), Some(path)) = (executable.file_name(), std::env::var_os("PATH")) {
        for folder in std::env::split_paths(&path).filter(|folder| folder.is_absolute()) {
            let candidate = folder.join(name);
            if candidate.canonical().ok().as_ref() == Some(&executable) {
                return Ok(candidate);
            }
        }
    }
    Ok(executable)
}

/// The name of `project`'s entry in a user-level client file, where
/// `existing` gives the entry a name holds: `base` (`graphicle-<folder
/// name>`) unless an entry of that name is not Graphicle's, or serves
/// another data folder or another project this store still has (two
/// repositories both named `api`); then `base-<short id>`, else
/// `base-<id>`.
fn entry_for_project(
    base: &str,
    project: &str,
    directory: &Path,
    existing: &dyn Fn(&str) -> Option<Value>,
    live: &dyn Fn(&str) -> bool,
) -> String {
    let slug: String = project
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let short: String = slug.chars().take(8).collect();
    let long: String = slug.chars().take(40).collect();
    let candidates = [
        base.to_string(),
        format!("{base}-{short}"),
        format!("{base}-{long}"),
    ];
    let usable = |name: &str| match existing(name) {
        None => true,
        Some(entry) => {
            agent_files::is_graphicle_entry(&entry)
                && agent_files::entry_uses_store(&entry, directory)
                && agent_files::entry_project(&entry)
                    .is_some_and(|named| named == project || !live(&named))
        }
    };
    candidates
        .iter()
        .find(|name| usable(name))
        .unwrap_or(&candidates[2])
        .clone()
}

/// Write one MCP entry for `options.client`:
/// - the project is `--project`, else the folder's project, else the saved
///   default;
/// - a project-scope client (Claude Code, VS Code) gets the entry
///   `graphicle`, written in this folder, which finds the project from the
///   folder the client starts it in (`--project .`; VS Code passes its
///   workspace folder); it names the project's id only when that is not
///   this folder's;
/// - a user-level client (Claude Desktop, Cursor, Codex) gets an entry named
///   after the project ([`entry_name`], or `--name`) that names it;
/// - `--data-dir` is passed only when the store is not the platform's
///   default folder; project-scope entries run `graphicle` from `PATH` when
///   that is this program.
pub fn connect(store: &Store, options: &Connect<'_>) -> Result<Value> {
    connect_with(store, options, Policy::default())
}

/// [`connect`] under `policy`. The reply says `"unchanged": true` when the
/// entry (and its hooks) were there as they are, and `"updated": true`
/// when an existing entry of that name was replaced.
pub fn connect_with(store: &Store, options: &Connect<'_>, policy: Policy) -> Result<Value> {
    let client = options.client;
    if !["claude", "claude-code", "cursor", "vscode", "codex"].contains(&client) {
        return Err(Error::bad(
            "Choose claude, claude-code, cursor, vscode or codex",
        ));
    }
    if options.hooks && !["claude-code", "codex"].contains(&client) {
        return Err(Error::bad(
            "Session-capture hooks are available for claude-code and codex",
        ));
    }
    let session = local::session(store, false)?;
    let project = store.resolve_project(
        &session.workspace,
        &session.capability,
        options.project,
        Some(options.cwd),
        session.project.as_deref(),
    )?;
    let executable = std::env::current_exe()?.canonical()?;
    // What the entry and hooks run: a path that survives an upgrade.
    let program = stable_program()?;
    let directory = store.directory.canonical()?;
    let mut arguments: Vec<String> = Vec::new();
    if local::default_data_dir().canonical().ok().as_deref() != Some(directory.as_path()) {
        arguments.extend([
            "--data-dir".into(),
            directory.to_string_lossy().into_owned(),
        ]);
    }
    arguments.push("mcp".into());
    // A project-scope entry in this folder's own file follows the folder
    // when the folder decided: the project of the folder the client starts
    // the server in (`.`), or of VS Code's workspace folder. It never falls
    // back to the saved default. A file elsewhere (`--config-path`) names
    // the project.
    let follows_folder =
        project_scope(client) && project.source == "folder" && options.config_path.is_none();
    let named = if !follows_folder {
        project.id.clone()
    } else if client == "vscode" {
        "${workspaceFolder}".into()
    } else {
        ".".into()
    };
    arguments.extend(["--project".into(), named]);
    // The tools the entry publishes: `--tools`, else the client's default.
    // The whole server's entry names no set, as 0.1.0's entries did.
    let tools = options.tools.unwrap_or_else(|| default_tools(client));
    if tools != ToolSet::All {
        arguments.extend(["--tools".into(), tools.name().into()]);
    }
    // `--graph` lets the agent propose memory where the set has room for the
    // proposal tools (the whole server, the memory half); the essentials
    // stay five tools unless `--allow-learning` asks for `propose_memory`.
    let learning = options.allow_learning
        || (options.graph && matches!(tools, ToolSet::All | ToolSet::Memory));
    // The same entry with `--allow-learning`, which setup's entries of the
    // whole server carry: a Graphicle entry of this store that differs from
    // the one asked for only by it keeps it (below), so `connect <client>`
    // after setup changes nothing.
    let mut learned_arguments = arguments.clone();
    learned_arguments.push("--allow-learning".into());
    if learning {
        arguments.push("--allow-learning".into());
    }
    if options.work_state {
        arguments.push("--work-state".into());
        learned_arguments.push("--work-state".into());
    }
    let command = if project_scope(client) {
        command_name(&program, &executable)
    } else {
        program.clone()
    };
    let live = |id: &str| {
        store
            .find_project(
                &session.workspace,
                &session.capability,
                Some(id),
                None,
                None,
            )
            .is_ok_and(|found| found.is_some())
    };
    let base = match options.name {
        Some(name) => {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(Error::bad(
                    "Use an entry name of 1-64 letters, digits, - or _",
                ));
            }
            name.to_string()
        }
        None if project_scope(client) => "graphicle".to_string(),
        None => entry_name(&project.name),
    };
    // A user-level entry named for the project takes another name when
    // that one serves another project (`entry_for_project`).
    let named_for_project = options.name.is_none() && !project_scope(client);
    let access = |learning: bool| {
        let mut access = String::from(match tools {
            ToolSet::Essentials => {
                "five read-only tools: query_graph, get_node and get_neighbors on the code graph, recall_project and read_project_source (--tools all publishes every tool)"
            }
            ToolSet::Graph => "read-only code-graph tools",
            ToolSet::Memory => "read-only source and reviewed memory tools",
            ToolSet::All => "read-only source, code-graph and reviewed memory tools",
        });
        if learning && tools != ToolSet::Graph {
            access.push_str(", plus memory drafts a member reviews");
        }
        if options.work_state {
            access.push_str(", plus the agent's own work-state lists");
        }
        access
    };
    let chosen = json!({"id":project.id,"name":project.name,"source":project.source,
        "resolves":if follows_folder {"from the folder the client starts in"} else {"named in the entry"}});
    // Where this connection's changes are recorded, and the folder they
    // belong to.
    let folder = options.cwd.canonical()?;
    let replace_hooks = options.replace || policy.refresh_ours;
    if client == "codex" {
        // `--replace` applies only to Graphicle's own hooks here.
        if options.config_path.is_some() || (options.replace && !options.hooks) {
            return Err(Error::bad(
                "Codex manages its own server configuration; remove an existing entry through codex mcp before reconnecting",
            ));
        }
        let env = crate::agent_detect::Environment::current();
        let config = env.codex_config();
        let text = config
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        let command_text = command.to_string_lossy().into_owned();
        let tables = codex_config::tables(&text);
        let entry = if named_for_project {
            entry_for_project(
                &base,
                &project.id,
                &directory,
                &|name| {
                    tables
                        .iter()
                        .find(|table| table.name == name)
                        .map(codex_config::Table::entry)
                },
                &live,
            )
        } else {
            base.clone()
        };
        let current = tables.into_iter().find(|table| table.name == entry);
        let unchanged = current.as_ref().is_some_and(|table| {
            table.command.as_deref() == Some(command_text.as_str())
                && table.args.as_deref() == Some(&arguments[..])
        });
        // Setup's entry, with `--allow-learning`, kept as it is.
        let learning_kept = !unchanged
            && !learning
            && current.as_ref().is_some_and(|table| {
                table.command.as_deref() == Some(command_text.as_str())
                    && table.args.as_deref() == Some(&learned_arguments[..])
            });
        let unchanged = unchanged || learning_kept;
        if current
            .as_ref()
            .is_some_and(|table| !unchanged && !agent_files::is_graphicle_entry(&table.entry()))
        {
            return Err(Error::conflict(format!(
                "Codex already has an MCP server named {entry} that is not Graphicle's; remove it with codex mcp remove {entry}, or choose another --name"
            )));
        }
        // Hook conflicts are found before anything is written.
        let hook_plan = if options.hooks {
            Some(plan_hooks(
                client,
                &program,
                &directory,
                replace_hooks,
                options.cwd,
                &hook_specs(client, true, false),
            )?)
        } else {
            None
        };
        let mut result = json!({"connected":true,"client":client,"entry":entry,"project":chosen,"access":access(learning || learning_kept),"tools":tools.name()});
        let existed = config.as_deref().is_some_and(Path::exists);
        if unchanged {
            result["unchanged"] = json!(true);
        } else {
            if let Some(config) = &config {
                agent_files::backup_in(&directory, config)?;
            }
            match crate::platform::command("codex")
                .args(["mcp", "add", &entry, "--"])
                .arg(&command)
                .args(&arguments)
                .output()
            {
                Ok(output) if output.status.success() => {}
                Ok(_) => {
                    return Err(Error::bad(format!(
                        "The client declined MCP installation; inspect the existing {entry} entry with codex mcp list"
                    )));
                }
                // Without the Codex CLI (the IDE extension keeps its own
                // copy), the entry is written into Codex's folder directly.
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && env.codex_dir().is_some_and(|folder| folder.is_dir()) =>
                {
                    let Some(config) = &config else {
                        return Err(Error::bad("Home directory unavailable"));
                    };
                    write_codex_table(config, &text, &entry, &command_text, &arguments)?;
                }
                Err(_) => {
                    return Err(Error::bad(
                        "Install the Codex CLI, or use the MCP configuration shown in Connections",
                    ));
                }
            }
            if current.is_some() {
                result["updated"] = json!(true);
            }
        }
        if let Some(config) = &config {
            agent_files::record(
                &directory,
                agent_files::Change {
                    path: std::path::absolute(config)?,
                    kind: "codex".into(),
                    client: Some(client.into()),
                    folder: Some(folder.clone()),
                    project: Some(project.id.clone()),
                    entry: Some(entry.clone()),
                    created: !existed,
                    ..Default::default()
                },
            )?;
            result["configuration"] = json!(config);
        }
        if let Some(plan) = hook_plan {
            result["hooks"] =
                write_hooks(plan, &directory, &folder, &project.id)?["capture"].take();
        }
        if options.guards {
            result["graph"] = json!({"guard_hooks":"not available for codex: its hooks cannot add context before a tool call"});
        }
        return Ok(result);
    }
    let user =
        crate::platform::home_dir().ok_or_else(|| Error::bad("Home directory unavailable"))?;
    let destination = if let Some(path) = options.config_path {
        path.to_path_buf()
    } else {
        match client {
            "claude" => {
                #[cfg(target_os = "macos")]
                {
                    user.join("Library/Application Support/Claude/claude_desktop_config.json")
                }
                #[cfg(windows)]
                {
                    crate::agent_detect::Environment::current()
                        .claude_desktop_config()
                        .unwrap_or_else(|| {
                            user.join(r"AppData\Roaming\Claude\claude_desktop_config.json")
                        })
                }
                #[cfg(all(not(target_os = "macos"), not(windows)))]
                {
                    user.join(".config/Claude/claude_desktop_config.json")
                }
            }
            "cursor" => user.join(".cursor/mcp.json"),
            "vscode" => options.cwd.join(".vscode/mcp.json"),
            _ => options.cwd.join(".mcp.json"),
        }
    };
    // The reply names the file as given; the record, absolutely.
    let recorded = std::path::absolute(&destination)?;
    let existed = destination.exists();
    let group = if client == "vscode" {
        "servers"
    } else {
        "mcpServers"
    };
    let mut server = json!({"command":command,"args":arguments});
    let mut learned = json!({"command":command,"args":learned_arguments});
    if client == "vscode" {
        server["type"] = json!("stdio");
        learned["type"] = json!("stdio");
    }
    // The entry to paste by hand when the file cannot be edited.
    let by_hand = |name: &str| -> String {
        format!(
            "add this to the \"{group}\" object of {} yourself (docs/mcp.md#other-clients): \"{name}\": {}",
            destination.display(),
            server
        )
    };
    let (mut document, original) = match read_config(&destination, "MCP") {
        Ok(read) => read,
        Err(error) => {
            return Err(Error::bad(format!(
                "{}; {}",
                error.message.trim_end_matches('.'),
                by_hand(base.as_str())
            )));
        }
    };
    if document.get(group).is_none() {
        document[group] = json!({});
    }
    if !document[group].is_object() {
        return Err(Error::bad(format!(
            "The \"{group}\" value in {} is not an object; {}",
            destination.display(),
            by_hand(base.as_str())
        )));
    }
    let entry = if named_for_project {
        entry_for_project(
            &base,
            &project.id,
            &directory,
            &|name| document[group].get(name).cloned(),
            &live,
        )
    } else {
        base.clone()
    };
    let old = document[group].get(&entry).cloned();
    // Setup's entry, with `--allow-learning`, kept as it is.
    let learning_kept = !learning && old.as_ref() == Some(&learned);
    // `connect <client>` without `--tools` or `--allow-learning` keeps an
    // entry of this store that differs only by its tool set: a 0.1.0
    // setup's, or `setup --tools all`'s, which run the whole server.
    let set_kept = !learning_kept
        && !policy.refresh_ours
        && options.tools.is_none()
        && !options.allow_learning
        && old
            .as_ref()
            .is_some_and(|old| same_but_tool_set(old, &server));
    let tools = if set_kept {
        old.as_ref().map_or(tools, entry_tools)
    } else {
        tools
    };
    let learning_kept = learning_kept
        || (set_kept
            && old.as_ref().is_some_and(|old| {
                old["args"]
                    .as_array()
                    .is_some_and(|args| args.iter().any(|arg| arg == "--allow-learning"))
            }));
    let server = match &old {
        Some(old) if set_kept => old.clone(),
        _ if learning_kept => learned.clone(),
        _ => server.clone(),
    };
    let unchanged = old.as_ref() == Some(&server);
    let replacing = old.is_some() && !unchanged;
    if replacing
        && !options.replace
        && !(policy.refresh_ours && old.as_ref().is_some_and(agent_files::is_graphicle_entry))
    {
        return Err(Error::conflict(format!(
            "A Graphicle connection named {entry} already exists; inspect it before using --replace, or choose another --name"
        )));
    }
    // Hook conflicts are found before anything is written. Claude Code's
    // capture and guard hooks share one file and one plan.
    let guards = options.guards && client == "claude-code";
    let drop_guards = policy.drop_guards
        && client == "claude-code"
        && !guards
        && read_config(&hook_file(client, options.cwd)?, "hook")
            .is_ok_and(|(document, _)| holds_guard_hooks(&document, client, &directory));
    let hook_plan = if options.hooks || guards || drop_guards {
        let mut plan = plan_hooks(
            client,
            &program,
            &directory,
            replace_hooks,
            options.cwd,
            &hook_specs(client, options.hooks, guards),
        )?;
        if drop_guards {
            plan.drop_guards(&directory);
        }
        Some(plan)
    } else {
        None
    };
    let earlier = entry != "graphicle"
        && document[group]
            .get("graphicle")
            .is_some_and(|earlier| agent_files::entry_uses_store(earlier, &directory));
    let mut result = json!({"connected":true,"client":client,"configuration":destination,"entry":entry,"project":chosen,"access":access(learning || learning_kept),"tools":tools.name(),"next":"Restart or reload your client to enable Graphicle"});
    if unchanged {
        result["unchanged"] = json!(true);
    } else {
        // Only Graphicle's entry changes in the file's text: its other
        // entries, formatting, comments and final newline stay.
        let edited = original
            .as_deref()
            .and_then(|text| crate::json_edit::set(text, group, &entry, &server));
        document[group][&entry] = server.clone();
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        agent_files::backup_in(&directory, &destination)?;
        agent_files::write_json(&destination, original.as_deref(), edited, &document)
            .map_err(|error| Error::bad(format!("{}; {}", error.message, by_hand(&entry))))?;
        if replacing {
            result["updated"] = json!(true);
        }
    }
    agent_files::record(
        &directory,
        agent_files::Change {
            path: recorded,
            kind: "mcp".into(),
            client: Some(client.into()),
            folder: Some(folder.clone()),
            project: Some(project.id.clone()),
            entry: Some(entry.clone()),
            group: Some(group.into()),
            created: !existed,
            ..Default::default()
        },
    )?;
    if earlier {
        result["note"] = json!(
            "An entry named graphicle from an earlier connect is still there; remove it if it serves the same project"
        );
    }
    if let Some(plan) = hook_plan {
        let mut written = write_hooks(plan, &directory, &folder, &project.id)?;
        if options.hooks {
            result["hooks"] = written["capture"].take();
        }
        if guards || drop_guards {
            result["graph"] = json!({"guard_hooks":written["guard"].take()});
        }
    } else if options.guards {
        result["graph"] = json!({"guard_hooks":format!("not available for {client}: only Claude Code's hooks can add context before a tool call")});
    }
    Ok(result)
}

/// Write Graphicle's table for `entry` into Codex's `config.toml` (`text`
/// is its content), in place of an earlier one of that name.
fn write_codex_table(
    config: &Path,
    text: &str,
    entry: &str,
    command: &str,
    arguments: &[String],
) -> Result<()> {
    if config.is_symlink() {
        return Err(Error::bad("Codex configuration cannot be a symlink"));
    }
    if codex_config::inline_servers(text) {
        return Err(Error::bad(
            "Codex's config.toml defines mcp_servers in a form Graphicle does not edit; install the Codex CLI and connect again",
        ));
    }
    let base = codex_config::remove(text, entry)
        .map(|(text, _, _)| text)
        .unwrap_or_else(|| text.to_string());
    let updated = codex_config::append(&base, entry, command, arguments);
    agent_files::write(config, updated.as_bytes(), 0o600)
}

/// A JSON object from a client configuration file, with the file's text
/// (`{}` and no text when the file is absent). Comments and trailing
/// commas (VS Code's JSONC) are read.
fn read_config(destination: &Path, what: &str) -> Result<(Value, Option<String>)> {
    if destination.is_symlink() {
        return Err(Error::bad(format!(
            "{what} configuration cannot be a symlink"
        )));
    }
    if !destination.exists() {
        return Ok((json!({}), None));
    }
    let bytes = std::fs::read(destination)?;
    if bytes.len() > 1024 * 1024 {
        return Err(Error::bad("Client configuration exceeds 1 MiB"));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        Error::bad(format!(
            "{} is not UTF-8 text, so Graphicle left it alone",
            destination.display()
        ))
    })?;
    let document = crate::json_edit::parse(&text).ok_or_else(|| {
        Error::bad(format!(
            "{} is not valid JSON ({}), so Graphicle left it alone",
            destination.display(),
            crate::json_edit::error_place(&text)
        ))
    })?;
    if !document.is_object() {
        return Err(Error::bad(format!(
            "{} does not hold a JSON object, so Graphicle left it alone",
            destination.display()
        )));
    }
    Ok((document, Some(text)))
}

/// POSIX shell quoting: hook commands run through `sh -c`, and the default
/// macOS data folder ("Application Support") contains a space.
#[cfg(not(windows))]
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Windows: a path double-quoted with `/` separators, which cmd and Git
/// Bash read alike (a Windows path cannot hold `"`). Codex's hooks use it:
/// its documentation names no shell, and on Windows Codex hands the command
/// to the system's command interpreter (cmd).
#[cfg(windows)]
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "/"))
}

/// The start of a hook command, `<program> --data-dir <store> hook <client> `,
/// quoted for the shell `client` runs its hooks with.
#[cfg(not(windows))]
fn hook_prefix(client: &str, program: &Path, directory: &Path) -> String {
    format!(
        "{} --data-dir {} hook {client} ",
        quote(&program.to_string_lossy()),
        quote(&directory.to_string_lossy())
    )
}

/// Windows: Claude Code runs a hook command through Git Bash when Git for
/// Windows is installed and through PowerShell otherwise (its hooks
/// reference, "shell form"). The program is written as [`claude_program`]
/// spells it and the data folder single-quoted, which both shells take
/// literally. Codex's hooks keep [`quote`]'s form.
#[cfg(windows)]
fn hook_prefix(client: &str, program: &Path, directory: &Path) -> String {
    let (spelled, directory) = (program.to_string_lossy(), directory.to_string_lossy());
    if client == "claude-code" {
        let on_path = || {
            std::env::current_exe()
                .ok()
                .and_then(|executable| executable.canonical().ok())
                .is_some_and(|executable| {
                    command_name(program, &executable).as_os_str() == "graphicle"
                })
        };
        format!(
            "{} --data-dir {} hook {client} ",
            claude_program(&spelled, on_path),
            single_quoted(&directory)
        )
    } else {
        format!(
            "{} --data-dir {} hook {client} ",
            quote(&spelled),
            quote(&directory)
        )
    }
}

/// The program of a Claude Code hook on Windows: the path bare when it
/// needs no quoting in either shell ([`bare_or_quoted`]); else the name
/// `graphicle` when that name on `PATH` is this program (`on_path`), since
/// PowerShell takes a leading quoted string as a value, not a command (a
/// profile folder with a space); else the path double-quoted, the form Git
/// Bash runs.
#[cfg(any(windows, test))]
fn claude_program(program: &str, on_path: impl FnOnce() -> bool) -> String {
    let spelled = bare_or_quoted(program);
    if spelled.starts_with('"') && on_path() {
        "graphicle".to_string()
    } else {
        spelled
    }
}

/// `value` with `/` separators, bare when every character means itself to
/// Git Bash and PowerShell alike (ASCII letters, digits and `/:._+-`, after
/// a drive letter), else double-quoted ([`quote`]).
#[cfg(any(windows, test))]
fn bare_or_quoted(value: &str) -> String {
    let slashed = value.replace('\\', "/");
    let drive = slashed.len() > 3
        && slashed.as_bytes()[0].is_ascii_alphabetic()
        && slashed.get(1..3) == Some(":/");
    let plain = slashed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/:._+-".contains(&b));
    if drive && plain {
        slashed
    } else {
        format!("\"{slashed}\"")
    }
}

/// `value` with `/` separators in single quotes, which Git Bash and
/// PowerShell both take literally; double-quoted ([`quote`]) when it holds
/// a quote character either shell ends a single-quoted string at
/// (PowerShell also ends one at a typographic single quote).
#[cfg(any(windows, test))]
fn single_quoted(value: &str) -> String {
    let slashed = value.replace('\\', "/");
    if slashed.contains(['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}']) {
        format!("\"{slashed}\"")
    } else {
        format!("'{slashed}'")
    }
}

/// One Graphicle hook a client runs: `graphicle --data-dir <store> hook
/// <client> <argument>` on `event`, with its timeout in seconds and the
/// tools it matches (Claude Code's `matcher`).
#[derive(Clone, Copy, Debug)]
struct HookSpec {
    event: &'static str,
    argument: &'static str,
    timeout: u64,
    matcher: Option<&'static str>,
    /// A guard hook of `--guards`, else a capture hook
    /// of `connect --hooks` (or the prompt-context hook, `prompt`).
    guard: bool,
    /// Claude Code's prompt-context hook of `setup --prompt-context`.
    prompt: bool,
}

const fn capture(event: &'static str, argument: &'static str, timeout: u64) -> HookSpec {
    HookSpec {
        event,
        argument,
        timeout,
        matcher: None,
        guard: false,
        prompt: false,
    }
}

/// Claude Code's capture hooks, in its personal project settings next to the
/// project's `.mcp.json`. Session start waits at most 2.5 s for context; its
/// larger timeout leaves room for the operating system's first-launch check
/// of an updated executable. Claude Code skips it on resume, whose
/// conversation still holds the context the session's start added.
const CLAUDE_CODE_CAPTURE: [HookSpec; 4] = [
    HookSpec {
        matcher: Some("startup|clear|compact"),
        ..capture("SessionStart", "session-start", 10)
    },
    capture("Stop", "stop", 10),
    capture("PreCompact", "pre-compact", 10),
    capture("SessionEnd", "session-end", 10),
];

/// Codex's capture hooks, user-level: Codex loads them regardless of project
/// trust. SessionEnd hooks may run for at most 3 seconds.
const CODEX_CAPTURE: [HookSpec; 3] = [
    capture("SessionStart", "session-start", 10),
    capture("Stop", "stop", 10),
    capture("SessionEnd", "session-end", 3),
];

/// Claude Code's guard hooks of `setup --guards` and `connect --guards`: before
/// a search (Bash, Grep) or a source read (Read, Glob), `graphicle hook
/// claude-code guard-search|guard-read` points the agent to the server's
/// graph and recall tools when the folder is a Graphicle project with a code
/// graph, and otherwise answers as the graph engine's own guard does.
const CLAUDE_CODE_GUARDS: [HookSpec; 2] = [
    HookSpec {
        event: "PreToolUse",
        argument: "guard-search",
        timeout: 10,
        matcher: Some("Bash|Grep"),
        guard: true,
        prompt: false,
    },
    HookSpec {
        event: "PreToolUse",
        argument: "guard-read",
        timeout: 10,
        matcher: Some("Read|Glob"),
        guard: true,
        prompt: false,
    },
];

/// Claude Code's prompt-context hook of `setup --prompt-context` (off by
/// default): on each prompt, `graphicle hook claude-code prompt-context`
/// adds `graphicle context`'s packet for a code question
/// ([`crate::prompt_hook`]). Claude Code's timeout is a backstop: the hook
/// gives up by itself after its budget ([`crate::prompt_hook::budget`], at
/// most [`crate::prompt_hook::MAX_SECONDS`]).
const CLAUDE_CODE_PROMPT_CONTEXT: HookSpec = HookSpec {
    event: "UserPromptSubmit",
    argument: "prompt-context",
    timeout: crate::prompt_hook::HOOK_TIMEOUT,
    matcher: None,
    guard: false,
    prompt: true,
};

/// The hooks `connect` installs for `client`: the capture hooks with
/// `capture`, and with `guards` the guard hooks (Claude Code only).
fn hook_specs(client: &str, capture: bool, guards: bool) -> Vec<HookSpec> {
    let mut specs = Vec::new();
    if capture {
        specs.extend(match client {
            "claude-code" => CLAUDE_CODE_CAPTURE.to_vec(),
            "codex" => CODEX_CAPTURE.to_vec(),
            _ => Vec::new(),
        });
    }
    if guards && client == "claude-code" {
        specs.extend(CLAUDE_CODE_GUARDS);
    }
    specs
}

/// Where a client reads Graphicle's hooks: Claude Code's personal project
/// settings, Codex's user-level hooks file.
fn hook_file(client: &str, cwd: &Path) -> Result<PathBuf> {
    match client {
        "claude-code" => Ok(cwd.join(".claude/settings.local.json")),
        "codex" => {
            let home = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| crate::platform::home_dir().map(|h| h.join(".codex")))
                .ok_or_else(|| Error::bad("Home directory unavailable"))?;
            Ok(home.join("hooks.json"))
        }
        _ => Err(Error::bad(
            "Session-capture hooks are available for claude-code and codex",
        )),
    }
}

/// A merged hooks file, ready to write.
struct HookPlan {
    client: String,
    destination: PathBuf,
    document: Value,
    /// The file's text before.
    original: Option<String>,
    /// `(spec, installed)` for each hook of the plan: installed now, or
    /// already there unchanged.
    outcomes: Vec<(HookSpec, bool)>,
    /// The guard hooks of this store the plan takes out ([`HookPlan::drop_guards`]).
    removed: Vec<&'static str>,
}

impl HookPlan {
    /// Take Claude Code's guard hooks that open the data folder `directory`
    /// out of the plan's file, with the groups and events that held only
    /// them; every other hook stays.
    fn drop_guards(&mut self, directory: &Path) {
        let client = self.client.clone();
        if remove_matching(&mut self.document, directory, &|handler| {
            is_guard_hook(handler, &client)
        }) {
            self.removed = GUARD_ARGUMENTS.to_vec();
        }
    }
}

/// Merge Graphicle's hooks `specs` into the client's hooks file, keeping
/// every other hook. An existing Graphicle hook with a different command is
/// a conflict unless `replace`.
fn plan_hooks(
    client: &str,
    program: &Path,
    directory: &Path,
    replace: bool,
    cwd: &Path,
    specs: &[HookSpec],
) -> Result<HookPlan> {
    let destination = hook_file(client, cwd)?;
    let (mut document, original) = read_config(&destination, "hook")?;
    if document.get("hooks").is_none() {
        document["hooks"] = json!({});
    }
    let Some(table) = document["hooks"].as_object_mut() else {
        return Err(Error::bad("Existing hooks configuration is not an object"));
    };
    let prefix = hook_prefix(client, program, directory);
    let mut outcomes = Vec::new();
    for spec in specs {
        let mut handler = json!({"type":"command","command":format!("{prefix}{}", spec.argument),"timeout":spec.timeout});
        if client == "codex" && spec.event == "SessionStart" {
            handler["statusMessage"] = json!("Loading Graphicle memory");
        }
        let groups = table
            .entry(spec.event.to_string())
            .or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else {
            return Err(Error::bad(format!(
                "Existing {} hooks are not a list",
                spec.event
            )));
        };
        let ours = format!(" hook {client} {}", spec.argument);
        let mut found = false;
        for group in groups.iter_mut() {
            for existing in group["hooks"].as_array_mut().into_iter().flatten() {
                // This store's hook for the event; another data folder's
                // hooks stay as they are.
                if !existing["command"]
                    .as_str()
                    .is_some_and(|c| c.ends_with(&ours) && hook_uses_store(c, directory))
                {
                    continue;
                }
                found = true;
                if *existing == handler {
                    outcomes.push((*spec, false));
                } else if replace {
                    *existing = handler.clone();
                    outcomes.push((*spec, true));
                } else {
                    return Err(Error::conflict(format!(
                        "A Graphicle {} hook already exists in {}; inspect it before using --replace",
                        spec.event,
                        destination.display()
                    )));
                }
            }
        }
        if !found {
            let mut group = json!({"hooks":[handler]});
            if let Some(matcher) = spec.matcher {
                group["matcher"] = json!(matcher);
            }
            groups.push(group);
            outcomes.push((*spec, true));
        }
    }
    Ok(HookPlan {
        client: client.to_string(),
        destination,
        document,
        original,
        outcomes,
        removed: Vec::new(),
    })
}

/// Whether a capture hook's command opens the data folder `directory`.
pub fn hook_uses_store(command: &str, directory: &Path) -> bool {
    let words = crate::terminal::words(command);
    agent_files::same_store(words.iter().map(String::as_str), directory)
}

/// Bring Graphicle's capture hooks for `client` up to date (`doctor --fix`
/// after an upgrade moved the program, or when an event's hook is
/// missing): every event's hook rewritten with this program, other hooks
/// kept. `cwd` is the project folder (Claude Code's hooks live there).
pub fn refresh_hooks(store: &Store, client: &str, cwd: &Path) -> Result<Value> {
    let session = local::session(store, false)?;
    let project = store.resolve_project(
        &session.workspace,
        &session.capability,
        None,
        Some(cwd),
        session.project.as_deref(),
    )?;
    let directory = store.directory.canonical()?;
    // The hooks the file has of this store: the capture hooks, and Claude
    // Code's guard hooks (`--guards`); the capture hooks when
    // it has neither.
    let (capture, guards) = match read_config(&hook_file(client, cwd)?, "hook") {
        Ok((document, _)) => (
            holds_capture_hooks(&document, client, &directory),
            client == "claude-code" && holds_guard_hooks(&document, client, &directory),
        ),
        Err(_) => (true, false),
    };
    let capture = capture || !guards;
    // The prompt-context hook of `setup --prompt-context`, when there.
    let prompt = client == "claude-code"
        && read_config(&hook_file(client, cwd)?, "hook")
            .is_ok_and(|(document, _)| holds_prompt_hooks(&document, client, &directory));
    let mut specs = hook_specs(client, capture, guards);
    if prompt {
        specs.push(CLAUDE_CODE_PROMPT_CONTEXT);
    }
    let plan = plan_hooks(client, &stable_program()?, &directory, true, cwd, &specs)?;
    let mut written = write_hooks(plan, &directory, &cwd.canonical()?, &project.id)?;
    let mut reply = written["capture"].take();
    if guards {
        reply["guard"] = written["guard"].take();
    }
    if prompt {
        reply["prompt_context"] = written["prompt_context"].take();
    }
    Ok(reply)
}

/// Install (or bring up to date) Claude Code's prompt-context hook
/// (`setup --prompt-context`, [`crate::prompt_hook`]) in the personal
/// settings of the project folder `cwd`, beside its other hooks, which
/// stay. Its report: the settings file and whether the hook was installed
/// or already there.
pub fn connect_prompt_context(store: &Store, cwd: &Path) -> Result<Value> {
    let session = local::session(store, false)?;
    let project = store.resolve_project(
        &session.workspace,
        &session.capability,
        None,
        Some(cwd),
        session.project.as_deref(),
    )?;
    let directory = store.directory.canonical()?;
    let plan = plan_hooks(
        "claude-code",
        &stable_program()?,
        &directory,
        true,
        cwd,
        &[CLAUDE_CODE_PROMPT_CONTEXT],
    )?;
    let mut written = write_hooks(plan, &directory, &cwd.canonical()?, &project.id)?;
    Ok(written["prompt_context"].take())
}

/// Write a hook plan (backing the file up first) and record it in the data
/// folder `directory`; `folder` is the folder `connect` ran in, for
/// `project`.
fn write_hooks(plan: HookPlan, directory: &Path, folder: &Path, project: &str) -> Result<Value> {
    let existed = plan.destination.exists();
    if plan.outcomes.iter().any(|(_, installed)| *installed) || !plan.removed.is_empty() {
        if let Some(parent) = plan.destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        agent_files::backup_in(directory, &plan.destination)?;
        agent_files::write_json(
            &plan.destination,
            plan.original.as_deref(),
            None,
            &plan.document,
        )?;
        // A personal settings file Graphicle created stays out of Git.
        if !existed && plan.client == "claude-code" {
            agent_files::exclude_created(directory, &plan.destination)?;
        }
    }
    agent_files::record(
        directory,
        agent_files::Change {
            path: std::path::absolute(&plan.destination)?,
            kind: "hooks".into(),
            client: Some(plan.client.clone()),
            // Codex's hooks are the user's, for every project.
            folder: (plan.client != "codex").then(|| folder.to_path_buf()),
            project: (plan.client != "codex").then(|| project.to_string()),
            created: !existed,
            ..Default::default()
        },
    )?;
    let listed = |guard: bool, installed: bool| -> Vec<&str> {
        plan.outcomes
            .iter()
            .filter(|(spec, done)| spec.guard == guard && !spec.prompt && *done == installed)
            .map(|(spec, _)| if guard { spec.argument } else { spec.event })
            .collect()
    };
    let prompt = |installed: bool| -> Vec<&str> {
        plan.outcomes
            .iter()
            .filter(|(spec, done)| spec.prompt && *done == installed)
            .map(|(spec, _)| spec.event)
            .collect()
    };
    let next = if plan.client == "codex" {
        "Review and trust the new hooks in Codex before they run"
    } else {
        "Keep .claude/settings.local.json out of version control; restart Claude Code"
    };
    // A capture hook is named by its event, a guard hook by its argument.
    Ok(json!({
        "capture": {"configuration":plan.destination,"installed":listed(false, true),"unchanged":listed(false, false),
            "capture":"completed turns after each answer; reviewed memory at session start; private spans dropped, credentials redacted",
            "next":next},
        "guard": {"configuration":plan.destination,"installed":listed(true, true),"unchanged":listed(true, false),
            "removed":plan.removed,
            "nudge":"before a search or a source read in a folder whose Graphicle project has a code graph, the agent is pointed to query_graph, get_node, get_neighbors and recall_project; elsewhere the graph engine's own guard answers",
            "next":next},
        "prompt_context": {"configuration":plan.destination,"installed":prompt(true),"unchanged":prompt(false),
            "context":"before each prompt that is a code question, the packet of graphicle context (at most 8,400 characters); nothing for other prompts, short ones, or when it fails or takes over 10 seconds",
            "next":next},
    }))
}

/// The arguments of the capture hooks `connect --hooks` installs.
const HOOK_ARGUMENTS: [&str; 4] = ["session-start", "stop", "pre-compact", "session-end"];

/// The arguments of the guard hooks `--guards` (setup, `connect`) installs
/// for Claude Code.
pub const GUARD_ARGUMENTS: [&str; 2] = ["guard-search", "guard-read"];

/// The argument of Claude Code's prompt-context hook (`setup
/// --prompt-context`).
pub const PROMPT_ARGUMENTS: [&str; 1] = ["prompt-context"];

/// Whether a hook handler runs `<program> --data-dir <folder> hook <client>
/// <argument>` for one of `arguments`.
fn is_hook(handler: &Value, client: &str, arguments: &[&str]) -> bool {
    handler["command"].as_str().is_some_and(|command| {
        command.contains(" --data-dir ")
            && arguments
                .iter()
                .any(|argument| command.ends_with(&format!(" hook {client} {argument}")))
    })
}

/// Whether a hook handler is one of Graphicle's capture hooks for `client`
/// (`'<program>' --data-dir '<folder>' hook <client> <event>`).
pub fn is_capture_hook(handler: &Value, client: &str) -> bool {
    is_hook(handler, client, &HOOK_ARGUMENTS)
}

/// Whether a hook handler is one of Graphicle's guard hooks for `client`.
pub fn is_guard_hook(handler: &Value, client: &str) -> bool {
    is_hook(handler, client, &GUARD_ARGUMENTS)
}

/// Whether a hook handler is Graphicle's prompt-context hook for `client`.
pub fn is_prompt_hook(handler: &Value, client: &str) -> bool {
    is_hook(handler, client, &PROMPT_ARGUMENTS)
}

/// Whether a hooks document holds one of the hooks `arguments` name for
/// `client` that opens the data folder `data`.
fn holds_hooks(document: &Value, client: &str, data: &Path, arguments: &[&str]) -> bool {
    document["hooks"]
        .as_object()
        .into_iter()
        .flat_map(|table| table.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group["hooks"].as_array())
        .flatten()
        .any(|handler| {
            is_hook(handler, client, arguments)
                && handler["command"]
                    .as_str()
                    .is_some_and(|command| hook_uses_store(command, data))
        })
}

/// Whether a hooks document holds a capture hook of `client` that opens the
/// data folder `data`.
pub fn holds_capture_hooks(document: &Value, client: &str, data: &Path) -> bool {
    holds_hooks(document, client, data, &HOOK_ARGUMENTS)
}

/// Whether a hooks document holds a guard hook of `client` that opens the
/// data folder `data`.
pub fn holds_guard_hooks(document: &Value, client: &str, data: &Path) -> bool {
    holds_hooks(document, client, data, &GUARD_ARGUMENTS)
}

/// Whether a hooks document holds the prompt-context hook of `client` that
/// opens the data folder `data`.
pub fn holds_prompt_hooks(document: &Value, client: &str, data: &Path) -> bool {
    holds_hooks(document, client, data, &PROMPT_ARGUMENTS)
}

/// Remove Graphicle's capture, guard and prompt-context hooks for `client`
/// that open the data folder `data` from a hooks document, with the groups,
/// events and `hooks` object that held only them; other data folders' hooks
/// stay. Returns whether any was there.
pub fn remove_hooks(document: &mut Value, client: &str, data: &Path) -> bool {
    remove_matching(document, data, &|handler| {
        is_capture_hook(handler, client)
            || is_guard_hook(handler, client)
            || is_prompt_hook(handler, client)
    })
}

/// Remove the hook handlers `ours` accepts that open the data folder `data`
/// from a hooks document, with the groups, events and `hooks` object that
/// held only them. Returns whether any was there.
fn remove_matching(document: &mut Value, data: &Path, ours: &dyn Fn(&Value) -> bool) -> bool {
    let Some(table) = document.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut removed = false;
    let events: Vec<String> = table.keys().cloned().collect();
    for event in events {
        let Some(groups) = table.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        let mut emptied = false;
        groups.retain_mut(|group| {
            let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = handlers.len();
            handlers.retain(|handler| {
                !(ours(handler)
                    && handler["command"]
                        .as_str()
                        .is_some_and(|command| hook_uses_store(command, data)))
            });
            if handlers.len() == before {
                return true;
            }
            removed = true;
            emptied = true;
            !handlers.is_empty()
        });
        if emptied && groups.is_empty() {
            table.shift_remove(&event);
        }
    }
    if removed
        && table.is_empty()
        && let Some(object) = document.as_object_mut()
    {
        object.shift_remove("hooks");
    }
    removed
}

/// Remove the entry `name` from `group` when it runs Graphicle's MCP
/// server for the data folder `data` and, given `project`, names that
/// project or follows its folder ([`agent_files::entry_serves`]), with the
/// group when that empties it. Returns whether it was removed.
pub fn remove_entry(
    document: &mut Value,
    group: &str,
    name: &str,
    data: &Path,
    project: Option<&str>,
) -> bool {
    let Some(servers) = document.get_mut(group).and_then(Value::as_object_mut) else {
        return false;
    };
    if !servers
        .get(name)
        .is_some_and(|entry| agent_files::entry_serves(entry, data, project))
    {
        return false;
    }
    servers.shift_remove(name);
    if servers.is_empty()
        && let Some(object) = document.as_object_mut()
    {
        object.shift_remove(group);
    }
    true
}

/// The names of the Graphicle entries in `group` that open the data folder
/// `data` (`project`: only those naming that project).
pub fn graphicle_entries(
    document: &Value,
    group: &str,
    project: Option<&str>,
    data: &Path,
) -> Vec<String> {
    document[group]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, entry)| {
            agent_files::entry_serves(entry, data, project)
                && (project.is_none() || agent_files::entry_project(entry).is_some())
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The Graphicle entries in `group` that open another data folder than
/// `data`: `(name, that folder)`.
pub fn other_stores(document: &Value, group: &str, data: &Path) -> Vec<(String, PathBuf)> {
    document[group]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, entry)| {
            agent_files::is_graphicle_entry(entry) && !agent_files::entry_uses_store(entry, data)
        })
        .map(|(name, entry)| {
            let args = entry["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            (name.clone(), agent_files::store_of(args))
        })
        .collect()
}

/// What a guard hook of `--guards` answers.
#[derive(Debug, PartialEq, Eq)]
pub enum Guard {
    /// Point the agent to the server's tools: print this hook output.
    Nudge(String),
    /// The folder is no Graphicle project with a code graph: the graph
    /// engine's own guard answers, unchanged.
    Engine,
    /// Nothing to say.
    Quiet,
}

/// The source files a read guard speaks about, as the graph engine's guard
/// lists them (`_HOOK_SOURCE_EXTS`).
const SOURCE_EXTENSIONS: [&str; 29] = [
    "py", "js", "cjs", "ts", "tsx", "jsx", "astro", "vue", "svelte", "go", "rs", "java", "rb", "c",
    "h", "cpp", "hpp", "cc", "cs", "kt", "swift", "php", "scala", "lua", "sh", "md", "rst", "txt",
    "mdx",
];

/// Source files the graph engine's guard speaks about beyond
/// [`SOURCE_EXTENSIONS`]: a read of one still opens the store, so that the
/// engine's guard answers it outside a Graphicle project.
const ENGINE_ONLY_EXTENSIONS: [&str; 4] = ["ejs", "ps1", "psm1", "psd1"];

/// Whether `target` (a path or a Glob pattern) names a file with one of
/// `extensions`.
fn has_extension(target: &str, extensions: &[&str]) -> bool {
    let name = target.rsplit(['/', '\\']).next().unwrap_or(target);
    name.rsplit_once('.')
        .is_some_and(|(_, extension)| extensions.contains(&extension.to_ascii_lowercase().as_str()))
}

/// How many nudges one agent gets in one session unless
/// `GRAPHICLE_HOOK_NUDGE_CAP` says otherwise.
const NUDGE_CAP: u64 = 5;

/// `GRAPHICLE_HOOK_NUDGE_CAP`, read as the graph engine's guard reads it:
/// unset is [`NUDGE_CAP`]; 0, or anything but a number, lifts the cap.
fn nudge_cap() -> u64 {
    match std::env::var("GRAPHICLE_HOOK_NUDGE_CAP") {
        Err(std::env::VarError::NotPresent) => NUDGE_CAP,
        Ok(value) => value.trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Whether a guard may nudge for this payload, with the graph engine's
/// budget: per agent (the payload's `session_id`, plus the `agent_id` Claude
/// Code sends for a subagent, which shares its parent's session but starts
/// with an empty context) the first nudge always fires and at most `cap`
/// fire (0: no cap). The count is a marker file in the data folder's
/// `hook-nudges` folder; markers older than a day are removed when a new one
/// starts. A payload without a session id, or a marker that cannot be
/// written, is not budgeted.
fn nudge_allowed(directory: &Path, payload: &Value, cap: u64) -> bool {
    use sha2::Digest;
    let session = payload["session_id"].as_str().unwrap_or("");
    // A marker file name: `[^A-Za-z0-9_-]` becomes `_`, at most 64 characters.
    let mut key: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if key.is_empty() {
        return true;
    }
    if let Some(agent) = payload["agent_id"]
        .as_str()
        .filter(|agent| !agent.is_empty())
    {
        // Hashed, so that long ids cannot truncate two agents into one key.
        let digest = sha2::Sha256::digest(agent.as_bytes());
        key.push('-');
        key.extend(digest.iter().take(8).map(|byte| format!("{byte:02x}")));
    }
    let folder = directory.join("hook-nudges");
    if std::fs::create_dir_all(&folder).is_err() {
        return true;
    }
    let marker = folder.join(format!("{key}.nudges"));
    let count: u64 = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    if cap > 0 && count >= cap {
        return false;
    }
    if count == 0 {
        let day = std::time::Duration::from_secs(86_400);
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            if entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|time| time.elapsed().unwrap_or_default() > day)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    // Written whole, then renamed over the marker.
    let _ = tempfile::NamedTempFile::new_in(&folder).and_then(|mut file| {
        use std::io::Write;
        file.write_all((count + 1).to_string().as_bytes())?;
        file.persist(&marker).map_err(|error| error.error)
    });
    true
}

/// Whether a shell command runs a text search (`grep`, `rg`, `find`, `fd`,
/// `ack`, `ag`, `git grep`), behind wrappers such as `sudo` or `xargs`, in
/// any of its pipeline or list segments; quoted text is not a command.
fn runs_a_search(command: &str) -> bool {
    const WRAPPERS: [&str; 12] = [
        "sudo", "command", "exec", "nohup", "time", "nice", "ionice", "env", "xargs", "timeout",
        "stdbuf", "doas",
    ];
    const SEARCHES: [&str; 10] = [
        "grep", "egrep", "fgrep", "zgrep", "rg", "ripgrep", "find", "fd", "ack", "ag",
    ];
    let quoted = regex::Regex::new(r#"'[^']*'|"[^"]*""#).expect("quoted text");
    let text = quoted.replace_all(command, " ");
    let separators = regex::Regex::new(r"[|;&\n(){}`]|\$\(").expect("separators");
    separators.split(&text).any(|segment| {
        let mut words = segment.split_whitespace().peekable();
        while let Some(word) = words.next() {
            if word.contains('=') && !word.starts_with(['-', '/']) {
                continue;
            }
            let program = word.rsplit('/').next().unwrap_or(word).to_ascii_lowercase();
            let program = program.trim_end_matches(".exe");
            if WRAPPERS.contains(&program) {
                while words.peek().is_some_and(|next| next.starts_with('-')) {
                    words.next();
                }
                continue;
            }
            if SEARCHES.contains(&program) {
                return true;
            }
            return program == "git" && words.take(3).any(|next| next == "grep");
        }
        false
    })
}

/// A guard hook's answer for Claude Code's `PreToolUse`
/// payload, in the store at `directory`: when the folder the client works in
/// (`cwd`, else `CLAUDE_PROJECT_DIR`, else this process's folder) is a
/// Graphicle project whose code graph holds code symbols (a
/// conversation or notes folder's graph holds none), a search (`search`:
/// Grep, or Bash running a search program) or a read (`read`: Read of one of
/// its source files that the graph has nodes in, or Glob of source files in
/// it) gets a nudge to the server's graph and recall tools, within each
/// agent's nudge budget ([`nudge_allowed`]); elsewhere the graph engine's own
/// guard answers. A call that is no search, or names no source file, is
/// answered before the store is opened. The store is read without migrating
/// or writing; any error leaves it to the engine.
pub fn guard(directory: &Path, kind: &str, payload: &Value) -> Guard {
    let folder = payload["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok());
    let Some(folder) = folder else {
        return Guard::Engine;
    };
    let input = payload.get("tool_input").unwrap_or(payload);
    let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    let targets = [text("file_path"), text("path"), text("pattern")];
    // Neither guard says anything about a command that searches nothing or
    // a read of no source file.
    let source = |target: &&str| has_extension(target, &SOURCE_EXTENSIONS);
    match kind {
        "search" => {
            let command = text("command");
            let searching = if command.is_empty() {
                !text("pattern").is_empty()
            } else {
                runs_a_search(command)
            };
            if !searching {
                return Guard::Quiet;
            }
        }
        "read" => {
            if !targets.iter().any(source)
                && !targets
                    .iter()
                    .any(|target| has_extension(target, &ENGINE_ONLY_EXTENSIONS))
            {
                return Guard::Quiet;
            }
        }
        _ => return Guard::Quiet,
    }
    // The file a Read names, resolved against the folder.
    let read_file =
        (kind == "read" && !text("file_path").is_empty()).then(|| folder.join(text("file_path")));
    let Some(found) =
        crate::graph_command::project_with_graph(directory, &folder, read_file.as_deref())
    else {
        return Guard::Engine;
    };
    let (name, root) = (found.name, found.root);
    let context = match kind {
        "search" => format!(
            "Graphicle: this folder is the project \"{name}\", whose code graph is built. For where or how something works, for questions across files or history, and before changing unfamiliar code, ask the graphicle MCP server first: query_graph \"<question>\" walks the code graph around a question, get_node and get_neighbors show a symbol with its callers and callees, and recall_project searches the code, documents and reviewed memory together. For an exact identifier you already know, grep is enough; search the files to modify or debug specific lines."
        ),
        "read" => {
            // A target is a path, or a Glob pattern, relative to the folder
            // the client works in unless absolute; `.` and `..` are resolved
            // before it is compared with the project's folder.
            let base = folder.canonical().unwrap_or_else(|_| folder.clone());
            let inside = |target: &str| {
                let path = base.join(target);
                path.canonical()
                    .unwrap_or_else(|_| lexical(&path))
                    .starts_with(&root)
            };
            // A read of the engine's other source files gets no nudge here.
            if !targets.iter().any(source) {
                return Guard::Quiet;
            }
            if !targets
                .iter()
                .filter(|target| !target.is_empty())
                .all(|target| inside(target))
            {
                return Guard::Quiet;
            }
            // A Read names one file: the graph must have nodes in it (a
            // `.txt` file, or one the graph left out, gets no nudge).
            if read_file.is_some() && !found.covers {
                return Guard::Quiet;
            }
            let what = if read_file.is_some() {
                "this file"
            } else {
                "these source files"
            };
            format!(
                "Graphicle: the code graph of the project \"{name}\" covers {what}. Orient with the graphicle MCP tools first: query_graph \"<question>\" for the code around a question, get_node or get_neighbors for one symbol, recall_project for what the code, documents and reviewed memory say. Reading the file to modify or debug specific lines is fine."
            )
        }
        _ => return Guard::Quiet,
    };
    if !nudge_allowed(directory, payload, nudge_cap()) {
        return Guard::Quiet;
    }
    Guard::Nudge(
        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":context}})
            .to_string(),
    )
}

/// `path` with its `.` and `..` components resolved without reading the
/// file system (a Glob pattern names no file).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_hook_paths_are_double_quoted_with_slashes() {
        let quoted = quote(r"C:\Program Files\Graphicle\graphicle.exe");
        assert_eq!(quoted, "\"C:/Program Files/Graphicle/graphicle.exe\"");
        assert_eq!(
            crate::terminal::words(&format!("{quoted} --data-dir \"C:/a b\" hook codex start")),
            [
                "C:/Program Files/Graphicle/graphicle.exe",
                "--data-dir",
                "C:/a b",
                "hook",
                "codex",
                "start"
            ]
        );
    }

    /// The Windows hook spellings (`hook_prefix` there) on any host: a plain
    /// program path is bare and the data folder single-quoted, so Git Bash
    /// and PowerShell both run the command; anything else falls back to
    /// double quotes. The readback parser takes every form back.
    #[test]
    fn windows_hook_spellings_read_back() {
        assert_eq!(
            bare_or_quoted(r"C:\Profile\me\AppData\Local\Programs\Graphicle\bin\graphicle.exe"),
            "C:/Profile/me/AppData/Local/Programs/Graphicle/bin/graphicle.exe"
        );
        for needs_quotes in [
            r"C:\Profile\Jo Smith\graphicle.exe",
            r"C:\Profile\O'Brien\graphicle.exe",
            "C:\\Profile\\Jos\u{e9}\\graphicle.exe",
            r"C:\a$b\graphicle.exe",
            r"C:\a&b\graphicle.exe",
            r"\\server\share\graphicle.exe",
            "graphicle.exe",
        ] {
            let quoted = bare_or_quoted(needs_quotes);
            assert_eq!(quoted, format!("\"{}\"", needs_quotes.replace('\\', "/")));
        }
        assert_eq!(
            single_quoted(r"C:\Profile\Jo Smith\AppData\Local\Graphicle"),
            "'C:/Profile/Jo Smith/AppData/Local/Graphicle'"
        );
        assert_eq!(
            single_quoted(r"C:\Profile\O'Brien\AppData"),
            "\"C:/Profile/O'Brien/AppData\""
        );
        assert_eq!(
            single_quoted("C:\\Profile\\O\u{2019}Brien"),
            "\"C:/Profile/O\u{2019}Brien\""
        );
        for (program, store) in [
            (r"C:\bin\graphicle.exe", r"C:\Profile\Jo Smith\Graphicle"),
            (
                r"C:\Program Files\graphicle.exe",
                r"C:\Profile\O'Brien\Graphicle",
            ),
        ] {
            let command = format!(
                "{} --data-dir {} hook claude-code stop",
                bare_or_quoted(program),
                single_quoted(store)
            );
            assert_eq!(
                crate::terminal::words(&command),
                [
                    program.replace('\\', "/").as_str(),
                    "--data-dir",
                    store.replace('\\', "/").as_str(),
                    "hook",
                    "claude-code",
                    "stop"
                ],
                "{command}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_hook_prefix_is_posix_quoted() {
        assert_eq!(
            hook_prefix(
                "claude-code",
                Path::new("/opt/bin/graphicle"),
                Path::new("/home/a/Application Support/Graphicle")
            ),
            "'/opt/bin/graphicle' --data-dir '/home/a/Application Support/Graphicle' hook claude-code "
        );
    }

    #[test]
    fn claude_code_hook_program_runs_in_both_windows_shells() {
        assert_eq!(
            claude_program(r"C:\bin\graphicle.exe", || true),
            "C:/bin/graphicle.exe"
        );
        assert_eq!(
            claude_program(r"D:\Profiles\Jo Smith\bin\graphicle.exe", || true),
            "graphicle"
        );
        assert_eq!(
            claude_program(r"D:\Profiles\Jo Smith\bin\graphicle.exe", || false),
            "\"D:/Profiles/Jo Smith/bin/graphicle.exe\""
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_hook_prefix_suits_each_clients_shell() {
        assert_eq!(
            hook_prefix(
                "claude-code",
                Path::new(r"C:\bin\graphicle.exe"),
                Path::new(r"D:\Profile\AppData\Local\Graphicle")
            ),
            "C:/bin/graphicle.exe --data-dir 'D:/Profile/AppData/Local/Graphicle' hook claude-code "
        );
        assert_eq!(
            hook_prefix(
                "codex",
                Path::new(r"C:\bin\graphicle.exe"),
                Path::new(r"D:\Profile\AppData\Local\Graphicle")
            ),
            "\"C:/bin/graphicle.exe\" --data-dir \"D:/Profile/AppData/Local/Graphicle\" hook codex "
        );
    }

    #[test]
    fn removing_capture_hooks_keeps_every_other_hook() {
        let d = Path::new("/d");
        let ours = |event: &str| json!({"type":"command","command":format!("'/bin/graphicle' --data-dir '/d' hook claude-code {event}"),"timeout":10});
        let original = json!({"permissions":{"allow":["Bash(ls)"]},
            "hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo other"}]}]}});
        let mut document = original.clone();
        document["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(json!({"hooks":[ours("stop")]}));
        document["hooks"]["SessionStart"] =
            json!([{"matcher":"startup|clear|compact","hooks":[ours("session-start")]}]);
        assert!(remove_hooks(&mut document, "claude-code", d));
        assert_eq!(document, original);
        assert!(!remove_hooks(&mut document, "claude-code", d));
        let mut only = json!({"hooks":{"Stop":[{"hooks":[ours("stop")]}]}});
        assert!(remove_hooks(&mut only, "claude-code", d));
        assert_eq!(only, json!({}));
        // Another client's hooks are not this one's.
        let mut codex = json!({"hooks":{"Stop":[{"hooks":[ours("stop")]}]}});
        assert!(!remove_hooks(&mut codex, "codex", d));
        // Nor another data folder's.
        let mut other = json!({"hooks":{"Stop":[{"hooks":[ours("stop")]}]}});
        assert!(!remove_hooks(&mut other, "claude-code", Path::new("/e")));
        // The guard hooks of `--guards` go too, another
        // tool's PreToolUse hook stays.
        let mut guarded = original.clone();
        guarded["hooks"]["PreToolUse"] = json!([
            {"matcher":"Bash","hooks":[{"type":"command","command":"lint-guard"}]},
            {"matcher":"Bash|Grep","hooks":[ours("guard-search")]},
            {"matcher":"Read|Glob","hooks":[ours("guard-read")]}]);
        assert!(holds_hooks(&guarded, "claude-code", d, &GUARD_ARGUMENTS));
        assert!(!holds_hooks(&guarded, "claude-code", d, &HOOK_ARGUMENTS));
        assert!(remove_hooks(&mut guarded, "claude-code", d));
        let mut kept = original.clone();
        kept["hooks"]["PreToolUse"] =
            json!([{"matcher":"Bash","hooks":[{"type":"command","command":"lint-guard"}]}]);
        assert_eq!(guarded, kept);
        // The prompt-context hook of `--prompt-context` goes too.
        let mut prompted = original.clone();
        prompted["hooks"]["UserPromptSubmit"] = json!([{"hooks":[ours("prompt-context")]}]);
        assert!(holds_prompt_hooks(&prompted, "claude-code", d));
        assert!(!holds_hooks(&prompted, "claude-code", d, &HOOK_ARGUMENTS));
        assert!(!holds_guard_hooks(&prompted, "claude-code", d));
        assert!(remove_hooks(&mut prompted, "claude-code", d));
        assert_eq!(prompted, original);
    }

    /// The entry `setup --prompt-context` writes: one UserPromptSubmit
    /// group, no matcher, the hook with its backstop timeout.
    #[cfg(not(windows))]
    #[test]
    fn the_prompt_hook_entry_runs_the_hook_with_its_timeout() {
        let folder = tempfile::TempDir::new().unwrap();
        let plan = plan_hooks(
            "claude-code",
            Path::new("/usr/local/bin/graphicle"),
            Path::new("/d"),
            false,
            folder.path(),
            &[CLAUDE_CODE_PROMPT_CONTEXT],
        )
        .unwrap();
        assert_eq!(
            plan.document,
            json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command",
                "command":"'/usr/local/bin/graphicle' --data-dir '/d' hook claude-code prompt-context",
                "timeout":crate::prompt_hook::HOOK_TIMEOUT}]}]}})
        );
        assert_eq!(plan.outcomes.len(), 1);
        assert!(plan.outcomes[0].0.prompt && plan.outcomes[0].1);
    }

    #[test]
    fn only_graphicle_entries_are_removed() {
        let d = Path::new("/d");
        let mut document = json!({"mcpServers":{
            "graphicle":{"command":"graphicle","args":["--data-dir","/d","mcp","--project","."]},
            "graphicle-docs":{"command":"node","args":["docs.js"]},
            "graphicle-b":{"command":"graphicle","args":["--data-dir","/b","mcp","--project","p9"]},
            "other":{"command":"x"}}});
        assert_eq!(
            graphicle_entries(&document, "mcpServers", None, d),
            ["graphicle"]
        );
        assert!(graphicle_entries(&document, "mcpServers", Some("p1"), d).is_empty());
        assert_eq!(
            other_stores(&document, "mcpServers", d),
            [("graphicle-b".to_string(), PathBuf::from("/b"))]
        );
        assert!(!remove_entry(
            &mut document,
            "mcpServers",
            "graphicle-docs",
            d,
            None
        ));
        assert!(!remove_entry(
            &mut document,
            "mcpServers",
            "graphicle-b",
            d,
            None
        ));
        assert!(remove_entry(
            &mut document,
            "mcpServers",
            "graphicle",
            d,
            Some("p1")
        ));
        assert!(document["mcpServers"].get("graphicle").is_none());
        assert!(document["mcpServers"].get("graphicle-docs").is_some());
        let mut alone = json!({"servers":{"graphicle":{"command":"/x/graphicle","args":["--data-dir","/d","mcp"]}}});
        assert!(remove_entry(&mut alone, "servers", "graphicle", d, None));
        assert_eq!(alone, json!({}));
    }

    #[test]
    fn a_project_named_like_another_gets_its_own_entry() {
        let d = Path::new("/d");
        let entry = |project: &str| json!({"command":"/bin/graphicle","args":["--data-dir","/d","mcp","--project",project]});
        let existing = |found: Vec<(&'static str, Value)>| {
            move |name: &str| {
                found
                    .iter()
                    .find(|(known, _)| *known == name)
                    .map(|(_, entry)| entry.clone())
            }
        };
        let live = |id: &str| id != "gone";
        // Free, ours, or a project this store no longer has: the plain name.
        assert_eq!(
            entry_for_project("graphicle-api", "Ab12cd34ef", d, &existing(vec![]), &live),
            "graphicle-api"
        );
        let ours = existing(vec![("graphicle-api", entry("Ab12cd34ef"))]);
        assert_eq!(
            entry_for_project("graphicle-api", "Ab12cd34ef", d, &ours, &live),
            "graphicle-api"
        );
        let stale = existing(vec![("graphicle-api", entry("gone"))]);
        assert_eq!(
            entry_for_project("graphicle-api", "Ab12cd34ef", d, &stale, &live),
            "graphicle-api"
        );
        // Another live project, another data folder, or not Graphicle's.
        let taken = existing(vec![("graphicle-api", entry("other"))]);
        assert_eq!(
            entry_for_project("graphicle-api", "Ab12cd34ef", d, &taken, &live),
            "graphicle-api-ab12cd34"
        );
        let elsewhere = existing(vec![("graphicle-api", entry("Ab12cd34ef"))]);
        assert_eq!(
            entry_for_project(
                "graphicle-api",
                "Ab12cd34ef",
                Path::new("/e"),
                &elsewhere,
                &live
            ),
            "graphicle-api-ab12cd34"
        );
        let foreign = existing(vec![("graphicle-api", json!({"command":"node"}))]);
        assert_eq!(
            entry_for_project("graphicle-api", "Ab12cd34ef", d, &foreign, &live),
            "graphicle-api-ab12cd34"
        );
    }
}

#[cfg(test)]
mod guard_tests {
    use super::{lexical, runs_a_search};
    use std::path::Path;

    #[test]
    fn a_target_is_resolved_against_the_folder_without_reading_it() {
        assert_eq!(
            lexical(Path::new("/w/project/./src/../../other/x.py")),
            Path::new("/w/other/x.py")
        );
        assert_eq!(
            lexical(Path::new("/w/project/src/**/*.py")),
            Path::new("/w/project/src/**/*.py")
        );
    }

    #[test]
    fn searches_are_found_in_any_segment_behind_wrappers() {
        for command in [
            "grep -rn alpha src",
            "cd src && rg beta",
            "FOO=1 sudo -n find . -name '*.py'",
            "ls | xargs -0 grep alpha",
            "git -C repo grep -n beta",
            "/usr/bin/fd main",
        ] {
            assert!(runs_a_search(command), "{command}");
        }
        for command in [
            "ls -la",
            "echo 'grep alpha'",
            "cargo test",
            "git status",
            "python grep.py",
        ] {
            assert!(!runs_a_search(command), "{command}");
        }
    }
}
