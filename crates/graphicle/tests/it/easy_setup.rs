//! `graphicle setup`, `connect` without a client, `disconnect` and
//! `uninstall`, run as the program with a temporary
//! HOME holding fake Claude Code, Codex, Cursor, VS Code and Claude Desktop
//! folders, each with one unrelated server entry, and a fixture Git
//! repository. No client program is on `PATH`, so Codex's entry is written
//! into its `config.toml` directly.
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

struct Machine {
    _dir: TempDir,
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
    repo: PathBuf,
    bin: PathBuf,
}

impl Machine {
    /// A home, a data folder and a committed repository `work/app`.
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonical().unwrap();
        let machine = Self {
            home: root.join("home"),
            data: root.join("data"),
            repo: root.join("work").join("app"),
            bin: root.join("bin"),
            root,
            _dir: dir,
        };
        for folder in [&machine.home, &machine.repo, &machine.bin] {
            std::fs::create_dir_all(folder).unwrap();
        }
        write(
            &machine.repo.join("src/billing.py"),
            "from money import cents\n\n\ndef settle_invoice(total, tax):\n    \"\"\"Settle an invoice with its tax.\"\"\"\n    return cents(total + tax)\n",
        );
        write(
            &machine.repo.join("src/money.py"),
            "def cents(value):\n    return round(value * 100) / 100\n",
        );
        write(
            &machine.repo.join("README.md"),
            "# App\n\nBilling settles invoices monthly.\n",
        );
        machine.git(&machine.repo, &["init", "-q"]);
        machine.git(&machine.repo, &["add", "."]);
        machine.git(&machine.repo, &["commit", "-q", "-m", "first"]);
        machine
    }

    /// The environment every child gets: this machine's home, no client
    /// program on `PATH`, no system or user Git configuration.
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

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        self.environment(&mut command);
        command.current_dir(cwd).args(args);
        let output = output(command);
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `graphicle --data-dir <data> <args>` in `cwd`.
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        self.environment(&mut command);
        command
            .current_dir(cwd)
            .arg("--data-dir")
            .arg(&self.data)
            .args(args);
        output(command)
    }

    /// [`Machine::run`] in the repository, which must succeed; its JSON.
    fn graphicle(&self, args: &[&str]) -> Value {
        self.graphicle_in(&self.repo, args)
    }

    fn graphicle_in(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "{args:?} printed no JSON: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
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

    /// The clients' own folders in the home.
    fn client_folders(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(".claude"),
            self.home.join(".codex"),
            self.home.join(".cursor"),
            self.home.join(".vscode"),
            self.desktop(),
        ]
    }

    /// Every client installed, each file with one unrelated server, a
    /// CLAUDE.md and a post-commit hook of the repository's own.
    fn install_clients(&self) {
        std::fs::create_dir_all(self.home.join(".claude")).unwrap();
        std::fs::create_dir_all(self.home.join(".vscode")).unwrap();
        write(
            &self.repo.join(".mcp.json"),
            &json!({"mcpServers":{"other":other()}}).to_string(),
        );
        write(
            &self.repo.join(".claude/settings.local.json"),
            "{\n  \"permissions\": {\"allow\": [\"Bash(ls)\"]},\n  \"hooks\": {\"Stop\": [{\"hooks\": [{\"type\": \"command\", \"command\": \"echo other\"}]}]}\n}\n",
        );
        write(
            &self.home.join(".codex/config.toml"),
            "model = \"o4-mini\"\n\n# My other server.\n[mcp_servers.other]\ncommand = \"/usr/local/bin/other-server\"\nargs = [\"--stdio\"]\n",
        );
        write(
            &self.home.join(".codex/hooks.json"),
            &json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo codex-other"}]}]}})
                .to_string(),
        );
        write(
            &self.home.join(".cursor/mcp.json"),
            &serde_json::to_string_pretty(&json!({"mcpServers":{"other":other()}})).unwrap(),
        );
        write(
            &self.repo.join(".vscode/mcp.json"),
            &json!({"servers":{"other":{"type":"stdio","command":"/usr/local/bin/other-server"}}})
                .to_string(),
        );
        write(
            &self.desktop().join("claude_desktop_config.json"),
            &json!({"mcpServers":{"other":other()},"globalShortcut":""}).to_string(),
        );
        write(
            &self.repo.join("CLAUDE.md"),
            "# App\n\nRun the tests with make test.\n",
        );
        let hook = self.repo.join(".git/hooks/post-commit");
        write(&hook, "#!/bin/sh\necho mine\n");
        set_mode(&hook, 0o755);
    }

    /// Every file of the repository (Git's objects aside) and of the
    /// clients' folders, with its mode.
    fn files(&self) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
        let mut files = tree(&self.repo);
        for folder in self.client_folders() {
            files.extend(tree(&folder));
        }
        files
    }
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

/// Every file under `root` (Git's object store left out), with its bytes
/// and mode.
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

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

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

