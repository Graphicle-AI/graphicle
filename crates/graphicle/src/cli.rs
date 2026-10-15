//! The `graphicle` command line, shared by every edition: a program's `main`
//! sets its allocator and calls [`main_with`] with its extensions (Core:
//! [`Extensions::none`]). Extensions add subcommands through
//! `Extension::cli` and run them through `Extension::run_cli`, which gets
//! the parsed command line of those subcommands before the core would.
use crate::{
    Error, Result,
    api::{self, App},
    extension::{CliContext, Extensions, ServeContext},
    jobs::Worker,
    local, model,
    store::{Store, s},
};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io::IsTerminal,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};
use tokio::io::AsyncWriteExt;

#[derive(Parser)]
#[command(
    name = "graphicle",
    version,
    about = crate::terminal::ABOUT,
    // `graphicle help [--all] [COMMAND]` is the core's own command.
    disable_help_subcommand = true
)]
struct Cli {
    /// The data folder (default: GRAPHICLE_DATA_DIR, else the platform's).
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// The model configuration (default: models.json in the data folder).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Set up this repository for you and your agents in one step.
    ///
    /// Syncs the folder and builds its code graph, connects the agent
    /// clients found on this machine to the one MCP server (the code graph,
    /// recall, and memory drafts for review; Claude Code gets its five
    /// essential tools), with session capture for Claude Code and Codex,
    /// adds a two-line Graphicle section to AGENTS.md (and CLAUDE.md when it
    /// exists) unless Claude Code is the only agent, and adds Git hooks that
    /// keep the project fresh. Every file it changes outside the data folder
    /// is listed, backed up once as <file>.graphicle-bak, and undone by
    /// `graphicle disconnect`. Running it again is safe.
    Setup {
        /// The folder to set up (default: this folder).
        path: Option<PathBuf>,
        /// Answer yes: connect every client found, create AGENTS.md when it
        /// is missing, and answer with a model found in this environment.
        #[arg(long, short = 'y')]
        yes: bool,
        /// The clients to connect: all, none, or a list such as
        /// claude-code,cursor (claude-code, codex, cursor, vscode, claude).
        #[arg(long, value_name = "LIST")]
        clients: Option<String>,
        /// No session-capture hooks for Claude Code and Codex.
        #[arg(long)]
        no_capture: bool,
        /// No Git hooks.
        #[arg(long)]
        no_hooks: bool,
        /// No Graphicle section in AGENTS.md or CLAUDE.md.
        #[arg(long)]
        no_instructions: bool,
        /// Replace Graphify's MCP entries and its CLAUDE.md or AGENTS.md
        /// section with Graphicle's (asked on a terminal).
        #[arg(long)]
        switch_from_graphify: bool,
        /// Also give Claude Code guard hooks that point its searches and
        /// source reads here to the graph tools. Without it, setup removes
        /// the guard hooks an earlier setup installed.
        #[arg(long)]
        guards: bool,
        /// The tools each client's entry publishes: all (every tool, as in
        /// 0.1.0), essentials (query_graph, get_node, get_neighbors,
        /// recall_project, read_project_source), graph or memory. Default:
        /// essentials for Claude Code, all for the other clients.
        #[arg(long, value_name = "SET", value_parser = ["all", "essentials", "graph", "memory"])]
        tools: Option<String>,
        /// Print the JSON report.
        #[arg(long)]
        json: bool,
    },
    /// Connect a coding client to this local project's MCP server: its
    /// sources, code graph and reviewed memory, in one server.
    ///
    /// The project is this folder's (or --project). Claude Code and VS Code
    /// get an entry in this folder that finds the project from the folder it
    /// starts in; Claude Desktop, Cursor and Codex get an entry named after
    /// the project (`graphicle-<name>`) that names it. Without a client,
    /// every client found on this machine is connected (asked on a
    /// terminal; --all does not ask) and set up for the code graph, as with
    /// --graph.
    Connect {
        /// claude-code, codex, cursor, vscode or claude (Claude Desktop).
        client: Option<String>,
        /// Connect every client found, without asking.
        #[arg(long, conflicts_with_all = ["client", "config_path", "name"])]
        all: bool,
        /// Print the JSON reply (the default when the output is not a
        /// terminal).
        #[arg(long)]
        json: bool,
        #[arg(long)]
        config_path: Option<PathBuf>,
        /// The project: an id, a key or a synced folder (default: this
        /// folder's project, else the last synced one).
        #[arg(long)]
        project: Option<String>,
        /// The entry's name in the client's configuration.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        replace: bool,
        /// Also install session-capture hooks (claude-code: this folder's
        /// .claude/settings.local.json; codex: the user's hooks.json). They
        /// import completed turns after each answer and add reviewed memory
        /// at session start; they never block the client.
        #[arg(long)]
        hooks: bool,
        /// Also give the client the work-state tools (`work_state_write`,
        /// `work_state_read`) for its own to-do lists. Open items appear,
        /// labelled unreviewed, in the session-start context.
        #[arg(long)]
        work_state: bool,
        /// Set the agent up for the code graph as well, as setup does: an
        /// entry with every tool lets it propose memory drafts for review
        /// (as --allow-learning).
        #[arg(long)]
        graph: bool,
        /// Let the agent propose memory drafts, which a member reviews
        /// (`propose_memory` and the other learning tools).
        #[arg(long)]
        allow_learning: bool,
        /// Claude Code: guard hooks (this folder's
        /// .claude/settings.local.json) that point its searches and source
        /// reads in a synced folder to the graph and recall tools.
        #[arg(long)]
        guards: bool,
        /// The tools the entry publishes: all, essentials (query_graph,
        /// get_node, get_neighbors, recall_project, read_project_source),
        /// graph or memory. Default: essentials for Claude Code, all for the
        /// other clients.
        #[arg(long, value_name = "SET", value_parser = ["all", "essentials", "graph", "memory"])]
        tools: Option<String>,
    },
    /// Remove what Graphicle added for this folder's project: client
    /// entries, capture and guard hooks, the AGENTS.md and CLAUDE.md section
    /// and the Git-hook lines. A backup is put back when the file is then the
    /// same.
    Disconnect {
        /// Only this client's entry and hooks.
        client: Option<String>,
        /// Everything Graphicle added, for every project.
        #[arg(long, conflicts_with = "client")]
        all: bool,
        /// Print the JSON report.
        #[arg(long)]
        json: bool,
    },
    #[command(hide = true)]
    DocumentWorker { path: PathBuf },
    /// Create a workspace for a team server; its access keys are saved
    /// locally.
    ///
    /// To set up a repository on this computer, run `graphicle setup`
    /// instead: it creates the local workspace itself.
    Init {
        #[arg(long, default_value = "My workspace")]
        name: String,
    },
    /// Open the app in your browser, reusing a running Graphicle server.
    ///
    /// With PATH, sync that folder first. The address is printed when no
    /// browser can be started (an SSH session, no display, or
    /// GRAPHICLE_NO_BROWSER=1).
    #[command(visible_alias = "start")]
    Open {
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 8854)]
        port: u16,
    },
    /// Run the native HTTP app and persistent workflow worker.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        listen: IpAddr,
        #[arg(long, default_value_t = 8854)]
        port: u16,
        #[arg(long)]
        open: bool,
        #[arg(long)]
        mcp_learning: bool,
        /// Publish the work-state tools on the HTTP MCP endpoint.
        #[arg(long)]
        mcp_work_state: bool,
        /// The tools the HTTP MCP endpoint publishes: all, graph (every tool
        /// that reads the code graph: the graph engine's tools and resources
        /// and the graph-history tools), memory (the other tools; recall
        /// still reads the code graph unless --mcp-set graph_retrieval=false)
        /// or essentials (query_graph, get_node, get_neighbors,
        /// recall_project, read_project_source).
        #[arg(long, default_value = "all", value_parser = ["all", "essentials", "graph", "memory"])]
        mcp_tools: String,
        /// Fix a switch of recall_project on the HTTP MCP endpoint, for
        /// example graph_retrieval=false: no request can change it. Each
        /// switch at most once.
        #[arg(long = "mcp-set", value_name = "SWITCH=VALUE")]
        mcp_set: Vec<String>,
        /// HTTPS origin of a reverse proxy. Public workspace creation is disabled.
        #[arg(long)]
        public_origin: Option<String>,
    },
    /// Import or update a project, respecting .gitignore and source limits,
    /// then build its code graph, with a line of progress on standard error.
    Sync {
        /// The folder to sync (default: the project this folder is in, else
        /// this folder).
        path: Option<PathBuf>,
        /// The project to sync the folder into: an id, a key or another of
        /// its folders (default: the project of this folder, else a new one).
        #[arg(long)]
        project: Option<String>,
        /// Build the code graph in a background process and return at once.
        #[arg(long, conflicts_with = "no_graph")]
        background: bool,
        /// Leave the code graph to be built later: `graphicle mcp`, `serve`,
        /// `ask` and the next sync build it.
        #[arg(long)]
        no_graph: bool,
        /// Print JSON (the default when the output is not a terminal).
        #[arg(long)]
        json: bool,
        /// Print nothing on success, and do nothing when the folder has not
        /// changed since its last sync (what the git hooks run).
        #[arg(long, conflicts_with = "json")]
        quiet: bool,
    },
    /// Build a project's code graph (the process `sync --background` starts).
    #[command(hide = true)]
    BuildGraph {
        #[arg(long)]
        project: String,
    },
    /// Attach a local workspace using a credential from the environment.
    Login {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        key_env: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Show this project's sync, code graph, agents, hooks and model.
    Status {
        /// The project: an id, a key or a synced folder (default: this
        /// folder's project, else the last synced one).
        #[arg(long)]
        project: Option<String>,
        /// Print the project's status as JSON (the default when the output
        /// is not a terminal).
        #[arg(long)]
        json: bool,
    },
    /// Check the installation, this project and its agents; print the
    /// command that fixes each problem.
    Doctor {
        /// Apply the safe fixes: re-sync this project (which rebuilds its
        /// code graph) and rewrite agent entries whose program is missing.
        #[arg(long)]
        fix: bool,
        /// The project: an id, a key or a synced folder (default: this
        /// folder's project, else the last synced one).
        #[arg(long)]
        project: Option<String>,
        /// The app's port to check.
        #[arg(long, default_value_t = 8854)]
        port: u16,
        /// Print JSON (the default when the output is not a terminal).
        #[arg(long)]
        json: bool,
    },
    /// Retrieve evidence without calling a model.
    Context {
        question: String,
        /// The project: an id, a key or a synced folder (default: this
        /// folder's project, else the last synced one).
        #[arg(long)]
        project: Option<String>,
        /// Search every project of the workspace you can read (at most 8):
        /// this folder's project (or --project) first, then the others.
        #[arg(long)]
        workspace: bool,
        #[arg(long, default_value_t = 3000)]
        max_tokens: usize,
        /// Configured local ONNX embedding profile; requires a prepared vector index.
        #[arg(long)]
        local_embedding_profile: Option<String>,
        /// Configured local ONNX cross-encoder; reranks at most 32 fused sources.
        /// Not a default: on one conversation's history it did not raise
        /// held-out recall.
        #[arg(long)]
        local_reranker_profile: Option<String>,
        /// Print the packet as JSON (the default when the output is not a
        /// terminal).
        #[arg(long)]
        json: bool,
    },
    /// Answer from the project's original sources with the configured model.
    ///
    /// By default a `basic` job: one model call reads the retrieved excerpts
    /// under the grounded-v2 answer contract and cites exact quotes.
    Ask {
        question: String,
        /// The project: an id, a key or a synced folder (default: this
        /// folder's project, else the last synced one).
        #[arg(long)]
        project: Option<String>,
        /// The answering workflow: `basic`, or `reflect`, whose model calls
        /// search and read further sources, then check the answer against
        /// them.
        #[arg(long, default_value = "basic", value_parser = ["basic", "reflect"])]
        workflow: String,
        /// Save the answer as a memory draft for review (`--workflow reflect`).
        #[arg(long)]
        retain: bool,
        /// Most model calls the job may make.
        #[arg(long, default_value_t = 4)]
        max_calls: usize,
        /// Print the job as JSON (the default when the output is not a
        /// terminal).
        #[arg(long)]
        json: bool,
    },
    /// Build, update and query code graphs with the graph engine
    /// (`graphicle graph --help` lists its commands).
    ///
    /// Runs the graph engine, `graphicle-graph`, with the arguments after
    /// `graph` unchanged: `graphicle graph update .` is `graphicle-graph
    /// update .`, with the engine's own input, output and exit status.
    /// `graphicle graph` alone prints the engine's help. The engine is found
    /// next to this program, else on PATH.
    #[command(disable_help_flag = true)]
    Graph {
        /// The engine's command and options.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Queue extraction, embeddings or source sync.
    Run {
        kind: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        arguments: Option<PathBuf>,
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 4)]
        max_calls: usize,
    },
    Jobs {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        job: Option<String>,
    },
    Worker {
        #[arg(long)]
        once: bool,
    },
    /// Run an inference consumer on another machine; canonical memory stays on the coordinator.
    RedisWorker {
        #[arg(long)]
        queue_config: PathBuf,
        #[arg(long)]
        model_profile: Option<String>,
        #[arg(long)]
        once: bool,
    },
    /// Serve a project over MCP on standard input and output: its sources,
    /// reviewed memory and code graph in one server (the graph engine's
    /// tools keep Graphify's names). No Python process is used.
    Mcp {
        /// The project: an id, a key or a synced folder (default: this
        /// folder's project, else the last synced one).
        #[arg(long, conflicts_with = "workspace")]
        project: Option<String>,
        /// Serve every project of the workspace you can read instead of one:
        /// `recall_project` searches them all unless its `repo` names one,
        /// and the other tools need `repo` (`list_projects` lists them).
        #[arg(long)]
        workspace: bool,
        #[arg(long)]
        allow_learning: bool,
        /// Publish `work_state_write` and `work_state_read`: the agent's own
        /// to-do lists, stored per project and shown at session start. Not
        /// with --workspace, which has no project to keep them in.
        #[arg(long, conflicts_with = "workspace")]
        work_state: bool,
        /// The tools to publish: all, graph (every tool that reads the code
        /// graph: the graph engine's tools and resources and the
        /// graph-history tools), memory (the other tools; recall still
        /// reads the code graph unless --set graph_retrieval=false) or
        /// essentials (query_graph, get_node, get_neighbors, recall_project
        /// and read_project_source, with one sentence of instructions: what
        /// setup gives Claude Code).
        #[arg(long, default_value = "all", value_parser = ["all", "essentials", "graph", "memory"])]
        tools: String,
        /// Fix a switch of recall_project for the session, for example
        /// graph_retrieval=false: no request can change it. Each switch at
        /// most once.
        #[arg(long = "set", value_name = "SWITCH=VALUE")]
        set: Vec<String>,
        /// Also publish the graph engine's pull-request tools (list_prs,
        /// get_pr_impact, triage_prs), which run gh here: start the server in
        /// the project's own folder. Not with --workspace, which has no
        /// project folder.
        #[arg(long, conflicts_with = "workspace")]
        pull_requests: bool,
    },
    /// Show the work-state lists agents recorded for a project, the most
    /// recently written first.
    WorkState {
        #[arg(long)]
        project: Option<String>,
        /// Only this list.
        #[arg(long)]
        list: Option<String>,
        /// Also show items and lists marked done or dropped.
        #[arg(long)]
        include_closed: bool,
        /// Print the JSON reply instead of the text.
        #[arg(long)]
        json: bool,
    },
    /// Capture Claude Code or Codex sessions as project sources: completed
    /// turns with `<private>` spans dropped, credentials redacted and large
    /// tool output replaced by its size and digest. No model is called.
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// Lifecycle-hook entry point (installed by `connect <client> --hooks`):
    /// reads the hook's JSON on stdin, never blocks the client, always exits 0.
    #[command(hide = true)]
    Hook { client: String, event: String },
    /// Call a project operation with arguments from a JSON file.
    Call {
        operation: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        arguments: Option<PathBuf>,
    },
    /// Create a consistent, private SQLite backup without stopping the app.
    Backup { path: PathBuf },
    /// Restore a database into a new data directory; existing data is never overwritten.
    Restore { path: PathBuf },
    /// Show or change settings, such as the answer model.
    ///
    /// `graphicle config set model anthropic` (or openai, or ollama) makes
    /// that provider the answer model; its key is read from the environment
    /// variable the configuration names, never written to the file.
    Config {
        #[command(subcommand)]
        action: Option<crate::terminal::ConfigAction>,
    },
    /// Show how to update Graphicle for the way it was installed, or check
    /// a signed publisher feed.
    ///
    /// Without --feed, nothing is downloaded and no network call is made.
    Update {
        #[arg(long, requires = "public_key")]
        feed: Option<String>,
        /// Base64 Ed25519 public key obtained independently from the publisher.
        #[arg(long, requires = "feed")]
        public_key: Option<String>,
        /// Download into this directory without installing or running the artifact.
        #[arg(long, requires = "feed")]
        output: Option<PathBuf>,
        /// Print JSON (the default when the output is not a terminal).
        #[arg(long)]
        json: bool,
    },
    /// Disconnect Graphicle everywhere, then say how to remove the program.
    Uninstall {
        /// Also delete the data folder, after confirmation.
        #[arg(long)]
        purge: bool,
        /// Confirm --purge without asking.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print the JSON report.
        #[arg(long)]
        json: bool,
    },
    /// Print a provider configuration example with no credentials embedded
    /// (also `graphicle config example`).
    #[command(hide = true)]
    ConfigExample,
    /// Show the commands; `--all` adds the advanced ones, and a command's
    /// name shows its options.
    #[command(hide = true)]
    Help {
        #[arg(long)]
        all: bool,
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SessionsAction {
    /// Import completed turns not captured yet. Without --path, the client's
    /// own transcript folder is scanned; without --project, each transcript
    /// goes to the synced project containing its working directory, and
    /// transcripts from other folders are skipped.
    Import {
        /// claude-code or codex.
        #[arg(long)]
        client: String,
        /// A transcript (.jsonl) or a folder of them.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Only transcripts modified within this many hours.
        #[arg(long)]
        since_hours: Option<u64>,
        #[arg(long)]
        project: Option<String>,
        /// Newest transcripts first, at most this many.
        #[arg(long, default_value_t = 2000)]
        limit: usize,
    },
    /// Forget every captured turn of a session with a hash-chained deletion
    /// receipt; its later turns are never captured.
    Forget {
        #[arg(long)]
        client: String,
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Captured turns around an anchor turn (the latest session by default).
    Timeline {
        #[arg(long)]
        session_id: Option<String>,
        /// Source id of a captured turn.
        #[arg(long)]
        anchor: Option<String>,
        #[arg(long, default_value_t = 3)]
        before: usize,
        #[arg(long, default_value_t = 3)]
        after: usize,
        #[arg(long)]
        project: Option<String>,
    },
    /// The context the session-start hook adds: reviewed memories and the
    /// latest captured turns (never drafts).
    Context {
        #[arg(long)]
        project: Option<String>,
    },
}
/// [`local::limits_note`]'s line on standard error, when a sync has one,
/// and the note on what one question reads of a larger project, when the
/// sync prints JSON (`json`, or not a terminal); its readable view lists
/// the same files and note ([`crate::terminal::sync_text`]).
fn note_limited(synced: &Value, json: bool) {
    if crate::terminal::human(json) {
        return;
    }
    if let Some(note) = local::limits_note(synced) {
        eprintln!("{note}");
    }
    if let Some(note) = synced["question_limits"].as_str() {
        eprintln!("graphicle: {note}");
    }
}
/// Print a JSON value, pretty, on standard output.
pub fn print(value: &Value) -> Result<()> {
    use std::io::Write;
    writeln!(
        std::io::stdout(),
        "{}",
        serde_json::to_string_pretty(value)?
    )?;
    Ok(())
}
fn arguments(path: Option<PathBuf>) -> Result<Value> {
    if let Some(path) = path {
        let bytes = std::fs::read(path)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(Error::bad("Arguments exceed 16 MiB"));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        if !value.is_object() {
            return Err(Error::bad("Arguments must be a JSON object"));
        }
        Ok(value)
    } else {
        Ok(json!({}))
    }
}
/// Run the `graphicle` command line of the process with `extensions`
/// installed (see [`crate::extension::install`]), on a multi-threaded Tokio
/// runtime, and return the exit status for `main`. A command that fails
/// prints `graphicle: <message>` and ends the process at once with status 1,
/// without waiting for blocking work it started; so this returns 0.
/// `graphicle graph …` becomes the graph engine before any of that (see
/// [`crate::graph_command`]); this returns only if the engine is missing or
/// cannot start, with the status to exit with.
pub fn main_with(extensions: Extensions) -> i32 {
    let args: Vec<OsString> = std::env::args_os().collect();
    if let Some(engine_args) = crate::graph_command::requested(&args) {
        crate::graph_command::note_project(&args, engine_args);
        return crate::graph_command::exec(engine_args);
    }
    crate::extension::install(extensions);
    crate::fresh::allow_background_builds();
    crate::update_notice::allow();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed building the Runtime")
        .block_on(async {
            let code = match run(std::env::args_os()).await {
                Ok(()) => {
                    // The new-release notice, when one is due.
                    crate::update_notice::finish();
                    0
                }
                Err(error) => {
                    // The common errors name the command that fixes them.
                    eprintln!("graphicle: {}", crate::terminal::explain(&error.message));
                    1
                }
            };
            crate::local_inference::exit_if_loaded(code);
            if code != 0 {
                std::process::exit(code);
            }
            code
        })
}
/// The version `graphicle --version` prints after the program name: the
/// package version and the edition, `0.1.0 (core)`, or the edition an extension declares.
fn version() -> &'static str {
    match crate::core_capabilities::edition() {
        "pro" => concat!(env!("CARGO_PKG_VERSION"), " (pro)"),
        _ => concat!(env!("CARGO_PKG_VERSION"), " (core)"),
    }
}
/// The core's own command line, before any extension adds to it.
pub(crate) fn core_command() -> clap::Command {
    Cli::command()
}
/// The command-line interface of this process: the core commands, the
/// installed extensions' commands and the edition's version.
fn interface() -> clap::Command {
    let command = crate::extension::installed()
        .cli(core_command())
        .version(version());
    // Usage lines name the program as it is typed: clap takes the file
    // name, which on Windows is `graphicle.exe`.
    #[cfg(windows)]
    let command = command.bin_name("graphicle");
    // The short, grouped help; `graphicle help --all` lists
    // every command.
    let help = crate::terminal::help(&command, false);
    command.override_help(help)
}
/// Environment variables that configure features outside Graphicle Core
/// (hosted billing and the storefront). A program in which no installed extension
/// reads them refuses to serve while one is set, instead of serving every
/// workspace without a subscription allowance or usage reports.
const PRO_ENVIRONMENT: &[&str] = &["GRAPHICLE_BILLING_CONFIG", "GRAPHICLE_COMMERCE_CONFIG"];
/// The first of [`PRO_ENVIRONMENT`] that is set but read by none of `read`.
fn unread_environment(read: &[&str], set: impl Fn(&str) -> bool) -> Option<&'static str> {
    PRO_ENVIRONMENT
        .iter()
        .copied()
        .find(|name| set(name) && !read.contains(name))
}
/// Whether the new-release notice ([`crate::update_notice`]) may follow
/// `command` (`None`: `graphicle` alone): a command people run at a terminal
/// that prints its readable view. Never the servers (`mcp`, `serve`, `open`),
/// hooks, session capture, workers, `update`, `uninstall`, a quiet sync or
/// JSON output.
fn notice_allowed(command: Option<&Command>) -> bool {
    use crate::terminal::ConfigAction;
    let json = match command {
        None
        | Some(Command::Config {
            action: None | Some(ConfigAction::Show { json: false }),
        }) => false,
        Some(
            Command::Setup { json, .. }
            | Command::Connect { json, .. }
            | Command::Disconnect { json, .. }
            | Command::Status { json, .. }
            | Command::Doctor { json, .. }
            | Command::Context { json, .. }
            | Command::Ask { json, .. }
            | Command::WorkState { json, .. }
            | Command::Config {
                action: Some(ConfigAction::Set { json, .. }),
            },
        ) => *json,
        Some(Command::Sync { json, quiet, .. }) => *json || *quiet,
        _ => return false,
    };
    crate::terminal::human(json)
}
/// Run one command line (`args` includes the program name). Usage errors,
/// `--help` and `--version` print and end the process, as clap does.
pub async fn run<I, T>(args: I) -> Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let extensions = crate::extension::installed();
    let interface = interface();
    let mut matches = match interface.clone().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => crate::terminal::usage_error(&interface, error),
    };
    let context = CliContext::new(
        matches
            .get_one::<PathBuf>("data_dir")
            .cloned()
            .unwrap_or_else(local::data_dir),
        matches.get_one::<PathBuf>("config").cloned(),
    );
    if let Some(command) = extensions.run_cli(&matches, &context) {
        return command.await;
    }
    let cli = match Cli::from_arg_matches_mut(&mut matches) {
        Ok(cli) => cli,
        Err(error) => error.format(&mut interface.clone()).exit(),
    };
    // The new-release check runs beside an interactive command (it does
    // nothing in a test's process); `main_with` prints the notice after it.
    crate::update_notice::start(
        notice_allowed(cli.command.as_ref()),
        &crate::terminal::config_path(
            cli.config.as_deref(),
            &cli.data_dir.clone().unwrap_or_else(local::data_dir),
        ),
    );
    // `graphicle` alone: this project's short status, or how to start. It
    // creates nothing (earlier builds opened the app).
    let Some(command) = cli.command else {
        let directory = cli.data_dir.unwrap_or_else(local::data_dir);
        return crate::terminal::home(&directory, cli.config.as_deref());
    };
    if let Command::Help { all, command } = &command {
        return crate::terminal::help_command(interface, *all, command);
    }
    // `main_with` hands `graphicle graph` to the engine before parsing; a
    // caller of this function gets the engine as a child process.
    if let Command::Graph { args } = &command {
        let code = crate::graph_command::status(args);
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    if let Command::DocumentWorker { path } = &command {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(crate::documents::max_binary() as u64 + 1)
            .read_to_end(&mut bytes)?;
        return print(&crate::documents::extract_in_process(
            path.file_name()
                .and_then(|p| p.to_str())
                .unwrap_or("source"),
            &bytes,
        )?);
    }
    if matches!(command, Command::ConfigExample) {
        return print(&crate::terminal::config_example());
    }
    if let Command::RedisWorker {
        queue_config,
        model_profile,
        once,
    } = &command
    {
        let bytes = std::fs::read(queue_config)?;
        if bytes.len() > 8192 {
            return Err(Error::bad("Queue configuration exceeds limits"));
        }
        let queue: crate::distributed::Queue = serde_json::from_slice(&bytes)?;
        let password = std::env::var(&queue.credential_ref).map_err(|_| {
            Error::bad("Worker queue credential environment variable is unavailable")
        })?;
        let config = model::Config::load(cli.config.as_deref())?;
        let (_, profile) = config.profile(model_profile.as_deref())?;
        let credential = profile
            .key_env
            .as_ref()
            .map(|name| {
                std::env::var(name).map_err(|_| Error::bad("Worker model credential unavailable"))
            })
            .transpose()?;
        let result = tokio::select! {
            result=crate::distributed::consume(&queue,&password,profile,credential.as_deref(),*once)=>result?,
            _=tokio::signal::ctrl_c()=>json!({"stopped":true})
        };
        return print(&result);
    }
    let directory = cli.data_dir.unwrap_or_else(local::data_dir);
    if let Command::Hook { client, event } = &command {
        hook(&directory, client, event);
        return Ok(());
    }
    if let Command::Restore { path } = command {
        return print(&local::restore(&path, &directory)?);
    }
    if let Command::Uninstall { purge, yes, json } = command {
        return crate::disconnect::uninstall(&directory, purge, yes, json);
    }
    // Commands that read the data folder without creating it.
    match command {
        Command::Config { action } => {
            return crate::terminal::config_command(action, &directory, cli.config.as_deref());
        }
        Command::Doctor {
            fix,
            project,
            port,
            json,
        } => {
            let report = crate::terminal::doctor(&crate::terminal::Doctor {
                directory: &directory,
                config: cli.config.as_deref(),
                project: project.as_deref(),
                fix,
                port,
            })?;
            crate::terminal::emit(&report, json, crate::terminal::doctor_text)?;
            if report["ok"] != true {
                std::process::exit(1);
            }
            return Ok(());
        }
        Command::Update {
            feed: None, json, ..
        } => {
            return crate::terminal::emit(
                &crate::terminal::update_instructions(),
                json,
                crate::terminal::update_text,
            );
        }
        // The Git hooks' quiet sync never creates a data folder.
        Command::Sync { quiet: true, .. } if !directory.join("reviews.sqlite3").is_file() => {
            return Ok(());
        }
        // An agent's server never creates one either: it says what to run.
        Command::Mcp { .. } if !directory.join("reviews.sqlite3").is_file() => {
            return Err(Error::bad(format!(
                "Graphicle is not set up yet: its data folder {} is missing. In your repository, run `graphicle setup`.",
                directory.display()
            )));
        }
        _ => {}
    }
    let store = Store::open(directory)?;
    if let Some(note) = crate::terminal::note_data_version(&store.directory)
        && std::io::stderr().is_terminal()
    {
        eprintln!("graphicle: {note}");
    }
    let default_config = store.directory.join("models.json");
    let config_path = cli
        .config
        .or_else(|| default_config.is_file().then_some(default_config));
    let config = model::Config::load(config_path.as_deref())?;
    // The size limits of whoever runs this program: the configuration
    // file's `limits`, then `GRAPHICLE_MAX_*`.
    let limits = crate::limits::Limits::from_environment(&config.limits)?;
    crate::limits::install(limits);
    let store = store.with_limits(limits);
    let app = App::new(store.clone(), config.clone(), vec![])?;
    match command {
        Command::Setup {
            path,
            yes,
            clients,
            no_capture,
            no_hooks,
            no_instructions,
            switch_from_graphify,
            guards,
            tools,
            json,
        } => crate::setup::setup(
            &store,
            &config,
            &crate::setup::Options {
                path,
                yes,
                clients,
                no_capture,
                no_hooks,
                no_instructions,
                switch_from_graphify,
                json,
                config: config_path.clone(),
                guards,
                tools: tools.as_deref().map(api::ToolSet::parse).transpose()?,
            },
        ),
        Command::Connect {
            client: None,
            config_path,
            name,
            all,
            json,
            project,
            replace,
            hooks,
            work_state,
            // Without a client, every agent found is set up for the code
            // graph, as setup does (`--graph`).
            graph: _,
            allow_learning: _,
            guards,
            tools,
        } => {
            if config_path.is_some() || name.is_some() {
                return Err(Error::bad(
                    "Name a client to use --config-path or --name, for example graphicle connect cursor --name NAME",
                ));
            }
            crate::setup::connect_found(
                &store,
                &crate::setup::ConnectFound {
                    all,
                    project,
                    replace,
                    hooks,
                    work_state,
                    json,
                    guards,
                    tools: tools.as_deref().map(api::ToolSet::parse).transpose()?,
                },
            )
        }
        Command::Connect {
            client: Some(client),
            config_path,
            project,
            name,
            replace,
            hooks,
            work_state,
            graph,
            allow_learning,
            guards,
            tools,
            json,
            ..
        } => {
            let cwd = std::env::current_dir()?;
            crate::setup::print_connect(
                &crate::agent_setup::connect(
                    &store,
                    &crate::agent_setup::Connect {
                        client: &client,
                        config_path: config_path.as_deref(),
                        project: project.as_deref(),
                        name: name.as_deref(),
                        replace,
                        hooks,
                        work_state,
                        graph,
                        allow_learning,
                        tools: tools.as_deref().map(api::ToolSet::parse).transpose()?,
                        guards,
                        cwd: &cwd,
                    },
                )?,
                json,
            )
        }
        Command::Disconnect { client, all, json } => crate::disconnect::disconnect(
            Some(&store),
            &store.directory,
            &crate::disconnect::Disconnect { client, all, json },
        ),
        Command::Uninstall { .. } => unreachable!(),
        Command::Sessions { action } => sessions(&store, action),
        Command::WorkState {
            project,
            list,
            include_closed,
            json,
        } => {
            let session = local::session(&store, false)?;
            let project = local::project(&store, &session, project.as_deref())?;
            let reply = store.work_state_read(
                &session.workspace,
                &session.capability,
                &json!({"repository_id":project,"list":list,"include_closed":include_closed}),
            )?;
            if json {
                print(&reply)
            } else {
                println!("{}", reply["text"].as_str().unwrap_or_default());
                Ok(())
            }
        }
        Command::Hook { .. } => unreachable!(),
        Command::Graph { .. } => unreachable!(),
        Command::Update {
            feed: Some(feed),
            public_key: Some(public_key),
            output,
            ..
        } => {
            let client = model::client_with_timeout(std::time::Duration::from_secs(300))?;
            let result = if let Some(output) = output {
                crate::releases::download(&store, &client, &feed, &public_key, &output).await?
            } else {
                crate::releases::check(&store, &client, &feed, &public_key).await?
            };
            print(&result)
        }
        Command::Update { .. } => crate::terminal::emit(
            &crate::terminal::update_instructions(),
            false,
            crate::terminal::update_text,
        ),
        Command::Backup { path } => print(&store.backup(&path)?),
        Command::DocumentWorker { .. } => unreachable!(),
        Command::Restore { .. } => unreachable!(),
        Command::RedisWorker { .. } => unreachable!(),
        Command::Init { name } => {
            let session = if store.directory.join("local-session.json").exists() {
                local::session(&store, false)?
            } else {
                let created = store.create_workspace(&name)?;
                let session = local::Session {
                    workspace: s(&created, "id").into(),
                    capability: s(&created, "capability").into(),
                    project: None,
                };
                local::save_session(&store, &session)?;
                session
            };
            print(
                &json!({"workspace":session.workspace,"data_directory":store.directory.as_ref(),"access_key":"saved in the private local session file","next":"graphicle sync /path/to/project"}),
            )
        }
        Command::Login {
            workspace,
            key_env,
            project,
        } => {
            let token = std::env::var(key_env)
                .map_err(|_| Error::bad("Credential environment variable is unavailable"))?;
            store.workspace(&workspace, &token)?;
            // An id, a key or a folder, saved as the project's id.
            let project = match project {
                Some(value) => {
                    let cwd = std::env::current_dir().ok();
                    Some(
                        store
                            .resolve_project(
                                &workspace,
                                &token,
                                Some(&value),
                                cwd.as_deref(),
                                None,
                            )?
                            .id,
                    )
                }
                None => None,
            };
            let session = local::Session {
                workspace,
                capability: token,
                project,
            };
            local::save_session(&store, &session)?;
            print(&json!({"connected":true,"workspace":session.workspace}))
        }
        Command::Sync {
            path,
            project,
            background,
            no_graph,
            json,
            quiet,
        } => {
            if quiet {
                // A hook's sync changes nothing that is not a project's.
                let Ok(session) = local::session(&store, false) else {
                    return Ok(());
                };
                let path = match path {
                    Some(path) => path,
                    None => enclosing_folder(&store, &session, project.as_deref()),
                };
                return crate::fresh::sync_quiet(
                    &store,
                    &session,
                    &path,
                    project.as_deref(),
                    no_graph,
                    background,
                );
            }
            let mut session = local::session(&store, true)?;
            // In a subfolder of a set-up project, `graphicle sync` syncs that
            // project, never a new one of the subfolder.
            let path = match path {
                Some(path) => path,
                None => enclosing_folder(&store, &session, project.as_deref()),
            };
            let mut synced = crate::fresh::locked(&store, &path, || {
                local::sync(&store, &mut session, &path, project.as_deref())
            })?;
            note_limited(&synced, json);
            // Every synced project gets
            // its code graph, in the foreground by default.
            let repository = s(&synced, "repository_id").to_string();
            synced["graph"] = if no_graph {
                local::graph_report(&store, &session, &repository)?
            } else if background {
                let mut report = local::graph_report(&store, &session, &repository)?;
                if report["status"] == "queued" {
                    local::spawn_graph_build(&store.directory, &repository)?;
                    report["status"] = json!("background");
                }
                report
            } else {
                local::build_graph(&store, &session, &repository, true)?
            };
            crate::terminal::emit(&synced, json, crate::terminal::sync_text)
        }
        Command::BuildGraph { project } => {
            let session = local::session(&store, false)?;
            let project = local::project(&store, &session, Some(&project))?;
            print(&local::build_graph(&store, &session, &project, false)?)
        }
        Command::Open { path, port } => {
            let mut session = local::session(&store, true)?;
            if let Some(path) = path {
                let synced = crate::fresh::locked(&store, &path, || {
                    local::sync(&store, &mut session, &path, None)
                })?;
                note_limited(&synced, false);
                crate::terminal::emit(&synced, false, crate::terminal::sync_text)?;
            } else if let Ok(resolved) = local::resolve(&store, &session, None) {
                // The app opens on this folder's project (not saved).
                session.project = Some(resolved.id);
            }
            // This data folder's server already on the port: sign in to it.
            if let Some(url) =
                crate::terminal::reuse_server(&store.directory, &session, port).await?
            {
                return crate::terminal::show_url(&url);
            }
            serve(
                store,
                config,
                ServeOptions {
                    listen: "127.0.0.1".parse().unwrap(),
                    port,
                    open: true,
                    learning: false,
                    work_state: false,
                    mcp: api::McpOptions::default(),
                    public_origin: None,
                    session,
                },
            )
            .await
        }
        Command::Serve {
            listen,
            port,
            open,
            mcp_learning,
            mcp_work_state,
            mcp_tools,
            mcp_set,
            public_origin,
        } => {
            // The HTTP route: no pull-request tools, and `repo` names no
            // folder of this machine.
            let mcp = api::McpOptions {
                tools: api::ToolSet::parse(&mcp_tools)?,
                fixed: api::fixed_switches(&mcp_set)?,
                ..Default::default()
            };
            let session = local::session(&store, true)?;
            serve(
                store,
                config,
                ServeOptions {
                    listen,
                    port,
                    open,
                    learning: mcp_learning,
                    work_state: mcp_work_state,
                    mcp,
                    public_origin,
                    session,
                },
            )
            .await
        }
        Command::Worker { once } => {
            let worker = Worker::new(store, config)?;
            if once {
                print(&json!({"processed":worker.once().await?}))
            } else {
                worker_loop(worker).await
            }
        }
        Command::Mcp {
            project,
            workspace,
            allow_learning,
            work_state,
            tools,
            set,
            pull_requests,
        } => {
            let session = local::session(&store, false)?;
            // A project connection, or with --workspace a workspace
            // connection, which has no project of its own.
            let project = if workspace {
                None
            } else {
                Some(local::project(&store, &session, project.as_deref())?)
            };
            let cwd = std::env::current_dir()?;
            // The pull-request tools run gh in this process's folder, so it
            // must be the project's own; a workspace
            // connection has none (clap refuses --pull-requests with
            // --workspace).
            if pull_requests
                && store
                    .find_project(
                        &session.workspace,
                        &session.capability,
                        None,
                        Some(&cwd),
                        None,
                    )?
                    .is_none_or(|found| project.as_deref() != Some(found.id.as_str()))
            {
                return Err(Error::bad(
                    "--pull-requests runs gh in this folder: start graphicle mcp in the folder of the project it serves",
                ));
            }
            // The projects whose waiting graphs the connection builds: its
            // own, or those a workspace recall searches by default, in its
            // order (at most 8). A project past them that a call names with
            // `repo` is read at its latest episode, as during any drain.
            let drained: Vec<String> = match &project {
                Some(project) => vec![project.clone()],
                None => store.workspace_projects(&session.workspace, &session.capability)?,
            };
            let mut app = app;
            app.mcp_learning = allow_learning;
            app.mcp_work_state = work_state;
            app.mcp = api::McpOptions {
                tools: api::ToolSet::parse(&tools)?,
                fixed: api::fixed_switches(&set)?,
                pull_requests,
                cwd: Some(cwd),
            };
            // SIGTERM and SIGINT end the session as a closed input does:
            // handled, they no longer kill the process in the middle
            // of a projection. Installed before the drain starts.
            let shutdown = shutdown_signal()?;
            tokio::pin!(shutdown);
            // The connection's projects get their code graphs without a
            // separately started worker. The
            // drain runs beside the requests (a query never builds; it reads
            // the latest episode meanwhile), and leases keep it from
            // repeating work a running `graphicle serve` holds.
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let drain = {
                let (store, stop) = (store.clone(), stop.clone());
                std::thread::spawn(move || {
                    for project in drained {
                        if stop.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        if let Err(error) =
                            crate::jobs::drain_projection_until(&store, &project, Some(&stop))
                        {
                            eprintln!("graphicle mcp: {}", error.message);
                        }
                    }
                })
            };
            let mut input = stdin_lines(MCP_MESSAGE_LIMIT);
            let mut stdout = tokio::io::stdout();
            let mut connection = api::McpSession::default();
            let freshness = crate::fresh::Options::answer(&config);
            let served: Result<()> = loop {
                let read = tokio::select! {
                    read = input.recv() => read,
                    () = &mut shutdown => break Ok(()),
                };
                let line = match read {
                    // The client closed its input.
                    None => break Ok(()),
                    Some(Ok(line)) if line.is_empty() => break Ok(()),
                    Some(Ok(line)) => line,
                    Some(Err(error)) => break Err(error.into()),
                };
                if line.len() as u64 > MCP_MESSAGE_LIMIT {
                    break Err(Error::bad("MCP message exceeds 16 MiB"));
                }
                let request: Value = match serde_json::from_slice(&line) {
                    Ok(request) => request,
                    Err(error) => break Err(error.into()),
                };
                // The easy flow's freshness, before every tool call of a
                // project connection; a workspace connection
                // reads each project as of its last sync.
                let fresh = match &project {
                    Some(project) if request["method"] == "tools/call" => {
                        Some(crate::fresh::ensure(&store, &session, project, freshness))
                    }
                    _ => None,
                };
                let reply = match &project {
                    Some(project) => api::mcp_session(
                        &app,
                        &session.workspace,
                        &session.capability,
                        project,
                        &request,
                        &mut connection,
                    ),
                    None => api::mcp_workspace_session(
                        &app,
                        &session.workspace,
                        &session.capability,
                        &request,
                        &mut connection,
                    ),
                };
                if let Some(mut value) = reply {
                    if let Some(fresh) = &fresh {
                        fresh.attach_mcp(&mut value);
                    }
                    let mut reply = value.to_string().into_bytes();
                    reply.push(b'\n');
                    if let Err(error) = async {
                        stdout.write_all(&reply).await?;
                        stdout.flush().await
                    }
                    .await
                    {
                        break Err(error.into());
                    }
                }
            };
            // The session ended (the client closed its input or went away, a
            // signal, or an unreadable message): finish the job in hand, so
            // its lease is released rather than left to expire, and start no
            // other.
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = tokio::task::spawn_blocking(move || drain.join()).await;
            served
        }
        Command::Status { project, json } => {
            let session = local::session(&store, false)?;
            let resolved = local::resolve(&store, &session, project.as_deref())?;
            // The easy flow's freshness: the folder's few changed files first.
            let fresh = crate::fresh::ensure(
                &store,
                &session,
                &resolved.id,
                crate::fresh::Options::answer(&config),
            );
            let mut status = api::command_async(
                &app,
                &session.workspace,
                &session.capability,
                "status",
                &json!({"repository_id":resolved.id}),
            )
            .await?;
            fresh.attach(&mut status);
            if crate::terminal::human(json) {
                // The freshness line is the view's "Changes".
                let mut view =
                    crate::terminal::overview(&store, &session, &resolved, status, &config)?;
                view.changes =
                    crate::terminal::changes_line(&store, &session, &resolved.id, &fresh);
                crate::terminal::say(&crate::terminal::status_text(
                    &view,
                    crate::terminal::Style::detect(),
                ))
            } else {
                fresh.tell();
                print(&status)
            }
        }
        Command::Jobs { project, job } => {
            let session = local::session(&store, false)?;
            let project = local::project(&store, &session, project.as_deref())?;
            print(&store.job_status(
                &session.workspace,
                &session.capability,
                &json!({"repository_id":project,"job_id":job}),
            )?)
        }
        Command::Context {
            question,
            project,
            workspace,
            max_tokens,
            local_embedding_profile,
            local_reranker_profile,
            json,
        } => {
            let session = crate::phase::phase("context_session", || local::session(&store, false))?;
            // With --workspace the project only comes first,
            // and none is needed outside every synced folder.
            let resolved = crate::phase::phase("context_resolve", || {
                match (workspace, project.as_deref()) {
                    (true, None) => local::find(&store, &session),
                    (_, requested) => local::resolve(&store, &session, requested).map(Some),
                }
            })?;
            // Outside a project, a person is told which project answers.
            let origin = resolved
                .as_ref()
                .filter(|_| !workspace)
                .and_then(|resolved| crate::terminal::origin_note(&store, &session, resolved));
            let project = resolved.map(|resolved| resolved.id);
            // The easy flow's freshness for the project (a workspace read
            // reads the other projects as of their last sync).
            let fresh = match &project {
                Some(project) => crate::phase::phase("context_freshness", || {
                    crate::fresh::ensure(
                        &store,
                        &session,
                        project,
                        crate::fresh::Options::answer(&config),
                    )
                }),
                None => crate::fresh::Outcome::default(),
            };
            let mut args = json!({"repository_id":project,"question":question,"max_tokens":max_tokens,"max_chars":max_tokens.saturating_mul(4).clamp(2000,64000),"local_embedding_profile":local_embedding_profile,"local_reranker_profile":local_reranker_profile});
            if workspace {
                args["scope"] = json!("workspace");
            }
            let mut packet = crate::phase::phase("context_packet", || {
                api::command(
                    &app,
                    &session.workspace,
                    &session.capability,
                    "context",
                    &args,
                )
            })?;
            fresh.attach(&mut packet);
            // On a terminal the note is part of the readable packet.
            if !crate::terminal::human(json) {
                fresh.tell();
            }
            crate::terminal::emit(&packet, json, |packet, style| {
                crate::terminal::with_origin(
                    origin.as_deref(),
                    style,
                    crate::terminal::context_text(packet, style),
                )
            })
        }
        Command::Ask {
            question,
            project,
            workflow,
            retain,
            max_calls,
            json,
        } => {
            // `basic` with its default contract (grounded-v2) answers unless
            // the caller opts into `reflect`, the workflow that checks
            // an answer before it may be kept as a draft.
            if retain && workflow != "reflect" {
                return Err(Error::bad(
                    "--retain needs --workflow reflect, which checks the answer before saving it",
                ));
            }
            let session = local::session(&store, false)?;
            let resolved = local::resolve(&store, &session, project.as_deref())?;
            let human = crate::terminal::human(json);
            // Outside a project, a person is told which project answers.
            let origin = human
                .then(|| crate::terminal::origin_note(&store, &session, &resolved))
                .flatten();
            let project = resolved.id;
            // The folder's few changed files first; the build below then
            // gives them their code graph.
            let fresh = crate::fresh::ensure(
                &store,
                &session,
                &project,
                crate::fresh::Options::ask(&config),
            );
            // On a terminal the note is part of the readable answer.
            if !human {
                fresh.tell();
            }
            // The project's waiting projection first, so the
            // answer reads its current code graph (the wait loop below runs
            // the answer before priority -1 work).
            local::build_graph(&store, &session, &project, true)?;
            // No answer model: the evidence, as `context` prints it, and the
            // command that adds one (exit status 0).
            if crate::terminal::no_answer_model(&store, &session, &config) {
                let mut packet = api::command(
                    &app,
                    &session.workspace,
                    &session.capability,
                    "context",
                    &json!({"repository_id":project,"question":question,"max_tokens":3000,"max_chars":12000}),
                )?;
                fresh.attach(&mut packet);
                return crate::terminal::evidence_without_model(&packet, human, origin.as_deref());
            }
            let mut request = json!({"repository_id":project,"kind":workflow,"question":question,"max_calls":max_calls});
            if workflow == "reflect" {
                request["retain"] = json!(retain);
            }
            let mut job =
                store.enqueue(&session.workspace, &session.capability, &request, &config)?;
            // The same question again after a failure (the key was missing,
            // the provider unreachable) is a new attempt, never the old
            // failure; an answer that completed is still reused.
            for _ in 0..64 {
                if job["replayed"] != true || !["failed", "cancelled"].contains(&s(&job, "state")) {
                    break;
                }
                job = store.retry_job(
                    &session.workspace,
                    &session.capability,
                    &json!({"repository_id":project,"job_id":job["id"]}),
                    &config,
                )?;
            }
            if !human {
                return wait_job(&app.worker, &session, &project, s(&job, "id"), Some(&fresh))
                    .await;
            }
            let answering = crate::terminal::model_state(Some((&store, &session)), &config);
            if let Some(model) = &answering {
                eprintln!("graphicle: asking {} ({})...", model.name, model.model);
            }
            let mut value = wait_job_value(&app.worker, &session, &project, s(&job, "id")).await?;
            fresh.attach(&mut value);
            if value["state"] == "complete" {
                let style = crate::terminal::Style::detect();
                crate::terminal::say(&crate::terminal::with_origin(
                    origin.as_deref(),
                    style,
                    crate::terminal::answer_text(&value, style),
                ))
            } else {
                Err(Error::bad(crate::terminal::job_failure(
                    &value,
                    answering.as_ref(),
                )))
            }
        }
        Command::Run {
            kind,
            project,
            arguments: path,
            wait,
            max_calls,
        } => {
            let session = local::session(&store, false)?;
            let project = local::project(&store, &session, project.as_deref())?;
            let mut args = arguments(path)?;
            args["repository_id"] = json!(project);
            args["kind"] = json!(kind);
            args["max_calls"] = json!(max_calls);
            if wait {
                // As `ask`, the waiting projection first.
                local::build_graph(&store, &session, &project, true)?;
            }
            let job = store.enqueue(&session.workspace, &session.capability, &args, &config)?;
            if wait {
                wait_job(&app.worker, &session, &project, s(&job, "id"), None).await
            } else {
                print(&job)
            }
        }
        Command::Call {
            operation,
            project,
            arguments: path,
        } => {
            let session = local::session(&store, false)?;
            let mut args = arguments(path)?;
            // `call` sends any operation: `--project` is
            // the project an id, a key or a folder names, else the value as
            // given, which the operation checks itself (`register`,
            // `build.info` and the other workspace operations ignore it).
            // Without it: this folder's project, else the saved default,
            // else no `repository_id` at all.
            let project = match project {
                Some(value) => {
                    Some(local::project(&store, &session, Some(&value)).unwrap_or(value))
                }
                None => local::find(&store, &session)?.map(|resolved| resolved.id),
            };
            if let Some(project) = project {
                args["repository_id"] = json!(project);
            }
            print(&api::command(
                &app,
                &session.workspace,
                &session.capability,
                &operation,
                &args,
            )?)
        }
        Command::ConfigExample
        | Command::Help { .. }
        | Command::Config { .. }
        | Command::Doctor { .. } => unreachable!(),
    }
}
/// `graphicle sessions …`: capture, forget and browse agent sessions.
fn sessions(store: &Store, action: SessionsAction) -> Result<()> {
    use crate::capture::Client;
    let session = local::session(store, false)?;
    match action {
        SessionsAction::Import {
            client,
            path,
            since_hours,
            project,
            limit,
        } => {
            // An explicit --project (an id, a key or a folder) takes every
            // transcript; without it each goes to its own folder's project.
            let project = project
                .map(|value| local::project(store, &session, Some(&value)))
                .transpose()?;
            print(&import_sessions(
                store,
                &session,
                Client::parse(&client)?,
                path,
                since_hours,
                project,
                limit,
            )?)
        }
        SessionsAction::Forget {
            client,
            session_id,
            project,
        } => {
            let project = local::project(store, &session, project.as_deref())?;
            print(&store.forget_session(
                &session.workspace,
                &session.capability,
                &json!({"repository_id":project,"client":client,"session_id":session_id}),
            )?)
        }
        SessionsAction::Timeline {
            session_id,
            anchor,
            before,
            after,
            project,
        } => {
            let project = local::project(store, &session, project.as_deref())?;
            print(&store.session_timeline(
                &session.workspace,
                &session.capability,
                &json!({"repository_id":project,"session_id":session_id,"anchor_source_id":anchor,"before":before,"after":after}),
            )?)
        }
        SessionsAction::Context { project } => {
            use std::io::Write;
            let project = local::project(store, &session, project.as_deref())?;
            let text =
                store.session_start_context(&session.workspace, &session.capability, &project)?;
            std::io::stdout().write_all(text.as_bytes())?;
            Ok(())
        }
    }
}

