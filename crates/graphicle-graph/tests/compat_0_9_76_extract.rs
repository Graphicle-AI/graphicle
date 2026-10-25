// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
//! Extractor fixes ported from Graphify 0.9.76.
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
    let by_id: HashMap<&str, &str> = result["nodes"]
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

// 9ca6298: a named C++ `union` is a class-like container with its members.
#[test]
fn cpp_union_specifier_is_extracted() {
    let result = extract_one(
        "value.hpp",
        "union Value { int i; float f; };\n\
         struct Point { int x; };\n\
         typedef union { int a; char b; } Anon;\n",
    );
    assert!(result.get("parse_errors").is_none());
    let labels = labels(&result);
    for label in ["Value", "i", "f"] {
        assert!(labels.contains(label), "{label} missing: {labels:?}");
    }
    assert!(edge_labels(&result, "contains").contains(&pair("value.hpp", "Value")));
    let defines = edge_labels(&result, "defines");
    assert!(defines.contains(&pair("Value", "i")));
    assert!(defines.contains(&pair("Value", "f")));
    // Graphify 0.9.95 (#4580): the typedef name `Anon` is a type alias node of
    // its own, owning none of the anonymous union's members.
    assert!(!labels.contains(""));
    assert!(edge_labels(&result, "contains").contains(&pair("value.hpp", "Anon")));
    assert!(!defines.iter().any(|(source, _)| source == "Anon"));
    assert!(!labels.contains("a") && !labels.contains("b"));
}

// 67a96aa: a VB.NET enum member is a `case_of` case, a field stays `contains`.
#[test]
fn vbnet_enum_members_emit_case_of_not_contains() {
    let result = extract_one(
        "Enums.vb",
        "Public Enum Color\n  Red\n  Green\n  Blue\nEnd Enum\n\
         Public Class Widget\n  Private count As Integer\nEnd Class\n",
    );
    let case_of = edge_labels(&result, "case_of");
    let contains = edge_labels(&result, "contains");
    for member in ["Red", "Green", "Blue"] {
        assert!(case_of.contains(&pair("Color", member)), "{case_of:?}");
    }
    assert!(!contains.contains(&pair("Color", "Red")));
    assert!(contains.contains(&pair("Widget", "count")));
    assert!(!case_of.contains(&pair("Widget", "count")));
}

// b462631: a Pascal enumerated type and its values are extracted.
#[test]
fn pascal_enum_type_and_values_are_extracted() {
    let result = extract_one(
        "colors.pas",
        "unit Colors;\ninterface\ntype\n  TColor = (clRed, clGreen, clBlue);\n\
         \x20 TNum = Integer;\nimplementation\nend.\n",
    );
    assert!(result.get("error").is_none());
    let labels = labels(&result);
    for label in ["TColor", "clRed", "clGreen", "clBlue"] {
        assert!(labels.contains(label), "{label} missing: {labels:?}");
    }
    let case_of = edge_labels(&result, "case_of");
    for value in ["clRed", "clGreen", "clBlue"] {
        assert!(case_of.contains(&pair("TColor", value)), "{case_of:?}");
    }
}

// 8566492 (#2255): a JSON Schema document is not a config manifest.
mod json_schema {
    use super::Fixture;
    use graphicle::extractors::json_config::extract_json;
    use serde_json::Value;