/// The owner's end-to-end check: setup with every client installed, twice
/// (the second run changes nothing), then `disconnect --all` leaves every
/// file of the repository and of the clients' folders as it was before.
#[test]
fn setup_connects_every_client_once_and_disconnect_restores_every_file() {
    let m = Machine::new();
    m.install_clients();
    let before = m.files();

    let first = m.graphicle(&["setup", "--yes"]);
    let project = &first["project"];
    assert_eq!(project["folder"], json!(m.repo), "{first:#}");
    assert_eq!(project["graph"]["status"], "built", "{first:#}");
    assert!(project["symbols"].as_u64().unwrap_or(0) >= 2, "{first:#}");
    assert_eq!(project["git"], true);
    let id = project["id"].as_str().unwrap().to_string();
    let added = |client: &str| (client.to_string(), "added".to_string());
    assert_eq!(
        states(&first, "clients", "client"),
        [
            added("claude-code"),
            added("codex"),
            added("cursor"),
            added("vscode"),
            added("claude")
        ],
        "{first:#}"
    );
    // Each client file keeps its own server beside Graphicle's.
    let claude_code = read_json(&m.repo.join(".mcp.json"));
    assert_eq!(claude_code["mcpServers"]["other"], other());
    // The one MCP server: Claude Code gets its five essential tools; the
    // other clients, the whole server, where the agent may propose memory
    // drafts for review.
    let args = &claude_code["mcpServers"]["graphicle"]["args"];
    assert_eq!(
        args.as_array().unwrap()[args.as_array().unwrap().len() - 5..],
        [
            json!("mcp"),
            json!("--project"),
            json!("."),
            json!("--tools"),
            json!("essentials")
        ],
        "{claude_code:#}"
    );
    assert_eq!(first["clients"][0]["tools"], "essentials", "{first:#}");
    let vscode = read_json(&m.repo.join(".vscode/mcp.json"));
    assert_eq!(vscode["servers"]["graphicle"]["type"], "stdio");
    assert!(vscode["servers"].get("other").is_some());
    assert_eq!(
        vscode["servers"]["graphicle"]["args"]
            .as_array()
            .unwrap()
            .last(),
        Some(&json!("--allow-learning")),
        "{vscode:#}"
    );
    for file in [
        m.home.join(".cursor/mcp.json"),
        m.desktop().join("claude_desktop_config.json"),
    ] {
        let document = read_json(&file);
        assert_eq!(document["mcpServers"]["other"], other(), "{document:#}");
        let entry = &document["mcpServers"]["graphicle-app"];
        assert!(entry["args"].to_string().contains(&id), "{document:#}");
    }
    let codex = std::fs::read_to_string(m.home.join(".codex/config.toml")).unwrap();
    let tables = graphicle_runtime::codex_config::tables(&codex);
    assert_eq!(
        tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["other", "graphicle-app"],
        "{codex}"
    );
    assert!(tables[1].args.as_ref().unwrap().contains(&id), "{codex}");
    assert!(codex.starts_with("model = \"o4-mini\"\n\n# My other server.\n"));
    // Capture hooks for Claude Code and Codex, beside the existing ones.
    let settings = read_json(&m.repo.join(".claude/settings.local.json"));
    assert_eq!(settings["permissions"]["allow"][0], "Bash(ls)");
    assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
    assert!(settings["hooks"].get("SessionStart").is_some());
    // No guard hooks unless asked for (--guards), and no prompt hook
    // (--prompt-context).
    assert!(
        settings["hooks"].get("PreToolUse").is_none(),
        "{settings:#}"
    );
    assert!(first["clients"][0].get("guard").is_none(), "{first:#}");
    assert!(
        settings["hooks"].get("UserPromptSubmit").is_none(),
        "{settings:#}"
    );
    assert!(first.get("prompt_context").is_none(), "{first:#}");
    let codex_hooks = read_json(&m.home.join(".codex/hooks.json"));
    assert_eq!(codex_hooks["hooks"]["Stop"].as_array().unwrap().len(), 2);
    assert_eq!(first["capture"]["state"], "on", "{first:#}");
    // Other agents than Claude Code are connected: AGENTS.md is created
    // (--yes); CLAUDE.md, which exists, gets the section after its own text.
    let section = graphicle_runtime::agent_instructions::section();
    assert_eq!(
        std::fs::read_to_string(m.repo.join("AGENTS.md")).unwrap(),
        format!("{section}\n")
    );
    assert_eq!(
        std::fs::read_to_string(m.repo.join("CLAUDE.md")).unwrap(),
        format!("# App\n\nRun the tests with make test.\n\n{section}\n")
    );
    // Freshness hooks: the repository's own post-commit keeps its line.
    let hooks = m.repo.join(".git/hooks");
    let post_commit = std::fs::read_to_string(hooks.join("post-commit")).unwrap();
    assert!(
        post_commit.starts_with(&format!(
            "#!/bin/sh\necho mine\n\n# graphicle-sync-start {}\n",
            m.repo.display()
        )),
        "{post_commit}"
    );
    assert!(post_commit.contains(" --quiet </dev/null >/dev/null 2>&1 &\n"));
    for name in ["post-merge", "post-checkout"] {
        let text = std::fs::read_to_string(hooks.join(name)).unwrap();
        assert!(
            text.starts_with("#!/bin/sh\n# graphicle-sync-start "),
            "{text}"
        );
        assert_eq!(mode(&hooks.join(name)), if cfg!(unix) { 0o755 } else { 0 });
    }
    // Every file changed is listed, and each one that existed was backed
    // up first.
    let changed: Vec<PathBuf> = first["files_changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| PathBuf::from(file.as_str().unwrap()))
        .collect();
    let mut expected = vec![
        m.repo.join(".mcp.json"),
        m.repo.join(".claude/settings.local.json"),
        m.home.join(".codex/config.toml"),
        m.home.join(".codex/hooks.json"),
        m.home.join(".cursor/mcp.json"),
        m.repo.join(".vscode/mcp.json"),
        m.desktop().join("claude_desktop_config.json"),
        m.repo.join("AGENTS.md"),
        m.repo.join("CLAUDE.md"),
        hooks.join("post-commit"),
        hooks.join("post-merge"),
        hooks.join("post-checkout"),
        m.repo.join(".git/info/exclude"),
    ];
    let mut listed = changed.clone();
    listed.sort();
    expected.sort();
    assert_eq!(listed, expected, "{first:#}");
    for (path, (bytes, _)) in &before {
        if changed.contains(path) {
            let backup = graphicle_runtime::agent_files::backup_path(path);
            assert_eq!(
                &std::fs::read(&backup).unwrap(),
                bytes,
                "{}",
                backup.display()
            );
        }
    }
    assert!(first["model"]["state"].is_string(), "{first:#}");

    // A second run changes nothing and says so.
    let after_first = m.files();
    let second = m.graphicle(&["setup", "--yes"]);
    assert!(
        states(&second, "clients", "client")
            .iter()
            .all(|(_, state)| state == "already set"),
        "{second:#}"
    );
    assert!(
        second["capture"]["clients"]
            .as_array()
            .unwrap()
            .iter()
            .all(|client| client["state"] == "already set")
    );
    assert!(
        states(&second, "instructions", "file")
            .iter()
            .all(|(_, state)| state == "already set"),
        "{second:#}"
    );
    assert_eq!(second["git_hooks"]["state"], "already set", "{second:#}");
    assert_eq!(second["files_changed"], json!([]), "{second:#}");
    assert_eq!(m.files(), after_first, "the second run wrote nothing");
    // From a subfolder, setup finds the same project.
    let from_src = m.graphicle_in(&m.repo.join("src"), &["setup", "--yes"]);
    assert_eq!(from_src["project"]["folder"], json!(m.repo), "{from_src:#}");
    assert_eq!(from_src["project"]["id"], json!(id));
    assert_eq!(from_src["files_changed"], json!([]), "{from_src:#}");
    assert_eq!(m.files(), after_first);

    // disconnect --all: everything Graphicle added is gone, nothing else
    // changed (bytes and modes), and no backup is left.
    let undone = m.graphicle(&["disconnect", "--all"]);
    assert_eq!(undone["kept_backups"], json!([]), "{undone:#}");
    let after = m.files();
    let differences: Vec<&PathBuf> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(differences.is_empty(), "{differences:#?}\n{undone:#}");
    assert!(!m.data.join("agent-changes.json").exists());
    // Nothing is left to remove.
    let again = m.graphicle(&["disconnect", "--all"]);
    assert_eq!(again["files"], json!([]), "{again:#}");
}

/// One stdio session of the entry setup wrote in `.mcp.json` (its own
/// command and arguments, started in the repository as Claude Code starts
/// it), answering `requests`: the replies by id.
fn session(m: &Machine, requests: &[Value]) -> impl Fn(u64) -> Value + use<> {
    let entry = read_json(&m.repo.join(".mcp.json"))["mcpServers"]["graphicle"].clone();
    let program = entry["command"].as_str().unwrap().to_string();
    let args: Vec<String> = entry["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect();
    let input = requests
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut command = Command::new(&program);
    m.environment(&mut command);
    command
        .current_dir(&m.repo)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    move |id: u64| {
        replies
            .iter()
            .find(|reply| reply["id"] == id)
            .unwrap_or_else(|| panic!("no reply {id}: {replies:#?}"))["result"]
            .clone()
    }
}

/// `initialize`, `tools/list`, then a graph tool and a recall tool on the
/// fixture repository's `cents()`.
fn graph_and_recall() -> Vec<Value> {
    vec![
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"easy-setup-test","version":"0"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query_graph","arguments":{"question":"settle_invoice cents","structured":true}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":"How does settle_invoice use cents to round the amount?","structured":true}}}),
    ]
}

