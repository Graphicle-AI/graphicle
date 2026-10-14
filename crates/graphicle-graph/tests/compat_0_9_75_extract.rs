//! Mirrors the 0.9.75 additions to upstream `tests/test_extract.py`.
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

/// `_single_file_call_pairs`: `(caller id, callee id)` for every `calls` edge
/// of one `svc.<ext>` file.
fn call_pairs(source: &str, ext: &str) -> BTreeSet<(String, String)> {
    let fx = Fixture::new();
    let path = fx.write(&format!("svc.{ext}"), source);
    let result = fx.extract(&[path]);
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls")
        .filter_map(|e| Some((e["source"].as_str()?.into(), e["target"].as_str()?.into())))
        .collect()
}

fn extract_one(name: &str, source: &str) -> Value {
    let fx = Fixture::new();
    let path = fx.write(name, source);
    fx.extract(&[path])
}

fn labels(result: &Value) -> BTreeSet<String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}

/// `(source label, target label)` for every `relation` edge.
fn edge_labels(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    let by_id: std::collections::HashMap<&str, &str> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n["label"].as_str()?)))
        .collect();
    let label = |v: &Value| {
        let id = v.as_str().unwrap_or("");
        by_id.get(id).copied().unwrap_or(id).to_string()
    };
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| (label(&e["source"]), label(&e["target"])))
        .collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

fn has(calls: &BTreeSet<(String, String)>, src: &str, tgt: &str) -> bool {
    calls.contains(&(src.to_string(), tgt.to_string()))
}

#[test]
fn python_self_call_binds_to_own_class_not_last_declared() {
    let calls = call_pairs(
        "class Server:\n\
         \x20   def save(self): return 1\n\
         \x20   def flush(self): return self.save()\n\
         \x20   @classmethod\n\
         \x20   def make(cls): return cls.build()\n\
         \x20   @classmethod\n\
         \x20   def build(cls): return cls()\n\
         \x20   def deferred(self):\n\
         \x20       def inner():\n\
         \x20           return self.save()\n\
         \x20       return inner()\n\n\
         class Cache:\n\
         \x20   def save(self): return 2\n\
         \x20   def flush(self): return self.save()\n\
         \x20   def build(self): return 3\n",
        "py",
    );
    assert!(
        has(&calls, "svc_server_flush", "svc_server_save"),
        "{calls:?}"
    );
    assert!(!has(&calls, "svc_server_flush", "svc_cache_save"));
    assert!(
        has(&calls, "svc_server_make", "svc_server_build"),
        "cls.build() stays in the class"
    );
    assert!(
        has(&calls, "svc_server_deferred_inner", "svc_server_save"),
        "a nested def closes over the enclosing method's self: {calls:?}"
    );
    assert!(has(&calls, "svc_cache_flush", "svc_cache_save"));
}

#[test]
fn python_self_call_never_binds_to_an_unrelated_class() {
    let calls = call_pairs(
        "class Server:\n\
         \x20   def run(self): return self.ping()\n\n\
         class Cache:\n\
         \x20   def ping(self): return 1\n",
        "py",
    );
    assert!(
        !has(&calls, "svc_server_run", "svc_cache_ping"),
        "{calls:?}"
    );
}

#[test]
fn python_self_and_super_calls_walk_in_file_bases() {
    let calls = call_pairs(
        "class Base:\n\
         \x20   def save(self): return 0\n\
         \x20   def ping(self): return 0\n\n\
         class Child(Base):\n\
         \x20   def save(self): return super().save()\n\
         \x20   def run(self): return self.ping()\n\n\
         class Other:\n\
         \x20   def save(self): return 9\n\
         \x20   def ping(self): return 9\n",
        "py",
    );
    assert!(has(&calls, "svc_child_run", "svc_base_ping"), "{calls:?}");
    assert!(!has(&calls, "svc_child_run", "svc_other_ping"));
    assert!(has(&calls, "svc_child_save", "svc_base_save"), "{calls:?}");
    assert!(!has(&calls, "svc_child_save", "svc_other_save"));
}

#[test]
fn python_self_call_multiple_inheritance_tie_binds_nothing() {
    let calls = call_pairs(
        "class A:\n\
         \x20   def m(self): return 1\n\n\
         class B:\n\
         \x20   def m(self): return 2\n\n\
         class C(A, B):\n\
         \x20   def run(self): return self.m()\n",
        "py",
    );
    assert!(!calls.iter().any(|(s, _)| s == "svc_c_run"), "{calls:?}");
}

#[test]
fn python_self_call_to_stored_module_function_still_binds() {
    let calls = call_pairs(
        "def handler(): return 1\n\n\
         class Job:\n\
         \x20   def __init__(self): self.handler = handler\n\
         \x20   def run(self): return self.handler()\n",
        "py",
    );
    assert!(has(&calls, "svc_job_run", "svc_handler"), "{calls:?}");
}

#[test]
fn js_this_call_binds_to_own_class_not_last_declared() {
    for ext in ["ts", "js"] {
        let calls = call_pairs(
            "class Server {\n\
             \x20 save() { return 1; }\n\
             \x20 flush() { return this.save(); }\n\
             \x20 later() { return [1].map(() => this.save()); }\n\
             }\n\
             class Cache {\n\
             \x20 save() { return 2; }\n\
             \x20 flush() { return this.save(); }\n\
             }\n",
            ext,
        );
        assert!(
            has(&calls, "svc_server_flush", "svc_server_save"),
            "{ext}: {calls:?}"
        );
        assert!(!has(&calls, "svc_server_flush", "svc_cache_save"), "{ext}");
        assert!(
            has(&calls, "svc_server_later", "svc_server_save"),
            "{ext}: {calls:?}"
        );
        assert!(has(&calls, "svc_cache_flush", "svc_cache_save"), "{ext}");
    }
}

