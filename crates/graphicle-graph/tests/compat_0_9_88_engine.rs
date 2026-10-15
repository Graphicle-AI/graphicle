// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
//! Generic-engine changes ported from Graphify 0.9.78-0.9.88: the per-file
//! facts the corpus resolvers read (receiver types and chains, property reads,
//! return tables, type paths) and the in-file identity and call fixes.
use graphicle::extractors::{
    engine::{enter_scan_root, extract_generic},
    engine_config::config,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

fn run(language: &str, name: &str, source: &str) -> Value {
    extract_generic(
        Path::new(name),
        &config(language).unwrap(),
        Some(source.as_bytes()),
    )
}

fn array<'a>(result: &'a Value, key: &str) -> &'a Vec<Value> {
    static EMPTY: Vec<Value> = Vec::new();
    result[key].as_array().unwrap_or(&EMPTY)
}

fn node<'a>(result: &'a Value, label: &str) -> &'a Value {
    array(result, "nodes")
        .iter()
        .find(|n| n["label"] == label)
        .unwrap_or_else(|| panic!("no node labelled {label}: {result:#}"))
}

fn label_of(result: &Value, id: &Value) -> String {
    array(result, "nodes")
        .iter()
        .find(|n| n["id"] == *id)
        .and_then(|n| n["label"].as_str())
        .unwrap_or("")
        .to_string()
}

/// `(source label, target label)` of every `relation` edge.
fn edges(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    array(result, "edges")
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

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

fn raw_calls<'a>(result: &'a Value, callee: &str) -> Vec<&'a Value> {
    array(result, "raw_calls")
        .iter()
        .filter(|c| c["callee"] == callee)
        .collect()
}

fn raw_call<'a>(result: &'a Value, callee: &str) -> &'a Value {
    let calls = raw_calls(result, callee);
    assert_eq!(calls.len(), 1, "one raw call to {callee}: {result:#}");
    calls[0]
}

// ── 0.9.78 ──────────────────────────────────────────────────────────────────

#[test]
fn csharp_scope_ids_do_not_depend_on_line_endings() {
    let lf = "namespace App.Core\n{\n    class Widget { }\n}\n";
    let crlf = lf.replace('\n', "\r\n");
    let scope = |source: &str| {
        node(&run("csharp", "W.cs", source), "Widget")["metadata"]["scope_chain"].clone()
    };
    assert_eq!(scope(lf), serde_json::json!(["s0:0"]));
    assert_eq!(scope(lf), scope(&crlf));
}

#[test]
fn csharp_cast_receiver_names_the_member_receiver() {
    let source = "class Repo {\n  void Run(object x) {\n    ((IStore)x).Save();\n    (x as ICache).Load();\n  }\n}\n";
    let result = run("csharp", "Repo.cs", source);
    assert_eq!(raw_call(&result, "Save")["receiver"], "IStore");
    assert_eq!(raw_call(&result, "Load")["receiver"], "ICache");
}

#[test]
fn unresolved_relative_require_uses_a_stable_ref_target() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.js");
    std::fs::write(&path, "function f() { const m = require('./missing'); }\n").unwrap();
    let result = extract_generic(&path, &config("js").unwrap(), None);
    let edge = array(&result, "edges")
        .iter()
        .find(|e| e["relation"] == "imports_from")
        .expect("require edge");
    assert_eq!(edge["target"], "ref_missing");
    assert!(edge.get("target_file").is_none());
}

#[test]
fn unresolved_relative_python_import_is_named_from_the_scan_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("pkg/sub")).unwrap();
    let path = root.join("pkg/sub/mod.py");
    std::fs::write(
        &path,
        "from .missing import a\nfrom .. import b\nfrom .... import c\n",
    )
    .unwrap();
    let _root = enter_scan_root(Some(&root));
    let result = extract_generic(&path, &config("python").unwrap(), None);
    let targets: Vec<&str> = array(&result, "edges")
        .iter()
        .filter(|e| e["relation"] == "imports_from")
        .filter_map(|e| e["target"].as_str())
        .collect();
    // `pkg/sub/missing.py`, `pkg/__init__.py` (a package marker: `pkg`), and
    // an import above the scan root keeps its raw spelling.
    assert_eq!(targets, ["pkg_sub_missing", "pkg", "ref"]);
}

