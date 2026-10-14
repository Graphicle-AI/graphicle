// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/resolution.py
//! Parser-derived import, alias, export and type facts.
use super::engine_parser::Parsed;
use super::{
    base::{file_stem, make_id, read_text},
    engine_config::probe_python_module_candidate,
    engine_types::KindName,
    engine_types::{children, first_child, named},
    models::*,
    parse_cache,
    resolution::resolved,
    resolution_js as js,
    resolution_paths::{is_python_package_dir, probe_declared_python_module_candidate},
};
use crate::rcfile::effective_suffix;
use rustc_hash::FxHashSet;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tree_sitter::{Language, Node};
/// `make_id` forms of absolute Python module names that more than one scanned
/// file answers to (#3729). Imports of these modules stay unresolved rather
/// than binding to an arbitrary same-named package.
pub type AmbiguousModules = FxHashSet<String>;
/// `_PYTHON_STDLIB_MODULE_NAMES`: the union of Python 3.10 and 3.13
/// `sys.stdlib_module_names`, frozen (sorted) so extraction does not depend on
/// the host interpreter. A bare `import logging` means the standard library, so
/// a loose local `logging.py` (or a namespace dir) never captures it (#4261);
/// a deliberate package shadow (`logging/__init__.py`) still does.
#[rustfmt::skip]
pub const PYTHON_STDLIB_MODULE_NAMES: &[&str] = &[
    "abc", "aifc", "annotationlib", "antigravity", "argparse", "array", "ast", "asynchat",
    "asyncio", "asyncore", "atexit", "audioop", "base64", "bdb", "binascii", "bisect", "builtins",
    "bz2", "cProfile", "calendar", "cgi", "cgitb", "chunk", "cmath", "cmd", "code", "codecs",
    "codeop", "collections", "colorsys", "compileall", "compression", "concurrent",
    "configparser", "contextlib", "contextvars", "copy", "copyreg", "crypt", "csv", "ctypes",
    "curses", "dataclasses", "datetime", "dbm", "decimal", "difflib", "dis", "doctest", "email",
    "encodings", "ensurepip", "enum", "errno", "faulthandler", "fcntl", "filecmp", "fileinput",
    "fnmatch", "fractions", "ftplib", "functools", "gc", "genericpath", "getopt", "getpass",
    "gettext", "glob", "graphlib", "grp", "gzip", "hashlib", "heapq", "hmac", "html", "http",
    "idlelib", "imaplib", "imghdr", "imp", "importlib", "inspect", "io", "ipaddress", "itertools",
    "json", "keyword", "lib2to3", "linecache", "locale", "logging", "lzma", "mailbox", "mailcap",
    "marshal", "math", "mimetypes", "mmap", "modulefinder", "msilib", "msvcrt", "multiprocessing",
    "netrc", "nis", "nntplib", "nt", "ntpath", "nturl2path", "numbers", "opcode", "operator",
    "optparse", "os", "ossaudiodev", "pathlib", "pdb", "pickle", "pickletools", "pipes",
    "pkgutil", "platform", "plistlib", "poplib", "posix", "posixpath", "pprint", "profile",
    "pstats", "pty", "pwd", "py_compile", "pyclbr", "pydoc", "pydoc_data", "pyexpat", "queue",
    "quopri", "random", "re", "readline", "reprlib", "resource", "rlcompleter", "runpy", "sched",
    "secrets", "select", "selectors", "shelve", "shlex", "shutil", "signal", "site", "smtpd",
    "smtplib", "sndhdr", "socket", "socketserver", "spwd", "sqlite3", "sre_compile",
    "sre_constants", "sre_parse", "ssl", "stat", "statistics", "string", "stringprep", "struct",
    "subprocess", "sunau", "symtable", "sys", "sysconfig", "syslog", "tabnanny", "tarfile",
    "telnetlib", "tempfile", "termios", "textwrap", "this", "threading", "time", "timeit",
    "tkinter", "token", "tokenize", "tomllib", "trace", "traceback", "tracemalloc", "tty",
    "turtle", "turtledemo", "types", "typing", "unicodedata", "unittest", "urllib", "uu", "uuid",
    "venv", "warnings", "wave", "weakref", "webbrowser", "winreg", "winsound", "wsgiref",
    "xdrlib", "xml", "xmlrpc", "zipapp", "zipfile", "zipimport", "zlib", "zoneinfo",
];
/// Whether `name` is in [`PYTHON_STDLIB_MODULE_NAMES`].
pub fn is_python_stdlib_module(name: &str) -> bool {
    PYTHON_STDLIB_MODULE_NAMES.binary_search(&name).is_ok()
}
/// Every node under `root` in pre-order, via one cursor instead of a child
/// vector per node.
pub fn walk(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::with_capacity(root.descendant_count());
    let mut cursor = root.walk();
    loop {
        out.push(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
}
/// Parse `path`; masked sources (Vue) bypass the run cache because their bytes
/// differ from the file on disk.
fn parse(
    path: &Path,
    tag: &'static str,
    language: Language,
    source: Option<Vec<u8>>,
) -> Option<parse_cache::Tree> {
    match source {
        Some(source) => parse_cache::parse_bytes(language, source).map(Arc::new),
        None => parse_cache::parse(path, tag, language),
    }
}
/// The grammar [`parse_js_tree`] parses `path` with, when it parses the file's
/// own bytes (`None` for Vue, whose non-script parts are masked first).
pub fn js_parse_tag(path: &Path) -> Option<&'static str> {
    Some(match effective_suffix(path).as_str() {
        ".vue" => return None,
        ".tsx" => "tsx",
        ".ts" | ".mts" | ".cts" => "typescript",
        _ => "javascript",
    })
}
/// `_parse_js_tree`: the grammar follows the file's effective suffix (a
/// project-declared language, #2961).
pub fn parse_js_tree(path: &Path) -> Option<parse_cache::Tree> {
    let suffix = effective_suffix(path);
    let ext = suffix.strip_prefix('.').unwrap_or("");
    let (mut source, mut vue_lang) = (None, None);
    if ext == "vue" {
        // `_read_source_text(path, warn=False)` (0.9.78); the other grammars
        // parse through the run cache, which decodes the same way.
        let (s, l) = js::vue_mask_non_script(&super::base::read_source_text(path, false).ok()?);
        source = Some(s.into_bytes());
        vue_lang = l;
    }
    let (tag, lang) = if ext == "tsx" {
        ("tsx", tree_sitter_typescript::LANGUAGE_TSX.into())
    } else if ["ts", "mts", "cts"].contains(&ext)
        || (ext == "vue" && !matches!(vue_lang.as_deref(), Some("js" | "jsx")))
    {
        (
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        )
    } else {
        ("javascript", tree_sitter_javascript::LANGUAGE.into())
    };
    parse(path, tag, lang, source)
}
pub fn parse_python_tree(path: &Path) -> Option<parse_cache::Tree> {
    parse(path, "python", tree_sitter_python::LANGUAGE.into(), None)
}
/// `_js_module_specifier`: the statement's `source` field; an import may
/// also name it as a bare string child, but an `export` only with `from`
/// (`export default "x"` exports a string, it names no module).
pub fn module_specifier(n: Node<'_>, s: &[u8]) -> Option<String> {
    n.child_by_field_name("source")
        .or_else(|| {
            if n.kind_name() == "export_statement" {
                None
            } else {
                first_child(n, &["string"])
            }
        })
        .map(|n| {
            read_text(n, s)
                .trim()
                .trim_matches(['\'', '"', '`'])
                .to_string()
        })
        .filter(|s| !s.is_empty())
}
pub fn named_specifiers(n: Node<'_>, s: &[u8], kind: &str) -> Vec<(String, String)> {
    walk(n)
        .into_iter()
        .filter(|c| c.kind_name() == kind)
        .filter_map(|n| {
            let name = read_text(n.child_by_field_name("name")?, s);
            let alias = n
                .child_by_field_name("alias")
                .map(|n| read_text(n, s))
                .unwrap_or_else(|| name.clone());
            (!name.is_empty() && !alias.is_empty()).then_some((name, alias))
        })
        .collect()
}
fn lexical_aliases(n: Node<'_>, s: &[u8]) -> Vec<(String, String)> {
    if n.kind_name() != "lexical_declaration" {
        return vec![];
    }
    children(n)
        .into_iter()
        .filter(|n| n.kind_name() == "variable_declarator")
        .filter_map(|n| {
            let name = n.child_by_field_name("name")?;
            let value = n
                .child_by_field_name("value")
                .filter(|v| matches!(v.kind_name(), "identifier" | "type_identifier"))?;
            Some((read_text(name, s), read_text(value, s)))
        })
        .collect()
}
fn exported_names(n: Node<'_>, s: &[u8]) -> Vec<String> {
    let Some(d) = n.child_by_field_name("declaration") else {
        return vec![];
    };
    if matches!(
        d.kind_name(),
        "lexical_declaration" | "variable_declaration"
    ) {
        let mut out = lexical_aliases(d, s)
            .into_iter()
            .map(|(a, _)| a)
            .collect::<Vec<_>>();
        for name in named(d)
            .into_iter()
            .filter(|n| n.kind_name() == "variable_declarator")
            .filter_map(|n| n.child_by_field_name("name"))
            .filter(|n| n.kind_name() == "identifier")
        {
            let name = read_text(name, s);
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    } else if [
        "class_declaration",
        "abstract_class_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "function_declaration",
    ]
    .contains(&d.kind_name())
    {
        d.child_by_field_name("name")
            .map(|n| vec![read_text(n, s)])
            .unwrap_or_default()
    } else {
        vec![]
    }
}
pub fn top_bodies<'t>(
    path: &Path,
    root: Node<'t>,
    s: &[u8],
    python: bool,
) -> Vec<(String, Node<'t>)> {
    let mut top = vec![];
    for n in children(root) {
        if !python && n.kind_name() == "export_statement" && first_child(n, &["string"]).is_none() {
            top.extend(children(n).into_iter().filter(|n| {
                matches!(
                    n.kind_name(),
                    "function_declaration" | "lexical_declaration"
                )
            }));
        } else {
            top.push(n);
        }
    }
    let stem = file_stem(path);
    let mut out = vec![];
    for n in top {
        if n.kind_name()
            == if python {
                "function_definition"
            } else {
                "function_declaration"
            }
        {
            if let (Some(name), Some(body)) =
                (n.child_by_field_name("name"), n.child_by_field_name("body"))
            {
                out.push((make_id([stem.as_str(), read_text(name, s).as_str()]), body));
            }
        } else if !python && n.kind_name() == "lexical_declaration" {
            for d in children(n)
                .into_iter()
                .filter(|n| n.kind_name() == "variable_declarator")
            {
                if let (Some(name), Some(value)) = (
                    d.child_by_field_name("name"),
                    d.child_by_field_name("value")
                        .filter(|v| v.kind_name() == "arrow_function"),
                ) {
                    out.push((make_id([stem.as_str(), read_text(name, s).as_str()]), value));
                }
            }
        }
    }
    out
}
pub fn call_identifier(n: Node<'_>, s: &[u8], python: bool) -> Option<String> {
    if n.kind_name() != if python { "call" } else { "call_expression" } {
        return None;
    }
    let f = n.child_by_field_name("function").or_else(|| {
        if python {
            None
        } else {
            named(n).into_iter().next()
        }
    })?;
    if f.kind_name() == "identifier" || (!python && f.kind_name() == "type_identifier") {
        Some(read_text(f, s))
    } else {
        None
    }
}
pub fn ts_collect_type_refs(
    n: Option<Node<'_>>,
    s: &[u8],
    generic: bool,
    out: &mut Vec<(String, String)>,
) {
    let Some(n) = n else { return };
    let primitive = |name: &str| {
        [
            "string",
            "number",
            "boolean",
            "any",
            "unknown",
            "void",
            "never",
            "object",
            "null",
            "undefined",
            "bigint",
            "symbol",
            "this",
        ]
        .contains(&name)
    };
    let push = |out: &mut Vec<(String, String)>, raw: String, generic: bool| {
        let name = raw.rsplit('.').next().unwrap_or("");
        if !name.is_empty() && !primitive(name) {
            out.push((
                name.into(),
                if generic { "generic_arg" } else { "type" }.into(),
            ));
        }
    };
    if matches!(
        n.kind_name(),
        "identifier" | "type_identifier" | "nested_type_identifier"
    ) {
        push(out, read_text(n, s), generic);
        return;
    }
    if n.kind_name() == "generic_type" {
        if let Some(name) = n
            .child_by_field_name("name")
            .or_else(|| first_child(n, &["type_identifier", "nested_type_identifier"]))
        {
            push(out, read_text(name, s), generic);
        }
        if let Some(args) = first_child(n, &["type_arguments"]) {
            for a in named(args) {
                ts_collect_type_refs(Some(a), s, true, out);
            }
        }
        return;
    }
    if n.is_named() {
        for c in named(n) {
            ts_collect_type_refs(Some(c), s, generic, out);
        }
    }
}
pub fn heritage_entries(n: Node<'_>, s: &[u8]) -> Vec<String> {
    named(n)
        .into_iter()
        .filter_map(|n| match n.kind_name() {
            "identifier" | "type_identifier" | "nested_type_identifier" => Some(read_text(n, s)),
            "generic_type" => n
                .child_by_field_name("name")
                .or_else(|| {
                    first_child(
                        n,
                        &["type_identifier", "nested_type_identifier", "identifier"],
                    )
                })
                .map(|n| read_text(n, s)),
            _ => None,
        })
        .map(|s| s.rsplit('.').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
fn use_refs(
    path: &Path,
    id: &str,
    n: Option<Node<'_>>,
    s: &[u8],
    line: i64,
    context: &str,
    f: &mut SymbolResolutionFacts,
) {
    let mut refs = vec![];
    ts_collect_type_refs(n, s, false, &mut refs);
    for (name, role) in refs {
        f.uses.push(SymbolUseFact::new(
            path.to_path_buf(),
            id.into(),
            name,
            "references".into(),
            if role == "generic_arg" {
                "generic_arg"
            } else {
                context
            }
            .into(),
            line,
        ));
    }
}
pub fn ts_class_members(
    n: Node<'_>,
    s: &[u8],
    path: &Path,
    id: &str,
    f: &mut SymbolResolutionFacts,
) {
    for child in children(n) {
        let mut clauses = vec![];
        if child.kind_name() == "class_heritage" {
            clauses = children(child)
                .into_iter()
                .filter(|c| matches!(c.kind_name(), "extends_clause" | "implements_clause"))
                .map(|c| {
                    (
                        c,
                        if c.kind_name() == "extends_clause" {
                            "inherits"
                        } else {
                            "implements"
                        },
                    )
                })
                .collect();
            if clauses.is_empty() {
                clauses.push((child, "inherits"));
            }
        } else if child.kind_name() == "extends_type_clause" {
            clauses.push((child, "inherits"));
        }
        for (c, rel) in clauses {
            for name in heritage_entries(c, s) {
                f.uses.push(SymbolUseFact::new(
                    path.to_path_buf(),
                    id.into(),
                    name,
                    rel.into(),
                    "type".into(),
                    (c.start_position().row + 1) as i64,
                ));
            }
        }
    }
    if let Some(body) = n.child_by_field_name("body") {
        for m in children(body) {
            let line = (m.start_position().row + 1) as i64;
            if [
                "method_definition",
                "method_signature",
                "abstract_method_signature",
            ]
            .contains(&m.kind_name())
            {
                let Some(name) = m.child_by_field_name("name") else {
                    continue;
                };
                let mid = make_id([id, read_text(name, s).as_str()]);
                if let Some(params) = m.child_by_field_name("parameters") {
                    for p in children(params).into_iter().filter(|p| {
                        matches!(p.kind_name(), "required_parameter" | "optional_parameter")
                    }) {
                        use_refs(
                            path,
                            &mid,
                            p.child_by_field_name("type"),
                            s,
                            line,
                            "parameter_type",
                            f,
                        );
                    }
                }
                use_refs(
                    path,
                    &mid,
                    m.child_by_field_name("return_type"),
                    s,
                    line,
                    "return_type",
                    f,
                );
            } else if matches!(
                m.kind_name(),
                "public_field_definition" | "property_signature"
            ) {
                use_refs(
                    path,
                    id,
                    first_child(m, &["type_annotation"]),
                    s,
                    line,
                    "field",
                    f,
                );
            }
        }
    }
}
/// One JS/TS file's facts, bucketed by the pass that produced them, so files
/// can be processed independently and still be appended pass by pass in path
/// order, exactly as four sequential passes over every file append them.
#[derive(Default)]
pub struct JsFileFacts {
    /// Pass 1: declarations, imports and lexical aliases.
    first: SymbolResolutionFacts,
    /// Pass 2: namespace, star and named exports.
    exports: SymbolResolutionFacts,
    /// Pass 3: calls from top-level bodies.
    calls: Vec<SymbolUseFact>,
    /// Pass 4: class and interface member type references.
    members: SymbolResolutionFacts,
}
/// `nodes` is [`walk`] of the file's tree: one pre-order walk serves every
/// whole-file pass below.
fn js_declarations_imports(
    path: &Path,
    parsed: &Parsed,
    nodes: &[Node<'_>],
    f: &mut SymbolResolutionFacts,
) {
    let s = &parsed.source;
    for &n in nodes {
        let line = (n.start_position().row + 1) as i64;
        if n.kind_name() == "export_statement" {
            for name in exported_names(n, s) {
                f.declarations
                    .push(SymbolDeclarationFact::new(path.to_path_buf(), name, line));
            }
        }
        if n.kind_name() == "import_statement"
            && let Some(raw) = module_specifier(n, s)
            && let Some(target) =
                js::resolve_js_module_path(&raw, path.parent()).map(|p| resolved(&p))
        {
            for (imported, local) in named_specifiers(n, s, "import_specifier") {
                f.imports.push(SymbolImportFact::new(
                    path.to_path_buf(),
                    local,
                    target.clone(),
                    imported,
                    line,
                ));
            }
            if let Some(name) =
                first_child(n, &["import_clause"]).and_then(|c| first_child(c, &["identifier"]))
            {
                f.imports.push(SymbolImportFact::new(
                    path.to_path_buf(),
                    read_text(name, s),
                    target,
                    "default".into(),
                    line,
                ));
            }
        }
    }
    for &n in nodes {
        for (alias, target) in lexical_aliases(n, s) {
            f.aliases.push(SymbolAliasFact::new(
                path.to_path_buf(),
                alias,
                target,
                (n.start_position().row + 1) as i64,
            ));
        }
    }
}
fn js_exports(path: &Path, parsed: &Parsed, nodes: &[Node<'_>], f: &mut SymbolResolutionFacts) {
    let s = &parsed.source;
    for &n in nodes.iter().filter(|n| n.kind_name() == "export_statement") {
        let line = (n.start_position().row + 1) as i64;
        let clause = first_child(n, &["export_clause"]);
        let type_only = children(n)
            .iter()
            .any(|c| c.kind_name() == "type" && !c.is_named());
        if let Some(raw) = module_specifier(n, s) {
            let Some(target) =
                js::resolve_js_module_path(&raw, path.parent()).map(|p| resolved(&p))
            else {
                continue;
            };
            let namespace =
                first_child(n, &["namespace_export"]).and_then(|n| first_child(n, &["identifier"]));
            if let Some(name) = namespace {
                f.namespace_exports.push(NamespaceExportFact {
                    file_path: path.to_path_buf(),
                    exported_name: read_text(name, s),
                    target_path: target.clone(),
                    line,
                    type_only,
                });
            } else if children(n).iter().any(|n| n.kind_name() == "*") {
                f.star_exports.push(StarExportFact {
                    file_path: path.to_path_buf(),
                    target_path: target.clone(),
                    line,
                    type_only,
                });
            }
            if let Some(clause) = clause {
                for (original, exported) in named_specifiers(clause, s, "export_specifier") {
                    f.exports.push(SymbolExportFact::reexport(
                        path.to_path_buf(),
                        exported,
                        line,
                        None,
                        Some(target.clone()),
                        Some(original),
                        type_only,
                    ));
                }
            }
            continue;
        }
        if let Some(clause) = clause {
            for (local, exported) in named_specifiers(clause, s, "export_specifier") {
                f.exports.push(SymbolExportFact::reexport(
                    path.to_path_buf(),
                    exported,
                    line,
                    Some(local),
                    None,
                    None,
                    false,
                ));
            }
            continue;
        }
        for name in exported_names(n, s) {
            f.exports.push(SymbolExportFact::reexport(
                path.to_path_buf(),
                name.clone(),
                line,
                Some(name),
                None,
                None,
                false,
            ));
        }
        if children(n).iter().any(|c| c.kind_name() == "default") {
            let name = n
                .child_by_field_name("declaration")
                .and_then(|d| d.child_by_field_name("name"))
                .or_else(|| {
                    n.child_by_field_name("value")
                        .filter(|v| v.kind_name() == "identifier")
                });
            if let Some(name) = name {
                f.exports.push(SymbolExportFact::reexport(
                    path.to_path_buf(),
                    "default".into(),
                    line,
                    Some(read_text(name, s)),
                    None,
                    None,
                    false,
                ));
            }
        }
    }
}
fn js_calls(path: &Path, parsed: &Parsed, uses: &mut Vec<SymbolUseFact>) {
    let s = &parsed.source;
    for (id, body) in top_bodies(path, parsed.tree.root_node(), s, false) {
        for n in walk(body) {
            if let Some(name) = call_identifier(n, s, false) {
                uses.push(SymbolUseFact::new(
                    path.to_path_buf(),
                    id.clone(),
                    name,
                    "calls".into(),
                    "call".into(),
                    (n.start_position().row + 1) as i64,
                ));
            }
        }
    }
}
fn js_members(path: &Path, parsed: &Parsed, nodes: &[Node<'_>], f: &mut SymbolResolutionFacts) {
    let s = &parsed.source;
    for &n in nodes.iter().filter(|n| {
        [
            "class_declaration",
            "abstract_class_declaration",
            "interface_declaration",
        ]
        .contains(&n.kind_name())
    }) {
        if let Some(name) = n.child_by_field_name("name") {
            let name = read_text(name, s);
            if !name.is_empty() {
                let id = make_id([file_stem(path).as_str(), name.as_str()]);
                ts_class_members(n, s, path, &id, f);
            }
        }
    }
}
fn js_file_facts(path: &Path, parsed: &Parsed) -> JsFileFacts {
    let mut out = JsFileFacts::default();
    let nodes = walk(parsed.tree.root_node());
    js_declarations_imports(path, parsed, &nodes, &mut out.first);
    js_exports(path, parsed, &nodes, &mut out.exports);
    js_calls(path, parsed, &mut out.calls);
    js_members(path, parsed, &nodes, &mut out.members);
    out
}
/// What [`collect_js`] reads from one JS/TS file's tree, derived by the file's
/// extraction from the tree it parsed, so assembly does not parse the file
/// again. It is used only for the same bytes and the same source corpus
/// (module paths resolve against the corpus), else the file is parsed.
pub struct JsSyntax {
    digest: (usize, u64),
    corpus: Option<js::SourceCorpus>,
    facts: JsFileFacts,
}
/// The kind under which [`JsSyntax`] is kept in the run cache.
pub const JS_SYNTAX: &str = "js_syntax";
fn source_digest(source: &[u8]) -> (usize, u64) {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write(source);
    (source.len(), h.finish())
}
/// `parsed` is `path`'s own bytes parsed with the grammar named `tag`; when
/// that is the parse [`collect_js`] would make, its facts are computed now.
pub fn js_syntax(path: &Path, tag: &str, parsed: &Parsed) -> Option<JsSyntax> {
    (js_parse_tag(path) == Some(tag)).then(|| JsSyntax {
        digest: source_digest(&parsed.source),
        corpus: js::current_source_corpus(),
        facts: js_file_facts(path, parsed),
    })
}
/// Whether corpora are the same set, comparing contents once per distinct
/// kept corpus (every file of one extraction shares one).
struct CorpusCheck {
    current: Option<js::SourceCorpus>,
    seen: std::sync::Mutex<Vec<(usize, bool)>>,
}
impl CorpusCheck {
    fn matches(&self, kept: &Option<js::SourceCorpus>) -> bool {
        match (kept, &self.current) {
            (None, None) => true,
            (Some(a), Some(b)) if Arc::ptr_eq(a, b) => true,
            (Some(a), Some(b)) => {
                let key = Arc::as_ptr(a) as usize;
                let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((_, same)) = seen.iter().find(|(k, _)| *k == key) {
                    return *same;
                }
                let same = a == b;
                seen.push((key, same));
                same
            }
            _ => false,
        }
    }
}
/// The facts the file's extraction kept, when they describe the bytes the
/// file holds now and were resolved against the current corpus.
fn kept_js_facts(path: &Path, check: &CorpusCheck) -> Option<JsFileFacts> {
    let kept = parse_cache::take_facts(JS_SYNTAX, path)?
        .downcast::<JsSyntax>()
        .ok()?;
    let source = parse_cache::read(path).ok()?;
    (source_digest(&source) == kept.digest && check.matches(&kept.corpus)).then_some(kept.facts)
}
pub fn collect_js(paths: &[PathBuf], f: &mut SymbolResolutionFacts) {
    let paths = paths
        .iter()
        .filter(|p| JS_CACHE_BYPASS_SUFFIXES.contains(&effective_suffix(p).as_str()))
        .collect::<Vec<_>>();
    let keys = paths.iter().map(|p| resolved(p)).collect::<Vec<_>>();
    let distinct = keys.iter().collect::<std::collections::BTreeSet<_>>().len() == keys.len();
    let per_file = if distinct {
        // Each file's facts depend only on its own tree and the filesystem, so
        // the files are processed in parallel (each tree is freed as soon as
        // its file is done) against this thread's source corpus. A file whose
        // extraction kept its facts this run is not parsed again.
        let corpus = js::current_source_corpus();
        let check = CorpusCheck {
            current: corpus.clone(),
            seen: Default::default(),
        };
        // Module paths and grammars follow this thread's language overrides.
        let overrides = crate::rcfile::snapshot();
        parse_cache::par_map(&paths, |path| {
            overrides.install();
            let _corpus = js::enter_source_corpus(corpus.clone());
            if let Some(kept) = kept_js_facts(path, &check) {
                return kept;
            }
            match parse_js_tree(path) {
                Some(parsed) => js_file_facts(path, &parsed),
                None => JsFileFacts::default(),
            }
        })
    } else {
        // Two paths name one file: the later passes read the tree parsed last
        // for that file, as the sequential passes over a shared table did.
        let mut trees = BTreeMap::new();
        let mut out = paths
            .iter()
            .map(|_| JsFileFacts::default())
            .collect::<Vec<_>>();
        for (i, path) in paths.iter().enumerate() {
            let Some(parsed) = parse_js_tree(path) else {
                continue;
            };
            js_declarations_imports(
                path,
                &parsed,
                &walk(parsed.tree.root_node()),
                &mut out[i].first,
            );
            trees.insert(keys[i].clone(), parsed);
        }
        for (i, path) in paths.iter().enumerate() {
            if let Some(parsed) = trees.get(&keys[i]) {
                let nodes = walk(parsed.tree.root_node());
                js_exports(path, parsed, &nodes, &mut out[i].exports);
                js_calls(path, parsed, &mut out[i].calls);
                js_members(path, parsed, &nodes, &mut out[i].members);
            }
        }
        out
    };
    let mut calls = Vec::with_capacity(per_file.len());
    let mut members = Vec::with_capacity(per_file.len());
    for part in per_file {
        f.declarations.extend(part.first.declarations);
        f.imports.extend(part.first.imports);
        f.aliases.extend(part.first.aliases);
        f.namespace_exports.extend(part.exports.namespace_exports);
        f.star_exports.extend(part.exports.star_exports);
        f.exports.extend(part.exports.exports);
        calls.push(part.calls);
        members.push(part.members.uses);
    }
    f.uses.extend(calls.into_iter().flatten());
    f.uses.extend(members.into_iter().flatten());
}
pub fn python_import_from_module(n: Node<'_>, s: &[u8]) -> Option<(usize, String)> {
    let mut level = 0;
    let mut module = String::new();
    for c in children(n) {
        if c.kind_name() == "import" {
            break;
        }
        if c.kind_name() == "relative_import" {
            let raw = read_text(c, s);
            level = raw.chars().take_while(|c| *c == '.').count();
            module = raw.trim_start_matches('.').into();
            if let Some(d) = first_child(c, &["dotted_name"]) {
                module = read_text(d, s);
            }
        } else if c.kind_name() == "dotted_name" {
            module = read_text(c, s);
        }
    }
    if level == 0 && module.is_empty() {
        None
    } else {
        Some((level, module))
    }
}
/// `(imported name, local binding)` pairs of a Python import, including a
/// `*` wildcard as `("*", "*")` (`extract._python_import_bindings`).
pub fn python_import_bindings(n: Node<'_>, s: &[u8]) -> Vec<(String, String)> {
    let mut past = false;
    let mut out = vec![];
    for c in children(n) {
        if c.kind_name() == "import" {
            past = true;
            continue;
        }
        if !past {
            continue;
        }
        match c.kind_name() {
            "dotted_name" => {
                let name = read_text(c, s);
                let local = name.rsplit('.').next().unwrap_or("").into();
                out.push((name, local));
            }
            "aliased_import" => {
                let Some(name) = c.child_by_field_name("name") else {
                    continue;
                };
                let name = read_text(name, s);
                let local = c
                    .child_by_field_name("alias")
                    .map(|n| read_text(n, s))
                    .unwrap_or_else(|| name.rsplit('.').next().unwrap_or("").into());
                out.push((name, local));
            }
            "wildcard_import" => out.push(("*".into(), "*".into())),
            _ => {}
        }
    }
    out
}
pub fn python_imported_names(n: Node<'_>, s: &[u8]) -> Vec<(String, String)> {
    let mut past = false;
    let mut out = vec![];
    for c in children(n) {
        if c.kind_name() == "import" {
            past = true;
            continue;
        }
        if !past {
            continue;
        }
        if c.kind_name() == "dotted_name" {
            let name = read_text(c, s);
            let local = name.rsplit('.').next().unwrap_or("").into();
            out.push((name, local));
        } else if c.kind_name() == "aliased_import"
            && let Some(name) = c.child_by_field_name("name")
        {
            let name = read_text(name, s);
            let local = c
                .child_by_field_name("alias")
                .map(|n| read_text(n, s))
                .unwrap_or_else(|| name.rsplit('.').next().unwrap_or("").into());
            out.push((name, local));
        }
    }
    out
}
/// Dotted package namespace of a scan root nested below package directories.
/// `…/Company/Apps/Team`, whose parents through `Company` contain
/// `__init__.py`, becomes `Company.Apps.Team`; a root at the package boundary
/// has no prefix to project (#3843).
fn scan_root_namespace(root: &Path) -> Option<String> {
    let root = resolved(root);
    let mut parts = vec![root.file_name()?.to_string_lossy().into_owned()];
    let mut current = root.parent();
    while let Some(parent) = current {
        if !parent.join("__init__.py").is_file() {
            break;
        }
        parts.push(parent.file_name()?.to_string_lossy().into_owned());
        current = parent.parent();
    }
    if parts.len() <= 1 {
        return None;
    }
    parts.reverse();
    Some(parts.join("."))
}

fn projected_python_module(module: &str, root: &Path) -> Option<PathBuf> {
    let namespace = scan_root_namespace(root)?;
    let stripped = module.strip_prefix(&namespace)?;
    if !stripped.is_empty() && !stripped.starts_with('.') {
        return None;
    }
    let stripped = stripped.trim_start_matches('.');
    Some(if stripped.is_empty() {
        root.to_path_buf()
    } else {
        root.join(stripped.replace('.', "/"))
    })
}

pub fn resolve_python_module_path(
    module: &str,
    current: &Path,
    root: &Path,
    level: usize,
) -> Option<PathBuf> {
    let rel = module.replace('.', "/");
    if level > 0 {
        let mut base = current.parent().unwrap_or(Path::new("."));
        for _ in 1..level {
            base = base.parent().unwrap_or(base);
        }
        return probe_python_module_candidate(&if module.is_empty() {
            base.to_path_buf()
        } else {
            base.join(rel)
        });
    }
    // A bare absolute import of a stdlib name means the stdlib module: only a
    // package initializer (a deliberate shadow) may answer it, never a loose
    // same-named module or namespace directory (#4261).
    let stdlib_shadow = is_python_stdlib_module(module.split('.').next().unwrap_or(""));
    let accept = |candidate: Option<PathBuf>| {
        candidate.filter(|c| {
            !stdlib_shadow
                || c.file_name()
                    .is_some_and(|n| n == "__init__.py" || n == "__init__.pyi")
        })
    };
    if let Some(p) = accept(probe_python_module_candidate(&root.join(&rel))) {
        return Some(p);
    }
    if let Some(p) = accept(
        projected_python_module(module, root)
            .as_deref()
            .and_then(probe_python_module_candidate),
    ) {
        return Some(p);
    }
    for dir in current.parent().into_iter().flat_map(Path::ancestors) {
        if !dir.starts_with(root) {
            break;
        }
        if dir == root || is_python_package_dir(dir, &[".py"]) {
            continue;
        }
        if let Some(p) = accept(probe_python_module_candidate(&dir.join(&rel))) {
            return Some(p);
        }
    }
    None
}
pub fn resolve_python_namespace_dir(
    module: &str,
    current: &Path,
    root: &Path,
    level: usize,
) -> Option<PathBuf> {
    let namespace = |p: PathBuf| {
        if p.is_dir()
            && !is_python_package_dir(&p, &[".py"])
            && resolved(&p).starts_with(resolved(root))
        {
            Some(p)
        } else {
            None
        }
    };
    let rel = module.replace('.', "/");
    if level > 0 {
        let mut base = current.parent().unwrap_or(Path::new("."));
        for _ in 1..level {
            base = base.parent().unwrap_or(base);
        }
        return namespace(if module.is_empty() {
            base.to_path_buf()
        } else {
            base.join(rel)
        });
    }
    if module.is_empty() {
        return None;
    }
    if let Some(p) = namespace(root.join(&rel)) {
        return Some(p);
    }
    if let Some(p) = projected_python_module(module, root).and_then(&namespace) {
        return Some(p);
    }
    for dir in current.parent().into_iter().flat_map(Path::ancestors) {
        if !dir.starts_with(root) {
            break;
        }
        if dir == root || is_python_package_dir(dir, &[".py"]) {
            continue;
        }
        if let Some(p) = namespace(dir.join(&rel)) {
            return Some(p);
        }
    }
    None
}
/// What [`collect_python`] (and the cross-file import pass after it) reads
/// from one Python file's tree: its `from ... import ...` statements, the
/// calls in its top-level function bodies and the import pass's walk record.
/// A file's extraction keeps this when it parsed the file's own bytes, so
/// assembly does not parse the file again ([`python_syntax`]).
pub struct PythonSyntax {
    /// `(level, module, line, (imported, local) names)` per statement.
    imports: super::resolution_types::PythonImportFroms,
    uses: Vec<SymbolUseFact>,
    visit: super::resolution_types::PythonVisitFacts,
}
/// The kind under which [`PythonSyntax`] is kept in the run cache.
pub const PYTHON_SYNTAX: &str = "python_syntax";
/// [`PythonSyntax`] as the AST cache stores it beside a file's record, so a
/// run that reads the record from the cache does not parse the file either.
/// The file's path is not stored (the entry's key already binds its bytes to
/// its relative path); the caller ids of `calls` derive from the file stem,
/// which is stored and must match the path the facts are used for.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredPythonSyntax {
    format: u32,
    stem: String,
    imports: super::resolution_types::PythonImportFroms,
    /// `(caller id, callee, line)` of each use (all `calls` from a call).
    calls: Vec<(String, String, i64)>,
    visit: super::resolution_types::PythonVisitFacts,
}
const STORED_PYTHON_SYNTAX_FORMAT: u32 = 1;
impl PythonSyntax {
    /// The bytes the AST cache keeps for these facts, recorded for `path`.
    pub fn encode(&self, path: &Path) -> Option<Vec<u8>> {
        serde_json::to_vec(&StoredPythonSyntax {
            format: STORED_PYTHON_SYNTAX_FORMAT,
            stem: file_stem(path),
            imports: self.imports.clone(),
            calls: self
                .uses
                .iter()
                .map(|u| (u.source_id.clone(), u.local_name.clone(), u.line))
                .collect(),
            visit: self.visit.clone(),
        })
        .ok()
    }
    /// Facts [`Self::encode`] stored, for `path`; `None` unless they were
    /// recorded for a path with the same stem in this format.
    pub fn decode(path: &Path, bytes: &[u8]) -> Option<Self> {
        let stored: StoredPythonSyntax = serde_json::from_slice(bytes).ok()?;
        if stored.format != STORED_PYTHON_SYNTAX_FORMAT || stored.stem != file_stem(path) {
            return None;
        }
        let uses = stored
            .calls
            .into_iter()
            .map(|(id, name, line)| {
                SymbolUseFact::new(
                    path.to_path_buf(),
                    id,
                    name,
                    "calls".into(),
                    "call".into(),
                    line,
                )
            })
            .collect();
        Some(PythonSyntax {
            imports: stored.imports,
            uses,
            visit: stored.visit,
        })
    }
}
pub fn python_syntax(path: &Path, parsed: &super::engine_parser::Parsed) -> PythonSyntax {
    let (visit, imports, calls) = super::resolution_types::record_python_syntax(
        Some(path),
        parsed.tree.root_node(),
        &parsed.source,
    );
    let uses = calls
        .into_iter()
        .map(|(id, name, line)| {
            SymbolUseFact::new(
                path.to_path_buf(),
                id,
                name,
                "calls".into(),
                "call".into(),
                line,
            )
        })
        .collect();
    PythonSyntax {
        imports,
        uses,
        visit,
    }
}
pub fn collect_python(
    paths: &[PathBuf],
    root: &Path,
    f: &mut SymbolResolutionFacts,
    ambiguous: &AmbiguousModules,
) {
    let paths = paths
        .iter()
        .filter(|p| effective_suffix(p) == ".py")
        .collect::<Vec<_>>();
    // Each file's facts depend only on its own tree and the filesystem, so
    // they are gathered in parallel and appended in path order: every file's
    // import facts first, then every file's uses, as the sequential passes did.
    // Module probes follow this thread's language overrides.
    let overrides = crate::rcfile::snapshot();
    let per_file = parse_cache::par_map(&paths, |path| {
        overrides.install();
        let path: &PathBuf = path;
        let mut part = SymbolResolutionFacts::default();
        // The file's extraction kept its syntax when it parsed the file this
        // run; otherwise (a cached extraction) it is parsed here.
        let kept = parse_cache::take_facts(PYTHON_SYNTAX, path)
            .and_then(|facts| facts.downcast::<PythonSyntax>().ok());
        let syntax = match kept {
            Some(syntax) => *syntax,
            None => {
                let Some(parsed) = parse_python_tree(path) else {
                    return (part, vec![]);
                };
                python_syntax(path, &parsed)
            }
        };
        let PythonSyntax {
            imports,
            uses,
            visit,
        } = syntax;
        // What the later cross-file import pass reads from this tree, kept so
        // it does not parse the file again.
        parse_cache::keep_facts(
            super::resolution_types::PYTHON_VISIT_FACTS,
            path,
            Box::new(visit),
        );
        for (level, module, line, names) in imports {
            if level == 0 && ambiguous.contains(&make_id([module.as_str()])) {
                continue;
            }
            let target = resolve_python_module_path(&module, path, root, level);
            let package = if let Some(t) = &target {
                if t.file_stem().is_some_and(|s| s == "__init__") && effective_suffix(t) == ".py" {
                    t.parent().map(Path::to_path_buf)
                } else {
                    None
                }
            } else {
                resolve_python_namespace_dir(&module, path, root, level)
            };
            if target.is_none() && package.is_none() {
                continue;
            }
            for (imported, local) in names {
                if let Some(p) = &package {
                    let sub = p.join(format!("{imported}.py"));
                    let pkg = p.join(&imported).join("__init__.py");
                    if let Some(sub) = if sub.is_file() {
                        Some(sub)
                    } else if pkg.is_file() {
                        Some(pkg)
                    } else {
                        probe_declared_python_module_candidate(&p.join(&imported))
                    } {
                        let imported_module = if module.is_empty() {
                            imported.clone()
                        } else {
                            format!("{module}.{imported}")
                        };
                        if level == 0 && ambiguous.contains(&make_id([imported_module.as_str()])) {
                            continue;
                        }
                        part.module_imports.push((path.clone(), sub, line, local));
                        continue;
                    }
                }
                if let Some(t) = &target {
                    part.imports.push(SymbolImportFact::new(
                        path.clone(),
                        local.clone(),
                        t.clone(),
                        imported.clone(),
                        line,
                    ));
                    if path.file_stem().is_some_and(|n| n == "__init__") {
                        part.exports.push(SymbolExportFact::reexport(
                            path.clone(),
                            local,
                            line,
                            None,
                            Some(t.clone()),
                            Some(imported),
                            false,
                        ));
                    }
                }
            }
        }
        (part, uses)
    });
    let (parts, uses): (Vec<_>, Vec<_>) = per_file.into_iter().unzip();
    for part in parts {
        f.module_imports.extend(part.module_imports);
        f.imports.extend(part.imports);
        f.exports.extend(part.exports);
    }
    f.uses.extend(uses.into_iter().flatten());
}

#[cfg(test)]
mod scan_root_tests {
    use super::*;

    #[test]
    fn nested_absolute_namespace_projects_onto_scan_root() {
        let temp = tempfile::tempdir().unwrap();
        let company = temp.path().join("Company");
        let apps = company.join("Apps");
        let team = apps.join("Team");
        std::fs::create_dir_all(team.join("lib")).unwrap();
        std::fs::write(company.join("__init__.py"), "").unwrap();
        std::fs::write(apps.join("__init__.py"), "").unwrap();
        std::fs::write(team.join("__init__.py"), "").unwrap();
        std::fs::write(team.join("app.py"), "").unwrap();
        std::fs::write(team.join("lib.py"), "").unwrap();
        std::fs::write(team.join("lib/mod.py"), "").unwrap();

        assert_eq!(
            scan_root_namespace(&team).as_deref(),
            Some("Company.Apps.Team")
        );
        assert_eq!(
            resolve_python_module_path("Company.Apps.Team.lib", &team.join("app.py"), &team, 0),
            Some(team.join("lib.py"))
        );
        assert_eq!(
            resolve_python_namespace_dir("Company.Apps.Team.lib", &team.join("app.py"), &team, 0),
            Some(team.join("lib"))
        );
        assert_eq!(
            resolve_python_module_path("Company.Apps.Other.lib", &team.join("app.py"), &team, 0),
            None
        );
        assert_eq!(scan_root_namespace(&company), None);
    }
}