#[test]
fn js_this_call_to_inherited_method_keeps_its_edge() {
    for ext in ["ts", "js"] {
        let calls = call_pairs(
            "class Base {\n\
             \x20 ping() { return 0; }\n\
             }\n\
             class Server extends Base {\n\
             \x20 run() { return this.ping(); }\n\
             }\n",
            ext,
        );
        assert!(
            has(&calls, "svc_server_run", "svc_base_ping"),
            "{ext}: {calls:?}"
        );
    }
}

/// b523898: `super.greet()` never means the caller's own override.
#[test]
fn js_super_call_does_not_self_loop_onto_the_overriding_method() {
    for ext in ["ts", "js"] {
        let calls = call_pairs(
            "class Base {\n\
             \x20 greet() { return 0; }\n\
             }\n\
             class Server extends Base {\n\
             \x20 greet() { return super.greet(); }\n\
             }\n",
            ext,
        );
        assert!(
            !has(&calls, "svc_server_greet", "svc_server_greet"),
            "{ext}: {calls:?}"
        );
    }
}

#[test]
fn swift_self_calls_bind_within_own_class_chain() {
    let calls = call_pairs(
        "class Base {\n\
         \x20   func ping() -> Int { return 0 }\n\
         }\n\
         class Server: Base {\n\
         \x20   func save() -> Int { return 1 }\n\
         \x20   func flush() -> Int { return self.save() }\n\
         \x20   func bare() -> Int { return save() }\n\
         \x20   func run() -> Int { return self.ping() }\n\
         \x20   func zuper() -> Int { return super.ping() }\n\
         }\n\
         class Cache {\n\
         \x20   func save() -> Int { return 2 }\n\
         \x20   func ping() -> Int { return 3 }\n\
         }\n",
        "swift",
    );
    assert!(
        has(&calls, "svc_server_flush", "svc_server_save"),
        "{calls:?}"
    );
    assert!(
        has(&calls, "svc_server_bare", "svc_server_save"),
        "{calls:?}"
    );
    assert!(has(&calls, "svc_server_run", "svc_base_ping"), "{calls:?}");
    assert!(
        has(&calls, "svc_server_zuper", "svc_base_ping"),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|(_, t)| t.starts_with("svc_cache_")),
        "{calls:?}"
    );
}

#[test]
fn swift_bare_call_to_free_function_and_extension_keep_their_edges() {
    let calls = call_pairs(
        "func helper() -> Int { return 1 }\n\
         class Foo {\n\
         \x20   func a() -> Int { return helper() }\n\
         \x20   func make() -> Foo { return Foo() }\n\
         }\n\
         extension Foo {\n\
         \x20   func b() -> Int { return a() }\n\
         }\n",
        "swift",
    );
    assert!(has(&calls, "svc_foo_a", "svc_helper"), "{calls:?}");
    assert!(has(&calls, "svc_foo_make", "svc_foo"), "{calls:?}");
    assert!(has(&calls, "svc_foo_b", "svc_foo_a"), "{calls:?}");
}

#[test]
fn ruby_self_sends_bind_within_own_class_chain() {
    let calls = call_pairs(
        "class Base\n\
         \x20 def ping; 0; end\n\
         end\n\
         class Server < Base\n\
         \x20 def save; 1; end\n\
         \x20 def flush; self.save; end\n\
         \x20 def bare; save; end\n\
         \x20 def run; self.ping; end\n\
         end\n\
         class Cache\n\
         \x20 def save; 2; end\n\
         \x20 def ping; 3; end\n\
         end\n",
        "rb",
    );
    assert!(
        has(&calls, "svc_server_flush", "svc_server_save"),
        "{calls:?}"
    );
    assert!(
        has(&calls, "svc_server_bare", "svc_server_save"),
        "{calls:?}"
    );
    assert!(has(&calls, "svc_server_run", "svc_base_ping"), "{calls:?}");
    assert!(
        !calls.iter().any(|(_, t)| t.starts_with("svc_cache_")),
        "{calls:?}"
    );
}

#[test]
fn ruby_implicit_self_keeps_top_level_and_mixin_edges() {
    let calls = call_pairs(
        "def helper\n\
         \x20 1\n\
         end\n\
         module Greet\n\
         \x20 def hi; 1; end\n\
         end\n\
         class Widget\n\
         \x20 include Greet\n\
         \x20 def use_helper; helper; end\n\
         \x20 def say; hi; end\n\
         end\n",
        "rb",
    );
    assert!(
        has(&calls, "svc_widget_use_helper", "svc_helper"),
        "{calls:?}"
    );
    assert!(has(&calls, "svc_widget_say", "svc_greet_hi"), "{calls:?}");
}

#[test]
fn zig_error_set_members_emit_case_of_nodes() {
    let r = extract_one(
        "errors.zig",
        "const FileError = error{ NotFound, PermissionDenied };\n\
         fn open() FileError!void { return error.NotFound; }\n",
    );
    let labels = labels(&r);
    assert!(labels.contains("FileError"), "{labels:?}");
    assert!(labels.contains("NotFound") && labels.contains("PermissionDenied"));
    let case_of = edge_labels(&r, "case_of");
    assert!(
        case_of.contains(&pair("FileError", "NotFound")),
        "{case_of:?}"
    );
    assert!(case_of.contains(&pair("FileError", "PermissionDenied")));
}

#[test]
fn zig_tagged_union_variants_emit_case_of_nodes() {
    let r = extract_one(
        "shapes.zig",
        "const Shape = union(enum) {\n\
         \x20   circle: f64,\n\
         \x20   rectangle: struct { w: f64, h: f64 },\n\
         \x20   point,\n\
         };\n\
         const Payload = union(Tag) { int: i64, text: []const u8 };\n\
         const Bare = union { int: i64, float: f64 };\n",
    );
    let labels = labels(&r);
    for want in ["circle", "rectangle", "point", "int", "text"] {
        assert!(labels.contains(want), "{want}: {labels:?}");
    }
    let case_of = edge_labels(&r, "case_of");
    for (a, b) in [
        ("Shape", "circle"),
        ("Shape", "rectangle"),
        ("Shape", "point"),
        ("Payload", "int"),
        ("Payload", "text"),
    ] {
        assert!(case_of.contains(&pair(a, b)), "{a}->{b}: {case_of:?}");
    }
    assert!(!case_of.iter().any(|(s, _)| s == "Bare"), "{case_of:?}");
    assert!(!labels.contains("float"));
}

