//! The terminal side of the `graphicle` command line (the easy flow's
//! terminal commands).
//!
//! - **Output for people and for scripts.** A command prints its readable
//!   view only when standard output is a terminal, and otherwise exactly the
//!   JSON it printed before. `--json` forces JSON; [`OUTPUT_ENV`] (`text` or
//!   `json`) overrides the terminal test. Colour is used only on a terminal,
//!   never with `NO_COLOR` set or `TERM=dumb`.
//! - **Help.** The short, grouped help ([`help`]); `graphicle help --all`
//!   adds the advanced and internal commands.
//! - **Commands.** `graphicle` alone ([`home`]), `status` ([`status_text`]),
//!   `doctor` ([`doctor`]), `ask` and `context` ([`answer_text`],
//!   [`context_text`]), `config` ([`config_show`], [`config_set`],
//!   [`set_answer_model`], [`detect_models`]), `open` ([`reuse_server`],
//!   [`show_url`]) and `update` ([`update_instructions`]).
//! - **Errors.** The common errors with the command that fixes them
//!   ([`explain`], [`port_in_use`], [`usage_error`], [`job_failure`]).
//!
//! Nothing here changes how a store, the SDKs or the HTTP and MCP servers
//! behave. The only file this module writes on its own is the data folder's
//! version stamp ([`note_data_version`]); `config set` writes the model
//! configuration file, after a first backup.
use crate::platform::CanonicalExt;
use crate::{
    Error, Result, local, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::{
    ffi::{OsStr, OsString},
    fmt::Write as _,
    io::{IsTerminal, Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

/// Overrides how output is chosen: `text` prints the readable view even
/// through a pipe, `json` prints JSON on a terminal too.
pub const OUTPUT_ENV: &str = "GRAPHICLE_OUTPUT";
/// `1`: `open` and `serve --open` print the app's address instead of
/// starting a browser (the desktop app sets it for the same purpose).
pub const NO_BROWSER_ENV: &str = "GRAPHICLE_NO_BROWSER";
/// The app's default port.
pub const DEFAULT_PORT: u16 = 8854;
/// What Graphicle is, in one line: the command line's about text.
pub const ABOUT: &str = "Code graph and memory for your repositories and agents";

// ---------------------------------------------------------------- output

/// Whether a command prints its readable view: never with `--json`, else as
/// [`OUTPUT_ENV`] says, else when standard output is a terminal.
pub fn human(json: bool) -> bool {
    human_with(
        json,
        std::env::var(OUTPUT_ENV).ok().as_deref(),
        std::io::stdout().is_terminal(),
    )
}

/// [`human`] for an explicit `--json`, [`OUTPUT_ENV`] value and terminal test.
pub fn human_with(json: bool, output: Option<&str>, terminal: bool) -> bool {
    !json
        && match output {
            Some("json") => false,
            Some("text") => true,
            _ => terminal,
        }
}

/// Text styles: ANSI colours only when [`color_with`] allows them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Style {
    color: bool,
}

impl Style {
    /// Colour on a terminal, unless `NO_COLOR` is set or `TERM` is `dumb`.
    pub fn detect() -> Self {
        Self {
            // On Windows, colour also needs virtual-terminal output.
            color: color_with(
                std::env::var_os("NO_COLOR").as_deref(),
                std::env::var("TERM").ok().as_deref(),
                std::io::stdout().is_terminal(),
            ) && crate::platform::ansi_output(),
        }
    }
    /// No colour.
    pub fn plain() -> Self {
        Self { color: false }
    }
    fn paint(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }
    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }
    pub fn good(self, text: &str) -> String {
        self.paint("32", text)
    }
    pub fn warn(self, text: &str) -> String {
        self.paint("33", text)
    }
    pub fn bad(self, text: &str) -> String {
        self.paint("31", text)
    }
}

/// Colour only on a terminal, with `NO_COLOR` unset or empty
/// (<https://no-color.org>) and `TERM` other than `dumb`.
pub fn color_with(no_color: Option<&OsStr>, term: Option<&str>, terminal: bool) -> bool {
    terminal && no_color.is_none_or(OsStr::is_empty) && term != Some("dumb")
}

/// Print `value` as JSON, or its readable view (`render`) when [`human`].
pub fn emit(value: &Value, json: bool, render: impl FnOnce(&Value, Style) -> String) -> Result<()> {
    emit_text(value, json, render).map(|_| ())
}

/// [`emit`], returning the text it printed (without the newline [`say`]
/// ends it with), for a command that counts what it serves
/// (`crate::metering`).
pub fn emit_text(
    value: &Value,
    json: bool,
    render: impl FnOnce(&Value, Style) -> String,
) -> Result<String> {
    let text = if human(json) {
        render(value, Style::detect())
    } else {
        serde_json::to_string_pretty(value)?
    };
    say(&text)?;
    Ok(text)
}

/// Print `text` on standard output, ending with one newline.
pub fn say(text: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

/// `1234567` as `1,234,567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `n` and `word`, with an `s` unless `n` is 1 (`memory`: `memories`).
fn plural(n: u64, word: &str) -> String {
    match (n, word.strip_suffix('y')) {
        (1, _) => format!("1 {word}"),
        (_, Some(stem)) if !stem.ends_with(['a', 'e', 'o', 'u']) => {
            format!("{} {stem}ies", count(n))
        }
        _ => format!("{} {word}s", count(n)),
    }
}

/// A byte size for people: `512 B`, `3.4 KB`, `12.0 MB`, `1.5 GB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// An RFC 3339 time as local time and its age: `2026-10-13 21:05 (5 minutes
/// ago)`; the text as given when it is not a time.
pub fn when(stamp: &str) -> String {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(stamp) else {
        return stamp.to_string();
    };
    let local = time.with_timezone(&chrono::Local);
    let age = chrono::Utc::now().signed_duration_since(time.with_timezone(&chrono::Utc));
    format!(
        "{} ({})",
        local.format("%Y-%m-%d %H:%M"),
        ago(age.num_seconds())
    )
}

/// `seconds` of age in words.
pub fn ago(seconds: i64) -> String {
    match seconds {
        i64::MIN..60 => "just now".into(),
        60..3600 => format!("{} ago", plural((seconds / 60) as u64, "minute")),
        3600..86400 => format!("{} ago", plural((seconds / 3600) as u64, "hour")),
        _ => format!("{} ago", plural((seconds / 86400) as u64, "day")),
    }
}

/// `path` with the home folder written `~` (also when one of them is
/// canonical and the other is not, as with macOS's `/var` and
/// `/private/var`).
pub fn tilde(path: &Path) -> String {
    if let Some(home) = home_dir() {
        let canonical = home.canonical().ok();
        if let Some(rest) = path.strip_prefix(&home).ok().or_else(|| {
            canonical
                .as_deref()
                .and_then(|home| path.strip_prefix(home).ok())
        }) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", after_tilde(rest))
            };
        }
    }
    path.display().to_string()
}

/// The part of a path after `~/`: as it is on macOS and Linux; with `/`
/// separators on Windows, so a shown path never mixes `/` and `\`
/// (`~/AppData/Local/Graphicle`).
fn after_tilde(rest: &Path) -> String {
    #[cfg(windows)]
    {
        rest.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        rest.display().to_string()
    }
}

fn home_dir() -> Option<PathBuf> {
    crate::platform::home_dir().filter(|h| !h.as_os_str().is_empty())
}

/// `path` relative to `folder` when it is inside it, else [`tilde`].
pub fn place(path: &Path, folder: Option<&Path>) -> String {
    match folder.and_then(|folder| path.strip_prefix(folder).ok()) {
        Some(inside) if !inside.as_os_str().is_empty() => inside.display().to_string(),
        _ => tilde(path),
    }
}

/// The canonical form of `path`, or `path` itself when it does not exist.
fn canonical(path: &Path) -> PathBuf {
    path.canonical().unwrap_or_else(|_| path.to_path_buf())
}

// ------------------------------------------------------------------ help

/// The short help's command groups, in order. A name the program does not
/// have (an edition's command, or one another part of the program adds) is
/// left out.
pub const GROUPS: &[(&str, &[&str])] = &[
    (
        "Start",
        &["setup", "ask", "context", "open", "status", "doctor"],
    ),
    ("Agents", &["connect", "disconnect", "mcp"]),
    ("Code graph", &["graph", "pr"]),
    (
        "Data",
        &["sync", "sessions", "backup", "restore", "offboard"],
    ),
    ("Server", &["serve", "init", "login"]),
];

/// Commands only `graphicle help --all` lists.
pub const ADVANCED: &[&str] = &[
    "run",
    "jobs",
    "worker",
    "redis-worker",
    "call",
    "work-state",
    "config-example",
];

/// One line per command for the grouped help; other commands use the first
/// sentence of their own description.
const SUMMARIES: &[(&str, &str)] = &[
    (
        "setup",
        "Set up this repository: sync it, connect your agents, add hooks",
    ),
    (
        "ask",
        "Answer a question from this project's code and documents",
    ),
    (
        "context",
        "Show the evidence for a question, without a model",
    ),
    ("open", "Open the app in your browser"),
    (
        "status",
        "Show this project's sync, code graph, agents and model",
    ),
    ("doctor", "Check the installation and fix common problems"),
    (
        "connect",
        "Connect a coding agent to this project's MCP tools",
    ),
    ("disconnect", "Remove what Graphicle added to your agents"),
    ("mcp", "Serve this project's tools to an MCP client (stdio)"),
    (
        "graph",
        "Build, update and query code graphs (graphicle graph --help)",
    ),
    ("sync", "Import or update a folder and build its code graph"),
    (
        "sessions",
        "Capture, browse and forget Claude Code and Codex sessions",
    ),
    ("backup", "Back up the store without stopping the app"),
    ("restore", "Restore a backup into a new data folder"),
    (
        "offboard",
        "Remove one project's data, its backups included, with a receipt",
    ),
    ("serve", "Run the app and the HTTP MCP server"),
    (
        "init",
        "Create a workspace for a team server (new here? graphicle setup)",
    ),
    ("login", "Attach a workspace with an access key"),
    (
        "config",
        "Show or change settings, such as the answer model",
    ),
    ("update", "Show how to update Graphicle"),
    (
        "uninstall",
        "Remove Graphicle from your agents and this computer",
    ),
    (
        "run",
        "Queue a job: extraction, embeddings or a source sync",
    ),
    ("jobs", "List a project's jobs and their model calls"),
    ("worker", "Run queued jobs (serve runs a worker too)"),
    ("redis-worker", "Run an inference worker on another machine"),
    ("call", "Call a project operation with JSON arguments"),
    ("work-state", "Show the to-do lists agents recorded"),
    ("config-example", "Print a model configuration example"),
    ("document-worker", "Convert one document (used by sync)"),
    (
        "build-graph",
        "Build a project's code graph (sync --background)",
    ),
    (
        "hook",
        "Agent lifecycle hooks (installed by connect --hooks)",
    ),
];

fn summary(command: &clap::Command) -> String {
    if let Some((_, text)) = SUMMARIES
        .iter()
        .find(|(name, _)| *name == command.get_name())
    {
        return (*text).to_string();
    }
    first_sentence(
        &command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

/// The first sentence of `text`'s first line, without its full stop, at
/// most 72 characters.
fn first_sentence(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let end = line.find(". ").map_or(line.len(), |i| i + 1);
    let sentence = line[..end].trim_end_matches('.');
    if sentence.chars().count() > 72 {
        format!("{}…", sentence.chars().take(71).collect::<String>())
    } else {
        sentence.to_string()
    }
}

/// The grouped help of `command` (the program's command line): the groups
/// of [`GROUPS`], then `More` (the other visible commands, in their help
/// order), the options and where to start. With `all`, also `Advanced`
/// ([`ADVANCED`]) and `Internal` (the hidden commands).
pub fn help(command: &clap::Command, all: bool) -> String {
    let mut sections: Vec<(&str, Vec<&clap::Command>)> = Vec::new();
    let mut placed: Vec<&str> = vec!["help"];
    for (title, names) in GROUPS {
        let rows: Vec<&clap::Command> = names
            .iter()
            .filter_map(|name| command.find_subcommand(name))
            .filter(|sub| sub.get_name() != "help" && (all || !sub.is_hide_set()))
            .collect();
        placed.extend(rows.iter().map(|sub| sub.get_name()));
        if !rows.is_empty() {
            sections.push((title, rows));
        }
    }
    let mut more: Vec<&clap::Command> = command
        .get_subcommands()
        .filter(|sub| {
            !sub.is_hide_set()
                && !placed.contains(&sub.get_name())
                && !ADVANCED.contains(&sub.get_name())
        })
        .collect();
    more.sort_by_key(|sub| sub.get_display_order());
    placed.extend(more.iter().map(|sub| sub.get_name()));
    if !more.is_empty() {
        sections.push(("More", more));
    }
    if all {
        let advanced: Vec<&clap::Command> = ADVANCED
            .iter()
            .filter_map(|name| command.find_subcommand(name))
            .collect();
        placed.extend(advanced.iter().map(|sub| sub.get_name()));
        if !advanced.is_empty() {
            sections.push(("Advanced", advanced));
        }
        let internal: Vec<&clap::Command> = command
            .get_subcommands()
            .filter(|sub| !placed.contains(&sub.get_name()))
            .collect();
        if !internal.is_empty() {
            sections.push(("Internal", internal));
        }
    }
    let width = sections
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|sub| sub.get_name().len()))
        .max()
        .unwrap_or(8)
        + 2;
    let about = command
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_else(|| ABOUT.to_string());
    let mut out = format!("{about}\n\nUsage: graphicle [OPTIONS] [COMMAND]\n");
    for (title, rows) in &sections {
        let _ = writeln!(out, "\n{title}:");
        for sub in rows {
            let _ = writeln!(out, "  {:<width$}{}", sub.get_name(), summary(sub));
        }
    }
    let mut built = command.clone();
    built.build();
    let options: Vec<(String, String)> = built
        .get_arguments()
        .filter(|arg| !arg.is_hide_set() && !arg.is_positional())
        .map(|arg| {
            let mut left = arg
                .get_short()
                .map_or_else(|| "    ".to_string(), |short| format!("-{short}, "));
            if let Some(long) = arg.get_long() {
                let _ = write!(left, "--{long}");
            }
            if arg.get_action().takes_values() {
                let name = arg
                    .get_value_names()
                    .and_then(|names| names.first())
                    .map_or_else(|| arg.get_id().as_str().to_uppercase(), ToString::to_string);
                let _ = write!(left, " <{name}>");
            }
            let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
            (left, help)
        })
        .collect();
    if !options.is_empty() {
        let width = options
            .iter()
            .map(|(left, _)| left.len())
            .max()
            .unwrap_or(0)
            + 2;
        out.push_str("\nOptions:\n");
        for (left, help) in options {
            let _ = writeln!(out, "  {left:<width$}{help}");
        }
    }
    out.push('\n');
    if all {
        out.push_str("One command's options: graphicle help <command>\n");
    } else {
        out.push_str("Start here:   cd your-repo && graphicle setup\n");
        out.push_str(
            "More help:    graphicle help --all (every command), graphicle help <command>\n",
        );
    }
    out
}

/// `graphicle help [--all] [COMMAND...]`: the grouped help of `interface`
/// (the program's command line), or a command's own (the graph engine's for
/// `graph`).
pub fn help_command(interface: clap::Command, all: bool, words: &[String]) -> Result<()> {
    if words.is_empty() {
        return say(&help(&interface, all));
    }
    if words[0] == "graph" {
        let code = crate::graph_command::status(&[OsString::from("--help")]);
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    let mut line: Vec<OsString> = vec!["graphicle".into()];
    line.extend(words.iter().map(OsString::from));
    line.push("--help".into());
    // Prints the command's help (or the error for an unknown one) and exits.
    interface.get_matches_from(line);
    Ok(())
}

/// A misspelt or unknown command: clap's error with its suggestion, or,
/// when the word is another tool's name for what a command does (`search`,
/// `index`, `query`...), that command; clap's exit status.
pub fn usage_error(command: &clap::Command, error: clap::Error) -> ! {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    if error.kind() == ErrorKind::InvalidSubcommand
        && let Some(ContextValue::String(word)) = error.get(ContextKind::InvalidSubcommand)
    {
        let tip = synonym(command, word).or_else(|| {
            // clap lists its suggestions best last: the best goes first.
            let Some(ContextValue::Strings(similar)) = error.get(ContextKind::SuggestedSubcommand)
            else {
                return None;
            };
            let names: Vec<String> = similar
                .iter()
                .rev()
                .map(|name| format!("'{name}'"))
                .collect();
            match names.as_slice() {
                [] => None,
                [one] => Some(format!("a similar subcommand exists: {one}")),
                _ => Some(format!(
                    "some similar subcommands exist: {}",
                    names.join(", ")
                )),
            }
        });
        if let Some(tip) = tip {
            eprintln!(
                "error: unrecognized subcommand '{word}'\n\n  tip: {tip}\n\nUsage: graphicle [OPTIONS] [COMMAND]\n\nFor more information, try 'graphicle help'."
            );
            std::process::exit(error.exit_code());
        }
    }
    error.exit()
}

/// What to run instead of `word` when it is another tool's name for one of
/// `command`'s subcommands; only subcommands the program has are named.
pub fn synonym(command: &clap::Command, word: &str) -> Option<String> {
    let has = |name: &str| command.find_subcommand(name).is_some();
    let word = word.to_ascii_lowercase();
    let tip = match word.as_str() {
        "search" | "find" | "grep" | "query" | "q" | "explain" => {
            "graphicle context \"QUESTION\" shows the evidence, graphicle ask \"QUESTION\" answers, and graphicle graph query \"QUESTION\" searches the code graph alone"
                .to_string()
        }
        "index" | "scan" | "import" | "refresh" | "reindex" | "ingest" | "add" | "update-index" => {
            "graphicle sync updates this folder's project".to_string()
        }
        "install" | "onboard" | "configure" | "quickstart" | "start-here" if has("setup") => {
            "graphicle setup sets up this repository".to_string()
        }
        "ui" | "web" | "app" | "browse" | "dashboard" | "launch" => {
            "graphicle open opens the app".to_string()
        }
        "check" | "diagnose" | "health" | "verify" | "fix" => {
            "graphicle doctor checks the installation; graphicle doctor --fix repairs it".to_string()
        }
        "settings" | "cfg" | "conf" | "preferences" | "model" | "models" => {
            "graphicle config shows the settings; graphicle config set model anthropic sets the answer model"
                .to_string()
        }
        "upgrade" | "self-update" => "graphicle update shows how to update".to_string(),
        "version" => "graphicle --version prints the version".to_string(),
        "remove" | "unlink" | "detach" | "unconnect" if has("disconnect") => {
            "graphicle disconnect removes Graphicle from your agents".to_string()
        }
        "path" | "neighbors" | "callers" | "callees" | "god-nodes" => {
            format!("graphicle graph {word} runs the code-graph engine's command")
        }
        _ => return None,
    };
    let named = tip.split_whitespace().nth(1).unwrap_or_default();
    (has(named) || named.starts_with('-')).then_some(tip)
}

/// A command's error message with the command that fixes it, for the
/// errors people meet first. Other messages are returned unchanged.
pub fn explain(message: &str) -> String {
    match message {
        "Select --project or sync a local folder first" => {
            "This folder is not a Graphicle project yet. Run `graphicle setup` here to add it (or `graphicle sync .`), or name a project with --project.".into()
        }
        "Run graphicle init, or graphicle login, first" => {
            "Graphicle is not set up yet. In your repository, run `graphicle setup` (cd your-repo && graphicle setup).".into()
        }
        "Configure a local or API model before starting inference" => {
            "No answer model is configured. Add one with `graphicle config set model anthropic` (or openai, or ollama); `graphicle context` shows the evidence without a model.".into()
        }
        _ => message.to_string(),
    }
}

// ------------------------------------------------------------- ports, web

/// What answers on a local port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Nothing listens.
    Free,
    /// A Graphicle server, with its version.
    Graphicle(String),
    /// Another program.
    Other,
}

/// What listens on `127.0.0.1:port`: its `/health` answer names Graphicle,
/// or not. Loopback only; at most about two seconds.
pub fn probe(port: u16) -> Probe {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    if TcpStream::connect_timeout(&address, Duration::from_millis(400)).is_err() {
        return Probe::Free;
    }
    match health(port, None) {
        Some(health) if health["product"] == "Graphicle" => {
            Probe::Graphicle(s(&health, "version").to_string())
        }
        _ => Probe::Other,
    }
}

/// The `/health` answer of `127.0.0.1:port` (with `?challenge=` when
/// given), read over a plain loopback connection: no proxy, no redirect.
fn health(port: u16, challenge: Option<&str>) -> Option<Value> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(400)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let query = challenge
        .map(|challenge| format!("?challenge={challenge}"))
        .unwrap_or_default();
    let request = format!(
        "GET /health{query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = Vec::new();
    let _ = stream.take(64 * 1024).read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response);
    let body = text.split_once("\r\n\r\n").map(|(_, body)| body)?;
    serde_json::from_str::<Value>(body.trim()).ok()
}

/// The process listening on TCP `port`, from `lsof`: its PID and command.
pub fn port_holder(port: u16) -> Option<(u32, String)> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let pid = text
        .lines()
        .find_map(|line| line.strip_prefix('p'))
        .and_then(|pid| pid.parse().ok())?;
    let name = text
        .lines()
        .find_map(|line| line.strip_prefix('c'))
        .unwrap_or("unknown")
        .to_string();
    Some((pid, name))
}

/// The error for a port another program holds: who holds it, and `--port`.
pub fn port_in_use(port: u16) -> Error {
    let holder = match (probe(port), port_holder(port)) {
        (Probe::Graphicle(version), Some((pid, _))) => {
            format!(" by another Graphicle server ({version}, PID {pid})")
        }
        (Probe::Graphicle(version), None) => format!(" by another Graphicle server ({version})"),
        (_, Some((pid, name))) => format!(" by PID {pid} ({name})"),
        _ => String::new(),
    };
    let next = port.checked_add(1).unwrap_or(8855);
    Error::bad(format!(
        "Port {port} is in use{holder}. Choose another port with --port, for example `graphicle open --port {next}` or `graphicle serve --port {next}`."
    ))
}

/// A failed bind of `port`, explained when the port is taken.
pub fn bind_error(port: u16, error: std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::AddrInUse {
        port_in_use(port)
    } else {
        error.into()
    }
}

/// The record a running `graphicle serve` keeps in its data folder: its
/// port, process and secret (private to the folder's owner).
const SERVER_RECORD: &str = "server.json";

/// The proof a server gives for `challenge` with its `secret`
/// (`/health?challenge=`): neither can be worked out from the other.
pub fn server_proof(secret: &str, challenge: &str) -> String {
    crate::digest(&json!({"server_secret":secret,"challenge":challenge}))
}

/// Record the server `serve` runs for the data folder `directory` on
/// `port`, with its `secret`.
pub fn record_server(directory: &Path, port: u16, secret: &str) -> Result<()> {
    local::write_private(
        &directory.join(SERVER_RECORD),
        json!({"port":port,"pid":std::process::id(),"secret":secret})
            .to_string()
            .as_bytes(),
    )
}

/// Remove the record of the server with `secret`, when it is still that
/// server's.
pub fn forget_server(directory: &Path, secret: &str) {
    let path = directory.join(SERVER_RECORD);
    let ours = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|record| record["secret"] == secret);
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether the Graphicle server on `port` is the one `serve` runs for the
/// data folder `directory`: the folder's record names that port, and the
/// server proves it holds the record's secret for a new challenge. Nothing
/// secret is sent: a server of another data folder, or another program
/// answering as Graphicle, learns nothing.
pub fn this_folders_server(directory: &Path, port: u16) -> bool {
    let Some(record) = std::fs::read(directory.join(SERVER_RECORD))
        .ok()
        .filter(|bytes| bytes.len() <= 4096)
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return false;
    };
    let Some(secret) = record["secret"].as_str() else {
        return false;
    };
    if record["port"].as_u64() != Some(u64::from(port)) {
        return false;
    }
    let challenge = crate::id();
    health(port, Some(&challenge))
        .is_some_and(|health| health["proof"] == server_proof(secret, &challenge).as_str())
}

/// An HTTP client for this computer's own servers: never through a proxy
/// (`HTTP_PROXY` would send a request, and its credentials, elsewhere),
/// never following a redirect. It speaks plain HTTP to 127.0.0.1 only, so it
/// does not read the system's certificate authorities.
fn loopback_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .tls_built_in_native_certs(false)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| Error::bad("Cannot construct HTTP client"))
}

