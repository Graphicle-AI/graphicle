//! The easy flow's review fixes, each run as the program in a
//! temporary HOME with fake client folders and fixture Git repositories:
//! backups stay out of Git and out of the evidence; two repositories with
//! one folder name keep their own entries; `disconnect --all` leaves other
//! data folders alone; entries and hooks survive an upgrade and `doctor
//! --fix` repairs old ones; a foreground sync waits for a hook's; `open`
//! never sends the workspace key to a server it cannot trust, nor through a
//! proxy; doctor reports broken, partial and stranded connections; `sync`
//! in a subfolder syncs its project; a file left empty is removed; client
//! files keep their format; a key is never written as `key_env`; a check
//! that fails is reported.
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

struct Machine {
    _dir: TempDir,
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
    bin: PathBuf,
    /// The program the commands run.
    program: PathBuf,
}

impl Machine {
    /// A home with Claude Code's, Cursor's, VS Code's and Codex's folders,
    /// a data folder, and `bin/graphicle` linked to this build.
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let bin = root.join("bin");
        for folder in [
            home.join(".claude"),
            home.join(".cursor"),
            home.join(".vscode"),
            home.join(".codex"),
            bin.clone(),
        ] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let program = bin.join("graphicle");
        symlink(Path::new(env!("CARGO_BIN_EXE_graphicle")), &program);
        Self {
            data: root.join("data"),
            home,
            bin,
            program,
            root,
            _dir: dir,
        }
    }

    /// A committed repository at `relative` under the home.
    fn repo(&self, relative: &str) -> PathBuf {
        let repo = self.home.join(relative);
        write(
            &repo.join("src/retry.py"),
            "def retry_failed(request, attempts=3):\n    \"\"\"Retry a failed request.\"\"\"\n    for _ in range(attempts):\n        if request():\n            return True\n    return False\n",
        );
        write(
            &repo.join("README.md"),
            "# Service\n\nRetries failed requests.\n",
        );
        self.git(&repo, &["init", "-q"]);
        self.git(&repo, &["add", "."]);
        self.git(&repo, &["commit", "-q", "-m", "first"]);
        repo
    }

    fn environment(&self, command: &mut Command) {
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GRAPHICLE_NO_BROWSER", "1")
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

    /// The program with this machine's environment and data folder `data`.
    fn command(&self, cwd: &Path, data: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(&self.program);
        self.environment(&mut command);
        for (name, value) in env {
            command.env(name, value);
        }
        command
            .current_dir(cwd)
            .arg("--data-dir")
            .arg(data)
            .args(args);
        command
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        output(self.command(cwd, &self.data, args, &[]))
    }

    fn run_with(&self, cwd: &Path, data: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        output(self.command(cwd, data, args, env))
    }

    /// [`Machine::run`], which must succeed: its JSON.
    fn json(&self, cwd: &Path, args: &[&str]) -> Value {
        json_of(&self.run(cwd, args), args)
    }

    /// The readable output (GRAPHICLE_OUTPUT=text).
    fn text(&self, cwd: &Path, args: &[&str]) -> Output {
        output(self.command(cwd, &self.data, args, &[("GRAPHICLE_OUTPUT", "text")]))
    }

    fn desktop(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/Claude")
        } else {
            self.home.join(".config/Claude")
        }
    }
}