#[test]
fn php_enum_cases_have_case_of_edge() {
    let r = extract_one(
        "suit.php",
        "<?php\n\
         enum Suit: string {\n\
         \x20   case Hearts = 'H';\n\
         \x20   case Spades = 'S';\n\
         \x20   public function color(): string { return 'x'; }\n\
         }\n\
         enum Status {\n\
         \x20   case Active;\n\
         \x20   case Closed;\n\
         }\n",
    );
    let labels = labels(&r);
    for want in ["Hearts", "Spades", "Active", "Closed", ".color()"] {
        assert!(labels.contains(want), "{want}: {labels:?}");
    }
    let case_of = edge_labels(&r, "case_of");
    for (a, b) in [
        ("Suit", "Hearts"),
        ("Suit", "Spades"),
        ("Status", "Active"),
        ("Status", "Closed"),
    ] {
        assert!(case_of.contains(&pair(a, b)), "{a}->{b}: {case_of:?}");
    }
}

#[test]
fn elixir_defmacro_and_defguard_are_extracted() {
    let r = extract_one(
        "macros.ex",
        "defmodule MyMod do\n\
         \x20 defmacro trace(expr) do\n\
         \x20   quote do: unquote(expr)\n\
         \x20 end\n\
         \n\
         \x20 defmacrop priv_macro(x) do\n\
         \x20   quote do: unquote(x)\n\
         \x20 end\n\
         \n\
         \x20 defguard is_even(x) when is_integer(x) and rem(x, 2) == 0\n\
         \n\
         \x20 def run(x) do\n\
         \x20   trace(priv_macro(x))\n\
         \x20 end\n\
         end\n",
    );
    let labels: BTreeSet<String> = labels(&r)
        .into_iter()
        .map(|l| l.trim_end_matches(['(', ')']).to_string())
        .collect();
    for want in ["trace", "priv_macro", "is_even"] {
        assert!(labels.contains(want), "{want}: {labels:?}");
    }
    let calls = edge_labels(&r, "calls");
    assert!(calls.contains(&pair("run()", "trace()")), "{calls:?}");
}

#[test]
fn rust_macro_rules_definitions_and_invocations() {
    let r = extract_one(
        "lib.rs",
        "macro_rules! my_vec {\n\
         \x20   ($($x:expr),*) => { vec![$($x),*] };\n\
         }\n\
         fn build() -> Vec<i32> {\n\
         \x20   my_vec!(1, 2, 3)\n\
         }\n",
    );
    assert!(labels(&r).contains("my_vec!"));
    let calls = edge_labels(&r, "calls");
    assert!(calls.contains(&pair("build()", "my_vec!")), "{calls:?}");
}

#[test]
fn rust_macro_does_not_collide_with_same_named_function() {
    let r = extract_one(
        "lib.rs",
        "macro_rules! thing {\n\
         \x20   () => { 0 };\n\
         }\n\
         fn thing() -> i32 { 1 }\n\
         fn run() -> i32 {\n\
         \x20   thing!()\n\
         }\n",
    );
    let labels = labels(&r);
    assert!(
        labels.contains("thing!") && labels.contains("thing()"),
        "{labels:?}"
    );
    let calls = edge_labels(&r, "calls");
    assert!(calls.contains(&pair("run()", "thing!")), "{calls:?}");
    assert!(!calls.contains(&pair("run()", "thing()")));
}

#[test]
fn rust_scoped_macro_invocation_stays_unresolved() {
    let r = extract_one("lib.rs", "fn run() {\n    log::info!(\"hi\");\n}\n");
    assert!(edge_labels(&r, "calls").is_empty());
}

fn reference_edges(result: &Value) -> BTreeSet<(String, String, String)> {
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "references")
        .map(|e| {
            let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
            (s("source"), s("target"), s("context"))
        })
        .collect()
}

const SCALA_TYPES: &str = "package demo

class Config
class Service
trait Ordering[A]
trait Async[F[_]]
trait Temporal[F[_]]

val cfg: Config = new Config()
var svc: Option[Service] = None

type Alias = Map[String, Config]
opaque type Id = Service
type Elem[X] = X match {
  case String => Char
  case List[t] => t
}

object Holder {
  val inner: Service = new Service()
  type Local = Config
}

def sort[A: Ordering](xs: List[A]): List[A] = xs
def program[F[_]: Async: Temporal](n: Config): Unit = ()
def bounded[T <: Comparable[T]](t: T): T = t
";

#[test]
fn scala_file_scope_bindings_and_type_aliases_are_owned_by_the_file() {
    // Graphify 447d41d (#2054) and cb973c0 (#2049).
    let refs = reference_edges(&extract_one("Main.scala", SCALA_TYPES));
    let has = |s: &str, t: &str, c: &str| refs.contains(&(s.into(), t.into(), c.into()));
    for (target, context) in [
        ("main_config", "field"),
        ("option", "field"),
        ("main_service", "generic_arg"),
        ("map", "field"),
        ("string", "generic_arg"),
        ("main_config", "generic_arg"),
        ("main_service", "field"),
        ("char", "field"),
        ("string", "field"),
    ] {
        assert!(
            has("main", target, context),
            "main -> {target} [{context}]: {refs:?}"
        );
    }
    assert!(has("main_holder", "main_service", "field"));
    assert!(has("main_holder", "main_config", "field"));
    assert!(!refs.iter().any(|(s, t, _)| s == "main" && t == "main"));
}

