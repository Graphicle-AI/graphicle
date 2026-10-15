//! Mirrors the 0.9.74 additions to upstream extraction tests:
//! `tests/test_kotlin_grammar.py` (#3835 annotation class literals),
//! `tests/test_lua_method_calls.py` (#3991 colon-method calls).
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// A path as the engine reports it: on Windows `/`-separated and without the
/// verbatim prefix; elsewhere `to_string_lossy` itself.
fn spelled(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.strip_prefix(r"\\?\")
            .unwrap_or(&text)
            .replace('\\', "/")
    } else {
        text.into_owned()
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
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
    fn extract(&self, paths: &[PathBuf]) -> Value {
        let options = ExtractOptions {
            root: Some(self.root.clone()),
            cache_root: Some(self.root.clone()),
            parallel: false,
            ..Default::default()
        };
        extract(paths, &options).unwrap()
    }
}

fn labels(result: &Value) -> BTreeSet<String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}

/// `_edge_labels`: `(source label, target label)` for every `relation` edge.
fn edge_labels(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    let by_id: HashMap<&str, &str> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n["label"].as_str()?)))
        .collect();
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == relation)
        .filter_map(|e| {
            Some((
                by_id.get(e["source"].as_str()?)?.to_string(),
                by_id.get(e["target"].as_str()?)?.to_string(),
            ))
        })
        .collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

fn call_edges(result: &Value) -> BTreeSet<(String, String)> {
    let mut out = edge_labels(result, "calls");
    out.extend(edge_labels(result, "indirect_call"));
    out
}

fn edges_with_context(result: &Value, relation: &str, context: &str) -> BTreeSet<(String, String)> {
    let by_id: HashMap<&str, &str> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n["label"].as_str()?)))
        .collect();
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == relation && e["context"] == context)
        .filter_map(|e| {
            Some((
                by_id.get(e["source"].as_str()?)?.to_string(),
                by_id.get(e["target"].as_str()?)?.to_string(),
            ))
        })
        .collect()
}

fn one(rel: &str, text: &str) -> Value {
    let fx = Fixture::new();
    let path = fx.write(rel, text);
    fx.extract(&[path])
}

#[test]
fn kotlin_annotation_class_literals_are_attribute_references() {
    let r = one(
        "Order.kt",
        "@Ann(target = com.example.Customer::class)\n\
         class Order(\n\
         \x20   @ManyToOne(targetEntity = Customer::class) val cust: Any\n\
         ) {\n\
         \x20   @ManyToOne(targetEntity = OrderLine::class)\n\
         \x20   var line: Any = Any()\n\
         }\n\
         class Customer\n\
         class OrderLine\n",
    );
    let attrs = edges_with_context(&r, "references", "attribute");
    assert!(attrs.contains(&pair("Order", "Customer")), "{attrs:?}");
    assert!(attrs.contains(&pair("Order", "OrderLine")), "{attrs:?}");
}

#[test]
fn lua_self_colon_calls_resolve_to_sibling_methods() {
    let r = one(
        "animal.lua",
        "local Animal = {}\nfunction Animal:speak()\n    return self:describe()\nend\n\
         function Animal:describe()\n    return self.name\nend\n",
    );
    assert!(call_edges(&r).contains(&pair("Animal:speak()", "Animal:describe()")));
    let r = one(
        "animal.lua",
        "local Animal = {}\nfunction Animal.helper()\n    return 1\nend\n\
         function Animal:run()\n    return self:helper()\nend\n",
    );
    assert!(call_edges(&r).contains(&pair("Animal:run()", "Animal.helper()")));
}

#[test]
fn lua_colon_calls_fail_closed_on_bare_names() {
    let r = one(
        "animal.lua",
        "function process()\n    return 1\nend\nlocal Animal = {}\n\
         function Animal:run()\n    return self:process()\nend\n",
    );
    assert!(!call_edges(&r).contains(&pair("Animal:run()", "process()")));
    let r = one(
        "animal.lua",
        "function process()\n    return 1\nend\nlocal Animal = {}\n\
         function Animal:run()\n    local obj = make()\n    return obj:process()\nend\n",
    );
    assert!(!call_edges(&r).contains(&pair("Animal:run()", "process()")));
    let r = one(
        "mod.lua",
        "local M = {}\nfunction M.helper()\n    return 1\nend\n\
         function M.main()\n    return M.helper()\nend\n",
    );
    assert!(call_edges(&r).contains(&pair("M.main()", "M.helper()")));
}

