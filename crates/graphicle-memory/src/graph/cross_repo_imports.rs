//! Cross-repository import resolution for independently indexed repositories.
//! Pure and typed: no I/O beyond an optional caller-supplied
//! source loader.
//!
//! * [`exports`] builds an [`ExportIndex`] for one repository graph: package
//!   identity from its manifests (`pyproject.toml` / `setup.cfg`,
//!   `package.json`, `go.mod`) and a map from importable module paths
//!   (`pkg.sub.mod`, `@scope/pkg/sub`, `example.com/mod/pkg`) to the graph's
//!   module and symbol nodes.
//! * [`link`] walks every import edge of an importing graph whose target is an
//!   external stub, recovers the import statements from the importing file
//!   (when its text is available) and matches them against the export indexes
//!   of the other repositories. Exact module + symbol matches are `EXTRACTED`;
//!   anything recovered by convention or without source text is `INFERRED`;
//!   ambiguity (two exporters, unsatisfied version constraint, missing symbol
//!   or module) is reported in `unresolved` with its reason and candidates and
//!   never guessed.
//!
//! Repository identity and episode are carried on both ends of every edge so a
//! host can persist them bi-temporally.
use crate::graph::{Error, Result, graph::Graph, session};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::OnceLock,
};

pub const RELATION_MODULE: &str = "imports_from";
pub const RELATION_SYMBOL: &str = "imports";
pub const EXTRACTED: &str = "EXTRACTED";
pub const INFERRED: &str = "INFERRED";

pub const REASON_NO_EXPORTER: &str = "no_exporter";
pub const REASON_DECLARED_NOT_INDEXED: &str = "declared_dependency_not_indexed";
pub const REASON_AMBIGUOUS: &str = "ambiguous_exporters";
pub const REASON_VERSION: &str = "version_mismatch";
pub const REASON_MODULE: &str = "module_not_exported";
pub const REASON_SYMBOL: &str = "symbol_not_exported";

const IMPORT_RELATIONS: &[&str] = &["imports", "imports_from", "requires", "require"];
const PY_EXTS: &[&str] = &[".py", ".pyi"];
const JS_EXTS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];
const JS_STRIP_DIRS: &[&str] = &["src", "lib"];
const JS_BUILD_DIRS: &[&str] = &["dist", "lib", "build", "out", "esm", "cjs"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    Python,
    Node,
    Go,
}

impl Ecosystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Ecosystem::Python => "python",
            Ecosystem::Node => "node",
            Ecosystem::Go => "go",
        }
    }
    fn of_file(relpath: &str) -> Option<Self> {
        let lower = relpath.to_ascii_lowercase();
        if PY_EXTS.iter().any(|e| lower.ends_with(e)) {
            Some(Ecosystem::Python)
        } else if JS_EXTS.iter().any(|e| lower.ends_with(e)) {
            Some(Ecosystem::Node)
        } else if lower.ends_with(".go") {
            Some(Ecosystem::Go)
        } else {
            None
        }
    }
}

/// A manifest file the caller read from the repository (the graph does not
/// carry manifest text). The kind is taken from the file name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageManifest {
    pub relpath: String,
    pub text: String,
}

impl PackageManifest {
    pub fn new(relpath: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            relpath: relpath.into(),
            text: text.into(),
        }
    }
    /// Whether `relpath` names a manifest this module understands.
    pub fn recognised(relpath: &str) -> bool {
        matches!(
            file_name(relpath),
            "pyproject.toml" | "setup.cfg" | "package.json" | "go.mod"
        )
    }
}

/// Package identity declared by one manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageIdentity {
    pub ecosystem: Ecosystem,
    /// Distribution name (Python), `package.json` name, or Go module path.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Directory of the manifest relative to the repository root (`""` = root).
    pub root: String,
    pub manifest: String,
    /// Importable roots: Python top-level packages, the Node package name, or
    /// the Go module path.
    pub import_roots: Vec<String>,
    /// Declared dependencies: name → version constraint (may be empty).
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    /// Node `exports` map: subpath (`"."`, `"./core"`) → file target without
    /// extension. Wildcard patterns are not expanded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exports_map: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    /// Python `src/` layout or `package_dir` prefix stripped before module paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_prefix: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRef {
    pub name: String,
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    pub kind: String,
    pub relpath: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleExport {
    pub module_path: String,
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    pub relpath: String,
    /// How the module path was derived: `path` (exact convention),
    /// `exports_map`, `main`, `index`, or `src_strip` (inferred convention).
    pub derivation: String,
    pub symbols: BTreeMap<String, SymbolRef>,
}

impl ModuleExport {
    fn exact(&self) -> bool {
        !self.derivation.starts_with("src_strip") && !self.derivation.contains("build_dir")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportIndex {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<String>,
    pub packages: Vec<PackageIdentity>,
    /// Module path → export; one module path may be reachable from several
    /// derivations, the most exact one wins.
    pub modules: BTreeMap<String, ModuleExport>,
}

impl ExportIndex {
    /// The package of this index that owns `module_path` in `ecosystem`, if any.
    pub fn claims(&self, ecosystem: Ecosystem, module_path: &str) -> Option<&PackageIdentity> {
        self.packages
            .iter()
            .filter(|p| p.ecosystem == ecosystem)
            .find(|p| {
                p.import_roots
                    .iter()
                    .any(|root| owns(ecosystem, root, module_path))
            })
    }
}

fn owns(ecosystem: Ecosystem, root: &str, module_path: &str) -> bool {
    match ecosystem {
        Ecosystem::Python => module_path == root || module_path.starts_with(&format!("{root}.")),
        Ecosystem::Node | Ecosystem::Go => {
            module_path == root || module_path.starts_with(&format!("{root}/"))
        }
    }
}

/// Public top-level modules of the CPython standard library (3.9–3.13);
/// underscore-prefixed and platform-private names are covered by the
/// `_` rule in [`is_stdlib`].
const PYTHON_STDLIB: &[&str] = &[
    "abc",
    "aifc",
    "argparse",
    "array",
    "ast",
    "asynchat",
    "asyncio",
    "asyncore",
    "atexit",
    "audioop",
    "base64",
    "bdb",
    "binascii",
    "binhex",
    "bisect",
    "builtins",
    "bz2",
    "calendar",
    "cgi",
    "cgitb",
    "chunk",
    "cmath",
    "cmd",
    "code",
    "codecs",
    "codeop",
    "collections",
    "colorsys",
    "compileall",
    "concurrent",
    "configparser",
    "contextlib",
    "contextvars",
    "copy",
    "copyreg",
    "cProfile",
    "crypt",
    "csv",
    "ctypes",
    "curses",
    "dataclasses",
    "datetime",
    "dbm",
    "decimal",
    "difflib",
    "dis",
    "distutils",
    "doctest",
    "email",
    "encodings",
    "ensurepip",
    "enum",
    "errno",
    "faulthandler",
    "fcntl",
    "filecmp",
    "fileinput",
    "fnmatch",
    "fractions",
    "ftplib",
    "functools",
    "gc",
    "getopt",
    "getpass",
    "gettext",
    "glob",
    "graphlib",
    "grp",
    "gzip",
    "hashlib",
    "heapq",
    "hmac",
    "html",
    "http",
    "idlelib",
    "imaplib",
    "imghdr",
    "imp",
    "importlib",
    "inspect",
    "io",
    "ipaddress",
    "itertools",
    "json",
    "keyword",
    "lib2to3",
    "linecache",
    "locale",
    "logging",
    "lzma",
    "mailbox",
    "mailcap",
    "marshal",
    "math",
    "mimetypes",
    "mmap",
    "modulefinder",
    "msilib",
    "msvcrt",
    "multiprocessing",
    "netrc",
    "nis",
    "nntplib",
    "ntpath",
    "nturl2path",
    "numbers",
    "opcode",
    "operator",
    "optparse",
    "os",
    "ossaudiodev",
    "pathlib",
    "pdb",
    "pickle",
    "pickletools",
    "pipes",
    "pkgutil",
    "platform",
    "plistlib",
    "poplib",
    "posix",
    "posixpath",
    "pprint",
    "profile",
    "pstats",
    "pty",
    "pwd",
    "py_compile",
    "pyclbr",
    "pydoc",
    "pydoc_data",
    "pyexpat",
    "queue",
    "quopri",
    "random",
    "re",
    "readline",
    "reprlib",
    "resource",
    "rlcompleter",
    "runpy",
    "sched",
    "secrets",
    "select",
    "selectors",
    "shelve",
    "shlex",
    "shutil",
    "signal",
    "site",
    "smtpd",
    "smtplib",
    "sndhdr",
    "socket",
    "socketserver",
    "spwd",
    "sqlite3",
    "sre_compile",
    "sre_constants",
    "sre_parse",
    "ssl",
    "stat",
    "statistics",
    "string",
    "stringprep",
    "struct",
    "subprocess",
    "sunau",
    "symtable",
    "sys",
    "sysconfig",
    "syslog",
    "tabnanny",
    "tarfile",
    "telnetlib",
    "tempfile",
    "termios",
    "textwrap",
    "this",
    "threading",
    "time",
    "timeit",
    "tkinter",
    "token",
    "tokenize",
    "tomllib",
    "trace",
    "traceback",
    "tracemalloc",
    "tty",
    "turtle",
    "turtledemo",
    "types",
    "typing",
    "unicodedata",
    "unittest",
    "urllib",
    "uu",
    "uuid",
    "venv",
    "warnings",
    "wave",
    "weakref",
    "webbrowser",
    "winreg",
    "winsound",
    "wsgiref",
    "xdrlib",
    "xml",
    "xmlrpc",
    "zipapp",
    "zipfile",
    "zipimport",
    "zlib",
    "zoneinfo",
];

/// Node.js built-in modules (also accepted with the `node:` scheme).
const NODE_BUILTINS: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "test",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Whether `module_path` names the language's standard library. Go's rule
/// (first segment without a dot) is applied while parsing import blocks.
fn is_stdlib(ecosystem: Ecosystem, module_path: &str) -> bool {
    match ecosystem {
        Ecosystem::Python => {
            let top = module_path.split('.').next().unwrap_or(module_path);
            top.starts_with('_') || PYTHON_STDLIB.contains(&top)
        }
        Ecosystem::Node => {
            let bare = module_path.strip_prefix("node:").unwrap_or(module_path);
            module_path.starts_with("node:")
                || NODE_BUILTINS.contains(&bare.split('/').next().unwrap_or(bare))
        }
        Ecosystem::Go => !module_path
            .split('/')
            .next()
            .unwrap_or(module_path)
            .contains('.'),
    }
}

/// One end of a cross-repository edge.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeEnd {
    pub repo: String,
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relpath: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossEdge {
    pub source: EdgeEnd,
    pub target: EdgeEnd,
    /// `imports_from` (module-level) or `imports` (symbol-level).
    pub relation: String,
    /// `EXTRACTED` when the module path and the symbol (or a whole-module
    /// import) matched exactly from the import statement; else `INFERRED`.
    pub confidence: String,
    pub derivation: String,
    pub module_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_constraint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exporter_version: Option<String>,
    /// `satisfied`, `unconstrained` (importer declares no constraint) or
    /// `unchecked` (exporter declares no version, or the constraint is not a
    /// version range).
    pub version_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedImport {
    pub repo: String,
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relpath: Option<String>,
    pub ecosystem: Ecosystem,
    pub module_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Package name the import belongs to (first segment / scoped name / Go
    /// module prefix as far as it is known).
    pub package: String,
    pub reason: String,
    /// Repositories that could have satisfied the import.
    #[serde(default)]
    pub candidates: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_constraint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exporter_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkReport {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<String>,
    pub edges: Vec<CrossEdge>,
    pub unresolved: Vec<UnresolvedImport>,
    /// Import edges to external targets that were examined.
    pub external_imports: usize,
    /// Importing files whose text the loader supplied.
    pub files_with_source: usize,
    pub files_without_source: usize,
    /// External imports not reported as unresolved: the importer's own
    /// packages (the extractor left them unresolved), the language's standard
    /// library, and — when the importer declares dependencies at all —
    /// packages it neither declares nor any index claims (third parties).
    #[serde(default)]
    pub skipped_internal: usize,
    #[serde(default)]
    pub skipped_stdlib: usize,
    #[serde(default)]
    pub skipped_undeclared: usize,
}

// ───────────────────────── helpers ─────────────────────────

fn file_name(relpath: &str) -> &str {
    relpath.rsplit('/').next().unwrap_or(relpath)
}

fn norm(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    p
}

fn dir_of(relpath: &str) -> String {
    match relpath.rfind('/') {
        Some(at) => relpath[..at].to_string(),
        None => String::new(),
    }
}

fn strip_ext<'a>(path: &'a str, exts: &[&str]) -> Option<&'a str> {
    let lower = path.to_ascii_lowercase();
    exts.iter()
        .find(|e| lower.ends_with(*e))
        .map(|e| &path[..path.len() - e.len()])
}

fn under(root: &str, relpath: &str) -> Option<String> {
    if root.is_empty() {
        Some(relpath.to_string())
    } else {
        relpath
            .strip_prefix(&format!("{root}/"))
            .map(str::to_string)
    }
}

fn attr<'a>(node: &'a impl crate::graph::record::Properties, key: &str) -> Option<&'a str> {
    node.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn node_relpath(node: &impl crate::graph::record::Properties) -> Option<String> {
    attr(node, "relpath")
        .or_else(|| attr(node, "source_file"))
        .map(norm)
}

fn is_external(node: &crate::graph::graph::Node) -> bool {
    let attrs = &node.attributes;
    attrs
        .get("external")
        .is_some_and(|v| v == &Value::Bool(true))
        || attr(attrs, "type") == Some("external")
        || (attr(attrs, "file_type") == Some("concept")
            && node.id.as_str().is_some_and(|id| id.starts_with("ref_")))
}

fn symbol_name(label: &str) -> Option<String> {
    let name = label.trim().trim_end_matches("()").trim();
    if name.is_empty() || name.starts_with('.') || name.contains(char::is_whitespace) {
        return None;
    }
    Some(name.to_string())
}

fn node_kind(node: &impl crate::graph::record::Properties) -> String {
    if let Some(kind) = attr(node, "kind") {
        return kind.to_string();
    }
    if node.get("_callable_class") == Some(&Value::Bool(true)) {
        "class".into()
    } else if node.get("_callable") == Some(&Value::Bool(true)) {
        "function".into()
    } else {
        attr(node, "file_type").unwrap_or("code").to_string()
    }
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with("_test.go")
        || lower.split('/').any(|seg| {
            seg == "tests" || seg == "test" || seg == "__tests__" || seg.starts_with("test_")
        })
}

fn normalize_py_name(name: &str) -> String {
    name.to_ascii_lowercase().replace(['-', '.'], "_")
}

fn sanitize_go_id(module_path: &str) -> String {
    module_path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn node_package_of(module_path: &str) -> String {
    let mut segments = module_path.split('/');
    match segments.next() {
        Some(scope) if scope.starts_with('@') => match segments.next() {
            Some(name) => format!("{scope}/{name}"),
            None => scope.to_string(),
        },
        Some(name) => name.to_string(),
        None => String::new(),
    }
}

// ───────────────────────── manifests ─────────────────────────

fn toml_str<'a>(table: &'a toml::Value, path: &[&str]) -> Option<&'a str> {
    let mut cursor = table;
    for key in path {
        cursor = cursor.get(key)?;
    }
    cursor.as_str()
}

fn toml_strings(table: &toml::Value, path: &[&str]) -> Vec<String> {
    let mut cursor = table;
    for key in path {
        match cursor.get(key) {
            Some(next) => cursor = next,
            None => return vec![],
        }
    }
    cursor
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `name>=1.0,<2 ; python_version>"3"` → (`name`, `>=1.0,<2`).
fn split_requirement(spec: &str) -> Option<(String, String)> {
    let spec = spec.split(';').next()?.trim();
    let mut name_end = spec.len();
    for (i, c) in spec.char_indices() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
            name_end = i;
            break;
        }
    }
    let name = spec[..name_end].trim();
    if name.is_empty() {
        return None;
    }
    let mut rest = spec[name_end..].trim();
    if let Some(close) = rest.strip_prefix('[').and_then(|r| r.find(']')) {
        rest = rest[close + 2..].trim();
    }
    let rest = rest.trim_start_matches('(').trim_end_matches(')').trim();
    Some((normalize_py_name(name), rest.to_string()))
}