#[test]
fn scala_context_bounds_resolve_to_type_bound_references() {
    // Graphify 562dfd6 (#2046): bounds, including both on a higher-kinded
    // parameter; an upper bound is not a dependency.
    let refs = reference_edges(&extract_one("Main.scala", SCALA_TYPES));
    let has = |s: &str, t: &str, c: &str| refs.contains(&(s.into(), t.into(), c.into()));
    assert!(has("main_sort", "main_ordering", "type_bound"));
    assert!(has("main_program", "main_async", "type_bound"));
    assert!(has("main_program", "main_temporal", "type_bound"));
    assert!(
        !refs
            .iter()
            .any(|(s, _, c)| s == "main_bounded" && c == "type_bound")
    );
    // The value parameters are found by node type, not the shared field name.
    assert!(has("main_sort", "list", "parameter_type"));
    assert!(has("main_program", "main_config", "parameter_type"));
    assert!(has("main_bounded", "t", "parameter_type"));
}

// ---- Swift protocol dispatch (Graphify 0bd22d4 + f1f363c) ----

/// `(source id, target id)` for every `dispatches_to` edge of a corpus.
fn swift_dispatch(files: &[(&str, &str)]) -> (BTreeSet<(String, String)>, Value) {
    let fx = Fixture::new();
    let paths: Vec<PathBuf> = files
        .iter()
        .map(|(rel, text)| fx.write(rel, text))
        .collect();
    let result = fx.extract(&paths);
    let pairs = result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "dispatches_to")
        .filter_map(|e| Some((e["source"].as_str()?.into(), e["target"].as_str()?.into())))
        .collect();
    (pairs, result)
}

const SWIFT_INJECTED: &[(&str, &str)] = &[
    (
        "Core/Store.swift",
        "protocol Store {\n    func fetch() async throws -> [Item]\n}\nstruct Item { let id: Int }\n",
    ),
    (
        "Services/RemoteStore.swift",
        "struct RemoteStore: Store {\n    func fetch() async throws -> [Item] { try decode() }\n    func decode() throws -> [Item] { [] }\n}\n",
    ),
    (
        "App/ItemModel.swift",
        "final class ItemModel {\n    private let store: Store\n    init(store: Store) { self.store = store }\n    func load() async throws -> [Item] { try await store.fetch() }\n}\n",
    ),
];

#[test]
fn swift_single_conformer_links_requirement_to_implementation() {
    let (dispatch, result) = swift_dispatch(SWIFT_INJECTED);
    assert_eq!(
        dispatch,
        BTreeSet::from([pair(
            "core_store_store_fetch",
            "services_remotestore_remotestore_fetch"
        )])
    );
    let edge = result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["relation"] == "dispatches_to")
        .unwrap();
    assert_eq!(edge["confidence"], "INFERRED");
    assert_eq!(edge["context"], "call");
    assert_eq!(
        edge["source_file"],
        result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "services_remotestore_remotestore_fetch")
            .unwrap()["source_file"]
    );
    // The injected call still lands on the protocol requirement.
    let calls = edge_labels(&result, "calls");
    assert!(calls.contains(&pair(".load()", ".fetch()")), "{calls:?}");
}

#[test]
fn swift_dispatch_makes_the_injected_chain_reachable() {
    let (_, result) = swift_dispatch(SWIFT_INJECTED);
    let edges = result["edges"].as_array().unwrap();
    let step = |src: &str, rels: &[&str]| -> Vec<String> {
        edges
            .iter()
            .filter(|e| e["source"] == src && rels.contains(&e["relation"].as_str().unwrap_or("")))
            .filter_map(|e| e["target"].as_str().map(str::to_string))
            .collect()
    };
    let mut seen = BTreeSet::new();
    let mut frontier = vec!["app_itemmodel_itemmodel_load".to_string()];
    while let Some(id) = frontier.pop() {
        if seen.insert(id.clone()) {
            frontier.extend(step(&id, &["calls", "dispatches_to"]));
        }
    }
    assert!(
        seen.contains("services_remotestore_remotestore_decode"),
        "{seen:?}"
    );
}

#[test]
fn swift_extension_and_same_file_class_conformances_link() {
    let (dispatch, _) = swift_dispatch(&[
        (
            "Sink.swift",
            "protocol Sink {\n    func put()\n}\nclass Disk {}\nextension Disk: Sink {\n    func put() {}\n}\n",
        ),
        (
            "Cache.swift",
            "protocol Cache { func get() }\nclass LocalCache: Cache { func get() {} }\n",
        ),
    ]);
    assert_eq!(
        dispatch,
        BTreeSet::from([
            pair("cache_cache_get", "cache_localcache_get"),
            pair("sink_sink_put", "sink_disk_put"),
        ])
    );
}

#[test]
fn swift_builtin_conformance_is_skipped_while_static_requirements_link() {
    let (dispatch, _) = swift_dispatch(&[(
        "V.swift",
        "protocol Factory { static func make() -> Self }\nstruct Box: Factory, Equatable { static func make() -> Box { Box() } }\n",
    )]);
    assert_eq!(
        dispatch,
        BTreeSet::from([pair("v_factory_make", "v_box_make")])
    );
}

#[test]
fn swift_ambiguous_or_unmatched_conformances_are_not_dispatched() {
    for files in [
        // Two conformers.
        &[(
            "Loud.swift",
            "protocol Loud { func shout() }\nstruct A1: Loud { func shout() {} }\nstruct A2: Loud { func shout() {} }\n",
        )][..],
        // No matching member.
        &[(
            "P.swift",
            "protocol P { func run() }\nstruct Q: P { func walk() {} }\n",
        )][..],
        // Default implementation in a protocol extension.
        &[(
            "G.swift",
            "protocol Greeter { func greet() }\nextension Greeter { func greet() {} }\nstruct Plain: Greeter {}\n",
        )][..],
        // A class conformer whose protocol lives in another file resolves as `inherits`.
        &[
            ("Store.swift", "protocol Store { func fetch() }\n"),
            ("Remote.swift", "class Remote: Store { func fetch() {} }\n"),
        ][..],
        // A Kotlin conformer of a same-named Swift protocol.
        &[
            ("Store.swift", "protocol Store { func fetch() }\n"),
            (
                "RemoteStore.kt",
                "class RemoteStore : Store { fun fetch() {} }\n",
            ),
        ][..],
    ] {
        let (dispatch, _) = swift_dispatch(files);
        assert!(dispatch.is_empty(), "{files:?} -> {dispatch:?}");
    }
}

