// Adapted from Graphify 0.9.104, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_update_missing_graph.py, tests/test_update_js_resolution_inputs.py
//! Update changes ported from Graphify 0.9.104. Cases follow the upstream
//! tests named in each comment.
use graphicle::watch::{self, RebuildOptions};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, path::Path};

fn graph(dir: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(dir.join(".graphicle/graph.json")).unwrap()).unwrap()
}

fn rebuild(dir: &Path, incremental: bool) {
    let options = RebuildOptions {
        acquire_lock: false,
        no_cluster: true,
        incremental,
        ..Default::default()
    };
    let result = watch::rebuild(dir, &options).unwrap();
    assert!(result.success, "{:?}", result.messages);
}

#[test]
fn update_recreates_missing_graph_with_all_sources() {
    // test_update_missing_graph.py (#5016)
    for change_one_source in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.py");
        fs::write(&first, "def first(): return 1\n").unwrap();
        fs::write(dir.path().join("second.py"), "def second(): return 2\n").unwrap();
        rebuild(dir.path(), false);
        assert!(dir.path().join(".graphicle/manifest.json").is_file());
        fs::remove_file(dir.path().join(".graphicle/graph.json")).unwrap();
        if change_one_source {
            fs::write(&first, "def renamed(): return 123\n").unwrap();
        }
        rebuild(dir.path(), true);
        let labels = graph(dir.path())["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["label"].as_str().unwrap_or("").to_string())
            .collect::<BTreeSet<_>>();
        assert!(labels.contains("second()"), "{labels:?}");
        let expected = if change_one_source {
            "renamed()"
        } else {
            "first()"
        };
        assert!(labels.contains(expected), "{labels:?}");
    }
}

#[test]
fn update_refreshes_unchanged_typescript_importers() {
    // test_update_js_resolution_inputs.py test_update_refreshes_unchanged_typescript_importers (#5017)
    for change in ["edit", "remove", "add"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("main.ts"),
            "import { target } from \"@app/target\";\nexport function run() { return target(); }\n",
        )
        .unwrap();
        for name in ["old", "new"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::write(
                root.join(name).join("target.ts"),
                format!("export function target() {{ return \"{name}\"; }}\n"),
            )
            .unwrap();
        }
        let config = root.join("tsconfig.json");
        let write_config = |target: &str| {
            let text = json!({"compilerOptions": {"baseUrl": ".", "paths": {"@app/*": [format!("./{target}/*")]}}});
            fs::write(&config, text.to_string()).unwrap();
        };
        let local_imports = || {
            let data = graph(root);
            let file = |id: &Value| {
                data["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["id"] == *id)
                    .map(|n| n["source_file"].as_str().unwrap_or("").replace('\\', "/"))
                    .unwrap_or_default()
            };
            data["links"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|l| l["relation"] == "imports_from" && file(&l["source"]) == "main.ts")
                .map(|l| file(&l["target"]))
                .filter(|f| f == "old/target.ts" || f == "new/target.ts")
                .collect::<BTreeSet<_>>()
        };
        if change != "add" {
            write_config("old");
        }
        rebuild(root, false);
        let before: BTreeSet<String> = if change == "add" {
            BTreeSet::new()
        } else {
            BTreeSet::from(["old/target.ts".to_string()])
        };
        assert_eq!(local_imports(), before, "{change}");
        if change == "remove" {
            fs::remove_file(&config).unwrap();
        } else {
            write_config("new");
        }
        rebuild(root, true);
        let after: BTreeSet<String> = if change == "remove" {
            BTreeSet::new()
        } else {
            BTreeSet::from(["new/target.ts".to_string()])
        };
        assert_eq!(local_imports(), after, "{change}");
    }
}

#[test]
fn resolution_inputs_by_name() {
    for name in [
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "jsconfig.json",
        "tsconfig.json",
        "tsconfig.base.json",
        "TSCONFIG.JSON",
    ] {
        assert!(watch::is_resolution_input(Path::new(name)), "{name}");
    }
    for name in ["tsconfig", "tsconfig..json", "config.json", "go.mod"] {
        assert!(!watch::is_resolution_input(Path::new(name)), "{name}");
    }
}