fn json_of(output: &Output, args: &[&str]) -> Value {
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("{}", path.display()))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&read(path)).unwrap()
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).unwrap();
}
#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) {
    std::fs::copy(target, link).unwrap();
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}
#[cfg(not(unix))]
fn mode(_: &Path) -> u32 {
    0
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

/// Every file under `root` (Git's objects and logs aside), with its bytes
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

/// The checks of a doctor report with `id`: `(state, detail, fix)`.
fn checks(report: &Value, id: &str) -> Vec<(String, String, String)> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["id"] == id)
        .map(|check| {
            (
                check["state"].as_str().unwrap_or_default().to_string(),
                check["detail"].as_str().unwrap_or_default().to_string(),
                check["fix"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// A copy of a backup of an ignored file never reaches `git status`, a
/// sync or an answer; Graphicle's own section and its Graphicle-only
/// client file are not evidence either; `disconnect --all` gives the
/// exclude file back as it was.
#[test]
fn backups_stay_out_of_git_and_out_of_the_evidence() {
    let m = Machine::new();
    let repo = m.repo("code/api");
    write(&repo.join(".gitignore"), ".claude/settings.local.json\n");
    write(
        &repo.join(".claude/settings.local.json"),
        "{\n  \"env\": {\"PRIVATE_TOKEN\": \"tok-123-secret\"}\n}\n",
    );
    write(&repo.join("CLAUDE.md"), "# API\n\nRun make test.\n");
    m.git(&repo, &["add", "."]);
    m.git(&repo, &["commit", "-q", "-m", "ignore local settings"]);
    let exclude = repo.join(".git/info/exclude");
    let exclude_before = std::fs::read(&exclude).unwrap();

    let setup = m.json(&repo, &["setup", "--yes", "--clients", "claude-code"]);
    assert_eq!(setup["capture"]["state"], "on", "{setup:#}");
    let backup = repo.join(".claude/settings.local.json.graphicle-bak");
    assert!(backup.is_file(), "the first copy is kept");
    let status = m.git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(!status.contains("graphicle-bak"), "{status}");
    assert!(read(&exclude).contains("*.graphicle-bak"));
    assert!(
        setup["files_changed"]
            .to_string()
            .contains(".git/info/exclude"),
        "{setup:#}"
    );

    // Neither the backup, the personal settings, AGENTS.md (Graphicle's
    // section alone) nor the Graphicle-only .mcp.json is a source.
    let packet = m.json(&repo, &["context", "PRIVATE_TOKEN env settings local"]);
    let paths: Vec<&str> = packet["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|source| source["path"].as_str())
        .collect();
    for path in &paths {
        assert!(
            !path.ends_with(".graphicle-bak")
                && !path.ends_with("settings.local.json")
                && *path != "AGENTS.md"
                && *path != ".mcp.json",
            "{paths:?}"
        );
    }
    assert!(!packet.to_string().contains("tok-123-secret"), "{packet:#}");
    let files = m.json(&repo, &["call", "files"]);
    let listed = files.to_string();
    for absent in [
        "graphicle-bak",
        "settings.local.json",
        "AGENTS.md",
        ".mcp.json",
    ] {
        assert!(!listed.contains(absent), "{absent}: {files:#}");
    }
    // CLAUDE.md is the project's: its own lines are, Graphicle's are not.
    let claude = m.json(&repo, &["context", "make test"]);
    assert!(claude.to_string().contains("Run make test"), "{claude:#}");
    assert!(!claude.to_string().contains("recall_project"), "{claude:#}");

    let undone = m.json(&repo, &["disconnect", "--all"]);
    assert_eq!(undone["kept_backups"], json!([]), "{undone:#}");
    assert_eq!(std::fs::read(&exclude).unwrap(), exclude_before);
    assert!(!backup.exists());
}

/// Two repositories named `api`: each keeps its own Cursor and Claude
/// Desktop entry, and disconnecting one leaves the other's working.
#[test]
fn two_projects_with_one_folder_name_keep_their_own_entries() {
    let m = Machine::new();
    std::fs::create_dir_all(m.desktop()).unwrap();
    let work = m.repo("work/api");
    let personal = m.repo("personal/api");
    let first = m.json(
        &work,
        &["setup", "--yes", "--clients", "cursor,claude", "--no-hooks"],
    );
    let second = m.json(
        &personal,
        &["setup", "--yes", "--clients", "cursor,claude", "--no-hooks"],
    );
    let (a, b) = (
        first["project"]["id"].as_str().unwrap().to_string(),
        second["project"]["id"].as_str().unwrap().to_string(),
    );
    assert_ne!(a, b);
    assert_eq!(second["clients"][0]["state"], "added", "{second:#}");
    let own = format!(
        "graphicle-api-{}",
        b.chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect::<String>()
            .to_ascii_lowercase()
    );
    assert_eq!(second["clients"][0]["entry"], own.as_str(), "{second:#}");
    for file in [
        m.home.join(".cursor/mcp.json"),
        m.desktop().join("claude_desktop_config.json"),
    ] {
        let servers = read_json(&file)["mcpServers"].clone();
        assert!(
            servers["graphicle-api"]["args"].to_string().contains(&a),
            "{servers:#}"
        );
        assert!(
            servers[&own]["args"].to_string().contains(&b),
            "{servers:#}"
        );
    }
    // Again: nothing changes.
    let again = m.json(
        &personal,
        &["setup", "--yes", "--clients", "cursor,claude", "--no-hooks"],
    );
    assert_eq!(again["files_changed"], json!([]), "{again:#}");
    // Disconnecting work/api leaves personal/api's entries.
    m.json(&work, &["disconnect"]);
    let servers = read_json(&m.home.join(".cursor/mcp.json"))["mcpServers"].clone();
    assert!(servers.get("graphicle-api").is_none(), "{servers:#}");
    assert!(
        servers[&own]["args"].to_string().contains(&b),
        "{servers:#}"
    );
    let shown = stdout(&m.text(&personal, &["status"]));
    assert!(
        shown.contains(&format!("Cursor (~/.cursor/mcp.json, {own}): ok")),
        "{shown}"
    );
}

/// `disconnect --all` of one data folder leaves another data folder's
/// entries, tables and capture hooks, and names them.
#[test]
fn disconnect_all_leaves_other_data_folders_alone() {
    let m = Machine::new();
    let one = m.repo("one");
    let two = m.repo("two");
    let other = m.root.join("other-data");
    let clients = [
        "setup",
        "--yes",
        "--clients",
        "cursor,codex",
        "--no-hooks",
        "--no-instructions",
    ];
    json_of(&m.run_with(&one, &m.data, &clients, &[]), &clients);
    json_of(&m.run_with(&two, &other, &clients, &[]), &clients);
    let hooks = m.home.join(".codex/hooks.json");
    assert_eq!(
        read_json(&hooks)["hooks"]["Stop"].as_array().unwrap().len(),
        2
    );

    let undone = m.json(&one, &["disconnect", "--all"]);
    let cursor = read_json(&m.home.join(".cursor/mcp.json"));
    assert!(
        cursor["mcpServers"].get("graphicle-one").is_none(),
        "{cursor:#}"
    );
    assert!(
        cursor["mcpServers"]["graphicle-two"].is_object(),
        "{cursor:#}"
    );
    let codex = read(&m.home.join(".codex/config.toml"));
    assert!(
        !codex.contains("graphicle-one") && codex.contains("graphicle-two"),
        "{codex}"
    );
    let stop = read_json(&hooks)["hooks"]["Stop"].clone();
    assert_eq!(stop.as_array().unwrap().len(), 1, "{stop:#}");
    assert!(stop.to_string().contains("other-data"), "{stop:#}");
    let others = undone["other_stores"].to_string();
    assert!(
        others.contains("graphicle-two") && others.contains("other-data"),
        "{undone:#}"
    );
    let text = stdout(&output(m.command(
        &one,
        &m.data,
        &["disconnect", "--all"],
        &[("GRAPHICLE_OUTPUT", "text")],
    )));
    assert!(
        text.contains("Left graphicle-two in ~/.cursor/mcp.json: it serves another data folder"),
        "{text}"
    );
}

/// Entries and capture hooks run the program through the link on PATH, so a
/// Homebrew upgrade that removes the old version leaves them working; hooks
/// an older setup pinned to a version that is gone are repaired by `doctor
/// --fix`.
#[test]
fn upgrades_keep_entries_and_hooks_working_and_fix_repairs_old_ones() {
    let mut m = Machine::new();
    let root = m.root.clone();
    let install = |version: &str| {
        let folder = root.join(format!("brew/Cellar/graphicle/{version}/bin"));
        std::fs::create_dir_all(&folder).unwrap();
        let program = folder.join("graphicle");
        if std::fs::hard_link(env!("CARGO_BIN_EXE_graphicle"), &program).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_graphicle"), &program).unwrap();
        }
        program
    };
    let brew_bin = m.root.join("brew/bin");
    std::fs::create_dir_all(&brew_bin).unwrap();
    symlink(&install("0.1.0"), &brew_bin.join("graphicle"));
    std::fs::remove_file(m.bin.join("graphicle")).unwrap();
    m.bin = brew_bin.clone();
    m.program = brew_bin.join("graphicle");
    let repo = m.repo("code/app");
    m.json(
        &repo,
        &["setup", "--yes", "--clients", "claude-code,cursor,codex"],
    );
    let link = brew_bin.join("graphicle").display().to_string();
    for file in [
        m.home.join(".cursor/mcp.json"),
        m.home.join(".codex/config.toml"),
        m.home.join(".codex/hooks.json"),
        repo.join(".claude/settings.local.json"),
    ] {
        let text = read(&file);
        assert!(
            text.contains(&link) && !text.contains("Cellar"),
            "{}: {text}",
            file.display()
        );
    }
    // `brew upgrade`: a new version, the link moved, the old one removed.
    symlink(&install("0.1.1"), &brew_bin.join("graphicle"));
    std::fs::remove_dir_all(m.root.join("brew/Cellar/graphicle/0.1.0")).unwrap();
    let doctor = m.json(&repo, &["doctor"]);
    assert_eq!(doctor["problems"], 0, "{doctor:#}");
    assert!(
        checks(&doctor, "capture")
            .iter()
            .all(|(state, _, _)| state == "ok"),
        "{doctor:#}"
    );
    assert!(
        checks(&doctor, "client")
            .iter()
            .all(|(state, _, _)| state == "ok"),
        "{doctor:#}"
    );
    // Hooks an older setup pinned to a version that is gone.
    let settings = repo.join(".claude/settings.local.json");
    let pinned = m
        .root
        .join("brew/Cellar/graphicle/0.0.9/bin/graphicle")
        .display()
        .to_string();
    std::fs::write(&settings, read(&settings).replace(&link, &pinned)).unwrap();
    let hooks = m.home.join(".codex/hooks.json");
    std::fs::write(&hooks, read(&hooks).replace(&link, &pinned)).unwrap();
    let broken = m.run(&repo, &["doctor"]);
    assert!(!broken.status.success());
    let broken: Value = serde_json::from_slice(&broken.stdout).unwrap();
    let capture = checks(&broken, "capture");
    assert_eq!(capture.len(), 2, "{broken:#}");
    for (state, detail, fix) in &capture {
        assert_eq!(state, "fail", "{detail}");
        assert_eq!(fix, "graphicle doctor --fix");
    }
    let fixed = m.json(&repo, &["doctor", "--fix"]);
    assert_eq!(fixed["ok"], true, "{fixed:#}");
    assert!(read(&settings).contains(&link) && !read(&settings).contains(&pinned));
    assert!(read(&hooks).contains(&link) && !read(&hooks).contains(&pinned));
    let text = stdout(&m.text(&repo, &["doctor"]));
    assert!(!text.contains("`graphicle doctor --fix` applies"), "{text}");
}

/// A foreground `graphicle sync` waits while another process (a Git hook's
/// background sync) syncs the same folder, then syncs, instead of failing.
#[test]
fn a_foreground_sync_waits_for_a_hook_sync_of_the_same_folder() {
    let m = Machine::new();
    let repo = m.repo("code/app");
    m.json(&repo, &["sync", "."]);
    write(&repo.join("src/extra.py"), "def extra():\n    return 2\n");
    let folder = repo.canonicalize().unwrap();
    let digest = graphicle_runtime::digest(&json!(folder.to_string_lossy()));
    let lock_path = m.data.join("fresh").join(format!("{}.lock", &digest[..32]));
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    held.lock().unwrap();
    let mut command = m.command(&repo, &m.data, &["sync"], &[]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the sync waits for the lock"
    );
    drop(held);
    let done = child.wait_with_output().unwrap();
    let synced = json_of(&done, &["sync"]);
    assert_eq!(synced["files"], 3, "{synced:#}");
}

/// A listener on a local port that answers every request with `body` and
/// keeps what it received.
fn listener(body: &'static str) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = seen.clone();
    std::thread::spawn(move || {
        for stream in socket.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(read) = stream.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") && !request.starts_with(b"POST") {
                    break;
                }
            }
            // A probe that only connects sends nothing.
            if !request.is_empty() {
                kept.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).into_owned());
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, seen)
}

/// `open` sends the workspace key only to the server of its own data folder
/// (which proves it holds the folder's secret), never to another program
/// that answers as Graphicle, and never through a proxy.
#[test]
fn open_never_sends_the_key_to_a_server_it_cannot_trust() {
    let m = Machine::new();
    let repo = m.repo("code/app");
    m.json(&repo, &["sync", "."]);
    let session: Value = read_json(&m.data.join("local-session.json"));
    let capability = session["capability"].as_str().unwrap().to_string();
    let (fake, received) = listener(r#"{"product":"Graphicle","version":"9.9.9"}"#);
    let (proxy, proxied) = listener("{}");
    let proxy_url = format!("http://127.0.0.1:{proxy}");
    let proxies = [
        ("HTTP_PROXY", proxy_url.as_str()),
        ("http_proxy", proxy_url.as_str()),
        ("HTTPS_PROXY", proxy_url.as_str()),
        ("ALL_PROXY", proxy_url.as_str()),
    ];
    let refused = m.run_with(
        &repo,
        &m.data,
        &["open", "--port", &fake.to_string()],
        &proxies,
    );
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(
        stderr(&refused).contains("another data folder"),
        "{}",
        stderr(&refused)
    );
    // Doctor and status say whose server it is.
    let doctor = m.json(&repo, &["doctor", "--port", &fake.to_string()]);
    let port_check = checks(&doctor, "port");
    assert_eq!(port_check[0].0, "warn", "{doctor:#}");
    assert!(
        port_check[0].1.contains("of another data folder"),
        "{doctor:#}"
    );
    let requests = received.lock().unwrap().clone();
    assert!(!requests.is_empty());
    for request in &requests {
        assert!(request.starts_with("GET /health"), "{request}");
        assert!(
            !request.contains(&capability) && !request.contains("x-workspace-capability"),
            "{request}"
        );
    }
    // This data folder's own server is reused, and not through the proxy.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut serve = m.command(&repo, &m.data, &["serve", "--port", &port.to_string()], &[]);
    serve
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut server = crate::fork_guard::spawn(&mut serve).unwrap();
    let ready = std::io::BufRead::lines(std::io::BufReader::new(server.stderr.take().unwrap()))
        .map_while(Result::ok)
        .any(|line| line.contains("Graphicle is ready at"));
    assert!(ready);
    let shown = stdout(&m.text(&repo, &["status"]));
    let opened = m.run_with(
        &repo,
        &m.data,
        &["open", "--port", &port.to_string()],
        &proxies,
    );
    let _ = server.kill();
    let _ = server.wait();
    assert!(opened.status.success(), "{}", stderr(&opened));
    assert!(
        stdout(&opened).contains(&format!("http://127.0.0.1:{port}/#session=")),
        "{opened:?}"
    );
    assert!(
        proxied.lock().unwrap().is_empty(),
        "nothing went through the proxy"
    );
    assert!(shown.contains("Server    "), "{shown}");
}

/// Doctor and status report a Graphicle entry that no longer runs `graphicle
/// mcp`, a client file that is no longer JSON, missing Git and capture
/// hooks, and agents left connected to a data folder that is gone; `--fix`
/// repairs what it safely can.
#[test]
fn doctor_reports_broken_partial_and_stranded_connections() {
    let m = Machine::new();
    let repo = m.repo("code/app");
    m.json(
        &repo,
        &["setup", "--yes", "--clients", "claude-code,vscode,cursor"],
    );
    // A Claude Code entry that lost `mcp`, and a VS Code file with a stray
    // brace.
    let mcp = repo.join(".mcp.json");
    let mut document = read_json(&mcp);
    document["mcpServers"]["graphicle"]["args"] = json!(["--project", "."]);
    std::fs::write(&mcp, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let vscode = repo.join(".vscode/mcp.json");
    let good = read(&vscode);
    std::fs::write(&vscode, format!("{good}}}")).unwrap();
    let broken = m.run(&repo, &["doctor"]);
    assert!(!broken.status.success());
    let report: Value = serde_json::from_slice(&broken.stdout).unwrap();
    let clients = checks(&report, "client");
    assert!(
        clients.iter().any(|(state, detail, fix)| state == "fail"
            && detail.contains("Claude Code entry graphicle")
            && detail.contains("no longer runs graphicle mcp")
            && fix == "graphicle doctor --fix"),
        "{report:#}"
    );
    assert!(
        clients.iter().any(|(state, detail, _)| state == "fail"
            && detail.contains("VS Code cannot read")
            && detail.contains("not valid JSON")),
        "{report:#}"
    );
    let shown = stdout(&m.text(&repo, &["status"]));
    assert!(
        shown.contains("Claude Code (.mcp.json): no longer runs graphicle mcp"),
        "{shown}"
    );
    assert!(
        shown.contains("VS Code (.vscode/mcp.json): not valid JSON"),
        "{shown}"
    );
    // The VS Code file still needs a hand: --fix leaves it, and says so.
    let fixing = m.run(&repo, &["doctor", "--fix"]);
    assert_eq!(fixing.status.code(), Some(1));
    let fixed: Value = serde_json::from_slice(&fixing.stdout).unwrap();
    assert!(read(&mcp).contains("\"mcp\""), "{fixed:#}");
    assert!(
        checks(&fixed, "client")
            .iter()
            .all(|(state, detail, _)| state == "ok" || detail.contains("VS Code")),
        "{fixed:#}"
    );
    std::fs::write(&vscode, &good).unwrap();
    assert_eq!(m.json(&repo, &["doctor"])["problems"], 0);

    // Missing hooks: post-merge's block, Claude Code's Stop hook.
    let hooks = repo.join(".git/hooks");
    std::fs::remove_file(hooks.join("post-merge")).unwrap();
    let settings = repo.join(".claude/settings.local.json");
    let mut local = read_json(&settings);
    local["hooks"].as_object_mut().unwrap().remove("Stop");
    std::fs::write(&settings, serde_json::to_string_pretty(&local).unwrap()).unwrap();
    let partial = m.json(&repo, &["doctor"]);
    let git = checks(&partial, "git_hooks");
    assert_eq!(git[0].0, "warn", "{partial:#}");
    assert!(
        git[0].1.contains("post-merge has no Graphicle block"),
        "{partial:#}"
    );
    assert_eq!(git[0].2, "graphicle setup");
    let capture = checks(&partial, "capture");
    assert!(
        capture.iter().any(|(state, detail, fix)| state == "warn"
            && detail.contains("no Stop hook")
            && fix == "graphicle doctor --fix"),
        "{partial:#}"
    );
    let shown = stdout(&m.text(&repo, &["status"]));
    assert!(shown.contains("claude-code (no Stop hook)"), "{shown}");
    assert!(
        shown.contains("(post-merge missing: graphicle setup)"),
        "{shown}"
    );
    m.json(&repo, &["doctor", "--fix"]);
    assert!(read_json(&settings)["hooks"]["Stop"].is_array());
    m.json(&repo, &["setup", "--yes", "--clients", "none"]);
    let whole = m.json(&repo, &["doctor"]);
    assert_eq!(checks(&whole, "git_hooks")[0].0, "ok", "{whole:#}");
    assert!(
        checks(&whole, "capture")
            .iter()
            .all(|(state, _, _)| state == "ok"),
        "{whole:#}"
    );

    // The data folder is gone: the connected agents cannot start.
    let moved = m.root.join("moved-data");
    std::fs::rename(&m.data, &moved).unwrap();
    let stranded = m.run(&repo, &["doctor"]);
    assert_eq!(stranded.status.code(), Some(1), "{}", stdout(&stranded));
    let report: Value = serde_json::from_slice(&stranded.stdout).unwrap();
    let agents = checks(&report, "agents");
    assert_eq!(agents.len(), 1, "{report:#}");
    assert_eq!(agents[0].0, "fail");
    assert!(
        agents[0].1.contains("does not exist") && agents[0].1.contains("Cursor entry"),
        "{report:#}"
    );
    assert_eq!(agents[0].2, "graphicle setup");
    assert!(!m.data.exists(), "doctor created nothing");
    let text = stdout(&m.text(&repo, &["doctor"]));
    assert!(
        text.contains("next: cd your-repo && graphicle setup"),
        "{text}"
    );
    // Neither does an agent's server.
    let mcp_run = m.run(&repo, &["mcp"]);
    assert_eq!(mcp_run.status.code(), Some(1));
    assert!(
        stderr(&mcp_run).contains("graphicle setup"),
        "{}",
        stderr(&mcp_run)
    );
    assert!(!m.data.exists(), "mcp created nothing");
}

/// `graphicle sync` without a folder, in a subfolder of a set-up project,
/// syncs that project instead of making the subfolder a project.
#[test]
fn sync_in_a_subfolder_syncs_its_project() {
    let m = Machine::new();
    let repo = m.repo("code/app");
    let project = m.json(&repo, &["sync"])["repository_id"].clone();
    write(&repo.join("src/more.py"), "def more():\n    return 1\n");
    let synced = m.json(&repo.join("src"), &["sync"]);
    assert_eq!(synced["repository_id"], project, "{synced:#}");
    assert_eq!(synced["files"], 3, "{synced:#}");
    let status = m.json(&repo.join("src"), &["status"]);
    assert_eq!(status["repository"]["id"], project);
    // An explicit folder still makes its own project.
    let nested = m.json(&repo.join("src"), &["sync", "."]);
    assert_ne!(nested["repository_id"], project);
}

/// After the data folder was lost and setup ran again, `disconnect` removes
/// the files Graphicle left with nothing else in them.
#[test]
fn disconnect_after_a_lost_data_folder_leaves_no_empty_files() {
    let m = Machine::new();
    let repo = m.repo("code/app");
    let before = tree(&repo);
    m.json(&repo, &["setup", "--yes", "--clients", "claude-code,codex"]);
    std::fs::remove_dir_all(&m.data).unwrap();
    m.json(&repo, &["setup", "--yes", "--clients", "claude-code,codex"]);
    let undone = m.json(&repo, &["disconnect", "--all"]);
    for gone in [
        repo.join("AGENTS.md"),
        repo.join(".mcp.json"),
        repo.join(".claude/settings.local.json"),
        repo.join(".git/hooks/post-commit"),
        repo.join(".git/hooks/post-merge"),
        repo.join(".git/hooks/post-checkout"),
        m.home.join(".codex/hooks.json"),
    ] {
        assert!(!gone.exists(), "{} is left: {undone:#}", gone.display());
    }
    let after = tree(&repo);
    let differences: Vec<&PathBuf> = before
        .keys()
        .chain(after.keys())
        .filter(|path| {
            before.get(*path) != after.get(*path) && !path.to_string_lossy().contains(".git/index")
        })
        .collect();
    assert!(differences.is_empty(), "{differences:#?}\n{undone:#}");
}

/// Connecting changes only Graphicle's entry in a client file: the other
/// entries keep their text, the file its indentation, final newline and
/// mode; disconnecting gives the file back byte for byte.
#[test]
fn connect_keeps_a_client_files_format_and_mode() {
    let m = Machine::new();
    let repo = m.repo("code/app");
    let original = "{\n    \"mcpServers\": {\n        \"docs\": {\"command\": \"node\", \"args\": [\"--docs\"]}\n    }\n}\n";
    let file = repo.join(".mcp.json");
    write(&file, original);
    set_mode(&file, 0o644);
    m.json(
        &repo,
        &["setup", "--yes", "--clients", "claude-code", "--no-capture"],
    );
    let text = read(&file);
    assert!(
        text.starts_with("{\n    \"mcpServers\": {\n        \"docs\": {\"command\": \"node\", \"args\": [\"--docs\"]},\n        \"graphicle\": {\n"),
        "{text}"
    );
    assert!(text.ends_with("    }\n}\n"), "{text}");
    assert_eq!(mode(&file), 0o644);
    m.json(&repo, &["disconnect"]);
    assert_eq!(read(&file), original);
    assert_eq!(mode(&file), 0o644);
}

/// `config set model … --key-env` takes a variable's name: a key is
/// refused, never written and never repeated.
#[test]
fn a_key_is_never_written_as_the_key_variable() {
    let m = Machine::new();
    let repo = m.repo("code/app");
    let refused = m.run(
        &repo,
        &[
            "config",
            "set",
            "model",
            "anthropic",
            "--key-env",
            "sk-ant-api03-SECRET",
        ],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(!stderr(&refused).contains("SECRET") && !stdout(&refused).contains("SECRET"));
    assert!(
        stderr(&refused).contains("name of the environment variable"),
        "{}",
        stderr(&refused)
    );
    assert!(!m.data.join("models.json").exists());
    let named = m.json(
        &repo,
        &[
            "config",
            "set",
            "model",
            "anthropic",
            "--key-env",
            "WORK_ANTHROPIC_KEY",
        ],
    );
    assert_eq!(named["key_env"], "WORK_ANTHROPIC_KEY");
}

/// Outside any project, `context` says on a terminal which project answers
/// (the last synced one); piped, its JSON is as before. A freshness check
/// that fails is reported as stale, never passed off as fresh.
#[test]
fn answers_name_their_project_and_a_failed_check_is_reported() {
    let m = Machine::new();
    let repo = m.repo("code/app");
    m.json(&repo, &["sync", "."]);
    let notes = m.home.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let text = stdout(&m.text(&notes, &["context", "where are failed requests retried?"]));
    assert!(
        text.starts_with("! From app (~/code/app), the last synced project: this folder is not set up (graphicle setup adds it).\n"),
        "{text}"
    );
    let piped = m.json(&notes, &["context", "where are failed requests retried?"]);
    assert!(
        !piped.to_string().contains("last synced project"),
        "{piped:#}"
    );
    let inside = stdout(&m.text(&repo, &["context", "where are failed requests retried?"]));
    assert!(!inside.contains("last synced project"), "{inside}");
    // The refresh lock cannot be taken: a stale note with the error.
    std::fs::remove_dir_all(m.data.join("fresh")).ok();
    write(&m.data.join("fresh"), "not a folder");
    write(
        &repo.join("src/retry.py"),
        "def retry_failed(request):\n    return request()\n",
    );
    let packet = m.json(&repo, &["context", "retry"]);
    let report = &packet["folder_freshness"];
    assert_eq!(report["status"], "stale", "{packet:#}");
    assert_eq!(report["reason"], "error", "{packet:#}");
}

/// A mistyped command gets clap's suggestions, the closest first.
#[test]
fn a_typo_suggests_the_closest_command_first() {
    let m = Machine::new();
    let typo = m.run(&m.home, &["setpu"]);
    assert_eq!(typo.status.code(), Some(2));
    let message = stderr(&typo);
    assert!(
        message.contains("tip: some similar subcommands exist: 'setup', 'status'"),
        "{message}"
    );
}
