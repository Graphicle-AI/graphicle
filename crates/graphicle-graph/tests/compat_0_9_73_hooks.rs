//! Mirrors the 0.9.73 additions to upstream `tests/test_hooks.py` (#3771):
//! upgrading never rewrites hooks already on disk, so `hook status` compares
//! the installed graphicle block against the current one and names the command
//! that refreshes it. Per-install values (the pinned executable and the
//! `.graphiclerc` viz limit export) and user content around the block are not
//! staleness.
//!
//! The native block pins its executable on the `_GCL_BIN=` line where upstream
//! pins `_PINNED='…'`; an "older release" is simulated by rewriting the launch
//! line, since the native block has no Windows launcher flags to roll back.
use graphicle::hooks::{install, status};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn make_git_repo(tmp: &Path) -> PathBuf {
    let repo = tmp.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    // Pin the hooks dir locally so a global core.hooksPath never leaks in.
    let hooks = repo.join(".git").join("hooks");
    git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
    repo
}

fn status_line(result: &str, name: &str) -> String {
    result
        .lines()
        .find(|line| line.starts_with(&format!("{name}:")))
        .unwrap_or_else(|| panic!("no {name} line in {result:?}"))
        .to_string()
}

const LAUNCH: &str = "\"$_GCL_BIN\" hook launch post-commit";

#[test]
fn test_status_flags_a_hook_block_from_an_older_release() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_git_repo(tmp.path());
    install(&repo).unwrap();
    let hook = repo.join(".git").join("hooks").join("post-commit");
    let text = fs::read_to_string(&hook).unwrap();
    assert!(text.contains(LAUNCH));
    fs::write(&hook, text.replace(LAUNCH, "\"$_GCL_BIN\" hook-rebuild")).unwrap();

    let res = status(&repo).unwrap();
    assert!(
        status_line(&res, "post-commit").contains("out of date"),
        "{res}"
    );
    assert!(
        status_line(&res, "post-commit").contains("graphicle-graph hook install"),
        "{res}"
    );
    assert!(
        !status_line(&res, "post-checkout").contains("out of date"),
        "{res}"
    );

    assert!(
        install(&repo)
            .unwrap()
            .contains("updated existing post-commit hook")
    );
    let res = status(&repo).unwrap();
    assert!(!res.contains("out of date"), "{res}");
}

#[test]
fn test_status_ignores_the_pinned_interpreter() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_git_repo(tmp.path());
    install(&repo).unwrap();
    let hook = repo.join(".git").join("hooks").join("post-commit");
    let text = fs::read_to_string(&hook).unwrap();
    let repinned = regex::Regex::new(r"(?m)^_GCL_BIN=.*$")
        .unwrap()
        .replace_all(&text, "_GCL_BIN='/opt/other/bin/graphicle-graph'")
        .into_owned();
    assert_ne!(repinned, text);
    fs::write(&hook, repinned).unwrap();

    let res = status(&repo).unwrap();
    assert!(!res.contains("out of date"), "{res}");
}

#[test]
fn test_status_compares_only_the_graphicle_block() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_git_repo(tmp.path());
    let hooks_dir = repo.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(
        hooks_dir.join("post-commit"),
        "#!/bin/sh\necho 'user before'\n",
    )
    .unwrap();
    install(&repo).unwrap();
    let hook = hooks_dir.join("post-commit");
    let text = fs::read_to_string(&hook).unwrap();
    fs::write(&hook, text + "echo 'user after'\n").unwrap();

    let res = status(&repo).unwrap();
    assert!(!res.contains("out of date"), "{res}");
}

#[test]
fn test_status_ignores_the_viz_limit_export_that_matches_the_rc() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_git_repo(tmp.path());
    fs::write(repo.join(".graphiclerc"), "viz_node_limit=1234\n").unwrap();
    install(&repo).unwrap();
    let text = fs::read_to_string(repo.join(".git").join("hooks").join("post-commit")).unwrap();
    assert!(text.contains("GRAPHICLE_VIZ_NODE_LIMIT:-1234"));

    let res = status(&repo).unwrap();
    assert!(!res.contains("out of date"), "{res}");
    assert!(res.contains("viz node limit: 1234"), "{res}");
}

#[test]
fn test_status_leaves_a_block_without_an_end_marker_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = make_git_repo(tmp.path());
    let hooks_dir = repo.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(
        hooks_dir.join("post-commit"),
        "#!/bin/sh\n# graphicle-hook-start\ngraphicle-graph --update\n",
    )
    .unwrap();

    let res = status(&repo).unwrap();
    assert_eq!(status_line(&res, "post-commit"), "post-commit: installed");
}
