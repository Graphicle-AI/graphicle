//! `serve --watch`: the in-memory patch must preview exactly what the
//! background incremental rebuild then writes to .graphicle/graph.json.
use graphicle::{graph::Graph, serve::load_graph, serve_live::LiveGraph};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// The tests change the working directory (extract stores paths from it).
static CWD: Mutex<()> = Mutex::new(());

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    path
}

fn extract(root: &Path) {
    let out = graphicle::cli::run(&[
        "extract".to_string(),
        root.to_string_lossy().into_owned(),
        "--code-only".to_string(),
    ]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
}

/// Node ids and (source, target, relation) triples, the topology an agent sees.
fn topology(g: &Graph) -> (BTreeSet<String>, BTreeSet<(String, String, String)>) {
    let id = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let nodes = g.nodes().iter().map(|n| id(&n.id)).collect();
    let edges = g
        .iter_edges()
        .map(|(u, v, e)| {
            (
                id(&g.nodes()[u].id),
                id(&g.nodes()[v].id),
                id(e.attributes.get("relation").unwrap_or(&Value::Null)),
            )
        })
        .collect();
    (nodes, edges)
}

#[test]
fn live_patch_matches_the_background_rebuild() {
    let _cwd = CWD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = graphicle::extractors::resolution::resolved(dir.path());
    write(&root, "pkg/__init__.py", "");
    write(
        &root,
        "pkg/a.py",
        "def helper():\n    return 1\n\nclass Base:\n    def run(self):\n        return helper()\n",
    );
    let b = write(
        &root,
        "pkg/b.py",
        "from pkg.a import helper\n\ndef caller():\n    return helper()\n",
    );
    write(
        &root,
        "pkg/c.py",
        "from pkg.b import caller\n\ndef top():\n    return caller()\n",
    );
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&root).unwrap();
    extract(&root);
    let graph_path = root.join(".graphicle/graph.json");
    let live = LiveGraph::new(&graph_path, &root).unwrap();

    // Edit: a new function calling into another file, and one removed.
    fs::write(
        &b,
        "from pkg.a import helper\n\ndef fresh():\n    return helper()\n",
    )
    .unwrap();
    assert!(live.observe(&BTreeSet::from([b.clone()])).unwrap());
    let fast = topology(&live.context().graph);
    assert!(
        fast.0.iter().any(|id| id.ends_with("fresh")),
        "{:?}",
        fast.0
    );
    assert!(
        !fast.0.iter().any(|id| id.ends_with("_caller")),
        "{:?}",
        fast.0
    );
    live.settle().unwrap();
    let exact = topology(&live.context().graph);
    assert_eq!(exact, topology(&load_graph(&graph_path).unwrap()));
    assert_eq!(fast, exact);

    // Delete a file.
    fs::remove_file(root.join("pkg/c.py")).unwrap();
    assert!(
        live.observe(&BTreeSet::from([root.join("pkg/c.py")]))
            .unwrap()
    );
    let fast = topology(&live.context().graph);
    assert!(
        !fast.0.iter().any(|id| id.contains("pkg_c")),
        "{:?}",
        fast.0
    );
    live.settle().unwrap();
    assert_eq!(fast, topology(&live.context().graph));

    // Create a file that subclasses and calls into existing ones.
    let d = write(
        &root,
        "pkg/d.py",
        "from pkg.a import Base, helper\n\nclass Child(Base):\n    def go(self):\n        return helper()\n",
    );
    assert!(live.observe(&BTreeSet::from([d])).unwrap());
    let fast = topology(&live.context().graph);
    assert!(
        fast.0.iter().any(|id| id.ends_with("child")),
        "{:?}",
        fast.0
    );
    live.settle().unwrap();
    assert_eq!(fast, topology(&live.context().graph));
    assert_eq!(
        topology(&live.context().graph),
        topology(&load_graph(&graph_path).unwrap())
    );
    std::env::set_current_dir(previous).unwrap();
    assert!(live.stats().last_error.is_none());
}

/// Consecutive in-memory updates reuse the resolution context kept from the
/// previous one. That must not change them: the previews equal those of a
/// server that rebuilds the context for every update, and settling serves
/// exactly the rebuilt graph.json.
#[test]
fn consecutive_live_patches_reuse_the_context_transparently() {
    let _cwd = CWD.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::current_dir().unwrap();
    let run = |reuse: bool| {
        let dir = tempfile::tempdir().unwrap();
        let root = graphicle::extractors::resolution::resolved(dir.path());
        std::env::set_current_dir(&root).unwrap();
        write(&root, "pkg/__init__.py", "");
        write(
            &root,
            "pkg/a.py",
            "def helper():\n    return 1\n\nclass Base:\n    def run(self):\n        return helper()\n",
        );
        let b = write(
            &root,
            "pkg/b.py",
            "from pkg.a import helper\n\ndef caller():\n    return helper()\n",
        );
        let c = write(
            &root,
            "pkg/c.py",
            "from pkg.b import caller\n\ndef top():\n    return caller()\n",
        );
        let out = graphicle::cli::run(&[
            "extract".to_string(),
            root.to_string_lossy().into_owned(),
            "--code-only".to_string(),
            "--out".to_string(),
            root.to_string_lossy().into_owned(),
        ]);
        assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
        let graph_path = root.join(".graphicle/graph.json");
        let live = LiveGraph::new(&graph_path, &root).unwrap();
        live.set_reuse_context(reuse);
        let mut previews = vec![];
        for (path, text) in [
            (
                &b,
                "from pkg.a import helper, Base\n\ndef fresh():\n    return helper()\n\nclass Mid(Base):\n    def run(self):\n        return fresh()\n",
            ),
            (
                &c,
                "from pkg.b import fresh\n\ndef top():\n    return fresh()\n\ndef other():\n    return top()\n",
            ),
            (
                &b,
                "from pkg.a import helper, Base\n\ndef fresh():\n    return helper()\n\nclass Mid(Base):\n    pass\n",
            ),
        ] {
            fs::write(path, text).unwrap();
            assert!(live.observe(&BTreeSet::from([path.clone()])).unwrap());
            previews.push(topology(&live.context().graph));
        }
        live.settle().unwrap();
        let settled = topology(&live.context().graph);
        assert_eq!(settled, topology(&load_graph(&graph_path).unwrap()));
        assert!(live.stats().last_error.is_none());
        (previews, settled)
    };
    let (kept, settled_kept) = run(true);
    let (rebuilt, settled_rebuilt) = run(false);
    std::env::set_current_dir(previous).unwrap();
    assert_eq!(kept, rebuilt);
    assert_eq!(settled_kept, settled_rebuilt);
}
