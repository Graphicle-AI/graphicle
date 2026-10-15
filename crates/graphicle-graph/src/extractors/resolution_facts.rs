// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
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
use rustc_hash::{FxHashMap, FxHashSet};
use std::{
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
/// The relation of a JS/TS use, as [`JsUse`] keeps it.
#[derive(Clone, Copy)]
enum UseRelation {
    Calls,
    References,
    Inherits,
    Implements,
}
impl UseRelation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::References => "references",
            Self::Inherits => "inherits",
            Self::Implements => "implements",
        }
    }
}
/// The context of a JS/TS use, as [`JsUse`] keeps it.
#[derive(Clone, Copy)]
enum UseContext {
    Call,
    Type,
    GenericArg,
    ParameterType,
    ReturnType,
    Field,
}
impl UseContext {
    fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Type => "type",
            Self::GenericArg => "generic_arg",
            Self::ParameterType => "parameter_type",
            Self::ReturnType => "return_type",
            Self::Field => "field",
        }
    }
}
fn use_refs(
    id: &str,
    n: Option<Node<'_>>,
    s: &[u8],
    line: i64,
    context: UseContext,
    f: &mut JsFileFacts,
) {
    let mut refs = vec![];
    ts_collect_type_refs(n, s, false, &mut refs);
    for (name, role) in refs {
        let source = f.source(id);
        f.members.push(JsUse {
            local_name: name.into(),
            line,
            source,
            relation: UseRelation::References,
            context: if role == "generic_arg" {
                UseContext::GenericArg
            } else {
                context
            },
        });
    }
}
fn ts_class_members(n: Node<'_>, s: &[u8], id: &str, f: &mut JsFileFacts) {
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
                            UseRelation::Inherits
                        } else {
                            UseRelation::Implements
                        },
                    )
                })
                .collect();
            if clauses.is_empty() {
                clauses.push((child, UseRelation::Inherits));
            }
        } else if child.kind_name() == "extends_type_clause" {
            clauses.push((child, UseRelation::Inherits));
        }
        for (c, relation) in clauses {
            for name in heritage_entries(c, s) {
                let source = f.source(id);
                f.members.push(JsUse {
                    local_name: name.into(),
                    line: (c.start_position().row + 1) as i64,
                    source,
                    relation,
                    context: UseContext::Type,
                });
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
                            &mid,
                            p.child_by_field_name("type"),
                            s,
                            line,
                            UseContext::ParameterType,
                            f,
                        );
                    }
                }
                use_refs(
                    &mid,
                    m.child_by_field_name("return_type"),
                    s,
                    line,
                    UseContext::ReturnType,
                    f,
                );
            } else if matches!(
                m.kind_name(),
                "public_field_definition" | "property_signature"
            ) {
                use_refs(
                    id,
                    first_child(m, &["type_annotation"]),
                    s,
                    line,
                    UseContext::Field,
                    f,
                );
            }
        }
    }
}
/// An exported declaration of a JS/TS file.
struct JsDeclaration {
    name: Box<str>,
    line: i64,
}
/// A named or default import from the module `target`, an index into the
/// file's [`JsFileFacts::targets`].
struct JsImport {
    local_name: Box<str>,
    imported_name: Box<str>,
    target: u32,
    line: i64,
}
/// A lexical alias (`const a = b`).
struct JsAlias {
    alias: Box<str>,
    target_name: Box<str>,
    line: i64,
}
/// `export * as name from 'target'`.
struct JsNamespaceExport {
    exported_name: Box<str>,
    target: u32,
    line: i64,
    type_only: bool,
}
/// `export * from 'target'`.
struct JsStarExport {
    target: u32,
    line: i64,
    type_only: bool,
}
/// A named export, re-exported from `target` when it has one.
struct JsExport {
    exported_name: Box<str>,
    local_name: Option<Box<str>>,
    target: Option<u32>,
    target_name: Option<Box<str>>,
    line: i64,
    type_only: bool,
}
/// A use of a name by the body or class `source`, an index into the file's
/// [`JsFileFacts::sources`].
struct JsUse {
    local_name: Box<str>,
    line: i64,
    source: u32,
    relation: UseRelation,
    context: UseContext,
}
/// One JS/TS file's facts, bucketed by the pass that produced them, so files
/// can be processed independently and still be read pass by pass in path
/// order, exactly as four sequential passes over every file append them. The
/// file's path, each statement's module path and each use's source id are
/// stored once rather than in every fact: the facts are kept from the file's
/// extraction until assembly reads them.
#[derive(Default)]
pub struct JsFileFacts {
    /// The path the facts were derived for: the file of every fact.
    path: PathBuf,
    /// Module paths, one per statement naming a module.
    targets: Vec<PathBuf>,
    /// Source ids of the uses, stored once per run of uses sharing one.
    sources: Vec<Box<str>>,
    /// Pass 1: declarations, imports and lexical aliases.
    declarations: Vec<JsDeclaration>,
    imports: Vec<JsImport>,
    aliases: Vec<JsAlias>,
    /// Pass 2: namespace, star and named exports.
    namespace_exports: Vec<JsNamespaceExport>,
    star_exports: Vec<JsStarExport>,
    exports: Vec<JsExport>,
    /// Pass 3: calls from top-level bodies.
    calls: Vec<JsUse>,
    /// Pass 4: class and interface member type references.
    members: Vec<JsUse>,
}
impl JsFileFacts {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            ..Self::default()
        }
    }
    fn target(&mut self, path: PathBuf) -> u32 {
        self.targets.push(path);
        (self.targets.len() - 1) as u32
    }
    fn source(&mut self, id: &str) -> u32 {
        if self.sources.last().is_none_or(|last| **last != *id) {
            self.sources.push(id.into());
        }
        (self.sources.len() - 1) as u32
    }
    /// Drops the room the passes left spare.
    fn shrink(&mut self) {
        self.targets.shrink_to_fit();
        self.sources.shrink_to_fit();
        self.declarations.shrink_to_fit();
        self.imports.shrink_to_fit();
        self.aliases.shrink_to_fit();
        self.namespace_exports.shrink_to_fit();
        self.star_exports.shrink_to_fit();
        self.exports.shrink_to_fit();
        self.calls.shrink_to_fit();
        self.members.shrink_to_fit();
    }
}
/// `nodes` is [`walk`] of the file's tree: one pre-order walk serves every
/// whole-file pass below.
fn js_declarations_imports(path: &Path, parsed: &Parsed, nodes: &[Node<'_>], f: &mut JsFileFacts) {
    let s = &parsed.source;
    for &n in nodes {
        let line = (n.start_position().row + 1) as i64;
        if n.kind_name() == "export_statement" {
            for name in exported_names(n, s) {
                f.declarations.push(JsDeclaration {
                    name: name.into(),
                    line,
                });
            }
        }
        if n.kind_name() == "import_statement"
            && let Some(raw) = module_specifier(n, s)
            && let Some(target) =
                js::resolve_js_module_path(&raw, path.parent()).map(|p| resolved(&p))
        {
            let specifiers = named_specifiers(n, s, "import_specifier");
            let default =
                first_child(n, &["import_clause"]).and_then(|c| first_child(c, &["identifier"]));
            if specifiers.is_empty() && default.is_none() {
                continue;
            }
            let target = f.target(target);
            for (imported, local) in specifiers {
                f.imports.push(JsImport {
                    local_name: local.into(),
                    imported_name: imported.into(),
                    target,
                    line,
                });
            }
            if let Some(name) = default {
                f.imports.push(JsImport {
                    local_name: read_text(name, s).into(),
                    imported_name: "default".into(),
                    target,
                    line,
                });
            }
        }
    }
    for &n in nodes {
        for (alias, target) in lexical_aliases(n, s) {
            f.aliases.push(JsAlias {
                alias: alias.into(),
                target_name: target.into(),
                line: (n.start_position().row + 1) as i64,
            });
        }
    }
}
fn js_exports(path: &Path, parsed: &Parsed, nodes: &[Node<'_>], f: &mut JsFileFacts) {
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
            let target = f.target(target);
            let namespace =
                first_child(n, &["namespace_export"]).and_then(|n| first_child(n, &["identifier"]));
            if let Some(name) = namespace {
                f.namespace_exports.push(JsNamespaceExport {
                    exported_name: read_text(name, s).into(),
                    target,
                    line,
                    type_only,
                });
            } else if children(n).iter().any(|n| n.kind_name() == "*") {
                f.star_exports.push(JsStarExport {
                    target,
                    line,
                    type_only,
                });
            }
            if let Some(clause) = clause {
                for (original, exported) in named_specifiers(clause, s, "export_specifier") {
                    f.exports.push(JsExport {
                        exported_name: exported.into(),
                        local_name: None,
                        target: Some(target),
                        target_name: Some(original.into()),
                        line,
                        type_only,
                    });
                }
            }
            continue;
        }
        if let Some(clause) = clause {
            for (local, exported) in named_specifiers(clause, s, "export_specifier") {
                f.exports.push(JsExport {
                    exported_name: exported.into(),
                    local_name: Some(local.into()),
                    target: None,
                    target_name: None,
                    line,
                    type_only: false,
                });
            }
            continue;
        }
        for name in exported_names(n, s) {
            f.exports.push(JsExport {
                exported_name: name.as_str().into(),
                local_name: Some(name.into()),
                target: None,
                target_name: None,
                line,
                type_only: false,
            });
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
                f.exports.push(JsExport {
                    exported_name: "default".into(),
                    local_name: Some(read_text(name, s).into()),
                    target: None,
                    target_name: None,
                    line,
                    type_only: false,
                });
            }
        }
    }
}
fn js_calls(path: &Path, parsed: &Parsed, f: &mut JsFileFacts) {
    let s = &parsed.source;
    for (id, body) in top_bodies(path, parsed.tree.root_node(), s, false) {
        for n in walk(body) {
            if let Some(name) = call_identifier(n, s, false) {
                let source = f.source(&id);
                f.calls.push(JsUse {
                    local_name: name.into(),
                    line: (n.start_position().row + 1) as i64,
                    source,
                    relation: UseRelation::Calls,
                    context: UseContext::Call,
                });
            }
        }
    }
}
fn js_members(path: &Path, parsed: &Parsed, nodes: &[Node<'_>], f: &mut JsFileFacts) {
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
                ts_class_members(n, s, &id, f);
            }
        }
    }
}
fn js_file_facts(path: &Path, parsed: &Parsed) -> JsFileFacts {
    let mut out = JsFileFacts::new(path);
    let nodes = walk(parsed.tree.root_node());
    js_declarations_imports(path, parsed, &nodes, &mut out);
    js_exports(path, parsed, &nodes, &mut out);
    js_calls(path, parsed, &mut out);
    js_members(path, parsed, &nodes, &mut out);
    out.shrink();
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
/// Every JS/TS file's facts among `paths`, in path order.
pub fn collect_js_files(paths: &[PathBuf]) -> Vec<JsFileFacts> {
    let paths = paths
        .iter()
        .filter(|p| JS_CACHE_BYPASS_SUFFIXES.contains(&effective_suffix(p).as_str()))
        .collect::<Vec<_>>();
    // Paths that name one file (a symlink beside its target) are read as one
    // unit, in path order; every other path is a unit of its own.
    let keys = paths.iter().map(|p| resolved(p)).collect::<Vec<_>>();
    let mut unit_of: FxHashMap<&PathBuf, usize> = FxHashMap::default();
    let mut units: Vec<Vec<usize>> = Vec::with_capacity(paths.len());
    for (i, key) in keys.iter().enumerate() {
        match unit_of.entry(key) {
            std::collections::hash_map::Entry::Occupied(unit) => units[*unit.get()].push(i),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(units.len());
                units.push(vec![i]);
            }
        }
    }
    drop(unit_of);
    // Each unit's facts depend only on its own trees and the filesystem, so
    // the units are processed in parallel (each tree is freed as soon as its
    // unit is done) against this thread's source corpus. A file whose
    // extraction kept its facts this run is not parsed again.
    let corpus = js::current_source_corpus();
    let check = CorpusCheck {
        current: corpus.clone(),
        seen: Default::default(),
    };
    // Module paths and grammars follow this thread's language overrides.
    let overrides = crate::rcfile::snapshot();
    let done = parse_cache::par_map(&units, |unit| {
        overrides.install();
        let _corpus = js::enter_source_corpus(corpus.clone());
        if let [i] = unit[..] {
            let path = paths[i];
            if let Some(kept) = kept_js_facts(path, &check) {
                return vec![kept];
            }
            return vec![match parse_js_tree(path) {
                Some(parsed) => js_file_facts(path, &parsed),
                None => JsFileFacts::default(),
            }];
        }
        js_shared_file_facts(&unit.iter().map(|&i| paths[i]).collect::<Vec<_>>())
    });
    let mut out = Vec::with_capacity(paths.len());
    out.resize_with(paths.len(), JsFileFacts::default);
    for (unit, facts) in units.iter().zip(done) {
        for (&i, facts) in unit.iter().zip(facts) {
            out[i] = facts;
        }
    }
    out
}
/// The facts of `paths`, which name one file, in path order: each path's
/// first pass reads the tree parsed from that path, and every path's later
/// passes read the tree parsed last, as sequential passes over one table of
/// trees by file did. Only these trees are held, not every file's.
fn js_shared_file_facts(paths: &[&PathBuf]) -> Vec<JsFileFacts> {
    let mut out = paths
        .iter()
        .map(|p| JsFileFacts::new(p))
        .collect::<Vec<_>>();
    let mut last = None;
    for (path, facts) in paths.iter().zip(&mut out) {
        // Facts kept for one path of the file are not read: its later passes
        // follow the tree parsed last.
        drop(parse_cache::take_facts(JS_SYNTAX, path));
        let Some(parsed) = parse_js_tree(path) else {
            continue;
        };
        js_declarations_imports(path, &parsed, &walk(parsed.tree.root_node()), facts);
        last = Some(parsed);
    }
    if let Some(parsed) = &last {
        let nodes = walk(parsed.tree.root_node());
        for (path, facts) in paths.iter().zip(&mut out) {
            js_exports(path, parsed, &nodes, facts);
            js_calls(path, parsed, facts);
            js_members(path, parsed, &nodes, facts);
        }
    }
    out
}
/// Appends every JS/TS file's facts among `paths` to `f`, kind by kind in
/// path order, the calls of every file before their member uses.
pub fn collect_js(paths: &[PathBuf], f: &mut SymbolResolutionFacts) {
    append_js_facts(collect_js_files(paths), f);
}
/// Appends `files`' facts to `f` as [`collect_js`] does.
fn append_js_facts(files: Vec<JsFileFacts>, f: &mut SymbolResolutionFacts) {
    let facts = PassFacts::new(files, SymbolResolutionFacts::default());
    f.declarations.extend(
        facts
            .declarations()
            .map(|d| SymbolDeclarationFact::new(d.file_path.into(), d.name.into(), d.line)),
    );
    f.imports.extend(facts.imports().map(|i| {
        SymbolImportFact::new(
            i.file_path.into(),
            i.local_name.into(),
            i.target_path.into(),
            i.imported_name.into(),
            i.line,
        )
    }));
    f.aliases.extend(facts.aliases().map(|a| {
        SymbolAliasFact::new(
            a.file_path.into(),
            a.alias.into(),
            a.target_name.into(),
            a.line,
        )
    }));
    f.namespace_exports
        .extend(facts.namespace_exports().map(|n| NamespaceExportFact {
            file_path: n.file_path.into(),
            exported_name: n.exported_name.into(),
            target_path: n.target_path.into(),
            line: n.line,
            type_only: n.type_only,
        }));
    f.star_exports
        .extend(facts.star_exports().map(|s| StarExportFact {
            file_path: s.file_path.into(),
            target_path: s.target_path.into(),
            line: s.line,
            type_only: s.type_only,
        }));
    f.exports.extend(facts.exports().map(|e| {
        SymbolExportFact::reexport(
            e.file_path.into(),
            e.exported_name.into(),
            e.line,
            e.local_name.map(Into::into),
            e.target_path.map(Into::into),
            e.target_name.map(Into::into),
            e.type_only,
        )
    }));
    f.uses.extend(facts.js_uses().map(|u| {
        SymbolUseFact::new(
            u.file_path.into(),
            u.source_id.into(),
            u.local_name.into(),
            u.relation.into(),
            u.context.into(),
            u.line,
        )
    }));
}
/// A declaration as a resolution pass reads it, from either form of facts.
pub(crate) struct DeclarationRef<'a> {
    pub(crate) file_path: &'a Path,
    pub(crate) name: &'a str,
    pub(crate) line: i64,
}
/// An import as a resolution pass reads it.
pub(crate) struct ImportRef<'a> {
    pub(crate) file_path: &'a Path,
    pub(crate) local_name: &'a str,
    pub(crate) target_path: &'a Path,
    pub(crate) imported_name: &'a str,
    pub(crate) line: i64,
}
/// A lexical alias as a resolution pass reads it.
pub(crate) struct AliasRef<'a> {
    pub(crate) file_path: &'a Path,
    pub(crate) alias: &'a str,
    pub(crate) target_name: &'a str,
    pub(crate) line: i64,
}
/// A namespace export as a resolution pass reads it.
pub(crate) struct NamespaceExportRef<'a> {
    pub(crate) file_path: &'a Path,
    pub(crate) exported_name: &'a str,
    pub(crate) target_path: &'a Path,
    pub(crate) line: i64,
    pub(crate) type_only: bool,
}
/// A star export as a resolution pass reads it.
pub(crate) struct StarExportRef<'a> {
    pub(crate) file_path: &'a Path,
    pub(crate) target_path: &'a Path,
    pub(crate) line: i64,
    pub(crate) type_only: bool,
}
/// A named export as a resolution pass reads it.
pub(crate) struct ExportRef<'a> {
    pub(crate) file_path: &'a Path,
    pub(crate) exported_name: &'a str,
    pub(crate) line: i64,
    pub(crate) local_name: Option<&'a str>,
    pub(crate) target_path: Option<&'a Path>,
    pub(crate) target_name: Option<&'a str>,
    pub(crate) type_only: bool,
}
/// A use as a resolution pass reads it.
pub(crate) struct UseRef<'a> {
    pub(crate) file_path: &'a Path,
    pub(crate) source_id: &'a str,
    pub(crate) local_name: &'a str,
    pub(crate) relation: &'a str,
    pub(crate) context: &'a str,
    pub(crate) line: i64,
}
impl<'a> From<&'a SymbolUseFact> for UseRef<'a> {
    fn from(u: &'a SymbolUseFact) -> Self {
        UseRef {
            file_path: &u.file_path,
            source_id: &u.source_id,
            local_name: &u.local_name,
            relation: &u.relation,
            context: &u.context,
            line: u.line,
        }
    }
}
/// `uses` of the file `f` as a resolution pass reads them.
fn js_use_refs<'a>(f: &'a JsFileFacts, uses: &'a [JsUse]) -> impl Iterator<Item = UseRef<'a>> {
    uses.iter().map(move |u| UseRef {
        file_path: &f.path,
        source_id: &f.sources[u.source as usize],
        local_name: &u.local_name,
        relation: u.relation.as_str(),
        context: u.context.as_str(),
        line: u.line,
    })
}
/// The facts one symbol-resolution pass applies: every JS/TS file's facts in
/// path order, then the facts given whole (Python's, or a caller's). Each kind
/// reads in the order [`collect_js`] and then [`collect_python`] append it,
/// and each can be freed once its last reader is done.
#[derive(Default)]
pub struct PassFacts {
    js: Vec<JsFileFacts>,
    rest: SymbolResolutionFacts,
}
impl PassFacts {
    pub fn new(js: Vec<JsFileFacts>, rest: SymbolResolutionFacts) -> Self {
        Self { js, rest }
    }
    pub(crate) fn declarations(&self) -> impl Iterator<Item = DeclarationRef<'_>> {
        let js = self.js.iter().flat_map(|f| {
            f.declarations.iter().map(move |d| DeclarationRef {
                file_path: &f.path,
                name: &d.name,
                line: d.line,
            })
        });
        js.chain(self.rest.declarations.iter().map(|d| DeclarationRef {
            file_path: &d.file_path,
            name: &d.name,
            line: d.line,
        }))
    }
    pub(crate) fn imports(&self) -> impl Iterator<Item = ImportRef<'_>> {
        let js = self.js.iter().flat_map(|f| {
            f.imports.iter().map(move |i| ImportRef {
                file_path: &f.path,
                local_name: &i.local_name,
                target_path: &f.targets[i.target as usize],
                imported_name: &i.imported_name,
                line: i.line,
            })
        });
        js.chain(self.rest.imports.iter().map(|i| ImportRef {
            file_path: &i.file_path,
            local_name: &i.local_name,
            target_path: &i.target_path,
            imported_name: &i.imported_name,
            line: i.line,
        }))
    }
    pub(crate) fn aliases(&self) -> impl Iterator<Item = AliasRef<'_>> {
        let js = self.js.iter().flat_map(|f| {
            f.aliases.iter().map(move |a| AliasRef {
                file_path: &f.path,
                alias: &a.alias,
                target_name: &a.target_name,
                line: a.line,
            })
        });
        js.chain(self.rest.aliases.iter().map(|a| AliasRef {
            file_path: &a.file_path,
            alias: &a.alias,
            target_name: &a.target_name,
            line: a.line,
        }))
    }
    pub(crate) fn namespace_exports(&self) -> impl Iterator<Item = NamespaceExportRef<'_>> {
        let js = self.js.iter().flat_map(|f| {
            f.namespace_exports.iter().map(move |n| NamespaceExportRef {
                file_path: &f.path,
                exported_name: &n.exported_name,
                target_path: &f.targets[n.target as usize],
                line: n.line,
                type_only: n.type_only,
            })
        });
        js.chain(
            self.rest
                .namespace_exports
                .iter()
                .map(|n| NamespaceExportRef {
                    file_path: &n.file_path,
                    exported_name: &n.exported_name,
                    target_path: &n.target_path,
                    line: n.line,
                    type_only: n.type_only,
                }),
        )
    }
    pub(crate) fn star_exports(&self) -> impl Iterator<Item = StarExportRef<'_>> {
        let js = self.js.iter().flat_map(|f| {
            f.star_exports.iter().map(move |s| StarExportRef {
                file_path: &f.path,
                target_path: &f.targets[s.target as usize],
                line: s.line,
                type_only: s.type_only,
            })
        });
        js.chain(self.rest.star_exports.iter().map(|s| StarExportRef {
            file_path: &s.file_path,
            target_path: &s.target_path,
            line: s.line,
            type_only: s.type_only,
        }))
    }
    pub(crate) fn exports(&self) -> impl Iterator<Item = ExportRef<'_>> {
        let js = self.js.iter().flat_map(|f| {
            f.exports.iter().map(move |e| ExportRef {
                file_path: &f.path,
                exported_name: &e.exported_name,
                line: e.line,
                local_name: e.local_name.as_deref(),
                target_path: e.target.map(|t| f.targets[t as usize].as_path()),
                target_name: e.target_name.as_deref(),
                type_only: e.type_only,
            })
        });
        js.chain(self.rest.exports.iter().map(|e| ExportRef {
            file_path: &e.file_path,
            exported_name: &e.exported_name,
            line: e.line,
            local_name: e.local_name.as_deref(),
            target_path: e.target_path.as_deref(),
            target_name: e.target_name.as_deref(),
            type_only: e.type_only,
        }))
    }
    pub(crate) fn module_imports(&self) -> &[(PathBuf, PathBuf, i64, String)] {
        &self.rest.module_imports
    }
    /// The JS/TS uses: every file's calls, then every file's member uses.
    pub(crate) fn js_uses(&self) -> impl Iterator<Item = UseRef<'_>> {
        let calls = self.js.iter().flat_map(|f| js_use_refs(f, &f.calls));
        calls.chain(self.js.iter().flat_map(|f| js_use_refs(f, &f.members)))
    }
    /// The uses given whole, which follow [`Self::js_uses`].
    pub(crate) fn take_rest_uses(&mut self) -> Vec<SymbolUseFact> {
        std::mem::take(&mut self.rest.uses)
    }
    pub(crate) fn free_declarations(&mut self) {
        self.js.iter_mut().for_each(|f| f.declarations = Vec::new());
        self.rest.declarations = Vec::new();
    }
    pub(crate) fn free_aliases(&mut self) {
        self.js.iter_mut().for_each(|f| f.aliases = Vec::new());
        self.rest.aliases = Vec::new();
    }
    pub(crate) fn free_star_exports(&mut self) {
        self.js.iter_mut().for_each(|f| f.star_exports = Vec::new());
        self.rest.star_exports = Vec::new();
    }
    pub(crate) fn free_namespace_exports(&mut self) {
        self.js
            .iter_mut()
            .for_each(|f| f.namespace_exports = Vec::new());
        self.rest.namespace_exports = Vec::new();
    }
    pub(crate) fn free_exports(&mut self) {
        self.js.iter_mut().for_each(|f| f.exports = Vec::new());
        self.rest.exports = Vec::new();
    }
    /// Frees the imports, and the module paths once nothing else reads them.
    pub(crate) fn free_imports(&mut self) {
        for f in &mut self.js {
            f.imports = Vec::new();
            if f.star_exports.is_empty() && f.namespace_exports.is_empty() && f.exports.is_empty() {
                f.targets = Vec::new();
            }
        }
        self.rest.imports = Vec::new();
    }
    pub(crate) fn free_module_imports(&mut self) {
        self.rest.module_imports = Vec::new();
    }
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
    f.module_imports
        .reserve_exact(parts.iter().map(|p| p.module_imports.len()).sum());
    f.imports
        .reserve_exact(parts.iter().map(|p| p.imports.len()).sum());
    f.exports
        .reserve_exact(parts.iter().map(|p| p.exports.len()).sum());
    for part in parts {
        f.module_imports.extend(part.module_imports);
        f.imports.extend(part.imports);
        f.exports.extend(part.exports);
    }
    f.uses.reserve_exact(uses.iter().map(Vec::len).sum());
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

