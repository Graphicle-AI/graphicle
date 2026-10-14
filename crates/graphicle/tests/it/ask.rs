//! `graphicle ask` answers with a `basic` job under its default
//! contract, grounded-v2; `--workflow reflect` opts into the reflect
//! workflow, and `--retain` (a memory draft) needs it.
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

const RELEASE: &str =
    "# Release\n\nShipping requires a signed release checklist and a green canary.\n";

/// The instructions of every model call, in order.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

/// A `basic` reader quotes its first source; any other call (reflect's
/// controller, the source check) gets text that is not JSON.
async fn respond(State(seen): State<Seen>, Json(request): Json<Value>) -> Json<Value> {
    let instructions = request["instructions"].as_str().unwrap_or_default();
    seen.0.lock().unwrap().push(instructions.to_string());
    let text = if instructions.starts_with("Answer only from these original excerpts") {
        let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
        let source = &input["sources"][0];
        json!({"answer":"A signed checklist and a green canary.","abstain":false,
            "evidence":[{"id":source["id"],"quote":source["text"]}],"unresolved":[]})
        .to_string()
    } else {
        "not json".to_string()
    };
    Json(
        json!({"status":"completed","model":"fixture","usage":{"input_tokens":100,"output_tokens":20},"output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}),
    )
}

fn graphicle(dir: &Path, args: &[&str]) -> Output {
    crate::fork_guard::spawn(
        Command::new(env!("CARGO_BIN_EXE_graphicle"))
            .arg("--data-dir")
            .arg(dir.join("data"))
            .arg("--config")
            .arg(dir.join("models.json"))
            .args(args)
            .env("HOME", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap()
    .wait_with_output()
    .unwrap()
}

fn json_of(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_answers_with_basic_by_default_and_reflect_on_request() {
    let seen = Seen::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(respond))
        .with_state(seen.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("models.json"),
        json!({"default_model":"fixture","models":{"fixture":{"protocol":"responses",
            "base_url":format!("http://{address}/v1"),"model":"fixture","input_limit":16000,"output_limit":2048}}})
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir(root.join("project")).unwrap();
    std::fs::write(root.join("project/release.md"), RELEASE).unwrap();
    // The program blocks; the provider keeps serving on the other workers.
    let run = |args: &[&str]| {
        let root = root.clone();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            graphicle(&root, &args)
        })
    };
    let project = dir.path().join("project");
    // The graph is left for later here, and `ask` builds it
    // before it queues the answer.
    let synced = json_of(
        &run(&["sync", project.to_str().unwrap(), "--no-graph"])
            .await
            .unwrap(),
    );
    assert_eq!(synced["graph"]["status"], "queued", "{synced}");
    let status = json_of(&run(&["status"]).await.unwrap());
    assert_eq!(status["graph"]["graph_source"], "legacy");

    // The default: one `basic` call under grounded-v2.
    let asked = run(&["ask", "What does shipping require?"]).await.unwrap();
    assert!(
        String::from_utf8_lossy(&asked.stderr).contains("graphicle: code graph ready"),
        "{}",
        String::from_utf8_lossy(&asked.stderr)
    );
    let basic = json_of(&asked);
    assert_eq!(basic["kind"], "basic", "{basic}");
    assert_eq!(basic["state"], "complete", "{basic}");
    assert_eq!(basic["result"]["mode"], "basic");
    assert_eq!(basic["result"]["context"]["answer_contract"], "grounded-v2");
    // A project of Markdown alone has an episode but no code symbol: the
    // answer stays source-only.
    let status = json_of(&run(&["status"]).await.unwrap());
    assert_eq!(status["graph"]["graph_source"], "episode");
    assert_eq!(
        basic["result"]["context"]["integration"]["reason"],
        "no_code_symbols"
    );
    assert_eq!(
        basic["result"]["context"]["code_graph"]["status"],
        "disabled"
    );
    {
        let calls = seen.0.lock().unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("The input ends with final_check"));
    }

    // Opt-in reflect: its controller runs first (here its reply is not JSON,
    // so reflect answers through its grounded-v2 fallback). Another
    // question, so the fallback is not the cached answer of the first call.
    let reflect = json_of(
        &run(&[
            "ask",
            "--workflow",
            "reflect",
            "What must a release have before shipping?",
        ])
        .await
        .unwrap(),
    );
    assert_eq!(reflect["kind"], "reflect", "{reflect}");
    assert_eq!(reflect["state"], "complete", "{reflect}");
    assert_eq!(
        reflect["result"]["trace"][0]["fallback"],
        "source_only_answer"
    );
    {
        let calls = seen.0.lock().unwrap();
        assert!(calls[1].starts_with("Answer the user's entire question"));
        assert!(calls[2].contains("The input ends with final_check"));
    }

    // A memory draft needs reflect.
    let refused = run(&["ask", "--retain", "What does shipping require?"])
        .await
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--workflow reflect"));
    // Only the two answering workflows.
    let unknown = run(&["ask", "--workflow", "observer_notes", "Anything?"])
        .await
        .unwrap();
    assert!(!unknown.status.success());

    // `run --wait` builds a waiting graph
    // before it queues its job, as `ask` does.
    std::fs::write(
        root.join("project/release.md"),
        format!("{RELEASE}\nThe canary runs for one hour.\n"),
    )
    .unwrap();
    let synced = json_of(
        &run(&["sync", project.to_str().unwrap(), "--no-graph"])
            .await
            .unwrap(),
    );
    assert_eq!(synced["graph"]["status"], "queued", "{synced}");
    let question = root.join("question.json");
    std::fs::write(
        &question,
        json!({"question":"How long does the canary run?"}).to_string(),
    )
    .unwrap();
    let waited = run(&[
        "run",
        "basic",
        "--arguments",
        question.to_str().unwrap(),
        "--wait",
    ])
    .await
    .unwrap();
    assert!(
        String::from_utf8_lossy(&waited.stderr).contains("graphicle: code graph ready"),
        "{}",
        String::from_utf8_lossy(&waited.stderr)
    );
    let job = json_of(&waited);
    assert_eq!(job["state"], "complete", "{job}");
    let status = json_of(&run(&["status"]).await.unwrap());
    assert_eq!(status["graph"]["episode"], synced["graph"]["episode"]);
    server.abort();
}
