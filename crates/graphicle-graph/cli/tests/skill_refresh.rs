// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_skill_auto_refresh.py
//! #1805: a CLI run after an upgrade refreshes the skills an older release
//! installed, under upstream's limits and skill-directory lock.
use graphicle::{VERSION, install};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const OLD: &str = "0.0.1";
const OLD_BODY: &str = "---\nname: graphicle\n---\nold skill body\n";
const PACKAGED: &str = include_str!("../../src/assets/install/SKILL.md");

/// The executable under `home` with nothing else from the caller's environment.
fn graph_cli(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"));
    cmd.args(args)
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("USERPROFILE", home);
    // Windows programs need SystemRoot to load their system libraries.
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        cmd.env("SystemRoot", root);
    }
    cmd.envs(env.iter().copied()).output().unwrap()
}

fn version(home: &Path, env: &[(&str, &str)]) -> (String, String) {
    let out = graph_cli(home, &["--version"], env);
    assert!(out.status.success());
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// Lay down a skill as an older release left it. The path is joined as the
/// engine joins a skill destination (`home.join("<dir>/SKILL.md")`), so the
/// paths it prints compare equal on Windows too; on Unix the same path.
fn stale(home: &Path, dir: &str, stamp: &str, body: &str) -> PathBuf {
    let skill = home.join(format!("{dir}/SKILL.md"));
    let dir = skill.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&skill, body).unwrap();
    std::fs::write(dir.join(".graphicle_version"), stamp).unwrap();
    skill
}

fn stamp(dst: &Path) -> String {
    std::fs::read_to_string(dst.parent().unwrap().join(".graphicle_version")).unwrap()
}

fn refreshed(dst: &Path) -> String {
    format!(
        "graphicle: refreshed skill at {} ({OLD} -> {VERSION}); set GRAPHICLE_NO_AUTO_REFRESH=1 to disable",
        dst.parent().unwrap().display()
    )
}

fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() && !path.is_symlink() {
                stack.push(path.clone());
            }
            paths.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    paths.sort();
    paths
}

fn lock_file(home: &Path, dst: &Path) -> PathBuf {
    install::skill_lock_path(dst.parent().unwrap(), home)
}

fn lock_is_free(home: &Path, dst: &Path) -> bool {
    let file = std::fs::File::open(lock_file(home, dst)).unwrap();
    file.try_lock().is_ok()
}

#[test]
fn every_stale_copy_is_refreshed_and_stdout_stays_clean() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    // On Windows Gemini's user skill lives in ~/.agents/skills, which the
    // generic Agent-Skills target writes too; upstream leaves a directory
    // with two writers to the version warning, and ~/.gemini/skills is no
    // Windows target, so the Gemini copy is not refreshed there.
    let dirs: &[&str] = if cfg!(windows) {
        &[
            ".claude/skills/graphicle",
            ".codex/skills/graphicle",
            ".config/opencode/skills/graphicle",
        ]
    } else {
        &[
            ".claude/skills/graphicle",
            ".codex/skills/graphicle",
            ".config/opencode/skills/graphicle",
            ".gemini/skills/graphicle",
        ]
    };
    let dsts: Vec<PathBuf> = dirs
        .iter()
        .map(|dir| stale(home, dir, OLD, PACKAGED))
        .collect();
    let (stdout, stderr) = version(home, &[]);
    assert_eq!(stdout, format!("graphicle-graph {VERSION}\n"));
    for dst in &dsts {
        assert_eq!(stamp(dst), VERSION, "{}", dst.display());
        assert_eq!(std::fs::read_to_string(dst).unwrap(), PACKAGED);
        assert!(stderr.contains(&refreshed(dst)), "{stderr}");
        // An unchanged copy keeps no backup; the lock is released and stays
        // outside the skill directory.
        assert!(!dst.with_extension("md.bak").exists());
        assert!(lock_is_free(home, dst));
        assert!(lock_file(home, dst).starts_with(home.join(".cache/graphicle/skill-locks")));
    }
    assert!(!stderr.contains("is from graphicle"), "{stderr}");
}