    const SCHEMA_WITH_DEFS: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://example.com/events-v1.json",
  "title": "Event catalogue",
  "$defs": {
    "dataClassification": {
      "description": "Handling class for the event payload.",
      "type": "string",
      "enum": ["Public", "Internal", "CUI"]
    },
    "event": {
      "description": "One domain event.",
      "type": "object",
      "required": ["id", "kind"],
      "properties": {
        "id": {"description": "Stable id.", "type": "string"},
        "kind": {"description": "Event kind.", "type": "string"},
        "seq": {"description": "Sequence.", "type": "integer", "minimum": 0},
        "tags": {"description": "Tags.", "type": "array",
                 "items": {"type": "string"}},
        "shape": {"description": "One shape.", "oneOf": [{"type": "string"}]}
      },
      "additionalProperties": false
    }
  },
  "$ref": "#/$defs/event"
}
"##;
    const SCHEMA_WITH_LEGACY_DEFS: &str = r##"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "definitions": {
    "point": {"description": "A point.", "type": "object"}
  },
  "$ref": "#/definitions/point"
}
"##;
    const BIOME: &str = r#"{
  "$schema": "https://biomejs.dev/schemas/1.7.0/schema.json",
  "files": {"include": ["src"]},
  "linter": {"enabled": true}
}
"#;
    const RENOVATE: &str = r#"{
  "$schema": "https://docs.renovatebot.com/renovate-schema.json",
  "extends": ["config:base"]
}
"#;
    const PROBE_EXTENDS: &str = r#"{"extends": "./base.json", "rules": {"eqeqeq": "error"}}"#;
    const PROBE_COMPILER: &str = r#"{"compilerOptions": {"strict": true, "target": "es2022"}}"#;

    fn run(name: &str, body: &str) -> Value {
        let fx = Fixture::new();
        extract_json(&fx.write(name, body))
    }

    fn labels(result: &Value) -> Vec<String> {
        result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["label"].as_str().map(str::to_string))
            .collect()
    }

    fn skipped(result: &Value) -> bool {
        result["skipped"] == "data json (not a config/manifest)"
    }

    #[test]
    fn schema_documents_are_skipped_not_walked() {
        for (name, body) in [
            ("events-v1.json", SCHEMA_WITH_DEFS),
            ("legacy.json", SCHEMA_WITH_LEGACY_DEFS),
            ("contracts/events-v1.json", SCHEMA_WITH_DEFS),
            ("ref.json", r#"{"$ref": "https://example.com/other.json"}"#),
        ] {
            let result = run(name, body);
            assert!(skipped(&result), "{name}: {result}");
            assert_eq!(result["nodes"], serde_json::json!([]), "{name}");
            assert_eq!(result["edges"], serde_json::json!([]), "{name}");
        }
    }

    #[test]
    fn configs_that_point_at_a_schema_are_still_extracted() {
        for (name, body, label) in [
            ("biome.json", BIOME, "linter"),
            ("renovate.json", RENOVATE, "extends"),
            (
                "package.json",
                r#"{"name": "x", "devDependencies": {"jest": "^29"}}"#,
                "devDependencies",
            ),
            ("custom.config.json", PROBE_EXTENDS, "extends"),
            ("weird.json", PROBE_COMPILER, "compilerOptions"),
            (
                "api.tsconfig.json",
                r#"{"compilerOptions": {"strict": true}}"#,
                "compilerOptions",
            ),
        ] {
            let result = run(name, body);
            assert!(!skipped(&result), "{name}: {result}");
            assert!(
                labels(&result).iter().any(|l| l == label),
                "{name}: {result}"
            );
        }
    }

    #[test]
    fn a_minimal_schema_without_defs_or_id_is_still_walked() {
        let result = run(
            "minimal.schema.json",
            r#"{"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object", "properties": {"a": {"type": "string"}}}"#,
        );
        assert!(!skipped(&result), "{result}");
        assert!(labels(&result).iter().any(|l| l == "properties"));
    }
}

// ac6ab9f (#2207): an unresolved base never rewires onto a same-named type in
// another language; a same-language match still does.
mod cross_language_rewire {
    use super::{Fixture, Value};
    use graphicle::extract_merge::rewire_unique_stubs;
    use serde_json::json;

