// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
//! Extractor fixes and regression tests ported from Graphify 0.9.77.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
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
            cache_root: Some(self.root.join("cache")),
            parallel: false,
            ..Default::default()
        };
        extract(paths, &options).unwrap()
    }
}

fn extract_one(name: &str, source: &str) -> Value {
    let fx = Fixture::new();
    let path = fx.write(name, source);
    fx.extract(&[path])
}

/// The single-file extractor (`extract_<language>(path)` upstream).
fn extract_single(name: &str, source: &str) -> Value {
    let fx = Fixture::new();
    let path = fx.write(name, source);
    graphicle::extract::extract_file(&path, None)
}

fn nodes(result: &Value) -> &Vec<Value> {
    result["nodes"].as_array().unwrap()
}

fn edges(result: &Value) -> &Vec<Value> {
    result["edges"].as_array().unwrap()
}

fn labels(result: &Value) -> BTreeSet<String> {
    nodes(result)
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}

/// `label.strip("()").lstrip(".")`, the symbol form upstream tests compare.
fn symbol(label: &str) -> String {
    label
        .trim_matches(['(', ')'])
        .trim_start_matches('.')
        .to_string()
}

/// `(source label, target label)` for every `relation` edge, optionally only
/// those with the given `context`.
fn edge_labels_in(
    result: &Value,
    relation: &str,
    context: Option<&str>,
    normalize: bool,
) -> BTreeSet<(String, String)> {
    let by_id: HashMap<&str, &str> = nodes(result)
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n["label"].as_str()?)))
        .collect();
    let label = |v: &Value| {
        let id = v.as_str().unwrap_or("");
        let label = by_id.get(id).copied().unwrap_or(id);
        if normalize {
            symbol(label)
        } else {
            label.to_string()
        }
    };
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .filter(|e| context.is_none_or(|c| e["context"] == c))
        .map(|e| (label(&e["source"]), label(&e["target"])))
        .collect()
}

fn edge_labels(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    edge_labels_in(result, relation, None, false)
}

