//! `graphicle see` against a scripted mock of the Responses API's computer
//! tool and a fake driver (a shell script that logs each request and answers
//! with a 1x1 PNG): the request has the documented shape (the computer tool,
//! no stored state, encrypted reasoning asked for, each step's reasoning and
//! call replayed exactly as returned, each output with its call's id and a
//! screenshot); a run ends with the model's answer and
//! saves its screenshots and step log; every call goes in the spend log as
//! `see`; a pending safety check stops the run unless acknowledged, and an
//! acknowledged one goes back on the next request; the run's cap and the
//! step cap stop a run before the call that would pass them; a profile
//! without a price is refused.
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[derive(Clone, Default)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
    /// The output items of each step's reply, in order; the last repeats.
    script: Arc<Mutex<Vec<Value>>>,
    /// What the mock found wrong in a request.
    problems: Arc<Mutex<Vec<String>>>,
}

async fn responses(
    State(mock): State<Mock>,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    mock.requests.lock().unwrap().push(request.clone());
    let input = request["input"].as_array().cloned().unwrap_or_default();
    let step = input
        .iter()
        .filter(|i| i["type"] == "computer_call_output")
        .count();
    let script = mock.script.lock().unwrap().clone();
    let mut problems = Vec::new();
    if request["tools"] != json!([{"type":"computer"}])
        || request["store"] != false
        || request["include"] != json!(["reasoning.encrypted_content"])
    {
        problems.push("not the computer tool without stored state".to_string());
    }
    // Each step's reasoning and call come back as the mock returned them, in
    // that order, and each output answers its call with a screenshot and its
    // acknowledged checks.
    for (index, group) in input[1..].chunks(3).enumerate() {
        let returned = &script[index.min(script.len() - 1)];
        if group[0] != reasoning(index) {
            problems.push(format!(
                "the reasoning of step {index} was not replayed unchanged"
            ));
        }
        if group.get(1) != Some(returned) {
            problems.push(format!("call {index} was not replayed unchanged"));
        }
        let output = group.get(2).cloned().unwrap_or(Value::Null);
        if output["call_id"] != returned["call_id"]
            || !output["output"]["image_url"]
                .as_str()
                .unwrap_or("")
                .starts_with("data:image/png;base64,")
        {
            problems.push(format!("output {index} does not answer its call"));
        }
        let pending = returned["pending_safety_checks"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if output["acknowledged_safety_checks"] != json!(pending) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"message":"pending safety checks were not acknowledged"}})),
            );
        }
    }
    mock.problems.lock().unwrap().extend(problems);
    let item = script[step.min(script.len() - 1)].clone();
    let output = if item["type"] == "computer_call" {
        json!([reasoning(step), item])
    } else {
        json!([item])
    };
    (
        StatusCode::OK,
        Json(
            json!({"id":format!("resp_{step}"),"object":"response","status":"completed",
            "model":"muse-wonder-1.0","output":output,
            "usage":{"input_tokens":2000 + 1600 * step,"output_tokens":100,
                "input_tokens_details":{"cached_tokens":1600 * step},
                "output_tokens_details":{"reasoning_tokens":20}}}),
        ),
    )
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

fn call(id: usize, actions: Value, checks: Value) -> Value {
    json!({"type":"computer_call","id":format!("cc_{id}"),"call_id":format!("call_{id}"),
        "actions":actions,"pending_safety_checks":checks,"status":"completed",
        "meta_safety_replay_receipt":format!("receipt-{id}")})
}

/// The reasoning item the mock returns before step `step`'s call.
fn reasoning(step: usize) -> Value {
    json!({"type":"reasoning","id":format!("rs_{step}"),
        "summary":[{"type":"summary_text","text":"The menu is open; click the canvas."}],
        "encrypted_content":format!("encrypted-{step}")})
}

fn answer(shown: &str) -> Value {
    let text = json!({"shown":shown,"point":{"x":0,"y":0},"summary":"The menu stays open."});
    json!({"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":text.to_string()}]})
}

