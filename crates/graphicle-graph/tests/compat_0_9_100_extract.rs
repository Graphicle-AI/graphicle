// Adapted from Graphify 0.9.100, Apache-2.0; see NOTICE.md.
//! Extraction changes ported from Graphify 0.9.100, from the upstream tests
//! of that release: PHP `Scope::MEMBER` outside function bodies (#4648),
//! case-only declaration ids (#4904), one JVM namespace for Java and Kotlin
//! (#4874), Scala extension methods (#2044), Terraform scope ids (#4858),
//! and fixes for R, Elixir, Dart and Fortran.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
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

fn nodes(result: &Value) -> &Vec<Value> {
    result["nodes"].as_array().unwrap()
}
fn edges(result: &Value) -> &Vec<Value> {
    result["edges"].as_array().unwrap()
}
fn node<'a>(result: &'a Value, id: &Value) -> Option<&'a Value> {
    nodes(result).iter().find(|n| n["id"] == *id)
}
fn label_of(result: &Value, id: &Value) -> String {
    node(result, id)
        .and_then(|n| n["label"].as_str())
        .unwrap_or("")
        .to_string()
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


fn labels(result: &Value) -> BTreeSet<String> {
    nodes(result)
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}
fn node_by<'a>(result: &'a Value, label: &str, suffix: &str) -> &'a Value {
    let found = nodes(result)
        .iter()
        .filter(|n| n["label"] == label)
        .filter(|n| n["source_file"].as_str().unwrap_or("").replace('\\', "/").ends_with(suffix))
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1, "{label} {suffix}");
    found[0]
}
fn has_edge(result: &Value, source: &Value, relation: &str, target: &Value) -> bool {
    edges(result).iter().any(|e| {
        e["relation"] == relation && e["source"] == source["id"] && e["target"] == target["id"]
    })
}

// --- PHP Scope::MEMBER outside function bodies (#4648) ------------------------

const ROLE: &str = "enum Role {\n    case Viewer;\n    case Admin;\n}\n";

#[test]
fn php_class_level_scope_members_are_referenced() {
    for body in [
        "class Catalog {\n    private const array MAP = ['posts.read' => Role::Viewer];\n}\n",
        "class Catalog {\n    private Role $fallback = Role::Viewer;\n}\n",
        "class Catalog {\n    public function grant(Role $r = Role::Admin) {}\n}\n",
    ] {
        let f = Fixture::new();
        let r = f.extract_files(&[("Perms.php", &format!("<?php\nnamespace App;\n{ROLE}{body}"))]);
        let refs = pairs(&r, "references_constant");
        assert!(refs.contains(&("Catalog".into(), "Role".into())), "{body}: {refs:?}");
    }
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "Perms.php",
        "<?php\nclass Catalog {\n    const A = self::B;\n    const B = static::C;\n    const C = \\Vendor\\Unknown::X;\n}\n",
    )]);
    assert!(pairs(&r, "references_constant").is_empty(), "{:?}", pairs(&r, "references_constant"));
}

// --- A type and a function differing only in case stay apart (#4904) ---------

#[test]
fn case_only_type_and_function_keep_two_nodes() {
    let f = Fixture::new();
    let r = f.extract_files(&[("m.ts", "export class Foo {}\nexport function foo() { return 1; }\n")]);
    let l = labels(&r);
    assert!(l.contains("Foo") && l.contains("foo()"), "{l:?}");
    let f = Fixture::new();
    let r = f.extract_files(&[("m.ts", "export class Foo {}\nexport namespace Foo { export const x = 1; }\n")]);
    let foo = nodes(&r).iter().filter(|n| n["label"] == "Foo").count();
    assert_eq!(foo, 1, "class and namespace Foo merge");
}

// --- JVM: Java and Kotlin share one namespace (#4874) -------------------------

