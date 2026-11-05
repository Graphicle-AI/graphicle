// Adapted from Graphify 0.9.97, Apache-2.0; see NOTICE.md.
//! Extraction changes ported from Graphify 0.9.97, from the upstream tests
//! of that release: JS/TS nested functions stay scoped, YAML keys as written
//! with their lines (#4784), rationale redaction gaps, aliased Terraform
//! providers, and fixes for Blade, COBOL, OCaml, Pascal, Razor, VB.NET,
//! SystemVerilog, Markdown (#4787, #4790), Fortran and pnpm globs (#4681).
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
fn by_label<'a>(result: &'a Value, label: &str) -> &'a Value {
    nodes(result)
        .iter()
        .find(|n| n["label"] == label)
        .unwrap_or_else(|| panic!("no node {label}"))
}
/// `(source, target, relation)` ids of every call edge.
fn call_ids(result: &Value) -> BTreeSet<(String, String, String)> {
    edges(result)
        .iter()
        .filter(|e| e["relation"] == "calls" || e["relation"] == "indirect_call")
        .map(|e| {
            (
                e["source"].as_str().unwrap_or("").to_string(),
                e["target"].as_str().unwrap_or("").to_string(),
                e["relation"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}
fn has(set: &BTreeSet<(String, String, String)>, s: &str, t: &str, r: &str) -> bool {
    set.contains(&(s.into(), t.into(), r.into()))
}

// --- JS/TS nested functions stay scoped -------------------------------------

#[test]
fn js_same_inner_name_in_two_functions_binds_to_its_own() {
    let f = Fixture::new();
    let e = call_ids(&f.extract_files(&[(
        "x.ts",
        "export function first() { function run() { return 1; } register(run); return run(); }\nexport function second() { function run() { return 2; } register(run); return run(); }\nfunction register(f: any) { return f; }\n",
    )]));
    assert!(has(&e, "x_first", "x_first_run", "calls"), "{e:?}");
    assert!(has(&e, "x_second", "x_second_run", "calls"));
    assert!(has(&e, "x_first", "x_first_run", "indirect_call"));
    assert!(has(&e, "x_second", "x_second_run", "indirect_call"));
    assert!(
        !e.iter()
            .any(|(s, t, _)| s == "x_first" && t == "x_second_run")
    );
    assert!(
        !e.iter()
            .any(|(s, t, _)| s == "x_second" && t == "x_first_run")
    );
}

#[test]
fn js_inner_function_does_not_take_other_callers() {
    let f = Fixture::new();
    let e = call_ids(&f.extract_files(&[(
        "x.ts",
        "export function helper() { return 0; }\nexport function a() { function helper() { return 1; } return helper(); }\nexport function b() { return helper(); }\n",
    )]));
    assert!(has(&e, "x_a", "x_a_helper", "calls"), "{e:?}");
    assert!(has(&e, "x_b", "x_helper", "calls"));
    assert!(!has(&e, "x_b", "x_a_helper", "calls"));
    let f = Fixture::new();
    let e = call_ids(&f.extract_files(&[
        ("lib.ts", "export function helper() { return 0; }\n"),
        (
            "x.ts",
            "import { helper } from \"./lib\";\nexport function a() { function helper() { return 1; } return helper(); }\nexport function b() { return helper(); }\n",
        ),
    ]));
    assert!(has(&e, "x_b", "lib_helper", "calls"), "{e:?}");
    assert!(!has(&e, "x_b", "x_a_helper", "calls"));
}

#[test]
fn js_siblings_and_deeper_inner_functions_still_resolve() {
    let f = Fixture::new();
    let e = call_ids(&f.extract_files(&[(
        "x.js",
        "function outer() {\n  function load() { return parse(); }\n  function parse() {\n    function step() { return load(); }\n    return step();\n  }\n  return load();\n}\n",
    )]));
    assert!(has(&e, "x_outer", "x_outer_load", "calls"), "{e:?}");
    assert!(has(&e, "x_outer_load", "x_outer_parse", "calls"));
    assert!(has(&e, "x_outer_parse", "x_outer_parse_step", "calls"));
    assert!(has(&e, "x_outer_parse_step", "x_outer_load", "calls"));
    let f = Fixture::new();
    let e = call_ids(&f.extract_files(&[(
        "Panels.tsx",
        "export const Left = () => { function onClick() { return 1; } return onClick(); };\nexport const Right = () => { function onClick() { return 2; } return onClick(); };\n",
    )]));
    assert!(
        has(&e, "panels_left", "panels_left_onclick", "calls"),
        "{e:?}"
    );
    assert!(has(&e, "panels_right", "panels_right_onclick", "calls"));
    assert!(!has(&e, "panels_left", "panels_right_onclick", "calls"));
}

// --- YAML keys as written, on their own lines (#4784) -------------------------

#[test]
fn yaml_keys_carry_their_own_line() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "values.yaml",
        "name: x\nimage:\n  repository: nginx\n\n  tag: '1.25'\nservice:\n  port: 80\n",
    )]);
    for (label, line) in [
        ("name", "L1"),
        ("image", "L2"),
        ("repository", "L3"),
        ("tag", "L5"),
        ("service", "L6"),
        ("port", "L7"),
    ] {
        assert_eq!(by_label(&r, label)["source_location"], line, "{label}");
    }
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "deploy.yaml",
        "kind: Deployment\nmetadata:\n  name: web\n---\nkind: Service\nmetadata:\n  name: web\n",
    )]);
    assert_eq!(by_label(&r, "Deployment web")["source_location"], "L1");
    assert_eq!(by_label(&r, "Service web")["source_location"], "L5");
}

