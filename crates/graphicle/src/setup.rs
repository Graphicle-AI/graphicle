//! `graphicle setup`: from a repository to agents that use it in one
//! command, and `connect` without a client.
//! `disconnect` and `uninstall` undo it ([`crate::disconnect`]).
//!
//! Setup's steps, in order:
//! 1. the folder's project: synced, with its code graph built in the
//!    foreground;
//! 2. the agent clients found on this machine, connected through
//!    [`agent_setup::connect_with`] (asked on a terminal, else `--yes` or
//!    `--clients`);
//! 3. session-capture hooks for Claude Code and Codex (`--no-capture`
//!    skips them), and Claude Code's guard hooks with `--guards`;
//! 4. the Graphicle section of AGENTS.md (created only with consent) and of
//!    CLAUDE.md when it exists (`--no-instructions` skips it); not added
//!    when Claude Code, which reads the server's instructions, is the only
//!    client connected;
//! 5. freshness Git hooks (`--no-hooks` skips them);
//! 6. Graphify: what is set up, and the switch to Graphicle (asked on a
//!    terminal, else `--switch-from-graphify`);
//! 7. the answer model: the one configured, else the best one this
//!    environment has, offered on a terminal and taken with `--yes`
//!    ([`model_step`]; the configuration names the key's variable, never
//!    the key);
//! 8. a second run changes nothing and says what was already set;
//! 9. a summary that lists every file changed outside the data folder and
//!    how fresh the project is (the files setup wrote inside the folder are
//!    synced before it, so the first answer has nothing to catch up on).
//!
//! A terminal gets text; a pipe or `--json` gets the JSON report
//! ([`crate::terminal::human`]: `GRAPHICLE_OUTPUT=text|json` overrides the
//! terminal test).
use crate::agent_detect::{self, Environment, Found};
use crate::agent_files::{self, Change};
use crate::platform::CanonicalExt;
use crate::{
    Error, Result, agent_instructions, agent_setup,
    api::ToolSet,
    codex_config, git_hooks, local, model,
    store::{Store, s},
    terminal,
};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// How a command talks to the person running it.
pub struct Ui {
    /// Text on standard output (a terminal, without `--json`).
    pub text: bool,
    /// Questions may be asked (standard input and error are terminals, and
    /// neither `--json` nor `--yes` was given).
    pub interactive: bool,
}

impl Ui {
    pub fn new(json: bool, yes: bool) -> Self {
        Self {
            // Every command's output rule: `--json`, else GRAPHICLE_OUTPUT,
            // else whether standard output is a terminal.
            text: terminal::human(json),
            interactive: !json
                && !yes
                && std::io::stdin().is_terminal()
                && std::io::stderr().is_terminal(),
        }
    }

    /// Ask `question` on standard error; the answer, trimmed (`None` when
    /// no question may be asked or the input ended).
    pub(crate) fn ask(&self, question: &str) -> Option<String> {
        if !self.interactive {
            return None;
        }
        let mut error = std::io::stderr();
        let _ = write!(error, "{question} ");
        let _ = error.flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_string()),
        }
    }

    /// A yes-or-no question; `None` when it cannot be asked.
    fn confirm(&self, question: &str, default: bool) -> Option<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        let answer = self.ask(&format!("{question} {hint}"))?;
        Some(match answer.to_ascii_lowercase().as_str() {
            "" => default,
            "y" | "yes" => true,
            _ => false,
        })
    }

    /// Print `report`: as text when [`Ui::text`], else as JSON.
    pub(crate) fn print(&self, report: &Value, render: impl Fn(&Value) -> String) -> Result<()> {
        if self.text {
            let mut out = std::io::stdout();
            write!(out, "{}", render(report))?;
            out.flush()?;
            Ok(())
        } else {
            crate::cli::print(report)
        }
    }
}

/// `graphicle setup`'s options.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub path: Option<PathBuf>,
    pub yes: bool,
    /// `all`, `none` or a list of clients.
    pub clients: Option<String>,
    pub no_capture: bool,
    pub no_hooks: bool,
    pub no_instructions: bool,
    pub switch_from_graphify: bool,
    pub json: bool,
    /// The model configuration step 7 may write (`--config`; default
    /// `models.json` in the data folder).
    pub config: Option<PathBuf>,
    /// `--guards`: Claude Code's guard hooks (off unless asked for; a run
    /// without it removes those an earlier setup installed).
    pub guards: bool,
    /// `--prompt-context`: Claude Code's prompt-context hook
    /// ([`crate::prompt_hook`]; off unless asked for; a run without it
    /// leaves an earlier setup's hook as it is).
    pub prompt_context: bool,
    /// `--tools`: the tools every connected client's entry publishes;
    /// `None` is each client's default ([`agent_setup::default_tools`]).
    pub tools: Option<ToolSet>,
}

/// Run `graphicle setup` and print its summary. A client that could not
/// be connected makes it fail after the summary.
pub fn setup(store: &Store, config: &model::Config, options: &Options) -> Result<()> {
    let ui = Ui::new(options.json, options.yes);
    let report = setup_report(store, config, options, &ui, &Environment::current())?;
    ui.print(&report, render_setup)?;
    let failed: Vec<String> = report["clients"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|client| client["state"] == "failed")
        .map(|client| {
            format!(
                "{}: {}",
                client["name"].as_str().unwrap_or_default(),
                client["error"].as_str().unwrap_or_default()
            )
        })
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::bad(format!("Not connected: {}", failed.join("; "))))
    }
}

/// The folder setup sets up: `PATH` as given; without it, the project
/// folder this folder is in, when that lies in the same Git repository (a
/// second run from a subfolder sets the same project up again), else this
/// folder.
fn setup_folder(store: &Store, session: &local::Session, path: Option<&Path>) -> Result<PathBuf> {
    let requested = path.unwrap_or(Path::new("."));
    let folder = requested
        .canonical()
        .ok()
        .filter(|folder| folder.is_dir())
        .ok_or_else(|| Error::bad(format!("{} is not a folder", requested.display())))?;
    if path.is_some() {
        return Ok(folder);
    }
    let Some(found) = local::find(store, session)? else {
        return Ok(folder);
    };
    match (found.source, found.folder, git_hooks::repository(&folder)) {
        ("folder", Some(project), Some(repo)) if project.starts_with(&repo.root) => Ok(project),
        _ => Ok(folder),
    }
}

/// The data folder's canonical path.
fn data_folder(store: &Store) -> Result<PathBuf> {
    Ok(store.directory.canonical()?)
}

/// `--data-dir` for the files setup writes: the store, unless it is the
/// platform's default folder (as `connect` writes it).
fn data_argument(data: &Path) -> Option<&Path> {
    (local::default_data_dir().canonical().ok().as_deref() != Some(data)).then_some(data)
}

/// Seconds, to a tenth.
fn seconds(elapsed: std::time::Duration) -> f64 {
    (elapsed.as_secs_f64() * 10.0).round() / 10.0
}

