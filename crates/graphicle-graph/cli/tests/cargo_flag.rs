// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_cargo_missing_manifest.py
//! `--cargo` through the executable (#3677); the library-level Cargo
//! introspection tests stay in the library's tests/compat_0_9_66_cargo.rs.
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn node_ids(result: &Value) -> BTreeSet<String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect()
}

fn graph_cli_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_graphicle-graph"))
}

/// The executable under a throwaway HOME: a run never checks or refreshes
/// the skills installed for the user running the tests.
fn graph_cli(home: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(graph_cli_bin());
    command.env("HOME", home).env("USERPROFILE", home);
    command
}

#[test]
fn cargo_flag_without_a_manifest_still_writes_the_graph_3677() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    write(&repo.join("app.py"), "def hello():\n    return 1\n");
    let out = graph_cli(dir.path())
        .args(["extract", ".", "--code-only", "--cargo"])
        .current_dir(&repo)
        .env("GRAPHICLE_OUT", repo.join(".graphicle"))
        .env_remove("GEMINI_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("no Cargo.toml at scan root"), "{text}");
    let graph = repo.join(".graphicle/graph.json");
    assert!(graph.exists(), "{text}");
    let g: Value = serde_json::from_str(&std::fs::read_to_string(graph).unwrap()).unwrap();
    assert!(
        g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["label"].as_str().unwrap_or("").starts_with("hello"))
    );
}

#[test]
fn cargo_flag_with_a_manifest_still_adds_crate_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    write(&repo.join("app.py"), "def hello():\n    return 1\n");
    write(&repo.join("Cargo.toml"), APP);
    let out = graph_cli(dir.path())
        .args(["extract", ".", "--code-only", "--cargo"])
        .current_dir(&repo)
        .env("GRAPHICLE_OUT", repo.join(".graphicle"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let g: Value =
        serde_json::from_str(&std::fs::read_to_string(repo.join(".graphicle/graph.json")).unwrap())
            .unwrap();
    assert!(node_ids(&g).iter().any(|id| id.contains("app")));
}

/// The executable's runtime is the 0.25.10 core whatever the library allows.
#[test]
fn executable_links_the_pinned_tree_sitter_runtime() {
    let lock =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.lock")).unwrap();
    let versions: Vec<&str> = lock
        .split("[[package]]")
        .filter(|p| p.contains("\nname = \"tree-sitter\"\n"))
        .filter_map(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
        .collect();
    assert_eq!(versions, ["\"0.25.10\""]);
}