/// `open` with a Graphicle server already on `port`: a one-time sign-in
/// address for this workspace's app, from that server. `None` when the port
/// is free; an error when another program holds it, or a Graphicle server
/// that is not proven to be this data folder's (`directory`,
/// [`this_folders_server`]): the workspace's key is sent only to that one.
pub async fn reuse_server(
    directory: &Path,
    session: &local::Session,
    port: u16,
) -> Result<Option<String>> {
    match probe(port) {
        Probe::Free => Ok(None),
        Probe::Other => Err(port_in_use(port)),
        Probe::Graphicle(_) => {
            let base = format!("http://127.0.0.1:{port}");
            let other = || {
                Error::bad(format!(
                    "Port {port} is used by a Graphicle server of another data folder (or of an earlier version). Stop it, or open this one on another port: `graphicle open --port {}`.",
                    port.checked_add(1).unwrap_or(8855)
                ))
            };
            if !this_folders_server(directory, port) {
                return Err(other());
            }
            let response = loopback_client()?
                .post(format!(
                    "{base}/api/workspaces/{}/browser-session",
                    session.workspace
                ))
                .header("x-workspace-capability", &session.capability)
                .json(&json!({"project":session.project}))
                .send()
                .await
                .map_err(|_| other())?;
            if !response.status().is_success() {
                return Err(other());
            }
            let ticket: Value = response.json().await.map_err(|_| other())?;
            Ok(Some(format!("{base}/#session={}", s(&ticket, "ticket"))))
        }
    }
}

/// Whether no browser can be started here: [`NO_BROWSER_ENV`] is `1`, the
/// session is remote (SSH), or (Linux) there is no display.
pub fn headless() -> bool {
    headless_with(&|name| std::env::var_os(name))
}

pub fn headless_with(var: &dyn Fn(&str) -> Option<OsString>) -> bool {
    let set = |name: &str| var(name).is_some_and(|v| !v.is_empty());
    if var(NO_BROWSER_ENV).is_some_and(|v| v == "1") {
        return true;
    }
    // Windows, like macOS, always has a desktop unless the session is remote.
    if cfg!(target_os = "macos") || cfg!(windows) {
        set("SSH_CONNECTION") || set("SSH_TTY")
    } else {
        !set("DISPLAY") && !set("WAYLAND_DISPLAY")
    }
}

/// Open `url` (the app with a one-time sign-in) in a browser; print it when
/// no browser is available ([`headless`]) or none starts.
pub fn show_url(url: &str) -> Result<()> {
    let plain = url.split_once("/#").map_or(url, |(base, _)| base);
    if !headless() && local::open_url(url).is_ok() {
        eprintln!("graphicle: opened {plain} in your browser");
        return Ok(());
    }
    let mut text = format!(
        "Open this address in your browser (it signs you in once, within 3 minutes):\n  {url}"
    );
    if let Some(port) = ssh_port(url) {
        let _ = write!(
            text,
            "\nOver SSH, forward the port first, from your own computer: ssh -L {port}:127.0.0.1:{port} <this host>"
        );
    }
    say(&text)
}

/// The port of a loopback `url` when this session is remote (SSH), so
/// `open` can say how to reach it.
fn ssh_port(url: &str) -> Option<u16> {
    let remote = ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()));
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    remote.then(|| digits.parse().ok()).flatten()
}

// ----------------------------------------------------------------- update

/// How this program was installed, and the commands that update it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    /// `homebrew`, `script`, `cargo`, `source` or `docker`.
    pub method: &'static str,
    /// The method in words.
    pub how: String,
    /// The commands to run, in order.
    pub commands: Vec<String>,
}

/// The install method of the program at `exe`: Homebrew (a Cellar or a
/// Homebrew prefix), Docker (`docker`), a source build (a Cargo `target`
/// folder), `cargo install` (`cargo_bin`), else the install script into the
/// program's folder.
pub fn install_method(
    exe: &Path,
    home: Option<&Path>,
    cargo_bin: Option<&Path>,
    docker: bool,
) -> Install {
    let text = exe.to_string_lossy();
    let folder = exe.parent().unwrap_or(Path::new("."));
    if text.contains("/Cellar/")
        || text.starts_with("/opt/homebrew/")
        || text.starts_with("/home/linuxbrew/")
        || text.starts_with("/usr/local/Homebrew/")
    {
        return Install {
            method: "homebrew",
            how: "with Homebrew".into(),
            commands: vec!["brew update".into(), "brew upgrade graphicle".into()],
        };
    }
    if docker {
        return Install {
            method: "docker",
            how: "in a container".into(),
            commands: vec!["docker compose pull".into(), "docker compose up -d".into()],
        };
    }
    let components: Vec<_> = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let cargo_target = folder
        .file_name()
        .is_some_and(|profile| profile == "release" || profile == "debug")
        && folder
            .parent()
            .is_some_and(|target| target.join("CACHEDIR.TAG").is_file());
    // Windows PowerShell 5.1 has no `&&`.
    let then = if cfg!(windows) { ";" } else { " &&" };
    if cargo_target && components.iter().rposition(|c| c == "target").is_none() {
        return Install {
            method: "source",
            how: format!("from source ({})", tilde(folder)),
            commands: vec![
                "git pull   # in your clone of graphicle-ai/graphicle".into(),
                format!("cd runtime{then} cargo build --release --locked"),
            ],
        };
    }
    if let Some(at) = components.iter().rposition(|c| c == "target")
        && components
            .get(at + 1)
            .is_some_and(|profile| profile == "release" || profile == "debug")
    {
        let target: PathBuf = exe.components().take(at + 1).collect();
        let crate_dir = target.parent().unwrap_or(Path::new("."));
        // PowerShell does not expand `~` in a program's arguments.
        let place = if cfg!(windows) {
            powershell_quote(&crate_dir.to_string_lossy())
        } else {
            shell_quote(&tilde(crate_dir))
        };
        return Install {
            method: "source",
            how: format!("from source ({})", tilde(crate_dir)),
            commands: vec![
                format!("git -C {place} pull"),
                format!("cd {place}{then} cargo build --release --locked"),
            ],
        };
    }
    if cargo_bin.is_some_and(|bin| folder == bin) {
        return Install {
            method: "cargo",
            how: format!("with cargo ({})", tilde(folder)),
            commands: vec![
                "git pull   # in your clone of graphicle-ai/graphicle".into(),
                "cargo install --path runtime --locked --force".into(),
            ],
        };
    }
    // The install script's default folder: `~/.local/bin` for install.sh,
    // `%LOCALAPPDATA%\Programs\Graphicle\bin` for install.ps1.
    #[cfg(not(windows))]
    let default_bin = home.map(|home| home.join(".local/bin"));
    #[cfg(windows)]
    let default_bin = {
        let _ = home;
        crate::platform::local_app_data().map(|local| local.join(r"Programs\Graphicle\bin"))
    };
    Install {
        method: "script",
        how: format!("with the install script ({})", tilde(folder)),
        commands: vec![script_command(
            folder,
            default_bin.as_deref(),
            cfg!(windows),
        )],
    }
}

/// The install script's command that updates the program in `folder`:
/// install.sh through `sh` on macOS and Linux; on Windows (`powershell`)
/// install.ps1 through PowerShell, with `-InstallDir` unless `folder` is
/// the script's default.
fn script_command(folder: &Path, default_bin: Option<&Path>, powershell: bool) -> String {
    const RELEASES: &str = "https://github.com/graphicle-ai/graphicle/releases/latest/download";
    let default = default_bin == Some(folder);
    if powershell {
        let script = format!("irm {RELEASES}/install.ps1");
        return if default {
            format!("{script} | iex")
        } else {
            format!(
                "& ([scriptblock]::Create(({script}))) -InstallDir {}",
                powershell_quote(&folder.to_string_lossy())
            )
        };
    }
    let script = format!("curl -fsSL {RELEASES}/install.sh");
    if default {
        format!("{script} | sh")
    } else {
        format!(
            "{script} | sh -s -- --bin-dir {}",
            shell_quote(&folder.to_string_lossy())
        )
    }
}

/// `value` as a PowerShell literal string: single quotes, a quote doubled.
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `value` quoted for a POSIX shell when it needs quoting.
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-~+:@%".contains(c))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// [`install_method`] of the program at `exe` (canonical) in this
/// environment: its home folder, Cargo's `bin` folder, and whether it runs
/// in a container. `graphicle update` and `graphicle uninstall` both use it.
pub fn installed(exe: &Path) -> Install {
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cargo")))
        .map(|cargo| cargo.join("bin"));
    install_method(
        exe,
        home_dir().as_deref(),
        cargo_bin.as_deref(),
        Path::new("/.dockerenv").exists(),
    )
}

/// `graphicle update` without a release feed: how to update this program
/// for the way it was installed. No network call.
pub fn update_instructions() -> Value {
    let exe = std::env::current_exe()
        .ok()
        .map(|exe| canonical(&exe))
        .unwrap_or_default();
    let install = installed(&exe);
    json!({"version":env!("CARGO_PKG_VERSION"),"program":exe,"installed":install.method,
        "how":install.how,"update":install.commands,"then":"graphicle doctor --fix",
        "feed":"graphicle update --feed URL --public-key KEY checks a signed release feed","network":false})
}

/// [`update_instructions`] for people.
pub fn update_text(value: &Value, style: Style) -> String {
    let mut out = format!(
        "Graphicle {}, installed {}.\n\nTo update:\n",
        s(value, "version"),
        s(value, "how")
    );
    for command in value["update"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "  {}",
            style.bold(command.as_str().unwrap_or_default())
        );
    }
    out.push_str("\nThen run `graphicle doctor --fix`, so your agents use the new program.\n");
    let _ = write!(
        out,
        "{}",
        style.dim(
            "To check a signed release feed instead: graphicle update --feed URL --public-key KEY"
        )
    );
    out
}

// ------------------------------------------------------------ data folder

/// The data folder's version stamp, written by the command line.
const STAMP: &str = "version.json";

/// `major.minor.patch` as numbers (a pre-release suffix ignored).
fn version_parts(version: &str) -> (u64, u64, u64) {
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// The version of Graphicle that last used the data folder `directory`,
/// from its stamp.
pub fn data_version(directory: &Path) -> Option<String> {
    let bytes = std::fs::read(directory.join(STAMP)).ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value["graphicle"].as_str().map(str::to_string)
}

/// After the command line opens the store in `directory`: a note when the
/// folder was last used by a newer Graphicle; else the stamp is brought up
/// to this version (written only when it is missing or older).
pub fn note_data_version(directory: &Path) -> Option<String> {
    let ours = env!("CARGO_PKG_VERSION");
    match data_version(directory) {
        Some(recorded) if version_parts(&recorded) > version_parts(ours) => Some(format!(
            "this data folder was last used by Graphicle {recorded}, newer than this program ({ours}). If a command fails, update Graphicle (`graphicle update` shows how) or use another --data-dir."
        )),
        Some(recorded) if recorded == ours => None,
        _ => {
            let _ = local::write_private(
                &directory.join(STAMP),
                json!({"graphicle":ours}).to_string().as_bytes(),
            );
            None
        }
    }
}

/// Free bytes on the file system of `path` (or its nearest existing
/// ancestor).
pub fn free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
        // SAFETY: `statvfs` writes only into `stat`, which lives across the
        // call, and reads the NUL-terminated `name`.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        // The field types differ between systems (u32 or u64).
        #[allow(clippy::unnecessary_cast)]
        let free = stat.f_bavail as u64 * stat.f_frsize as u64;
        Some(free)
    }
    #[cfg(windows)]
    {
        let folder = if existing.is_dir() {
            existing
        } else {
            existing.parent()?
        };
        crate::platform::free_bytes(folder)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = existing;
        None
    }
}

/// Whether a file can be created in `directory`.
fn writable(directory: &Path) -> bool {
    let probe = directory.join(format!(".graphicle-doctor-{}", crate::id()));
    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    let _ = std::fs::remove_file(&probe);
    made
}

// ---------------------------------------------------------------- clients

/// Where a Graphicle entry stands in its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    /// It runs `graphicle … mcp`.
    Runs,
    /// Setup or connect wrote it, and it no longer runs `graphicle … mcp`
    /// (edited by hand, or its arguments lost `mcp`).
    NotGraphicle,
    /// Setup or connect wrote it, and it is no longer in the file.
    Gone,
}

/// A Graphicle entry in a coding client's MCP configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientEntry {
    /// `claude-code`, `vscode`, `cursor`, `claude` (Claude Desktop) or `codex`.
    pub client: &'static str,
    /// The configuration file.
    pub file: PathBuf,
    /// The entry's name.
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub presence: Presence,
    /// What setup or connect recorded when it wrote the entry, when it did
    /// (this data folder's `agent-changes.json`).
    pub recorded: Option<crate::agent_files::Change>,
}

impl ClientEntry {
    fn after(&self, flag: &str) -> Option<&str> {
        let at = self.args.iter().position(|arg| arg == flag)?;
        self.args.get(at + 1).map(String::as_str)
    }
    /// The store the entry's `graphicle mcp` opens.
    pub fn data_dir(&self) -> PathBuf {
        self.after("--data-dir")
            .map(PathBuf::from)
            .unwrap_or_else(local::default_data_dir)
    }
    /// Its `--project`: `.` or `${workspaceFolder}` (the folder the client
    /// starts it in), a project id, or none (the saved default).
    pub fn project(&self) -> Option<&str> {
        self.after("--project")
    }
    pub fn work_state(&self) -> bool {
        self.args.iter().any(|arg| arg == "--work-state")
    }
    /// Whether the entry lets the agent propose memory drafts for review
    /// (`--allow-learning`; setup and `connect --graph` add it to an entry of
    /// the whole server).
    pub fn allow_learning(&self) -> bool {
        self.args.iter().any(|arg| arg == "--allow-learning")
    }
    /// The tools the entry names with `--tools`; an entry without it serves
    /// the whole server.
    pub fn tools(&self) -> Option<crate::api::ToolSet> {
        self.after("--tools")
            .and_then(|set| crate::api::ToolSet::parse(set).ok())
    }
    /// For an entry that follows its folder: the folder it is written in.
    pub fn folder(&self) -> Option<PathBuf> {
        let parent = self.file.parent()?;
        match self.client {
            "claude-code" => Some(parent.to_path_buf()),
            "vscode" => parent.parent().map(Path::to_path_buf),
            _ => None,
        }
    }
    /// The client's name for people.
    pub fn label(&self) -> &'static str {
        match self.client {
            "claude-code" => "Claude Code",
            "vscode" => "VS Code",
            "cursor" => "Cursor",
            "claude" => "Claude Desktop",
            "codex" => "Codex",
            _ => "client",
        }
    }
}

/// Where Codex keeps its configuration: `CODEX_HOME`, else `~/.codex`.
fn codex_home(home: &Path) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
}

/// Claude Desktop's configuration file under `home`.
fn claude_desktop_file(home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        crate::platform::claude_desktop_dir(
            crate::platform::roaming_app_data().as_deref(),
            crate::platform::local_app_data().as_deref(),
        )
        .unwrap_or_else(|| home.join(r"AppData\Roaming\Claude"))
        .join("claude_desktop_config.json")
    }
    #[cfg(not(windows))]
    {
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Claude/claude_desktop_config.json")
        } else {
            home.join(".config/Claude/claude_desktop_config.json")
        }
    }
}

/// Whether an MCP server entry runs `graphicle mcp`.
fn is_graphicle(command: &str, args: &[String]) -> bool {
    Path::new(command)
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name == "graphicle" || name == "graphicle.exe")
        && args.iter().any(|arg| arg == "mcp")
}

/// A JSON configuration file (comments and trailing commas allowed), when
/// it exists and is a JSON object of at most 1 MiB.
fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    crate::json_edit::parse(std::str::from_utf8(&bytes).ok()?).filter(Value::is_object)
}

/// The JSON client files `connect` writes, with the object that holds the
/// entries: Claude Code (`.mcp.json`) and VS Code (`.vscode/mcp.json`) in
/// `folder`, and Cursor and Claude Desktop under `home`.
fn client_files(folder: Option<&Path>, home: &Path) -> Vec<(&'static str, PathBuf, &'static str)> {
    let mut files: Vec<(&'static str, PathBuf, &'static str)> = Vec::new();
    if let Some(folder) = folder {
        files.push(("claude-code", folder.join(".mcp.json"), "mcpServers"));
        files.push(("vscode", folder.join(".vscode/mcp.json"), "servers"));
    }
    files.push(("cursor", home.join(".cursor/mcp.json"), "mcpServers"));
    files.push(("claude", claude_desktop_file(home), "mcpServers"));
    files
}

/// The recorded change that wrote entry `name` of `file`.
fn recorded_entry(
    recorded: &[crate::agent_files::Change],
    file: &Path,
    name: &str,
) -> Option<crate::agent_files::Change> {
    recorded
        .iter()
        .find(|change| {
            matches!(change.kind.as_str(), "mcp" | "codex")
                && change.path == file
                && change.entry.as_deref() == Some(name)
        })
        .cloned()
}

/// Every Graphicle entry in the MCP configuration of the clients `connect`
/// writes (Claude Code and VS Code in `folder`; Cursor, Claude Desktop and
/// Codex under `home`): the entries that run `graphicle mcp`, and the ones
/// this data folder's record (`recorded`) says Graphicle wrote, whatever
/// they run now and whether or not they are still there.
pub fn client_entries(
    folder: Option<&Path>,
    home: &Path,
    recorded: &[crate::agent_files::Change],
) -> Vec<ClientEntry> {
    let mut entries = Vec::new();
    let mut add =
        |client: &'static str, file: &Path, name: &str, command: String, args: Vec<String>| {
            let record = recorded_entry(recorded, file, name);
            let presence = if is_graphicle(&command, &args) {
                Presence::Runs
            } else if record.is_some() {
                Presence::NotGraphicle
            } else {
                return;
            };
            entries.push(ClientEntry {
                client,
                file: file.to_path_buf(),
                name: name.to_string(),
                command,
                args,
                presence,
                recorded: record,
            });
        };
    let mut seen: Vec<(PathBuf, String)> = Vec::new();
    for (client, file, group) in client_files(folder, home) {
        let Some(document) = read_json(&file) else {
            continue;
        };
        for (name, server) in document[group].as_object().into_iter().flatten() {
            let command = s(server, "command").to_string();
            let args: Vec<String> = server["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|arg| arg.as_str().map(str::to_string))
                .collect();
            seen.push((file.clone(), name.clone()));
            add(client, &file, name, command, args);
        }
    }
    let codex = codex_home(home).join("config.toml");
    if let Ok(text) = std::fs::read_to_string(&codex) {
        for (name, command, args) in codex_servers(&text) {
            seen.push((codex.clone(), name.clone()));
            add("codex", &codex, &name, command, args);
        }
    }
    // Entries Graphicle wrote that are no longer there, in files that are
    // still readable (an unreadable file is reported on its own).
    let mut files: Vec<(&'static str, PathBuf)> = client_files(folder, home)
        .into_iter()
        .map(|(client, file, _)| (client, file))
        .collect();
    files.push(("codex", codex.clone()));
    for (client, file) in files {
        let readable = !file.exists() || read_json(&file).is_some() || file == codex;
        for change in recorded
            .iter()
            .filter(|change| change.path == file && matches!(change.kind.as_str(), "mcp" | "codex"))
        {
            let Some(name) = change.entry.as_deref() else {
                continue;
            };
            if readable
                && !seen
                    .iter()
                    .any(|(known, entry)| *known == file && entry == name)
            {
                entries.push(ClientEntry {
                    client,
                    file: file.clone(),
                    name: name.to_string(),
                    command: String::new(),
                    args: Vec::new(),
                    presence: Presence::Gone,
                    recorded: Some(change.clone()),
                });
            }
        }
    }
    entries
}

/// The client files that exist but cannot be read as JSON, when Graphicle
/// wrote to them (`recorded`) or they name it: `(client, file, where the
/// JSON breaks)`. Their entries cannot start.
pub fn unreadable_client_files(
    folder: Option<&Path>,
    home: &Path,
    recorded: &[crate::agent_files::Change],
) -> Vec<(&'static str, PathBuf, String)> {
    let mut found = Vec::new();
    for (client, file, _) in client_files(folder, home) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if crate::json_edit::parse(&text).is_some_and(|value| value.is_object()) {
            continue;
        }
        if recorded.iter().any(|change| change.path == file) || text.contains("graphicle") {
            found.push((client, file, crate::json_edit::error_place(&text)));
        }
    }
    found
}

/// The `[mcp_servers.NAME]` tables of a Codex `config.toml`, with their
/// `command` and `args` (basic or literal strings; `args` may span lines).
/// Enough of TOML for what `codex mcp add` writes; other tables are skipped.
pub fn codex_servers(text: &str) -> Vec<(String, String, Vec<String>)> {
    let mut servers = Vec::new();
    let mut current: Option<(String, Option<String>, Vec<String>)> = None;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if let Some((name, Some(command), args)) = current.take() {
                servers.push((name, command, args));
            }
            let header = trimmed.trim_start_matches('[');
            let header = header.split(']').next().unwrap_or_default().trim();
            current = header
                .strip_prefix("mcp_servers.")
                .and_then(table_name)
                .map(|name| (name, None, Vec::new()));
            continue;
        }
        let Some((_, command, args)) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        match key.trim() {
            "command" => *command = toml_strings(value).into_iter().next(),
            "args" => {
                let mut value = value.to_string();
                while !array_closed(&value) {
                    let Some(next) = lines.next() else {
                        break;
                    };
                    value.push('\n');
                    value.push_str(next);
                }
                *args = toml_strings(&value);
            }
            _ => {}
        }
    }
    if let Some((name, Some(command), args)) = current {
        servers.push((name, command, args));
    }
    servers
}

/// A table name after `mcp_servers.`: one bare or quoted key; a deeper
/// table (`mcp_servers.x.env`) is not a server.
fn table_name(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if let Some(quoted) = rest.strip_prefix('"') {
        let (name, tail) = quoted.split_once('"')?;
        return tail.trim().is_empty().then(|| name.to_string());
    }
    if let Some(quoted) = rest.strip_prefix('\'') {
        let (name, tail) = quoted.split_once('\'')?;
        return tail.trim().is_empty().then(|| name.to_string());
    }
    (!rest.contains('.') && !rest.is_empty()).then(|| rest.to_string())
}

/// Whether the TOML value starting in `text` is complete: an array has
/// closed (strings and comments skipped), or it is not an array.
fn array_closed(text: &str) -> bool {
    let (mut depth, mut opened, mut quote, mut escaped, mut comment) =
        (0i32, false, None::<char>, false, false);
    for c in text.chars() {
        if comment {
            comment = c != '\n';
            continue;
        }
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '#' => comment = true,
                '[' => {
                    depth += 1;
                    opened = true;
                }
                ']' => {
                    depth -= 1;
                    if depth <= 0 {
                        return true;
                    }
                }
                _ => {}
            },
        }
    }
    !opened
}

/// The string values in a TOML value, in order: basic strings with their
/// escapes, and literal strings; comments are skipped.
fn toml_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '#' => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '\'' => {
                let mut value = String::new();
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    value.push(n);
                }
                out.push(value);
            }
            '"' => {
                let mut value = String::new();
                while let Some(n) = chars.next() {
                    match n {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => value.push('\n'),
                            Some('t') => value.push('\t'),
                            Some('r') => value.push('\r'),
                            Some(digits @ ('u' | 'U')) => {
                                let width = if digits == 'u' { 4 } else { 8 };
                                let hex: String = chars.by_ref().take(width).collect();
                                if let Some(decoded) =
                                    u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                                {
                                    value.push(decoded);
                                }
                            }
                            Some(other) => value.push(other),
                            None => break,
                        },
                        _ => value.push(n),
                    }
                }
                out.push(value);
            }
            _ => {}
        }
    }
    out
}

/// The program a configured command runs: an absolute path that exists, or
/// a bare name found on `PATH`.
pub fn resolve_program(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() {
        return runnable(path).then(|| path.to_path_buf());
    }
    if command.contains('/') || (cfg!(windows) && command.contains('\\')) {
        return None;
    }
    let search = std::env::var_os("PATH")?;
    // On Windows, with each `PATHEXT` extension.
    let names = crate::platform::program_names(command);
    std::env::split_paths(&search)
        .filter(|folder| folder.is_absolute())
        .flat_map(|folder| names.iter().map(move |name| folder.join(name)))
        .find(|candidate| runnable(candidate))
}

