// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_hook_guard.py, tests/test_hook_strict.py, tests/test_query_seeds.py
//! Soft hook nudges are budgeted per session and agent (Graphify 0.9.87,
//! #3435), PowerShell and EJS sources are graphable (0.9.85, 0.9.88), and the
//! query log records `--seed` values (0.9.88).
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

/// `hook-guard <kind>` in `dir` with `payload` on stdin and only `env` set.
fn guard(dir: &Path, kind: &str, payload: &Value, env: &[(&str, &str)]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
        .args(["hook-guard", kind])
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
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
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".graphicle")).unwrap();
    std::fs::write(dir.path().join(".graphicle/graph.json"), "{}").unwrap();
    dir
}

fn read(session: Option<&str>, agent: Option<&str>) -> Value {
    let mut payload = json!({"tool_name": "Read", "tool_input": {"file_path": "src/app.py"}});
    if let Some(session) = session {
        payload["session_id"] = json!(session);
    }
    if let Some(agent) = agent {
        payload["agent_id"] = json!(agent);
    }
    payload
}

fn search(session: &str) -> Value {
    json!({"session_id": session, "tool_input": {"command": "grep -rn foo ."}})
}

fn fresh_stamp(dir: &Path) {
    let stamp = dir.join(".graphicle/cache/last_query_stamp");
    std::fs::create_dir_all(stamp.parent().unwrap()).unwrap();
    std::fs::write(stamp, "x").unwrap();
}

fn nudged(out: &str) -> bool {
    out.contains("MANDATORY")
}

#[test]
fn fresh_query_stamp_silences_nudges_after_the_first() {
    let dir = project();
    fresh_stamp(dir.path());
    // The stamp is project-wide, so each agent still gets its first nudge.
    assert!(nudged(&guard(
        dir.path(),
        "read",
        &read(Some("s1"), None),
        &[]
    )));
    assert_eq!(guard(dir.path(), "read", &read(Some("s1"), None), &[]), "");
    assert_eq!(guard(dir.path(), "search", &search("s1"), &[]), "");
    // Without a session id only the stamp applies.
    assert_eq!(guard(dir.path(), "read", &read(None, None), &[]), "");
}

#[test]
fn subagents_get_their_own_nudge_budget() {
    let dir = project();
    let cap = [("GRAPHICLE_HOOK_NUDGE_CAP", "1")];
    let sid = "s".repeat(64);
    let sid = Some(sid.as_str());
    assert!(nudged(&guard(dir.path(), "read", &read(sid, None), &cap)));
    assert_eq!(guard(dir.path(), "read", &read(sid, None), &cap), "");
    assert!(nudged(&guard(
        dir.path(),
        "read",
        &read(sid, Some("agent-a")),
        &cap
    )));
    assert_eq!(
        guard(dir.path(), "read", &read(sid, Some("agent-a")), &cap),
        ""
    );
    assert!(nudged(&guard(
        dir.path(),
        "read",
        &read(sid, Some("agent-b")),
        &cap
    )));
    // A subagent's first nudge survives the parent's query.
    let dir = project();
    assert!(nudged(&guard(
        dir.path(),
        "read",
        &read(Some("p"), None),
        &[]
    )));
    fresh_stamp(dir.path());
    assert_eq!(guard(dir.path(), "read", &read(Some("p"), None), &[]), "");
    assert!(nudged(&guard(
        dir.path(),
        "read",
        &read(Some("p"), Some("agent-a")),
        &[]
    )));
}

#[test]
fn nudges_are_capped_per_session_and_shared_by_read_and_search() {
    let dir = project();
    let cap = [("GRAPHICLE_HOOK_NUDGE_CAP", "3")];
    for _ in 0..3 {
        assert!(nudged(&guard(
            dir.path(),
            "read",
            &read(Some("cap"), None),
            &cap
        )));
    }
    assert_eq!(
        guard(dir.path(), "read", &read(Some("cap"), None), &cap),
        ""
    );
    assert_eq!(guard(dir.path(), "search", &search("cap"), &cap), "");
    assert!(nudged(&guard(
        dir.path(),
        "read",
        &read(Some("other"), None),
        &cap
    )));
    let cap = [("GRAPHICLE_HOOK_NUDGE_CAP", "2")];
    assert!(nudged(&guard(dir.path(), "search", &search("mix"), &cap)));
    assert!(nudged(&guard(
        dir.path(),
        "read",
        &read(Some("mix"), None),
        &cap
    )));
    assert_eq!(guard(dir.path(), "search", &search("mix"), &cap), "");
    let marker = dir.path().join(".graphicle/cache/hook_sessions/mix.nudges");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "2");
}

#[test]
fn default_cap_is_five_and_zero_or_junk_disables_it() {
    let dir = project();
    let outs = (0..6)
        .map(|_| nudged(&guard(dir.path(), "read", &read(Some("d"), None), &[])))
        .collect::<Vec<_>>();
    assert_eq!(outs, [true, true, true, true, true, false]);
    for value in ["0", "-1", "not-a-number"] {
        let dir = project();
        let cap = [("GRAPHICLE_HOOK_NUDGE_CAP", value)];
        for _ in 0..8 {
            assert!(
                nudged(&guard(dir.path(), "read", &read(Some("nocap"), None), &cap)),
                "{value}"
            );
        }
    }
    // Calls without a session id are not counted.
    let dir = project();
    let cap = [("GRAPHICLE_HOOK_NUDGE_CAP", "1")];
    for _ in 0..3 {
        assert!(nudged(&guard(dir.path(), "read", &read(None, None), &cap)));
    }
}

