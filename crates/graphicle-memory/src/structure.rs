//! Graphify Tree-sitter extraction adaptation (Apache-2.0 and retained MIT
//! portions). Limited to explicit source-located declarations/imports/calls;
//! unknown receiver types and ambiguous targets are never silently merged.
use crate::Result;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::sync::OnceLock;
use tree_sitter::{Language, Node, Parser};

/// The revision stored with every parse a repository sync caches
/// (`repo_parses`): a new value re-parses cached files once. Bump it whenever
/// the analysis output changes. 7: the issue text says that
/// type and overload resolution are not performed instead of pointing at a
/// configured language server.
pub const REVISION: &str = "graphicle-static-rust-7";

/// Per-file analysis limits. No limit fails an analysis: a
/// file past one is analysed as far as the limit allows, its analysis is
/// `complete: false` with an issue that names the limit, and `limited` says
/// which limit it reached (`"size"`, `"nodes"`, `"calls"` or `"symbols"`).
/// The application passes its operator's settings; [`Limits::default`] are
/// Graphicle Core's defaults. Analyses within the limits are unchanged, so
/// [`REVISION`] stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Content bytes of a file that is analysed at all; a larger file gets a
    /// text-only analysis (`"size"`).
    pub bytes: usize,
    /// Calls collected per file (`"calls"`).
    pub calls: usize,
    /// Declarations collected per file, and with the bundled parsers imports
    /// and document links (each list) (`"symbols"`).
    pub symbols: usize,
    /// Syntax nodes walked per file (`"nodes"`).
    pub nodes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 4 * 1024 * 1024,
            calls: 50_000,
            symbols: 16_000,
            nodes: 2_000_000,
        }
    }
}

/// The longest source path analysed, in bytes; a longer one gets the
/// text-only analysis.
pub const MAX_PATH_BYTES: usize = 2000;

/// Most analyses [`graph`] relates in one call. The application's project
/// limit applies first; past this a project's file relationships come from
/// its projected code graph only.
pub const GRAPH_FILE_LIMIT: usize = 5000;

/// The revision a cache records for an analysis that is not complete within
/// `limits` (one with `limited` or `skipped`): such an analysis is redone
/// once when the limits change, while complete analyses keep [`REVISION`].
pub fn limited_revision(limits: &Limits) -> String {
    format!(
        "{REVISION}+limits-{}-{}-{}-{}",
        limits.bytes, limits.calls, limits.symbols, limits.nodes
    )
}

/// The issues of `analysis` that its [`Limits`] caused (none for an
/// analysis within them).
pub fn limit_issues(analysis: &Value) -> Vec<&str> {
    analysis["issues"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|issue| {
            issue.starts_with("Too large to analyse")
                || issue.starts_with("Path too long to analyse")
                || issue.starts_with("Analysis stopped at ")
                || issue.contains(" list truncated at ")
        })
        .collect()
}

/// `12345678` as `12,345,678`.
fn grouped(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `n` things: `1 call`, `50,000 calls`.
fn counted(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", grouped(n))
    }
}

/// The analysis of a file that is not parsed: no symbols, imports, calls or
/// links; its text is still indexed and searchable.
fn text_only(issue: String, limited: Option<&str>, skipped: Option<&str>) -> Value {
    let mut analysis = json!({"revision":REVISION,"symbols":[],"imports":[],"calls":[],"links":[],"shadowed_bindings":[],
        "parser":"text","complete":false,"issues":[issue]});
    if let Some(limited) = limited {
        analysis["limited"] = json!(limited);
    }
    if let Some(reason) = skipped {
        analysis["skipped"] = json!(reason);
    }
    analysis
}

/// The analysis recorded for a file whose analysis failed (`reason`): the
/// file is skipped by structure analysis and searchable as text only.
pub fn skipped(reason: &str) -> Value {
    text_only(
        format!("Not analysed: {reason}; searchable as text only"),
        None,
        Some(reason),
    )
}

fn value<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.byte_range()]).unwrap_or("")
}
fn field<'a>(node: Node<'_>, name: &str, source: &'a [u8]) -> &'a str {
    node.child_by_field_name(name)
        .map(|n| value(n, source))
        .unwrap_or("")
}

