//! One corpus gives the same graph on every run: whatever the number of
//! workers, wherever the output folder (`GRAPHICLE_OUT`) and the project are.
//! An Office document is converted to a Markdown sidecar in the output folder;
//! when that folder was outside the project, the sidecar's nodes were named
//! after the folder, so each output folder gave a different graph.
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[allow(dead_code)]
mod common;

/// A Word document with two headings that the notes mention code under.
fn docx() -> Vec<u8> {
    common::word_document(&[
        (Some("Heading1"), "Release notes"),
        (None, "The parser now reads every file."),
        (Some("Heading2"), "Usage"),
        (None, "Call `parse_all()` before `Report.render()`."),
    ])
}

fn write_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(
        dir.join("parser.py"),
        "def parse_all(paths):\n    return [parse(p) for p in paths]\n\n\ndef parse(path):\n    return Report(path)\n\n\nclass Report:\n    def __init__(self, path):\n        self.path = path\n\n    def render(self):\n        return str(self.path)\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.py"),
        "from parser import parse_all\n\n\ndef main():\n    return [r.render() for r in parse_all(['a', 'b'])]\n",
    )
    .unwrap();
    std::fs::write(dir.join("docs/notes.docx"), docx()).unwrap();
}

/// The files of the output folder that describe the graph, read after the
/// harness's three commands (an extract of the code, a first update, an
/// update with nothing changed) run with `workers` workers.
fn outputs(project: &Path, out: &Path, workers: &str) -> Vec<(String, Option<Vec<u8>>)> {
    outputs_as(project, out.as_os_str(), out, workers)
}

/// [`outputs`] with `GRAPHICLE_OUT` set to `setting`, which leads to `out`.
fn outputs_as(
    project: &Path,
    setting: &std::ffi::OsStr,
    out: &Path,
    workers: &str,
) -> Vec<(String, Option<Vec<u8>>)> {
    let home = tempfile::tempdir().unwrap();
    let source = project.to_string_lossy().into_owned();
    for args in [
        // The harness starts with a code-only extract, which keeps the graph
        // code-only (Graphify 0.9.101, #4923): an update builds it here so
        // the Office document is in it.
        vec!["update", &source],
        vec!["update", &source],
        vec!["update", &source],
    ] {
        let run = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
            .args(&args)
            .current_dir(home.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home.path())
            .env("GRAPHICLE_OUT", setting)
            .env("GRAPHICLE_MAX_WORKERS", workers)
            .env("RAYON_NUM_THREADS", workers)
            .output()
            .unwrap();
        assert!(run.status.success(), "{args:?}: {run:?}");
    }
    [
        "graph.json",
        "GRAPH_REPORT.md",
        ".graphicle_labels.json",
        ".graphicle_labels.pending.json",
        ".graphicle_labels.json.sig",
    ]
    .iter()
    .map(|name| (name.to_string(), std::fs::read(out.join(name)).ok()))
    .collect()
}

#[test]
fn every_run_writes_the_same_graph() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    // The project at two depths, its output folder beside it or six folders up.
    let shallow = base.join("p/project");
    let deep = base.join("q/r/s/t/u/project");
    for project in [&shallow, &deep] {
        write_project(project);
    }
    let runs: [(&PathBuf, PathBuf, &str); 4] = [
        (&shallow, base.join("p/outa"), "1"),
        (&shallow, base.join("p/outa"), "4"),
        (&shallow, base.join("p/outb/graph"), "4"),
        (&deep, base.join("outc"), "2"),
    ];
    let mut first: Option<Vec<(String, Option<Vec<u8>>)>> = None;
    for (project, out, workers) in runs {
        let _ = std::fs::remove_dir_all(&out);
        let written = outputs(project, &out, workers);
        let graph =
            String::from_utf8_lossy(written[0].1.as_deref().expect("graph.json")).into_owned();
        // The Word document's headings are in the graph, nothing in it names
        // the output folder, and the document is named by its sidecar.
        assert!(graph.contains("release_notes"), "{}", out.display());
        for name in ["outa", "outb", "outc"] {
            assert!(!graph.contains(name), "{name} in {}", out.display());
        }
        assert!(graph.contains("ext_notes_"), "{}", out.display());
        match &first {
            None => first = Some(written),
            Some(first) => {
                for ((name, a), (_, b)) in first.iter().zip(&written) {
                    assert!(
                        a == b,
                        "{name} differs: {} with {workers} workers",
                        out.display()
                    );
                }
            }
        }
    }
}

/// A relative output folder that leads outside the project (`../out`) gives
/// the graph an absolute output folder there gives: the Office document is
/// named by its sidecar, not by the folder.
#[test]
fn a_relative_output_folder_outside_the_project_writes_the_same_graph() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("p/project");
    write_project(&project);
    let absolute = outputs(&project, &base.join("p/outa"), "2");
    for (setting, out) in [
        ("../outd", base.join("p/outd")),
        ("../../oute/graph", base.join("oute/graph")),
    ] {
        let written = outputs_as(&project, std::ffi::OsStr::new(setting), &out, "2");
        let graph =
            String::from_utf8_lossy(written[0].1.as_deref().expect("graph.json")).into_owned();
        assert!(graph.contains("release_notes"), "{setting}");
        for name in ["outd", "oute"] {
            assert!(!graph.contains(name), "{name} in the graph of {setting}");
        }
        for ((name, a), (_, b)) in absolute.iter().zip(&written) {
            assert!(a == b, "{name} differs with GRAPHICLE_OUT={setting}");
        }
    }
}