/// What importing one transcript file did.
enum Imported {
    Captured(Value),
    Unparsed,
    Empty,
    OutsideProjects,
}

/// Import one transcript into `project`, else into the synced project that
/// contains its working directory (`targets` caches that lookup).
fn import_transcript(
    store: &Store,
    session: &local::Session,
    client: crate::capture::Client,
    file: &std::path::Path,
    project: Option<&str>,
    targets: &mut std::collections::BTreeMap<String, Option<String>>,
) -> Result<Imported> {
    use std::io::Read;
    let max = crate::limits::current().transcript_bytes;
    let mut bytes = Vec::new();
    std::fs::File::open(file)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(Error::bad(format!(
            "Transcript exceeds {}; raise the limit with {}",
            crate::limits::size(max),
            crate::limits::raise("transcript_bytes")
        )));
    }
    let Ok(parsed) = crate::capture::parse(client, &String::from_utf8_lossy(&bytes)) else {
        return Ok(Imported::Unparsed);
    };
    if parsed.turns.is_empty() {
        return Ok(Imported::Empty);
    }
    let target = match (project, &parsed.cwd) {
        (Some(project), _) => Some(project.to_string()),
        (None, Some(cwd)) => {
            if !targets.contains_key(cwd) {
                let found =
                    crate::capture::project_for_cwd(store, session, std::path::Path::new(cwd))?;
                targets.insert(cwd.clone(), found);
            }
            targets[cwd].clone()
        }
        (None, None) => None,
    };
    let Some(target) = target else {
        return Ok(Imported::OutsideProjects);
    };
    Ok(Imported::Captured(store.capture_session(
        &session.workspace,
        &session.capability,
        &target,
        client,
        &parsed,
    )?))
}

