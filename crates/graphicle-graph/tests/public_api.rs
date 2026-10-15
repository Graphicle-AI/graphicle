#[test]
fn top_level_api_builds_and_exports_a_native_extraction() {
    // All 17 names exposed by the original package are public native functions.
    let _ = graphicle::collect_files;
    let _ = graphicle::extract;
    let _ = graphicle::surprising_connections;
    let _ = graphicle::suggest_questions;
    let _ = graphicle::generate;
    let _ = graphicle::to_html;
    let _ = graphicle::to_svg;
    let _ = graphicle::to_canvas;
    let _ = graphicle::to_wiki;
    let _ = graphicle::reflect;
    let _ = graphicle::save_query_result;
    let mut data = serde_json::json!({
        "nodes":[{"id":"a","label":"Caller","file_type":"code"},{"id":"b","label":"Callee","file_type":"code"}],
        "edges":[{"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED"}],"hyperedges":[]
    });
    let mut diagnostics = graphicle::build::Diagnostics::default();
    let graph = graphicle::build_from_json(&mut data, true, None, &mut diagnostics).unwrap();
    let communities = graphicle::cluster(&graph, 1.0, None, "louvain").unwrap();
    assert!(!communities.is_empty());
    assert!(graphicle::score_all(&graph, &communities).is_object());
    assert!(graphicle::cohesion_score(&graph, &communities[0].1).is_finite());
    let _hubs = graphicle::god_nodes(&graph, 10, None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.json");
    assert!(
        graphicle::to_json(
            &graph,
            &communities,
            &path,
            &Default::default(),
            &mut diagnostics
        )
        .unwrap()
    );
    let exported: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(exported["nodes"].as_array().unwrap().len(), 2);
}

#[test]
fn incremental_extraction_reports_cache_traffic_and_equals_a_full_rebuild() {
    use graphicle::api::{ExtractOptions, extract, extract_with_report};
    use std::path::PathBuf;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("repo");
    let cache = dir.path().canonicalize().unwrap().join("cache");
    std::fs::create_dir_all(root.join("pkg")).unwrap();
    let write = |name: &str, text: &str| std::fs::write(root.join(name), text).unwrap();
    write("pkg/__init__.py", "");
    write(
        "pkg/core.py",
        "def compute(values):\n    return sum(values)\n",
    );
    write(
        "pkg/service.py",
        "from pkg.core import compute\n\n\ndef start(values):\n    return compute(values)\n",
    );
    write("notes.unknownext", "not extractable");
    let paths: Vec<PathBuf> = [
        "pkg/__init__.py",
        "pkg/core.py",
        "pkg/service.py",
        "notes.unknownext",
    ]
    .iter()
    .map(|p| root.join(p))
    .collect();
    let options = ExtractOptions {
        root: Some(root.clone()),
        cache_root: Some(cache),
        parallel: false,
        ..Default::default()
    };
    let canonical = |mut value: serde_json::Value| {
        for key in ["nodes", "edges"] {
            if let Some(items) = value[key].as_array_mut() {
                items.sort_by_key(|item| item.to_string());
            }
        }
        value
    };

    let (cold, report) = extract_with_report(&paths, &options).unwrap();
    assert_eq!(report.changed.len(), 3, "{report:?}");
    assert_eq!((report.unchanged, report.unsupported), (0, 1));

    let (warm, report) = extract_with_report(&paths, &options).unwrap();
    assert!(report.changed.is_empty(), "{report:?}");
    assert_eq!((report.unchanged, report.unsupported), (3, 1));
    assert_eq!(canonical(warm), canonical(cold));

    // A signature change in one file: only that file is re-parsed, yet the
    // importer's INFERRED edges are re-resolved, so the result equals an
    // uncached full extraction of the new tree.
    write(
        "pkg/core.py",
        "def compute_total(values):\n    return sum(values)\n",
    );
    write(
        "pkg/service.py",
        "from pkg.core import compute_total\n\n\ndef start(values):\n    return compute_total(values)\n",
    );
    let (incremental, report) = extract_with_report(&paths, &options).unwrap();
    // Compared as text: `/`-separated on Windows, as the engine reports paths.
    let mut changed: Vec<_> = report
        .changed
        .iter()
        .map(|p| {
            let rel = p
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if cfg!(windows) {
                rel.replace('\\', "/")
            } else {
                rel
            }
        })
        .collect();
    changed.sort();
    assert_eq!(changed, ["pkg/core.py", "pkg/service.py"]);
    assert_eq!((report.unchanged, report.unsupported), (1, 1));
    let full = extract(
        &paths,
        &ExtractOptions {
            use_cache: false,
            ..options.clone()
        },
    )
    .unwrap();
    assert!(
        full["edges"].as_array().unwrap().iter().any(|e| e["target"]
            .as_str()
            .is_some_and(|t| t.ends_with("compute_total"))),
        "the renamed symbol must be the resolution target"
    );
    assert_eq!(canonical(incremental), canonical(full));
}