// ── 0.9.80 ──────────────────────────────────────────────────────────────────

#[test]
fn python_type_checking_imports_are_type_only() {
    let source = "from typing import TYPE_CHECKING\nimport typing\nif TYPE_CHECKING:\n    import a\nelse:\n    import b\nif typing.TYPE_CHECKING:\n    from c import C\nif not TYPE_CHECKING:\n    import d\n";
    let result = run("python", "m.py", source);
    let type_only: BTreeSet<String> = array(&result, "edges")
        .iter()
        .filter(|e| e["type_only"] == true)
        .filter_map(|e| e["_python_import_module"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        type_only,
        BTreeSet::from(["a".to_string(), "c".to_string()])
    );
}

#[test]
fn python_self_attribute_receivers_carry_their_class() {
    let source = "class Service:\n    def __init__(self):\n        self.client = Client()\n        self.cache: Cache = make()\n        self.other = Other()\n        self.other = Another()\n    def run(self):\n        self.client.get()\n        self.cache.put()\n        self.other.go()\n";
    let result = run("python", "svc.py", source);
    assert_eq!(raw_call(&result, "get")["_python_self_attr_type"], "Client");
    assert_eq!(raw_call(&result, "put")["_python_self_attr_type"], "Cache");
    // Two classes bound to one field poison it.
    assert!(
        raw_call(&result, "go")
            .get("_python_self_attr_type")
            .is_none()
    );
}

#[test]
fn python_typed_locals_and_parameters_type_member_calls() {
    let source = "def f(req: Request, opt: Optional[Session], text: \"Doc\", u: A | None, many: list[A], plain):\n    client = Client()\n    with Pool() as pool:\n        pool.take()\n    req.send()\n    opt.close()\n    text.render()\n    u.go()\n    many.pop()\n    plain.x()\n    client.get()\n    made = make_client()\n    made.post()\n    swapped = Client()\n    swapped = Other()\n    swapped.run()\n    loop = Client()\n    for loop in items:\n        pass\n    loop.stop()\n";
    let result = run("python", "f.py", source);
    let typed = |callee: &str| raw_call(&result, callee).get("receiver_type").cloned();
    assert_eq!(typed("send"), Some("Request".into()));
    assert_eq!(typed("close"), Some("Session".into()));
    assert_eq!(typed("render"), Some("Doc".into()));
    assert_eq!(typed("go"), Some("A".into()));
    assert_eq!(typed("take"), Some("Pool".into()));
    assert_eq!(typed("get"), Some("Client".into()));
    for untyped in ["pop", "x", "post", "run", "stop"] {
        assert_eq!(typed(untyped), None, "{untyped}");
    }
}

#[test]
fn java_anonymous_classes_get_a_node_and_their_calls() {
    let source = "class Worker {\n  void start() {\n    Runnable r = new Runnable() {\n      public void run() { helper(); }\n    };\n  }\n  void helper() {}\n}\n";
    let result = run("java", "Worker.java", source);
    let anon = node(&result, "Runnable");
    assert_eq!(anon["_callable_class"], true);
    assert!(edges(&result, "contains").contains(&pair(".start()", "Runnable")));
    assert!(edges(&result, "method").contains(&pair("Runnable", ".run()")));
    // The unqualified call binds through the enclosing class (0.9.83).
    assert!(edges(&result, "calls").contains(&pair(".run()", ".helper()")));
}

// ── 0.9.81 ──────────────────────────────────────────────────────────────────

#[test]
fn ts_class_fields_type_this_receivers() {
    let source = "class Svc {\n  protected repo: Repo;\n  #root: Node | undefined;\n  private logger = new Logger();\n  static shared: Cache;\n  run() { this.repo.save(); }\n}\n";
    let result = run("ts", "svc.ts", source);
    // Since Graphify 0.9.91 the fields ride on their class node rather than
    // in the file-wide table (static ones apart), so `this.repo` reads only
    // the caller's own class.
    let svc = node(&result, "Svc");
    assert_eq!(svc["_ts_fields"]["repo"], "Repo");
    assert_eq!(svc["_ts_fields"]["#root"], "Node");
    assert_eq!(svc["_ts_fields"]["logger"], "Logger");
    assert!(svc["_ts_fields"].get("shared").is_none());
    assert_eq!(svc["_ts_static_fields"]["shared"], "Cache");
    let table = &result["ts_type_table"]["table"];
    assert!(table.get("repo").is_none() && table.get("shared").is_none());
}

#[test]
fn python_nested_classes_are_qualified_by_their_class() {
    let source = "class Outer:\n    class Inner:\n        def m(self): pass\n    @decorate\n    class Decorated: pass\nclass Inner: pass\n";
    let result = run("python", "pkg/mod.py", source);
    let ids: BTreeSet<&str> = array(&result, "nodes")
        .iter()
        .filter(|n| n["label"] == "Inner")
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        BTreeSet::from(["pkg_mod_inner", "pkg_mod_outer_inner"])
    );
    assert!(edges(&result, "references").contains(&pair("Decorated", "decorate")));
    assert_eq!(node(&result, "Decorated")["id"], "pkg_mod_outer_decorated");
}