#[test]
fn erlang_local_fun_references_are_indirect_calls() {
    let r = one(
        "worker.erl",
        "-module(worker).\n-export([run/0]).\nrun() -> lists:map(fun helper/1, [1, 2, 3]).\nhelper(X) -> X + 1.\n",
    );
    assert!(edge_labels(&r, "indirect_call").contains(&pair("run/0", "helper/1")));
    let r = one(
        "worker.erl",
        "-module(worker).\nrun() -> spawn(fun helper/0).\nhelper(X) -> X.\n",
    );
    assert!(
        !edge_labels(&r, "indirect_call")
            .iter()
            .any(|(s, _)| s == "run/0")
    );
    let r = one(
        "worker.erl",
        "-module(worker).\nrun() -> helper(5).\nhelper(X) -> X.\n",
    );
    assert!(edge_labels(&r, "calls").contains(&pair("run/0", "helper/1")));
    assert!(!edge_labels(&r, "indirect_call").contains(&pair("run/0", "helper/1")));
    let r = one(
        "worker.erl",
        "-module(worker).\nrun() -> lists:foreach(fun(X) -> helper(X) end, [1, 2, 3]).\nhelper(X) -> X.\n",
    );
    assert!(edge_labels(&r, "calls").contains(&pair("run/0", "helper/1")));
}

fn many(files: &[(&str, &str)]) -> Value {
    let fx = Fixture::new();
    let paths = files
        .iter()
        .map(|(rel, text)| fx.write(rel, text))
        .collect::<Vec<_>>();
    fx.extract(&paths)
}

/// `_find`: the id of the node with `label` whose id contains `needle`.
fn find(result: &Value, label: &str, needle: &str) -> String {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == label && n["id"].as_str().unwrap().contains(needle))
        .and_then(|n| n["id"].as_str())
        .unwrap_or_else(|| panic!("no {label} node containing {needle}"))
        .to_string()
}

