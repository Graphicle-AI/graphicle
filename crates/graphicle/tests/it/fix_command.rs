//! `graphicle fix` against a scripted mock of the Responses API: round 1's
//! edit still fails the test, round 2's passes, and the follow-up's start is
//! cached; an edit that leaves the repository is refused; with no test
//! command the change is made and reported untested; hooks never run, not
//! even ones the model edited; git ignores variables that point it
//! elsewhere; the test command gets no model key; nothing is made before
//! the plan; each round fits the window; a provider's error is reported as
//! one. The person's working tree and branch never change.
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

#[derive(Clone, Default)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
    last: Arc<Mutex<HashMap<String, String>>>,
    /// The edits' replies, in order.
    script: Arc<Mutex<Vec<Value>>>,
    /// Refuse every request for edits, as a provider refuses.
    refuse_edits: Arc<Mutex<bool>>,
    /// Bytes of padding in the plan's summary.
    plan_padding: Arc<Mutex<usize>>,
    /// The context window (bytes over four), and the requests refused for it.
    window: Arc<Mutex<Option<u64>>>,
    over_window: Arc<Mutex<u64>>,
}

async fn responses(State(mock): State<Mock>, Json(request): Json<Value>) -> Response {
    let instructions = request["instructions"].as_str().unwrap_or("");
    let prompt = format!("{instructions}{}", request["input"].as_str().unwrap_or(""));
    let input = prompt.len() as u64 / 4;
    if let Some(window) = *mock.window.lock().unwrap()
        && input > window
    {
        *mock.over_window.lock().unwrap() += 1;
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"input exceeds the context window"}})),
        )
            .into_response();
    }
    if instructions.starts_with("You fix") && *mock.refuse_edits.lock().unwrap() {
        mock.requests.lock().unwrap().push(request.clone());
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"the request was refused by the fixture"}})),
        )
            .into_response();
    }
    let mut cached = 0;
    if let Some(key) = request["prompt_cache_key"].as_str() {
        let mut last = mock.last.lock().unwrap();
        if let Some(previous) = last.get(key) {
            let common = previous
                .bytes()
                .zip(prompt.bytes())
                .take_while(|(a, b)| a == b)
                .count() as u64
                / 4;
            if common >= 1024 {
                cached = common;
            }
        }
        last.insert(key.to_string(), prompt.clone());
    }
    mock.requests.lock().unwrap().push(request.clone());
    let text = if instructions.starts_with("You fix") {
        let mut script = mock.script.lock().unwrap();
        if script.is_empty() {
            json!({"explanation":"Nothing to change.","edits":[],"new_files":[],"tests":[]})
        } else {
            script.remove(0)
        }
    } else {
        let padding = "x".repeat(*mock.plan_padding.lock().unwrap());
        json!({"summary":format!("Return 42 from Answer.{padding}"),"steps":["Change the return value"],
            "edits":[{"path":"src/answer.go","start_line":4,"end_line":6,"reason":"the value"}],
            "new_files":[],"tests":[{"action":"run","path":"check.sh","what":"the check"}],"risks":[]})
    };
    Json(json!({"status":"completed","model":"muse-wonder-1.0",
        "usage":{"input_tokens":input,"input_tokens_details":{"cached_tokens":cached},
            "output_tokens":60,"output_tokens_details":{"reasoning_tokens":30}},
        "output":[{"type":"message","content":[{"type":"output_text","text":text.to_string()}]}]}))
    .into_response()
}

async fn serve(mock: Mock) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .with_state(mock);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}/v1"), server)
}

fn identity(command: &mut Command) -> &mut Command {
    command
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.com")
}

fn graphicle_env(dir: &Path, args: &[String], env: &[(String, String)]) -> Output {
    graphicle_as(dir, args, env, true)
}

