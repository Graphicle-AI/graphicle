//! Mirrors the 0.9.66 additions to upstream `tests/test_cargo_introspect.py`
//! (#3678 subdirectory discovery, inherited workspace dependency identity) and
//! `tests/test_cargo_missing_manifest.py` (#3677).
use graphicle::cargo_introspect::introspect_cargo;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn node_ids(result: &Value) -> BTreeSet<String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect()
}

fn edge_pairs(result: &Value) -> Vec<(String, String)> {
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_string(),
                e["target"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

#[test]
fn discovers_a_manifest_in_a_subdirectory_3678() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("src-tauri/Cargo.toml"), APP);
    let result = introspect_cargo(dir.path()).unwrap();
    assert_eq!(node_ids(&result), BTreeSet::from(["crate:app".to_string()]));
    let node = &result["nodes"][0];
    assert_eq!(node["source_file"], "src-tauri/Cargo.toml");
}

#[test]
fn discovery_resolves_workspace_members_from_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("src-tauri/Cargo.toml"),
        "[workspace]\nmembers = [\"core\"]\n",
    );
    write(
        &dir.path().join("src-tauri/core/Cargo.toml"),
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let result = introspect_cargo(dir.path()).unwrap();
    assert_eq!(
        node_ids(&result),
        BTreeSet::from(["crate:core".to_string()])
    );
    assert_eq!(
        result["nodes"][0]["source_file"],
        "src-tauri/core/Cargo.toml"
    );
}

#[test]
fn discovery_skips_noise_directories() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("node_modules/some-pkg/Cargo.toml"),
        "[package]\nname = \"decoy\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&dir.path().join("src-tauri/Cargo.toml"), APP);
    let result = introspect_cargo(dir.path()).unwrap();
    assert_eq!(node_ids(&result), BTreeSet::from(["crate:app".to_string()]));
}

#[test]
fn discovery_does_not_search_past_the_depth_cap() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("a/b/c/d/Cargo.toml"),
        "[package]\nname = \"too_deep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let err = introspect_cargo(dir.path()).expect_err("no manifest within depth 2 must still fail");
    assert!(err.to_string().starts_with("OSError"), "{err}");
}

