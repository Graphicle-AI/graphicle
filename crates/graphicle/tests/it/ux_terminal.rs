//! The easy flow's terminal commands (ux-terminal): `graphicle` alone,
//! the grouped help, `status`, `doctor [--fix]`, `ask` and `context` on a
//! terminal, `config`, `open`, `sync` without a path, `update` without a
//! feed and the common errors. Each test runs the program in a temporary
//! HOME with fake client configuration folders and a fixture Git
//! repository, with a cleared environment. Piped output keeps the JSON it
//! had; `GRAPHICLE_OUTPUT=text` stands in for a terminal.
use axum::{Json, Router, routing::post};
use graphicle_runtime::platform::CanonicalExt;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

const AUTH: &str = "def login(user):\n    \"\"\"Check the password and start a session.\"\"\"\n    return check(user) and start_session(user)\n\ndef check(user):\n    return user.password == \"x\"\n";
const APP: &str = "from auth import login\n\ndef main():\n    login(None)\n";
const README: &str = "# Demo\n\nThe app logs users in with `login`.\n";
/// The system folders only: `git` and `lsof`, and no Ollama or other tool
/// of the machine running the tests.
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// A temporary HOME (with `.cursor`, `.codex` and Claude Desktop folders),
/// a fixture Git repository `repo`, a folder outside it and a data folder.
struct Sandbox {
    _dir: TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonical().unwrap();
        for folder in [
            "home/.cursor",
            "home/.codex",
            "home/Library/Application Support/Claude",
            "home/.config/Claude",
            "repo/src",
            "elsewhere",
        ] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        // Claude Desktop's folder on Windows (`%APPDATA%\Claude`).
        #[cfg(windows)]
        std::fs::create_dir_all(root.join("home").join(r"AppData\Roaming\Claude")).unwrap();
        let repo = root.join("repo");
        std::fs::write(repo.join("src/auth.py"), AUTH).unwrap();
        std::fs::write(repo.join("src/app.py"), APP).unwrap();
        std::fs::write(repo.join("README.md"), README).unwrap();
        let sandbox = Self { _dir: dir, root };
        for args in [
            vec!["init", "-q", "."],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            let mut git = Command::new("git");
            git.args(&args).current_dir(&repo);
            crate::child_env::isolate(&mut git, &sandbox.home(), &[], PATH)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let status = crate::fork_guard::spawn(&mut git).unwrap().wait().unwrap();
            assert!(status.success(), "git {args:?}");
        }
        sandbox
    }
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }
    fn elsewhere(&self) -> PathBuf {
        self.root.join("elsewhere")
    }
    fn data(&self) -> PathBuf {
        self.root.join("data")
    }
    /// The program with this sandbox's environment, in `cwd`.
    fn command(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
        let home = self.home();
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        command
            .current_dir(cwd)
            .arg("--data-dir")
            .arg(self.data())
            .args(args);
        crate::child_env::isolate(&mut command, &home, &[], PATH)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("GRAPHICLE_NO_BROWSER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmp);
        }
        for (name, value) in env {
            command.env(name, value);
        }
        command
    }
    fn run(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        crate::fork_guard::spawn(&mut self.command(cwd, args, env))
            .unwrap()
            .wait_with_output()
            .unwrap()
    }
    /// The readable view, as on a terminal.
    fn text(&self, cwd: &Path, args: &[&str]) -> Output {
        self.run(cwd, args, &[("GRAPHICLE_OUTPUT", "text")])
    }
    fn sync(&self) -> Value {
        json_of(&self.run(&self.repo(), &["sync"], &[]))
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
    stdout(output)
}
fn json_of(output: &Output) -> Value {
    serde_json::from_str(&ok(output)).unwrap()
}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn graphicle_alone_shows_the_project_or_how_to_start_and_creates_nothing() {
    let sandbox = Sandbox::new();
    // Outside any repository, with no data folder: one line about
    // Graphicle, then where to start; nothing is created.
    let outside = ok(&sandbox.run(&sandbox.elsewhere(), &[], &[]));
    assert!(
        outside.contains("code graph and memory for your repositories and agents"),
        "{outside}"
    );
    assert!(
        outside.contains("cd your-repo && graphicle setup"),
        "{outside}"
    );
    assert!(!sandbox.data().exists());
    // In a repository that is not set up: setup, here.
    let repo = ok(&sandbox.run(&sandbox.repo(), &[], &[]));
    assert!(repo.contains("not set up yet"), "{repo}");
    assert!(repo.contains("  graphicle setup"), "{repo}");
    assert!(!sandbox.data().exists());
    // In a synced project: its short status and the next commands.
    sandbox.sync();
    let home = ok(&sandbox.run(&sandbox.repo(), &[], &[]));
    assert!(home.starts_with("repo"), "{home}");
    assert!(home.contains("3 files"), "{home}");
    assert!(home.contains("Code graph built"), "{home}");
    assert!(home.contains("graphicle setup"), "no agent yet: {home}");
    assert!(home.contains("graphicle ask"), "{home}");
    assert!(home.contains("graphicle open"), "{home}");
    // A subfolder is the same project.
    let sub = ok(&sandbox.run(&sandbox.repo().join("src"), &[], &[]));
    assert!(sub.starts_with("repo"), "{sub}");
}