/// Setup's steps; returns the report.
pub fn setup_report(
    store: &Store,
    config: &model::Config,
    options: &Options,
    ui: &Ui,
    env: &Environment,
) -> Result<Value> {
    let started = std::time::Instant::now();
    let data = data_folder(store)?;

    // 1. The project: the folder decides it, and its code graph is
    // built in the foreground.
    let mut session = local::session(store, true)?;
    let folder = setup_folder(store, &session, options.path.as_deref())?;
    eprintln!("graphicle: syncing {}...", crate::terminal::tilde(&folder));
    // Under the folder's lock, as the Git hooks' background sync takes it.
    let mut synced = crate::phase::phase("setup_sync", || {
        crate::fresh::locked(store, &folder, || {
            local::sync(store, &mut session, &folder, None)
        })
    })?;
    let repository = s(&synced, "repository_id").to_string();
    synced["graph"] = crate::phase::phase("setup_graph", || {
        local::sync_graph(store, &session, &repository, true)
    })?;
    let project = store.resolve_project(
        &session.workspace,
        &session.capability,
        Some(&repository),
        Some(&folder),
        None,
    )?;
    let repo = git_hooks::repository(&folder);
    // The repository's exclude file as it is before setup writes: a backup
    // in the working tree adds the line that keeps backups out of Git.
    let exclude = agent_files::work_tree(&folder).map(|(_, exclude)| {
        let before = std::fs::read(&exclude).ok();
        (exclude, before)
    });
    let mut project_report = json!({"id":project.id,"name":project.name,"folder":folder,
        "files":synced["files"],"symbols":local::code_symbols(store, &session, &repository),
        "graph":synced["graph"],"git":repo.is_some(),"sync":synced,
        "seconds":seconds(started.elapsed())});
    if repo.is_none() {
        project_report["note"] = json!(
            "Not a Git repository: no Git hooks keep it fresh; run graphicle sync . after changes"
        );
    }

    // 6, found first: the switch replaces Graphify's entries and section in
    // the steps that write Graphicle's.
    let graphify = agent_detect::detect_graphify(env, &folder);
    let switch = graphify.switchable()
        && (options.switch_from_graphify
            || ui
                .confirm(&graphify_question(&graphify, &folder, env), true)
                .unwrap_or(false));

    // 2 and 3. Agent clients, with capture hooks.
    let detected = agent_detect::detect(env);
    let (mut chosen, clients_note) = choose_clients(options, &detected, ui)?;
    if switch {
        for entry in graphify
            .entries
            .iter()
            .filter(|entry| entry.by_hand.is_none())
        {
            if !chosen.contains(&entry.client) {
                chosen.push(entry.client);
            }
        }
    }
    let mut clients = Vec::new();
    for client in &chosen {
        let connection = agent_setup::default_connection(
            client,
            &folder,
            !options.no_capture,
            options.guards,
            options.tools,
        );
        clients.push(
            match agent_setup::connect_with(
                store,
                &connection,
                agent_setup::Policy {
                    refresh_ours: true,
                    drop_guards: true,
                },
            ) {
                Ok(result) => client_report(client, &result),
                Err(error) => json!({"client":client,"name":agent_detect::name(client),
                    "state":"failed","error":error.message}),
            },
        );
    }
    // Claude Code's prompt-context hook (`--prompt-context`, off by
    // default), beside its other hooks.
    let prompt_context = if !options.prompt_context {
        Value::Null
    } else if clients
        .iter()
        .any(|client| client["client"] == "claude-code" && client["state"] != "failed")
    {
        match agent_setup::connect_prompt_context(store, &folder) {
            Ok(hook) => {
                let added = hook["installed"].as_array().is_some_and(|i| !i.is_empty());
                json!({"state":if added { "added" } else { "already set" },"file":hook["configuration"],
                    "context":hook["context"],"next":hook["next"]})
            }
            Err(error) => json!({"state":"failed","error":error.message}),
        }
    } else {
        json!({"state":"skipped","reason":"Claude Code was not connected"})
    };
    let capture = capture_report(&clients, &chosen, options.no_capture);
    let connected: Vec<&str> = clients
        .iter()
        .filter(|client| client["state"] != "failed")
        .filter_map(|client| client["client"].as_str())
        .collect();

    // 4. Agent instructions: none are added when Claude Code, which reads
    // the server's instructions, is the only client connected.
    let instructions = if options.no_instructions {
        json!([{"state":"skipped","reason":"--no-instructions"}])
    } else {
        instructions_step(
            &data,
            &folder,
            &project.id,
            (
                switch,
                connected == ["claude-code"],
                options.tools == Some(ToolSet::All),
            ),
            ui,
            options.yes,
        )?
    };

    // 5. Freshness Git hooks.
    let git = match (&repo, options.no_hooks) {
        (_, true) => json!({"state":"skipped","reason":"--no-hooks"}),
        (None, false) => json!({"state":"skipped","reason":"not a Git repository"}),
        (Some(repo), false) => git_hooks_step(&data, &folder, &project.id, repo)?,
    };

    // 6. Graphify's entries (its sections went in step 4), in the clients
    // that got Graphicle's.
    let graphify_report = graphify_step(
        &data,
        (&folder, &project.id),
        &graphify,
        (switch, &connected),
        options,
    )?;

    // 7. The answer model.
    let model = model_step(store, &session, config, options, ui);

    // 9. The summary.
    let mut files = Vec::new();
    let mut add = |file: &Value| {
        if let Some(file) = file.as_str()
            && !files.iter().any(|known: &Value| known == file)
        {
            files.push(json!(file));
        }
    };
    for client in &clients {
        if matches!(client["state"].as_str(), Some("added" | "updated")) {
            add(&client["file"]);
        }
        if client["capture"]["installed"]
            .as_array()
            .is_some_and(|installed| !installed.is_empty())
        {
            add(&client["capture"]["file"]);
        }
        if client["guard"]["installed"]
            .as_array()
            .is_some_and(|installed| !installed.is_empty())
            || client["guard"]["state"] == "removed"
        {
            add(&client["guard"]["file"]);
        }
    }
    for written in instructions
        .as_array()
        .into_iter()
        .flatten()
        .chain(git["hooks"].as_array().into_iter().flatten())
        .chain(graphify_report["changed"].as_array().into_iter().flatten())
    {
        if matches!(
            written["state"].as_str(),
            Some("added" | "updated" | "removed")
        ) {
            add(&written["file"]);
        }
    }
    if let Some((exclude, before)) = &exclude
        && std::fs::read(exclude).ok() != *before
    {
        add(&json!(exclude));
    }
    // A model configuration given with --config outside the data folder.
    if model["state"] == "set"
        && let Some(file) = model["file"].as_str()
        && !Path::new(file).starts_with(&data)
    {
        add(&model["file"]);
    }
    // The files written inside the folder (the instructions, Claude Code's
    // and VS Code's entries, capture hooks). A sync leaves Graphicle's own
    // files and lines out, so they usually change nothing it reads: the
    // folder's freshness record then takes them in without a second sync,
    // and the first answer finds the folder up to date. Otherwise (an MCP
    // file of the project's that also names other servers, a Graphify
    // section replaced), they are synced now.
    let written: Vec<String> = files
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|file| Path::new(file).strip_prefix(&folder).ok())
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .collect();
    if !written.is_empty()
        && !crate::fresh::locked(store, &folder, || {
            crate::fresh::absorb_own_writes(store, &session, &repository, &folder, &written)
        })
        .unwrap_or(false)
    {
        eprintln!("graphicle: syncing the files setup wrote in the folder...");
        let resynced = crate::phase::phase("setup_resync", || {
            crate::fresh::locked(store, &folder, || {
                local::sync_folder(store, &session, &folder, Some(&repository))
            })
        })?;
        project_report["files"] = resynced["files"].clone();
        project_report["graph"] = crate::phase::phase("setup_resync_graph", || {
            local::sync_graph(store, &session, &repository, true)
        })?;
        project_report["symbols"] = json!(local::code_symbols(store, &session, &repository));
    }
    // How fast questions are: the passage index and the saved code graph.
    if let Ok(questions) =
        store.authorized_read(&session.workspace, &session.capability, "read", |db, _| {
            crate::indexed_query::readiness(db, &repository)
        })
    {
        project_report["questions"] = questions;
    }
    let freshness = crate::phase::phase("setup_freshness", || {
        freshness_report(store, &session, &repository, config)
    });
    let backups: Vec<Value> = files
        .iter()
        .filter_map(Value::as_str)
        .map(|file| agent_files::backup_path(Path::new(file)))
        .filter(|backup| backup.is_file())
        .map(|backup| json!(backup))
        .collect();
    let mut report = json!({"project":project_report,"clients":clients,"capture":capture,
        "instructions":instructions,"git_hooks":git,"graphify":graphify_report,"model":model,
        "freshness":freshness,"files_changed":files,"backups":backups,"data_dir":data,
        "seconds":seconds(started.elapsed()),
        "next":["graphicle ask \"how is this project organized?\"","graphicle open",
            "In your agent: \"use graphicle to find where errors are handled\""]});
    if let Some(note) = clients_note {
        report["clients_note"] = json!(note);
    }
    if !prompt_context.is_null() {
        report["prompt_context"] = prompt_context;
    }
    Ok(report)
}

