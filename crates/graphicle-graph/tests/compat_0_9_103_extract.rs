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

// --- A bare Python builtin binds to no same-named package class (#4989) -----

/// Edges of `relations` from a node of `from` to a node of `to`.
fn file_edges<'a>(r: &'a Value, from: &str, to: &str, relations: &[&str]) -> Vec<&'a Value> {
    let file_of = |id: &Value| {
        nodes(r)
            .iter()
            .find(|n| n["id"] == *id)
            .and_then(|n| n["source_file"].as_str())
            .unwrap_or("")
            .to_owned()
    };
    edges(r)
        .iter()
        .filter(|e| relations.contains(&e["relation"].as_str().unwrap_or("")))
        .filter(|e| file_of(&e["source"]) == from && file_of(&e["target"]) == to)
        .collect()
}

const ERRORS: &str = "class NotImplementedError(Exception):\n    pass\n";

fn builtin_case(files: &[(&str, &str)]) -> (Fixture, Value) {
    let f = Fixture::new();
    let mut all = vec![("pkg/__init__.py", ""), ("pkg/errors.py", ERRORS)];
    for file in files {
        all.retain(|(rel, _)| *rel != file.0);
        all.push(*file);
    }
    let r = f.extract_files(&all);
    (f, r)
}

#[test]
fn unimported_builtin_exception_has_no_cross_file_edge() {
    // test_python_builtin_call_shadowing_4989.py
    let (_f, r) = builtin_case(&[("pkg/a.py", "def todo():\n    raise NotImplementedError(\"x\")\n")]);
    assert!(file_edges(&r, "pkg/a.py", "pkg/errors.py", &["calls"]).is_empty());
}

#[test]
fn imported_builtin_exception_still_resolves_as_extracted() {
    let (_f, r) = builtin_case(&[(
        "pkg/b.py",
        "from pkg.errors import NotImplementedError\n\n\ndef todo2():\n    raise NotImplementedError(\"x\")\n",
    )]);
    let found = file_edges(&r, "pkg/b.py", "pkg/errors.py", &["calls"]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["confidence"], "EXTRACTED");
}

#[test]
fn other_symbol_from_same_module_does_not_certify_builtin() {
    let (_f, r) = builtin_case(&[
        (
            "pkg/errors.py",
            "class NotImplementedError(Exception):\n    pass\n\n\nclass ValueError(Exception):\n    pass\n",
        ),
        (
            "pkg/base.py",
            "from pkg.errors import ValueError\n\n\ndef todo3():\n    raise NotImplementedError(\"x\")\n",
        ),
    ]);
    assert!(file_edges(&r, "pkg/base.py", "pkg/errors.py", &["calls"]).is_empty());
}

#[test]
fn plain_module_import_does_not_bind_builtin_name() {
    let (_f, r) = builtin_case(&[(
        "pkg/c.py",
        "import pkg.errors\n\n\ndef todo4():\n    raise NotImplementedError(\"x\")\n",
    )]);
    assert!(file_edges(&r, "pkg/c.py", "pkg/errors.py", &["calls"]).is_empty());
}

#[test]
fn local_class_with_builtin_name_still_binds_in_file() {
    let (_f, r) = builtin_case(&[(
        "pkg/d.py",
        "class NotImplementedError(Exception):\n    pass\n\n\ndef todo5():\n    raise NotImplementedError(\"x\")\n",
    )]);
    let in_file: Vec<&Value> = file_edges(&r, "pkg/d.py", "pkg/d.py", &["calls"])
        .into_iter()
        .filter(|e| {
            nodes(&r)
                .iter()
                .any(|n| n["id"] == e["target"] && n["label"] == "NotImplementedError")
        })
        .collect();
    assert_eq!(in_file.len(), 1, "{in_file:?}");
    assert_eq!(in_file[0]["confidence"], "EXTRACTED");
    assert!(file_edges(&r, "pkg/d.py", "pkg/errors.py", &["calls"]).is_empty());
}

#[test]
fn builtin_exception_passed_by_name_has_no_cross_file_edge() {
    let (_f, r) = builtin_case(&[(
        "pkg/e.py",
        "def check(fn):\n    return fn\n\n\ndef todo6():\n    return check(NotImplementedError)\n",
    )]);
    assert!(file_edges(&r, "pkg/e.py", "pkg/errors.py", &["calls", "indirect_call"]).is_empty());
}

#[test]
fn non_builtin_unimported_name_keeps_existing_inferred_edge() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("pkg/__init__.py", ""),
        ("pkg/errors.py", "class MyError(Exception):\n    pass\n"),
        ("pkg/f.py", "def todo7():\n    raise MyError(\"x\")\n"),
    ]);
    let found = file_edges(&r, "pkg/f.py", "pkg/errors.py", &["calls"]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["confidence"], "INFERRED");
}

