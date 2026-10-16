//! `extract` runs that start worker threads succeed, run after run. On macOS
//! (Apple silicon), mimalloc 3.3.2 crashed such a run when memory was freed
//! after the worker threads had exited (microsoft/mimalloc#1306), so the
//! executable does not use it there.
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

/// Node and edge counts of the graph a run wrote.
fn written_graph(dir: &Path, run: usize) -> (usize, usize) {
    let path = dir.join(".graphicle/graph.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("run {run}: {} not written: {e}", path.display()));
    let graph: Value = serde_json::from_str(&text).unwrap();
    let count = |key: &str| graph[key].as_array().map_or(0, Vec::len);
    (count("nodes"), count("edges"))
}

/// 40 code-only runs in a row, each a project's first run (as in the run that
/// crashed): every run exits successfully and writes the same graph.
#[test]
fn extract_with_worker_threads_succeeds_every_run() {
    let project = tempfile::tempdir().unwrap();
    write_project(project.path());
    let mut first = None;
    for run in 1..=40 {
        let _ = std::fs::remove_dir_all(project.path().join(".graphicle"));
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
            .args(["extract", ".", "--code-only", "--no-cluster"])
            .current_dir(project.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "run {run}: {out:?}");
        let counts = written_graph(project.path(), run);
        let (nodes, edges) = counts;
        assert!(
            nodes > MODULES && edges > PARALLEL_FROM,
            "run {run}: {nodes} nodes, {edges} edges"
        );
        assert_eq!(*first.get_or_insert(counts), counts, "run {run}");
    }
}