fn runnable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// How a Graphicle client entry stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryState {
    Ok,
    /// Its program does not exist (an upgrade moved it, or it was deleted).
    MissingProgram,
    /// It runs another `graphicle` than this one.
    OtherProgram(PathBuf),
    /// Its project no longer resolves.
    MissingProject,
    /// It opens another data folder than this command's.
    OtherStore(PathBuf),
    /// Graphicle wrote it, and it no longer runs `graphicle … mcp`.
    NotGraphicle,
    /// Graphicle wrote it, and it is no longer in its file.
    Gone,
}

/// A client entry, checked against this program and store.
#[derive(Clone, Debug)]
pub struct EntryCheck {
    pub entry: ClientEntry,
    pub state: EntryState,
    /// The project it serves, when it resolves.
    pub serves: Option<String>,
}

/// Check `entries` against the program `exe` and the store: which project
/// each serves, and whether it would start.
pub fn check_entries(
    store: &Store,
    session: &local::Session,
    entries: Vec<ClientEntry>,
    exe: &Path,
) -> Vec<EntryCheck> {
    let ours = canonical(&store.directory);
    let exe = canonical(exe);
    entries
        .into_iter()
        .map(|entry| {
            // An entry this data folder's record holds is this store's,
            // whatever its arguments say now.
            let recorded_project = entry
                .recorded
                .as_ref()
                .and_then(|change| change.project.clone());
            match entry.presence {
                Presence::NotGraphicle => {
                    return EntryCheck {
                        state: EntryState::NotGraphicle,
                        entry,
                        serves: recorded_project,
                    };
                }
                Presence::Gone => {
                    return EntryCheck {
                        state: EntryState::Gone,
                        entry,
                        serves: recorded_project,
                    };
                }
                Presence::Runs => {}
            }
            let data = canonical(&entry.data_dir());
            if data != ours {
                return EntryCheck {
                    state: EntryState::OtherStore(data),
                    entry,
                    serves: None,
                };
            }
            let serves = match entry.project() {
                Some("." | "${workspaceFolder}") => entry.folder().and_then(|folder| {
                    store
                        .project_for_path(&session.workspace, &session.capability, &folder)
                        .ok()
                        .flatten()
                        .map(|found| found.id)
                }),
                Some(value) => store
                    .find_project(
                        &session.workspace,
                        &session.capability,
                        Some(value),
                        None,
                        None,
                    )
                    .ok()
                    .flatten()
                    .map(|resolved| resolved.id),
                None => session.project.clone(),
            };
            let state = match resolve_program(&entry.command) {
                None => EntryState::MissingProgram,
                Some(_) if serves.is_none() => EntryState::MissingProject,
                Some(program) if canonical(&program) != exe => EntryState::OtherProgram(program),
                Some(_) => EntryState::Ok,
            };
            EntryCheck {
                entry,
                state,
                serves,
            }
        })
        .collect()
}

/// Whether the Codex command line is on `PATH`. With it, Codex manages its
/// own entries; without it, `connect` writes Graphicle's table into Codex's
/// `config.toml` itself.
fn codex_cli() -> bool {
    resolve_program("codex").is_some()
}

/// Rewrite a broken client entry with this program, through `connect` (its
/// name, project, tools, work-state and learning tools and folder kept): one whose
/// program is gone, one Graphicle wrote that no longer runs `graphicle … mcp`,
/// or one it wrote that is gone from its file (from the record). A Codex
/// entry is rewritten only when `connect` wrote it into `config.toml` (no
/// Codex command line): Codex manages the entries it added itself.
pub fn repair_entry(store: &Store, check: &EntryCheck) -> Result<Value> {
    let entry = &check.entry;
    if let Some(change) = entry
        .recorded
        .as_ref()
        .filter(|_| matches!(check.state, EntryState::NotGraphicle | EntryState::Gone))
    {
        if entry.client == "codex" && codex_cli() {
            return Err(Error::bad(
                "Codex manages its own MCP entries: run `codex mcp remove` for it, then `graphicle connect codex`",
            ));
        }
        let cwd = match &change.folder {
            Some(folder) if folder.is_dir() => folder.clone(),
            _ => std::env::current_dir()?,
        };
        let project_scope = matches!(entry.client, "claude-code" | "vscode");
        return crate::agent_setup::connect_with(
            store,
            &crate::agent_setup::Connect {
                client: entry.client,
                config_path: None,
                project: if project_scope {
                    None
                } else {
                    change.project.as_deref()
                },
                name: Some(entry.name.as_str()),
                replace: entry.client != "codex",
                hooks: false,
                work_state: entry.work_state(),
                graph: false,
                allow_learning: entry.allow_learning(),
                // The set it names; an entry that names none is the whole
                // server (0.1.0's entries, `setup --tools all`), and so is
                // one that is gone, since nothing is left to say otherwise.
                tools: Some(entry.tools().unwrap_or_default()),
                guards: false,
                cwd: &cwd,
            },
            crate::agent_setup::Policy {
                refresh_ours: true,
                drop_guards: false,
            },
        );
    }
    if entry.client == "codex" {
        if codex_cli() {
            return Err(Error::bad(
                "Codex manages its own MCP entries: run `codex mcp remove` for it, then `graphicle connect codex`",
            ));
        }
        let cwd = std::env::current_dir()?;
        return crate::agent_setup::connect_with(
            store,
            &crate::agent_setup::Connect {
                client: "codex",
                config_path: None,
                project: entry.project(),
                name: Some(entry.name.as_str()),
                replace: false,
                hooks: false,
                work_state: entry.work_state(),
                graph: false,
                allow_learning: entry.allow_learning(),
                tools: Some(entry.tools().unwrap_or_default()),
                guards: false,
                cwd: &cwd,
            },
            crate::agent_setup::Policy {
                refresh_ours: true,
                drop_guards: false,
            },
        );
    }
    let cwd = match entry.folder() {
        Some(folder) => folder,
        None => std::env::current_dir()?,
    };
    let project = match entry.project() {
        Some("." | "${workspaceFolder}") | None => None,
        Some(id) => Some(id),
    };
    crate::agent_setup::connect(
        store,
        &crate::agent_setup::Connect {
            client: entry.client,
            config_path: None,
            project,
            name: Some(entry.name.as_str()),
            replace: true,
            hooks: false,
            work_state: entry.work_state(),
            graph: false,
            allow_learning: entry.allow_learning(),
            tools: Some(entry.tools().unwrap_or_default()),
            guards: false,
            cwd: &cwd,
        },
    )
}

// ------------------------------------------------------------------ hooks

/// The capture hooks of one data folder in a client's hooks file
/// (`connect --hooks`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureHook {
    /// `claude-code` or `codex`.
    pub client: &'static str,
    pub file: PathBuf,
    /// Whether every program they run exists.
    pub runs: bool,
    /// The data folder they import into.
    pub data_dir: Option<PathBuf>,
    /// The events they are set for.
    pub events: Vec<String>,
}

impl CaptureHook {
    /// The events setup sets hooks for that these lack: those sessions, or
    /// those moments of them, are not captured.
    pub fn missing(&self) -> Vec<&'static str> {
        capture_events(self.client)
            .iter()
            .copied()
            .filter(|event| !self.events.iter().any(|have| have == event))
            .collect()
    }
}

/// The events `connect --hooks` sets capture hooks for.
pub fn capture_events(client: &str) -> &'static [&'static str] {
    match client {
        "claude-code" => &["SessionStart", "Stop", "PreCompact", "SessionEnd"],
        "codex" => &["SessionStart", "Stop", "SessionEnd"],
        _ => &[],
    }
}

/// The words of a shell command, unquoted as `sh` would (`'...'`, `"..."`
/// and `\x`): enough for the hook commands `connect` writes.
pub(crate) fn words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    word.push(n);
                }
            }
            '"' => {
                started = true;
                while let Some(n) = chars.next() {
                    match n {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        _ => word.push(n),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// The first word of a shell command ([`words`]).
fn first_word(command: &str) -> String {
    words(command).into_iter().next().unwrap_or_default()
}

/// The capture hooks of Claude Code (`.claude/settings.local.json` in
/// `folder`) and Codex (`hooks.json` in its home). Claude Code's guard hooks
/// are not among them ([`guard_hooks`]).
pub fn capture_hooks(folder: Option<&Path>, home: &Path) -> Vec<CaptureHook> {
    graphicle_hooks(folder, Some(home), false)
}

/// Claude Code's guard hooks in `.claude/settings.local.json` in `folder`
/// (setup and `connect --graph` add them), one record per data
/// folder; their `events` are the hook arguments found.
pub fn guard_hooks(folder: Option<&Path>) -> Vec<CaptureHook> {
    graphicle_hooks(folder, None, true)
}

/// The capture hooks (`guard` false) or the guard hooks (`guard` true) of
/// the clients' hooks files: Claude Code's in `folder`, and Codex's in
/// `home` for capture hooks.
fn graphicle_hooks(folder: Option<&Path>, home: Option<&Path>, guard: bool) -> Vec<CaptureHook> {
    let mut files: Vec<(&'static str, PathBuf)> = Vec::new();
    if let Some(folder) = folder {
        files.push(("claude-code", folder.join(".claude/settings.local.json")));
    }
    if let Some(home) = home.filter(|_| !guard) {
        files.push(("codex", codex_home(home).join("hooks.json")));
    }
    let mut found: Vec<CaptureHook> = Vec::new();
    for (client, file) in files {
        let Some(document) = read_json(&file) else {
            continue;
        };
        let ours = format!(" hook {client} ");
        let is_guard = |command: &str| {
            crate::agent_setup::GUARD_ARGUMENTS
                .iter()
                .any(|argument| command.ends_with(&format!("{ours}{argument}")))
        };
        for (event, groups) in document["hooks"].as_object().into_iter().flatten() {
            for command in groups
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|group| group["hooks"].as_array())
                .flatten()
                .filter_map(|hook| hook["command"].as_str())
                .filter(|command| command.contains(&ours) && is_guard(command) == guard)
            {
                // A guard hook is named by its argument, a capture hook by
                // its event.
                let event = if guard {
                    command.rsplit(' ').next().unwrap_or(event).to_string()
                } else {
                    event.clone()
                };
                let event = &event;
                let words = words(command);
                let data_dir = words
                    .iter()
                    .position(|w| w == "--data-dir")
                    .and_then(|at| words.get(at + 1))
                    .map(PathBuf::from);
                let runs = resolve_program(&first_word(command)).is_some();
                // One record per file and data folder.
                match found
                    .iter_mut()
                    .find(|hook| hook.file == file && hook.data_dir == data_dir)
                {
                    Some(hook) => {
                        hook.runs &= runs;
                        if !hook.events.contains(event) {
                            hook.events.push(event.clone());
                        }
                    }
                    None => found.push(CaptureHook {
                        client,
                        file: file.clone(),
                        runs,
                        data_dir,
                        events: vec![event.clone()],
                    }),
                }
            }
        }
    }
    found
}

/// The Git hooks that keep the project of `folder` fresh: `post-commit`,
/// `post-merge` and `post-checkout` holding setup's block for this folder
/// ([`crate::git_hooks`]) or a line of their own running `graphicle ...
/// sync`. `None` outside a Git repository.
pub fn git_hooks(folder: &Path) -> Option<Vec<String>> {
    // Git refuses Windows' verbatim `\\?\` paths.
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(crate::platform::simplified(folder.to_path_buf()))
        .args(["rev-parse", "--git-path", "hooks"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hooks = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let hooks = if hooks.is_absolute() {
        hooks
    } else {
        folder.join(hooks)
    };
    let folder = canonical(folder);
    Some(
        crate::git_hooks::HOOKS
            .into_iter()
            .filter(|name| {
                std::fs::read_to_string(hooks.join(name))
                    .is_ok_and(|text| hook_syncs(&text, &folder))
            })
            .map(str::to_string)
            .collect(),
    )
}

/// Whether a hook's `text` keeps `folder` fresh: setup's block for that
/// folder, or, outside setup's blocks (which name their own folders), a
/// line of its own that runs `graphicle ... sync`.
fn hook_syncs(text: &str, folder: &Path) -> bool {
    if crate::git_hooks::folders(text)
        .iter()
        .any(|block| canonical(block) == folder)
    {
        return true;
    }
    let own = crate::git_hooks::remove(text, None).unwrap_or_else(|| text.to_string());
    own.lines().any(|line| {
        let line = line.trim_start();
        !line.starts_with('#') && line.contains("graphicle") && line.contains("sync")
    })
}

/// The top folder of the Git repository holding `folder`, by its `.git`.
pub fn git_root(folder: &Path) -> Option<PathBuf> {
    folder
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

// ------------------------------------------------------------------ model

/// The answer model as configured: the default profile of the
/// configuration file and the workspace's own profiles (the app's).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelState {
    /// The profile's name.
    pub name: String,
    pub protocol: String,
    pub model: String,
    pub base_url: String,
    pub key_env: Option<String>,
    /// Whether its key is available (never the key).
    pub key_available: bool,
    pub input_limit: usize,
    pub output_limit: usize,
}

/// The configured answer model, `None` when there is none. With a store,
/// the workspace's profiles count too, and a key may be in its vault.
pub fn model_state(
    store: Option<(&Store, &local::Session)>,
    config: &model::Config,
) -> Option<ModelState> {
    let effective = match store {
        Some((store, session)) => crate::settings::configuration(store, &session.workspace, config)
            .unwrap_or_else(|_| config.clone()),
        None => config.clone(),
    };
    let name = effective.default_model.clone()?;
    let profile = effective.models.get(&name)?;
    let key_available = match (&profile.key_env, store) {
        (None, _) => true,
        (Some(key), Some((store, session))) => {
            store.resolve_credential(&session.workspace, key).is_ok()
        }
        (Some(key), None) => std::env::var(key).is_ok_and(|v| !v.is_empty()),
    };
    Some(ModelState {
        name,
        protocol: profile.protocol.clone(),
        model: profile.model.clone(),
        base_url: profile.base_url.clone(),
        key_env: profile.key_env.as_deref().map(shown_key_env),
        key_available,
        input_limit: profile.input_limit,
        output_limit: profile.output_limit,
    })
}

/// The answer model of the data folder `directory`: [`model_state`] with
/// the workspace's profiles when its store exists (nothing is created).
pub fn configured_model(directory: &Path, config: &model::Config) -> Option<ModelState> {
    match existing_store(directory) {
        Some(Ok((store, Some(session)))) => model_state(Some((&store, &session)), config),
        _ => model_state(None, config),
    }
}

/// Whether `ask` has no answer model to call.
pub fn no_answer_model(store: &Store, session: &local::Session, config: &model::Config) -> bool {
    crate::settings::configuration(store, &session.workspace, config)
        .is_ok_and(|effective| effective.default_model.is_none())
}

/// The answer model in one line, for `status` and `config`.
pub fn model_line(model: Option<&ModelState>) -> String {
    match model {
        None => "none: ask shows the evidence only (graphicle config set model anthropic)".into(),
        Some(m) => {
            let key = match &m.key_env {
                None => String::new(),
                Some(key) if m.key_available => format!(", key from {key}"),
                Some(key) => format!(", {key} is not set"),
            };
            format!("{} · {} ({}){key}", m.name, m.model, m.protocol)
        }
    }
}

/// The Claude model `config set model anthropic` chooses. Its profile asks
/// for JSON in the prompt (`structured_output: false`): the Anthropic
/// transport's structured mode forces `tool_choice`, which Claude Opus 5.5
/// does not accept.
pub const ANTHROPIC_MODEL: &str = "claude-opus-5-5";
/// The OpenAI model `config set model openai` chooses.
pub const OPENAI_MODEL: &str = "gpt-5-mini";
/// The Ollama model `config set model ollama` chooses when none is pulled.
pub const OLLAMA_MODEL: &str = "qwen3:4b";
/// The providers `config set model` knows.
pub const PROVIDERS: &[&str] = &["anthropic", "openai", "ollama"];

/// An answer model `setup` can offer: what is configured in this
/// environment, best first.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DetectedModel {
    /// `anthropic`, `openai` or `ollama`.
    pub provider: &'static str,
    /// The model the profile would use.
    pub model: String,
    /// The environment variable holding the key (its name only).
    pub key_env: Option<&'static str>,
    /// Why it was found, in words.
    pub found: String,
}

impl DetectedModel {
    /// The command that makes it the answer model.
    pub fn command(&self) -> String {
        match self.provider {
            "ollama" => format!("graphicle config set model ollama:{}", self.model),
            provider => format!("graphicle config set model {provider}"),
        }
    }
}

/// The answer models this environment has configured, best first: an
/// `ANTHROPIC_API_KEY`, an `OPENAI_API_KEY`, then a local Ollama (its
/// `OLLAMA_HOST`, its program on `PATH` or its model folder). Nothing is
/// called: no network, no model.
pub fn detect_models() -> Vec<DetectedModel> {
    detect_models_with(&|name| std::env::var_os(name), home_dir().as_deref())
}

/// [`detect_models`] with the environment `var` and the home folder.
pub fn detect_models_with(
    var: &dyn Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
) -> Vec<DetectedModel> {
    let set = |name: &str| var(name).is_some_and(|v| !v.is_empty());
    let mut found = Vec::new();
    if set("ANTHROPIC_API_KEY") {
        found.push(DetectedModel {
            provider: "anthropic",
            model: ANTHROPIC_MODEL.into(),
            key_env: Some("ANTHROPIC_API_KEY"),
            found: "ANTHROPIC_API_KEY is set".into(),
        });
    }
    if set("OPENAI_API_KEY") {
        found.push(DetectedModel {
            provider: "openai",
            model: OPENAI_MODEL.into(),
            key_env: Some("OPENAI_API_KEY"),
            found: "OPENAI_API_KEY is set".into(),
        });
    }
    let models_dir = var("OLLAMA_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".ollama/models")));
    let pulled = models_dir.as_deref().map(ollama_models).unwrap_or_default();
    let on_path = var("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .filter(|folder| folder.is_absolute())
            .any(|folder| runnable(&folder.join("ollama")))
    });
    let reason = if set("OLLAMA_HOST") {
        Some("OLLAMA_HOST is set".to_string())
    } else if !pulled.is_empty() {
        Some(format!(
            "Ollama has {} pulled",
            plural(pulled.len() as u64, "model")
        ))
    } else if on_path {
        Some("Ollama is installed".to_string())
    } else {
        None
    };
    if let Some(reason) = reason {
        found.push(DetectedModel {
            provider: "ollama",
            model: preferred_ollama(&pulled).unwrap_or_else(|| OLLAMA_MODEL.into()),
            key_env: None,
            found: reason,
        });
    }
    found
}

/// The chat models pulled into an Ollama models folder, `name:tag`
/// (embedding models left out).
pub fn ollama_models(models: &Path) -> Vec<String> {
    let library = models.join("manifests/registry.ollama.ai/library");
    let mut names = Vec::new();
    for model in std::fs::read_dir(&library).into_iter().flatten().flatten() {
        let name = model.file_name().to_string_lossy().into_owned();
        if ["embed", "minilm", "bge"]
            .iter()
            .any(|word| name.contains(word))
        {
            continue;
        }
        for tag in std::fs::read_dir(model.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            names.push(format!("{name}:{}", tag.file_name().to_string_lossy()));
        }
    }
    names.sort();
    names
}

/// The pulled model to answer with: a known good family first.
fn preferred_ollama(pulled: &[String]) -> Option<String> {
    for family in [
        "qwen3", "llama3.2", "llama3.1", "gemma3", "mistral", "phi4", "llama3",
    ] {
        if let Some(name) = pulled
            .iter()
            .find(|name| name.split(':').next() == Some(family))
        {
            return Some(name.clone());
        }
    }
    pulled.first().cloned()
}

/// Whether `value` reads as an environment variable's name (letters,
/// digits and `_`, not starting with a digit) and not as a key: a key, such
/// as `sk-...`, is never written to the configuration.
pub fn variable_name(value: &str) -> bool {
    let name = !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    // Keys' own prefixes, and a long mix of lower-case letters and digits,
    // are keys, not names.
    let prefixed = [
        "sk-",
        "sk_",
        "pk_",
        "rk_",
        "ghp_",
        "gho_",
        "github_pat_",
        "xox",
        "AKIA",
    ]
    .iter()
    .any(|prefix| value.starts_with(prefix));
    let keylike = value.len() >= 32
        && value.chars().any(|c| c.is_ascii_lowercase())
        && value.chars().any(|c| c.is_ascii_digit());
    name && !prefixed && !keylike
}

/// [`variable_name`] for `--key-env` and `key_env` settings; the refusal
/// never repeats the value, which may be a key.
pub fn check_key_env(value: &str) -> Result<()> {
    if variable_name(value) {
        Ok(())
    } else {
        Err(Error::bad(
            "key_env takes the name of the environment variable that holds the key, such as ANTHROPIC_API_KEY, never the key itself; nothing was written",
        ))
    }
}

/// A configuration's `key_env` for display: its name, or a placeholder
/// when it is not a variable's name (it may be a key written by mistake).
pub fn shown_key_env(value: &str) -> String {
    if variable_name(value) {
        value.to_string()
    } else {
        "(a value that is not a variable name: graphicle config set model ... --key-env NAME)"
            .into()
    }
}

/// The Ollama server `OLLAMA_HOST` names, read as Ollama's own client reads
/// it: `host`, `host:port`, an IPv6 address with or without brackets, or a
/// URL; port 11434 unless given (80 or 443 after `http://` or `https://`,
/// as Ollama). A server listening on every address (`0.0.0.0`, `::`, or no
/// host) is reached on loopback. `None` when the value is empty or not an
/// address.
pub fn ollama_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (scheme, rest, default_port) = match value.split_once("://") {
        None => ("http", value, 11434),
        Some((raw, rest)) => match raw.to_ascii_lowercase().as_str() {
            "http" => ("http", rest, 80),
            "https" => ("https", rest, 443),
            _ => return None,
        },
    };
    let hostport = rest.split('/').next().unwrap_or_default();
    let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
        let (address, after) = bracketed.split_once(']')?;
        (address.to_string(), after.strip_prefix(':'))
    } else if hostport.parse::<std::net::Ipv6Addr>().is_ok() {
        (hostport.to_string(), None)
    } else {
        match hostport.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(port)),
            None => (hostport.to_string(), None),
        }
    };
    let port = match port {
        Some(port) => port.parse::<u16>().ok()?,
        None => default_port,
    };
    let host = match host.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_unspecified() && ip.is_ipv6() => "[::1]".to_string(),
        Ok(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        Ok(std::net::IpAddr::V6(ip)) => format!("[{ip}]"),
        Ok(ip) => ip.to_string(),
        Err(_) if host.is_empty() => "127.0.0.1".to_string(),
        Err(_) => host,
    };
    let url = format!("{scheme}://{host}:{port}");
    url::Url::parse(&url).ok().map(|_| url)
}

/// The Ollama address `config set model ollama` writes without `--url`:
/// [`OLLAMA_HOST`](ollama_url)'s, as `ollama` itself uses it, else
/// `http://127.0.0.1:11434`.
pub fn default_ollama_url() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .and_then(|value| ollama_url(&value))
        .unwrap_or_else(|| "http://127.0.0.1:11434".into())
}

/// The model profile `config set model PROVIDER` writes: the provider's
/// protocol and address, `model` (else its default) and `key_env` (else its
/// usual variable). The key itself is never written: only its variable's
/// name.
pub fn model_profile(
    provider: &str,
    model: Option<&str>,
    key_env: Option<&str>,
    url: Option<&str>,
) -> Result<Value> {
    model_profile_at(provider, model, key_env, url, &default_ollama_url())
}

/// [`model_profile`] with the Ollama address used when `url` is `None`.
pub fn model_profile_at(
    provider: &str,
    model: Option<&str>,
    key_env: Option<&str>,
    url: Option<&str>,
    ollama: &str,
) -> Result<Value> {
    if let Some(key_env) = key_env {
        check_key_env(key_env)?;
    }
    Ok(match provider {
        "anthropic" => {
            json!({"protocol":"anthropic","base_url":url.unwrap_or("https://api.anthropic.com/v1"),
            "model":model.unwrap_or(ANTHROPIC_MODEL),"key_env":key_env.unwrap_or("ANTHROPIC_API_KEY"),
            "input_limit":16000,"output_limit":16000,"structured_output":false})
        }
        "openai" => {
            json!({"protocol":"responses","base_url":url.unwrap_or("https://api.openai.com/v1"),
            "model":model.unwrap_or(OPENAI_MODEL),"key_env":key_env.unwrap_or("OPENAI_API_KEY"),
            "input_limit":16000,"output_limit":16000})
        }
        "ollama" => {
            let mut profile = json!({"protocol":"ollama","base_url":url.unwrap_or(ollama),
                "model":model.unwrap_or(OLLAMA_MODEL),"input_limit":16000,"output_limit":2048});
            if let Some(key) = key_env {
                profile["key_env"] = json!(key);
            }
            profile
        }
        _ => {
            return Err(Error::bad(format!(
                "Unknown model provider {provider:?}: choose anthropic, openai or ollama, optionally with a model (anthropic:MODEL, ollama:llama3.2)"
            )));
        }
    })
}