#[test]
fn yaml_workflow_on_key_keeps_its_spelling() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        ".github/workflows/ci.yml",
        "name: CI\non:\n  push:\n    branches: [main]\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
    )]);
    assert!(!labels(&r).contains("True"));
    let on = by_label(&r, "on");
    assert!(on["id"].as_str().unwrap().ends_with("_ci_on"), "{on}");
    assert_eq!(on["source_location"], "L2");
    assert!(
        by_label(&r, "push")["id"]
            .as_str()
            .unwrap()
            .ends_with("_ci_on_push")
    );
    let f = Fixture::new();
    let r = f.extract_files(&[("pipeline-trigger.yml", "on: push\nretries: 3\n")]);
    assert!(labels(&r).contains("on"), "{r}");
}

// --- Redaction of rationale labels ---------------------------------------------

#[test]
fn redaction_covers_userinfo_pem_bodies_and_glued_tokens() {
    use graphicle::security::redact_secrets;
    assert_eq!(
        redact_secrets("redis://:s3cretpass@cache.internal"),
        "redis://[redacted]@cache.internal"
    );
    assert_eq!(
        redact_secrets("https://svc:p@ss@host/x"),
        "https://[redacted]@host/x"
    );
    assert_eq!(
        redact_secrets("https://svc:ab/cd@host"),
        "https://[redacted]@host"
    );
    for benign in [
        "https://host:8080/path?email=a@b.com",
        "see https://example.com:443/a and mail ops@example.com",
        "# NOTE: use the sk-learn-compatible-estimator-wrapper here",
    ] {
        assert_eq!(redact_secrets(benign), benign);
    }
    let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC";
    let full = format!(
        "key:\n-----BEGIN RSA PRIVATE KEY-----\n{body}\n-----END RSA PRIVATE KEY-----\nafter"
    );
    assert_eq!(redact_secrets(&full), "key:\n[redacted]\nafter");
    assert!(!redact_secrets(&format!("-----BEGIN PRIVATE KEY-----\n{body}")).contains(body));
    let ghp = format!("ghp_{}", "a".repeat(36));
    let aiza = format!("AIza{}", "b".repeat(35));
    for (secret, text) in [
        (
            "AKIAIOSFODNN7EXAMPLE".to_string(),
            "AKIAIOSFODNN7EXAMPLE_prod".to_string(),
        ),
        (ghp.clone(), format!("token_{ghp}")),
        (aiza.clone(), format!("{aiza}_backup")),
    ] {
        let out = redact_secrets(&text);
        assert!(
            !out.contains(&secret) && out.contains("[redacted]"),
            "{text}"
        );
    }
    for secret in [
        format!("xoxe-1-{}", "c".repeat(40)),
        format!("eyJhbGciOiJIUzI1NiJ9.e30.{}", "d".repeat(43)),
    ] {
        let out = redact_secrets(&format!("see {secret} here"));
        assert!(
            !out.contains(&secret) && out.contains("[redacted]"),
            "{secret}"
        );
    }
    assert!(redact_secrets(&format!("key sk-proj-{}", "a1".repeat(12))).contains("[redacted]"));
}

