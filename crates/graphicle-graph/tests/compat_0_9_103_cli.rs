// Adapted from Graphify 0.9.103 (16218bfc), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_hooks.py, tests/test_hook_compact.py
//! Hook, install and CLI changes ported from Graphify 0.9.103. Cases follow
//! the upstream tests named in each comment.
use graphicle::install::Options;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
#[test]
fn worktree_guard_survives_exported_cdpath() {
    // test_hooks.py test_worktree_guard_survives_exported_cdpath (#4977): with
    // CDPATH set, `cd` prints the directory it resolved, so an unguarded
    // capture became two lines and the anchor exited before the rebuild.
    if Command::new("bash").arg("--version").output().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("myrepo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    graphicle::hooks::install(&repo).unwrap();
    let script = fs::read_to_string(repo.join(".git/hooks/post-commit")).unwrap();
    let anchor = "[ -n \"$_GCL_TOP\" ] && cd \"$_GCL_TOP\" || exit 0\n";
    let head = script.split(anchor).next().unwrap();
    assert!(head.contains("_GCL_GITDIR=$(CDPATH= cd "), "{script}");
    let probe = format!("CDPATH=\".:$HOME\"\n{head}{anchor}printf 'TOP=%s\\n' \"$_GCL_TOP\"\n)\n");
    let out = Command::new("bash")
        .args(["-c", &probe])
        .current_dir(&repo)
        .env_remove("GRAPHICLE_SKIP_HOOK")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let top = stdout
        .split("TOP=")
        .nth(1)
        .unwrap_or_else(|| panic!("anchor exited before printing: {stdout:?}"))
        .trim();
    assert_eq!(Path::new(top).file_name().unwrap(), "myrepo", "{stdout:?}");
}

fn options(home: &Path, project: &Path) -> Options {
    Options {
        home: home.to_path_buf(),
        project_dir: project.to_path_buf(),
        project: true,
        windows: false,
        claude_config_dir: None,
        local_appdata: None,
        executable: Some("/native bin/graphicle-graph".into()),
        strict: false,
    }
}

fn settings_hook(o: &Options, remove: bool) {
    graphicle::install::dispatch(&json!({
        "action": "hook", "platform": "claude", "remove": remove,
        "options": serde_json::to_value(o).unwrap()
    }))
    .unwrap();
}

fn settings(project: &Path, name: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(project.join(".claude").join(name)).unwrap()).unwrap()
}

/// The groups holding an engine hook command.
fn ours(groups: &Value) -> Vec<Value> {
    groups
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| {
            g["hooks"].as_array().is_some_and(|h| {
                h.iter().any(|e| {
                    e["command"]
                        .as_str()
                        .is_some_and(|c| c.split_whitespace().any(|w| w == "hook-guard"))
                })
            })
        })
        .cloned()
        .collect()
}

#[test]
fn install_adds_one_compact_session_start_hook() {
    // test_hook_compact.py (Graphify 0.9.103).
    let dir = tempfile::tempdir().unwrap();
    let o = options(dir.path(), dir.path());
    settings_hook(&o, false);
    settings_hook(&o, false); // a re-install does not duplicate it
    let groups = &settings(dir.path(), "settings.json")["hooks"]["SessionStart"];
    let ours = ours(groups);
    assert_eq!(ours.len(), 1, "{groups}");
    assert_eq!(ours[0]["matcher"], "compact");
    let entries = ours[0]["hooks"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["type"], "command");
    assert!(entries[0]["command"].as_str().unwrap().ends_with("hook-guard compact"));
    let timeout = entries[0]["timeout"].as_i64().unwrap();
    assert!(0 < timeout && timeout <= 30);
}

#[test]
fn install_and_uninstall_keep_the_users_session_start_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".claude/settings.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let user_group = json!({"matcher": "startup", "hooks": [{"type": "command", "command": "echo hello"}]});
    let shared = json!({"matcher": "compact", "hooks": [{"type": "command", "command": "./notify.sh"}]});
    fs::write(&path, json!({"hooks": {"SessionStart": [user_group, shared]}}).to_string()).unwrap();
    let o = options(dir.path(), dir.path());
    settings_hook(&o, false);
    let groups = settings(dir.path(), "settings.json")["hooks"]["SessionStart"].clone();
    assert_eq!(groups[0], user_group);
    assert_eq!(groups[1], shared);
    assert_eq!(ours(&groups).len(), 1);
    settings_hook(&o, true);
    let hooks = settings(dir.path(), "settings.json")["hooks"].clone();
    assert_eq!(hooks["SessionStart"], json!([user_group, shared]));
    assert!(ours(&hooks["PreToolUse"]).is_empty());
}

#[test]
fn uninstall_removes_the_session_start_hook() {
    let dir = tempfile::tempdir().unwrap();
    let o = options(dir.path(), dir.path());
    settings_hook(&o, false);
    settings_hook(&o, true);
    let hooks = settings(dir.path(), "settings.json")["hooks"].clone();
    assert!(ours(&hooks["SessionStart"]).is_empty());
    assert!(ours(&hooks["PreToolUse"]).is_empty());
}

#[test]
fn uninstall_cleans_session_start_from_settings_local() {
    let dir = tempfile::tempdir().unwrap();
    let o = options(dir.path(), dir.path());
    let local = dir.path().join(".claude/settings.local.json");
    fs::create_dir_all(local.parent().unwrap()).unwrap();
    let entry = graphicle::install::claude_sessionstart_hooks(&o);
    fs::write(&local, json!({"hooks": {"SessionStart": entry}}).to_string()).unwrap();
    settings_hook(&o, true);
    assert_eq!(settings(dir.path(), "settings.local.json")["hooks"]["SessionStart"], json!([]));
}

#[test]
fn pi_destinations() {
    // test_pi_install.py (Graphify 0.9.103, #5011): Pi reads project skills
    // from .pi/skills; the user scope keeps ~/.pi/agent/skills.
    let dir = tempfile::tempdir().unwrap();
    let (home, project) = (dir.path().join("home"), dir.path().join("project"));
    let mut o = options(&home, &project);
    o.project = true;
    assert_eq!(
        graphicle::install::destination("pi", &o).unwrap(),
        project.join(".pi/skills/graphicle/SKILL.md")
    );
    o.project = false;
    assert_eq!(
        graphicle::install::destination("pi", &o).unwrap(),
        home.join(".pi/agent/skills/graphicle/SKILL.md")
    );
}