fn id_edges(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_string(),
                e["target"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn csharp_null_conditional_member_calls_resolve_through_the_receiver() {
    let types = "public class Server { public bool Save() => true; }\n\
                 public class Cache  { public bool Save() => false; }\n";
    for body in ["return _s?.Save();", "return this._s?.Save();"] {
        let r = many(&[(
            "S.cs",
            &format!(
                "{types}public class Repo {{\n    private Server _s = new Server();\n    public bool? Commit() {{ {body} }}\n}}\n"
            ),
        )]);
        let calls = id_edges(&r, "calls");
        let commit = find(&r, ".Commit()", "commit");
        assert!(
            calls.contains(&(commit.clone(), find(&r, ".Save()", "server"))),
            "{body}"
        );
        assert!(
            !calls.contains(&(commit, find(&r, ".Save()", "cache"))),
            "{body}"
        );
    }
    let r = many(&[
        ("Server.cs", types),
        (
            "Repo.cs",
            "public class Repo { private Server _s = new Server(); public bool? Commit() { return _s?.Save(); } }\n",
        ),
    ]);
    let calls = id_edges(&r, "calls");
    assert!(
        calls
            .iter()
            .any(|(s, t)| s.contains("commit") && t.contains("server_save"))
    );
    assert!(
        !calls
            .iter()
            .any(|(s, t)| s.contains("commit") && t.contains("cache_save"))
    );
    let r = many(&[
        (
            "T.cs",
            "public interface IThing {}\npublic class Registry { public void Do<T>() {} }\n",
        ),
        (
            "P.cs",
            "public class Probe { public void A(Registry r) => r?.Do<IThing>(); }\n",
        ),
    ]);
    assert!(edges_with_context(&r, "references", "generic_arg").contains(&pair(".A()", "IThing")));
}

#[test]
fn ruby_paren_less_self_sends_are_calls() {
    let sends = [
        (
            "class Widget\n  def run\n    build\n  end\n  def build\n    1\n  end\nend\n",
            ".run()",
            ".build()",
        ),
        (
            "module Greetable\n  def greet\n    build_message\n  end\n  def build_message\n    \"hi\"\n  end\nend\n",
            ".greet()",
            ".build_message()",
        ),
        (
            "module Greetable\n  def greet\n    \"hi\"\n  end\nend\nclass Person\n  include Greetable\n  def initialize\n    greet\n  end\nend\n",
            ".initialize()",
            ".greet()",
        ),
        (
            "class W\n  def run\n    log hello\n  end\n  def hello\n  end\n  def log(x)\n  end\nend\n",
            ".run()",
            ".hello()",
        ),
    ];
    for (text, a, b) in sends {
        assert!(
            edge_labels(&one("w.rb", text), "calls").contains(&pair(a, b)),
            "{text}"
        );
    }
    let reads = [
        "class W\n  def run\n    build = 1\n    build\n  end\n  def build\n    2\n  end\nend\n",
        "class W\n  def run(build)\n    build\n  end\n  def build\n    1\n  end\nend\n",
        "class W\n  def run\n    [1].each { |build| build }\n  end\n  def build\n    1\n  end\nend\n",
        "class W\n  def run(opts = {}, *build)\n    build\n  end\n  def build\n  end\nend\n",
        "class W\n  def run\n    for build in [1]\n      build\n    end\n  end\n  def build\n  end\nend\n",
        "class W\n  def run\n    begin\n      1\n    rescue => build\n      build\n    end\n  end\n  def build\n  end\nend\n",
    ];
    for text in reads {
        assert!(
            !edge_labels(&one("w.rb", text), "calls").contains(&pair(".run()", ".build()")),
            "{text}"
        );
    }
}

#[test]
fn typescript_abstract_methods_are_method_nodes() {
    let r = one(
        "s.ts",
        "abstract class Shape {\n  abstract area(): number;\n  describe(): string { return this.area().toString(); }\n}\n\
         function use(s: Shape): number {\n  return s.area();\n}\n",
    );
    let methods = edge_labels(&r, "method");
    assert!(methods.contains(&pair("Shape", ".area()")), "{methods:?}");
    assert!(methods.contains(&pair("Shape", ".describe()")));
    let calls = edge_labels(&r, "calls");
    assert!(calls.contains(&pair(".describe()", ".area()")), "{calls:?}");
    assert!(calls.contains(&pair("use()", ".area()")), "{calls:?}");
}

#[test]
fn scala_deferred_methods_are_method_nodes() {
    let r = one(
        "s.scala",
        "trait Shape {\n  def area: Double\n  def scale(f: Double): Shape\n  def name(): String\n  def describe: String = name()\n}\n",
    );
    let methods = edge_labels(&r, "method");
    for m in [".area()", ".scale()", ".name()", ".describe()"] {
        assert!(methods.contains(&pair("Shape", m)), "{m}: {methods:?}");
    }
    assert!(edge_labels(&r, "calls").contains(&pair(".describe()", ".name()")));
    let r = one(
        "s.scala",
        "abstract class Base {\n  def compute(x: Int): Int\n  def run(): Int = compute(1)\n}\n",
    );
    assert!(edge_labels(&r, "method").contains(&pair("Base", ".compute()")));
    assert!(edge_labels(&r, "calls").contains(&pair(".run()", ".compute()")));
}

#[test]
fn cpp_member_function_declarations_are_methods() {
    let r = one(
        "s.cpp",
        "class Shape {\npublic:\n  virtual double area() = 0;\n  double describe() { return area(); }\n};\n\
         class Svc {\npublic:\n  virtual void draw();\n  int run();\n  int count;\n};\n",
    );
    let methods = edge_labels(&r, "method");
    for (owner, m) in [("Shape", ".area()"), ("Svc", ".draw()"), ("Svc", ".run()")] {
        assert!(methods.contains(&pair(owner, m)), "{owner}{m}: {methods:?}");
    }
    let defines = edge_labels(&r, "defines");
    assert!(defines.contains(&pair("Svc", "count")), "{defines:?}");
    assert!(!defines.contains(&pair("Shape", "area")));
    assert!(edge_labels(&r, "calls").contains(&pair(".describe()", ".area()")));
}

fn rust_file(name: &str, text: &str) -> (tempfile::TempDir, PathBuf, Value) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join(name);
    std::fs::write(&path, text).unwrap();
    let r = graphicle::extractors::rust::extract_rust(&path);
    (dir, path, r)
}

/// `(target label, target source_file)` of every `references` edge.
fn reference_targets(r: &Value) -> Vec<(String, String)> {
    let by_id: HashMap<&str, &Value> = r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n)))
        .collect();
    r["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "references")
        .filter_map(|e| by_id.get(e["target"].as_str()?))
        .map(|n| {
            (
                n["label"].as_str().unwrap().to_string(),
                n["source_file"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn rust_forward_type_references_resolve_to_local_declarations() {
    let (_d, _p, r) = rust_file(
        "engine.rs",
        "fn before(s: &Sink) -> Mode { Mode::A }\n\nstruct Sink {\n    n: u32,\n}\n\n\
         impl Handler for Sink {}\n\nenum Mode {\n    A,\n}\n\ntrait Handler {}\n",
    );
    let sourceless: Vec<_> = r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["source_file"] == "")
        .collect();
    assert!(sourceless.is_empty(), "{sourceless:?}");
    assert!(
        edges_with_context(&r, "references", "parameter_type").contains(&pair("before()", "Sink"))
    );
    assert!(
        edges_with_context(&r, "references", "return_type").contains(&pair("before()", "Mode"))
    );
    assert!(edge_labels(&r, "implements").contains(&pair("Sink", "Handler")));
    // Items walk() never makes nodes of keep their sourceless stubs.
    let (_d, _p, r) = rust_file(
        "shapes.rs",
        "fn user(a: &Alias, u: &Bits, l: &Local, e: &Elsewhere) {}\n\ntype Alias = u32;\n\n\
         union Bits { a: u32, b: f32 }\n\nfn host() {\n    struct Local;\n}\n",
    );
    let targets = reference_targets(&r);
    for name in ["Alias", "Bits", "Local", "Elsewhere"] {
        assert!(
            targets.contains(&(name.into(), String::new())),
            "{name}: {targets:?}"
        );
    }
    let (_d, path, r) = rust_file(
        "nested.rs",
        "use inner::Sink;\n\nfn before(s: &Sink) {}\n\nmod inner {\n    pub struct Sink;\n}\n",
    );
    let file = spelled(&path);
    assert!(reference_targets(&r).contains(&("Sink".into(), file)));
}

#[test]
fn rust_forward_reference_to_a_colliding_id_keeps_its_stub() {
    let extern_fn = "extern \"C\" {\n    fn handle();\n}";
    let impl_for_alias = "type HANDLE = usize;\n\ntrait Tr {}\n\nimpl Tr for HANDLE {}";
    for (name, user, other, label) in [
        (
            "fn_collision.rs",
            "fn user(h: &Handle) {}",
            "fn handle() {}",
            "Handle",
        ),
        (
            "extern_fn_collision.rs",
            "fn user(h: &Handle) {}",
            extern_fn,
            "Handle",
        ),
        (
            "const_collision.rs",
            "fn user(h: &Handle) {}",
            "const HANDLE: u32 = 0;",
            "Handle",
        ),
        (
            "static_collision.rs",
            "fn user(h: &Handle) {}",
            "static HANDLE: u32 = 0;",
            "Handle",
        ),
        (
            "type_collision.rs",
            "fn user(h: &Handle) {}",
            "struct HANDLE;",
            "Handle",
        ),
        (
            "impl_collision.rs",
            "fn user(h: &Handle) {}",
            impl_for_alias,
            "Handle",
        ),
        (
            "alias_collision.rs",
            "fn user(h: HANDLE) {}",
            "type HANDLE = usize;",
            "HANDLE",
        ),
    ] {
        let (_d, _p, r) = rust_file(
            name,
            &format!("{user}\n\n{other}\n\nstruct Handle {{\n    n: u32,\n}}\n"),
        );
        assert_eq!(
            reference_targets(&r),
            [(label.to_string(), String::new())],
            "{name}"
        );
    }
    let impl_for_type = "struct Handle<'a> {\n    s: &'a str,\n}\n\ntrait Tr {}\n\n\
                         impl Tr for Handle<'_> {}\n\nimpl Tr for &Handle<'_> {}\n";
    for (name, source) in [
        (
            "no_collision.rs",
            "struct Handle {\n    n: u32,\n}\n\nfn other() {}\n",
        ),
        (
            "cfg_twins.rs",
            "#[cfg(unix)]\nstruct Handle;\n\n#[cfg(not(unix))]\nstruct Handle;\n",
        ),
        ("impl_for_type.rs", impl_for_type),
        (
            "method_like_type.rs",
            "struct Handle;\n\nimpl Handle {\n    fn handle(&self) {}\n}\n",
        ),
    ] {
        let (_d, path, r) = rust_file(name, &format!("fn user(h: &Handle) {{}}\n\n{source}"));
        let file = spelled(&path);
        assert!(
            edge_labels(&r, "references").contains(&pair("user()", "Handle"))
                && reference_targets(&r).contains(&("Handle".into(), file)),
            "{name}: {:?}",
            reference_targets(&r)
        );
    }
}

#[test]
fn rust_prelude_types_are_not_nodes_and_constructors_are_references() {
    let fx = Fixture::new();
    let a = fx.write(
        "a.rs",
        "pub fn parse(items: Vec<String>, name: Option<String>) -> Result<u32, String> {\n\
         \x20   let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();\n\
         \x20   let _ = &mut seen;\n    Ok(name.map(|_| 1u32).unwrap_or(0))\n}\n",
    );
    let b = fx.write(
        "b.rs",
        "pub struct Widget {\n    pub name: String,\n    pub parts: Vec<u32>,\n}\n\
         pub fn widget() -> Widget {\n    Widget { name: String::new(), parts: Vec::new() }\n}\n",
    );
    let r = fx.extract(&[a, b]);
    let names = labels(&r);
    for builtin in ["Option", "Vec", "String", "Result", "HashMap"] {
        assert!(!names.contains(builtin), "{builtin}: {names:?}");
    }
    assert!(names.contains("Widget"));

    let fx = Fixture::new();
    let ids = fx.write("ids.rs", "pub struct ClientId(pub u32);\n");
    let user = fx.write(
        "use_ids.rs",
        "use crate::ids::ClientId;\npub fn wrap(raw: u32) -> u32 {\n    let _id = ClientId(raw);\n    raw\n}\n",
    );
    let r = fx.extract(&[ids, user]);
    assert!(
        edges_with_context(&r, "references", "constructor").contains(&pair("wrap()", "ClientId"))
    );
    assert!(!call_edges(&r).iter().any(|(_, t)| t == "ClientId"));

    let r = one(
        "ids.rs",
        "pub struct ClientId(pub u32);\npub fn wrap(raw: u32) -> u32 {\n    let _id = ClientId(raw);\n    raw\n}\n\
         pub fn helper() -> u32 { 1 }\npub fn run() -> u32 { helper() }\n",
    );
    assert!(
        edges_with_context(&r, "references", "constructor").contains(&pair("wrap()", "ClientId"))
    );
    assert!(!call_edges(&r).iter().any(|(_, t)| t == "ClientId"));
    assert!(edge_labels(&r, "calls").contains(&pair("run()", "helper()")));

    let r = one(
        "shadow.rs",
        "pub struct Result<T>(pub T);\npub struct Holder {\n    pub value: Result<u32>,\n}\n",
    );
    assert!(edge_labels(&r, "references").contains(&pair("Holder", "Result")));
}

fn many_files(files: &[(&str, &str)]) -> Value {
    let fx = Fixture::new();
    let paths: Vec<PathBuf> = files
        .iter()
        .map(|(rel, text)| fx.write(rel, text))
        .collect();
    fx.extract(&paths)
}

/// `(caller label, target label, target source_file)` of every `calls` edge.
fn call_triples(result: &Value) -> Vec<(String, String, String)> {
    let by_id: HashMap<&str, &Value> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n)))
        .collect();
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls")
        .filter_map(|e| {
            let src = by_id.get(e["source"].as_str()?)?;
            let tgt = by_id.get(e["target"].as_str()?)?;
            Some((
                src["label"].as_str()?.to_string(),
                tgt["label"].as_str()?.to_string(),
                tgt["source_file"].as_str()?.to_string(),
            ))
        })
        .collect()
}

fn has_call(calls: &[(String, String, String)], caller: &str, target: &str, file: &str) -> bool {
    calls
        .iter()
        .any(|(c, t, f)| c == caller && t == target && f.ends_with(file))
}

#[test]
fn constructor_calls_survive_a_same_file_annotation_stub() {
    let r = many_files(&[
        (
            "app/engines/transcript/__init__.py",
            "class Parser:\n    pass\n",
        ),
        (
            "app/engines/__init__.py",
            "from typing import Optional\nfrom app.engines.transcript import Parser\n\n\n\
             class Engine:\n    def detect(self):\n        return Parser()\n\n\
             \x20   def get_parser(self) -> Optional[Parser]:\n        return None\n",
        ),
    ]);
    let calls = call_triples(&r);
    assert!(
        has_call(
            &calls,
            ".detect()",
            "Parser",
            "app/engines/transcript/__init__.py"
        ),
        "{calls:?}"
    );

    let r = many_files(&[
        (
            "Lib/Widget.cs",
            "namespace Demo\n{\n    public class Widget\n    {\n    }\n}\n",
        ),
        (
            "Lib/Kinds.cs",
            "namespace Demo\n{\n    public enum ViewKind { List, Grid }\n}\n",
        ),
        (
            "App/Factory.cs",
            "using System.Collections.Generic;\n\nnamespace Demo\n{\n    public class Registry\n    {\n\
             \x20       public List<Widget> Items;\n    }\n\n    public class Factory\n    {\n\
             \x20       public object MakeA() { return new Widget(); }\n\n\
             \x20       public object MakeList() { return new List<int>(); }\n    }\n}\n",
        ),
        (
            "App/Plain.cs",
            "namespace Demo\n{\n    public class Plain\n    {\n\
             \x20       public object Make() { return new Widget(); }\n    }\n}\n",
        ),
    ]);
    let calls = call_triples(&r);
    assert!(
        has_call(&calls, ".Make()", "Widget", "Lib/Widget.cs"),
        "{calls:?}"
    );
    assert!(
        has_call(&calls, ".MakeA()", "Widget", "Lib/Widget.cs"),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|(c, _, _)| c == ".MakeList()"),
        "{calls:?}"
    );
}