/// The clients a list names: `all` (those found), `none`, or names and
/// ids separated by commas or spaces.
fn parse_clients(list: &str, detected: &[Found]) -> Result<Vec<&'static str>> {
    let list = list.trim();
    match list.to_ascii_lowercase().as_str() {
        "all" => return Ok(detected.iter().map(|found| found.client).collect()),
        "none" | "" => return Ok(Vec::new()),
        _ => {}
    }
    let mut chosen = Vec::new();
    for word in list
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|word| !word.is_empty())
    {
        let client = word
            .parse::<usize>()
            .ok()
            .and_then(|number| detected.get(number.checked_sub(1)?))
            .map(|found| found.client)
            .or_else(|| agent_detect::parse(word))
            .ok_or_else(|| {
                Error::bad(format!(
                    "Unknown client {word}: choose claude-code, codex, cursor, vscode or claude (Claude Desktop)"
                ))
            })?;
        if !chosen.contains(&client) {
            chosen.push(client);
        }
    }
    Ok(chosen)
}

/// The clients to connect, and a note when none could be chosen.
fn choose_clients(
    options: &Options,
    detected: &[Found],
    ui: &Ui,
) -> Result<(Vec<&'static str>, Option<String>)> {
    if let Some(list) = &options.clients {
        return Ok((parse_clients(list, detected)?, None));
    }
    if detected.is_empty() {
        return Ok((
            Vec::new(),
            Some("No supported agent client was found (Claude Code, Codex, Cursor, VS Code, Claude Desktop); connect one later with graphicle connect <client>".into()),
        ));
    }
    if options.yes {
        return Ok((detected.iter().map(|found| found.client).collect(), None));
    }
    if ui.interactive {
        eprintln!("Agent clients found on this machine:");
        for (number, found) in detected.iter().enumerate() {
            eprintln!("  {}. {}", number + 1, found.name);
        }
        for _ in 0..3 {
            let Some(answer) = ui.ask("Connect which? [all] (all, none, or numbers such as 1 3):")
            else {
                break;
            };
            let answer = if answer.is_empty() {
                "all".to_string()
            } else {
                answer
            };
            match parse_clients(&answer, detected) {
                Ok(chosen) => return Ok((chosen, None)),
                Err(error) => eprintln!("{}", error.message),
            }
        }
        return Ok((Vec::new(), Some("No agent client was chosen".into())));
    }
    let names: Vec<&str> = detected.iter().map(|found| found.name).collect();
    Ok((
        Vec::new(),
        Some(format!(
            "Not run in a terminal, so no agent client was connected; found {}. Run graphicle setup --yes to connect them all, or --clients LIST",
            names.join(", ")
        )),
    ))
}

/// One client's line of the summary, from `connect`'s reply.
fn client_report(client: &str, result: &Value) -> Value {
    let state = if result["unchanged"] == true {
        "already set"
    } else if result["updated"] == true {
        "updated"
    } else {
        "added"
    };
    let mut report = json!({"client":client,"name":agent_detect::name(client),"state":state,
        "file":result["configuration"],"entry":result["entry"],"project":result["project"]["id"],
        "tools":result["tools"]});
    if let Some(hooks) = result.get("hooks") {
        let installed = hooks["installed"].as_array().map_or(0, Vec::len);
        report["capture"] = json!({"file":hooks["configuration"],"installed":hooks["installed"],
            "state":if installed > 0 { "added" } else { "already set" },"next":hooks["next"]});
    }
    // Claude Code's guard hooks (`--guards`), or those of an earlier setup
    // that this run removed.
    if let Some(guard) = result["graph"]["guard_hooks"].as_object() {
        report["guard"] = json!({"file":guard.get("configuration"),"installed":guard.get("installed"),
            "state":guard_state(guard)});
    }
    if let Some(access) = result.get("access") {
        report["access"] = access.clone();
    }
    if let Some(note) = result.get("note") {
        report["note"] = note.clone();
    }
    report
}

/// What `connect` did with Claude Code's guard hooks: `added`, `removed`
/// (an earlier setup's, without `--guards`) or `already set`.
fn guard_state(guard: &serde_json::Map<String, Value>) -> &'static str {
    let listed = |key: &str| guard.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    if listed("removed") > 0 {
        "removed"
    } else if listed("installed") > 0 {
        "added"
    } else {
        "already set"
    }
}

/// Step 3's report: the capture hooks of the connected clients.
fn capture_report(clients: &[Value], chosen: &[&str], skipped: bool) -> Value {
    if skipped {
        return json!({"state":"skipped","reason":"--no-capture"});
    }
    let hooked: Vec<Value> = clients
        .iter()
        .filter(|client| client.get("capture").is_some())
        .map(|client| {
            json!({"client":client["client"],"name":client["name"],"file":client["capture"]["file"],
                "state":client["capture"]["state"]})
        })
        .collect();
    if hooked.is_empty() {
        let reason = if chosen.iter().any(|client| agent_setup::has_hooks(client)) {
            "the connection failed"
        } else {
            "neither Claude Code nor Codex was connected"
        };
        return json!({"state":"skipped","reason":reason});
    }
    json!({"state":"on","clients":hooked,
        "what":"After each answer, the session's finished turns are stored as sources of this project (private spans dropped, credentials redacted); at session start the agent gets your reviewed memory. graphicle setup --no-capture skips this."})
}

/// Text with Windows line endings read with Unix ones, and the way back.
pub(crate) fn unix(text: &str) -> (String, bool) {
    if text.contains("\r\n") {
        (text.replace("\r\n", "\n"), true)
    } else {
        (text.to_string(), false)
    }
}
pub(crate) fn native(text: String, crlf: bool) -> String {
    if crlf {
        text.replace('\n', "\r\n")
    } else {
        text
    }
}