// --- TypeScript import types are no runtime dependencies (#4992) ------------

fn extract_js_text(suffix: &str, bytes: &[u8]) -> (tempfile::TempDir, Value) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("example{suffix}"));
    std::fs::write(&path, bytes).unwrap();
    let r = graphicle::extract_languages::extract_language(&path, "js");
    (dir, r)
}

fn runtime_edges(r: &Value) -> Vec<&Value> {
    edges(r)
        .iter()
        .filter(|e| e["relation"] == "dynamic_import" || e["deferred"] == true)
        .collect()
}

#[test]
fn type_import_does_not_emit_runtime_edge_or_package_path() {
    // test_4992_ts_import_types.py
    for suffix in [".ts", ".tsx", ".mts", ".cts"] {
        for source in [
            "export function accept(page: import('@playwright/test').Page): void {}",
            "function f(): import('@playwright/test').Page { throw 1; }",
            "type Page = import('@playwright/test').Page;",
            "type Module = typeof import('@playwright/test');",
            "interface Options { page: import('@playwright/test').Page }",
            "function f<T extends import('@playwright/test').Page>() {}",
            "function f<T = import('@playwright/test').Page>() {}",
            "function f() { const page: import('@playwright/test').Page = undefined; }",
            "f<import('@playwright/test').Page>();\nfunction after() {}",
            "f<typeof import('@playwright/test')>();\nfunction after() {}",
        ] {
            let (_dir, r) = extract_js_text(suffix, source.as_bytes());
            assert!(r.get("error").is_none(), "{suffix} {source}: {r}");
            assert!(runtime_edges(&r).is_empty(), "{suffix} {source}: {:?}", runtime_edges(&r));
            assert!(
                !nodes(&r).iter().any(|n| n["source_file"] == "@playwright/test"),
                "{suffix} {source}"
            );
            if source.contains("function after") {
                assert!(nodes(&r).iter().any(|n| n["label"] == "after()"), "{suffix} {source}");
            }
        }
    }
}

#[test]
fn runtime_import_survives_type_import_of_same_package() {
    for suffix in [".ts", ".tsx", ".mts", ".cts"] {
        for prefix in ["", "// caf\u{e9}\r\n", "\u{feff}// caf\u{e9}\r\n"] {
            let source = format!(
                "{prefix}export function load(page: import('@playwright/test').Page) {{\r\n  return import('@playwright/test');\r\n}}\r\n"
            );
            let (_dir, r) = extract_js_text(suffix, source.as_bytes());
            assert!(r.get("error").is_none(), "{suffix} {prefix:?}: {r}");
            let found = runtime_edges(&r);
            assert!(!found.is_empty(), "{suffix} {prefix:?}");
            let line = if prefix.is_empty() { "L2" } else { "L3" };
            assert!(
                found
                    .iter()
                    .any(|e| e["deferred"] == true && e["source_location"] == line),
                "{suffix} {prefix:?}: {found:?}"
            );
        }
    }
}

#[test]
fn runtime_comparison_import_is_not_a_type_import() {
    for suffix in [".js", ".ts", ".tsx"] {
        let (_dir, r) = extract_js_text(suffix, b"const x = a < b, import(\"runtime\") > (d);\n");
        assert!(r.get("error").is_none(), "{suffix}: {r}");
        assert!(!runtime_edges(&r).is_empty(), "{suffix}: {r}");
    }
}

// --- A call inside `_main` belongs to `_main`, not `main` (#4994) ------------

const TARGETS: &str = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n";
const CLI: &str =
    "from targets import alpha, beta\n\ndef _main():\n    return beta()\n\ndef main():\n    return alpha()\n";

/// `(source label, target label, line)` of every calls edge.
fn labelled_calls(r: &Value) -> std::collections::BTreeSet<(String, String, String)> {
    let label = |id: &Value| {
        nodes(r)
            .iter()
            .find(|n| n["id"] == *id)
            .and_then(|n| n["label"].as_str())
            .map(str::to_owned)
    };
    edges(r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .filter_map(|e| {
            Some((
                label(&e["source"])?,
                label(&e["target"])?,
                e["source_location"].as_str().unwrap_or("").to_owned(),
            ))
        })
        .collect()
}

fn triple(a: &str, b: &str, c: &str) -> (String, String, String) {
    (a.to_owned(), b.to_owned(), c.to_owned())
}

#[test]
fn private_main_call_not_attributed_to_public_main() {
    // test_python_imported_call_caller_collision.py
    let f = Fixture::new();
    let r = f.extract_files(&[("targets.py", TARGETS), ("cli.py", CLI)]);
    let calls = labelled_calls(&r);
    assert!(calls.contains(&triple("_main()", "beta()", "L4")), "{calls:?}");
    assert!(calls.contains(&triple("main()", "alpha()", "L7")), "{calls:?}");
    assert!(!calls.contains(&triple("main()", "beta()", "L4")), "{calls:?}");
    let id = |label: &str| {
        nodes(&r)
            .iter()
            .find(|n| n["label"] == label && n["source_file"].is_string())
            .map(|n| n["id"].clone())
    };
    assert_ne!(id("_main()"), id("main()"));
}

#[test]
fn underscore_sibling_without_collision_is_unchanged() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("targets.py", TARGETS),
        ("cli.py", "from targets import beta\n\ndef _helper():\n    return beta()\n"),
    ]);
    let calls = labelled_calls(&r);
    assert!(calls.contains(&triple("_helper()", "beta()", "L4")), "{calls:?}");
    assert!(!calls.iter().any(|(s, _, _)| s == "main()"));
}