fn pattern_names(node: Node<'_>, source: &[u8], out: &mut BTreeSet<String>) {
    if matches!(
        node.kind(),
        "identifier" | "shorthand_property_identifier_pattern"
    ) {
        out.insert(value(node, source).to_string());
        return;
    }
    if matches!(
        node.kind(),
        "attribute" | "member_expression" | "type_annotation" | "type_identifier" | "generic_type"
    ) {
        return;
    }
    for name in ["name", "pattern", "left"] {
        if let Some(pattern) = node.child_by_field_name(name) {
            pattern_names(pattern, source, out);
            return;
        }
    }
    if matches!(node.kind(), "typed_parameter" | "typed_default_parameter") {
        if let Some(pattern) = node.named_child(0) {
            pattern_names(pattern, source, out);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        pattern_names(child, source, out);
    }
}

fn direct_token(node: Node<'_>, token: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == token)
}

// A reassigned module attribute is no longer an unambiguous imported callable.
// Conservatively withhold bindings in that lexical scope; do not infer data flow.
fn assignment_names(node: Node<'_>, source: &[u8], out: &mut BTreeSet<String>) {
    if matches!(
        node.kind(),
        "attribute" | "member_expression" | "subscript" | "subscript_expression"
    ) {
        if let Some(object) = node
            .child_by_field_name("object")
            .or_else(|| node.child_by_field_name("value"))
        {
            assignment_names(object, source, out);
        }
    } else {
        pattern_names(node, source, out);
    }
}

pub fn expanded_language(path: &str) -> Option<&'static str> {
    let name = Path::new(path).file_name()?.to_str()?;
    if name == "Dockerfile" {
        return Some("dockerfile");
    }
    if name == "Makefile" {
        return Some("make");
    }
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "go" => "go",
        "java" => "java",
        "astro" => "astro",
        "razor" | "cshtml" => "razor",
        "robot" => "robot",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "cu" | "cuh" | "metal" => "cpp",
        "rb" | "rake" => "ruby",
        "cs" => "csharp",
        "kt" | "kts" => "kotlin",
        "scala" => "scala",
        "php" => "php",
        "swift" => "swift",
        "lua" | "luau" => "lua",
        "zig" => "zig",
        "ps1" | "psm1" | "psd1" => "powershell",
        "ex" | "exs" => "elixir",
        "m" | "mm" => "objc",
        "jl" => "julia",
        "f" | "f90" | "f95" | "f03" | "f08" => "fortran",
        "vue" => "vue",
        "svelte" => "svelte",
        "dart" => "dart",
        "ml" | "mli" => "ocaml",
        "lisp" | "cl" | "lsp" | "asd" => "commonlisp",
        "v" | "sv" | "svh" => "verilog",
        "sql" => "sql",
        "pas" | "pp" | "dpr" | "dpk" | "lpr" | "inc" => "pascal",
        "sh" | "bash" => "bash",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "tf" | "tfvars" | "hcl" => "hcl",
        "cls" | "trigger" => "apex",
        "xml" | "csproj" | "fsproj" | "vbproj" | "slnx" | "xaml" | "lpk" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "r" => "r",
        "hs" => "haskell",
        "cmake" => "cmake",
        _ => return None,
    })
}

/// The time a parse of `content` may take: two seconds, plus a millisecond
/// per kilobyte.
fn parse_budget_ms(content: &str) -> u64 {
    2000 + content.len() as u64 / 1000
}

/// The innermost symbol around `start..end` (the first one on ties), as the
/// spans `(byte_start, byte_end)` of `symbols` in their order.
fn innermost(spans: &[(u64, u64)], start: u64, end: u64) -> Option<usize> {
    let mut best: Option<(u64, usize)> = None;
    for (index, &(from, to)) in spans.iter().enumerate() {
        if from <= start && to >= end {
            let width = to - from;
            if best.is_none_or(|(narrowest, _)| width < narrowest) {
                best = Some((width, index));
            }
        }
    }
    best.map(|(_, index)| index)
}

/// The `limited` kind of an analysis: the most severe limit it reached.
fn limited_kind(nodes: bool, calls: bool, symbols: bool) -> Option<&'static str> {
    if nodes {
        Some("nodes")
    } else if calls {
        Some("calls")
    } else if symbols {
        Some("symbols")
    } else {
        None
    }
}

