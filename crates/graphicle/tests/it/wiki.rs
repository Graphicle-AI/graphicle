//! `graphicle wiki` against a local stand-in of the Responses API, on a
//! fixture repository with two separate areas of code and a guide that
//! names one of their functions: `build --dry-run` prices the plan and
//! sends nothing; `build` writes one article per area and an index, drops a
//! paragraph whose citation does not hold and logs every call; `update`
//! leaves unchanged articles byte-identical and writes again only the
//! article whose code changed; `check --no-model` lists the stale doc
//! mention; `check` writes a suggested patch and edits no file; a run over
//! `--max-cost` sends nothing; `hook on` and `hook off` add and remove the
//! post-commit block.
use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use graphicle_runtime::store::s;
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
}

/// The first `path:line` the text gives after `marker`.
fn after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let at = text.find(marker)? + marker.len();
    text[at..].split([',', '\n', ' ']).find(|w| !w.is_empty())
}

/// A reply for each kind of request: an article citing the area's first
/// file (and one paragraph citing a file that does not exist), a verdict
/// that the passage is outdated, or suggestions for the gaps.
fn reply_for(request: &Value) -> Value {
    let instructions = request["instructions"].as_str().unwrap_or("");
    let input = request["input"].as_str().unwrap_or("");
    if instructions.starts_with("You write one article") {
        let tail = &input[input.find("# The area: ").unwrap_or(0)..];
        let area = after(tail, "# The area: ").unwrap_or("area");
        let file = after(tail, "): ").unwrap_or("src/none.py");
        return json!({"title":format!("Area {area}"),"summary":format!("What {area} does."),
            "purpose":[{"text":format!("{area} holds this code."),"citations":[format!("{file}:1")]},
                {"text":"A claim with no ground.","citations":["src/nowhere.py:3"]}],
            "key_parts":[],"connections":[],"docs_and_memories":[],"open_questions":["Why two areas?"]});
    }
    if instructions.starts_with("You check one passage") {
        let code = after(input, "Its current code, ")
            .and_then(|c| c.split(':').next())
            .unwrap_or("src/none.py")
            .to_string();
        return json!({"verdict":"outdated","reason":"The parser now lowercases values too.",
            "corrected_text":"Headers go through `parse_header`, which lowercases names and values.",
            "citations":[format!("{code}:1")]});
    }
    json!({"articles":[{"title":"Storage","area":"x","why":"No doc says how saving works."}],
        "questions":["What happens when a save fails?"]})
}

