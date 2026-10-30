//! A model profile may call a plain-HTTP host that
//! `GRAPHICLE_PROVIDER_HTTP_HOSTS` lists exactly, provided the profile sends
//! no credential; it sends a credential over plain HTTP only to a host that
//! `GRAPHICLE_PROVIDER_HTTP_KEY_HOSTS` lists exactly. Connector endpoints
//! keep HTTPS or loopback HTTP whatever the lists say. The program checks the
//! configuration it loads, so `init` succeeds or fails on it.
use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const HOSTS: &str = "GRAPHICLE_PROVIDER_HTTP_HOSTS";
const KEY_HOSTS: &str = "GRAPHICLE_PROVIDER_HTTP_KEY_HOSTS";

fn init(config: &Value, hosts: Option<&str>) -> Output {
    init_lists(config, hosts, None)
}

fn init_lists(config: &Value, hosts: Option<&str>, key_hosts: Option<&str>) -> Output {
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
        .env_remove(HOSTS)
        .env_remove(KEY_HOSTS)
        .env("VLLM_KEY", "not-a-real-key");
    if let Some(hosts) = hosts {
        command.env(HOSTS, hosts);
    }
    if let Some(key_hosts) = key_hosts {
        command.env(KEY_HOSTS, key_hosts);
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
    // A credential is not sent over plain HTTP to a host that this list
    // alone names.
    refused(
        &init(
            &model("http://192.168.210.33:8000/v1", Some("VLLM_KEY")),
            Some("192.168.210.33"),
        ),
        KEY_HOSTS,
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
    refused(
        &init_lists(&config, None, Some("192.168.210.33")),
        "Use HTTPS, or HTTP on loopback",
    );
}

#[test]
fn a_credential_goes_over_plain_http_only_to_a_host_listed_for_it() {
    let keyed = model("http://192.168.210.33:8000/v1", Some("VLLM_KEY"));
    // Listed for a credential: accepted, with one line on stderr that names
    // the host and the variable.
    let accepted = init_lists(&keyed, None, Some("192.168.210.33"));
    let stderr = String::from_utf8_lossy(&accepted.stderr);
    assert!(accepted.status.success(), "{stderr}");
    let warnings: Vec<&str> = stderr.lines().filter(|l| l.contains(KEY_HOSTS)).collect();
    assert_eq!(warnings.len(), 1, "{stderr}");
    assert!(warnings[0].contains("192.168.210.33"), "{stderr}");
    assert!(warnings[0].contains("clear text"), "{stderr}");
    // Listed only for plain HTTP without a credential, listed nowhere, or a
    // neighbouring address: refused, naming the variable.
    refused(&init_lists(&keyed, Some("192.168.210.33"), None), KEY_HOSTS);
    refused(&init_lists(&keyed, None, None), KEY_HOSTS);
    refused(&init_lists(&keyed, None, Some("192.168.210.3")), KEY_HOSTS);
    // A host listed for a credential also serves a profile without one,
    // and prints nothing about a key.
    let lan = model("http://192.168.210.33:8000/v1", None);
    let accepted = init_lists(&lan, None, Some("192.168.210.33"));
    let stderr = String::from_utf8_lossy(&accepted.stderr);
    assert!(accepted.status.success(), "{stderr}");
    assert!(!stderr.contains(KEY_HOSTS), "{stderr}");
    // HTTPS and loopback HTTP take a credential without either list.
    for url in ["https://api.example.com/v1", "http://127.0.0.1:11434/v1"] {
        let accepted = init_lists(&model(url, Some("VLLM_KEY")), None, None);
        let stderr = String::from_utf8_lossy(&accepted.stderr);
        assert!(accepted.status.success(), "{url}: {stderr}");
        assert!(!stderr.contains(KEY_HOSTS), "{url}: {stderr}");
    }
    // An entry that is not exactly a host is an error that names the variable.
    for entry in [
        "192.168.210.33:8000",
        "http://192.168.210.33",
        "*.lan",
        "192.168.210.33/v1",
    ] {
        refused(&init_lists(&lan, None, Some(entry)), KEY_HOSTS);
    }
}