#[test]
fn the_help_is_short_and_grouped_and_help_all_lists_every_command() {
    let sandbox = Sandbox::new();
    let help = ok(&sandbox.run(&sandbox.elsewhere(), &["--help"], &[]));
    assert!(
        help.starts_with("Code graph and memory for your repositories and agents\n"),
        "{help}"
    );
    let mut at = 0;
    for group in [
        "\nStart:\n",
        "\nAgents:\n",
        "\nCode graph:\n",
        "\nData:\n",
        "\nServer:\n",
        "\nMore:\n",
        "\nOptions:\n",
    ] {
        let found = help[at..]
            .find(group)
            .unwrap_or_else(|| panic!("{group:?} in order: {help}"));
        at += found + group.len();
    }
    let listed = |text: &str, name: &str| {
        text.lines()
            .any(|line| line.starts_with("  ") && line.split_whitespace().next() == Some(name))
    };
    for name in [
        "ask", "context", "open", "status", "doctor", "connect", "mcp", "graph", "sync",
        "sessions", "backup", "restore", "serve", "init", "login", "config", "update",
    ] {
        assert!(listed(&help, name), "{name}: {help}");
    }
    for name in [
        "run",
        "jobs",
        "worker",
        "redis-worker",
        "call",
        "work-state",
        "config-example",
        "hook",
        "build-graph",
        "document-worker",
        "help",
    ] {
        assert!(!listed(&help, name), "{name} is advanced: {help}");
    }
    assert!(help.contains("graphicle help --all"), "{help}");
    assert_eq!(ok(&sandbox.run(&sandbox.elsewhere(), &["help"], &[])), help);
    assert_eq!(ok(&sandbox.run(&sandbox.elsewhere(), &["-h"], &[])), help);
    let all = ok(&sandbox.run(&sandbox.elsewhere(), &["help", "--all"], &[]));
    assert!(
        all.contains("\nAdvanced:\n") && all.contains("\nInternal:\n"),
        "{all}"
    );
    for name in [
        "run",
        "jobs",
        "worker",
        "redis-worker",
        "call",
        "work-state",
        "config-example",
        "hook",
        "build-graph",
        "document-worker",
        "ask",
        "sync",
    ] {
        assert!(listed(&all, name), "{name}: {all}");
    }
    // One command's help, and `open` answers to `start` too.
    let sync = ok(&sandbox.run(&sandbox.elsewhere(), &["help", "sync"], &[]));
    assert!(
        sync.contains("Usage: graphicle sync [OPTIONS] [PATH]"),
        "{sync}"
    );
    // The folder's default is said once, and every option says what it does.
    assert_eq!(
        sync.matches("default: the project this folder is in")
            .count(),
        1,
        "{sync}"
    );
    assert!(!sync.contains("[default:"), "{sync}");
    assert!(
        sync.contains("--project <PROJECT>  The project to sync the folder into"),
        "{sync}"
    );
    for command in ["status", "context", "ask", "mcp", "doctor"] {
        let help = ok(&sandbox.run(&sandbox.elsewhere(), &["help", command], &[]));
        assert!(
            help.contains("--project <PROJECT>")
                && help.contains("The project: an id, a key or a synced folder"),
            "{command}: {help}"
        );
    }
    let start = ok(&sandbox.run(&sandbox.elsewhere(), &["start", "--help"], &[]));
    assert!(start.contains("Usage: graphicle open"), "{start}");
    // `init` points to setup.
    let init = ok(&sandbox.run(&sandbox.elsewhere(), &["help", "init"], &[]));
    assert!(init.contains("graphicle setup"), "{init}");
    // Another tool's word for a command names it; a typo gets clap's
    // suggestion; both exit 2 as before.
    let search = sandbox.run(&sandbox.elsewhere(), &["search", "login"], &[]);
    assert_eq!(search.status.code(), Some(2));
    assert!(
        stderr(&search).contains("graphicle context \"QUESTION\""),
        "{}",
        stderr(&search)
    );
    let typo = sandbox.run(&sandbox.elsewhere(), &["stauts"], &[]);
    assert_eq!(typo.status.code(), Some(2));
    assert!(stderr(&typo).contains("'status'"), "{}", stderr(&typo));
}

