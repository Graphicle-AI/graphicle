// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
//! Extraction changes ported from Graphify 0.9.92-0.9.95, from the upstream
//! tests of those releases: YAML structural extraction (yaml_config.py) and
//! files an extractor declines by design (`skipped_sources`, the manifest's
//! `ast_empty`, #2879), a node per C# overload (#4584), T-SQL procedure
//! bodies and `GO` batches, redaction of credentials in rationale labels
//! (#4566) and JS/TS members that differ only by `_` (#4475).
use graphicle::api::{ExtractOptions, extract};
use graphicle::detect::{ManifestOptions, save_manifest};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        graphicle::rcfile::set_language_overrides(None);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    fn extract_files(&self, files: &[(&str, &str)]) -> Value {
        let paths = files
            .iter()
            .map(|(rel, text)| self.write(rel, text))
            .collect::<Vec<_>>();
        extract(
            &paths,
            &ExtractOptions {
                root: Some(self.root.clone()),
                cache_root: Some(self.root.join("graphify-out")),
                parallel: false,
                ..Default::default()
            },
        )
        .unwrap()
    }
}

fn nodes(result: &Value) -> &Vec<Value> {
    result["nodes"].as_array().unwrap()
}
fn edges(result: &Value) -> &Vec<Value> {
    result["edges"].as_array().unwrap()
}
fn label_of(result: &Value, id: &Value) -> String {
    nodes(result)
        .iter()
        .find(|n| n["id"] == *id)
        .and_then(|n| n["label"].as_str())
        .unwrap_or("")
        .to_string()
}
fn labels_in(result: &Value, file: &str) -> Vec<String> {
    nodes(result)
        .iter()
        .filter(|n| n["source_file"] == file)
        .map(|n| n["label"].as_str().unwrap_or("").to_string())
        .collect()
}
/// `(source label, target label)` of every edge of `relation`.
fn pairs(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| {
            (
                label_of(result, &e["source"]),
                label_of(result, &e["target"]),
            )
        })
        .collect()
}

#[test]
fn yaml_manifests_are_code_and_data_yaml_is_declined_by_design() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        (
            "deploy.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  replicas: 2\n",
        ),
        ("strings.yml", "greeting: hello\nfarewell: goodbye\n"),
    ]);
    assert_eq!(
        labels_in(&r, "deploy.yaml"),
        [
            "deploy.yaml",
            "apiVersion",
            "kind",
            "metadata",
            "name",
            "spec",
            "replicas"
        ]
    );
    assert!(
        nodes(&r)
            .iter()
            .filter(|n| n["source_file"] == "deploy.yaml")
            .all(|n| n["file_type"] == "code")
    );
    assert!(pairs(&r, "contains").contains(&("metadata".into(), "name".into())));
    // A flat bundle of strings is data: zero nodes, by design, not a failure.
    assert!(labels_in(&r, "strings.yml").is_empty());
    let skipped = r["skipped_sources"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0].as_str().unwrap().ends_with("strings.yml"));
    assert!(r["failed_sources"].as_array().unwrap().is_empty());
}

#[test]
fn a_declined_file_keeps_its_ast_empty_marker_until_it_fails() {
    let f = Fixture::new();
    let data = f.write("strings.yml", "greeting: hello\n");
    let code = f.write("a.py", "x = 1\n");
    let manifest = f.root.join("graphify-out/manifest.json");
    let mut groups = Map::new();
    groups.insert(
        "code".into(),
        json!([data.to_string_lossy(), code.to_string_lossy()]),
    );
    let save = |groups: &Map<String, Value>, options: ManifestOptions| -> Value {
        save_manifest(groups, &manifest, "ast", &options).unwrap();
        serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap()
    };
    let declined = vec![data.to_string_lossy().into_owned()];
    let saved = save(
        &groups,
        ManifestOptions {
            root: Some(f.root.clone()),
            ast_empty: Some(declined.clone()),
            ..Default::default()
        },
    );
    assert_eq!(saved["strings.yml"]["ast_empty"], true);
    assert!(
        saved["strings.yml"]["ast_hash"]
            .as_str()
            .is_some_and(|h| !h.is_empty())
    );
    assert!(saved["a.py"].get("ast_empty").is_none());
    // An unchanged rerun that does not list it keeps the marker.
    let saved = save(
        &groups,
        ManifestOptions {
            root: Some(f.root.clone()),
            ..Default::default()
        },
    );
    assert_eq!(saved["strings.yml"]["ast_empty"], true);
    // A genuine failure leaves the stamped set and is no intentional empty:
    // its kept row loses the marker and both hashes.
    let mut failed = Map::new();
    failed.insert("code".into(), json!([code.to_string_lossy()]));
    let saved = save(
        &failed,
        ManifestOptions {
            root: Some(f.root.clone()),
            clear_ast: Some(declined),
            ..Default::default()
        },
    );
    assert!(saved["strings.yml"].get("ast_empty").is_none());
    assert_eq!(saved["strings.yml"]["ast_hash"], "");
}