/// Step 4: the Graphicle section of AGENTS.md (created only with consent)
/// and of CLAUDE.md when it exists and does not already read AGENTS.md.
/// When Claude Code is the only client connected (`only_claude_code`), the
/// server's instructions tell it when to call the tools, so no section is
/// added: a file keeps (and brings up to date) a section an earlier setup
/// wrote, and the switch from Graphify still replaces Graphify's section.
/// `--tools all` (`full`) writes 0.1.0's section, as 0.1.0 did, whichever
/// clients are connected.
fn instructions_step(
    data: &Path,
    folder: &Path,
    project: &str,
    (switch, only_claude_code, full): (bool, bool, bool),
    ui: &Ui,
    yes: bool,
) -> Result<Value> {
    let only_claude_code = only_claude_code && !full;
    let agents = folder.join("AGENTS.md");
    let claude = folder.join("CLAUDE.md");
    let mut written = Vec::new();
    // With Claude Code alone, a file gets the section only where it has one
    // or where the switch replaces Graphify's.
    let skipped = |path: &Path| {
        only_claude_code
            && !std::fs::read_to_string(path).is_ok_and(|text| {
                let text = unix(&text).0;
                agent_instructions::present(&text)
                    || (switch && agent_detect::graphify_section(&text).is_some())
            })
    };
    let alone = |path: &Path| {
        json!({"file":path,"state":"skipped",
            "reason":"Claude Code is the only agent connected, and the graphicle server's instructions tell it when to use the tools"})
    };
    let agents_skipped = skipped(&agents);
    let agents_exists = std::fs::symlink_metadata(&agents).is_ok();
    let create = !agents_skipped
        && (agents_exists
            || yes
            || ui
                .confirm(
                    "Create AGENTS.md with a short section that tells agents when to use Graphicle?",
                    true,
                )
                .unwrap_or(false));
    if agents_skipped {
        written.push(alone(&agents));
    } else if create {
        written.push(write_instructions(
            data,
            (folder, project),
            &agents,
            (switch, full),
        )?);
    } else {
        written.push(json!({"file":agents,"state":"skipped",
            "reason":"AGENTS.md does not exist; graphicle setup --yes creates it"}));
    }
    if std::fs::symlink_metadata(&claude).is_ok() {
        let same =
            agents.canonical().ok().is_some() && agents.canonical().ok() == claude.canonical().ok();
        let text = std::fs::read_to_string(&claude).unwrap_or_default();
        let graphify_here = switch && agent_detect::graphify_section(&text).is_some();
        if same {
            written.push(
                json!({"file":claude,"state":"skipped","reason":"the same file as AGENTS.md"}),
            );
        } else if skipped(&claude) {
            written.push(alone(&claude));
        } else if agent_instructions::imports_agents(&text)
            && create
            && !graphify_here
            && !agent_instructions::present(&text)
        {
            written.push(
                json!({"file":claude,"state":"skipped","reason":"CLAUDE.md imports AGENTS.md"}),
            );
        } else {
            written.push(write_instructions(
                data,
                (folder, project),
                &claude,
                (switch, full),
            )?);
        }
    }
    Ok(json!(written))
}

/// The Graphicle section in `path` (a symlink is followed when it leads
/// into the folder).
fn write_instructions(
    data: &Path,
    (folder, project): (&Path, &str),
    path: &Path,
    (switch, full): (bool, bool),
) -> Result<Value> {
    let target = if path.is_symlink() {
        match path.canonical() {
            Ok(target) if target.starts_with(folder) => target,
            _ => {
                return Ok(json!({"file":path,"state":"skipped",
                    "reason":"a symlink to a file outside this folder"}));
            }
        }
    } else {
        path.to_path_buf()
    };
    let existed = target.exists();
    let raw = if existed {
        match std::fs::read_to_string(&target) {
            Ok(text) => text,
            Err(_) => {
                return Ok(json!({"file":target,"state":"skipped","reason":"not UTF-8 text"}));
            }
        }
    } else {
        String::new()
    };
    let (text, crlf) = unix(&raw);
    // Graphify's section, when the switch replaces it: its lines, shown.
    let graphify_lines = switch
        .then(|| agent_detect::graphify_section(&text))
        .flatten()
        .filter(|_| !agent_instructions::present(&text));
    let section = if full {
        agent_instructions::section_0_1_0()
    } else {
        agent_instructions::section()
    };
    let upserted = agent_instructions::upsert_section(&text, switch, &section);
    if let Some(new) = upserted.text {
        agent_files::backup_in(data, &target)?;
        agent_files::write(&target, native(new, crlf).as_bytes(), 0o644)?;
    }
    agent_files::record(
        data,
        Change {
            path: target.clone(),
            kind: "instructions".into(),
            folder: Some(folder.to_path_buf()),
            project: Some(project.into()),
            created: !existed,
            replaced: upserted.replaced.as_ref().map(|text| json!({"text":text})),
            ..Change::default()
        },
    )?;
    let mut report = json!({"file":target,"state":upserted.state});
    if upserted.replaced.is_some() {
        report["replaced"] = json!("Graphify's section");
        if let Some((start, end)) = graphify_lines {
            report["lines"] = json!([start + 1, end]);
        }
    }
    Ok(report)
}