#[test]
fn jvm_references_cross_java_and_kotlin() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("src/Logger.java", "package com.example.util;\npublic class Logger { public static void info(String m) { System.out.println(m); } }\n"),
        ("src/BaseScreen.kt", "package com.example.ui\nopen class BaseScreen { fun show() {} }\n"),
        ("src/JavaUser.java", "package com.example.app;\nimport com.example.util.Logger;\nimport com.example.ui.BaseScreen;\npublic class JavaUser extends BaseScreen { public void run() { Logger.info(\"java\"); } }\n"),
        ("src/KotlinUser.kt", "package com.example.app\nimport com.example.util.Logger\nimport com.example.ui.BaseScreen\nclass KotlinUser : BaseScreen() { fun run() { Logger.info(\"kotlin\") } }\n"),
    ]);
    let logger = node_by(&r, "Logger", "Logger.java");
    let base = node_by(&r, "BaseScreen", "BaseScreen.kt");
    let java_user = node_by(&r, "JavaUser", "JavaUser.java");
    assert!(has_edge(&r, java_user, "inherits", base), "{:?}", pairs(&r, "inherits"));
    let kotlin_file = node_by(&r, "KotlinUser.kt", "KotlinUser.kt");
    assert!(has_edge(&r, kotlin_file, "imports", logger), "{:?}", pairs(&r, "imports"));
    let info = node_by(&r, ".info()", "Logger.java");
    let calls = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls" && e["target"] == info["id"])
        .filter_map(|e| node(&r, &e["source"]))
        .map(|n| n["source_file"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert!(calls.iter().any(|f| f.ends_with("KotlinUser.kt")), "{calls:?}");
}

// --- Single-language fixes ------------------------------------------------------

#[test]
fn r_contains_is_an_inheritance_keyword() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "classes.R",
        "setClass(\"Base\", representation(x = \"numeric\"))\nsetClass(\"Derived\", contains = \"Base\")\n",
    )]);
    assert!(pairs(&r, "inherits").iter().any(|(s, t)| s.contains("Derived") && t.contains("Base")), "{:?}", pairs(&r, "inherits"));
}

#[test]
fn elixir_bare_call_reaches_its_own_modules_def() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "lib.ex",
        "defmodule A do\n  def helper, do: 1\n  def run, do: helper()\nend\n\ndefmodule B do\n  def helper, do: 2\nend\n",
    )]);
    let a_run = node_by(&r, "run()", "lib.ex");
    let targets = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls" && e["source"] == a_run["id"])
        .map(|e| e["target"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert!(targets[0].starts_with(&a_run["id"].as_str().unwrap().trim_end_matches("_run").to_string()), "{targets:?}");
}

#[test]
fn dart_mixin_on_clause_lists_every_constraint() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "m.dart",
        "class A {}\nclass B<T> {}\nmixin M on A, B<int> {}\n",
    )]);
    let inherits = pairs(&r, "inherits");
    assert!(inherits.contains(&("M".into(), "A".into())) && inherits.contains(&("M".into(), "B".into())), "{inherits:?}");
}

#[test]
fn fortran_extends_clause_is_an_inherits_edge() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "shapes.f90",
        "module shapes\n  type :: shape\n  end type shape\n  type, extends(shape) :: circle\n  end type circle\nend module shapes\n",
    )]);
    assert!(pairs(&r, "inherits").contains(&("circle".into(), "shape".into())), "{:?}", pairs(&r, "inherits"));
}

#[test]
fn scala_extension_methods_belong_to_their_receiver_type() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "Point.scala",
        "case class Point(x: Int, y: Int)\n\nextension (p: Point)\n  def flip: Point = Point(p.y, p.x)\n",
    )]);
    let owners = edges(&r)
        .iter()
        .filter(|e| label_of(&r, &e["target"]).trim_start_matches('.').starts_with("flip"))
        .map(|e| label_of(&r, &e["source"]))
        .collect::<Vec<_>>();
    assert!(owners.iter().any(|o| o == "Point"), "{owners:?}");
}

#[test]
fn terraform_scope_marker_never_leaks() {
    let f = Fixture::new();
    let r = f.extract_files(&[("infra/main.tf", "resource \"aws_s3_bucket\" \"b\" {}\n")]);
    assert!(labels(&r).contains("aws_s3_bucket.b"));
    assert!(r.get("_terraform_scope").is_none());
}