// ----------------------------------------------------------------- config

/// The model configuration file: `--config`, else `models.json` in the data
/// folder.
pub fn config_path(explicit: Option<&Path>, directory: &Path) -> PathBuf {
    explicit.map_or_else(|| directory.join("models.json"), Path::to_path_buf)
}

/// The configuration file as JSON: `{}` when it does not exist.
pub fn read_config(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > 65536 {
        return Err(Error::bad("Provider configuration exceeds 64 KiB"));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        Error::bad(format!(
            "{} is not valid JSON ({error}); fix or remove it",
            path.display()
        ))
    })?;
    if !value.is_object() {
        return Err(Error::bad(format!(
            "{} does not hold a JSON object",
            path.display()
        )));
    }
    Ok(value)
}

/// Write the configuration file after checking it as Graphicle will read
/// it. The first time an existing file changes, it is kept as
/// `<file>.graphicle-bak`. A new file's folder is created private.
pub fn write_config(path: &Path, document: &Value) -> Result<()> {
    let config: model::Config = serde_json::from_value(document.clone())
        .map_err(|error| Error::bad(format!("The configuration would be invalid: {error}")))?;
    config.validate().map_err(|error| {
        Error::bad(format!(
            "The configuration would be invalid: {}",
            error.message
        ))
    })?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    crate::agent_files::backup_once(path)?;
    local::write_private(path, &serde_json::to_vec_pretty(document)?)
}

/// The value at the dotted `key` of `document` (`models.local.model`).
pub fn config_get(document: &Value, key: &str) -> Option<Value> {
    let mut value = document;
    for part in key.split('.') {
        value = value.get(part)?;
    }
    Some(value.clone())
}

/// Set the dotted `key` of `document` to `value` (null removes it),
/// creating the objects on the way.
pub fn config_put(document: &mut Value, key: &str, value: Value) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(Error::bad(format!("Invalid key {key:?}")));
    }
    let mut target = document;
    for part in &parts[..parts.len() - 1] {
        if !target.get(*part).is_some_and(Value::is_object) {
            target[*part] = json!({});
        }
        target = &mut target[*part];
    }
    let last = parts[parts.len() - 1];
    if value.is_null() {
        if let Some(object) = target.as_object_mut() {
            object.remove(last);
        }
    } else {
        target[last] = value;
    }
    Ok(())
}

/// A value from the command line: JSON when it parses (numbers, booleans,
/// null, objects), else the text itself.
pub fn parse_value(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

/// `config set model SPEC`: make a provider's profile the answer model.
/// `SPEC` is `anthropic`, `openai` or `ollama`, optionally with a model
/// after a colon (`ollama:llama3.2`), or `none` to remove the answer model.
/// An existing profile of the provider keeps the fields this does not set
/// (its limits, temperature). The key's variable is named, never read into
/// the file.
pub fn set_answer_model(
    path: &Path,
    spec: &str,
    model: Option<&str>,
    key_env: Option<&str>,
    url: Option<&str>,
) -> Result<Value> {
    let mut document = read_config(path)?;
    if matches!(spec, "none" | "off") {
        if let Some(object) = document.as_object_mut() {
            object.remove("default_model");
        }
        write_config(path, &document)?;
        return Ok(json!({"path":path,"default_model":null}));
    }
    let (provider, named) = spec
        .split_once(':')
        .map_or((spec, None), |(provider, model)| (provider, Some(model)));
    if !PROVIDERS.contains(&provider) {
        return Err(Error::bad(format!(
            "Unknown model provider {provider:?}: choose anthropic, openai or ollama, optionally with a model (ollama:llama3.2), or none"
        )));
    }
    let profile = model_profile(provider, model.or(named), key_env, url)?;
    if !document.get("models").is_some_and(Value::is_object) {
        document["models"] = json!({});
    }
    let merged = match document["models"].get(provider) {
        Some(existing) if existing["protocol"] == profile["protocol"] => {
            let mut merged = existing.clone();
            for field in ["base_url", "model", "key_env", "structured_output"] {
                match profile.get(field) {
                    Some(value) => merged[field] = value.clone(),
                    None => {
                        if let Some(object) = merged.as_object_mut() {
                            object.remove(field);
                        }
                    }
                }
            }
            merged
        }
        _ => profile,
    };
    document["models"][provider] = merged.clone();
    document["default_model"] = json!(provider);
    write_config(path, &document)?;
    let key_env = merged["key_env"].as_str();
    Ok(
        json!({"path":path,"default_model":provider,"profile":merged,
        "key_env":key_env,"key_set":key_env.is_some_and(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()))}),
    )
}

/// Whether the Ollama server at `base_url` answers, and whether it has
/// `model` pulled: `{answers, pulled}` from `GET /api/tags` over plain HTTP,
/// with no proxy, in a few seconds at most. `None` for an address it does
/// not read (not `http://`).
pub fn ollama_check(base_url: &str, model: &str) -> Option<Value> {
    let url = url::Url::parse(base_url).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let silent = Some(json!({"answers": false}));
    let addresses = url.socket_addrs(|| None).unwrap_or_default();
    let Some(mut stream) = addresses
        .iter()
        .find_map(|address| TcpStream::connect_timeout(address, Duration::from_millis(1000)).ok())
    else {
        return silent;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    // HTTP/1.0: the answer ends when the connection closes, never chunked.
    let request = format!(
        "GET {}/api/tags HTTP/1.0\r\nHost: {host}\r\nAccept: application/json\r\n\r\n",
        url.path().trim_end_matches('/')
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return silent;
    }
    let mut response = Vec::new();
    let _ = stream.take(8 * 1024 * 1024).read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return silent;
    };
    let ok = head.starts_with("HTTP/1.") && head.split_whitespace().nth(1) == Some("200");
    let Some(tags) = serde_json::from_str::<Value>(body.trim())
        .ok()
        .filter(|tags| ok && tags["models"].is_array())
    else {
        return silent;
    };
    let pulled = tags["models"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|listed| {
            ["name", "model"].iter().any(|field| {
                listed[field].as_str().is_some_and(|name| {
                    name == model || (!model.contains(':') && name == format!("{model}:latest"))
                })
            })
        });
    Some(json!({"answers": true, "pulled": pulled}))
}

/// `config set KEY VALUE` for any other key: the dotted key in the file,
/// checked as Graphicle reads it.
pub fn config_set(path: &Path, key: &str, value: &str) -> Result<Value> {
    if key == "key_env" || key.ends_with(".key_env") {
        check_key_env(value)?;
    }
    let mut document = read_config(path)?;
    let value = parse_value(value);
    config_put(&mut document, key, value.clone())?;
    write_config(path, &document)?;
    Ok(json!({"path":path,"key":key,"value":value}))
}

/// What `config set` did, for people.
pub fn config_set_text(value: &Value, style: Style) -> String {
    let path = tilde(Path::new(s(value, "path")));
    if value.get("default_model").is_some() {
        let Some(name) = value["default_model"].as_str() else {
            return format!("No answer model now: ask shows the evidence only.\nSaved in {path}.");
        };
        let profile = &value["profile"];
        let mut out = format!(
            "Answer model: {} · {} ({})\nSaved in {path}.\n",
            style.bold(name),
            s(profile, "model"),
            s(profile, "protocol")
        );
        if let Some(key) = value["key_env"].as_str() {
            if value["key_set"] == true {
                let _ = writeln!(
                    out,
                    "The key is read from {key} when a question is asked (it is set in this shell)."
                );
            } else {
                let _ = writeln!(
                    out,
                    "{} {key} is not set in this shell: export it before you ask (export {key}=...).",
                    style.warn("!")
                );
            }
        }
        match (&value["ollama"]["answers"], &value["ollama"]["pulled"]) {
            (Value::Bool(false), _) => {
                let _ = writeln!(
                    out,
                    "{} Ollama is not answering at {}: start it (ollama serve) before you ask.",
                    style.warn("!"),
                    s(profile, "base_url")
                );
            }
            (_, Value::Bool(false)) => {
                let _ = writeln!(
                    out,
                    "{} Ollama does not have {name} yet: run ollama pull {name} before you ask.",
                    style.warn("!"),
                    name = s(profile, "model")
                );
            }
            (Value::Bool(true), Value::Bool(true)) => {
                let _ = writeln!(out, "Ollama answers and has {}.", s(profile, "model"));
            }
            _ => {}
        }
        out.push_str("\nTry: graphicle ask \"how does this project work?\"");
        return out;
    }
    format!(
        "Set {} = {} in {path}.",
        s(value, "key"),
        match &value["value"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    )
}

/// `config show`: the file, the answer model and what this environment
/// could use.
pub fn config_show(path: &Path, model: Option<&ModelState>) -> Result<Value> {
    let mut document = read_config(path)?;
    for profile in document["models"]
        .as_object_mut()
        .into_iter()
        .flat_map(|models| models.values_mut())
    {
        if let Some(key_env) = profile["key_env"].as_str().map(str::to_string) {
            profile["key_env"] = json!(shown_key_env(&key_env));
        }
    }
    Ok(json!({"path":path,"exists":path.is_file(),"file":document,
        "answer_model":model.map(|m| json!({"name":m.name,"protocol":m.protocol,"model":m.model,
            "key_env":m.key_env,"key_available":m.key_available})),
        "detected":detect_models()}))
}

/// [`config_show`] for people.
pub fn config_show_text(value: &Value, style: Style) -> String {
    let path = Path::new(s(value, "path"));
    let mut out = format!(
        "{}  {}{}\n",
        style.bold("Configuration:"),
        tilde(path),
        if value["exists"] == true {
            ""
        } else {
            " (not created yet)"
        }
    );
    let answer = &value["answer_model"];
    let line = if answer.is_object() {
        let key = match answer["key_env"].as_str() {
            None => String::new(),
            Some(key) if answer["key_available"] == true => format!(", key from {key}"),
            Some(key) => format!(", {key} is not set"),
        };
        format!(
            "{} · {} ({}){key}",
            s(answer, "name"),
            s(answer, "model"),
            s(answer, "protocol")
        )
    } else {
        "none: ask shows the evidence only".into()
    };
    let _ = writeln!(out, "{}   {line}", style.bold("Answer model:"));
    let file = &value["file"];
    if let Some(models) = file["models"].as_object().filter(|m| !m.is_empty()) {
        let names: Vec<&str> = models.keys().map(String::as_str).collect();
        let _ = writeln!(
            out,
            "{}       {}",
            style.bold("Profiles:"),
            names.join(", ")
        );
    }
    if let Some(embedding) = file["embedding_model"].as_str() {
        let _ = writeln!(out, "{}     {embedding}", style.bold("Embeddings:"));
    }
    if let Some(limits) = value["limits"].as_object() {
        // The three a large repository meets first, and any other that is set.
        let main = ["project_files", "project_bytes", "file_bytes"];
        let listed: Vec<String> = limits
            .iter()
            .filter(|(name, limit)| main.contains(&name.as_str()) || limit["set"] == true)
            .map(|(name, limit)| format!("{name} {}", s(limit, "shown")))
            .collect();
        let _ = writeln!(
            out,
            "{}         {} (graphicle config get limits.<name>; graphicle config set limits.<name> <value>)",
            style.bold("Limits:"),
            listed.join(" · ")
        );
    }
    match value["freshness"].as_str() {
        Some("off") => {
            let _ = writeln!(
                out,
                "{}      off: answers use the last sync (graphicle config set freshness auto)",
                style.bold("Freshness:")
            );
        }
        Some(_) => {
            let _ = writeln!(
                out,
                "{}      auto: changed files are synced before answers",
                style.bold("Freshness:")
            );
        }
        None => {}
    }
    let detected: Vec<String> = value["detected"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| format!("{} ({})", s(d, "provider"), s(d, "found")))
        .collect();
    if !detected.is_empty() {
        let _ = writeln!(
            out,
            "{}       {}",
            style.bold("Detected:"),
            detected.join("; ")
        );
    }
    out.push_str(
        "\nChange the answer model:  graphicle config set model anthropic|openai|ollama[:MODEL]\n",
    );
    out.push_str("Change a setting:         graphicle config set KEY VALUE (graphicle config example lists them)");
    out
}

/// `graphicle config`'s actions.
#[derive(clap::Subcommand)]
pub enum ConfigAction {
    /// The configuration file, the answer model and the models this
    /// environment could use.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// The configuration file's path.
    Path,
    /// One setting: `model` (the answer model's profile), or a dotted key
    /// such as `models.anthropic.model`.
    Get { key: String },
    /// Change one setting. `model VALUE` sets the answer model: anthropic,
    /// openai or ollama, optionally with a model (`ollama:llama3.2`), or
    /// none. Other keys are dotted paths in the file; VALUE is JSON or text.
    Set {
        key: String,
        value: String,
        /// With `model`: the provider's model name.
        #[arg(long)]
        model: Option<String>,
        /// With `model`: the environment variable that holds the API key.
        #[arg(long)]
        key_env: Option<String>,
        /// With `model`: the provider's base URL.
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// A configuration example with every field.
    Example,
}

/// `graphicle config ...`: the model configuration file (`--config`, else
/// `models.json` in the data folder `directory`, which is not created).
pub fn config_command(
    action: Option<ConfigAction>,
    directory: &Path,
    explicit: Option<&Path>,
) -> Result<()> {
    let path = config_path(explicit, directory);
    let load = || model::Config::load(path.is_file().then_some(path.as_path()));
    match action.unwrap_or(ConfigAction::Show { json: false }) {
        ConfigAction::Show { json } => {
            let config = load()?;
            let answering = configured_model(directory, &config);
            let mut shown = config_show(&path, answering.as_ref())?;
            shown["freshness"] = json!(crate::fresh::mode(&config));
            // GRAPHICLE_SYNC_TRACKED_IGNORED overrides the file.
            shown["sync"] =
                json!({"tracked_ignored": crate::local::tracked_ignored_setting(Some(&config))});
            // GRAPHICLE_PROMPT_CONTEXT_SECONDS overrides the file.
            shown["prompt_context"] =
                json!({"seconds": crate::prompt_hook::budget(Some(&config)).as_secs()});
            shown["limits"] = crate::limits::Limits::from_environment(&config.limits)?.view();
            emit(&shown, json, config_show_text)
        }
        ConfigAction::Path => say(&path.to_string_lossy()),
        ConfigAction::Get { key } => {
            let value = if key == "model" {
                configured_model(directory, &load()?).map(|answering| json!(answering.name))
            } else if key == "freshness" {
                // GRAPHICLE_FRESHNESS overrides the file (ux-fresh).
                Some(json!(crate::fresh::mode(&load()?)))
            } else if key == "sync.tracked_ignored" {
                // GRAPHICLE_SYNC_TRACKED_IGNORED overrides the file.
                Some(json!(crate::local::tracked_ignored_setting(Some(&load()?))))
            } else if key == "prompt_context.seconds" {
                // GRAPHICLE_PROMPT_CONTEXT_SECONDS overrides the file.
                Some(json!(crate::prompt_hook::budget(Some(&load()?)).as_secs()))
            } else if key == crate::update_notice::SETTING {
                // GRAPHICLE_NO_UPDATE_NOTIFIER overrides the file.
                Some(json!(crate::update_notice::on(&path)))
            } else if let Some(name) = key.strip_prefix("limits.")
                && crate::limits::SETTINGS
                    .iter()
                    .any(|(known, ..)| *known == name)
            {
                // The limit in effect: the default, the file or GRAPHICLE_MAX_*.
                crate::limits::Limits::from_environment(&load()?.limits)?
                    .get(name)
                    .map(|value| json!(value))
            } else {
                config_get(&read_config(&path)?, &key)
            };
            match value {
                Some(Value::String(text)) => say(&text),
                Some(value) => say(&value.to_string()),
                None => Err(Error::bad(format!("{key} is not set"))),
            }
        }
        ConfigAction::Set {
            key,
            value,
            model,
            key_env,
            url,
            json,
        } => {
            let result = if key == "model" {
                let mut result = set_answer_model(
                    &path,
                    &value,
                    model.as_deref(),
                    key_env.as_deref(),
                    url.as_deref(),
                )?;
                // An Ollama model: whether Ollama answers and has it.
                let profile = &result["profile"];
                if profile["protocol"] == "ollama"
                    && let Some(check) = ollama_check(s(profile, "base_url"), s(profile, "model"))
                {
                    result["ollama"] = check;
                }
                result
            } else {
                if model.is_some() || key_env.is_some() || url.is_some() {
                    return Err(Error::bad(
                        "--model, --key-env and --url go with `config set model`",
                    ));
                }
                config_set(&path, &key, &value)?
            };
            emit(&result, json, config_set_text)
        }
        ConfigAction::Example => crate::cli::print(&config_example()),
    }
}

/// The example configuration `config example` (and `config-example`)
/// prints.
pub fn config_example() -> Value {
    json!({"default_model":"local","embedding_model":"embedding","worker_concurrency":1,"models":{"local":{"protocol":"ollama","base_url":"http://127.0.0.1:11434","model":"qwen3:4b","input_limit":16000,"output_limit":2048},"embedding":{"protocol":"ollama","base_url":"http://127.0.0.1:11434","model":"nomic-embed-text","input_limit":16000,"output_limit":2048},"api":{"protocol":"responses","base_url":"https://api.openai.com/v1","model":"YOUR_MODEL","key_env":"OPENAI_API_KEY","input_limit":16000,"output_limit":2048}}})
}

// ------------------------------------------------------- context and ask

/// The line range of a quote inside an excerpt that starts at `line`.
fn quote_lines(excerpt: &str, line: u64, quote: &str) -> Option<(u64, u64)> {
    let quote = quote.trim();
    if quote.is_empty() {
        return None;
    }
    let at = excerpt.find(quote)?;
    let first = line + excerpt[..at].matches('\n').count() as u64;
    Some((first, first + quote.matches('\n').count() as u64))
}

/// `path:line-end` of a source of a context packet.
fn location(source: &Value) -> String {
    let path = source["path"]
        .as_str()
        .or_else(|| source["title"].as_str())
        .or_else(|| source["id"].as_str())
        .unwrap_or("source");
    let place = match (source["line"].as_u64(), source["end_line"].as_u64()) {
        (Some(line), Some(end)) if end > line => format!("{path}:{line}-{end}"),
        (Some(line), _) => format!("{path}:{line}"),
        _ => path.to_string(),
    };
    // The date its file last changed, when the packet dated it.
    match source["changed"].as_str().filter(|c| !c.is_empty()) {
        Some(changed) => format!("{place} (changed {changed})"),
        None => place,
    }
}

/// Why a source was chosen, from its code-graph match.
fn reason(source: &Value) -> Option<String> {
    let graph = source.get("code_graph").filter(|g| g.is_object())?;
    let label = graph["label"].as_str()?;
    Some(match graph["via"]["relation"].as_str() {
        Some(relation) => format!("{label}, by {}", relation.replace('_', " ")),
        None => label.to_string(),
    })
}

/// At most `limit` lines of `text`, indented, with how many were left out.
fn excerpt(text: &str, limit: usize, indent: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let mut out = String::new();
    for line in lines.iter().take(limit) {
        let _ = writeln!(out, "{indent}{line}");
    }
    if lines.len() > limit {
        let _ = writeln!(
            out,
            "{indent}… {} more",
            plural((lines.len() - limit) as u64, "line")
        );
    }
    out
}

/// Each source of a context packet with the text of its own excerpt. One
/// file can give several excerpts, all with the file's id, so a source takes
/// the evidence window that starts where it starts: the one at its own
/// position when that matches, else the first with its id and start. A
/// source without such a window shows its preview.
fn excerpts_of(packet: &Value) -> Vec<(&Value, &str)> {
    let sources: &[Value] = packet["sources"].as_array().map_or(&[], Vec::as_slice);
    let evidence: &[Value] = packet["evidence"].as_array().map_or(&[], Vec::as_slice);
    let unset = |value: &Value, key: &str| value.get(key).is_none_or(Value::is_null);
    let same = |source: &Value, window: &Value| {
        window["id"] == source["id"]
            && (unset(source, "byte_start")
                || unset(window, "byte_start")
                || window["byte_start"] == source["byte_start"])
    };
    sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let text = evidence
                .get(index)
                .filter(|window| same(source, window))
                .or_else(|| evidence.iter().find(|window| same(source, window)))
                .and_then(|window| window["text"].as_str())
                .or_else(|| source["preview"].as_str())
                .unwrap_or_default();
            (source, text)
        })
        .collect()
}

/// A `context` packet for people: the numbered excerpts with their place
/// and why each was chosen, the reviewed memories and the budget.
pub fn context_text(packet: &Value, style: Style) -> String {
    // The identifier route's answer: definitions and references, as
    // path:line lines (crate::identifier_route).
    if packet[crate::identifier_route::REPORT]["path"] == "identifier" {
        let mut out = format!(
            "{} {}\n",
            style.bold("Definitions and references for"),
            style.bold(&format!("\"{}\"", s(packet, "question")))
        );
        let _ = writeln!(
            out,
            "{}",
            style.dim(&format!(
                "from the code graph · {} of {} tokens",
                count(packet["tokens"].as_u64().unwrap_or(0)),
                count(packet["max_tokens"].as_u64().unwrap_or(0))
            ))
        );
        // The lines between the header and the footer: the footer tells an
        // agent which tool reads on, and the budget, which the line above
        // gives.
        for line in s(packet, "markdown").lines().skip(1).filter(|line| {
            *line != crate::identifier_route::READ_ON
                && *line != crate::identifier_route::READ_ON_FILES
                && !line.starts_with("Budget: ")
        }) {
            let _ = writeln!(out, "{line}");
        }
        return out;
    }
    let sources = excerpts_of(packet);
    let mut out = format!(
        "{} {}\n",
        style.bold("Evidence for"),
        style.bold(&format!("\"{}\"", s(packet, "question")))
    );
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!(
            "{} · {} of {} tokens",
            plural(sources.len() as u64, "excerpt"),
            count(packet["tokens"].as_u64().unwrap_or(0)),
            count(packet["max_tokens"].as_u64().unwrap_or(0))
        ))
    );
    if let Some(note) = fresh_note(packet, style) {
        let _ = writeln!(out, "{note}");
    }
    if sources.is_empty() {
        out.push_str("\nNothing in this project matches. Try other words, or `graphicle sync` if files changed.\n");
    }
    for (index, (source, text)) in sources.iter().enumerate() {
        let why = reason(source)
            .map(|why| style.dim(&format!("  {why}")))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "\n{} {}{why}",
            style.bold(&format!("[{}]", index + 1)),
            location(source)
        );
        for mark in source["age_marks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let _ = writeln!(out, "    {} {mark}", style.warn("!"));
        }
        out.push_str(&excerpt(text, 12, "    "));
    }
    // The files that rank next, which the excerpts do not quote.
    let more: Vec<&str> = packet["more_files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !more.is_empty() {
        let _ = writeln!(out, "\n{}", style.bold("Next ranked files:"));
        for path in more {
            let _ = writeln!(out, "  - {path}");
        }
    }
    let memories = packet["memories"].as_array().cloned().unwrap_or_default();
    if !memories.is_empty() {
        let _ = writeln!(out, "\n{}", style.bold("Reviewed memory:"));
        for memory in &memories {
            let title = memory["title"]
                .as_str()
                .or_else(|| memory["memory"]["title"].as_str())
                .unwrap_or("memory");
            let _ = writeln!(out, "  - {title}");
        }
    }
    // Only what `graphicle sync` refreshes: passages of changed files and the
    // code graph's nodes (a stale memory or cached answer has its own refresh).
    let freshness = &packet["freshness"];
    let stale = match freshness["by_kind"].as_object() {
        Some(kinds) => ["chunk", "graph_node"]
            .iter()
            .map(|kind| kinds.get(*kind).and_then(Value::as_u64).unwrap_or(0))
            .sum(),
        None => freshness["stale_count"].as_u64().unwrap_or(0),
    };
    if stale > 0 {
        let _ = writeln!(
            out,
            "\n{} {} may be out of date; `graphicle sync` refreshes the project.",
            style.warn("!"),
            plural(stale, "source")
        );
    }
    out
}

