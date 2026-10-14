//! HTTP routes and the calls that reach them (opt-in: `--routes` /
//! `GRAPHICLE_ROUTES=1`).
//!
//! Graphify links code by calls and imports; a request from a client to a
//! route crosses a boundary no call edge shows. This pass adds:
//!
//! - a `route` node per declared route (`GET /items/{id}`), with
//!   `route_method` and `route_path`;
//! - `handles` edges from the handler function to its route;
//! - `http_calls` edges from a function that sends a request with a literal
//!   path or URL to the one route it matches.
//!
//! Routes come from FastAPI and Flask decorators (`@app.get("/p")`,
//! `@router.post`, `@bp.route("/p", methods=[...])`, `@app.websocket`),
//! `add_api_route`/`add_url_rule`, Django `path()`/`re_path()` in files that
//! define `urlpatterns`, and Express-style calls (`app.get("/p", handler)`).
//!
//! A route's path carries the prefixes of the routers it is mounted through,
//! in any file of the corpus: the router's own (`APIRouter(prefix=...)`,
//! `Blueprint(..., url_prefix=...)`, koa `new Router({prefix})`), FastAPI
//! `include_router(r, prefix=...)` and `mount("/p", app)`, Flask
//! `register_blueprint(bp, url_prefix=...)` (which replaces the blueprint's
//! own prefix, as Flask does, and joins parts with one `/`), Express
//! `app.use("/p", router)` and Django `path("p/", include("app.urls"))`.
//! Routers and modules are followed through imports (`from .routers import
//! items` ... `items.router`, `from . import bp`, `require("./users")`,
//! `module.exports`, `export default`) and re-exports; prefixes may be
//! module-level string constants. A router mounted several times has a route
//! per mount; one never mounted keeps its own path. A prefix that is neither
//! a literal nor such a constant shows as `{}`, an unknown base that, like a
//! request's `f"{base}/p"`, matches any leading segments.
//!
//! Incremental rebuilds (`watch`, hooks, `extract` with a stored graph) stay
//! exact: route nodes record the other files that shaped their path
//! (`_route_mounts`, relative to the route's file), and a change set that
//! touches one of them, or a file that now mounts a router, re-extracts every
//! file (see [`StoredMounts`]).
//!
//! Requests are `<client>.get("/p")`-style calls on receivers named like a
//! client (`client`, `session`, `requests`, `httpx`, `api`, `http`,
//! `axios`) and `fetch("/p")`. Parameters (`{id}`, `<int:id>`, `:id`) and
//! f-string or template placeholders match one path segment. When several
//! routes match, the one whose file the caller's file imports wins; with no
//! single winner, no edge is added. A route made entirely of wildcard segments
//! cannot identify an application by path, so it gets no `http_calls` edges.
//! Its route node and handler remain available. Local router bindings are kept
//! separate across factory functions and JS blocks. A Python `TestClient(app)`
//! restricts candidates to the exact application, including separate apps in
//! the same file. Aliases, imported zero-argument factories with one direct
//! return, and `with TestClient(...) as c` are followed. Conditional returns,
//! reassigned application bindings and factories requiring arguments remain
//! unresolved, without falling back to an unrelated route with the same URL.
use crate::extractors::parse_cache;
use crate::extractors::resolution::source_key;
use crate::ids::make_id;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tree_sitter::Node;

/// `--routes` / `GRAPHICLE_ROUTES=1`.
pub static ENABLED: AtomicBool = AtomicBool::new(false);
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed) || std::env::var("GRAPHICLE_ROUTES").is_ok_and(|v| v == "1")
}

const VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn text<'s>(n: Node, src: &'s [u8]) -> &'s str {
    n.utf8_text(src).unwrap_or("")
}

/// The key Django routes and `include()` mounts use for a file's
/// `urlpatterns`.
const DJANGO: &str = "urlpatterns";
/// How many imports or re-exports a reference is followed through.
const MAX_DEPTH: usize = 8;
/// Paths kept per route when its routers are mounted many ways.
const MAX_PATHS: usize = 16;
/// A prefix this pass cannot read (see the module docs).
const UNKNOWN: &str = "{}";

/// How a framework joins the prefixes of a route's routers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// FastAPI and Starlette: prefixes are concatenated.
    FastApi,
    /// Flask: a registration's `url_prefix` replaces the blueprint's own,
    /// and parts are joined with one `/`.
    Flask,
    /// Express and koa: a mount path is joined to the route path.
    Express,
    /// Django: `include()` patterns are concatenated below a leading `/`.
    Django,
}

/// A router or app created in a file (`router = APIRouter(prefix="/p")`).
#[derive(Debug, Clone)]
struct Router {
    kind: Kind,
    /// Its own prefix, if it was given one.
    prefix: Option<PrefixArg>,
}

/// A module a file refers to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ModRef {
    /// A Python absolute import (`app.routers.items`).
    Dotted(String),
    /// A relative import or `./` specifier: the path without extension.
    Path(PathBuf),
}

impl ModRef {
    /// The submodule `name` of this module or package.
    fn child(&self, name: &str) -> ModRef {
        match self {
            ModRef::Dotted(d) if d.is_empty() => ModRef::Dotted(name.into()),
            ModRef::Dotted(d) => ModRef::Dotted(format!("{d}.{name}")),
            ModRef::Path(p) => ModRef::Path(p.join(name)),
        }
    }
}

/// What a name bound by an import refers to.
#[derive(Debug, Clone)]
enum Binding {
    /// `import a.b as m`, `import * as m from "./m"`.
    Module(ModRef),
    /// Python `from m import n`; JS `import {n} from "./m"`, `const {n} =
    /// require("./m")`, and (as `default`) `import n from` or `require()`.
    Name(ModRef, String),
}

/// The router (or Django `urlpatterns` module) an include call mounts.
#[derive(Debug, Clone)]
enum Target {
    /// `router`
    Name(String),
    /// `items.router`
    Attr(String, String),
    /// `include("blog.urls")`, `require("./users")`
    Module(ModRef),
    /// A zero-argument Python factory call with one direct return.
    Factory(Box<Target>),
    /// An explicit application whose runtime value is not statically known.
    Unknown,
}

/// The prefix argument of an include call.
#[derive(Debug, Clone)]
enum PrefixArg {
    Literal(String),
    /// A name that should be a string constant; unknown otherwise.
    Name(String),
    /// Express `app.use(x, router)`: a mount path only if `x` is a string
    /// constant (otherwise `x` is middleware).
    Const(String),
    /// Not given (`None`, `null`).
    Absent,
    Unknown,
}

/// `parent.include_router(child, prefix=...)` and its kin.
#[derive(Debug, Clone)]
struct Include {
    /// The receiver (`app`); Django: [`DJANGO`].
    parent: String,
    child: Target,
    prefix: Option<PrefixArg>,
    kind: Kind,
}

/// What a file declares that can shape routes declared elsewhere.
#[derive(Debug, Clone, Default)]
struct Facts {
    python: bool,
    routers: FxHashMap<String, Router>,
    includes: Vec<Include>,
    bindings: FxHashMap<String, Binding>,
    /// Explicit in-process test clients, scoped like their router bindings.
    clients: FxHashMap<String, Target>,
    factories: FxHashMap<String, Target>,
    aliases: FxHashMap<String, Target>,
    assigned: FxHashSet<String>,
    uncertain: FxHashSet<String>,
    /// JS exports: exported name (`default` for the default export) to
    /// local name.
    exports: FxHashMap<String, String>,
    /// Module-level string constants.
    consts: FxHashMap<String, String>,
}

/// A declared route.
#[derive(Debug, Clone)]
struct Route {
    file: String,
    line: usize,
    method: String,
    /// The path as declared, before any router prefix.
    path: String,
    /// The receiver it is declared on (`router` in `@router.get`); Django:
    /// [`DJANGO`].
    router: String,
    /// How it was declared, for a receiver this pass cannot trace.
    kind: Kind,
    /// (name, definition line) of the handler, when known.
    handler: Option<(String, usize)>,
}

/// A request with a literal path.
#[derive(Debug, Clone)]
struct Request {
    file: String,
    line: usize,
    method: Option<String>,
    path: String,
    /// (name, definition line) of the enclosing function.
    caller: Option<(String, usize)>,
    /// A scoped client variable, or an inline `TestClient(app)` application.
    client: Option<String>,
    application: Option<Target>,
}

/// The static text of a string literal, with placeholders (`{x}` in
/// f-strings, `${x}` in templates) as `{}`; None for other expressions.
fn literal(n: Node, src: &[u8]) -> Option<String> {
    match n.kind() {
        "string" => {
            // Python: prefix, quotes and interpolations are children.
            let raw = text(n, src);
            let mut out = String::new();
            let mut cursor = n.walk();
            let mut saw_content = false;
            for c in n.children(&mut cursor) {
                match c.kind() {
                    "string_content" | "escape_sequence" => {
                        out.push_str(text(c, src));
                        saw_content = true;
                    }
                    "interpolation" => out.push_str("{}"),
                    "string_fragment" => {
                        // JS string
                        out.push_str(text(c, src));
                        saw_content = true;
                    }
                    _ => {}
                }
            }
            if !saw_content && !out.contains("{}") {
                // `""`, or a grammar without content children.
                let t = raw.trim_matches(|c| c == '"' || c == '\'');
                return Some(t.to_string());
            }
            Some(out)
        }
        "template_string" => {
            let mut out = String::new();
            let mut cursor = n.walk();
            for c in n.children(&mut cursor) {
                match c.kind() {
                    "string_fragment" => out.push_str(text(c, src)),
                    "template_substitution" => out.push_str("{}"),
                    _ => {}
                }
            }
            Some(out)
        }
        "concatenated_string" => {
            let mut out = String::new();
            let mut cursor = n.walk();
            for c in n.named_children(&mut cursor) {
                out.push_str(&literal(c, src)?);
            }
            Some(out)
        }
        _ => None,
    }
}

/// Positional arguments and keyword arguments of a call's argument list.
fn arguments<'t>(args: Node<'t>, src: &[u8]) -> (Vec<Node<'t>>, FxHashMap<String, Node<'t>>) {
    let mut pos = vec![];
    let mut kw = FxHashMap::default();
    let mut cursor = args.walk();
    for a in args.named_children(&mut cursor) {
        if a.kind() == "keyword_argument"
            && let (Some(k), Some(v)) = (
                a.child_by_field_name("name"),
                a.child_by_field_name("value"),
            )
        {
            kw.insert(text(k, src).to_string(), v);
        } else if a.kind() != "comment" {
            pos.push(a);
        }
    }
    (pos, kw)
}

/// Strings in a list/tuple/set literal (`methods=["GET", "POST"]`).
fn string_list(n: Node, src: &[u8]) -> Vec<String> {
    let mut out = vec![];
    let mut cursor = n.walk();
    for c in n.named_children(&mut cursor) {
        if let Some(v) = literal(c, src) {
            out.push(v.to_uppercase());
        }
    }
    out
}

/// The last name of a handler expression: `f`, `views.f`, `View.as_view()`.
fn handler_name(n: Node, src: &[u8]) -> Option<String> {
    match n.kind() {
        "identifier" => Some(text(n, src).to_string()),
        "attribute" | "member_expression" => n
            .child_by_field_name(if n.kind() == "attribute" {
                "attribute"
            } else {
                "property"
            })
            .map(|a| text(a, src).to_string()),
        "call" | "call_expression" => {
            // `View.as_view()` -> View
            let f = n.child_by_field_name("function")?;
            let obj = f.child_by_field_name("object")?;
            handler_name(obj, src)
        }
        _ => None,
    }
}

/// Name and line of the innermost function or class enclosing `n`.
fn enclosing(mut n: Node, src: &[u8]) -> Option<(String, usize)> {
    while let Some(p) = n.parent() {
        match p.kind() {
            "function_definition"
            | "function_declaration"
            | "method_definition"
            | "class_definition"
            | "class_declaration" => {
                let name = p.child_by_field_name("name")?;
                return Some((text(name, src).to_string(), name.start_position().row + 1));
            }
            "variable_declarator" | "public_field_definition" => {
                if let (Some(name), Some(value)) = (
                    p.child_by_field_name("name"),
                    p.child_by_field_name("value"),
                ) && matches!(
                    value.kind(),
                    "arrow_function" | "function_expression" | "function"
                ) {
                    return Some((text(name, src).to_string(), name.start_position().row + 1));
                }
            }
            _ => {}
        }
        n = p;
    }
    None
}

fn client_receiver(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let last = lower.rsplit('.').next().unwrap_or(&lower);
    last.contains("client")
        || last.contains("session")
        || ["requests", "httpx", "api", "http", "axios", "ac"]
            .iter()
            .any(|k| {
                last == *k || last.ends_with(&format!("_{k}")) || last.starts_with(&format!("{k}_"))
            })
}

fn looks_like_path(p: &str) -> bool {
    p.starts_with('/')
        || p.starts_with("http://")
        || p.starts_with("https://")
        || p.starts_with("{}/")
}

/// Whether an assignment or declarator is at module level.
fn top_level(n: Node, python: bool) -> bool {
    let Some(p) = n.parent() else {
        return false;
    };
    if python {
        return p.kind() == "expression_statement"
            && p.parent().is_some_and(|m| m.kind() == "module");
    }
    matches!(p.kind(), "lexical_declaration" | "variable_declaration")
        && p.parent().is_some_and(|m| {
            m.kind() == "program"
                || (m.kind() == "export_statement"
                    && m.parent().is_some_and(|x| x.kind() == "program"))
        })
}

/// The value of `key` in an object literal (`{ prefix: "/api" }`).
fn object_value<'t>(o: Node<'t>, key: &str, src: &[u8]) -> Option<Node<'t>> {
    let mut cursor = o.walk();
    for p in o.named_children(&mut cursor) {
        if p.kind() == "pair"
            && p.child_by_field_name("key")
                .is_some_and(|k| text(k, src).trim_matches(['"', '\'']) == key)
        {
            return p.child_by_field_name("value");
        }
        if p.kind() == "shorthand_property_identifier" && text(p, src) == key {
            return Some(p);
        }
    }
    None
}

