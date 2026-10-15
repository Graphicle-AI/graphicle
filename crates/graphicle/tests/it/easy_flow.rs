//! The whole easy flow, as a person runs it,
//! with the program itself: a temporary HOME holding fake Claude Code,
//! Codex, Cursor, VS Code and Claude Desktop folders (each client file with
//! one unrelated server) and a committed fixture Git repository. No client
//! program and no model are available, and nothing reaches the network.
//!
//! `setup --yes`, `status`, `graphicle` alone, `context`, `ask` without a
//! model (the evidence and the command that adds one, exit 0), an edit then
//! `ask` (the changed file synced first), `connect` again (nothing written),
//! `doctor` (no problem), `disconnect --all` (everything Graphicle added is
//! gone and nothing else changed, in bytes and modes) and `uninstall`
//! without `--purge` (the data stays).
use graphicle_runtime::platform::CanonicalExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// An unrelated MCP server, in every client file.
fn other() -> Value {
    json!({"command":"/usr/local/bin/other-server","args":["--stdio"]})
}

struct Flow {
    _dir: TempDir,
    home: PathBuf,
    data: PathBuf,
    repo: PathBuf,
    bin: PathBuf,
}

impl Flow {
    /// The machine: the clients' folders under a new HOME, and the
    /// committed repository `code/shop`.
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonical().unwrap();
        let flow = Self {
            home: root.join("home"),
            data: root.join("data"),
            repo: root.join("home").join("code").join("shop"),
            bin: root.join("bin"),
            _dir: dir,
        };
        std::fs::create_dir_all(&flow.bin).unwrap();
        // `graphicle` on PATH is this build, as an installed program is; the
        // engine beside it when the environment names one.
        link(
            Path::new(env!("CARGO_BIN_EXE_graphicle")),
            &flow.bin.join(crate::child_env::program("graphicle")),
        );
        if let Some(engine) = std::env::var_os("GRAPHICLE_GRAPH_BIN") {
            link(
                Path::new(&engine),
                &flow.bin.join(crate::child_env::program("graphicle-graph")),
            );
        }
        // The clients: Claude Code and VS Code by their folders, the others
        // with a settings file holding another server.
        std::fs::create_dir_all(flow.home.join(".claude")).unwrap();
        std::fs::create_dir_all(flow.home.join(".vscode")).unwrap();
        write(
            &flow.home.join(".cursor/mcp.json"),
            &serde_json::to_string_pretty(&json!({"mcpServers":{"other":other()}})).unwrap(),
        );
        write(
            &flow.desktop().join("claude_desktop_config.json"),
            &json!({"mcpServers":{"other":other()},"globalShortcut":""}).to_string(),
        );
        write(
            &flow.home.join(".codex/config.toml"),
            "model = \"o4-mini\"\n\n[mcp_servers.other]\ncommand = \"/usr/local/bin/other-server\"\nargs = [\"--stdio\"]\n",
        );
        // The repository: a little shop, its own CLAUDE.md and post-commit.
        write(
            &flow.repo.join("shop/pricing.py"),
            "\"\"\"Prices and tax.\"\"\"\nTAX_RATE = 0.2\n\n\ndef price_with_tax(price):\n    \"\"\"A price with the sales tax added.\"\"\"\n    return round(price * (1 + TAX_RATE), 2)\n",
        );
        write(
            &flow.repo.join("shop/cart.py"),
            "from shop.pricing import price_with_tax\n\n\nclass Cart:\n    def __init__(self):\n        self.items = []\n\n    def total(self):\n        \"\"\"The total price, tax included.\"\"\"\n        return sum(price_with_tax(price) for price in self.items)\n",
        );
        write(
            &flow.repo.join("README.md"),
            "# Shop\n\nA cart, prices with tax, and checkout.\n",
        );
        write(
            &flow.repo.join("CLAUDE.md"),
            "# Shop\n\nRun the tests with make test.\n",
        );
        flow.git(&["init", "-q"]);
        flow.git(&["add", "."]);
        flow.git(&["commit", "-q", "-m", "first"]);
        let hook = flow.repo.join(".git/hooks/post-commit");
        write(&hook, "#!/bin/sh\necho mine\n");
        set_mode(&hook, 0o755);
        flow
    }

    /// Claude Desktop's folder on this platform.
    fn desktop(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/Claude")
        } else if cfg!(windows) {
            // `%APPDATA%\Claude`; child_env::home points APPDATA here.
            self.home.join("AppData").join("Roaming").join("Claude")
        } else {
            self.home.join(".config/Claude")
        }
    }

    /// Every child's environment: this machine's HOME and XDG folders, no
    /// client program and no model key, no system or user Git settings.
    fn environment(&self, command: &mut Command) {
        crate::child_env::isolate(command, &self.home, &[&self.bin], "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("TMPDIR", std::env::temp_dir());
    }

    fn git(&self, args: &[&str]) {
        let mut command = Command::new("git");
        self.environment(&mut command);
        command.current_dir(&self.repo).args(args);
        let output = output(command);
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    /// `graphicle --data-dir <data> <args>` in the repository; `text` asks
    /// for the readable output (GRAPHICLE_OUTPUT=text), else it is piped.
    fn run(&self, args: &[&str], text: bool) -> Output {
        let mut command = Command::new(self.bin.join("graphicle"));
        self.environment(&mut command);
        if text {
            command.env("GRAPHICLE_OUTPUT", "text");
        }
        command
            .current_dir(&self.repo)
            .arg("--data-dir")
            .arg(&self.data)
            .args(args);
        output(command)
    }

    /// [`Flow::run`] piped, which must succeed: its JSON.
    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args, false);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "{args:?} printed no JSON: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// [`Flow::run`] for people, which must succeed: its text.
    fn text(&self, args: &[&str]) -> String {
        let output = self.run(args, true);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        String::from_utf8(output.stdout).unwrap()
    }

    /// Every file of the repository (Git's objects aside) and of the
    /// clients' folders, with its bytes and mode.
    fn files(&self) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
        let mut files = tree(&self.repo);
        for folder in [
            self.home.join(".claude"),
            self.home.join(".codex"),
            self.home.join(".cursor"),
            self.home.join(".vscode"),
            self.desktop(),
        ] {
            files.extend(tree(&folder));
        }
        files
    }
}

