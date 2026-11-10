// Adapted from Graphify 0.9.103 (16218bfc), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_hooks.py, tests/test_hook_compact.py
//! Hook, install and CLI changes ported from Graphify 0.9.103. Cases follow
//! the upstream tests named in each comment.
use graphicle::install::Options;
use serde_json::{Value, json};
#[cfg(unix)]
use std::process::Command;
use std::{fs, path::Path};

#[cfg(unix)]
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
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    assert!(
        entries[0]["command"]
            .as_str()
            .unwrap()
            .ends_with("hook-guard compact")
    );
    let timeout = entries[0]["timeout"].as_i64().unwrap();
    assert!(0 < timeout && timeout <= 30);
}

#[test]
fn install_and_uninstall_keep_the_users_session_start_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".claude/settings.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let user_group =
        json!({"matcher": "startup", "hooks": [{"type": "command", "command": "echo hello"}]});
    let shared =
        json!({"matcher": "compact", "hooks": [{"type": "command", "command": "./notify.sh"}]});
    fs::write(
        &path,
        json!({"hooks": {"SessionStart": [user_group, shared]}}).to_string(),
    )
    .unwrap();
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
    fs::write(
        &local,
        json!({"hooks": {"SessionStart": entry}}).to_string(),
    )
    .unwrap();
    settings_hook(&o, true);
    assert_eq!(
        settings(dir.path(), "settings.local.json")["hooks"]["SessionStart"],
        json!([])
    );
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

fn detect_dir(dir: &Path) -> Value {
    let mut detector = graphicle::detect::Detector::default();
    detector
        .detect(dir, &graphicle::detect::DetectOptions::default())
        .unwrap()
}

#[test]
fn unclassified_warning_fires_when_most_of_the_corpus_has_no_extractor() {
    // test_detect.py (Graphify 0.9.103, #2942)
    let dir = tempfile::tempdir().unwrap();
    for i in 0..30 {
        fs::write(dir.path().join(format!("doc{i}.xml")), "<root/>\n").unwrap();
    }
    for i in 0..10 {
        fs::write(dir.path().join(format!("code{i}.py")), "x = 1\n").unwrap();
    }
    let res = detect_dir(dir.path());
    assert_eq!(res["unclassified"].as_array().unwrap().len(), 30);
    let warning = res["unclassified_warning"].as_str().unwrap();
    assert!(
        warning.contains("30 of 40") && warning.contains("75%"),
        "{warning}"
    );
}

#[test]
fn unclassified_warning_silent_under_the_floor_and_the_fraction() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("odd1.xml"), "<root/>\n").unwrap();
    fs::write(dir.path().join("odd2.xml"), "<root/>\n").unwrap();
    for i in 0..20 {
        fs::write(dir.path().join(format!("code{i}.py")), "x = 1\n").unwrap();
    }
    let res = detect_dir(dir.path());
    assert_eq!(res["unclassified"].as_array().unwrap().len(), 2);
    assert!(res["unclassified_warning"].is_null());
    // Past the floor but a small share of a large corpus: still silent.
    assert_eq!(graphicle::detect::unclassified_warning(25, 400), None);
    assert!(graphicle::detect::unclassified_warning(20, 80).is_some());
    assert_eq!(graphicle::detect::unclassified_warning(19, 0), None);
}