/// [`graphicle_env`]; without `with_identity`, git takes the commit identity
/// from its configuration files (the identity variables and EMAIL removed).
fn graphicle_as(
    dir: &Path,
    args: &[String],
    env: &[(String, String)],
    with_identity: bool,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    command.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    crate::child_env::home(&mut command, dir)
        .arg("--data-dir")
        .arg(dir.join("data"))
        .arg("--config")
        .arg(dir.join("models.json"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if with_identity {
        identity(&mut command);
    } else {
        for name in [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "EMAIL",
        ] {
            command.env_remove(name);
        }
    }
    crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

async fn run(root: &Path, args: &[&str]) -> Output {
    run_env(root, args, &[]).await
}

async fn run_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let root = root.to_path_buf();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    tokio::task::spawn_blocking(move || graphicle_env(&root, &args, &env))
        .await
        .unwrap()
}

/// The JSON a command printed, whatever its exit status.
fn printed(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn git(dir: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    identity(&mut command);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A git repository whose check fails: Answer returns 41, the check wants 42.
fn repository(root: &Path) -> PathBuf {
    let project = root.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("src/answer.go"),
        "package answer\n\n// Answer is the answer.\nfunc Answer() int {\n\treturn 41\n}\n",
    )
    .unwrap();
    for i in 0..12 {
        let mut text = format!("package answer\n\n// Helper{i} helps the answer.\n");
        for f in 0..6 {
            text.push_str(&format!(
                "func Helper{i}x{f}() int {{\n\tvalue := Answer() + {f}\n\treturn value * {i}\n}}\n\n"
            ));
        }
        std::fs::write(project.join(format!("src/helper{i}.go")), text).unwrap();
    }
    std::fs::write(
        project.join("check.sh"),
        "grep -q 'return 42' src/answer.go\n",
    )
    .unwrap();
    std::fs::write(
        project.join("README.md"),
        "# Answer\n\nAnswer returns the answer.\n",
    )
    .unwrap();
    git(&project, &["init", "-q"]);
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "The answer"]);
    project
}

fn models(root: &Path, base: &str) {
    models_with(root, base, json!({}));
}

fn models_with(root: &Path, base: &str, extra: Value) {
    let mut wonder = json!({"protocol":"responses","base_url":base,
        "model":"muse-wonder-1.0","input_limit":60_000,"output_limit":4_000,"prompt_cache":true,
        "reasoning_effort":"medium","price":{"input":4.0,"output":20.0,"cached_input":0.2}});
    for (k, v) in extra.as_object().unwrap() {
        wonder[k] = v.clone();
    }
    std::fs::write(
        root.join("models.json"),
        json!({"default_model":"wonder","models":{"wonder":wonder}}).to_string(),
    )
    .unwrap();
}

/// The repository with its hooks in the tree (`core.hooksPath .githooks`, as
/// husky-style setups have them): pre-commit, post-commit and post-checkout,
/// each leaving a marker when it runs.
#[cfg(unix)]
fn hooked_repository(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let project = root.join("project");
    std::fs::create_dir_all(project.join(".githooks")).unwrap();
    for hook in ["pre-commit", "post-commit", "post-checkout"] {
        let path = project.join(".githooks").join(hook);
        let marker = root.join(format!("{hook}-ran"));
        std::fs::write(
            &path,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let project = repository(root);
    git(&project, &["config", "core.hooksPath", ".githooks"]);
    // The fixture's own commit ran pre-commit and post-commit: start clean.
    for hook in ["pre-commit", "post-commit", "post-checkout"] {
        let _ = std::fs::remove_file(root.join(format!("{hook}-ran")));
    }
    project
}

fn edit(path: &str, search: &str, replace: &str) -> Value {
    json!({"explanation":format!("Answer returns the value at {path}:5."),
        "edits":[{"path":path,"search":search,"replace":replace}],"new_files":[],
        "tests":["go test ./..."]})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_round_is_fed_back_and_the_next_one_passes() {
    let mock = Mock::default();
    mock.script.lock().unwrap().extend([
        edit("src/answer.go", "\treturn 41\n", "\treturn 40\n"),
        edit("src/answer.go", "\treturn 40\n", "\treturn 42\n"),
    ]);
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &base);
    let project = repository(root);
    let branch = git(&project, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let synced = run(root, &["sync", project.to_str().unwrap()]).await;
    assert!(synced.status.success());
    let fixed = run(
        root,
        &[
            "fix",
            "The answer should be 42, not 41",
            "--test",
            "sh check.sh",
            "--rounds",
            "3",
            "--json",
        ],
    )
    .await;
    let report = printed(&fixed);
    assert!(fixed.status.success(), "{report}");
    assert_eq!(report["result"], "passed");
    assert_eq!(
        report["branch"],
        "graphicle/fix-the-answer-should-be-42-not-41"
    );
    let rounds = report["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 2, "{report}");
    assert_eq!(rounds[0]["result"], "failed");
    assert_eq!(rounds[0]["test"]["exit_code"], 1);
    assert_eq!(rounds[1]["result"], "passed");
    assert_eq!(report["files_changed"], json!(["src/answer.go"]));
    assert_eq!(report["suggested_tests"], json!(["go test ./..."]));
    assert!(report["committed"].is_null());
    let cited = report["citations"].as_array().unwrap();
    assert!(
        cited
            .iter()
            .any(|c| c["path"] == "src/answer.go" && c["valid"] == true)
    );
    // The change is in the new worktree; the person's tree and branch are as
    // they were.
    let worktree = PathBuf::from(report["worktree"].as_str().unwrap());
    assert!(worktree.starts_with(root.join("data")));
    assert!(
        std::fs::read_to_string(worktree.join("src/answer.go"))
            .unwrap()
            .contains("return 42")
    );
    assert!(
        std::fs::read_to_string(project.join("src/answer.go"))
            .unwrap()
            .contains("return 41")
    );
    assert_eq!(
        git(&project, &["rev-parse", "--abbrev-ref", "HEAD"]),
        branch
    );
    assert_eq!(
        git(&project, &["status", "--porcelain", "--untracked-files=no"]),
        ""
    );
    // Round 2 got round 1's failure and diff, after the same start, which the
    // cache held.
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "a plan and two rounds");
    let (one, two) = (
        requests[1]["input"].as_str().unwrap(),
        requests[2]["input"].as_str().unwrap(),
    );
    assert!(
        two.contains("the test command failed (exit code 1)"),
        "{}",
        &two[two.len() - 600..]
    );
    assert!(two.contains("-\treturn 41") && two.contains("+\treturn 40"));
    let start = one.find("Round 1: return the edits.").unwrap();
    assert_eq!(&two[..start], &one[..start]);
    assert_eq!(
        requests[1]["prompt_cache_key"],
        requests[2]["prompt_cache_key"]
    );
    drop(requests);
    assert!(rounds[1]["usage"]["cached_tokens"].as_u64().unwrap() >= 1024);
    let log: Vec<Value> = std::fs::read_to_string(root.join("data/usage.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(log.len(), 3);
    assert!(log.iter().all(|e| e["command"] == "fix"));
    assert_eq!(log[2]["cached_tokens"], rounds[1]["usage"]["cached_tokens"]);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_that_leaves_the_repository_is_refused() {
    let mock = Mock::default();
    mock.script
        .lock()
        .unwrap()
        .push(json!({"explanation":"Escape.",
        "edits":[{"path":"src/answer.go","search":"\treturn 41\n","replace":"\treturn 42\n"},
            {"path":"../outside.txt","search":"a","replace":"b"}],
        "new_files":[{"path":".git/hooks/pre-commit","content":"#!/bin/sh\n"},
            {"path":"/tmp/graphicle-fix-escape","content":"x"}],"tests":[]}));
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &base);
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let fixed = run(
        root,
        &[
            "fix",
            "Make it 42",
            "--test",
            "sh check.sh",
            "--rounds",
            "1",
            "--json",
        ],
    )
    .await;
    assert!(!fixed.status.success());
    let report = printed(&fixed);
    assert_eq!(report["result"], "not applied");
    let refused: Vec<&str> = report["rounds"][0]["refused"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(
        refused
            .iter()
            .any(|p| p.starts_with("../outside.txt: refused")),
        "{refused:?}"
    );
    assert!(
        refused
            .iter()
            .any(|p| p.starts_with(".git/hooks/pre-commit: refused"))
    );
    assert!(
        refused
            .iter()
            .any(|p| p.starts_with("/tmp/graphicle-fix-escape: refused"))
    );
    // All or none: the good edit was not applied either, and nothing was
    // written outside.
    let worktree = PathBuf::from(report["worktree"].as_str().unwrap());
    assert!(
        std::fs::read_to_string(worktree.join("src/answer.go"))
            .unwrap()
            .contains("return 41")
    );
    assert!(!worktree.parent().unwrap().join("outside.txt").exists());
    assert!(!Path::new("/tmp/graphicle-fix-escape").exists());
    assert_eq!(report["files_changed"], json!([]));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_test_command_the_change_is_made_and_reported_untested() {
    let mock = Mock::default();
    mock.script
        .lock()
        .unwrap()
        .push(edit("src/answer.go", "\treturn 41\n", "\treturn 42\n"));
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &base);
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let fixed = run(
        root,
        &[
            "fix",
            "Make it 42",
            "--rounds",
            "2",
            "--commit",
            "--branch",
            "answer-42",
            "--json",
        ],
    )
    .await;
    let report = printed(&fixed);
    assert!(fixed.status.success(), "{report}");
    assert_eq!(report["result"], "untested");
    assert_eq!(report["tested"], false);
    assert_eq!(report["rounds"].as_array().unwrap().len(), 1);
    assert_eq!(report["branch"], "answer-42");
    let worktree = PathBuf::from(report["worktree"].as_str().unwrap());
    let sha = report["committed"].as_str().unwrap();
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), sha);
    assert_eq!(git(&worktree, &["log", "-1", "--format=%s"]), "Make it 42");
    assert_eq!(git(&project, &["rev-parse", "answer-42"]), sha);
    assert!(
        std::fs::read_to_string(project.join("src/answer.go"))
            .unwrap()
            .contains("return 41")
    );
    server.abort();
}

/// `--focus` puts the passages it names in the pack whatever the issue's
/// words rank: a file they never rank is not in a small pack without it, and
/// is with it; the report says which passages; a bad range is refused. (The
/// standard layout: in the cache layout the passages lead the question part,
/// which a budget this small leaves no room.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn focus_puts_the_located_passages_in_the_pack() {
    let mock = Mock::default();
    for _ in 0..2 {
        mock.script
            .lock()
            .unwrap()
            .push(edit("src/answer.go", "\treturn 41\n", "\treturn 42\n"));
    }
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &base);
    let project = repository(root);
    std::fs::write(
        project.join("src/zebra.go"),
        "package answer\n\n// Zebra paints stripes.\nfunc Zebra() string {\n\treturn \"stripes\"\n}\n",
    )
    .unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "Stripes"]);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let plain = run(
        root,
        &[
            "fix",
            "Make it 42",
            "--budget",
            "600",
            "--layout",
            "standard",
            "--branch",
            "plain",
            "--json",
        ],
    )
    .await;
    assert!(plain.status.success(), "{}", printed(&plain));
    assert!(printed(&plain).get("focus").is_none());
    let focused = run(
        root,
        &[
            "fix",
            "Make it 42",
            "--budget",
            "600",
            "--layout",
            "standard",
            "--branch",
            "focused",
            "--focus",
            "src/zebra.go:4",
            "--json",
        ],
    )
    .await;
    let report = printed(&focused);
    assert!(focused.status.success(), "{report}");
    assert_eq!(
        report["focus"],
        json!([{"path":"src/zebra.go","start_line":1,"end_line":44}])
    );
    let requests = mock.requests.lock().unwrap().clone();
    let plan = |i: usize| requests[i]["input"].as_str().unwrap().to_string();
    assert!(!plan(0).contains("Zebra paints stripes"));
    let focused_plan = requests
        .iter()
        .skip(1)
        .find(|r| {
            !r["instructions"]
                .as_str()
                .unwrap_or("")
                .starts_with("You fix")
        })
        .unwrap();
    assert!(
        focused_plan["input"]
            .as_str()
            .unwrap()
            .contains("Zebra paints stripes")
    );
    let refused = run(
        root,
        &["fix", "Make it 42", "--focus", "src/answer.go:0", "--json"],
    )
    .await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--focus"));
    server.abort();
}

/// `--commit` runs no hook: not the person's own (post-checkout, as the
/// worktree is made), and not the ones the model edited in the round.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_never_run_even_ones_the_model_edited() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mock = Mock::default();
    let edited = |hook: &str| {
        json!({"path":format!(".githooks/{hook}"),"search":"exit 0\n",
            "replace":format!("touch '{}'\nexit 0\n", root.join(format!("{hook}-model-ran")).display())})
    };
    mock.script
        .lock()
        .unwrap()
        .push(json!({"explanation":"See src/answer.go:5.",
        "edits":[{"path":"src/answer.go","search":"\treturn 41\n","replace":"\treturn 42\n"},
            edited("pre-commit"), edited("post-commit")],
        "new_files":[],"tests":[]}));
    let (base, server) = serve(mock.clone()).await;
    models(root, &base);
    let project = hooked_repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let fixed = run(root, &["fix", "Make it 42", "--commit", "--json"]).await;
    let report = printed(&fixed);
    assert!(fixed.status.success(), "{report}");
    assert!(report["committed"].is_string(), "{report}");
    for marker in [
        "pre-commit-ran",
        "post-commit-ran",
        "post-checkout-ran",
        "pre-commit-model-ran",
        "post-commit-model-ran",
    ] {
        assert!(!root.join(marker).exists(), "{marker}");
    }
    server.abort();
}

