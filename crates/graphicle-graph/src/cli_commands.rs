// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/cli.py
use crate::{
    Result, build,
    cli::{Args, Output, expand},
    cli_state::{ar, strings},
    paths, serve_search as search,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
/// `retrieve <question> [--graph PATH] [--budget N] [--root DIR] [--include-tests] [--json [--explain]]`.
pub fn retrieve(a: &Args, out: &mut Output) -> Result<()> {
    let question = a.require(
        0,
        "retrieve <question> [--graph PATH] [--budget N] [--include-tests] [--json [--explain]]",
    )?;
    let gp = crate::cli_state::resolve(&a.graph());
    let (g, index) =
        crate::serve_socket::shared_indexed(&gp, None, None, crate::snapshot::Variant::Query)?;
    let budget = a.number("budget", 2000)?.clamp(100, 100_000) as usize;
    let options = crate::retrieve::Options {
        budget,
        snippet_lines: 20,
        root: a
            .get("root")
            .map(expand)
            .or_else(|| crate::retrieve::root_for(&gp)),
        include_tests: a.flag("include-tests"),
    };
    let (items, ranking) = crate::retrieve::retrieve_ranked(&g, &index, question, &options);
    if a.flag("json") {
        let mut value = crate::retrieve::render_json(question, &items, budget);
        if a.flag("explain") {
            value["ranking"] = crate::retrieve::ranking_json(&ranking, 200);
        }
        out.line(value.to_string());
    } else {
        out.line(crate::retrieve::render_text(question, &items, budget));
    }
    Ok(())
}
pub fn query(command: &str, a: &Args, out: &mut Output) -> Result<()> {
    let gp = crate::cli_state::resolve(&a.graph());
    let (g, index) = crate::serve_socket::shared_indexed(
        &gp,
        if command == "query" { None } else { Some(true) },
        (command == "path").then_some(true),
        match command {
            "query" => crate::snapshot::Variant::Query,
            "path" => crate::snapshot::Variant::Path,
            _ => crate::snapshot::Variant::Explain,
        },
    )?;
    index.share_traversal(&g);
    let q = a.positional(0).unwrap_or("");
    let answered = Instant::now();
    let result = query_loaded(command, a, out, &gp, g, &index, q);
    if crate::snapshot::timing() {
        eprintln!(
            "[graphicle timing] {command} {:.1} ms",
            answered.elapsed().as_secs_f64() * 1e3
        );
    }
    result
}
fn query_loaded(
    command: &str,
    a: &Args,
    out: &mut Output,
    gp: &Path,
    g: Arc<crate::graph::Graph>,
    index: &search::SearchIndex,
    q: &str,
) -> Result<()> {
    let gp = gp.to_path_buf();
    match command {
        "query" => {
            a.require(0, "query <question> [--seed X] [--graph PATH]")?;
            let mode = if a.flag("dfs") { "dfs" } else { "bfs" };
            let budget = a.number("budget", 2000)?;
            let seeds = a.all("seed");
            let start = Instant::now();
            let result = crate::serve_text::query_text(
                &g,
                index,
                q,
                crate::serve_text::QueryOptions {
                    mode,
                    depth: 2,
                    budget,
                    filters: &a.all("context"),
                    path: gp.to_str(),
                    seeds: Some(&seeds),
                    // The root `affected` derives (#2706): <root>/.graphicle/graph.json.
                    seed_root: graph_root_for(&gp),
                },
            )?;
            crate::querylog::log_query(
                &json!({"kind":"query","question":q,"corpus":gp,"result":result,"duration_ms":start.elapsed().as_secs_f64()*1000.0,"extra":{"mode":mode,"depth":2,"token_budget":budget,"seeds":(!seeds.is_empty()).then_some(&seeds)}}),
            );
            crate::cli_guard::touch_query(&gp);
            out.line(&result);
            if result == crate::serve_text::QUERY_NO_MATCH {
                out.exit_code = crate::cli::EXIT_NOT_FOUND;
            }
        }
        "path" => {
            a.direction()?;
            let target = a.require(1, "path <source> <target> [--graph PATH]")?;
            let answer = crate::serve_find::shortest_path_cli(
                &g,
                index,
                &json!({"source":q,"target":target,"undirected":a.flag("undirected"),"max_hops":i64::MAX}),
            )?;
            if let Some(refusal) = answer.refusal {
                out.warn(
                    answer
                        .text
                        .replacen("No node matching source ", "No node matching ", 1)
                        .replacen("No node matching target ", "No node matching ", 1),
                );
                out.exit_code = match refusal {
                    crate::serve_find::Refusal::Ambiguous => crate::cli::EXIT_AMBIGUOUS,
                    crate::serve_find::Refusal::NotFound => crate::cli::EXIT_NOT_FOUND,
                };
                return Ok(());
            }
            let result = answer
                .text
                .replace("Retry with undirected=true", "Re-run with --undirected");
            for line in result.lines() {
                if line.starts_with("warning:") {
                    out.warn(line);
                } else {
                    out.line(line);
                }
            }
            if let Some(hops) = answer.hops {
                crate::cli_guard::touch_query(&gp);
                crate::querylog::log_query(
                    &json!({"kind":"path","question":format!("{q} -> {target}"),"corpus":gp,"nodes_returned":hops}),
                );
            }
        }
        "explain" => {
            a.require(0, "explain <node> [--all] [--graph PATH]")?;
            let show_all = a.flag("all");
            let i = match crate::serve_find::resolve(&g, index, q) {
                Ok(i) => i,
                Err(e) => {
                    out.line(&e);
                    out.exit_code = match crate::serve_find::Refusal::of(&e) {
                        crate::serve_find::Refusal::Ambiguous => crate::cli::EXIT_AMBIGUOUS,
                        crate::serve_find::Refusal::NotFound => crate::cli::EXIT_NOT_FOUND,
                    };
                    return Ok(());
                }
            };
            let get = |k| search::text(search::field(&g, i, k));
            out.line(format!(
                "Node: {}\n  ID:        {}\n{}\n  Type:      {}\n  Community: {}",
                search::label(&g, i),
                search::id(&g, i),
                format!(
                    "  Source:    {} {}",
                    get("source_file"),
                    get("source_location")
                )
                .trim_end(),
                get("file_type"),
                if get("community_name").is_empty() {
                    get("community")
                } else {
                    get("community_name")
                }
            ));
            if let Ok(overlay) = crate::reflect::load_learning_overlay(&gp) {
                let e = &overlay[search::id(&g, i)];
                if e.is_object() {
                    let status = crate::security::sanitize_label(&e["status"]);
                    let mut line = match status.as_str() {
                        "contested" => format!(
                            "  Lesson: contested (useful {} / dead-end {})",
                            e["uses"], e["neg"]
                        ),
                        "preferred" => format!(
                            "  Lesson: preferred source (start here) — {} useful, score={}",
                            e["uses"], e["score"]
                        ),
                        _ => format!(
                            "  Lesson: {} — {} useful, score={}",
                            if status.is_empty() {
                                "tentative"
                            } else {
                                &status
                            },
                            e["uses"],
                            e["score"]
                        ),
                    };
                    if e["stale"] == true {
                        line.push_str(" [code changed since — re-verify]");
                    }
                    out.line(line);
                }
            }
            let degrees = search::degrees(&g);
            out.line(format!("  Degree:    {}", degrees[i]));
            let mut connections = vec![];
            for incoming in [false, true] {
                for nb in search::neighbors(&g, i, incoming) {
                    let (u, v) = if incoming { (nb, i) } else { (i, nb) };
                    let e = build::edge_data(&g, &g.nodes[u].id, &g.nodes[v].id)?.clone();
                    let src = e.get("_src").unwrap_or(&g.nodes[u].id);
                    let forward = src == &g.nodes[i].id;
                    connections.push((forward, nb, e));
                }
            }
            if !connections.is_empty() {
                out.line(format!("\nConnections ({}):", connections.len()));
                connections.sort_by_key(|(_, nb, _)| std::cmp::Reverse(degrees[*nb]));
                let shown = if show_all {
                    connections.len()
                } else {
                    connections.len().min(20)
                };
                for (forward, nb, e) in &connections[..shown] {
                    let text = |k| e.get(k).map(search::text).unwrap_or_default();
                    let loc = text("source_location");
                    let at = if loc.is_empty() {
                        String::new()
                    } else {
                        format!(" {}:{loc}", text("source_file"))
                    };
                    out.line(format!(
                        "  {} {} [{}] [{}]{at}",
                        if *forward { "-->" } else { "<--" },
                        search::label(&g, *nb),
                        text("relation"),
                        text("confidence")
                    ));
                }
                if connections.len() > shown {
                    out.line(format!(
                        "  ... and {} more (--all lists all {})",
                        connections.len() - shown,
                        connections.len()
                    ));
                    let mut grouped = BTreeMap::new();
                    for (forward, _, e) in &connections[shown..] {
                        let sf = e
                            .get("source_file")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("(unknown file)");
                        *grouped
                            .entry((if *forward { "out" } else { "in" }, sf))
                            .or_insert(0usize) += 1;
                    }
                    let mut grouped = grouped.into_iter().collect::<Vec<_>>();
                    grouped.sort_by_key(|(k, n)| (std::cmp::Reverse(*n), *k));
                    out.line("  Grouped by file:");
                    for ((direction, sf), n) in grouped.iter().take(20) {
                        out.line(format!(
                            "    {} {sf}: {n} connection{}",
                            if *direction == "out" { "-->" } else { "<--" },
                            if *n == 1 { "" } else { "s" }
                        ));
                    }
                    if grouped.len() > 20 {
                        out.line(format!("    ... and {} more files", grouped.len() - 20));
                    }
                }
            }
            crate::querylog::log_query(
                &json!({"kind":"explain","question":q,"corpus":gp,"nodes_returned":connections.len()}),
            );
            crate::cli_guard::touch_query(&gp);
        }
        "affected" => {
            a.require(0, "affected <node> [--graph PATH]")?;
            let g = crate::serve_socket::shared_graph(&gp)?;
            let root = graph_root_for(&gp);
            let mut rels = a.all("relation");
            if rels.is_empty() {
                rels = crate::affected::DEFAULT_AFFECTED_RELATIONS
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            }
            out.line(crate::affected::format_affected(
                &g,
                q,
                &rels,
                a.number("depth", 2)?,
                root,
            )?);
        }
        _ => {
            let g = crate::serve_socket::shared_graph(&gp)?;
            let gods = crate::analyze::god_nodes(
                &g,
                a.number("top", 10)?,
                a.get("exclude-hubs")
                    .map(|_| a.float("exclude-hubs", 0.0))
                    .transpose()?,
            )?;
            if a.flag("json") {
                out.value(&json!(gods));
            } else {
                out.line("God nodes (most connected):");
                for (i, n) in gods.iter().enumerate() {
                    out.line(format!(
                        "  {}. {} - {} edges",
                        i + 1,
                        crate::security::sanitize_label(&n["label"]),
                        n["degree"]
                    ));
                }
            }
        }
    }
    Ok(())
}
/// The analysed repository root of a graph file: `<root>/.graphicle/graph.json`,
/// else the file's own directory (Graphify 0.9.88 `_graph_root_for`, as
/// `affected` derived it, #2706).
fn graph_root_for(gp: &Path) -> Option<&Path> {
    if gp.parent().is_some_and(|p| {
        p.file_name()
            .is_some_and(|n| n == paths::OUT_DIR_NAME.as_str())
    }) {
        gp.parent().and_then(Path::parent)
    } else {
        gp.parent()
    }
}
pub fn install(command: &str, a: &Args, out: &mut Output) -> Result<()> {
    let generic = ["install", "uninstall"].contains(&command);
    for flag in &a.flags {
        if !["project", "strict", "purge", "help"].contains(&flag.as_str())
            || command == "install" && flag == "purge"
            || command == "uninstall" && flag == "strict"
        {
            return Err(crate::Error::invalid(format!(
                "unknown {command} option '--{flag}'"
            )));
        }
    }
    if let Some(option) = a.values.keys().find(|key| key.as_str() != "platform") {
        return Err(crate::Error::invalid(format!(
            "unknown {command} option '--{option}'"
        )));
    }
    if generic {
        let selections: std::collections::BTreeSet<_> = a
            .all("platform")
            .into_iter()
            .chain(a.pos.iter().cloned())
            .collect();
        if selections.len() > 1 {
            return Err(crate::Error::invalid("specify install platform only once"));
        }
    } else if command == "pi" && a.positional(0) == Some("guard") {
        // `pi guard install|uninstall|path` (Graphify 0.9.89).
        let verb = a.positional(1).filter(|_| a.pos.len() == 2);
        let Some(verb) = verb.filter(|v| ["install", "uninstall", "path"].contains(v)) else {
            out.warn(format!(
                "Usage: {0} pi [install|uninstall] | {0} pi guard [install|uninstall|path]",
                crate::cli::program()
            ));
            out.exit_code = 1;
            return Ok(());
        };
        let (lines, errors, code) = crate::install::pi_guard(verb)?;
        for line in lines {
            out.line(line);
        }
        for line in errors {
            out.warn(line);
        }
        out.exit_code = code;
        return Ok(());
    } else if a.pos.len() > 1 || a.get("platform").is_some() {
        return Err(crate::Error::invalid(
            "Unexpected platform or positional argument",
        ));
    }
    let mut o = crate::install::Options {
        project: a.flag("project"),
        strict: a.flag("strict"),
        ..Default::default()
    };
    let action = if generic {
        command
    } else {
        a.require(0, "<platform> install|uninstall [--project]")?
    };
    if !["install", "uninstall"].contains(&action) {
        return Err("Expected install or uninstall".into());
    }
    let selected = if generic {
        a.get("platform").or(a.positional(0))
    } else {
        Some(command)
    };
    let messages = if action == "uninstall" {
        if let Some(name) = selected {
            crate::install::uninstall(name, &o)?
        } else {
            crate::install::uninstall_all(&o, a.flag("purge"))?
        }
    } else {
        let name = selected.unwrap_or(if cfg!(windows) { "windows" } else { "claude" });
        if !generic
            && !o.project
            && [
                "claude", "aider", "codex", "opencode", "claw", "droid", "trae", "trae-cn",
                "hermes",
            ]
            .contains(&name)
        {
            crate::install::configure(name, &o, false)?
        } else {
            if name == "codebuddy" && !generic {
                o.project = false;
            }
            let mut m = crate::install::install(name, &o)?;
            if !generic && ["codebuddy", "kilo"].contains(&name) {
                m.extend(crate::install::configure(name, &o, false)?);
            }
            m
        }
    };
    for m in messages {
        out.line(m);
    }
    Ok(())
}
pub fn hook(a: &Args, out: &mut Output) -> Result<()> {
    let event = a.positional(1).unwrap_or("post-commit");
    match a.positional(0).unwrap_or("") {
        "install" => out.line(crate::hooks::install(Path::new("."))?),
        "uninstall" => out.line(crate::hooks::uninstall(Path::new("."))?),
        "status" => out.line(crate::hooks::status(Path::new("."))?),
        "launch" => {
            let s = crate::hooks::launch(Path::new("."), event)?;
            if !s.is_empty() {
                out.line(s);
            }
        }
        "run" => {
            let r = crate::hooks::run_worker(event)?;
            for s in r.messages {
                out.line(s);
            }
            if !r.success {
                out.exit_code = 1;
            }
        }
        _ => {
            return Err(format!(
                "Usage: {} hook install|uninstall|status",
                crate::cli::program()
            )
            .into());
        }
    }
    Ok(())
}
pub fn update(a: &Args, out: &mut Output) -> Result<()> {
    if a.pos.len() > 1 {
        return Err("update accepts at most one path argument".into());
    }
    for key in a.flags.iter().chain(a.values.keys()) {
        if !["force", "no-cluster", "follow-symlinks", "precise"].contains(&key.as_str()) {
            return Err(format!("unknown update option: --{key}").into());
        }
    }
    let path = a.positional(0).map(expand).unwrap_or_else(|| {
        fs::read_to_string(Path::new(paths::OUT_DIR.as_str()).join(".graphicle_root"))
            .ok()
            .map(|s| PathBuf::from(s.trim_start_matches('\u{feff}').trim()))
            .unwrap_or(".".into())
    });
    if !path.exists() {
        return Err(format!("path not found: {}", path.display()).into());
    }
    out.line(format!(
        "Re-extracting code files in {} (no LLM needed)...",
        path.display()
    ));
    let r = crate::watch::rebuild(
        &path,
        &crate::watch::RebuildOptions {
            force: crate::cli::force(a),
            no_cluster: a.flag("no-cluster"),
            follow_symlinks: a.flag("follow-symlinks"),
            block_on_lock: true,
            ..Default::default()
        },
    )?;
    for m in r.messages {
        out.line(m);
    }
    if !r.success {
        return Err("Nothing to update or rebuild failed — check output above.".into());
    }
    out.line(format!(
        "Code graph updated. For doc/paper/image changes run {} extract.",
        crate::cli::program()
    ));
    Ok(())
}
static STOP: AtomicBool = AtomicBool::new(false);
/// Windows console control handler: Ctrl+C and Ctrl+Break stop `watch` and
/// `serve` as SIGINT and SIGTERM do on Unix; other events keep the default.
#[cfg(windows)]
extern "system" fn console_stop(event: u32) -> i32 {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        STOP.store(true, Ordering::Relaxed);
        1
    } else {
        0
    }
}
fn stop_signals() {
    STOP.store(false, Ordering::Relaxed);
    #[cfg(windows)]
    {
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| {
            let _ = crate::win::add_ctrl_handler(console_stop);
        });
    }
    #[cfg(unix)]
    {
        extern "C" fn stop(_: i32) {
            STOP.store(true, Ordering::Relaxed);
        }
        unsafe {
            nix::libc::signal(
                nix::libc::SIGINT,
                stop as *const () as nix::libc::sighandler_t,
            );
            nix::libc::signal(
                nix::libc::SIGTERM,
                stop as *const () as nix::libc::sighandler_t,
            );
        }
    }
}
pub fn watch(a: &Args, _out: &mut Output) -> Result<()> {
    let p = expand(a.positional(0).unwrap_or("."));
    if !p.exists() {
        return Err(format!("path not found: {}", p.display()).into());
    }
    if a.flag("notify-only") {
        crate::watch::notify_only(&p)?;
        return Ok(());
    }
    stop_signals();
    crate::watch::watch(&p, a.positive_float("debounce", 2.0)?, &STOP, |s| {
        eprintln!("{s}")
    })
}
pub fn serve(a: &Args, _out: &mut Output) -> Result<()> {
    // Opt-in: answer the CLI's read-only graph commands over a Unix socket
    // (see `serve_socket`) instead of serving MCP.
    if let Some(socket) = a.get("socket") {
        return crate::serve_socket::serve(&expand(socket));
    }
    let p = a.positional(0).map(expand).unwrap_or_else(|| a.graph());
    let transport = a.value("transport", "stdio");
    // Opt-in: keep the graph resident and apply file changes in memory, with
    // the normal incremental rebuild of .graphicle/ running in the background.
    let live = if a.flag("watch") {
        let graph = crate::extractors::resolution::resolved(&p);
        let root = a.get("root").map(expand).unwrap_or_else(|| {
            graph
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        });
        let live = crate::serve_live::LiveGraph::new(&graph, &root)?;
        // Over stdio, SIGINT/SIGTERM keep their default action (exit); the HTTP
        // transport installs its own stop handler below, which also ends this.
        let watcher = live.clone();
        std::thread::spawn(move || {
            if let Err(e) = watcher.run(&STOP) {
                eprintln!("serve --watch stopped: {e}");
            }
        });
        Some(live)
    } else {
        None
    };
    let server = |p: &Path| match &live {
        Some(live) => crate::serve::Server::with_live(live.clone()),
        None => crate::serve::Server::new(p),
    };
    if transport == "stdio" {
        return crate::serve::serve_stdio(&server(&p));
    }
    if !["http", "streamable-http"].contains(&transport) {
        return Err("--transport must be stdio or http".into());
    }
    let port = a.number("port", 8080)?;
    if !(0..=65535).contains(&port) {
        return Err("--port must be between 0 and 65535".into());
    }
    let options = crate::serve_http::HttpOptions {
        host: a.value("host", "127.0.0.1").into(),
        port: port as u16,
        api_key: a
            .get("api-key")
            .map(str::to_string)
            .or_else(|| std::env::var("GRAPHICLE_MCP_API_KEY").ok()),
        path: a.value("path", "/mcp").into(),
        json_response: a.flag("json-response"),
        stateless: a.flag("stateless"),
        session_timeout: Some(a.float("session-timeout", 3600.0)?),
    };
    let listener = std::net::TcpListener::bind((options.host.as_str(), options.port))
        .map_err(crate::Error::from)?;
    let mut options = options;
    options.port = listener.local_addr().map_err(crate::Error::from)?.port();
    eprintln!(
        "Graphicle MCP listening on http://{}:{}{}",
        options.host, options.port, options.path
    );
    let server = Arc::new(crate::serve_http::HttpServer::new(
        Arc::new(server(&p)),
        options,
    )?);
    stop_signals();
    crate::serve_http::run(listener, server, &STOP)
}
pub fn prs(args: &[String], out: &mut Output) -> Result<()> {
    let mut d = build::Diagnostics::default();
    let runtime = crate::llm::Runtime::from_environment(&mut d);
    let triage = crate::prs::NativeTriage {
        runtime,
        commands: Arc::new(crate::prs::NativeCommands::default()),
    };
    let r = crate::prs::cmd_prs(
        args,
        &crate::prs::NativeCommands::default(),
        &triage,
        &crate::prs::RenderContext::default(),
    )?;
    out.stdout.push_str(&r.stdout);
    out.stderr.push_str(&r.stderr);
    out.exit_code = r.code;
    out.diagnostics(d);
    Ok(())
}
pub fn provider(a: &Args, out: &mut Output) -> Result<()> {
    let p = crate::install::Options::default()
        .home
        .join(".graphicle-graph/providers.json");
    let mut cfg = crate::install_fs::json_like(&p);
    let sub = a.positional(0).unwrap_or("");
    match sub {
        "list" => {
            if cfg.as_object().unwrap().is_empty() {
                out.line("No custom providers registered.");
            } else {
                for (k, v) in cfg.as_object().unwrap() {
                    out.line(format!("  {k}  ({})", v["base_url"].as_str().unwrap_or("")));
                }
            }
        }
        "show" => {
            let name = a.require(1, "provider show <name>")?;
            let v = cfg
                .get(name)
                .ok_or_else(|| format!("Provider '{name}' not found."))?;
            out.value(&json!({name:v}));
        }
        "add" => {
            let name = a.require(
                1,
                "provider add <name> --base-url URL --default-model MODEL --env-key KEY",
            )?;
            let mut d = build::Diagnostics::default();
            let builtins = crate::llm_config::Configuration::builtins(Default::default());
            if builtins.backends.contains_key(name) {
                return Err(
                    format!("'{name}' is a built-in provider and cannot be overridden.").into(),
                );
            }
            let url = a.get("base-url").ok_or("--base-url is required")?;
            let model = a
                .get("default-model")
                .ok_or("--default-model is required")?;
            let key = a.get("env-key").ok_or("--env-key is required")?;
            if !crate::llm_config::provider_base_url_ok(url, name, false, &mut d) {
                return Err("refusing to add provider with unsafe base_url".into());
            }
            cfg[name] = json!({"base_url":url,"default_model":model,"env_key":key,"pricing":{"input":a.float("pricing-input",0.0)?,"output":a.float("pricing-output",0.0)?},"temperature":0});
            crate::install_fs::write(&p, &format!("{}\n", crate::install_fs::json_text(&cfg)))?;
            out.line(format!(
                "Provider '{name}' added. Use with: {} extract . --backend {name}",
                crate::cli::program()
            ));
        }
        "remove" => {
            let name = a.require(1, "provider remove <name>")?;
            if cfg.as_object_mut().unwrap().shift_remove(name).is_none() {
                return Err(format!("Provider '{name}' not found.").into());
            }
            crate::install_fs::write(&p, &format!("{}\n", crate::install_fs::json_text(&cfg)))?;
            out.line(format!("Provider '{name}' removed."));
        }
        _ => {
            return Err(format!(
                "Usage: {} provider list|show|add|remove",
                crate::cli::program()
            )
            .into());
        }
    }
    Ok(())
}
pub fn global(a: &Args, out: &mut Output) -> Result<()> {
    let mut store = crate::global_graph::GlobalStore::default_location()?;
    match a.positional(0).unwrap_or("") {
        "add" => {
            let p = expand(a.require(1, "global add <graph.json> [--as TAG]")?);
            let tags = build::distinct_repo_tags(&[crate::cli_state::resolve(&p)
                .to_string_lossy()
                .into_owned()]);
            let tag = a.get("as").unwrap_or(&tags[0]);
            let result = store.add(&p, tag)?;
            if result["skipped"] == true {
                out.line(format!(
                    "'{tag}' unchanged since last add - global graph not modified."
                ));
            } else {
                out.line(format!(
                    "Added '{tag}' to global graph: +{} nodes, -{} pruned. Global: {}",
                    result["nodes_added"],
                    result["nodes_removed"],
                    store.graph_path.display()
                ));
                if result["cross_repo_calls"].as_u64().unwrap_or(0) > 0 {
                    out.line(format!(
                        "  resolved {} member call(s) across repos",
                        result["cross_repo_calls"]
                    ));
                }
            }
        }
        "remove" => {
            let tag = a.require(1, "global remove <tag>")?;
            out.line(format!(
                "Removed '{tag}' from global graph ({} nodes pruned).",
                store.remove(tag)?
            ));
        }
        "list" => {
            let repos = store.list()?;
            if repos.as_object().is_none_or(|r| r.is_empty()) {
                out.line(format!(
                    "Global graph is empty. Use '{} global add' to add a project.",
                    crate::cli::program()
                ));
            } else {
                out.line(format!("Global graph: {}", store.graph_path.display()));
                for (tag, info) in repos.as_object().unwrap() {
                    let count = info
                        .get("node_count")
                        .map(search::text)
                        .unwrap_or("?".into());
                    let added = info["added_at"]
                        .as_str()
                        .unwrap_or("?")
                        .chars()
                        .take(10)
                        .collect::<String>();
                    out.line(format!("  {tag}: {count} nodes, added {added}"));
                }
            }
        }
        "path" => out.line(store.graph_path.display().to_string()),
        _ => {
            return Err(format!(
                "Usage: {} global add|remove|list|path",
                crate::cli::program()
            )
            .into());
        }
    }
    Ok(())
}
pub fn misc(command: &str, a: &Args, out: &mut Output) -> Result<()> {
    match command {
        "save-result" => {
            let q = a.get("question").ok_or("--question is required")?;
            let answer = if let Some(p) = a.get("answer-file") {
                fs::read_to_string(expand(p))
                    .map_err(crate::Error::from)?
                    .trim()
                    .to_string()
            } else {
                a.get("answer")
                    .ok_or("--answer or --answer-file is required")?
                    .into()
            };
            let options = crate::ingest::SaveOptions {
                query_type: a.value("type", "query").into(),
                source_nodes: a.all("nodes"),
                outcome: a.get("outcome").map(str::to_string),
                correction: a.get("correction").map(str::to_string),
            };
            let p = crate::ingest::save_query_result(
                q,
                &answer,
                &a.path(
                    "memory-dir",
                    Path::new(paths::OUT_DIR.as_str()).join("memory"),
                ),
                &options,
            )?;
            out.line(format!("Saved to {}", p.display()));
        }
        "reflect" => {
            let memory = a.path(
                "memory-dir",
                Path::new(paths::OUT_DIR.as_str()).join("memory"),
            );
            let output = a.path(
                "out",
                Path::new(paths::OUT_DIR.as_str()).join("reflections/LESSONS.md"),
            );
            let graph = a.graph();
            let graph = graph.exists().then_some(graph);
            let analysis = a.get("analysis").map(expand).or_else(|| {
                graph
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(|p| p.join(".graphicle_analysis.json"))
            });
            let labels = a.get("labels").map(expand).or_else(|| {
                graph
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(|p| p.join(".graphicle_labels.json"))
            });
            if a.flag("if-stale")
                && crate::reflect::lessons_fresh(
                    &output,
                    &memory,
                    graph.as_deref(),
                    analysis.as_deref(),
                    labels.as_deref(),
                )
            {
                out.line(format!(
                    "Lessons already up to date -> {} (skipped; omit --if-stale to force)",
                    output.display()
                ));
            } else {
                let (_, agg) = crate::reflect::reflect(
                    &memory,
                    &output,
                    crate::reflect::ReflectOptions {
                        graph: graph.as_deref(),
                        analysis: analysis.as_deref(),
                        labels: labels.as_deref(),
                        now: &crate::python_datetime::DateTime::now(),
                        half_life: a.float("half-life-days", 30.0)?,
                        min_corroboration: a.number("min-corroboration", 2)?,
                    },
                )?;
                out.line(format!(
                    "Reflected {} memories ({} useful, {} dead ends, {} corrected) -> {}",
                    agg["total"],
                    agg["counts"]["useful"],
                    agg["counts"]["dead_end"],
                    agg["counts"]["corrected"],
                    output.display()
                ));
            }
        }
        "add" => {
            let mut d = build::Diagnostics::default();
            let path = crate::ingest::Runtime::default().ingest(
                a.require(0, "add <url>")?,
                &a.path("dir", "raw"),
                &crate::ingest::CaptureOptions {
                    author: a.get("author").map(str::to_string),
                    contributor: a.get("contributor").map(str::to_string),
                },
                &mut d,
            )?;
            out.diagnostics(d);
            out.line(format!("Saved to {}", path.display()));
            out.line(format!(
                "Run {} extract to update the graph.",
                crate::cli::program()
            ));
        }
        "tree" => {
            let gp = a.graph();
            let target = a.path(
                "output",
                gp.parent()
                    .unwrap_or(Path::new("."))
                    .join("GRAPH_TREE.html"),
            );
            let result = crate::tree_html::write_tree_html(
                &gp,
                &target,
                a.get("root"),
                a.number("max-children", 200)?,
                a.get("label"),
            )?;
            out.line(format!(
                "wrote {result} ({:.1} KB)",
                target.metadata().map_err(crate::Error::from)?.len() as f64 / 1024.0
            ));
        }
        "benchmark" => {
            let graph = a.positional(0).map(expand).unwrap_or_else(|| a.graph());
            let words =
                crate::install_fs::json_like(Path::new(".graphicle_detect.json"))["total_words"]
                    .as_i64();
            let value = crate::benchmark::run_benchmark(Some(&graph), words, None, None)?;
            out.line(crate::benchmark::format_benchmark(&value, "utf-8"));
        }
        "diagnose" => {
            if a.positional(0) != Some("multigraph") {
                return Err(format!(
                    "Usage: {} diagnose multigraph [--graph PATH]",
                    crate::cli::program()
                )
                .into());
            }
            let mut d = build::Diagnostics::default();
            let summary = crate::diagnostics::diagnose_file(
                &a.graph(),
                a.direction()?,
                &crate::diagnostics::Options {
                    directed: true,
                    root: Some(crate::cli_state::resolve(Path::new("."))),
                    max_examples: a.number("max-examples", 5)?,
                    extract_path: a.get("extract-path").map(expand),
                },
                None,
                &mut d,
            )?;
            if a.flag("json") {
                out.value(&crate::diagnostics_report::format_json(&summary));
            } else {
                out.line(crate::diagnostics_report::format_report(&summary)?);
            }
            out.diagnostics(d);
        }
        "cache-check" => {
            let list = expand(a.require(0, "cache-check <files_from> [--root DIR]")?);
            let root = a.path("root", ".");
            let files = fs::read_to_string(&list)
                .map_err(crate::Error::from)?
                .lines()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            let mut cache = crate::cache::ExtractionCache::default();
            let result = cache.check_semantic(
                &files,
                &crate::cache::SemanticOptions {
                    root: root.clone(),
                    mode: a
                        .get("mode")
                        .map(str::to_string)
                        .or_else(|| a.flag("deep").then(|| "deep".into())),
                    prompt_file: a.get("prompt-file").map(expand),
                    ..Default::default()
                },
            )?;
            let dir = root.join(paths::OUT_DIR.as_str());
            fs::create_dir_all(&dir).map_err(crate::Error::from)?;
            if (0..3).any(|i| !ar(&result[i]).is_empty()) {
                paths::write_json_atomic(
                    &dir.join(".graphicle_cached.json"),
                    &json!({"nodes":result[0],"edges":result[1],"hyperedges":result[2]}),
                    None,
                    false,
                )
                .map_err(crate::Error::from)?;
            }
            crate::install_fs::write(
                &dir.join(".graphicle_uncached.txt"),
                &strings(&result[3]).join("\n"),
            )?;
            out.line(format!(
                "Cache: {} hit, {} miss",
                files.len() - ar(&result[3]).len(),
                ar(&result[3]).len()
            ));
            cache.flush();
        }
        "check-update" => {
            let p = expand(a.require(0, "check-update <path>")?);
            let mut messages = vec![];
            crate::watch::check_update(&p, &mut messages);
            for message in messages {
                out.line(message);
            }
        }
        "clone" => {
            let dest = clone_repo(
                a.require(0, "clone <github-url>")?,
                a.get("branch"),
                a.get("out").map(expand),
                out,
            )?;
            out.line(dest.display().to_string());
        }
        _ => return Err(format!("Unknown command: {command}").into()),
    }
    Ok(())
}
fn clone_repo(
    url: &str,
    branch: Option<&str>,
    output: Option<PathBuf>,
    out: &mut Output,
) -> Result<PathBuf> {
    let url = url.trim_end_matches('/');
    let git_url = if url.ends_with(".git") {
        url.into()
    } else {
        format!("{url}.git")
    };
    let clean = url.strip_suffix(".git").unwrap_or(url);
    let re = regex::Regex::new(r"github\.com[:/]([^/]+)/([^/]+)$").unwrap();
    let cap = re
        .captures(clean)
        .ok_or_else(|| format!("not a recognised GitHub URL: {clean}"))?;
    if branch.is_some_and(|s| s.starts_with('-')) {
        return Err("invalid branch name".into());
    }
    if [&cap[1], &cap[2]]
        .iter()
        .any(|s| *s == "." || *s == ".." || s.contains(['\\', '\0']))
    {
        return Err("invalid repository name".into());
    }
    let dest = output.unwrap_or_else(|| {
        crate::install::Options::default()
            .home
            .join(".graphicle-graph/repos")
            .join(&cap[1])
            .join(&cap[2])
    });
    let exists = dest.exists();
    let mut cmd = std::process::Command::new("git");
    if exists {
        cmd.arg("-C").arg(&dest).arg("pull");
        if let Some(branch) = branch {
            cmd.args(["origin", "--", branch]);
        }
    } else {
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p).map_err(crate::Error::from)?;
        }
        cmd.args(["clone", "--depth", "1"]);
        if let Some(branch) = branch {
            cmd.args(["--branch", branch]);
        }
        cmd.arg("--").arg(git_url).arg(&dest);
    }
    let r =
        crate::process::capture(&mut cmd, Duration::from_secs(600)).map_err(crate::Error::from)?;
    if !r.success {
        let e = String::from_utf8_lossy(&r.stderr);
        if !exists {
            return Err(format!("git clone failed:\n{e}").into());
        }
        out.warn(format!("git pull failed:\n{e}"));
    }
    out.line(format!("Ready at: {}", dest.display()));
    Ok(dest)
}

#[cfg(all(test, windows))]
mod windows_stop_tests {
    use super::{STOP, console_stop};
    use std::sync::atomic::Ordering;

    #[test]
    fn ctrl_c_and_ctrl_break_set_stop() {
        assert_eq!(console_stop(2), 0);
        assert!(!STOP.load(Ordering::Relaxed));
        assert_eq!(console_stop(0), 1);
        assert!(STOP.load(Ordering::Relaxed));
        STOP.store(false, Ordering::Relaxed);
        assert_eq!(console_stop(1), 1);
        assert!(STOP.load(Ordering::Relaxed));
        STOP.store(false, Ordering::Relaxed);
    }
}
