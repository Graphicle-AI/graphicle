// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/cli.py
//! Native command routing; JSON Lines remains available for embedding clients.
use crate::{Result, build, paths};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
pub const HELP: &str = include_str!("assets/cli/help.txt");
/// The name of this crate's executable. Help, usage and error messages,
/// `--version` and the commands written into installed skills, rules and
/// hooks name the program the command line runs as: this one, or the name an
/// embedding executable passes to [`run_as`].
pub const PROGRAM: &str = "graphicle-graph";
/// Exit code of a lookup (`query`, `path`, `explain`) refused because a name
/// matches several nodes; the candidates are listed (Graphify 0.9.86, #4320).
pub const EXIT_AMBIGUOUS: i32 = 3;
/// Exit code of a lookup that matched no node.
pub const EXIT_NOT_FOUND: i32 = 4;
/// `query --seed` without a value fails closed rather than becoming a keyword
/// query (Graphify 0.9.88).
const SEED_NEEDS_VALUE: &str = "--seed needs a value (node id, label or file path)";
thread_local! {
    static RUNNING_AS: std::cell::Cell<&'static str> = const { std::cell::Cell::new(PROGRAM) };
}
/// The program the command line on this thread runs as: [`PROGRAM`] unless
/// [`run_as`] or [`with_program`] named another.
pub fn program() -> &'static str {
    RUNNING_AS.with(std::cell::Cell::get)
}
/// Run `f` with [`program`] answering `program` on this thread, for library
/// entry points other than [`run_as`] (the JSON Lines API, for example).
pub fn with_program<T>(program: &'static str, f: impl FnOnce() -> T) -> T {
    struct Restore(&'static str);
    impl Drop for Restore {
        fn drop(&mut self) {
            RUNNING_AS.with(|name| name.set(self.0));
        }
    }
    let _restore = Restore(RUNNING_AS.with(|name| name.replace(program)));
    f()
}
/// `text`, which spells commands with [`PROGRAM`], for the running
/// [`program`]. Only the command name is replaced: a `.graphicle-graph`
/// folder or a `graphicle-graph://` resource keeps its name.
pub fn program_text(text: &str) -> std::borrow::Cow<'_, str> {
    text_for_program(text, program())
}
/// [`program_text`] for the program `name`.
pub fn text_for_program<'a>(text: &'a str, name: &str) -> std::borrow::Cow<'a, str> {
    if name == PROGRAM {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for at in command_positions(text) {
        out.push_str(&text[copied..at]);
        out.push_str(name);
        copied = at + PROGRAM.len();
    }
    out.push_str(&text[copied..]);
    std::borrow::Cow::Owned(out)
}
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || "_-".contains(c)
}
/// Where `text` names [`PROGRAM`] as a command, the places
/// [`program_text`] replaces.
fn command_positions(text: &str) -> impl Iterator<Item = usize> + '_ {
    text.match_indices(PROGRAM).filter_map(move |(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + PROGRAM.len()..].chars().next();
        let word = |c: Option<char>| c.is_some_and(is_name_char);
        let command = !word(before) && before != Some('.') && !word(after) && after != Some(':');
        command.then_some(at)
    })
}
/// Whether `installed` is `text` (spelled with [`PROGRAM`]) as written by
/// some program: a file or hook block that another executable embedding this
/// command line wrote differs from this program's only in the command name.
pub fn same_but_program(installed: &str, text: &str) -> bool {
    let Some(first) = command_positions(text).next() else {
        return installed == text;
    };
    let Some(rest) = installed.strip_prefix(&text[..first]) else {
        return false;
    };
    let name = &rest[..rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len())];
    !name.is_empty() && installed == text_for_program(text, name)
}
#[derive(Default, Serialize, Deserialize)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}
impl Output {
    pub fn line(&mut self, s: impl AsRef<str>) {
        self.stdout.push_str(s.as_ref());
        self.stdout.push('\n');
    }
    pub fn warn(&mut self, s: impl AsRef<str>) {
        self.stderr.push_str(s.as_ref());
        self.stderr.push('\n');
    }
    pub fn diagnostics(&mut self, d: build::Diagnostics) {
        for s in d.stdout {
            self.line(s);
        }
        for s in d.stderr {
            self.warn(s);
        }
    }
    pub fn value(&mut self, v: &Value) {
        self.line(crate::serve_text::json_text(v, Some(2), true));
    }
}
#[derive(Default, Clone)]
pub struct Args {
    pub pos: Vec<String>,
    pub values: BTreeMap<String, Vec<String>>,
    pub flags: BTreeSet<String>,
}
impl Args {
    pub fn parse(args: &[String], literal: usize) -> Result<Self> {
        let mut out = Self::default();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if i < literal || !arg.starts_with('-') || arg == "-" {
                out.pos.push(arg.clone());
                i += 1;
                continue;
            }
            if arg == "--" {
                out.pos.extend_from_slice(&args[i + 1..]);
                break;
            }
            if ["-h", "-?"].contains(&arg.as_str()) {
                out.flags.insert("help".into());
                i += 1;
                continue;
            }
            let raw = arg.strip_prefix("--").unwrap_or(arg);
            if let Some((k, v)) = raw.split_once('=') {
                if arg == "--seed=" {
                    return Err(SEED_NEEDS_VALUE.into());
                }
                out.values.entry(k.into()).or_default().push(v.into());
                i += 1;
                continue;
            }
            if [
                "help",
                "dfs",
                "json",
                "directed",
                "undirected",
                "force",
                "no-cluster",
                "no-viz",
                "no-label",
                "missing-only",
                "code-only",
                "google-workspace",
                "no-gitignore",
                "global",
                "cargo",
                "allow-partial",
                "allow-dedup-shrink",
                "timing",
                "no-dedup",
                "dedup-llm",
                "project",
                "strict",
                "purge",
                "if-stale",
                "deep",
                "stateless",
                "json-response",
                "follow-symlinks",
                "notify-only",
                "triage",
                "worktrees",
                "conflicts",
                "wrong-base",
                "watch",
                "precise",
                "retrieval",
                "explain",
                "include-tests",
                "docs",
                "routes",
                "dry-run",
                "all",
            ]
            .contains(&raw)
            {
                out.flags.insert(raw.into());
                i += 1;
                continue;
            }
            i += 1;
            if raw == "nodes" {
                let mut values = vec![];
                while i < args.len() && !args[i].starts_with("--") {
                    values.push(args[i].clone());
                    i += 1;
                }
                out.values.insert(raw.into(), values);
                continue;
            }
            let v = args.get(i).ok_or_else(|| {
                if arg == "--seed" {
                    SEED_NEEDS_VALUE.to_string()
                } else {
                    format!("--{raw} requires a value")
                }
            })?;
            out.values.entry(raw.into()).or_default().push(v.clone());
            i += 1;
        }
        Ok(out)
    }
    pub fn flag(&self, k: &str) -> bool {
        self.flags.contains(k)
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.values
            .get(k)
            .and_then(|v| v.last())
            .map(String::as_str)
    }
    pub fn all(&self, k: &str) -> Vec<String> {
        self.values.get(k).cloned().unwrap_or_default()
    }
    pub fn value<'a>(&'a self, k: &str, default: &'a str) -> &'a str {
        self.get(k).unwrap_or(default)
    }
    pub fn positional(&self, i: usize) -> Option<&str> {
        self.pos.get(i).map(String::as_str)
    }
    pub fn require(&self, i: usize, usage: &str) -> Result<&str> {
        Ok(self
            .positional(i)
            .ok_or_else(|| format!("Usage: {} {usage}", program()))?)
    }
    pub fn number(&self, k: &str, default: i64) -> Result<i64> {
        Ok(self
            .get(k)
            .map(|s| s.parse().map_err(|_| format!("--{k} must be an integer")))
            .unwrap_or(Ok(default))?)
    }
    pub fn float(&self, k: &str, default: f64) -> Result<f64> {
        Ok(self
            .get(k)
            .map(|s| {
                s.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| format!("--{k} must be a finite number"))
            })
            .unwrap_or(Ok(default))?)
    }
    pub fn positive(&self, k: &str, default: i64) -> Result<i64> {
        let n = self.number(k, default)?;
        if n <= 0 {
            return Err(format!("--{k} must be > 0 (got {n})").into());
        }
        Ok(n)
    }
    pub fn positive_float(&self, k: &str, default: f64) -> Result<f64> {
        let n = self.float(k, default)?;
        if n <= 0.0 {
            return Err(format!("--{k} must be > 0 (got {n})").into());
        }
        Ok(n)
    }
    pub fn path(&self, k: &str, default: impl AsRef<Path>) -> PathBuf {
        self.get(k)
            .map(expand)
            .unwrap_or_else(|| default.as_ref().into())
    }
    pub fn graph(&self) -> PathBuf {
        self.path("graph", paths::default_graph_json())
    }
    pub fn direction(&self) -> Result<Option<bool>> {
        if self.flag("directed") && self.flag("undirected") {
            return Err("--directed and --undirected are mutually exclusive".into());
        }
        Ok(if self.flag("directed") {
            Some(true)
        } else if self.flag("undirected") {
            Some(false)
        } else {
            None
        })
    }
}
pub fn expand(s: &str) -> PathBuf {
    if s == "~" || s.starts_with("~/") {
        crate::install::Options::default()
            .home
            .join(s.strip_prefix("~/").unwrap_or(""))
    } else {
        s.into()
    }
}
pub fn force(args: &Args) -> bool {
    args.flag("force")
        || std::env::var("GRAPHICLE_FORCE")
            .is_ok_and(|s| ["1", "true", "yes"].contains(&s.to_lowercase().as_str()))
}
pub fn load(path: &Path, direction: Option<bool>) -> Result<crate::graph::Graph> {
    load_as(path, direction, None)
}
pub fn load_as(
    path: &Path,
    direction: Option<bool>,
    multigraph: Option<bool>,
) -> Result<crate::graph::Graph> {
    if !path.is_file() {
        return Err(format!("graph file not found: {}", path.display()).into());
    }
    let mut raw = crate::cli_state::read_graph(path)?;
    if raw.get("links").is_none() {
        raw["links"] = raw.get_mut("edges").map(Value::take).unwrap_or(json!([]));
    }
    if let Some(d) = direction {
        raw["directed"] = json!(d);
    }
    if let Some(multigraph) = multigraph {
        raw["multigraph"] = json!(multigraph);
    }
    if let Some(links) = raw.get_mut("links").and_then(Value::as_array_mut) {
        crate::extractors::evidence::filter_links(links);
        for e in links {
            if let Some(m) = e.as_object_mut() {
                let u = m.get("source").cloned().unwrap_or(Value::Null);
                let v = m.get("target").cloned().unwrap_or(Value::Null);
                m.entry("_src").or_insert(u);
                m.entry("_tgt").or_insert(v);
            }
        }
    }
    let hyperedges = raw
        .get_mut("hyperedges")
        .filter(|v| v.is_array())
        .map(Value::take);
    let mut g = crate::graph::Graph::from_node_link_owned(raw, "links")?;
    if let Some(h) = hyperedges {
        g.attributes["hyperedges"] = h;
    }
    Ok(g)
}
/// `load_as` plus the search index, through the query snapshot when it is
/// current (see `crate::snapshot`). Errors are those of `load_as`.
pub fn load_indexed(
    path: &Path,
    direction: Option<bool>,
    multigraph: Option<bool>,
    variant: crate::snapshot::Variant,
) -> Result<(crate::graph::Graph, crate::serve_search::SearchIndex)> {
    if !path.is_file() {
        return Err(format!("graph file not found: {}", path.display()).into());
    }
    crate::security::check_graph_file_size_cap(path)?;
    let build = || {
        let g = load_as(path, direction, multigraph)?;
        let index = crate::serve_search::SearchIndex::new(&g);
        Ok((g, index))
    };
    // A `--min-evidence` load has its own snapshot (the filtered graph).
    crate::snapshot::load(path, variant, build)
}
/// Writes the `query` snapshot for `path` now, as its first query would, so
/// the first query after `update` or a watch rebuild reads it (a cold
/// `extract` leaves it to the first query: it is benchmarked on its own).
/// Answers are unchanged; skipped when snapshots are off or cannot be
/// written (no `cache/` next to graph.json).
pub fn prewarm_query_snapshot(path: &Path) {
    let variant = crate::snapshot::Variant::Query;
    if !crate::snapshot::enabled()
        || crate::extractors::evidence::min_level() > 0
        || crate::snapshot::path_for(path, variant).is_none()
        || !path.is_file()
    {
        return;
    }
    if let Err(e) = load_indexed(path, None, None, variant) {
        log::debug!("query snapshot not written for {}: {e}", path.display());
    }
}
/// Run the command line as [`PROGRAM`].
pub fn run(args: &[String]) -> Output {
    run_as(PROGRAM, args)
}
/// Run the command line as `program`, the name of the executable that embeds
/// it: its help, messages and installed files name `program` (see
/// [`PROGRAM`]).
pub fn run_as(program: &'static str, args: &[String]) -> Output {
    with_program(program, || run_program(args))
}
fn run_program(args: &[String]) -> Output {
    let mut out = Output::default();
    if !args.iter().any(|s| {
        ["install", "uninstall", "hook-check", "hook-guard", "serve"].contains(&s.as_str())
    }) {
        let options = crate::install::Options::default();
        // Graphify 0.9.72 (#1805): refresh skills an upgrade left behind
        // first, so the check below only warns for copies the refresh could
        // not or must not touch.
        for message in crate::install::refresh_stale_skills(&options) {
            out.warn(message);
        }
        // Graphify 0.9.84 (#3554/#3732): OpenCode plugins an older release wrote.
        for message in crate::install::refresh_v1_opencode_plugins(&options) {
            out.warn(message);
        }
        let mut destinations = BTreeMap::<PathBuf, Vec<String>>::new();
        for name in crate::install::platforms() {
            if let Ok(path) = crate::install::destination(&name, &options) {
                destinations.entry(path).or_default().push(name);
            }
        }
        for (path, names) in destinations {
            for warning in crate::install::check_skill_version(&path, &names) {
                out.warn(warning);
            }
        }
    }
    if let Err(e) = run_into(args, &mut out) {
        out.warn(format!("error: {e}"));
        out.exit_code = 1;
    }
    out
}
fn run_into(args: &[String], out: &mut Output) -> Result<()> {
    // 0.9.72: the usage text opens with the logo banner (terminal only).
    let Some(command) = args.first().map(String::as_str) else {
        crate::install_platforms::print_banner();
        out.line(program_text(HELP));
        return Ok(());
    };
    if ["-h", "--help", "-?", "help"].contains(&command) {
        crate::install_platforms::print_banner();
        out.line(program_text(HELP));
        return Ok(());
    }
    if ["-v", "--version", "version"].contains(&command) {
        out.line(format!("{} {}", program(), crate::VERSION));
        return Ok(());
    }
    if command == "hook-check" {
        return Ok(());
    }
    if command == "prs" {
        return crate::cli_commands::prs(&args[1..], out);
    }
    // Before parsing: an unsupported option would take the next word as its
    // value (see crate::archived).
    if command == "watch" {
        crate::archived::check_watch(&args[1..])?;
    }
    let literal = match command {
        "query" | "explain" | "affected" | "retrieve" => 1,
        "path" => 2,
        _ => 0,
    };
    let a = Args::parse(&args[1..], literal.min(args.len() - 1))?;
    if a.flag("precise") {
        crate::extract::PRECISE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if a.flag("retrieval") {
        crate::retrieve::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if a.flag("docs") {
        crate::extractors::rst::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if a.flag("routes") {
        crate::extractors::routes::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // Opt-in: leave out call edges below an evidence level when loading.
    if let Some(level) = a
        .get("min-evidence")
        .map(str::to_string)
        .or_else(|| std::env::var("GRAPHICLE_MIN_EVIDENCE").ok())
    {
        let rank = crate::extractors::evidence::rank(&level).ok_or_else(|| {
            format!(
                "--min-evidence must be one of: {}",
                crate::extractors::evidence::LEVELS.join(", ")
            )
        })?;
        crate::extractors::evidence::MIN_LEVEL.store(rank, std::sync::atomic::Ordering::Relaxed);
    }
    if a.flag("help") {
        out.line(program_text(HELP));
        return Ok(());
    }
    if command == "install"
        || command == "uninstall"
        || crate::install::platforms()
            .iter()
            .any(|p| p == crate::install::canonical(command))
    {
        return crate::cli_commands::install(command, &a, out);
    }
    // Opt-in: a resident server (`serve --socket`) answers the read-only
    // graph commands when GRAPHICLE_SOCKET names one that accepts them.
    if let Some(result) = crate::serve_socket::try_forward(command, &a, out) {
        return result;
    }
    match command {
        "extract" => crate::cli_extract::run(&a, out),
        "cluster-only" | "label" => crate::cli_cluster::run(command, &a, out),
        "query" | "path" | "explain" | "affected" | "god-nodes" | "god_nodes" => {
            crate::cli_commands::query(command, &a, out)
        }
        // Opt-in beyond the upstream port: ranked, cited evidence within a budget.
        "retrieve" => crate::cli_commands::retrieve(&a, out),
        // Where each long-tail grammar comes from in this build.
        "grammars" => {
            out.line(crate::grammar_packs::report());
            Ok(())
        }
        "hook" => crate::cli_commands::hook(&a, out),
        "update" => crate::cli_commands::update(&a, out),
        "watch" => crate::cli_commands::watch(&a, out),
        "serve" => crate::cli_commands::serve(&a, out),
        "hook-guard" => {
            use std::io::Read;
            // Fail open, as upstream: stdin is decoded with replacement
            // characters and an unreadable or non-JSON payload prints nothing,
            // so a guard never fails the tool call it inspects.
            let mut input = Vec::new();
            if a.positional(0) != Some("gemini") {
                let _ = std::io::stdin().read_to_end(&mut input);
            }
            out.stdout.push_str(&crate::cli_guard::run(
                a.positional(0).unwrap_or(""),
                &serde_json::from_str(&String::from_utf8_lossy(&input)).unwrap_or(Value::Null),
                a.flag("strict"),
            ));
            Ok(())
        }
        "export" => crate::cli_export::run(&a, out),
        "provider" => crate::cli_commands::provider(&a, out),
        "global" => crate::cli_commands::global(&a, out),
        "merge-driver" | "merge-graphs" | "merge-chunks" | "merge-semantic" => {
            crate::cli_merge::run(command, &a, out)
        }
        "save-result" | "reflect" | "add" | "tree" | "diagnose" | "benchmark" | "cache-check"
        | "check-update" | "clone" => crate::cli_commands::misc(command, &a, out),
        path if Path::new(path).exists()
            || [".", ".."].contains(&path)
            || path.starts_with(['.', '/', '~']) =>
        {
            let a = Args::parse(args, 1)?;
            crate::cli_extract::run(&a, out)
        }
        _ => Err(format!(
            "unknown command '{command}'. Run {} --help for usage.",
            program()
        )
        .into()),
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    use crate::cli_state as s;
    let p = |k| Path::new(r.get(k).and_then(Value::as_str).unwrap_or("."));
    let text = |k| r.get(k).and_then(Value::as_str).unwrap_or("");
    Ok(match r["action"].as_str().unwrap_or("run") {
        "run" => serde_json::to_value(run_as(
            program(),
            &serde_json::from_value::<Vec<String>>(r["args"].clone())
                .map_err(crate::Error::from)?,
        ))
        .map_err(crate::Error::from)?,
        "stamped" => s::stamped(
            &r["files"],
            &r["semantic"],
            p("root"),
            &s::strings(&r["partial"]),
            &s::strings(&r["failed"]),
            &s::strings(&r["unverified"]),
        ),
        "stale" => {
            let (v, kept) = s::stale_sources(
                p("graph"),
                p("root"),
                &s::strings(&r["seen"]).into_iter().collect(),
                &r["detection"],
            );
            json!({"sources":v,"kept_alive":kept})
        }
        "zero_sources" => json!(s::zero_sources(
            p("graph"),
            p("root"),
            &s::strings(&r["unchanged"]),
            r["semantic"].as_bool().unwrap_or(false)
        )),
        "prune" => json!(s::prune_sources(p("graph"), &s::strings(&r["stale"]))?),
        "bash_search" => json!(crate::cli_guard::bash_search(text("command"))),
        "cwd_relative" => json!(crate::cli_guard::cwd_relative(
            text("path"),
            r["windows"].as_bool().unwrap_or(cfg!(windows))
        )),
        "target_indexed" => json!(crate::cli_guard::target_indexed(text("path"), p("root"))),
        "guard" => json!(crate::cli_guard::run(
            text("kind"),
            &r["payload"],
            r["strict"].as_bool().unwrap_or(false)
        )),
        "strict" => json!(crate::cli_guard::strict(
            r["flag"].as_bool().unwrap_or(false)
        )),
        "touch_query" => {
            crate::cli_guard::touch_query(p("graph"));
            Value::Null
        }
        "query_fresh" => json!(crate::cli_guard::query_fresh()),
        "mark_denied" => json!(crate::cli_guard::mark_denied(text("session"))),
        x => return Err(format!("Unknown CLI action: {x}").into()),
    })
}
#[cfg(test)]
mod tests {
    use super::{
        PROGRAM, program, program_text, run, run_as, same_but_program, text_for_program,
        with_program,
    };

    /// An embedding executable's name reaches help, usage errors and
    /// `--version`, only for its own run, and only where a command is named.
    #[test]
    fn run_as_names_the_embedding_program() {
        let args = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(program(), PROGRAM);
        let version = run_as("embedding-cli", &args(&["--version"]));
        assert_eq!(
            version.stdout,
            format!("embedding-cli {}\n", crate::VERSION)
        );
        assert_eq!(program(), PROGRAM, "the name is restored after the run");
        assert_eq!(
            run(&args(&["--version"])).stdout,
            format!("{PROGRAM} {}\n", crate::VERSION)
        );
        let help = run_as("embedding-cli", &args(&["--help"])).stdout;
        assert!(help.starts_with("Usage: embedding-cli <command>"), "{help}");
        let unknown = run_as("embedding-cli", &args(&["no-such-command"]));
        assert!(
            unknown
                .stderr
                .contains("Run embedding-cli --help for usage."),
            "{}",
            unknown.stderr
        );
        let text = "`graphicle-graph query`; ~/.graphicle-graph/; graphicle-graph://report";
        assert_eq!(program_text(text), text);
        with_program("embedding-cli", || {
            assert_eq!(
                program_text(text),
                "`embedding-cli query`; ~/.graphicle-graph/; graphicle-graph://report"
            );
            assert_eq!(
                super::Args::default()
                    .require(0, "query QUESTION")
                    .err()
                    .map(|e| e.to_string()),
                Some("Usage: embedding-cli query QUESTION".to_string())
            );
        });
    }

    /// Text written for one program matches the packaged text for any other
    /// program name, in both directions, and only when the command name is
    /// all that differs.
    #[test]
    fn installed_text_is_compared_modulo_the_program_name() {
        let text = "Run `graphicle-graph extract .`, then graphicle-graph query.\n\
                    Config: ~/.graphicle-graph/providers.json; graphicle-graph://report\n";
        let other = text_for_program(text, "embedding-cli");
        assert_eq!(
            other,
            "Run `embedding-cli extract .`, then embedding-cli query.\n\
             Config: ~/.graphicle-graph/providers.json; graphicle-graph://report\n"
        );
        assert!(same_but_program(text, text));
        assert!(same_but_program(&other, text));
        with_program("embedding-cli", || {
            assert!(same_but_program(text, text));
            assert!(same_but_program(&program_text(text), text));
        });
        // One name throughout, and nothing else may differ.
        let mixed = other.replacen("embedding-cli", "graphicle-graph", 1);
        assert!(!same_but_program(&mixed, text));
        assert!(!same_but_program(&other.replace("query", "explain"), text));
        assert!(!same_but_program(&format!("{other}# local edit\n"), text));
        assert!(!same_but_program("", text));
        assert!(same_but_program("no command", "no command"));
        assert!(!same_but_program("no command.", "no command"));
    }
}