/// The key of an object pair or pattern pair (`b` in `{ b: c }`).
fn pair_key(p: Node, src: &[u8]) -> Option<String> {
    let k = p.child_by_field_name("key")?;
    literal(k, src).or_else(|| Some(text(k, src).to_string()))
}

fn lexical(p: &Path) -> PathBuf {
    crate::detect_ignore::absolute_lexical(p)
}

/// `to` relative to the directory `from`, with `/` separators.
fn relative(from: &Path, to: &Path) -> String {
    let a = from.components().collect::<Vec<_>>();
    let b = to.components().collect::<Vec<_>>();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if common == 0 {
        return to.to_string_lossy().replace('\\', "/");
    }
    let mut parts = vec!["..".to_string(); a.len() - common];
    parts.extend(
        b[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

/// The grammar this pass reads `path` with, if any.
fn language(path: &Path) -> Option<(&'static str, tree_sitter::Language)> {
    Some(match crate::extract_languages::suffix(path).as_str() {
        ".py" => ("python", tree_sitter_python::LANGUAGE.into()),
        ".js" | ".jsx" | ".mjs" | ".cjs" => ("javascript", tree_sitter_javascript::LANGUAGE.into()),
        ".ts" | ".mts" | ".cts" => (
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        ".tsx" => ("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        _ => return None,
    })
}

#[derive(Default)]
struct LocalNames {
    bound: FxHashSet<String>,
    global: FxHashSet<String>,
    nonlocal: FxHashSet<String>,
}

/// Router names are lexical bindings, not file-wide identifiers. Build the
/// binding inventory before scanning uses so forward declarations, parameters
/// and unknown local values still shadow outer routers/imports/constants.
#[derive(Default)]
struct ScopeNames(FxHashMap<usize, LocalNames>);

fn function_scope(n: Node) -> bool {
    matches!(
        n.kind(),
        "function_definition"
            | "lambda"
            | "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
    )
}

fn binding_pattern(n: Node, src: &[u8], names: &mut FxHashSet<String>) {
    match n.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            names.insert(text(n, src).into());
        }
        "default_parameter" | "typed_default_parameter" => {
            if let Some(name) = n.child_by_field_name("name") {
                binding_pattern(name, src, names);
            }
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(name) = n
                .child_by_field_name("pattern")
                .or_else(|| n.child_by_field_name("name"))
            {
                binding_pattern(name, src, names);
            }
        }
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(left) = n.child_by_field_name("left") {
                binding_pattern(left, src, names);
            }
        }
        "pair_pattern" => {
            if let Some(value) = n.child_by_field_name("value") {
                binding_pattern(value, src, names);
            }
        }
        "parameters"
        | "lambda_parameters"
        | "formal_parameters"
        | "typed_parameter"
        | "pattern_list"
        | "tuple_pattern"
        | "list_pattern"
        | "list_splat_pattern"
        | "dictionary_splat_pattern"
        | "object_pattern"
        | "array_pattern"
        | "rest_pattern" => {
            let mut cursor = n.walk();
            for c in n.named_children(&mut cursor) {
                if n.child_by_field_name("type") != Some(c) {
                    binding_pattern(c, src, names);
                }
            }
        }
        // Attribute/subscript writes do not bind their receiver. Types and
        // default expressions likewise do not declare parameter names.
        _ => {}
    }
}

impl ScopeNames {
    fn collect(root: Node, src: &[u8], python: bool) -> Self {
        let mut names = Self::default();
        names.visit(root, None, src, python);
        names
    }

    fn bind(&mut self, scope: Option<Node>, pattern: Node, src: &[u8]) {
        if let Some(scope) = scope {
            binding_pattern(
                pattern,
                src,
                &mut self.0.entry(scope.id()).or_default().bound,
            );
        }
    }

    fn visit<'t>(&mut self, n: Node<'t>, mut scope: Option<Node<'t>>, src: &[u8], python: bool) {
        let function = function_scope(n);
        if (function || matches!(n.kind(), "class_definition" | "class_declaration"))
            && let Some(name) = n.child_by_field_name("name")
        {
            self.bind(scope, name, src);
        }
        if function
            || (python && n.kind() == "class_definition")
            || (!python
                && matches!(
                    n.kind(),
                    "statement_block" | "for_statement" | "for_in_statement" | "catch_clause"
                ))
        {
            scope = Some(n);
            self.0.entry(n.id()).or_default();
            if let Some(parameters) = n
                .child_by_field_name("parameters")
                .or_else(|| n.child_by_field_name("parameter"))
            {
                self.bind(scope, parameters, src);
            }
        }
        match n.kind() {
            "assignment" | "augmented_assignment" | "for_statement" if python => {
                if let Some(left) = n.child_by_field_name("left") {
                    self.bind(scope, left, src);
                }
            }
            "as_pattern" if python => {
                if let Some(alias) = n.child_by_field_name("alias") {
                    // The Python grammar wraps with/except aliases in an
                    // as_pattern_target rather than an identifier directly.
                    let mut cursor = alias.walk();
                    for c in alias.named_children(&mut cursor) {
                        self.bind(scope, c, src);
                    }
                    self.bind(scope, alias, src);
                }
            }
            "variable_declarator" => {
                let binding_scope = if n
                    .parent()
                    .is_some_and(|p| p.kind() == "variable_declaration")
                {
                    // JS `var` belongs to the function, while let/const use
                    // the enclosing block.
                    let mut parent = n.parent();
                    while parent.is_some_and(|p| !function_scope(p)) {
                        parent = parent.and_then(|p| p.parent());
                    }
                    parent
                } else {
                    scope
                };
                if let Some(name) = n.child_by_field_name("name") {
                    self.bind(binding_scope, name, src);
                }
            }
            "global_statement" | "nonlocal_statement" if python => {
                if let Some(scope) = scope {
                    let names = self.0.entry(scope.id()).or_default();
                    let set = if n.kind() == "global_statement" {
                        &mut names.global
                    } else {
                        &mut names.nonlocal
                    };
                    let mut cursor = n.walk();
                    for c in n.named_children(&mut cursor) {
                        set.insert(text(c, src).into());
                    }
                }
            }
            "import_statement" | "import_from_statement" if python => {
                if let Some(scope) = scope {
                    let names = &mut self.0.entry(scope.id()).or_default().bound;
                    let mut cursor = n.walk();
                    for c in n.children_by_field_name("name", &mut cursor) {
                        let local = c.child_by_field_name("alias").unwrap_or(c);
                        names.insert(text(local, src).split('.').next().unwrap_or("").into());
                    }
                }
            }
            _ => {}
        }
        let mut cursor = n.walk();
        for c in n.named_children(&mut cursor) {
            self.visit(c, scope, src, python);
        }
    }

    fn name(&self, mut n: Node, name: &str, python: bool) -> String {
        let root = name.split('.').next().unwrap_or(name);
        let mut inside_function = false;
        loop {
            // Python methods do not close over class attributes. Decorators
            // in the class body do, before entering the method's scope.
            if !(python && inside_function && n.kind() == "class_definition")
                && let Some(names) = self.0.get(&n.id())
            {
                if names.global.contains(root) {
                    return name.into();
                }
                if names.bound.contains(root) && !names.nonlocal.contains(root) {
                    return format!("@{}:{name}", n.start_byte());
                }
            }
            inside_function |= function_scope(n);
            let Some(parent) = n.parent() else {
                return name.into();
            };
            n = parent;
        }
    }
}

struct Scan<'a> {
    file: &'a str,
    /// The file's directory, for relative imports.
    dir: PathBuf,
    src: &'a [u8],
    python: bool,
    django: bool,
    names: ScopeNames,
    facts: Facts,
    routes: Vec<Route>,
    requests: Vec<Request>,
}