    /// The `inherits` target of `child`, and whether it is still a stub.
    fn inherited(files: &[(&str, &str)], child: &str) -> (String, bool) {
        let fx = Fixture::new();
        let paths = files
            .iter()
            .map(|(rel, text)| fx.write(rel, text))
            .collect::<Vec<_>>();
        let result = fx.extract(&paths);
        let nodes = result["nodes"].as_array().unwrap();
        let by_id = |id: &Value| nodes.iter().find(|n| n["id"] == *id).unwrap();
        let matching = result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "inherits" && by_id(&e["source"])["label"] == child)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "{result}");
        let target = by_id(&matching[0]["target"]);
        (
            target["label"].as_str().unwrap().to_string(),
            target["source_file"].as_str().unwrap_or("").is_empty(),
        )
    }

    #[test]
    fn csharp_subclass_keeps_its_stub_when_the_only_class_is_python() {
        let got = inherited(
            &[
                ("interfaces.py", "class BookStore:\n    pass\n"),
                (
                    "services/BookStore.cs",
                    "class SqliteBookStore : BookStore { }\n",
                ),
            ],
            "SqliteBookStore",
        );
        assert_eq!(got, ("BookStore".to_string(), true));
    }

    #[test]
    fn python_base_never_lands_on_a_typescript_const_or_class() {
        for widget in [
            (
                "frontend/widget.test.tsx",
                "const Base = { id: 1, name: \"test\" };\n",
            ),
            (
                "frontend/widget.tsx",
                "export class Base { id: number = 1; }\n",
            ),
        ] {
            let got = inherited(
                &[
                    ("backend/models.py", "class User(Base):\n    pass\n"),
                    widget,
                ],
                "User",
            );
            assert_eq!(got, ("Base".to_string(), true), "{}", widget.0);
        }
    }

    #[test]
    fn csharp_base_never_lands_on_a_typescript_class() {
        let got = inherited(
            &[
                ("services/User.cs", "class User : Base { }\n"),
                (
                    "frontend/widget.tsx",
                    "export class Base { id: number = 1; }\n",
                ),
            ],
            "User",
        );
        assert_eq!(got, ("Base".to_string(), true));
    }

    #[test]
    fn a_unique_base_in_the_same_language_still_rewires() {
        let got = inherited(
            &[
                ("a.py", "class Widget:\n    pass\n"),
                ("b.py", "class User(Widget):\n    pass\n"),
            ],
            "User",
        );
        assert_eq!(got, ("Widget".to_string(), false));
    }

    fn rewire(mut nodes: Vec<Value>, mut edges: Vec<Value>) -> (Vec<Value>, Vec<Value>) {
        rewire_unique_stubs(&mut nodes, &mut edges);
        (nodes, edges)
    }

    fn has_node(nodes: &[Value], id: &str) -> bool {
        nodes.iter().any(|n| n["id"] == id)
    }

    #[test]
    fn the_casefolded_candidate_list_does_not_cross_languages() {
        let (nodes, edges) = rewire(
            vec![
                json!({"id": "base_base", "label": "base", "file_type": "code",
                       "source_file": "Base.php", "source_location": "L1"}),
                json!({"id": "Base", "label": "Base", "file_type": "code", "source_file": ""}),
            ],
            vec![
                json!({"source": "models_user", "target": "Base", "relation": "inherits",
                        "source_file": "models.py", "weight": 1.0}),
            ],
        );
        assert_eq!(edges[0]["target"], "Base");
        assert!(has_node(&nodes, "Base"));
    }

    #[test]
    fn a_constant_reference_does_not_cross_languages() {
        let (nodes, edges) = rewire(
            vec![
                json!({"id": "ui_command_command", "label": "Command", "file_type": "code",
                       "source_file": "resources/js/components/ui/command.tsx",
                       "source_location": "L1"}),
                json!({"id": "command", "label": "Command", "file_type": "code", "source_file": ""}),
            ],
            vec![json!({"source": "retry_handle", "target": "command",
                        "relation": "references_constant",
                        "source_file": "app/Console/Commands/RetryDeadLetters.php",
                        "weight": 1.0})],
        );
        assert_eq!(edges[0]["target"], "command");
        assert!(has_node(&nodes, "command"));
    }

    #[test]
    fn the_block_is_per_edge() {
        let (nodes, edges) = rewire(
            vec![
                json!({"id": "frontend_widget", "label": "Widget", "file_type": "code",
                       "source_file": "frontend/widget.tsx", "source_location": "L1"}),
                json!({"id": "widget", "label": "Widget", "file_type": "code", "source_file": ""}),
            ],
            vec![
                json!({"source": "php_user", "target": "widget", "relation": "inherits",
                       "source_file": "User.php", "weight": 1.0}),
                json!({"source": "tsx_user", "target": "widget", "relation": "inherits",
                       "source_file": "User.tsx", "weight": 1.0}),
            ],
        );
        assert_eq!(edges[0]["target"], "widget");
        assert_eq!(edges[1]["target"], "frontend_widget");
        assert!(has_node(&nodes, "widget"));
    }
}
// 8629f02 (#3830): PHP language constructs written like functions are not
// calls into same-named methods.
const PHP_BAG: &str = r#"<?php
namespace App;