/// Upstream `test_languages._edge_labels`: labels normalized to symbols.
fn symbol_edges(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    edge_labels_in(result, relation, None, true)
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

/// IDs of nodes from a file ending in `suffix` whose symbol is `name`.
fn ids_in(result: &Value, suffix: &str, name: &str) -> BTreeSet<String> {
    nodes(result)
        .iter()
        .filter(|n| {
            n["source_file"]
                .as_str()
                .is_some_and(|f| f.ends_with(suffix))
        })
        .filter(|n| n["label"].as_str().is_some_and(|l| symbol(l) == name))
        .filter_map(|n| n["id"].as_str().map(str::to_string))
        .collect()
}

fn edges_between(result: &Value, sources: &BTreeSet<String>, targets: &BTreeSet<String>) -> usize {
    edges(result)
        .iter()
        .filter(|e| {
            e["source"].as_str().is_some_and(|s| sources.contains(s))
                && e["target"].as_str().is_some_and(|t| targets.contains(t))
        })
        .count()
}

/// The first node with this exact label whose id contains `id_part`.
fn find(result: &Value, label: &str, id_part: &str) -> String {
    nodes(result)
        .iter()
        .find(|n| n["label"] == label && n["id"].as_str().is_some_and(|id| id.contains(id_part)))
        .and_then(|n| n["id"].as_str())
        .unwrap_or_else(|| panic!("no {label} node ({id_part}): {:?}", labels(result)))
        .to_string()
}

fn id_pairs(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
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

// 5c5ce7f (#3778): `res.format = fn` on an object the module exports through
// `module.exports = res` is a method, and its calls resolve.
#[test]
fn js_exported_object_member_assignment_and_calls() {
    let fx = Fixture::new();
    let utils = fx.write(
        "utils.js",
        "exports.normalizeType = function(val) { return val; };\n",
    );
    let response = fx.write(
        "response.js",
        "var normalizeType = require('./utils').normalizeType;\n\
         var res = Object.create(null);\n\
         module.exports = res;\n\
         res.format = function(obj) {\n\
         \x20   return normalizeType(obj);\n\
         };\n",
    );
    let result = fx.extract(&[utils, response]);
    let labels = labels(&result);
    assert!(labels.contains(".format()"), "{labels:?}");
    assert!(labels.contains("res"), "{labels:?}");
    let matching = id_pairs(&result, "calls")
        .into_iter()
        .filter(|(s, t)| s.ends_with("res_format") && t.ends_with("normalizetype"))
        .count();
    assert_eq!(matching, 1, "{:?}", id_pairs(&result, "calls"));
}

#[test]
fn js_esm_exported_object_member_assignment() {
    let result = extract_one(
        "app.js",
        "export const app = {};\n\
         app.use = function(middleware) {\n\
         \x20   return middleware;\n\
         };\n",
    );
    let labels = labels(&result);
    assert!(labels.contains(".use()"), "{labels:?}");
    assert!(labels.contains("app"), "{labels:?}");
}

#[test]
fn js_unexported_object_member_assignment_stays_unclaimed() {
    let result = extract_one(
        "local.js",
        "const local = {};\n\
         local.use = function(m) { return m; };\n",
    );
    assert!(!labels(&result).contains(".use()"), "{:?}", labels(&result));
}

fn php_extract(files: &[(&str, &str)]) -> Value {
    let fx = Fixture::new();
    let mut paths: Vec<PathBuf> = files.iter().map(|(n, s)| fx.write(n, s)).collect();
    paths.sort();
    fx.extract(&paths)
}

// d2b5e19: a method named like a PHP construct is still callable through
// `$this->name()`, and only on the caller's own class.
#[test]
fn php_construct_named_methods_keep_only_real_member_calls() {
    for (name, statement) in [
        ("die", "die($value);"),
        ("eval", "eval($value);"),
        ("array", "$result = array($value);"),
        ("exit", "exit($value);"),
        ("list", "list($value) = $values;"),
        ("unset", "unset($value);"),
    ] {
        let bag = format!(
            "<?php\nclass Bag {{\n \
             public function {name}($value) {{ return $value; }}\n \
             public function constructUse($value, $values) {{ {statement} }}\n \
             public function memberUse($value) {{ return $this->{name}($value); }}\n\
             }}\n"
        );
        let outside = format!("<?php\nfunction externalUse($value, $values) {{ {statement} }}\n");
        let result = php_extract(&[("Bag.php", &bag), ("outside.php", &outside)]);
        let target = ids_in(&result, "Bag.php", name);
        let local = ids_in(&result, "Bag.php", "constructUse");
        let external = ids_in(&result, "outside.php", "externalUse");
        let member = ids_in(&result, "Bag.php", "memberUse");
        assert!(
            !target.is_empty() && !local.is_empty() && !external.is_empty() && !member.is_empty(),
            "{name}: {:?}",
            labels(&result)
        );
        let uses = local.union(&external).cloned().collect();
        assert_eq!(edges_between(&result, &uses, &target), 0, "{name}");
        assert!(edges_between(&result, &member, &target) > 0, "{name}");
    }
}

#[test]
fn php_builtin_named_self_method_uses_its_own_class_and_php_case_rules() {
    let result = php_extract(&[(
        "Bag.php",
        "<?php\nclass Bag { public function list() {}\n \
         public function useList() { $this->LIST(); } }\n\
         class Other { public function list() {} }\n",
    )]);
    let bag = find(&result, "Bag", "");
    let other = find(&result, "Other", "");
    let targets = ids_in(&result, "Bag.php", "list");
    let methods_of = |owner: &str| -> BTreeSet<String> {
        id_pairs(&result, "method")
            .into_iter()
            .filter(|(s, t)| s == owner && targets.contains(t))
            .map(|(_, t)| t)
            .collect()
    };
    let (bag_targets, other_targets) = (methods_of(&bag), methods_of(&other));
    let caller = ids_in(&result, "Bag.php", "useList");
    assert!(!bag_targets.is_empty() && !other_targets.is_empty() && !caller.is_empty());
    assert!(edges_between(&result, &caller, &bag_targets) > 0);
    assert_eq!(edges_between(&result, &caller, &other_targets), 0);
}

#[test]
fn php_builtin_named_self_call_does_not_bind_to_another_class() {
    let result = php_extract(&[(
        "Bag.php",
        "<?php\nclass Bag { public function useList() { $this->list(); } }\n\
         class Other { public function list() {} }\n",
    )]);
    let caller = ids_in(&result, "Bag.php", "useList");
    let target = ids_in(&result, "Bag.php", "list");
    assert!(!caller.is_empty() && !target.is_empty());
    assert_eq!(edges_between(&result, &caller, &target), 0);
}

#[test]
fn php_builtin_named_self_call_does_not_bind_to_a_free_function() {
    let result = php_extract(&[(
        "Bag.php",
        "<?php\nfunction open() {}\n\
         class Bag { public function useOpen() { $this->open(); } }\n",
    )]);
    let caller = ids_in(&result, "Bag.php", "useOpen");
    let target = ids_in(&result, "Bag.php", "open");
    assert!(!caller.is_empty() && !target.is_empty());
    assert_eq!(edges_between(&result, &caller, &target), 0);
}

#[test]
fn php_builtin_named_this_call_without_a_class_owner_stays_unresolved() {
    let result = php_extract(&[(
        "Bag.php",
        "<?php\nfunction useList() { $this->list(); }\n\
         function makeListCallable() { return function() { $this->list(); }; }\n\
         class Other { public function list() {} }\n",
    )]);
    let target = ids_in(&result, "Bag.php", "list");
    assert!(!target.is_empty() && !ids_in(&result, "Bag.php", "useList").is_empty());
    assert!(!ids_in(&result, "Bag.php", "makeListCallable").is_empty());
    assert!(
        !edges(&result).iter().any(|e| e["relation"] == "calls"
            && e["target"].as_str().is_some_and(|t| target.contains(t))),
        "{:?}",
        id_pairs(&result, "calls")
    );
}

// 1f078a3 (#4041): a zero-byte file has no line 1.
#[test]
fn zero_byte_file_keeps_node_without_source_location() {
    let fx = Fixture::new();
    let empty = fx.write("empty.py", "");
    let witness = fx.write("witness.py", "VALUE = 1\n");
    let result = fx.extract(&[empty, witness]);
    let by_label: HashMap<&str, &Value> = nodes(&result)
        .iter()
        .filter_map(|n| Some((n["label"].as_str()?, n)))
        .collect();
    let empty = by_label.get("empty.py").expect("empty.py node kept");
    assert!(empty["source_location"].is_null(), "{empty}");
    assert_eq!(by_label["witness.py"]["source_location"], "L1");
}

const SCALA_CLASSES: &str = "trait Ordering\n\
trait Show\n\
trait Eq\n\
trait Async\n\
trait Comparable\n\
\n\
class Sorted[A: Ordering](xs: List[A])\n\
\n\
case class Pretty[T: Show: Eq](t: T)\n\
\n\
trait Effectful[F[_]: Async]\n\
\n\
enum Ranked[A: Ordering] { case Low }\n\
\n\
class Plain[T](t: T)\n\
\n\
class Upper[T <: Comparable[T]](t: T)\n\
\n\
class Holder {\n\
\x20 def member[K: Ordering](k: K): K = k\n\
}\n";

// ee29272 (#4080): class-level context bounds are `type_bound` references.
#[test]
fn scala_class_context_bounds_emit_references() {
    let r = extract_single("ClassContextBounds.scala", SCALA_CLASSES);
    let nid: HashMap<&str, &str> = nodes(&r)
        .iter()
        .filter_map(|n| Some((n["label"].as_str()?, n["id"].as_str()?)))
        .collect();
    let bound_targets = |label: &str| -> BTreeSet<&str> {
        edges(&r)
            .iter()
            .filter(|e| {
                e["relation"] == "references"
                    && e["source"] == nid[label]
                    && e["context"] == "type_bound"
            })
            .filter_map(|e| e["target"].as_str())
            .collect()
    };
    assert!(bound_targets("Sorted").contains(nid["Ordering"]));
    let pretty = bound_targets("Pretty");
    assert!(
        pretty.contains(nid["Show"]) && pretty.contains(nid["Eq"]),
        "{pretty:?}"
    );
    assert!(bound_targets("Effectful").contains(nid["Async"]));
    assert!(bound_targets("Ranked").contains(nid["Ordering"]));
    assert!(bound_targets("Plain").is_empty());
    assert!(bound_targets("Upper").is_empty());
    let sorted_refs: BTreeSet<(&str, &str)> = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "references" && e["source"] == nid["Sorted"])
        .filter_map(|e| Some((e["target"].as_str()?, e["context"].as_str()?)))
        .collect();
    assert!(
        sorted_refs.contains(&(nid["List"], "field")),
        "{sorted_refs:?}"
    );
    assert!(!sorted_refs.contains(&(nid["List"], "type_bound")));
    assert!(bound_targets(".member()").contains(nid["Ordering"]));
    assert!(bound_targets("Holder").is_empty());
    let located: Vec<&Value> = edges(&r)
        .iter()
        .filter(|e| {
            e["relation"] == "references"
                && e["source"] == nid["Sorted"]
                && e["context"] == "type_bound"
        })
        .collect();
    assert!(!located.is_empty());
    for e in located {
        assert_eq!(e["confidence"], "EXTRACTED", "{e}");
        assert_eq!(e["source_location"], "L7", "{e}");
    }
}

// e4c48bf: Objective-C C-style enums carry `case_of` members.
#[test]
fn objc_named_enum_members_are_case_of() {
    let r = extract_single(
        "Color.m",
        "enum Color { Red, Green, Blue };\n\
         @interface Foo : NSObject\n\
         @end\n\
         @implementation Foo\n\
         @end\n",
    );
    let case_of = edge_labels(&r, "case_of");
    for member in ["Red", "Green", "Blue"] {
        assert!(case_of.contains(&pair("Color", member)), "{case_of:?}");
    }
    assert!(labels(&r).contains("Foo"));
}

#[test]
fn objc_typedef_enum_takes_the_alias_name() {
    let r = extract_single(
        "Direction.m",
        "#import <Foundation/Foundation.h>\n\
         typedef enum { North, South } Direction;\n",
    );
    let case_of = edge_labels(&r, "case_of");
    assert!(case_of.contains(&pair("Direction", "North")), "{case_of:?}");
    assert!(case_of.contains(&pair("Direction", "South")), "{case_of:?}");
}

#[test]
fn objc_anonymous_enum_mints_nothing() {
    let r = extract_single(
        "Loose.m",
        "enum { Loose1, Loose2 };\n\
         @interface Keep : NSObject\n\
         @end\n",
    );
    let labels = labels(&r);
    assert!(
        !labels.contains("Loose1") && !labels.contains("Loose2"),
        "{labels:?}"
    );
    assert!(edge_labels(&r, "case_of").is_empty());
}

// e444d5a: a trigger `executes` the routine it runs when the file defines it.
#[test]
fn sql_trigger_executes_a_defined_routine() {
    let r = extract_single(
        "audit.sql",
        "CREATE TABLE users (id INT);\n\
         CREATE FUNCTION log_it() RETURNS TRIGGER AS 'SELECT 1';\n\
         CREATE TRIGGER audit_ins AFTER INSERT ON users\n\
         \x20 FOR EACH ROW EXECUTE FUNCTION log_it();\n",
    );
    let triggers = edge_labels(&r, "triggers");
    assert!(triggers.iter().any(|(_, t)| t == "users"), "{triggers:?}");
    let executes = edge_labels(&r, "executes");
    assert!(
        executes.iter().any(|(_, t)| t == "log_it()"),
        "{executes:?}"
    );
    let ids: BTreeSet<&str> = nodes(&r).iter().filter_map(|n| n["id"].as_str()).collect();
    for e in edges(&r) {
        assert!(ids.contains(e["source"].as_str().unwrap()), "dangling {e}");
        assert!(ids.contains(e["target"].as_str().unwrap()), "dangling {e}");
    }
}

#[test]
fn sql_trigger_on_an_undefined_routine_fails_closed() {
    let r = extract_single(
        "audit.sql",
        "CREATE TABLE users (id INT);\n\
         CREATE TRIGGER audit_ins AFTER INSERT ON users\n\
         \x20 FOR EACH ROW EXECUTE FUNCTION elsewhere();\n",
    );
    assert!(edge_labels(&r, "executes").is_empty());
    assert!(!labels(&r).contains("elsewhere()"));
}

// 282e2bb: PowerShell enum members are `case_of`, not `contains`.
#[test]
fn powershell_enum_members_emit_case_of_not_contains() {
    let r = extract_single(
        "status.ps1",
        "enum Status {\n    Active\n    Paused\n    Closed\n}\n",
    );
    let case_of = symbol_edges(&r, "case_of");
    let contains = symbol_edges(&r, "contains");
    for member in ["Active", "Paused", "Closed"] {
        assert!(case_of.contains(&pair("Status", member)), "{case_of:?}");
    }
    assert!(!contains.contains(&pair("Status", "Active")));
    assert!(
        contains.contains(&pair("status.ps1", "Status")),
        "{contains:?}"
    );
    assert!(!case_of.contains(&pair("status.ps1", "Status")));
}

// 7657686 (#3992): PowerShell method invocations bind only to methods.
#[test]
fn powershell_this_method_call_resolves() {
    let r = extract_single(
        "calc.ps1",
        "class Calc {\n\
         \x20   [int] Square([int]$x) { return $x * $x }\n\
         \x20   [int] Run() { return $this.Square(3) }\n\
         }\n",
    );
    assert!(r.get("error").is_none_or(Value::is_null));
    assert!(symbol_edges(&r, "calls").contains(&pair("Run", "Square")));
}

#[test]
fn powershell_static_method_call_resolves() {
    let r = extract_single(
        "calc.ps1",
        "class Calc {\n\
         \x20   static [Calc] Make() { return [Calc]::new() }\n\
         \x20   [Calc] Run() { return [Calc]::Make() }\n\
         }\n",
    );
    assert!(symbol_edges(&r, "calls").contains(&pair("Run", "Make")));
}

#[test]
fn powershell_member_call_does_not_bind_to_free_function() {
    let r = extract_single(
        "calc.ps1",
        "function Process { return 1 }\n\
         class Calc {\n\
         \x20   [int] Run() {\n\
         \x20       $obj = Get-Thing\n\
         \x20       return $obj.Process()\n\
         \x20   }\n\
         }\n",
    );
    assert!(!symbol_edges(&r, "calls").contains(&pair("Run", "Process")));
}

#[test]
fn powershell_command_call_still_resolves() {
    let r = extract_single(
        "mod.ps1",
        "function Helper { return 1 }\nfunction Run { Helper }\n",
    );
    assert!(symbol_edges(&r, "calls").contains(&pair("Run", "Helper")));
}

// 28675a1: anonymous Rust constants never replace the file node.
#[test]
fn rust_anonymous_constants_preserve_file_identity() {
    let fx = Fixture::new();
    let source = fx.write(
        "codec.rs",
        "pub const WIDTH: usize = 8;\n\
         const _: () = assert!(WIDTH == 8);\n\
         const _: () = ();\n\
         static READY: bool = true;\n\
         pub fn encode() {}\n",
    );
    let mut extracted = fx.extract(&[source]);
    let ids: BTreeSet<&str> = nodes(&extracted)
        .iter()
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert_eq!(ids.len(), nodes(&extracted).len());
    let mut messages = graphicle::build::Diagnostics::default();
    let graph =
        graphicle::build_from_json(&mut extracted, false, Some(&fx.root), &mut messages).unwrap();
    let data = graph.to_node_link("links");
    let labels = labels(&json_nodes(&data));
    for want in ["codec.rs", "WIDTH", "READY", "encode()"] {
        assert!(labels.contains(want), "{want}: {labels:?}");
    }
    assert!(!labels.contains("_"));
    for link in data["links"].as_array().unwrap() {
        assert_ne!(link["source"], link["target"], "{link}");
    }
}

fn json_nodes(data: &Value) -> Value {
    serde_json::json!({"nodes": data["nodes"]})
}

// 4f0eb67: Solidity enum values are `case_of`; struct fields stay `contains`.
#[test]
fn solidity_enum_values_emit_case_of_not_contains() {
    let r = extract_one(
        "Status.sol",
        "pragma solidity ^0.8.0;\n\
         contract Order {\n\
         \x20 enum Status { Pending, Shipped, Delivered }\n\
         \x20 struct Point { uint x; uint y; }\n\
         \x20 Status public status;\n\
         }\n",
    );
    let case_of = symbol_edges(&r, "case_of");
    let contains = symbol_edges(&r, "contains");
    for value in ["Pending", "Shipped", "Delivered"] {
        assert!(case_of.contains(&pair("Status", value)), "{case_of:?}");
    }
    assert!(!contains.contains(&pair("Status", "Pending")));
    assert!(contains.contains(&pair("Point", "x")), "{contains:?}");
    assert!(!case_of.contains(&pair("Point", "x")));
}

// 674338b (#4072): a trailing comment at `</script>` ends before the next one.
#[test]
fn astro_trailing_comment_does_not_hide_next_script() {
    let r = extract_single(
        "inline-comment.astro",
        "<script>const a = 1 // trailing comment</script>\
         <script>function visible() {}</script>\n",
    );
    assert!(r.get("parse_errors").is_none_or(Value::is_null), "{r}");
    let visible = nodes(&r)
        .iter()
        .find(|n| n["label"] == "visible()")
        .expect("visible() extracted");
    assert_eq!(visible["source_location"], "L1");
}

#[test]
fn astro_mask_preserves_bytes_and_newlines_after_comment() {
    let source = "---\nconst title = 'hello';\n---\n\
                  <script>const a = 1 // comment</script>\
                  <script>function visible() {}</script>\r\n";
    let masked = graphicle::extract_languages::astro_mask_non_script(source);
    let (masked, original) = (masked.as_bytes(), source.as_bytes());
    assert_eq!(masked.len(), original.len());
    let at = |hay: &[u8]| hay.windows(16).position(|w| w == b"function visible");
    assert_eq!(at(masked), at(original));
    let breaks = |bytes: &[u8]| -> Vec<(usize, u8)> {
        bytes
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c, b'\n' | b'\r'))
            .map(|(i, c)| (i, *c))
            .collect()
    };
    assert_eq!(breaks(masked), breaks(original));
}