const MESSAGES: &str = "namespace Demo
{
    public static class Messages
    {
        public static string Banner(int code)
        {
            return Format(code);
        }

        public static string Banner(int code, string format)
        {
            return Pad(format);
        }

        private static string Format(int code) { return code.ToString(); }
        private static string Pad(string text) { return text; }
    }
}
";

const CALLER: &str = "namespace Demo
{
    public class Caller
    {
        public string OneArg() { return Messages.Banner(1); }
        public string TwoArgs() { return Messages.Banner(1, \"x\"); }
    }
}
";

#[test]
fn each_csharp_overload_is_a_node_and_calls_pick_by_argument_count() {
    let f = Fixture::new();
    let r = f.extract_files(&[("Messages.cs", MESSAGES), ("Caller.cs", CALLER)]);
    let banners = nodes(&r)
        .iter()
        .filter(|n| n["label"] == ".Banner()" && n["source_file"] == "Messages.cs")
        .map(|n| {
            (
                n["source_location"].as_str().unwrap().to_string(),
                n["id"].as_str().unwrap().to_string(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(banners.len(), 2, "{banners:?}");
    // The first declared overload keeps the plain id; the other is salted.
    let first = &banners["L5"];
    let second = &banners["L10"];
    assert!(second.starts_with(first.as_str()) && second != first);
    let calls = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| {
            (
                label_of(&r, &e["source"]),
                e["target"].as_str().unwrap().to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    assert!(calls.contains(&(".OneArg()".into(), first.clone())));
    assert!(calls.contains(&(".TwoArgs()".into(), second.clone())));
    // Each overload keeps its own callee.
    let id_of = |label: &str| {
        nodes(&r)
            .iter()
            .find(|n| n["label"] == label)
            .and_then(|n| n["id"].as_str())
            .unwrap()
            .to_string()
    };
    let by_source = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| {
            (
                e["source"].as_str().unwrap_or("").to_string(),
                e["target"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    assert!(by_source.contains(&(first.clone(), id_of(".Format()"))));
    assert!(by_source.contains(&(second.clone(), id_of(".Pad()"))));
}

#[test]
fn tsql_procedures_read_and_write_unbracketed_tables_across_go_batches() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "schema.sql",
        "CREATE TABLE [dbo].[Orders] (Id INT)\nGO\nCREATE TABLE [dbo].[Audit] (Id INT)\nGO\n\
         CREATE PROCEDURE [dbo].[ArchiveOrders] AS\nBEGIN\n  INSERT INTO [dbo].[Audit] (Id) SELECT Id FROM [dbo].[Orders];\nEND\nGO\n",
    )]);
    let labels = labels_in(&r, "schema.sql");
    for table in ["dbo.Orders", "dbo.Audit"] {
        assert!(labels.contains(&table.to_string()), "{labels:?}");
    }
    assert!(!labels.iter().any(|l| l.contains('[')), "{labels:?}");
    let procedure = labels
        .iter()
        .find(|l| l.contains("ArchiveOrders"))
        .unwrap_or_else(|| panic!("{labels:?}"))
        .clone();
    assert!(pairs(&r, "reads_from").contains(&(procedure.clone(), "dbo.Orders".into())));
    assert!(pairs(&r, "writes_to").contains(&(procedure, "dbo.Audit".into())));
}

#[test]
fn credentials_in_rationale_comments_are_redacted() {
    let f = Fixture::new();
    let token = format!("ghp_{}", "a".repeat(36));
    let r = f.extract_files(&[(
        "svc.py",
        &format!(
            "def connect():\n    # WHY: the staging token {token} is rotated weekly\n    return 1\n"
        ),
    )]);
    let labels = nodes(&r)
        .iter()
        .filter_map(|n| n["label"].as_str())
        .collect::<Vec<_>>();
    assert!(!labels.iter().any(|l| l.contains(&token)), "{labels:?}");
    assert!(
        labels.iter().any(|l| l.contains("[redacted]")),
        "{labels:?}"
    );
}

#[test]
fn js_members_that_differ_by_an_underscore_are_two_nodes() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "a.js",
        "export class A {\n  updateHash() { return 1; }\n  _updateHash() { return 2; }\n}\n\
         export function f() { const a = new A(); return a._updateHash(); }\n",
    )]);
    let methods = pairs(&r, "method");
    assert!(methods.contains(&("A".into(), ".updateHash()".into())));
    assert!(methods.contains(&("A".into(), "._updateHash()".into())));
    // Typed through the local `new A()`: INFERRED, on the exact member.
    let call = edges(&r)
        .iter()
        .find(|e| e["relation"] == "calls" && label_of(&r, &e["target"]) == "._updateHash()")
        .expect("a call to the exact member");
    assert_eq!(call["confidence"], "INFERRED");
}