fn setup(base: &str, price: bool) -> TempDir {
    let root = TempDir::new().unwrap();
    let mut wonder = json!({"protocol":"responses","base_url":base,"model":"muse-wonder-1.0",
        "input_limit":1_000_000,"output_limit":16_000});
    if price {
        wonder["price"] = json!({"input":4.0,"cached_input":0.2,"output":20.0});
    }
    std::fs::write(
        root.path().join("models.json"),
        json!({"default_model":"wonder","models":{"wonder":wonder}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("driver.sh"),
        format!(
            "printf 'meta=[%s]\\n' \"$META_API_KEY\" > \"$1.env\"\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> \"$1\"\n  case \"$line\" in *'\"op\":\"close\"'*) exit 0;; esac\n  printf '{{\"ok\":true,\"done\":1,\"error\":null,\"url\":\"http://app.test/\",\"blocked\":0,\"png\":\"%s\"}}\\n' '{PNG}'\ndone\n"
        ),
    )
    .unwrap();
    root
}

fn see(root: &Path, extra: &[&str]) -> Output {
    let driver = format!(
        "sh '{}' '{}'",
        root.join("driver.sh").display(),
        root.join("driver-requests.jsonl").display()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    crate::child_env::home(&mut command, root)
        .arg("--data-dir")
        .arg(root.join("data"))
        .arg("--config")
        .arg(root.join("models.json"))
        .args([
            "see",
            "Open the main menu, then click the canvas: the menu should close.",
            "--driver",
            &driver,
            "--out",
        ])
        .arg(root.join("out"))
        .arg("--json")
        .args(extra)
        .env("META_API_KEY", "meta-fixture-0123456789abcdef")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

async fn run(root: &Path, extra: &[&str]) -> Output {
    let root: PathBuf = root.to_path_buf();
    let extra: Vec<String> = extra.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
        see(&root, &extra)
    })
    .await
    .unwrap()
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn spend_log(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("data/usage.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn driver_requests(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("driver-requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn a_run_reproduces_the_bug_replays_each_call_and_logs_each_step() {
    let mock = Mock::default();
    *mock.script.lock().unwrap() = vec![
        call(
            1,
            json!([{"type":"click","button":"left","x":20,"y":20}]),
            json!([]),
        ),
        call(
            2,
            json!([{"type":"click","button":"left","x":600,"y":400},{"type":"wait"}]),
            json!([]),
        ),
        answer("bug"),
    ];
    let (base, server) = serve(mock.clone()).await;
    let root = setup(&base, true);
    let output = run(root.path(), &[]).await;
    let report = report(&output);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["result"], "reproduced");
    assert_eq!(report["shown"], "bug");
    assert_eq!(report["point"], json!({"x":0,"y":0}));
    assert_eq!(report["calls"], 3);
    assert_eq!(report["steps"].as_array().unwrap().len(), 3);
    assert_eq!(report["last_screenshot"], "step-02.png");
    assert!(
        mock.problems.lock().unwrap().is_empty(),
        "{:?}",
        mock.problems
    );
    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    // The first message, then reasoning, call and output for each step.
    assert_eq!(requests[2]["input"].as_array().unwrap().len(), 7);
    assert_eq!(requests[2]["input"][4], reasoning(1));
    assert_eq!(
        requests[2]["input"][5]["meta_safety_replay_receipt"],
        "receipt-2"
    );
    // The driver ran each batch, then was asked to close.
    let asked = driver_requests(root.path());
    assert_eq!(asked[0], json!({"op":"screenshot"}));
    assert_eq!(asked[1]["actions"][0]["x"], 20);
    assert_eq!(asked[2]["actions"][1]["type"], "wait");
    assert_eq!(asked.last().unwrap(), &json!({"op":"close"}));
    // The driver never gets a model key, META_API_KEY among them.
    let env = std::fs::read_to_string(root.path().join("driver-requests.jsonl.env")).unwrap();
    assert_eq!(env.trim(), "meta=[]");
    let out = root.path().join("out");
    for file in [
        "step-00.png",
        "step-01.png",
        "step-02.png",
        "steps.jsonl",
        "see.json",
        "driver.log",
    ] {
        assert!(out.join(file).exists(), "{file}");
    }
    let log = spend_log(root.path());
    assert_eq!(log.len(), 3);
    for (index, entry) in log.iter().enumerate() {
        assert_eq!(entry["command"], "see");
        assert_eq!(entry["step"], index as u64 + 1);
        assert_eq!(entry["model"], "muse-wonder-1.0");
        assert!(entry["cost_usd"].as_f64().unwrap() > 0.0);
    }
    assert_eq!(log[2]["cached_tokens"], 3200);
    // Step 3: 5,200 input tokens of which 3,200 cached, and 100 output.
    let expected = (2000.0 * 4.0 + 3200.0 * 0.2 + 100.0 * 20.0) / 1e6;
    assert!((log[2]["cost_usd"].as_f64().unwrap() - expected).abs() < 1e-9);
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn a_safety_check_stops_the_run_unless_acknowledged_and_then_goes_back() {
    let check = json!([{"id":"csc_1","code":"malicious_instructions","message":"Potentially unsafe action detected."}]);
    let mock = Mock::default();
    *mock.script.lock().unwrap() = vec![
        call(1, json!([{"type":"type","text":"hello"}]), check.clone()),
        answer("bug"),
    ];
    let (base, server) = serve(mock.clone()).await;
    let root = setup(&base, true);
    let output = run(root.path(), &[]).await;
    assert!(!output.status.success());
    let stopped = report(&output);
    assert_eq!(stopped["result"], "stopped at a safety check");
    assert_eq!(
        driver_requests(root.path()).len(),
        2,
        "a screenshot, then close: no action ran"
    );

    let root = setup(&base, true);
    mock.requests.lock().unwrap().clear();
    let output = run(root.path(), &["--acknowledge-safety-checks"]).await;
    let done = report(&output);
    assert!(output.status.success(), "{done}");
    assert_eq!(
        done["acknowledged"][0]["check"]["code"],
        "malicious_instructions"
    );
    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests[1]["input"][3]["acknowledged_safety_checks"], check);
    assert!(String::from_utf8_lossy(&output.stderr).contains("acknowledged the safety check"));
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn the_caps_and_a_missing_price_stop_a_run_before_its_call() {
    let mock = Mock::default();
    *mock.script.lock().unwrap() = vec![call(
        1,
        json!([{"type":"click","button":"left","x":1,"y":1}]),
        json!([]),
    )];
    let (base, server) = serve(mock.clone()).await;
    // The run's cap is under one step's worst case: nothing is sent.
    let root = setup(&base, true);
    let output = run(root.path(), &["--max-cost", "0.0001"]).await;
    assert!(!output.status.success());
    assert_eq!(report(&output)["result"], "refused by a cap");
    assert!(mock.requests.lock().unwrap().is_empty());
    // The model keeps acting: two steps, then the step cap.
    let root = setup(&base, true);
    let output = run(root.path(), &["--steps", "2"]).await;
    assert!(!output.status.success());
    let capped = report(&output);
    assert_eq!(capped["result"], "step cap reached");
    assert_eq!(capped["calls"], 2);
    assert_eq!(spend_log(root.path()).len(), 2);
    // No price: refused before the driver starts.
    let root = setup(&base, false);
    let output = run(root.path(), &[]).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("price"));
    assert!(driver_requests(root.path()).is_empty());
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn a_check_run_passes_on_the_expected_screen_and_fails_on_the_bug() {
    let mock = Mock::default();
    *mock.script.lock().unwrap() = vec![answer("expected")];
    let (base, server) = serve(mock.clone()).await;
    let root = setup(&base, true);
    let output = run(root.path(), &["--look-for", "expected"]).await;
    assert!(output.status.success());
    assert_eq!(report(&output)["result"], "expected shown");
    *mock.script.lock().unwrap() = vec![answer("bug")];
    let root = setup(&base, true);
    let output = run(root.path(), &["--look-for", "expected"]).await;
    assert!(!output.status.success());
    assert_eq!(report(&output)["result"], "bug still shown");
    server.abort();
}