impl Scan<'_> {
    fn record_binding(&mut self, site: Node, name: &str) {
        if !self.python {
            return;
        }
        let mut parent = site.parent();
        let mut conditional = false;
        while let Some(n) = parent {
            if function_scope(n) || n.kind() == "class_definition" {
                break;
            }
            conditional |= matches!(
                n.kind(),
                "if_statement" | "for_statement" | "while_statement" | "try_statement"
            );
            parent = n.parent();
        }
        if !self.facts.assigned.insert(name.into()) || conditional {
            self.facts.uncertain.insert(name.into());
        }
    }

    fn name(&self, n: Node, name: &str) -> String {
        self.names.name(n, name, self.python)
    }

    fn text_name(&self, n: Node) -> String {
        self.name(n, text(n, self.src))
    }

    fn route(
        &mut self,
        site: Node,
        methods: Vec<String>,
        path: String,
        router: &str,
        kind: Kind,
        handler: Option<(String, usize)>,
    ) {
        for method in methods {
            self.routes.push(Route {
                file: self.file.to_string(),
                line: site.start_position().row + 1,
                method,
                path: path.clone(),
                router: self.name(site, router),
                kind,
                handler: handler.clone(),
            });
        }
    }

    fn visit(&mut self, n: Node) {
        match n.kind() {
            // x = APIRouter(prefix="/p"), const users = require("./users"),
            // API = "/api"
            "assignment" | "variable_declarator" => self.assign(n),
            "assignment_expression" if !self.python => self.exports_assignment(n),
            "decorated_definition" if self.python => self.decorated(n),
            "function_definition" if self.python => self.factory(n),
            "as_pattern" if self.python => self.with_client(n),
            "call" | "call_expression" => self.call(n),
            "import_statement" | "import_from_statement" if self.python => self.python_import(n),
            "import_statement" => self.js_import(n),
            "export_statement" => self.js_export(n),
            _ => {}
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            self.visit(c);
        }
    }

    /// The module a `./`-relative JS specifier names.
    fn js_module(&self, spec: &str) -> Option<ModRef> {
        (spec.starts_with("./") || spec.starts_with("../"))
            .then(|| ModRef::Path(lexical(&self.dir.join(spec))))
    }

    /// `require("./m")`
    fn required(&self, n: Node) -> Option<ModRef> {
        if n.kind() != "call_expression" {
            return None;
        }
        let f = n.child_by_field_name("function")?;
        if text(f, self.src) != "require" {
            return None;
        }
        let (pos, _) = arguments(n.child_by_field_name("arguments")?, self.src);
        self.js_module(&literal(*pos.first()?, self.src)?)
    }

    fn test_application(&self, n: Node) -> Option<Target> {
        if !self.python || n.kind() != "call" {
            return None;
        }
        let function = n.child_by_field_name("function")?;
        if text(function, self.src).rsplit('.').next() != Some("TestClient") {
            return None;
        }
        let (pos, kw) = arguments(n.child_by_field_name("arguments")?, self.src);
        Some(
            pos.first()
                .or_else(|| kw.get("app"))
                .and_then(|argument| self.target(*argument))
                .unwrap_or(Target::Unknown),
        )
    }

    fn with_client(&mut self, n: Node) {
        let Some(value) = n.named_child(0) else {
            return;
        };
        let Some(alias) = n.child_by_field_name("alias") else {
            return;
        };
        let Some(application) = self.test_application(value) else {
            return;
        };
        let name = self.name(alias, text(alias, self.src));
        self.record_binding(n, &name);
        self.facts.clients.insert(name, application);
    }

    fn factory(&mut self, n: Node) {
        let (Some(name), Some(body)) =
            (n.child_by_field_name("name"), n.child_by_field_name("body"))
        else {
            return;
        };
        fn returns<'a>(n: Node<'a>, out: &mut Vec<Node<'a>>, generator: &mut bool) {
            if function_scope(n) || n.kind() == "class_definition" {
                return;
            }
            if n.kind() == "return_statement" {
                out.push(n);
                return;
            }
            *generator |= n.kind() == "yield";
            let mut cursor = n.walk();
            for child in n.named_children(&mut cursor) {
                returns(child, out, generator);
            }
        }
        let mut found = vec![];
        let mut generator = false;
        returns(body, &mut found, &mut generator);
        let mut cursor = n.walk();
        let asynchronous = n.children(&mut cursor).any(|c| c.kind() == "async");
        let decorated = n
            .parent()
            .is_some_and(|p| p.kind() == "decorated_definition");
        let target = if !generator
            && !asynchronous
            && !decorated
            && found.len() == 1
            && found[0].parent().is_some_and(|p| p.id() == body.id())
        {
            found[0]
                .named_child(0)
                .and_then(|value| self.target(value))
                .unwrap_or(Target::Unknown)
        } else {
            Target::Unknown
        };
        let name = self.text_name(name);
        self.record_binding(n, &name);
        self.facts.factories.insert(name, target);
    }

    fn request(&mut self, n: Node, method: Option<String>, path: String) {
        let receiver = n
            .child_by_field_name("function")
            .and_then(|f| f.child_by_field_name("object"));
        let application = receiver.and_then(|r| self.test_application(r));
        let client = receiver
            .filter(|r| matches!(r.kind(), "identifier" | "attribute" | "member_expression"))
            .map(|r| self.text_name(r));
        self.requests.push(Request {
            file: self.file.to_string(),
            line: n.start_position().row + 1,
            method,
            path,
            caller: enclosing(n, self.src),
            client,
            application,
        });
    }

    fn assign(&mut self, n: Node) {
        let (Some(left), Some(right)) = (
            n.child_by_field_name(if self.python { "left" } else { "name" }),
            n.child_by_field_name(if self.python { "right" } else { "value" }),
        ) else {
            return;
        };
        let name = self.name(left, text(left, self.src));
        self.record_binding(n, &name);
        if let Some(application) = self.test_application(right) {
            self.facts.clients.insert(name, application);
            return;
        }
        if self.python
            && left.kind() == "identifier"
            && let Some(target) = self.target(right)
        {
            self.facts.aliases.insert(name.clone(), target);
        }
        if left.kind() == "identifier"
            && top_level(n, self.python)
            && let Some(v) = literal(right, self.src)
        {
            self.facts.consts.insert(name.clone(), v);
        }
        if !self.python
            && let Some(m) = self.required(right)
        {
            match left.kind() {
                "identifier" => {
                    self.facts
                        .bindings
                        .insert(name, Binding::Name(m, "default".into()));
                }
                // const { router, api: v1 } = require("./routes")
                "object_pattern" => {
                    let mut cursor = left.walk();
                    for p in left.named_children(&mut cursor) {
                        if p.kind() == "shorthand_property_identifier_pattern" {
                            let t = text(p, self.src).to_string();
                            self.facts
                                .bindings
                                .insert(self.name(p, &t), Binding::Name(m.clone(), t));
                        } else if p.kind() == "pair_pattern"
                            && let (Some(k), Some(v)) =
                                (pair_key(p, self.src), p.child_by_field_name("value"))
                            && v.kind() == "identifier"
                        {
                            self.facts.bindings.insert(
                                self.name(v, text(v, self.src)),
                                Binding::Name(m.clone(), k),
                            );
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if !matches!(right.kind(), "call" | "new_expression" | "call_expression") {
            return;
        }
        let Some(f) = right.child_by_field_name(if right.kind() == "new_expression" {
            "constructor"
        } else {
            "function"
        }) else {
            return;
        };
        let ctor = text(f, self.src).rsplit('.').next().unwrap_or("");
        let kind = match (self.python, ctor) {
            (true, "APIRouter" | "FastAPI" | "Router" | "Starlette") => Kind::FastApi,
            (true, "Blueprint" | "Flask") => Kind::Flask,
            (false, "Router" | "express") => Kind::Express,
            _ => return,
        };
        let prefix = right.child_by_field_name("arguments").and_then(|args| {
            let (pos, kw) = arguments(args, self.src);
            if self.python {
                kw.get("prefix")
                    .or_else(|| kw.get("url_prefix"))
                    .map(|v| self.prefix_arg(*v))
            } else {
                // koa-router: new Router({ prefix: "/api" })
                pos.first()
                    .filter(|o| o.kind() == "object")
                    .and_then(|o| object_value(*o, "prefix", self.src))
                    .map(|v| self.prefix_arg(v))
            }
        });
        self.facts.routers.insert(name, Router { kind, prefix });
    }

    /// `module.exports = router`, `module.exports = { router }`,
    /// `exports.router = router`.
    fn exports_assignment(&mut self, n: Node) {
        let (Some(left), Some(right)) = (
            n.child_by_field_name("left"),
            n.child_by_field_name("right"),
        ) else {
            return;
        };
        let l = text(left, self.src);
        if l == "module.exports" {
            if right.kind() == "identifier" {
                self.facts
                    .exports
                    .insert("default".into(), text(right, self.src).into());
            } else if right.kind() == "object" {
                let mut cursor = right.walk();
                for p in right.named_children(&mut cursor) {
                    if p.kind() == "shorthand_property_identifier" {
                        let t = text(p, self.src).to_string();
                        self.facts.exports.insert(t.clone(), t);
                    } else if p.kind() == "pair"
                        && let (Some(k), Some(v)) =
                            (pair_key(p, self.src), p.child_by_field_name("value"))
                        && v.kind() == "identifier"
                    {
                        self.facts.exports.insert(k, text(v, self.src).into());
                    }
                }
            }
        } else if let Some(name) = l
            .strip_prefix("module.exports.")
            .or_else(|| l.strip_prefix("exports."))
            && right.kind() == "identifier"
        {
            self.facts
                .exports
                .insert(name.into(), text(right, self.src).into());
        }
    }

    /// `export default router`, `export const router = ...`, `export {
    /// router as api }`, `export { router } from "./r"`, TS `export =
    /// router`.
    fn js_export(&mut self, n: Node) {
        let mut cursor = n.walk();
        let children = n.children(&mut cursor).collect::<Vec<_>>();
        let default = children.iter().any(|c| c.kind() == "default");
        if let Some(d) = n.child_by_field_name("declaration") {
            if matches!(d.kind(), "lexical_declaration" | "variable_declaration") {
                let mut cursor = d.walk();
                for v in d.named_children(&mut cursor) {
                    if let Some(name) = v
                        .child_by_field_name("name")
                        .filter(|x| x.kind() == "identifier")
                    {
                        let t = text(name, self.src).to_string();
                        self.facts.exports.insert(t.clone(), t);
                    }
                }
            } else if let Some(name) = d.child_by_field_name("name") {
                let t = text(name, self.src).to_string();
                let key = if default { "default".into() } else { t.clone() };
                self.facts.exports.insert(key, t);
            }
            return;
        }
        if let Some(v) = n.child_by_field_name("value") {
            if default && v.kind() == "identifier" {
                self.facts
                    .exports
                    .insert("default".into(), text(v, self.src).into());
            }
            return;
        }
        let from = n
            .child_by_field_name("source")
            .and_then(|s| literal(s, self.src))
            .and_then(|s| self.js_module(&s));
        let assigns = children.iter().any(|c| c.kind() == "=");
        for c in &children {
            if c.kind() == "export_clause" {
                let mut cursor = c.walk();
                for spec in c.named_children(&mut cursor) {
                    let Some(name) = spec.child_by_field_name("name") else {
                        continue;
                    };
                    let name = text(name, self.src).to_string();
                    let alias = spec
                        .child_by_field_name("alias")
                        .map(|a| text(a, self.src).to_string())
                        .unwrap_or_else(|| name.clone());
                    if let Some(m) = &from {
                        // A re-export binds the exported name here.
                        self.facts
                            .bindings
                            .insert(alias.clone(), Binding::Name(m.clone(), name));
                        self.facts.exports.insert(alias.clone(), alias);
                    } else {
                        self.facts.exports.insert(alias, name);
                    }
                }
            } else if assigns && c.kind() == "identifier" {
                self.facts
                    .exports
                    .insert("default".into(), text(*c, self.src).into());
            }
        }
    }

    /// `import a.b as m`, `from .routers import items`, `from . import bp`.
    fn python_import(&mut self, n: Node) {
        let mut cursor = n.walk();
        if n.kind() == "import_statement" {
            for c in n.children_by_field_name("name", &mut cursor) {
                if c.kind() == "dotted_name" {
                    let d = text(c, self.src).to_string();
                    self.facts
                        .bindings
                        .insert(self.name(c, &d), Binding::Module(ModRef::Dotted(d)));
                } else if c.kind() == "aliased_import"
                    && let (Some(name), Some(alias)) = (
                        c.child_by_field_name("name"),
                        c.child_by_field_name("alias"),
                    )
                {
                    self.facts.bindings.insert(
                        self.text_name(alias),
                        Binding::Module(ModRef::Dotted(text(name, self.src).into())),
                    );
                }
            }
            return;
        }
        let Some(module) = n.child_by_field_name("module_name") else {
            return;
        };
        let m = match module.kind() {
            "dotted_name" => ModRef::Dotted(text(module, self.src).into()),
            "relative_import" => {
                let raw = text(module, self.src);
                let level = raw.chars().take_while(|c| *c == '.').count();
                let mut base = self.dir.clone();
                for _ in 1..level {
                    base.pop();
                }
                for part in raw[level..].split('.').filter(|p| !p.is_empty()) {
                    base.push(part.trim());
                }
                ModRef::Path(base)
            }
            _ => return,
        };
        for c in n.children_by_field_name("name", &mut cursor) {
            let (name, local) = if c.kind() == "aliased_import" {
                let (Some(name), Some(alias)) = (
                    c.child_by_field_name("name"),
                    c.child_by_field_name("alias"),
                ) else {
                    continue;
                };
                (text(name, self.src), text(alias, self.src))
            } else {
                (text(c, self.src), text(c, self.src))
            };
            self.facts
                .bindings
                .insert(self.name(c, local), Binding::Name(m.clone(), name.into()));
        }
    }

    /// `import users from "./users"`, `import { router as api } from`,
    /// `import * as routes from`.
    fn js_import(&mut self, n: Node) {
        let Some(m) = n
            .child_by_field_name("source")
            .and_then(|s| literal(s, self.src))
            .and_then(|s| self.js_module(&s))
        else {
            return;
        };
        let mut cursor = n.walk();
        for clause in n
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "import_clause")
        {
            let mut inner = clause.walk();
            for c in clause.named_children(&mut inner) {
                match c.kind() {
                    "identifier" => {
                        self.facts.bindings.insert(
                            text(c, self.src).into(),
                            Binding::Name(m.clone(), "default".into()),
                        );
                    }
                    "namespace_import" => {
                        if let Some(id) = c.named_child(0) {
                            self.facts
                                .bindings
                                .insert(text(id, self.src).into(), Binding::Module(m.clone()));
                        }
                    }
                    "named_imports" => {
                        let mut specs = c.walk();
                        for spec in c.named_children(&mut specs) {
                            let Some(name) = spec.child_by_field_name("name") else {
                                continue;
                            };
                            let name = text(name, self.src).to_string();
                            let local = spec
                                .child_by_field_name("alias")
                                .map(|a| text(a, self.src).to_string())
                                .unwrap_or_else(|| name.clone());
                            self.facts
                                .bindings
                                .insert(local, Binding::Name(m.clone(), name));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// What an include call's router argument names.
    fn target(&self, n: Node) -> Option<Target> {
        match n.kind() {
            "identifier" => Some(Target::Name(self.text_name(n))),
            "attribute" | "member_expression" => {
                let obj = n.child_by_field_name("object")?;
                let prop = n.child_by_field_name(if n.kind() == "attribute" {
                    "attribute"
                } else {
                    "property"
                })?;
                Some(Target::Attr(
                    self.text_name(obj),
                    text(prop, self.src).into(),
                ))
            }
            "call_expression" => {
                if let Some(m) = self.required(n) {
                    return Some(Target::Module(m));
                }
                // koa-router: app.use(router.routes())
                let f = n.child_by_field_name("function")?;
                if f.kind() == "member_expression"
                    && f.child_by_field_name("property")
                        .is_some_and(|p| matches!(text(p, self.src), "routes" | "middleware"))
                {
                    return self.target(f.child_by_field_name("object")?);
                }
                None
            }
            "call" if self.python => {
                let args = n.child_by_field_name("arguments")?;
                let (pos, kw) = arguments(args, self.src);
                if !pos.is_empty() || !kw.is_empty() {
                    return Some(Target::Unknown);
                }
                let function = n.child_by_field_name("function")?;
                self.target(function)
                    .map(|target| Target::Factory(Box::new(target)))
            }
            _ => None,
        }
    }

    fn prefix_arg(&self, n: Node) -> PrefixArg {
        if let Some(l) = literal(n, self.src) {
            return PrefixArg::Literal(l);
        }
        match n.kind() {
            "identifier" | "shorthand_property_identifier" => PrefixArg::Name(self.text_name(n)),
            "none" | "null" | "undefined" => PrefixArg::Absent,
            _ => PrefixArg::Unknown,
        }
    }

    /// `path("blog/", include("blog.urls"))`, `re_path(r"^blog/",
    /// include(blog.urls))`.
    fn django_include(&mut self, pattern: &str, call: Node, regex: bool) {
        let prefix = if regex {
            let fixed = pattern.trim_start_matches('^');
            if fixed.chars().any(|c| "()[]?*+\\|{}$".contains(c)) {
                format!("{UNKNOWN}{}", if fixed.ends_with('/') { "/" } else { "" })
            } else {
                fixed.to_string()
            }
        } else {
            pattern.to_string()
        };
        let Some(args) = call.child_by_field_name("arguments") else {
            return;
        };
        let (pos, _) = arguments(args, self.src);
        let Some(mut first) = pos.first().copied() else {
            return;
        };
        if first.kind() == "tuple"
            && let Some(inner) = first.named_child(0)
        {
            first = inner;
        }
        let child = match literal(first, self.src) {
            Some(m) => Target::Module(ModRef::Dotted(m)),
            None => match self.target(first) {
                Some(t) => t,
                None => return,
            },
        };
        self.facts.includes.push(Include {
            parent: self.name(call, DJANGO),
            child,
            prefix: Some(PrefixArg::Literal(prefix)),
            kind: Kind::Django,
        });
    }

    fn decorated(&mut self, n: Node) {
        let Some(def) = n.child_by_field_name("definition") else {
            return;
        };
        let Some(name) = def.child_by_field_name("name") else {
            return;
        };
        let handler = (
            text(name, self.src).to_string(),
            name.start_position().row + 1,
        );
        let mut cursor = n.walk();
        for d in n.children(&mut cursor).filter(|c| c.kind() == "decorator") {
            let Some(call) = d.named_child(0).filter(|c| c.kind() == "call") else {
                continue;
            };
            let (Some(f), Some(args)) = (
                call.child_by_field_name("function"),
                call.child_by_field_name("arguments"),
            ) else {
                continue;
            };
            if f.kind() != "attribute" {
                continue;
            }
            let (Some(obj), Some(attr)) = (
                f.child_by_field_name("object"),
                f.child_by_field_name("attribute"),
            ) else {
                continue;
            };
            let verb = text(attr, self.src);
            let (pos, kw) = arguments(args, self.src);
            let Some(path) = pos
                .first()
                .and_then(|p| literal(*p, self.src))
                .filter(|p| looks_like_path(p) || p.is_empty())
            else {
                continue;
            };
            let methods = if VERBS.contains(&verb) {
                vec![verb.to_uppercase()]
            } else if verb == "websocket" || verb == "websocket_route" {
                vec!["WEBSOCKET".into()]
            } else if verb == "route" || verb == "api_route" {
                kw.get("methods")
                    .map(|m| string_list(*m, self.src))
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| vec!["GET".into()])
            } else {
                continue;
            };
            self.route(
                d,
                methods,
                path,
                text(obj, self.src),
                Kind::FastApi,
                Some(handler.clone()),
            );
        }
    }

    fn call(&mut self, n: Node) {
        let (Some(f), Some(args)) = (
            n.child_by_field_name("function"),
            n.child_by_field_name("arguments"),
        ) else {
            return;
        };
        let (pos, kw) = arguments(args, self.src);
        let member = matches!(f.kind(), "attribute" | "member_expression");
        let (obj, name) = if member {
            let o = f
                .child_by_field_name("object")
                .map(|o| text(o, self.src))
                .unwrap_or("");
            let a = f
                .child_by_field_name(if f.kind() == "attribute" {
                    "attribute"
                } else {
                    "property"
                })
                .map(|a| text(a, self.src))
                .unwrap_or("");
            (o, a)
        } else {
            ("", text(f, self.src))
        };
        let first = pos.first().and_then(|p| literal(*p, self.src));
        // Mounts: include_router / mount / register_blueprint / use.
        if member {
            let include = match (self.python, name) {
                (true, "include_router") => pos
                    .first()
                    .map(|c| (Kind::FastApi, *c, kw.get("prefix").copied())),
                (true, "register_blueprint") => pos
                    .first()
                    .map(|c| (Kind::Flask, *c, kw.get("url_prefix").copied())),
                (true, "mount") => pos.get(1).or_else(|| kw.get("app")).map(|c| {
                    (
                        Kind::FastApi,
                        *c,
                        pos.first().or_else(|| kw.get("path")).copied(),
                    )
                }),
                _ => None,
            };
            if let Some((kind, child, prefix)) = include {
                if let Some(child) = self.target(child) {
                    let prefix = prefix.map(|p| self.prefix_arg(p));
                    self.facts.includes.push(Include {
                        parent: self.name(n, obj),
                        child,
                        prefix,
                        kind,
                    });
                }
                return;
            }
            if !self.python && name == "use" && !pos.is_empty() {
                let (prefix, rest) = match pos[0].kind() {
                    "string" | "template_string" => (Some(self.prefix_arg(pos[0])), &pos[1..]),
                    // `app.use(base, router)` with `base` a path constant, or
                    // `app.use(auth, router)` with middleware first.
                    "identifier" if pos.len() > 1 => {
                        (Some(PrefixArg::Const(self.text_name(pos[0]))), &pos[..])
                    }
                    _ => (None, &pos[..]),
                };
                for (i, a) in rest.iter().enumerate() {
                    let Some(child) = self.target(*a) else {
                        continue;
                    };
                    // In `use(x, router)` the `x` may itself be a router.
                    let prefix = if i == 0 && matches!(prefix, Some(PrefixArg::Const(_))) {
                        None
                    } else {
                        prefix.clone()
                    };
                    self.facts.includes.push(Include {
                        parent: self.name(n, obj),
                        child,
                        prefix,
                        kind: Kind::Express,
                    });
                }
                return;
            }
        }
        // Registrations: add_api_route / add_url_rule / Django path().
        if member && (name == "add_api_route" || name == "add_url_rule") {
            let Some(path) = first.clone().filter(|p| looks_like_path(p)) else {
                return;
            };
            let handler = kw
                .get("endpoint")
                .or_else(|| kw.get("view_func"))
                .copied()
                .or_else(|| pos.get(if name == "add_url_rule" { 2 } else { 1 }).copied())
                .and_then(|h| handler_name(h, self.src))
                .map(|h| (h, 0));
            let methods = kw
                .get("methods")
                .map(|m| string_list(*m, self.src))
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| vec!["GET".into()]);
            let kind = if name == "add_url_rule" {
                Kind::Flask
            } else {
                Kind::FastApi
            };
            self.route(n, methods, path, obj, kind, handler);
            return;
        }
        if self.django && !member && (name == "path" || name == "re_path") {
            let (Some(path), Some(view)) = (first.clone(), pos.get(1)) else {
                return;
            };
            if view.kind() == "call"
                && view
                    .child_by_field_name("function")
                    .is_some_and(|f| text(f, self.src) == "include")
            {
                self.django_include(&path, *view, name == "re_path");
                return;
            }
            let handler = handler_name(*view, self.src).map(|h| (h, 0));
            if name == "re_path" {
                // Only fixed regexes: `^about/$`.
                let fixed = path.trim_start_matches('^').trim_end_matches('$');
                if fixed.chars().any(|c| "()[]?*+\\|{}".contains(c)) {
                    return;
                }
                self.route(
                    n,
                    vec!["ANY".into()],
                    fixed.to_string(),
                    DJANGO,
                    Kind::Django,
                    handler,
                );
                return;
            }
            self.route(n, vec!["ANY".into()], path, DJANGO, Kind::Django, handler);
            return;
        }
        // Express-style: app.get("/p", ..., handler)
        if !self.python && member && (VERBS.contains(&name) || name == "all") && pos.len() >= 2 {
            let receiver = obj.rsplit('.').next().unwrap_or(obj).to_ascii_lowercase();
            if (["app", "router", "server", "api", "route", "routes"]
                .iter()
                .any(|k| receiver.ends_with(k))
                || self.facts.routers.contains_key(&self.name(n, obj)))
                && let Some(path) = first.clone().filter(|p| p.starts_with('/'))
            {
                let handler = pos
                    .last()
                    .and_then(|h| handler_name(*h, self.src))
                    .map(|h| (h, 0));
                let method = if name == "all" {
                    "ANY".to_string()
                } else {
                    name.to_uppercase()
                };
                self.route(n, vec![method], path, obj, Kind::Express, handler);
                return;
            }
        }
        // Requests.
        let method = if member
            && (client_receiver(obj) || self.facts.clients.contains_key(&self.name(n, obj)))
        {
            if VERBS.contains(&name) {
                Some(Some(name.to_uppercase()))
            } else if name == "request" {
                // requests.request("GET", url)
                let m = first.clone().map(|m| m.to_uppercase());
                let url = pos.get(1).and_then(|p| literal(*p, self.src));
                if let (Some(m), Some(url)) = (m, url.filter(|u| looks_like_path(u))) {
                    self.request(n, Some(m), url);
                }
                return;
            } else if name == "websocket_connect" {
                Some(Some("WEBSOCKET".into()))
            } else {
                None
            }
        } else if !member && (name == "fetch" || name == "axios") {
            Some(None)
        } else {
            None
        };
        if let Some(method) = method
            && let Some(url) = first.filter(|u| looks_like_path(u))
        {
            self.request(n, method, url);
        }
    }
}

/// Segments of a route template or request path; parameters and
/// placeholders become `*`, a `{x:path}` parameter `**` (the rest), and a
/// leading placeholder (`f"{base}/items"`, a prefix this pass cannot read)
/// `+`, an unknown base (see [`matches`]).
fn segments(path: &str) -> Vec<String> {
    let mut p = path;
    if let Some(at) = p.find("://") {
        let rest = &p[at + 3..];
        p = rest.find('/').map_or("", |i| &rest[i..]);
    }
    let mut out = vec![];
    if let Some(rest) = p.strip_prefix(UNKNOWN) {
        out.push("+".to_string());
        p = rest;
    }
    let p = p.split(['?', '#']).next().unwrap_or("");
    out.extend(p.split('/').filter(|s| !s.is_empty()).map(|s| {
        if s.contains("{}")
            || (s.starts_with('{') && s.ends_with('}') && !s.contains(":path"))
            || (s.starts_with('<') && s.ends_with('>'))
            || s.starts_with(':')
        {
            "*".to_string()
        } else if s.starts_with('{') && s.contains(":path") {
            "**".to_string()
        } else {
            s.to_string()
        }
    }));
    out
}

/// Whether a request's segments reach a route's. An unknown base stands for
/// one or more leading segments; on both sides, the rest must match.
fn matches(route: &[String], request: &[String]) -> bool {
    let base = |s: &[String]| s.first().is_some_and(|x| x == "+");
    match (base(route), base(request)) {
        (true, true) => exact(&route[1..], &request[1..]),
        (true, false) => (1..=request.len()).any(|k| exact(&route[1..], &request[k..])),
        (false, true) => exact(route, &request[1..]),
        (false, false) => exact(route, request),
    }
}

fn exact(route: &[String], request: &[String]) -> bool {
    let mut i = 0;
    for (k, r) in route.iter().enumerate() {
        if r == "**" {
            return request.len() > k;
        }
        let Some(q) = request.get(i) else {
            return false;
        };
        if r != "*" && q != "*" && r != q {
            return false;
        }
        i += 1;
    }
    i == request.len()
}

/// A router: its file (index in [`Corpus::files`]) and variable.
type Key = (usize, String);

/// A router mounted on `parent`.
#[derive(Debug, Clone)]
struct Mount {
    parent: Key,
    /// The include's prefix, if one was given.
    prefix: Option<String>,
    kind: Kind,
    /// Files whose include calls, imports or constants made this mount.
    via: Vec<usize>,
}

/// One file read by [`scan`].
struct Scanned {
    facts: Facts,
    routes: Vec<Route>,
    requests: Vec<Request>,
}

fn scan(path: &Path) -> Option<Scanned> {
    let (tag, language) = language(path)?;
    let parsed = parse_cache::parse(path, tag, language)?;
    Some(scan_parsed(path, tag == "python", &parsed))
}

fn scan_parsed(path: &Path, python: bool, parsed: &super::engine_parser::Parsed) -> Scanned {
    let src = parsed.source.as_slice();
    let file = path.to_string_lossy().to_string();
    let mut scan = Scan {
        file: &file,
        dir: lexical(path)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        src,
        python,
        django: python && String::from_utf8_lossy(src).contains("urlpatterns"),
        names: ScopeNames::collect(parsed.tree.root_node(), src, python),
        facts: Facts {
            python,
            ..Default::default()
        },
        routes: vec![],
        requests: vec![],
    };
    scan.visit(parsed.tree.root_node());
    Scanned {
        facts: scan.facts,
        routes: scan.routes,
        requests: scan.requests,
    }
}

/// Text every file with an include call contains. Look for names rather than
/// adjacent punctuation: whitespace and JS comments may precede the `(`.
static MOUNT_TEXT: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"include|register_blueprint|\b(?:mount|use)\b").expect("valid")
});

/// Whether `path` may contain an include call (a cheap text test).
fn may_mount(path: &Path) -> bool {
    language(path).is_some() && parse_cache::read(path).is_ok_and(|b| MOUNT_TEXT.is_match(&b))
}

/// Change detection must inspect the current file even if another extraction
/// run still holds an older version in the shared source/tree cache.
fn freshly_mounts(path: &Path) -> bool {
    let Some((tag, language)) = language(path) else {
        return false;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    MOUNT_TEXT.is_match(&bytes)
        && parse_cache::parse_bytes(language, bytes).is_some_and(|parsed| {
            !scan_parsed(path, tag == "python", &parsed)
                .facts
                .includes
                .is_empty()
        })
}

/// The files of a run and what they declare, read on demand.
struct Corpus {
    files: Vec<PathBuf>,
    index: FxHashMap<PathBuf, usize>,
    facts: Vec<Option<Facts>>,
    scanned: Vec<bool>,
    /// Files answering to a dotted Python module name.
    dotted: FxHashMap<String, Vec<usize>>,
    mounts: FxHashMap<Key, Vec<Mount>>,
}

impl Corpus {
    fn new(files: Vec<PathBuf>) -> Self {
        let index = files
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect();
        let n = files.len();
        Corpus {
            files,
            index,
            facts: vec![None; n],
            scanned: vec![false; n],
            dotted: FxHashMap::default(),
            mounts: FxHashMap::default(),
        }
    }

    fn set(&mut self, i: usize, facts: Option<Facts>) {
        self.facts[i] = facts;
        self.scanned[i] = true;
    }

    fn facts(&mut self, i: usize) -> Option<&Facts> {
        if !self.scanned[i] {
            self.scanned[i] = true;
            self.facts[i] = scan(&self.files[i]).map(|s| s.facts);
        }
        self.facts[i].as_ref()
    }

    fn router(&self, key: &Key) -> Option<&Router> {
        self.facts.get(key.0)?.as_ref()?.routers.get(&key.1)
    }

    /// The file module `m`, named in file `from`, is.
    fn module(&mut self, from: usize, m: &ModRef) -> Option<usize> {
        let python = self.facts(from).is_some_and(|f| f.python);
        match m {
            ModRef::Path(base) => {
                let s = base.to_string_lossy().into_owned();
                let mut candidates = vec![];
                if python {
                    candidates.push(PathBuf::from(format!("{s}.py")));
                    candidates.push(base.join("__init__.py"));
                } else {
                    candidates.push(base.clone());
                    // TS ESM imports name `./users.js` for users.ts.
                    if let Some(stem) = s.strip_suffix(".js") {
                        candidates
                            .extend([".ts", ".tsx"].map(|e| PathBuf::from(format!("{stem}{e}"))));
                    }
                    for e in [".js", ".ts", ".tsx", ".jsx", ".mjs", ".cjs", ".mts", ".cts"] {
                        candidates.push(PathBuf::from(format!("{s}{e}")));
                    }
                    for e in ["js", "ts", "tsx", "jsx", "mjs", "cjs"] {
                        candidates.push(base.join(format!("index.{e}")));
                    }
                }
                candidates.iter().find_map(|c| self.index.get(c).copied())
            }
            ModRef::Dotted(d) if python && !d.is_empty() => {
                if !self.dotted.contains_key(d) {
                    let rel = d.replace('.', "/");
                    let (file, package) = (format!("{rel}.py"), format!("{rel}/__init__.py"));
                    let hits = (0..self.files.len())
                        .filter(|&i| {
                            self.files[i].ends_with(&file) || self.files[i].ends_with(&package)
                        })
                        .collect();
                    self.dotted.insert(d.clone(), hits);
                }
                let hits = &self.dotted[d];
                if hits.len() <= 1 {
                    return hits.first().copied();
                }
                // Several files answer: the one nearest the importer, if one is.
                let at = &self.files[from];
                let shared = |i: usize| {
                    at.components()
                        .zip(self.files[i].components())
                        .take_while(|(a, b)| a == b)
                        .count()
                };
                let best = hits.iter().map(|&i| shared(i)).max()?;
                let mut nearest = hits.iter().filter(|&&i| shared(i) == best);
                let first = *nearest.next()?;
                nearest.next().is_none().then_some(first)
            }
            ModRef::Dotted(_) => None,
        }
    }

    /// The router `name` refers to in file `f`: one created there, or one it
    /// imports (through re-exports).
    fn local(&mut self, f: usize, name: &str, depth: usize, via: &mut Vec<usize>) -> Option<Key> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let facts = self.facts(f)?;
        if facts.uncertain.contains(name) {
            return None;
        }
        if facts.routers.contains_key(name) {
            return Some((f, name.to_string()));
        }
        if let Some(target) = facts.aliases.get(name).cloned() {
            return self.router_target_depth(f, &target, depth + 1, via);
        }
        let Binding::Name(m, n) = facts.bindings.get(name)?.clone() else {
            return None;
        };
        if depth >= MAX_DEPTH {
            return None;
        }
        let g = self.module(f, &m)?;
        via.push(g);
        self.exported(g, &n, depth + 1, via)
    }

    /// The router module `g` exports as `name` (Python: a module attribute).
    fn exported(
        &mut self,
        g: usize,
        name: &str,
        depth: usize,
        via: &mut Vec<usize>,
    ) -> Option<Key> {
        let facts = self.facts(g)?;
        let local = if facts.python {
            name.to_string()
        } else {
            facts.exports.get(name)?.clone()
        };
        self.local(g, &local, depth, via)
    }

    /// The module file `name` is bound to in file `f`: `import a.b as m`,
    /// `from pkg import mod`, `import * as m`, or a CommonJS exports object.
    fn module_of(
        &mut self,
        f: usize,
        name: &str,
        depth: usize,
        via: &mut Vec<usize>,
    ) -> Option<usize> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let facts = self.facts(f)?;
        let python = facts.python;
        match facts.bindings.get(name)?.clone() {
            Binding::Module(m) => self.module(f, &m),
            Binding::Name(m, n) if python => {
                if let Some(g) = self.module(f, &m.child(&n)) {
                    return Some(g);
                }
                // `from pkg import mod` where `pkg/__init__.py` binds `mod`.
                let g = self.module(f, &m)?;
                via.push(g);
                self.module_of(g, &n, depth + 1, via)
            }
            Binding::Name(m, n) if n == "default" => self.module(f, &m),
            Binding::Name(..) => None,
        }
    }

    /// The router an include call's argument names.
    fn router_target(&mut self, f: usize, t: &Target, via: &mut Vec<usize>) -> Option<Key> {
        self.router_target_depth(f, t, 0, via)
    }

    fn factory_target(
        &mut self,
        f: usize,
        target: &Target,
        depth: usize,
        via: &mut Vec<usize>,
    ) -> Option<Key> {
        if depth >= MAX_DEPTH {
            return None;
        }
        match target {
            Target::Name(name) => {
                let facts = self.facts(f)?;
                if facts.uncertain.contains(name) {
                    return None;
                }
                if let Some(alias) = facts.aliases.get(name).cloned() {
                    return self.factory_target(f, &alias, depth + 1, via);
                }
                if facts.factories.contains_key(name) {
                    return Some((f, name.clone()));
                }
                let Binding::Name(module, imported) = facts.bindings.get(name)?.clone() else {
                    return None;
                };
                let g = self.module(f, &module)?;
                via.push(g);
                self.factory_target(g, &Target::Name(imported), depth + 1, via)
            }
            Target::Attr(module, name) => {
                let g = self.module_of(f, module, 0, via)?;
                via.push(g);
                self.factory_target(g, &Target::Name(name.clone()), depth + 1, via)
            }
            _ => None,
        }
    }

    fn router_target_depth(
        &mut self,
        f: usize,
        t: &Target,
        depth: usize,
        via: &mut Vec<usize>,
    ) -> Option<Key> {
        if depth >= MAX_DEPTH {
            return None;
        }
        match t {
            Target::Name(n) => self.local(f, n, depth, via),
            Target::Attr(o, a) => {
                let g = self.module_of(f, o, 0, via)?;
                via.push(g);
                self.exported(g, a, depth + 1, via)
            }
            Target::Module(m) => {
                let g = self.module(f, m)?;
                via.push(g);
                self.exported(g, "default", depth + 1, via)
            }
            Target::Factory(target) => {
                let (g, name) = self.factory_target(f, target, depth + 1, via)?;
                let returned = self.facts(g)?.factories.get(&name)?.clone();
                self.router_target_depth(g, &returned, depth + 1, via)
            }
            Target::Unknown => None,
        }
    }

    /// The `urlpatterns` module a Django `include()` names.
    fn module_target(&mut self, f: usize, t: &Target, via: &mut Vec<usize>) -> Option<usize> {
        match t {
            Target::Factory(_) | Target::Unknown => None,
            Target::Module(m) => self.module(f, m),
            Target::Name(n) => self.module_of(f, n, 0, via),
            Target::Attr(o, a) => {
                // `import blog.urls` ... include(blog.urls)
                let dotted = format!("{o}.{a}");
                if self.facts(f)?.bindings.contains_key(&dotted) {
                    return self.module_of(f, &dotted, 0, via);
                }
                // `from . import blog` ... include(blog.urls)
                let g = self.module_of(f, o, 0, via)?;
                via.push(g);
                let package = self.files[g]
                    .file_name()
                    .is_some_and(|n| n == "__init__.py")
                    .then(|| self.files[g].parent().map(|p| p.join(a)))
                    .flatten()?;
                self.module(g, &ModRef::Path(package))
            }
        }
    }

    /// A module-level string constant `name` names in file `f`.
    fn constant(
        &mut self,
        f: usize,
        name: &str,
        depth: usize,
        via: &mut Vec<usize>,
    ) -> Option<String> {
        let facts = self.facts(f)?;
        if let Some(v) = facts.consts.get(name) {
            return Some(v.clone());
        }
        let Binding::Name(m, n) = facts.bindings.get(name)?.clone() else {
            return None;
        };
        if depth >= MAX_DEPTH {
            return None;
        }
        let g = self.module(f, &m)?;
        via.push(g);
        let gf = self.facts(g)?;
        let local = if gf.python {
            n
        } else {
            gf.exports.get(&n)?.clone()
        };
        self.constant(g, &local, depth + 1, via)
    }

    fn prefix(&mut self, f: usize, p: &PrefixArg, via: &mut Vec<usize>) -> Option<String> {
        match p {
            PrefixArg::Literal(s) => Some(s.clone()),
            PrefixArg::Name(n) => Some(
                self.constant(f, n, 0, via)
                    .unwrap_or_else(|| UNKNOWN.into()),
            ),
            PrefixArg::Const(n) => self.constant(f, n, 0, via),
            PrefixArg::Absent => None,
            PrefixArg::Unknown => Some(UNKNOWN.into()),
        }
    }

    /// Record every include call of the files read so far as a mount.
    fn link(&mut self) {
        for f in 0..self.files.len() {
            let includes = match &self.facts[f] {
                Some(x) if !x.includes.is_empty() => x.includes.clone(),
                _ => continue,
            };
            for inc in includes {
                let mut via = vec![f];
                let (parent, child) = if inc.kind == Kind::Django {
                    let Some(g) = self.module_target(f, &inc.child, &mut via) else {
                        continue;
                    };
                    ((f, DJANGO.to_string()), (g, DJANGO.to_string()))
                } else {
                    let Some(child) = self.router_target(f, &inc.child, &mut via) else {
                        continue;
                    };
                    let parent = self
                        .local(f, &inc.parent, 0, &mut via)
                        .unwrap_or_else(|| (f, inc.parent.clone()));
                    (parent, child)
                };
                if parent == child {
                    continue;
                }
                let prefix = inc
                    .prefix
                    .as_ref()
                    .and_then(|p| self.prefix(f, p, &mut via));
                via.sort_unstable();
                via.dedup();
                let mounts = self.mounts.entry(child).or_default();
                if !mounts
                    .iter()
                    .any(|m| m.parent == parent && m.prefix == prefix && m.kind == inc.kind)
                {
                    mounts.push(Mount {
                        parent,
                        prefix,
                        kind: inc.kind,
                        via,
                    });
                }
            }
        }
    }

    /// The mounts of `key` not already on the current chain.
    fn live_mounts(&self, key: &Key, chain: &[Key]) -> Vec<Mount> {
        self.mounts
            .get(key)
            .into_iter()
            .flatten()
            .filter(|m| m.parent != *key && !chain.contains(&m.parent))
            .cloned()
            .collect()
    }

    /// The paths a route of file `f` is served at, and the other files that
    /// shaped them.
    fn served(&mut self, f: usize, r: &Route) -> (Vec<(String, Key)>, Vec<usize>) {
        let mut via = vec![];
        let key = if r.kind == Kind::Django {
            (f, DJANGO.to_string())
        } else {
            self.local(f, &r.router, 0, &mut via)
                .unwrap_or_else(|| (f, r.router.clone()))
        };
        if key.0 != f {
            via.push(key.0);
        }
        let kind = self.router(&key).map_or(r.kind, |d| d.kind);
        let mut out = vec![];
        if kind == Kind::Flask {
            let mut prefixes = vec![];
            self.flask(&key, &mut vec![], &mut via, &mut prefixes);
            out.extend(
                prefixes
                    .into_iter()
                    .map(|(p, app)| (flask_rule(p.as_deref(), &r.path), app)),
            );
        } else {
            self.up(&key, kind, r.path.clone(), &mut vec![], &mut via, &mut out);
        }
        let mut seen = FxHashSet::default();
        out.retain(|p| seen.insert(p.clone()));
        via.retain(|g| *g != f);
        via.sort_unstable();
        via.dedup();
        (out, via)
    }

    /// FastAPI, Express and Django: the paths `path`, declared on `key`
    /// below its own prefix, is served at.
    fn up(
        &mut self,
        key: &Key,
        kind: Kind,
        path: String,
        chain: &mut Vec<Key>,
        via: &mut Vec<usize>,
        out: &mut Vec<(String, Key)>,
    ) {
        if out.len() >= MAX_PATHS {
            return;
        }
        let (kind, own) = match self.router(key) {
            Some(d) => (d.kind, d.prefix.clone()),
            None => (kind, None),
        };
        let own = own.as_ref().and_then(|p| self.prefix(key.0, p, via));
        let path = match kind {
            Kind::Django => path,
            _ => concat(own.as_deref().unwrap_or(""), &path),
        };
        let mounts = self.live_mounts(key, chain);
        if mounts.is_empty() {
            out.push((
                if kind == Kind::Django {
                    format!("/{}", path.trim_start_matches('/'))
                } else {
                    path
                },
                key.clone(),
            ));
            return;
        }
        chain.push(key.clone());
        for m in mounts {
            via.extend(&m.via);
            let prefix = m.prefix.as_deref().unwrap_or("");
            let joined = match m.kind {
                Kind::Express => express_join(prefix, &path),
                Kind::Flask => flask_rule(m.prefix.as_deref(), &path),
                Kind::FastApi | Kind::Django => concat(prefix, &path),
            };
            self.up(&m.parent, m.kind, joined, chain, via, out);
        }
        chain.pop();
    }

    /// Flask: the URL prefixes blueprint `key` is registered with (None: no
    /// prefix), following nested blueprints up to the app.
    fn flask(
        &mut self,
        key: &Key,
        chain: &mut Vec<Key>,
        via: &mut Vec<usize>,
        out: &mut Vec<(Option<String>, Key)>,
    ) {
        if out.len() >= MAX_PATHS {
            return;
        }
        let own = self.router(key).and_then(|d| d.prefix.clone());
        let own = own.as_ref().and_then(|p| self.prefix(key.0, p, via));
        let mounts = self.live_mounts(key, chain);
        if mounts.is_empty() {
            out.push((own, key.clone()));
            return;
        }
        chain.push(key.clone());
        for m in mounts {
            via.extend(&m.via);
            // A registration's url_prefix replaces the blueprint's own.
            let prefix = m.prefix.clone().or_else(|| own.clone());
            let mut parents = vec![];
            self.flask(&m.parent, chain, via, &mut parents);
            out.extend(
                parents
                    .into_iter()
                    .map(|(p, app)| (flask_join(p.as_deref(), prefix.as_deref()), app)),
            );
        }
        chain.pop();
    }
}

/// FastAPI and Django: prefixes are concatenated (an unknown part as its own
/// segment).
fn concat(prefix: &str, path: &str) -> String {
    if !prefix.is_empty() && path.starts_with(UNKNOWN) && !prefix.ends_with('/') {
        format!("{prefix}/{path}")
    } else {
        format!("{prefix}{path}")
    }
}

/// Express: a mount path joined to the path below it.
fn express_join(mount: &str, path: &str) -> String {
    let mount = mount.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{mount}{path}")
    } else {
        format!("{mount}/{path}")
    }
}

/// Flask's join of two blueprint URL prefixes.
fn flask_join(outer: Option<&str>, inner: Option<&str>) -> Option<String> {
    match (outer, inner) {
        (Some(a), Some(b)) => Some(format!(
            "{}/{}",
            a.trim_end_matches('/'),
            b.trim_start_matches('/')
        )),
        (a, b) => a.or(b).map(str::to_string),
    }
}

/// Flask's rule under a URL prefix.
fn flask_rule(prefix: Option<&str>, rule: &str) -> String {
    match prefix {
        Some(p) if rule.is_empty() => p.to_string(),
        Some(p) => format!(
            "{}/{}",
            p.trim_end_matches('/'),
            rule.trim_start_matches('/')
        ),
        None => rule.to_string(),
    }
}

/// What a stored graph's routes depend on outside their own files, for an
/// incremental rebuild (see the module docs).
pub struct StoredMounts {
    /// The graph has route nodes.
    routes: bool,
    /// Files whose include calls, imports or constants shaped a stored
    /// route's path, as absolute lexical paths.
    files: FxHashSet<PathBuf>,
}

/// [`StoredMounts`] of `graph`; `path` gives the absolute path of a stored
/// `source_file`.
pub fn stored_mounts(graph: &Value, path: impl Fn(&str) -> PathBuf) -> StoredMounts {
    let mut out = StoredMounts {
        routes: false,
        files: FxHashSet::default(),
    };
    for n in graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|n| s(n, "node_kind") == "route")
    {
        out.routes = true;
        let Some(mounts) = n.get("_route_mounts").and_then(Value::as_array) else {
            continue;
        };
        let file = path(s(n, "source_file"));
        let Some(dir) = file.parent() else {
            continue;
        };
        for m in mounts.iter().filter_map(Value::as_str) {
            out.files.insert(lexical(&dir.join(m)));
        }
    }
    out
}

impl StoredMounts {
    /// Whether re-extracting only `changed` (files that changed or were
    /// deleted) could leave a stored route at a stale path: one of them
    /// shaped a stored route's path from another file, or now mounts a
    /// router.
    pub fn moved_by(&self, changed: &[PathBuf]) -> bool {
        self.routes
            && changed
                .iter()
                .any(|p| self.files.contains(&lexical(p)) || (p.is_file() && freshly_mounts(p)))
    }
}

fn edge(src: &str, tgt: &str, relation: &str, file: &str, line: usize) -> Value {
    json!({"source": src, "target": tgt, "relation": relation, "confidence": "INFERRED",
        "confidence_score": 0.85, "source_file": file, "source_location": format!("L{line}"),
        "weight": 1.0, "_origin": "ast", "resolution": "route"})
}

/// Adds route nodes and `handles`/`http_calls` edges for `paths` (see the
/// module docs). Routes already in the graph (`context`, incremental builds)
/// can be reached by requests in `paths`.
pub fn resolve(
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
    context: &[Value],
    root: &Path,
) {
    // This run's files, read in parallel.
    let own = paths
        .iter()
        .filter(|p| language(p).is_some())
        .cloned()
        .collect::<Vec<_>>();
    let scanned = parse_cache::par_map(&own, |p| scan(p));
    // An incremental run's unchanged files: the ones that may mount a
    // router are read now, any other when an import reaches it.
    let own_files = own.iter().map(|p| lexical(p)).collect::<Vec<_>>();
    let in_run = own_files.iter().cloned().collect::<FxHashSet<_>>();
    let mut others = vec![];
    let mut sources = FxHashSet::default();
    for n in context {
        let source = s(n, "source_file");
        if source.is_empty() || !sources.insert(source) {
            continue;
        }
        let path = if Path::new(source).is_absolute() {
            PathBuf::from(source)
        } else {
            root.join(source)
        };
        let file = lexical(&path);
        if language(&path).is_some() && !in_run.contains(&file) && path.is_file() {
            others.push((path, file));
        }
    }
    let mounting = parse_cache::par_map(&others, |(p, _)| {
        may_mount(p).then(|| scan(p).map(|s| s.facts))
    });
    let mut files = own_files;
    files.extend(others.into_iter().map(|(_, f)| f));
    let mut corpus = Corpus::new(files);
    let mut routes = vec![];
    let mut requests = vec![];
    for (i, sc) in scanned.into_iter().enumerate() {
        let Some(sc) = sc else {
            corpus.set(i, None);
            continue;
        };
        routes.extend(sc.routes.into_iter().map(|r| (i, r)));
        requests.extend(sc.requests);
        corpus.set(i, Some(sc.facts));
    }
    for (k, facts) in mounting.into_iter().enumerate() {
        if let Some(facts) = facts {
            corpus.set(own.len() + k, facts);
        }
    }
    if routes.is_empty() && requests.is_empty() {
        return;
    }
    corpus.link();
    // Graph nodes by file and bare name, for handlers and callers.
    let mut by_name: FxHashMap<(String, String), Vec<(usize, String)>> = FxHashMap::default();
    let mut by_label: FxHashMap<String, Vec<String>> = FxHashMap::default();
    for n in nodes.iter().chain(context) {
        let (id, label, file) = (s(n, "id"), s(n, "label"), s(n, "source_file"));
        if id.is_empty() || file.is_empty() || s(n, "node_kind") == "route" {
            continue;
        }
        let key = source_key(file, root);
        let Some(line) = s(n, "source_location")
            .strip_prefix('L')
            .and_then(|l| l.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|l| l.parse::<usize>().ok())
        else {
            continue;
        };
        let bare = label
            .trim_start_matches('.')
            .trim_end_matches("()")
            .to_string();
        by_label
            .entry(bare.clone())
            .or_default()
            .push(id.to_string());
        by_name
            .entry((key, bare.rsplit('.').next().unwrap_or(&bare).to_string()))
            .or_default()
            .push((line, id.to_string()));
    }
    let node_for = |file: &str, name: &str, line: usize| -> Option<String> {
        let key = source_key(file, root);
        let candidates = by_name.get(&(key, name.to_string()))?;
        if line == 0 {
            return (candidates.len() == 1).then(|| candidates[0].1.clone());
        }
        let distance = candidates
            .iter()
            .map(|(l, _)| l.abs_diff(line))
            .filter(|d| *d <= 3)
            .min()?;
        let mut nearest = candidates
            .iter()
            .filter(|(l, _)| l.abs_diff(line) == distance);
        let (_, id) = nearest.next()?;
        nearest.all(|(_, other)| other == id).then(|| id.clone())
    };
    // Files each file imports (by file node), to pick among equal routes.
    let file_of_node: FxHashMap<String, String> = nodes
        .iter()
        .chain(context)
        .filter(|n| !s(n, "source_file").is_empty())
        .map(|n| {
            (
                s(n, "id").to_string(),
                source_key(s(n, "source_file"), root),
            )
        })
        .collect();
    let mut imports: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
    for e in edges.iter() {
        if matches!(s(e, "relation"), "imports" | "imports_from")
            && let (Some(a), Some(b)) = (
                file_of_node.get(s(e, "source")),
                file_of_node.get(s(e, "target")),
            )
        {
            imports.entry(a.clone()).or_default().insert(b.clone());
        }
    }
    let mut ids = nodes
        .iter()
        .map(|n| s(n, "id").to_string())
        .collect::<FxHashSet<_>>();
    let mut declared = vec![];
    // Routes already in the graph.
    for n in context.iter().filter(|n| s(n, "node_kind") == "route") {
        declared.push((
            s(n, "id").to_string(),
            source_key(s(n, "source_file"), root),
            s(n, "route_method").to_string(),
            segments(s(n, "route_path")),
        ));
    }
    let mut prepared = vec![];
    let mut owners: FxHashMap<_, FxHashSet<_>> = FxHashMap::default();
    for (f, r) in &routes {
        let key = source_key(&r.file, root);
        let (served, via) = corpus.served(*f, r);
        let dir = corpus.files[*f].parent().unwrap_or(Path::new(""));
        let mounts = via
            .iter()
            .map(|g| relative(dir, &corpus.files[*g]))
            .collect::<BTreeSet<_>>();
        for (path, app) in served {
            let application = (
                source_key(&corpus.files[app.0].to_string_lossy(), root),
                app.1.clone(),
            );
            let application_relative = (relative(dir, &corpus.files[app.0]), app.1);
            owners
                .entry((key.clone(), r.method.clone(), path.clone()))
                .or_default()
                .insert(application.clone());
            prepared.push((
                r,
                key.clone(),
                path,
                application,
                application_relative,
                mounts.clone(),
            ));
        }
    }
    for (r, key, path, application, application_relative, mounts) in prepared {
        let ambiguous = owners
            .get(&(key.clone(), r.method.clone(), path.clone()))
            .is_some_and(|apps| apps.len() > 1);
        let id = if ambiguous {
            make_id([
                "route",
                key.as_str(),
                r.method.as_str(),
                path.as_str(),
                application.0.as_str(),
                application.1.as_str(),
            ])
        } else {
            make_id(["route", key.as_str(), r.method.as_str(), path.as_str()])
        };
        if ids.insert(id.clone()) {
            let mut node = json!({"id": id, "label": format!("{} {}", r.method, path),
                    "file_type": "code", "node_kind": "route", "source_file": r.file,
                    "source_location": format!("L{}", r.line), "route_method": r.method,
                    "route_path": path, "_route_apps": [application_relative], "_origin": "ast"});
            if !mounts.is_empty() {
                node["_route_mounts"] = json!(mounts);
            }
            nodes.push(node);
            declared.push((id.clone(), key.clone(), r.method.clone(), segments(&path)));
        }
        // A path declared twice in one file keeps one node and both handlers.
        if let Some((name, line)) = &r.handler {
            let handler = node_for(&r.file, name, *line).or_else(|| {
                // Django views live in another module: a unique name.
                // A known definition line must resolve in its own file.
                if *line != 0 {
                    return None;
                }
                by_label
                    .get(name)
                    .filter(|v| v.len() == 1)
                    .map(|v| v[0].clone())
            });
            if let Some(h) = handler {
                edges.push(edge(&h, &id, "handles", &r.file, r.line));
            }
        }
    }
    // A statically bound TestClient cannot reach an unrelated application
    // merely because its URL happens to be unique in the corpus. This is a
    // scoped application constraint: mounted routes retain their terminal
    // application, including when supplied as unchanged incremental context.
    let serving_apps: FxHashMap<_, FxHashSet<_>> = nodes
        .iter()
        .chain(context)
        .filter(|n| s(n, "node_kind") == "route")
        .map(|n| {
            let file = root.join(s(n, "source_file"));
            let dir = file.parent().unwrap_or(root);
            let apps = n
                .get("_route_apps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|app| Some((app.get(0)?.as_str()?, app.get(1)?.as_str()?)))
                .map(|(p, name)| {
                    (
                        source_key(&dir.join(p).to_string_lossy(), root),
                        name.to_owned(),
                    )
                })
                .collect();
            (s(n, "id"), apps)
        })
        .collect();
    let mut seen = FxHashSet::default();
    for q in &requests {
        let Some((name, line)) = &q.caller else {
            continue;
        };
        let Some(caller) = node_for(&q.file, name, *line) else {
            continue;
        };
        let mut explicit = false;
        let application = corpus
            .index
            .get(&lexical(Path::new(&q.file)))
            .copied()
            .and_then(|f| {
                let application = q.application.clone().or_else(|| {
                    q.client.as_ref().and_then(|name| {
                        corpus.facts(f).and_then(|facts| {
                            facts.clients.get(name).map(|target| {
                                if facts.uncertain.contains(name) {
                                    Target::Unknown
                                } else {
                                    target.clone()
                                }
                            })
                        })
                    })
                })?;
                explicit = true;
                let (app, name) = corpus.router_target(f, &application, &mut vec![])?;
                Some((source_key(&corpus.files[app].to_string_lossy(), root), name))
            });
        if explicit && application.is_none() {
            continue;
        }
        let want = segments(&q.path);
        let mut hits = declared
            .iter()
            .filter(|(id, _, m, segs)| {
                let identifiable =
                    segs.is_empty() || segs.iter().any(|s| !matches!(s.as_str(), "*" | "**" | "+"));
                identifiable
                    && application.as_ref().is_none_or(|app| {
                        serving_apps
                            .get(id.as_str())
                            .is_some_and(|apps| apps.contains(app))
                    })
                    && q.method.as_deref().is_none_or(|qm| m == qm || m == "ANY")
                    && matches(segs, &want)
            })
            .collect::<Vec<_>>();
        // A route whose whole path is known beats one under an unknown prefix.
        let known = |segs: &[String]| segs.first().is_none_or(|s| s != "+");
        if hits.iter().any(|(_, _, _, segs)| known(segs.as_slice())) {
            hits.retain(|(_, _, _, segs)| known(segs.as_slice()));
        }
        let key = source_key(&q.file, root);
        let chosen = if hits.len() == 1 {
            Some(hits[0])
        } else {
            let imported = imports.get(&key);
            let local = hits
                .iter()
                .filter(|(_, f, _, _)| *f == key || imported.is_some_and(|i| i.contains(f)))
                .collect::<Vec<_>>();
            (local.len() == 1).then(|| *local[0])
        };
        if let Some((route, _, _, _)) = chosen
            && seen.insert((caller.clone(), route.clone()))
        {
            edges.push(edge(&caller, route, "http_calls", &q.file, q.line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_handlers_and_requests_across_frameworks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let write = |name: &str, body: &str| {
            let p = root.join(name);
            std::fs::write(&p, body).unwrap();
            p
        };
        let app = write(
            "app.py",
            "from fastapi import APIRouter, FastAPI\napp = FastAPI()\nrouter = APIRouter(prefix=\"/api\")\n\n\
             @app.get(\"/items/{item_id}\")\ndef read_item(item_id: int):\n    return item_id\n\n\
             @router.post(\"/users\")\nasync def create_user():\n    return {}\n",
        );
        let test = write(
            "test_app.py",
            "def test_read():\n    client.get(\"/items/3\")\n    client.post(\"/api/users\")\n    client.get(\"/nothing\")\n",
        );
        let js = write(
            "server.js",
            "const app = express();\napp.get(\"/health\", healthCheck);\nfunction healthCheck(req, res) { res.send(\"ok\"); }\nasync function ping() { await fetch(\"/health\"); }\n",
        );
        let urls = write(
            "urls.py",
            "urlpatterns = [path(\"about/\", views.about), path(\"blog/\", include(\"blog.urls\"))]\n",
        );
        let views = write("views.py", "def about(request):\n    return None\n");
        let node = |id: &str, label: &str, file: &Path, line: usize| {
            json!({"id": id, "label": label, "file_type": "code", "source_file": file.to_string_lossy(),
                "source_location": format!("L{line}")})
        };
        let mut nodes = vec![
            node("read_item", "read_item()", &app, 6),
            node("create_user", "create_user()", &app, 10),
            node("test_read", "test_read()", &test, 1),
            node("health", "healthCheck()", &js, 3),
            node("ping", "ping()", &js, 4),
            node("about", "about()", &views, 1),
        ];
        let mut edges = vec![];
        resolve(&[app, test, js, urls], &mut nodes, &mut edges, &[], &root);
        let label = |id: &str| {
            nodes
                .iter()
                .find(|n| n["id"] == id)
                .map(|n| n["label"].as_str().unwrap().to_string())
                .unwrap_or_else(|| id.to_string())
        };
        let mut got = edges
            .iter()
            .map(|e| {
                format!(
                    "{} -{}-> {}",
                    label(e["source"].as_str().unwrap()),
                    e["relation"].as_str().unwrap(),
                    label(e["target"].as_str().unwrap())
                )
            })
            .collect::<Vec<_>>();
        got.sort();
        assert_eq!(
            got,
            [
                "about() -handles-> ANY /about/",
                "create_user() -handles-> POST /api/users",
                "healthCheck() -handles-> GET /health",
                "ping() -http_calls-> GET /health",
                "read_item() -handles-> GET /items/{item_id}",
                "test_read() -http_calls-> GET /items/{item_id}",
                "test_read() -http_calls-> POST /api/users",
            ]
        );
    }

    #[test]
    fn segment_matching() {
        let r = segments("/items/{item_id}");
        assert!(matches(&r, &segments("/items/42")));
        assert!(matches(&r, &segments("http://testserver/items/42?q=1")));
        assert!(matches(&r, &segments("/items/{}")));
        assert!(!matches(&r, &segments("/items/42/extra")));
        assert!(matches(
            &segments("/files/{p:path}"),
            &segments("/files/a/b.txt")
        ));
        assert!(matches(
            &segments("/users/<int:uid>"),
            &segments("/users/7")
        ));
        assert!(matches(&segments("/users/:id"), &segments("{}/users/9")));
        // An unknown base on the route side stands for leading segments.
        let unknown = segments("{}/items/");
        assert!(matches(&unknown, &segments("/api/v1/items/")));
        assert!(matches(&unknown, &segments("{}/items/")));
        assert!(!matches(&unknown, &segments("/items/")));
        assert!(!matches(&unknown, &segments("/api/users/")));
    }

    /// Labels of the route nodes and of `handles`/`http_calls` edges.
    fn summary(nodes: &[Value], edges: &[Value]) -> Vec<String> {
        let label = |id: &str| {
            nodes
                .iter()
                .find(|n| n["id"] == id)
                .map(|n| n["label"].as_str().unwrap().to_string())
                .unwrap_or_else(|| id.to_string())
        };
        let mut got = nodes
            .iter()
            .filter(|n| n["node_kind"] == "route")
            .map(|n| format!("route {}", n["label"].as_str().unwrap()))
            .chain(edges.iter().map(|e| {
                format!(
                    "{} -{}-> {}",
                    label(e["source"].as_str().unwrap()),
                    e["relation"].as_str().unwrap(),
                    label(e["target"].as_str().unwrap())
                )
            }))
            .collect::<Vec<_>>();
        got.sort();
        got
    }

    fn corpus(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = files
            .iter()
            .map(|(name, body)| {
                let p = root.join(name);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, body).unwrap();
                p
            })
            .collect();
        (dir, root, paths)
    }

    fn function(id: &str, file: &Path, line: usize) -> Value {
        json!({"id": id, "label": format!("{id}()"), "file_type": "code",
            "source_file": file.to_string_lossy(), "source_location": format!("L{line}")})
    }

    fn reached_handlers(edges: &[Value]) -> Vec<(String, String)> {
        let mut pairs = edges
            .iter()
            .filter(|e| e["relation"] == "http_calls")
            .flat_map(|call| {
                edges
                    .iter()
                    .filter(|e| e["relation"] == "handles" && e["target"] == call["target"])
                    .map(|handler| {
                        (
                            s(call, "source").to_owned(),
                            s(handler, "source").to_owned(),
                        )
                    })
            })
            .collect::<Vec<_>>();
        pairs.sort();
        pairs
    }

    #[test]
    fn test_clients_distinguish_apps_with_the_same_file_and_path() {
        let (_dir, root, paths) = corpus(&[(
            "apps.py",
            "a = FastAPI()\nb = FastAPI()\n@a.get('/same')\ndef first(): pass\n@b.get('/same')\ndef second(): pass\n\ndef visit_a():\n    c = TestClient(a)\n    c.get('/same')\ndef visit_b():\n    with TestClient(b) as c:\n        c.get('/same')\n",
        )]);
        let mut nodes = vec![
            function("first", &paths[0], 4),
            function("second", &paths[0], 6),
            function("visit_a", &paths[0], 8),
            function("visit_b", &paths[0], 11),
        ];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        assert_eq!(
            nodes.iter().filter(|n| n["node_kind"] == "route").count(),
            2
        );
        assert_eq!(
            reached_handlers(&edges),
            [
                ("visit_a".into(), "first".into()),
                ("visit_b".into(), "second".into())
            ]
        );
    }

    #[test]
    fn imported_factories_aliases_and_incremental_context_keep_exact_mounts() {
        let (_dir, root, paths) = corpus(&[
            (
                "routes.py",
                "router = APIRouter()\n@router.get('/item')\ndef item(): pass\n",
            ),
            (
                "apps.py",
                "from routes import router\na = FastAPI()\nb = FastAPI()\na.include_router(router, prefix='/a')\nb.include_router(router, prefix='/b')\ndef make_app():\n    alias = a\n    return alias\n",
            ),
            ("exported.py", "from apps import make_app as factory\n"),
            (
                "test_app.py",
                "from exported import factory\ndef visit():\n    with TestClient(factory()) as c:\n        c.get('/a/item')\n        c.get('/b/item')\n",
            ),
        ]);
        let mut nodes = vec![
            function("item", &paths[0], 3),
            function("make_app", &paths[1], 6),
            function("visit", &paths[3], 2),
        ];
        for (i, p) in paths.iter().enumerate() {
            nodes.push(json!({"id":format!("file-{i}"),"source_file":p,"label":p.file_name().unwrap().to_string_lossy(),"source_location":"L1"}));
        }
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        let calls = edges
            .iter()
            .filter(|e| e["relation"] == "http_calls")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            nodes
                .iter()
                .find(|n| n["id"] == calls[0]["target"])
                .unwrap()["route_path"],
            "/a/item"
        );
        let context = nodes.clone();
        let mut changed = vec![function("visit", &paths[3], 2)];
        let mut changed_edges = vec![];
        resolve(
            &paths[3..],
            &mut changed,
            &mut changed_edges,
            &context,
            &root,
        );
        assert_eq!(
            changed_edges,
            calls.into_iter().cloned().collect::<Vec<_>>()
        );
    }

    #[test]
    fn unresolved_factories_and_rebound_clients_do_not_guess_an_application() {
        let (_dir, root, paths) = corpus(&[(
            "apps.py",
            "app = FastAPI()\n@app.get('/unique')\ndef handler(): pass\ndef conditional():\n    if enabled:\n        return app\n    return other\ndef recursive():\n    return recursive()\ndef rebound():\n    value = app\n    value = other\n    return value\ndef assigned_conditionally():\n    if enabled:\n        value = app\n    return value\ndef visit():\n    TestClient(conditional()).get('/unique')\n    TestClient(recursive()).get('/unique')\n    TestClient(rebound()).get('/unique')\n    TestClient(assigned_conditionally()).get('/unique')\n    TestClient(unknown()).get('/unique')\n    TestClient(conditional(1)).get('/unique')\n    client = TestClient(app)\n    client = external()\n    client.get('/unique')\n",
        )]);
        let mut nodes = vec![
            function("handler", &paths[0], 3),
            function("visit", &paths[0], 18),
        ];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        assert!(edges.iter().any(|e| e["relation"] == "handles"));
        assert!(reached_handlers(&edges).is_empty(), "{edges:?}");
    }

    #[test]
    fn coroutines_generators_and_wrapped_factories_are_not_app_instances() {
        let (_dir, root, paths) = corpus(&[(
            "apps.py",
            "app = FastAPI()\n@app.get('/unique')\ndef handler(): pass\nasync def coroutine():\n    return app\ndef generator():\n    yield app\n    return app\n@unknown_wrapper\ndef decorated():\n    return app\ndef visit():\n    TestClient(coroutine()).get('/unique')\n    TestClient(generator()).get('/unique')\n    TestClient(decorated()).get('/unique')\n",
        )]);
        let mut nodes = vec![
            function("handler", &paths[0], 3),
            function("visit", &paths[0], 12),
        ];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        assert!(reached_handlers(&edges).is_empty(), "{edges:?}");
    }

    #[test]
    fn l1_catch_all_routes_do_not_claim_unrelated_requests() {
        let (_dir, root, paths) = corpus(&[
            (
                "app.py",
                "app = FastAPI()\n@app.get(\"/{value}\")\ndef catch_all(value): return value\n\n\
                 @app.get(\"/things/{item_id}\")\ndef thing(item_id): return item_id\n\n\
                 def local():\n    client.get(\"/openapi.json\")\n",
            ),
            (
                "unrelated.py",
                "def visit():\n    client.get(\"/openapi.json\")\n    client.get(\"/anything\")\n    client.get(\"/things/42\")\n",
            ),
        ]);
        let mut nodes = vec![
            function("catch_all", &paths[0], 3),
            function("thing", &paths[0], 6),
            function("local", &paths[0], 8),
            function("visit", &paths[1], 1),
        ];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        let got = summary(&nodes, &edges);
        assert!(got.contains(&"catch_all() -handles-> GET /{value}".into()));
        assert_eq!(
            got.iter()
                .filter(|s| s.contains(" -http_calls-> "))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["visit() -http_calls-> GET /things/{item_id}"],
            "An all-wildcard route does not identify the application a request reaches: {got:?}"
        );
    }

    #[test]
    fn l1_function_local_routers_keep_their_own_mounts_and_handlers() {
        let (_dir, root, paths) = corpus(&[(
            "apps.py",
            "from fastapi import FastAPI, APIRouter\n\n\
             def build_one():\n    app = FastAPI()\n    router = APIRouter()\n    @router.get(\"/first\")\n    def first(): return 1\n    app.include_router(router, prefix=\"/one\")\n    return app\n\n\
             def build_two():\n    app = FastAPI()\n    router = APIRouter()\n    @router.get(\"/second\")\n    def second(): return 2\n    app.include_router(router, prefix=\"/two\")\n    return app\n",
        )]);
        let mut nodes = vec![
            function("build_one.first", &paths[0], 7),
            function("build_two.second", &paths[0], 15),
        ];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        assert_eq!(
            summary(&nodes, &edges),
            [
                "build_one.first() -handles-> GET /one/first",
                "build_two.second() -handles-> GET /two/second",
                "route GET /one/first",
                "route GET /two/second",
            ]
        );
    }

    #[test]
    fn local_shadowing_closures_and_explicit_globals_choose_the_right_router() {
        let (_dir, root, paths) = corpus(&[(
            "apps.py",
            r#"
router = APIRouter(prefix="/module")
BASE = "/module-prefix"
@router.get("/root")
def root(): pass

def parameter(router: APIRouter):
    app = FastAPI()
    app.include_router(router, prefix="/wrong-parameter")

def local():
    app = FastAPI()
    app.include_router(router, prefix="/wrong-local")
    router = unknown_router()

def factory(BASE):
    router = APIRouter(prefix="/inner")
    @router.get("/item")
    def item(): pass
    def mount():
        nonlocal router
        app = FastAPI()
        app.include_router(router, prefix="/outer")
        app.include_router(router, prefix=BASE)
    mount()

def global_mount():
    global router
    app = FastAPI()
    app.include_router(router, prefix="/global")
"#,
        )]);
        let mut nodes = vec![];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        assert_eq!(
            summary(&nodes, &edges),
            [
                "route GET /global/module/root",
                "route GET /outer/inner/item",
                "route GET {}/inner/item",
            ]
        );
    }

    #[test]
    fn factory_local_imports_do_not_replace_each_other() {
        let (_dir, root, paths) = corpus(&[
            (
                "a.py",
                "router = APIRouter()\n@router.get(\"/a\")\ndef a(): pass\n",
            ),
            (
                "b.py",
                "router = APIRouter()\n@router.get(\"/b\")\ndef b(): pass\n",
            ),
            (
                "apps.py",
                "def first():\n    from a import router\n    app = FastAPI()\n    app.include_router(router, prefix=\"/one\")\n\ndef second():\n    from b import router\n    app = FastAPI()\n    app.include_router(router, prefix=\"/two\")\n",
            ),
        ]);
        let mut nodes = vec![];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        assert_eq!(
            summary(&nodes, &edges),
            ["route GET /one/a", "route GET /two/b"]
        );
    }

    #[test]
    fn router_constructor_prefixes_resolve_constants_and_keep_unknown_values() {
        let (_dir, root, paths) = corpus(&[
            ("config.py", "BASE = \"/api\"\n"),
            (
                "app.py",
                "from config import BASE\nrouter = APIRouter(prefix=BASE)\n@router.get(\"/things\")\ndef things(): pass\n\ndef factory(prefix):\n    router = APIRouter(prefix=prefix)\n    @router.get(\"/unknown\")\n    def unknown(): pass\n",
            ),
            (
                "app.js",
                "const BASE = '/js';\nconst router = new Router({prefix: BASE});\nrouter.get('/things', handler);\n",
            ),
        ]);
        let mut nodes = vec![];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        assert_eq!(
            summary(&nodes, &edges),
            [
                "route GET /api/things",
                "route GET /js/things",
                "route GET {}/unknown"
            ]
        );
        let route = nodes
            .iter()
            .find(|n| n["label"] == "GET /api/things")
            .unwrap();
        assert_eq!(route["_route_mounts"], json!(["config.py"]));
    }

    #[test]
    fn test_clients_only_reach_routes_declared_or_mounted_by_their_app_file() {
        let (_dir, root, paths) = corpus(&[
            (
                "app.py",
                "from actual import router\napp = FastAPI()\napp.include_router(router, prefix=\"/api\")\n",
            ),
            (
                "actual.py",
                "router = APIRouter()\n@router.get(\"/thing\")\ndef thing(): pass\n",
            ),
            (
                "foreign.py",
                "app = FastAPI()\n@app.get(\"/admin\")\ndef admin(): pass\n@app.get(\"/hello/\")\ndef hello(): pass\n",
            ),
            (
                "test_app.py",
                "from app import app\nclient = TestClient(app)\ndef visit(path):\n    client.get(\"/admin\")\n    client.get(f\"{path}/hello\")\n    TestClient(app).get(\"/admin\")\n    client.get(\"/api/thing\")\n",
            ),
        ]);
        let mut nodes = vec![
            function("visit", &paths[3], 3),
            function("thing", &paths[1], 3),
        ];
        // Real extraction retains a file node even when a module has no
        // functions of its own. It keeps imported apps in incremental context.
        nodes.push(json!({"id":"app-file", "label":"app.py", "source_file":paths[0], "source_location":"L1"}));
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        let calls = |nodes: &[Value], edges: &[Value]| {
            summary(nodes, edges)
                .into_iter()
                .filter(|s| s.contains(" -http_calls-> "))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            calls(&nodes, &edges),
            ["visit() -http_calls-> GET /api/thing"]
        );
        let context = nodes.clone();
        let mut changed = vec![function("visit", &paths[3], 3)];
        let mut changed_edges = vec![];
        resolve(
            &paths[3..],
            &mut changed,
            &mut changed_edges,
            &context,
            &root,
        );
        assert_eq!(
            calls(&context, &changed_edges),
            ["visit() -http_calls-> GET /api/thing"]
        );
    }

    #[test]
    fn express_factories_blocks_and_var_hoisting_keep_distinct_routers() {
        let (_dir, root, paths) = corpus(&[(
            "apps.ts",
            r#"
function first() {
    var app = express();
    const router = express.Router();
    router.get('/first', firstHandler);
    if (enabled) {
        var parent = express();
        const router = express.Router();
        router.get('/block', blockHandler);
        parent.use('/nested', router);
    }
    parent.use('/one', router);
    app.use('/api', parent);
}
function second() {
    const app = express();
    const router = express.Router();
    router.get('/second', secondHandler);
    app.use('/two', router);
}
"#,
        )]);
        let mut nodes = vec![];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        assert_eq!(
            summary(&nodes, &edges),
            [
                "route GET /api/nested/block",
                "route GET /api/one/first",
                "route GET /two/second"
            ]
        );
    }

    #[test]
    fn fastapi_prefixes_from_other_files() {
        let (_dir, root, paths) = corpus(&[
            (
                "app/main.py",
                "from fastapi import FastAPI\nfrom .routers import items, users\nfrom .internal import admin\n\
                 from .config import API\nfrom .routers.v1 import router as v1\nfrom . import settings\n\
                 app = FastAPI()\napp.include_router(users.router)\napp.include_router(items.router)\n\
                 app.include_router(admin.router, prefix=\"/admin\")\n\
                 app.include_router(v1, prefix=API + \"/v1\")\napp.include_router(v1, prefix=API)\n\
                 app.include_router(users.legacy, prefix=settings.LEGACY)\napp.mount(\"/sub\", sub)\n\
                 sub = FastAPI()\n@sub.get(\"/ping\")\ndef ping():\n    return 1\n",
            ),
            ("app/__init__.py", ""),
            ("app/config.py", "API = \"/api\"\n"),
            ("app/routers/__init__.py", ""),
            (
                "app/routers/items.py",
                "router = APIRouter(prefix=\"/items\")\n\n@router.get(\"/\")\ndef read_items():\n    return []\n",
            ),
            (
                "app/routers/users.py",
                "router = APIRouter()\nlegacy = APIRouter()\n\n@router.get(\"/users/{uid}\")\ndef read_user(uid):\n    return uid\n\n\
                 @legacy.get(\"/old\")\ndef old():\n    return 0\n",
            ),
            (
                "app/routers/v1.py",
                "router = APIRouter()\n\n@router.get(\"/things\")\ndef things():\n    return []\n",
            ),
            ("app/internal/__init__.py", ""),
            (
                "app/internal/admin.py",
                "router = APIRouter()\n\n@router.post(\"/\")\ndef update_admin():\n    return {}\n",
            ),
            (
                "tests/test_api.py",
                "def test_all():\n    client.post(\"/admin/\")\n    client.get(\"/api/things\")\n    client.get(f\"{base}/old\")\n",
            ),
        ]);
        let mut nodes = vec![
            function("read_items", &paths[4], 4),
            function("read_user", &paths[5], 5),
            function("old", &paths[5], 9),
            function("things", &paths[6], 4),
            function("update_admin", &paths[8], 4),
            function("ping", &paths[0], 17),
            function("test_all", &paths[9], 1),
        ];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        let got = summary(&nodes, &edges);
        for want in [
            "route GET /items/",
            "route GET /users/{uid}",
            "route POST /admin/",
            "route GET /api/things",
            // `API + "/v1"` is no literal: an unknown prefix
            "route GET {}/things",
            "route GET {}/old",
            "route GET /sub/ping",
            "update_admin() -handles-> POST /admin/",
            "things() -handles-> GET /api/things",
            "things() -handles-> GET {}/things",
            "test_all() -http_calls-> POST /admin/",
            "test_all() -http_calls-> GET /api/things",
            "test_all() -http_calls-> GET {}/old",
        ] {
            assert!(got.contains(&want.to_string()), "{want} not in {got:#?}");
        }
        // Mounted routers keep no unprefixed copy.
        for gone in ["route POST /", "route GET /things", "route GET /ping"] {
            assert!(!got.contains(&gone.to_string()), "{gone} in {got:#?}");
        }
        // `/api/things` also matches `{}/things`; the known path wins.
        assert_eq!(
            got.iter().filter(|g| g.contains("http_calls")).count(),
            3,
            "{got:#?}"
        );
        let admin = nodes.iter().find(|n| n["label"] == "POST /admin/").unwrap();
        assert_eq!(admin["_route_mounts"], json!(["../main.py"]));
        let things = nodes
            .iter()
            .find(|n| n["label"] == "GET /api/things")
            .unwrap();
        assert_eq!(
            things["_route_mounts"],
            json!(["../config.py", "../main.py"])
        );
        let items = nodes.iter().find(|n| n["label"] == "GET /items/").unwrap();
        assert_eq!(items["_route_mounts"], json!(["../main.py"]));
    }

    #[test]
    fn flask_blueprints_registered_elsewhere() {
        let (_dir, root, paths) = corpus(&[
            (
                "app/__init__.py",
                "from flask import Flask\n\ndef create_app():\n    app = Flask(__name__)\n\
                 \x20   from . import auth\n    app.register_blueprint(auth.bp)\n\
                 \x20   from .blog import bp as blog_bp\n    app.register_blueprint(blog_bp, url_prefix=\"/blog/\")\n\
                 \x20   from .main import bp as main_bp\n    app.register_blueprint(main_bp)\n\
                 \x20   from .api import parent\n    app.register_blueprint(parent)\n    return app\n",
            ),
            (
                "app/auth.py",
                "bp = Blueprint(\"auth\", __name__, url_prefix=\"/auth\")\n\n@bp.route(\"/login\", methods=[\"GET\", \"POST\"])\ndef login():\n    pass\n",
            ),
            (
                "app/blog.py",
                "bp = Blueprint(\"blog\", __name__, url_prefix=\"/ignored\")\n\n@bp.route(\"/\")\ndef index():\n    pass\n\n\
                 @bp.route(\"/post\")\ndef post():\n    pass\n",
            ),
            (
                "app/main/__init__.py",
                "from flask import Blueprint\nbp = Blueprint(\"main\", __name__, url_prefix=\"/m/\")\nfrom . import views\n",
            ),
            (
                "app/main/views.py",
                "from . import bp\n\n@bp.route(\"/home\")\ndef home():\n    pass\n",
            ),
            (
                "app/api.py",
                "parent = Blueprint(\"parent\", __name__, url_prefix=\"/parent\")\nchild = Blueprint(\"child\", __name__)\n\
                 parent.register_blueprint(child, url_prefix=\"/child\")\n\n@child.route(\"/x\")\ndef x():\n    pass\n",
            ),
        ]);
        let mut nodes = vec![];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        let got = summary(&nodes, &edges);
        assert_eq!(
            got,
            [
                "route GET /auth/login",
                "route GET /blog/",
                "route GET /blog/post",
                "route GET /m/home",
                "route GET /parent/child/x",
                "route POST /auth/login",
            ],
        );
    }

    #[test]
    fn express_routers_and_django_includes() {
        let (_dir, root, paths) = corpus(&[
            (
                "server/app.js",
                "const express = require('express');\nconst app = express();\nconst users = require('./routes/users');\n\
                 import api from './routes/api.js';\nconst { admin } = require('./routes/admin');\nconst BASE = '/v2';\n\
                 app.use('/users', users);\napp.use('/api/', api);\napp.use(BASE, admin);\napp.use(express.json());\n\
                 async function load() { await fetch('/users/7'); }\n",
            ),
            (
                "server/routes/users.js",
                "const router = express.Router();\nrouter.get('/', list);\nrouter.get('/:id', show);\nmodule.exports = router;\n",
            ),
            (
                "server/routes/api.ts",
                "const r = Router();\nr.post('/jobs', run);\nexport default r;\n",
            ),
            (
                "server/routes/admin.js",
                "const admin = express.Router();\nadmin.delete('/cache', clear);\nmodule.exports = { admin };\n",
            ),
            (
                "mysite/urls.py",
                "urlpatterns = [path(\"blog/\", include(\"blog.urls\")), path(\"\", views.home)]\n",
            ),
            (
                "blog/urls.py",
                "urlpatterns = [path(\"post/<int:pk>/\", views.post), re_path(r\"^about/$\", views.about)]\n",
            ),
        ]);
        let mut nodes = vec![function("load", &paths[0], 11)];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        let got = summary(&nodes, &edges);
        assert_eq!(
            got,
            [
                "load() -http_calls-> GET /users/:id",
                "route ANY /",
                "route ANY /blog/about/",
                "route ANY /blog/post/<int:pk>/",
                "route DELETE /v2/cache",
                "route GET /users/",
                "route GET /users/:id",
                "route POST /api/jobs",
            ],
        );
    }

    #[test]
    fn mount_cycles_end_and_incremental_runs_read_unchanged_files() {
        let _serial = parse_cache::test_serial();
        let (_dir, root, paths) = corpus(&[
            (
                "a.py",
                "from b import rb\nra = APIRouter(prefix=\"/a\")\nra.include_router(rb)\n\n@ra.get(\"/x\")\ndef x():\n    pass\n",
            ),
            (
                "b.py",
                "from a import ra\nrb = APIRouter(prefix=\"/b\")\nrb.include_router(ra)\n\n@rb.get(\"/y\")\ndef y():\n    pass\n",
            ),
            (
                "main.py",
                "from pkg.routes import router\napp = FastAPI()\napp.include_router(router, prefix=\"/v1\")\n",
            ),
            (
                "pkg/routes.py",
                "router = APIRouter()\n\n@router.get(\"/items\")\ndef items():\n    pass\n",
            ),
        ]);
        let mut nodes = vec![];
        let mut edges = vec![];
        resolve(&paths, &mut nodes, &mut edges, &[], &root);
        let got = summary(&nodes, &edges);
        // `x` is declared on `ra`, which `rb` mounts (and the other way round).
        assert_eq!(
            got,
            [
                "route GET /a/b/y",
                "route GET /b/a/x",
                "route GET /v1/items"
            ]
        );
        // Only the router file changed: main.py comes as context.
        let context = vec![
            json!({"id": "main", "label": "main.py", "file_type": "code",
            "source_file": "main.py", "source_location": "L1"}),
        ];
        let mut nodes = vec![];
        let mut edges = vec![];
        resolve(&paths[3..], &mut nodes, &mut edges, &context, &root);
        assert_eq!(summary(&nodes, &edges), ["route GET /v1/items"]);
        // A stored graph: main.py shaped the route; pkg/routes.py did not.
        let graph = json!({"nodes": nodes});
        let mounts = stored_mounts(&graph, |s| root.join(s));
        assert!(mounts.moved_by(&[root.join("main.py")]));
        assert!(!mounts.moved_by(&[root.join("pkg/routes.py")]));
        // A file that now includes a router moves routes; one that does not, not.
        // Keep an older cached version alive as another extraction run may do.
        let _run = parse_cache::RunScope::begin();
        std::fs::write(root.join("pkg/extra.py"), "x = 1\n").unwrap();
        assert!(!mounts.moved_by(&[root.join("pkg/extra.py")]));
        assert_eq!(
            &*parse_cache::read(&root.join("pkg/extra.py")).unwrap(),
            b"x = 1\n"
        );
        std::fs::write(
            root.join("pkg/extra.py"),
            "from pkg.routes import router\nother.include_router(router)\n",
        )
        .unwrap();
        assert!(mounts.moved_by(&[root.join("pkg/extra.py")]));
        std::fs::write(
            root.join("pkg/extra.py"),
            "from pkg.routes import router\nother.mount (\"/v2\", router)\n",
        )
        .unwrap();
        assert!(mounts.moved_by(&[root.join("pkg/extra.py")]));
        std::fs::write(
            root.join("pkg/extra.js"),
            "app.use /* mounted router */ (\"/v3\", router);\n",
        )
        .unwrap();
        assert!(mounts.moved_by(&[root.join("pkg/extra.js")]));
        // A deleted mount file moves routes too; a graph without routes never.
        std::fs::remove_file(root.join("main.py")).unwrap();
        assert!(mounts.moved_by(&[root.join("main.py")]));
        assert!(
            !stored_mounts(&json!({"nodes": []}), |s| root.join(s))
                .moved_by(&[root.join("pkg/extra.py")])
        );
    }
}
