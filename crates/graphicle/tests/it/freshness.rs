//! Freshness: answers come from what is in the folder
//! now. The built program runs in a temporary home that holds client
//! configuration folders, on a fixture git repository (and a plain folder):
//! `context`, `status`, `ask`'s path and MCP tool calls sync a few changed
//! files first and say so in `folder_freshness`; many changes give a stale
//! note and leave the store as it was; `freshness: off` and
//! `GRAPHICLE_FRESHNESS=off` turn it off; `graphicle sync --quiet`, which the
//! git hooks run, prints nothing and does nothing when nothing changed; two
//! processes never sync one folder twice; projects without a folder are never
//! touched.
use graphicle_runtime::platform::CanonicalExt;
use graphicle_runtime::{
    graph_projection,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const A_V1: &str = "def alpha():\n    return 1\n";
const A_V2: &str = "def alpha():\n    return 2\n\ndef gamma_helper():\n    return 3\n";
const B: &str = "from a import alpha\n\ndef run():\n    return alpha()\n";

/// A temporary home with the configuration folders of the clients `connect`
/// knows, a store, and a git repository `app` with one commit.
struct Home {
    _dir: TempDir,
    home: PathBuf,
    data: PathBuf,
    app: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let home = dir.path().canonical().unwrap();
        for client in [
            ".claude",
            ".cursor",
            ".codex",
            "Library/Application Support/Claude",
            ".config/Code/User",
        ] {
            std::fs::create_dir_all(home.join(client)).unwrap();
        }
        let app = home.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let this = Self {
            data: home.join("data"),
            app,
            home,
            _dir: dir,
        };
        this.git(&["init", "-q", "-b", "main"]);
        std::fs::write(this.app.join("a.py"), A_V1).unwrap();
        std::fs::write(this.app.join("b.py"), B).unwrap();
        std::fs::write(
            this.app.join("notes.md"),
            "# Notes\n\nThe app runs alpha.\n",
        )
        .unwrap();
        this.git(&["add", "-A"]);
        this.git(&["commit", "-q", "-m", "one"]);
        this
    }

    fn git(&self, args: &[&str]) {
        let mut command = Command::new("git");
        crate::child_env::home(&mut command, &self.home)
            .arg("-C")
            .arg(&self.app)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = crate::fork_guard::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The program in `cwd`, in this home, with `env` set (and
    /// `GRAPHICLE_FRESHNESS` only when `env` sets it).
    fn command(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        crate::child_env::home(&mut command, &self.home)
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GRAPHICLE_FRESHNESS")
            .env_remove("GRAPHICLE_DATA_DIR")
            .arg("--data-dir")
            .arg(&self.data)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        command
    }

    fn output(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        crate::fork_guard::spawn(&mut self.command(cwd, args, env))
            .unwrap()
            .wait_with_output()
            .unwrap()
    }

    /// The JSON the program prints in the repository.
    fn json(&self, args: &[&str]) -> Value {
        self.json_in(&self.app, args, &[])
    }

    fn json_in(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Value {
        let output = self.output(cwd, args, env);
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

    fn store(&self) -> Store {
        Store::open(&self.data).unwrap()
    }

    /// The project's snapshot, completed syncs and head paths.
    fn state(&self, project: &str) -> (String, i64, Vec<String>) {
        self.store()
            .read(|db| {
                let snapshot: String = db.query_row(
                    "SELECT snapshot FROM repo_projects WHERE id=?",
                    [project],
                    |r| r.get(0),
                )?;
                let syncs: i64 = db.query_row(
                    "SELECT count(*) FROM repo_syncs WHERE repository_id=? AND state='complete'",
                    [project],
                    |r| r.get(0),
                )?;
                let mut statement =
                    db.prepare("SELECT path FROM repo_heads WHERE repository_id=? ORDER BY path")?;
                let heads = statement
                    .query_map([project], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((snapshot, syncs, heads))
            })
            .unwrap()
    }

    /// Record the project's last sync as quick (100 ms), as an idle machine
    /// times it. The tests that expect a refresh pin this input of the
    /// slow-sync rule, which a busy machine (a build beside the tests) trips
    /// by itself; the rule has its own test below.
    fn quick_sync(&self, project: &str) {
        self.store()
            .transaction(|db| {
                db.execute(
                    "UPDATE repo_folders SET freshness=json_set(freshness,'$.scan_ms',100) WHERE repository_id=?",
                    [project],
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// Sync the repository and return its project.
    fn sync(&self) -> String {
        let synced = self.json(&["sync", "."]);
        assert!(
            ["built", "current"].contains(&s(&synced["graph"], "status")),
            "{synced:#}"
        );
        s(&synced, "repository_id").to_string()
    }

    /// Wait until the project's current head set has its code graph.
    fn graph_current(&self, project: &str) {
        let store = self.store();
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let recorded = store
                .read(|db| {
                    let episode = graph_projection::current_episode(db, project)?;
                    graph_projection::is_recorded(db, project, &episode)
                })
                .unwrap();
            if recorded {
                return;
            }
            assert!(Instant::now() < deadline, "the code graph was not built");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// `context` and `status` sync a few changed files first (tracked, new and
/// deleted ones) and report it beside the packet's own `freshness`; the next
/// call has nothing to report.
#[test]
fn context_and_status_sync_a_few_changed_files_first() {
    let h = Home::new();
    let project = h.sync();
    h.quick_sync(&project);
    let quiet = h.json(&["context", "what does run call"]);
    assert!(quiet.get("folder_freshness").is_none(), "{quiet:#}");
    assert!(quiet["freshness"].is_object(), "the packet's own freshness");
    assert!(h.json(&["status"]).get("folder_freshness").is_none());
    let (before, syncs, _) = h.state(&project);

    std::fs::write(h.app.join("a.py"), A_V2).unwrap();
    std::fs::write(h.app.join("c.py"), "def delta():\n    return 4\n").unwrap();
    // The retrieval's excerpts (a question that only asks where a name is
    // defined takes the identifier route unless asked otherwise).
    let packet = h.json(&[
        "context",
        "where is gamma_helper defined",
        "--no-identifier-route",
    ]);
    let fresh = &packet["folder_freshness"];
    assert_eq!(fresh["status"], "refreshed", "{packet:#}");
    assert_eq!(fresh["changed"], 2);
    assert_eq!(fresh["sample"], json!(["a.py", "c.py"]));
    assert!(
        ["built", "background"].contains(&s(fresh, "graph")),
        "{fresh}"
    );
    assert!(
        s(fresh, "note").starts_with("synced 2 changed files before answering"),
        "{fresh}"
    );
    assert!(
        packet["freshness"].get("stale_count").is_some(),
        "{packet:#}"
    );
    assert!(
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|window| window["id"] == "file:a.py"
                && window["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("gamma_helper"))),
        "the answer reads the new a.py: {}",
        packet["evidence"]
    );
    let (after, synced, heads) = h.state(&project);
    assert_ne!(after, before);
    assert_eq!(synced, syncs + 1);
    assert_eq!(heads, ["a.py", "b.py", "c.py", "notes.md"]);
    h.graph_current(&project);
    let again = h.json(&["context", "where is gamma_helper defined"]);
    assert!(again.get("folder_freshness").is_none(), "{again:#}");
    assert_eq!(
        again["code_graph"]["episode_lag"], 0,
        "{}",
        again["code_graph"]
    );
    // The identifier route reads the new definition on the current graph.
    assert_eq!(again["route"]["path"], "identifier", "{again:#}");
    assert_eq!(again["symbols"][0]["definitions"][0]["relpath"], "a.py");

    // A deleted file, before `status`.
    std::fs::remove_file(h.app.join("b.py")).unwrap();
    let status = h.json(&["status"]);
    assert_eq!(
        status["folder_freshness"]["status"], "refreshed",
        "{status:#}"
    );
    assert_eq!(status["folder_freshness"]["changed"], 1);
    assert_eq!(h.state(&project).2, ["a.py", "c.py", "notes.md"]);
}

/// Over 200 changed files: the answer comes with a stale note naming the
/// command, and the store keeps its snapshot. Outside the folder the note
/// names it. A folder whose sync reads slowly is not refreshed either.
#[test]
fn many_changes_or_a_slow_sync_give_a_stale_note_and_leave_the_store() {
    let h = Home::new();
    let project = h.sync();
    let (before, syncs, _) = h.state(&project);
    for i in 0..201 {
        std::fs::write(h.app.join(format!("gen{i}.py")), format!("X = {i}\n")).unwrap();
    }
    let packet = h.json(&["context", "what does run call"]);
    let fresh = &packet["folder_freshness"];
    assert_eq!(fresh["status"], "stale", "{packet:#}");
    assert_eq!(fresh["reason"], "too_many_changes");
    assert_eq!(fresh["changed"], 201);
    assert_eq!(
        fresh["note"],
        "201 files changed since the last sync; run graphicle sync"
    );
    let elsewhere = h.json_in(&h.home, &["status", "--project", &project], &[]);
    // The folder is quoted for the shell when it needs it (a Windows path
    // does), as `graphicle sync` prints it.
    let mut folder = h.app.display().to_string();
    if folder.contains(|c: char| !(c.is_ascii_alphanumeric() || "/._-+".contains(c))) {
        folder = format!("'{}'", folder.replace('\'', r"'\''"));
    }
    assert_eq!(
        elsewhere["folder_freshness"]["note"],
        format!("201 files changed since the last sync; run graphicle sync {folder}"),
        "{elsewhere:#}"
    );
    assert_eq!(h.state(&project).0, before);
    assert_eq!(h.state(&project).1, syncs);

    // The same folder after a sync that read slowly.
    for i in 0..201 {
        std::fs::remove_file(h.app.join(format!("gen{i}.py"))).unwrap();
    }
    h.sync();
    h.store()
        .transaction(|db| {
            db.execute(
                "UPDATE repo_folders SET freshness=json_set(freshness,'$.scan_ms',5000) WHERE repository_id=?",
                [&project],
            )?;
            Ok(())
        })
        .unwrap();
    std::fs::write(h.app.join("a.py"), A_V2).unwrap();
    let slow = h.json(&["context", "what does run call"]);
    assert_eq!(slow["folder_freshness"]["reason"], "slow_sync", "{slow:#}");
    assert!(
        s(&slow["folder_freshness"], "note").contains("run graphicle sync (a sync of this folder")
    );
}

/// `freshness: off` in the configuration, or `GRAPHICLE_FRESHNESS=off`,
/// leaves the folder alone and the output as before; `auto` syncs.
#[test]
fn freshness_off_leaves_the_folder_alone() {
    let h = Home::new();
    let project = h.sync();
    h.quick_sync(&project);
    let (before, _, _) = h.state(&project);
    std::fs::write(h.app.join("a.py"), A_V2).unwrap();
    std::fs::write(
        h.data.join("models.json"),
        json!({"freshness":"off"}).to_string(),
    )
    .unwrap();
    let off = h.json(&["context", "what does run call"]);
    assert!(off.get("folder_freshness").is_none(), "{off:#}");
    assert!(h.json(&["status"]).get("folder_freshness").is_none());
    assert_eq!(h.state(&project).0, before);
    std::fs::remove_file(h.data.join("models.json")).unwrap();
    let env = h.json_in(
        &h.app,
        &["context", "what does run call"],
        &[("GRAPHICLE_FRESHNESS", "off")],
    );
    assert!(env.get("folder_freshness").is_none(), "{env:#}");
    assert_eq!(h.state(&project).0, before);
    let on = h.json(&["context", "what does run call"]);
    assert_eq!(on["folder_freshness"]["status"], "refreshed", "{on:#}");
    assert_ne!(h.state(&project).0, before);
}

/// What the git hooks run: `graphicle sync --quiet` (the folder by default)
/// prints nothing. Nothing changed: no sync. A commit alone: no sync, and the
/// record takes the new git state. A changed file: the sync and its code
/// graph.
#[test]
fn sync_quiet_prints_nothing_and_does_nothing_when_nothing_changed() {
    let h = Home::new();
    let silent = |label: &str| {
        let output = h.output(&h.app, &["sync", "--quiet"], &[]);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{label}: {output:?}");
        assert!(output.stderr.is_empty(), "{label}: {output:?}");
    };
    // A hook's quiet sync never creates a data folder, nor a project of a
    // folder that is not one, nor moves the saved default.
    silent("no data folder");
    assert!(!h.data.exists(), "nothing was created");
    let other = h.home.join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("x.py"), "def x():\n    return 0\n").unwrap();
    h.json_in(&other, &["sync", "."], &[]);
    let project = h.sync();
    let quiet = h.output(&other, &["sync", "--quiet", "."], &[]);
    assert!(
        quiet.status.success() && quiet.stdout.is_empty(),
        "{quiet:?}"
    );
    std::fs::write(h.app.join("never.py"), "def never():\n    return 9\n").unwrap();
    let elsewhere = h.home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let unregistered = h.output(&elsewhere, &["sync", "--quiet", "."], &[]);
    assert!(unregistered.status.success() && unregistered.stdout.is_empty());
    let saved: Value =
        serde_json::from_slice(&std::fs::read(h.data.join("local-session.json")).unwrap()).unwrap();
    assert_eq!(
        saved["project"],
        project.as_str(),
        "the saved default stays"
    );
    std::fs::remove_file(h.app.join("never.py")).unwrap();
    silent("first sync");
    let status = h.json(&["status"]);
    assert_eq!(s(&status["repository"], "id"), project, "{status:#}");
    h.graph_current(&project);
    let (first, syncs, _) = h.state(&project);
    let record = || -> Value {
        h.store()
            .read(|db| {
                let text: String = db.query_row(
                    "SELECT freshness FROM repo_folders WHERE repository_id=?",
                    [&project],
                    |r| r.get(0),
                )?;
                Ok(serde_json::from_str(&text).unwrap())
            })
            .unwrap()
    };
    let recorded = record();
    assert_eq!(recorded["snapshot"], first, "{recorded}");
    assert!(recorded["git"].is_string(), "{recorded}");

    silent("nothing changed");
    let (unchanged, same, _) = h.state(&project);
    assert_eq!((unchanged, same), (first.clone(), syncs));
    assert_eq!(record(), recorded);

    // A commit moves HEAD and the status, not the files.
    std::fs::write(h.app.join("c.py"), "def delta():\n    return 4\n").unwrap();
    silent("a new file");
    let (second, synced, heads) = h.state(&project);
    assert_ne!(second, first);
    assert_eq!(synced, syncs + 1);
    assert!(heads.contains(&"c.py".to_string()));
    h.graph_current(&project);
    let before_commit = record();
    h.git(&["add", "-A"]);
    h.git(&["commit", "-q", "-m", "two"]);
    silent("a commit");
    assert_eq!(h.state(&project).1, synced, "a commit is no sync");
    let after_commit = record();
    assert_eq!(after_commit["snapshot"], second);
    assert_ne!(
        after_commit["git"], before_commit["git"],
        "the new git state"
    );
    silent("after the commit");
    assert_eq!(record(), after_commit);
    assert_eq!(h.state(&project).1, synced);
}

/// Every MCP tool call checks first: a changed file is synced before the
/// call answers, reported in the structured content and the JSON text the
/// agent reads, beside the packet's own `freshness`.
#[test]
fn mcp_tool_calls_sync_changed_files_before_answering() {
    use std::io::{BufRead, Write};
    let h = Home::new();
    let project = h.sync();
    let connected = h.json(&["connect", "claude-code"]);
    assert_eq!(connected["entry"], "graphicle", "{connected:#}");
    // `connect` wrote .mcp.json into the folder: sync it, so the session
    // starts fresh.
    h.sync();
    let entry: Value =
        serde_json::from_slice(&std::fs::read(h.app.join(".mcp.json")).unwrap()).unwrap();
    let args: Vec<String> = entry["mcpServers"]["graphicle"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect();
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    crate::child_env::home(&mut command, &h.home)
        .current_dir(&h.app)
        .env_remove("GRAPHICLE_FRESHNESS")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut replies = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut call = |id: u64, method: &str, params: Value| -> Value {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        replies.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not a reply: {line:?}"))
    };
    call(
        1,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fresh","version":"0"}}),
    );
    // The retrieval's excerpts, not the identifier route's definitions.
    let recall = json!({"name":"recall_project","arguments":{"question":"where is gamma_helper defined","structured":true,"identifier_route":false}});
    let first = call(2, "tools/call", recall.clone());
    assert!(
        first["result"]["structuredContent"]
            .get("folder_freshness")
            .is_none(),
        "{first:#}"
    );
    let (before, _, _) = h.state(&project);
    std::fs::write(h.app.join("a.py"), A_V2).unwrap();
    // Past the few seconds a check is reused for.
    std::thread::sleep(Duration::from_millis(2200));
    let second = call(3, "tools/call", recall.clone());
    let packet = &second["result"]["structuredContent"];
    assert_eq!(
        packet["folder_freshness"]["status"], "refreshed",
        "{second:#}"
    );
    assert_eq!(
        packet["folder_freshness"]["changed"], 1,
        "{}",
        packet["folder_freshness"]
    );
    assert!(packet["freshness"].get("stale_count").is_some());
    let text: Value =
        serde_json::from_str(second["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["folder_freshness"], packet["folder_freshness"]);
    assert!(
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|window| window["text"]
                .as_str()
                .is_some_and(|text| text.contains("gamma_helper"))),
        "{}",
        packet["evidence"]
    );
    assert_ne!(h.state(&project).0, before);
    let third = call(4, "tools/call", recall);
    assert!(
        third["result"]["structuredContent"]
            .get("folder_freshness")
            .is_none(),
        "{third:#}"
    );
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    h.graph_current(&project);
}

/// The HTTP MCP endpoint of `graphicle serve` refreshes a changed folder
/// before a tool call as `graphicle mcp` does, for a caller of the data
/// folder's own workspace who may read the project (an agent's key too):
/// the sync runs as that folder's owner. A caller of another workspace
/// changes nothing.
#[tokio::test]
async fn http_mcp_tool_calls_sync_changed_files_before_answering() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    if std::env::var_os("GRAPHICLE_FRESHNESS").is_some() {
        return;
    }
    let h = Home::new();
    let project = h.sync();
    let session: Value =
        serde_json::from_slice(&std::fs::read(h.data.join("local-session.json")).unwrap()).unwrap();
    let (workspace, owner) = (s(&session, "workspace"), s(&session, "capability"));
    let store = h.store();
    let agent = store
        .grant(
            workspace,
            owner,
            &json!({"role":"agent","label":"an agent"}),
        )
        .unwrap();
    let agent = s(&agent, "capability").to_string();
    let other = store.create_workspace("Another").unwrap();
    let app = graphicle_runtime::api::router(
        graphicle_runtime::api::App::new(
            store,
            graphicle_runtime::model::Config::default(),
            vec!["localhost".into()],
        )
        .unwrap(),
    );
    let call = |workspace: String, key: String| {
        let app = app.clone();
        let request = Request::builder()
            .method("POST")
            .uri(format!("/mcp/{workspace}/{project}"))
            .header("authorization", format!("Bearer {key}"))
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":
                    {"name":"recall_project","arguments":{"question":"where is gamma_helper defined","structured":true}}})
                .to_string(),
            ))
            .unwrap();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null)
        }
    };
    let quiet = call(workspace.to_string(), agent.clone()).await;
    assert!(
        quiet["result"]["structuredContent"]
            .get("folder_freshness")
            .is_none(),
        "{quiet:#}"
    );
    // Another workspace's key: no refresh (and no answer).
    let (before, _, _) = h.state(&project);
    std::fs::write(h.app.join("a.py"), A_V2).unwrap();
    std::thread::sleep(Duration::from_millis(2200));
    let refused = call(
        s(&other, "id").to_string(),
        s(&other, "capability").to_string(),
    )
    .await;
    assert!(
        refused
            .get("result")
            .is_none_or(|result| result.get("structuredContent").is_none()),
        "{refused:#}"
    );
    assert_eq!(h.state(&project).0, before);
    // The agent's key: the change is synced first and reported.
    let second = call(workspace.to_string(), agent.clone()).await;
    let packet = &second["result"]["structuredContent"];
    assert_eq!(
        packet["folder_freshness"]["status"], "refreshed",
        "{second:#}"
    );
    assert_eq!(packet["folder_freshness"]["changed"], 1);
    assert!(
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|window| window["text"]
                .as_str()
                .is_some_and(|text| text.contains("gamma_helper"))),
        "{}",
        packet["evidence"]
    );
    assert_ne!(h.state(&project).0, before);
}

/// A sync of an unchanged folder converts no document again: an unchanged
/// original keeps the text this build extracted from it, and the result is
/// the same. A changed document and a record of an earlier build convert
/// again. (In this test process documents convert in-process; the head's
/// parser is marked to see whether a sync kept the head or converted anew.)
#[test]
fn an_unchanged_sync_converts_no_document_again() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonical().unwrap().join("docs");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.py"), A_V1).unwrap();
    std::fs::write(
        root.join("guide.html"),
        "<html><body><h1>Guide</h1><p>Alpha runs first.</p></body></html>",
    )
    .unwrap();
    std::fs::write(root.join("diagram.png"), b"\x89PNG\r\n\x1a\nnot an image").unwrap();
    let store = Store::open(dir.path().join("data")).unwrap();
    let mut session = graphicle_runtime::local::session(&store, true).unwrap();
    let sync = |session: &mut graphicle_runtime::local::Session| {
        graphicle_runtime::local::sync(&store, session, &root, None).unwrap()
    };
    let mark = |parser: &str| {
        store
            .transaction(|db| {
                db.execute(
                    "UPDATE am_binary_heads SET parser=? WHERE path='guide.html'",
                    [parser],
                )?;
                Ok(())
            })
            .unwrap();
    };
    let parser = || -> String {
        store
            .read(|db| {
                Ok(db.query_row(
                    "SELECT parser FROM am_binary_heads WHERE path='guide.html'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap()
    };
    let same = |a: &Value, b: &Value| {
        for key in [
            "snapshot",
            "files",
            "binary_documents",
            "original_bytes",
            "bytes",
            "excluded",
            "skipped",
        ] {
            assert_eq!(a[key], b[key], "{key}: {a:#} {b:#}");
        }
    };
    let first = sync(&mut session);
    assert_eq!(first["binary_documents"], 1, "{first:#}");
    assert_eq!(first["skipped"][0]["path"], "diagram.png", "{first:#}");
    assert_eq!(parser(), "html2text");
    // Unchanged: the page's head is kept as it is, and nothing differs.
    mark("kept");
    let second = sync(&mut session);
    assert_eq!(second["replayed"], true, "{second:#}");
    same(&first, &second);
    assert_eq!(parser(), "kept");
    // A record of an earlier build: the page is converted again.
    let project = s(&first, "repository_id").to_string();
    store
        .transaction(|db| {
            db.execute(
                "UPDATE repo_folders SET freshness=json_remove(freshness,'$.converter') WHERE repository_id=?",
                [&project],
            )?;
            Ok(())
        })
        .unwrap();
    let third = sync(&mut session);
    same(&first, &third);
    assert_eq!(parser(), "html2text");
    // A changed page is converted again, and its new text is synced.
    mark("kept");
    std::fs::write(
        root.join("guide.html"),
        "<html><body><h1>Guide</h1><p>Beta runs first.</p></body></html>",
    )
    .unwrap();
    let fourth = sync(&mut session);
    assert_ne!(fourth["snapshot"], first["snapshot"]);
    assert_eq!(parser(), "html2text");
    let text: String = store
        .read(|db| {
            Ok(db.query_row(
                "SELECT o.content FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 WHERE h.repository_id=? AND h.path='guide.html'",
                [&project],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert!(text.contains("Beta runs first."), "{text}");
}

/// Two processes that find the same change sync it once: the second waits
/// for the first's lock, then finds the folder fresh.
#[test]
fn two_processes_sync_one_change_once() {
    let h = Home::new();
    let project = h.sync();
    let (before, syncs, _) = h.state(&project);
    for name in ["a.py", "c.py", "d.py"] {
        std::fs::write(
            h.app.join(name),
            format!("def {}():\n    return 5\n", &name[..1]),
        )
        .unwrap();
    }
    let children: Vec<_> = (0..2)
        .map(|_| {
            crate::fork_guard::spawn(&mut h.command(
                &h.app,
                &["context", "what does run call"],
                &[],
            ))
            .unwrap()
        })
        .collect();
    let packets: Vec<Value> = children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        })
        .collect();
    let refreshed = packets
        .iter()
        .filter(|packet| packet["folder_freshness"]["status"] == "refreshed")
        .count();
    assert!(refreshed <= 1, "{packets:#?}");
    assert!(
        packets
            .iter()
            .all(|packet| packet.get("folder_freshness").is_none_or(|fresh| [
                "refreshed",
                "stale"
            ]
            .contains(&s(fresh, "status")))),
        "{packets:#?}"
    );
    let (after, synced, heads) = h.state(&project);
    assert_ne!(after, before);
    assert_eq!(synced, syncs + 1, "one sync for one change");
    assert_eq!(heads, ["a.py", "b.py", "c.py", "d.py", "notes.md"]);
}

/// A folder the repository ignores lists nothing in `git status`, yet the
/// sync reads its files: the check scans it instead.
#[test]
fn a_folder_the_repository_ignores_is_scanned() {
    let h = Home::new();
    std::fs::write(h.app.join(".gitignore"), "scratch/\n").unwrap();
    h.git(&["add", "-A"]);
    h.git(&["commit", "-q", "-m", "ignore scratch"]);
    let scratch = h.app.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(scratch.join("plan.md"), "# Plan\n\nShip it.\n").unwrap();
    let synced = h.json_in(&scratch, &["sync"], &[]);
    let project = s(&synced, "repository_id").to_string();
    h.quick_sync(&project);
    std::fs::write(
        scratch.join("risks.md"),
        "# Risks\n\nThe canary may fail.\n",
    )
    .unwrap();
    let packet = h.json_in(&scratch, &["context", "canary risks"], &[]);
    assert_eq!(
        packet["folder_freshness"]["status"], "refreshed",
        "{packet:#}"
    );
    assert_eq!(h.state(&project).2, ["plan.md", "risks.md"]);
}

/// Windows: wait until `folder` can be renamed, which it cannot while it is
/// a running process's current folder (a detached build a refresh started).
#[cfg(windows)]
fn wait_until_renamable(folder: &Path) {
    let probe = folder.with_extension("probe");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if std::fs::rename(folder, &probe).is_ok() {
            std::fs::rename(&probe, folder).unwrap();
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{} stayed in use", folder.display());
}

/// A plain folder (no git) is checked by a scan; a project without a folder
/// (created by `call register`, as over HTTP) and a project whose folder is
/// gone are never touched.
#[test]
fn plain_folders_are_scanned_and_projects_without_a_folder_are_left_alone() {
    let h = Home::new();
    let plain = h.home.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("notes.md"), "# Plain\n\nNo repository here.\n").unwrap();
    let synced = h.json_in(&plain, &["sync"], &[]);
    let project = s(&synced, "repository_id").to_string();
    h.quick_sync(&project);
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(plain.join("more.md"), "# More\n\nA second note.\n").unwrap();
    let packet = h.json_in(&plain, &["context", "second note"], &[]);
    assert_eq!(
        packet["folder_freshness"]["status"], "refreshed",
        "{packet:#}"
    );
    assert_eq!(packet["folder_freshness"]["changed"], 1);
    assert!(h.state(&project).2.contains(&"more.md".to_string()));
    let quiet = h.json_in(&plain, &["context", "second note"], &[]);
    assert!(quiet.get("folder_freshness").is_none(), "{quiet:#}");

    // Registered without a folder.
    let arguments = h.home.join("register.json");
    std::fs::write(
        &arguments,
        json!({"project_key":"conversations","name":"Conversations"}).to_string(),
    )
    .unwrap();
    let registered = h.json_in(
        &h.home,
        &[
            "call",
            "register",
            "--arguments",
            arguments.to_str().unwrap(),
        ],
        &[],
    );
    let conversations = s(&registered, "id").to_string();
    let packet = h.json_in(
        &h.home,
        &["context", "anything", "--project", &conversations],
        &[],
    );
    assert!(packet.get("folder_freshness").is_none(), "{packet:#}");

    // The folder moved away: nothing to sync from. Windows refuses to rename
    // a folder that is a running process's current folder (the detached
    // graph build the refresh above started), so wait for it there first.
    #[cfg(windows)]
    wait_until_renamable(&plain);
    let (before, _, _) = h.state(&project);
    std::fs::rename(&plain, h.home.join("moved")).unwrap();
    let packet = h.json_in(
        &h.home,
        &["context", "second note", "--project", &project],
        &[],
    );
    assert!(packet.get("folder_freshness").is_none(), "{packet:#}");
    assert_eq!(h.state(&project).0, before);
}
