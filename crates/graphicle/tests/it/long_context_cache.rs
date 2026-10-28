//! `ask --pack` and `plan` against a mock of the Meta Model API's Responses
//! endpoints that behaves like its prompt cache: the cache key, retention,
//! reasoning effort and output cap are sent; a follow-up question on the same
//! repository reports cached tokens and the spend log records them; each cap
//! refuses before anything is sent; `graphicle usage` adds the log up; and the
//! provider's exact count shrinks a pack that would not fit.
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

/// What the mock saw, and the last prompt of each cache key.
#[derive(Clone, Default)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
    counts: Arc<Mutex<Vec<Value>>>,
    last: Arc<Mutex<HashMap<String, String>>>,
    /// Bytes per token of the `input_tokens` endpoint's count (0: four).
    count_rate: Arc<Mutex<f64>>,
    /// Bytes per token of the input usage a response reports (0: four).
    usage_rate: Arc<Mutex<f64>>,
}

/// The prompt as the provider sees it: the instructions, then the input.
fn prompt_of(request: &Value) -> String {
    format!(
        "{}{}",
        request["instructions"].as_str().unwrap_or(""),
        request["input"].as_str().unwrap_or("")
    )
}

/// A completed response whose usage looks real: input tokens are the
/// prompt's bytes over the usage rate (four unless set); cached tokens are
/// the start the prompt shares with the last one of its cache key, counted
/// from 1,024 tokens up.
async fn responses(State(mock): State<Mock>, Json(request): Json<Value>) -> Json<Value> {
    let prompt = prompt_of(&request);
    let rate = match *mock.usage_rate.lock().unwrap() {
        r if r > 0.0 => r,
        _ => 4.0,
    };
    let input = (prompt.len() as f64 / rate) as u64;
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
    let text = if request["text"]["format"]["type"] == "json_schema" {
        json!({"summary":"Start the canary first.","steps":["Edit the canary"],
            "edits":[{"path":"pkg/release/file0.go","start_line":1,"end_line":2,"reason":"the start"}],
            "new_files":[],"tests":[],"risks":[]})
        .to_string()
    } else {
        "The canary starts in pkg/release/file0.go:3.".to_string()
    };
    Json(json!({"status":"completed","model":request["model"],
        "usage":{"input_tokens":input,"input_tokens_details":{"cached_tokens":cached},
            "output_tokens":40,"output_tokens_details":{"reasoning_tokens":25},"total_tokens":input + 40},
        "output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}))
}

/// `POST /v1/responses/input_tokens`: the prompt's bytes over the count rate.
async fn input_tokens(State(mock): State<Mock>, Json(request): Json<Value>) -> Json<Value> {
    let rate = match *mock.count_rate.lock().unwrap() {
        r if r > 0.0 => r,
        _ => 4.0,
    };
    mock.counts.lock().unwrap().push(request.clone());
    let tokens = (prompt_of(&request).len() as f64 / rate).ceil() as u64;
    Json(json!({"object":"response.input_tokens","input_tokens":tokens}))
}

async fn serve(mock: Mock) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .route("/v1/responses/input_tokens", post(input_tokens))
        .with_state(mock);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}/v1"), server)
}

fn graphicle(dir: &Path, args: &[String]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    crate::child_env::home(&mut command, dir)
        .arg("--data-dir")
        .arg(dir.join("data"))
        .arg("--config")
        .arg(dir.join("models.json"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

/// The program blocks; the provider keeps serving on the other workers.
async fn run(root: &Path, args: &[&str]) -> Output {
    let root = root.to_path_buf();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || graphicle(&root, &args))
        .await
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

/// A Go project of `n` files in four packages, with a README.
fn project(root: &Path, n: usize) -> PathBuf {
    let project = root.join("project");
    for i in 0..n {
        let folder = ["pkg/release", "pkg/canary", "cmd/ship", "internal/check"][i % 4];
        std::fs::create_dir_all(project.join(folder)).unwrap();
        let mut text = String::from("package x\n\n");
        for f in 0..6 {
            text.push_str(&format!(
                "// Step{i}x{f} runs step {f} of release stage {i}.\nfunc Step{i}x{f}() {{\n"
            ));
            for k in 0..8 {
                text.push_str(&format!(
                    "\tvalue{k} := check(\"stage {i} canary {f} gate {k}\")\n"
                ));
            }
            if f > 0 {
                text.push_str(&format!("\tStep{i}x{}()\n", f - 1));
            }
            text.push_str("}\n\n");
        }
        std::fs::write(project.join(format!("{folder}/file{i}.go")), text).unwrap();
    }
    std::fs::write(
        project.join("README.md"),
        "# Ship\n\nShip releases behind a canary and a signed checklist.\n",
    )
    .unwrap();
    project
}

fn models(root: &Path, base: &str, extra: Value) {
    let mut meta = json!({"protocol":"responses","base_url":base,"model":"muse-spark-1.3",
        "input_limit":60_000,"output_limit":2_048,"prompt_cache":true,"reasoning_effort":"low",
        "price":{"input":1.25,"output":4.25,"cached_input":0.15}});
    for (k, v) in extra.as_object().unwrap() {
        meta[k] = v.clone();
    }
    std::fs::write(
        root.join("models.json"),
        json!({"default_model":"meta","models":{"meta":meta}}).to_string(),
    )
    .unwrap();
}

fn log_of(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("data/usage.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_up_questions_hit_the_cache_and_the_spend_log_records_them() {
    let mock = Mock::default();
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &base, json!({}));
    let project = project(root, 24);
    json_of(&run(root, &["sync", project.to_str().unwrap()]).await);

    let ask = |question: &'static str| {
        let root = root.to_path_buf();
        async move {
            json_of(
                &run(
                    &root,
                    &[
                        "ask", "--pack", "--layout", "cache", "--budget", "20000", "--json",
                        question,
                    ],
                )
                .await,
            )
        }
    };
    let first = ask("How does the canary start?").await;
    let second = ask("Where is the signed checklist checked?").await;
    let key = first["prompt_cache_key"].as_str().unwrap().to_string();
    assert!(key.starts_with("graphicle:project@"), "{key}");
    assert_eq!(second["prompt_cache_key"], key.as_str());
    assert!(!key.contains("canary") && !key.contains("checklist"));
    assert_eq!(first["pack"]["layout"], "cache");
    assert_eq!(
        first["pack"]["prefix_sha256"],
        second["pack"]["prefix_sha256"]
    );
    assert!(key.ends_with(&first["pack"]["prefix_sha256"].as_str().unwrap()[..12]));
    // The first question warms the cache; the follow-up reads the shared
    // instructions and repository part from it.
    assert_eq!(first["usage"]["cached_tokens"], 0);
    let cached = second["usage"]["cached_tokens"].as_u64().unwrap();
    let prefix = first["pack"]["prefix_tokens"].as_u64().unwrap();
    assert!(cached >= 1024, "{second}");
    assert!(
        cached * 10 >= prefix * 8,
        "{cached} of a {prefix}-token prefix"
    );
    {
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(request["prompt_cache_key"], key.as_str());
            assert_eq!(request["prompt_cache_retention"], "24h");
            assert_eq!(request["reasoning"]["effort"], "low");
            assert_eq!(request["max_output_tokens"], 2_048);
            assert_eq!(request["instructions"], requests[0]["instructions"]);
            // The question comes after the pack.
            assert!(request["input"].as_str().unwrap().trim_end().ends_with('?'));
        }
    }
    // The spend log: one line per call, the cached tokens and the cost at
    // the configured price, never the prompt or the answer.
    let log = log_of(root);
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1]["cached_tokens"], cached);
    assert_eq!(log[1]["command"], "ask --pack");
    assert_eq!(log[1]["profile"], "meta");
    assert_eq!(log[1]["model"], "muse-spark-1.3");
    assert_eq!(log[1]["prompt_cache_key"], key.as_str());
    assert_eq!(log[1]["status"], "ok");
    let raw = std::fs::read_to_string(root.join("data/usage.jsonl")).unwrap();
    assert!(!raw.contains("canary") && !raw.contains("Ship releases"));
    let input = log[1]["input_tokens"].as_u64().unwrap() as f64;
    let expected =
        ((input - cached as f64) * 1.25 + cached as f64 * 0.15 + 40.0 * 4.25) / 1_000_000.0;
    assert!((log[1]["cost_usd"].as_f64().unwrap() - expected).abs() < 1e-6);
    assert_eq!(second["usage"]["cost_usd"], log[1]["cost_usd"]);

    // A plan on the same repository: its own call, logged too.
    let planned = json_of(
        &run(
            root,
            &[
                "plan",
                "--layout",
                "cache",
                "--budget",
                "20000",
                "--json",
                "Start the canary before the checklist",
            ],
        )
        .await,
    );
    assert_eq!(planned["prompt_cache_key"], key.as_str());
    assert_eq!(log_of(root).len(), 3);

    // The report adds the log up.
    let report = json_of(&run(root, &["usage", "--json"]).await);
    let log = log_of(root);
    let sum = |field: &str| log.iter().map(|e| e[field].as_u64().unwrap()).sum::<u64>();
    assert_eq!(report["total"]["calls"], 3);
    assert_eq!(report["total"]["input_tokens"], sum("input_tokens"));
    assert_eq!(report["total"]["cached_tokens"], sum("cached_tokens"));
    assert_eq!(report["total"]["output_tokens"], sum("output_tokens"));
    let cost: f64 = log.iter().map(|e| e["cost_usd"].as_f64().unwrap()).sum();
    assert!((report["total"]["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-6);
    assert_eq!(report["days"][0]["model"], "muse-spark-1.3");
    let today = json_of(&run(root, &["usage", "--json", "--since", "2999-01-01"]).await);
    assert_eq!(today["total"]["calls"], 0);
    let text = run(root, &["usage"]).await;
    assert!(text.status.success());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_cap_refuses_before_anything_is_sent() {
    let mock = Mock::default();
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let project = project(root, 8);
    models(root, &base, json!({}));
    json_of(&run(root, &["sync", project.to_str().unwrap()]).await);
    let ask = [
        "ask",
        "--pack",
        "--budget",
        "8000",
        "--json",
        "How does the canary start?",
    ];

    // A call whose worst case is over max_cost_per_call.
    models(root, &base, json!({"max_cost_per_call":0.001}));
    let refused = run(root, &ask).await;
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("max_cost_per_call"), "{message}");
    assert!(
        message.contains("models.meta.max_cost_per_call"),
        "{message}"
    );
    assert!(mock.requests.lock().unwrap().is_empty());
    assert!(log_of(root).is_empty());

    // A day cap with room: the call goes out, and the log holds its cost
    // and its worst case.
    models(root, &base, json!({"max_cost_per_day":10.0}));
    json_of(&run(root, &ask).await);
    let logged = &log_of(root)[0];
    let (cost, worst) = (
        logged["cost_usd"].as_f64().unwrap(),
        logged["worst_cost_usd"].as_f64().unwrap(),
    );
    assert!(cost > 0.0 && worst > cost, "{logged}");
    // Today's spend with this call's worst case would pass the day cap: the
    // call is refused, though the logged total alone is under the cap.
    models(root, &base, json!({"max_cost_per_day":cost + worst / 2.0}));
    let refused = run(root, &ask).await;
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("models.meta.max_cost_per_day"),
        "{message}"
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    // A spend log that cannot be read refuses capped calls.
    std::fs::rename(root.join("data/usage.jsonl"), root.join("data/usage.saved")).unwrap();
    std::fs::create_dir(root.join("data/usage.jsonl")).unwrap();
    models(root, &base, json!({"max_cost_per_day":10.0}));
    let refused = run(root, &ask).await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Cannot read the spend log"));
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    std::fs::remove_dir(root.join("data/usage.jsonl")).unwrap();
    std::fs::rename(root.join("data/usage.saved"), root.join("data/usage.jsonl")).unwrap();

    // A cap needs the price it is counted at.
    models(root, &base, json!({"max_cost_per_day":1.0,"price":null}));
    let invalid = run(root, &ask).await;
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("models.meta.price"));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_providers_exact_count_shrinks_a_pack_that_would_not_fit() {
    let mock = Mock::default();
    // The provider counts twice the tokens of the estimate's bytes.
    *mock.count_rate.lock().unwrap() = 1.75;
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let project = project(root, 160);
    models(
        root,
        &base,
        json!({"input_limit":40_000,"count_tokens":true}),
    );
    json_of(&run(root, &["sync", project.to_str().unwrap()]).await);
    // The cache layout fills its repository part whatever the question.
    let asked = json_of(
        &run(
            root,
            &[
                "ask",
                "--pack",
                "--layout",
                "cache",
                "--json",
                "How does the canary start?",
            ],
        )
        .await,
    );
    let counted = asked["input_tokens_counted"].as_u64().unwrap_or_else(|| {
        panic!(
            "not counted: {} counts, pack {}",
            mock.counts.lock().unwrap().len(),
            asked["pack"]
        )
    });
    assert!(counted <= 40_000, "{asked}");
    let budget = 40_000 - 4_000;
    assert!(
        asked["pack"]["budget"].as_u64().unwrap() < budget,
        "{asked}"
    );
    {
        let counts = mock.counts.lock().unwrap();
        assert!(counts.len() >= 2, "{}", counts.len());
        assert_eq!(counts[0]["model"], "muse-spark-1.3");
        assert!(counts[0].get("max_output_tokens").is_none());
    }
    // The call sent is the one counted last.
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let sent = (prompt_of(&requests[0]).len() as f64 / 1.75).ceil() as u64;
    assert_eq!(sent, counted);
    drop(requests);
    server.abort();
}

/// Without --budget, the budget moves with the calibration after each call;
/// the cache layout's repository part, sized from the budget before
/// calibration, stays the same bytes, and so does the cache key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repository_part_holds_while_the_calibration_moves_the_budget() {
    let mock = Mock::default();
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &base, json!({}));
    let project = project(root, 160);
    json_of(&run(root, &["sync", project.to_str().unwrap()]).await);
    let mut seen = Vec::new();
    for question in [
        "How does the canary start?",
        "Where is the signed checklist checked?",
        "Which stage runs the gate checks?",
    ] {
        let asked = json_of(
            &run(
                root,
                &["ask", "--pack", "--layout", "cache", "--json", question],
            )
            .await,
        );
        seen.push((
            asked["pack"]["budget"].as_u64().unwrap(),
            asked["pack"]["prefix_sha256"].as_str().unwrap().to_string(),
            asked["prompt_cache_key"].as_str().unwrap().to_string(),
        ));
    }
    let calibration = std::fs::read_to_string(root.join("data/pack-calibration.json")).unwrap();
    assert!(calibration.contains("\"calls\": 3"), "{calibration}");
    // The budget moved with the calibration...
    assert_ne!(seen[0].0, seen[1].0, "{seen:?}");
    // ...and the repository part and the cache key did not.
    assert!(
        seen.windows(2)
            .all(|w| w[0].1 == w[1].1 && w[0].2 == w[1].2),
        "{seen:?}"
    );
    server.abort();
}

/// A provider that counts more than Graphicle's estimate (a calibration
/// above 1) puts the calibrated budget below the repository part's share of
/// the fixed budget. The part then steps down to the first size that fits
/// (0.8 of it here) and holds there while the calibration moves within the
/// step, from the first calibrated call on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repository_part_holds_when_the_provider_counts_more() {
    // (the prefix share, the usage rates of the four calls, and the 0.8
    // step's bounds on the calibration: 0.97 / share, 0.97 / (0.8 * share));
    // the rates give a calibration near 1.1 and near 1.19.
    for (share, rates, within) in [
        ("1.0", [3.25, 3.4, 3.1, 3.3], (0.97, 1.21)),
        ("0.9", [3.0, 3.15, 2.9, 3.05], (1.08, 1.34)),
    ] {
        let mock = Mock::default();
        let (base, server) = serve(mock.clone()).await;
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        models(root, &base, json!({}));
        let project = project(root, 160);
        json_of(&run(root, &["sync", project.to_str().unwrap()]).await);
        let mut seen = Vec::new();
        for (rate, question) in rates.into_iter().zip([
            "How does the canary start?",
            "Where is the signed checklist checked?",
            "Which stage runs the gate checks?",
            "What does the rollback stage undo?",
        ]) {
            *mock.usage_rate.lock().unwrap() = rate;
            let asked = json_of(
                &run(
                    root,
                    &[
                        "ask",
                        "--pack",
                        "--layout",
                        "cache",
                        "--prefix-share",
                        share,
                        "--json",
                        question,
                    ],
                )
                .await,
            );
            let calibration: Value = serde_json::from_str(
                &std::fs::read_to_string(root.join("data/pack-calibration.json")).unwrap(),
            )
            .unwrap();
            seen.push((
                asked["pack"]["budget"].as_u64().unwrap(),
                asked["pack"]["prefix_sha256"].as_str().unwrap().to_string(),
                calibration["meta"]["ratio"].as_f64().unwrap(),
            ));
        }
        // The calibration is above 1 and moves, and so does the budget...
        assert!(
            seen.iter().all(|s| s.2 > within.0 && s.2 < within.1),
            "{share}: {seen:?}"
        );
        assert!(
            seen[1].0 != seen[2].0 && seen[2].0 != seen[3].0,
            "{share}: {seen:?}"
        );
        // ...while the repository part holds from the first calibrated call.
        assert!(
            seen[1..].windows(2).all(|w| w[0].1 == w[1].1),
            "{share}: {seen:?}"
        );
        server.abort();
    }
}

