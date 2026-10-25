// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
//! Corpus assembly and cross-file resolution changes of Graphify
//! 0.9.89-0.9.91, ported from the upstream tests of those releases: Go member
//! calls through the receiver's declared type, the Go package type fold, Go
//! generic receivers, PHP instance member calls, C# `new X()` targets, Rust
//! `use` declarations landing on their declaration (#4439), `.js` imports of
//! `.tsx` sources, Python module calls from methods (#2447) and the
//! cross-repository parking of Go, PHP, Kotlin, ObjC and TS member calls.
use graphicle::api::{ExtractOptions, extract};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn isolate() {
    graphicle::rcfile::set_language_overrides(None);
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        isolate();
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
/// `(source label, target label, confidence)` of every `calls` edge.
fn calls(result: &Value) -> BTreeSet<(String, String, String)> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .map(|e| {
            (
                label_of(result, &e["source"]),
                label_of(result, &e["target"]),
                e["confidence"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}
fn call_to(result: &Value, from_suffix: &str, to: &str) -> Option<String> {
    calls(result)
        .into_iter()
        .find(|(s, t, _)| s.ends_with(from_suffix) && t == to)
        .map(|(_, _, c)| c)
}
fn parked(result: &Value, label: &str) -> Value {
    nodes(result)
        .iter()
        .find(|n| n["label"] == label)
        .map(|n| n["metadata"]["unresolved_calls"].clone())
        .unwrap_or(Value::Null)
}

// ── Go (0.9.89) ─────────────────────────────────────────────────────────────

const GREETER: &str = "package svc\n\ntype Greeter struct{}\n\nfunc (g *Greeter) Greet() {}\n";

fn go_run(app: &str) -> Value {
    Fixture::new().extract_files(&[("svc/greeter.go", GREETER), ("svc/app.go", app)])
}

#[test]
fn go_typed_parameter_types_the_receiver_inferred() {
    let r = go_run("package svc\n\nfunc Run(g *Greeter) { g.Greet() }\n");
    assert_eq!(
        call_to(&r, "un()", ".Greet()").as_deref(),
        Some("INFERRED"),
        "{:?}",
        calls(&r)
    );
}

#[test]
fn go_receiver_typing_sources() {
    for app in [
        "package svc\n\ntype App struct {\n\tgreeter *Greeter\n}\n\nfunc (a *App) Run() { a.greeter.Greet() }\n",
        "package svc\n\nfunc Run() {\n\tvar g Greeter\n\tg.Greet()\n}\n",
        "package svc\n\nfunc Run() {\n\tg := Greeter{}\n\tg.Greet()\n}\n",
        "package svc\n\nfunc Run() {\n\tg := &Greeter{}\n\tg.Greet()\n}\n",
    ] {
        let r = go_run(app);
        assert!(
            call_to(&r, "un()", ".Greet()").is_some(),
            "{app}: {:?}",
            calls(&r)
        );
    }
}

#[test]
fn go_untyped_or_foreign_receivers_resolve_to_nothing() {
    for app in [
        // A constructor's return type is not read.
        "package svc\n\nfunc NewG() *Greeter { return &Greeter{} }\n\nfunc Run() {\n\tg := NewG()\n\tg.Greet()\n}\n",
        // A chain not rooted at the method's own receiver.
        "package svc\n\ntype App struct {\n\tgreeter *Greeter\n}\n\nfunc Run(p *App) { p.greeter.Greet() }\n",
        // A package-qualified type.
        "package svc\n\nimport \"example.com/other\"\n\nfunc Run(g *other.Greeter) { g.Greet() }\n",
    ] {
        let r = go_run(app);
        assert!(
            call_to(&r, "un()", ".Greet()").is_none(),
            "{app}: {:?}",
            calls(&r)
        );
    }
}

#[test]
fn go_receiver_typed_to_an_absent_type_is_parked() {
    let r = Fixture::new().extract_files(&[(
        "svc/app.go",
        "package svc\n\nfunc Run(c *Client) { c.Send() }\n",
    )]);
    // A parked call records its call-site file (Graphify 0.9.94, #4668).
    assert_eq!(
        parked(&r, "Run()"),
        json!([{"callee": "Send", "receiver_type": "Client", "lang": "go", "line": "L3",
                "file": "svc/app.go"}])
    );
}

#[test]
fn go_package_type_folds_into_one_node() {
    let r = Fixture::new().extract_files(&[
        ("svc/server.go", "package svc\n\ntype Server struct{}\n"),
        (
            "svc/close.go",
            "package svc\n\nfunc (s *Server) Close() {}\n",
        ),
        ("svc/open.go", "package svc\n\nfunc (s *Server) Open() {}\n"),
    ]);
    let servers = nodes(&r)
        .iter()
        .filter(|n| n["label"] == "Server")
        .collect::<Vec<_>>();
    assert_eq!(servers.len(), 1, "{servers:?}");
    assert!(
        servers[0]["source_file"]
            .as_str()
            .unwrap()
            .ends_with("server.go")
    );
    let owner = &servers[0]["id"];
    let methods = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "method" && e["source"] == *owner)
        .count();
    assert_eq!(methods, 2);
}

#[test]
fn go_package_type_fold_stops_at_test_files_and_other_directories() {
    let r = Fixture::new().extract_files(&[
        ("a/svc/server.go", "package svc\n\ntype Server struct{}\n"),
        (
            "b/svc/close.go",
            "package svc\n\nfunc (s *Server) Close() {}\n",
        ),
        ("c/svc/server.go", "package svc\n\ntype Server struct{}\n"),
        (
            "c/svc/server_test.go",
            "package svc\n\nfunc (s *Server) Check() {}\n",
        ),
    ]);
    let servers = nodes(&r).iter().filter(|n| n["label"] == "Server").count();
    assert_eq!(servers, 4);
}

#[test]
fn go_generic_receiver_methods_attach_to_their_type() {
    let r = Fixture::new().extract_files(&[(
        "p/g.go",
        "package p\ntype Box[T any] struct { val T }\nfunc (b *Box[T]) Get() T { return b.val }\nfunc (b Box[T]) Set(v T) { b.val = v }\n",
    )]);
    let boxes = nodes(&r)
        .iter()
        .filter(|n| n["label"].as_str().unwrap_or("").starts_with("Box"))
        .collect::<Vec<_>>();
    assert_eq!(boxes.len(), 1, "{boxes:?}");
    assert_eq!(boxes[0]["_callable_class"], true);
    let methods = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "method" && e["source"] == boxes[0]["id"])
        .count();
    assert_eq!(methods, 2);
}

#[test]
fn go_declarations_carry_callable_markers() {
    let r = go_run("package svc\n\nfunc Run() {}\n");
    let by_label = |label: &str| {
        nodes(&r)
            .iter()
            .find(|n| n["label"] == label)
            .unwrap()
            .clone()
    };
    assert_eq!(by_label("Greeter")["_callable_class"], true);
    assert_eq!(by_label("Greeter")["_callable"], true);
    assert_eq!(by_label(".Greet()")["_callable"], true);
    assert!(by_label(".Greet()").get("_callable_class").is_none());
    assert_eq!(by_label("Run()")["_callable"], true);
}

// ── PHP (0.9.90) ────────────────────────────────────────────────────────────

const PHP_GREETER: &str = "<?php\nclass Greeter {\n    public function greet() {}\n}\n";

fn php_run(app: &str) -> Value {
    Fixture::new().extract_files(&[("Greeter.php", PHP_GREETER), ("App.php", app)])
}

#[test]
fn php_instance_receivers_typed_by_annotations_bind_inferred() {
    for app in [
        "<?php\nclass App {\n    private Greeter $g;\n    public function run() { $this->g->greet(); }\n}\n",
        "<?php\nclass App {\n    public function __construct(private Greeter $g) {}\n    public function run() { $this->g->greet(); }\n}\n",
        "<?php\nclass App {\n    public function run(Greeter $g) { $g->greet(); }\n}\n",
        "<?php\nclass App {\n    public function run() { $g = new Greeter(); $g->greet(); }\n}\n",
    ] {
        let r = php_run(app);
        assert_eq!(
            call_to(&r, "run()", ".greet()").as_deref(),
            Some("INFERRED"),
            "{app}: {:?}",
            calls(&r)
        );
    }
}

#[test]
fn php_untyped_or_union_receivers_stay_unresolved() {
    for app in [
        "<?php\nclass App {\n    public function run($g) { $g->greet(); }\n}\n",
        "<?php\nclass App {\n    public function run(Greeter|Other $g) { $g->greet(); }\n}\n",
    ] {
        let r = php_run(app);
        assert!(
            call_to(&r, "run()", ".greet()").is_none(),
            "{app}: {:?}",
            calls(&r)
        );
    }
}

#[test]
fn php_static_call_stays_extracted() {
    let r = Fixture::new().extract_files(&[
        (
            "Helper.php",
            "<?php\nclass Helper {\n    public static function format() {}\n}\n",
        ),
        (
            "App.php",
            "<?php\nclass App {\n    public function run() { Helper::format(); }\n}\n",
        ),
    ]);
    assert_eq!(
        call_to(&r, "run()", ".format()").as_deref(),
        Some("EXTRACTED")
    );
}

#[test]
fn php_receiver_typed_to_an_absent_class_is_parked() {
    let r = Fixture::new().extract_files(&[(
        "App.php",
        "<?php\nclass App {\n    public function run(Client $c) { $c->send(); }\n}\n",
    )]);
    // A parked call records its call-site file (Graphify 0.9.94, #4668).
    assert_eq!(
        parked(&r, ".run()"),
        json!([{"callee": "send", "receiver_type": "Client", "lang": "php", "line": "L3",
                "file": "App.php"}])
    );
}

// ── C# (0.9.90) ─────────────────────────────────────────────────────────────

#[test]
fn csharp_new_never_binds_to_a_same_named_method() {
    let r = Fixture::new().extract_files(&[
        (
            "Shapes.cs",
            "namespace App {\n    public static class Factory {\n        public static Bounds Bounds(int x) { return new Bounds(x); }\n    }\n}\n",
        ),
        (
            "Use.cs",
            "namespace App {\n    public class Use {\n        public void Run() { var b = new Bounds(1); }\n    }\n}\n",
        ),
    ]);
    let bad = calls(&r)
        .into_iter()
        .filter(|(_, t, _)| t == ".Bounds()")
        .collect::<Vec<_>>();
    assert!(bad.is_empty(), "{bad:?}");
}

#[test]
fn csharp_nameless_receiver_does_not_bind_the_callers_own_method() {
    let r = Fixture::new().extract_files(&[(
        "A.cs",
        "namespace App {\n    public class Runner {\n        public void Apply() { Make().Apply(); }\n        public Other Make() { return null; }\n    }\n}\n",
    )]);
    let self_loops = calls(&r)
        .into_iter()
        .filter(|(s, t, _)| s == t)
        .collect::<Vec<_>>();
    assert!(self_loops.is_empty(), "{self_loops:?}");
    assert!(
        !calls(&r)
            .iter()
            .any(|(s, t, _)| s == ".Apply()" && t == ".Apply()")
    );
}

// ── C# names that differ only by `_` or case (0.9.90, #4415) ────────────────

/// Ids of the sourced nodes labelled `label`.
fn ids_of(result: &Value, label: &str) -> Vec<String> {
    nodes(result)
        .iter()
        .filter(|n| n["label"] == label && n["source_file"].as_str().is_some_and(|s| !s.is_empty()))
        .map(|n| n["id"].as_str().unwrap_or("").to_string())
        .collect()
}
fn one(result: &Value, label: &str) -> String {
    let ids = ids_of(result, label);
    assert_eq!(ids.len(), 1, "{label}: {ids:?}");
    ids[0].clone()
}
/// `(source, target)` ids of every edge of `relation`.
fn pairs(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| {
            (
                e["source"].as_str().unwrap_or("").to_string(),
                e["target"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}
fn methods_owned_by(result: &Value, id: &str) -> BTreeSet<String> {
    pairs(result, "method")
        .into_iter()
        .filter(|(s, _)| s == id)
        .map(|(_, t)| label_of(result, &json!(t)))
        .collect()
}

#[test]
fn csharp_underscore_method_pair_keeps_both_methods() {
    let r = Fixture::new().extract_files(&[(
        "S.cs",
        "public class Widget {\n    public bool Action() { return true; }\n    protected bool _Action() { return Action(); }\n}\n",
    )]);
    let widget = one(&r, "Widget");
    let (action, private) = (one(&r, ".Action()"), one(&r, "._Action()"));
    assert_ne!(action, private);
    let method = pairs(&r, "method");
    assert!(
        method.contains(&(widget.clone(), action.clone())),
        "{method:?}"
    );
    assert!(method.contains(&(widget, private.clone())), "{method:?}");
    // The private method's call is its own, not a self-loop on the public one.
    let calls = pairs(&r, "calls");
    assert!(calls.contains(&(private, action.clone())), "{calls:?}");
    assert!(!calls.contains(&(action.clone(), action)));
}

#[test]
fn csharp_public_name_keeps_its_id_whatever_the_order() {
    let solo = Fixture::new().extract_files(&[(
        "S.cs",
        "public class Widget { public bool Action() { return true; } }\n",
    )]);
    let first = Fixture::new().extract_files(&[(
        "S.cs",
        "public class Widget {\n    public bool Action() { return true; }\n    protected bool _Action() { return true; }\n}\n",
    )]);
    let second = Fixture::new().extract_files(&[(
        "S.cs",
        "public class Widget {\n    protected bool _Action() { return true; }\n    public bool Action() { return true; }\n}\n",
    )]);
    assert_eq!(one(&first, ".Action()"), one(&solo, ".Action()"));
    for label in [".Action()", "._Action()"] {
        assert_eq!(one(&first, label), one(&second, label));
    }
}

#[test]
fn csharp_case_property_enum_and_preprocessor_pairs_stay_apart() {
    let r = Fixture::new().extract_files(&[(
        "S.cs",
        "public class Host {\n    public void Load() { }\n    public void load() { }\n    public int Size { get; set; }\n    private int _Size { get; set; }\n    public void Run() { }\n#if DEBUG\n    private void _Run() { }\n#endif\n    public void Save() { }\n    public void Save(int x) { }\n}\npublic enum Mode { Value, value }\n",
    )]);
    assert_ne!(one(&r, ".Load()"), one(&r, ".load()"));
    assert_ne!(one(&r, ".Run()"), one(&r, "._Run()"));
    let host = one(&r, "Host");
    let defines = pairs(&r, "defines");
    assert!(
        defines.contains(&(host.clone(), one(&r, "Size"))),
        "{defines:?}"
    );
    assert!(defines.contains(&(host, one(&r, "_Size"))), "{defines:?}");
    // Overloads are no underscore collision: since Graphify 0.9.95 (#4584)
    // they get a node each through the overload rule, the first keeping the
    // plain id.
    let saves = ids_of(&r, ".Save()");
    assert_eq!(saves.len(), 2, "{saves:?}");
    assert!(saves.iter().any(|id| id == "s_host_save"), "{saves:?}");
    let mode = one(&r, "Mode");
    let cases = pairs(&r, "case_of")
        .into_iter()
        .filter(|(s, _)| *s == mode)
        .collect::<Vec<_>>();
    assert_eq!(cases.len(), 2, "{cases:?}");
}

#[test]
fn csharp_underscore_type_pair_keeps_both_types_and_their_members() {
    let r = Fixture::new().extract_files(&[
        (
            "A.cs",
            "namespace Demo {\n    public class _Pair { public void Stop() { } }\n    public class Pair { public void Go() { } }\n}\n",
        ),
        (
            "B.cs",
            "namespace Demo {\n    public class Z : _Pair { public void M() { this.Stop(); } }\n    public class Y : Pair { public void N() { this.Go(); } }\n}\n",
        ),
    ]);
    let (pair, private) = (one(&r, "Pair"), one(&r, "_Pair"));
    assert_ne!(pair, private);
    let method = pairs(&r, "method");
    assert!(
        method.contains(&(pair.clone(), one(&r, ".Go()"))),
        "{method:?}"
    );
    assert!(
        method.contains(&(private.clone(), one(&r, ".Stop()"))),
        "{method:?}"
    );
    let inherits = pairs(&r, "inherits");
    assert!(inherits.contains(&(one(&r, "Z"), private)), "{inherits:?}");
    assert!(inherits.contains(&(one(&r, "Y"), pair)), "{inherits:?}");
    let calls = pairs(&r, "calls");
    assert!(
        calls.contains(&(one(&r, ".M()"), one(&r, ".Stop()"))),
        "{calls:?}"
    );
    assert!(
        calls.contains(&(one(&r, ".N()"), one(&r, ".Go()"))),
        "{calls:?}"
    );
}

#[test]
fn csharp_member_call_does_not_bind_to_an_underscore_sibling_on_a_nearer_base() {
    let r = Fixture::new().extract_files(&[(
        "S.cs",
        "public class BaseStore {\n    public virtual object Get(string n) { return null; }\n    public virtual bool Put(string n, object v) { return true; }\n}\npublic class CachedStore : BaseStore {\n    protected object _Get(string n) { return null; }\n    protected bool _Put(string n, object v) { return true; }\n}\npublic class Widget : CachedStore {\n    public void Consume(Widget other) {\n        this.Put(\"A\", 1);\n        other.Get(\"B\");\n    }\n}\n",
    )]);
    let calls = pairs(&r, "calls");
    let consume = one(&r, ".Consume()");
    assert!(
        calls.contains(&(consume.clone(), one(&r, ".Put()"))),
        "{calls:?}"
    );
    assert!(
        calls.contains(&(consume.clone(), one(&r, ".Get()"))),
        "{calls:?}"
    );
    assert!(!calls.contains(&(consume.clone(), one(&r, "._Put()"))));
    assert!(!calls.contains(&(consume, one(&r, "._Get()"))));
}

// ── C# `foreach` loop variables (0.9.90) ────────────────────────────────────

const CS_MODEL: &str = "namespace Demo\n{\n    public class Look { public void Animate() { } }\n    public class Agent { public Look look; public void Step() { } }\n    public class Decoy { public void Step() { } public void Animate() { } }\n}\n";

fn cs_crowd(methods: &str) -> Value {
    let crowd = format!(
        "using System.Collections.Generic;\nnamespace Demo\n{{\n    public class Crowd\n    {{\n        readonly Dictionary<string, Agent> agents = new Dictionary<string, Agent>();\n        readonly Dictionary<Agent, int> scores = new Dictionary<Agent, int>();\n        readonly List<Agent> list = new List<Agent>();\n        Agent[] array;\n        public IEnumerable<Agent> All {{ get; set; }}\n        {methods}\n    }}\n}}\n"
    );
    Fixture::new().extract_files(&[("Model.cs", CS_MODEL), ("Crowd.cs", &crowd)])
}
/// A calls edge from the Crowd method `caller` to `label` on the node whose
/// id contains `owner`.
fn crowd_calls(r: &Value, caller: &str, label: &str, owner: &str) -> bool {
    let find = |label: &str, part: &str| {
        nodes(r)
            .iter()
            .find(|n| n["label"] == label && n["id"].as_str().is_some_and(|i| i.contains(part)))
            .and_then(|n| n["id"].as_str())
            .unwrap_or("")
            .to_string()
    };
    pairs(r, "calls").contains(&(find(caller, "crowd"), find(label, owner)))
}

#[test]
fn csharp_foreach_over_dictionary_values_and_keys_types_the_loop_variable() {
    let r = cs_crowd(
        "public void Tick() { foreach (var a in agents.Values) a.look.Animate(); }\n        public void Keys() { foreach (var a in scores.Keys) a.Step(); }",
    );
    assert!(
        crowd_calls(&r, ".Tick()", ".Animate()", "look"),
        "{:?}",
        calls(&r)
    );
    assert!(!crowd_calls(&r, ".Tick()", ".Animate()", "decoy"));
    assert!(
        crowd_calls(&r, ".Keys()", ".Step()", "agent"),
        "{:?}",
        calls(&r)
    );
}

#[test]
fn csharp_foreach_over_sequences_and_explicit_types_takes_the_element_type() {
    let r = cs_crowd(
        "public void Listed() { foreach (var a in list) a.Step(); }\n        public void Arrayed() { foreach (var a in array) a.Step(); }\n        public void Property() { foreach (var a in All) a.Step(); }\n        public void ThisField() { foreach (var a in this.list) a.Step(); }\n        public void Typed(IEnumerable<object> xs) { foreach (Agent a in xs) a.Step(); }",
    );
    for caller in [
        ".Listed()",
        ".Arrayed()",
        ".Property()",
        ".ThisField()",
        ".Typed()",
    ] {
        assert!(
            crowd_calls(&r, caller, ".Step()", "agent"),
            "{caller}: {:?}",
            calls(&r)
        );
        assert!(!crowd_calls(&r, caller, ".Step()", "decoy"), "{caller}");
    }
}

#[test]
fn csharp_foreach_over_a_dictionary_or_a_shadowed_field_stays_untyped() {
    let r = cs_crowd(
        "public void Pairs() { foreach (var kv in agents) kv.Step(); }\n        public void Shadow() { var list = new List<Decoy>(); foreach (var a in list) a.Step(); }\n        public void After(Decoy a) { foreach (var b in list) { } a.Step(); }",
    );
    assert!(!crowd_calls(&r, ".Pairs()", ".Step()", "agent"));
    assert!(!crowd_calls(&r, ".Pairs()", ".Step()", "decoy"));
    assert!(!crowd_calls(&r, ".Shadow()", ".Step()", "agent"));
    // The loop variable does not leak past its loop.
    assert!(
        crowd_calls(&r, ".After()", ".Step()", "decoy"),
        "{:?}",
        calls(&r)
    );
    assert!(!crowd_calls(&r, ".After()", ".Step()", "agent"));
}

// ── C# partial halves merge after the id salting (0.9.91, #4411, #4428) ─────

#[test]
fn csharp_partial_half_with_a_path_colliding_id_keeps_its_members() {
    let r = Fixture::new().extract_files(&[
        ("Lib.csproj", "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>\n"),
        (
            "Cfg/Helpers.cs",
            "namespace App { public partial class Cfg { public void Alpha() {} } }\n",
        ),
        (
            "Cfg_Helpers.cs",
            "namespace App { public partial class Cfg { public void Beta() {} } }\n",
        ),
        (
            "generated/Cfg.cs",
            "namespace App { public partial class Cfg { public void Gamma() { Alpha(); Beta(); } } }\n",
        ),
    ]);
    let cfgs = nodes(&r)
        .iter()
        .filter(|n| n["label"] == "Cfg")
        .collect::<Vec<_>>();
    assert_eq!(cfgs.len(), 1, "{cfgs:?}");
    assert_eq!(
        methods_owned_by(&r, cfgs[0]["id"].as_str().unwrap()),
        BTreeSet::from([".Alpha()".into(), ".Beta()".into(), ".Gamma()".into()])
    );
}

#[test]
fn csharp_partial_half_next_to_an_underscore_file_keeps_its_members() {
    let r = Fixture::new().extract_files(&[
        (
            "Spec.cs",
            "namespace App { public partial class Spec { public int Size; } }\n",
        ),
        (
            "_Spec.cs",
            "namespace App { public class _Spec : Spec { public void Extra() { Move(); } } }\n",
        ),
        (
            "generated/Spec.cs",
            "namespace App { public partial class Spec { public void Move() {} public void Copy() {} } }\n",
        ),
    ]);
    let spec = one(&r, "Spec");
    assert_eq!(
        methods_owned_by(&r, &spec),
        BTreeSet::from([".Move()".into(), ".Copy()".into()])
    );
    let sub = one(&r, "_Spec");
    assert_ne!(sub, spec);
    assert!(pairs(&r, "inherits").contains(&(sub, spec)));
}

// ── Ext-tagged file ids (0.9.91, #4449) ─────────────────────────────────────

#[test]
fn a_symbol_spelled_like_a_sibling_files_old_id_keeps_its_own_node() {
    let r = Fixture::new().extract_files(&[
        (
            "esquema/x.mjs",
            "import postgres from \"postgres\";\nconst sql = postgres(process.env.U);\nasync function prueba(){ return sql`select 1` }\n",
        ),
        ("esquema/x.sql", "create table t(a int);\n"),
    ]);
    let found = |label: &str, file: &str| {
        let hits = nodes(&r)
            .iter()
            .filter(|n| {
                n["label"] == label
                    && n["source_file"]
                        .as_str()
                        .is_some_and(|s| s == file || s.ends_with(&format!("/{file}")))
            })
            .map(|n| n["id"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();
        assert_eq!(hits.len(), 1, "{label} in {file}: {hits:?}");
        hits[0].clone()
    };
    let ids = BTreeSet::from([
        found("x.mjs", "esquema/x.mjs"),
        found("sql", "esquema/x.mjs"),
        found("x.sql", "esquema/x.sql"),
    ]);
    assert_eq!(ids.len(), 3, "{ids:?}");
    let all = nodes(&r)
        .iter()
        .map(|n| n["id"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert_eq!(
        all.len(),
        all.iter().collect::<BTreeSet<_>>().len(),
        "{all:?}"
    );
}

// ── Rust `use` (0.9.91, #4439) ──────────────────────────────────────────────

fn rust_imports(result: &Value, file: &str) -> Vec<String> {
    let mut out = edges(result)
        .iter()
        .filter(|e| {
            e["context"] == "import" && e["source_file"].as_str().unwrap_or("").ends_with(file)
        })
        .map(|e| e["target"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    out.sort();
    out
}

#[test]
fn rust_use_lands_on_the_declaration_without_an_annotation() {
    let r = Fixture::new().extract_files(&[
        ("src/lib.rs", "pub mod billing;\npub mod model;\n"),
        ("src/model.rs", "pub struct Invoice(pub u64);\n"),
        (
            "src/billing.rs",
            "use crate::model::Invoice;\nuse std::fmt;\n\npub fn make() -> u64 {\n    Invoice(1).0\n}\n",
        ),
    ]);
    let invoice = nodes(&r).iter().find(|n| n["label"] == "Invoice").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut want = vec!["fmt".to_string(), invoice];
    want.sort();
    assert_eq!(rust_imports(&r, "billing.rs"), want);
    assert!(
        nodes(&r)
            .iter()
            .all(|n| !n["source_file"].as_str().unwrap_or("").is_empty())
    );
    assert!(edges(&r).iter().chain(nodes(&r)).all(|item| {
        item.as_object()
            .unwrap()
            .keys()
            .all(|k| !k.starts_with("_rust_use"))
    }));
}

#[test]
fn rust_use_of_a_twice_declared_name_stays_unresolved() {
    let r = Fixture::new().extract_files(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\npub mod user;\n"),
        ("src/a.rs", "pub struct Thing;\n"),
        ("src/b.rs", "pub struct Thing;\n"),
        ("src/user.rs", "use crate::a::Thing;\n"),
    ]);
    assert_eq!(rust_imports(&r, "user.rs"), vec!["thing".to_string()]);
}

#[test]
fn rust_use_from_another_crate_stays_on_its_own_name() {
    let r = Fixture::new().extract_files(&[
        ("src/lib.rs", "pub mod a;\npub mod user;\n"),
        (
            "src/a.rs",
            "pub struct File;\npub struct Command;\npub struct Serialize;\n",
        ),
        (
            "src/user.rs",
            "use std::fs::File;\nuse std::{process::Command, io};\nuse serde::Serialize;\n",
        ),
    ]);
    assert_eq!(
        rust_imports(&r, "user.rs"),
        vec!["command", "file", "io", "serialize"]
    );
}

#[test]
fn rust_use_name_is_written_only_for_crate_rooted_paths() {
    use graphicle::extractors::rust::use_name;
    assert_eq!(
        use_name("Invoice", "crate::model::Invoice").as_deref(),
        Some("Invoice")
    );
    assert_eq!(use_name("b as c", "self::a::b as c").as_deref(), Some("b"));
    assert_eq!(use_name("x", "::super::x").as_deref(), Some("x"));
    assert_eq!(use_name("File", "std::fs::File"), None);
}

// ── JS/TS (0.9.89) ──────────────────────────────────────────────────────────

#[test]
fn js_specifier_resolves_to_a_tsx_source() {
    let fx = Fixture::new();
    let tsx = fx.write("src/view.tsx", "export function View() { return 1; }\n");
    let resolved =
        graphicle::extractors::resolution_js::resolve_js_import_path(&fx.root.join("src/view.js"));
    assert_eq!(resolved, tsx);
    // A `.ts` source still wins over a `.tsx` one.
    let ts = fx.write("src/view.ts", "export const v = 1;\n");
    let resolved =
        graphicle::extractors::resolution_js::resolve_js_import_path(&fx.root.join("src/view.js"));
    assert_eq!(resolved, ts);
}

#[test]
fn ts_this_field_reads_the_callers_own_class() {
    let r = Fixture::new().extract_files(&[
        (
            "repo.ts",
            "export class Repo { save() {} }\nexport class Other { save() {} }\n",
        ),
        (
            "svc.ts",
            "import { Repo, Other } from './repo';\nexport class A {\n  repo: Repo;\n  run() { this.repo.save(); }\n}\nexport class B {\n  repo: Other;\n  go() { this.repo.save(); }\n}\n",
        ),
    ]);
    let edges = calls(&r)
        .into_iter()
        .filter(|(_, t, _)| t == ".save()")
        .collect::<Vec<_>>();
    assert_eq!(edges.len(), 2, "{edges:?}");
    let class_of_target = |from: &str| {
        let e = crate_edges(&r)
            .into_iter()
            .find(|e| label_of(&r, &e["source"]) == from && label_of(&r, &e["target"]) == ".save()")
            .unwrap();
        e["target"].as_str().unwrap().to_string()
    };
    assert!(class_of_target(".run()").contains("repo_repo"));
    assert!(class_of_target(".go()").contains("repo_other"));
}

fn crate_edges(r: &Value) -> Vec<Value> {
    edges(r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .cloned()
        .collect()
}

#[test]
fn ts_inherited_field_types_the_receiver() {
    let r = Fixture::new().extract_files(&[
        ("repo.ts", "export class Repo { save() {} }\n"),
        (
            "base.ts",
            "import { Repo } from './repo';\nexport class Base {\n  constructor(protected repo: Repo) {}\n}\n",
        ),
        (
            "child.ts",
            "import { Base } from './base';\nexport class Child extends Base {\n  run() { this.repo.save(); }\n}\n",
        ),
    ]);
    assert!(
        call_to(&r, ".run()", ".save()").is_some(),
        "{:?}",
        calls(&r)
    );
}

#[test]
fn ts_receiver_typed_to_an_absent_class_is_parked() {
    let r = Fixture::new()
        .extract_files(&[("svc.ts", "export function run(c: Client) { c.send(); }\n")]);
    // A parked call records its call-site file (Graphify 0.9.94, #4668).
    assert_eq!(
        parked(&r, "run()"),
        json!([{"callee": "send", "receiver_type": "Client", "lang": "typescript", "line": "L1",
                "file": "svc.ts"}])
    );
}

// ── Kotlin and ObjC parking (0.9.89) ────────────────────────────────────────

#[test]
fn kotlin_object_declared_nowhere_is_parked() {
    let r = Fixture::new().extract_files(&[(
        "App.kt",
        "package app\n\nfun run() {\n    Registry.lookup()\n    String.format(\"x\")\n}\n",
    )]);
    let entries = parked(&r, "run()");
    let entries = entries.as_array().cloned().unwrap_or_default();
    assert!(
        entries.iter().any(|e| e["callee"] == "lookup"
            && e["receiver_type"] == "Registry"
            && e["lang"] == "kotlin"),
        "{entries:?}"
    );
    assert!(!entries.iter().any(|e| e["receiver_type"] == "String"));
}

// ── Cross-repo linking (0.9.89) ─────────────────────────────────────────────

#[test]
fn cross_repo_member_keys_try_both_objc_sigils() {
    use graphicle::cross_repo_calls::{language_suffixes, member_keys};
    assert_eq!(member_keys("objc", "greet"), vec!["-greet", "+greet"]);
    assert_eq!(member_keys("objc", "+-greet"), vec!["-greet", "+greet"]);
    assert_eq!(member_keys("go", "Greet"), vec!["Greet"]);
    for (lang, suffix) in [
        ("go", ".go"),
        ("kotlin", ".kts"),
        ("objc", ".mm"),
        ("php", ".phtml"),
        ("typescript", ".vue"),
        ("typescript", ".mjs"),
    ] {
        assert!(language_suffixes(lang).contains(&suffix), "{lang} {suffix}");
    }
}

// ── Python (0.9.91, #2447) ──────────────────────────────────────────────────

#[test]
fn python_module_call_from_a_method_and_a_nested_function() {
    let r = Fixture::new().extract_files(&[
        ("pkg/__init__.py", ""),
        ("pkg/helpers.py", "def tool():\n    return 1\n"),
        (
            "pkg/app.py",
            "from pkg import helpers\n\nclass App:\n    def run(self):\n        return helpers.tool()\n\ndef outer():\n    def inner():\n        return helpers.tool()\n    return inner\n",
        ),
    ]);
    assert!(call_to(&r, ".run()", "tool()").is_some(), "{:?}", calls(&r));
    assert!(
        call_to(&r, "inner()", "tool()").is_some(),
        "{:?}",
        calls(&r)
    );
}