/// Step 5: the freshness block in the repository's post-commit,
/// post-merge and post-checkout hooks.
fn git_hooks_step(
    data: &Path,
    folder: &Path,
    project: &str,
    repo: &git_hooks::Repository,
) -> Result<Value> {
    let hooks_dir = match &repo.hooks {
        Ok(dir) => dir.clone(),
        Err(reason) => {
            return Ok(json!({"state":"skipped","reason":reason,
                "by_hand":format!("Add this line to post-commit, post-merge and post-checkout in that folder: {}",
                    git_hooks::manual_line(folder, data_argument(data)))}));
        }
    };
    let block = git_hooks::block(
        folder,
        &repo.root,
        &agent_setup::stable_program()?,
        data_argument(data),
    )?;
    let mut hooks = Vec::new();
    for name in git_hooks::HOOKS {
        let path = hooks_dir.join(name);
        if path.is_symlink() {
            hooks.push(json!({"hook":name,"file":path,"state":"skipped","reason":"a symlink"}));
            continue;
        }
        let existed = path.exists();
        let raw = match existed.then(|| std::fs::read_to_string(&path)) {
            None => None,
            Some(Ok(text)) => Some(text),
            Some(Err(_)) => {
                hooks.push(
                    json!({"hook":name,"file":path,"state":"skipped","reason":"not a text file"}),
                );
                continue;
            }
        };
        let (text, crlf) = match &raw {
            Some(raw) => {
                let (text, crlf) = unix(raw);
                (Some(text), crlf)
            }
            None => (None, false),
        };
        let mut entry = match git_hooks::upsert(text.as_deref(), &block, folder) {
            git_hooks::Upserted::Changed(new, state) => {
                agent_files::backup_in(data, &path)?;
                agent_files::write(&path, native(new, crlf).as_bytes(), 0o755)?;
                json!({"hook":name,"file":path,"state":state})
            }
            git_hooks::Upserted::Unchanged => {
                json!({"hook":name,"file":path,"state":"already set"})
            }
            git_hooks::Upserted::NotShell => {
                hooks.push(json!({"hook":name,"file":path,"state":"skipped",
                    "reason":format!("not a shell script; add this line to it yourself: {}",
                        git_hooks::manual_line(folder, data_argument(data)))}));
                continue;
            }
        };
        if existed && !executable(&path) {
            entry["note"] = json!(format!(
                "{name} is not executable, so Git does not run it; chmod +x it to turn it on"
            ));
        }
        agent_files::record(
            data,
            Change {
                path: path.clone(),
                kind: "git-hook".into(),
                folder: Some(folder.to_path_buf()),
                project: Some(project.into()),
                created: !existed,
                ..Change::default()
            },
        )?;
        hooks.push(entry);
    }
    let changed = hooks
        .iter()
        .any(|hook| matches!(hook["state"].as_str(), Some("added" | "updated")));
    let all_set = hooks.iter().all(|hook| hook["state"] == "already set");
    Ok(
        json!({"state":if changed { "added" } else if all_set { "already set" } else { "partly set" },
        "directory":hooks_dir,"hooks":hooks,
        "what":"After commits, merges and checkouts, Graphicle syncs this folder in the background"}),
    )
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn executable(_: &Path) -> bool {
    true
}

/// `~/…` for a path in the home folder, a relative path inside `folder`.
pub(crate) fn shown(path: &Path, folder: Option<&Path>, env: &Environment) -> String {
    if let Some(relative) = folder.and_then(|folder| path.strip_prefix(folder).ok())
        && !relative.as_os_str().is_empty()
    {
        return relative.display().to_string();
    }
    if let Some(relative) = env
        .home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

/// The question that offers the switch from Graphify: what it replaces
/// (each section with its lines), what stays, and what setup cannot edit.
fn graphify_question(
    graphify: &agent_detect::Graphify,
    folder: &Path,
    env: &Environment,
) -> String {
    let mut replaced = Vec::new();
    for entry in graphify
        .entries
        .iter()
        .filter(|entry| entry.by_hand.is_none())
    {
        replaced.push(format!(
            "the {} MCP entry in {}",
            entry.name,
            shown(&entry.file, Some(folder), env)
        ));
    }
    for section in &graphify.sections {
        let lines = std::fs::read_to_string(section)
            .ok()
            .and_then(|text| agent_detect::graphify_section(&setup_unix(&text)))
            .map(|(start, end)| format!(" (lines {}-{end})", start + 1))
            .unwrap_or_default();
        replaced.push(format!(
            "the graphify section of {}{lines}",
            shown(section, Some(folder), env)
        ));
    }
    let mut kept = Vec::new();
    if graphify.output.is_some() {
        kept.push("graphify-out/".to_string());
    }
    if let Some(skill) = &graphify.skill {
        kept.push(format!("the graphify skill ({})", shown(skill, None, env)));
    }
    let mut question = format!(
        "Graphify is set up here. Switch to Graphicle: replace {}? Backups are kept",
        replaced.join(", ")
    );
    if !kept.is_empty() {
        question.push_str(&format!("; {} stay", kept.join(" and ")));
    }
    question.push('.');
    for entry in graphify
        .entries
        .iter()
        .filter(|entry| entry.by_hand.is_some())
    {
        question.push_str(&format!(
            " (Claude Code's own {} server in ~/.claude.json is not edited by setup: {} removes it.)",
            entry.name,
            entry.by_hand.as_deref().unwrap_or_default()
        ));
    }
    if let Some(hook) = &graphify.hook {
        question.push_str(&format!(
            " (Graphify's hook in {} stays: remove it yourself.)",
            shown(hook, Some(folder), env)
        ));
    }
    question
}

/// [`unix`]'s text alone.
fn setup_unix(text: &str) -> String {
    unix(text).0
}

/// Step 6: what of Graphify was found, and the switch: its MCP entries
/// removed (the clients got Graphicle's in step 2), each recorded so that
/// `graphicle disconnect` puts it back.
fn graphify_step(
    data: &Path,
    owner: (&Path, &str),
    graphify: &agent_detect::Graphify,
    (switch, connected): (bool, &[&str]),
    options: &Options,
) -> Result<Value> {
    if !graphify.found() {
        return Ok(Value::Null);
    }
    let mut report = json!({"found":graphify,"switched":switch});
    let mut kept = Vec::new();
    if let Some(output) = &graphify.output {
        kept.push(json!(output));
    }
    if let Some(skill) = &graphify.skill {
        kept.push(json!(skill));
    }
    report["kept"] = json!(kept);
    let by_hand: Vec<Value> = graphify
        .entries
        .iter()
        .filter_map(|entry| entry.by_hand.as_ref().map(|command| json!(command)))
        .collect();
    if !by_hand.is_empty() {
        report["by_hand"] = json!(by_hand);
    }
    if let Some(hook) = &graphify.hook {
        report["hook"] = json!({"file":hook,
            "by_hand":"Graphify's PreToolUse hook in this file still points Claude at graphify-out/ before every search; remove it from the file's hooks when you no longer use Graphify"});
    }
    if !switch {
        if graphify.switchable() {
            report["next"] = json!(
                "graphicle setup --switch-from-graphify replaces Graphify's MCP entries and CLAUDE.md or AGENTS.md section with Graphicle's, keeping backups"
            );
        }
        return Ok(report);
    }
    let mut changed = Vec::new();
    for entry in graphify
        .entries
        .iter()
        .filter(|entry| entry.by_hand.is_none())
    {
        if !connected.contains(&entry.client) {
            changed.push(json!({"file":entry.file,"entry":entry.name,"client":entry.client,
                "state":"skipped","reason":"Graphicle's connection failed, so Graphify's entry stays"}));
            continue;
        }
        let state = match &entry.group {
            Some(group) => remove_json_entry(data, owner, entry, group)?,
            None => remove_codex_entry(data, owner, entry)?,
        };
        changed.push(
            json!({"file":entry.file,"entry":entry.name,"client":entry.client,"state":state}),
        );
    }
    if options.no_instructions && !graphify.sections.is_empty() {
        report["note"] =
            json!("--no-instructions: Graphify's CLAUDE.md or AGENTS.md section was left");
    }
    report["changed"] = json!(changed);
    Ok(report)
}

/// Remove Graphify's entry from a JSON client file, recording it.
fn remove_json_entry(
    data: &Path,
    (folder, project): (&Path, &str),
    entry: &agent_detect::GraphifyEntry,
    group: &str,
) -> Result<&'static str> {
    let Some(mut document) = agent_detect::json_file(&entry.file) else {
        return Ok("skipped");
    };
    let Some(value) = document[group]
        .as_object_mut()
        .and_then(|servers| servers.shift_remove(&entry.name))
    else {
        return Ok("skipped");
    };
    // A group it was alone in goes with it.
    if document[group]
        .as_object()
        .is_some_and(serde_json::Map::is_empty)
        && let Some(object) = document.as_object_mut()
    {
        object.shift_remove(group);
    }
    // Only Graphify's entry leaves the file's text.
    let original = std::fs::read_to_string(&entry.file).ok();
    let edited = original
        .as_deref()
        .and_then(|text| crate::json_edit::remove(text, group, &entry.name));
    agent_files::backup_in(data, &entry.file)?;
    agent_files::write_json(&entry.file, original.as_deref(), edited, &document)?;
    agent_files::record(
        data,
        Change {
            path: entry.file.clone(),
            kind: "removed-mcp".into(),
            client: Some(entry.client.into()),
            folder: Some(folder.to_path_buf()),
            project: Some(project.into()),
            entry: Some(entry.name.clone()),
            group: Some(group.into()),
            replaced: Some(json!({"value":value})),
            ..Change::default()
        },
    )?;
    Ok("removed")
}

/// Remove Graphify's table from Codex's `config.toml`, recording it.
fn remove_codex_entry(
    data: &Path,
    (folder, project): (&Path, &str),
    entry: &agent_detect::GraphifyEntry,
) -> Result<&'static str> {
    let Ok(raw) = std::fs::read_to_string(&entry.file) else {
        return Ok("skipped");
    };
    let (text, crlf) = unix(&raw);
    let Some((new, removed, at)) = codex_config::remove(&text, &entry.name) else {
        return Ok("skipped");
    };
    agent_files::backup_in(data, &entry.file)?;
    agent_files::write(&entry.file, native(new, crlf).as_bytes(), 0o600)?;
    agent_files::record(
        data,
        Change {
            path: entry.file.clone(),
            kind: "removed-codex".into(),
            client: Some("codex".into()),
            folder: Some(folder.to_path_buf()),
            project: Some(project.into()),
            entry: Some(entry.name.clone()),
            replaced: Some(json!({"text":removed,"line":at})),
            ..Change::default()
        },
    )?;
    Ok("removed")
}

