// Adapted from Graphify 0.9.103 (16218bfc), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_hooks.py
//! Hook, install and CLI changes ported from Graphify 0.9.103. Cases follow
//! the upstream tests named in each comment.
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