// --- Terraform aliased providers ------------------------------------------------

#[test]
fn terraform_aliased_providers_get_their_own_nodes_and_references() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "main.tf",
        "provider \"aws\" {\n  alias  = \"west\"\n  region = \"us-west-1\"\n}\n\nprovider \"aws\" {\n  alias  = \"east\"\n  region = \"us-east-1\"\n}\n\nresource \"aws_instance\" \"web\" {\n  provider = aws.west\n}\n",
    )]);
    let l = labels(&r);
    assert!(
        l.contains("provider.aws.west") && l.contains("provider.aws.east"),
        "{l:?}"
    );
    assert!(
        pairs(&r, "references").contains(&("aws_instance.web".into(), "provider.aws.west".into()))
    );
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "main.tf",
        "provider \"aws\" { region = \"us-east-1\" }\n\nresource \"aws_instance\" \"web\" {\n  provider = aws\n}\n\nlocals {\n  provider = data.aws_caller_identity.current.account_id\n}\n\ndata \"aws_caller_identity\" \"current\" {}\n",
    )]);
    let refs = pairs(&r, "references");
    assert!(
        refs.contains(&("aws_instance.web".into(), "provider.aws".into())),
        "{refs:?}"
    );
    assert!(refs.contains(&(
        "local.provider".into(),
        "data.aws_caller_identity.current".into()
    )));
}

// --- Single-language fixes -----------------------------------------------------

#[test]
fn blade_wire_click_binds_the_bare_method_name() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "page.blade.php",
        "<div wire:click=\"deleteItem(5)\">Delete</div>\n<div wire:click=\"save\">Save</div>\n<div wire:click.prevent=\"submit\">Submit</div>\n<div wire:click=\"$set('foo', 1)\">Reset</div>\n",
    )]);
    let bound = pairs(&r, "binds_method")
        .into_iter()
        .map(|(_, t)| t)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        bound,
        BTreeSet::from(["deleteItem".to_string(), "save".into(), "submit".into()])
    );
}

#[test]
fn cobol_end_program_restores_the_enclosing_program() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "nested.cbl",
        "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. PARENT.\n       PROCEDURE DIVISION.\n       PARENT-PARA.\n           CALL 'CHILD'.\n           STOP RUN.\n       IDENTIFICATION DIVISION.\n       PROGRAM-ID. CHILD.\n       PROCEDURE DIVISION.\n       CHILD-PARA.\n           DISPLAY 'CHILD'.\n       END PROGRAM CHILD.\n       PARENT-PARA-2.\n           DISPLAY 'BACK IN PARENT'.\n       END PROGRAM PARENT.\n",
    )]);
    let contains = pairs(&r, "contains");
    assert!(
        contains.contains(&("PARENT".into(), "PARENT-PARA-2".into())),
        "{contains:?}"
    );
    assert!(!contains.contains(&("CHILD".into(), "PARENT-PARA-2".into())));
}

#[test]
fn ocaml_and_chained_modules_and_classes_are_walked() {
    // The extractor itself, as upstream's test calls it.
    let f = Fixture::new();
    let run =
        |name: &str, text: &str| graphicle::extractors::ocaml::extract_ocaml(&f.write(name, text));
    let r = run(
        "pair.ml",
        "module Alpha = struct\n  let f x = x\nend\nand Beta = struct\n  let g x = x\nend\n",
    );
    assert!(
        ["Alpha", "f", "Beta", "g"]
            .iter()
            .all(|l| labels(&r).contains(*l)),
        "{r}"
    );
    assert!(pairs(&r, "contains").contains(&("Beta".into(), "g".into())));
    assert_eq!(by_label(&r, "Beta")["source_location"], "L4");
    let r = run(
        "cls.ml",
        "class alpha = object\n  method m = 1\nend\nand beta = object\n  method n = 2\nend\n",
    );
    let methods = pairs(&r, "method");
    assert!(
        methods.contains(&("alpha".into(), "m".into()))
            && methods.contains(&("beta".into(), "n".into())),
        "{r}"
    );
}