/// The summary's freshness (ux-fresh): the project's folder against its
/// last sync, read only, and whether answers sync changed files first.
fn freshness_report(
    store: &Store,
    session: &local::Session,
    repository: &str,
    config: &model::Config,
) -> Value {
    let mode = crate::fresh::mode(config);
    match crate::fresh::check(store, session, repository) {
        Ok(check) => {
            let mut report = check.to_json();
            report["mode"] = json!(mode);
            report
        }
        Err(error) => json!({"state":"unknown","error":error.message,"mode":mode}),
    }
}

/// Step 7: the answer model. The one configured is kept. Else the best
/// model this environment has ([`terminal::detect_models`]: a key's
/// variable, or a local Ollama) is offered on a terminal and taken with
/// `--yes`, written by `graphicle config set model`'s own writer
/// ([`terminal::set_answer_model`]): the configuration names the key's
/// variable, never the key. Else the one command that adds a model. No
/// model is called.
fn model_step(
    store: &Store,
    session: &local::Session,
    config: &model::Config,
    options: &Options,
    ui: &Ui,
) -> Value {
    let configured = terminal::model_state(Some((store, session)), config);
    let detected = terminal::detect_models();
    let report = setup_model_step(configured.as_ref(), &detected);
    let Some(best) = detected.first().filter(|_| report["state"] == "found") else {
        return report;
    };
    let question = format!(
        "Answer `graphicle ask` questions with {} ({}; {})?",
        provider_name(best.provider),
        best.model,
        best.found
    );
    if !(options.yes || ui.confirm(&question, true).unwrap_or(false)) {
        return report;
    }
    let path = terminal::config_path(options.config.as_deref(), &store.directory);
    let written =
        terminal::set_answer_model(&path, best.provider, Some(&best.model), best.key_env, None)
            .and_then(|_| model::Config::load(Some(path.as_path())));
    match written {
        Ok(config) => {
            let state = terminal::model_state(Some((store, session)), &config);
            json!({"state":"set","model":state.as_ref().map(|m| m.name.clone()),
                "provider":best.provider,"key_env":best.key_env,"file":path,
                "line":terminal::model_line(state.as_ref()),
                "hint":"graphicle config set model none turns it off"})
        }
        Err(error) => {
            let mut report = report;
            report["error"] = json!(error.message);
            report
        }
    }
}

/// A provider's name for people.
fn provider_name(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "Claude",
        "openai" => "OpenAI",
        "ollama" => "Ollama",
        _ => "the model",
    }
}

/// Step 7's report before any offer: the answer model `configured`, else
/// the models `detected` in this environment (best first) with the one
/// command that adds the first, else the command that adds one. Variable
/// names only: no key is ever read, printed or written.
pub fn setup_model_step(
    configured: Option<&terminal::ModelState>,
    detected: &[terminal::DetectedModel],
) -> Value {
    if let Some(model) = configured {
        let hint = match (&model.key_env, model.key_available) {
            (Some(variable), false) => format!(
                "{variable} is not set in this shell: graphicle ask shows the evidence without a written answer until it is"
            ),
            _ => "graphicle ask answers with this model".into(),
        };
        return json!({"state":"configured","model":model.name,"protocol":model.protocol,
            "key_env":model.key_env,"key_present":model.key_available,
            "line":terminal::model_line(Some(model)),"hint":hint});
    }
    match detected.first() {
        Some(best) => json!({"state":"found","found":detected,"command":best.command(),
            "hint":format!("{} makes it the answer model; the configuration records the variable's name, never the key", best.command())}),
        None => json!({"state":"none","command":"graphicle config set model anthropic",
            "hint":"export ANTHROPIC_API_KEY (or OPENAI_API_KEY), then run graphicle config set model anthropic (or openai, or ollama)"}),
    }
}