// eb283ca (#4071): a valued Pascal enumeration names its cases only.
#[test]
fn pascal_valued_enum_literals_are_not_nodes() {
    let r = extract_single(
        "colors.pas",
        "unit Colors;\ninterface\ntype\n  TColor = (clRed = 1, clGreen = 2);\nimplementation\nend.\n",
    );
    assert!(r.get("error").is_none_or(Value::is_null));
    let labels = labels(&r);
    for want in ["TColor", "clRed", "clGreen"] {
        assert!(labels.contains(want), "{want}: {labels:?}");
    }
    assert!(!labels.contains("1") && !labels.contains("2"));
    let cases: BTreeSet<String> = edge_labels(&r, "case_of")
        .into_iter()
        .filter(|(s, _)| s == "TColor")
        .map(|(_, t)| t)
        .collect();
    assert_eq!(cases, ["clRed", "clGreen"].map(String::from).into());
}

// 9f6c0b9 (#4074): a tagged-union variant's struct payload is not recursed.
#[test]
fn zig_tagged_union_struct_payload_fields_are_not_minted() {
    let r = extract_single(
        "shapes.zig",
        "const Shape = union(enum) {\n\
         \x20   circle: f64,\n\
         \x20   rectangle: struct { w: f64, h: f64 },\n\
         \x20   point,\n\
         };\n",
    );
    let labels = labels(&r);
    assert!(labels.contains("rectangle"), "{labels:?}");
    assert!(!labels.contains("w") && !labels.contains("h"), "{labels:?}");
    assert!(
        !edge_labels(&r, "case_of")
            .iter()
            .any(|(s, _)| s == "rectangle")
    );
}

