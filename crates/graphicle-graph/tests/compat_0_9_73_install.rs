//! Mirrors the 0.9.73 additions to upstream `tests/test_install.py` and the new
//! `tests/test_uninstall_symlink.py` (#3805): an instructions file that is a
//! symlink (e.g. `CLAUDE.md -> AGENTS.md` or a dotfiles checkout) is edited
//! through the link. Uninstall used to delete the link when removing graphicle's
//! section left the file empty, which left the section in the link target.
//!
//! Upstream also names the link target in its console lines (`_shown_path`);
//! the native installer reports through returned messages and does not print
//! per-file registration lines, so only the file-system contract is mirrored.
use graphicle::install;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

/// Link `link` to `target`. Windows: a file symlink needs Developer Mode or
/// the privilege; `false` there skips the case.
fn symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
}

fn options(home: &Path, project_dir: &Path, project: bool) -> Value {
    json!({
        "home": home,
        "project_dir": project_dir,
        "project": project,
        "windows": false,
        "claude_config_dir": null,
        "local_appdata": null,
        "executable": "graphicle-graph",
        "strict": false,
    })
}

/// (instructions file relative to the project, graphicle's heading, platform,
/// whether the cleanup runs through `uninstall` rather than `configure`).
const CASES: [(&str, &str, &str, bool); 6] = [
    (".claude/CLAUDE.md", "# graphicle", "claude", true),
    ("CLAUDE.md", "## graphicle", "claude", false),
    ("GEMINI.md", "## graphicle", "gemini", false),
    (
        ".github/copilot-instructions.md",
        "## graphicle",
        "vscode",
        false,
    ),
    ("AGENTS.md", "## graphicle", "agents", false),
    ("CODEBUDDY.md", "## graphicle", "codebuddy", false),
];

fn section(marker: &str) -> String {
    format!("{marker}\n\ngraphicle rules\n")
}

fn uninstall(project: &Path, platform: &str, via_uninstall: bool) {
    let home = project.parent().unwrap().join("home");
    fs::create_dir_all(&home).unwrap();
    let request = if via_uninstall {
        json!({"action": "uninstall", "platform": platform, "options": options(&home, project, true)})
    } else {
        json!({"action": "configure", "platform": platform, "remove": true, "options": options(&home, project, true)})
    };
    install::dispatch(&request).unwrap_or_else(|e| panic!("uninstall {platform}: {e}"));
}

#[test]
fn test_uninstall_empties_symlink_target_and_keeps_link() {
    for (rel, marker, platform, via_uninstall) in CASES {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let target = tmp.path().join("dotfiles").join("rules.md");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, section(marker)).unwrap();
        let link = project.join(rel);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        if !symlink(&target, &link) {
            return;
        }

        uninstall(&project, platform, via_uninstall);

        assert!(
            fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()),
            "{rel}: the user's symlink was deleted"
        );
        assert_eq!(
            fs::canonicalize(&link).unwrap(),
            fs::canonicalize(&target).unwrap(),
            "{rel}"
        );
        assert!(
            !fs::read_to_string(&target).unwrap().contains(marker),
            "{rel}: graphicle's section is still in the link target"
        );
    }
}

#[test]
fn test_uninstall_still_deletes_a_regular_file_left_empty() {
    for (rel, marker, platform, via_uninstall) in CASES {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let path = project.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, section(marker)).unwrap();

        uninstall(&project, platform, via_uninstall);

        assert!(
            !path.exists(),
            "{rel}: an emptied regular file must be deleted"
        );
    }
}

#[test]
fn test_install_claude_md_writes_through_the_symlink_target() {
    for existing in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let rules = tmp.path().join("rules.md");
        if existing {
            fs::write(&rules, "# my rules\n").unwrap();
        }
        let link = home.join(".claude").join("CLAUDE.md");
        if !symlink(&rules, &link) {
            return;
        }

        let request = json!({
            "action": "install",
            "platform": "claude",
            "options": options(&home, tmp.path(), false),
        });
        install::dispatch(&request).expect("install claude");

        assert!(
            fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()),
            "existing={existing}: the link itself must be left in place"
        );
        let text = fs::read_to_string(&rules).unwrap();
        assert!(
            text.contains("# graphicle\n"),
            "existing={existing}: {text:?}"
        );
        if existing {
            assert!(text.starts_with("# my rules\n"), "{text:?}");
        }
    }
}

#[test]
fn test_uninstall_claude_md_edits_the_symlink_target() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    let rules = tmp.path().join("rules.md");
    fs::write(&rules, "# my rules\n\n# graphicle\n- skill line\n").unwrap();
    let link = tmp.path().join(".claude").join("CLAUDE.md");
    if !symlink(&rules, &link) {
        return;
    }

    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let request = json!({
        "action": "uninstall",
        "platform": "claude",
        "options": options(&home, tmp.path(), true),
    });
    install::dispatch(&request).expect("uninstall claude");

    assert_eq!(fs::read_to_string(&rules).unwrap(), "# my rules\n");
    assert!(fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()));
}