#[test]
fn swift_pass_leaves_csharp_pairs_to_the_csharp_pass() {
    use serde_json::json;
    let nodes = vec![
        json!({"id": "iface", "label": "IR", "source_file": "a.cs", "_callable_class": true}),
        json!({"id": "impl", "label": "A", "source_file": "a.cs", "_callable_class": true}),
        json!({"id": "iface_m", "label": ".M()", "source_file": "a.cs", "_callable": true}),
        json!({"id": "impl_m", "label": ".M()", "source_file": "a.cs", "_callable": true}),
    ];
    let mut edges = vec![
        json!({"source": "impl", "target": "iface", "relation": "implements"}),
        json!({"source": "iface", "target": "iface_m", "relation": "method"}),
        json!({"source": "impl", "target": "impl_m", "relation": "method"}),
    ];
    graphicle::swift_dispatch::resolve_swift_protocol_dispatch(&[], &nodes, &mut edges).unwrap();
    assert_eq!(edges.len(), 3);
    graphicle::csharp_dispatch::resolve_csharp_interface_dispatch(&[], &nodes, &mut edges).unwrap();
    assert_eq!(edges.len(), 4);
    assert_eq!(edges[3]["relation"], "dispatches_to");
}

#[test]
fn swift_pass_is_registered_after_the_csharp_pass() {
    graphicle::extract::register_resolvers();
    let names: Vec<String> = graphicle::resolver_registry::registered_resolvers()
        .into_iter()
        .map(|r| r.name)
        .collect();
    let at = |name: &str| names.iter().position(|n| n == name).unwrap();
    assert!(
        at("csharp_interface_dispatch") < at("swift_protocol_dispatch"),
        "{names:?}"
    );
}

// ---- C# partial-class merge keeps recursive calls (Graphify 117e6b4) ----

const FOO_PART_A: &str =
    "namespace App {\n    public partial class Foo {\n        public void Alpha() {}\n    }\n}\n";
const FOO_PART_B: &str = "namespace App {\n    public partial class Foo {\n        public void Beta() { Alpha(); }\n    }\n}\n";

fn corpus(files: &[(&str, &str)]) -> Value {
    let fx = Fixture::new();
    let paths: Vec<PathBuf> = files
        .iter()
        .map(|(rel, text)| fx.write(rel, text))
        .collect();
    fx.extract(&paths)
}

/// Names (without a trailing `()`) of functions with a recursive `calls` loop.
fn self_calls(result: &Value) -> BTreeSet<String> {
    edge_labels(result, "calls")
        .into_iter()
        .filter(|(a, b)| a == b)
        .map(|(a, _)| a.strip_suffix("()").unwrap_or(&a).to_string())
        .collect()
}

fn recursive_ids(result: &Value) -> usize {
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "calls" && e["source"] == e["target"])
        .count()
}

#[test]
fn csharp_partial_class_keeps_recursive_calls_in_other_languages() {
    let r = corpus(&[
        ("FooPartA.cs", FOO_PART_A),
        ("FooPartB.cs", FOO_PART_B),
        (
            "tree.py",
            "def walk(node):\n    for child in node.children:\n        walk(child)\n",
        ),
        (
            "tree.ts",
            "export function visit(n: any): void {\n  for (const c of n.children) visit(c);\n}\n",
        ),
    ]);
    let foo = r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["label"] == "Foo")
        .count();
    assert_eq!(foo, 1, "the partial halves still merge");
    let calls = self_calls(&r);
    assert!(
        calls.contains("walk") && calls.contains("visit"),
        "{calls:?}"
    );
    assert!(recursive_ids(&r) >= 2);
}

#[test]
fn csharp_partial_class_keeps_recursive_call_inside_a_half() {
    let r = corpus(&[
        (
            "FooPartA.cs",
            "namespace App {\n    public partial class Foo {\n        public int Depth(int n) { return n <= 0 ? 0 : 1 + Depth(n - 1); }\n    }\n}\n",
        ),
        ("FooPartB.cs", FOO_PART_B),
    ]);
    assert!(self_calls(&r).contains(".Depth"), "{:?}", self_calls(&r));
}

fn partial_half(id: &str, file: &str) -> Value {
    serde_json::json!({"id": id, "label": "Foo", "source_file": file, "file_type": "code",
        "metadata": {"is_partial": true, "namespace": "App"}})
}

fn merged(
    mut nodes: Vec<Value>,
    mut edges: Vec<Value>,
) -> BTreeSet<(String, String, String, String)> {
    let root = tempfile::tempdir().unwrap();
    graphicle::extract_merge::merge_csharp_partials(
        &mut [],
        &mut nodes,
        &mut edges,
        &[],
        root.path(),
    );
    edges
        .iter()
        .map(|e| {
            let f = |k: &str| e[k].as_str().unwrap_or("").to_string();
            (f("source"), f("target"), f("relation"), f("confidence"))
        })
        .collect()
}

#[test]
fn csharp_partial_merge_drops_only_the_self_loops_it_creates() {
    use serde_json::json;
    let got = merged(
        vec![
            partial_half("a_app_foo", "A.cs"),
            partial_half("b_app_foo", "B.cs"),
            json!({"id": "tree_walk", "label": "walk()", "source_file": "tree.py", "file_type": "code"}),
        ],
        vec![
            json!({"source": "tree_walk", "target": "tree_walk", "relation": "calls"}),
            json!({"source": "b_app_foo", "target": "b_app_foo", "relation": "references"}),
            json!({"source": "b_app_foo", "target": "a_app_foo", "relation": "references",
                "source_file": "B.cs", "source_location": "L3"}),
        ],
    );
    let t = |a: &str, b: &str, r: &str| (a.into(), b.into(), r.into(), String::new());
    assert!(
        got.contains(&t("tree_walk", "tree_walk", "calls")),
        "{got:?}"
    );
    assert!(
        got.contains(&t("a_app_foo", "a_app_foo", "references")),
        "{got:?}"
    );
    assert_eq!(got.len(), 2, "{got:?}");
}

