// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
//! Language extractor changes and regression tests ported from Graphify
//! 0.9.78-0.9.88: source decoding, C++ export macros, .dmf ids, PowerShell,
//! R, Markdown links, Elixir, PHP casts, Rust, TypeScript `export type *`,
//! Julia, Fortran, Go generics, Objective-C includes, C# verbatim strings,
//! rescued npm imports and EJS templates.
use graphicle::api::{ExtractOptions, extract};
use graphicle::extract::extract_file;
use graphicle::extract_languages::{
    ejs_mask_non_script, normalize_cpp_export_macros, normalize_csharp_verbatim_interpolation,
    normalize_ts_export_type_star, php_blank_unparsed_casts,
};
use graphicle::extractors::base::{read_source_bytes, read_source_text};
use graphicle::ids::make_id;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
        self.write_bytes(rel, text.as_bytes())
    }
    fn write_bytes(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }
    fn extract_at(&self, root: &Path, paths: &[PathBuf]) -> Value {
        let options = ExtractOptions {
            root: Some(root.to_path_buf()),
            cache_root: Some(root.join("graphify-out")),
            parallel: false,
            ..Default::default()
        };
        extract(paths, &options).unwrap()
    }
    fn extract(&self, paths: &[PathBuf]) -> Value {
        self.extract_at(&self.root, paths)
    }
}

/// The single-file extractor for one source (`extract_<language>(path)`).
fn single(name: &str, source: &str) -> Value {
    let fx = Fixture::new();
    let path = fx.write(name, source);
    extract_file(&path, None)
}

fn nodes(result: &Value) -> &Vec<Value> {
    result["nodes"].as_array().unwrap()
}

fn edges(result: &Value) -> &Vec<Value> {
    result["edges"].as_array().unwrap()
}

fn raw_calls(result: &Value) -> Vec<Value> {
    result["raw_calls"].as_array().cloned().unwrap_or_default()
}

fn label_set(result: &Value) -> BTreeSet<String> {
    nodes(result)
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}

fn label_of(result: &Value) -> BTreeMap<String, String> {
    nodes(result)
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?.into(), n["label"].as_str()?.into())))
        .collect()
}

/// `label.strip("()").lstrip(".")`, the symbol form the upstream tests compare.
fn symbol(label: &str) -> String {
    label
        .trim_matches(['(', ')'])
        .trim_start_matches('.')
        .to_string()
}

/// Upstream `_edge_labels`: `(source, target)` symbols of `relation` edges.
fn edge_labels(
    result: &Value,
    relation: &str,
    context: Option<&str>,
) -> BTreeSet<(String, String)> {
    let by_id = label_of(result);
    let name = |v: &Value| {
        let id = v.as_str().unwrap_or("");
        symbol(by_id.get(id).map_or(id, String::as_str))
    };
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .filter(|e| context.is_none_or(|c| e["context"] == c))
        .map(|e| (name(&e["source"]), name(&e["target"])))
        .collect()
}

/// `(source label, target label)` of `relation` edges, labels as written.
fn raw_edge_labels(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    let by_id = label_of(result);
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .filter_map(|e| {
            Some((
                by_id.get(e["source"].as_str()?)?.clone(),
                by_id.get(e["target"].as_str()?)?.clone(),
            ))
        })
        .collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

fn utf16(text: &str, little: bool) -> Vec<u8> {
    let mut out = if little {
        vec![0xff, 0xfe]
    } else {
        vec![0xfe, 0xff]
    };
    for unit in text.encode_utf16() {
        out.extend(if little {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        });
    }
    out
}

fn utf32(text: &str, little: bool) -> Vec<u8> {
    let mut out = if little {
        vec![0xff, 0xfe, 0, 0]
    } else {
        vec![0, 0, 0xfe, 0xff]
    };
    for c in text.chars() {
        out.extend(if little {
            (c as u32).to_le_bytes()
        } else {
            (c as u32).to_be_bytes()
        });
    }
    out
}

fn cp1252(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).expect("Latin-1 text"))
        .collect()
}

// ── 0.9.78: source decoding (#4146) ──────────────────────────────────────────

#[test]
fn valid_utf8_is_passed_through_byte_for_byte() {
    let fx = Fixture::new();
    for (index, data) in [
        b"class Order {}\n".to_vec(),
        "class CaféOrder {}\n".as_bytes().to_vec(),
        [
            b"\xef\xbb\xbf".as_slice(),
            "class CaféOrder {}\n".as_bytes(),
        ]
        .concat(),
        "class CaféOrder {}\r\n".as_bytes().to_vec(),
        vec![],
    ]
    .into_iter()
    .enumerate()
    {
        // A fresh path each time: a concurrent run's cache keeps what it read.
        let path = fx.write_bytes(&format!("f{index}.cs"), &data);
        assert_eq!(&*read_source_bytes(&path, true).unwrap(), data.as_slice());
    }
}

#[test]
fn bom_marked_utf16_and_utf32_are_decoded_exactly() {
    let fx = Fixture::new();
    let text = "class CaféOrder { void Pay() { Charge(); } }\n";
    for (index, data) in [
        utf16(text, true),
        utf16(text, false),
        utf32(text, true),
        utf32(text, false),
    ]
    .into_iter()
    .enumerate()
    {
        let path = fx.write_bytes(&format!("f{index}.cs"), &data);
        assert_eq!(&*read_source_bytes(&path, false).unwrap(), text.as_bytes());
    }
    // FF FE 00 00 is both the UTF-32-LE BOM and UTF-16-LE BOM + U+0000.
    let data = utf16("\0ab", true);
    assert!(data.starts_with(b"\xff\xfe\x00\x00"));
    let path = fx.write_bytes("g.cs", &data);
    assert_eq!(&*read_source_bytes(&path, false).unwrap(), b"\0ab");
}

#[test]
fn cp1252_then_latin1_fallback_reads_names_whole() {
    let fx = Fixture::new();
    let path = fx.write_bytes("f.cs", &cp1252("class CaféOrder {}\n"));
    assert_eq!(
        &*read_source_bytes(&path, false).unwrap(),
        "class CaféOrder {}\n".as_bytes()
    );
    // 0x81 has no cp1252 mapping; latin-1 maps every byte.
    let path = fx.write_bytes("g.cs", b"class Caf\x81Order {}\n");
    assert_eq!(
        &*read_source_bytes(&path, false).unwrap(),
        "class Caf\u{81}Order {}\n".as_bytes()
    );
}

#[test]
fn read_source_text_matches_read_text_for_utf8_and_decodes_utf16() {
    let fx = Fixture::new();
    for (index, (data, want)) in [
        ("a = 1\r\nb = 'é'\r\n", "a = 1\nb = 'é'\n"),
        ("a = 1\rb = 2\r", "a = 1\nb = 2\n"),
        ("\u{feff}a = 'é'\n", "\u{feff}a = 'é'\n"),
        ("a = 1\r\n\rb = 2\n", "a = 1\n\nb = 2\n"),
    ]
    .into_iter()
    .enumerate()
    {
        let path = fx.write(&format!("f{index}.txt"), data);
        assert_eq!(read_source_text(&path, true).unwrap(), want);
    }
    let path = fx.write_bytes("g.txt", &utf16("a = 'é'\r\nb = 2\r\n", true));
    assert_eq!(read_source_text(&path, true).unwrap(), "a = 'é'\nb = 2\n");
}

#[test]
fn utf16_files_from_windows_tools_are_extracted() {
    for (name, text, want) in [
        (
            "schema.sql",
            "CREATE TABLE Orders (id INT);\nCREATE VIEW RecentOrders AS SELECT id FROM Orders;\n",
            vec!["Orders", "RecentOrders"],
        ),
        (
            "Deploy.ps1",
            "function Invoke-Deploy {\n    Get-Config\n}\nfunction Get-Config {\n    return 1\n}\n",
            vec!["Invoke-Deploy()", "Get-Config()"],
        ),
        (
            "Card.svelte",
            "<script>\n  export function formatPrice(p) { return p; }\n</script>\n<p>{formatPrice(1)}</p>\n",
            vec!["formatPrice()"],
        ),
    ] {
        let fx = Fixture::new();
        let path = fx.write_bytes(name, &utf16(text, true));
        let result = fx.extract(&[path]);
        let labels = label_set(&result);
        for label in want {
            assert!(labels.contains(label), "{name}: {labels:?}");
        }
    }
}