#[test]
fn status_keeps_its_json_when_piped_and_reads_well_on_a_terminal() {
    let sandbox = Sandbox::new();
    let synced = sandbox.sync();
    let id = synced["repository_id"].as_str().unwrap().to_string();
    // Piped, and with --json: the status operation's reply, unchanged.
    let piped = json_of(&sandbox.run(&sandbox.repo(), &["status"], &[]));
    let keys: Vec<&str> = piped
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "repository",
            "files",
            "evidence_sources",
            "memories",
            "stale_memories",
            "draft_memories",
            "source_state",
            "native_runtime",
            "structural_revision",
            "graph",
            "automatic_model_calls",
            "write_lock",
            "freshness"
        ]
    );
    assert_eq!(piped["repository"]["id"], id.as_str());
    let forced = json_of(&sandbox.run(
        &sandbox.repo(),
        &["status", "--json"],
        &[("GRAPHICLE_OUTPUT", "text")],
    ));
    assert_eq!(forced["repository"]["id"], id.as_str());
    // A Cursor entry for this project, as `connect` writes it.
    let exe = Path::new(env!("CARGO_BIN_EXE_graphicle"))
        .canonical()
        .unwrap();
    std::fs::write(
        sandbox.home().join(".cursor/mcp.json"),
        json!({"mcpServers":{"graphicle-repo":{"command":exe,"args":["--data-dir",sandbox.data(),"mcp","--project",id]}}})
            .to_string(),
    )
    .unwrap();
    let text = ok(&sandbox.text(&sandbox.repo(), &["status"]));
    for label in [
        "Project ",
        "Folder ",
        "Synced ",
        "Contents ",
        "Graph ",
        "Agents ",
        "Hooks ",
        "Model ",
        "Server ",
    ] {
        assert!(text.contains(label), "{label}: {text}");
    }
    // The code symbols, counted as setup counts them.
    assert!(text.contains("3 files, 3 symbols, 0 memories"), "{text}");
    assert!(text.contains("built: 7 nodes"), "{text}");
    assert!(
        text.contains("Cursor (~/.cursor/mcp.json, graphicle-repo): ok"),
        "{text}"
    );
    assert!(
        text.contains("Hooks     none (graphicle setup adds session capture and Git hooks)"),
        "{text}"
    );
    assert!(text.contains("none: ask shows the evidence only"), "{text}");
    // NO_COLOR, and no terminal: no escape codes.
    assert!(!text.contains('\x1b'), "{text}");
    // Outside the project: the last synced project, said so.
    let outside = ok(&sandbox.text(&sandbox.elsewhere(), &["status"]));
    assert!(outside.contains("the last synced project"), "{outside}");
}

