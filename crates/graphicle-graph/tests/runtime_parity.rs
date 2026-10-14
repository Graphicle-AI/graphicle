//! The tree-sitter runtime decides how a file the grammar rejects is
//! recovered, and so what is extracted from it. Python Graphify parses with
//! py-tree-sitter 0.25.2, which bundles runtime 0.25.10 (commit da6fe9be);
//! Cargo.lock pins the same runtime. Runtime 0.26 changed the order in which
//! error recovery tries candidate reductions: under 0.27 the file below parses
//! to a single ERROR node, and `load()` and `SentinelType` were lost.
//!
//! Fixture: the reproduction from upstream issue #3540 (valid Swift that
//! tree-sitter-swift 0.7.3 rejects at line 9), and its tree as py-tree-sitter
//! 0.25.2 dumps it, one node per line: depth, kind, byte range, and flags for
//! named, error and missing nodes.
use std::{fmt::Write, fs, path::Path};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/runtime");

fn dump(node: tree_sitter::Node<'_>, depth: usize, out: &mut String) {
    writeln!(
        out,
        "{depth} {} {} {} {}{}{}",
        node.kind(),
        node.start_byte(),
        node.end_byte(),
        if node.is_named() { "N" } else { "-" },
        if node.is_error() { "E" } else { "-" },
        if node.is_missing() { "M" } else { "-" },
    )
    .unwrap();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        dump(child, depth + 1, out);
    }
}

#[test]
fn runtime_recovers_rejected_files_like_py_tree_sitter_0_25_2() {
    let source = fs::read(Path::new(FIXTURES).join("swift-3540.swift")).unwrap();
    let expected =
        fs::read_to_string(Path::new(FIXTURES).join("swift-3540.py-tree-sitter-0.25.2.tree"))
            .unwrap();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_swift::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(&source, None).unwrap();
    let mut actual = String::new();
    dump(tree.root_node(), 0, &mut actual);
    assert!(
        actual == expected,
        "the tree-sitter runtime recovers this file differently from py-tree-sitter 0.25.2 \
         (runtime 0.25.10); build with `--locked` so Cargo.lock's runtime is used"
    );
}

#[test]
fn swift_issue_3540_extracts_what_the_python_reference_extracts() {
    let dir = tempfile::tempdir().unwrap();
    let root = graphicle::extractors::resolution::resolved(dir.path());
    fs::copy(
        Path::new(FIXTURES).join("swift-3540.swift"),
        root.join("repro.swift"),
    )
    .unwrap();
    let out = graphicle::cli::run(&[
        "extract".to_string(),
        root.to_string_lossy().into_owned(),
        "--code-only".to_string(),
        "--no-cluster".to_string(),
    ]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
    let graph: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join(".graphicle/graph.json")).unwrap())
            .unwrap();
    let mut labels: Vec<&str> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["label"].as_str())
        .collect();
    labels.sort_unstable();
    // Python Graphify 0.9.76 on the same file (graphifyy with py-tree-sitter
    // 0.25.2): it still reports the syntax error, and keeps these symbols.
    assert_eq!(
        labels,
        [
            "Double",
            "Foundation",
            "Rings",
            "SentinelType",
            "fetchRings()",
            "load()",
            "repro.swift"
        ]
    );
}
