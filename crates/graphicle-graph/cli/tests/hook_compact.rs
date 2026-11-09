// Adapted from Graphify 0.9.103 (7fc5312e), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_hook_compact.py
//! The nudge budget comes back after a Claude Code context compaction
//! (Graphify 0.9.103): `hook-guard compact`, the SessionStart hook with
//! matcher "compact", deletes the session's nudge counters (its own and its
//! subagents') and its strict-mode deny marker, and restates the graph.
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const SID: &str = "9f1c2d3e-4b5a-6789-abcd-ef0123456789";

fn guard_raw(dir: &Path, kind: &str, stdin: &str, strict: bool, env: &[(&str, &str)]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
        .args(["hook-guard", kind])
        .args(if strict { &["--strict"][..] } else { &[] })
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("CLAUDE_PROJECT_DIR", dir)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn guard(dir: &Path, kind: &str, payload: &Value, strict: bool) -> String {
    guard_raw(dir, kind, &payload.to_string(), strict, &[])
}

/// src/mod.py, listed in the manifest and older than graph.json.
fn project(dir: &Path, out: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let f = dir.join("src/mod.py");
    std::fs::write(&f, "def x():\n    return 1\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))
        .unwrap();
    let out = dir.join(out);
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(
        out.join("manifest.json"),
        json!({"src/mod.py": {"mtime": 1}}).to_string(),
    )
    .unwrap();
    std::fs::write(out.join("graph.json"), r#"{"nodes":[],"links":[]}"#).unwrap();
    f
}

fn read(dir: &Path, f: &Path, sid: &str, agent: Option<&str>, strict: bool) -> String {
    let mut payload = json!({"session_id": sid, "cwd": dir, "hook_event_name": "PreToolUse",
                             "tool_name": "Read", "tool_input": {"file_path": f}});
    if let Some(agent) = agent {
        payload["agent_id"] = json!(agent);
        payload["agent_type"] = json!("Explore");
    }
    guard(dir, "read", &payload, strict)
}

fn session_start(dir: &Path, source: &str) -> String {
    guard(
        dir,
        "compact",
        &json!({"session_id": SID, "cwd": dir, "hook_event_name": "SessionStart",
                "source": source, "model": "claude-opus-4-1"}),
        false,
    )
}

fn kind(out: &str) -> &'static str {
    if out.trim().is_empty() {
        return "silent";
    }
    let v: Value = serde_json::from_str(out).unwrap();
    if v["hookSpecificOutput"]["permissionDecision"] == "deny" {
        "deny"
    } else {
        "nudge"
    }
}

fn markers(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join(".graphicle/cache/hook_sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn compact_restores_the_nudge_budget_for_session_and_subagent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let f = project(dir, ".graphicle");
    for _ in 0..5 {
        assert_eq!(kind(&read(dir, &f, SID, None, false)), "nudge");
    }
    assert_eq!(
        kind(&read(dir, &f, SID, None, false)),
        "silent",
        "cap of 5 reached"
    );
    for _ in 0..5 {
        assert_eq!(kind(&read(dir, &f, SID, Some("a1"), false)), "nudge");
    }
    assert_eq!(kind(&read(dir, &f, SID, Some("a1"), false)), "silent");
    // Another session's spent budget survives this session's compaction.
    let other = "11111111-2222-3333-4444-555555555555";
    for _ in 0..5 {
        read(dir, &f, other, None, false);
    }
    assert_eq!(kind(&read(dir, &f, other, None, false)), "silent");

    let out = session_start(dir, "compact");
    let v: Value = serde_json::from_str(&out).unwrap();
    let hso = v["hookSpecificOutput"].as_object().unwrap();
    assert_eq!(hso["hookEventName"], "SessionStart");
    assert!(
        hso["additionalContext"]
            .as_str()
            .unwrap()
            .contains("graphicle-graph query")
    );
    assert_eq!(hso.len(), 2, "{out}");

    assert_eq!(
        kind(&read(dir, &f, SID, None, false)),
        "nudge",
        "budget back"
    );
    assert_eq!(
        kind(&read(dir, &f, SID, Some("a1"), false)),
        "nudge",
        "subagent too"
    );
    assert_eq!(
        kind(&read(dir, &f, other, None, false)),
        "silent",
        "other session untouched"
    );
}

#[test]
fn compact_rearms_the_strict_deny() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let f = project(dir, ".graphicle");
    assert_eq!(kind(&read(dir, &f, SID, None, true)), "deny");
    assert_eq!(
        kind(&read(dir, &f, SID, None, true)),
        "nudge",
        "deny fires once"
    );
    assert!(markers(dir).contains(&format!("{SID}.denied")));
    session_start(dir, "compact");
    assert!(!markers(dir).contains(&format!("{SID}.denied")));
    assert_eq!(
        kind(&read(dir, &f, SID, None, true)),
        "deny",
        "strict redirects once more"
    );
    assert_eq!(kind(&read(dir, &f, SID, None, true)), "nudge");
}

#[test]
fn other_session_start_sources_do_not_reset() {
    for source in [
        Some("startup"),
        Some("resume"),
        Some("clear"),
        Some("fork"),
        None,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let f = project(dir, ".graphicle");
        assert_eq!(kind(&read(dir, &f, SID, None, true)), "deny");
        for _ in 0..5 {
            read(dir, &f, SID, None, true);
        }
        read(dir, &f, SID, Some("a1"), false);
        let before = markers(dir);
        let mut payload = json!({"session_id": SID, "hook_event_name": "SessionStart", "cwd": dir});
        if let Some(source) = source {
            payload["source"] = json!(source);
        }
        assert_eq!(guard(dir, "compact", &payload, false), "", "{source:?}");
        assert_eq!(markers(dir), before, "{source:?}");
        assert_eq!(
            kind(&read(dir, &f, SID, None, true)),
            "silent",
            "{source:?}"
        );
        // Control: "compact" does give the budget back.
        session_start(dir, "compact");
        assert_eq!(kind(&read(dir, &f, SID, None, true)), "deny", "{source:?}");
    }
}

#[test]
fn compact_on_another_event_does_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let f = project(dir, ".graphicle");
    for _ in 0..6 {
        read(dir, &f, SID, None, false);
    }
    let before = markers(dir);
    let out = guard(
        dir,
        "compact",
        &json!({"session_id": SID, "hook_event_name": "PreCompact", "source": "compact"}),
        false,
    );
    assert_eq!(out, "");
    assert_eq!(markers(dir), before);
    session_start(dir, "compact");
    assert_eq!(kind(&read(dir, &f, SID, None, false)), "nudge");
}

#[test]
fn compact_without_markers_or_graph_is_quiet() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_eq!(session_start(dir, "compact"), "");
    project(dir, ".graphicle");
    assert_eq!(kind(&session_start(dir, "compact")), "nudge");
}