#[test]
fn doctor_finds_a_client_entry_whose_program_is_gone_and_fix_rewrites_it() {
    let sandbox = Sandbox::new();
    let exe = Path::new(env!("CARGO_BIN_EXE_graphicle"))
        .canonical()
        .unwrap();
    // Claude Code's project entry, valid (written before the sync, so the
    // repair's re-sync finds nothing changed).
    std::fs::write(
        sandbox.repo().join(".mcp.json"),
        json!({"mcpServers":{"graphicle":{"command":exe,"args":["--data-dir",sandbox.data(),"mcp","--project","."]}}})
            .to_string(),
    )
    .unwrap();
    let synced = sandbox.sync();
    let id = synced["repository_id"].as_str().unwrap().to_string();
    let cursor = sandbox.home().join(".cursor/mcp.json");
    // An upgrade moved the program: the entry names a path that is gone.
    // Another server's entry must survive the repair.
    std::fs::write(
        &cursor,
        json!({"mcpServers":{
            "graphicle-repo":{"command":"/nonexistent/Cellar/graphicle/0.0.9/bin/graphicle","args":["--data-dir",sandbox.data(),"mcp","--project",id]},
            "other":{"command":"npx","args":["-y","other-server"]}}})
        .to_string(),
    )
    .unwrap();
    let report = sandbox.run(&sandbox.repo(), &["doctor"], &[]);
    assert_eq!(report.status.code(), Some(1), "{}", stderr(&report));
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["ok"], false, "{report}");
    let checks = report["checks"].as_array().unwrap();
    let find = |id: &str| -> Vec<&Value> { checks.iter().filter(|c| c["id"] == id).collect() };
    for id in [
        "program", "engine", "data", "project", "graph", "model", "port", "disk",
    ] {
        assert_eq!(find(id).len(), 1, "{id}: {report}");
    }
    assert_eq!(find("project")[0]["state"], "ok", "{report}");
    assert_eq!(find("graph")[0]["state"], "ok", "{report}");
    assert_eq!(find("model")[0]["state"], "warn", "{report}");
    assert_eq!(
        find("model")[0]["fix"],
        "graphicle config set model anthropic"
    );
    let clients = find("client");
    assert_eq!(clients.len(), 2, "{report}");
    let broken = clients
        .iter()
        .find(|c| c["state"] == "fail")
        .expect("the moved program fails");
    assert!(
        broken["detail"]
            .as_str()
            .unwrap()
            .contains("graphicle-repo"),
        "{broken}"
    );
    assert_eq!(broken["fix"], "graphicle doctor --fix");
    assert!(
        clients
            .iter()
            .any(|c| c["state"] == "ok" && c["detail"].as_str().unwrap().contains("Claude Code"))
    );
    // The readable report names the fix.
    let text = sandbox.text(&sandbox.repo(), &["doctor"]);
    assert_eq!(text.status.code(), Some(1));
    let text = stdout(&text);
    assert!(text.contains("✗ Agent"), "{text}");
    assert!(text.contains("fix: graphicle doctor --fix"), "{text}");
    assert!(text.contains("1 problem"), "{text}");
    // --fix: re-sync, then the entry runs this program; the other server's
    // entry is untouched.
    let fixed = sandbox.run(&sandbox.repo(), &["doctor", "--fix"], &[]);
    let fixed_report: Value = serde_json::from_slice(&fixed.stdout).unwrap();
    assert!(fixed.status.success(), "{fixed_report}");
    let fixes = fixed_report["fixed"].as_array().unwrap();
    assert!(
        fixes
            .iter()
            .any(|f| f["fix"] == "sync" && f["changed"] == false),
        "{fixed_report}"
    );
    assert!(
        fixes.iter().any(|f| f["fix"] == "client"
            && f["entry"] == "graphicle-repo"
            && f.get("error").is_none()),
        "{fixed_report}"
    );
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
    assert_eq!(
        document["mcpServers"]["graphicle-repo"]["command"],
        json!(exe)
    );
    assert_eq!(
        document["mcpServers"]["graphicle-repo"]["args"][4],
        id.as_str()
    );
    assert_eq!(document["mcpServers"]["other"]["command"], "npx");
    // Without a data folder: reported, not created.
    let fresh = Sandbox::new();
    let report = fresh.run(&fresh.elsewhere(), &["doctor"], &[]);
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "data" && c["state"] == "info"),
        "{report}"
    );
    assert!(!fresh.data().exists());
}