/// The `stable_id` `query_graph` (reply 3) cites `cents()` by, checked
/// against the file, and the same identity in `recall_project` (reply 4).
fn cents_cited_once(reply: &dyn Fn(u64) -> Value) -> String {
    let graph = reply(3);
    assert_eq!(graph["isError"], false, "{graph:#}");
    assert!(
        graph["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cents()"),
        "{graph:#}"
    );
    let cents = graph["structuredContent"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["label"] == "cents()")
        .unwrap_or_else(|| panic!("{:#}", graph["structuredContent"]))
        .clone();
    assert_eq!(cents["checked"], true, "{cents:#}");
    assert_eq!(cents["source_id"], "file:src/money.py", "{cents:#}");
    let stable_id = cents["stable_id"].as_str().unwrap().to_string();
    let recall = reply(4);
    assert_eq!(recall["isError"], false, "{recall:#}");
    let from_recall = recall["structuredContent"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|window| window.get("code_graph"))
        .find(|fact| fact["label"] == "cents()")
        .unwrap_or_else(|| panic!("{:#}", recall["structuredContent"]["evidence"]))
        .clone();
    assert_eq!(
        from_recall["stable_id"],
        json!(stable_id),
        "{from_recall:#}"
    );
    stable_id
}

/// The one MCP server through the easy flow, end to end, with Claude Code
/// alone. `graphicle setup` gives it the five essential tools and one
/// sentence of instructions, no guard hooks and no AGENTS.md section (the
/// server's instructions are Claude Code's); one stdio session of the entry
/// lists exactly those tools and answers a graph tool (`query_graph`, citing
/// the function with checked bytes) and a recall tool (`recall_project`,
/// citing the same `stable_id`). `--tools all --guards` brings back 0.1.0's
/// setup for Claude Code, but for the session-start note (see the
/// CHANGELOG): the whole server, where a memory tool (`propose_memory`)
/// drafts a memory anchored to the function its quote lies in, the guard
/// hooks, which point a search in the repository to the graph tools, and
/// 0.1.0's AGENTS.md section. A later setup without them returns to the
/// defaults, removes the guard hooks and brings the section up to date.
/// `disconnect --all` then leaves every file as it was.
#[test]
fn setup_then_one_mcp_session_answers_graph_and_memory_tools() {
    let m = Machine::new();
    std::fs::create_dir_all(m.home.join(".claude")).unwrap();
    let before = m.files();

    let report = m.graphicle(&["setup", "--yes", "--clients", "claude-code"]);
    assert_eq!(report["project"]["graph"]["status"], "built", "{report:#}");
    let client = &report["clients"][0];
    assert_eq!(client["client"], "claude-code", "{report:#}");
    assert_eq!(client["state"], "added", "{report:#}");
    assert_eq!(client["tools"], "essentials", "{report:#}");
    assert!(client.get("guard").is_none(), "{report:#}");
    let access = client["access"].as_str().unwrap_or_default();
    assert!(
        access.starts_with("five read-only tools") && !access.contains("memory drafts"),
        "{report:#}"
    );
    // Claude Code alone: no AGENTS.md section.
    assert_eq!(report["instructions"][0]["state"], "skipped", "{report:#}");
    assert!(
        report["instructions"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("Claude Code is the only agent connected"),
        "{report:#}"
    );
    assert!(!m.repo.join("AGENTS.md").exists());
    let mcp_file = m.repo.join(".mcp.json");
    let settings = m.repo.join(".claude/settings.local.json");
    for file in [&mcp_file, &settings] {
        assert!(
            report["files_changed"]
                .as_array()
                .unwrap()
                .contains(&json!(file)),
            "{}: {report:#}",
            file.display()
        );
    }
    let local = read_json(&settings);
    assert!(local["hooks"].get("PreToolUse").is_none(), "{local:#}");
    let text = graphicle_runtime::setup::render_setup(&report);
    assert!(
        text.contains("one MCP server, five of its tools: query_graph, get_node, get_neighbors, recall_project, read_project_source"),
        "{text}"
    );
    assert!(!text.contains("Graph guard"), "{text}");

    let entry = read_json(&mcp_file)["mcpServers"]["graphicle"].clone();
    let args: Vec<&str> = entry["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect();
    assert!(
        args.ends_with(&["mcp", "--project", ".", "--tools", "essentials"]),
        "{args:?}"
    );
    let reply = session(&m, &graph_and_recall());
    let initialized = reply(1);
    assert_eq!(
        initialized["instructions"],
        graphicle_runtime::api::ESSENTIAL_INSTRUCTIONS,
        "{initialized:#}"
    );
    assert!(
        initialized["capabilities"].get("resources").is_some(),
        "{initialized:#}"
    );
    let mut listed: Vec<String> = reply(2)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    listed.sort();
    let mut essentials = graphicle_runtime::api::ESSENTIAL_TOOLS.map(String::from);
    essentials.sort();
    assert_eq!(listed, essentials);
    let stable_id = cents_cited_once(&reply);

    // --tools all --guards: 0.1.0's setup for Claude Code.
    let whole = m.graphicle(&[
        "setup",
        "--yes",
        "--clients",
        "claude-code",
        "--tools",
        "all",
        "--guards",
    ]);
    let client = &whole["clients"][0];
    assert_eq!(client["state"], "updated", "{whole:#}");
    assert_eq!(client["tools"], "all", "{whole:#}");
    assert_eq!(client["guard"]["state"], "added", "{whole:#}");
    // --prompt-context: Claude Code's prompt hook, one UserPromptSubmit
    // entry beside the other hooks; a setup without it leaves it as it is.
    let with_prompt = [
        "setup",
        "--yes",
        "--clients",
        "claude-code",
        "--tools",
        "all",
        "--guards",
        "--prompt-context",
    ];
    let prompted = m.graphicle(&with_prompt);
    assert_eq!(prompted["prompt_context"]["state"], "added", "{prompted:#}");
    let hooked = read_json(&m.repo.join(".claude/settings.local.json"));
    let groups = hooked["hooks"]["UserPromptSubmit"].clone();
    assert_eq!(groups.as_array().unwrap().len(), 1, "{hooked:#}");
    assert!(groups[0].get("matcher").is_none(), "{groups:#}");
    let handler = &groups[0]["hooks"][0];
    assert_eq!(handler["type"], "command", "{handler:#}");
    assert_eq!(
        handler["timeout"],
        graphicle_runtime::prompt_hook::HOOK_TIMEOUT
    );
    let command = handler["command"].as_str().unwrap();
    assert!(
        command.ends_with(" hook claude-code prompt-context") && command.contains(" --data-dir "),
        "{command}"
    );
    assert!(hooked["hooks"].get("SessionStart").is_some());
    assert!(hooked["hooks"].get("PreToolUse").is_some());
    let again = m.graphicle(&with_prompt);
    assert_eq!(again["prompt_context"]["state"], "already set", "{again:#}");
    let without = m.graphicle(&with_prompt[..7]);
    assert!(without.get("prompt_context").is_none(), "{without:#}");
    assert_eq!(
        read_json(&m.repo.join(".claude/settings.local.json"))["hooks"]["UserPromptSubmit"],
        groups
    );
    // 0.1.0's AGENTS.md section, with Claude Code alone too.
    let agents = std::fs::read_to_string(m.repo.join("AGENTS.md")).unwrap();
    assert!(
        agents.contains("This repository is indexed by Graphicle")
            && agents.contains("`session_timeline`"),
        "{agents}"
    );
    assert!(
        client["access"].as_str().unwrap().contains("memory drafts"),
        "{whole:#}"
    );
    let args = read_json(&mcp_file)["mcpServers"]["graphicle"]["args"].clone();
    assert_eq!(
        args.as_array().unwrap()[args.as_array().unwrap().len() - 4..],
        [
            json!("mcp"),
            json!("--project"),
            json!("."),
            json!("--allow-learning")
        ],
        "{args:#}"
    );
    // A plain `connect claude-code` (no --tools) keeps that entry as it is.
    let kept = m.graphicle(&["connect", "claude-code"]);
    assert_eq!(kept["unchanged"], true, "{kept:#}");
    assert_eq!(kept["tools"], "all", "{kept:#}");
    assert_eq!(
        read_json(&mcp_file)["mcpServers"]["graphicle"]["args"],
        args
    );
    let mut requests = graph_and_recall();
    requests.push(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"propose_memory","arguments":{"kind":"decision","title":"Amounts are rounded to cents","body":"cents() rounds an amount to whole cents before an invoice is settled.","evidence":[{"source_id":"file:src/money.py","quote":"return round(value * 100) / 100"}],"idempotency_key":"rounding-1"}}}));
    requests.push(json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"project_memory","arguments":{}}}));
    let reply = session(&m, &requests);
    // One server: the graph tools under Graphify's names, recall and memory.
    let listed: Vec<String> = reply(2)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    for name in [
        "query_graph",
        "get_node",
        "get_neighbors",
        "shortest_path",
        "recall_project",
        "read_project_source",
        "project_memory",
        "search_project_memory",
        "propose_memory",
        "graph_history_stats",
    ] {
        assert!(listed.contains(&name.to_string()), "{name}: {listed:?}");
    }
    assert_eq!(cents_cited_once(&reply), stable_id);
    // A memory tool: a draft for review, anchored to the function its quote
    // lies in, listed with that symbol.
    let proposed = reply(5);
    assert_eq!(proposed["isError"], false, "{proposed:#}");
    assert_eq!(
        proposed["structuredContent"]["state"], "draft",
        "{proposed:#}"
    );
    let memory_id = proposed["structuredContent"]["memory_id"]
        .as_str()
        .unwrap()
        .to_string();
    let memories = reply(6);
    assert_eq!(memories["isError"], false, "{memories:#}");
    let listed = memories["content"][0]["text"].as_str().unwrap();
    assert!(
        listed.contains(&memory_id) && listed.contains(&stable_id),
        "{memory_id} anchored to {stable_id}: {listed}"
    );

    // The guard hooks: a search in the repository is pointed to the tools.
    let hook = |kind: &str, payload: Value| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        m.environment(&mut command);
        command
            .current_dir(&m.repo)
            .arg("--data-dir")
            .arg(&m.data)
            .args(["hook", "claude-code", kind])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::fork_guard::spawn(&mut command).unwrap();
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.to_string().as_bytes())
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let nudge = hook(
        "guard-search",
        json!({"cwd":m.repo,"tool_name":"Grep","tool_input":{"pattern":"settle_invoice"}}),
    );
    assert!(
        nudge.contains("query_graph") && nudge.contains("recall_project"),
        "{nudge}"
    );
    let guarded = read_json(&settings)["hooks"]["PreToolUse"].to_string();
    assert!(
        guarded.contains(" hook claude-code guard-search"),
        "{guarded}"
    );

    // Setup again without them: the essentials, and the guard hooks go
    // (the capture hooks stay).
    let back = m.graphicle(&["setup", "--yes", "--clients", "claude-code"]);
    let client = &back["clients"][0];
    assert_eq!(client["state"], "updated", "{back:#}");
    assert_eq!(client["tools"], "essentials", "{back:#}");
    assert_eq!(client["guard"]["state"], "removed", "{back:#}");
    let agents = std::fs::read_to_string(m.repo.join("AGENTS.md")).unwrap();
    assert!(
        !agents.contains("This repository is indexed by Graphicle")
            && agents
                .to_lowercase()
                .contains(graphicle_runtime::agent_instructions::ROUTING_GREP),
        "{agents}"
    );
    assert!(
        back["files_changed"]
            .as_array()
            .unwrap()
            .contains(&json!(settings)),
        "{back:#}"
    );
    let local = read_json(&settings);
    assert!(local["hooks"].get("PreToolUse").is_none(), "{local:#}");
    assert!(local["hooks"].get("Stop").is_some(), "{local:#}");
    let text = graphicle_runtime::setup::render_setup(&back);
    assert!(
        text.contains("graphicle setup --guards turns them on"),
        "{text}"
    );
    let again = m.graphicle(&["setup", "--yes", "--clients", "claude-code"]);
    assert_eq!(again["files_changed"], json!([]), "{again:#}");

    // disconnect --all: every file as it was (Git's index aside, which the
    // freshness checks' `git status` may refresh), no backup left.
    let undone = m.graphicle(&["disconnect", "--all"]);
    assert_eq!(undone["kept_backups"], json!([]), "{undone:#}");
    let after = m.files();
    let differences: Vec<&PathBuf> = before
        .keys()
        .chain(after.keys())
        .filter(|path| !path.ends_with(".git/index"))
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(differences.is_empty(), "{differences:#?}\n{undone:#}");
}