#[test]
fn reflect_is_stale_tracks_the_source_content() {
    // test_reflect.py (Graphify 0.9.103, 892c452d): the overlay entry's
    // staleness compares a recomputed content hash with the stored one; a
    // changed or vanished source is stale.
    use graphicle::reflect::{code_fingerprint, is_stale};
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(".graphicle");
    fs::create_dir_all(&out).unwrap();
    let graph = out.join("graph.json");
    fs::write(&graph, r#"{"nodes": [], "links": []}"#).unwrap();
    let src = dir.path().join("auth.py");
    fs::write(&src, "def login(): pass\n").unwrap();
    let node = json!({"source_file": src.to_string_lossy()});
    let entry = json!({"source_file": src.to_string_lossy(),
                       "code_fingerprint": code_fingerprint(Some(&node), &graph).unwrap()});
    let entry = entry.as_object().unwrap();
    assert!(!is_stale(entry, &graph).unwrap());
    fs::write(&src, "def login(): return 1  # changed\n").unwrap();
    assert!(is_stale(entry, &graph).unwrap());
    fs::write(&src, "def login(): pass\n").unwrap();
    assert!(!is_stale(entry, &graph).unwrap());
    fs::remove_file(&src).unwrap();
    assert!(is_stale(entry, &graph).unwrap());
}

// --- The tree view caps directory children (Graphify 0.9.103, #4766) --------

fn tree_graph(files: &[(&str, &str)]) -> Value {
    let mut files = files.to_vec();
    files.sort();
    json!({"directed": false, "links": [], "nodes": files
        .iter()
        .enumerate()
        .map(|(i, (src, label))| json!({"id": format!("n{i}"), "label": label, "source_file": src}))
        .collect::<Vec<_>>()})
}

fn names(node: &Value) -> Vec<String> {
    node["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn build_tree_caps_directories_with_full_hidden_totals() {
    // test_tree_html.py (17cf5366)
    use graphicle::tree_html::build_tree;
    let mut graph = tree_graph(&[
        ("pkg/Adir/a.py", "A"),
        ("pkg/Adir/b.py", "B"),
        ("pkg/Adir/c.py", "C"),
        ("pkg/b.py", "B0"),
        ("pkg/Zdir/a.py", "Z1"),
        ("pkg/Zdir/b.py", "Z2"),
    ]);
    graph["nodes"].as_array_mut().unwrap().push(
        json!({"id": "file", "label": "leaf.py", "source_file": "pkg/leaf.py", "file_type": "code"}),
    );
    let tree = build_tree(&graph, Some("pkg"), 2, None).unwrap();
    assert_eq!(names(&tree), ["Adir", "b.py", "(+2 more)"]);
    assert_eq!(tree["total_count"], 7);
    assert_eq!(
        tree["children"][2],
        json!({"name": "(+2 more)", "total_count": 3, "children": []})
    );
    let nested = &tree["children"][0];
    assert_eq!(names(nested), ["a.py", "b.py", "(+1 more)"]);
    assert_eq!(nested["total_count"], 3);
}

#[test]
fn build_tree_directory_cap_preserves_file_symbol_behavior() {
    use graphicle::tree_html::build_tree;
    let nodes: Vec<_> = ["gamma", "Beta", "_private", "Alpha"]
        .iter()
        .map(|n| json!({"id": n, "label": n, "source_file": "pkg/a.py"}))
        .collect();
    let graph = json!({ "nodes": nodes });
    let tree = build_tree(&graph, Some("pkg"), 2, None).unwrap();
    let file = &tree["children"][0];
    assert_eq!(names(file), ["(+2 more)", "Alpha", "Beta"]);
    assert_eq!(file["children"][0]["total_count"], 2);
    assert_eq!(file["total_count"], 4);
    assert_eq!(tree["total_count"], 4);
}

#[test]
fn build_tree_caps_the_synthetic_merged_root() {
    use graphicle::tree_html::build_tree;
    let node = |repo: &str, src: &str, label: &str| json!({"id": format!("{repo}::{label}"), "label": label, "source_file": src, "repo": repo});
    let graph = json!({"nodes": [
        node("Alpha", "a.py", "A"), node("Beta", "b.py", "B"),
        node("Zulu", "x.py", "X"), node("Zulu", "y.py", "Y"), node("Zulu", "z.py", "Z")
    ]});
    let tree = build_tree(&graph, None, 2, None).unwrap();
    assert_eq!(names(&tree), ["Alpha", "Beta", "(+1 more)"]);
    assert_eq!(
        tree["children"][2],
        json!({"name": "(+1 more)", "total_count": 3, "children": []})
    );
    assert_eq!(tree["total_count"], 5);
}

#[test]
fn build_tree_directory_cap_boundaries_and_negative() {
    use graphicle::tree_html::build_tree;
    for (cap, expected) in [(0, vec!["(+2 more)"]), (2, vec!["a.py", "b.py"])] {
        let tree = build_tree(
            &tree_graph(&[("pkg/a.py", "A"), ("pkg/b.py", "B")]),
            Some("pkg"),
            cap,
            None,
        )
        .unwrap();
        assert_eq!(names(&tree), expected);
        assert_eq!(tree["total_count"], 2);
        let sum: i64 = tree["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["total_count"].as_i64().unwrap())
            .sum();
        assert_eq!(sum, 2);
        assert!(
            build_tree(&json!({"nodes": []}), None, cap, None).unwrap()["children"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    let wide: Vec<(String, String)> = (0..201)
        .map(|i| (format!("pkg/f{i:03}.py"), i.to_string()))
        .collect();
    let wide: Vec<(&str, &str)> = wide.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let tree = build_tree(&tree_graph(&wide), Some("pkg"), 200, None).unwrap();
    assert_eq!(tree["children"].as_array().unwrap().len(), 201);
    assert_eq!(tree["children"][199]["name"], "f199.py");
    assert_eq!(
        tree["children"][200],
        json!({"name": "(+1 more)", "total_count": 1, "children": []})
    );
    for graph in [json!({"nodes": []}), tree_graph(&[("pkg/a.py", "A")])] {
        let error = build_tree(&graph, None, -1, None).unwrap_err().to_string();
        assert!(
            error.contains("--max-children must be nonnegative"),
            "{error}"
        );
    }
}