/// An `ask` job for people: the answer, its numbered sources with the
/// quoted lines, and what it cost. A result without an answer is shown as
/// JSON.
pub fn answer_text(job: &Value, style: Style) -> String {
    let result = &job["result"];
    let Some(answer) = result["answer"].as_str() else {
        return serde_json::to_string_pretty(result).unwrap_or_default();
    };
    let context = &result["context"];
    let excerpts = excerpts_of(context);
    // A `reflect` answer lists the windows it read (`evidence`) and no
    // `sources`: a window whose id no source has stands for its source, and
    // one that starts its file gives the quote's lines.
    let windows: Vec<(Value, &str)> = context["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|window| {
            !excerpts
                .iter()
                .any(|(source, _)| source["id"] == window["id"])
        })
        .map(|window| {
            let mut place = window.clone();
            if place["path"].is_null() {
                place["path"] = window["title"].clone();
            }
            if place["line"].is_null() && window["byte_start"].as_u64() == Some(0) {
                place["line"] = json!(1);
            }
            (place, window["text"].as_str().unwrap_or_default())
        })
        .collect();
    let excerpts: Vec<(&Value, &str)> = excerpts
        .into_iter()
        .chain(windows.iter().map(|(place, text)| (place, *text)))
        .collect();
    let cited: Vec<Value> = result["evidence"]
        .as_array()
        .or_else(|| result["citations"].as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = String::new();
    if result["abstain"] == true {
        let _ = writeln!(
            out,
            "{} The sources do not answer this question.",
            style.warn("!")
        );
        if !answer.trim().is_empty() {
            let _ = writeln!(out, "{answer}");
        }
    } else {
        let marks: String = (1..=cited.len()).map(|n| format!("[{n}]")).collect();
        let _ = writeln!(
            out,
            "{}{}",
            answer.trim(),
            if marks.is_empty() {
                String::new()
            } else {
                format!(" {}", style.dim(&marks))
            }
        );
    }
    for need in result["unresolved"].as_array().into_iter().flatten() {
        let need = need
            .as_str()
            .map_or_else(|| need.to_string(), str::to_string);
        let _ = writeln!(out, "{} Not found in the sources: {need}", style.dim("-"));
    }
    // `--workflow reflect` reviews its answer in a second call; when that
    // call fails the answer stands unreviewed, and says so.
    if result["review"]["status"] == "failed" {
        let reason = s(&result["review"], "reason");
        let reason: String = reason.chars().take(200).collect();
        let _ = writeln!(
            out,
            "{} This answer was not reviewed against the sources: the review call failed ({}).",
            style.warn("!"),
            reason.trim_end_matches('.')
        );
    }
    if !cited.is_empty() {
        let _ = writeln!(out, "\n{}", style.bold("Sources"));
    }
    for (index, citation) in cited.iter().enumerate() {
        let id = &citation["id"];
        let quote = s(citation, "quote");
        // One file can give several excerpts under its id: the quote's lines
        // come from the excerpt that holds it, counted from that excerpt's
        // own first line.
        let mut same_id = excerpts.iter().filter(|(source, _)| source["id"] == *id);
        let holder = same_id
            .clone()
            .find(|(_, text)| quote_lines(text, 1, quote).is_some())
            .or_else(|| same_id.next());
        let place = holder.map_or_else(
            || id.as_str().unwrap_or("source").to_string(),
            |(source, text)| {
                let path = source["path"]
                    .as_str()
                    .or_else(|| source["title"].as_str())
                    .unwrap_or("source");
                match source["line"]
                    .as_u64()
                    .and_then(|line| quote_lines(text, line, quote))
                {
                    Some((first, last)) if last > first => format!("{path}:{first}-{last}"),
                    Some((first, _)) => format!("{path}:{first}"),
                    None => location(source),
                }
            },
        );
        let _ = writeln!(out, "  {} {place}", style.bold(&format!("[{}]", index + 1)));
        out.push_str(&excerpt(quote, 4, "      "));
    }
    let calls = job["calls"].as_array().map_or(0, Vec::len);
    let (input, output) =
        job["calls"]
            .as_array()
            .into_iter()
            .flatten()
            .fold((0, 0), |(i, o), call| {
                (
                    i + call["input_tokens"].as_u64().unwrap_or(0),
                    o + call["output_tokens"].as_u64().unwrap_or(0),
                )
            });
    let _ = write!(
        out,
        "\n{}",
        style.dim(&format!(
            "{} · {} · {} in, {} out tokens",
            s(job, "kind"),
            plural(calls as u64, "model call"),
            count(input),
            count(output)
        ))
    );
    if let Some(note) = fresh_note(job, style) {
        let _ = write!(out, "\n{note}");
    }
    out
}

/// `ask` with no answer model: the evidence as `context` shows it, and the
/// command that adds a model. JSON: `answered` false with the packet.
pub fn evidence_without_model(packet: &Value, human: bool, origin: Option<&str>) -> Result<()> {
    let detected = detect_models();
    let command = detected.first().map_or_else(
        || "graphicle config set model anthropic".to_string(),
        DetectedModel::command,
    );
    if !human {
        return crate::cli::print(
            &json!({"answered":false,"reason":"no_answer_model","hint":command,"context":packet}),
        );
    }
    let style = Style::detect();
    let mut out = with_origin(origin, style, context_text(packet, style));
    let found = detected
        .first()
        .map(|d| format!(" ({} found: {})", d.provider, d.found))
        .unwrap_or_default();
    let _ = write!(
        out,
        "\n{} No answer model is configured, so this is the evidence without a written answer. Add one{found}:\n  {}",
        style.warn("!"),
        style.bold(&command)
    );
    say(&out)
}

/// The note that names the project when it is not this folder's: `context`
/// and `ask` outside a project use the last synced one (the saved
/// default). `None` when this folder (or `--project`) decided.
pub fn origin_note(
    store: &Store,
    session: &local::Session,
    resolved: &crate::folders::Resolved,
) -> Option<String> {
    if resolved.source != "saved_default" {
        return None;
    }
    let folder = store
        .authorized_read(&session.workspace, &session.capability, "read", |db, _| {
            crate::folders::current_folder(db, &resolved.id)
        })
        .ok()
        .flatten();
    let place = folder
        .map(|folder| format!(" ({})", tilde(&folder)))
        .unwrap_or_default();
    Some(format!(
        "From {}{place}, the last synced project: this folder is not set up (graphicle setup adds it).",
        resolved.name
    ))
}

/// `text` after the [`origin_note`], when there is one.
pub fn with_origin(origin: Option<&str>, style: Style, text: String) -> String {
    match origin {
        Some(note) => format!("{} {note}\n{text}", style.warn("!")),
        None => text,
    }
}

/// Why a job failed and what to do, from its recorded error and the answer
/// model; the error as recorded when it is not a known one.
pub fn job_failure(job: &Value, model: Option<&ModelState>) -> String {
    let error = job["error"]
        .as_str()
        .filter(|e| !e.is_empty())
        .or_else(|| {
            job["calls"]
                .as_array()
                .into_iter()
                .flatten()
                .rev()
                .find_map(|call| call["error"].as_str().filter(|e| !e.is_empty()))
        })
        .unwrap_or("no error was recorded");
    let key = model
        .and_then(|m| m.key_env.clone())
        .unwrap_or_else(|| "its key variable".into());
    let name = model.map_or_else(|| "the model".to_string(), |m| m.model.clone());
    let ollama = model.is_some_and(|m| m.protocol == "ollama");
    let address = model.map_or_else(|| "the provider".to_string(), |m| m.base_url.clone());
    let status = error
        .split("HTTP ")
        .nth(1)
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok());
    let lower = error.to_lowercase();
    let context_window = [
        "context length",
        "context window",
        "too many tokens",
        "too long",
    ]
    .iter()
    .any(|words| lower.contains(words));
    let fix = if error.contains("Credential is unavailable")
        || error.contains("credential unavailable")
        || error.contains("credential is unavailable")
    {
        match model.and_then(|m| m.key_env.as_deref()) {
            Some(key) => format!(
                "The answer model's API key is not available: set {key} in this shell (export {key}=...), or add the key in the app (graphicle open, then Connections)."
            ),
            None => "The answer model's API key is not available: set the variable its configuration names (graphicle config show), or add the key in the app (graphicle open, then Connections).".to_string(),
        }
    } else if matches!(status, Some(401 | 403)) {
        format!(
            "The provider refused the API key in {key}. Check the key, or choose another model with `graphicle config set model ...`."
        )
    } else if matches!(status, Some(400 | 413 | 422)) && context_window {
        let (limit, profile) = model.map_or((16000, "NAME".to_string()), |m| {
            (m.input_limit, m.name.clone())
        });
        format!(
            "The question and its evidence are longer than the model accepts. Lower the input limit to fit the model's context window (now {}): `graphicle config set models.{profile}.input_limit {}`, or ask a narrower question.",
            count(limit as u64),
            (limit / 2).max(512)
        )
    } else if matches!(status, Some(400 | 422)) {
        "The provider rejected the request for the reason in brackets. `graphicle config show` shows the answer model's settings; `graphicle config set model ...` chooses another.".into()
    } else if status == Some(404) && ollama {
        format!(
            "Ollama does not have {name}: run `ollama pull {name}`, or choose a model you have with `graphicle config set model ollama:NAME`."
        )
    } else if status == Some(404) {
        format!(
            "The provider does not know the model {name}. Choose another with `graphicle config set model PROVIDER:MODEL`."
        )
    } else if status == Some(429) {
        "The provider is limiting requests for this key, or its credit ran out. Wait a minute and ask again, or check your plan.".into()
    } else if status.is_some_and(|code| code >= 500) {
        "The provider failed. Ask again in a minute.".into()
    } else if graphicle::net::certificate_refused(error) {
        format!("Could not reach {address}: its certificate is not trusted.")
    } else if error.contains("Could not establish the provider connection")
        || error.contains("connection timed out")
    {
        if ollama {
            format!("Could not reach Ollama at {address}: start it (`ollama serve`) and ask again.")
        } else {
            format!("Could not reach {address}: check the network or proxy, then ask again.")
        }
    } else if error.contains("timed out") {
        "The model did not answer in time. Ask again, or choose a faster model.".into()
    } else if error.contains("exceeds the configured model budget") {
        let (limit, profile) = model.map_or((16000, "NAME".to_string()), |m| {
            (m.input_limit, m.name.clone())
        });
        format!(
            "The evidence for this question does not fit the model's input limit ({} tokens). Raise it with `graphicle config set models.{profile}.input_limit {}`, or ask a narrower question.",
            count(limit as u64),
            (limit * 2).min(128_000)
        )
    } else if error.contains("max_output_tokens") {
        let (limit, profile) = model.map_or((2048, "NAME".to_string()), |m| {
            (m.output_limit, m.name.clone())
        });
        if limit < 8192 {
            format!(
                "The model's reply reached its output limit ({} tokens) before the answer was complete. Raise it with `graphicle config set models.{profile}.output_limit {}`, or ask a narrower question.",
                count(limit as u64),
                (limit * 2).min(16000)
            )
        } else {
            "The model's reply reached the output limit of one answer (8,192 tokens) before it was complete. Ask a narrower question.".into()
        }
    } else if error.contains("cites text the model did not read") {
        if job["kind"] == "reflect" {
            "The model quoted text that is not in the sources, and its review could not repair the quotes, so no answer is shown. Ask again, or choose a stronger model with `graphicle config set model ...`.".into()
        } else {
            "The model quoted text that is not in the sources, so its answer is not shown. Ask again, use `--workflow reflect` (which reviews and repairs citations), or choose a stronger model with `graphicle config set model ...`.".into()
        }
    } else if error.contains("structured")
        || error.contains("invalid structured JSON")
        || error.contains("answer was incomplete")
    {
        "The model's reply did not follow the answer format. Ask again, or choose a stronger model with `graphicle config set model ...`.".into()
    } else if error.contains("declined") {
        "The model declined to answer this question.".into()
    } else {
        "`graphicle doctor` checks the answer model's settings.".into()
    };
    let error = error.trim_end_matches('.');
    if fix.starts_with('`') {
        format!("{error}. {fix}")
    } else {
        format!("{fix} ({error})")
    }
}

// ----------------------------------------------------------- sync, status

/// A `sync` reply for people.
pub fn sync_text(synced: &Value, style: Style) -> String {
    let files = synced["files"].as_u64().unwrap_or(0);
    let mut out = if synced["replayed"] == true {
        format!(
            "{} Synced: no change since the last sync ({})",
            style.good("✓"),
            plural(files, "file")
        )
    } else {
        format!(
            "{} Synced {} ({})",
            style.good("✓"),
            plural(files, "file"),
            bytes(synced["bytes"].as_u64().unwrap_or(0))
        )
    };
    let excluded = synced["excluded"].as_u64().unwrap_or(0);
    if excluded > 0 {
        let _ = write!(
            out,
            "; {} left out (secrets, binaries, large files)",
            count(excluded)
        );
    }
    out.push('\n');
    limits_lines(&mut out, synced, style);
    let graph = &synced["graph"];
    let line = match graph["status"].as_str() {
        Some("built" | "current") => format!(
            "{} Code graph: {} nodes, {} edges",
            style.good("✓"),
            count(graph["nodes"].as_u64().unwrap_or(0)),
            count(graph["edges"].as_u64().unwrap_or(0))
        ),
        Some("background") => "Code graph: building in the background".to_string(),
        Some("queued") => {
            "Code graph: waiting (graphicle mcp, serve or the next sync builds it)".to_string()
        }
        Some("running") => "Code graph: another graphicle process is building it".to_string(),
        Some("failed") => format!(
            "{} Code graph: not built ({}); the next sync tries again",
            style.warn("!"),
            s(graph, "error")
        ),
        _ => String::new(),
    };
    if !line.is_empty() {
        let _ = writeln!(out, "{line}");
    }
    out.push_str("\nNext: graphicle ask \"how does this project work?\" · graphicle status");
    out
}

/// The files a sync analysed in part (`limited`: past an analysis limit,
/// still searchable as text) or left out (`skipped`), the first five of
/// each with the reason (`--json` lists every one), and the note on what
/// one question reads of a larger project (`question_limits`).
fn limits_lines(out: &mut String, synced: &Value, style: Style) {
    const SHOWN: usize = 5;
    for (key, what) in [
        ("limited", "analysed in part (still searchable as text)"),
        ("skipped", "skipped"),
    ] {
        let Some(files) = synced[key].as_array().filter(|files| !files.is_empty()) else {
            continue;
        };
        let _ = writeln!(
            out,
            "{} {} {what}:",
            style.warn("!"),
            plural(files.len() as u64, "file")
        );
        for file in files.iter().take(SHOWN) {
            let _ = writeln!(out, "    {}: {}", s(file, "path"), s(file, "reason"));
        }
        if files.len() > SHOWN {
            let _ = writeln!(
                out,
                "    and {} more: graphicle sync --json lists each with the reason",
                count((files.len() - SHOWN) as u64)
            );
        }
    }
    if let Some(note) = synced["question_limits"].as_str() {
        let _ = writeln!(out, "{} {note}", style.warn("!"));
    }
}

/// One line on the files a sync analysed in part or skipped, for `setup`'s
/// summary; `None` when there are none.
pub fn limits_summary(synced: &Value) -> Option<String> {
    let listed = |key: &str| synced[key].as_array().map_or(0, Vec::len) as u64;
    let (limited, skipped) = (listed("limited"), listed("skipped"));
    let counts = match (limited, skipped) {
        (0, 0) => return None,
        (limited, 0) => format!("{} analysed in part", plural(limited, "file")),
        (0, skipped) => format!("{} skipped", plural(skipped, "file")),
        (limited, skipped) => format!(
            "{} analysed in part and {} skipped",
            plural(limited, "file"),
            plural(skipped, "file")
        ),
    };
    Some(format!(
        "{counts} (size limits): graphicle sync lists them with the reasons"
    ))
}

