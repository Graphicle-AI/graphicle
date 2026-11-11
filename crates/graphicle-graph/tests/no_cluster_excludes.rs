use std::fs;

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

#[test]
fn incremental_exclusion_is_written_without_clustering() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("keep.py"), "def keep():\n    return 1\n").unwrap();
    fs::write(
        root.join("excluded.py"),
        "def exclude_me():\n    return 2\n",
    )
    .unwrap();

    let first = run(&[
        "extract",
        root.to_str().unwrap(),
        "--code-only",
        "--no-cluster",
    ]);
    assert_eq!(first.exit_code, 0, "{}{}", first.stdout, first.stderr);

    let graph_path = root.join(".graphicle/graph.json");
    let before: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&graph_path).unwrap()).unwrap();
    assert!(before["nodes"].as_array().unwrap().iter().any(|node| {
        node["source_file"]
            .as_str()
            .is_some_and(|path| path.ends_with("excluded.py"))
    }));

    let second = run(&[
        "extract",
        root.to_str().unwrap(),
        "--code-only",
        "--no-cluster",
        "--exclude",
        "excluded.py",
    ]);
    assert_eq!(second.exit_code, 0, "{}{}", second.stdout, second.stderr);
    assert!(second.stdout.contains("wrote "), "{}", second.stdout);
    assert!(
        !second.stdout.contains("outputs left untouched"),
        "{}",
        second.stdout
    );

    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(graph_path).unwrap()).unwrap();
    assert!(after["nodes"].as_array().unwrap().iter().any(|node| {
        node["source_file"]
            .as_str()
            .is_some_and(|path| path.ends_with("keep.py"))
    }));
    assert!(!after["nodes"].as_array().unwrap().iter().any(|node| {
        node["source_file"]
            .as_str()
            .is_some_and(|path| path.ends_with("excluded.py"))
    }));
}