#[test]
fn ask_without_a_model_prints_the_evidence_and_the_command_that_adds_one() {
    let sandbox = Sandbox::new();
    sandbox.sync();
    let piped = json_of(&sandbox.run(&sandbox.repo(), &["ask", "how does login work"], &[]));
    assert_eq!(piped["answered"], false, "{piped}");
    assert_eq!(piped["reason"], "no_answer_model");
    assert_eq!(piped["hint"], "graphicle config set model anthropic");
    assert!(
        piped["context"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == "file:src/auth.py"),
        "{piped}"
    );
    let text = ok(&sandbox.text(&sandbox.repo(), &["ask", "how does login work"]));
    assert!(
        text.starts_with("Evidence for \"how does login work\""),
        "{text}"
    );
    assert!(text.contains("src/auth.py:1-6"), "{text}");
    assert!(text.contains("No answer model is configured"), "{text}");
    assert!(
        text.contains("graphicle config set model anthropic"),
        "{text}"
    );
    // A key in the environment is named, never shown.
    let found = ok(&sandbox.run(
        &sandbox.repo(),
        &["ask", "how does login work"],
        &[
            ("GRAPHICLE_OUTPUT", "text"),
            ("OPENAI_API_KEY", "sk-test-not-a-key"),
        ],
    ));
    assert!(
        found.contains("openai found: OPENAI_API_KEY is set"),
        "{found}"
    );
    assert!(
        found.contains("graphicle config set model openai"),
        "{found}"
    );
    assert!(!found.contains("sk-test-not-a-key"), "{found}");
    // `context` reads the same on a terminal.
    let context = ok(&sandbox.text(&sandbox.repo(), &["context", "how does login work"]));
    assert!(context.contains("[1] "), "{context}");
    assert!(context.contains("login()"), "{context}");
    let packet = json_of(&sandbox.run(&sandbox.repo(), &["context", "how does login work"], &[]));
    assert_eq!(packet["question"], "how does login work");
}