#[test]
fn utf16_spock_spec_and_catch2_test_case_keep_their_names() {
    let fx = Fixture::new();
    let spec = fx.write_bytes(
        "OrderSpec.groovy",
        &utf16(
            "import spock.lang.Specification\n\n\
             class OrderSpec extends Specification {\n\
             \x20   def \"pays the café order\"() {\n        expect: true\n    }\n}\n",
            true,
        ),
    );
    let test = fx.write_bytes(
        "order_test.cpp",
        &utf16(
            "#include \"catch.hpp\"\n\nTEST_CASE(\"pays the café order\") {\n    REQUIRE(true);\n}\n",
            true,
        ),
    );
    let spec = label_set(&fx.extract(&[spec]));
    assert!(spec.contains("OrderSpec"), "{spec:?}");
    assert!(
        spec.iter().any(|l| l.contains("pays the café order")),
        "{spec:?}"
    );
    let test = label_set(&fx.extract(&[test]));
    assert!(
        test.iter().any(|l| l.contains("pays the café order")),
        "{test:?}"
    );
}

#[test]
fn cp1252_identifier_is_not_cut_at_its_first_non_utf8_byte() {
    let fx = Fixture::new();
    let path = fx.write_bytes(
        "orders.go",
        &cp1252("package p\n\ntype CaféOrder struct{}\n\nfunc (o CaféOrder) Pay() {}\n"),
    );
    let labels = label_set(&extract_file(&path, None));
    assert!(labels.contains("CaféOrder"), "{labels:?}");
    assert!(labels.contains(".Pay()"), "{labels:?}");
}

#[test]
fn slices_read_documents_with_the_source_decoder() {
    let fx = Fixture::new();
    let text = "# Café notes\r\n\r\nSome text about the café.\r\n";
    let mut cache = graphicle::file_slice::SourceTextCache::default();
    for (name, data) in [
        ("a.md", text.as_bytes().to_vec()),
        ("b.md", utf16(text, true)),
        ("c.md", cp1252(text)),
    ] {
        let path = fx.write_bytes(name, &data);
        let got = cache.unit_source_text(&path).unwrap();
        assert_eq!(&*got, text.replace("\r\n", "\n"), "{name}");
    }
}

// ── 0.9.78: C++ export macros (#3661) ─────────────────────────────────────────

#[test]
fn cpp_export_macros_survive() {
    let result = single(
        "export.h",
        "class MODULE_API Widget : public BaseWidget {\n\
         public:\n\
         \x20   void DoThing() {}\n\
         };\n\
         class SOME_OTHER_MACRO Widget2 {};\n\
         class MODULE_API Widget3 final : public BaseWidget {};\n\
         class API FINAL Foo {};\n\
         struct EXPORT S {};\n",
    );
    let labels = label_set(&result);
    for want in ["Widget", ".DoThing()", "Widget2", "Widget3", "Foo", "S"] {
        assert!(labels.contains(want), "{want}: {labels:?}");
    }
    let inherits = raw_edge_labels(&result, "inherits");
    assert!(
        inherits.contains(&pair("Widget", "BaseWidget")),
        "{inherits:?}"
    );
    assert!(
        inherits.contains(&pair("Widget3", "BaseWidget")),
        "{inherits:?}"
    );
}

#[test]
fn cpp_export_macro_leaves_variables_and_final_names() {
    let result = single(
        "vars.cpp",
        "void F() {\n\
         \x20   for (class MODULE_API var: container) {}\n\
         \x20   class MODULE_API var2{1};\n\
         \x20   class MODULE_API v {1};\n\
         }\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    let labels = label_set(&single(
        "final.h",
        "class MY_WIDGET final : public Base {};\nclass MACRO Foo final : public Base {};\n",
    ));
    assert!(labels.contains("MY_WIDGET"), "{labels:?}");
    assert!(labels.contains("Foo"), "{labels:?}");
}

#[test]
fn cpp_export_macro_normalization_preserves_byte_offsets() {
    let src = b"#define API\nclass API \nWidget : public Base {\n  int x;\n};\n";
    let out = normalize_cpp_export_macros(src);
    assert_eq!(out.len(), src.len());
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(text.contains("class     \nWidget : public Base"), "{text}");
    assert_eq!(
        out.iter().filter(|b| **b == b'\n').count(),
        src.iter().filter(|b| **b == b'\n').count()
    );
}

// ── 0.9.78: .dmf element ids (#4153) ──────────────────────────────────────────

