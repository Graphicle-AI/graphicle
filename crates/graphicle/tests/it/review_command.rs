//! `graphicle review` against a mock of the Responses API, on a fixture
//! repository with a two-commit change: the request holds the instructions,
//! a cache-layout pack whose top-ranked file is the changed one, the changed
//! hunks and the diff; a comment on a file the change does not touch is
//! dropped and one whose evidence does not hold is flagged; `--format
//! github` prints a COMMENT review at the head, with paths from the
//! repository's root for a project in a subfolder; a head that is not
//! checked out is refused, and a base equal to the head or a change that
//! only deletes makes no call.
use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

#[derive(Clone, Default)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
    reply: Arc<Mutex<Value>>,
}

async fn responses(State(mock): State<Mock>, Json(request): Json<Value>) -> impl IntoResponse {
    let prompt = format!(
        "{}{}",
        request["instructions"].as_str().unwrap_or(""),
        request["input"].as_str().unwrap_or("")
    );
    mock.requests.lock().unwrap().push(request.clone());
    let text = mock.reply.lock().unwrap().to_string();
    Json(json!({"status":"completed","model":"muse-spark-1.3",
        "usage":{"input_tokens":prompt.len() / 4,"input_tokens_details":{"cached_tokens":0},
            "output_tokens":80,"output_tokens_details":{"reasoning_tokens":20}},
        "output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}))
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

fn graphicle(dir: &Path, cwd: &Path, args: &[String]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    crate::child_env::home(&mut command, dir)
        .current_dir(cwd)
        .arg("--data-dir")
        .arg(dir.join("data"))
        .arg("--config")
        .arg(dir.join("models.json"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    identity(&mut command);
    crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

async fn run(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    let (root, cwd) = (root.to_path_buf(), cwd.to_path_buf());
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || graphicle(&root, &cwd, &args))
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

/// A repository whose project is `folder` inside it (the root when empty):
/// a base commit where Answer returns 41, its caller and its test, plus
/// helpers; then a head commit where it returns 42. The base and head shas.
fn repository(root: &Path, folder: &str) -> (PathBuf, String, String) {
    let top = root.join("repo");
    let project = if folder.is_empty() {
        top.clone()
    } else {
        top.join(folder)
    };
    std::fs::create_dir_all(project.join("src")).unwrap();
    let write = |path: &str, text: &str| std::fs::write(project.join(path), text).unwrap();
    write(
        "src/answer.go",
        "package answer\n\n// Answer is the answer.\nfunc Answer() int {\n\treturn 41\n}\n",
    );
    write(
        "src/use.go",
        "package answer\n\n// Double doubles the answer.\nfunc Double() int {\n\treturn Answer() * 2\n}\n",
    );
    write(
        "src/answer_test.go",
        "package answer\n\nimport \"testing\"\n\nfunc TestAnswer(t *testing.T) {\n\tif Answer() != 41 {\n\t\tt.Fatal(\"wrong\")\n\t}\n}\n",
    );
    for i in 0..12 {
        let mut text = format!("package answer\n\n// Helper{i} helps.\n");
        for f in 0..6 {
            text.push_str(&format!(
                "func Helper{i}x{f}() int {{\n\tvalue := Double() + {f}\n\treturn value * {i}\n}}\n\n"
            ));
        }
        write(&format!("src/helper{i}.go"), &text);
    }
    write("README.md", "# Answer\n\nAnswer returns the answer.\n");
    git(&top, &["init", "-q"]);
    git(&top, &["add", "-A"]);
    git(&top, &["commit", "-q", "-m", "The answer"]);
    let base = git(&top, &["rev-parse", "HEAD"]);
    write(
        "src/answer.go",
        "package answer\n\n// Answer is the answer.\nfunc Answer() int {\n\treturn 42\n}\n",
    );
    git(&top, &["commit", "-q", "-a", "-m", "Make it 42"]);
    let head = git(&top, &["rev-parse", "HEAD"]);
    (project, base, head)
}

fn models(root: &Path, base: &str) {
    let spark = json!({"protocol":"responses","base_url":base,
        "model":"muse-spark-1.3","input_limit":60_000,"output_limit":4_000,"prompt_cache":true,
        "price":{"input":1.25,"output":4.25,"cached_input":0.15}});
    std::fs::write(
        root.join("models.json"),
        json!({"default_model":"spark","models":{"spark":spark}}).to_string(),
    )
    .unwrap();
}

fn reply() -> Value {
    json!({"summary":"Answer now returns 42.","comments":[
        {"path":"src/answer.go","line":5,"severity":"major",
         "body":"TestAnswer still expects 41, so it fails now; update it with the change.",
         "evidence":["src/answer_test.go:6","src/answer.go:5"]},
        {"path":"src/use.go","line":5,"severity":"minor","body":"Double changes too.",
         "evidence":["src/use.go:5"]},
        {"path":"src/answer.go","line":4,"severity":"nit","body":"Say why 42.",
         "evidence":["src/missing.go:3"]}]})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_is_reviewed_and_its_comments_checked() {
    let mock = Mock::default();
    *mock.reply.lock().unwrap() = reply();
    let (url, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &url);
    let (project, base, head) = repository(root, "");
    let synced = run(root, &project, &["sync", project.to_str().unwrap()]).await;
    assert!(
        synced.status.success(),
        "{}",
        String::from_utf8_lossy(&synced.stderr)
    );
    let reviewed = run(
        root,
        &project,
        &[
            "review", "--base", &base, "--format", "json", "--budget", "6000",
        ],
    )
    .await;
    let report = printed(&reviewed);
    assert!(reviewed.status.success(), "{report}");
    assert_eq!(report["result"], "reviewed");
    assert_eq!(report["head"], head.as_str());
    assert_eq!(report["merge_base"], base.as_str());
    assert_eq!(report["files"][0]["path"], "src/answer.go");
    assert_eq!(report["files"][0]["hunks"], json!([[2, 6]]));
    // Kept: on a changed line, with evidence that holds.
    let comments = report["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1, "{report}");
    assert_eq!(comments[0]["path"], "src/answer.go");
    assert_eq!(comments[0]["line"], 5);
    assert_eq!(comments[0]["severity"], "major");
    assert_eq!(comments[0]["evidence"][0]["valid"], true);
    // Flagged: its only citation names no file of the tree.
    assert_eq!(report["flagged"].as_array().unwrap().len(), 1);
    assert_eq!(report["flagged"][0]["line"], 4);
    // Dropped: a file the change does not touch.
    let dropped = report["dropped"].as_array().unwrap();
    assert_eq!(dropped.len(), 1);
    assert!(
        dropped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("src/use.go is not a file the change adds or modifies")
    );
    // The pack: the cache layout, the changed file ranked first.
    assert_eq!(report["pack"]["layout"], "cache");
    let first = report["pack"]["included"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["rank"] == 1)
        .unwrap();
    assert_eq!(first["path"], "src/answer.go");
    // The request: the instructions, the schema, the pack, then the hunks
    // and the diff after it.
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        request["instructions"]
            .as_str()
            .unwrap()
            .starts_with("You review a change")
    );
    assert_eq!(request["text"]["format"]["type"], "json_schema");
    assert!(
        request["prompt_cache_key"]
            .as_str()
            .unwrap()
            .starts_with("graphicle:")
    );
    let input = request["input"].as_str().unwrap();
    let hunks = input.find("Changed hunks of").unwrap();
    assert!(
        input[..hunks].starts_with("# Code pack"),
        "{}",
        &input[..200]
    );
    assert!(input[hunks..].contains("- src/answer.go (modified, +1 -1): 2-6"));
    assert!(input[hunks..].contains("-\treturn 41\n+\treturn 42\n"));
    assert!(input.ends_with("Review the change.\n"));
    drop(requests);
    let log = std::fs::read_to_string(root.join("data/usage.jsonl")).unwrap();
    let entries: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["command"], "review");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_github_format_is_a_comment_review_from_the_repository_root() {
    let mock = Mock::default();
    *mock.reply.lock().unwrap() = reply();
    let (url, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &url);
    let (project, base, head) = repository(root, "services/answer");
    let synced = run(root, &project, &["sync", project.to_str().unwrap()]).await;
    assert!(
        synced.status.success(),
        "{}",
        String::from_utf8_lossy(&synced.stderr)
    );
    let reviewed = run(
        root,
        &project,
        &[
            "review", "--base", &base, "--head", &head, "--format", "github",
        ],
    )
    .await;
    let review = printed(&reviewed);
    assert!(reviewed.status.success(), "{review}");
    assert_eq!(review["event"], "COMMENT");
    assert_eq!(review["commit_id"], head.as_str());
    let comments = review["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1, "{review}");
    assert_eq!(comments[0]["path"], "services/answer/src/answer.go");
    assert_eq!(comments[0]["line"], 5);
    assert_eq!(comments[0]["side"], "RIGHT");
    let body = comments[0]["body"].as_str().unwrap();
    assert!(
        body.starts_with("**major**: TestAnswer still expects 41"),
        "{body}"
    );
    assert!(body.contains("`src/answer_test.go:6`"));
    let summary = review["body"].as_str().unwrap();
    assert!(summary.contains("Answer now returns 42."));
    assert!(summary.contains("1 inline comment(s); 1 withheld"));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_head_not_checked_out_and_an_empty_change_make_no_call() {
    let mock = Mock::default();
    *mock.reply.lock().unwrap() = reply();
    let (url, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &url);
    let (project, base, head) = repository(root, "");
    let synced = run(root, &project, &["sync", project.to_str().unwrap()]).await;
    assert!(
        synced.status.success(),
        "{}",
        String::from_utf8_lossy(&synced.stderr)
    );
    let elsewhere = run(
        root,
        &project,
        &[
            "review", "--base", &base, "--head", &base, "--format", "json",
        ],
    )
    .await;
    assert!(!elsewhere.status.success());
    let message = String::from_utf8_lossy(&elsewhere.stderr).to_string()
        + &String::from_utf8_lossy(&elsewhere.stdout);
    assert!(message.contains("check out"), "{message}");
    let empty = run(
        root,
        &project,
        &["review", "--base", &head, "--format", "json"],
    )
    .await;
    let report = printed(&empty);
    assert!(empty.status.success(), "{report}");
    assert_eq!(report["result"], "no changes");
    // A change that only deletes a file leaves no line to comment on.
    git(&project, &["rm", "-q", "README.md"]);
    git(&project, &["commit", "-q", "-m", "Drop the readme"]);
    let deleted = run(
        root,
        &project,
        &["review", "--base", &head, "--format", "json"],
    )
    .await;
    let report = printed(&deleted);
    assert!(deleted.status.success(), "{report}");
    assert_eq!(report["result"], "nothing to review");
    assert_eq!(report["files"][0]["status"], "deleted");
    assert!(mock.requests.lock().unwrap().is_empty());
    server.abort();
}
