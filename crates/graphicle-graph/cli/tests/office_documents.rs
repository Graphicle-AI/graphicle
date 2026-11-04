//! Office documents are converted to Markdown sidecars in the output folder
//! (`<output>/converted/`), and the graph names a document by its sidecar:
//! by its path when the output folder is in the project, by its file name
//! when the folder is outside it.
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[allow(dead_code)]
mod common;

fn write_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(
        dir.join("parser.py"),
        "def parse_all(paths):\n    return [parse(p) for p in paths]\n\n\ndef parse(path):\n    return path\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("docs/notes.docx"),
        common::word_document(&[
            (Some("Heading1"), "Release notes"),
            (None, "Call `parse_all()` on every file."),
        ]),
    )
    .unwrap();
}

/// The engine with `args` on a project, with `GRAPHICLE_OUT` set to
/// `setting` (unset: the output folder is `.graphicle` in the project).
fn run(home: &Path, setting: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"));
    command
        .args(args)
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home);
    if let Some(setting) = setting {
        command.env("GRAPHICLE_OUT", setting);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    output
}

/// The sidecars in `<out>/converted/`.
fn sidecars(out: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(out.join("converted"))
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// When an Office document goes, its nodes go, while its old sidecar stays
/// in `<output>/converted/`: with the output folder in the project, beside
/// it, more than three folders up, or a relative one that leads outside it.
#[test]
fn a_deleted_document_takes_its_nodes_with_it() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let project = base.join("a/b/c/d/project");
    let beside = base.join("a/b/c/d/beside");
    let far = base.join("far");
    let spelled = |path: &Path| path.to_string_lossy().into_owned();
    for (setting, out) in [
        (None, project.join(".graphicle")),
        (Some(spelled(&beside)), beside.clone()),
        (Some(spelled(&far)), far.clone()),
        (
            Some("../relative".to_string()),
            base.join("a/b/c/d/relative"),
        ),
    ] {
        let setting = setting.as_deref();
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&out);
        write_project(&project);
        let source = spelled(&project);
        run(
            &home,
            setting,
            &["extract", &source, "--code-only", "--force"],
        );
        run(&home, setting, &["update", &source]);
        let graph = std::fs::read_to_string(out.join("graph.json")).unwrap();
        assert!(graph.contains("release_notes"), "{setting:?}: {graph}");
        std::fs::remove_file(project.join("docs/notes.docx")).unwrap();
        run(&home, setting, &["update", &source]);
        assert_eq!(sidecars(&out).len(), 1, "{setting:?}");
        let graph = std::fs::read_to_string(out.join("graph.json")).unwrap();
        assert!(!graph.contains("release_notes"), "{setting:?}: {graph}");
        assert!(!graph.contains("notes_"), "{setting:?}: {graph}");
        assert!(graph.contains("parse_all"), "{setting:?}: {graph}");
    }
}