fn inherited_rename_fixture(root_package: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root_header = if root_package {
        "[package]\nname = \"root-app\"\nversion = \"0.1.0\"\n"
    } else {
        ""
    };
    let root_dependency = if root_package {
        "[dependencies]\ndb.workspace = true\n"
    } else {
        ""
    };
    write(
        &dir.path().join("Cargo.toml"),
        &format!(
            "{root_header}[workspace]\nmembers = [\"app\", \"storage\"]\n\
             [workspace.dependencies]\ndb = {{ path = \"storage\", package = \"internal-storage\" }}\n\
             {root_dependency}"
        ),
    );
    write(
        &dir.path().join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\
         [dependencies]\ndb = { workspace = true, features = [\"fast\"] }\n",
    );
    write(
        &dir.path().join("storage/Cargo.toml"),
        "[package]\nname = \"internal-storage\"\nversion = \"0.1.0\"\n[features]\nfast = []\n",
    );
    dir
}

#[test]
fn inherited_rename_uses_workspace_dependency_identity() {
    for root_package in [false, true] {
        let dir = inherited_rename_fixture(root_package);
        let result = introspect_cargo(dir.path()).unwrap();
        let mut expected_sources = BTreeSet::from(["crate:app".to_string()]);
        if root_package {
            expected_sources.insert("crate:root-app".to_string());
        }
        let edges = result["edges"].as_array().unwrap();
        assert_eq!(edges.len(), expected_sources.len(), "{edges:?}");
        let pairs: BTreeSet<(String, String)> = edge_pairs(&result).into_iter().collect();
        let expected: BTreeSet<(String, String)> = expected_sources
            .iter()
            .map(|s| (s.clone(), "crate:internal-storage".to_string()))
            .collect();
        assert_eq!(pairs, expected);
        for edge in edges {
            let want = if edge["source"] == "crate:root-app" {
                "Cargo.toml"
            } else {
                "app/Cargo.toml"
            };
            assert_eq!(edge["source_file"], want);
            assert_eq!(edge["relation"], "crate_depends_on");
            assert_eq!(edge["context"], "cargo_dependency");
            assert_eq!(edge["confidence"], "EXTRACTED");
        }
    }
}

#[test]
fn inherited_dependency_requires_matching_local_path() {
    let cases: [(&str, &str, bool); 12] = [
        ("db = { path = \"db\" }", "db.workspace = true", true),
        ("db = \"1\"", "db.workspace = true", false),
        ("db = { version = \"1\" }", "db.workspace = true", false),
        (
            "db = { git = \"https://example.com/db\" }",
            "db.workspace = true",
            false,
        ),
        ("db = { path = \"missing\" }", "db.workspace = true", false),
        ("db = { path = \"app\" }", "db.workspace = true", false),
        ("db = { path = 42 }", "db.workspace = true", false),
        ("db = false", "db.workspace = true", false),
        ("", "db.workspace = true", false),
        ("db = { path = \"db\" }", "db.workspace = false", false),
        ("db = { path = \"db\" }", "db.workspace = \"true\"", false),
        (
            "db = { path = \"missing\" }",
            "db = { path = \"../db\" }",
            true,
        ),
    ];
    for (root_declaration, member_declaration, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("Cargo.toml"),
            &format!(
                "[workspace]\nmembers = [\"app\", \"db\"]\n[workspace.dependencies]\n{root_declaration}\n"
            ),
        );
        for name in ["app", "db"] {
            let deps = if name == "app" {
                format!("[dependencies]\n{member_declaration}\n")
            } else {
                String::new()
            };
            write(
                &dir.path().join(name).join("Cargo.toml"),
                &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{deps}"),
            );
        }
        let result = introspect_cargo(dir.path()).unwrap();
        let want = if expected {
            vec![("crate:app".to_string(), "crate:db".to_string())]
        } else {
            vec![]
        };
        assert_eq!(
            edge_pairs(&result),
            want,
            "root={root_declaration:?} member={member_declaration:?}"
        );
    }
}

#[test]
fn discovered_workspace_resolves_inherited_paths_from_its_manifest() {
    let fixture = inherited_rename_fixture(false);
    let scan = tempfile::tempdir().unwrap();
    std::fs::rename(fixture.path(), scan.path().join("nested")).unwrap();
    let result = introspect_cargo(scan.path()).unwrap();
    assert_eq!(
        edge_pairs(&result),
        vec![(
            "crate:app".to_string(),
            "crate:internal-storage".to_string()
        )]
    );
    assert_eq!(result["edges"][0]["source_file"], "nested/app/Cargo.toml");
}

#[test]
fn inherited_dependency_can_target_root_package() {
    for root_path in [".", ""] {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("Cargo.toml"),
            &format!(
                "[package]\nname = \"db\"\nversion = \"0.1.0\"\n[workspace]\nmembers = [\"app\"]\n\
                 [workspace.dependencies]\ndb = {{ path = \"{root_path}\" }}\n"
            ),
        );
        write(
            &dir.path().join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\ndb.workspace = true\n",
        );
        let result = introspect_cargo(dir.path()).unwrap();
        assert_eq!(
            edge_pairs(&result),
            vec![("crate:app".to_string(), "crate:db".to_string())],
            "root_path={root_path:?}"
        );
    }
}

#[test]
fn missing_or_malformed_workspace_dependencies_are_ignored() {
    for workspace in [
        "",
        "workspace = false\n",
        "[workspace]\n",
        "[workspace]\ndependencies = false\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("Cargo.toml"),
            &format!(
                "{workspace}[package]\nname = \"db\"\nversion = \"0.1.0\"\n[dependencies]\ndb.workspace = true\n"
            ),
        );
        let result = introspect_cargo(dir.path()).unwrap();
        assert_eq!(
            result["edges"],
            serde_json::json!([]),
            "workspace={workspace:?}"
        );
    }
}

// The two `--cargo` executable tests (#3677) live in cli/tests/cargo_flag.rs,
// the package that owns the executable.