// 438a050: `defguardp` private guards are extracted.
#[test]
fn elixir_defmacro_and_defguard_are_extracted() {
    let r = extract_single(
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
         \x20 defguardp is_small(x) when is_integer(x) and x < 10\n\
         \n\
         \x20 def run(x) do\n\
         \x20   trace(priv_macro(x))\n\
         \x20 end\n\
         end\n",
    );
    assert!(r.get("error").is_none_or(Value::is_null));
    let labels: BTreeSet<String> = labels(&r)
        .iter()
        .map(|l| l.trim_end_matches(['(', ')']).to_string())
        .collect();
    for want in ["trace", "priv_macro", "is_even", "is_small"] {
        assert!(labels.contains(want), "{want} dropped: {labels:?}");
    }
}

// d07c91c (#4077): `this?.Save()` resolves like `this.Save()`.
#[test]
fn csharp_null_conditional_bare_this_receiver_resolves() {
    let r = extract_one(
        "S.cs",
        "public class Other { public bool Save() => false; }\n\
         public class Repo {\n\
         \x20   public bool Save() => true;\n\
         \x20   public bool? Commit() { return this?.Save(); }\n\
         }\n",
    );
    let calls = id_pairs(&r, "calls");
    let commit = find(&r, ".Commit()", "commit");
    let repo_save = find(&r, ".Save()", "repo");
    let other_save = find(&r, ".Save()", "other");
    assert!(calls.contains(&(commit.clone(), repo_save)), "{calls:?}");
    assert!(!calls.contains(&(commit, other_save)));
}