#[test]
fn csharp_partial_merge_does_not_dedup_edges_it_never_rewrote() {
    use serde_json::json;
    let got = merged(
        vec![
            partial_half("a_app_foo", "A.cs"),
            partial_half("b_app_foo", "B.cs"),
        ],
        vec![
            json!({"source": "x", "target": "y", "relation": "calls", "confidence": "EXTRACTED",
                "source_file": "x.py", "source_location": "L1"}),
            json!({"source": "x", "target": "y", "relation": "calls", "confidence": "INFERRED",
                "source_file": "x.py", "source_location": "L1"}),
        ],
    );
    let t = |c: &str| ("x".into(), "y".into(), "calls".into(), c.to_string());
    assert_eq!(got, BTreeSet::from([t("EXTRACTED"), t("INFERRED")]));
}

// ---- Elixir unqualified calls are scoped to import/use (Graphify c96a743, 48de615) ----

const EX_COMPONENTS: &str =
    "defmodule MyAppWeb.CoreComponents do\n  def table(assigns), do: assigns\nend\n";
const EX_HELPERS: &str = "defmodule MyApp.Helpers do\n  def fmt(x), do: x\nend\n";

fn elixir_calls(files: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    edge_labels(&corpus(files), "calls")
}

#[test]
fn elixir_dsl_call_does_not_bind_to_an_unrelated_same_named_def() {
    let calls = elixir_calls(&[
        (
            "lib/my_app_web/components/core_components.ex",
            EX_COMPONENTS,
        ),
        (
            "priv/repo/migrations/20240101000000_create_users.exs",
            "defmodule MyApp.Repo.Migrations.CreateUsers do\n  use Ecto.Migration\n\n  def change do\n    create table(:users) do\n      add :name, :string\n    end\n  end\nend\n",
        ),
    ]);
    assert!(!calls.contains(&pair("change()", "table()")), "{calls:?}");
}

#[test]
fn elixir_call_without_import_or_use_does_not_bind() {
    let calls = elixir_calls(&[
        ("lib/my_app/helpers.ex", EX_HELPERS),
        (
            "lib/my_app/other.ex",
            "defmodule MyApp.Other do\n  alias MyApp.Helpers\n\n  def go(x) do\n    fmt(x)\n  end\nend\n",
        ),
    ]);
    assert!(!calls.contains(&pair("go()", "fmt()")), "{calls:?}");
}

#[test]
fn elixir_imported_and_used_module_calls_still_resolve() {
    let calls = elixir_calls(&[
        ("lib/my_app/helpers.ex", EX_HELPERS),
        (
            "lib/my_app/user.ex",
            "defmodule MyApp.User do\n  import MyApp.Helpers\n\n  def show(x) do\n    fmt(x)\n  end\nend\n",
        ),
        (
            "lib/my_app/schema.ex",
            "defmodule MyApp.Schema do\n  def timestamps_opts, do: []\nend\n",
        ),
        (
            "lib/my_app/post.ex",
            "defmodule MyApp.Post do\n  use MyApp.Schema\n\n  def opts do\n    timestamps_opts()\n  end\nend\n",
        ),
    ]);
    assert!(calls.contains(&pair("show()", "fmt()")), "{calls:?}");
    assert!(
        calls.contains(&pair("opts()", "timestamps_opts()")),
        "{calls:?}"
    );
}

#[test]
fn elixir_import_is_scoped_to_the_module_that_declares_it() {
    let calls = elixir_calls(&[
        ("lib/my_app/helpers.ex", EX_HELPERS),
        (
            "lib/my_app/pair.ex",
            "defmodule MyApp.Importer do\n  import MyApp.Helpers\n\n  def show(x) do\n    fmt(x)\n  end\n\n  defmodule Nested do\n    def inner(x) do\n      fmt(x)\n    end\n  end\nend\n\ndefmodule MyApp.Sibling do\n  def go(x) do\n    fmt(x)\n  end\nend\n",
        ),
    ]);
    assert!(calls.contains(&pair("show()", "fmt()")), "{calls:?}");
    assert!(calls.contains(&pair("inner()", "fmt()")), "{calls:?}");
    assert!(!calls.contains(&pair("go()", "fmt()")), "{calls:?}");
}

#[test]
fn elixir_imported_call_survives_incremental_rebuild() {
    use graphicle::watch::{RebuildOptions, rebuild};
    let fx = Fixture::new();
    fx.write("helpers.ex", EX_HELPERS);
    let body = |extra: &str| {
        format!(
            "defmodule MyApp.User do\n  import MyApp.Helpers\n\n  def show(x) do\n    fmt(x)\n  end\n{extra}end\n"
        )
    };
    let caller = fx.write("user.ex", &body(""));
    let calls = || {
        let text = std::fs::read_to_string(fx.root.join(".graphicle/graph.json")).unwrap();
        let data: Value = serde_json::from_str(&text).unwrap();
        let mut data = data;
        data["edges"] = data["links"].take();
        edge_labels(&data, "calls")
    };
    let options = |changed: Option<Vec<PathBuf>>| RebuildOptions {
        changed_paths: changed,
        no_cluster: true,
        acquire_lock: false,
        ..Default::default()
    };
    assert!(rebuild(&fx.root, &options(None)).unwrap().success);
    assert!(calls().contains(&pair("show()", "fmt()")), "{:?}", calls());
    std::fs::write(&caller, body("\n  def other, do: :ok\n")).unwrap();
    assert!(
        rebuild(&fx.root, &options(Some(vec![caller.clone()])))
            .unwrap()
            .success
    );
    assert!(calls().contains(&pair("show()", "fmt()")), "{:?}", calls());
}

// ── Graphify 0d2ade6 + 0110d96: Bash script invocations through interpreters ──

fn bash_invokes(fx: &Fixture, script: &std::path::Path) -> Vec<Value> {
    let result = graphicle::extractors::bash::extract_bash(script).unwrap();
    let _ = fx;
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == "invokes" && e["context"] == "script_invocation")
        .cloned()
        .collect()
}

