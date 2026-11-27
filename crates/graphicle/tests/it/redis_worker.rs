//! `graphicle redis-worker` refuses an empty queue password or model key before it connects.
use serde_json::json;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

#[test]
fn redis_worker_rejects_empty_credentials_before_connecting() {
    const KEY: &str = "GRAPHICLE_TEST_WORKER_KEY";
    const QUEUE_PASSWORD: &str = "GRAPHICLE_TEST_WORKER_QUEUE_PASSWORD";
    const QUEUE_UNAVAILABLE: &str = "Worker queue credential environment variable is unavailable";
    const MODEL_UNAVAILABLE: &str = "Worker model credential unavailable";
    let dir = TempDir::new().unwrap();
    let models = dir.path().join("models.json");
    let queue = dir.path().join("queue.json");
    std::fs::write(
        &models,
        json!({"default_model":"m","models":{"m":{"protocol":"anthropic","base_url":"https://api.anthropic.com/v1","model":"fixture","input_limit":16000,"output_limit":2048,"key_env":KEY}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        &queue,
        json!({"endpoint":"redis://127.0.0.1:1","namespace":"fixture","credential_ref":QUEUE_PASSWORD}).to_string(),
    )
    .unwrap();
    let run = |queue_password: &str, key: &str| -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        command
            .arg("--config")
            .arg(&models)
            .arg("redis-worker")
            .arg("--queue-config")
            .arg(&queue)
            .arg("--once")
            .env_remove("GRAPHICLE_DATA_DIR")
            .env(KEY, key)
            .env(QUEUE_PASSWORD, queue_password)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::fork_guard::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap()
    };

    for (output, expected) in [
        (run("", "fixture-key"), QUEUE_UNAVAILABLE),
        (run("fixture-password", ""), MODEL_UNAVAILABLE),
    ] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "accepted empty credential");
        assert!(stderr.contains(expected), "{stderr}");
    }

    let present = run("fixture-password", "fixture-key");
    let stderr = String::from_utf8_lossy(&present.stderr);
    assert!(
        !present.status.success(),
        "connected to the fixture Redis port"
    );
    assert!(!stderr.contains(QUEUE_UNAVAILABLE), "{stderr}");
    assert!(!stderr.contains(MODEL_UNAVAILABLE), "{stderr}");
}
