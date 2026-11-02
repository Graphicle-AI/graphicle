//! `extract` runs that start worker threads succeed, run after run. On macOS
//! (Apple silicon) and on Windows (issue #18), mimalloc 3.3.2 crashed such a
//! run when memory was freed after the worker threads had exited
//! (microsoft/mimalloc#1306), so the executable does not use it there.
use serde_json::Value;
use std::{path::Path, process::Command};

/// The cross-file passes split their files or edges across worker threads
/// from 8 items on (`par_map` and `par_for_each_mut` in
/// src/extractors/parse_cache.rs).
const PARALLEL_FROM: usize = 8;

/// Python modules that each import from and call into the next one: enough
/// files, and far more edges, than the passes need to start their workers.
const MODULES: usize = PARALLEL_FROM;

fn write_project(dir: &Path) {
    for k in 0..MODULES {
        let next = (k + 1) % MODULES;
        let source = format!(
            "from mod_{next} import Worker{next}, helper_{next}\n\
             def helper_{k}(value):\n    return helper_{next}(value) + 1\n\
             class Worker{k}:\n    def run(self):\n        return Worker{next}().run() + helper_{k}(1)\n"
        );
        std::fs::write(dir.join(format!("mod_{k}.py")), source).unwrap();
    }
}

/// Node and edge counts of the graph a run wrote. A clustered graph is written
/// in node-link form, with its edges under `links` instead of `edges`.
fn written_graph(dir: &Path, run: usize) -> (usize, usize) {
    let path = dir.join(".graphicle/graph.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("run {run}: {} not written: {e}", path.display()));
    let graph: Value = serde_json::from_str(&text).unwrap();
    let count = |key: &str| graph[key].as_array().map_or(0, Vec::len);
    (count("nodes"), count("edges") + count("links"))
}

/// One TypeScript file, as in issue #18: twenty type aliases and twenty
/// functions that call each other, enough symbols in a single file for the
/// passes to start their workers.
fn write_typescript(dir: &Path) {
    let mut source = String::new();
    for k in 0..20 {
        source.push_str(&format!("export type T{k} = 'a' | 'b';\n"));
    }
    for k in 0..20 {
        let next = (k + 1) % 20;
        source.push_str(&format!(
            "export function f{k}(value: T{k}): number {{\n  return f{next}(value) + 1;\n}}\n"
        ));
    }
    std::fs::write(dir.join("index.ts"), source).unwrap();
}

/// `runs` code-only runs of `args` in a row, each a project's first run (as in
/// the runs that crashed): every run exits successfully and writes the same
/// graph, with more than `more_nodes_than` nodes and `PARALLEL_FROM` edges.
fn first_runs_succeed(project: &Path, args: &[&str], runs: usize, more_nodes_than: usize) {
    let mut first = None;
    for run in 1..=runs {
        let _ = std::fs::remove_dir_all(project.join(".graphicle"));
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
            .args(args)
            .current_dir(project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "run {run}: {out:?}");
        let counts = written_graph(project, run);
        let (nodes, edges) = counts;
        assert!(
            nodes > more_nodes_than && edges > PARALLEL_FROM,
            "run {run}: {nodes} nodes, {edges} edges"
        );
        assert_eq!(*first.get_or_insert(counts), counts, "run {run}");
    }
}

/// 40 runs of the Python project without clustering, as in the run that
/// crashed on macOS.
#[test]
fn extract_with_worker_threads_succeeds_every_run() {
    let project = tempfile::tempdir().unwrap();
    write_project(project.path());
    first_runs_succeed(
        project.path(),
        &["extract", ".", "--code-only", "--no-cluster"],
        40,
        MODULES,
    );
}

/// 20 runs of the TypeScript file with clustering, as in the runs that
/// crashed on Windows (issue #18).
#[test]
fn extract_of_one_typescript_file_succeeds_every_run() {
    let project = tempfile::tempdir().unwrap();
    write_typescript(project.path());
    first_runs_succeed(
        project.path(),
        &["extract", ".", "--code-only"],
        20,
        PARALLEL_FROM,
    );
}
