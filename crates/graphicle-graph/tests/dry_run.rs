//! `extract --dry-run` (beyond 0.9.77) estimates the LLM pass and leaves
//! nothing a later build would read: no graph, and no persisted `--docs`,
//! `--routes` or `--exclude`.
use std::fs;

#[test]
fn dry_run_writes_no_graph_and_persists_no_choices() {
    let dir = tempfile::tempdir().unwrap();
    let root = graphicle::extractors::resolution::resolved(dir.path());
    fs::write(root.join("app.py"), "def handler():\n    return 1\n").unwrap();
    fs::write(root.join("guide.md"), "# Guide\n\nCall `handler`.\n").unwrap();
    let out = graphicle::cli::run(&[
        "extract".to_string(),
        root.to_string_lossy().into_owned(),
        "--dry-run".to_string(),
        "--docs".to_string(),
        "--routes".to_string(),
        "--exclude".to_string(),
        "vendor".to_string(),
        "--backend".to_string(),
        "claude".to_string(),
    ]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("dry run"), "{}", out.stdout);
    assert!(out.stdout.contains("nothing was sent"), "{}", out.stdout);
    let out_dir = root.join(".graphicle");
    assert!(!out_dir.join("graph.json").exists());
    assert!(
        !out_dir.join(".graphicle_build.json").exists(),
        "{:?}",
        fs::read_to_string(out_dir.join(".graphicle_build.json"))
    );
}