#[test]
fn python_opaque_bases_and_super_arguments() {
    let source = "class A(Base): pass\nclass B(mod.Base, metaclass=M): pass\nclass C(Generic[T]):\n    def m(self):\n        super().m()\n        super(C, self).m()\n";
    let result = run("python", "a.py", source);
    assert!(node(&result, "A").get("metadata").is_none());
    assert_eq!(node(&result, "B")["metadata"]["python_opaque_bases"], true);
    assert_eq!(node(&result, "C")["metadata"]["python_opaque_bases"], true);
    let supers = raw_calls(&result, "m");
    let args: Vec<bool> = supers
        .iter()
        .map(|c| c.get("_python_super_args").is_some())
        .collect();
    assert_eq!(args, [false, true], "{supers:#?}");
}

// ── 0.9.83 ──────────────────────────────────────────────────────────────────

#[test]
fn unqualified_calls_bind_to_the_nearest_class() {
    let java = "class Outer {\n  void log() {}\n  class Inner {\n    void log() {}\n    void run() { log(); }\n  }\n  void go() { log(); }\n}\nclass Other { void log() {} }\n";
    let result = run("java", "Outer.java", java);
    let calls = edges(&result, "calls");
    let ids = |label: &str| -> Vec<String> {
        array(&result, "edges")
            .iter()
            .filter(|e| e["relation"] == "calls" && label_of(&result, &e["source"]) == label)
            .map(|e| e["target"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(".run()"), ["outer_inner_log"]);
    assert_eq!(ids(".go()"), ["outer_outer_log"]);
    assert!(calls.contains(&pair(".go()", ".log()")));
}

#[test]
fn unqualified_call_ties_bind_nothing() {
    let cpp = "struct A { void f(); };\nstruct B { void f(); };\nstruct C : A, B { void g() { f(); } };\n";
    let result = run("cpp", "c.cpp", cpp);
    assert!(
        !edges(&result, "calls")
            .iter()
            .any(|(source, _)| source == ".g()"),
        "{result:#}"
    );
}

#[test]
fn csharp_member_chains_and_constructor_bodies() {
    let source = "class Shop {\n  Cart cart;\n  List<Item> items;\n  Item[] all;\n  Shop(Cart c) { c.Reset(); }\n  void Buy(Order o, int i) {\n    o.Lines.Add();\n    items[i].Price();\n    all[i].Weigh();\n  }\n}\n";
    let result = run("csharp", "Shop.cs", source);
    assert_eq!(
        raw_call(&result, "Add")["receiver_chain"],
        serde_json::json!(["Order", "Lines"])
    );
    assert_eq!(
        raw_call(&result, "Price")["receiver_chain"],
        serde_json::json!(["Item"])
    );
    assert_eq!(
        raw_call(&result, "Weigh")["receiver_chain"],
        serde_json::json!(["Item"])
    );
    let reset = raw_call(&result, "Reset");
    assert_eq!(label_of(&result, &reset["caller_nid"]), "Shop");
    assert_eq!(reset["receiver_type"], "Cart");
    let tables = array(&result, "member_field_tables");
    let fields = &tables.iter().find(|t| t["lang"] == "csharp").unwrap()["fields"];
    assert_eq!(fields["items[]"], "Item");
    assert_eq!(fields["all[]"], "Item");
}

#[test]
fn csharp_generic_arity_is_part_of_type_ids() {
    let source = "namespace N {\n  class Effect { }\n  class Effect<T> : Effect { }\n  class Tween<T> : Effect<T> { }\n}\n";
    let result = run("csharp", "E.cs", source);
    let ids: BTreeSet<&str> = array(&result, "nodes")
        .iter()
        .filter(|n| n["label"] == "Effect")
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert_eq!(ids, BTreeSet::from(["e_n_effect", "e_n_effect_1"]));
    let inherits: BTreeSet<(String, String)> = array(&result, "edges")
        .iter()
        .filter(|e| e["relation"] == "inherits")
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_string(),
                e["target"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        inherits,
        BTreeSet::from([
            pair("e_n_effect_1", "e_n_effect"),
            pair("e_n_tween_1", "e_n_effect_1"),
        ])
    );
}

#[test]
fn enum_members_are_not_call_targets() {
    let source = "enum Kind { Delegate, Other }\nclass Runner {\n  System.Action Delegate;\n  void Run() { Delegate(); }\n}\n";
    let result = run("csharp", "K.cs", source);
    assert!(edges(&result, "calls").is_empty(), "{result:#}");
    assert_eq!(raw_calls(&result, "Delegate").len(), 1);
}

// ── 0.9.85 / 0.9.86 ─────────────────────────────────────────────────────────

#[test]
fn scala_givens_scope_their_members() {
    let source = "trait Show[A] { def show(a: A): String }\ngiven Show[Int] with\n  def show(a: Int): String = a.toString\ngiven intOrd: Ordering[Int] with\n  def compare(x: Int, y: Int): Int = 0\n";
    let result = run("scala", "g.scala", source);
    let methods = edges(&result, "method");
    assert!(
        methods.contains(&pair("given Show", ".show()")),
        "{methods:?}"
    );
    assert!(
        methods.contains(&pair("intOrd", ".compare()")),
        "{methods:?}"
    );
}

#[test]
fn scala_derives_clauses_reference_their_typeclasses() {
    let source =
        "case class P(x: Int) derives Show, cats.Eq\nenum Color derives CanEqual { case Red }\n";
    let result = run("scala", "d.scala", source);
    let derives: BTreeSet<(String, String)> = array(&result, "edges")
        .iter()
        .filter(|e| e["context"] == "derives")
        .map(|e| {
            (
                label_of(&result, &e["source"]),
                label_of(&result, &e["target"]),
            )
        })
        .collect();
    assert_eq!(
        derives,
        BTreeSet::from([
            pair("P", "Show"),
            pair("P", "Eq"),
            pair("Color", "CanEqual")
        ])
    );
}

#[test]
fn js_export_without_from_is_not_a_reexport() {
    let result = run(
        "js",
        "a.js",
        "export default \"./b\";\nexport * from './c';\n",
    );
    let targets: Vec<&str> = array(&result, "edges")
        .iter()
        .filter(|e| e["relation"] == "imports_from")
        .filter_map(|e| e["target"].as_str())
        .collect();
    assert_eq!(targets, ["ref_c"]);
}

// ── 0.9.87 ──────────────────────────────────────────────────────────────────

#[test]
fn python_property_reads_are_raw_calls_on_typed_receivers() {
    let source = "class Response:\n    @property\n    def text(self): return ''\n    @cached_property\n    def size(self): return 0\n    def plain(self): pass\nclass Api:\n    def __init__(self):\n        self.resp = Response()\n        self.count = 0\n    def run(self, r: Response):\n        r.text\n        self.resp.size\n        self.count\n        self.flag\n        r.text = 'x'\n        r.plain()\n";
    let result = run("python", "api.py", source);
    assert_eq!(node(&result, ".text()")["_py_property"], true);
    assert_eq!(node(&result, ".size()")["_py_property"], true);
    assert!(node(&result, ".plain()").get("_py_property").is_none());
    let reads: Vec<(String, String)> = array(&result, "raw_calls")
        .iter()
        .filter(|c| c["_python_attr_read"] == true)
        .map(|c| {
            (
                c["callee"].as_str().unwrap().to_string(),
                c["receiver"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    // `self.count` is a field the class assigns; `r.text = ...` is a store and
    // `r.plain()` a call. The walk visits the body last statement first.
    assert_eq!(
        reads,
        [
            pair("flag", "self"),
            pair("size", "self.resp"),
            pair("text", "r")
        ]
    );
    let size = array(&result, "raw_calls")
        .iter()
        .find(|c| c["callee"] == "size")
        .unwrap();
    assert_eq!(size["_python_self_attr_type"], "Response");
}

#[test]
fn php_static_calls_name_the_method_on_the_scope() {
    let source = "<?php\nclass Helper { public static function format() {} }\nfunction run() { Helper::format(); }\n";
    let result = run("php", "h.php", source);
    let call = raw_call(&result, "format");
    assert_eq!(call["receiver"], "Helper");
    assert_eq!(call["is_member_call"], true);
    assert!(edges(&result, "calls").is_empty());
}

#[test]
fn csharp_partial_types_carry_their_type_path() {
    let source = "namespace N {\n  partial class Outer<T> {\n    partial class Inner { }\n  }\n  class A { partial class Same { } }\n  class B { partial class Same { } }\n}\n";
    let result = run("csharp", "P.cs", source);
    assert_eq!(node(&result, "Outer")["metadata"]["type_path"], "Outer`1");
    assert_eq!(
        node(&result, "Inner")["metadata"]["type_path"],
        "Outer`1.Inner"
    );
    // Two nested types share one id but not one path: neither keeps one.
    let same = node(&result, "Same");
    assert_eq!(same["metadata"]["is_partial"], true);
    assert!(same["metadata"].get("type_path").is_none());
}

#[test]
fn csharp_var_from_a_call_defers_to_its_return_type() {
    let source = "class Factory {\n  public static Widget Make<T>() => null;\n  public T Get<T>() => default;\n  void Run(Factory f) {\n    var w = Factory.Make<Gear>();\n    w.Spin();\n    var g = f.Get<Gear>();\n    g.Turn();\n    var x = local.Get<Gear>();\n    x.Stop();\n  }\n}\n";
    let result = run("csharp", "F.cs", source);
    assert_eq!(
        raw_call(&result, "Spin")["receiver_call"],
        serde_json::json!(["Factory", "Make", ["Gear"]])
    );
    assert_eq!(
        raw_call(&result, "Turn")["receiver_call"],
        serde_json::json!(["Factory", "Get", ["Gear"]])
    );
    // An untyped lowercase receiver: no guess.
    assert!(raw_call(&result, "Stop").get("receiver_call").is_none());
    let returns = array(&result, "member_field_tables")
        .iter()
        .find(|t| t["lang"] == "csharp_returns")
        .unwrap();
    assert_eq!(returns["class_label"], "Factory");
    assert_eq!(
        returns["fields"]["Make"],
        serde_json::json!([["Widget", 1]])
    );
    assert_eq!(returns["fields"]["Get"], serde_json::json!([["#0", 1]]));
    assert_eq!(returns["fields"]["Run"], serde_json::json!([[null, 0]]));
}

#[test]
fn ts_public_twins_are_marked() {
    let source = "class A {\n  foo() {}\n  #foo() {}\n  #bar() {}\n}\n";
    let result = run("ts", "a.ts", source);
    let marked: Vec<&str> = array(&result, "nodes")
        .iter()
        .filter(|n| n["_ts_public_twin"] == true)
        .filter_map(|n| n["label"].as_str())
        .collect();
    assert_eq!(marked, [".foo()"]);
}

// ── 0.9.88 ──────────────────────────────────────────────────────────────────

#[test]
fn self_is_not_a_type_reference() {
    let python = "from typing import Self\nimport typing\nclass A:\n    def a(self) -> Self: ...\n    def b(self) -> typing.Self: ...\n";
    let result = run("python", "a.py", python);
    assert!(array(&result, "nodes").iter().all(|n| n["id"] != "self"));
    let swift = "class A {\n  func copy() -> Self { return self }\n  func list() -> [Self] { return [] }\n}\n";
    let result = run("swift", "a.swift", swift);
    assert!(array(&result, "nodes").iter().all(|n| n["id"] != "self"));
}

#[test]
fn python_property_accessor_decorators_are_not_references() {
    let source = "class A:\n    @property\n    def x(self): return 1\n    @x.setter\n    def x(self, v): pass\n    @x.deleter\n    def x(self): pass\n    @cache.memo\n    def y(self): pass\n";
    let result = run("python", "a.py", source);
    let decorators: BTreeSet<String> = array(&result, "edges")
        .iter()
        .filter(|e| e["context"] == "decorator")
        .map(|e| label_of(&result, &e["target"]))
        .collect();
    assert_eq!(decorators, BTreeSet::from(["memo".to_string()]));
    assert!(
        array(&result, "nodes")
            .iter()
            .all(|n| !matches!(n["label"].as_str(), Some("setter" | "deleter")))
    );
}

#[test]
fn python_forward_reference_to_the_file_stem_class() {
    let source = "def build() -> Widget:\n    return Widget()\nclass Widget:\n    pass\n";
    let result = run("python", "pkg/widget.py", source);
    assert!(array(&result, "nodes").iter().all(|n| n["id"] != "widget"));
    let refs = edges(&result, "references");
    assert!(refs.contains(&pair("build()", "Widget")), "{refs:?}");
    // A name that is not a class here keeps its stub for the cross-file rewire.
    let other = run(
        "python",
        "pkg/helper.py",
        "def f() -> Helper: ...\ndef helper(): pass\n",
    );
    assert!(
        array(&other, "nodes")
            .iter()
            .any(|n| n["id"] == "helper" && n["source_file"] == "")
    );
}

#[test]
fn java_receiver_chains_unbound_fields_and_return_types() {
    let source = "class Repo extends Base {\n  Store store;\n  Item find() { return null; }\n  Item find(int i) { return null; }\n  Store open() { return store; }\n  void run(Item item) {\n    open().commit();\n    new Store().flush();\n    ((Store) item).close();\n    Factory.create().start();\n    item.save();\n    store.sync();\n    inherited.touch();\n    this.helper();\n  }\n}\n";
    let result = run("java", "Repo.java", source);
    let chain = |callee: &str| {
        raw_call(&result, callee)
            .get("java_receiver_chain")
            .cloned()
    };
    assert_eq!(
        chain("commit"),
        Some(serde_json::json!(["call", "open", null]))
    );
    assert_eq!(chain("flush"), Some(serde_json::json!(["type", "Store"])));
    assert_eq!(chain("close"), Some(serde_json::json!(["type", "Store"])));
    assert_eq!(
        chain("start"),
        Some(serde_json::json!(["call", "create", ["type", "Factory"]]))
    );
    let unbound = |callee: &str| raw_call(&result, callee).get("receiver_unbound").is_some();
    assert!(unbound("touch"));
    assert!(unbound("helper"));
    assert!(!unbound("save") && !unbound("sync"));
    assert_eq!(node(&result, ".open()")["_java_ret"], "Store");
    // Overloads that agree keep the type.
    assert_eq!(node(&result, ".find()")["_java_ret"], "Item");
    assert!(node(&result, ".run()").get("_java_ret").is_none());
}

#[test]
fn unresolved_quoted_includes_keep_their_spelling() {
    let result = run(
        "c",
        "/nonexistent/a.c",
        "#include \"lib/util.h\"\n#include <stdio.h>\n",
    );
    let imports: Vec<&Value> = array(&result, "edges")
        .iter()
        .filter(|e| e["relation"] == "imports")
        .collect();
    assert_eq!(imports.len(), 2);
    assert_eq!(
        imports[0]["metadata"],
        serde_json::json!({"raw_include": "lib/util.h", "include_kind": "quoted"})
    );
    assert!(imports[1].get("metadata").is_none());
}
