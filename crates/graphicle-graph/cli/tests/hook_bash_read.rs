// Adapted from Graphify 0.9.103 (fd1ee645), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_hook_bash_read.py
//! A Bash command that prints an in-project code file (cat, head, tail,
//! sed -n, nl, less, more, bat) gets the Read nudge for that file, through
//! the same per-agent budget, and is never denied (Graphify 0.9.103).
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const SID: &str = "3e1f0c2a-7b6d-4e5f-9a8b-1c2d3e4f5a6b";

fn guard(dir: &Path, kind: &str, payload: &Value, extra: &[&str], env: &[(&str, &str)]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
        .args(["hook-guard", kind])
        .args(extra)
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
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

struct Project {
    tmp: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();
        let app = proj.join("src/app.py");
        std::fs::write(&app, "import os\n\ndef run():\n    return os.getcwd()\n").unwrap();
        // Older than the graph, so the file is not stale.
        std::fs::File::options()
            .write(true)
            .open(&app)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        std::fs::write(proj.join("README.md"), "# demo\n").unwrap();
        std::fs::write(proj.join("package.json"), "{}\n").unwrap();
        std::fs::write(proj.join("logo.png"), b"\x89PNG").unwrap();
        let out = proj.join(".graphicle");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("GRAPH_REPORT.md"), "# report\n").unwrap();
        let graph = json!({"nodes": [
            {"id": "app", "label": "app.py", "source_file": "src/app.py", "source_location": "L1"},
            {"id": "app_run", "label": "run()", "source_file": "src/app.py", "source_location": "L3"}
        ], "links": []});
        std::fs::write(out.join("graph.json"), graph.to_string()).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("other.py"), "x = 1\n").unwrap();
        Self { tmp }
    }
    fn dir(&self) -> std::path::PathBuf {
        self.tmp.path().join("proj")
    }
    fn bash_as(&self, command: &str, sid: &str, agent: Option<&str>, tool: Option<&str>, strict: bool) -> String {
        let mut payload = json!({
            "session_id": sid,
            "cwd": self.dir(),
            "hook_event_name": "PreToolUse",
            "tool_input": {"command": command, "description": "Read the file"},
        });
        if let Some(tool) = tool {
            payload["tool_name"] = json!(tool);
        }
        if let Some(agent) = agent {
            payload["agent_id"] = json!(agent);
        }
        let extra: &[&str] = if strict { &["--strict"] } else { &[] };
        guard(&self.dir(), "search", &payload, extra, &[])
    }
    fn bash(&self, command: &str) -> String {
        self.bash_as(command, SID, None, Some("Bash"), false)
    }
    fn read_tool(&self) -> String {
        let payload = json!({"session_id": SID, "tool_name": "Read", "cwd": self.dir(),
                             "tool_input": {"file_path": self.dir().join("src/app.py")}});
        guard(&self.dir(), "read", &payload, &[], &[])
    }
    fn nudges(&self) -> Vec<(String, String)> {
        let dir = self.dir().join(".graphicle/cache/hook_sessions");
        let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".nudges"))
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read_to_string(e.path()).unwrap(),
                )
            })
            .collect();
        out.sort();
        out
    }
}

