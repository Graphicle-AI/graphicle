//! The process-wide value table (`graphicle::vtab`) is never freed, so a
//! long-running `watch` or `serve` must not keep adding to it while the
//! repository only revisits states it has had: 100 incremental rebuild cycles
//! (a file flipping between two versions) and 100 serve reloads leave its size
//! where the first two cycles put it. Its own test binary, so no other test
//! adds values meanwhile.
use graphicle::watch::{RebuildOptions, rebuild};
use std::{fs, path::Path};

fn write(root: &Path, name: &str, body: &str) {
    let p = root.join(name);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn revisited_update_and_reload_cycles_do_not_grow_the_value_table() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(
        &root,
        "pkg/models.py",
        "class Item:\n    def __init__(self, name):\n        self.name = name\n\n    def label(self):\n        return self.name.title()\n",
    );
    let versions = [
        "from .models import Item\n\n\ndef make(name):\n    item = Item(name)\n    return item.label()\n",
        "from .models import Item\n\n\ndef make(name):\n    return Item(name).label()\n\n\ndef make_all(names):\n    return [make(n) for n in names]\n",
    ];
    write(&root, "pkg/service.py", versions[0]);
    write(
        &root,
        "cmd/main.go",
        "package main\n\nimport \"fmt\"\n\nfunc helper(x int) int { return x * 2 }\n\nfunc main() { fmt.Println(helper(2)) }\n",
    );
    assert!(rebuild(&root, &RebuildOptions::default()).unwrap().success);
    let graph = root.join(".graphicle/graph.json");
    let changed = RebuildOptions {
        changed_paths: Some(vec![root.join("pkg/service.py")]),
        ..Default::default()
    };
    let cycle = |i: usize| {
        write(&root, "pkg/service.py", versions[i % 2]);
        let result = rebuild(&root, &changed).unwrap();
        assert!(result.success && result.changed, "cycle {i}");
        let loaded = graphicle::serve::load_graph(&graph).unwrap();
        assert!(loaded.node_count() > 0);
    };
    // Warm-up: both versions once reach the steady state.
    cycle(1);
    cycle(2);
    let settled = graphicle::vtab::stats().values;
    assert!(settled > 0);
    for i in 3..103 {
        cycle(i);
    }
    assert_eq!(graphicle::vtab::stats().values, settled);
}