#[test]
fn a_local_edit_survives_as_bak_and_is_the_only_other_line() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let dst = stale(
        home,
        ".codex/skills/graphicle",
        OLD,
        &format!("{OLD_BODY}MY LOCAL EDIT\n"),
    );
    let (_, stderr) = version(home, &[]);
    let backup = dst.with_extension("md.bak");
    assert!(
        std::fs::read_to_string(&backup)
            .unwrap()
            .contains("MY LOCAL EDIT")
    );
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), PACKAGED);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(
        lines,
        [
            refreshed(&dst),
            format!(
                "  previous copy    ->  {} (differed from the packaged skill)",
                backup.display()
            )
        ]
    );
}

#[test]
fn nothing_is_created_and_incomplete_copies_are_left_for_the_warning() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let before = tree(home);
    version(home, &[]);
    assert_eq!(
        tree(home),
        before,
        "a platform never installed is not created"
    );
    let dst = stale(home, ".codex/skills/graphicle", OLD, OLD_BODY);
    std::fs::remove_file(&dst).unwrap();
    let (_, stderr) = version(home, &[]);
    assert!(!dst.exists());
    assert_eq!(stamp(&dst), OLD);
    assert!(stderr.contains("SKILL.md is missing"), "{stderr}");
}

/// The coexistence marker is gone: a copy in the third-party tool's own
/// folder (`skills/graphify`, stamped `.graphify_version`) is not this
/// engine's, so it keeps its variant and gets neither a refresh nor a warning.
#[test]
fn a_third_party_skill_folder_is_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let dir = home.join(".claude/skills/graphify");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), OLD_BODY).unwrap();
    std::fs::write(dir.join(".graphify_version"), OLD).unwrap();
    let (_, stderr) = version(home, &[]);
    assert_eq!(
        std::fs::read_to_string(dir.join(".graphify_version")).unwrap(),
        OLD
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
        OLD_BODY
    );
    assert!(!stderr.contains("refreshed"), "{stderr}");
    assert!(!stderr.contains("is from"), "{stderr}");
}

#[test]
fn the_opt_out_disables_it_and_a_falsy_value_keeps_it_on() {
    for value in ["1", "true", "YES", " yes "] {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let dst = stale(home, ".codex/skills/graphicle", OLD, OLD_BODY);
        let (_, stderr) = version(home, &[("GRAPHICLE_NO_AUTO_REFRESH", value)]);
        assert_eq!(stamp(&dst), OLD, "{value}");
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), OLD_BODY);
        assert!(!stderr.contains("refreshed"), "{stderr}");
        assert!(
            stderr.contains(&format!("is from graphicle {OLD}")),
            "{stderr}"
        );
    }
    let home = tempfile::tempdir().unwrap();
    let dst = stale(home.path(), ".codex/skills/graphicle", OLD, OLD_BODY);
    version(home.path(), &[("GRAPHICLE_NO_AUTO_REFRESH", "0")]);
    assert_eq!(stamp(&dst), VERSION);
}

#[test]
fn newer_and_current_copies_are_untouched() {
    for installed in ["999.0.0", VERSION] {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let dst = stale(home, ".codex/skills/graphicle", installed, OLD_BODY);
        let (_, stderr) = version(home, &[]);
        assert_eq!(stamp(&dst), installed);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), OLD_BODY);
        assert!(!dst.with_extension("md.bak").exists());
        assert!(!stderr.contains("refreshed"), "{stderr}");
    }
}