class Bag
{
    private array $items = [];

    public function empty(): bool
    {
        return $this->items === [];
    }

    public function isset(string $k): bool
    {
        return array_key_exists($k, $this->items);
    }
}
"#;

fn php_ids(result: &Value, suffix: &str, name: &str) -> BTreeSet<String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["source_file"].as_str().unwrap_or("").ends_with(suffix))
        .filter(|n| {
            n["label"]
                .as_str()
                .unwrap_or("")
                .trim_matches(|c| c == '.' || c == '(' || c == ')')
                == name
        })
        .filter_map(|n| n["id"].as_str().map(str::to_string))
        .collect()
}

fn php_edges_between(
    result: &Value,
    sources: &BTreeSet<String>,
    targets: &BTreeSet<String>,
) -> Vec<Value> {
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            sources.contains(e["source"].as_str().unwrap_or(""))
                && targets.contains(e["target"].as_str().unwrap_or(""))
        })
        .cloned()
        .collect()
}

fn php_extract(files: &[(&str, &str)]) -> Value {
    let fx = Fixture::new();
    let mut paths: Vec<PathBuf> = files
        .iter()
        .map(|(name, text)| fx.write(name, text))
        .collect();
    paths.sort();
    fx.extract(&paths)
}

/// A construct in another file does not bind to the same-named method.
#[test]
fn php_construct_in_another_file_does_not_bind_to_user_method() {
    let result = php_extract(&[
        ("Bag.php", PHP_BAG),
        (
            "Mailer.php",
            r#"<?php
namespace App;

class Mailer
{
    public function send(array $to): void
    {
        if (empty($to) || !isset($to[0])) {
            return;
        }
    }
}
"#,
        ),
    ]);
    let mut targets = BTreeSet::new();
    targets.extend(php_ids(&result, "Bag.php", "empty"));
    targets.extend(php_ids(&result, "Bag.php", "isset"));
    let callers = php_ids(&result, "Mailer.php", "send");
    assert!(
        !targets.is_empty() && !callers.is_empty(),
        "the methods and the caller must still be extracted"
    );
    let edges = php_edges_between(&result, &callers, &targets);
    assert!(edges.is_empty(), "{edges:?}");
}

