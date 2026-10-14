//! Mirrors the 0.9.66 additions to upstream `tests/test_install.py` (#3668):
//! the CLAUDE.md skill registration must be idempotent and marker-anchored
//! rather than a bare substring check on the product name, and writing
//! must never translate the pre-existing file's line endings.
//!
//! The Rust port already registers through the marker-anchored
//! `register_block`, so these run unconditionally (no compat switch).
use graphicle::install;
use serde_json::json;
use std::fs;
use std::path::Path;

fn install_claude(home: &Path, project_dir: &Path) {
    let request = json!({
        "action": "install",
        "platform": "claude",
        "options": {
            "home": home,
            "project_dir": project_dir,
            "project": false,
            "windows": false,
            "claude_config_dir": null,
            "local_appdata": null,
            "executable": "graphicle-graph",
            "strict": false,
        }
    });
    install::dispatch(&request).expect("install claude");
}

#[test]
fn test_install_claude_md_does_not_skip_on_an_unrelated_mention_of_the_word() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let claude_md = home.join(".claude").join("CLAUDE.md");
    fs::create_dir_all(claude_md.parent().unwrap()).unwrap();
    fs::write(
        &claude_md,
        "See https://example.org/docs/graphicle for details.\n",
    )
    .unwrap();

    install_claude(&home, tmp.path());

    let content = fs::read_to_string(&claude_md).unwrap();
    assert!(
        content.contains("# graphicle\n"),
        "an unrelated mention of the word must not suppress the real registration block; got {content:?}"
    );
    assert!(
        content.contains("See https://example.org/docs/graphicle for details."),
        "the user's own pre-existing content must survive"
    );
}

#[test]
fn test_install_claude_md_refreshes_a_stale_registration_block() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let claude_md = home.join(".claude").join("CLAUDE.md");
    fs::create_dir_all(claude_md.parent().unwrap()).unwrap();
    fs::write(
        &claude_md,
        "# graphicle\n- an old, hand-edited line that does not match the current registration text\n",
    )
    .unwrap();

    install_claude(&home, tmp.path());

    let content = fs::read_to_string(&claude_md).unwrap();
    assert!(
        !content.contains("hand-edited line"),
        "the stale block must be replaced, not kept"
    );
    assert!(
        content.contains("Trigger: `/graphicle`"),
        "the current registration text must be written"
    );

    // Re-running is a no-op: the block is anchored on the H1 marker.
    let before = content.clone();
    install_claude(&home, tmp.path());
    assert_eq!(fs::read_to_string(&claude_md).unwrap(), before);
    assert_eq!(content.matches("# graphicle\n").count(), 1);
}

#[test]
fn test_register_always_on_block_writes_without_newline_translation() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("CLAUDE.md");
    fs::write(&target, "Some existing notes.\r\nSecond line.\r\n").unwrap();

    let registration = install::skill_registration("~/.claude/skills/graphicle/SKILL.md");
    let changed = install::dispatch(&json!({
        "action": "register_block",
        "path": target,
        "registration": registration,
    }))
    .expect("register_block");
    assert_eq!(changed, json!(true));

    let content = fs::read_to_string(&target).unwrap();
    assert!(
        content.starts_with("Some existing notes.\r\nSecond line."),
        "the file's own CRLF line endings must be preserved verbatim; got {content:?}"
    );
    // Graphify 0.9.88 (#4360): the merged block takes the file's own ending.
    assert!(content.contains("# graphicle\r\n"));
    assert_eq!(
        content.matches('\n').count(),
        content.matches("\r\n").count()
    );
}