fn parse_pyproject(relpath: &str, text: &str) -> Option<PackageIdentity> {
    let doc: toml::Value = toml::from_str(text).ok()?;
    let root = dir_of(relpath);
    let mut dependencies = BTreeMap::new();
    let mut name = toml_str(&doc, &["project", "name"]).map(str::to_string);
    let mut version = toml_str(&doc, &["project", "version"]).map(str::to_string);
    for spec in toml_strings(&doc, &["project", "dependencies"]) {
        if let Some((dep, constraint)) = split_requirement(&spec) {
            dependencies.insert(dep, constraint);
        }
    }
    let mut packages: Vec<String> = toml_strings(&doc, &["tool", "setuptools", "packages"]);
    let mut source_prefix = None;
    for where_ in toml_strings(&doc, &["tool", "setuptools", "packages", "find", "where"]) {
        let where_ = norm(&where_);
        if !where_.is_empty() && where_ != "." {
            source_prefix = Some(where_);
        }
    }
    if let Some(dir) = doc
        .get("tool")
        .and_then(|t| t.get("setuptools"))
        .and_then(|t| t.get("package-dir"))
        .and_then(|d| d.get(""))
        .and_then(toml::Value::as_str)
    {
        let dir = norm(dir);
        if !dir.is_empty() && dir != "." {
            source_prefix = Some(dir);
        }
    }
    if let Some(poetry) = doc.get("tool").and_then(|t| t.get("poetry")) {
        if name.is_none() {
            name = poetry
                .get("name")
                .and_then(toml::Value::as_str)
                .map(str::to_string);
        }
        if version.is_none() {
            version = poetry
                .get("version")
                .and_then(toml::Value::as_str)
                .map(str::to_string);
        }
        if let Some(items) = poetry.get("packages").and_then(toml::Value::as_array) {
            for item in items {
                if let Some(include) = item.get("include").and_then(toml::Value::as_str) {
                    packages.push(include.to_string());
                }
                if let Some(from) = item.get("from").and_then(toml::Value::as_str) {
                    source_prefix = Some(norm(from));
                }
            }
        }
        if let Some(deps) = poetry.get("dependencies").and_then(toml::Value::as_table) {
            for (dep, constraint) in deps {
                if dep == "python" {
                    continue;
                }
                let constraint = match constraint {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Table(t) => t
                        .get("version")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    _ => String::new(),
                };
                dependencies.insert(normalize_py_name(dep), constraint);
            }
        }
    }
    let name = name?;
    Some(PackageIdentity {
        ecosystem: Ecosystem::Python,
        import_roots: packages
            .iter()
            .map(|p| p.split('.').next().unwrap_or(p).to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        name: normalize_py_name(&name),
        version,
        root,
        manifest: relpath.to_string(),
        dependencies,
        exports_map: BTreeMap::new(),
        main: None,
        source_prefix,
    })
}

/// Minimal INI reader for `setup.cfg`: `[section]`, `key = value`, and
/// indented continuation lines for multi-line values.
fn parse_ini(text: &str) -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut section = String::new();
    let mut key = String::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line
            .trim()
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
        {
            section = name.trim().to_string();
            key.clear();
            continue;
        }
        if line.starts_with([' ', '\t']) && !key.is_empty() {
            out.entry(section.clone())
                .or_default()
                .entry(key.clone())
                .or_default()
                .push(line.trim().to_string());
            continue;
        }
        if let Some((k, v)) = line.split_once('=').or_else(|| line.split_once(':')) {
            key = k.trim().to_string();
            let values = out
                .entry(section.clone())
                .or_default()
                .entry(key.clone())
                .or_default();
            let v = v.trim();
            if !v.is_empty() {
                values.push(v.to_string());
            }
        }
    }
    out
}