/// A construct does not bind to a sibling method in the same file.
#[test]
fn php_construct_does_not_bind_in_file() {
    let result = php_extract(&[(
        "Bag.php",
        r#"<?php
namespace App;

class Bag
{
    private array $items = [];

    public function empty(): bool
    {
        return $this->items === [];
    }

    public function isset(string $k): bool
    {
        return array_key_exists($k, $this->items);
    }

    public function add(string $k, $v): void
    {
        if (empty($k) || isset($this->items[$k])) {
            return;
        }
        $this->items[$k] = $v;
    }
}
"#,
    )]);
    let mut targets = BTreeSet::new();
    targets.extend(php_ids(&result, "Bag.php", "empty"));
    targets.extend(php_ids(&result, "Bag.php", "isset"));
    let callers = php_ids(&result, "Bag.php", "add");
    assert!(
        !targets.is_empty() && !callers.is_empty(),
        "the methods and the caller must still be extracted"
    );
    let edges = php_edges_between(&result, &callers, &targets);
    assert!(edges.is_empty(), "{edges:?}");
}

/// An upper-case construct is still a construct.
#[test]
fn php_construct_is_matched_case_insensitively() {
    let result = php_extract(&[
        ("Bag.php", PHP_BAG),
        (
            "helpers.php",
            r#"<?php
namespace App;

function blank(array $a): bool
{
    return EMPTY($a);
}
"#,
        ),
    ]);
    let mut targets = BTreeSet::new();
    targets.extend(php_ids(&result, "Bag.php", "empty"));
    let callers = php_ids(&result, "helpers.php", "blank");
    assert!(
        !targets.is_empty() && !callers.is_empty(),
        "the methods and the caller must still be extracted"
    );
    let edges = php_edges_between(&result, &callers, &targets);
    assert!(edges.is_empty(), "{edges:?}");
}

/// A member call into a construct-named method still resolves.
#[test]
fn php_member_call_to_construct_named_method_survives() {
    let result = php_extract(&[(
        "Bag.php",
        r#"<?php
namespace App;

class Bag
{
    public function empty(): bool
    {
        return true;
    }

    public function bothEmpty(Bag $other): bool
    {
        return $other->empty();
    }
}
"#,
    )]);
    let mut targets = BTreeSet::new();
    targets.extend(php_ids(&result, "Bag.php", "empty"));
    let callers = php_ids(&result, "Bag.php", "bothEmpty");
    assert!(
        !targets.is_empty() && !callers.is_empty(),
        "the methods and the caller must still be extracted"
    );
    let edges = php_edges_between(&result, &callers, &targets);
    assert!(!edges.is_empty(), "{edges:?}");
}

/// Calls inside a construct argument still resolve.
#[test]
fn php_calls_inside_construct_arguments_still_resolve() {
    let result = php_extract(&[(
        "Repo.php",
        r#"<?php
namespace App;

class Repo
{
    public function load(): array
    {
        return [];
    }

    public function missing(): bool
    {
        return empty($this->load());
    }
}
"#,
    )]);
    let mut targets = BTreeSet::new();
    targets.extend(php_ids(&result, "Repo.php", "load"));
    let callers = php_ids(&result, "Repo.php", "missing");
    assert!(
        !targets.is_empty() && !callers.is_empty(),
        "the methods and the caller must still be extracted"
    );
    let edges = php_edges_between(&result, &callers, &targets);
    assert!(!edges.is_empty(), "{edges:?}");
}

/// An ordinary function call still resolves across files.
#[test]
fn php_non_construct_function_call_still_resolves() {
    let result = php_extract(&[
        (
            "User.php",
            r#"<?php
namespace App;

class User
{
    public function label(string $n): string
    {
        return format_name($n);
    }
}
"#,
        ),
        (
            "helpers.php",
            r#"<?php
namespace App;

function format_name(string $n): string
{
    return trim($n);
}
"#,
        ),
    ]);
    let mut targets = BTreeSet::new();
    targets.extend(php_ids(&result, "helpers.php", "format_name"));
    let callers = php_ids(&result, "User.php", "label");
    assert!(
        !targets.is_empty() && !callers.is_empty(),
        "the methods and the caller must still be extracted"
    );
    let edges = php_edges_between(&result, &callers, &targets);
    assert!(!edges.is_empty(), "{edges:?}");
}