/// What `status`, `graphicle` alone and `doctor` show about a project.
pub struct Overview {
    pub id: String,
    pub name: String,
    /// `folder` (this folder's), `requested` or `saved_default`.
    pub source: &'static str,
    /// The folder it was synced from.
    pub folder: Option<PathBuf>,
    /// What changed in the folder since the last sync, in words: the
    /// freshness check's line (ux-fresh); `None` prints nothing.
    pub changes: Option<String>,
    /// The `status` operation's reply.
    pub status: Value,
    /// [`local::graph_report`].
    pub graph: Value,
    /// The client entries that serve it.
    pub clients: Vec<EntryCheck>,
    /// Client files Graphicle wrote to that are no longer valid JSON:
    /// `(client, file, where the JSON breaks)`.
    pub unreadable: Vec<(&'static str, PathBuf, String)>,
    pub capture: Vec<CaptureHook>,
    /// Claude Code's guard hooks of this data folder ([`guard_hooks`]).
    pub guard: Vec<CaptureHook>,
    /// `None` outside Git.
    pub git_hooks: Option<Vec<String>>,
    /// The Git hooks setup added for the folder that lack its block.
    pub git_missing: Vec<String>,
    pub model: Option<ModelState>,
    pub server: Probe,
    /// Whether the Graphicle server on the port is this data folder's.
    pub server_ours: bool,
    /// The code symbols of its code graph ([`local::code_symbols`]).
    pub symbols: Option<usize>,
}

/// The Git hooks of `folder` that should hold setup's block and do not: the
/// ones the record (`recorded`) says setup wrote, else all three once one
/// has it.
pub fn missing_git_hooks(
    folder: &Path,
    present: &[String],
    recorded: &[crate::agent_files::Change],
) -> Vec<String> {
    let folder = canonical(folder);
    let mut expected: Vec<String> = recorded
        .iter()
        .filter(|change| {
            change.kind == "git-hook"
                && change.folder.as_deref().map(canonical) == Some(folder.clone())
        })
        .filter_map(|change| change.path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    if expected.is_empty() && !present.is_empty() {
        expected = crate::git_hooks::HOOKS
            .iter()
            .map(|name| name.to_string())
            .collect();
    }
    expected.retain(|name| !present.contains(name));
    expected.dedup();
    expected
}

/// Gather the [`Overview`] of `resolved`, whose `status` reply is `status`.
pub fn overview(
    store: &Store,
    session: &local::Session,
    resolved: &crate::folders::Resolved,
    status: Value,
    config: &model::Config,
) -> Result<Overview> {
    let folder = match &resolved.folder {
        Some(folder) => Some(folder.clone()),
        None => {
            store.authorized_read(&session.workspace, &session.capability, "read", |db, _| {
                crate::folders::current_folder(db, &resolved.id)
            })?
        }
    };
    let graph = local::graph_report(store, session, &resolved.id)?;
    let home = home_dir().unwrap_or_default();
    let exe = std::env::current_exe().unwrap_or_default();
    let recorded = crate::agent_files::recorded(&store.directory);
    let clients = check_entries(
        store,
        session,
        client_entries(folder.as_deref(), &home, &recorded),
        &exe,
    )
    .into_iter()
    .filter(|check| check.serves.as_deref() == Some(resolved.id.as_str()))
    .collect();
    let unreadable = unreadable_client_files(folder.as_deref(), &home, &recorded);
    let ours = canonical(&store.directory);
    let capture = capture_hooks(folder.as_deref(), &home)
        .into_iter()
        .filter(|hook| {
            hook.data_dir
                .as_deref()
                .is_none_or(|dir| canonical(dir) == ours)
        })
        .collect();
    let guard = guard_hooks(folder.as_deref())
        .into_iter()
        .filter(|hook| {
            hook.data_dir
                .as_deref()
                .is_none_or(|dir| canonical(dir) == ours)
        })
        .collect();
    let git_hooks = folder.as_deref().and_then(git_hooks);
    let git_missing = match (folder.as_deref(), &git_hooks) {
        (Some(folder), Some(present)) => missing_git_hooks(folder, present, &recorded),
        _ => Vec::new(),
    };
    let server = probe(DEFAULT_PORT);
    let server_ours = matches!(server, Probe::Graphicle(_))
        && this_folders_server(&store.directory, DEFAULT_PORT);
    Ok(Overview {
        id: resolved.id.clone(),
        name: resolved.name.clone(),
        source: resolved.source,
        folder,
        changes: None,
        status,
        graph,
        clients,
        unreadable,
        capture,
        guard,
        git_hooks,
        git_missing,
        model: model_state(Some((store, session)), config),
        server,
        server_ours,
        symbols: local::code_symbols(store, session, &resolved.id),
    })
}

/// When the project was last synced.
fn last_sync(status: &Value) -> Option<String> {
    let repository = &status["repository"];
    repository["metadata"]["scanned_at"]
        .as_str()
        .or_else(|| repository["updated_at"].as_str())
        .map(when)
}

/// The code graph in words.
fn graph_line(graph: &Value) -> String {
    match graph["status"].as_str() {
        Some("current" | "built") => format!(
            "built: {} nodes, {} edges",
            count(graph["nodes"].as_u64().unwrap_or(0)),
            count(graph["edges"].as_u64().unwrap_or(0))
        ),
        Some("queued") => "waiting to be built (graphicle sync builds it now)".into(),
        Some("running") => "being built".into(),
        Some("failed") => "not built: its build failed (graphicle sync tries again)".into(),
        _ => "not built (graphicle sync builds it)".into(),
    }
}

/// The status "Changes" line (ux-fresh): what the check before this
/// command did (a refresh, or the stale note naming the command that
/// syncs), else how the folder stands against its last sync. `None` for a
/// project without a folder.
pub fn changes_line(
    store: &Store,
    session: &local::Session,
    project: &str,
    outcome: &crate::fresh::Outcome,
) -> Option<String> {
    if let Some(note) = outcome.note() {
        return Some(note.to_string());
    }
    fresh_words(&crate::fresh::check(store, session, project).ok()?)
}

/// A freshness check in words; `None` for a project without a folder.
pub fn fresh_words(check: &crate::fresh::Check) -> Option<String> {
    use crate::fresh::State as Fresh;
    Some(match check.state {
        Fresh::NotFolder => return None,
        Fresh::Missing => "the folder it was synced from is gone".into(),
        Fresh::Fresh => "none since the last sync".into(),
        Fresh::Changed => format!(
            "{} since the last sync (graphicle sync)",
            match check.changed {
                Some(n) => format!("{} changed", plural(n as u64, "file")),
                None => "files changed".into(),
            }
        ),
    })
}

/// The `folder_freshness` note of a reply (ux-fresh), styled: a warning when
/// the data is stale.
fn fresh_note(value: &Value, style: Style) -> Option<String> {
    let report = value.get(crate::fresh::KEY)?;
    let note = report["note"].as_str()?;
    Some(if report["status"] == "stale" {
        format!("{} {note}", style.warn("!"))
    } else {
        style.dim(note)
    })
}

fn entry_line(check: &EntryCheck, folder: Option<&Path>, style: Style) -> String {
    let file = place(&check.entry.file, folder);
    let state = match &check.state {
        EntryState::Ok => style.good("ok"),
        EntryState::MissingProgram => style.bad("its program is missing: graphicle doctor --fix"),
        EntryState::OtherProgram(program) => style.warn(&format!("runs {}", tilde(program))),
        EntryState::MissingProject => style.bad("its project is gone: graphicle doctor"),
        EntryState::OtherStore(dir) => style.dim(&format!("uses {}", tilde(dir))),
        EntryState::NotGraphicle => {
            style.bad("no longer runs graphicle mcp: graphicle doctor --fix")
        }
        EntryState::Gone => style.bad("the entry setup added is gone: graphicle doctor --fix"),
    };
    let name = if check.entry.name == "graphicle" {
        String::new()
    } else {
        format!(", {}", check.entry.name)
    };
    format!("{} ({file}{name}): {state}", check.entry.label())
}

/// The `status` of a project for people.
pub fn status_text(view: &Overview, style: Style) -> String {
    let status = &view.status;
    let label = |text: &str| style.bold(&format!("{text:<10}"));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}{}  {}",
        label("Project"),
        style.bold(&view.name),
        style.dim(&view.id)
    );
    if view.source == "saved_default" {
        let _ = writeln!(
            out,
            "{}{}",
            label(""),
            style.dim(
                "the last synced project: this folder is not a project (graphicle setup adds it)"
            )
        );
    }
    if let Some(folder) = &view.folder {
        let _ = writeln!(out, "{}{}", label("Folder"), tilde(folder));
    }
    if let Some(synced) = last_sync(status) {
        let _ = writeln!(out, "{}{synced}", label("Synced"));
    }
    if let Some(changes) = &view.changes {
        let _ = writeln!(out, "{}{changes}", label("Changes"));
    }
    let files = status["files"].as_u64().unwrap_or(0);
    // The code symbols of the code graph, as setup counts them; none
    // before the graph is built.
    let symbols = view
        .symbols
        .map(|symbols| format!(", {}", plural(symbols as u64, "symbol")))
        .unwrap_or_default();
    let memories = status["memories"].as_u64().unwrap_or(0);
    let drafts = status["draft_memories"].as_u64().unwrap_or(0);
    let mut contents = format!(
        "{}{symbols}, {}",
        plural(files, "file"),
        plural(memories, "memory")
    );
    if drafts > 0 {
        let _ = write!(contents, " ({drafts} to review)");
    }
    let _ = writeln!(out, "{}{contents}", label("Contents"));
    let _ = writeln!(out, "{}{}", label("Graph"), graph_line(&view.graph));
    if view.clients.is_empty() && view.unreadable.is_empty() {
        let _ = writeln!(
            out,
            "{}none connected (graphicle connect <client> adds one: claude-code, codex, cursor, vscode or claude)",
            label("Agents")
        );
    }
    let mut rows: Vec<String> = view
        .clients
        .iter()
        .map(|check| entry_line(check, view.folder.as_deref(), style))
        .collect();
    for (client, file, place) in &view.unreadable {
        rows.push(format!(
            "{} ({}): {}",
            crate::agent_detect::name(client),
            self::place(file, view.folder.as_deref()),
            style.bad(&format!(
                "not valid JSON ({place}): fix it by hand, then graphicle doctor"
            ))
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        let _ = writeln!(
            out,
            "{}{row}",
            label(if index == 0 { "Agents" } else { "" })
        );
    }
    // Each entry runs the one MCP server.
    if !view.clients.is_empty() {
        let learning = view
            .clients
            .iter()
            .any(|check| check.entry.allow_learning());
        let _ =
            writeln!(
                out,
                "{}{}",
                label(""),
                style.dim(&format!(
                    "one MCP server, graphicle mcp: {}{}",
                    if view.clients.iter().all(|check| {
                        check.entry.tools() == Some(crate::api::ToolSet::Essentials)
                    }) {
                        "its five essential tools, the code graph and recall"
                    } else {
                        "the code graph, recall and memory"
                    },
                    if learning {
                        "; memory drafts for review"
                    } else {
                        ""
                    }
                ))
            );
    }
    let capture: Vec<String> = view
        .capture
        .iter()
        .map(|hook| {
            let missing = hook.missing();
            if !hook.runs {
                format!("{} (program missing)", hook.client)
            } else if !missing.is_empty() {
                format!("{} (no {} hook)", hook.client, missing.join(", "))
            } else {
                hook.client.to_string()
            }
        })
        .collect();
    let guard: Vec<String> = view
        .guard
        .iter()
        .map(|hook| {
            if hook.runs {
                hook.client.to_string()
            } else {
                format!("{} (program missing)", hook.client)
            }
        })
        .collect();
    let git = view.git_hooks.as_ref();
    let hooks = if capture.is_empty() && guard.is_empty() && git.is_none_or(Vec::is_empty) {
        "none (graphicle setup adds session capture and Git hooks)".to_string()
    } else {
        let mut parts = vec![if capture.is_empty() {
            "capture: off".to_string()
        } else {
            format!("capture: {}", capture.join(", "))
        }];
        // The guard hooks point the agent's searches to the graph tools.
        if !guard.is_empty() {
            parts.push(format!("graph guard: {}", guard.join(", ")));
        }
        match git {
            Some(git) if git.is_empty() => parts.push("git: none".into()),
            Some(git) if !view.git_missing.is_empty() => parts.push(format!(
                "git: {} ({} missing: graphicle setup)",
                git.join(", "),
                view.git_missing.join(", ")
            )),
            Some(git) => parts.push(format!("git: {}", git.join(", "))),
            None => {}
        }
        parts.join(" · ")
    };
    let _ = writeln!(out, "{}{hooks}", label("Hooks"));
    let _ = writeln!(out, "{}{}", label("Model"), model_line(view.model.as_ref()));
    let server = match &view.server {
        Probe::Graphicle(_) if view.server_ours => {
            format!("running at http://127.0.0.1:{DEFAULT_PORT}")
        }
        Probe::Graphicle(_) => format!(
            "not running; port {DEFAULT_PORT} has another Graphicle data folder's server (graphicle open --port {} opens this one)",
            DEFAULT_PORT + 1
        ),
        Probe::Free => "not running (graphicle open starts it)".into(),
        Probe::Other => format!("port {DEFAULT_PORT} is used by another program"),
    };
    let _ = write!(out, "{}{server}", label("Server"));
    let broken = view
        .clients
        .iter()
        .any(|check| !matches!(check.state, EntryState::Ok | EntryState::OtherProgram(_)))
        || view
            .capture
            .iter()
            .any(|hook| !hook.runs || !hook.missing().is_empty())
        || view.guard.iter().any(|hook| !hook.runs);
    if broken {
        let _ = write!(
            out,
            "\n\n{} Some connections need repair: graphicle doctor --fix",
            style.warn("!")
        );
    }
    out
}

/// `graphicle` alone: inside a project, a short status and the next three
/// commands; elsewhere, what Graphicle is and how to start. Nothing is
/// created: a missing data folder stays missing.
pub fn home(directory: &Path, config: Option<&Path>) -> Result<()> {
    let style = Style::detect();
    let cwd = std::env::current_dir().ok();
    if let Some(cwd) = &cwd
        && crate::graph_command::project_of_folder(directory, cwd).is_some()
        && let Ok(text) = home_in_project(directory, config, style)
    {
        return say(&text);
    }
    let mut out = format!("{} {}\n\n", style.bold("Graphicle:"), first_lower(ABOUT));
    if cwd.as_deref().and_then(git_root).is_some() {
        out.push_str("This repository is not set up yet. Set it up with:\n  graphicle setup\n");
    } else {
        out.push_str("Get started:\n  cd your-repo && graphicle setup\n");
    }
    out.push_str("\nAll commands: graphicle help");
    say(&out)
}

fn first_lower(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

fn home_in_project(directory: &Path, config: Option<&Path>, style: Style) -> Result<String> {
    let store = Store::open(directory)?;
    let session = local::session(&store, false)?;
    let resolved = local::resolve(&store, &session, None)?;
    let status = store.project_status(
        &session.workspace,
        &session.capability,
        &json!({"repository_id":resolved.id}),
    )?;
    let config_file = config_path(config, directory);
    let config = model::Config::load(config_file.is_file().then_some(config_file.as_path()))
        .unwrap_or_default();
    let view = overview(&store, &session, &resolved, status, &config)?;
    let mut out = style.bold(&view.name);
    if let Some(folder) = &view.folder {
        let _ = write!(out, "  {}", style.dim(&tilde(folder)));
    }
    out.push('\n');
    let files = view.status["files"].as_u64().unwrap_or(0);
    let synced = last_sync(&view.status).unwrap_or_else(|| "not synced yet".into());
    // Read only: `graphicle` alone checks the folder and changes nothing.
    let changes = crate::fresh::check(&store, &session, &resolved.id)
        .ok()
        .and_then(|check| {
            use crate::fresh::State as Fresh;
            let auto = crate::fresh::mode(&config) == crate::fresh::Mode::Auto;
            match (check.state, check.changed) {
                (Fresh::Fresh, _) => Some(style.good("up to date")),
                (Fresh::Changed, Some(n)) if auto && n <= crate::fresh::REFRESH_LIMIT => {
                    Some(format!(
                        "{} changed since (synced before the next answer)",
                        plural(n as u64, "file")
                    ))
                }
                (Fresh::Changed, Some(n)) => Some(style.warn(&format!(
                    "{} changed since: graphicle sync",
                    plural(n as u64, "file")
                ))),
                (Fresh::Changed, None) => Some(style.warn("files changed since: graphicle sync")),
                (Fresh::Missing, _) => Some(style.bad("its folder is gone")),
                (Fresh::NotFolder, _) => None,
            }
        })
        .map(|words| format!(" · {words}"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "  Synced {synced} · {}{changes}",
        plural(files, "file")
    );
    let _ = writeln!(out, "  Code graph {}", graph_line(&view.graph));
    let mut agents: Vec<&str> = Vec::new();
    for check in &view.clients {
        if !agents.contains(&check.entry.label()) {
            agents.push(check.entry.label());
        }
    }
    let _ = writeln!(
        out,
        "  Agents: {} · Model: {}",
        if agents.is_empty() {
            "none".to_string()
        } else {
            agents.join(", ")
        },
        view.model.as_ref().map_or_else(
            || "none".to_string(),
            |m| format!("{} ({})", m.name, m.model)
        )
    );
    let mut next = Vec::new();
    if agents.is_empty() {
        next.push("graphicle setup                       connect your coding agents");
    }
    next.push("graphicle ask \"how does this work?\"   ask about this project");
    next.push("graphicle open                        open the app");
    next.push("graphicle status                      the full status");
    let _ = write!(out, "\n{}", style.bold("Next:"));
    for line in next.into_iter().take(3) {
        let _ = write!(out, "\n  {line}");
    }
    Ok(out)
}

// ----------------------------------------------------------------- doctor

/// A doctor check's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ok,
    Info,
    Warn,
    Fail,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Info => "info",
            State::Warn => "warn",
            State::Fail => "fail",
        }
    }
}

/// One doctor check.
#[derive(Clone, Debug)]
pub struct Check {
    pub id: &'static str,
    pub state: State,
    pub title: String,
    pub detail: String,
    /// The command that fixes it.
    pub fix: Option<String>,
}

impl Check {
    fn new(
        id: &'static str,
        state: State,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            id,
            state,
            title: title.into(),
            detail: detail.into(),
            fix: None,
        }
    }
    fn with_fix(mut self, command: impl Into<String>) -> Self {
        self.fix = Some(command.into());
        self
    }
    fn json(&self) -> Value {
        json!({"id":self.id,"state":self.state.name(),"title":self.title,"detail":self.detail,"fix":self.fix})
    }
}

/// What `doctor` was asked to check.
pub struct Doctor<'a> {
    /// The data folder (`--data-dir`).
    pub directory: &'a Path,
    /// `--config`.
    pub config: Option<&'a Path>,
    /// `--project`.
    pub project: Option<&'a str>,
    /// `--fix`: apply the safe fixes.
    pub fix: bool,
    /// The app's port.
    pub port: u16,
}

/// `graphicle doctor`: check the program, the engine, the data folder, this
/// folder's project and its code graph, the agents' entries and hooks, the
/// answer model, the app's port and the free disk; each problem with the
/// command that fixes it. With `fix`, first apply the safe fixes (re-sync the
/// project, which rebuilds its graph; rewrite agent entries whose program is
/// missing) and check again. A missing data folder is reported, not created.
pub fn doctor(options: &Doctor<'_>) -> Result<Value> {
    let mut fixed = Vec::new();
    if options.fix {
        fixed = apply_fixes(options);
    }
    let checks = checks(options);
    let fails = checks.iter().filter(|c| c.state == State::Fail).count();
    let warns = checks.iter().filter(|c| c.state == State::Warn).count();
    Ok(
        json!({"ok":fails==0,"problems":fails,"warnings":warns,"checks":checks.iter().map(Check::json).collect::<Vec<_>>(),"fixed":fixed,"fix_ran":options.fix}),
    )
}

/// The store, its session and the project the check is about, when they
/// exist (nothing is created).
fn existing_store(
    directory: &Path,
) -> Option<std::result::Result<(Store, Option<local::Session>), Error>> {
    if !directory.join("reviews.sqlite3").is_file() {
        return None;
    }
    Some(Store::open(directory).map(|store| {
        let session = directory
            .join("local-session.json")
            .is_file()
            .then(|| local::session(&store, false).ok())
            .flatten();
        (store, session)
    }))
}

fn apply_fixes(options: &Doctor<'_>) -> Vec<Value> {
    let mut fixed = Vec::new();
    let Some(Ok((store, Some(mut session)))) = existing_store(options.directory) else {
        return fixed;
    };
    let cwd = std::env::current_dir().ok();
    let resolved = match options.project {
        Some(project) => local::resolve(&store, &session, Some(project)).ok(),
        None => local::find(&store, &session)
            .ok()
            .flatten()
            .filter(|r| r.source == "folder"),
    };
    if let Some(resolved) = &resolved {
        let folder = resolved.folder.clone().or_else(|| {
            store
                .authorized_read(&session.workspace, &session.capability, "read", |db, _| {
                    crate::folders::current_folder(db, &resolved.id)
                })
                .ok()
                .flatten()
        });
        let outcome = match folder.filter(|f| f.is_dir()) {
            Some(folder) => crate::fresh::locked(&store, &folder, || {
                local::sync(&store, &mut session, &folder, Some(&resolved.id))
            })
            .and_then(|synced| {
                let graph = local::build_graph(&store, &session, &resolved.id, true)?;
                Ok(json!({"fix":"sync","project":resolved.name,"folder":folder,
                        "changed":synced["replayed"]!=true,"graph":graph["status"]}))
            }),
            None => local::build_graph(&store, &session, &resolved.id, true).map(
                |graph| json!({"fix":"graph","project":resolved.name,"graph":graph["status"]}),
            ),
        };
        fixed.push(outcome.unwrap_or_else(
            |error| json!({"fix":"sync","project":resolved.name,"error":error.message}),
        ));
    }
    let folder = resolved
        .as_ref()
        .and_then(|r| {
            r.folder.clone().or_else(|| {
                store
                    .authorized_read(&session.workspace, &session.capability, "read", |db, _| {
                        crate::folders::current_folder(db, &r.id)
                    })
                    .ok()
                    .flatten()
            })
        })
        .or(cwd);
    let home = home_dir().unwrap_or_default();
    let exe = std::env::current_exe().unwrap_or_default();
    let recorded = crate::agent_files::recorded(options.directory);
    for check in check_entries(
        &store,
        &session,
        client_entries(folder.as_deref(), &home, &recorded),
        &exe,
    ) {
        let repairable = match check.state {
            EntryState::MissingProgram => true,
            EntryState::NotGraphicle | EntryState::Gone => check.entry.recorded.is_some(),
            _ => false,
        };
        if !repairable || (check.entry.client == "codex" && codex_cli()) {
            continue;
        }
        let outcome = repair_entry(&store, &check);
        fixed.push(match outcome {
            Ok(_) => json!({"fix":"client","client":check.entry.client,"entry":check.entry.name,"file":check.entry.file}),
            Err(error) => json!({"fix":"client","client":check.entry.client,"entry":check.entry.name,"file":check.entry.file,"error":error.message}),
        });
    }
    // Capture hooks whose program moved (an upgrade), or that lack an
    // event's hook: rewritten with this program, other hooks kept.
    let ours = canonical(&store.directory);
    for hook in capture_hooks(folder.as_deref(), &home) {
        if hook
            .data_dir
            .as_deref()
            .is_none_or(|dir| canonical(dir) != ours)
            || (hook.runs && hook.missing().is_empty())
        {
            continue;
        }
        let cwd = match hook.client {
            "claude-code" => hook
                .file
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            _ => folder.clone(),
        };
        let outcome = match cwd {
            Some(cwd) => crate::agent_setup::refresh_hooks(&store, hook.client, &cwd),
            None => Err(Error::bad("no project folder to resolve")),
        };
        fixed.push(match outcome {
            Ok(_) => json!({"fix":"hooks","client":hook.client,"file":hook.file}),
            Err(error) => {
                json!({"fix":"hooks","client":hook.client,"file":hook.file,"error":error.message})
            }
        });
    }
    // Claude Code's guard hooks whose program moved (the capture hooks of
    // the same file, rewritten above, bring them along).
    for hook in guard_hooks(folder.as_deref()) {
        if hook.runs
            || hook
                .data_dir
                .as_deref()
                .is_none_or(|dir| canonical(dir) != ours)
            || fixed
                .iter()
                .any(|fix| fix["fix"] == "hooks" && fix["file"] == json!(hook.file))
        {
            continue;
        }
        let outcome = match hook.file.parent().and_then(Path::parent) {
            Some(cwd) => crate::agent_setup::refresh_hooks(&store, hook.client, cwd),
            None => Err(Error::bad("no project folder to resolve")),
        };
        fixed.push(match outcome {
            Ok(_) => json!({"fix":"hooks","client":hook.client,"file":hook.file}),
            Err(error) => {
                json!({"fix":"hooks","client":hook.client,"file":hook.file,"error":error.message})
            }
        });
    }
    fixed
}

/// The agent entries and capture hooks under `folders` and `home` that use
/// the data folder `directory`, which has no store or no workspace: each
/// fails to start (agents) or imports nothing (hooks).
fn connected_without_store(directory: &Path, folders: &[PathBuf], home: &Path) -> Vec<String> {
    let wanted = normalized(directory);
    let mut found: Vec<String> = Vec::new();
    for folder in folders.iter().map(Some).chain([None]) {
        let folder = folder.map(PathBuf::as_path);
        for entry in client_entries(folder, home, &[]) {
            let what = format!(
                "{} entry {} in {}",
                entry.label(),
                entry.name,
                tilde(&entry.file)
            );
            if normalized(&entry.data_dir()) == wanted && !found.contains(&what) {
                found.push(what);
            }
        }
        for hook in capture_hooks(folder, home) {
            let what = format!("{} capture hooks in {}", hook.client, tilde(&hook.file));
            if hook
                .data_dir
                .as_deref()
                .is_some_and(|dir| normalized(dir) == wanted)
                && !found.contains(&what)
            {
                found.push(what);
            }
        }
    }
    found
}

/// `path` absolute, with its longest existing part made canonical: two
/// spellings of a folder that does not exist (any more) compare equal.
fn normalized(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    for ancestor in path.ancestors() {
        if let Ok(real) = ancestor.canonical() {
            let rest = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
            return if rest.as_os_str().is_empty() {
                real
            } else {
                real.join(rest)
            };
        }
    }
    path
}