fn parse_setup_cfg(relpath: &str, text: &str) -> Option<PackageIdentity> {
    let ini = parse_ini(text);
    let first = |section: &str, key: &str| -> Option<String> {
        ini.get(section)?.get(key)?.first().cloned()
    };
    let all = |section: &str, key: &str| -> Vec<String> {
        ini.get(section)
            .and_then(|s| s.get(key))
            .cloned()
            .unwrap_or_default()
    };
    let name = first("metadata", "name")?;
    let version =
        first("metadata", "version").filter(|v| !v.starts_with("attr:") && !v.starts_with("file:"));
    let mut packages: Vec<String> = all("options", "packages")
        .into_iter()
        .flat_map(|line| {
            line.split(',')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|p| !p.is_empty() && !p.starts_with("find"))
        .collect();
    let mut source_prefix = None;
    for line in all("options", "package_dir") {
        if let Some((_, dir)) = line.split_once('=') {
            let dir = norm(dir.trim());
            if !dir.is_empty() && dir != "." {
                source_prefix = Some(dir);
            }
        }
    }
    for line in all("options.packages.find", "where") {
        let dir = norm(line.trim());
        if !dir.is_empty() && dir != "." {
            source_prefix = Some(dir);
        }
    }
    packages = packages
        .iter()
        .map(|p| p.split('.').next().unwrap_or(p).to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut dependencies = BTreeMap::new();
    for spec in all("options", "install_requires") {
        if let Some((dep, constraint)) = split_requirement(&spec) {
            dependencies.insert(dep, constraint);
        }
    }
    Some(PackageIdentity {
        ecosystem: Ecosystem::Python,
        name: normalize_py_name(&name),
        version,
        root: dir_of(relpath),
        manifest: relpath.to_string(),
        import_roots: packages,
        dependencies,
        exports_map: BTreeMap::new(),
        main: None,
        source_prefix,
    })
}

fn export_target(value: &Value) -> Option<String> {
    match value {
        Value::String(target) => Some(target.clone()),
        Value::Object(conditions) => ["import", "require", "default", "node", "browser", "types"]
            .iter()
            .filter_map(|k| conditions.get(*k))
            .find_map(export_target)
            .or_else(|| conditions.values().find_map(export_target)),
        Value::Array(items) => items.iter().find_map(export_target),
        _ => None,
    }
}

fn parse_package_json(relpath: &str, text: &str) -> Option<PackageIdentity> {
    let doc: Value = serde_json::from_str(text).ok()?;
    let name = doc.get("name")?.as_str()?.to_string();
    let version = doc
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut dependencies = BTreeMap::new();
    for section in [
        "dependencies",
        "peerDependencies",
        "optionalDependencies",
        "devDependencies",
    ] {
        if let Some(map) = doc.get(section).and_then(Value::as_object) {
            for (dep, constraint) in map {
                dependencies
                    .entry(dep.clone())
                    .or_insert_with(|| constraint.as_str().unwrap_or("").to_string());
            }
        }
    }
    let mut exports_map = BTreeMap::new();
    match doc.get("exports") {
        Some(Value::String(target)) => {
            exports_map.insert(".".to_string(), stem_of(target));
        }
        Some(Value::Object(map)) => {
            if map.keys().any(|k| k.starts_with('.')) {
                for (subpath, target) in map {
                    if !subpath.starts_with('.') || subpath.contains('*') {
                        continue;
                    }
                    if let Some(target) = export_target(target) {
                        exports_map.insert(subpath.clone(), stem_of(&target));
                    }
                }
            } else if let Some(target) = export_target(&Value::Object(map.clone())) {
                exports_map.insert(".".to_string(), stem_of(&target));
            }
        }
        _ => {}
    }
    let main = doc
        .get("module")
        .or_else(|| doc.get("main"))
        .and_then(Value::as_str)
        .map(stem_of);
    Some(PackageIdentity {
        ecosystem: Ecosystem::Node,
        import_roots: vec![name.clone()],
        name,
        version,
        root: dir_of(relpath),
        manifest: relpath.to_string(),
        dependencies,
        exports_map,
        main,
        source_prefix: None,
    })
}

/// Whether a manifest target (`dist/core`, `dist`) names the source file with
/// stem `stem` / `module_stem`: `Some(true)` on an exact path match,
/// `Some(false)` when only a build-directory ↔ source-directory swap matches.
fn target_matches(target: &str, stem: &str, module_stem: &str) -> Option<bool> {
    if target == stem || target == module_stem {
        return Some(true);
    }
    let strip = |value: &str, dirs: &[&str]| -> Option<String> {
        dirs.iter().find_map(|dir| {
            if value == *dir {
                Some(String::new())
            } else {
                value.strip_prefix(&format!("{dir}/")).map(str::to_string)
            }
        })
    };
    let target_tail = strip(target, JS_BUILD_DIRS)?;
    let tails = [
        strip(stem, JS_STRIP_DIRS),
        strip(module_stem, JS_STRIP_DIRS),
    ];
    tails
        .iter()
        .flatten()
        .any(|tail| *tail == target_tail)
        .then_some(false)
}

fn stem_of(target: &str) -> String {
    let target = norm(target);
    let stem = strip_ext(&target, &[".d.ts", ".d.mts", ".d.cts"])
        .or_else(|| strip_ext(&target, JS_EXTS))
        .unwrap_or(&target);
    let stem = stem.strip_suffix("/index").unwrap_or(stem);
    stem.trim_end_matches('/').to_string()
}

fn parse_go_mod(relpath: &str, text: &str) -> Option<PackageIdentity> {
    let mut module = None;
    let mut dependencies = BTreeMap::new();
    let mut in_require = false;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if in_require {
            if line == ")" {
                in_require = false;
                continue;
            }
            let mut parts = line.split_whitespace();
            if let (Some(path), Some(version)) = (parts.next(), parts.next()) {
                dependencies.insert(path.to_string(), version.to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("module ") {
            module = Some(rest.trim().trim_matches('"').to_string());
        } else if line == "require (" {
            in_require = true;
        } else if let Some(rest) = line.strip_prefix("require ") {
            let mut parts = rest.split_whitespace();
            if let (Some(path), Some(version)) = (parts.next(), parts.next()) {
                dependencies.insert(path.to_string(), version.to_string());
            }
        }
    }
    let module = module?;
    Some(PackageIdentity {
        ecosystem: Ecosystem::Go,
        import_roots: vec![module.clone()],
        name: module,
        version: None,
        root: dir_of(relpath),
        manifest: relpath.to_string(),
        dependencies,
        exports_map: BTreeMap::new(),
        main: None,
        source_prefix: None,
    })
}

/// Parse the manifests the caller supplied; unreadable or unknown files are skipped.
pub fn package_identities(manifests: &[PackageManifest]) -> Vec<PackageIdentity> {
    let mut out = Vec::new();
    for manifest in manifests {
        let relpath = norm(&manifest.relpath);
        let parsed = match file_name(&relpath) {
            "pyproject.toml" => parse_pyproject(&relpath, &manifest.text),
            "setup.cfg" => parse_setup_cfg(&relpath, &manifest.text),
            "package.json" => parse_package_json(&relpath, &manifest.text),
            "go.mod" => parse_go_mod(&relpath, &manifest.text),
            _ => None,
        };
        if let Some(identity) = parsed {
            // pyproject wins over setup.cfg for the same directory; merge roots.
            if let Some(existing) = out.iter_mut().find(|p: &&mut PackageIdentity| {
                p.ecosystem == identity.ecosystem && p.root == identity.root
            }) {
                for root in identity.import_roots {
                    if !existing.import_roots.contains(&root) {
                        existing.import_roots.push(root);
                    }
                }
                if existing.version.is_none() {
                    existing.version = identity.version;
                }
                if existing.source_prefix.is_none() {
                    existing.source_prefix = identity.source_prefix;
                }
                for (dep, constraint) in identity.dependencies {
                    existing.dependencies.entry(dep).or_insert(constraint);
                }
            } else {
                out.push(identity);
            }
        }
    }
    out
}

// ───────────────────────── exports ─────────────────────────

struct FileNode {
    index: usize,
    relpath: String,
}

fn file_nodes(graph: &Graph) -> Vec<FileNode> {
    let mut by_path: BTreeMap<String, usize> = BTreeMap::new();
    // A file node's id is the node whose `source_file` equals the path and
    // whose label is the file name, or (fallback) the first node per path.
    for (index, node) in graph.nodes().iter().enumerate() {
        let attrs = &node.attributes;
        if is_external(node) {
            continue;
        }
        let Some(relpath) = node_relpath(attrs) else {
            continue;
        };
        let label = attr(attrs, "label").unwrap_or("");
        let is_file = label == file_name(&relpath)
            || attr(attrs, "kind") == Some("file")
            || attr(attrs, "kind") == Some("module");
        match by_path.get(&relpath) {
            None => {
                by_path.insert(relpath, index);
            }
            Some(&existing) if is_file => {
                let existing_label =
                    attr(&graph.nodes()[existing].attributes, "label").unwrap_or("");
                if existing_label != file_name(&relpath) {
                    by_path.insert(relpath, index);
                }
            }
            _ => {}
        }
    }
    by_path
        .into_iter()
        .map(|(relpath, index)| FileNode { index, relpath })
        .collect()
}

fn symbols_of(graph: &Graph, file: &FileNode, ecosystem: Ecosystem) -> BTreeMap<String, SymbolRef> {
    let mut out = BTreeMap::new();
    // When no node is labelled with the file name, a symbol node stands in
    // for the file (Go `store/store.go` and `type Store` share one id); it
    // is then both the module node and an exported symbol.
    let file_label = attr(&graph.nodes()[file.index].attributes, "label").unwrap_or("");
    let file_is_symbol = file_label != file_name(&file.relpath);
    for (index, node) in graph.nodes().iter().enumerate() {
        if (index == file.index && !file_is_symbol) || is_external(node) {
            continue;
        }
        let attrs = &node.attributes;
        if node_relpath(attrs).as_deref() != Some(file.relpath.as_str()) {
            continue;
        }
        if attr(attrs, "file_type") == Some("rationale") || attr(attrs, "type") == Some("package") {
            continue;
        }
        let Some(name) = attr(attrs, "label").and_then(symbol_name) else {
            continue;
        };
        if ecosystem == Ecosystem::Go && !name.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        if ecosystem == Ecosystem::Python && name.starts_with('_') {
            continue;
        }
        let id = node_id_text(&node.id);
        out.entry(name.clone()).or_insert(SymbolRef {
            name,
            node_id: id,
            stable_id: attr(attrs, "stable_id").map(str::to_string),
            kind: node_kind(attrs),
            relpath: file.relpath.clone(),
        });
    }
    out
}

fn node_id_text(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python import roots when the manifest names none: top-level directories
/// (optionally below `src/`) that contain `__init__.py`, plus root modules.
fn infer_python_roots(
    files: &[FileNode],
    package: &PackageIdentity,
) -> (Vec<String>, Option<String>) {
    let mut roots = BTreeSet::new();
    let mut prefix = package.source_prefix.clone();
    let candidates: Vec<Option<String>> = match &prefix {
        Some(p) => vec![Some(p.clone())],
        None => vec![None, Some("src".to_string())],
    };
    for candidate in candidates {
        for file in files {
            let Some(rel) = under(&package.root, &file.relpath) else {
                continue;
            };
            let rel = match &candidate {
                Some(p) => match under(p, &rel) {
                    Some(r) => r,
                    None => continue,
                },
                None => rel,
            };
            let segments: Vec<&str> = rel.split('/').collect();
            if segments.len() == 2 && segments[1] == "__init__.py" {
                roots.insert(segments[0].to_string());
                if candidate.is_some() && prefix.is_none() {
                    prefix = candidate.clone();
                }
            }
        }
        if !roots.is_empty() {
            break;
        }
    }
    if roots.is_empty() {
        // Single-file distributions: `foo.py` at the package root.
        for file in files {
            if let Some(rel) = under(&package.root, &file.relpath)
                && !rel.contains('/')
                && let Some(stem) = strip_ext(&rel, PY_EXTS)
                && stem != "setup"
                && stem != "conftest"
            {
                roots.insert(stem.to_string());
            }
        }
    }
    (roots.into_iter().collect(), prefix)
}

fn insert_module(
    modules: &mut BTreeMap<String, ModuleExport>,
    module_path: String,
    export: ModuleExport,
) {
    match modules.get(&module_path) {
        Some(existing) if existing.exact() || !export.exact() => {}
        _ => {
            modules.insert(module_path, export);
        }
    }
}

/// Build the export index of `graph` for repository `repo`. `manifests` are
/// the manifest files of the repository (their text is not in the graph);
/// when none names a Python package, top-level packages are inferred from
/// `__init__.py` files.
pub fn exports(
    graph: &Graph,
    repo: &str,
    episode: Option<&str>,
    manifests: &[PackageManifest],
) -> ExportIndex {
    let mut packages = package_identities(manifests);
    let files = file_nodes(graph);
    if !packages.iter().any(|p| p.ecosystem == Ecosystem::Python)
        && files
            .iter()
            .any(|f| Ecosystem::of_file(&f.relpath) == Some(Ecosystem::Python))
    {
        // No Python manifest: identity is the inferred top-level package.
        let stub = PackageIdentity {
            ecosystem: Ecosystem::Python,
            name: String::new(),
            version: None,
            root: String::new(),
            manifest: String::new(),
            import_roots: vec![],
            dependencies: BTreeMap::new(),
            exports_map: BTreeMap::new(),
            main: None,
            source_prefix: None,
        };
        let (roots, prefix) = infer_python_roots(&files, &stub);
        if !roots.is_empty() {
            packages.push(PackageIdentity {
                name: roots.first().cloned().unwrap_or_default(),
                import_roots: roots,
                source_prefix: prefix,
                ..stub
            });
        }
    }
    for package in &mut packages {
        if package.ecosystem == Ecosystem::Python && package.import_roots.is_empty() {
            let (roots, prefix) = infer_python_roots(&files, package);
            package.import_roots = roots;
            if package.source_prefix.is_none() {
                package.source_prefix = prefix;
            }
        }
    }
    let mut modules = BTreeMap::new();
    for file in &files {
        let Some(ecosystem) = Ecosystem::of_file(&file.relpath) else {
            continue;
        };
        if is_test_path(&file.relpath) {
            continue;
        }
        let node = &graph.nodes()[file.index];
        let base = ModuleExport {
            module_path: String::new(),
            node_id: node_id_text(&node.id),
            stable_id: attr(&node.attributes, "stable_id").map(str::to_string),
            relpath: file.relpath.clone(),
            derivation: String::new(),
            symbols: BTreeMap::new(),
        };
        let mut symbols: Option<BTreeMap<String, SymbolRef>> = None;
        let mut paths: Vec<(String, &'static str)> = Vec::new();
        for package in packages.iter().filter(|p| p.ecosystem == ecosystem) {
            let Some(rel) = under(&package.root, &file.relpath) else {
                continue;
            };
            match ecosystem {
                Ecosystem::Python => {
                    let rel = match &package.source_prefix {
                        Some(prefix) => match under(prefix, &rel) {
                            Some(r) => r,
                            None => continue,
                        },
                        None => rel,
                    };
                    let Some(stem) = strip_ext(&rel, PY_EXTS) else {
                        continue;
                    };
                    let stem = stem.strip_suffix("/__init__").unwrap_or(stem);
                    let module_path = stem.replace('/', ".");
                    let first = module_path.split('.').next().unwrap_or("");
                    if package.import_roots.iter().any(|r| r == first) {
                        paths.push((module_path, "path"));
                    }
                }
                Ecosystem::Node => {
                    if rel.split('/').any(|seg| seg == "node_modules") {
                        continue;
                    }
                    let Some(stem) = strip_ext(&rel, JS_EXTS) else {
                        continue;
                    };
                    let stem = stem.strip_suffix(".d").unwrap_or(stem);
                    let module_stem = stem.strip_suffix("/index").unwrap_or(stem);
                    let name = &package.name;
                    if let Some(main) = &package.main
                        && let Some(exact) = target_matches(main, stem, module_stem)
                    {
                        paths.push((name.clone(), if exact { "main" } else { "main:build_dir" }));
                    }
                    for (subpath, target) in &package.exports_map {
                        let Some(exact) = target_matches(target, stem, module_stem) else {
                            continue;
                        };
                        let module_path = match subpath.as_str() {
                            "." => name.clone(),
                            sub => format!("{name}/{}", sub.trim_start_matches("./")),
                        };
                        paths.push((
                            module_path,
                            if exact {
                                "exports_map"
                            } else {
                                "exports_map:build_dir"
                            },
                        ));
                    }
                    paths.push((format!("{name}/{module_stem}"), "path"));
                    if stem == "index"
                        && package.main.is_none()
                        && !package.exports_map.contains_key(".")
                    {
                        paths.push((name.clone(), "index"));
                    }
                    for dir in JS_STRIP_DIRS {
                        if let Some(short) = module_stem.strip_prefix(&format!("{dir}/")) {
                            paths.push((format!("{name}/{short}"), "src_strip"));
                        } else if module_stem == *dir
                            && package.main.is_none()
                            && !package.exports_map.contains_key(".")
                        {
                            paths.push((name.clone(), "src_strip:index"));
                        }
                    }
                }
                Ecosystem::Go => {
                    let dir = dir_of(&rel);
                    let module_path = if dir.is_empty() {
                        package.name.clone()
                    } else {
                        format!("{}/{dir}", package.name)
                    };
                    paths.push((module_path, "path"));
                }
            }
        }
        if paths.is_empty() {
            continue;
        }
        let symbols = symbols.get_or_insert_with(|| symbols_of(graph, file, ecosystem));
        for (module_path, derivation) in paths {
            let mut export = base.clone();
            export.module_path = module_path.clone();
            export.derivation = derivation.to_string();
            if ecosystem == Ecosystem::Go {
                // A Go package spans every file of its directory: merge symbols
                // and keep the first file as the module node.
                match modules.get_mut(&module_path) {
                    Some(existing) => {
                        let existing: &mut ModuleExport = existing;
                        for (name, symbol) in symbols.iter() {
                            existing
                                .symbols
                                .entry(name.clone())
                                .or_insert(symbol.clone());
                        }
                        continue;
                    }
                    None => {
                        export.symbols = symbols.clone();
                        modules.insert(module_path, export);
                    }
                }
            } else {
                export.symbols = symbols.clone();
                insert_module(&mut modules, module_path, export);
            }
        }
    }
    ExportIndex {
        repo: repo.to_string(),
        episode: episode.map(str::to_string),
        packages,
        modules,
    }
}

// ───────────────────────── import statements ─────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
enum ImportShape {
    /// `import a.b`, `import * as ns from`, `require('x')`, side-effect import.
    Whole,
    /// `from a.b import x, y` / `import { x, y } from` — named symbols.
    Named(Vec<String>),
    /// `from a.b import *`.
    Wildcard,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ImportSpec {
    module: String,
    shape: ImportShape,
    line: usize,
    /// Go: alias or last path segment used for qualified identifiers.
    alias: Option<String>,
}

fn logical_lines(text: &str) -> Vec<(usize, String)> {
    // Join Python parenthesised / backslash-continued import statements.
    let mut out = Vec::new();
    let mut buffer = String::new();
    let mut start = 0;
    let mut depth = 0i32;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if buffer.is_empty() {
            start = index + 1;
        }
        let mut piece = line.to_string();
        let continued = piece.ends_with('\\');
        if continued {
            piece.pop();
        }
        depth += piece.matches('(').count() as i32 - piece.matches(')').count() as i32;
        buffer.push_str(piece.trim());
        buffer.push(' ');
        if depth <= 0 && !continued {
            out.push((start, std::mem::take(&mut buffer)));
            depth = 0;
        }
    }
    if !buffer.is_empty() {
        out.push((start, buffer));
    }
    out
}

fn python_imports(text: &str) -> Vec<ImportSpec> {
    let mut out = Vec::new();
    for (line, statement) in logical_lines(text) {
        let statement = statement.split('#').next().unwrap_or("").trim();
        if let Some(rest) = statement.strip_prefix("from ") {
            let Some((module, names)) = rest.split_once(" import ") else {
                continue;
            };
            let module = module.trim();
            if module.starts_with('.') || module.is_empty() {
                continue;
            }
            let names = names.trim().trim_matches(['(', ')']).trim();
            let shape = if names == "*" {
                ImportShape::Wildcard
            } else {
                ImportShape::Named(
                    names
                        .split(',')
                        .map(|n| n.split_whitespace().next().unwrap_or("").to_string())
                        .filter(|n| !n.is_empty())
                        .collect(),
                )
            };
            out.push(ImportSpec {
                module: module.to_string(),
                shape,
                line,
                alias: None,
            });
        } else if let Some(rest) = statement.strip_prefix("import ") {
            for item in rest.split(',') {
                let module = item.split_whitespace().next().unwrap_or("");
                if module.is_empty() || module.starts_with('.') {
                    continue;
                }
                out.push(ImportSpec {
                    module: module.to_string(),
                    shape: ImportShape::Whole,
                    line,
                    alias: None,
                });
            }
        }
    }
    out
}

fn js_regexes() -> &'static (Regex, Regex, Regex) {
    static RE: OnceLock<(Regex, Regex, Regex)> = OnceLock::new();
    RE.get_or_init(|| {
        (
            // import <clause> from '<spec>'  |  export <clause> from '<spec>'  |  import '<spec>'
            Regex::new(
                r#"(?s)\b(?:import|export)\s+(?:type\s+)?(?:([^;'"]*?)\s+from\s+)?['"]([^'"]+)['"]"#,
            )
            .unwrap(),
            Regex::new(r#"\brequire\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap(),
            Regex::new(r#"\bimport\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap(),
        )
    })
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

fn js_clause_names(clause: &str) -> ImportShape {
    let clause = clause.trim();
    if clause.is_empty() {
        return ImportShape::Whole;
    }
    let mut names = Vec::new();
    let mut whole = false;
    if let (Some(open), Some(close)) = (clause.find('{'), clause.rfind('}')) {
        for item in clause[open + 1..close].split(',') {
            let item = item.trim();
            let item = item.strip_prefix("type ").unwrap_or(item).trim();
            let name = item.split_whitespace().next().unwrap_or("");
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
        let outside = format!("{}{}", &clause[..open], &clause[close + 1..]);
        if outside.contains('*') || outside.split(',').any(|s| !s.trim().is_empty()) {
            whole = true;
        }
    } else {
        whole = true;
    }
    let _ = whole;
    if names.is_empty() {
        ImportShape::Whole
    } else if names.iter().any(|n| n == "*") {
        ImportShape::Wildcard
    } else {
        ImportShape::Named(names)
    }
}

fn js_imports(text: &str) -> Vec<ImportSpec> {
    let (import_re, require_re, dynamic_re) = js_regexes();
    let mut out = Vec::new();
    let bare =
        |spec: &str| !(spec.starts_with('.') || spec.starts_with('/') || spec.starts_with("node:"));
    for captures in import_re.captures_iter(text) {
        let spec = &captures[2];
        if !bare(spec) {
            continue;
        }
        let shape = captures
            .get(1)
            .map(|m| js_clause_names(m.as_str()))
            .unwrap_or(ImportShape::Whole);
        out.push(ImportSpec {
            module: spec.to_string(),
            shape,
            line: line_of(text, captures.get(0).unwrap().start()),
            alias: None,
        });
    }
    for re in [require_re, dynamic_re] {
        for captures in re.captures_iter(text) {
            let spec = &captures[1];
            if !bare(spec) {
                continue;
            }
            out.push(ImportSpec {
                module: spec.to_string(),
                shape: ImportShape::Whole,
                line: line_of(text, captures.get(0).unwrap().start()),
                alias: None,
            });
        }
    }
    out
}

fn go_package_alias(path: &str) -> String {
    let mut segments: Vec<&str> = path.split('/').collect();
    while let Some(last) = segments.last()
        && last.len() > 1
        && last.starts_with('v')
        && last[1..].chars().all(|c| c.is_ascii_digit())
        && segments.len() > 1
    {
        segments.pop();
    }
    let last = segments.last().copied().unwrap_or(path);
    last.split('.')
        .next()
        .unwrap_or(last)
        .trim_start_matches("go-")
        .to_string()
}

fn go_imports(text: &str) -> Vec<ImportSpec> {
    let mut specs = Vec::new();
    let mut in_block = false;
    let mut aliases: Vec<(String, usize)> = Vec::new();
    let parse_entry = |entry: &str| -> Option<(Option<String>, String)> {
        let entry = entry.split("//").next().unwrap_or("").trim();
        let (alias, path) = match entry.find('"') {
            Some(at) => {
                let alias = entry[..at].trim();
                let path = entry[at..].trim_matches('"');
                ((!alias.is_empty()).then(|| alias.to_string()), path)
            }
            None => return None,
        };
        (!path.is_empty()).then(|| (alias, path.to_string()))
    };
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if in_block {
            if line.starts_with(')') {
                in_block = false;
                continue;
            }
            if let Some((alias, path)) = parse_entry(line) {
                specs.push((alias, path, index + 1));
            }
            continue;
        }
        if line == "import (" {
            in_block = true;
        } else if let Some(rest) = line.strip_prefix("import ")
            && let Some((alias, path)) = parse_entry(rest)
        {
            specs.push((alias, path, index + 1));
        }
    }
    let mut out = Vec::new();
    for (alias, path, line) in specs {
        if !path.contains('.') && !path.contains('/')
            || !path.split('/').next().unwrap_or("").contains('.')
        {
            // Standard library (`fmt`, `net/http`): no exporter can claim it.
            continue;
        }
        let alias = match alias {
            Some(a) if a == "_" || a == "." => None,
            Some(a) => Some(a),
            None => Some(go_package_alias(&path)),
        };
        if let Some(a) = &alias {
            aliases.push((a.clone(), out.len()));
        }
        out.push(ImportSpec {
            module: path,
            shape: ImportShape::Whole,
            line,
            alias,
        });
    }
    // Qualified identifiers `alias.Name` turn a whole-package import into a
    // named one; Go exports are the capitalised identifiers.
    for (alias, at) in aliases {
        let pattern = Regex::new(&format!(
            r"\b{}\.([A-Z][A-Za-z0-9_]*)",
            regex::escape(&alias)
        ))
        .unwrap();
        let mut names: Vec<String> = pattern
            .captures_iter(text)
            .map(|c| c[1].to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !names.is_empty() {
            names.sort();
            out[at].shape = ImportShape::Named(names);
        }
    }
    out
}

fn imports_of(ecosystem: Ecosystem, text: &str) -> Vec<ImportSpec> {
    match ecosystem {
        Ecosystem::Python => python_imports(text),
        Ecosystem::Node => js_imports(text),
        Ecosystem::Go => go_imports(text),
    }
}

/// Whether a parsed import statement is the one behind the external stub node
/// the extractor produced.
fn spec_matches_target(
    ecosystem: Ecosystem,
    spec: &ImportSpec,
    target_id: &str,
    target_label: &str,
) -> bool {
    match ecosystem {
        Ecosystem::Python => {
            let id = spec.module.replace('.', "_");
            id == target_id || target_id == spec.module
        }
        Ecosystem::Node => {
            let package = node_package_of(&spec.module);
            package == target_label
                || target_id == format!("ref_{}", sanitize_go_id(&package).trim_start_matches('_'))
                || spec.module == target_label
        }
        Ecosystem::Go => {
            format!("go_pkg_{}", sanitize_go_id(&spec.module)) == target_id
                || target_label == spec.module
        }
    }
}

// ───────────────────────── versions ─────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    parts: Vec<u64>,
    pre: Option<String>,
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches(['v', 'V', '=']);
        if text.is_empty() {
            return None;
        }
        let (core, pre) = match text.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
            Some(at) => (
                &text[..at],
                Some(text[at..].trim_start_matches(['-', '.']).to_string()),
            ),
            None => (text, None),
        };
        let parts: Option<Vec<u64>> = core
            .split('.')
            .filter(|p| !p.is_empty())
            .map(|p| p.parse::<u64>().ok())
            .collect();
        let parts = parts?;
        if parts.is_empty() {
            return None;
        }
        Some(Version {
            parts,
            pre: pre.filter(|p| !p.is_empty() && !p.starts_with('+')),
        })
    }
    fn part(&self, index: usize) -> u64 {
        self.parts.get(index).copied().unwrap_or(0)
    }
    fn bump(&self, index: usize) -> Version {
        let mut parts: Vec<u64> = (0..=index).map(|i| self.part(i)).collect();
        parts[index] += 1;
        Version { parts, pre: None }
    }
    fn matches_prefix(&self, prefix: &Version) -> bool {
        (0..prefix.parts.len()).all(|i| self.part(i) == prefix.part(i))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let width = self.parts.len().max(other.parts.len());
        for i in 0..width {
            match self.part(i).cmp(&other.part(i)) {
                std::cmp::Ordering::Equal => continue,
                ordering => return ordering,
            }
        }
        match (&self.pre, &other.pre) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VersionCheck {
    Satisfied,
    Unsatisfied,
    Unchecked,
}

fn compare(op: &str, version: &Version, bound: &Version) -> Option<bool> {
    Some(match op {
        ">=" => version >= bound,
        "<=" => version <= bound,
        ">" => version > bound,
        "<" => version < bound,
        "==" | "=" | "===" => version.cmp(bound) == std::cmp::Ordering::Equal,
        "!=" => version.cmp(bound) != std::cmp::Ordering::Equal,
        _ => return None,
    })
}

fn split_op(clause: &str) -> (&str, &str) {
    for op in ["===", ">=", "<=", "==", "!=", "~=", ">", "<", "=", "^", "~"] {
        if let Some(rest) = clause.strip_prefix(op) {
            return (op, rest.trim());
        }
    }
    ("", clause.trim())
}

fn python_satisfies(constraint: &str, version: &Version) -> VersionCheck {
    let constraint = constraint.split(';').next().unwrap_or("").trim();
    if constraint.is_empty() {
        return VersionCheck::Satisfied;
    }
    for clause in constraint.split(',') {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        let (op, value) = split_op(clause);
        let wildcard = value.ends_with(".*");
        let value = value.trim_end_matches(".*");
        let Some(bound) = Version::parse(value) else {
            return VersionCheck::Unchecked;
        };
        let ok = match op {
            "~=" => {
                let index = bound.parts.len().saturating_sub(2);
                version >= &bound && version < &bound.bump(index)
            }
            "==" | "===" if wildcard => version.matches_prefix(&bound),
            "!=" if wildcard => !version.matches_prefix(&bound),
            "" => version.cmp(&bound) == std::cmp::Ordering::Equal,
            op => match compare(op, version, &bound) {
                Some(ok) => ok,
                None => return VersionCheck::Unchecked,
            },
        };
        if !ok {
            return VersionCheck::Unsatisfied;
        }
    }
    VersionCheck::Satisfied
}

fn node_range_satisfies(range: &str, version: &Version) -> Option<bool> {
    let range = range.trim();
    if range.is_empty() || range == "*" || range == "latest" || range == "x" {
        return Some(true);
    }
    if let Some((low, high)) = range.split_once(" - ") {
        let low = Version::parse(low.trim())?;
        let high = Version::parse(high.trim())?;
        let upper = if high.parts.len() < 3 {
            version < &high.bump(high.parts.len() - 1)
        } else {
            version <= &high
        };
        return Some(version >= &low && upper);
    }
    for clause in range.split_whitespace() {
        let (op, value) = split_op(clause);
        let wildcard = value.ends_with(".x") || value.ends_with(".*") || value.ends_with(".X");
        let value = value
            .trim_end_matches(".x")
            .trim_end_matches(".X")
            .trim_end_matches(".*");
        let bound = Version::parse(value)?;
        let ok = match op {
            "^" => {
                let index = bound
                    .parts
                    .iter()
                    .position(|&p| p != 0)
                    .unwrap_or(bound.parts.len() - 1)
                    .min(bound.parts.len() - 1);
                version >= &bound && version < &bound.bump(index)
            }
            "~" => {
                let index = if bound.parts.len() >= 2 { 1 } else { 0 };
                version >= &bound && version < &bound.bump(index)
            }
            "" | "=" | "==" => {
                if wildcard || bound.parts.len() < 3 {
                    version.matches_prefix(&bound)
                } else {
                    version.cmp(&bound) == std::cmp::Ordering::Equal
                }
            }
            op => compare(op, version, &bound)?,
        };
        if !ok {
            return Some(false);
        }
    }
    Some(true)
}

fn node_satisfies(constraint: &str, version: &Version) -> VersionCheck {
    let constraint = constraint.trim();
    if constraint.is_empty() {
        return VersionCheck::Satisfied;
    }
    if constraint.contains(':') || constraint.contains('/') {
        // workspace:, file:, npm:, git URLs — not a version range.
        return VersionCheck::Unchecked;
    }
    let mut any_checked = false;
    for range in constraint.split("||") {
        match node_range_satisfies(range, version) {
            Some(true) => return VersionCheck::Satisfied,
            Some(false) => any_checked = true,
            None => {}
        }
    }
    if any_checked {
        VersionCheck::Unsatisfied
    } else {
        VersionCheck::Unchecked
    }
}

fn go_satisfies(required: &str, version: &Version) -> VersionCheck {
    // Minimal version selection: the built module is at least the required
    // version within the same major (module paths carry /vN for v2+).
    match Version::parse(required) {
        Some(bound) => {
            if version.part(0) != bound.part(0) {
                VersionCheck::Unsatisfied
            } else if version >= &bound {
                VersionCheck::Satisfied
            } else {
                VersionCheck::Unsatisfied
            }
        }
        None => VersionCheck::Unchecked,
    }
}

fn check_version(
    ecosystem: Ecosystem,
    constraint: Option<&str>,
    exporter: Option<&str>,
) -> (VersionCheck, &'static str) {
    let Some(constraint) = constraint
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != "*")
    else {
        return (VersionCheck::Satisfied, "unconstrained");
    };
    let Some(exporter) = exporter.and_then(Version::parse) else {
        return (VersionCheck::Unchecked, "unchecked");
    };
    let check = match ecosystem {
        Ecosystem::Python => python_satisfies(constraint, &exporter),
        Ecosystem::Node => node_satisfies(constraint, &exporter),
        Ecosystem::Go => go_satisfies(constraint, &exporter),
    };
    match check {
        VersionCheck::Satisfied => (check, "satisfied"),
        VersionCheck::Unsatisfied => (check, "unsatisfied"),
        VersionCheck::Unchecked => (check, "unchecked"),
    }
}

// ───────────────────────── link ─────────────────────────

/// Source text of an importing file by repository-relative path; `None` when
/// unavailable (the linker then falls back to id matching, `INFERRED`).
pub type SourceLoader<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// No source text available.
pub fn no_sources(_: &str) -> Option<String> {
    None
}

/// Load sources from files below `root`.
pub fn sources_below(root: &Path) -> impl Fn(&str) -> Option<String> + '_ {
    move |relpath: &str| {
        if relpath.split('/').any(|seg| seg == "..") {
            return None;
        }
        std::fs::read_to_string(join_below(root, relpath)?).ok()
    }
}

/// `root.join(relpath)` for a relpath from graph JSON. On Windows only when
/// every component is a plain name (a leading `./` aside), so that a drive or
/// UNC prefix (`C:/x`, `//host/share`), a rooted path or `..` (also spelt with
/// `\`) cannot leave `root`. On Linux and macOS it is `root.join(relpath)`.
pub(crate) fn join_below(root: &Path, relpath: &str) -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        use std::path::Component;
        let below = Path::new(relpath)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
        if !below {
            return None;
        }
    }
    Some(root.join(relpath))
}

struct Importer<'a> {
    repo: &'a str,
    episode: Option<&'a str>,
    packages: Vec<PackageIdentity>,
}

impl Importer<'_> {
    fn constraint(&self, ecosystem: Ecosystem, package: &str) -> Option<String> {
        let key = match ecosystem {
            Ecosystem::Python => normalize_py_name(package),
            _ => package.to_string(),
        };
        self.packages
            .iter()
            .filter(|p| p.ecosystem == ecosystem)
            .find_map(|p| {
                if ecosystem == Ecosystem::Go {
                    // `require example.com/mod v1` covers every package below it.
                    p.dependencies
                        .iter()
                        .filter(|(dep, _)| owns(Ecosystem::Go, dep, &key))
                        .max_by_key(|(dep, _)| dep.len())
                        .map(|(_, constraint)| constraint.clone())
                } else {
                    p.dependencies.get(&key).cloned()
                }
            })
    }
    fn declares(&self, ecosystem: Ecosystem, package: &str) -> bool {
        self.constraint(ecosystem, package).is_some()
    }
    /// Whether one of the importer's own packages owns `module_path`.
    fn owns_module(&self, ecosystem: Ecosystem, module_path: &str) -> bool {
        self.packages
            .iter()
            .filter(|p| p.ecosystem == ecosystem)
            .any(|p| {
                p.import_roots
                    .iter()
                    .any(|root| owns(ecosystem, root, module_path))
            })
    }
    /// Whether the importer's manifests say anything about dependencies.
    fn knows_dependencies(&self, ecosystem: Ecosystem) -> bool {
        self.packages
            .iter()
            .any(|p| p.ecosystem == ecosystem && !p.dependencies.is_empty())
    }
}

fn package_name_of(ecosystem: Ecosystem, module_path: &str) -> String {
    match ecosystem {
        Ecosystem::Python => module_path
            .split('.')
            .next()
            .unwrap_or(module_path)
            .to_string(),
        Ecosystem::Node => node_package_of(module_path),
        Ecosystem::Go => module_path.to_string(),
    }
}

/// Link every external import of `graph` (repository `repo`) against
/// `indexes`. `manifests` are the importing repository's own manifests (for
/// declared dependency constraints); `source_of` supplies importing file text.
pub fn link(
    graph: &Graph,
    repo: &str,
    episode: Option<&str>,
    manifests: &[PackageManifest],
    indexes: &[ExportIndex],
    source_of: &SourceLoader<'_>,
) -> LinkReport {
    let importer = Importer {
        repo,
        episode,
        packages: package_identities(manifests),
    };
    let indexes: Vec<&ExportIndex> = indexes.iter().filter(|i| i.repo != repo).collect();
    let mut report = LinkReport {
        repo: repo.to_string(),
        episode: episode.map(str::to_string),
        ..Default::default()
    };
    let mut sources: BTreeMap<String, Option<Vec<ImportSpec>>> = BTreeMap::new();
    let mut seen_edges: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut seen_unresolved: BTreeSet<(String, String, Option<String>, String)> = BTreeSet::new();
    // (source node, external target) pairs; a multigraph may repeat them.
    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    let nodes = graph.nodes();
    for (source, target, edge) in graph.iter_edges() {
        let relation = attr(&edge.attributes, "relation").unwrap_or("");
        if !IMPORT_RELATIONS.contains(&relation) || !is_external(&nodes[target]) {
            continue;
        }
        pairs.insert((source, target));
    }
    for (source, target) in pairs {
        report.external_imports += 1;
        let source_node = &nodes[source];
        let target_node = &nodes[target];
        let Some(relpath) = node_relpath(&source_node.attributes) else {
            continue;
        };
        let Some(ecosystem) = Ecosystem::of_file(&relpath) else {
            continue;
        };
        let target_id = node_id_text(&target_node.id);
        let target_label = attr(&target_node.attributes, "label")
            .unwrap_or("")
            .to_string();
        let specs = sources.entry(relpath.clone()).or_insert_with(|| {
            let text = source_of(&relpath);
            match text {
                Some(text) => {
                    report.files_with_source += 1;
                    Some(imports_of(ecosystem, &text))
                }
                None => {
                    report.files_without_source += 1;
                    None
                }
            }
        });
        let from = EdgeEnd {
            repo: repo.to_string(),
            node_id: node_id_text(&source_node.id),
            stable_id: attr(&source_node.attributes, "stable_id").map(str::to_string),
            episode: importer.episode.map(str::to_string),
            relpath: Some(relpath.clone()),
        };
        let candidates: Vec<ImportSpec> = match specs {
            Some(specs) => specs
                .iter()
                .filter(|s| spec_matches_target(ecosystem, s, &target_id, &target_label))
                .cloned()
                .collect(),
            None => vec![],
        };
        let recovered = candidates.is_empty();
        let candidates = if recovered {
            recover_specs(ecosystem, &indexes, &target_id, &target_label)
        } else {
            candidates
        };
        for spec in candidates {
            resolve_spec(
                &importer,
                &indexes,
                ecosystem,
                &from,
                &spec,
                recovered,
                &mut report,
                &mut seen_edges,
                &mut seen_unresolved,
            );
        }
    }
    report
}

/// Without source text the module path is recovered from the stub id when
/// exactly one exported module path maps onto it.
fn recover_specs(
    ecosystem: Ecosystem,
    indexes: &[&ExportIndex],
    target_id: &str,
    target_label: &str,
) -> Vec<ImportSpec> {
    let mut matches: BTreeSet<String> = BTreeSet::new();
    for index in indexes {
        for module_path in index.modules.keys() {
            let hit = match ecosystem {
                Ecosystem::Python => module_path.replace('.', "_") == target_id,
                Ecosystem::Node => module_path == target_label,
                Ecosystem::Go => format!("go_pkg_{}", sanitize_go_id(module_path)) == target_id,
            };
            if hit {
                matches.insert(module_path.clone());
            }
        }
    }
    if matches.is_empty() && ecosystem == Ecosystem::Node && !target_label.is_empty() {
        matches.insert(target_label.to_string());
    }
    matches
        .into_iter()
        .map(|module| ImportSpec {
            module,
            shape: ImportShape::Whole,
            line: 0,
            alias: None,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn resolve_spec(
    importer: &Importer<'_>,
    indexes: &[&ExportIndex],
    ecosystem: Ecosystem,
    from: &EdgeEnd,
    spec: &ImportSpec,
    recovered: bool,
    report: &mut LinkReport,
    seen_edges: &mut BTreeSet<(String, String, String)>,
    seen_unresolved: &mut BTreeSet<(String, String, Option<String>, String)>,
) {
    let line = (spec.line > 0).then_some(spec.line);
    let package = package_name_of(ecosystem, &spec.module);
    let mut unresolved = |report: &mut LinkReport,
                          module_path: &str,
                          symbol: Option<String>,
                          reason: &str,
                          candidates: Vec<String>,
                          constraint: Option<String>,
                          exporter_version: Option<String>,
                          package: &str| {
        if seen_unresolved.insert((
            from.node_id.clone(),
            module_path.to_string(),
            symbol.clone(),
            reason.to_string(),
        )) {
            report.unresolved.push(UnresolvedImport {
                repo: importer.repo.to_string(),
                node_id: from.node_id.clone(),
                stable_id: from.stable_id.clone(),
                relpath: from.relpath.clone(),
                ecosystem,
                module_path: module_path.to_string(),
                symbol,
                package: package.to_string(),
                reason: reason.to_string(),
                candidates,
                version_constraint: constraint,
                exporter_version,
                line,
            });
        }
    };
    let claimants: Vec<(&ExportIndex, &PackageIdentity)> = indexes
        .iter()
        .filter_map(|index| index.claims(ecosystem, &spec.module).map(|p| (*index, p)))
        .collect();
    if claimants.is_empty() {
        let declared = importer.declares(ecosystem, &package);
        if !declared {
            if importer.owns_module(ecosystem, &spec.module) {
                report.skipped_internal += 1;
                return;
            }
            if is_stdlib(ecosystem, &spec.module) {
                report.skipped_stdlib += 1;
                return;
            }
            if importer.knows_dependencies(ecosystem) {
                report.skipped_undeclared += 1;
                return;
            }
        }
        let reason = if declared {
            REASON_DECLARED_NOT_INDEXED
        } else {
            REASON_NO_EXPORTER
        };
        unresolved(
            report,
            &spec.module,
            None,
            reason,
            vec![],
            importer.constraint(ecosystem, &package),
            None,
            &package,
        );
        return;
    }
    // The declared dependency name is the exporter's package name, which may
    // differ from the import root (Python `dist-name` vs `import_root`).
    let mut viable: Vec<(&ExportIndex, &PackageIdentity, Option<String>, &'static str)> =
        Vec::new();
    let mut mismatched: Vec<(&ExportIndex, &PackageIdentity, Option<String>)> = Vec::new();
    for (index, exporter) in &claimants {
        let constraint = importer
            .constraint(ecosystem, &exporter.name)
            .or_else(|| importer.constraint(ecosystem, &package));
        let (check, status) = check_version(
            ecosystem,
            constraint.as_deref(),
            exporter.version.as_deref(),
        );
        if check == VersionCheck::Unsatisfied {
            mismatched.push((index, exporter, constraint));
        } else {
            viable.push((index, exporter, constraint, status));
        }
    }
    if viable.is_empty() {
        for (index, exporter, constraint) in mismatched {
            unresolved(
                report,
                &spec.module,
                None,
                REASON_VERSION,
                vec![index.repo.clone()],
                constraint,
                exporter.version.clone(),
                &exporter.name,
            );
        }
        return;
    }
    if viable.len() > 1 {
        let candidates: Vec<String> = viable.iter().map(|(i, ..)| i.repo.clone()).collect();
        unresolved(
            report,
            &spec.module,
            None,
            REASON_AMBIGUOUS,
            candidates,
            importer.constraint(ecosystem, &package),
            None,
            &package,
        );
        return;
    }
    let (index, exporter, constraint, version_status) = viable.remove(0);
    let Some(module) = index.modules.get(&spec.module) else {
        unresolved(
            report,
            &spec.module,
            None,
            REASON_MODULE,
            vec![index.repo.clone()],
            constraint,
            exporter.version.clone(),
            &exporter.name,
        );
        return;
    };
    let mut push_edge = |report: &mut LinkReport,
                         target: EdgeEnd,
                         relation: &str,
                         exact: bool,
                         derivation: String,
                         symbol: Option<String>| {
        if !seen_edges.insert((
            from.node_id.clone(),
            target.node_id.clone(),
            relation.to_string(),
        )) {
            return;
        }
        report.edges.push(CrossEdge {
            source: from.clone(),
            target,
            relation: relation.to_string(),
            confidence: if exact && !recovered && module.exact() {
                EXTRACTED
            } else {
                INFERRED
            }
            .to_string(),
            derivation,
            module_path: spec.module.clone(),
            symbol,
            version_constraint: constraint.clone(),
            exporter_version: exporter.version.clone(),
            version_status: version_status.to_string(),
            line,
        });
    };
    let module_end = || EdgeEnd {
        repo: index.repo.clone(),
        node_id: module.node_id.clone(),
        stable_id: module.stable_id.clone(),
        episode: index.episode.clone(),
        relpath: Some(module.relpath.clone()),
    };
    let base_derivation = if recovered {
        format!("id_match:{}", module.derivation)
    } else {
        format!("import_statement:{}", module.derivation)
    };
    match &spec.shape {
        ImportShape::Whole => push_edge(
            report,
            module_end(),
            RELATION_MODULE,
            true,
            format!("{base_derivation}:whole_module"),
            None,
        ),
        ImportShape::Wildcard => push_edge(
            report,
            module_end(),
            RELATION_MODULE,
            true,
            format!("{base_derivation}:wildcard"),
            None,
        ),
        ImportShape::Named(names) => {
            let mut missing = false;
            for name in names {
                if let Some(symbol) = module.symbols.get(name) {
                    let target = EdgeEnd {
                        repo: index.repo.clone(),
                        node_id: symbol.node_id.clone(),
                        stable_id: symbol.stable_id.clone(),
                        episode: index.episode.clone(),
                        relpath: Some(symbol.relpath.clone()),
                    };
                    push_edge(
                        report,
                        target,
                        RELATION_SYMBOL,
                        true,
                        format!("{base_derivation}:symbol"),
                        Some(name.clone()),
                    );
                    continue;
                }
                // `from pkg.sub import mod` names a submodule, not a symbol.
                let submodule_path = match ecosystem {
                    Ecosystem::Python => format!("{}.{name}", spec.module),
                    _ => format!("{}/{name}", spec.module),
                };
                if let Some(sub) = index.modules.get(&submodule_path) {
                    let target = EdgeEnd {
                        repo: index.repo.clone(),
                        node_id: sub.node_id.clone(),
                        stable_id: sub.stable_id.clone(),
                        episode: index.episode.clone(),
                        relpath: Some(sub.relpath.clone()),
                    };
                    push_edge(
                        report,
                        target,
                        RELATION_MODULE,
                        sub.exact(),
                        format!("{base_derivation}:submodule"),
                        Some(name.clone()),
                    );
                    continue;
                }
                missing = true;
                unresolved(
                    report,
                    &spec.module,
                    Some(name.clone()),
                    REASON_SYMBOL,
                    vec![index.repo.clone()],
                    constraint.clone(),
                    exporter.version.clone(),
                    &exporter.name,
                );
            }
            if missing || names.is_empty() {
                // The module is still imported; the symbol could not be seen.
                push_edge(
                    report,
                    module_end(),
                    RELATION_MODULE,
                    false,
                    format!("{base_derivation}:symbol_missing"),
                    None,
                );
            }
        }
    }
}

// ───────────────────────── JSON boundary ─────────────────────────

fn manifests_from(value: &Value) -> Result<Vec<PackageManifest>> {
    match value {
        Value::Null => Ok(vec![]),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                let relpath = item["relpath"]
                    .as_str()
                    .ok_or_else(|| Error::invalid("Expected manifests[].relpath"))?;
                let text = item["text"]
                    .as_str()
                    .ok_or_else(|| Error::invalid("Expected manifests[].text"))?;
                Ok(PackageManifest::new(relpath, text))
            })
            .collect(),
        _ => Err(Error::invalid("Expected manifests to be an array")),
    }
}

fn graph_from(request: &Value, key_graph: &str, key_ref: &str, key_path: &str) -> Result<Graph> {
    let reference = request.get(key_ref).filter(|v| !v.is_null());
    let inline = request.get(key_graph).filter(|v| !v.is_null());
    let path = request.get(key_path).filter(|v| !v.is_null());
    match (reference, inline, path) {
        (Some(Value::String(reference)), None, None) if !reference.is_empty() => {
            Ok((*session::registry().get(reference)?).clone())
        }
        (None, Some(graph), None) => Graph::from_node_link(graph, session::edges_key(request)?),
        (None, None, Some(Value::String(path))) => {
            crate::graph::affected::load_graph(Path::new(path))
        }
        _ => Err(Error::invalid(format!(
            "Provide exactly one of {key_graph}, {key_path} or {key_ref}"
        ))),
    }
}

fn exports_from(request: &Value) -> Result<ExportIndex> {
    let graph = graph_from(request, "graph", "graph_ref", "graph_path")?;
    let repo = request["repo"]
        .as_str()
        .ok_or_else(|| Error::invalid("Expected repo"))?;
    let episode = request["episode"].as_str();
    let manifests = manifests_from(&request["manifests"])?;
    Ok(exports(&graph, repo, episode, &manifests))
}

/// `cross_repo.exports` → [`ExportIndex`]; `cross_repo.link` → [`LinkReport`].
///
/// `cross_repo.link` takes the importing graph (`graph` | `graph_ref` |
/// `graph_path`, `repo`, `episode?`, `manifests?`), the importing sources as
/// `sources: {relpath: text}` and/or `root` (a directory to read them from),
/// and the exporters either prebuilt as `indexes: [ExportIndex]` or as
/// `exporters: [{graph|graph_ref|graph_path, repo, episode?, manifests?}]`.
pub fn dispatch(request: &Value) -> Result<Value> {
    match request["operation"].as_str().unwrap_or("") {
        "cross_repo.exports" => Ok(serde_json::to_value(exports_from(request)?)?),
        "cross_repo.link" => {
            let graph = graph_from(request, "graph", "graph_ref", "graph_path")?;
            let repo = request["repo"]
                .as_str()
                .ok_or_else(|| Error::invalid("Expected repo"))?;
            let episode = request["episode"].as_str();
            let manifests = manifests_from(&request["manifests"])?;
            let mut indexes: Vec<ExportIndex> = match request.get("indexes") {
                None | Some(Value::Null) => vec![],
                Some(list) => serde_json::from_value(list.clone())?,
            };
            if let Some(exporters) = request["exporters"].as_array() {
                for exporter in exporters {
                    let mut exporter = exporter.clone();
                    if exporter.get("edges_key").is_none()
                        && let Some(key) = request.get("edges_key")
                    {
                        exporter["edges_key"] = key.clone();
                    }
                    indexes.push(exports_from(&exporter)?);
                }
            }
            let inline: BTreeMap<String, String> = match request.get("sources") {
                None | Some(Value::Null) => BTreeMap::new(),
                Some(map) => serde_json::from_value(map.clone())?,
            };
            let root = request["root"].as_str().map(|r| Path::new(r).to_path_buf());
            let loader = |relpath: &str| -> Option<String> {
                if let Some(text) = inline.get(relpath) {
                    return Some(text.clone());
                }
                root.as_deref()
                    .and_then(|root| sources_below(root)(relpath))
            };
            let report = link(&graph, repo, episode, &manifests, &indexes, &loader);
            Ok(serde_json::to_value(report)?)
        }
        _ => Err(Error::UnsupportedOperation(
            "Unsupported cross_repo operation".into(),
        )),
    }
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sources_join_relpaths_below_the_root() {
        let root = Path::new("repo");
        for inside in ["a.py", "src/a.py", "./src/a.py", "a/./b.py"] {
            assert_eq!(
                join_below(root, inside),
                Some(root.join(inside)),
                "{inside}"
            );
        }
    }

    /// On Windows a relpath that could leave the root is not joined: a drive,
    /// UNC, rooted or absolute path, or `..`. On Linux and macOS `join_below`
    /// is `root.join`, and `sources_below` refuses `..` segments as before.
    #[cfg(windows)]
    #[test]
    fn windows_prefixed_relpaths_stay_unjoined() {
        let root = Path::new(r"C:\repo");
        for outside in [
            "C:/x.py",
            "C:x.py",
            "//host/share/x.py",
            r"\\host\share\x.py",
            r"\x.py",
            "/abs/a.py",
            "../a.py",
            r"src\..\..\a.py",
            "..",
        ] {
            assert_eq!(join_below(root, outside), None, "{outside}");
        }
        assert_eq!(
            join_below(root, r"src\a.py"),
            Some(Path::new(r"C:\repo\src\a.py").to_path_buf())
        );
    }

    fn node(id: &str, label: &str, relpath: &str, extra: Value) -> Value {
        let mut node = json!({"id": id, "label": label, "source_file": relpath, "relpath": relpath, "stable_id": format!("sid:{id}")});
        if let (Value::Object(map), Value::Object(more)) = (&mut node, extra) {
            map.extend(more);
        }
        node
    }
    fn external(id: &str, label: &str) -> Value {
        json!({"id": id, "label": label, "file_type": "concept", "type": "external", "external": true, "source_file": ""})
    }
    fn edge(source: &str, target: &str, relation: &str) -> Value {
        json!({"source": source, "target": target, "relation": relation, "confidence": "EXTRACTED"})
    }
    fn graph(nodes: Vec<Value>, edges: Vec<Value>) -> Graph {
        Graph::from_node_link(
            &json!({"directed": true, "multigraph": false, "graph": {}, "nodes": nodes, "links": edges}),
            "links",
        )
        .unwrap()
    }

    // ── Python ──
    fn alib(version: &str) -> (Graph, Vec<PackageManifest>) {
        let g = graph(
            vec![
                node("alib_init", "__init__.py", "alib/__init__.py", json!({})),
                node("alib_core", "core.py", "alib/core.py", json!({})),
                node(
                    "alib_core_process",
                    "process()",
                    "alib/core.py",
                    json!({"_callable": true}),
                ),
                node(
                    "alib_core_normalise",
                    "normalise()",
                    "alib/core.py",
                    json!({"_callable": true}),
                ),
                node("alib_store", "store.py", "alib/store.py", json!({})),
                node(
                    "alib_store_store",
                    "Store",
                    "alib/store.py",
                    json!({"_callable": true, "_callable_class": true}),
                ),
                node(
                    "alib_store_store_save",
                    ".save()",
                    "alib/store.py",
                    json!({"_callable": true}),
                ),
                node(
                    "alib_sub_init",
                    "__init__.py",
                    "alib/sub/__init__.py",
                    json!({}),
                ),
                node("alib_sub_deep", "deep.py", "alib/sub/deep.py", json!({})),
                node(
                    "tests_test_core",
                    "test_core.py",
                    "tests/test_core.py",
                    json!({}),
                ),
            ],
            vec![edge("alib_core", "alib_core_process", "contains")],
        );
        let manifest = PackageManifest::new(
            "pyproject.toml",
            format!("[project]\nname = \"alib\"\nversion = \"{version}\"\n"),
        );
        (g, vec![manifest])
    }
    fn bapp(constraint: &str) -> (Graph, Vec<PackageManifest>, BTreeMap<String, String>) {
        let g = graph(
            vec![
                node("bapp_ingest", "ingest.py", "bapp/ingest.py", json!({})),
                node("bapp_cli", "cli.py", "bapp/cli.py", json!({})),
                external("alib_core", "alib_core"),
                external("alib_store", "alib_store"),
                external("alib_sub", "alib_sub"),
                external("sys", "sys"),
                external("clib_x", "clib_x"),
            ],
            vec![
                edge("bapp_ingest", "alib_core", "imports_from"),
                edge("bapp_cli", "alib_store", "imports_from"),
                edge("bapp_cli", "alib_sub", "imports_from"),
                edge("bapp_cli", "sys", "imports"),
                edge("bapp_cli", "clib_x", "imports_from"),
            ],
        );
        let manifest = PackageManifest::new(
            "pyproject.toml",
            format!(
                "[project]\nname = \"bapp\"\nversion = \"0.1\"\ndependencies = [\"alib{constraint}\", \"clib>=1\"]\n"
            ),
        );
        let mut sources = BTreeMap::new();
        sources.insert(
            "bapp/ingest.py".to_string(),
            "from alib.core import process, missing_name\n\ndef run():\n    return process([])\n"
                .to_string(),
        );
        sources.insert("bapp/cli.py".to_string(), "import sys\nfrom alib.store import Store\nfrom alib.sub import deep\nfrom clib.x import y\n".to_string());
        (g, vec![manifest], sources)
    }
    fn loader(sources: &BTreeMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        move |relpath: &str| sources.get(relpath).cloned()
    }

    #[test]
    fn python_exports_map_module_paths_and_symbols() {
        let (g, manifests) = alib("1.2.0");
        let index = exports(&g, "repo-a", Some("ep-a1"), &manifests);
        assert_eq!(index.packages[0].name, "alib");
        assert_eq!(index.packages[0].import_roots, vec!["alib".to_string()]);
        assert_eq!(index.packages[0].version.as_deref(), Some("1.2.0"));
        let core = &index.modules["alib.core"];
        assert_eq!(core.node_id, "alib_core");
        assert_eq!(core.derivation, "path");
        assert!(core.symbols.contains_key("process"));
        assert!(core.symbols.contains_key("normalise"));
        assert_eq!(index.modules["alib"].node_id, "alib_init");
        assert_eq!(index.modules["alib.sub"].node_id, "alib_sub_init");
        assert!(index.modules.contains_key("alib.sub.deep"));
        // Methods (`.save()`) are not module-level exports; tests are excluded.
        assert!(!index.modules["alib.store"].symbols.contains_key("save"));
        assert!(!index.modules.keys().any(|m| m.starts_with("tests")));
    }

    #[test]
    fn python_exports_infer_roots_without_manifest() {
        let (g, _) = alib("1.0");
        let index = exports(&g, "repo-a", None, &[]);
        assert_eq!(index.packages.len(), 1);
        assert_eq!(index.packages[0].import_roots, vec!["alib".to_string()]);
        assert!(index.modules.contains_key("alib.core"));
    }

    #[test]
    fn python_links_symbols_modules_and_reports_misses() {
        let (a, a_manifests) = alib("1.2.0");
        let index = exports(&a, "repo-a", Some("ep-a1"), &a_manifests);
        let (b, b_manifests, sources) = bapp(">=1.0,<2");
        let report = link(
            &b,
            "repo-b",
            Some("ep-b1"),
            &b_manifests,
            &[index],
            &loader(&sources),
        );
        let find = |src: &str, tgt: &str| {
            report
                .edges
                .iter()
                .find(|e| e.source.node_id == src && e.target.node_id == tgt)
        };
        let process = find("bapp_ingest", "alib_core_process").expect("symbol edge");
        assert_eq!(process.relation, RELATION_SYMBOL);
        assert_eq!(process.confidence, EXTRACTED);
        assert_eq!(process.symbol.as_deref(), Some("process"));
        assert_eq!(process.source.episode.as_deref(), Some("ep-b1"));
        assert_eq!(process.target.episode.as_deref(), Some("ep-a1"));
        assert_eq!(
            process.target.stable_id.as_deref(),
            Some("sid:alib_core_process")
        );
        assert_eq!(process.version_constraint.as_deref(), Some(">=1.0,<2"));
        assert_eq!(process.exporter_version.as_deref(), Some("1.2.0"));
        assert_eq!(process.version_status, "satisfied");
        assert_eq!(process.line, Some(1));
        // The missing symbol leaves a module-level INFERRED edge plus an unresolved record.
        let module = find("bapp_ingest", "alib_core").expect("module edge");
        assert_eq!(module.relation, RELATION_MODULE);
        assert_eq!(module.confidence, INFERRED);
        assert!(
            report
                .unresolved
                .iter()
                .any(|u| u.reason == REASON_SYMBOL && u.symbol.as_deref() == Some("missing_name"))
        );
        let store = find("bapp_cli", "alib_store_store").expect("class edge");
        assert_eq!(store.confidence, EXTRACTED);
        // `from alib.sub import deep` names a submodule.
        let deep = find("bapp_cli", "alib_sub_deep").expect("submodule edge");
        assert_eq!(deep.relation, RELATION_MODULE);
        assert_eq!(deep.confidence, EXTRACTED);
        // Declared but unindexed dependency vs plain stdlib.
        assert!(
            report
                .unresolved
                .iter()
                .any(|u| u.reason == REASON_DECLARED_NOT_INDEXED && u.package == "clib")
        );
        assert!(!report.unresolved.iter().any(|u| u.package == "sys"));
        assert_eq!(report.skipped_stdlib, 1, "`import sys` is standard library");
        assert!(!report.edges.iter().any(|e| e.target.repo == "repo-b"));
        assert_eq!(report.files_with_source, 2);
    }

    #[test]
    fn python_without_sources_is_inferred_module_level() {
        let (a, a_manifests) = alib("1.2.0");
        let index = exports(&a, "repo-a", None, &a_manifests);
        let (b, b_manifests, _) = bapp("");
        let report = link(&b, "repo-b", None, &b_manifests, &[index], &no_sources);
        let core = report
            .edges
            .iter()
            .find(|e| e.target.node_id == "alib_core")
            .unwrap();
        assert_eq!(core.confidence, INFERRED);
        assert!(core.derivation.starts_with("id_match"));
        assert_eq!(core.version_status, "unconstrained");
        assert!(!report.edges.iter().any(|e| e.relation == RELATION_SYMBOL));
    }

    #[test]
    fn python_version_mismatch_is_unresolved() {
        let (a, a_manifests) = alib("2.0.0");
        let index = exports(&a, "repo-a", None, &a_manifests);
        let (b, b_manifests, sources) = bapp(">=1.0,<2");
        let report = link(
            &b,
            "repo-b",
            None,
            &b_manifests,
            &[index],
            &loader(&sources),
        );
        assert!(report.edges.iter().all(|e| e.target.repo != "repo-a"));
        let miss = report
            .unresolved
            .iter()
            .find(|u| u.reason == REASON_VERSION)
            .unwrap();
        assert_eq!(miss.candidates, vec!["repo-a".to_string()]);
        assert_eq!(miss.exporter_version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn python_ambiguous_exporters_are_unresolved() {
        let (a, a_manifests) = alib("1.2.0");
        let first = exports(&a, "repo-a", None, &a_manifests);
        let second = exports(&a, "repo-a-fork", None, &a_manifests);
        let (b, b_manifests, sources) = bapp("");
        let report = link(
            &b,
            "repo-b",
            None,
            &b_manifests,
            &[first, second],
            &loader(&sources),
        );
        assert!(report.edges.is_empty());
        let ambiguous: Vec<_> = report
            .unresolved
            .iter()
            .filter(|u| u.reason == REASON_AMBIGUOUS)
            .collect();
        assert!(!ambiguous.is_empty());
        assert_eq!(
            ambiguous[0].candidates,
            vec!["repo-a".to_string(), "repo-a-fork".to_string()]
        );
    }

    #[test]
    fn python_version_mismatch_disambiguates_when_one_exporter_fits() {
        let (a1, m1) = alib("1.5.0");
        let (a2, m2) = alib("2.0.0");
        let one = exports(&a1, "repo-a-v1", None, &m1);
        let two = exports(&a2, "repo-a-v2", None, &m2);
        let (b, b_manifests, sources) = bapp("~=1.4");
        let report = link(
            &b,
            "repo-b",
            None,
            &b_manifests,
            &[one, two],
            &loader(&sources),
        );
        assert!(report.edges.iter().all(|e| e.target.repo == "repo-a-v1"));
        assert!(
            report
                .edges
                .iter()
                .any(|e| e.target.node_id == "alib_core_process")
        );
    }

    #[test]
    fn python_unknown_module_in_known_package() {
        let (a, a_manifests) = alib("1.0");
        let index = exports(&a, "repo-a", None, &a_manifests);
        let b = graph(
            vec![
                node("bapp_x", "x.py", "bapp/x.py", json!({})),
                external("alib_missing", "alib_missing"),
            ],
            vec![edge("bapp_x", "alib_missing", "imports_from")],
        );
        let mut sources = BTreeMap::new();
        sources.insert(
            "bapp/x.py".to_string(),
            "from alib.missing import thing\n".to_string(),
        );
        let report = link(&b, "repo-b", None, &[], &[index], &loader(&sources));
        assert!(report.edges.is_empty());
        assert_eq!(report.unresolved[0].reason, REASON_MODULE);
    }

    // ── TypeScript ──
    fn ts_lib() -> (Graph, Vec<PackageManifest>) {
        let g = graph(
            vec![
                node("src_index", "index.ts", "src/index.ts", json!({})),
                node("src_core", "core.ts", "src/core.ts", json!({})),
                node(
                    "src_core_process",
                    "process()",
                    "src/core.ts",
                    json!({"_callable": true}),
                ),
                node(
                    "src_core_normalise",
                    "normalise()",
                    "src/core.ts",
                    json!({"_callable": true}),
                ),
                node("src_parse", "parse.ts", "src/parse.ts", json!({})),
                node(
                    "src_parse_parselines",
                    "parseLines()",
                    "src/parse.ts",
                    json!({"_callable": true}),
                ),
            ],
            vec![],
        );
        let manifest = PackageManifest::new(
            "package.json",
            r#"{"name":"@acme/alib","version":"1.4.2","main":"dist/index.js","exports":{".":{"import":"./dist/index.js"},"./core":"./dist/core.js","./parse":"./src/parse.ts"}}"#,
        );
        (g, vec![manifest])
    }
    fn ts_app(constraint: &str) -> (Graph, Vec<PackageManifest>, BTreeMap<String, String>) {
        let g = graph(
            vec![
                node("src_ingest", "ingest.ts", "src/ingest.ts", json!({})),
                json!({"id": "ref_acme_alib", "label": "@acme/alib", "file_type": "concept", "source_file": "package.json"}),
                external("ref_lodash", "lodash"),
            ],
            vec![
                edge("src_ingest", "ref_acme_alib", "imports_from"),
                edge("src_ingest", "ref_lodash", "imports_from"),
            ],
        );
        let manifest = PackageManifest::new(
            "package.json",
            format!(
                r#"{{"name":"@acme/bapp","version":"0.0.1","dependencies":{{"@acme/alib":"{constraint}"}}}}"#
            ),
        );
        let mut sources = BTreeMap::new();
        sources.insert(
            "src/ingest.ts".to_string(),
            "import { process, normalise as norm, type Thing } from \"@acme/alib/core\";\nimport * as parse from '@acme/alib/parse';\nimport alib from \"@acme/alib\";\nconst store = require(\"@acme/alib/store\");\nimport { local } from \"./local\";\nimport _ from 'lodash';\n".to_string(),
        );
        (g, vec![manifest], sources)
    }

    #[test]
    fn node_exports_cover_exports_map_main_and_src_convention() {
        let (g, manifests) = ts_lib();
        let index = exports(&g, "ts-a", Some("ep-ts-a"), &manifests);
        let root = &index.modules["@acme/alib"];
        assert_eq!(root.node_id, "src_index");
        // `dist/index.js` ↔ `src/index.ts` is a build-directory convention.
        assert!(
            root.derivation.ends_with(":build_dir"),
            "{}",
            root.derivation
        );
        let core = &index.modules["@acme/alib/core"];
        assert_eq!(core.node_id, "src_core");
        assert_eq!(core.derivation, "exports_map:build_dir");
        assert!(core.symbols.contains_key("process"));
        // An exports entry that names the source file is exact and beats src_strip.
        assert_eq!(index.modules["@acme/alib/parse"].derivation, "exports_map");
        assert_eq!(index.modules["@acme/alib/src/parse"].derivation, "path");
        assert_eq!(index.modules["@acme/alib/src/core"].derivation, "path");
    }

    #[test]
    fn node_links_named_namespace_default_and_require_imports() {
        let (a, a_manifests) = ts_lib();
        let index = exports(&a, "ts-a", Some("ep-ts-a"), &a_manifests);
        let (b, b_manifests, sources) = ts_app("^1.2.0");
        let report = link(
            &b,
            "ts-b",
            Some("ep-ts-b"),
            &b_manifests,
            &[index],
            &loader(&sources),
        );
        let find = |tgt: &str| report.edges.iter().find(|e| e.target.node_id == tgt);
        let process = find("src_core_process").expect("named symbol");
        // Symbol matched, but `./core → ./dist/core.js` reaches src/core.ts by convention.
        assert_eq!(process.confidence, INFERRED);
        assert!(
            process.derivation.contains("build_dir"),
            "{}",
            process.derivation
        );
        assert_eq!(process.version_status, "satisfied");
        let normalise = find("src_core_normalise").expect("aliased symbol");
        assert_eq!(normalise.symbol.as_deref(), Some("normalise"));
        // `type Thing` is not in the graph → unresolved symbol, module edge kept.
        assert!(
            report
                .unresolved
                .iter()
                .any(|u| u.reason == REASON_SYMBOL && u.symbol.as_deref() == Some("Thing"))
        );
        let parse = find("src_parse").expect("namespace import");
        assert_eq!(
            parse.confidence, EXTRACTED,
            "exports entry names the source file"
        );
        assert_eq!(parse.relation, RELATION_MODULE);
        let root = find("src_index").expect("default import → package root");
        assert_eq!(root.confidence, INFERRED);
        assert!(
            report
                .unresolved
                .iter()
                .any(|u| u.reason == REASON_MODULE && u.module_path == "@acme/alib/store")
        );
        // lodash is neither declared nor exported by an index: skipped, not guessed.
        assert!(!report.unresolved.iter().any(|u| u.package == "lodash"));
        assert_eq!(report.skipped_undeclared, 1);
        assert!(!report.edges.iter().any(|e| e.module_path.starts_with("./")));
    }

    #[test]
    fn node_semver_mismatch_is_unresolved() {
        let (a, a_manifests) = ts_lib();
        let index = exports(&a, "ts-a", None, &a_manifests);
        let (b, b_manifests, sources) = ts_app("^2.0.0");
        let report = link(&b, "ts-b", None, &b_manifests, &[index], &loader(&sources));
        assert!(report.edges.is_empty());
        assert!(
            report
                .unresolved
                .iter()
                .all(|u| u.reason == REASON_VERSION || u.reason == REASON_NO_EXPORTER)
        );
        let (b, b_manifests, sources) = ts_app("workspace:*");
        let (a, a_manifests) = ts_lib();
        let index = exports(&a, "ts-a", None, &a_manifests);
        let report = link(&b, "ts-b", None, &b_manifests, &[index], &loader(&sources));
        assert!(report.edges.iter().all(|e| e.version_status == "unchecked"));
        assert!(!report.edges.is_empty());
    }

    // ── Go ──
    fn go_lib() -> (Graph, Vec<PackageManifest>) {
        let g = graph(
            vec![
                node("core_core", "core.go", "core/core.go", json!({})),
                node("core_core_process", "Process()", "core/core.go", json!({})),
                node("core_core_helper", "helper()", "core/core.go", json!({})),
                node("core_store", "Store", "core/core.go", json!({})),
                node("core_extra", "extra.go", "core/extra.go", json!({})),
                node("core_extra_more", "More()", "core/extra.go", json!({})),
                node("parse_parse", "parse.go", "parse/parse.go", json!({})),
                node(
                    "parse_parse_parselines",
                    "ParseLines()",
                    "parse/parse.go",
                    json!({}),
                ),
                node(
                    "parse_parse_test",
                    "parse_test.go",
                    "parse/parse_test.go",
                    json!({}),
                ),
                json!({"id": "pkg_example_com_alib", "label": "example.com/alib", "type": "package", "ecosystem": "go", "source_file": "go.mod"}),
            ],
            vec![],
        );
        (
            g,
            vec![PackageManifest::new(
                "go.mod",
                "module example.com/alib\n\ngo 1.22\n",
            )],
        )
    }
    fn go_app() -> (Graph, Vec<PackageManifest>, BTreeMap<String, String>) {
        let g = graph(
            vec![
                node("cmd_main", "main.go", "cmd/main.go", json!({})),
                external(
                    "go_pkg_example_com_alib_core",
                    "go_pkg_example_com_alib_core",
                ),
                external(
                    "go_pkg_example_com_alib_parse",
                    "go_pkg_example_com_alib_parse",
                ),
                external("go_pkg_fmt", "go_pkg_fmt"),
                external("go_pkg_example_com_other_x", "go_pkg_example_com_other_x"),
            ],
            vec![
                edge("cmd_main", "go_pkg_example_com_alib_core", "imports_from"),
                edge("cmd_main", "go_pkg_example_com_alib_parse", "imports_from"),
                edge("cmd_main", "go_pkg_fmt", "imports_from"),
                edge("cmd_main", "go_pkg_example_com_other_x", "imports_from"),
            ],
        );
        let manifest = PackageManifest::new(
            "go.mod",
            "module example.com/bapp\n\ngo 1.22\n\nrequire (\n\texample.com/alib v1.2.0\n\texample.com/other v0.1.0 // indirect\n)\n",
        );
        let mut sources = BTreeMap::new();
        sources.insert(
            "cmd/main.go".to_string(),
            "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/alib/core\"\n\tp \"example.com/alib/parse\"\n\t\"example.com/other/x\"\n)\n\nfunc main() {\n\tvar s core.Store\n\tfmt.Println(core.Process(p.ParseLines(nil)), core.More(), core.Missing, s, x.Y)\n}\n".to_string(),
        );
        (g, vec![manifest], sources)
    }

    #[test]
    fn go_exports_merge_package_directories() {
        let (g, manifests) = go_lib();
        let index = exports(&g, "go-a", None, &manifests);
        assert_eq!(index.packages[0].name, "example.com/alib");
        let core = &index.modules["example.com/alib/core"];
        assert!(core.symbols.contains_key("Process"));
        assert!(
            core.symbols.contains_key("More"),
            "second file of the package merged"
        );
        assert!(core.symbols.contains_key("Store"));
        assert!(
            !core.symbols.contains_key("helper"),
            "unexported identifiers are not exports"
        );
        assert!(index.modules.contains_key("example.com/alib/parse"));
        assert!(
            !index.modules["example.com/alib/parse"]
                .relpath
                .ends_with("_test.go")
        );
    }

    #[test]
    fn symbol_node_standing_in_for_its_file_is_still_exported() {
        // Go builds give `store/store.go` and `type Store` the same id, so the
        // only node of the file is the struct; it must be the module node and
        // an exported symbol, and intra-module imports are not "unresolved".
        let g = graph(
            vec![
                node("store_store", "Store", "store/store.go", json!({})),
                node("store_store_save", ".Save()", "store/store.go", json!({})),
                node("cmd_main", "main.go", "cmd/main.go", json!({})),
                external("go_pkg_example_com_alib_store", "example.com/alib/store"),
                external("go_pkg_example_com_alib_util", "example.com/alib/util"),
            ],
            vec![
                edge("cmd_main", "go_pkg_example_com_alib_store", "imports_from"),
                edge("cmd_main", "go_pkg_example_com_alib_util", "imports_from"),
            ],
        );
        let manifests = vec![PackageManifest::new("go.mod", "module example.com/alib\n")];
        let index = exports(&g, "go-a", None, &manifests);
        let store = &index.modules["example.com/alib/store"];
        assert_eq!(store.node_id, "store_store");
        assert!(store.symbols.contains_key("Store"));
        assert!(
            !store.symbols.contains_key("Save"),
            "methods are not module exports"
        );
        let source = |relpath: &str| -> Option<String> {
            (relpath == "cmd/main.go").then(|| {
                "package main\n\nimport (\n\t\"os\"\n\n\t\"example.com/alib/store\"\n\t\"example.com/alib/util\"\n)\n"
                    .to_string()
            })
        };
        let report = link(&g, "go-a", None, &manifests, &[], &source);
        assert!(report.edges.is_empty());
        assert!(report.unresolved.is_empty(), "{:?}", report.unresolved);
        assert_eq!(report.skipped_internal, 2);
    }

    #[test]
    fn go_links_qualified_identifiers_and_skips_stdlib() {
        let (a, a_manifests) = go_lib();
        let index = exports(&a, "go-a", Some("ep-go-a"), &a_manifests);
        let (b, b_manifests, sources) = go_app();
        let report = link(
            &b,
            "go-b",
            Some("ep-go-b"),
            &b_manifests,
            &[index],
            &loader(&sources),
        );
        let find = |tgt: &str| report.edges.iter().find(|e| e.target.node_id == tgt);
        let process = find("core_core_process").expect("core.Process");
        assert_eq!(process.confidence, EXTRACTED);
        assert_eq!(process.version_constraint.as_deref(), Some("v1.2.0"));
        assert_eq!(
            process.version_status, "unchecked",
            "go.mod declares no exporter version"
        );
        assert!(find("core_store").is_some());
        assert!(find("core_extra_more").is_some());
        let parse = find("parse_parse_parselines").expect("aliased package p.ParseLines");
        assert_eq!(parse.symbol.as_deref(), Some("ParseLines"));
        assert!(
            report
                .unresolved
                .iter()
                .any(|u| u.reason == REASON_SYMBOL && u.symbol.as_deref() == Some("Missing"))
        );
        assert!(
            report
                .unresolved
                .iter()
                .any(|u| u.reason == REASON_DECLARED_NOT_INDEXED
                    && u.module_path == "example.com/other/x")
        );
        assert!(
            !report.unresolved.iter().any(|u| u.module_path == "fmt"),
            "stdlib is not an exporter candidate"
        );
        assert!(
            report
                .edges
                .iter()
                .all(|e| e.target.episode.as_deref() == Some("ep-go-a"))
        );
    }

    // ── versions ──
    #[test]
    fn version_constraints() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert_eq!(
            python_satisfies(">=1.0,<2", &v("1.9.3")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            python_satisfies(">=1.0,<2", &v("2.0")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(
            python_satisfies("~=1.4", &v("1.9")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            python_satisfies("~=1.4.2", &v("1.5.0")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(
            python_satisfies("==1.4.*", &v("1.4.9")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            python_satisfies("==1.4.*", &v("1.5.0")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(
            python_satisfies("!=1.4.2", &v("1.4.2")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(python_satisfies("", &v("1.0")), VersionCheck::Satisfied);
        assert_eq!(
            python_satisfies(">=1.0; python_version>'3'", &v("1.0")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            node_satisfies("^1.2.0", &v("1.4.2")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            node_satisfies("^1.2.0", &v("2.0.0")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(
            node_satisfies("^0.2.3", &v("0.2.9")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            node_satisfies("^0.2.3", &v("0.3.0")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(
            node_satisfies("~1.2.3", &v("1.2.9")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            node_satisfies("~1.2.3", &v("1.3.0")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(
            node_satisfies("1.2.x", &v("1.2.7")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            node_satisfies(">=1.0.0 <2.0.0 || ^3.0.0", &v("3.1.0")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            node_satisfies("1.0.0 - 1.5.0", &v("1.5.0")),
            VersionCheck::Satisfied
        );
        assert_eq!(node_satisfies("*", &v("9.9.9")), VersionCheck::Satisfied);
        assert_eq!(
            node_satisfies("workspace:*", &v("1.0.0")),
            VersionCheck::Unchecked
        );
        assert_eq!(
            node_satisfies("^1.0.0", &v("1.0.0-beta.1")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(
            go_satisfies("v1.2.0", &v("v1.3.0")),
            VersionCheck::Satisfied
        );
        assert_eq!(
            go_satisfies("v1.2.0", &v("v1.1.0")),
            VersionCheck::Unsatisfied
        );
        assert_eq!(
            go_satisfies("v1.2.0", &v("v2.0.0")),
            VersionCheck::Unsatisfied
        );
    }

    #[test]
    fn manifests_parse_setup_cfg_and_poetry() {
        let cfg = PackageManifest::new(
            "setup.cfg",
            "[metadata]\nname = My-Lib\nversion = 0.3\n\n[options]\npackage_dir =\n    =src\npackages = find:\ninstall_requires =\n    requests>=2\n    alib (>=1.0)\n\n[options.packages.find]\nwhere = src\n",
        );
        let ids = package_identities(&[cfg]);
        assert_eq!(ids[0].name, "my_lib");
        assert_eq!(ids[0].version.as_deref(), Some("0.3"));
        assert_eq!(ids[0].source_prefix.as_deref(), Some("src"));
        assert_eq!(ids[0].dependencies["alib"], ">=1.0");
        let poetry = PackageManifest::new(
            "pyproject.toml",
            "[tool.poetry]\nname = \"plib\"\nversion = \"2.1.0\"\npackages = [{ include = \"plib\", from = \"src\" }]\n\n[tool.poetry.dependencies]\npython = \"^3.10\"\nalib = \"^1.2\"\nclib = { version = \">=3\", optional = true }\n",
        );
        let ids = package_identities(&[poetry]);
        assert_eq!(ids[0].name, "plib");
        assert_eq!(ids[0].import_roots, vec!["plib".to_string()]);
        assert_eq!(ids[0].source_prefix.as_deref(), Some("src"));
        assert_eq!(ids[0].dependencies["alib"], "^1.2");
        assert_eq!(ids[0].dependencies["clib"], ">=3");
        assert!(!ids[0].dependencies.contains_key("python"));
    }

    #[test]
    fn import_statement_parsers() {
        let py = python_imports(
            "import os, sys\nfrom a.b import (\n    x,\n    y as z,\n)\nfrom . import rel\nfrom c import *\n",
        );
        assert_eq!(py[0].module, "os");
        assert_eq!(py[2].module, "a.b");
        assert_eq!(
            py[2].shape,
            ImportShape::Named(vec!["x".into(), "y".into()])
        );
        assert_eq!(py[2].line, 2);
        assert_eq!(py[3].module, "c");
        assert_eq!(py[3].shape, ImportShape::Wildcard);
        assert_eq!(py.len(), 4);
        let js = js_imports(
            "import Foo, { a, b as c } from 'pkg/sub';\nexport { d } from \"@s/p\";\nimport 'side';\nconst q = require(\"pkg\");\nimport type { T } from './rel';\n",
        );
        assert_eq!(js[0].module, "pkg/sub");
        assert_eq!(
            js[0].shape,
            ImportShape::Named(vec!["a".into(), "b".into()])
        );
        assert_eq!(js[1].module, "@s/p");
        assert_eq!(js[1].shape, ImportShape::Named(vec!["d".into()]));
        assert_eq!(js[2].shape, ImportShape::Whole);
        assert_eq!(js[3].module, "pkg");
        assert_eq!(js.len(), 4);
        let go = go_imports(
            "package x\nimport (\n\t\"fmt\"\n\tyaml \"gopkg.in/yaml.v3\"\n\t\"github.com/o/r/v2\"\n)\nfunc f() { yaml.Marshal(nil); r.New() }\n",
        );
        assert_eq!(go[0].module, "gopkg.in/yaml.v3");
        assert_eq!(go[0].shape, ImportShape::Named(vec!["Marshal".into()]));
        assert_eq!(go[1].module, "github.com/o/r/v2");
        assert_eq!(go[1].alias.as_deref(), Some("r"));
        assert_eq!(go[1].shape, ImportShape::Named(vec!["New".into()]));
    }

    #[test]
    fn dispatch_round_trip() {
        let (a, a_manifests) = alib("1.2.0");
        let (b, b_manifests, sources) = bapp(">=1");
        let request = json!({
            "operation": "cross_repo.link",
            "graph": b.to_node_link("links"),
            "repo": "repo-b",
            "episode": "ep-b",
            "manifests": b_manifests,
            "sources": sources,
            "exporters": [{"graph": a.to_node_link("links"), "repo": "repo-a", "episode": "ep-a", "manifests": a_manifests}],
        });
        let out = dispatch(&request).unwrap();
        let report: LinkReport = serde_json::from_value(out).unwrap();
        assert!(
            report
                .edges
                .iter()
                .any(|e| e.target.node_id == "alib_core_process"
                    && e.target.episode.as_deref() == Some("ep-a"))
        );
        let index = dispatch(&json!({"operation": "cross_repo.exports", "graph": a.to_node_link("links"), "repo": "repo-a", "manifests": a_manifests})).unwrap();
        let index: ExportIndex = serde_json::from_value(index).unwrap();
        assert!(index.modules.contains_key("alib.core"));
    }
}