fn context(out: &str) -> String {
    let v: Value = serde_json::from_str(out).unwrap();
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

fn is_read_nudge(out: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(out) else {
        return false;
    };
    let hso = &v["hookSpecificOutput"];
    hso["hookEventName"] == "PreToolUse"
        && hso.get("permissionDecision").is_none()
        && context(out).contains("reading source files one by one")
}

#[test]
fn bash_reader_gets_the_read_nudge_with_the_file_hint() {
    for command in [
        "cat src/app.py",
        "cat -n src/app.py",
        "sed -n 1,80p src/app.py",
        "sed -n '/def run/,/^$/p' src/app.py",
        "head -50 src/app.py",
        "head -n 20 src/app.py",
        "tail src/app.py",
        "tail -n +3 src/app.py",
        "nl -ba src/app.py",
        "less src/app.py",
        "more src/app.py",
        "bat --paging=never src/app.py",
        "cat < src/app.py",
        "cat src/app.py | grep run",
        "sudo cat ./src/app.py",
        "git status && cat src/app.py",
        "cat src/app.py 2>/dev/null",
        "head -50 src/app.py 2>&1",
        "cat {proj}/src/app.py",
    ] {
        let p = Project::new();
        let command = command.replace("{proj}", &p.dir().to_string_lossy());
        let out = p.bash(&command);
        assert!(is_read_nudge(&out), "{command}: {out}");
        let ctx = context(&out);
        assert!(ctx.contains("Start with: `graphicle-graph explain \"app.py\"`"), "{command}: {ctx}");
        assert!(ctx.contains("- run() (src/app.py L3)"), "{command}: {ctx}");
        assert_eq!(p.nudges(), vec![(format!("{SID}.nudges"), "1".to_string())], "{command}");
    }
}

#[test]
fn non_reads_stay_silent() {
    for command in [
        "cat README.md",
        "cat package.json",
        "cat logo.png",
        "cat src/missing.py",
        "cat > src/new.py",
        "cat >> src/app.py",
        "cat src/app.py > /tmp/copy.py",
        "cat <<'EOF' > src/new.py\nimport os\nEOF",
        "sed -i 's/a/b/' src/app.py",
        "sed -ni 's/a/b/p' src/app.py",
        "echo x | cat",
        "git diff | head -50",
        "cat -",
        "echo \"cat src/app.py\"",
        "# cat src/app.py",
        "cat ../elsewhere/other.py",
        "cat {tmp}/elsewhere/other.py",
        "cat .graphicle/GRAPH_REPORT.md",
        "head .graphicle/graph.json",
        "cat $HOME/src/app.py",
        "cat src/*.py",
        "python src/app.py",
    ] {
        let p = Project::new();
        let command = command.replace("{tmp}", &p.tmp.path().to_string_lossy());
        assert_eq!(p.bash(&command), "", "{command}");
        assert!(p.nudges().is_empty(), "{command}: a silent call spends no budget");
        // Control in the same project and session: a real read nudges.
        assert!(is_read_nudge(&p.bash("cat src/app.py")), "{command}");
    }
}

#[test]
fn bash_reads_share_the_read_budget() {
    let p = Project::new();
    let outs = [
        p.bash("cat src/app.py"),
        p.read_tool(),
        p.bash("head -50 src/app.py"),
        p.bash("sed -n 1,9p src/app.py"),
        p.read_tool(),
    ];
    assert!(outs.iter().all(|o| !o.is_empty()), "{outs:?}");
    assert!(is_read_nudge(&outs[0]));
    assert_eq!(p.bash("tail src/app.py"), "", "cap of 5 shared with the Read tool");
    let sub = p.bash_as("cat src/app.py", SID, Some("agent-7"), Some("Bash"), false);
    assert!(is_read_nudge(&sub), "a subagent has its own budget (#3435)");
}

#[test]
fn strict_mode_never_blocks_a_bash_read() {
    for tool in [Some("Bash"), None] {
        let p = Project::new();
        std::fs::write(
            p.dir().join(".graphicle/manifest.json"),
            json!({"src/app.py": {"mtime": 1}}).to_string(),
        )
        .unwrap();
        let out = p.bash_as("cat src/app.py", "strict-a", None, tool, true);
        assert!(is_read_nudge(&out), "{tool:?}: {out}");
        let denied = std::fs::read_dir(p.dir().join(".graphicle/cache/hook_sessions"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".denied"));
        assert!(!denied, "{tool:?}");
        // The Read tool on the same file is still denied once.
        let r = guard(
            &p.dir(),
            "read",
            &json!({"session_id": "r", "tool_name": "Read",
                    "tool_input": {"file_path": p.dir().join("src/app.py")}}),
            &["--strict"],
            &[],
        );
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{r}");
    }
}

#[test]
fn relative_path_resolves_against_the_bash_cwd() {
    let p = Project::new();
    let payload = json!({"session_id": SID, "tool_name": "Bash", "cwd": p.dir().join("src"),
                         "tool_input": {"command": "cat app.py"}});
    let out = guard(&p.dir(), "search", &payload, &[], &[]);
    assert!(is_read_nudge(&out), "{out}");
    assert!(context(&out).contains("- run() (src/app.py L3)"), "{out}");
}

#[test]
fn stale_file_gets_the_stale_read_nudge() {
    let p = Project::new();
    std::fs::write(p.dir().join(".graphicle/needs_update"), "1").unwrap();
    let out = p.bash("cat src/app.py");
    assert!(context(&out).contains("may be STALE"), "{out}");
}

#[test]
fn searches_keep_the_search_nudge() {
    for command in [
        "grep -rn run src",
        "rg run src/app.py",
        "sed 's/a/b/' src/app.py",
        "awk '/run/' src/app.py",
    ] {
        let p = Project::new();
        let out = p.bash(command);
        assert!(
            context(&out).contains("Finding where something lives is what the graph answers"),
            "{command}: {out}"
        );
        assert!(is_read_nudge(&p.bash("cat src/app.py")), "{command}");
    }
}

#[test]
fn no_graph_no_nudge() {
    let p = Project::new();
    let graph = p.dir().join(".graphicle/graph.json");
    let saved = std::fs::read(&graph).unwrap();
    std::fs::remove_file(&graph).unwrap();
    assert_eq!(p.bash("cat src/app.py"), "");
    std::fs::write(&graph, saved).unwrap();
    assert!(is_read_nudge(&p.bash("cat src/app.py")));
}
