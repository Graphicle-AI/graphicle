//! The graph size cap and the engine's own large graphs (Graphicle's rule, not
//! upstream's): a graph.json the engine wrote is read for update, serve and
//! query whatever its size; a file from elsewhere over the cap is refused, and
//! so is the engine's own file once anything else changes it.
use graphicle::security::{
    MAX_GRAPH_FILE_BYTES, check_graph_file_size_cap, check_own_graph_size_cap, note_written_graph,
};
use std::fs;

fn set(key: &str, value: Option<&std::ffi::OsStr>) {
    // SAFETY: this test binary runs one test, and no other thread reads the
    // environment while it changes.
    unsafe {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

#[test]
fn a_large_graph_the_engine_wrote_is_read_and_one_from_elsewhere_is_not() {
    let home = tempfile::tempdir().unwrap();
    set("HOME", Some(home.path().as_os_str()));
    set("GRAPHICLE_MAX_GRAPH_BYTES", None);
    let project = tempfile::tempdir().unwrap();
    let out = project.path().join(".graphicle");
    fs::create_dir_all(&out).unwrap();
    let graph = out.join("graph.json");
    let big = MAX_GRAPH_FILE_BYTES + 1;
    // Sparse files: the size without the disk use.
    let resize = |len: u64| {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&graph)
            .unwrap()
            .set_len(len)
            .unwrap()
    };
    resize(big);
    // Shipped by a repository, copied in or written by an earlier release:
    // refused, as before, with the one-time rebuild that gives it a record.
    let refused = check_own_graph_size_cap(&graph).unwrap_err().to_string();
    assert!(refused.contains("exceeds"), "{refused}");
    assert!(
        refused.contains("GRAPHICLE_MAX_GRAPH_BYTES=<N>GB graphicle-graph extract --force"),
        "{refused}"
    );
    // Written by the engine: read for update, serve and query.
    note_written_graph(&graph);
    assert!(check_own_graph_size_cap(&graph).is_ok());
    let records = home.path().join(".graphicle-graph/graph-stamps");
    assert_eq!(records.read_dir().unwrap().count(), 1);
    // The plain cap still applies where the size decides the view (export).
    assert!(check_graph_file_size_cap(&graph).is_err());
    // Changed by anything else: no longer the engine's file.
    resize(big + 1);
    assert!(check_own_graph_size_cap(&graph).is_err());
    // An explicit setting wins over the record.
    note_written_graph(&graph);
    assert!(check_own_graph_size_cap(&graph).is_ok());
    set("GRAPHICLE_MAX_GRAPH_BYTES", Some("1MB".as_ref()));
    let refused = check_own_graph_size_cap(&graph).unwrap_err().to_string();
    assert!(!refused.contains("extract --force"), "{refused}");
    set("GRAPHICLE_MAX_GRAPH_BYTES", None);
    // A small graph written at the same path drops the record.
    graphicle::paths::write_text_atomic(&graph, "{}").unwrap();
    resize(big);
    assert!(check_own_graph_size_cap(&graph).is_err());
    // The rebuild renames its temporary graph over graph.json: recorded too.
    let tmp = out.join(".graph.rebuild.tmp.json");
    fs::File::create(&tmp).unwrap().set_len(big).unwrap();
    graphicle::paths::os_replace_with_fallback(&tmp, &graph).unwrap();
    assert!(check_own_graph_size_cap(&graph).is_ok());
    // Only graph.json files get a record.
    let other = out.join("other.json");
    fs::File::create(&other).unwrap().set_len(big).unwrap();
    note_written_graph(&other);
    assert!(check_own_graph_size_cap(&other).is_err());
}