/// Variables that would point git at another repository are not passed on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_ignores_variables_that_point_it_elsewhere() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mock = Mock::default();
    mock.script
        .lock()
        .unwrap()
        .push(edit("src/answer.go", "\treturn 41\n", "\treturn 42\n"));
    let (base, server) = serve(mock.clone()).await;
    models(root, &base);
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let elsewhere = root.join("elsewhere");
    let elsewhere = elsewhere.to_str().unwrap();
    let fixed = run_env(
        root,
        &["fix", "Make it 42", "--test", "sh check.sh", "--json"],
        &[
            ("GIT_DIR", elsewhere),
            ("GIT_WORK_TREE", elsewhere),
            ("GIT_INDEX_FILE", elsewhere),
        ],
    )
    .await;
    let report = printed(&fixed);
    assert!(fixed.status.success(), "{report}");
    assert_eq!(report["result"], "passed");
    let branch = report["branch"].as_str().unwrap();
    assert_eq!(
        git(&project, &["branch", "--list", branch]).trim_start_matches(['*', '+', ' ']),
        branch
    );
    server.abort();
}

/// META_API_KEY, which Muse Code reads the Meta key from, is one of the
/// variables a test command never gets, and its value is not in the report.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_test_command_gets_no_meta_key() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let secret = "meta-fixture-0123456789abcdef";
    let mock = Mock::default();
    mock.script
        .lock()
        .unwrap()
        .push(edit("src/answer.go", "\treturn 41\n", "\treturn 42\n"));
    let (base, server) = serve(mock.clone()).await;
    models(root, &base);
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let fixed = run_env(
        root,
        &[
            "fix",
            "Make it 42",
            "--test",
            "echo \"meta=[$META_API_KEY]\"; sh check.sh",
            "--rounds",
            "1",
            "--json",
        ],
        &[("META_API_KEY", secret)],
    )
    .await;
    let report = printed(&fixed);
    assert!(fixed.status.success(), "{report}");
    let tail = report["rounds"][0]["test"]["output_tail"].as_str().unwrap();
    assert!(tail.contains("meta=[]"), "{tail}");
    assert!(!report.to_string().contains(secret));
    server.abort();
}

