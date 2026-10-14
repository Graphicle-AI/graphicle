//! Regression tests for upstream issues that the native port already handles
//! at the pinned release. They were first confirmed by hand-run probes; these
//! tests keep them from regressing silently.
//!
//! - #1206: nested `.gitignore` files (including negation) are respected.
//! - #1313: Go cross-package `pkg.Func()` calls produce EXTRACTED `calls` edges.
//! - #462: pnpm workspace package imports resolve to the package's source file.
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Runs `extract <root> --code-only --no-cluster` and returns graph.json.
fn extract(root: &Path) -> Value {
    let out = graphicle::cli::run(&[
        "extract".to_string(),
        root.to_string_lossy().into_owned(),
        "--code-only".to_string(),
        "--no-cluster".to_string(),
    ]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
    let text = fs::read_to_string(root.join(".graphicle/graph.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = graphicle::extractors::resolution::resolved(dir.path());
    (dir, root)
}

fn nodes(graph: &Value) -> &[Value] {
    graph["nodes"].as_array().unwrap()
}

fn links(graph: &Value) -> &[Value] {
    graph
        .get("links")
        .or_else(|| graph.get("edges"))
        .and_then(Value::as_array)
        .unwrap()
}

/// Source files (relative, `/`-separated) that contributed at least one node.
fn source_files(graph: &Value, root: &Path) -> Vec<String> {
    let mut files = nodes(graph)
        .iter()
        .filter_map(|n| n["source_file"].as_str())
        .map(|s| {
            let p = Path::new(s);
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files
}

fn node_id<'a>(graph: &'a Value, label: &str, file_suffix: &str) -> &'a str {
    let matches = nodes(graph)
        .iter()
        .filter(|n| {
            n["label"].as_str() == Some(label)
                && n["source_file"]
                    .as_str()
                    .is_some_and(|s| s.replace('\\', "/").ends_with(file_suffix))
        })
        .filter_map(|n| n["id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "exactly one node {label:?} in {file_suffix}: {matches:?}"
    );
    matches[0]
}

#[test]
fn nested_gitignore_files_are_respected() {
    // Upstream #1206.
    let (_dir, root) = tempdir();
    write(&root, ".gitignore", "*.generated.py\n");
    write(&root, "app.py", "def top():\n    return 1\n");
    write(
        &root,
        "build.generated.py",
        "def generated():\n    return 2\n",
    );
    write(&root, "sub/.gitignore", "secret.py\n");
    write(&root, "sub/secret.py", "def hidden():\n    return 3\n");
    write(&root, "sub/visible.py", "def shown():\n    return 4\n");
    write(&root, "sub/deep/.gitignore", "*.py\n!keep.py\n");
    write(&root, "sub/deep/drop.py", "def dropped():\n    return 5\n");
    write(&root, "sub/deep/keep.py", "def kept():\n    return 6\n");
    let graph = extract(&root);
    let files = source_files(&graph, &root);
    for expected in ["app.py", "sub/visible.py", "sub/deep/keep.py"] {
        assert!(
            files.iter().any(|f| f == expected),
            "{expected} missing: {files:?}"
        );
    }
    for ignored in ["build.generated.py", "sub/secret.py", "sub/deep/drop.py"] {
        assert!(
            !files.iter().any(|f| f == ignored),
            "{ignored} not ignored: {files:?}"
        );
    }
}

#[test]
fn go_cross_package_calls_are_extracted() {
    // Upstream #1313: `util.Helper()` from another package.
    let (_dir, root) = tempdir();
    write(&root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        &root,
        "pkg/util/util.go",
        "package util\n\nfunc Helper() int {\n\treturn 1\n}\n",
    );
    write(
        &root,
        "cmd/app/main.go",
        "package main\n\nimport \"example.com/app/pkg/util\"\n\nfunc Run() int {\n\treturn util.Helper()\n}\n\nfunc main() {\n\tRun()\n}\n",
    );
    let graph = extract(&root);
    let caller = node_id(&graph, "Run()", "cmd/app/main.go");
    let callee = node_id(&graph, "Helper()", "pkg/util/util.go");
    let edge = links(&graph)
        .iter()
        .find(|e| {
            e["relation"] == "calls"
                && ((e["source"] == caller && e["target"] == callee)
                    || (e["_src"] == caller && e["_tgt"] == callee))
        })
        .unwrap_or_else(|| panic!("no calls edge {caller} -> {callee}"));
    assert_eq!(edge["confidence"], "EXTRACTED", "{edge}");
}

#[test]
fn pnpm_workspace_packages_resolve_to_source_files() {
    // Upstream #462: `import { X } from "@scope/types"` inside a pnpm workspace.
    let (_dir, root) = tempdir();
    write(
        &root,
        "package.json",
        "{\"name\":\"root\",\"private\":true}\n",
    );
    write(
        &root,
        "pnpm-workspace.yaml",
        "packages:\n  - 'packages/*'\n",
    );
    write(
        &root,
        "packages/types/package.json",
        "{\"name\":\"@scope/types\",\"version\":\"1.0.0\",\"main\":\"src/index.ts\",\"types\":\"src/index.ts\"}\n",
    );
    write(
        &root,
        "packages/types/src/index.ts",
        "export interface User {\n  id: string;\n}\n\nexport function makeUser(id: string): User {\n  return { id };\n}\n",
    );
    write(
        &root,
        "packages/app/package.json",
        "{\"name\":\"@scope/app\",\"version\":\"1.0.0\",\"dependencies\":{\"@scope/types\":\"workspace:*\"}}\n",
    );
    write(
        &root,
        "packages/app/src/main.ts",
        "import { makeUser } from \"@scope/types\";\n\nexport function start() {\n  return makeUser(\"a\");\n}\n",
    );
    let graph = extract(&root);
    let index_file = node_id(&graph, "index.ts", "packages/types/src/index.ts");
    let main_file = node_id(&graph, "main.ts", "packages/app/src/main.ts");
    let imports = links(&graph)
        .iter()
        .filter(|e| {
            e["relation"]
                .as_str()
                .is_some_and(|r| r.starts_with("imports"))
        })
        .filter(|e| e["source"] == main_file || e["_src"] == main_file)
        .filter_map(|e| e["target"].as_str())
        .collect::<Vec<_>>();
    assert!(
        imports.contains(&index_file),
        "main.ts imports {imports:?}, expected {index_file}"
    );
}