#[test]
fn pascal_record_with_methods_is_a_type() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "rec.pas",
        "unit Rec;\ninterface\ntype\n  TPoint = record\n    X, Y: Integer;\n    function Len: Integer;\n    class operator Add(A, B: TPoint): TPoint;\n  end;\nimplementation\nfunction TPoint.Len: Integer;\nbegin\n  Result := X;\nend;\nclass operator TPoint.Add(A, B: TPoint): TPoint;\nbegin\n  Result := A;\nend;\nend.\n",
    )]);
    let l = labels(&r);
    assert!(l.contains("TPoint"), "{l:?}");
    let methods = pairs(&r, "method")
        .into_iter()
        .filter(|(s, _)| s == "TPoint")
        .count();
    assert!(methods >= 2, "{:?}", pairs(&r, "method"));
}

#[test]
fn razor_implements_and_vbnet_operator_and_sv_task() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "Widget.razor",
        "@implements IDisposable\n<div>hi</div>\n@code {\n    public void Dispose() {}\n}\n",
    )]);
    assert!(
        pairs(&r, "implements")
            .iter()
            .any(|(_, t)| t == "IDisposable"),
        "{:?}",
        pairs(&r, "implements")
    );
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "Money.vb",
        "Public Class Money\n    Public Shared Operator +(a As Money, b As Money) As Money\n        Return a\n    End Operator\nEnd Class\n",
    )]);
    assert!(
        pairs(&r, "method").iter().any(|(s, _)| s == "Money"),
        "{:?}",
        pairs(&r, "method")
    );
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "drv.sv",
        "class Config;\nendclass\nclass Driver;\n  task run(Config cfg);\n    int local_count;\n  endtask\nendclass\n",
    )]);
    let method = pairs(&r, "method");
    assert!(
        method.contains(&("Driver".into(), "run".into())),
        "{method:?}"
    );
    assert!(
        pairs(&r, "references")
            .iter()
            .any(|(s, t)| s == "run" && t == "Config")
    );
}

#[test]
fn markdown_headings_and_fences_follow_commonmark() {
    let f = Fixture::new();
    let r = f.extract_files(&[(
        "doc.md",
        "# Title\n\n## Setup ##\n\n## C#\n\n    # indented code\n\n````\n```\n# inside\n```\n````\n\n~~~\n# tilde inside\n~~~\n\n```python print(1)``` inline\n\n## After\n",
    )]);
    let l = labels(&r);
    assert!(l.contains("Setup") && !l.contains("Setup ##"), "{l:?}");
    assert!(l.contains("C#"));
    assert!(!l.contains("indented code") && !l.contains("inside") && !l.contains("tilde inside"));
    assert!(l.contains("After"));
}

#[test]
fn fortran_call_to_another_files_subroutine_binds_its_definition() {
    let f = Fixture::new();
    let r = f.extract_files(&[
        ("lib.f90", "module lib\ncontains\n  subroutine helper()\n  end subroutine helper\nend module lib\n"),
        ("main.f90", "program main\n  use lib\n  call helper()\nend program main\n"),
    ]);
    let calls = edges(&r)
        .iter()
        .filter(|e| e["relation"] == "calls")
        .filter_map(|e| node(&r, &e["target"]))
        .filter(|n| n["label"].as_str().unwrap_or("").trim_end_matches("()") == "helper")
        .map(|n| n["source_file"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert!(calls.iter().any(|f| f.ends_with("lib.f90")), "{calls:?}");
}

#[test]
fn pnpm_workspace_glob_comments_and_rejected_globs() {
    for workspace in [
        "  - \"pk/*\" # staging runner\n",
        "  - 'pk/**' # x\n",
        "  - pk/* # note\n",
    ] {
        let f = Fixture::new();
        f.write(
            "pk/p/package.json",
            "{\"name\": \"@x/p\", \"main\": \"index.ts\"}",
        );
        let lib = f.write("pk/p/index.ts", "export function boot() { return 1; }\n");
        let app = f.write(
            "app/c.ts",
            "import { boot } from \"@x/p\";\nexport function go() { return boot(); }\n",
        );
        f.write("package.json", "{\"name\": \"root\"}");
        f.write("pnpm-workspace.yaml", &format!("packages:\n{workspace}"));
        let r = f.run(&[app, lib]);
        let resolved = edges(&r)
            .iter()
            .filter(|e| e["relation"] == "imports_from")
            .filter_map(|e| node(&r, &e["target"]))
            .any(|n| {
                n["source_file"]
                    .as_str()
                    .unwrap_or("")
                    .ends_with("p/index.ts")
            });
        assert!(resolved, "{workspace}");
    }
}