/// The test command runs without the variable the model key is read from,
/// and the key's value never reaches the model or the report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_test_command_gets_no_model_key() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let secret = "sk-fixture-0123456789abcdef";
    let mock = Mock::default();
    mock.script.lock().unwrap().extend([
        edit("src/answer.go", "\treturn 41\n", "\treturn 40\n"),
        edit("src/answer.go", "\treturn 40\n", "\treturn 39\n"),
    ]);
    let (base, server) = serve(mock.clone()).await;
    models_with(root, &base, json!({"key_env":"FIX_TEST_MODEL_KEY"}));
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    // A test that reads the key some other way: from a file outside the
    // repository, and on Linux from graphicle's own environment
    // (/proc/$PPID/environ), printed shifted (a to b) so that the output's
    // redaction would not hide it.
    let stash = root.join("stash.txt");
    std::fs::write(&stash, secret).unwrap();
    let probe = if cfg!(target_os = "linux") {
        "if tr 'a-z' 'b-za' < /proc/$PPID/environ; then echo environ=read; else echo environ=refused; fi; "
    } else {
        ""
    };
    let test = format!(
        "echo \"key=[$FIX_TEST_MODEL_KEY]\"; echo leaked $(cat '{}'); {probe}exit 1",
        stash.display()
    );
    let shifted: String = secret
        .chars()
        .map(|c| match c {
            'a'..='y' => char::from(c as u8 + 1),
            'z' => 'a',
            c => c,
        })
        .collect();
    let fixed = run_env(
        root,
        &[
            "fix",
            "Make it 42",
            "--test",
            &test,
            "--rounds",
            "2",
            "--json",
        ],
        &[("FIX_TEST_MODEL_KEY", secret)],
    )
    .await;
    let report = printed(&fixed);
    assert_eq!(report["result"], "failed", "{report}");
    let tail = report["rounds"][0]["test"]["output_tail"].as_str().unwrap();
    assert!(
        tail.contains("key=[]") && tail.contains("leaked [redacted]"),
        "{tail}"
    );
    if cfg!(target_os = "linux") {
        assert!(tail.contains("environ=refused"), "{tail}");
    }
    let stdout = String::from_utf8_lossy(&fixed.stdout);
    assert!(!stdout.contains(secret) && !stdout.contains(&shifted));
    let requests = mock.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|r| !r.to_string().contains(secret) && !r.to_string().contains(&shifted))
    );
    drop(requests);
    server.abort();
}