fn path_id(path: &std::path::Path) -> String {
    graphicle::extractors::base::make_id([path.to_string_lossy().as_ref()])
}

#[test]
fn bash_emits_invokes_for_a_non_shell_interpreter() {
    for command in ["python3 stage.py --client \"$C\"", "python stage.py"] {
        let fx = Fixture::new();
        let stage = fx.write("stage.py", "print('stage')\n");
        let script = fx.write("runner.sh", &format!("#!/bin/bash\n{command}\n"));
        let invocation = bash_invokes(&fx, &script);
        let stage_text = stage.to_string_lossy().to_string();
        assert_eq!(
            invocation,
            vec![serde_json::json!({
                "source": path_id(&script) + "__entry",
                "target": path_id(&stage),
                "relation": "invokes",
                "confidence": "INFERRED",
                "source_file": script.to_string_lossy(),
                "source_location": "L2",
                "weight": 1.0,
                "context": "script_invocation",
                "target_file": stage_text,
            })],
            "{command}"
        );
    }
}

#[test]
fn bash_emits_invokes_for_a_bare_interpreter_variable() {
    let fx = Fixture::new();
    let stage = fx.write("stage_two.py", "print('stage two')\n");
    let script = fx.write(
        "runner.sh",
        "#!/bin/bash\nPYTHON=\"python3\"\n\"$PYTHON\" stage_two.py\n",
    );
    let invocation = bash_invokes(&fx, &script);
    assert_eq!(invocation.len(), 1);
    assert_eq!(invocation[0]["source"], path_id(&script) + "__entry");
    assert_eq!(invocation[0]["target"], path_id(&stage));
    assert_eq!(invocation[0]["source_location"], "L3");
    assert_eq!(invocation[0]["confidence"], "INFERRED");
}

#[test]
fn bash_invokes_targets_a_bash_entrypoint_when_the_target_is_sh() {
    let fx = Fixture::new();
    let helper = fx.write("helper.sh", "#!/bin/bash\necho helper\n");
    let script = fx.write("runner.sh", "#!/bin/bash\nnode ./helper.sh\n");
    let invocation = bash_invokes(&fx, &script);
    assert_eq!(invocation[0]["target"], path_id(&helper) + "__entry");
}

#[test]
fn bash_skips_invokes_for_non_interpreters_variable_paths_and_missing_scripts() {
    let fx = Fixture::new();
    let stage = fx.write("stage.py", "print('stage')\n");
    fx.write("run.sh", "#!/bin/bash\necho run\n");
    let plain = fx.write(
        "plain.sh",
        "#!/bin/bash\ncp stage.py /tmp/backup.py\nsome_custom_tool stage.py\n",
    );
    assert!(bash_invokes(&fx, &plain).is_empty());
    let missing = fx.write("missing.sh", "#!/bin/bash\npython3 does_not_exist.py\n");
    assert!(bash_invokes(&fx, &missing).is_empty());
    // `"$DIR/run.sh" stage.py` runs run.sh with stage.py as its argument.
    let caller = fx.write(
        "caller.sh",
        "#!/bin/bash\nDIR=\".\"\n\"$DIR/run.sh\" stage.py\n",
    );
    let result = graphicle::extractors::bash::extract_bash(&caller).unwrap();
    assert!(
        !result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| { e["relation"] == "invokes" && e["target"] == path_id(&stage) })
    );
}

#[test]
fn bash_invokes_from_inside_a_function_and_shadowed_interpreter() {
    let fx = Fixture::new();
    fx.write("stage.py", "print('stage')\n");
    let script = fx.write(
        "runner.sh",
        "#!/bin/bash\ndeploy() {\n  python3 stage.py\n}\npython() {\n  echo shadow\n}\npython stage.py\n",
    );
    let result = graphicle::extractors::bash::extract_bash(&script).unwrap();
    let deploy = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == "deploy()")
        .unwrap()["id"]
        .clone();
    let invocation = bash_invokes(&fx, &script);
    // The user-defined `python()` function shadows the interpreter.
    assert_eq!(invocation.len(), 1);
    assert_eq!(invocation[0]["source"], deploy);
}

// ── Graphify 1dcac12 + e402b87: Dart part-file nodes and configurable URIs ──