#[test]
fn stub_rescued_calls_need_import_or_scope_evidence() {
    for files in [
        vec![
            (
                "other/Client.java",
                "package other;\npublic class Client {}\n",
            ),
            (
                "app/A.java",
                "package app;\nimport com.lib.Client;\n\npublic class A {\n\
                 \x20   public Client make() { return new Client(); }\n}\n",
            ),
        ],
        vec![
            (
                "Other/List.cs",
                "namespace Other\n{\n    public class List { }\n}\n",
            ),
            (
                "App/A.cs",
                "using System.Collections.Generic;\n\nnamespace App\n{\n    public class A\n    {\n\
                 \x20       public List<int> Items;\n\
                 \x20       public object Make() { return new List<int>(); }\n    }\n}\n",
            ),
        ],
    ] {
        let calls = call_triples(&many_files(&files));
        assert!(
            !calls
                .iter()
                .any(|(c, _, _)| c == ".make()" || c == ".Make()"),
            "{calls:?}"
        );
    }
    let calls = call_triples(&many_files(&[
        (
            "Other/List.cs",
            "namespace Other\n{\n    public class List { }\n}\n",
        ),
        (
            "App/A.cs",
            "using Other;\n\nnamespace App\n{\n    public class A\n    {\n        public List Items;\n\
             \x20       public object Make() { return new List(); }\n    }\n}\n",
        ),
    ]));
    assert!(
        has_call(&calls, ".Make()", "List", "Other/List.cs"),
        "{calls:?}"
    );
}