fn import_sessions(
    store: &Store,
    session: &local::Session,
    client: crate::capture::Client,
    path: Option<PathBuf>,
    since_hours: Option<u64>,
    project: Option<String>,
    limit: usize,
) -> Result<Value> {
    use crate::capture;
    let root = match path {
        Some(path) => path,
        None => capture::default_root(client)
            .ok_or_else(|| Error::bad("Home directory unavailable; pass --path"))?,
    };
    let since = since_hours.map(|hours| {
        std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(hours.saturating_mul(3600)))
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    let files = if root.is_file() {
        vec![root]
    } else if root.is_dir() {
        capture::transcripts(&root, since, limit.clamp(1, 100_000))
    } else {
        return Err(Error::bad("Transcript path not found"));
    };
    let mut targets = std::collections::BTreeMap::new();
    let (mut sessions, mut turns, mut outside, mut unparsed, mut empty) = (0, 0, 0, 0, 0);
    let mut captured = Vec::new();
    let mut errors = Vec::new();
    for file in &files {
        match import_transcript(
            store,
            session,
            client,
            file,
            project.as_deref(),
            &mut targets,
        ) {
            Ok(Imported::Captured(value)) => {
                sessions += 1;
                turns += value["imported"].as_u64().unwrap_or(0);
                let notable = value["imported"].as_u64().unwrap_or(0) > 0
                    || value["session_forgotten"] == true
                    || value["paused"] == true;
                if notable && captured.len() < 100 {
                    captured.push(value);
                }
            }
            Ok(Imported::Unparsed) => unparsed += 1,
            Ok(Imported::Empty) => empty += 1,
            Ok(Imported::OutsideProjects) => outside += 1,
            Err(error) => {
                if errors.len() < 10 {
                    errors.push(json!({"path":file,"error":error.message}));
                }
            }
        }
    }
    Ok(
        json!({"client":client.as_str(),"transcripts":files.len(),"sessions":sessions,"turns_imported":turns,
        "skipped_outside_synced_projects":outside,"skipped_unparsed":unparsed,"skipped_without_completed_turns":empty,
        "failed":errors.len(),"errors":errors,"captured":captured}),
    )
}

/// Client lifecycle hook. It never blocks or fails the client: the session
/// start context has 2.5 seconds, imports run in a detached process, every
/// error is dropped and the exit code is 0.
fn hook(directory: &std::path::Path, client: &str, event: &str) {
    use std::io::Read;
    let Ok(client) = crate::capture::Client::parse(client) else {
        return;
    };
    // A data folder that is gone stays gone: `graphicle setup` makes a new
    // one (doctor reports the hooks that point at it).
    if !directory.join("reviews.sqlite3").is_file() {
        return;
    }
    let mut input = Vec::new();
    if std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut input)
        .is_err()
        || input.len() > 1024 * 1024
    {
        return;
    }
    let payload: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    let event: String = event
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    match event.as_str() {
        "sessionstart" => {
            // A resumed Claude Code conversation still holds the context its
            // own start added (claude-mem #4423).
            if client == crate::capture::Client::ClaudeCode && payload["source"] == "resume" {
                return;
            }
            let Some(cwd) = payload["cwd"]
                .as_str()
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
            else {
                return;
            };
            let directory = directory.to_path_buf();
            let (sender, receiver) = std::sync::mpsc::channel();
            // A plain thread: returning from main does not wait for it.
            std::thread::spawn(move || {
                let context = (|| -> Result<Option<String>> {
                    let store = Store::open(directory)?;
                    let session = local::session(&store, false)?;
                    let Some(project) = crate::capture::project_for_cwd(&store, &session, &cwd)?
                    else {
                        return Ok(None);
                    };
                    // An empty context (nothing reviewed, open or stale)
                    // prints nothing, so the session gets no Graphicle text.
                    let context = store.session_start_context(
                        &session.workspace,
                        &session.capability,
                        &project,
                    )?;
                    Ok((!context.is_empty()).then_some(context))
                })();
                let _ = sender.send(context);
            });
            if let Ok(Ok(Some(context))) =
                receiver.recv_timeout(std::time::Duration::from_millis(2500))
            {
                use std::io::Write;
                let output = json!({"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":context}});
                let _ = writeln!(std::io::stdout(), "{output}");
            }
        }
        "stop" | "sessionend" | "precompact" => {
            let Some(transcript) = payload["transcript_path"].as_str().map(PathBuf::from) else {
                return;
            };
            if transcript.extension().is_none_or(|e| e != "jsonl") || !transcript.is_file() {
                return;
            }
            let Ok(executable) = std::env::current_exe() else {
                return;
            };
            let mut command = std::process::Command::new(executable);
            command
                .arg("--data-dir")
                .arg(directory)
                .args(["sessions", "import", "--client", client.as_str(), "--path"])
                .arg(&transcript)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            // Its own process group: a client ending the hook's group on
            // timeout does not stop the import.
            let _ = crate::platform::spawn_detached(&mut command);
        }
        // The guard hooks of `connect --graph`.
        "guardsearch" | "guardread" => {
            use std::io::Write;
            let kind = if event == "guardsearch" {
                "search"
            } else {
                "read"
            };
            match crate::agent_setup::guard(directory, kind, &payload) {
                crate::agent_setup::Guard::Nudge(output) => {
                    let _ = writeln!(std::io::stdout(), "{output}");
                }
                crate::agent_setup::Guard::Engine => engine_guard(kind, &input),
                crate::agent_setup::Guard::Quiet => {}
            }
        }
        _ => {}
    }
}
/// The graph engine's own guard (`graphicle-graph hook-guard <kind>`, found
/// as `graphicle graph` finds the engine) on the hook's input, its output
/// passed on unchanged; nothing when the engine is missing or fails.
fn engine_guard(kind: &str, input: &[u8]) {
    use std::io::Write;
    let program = std::env::current_exe().ok();
    let Some(engine) =
        crate::graph_command::locate(program.as_deref(), std::env::var_os("PATH").as_deref())
    else {
        return;
    };
    let Ok(mut child) = std::process::Command::new(engine)
        .args(["hook-guard", kind])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    if let Ok(output) = child.wait_with_output() {
        let _ = std::io::stdout().write_all(&output.stdout);
    }
}
async fn wait_job(
    worker: &Worker,
    session: &local::Session,
    project: &str,
    id: &str,
    fresh: Option<&crate::fresh::Outcome>,
) -> Result<()> {
    let mut value = wait_job_value(worker, session, project, id).await?;
    if let Some(fresh) = fresh {
        fresh.attach(&mut value);
    }
    print(&value)?;
    if value["state"] == "complete" {
        Ok(())
    } else {
        let model = crate::terminal::model_state(Some((&worker.store, session)), &worker.config);
        Err(Error::bad(format!(
            "Workflow did not complete: {}",
            crate::terminal::job_failure(&value, model.as_ref())
        )))
    }
}
async fn wait_job_value(
    worker: &Worker,
    session: &local::Session,
    project: &str,
    id: &str,
) -> Result<Value> {
    loop {
        let value = worker.store.job_status(
            &session.workspace,
            &session.capability,
            &json!({"repository_id":project,"job_id":id}),
        )?;
        if !["queued", "retry", "running"].contains(&s(&value, "state")) {
            return Ok(value);
        }
        if !worker.once().await? {
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        }
    }
}
async fn worker_loop(worker: Worker) -> Result<()> {
    let mut last_schedule = std::time::Instant::now() - std::time::Duration::from_secs(60);
    loop {
        if last_schedule.elapsed() >= std::time::Duration::from_secs(60) {
            if let Err(error) = worker.store.schedule_connectors() {
                eprintln!("graphicle source scheduler: {}", error.message);
            }
            if let Err(error) = worker.store.auto_observe(&worker.config, None) {
                eprintln!("graphicle observation scheduler: {}", error.message);
            }
            last_schedule = std::time::Instant::now();
        }
        // Drain ready work back to back; when idle, wait for a newly queued
        // job or the next poll (retries, schedules and other processes).
        match worker.once().await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => eprintln!("graphicle worker: {}", error.message),
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            _ = worker.store.wake.notified() => {}
        }
    }
}
struct ServeOptions {
    listen: IpAddr,
    port: u16,
    open: bool,
    learning: bool,
    work_state: bool,
    mcp: api::McpOptions,
    public_origin: Option<String>,
    session: local::Session,
}
async fn serve(store: Store, config: model::Config, options: ServeOptions) -> Result<()> {
    let ServeOptions {
        listen,
        port,
        open,
        learning,
        work_state,
        mcp,
        public_origin,
        session,
    } = options;

    if let Some(name) = unread_environment(&crate::extension::installed().environment(), |name| {
        std::env::var_os(name).is_some()
    }) {
        return Err(Error::bad(format!(
            "{name} configures a feature that is not part of Graphicle Core, and this program does not read it; unset it"
        )));
    }
    if !listen.is_loopback() {
        return Err(Error::bad(
            "Bind this local app to loopback; use an authenticated reverse proxy for remote access",
        ));
    }
    let address = SocketAddr::new(listen, port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| crate::terminal::bind_error(port, error))?;
    let actual = listener.local_addr()?;
    let hosts = vec![actual.to_string(), format!("localhost:{}", actual.port())];
    let mut app = App::new(store, config, hosts)?;
    if let Some(origin) = public_origin {
        app = app.hosted(&origin)?;
    }
    // `graphicle open` reuses this server only when it proves it holds
    // this secret, which the data folder's `server.json` keeps.
    let secret = format!("{}{}", crate::id(), crate::id());
    app.instance_secret = Some(std::sync::Arc::new(secret.clone()));
    let directory = app.worker.store.directory.to_path_buf();
    if let Err(error) = crate::terminal::record_server(&directory, actual.port(), &secret) {
        eprintln!("graphicle: could not record the server ({})", error.message);
    }
    let mut extension_context = ServeContext {
        store: &app.worker.store,
        public_origin: app.public_origin.as_deref(),
        background: Vec::new(),
    };
    crate::extension::installed().serve(&mut extension_context)?;
    let extension_tasks = extension_context.background;
    app.mcp_learning = learning;
    app.mcp_work_state = work_state;
    app.mcp = mcp;
    // Install handlers before announcing readiness. A service manager can send
    // SIGTERM immediately after that announcement, before Axum polls its future.
    let shutdown = shutdown_signal()?;
    let url = format!("http://{actual}");
    eprintln!("Graphicle is ready at {url}");
    if open {
        let ticket=app.browser_ticket(json!({"workspace":session.workspace,"capability":session.capability,"project":session.project}))?;
        // A browser when one can start; else the address.
        crate::terminal::show_url(&format!("{url}/#session={ticket}"))?;
        eprintln!("graphicle: the app runs here until you press Ctrl-C");
    }
    let mut workers = Vec::new();
    for _ in 0..app.worker.config.worker_concurrency {
        workers.push(tokio::spawn(worker_loop(app.worker.clone())));
    }
    let mut background = Vec::new();
    for task in extension_tasks {
        background.push(tokio::spawn(task));
    }
    let result = axum::serve(listener, api::router(app))
        .with_graceful_shutdown(shutdown)
        .await;
    for worker in workers {
        worker.abort();
    }
    for task in background {
        task.abort();
    }
    crate::terminal::forget_server(&directory, &secret);
    result.map_err(Error::from)
}