#[test]
fn strict_mode_and_stale_notice_share_the_budget() {
    // A fresh query stamp: no block, the session's first nudge, then silence.
    let dir = project();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/app.py"), "x = 1\n").unwrap();
    // The graph is newer than the file, so the file is not stale.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(dir.path().join(".graphicle/graph.json"), "{}").unwrap();
    std::fs::write(
        dir.path().join(".graphicle/manifest.json"),
        json!({"src/app.py": {}}).to_string(),
    )
    .unwrap();
    fresh_stamp(dir.path());
    let strict = [("GRAPHICLE_HOOK_STRICT", "1")];
    let first = guard(dir.path(), "read", &read(Some("s1"), None), &strict);
    assert!(nudged(&first) && !first.contains("deny"), "{first}");
    assert_eq!(
        guard(dir.path(), "read", &read(Some("s1"), None), &strict),
        ""
    );
    // A file changed after the build gets the stale notice, within the budget.
    let dir = project();
    std::fs::write(dir.path().join(".graphicle/needs_update"), "1").unwrap();
    let cap = [("GRAPHICLE_HOOK_NUDGE_CAP", "1")];
    let stale = guard(dir.path(), "read", &read(Some("s2"), None), &cap);
    assert!(stale.to_lowercase().contains("stale"), "{stale}");
    assert_eq!(guard(dir.path(), "read", &read(Some("s2"), None), &cap), "");
}

#[test]
fn powershell_and_ejs_sources_nudge() {
    let dir = project();
    let cap = [("GRAPHICLE_HOOK_NUDGE_CAP", "0")];
    for file in [
        "scripts/deploy.ps1",
        "scripts/Module.psm1",
        "scripts/Module.psd1",
        "scripts/DEPLOY.PS1",
        "views/index.ejs",
    ] {
        let payload =
            json!({"session_id": "x", "tool_name": "Read", "tool_input": {"file_path": file}});
        assert!(nudged(&guard(dir.path(), "read", &payload, &cap)), "{file}");
    }
}

#[test]
fn query_log_records_the_seeds() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(".graphicle");
    std::fs::create_dir_all(&out).unwrap();
    let graph = json!({"directed": false, "multigraph": false, "graph": {}, "links": [], "nodes": [
        {"id": "n1", "label": "extract", "source_file": "pkg/extract.py", "source_location": "L10"},
        {"id": "n3", "label": "build", "source_file": "pkg/build.py", "source_location": "L1"},
    ]});
    std::fs::write(out.join("graph.json"), graph.to_string()).unwrap();
    let log = dir.path().join("queries.log");
    let run = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
        .args(["query", "q", "--seed", "build", "--graph"])
        .arg(out.join("graph.json"))
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("GRAPHICLE_QUERY_LOG", &log)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let text = std::fs::read_to_string(&log).unwrap();
    let record: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(record["seeds"], json!(["build"]), "{record}");
    assert_eq!(record["mode"], "bfs", "{record}");
    assert_eq!(record["depth"], 2, "{record}");
    assert_eq!(record["token_budget"], 2000, "{record}");
}

/// The guard fails open on any payload (upstream `test_fail_open_on_bad_stdin`):
/// stdin that is not UTF-8 is decoded with replacement characters, and a
/// payload that is not a JSON object prints nothing and exits 0.
#[test]
fn guard_fails_open_on_bytes_that_are_not_utf8() {
    let dir = project();
    for kind in ["search", "read"] {
        for raw in [&b"\xff\xfe\x00bad"[..], b"not json at all", b"", b"[1,2,3]"] {
            let mut child = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
                .args(["hook-guard", kind])
                .current_dir(dir.path())
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", dir.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(raw).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{kind} {raw:?}: {out:?}");
            assert!(out.stdout.is_empty(), "{kind} {raw:?}: {out:?}");
        }
    }
}

/// A source file that is not UTF-8 is named once per run, whichever passes
/// re-read it (upstream `test_cp1252_file_warns_once_through_every_pass`; the
/// harness can only count the reference's own warning there).
#[test]
fn extract_warns_once_per_file_that_is_not_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let order = b"class Caf\xe9Order:\n    def pay(self):\n        return self.charge()\n\n    def charge(self):\n        return 1\n";
    std::fs::write(dir.path().join("order.py"), order).unwrap();
    std::fs::write(
        dir.path().join("main.py"),
        b"from order import Caf\xe9Order\n\ndef go():\n    return Caf\xe9Order().pay()\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
        .args(["extract", ".", "--code-only", "--no-cluster"])
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for name in ["order.py", "main.py"] {
        let warning = format!("{name} is not valid UTF-8; read it as cp1252.");
        assert_eq!(stderr.matches(&warning).count(), 1, "{stderr}");
    }
    let graph: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".graphicle/graph.json")).unwrap(),
    )
    .unwrap();
    assert!(
        graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["label"] == "Caf\u{e9}Order"),
        "{graph}"
    );
}
