//! The member passes over compact records (per-file results and edges as
//! `Attributes`) give byte for byte what they give over JSON, for every
//! language they serve, cold and from the cache.
use super::NATIVE_RUNS;
use crate::extract::{ExtractOptions, extract};
use crate::extractors;
use serde_json::Value;

/// Every member pass, in registry order: the fixture wakes each of them, and
/// none of the passes before them reads only JSON.
const PASSES: [&str; 15] = [
    "swift_member_calls",
    "python_member_calls",
    "typescript_member_calls",
    "cpp_member_calls",
    "objc_member_calls",
    "csharp_member_calls",
    "java_member_calls",
    "php_member_calls",
    "rust_self_member_calls",
    "elixir_import_targets",
    "elixir_qualified_calls",
    "kotlin_qualified_calls",
    "kotlin_member_calls",
    "go_member_calls",
    "csharp_qualified_calls",
];

#[test]
fn member_passes_on_records_extract_exactly_like_json() {
    let _serial = extractors::parse_cache::test_serial();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let write = |rel: &str, text: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    };
    let mut paths = vec![
        write(
            "ts/repo.ts",
            "export class Repo {\n  save(): void {}\n  load(): void {}\n}\n",
        ),
        write(
            "ts/svc.ts",
            "import { Repo } from \"./repo\";\n\nexport class Svc {\n  protected repo: Repo;\n  run() { this.repo.save(); }\n  go(r: Repo) { r.load(); }\n}\n",
        ),
        write("ts/util.js", "export function helper(x) { return x; }\n"),
        write(
            "cpp/shape.h",
            "class Shape {\npublic:\n  int area();\n  int twice();\n};\n",
        ),
        write(
            "cpp/shape.cpp",
            "#include \"shape.h\"\n\nint Shape::area() { return 1; }\nint Shape::twice() { return this->area() * 2; }\nint use() { Shape s; return s.area() + Missing::call(); }\n",
        ),
        write(
            "objc/Thing.m",
            "@interface Thing : NSObject\n- (void)run;\n- (void)go;\n@end\n\n@implementation Thing\n- (void)run { [self go]; }\n- (void)go {}\n@end\n",
        ),
        write(
            "cs/Cart.cs",
            "namespace Shop {\n  public class Cart {\n    public void Reset() { }\n  }\n}\n",
        ),
        write(
            "cs/Store.cs",
            "namespace Shop {\n  public class Store {\n    Cart cart;\n    public Store(Cart c) { c.Reset(); }\n    public void Clear() { this.cart.Reset(); Helper.Run(); Gone.Away(); }\n  }\n  public static class Helper {\n    public static void Run() { }\n  }\n}\n",
        ),
        write(
            "j/p/A.java",
            "package p;\n\npublic class A {\n    public void f() {}\n    public A self() { return this; }\n}\n",
        ),
        write(
            "j/q/B.java",
            "package q;\n\nimport p.A;\n\nclass B extends A {\n    A a;\n    void g() { new A().f(); a.f(); A x = new A(); x.self().f(); f(); Unknown.go(); }\n}\n",
        ),
        write(
            "ph/A.php",
            "<?php\nnamespace P;\n\nclass A {\n    public static function make() {}\n    public function f() { self::make(); }\n}\n",
        ),
        write(
            "ph/B.php",
            "<?php\nnamespace Q;\n\nuse P\\A;\n\nclass B extends A {\n    public function g() { A::make(); parent::f(); Missing::call(); }\n}\n",
        ),
        write(
            "sw/S.swift",
            "struct S {\n    func a() {}\n    static func make() -> S { return S() }\n}\n",
        ),
        write(
            "sw/Box.swift",
            "class Box {\n    let s: S = S()\n    func f() { s.a(); S.make(); Gone.away() }\n}\n",
        ),
        write(
            "k/A.kt",
            "package demo.k\n\nobject Registry {\n    fun lookup(): Int = 1\n}\n\nfun top(): Int = 2\n",
        ),
        write(
            "k/B.kt",
            "package demo.other\n\nimport demo.k.Registry\n\nfun g(): Int = Registry.lookup() + demo.k.top()\n",
        ),
        write(
            "go/greeter.go",
            "package greet\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() string { return \"hi\" }\n",
        ),
        write(
            "go/run.go",
            "package greet\n\nfunc Run(g *Greeter) string { return g.Greet() }\n",
        ),
        write(
            "src/lib.rs",
            "pub struct S;\n\nimpl S {\n    pub fn f(&self) -> usize { self.g() }\n    fn g(&self) -> usize { 1 }\n}\n",
        ),
        write(
            "ex/accounts.ex",
            "defmodule App.Accounts do\n  def get_user(id) do\n    id\n  end\nend\n",
        ),
        write(
            "ex/web.ex",
            "defmodule App.Web do\n  import App.Accounts\n  def show(id) do\n    App.Accounts.get_user(id)\n    Repo.all(id)\n  end\nend\n",
        ),
        write(
            "py/models.py",
            "class Base:\n    def run(self):\n        return 1\n\n\nclass Child(Base):\n    def go(self):\n        return self.run()\n",
        ),
        write(
            "py/app.py",
            "import models\nfrom models import Child\n\n\ndef main():\n    c = Child()\n    models.Base().run()\n    return c.go()\n",
        ),
    ];
    paths.sort();
    let run = |json_only: bool, use_cache: bool| {
        crate::resolver_registry::JSON_ONLY.with(|f| f.set(json_only));
        NATIVE_RUNS.with(|runs| runs.borrow_mut().clear());
        let out = extract(
            &paths,
            &ExtractOptions {
                root: Some(root.clone()),
                cache_root: Some(root.join(if json_only { "cache-json" } else { "cache-rec" })),
                use_cache,
                parallel: false,
                ..Default::default()
            },
        );
        crate::resolver_registry::JSON_ONLY.with(|f| f.set(false));
        let runs = NATIVE_RUNS.with(|runs| std::mem::take(&mut *runs.borrow_mut()));
        (serde_json::to_string(&out.unwrap()).unwrap(), runs)
    };
    for use_cache in [false, true, true] {
        let (json, json_runs) = run(true, use_cache);
        let (records, runs) = run(false, use_cache);
        assert!(json_runs.is_empty(), "{json_runs:?}");
        assert_eq!(runs, PASSES, "cache {use_cache}");
        assert_eq!(records, json, "cache {use_cache}");
    }
    // The fixture reaches the member passes of every language: calls bound
    // across files and calls parked on their callers.
    let out: Value = serde_json::from_str(&run(false, false).0).unwrap();
    let edges = out["edges"].as_array().unwrap();
    for suffix in [
        ".ts", ".cpp", ".m", ".cs", ".java", ".php", ".swift", ".kt", ".rs", ".ex", ".py",
    ] {
        assert!(
            edges.iter().any(|e| e["relation"] == "calls"
                && e["source_file"]
                    .as_str()
                    .is_some_and(|f| f.ends_with(suffix))),
            "no call edge from a {suffix} file"
        );
    }
}
