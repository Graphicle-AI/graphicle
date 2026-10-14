//! Mirrors the `watch.py` tests added upstream in Graphify 0.9.66
//! (`tests/test_watch.py::test_graphify_root_*`, #3375). The marker value
//! helper is exercised directly because `GRAPHICLE_OUT` is read once at
//! process start in the Rust port (the same shape as upstream's module
//! constant, which the Python tests monkeypatch).
use graphicle::watch::root_marker_value_for;
use std::path::Path;

#[test]
fn root_marker_is_resolved_when_the_output_folder_is_shared_and_absolute() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus).unwrap();
    let shared_out = tmp.path().join("shared-graphicle-out");
    let cwd = std::env::current_dir().unwrap();

    let saved = root_marker_value_for(Path::new("."), shared_out.to_str().unwrap()).unwrap();
    assert_eq!(
        saved,
        cwd.canonicalize().unwrap().to_string_lossy(),
        ".graphicle_root must be resolved when GRAPHICLE_OUT is absolute"
    );

    let relative = corpus.strip_prefix(&cwd).unwrap_or(&corpus);
    let saved = root_marker_value_for(relative, shared_out.to_str().unwrap()).unwrap();
    assert!(Path::new(&saved).is_absolute(), "{saved}");
    assert_eq!(saved, corpus.canonicalize().unwrap().to_string_lossy());
}

#[test]
fn root_marker_still_preserves_relative_when_the_output_folder_is_relative() {
    let saved = root_marker_value_for(Path::new("."), ".graphicle").unwrap();
    assert_eq!(
        saved, ".",
        "relative GRAPHICLE_OUT keeps the #777 behaviour"
    );
    let saved = root_marker_value_for(Path::new("corpus/sub"), ".graphicle").unwrap();
    assert_eq!(saved, "corpus/sub");
}