/// `graphicle usage` lists the calls not logged yet that still count (not
/// one whose process ended, nor one older than the longest call), and
/// `--clear-pending` removes them all.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_lists_and_clears_the_calls_not_logged_yet() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, "http://127.0.0.1:9/v1", json!({}));
    std::fs::create_dir_all(root.join("data")).unwrap();
    let mut ended = std::process::Command::new("true").spawn().unwrap();
    let ended_pid = ended.id();
    ended.wait().unwrap();
    let me = std::process::id();
    let at = |minutes: i64| {
        (chrono::Utc::now() - chrono::Duration::minutes(minutes))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    let held = json!({
        "running":{"profile":"meta","day":"2026-10-26","worst_cost_usd":0.5,"pid":me,"time":at(5)},
        "ended":{"profile":"meta","day":"2026-10-27","worst_cost_usd":0.5,"pid":ended_pid,"time":at(1)},
        "old":{"profile":"meta","day":"2026-10-26","worst_cost_usd":0.5,"pid":me,"time":at(120)}});
    std::fs::write(root.join("data/usage-pending.json"), held.to_string()).unwrap();
    let report = json_of(&run(root, &["usage", "--json"]).await);
    let pending = report["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "{report}");
    assert_eq!(pending[0]["pid"], me);
    let cleared = json_of(&run(root, &["usage", "--clear-pending", "--json"]).await);
    assert_eq!(cleared["cleared_pending"], 3, "{cleared}");
    assert_eq!(cleared["pending"], json!([]), "{cleared}");
    assert!(!root.join("data/usage-pending.json").exists());
}