// 9a1d8c0: Kotlin annotation class-literal boundaries.
#[test]
fn kotlin_annotation_fully_qualified_class_literal() {
    let r = extract_one(
        "Order.kt",
        "@Ann(target = com.example.Customer::class)\nclass Order\nclass Customer\n",
    );
    let order = find(&r, "Order", "");
    let customer = find(&r, "Customer", "");
    let attributes: BTreeSet<(String, String)> = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "references" && e["context"] == "attribute")
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_string(),
                e["target"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(attributes.contains(&(order, customer)), "{attributes:?}");
}

#[test]
fn kotlin_annotation_builtin_class_literal_is_filtered() {
    let r = extract_one("Order.kt", "@Ann(target = String::class)\nclass Order\n");
    let order = find(&r, "Order", "");
    let annotation = find(&r, "Ann", "");
    assert!(edges(&r).iter().any(|e| e["source"] == order.as_str()
        && e["relation"] == "references"
        && e["context"] == "attribute"
        && e["target"] == annotation.as_str()));
    assert!(!labels(&r).contains("String"));
}

// 5ac6a3c: a Kotlin subclass keeps its Java base across the extension split.
#[test]
fn cross_file_inheritance_resolves_across_jvm_languages() {
    let fx = Fixture::new();
    let base = fx.write("Base.java", "package models;\npublic class Base {}\n");
    let child = fx.write("Child.kt", "import models.Base\nclass Child : Base()\n");
    let result = fx.extract(&[base, child]);
    let node = |label: &str, file: &str| -> String {
        nodes(&result)
            .iter()
            .find(|n| n["label"] == label && n["source_file"] == file)
            .and_then(|n| n["id"].as_str())
            .unwrap_or_else(|| panic!("{label} in {file}: {:?}", nodes(&result)))
            .to_string()
    };
    let (base, child) = (node("Base", "Base.java"), node("Child", "Child.kt"));
    assert!(id_pairs(&result, "inherits").contains(&(child, base)));
}