async fn responses(State(mock): State<Mock>, Json(request): Json<Value>) -> impl IntoResponse {
    let prompt = format!(
        "{}{}",
        request["instructions"].as_str().unwrap_or(""),
        request["input"].as_str().unwrap_or("")
    );
    let text = reply_for(&request).to_string();
    mock.requests.lock().unwrap().push(request);
    Json(json!({"status":"completed","model":"muse-spark-1.3",
        "usage":{"input_tokens":prompt.len() / 4,"input_tokens_details":{"cached_tokens":0},
            "output_tokens":200,"output_tokens_details":{"reasoning_tokens":0}},
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

/// The JSON a command printed; the command must succeed.
fn printed(output: &Output) -> Value {
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        output.status.success(),
        "{value}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    value
}

fn git(dir: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    identity(&mut command);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const PARSE: &str = "def parse_header(line):\n    name, _, value = line.partition(\":\")\n    return clean(name), value.strip()\n\n\ndef clean(text):\n    return text.strip().lower()\n\n\ndef parse_all(lines):\n    return [parse_header(line) for line in lines]\n";
const READ: &str = "from wire.parse import parse_all, parse_header\n\n\nclass Reader:\n    def read(self, lines):\n        return parse_all(lines)\n\n    def first(self, lines):\n        return parse_header(lines[0])\n\n    def count(self, lines):\n        return len(self.read(lines))\n";
const SAVE: &str = "import json\n\n\ndef save(path, data):\n    with open(path, \"w\") as f:\n        f.write(encode(data))\n\n\ndef encode(data):\n    return json.dumps(data, sort_keys=True)\n\n\ndef decode(text):\n    return json.loads(text)\n";
const LOAD: &str = "from vault.save import decode, save\n\n\nclass Vault:\n    def load(self, path):\n        with open(path) as f:\n            return decode(f.read())\n\n    def store(self, path, data):\n        save(path, data)\n\n    def copy(self, a, b):\n        self.store(b, self.load(a))\n";
const GUIDE: &str = "# Guide\n\nHeaders go through `parse_header`, which lowercases names.\n\nThe reader is `Reader.read`.\n";

fn repository(root: &Path) -> PathBuf {
    let top = root.join("repo");
    for (path, text) in [
        ("wire/parse.py", PARSE),
        ("wire/read.py", READ),
        ("vault/save.py", SAVE),
        ("vault/load.py", LOAD),
        ("docs/guide.md", GUIDE),
    ] {
        let file = top.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    git(&top, &["init", "-q"]);
    git(&top, &["add", "-A"]);
    git(&top, &["commit", "-q", "-m", "Fixture"]);
    top
}

fn models(root: &Path, base: &str) {
    let spark = json!({"protocol":"responses","base_url":base,
        "model":"muse-spark-1.3","input_limit":60_000,"output_limit":4_000,
        "price":{"input":1.25,"output":4.25,"cached_input":0.15}});
    std::fs::write(
        root.join("models.json"),
        json!({"default_model":"spark","models":{"spark":spark}}).to_string(),
    )
    .unwrap();
}

fn strings(value: &Value) -> Vec<String> {
    let mut out: Vec<String> = value
        .as_array()
        .unwrap_or_else(|| panic!("not a list: {value}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wiki_is_written_kept_current_and_checked() {
    let mock = Mock::default();
    let (url, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &url);
    let project = repository(root);
    let synced = run(root, &project, &["sync", project.to_str().unwrap()]).await;
    assert!(
        synced.status.success(),
        "{}",
        String::from_utf8_lossy(&synced.stderr)
    );
    let calls = || mock.requests.lock().unwrap().len();
    let wiki = project.join(".graphicle/wiki/written");

    // The plan and its worst case; nothing is sent or written.
    let plan = printed(&run(root, &project, &["wiki", "build", "--dry-run", "--json"]).await);
    assert_eq!(plan["result"], "dry_run");
    let areas = plan["to_write"].as_u64().unwrap() as usize;
    assert!(areas >= 2, "{plan}");
    assert!(plan["worst_cost_usd"].as_f64().unwrap() > 0.0, "{plan}");
    assert_eq!(calls(), 0);
    assert!(!wiki.exists());

    // Build: one article per area, the index and the pointer.
    let built = printed(
        &run(
            root,
            &project,
            &["wiki", "build", "--json", "--max-cost", "5"],
        )
        .await,
    );
    assert_eq!(built["result"], "done", "{built}");
    let written = strings(&built["written"]);
    assert_eq!(written.len(), areas, "{built}");
    assert_eq!(calls(), areas);
    // Each article's claim citing a missing file is dropped.
    assert_eq!(built["dropped"]["paragraphs"], areas, "{built}");
    assert!(built["cost_usd"].as_f64().unwrap() > 0.0);
    let index = std::fs::read_to_string(wiki.join("index.md")).unwrap();
    for slug in &written {
        assert!(
            index.contains(&format!("]({slug}.md): What {slug} does.")),
            "{index}"
        );
        let article = std::fs::read_to_string(wiki.join(format!("{slug}.md"))).unwrap();
        assert!(
            article.starts_with("---\ngraphicle_wiki: project-wiki/1\n"),
            "{article}"
        );
        assert!(article.contains("\nmodel: muse-spark-1.3\n"));
        assert!(article.contains("holds this code. (`"), "{article}");
        assert!(!article.contains("nowhere.py"));
        assert!(article.contains("- Why two areas?"));
    }
    assert!(
        std::fs::read_to_string(project.join(".graphicle/wiki/index.md"))
            .unwrap()
            .contains("written/index.md")
    );
    // The request: the instructions, the schema, a pack, then the area.
    {
        let requests = mock.requests.lock().unwrap();
        let input = requests[0]["input"].as_str().unwrap();
        assert!(input.starts_with("# Code pack"), "{}", &input[..200]);
        assert!(input.contains("# The area: "));
        assert_eq!(requests[0]["text"]["format"]["type"], "json_schema");
    }
    // Every call is in the spend log.
    let log = std::fs::read_to_string(root.join("data/usage.jsonl")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|l| l.contains("\"command\":\"wiki\""))
            .count(),
        areas,
        "{log}"
    );
    let snapshot = |slug: &str| std::fs::read(wiki.join(format!("{slug}.md"))).unwrap();
    let before: Vec<Vec<u8>> = written.iter().map(|slug| snapshot(slug)).collect();

    // Update with nothing changed: no call, the same bytes.
    let same = printed(&run(root, &project, &["wiki", "update", "--json"]).await);
    assert_eq!(same["written"], json!([]), "{same}");
    assert_eq!(strings(&same["kept"]), written);
    assert_eq!(calls(), areas);
    for (slug, bytes) in written.iter().zip(&before) {
        assert_eq!(&snapshot(slug), bytes, "{slug} changed");
    }

    // The parser changes: only its article is written again.
    std::fs::write(
        project.join("wire/parse.py"),
        PARSE.replace("value.strip()", "clean(value)"),
    )
    .unwrap();
    let synced = run(root, &project, &["sync", project.to_str().unwrap()]).await;
    assert!(synced.status.success());
    let checked = printed(&run(root, &project, &["wiki", "check", "--no-model", "--json"]).await);
    assert_eq!(checked["stale_mentions"]["count"], 1, "{checked}");
    assert_eq!(
        checked["stale_mentions"]["stale"][0]["mention"],
        "parse_header"
    );
    let changed: Vec<String> = checked["wiki"]["sources_changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["article"].as_str().unwrap().to_string())
        .collect();
    assert!(!changed.is_empty() && changed.len() < areas, "{checked}");
    assert_eq!(checked["wiki"]["broken_citations"], json!([]));
    assert_eq!(checked["model_pass"]["status"], "not_requested");
    assert_eq!(calls(), areas);
    let updated = printed(&run(root, &project, &["wiki", "update", "--json"]).await);
    let mut expected = changed.clone();
    expected.sort();
    assert_eq!(strings(&updated["written"]), expected, "{updated}");
    assert_eq!(calls(), areas + changed.len());
    for (slug, bytes) in written.iter().zip(&before) {
        if !changed.contains(slug) {
            assert_eq!(&snapshot(slug), bytes, "{slug} changed");
        }
    }

    // The model pass: a suggested patch for the guide, which stays as it was.
    let checked = printed(
        &run(
            root,
            &project,
            &["wiki", "check", "--json", "--max-cost", "5"],
        )
        .await,
    );
    let pass = &checked["model_pass"];
    assert_eq!(pass["status"], "done", "{pass}");
    let item = &pass["items"][0];
    assert_eq!(item["kind"], "doc_mention", "{pass}");
    assert_eq!(item["verdict"], "outdated");
    let patch = std::fs::read_to_string(item["patch"].as_str().unwrap()).unwrap();
    assert!(
        patch.starts_with("--- a/docs/guide.md\n+++ b/docs/guide.md\n@@ -1,5 +1,5 @@\n"),
        "{patch}"
    );
    assert!(
        patch
            .contains("\n+Headers go through `parse_header`, which lowercases names and values.\n")
    );
    assert_eq!(
        std::fs::read_to_string(project.join("docs/guide.md")).unwrap(),
        GUIDE
    );

    // A cap below one call's worst case sends nothing.
    let sent = calls();
    let capped = printed(
        &run(
            root,
            &project,
            &[
                "wiki",
                "build",
                "--force",
                "--json",
                "--max-cost",
                "0.000001",
            ],
        )
        .await,
    );
    assert_eq!(capped["result"], "stopped", "{capped}");
    assert_eq!(capped["written"], json!([]));
    assert_eq!(calls(), sent);

    // A wiki kept in the repository is a source of recall, marked as
    // written by a model.
    let kept = printed(
        &run(
            root,
            &project,
            &[
                "wiki",
                "build",
                "--out",
                "wiki-docs",
                "--json",
                "--max-cost",
                "5",
            ],
        )
        .await,
    );
    assert_eq!(kept["result"], "done", "{kept}");
    assert!(project.join("wiki-docs/index.md").exists());
    let synced = run(root, &project, &["sync", project.to_str().unwrap()]).await;
    assert!(synced.status.success());
    let packet = printed(&run(root, &project, &["context", "which area holds this code"]).await);
    assert!(
        packet["wiki_articles"].as_u64().unwrap_or(0) >= 1,
        "{packet:#}"
    );
    assert!(
        s(&packet, "markdown").contains(
            "written by a model (graphicle wiki): read the code it cites before relying on it"
        ),
        "{}",
        s(&packet, "markdown")
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_commit_hook_is_off_until_turned_on() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, "http://127.0.0.1:9/v1");
    let project = repository(root);
    let synced = run(root, &project, &["sync", project.to_str().unwrap()]).await;
    assert!(synced.status.success());
    let hook = project.join(".git/hooks/post-commit");
    let marked = || {
        std::fs::read_to_string(&hook)
            .map(|text| text.contains("# graphicle-wiki-start"))
            .unwrap_or(false)
    };
    assert!(!marked());
    let on = run(
        root,
        &project,
        &["wiki", "hook", "on", "--max-cost", "0.25"],
    )
    .await;
    assert!(
        on.status.success(),
        "{}",
        String::from_utf8_lossy(&on.stderr)
    );
    let text = std::fs::read_to_string(&hook).unwrap();
    assert!(text.contains("wiki update --project"), "{text}");
    assert!(text.contains("--max-cost 0.25 --quiet"), "{text}");
    let off = run(root, &project, &["wiki", "hook", "off"]).await;
    assert!(off.status.success());
    assert!(!marked());
}