#[cfg(unix)]
fn link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}
#[cfg(not(unix))]
fn link(target: &Path, link: &Path) {
    std::fs::copy(target, link).unwrap();
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}
#[cfg(not(unix))]
fn mode(_: &Path) -> u32 {
    0
}

/// Every file under `root` (Git's object store and logs left out).
fn tree(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    let mut files = BTreeMap::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.ends_with(".git/objects") || path.ends_with(".git/logs") {
                continue;
            }
            if path.is_dir() {
                folders.push(path);
            } else {
                files.insert(path.clone(), (std::fs::read(&path).unwrap(), mode(&path)));
            }
        }
    }
    files
}

fn output(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The `state` of each item of `report[list]`, by `key`.
fn states(report: &Value, list: &str, key: &str) -> Vec<(String, String)> {
    report[list]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item[key].as_str().unwrap_or_default().to_string(),
                item["state"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// The names a group of the short help lists.
fn group(help: &str, title: &str) -> Vec<String> {
    help.split(&format!("\n{title}:\n"))
        .nth(1)
        .unwrap_or_default()
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect()
}

/// The paths of a context packet's sources.
fn paths(packet: &Value) -> Vec<String> {
    packet["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|source| source["path"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn the_easy_flow_from_setup_to_uninstall() {
    let flow = Flow::new();
    let mut before = flow.files();

    // The help names the flow's commands in their groups.
    let help = String::from_utf8(flow.run(&["--help"], false).stdout).unwrap();
    for name in ["setup", "open", "doctor"] {
        assert!(group(&help, "Start").contains(&name.to_string()), "{help}");
    }
    for name in ["connect", "disconnect"] {
        assert!(group(&help, "Agents").contains(&name.to_string()), "{help}");
    }
    for name in ["config", "uninstall"] {
        assert!(group(&help, "More").contains(&name.to_string()), "{help}");
    }

    // 1. setup --yes, without a terminal: the project with its code graph,
    // every client, capture, instructions, Git hooks, the freshness check
    // and the model step, and every file it changed.
    let output = flow.run(&["setup", "--yes"], false);
    assert!(output.status.success(), "setup: {}", stderr(&output));
    let setup: Value = serde_json::from_slice(&output.stdout).unwrap();
    // Setup's own files change nothing a sync reads (the section appended
    // to CLAUDE.md, the new AGENTS.md and MCP files): no second sync, no
    // lock diagnostics, and the folder shown with `~`.
    let progress = stderr(&output);
    assert!(
        !progress.contains("syncing the files setup wrote"),
        "{progress}"
    );
    assert!(!progress.contains("write lock"), "{progress}");
    assert!(progress.contains("graphicle: syncing ~/"), "{progress}");
    let project = &setup["project"];
    assert_eq!(project["folder"], json!(flow.repo), "{setup:#}");
    assert_eq!(project["graph"]["status"], "built", "{setup:#}");
    assert!(project["symbols"].as_u64().unwrap_or(0) >= 3, "{setup:#}");
    let added = |client: &str| (client.to_string(), "added".to_string());
    assert_eq!(
        states(&setup, "clients", "client"),
        [
            added("claude-code"),
            added("codex"),
            added("cursor"),
            added("vscode"),
            added("claude")
        ],
        "{setup:#}"
    );
    assert_eq!(setup["capture"]["state"], "on", "{setup:#}");
    assert_eq!(
        states(&setup, "instructions", "file"),
        [
            (
                flow.repo.join("AGENTS.md").display().to_string(),
                "added".to_string()
            ),
            (
                flow.repo.join("CLAUDE.md").display().to_string(),
                "added".to_string()
            )
        ],
        "{setup:#}"
    );
    assert_eq!(setup["git_hooks"]["state"], "added", "{setup:#}");
    // The files setup wrote in the folder were synced before the summary:
    // nothing is left for the first answer to catch up on.
    assert_eq!(setup["freshness"]["state"], "fresh", "{setup:#}");
    assert_eq!(setup["freshness"]["mode"], "auto", "{setup:#}");
    // No key in this environment and no Ollama: the command that adds one.
    assert_eq!(setup["model"]["state"], "none", "{setup:#}");
    assert_eq!(
        setup["model"]["command"],
        "graphicle config set model anthropic"
    );
    let changed = setup["files_changed"].as_array().unwrap().len();
    // The clients' files, the instructions, the Git hooks, and the line of
    // .git/info/exclude that keeps the backups out of Git.
    assert_eq!(changed, 13, "{setup:#}");
    assert!(
        setup["files_changed"]
            .to_string()
            .contains(".git/info/exclude"),
        "{setup:#}"
    );
    let after_setup = flow.files();

    // 2. status: the JSON keeps its fields (nothing to report about the
    // folder); the readable view shows the freshness line, the agents and
    // the hooks.
    let status = flow.json(&["status"]);
    assert!(status.get("folder_freshness").is_none(), "{status:#}");
    assert!(status["files"].as_u64().unwrap() >= 4, "{status:#}");
    let shown = flow.text(&["status"]);
    for expected in [
        "Changes   none since the last sync",
        "Claude Code (.mcp.json): ok",
        "Codex (",
        "capture: claude-code, codex",
        "git: post-commit, post-merge, post-checkout",
        "Model     none",
    ] {
        assert!(shown.contains(expected), "{expected:?} in {shown}");
    }
    // `graphicle` alone, in the project: a short status and what to try.
    let home = flow.text(&[]);
    assert!(home.contains("up to date"), "{home}");
    assert!(home.contains("graphicle ask"), "{home}");

    // 3. context: the evidence, with nothing synced first.
    let context = flow.json(&["context", "where is tax added to prices"]);
    assert!(
        paths(&context).contains(&"shop/pricing.py".to_string()),
        "{context:#}"
    );
    assert!(context.get("folder_freshness").is_none(), "{context:#}");
    // Setup said "up to date", and the evidence agrees.
    assert_eq!(context["freshness"]["stale_count"], 0, "{context:#}");
    assert!(
        !flow
            .text(&["context", "where is tax added to prices"])
            .contains("may be out of date")
    );

    // 4. ask without a model: the evidence and the command that adds one,
    // exit 0, piped and on a terminal.
    let asked = flow.json(&["ask", "where is tax added to prices"]);
    assert_eq!(asked["answered"], false, "{asked:#}");
    assert_eq!(asked["reason"], "no_answer_model");
    assert_eq!(asked["hint"], "graphicle config set model anthropic");
    assert!(paths(&asked["context"]).contains(&"shop/pricing.py".to_string()));
    let readable = flow.text(&["ask", "where is tax added to prices"]);
    assert!(readable.starts_with("Evidence for"), "{readable}");
    assert!(readable.contains("shop/pricing.py"), "{readable}");
    assert!(
        readable.contains("No answer model is configured")
            && readable.contains("graphicle config set model anthropic"),
        "{readable}"
    );

    // 5. An edit, then ask: the changed file is synced before the answer.
    let pricing = flow.repo.join("shop/pricing.py");
    let mut text = std::fs::read_to_string(&pricing).unwrap();
    text.push_str("\n\ndef discount(price, percent):\n    \"\"\"A price after a percentage discount.\"\"\"\n    return round(price * (1 - percent / 100), 2)\n");
    std::fs::write(&pricing, &text).unwrap();
    before.insert(pricing.clone(), (text.into_bytes(), mode(&pricing)));
    let fresh = flow.json(&["ask", "how is a discount computed"]);
    let report = &fresh["context"]["folder_freshness"];
    assert_eq!(report["status"], "refreshed", "{fresh:#}");
    assert_eq!(report["changed"], 1, "{fresh:#}");
    assert_eq!(report["sample"], json!(["shop/pricing.py"]), "{fresh:#}");
    assert!(
        fresh["context"]["evidence"]
            .to_string()
            .contains("def discount"),
        "{fresh:#}"
    );
    // Once synced, the next answer has nothing to report.
    let again = flow.json(&["context", "how is a discount computed"]);
    assert!(again.get("folder_freshness").is_none(), "{again:#}");
    // On a terminal the note is part of the evidence.
    std::fs::write(
        flow.repo.join("README.md"),
        "# Shop\n\nA cart, prices with tax, discounts and checkout.\n",
    )
    .unwrap();
    before.insert(
        flow.repo.join("README.md"),
        (
            b"# Shop\n\nA cart, prices with tax, discounts and checkout.\n".to_vec(),
            mode(&flow.repo.join("README.md")),
        ),
    );
    let noted = flow.text(&["context", "what does the shop do"]);
    assert!(
        noted.contains("synced 1 changed file before answering"),
        "{noted}"
    );

    // 6. connect again, and setup again: everything is already there and
    // nothing is written.
    let edited = flow.files();
    let connected = flow.json(&["connect"]);
    let replies = connected["connected"].as_array().unwrap();
    assert_eq!(replies.len(), 5, "{connected:#}");
    assert!(
        replies.iter().all(|reply| reply["unchanged"] == true),
        "{connected:#}"
    );
    let all = flow.json(&["connect", "--all"]);
    assert!(
        all["connected"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reply| reply["unchanged"] == true),
        "{all:#}"
    );
    let again = flow.json(&["setup", "--yes"]);
    assert_eq!(again["files_changed"], json!([]), "{again:#}");
    assert_eq!(flow.files(), edited, "connect and setup wrote nothing");
    for (path, entry) in &after_setup {
        if !path.ends_with("shop/pricing.py") && !path.ends_with("README.md") {
            assert_eq!(edited.get(path), Some(entry), "{}", path.display());
        }
    }

    // 7. doctor: no problem; the agents' entries, the capture and Git hooks,
    // the instructions and the folder's freshness all check out.
    let doctor = flow.json(&["doctor"]);
    assert_eq!(doctor["ok"], true, "{doctor:#}");
    assert_eq!(doctor["problems"], 0, "{doctor:#}");
    let checks = doctor["checks"].as_array().unwrap();
    let state = |id: &str| -> Vec<&str> {
        checks
            .iter()
            .filter(|check| check["id"] == id)
            .map(|check| check["state"].as_str().unwrap())
            .collect()
    };
    assert_eq!(state("client"), ["ok"; 5], "{doctor:#}");
    assert_eq!(state("capture"), ["ok"; 2], "{doctor:#}");
    for id in [
        "data",
        "project",
        "graph",
        "fresh",
        "instructions",
        "git_hooks",
    ] {
        assert_eq!(state(id), ["ok"], "{id}: {doctor:#}");
    }
    // Without a key or Ollama, the model is a warning with its command.
    assert_eq!(state("model"), ["warn"], "{doctor:#}");
    let readable = flow.text(&["doctor"]);
    assert!(readable.contains("fix: graphicle config set model anthropic"));
    // After an upgrade moved the program: doctor names the broken Codex
    // entry (written into config.toml, there being no Codex command line),
    // and --fix brings Graphicle's table back as it was.
    let codex = flow.home.join(".codex/config.toml");
    let written = std::fs::read_to_string(&codex).unwrap();
    // The program on PATH (the link an upgrade keeps), not the file it
    // leads to.
    let program = flow
        .bin
        .join(crate::child_env::program("graphicle"))
        .display()
        .to_string();
    // As a TOML basic string spells it (a Windows path's `\` doubled).
    let program = program.replace('\\', "\\\\");
    assert!(written.contains(&format!("\"{program}\"")), "{written}");
    std::fs::write(&codex, written.replace(&program, "/gone/graphicle")).unwrap();
    let broken = flow.run(&["doctor"], false);
    assert!(!broken.status.success(), "a broken entry fails the check");
    let broken: Value = serde_json::from_slice(&broken.stdout).unwrap();
    let codex_check = broken["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "client" && check["state"] == "fail")
        .cloned()
        .unwrap_or_else(|| panic!("{broken:#}"));
    assert!(
        codex_check["title"] == "Agent" && codex_check["detail"].to_string().contains("Codex"),
        "{codex_check:#}"
    );
    assert_eq!(
        codex_check["fix"], "graphicle doctor --fix",
        "{codex_check:#}"
    );
    let fixed = flow.json(&["doctor", "--fix"]);
    assert_eq!(fixed["ok"], true, "{fixed:#}");
    assert!(
        fixed["fixed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fix| fix["client"] == "codex" && fix.get("error").is_none()),
        "{fixed:#}"
    );
    assert_eq!(std::fs::read_to_string(&codex).unwrap(), written);

    // 8. disconnect --all: everything Graphicle added is gone, and every
    // other file is as it was, in bytes and modes (the two edits aside,
    // which are the user's).
    let undone = flow.json(&["disconnect", "--all"]);
    assert_eq!(undone["kept_backups"], json!([]), "{undone:#}");
    let after = flow.files();
    let differences: Vec<&PathBuf> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(differences.is_empty(), "{differences:#?}\n{undone:#}");
    assert!(!flow.data.join("agent-changes.json").exists());
    let shown = flow.text(&["status"]);
    assert!(shown.contains("none connected"), "{shown}");

    // 9. uninstall without --purge: nothing is left to undo, it says how to
    // remove the program, and the data stays.
    let uninstall = flow.json(&["uninstall"]);
    assert_eq!(
        uninstall["disconnected"]["files"],
        json!([]),
        "{uninstall:#}"
    );
    assert!(
        uninstall["remove_program"]["command"].is_string(),
        "{uninstall:#}"
    );
    assert!(uninstall.get("purged").is_none(), "{uninstall:#}");
    assert!(flow.data.join("reviews.sqlite3").is_file());
    let readable = flow.text(&["uninstall"]);
    assert!(
        readable.contains("graphicle uninstall --purge deletes it"),
        "{readable}"
    );
    assert_eq!(flow.files(), after, "uninstall changed nothing more");
}