fn imports_between(result: &Value, from: &str, to: &str) -> bool {
    let file_of: HashMap<&str, &str> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n["source_file"].as_str()?)))
        .collect();
    result["edges"].as_array().unwrap().iter().any(|e| {
        e["relation"] == "imports_from"
            && e["source"]
                .as_str()
                .and_then(|id| file_of.get(id))
                .is_some_and(|f| f.ends_with(from))
            && e["target"]
                .as_str()
                .and_then(|id| file_of.get(id))
                .is_some_and(|f| f.ends_with(to))
    })
}

#[test]
fn workspace_packages_prefer_the_source_entry_over_built_dist() {
    let fx = Fixture::new();
    fx.write(
        "pnpm-workspace.yaml",
        "packages:\n  - 'apps/*'\n  - 'packages/*'\n",
    );
    fx.write(
        "packages/pkg-a/package.json",
        r#"{"name":"@example/pkg-a","main":"./dist/index.js","types":"./dist/index.d.ts"}"#,
    );
    fx.write(
        "packages/pkg-a/dist/index.js",
        "export const value = \"from-dist\";\n",
    );
    fx.write(
        "packages/pkg-a/dist/index.d.ts",
        "export declare const value: string;\n",
    );
    let source = fx.write(
        "packages/pkg-a/src/index.ts",
        "export const value = \"ok\";\n",
    );
    let consumer = fx.write(
        "apps/web/src/consumer.ts",
        "import { value } from '@example/pkg-a'\nexport const v = value\n",
    );
    let r = fx.extract(&[source, consumer]);
    assert!(imports_between(
        &r,
        "apps/web/src/consumer.ts",
        "packages/pkg-a/src/index.ts"
    ));

    let fx = Fixture::new();
    fx.write(
        "pnpm-workspace.yaml",
        "packages:\n  - 'apps/*'\n  - 'packages/*'\n",
    );
    fx.write(
        "packages/pkg-a/package.json",
        r#"{"name":"@example/pkg-a","main":"./dist/index.js"}"#,
    );
    let dist = fx.write(
        "packages/pkg-a/dist/index.js",
        "export const value = \"from-dist\";\n",
    );
    let consumer = fx.write(
        "apps/web/src/consumer.ts",
        "import { value } from '@example/pkg-a'\nexport const v = value\n",
    );
    let r = fx.extract(&[dist, consumer]);
    assert!(imports_between(
        &r,
        "apps/web/src/consumer.ts",
        "packages/pkg-a/dist/index.js"
    ));
}