#[test]
fn compact_fails_open_on_bad_input() {
    for stdin in [
        "",
        "not json",
        "[]",
        "\"compact\"",
        "{\"source\": [\"compact\"]}",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let f = project(dir, ".graphicle");
        for _ in 0..6 {
            read(dir, &f, SID, None, false);
        }
        let before = markers(dir);
        assert_eq!(guard_raw(dir, "compact", stdin, false, &[]), "", "{stdin}");
        assert_eq!(markers(dir), before, "{stdin}");
        session_start(dir, "compact");
        assert_eq!(kind(&read(dir, &f, SID, None, false)), "nudge", "{stdin}");
    }
}

#[test]
fn compact_respects_graphicle_out() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let f = project(dir, "alt-out");
    let env = [("GRAPHICLE_OUT", "alt-out")];
    let read_payload =
        json!({"session_id": SID, "tool_name": "Read", "tool_input": {"file_path": f}});
    let run = |kind: &str, payload: &Value| guard_raw(dir, kind, &payload.to_string(), false, &env);
    for _ in 0..5 {
        run("read", &read_payload);
    }
    assert_eq!(run("read", &read_payload), "");
    let out = run(
        "compact",
        &json!({"session_id": SID, "hook_event_name": "SessionStart", "source": "compact"}),
    );
    assert!(out.contains("alt-out/graph.json"), "{out}");
    assert_ne!(run("read", &read_payload), "");
}