/// A profile stored in the workspace (not in the configuration file) has its
/// key variable removed from the test command's environment too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_key_of_a_profile_stored_in_the_workspace_is_removed_too() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let secret = "sk-stored-fedcba9876543210";
    let mock = Mock::default();
    mock.script
        .lock()
        .unwrap()
        .push(edit("src/answer.go", "\treturn 41\n", "\treturn 40\n"));
    let (base, server) = serve(mock.clone()).await;
    models(root, &base);
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    {
        let store = graphicle_runtime::store::Store::open(root.join("data")).unwrap();
        let session = graphicle_runtime::local::session(&store, false).unwrap();
        let profile = json!({"protocol":"responses","base_url":"https://api.example.com/v1",
            "model":"stored-1","key_env":"FIX_STORED_MODEL_KEY","input_limit":60_000,
            "output_limit":4_000});
        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO am_model_profiles VALUES(?,?,?,0,0)",
                rusqlite::params![session.workspace, "stored", profile.to_string()],
            )
            .unwrap();
    }
    let fixed = run_env(
        root,
        &[
            "fix",
            "Make it 42",
            "--test",
            "echo \"stored=[$FIX_STORED_MODEL_KEY]\"; exit 1",
            "--rounds",
            "1",
            "--json",
        ],
        &[("FIX_STORED_MODEL_KEY", secret)],
    )
    .await;
    let report = printed(&fixed);
    assert_eq!(report["result"], "failed", "{report}");
    let tail = report["rounds"][0]["test"]["output_tail"].as_str().unwrap();
    assert!(tail.contains("stored=[]"), "{tail}");
    assert!(!String::from_utf8_lossy(&fixed.stdout).contains(secret));
    let requests = mock.requests.lock().unwrap();
    assert!(requests.iter().all(|r| !r.to_string().contains(secret)));
    drop(requests);
    server.abort();
}