// b182659: a union's method declaration is a method, not a data field.
#[test]
fn cpp_union_member_function_is_a_method() {
    let r = extract_single("value.hpp", "union Value { int i; int get() const; };\n");
    assert!(r.get("parse_errors").is_none_or(Value::is_null));
    let by_id: HashMap<&str, &str> = nodes(&r)
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n["label"].as_str()?)))
        .collect();
    let relationships: BTreeSet<(&str, &str, &str)> = edges(&r)
        .iter()
        .filter_map(|e| {
            Some((
                *by_id.get(e["source"].as_str()?)?,
                *by_id.get(e["target"].as_str()?)?,
                e["relation"].as_str()?,
            ))
        })
        .collect();
    assert!(
        relationships.contains(&("Value", ".get()", "method")),
        "{relationships:?}"
    );
    assert!(relationships.contains(&("Value", "i", "defines")));
    assert!(!relationships.contains(&("Value", "get", "defines")));
    assert!(!by_id.values().any(|l| *l == "get"));
}

// af83bc0 (#4081): sourcing a shell file is an import, not an invocation.
#[test]
fn bash_source_forms_are_imports_not_invokes() {
    for form in ["source ./helpers.sh", ". ./helpers.sh"] {
        let fx = Fixture::new();
        let helpers = fx.write("helpers.sh", "# helper\n");
        let script = fx.write("deploy.sh", &format!("#!/bin/bash\n{form}\n"));
        let result = graphicle::extract::extract_file(&script, None);
        let imports: Vec<&Value> = edges(&result)
            .iter()
            .filter(|e| e["relation"] == "imports_from")
            .collect();
        assert!(!imports.is_empty(), "{form}: {result}");
        let helpers = helpers.canonicalize().unwrap();
        assert_eq!(
            imports[0]["target"],
            graphicle::ids::make_id([helpers.to_string_lossy().as_ref()]),
            "{form}"
        );
        assert!(
            !edges(&result)
                .iter()
                .any(|e| e["context"] == "script_invocation"),
            "{form}: {result}"
        );
    }
}
