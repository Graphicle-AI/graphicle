// Adapted from Graphify 0.9.103 (16218bfc), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_4978_reconcile_cost.py
//! Extraction and resolution changes ported from Graphify 0.9.103. Cases
//! follow the upstream tests named in each comment.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::path::PathBuf;

#[allow(dead_code)]
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

#[allow(dead_code)]
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
    fn run(&self, paths: &[PathBuf]) -> Value {
        extract(
            paths,
            &ExtractOptions {
                root: Some(self.root.clone()),
                cache_root: Some(self.root.join("graphify-out")),
                parallel: false,
                ..Default::default()
            },
        )
        .unwrap()
    }
    fn extract_files(&self, files: &[(&str, &str)]) -> Value {
        let paths = files
            .iter()
            .map(|(rel, text)| self.write(rel, text))
            .collect::<Vec<_>>();
        self.run(&paths)
    }
}

#[allow(dead_code)]
fn nodes(result: &Value) -> &Vec<Value> {
    result["nodes"].as_array().unwrap()
}
#[allow(dead_code)]
fn edges(result: &Value) -> &Vec<Value> {
    result["edges"].as_array().unwrap()
}
/// The id of the node of `file` (root-relative) labelled `label`.
#[allow(dead_code)]
fn id_of(result: &Value, file: &str, label: &str) -> Option<String> {
    nodes(result)
        .iter()
        .find(|n| {
            n["label"] == label
                && n["source_file"]
                    .as_str()
                    .is_some_and(|s| std::path::Path::new(s).ends_with(file))
        })
        .and_then(|n| n["id"].as_str().map(str::to_owned))
}
/// Edges of `relation` from `source` to `target` (ids).
#[allow(dead_code)]
fn count(result: &Value, source: &str, target: &str, relation: &str) -> usize {
    edges(result)
        .iter()
        .filter(|e| e["source"] == source && e["target"] == target && e["relation"] == relation)
        .count()
}

#[test]
fn ownership_without_header_sniff_reads_no_header() {
    // test_4978_reconcile_cost.py (#4978): the reconcile asks only whether an
    // extractor owns a source; a `.h` is always owned, so it is not read, and
    // a missing or unreadable header still counts as owned.
    use graphicle::extract::{get_extractor, get_extractor_with};
    let f = Fixture::new();
    let missing = f.root.join("nowhere/big.h");
    assert!(get_extractor_with(&missing, false).is_some());
    let cpp = f.write("lib.h", "namespace n { class A {}; }\n");
    assert_eq!(get_extractor(&cpp).as_deref(), Some("cpp"));
    // The weak question never picks the grammar: a C++ header answers as C.
    assert_eq!(
        get_extractor_with(&cpp, false),
        get_extractor_with(&missing, false)
    );
    let objc = f.write("v.h", "@interface V\n@end\n");
    assert_eq!(get_extractor(&objc).as_deref(), Some("objc"));
    assert!(get_extractor_with(&f.root.join("notes.txt"), false).is_none());
}

#[test]
fn dotted_mention_resolves_when_method_name_is_a_builtin() {
    // test_markdown_code_spans.py (Graphify 0.9.103, 13e05097): only a bare
    // builtin-named mention is skipped; `Repository.open` resolves through
    // its qualifier like `Repository.connect`.
    let f = Fixture::new();
    let r = f.extract_files(&[
        (
            "src/repo.py",
            "class Repository:\n    def open(self):\n        return 1\n    def connect(self):\n        return 2\n",
        ),
        (
            "docs/notes.md",
            "# Notes\n\nSee `Repository.open` and `Repository.connect`.\n",
        ),
    ]);
    let notes = nodes(&r)
        .iter()
        .find(|n| n["label"] == "Notes")
        .unwrap()["id"]
        .clone();
    let cited: Vec<&Value> = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "references" && e["source"] == notes)
        .map(|e| &e["target"])
        .collect();
    for label in [".open()", ".connect()"] {
        let id = &nodes(&r)
            .iter()
            .find(|n| n["label"] == label && n["id"].as_str().unwrap().contains("repo"))
            .unwrap()["id"];
        assert!(cited.contains(&id), "{label}: {cited:?}");
    }
}
