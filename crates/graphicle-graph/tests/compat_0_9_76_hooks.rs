// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
//! Graphify 0.9.76 (762cfd3, #3869): `hook install` refuses a `core.hooksPath`
//! that resolves outside the checkout and installs into git's own hooks
//! directory instead. In-repo paths (Husky) and a linked worktree's shared
//! `.git/hooks` are still honoured.
use graphicle::hooks::{hooks_dir, install};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn make_git_repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    path.canonicalize().unwrap()
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}

#[test]
fn hookspath_outside_repo_falls_back_to_default() {
    let (_dir, tmp) = tmp();
    let repo = make_git_repo(&tmp.join("repo"));
    let outside = tmp.join("outside");
    git(
        &repo,
        &["config", "core.hooksPath", outside.to_str().unwrap()],
    );
    let msg = install(&repo).unwrap();
    assert!(!outside.exists());
    assert!(repo.join(".git/hooks/post-commit").exists());
    assert!(repo.join(".git/hooks/post-checkout").exists());
    assert!(msg.contains(".git") && msg.contains("hooks"), "{msg}");
}

/// Runs only as the child of [`the_refusal_is_reported_on_stderr`], which
/// re-executes this test binary to read what `hooks_dir` prints on stderr.
#[test]
fn refusal_child() {
    let Some(repo) = std::env::var_os("GFY_HOOKS_CHILD_REPO") else {
        return;
    };
    hooks_dir(Path::new(&repo)).unwrap();
}

#[test]
fn the_refusal_is_reported_on_stderr() {
    let (_dir, tmp) = tmp();
    let repo = make_git_repo(&tmp.join("repo"));
    let outside = tmp.join("outside");
    git(
        &repo,
        &["config", "core.hooksPath", outside.to_str().unwrap()],
    );
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "refusal_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GFY_HOOKS_CHILD_REPO", &repo)
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("refusing hooks path"), "{err}");
    assert!(err.contains("(#3869)"), "{err}");
    assert!(!outside.exists());
    assert!(repo.join(".git/hooks").is_dir());
}

#[test]
fn hookspath_relative_escape_falls_back_to_default() {
    let (_dir, tmp) = tmp();
    let repo = make_git_repo(&tmp.join("repo"));
    git(&repo, &["config", "core.hooksPath", "../escaped"]);
    hooks_dir(&repo).unwrap();
    assert!(!tmp.join("escaped").exists());
    assert_eq!(hooks_dir(&repo).unwrap(), repo.join(".git/hooks"));
}

#[test]
fn an_in_repo_hookspath_is_still_honoured() {
    let (_dir, tmp) = tmp();
    let repo = make_git_repo(&tmp.join("repo"));
    git(&repo, &["config", "core.hooksPath", ".husky"]);
    assert_eq!(hooks_dir(&repo).unwrap(), repo.join(".husky"));
    assert!(repo.join(".husky").is_dir());
}

#[test]
fn linked_worktree_keeps_its_default_hooks_dir() {
    let (_dir, tmp) = tmp();
    let main = make_git_repo(&tmp.join("main"));
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    fs::write(main.join("f"), "x\n").unwrap();
    git(&main, &["add", "f"]);
    git(&main, &["commit", "-q", "-m", "x"]);
    let wt = tmp.join("wt");
    git(
        &main,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt"],
    );
    let expected = git(&wt, &["rev-parse", "--git-path", "hooks"]);
    let got = hooks_dir(&wt).unwrap();
    assert_eq!(got, wt.join(&expected).canonicalize().unwrap());
    assert!(got.starts_with(main.join(".git")), "{}", got.display());
    assert!(!wt.join(".git/hooks").exists());
}
