//! P-URL (EVAL_PLAN R5): a model profile may call a plain-HTTP host that
//! `GRAPHICLE_PROVIDER_HTTP_HOSTS` lists exactly, provided the profile sends
//! no credential. Connector endpoints keep HTTPS or loopback HTTP whatever
//! the list says. The program checks the configuration it loads, so `init`
//! succeeds or fails on it.
use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const HOSTS: &str = "GRAPHICLE_PROVIDER_HTTP_HOSTS";

fn init(config: &Value, hosts: Option<&str>) -> Output {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("models.json");
    std::fs::write(&path, config.to_string()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    command
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .arg("--config")
        .arg(&path)
        .arg("init")
        .env_remove(HOSTS);
    if let Some(hosts) = hosts {
        command.env(HOSTS, hosts);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

fn model(base_url: &str, key_env: Option<&str>) -> Value {
    let mut profile = json!({"protocol":"chat","base_url":base_url,"model":"qwen","input_limit":16000,"output_limit":2048});
    if let Some(key) = key_env {
        profile["key_env"] = json!(key);
    }
    json!({"default_model":"lan","models":{"lan":profile}})
}

fn refused(output: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "accepted; expected {expected:?}");
    assert!(stderr.contains(expected), "{stderr}");
}

#[test]
fn a_listed_plain_http_model_host_is_accepted_without_a_credential() {
    let lan = model("http://192.168.210.33:8000/v1", None);
    // Off by default: the strict rule names the opt-in.
    refused(&init(&lan, None), HOSTS);
    let accepted = init(&lan, Some("192.168.210.33"));
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    // Exact hosts only: a neighbouring address or a prefix does not match.
    refused(&init(&lan, Some("192.168.210.3")), HOSTS);
    refused(&init(&lan, Some("192.168.210.34 vllm.internal")), HOSTS);
    // A credential is never sent over plain HTTP, listed host or not.
    refused(
        &init(
            &model("http://192.168.210.33:8000/v1", Some("VLLM_KEY")),
            Some("192.168.210.33"),
        ),
        "never sent over plain HTTP",
    );
    // An entry that is not exactly a host is an error, not a silent no-op.
    refused(&init(&lan, Some("192.168.210.33:8000")), HOSTS);
}

#[test]
fn connector_endpoints_keep_https_or_loopback_whatever_the_list_says() {
    let mut config = model("https://api.example.com/v1", None);
    config["connector_endpoints"] = json!({"github":"http://192.168.210.33/api"});
    refused(
        &init(&config, Some("192.168.210.33")),
        "Use HTTPS, or HTTP on loopback",
    );
}