/// The text summary of a setup report.
pub fn render_setup(report: &Value) -> String {
    let env = Environment::current();
    let folder = report["project"]["folder"].as_str().map(PathBuf::from);
    let folder = folder.as_deref();
    let show = |value: &Value| -> String {
        value
            .as_str()
            .map(|path| shown(Path::new(path), folder, &env))
            .unwrap_or_default()
    };
    let project = &report["project"];
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line(format!(
        "Graphicle is set up for {} ({}).",
        project["name"].as_str().unwrap_or("this project"),
        crate::terminal::tilde(Path::new(project["folder"].as_str().unwrap_or_default()))
    ));
    line(String::new());
    let graph = match project["graph"]["status"].as_str() {
        Some("built" | "current") => "code graph ready".to_string(),
        Some(other) => format!("code graph {other}"),
        None => "no code graph".to_string(),
    };
    let symbols = project["symbols"]
        .as_u64()
        .map(|count| {
            format!(
                ", {}",
                if count == 1 {
                    "1 symbol".to_string()
                } else {
                    format!("{count} symbols")
                }
            )
        })
        .unwrap_or_default();
    line(format!(
        "  Project       {} files{symbols}, {graph} ({} s)",
        project["files"], project["seconds"]
    ));
    if let Some(note) = project["note"].as_str() {
        line(format!("                {note}"));
    }
    if let Some(note) = crate::terminal::limits_summary(&project["sync"]) {
        line(format!("                {note}"));
    }
    if let Some(note) = project["sync"]["question_limits"].as_str() {
        line(format!("                {note}"));
    }
    // Questions read every file until the passage index is built.
    let questions = &project["questions"];
    if !matches!(questions["state"].as_str(), None | Some("ready" | "small")) {
        let fix = questions["fix"].as_str().filter(|fix| !fix.is_empty());
        line(format!(
            "                {}{}",
            s(questions, "detail"),
            fix.map(|fix| format!(" (run {fix})")).unwrap_or_default()
        ));
    }
    let clients = report["clients"].as_array().cloned().unwrap_or_default();
    if clients.is_empty() {
        line(format!(
            "  Agents        none connected{}",
            report["clients_note"]
                .as_str()
                .map(|note| format!(": {note}"))
                .unwrap_or_default()
        ));
    }
    for (index, client) in clients.iter().enumerate() {
        let label = if index == 0 { "Agents" } else { "" };
        let detail = match client["state"].as_str() {
            Some("failed") => format!("failed: {}", client["error"].as_str().unwrap_or_default()),
            Some(state) => format!("{state}: {}", show(&client["file"])),
            None => String::new(),
        };
        line(format!(
            "  {label:<13} {:<15} {detail}",
            client["name"].as_str().unwrap_or_default()
        ));
    }
    // Each entry runs the one MCP server; Claude Code's, its five
    // essential tools unless --tools says otherwise.
    let connected: Vec<&Value> = clients
        .iter()
        .filter(|client| client["state"] != "failed")
        .collect();
    let essentials: Vec<&str> = connected
        .iter()
        .filter(|client| client["tools"] == "essentials")
        .filter_map(|client| client["name"].as_str())
        .collect();
    if !connected.is_empty() && essentials.len() == connected.len() {
        line("                one MCP server, five of its tools: query_graph, get_node, get_neighbors, recall_project, read_project_source (--tools all for every tool)".into());
    } else if !connected.is_empty() {
        line("                one MCP server each: the code graph, recall and memory; memory drafts wait for your review".into());
        if !essentials.is_empty() {
            line(format!(
                "                {} gets five of its tools (--tools all for every tool)",
                essentials.join(" and ")
            ));
        }
    }
    let capture = &report["capture"];
    match capture["state"].as_str() {
        Some("on") => {
            for (index, client) in capture["clients"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let label = if index == 0 { "Capture" } else { "" };
                line(format!(
                    "  {label:<13} {:<15} {}: {}",
                    client["name"].as_str().unwrap_or_default(),
                    client["state"].as_str().unwrap_or_default(),
                    show(&client["file"])
                ));
            }
            line("                Finished turns become project sources; reviewed memory reaches the agent at session start (--no-capture skips this)".into());
            if capture["clients"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|client| client["client"] == "codex" && client["state"] == "added")
            {
                line("                Codex asks you to review and trust the new hooks before they run".into());
            }
        }
        _ => line(format!(
            "  Capture       off: {}",
            capture["reason"].as_str().unwrap_or_default()
        )),
    }
    // Claude Code's guard hooks.
    let guarded: Vec<&Value> = clients
        .iter()
        .filter(|client| client.get("guard").is_some())
        .collect();
    for (index, client) in guarded.iter().enumerate() {
        let label = if index == 0 { "Graph guard" } else { "" };
        line(format!(
            "  {label:<13} {:<15} {}: {}",
            client["name"].as_str().unwrap_or_default(),
            client["guard"]["state"].as_str().unwrap_or_default(),
            show(&client["guard"]["file"])
        ));
    }
    if guarded
        .iter()
        .any(|client| client["guard"]["state"] != "removed")
    {
        line("                Before a search or a source read here, Claude Code is pointed to the graph tools".into());
    }
    if guarded
        .iter()
        .any(|client| client["guard"]["state"] == "removed")
    {
        line("                The guard hooks are off unless asked for: graphicle setup --guards turns them on".into());
    }
    // Claude Code's prompt-context hook (`--prompt-context`).
    let prompt = &report["prompt_context"];
    if let Some(state) = prompt["state"].as_str() {
        let detail = match state {
            "added" | "already set" => show(&prompt["file"]),
            _ => prompt["reason"]
                .as_str()
                .or(prompt["error"].as_str())
                .unwrap_or_default()
                .to_string(),
        };
        line(format!("  Prompt hook   {state}: {detail}"));
        if matches!(state, "added" | "already set") {
            line("                Before each prompt that is a code question, Claude Code gets graphicle context's packet".into());
        }
    }
    for (index, written) in report["instructions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let label = if index == 0 { "Instructions" } else { "" };
        let state = written["state"].as_str().unwrap_or_default();
        let detail = match (written["reason"].as_str(), written.get("replaced")) {
            (Some(reason), _) => format!("{state}: {reason}"),
            (None, Some(_)) => match written["lines"].as_array() {
                Some(lines) if lines.len() == 2 => format!(
                    "replaced the graphify section (its lines {}-{} are in the backup)",
                    lines[0], lines[1]
                ),
                _ => "replaced the graphify section".to_string(),
            },
            (None, None) => state.to_string(),
        };
        let file = if written["file"].is_null() {
            String::new()
        } else {
            format!("{} ", show(&written["file"]))
        };
        line(format!("  {label:<13} {file}{detail}"));
    }
    let git = &report["git_hooks"];
    match git["hooks"].as_array() {
        Some(hooks) => {
            let names: Vec<String> = hooks
                .iter()
                .map(|hook| {
                    format!(
                        "{} {}",
                        hook["hook"].as_str().unwrap_or_default(),
                        hook["state"].as_str().unwrap_or_default()
                    )
                })
                .collect();
            line(format!(
                "  Git hooks     {} ({})",
                names.join(", "),
                show(&git["directory"])
            ));
            for hook in hooks {
                if let Some(text) = hook["reason"].as_str().or(hook["note"].as_str()) {
                    line(format!("                {text}"));
                }
            }
        }
        None => {
            line(format!(
                "  Git hooks     skipped: {}",
                git["reason"].as_str().unwrap_or_default()
            ));
            if let Some(by_hand) = git["by_hand"].as_str() {
                line(format!("                {by_hand}"));
            }
        }
    }
    let graphify = &report["graphify"];
    if !graphify.is_null() {
        if graphify["switched"] == true {
            line("  Graphify      switched to Graphicle; graphify-out/ and its skill stay (delete them yourself when you no longer need them)".into());
        } else {
            line("  Graphify      found; left as it is".into());
            if let Some(next) = graphify["next"].as_str() {
                line(format!("                {next}"));
            }
        }
        for command in graphify["by_hand"].as_array().into_iter().flatten() {
            line(format!(
                "                Claude Code's own graphify server is in ~/.claude.json, which setup does not edit: run {}",
                command.as_str().unwrap_or_default()
            ));
        }
        if let Some(file) = graphify["hook"]["file"].as_str() {
            line(format!(
                "                Graphify's hook in {} still points Claude at graphify-out/: remove it from that file's hooks",
                show(&json!(file))
            ));
        }
    }
    let freshness = &report["freshness"];
    if freshness.is_object() {
        let words = if freshness["mode"] == "off" {
            "off: answers use the last sync (graphicle config set freshness auto turns it on)"
                .to_string()
        } else {
            match (freshness["state"].as_str(), freshness["changed"].as_u64()) {
                (Some("fresh"), _) => {
                    "up to date; files that change are synced before each answer".into()
                }
                (Some("changed"), Some(n)) => {
                    format!("{n} files changed since the sync: graphicle sync")
                }
                (Some("changed"), None) => "files changed since the sync: graphicle sync".into(),
                (Some("missing"), _) => "the folder is gone".into(),
                _ => "unknown".into(),
            }
        };
        line(format!("  Freshness     {words}"));
    }
    let model = &report["model"];
    match model["state"].as_str() {
        Some("configured") => {
            line(format!(
                "  Model         {}",
                model["line"].as_str().unwrap_or_default()
            ));
            if model["key_present"] == false {
                line(format!(
                    "                {}",
                    model["hint"].as_str().unwrap_or_default()
                ));
            }
        }
        Some("set") => line(format!(
            "  Model         {} (set now; {})",
            model["line"].as_str().unwrap_or_default(),
            model["hint"].as_str().unwrap_or_default()
        )),
        Some("found") => {
            line(format!(
                "  Model         none yet: {}",
                model["hint"].as_str().unwrap_or_default()
            ));
            if let Some(error) = model["error"].as_str() {
                line(format!("                not set: {error}"));
            }
        }
        _ => {
            line(
                "  Model         none: graphicle ask shows the evidence without a written answer"
                    .into(),
            );
            line(format!(
                "                to add one: {}",
                model["hint"].as_str().unwrap_or_default()
            ));
        }
    }
    line(String::new());
    let files = report["files_changed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if files.is_empty() {
        line("Nothing changed outside the data folder: everything was already set.".into());
    } else {
        line("Files changed outside the data folder (a file that existed is backed up once as <file>.graphicle-bak; graphicle disconnect undoes them):".into());
        for file in &files {
            line(format!("  {}", show(file)));
        }
    }
    line(String::new());
    line("Try:".into());
    for next in report["next"].as_array().into_iter().flatten() {
        line(format!("  {}", next.as_str().unwrap_or_default()));
    }
    if clients
        .iter()
        .any(|client| client["state"] != "already set" && client["state"] != "failed")
    {
        line("Restart or reload your agent clients to load Graphicle.".into());
    }
    if clients
        .iter()
        .any(|client| client["client"] == "claude-code" && client["state"] == "added")
    {
        line("Claude Code asks once to approve the graphicle server in .mcp.json.".into());
    }
    out
}

/// `graphicle connect` without a client: the clients found here, every
/// one connected (asked on a terminal; `--all` never asks).
#[derive(Clone, Debug, Default)]
pub struct ConnectFound {
    pub all: bool,
    pub project: Option<String>,
    pub replace: bool,
    pub hooks: bool,
    pub work_state: bool,
    pub json: bool,
    /// `--guards`, as for setup.
    pub guards: bool,
    /// `--tools`, as for setup.
    pub tools: Option<ToolSet>,
}