#[test]
fn verilog_header_files_are_extracted() {
    let fx = Fixture::new();
    let header = fx.write(
        "rtl/defs.vh",
        "module defs(input a, output b);\nendmodule\n",
    );
    let r = fx.extract(&[header]);
    assert!(labels(&r).contains("defs"), "{:?}", labels(&r));
}

/// Targets of `references` edges from the page of `from` (vault-relative),
/// as vault-relative page files; also checks no target is a ghost node.
fn page_refs(fx: &Fixture, files: &[(&str, &str)], from: &str) -> BTreeSet<String> {
    let mut paths: Vec<PathBuf> = files
        .iter()
        .map(|(rel, text)| fx.write(rel, text))
        .collect();
    paths.sort();
    let r = fx.extract(&paths);
    let nodes = r["nodes"].as_array().unwrap();
    let page_file: HashMap<&str, &str> = nodes
        .iter()
        .filter(|n| n["node_kind"] == "page")
        .filter_map(|n| Some((n["id"].as_str()?, n["source_file"].as_str()?)))
        .collect();
    let ids: BTreeSet<&str> = nodes.iter().filter_map(|n| n["id"].as_str()).collect();
    let from_id = page_file
        .iter()
        .find(|(_, f)| f.ends_with(from))
        .map(|(id, _)| *id)
        .unwrap();
    r["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "references" && e["source"] == from_id)
        .map(|e| {
            let target = e["target"].as_str().unwrap();
            assert!(ids.contains(target), "ghost target {e}");
            page_file
                .get(target)
                .map_or(target.to_string(), |f| f.to_string())
        })
        .collect()
}

#[test]
fn wikilinks_resolve_note_names_containing_a_dot() {
    let fx = Fixture::new();
    let refs = page_refs(
        &fx,
        &[
            ("log/note.en.md", "# Note\n"),
            ("log/sub/deep.fr.md", "# Deep\n"),
            ("v1.2 release.md", "# Release\n"),
            (
                "log/entry.md",
                "See [[note.en]], [[v1.2 release]] and [[sub/deep.fr|deep]].\n",
            ),
        ],
        "log/entry.md",
    );
    let ends = |suffix: &str| refs.iter().any(|f| f.ends_with(suffix));
    assert_eq!(refs.len(), 3, "{refs:?}");
    assert!(ends("log/note.en.md") && ends("log/sub/deep.fr.md") && ends("v1.2 release.md"));

    let fx = Fixture::new();
    fx.write("log/image.png", "\u{89}PNG\r\n");
    let refs = page_refs(
        &fx,
        &[
            ("log/note.en.md", "# Note\n"),
            (
                "log/entry.md",
                "See [[missing.en]], [[image.png]] and [note](note.en).\n",
            ),
        ],
        "log/entry.md",
    );
    assert!(refs.is_empty(), "{refs:?}");

    let fx = Fixture::new();
    fx.write("log/pic.png", "\u{89}PNG\r\n");
    fx.write("log/thirteen.en", "raw\n");
    let refs = page_refs(
        &fx,
        &[
            ("log/pic.png.md", "# Pic\n"),
            ("log/thirteen.en.md", "# Thirteen\n"),
            ("log/entry.md", "See [[pic.png]] and [[thirteen.en]].\n"),
        ],
        "log/entry.md",
    );
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert!(
        refs.iter().all(|f| f.ends_with("log/thirteen.en.md")),
        "{refs:?}"
    );
}