#[cfg(all(test, unix))]
mod js_unit_tests {
    use super::*;

    /// Paths naming one file read their first pass from their own tree and
    /// their later passes from the tree parsed last, as sequential passes over
    /// one table of trees by file did, even when the paths' grammars differ.
    #[test]
    fn symlinked_paths_read_the_tree_parsed_last() {
        let temp = tempfile::tempdir().unwrap();
        let root = resolved(temp.path());
        let a = root.join("a.ts");
        let b = root.join("b.ts");
        let c = root.join("c.js");
        std::fs::write(
            &a,
            "import { Base, helper } from './b';\n\
             export * from './b';\n\
             export class Child extends Base {\n  field: Base;\n  run(p: Base): Base { return p }\n}\n\
             export function main() { helper(1); }\n",
        )
        .unwrap();
        std::fs::write(
            &b,
            "export class Base {}\nexport function helper(x) { return x }\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&a, &c).unwrap();

        let tree = |p: &Path| parse_js_tree(p).unwrap();
        let (ta, tb, tc) = (tree(&a), tree(&b), tree(&c));
        // a.ts: its first pass from its own (TypeScript) tree, the later ones
        // from c.js's (JavaScript) tree, which was parsed last.
        let mut expected_a = JsFileFacts::new(&a);
        js_declarations_imports(&a, &ta, &walk(ta.tree.root_node()), &mut expected_a);
        let nodes_c = walk(tc.tree.root_node());
        js_exports(&a, &tc, &nodes_c, &mut expected_a);
        js_calls(&a, &tc, &mut expected_a);
        js_members(&a, &tc, &nodes_c, &mut expected_a);
        let mut expected = SymbolResolutionFacts::default();
        append_js_facts(
            vec![expected_a, js_file_facts(&b, &tb), js_file_facts(&c, &tc)],
            &mut expected,
        );
        // The grammars do differ here: a.ts's own tree has member types.
        let mut own = SymbolResolutionFacts::default();
        append_js_facts(vec![js_file_facts(&a, &ta)], &mut own);
        assert!(own.uses.len() > expected.uses.iter().filter(|u| u.file_path == a).count());

        let mut got = SymbolResolutionFacts::default();
        collect_js(&[a.clone(), b.clone(), c.clone()], &mut got);
        assert_eq!(got, expected);
        assert!(!got.imports.is_empty() && !got.star_exports.is_empty());
    }
}