fn checks(options: &Doctor<'_>) -> Vec<Check> {
    let mut checks = Vec::new();
    let exe = std::env::current_exe()
        .map(|exe| canonical(&exe))
        .unwrap_or_default();
    checks.push(Check::new(
        "program",
        State::Ok,
        "Program",
        format!(
            "graphicle {} ({}) at {}",
            env!("CARGO_PKG_VERSION"),
            crate::core_capabilities::edition(),
            tilde(&exe)
        ),
    ));
    let update = update_instructions();
    let reinstall = if update["installed"] == "source" {
        // The engine is built from the same clone, in `graphicle/`.
        "cd graphicle && cargo build --release --locked -p graphicle-graph".to_string()
    } else {
        update["update"]
            .as_array()
            .and_then(|commands| commands.last())
            .and_then(Value::as_str)
            .unwrap_or("graphicle update")
            .to_string()
    };
    checks.push(
        match crate::graph_command::locate(Some(&exe), std::env::var_os("PATH").as_deref()) {
            Some(engine) => {
                let version = std::process::Command::new(&engine)
                    .arg("--version")
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .unwrap_or_default();
                let reported = version.split_whitespace().nth(1).unwrap_or_default();
                if !reported.is_empty() && reported != env!("CARGO_PKG_VERSION") {
                    Check::new(
                        "engine",
                        State::Warn,
                        "Graph engine",
                        format!(
                            "{} is version {reported}, not {}; install both programs from one release",
                            tilde(&engine),
                            env!("CARGO_PKG_VERSION")
                        ),
                    )
                    .with_fix(reinstall.clone())
                } else {
                    Check::new("engine", State::Ok, "Graph engine", tilde(&engine))
                }
            }
            None => Check::new(
                "engine",
                State::Warn,
                "Graph engine",
                "graphicle-graph was not found next to this program or on PATH; `graphicle graph` needs it (sync and answers do not)",
            )
            .with_fix(reinstall.clone()),
        },
    );
    let directory = options.directory;
    let data = if !directory.exists() {
        Check::new(
            "data",
            State::Info,
            "Data folder",
            format!("{} does not exist yet; setup creates it", tilde(directory)),
        )
        .with_fix("graphicle setup")
    } else if !writable(directory) {
        Check::new(
            "data",
            State::Fail,
            "Data folder",
            format!("{} is not writable", tilde(directory)),
        )
        .with_fix(format!(
            "ls -ld {}",
            shell_quote(&directory.to_string_lossy())
        ))
    } else {
        let stamped = data_version(directory).unwrap_or_else(|| "unknown".into());
        match data_version(directory) {
            Some(recorded)
                if version_parts(&recorded) > version_parts(env!("CARGO_PKG_VERSION")) =>
            {
                Check::new(
                    "data",
                    State::Warn,
                    "Data folder",
                    format!(
                        "{} was last used by Graphicle {recorded}, newer than this program",
                        tilde(directory)
                    ),
                )
                .with_fix("graphicle update")
            }
            _ => Check::new(
                "data",
                State::Ok,
                "Data folder",
                format!(
                    "{} (writable; written by Graphicle {stamped})",
                    tilde(directory)
                ),
            ),
        }
    };
    checks.push(data);
    let cwd = std::env::current_dir().ok();
    let home = home_dir().unwrap_or_default();
    let mut answer_model = None;
    let config_file = config_path(options.config, directory);
    let file_config = model::Config::load(config_file.is_file().then_some(config_file.as_path()));
    let mut project_folder = cwd.clone();
    // The folder of the project this check is about, when one resolved.
    let mut synced_folder: Option<PathBuf> = None;
    // Agents connected to a data folder that is gone (or has no workspace)
    // fail to start: that is a problem, whatever else is set up.
    let stranded = || {
        let mut folders: Vec<PathBuf> = cwd.iter().cloned().collect();
        if let Some(root) = cwd.as_deref().and_then(git_root)
            && !folders.contains(&root)
        {
            folders.push(root);
        }
        let found = connected_without_store(directory, &folders, &home);
        (!found.is_empty()).then(|| {
            let state = if directory.join("reviews.sqlite3").is_file() {
                "has no workspace"
            } else {
                "does not exist"
            };
            let (verb, they) = if found.len() == 1 {
                ("uses", "it")
            } else {
                ("use", "they")
            };
            Check::new(
                "agents",
                State::Fail,
                "Agents",
                format!(
                    "{} {verb} the data folder {}, which {state}, so {they} cannot start: {}",
                    plural(found.len() as u64, "agent connection"),
                    tilde(directory),
                    found.join("; ")
                ),
            )
            .with_fix("graphicle setup")
        })
    };
    match existing_store(directory) {
        None => {
            checks.push(
                Check::new(
                    "project",
                    State::Info,
                    "Project",
                    "Graphicle is not set up on this computer yet",
                )
                .with_fix("cd your-repo && graphicle setup"),
            );
            checks.extend(stranded());
        }
        Some(Err(error)) => checks.push(Check::new(
            "store",
            State::Fail,
            "Store",
            format!("the store could not be opened: {}", error.message),
        )),
        Some(Ok((_, None))) => {
            checks.push(
                Check::new("project", State::Warn, "Project", "no workspace yet")
                    .with_fix("graphicle setup"),
            );
            checks.extend(stranded());
        }
        Some(Ok((store, Some(session)))) => {
            let resolved = match options.project {
                Some(project) => local::resolve(&store, &session, Some(project)).map(Some),
                None => local::find(&store, &session)
                    .map(|found| found.filter(|r| r.source == "folder")),
            };
            match resolved {
                Err(error) => {
                    checks.push(Check::new("project", State::Fail, "Project", error.message))
                }
                Ok(None) => {
                    let in_git = cwd.as_deref().and_then(git_root).is_some();
                    checks.push(
                        Check::new(
                            "project",
                            if in_git { State::Warn } else { State::Info },
                            "Project",
                            if in_git {
                                "this repository is not a Graphicle project yet"
                            } else {
                                "this folder is not in a Graphicle project"
                            },
                        )
                        .with_fix("graphicle setup"),
                    );
                }
                Ok(Some(resolved)) => {
                    let status = store.project_status(
                        &session.workspace,
                        &session.capability,
                        &json!({"repository_id":resolved.id}),
                    );
                    let folder = resolved.folder.clone().or_else(|| {
                        store
                            .authorized_read(
                                &session.workspace,
                                &session.capability,
                                "read",
                                |db, _| crate::folders::current_folder(db, &resolved.id),
                            )
                            .ok()
                            .flatten()
                    });
                    if folder.is_some() {
                        project_folder.clone_from(&folder);
                        synced_folder.clone_from(&folder);
                    }
                    let synced = status.as_ref().ok().and_then(last_sync).unwrap_or_default();
                    let files = status
                        .as_ref()
                        .ok()
                        .and_then(|s| s["files"].as_u64())
                        .unwrap_or(0);
                    checks.push(Check::new(
                        "project",
                        State::Ok,
                        "Project",
                        format!(
                            "{}: {}, synced {synced}",
                            resolved.name,
                            plural(files, "file")
                        ),
                    ));
                    let graph = local::graph_report(&store, &session, &resolved.id);
                    checks.push(match graph {
                        Ok(graph) if matches!(graph["status"].as_str(), Some("current")) => {
                            Check::new("graph", State::Ok, "Code graph", graph_line(&graph))
                        }
                        Ok(graph) if graph["status"] == "running" => {
                            Check::new("graph", State::Info, "Code graph", graph_line(&graph))
                        }
                        Ok(graph) => {
                            Check::new("graph", State::Warn, "Code graph", graph_line(&graph))
                                .with_fix("graphicle doctor --fix")
                        }
                        Err(error) => Check::new("graph", State::Fail, "Code graph", error.message)
                            .with_fix("graphicle sync"),
                    });
                    // How fast questions are: the passage index and the saved
                    // code graph (crate::indexed_query::readiness).
                    if let Ok(check) = store.authorized_read(
                        &session.workspace,
                        &session.capability,
                        "read",
                        |db, _| crate::indexed_query::readiness(db, &resolved.id),
                    ) {
                        let state = match check["state"].as_str() {
                            Some("ready") => State::Ok,
                            Some("small") => State::Info,
                            _ => State::Warn,
                        };
                        let mut line = Check::new(
                            "context",
                            state,
                            "Questions",
                            s(&check, "detail").to_string(),
                        );
                        if state == State::Warn {
                            line = line.with_fix(s(&check, "fix").to_string());
                        }
                        checks.push(line);
                    }
                    // Freshness (ux-fresh): the folder against its last
                    // sync, read only.
                    if let Ok(fresh) = crate::fresh::check(&store, &session, &resolved.id) {
                        let sync = match fresh.folder.as_deref() {
                            Some(folder)
                                if cwd.as_deref().map(canonical) != Some(canonical(folder)) =>
                            {
                                format!("graphicle sync {}", shell_quote(&folder.to_string_lossy()))
                            }
                            _ => "graphicle sync".to_string(),
                        };
                        let place = fresh.folder.as_deref().map(tilde).unwrap_or_default();
                        match fresh.state {
                            crate::fresh::State::NotFolder => {}
                            crate::fresh::State::Fresh => checks.push(Check::new(
                                "fresh",
                                State::Ok,
                                "Freshness",
                                format!("up to date with {place}"),
                            )),
                            crate::fresh::State::Changed => checks.push(
                                Check::new(
                                    "fresh",
                                    State::Warn,
                                    "Freshness",
                                    format!(
                                        "{} changed in {place} since the last sync",
                                        fresh.changed.map_or_else(
                                            || "files".to_string(),
                                            |n| plural(n as u64, "file")
                                        )
                                    ),
                                )
                                .with_fix(sync),
                            ),
                            crate::fresh::State::Missing => checks.push(
                                Check::new(
                                    "fresh",
                                    State::Fail,
                                    "Freshness",
                                    format!(
                                        "{place}, the folder it was synced from, is gone; set it up where it is now"
                                    ),
                                )
                                .with_fix("graphicle setup"),
                            ),
                        }
                    }
                }
            }
            let recorded = crate::agent_files::recorded(directory);
            let entries = check_entries(
                &store,
                &session,
                client_entries(project_folder.as_deref(), &home, &recorded),
                &exe,
            );
            let ours: Vec<&EntryCheck> = entries
                .iter()
                .filter(|check| !matches!(check.state, EntryState::OtherStore(_)))
                .collect();
            let unreadable = unreadable_client_files(project_folder.as_deref(), &home, &recorded);
            for (client, file, place) in &unreadable {
                let backup = crate::agent_files::backup_path(file);
                checks.push(
                    Check::new(
                        "client",
                        State::Fail,
                        "Agent",
                        format!(
                            "{} cannot read {}: it is not valid JSON ({place}), so its servers do not start",
                            crate::agent_detect::name(client),
                            tilde(file)
                        ),
                    )
                    .with_fix(if backup.is_file() {
                        format!(
                            "fix the JSON by hand (your copy from before Graphicle: {}), then graphicle doctor --fix",
                            tilde(&backup)
                        )
                    } else {
                        "fix the JSON by hand, then graphicle doctor --fix".to_string()
                    }),
                );
            }
            if ours.is_empty() && unreadable.is_empty() {
                checks.push(
                    Check::new(
                        "clients",
                        State::Info,
                        "Agents",
                        "no coding agent is connected to this data folder",
                    )
                    .with_fix(if project_folder.is_some() {
                        "graphicle connect <client> (claude-code, codex, cursor, vscode or claude)"
                    } else {
                        "graphicle setup"
                    }),
                );
            }
            for check in ours {
                let what = format!(
                    "{} entry {} in {}",
                    check.entry.label(),
                    check.entry.name,
                    place(&check.entry.file, project_folder.as_deref())
                );
                let reconnect = if check.entry.client == "codex" && codex_cli() {
                    format!(
                        "codex mcp remove {} && graphicle connect codex",
                        check.entry.name
                    )
                } else {
                    "graphicle doctor --fix".to_string()
                };
                checks.push(match &check.state {
                    // The one MCP server: code graph, recall
                    // and memory tools in one connection.
                    EntryState::Ok => Check::new(
                        "client",
                        State::Ok,
                        "Agent",
                        format!(
                            "{what}: graphicle mcp, with {}{}",
                            if check.entry.tools() == Some(crate::api::ToolSet::Essentials) {
                                "its five essential tools (code graph and recall)"
                            } else {
                                "the code-graph, recall and memory tools"
                            },
                            if check.entry.allow_learning() {
                                " and memory drafts for review"
                            } else {
                                ""
                            }
                        ),
                    ),
                    EntryState::MissingProgram => Check::new(
                        "client",
                        State::Fail,
                        "Agent",
                        format!("{what} runs {}, which does not exist", check.entry.command),
                    )
                    .with_fix(reconnect),
                    EntryState::OtherProgram(program) => Check::new(
                        "client",
                        State::Warn,
                        "Agent",
                        format!("{what} runs {}, not this program", tilde(program)),
                    )
                    .with_fix(format!(
                        "graphicle connect {} --replace",
                        check.entry.client
                    )),
                    EntryState::MissingProject => Check::new(
                        "client",
                        State::Fail,
                        "Agent",
                        format!("{what} names a project this store does not have"),
                    )
                    .with_fix(format!(
                        "graphicle connect {} --replace",
                        check.entry.client
                    )),
                    EntryState::NotGraphicle => Check::new(
                        "client",
                        State::Fail,
                        "Agent",
                        format!("{what}, which Graphicle wrote, no longer runs graphicle mcp, so it does not start"),
                    )
                    .with_fix(reconnect),
                    EntryState::Gone => Check::new(
                        "client",
                        State::Warn,
                        "Agent",
                        format!(
                            "{what}, which Graphicle added, is gone (graphicle disconnect {} forgets it)",
                            check.entry.client
                        ),
                    )
                    .with_fix(reconnect),
                    EntryState::OtherStore(_) => unreachable!(),
                });
            }
            answer_model = Some(model_state(
                Some((&store, &session)),
                file_config.as_ref().unwrap_or(&model::Config::default()),
            ));
        }
    }
    // This data folder's capture hooks (another data folder's are its own).
    let ours = normalized(directory);
    let hooks: Vec<CaptureHook> = capture_hooks(project_folder.as_deref(), &home)
        .into_iter()
        .filter(|hook| {
            hook.data_dir
                .as_deref()
                .is_none_or(|dir| normalized(dir) == ours)
        })
        .collect();
    if hooks.is_empty() {
        checks.push(
            Check::new(
                "capture",
                State::Info,
                "Session capture",
                "off: agent sessions are not saved as project sources",
            )
            .with_fix(if project_folder.is_some() {
                "graphicle connect claude-code --hooks (or codex)"
            } else {
                "graphicle setup"
            }),
        );
    }
    for hook in hooks {
        let missing = hook.missing();
        let file = place(&hook.file, project_folder.as_deref());
        checks.push(if !hook.runs {
            Check::new(
                "capture",
                State::Fail,
                "Session capture",
                format!(
                    "{} hooks in {file} run a program that does not exist",
                    hook.client
                ),
            )
            .with_fix("graphicle doctor --fix")
        } else if !missing.is_empty() {
            Check::new(
                "capture",
                State::Warn,
                "Session capture",
                format!(
                    "{} hooks in {file} have no {} hook, so those turns are not captured",
                    hook.client,
                    missing.join(", ")
                ),
            )
            .with_fix("graphicle doctor --fix")
        } else {
            Check::new(
                "capture",
                State::Ok,
                "Session capture",
                format!("{} hooks in {file}", hook.client),
            )
        });
    }
    // Claude Code's guard hooks (setup, `connect --graph`): present ones
    // must run.
    for hook in guard_hooks(project_folder.as_deref())
        .into_iter()
        .filter(|hook| {
            hook.data_dir
                .as_deref()
                .is_none_or(|dir| normalized(dir) == ours)
        })
    {
        let file = place(&hook.file, project_folder.as_deref());
        checks.push(if hook.runs {
            Check::new(
                "guard",
                State::Ok,
                "Graph guard",
                format!(
                    "{} hooks in {file} point searches and source reads to the graph tools",
                    hook.client
                ),
            )
        } else {
            Check::new(
                "guard",
                State::Fail,
                "Graph guard",
                format!(
                    "{} hooks in {file} run a program that does not exist",
                    hook.client
                ),
            )
            .with_fix("graphicle doctor --fix")
        });
    }
    // The Graphicle section setup adds to AGENTS.md and CLAUDE.md.
    if let Some(folder) = synced_folder.as_deref() {
        let marked: Vec<&str> = ["AGENTS.md", "CLAUDE.md"]
            .into_iter()
            .filter(|name| {
                std::fs::read_to_string(folder.join(name))
                    .is_ok_and(|text| crate::agent_instructions::present(&text))
            })
            .collect();
        checks.push(if marked.is_empty() {
            Check::new(
                "instructions",
                State::Info,
                "Agent instructions",
                "no Graphicle section in AGENTS.md or CLAUDE.md: Claude Code gets the server's instructions, and setup adds the section when it connects another agent, or with --tools all",
            )
        } else {
            Check::new(
                "instructions",
                State::Ok,
                "Agent instructions",
                format!("the Graphicle section in {}", marked.join(" and ")),
            )
        });
    }
    if let Some(folder) = project_folder.as_deref()
        && let Some(git) = git_hooks(folder)
    {
        let missing = missing_git_hooks(folder, &git, &crate::agent_files::recorded(directory));
        checks.push(if git.is_empty() {
            Check::new(
                "git_hooks",
                State::Info,
                "Git hooks",
                "none: the project is refreshed when you sync or ask",
            )
            .with_fix("graphicle setup")
        } else if !missing.is_empty() {
            Check::new(
                "git_hooks",
                State::Warn,
                "Git hooks",
                format!(
                    "{} {} no Graphicle block ({} {}): the project is not refreshed after those",
                    missing.join(", "),
                    if missing.len() == 1 { "has" } else { "have" },
                    git.join(", "),
                    if git.len() == 1 { "has it" } else { "have it" }
                ),
            )
            .with_fix("graphicle setup")
        } else {
            Check::new("git_hooks", State::Ok, "Git hooks", git.join(", "))
        });
    }
    let answer_model = match answer_model {
        Some(found) => found,
        None => model_state(
            None,
            file_config.as_ref().unwrap_or(&model::Config::default()),
        ),
    };
    checks.push(match (&file_config, &answer_model) {
        (Err(error), _) => Check::new(
            "model",
            State::Fail,
            "Answer model",
            format!("{} is invalid: {}", tilde(&config_file), error.message),
        )
        .with_fix("graphicle config show"),
        (Ok(_), None) => {
            let command = detect_models().first().map_or_else(
                || "graphicle config set model anthropic".to_string(),
                DetectedModel::command,
            );
            Check::new(
                "model",
                State::Warn,
                "Answer model",
                "none: ask shows the evidence without a written answer",
            )
            .with_fix(command)
        }
        (Ok(_), Some(m)) if !m.key_available => Check::new(
            "model",
            State::Fail,
            "Answer model",
            format!(
                "{} uses {}, which is not set in this environment",
                m.name,
                m.key_env.clone().unwrap_or_default()
            ),
        )
        .with_fix(format!(
            "export {}=...",
            m.key_env.clone().unwrap_or_default()
        )),
        (Ok(_), Some(m)) => Check::new("model", State::Ok, "Answer model", model_line(Some(m))),
    });
    checks.push(match probe(options.port) {
        Probe::Free => Check::new(
            "port",
            State::Ok,
            "App port",
            format!("{} is free", options.port),
        ),
        Probe::Graphicle(version) if this_folders_server(directory, options.port) => Check::new(
            "port",
            State::Ok,
            "App port",
            format!(
                "Graphicle {version} is running at http://127.0.0.1:{}",
                options.port
            ),
        ),
        Probe::Graphicle(version) => Check::new(
            "port",
            State::Warn,
            "App port",
            format!(
                "{} is used by a Graphicle server ({version}) of another data folder",
                options.port
            ),
        )
        .with_fix(format!(
            "graphicle open --port {}",
            options.port.checked_add(1).unwrap_or(8855)
        )),
        Probe::Other => {
            let holder = port_holder(options.port)
                .map(|(pid, name)| format!(" by PID {pid} ({name})"))
                .unwrap_or_default();
            Check::new(
                "port",
                State::Warn,
                "App port",
                format!("{} is used{holder}", options.port),
            )
            .with_fix(format!(
                "graphicle open --port {}",
                options.port.checked_add(1).unwrap_or(8855)
            ))
        }
    });
    checks.push(match free_bytes(directory) {
        Some(free) if free < 256 * 1024 * 1024 => Check::new(
            "disk",
            State::Fail,
            "Disk",
            format!("{} free where the data folder is", bytes(free)),
        )
        .with_fix("free some disk space"),
        Some(free) if free < 1024 * 1024 * 1024 => Check::new(
            "disk",
            State::Warn,
            "Disk",
            format!("{} free where the data folder is", bytes(free)),
        )
        .with_fix("free some disk space"),
        Some(free) => Check::new("disk", State::Ok, "Disk", format!("{} free", bytes(free))),
        None => Check::new("disk", State::Info, "Disk", "free space unknown"),
    });
    checks
}

