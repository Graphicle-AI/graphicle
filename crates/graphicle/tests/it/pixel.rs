//! `graphicle pixel` against a mock of the Responses API: SAM 3.1 segments
//! the point (the documented point prompt and record format), the reading
//! model returns the element's strings, and the lookup finds the string
//! literal that draws it in a synced fixture repository. The spend log
//! records both calls under `pixel`, SAM's at its per-image price. With
//! --text the lookup runs alone, with no model call; a point SAM cannot
//! segment gets the area around it; --explain asks for a ranking from a
//! small pack and keeps only paths the project has.
use axum::{Json, Router, extract::State, routing::post};
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
    /// SAM finds no object at the point.
    sam_empty: Arc<Mutex<bool>>,
}

const RECORDS: &str = "<0f>1<|box;x1=10;y1=5;x2=90;y2=25;w=200;h=100;c=0.97|><|mask;x=10;y=5;c=0.97;data=20,80,!0a|b>c|>";

async fn responses(State(mock): State<Mock>, Json(request): Json<Value>) -> Json<Value> {
    mock.requests.lock().unwrap().push(request.clone());
    let model = request["model"].as_str().unwrap_or("").to_string();
    if model == "sam-3.1" {
        let text = if *mock.sam_empty.lock().unwrap() {
            ""
        } else {
            RECORDS
        };
        return Json(
            json!({"id":"resp_sam","object":"response","status":"completed",
            "model":"sam-3.1","output":[{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":text}]}]}),
        );
    }
    let instructions = request["instructions"].as_str().unwrap_or("");
    let text = if instructions.starts_with("You read one element") {
        json!({"strings":["Open Folder..."],"element_type":"button","icon":"",
            "nearby_labels":["Explorer"],"names":["workbench.action.files.openFolder"]})
    } else {
        json!({"ranking":[{"path":"src/ui/welcome.ts","line":4,"reason":"The button's label."},
            {"path":"no/such/file.ts","line":1,"reason":"Not in the project."}],
            "explanation":"src/ui/welcome.ts:4 gives the Open Folder button its label."})
    };
    Json(
        json!({"id":"resp_read","object":"response","status":"completed","model":model,
        "output":[{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":text.to_string()}]}],
        "usage":{"input_tokens":1500,"output_tokens":120,
            "input_tokens_details":{"cached_tokens":0},
            "output_tokens_details":{"reasoning_tokens":40}}}),
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

async fn run(root: &Path, args: &[&str]) -> Output {
    let root = root.to_path_buf();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || graphicle(&root, &args))
        .await
        .unwrap()
}

fn printed(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn models(root: &Path, base: &str) {
    let spark = json!({"protocol":"responses","base_url":base,"model":"muse-spark-1.3",
        "input_limit":60_000,"output_limit":4_000,"reasoning_effort":"medium",
        "price":{"input":1.25,"output":4.25,"cached_input":0.15}});
    let sam = json!({"protocol":"responses","base_url":base,"model":"sam-3.1",
        "price":{"input":0.0,"output":0.0,"per_image":0.0025}});
    std::fs::write(
        root.join("models.json"),
        json!({"default_model":"spark","models":{"spark":spark,"sam":sam}}).to_string(),
    )
    .unwrap();
}

fn repository(root: &Path) -> PathBuf {
    let project = root.join("project");
    for folder in ["src/ui", "src/test"] {
        std::fs::create_dir_all(project.join(folder)).unwrap();
    }
    let files = [
        (
            "src/ui/welcome.ts",
            "import { localize } from '../nls';\n\nexport const actions = [\n\t{ id: 'workbench.action.files.openFolder', label: localize('openFolder', \"Open &&Folder...\") },\n\t{ id: 'workbench.action.files.newFile', label: localize('newFile', \"New File...\") },\n];\n\nexport function renderWelcome(container: HTMLElement) {\n\tfor (const action of actions) {\n\t\tcontainer.append(button(action.label));\n\t}\n}\n",
        ),
        (
            "src/ui/explorer.ts",
            "import { localize } from '../nls';\nexport const EXPLORER_TITLE = localize('explorer', \"Explorer\");\nexport function emptyMessage() {\n\treturn localize('noFolder', \"You have not yet opened a folder.\");\n}\n",
        ),
        (
            "src/test/welcome.test.ts",
            "import * as assert from 'assert';\nassert.strictEqual(label, 'Open Folder...');\n",
        ),
        ("README.md", "# Fixture\n\nUse Open Folder... to start.\n"),
    ];
    for (path, text) in files {
        std::fs::write(project.join(path), text).unwrap();
    }
    project
}

/// A PNG's signature and header (200 x 100): all `pixel` reads of it.
fn screenshot(root: &Path) -> PathBuf {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    bytes.extend_from_slice(&200u32.to_be_bytes());
    bytes.extend_from_slice(&100u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    let path = root.join("shot.png");
    std::fs::write(&path, bytes).unwrap();
    path
}

fn pixel_log(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("data").join("usage.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["command"] == "pixel")
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_point_is_segmented_read_and_found_in_the_code() {
    let mock = Mock::default();
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &base);
    let project = repository(root);
    let shot = screenshot(root);
    let synced = run(root, &["sync", project.to_str().unwrap()]).await;
    assert!(
        synced.status.success(),
        "{}",
        String::from_utf8_lossy(&synced.stderr)
    );

    let out = run(
        root,
        &["pixel", shot.to_str().unwrap(), "--at", "50,15", "--json"],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = printed(&out);
    // SAM's box, its confidence and its (undecoded) one-bit mask.
    let b = &report["box"];
    assert_eq!(
        (
            b["x1"].as_u64(),
            b["y1"].as_u64(),
            b["x2"].as_u64(),
            b["y2"].as_u64()
        ),
        (Some(10), Some(5), Some(90), Some(25)),
        "{report}"
    );
    assert_eq!(b["from"], "sam");
    assert_eq!(b["model"], "sam-3.1");
    assert_eq!(b["confidence"], 0.97);
    assert_eq!(
        b["mask"],
        json!({"rows":20,"columns":80,"encoding":"one_bit"})
    );
    assert_eq!(report["read"]["strings"], json!(["Open Folder..."]));
    assert_eq!(report["screenshot"]["width"], 200);
    // The string that draws the button: the localized label, not the test
    // or the README.
    let first = &report["candidates"][0];
    assert_eq!(first["path"], "src/ui/welcome.ts", "{report}");
    assert_eq!(first["line"], 4);
    let paths: Vec<&str> = report["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["path"].as_str())
        .collect();
    let test = paths
        .iter()
        .position(|p| *p == "src/test/welcome.test.ts")
        .unwrap();
    assert!(test > 0);
    eprintln!("graph read: {}", report["lookup"]["graph"]);

    // The requests: SAM's point prompt in source pixels with the image as a
    // data URL; the reading call with the point, the box and the image.
    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let sam = &requests[0];
    assert_eq!(sam["model"], "sam-3.1");
    let content = &sam["input"][0]["content"];
    assert_eq!(content[0]["text"], "<0f>1<|point;x=50;y=15;w=200;h=100|>");
    assert!(
        content[1]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,iVBORw0KGgo")
    );
    assert_eq!(sam["metadata"]["include_confidence"], "true");
    let read = &requests[1];
    assert_eq!(read["model"], "muse-spark-1.3");
    assert!(
        read["instructions"]
            .as_str()
            .unwrap()
            .starts_with("You read one element")
    );
    let text = read["input"][0]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("x=50, y=15") && text.contains("from (10, 5) to (90, 25)"),
        "{text}"
    );
    assert_eq!(read["input"][0]["content"][1]["type"], "input_image");
    assert!(read.get("prompt_cache_key").is_none());

    // The spend log: SAM at its per-image price, the reading at its tokens.
    let log = pixel_log(root);
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(
        (log[0]["profile"].as_str(), log[0]["model"].as_str()),
        (Some("sam"), Some("sam-3.1"))
    );
    assert!((log[0]["cost_usd"].as_f64().unwrap() - 0.0025).abs() < 1e-12);
    let read_cost = (1500.0 * 1.25 + 120.0 * 4.25) / 1_000_000.0;
    assert!((log[1]["cost_usd"].as_f64().unwrap() - read_cost).abs() < 1e-12);
    assert_eq!(log[1]["input_tokens"], 1500);
    assert!((report["cost_usd"].as_f64().unwrap() - (0.0025 + read_cost)).abs() < 1e-9);
    assert_eq!(report["calls"][0]["step"], "segment");
    assert_eq!(report["calls"][1]["step"], "read");

    // Nothing segmented at the point: the area around it, and a note.
    *mock.sam_empty.lock().unwrap() = true;
    let out = run(
        root,
        &["pixel", shot.to_str().unwrap(), "--at", "50,15", "--json"],
    )
    .await;
    let report = printed(&out);
    assert_eq!(report["box"]["from"], "around the point");
    assert_eq!(
        (report["box"]["x1"].as_u64(), report["box"]["x2"].as_u64()),
        (Some(0), Some(199))
    );
    assert!(report["notes"][0].as_str().unwrap().contains("no object"));
    assert_eq!(report["candidates"][0]["path"], "src/ui/welcome.ts");

    // The text given: the lookup alone, no model call, nothing logged.
    let before = mock.requests.lock().unwrap().len();
    let out = run(
        root,
        &[
            "pixel",
            "--text",
            "You have not yet opened a folder.",
            "--json",
        ],
    )
    .await;
    let report = printed(&out);
    assert_eq!(
        report["candidates"][0]["path"], "src/ui/explorer.ts",
        "{report}"
    );
    assert_eq!(report["candidates"][0]["line"], 4);
    assert_eq!(report["calls"], json!([]));
    assert_eq!(mock.requests.lock().unwrap().len(), before);
    assert_eq!(pixel_log(root).len(), 4);

    // --explain: one call with a small pack; a path the project lacks is
    // dropped from the model's ranking.
    let out = run(
        root,
        &[
            "pixel",
            "--text",
            "Open Folder...",
            "--explain",
            "--budget",
            "4000",
            "--json",
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = printed(&out);
    assert_eq!(
        report["explain"]["ranking"].as_array().unwrap().len(),
        1,
        "{report}"
    );
    assert_eq!(report["explain"]["ranking"][0]["path"], "src/ui/welcome.ts");
    assert!(
        report["explain"]["explanation"]
            .as_str()
            .unwrap()
            .contains("welcome.ts:4")
    );
    assert_eq!(report["calls"][0]["step"], "explain");
    let last = mock.requests.lock().unwrap().last().cloned().unwrap();
    assert!(
        last["input"]
            .as_str()
            .unwrap()
            .contains("1. src/ui/welcome.ts:4")
    );
    assert_eq!(pixel_log(root).len(), 5);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_input_is_refused_before_any_call() {
    let mock = Mock::default();
    let (base, server) = serve(mock.clone()).await;
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    models(root, &base);
    let project = repository(root);
    let shot = screenshot(root);
    assert!(
        run(root, &["sync", project.to_str().unwrap()])
            .await
            .status
            .success()
    );
    // Outside the image, not an image, and no point.
    let out = run(
        root,
        &["pixel", shot.to_str().unwrap(), "--at", "500,15", "--json"],
    )
    .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside"));
    let readme = project.join("README.md");
    let out = run(
        root,
        &["pixel", readme.to_str().unwrap(), "--at", "1,1", "--json"],
    )
    .await;
    assert!(!out.status.success());
    let out = run(root, &["pixel", shot.to_str().unwrap(), "--json"]).await;
    assert!(!out.status.success());
    assert!(mock.requests.lock().unwrap().is_empty());
    server.abort();
}