#[test]
fn class_field_reference_stays_on_the_class_not_a_method() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("widget.ts", "export class Baz {}\n"),
        (
            "foo.ts",
            "import { Baz } from './widget';\nexport class Foo {\n  render(): number { return 1; }\n  item: Baz = new Baz();\n}\n",
        ),
    ]);
    let label = |id: &Value| {
        nodes(&r)
            .iter()
            .find(|n| n["id"] == *id)
            .and_then(|n| n["label"].as_str())
            .unwrap_or("")
            .to_owned()
    };
    let refs: Vec<(String, String)> = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "references")
        .map(|e| (label(&e["source"]), label(&e["target"])))
        .collect();
    assert!(refs.contains(&("Foo".into(), "Baz".into())), "{refs:?}");
    assert!(!refs.iter().any(|(s, t)| s.ends_with("render()") && t == "Baz"), "{refs:?}");
}

// --- C# preprocessor directives no longer derail the parse ------------------

fn cs_calls(files: &[(&str, &str)]) -> (Fixture, Value, std::collections::BTreeSet<(String, String)>) {
    let f = Fixture::new();
    let r = f.extract_files(files);
    let calls = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| {
            (
                e["source"].as_str().unwrap_or("").to_owned(),
                e["target"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    (f, r, calls)
}

/// The one method node `name` declared inside class `owner`.
fn member(r: &Value, owner: &str, name: &str) -> Value {
    let suffix = format!("_{owner}_{name}").to_lowercase();
    let found: Vec<&Value> = nodes(r)
        .iter()
        .filter(|n| {
            n["label"] == format!(".{name}()").as_str()
                && n["id"].as_str().unwrap_or("").ends_with(&suffix)
        })
        .collect();
    assert_eq!(found.len(), 1, "{owner}.{name}: {:?}", nodes(r));
    found[0].clone()
}

fn pair(a: &Value, b: &Value) -> (String, String) {
    (a["id"].as_str().unwrap().to_owned(), b["id"].as_str().unwrap().to_owned())
}

const PP_BUFFER: &str = "namespace Repro3;\n\npublic class Buffer\n{\n    public void Append(char c) { }\n}\n";
const PP_WRITER: &str = "namespace Repro3\n{\n    public class Writer\n    {\n        private readonly Buffer _buffer = new Buffer();\n        private int _state;\n\n        public int State\n        {\n            get\n            {\n                {\n                    return _state;\n                }\n            }\n        }\n\n        public string Path\n        {\n            get\n            {\n                {\n                    return \"\";\n                }\n            }\n        }\n\n        public void WriteValue(int typeCode)\n        {\n            {\n                switch (typeCode)\n                {\n#if HAVE_DB_NULL\n                    case 1:\n                        break;\n#endif\n                }\n            }\n        }\n\n        public void Flush()\n        {\n            Prepare();\n            _buffer.Append('x');\n        }\n\n        private void Prepare() { }\n    }\n}\n";

#[test]
fn preprocessor_if_does_not_orphan_later_members() {
    // test_csharp_member_calls.py (Graphify 0.9.103, 52a7acb6)
    let (_f, r, calls) = cs_calls(&[("Writer.cs", PP_WRITER), ("Buffer.cs", PP_BUFFER)]);
    let flush = member(&r, "Writer", "Flush");
    assert!(calls.contains(&pair(&flush, &member(&r, "Writer", "Prepare"))));
    assert!(
        calls.contains(&pair(&flush, &member(&r, "Buffer", "Append"))),
        "_buffer.Append() resolves through the field type"
    );
}

#[test]
fn preprocessor_blanking_keeps_member_line_numbers() {
    let (_f, r, _) = cs_calls(&[("Writer.cs", PP_WRITER), ("Buffer.cs", PP_BUFFER)]);
    for (name, decl) in [
        ("WriteValue", "public void WriteValue("),
        ("Flush", "public void Flush()"),
        ("Prepare", "private void Prepare()"),
    ] {
        let want = PP_WRITER.lines().position(|l| l.contains(decl)).unwrap() + 1;
        assert_eq!(member(&r, "Writer", name)["source_location"], format!("L{want}"), "{name}");
    }
}

#[test]
fn preprocessor_nested_split_signature_keeps_one_header() {
    let (_f, r, calls) = cs_calls(&[
        (
            "Sink.cs",
            "public class Fast { public void Run() {} }\npublic class Slow { public void Run() {} }\npublic class Old { public void Run() {} }\n",
        ),
        (
            "Host.cs",
            "public class Host\n{\n    private Fast _fast = new Fast();\n    private Slow _slow = new Slow();\n    private Old _old = new Old();\n#if NET\n  #if NET8_0_OR_GREATER\n    public void Write(System.ReadOnlySpan<char> s)\n  #else\n    public void Write(char[] s)\n  #endif\n#else\n    public void Write(string s)\n#endif\n    {\n#if NET\n        _fast.Run();\n  #if DEBUG\n        _slow.Run();\n  #else\n        _old.Run();\n  #endif\n#else\n        _old.Run();\n#endif\n    }\n\n    public void Flush() { _fast.Run(); }\n}\n",
        ),
    ]);
    let write = member(&r, "Host", "Write");
    let flush = member(&r, "Host", "Flush");
    assert_eq!(write["source_location"], "L8");
    assert_eq!(flush["source_location"], "L28");
    assert!(calls.contains(&pair(&write, &member(&r, "Fast", "Run"))));
    assert!(calls.contains(&pair(&write, &member(&r, "Slow", "Run"))), "nested first branch kept");
    assert!(calls.contains(&pair(&write, &member(&r, "Old", "Run"))), "balanced #else kept");
    assert!(calls.contains(&pair(&flush, &member(&r, "Fast", "Run"))));
}

#[test]
fn preprocessor_elif_else_redeclaring_a_member_are_blanked() {
    let (_f, r, calls) = cs_calls(&[
        (
            "Sink.cs",
            "public class A { public void Go() {} }\npublic class B { public void Go() {} }\npublic class C { public void Go() {} }\n",
        ),
        (
            "Host.cs",
            "public class Host\n{\n    private A _a = new A();\n    private B _b = new B();\n    private C _c = new C();\n#if HAVE_A\n    public void Run() { _a.Go(); }\n#elif HAVE_B\n    public void Run() { _b.Go(); }\n#else\n    public void Run() { _c.Go(); }\n#endif\n#region tail\n    public void Tail() { Run(); }\n#endregion\n}\n",
        ),
    ]);
    let run = member(&r, "Host", "Run");
    let tail = member(&r, "Host", "Tail");
    assert_eq!(run["source_location"], "L7");
    assert_eq!(tail["source_location"], "L14");
    assert!(calls.contains(&pair(&run, &member(&r, "A", "Go"))));
    for other in ["B", "C"] {
        assert!(!calls.contains(&pair(&run, &member(&r, other, "Go"))), "{other}");
    }
    assert!(calls.contains(&pair(&tail, &run)));
}

#[test]
fn preprocessor_unbalanced_else_type_header_is_blanked() {
    let (_f, r, calls) = cs_calls(&[
        ("Base.cs", "public class Base { public void Ping() {} }\n"),
        (
            "Host.cs",
            "public class Sink { public void Go() {} }\n#if NET\npublic class Host : Base\n{\n#else\npublic class Host\n{\n    public void OldOnly() { }\n#endif\n    private Sink _sink = new Sink();\n    public void Run() { _sink.Go(); Ping(); }\n}\n",
        ),
    ]);
    let run = member(&r, "Host", "Run");
    assert_eq!(run["source_location"], "L11");
    assert!(calls.contains(&pair(&run, &member(&r, "Sink", "Go"))));
    assert!(calls.contains(&pair(&run, &member(&r, "Base", "Ping"))));
    assert!(!nodes(&r).iter().any(|n| n["label"] == ".OldOnly()"));
}

#[test]
fn preprocessor_blanking_is_length_preserving() {
    use graphicle::extract_languages::blank_csharp_preprocessor;
    let src = b"class A {\r\n#if X\r\n  void F() {}\r\n#else\r\n  void G() {}\r\n#endif\r\n  // #if not a directive\r\n  string s = @\"\n#if inside a string\n\";\r\n}\r\n";
    let out = blank_csharp_preprocessor(src).unwrap();
    assert_eq!(out.len(), src.len());
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.matches('\n').count(), src.iter().filter(|b| **b == b'\n').count());
    assert!(text.contains("void F()") && text.contains("void G()"), "{text}");
    assert!(!text.contains("#endif") && !text.contains("#else"), "{text}");
    assert!(text.contains("#if inside a string") && text.contains("// #if not a directive"));
    assert_eq!(blank_csharp_preprocessor(b"class A {}\n"), None);
}