#[test]
fn directories_with_more_than_one_writer_are_not_guessed() {
    // `graphicle-graph vscode install` writes copilot's directory.
    let home = tempfile::tempdir().unwrap();
    let dst = stale(home.path(), ".copilot/skills/graphicle", OLD, OLD_BODY);
    let (_, stderr) = version(home.path(), &[]);
    assert_eq!(stamp(&dst), OLD);
    assert!(
        stderr.contains(&format!("is from graphicle {OLD}")),
        "{stderr}"
    );
    // Two platform directories linked to one real directory hold one copy.
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let real = home.join(".config/opencode/skills/graphicle");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(home.join(".codex/skills")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, home.join(".codex/skills/graphicle")).unwrap();
    // Windows: a directory symlink needs Developer Mode or the privilege.
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&real, home.join(".codex/skills/graphicle")).is_err() {
        return;
    }
    let dst = stale(home, ".codex/skills/graphicle", OLD, OLD_BODY);
    version(home, &[]);
    assert_eq!(stamp(&dst), OLD);
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), OLD_BODY);
}

#[test]
fn a_directory_another_process_holds_is_skipped() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let dst = stale(home, ".codex/skills/graphicle", OLD, OLD_BODY);
    let path = lock_file(home, &dst);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let holder = std::fs::File::create(&path).unwrap();
    holder.lock().unwrap();
    version(home, &[]);
    assert_eq!(stamp(&dst), OLD);
    drop(holder);
    // The OS drops a lock with its holder: nothing is left to time out.
    version(home, &[]);
    assert_eq!(stamp(&dst), VERSION);
}

#[test]
fn install_and_uninstall_wait_for_the_lock() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path().to_path_buf();
    // Unedited, so the install keeps no SKILL.md.bak in the directory.
    let dst = stale(&home, ".codex/skills/graphicle", OLD, PACKAGED);
    let path = lock_file(&home, &dst);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let options = install::Options {
        home: home.clone(),
        project: false,
        ..Default::default()
    };
    for write in [true, false] {
        let holder = std::fs::File::create(&path).unwrap();
        holder.lock().unwrap();
        let worker = {
            let options = options.clone();
            std::thread::spawn(move || {
                if write {
                    install::copy_skill("codex", &options).map(|_| ())
                } else {
                    install::remove_skill("codex", &options).map(|_| ())
                }
            })
        };
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !worker.is_finished(),
            "wrote while another process held the lock"
        );
        assert_eq!(stamp(&dst), if write { OLD } else { VERSION });
        drop(holder);
        worker.join().unwrap().unwrap();
    }
    // The lock file outlives the uninstall but keeps no directory alive.
    assert!(!dst.parent().unwrap().exists());
    assert!(path.exists());
}

/// A copy another program embedding the command line installed names that
/// program in its commands and is otherwise the packaged skill: the refresh
/// keeps no backup of it, whichever of the two programs refreshes.
#[test]
fn a_copy_written_for_another_program_name_keeps_no_backup() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let other = graphicle::cli::text_for_program(PACKAGED, "embedding-cli").into_owned();
    assert_ne!(other, PACKAGED);
    let dst = stale(home, ".codex/skills/graphicle", OLD, &other);
    let (_, stderr) = version(home, &[]);
    assert_eq!(stderr.lines().collect::<Vec<_>>(), [refreshed(&dst)]);
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), PACKAGED);
    assert!(!dst.with_extension("md.bak").exists());
    // This program's copy, refreshed by the other one (in this process, with
    // every skill destination below the temporary home).
    std::fs::write(dst.parent().unwrap().join(".graphicle_version"), OLD).unwrap();
    let options = install::Options {
        home: home.to_path_buf(),
        project: false,
        claude_config_dir: None,
        local_appdata: None,
        ..Default::default()
    };
    let messages =
        graphicle::cli::with_program("embedding-cli", || install::refresh_stale_skills(&options));
    assert_eq!(messages, [refreshed(&dst)]);
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), other);
    assert!(!dst.with_extension("md.bak").exists());
}

#[test]
fn install_and_hook_commands_skip_the_refresh() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let dst = stale(home, ".codex/skills/graphicle", OLD, OLD_BODY);
    for command in ["hook-check", "hook-guard"] {
        graph_cli(home, &[command], &[]);
        assert_eq!(stamp(&dst), OLD, "{command}");
    }
}