/// The switch from Graphify replaces its MCP entry and its CLAUDE.md
/// section, keeps `graphify-out/` and the skill, and `disconnect` puts both
/// back exactly. Without a terminal or --yes, AGENTS.md is not created.
#[test]
fn setup_switches_from_graphify_and_disconnect_puts_it_back() {
    let m = Machine::new();
    write(&m.repo.join("graphify-out/graph.json"), "{}");
    write(
        &m.repo.join(".mcp.json"),
        &json!({"mcpServers":{"graphify":{"command":"graphify","args":["--mcp"]},"other":other()}})
            .to_string(),
    );
    // The team's own rules follow Graphify's section under a level-one
    // heading: they are not Graphify's.
    let claude_md = "# App\n\n## graphify\n\nThis project has a graphify knowledge graph at graphify-out/.\n\n```sh\n# rebuild it\ngraphify .\n```\n\n# Team rules\n\nNever push to main.\n";
    write(&m.repo.join("CLAUDE.md"), claude_md);
    // Graphify's PreToolUse hook, as `graphify claude install` writes it.
    write(
        &m.repo.join(".claude/settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Glob|Grep","hooks":[{"type":"command","command":"[ -f graphify-out/graph.json ] && echo graphify || true"}]}]}}"#,
    );
    write(
        &m.home.join(".claude/skills/graphify/SKILL.md"),
        "# graphify\n",
    );
    let before = m.files();

    let report = m.graphicle(&[
        "setup",
        "--switch-from-graphify",
        "--clients",
        "claude-code",
        "--no-hooks",
        "--no-capture",
    ]);
    assert_eq!(report["graphify"]["switched"], true, "{report:#}");
    let kept = [
        m.repo.join("graphify-out"),
        m.home.join(".claude/skills/graphify"),
    ];
    if cfg!(windows) {
        // Windows reads `\` and `/` alike, and the product joins
        // `skills/graphify` onto the `.claude` folder: compare as paths.
        let reported = report["graphify"]["kept"].as_array().unwrap();
        assert_eq!(reported.len(), kept.len(), "{report:#}");
        for (reported, kept) in reported.iter().zip(&kept) {
            assert_eq!(
                Path::new(reported.as_str().unwrap()),
                kept.as_path(),
                "{report:#}"
            );
        }
    } else {
        assert_eq!(report["graphify"]["kept"], json!(kept));
    }
    let servers = read_json(&m.repo.join(".mcp.json"))["mcpServers"].clone();
    assert!(servers.get("graphify").is_none(), "{servers:#}");
    assert!(servers.get("graphicle").is_some() && servers.get("other").is_some());
    let section = graphicle_runtime::agent_instructions::section();
    assert_eq!(
        std::fs::read_to_string(m.repo.join("CLAUDE.md")).unwrap(),
        format!("# App\n\n{section}\n\n# Team rules\n\nNever push to main.\n")
    );
    // The lines it replaced are named, and the hook setup does not edit.
    let claude = report["instructions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|written| written["file"] == json!(m.repo.join("CLAUDE.md")))
        .cloned()
        .unwrap();
    assert_eq!(claude["lines"], json!([3, 10]), "{report:#}");
    assert_eq!(
        report["graphify"]["hook"]["file"],
        json!(m.repo.join(".claude/settings.json")),
        "{report:#}"
    );
    let text = graphicle_runtime::setup::render_setup(&report);
    assert!(
        text.contains("replaced the graphify section (its lines 3-10 are in the backup)"),
        "{text}"
    );
    assert!(
        text.contains("Graphify's hook in .claude/settings.json"),
        "{text}"
    );
    assert!(m.repo.join("graphify-out/graph.json").is_file());
    assert!(m.home.join(".claude/skills/graphify/SKILL.md").is_file());
    assert!(
        !m.repo.join("AGENTS.md").exists(),
        "created only with consent"
    );
    assert_eq!(report["instructions"][0]["state"], "skipped", "{report:#}");
    assert_eq!(report["git_hooks"]["state"], "skipped");
    assert_eq!(report["capture"]["state"], "skipped");

    let undone = m.graphicle(&["disconnect"]);
    assert_eq!(m.files(), before, "{undone:#}");
    let text = graphicle_runtime::disconnect::render_disconnect(&undone);
    assert!(
        text.contains(".mcp.json: removed the graphicle entry and put back Graphify's graphify entry; the file is back as it was (from its backup)"),
        "{text}"
    );
    assert!(
        text.contains("CLAUDE.md: replaced the Graphicle section with Graphify's; the file is back as it was (from its backup)"),
        "{text}"
    );
}