/// The user's own configuration files stay: GIT_CONFIG_GLOBAL, with the
/// commit identity, and GIT_CONFIG_NOSYSTEM (the system file named here
/// cannot be read). The command line's core.hooksPath still wins over the
/// hooks folder they name.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_keeps_the_users_configuration_files_but_not_their_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mock = Mock::default();
    mock.script
        .lock()
        .unwrap()
        .push(edit("src/answer.go", "\treturn 41\n", "\treturn 42\n"));
    let (base, server) = serve(mock.clone()).await;
    models(root, &base);
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let hooks = root.join("global-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let names = ["pre-commit", "commit-msg", "post-commit", "post-checkout"];
    for hook in names {
        let path = hooks.join(hook);
        let marker = root.join(format!("{hook}-ran"));
        std::fs::write(
            &path,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let global = root.join("global.gitconfig");
    std::fs::write(
        &global,
        format!(
            "[user]\n\tname = Global Fixture\n\temail = global@example.com\n[core]\n\thooksPath = {}\n",
            hooks.display()
        ),
    )
    .unwrap();
    let system = root.join("system.gitconfig");
    std::fs::write(&system, "[user\n").unwrap();
    let args: Vec<String> = [
        "fix",
        "Make it 42",
        "--test",
        "sh check.sh",
        "--commit",
        "--json",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    let env: Vec<(String, String)> = [
        ("GIT_CONFIG_GLOBAL", global.display().to_string()),
        ("GIT_CONFIG_SYSTEM", system.display().to_string()),
        ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let home = root.to_path_buf();
    let fixed = tokio::task::spawn_blocking(move || graphicle_as(&home, &args, &env, false))
        .await
        .unwrap();
    let report = printed(&fixed);
    assert!(fixed.status.success(), "{report}");
    assert_eq!(report["result"], "passed", "{report}");
    let commit = report["committed"].as_str().unwrap();
    assert_eq!(
        git(
            &project,
            &["log", "-1", "--format=%an <%ae>|%cn <%ce>", commit]
        ),
        "Global Fixture <global@example.com>|Global Fixture <global@example.com>"
    );
    for hook in names {
        assert!(!root.join(format!("{hook}-ran")).exists(), "{hook}");
    }
    server.abort();
}

/// A cap that refuses the plan leaves no branch and no worktree behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_made_before_the_plan() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mock = Mock::default();
    let (base, server) = serve(mock.clone()).await;
    models_with(root, &base, json!({"max_cost_per_call":0.0001}));
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let fixed = run(root, &["fix", "Make it 42", "--test", "sh check.sh"]).await;
    assert!(!fixed.status.success());
    assert!(String::from_utf8_lossy(&fixed.stderr).contains("models.wonder.max_cost_per_call"));
    assert!(mock.requests.lock().unwrap().is_empty());
    assert_eq!(git(&project, &["branch", "--list", "graphicle/*"]), "");
    let made: Vec<String> = std::fs::read_dir(root.join("data/fix"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name != "no-hooks")
                .collect()
        })
        .unwrap_or_default();
    assert!(made.is_empty(), "{made:?}");
    server.abort();
}

/// What a round adds (here a long plan) cannot take its request over the
/// window: the pack part is made smaller first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_round_is_fitted_to_the_window() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mock = Mock::default();
    *mock.plan_padding.lock().unwrap() = 90_000;
    *mock.window.lock().unwrap() = Some(30_000);
    mock.script
        .lock()
        .unwrap()
        .push(edit("src/answer.go", "\treturn 41\n", "\treturn 42\n"));
    let (base, server) = serve(mock.clone()).await;
    models_with(root, &base, json!({"input_limit":30_000}));
    let project = repository(root);
    for i in 0..60 {
        std::fs::write(
            project.join(format!("src/more{i}.go")),
            format!("package answer\n\n// More{i} adds to the answer.\nfunc More{i}() int {{\n{}\treturn {i}\n}}\n", "\t_ = Answer()\n".repeat(60)),
        )
        .unwrap();
    }
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-q", "-m", "More"]);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let fixed = run(root, &["fix", "Make it 42", "--json"]).await;
    let report = printed(&fixed);
    assert!(fixed.status.success(), "{report}");
    assert_eq!(report["result"], "untested");
    assert_eq!(*mock.over_window.lock().unwrap(), 0);
    let requests = mock.requests.lock().unwrap();
    let length = |r: &Value| r["input"].as_str().unwrap().len();
    // The round's request carries the long plan, yet its pack part shrank.
    let (plan, round) = (length(&requests[0]), length(&requests[1]));
    assert!(round < plan + 90_000, "plan {plan}, round {round}");
    drop(requests);
    server.abort();
}

/// A provider's refusal of a round is reported as the provider's error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_error_in_a_round_is_reported_as_one() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mock = Mock::default();
    *mock.refuse_edits.lock().unwrap() = true;
    let (base, server) = serve(mock.clone()).await;
    models(root, &base);
    let project = repository(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    let fixed = run(
        root,
        &["fix", "Make it 42", "--test", "sh check.sh", "--json"],
    )
    .await;
    assert!(!fixed.status.success());
    let report = printed(&fixed);
    assert_eq!(report["result"], "provider error", "{report}");
    let error = report["rounds"][0]["error"].as_str().unwrap();
    assert!(error.contains("refused by the fixture"), "{error}");
    assert!(String::from_utf8_lossy(&fixed.stderr).contains("refused by the fixture"));
    assert_eq!(report["rounds"].as_array().unwrap().len(), 1);
    server.abort();
}

/// --timeout takes 1 to 86400 seconds, as the configuration does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_timeout_is_checked() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for bad in ["0", "86401"] {
        let refused = run(root, &["fix", "Make it 42", "--timeout", bad]).await;
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("--timeout"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
}