/// The folder `graphicle sync` without PATH syncs: the registered folder of
/// the project this folder is in (a subfolder of a set-up project syncs
/// that project), else this folder. With `--project`, this folder.
fn enclosing_folder(store: &Store, session: &local::Session, requested: Option<&str>) -> PathBuf {
    if requested.is_none()
        && let Ok(Some(found)) = local::find(store, session)
        && found.source == "folder"
        && let Some(folder) = found.folder
    {
        return folder;
    }
    PathBuf::from(".")
}
/// The largest MCP message `graphicle mcp` reads from standard input.
const MCP_MESSAGE_LIMIT: u64 = 16 * 1024 * 1024;

/// Standard input's lines, each with its newline and at most `limit` + 1
/// bytes (a longer one is cut there), read on a thread of its own; an empty
/// line is the end of the input. A read that waits for input cannot be
/// cancelled: on tokio's blocking pool it would keep the runtime, and so the
/// process, from ending after a signal. This thread does
/// not: it ends with the process.
fn stdin_lines(limit: u64) -> tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (lines, receiver) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        use std::io::{BufRead, Read};
        let mut input = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            let read = (&mut input).take(limit + 1).read_until(b'\n', &mut line);
            let last = !matches!(read, Ok(size) if size > 0);
            if lines.blocking_send(read.map(|_| line)).is_err() || last {
                break;
            }
        }
    });
    receiver
}
fn shutdown_signal() -> Result<impl std::future::Future<Output = ()>> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
    }
    #[cfg(not(unix))]
    {
        Ok(async {
            let _ = tokio::signal::ctrl_c().await;
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn serving_refuses_pro_configuration_no_extension_reads() {
        let billing = |name: &str| name == "GRAPHICLE_BILLING_CONFIG";
        assert_eq!(
            super::unread_environment(&[], billing),
            Some("GRAPHICLE_BILLING_CONFIG")
        );
        assert_eq!(
            super::unread_environment(&["GRAPHICLE_BILLING_CONFIG"], billing),
            None
        );
        assert_eq!(super::unread_environment(&[], |_| false), None);
    }
    #[test]
    fn version_names_the_edition() {
        // No extension is installed in this test binary: the Core edition.
        assert_eq!(
            super::interface().render_version(),
            concat!("graphicle ", env!("CARGO_PKG_VERSION"), " (core)\n")
        );
    }

    /// `graphicle --help` lists `graph`, and the subcommand takes every word
    /// after it, options and `--help` included, for the engine.
    #[test]
    fn graph_is_listed_and_passes_its_words_on_unchanged() {
        use clap::Parser;
        let help = super::interface().render_help().to_string();
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with("graph ")
                    && line.contains("Build, update and query code graphs")),
            "{help}"
        );
        for line in [
            "graphicle graph",
            "graphicle graph --help",
            "graphicle graph update . --force -x",
            "graphicle graph query how does auth work --budget 2000",
        ] {
            let words: Vec<&str> = line.split(' ').collect();
            let cli = super::Cli::try_parse_from(&words).unwrap();
            let Some(super::Command::Graph { args }) = cli.command else {
                panic!("{line}");
            };
            let expected: Vec<std::ffi::OsString> = words[2..].iter().map(Into::into).collect();
            assert_eq!(args, expected, "{line}");
        }
    }
}