fn expanded(_path: &str, content: &str, language: &str, limits: &Limits) -> Result<Value> {
    use tree_sitter_language_pack::{ProcessConfig, StructureItem};
    let mut config = ProcessConfig::new(language);
    config.symbols = true;
    config.diagnostics = true;
    config.max_source_bytes = Some(limits.bytes);
    config.parse_timeout_ms = Some(parse_budget_ms(content));
    let processed = tree_sitter_language_pack::process(content, &config)
        .map_err(|e| format!("Source analysis failed: {e}"))?;
    let mut symbols = Vec::new();
    let mut symbols_truncated = false;
    let mut stack: Vec<(&StructureItem, String)> = processed
        .structure
        .iter()
        .rev()
        .map(|s| (s, String::new()))
        .collect();
    while let Some((item, parent)) = stack.pop() {
        let local = item.name.as_deref().unwrap_or("");
        let name = if parent.is_empty() {
            local.to_string()
        } else if local.is_empty() {
            parent
        } else {
            format!("{parent}.{local}")
        };
        if !local.is_empty() {
            if symbols.len() >= limits.symbols {
                symbols_truncated = true;
                break;
            }
            symbols.push(json!({"name":name,"kind":format!("{:?}",item.kind).to_lowercase(),"line":item.span.start_line+1,"end_line":item.span.end_line+1,"byte_start":item.span.start_byte,"byte_end":item.span.end_byte,"signature":item.signature,"basis":"syntax"}));
        }
        for child in item.children.iter().rev() {
            stack.push((child, name.clone()));
        }
    }
    let imports:Vec<_>=processed.imports.iter().map(|i|json!({"module":i.source,"names":i.items,"alias":i.alias,"line":i.span.start_line+1,"level":0,"wildcard":i.is_wildcard})).collect();
    let spans: Vec<(u64, u64)> = symbols
        .iter()
        .map(|s| {
            (
                s["byte_start"].as_u64().unwrap_or(u64::MAX),
                s["byte_end"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    let mut parser = Parser::new();
    parser
        .set_language(
            &tree_sitter_language_pack::get_language(language).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    let tree = parser.parse(content, None).ok_or("Source parse failed")?;
    let mut nodes = vec![tree.root_node()];
    let mut calls = Vec::new();
    let mut visited = 0;
    let mut nodes_stopped = false;
    let mut calls_truncated = false;
    while let Some(node) = nodes.pop() {
        visited += 1;
        if visited > limits.nodes {
            nodes_stopped = true;
            break;
        }
        if [
            "call",
            "call_expression",
            "method_invocation",
            "method_call_expression",
            "function_call_expression",
            "invocation_expression",
        ]
        .contains(&node.kind())
        {
            let function = node
                .child_by_field_name("function")
                .or_else(|| node.child_by_field_name("name"))
                .or_else(|| node.child_by_field_name("method"));
            if let Some(function) = function {
                let callee = value(function, content.as_bytes());
                if callee.len() <= 500 {
                    if calls.len() >= limits.calls {
                        // Nothing else is collected from the walk.
                        calls_truncated = true;
                        break;
                    }
                    let caller =
                        innermost(&spans, node.start_byte() as u64, node.end_byte() as u64)
                            .and_then(|index| symbols[index]["name"].as_str())
                            .unwrap_or("");
                    calls.push(json!({"callee":callee,"caller":caller,"line":node.start_position().row+1,"byte_start":node.start_byte(),"byte_end":node.end_byte(),"basis":"syntactic_call"}));
                }
            }
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    let mut issues = vec!["Compiler type and overload resolution are not performed".to_string()];
    if processed.metrics.error_count > 0 {
        issues.push("Source contains syntax errors".into());
    }
    if symbols_truncated {
        issues.push(format!(
            "Declaration list truncated at {}",
            counted(limits.symbols, "declaration", "declarations")
        ));
    }
    if calls_truncated {
        issues.push(format!(
            "Call list truncated at {}",
            counted(limits.calls, "call", "calls")
        ));
    }
    if nodes_stopped {
        issues.push(format!(
            "Analysis stopped at {}",
            counted(limits.nodes, "syntax node", "syntax nodes")
        ));
    }
    let mut result = json!({"revision":REVISION,"symbols":symbols,"imports":imports,"calls":calls,"links":[],"parser":"tree-sitter-language-pack","language":language,"complete":false,"issues":issues,"metrics":processed.metrics,"capabilities":{"syntax":true,"compiler_resolution":false}});
    if let Some(kind) = limited_kind(nodes_stopped, calls_truncated, symbols_truncated) {
        result["limited"] = json!(kind);
    }
    Ok(result)
}

/// [`analyze_with`] Graphicle Core's default [`Limits`].
pub fn analyze(path: &str, content: &str) -> Result<Value> {
    analyze_with(path, content, &Limits::default())
}

/// The structure of one source file within `limits`. Limits never fail it
/// (see [`Limits`]); an error means the file could not be analysed at all.
pub fn analyze_with(path: &str, content: &str, limits: &Limits) -> Result<Value> {
    if path.len() > MAX_PATH_BYTES {
        return Ok(text_only(
            format!(
                "Path too long to analyse ({} bytes; limit {}); searchable as text only",
                grouped(path.len()),
                grouped(MAX_PATH_BYTES)
            ),
            Some("size"),
            None,
        ));
    }
    if content.len() > limits.bytes {
        return Ok(text_only(
            format!(
                "Too large to analyse ({} bytes; limit {}); searchable as text only",
                grouped(content.len()),
                grouped(limits.bytes)
            ),
            Some("size"),
            None,
        ));
    }
    let ext = Path::new(path)
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("");
    let legacy = match ext {
        "py" => Some("python"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "rs" => Some("rust"),
        _ => None,
    };
    if legacy.is_none()
        && let Some(language) = expanded_language(path)
    {
        let mut result = expanded(path, content, language, limits)?;
        if language == "json"
            && let Some(config) = crate::bindings::configuration(content)
            && (config.get("compilerOptions").is_some() || config.get("extends").is_some())
        {
            result["module_configuration"] = config;
        }
        return Ok(result);
    }
    let language: Option<Language> = legacy
        .map(tree_sitter_language_pack::get_language)
        .transpose()
        .map_err(|e| format!("Bundled parser unavailable: {e}"))?;
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut calls = Vec::new();
    let mut links = Vec::new();
    let mut issues = Vec::new();
    let mut shadowed = Vec::new();
    let (mut symbols_full, mut imports_full, mut links_full) = (false, false, false);
    let (mut nodes_stopped, mut calls_truncated) = (false, false);
    let parser_name = if language.is_some() {
        "tree-sitter-rust"
    } else {
        "text"
    };
    if let Some(language) = language {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|_| "Parser grammar is incompatible")?;
        let tree = parser
            .parse(content, None)
            .ok_or("Source parsing did not complete")?;
        if tree.root_node().has_error() {
            issues.push(
                "Source has syntax errors; extracted relationships may be incomplete".to_owned(),
            );
        }
        let raw = content.as_bytes();
        let mut stack = vec![(tree.root_node(), Vec::<String>::new())];
        let mut visited = 0;
        while let Some((node, scope)) = stack.pop() {
            visited += 1;
            if visited > limits.nodes {
                nodes_stopped = true;
                break;
            }
            let kind = node.kind();
            let line = node.start_position().row + 1;
            let mut child_scope = scope.clone();
            let definition = match kind {
                "function_definition"
                | "function_declaration"
                | "function_item"
                | "method_definition"
                | "function_signature" => Some("function"),
                "class_definition" | "class_declaration" => Some("class"),
                "struct_item" => Some("struct"),
                "trait_item" => Some("trait"),
                "enum_item" | "enum_declaration" => Some("enum"),
                "interface_declaration" => Some("interface"),
                "type_alias_declaration" | "type_item" => Some("type"),
                _ => None,
            };
            if let Some(definition) = definition {
                let name = field(node, "name", raw);
                if !name.is_empty() {
                    child_scope.push(name.to_owned());
                    push_within(
                        &mut symbols,
                        limits.symbols,
                        &mut symbols_full,
                        json!({"name":child_scope.join("."),"kind":definition,
                    "line":line,"end_line":node.end_position().row+1,"byte_start":node.start_byte(),"byte_end":node.end_byte()}),
                    );
                }
            }
            if kind == "variable_declarator"
                && let Some(v) = node.child_by_field_name("value")
                && matches!(v.kind(), "arrow_function" | "function_expression")
            {
                let name = field(node, "name", raw);
                if !name.is_empty() {
                    child_scope.push(name.to_owned());
                    push_within(
                        &mut symbols,
                        limits.symbols,
                        &mut symbols_full,
                        json!({"name":child_scope.join("."),
                            "kind":"function","line":line,"end_line":node.end_position().row+1,"byte_start":node.start_byte(),"byte_end":node.end_byte()}),
                    );
                }
            }
            if let Some(parameters) = node.child_by_field_name("parameters") {
                let mut names = BTreeSet::new();
                pattern_names(parameters, raw, &mut names);
                for name in names {
                    shadowed.push(
                        json!({"name":name,"scope":child_scope.join("."),"kind":"parameter"}),
                    );
                }
            }
            if matches!(
                kind,
                "assignment"
                    | "assignment_expression"
                    | "augmented_assignment"
                    | "augmented_assignment_expression"
                    | "variable_declarator"
                    | "let_declaration"
                    | "for_statement"
            ) {
                let target = node
                    .child_by_field_name("left")
                    .or_else(|| node.child_by_field_name("name"))
                    .or_else(|| node.child_by_field_name("pattern"));
                let callable = node
                    .child_by_field_name("value")
                    .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"));
                if !callable && let Some(target) = target {
                    let mut names = BTreeSet::new();
                    assignment_names(target, raw, &mut names);
                    for name in names {
                        shadowed
                            .push(json!({"name":name,"scope":scope.join("."),"kind":"assignment"}));
                    }
                }
            }
            if ext == "py" && kind == "import_statement" {
                let mut cursor = node.walk();
                for item in node.named_children(&mut cursor) {
                    let module = if item.kind() == "aliased_import" {
                        field(item, "name", raw)
                    } else {
                        value(item, raw)
                    };
                    if !module.is_empty() {
                        push_within(
                            &mut imports,
                            limits.symbols,
                            &mut imports_full,
                            json!({"module":module,"level":0,"names":[],"line":line,
                        "alias":field(item,"alias",raw),"scope":scope.join(".")}),
                        );
                    }
                }
            }
            if ext == "py" && kind == "import_from_statement" {
                let module_node = node.child_by_field_name("module_name");
                let original = module_node.map(|n| value(n, raw)).unwrap_or("");
                let level = original.chars().take_while(|x| *x == '.').count();
                let mut names = Vec::new();
                let mut bindings = BTreeMap::new();
                let mut cursor = node.walk();
                for item in node.named_children(&mut cursor) {
                    if Some(item) == module_node {
                        continue;
                    }
                    let name = if item.kind() == "aliased_import" {
                        field(item, "name", raw)
                    } else {
                        value(item, raw)
                    };
                    if name == "*" {
                        issues.push(format!(
                            "Wildcard import at line {line}; symbol resolution is incomplete"
                        ));
                    }
                    if !name.is_empty() {
                        names.push(name.to_owned());
                        let alias = field(item, "alias", raw);
                        bindings.insert(
                            if alias.is_empty() { name } else { alias }.to_owned(),
                            name.to_owned(),
                        );
                    }
                }
                push_within(
                    &mut imports,
                    limits.symbols,
                    &mut imports_full,
                    json!({"module":original.trim_start_matches('.'),"level":level,"names":names,"line":line,"bindings":bindings,"scope":scope.join(".")}),
                );
            }
            if matches!(kind, "import_statement" | "export_statement") && ext != "py" {
                let source = field(node, "source", raw);
                if !source.is_empty() {
                    if source.contains('\\') {
                        issues.push(format!("Escaped module specifier at line {line}"));
                    } else {
                        let mut bindings = BTreeMap::new();
                        let mut namespace = String::new();
                        let type_only = direct_token(node, "type");
                        let mut parts = vec![node];
                        while let Some(part) = parts.pop() {
                            if matches!(part.kind(), "import_specifier" | "export_specifier") {
                                if type_only || direct_token(part, "type") {
                                    continue;
                                }
                                let name = field(part, "name", raw);
                                let alias = field(part, "alias", raw);
                                if !name.is_empty() {
                                    bindings.insert(
                                        if alias.is_empty() { name } else { alias }.to_string(),
                                        name.to_string(),
                                    );
                                }
                            } else if part.kind() == "namespace_import"
                                && let Some(name) = part.named_child(0)
                            {
                                namespace = value(name, raw).to_string();
                            }
                            let mut cursor = part.walk();
                            parts.extend(part.named_children(&mut cursor));
                        }
                        push_within(
                            &mut imports,
                            limits.symbols,
                            &mut imports_full,
                            json!({"module":source.trim_matches(['\'','"']),"level":0,"names":[],"line":line,"bindings":bindings,"namespace":namespace,"scope":scope.join("."),"reexport":kind=="export_statement","type_only":type_only}),
                        );
                    }
                }
            }
            if ext == "rs" && matches!(kind, "use_declaration" | "mod_item") {
                if kind == "mod_item" && node.child_by_field_name("body").is_none() {
                    push_within(
                        &mut imports,
                        limits.symbols,
                        &mut imports_full,
                        json!({"module":field(node,"name",raw),"level":0,"names":[],"line":line,"namespace":field(node,"name",raw),"scope":scope.join(".")}),
                    );
                }
                if kind == "use_declaration" {
                    let argument = field(node, "argument", raw);
                    if argument.contains('{') || argument.contains('*') {
                        issues.push(format!(
                            "Rust grouped/glob use at line {line}; explicit module navigation only"
                        ));
                    }
                    let module = argument
                        .split("::{")
                        .next()
                        .unwrap_or(argument)
                        .split(" as ")
                        .next()
                        .unwrap_or(argument);
                    let mut bindings = BTreeMap::new();
                    if !argument.contains(['{', '*'])
                        && let Some((_, name)) = module.rsplit_once("::")
                    {
                        let local = argument
                            .split_once(" as ")
                            .map(|(_, name)| name)
                            .unwrap_or(name);
                        bindings.insert(local.to_string(), name.to_string());
                    }
                    push_within(
                        &mut imports,
                        limits.symbols,
                        &mut imports_full,
                        json!({"module":module,"level":0,"names":[],"line":line,"bindings":bindings,"scope":scope.join(".")}),
                    );
                }
            }
            if matches!(kind, "call" | "call_expression") {
                let function = field(node, "function", raw);
                if !function.is_empty() && function.len() < 500 {
                    if calls.len() < limits.calls {
                        calls.push(json!({"caller":scope.join("."),"callee":function,"line":line,"byte_start":node.start_byte(),"byte_end":node.end_byte(),"basis":"syntactic_call"}));
                    } else {
                        calls_truncated = true;
                    }
                    if matches!(function, "require" | "import") && ext != "py" {
                        let argument = field(node, "arguments", raw);
                        let arg = argument
                            .trim_start_matches('(')
                            .trim_end_matches(')')
                            .trim();
                        if arg.len() > 1
                            && (arg.starts_with('"') || arg.starts_with('\''))
                            && !arg.contains('\\')
                            && !arg.contains(',')
                        {
                            push_within(
                                &mut imports,
                                limits.symbols,
                                &mut imports_full,
                                json!({"module":arg.trim_matches(['\'','"']),"level":0,"names":[],"line":line}),
                            );
                        } else {
                            issues.push(format!("Dynamic module reference at line {line}"));
                        }
                    }
                }
            }
            if child_scope.len() > 100 {
                issues.push("Declaration nesting limit reached".into());
                continue;
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push((child, child_scope.clone()));
            }
        }
    } else if !matches!(
        ext,
        "md" | "mdx"
            | "txt"
            | "rst"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "graphql"
            | "gql"
            | "sql"
            | ""
    ) {
        issues.push(
            "Language is searchable as text; native structural extraction is unavailable".into(),
        );
    }
    if matches!(ext, "md" | "mdx" | "rst") {
        static LINK: OnceLock<Regex> = OnceLock::new();
        let re = LINK.get_or_init(|| Regex::new(r"\[[^\]\n]+\]\(([^\s)]+)\)").unwrap());
        for (number, line) in content.lines().enumerate() {
            if line.starts_with('#') {
                push_within(
                    &mut symbols,
                    limits.symbols,
                    &mut symbols_full,
                    json!({"name":line.trim_start_matches(['#',' ']).chars().take(160).collect::<String>(),"kind":"heading","line":number+1,"end_line":number+1}),
                );
            }
            for m in re.captures_iter(line) {
                push_within(
                    &mut links,
                    limits.symbols,
                    &mut links_full,
                    json!({"module":m[1].to_owned(),"line":number+1}),
                );
            }
        }
    }
    for (full, list, one, many) in [
        (symbols_full, "Declaration", "declaration", "declarations"),
        (imports_full, "Import", "import", "imports"),
        (links_full, "Link", "link", "links"),
    ] {
        if full {
            issues.push(format!(
                "{list} list truncated at {}",
                counted(limits.symbols, one, many)
            ));
        }
    }
    if calls_truncated {
        issues.push(format!(
            "Call list truncated at {}",
            counted(limits.calls, "call", "calls")
        ));
    }
    if nodes_stopped {
        issues.push(format!(
            "Analysis stopped at {}",
            counted(limits.nodes, "syntax node", "syntax nodes")
        ));
    }
    issues.sort();
    issues.dedup();
    let mut result = json!({"revision":REVISION,"symbols":symbols,"imports":imports,"calls":calls,"links":links,"shadowed_bindings":shadowed,
        "parser":parser_name,"complete":issues.is_empty(),"issues":issues});
    if let Some(kind) = limited_kind(
        nodes_stopped,
        calls_truncated,
        symbols_full || imports_full || links_full,
    ) {
        result["limited"] = json!(kind);
    }
    Ok(result)
}

/// Appends `item` to `list` unless it holds `cap` items already, which sets
/// `full`.
fn push_within(list: &mut Vec<Value>, cap: usize, full: &mut bool, item: Value) {
    if list.len() < cap {
        list.push(item);
    } else {
        *full = true;
    }
}

pub(crate) fn normalize(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(p) => parts.push(p.to_str()?.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.join("/"))
}
fn get_items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Resolve only unambiguous paths from the caller's authorized manifest.
pub fn graph(analyses: &Value) -> Result<Value> {
    let analyses = analyses
        .as_object()
        .ok_or("Expected analyses keyed by source path")?;
    if analyses.len() > GRAPH_FILE_LIMIT {
        return Err("Graph file limit exceeded".into());
    }
    let paths: BTreeSet<_> = analyses.keys().cloned().collect();
    let mut modules: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in &paths {
        if let Some(name) = path.strip_suffix(".py") {
            // importlib's FileFinder tries `<name>/__init__.py` before
            // `<name>.py`, so a regular package shadows a same-named sibling
            // module file (Graphify #3777).
            if paths.contains(&format!("{name}/__init__.py")) {
                continue;
            }
            let name = name.replace('/', ".");
            let name = name.strip_suffix(".__init__").unwrap_or(&name).to_owned();
            for alias in [
                &name,
                name.strip_prefix("src.").unwrap_or(&name),
                name.strip_prefix("lib.").unwrap_or(&name),
            ] {
                modules
                    .entry(alias.to_owned())
                    .or_default()
                    .insert(path.clone());
            }
        }
    }
    let mut edges = Vec::new();
    let mut unresolved = Vec::new();
    let mut seen = BTreeSet::new();
    for (path, analysis) in analyses {
        for (field, relation) in [("imports", "imports"), ("links", "document_link")] {
            for item in get_items(analysis, field) {
                let module = item.get("module").and_then(Value::as_str).unwrap_or("");
                let mut targets = BTreeSet::new();
                // `(imported name, file)` for `from pkg import sub` submodules.
                let mut submodules = Vec::new();
                let mut ambiguous_submodule = false;
                if relation == "imports" && path.ends_with(".py") {
                    let level = item.get("level").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let mut absolute = module.to_owned();
                    if level > 0 {
                        let mut dir: Vec<_> = path.split('/').collect();
                        dir.pop();
                        if level <= dir.len() + 1 {
                            for _ in 1..level {
                                dir.pop();
                            }
                            if !module.is_empty() {
                                dir.push(module);
                            }
                            absolute = dir.join(".");
                        } else {
                            absolute = "<outside-manifest>".into();
                        }
                    }
                    let base = modules.get(&absolute);
                    if let Some(found) = base {
                        targets.extend(found.iter().cloned());
                    }
                    // `from X import name` also imports the submodule `X.name`
                    // when X is a namespace package (no module file) or a
                    // regular package whose `__init__` leaves `name` unbound.
                    let init = base
                        .filter(|found| found.len() == 1)
                        .and_then(|found| found.first())
                        .filter(|p| Path::new(p).file_name().is_some_and(|f| f == "__init__.py"));
                    if base.is_none() || init.is_some() {
                        for name in get_items(item, "names").iter().filter_map(Value::as_str) {
                            if init.is_some_and(|init| {
                                !crate::bindings::leaves_submodule(&analyses[init], name)
                            }) {
                                continue;
                            }
                            let child = if absolute.is_empty() {
                                name.to_owned()
                            } else {
                                format!("{absolute}.{name}")
                            };
                            if let Some(found) = modules.get(&child) {
                                if found.len() == 1 {
                                    submodules
                                        .extend(found.iter().map(|p| (name.to_owned(), p.clone())));
                                } else {
                                    ambiguous_submodule = true;
                                }
                            }
                        }
                    }
                } else if path.ends_with(".rs") && relation == "imports" {
                    let mut base = module.replace("::", "/");
                    if let Some(rest) = base.strip_prefix("crate/") {
                        base = format!("src/{rest}");
                    } else {
                        let parent = Path::new(path).parent().unwrap_or(Path::new(""));
                        base = parent.join(base).to_string_lossy().into_owned();
                    }
                    if let Some(base) = normalize(&base) {
                        for p in [format!("{base}.rs"), format!("{base}/mod.rs")] {
                            if paths.contains(&p) {
                                targets.insert(p);
                            }
                        }
                        if targets.is_empty()
                            && item["bindings"].as_object().is_some_and(|v| !v.is_empty())
                            && let Some((base, _)) = base.rsplit_once('/')
                        {
                            for p in [format!("{base}.rs"), format!("{base}/mod.rs")] {
                                if paths.contains(&p) {
                                    targets.insert(p);
                                }
                            }
                        }
                    }
                } else {
                    let local = module.split(['#', '?']).next().unwrap_or("");
                    if (relation == "document_link"
                        && !local.is_empty()
                        && !local.contains(':')
                        && !local.starts_with('/'))
                        || local.starts_with('.')
                    {
                        let parent = Path::new(path)
                            .parent()
                            .and_then(Path::to_str)
                            .unwrap_or("");
                        if let Some(base) =
                            normalize(&Path::new(parent).join(local).to_string_lossy())
                        {
                            let mut options = vec![base.clone()];
                            if relation == "imports" && !paths.contains(&base) {
                                let mut roots = vec![base.clone()];
                                for suffix in [".js", ".jsx", ".mjs", ".cjs"] {
                                    if let Some(root) = base.strip_suffix(suffix) {
                                        roots.push(root.to_owned());
                                    }
                                }
                                for root in roots {
                                    for suffix in
                                        ["js", "ts", "tsx", "jsx", "mjs", "cjs", "mts", "cts"]
                                    {
                                        options.push(format!("{root}.{suffix}"));
                                        options.push(format!("{root}/index.{suffix}"));
                                    }
                                }
                            }
                            for p in options {
                                if paths.contains(&p) {
                                    targets.insert(p);
                                }
                            }
                        }
                    }
                }
                if relation == "imports"
                    && targets.is_empty()
                    && !path.ends_with(".py")
                    && !path.ends_with(".rs")
                    && !module.starts_with('.')
                {
                    targets.extend(crate::bindings::configured_targets(
                        path, module, analyses, &paths,
                    ));
                    if targets.is_empty() {
                        targets.extend(crate::bindings::convention_alias_targets(
                            path, module, &paths,
                        ));
                    }
                }
                let line = item.get("line").and_then(Value::as_u64).unwrap_or(1);
                if targets.len() == 1 {
                    let target = targets.into_iter().next().unwrap();
                    if target != *path
                        && seen.insert((path.clone(), target.clone(), line, relation.to_owned()))
                    {
                        edges.push(json!({"source":path,"target":target,"line":line,"kind":relation,"basis":"static_source_resolution","module":module}));
                    }
                } else if targets.len() > 1 || submodules.is_empty() {
                    let reason = if targets.is_empty() && !ambiguous_submodule {
                        "external_or_unresolved"
                    } else {
                        "ambiguous"
                    };
                    unresolved.push(json!({"source":path,"line":item.get("line"),"module":module,"reason":reason}));
                }
                for (name, target) in submodules {
                    if target != *path
                        && seen.insert((path.clone(), target.clone(), line, relation.to_owned()))
                    {
                        edges.push(json!({"source":path,"target":target,"line":line,"kind":relation,"basis":"static_source_resolution","module":module,"submodule":name}));
                    }
                }
            }
        }
    }
    let (symbol_calls, unresolved_calls) = crate::bindings::resolve_calls(analyses, &edges);
    let incomplete: Vec<_> = analyses
        .iter()
        .filter(|(_, v)| v.get("complete").and_then(Value::as_bool) != Some(true))
        .map(|(p, v)| json!({"path":p,"issues":v.get("issues")}))
        .collect();
    let incomplete_files: Vec<_> = incomplete
        .iter()
        .filter_map(|x| x.get("path"))
        .cloned()
        .collect();
    let symbol_count: usize = analyses
        .values()
        .map(|a| get_items(a, "symbols").len())
        .sum();
    Ok(
        json!({"edges":edges,"unresolved":unresolved,"symbol_calls":symbol_calls,"unresolved_calls":unresolved_calls,"coverage":{"files":analyses.len(),"symbols":symbol_count,"incomplete_files":incomplete_files,"incomplete":incomplete},
        "scope":"Static source relationships in the authorized manifest; not a runtime call graph","revision":REVISION}),
    )
}