/// `context` builds 0.1.2's packet unless asked: no excerpt dates and no
/// notes-first order. `--notes-first` dates each excerpt from Git, in the
/// packet and on a terminal, and reports the order it used. The packet holds
/// 8,400 characters at most, or 4 per token of `--max-tokens` when only that
/// is given, or `--max-chars`.
#[test]
fn context_dates_excerpts_with_notes_first_and_sizes_itself_by_its_flags() {
    let sandbox = Sandbox::new();
    sandbox.sync();
    let question = "how does login work";
    let context = |extra: &[&str]| {
        let mut args = vec!["context", question];
        args.extend_from_slice(extra);
        json_of(&sandbox.run(&sandbox.repo(), &args, &[]))
    };
    let dated = |packet: &Value| {
        packet["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|source| source.get("changed").is_some())
            .count()
    };
    let plain = context(&[]);
    assert_eq!(
        plain["notes_first"],
        json!({"status":"off","defaulted":true}),
        "{plain:#}"
    );
    assert_eq!(plain["source_age"]["status"], "disabled");
    assert_eq!(dated(&plain), 0, "{}", plain["sources"]);
    assert!(!plain["markdown"].as_str().unwrap().contains("(changed "));
    assert_eq!(plain["max_chars"], 8400);
    let first = context(&["--notes-first"]);
    assert_eq!(first["notes_first"]["defaulted"], false, "{first:#}");
    assert_eq!(first["notes_first"]["status"], "reranked", "{first:#}");
    assert_eq!(
        first["source_age"]["status"], "dated",
        "{}",
        first["source_age"]
    );
    assert!(dated(&first) >= 1, "{}", first["sources"]);
    assert!(
        first["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|source| source.get("changed").is_some())
            .all(|source| source["changed_basis"] == "git"),
        "{}",
        first["sources"]
    );
    assert!(first["markdown"].as_str().unwrap().contains(" (changed "));
    let text = ok(&sandbox.text(&sandbox.repo(), &["context", question, "--notes-first"]));
    assert!(text.contains("(changed "), "{text}");
    assert_eq!(context(&["--max-tokens", "1000"])["max_chars"], 4000);
    assert_eq!(
        context(&["--max-tokens", "1000", "--max-chars", "3000"])["max_chars"],
        3000
    );
    assert_eq!(context(&["--max-chars", "5000"])["max_chars"], 5000);
}

/// A stand-in Responses provider: a `basic` reader quotes its first two
/// sources.
async fn respond(Json(request): Json<Value>) -> Json<Value> {
    let instructions = request["instructions"].as_str().unwrap_or_default();
    let text = if instructions.starts_with("Answer only from these original excerpts") {
        let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        let evidence: Vec<Value> = input["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["id"] == "file:src/auth.py")
            .map(|s| json!({"id":s["id"],"quote":"return check(user) and start_session(user)"}))
            .collect();
        json!({"answer":"login() checks the password with check(), then starts a session.","abstain":false,
            "evidence":evidence,"unresolved":[]})
        .to_string()
    } else {
        "not json".to_string()
    };
    Json(
        json!({"status":"completed","model":"fixture","usage":{"input_tokens":100,"output_tokens":20},"output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_answers_with_numbered_sources_and_explains_model_errors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/responses", post(respond)),
        )
        .await
        .unwrap();
    });
    let sandbox = std::sync::Arc::new(Sandbox::new());
    let run = |args: Vec<String>, env: Vec<(String, String)>| {
        let sandbox = sandbox.clone();
        tokio::task::spawn_blocking(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let env: Vec<(&str, &str)> =
                env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            sandbox.run(&sandbox.repo(), &args, &env)
        })
    };
    let words = |line: &str| line.split('|').map(str::to_string).collect::<Vec<_>>();
    let text = || vec![("GRAPHICLE_OUTPUT".to_string(), "text".to_string())];
    ok(&run(words("sync"), vec![]).await.unwrap());
    let models = sandbox.root.join("models.json");
    let write_models = |profile: Value| {
        std::fs::write(
            &models,
            json!({"default_model":"fixture","models":{"fixture":profile}}).to_string(),
        )
        .unwrap();
    };
    write_models(
        json!({"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fixture"}),
    );
    let config = models.to_str().unwrap().to_string();
    // On a terminal: the answer, its numbered sources at their lines, and
    // what it cost.
    let answer = ok(&run(
        words(&format!("--config|{config}|ask|how does login work")),
        text(),
    )
    .await
    .unwrap());
    assert!(
        answer.starts_with("login() checks the password with check(), then starts a session. [1]"),
        "{answer}"
    );
    assert!(
        answer.contains("Sources\n  [1] src/auth.py:3\n"),
        "{answer}"
    );
    assert!(
        answer.contains("return check(user) and start_session(user)"),
        "{answer}"
    );
    assert!(
        answer.contains("basic · 1 model call · 100 in, 20 out tokens"),
        "{answer}"
    );
    // Piped: the job's JSON, as before.
    let job = json_of(
        &run(
            words(&format!("--config|{config}|ask|what does check do")),
            vec![],
        )
        .await
        .unwrap(),
    );
    assert_eq!(job["state"], "complete", "{job}");
    assert_eq!(job["kind"], "basic");
    // The provider cannot be reached: what failed and what to do.
    let closed = free_port();
    write_models(
        json!({"protocol":"responses","base_url":format!("http://127.0.0.1:{closed}/v1"),"model":"fixture"}),
    );
    let failed = run(
        words(&format!("--config|{config}|ask|is anything there")),
        text(),
    )
    .await
    .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        stderr(&failed).contains(&format!("Could not reach http://127.0.0.1:{closed}/v1")),
        "{}",
        stderr(&failed)
    );
    let piped = run(
        words(&format!("--config|{config}|ask|is anything there now")),
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(piped.status.code(), Some(1));
    let failed_job: Value = serde_json::from_slice(&piped.stdout).unwrap();
    assert_eq!(failed_job["state"], "failed", "{failed_job}");
    assert!(
        stderr(&piped).contains("Workflow did not complete: Could not reach"),
        "{}",
        stderr(&piped)
    );
    // The key's variable is not set: say which one, and how.
    write_models(
        json!({"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fixture","key_env":"GRAPHICLE_TEST_UNSET_KEY"}),
    );
    let keyless = run(words(&format!("--config|{config}|ask|who logs in")), text())
        .await
        .unwrap();
    assert_eq!(keyless.status.code(), Some(1));
    assert!(
        stderr(&keyless).contains("export GRAPHICLE_TEST_UNSET_KEY=..."),
        "{}",
        stderr(&keyless)
    );
    let piped_keyless = run(words(&format!("--config|{config}|ask|who logs in")), vec![])
        .await
        .unwrap();
    let failed: Value = serde_json::from_slice(&piped_keyless.stdout).unwrap();
    assert_eq!(failed["state"], "failed", "{failed}");
    // The same question once the key is set: a new attempt, not the
    // earlier failure.
    let keyed = || {
        vec![(
            "GRAPHICLE_TEST_UNSET_KEY".to_string(),
            "fixture-key".to_string(),
        )]
    };
    let retried = json_of(
        &run(
            words(&format!("--config|{config}|ask|who logs in")),
            keyed(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(retried["state"], "complete", "{retried}");
    assert_ne!(retried["id"], failed["id"], "{retried}");
    // A completed answer is still reused.
    let again = json_of(
        &run(
            words(&format!("--config|{config}|ask|who logs in")),
            keyed(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(again["id"], retried["id"], "{again}");
    server.abort();
}

#[test]
fn config_sets_the_answer_model_in_one_command_and_never_writes_the_key() {
    let sandbox = Sandbox::new();
    let cwd = sandbox.elsewhere();
    // Reading the configuration creates nothing.
    let path = ok(&sandbox.run(&cwd, &["config", "path"], &[]));
    assert_eq!(
        path.trim_end(),
        sandbox.data().join("models.json").to_str().unwrap()
    );
    let shown = json_of(&sandbox.run(&cwd, &["config", "show"], &[]));
    assert_eq!(shown["exists"], false, "{shown}");
    assert_eq!(shown["answer_model"], Value::Null);
    assert!(!sandbox.data().exists());
    // One command: the provider's profile, the key's variable by name.
    let secret = "sk-ant-test-value-never-written";
    let set = json_of(&sandbox.run(
        &cwd,
        &["config", "set", "model", "anthropic"],
        &[("ANTHROPIC_API_KEY", secret)],
    ));
    assert_eq!(set["default_model"], "anthropic", "{set}");
    assert_eq!(set["key_set"], true);
    let file = sandbox.data().join("models.json");
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(!written.contains(secret), "{written}");
    let document: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(document["default_model"], "anthropic");
    let profile = &document["models"]["anthropic"];
    assert_eq!(profile["protocol"], "anthropic");
    assert_eq!(profile["base_url"], "https://api.anthropic.com/v1");
    assert_eq!(profile["key_env"], "ANTHROPIC_API_KEY");
    assert_eq!(profile["structured_output"], false);
    assert_eq!(
        ok(&sandbox.run(&cwd, &["config", "get", "model"], &[])).trim(),
        "anthropic"
    );
    assert_eq!(
        ok(&sandbox.run(&cwd, &["config", "get", "models.anthropic.key_env"], &[])).trim(),
        "ANTHROPIC_API_KEY"
    );
    let missing = sandbox.run(&cwd, &["config", "get", "embedding_model"], &[]);
    assert_eq!(missing.status.code(), Some(1));
    // Another provider; the first profile stays, and the file's first
    // version is kept as a backup.
    let text = ok(&sandbox.text(&cwd, &["config", "set", "model", "ollama:llama3.2"]));
    assert!(
        text.contains("Answer model: ollama · llama3.2 (ollama)"),
        "{text}"
    );
    let document: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(document["default_model"], "ollama");
    assert_eq!(
        document["models"]["ollama"]["base_url"],
        "http://127.0.0.1:11434"
    );
    assert_eq!(document["models"]["anthropic"]["protocol"], "anthropic");
    let backup = sandbox.data().join("models.json.graphicle-bak");
    let first: Value = serde_json::from_str(&std::fs::read_to_string(&backup).unwrap()).unwrap();
    assert_eq!(first["default_model"], "anthropic");
    // Any other key, checked as Graphicle reads it; a bad value changes
    // nothing.
    ok(&sandbox.run(
        &cwd,
        &["config", "set", "models.ollama.input_limit", "32000"],
        &[],
    ));
    let before = std::fs::read(&file).unwrap();
    let refused = sandbox.run(&cwd, &["config", "set", "worker_concurrency", "65"], &[]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("would be invalid"),
        "{}",
        stderr(&refused)
    );
    let unknown = sandbox.run(&cwd, &["config", "set", "model", "gemini"], &[]);
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(std::fs::read(&file).unwrap(), before);
    // The example, under both names.
    let example = ok(&sandbox.run(&cwd, &["config", "example"], &[]));
    assert_eq!(ok(&sandbox.run(&cwd, &["config-example"], &[])), example);
    assert_eq!(
        serde_json::from_str::<Value>(&example).unwrap()["default_model"],
        "local"
    );
    // No answer model again.
    let none = json_of(&sandbox.run(&cwd, &["config", "set", "model", "none"], &[]));
    assert_eq!(none["default_model"], Value::Null);
    let shown = json_of(&sandbox.run(&cwd, &["config", "show"], &[]));
    assert_eq!(shown["answer_model"], Value::Null, "{shown}");
}

#[test]
fn update_without_a_feed_says_how_to_update_and_sync_defaults_to_this_folder() {
    let sandbox = Sandbox::new();
    let update = json_of(&sandbox.run(&sandbox.elsewhere(), &["update"], &[]));
    assert_eq!(update["network"], false, "{update}");
    assert!(
        update["update"].as_array().is_some_and(|u| !u.is_empty()),
        "{update}"
    );
    assert_eq!(update["then"], "graphicle doctor --fix");
    assert!(!sandbox.data().exists());
    let text = ok(&sandbox.text(&sandbox.elsewhere(), &["update"]));
    assert!(text.contains("To update:"), "{text}");
    let half = sandbox.run(
        &sandbox.elsewhere(),
        &["update", "--feed", "https://example.invalid/feed.json"],
        &[],
    );
    assert_eq!(half.status.code(), Some(2), "a feed needs its key");
    // `sync` with no path: this folder.
    let synced = sandbox.sync();
    assert_eq!(synced["files"], 3, "{synced}");
    assert_eq!(synced["graph"]["status"], "built");
    let again = ok(&sandbox.text(&sandbox.repo(), &["sync"]));
    assert!(
        again.contains("no change since the last sync (3 files)"),
        "{again}"
    );
    assert!(again.contains("Code graph: 7 nodes"), "{again}");
}

#[test]
fn open_reuses_a_running_server_and_prints_the_address_without_a_browser() {
    let sandbox = Sandbox::new();
    sandbox.sync();
    let port = free_port();
    let mut serve = crate::fork_guard::spawn(&mut sandbox.command(
        &sandbox.repo(),
        &["serve", "--port", &port.to_string()],
        &[],
    ))
    .unwrap();
    let mut lines = BufReader::new(serve.stderr.take().unwrap()).lines();
    let ready = lines
        .by_ref()
        .map_while(Result::ok)
        .find(|line| line.contains("Graphicle is ready at"));
    assert!(ready.is_some(), "the server started");
    let opened = sandbox.run(&sandbox.repo(), &["open", "--port", &port.to_string()], &[]);
    let _ = serve.kill();
    let _ = serve.wait();
    let address = ok(&opened);
    assert!(
        address.contains(&format!("http://127.0.0.1:{port}/#session=")),
        "{address}"
    );
    // Another program holds the port: who, and --port.
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = held.local_addr().unwrap().port();
    let refused = sandbox.run(&sandbox.repo(), &["open", "--port", &busy.to_string()], &[]);
    assert_eq!(refused.status.code(), Some(1));
    let message = stderr(&refused);
    assert!(
        message.contains(&format!("Port {busy} is in use")),
        "{message}"
    );
    assert!(message.contains("--port"), "{message}");
    drop(held);
}

#[test]
fn common_errors_name_the_next_command() {
    let sandbox = Sandbox::new();
    // Nothing set up yet.
    let fresh = sandbox.run(&sandbox.repo(), &["status"], &[]);
    assert_eq!(fresh.status.code(), Some(1));
    assert!(
        stderr(&fresh).contains("graphicle setup"),
        "{}",
        stderr(&fresh)
    );
    // A workspace, but no project here.
    ok(&sandbox.run(&sandbox.elsewhere(), &["init"], &[]));
    let unknown = sandbox.run(&sandbox.elsewhere(), &["context", "anything"], &[]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown).contains("not a Graphicle project yet"),
        "{}",
        stderr(&unknown)
    );
    assert!(
        stderr(&unknown).contains("graphicle setup"),
        "{}",
        stderr(&unknown)
    );
    // Over the limits: which one, and how to leave paths out.
    let big = sandbox.root.join("big");
    std::fs::create_dir_all(&big).unwrap();
    for n in 0..5001 {
        std::fs::write(big.join(format!("f{n}.txt")), "x").unwrap();
    }
    let over = sandbox.run(&big, &["sync"], &[("GRAPHICLE_MAX_PROJECT_FILES", "5000")]);
    assert_eq!(over.status.code(), Some(1));
    let message = stderr(&over);
    assert!(
        message.contains("5,001 text files (the limit is 5,000)"),
        "{message}"
    );
    assert!(message.contains("GRAPHICLE_MAX_PROJECT_FILES"), "{message}");
    assert!(message.contains(".gitignore"), "{message}");
    assert!(
        message.contains("graphicle setup path/to/subfolder"),
        "{message}"
    );
}