#[test]
fn dart_part_file_keeps_its_own_node() {
    let fx = Fixture::new();
    let lib = fx.write(
        "settlement.dart",
        "part 'derive.dart';\n\nclass Settlement {}\n",
    );
    let part = fx.write(
        "derive.dart",
        "part of 'settlement.dart';\n\nclass DeriveHelper {}\n\nvoid deriveAll() {}\n",
    );
    let result = graphicle::extractors::dart::extract_dart(&part).unwrap();
    let (lib_id, part_id) = (path_id(&lib), path_id(&part));
    let nodes = result["nodes"].as_array().unwrap();
    let edges = result["edges"].as_array().unwrap();
    let part_node = nodes.iter().find(|n| n["id"] == part_id.as_str()).unwrap();
    assert_eq!(part_node["label"], "derive.dart");
    assert_eq!(part_node["source_file"], part.to_string_lossy().as_ref());
    let includes = edges
        .iter()
        .filter(|e| e["relation"] == "includes")
        .collect::<Vec<_>>();
    assert_eq!(includes.len(), 1);
    assert_eq!(includes[0]["source"], lib_id.as_str());
    assert_eq!(includes[0]["target"], part_id.as_str());
    assert_eq!(includes[0]["source_location"], "L1");
    let targets = |source: &str, relation: &str| {
        edges
            .iter()
            .filter(|e| e["source"] == source && e["relation"] == relation)
            .map(|e| e["target"].as_str().unwrap().to_string())
            .collect::<BTreeSet<_>>()
    };
    let defined = targets(&lib_id, "defines");
    let label = |id: &str| {
        nodes.iter().find(|n| n["id"] == id).unwrap()["label"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        defined.iter().map(|t| label(t)).collect::<BTreeSet<_>>(),
        BTreeSet::from(["DeriveHelper".to_string(), "deriveAll".to_string()])
    );
    assert_eq!(targets(&part_id, "contains"), defined);
    let part_stem =
        graphicle::extractors::base::make_id([
            graphicle::extractors::base::file_stem(&part).as_str()
        ]);
    assert!(
        defined
            .iter()
            .all(|id| !id.starts_with(&format!("{part_stem}_")))
    );
}

#[test]
fn dart_conditional_uris_emit_every_branch() {
    let fx = Fixture::new();
    let path = fx.write(
        "facade.dart",
        "import 'conn_stub.dart'\n    if (dart.library.io) 'conn_io.dart'\n    if (dart.library.js_interop) 'conn_web.dart';\nimport 'flags.dart' if (app.flavor == 'prod') 'flags_prod.dart';\nexport 'service_stub.dart' if (dart.library.io) \"service_io.dart\";\n",
    );
    let result = graphicle::extractors::dart::extract_dart(&path).unwrap();
    let file_id = path_id(&path);
    let edges = result["edges"].as_array().unwrap();
    let targets = |relation: &str, context: Option<&str>| {
        edges
            .iter()
            .filter(|e| {
                e["source"] == file_id.as_str()
                    && e["relation"] == relation
                    && e.get("context").and_then(Value::as_str) == context
            })
            .map(|e| e["target"].as_str().unwrap().to_string())
            .collect::<BTreeSet<_>>()
    };
    let set = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    assert_eq!(
        targets("imports", None),
        set(&["conn_stub_dart", "flags_dart"])
    );
    assert_eq!(
        targets("imports", Some("conditional_uri")),
        set(&["conn_io_dart", "conn_web_dart", "flags_prod_dart"])
    );
    assert_eq!(targets("exports", None), set(&["service_stub_dart"]));
    assert_eq!(
        targets("exports", Some("conditional_uri")),
        set(&["service_io_dart"])
    );
    assert!(!labels(&result).contains("prod"));
    let location = |target: &str| {
        edges.iter().find(|e| e["target"] == target).unwrap()["source_location"].clone()
    };
    assert_eq!(location("conn_web_dart"), "L3");
    assert_eq!(location("service_io_dart"), "L5");
}

// Graphify 0.9.75 (f81c4e5, 97608ef, #3946): a code file that parses cleanly
// but yields only its own file node is named in a warning.
mod symbolless {
    use super::Fixture;
    use std::cell::RefCell;

    thread_local! {
        static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    /// Records warnings per thread, so concurrently running tests (and the
    /// single-threaded extraction below) never see each other's messages.
    struct Capture;
    impl log::Log for Capture {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Warn
        }
        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                WARNINGS.with(|w| w.borrow_mut().push(record.args().to_string()));
            }
        }
        fn flush(&self) {}
    }
    static CAPTURE: Capture = Capture;

    fn warnings(fx: &Fixture, files: &[(&str, &str)]) -> Vec<String> {
        let _ = log::set_logger(&CAPTURE);
        log::set_max_level(log::LevelFilter::Warn);
        WARNINGS.with(|w| w.borrow_mut().clear());
        let paths = files
            .iter()
            .map(|(rel, text)| fx.write(rel, text))
            .collect::<Vec<_>>();
        fx.extract(&paths);
        WARNINGS.with(|w| w.borrow().clone())
    }

    fn symbolless(warnings: &[String]) -> Vec<&String> {
        warnings
            .iter()
            .filter(|w| w.contains("yielded no symbols"))
            .collect()
    }

    #[test]
    fn names_a_clean_parse_data_file() {
        let fx = Fixture::new();
        let got = warnings(&fx, &[("data.js", "module.exports = [{a: 1}, {b: 2}];\n")]);
        assert_eq!(
            symbolless(&got),
            [
                "1 code file(s) yielded no symbols (0.00 MB) — a data file, or source this extractor models no symbols for: data.js"
            ]
        );
        assert!(
            !got.iter()
                .any(|w| w.contains("may be data rather than code"))
        );
    }

    #[test]
    fn counts_every_file_and_their_total_size() {
        let fx = Fixture::new();
        let got = warnings(
            &fx,
            &[
                ("a.js", "module.exports = [1, 2, 3];\n"),
                ("b.js", "export default [4, 5, 6];\n"),
                ("real.js", "function run() { return 1; }\n"),
                ("blank.py", "\n   \n\t\n"),
                ("pkg/__init__.py", ""),
            ],
        );
        assert_eq!(
            symbolless(&got),
            [
                "2 code file(s) yielded no symbols (0.00 MB) — a data file, or source this extractor models no symbols for: a.js, b.js"
            ]
        );
    }

    #[test]
    fn lists_five_files_then_a_count() {
        let fx = Fixture::new();
        let files = (0..7)
            .map(|i| (format!("d{i}.js"), "module.exports = [1];\n"))
            .collect::<Vec<_>>();
        let files = files
            .iter()
            .map(|(rel, text)| (rel.as_str(), *text))
            .collect::<Vec<_>>();
        let got = warnings(&fx, &files);
        assert_eq!(
            symbolless(&got),
            [
                "7 code file(s) yielded no symbols (0.00 MB) — a data file, or source this extractor models no symbols for: d0.js, d1.js, d2.js, d3.js, d4.js (+2 more)"
            ]
        );
    }

    #[test]
    fn stays_silent_for_real_code_blank_files_and_syntax_errors() {
        let fx = Fixture::new();
        let got = warnings(
            &fx,
            &[
                ("real.js", "function run() { return 1; }\n"),
                ("blank.py", "\n   \n\t\n"),
                ("pkg/__init__.py", ""),
                ("broken.lua", "local t = {\nfunction f() end\n"),
            ],
        );
        assert!(symbolless(&got).is_empty(), "{got:?}");
        assert!(
            got.iter()
                .any(|w| w.contains("broken.lua has syntax errors")),
            "the broken file must stay with the syntax-error warning: {got:?}"
        );
    }
}