/// Filtering call targets must not delete the same-named method itself.
#[test]
fn php_user_method_node_survives_the_filter() {
    let result = php_extract(&[("Bag.php", PHP_BAG)]);
    assert!(!php_ids(&result, "Bag.php", "empty").is_empty());
    assert!(!php_ids(&result, "Bag.php", "isset").is_empty());
}

// f9b42e8 + aa448fb (#3928): the Svelte AST pass parses only `<script>` bodies.
fn svelte(rel: &str, text: &str) -> (Fixture, Value) {
    let fx = Fixture::new();
    let path = fx.write(rel, text);
    let result = graphicle::extract_languages::extract_language(&path, "svelte");
    (fx, result)
}

/// Label -> source_location for every node except the file node.
fn svelte_labels(result: &Value) -> HashMap<String, String> {
    let nodes = result["nodes"].as_array().unwrap();
    let file = nodes
        .first()
        .map(|n| n["id"].clone())
        .unwrap_or(Value::Null);
    nodes
        .iter()
        .filter(|n| n["id"] != file)
        .map(|n| {
            (
                n["label"].as_str().unwrap_or("").to_string(),
                n["source_location"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

fn svelte_targets(result: &Value, relation: &str) -> BTreeSet<String> {
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == relation)
        .filter_map(|e| e["target"].as_str().map(str::to_string))
        .collect()
}

fn file_id(path: &std::path::Path) -> String {
    graphicle::extractors::base::make_id([path.to_string_lossy().as_ref()])
}

#[test]
fn svelte_template_is_not_a_parse_error() {
    let (_fx, result) = svelte(
        "src/A.svelte",
        "<script>\n  import { foo } from \"./x\";\n  let n = 1;\n  function bump() { n++; }\n</script>\n<button onclick={bump}>{n}</button>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    // Masking keeps offsets, so locations still point at the original lines.
    assert_eq!(
        svelte_labels(&result).get("bump()").map(String::as_str),
        Some("L4")
    );
}

#[test]
fn svelte_parses_module_and_instance_scripts() {
    let (_fx, result) = svelte(
        "src/Both.svelte",
        "<script module>\n  export function shared() { return 1; }\n</script>\n<script>\n  function local() { return 2; }\n</script>\n<p>hi</p>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    let labels = svelte_labels(&result);
    assert_eq!(labels.get("shared()").map(String::as_str), Some("L2"));
    assert_eq!(labels.get("local()").map(String::as_str), Some("L5"));
}

#[test]
fn svelte_lang_ts_parses_type_syntax() {
    let (_fx, result) = svelte(
        "src/Typed.svelte",
        "<script lang=\"ts\">\n  interface Props { year: number }\n  let n = $state(1);\n  function bump(): void { n++; }\n</script>\n<button onclick={bump}>{n}</button>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    let labels = svelte_labels(&result);
    assert!(
        labels.contains_key("Props") && labels.contains_key("bump()"),
        "{labels:?}"
    );
}

#[test]
fn svelte_non_js_script_type_is_not_parsed_as_code() {
    let (_fx, result) = svelte(
        "src/Ld.svelte",
        "<script type=\"application/ld+json\">{ \"@type\": \"Event\", \"name\": \"x\" }</script>\n<script>\n  function go() { return 1; }\n</script>\n<p>hi</p>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    assert!(svelte_labels(&result).contains_key("go()"), "{result}");
}

/// Like the Astro masker, a quoted JS `type` is still JS (the reviewed
/// attribute parser; upstream's pattern blanks `type="module"`).
#[test]
fn svelte_quoted_module_type_is_still_parsed() {
    let (_fx, result) = svelte(
        "src/Mod.svelte",
        "<script type=\"module\">\n  function go() { return 1; }\n</script>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    assert_eq!(
        svelte_labels(&result).get("go()").map(String::as_str),
        Some("L2")
    );
}

#[test]
fn svelte_static_import_still_resolves() {
    let fx = Fixture::new();
    let component = fx.write(
        "src/Imports.svelte",
        "<script>\n  import { helper } from \"./helper\";\n</script>\n<p>hi</p>\n",
    );
    let helper = fx.write("src/helper.ts", "export function helper(){}\n");
    let result = graphicle::extract_languages::extract_language(&component, "svelte");
    assert!(
        svelte_targets(&result, "imports_from").contains(&file_id(&helper)),
        "{result}"
    );
}

#[test]
fn svelte_dynamic_import_in_template() {
    let fx = Fixture::new();
    let component = fx.write(
        "src/Lazy.svelte",
        "<script>\n  let show = true;\n</script>\n{#await import('./Other.svelte') then Mod}\n  <Mod.default />\n{/await}\n",
    );
    let other = fx.write("src/Other.svelte", "<p>o</p>\n");
    let result = graphicle::extract_languages::extract_language(&component, "svelte");
    assert!(
        svelte_targets(&result, "dynamic_import").contains(&file_id(&other)),
        "{result}"
    );
}

#[test]
fn svelte_scripts_on_one_line_do_not_merge() {
    let (_fx, result) = svelte(
        "src/Inline.svelte",
        "<script>const a = 1</script><script>function b() {}</script>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    assert!(svelte_labels(&result).contains_key("b()"), "{result}");
}

#[test]
fn svelte_line_comment_does_not_swallow_next_script() {
    let (_fx, result) = svelte(
        "src/Commented.svelte",
        "<script module>// shared</script><script>function b() {}</script>\n<script>function c() {}</script>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    let labels = svelte_labels(&result);
    assert_eq!(labels.get("b()").map(String::as_str), Some("L1"));
    assert_eq!(labels.get("c()").map(String::as_str), Some("L2"));
}

#[test]
fn svelte_comment_after_unterminated_statement() {
    let (_fx, result) = svelte(
        "src/Unterminated.svelte",
        "<script module>const a = 1 // shared</script><script>function b() {}</script>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    assert_eq!(
        svelte_labels(&result).get("b()").map(String::as_str),
        Some("L1")
    );
}

/// An unreadable file reports Python's `str(OSError)`, as every generic
/// extraction does.
#[test]
fn svelte_unreadable_file_reports_error() {
    let fx = Fixture::new();
    for (name, language) in [("Gone.svelte", "svelte"), ("gone.py", "python")] {
        let path = fx.root.join("missing").join(name);
        let result = graphicle::extract_languages::extract_language(&path, language);
        // The path as the engine spells it, quoted as Python's repr quotes it.
        let expected = format!(
            "[Errno 2] No such file or directory: {}",
            graphicle::python_compat::repr_string(&spelled(&path))
        );
        assert_eq!(
            result["error"].as_str(),
            Some(expected.as_str()),
            "{result}"
        );
        assert_eq!(result["nodes"], serde_json::json!([]));
        assert_eq!(result["edges"], serde_json::json!([]));
    }
}

#[test]
fn svelte_markup_only_component_does_not_crash() {
    let (_fx, result) = svelte("src/Plain.svelte", "<h1>no script here</h1>\n");
    assert!(result.get("parse_errors").is_none(), "{result}");
    assert!(
        svelte_targets(&result, "imports_from").is_empty(),
        "{result}"
    );
}

#[test]
fn svelte_mask_keeps_lines_and_byte_offsets() {
    let src = "<script module>// a</script><script lang=\"TS\">let b = 1</script>\r\n<p>x</p>\n";
    let (masked, lang) = graphicle::extract_languages::svelte_mask_non_script(src);
    assert_eq!(lang.as_deref(), Some("ts"));
    assert_eq!(masked.len(), src.len());
    assert_eq!(masked.matches('\n').count(), src.matches('\n').count());
    assert!(masked.contains("// a\u{2028};"), "{masked:?}");
    assert!(!masked.contains('<'), "{masked:?}");
}