pub fn connect_found(store: &Store, options: &ConnectFound) -> Result<()> {
    let ui = Ui::new(options.json, options.all);
    let env = Environment::current();
    let cwd = std::env::current_dir()?;
    let detected = agent_detect::detect(&env);
    if detected.is_empty() {
        return Err(Error::bad(
            "No supported agent client was found (Claude Code, Codex, Cursor, VS Code, Claude Desktop); name one: graphicle connect claude-code",
        ));
    }
    let chosen = if ui.interactive {
        let setup_options = Options::default();
        let (chosen, _) = choose_clients(&setup_options, &detected, &ui)?;
        chosen
    } else {
        detected.iter().map(|found| found.client).collect()
    };
    let mut connected = Vec::new();
    let mut failed = Vec::new();
    for client in &chosen {
        let mut connection = agent_setup::default_connection(
            client,
            &cwd,
            options.hooks,
            options.guards,
            options.tools,
        );
        connection.project = options.project.as_deref();
        connection.replace = options.replace;
        connection.work_state = options.work_state;
        match agent_setup::connect_with(
            store,
            &connection,
            agent_setup::Policy {
                refresh_ours: true,
                drop_guards: true,
            },
        ) {
            Ok(result) => connected.push(result),
            Err(error) => failed.push(json!({"client":client,"error":error.message})),
        }
    }
    let report = json!({"detected":detected,"connected":connected,"failed":failed,
        "next":"Restart or reload your clients to enable Graphicle"});
    ui.print(&report, |report| render_connected(report, &cwd))?;
    if connected.is_empty()
        && let Some(first) = failed.first()
    {
        return Err(Error::bad(format!(
            "No client was connected: {}",
            first["error"].as_str().unwrap_or_default()
        )));
    }
    Ok(())
}

/// The text of one or more `connect` replies.
fn render_connected(report: &Value, cwd: &Path) -> String {
    let env = Environment::current();
    let mut out = String::new();
    if report["connected"].as_array().is_none_or(Vec::is_empty)
        && report["failed"].as_array().is_none_or(Vec::is_empty)
    {
        out.push_str("No client was connected.\n");
    }
    for result in report["connected"].as_array().into_iter().flatten() {
        out.push_str(&render_connect(result, cwd, &env));
    }
    for failed in report["failed"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "{}: not connected: {}\n",
            agent_detect::name(failed["client"].as_str().unwrap_or_default()),
            failed["error"].as_str().unwrap_or_default()
        ));
    }
    if report["connected"]
        .as_array()
        .is_some_and(|connected| !connected.is_empty())
    {
        out.push_str("Restart or reload your clients to enable Graphicle.\n");
    }
    out
}

/// The text of one `connect` reply.
fn render_connect(result: &Value, cwd: &Path, env: &Environment) -> String {
    let client = result["client"].as_str().unwrap_or_default();
    let state = if result["unchanged"] == true {
        "already connected"
    } else if result["updated"] == true {
        "connection updated"
    } else {
        "connected"
    };
    let file = result["configuration"]
        .as_str()
        .map(|file| format!(" ({})", shown(Path::new(file), Some(cwd), env)))
        .unwrap_or_default();
    let mut out = format!(
        "{}: {state} to {} as {}{file}\n",
        agent_detect::name(client),
        result["project"]["name"].as_str().unwrap_or("the project"),
        result["entry"].as_str().unwrap_or_default()
    );
    if let Some(hooks) = result.get("hooks") {
        out.push_str(&format!(
            "  capture hooks: {} ({})\n",
            if hooks["installed"].as_array().is_some_and(|i| !i.is_empty()) {
                "added"
            } else {
                "already set"
            },
            hooks["configuration"]
                .as_str()
                .map(|file| shown(Path::new(file), Some(cwd), env))
                .unwrap_or_default()
        ));
    }
    if let Some(guard) = result["graph"]["guard_hooks"].as_object() {
        out.push_str(&format!(
            "  graph guard hooks: {} ({})\n",
            guard_state(guard),
            guard
                .get("configuration")
                .and_then(Value::as_str)
                .map(|file| shown(Path::new(file), Some(cwd), env))
                .unwrap_or_default()
        ));
    }
    if result["tools"] == "essentials" {
        out.push_str("  tools: query_graph, get_node, get_neighbors, recall_project, read_project_source (--tools all for every tool)\n");
    }
    if let Some(note) = result["note"].as_str() {
        out.push_str(&format!("  {note}\n"));
    }
    out
}

/// Print one `connect <client>` reply: text on a terminal, else the JSON.
pub fn print_connect(result: &Value, json_flag: bool) -> Result<()> {
    let ui = Ui::new(json_flag, true);
    let cwd = std::env::current_dir()?;
    ui.print(result, |result| {
        let mut text = render_connect(result, &cwd, &Environment::current());
        if result["unchanged"] != true {
            text.push_str("Restart or reload the client to enable Graphicle.\n");
        }
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(client: &'static str) -> Found {
        Found {
            client,
            name: agent_detect::name(client),
            evidence: PathBuf::from("/x"),
        }
    }

    #[test]
    fn client_lists_name_clients_or_found_numbers() {
        let detected = [found("claude-code"), found("cursor")];
        assert_eq!(
            parse_clients("all", &detected).unwrap(),
            ["claude-code", "cursor"]
        );
        assert!(parse_clients("none", &detected).unwrap().is_empty());
        assert_eq!(
            parse_clients("2, codex claude-desktop", &detected).unwrap(),
            ["cursor", "codex", "claude"]
        );
        assert!(parse_clients("windsurf", &detected).is_err());
        assert!(parse_clients("3", &detected).is_err());
    }

    #[test]
    fn non_interactive_setup_connects_only_when_told() {
        let ui = Ui {
            text: false,
            interactive: false,
        };
        let detected = [found("claude-code")];
        let (chosen, note) = choose_clients(&Options::default(), &detected, &ui).unwrap();
        assert!(chosen.is_empty());
        assert!(note.unwrap().contains("graphicle setup --yes"));
        let yes = Options {
            yes: true,
            ..Options::default()
        };
        assert_eq!(
            choose_clients(&yes, &detected, &ui).unwrap().0,
            ["claude-code"]
        );
        let listed = Options {
            clients: Some("cursor".into()),
            ..Options::default()
        };
        assert_eq!(
            choose_clients(&listed, &detected, &ui).unwrap().0,
            ["cursor"]
        );
    }

    #[test]
    fn the_model_step_names_variables_and_never_keys() {
        let none = setup_model_step(None, &[]);
        assert_eq!(none["state"], "none");
        assert_eq!(none["command"], "graphicle config set model anthropic");
        let detected = terminal::detect_models_with(
            &|name| (name == "OPENAI_API_KEY").then(|| "sk-secret".into()),
            None,
        );
        let found = setup_model_step(None, &detected);
        assert_eq!(found["state"], "found");
        assert_eq!(found["command"], "graphicle config set model openai");
        assert_eq!(found["found"][0]["key_env"], "OPENAI_API_KEY");
        assert!(!found.to_string().contains("sk-secret"));
        let configured = terminal::ModelState {
            name: "api".into(),
            protocol: "anthropic".into(),
            model: "m".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            key_env: Some("ANTHROPIC_API_KEY".into()),
            key_available: false,
            input_limit: 1000,
            output_limit: 1000,
        };
        let configured = setup_model_step(Some(&configured), &detected);
        assert_eq!(configured["state"], "configured");
        assert_eq!(configured["key_present"], false);
        assert!(
            configured["hint"]
                .as_str()
                .unwrap()
                .starts_with("ANTHROPIC_API_KEY is not set")
        );
    }
}