/// A `doctor` report for people: one line per check, each problem with its
/// fix, and what `--fix` did.
pub fn doctor_text(report: &Value, style: Style) -> String {
    let mut out = format!("{}\n", style.bold("Graphicle doctor"));
    for fixed in report["fixed"].as_array().into_iter().flatten() {
        let what = match s(fixed, "fix") {
            "client" => format!(
                "rewrote the {} entry {}",
                s(fixed, "client"),
                s(fixed, "entry")
            ),
            "hooks" => format!(
                "rewrote the {} capture hooks in {}",
                s(fixed, "client"),
                tilde(Path::new(s(fixed, "file")))
            ),
            "graph" => format!("built the code graph of {}", s(fixed, "project")),
            _ if fixed["changed"] == false => {
                format!("re-synced {} (no change)", s(fixed, "project"))
            }
            _ => format!("re-synced {}", s(fixed, "project")),
        };
        match fixed["error"].as_str() {
            Some(error) => {
                let _ = writeln!(out, "  {} could not fix: {what}: {error}", style.bad("✗"));
            }
            None => {
                let _ = writeln!(out, "  {} fixed: {what}", style.good("✓"));
            }
        }
    }
    if report["fixed"].as_array().is_some_and(|f| !f.is_empty()) {
        out.push('\n');
    }
    let width = report["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| s(c, "title").chars().count())
        .max()
        .unwrap_or(10)
        + 2;
    for check in report["checks"].as_array().into_iter().flatten() {
        let mark = match s(check, "state") {
            "ok" => style.good("✓"),
            "info" => style.dim("·"),
            "warn" => style.warn("!"),
            _ => style.bad("✗"),
        };
        let _ = writeln!(
            out,
            "  {mark} {}{}",
            style.bold(&format!("{:<width$}", s(check, "title"))),
            s(check, "detail")
        );
        if let Some(fix) = check["fix"].as_str()
            && s(check, "state") != "ok"
        {
            let label = if s(check, "state") == "info" {
                "next"
            } else {
                "fix"
            };
            let _ = writeln!(out, "    {:<width$}{label}: {}", "", style.bold(fix));
        }
    }
    let problems = report["problems"].as_u64().unwrap_or(0);
    let warnings = report["warnings"].as_u64().unwrap_or(0);
    // `--fix` is offered only for what it repairs, and not again after it ran.
    let fixable = report["fix_ran"] != true
        && report["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|check| {
                matches!(s(check, "state"), "warn" | "fail")
                    && check["fix"] == "graphicle doctor --fix"
            });
    let then = if fixable {
        " `graphicle doctor --fix` applies the safe fixes."
    } else if problems + warnings > 0 {
        " Run the fix commands above."
    } else {
        ""
    };
    out.push('\n');
    out.push_str(&match (problems, warnings) {
        (0, 0) => style.good("Everything looks good."),
        (0, w) => format!("{}, no problems.{then}", plural(w, "warning")),
        (p, w) => format!(
            "{} and {}.{then}",
            plural(p, "problem"),
            plural(w, "warning")
        ),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_json_unless_a_terminal_or_text_is_asked_for() {
        assert!(human_with(false, None, true));
        assert!(!human_with(false, None, false));
        assert!(!human_with(true, None, true));
        assert!(human_with(false, Some("text"), false));
        assert!(!human_with(true, Some("text"), false));
        assert!(!human_with(false, Some("json"), true));
        assert!(human_with(false, Some("other"), true));
    }

    #[test]
    fn colour_follows_no_color_and_dumb_terminals() {
        assert!(color_with(None, Some("xterm-256color"), true));
        assert!(!color_with(None, Some("xterm"), false));
        assert!(!color_with(Some(OsStr::new("1")), Some("xterm"), true));
        assert!(color_with(Some(OsStr::new("")), None, true));
        assert!(!color_with(None, Some("dumb"), true));
        assert_eq!(Style::plain().bold("x"), "x");
        assert_eq!(Style { color: true }.bold("x"), "\x1b[1mx\x1b[0m");
    }

    #[test]
    fn numbers_sizes_and_ages_read_well() {
        assert_eq!(count(0), "0");
        assert_eq!(count(1204), "1,204");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(3, "file"), "3 files");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(3400), "3.4 KB");
        assert_eq!(bytes(1_500_000_000), "1.5 GB");
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(120), "2 minutes ago");
        assert_eq!(ago(3600), "1 hour ago");
        assert_eq!(ago(3 * 86400), "3 days ago");
        assert!(when("2026-10-13T15:56:12.107781Z").starts_with("2026-10-1"));
        assert_eq!(when("not a time"), "not a time");
    }

    #[test]
    fn first_sentences_are_short() {
        assert_eq!(
            first_sentence("Build, update and query code graphs. More text here."),
            "Build, update and query code graphs"
        );
        assert_eq!(first_sentence("One line\nsecond"), "One line");
        assert!(first_sentence(&"word ".repeat(40)).chars().count() <= 72);
    }

    #[test]
    fn common_errors_name_the_command_that_fixes_them() {
        assert!(
            explain("Select --project or sync a local folder first").contains("graphicle setup")
        );
        assert!(
            explain("Run graphicle init, or graphicle login, first").contains("graphicle setup")
        );
        assert!(
            explain("Configure a local or API model before starting inference")
                .contains("graphicle config set model")
        );
        assert_eq!(explain("Something else"), "Something else");
    }

    #[test]
    fn ollama_check_says_whether_ollama_answers_and_has_the_model() {
        use std::io::{Read, Write};
        // IPv6 loopback first: some build machines refuse IPv4 loopback.
        let Some(listener) = ["[::1]:0", "127.0.0.1:0"]
            .iter()
            .find_map(|address| std::net::TcpListener::bind(address).ok())
        else {
            return;
        };
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for body in [
                r#"{"models":[{"name":"qwen3:4b","model":"qwen3:4b"}]}"#,
                r#"{"models":[{"name":"llama3.2:latest","model":"llama3.2:latest"}]}"#,
                "not Ollama",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]).into_owned();
                assert!(
                    request.starts_with("GET /api/tags HTTP/1.0\r\n"),
                    "{request}"
                );
                write!(
                    stream,
                    "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}"
                )
                .unwrap();
            }
        });
        let url = format!("http://{address}");
        assert_eq!(
            ollama_check(&url, "llama3.2"),
            Some(json!({"answers": true, "pulled": false}))
        );
        assert_eq!(
            ollama_check(&url, "llama3.2"),
            Some(json!({"answers": true, "pulled": true}))
        );
        assert_eq!(
            ollama_check(&url, "llama3.2"),
            Some(json!({"answers": false}))
        );
        server.join().unwrap();
        // Nothing listening there now.
        assert_eq!(
            ollama_check(&url, "llama3.2"),
            Some(json!({"answers": false}))
        );
        // Not read: an HTTPS address.
        assert_eq!(ollama_check("https://ollama.example.com", "llama3.2"), None);
        let text = config_set_text(
            &json!({"path":"/x/models.json","default_model":"ollama",
                "profile":{"model":"llama3.2","protocol":"ollama","base_url":"http://127.0.0.1:11434"},
                "ollama":{"answers":false}}),
            Style::plain(),
        );
        assert!(
            text.contains(
                "Ollama is not answering at http://127.0.0.1:11434: start it (ollama serve)"
            ),
            "{text}"
        );
        let text = config_set_text(
            &json!({"path":"/x/models.json","default_model":"ollama",
                "profile":{"model":"llama3.2","protocol":"ollama","base_url":"http://127.0.0.1:11434"},
                "ollama":{"answers":true,"pulled":false}}),
            Style::plain(),
        );
        assert!(text.contains("run ollama pull llama3.2"), "{text}");
    }

    #[test]
    fn status_counts_symbols_from_the_code_graph_as_setup_does() {
        let mut view = Overview {
            id: "p1".into(),
            name: "shop".into(),
            source: "folder",
            folder: None,
            changes: None,
            status: json!({"files":3,"memories":0,"draft_memories":0,
                "graph":{"coverage":{"symbols":10}}}),
            graph: json!({"status":"queued"}),
            clients: Vec::new(),
            unreadable: Vec::new(),
            capture: Vec::new(),
            guard: Vec::new(),
            git_hooks: None,
            git_missing: Vec::new(),
            model: None,
            server: Probe::Free,
            server_ours: false,
            symbols: None,
        };
        // No code graph yet: no symbol count (the memory index's is not shown).
        let text = status_text(&view, Style::plain());
        assert!(text.contains("3 files, 0 memories"), "{text}");
        assert!(!text.contains("symbol"), "{text}");
        view.symbols = Some(8);
        let text = status_text(&view, Style::plain());
        assert!(text.contains("3 files, 8 symbols, 0 memories"), "{text}");
    }

    #[test]
    fn install_methods_come_from_the_program_path() {
        let home = Path::new("/home/me");
        let cargo = Path::new("/home/me/.cargo/bin");
        let brew = install_method(
            Path::new("/opt/homebrew/Cellar/graphicle/0.1.0/bin/graphicle"),
            Some(home),
            Some(cargo),
            false,
        );
        assert_eq!(brew.method, "homebrew");
        assert!(
            brew.commands
                .contains(&"brew upgrade graphicle".to_string())
        );
        let script = install_method(
            Path::new("/home/me/.local/bin/graphicle"),
            Some(home),
            Some(cargo),
            false,
        );
        assert_eq!(script.method, "script");
        // On Windows the script is install.ps1 (`script_command` below).
        assert!(
            cfg!(windows) || script.commands[0].ends_with("install.sh | sh"),
            "{script:?}"
        );
        let elsewhere = install_method(
            Path::new("/opt/tools/graphicle"),
            Some(home),
            Some(cargo),
            false,
        );
        assert!(
            cfg!(windows) || elsewhere.commands[0].ends_with("--bin-dir /opt/tools"),
            "{elsewhere:?}"
        );
        let built = install_method(
            Path::new("/home/me/src/graphicle/runtime/target/release/graphicle"),
            Some(home),
            Some(cargo),
            false,
        );
        assert_eq!(built.method, "source");
        // `~/src/...` or the full folder; `~\src/...` on Windows.
        assert!(
            built.commands[1]
                .replace('\\', "/")
                .contains("/src/graphicle/runtime"),
            "{built:?}"
        );
        let cargo_install = install_method(
            Path::new("/home/me/.cargo/bin/graphicle"),
            Some(home),
            Some(cargo),
            false,
        );
        assert_eq!(cargo_install.method, "cargo");
        let docker = install_method(Path::new("/usr/local/bin/graphicle"), None, None, true);
        assert_eq!(docker.method, "docker");
    }

    /// The install script's update line: install.sh on macOS and Linux,
    /// install.ps1 in PowerShell on Windows, whatever the host.
    #[test]
    fn script_updates_name_the_platforms_script() {
        let releases = "https://github.com/graphicle-ai/graphicle/releases/latest/download";
        let bin = Path::new("/home/me/.local/bin");
        assert_eq!(
            script_command(bin, Some(bin), false),
            format!("curl -fsSL {releases}/install.sh | sh")
        );
        assert_eq!(
            script_command(Path::new("/opt/my tools"), Some(bin), false),
            format!("curl -fsSL {releases}/install.sh | sh -s -- --bin-dir '/opt/my tools'")
        );
        let programs = Path::new(r"C:\Users\me\AppData\Local\Programs\Graphicle\bin");
        assert_eq!(
            script_command(programs, Some(programs), true),
            format!("irm {releases}/install.ps1 | iex")
        );
        assert_eq!(
            script_command(Path::new(r"D:\O'Brien\tools"), Some(programs), true),
            format!(
                "& ([scriptblock]::Create((irm {releases}/install.ps1))) -InstallDir 'D:\\O''Brien\\tools'"
            )
        );
    }

    #[test]
    fn codex_servers_are_read_from_its_toml() {
        let text = r#"
model = "o3"
[mcp_servers.graphicle-repo]
command = "/opt/homebrew/bin/graphicle"
args = ["--data-dir", "/home/me/Library/Application Support/Graphicle", "mcp", "--project", "abc"]

[mcp_servers."graphicle-two"]
command = '/usr/local/bin/graphicle'
args = [
  "mcp", # the server
  "--project",
  "def",
]
[mcp_servers.graphicle-two.env]
command = "ignored"
[mcp_servers.other]
command = "npx"
args = ["-y", "thing"]
"#;
        let servers = codex_servers(text);
        assert_eq!(servers.len(), 3, "{servers:?}");
        assert_eq!(servers[0].0, "graphicle-repo");
        assert_eq!(servers[0].1, "/opt/homebrew/bin/graphicle");
        assert_eq!(
            servers[0].2[1],
            "/home/me/Library/Application Support/Graphicle"
        );
        assert_eq!(servers[1].0, "graphicle-two");
        assert_eq!(servers[1].1, "/usr/local/bin/graphicle");
        assert_eq!(servers[1].2, vec!["mcp", "--project", "def"]);
        assert_eq!(servers[2].0, "other");
        assert!(is_graphicle(&servers[0].1, &servers[0].2));
        assert!(!is_graphicle(&servers[2].1, &servers[2].2));
    }

    #[test]
    fn hook_commands_are_split_as_a_shell_would() {
        let command = "'/Applications/My Tools/graphicle' --data-dir '/home/me/it'\\''s' hook claude-code stop";
        assert_eq!(first_word(command), "/Applications/My Tools/graphicle");
        assert_eq!(
            words(command),
            vec![
                "/Applications/My Tools/graphicle",
                "--data-dir",
                "/home/me/it's",
                "hook",
                "claude-code",
                "stop"
            ]
        );
    }

    #[test]
    fn answer_models_map_onto_provider_profiles_without_keys() {
        let anthropic = model_profile("anthropic", None, None, None).unwrap();
        assert_eq!(anthropic["protocol"], "anthropic");
        assert_eq!(anthropic["base_url"], "https://api.anthropic.com/v1");
        assert_eq!(anthropic["key_env"], "ANTHROPIC_API_KEY");
        assert_eq!(anthropic["model"], ANTHROPIC_MODEL);
        assert_eq!(anthropic["structured_output"], false);
        let openai = model_profile("openai", Some("gpt-x"), Some("MY_KEY"), None).unwrap();
        assert_eq!(openai["protocol"], "responses");
        assert_eq!(openai["model"], "gpt-x");
        assert_eq!(openai["key_env"], "MY_KEY");
        let ollama = model_profile("ollama", Some("llama3.2:latest"), None, None).unwrap();
        assert_eq!(ollama["protocol"], "ollama");
        assert!(ollama.get("key_env").is_none());
        assert!(model_profile("gemini", None, None, None).is_err());
        // Each profile is valid as Graphicle reads it.
        for profile in [anthropic, openai, ollama] {
            let config: model::Config =
                serde_json::from_value(json!({"default_model":"m","models":{"m":profile}}))
                    .unwrap();
            config.validate().unwrap();
        }
    }

    #[test]
    fn models_are_detected_from_the_environment_only() {
        let home = tempfile::tempdir().unwrap();
        let library = home
            .path()
            .join(".ollama/models/manifests/registry.ollama.ai/library");
        std::fs::create_dir_all(library.join("nomic-embed-text")).unwrap();
        std::fs::write(library.join("nomic-embed-text/latest"), "{}").unwrap();
        std::fs::create_dir_all(library.join("llama3.2")).unwrap();
        std::fs::write(library.join("llama3.2/3b"), "{}").unwrap();
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        let none = detect_models_with(&env(&[]), Some(Path::new("/nonexistent")));
        assert!(none.is_empty(), "{none:?}");
        let keys = detect_models_with(
            &env(&[("ANTHROPIC_API_KEY", "sk"), ("OPENAI_API_KEY", "sk")]),
            Some(Path::new("/nonexistent")),
        );
        assert_eq!(
            keys.iter().map(|d| d.provider).collect::<Vec<_>>(),
            ["anthropic", "openai"]
        );
        assert_eq!(keys[0].command(), "graphicle config set model anthropic");
        let empty_key = detect_models_with(&env(&[("ANTHROPIC_API_KEY", "")]), None);
        assert!(empty_key.is_empty());
        let ollama = detect_models_with(&env(&[]), Some(home.path()));
        assert_eq!(ollama.len(), 1, "{ollama:?}");
        assert_eq!(ollama[0].provider, "ollama");
        assert_eq!(ollama[0].model, "llama3.2:3b");
        assert_eq!(
            ollama[0].command(),
            "graphicle config set model ollama:llama3.2:3b"
        );
    }

    #[test]
    fn key_variables_are_names_and_never_keys() {
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "_MY_KEY",
            "work_key2",
        ] {
            assert!(variable_name(name), "{name}");
            assert!(check_key_env(name).is_ok());
        }
        for key in [
            "sk-ant-api03-SECRET",
            "sk_live_abc",
            "1KEY",
            "",
            "a8f9c054e1b2d3c4a5b6c7d8e9f0a1b2c3d4",
        ] {
            assert!(!variable_name(key), "{key}");
            let refused = check_key_env(key).unwrap_err().message;
            assert!(key.is_empty() || !refused.contains(key), "{refused}");
        }
        assert!(model_profile("anthropic", None, Some("sk-ant-api03-SECRET"), None).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        assert!(config_set(&path, "models.api.key_env", "sk-ant-api03-SECRET").is_err());
        assert!(!path.exists(), "nothing was written");
        assert!(!shown_key_env("sk-ant-api03-SECRET").contains("SECRET"));
    }

    /// `config set limits.<name>` writes one limit, checked as a sync reads
    /// it: a bad value or an unknown limit is refused and nothing is written.
    #[test]
    fn config_set_writes_a_limit_after_checking_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        config_set(&path, "limits.project_files", "250000").unwrap();
        config_set(&path, "limits.project_bytes", "4GiB").unwrap();
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            written["limits"],
            json!({"project_files":250000,"project_bytes":"4GiB"})
        );
        let config = model::Config::load(Some(&path)).unwrap();
        let limits = crate::limits::Limits::resolve(&config.limits, |_| None).unwrap();
        assert_eq!(limits.project_files, 250_000);
        assert_eq!(limits.project_bytes, 4 << 30);
        let before = std::fs::read(&path).unwrap();
        let refused = config_set(&path, "limits.project_files", "0").unwrap_err();
        assert!(
            refused.message.contains("Invalid limits.project_files"),
            "{}",
            refused.message
        );
        assert!(config_set(&path, "limits.files", "10").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    }

    #[test]
    fn config_keys_are_dotted_paths() {
        let mut document = json!({"models":{"a":{"model":"x"}}});
        assert_eq!(config_get(&document, "models.a.model"), Some(json!("x")));
        assert_eq!(config_get(&document, "models.b"), None);
        config_put(&mut document, "models.a.input_limit", json!(32000)).unwrap();
        config_put(&mut document, "worker_concurrency", json!(2)).unwrap();
        assert_eq!(document["models"]["a"]["input_limit"], 32000);
        config_put(&mut document, "worker_concurrency", Value::Null).unwrap();
        assert!(document.get("worker_concurrency").is_none());
        assert!(config_put(&mut document, "models..x", json!(1)).is_err());
        assert_eq!(parse_value("2"), json!(2));
        assert_eq!(parse_value("true"), json!(true));
        assert_eq!(parse_value("qwen3:4b"), json!("qwen3:4b"));
    }

    #[test]
    fn answers_cite_their_sources_by_number_and_line() {
        let job = json!({"kind":"basic","calls":[{"input_tokens":100,"output_tokens":20}],
            "result":{"answer":"login() checks the password.","abstain":false,"unresolved":[],
            "evidence":[{"id":"file:src/auth.py","quote":"return check(user)"}],
            "context":{"sources":[{"id":"file:src/auth.py","path":"src/auth.py","line":1,"end_line":6}],
                "evidence":[{"id":"file:src/auth.py","text":"def login(user):\n    \"\"\"Doc.\"\"\"\n    return check(user)\n"}]}}});
        let text = answer_text(&job, Style::plain());
        assert!(
            text.starts_with("login() checks the password. [1]"),
            "{text}"
        );
        assert!(text.contains("[1] src/auth.py:3"), "{text}");
        assert!(text.contains("return check(user)"), "{text}");
        assert!(
            text.contains("1 model call · 100 in, 20 out tokens"),
            "{text}"
        );
        let unanswered = json!({"result":{"mode":"other"}});
        assert!(answer_text(&unanswered, Style::plain()).contains("\"mode\""));
    }

    /// One file can give several excerpts, all with the file's id: each
    /// shows its own text under its own lines, in `context` and in `ask`
    /// without a model, and a model's quote takes its lines from the excerpt
    /// that holds it.
    #[test]
    fn two_excerpts_of_one_file_keep_their_own_text_and_lines() {
        let first = "import hmac\nfrom .exc import BadSignature\n";
        let second = "def verify(self, value):\n    return hmac.compare_digest(a, b)\n";
        let packet = json!({"question":"where are signatures verified?","tokens":120,"max_tokens":3000,
            "sources":[
                {"id":"file:src/signer.py","path":"src/signer.py","line":1,"end_line":2,"byte_start":0,"byte_end":first.len()},
                {"id":"file:src/signer.py","path":"src/signer.py","line":192,"end_line":193,"byte_start":5000,"byte_end":5000 + second.len()}],
            "evidence":[
                {"id":"file:src/signer.py","text":first,"byte_start":0,"byte_end":first.len()},
                {"id":"file:src/signer.py","text":second,"byte_start":5000,"byte_end":5000 + second.len()}]});
        let text = context_text(&packet, Style::plain());
        let one = text.find("[1] src/signer.py:1-2\n").expect(&text);
        let two = text.find("[2] src/signer.py:192-193\n").expect(&text);
        assert!(
            text[one..two].contains("from .exc import BadSignature"),
            "{text}"
        );
        assert!(!text[one..two].contains("compare_digest"), "{text}");
        assert!(
            text[two..].contains("return hmac.compare_digest(a, b)"),
            "{text}"
        );
        assert!(!text[two..].contains("BadSignature"), "{text}");
        // Windows in another order still pair with their sources by start.
        let mut swapped = packet.clone();
        swapped["evidence"].as_array_mut().unwrap().reverse();
        assert_eq!(context_text(&swapped, Style::plain()), text);

        let job = json!({"kind":"basic","calls":[{"input_tokens":10,"output_tokens":5}],
            "result":{"answer":"verify() compares the signatures.","abstain":false,"unresolved":[],
            "evidence":[{"id":"file:src/signer.py","quote":"return hmac.compare_digest(a, b)"},
                {"id":"file:src/signer.py","quote":"from .exc import BadSignature"}],
            "context":packet}});
        let answer = answer_text(&job, Style::plain());
        assert!(answer.contains("  [1] src/signer.py:193\n"), "{answer}");
        assert!(answer.contains("  [2] src/signer.py:2\n"), "{answer}");
    }

    /// A `reflect` answer lists the windows it read, without `sources`: a
    /// citation still shows its file and lines, and an answer whose review
    /// call failed says it was not reviewed.
    #[test]
    fn reflect_answers_show_cited_lines_and_an_unreviewed_answer() {
        let job = json!({"kind":"reflect","calls":[{"input_tokens":10,"output_tokens":5}],
            "result":{"answer":"SAVE10 takes off 10%.","abstain":false,"unresolved":[],
            "evidence":[{"id":"file:shop/discounts.py","quote":"COUPONS = {\"SAVE10\": 0.10}"}],
            "review":{"status":"failed","reason":"Local model response was incomplete (max_output_tokens); no memory was committed","semantic_proof":false},
            "context":{"evidence":[{"id":"file:shop/discounts.py","title":"shop/discounts.py","byte_start":0,
                "text":"\"\"\"Coupon codes.\"\"\"\nCOUPONS = {\"SAVE10\": 0.10}\n"}]}}});
        let text = answer_text(&job, Style::plain());
        assert!(text.contains("[1] shop/discounts.py:2"), "{text}");
        assert!(
            text.contains("This answer was not reviewed against the sources: the review call failed (Local model response was incomplete (max_output_tokens); no memory was committed)"),
            "{text}"
        );
        let mut reviewed = job.clone();
        reviewed["result"]["review"] = json!({"status":"completed"});
        assert!(!answer_text(&reviewed, Style::plain()).contains("not reviewed"));
    }

    /// `config set model ollama` without `--url` uses the server
    /// `OLLAMA_HOST` names, read as Ollama reads it.
    #[test]
    fn ollama_host_is_read_as_ollama_reads_it() {
        for (value, url) in [
            ("127.0.0.1", "http://127.0.0.1:11434"),
            ("127.0.0.1:11500", "http://127.0.0.1:11500"),
            ("localhost:8080", "http://localhost:8080"),
            ("0.0.0.0", "http://127.0.0.1:11434"),
            ("0.0.0.0:11435", "http://127.0.0.1:11435"),
            (":11436", "http://127.0.0.1:11436"),
            ("[::1]:11434", "http://[::1]:11434"),
            ("::1", "http://[::1]:11434"),
            ("[::]:11434", "http://[::1]:11434"),
            ("http://gpu.lan:11434", "http://gpu.lan:11434"),
            ("http://gpu.lan", "http://gpu.lan:80"),
            (
                "https://ollama.example.com",
                "https://ollama.example.com:443",
            ),
            (
                "https://ollama.example.com/",
                "https://ollama.example.com:443",
            ),
            (" 192.168.1.20 ", "http://192.168.1.20:11434"),
        ] {
            assert_eq!(ollama_url(value).as_deref(), Some(url), "{value}");
        }
        for value in ["", "  ", "ftp://host", "host:port", "[::1"] {
            assert_eq!(ollama_url(value), None, "{value}");
        }
        // A scheme is case-insensitive. Rejecting HTTP:// makes config set
        // ignore OLLAMA_HOST and save http://127.0.0.1:11434.
        for (value, url) in [
            ("HTTP://gpu.lan:11434", "http://gpu.lan:11434"),
            (
                "Https://ollama.example.com",
                "https://ollama.example.com:443",
            ),
            ("HTTP://[::1]:11434", "http://[::1]:11434"),
        ] {
            assert_eq!(ollama_url(value).as_deref(), Some(url), "{value}");
        }
        let profile = model_profile_at("ollama", None, None, None, "http://[::1]:11500").unwrap();
        assert_eq!(profile["base_url"], "http://[::1]:11500");
        let explicit = model_profile_at(
            "ollama",
            None,
            None,
            Some("http://127.0.0.1:9"),
            "http://[::1]:11500",
        )
        .unwrap();
        assert_eq!(explicit["base_url"], "http://127.0.0.1:9");
        let anthropic = model_profile_at("anthropic", None, None, None, "http://[::1]:1").unwrap();
        assert_eq!(anthropic["base_url"], "https://api.anthropic.com/v1");
    }

    #[test]
    fn model_failures_say_what_to_do() {
        let model = ModelState {
            name: "anthropic".into(),
            protocol: "anthropic".into(),
            model: "claude-x".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            key_env: Some("ANTHROPIC_API_KEY".into()),
            key_available: false,
            input_limit: 16000,
            output_limit: 16000,
        };
        let failed = |error: &str| json!({"state":"failed","error":error});
        assert!(
            job_failure(
                &failed("Credential is unavailable; add it in Connections"),
                Some(&model)
            )
            .contains("export ANTHROPIC_API_KEY=")
        );
        assert!(
            job_failure(&failed("Provider returned HTTP 401"), Some(&model))
                .contains("refused the API key")
        );
        assert!(
            job_failure(&failed("Provider returned HTTP 404"), Some(&model))
                .contains("does not know the model claude-x")
        );
        let ollama = ModelState {
            protocol: "ollama".into(),
            base_url: "http://127.0.0.1:11434".into(),
            model: "qwen3:4b".into(),
            key_env: None,
            ..model.clone()
        };
        assert!(
            job_failure(
                &failed("Could not establish the provider connection"),
                Some(&ollama)
            )
            .contains("ollama serve")
        );
        // A refused certificate names the provider and keeps the causes and the hint.
        let untrusted = job_failure(
            &failed(&graphicle::net::with_hint(
                "Could not establish the provider connection: client error (Connect): invalid peer certificate: UnknownIssuer".into(),
            )),
            Some(&model),
        );
        assert!(
            untrusted.starts_with(&format!(
                "Could not reach {}: its certificate is not trusted.",
                model.base_url
            )),
            "{untrusted}"
        );
        assert!(untrusted.contains("UnknownIssuer"), "{untrusted}");
        assert!(untrusted.contains("set SSL_CERT_FILE"), "{untrusted}");
        assert!(
            job_failure(&failed("Provider returned HTTP 404"), Some(&ollama))
                .contains("ollama pull qwen3:4b")
        );
        assert!(
            job_failure(
                &failed("Inference request exceeds the configured model budget"),
                Some(&model)
            )
            .contains("models.anthropic.input_limit 32000")
        );
        let unknown = job_failure(&json!({"calls":[{"error":"Something odd"}]}), None);
        assert!(unknown.starts_with("Something odd."), "{unknown}");
        // A reply cut at the output limit names the setting that raises it.
        let cut = job_failure(
            &failed(
                "Local model response was incomplete (max_output_tokens); no memory was committed",
            ),
            Some(&ModelState {
                name: "ollama".into(),
                output_limit: 2048,
                ..ollama.clone()
            }),
        );
        assert!(
            cut.contains("graphicle config set models.ollama.output_limit 4096"),
            "{cut}"
        );
        let capped = job_failure(
            &failed(
                "Model did not complete a structured answer (max_output_tokens); no memory was committed",
            ),
            Some(&model),
        );
        assert!(capped.contains("8,192 tokens"), "{capped}");
        let cited = job_failure(
            &failed("Answer cites text the model did not read"),
            Some(&model),
        );
        assert!(cited.contains("--workflow reflect"), "{cited}");
        let reflected = job_failure(
            &json!({"kind":"reflect","state":"failed",
                "error":"Answer cites text the model did not read; answer review could not repair it: Answer cites text the model did not read"}),
            Some(&model),
        );
        assert!(
            reflected.contains("its review could not repair the quotes")
                && !reflected.contains("--workflow reflect"),
            "{reflected}"
        );
        let declined = job_failure(
            &failed("Model declined the inference request; no memory was committed"),
            Some(&model),
        );
        assert!(declined.starts_with("The model declined"), "{declined}");
        // The provider's own reason stays in the message.
        let window = job_failure(
            &failed(
                "Provider returned HTTP 400: This model's maximum context length is 4096 tokens.",
            ),
            Some(&model),
        );
        assert!(
            window.contains("models.anthropic.input_limit 8000")
                && window.contains("maximum context length is 4096"),
            "{window}"
        );
        let schema = job_failure(
            &failed("Provider returned HTTP 400: Invalid schema for response_format"),
            Some(&model),
        );
        assert!(
            schema.starts_with("The provider rejected the request")
                && schema.contains("Invalid schema for response_format"),
            "{schema}"
        );
    }

    #[test]
    fn version_stamps_compare_by_number() {
        assert!(version_parts("0.10.0") > version_parts("0.9.9"));
        assert!(version_parts("1.2.0-rc1") == version_parts("1.2.0"));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(note_data_version(dir.path()), None);
        assert_eq!(
            data_version(dir.path()).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        std::fs::write(dir.path().join(STAMP), r#"{"graphicle":"999.0.0"}"#).unwrap();
        let note = note_data_version(dir.path()).unwrap();
        assert!(
            note.contains("999.0.0") && note.contains("graphicle update"),
            "{note}"
        );
        // A newer stamp is never lowered.
        assert_eq!(data_version(dir.path()).as_deref(), Some("999.0.0"));
    }

    #[test]
    fn headless_sessions_print_the_address() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert!(headless_with(&env(&[
            (NO_BROWSER_ENV, "1"),
            ("DISPLAY", ":0")
        ])));
        if cfg!(target_os = "macos") || cfg!(windows) {
            assert!(!headless_with(&env(&[])));
            assert!(headless_with(&env(&[("SSH_CONNECTION", "1 2 3 4")])));
        } else {
            assert!(headless_with(&env(&[])));
            assert!(!headless_with(&env(&[("DISPLAY", ":0")])));
        }
    }

    /// The limits' files and note in the readable sync view and in setup's
    /// summary (a piped sync prints them as JSON, with one line on stderr).
    #[test]
    fn sync_text_lists_limited_and_skipped_files_with_the_reasons() {
        let skipped: Vec<Value> = (0..7)
            .map(|n| json!({"path":format!("big{n}.json"),"reason":"Too large to index (9,000,000 bytes; limit 8,388,608); left out"}))
            .collect();
        let synced = json!({"files":3,"bytes":2048,"replayed":false,
            "limited":[{"path":"main.py","limit":"calls","reason":"Call list truncated at 1 call"}],
            "skipped":skipped,
            "question_limits":"This project has 60,000 text files and 1 GiB of text; one question reads at most 50,000 files"});
        let text = sync_text(&synced, Style::plain());
        assert!(
            text.contains("! 1 file analysed in part (still searchable as text):\n    main.py: Call list truncated at 1 call\n"),
            "{text}"
        );
        assert!(
            text.contains("! 7 files skipped:\n    big0.json: Too large to index"),
            "{text}"
        );
        assert!(!text.contains("big5.json"), "{text}");
        assert!(
            text.contains("    and 2 more: graphicle sync --json lists each with the reason\n"),
            "{text}"
        );
        assert!(
            text.contains("! This project has 60,000 text files"),
            "{text}"
        );
        assert_eq!(
            limits_summary(&synced).unwrap(),
            "1 file analysed in part and 7 files skipped (size limits): graphicle sync lists them with the reasons"
        );
        let plain = json!({"files":3,"bytes":2048,"replayed":false});
        assert!(!sync_text(&plain, Style::plain()).contains('!'));
        assert_eq!(limits_summary(&plain), None);
    }

    #[test]
    fn unknown_words_point_to_the_command_that_does_it() {
        let command = clap::Command::new("graphicle")
            .subcommand(clap::Command::new("context"))
            .subcommand(clap::Command::new("sync"))
            .subcommand(clap::Command::new("doctor"));
        assert!(
            synonym(&command, "search")
                .unwrap()
                .contains("graphicle context")
        );
        assert!(
            synonym(&command, "Index")
                .unwrap()
                .contains("graphicle sync")
        );
        assert!(
            synonym(&command, "health")
                .unwrap()
                .contains("graphicle doctor")
        );
        // Commands the program lacks are never suggested.
        assert_eq!(synonym(&command, "ui"), None);
        assert_eq!(synonym(&command, "frobnicate"), None);
    }
}