#[test]
fn dmf_element_ids_do_not_depend_on_the_checkout_path() {
    let skin = "window \"mapwindow\"\n\telem \"mapwindow\"\n\t\ttype = MAIN\n\telem \"map\"\n\t\ttype = MAP\n\
                window \"infowindow\"\n\telem \"info\"\n\t\ttype = CHILD\n";
    let fx = Fixture::new();
    let ids_for = |checkout: &str| {
        let root = fx.root.join(checkout);
        let path = fx.write(&format!("{checkout}/ui/skin.dmf"), skin);
        fx.extract_at(&root, &[path])["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["id"].as_str().map(str::to_string))
            .collect::<BTreeSet<_>>()
    };
    let first = ids_for("clone_one");
    let second = ids_for("elsewhere/clone_two");
    assert!(first.iter().any(|i| i.contains("elem")), "{first:?}");
    assert_eq!(first, second);
    assert!(
        !first
            .iter()
            .any(|i| i.contains("clone_one") || i.contains("elsewhere")),
        "{first:?}"
    );
}

// ── 0.9.78, 0.9.84, 0.9.88: PowerShell ────────────────────────────────────────

fn ps(source: &str) -> Value {
    let result = single("script.ps1", source);
    assert!(result.get("error").is_none(), "{result}");
    result
}

#[test]
fn powershell_member_calls_keep_receiver_class() {
    for reverse in [false, true] {
        let mut classes = ["Alpha", "Beta"]
            .iter()
            .map(|name| {
                format!(
                    "class {name} {{\n [int] Pick() {{ return 1 }}\n static [int] Make() {{ return 1 }}\n \
                     [int] Run() {{ $THIS.pIcK(); return 1 }}\n}}\n"
                )
            })
            .collect::<Vec<_>>();
        if reverse {
            classes.reverse();
        }
        let result = ps(&format!(
            "{}function Start {{ [aLpHa]::mAkE(); [Beta]::Make() }}\n",
            classes.concat()
        ));
        let by_id = label_of(&result);
        let methods: BTreeMap<(String, String), String> = edges(&result)
            .iter()
            .filter(|e| e["relation"] == "method")
            .map(|e| {
                let source = e["source"].as_str().unwrap();
                let target = e["target"].as_str().unwrap();
                (
                    (by_id[source].clone(), by_id[target].clone()),
                    target.to_string(),
                )
            })
            .collect();
        let start = nodes(&result)
            .iter()
            .find(|n| n["label"] == "Start()")
            .and_then(|n| n["id"].as_str())
            .unwrap()
            .to_string();
        let calls: BTreeSet<(String, String)> = edges(&result)
            .iter()
            .filter(|e| e["relation"] == "calls")
            .map(|e| {
                (
                    e["source"].as_str().unwrap().to_string(),
                    e["target"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let mut want = BTreeSet::new();
        for name in ["Alpha", "Beta"] {
            let m = |label: &str| methods[&(name.to_string(), label.to_string())].clone();
            want.insert((m(".Run()"), m(".Pick()")));
            want.insert((start.clone(), m(".Make()")));
        }
        assert_eq!(calls, want, "reverse={reverse}");
    }
}

#[test]
fn powershell_uncertain_receivers_remain_raw() {
    for (call, callee) in [
        ("$unknown.Pick()", "Pick"),
        ("$typed.Pick()", "Pick"),
        ("$local.Pick()", "Pick"),
        ("$this.child.Pick()", "Pick"),
        ("([Alpha]$this).Pick()", "Pick"),
        ("$this.$dynamic()", "$dynamic"),
        ("[External]::Pick()", "Pick"),
        ("[Alpha+Nested]::Pick()", "Pick"),
        ("[Alpha, MyAssembly]::Pick()", "Pick"),
        ("[Beta]::Pick()", "Pick"),
    ] {
        let result = ps(&format!(
            "function Pick {{ return 3 }}\nclass Alpha {{\n [int] Pick() {{ return 1 }}\n \
             [int] Run([Alpha]$typed) {{\n  $local = [Alpha]::new()\n  {call}\n  return 1\n }}\n}}\n\
             class Beta {{ [int] Other() {{ return 1 }} }}\n"
        ));
        assert!(
            !edges(&result).iter().any(|e| e["relation"] == "calls"),
            "{call}: {result}"
        );
        let picked: Vec<_> = raw_calls(&result)
            .into_iter()
            .filter(|c| c["callee"] == callee)
            .collect();
        assert_eq!(picked.len(), 1, "{call}: {picked:?}");
        assert_eq!(picked[0]["is_member_call"], true, "{call}");
    }
}

#[test]
fn powershell_complete_static_type_literal_resolves() {
    for receiver in ["[Alpha]", "[aLpHa]", "[ Alpha ]"] {
        let result = ps(&format!(
            "class Alpha {{ static [int] Pick() {{ return 1 }} }}\nfunction Start {{ {receiver}::Pick() }}\n"
        ));
        assert_eq!(
            raw_edge_labels(&result, "calls"),
            BTreeSet::from([pair("Start()", ".Pick()")]),
            "{receiver}"
        );
        assert!(!raw_calls(&result).iter().any(|c| c["callee"] == "Pick"));
    }
}

#[test]
fn powershell_duplicate_class_names_do_not_guess() {
    for second in ["Alpha", "alpha"] {
        let result = ps(&format!(
            "class Alpha {{ [int] Pick() {{ return 1 }} [int] Run() {{ return $this.Pick() }} }}\n\
             class {second} {{ [int] Pick() {{ return 2 }} }}\nfunction Start {{ [Alpha]::Pick() }}\n"
        ));
        assert!(!edges(&result).iter().any(|e| e["relation"] == "calls"));
        let picks = raw_calls(&result)
            .into_iter()
            .filter(|c| c["callee"] == "Pick" && c["is_member_call"] == true)
            .count();
        assert_eq!(picks, 2, "{second}");
    }
}

#[test]
fn powershell_property_reads_and_nested_calls() {
    let result = ps("class Alpha {\n static [int] Make() { return 1 }\n \
                     [int] Run() { $this.Make; [Alpha]::Make; $unknown.Pick([Alpha]::Make()); return 1 }\n}\n");
    assert_eq!(
        edge_labels(&result, "calls", None),
        BTreeSet::from([pair("Run", "Make")])
    );
    let callees: Vec<_> = raw_calls(&result)
        .iter()
        .map(|c| c["callee"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(callees, ["Pick"]);
}

#[test]
fn powershell_dot_source_quoted_or_variable_path_emits_edge() {
    for line in [
        ". \"$PSScriptRoot\\lib.ps1\"",
        ". $PSScriptRoot\\lib.ps1",
        ". '.\\lib.ps1'",
    ] {
        let result = ps(&format!("{line}\n"));
        assert!(
            edges(&result)
                .iter()
                .any(|e| e["relation"] == "imports_from" && e["target"] == "lib"),
            "{line}: {result}"
        );
    }
}

#[test]
fn powershell_script_scope_calls_come_from_the_file() {
    let result = ps("function Helper { return 1 }\nHelper\nGet-External\n");
    assert!(
        edges(&result)
            .iter()
            .any(|e| e["relation"] == "calls" && e["target"].as_str().unwrap().ends_with("helper")),
        "{result}"
    );
    assert!(
        raw_calls(&result)
            .iter()
            .any(|c| c["callee"] == "Get-External")
    );
    // Calls inside a function body stay the function's.
    let result = ps("function Helper { return 1 }\nfunction Run { Helper }\n");
    let file = nodes(&result)[0]["id"].clone();
    assert!(
        !edges(&result)
            .iter()
            .any(|e| e["relation"] == "calls" && e["source"] == file)
    );
    assert!(!raw_calls(&result).iter().any(|c| c["caller_nid"] == file));
}

#[test]
fn powershell_functions_in_file_scope_begin_block_are_extracted() {
    let result = ps(
        "[CmdletBinding()]\nparam()\nbegin {\n    function Get-Foo { Get-Bar }\n    \
                     function Get-Bar { 'bar' }\n}\nprocess {\n    Get-Foo\n}\n",
    );
    let labels = label_set(&result);
    assert!(
        labels.contains("Get-Foo()") && labels.contains("Get-Bar()"),
        "{labels:?}"
    );
    assert!(edge_labels(&result, "calls", None).contains(&pair("Get-Foo", "Get-Bar")));
}

// ── 0.9.78, 0.9.84: R operator bindings (#4171, #4253) ────────────────────────

#[test]
fn r_symbol_and_backtick_bindings_have_distinct_ids() {
    let result = single(
        "ops.R",
        "`%||%` <- function(a, b) a\n`%>%` <- function(a, b) b\n`%+%` <- 42\n",
    );
    let by_label: BTreeMap<String, String> = nodes(&result)
        .iter()
        .map(|n| {
            (
                n["label"].as_str().unwrap().into(),
                n["id"].as_str().unwrap().into(),
            )
        })
        .collect();
    for label in ["`%||%`()", "`%>%`()", "`%+%`"] {
        assert!(by_label.contains_key(label), "{by_label:?}");
        let contains = edges(&result).iter().any(|e| {
            e["relation"] == "contains"
                && e["source"] == by_label["ops.R"]
                && e["target"] == by_label[label]
        });
        assert!(contains, "{label}");
    }
    let ids: BTreeSet<_> = by_label.values().collect();
    assert_eq!(ids.len(), by_label.len());
    // A partially symbolic name no longer meets the plain one (#4253).
    let result = single(
        "ops.R",
        "foo <- function() 1\n`%foo%` <- function(a, b) is.null(a)\n",
    );
    let labels = label_set(&result);
    assert!(
        labels.contains("foo()") && labels.contains("`%foo%`()"),
        "{labels:?}"
    );
}

#[test]
fn r_operator_ids_are_stable_and_scopes_stay_apart() {
    let operator = "`%||%` <- function(a, b) a\n";
    let ordinary = "operator_60257c7c2560 <- function(a, b) b\n";
    let mut ids_by_order = vec![];
    for declarations in [
        format!("{operator}{ordinary}"),
        format!("{ordinary}{operator}"),
    ] {
        let fx = Fixture::new();
        let path = fx.write(
            "ops.R",
            &format!(
                "{declarations}ordinary_call <- function() operator_60257c7c2560(1, 2)\n\
                 operator_call <- function() `%||%`(1, 2)\n"
            ),
        );
        let result = extract_file(&path, None);
        // Ids relative to the file's own (absolute) stem.
        let stem = make_id([graphicle::extractors::base::file_stem(&path).as_str()]);
        let calls = raw_edge_labels(&result, "calls");
        assert!(calls.contains(&pair("ordinary_call()", "operator_60257c7c2560()")));
        assert!(calls.contains(&pair("operator_call()", "`%||%`()")));
        let ids: BTreeMap<String, String> = nodes(&result)
            .iter()
            .filter(|n| n["label"] != "ops.R")
            .map(|n| {
                let id = n["id"].as_str().unwrap();
                (
                    n["label"].as_str().unwrap().into(),
                    id.strip_prefix(stem.as_str()).unwrap_or(id).into(),
                )
            })
            .collect();
        ids_by_order.push(ids);
    }
    assert_eq!(ids_by_order[0], ids_by_order[1]);
    let result = single(
        "ops.R",
        "`%||%` <- function() { inner <- function() 1; inner() }\n\
         operator_60257c7c2560 <- function() { inner <- function() 2; inner() }\n",
    );
    assert_eq!(
        nodes(&result)
            .iter()
            .filter(|n| n["label"] == "inner()")
            .count(),
        2
    );
    for label in ["`%||%`()", "operator_60257c7c2560()"] {
        let owner = nodes(&result).iter().find(|n| n["label"] == label).unwrap()["id"].clone();
        let targets = |relation: &str| {
            edges(&result)
                .iter()
                .filter(|e| e["source"] == owner && e["relation"] == relation)
                .map(|e| e["target"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(targets("contains").len(), 1, "{label}");
        assert_eq!(targets("calls"), targets("contains"), "{label}");
    }
}

// ── 0.9.80: Markdown link destinations (#4178) ────────────────────────────────

fn page_refs(
    fx: &Fixture,
    vault: &Path,
    paths: &[PathBuf],
) -> (BTreeSet<String>, Vec<Value>, BTreeMap<String, String>) {
    let mut paths = paths.to_vec();
    paths.sort();
    let result = fx.extract_at(vault, &paths);
    let ids = nodes(&result)
        .iter()
        .filter_map(|n| n["id"].as_str().map(str::to_string))
        .collect();
    let refs = edges(&result)
        .iter()
        .filter(|e| e["relation"] == "references")
        .cloned()
        .collect();
    let pages = nodes(&result)
        .iter()
        .filter(|n| n["node_kind"] == "page")
        .map(|n| {
            (
                n["source_file"].as_str().unwrap().into(),
                n["id"].as_str().unwrap().into(),
            )
        })
        .collect();
    (ids, refs, pages)
}

#[test]
fn markdown_link_destination_with_spaces() {
    let fx = Fixture::new();
    let vault = fx.root.join("vault");
    let docs: Vec<PathBuf> = ["My Note.md", "sub/Deep Note.md", "Ref One.md", "Ref Two.md"]
        .iter()
        .map(|d| fx.write(&format!("vault/{d}"), "# Note\n"))
        .collect();
    let entry = fx.write(
        "vault/entry.md",
        "See [a](My%20Note.md), [b](<sub/Deep Note.md> \"title\"), [c][r1] and [d][r2].\n\n\
         [r1]: <Ref One.md>\n[r2]: Ref%20Two.md\n",
    );
    let mut all = docs.clone();
    all.push(entry);
    let (ids, refs, pages) = page_refs(&fx, &vault, &all);
    let targets: BTreeSet<_> = refs
        .iter()
        .filter(|e| e["source"] == pages["entry.md"].as_str())
        .map(|e| e["target"].as_str().unwrap().to_string())
        .collect();
    let want: BTreeSet<_> = ["My Note.md", "sub/Deep Note.md", "Ref One.md", "Ref Two.md"]
        .iter()
        .map(|d| pages[*d].clone())
        .collect();
    assert_eq!(targets, want, "{refs:?}");
    for e in &refs {
        assert!(ids.contains(e["target"].as_str().unwrap()), "{e}");
    }
}

#[test]
fn markdown_link_forms_keep_their_existing_targets() {
    let fx = Fixture::new();
    let vault = fx.root.join("vault");
    let mut paths: Vec<PathBuf> = ["100%25.md", "My Note.md", "b.md", "d.md", "f.md", "h.md"]
        .iter()
        .map(|d| fx.write(&format!("vault/{d}"), "# Note\n"))
        .collect();
    paths.push(fx.write(
        "vault/entry.md",
        "See [x](100%25.md) and [[My%20Note]].\nThen [a](<b.md) -> [c](d.md).\n\
         And [e](<f.md) -> (see [g](h.md)).\n",
    ));
    paths.push(fx.write("vault/bare.md", "See [y](100%25).\n"));
    let (ids, refs, pages) = page_refs(&fx, &vault, &paths);
    let from = |page: &str| -> BTreeSet<String> {
        refs.iter()
            .filter(|e| e["source"] == pages[page].as_str())
            .map(|e| e["target"].as_str().unwrap().to_string())
            .collect()
    };
    let reached: BTreeSet<_> = from("entry.md").intersection(&ids).cloned().collect();
    let want: BTreeSet<_> = ["100%25.md", "b.md", "d.md", "f.md", "h.md"]
        .iter()
        .map(|d| pages[*d].clone())
        .collect();
    assert_eq!(reached, want, "{refs:?}");
    assert_eq!(
        from("bare.md"),
        BTreeSet::from([pages["100%25.md"].clone()])
    );
}

#[test]
fn markdown_decoded_link_stays_external() {
    let dir = tempfile::tempdir().unwrap();
    for raw in [
        "%2F%2Fhost%2Fshare%2Fx.md",
        "https%3A%2F%2Fexample.com%2Fa.md",
    ] {
        assert!(
            graphicle::extractors::markdown::resolve_markdown_link(raw, dir.path(), false)
                .unwrap()
                .is_none(),
            "{raw}"
        );
    }
}

#[test]
fn markdown_code_span_file_names_of_every_code_language_are_not_mentions() {
    // The Markdown port's copy of detect's CODE_EXTENSIONS (0.9.88 values).
    for name in [
        "report.cbl",
        "copy.cpy",
        "rpc.erl",
        "app.hrl",
        "Token.sol",
        "Main.vb",
    ] {
        assert!(
            graphicle::extractors::markdown::code_span_mention(name).is_none(),
            "{name}"
        );
    }
    assert!(graphicle::extractors::markdown::code_span_mention("pkg.helper").is_some());
}

// ── 0.9.81, 0.9.85: Elixir (#4206, #4207, #4132) ──────────────────────────────

fn elixir(source: &str) -> Value {
    let result = single("lib.ex", source);
    assert!(result.get("error").is_none(), "{result}");
    result
}

#[test]
fn elixir_keyword_def_bodies_are_walked() {
    let result = elixir(
        "defmodule App.Short do\n  def a(x), do: b(x)\n\n  def b(x) do\n    x\n  end\n\n  \
         defp c(x), do: d(x) |> e(x)\n\n  def g(x) when is_list(x), do: b(x)\nend\n",
    );
    let calls = raw_edge_labels(&result, "calls");
    assert!(calls.contains(&pair("a()", "b()")), "{calls:?}");
    assert!(calls.contains(&pair("g()", "b()")), "{calls:?}");
    assert!(!calls.iter().any(|(s, _)| s == "c()"), "{calls:?}");
    assert!(!calls.contains(&pair("b()", "b()")));
}

#[test]
fn elixir_defdelegate_defines_functions() {
    let result = elixir(
        "defmodule Facade do\n  defdelegate start(x), to: Worker\n  \
         defdelegate stop(x), to: Worker, as: :halt\n  def run(x) do\n    start(x)\n    stop(x)\n  end\nend\n",
    );
    let methods = raw_edge_labels(&result, "method");
    assert!(methods.contains(&pair("Facade", "start()")), "{methods:?}");
    assert!(methods.contains(&pair("Facade", "stop()")), "{methods:?}");
    let calls = raw_edge_labels(&result, "calls");
    assert!(calls.contains(&pair("run()", "start()")), "{calls:?}");
    assert!(calls.contains(&pair("run()", "stop()")), "{calls:?}");
}

#[test]
fn elixir_qualified_calls_bind_only_to_the_named_module() {
    // `__MODULE__.f()` and a nested module's auto-alias resolve in the file.
    let result = elixir(
        "defmodule App.Web do\n  defmodule Helpers do\n    def fmt(x) do\n      x\n    end\n  end\n\n  \
         def show(x) do\n    Helpers.fmt(x)\n  end\n\n  def again(x) do\n    __MODULE__.show(x)\n  end\nend\n",
    );
    let calls = raw_edge_labels(&result, "calls");
    assert!(calls.contains(&pair("show()", "fmt()")), "{calls:?}");
    assert!(calls.contains(&pair("again()", "show()")), "{calls:?}");
    // A remote call never reaches the file's own same-named def; it stays raw
    // with its module for the corpus pass.
    let result = elixir(
        "defmodule App.Admin do\n  alias App.Repo, as: R\n  def save(x) do\n    App.Repo.insert(x)\n    \
         R.insert(x)\n    mod = x\n    mod.insert(x)\n  end\n\n  def insert(x) do\n    x\n  end\nend\n",
    );
    assert!(!raw_edge_labels(&result, "calls").contains(&pair("save()", "insert()")));
    let modules: Vec<_> = raw_calls(&result)
        .into_iter()
        .filter(|c| c["callee"] == "insert")
        .map(|c| c["elixir_module"].clone())
        .collect();
    assert_eq!(
        modules,
        [
            Value::from("App.Repo"),
            Value::from("App.Repo"),
            Value::Null
        ]
    );
}

// ── 0.9.81: PHP casts the grammar cannot parse (#4202) ────────────────────────

#[test]
fn php_void_and_match_casts_are_blanked_in_code_only() {
    let src = b"<?php\n// (void) foo();\n/* (string) match (x) */\n\
                $a = '(void) Foo::bar();';\n$b = \"(int) match (1) {$a}\";\n\
                $c = <<<TXT\n(void) Heredoc::thing();\nTXT;\n\
                $d = <<<'RAW'\n(bool) match (y) {}\nRAW;\n?>\n<p>(void) html</p>\n";
    assert!(php_blank_unparsed_casts(src).is_none());
    let src = b"<?php\n(\n  VOID\n) foo();\n$x = (string)\n  match (1) { 1 => 2 };\n";
    let out = php_blank_unparsed_casts(src).unwrap();
    assert_eq!(out.len(), src.len());
    let newlines = |b: &[u8]| {
        b.iter()
            .enumerate()
            .filter(|(_, c)| **c == b'\n')
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    };
    assert_eq!(newlines(&out), newlines(src));
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("VOID") && !text.contains("(string)") && text.contains("match (1)"));
}

#[test]
fn php_casts_keep_the_calls_after_them() {
    let kernel = |cast: &str| {
        format!(
            "<?php\nnamespace App;\nclass Kernel {{\n    public function boot(): void {{\n        \
             {cast}Cache::warm('routes');\n        {cast}$this->ready();\n    }}\n    \
             private function ready(): void {{}}\n}}\n"
        )
    };
    let cast = single("Kernel.php", &kernel("(void) "));
    let plain = single("Kernel.php", &kernel(""));
    assert!(cast.get("parse_errors").is_none(), "{cast}");
    assert_eq!(
        raw_edge_labels(&cast, "calls"),
        raw_edge_labels(&plain, "calls")
    );
    assert!(raw_edge_labels(&cast, "calls").contains(&pair(".boot()", ".ready()")));
    let picker = single(
        "Picker.php",
        "<?php\nnamespace App;\nclass Picker {\n    public function label(int $mode): string {\n        \
         return (string) match ($mode) {\n            1 => $this->first(),\n            \
         default => $this->second(),\n        };\n    }\n    public function first(): int { return 1; }\n    \
         public function second(): int { return 2; }\n    \
         public function after(): string { return $this->label(1); }\n}\n",
    );
    assert!(picker.get("parse_errors").is_none(), "{picker}");
    let calls = raw_edge_labels(&picker, "calls");
    for (s, t) in [
        (".label()", ".first()"),
        (".label()", ".second()"),
        (".after()", ".label()"),
    ] {
        assert!(calls.contains(&pair(s, t)), "{calls:?}");
    }
}

// ── 0.9.82, 0.9.86: Rust (#4238, #4135) ───────────────────────────────────────

fn rust_edges(source: &str, relation: &str) -> BTreeSet<(String, String, String)> {
    let result = single("lib.rs", source);
    let by_id: BTreeMap<&str, &Value> = nodes(&result)
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n))
        .collect();
    edges(&result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .filter_map(|e| {
            let source = by_id.get(e["source"].as_str()?)?;
            let target = by_id.get(e["target"].as_str()?)?;
            Some((
                source["label"].as_str()?.into(),
                target["label"].as_str()?.into(),
                target["source_location"].as_str()?.into(),
            ))
        })
        .collect()
}

fn triple(a: &str, b: &str, c: &str) -> (String, String, String) {
    (a.into(), b.into(), c.into())
}

#[test]
fn rust_type_qualified_calls_bind_to_that_types_method() {
    let calls = rust_edges(
        "struct A;\nstruct B;\nimpl A { fn new() -> A { A } }\nimpl B { fn new() -> B { B } }\n\
         fn make_a() -> A { A::new() }\nfn make_b() -> B { crate::B::new() }\n\
         impl A { fn again() -> A { Self::new() } }\n",
        "calls",
    );
    assert!(
        calls.contains(&triple("make_a()", ".new()", "L3")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&triple("make_b()", ".new()", "L4")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&triple(".again()", ".new()", "L3")),
        "{calls:?}"
    );
    assert!(!calls.contains(&triple("make_a()", ".new()", "L4")));
    assert!(!calls.contains(&triple("make_b()", ".new()", "L3")));
    let calls = rust_edges(
        "use std::sync::Arc;\nstruct Builder;\n\
         impl Builder { fn new() -> Builder { Builder } fn default() -> Builder { Builder } }\n\
         fn shared() -> Arc<u8> { Arc::new(1) }\nfn options() -> Vec<u8> { Vec::default() }\n",
        "calls",
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.0 == "shared()" || c.0 == "options()"),
        "{calls:?}"
    );
}

#[test]
fn rust_self_calls_and_call_shapes_use_their_own_index() {
    let calls = rust_edges(
        "struct A;\nstruct B;\nimpl A {\n    fn run(&self) { self.go(); self.stop(); }\n    \
         fn go(&self) {}\n}\nimpl B {\n    fn go(&self) {}\n    fn stop(&self) {}\n}\n",
        "calls",
    );
    assert!(
        calls.contains(&triple(".run()", ".go()", "L5")),
        "{calls:?}"
    );
    assert!(!calls.contains(&triple(".run()", ".go()", "L8")));
    assert!(!calls.contains(&triple(".run()", ".stop()", "L9")));
    let calls = rust_edges(
        "struct Child;\nimpl Drop for Child { fn drop(&mut self) {} }\nfn total() -> usize { 0 }\n\
         fn close(c: Child) { drop(c); }\nfn count(v: &Stats) -> usize { v.total() }\n",
        "calls",
    );
    assert!(
        !calls.iter().any(|c| c.0 == "close()" || c.0 == "count()"),
        "{calls:?}"
    );
    let calls = rust_edges(
        "use std::io;\nstruct Code;\nstruct Failure;\n\
         impl From<Code> for i32 { fn from(c: Code) -> i32 { 0 } }\n\
         impl From<Failure> for io::Error { fn from(f: Failure) -> io::Error { todo!() } }\n\
         fn status(c: Code) -> i32 { i32::from(c) }\nfn fail(f: Failure) -> io::Error { io::Error::from(f) }\n",
        "calls",
    );
    assert!(
        calls.contains(&triple("status()", ".from()", "L4")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&triple("fail()", ".from()", "L5")),
        "{calls:?}"
    );
    let refs = rust_edges(
        "enum Shape { Circle(u32), Square(u32) }\nfn mk() -> Shape { Shape::Circle(1) }\n",
        "references",
    );
    assert!(refs.contains(&triple("mk()", "Circle", "L1")), "{refs:?}");
}

#[test]
fn rust_self_call_to_another_files_impl_still_resolves() {
    let fx = Fixture::new();
    let a = fx.write(
        "a.rs",
        "pub struct A;\nimpl A { pub fn launch(&self) {} }\n",
    );
    let b = fx.write(
        "b.rs",
        "use crate::a::A;\npub struct B;\nimpl B { pub fn launch(&self) {} }\n\
         impl A { pub fn run(&self) { self.launch(); } }\n",
    );
    let result = fx.extract(&[a, b]);
    let by_id: BTreeMap<&str, &Value> = nodes(&result)
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n))
        .collect();
    let targets: BTreeSet<_> = edges(&result)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .filter(|e| {
            by_id
                .get(e["source"].as_str().unwrap())
                .is_some_and(|n| n["label"] == ".run()")
        })
        .map(|e| {
            Path::new(
                by_id[e["target"].as_str().unwrap()]["source_file"]
                    .as_str()
                    .unwrap(),
            )
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
        })
        .collect();
    assert_eq!(targets, BTreeSet::from(["a.rs".to_string()]));
}

/// The `imports_from` targets and lines a `use` declaration gives a file,
/// without the grouping markers.
fn use_edges(source: &str) -> Vec<(String, String)> {
    let result = single("user.rs", source);
    let mut out: Vec<_> = edges(&result)
        .iter()
        .filter(|e| e["context"] == "import" && e.get("_rust_use_group").is_none())
        .map(|e| {
            (
                e["target"].as_str().unwrap().to_string(),
                e["source_location"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn rust_use_tree_imports_what_its_plain_list_does() {
    for (tree, plain) in [
        (
            "use crate::util::{parse, render};\n",
            "use crate::util::parse; use crate::util::render;\n",
        ),
        (
            "use crate::{util::{parse, render}, lib};\n",
            "use crate::util::parse; use crate::util::render; use crate::lib;\n",
        ),
        (
            "use crate::util::{self, parse};\n",
            "use crate::util; use crate::util::parse;\n",
        ),
        ("use crate::util::{self as u};\n", "use crate::util as u;\n"),
        (
            "use crate::util::{parse as p, render};\n",
            "use crate::util::parse as p; use crate::util::render;\n",
        ),
        ("use crate::util :: {*};\n", "use crate::util :: *;\n"),
        (
            "use crate::{util::*, lib};\n",
            "use crate::util::*; use crate::lib;\n",
        ),
        (
            "use {crate::util::parse, std::io};\n",
            "use crate::util::parse; use std::io;\n",
        ),
        (
            "use ::std::{io, fmt::Write};\n",
            "use ::std::io; use ::std::fmt::Write;\n",
        ),
        (
            "use std::{{self}, {io::*}};\n",
            "use std; use std::io::*;\n",
        ),
        (
            "use super::{util, lib::Thing};\n",
            "use super::util; use super::lib::Thing;\n",
        ),
        (
            "pub use self::{a::B, c};\n",
            "pub use self::a::B; pub use self::c;\n",
        ),
        (
            "use crate::util::{\n    parse,\n    render as draw,\n};\n",
            "\nuse crate::util::parse;\nuse crate::util::render as draw;\n\n",
        ),
    ] {
        assert_eq!(use_edges(tree), use_edges(plain), "{tree}");
    }
}

#[test]
fn rust_use_groups_mark_the_group_edge_and_each_binding() {
    let result = single(
        "user.rs",
        "use crate::util::{self, parse, render as draw, nested::{deep, *}};\nuse {a::b, c};\nuse crate::util::parse;\n",
    );
    let marked = |key: &str| {
        let mut out: Vec<_> = edges(&result)
            .iter()
            .filter(|e| e.get(key) == Some(&Value::Bool(true)))
            .map(|e| {
                (
                    e["target"].as_str().unwrap().to_string(),
                    e["source_location"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        out.sort();
        out
    };
    let l = |t: &str, line: &str| (t.to_string(), line.to_string());
    assert_eq!(marked("_rust_use_group"), [l("util", "L1")]);
    assert_eq!(
        marked("_rust_use_binding"),
        [
            l("b", "L2"),
            l("c", "L2"),
            l("deep", "L1"),
            l("nested", "L1"),
            l("parse", "L1"),
            l("render_as_draw", "L1"),
            l("util", "L1"),
        ]
    );
    // A plain `use` is unchanged and unmarked.
    assert!(edges(&result).iter().any(|e| {
        e["target"] == "parse"
            && e["source_location"] == "L3"
            && e.get("_rust_use_binding").is_none()
    }));
}

/// Type references drop prelude names unless the file declares a struct, enum
/// or trait of that name (upstream `extract_rust` rebinds `local_type_names`
/// to the declared types before walking). A `type Result<T> = ...` alias or a
/// `union` mints no sourceless `Result` node and no `references` edges.
#[test]
fn rust_prelude_alias_is_not_a_type_reference() {
    let result = single(
        "lib.rs",
        "pub type Result<T> = std::result::Result<T, Error>;\npub union Option { a: u8 }\n\
         pub struct Error;\nfn f() -> Result<i32> { todo!() }\n\
         fn g(x: Result<String>, y: Option) -> Vec<Error> { todo!() }\n",
    );
    let labels = label_set(&result);
    assert!(
        !labels.contains("Result") && !labels.contains("Option"),
        "{labels:?}"
    );
    let refs = rust_edges(
        "pub type Result<T> = std::result::Result<T, Error>;\npub union Option { a: u8 }\n\
         pub struct Error;\nfn f() -> Result<i32> { todo!() }\n\
         fn g(x: Result<String>, y: Option) -> Vec<Error> { todo!() }\n",
        "references",
    );
    assert!(
        refs.iter().all(|r| r.1 != "Result" && r.1 != "Option"),
        "{refs:?}"
    );
    assert!(refs.contains(&triple("g()", "Error", "L3")), "{refs:?}");
    // A declared struct of a prelude name still shadows it.
    let refs = rust_edges(
        "struct Result<T>(T);\nfn f() -> Result<i32> { todo!() }\n",
        "references",
    );
    assert!(refs.contains(&triple("f()", "Result", "L1")), "{refs:?}");
}

// ── 0.9.85: TypeScript `export type *` (#3942) ────────────────────────────────

#[test]
fn ts_export_type_star_is_blanked_to_a_plain_star_export() {
    let source = b"export type * from \"./types.js\";\n";
    let normalized = normalize_ts_export_type_star(source).unwrap();
    assert_eq!(normalized, b"export      * from \"./types.js\";\n");
    assert!(normalize_ts_export_type_star(b"export * from \"./x\";\n").is_none());
    assert!(normalize_ts_export_type_star(b"export type { X } from \"./x\";\n").is_none());
    let fx = Fixture::new();
    let index = fx.write("index.ts", "export type * from \"./types.js\";\n");
    let types = fx.write(
        "types.ts",
        "export type FetchFn = typeof globalThis.fetch;\n",
    );
    let result = fx.extract(&[index, types]);
    let index_id = make_id([fx.root.join("index.ts").to_string_lossy().as_ref()]);
    let relations: BTreeSet<_> = edges(&result)
        .iter()
        .filter(|e| {
            e["source"]
                .as_str()
                .is_some_and(|s| s == index_id || s == "index" || s.ends_with("index_ts"))
        })
        .map(|e| e["relation"].as_str().unwrap().to_string())
        .collect();
    assert!(
        relations.contains("imports_from") || relations.contains("re_exports"),
        "{result}"
    );
    assert!(label_set(&result).contains("FetchFn"));
    assert!(
        result.get("parse_errors").is_none()
            || result["parse_errors"].as_array().is_some_and(Vec::is_empty)
    );
}

// ── 0.9.85-0.9.87: Julia (#4126, #4128, #4310, #4351) ─────────────────────────

fn julia(source: &str) -> Value {
    let result = single("demo.jl", source);
    assert!(result.get("error").is_none(), "{result}");
    result
}

#[test]
fn julia_where_clause_and_parametric_types_are_extracted() {
    let result = julia(
        "function combine(a::Vector{T}, b::Vector{T}) where {T<:Number}\n    merge_all(a, b)\nend\n\
         function merge_all(a, b)\n    return a\nend\nshort(x::T) where {T} = merge_all(x, x)\n",
    );
    let labels = label_set(&result);
    assert!(
        labels.contains("combine()") && labels.contains("short()"),
        "{labels:?}"
    );
    let calls = edge_labels(&result, "calls", None);
    assert!(calls.contains(&pair("combine", "merge_all")), "{calls:?}");
    assert!(calls.contains(&pair("short", "merge_all")), "{calls:?}");
    let result = julia(
        "struct Box{T}\n    value::T\nend\nabstract type Shape{T} end\nstruct Sq{T} <: Shape{T}\nend\n\
         mutable struct M{T,U}\n    a::T\nend\n",
    );
    let types = label_set(&result);
    for want in ["Box", "Shape", "Sq", "M"] {
        assert!(types.contains(want), "{types:?}");
    }
    assert!(edge_labels(&result, "inherits", None).contains(&pair("Sq", "Shape")));
    assert!(!types.contains("T"), "{types:?}");
}

const JULIA_DISPATCH: &str = "\nmodule Demo\nabstract type Shape end\nstruct Circle{T<:Real} <: Shape\n    r::T\nend\n\
helper(x) = 2x\nfunction param(x::T) where {T<:Real}\n    helper(x)\nend\nfunction rettype(x)::Float64\n    param(x)\nend\n\
function Base.show(io::IO, c::Circle)\n    print(io, c.r)\nend\nBase.length(c::Circle) = 1\n\
area(c::Circle{T}) where T = pi * c.r^2\n(c::Circle)(x) = area(c) * x\nBase.:+(a::Circle, b::Circle) = Circle(a.r + b.r)\n\
@inline function fast(x)\n    x |> helper\nend\nbcast(xs) = helper.(xs)\nend\n";

#[test]
fn julia_dispatch_definitions_broadcast_and_pipe() {
    let result = julia(JULIA_DISPATCH);
    let labels = label_set(&result);
    for want in [
        "param()",
        "rettype()",
        "Base.show()",
        "Base.length()",
        "area()",
        "(Circle)()",
        "Base.:+()",
        "fast()",
        "bcast()",
        "Circle",
    ] {
        assert!(labels.contains(want), "{want}: {labels:?}");
    }
    assert!(!labels.contains("T"));
    assert!(raw_edge_labels(&result, "inherits").contains(&pair("Circle", "Shape")));
    let calls = raw_edge_labels(&result, "calls");
    for (s, t) in [
        ("fast()", "helper()"),
        ("bcast()", "helper()"),
        ("param()", "helper()"),
        ("rettype()", "param()"),
    ] {
        assert!(calls.contains(&pair(s, t)), "{s} -> {t}: {calls:?}");
    }
}

#[test]
fn julia_functor_names_come_from_the_type_not_the_variable() {
    for source in [
        "struct Box{T}\n    v::T\nend\n(b::Box{T})(x) where T = x\n(b::Box)(x, y) = x\n(::Box{T})(x, y, z) where T = x\n",
        "(b::Mod.Box)(x) = x\n(b::Mod.Box{T})(x, y) where T = x\n(::Mod.Box)(x, y, z) = x\n(b::A.B.Box)(x, y, z, w) = x\n",
    ] {
        let result = julia(source);
        let functors: Vec<_> = nodes(&result)
            .iter()
            .filter(|n| n["id"].as_str().unwrap().ends_with("_call"))
            .map(|n| n["label"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(functors, ["(Box)()"], "{source}");
    }
}

// ── 0.9.85: Fortran named generic interfaces (#4141) ──────────────────────────

#[test]
fn fortran_generic_interface_is_a_callable_node() {
    let result = single(
        "shapes.f90",
        "module shapes\n  interface area\n    module procedure area_circle, area_square\n  end interface\n\
         contains\n  real function area_circle(r)\n    real :: r\n    area_circle = r\n  end function\n\
         \x20 real function area_square(s)\n    real :: s\n    area_square = s\n  end function\n\
         \x20 subroutine report(x)\n    real :: x, a\n    a = area(x)\n  end subroutine\nend module\n",
    );
    assert!(result.get("error").is_none(), "{result}");
    assert!(label_set(&result).contains("area()"));
    let dispatch = edge_labels(&result, "dispatches_to", Some("generic_interface"));
    assert!(
        dispatch.contains(&pair("area", "area_circle")),
        "{dispatch:?}"
    );
    assert!(
        dispatch.contains(&pair("area", "area_square")),
        "{dispatch:?}"
    );
    assert!(edge_labels(&result, "calls", None).contains(&pair("report", "area")));
}

// ── 0.9.88: Go generic type parameters (#4383) ────────────────────────────────

fn go_sourceless(source: &str) -> (Value, Vec<String>) {
    let fx = Fixture::new();
    let path = fx.write("g.go", source);
    let result = fx.extract(&[path]);
    let sourceless = nodes(&result)
        .iter()
        .filter(|n| n["source_file"].as_str().is_none_or(str::is_empty))
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect();
    (result, sourceless)
}

#[test]
fn go_generic_type_params_mint_no_stub() {
    for source in [
        "package p\nfunc Map[T any, R comparable](x T, f func(T) R) R {\n    var r R\n    return r\n}\n",
        "package p\ntype Box[T any] struct { v T }\nfunc (b Box[T]) Get() T { return b.v }\n",
        "package p\ntype Container[T any] interface {\n    Add(item T)\n    Items() []T\n}\n",
    ] {
        let (result, sourceless) = go_sourceless(source);
        assert!(sourceless.is_empty(), "{source}: {sourceless:?}");
        assert!(
            !edges(&result)
                .iter()
                .any(|e| e["target"] == "t" || e["target"] == "r"),
            "{source}"
        );
    }
    let (result, _) = go_sourceless(
        "package p\ntype User struct { name string }\ntype Box[T any] struct { v T }\n\
         func Wrap[T any](u User) Box[T] { return Box[T]{} }\n",
    );
    let targets: BTreeSet<_> = raw_edge_labels(&result, "references")
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    assert!(
        targets.contains("User") && targets.contains("Box"),
        "{targets:?}"
    );
    assert!(!targets.contains("T"));
}

// ── 0.9.88: Objective-C quoted include metadata (#4354) ───────────────────────

#[test]
fn objc_unresolved_quoted_include_carries_its_spelling() {
    let result = single(
        "View.m",
        "#import \"Missing/Thing.h\"\n#import <Foundation/Foundation.h>\n@implementation View\n@end\n",
    );
    let quoted: Vec<_> = edges(&result)
        .iter()
        .filter(|e| e["relation"] == "imports" && e["target"] == "thing")
        .collect();
    assert_eq!(quoted.len(), 1, "{result}");
    assert_eq!(
        quoted[0]["metadata"],
        serde_json::json!({"raw_include": "Missing/Thing.h", "include_kind": "quoted"})
    );
    assert!(
        edges(&result)
            .iter()
            .any(|e| e["target"] == "foundation" && e.get("metadata").is_none())
    );
}

// ── 0.9.88: C# verbatim interpolated strings (#3376) ──────────────────────────

const OFFENDER: &str = "var s = $@\"a\"\"{e}\"\"\";";

fn cs_class(lines: &[&str]) -> String {
    format!(
        "using System;\n\npublic class Config\n{{\n{}\n}}\n",
        lines.join("\n")
    )
}

#[test]
fn csharp_members_after_the_offending_literal_keep_their_lines() {
    let offender = format!("    public string Offender(string e) {{ {OFFENDER} return s; }}");
    let src = cs_class(&[
        "    public void BeforeOne() { }",
        "",
        &offender,
        "",
        "    public void AfterOne() { }",
        "",
        "    public void AfterTwo() { }",
    ]);
    let fx = Fixture::new();
    let path = fx.write("Config.cs", &src);
    let result = fx.extract(&[path]);
    let mut seen = BTreeMap::new();
    for n in nodes(&result) {
        let name = symbol(n["label"].as_str().unwrap());
        if let Some(line) = n["source_location"]
            .as_str()
            .and_then(|l| l.strip_prefix('L'))
        {
            seen.insert(name, line.to_string());
        }
    }
    for (name, line) in [
        ("BeforeOne", "5"),
        ("Offender", "7"),
        ("AfterOne", "9"),
        ("AfterTwo", "11"),
    ] {
        assert_eq!(
            seen.get(name).map(String::as_str),
            Some(line),
            "{name}: {seen:?}"
        );
    }
}

#[test]
fn csharp_verbatim_mask_is_length_preserving_and_minimal() {
    let offender = format!("    public string Offender(string e) {{ {OFFENDER} return s; }}");
    let src = cs_class(&[&offender, "    public void AfterOne() { }"]).into_bytes();
    let masked = normalize_csharp_verbatim_interpolation(&src).unwrap();
    assert_eq!(masked.len(), src.len());
    let changed: Vec<_> = (0..src.len()).filter(|i| src[*i] != masked[*i]).collect();
    assert_eq!(changed.len(), 2);
    assert_eq!(changed[1], changed[0] + 1);
    assert_eq!(&src[changed[0]..changed[0] + 2], b"\"\"");
    assert_eq!(&masked[changed[0]..changed[0] + 2], b"  ");
    // A multi-line literal keeps its line breaks.
    let src = cs_class(&[
        "    public string Offender(string e) { var s = $@\"one",
        "two {e}\"\"\"; return s; }",
        "    public void AfterOne() { }",
    ])
    .into_bytes();
    let masked = normalize_csharp_verbatim_interpolation(&src).unwrap();
    assert_eq!(masked.len(), src.len());
    assert_eq!(
        masked.iter().filter(|b| **b == b'\n').count(),
        src.iter().filter(|b| **b == b'\n').count()
    );
    // Healthy files and the spelling inside comments are left alone.
    for src in [
        cs_class(&[
            "    public string Fine(string e) { var s = $@\"a{e}b\"; return s; }",
            "    public string AlsoFine(string e) { var s = $@\"a\"\"x\"\"\"; return s; }",
        ]),
        cs_class(&[
            &format!("    // example of the broken spelling: {OFFENDER}"),
            "    /* also here: var s = $@\"a\"\"{e}\"\"\"; */",
            "    public void AfterOne() { }",
        ]),
    ] {
        assert!(normalize_csharp_verbatim_interpolation(src.as_bytes()).is_none());
    }
    // Two offenders in one file are both cleared.
    let src = cs_class(&[&offender, "    public void Between() { }", &offender]).into_bytes();
    let masked = normalize_csharp_verbatim_interpolation(&src).unwrap();
    assert_eq!((0..src.len()).filter(|i| src[*i] != masked[*i]).count(), 4);
}

#[test]
fn csharp_long_prefix_run_scans_in_linear_time() {
    let mut src = b"class C { ".to_vec();
    src.extend(std::iter::repeat_n(b'$', 1_000_000));
    src.extend(b" }");
    let started = std::time::Instant::now();
    assert!(normalize_csharp_verbatim_interpolation(&src).is_none());
    assert!(started.elapsed().as_secs_f64() < 5.0);
}

// ── 0.9.88: rescued npm imports anchor to the package (#3084) ─────────────────

#[test]
fn rescued_bare_imports_are_package_module_nodes() {
    let fx = Fixture::new();
    let a = fx.write(
        "a.mts",
        "const x = (await import('lodash/fp/map')).default;\nexport const A = 1;\n",
    );
    let b = fx.write(
        "b.mts",
        "const y = (await import('lodash/debounce')).default;\nexport const B = 2;\n",
    );
    let result = fx.extract(&[a, b]);
    let stubs: Vec<_> = nodes(&result)
        .iter()
        .filter(|n| n["type"] == "module")
        .collect();
    assert!(!stubs.is_empty());
    assert!(
        stubs
            .iter()
            .all(|n| n["id"] == "lodash" && n["label"] == "lodash"),
        "{stubs:?}"
    );
    let targets: BTreeSet<_> = edges(&result)
        .iter()
        .filter(|e| e["relation"] == "dynamic_import")
        .map(|e| e["target"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(targets, BTreeSet::from(["lodash".to_string()]));

    let fx = Fixture::new();
    let a = fx.write(
        "a.mts",
        "const x = (await import('@scope-a/utils')).default;\nexport const A = 1;\n",
    );
    let b = fx.write(
        "b.mts",
        "const y = (await import('@scope-b/utils')).default;\nexport const B = 2;\n",
    );
    let result = fx.extract(&[a, b]);
    let stubs: BTreeSet<_> = nodes(&result)
        .iter()
        .filter(|n| n["type"] == "module")
        .map(|n| n["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        stubs,
        BTreeSet::from(["@scope-a/utils".to_string(), "@scope-b/utils".to_string()])
    );

    // An unresolved relative specifier is a path, never a module anchor.
    let result = single(
        "app.mts",
        "const h = (await import('./missing-helper')).default;\n",
    );
    assert!(
        nodes(&result)
            .iter()
            .filter(|n| n["label"].as_str().unwrap_or("").contains("missing"))
            .all(|n| n["type"] != "module"),
        "{result}"
    );
}

#[test]
fn declared_npm_dependency_keeps_its_import_edge() {
    let fx = Fixture::new();
    let manifest = fx.write(
        "package.json",
        "{\"name\": \"mre\", \"version\": \"1.0.0\", \"dependencies\": {\"postgres\": \"^3.4.0\"}}\n",
    );
    let app = fx.write(
        "app.mts",
        "const postgres = (await import('postgres')).default;\n\
         export async function connect(url: string): Promise<void> {\n  postgres(url, { max: 1 });\n}\n",
    );
    let result = fx.extract(&[app, manifest]);
    let ids: BTreeSet<_> = nodes(&result)
        .iter()
        .filter_map(|n| n["id"].as_str())
        .collect();
    let pg: Vec<_> = edges(&result)
        .iter()
        .filter(|e| {
            matches!(
                e["relation"].as_str(),
                Some("dynamic_import" | "imports_from")
            ) && e["target"].as_str().unwrap().contains("postgres")
        })
        .collect();
    assert!(!pg.is_empty(), "{result}");
    for e in pg {
        assert!(ids.contains(e["target"].as_str().unwrap()), "{e}");
    }
}

// ── 0.9.88: EJS templates (#3811, #2511) ──────────────────────────────────────

fn ejs_labels(result: &Value) -> BTreeMap<String, String> {
    let file = nodes(result)[0]["id"].clone();
    nodes(result)
        .iter()
        .filter(|n| n["id"] != file)
        .map(|n| {
            (
                n["label"].as_str().unwrap().into(),
                n["source_location"].as_str().unwrap_or("").into(),
            )
        })
        .collect()
}

fn ejs_edges<'a>(result: &'a Value, relation: &str) -> Vec<&'a Value> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == relation)
        .collect()
}

#[test]
fn ejs_is_dispatched_as_a_js_family_template() {
    let fx = Fixture::new();
    let path = fx.write("views/page.ejs", "<h1><%= title %></h1>\n");
    assert_eq!(
        graphicle::extract::get_extractor(&path).as_deref(),
        Some("ejs")
    );
    assert!(graphicle::extractors::models::JS_CACHE_BYPASS_SUFFIXES.contains(&".ejs"));
}

#[test]
fn ejs_scriptlets_and_inline_scripts_parse_with_their_lines() {
    let result = single(
        "page.ejs",
        "<h1><%= title %></h1>\n<%\nfunction greet(name) { return name; }\n%>\n<p><%= greet(user) %></p>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    assert_eq!(ejs_labels(&result)["greet()"], "L3");
    let result = single(
        "inline.ejs",
        "<script>\n  const data = <%- JSON.stringify(items) %>;\n  function boot() { return data; }\n</script>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    assert!(ejs_labels(&result).contains_key("boot()"));
    let result = single(
        "quirks.ejs",
        "<%# a comment %>\n<p>literal <%% not a tag %></p>\n<% function real() { return 1; } %>\n",
    );
    assert!(result.get("parse_errors").is_none(), "{result}");
    let labels = ejs_labels(&result);
    assert!(labels.contains_key("real()"));
    assert!(!labels.keys().any(|l| l.contains("not a tag")));
    let result = single("plain.ejs", "<h1>no script here</h1>\n");
    assert!(result.get("parse_errors").is_none());
    assert!(ejs_edges(&result, "includes").is_empty());
}

#[test]
fn ejs_masker_keeps_lines_and_neutralises_output_tags() {
    let src = "<p><%= a %></p>\r\n<script>x = <%- data %>;</script>\n<%_ go() _%>\n";
    let masked = ejs_mask_non_script(src);
    assert_eq!(masked.chars().count(), src.chars().count());
    assert_eq!(
        masked.match_indices(['\n', '\r']).collect::<Vec<_>>(),
        src.match_indices(['\n', '\r']).collect::<Vec<_>>()
    );
    assert!(masked.contains("x =   0        ;"), "{masked:?}");
    assert!(masked.contains("    go() _  "), "{masked:?}");
    // Script types are read with the reviewed attribute parser Astro and
    // Svelte share: a quoted JavaScript type keeps its body, data does not.
    let masked = ejs_mask_non_script(
        "<script type=\"module\">boot()</script>\n<script type=\"application/json\">{\"a\": 1}</script>\n",
    );
    assert!(masked.contains("boot()"), "{masked:?}");
    assert!(!masked.contains("\"a\""), "{masked:?}");
}

#[test]
fn ejs_requires_imports_includes_and_dynamic_imports() {
    let fx = Fixture::new();
    let tpl = fx.write(
        "views/page.ejs",
        "<%\nconst helpers = require('../lib/helpers');\nimport { format } from '../lib/format';\n%>\n<p><%= format(x) %></p>\n",
    );
    let helpers = fx.write("lib/helpers.js", "exports.h = {};\n");
    let format = fx.write("lib/format.ts", "export function format() {}\n");
    let result = extract_file(&tpl, None);
    let targets: BTreeSet<_> = edges(&result)
        .iter()
        .filter_map(|e| e["target_file"].as_str())
        .collect();
    assert!(targets.contains(helpers.to_str().unwrap()), "{result}");
    assert!(targets.contains(format.to_str().unwrap()), "{result}");

    // An include spelled relative to the view resolves to the partial file.
    let tpl = fx.write(
        "views/inbox.ejs",
        "<%- include('partials/header.ejs') %>\n<p>body</p>\n",
    );
    let partial = fx.write("views/partials/header.ejs", "<header>hi</header>\n");
    let result = extract_file(&tpl, None);
    let includes = ejs_edges(&result, "includes");
    assert_eq!(includes.len(), 1, "{result}");
    assert_eq!(includes[0]["target"], make_id([partial.to_str().unwrap()]));
    assert_eq!(includes[0]["target_file"], partial.to_str().unwrap());
    assert_eq!(includes[0]["source_location"], "L1");

    // An include that resolves to no file keeps a stub.
    let tpl = fx.write(
        "views/other.ejs",
        "<%- include('layouts/app', { title: 'Inbox' }) %>\n",
    );
    let result = extract_file(&tpl, None);
    let includes = ejs_edges(&result, "includes");
    assert_eq!(includes.len(), 1);
    assert_eq!(
        label_of(&result)[includes[0]["target"].as_str().unwrap()],
        "layouts/app"
    );

    let tpl = fx.write(
        "views/lazy.ejs",
        "<% import('./chart.js').then(function(m) { m.init(); }); %>\n",
    );
    let chart = fx.write("views/chart.js", "export function init() {}\n");
    let result = extract_file(&tpl, None);
    let dynamic = ejs_edges(&result, "dynamic_import");
    assert_eq!(dynamic.len(), 1, "{result}");
    assert_eq!(dynamic[0]["target_file"], chart.to_str().unwrap());

    let missing = extract_file(&fx.root.join("missing/Gone.ejs"), None);
    assert!(nodes(&missing).is_empty() && edges(&missing).is_empty());
}

#[test]
fn ejs_template_joins_the_corpus() {
    let fx = Fixture::new();
    let tpl = fx.write(
        "views/page.ejs",
        "<% function f() { return 1; } %>\n<p><%= f() %></p>\n",
    );
    let result = fx.extract(&[tpl]);
    assert!(label_set(&result).contains("page.ejs"), "{result}");
    assert!(label_set(&result).contains("f()"), "{result}");
}