/// `connect` without a client connects the clients found here (`--all`
/// without asking), a second time changes nothing, and `disconnect
/// <client>` removes that client's entry; with no client installed it says
/// how to name one.
#[test]
fn connect_without_a_client_connects_what_is_installed() {
    let m = Machine::new();
    let refused = m.run(&m.repo, &["connect", "--all"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("No supported agent client was found"),
        "{refused:?}"
    );
    std::fs::create_dir_all(m.home.join(".cursor")).unwrap();
    m.graphicle(&["sync", "."]);
    let reply = m.graphicle(&["connect", "--all"]);
    assert_eq!(reply["detected"][0]["client"], "cursor", "{reply:#}");
    assert_eq!(reply["connected"][0]["entry"], "graphicle-app", "{reply:#}");
    let file = m.home.join(".cursor/mcp.json");
    assert!(read_json(&file)["mcpServers"]["graphicle-app"].is_object());
    let again = m.graphicle(&["connect", "--all"]);
    assert_eq!(again["connected"][0]["unchanged"], true, "{again:#}");
    // A single client's reply keeps its fields.
    let single = m.graphicle(&["connect", "cursor"]);
    assert_eq!(single["entry"], "graphicle-app");
    assert_eq!(single["unchanged"], true);
    let undone = m.graphicle(&["disconnect", "cursor"]);
    assert_eq!(undone["files"][0]["state"], "deleted", "{undone:#}");
    assert!(!file.exists() && m.home.join(".cursor").is_dir());
}

/// Setup outside a Git repository and without a terminal: the project is
/// synced, no client is connected without --yes or --clients, no Git hook
/// is written, AGENTS.md is not created, and nothing outside the data
/// folder changes.
#[test]
fn setup_outside_git_and_without_a_terminal_asks_for_nothing() {
    let m = Machine::new();
    std::fs::create_dir_all(m.home.join(".claude")).unwrap();
    let notes = m.root.join("notes");
    write(&notes.join("plan.md"), "# Plan\n\nShip the easy setup.\n");
    let report = m.graphicle_in(&notes, &["setup"]);
    assert!(report["project"]["files"].as_u64().unwrap() >= 1);
    assert!(
        report["project"]["note"]
            .as_str()
            .unwrap()
            .starts_with("Not a Git repository"),
        "{report:#}"
    );
    assert_eq!(report["clients"], json!([]));
    assert!(
        report["clients_note"]
            .as_str()
            .unwrap()
            .contains("graphicle setup --yes"),
        "{report:#}"
    );
    assert_eq!(report["git_hooks"]["state"], "skipped");
    assert_eq!(report["files_changed"], json!([]), "{report:#}");
    assert!(!notes.join("AGENTS.md").exists());
}

/// The freshness hooks: Git runs them without waiting for the sync; a
/// `core.hooksPath` inside the working tree is left alone, with the line to
/// add there; and, once this build's sync takes `--quiet` (the freshness
/// lane), a commit refreshes the project in the background.
#[test]
fn git_hooks_run_in_the_background_and_respect_core_hooks_path() {
    let m = Machine::new();
    m.graphicle(&["setup", "--yes", "--clients", "none", "--no-instructions"]);
    let status = |m: &Machine| m.graphicle(&["status"])["files"].as_u64().unwrap();
    let files = status(&m);
    write(&m.repo.join("src/extra.py"), "def extra():\n    return 1\n");
    m.git(&m.repo, &["add", "src/extra.py"]);
    let started = std::time::Instant::now();
    m.git(&m.repo, &["commit", "-q", "-m", "extra"]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the hook kept Git waiting"
    );
    let help = m.run(&m.repo, &["sync", "--help"]);
    if String::from_utf8_lossy(&help.stdout).contains("--quiet") {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while status(&m) == files {
            assert!(
                std::time::Instant::now() < deadline,
                "the post-commit hook did not refresh the project"
            );
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    } else {
        eprintln!(
            "sync --quiet is not in this build: the background refresh is checked once it is"
        );
    }

    let husky = m.root.join("work").join("husky");
    write(&husky.join("index.js"), "module.exports = 1;\n");
    m.git(&husky, &["init", "-q"]);
    m.git(&husky, &["config", "core.hooksPath", ".githooks"]);
    write(&husky.join(".githooks/pre-commit"), "#!/bin/sh\nnpm test\n");
    let report = m.graphicle_in(&husky, &["setup", "--yes", "--clients", "none"]);
    let git = &report["git_hooks"];
    assert_eq!(git["state"], "skipped", "{report:#}");
    assert!(git["reason"].as_str().unwrap().contains("core.hooksPath"));
    assert!(
        git["by_hand"]
            .as_str()
            .unwrap()
            .contains(&format!("sync '{}' --quiet", husky.display())),
        "{report:#}"
    );
    assert!(!husky.join(".githooks/post-commit").exists());
}

/// `uninstall --purge` disconnects everywhere, says how to remove the
/// program and deletes the data folder; without a terminal it needs --yes.
#[test]
fn uninstall_disconnects_and_purges_only_when_confirmed() {
    let m = Machine::new();
    std::fs::create_dir_all(m.home.join(".cursor")).unwrap();
    m.graphicle(&["setup", "--yes", "--no-hooks"]);
    assert!(m.home.join(".cursor/mcp.json").is_file());
    let refused = m.run(&m.repo, &["uninstall", "--purge"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes"));
    assert!(m.data.join("reviews.sqlite3").is_file(), "nothing purged");
    let report = m.graphicle(&["uninstall", "--purge", "--yes"]);
    assert_eq!(report["purged"], true, "{report:#}");
    assert!(!m.data.exists());
    assert!(!m.home.join(".cursor/mcp.json").exists());
    assert!(!m.repo.join("AGENTS.md").exists());
    // The test program is a build, or runs in a container (as `update`
    // finds it).
    let remove = report["remove_program"]["command"].as_str().unwrap();
    let rm = if cfg!(windows) {
        "Remove-Item -LiteralPath "
    } else {
        "rm "
    };
    assert!(
        remove.starts_with(rm) || remove == "docker compose down --rmi all",
        "{report:#}"
    );
}

/// A client whose file cannot be read is reported with the entry to paste,
/// the others are connected, and setup exits with an error that names it;
/// VS Code's JSONC (comments, trailing commas) is read and edited in place.
#[test]
fn setup_reports_a_client_it_could_not_connect() {
    let m = Machine::new();
    std::fs::create_dir_all(m.home.join(".vscode")).unwrap();
    std::fs::create_dir_all(m.home.join(".cursor")).unwrap();
    let broken = "{\"servers\": {\n";
    write(&m.repo.join(".vscode/mcp.json"), broken);
    let output = m.run(&m.repo, &["setup", "--yes", "--no-hooks"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("graphicle: Not connected: VS Code: "),
        "{stderr}"
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        states(&report, "clients", "client"),
        [
            ("cursor".to_string(), "added".to_string()),
            ("vscode".to_string(), "failed".to_string())
        ],
        "{report:#}"
    );
    let error = report["clients"][1]["error"].as_str().unwrap();
    assert!(error.contains("is not valid JSON (line 2"), "{error}");
    assert!(
        error.contains("docs/mcp.md#other-clients") && error.contains("\"graphicle\": {"),
        "the entry to paste: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(m.repo.join(".vscode/mcp.json")).unwrap(),
        broken,
        "left as it was"
    );
    // VS Code's comments and trailing commas: read, and kept.
    let jsonc = "// my servers\n{\n  \"servers\": {\n    \"docs\": {\"command\": \"node\"}, // the docs\n  },\n}\n";
    write(&m.repo.join(".vscode/mcp.json"), jsonc);
    let report = m.graphicle(&["setup", "--yes", "--no-hooks", "--clients", "vscode"]);
    assert_eq!(report["clients"][0]["state"], "added", "{report:#}");
    let text = std::fs::read_to_string(m.repo.join(".vscode/mcp.json")).unwrap();
    // The comment stays on the line it was written on.
    assert!(
        text.starts_with("// my servers\n{\n  \"servers\": {\n    \"docs\": {\"command\": \"node\"}, // the docs\n    \"graphicle\": {"),
        "{text}"
    );
    assert!(text.ends_with("    },\n  },\n}\n"), "{text}");
    m.graphicle(&["disconnect", "vscode"]);
    assert_eq!(
        std::fs::read_to_string(m.repo.join(".vscode/mcp.json")).unwrap(),
        jsonc
    );
    // A comment the user changes while connected is kept by disconnect
    // (the backup is not put back over it).
    m.graphicle(&["setup", "--yes", "--no-hooks", "--clients", "vscode"]);
    let connected = std::fs::read_to_string(m.repo.join(".vscode/mcp.json")).unwrap();
    write(
        &m.repo.join(".vscode/mcp.json"),
        &connected.replace("// the docs", "// the docs server"),
    );
    m.graphicle(&["disconnect", "vscode"]);
    assert_eq!(
        std::fs::read_to_string(m.repo.join(".vscode/mcp.json")).unwrap(),
        jsonc.replace("// the docs", "// the docs server")
    );
}
