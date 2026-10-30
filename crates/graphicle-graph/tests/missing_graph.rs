//! A query run before any extract: the error names the missing graph file
//! and, for the default one, the extract command that builds it, and that
//! command makes the same query work.
use std::fs;

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

#[test]
fn a_query_before_any_extract_names_the_command_that_builds_the_graph() {
    let project = tempfile::tempdir().unwrap();
    // This test binary runs one test, so it may change the working directory,
    // where the default graph file (.graphicle/graph.json) lives.
    std::env::set_current_dir(project.path()).unwrap();
    fs::write(
        project.path().join("client.py"),
        "def retry():\n    pass\n\n\ndef fetch():\n    retry()\n",
    )
    .unwrap();
    let hint = "(build it with `graphicle-graph extract . --code-only` in this folder)";
    for args in [&["affected", "retry()"][..], &["query", "retry"]] {
        let out = run(args);
        assert_eq!(out.exit_code, 1, "{}{}", out.stdout, out.stderr);
        assert!(
            out.stderr.contains("graph file not found: "),
            "{}",
            out.stderr
        );
        assert!(out.stderr.contains(hint), "{}", out.stderr);
    }
    // A graph file named with --graph is the caller's own: the error names it
    // and suggests no command.
    let elsewhere = project.path().join("elsewhere.json");
    let out = run(&[
        "affected",
        "retry()",
        "--graph",
        elsewhere.to_str().unwrap(),
    ]);
    assert_eq!(out.exit_code, 1, "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains("graph file not found: "),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("--code-only"), "{}", out.stderr);
    // The command the error names builds the file the query reads.
    let out = run(&["extract", ".", "--code-only"]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
    let out = run(&["affected", "retry()"]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("fetch"), "{}", out.stdout);
}
