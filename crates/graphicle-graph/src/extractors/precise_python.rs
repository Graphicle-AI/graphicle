//! Opt-in precise Python call resolution (`--precise`), beyond the upstream port's.
//!
//! A flow- and context-insensitive points-to analysis over function, class,
//! instance and module values, in the spirit of PyCG (Salis et al., ICSE
//! 2021), limited to the definitions the graph already has nodes for:
//!
//! * names resolve lexically (locals, enclosing functions, module globals and
//!   imports bound by the graph's own import edges), never by label guesses;
//! * values flow through assignments, parameters (positional, keyword and
//!   defaults), returns (a returned parameter passes that call site's argument
//!   through), instance and class attributes (`self.x = ...`), containers
//!   (conflated with their elements), loops and decorators;
//! * `obj.m` binds through the class hierarchy of the instances `obj` may
//!   hold, and `super().m` through the enclosing class's bases.
//!
//! Each call site inside a graph function or method gets a `calls` edge to
//! every callee found for it, unless the default engine already has that pair
//! or the site resolves to more than [`MAX_TARGETS`] callees (too ambiguous to
//! help). At a `super().m` site the default engine's edge to a same-named
//! method elsewhere is replaced. Added edges carry `resolution: points_to` or
//! `super_call`. Module-level statements feed the analysis but add no edges,
//! because Graphify has no module-level caller node. The default extraction
//! never runs this module.
use super::{parse_cache, resolution::source_key, resolution_facts};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tree_sitter::Node;

/// Call sites that resolve to more callees than this add no edges.
pub const MAX_TARGETS: usize = 8;
/// Values kept per variable; a larger set stops growing, which bounds the work
/// on deep hierarchies without affecting sites small enough to emit.
const MAX_VALUES: usize = 64;
/// Passes over the constraints before settling for the current solution.
const MAX_PASSES: usize = 50;
/// Syntax nesting beyond this is skipped (bounds recursion on generated code).
const MAX_DEPTH: usize = 200;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn bare(label: &str) -> &str {
    let label = label.strip_prefix('.').unwrap_or(label);
    label.strip_suffix("()").unwrap_or(label)
}
fn text<'a>(n: Node<'_>, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[n.byte_range()]).unwrap_or("")
}
fn named(n: Node<'_>) -> Vec<Node<'_>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}
fn line(n: Node<'_>) -> usize {
    n.start_position().row + 1
}
fn location(v: &Value) -> Option<usize> {
    s(v, "source_location").strip_prefix('L')?.parse().ok()
}
fn is_super(n: Node<'_>, source: &[u8]) -> bool {
    n.kind() == "call"
        && n.child_by_field_name("function")
            .is_some_and(|f| text(f, source) == "super")
}
/// The name a module file answers to (`pkg/mod.py` -> `mod`, `pkg/__init__.py` -> `pkg`).
fn module_stem(key: &str) -> &str {
    let path = Path::new(key);
    let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    if file == "__init__.py" {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|f| f.to_str())
            .unwrap_or("");
    }
    file.strip_suffix(".py").unwrap_or(file)
}
/// Whether file `key` is the module `dotted` (`a.b` -> `…a/b.py` or `…a/b/__init__.py`).
fn is_module(key: &str, dotted: &str) -> bool {
    let rel = dotted.replace('.', "/");
    [format!("{rel}.py"), format!("{rel}/__init__.py")]
        .iter()
        .any(|p| key == p || key.ends_with(&format!("/{p}")))
}

/// What the graph already says about definitions, files, classes and imports.
#[derive(Default)]
struct Facts {
    /// (file key, definition line) -> node ids starting there.
    at: FxHashMap<(String, usize), Vec<String>>,
    label: FxHashMap<String, String>,
    /// file key -> file node id, and back.
    file_node: FxHashMap<String, String>,
    file_key: FxHashMap<String, String>,
    classes: FxHashSet<String>,
    /// (class id, method name) -> method id.
    methods: FxHashMap<(String, String), String>,
    bases: FxHashMap<String, Vec<String>>,
    /// (file key, line) -> targets of the import edges drawn from that line.
    imports: FxHashMap<(String, usize), Vec<String>>,
}
impl Facts {
    fn build(nodes: &[&Value], edges: &[&Value], root: &Path) -> Self {
        let mut f = Facts::default();
        for n in nodes {
            let (id, sf, label) = (s(n, "id"), s(n, "source_file"), s(n, "label"));
            if id.is_empty() || !sf.ends_with(".py") {
                continue;
            }
            let key = source_key(sf, root);
            f.label.insert(id.into(), label.into());
            if Path::new(sf).file_name() == Path::new(label).file_name() {
                f.file_node.entry(key.clone()).or_insert_with(|| id.into());
                f.file_key.entry(id.into()).or_insert(key);
            } else if let Some(l) = location(n) {
                f.at.entry((key, l)).or_default().push(id.into());
            }
        }
        for e in edges {
            let (src, tgt) = (s(e, "source"), s(e, "target"));
            match s(e, "relation") {
                "method" => {
                    f.classes.insert(src.into());
                    if let Some(label) = f.label.get(tgt) {
                        f.methods
                            .entry((src.into(), bare(label).into()))
                            .or_insert_with(|| tgt.into());
                    }
                }
                "inherits" | "extends" => {
                    f.classes.insert(src.into());
                    f.classes.insert(tgt.into());
                    let bases = f.bases.entry(src.into()).or_default();
                    if !bases.iter().any(|b| b == tgt) {
                        bases.push(tgt.into());
                    }
                }
                "imports" | "imports_from" => {
                    if let (Some(key), Some(l)) = (f.file_key.get(src), location(e)) {
                        f.imports
                            .entry((key.clone(), l))
                            .or_default()
                            .push(tgt.into());
                    }
                }
                _ => {}
            }
        }
        f
    }
}

// ---------------------------------------------------------------------------
// Per-file intermediate form (built in parallel, names still as strings).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Module,
    Function,
    Class,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FnKind {
    Plain,
    Static,
    ClassMethod,
}
/// Where a name's values live.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Slot {
    /// (file-local scope index, name) inside a function.
    Local(usize, String),
    /// (class id, attribute): class-body names and `obj.attr = ...` stores.
    Field(String, String),
    /// Return values of a function id.
    Ret(String),
    /// (file key, name) at module level.
    Module(String, String),
}
#[derive(Clone, Debug)]
enum Ex {
    Slot(Slot),
    /// An imported graph symbol (function or class, decided once all files are known).
    Sym(String),
    Func(String),
    Class(String),
    Inst(String),
    Module(String),
    Attr(Box<Ex>, String),
    /// `super()` in a method of the class, with the receiver's slot.
    Super(String, Slot),
    /// The result of the file-local call site.
    Call(usize),
    Many(Vec<Ex>),
    /// The first operand's values, or the second's when it has none.
    OrElse(Box<Ex>, Box<Ex>),
}
struct SiteB {
    caller: Option<String>,
    line: usize,
    func: Ex,
    args: Vec<Ex>,
    kwargs: Vec<(String, Ex)>,
    super_name: Option<String>,
}
enum ConB {
    Flow(Slot, Ex),
    Store(Ex, String, Ex),
    Iter {
        caller: Option<String>,
        line: usize,
        iter: Ex,
        targets: Vec<Slot>,
    },
}
struct FuncB {
    id: String,
    scope: usize,
    params: Vec<String>,
    /// Parameters a positional argument can bind (before `*`, `*args`, `**kw`).
    positional: usize,
    /// Parameters returned unchanged (`return p`).
    pass: Vec<usize>,
    kind: FnKind,
}
#[derive(Default)]
struct FileIr {
    path: String,
    key: String,
    scopes: usize,
    sites: Vec<SiteB>,
    cons: Vec<ConB>,
    funcs: Vec<FuncB>,
    classes: Vec<String>,
    /// Modules this file re-exports with `from m import *` (file keys).
    stars: Vec<String>,
    /// Names the module body binds (defs, classes, assignments, imports).
    module_names: Vec<String>,
}
struct ScopeB {
    kind: Kind,
    parent: Option<usize>,
    locals: FxHashSet<String>,
    /// Names bound other than as parameters.
    assigned: FxHashSet<String>,
    /// Names declared `global` / `nonlocal`.
    outer: FxHashSet<String>,
    /// `global` bypasses all enclosing function bindings.
    globals: FxHashSet<String>,
    def: Option<String>,
    /// The class of a class body, or of a method.
    class: Option<String>,
    receiver: Option<String>,
    func: Option<usize>,
    defaults: Vec<(usize, Ex)>,
}
impl ScopeB {
    fn new(kind: Kind, parent: Option<usize>) -> Self {
        ScopeB {
            kind,
            parent,
            locals: FxHashSet::default(),
            assigned: FxHashSet::default(),
            outer: FxHashSet::default(),
            globals: FxHashSet::default(),
            def: None,
            class: None,
            receiver: None,
            func: None,
            defaults: vec![],
        }
    }
}

struct Builder<'a> {
    src: &'a [u8],
    key: String,
    facts: &'a Facts,
    scopes: Vec<ScopeB>,
    ir: FileIr,
    depth: usize,
}
impl<'a> Builder<'a> {
    fn slot(&self, scope: usize, name: &str) -> Slot {
        let sc = &self.scopes[scope];
        match (sc.kind, &sc.class) {
            (Kind::Module, _) => Slot::Module(self.key.clone(), name.into()),
            (Kind::Class, Some(c)) => Slot::Field(c.clone(), name.into()),
            _ => Slot::Local(scope, name.into()),
        }
    }
    /// Lexical lookup; class bodies are visible only to their own statements.
    fn lookup(&self, scope: usize, name: &str) -> Option<Slot> {
        let (mut cur, mut first) = (Some(scope), true);
        while let Some(i) = cur {
            let sc = &self.scopes[i];
            if sc.globals.contains(name) {
                return Some(Slot::Module(self.key.clone(), name.into()));
            }
            if (first || sc.kind != Kind::Class)
                && sc.locals.contains(name)
                && !sc.outer.contains(name)
            {
                return Some(self.slot(i, name));
            }
            first = false;
            cur = sc.parent;
        }
        None
    }
    /// The graph function whose body runs this scope's statements.
    fn caller(&self, scope: usize) -> Option<String> {
        let mut cur = Some(scope);
        while let Some(i) = cur {
            let sc = &self.scopes[i];
            match sc.kind {
                Kind::Function if sc.def.is_some() => return sc.def.clone(),
                Kind::Module => return None,
                _ => cur = sc.parent,
            }
        }
        None
    }
    fn nearest(&self, scope: usize, method: bool) -> Option<usize> {
        let mut cur = Some(scope);
        while let Some(i) = cur {
            let sc = &self.scopes[i];
            if sc.kind == Kind::Function && (!method || sc.class.is_some()) {
                return Some(i);
            }
            if sc.kind == Kind::Module {
                return None;
            }
            cur = sc.parent;
        }
        None
    }
    fn def_id(&self, l: usize, name: &str, function: bool) -> Option<String> {
        self.facts
            .at
            .get(&(self.key.clone(), l))?
            .iter()
            .find(|id| {
                let label = self.facts.label.get(*id).map_or("", String::as_str);
                if function {
                    label.ends_with(')') && bare(label) == name
                } else {
                    label == name
                }
            })
            .cloned()
    }
    fn push(&mut self, c: ConB) {
        self.ir.cons.push(c);
    }

    fn targets(&self, n: Node<'_>, out: &mut Vec<String>) {
        match n.kind() {
            "identifier" => out.push(text(n, self.src).into()),
            "pattern_list"
            | "tuple_pattern"
            | "list_pattern"
            | "tuple"
            | "list"
            | "list_splat_pattern"
            | "parenthesized_expression"
            | "as_pattern_target" => {
                for c in named(n) {
                    self.targets(c, out);
                }
            }
            _ => {}
        }
    }
    /// Names bound in a scope's body, without entering nested scopes.
    fn collect(
        &self,
        body: Node<'_>,
        assigned: &mut FxHashSet<String>,
        outer: &mut FxHashSet<String>,
        globals: &mut FxHashSet<String>,
    ) {
        // Iterative: generated code can nest expressions thousands deep.
        let mut stack = vec![body];
        while let Some(n) = stack.pop() {
            let mut names = vec![];
            match n.kind() {
                "function_definition" | "class_definition" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        assigned.insert(text(name, self.src).into());
                    }
                    continue;
                }
                "lambda" => continue,
                "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                    if let Some(l) = n.child_by_field_name("left") {
                        self.targets(l, &mut names);
                    }
                }
                "named_expression" => {
                    if let Some(l) = n.child_by_field_name("name") {
                        self.targets(l, &mut names);
                    }
                }
                "as_pattern" => {
                    if let Some(a) = n.child_by_field_name("alias") {
                        self.targets(a, &mut names);
                    }
                }
                "import_statement" | "import_from_statement" => {
                    names.extend(self.import_names(n).into_iter().map(|(_, local)| local));
                }
                "global_statement" | "nonlocal_statement" => {
                    for c in named(n).into_iter().filter(|c| c.kind() == "identifier") {
                        outer.insert(text(c, self.src).into());
                        if n.kind() == "global_statement" {
                            globals.insert(text(c, self.src).into());
                        }
                    }
                }
                _ => {}
            }
            assigned.extend(names);
            stack.extend(named(n));
        }
    }
    /// (imported dotted name, local binding) pairs of an import statement.
    fn import_names(&self, n: Node<'_>) -> Vec<(String, String)> {
        let module = n.child_by_field_name("module_name").map(|m| m.id());
        let mut out = vec![];
        for c in named(n) {
            if Some(c.id()) == module {
                continue;
            }
            match c.kind() {
                "dotted_name" => {
                    let full = text(c, self.src).to_string();
                    let local = full.split('.').next().unwrap_or("").to_string();
                    out.push((full, local));
                }
                "aliased_import" => {
                    if let (Some(name), Some(alias)) = (
                        c.child_by_field_name("name"),
                        c.child_by_field_name("alias"),
                    ) {
                        out.push((text(name, self.src).into(), text(alias, self.src).into()));
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn visit(&mut self, n: Node<'_>, scope: usize) {
        match n.kind() {
            "function_definition" => self.function(n, scope, &[]),
            "class_definition" => self.class(n, scope, &[]),
            "decorated_definition" => {
                let decorators = named(n)
                    .into_iter()
                    .filter(|d| d.kind() == "decorator")
                    .collect::<Vec<_>>();
                match n.child_by_field_name("definition") {
                    Some(d) if d.kind() == "function_definition" => {
                        self.function(d, scope, &decorators)
                    }
                    Some(d) if d.kind() == "class_definition" => self.class(d, scope, &decorators),
                    _ => {}
                }
            }
            "assignment" | "augmented_assignment" => {
                match (
                    n.child_by_field_name("left"),
                    n.child_by_field_name("right"),
                ) {
                    (Some(l), Some(r)) => self.assign(l, r, scope),
                    _ => {
                        for c in named(n) {
                            self.expr(c, scope);
                        }
                    }
                }
            }
            "return_statement" => self.ret(n, scope),
            "for_statement" => {
                let (left, right) = (
                    n.child_by_field_name("left"),
                    n.child_by_field_name("right"),
                );
                if let Some(it) = right.and_then(|r| self.expr(r, scope)) {
                    self.iterate(left, it, line(n), scope);
                }
                let skip = [left.map(|l| l.id()), right.map(|r| r.id())];
                for c in named(n) {
                    if !skip.contains(&Some(c.id())) {
                        self.visit(c, scope);
                    }
                }
            }
            "import_statement" | "import_from_statement" => self.import(n, scope),
            "lambda" | "global_statement" | "nonlocal_statement" | "comment" => {}
            _ => {
                self.expr(n, scope);
            }
        }
    }
    fn iterate(&mut self, left: Option<Node<'_>>, iter: Ex, l: usize, scope: usize) {
        let Some(left) = left else { return };
        let mut names = vec![];
        self.targets(left, &mut names);
        let targets = names.iter().filter_map(|x| self.lookup(scope, x)).collect();
        let caller = self.caller(scope);
        self.push(ConB::Iter {
            caller,
            line: l,
            iter,
            targets,
        });
    }
    fn assign(&mut self, left: Node<'_>, right: Node<'_>, scope: usize) {
        let seq = |k: &str| {
            matches!(
                k,
                "pattern_list"
                    | "tuple_pattern"
                    | "list_pattern"
                    | "tuple"
                    | "list"
                    | "expression_list"
            )
        };
        if seq(left.kind()) && seq(right.kind()) {
            let (l, r) = (named(left), named(right));
            if l.len() == r.len() {
                for (a, b) in l.into_iter().zip(r) {
                    self.assign(a, b, scope);
                }
                return;
            }
        }
        // `a = b = value` nests assignments on the right.
        let (mut lefts, mut value) = (vec![left], right);
        while value.kind() == "assignment" {
            lefts.extend(value.child_by_field_name("left"));
            match value.child_by_field_name("right") {
                Some(r) => value = r,
                None => break,
            }
        }
        let v = self.expr(value, scope);
        for l in lefts {
            self.bind_target(l, v.clone(), scope);
        }
    }
    fn bind_target(&mut self, target: Node<'_>, value: Option<Ex>, scope: usize) {
        match target.kind() {
            "identifier" => {
                if let (Some(v), Some(slot)) = (value, self.lookup(scope, text(target, self.src))) {
                    self.push(ConB::Flow(slot, v));
                }
            }
            "attribute" => {
                let object = target
                    .child_by_field_name("object")
                    .and_then(|o| self.expr(o, scope));
                if let (Some(o), Some(a), Some(v)) =
                    (object, target.child_by_field_name("attribute"), value)
                {
                    let attr = text(a, self.src).to_string();
                    self.push(ConB::Store(o, attr, v));
                }
            }
            "subscript" => {
                // A container store: the container holds the value.
                if let Some(ix) = target.child_by_field_name("subscript") {
                    self.expr(ix, scope);
                }
                if let Some(v) = target.child_by_field_name("value") {
                    self.bind_target(v, value, scope);
                }
            }
            "pattern_list"
            | "tuple_pattern"
            | "list_pattern"
            | "tuple"
            | "list"
            | "list_splat_pattern"
            | "parenthesized_expression" => {
                for c in named(target) {
                    self.bind_target(c, value.clone(), scope);
                }
            }
            _ => {
                self.expr(target, scope);
            }
        }
    }
    fn ret(&mut self, n: Node<'_>, scope: usize) {
        let Some(value) = named(n).into_iter().next() else {
            return;
        };
        let owner = self.nearest(scope, false).and_then(|fs| {
            let sc = &self.scopes[fs];
            Some((fs, sc.func?, sc.def.clone()?))
        });
        let Some((fs, fi, id)) = owner else {
            self.expr(value, scope);
            return;
        };
        if value.kind() == "identifier" {
            let name = text(value, self.src);
            if !self.scopes[fs].assigned.contains(name)
                && let Some(p) = self.ir.funcs[fi].params.iter().position(|x| x == name)
            {
                // `return p`: each call site gets its own argument back.
                if !self.ir.funcs[fi].pass.contains(&p) {
                    self.ir.funcs[fi].pass.push(p);
                }
                let default = self.scopes[fs]
                    .defaults
                    .iter()
                    .find(|(i, _)| *i == p)
                    .map(|(_, e)| e.clone());
                if let Some(d) = default {
                    self.push(ConB::Flow(Slot::Ret(id), d));
                }
                return;
            }
        }
        if let Some(ex) = self.expr(value, scope) {
            self.push(ConB::Flow(Slot::Ret(id), ex));
        }
    }
    fn import(&mut self, n: Node<'_>, scope: usize) {
        let Some(targets) = self.facts.imports.get(&(self.key.clone(), line(n))) else {
            return;
        };
        let from = n.kind() == "import_from_statement";
        let files = targets
            .iter()
            .filter_map(|t| self.facts.file_key.get(t))
            .collect::<Vec<_>>();
        // `from m import *` at module level re-exports m's names: `pkg.Name`
        // then finds `Name` through the star (see `Solver::attr`).
        if from
            && self.scopes[scope].kind == Kind::Module
            && named(n).iter().any(|c| c.kind() == "wildcard_import")
            && let [k] = files[..]
        {
            self.ir.stars.push(k.clone());
            return;
        }
        let mut flows = vec![];
        for (name, local) in self.import_names(n) {
            let value = if from {
                let symbol = targets.iter().find(|t| {
                    !self.facts.file_key.contains_key(*t)
                        && self.facts.label.get(*t).is_some_and(|l| bare(l) == name)
                });
                if let Some(t) = symbol {
                    Some(Ex::Sym(t.clone()))
                } else if let Some(k) = files.iter().find(|k| module_stem(k) == name) {
                    Some(Ex::Module((*k).clone()))
                } else if let [k] = files[..] {
                    // A module-level name of the source module (`from m import
                    // handler`), read as `m.handler` so names m re-exports with
                    // `import *` resolve too.
                    Some(Ex::Attr(Box::new(Ex::Module(k.clone())), name.clone()))
                } else {
                    None
                }
            } else if !name.contains('.') || name != local {
                files
                    .iter()
                    .find(|k| is_module(k, &name))
                    .map(|k| Ex::Module((*k).clone()))
            } else {
                None
            };
            if let (Some(v), Some(slot)) = (value, self.lookup(scope, &local)) {
                flows.push(ConB::Flow(slot, v));
            }
        }
        self.ir.cons.extend(flows);
    }

    fn function(&mut self, n: Node<'_>, scope: usize, decorators: &[Node<'_>]) {
        let Some(name) = n.child_by_field_name("name") else {
            return;
        };
        let name = text(name, self.src).to_string();
        let id = self.def_id(line(n), &name, true);
        let class = match self.scopes[scope].kind {
            Kind::Class => self.scopes[scope].class.clone(),
            _ => None,
        };
        let decorated = |d: &str| {
            decorators
                .iter()
                .any(|n| text(*n, self.src).trim_start_matches('@').trim() == d)
        };
        let kind = if decorated("staticmethod") {
            FnKind::Static
        } else if decorated("classmethod") {
            FnKind::ClassMethod
        } else {
            FnKind::Plain
        };
        let (mut params, mut positional, mut default_nodes) = (vec![], usize::MAX, vec![]);
        if let Some(list) = n.child_by_field_name("parameters") {
            for p in named(list) {
                match p.kind() {
                    "identifier" => params.push(text(p, self.src).to_string()),
                    "default_parameter" | "typed_default_parameter" => {
                        if let Some(nm) = p.child_by_field_name("name") {
                            params.push(text(nm, self.src).to_string());
                            if let Some(v) = p.child_by_field_name("value") {
                                default_nodes.push((params.len() - 1, v));
                            }
                        }
                    }
                    "typed_parameter" => {
                        match named(p).into_iter().find(|c| c.kind() == "identifier") {
                            Some(nm) => params.push(text(nm, self.src).to_string()),
                            None => positional = positional.min(params.len()),
                        }
                    }
                    "list_splat_pattern" | "dictionary_splat_pattern" | "keyword_separator" => {
                        positional = positional.min(params.len());
                    }
                    _ => {}
                }
            }
        }
        let positional = positional.min(params.len());
        // Defaults evaluate in the defining scope.
        let defaults = default_nodes
            .into_iter()
            .filter_map(|(i, v)| Some((i, self.expr(v, scope)?)))
            .collect::<Vec<_>>();
        let new = self.scopes.len();
        let mut sc = ScopeB::new(Kind::Function, Some(scope));
        if let Some(body) = n.child_by_field_name("body") {
            self.collect(body, &mut sc.assigned, &mut sc.outer, &mut sc.globals);
        }
        sc.locals = params
            .iter()
            .cloned()
            .chain(sc.assigned.iter().cloned())
            .collect();
        sc.def = id.clone();
        sc.class = class.clone();
        sc.receiver = if class.is_some() && kind != FnKind::Static {
            params.first().cloned()
        } else {
            None
        };
        sc.func = id.as_ref().map(|id| {
            self.ir.funcs.push(FuncB {
                id: id.clone(),
                scope: new,
                params: params.clone(),
                positional,
                pass: vec![],
                kind,
            });
            self.ir.funcs.len() - 1
        });
        sc.defaults = defaults.clone();
        let receiver = sc.receiver.clone();
        self.scopes.push(sc);
        for (i, ex) in defaults {
            self.push(ConB::Flow(Slot::Local(new, params[i].clone()), ex));
        }
        if let (Some(c), Some(r)) = (class, receiver) {
            let v = match kind {
                FnKind::ClassMethod => Ex::Class(c),
                _ => Ex::Inst(c),
            };
            self.push(ConB::Flow(Slot::Local(new, r), v));
        }
        if let Some(body) = n.child_by_field_name("body") {
            self.visit(body, new);
        }
        // Methods are found through the class hierarchy, not class-body names.
        let value = id.map(Ex::Func);
        self.bind_decorated(&name, value, decorators, scope);
    }
    fn class(&mut self, n: Node<'_>, scope: usize, decorators: &[Node<'_>]) {
        let Some(name) = n.child_by_field_name("name") else {
            return;
        };
        let name = text(name, self.src).to_string();
        let id = self.def_id(line(n), &name, false);
        if let Some(bases) = n.child_by_field_name("superclasses") {
            self.expr(bases, scope);
        }
        let new = self.scopes.len();
        let mut sc = ScopeB::new(Kind::Class, Some(scope));
        if let Some(body) = n.child_by_field_name("body") {
            self.collect(body, &mut sc.assigned, &mut sc.outer, &mut sc.globals);
        }
        sc.locals = sc.assigned.clone();
        sc.class = id.clone();
        self.scopes.push(sc);
        if let Some(c) = &id {
            self.ir.classes.push(c.clone());
        }
        if let Some(body) = n.child_by_field_name("body") {
            self.visit(body, new);
        }
        self.bind_decorated(&name, id.map(Ex::Class), decorators, scope);
    }
    /// `@d1 @d2 def f` binds `f = d1(d2(f))`; an unknown decorator, or one
    /// with no known result, is taken to return its argument.
    fn bind_decorated(
        &mut self,
        name: &str,
        value: Option<Ex>,
        decorators: &[Node<'_>],
        scope: usize,
    ) {
        let mut value = value;
        let caller = self.caller(scope);
        for d in decorators.iter().rev() {
            let Some(e) = named(*d).into_iter().next() else {
                continue;
            };
            let Some(func) = self.expr(e, scope) else {
                continue;
            };
            self.ir.sites.push(SiteB {
                caller: caller.clone(),
                line: line(*d),
                func,
                args: vec![value.clone().unwrap_or(Ex::Many(vec![]))],
                kwargs: vec![],
                super_name: None,
            });
            let result = Ex::Call(self.ir.sites.len() - 1);
            value = Some(match value {
                Some(v) => Ex::OrElse(Box::new(result), Box::new(v)),
                None => result,
            });
        }
        if self.scopes[scope].kind != Kind::Class
            && let (Some(v), Some(slot)) = (value, self.lookup(scope, name))
        {
            self.push(ConB::Flow(slot, v));
        }
    }

    fn expr(&mut self, n: Node<'_>, scope: usize) -> Option<Ex> {
        if self.depth > MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        let out = self.expr_inner(n, scope);
        self.depth -= 1;
        out
    }
    fn many(items: Vec<Ex>) -> Option<Ex> {
        (!items.is_empty()).then_some(Ex::Many(items))
    }
    fn expr_inner(&mut self, n: Node<'_>, scope: usize) -> Option<Ex> {
        match n.kind() {
            "identifier" => self.lookup(scope, text(n, self.src)).map(Ex::Slot),
            "attribute" => {
                let attr = text(n.child_by_field_name("attribute")?, self.src).to_string();
                let object = n.child_by_field_name("object")?;
                if is_super(object, self.src) {
                    let m = self.nearest(scope, true)?;
                    let sc = &self.scopes[m];
                    let (class, recv) = (sc.class.clone()?, sc.receiver.clone()?);
                    return Some(Ex::Attr(
                        Box::new(Ex::Super(class, Slot::Local(m, recv))),
                        attr,
                    ));
                }
                let object = self.expr(object, scope)?;
                Some(Ex::Attr(Box::new(object), attr))
            }
            "call" => self.call(n, scope).map(Ex::Call),
            "list" | "tuple" | "set" | "expression_list" | "boolean_operator" => {
                let items = named(n)
                    .into_iter()
                    .filter_map(|c| self.expr(c, scope))
                    .collect();
                Self::many(items)
            }
            "dictionary" => {
                let mut items = vec![];
                for c in named(n) {
                    if c.kind() == "pair" {
                        if let Some(k) = c.child_by_field_name("key") {
                            self.expr(k, scope);
                        }
                        items.extend(
                            c.child_by_field_name("value")
                                .and_then(|v| self.expr(v, scope)),
                        );
                    } else {
                        items.extend(self.expr(c, scope));
                    }
                }
                Self::many(items)
            }
            "conditional_expression" => {
                let mut items = vec![];
                for (i, c) in named(n).into_iter().enumerate() {
                    let v = self.expr(c, scope);
                    if i != 1 {
                        items.extend(v);
                    }
                }
                Self::many(items)
            }
            "parenthesized_expression" | "await" | "list_splat" | "dictionary_splat" => {
                let mut last = None;
                for c in named(n) {
                    last = self.expr(c, scope);
                }
                last
            }
            "subscript" => {
                let value = n.child_by_field_name("value");
                for c in named(n) {
                    if Some(c.id()) != value.map(|v| v.id()) {
                        self.expr(c, scope);
                    }
                }
                value.and_then(|v| self.expr(v, scope))
            }
            "named_expression" => {
                let v = n
                    .child_by_field_name("value")
                    .and_then(|v| self.expr(v, scope));
                if let (Some(name), Some(v)) = (n.child_by_field_name("name"), v.clone())
                    && let Some(slot) = self.lookup(scope, text(name, self.src))
                {
                    self.push(ConB::Flow(slot, v));
                }
                v
            }
            "yield" => {
                let v = named(n)
                    .into_iter()
                    .next()
                    .and_then(|c| self.expr(c, scope));
                let owner = self
                    .nearest(scope, false)
                    .and_then(|fs| self.scopes[fs].def.clone());
                if let (Some(v), Some(id)) = (v, owner) {
                    self.push(ConB::Flow(Slot::Ret(id), v));
                }
                None
            }
            "list_comprehension"
            | "set_comprehension"
            | "generator_expression"
            | "dictionary_comprehension" => {
                let body = n.child_by_field_name("body");
                for c in named(n) {
                    if c.kind() == "for_in_clause" {
                        let it = c
                            .child_by_field_name("right")
                            .and_then(|r| self.expr(r, scope));
                        if let Some(it) = it {
                            self.iterate(c.child_by_field_name("left"), it, line(c), scope);
                        }
                    } else if Some(c.id()) != body.map(|b| b.id()) {
                        self.expr(c, scope);
                    }
                }
                let body = body?;
                let value = if body.kind() == "pair" {
                    if let Some(k) = body.child_by_field_name("key") {
                        self.expr(k, scope);
                    }
                    body.child_by_field_name("value")
                        .and_then(|v| self.expr(v, scope))
                } else {
                    self.expr(body, scope)
                };
                value.map(|v| Ex::Many(vec![v]))
            }
            "lambda" | "integer" | "float" | "true" | "false" | "none" | "comment" | "ellipsis" => {
                None
            }
            _ => {
                for c in named(n) {
                    self.visit(c, scope);
                }
                None
            }
        }
    }
    fn call(&mut self, n: Node<'_>, scope: usize) -> Option<usize> {
        let function = n.child_by_field_name("function")?;
        let func = self.expr(function, scope);
        let super_name = if function.kind() == "attribute"
            && function
                .child_by_field_name("object")
                .is_some_and(|o| is_super(o, self.src))
        {
            function
                .child_by_field_name("attribute")
                .map(|a| text(a, self.src).to_string())
        } else {
            None
        };
        let (mut args, mut kwargs, mut splat) = (vec![], vec![], false);
        if let Some(list) = n.child_by_field_name("arguments") {
            if list.kind() == "argument_list" {
                for a in named(list) {
                    match a.kind() {
                        "keyword_argument" => {
                            let v = a
                                .child_by_field_name("value")
                                .and_then(|v| self.expr(v, scope));
                            if let (Some(k), Some(v)) = (a.child_by_field_name("name"), v) {
                                kwargs.push((text(k, self.src).to_string(), v));
                            }
                        }
                        "list_splat" | "dictionary_splat" => {
                            // Positions after a splat are unknown.
                            splat = true;
                            self.expr(a, scope);
                        }
                        "comment" => {}
                        _ => {
                            let v = self.expr(a, scope);
                            if !splat {
                                args.push(v.unwrap_or(Ex::Many(vec![])));
                            }
                        }
                    }
                }
            } else {
                let v = self.expr(list, scope);
                args.push(v.unwrap_or(Ex::Many(vec![])));
            }
        }
        let func = func?;
        let caller = self.caller(scope);
        self.ir.sites.push(SiteB {
            caller,
            line: line(n),
            func,
            args,
            kwargs,
            super_name,
        });
        Some(self.ir.sites.len() - 1)
    }
}

/// Stack for the per-file builders: the same as per-file extraction, since the
/// builder recurses once per syntax level (up to `MAX_DEPTH`) and debug builds
/// use far larger frames than release.
const STACK: usize = 32 << 20;

/// Per-file IR for every file, in file order, on as many big-stack workers as
/// the current run allows (always off the caller's thread, whose stack size is
/// unknown).
fn build_all(files: &[(PathBuf, String)], facts: &Facts) -> Vec<Option<FileIr>> {
    let workers = parse_cache::workers().clamp(1, files.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots = std::sync::Mutex::new(Vec::with_capacity(files.len()));
    let work = || {
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some((path, key)) = files.get(i) else {
                break;
            };
            let ir = build_file(path, key.clone(), facts);
            slots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((i, ir));
        }
    };
    std::thread::scope(|scope| {
        let spawned = (0..workers)
            .filter(|_| {
                std::thread::Builder::new()
                    .stack_size(STACK)
                    .spawn_scoped(scope, work)
                    .is_ok()
            })
            .count();
        if spawned == 0 {
            work();
        }
    });
    let mut slots = slots.into_inner().unwrap_or_else(|e| e.into_inner());
    slots.sort_by_key(|(i, _)| *i);
    slots.into_iter().map(|(_, ir)| ir).collect()
}

fn build_file(path: &Path, key: String, facts: &Facts) -> Option<FileIr> {
    let tree = resolution_facts::parse_python_tree(path)?;
    let root = tree.tree.root_node();
    let mut b = Builder {
        src: &tree.source,
        key: key.clone(),
        facts,
        scopes: vec![],
        ir: FileIr {
            path: path.to_string_lossy().into_owned(),
            key,
            ..FileIr::default()
        },
        depth: 0,
    };
    let mut module = ScopeB::new(Kind::Module, None);
    b.collect(
        root,
        &mut module.assigned,
        &mut module.outer,
        &mut module.globals,
    );
    module.locals = module.assigned.clone();
    b.ir.module_names = module.assigned.iter().cloned().collect();
    b.scopes.push(module);
    b.visit(root, 0);
    b.ir.scopes = b.scopes.len();
    Some(b.ir)
}

// ---------------------------------------------------------------------------
// Whole-program solution over interned values.

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Val {
    Func(u32),
    /// A method bound to an instance (or, for a classmethod, the class) of `.1`.
    Bound(u32, u32),
    Class(u32),
    Inst(u32),
    Module(u32),
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Local(u32, u32),
    Field(u32, u32),
    Ret(u32),
    Module(u32, u32),
}
enum E {
    Var(u32),
    Val(Val),
    Attr(Box<E>, u32),
    Super(u32, u32),
    Call(u32),
    Many(Vec<E>),
    OrElse(Box<E>, Box<E>),
}
struct Site {
    caller: Option<u32>,
    file: usize,
    line: usize,
    func: E,
    args: Vec<E>,
    kwargs: Vec<(u32, E)>,
    super_name: Option<u32>,
}
enum Con {
    Flow(u32, E),
    Store(E, u32, E),
    Iter {
        caller: Option<u32>,
        file: usize,
        line: usize,
        iter: E,
        targets: Vec<u32>,
    },
}
struct Func {
    params: Vec<u32>,
    names: Vec<u32>,
    positional: usize,
    pass: Vec<usize>,
    kind: FnKind,
}

#[derive(Default)]
struct Interner {
    map: FxHashMap<String, u32>,
    list: Vec<String>,
}
impl Interner {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.map.get(s) {
            return i;
        }
        let i = self.list.len() as u32;
        self.map.insert(s.to_string(), i);
        self.list.push(s.to_string());
        i
    }
    fn name(&self, i: u32) -> &str {
        &self.list[i as usize]
    }
}

#[derive(Default)]
struct Solver {
    ids: Interner,
    syms: Interner,
    files: Interner,
    vars: FxHashMap<Key, u32>,
    pts: Vec<Vec<Val>>,
    sites: Vec<Site>,
    cons: Vec<Con>,
    funcs: FxHashMap<u32, Func>,
    methods: FxHashMap<(u32, u32), u32>,
    bases: FxHashMap<u32, Vec<u32>>,
    /// Python C3 linearizations, computed once per solve. Empty means the
    /// hierarchy is cyclic, inconsistent or exceeds the analysis bound.
    mros: FxHashMap<u32, Vec<u32>>,
    submodules: FxHashMap<(u32, u32), u32>,
    /// `from m import *` re-exports: file -> source modules.
    stars: FxHashMap<u32, Vec<u32>>,
    /// (file, name) pairs a module body binds itself.
    bound: FxHashSet<(u32, u32)>,
    classes: FxHashSet<u32>,
    functions: FxHashSet<u32>,
    init: u32,
    call: u32,
    iter: u32,
    next: u32,
}
impl Solver {
    fn var(&mut self, k: Key) -> u32 {
        if let Some(&v) = self.vars.get(&k) {
            return v;
        }
        let v = self.pts.len() as u32;
        self.pts.push(vec![]);
        self.vars.insert(k, v);
        v
    }
    fn slot(&mut self, base: u32, s: &Slot) -> u32 {
        let k = match s {
            Slot::Local(i, n) => Key::Local(base + *i as u32, self.syms.id(n)),
            Slot::Field(c, n) => Key::Field(self.ids.id(c), self.syms.id(n)),
            Slot::Ret(f) => Key::Ret(self.ids.id(f)),
            Slot::Module(k, n) => Key::Module(self.files.id(k), self.syms.id(n)),
        };
        self.var(k)
    }
    fn conv(&mut self, base: u32, sites: u32, e: &Ex) -> E {
        match e {
            Ex::Slot(s) => E::Var(self.slot(base, s)),
            Ex::Sym(id) => {
                let i = self.ids.id(id);
                if self.functions.contains(&i) {
                    E::Val(Val::Func(i))
                } else if self.classes.contains(&i) {
                    E::Val(Val::Class(i))
                } else {
                    E::Many(vec![])
                }
            }
            Ex::Func(id) => E::Val(Val::Func(self.ids.id(id))),
            Ex::Class(id) => E::Val(Val::Class(self.ids.id(id))),
            Ex::Inst(id) => E::Val(Val::Inst(self.ids.id(id))),
            Ex::Module(k) => E::Val(Val::Module(self.files.id(k))),
            Ex::Attr(o, a) => {
                let o = self.conv(base, sites, o);
                E::Attr(Box::new(o), self.syms.id(a))
            }
            Ex::Super(c, recv) => {
                let c = self.ids.id(c);
                E::Super(c, self.slot(base, recv))
            }
            Ex::Call(i) => E::Call(sites + *i as u32),
            Ex::Many(xs) => E::Many(xs.iter().map(|x| self.conv(base, sites, x)).collect()),
            Ex::OrElse(a, b) => {
                let a = self.conv(base, sites, a);
                E::OrElse(Box::new(a), Box::new(self.conv(base, sites, b)))
            }
        }
    }

    fn prepare_mros(&mut self) {
        fn linearize(
            c: u32,
            bases: &FxHashMap<u32, Vec<u32>>,
            cache: &mut FxHashMap<u32, Vec<u32>>,
            active: &mut FxHashSet<u32>,
        ) -> Option<Vec<u32>> {
            if let Some(mro) = cache.get(&c) {
                return (!mro.is_empty()).then(|| mro.clone());
            }
            if active.len() >= MAX_VALUES || !active.insert(c) {
                return None;
            }
            let result = (|| {
                let parents = bases.get(&c).cloned().unwrap_or_default();
                if parents.len() >= MAX_VALUES {
                    return None;
                }
                let mut sequences = parents
                    .iter()
                    .map(|p| linearize(*p, bases, cache, active))
                    .collect::<Option<Vec<_>>>()?;
                sequences.push(parents);
                let mut out = vec![c];
                while sequences.iter().any(|s| !s.is_empty()) {
                    let candidate =
                        sequences
                            .iter()
                            .filter_map(|s| s.first())
                            .copied()
                            .find(|head| {
                                sequences
                                    .iter()
                                    .all(|s| !s.get(1..).unwrap_or_default().contains(head))
                            })?;
                    if out.len() >= MAX_VALUES {
                        return None;
                    }
                    out.push(candidate);
                    for sequence in &mut sequences {
                        if sequence.first() == Some(&candidate) {
                            sequence.remove(0);
                        }
                    }
                }
                Some(out)
            })();
            active.remove(&c);
            cache.insert(c, result.clone().unwrap_or_default());
            result
        }
        self.mros.clear();
        for c in &self.classes {
            linearize(*c, &self.bases, &mut self.mros, &mut FxHashSet::default());
        }
    }
    fn mro(&self, c: u32) -> &[u32] {
        self.mros.get(&c).map(Vec::as_slice).unwrap_or_default()
    }
    fn method(&self, c: u32, name: u32) -> Option<u32> {
        self.mro(c)
            .iter()
            .copied()
            .find_map(|k| self.methods.get(&(k, name)).copied())
    }
    fn receiver(&self, f: u32, class: u32) -> Val {
        match self.funcs.get(&f).map(|f| f.kind) {
            Some(FnKind::ClassMethod) => Val::Class(class),
            _ => Val::Inst(class),
        }
    }
    fn eval(&self, e: &E, out: &mut Vec<Val>) {
        match e {
            E::Var(v) => out.extend_from_slice(&self.pts[*v as usize]),
            E::Val(x) => out.push(*x),
            E::Attr(object, a) => {
                if let E::Super(class, recv) = &**object {
                    let found = self
                        .mro(*class)
                        .iter()
                        .copied()
                        .skip(1)
                        .find_map(|k| self.methods.get(&(k, *a)).copied());
                    if let Some(m) = found {
                        let mut bound = false;
                        for r in &self.pts[*recv as usize] {
                            if let Val::Inst(c) | Val::Class(c) = r {
                                out.push(Val::Bound(m, *c));
                                bound = true;
                            }
                        }
                        if !bound {
                            out.push(Val::Bound(m, *class));
                        }
                    }
                    return;
                }
                let mut objects = vec![];
                self.eval(object, &mut objects);
                objects.sort_unstable();
                objects.dedup();
                for o in objects {
                    self.attr(o, *a, out);
                }
            }
            E::Super(..) => {}
            E::Call(site) => self.result(*site as usize, out),
            E::Many(items) => {
                for i in items {
                    self.eval(i, out);
                }
            }
            E::OrElse(a, b) => {
                let n = out.len();
                self.eval(a, out);
                if out.len() == n {
                    self.eval(b, out);
                }
            }
        }
    }
    fn values(&self, e: &E) -> Vec<Val> {
        let mut out = vec![];
        self.eval(e, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }
    fn attr(&self, o: Val, a: u32, out: &mut Vec<Val>) {
        match o {
            Val::Module(f) => {
                // The module's own binding and submodule, as before; a name the
                // module never binds is looked up in its `import *` sources.
                // Whether a module binds a name is fixed before solving, so the
                // lookup stays monotone.
                let (mut stack, mut seen) = (vec![f], FxHashSet::default());
                while let Some(m) = stack.pop() {
                    if !seen.insert(m) || seen.len() > 64 {
                        continue;
                    }
                    let own = self.vars.get(&Key::Module(m, a));
                    if let Some(v) = own {
                        out.extend_from_slice(&self.pts[*v as usize]);
                    }
                    let sub = self.submodules.get(&(m, a));
                    if let Some(sub) = sub {
                        out.push(Val::Module(*sub));
                    }
                    if !self.bound.contains(&(m, a))
                        && sub.is_none()
                        && let Some(sources) = self.stars.get(&m)
                    {
                        stack.extend(sources.iter().rev());
                    }
                }
            }
            Val::Class(c) | Val::Inst(c) => {
                let mro = self.mro(c);
                if let Some(m) = mro.iter().find_map(|k| self.methods.get(&(*k, a)).copied()) {
                    let kind = self.funcs.get(&m).map_or(FnKind::Plain, |f| f.kind);
                    out.push(match (o, kind) {
                        (_, FnKind::Static) => Val::Func(m),
                        (_, FnKind::ClassMethod) | (Val::Inst(_), FnKind::Plain) => {
                            Val::Bound(m, c)
                        }
                        _ => Val::Func(m),
                    });
                }
                for k in mro {
                    if let Some(v) = self.vars.get(&Key::Field(*k, a)) {
                        out.extend_from_slice(&self.pts[*v as usize]);
                    }
                }
            }
            _ => {}
        }
    }
    /// The callee a value runs when called: (function, receiver class).
    fn callee(&self, v: Val) -> Option<(u32, Option<u32>)> {
        match v {
            Val::Func(f) => Some((f, None)),
            Val::Bound(f, c) => Some((f, Some(c))),
            Val::Class(c) => self.method(c, self.init).map(|m| (m, Some(c))),
            Val::Inst(c) => self.method(c, self.call).map(|m| (m, Some(c))),
            Val::Module(_) => None,
        }
    }
    fn returns(
        &self,
        f: u32,
        recv: Option<u32>,
        args: &[E],
        kwargs: &[(u32, E)],
        out: &mut Vec<Val>,
    ) {
        if let Some(v) = self.vars.get(&Key::Ret(f)) {
            out.extend_from_slice(&self.pts[*v as usize]);
        }
        let Some(func) = self.funcs.get(&f) else {
            return;
        };
        let offset = usize::from(recv.is_some());
        for &p in &func.pass {
            if p < offset {
                out.extend(recv.map(|r| self.receiver(f, r)));
                continue;
            }
            if p < func.positional
                && let Some(a) = args.get(p - offset)
            {
                self.eval(a, out);
            }
            if let Some((_, a)) = kwargs.iter().find(|(k, _)| Some(k) == func.names.get(p)) {
                self.eval(a, out);
            }
        }
    }
    fn result(&self, site: usize, out: &mut Vec<Val>) {
        let s = &self.sites[site];
        for v in self.values(&s.func) {
            if let Val::Class(c) = v {
                out.push(Val::Inst(c));
            } else if let Some((f, recv)) = self.callee(v) {
                self.returns(f, recv, &s.args, &s.kwargs, out);
            }
        }
    }
    fn add(&mut self, v: u32, vals: &[Val]) -> bool {
        let set = &mut self.pts[v as usize];
        let mut changed = false;
        for x in vals {
            if set.len() >= MAX_VALUES {
                break;
            }
            if let Err(i) = set.binary_search(x) {
                set.insert(i, *x);
                changed = true;
            }
        }
        changed
    }
    /// Parameter flows of one call site: (parameter var, values).
    fn bindings(&self, site: usize) -> Vec<(u32, Vec<Val>)> {
        let s = &self.sites[site];
        let mut flows = vec![];
        for v in self.values(&s.func) {
            let Some((f, recv)) = self.callee(v) else {
                continue;
            };
            let Some(func) = self.funcs.get(&f) else {
                continue;
            };
            let offset = usize::from(recv.is_some());
            if let (Some(r), Some(&p0)) = (recv, func.params.first()) {
                flows.push((p0, vec![self.receiver(f, r)]));
            }
            for (i, a) in s.args.iter().enumerate() {
                if i + offset < func.positional {
                    flows.push((func.params[i + offset], self.values(a)));
                }
            }
            for (k, a) in &s.kwargs {
                if let Some(p) = func.names.iter().position(|n| n == k) {
                    flows.push((func.params[p], self.values(a)));
                }
            }
        }
        flows
    }
    /// Values a `for` loop over `iter` assigns, and the protocol methods it calls.
    fn iteration(&self, iter: &E) -> (Vec<Val>, Vec<u32>) {
        let (mut items, mut called) = (vec![], vec![]);
        for v in self.values(iter) {
            let Val::Inst(c) = v else {
                items.push(v);
                continue;
            };
            let Some(it) = self.method(c, self.iter) else {
                continue;
            };
            called.push(it);
            let mut iterators = vec![];
            self.returns(it, Some(c), &[], &[], &mut iterators);
            for i in iterators {
                if let Val::Inst(k) = i
                    && let Some(nx) = self.method(k, self.next)
                {
                    called.push(nx);
                    self.returns(nx, Some(k), &[], &[], &mut items);
                }
            }
        }
        (items, called)
    }
    fn solve(&mut self) -> usize {
        self.prepare_mros();
        for pass in 1..=MAX_PASSES {
            let mut changed = false;
            for i in 0..self.cons.len() {
                let flows: Vec<(u32, Vec<Val>)> = match &self.cons[i] {
                    Con::Flow(v, e) => vec![(*v, self.values(e))],
                    Con::Store(object, a, e) => {
                        let vals = self.values(e);
                        let (objects, a) = (self.values(object), *a);
                        let mut out = vec![];
                        for o in objects {
                            if let Val::Inst(c) | Val::Class(c) = o {
                                out.push((Key::Field(c, a), vals.clone()));
                            }
                        }
                        let mut flows = vec![];
                        for (k, vals) in out {
                            flows.push((self.var(k), vals));
                        }
                        flows
                    }
                    Con::Iter { iter, targets, .. } => {
                        let items = self.iteration(iter).0;
                        targets.iter().map(|t| (*t, items.clone())).collect()
                    }
                };
                for (v, vals) in flows {
                    changed |= self.add(v, &vals);
                }
            }
            for i in 0..self.sites.len() {
                for (v, vals) in self.bindings(i) {
                    changed |= self.add(v, &vals);
                }
            }
            if !changed {
                return pass;
            }
        }
        MAX_PASSES
    }
    /// Graph ids a call site may run.
    fn site_targets(&self, func: &E) -> Vec<u32> {
        let mut out = vec![];
        for v in self.values(func) {
            match v {
                Val::Func(f) | Val::Bound(f, _) => out.push(f),
                Val::Class(c) => out.push(c),
                Val::Inst(c) => out.extend(self.method(c, self.call)),
                Val::Module(_) => {}
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

fn edge(src: &str, tgt: &str, line: usize, file: &str, resolution: &str) -> Value {
    json!({"source": src, "target": tgt, "relation": "calls", "context": "call",
        "confidence": "INFERRED", "confidence_score": 0.85, "source_file": file,
        "source_location": format!("L{line}"), "weight": 1.0, "_origin": "ast",
        "resolution": resolution})
}

/// Add precise `calls` edges for the Python files among `paths`. `context_*`
/// is the rest of the graph when only some files are being re-extracted; its
/// Python files are analysed too, but only callers in `paths` get edges.
pub fn resolve(
    paths: &[PathBuf],
    nodes: &[Value],
    edges: &mut Vec<Value>,
    context_nodes: &[Value],
    context_edges: &[Value],
    root: &Path,
) {
    let started = std::time::Instant::now();
    let timing = std::env::var("GRAPHICLE_PRECISE_TIMING").is_ok_and(|v| v == "1");
    let all_nodes = nodes.iter().chain(context_nodes).collect::<Vec<_>>();
    let all_edges = edges.iter().chain(context_edges).collect::<Vec<_>>();
    let facts = Facts::build(&all_nodes, &all_edges, root);
    let mut files: Vec<(PathBuf, String)> = vec![];
    let mut seen = FxHashSet::default();
    for p in paths
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "py"))
    {
        let key = source_key(&p.to_string_lossy(), root);
        if seen.insert(key.clone()) {
            files.push((p.clone(), key));
        }
    }
    let emit = seen.clone();
    for n in context_nodes {
        let sf = s(n, "source_file");
        if sf.ends_with(".py") && Path::new(sf).file_name() == Path::new(s(n, "label")).file_name()
        {
            let key = source_key(sf, root);
            let path = if Path::new(sf).is_absolute() {
                PathBuf::from(sf)
            } else {
                root.join(sf)
            };
            if seen.insert(key.clone()) {
                files.push((path, key));
            }
        }
    }
    let irs = build_all(&files, &facts);
    let parsed = started.elapsed();

    let mut sv = Solver::default();
    for id in facts.label.keys() {
        let label = &facts.label[id];
        if label.ends_with(')') {
            let i = sv.ids.id(id);
            sv.functions.insert(i);
        }
    }
    for c in facts
        .classes
        .iter()
        .chain(irs.iter().flatten().flat_map(|ir| &ir.classes))
    {
        let i = sv.ids.id(c);
        sv.classes.insert(i);
    }
    for ((c, name), m) in &facts.methods {
        let key = (sv.ids.id(c), sv.syms.id(name));
        let m = sv.ids.id(m);
        sv.methods.insert(key, m);
    }
    for (c, bases) in &facts.bases {
        let c = sv.ids.id(c);
        let bases = bases.iter().map(|b| sv.ids.id(b)).collect();
        sv.bases.insert(c, bases);
    }
    for key in facts.file_node.keys() {
        let path = Path::new(key);
        let (package, name) = if path.file_name().is_some_and(|f| f == "__init__.py") {
            let dir = path.parent().unwrap_or(Path::new(""));
            (dir.parent().unwrap_or(Path::new("")), module_stem(key))
        } else {
            (path.parent().unwrap_or(Path::new("")), module_stem(key))
        };
        let init = package.join("__init__.py").to_string_lossy().into_owned();
        if facts.file_node.contains_key(&init) {
            let k = (sv.files.id(&init), sv.syms.id(name));
            let m = sv.files.id(key);
            sv.submodules.insert(k, m);
        }
    }
    sv.init = sv.syms.id("__init__");
    sv.call = sv.syms.id("__call__");
    sv.iter = sv.syms.id("__iter__");
    sv.next = sv.syms.id("__next__");

    let mut file_info: Vec<(String, bool)> = vec![];
    let mut scope_base = 0u32;
    for ir in irs.iter().flatten() {
        let file = file_info.len();
        file_info.push((ir.path.clone(), emit.contains(&ir.key)));
        if !ir.stars.is_empty() {
            let f = sv.files.id(&ir.key);
            let sources = ir.stars.iter().map(|k| sv.files.id(k)).collect();
            sv.stars.insert(f, sources);
            for name in &ir.module_names {
                let name = sv.syms.id(name);
                sv.bound.insert((f, name));
            }
        }
        let sites = sv.sites.len() as u32;
        for f in &ir.funcs {
            let params = f
                .params
                .iter()
                .map(|p| sv.slot(scope_base, &Slot::Local(f.scope, p.clone())))
                .collect();
            let names = f.params.iter().map(|p| sv.syms.id(p)).collect();
            let id = sv.ids.id(&f.id);
            sv.funcs.insert(
                id,
                Func {
                    params,
                    names,
                    positional: f.positional,
                    pass: f.pass.clone(),
                    kind: f.kind,
                },
            );
        }
        for site in &ir.sites {
            let caller = site.caller.as_ref().map(|c| sv.ids.id(c));
            let func = sv.conv(scope_base, sites, &site.func);
            let args = site
                .args
                .iter()
                .map(|a| sv.conv(scope_base, sites, a))
                .collect();
            let kwargs = site
                .kwargs
                .iter()
                .map(|(k, a)| (sv.syms.id(k), sv.conv(scope_base, sites, a)))
                .collect();
            let super_name = site.super_name.as_ref().map(|n| sv.syms.id(n));
            sv.sites.push(Site {
                caller,
                file,
                line: site.line,
                func,
                args,
                kwargs,
                super_name,
            });
        }
        for c in &ir.cons {
            let con = match c {
                ConB::Flow(slot, e) => {
                    Con::Flow(sv.slot(scope_base, slot), sv.conv(scope_base, sites, e))
                }
                ConB::Store(o, a, e) => {
                    let o = sv.conv(scope_base, sites, o);
                    let a = sv.syms.id(a);
                    Con::Store(o, a, sv.conv(scope_base, sites, e))
                }
                ConB::Iter {
                    caller,
                    line,
                    iter,
                    targets,
                } => Con::Iter {
                    caller: caller.as_ref().map(|c| sv.ids.id(c)),
                    file,
                    line: *line,
                    iter: sv.conv(scope_base, sites, iter),
                    targets: targets.iter().map(|t| sv.slot(scope_base, t)).collect(),
                },
            };
            sv.cons.push(con);
        }
        scope_base += ir.scopes as u32;
    }
    drop(irs);
    let interned = started.elapsed();
    let passes = sv.solve();
    let solved = started.elapsed();

    // A `super().m` site replaces the default engine's same-named guesses.
    let mut drop_edges: FxHashSet<(String, String, String)> = FxHashSet::default();
    let mut by_site: FxHashMap<(&str, usize), Vec<&Value>> = FxHashMap::default();
    for e in edges.iter().filter(|e| s(e, "relation") == "calls") {
        if let Some(l) = location(e) {
            by_site.entry((s(e, "source"), l)).or_default().push(e);
        }
    }
    for site in &sv.sites {
        let (Some(caller), Some(name)) = (site.caller, site.super_name) else {
            continue;
        };
        let targets = sv.site_targets(&site.func);
        if targets.is_empty() || !file_info[site.file].1 {
            continue;
        }
        let (caller, name) = (sv.ids.name(caller), sv.syms.name(name));
        for e in by_site.get(&(caller, site.line)).into_iter().flatten() {
            let tgt = s(e, "target");
            if facts.label.get(tgt).is_some_and(|l| bare(l) == name)
                && !targets.iter().any(|t| sv.ids.name(*t) == tgt)
            {
                drop_edges.insert((caller.into(), tgt.into(), s(e, "source_location").into()));
            }
        }
    }
    drop(by_site);
    let drop_count = drop_edges.len();
    if !drop_edges.is_empty() {
        edges.retain(|e| {
            !(s(e, "relation") == "calls"
                && drop_edges.contains(&(
                    s(e, "source").into(),
                    s(e, "target").into(),
                    s(e, "source_location").into(),
                )))
        });
    }
    // Edges, in file and site order, callees sorted by id. The graph keeps one
    // edge per node pair (either direction when undirected), so a pair that
    // already has any edge gets no second one.
    let mut have = edges
        .iter()
        .chain(context_edges)
        .flat_map(|e| {
            let (a, b) = (s(e, "source").to_string(), s(e, "target").to_string());
            [(a.clone(), b.clone()), (b, a)]
        })
        .collect::<FxHashSet<_>>();
    let mut added = vec![];
    let mut emit_site =
        |caller: u32, file: usize, l: usize, targets: Vec<u32>, resolution: &str| {
            let (path, emit) = &file_info[file];
            if !*emit || targets.is_empty() || targets.len() > MAX_TARGETS {
                return;
            }
            let caller = sv.ids.name(caller);
            let mut names = targets.iter().map(|t| sv.ids.name(*t)).collect::<Vec<_>>();
            names.sort_unstable();
            for t in names {
                if have.insert((caller.to_string(), t.to_string())) {
                    have.insert((t.to_string(), caller.to_string()));
                    added.push(edge(caller, t, l, path, resolution));
                }
            }
        };
    for site in &sv.sites {
        let Some(caller) = site.caller else { continue };
        let resolution = if site.super_name.is_some() {
            "super_call"
        } else {
            "points_to"
        };
        emit_site(
            caller,
            site.file,
            site.line,
            sv.site_targets(&site.func),
            resolution,
        );
    }
    for c in &sv.cons {
        if let Con::Iter {
            caller: Some(caller),
            file,
            line,
            iter,
            ..
        } = c
        {
            let mut called = sv.iteration(iter).1;
            called.sort_unstable();
            called.dedup();
            emit_site(*caller, *file, *line, called, "points_to");
        }
    }
    if timing {
        eprintln!(
            "[precise timing] files={} sites={} constraints={} vars={} passes={passes} \
             added={} dropped={} parse+ir={:.0}ms intern={:.0}ms solve={:.0}ms emit={:.0}ms",
            file_info.len(),
            sv.sites.len(),
            sv.cons.len(),
            sv.pts.len(),
            added.len(),
            drop_count,
            parsed.as_secs_f64() * 1e3,
            (interned - parsed).as_secs_f64() * 1e3,
            (solved - interned).as_secs_f64() * 1e3,
            (started.elapsed() - solved).as_secs_f64() * 1e3,
        );
    }
    edges.extend(added);
}

#[cfg(test)]
mod tests {
    use crate::extract::{ExtractOptions, extract};
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }
    fn run(files: &[(&str, &str)], precise: bool) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = files
            .iter()
            .map(|(rel, body)| write(&root, rel, body))
            .collect::<Vec<_>>();
        let _serial = crate::extractors::parse_cache::test_serial();
        extract(
            &paths,
            &ExtractOptions {
                root: Some(root.clone()),
                parallel: false,
                use_cache: false,
                receiver_resolution: false,
                precise,
                ..Default::default()
            },
        )
        .unwrap()
    }
    /// (source label, target label, resolution) of every `calls` edge.
    fn calls(v: &Value) -> Vec<(String, String, String)> {
        let label = |id: &str| {
            v["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["id"] == id)
                .map(|n| n["label"].as_str().unwrap().to_string())
                .unwrap_or_default()
        };
        let mut out = v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls")
            .map(|e| {
                (
                    label(e["source"].as_str().unwrap()),
                    label(e["target"].as_str().unwrap()),
                    e["resolution"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect::<Vec<_>>();
        out.sort();
        out
    }
    fn has(v: &Value, src: &str, tgt: &str) -> bool {
        calls(v).iter().any(|(s, t, _)| s == src && t == tgt)
    }
    fn precise_edges(v: &Value) -> Vec<(String, String, String)> {
        calls(v)
            .into_iter()
            .filter(|(_, _, r)| !r.is_empty())
            .collect()
    }

    #[test]
    fn default_mode_adds_nothing() {
        let files = [(
            "main.py",
            "def g():\n    pass\n\ndef f(cb):\n    cb()\n\ndef h():\n    f(g)\n",
        )];
        assert!(precise_edges(&run(&files, false)).is_empty());
        assert!(has(&run(&files, true), "f()", "g()"));
    }

    #[test]
    fn super_call_replaces_the_same_named_guess() {
        let files = [(
            "main.py",
            "class A:\n    def run(self):\n        pass\n\nclass B(A):\n    def run(self):\n        super().run()\n\nclass C:\n    def run(self):\n        pass\n",
        )];
        let v = run(&files, true);
        let from_b = calls(&v)
            .into_iter()
            .filter(|(s, t, _)| s == ".run()" && t == ".run()")
            .collect::<Vec<_>>();
        // Only the base-class edge survives; labels are shared, so check ids.
        let ids = v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls")
            .map(|e| (e["source"].as_str().unwrap(), e["target"].as_str().unwrap()))
            .collect::<Vec<_>>();
        assert!(ids.contains(&("main_b_run", "main_a_run")), "{ids:?}");
        assert!(!ids.contains(&("main_b_run", "main_c_run")), "{ids:?}");
        assert!(!from_b.is_empty());
    }

    #[test]
    fn values_flow_through_parameters_defaults_and_returns() {
        let files = [(
            "main.py",
            "def a():\n    pass\n\ndef b():\n    pass\n\ndef pick():\n    return b\n\n\
             def ident(x):\n    return x\n\n\
             def run(cb, other=a):\n    cb()\n    other()\n\n\
             def main():\n    run(pick())\n    run(cb=ident(a))\n    alias = run\n    alias(b)\n",
        )];
        let v = run(&files, true);
        assert!(has(&v, "run()", "b()"));
        assert!(has(&v, "run()", "a()"));
        // `ident` passes its argument through per call site.
        assert!(!has(&v, "main()", "b()"));
        assert!(has(&v, "main()", "ident()"));
    }

    #[test]
    fn python_dispatch_uses_module_globals_past_enclosing_locals() {
        let v = run(
            &[(
                "main.py",
                "def intended():\n    pass\ndef shadow():\n    pass\ncallback = intended\ndef outer():\n    callback = shadow\n    def inner():\n        global callback\n        callback()\n    return inner\ndef entry():\n    outer()()\n",
            )],
            true,
        );
        assert!(has(&v, "inner()", "intended()"), "{:?}", calls(&v));
        assert!(!has(&v, "inner()", "shadow()"), "{:?}", calls(&v));
    }

    #[test]
    fn python_dispatch_uses_c3_method_order_for_a_diamond() {
        let v = run(
            &[(
                "main.py",
                "class A:\n    def run(self):\n        pass\nclass B(A):\n    pass\nclass C(A):\n    def run(self):\n        pass\nclass D(B, C):\n    def invoke(self):\n        super().run()\n",
            )],
            true,
        );
        let targets = v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["source"] == "main_d_invoke" && e["relation"] == "calls")
            .map(|e| e["target"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(targets, ["main_c_run"]);
    }

    #[test]
    fn reassigned_names_and_unknown_callers_stay_quiet() {
        let files = [(
            "main.py",
            "import os\n\ndef g():\n    pass\n\ndef f(cb):\n    cb = os.getcwd\n    cb()\n\ndef h():\n    f(g)\n",
        )];
        let v = run(&files, true);
        // Flow-insensitive: the parameter still holds `g`.
        assert!(has(&v, "f()", "g()"));
        // Calls of names with no known value add nothing.
        assert!(
            precise_edges(&v)
                .iter()
                .all(|(s, t, _)| s == "f()" && t == "g()")
        );
    }

    #[test]
    fn instances_fields_and_dynamic_dispatch() {
        let files = [(
            "main.py",
            "class Base:\n    def go(self):\n        self.step()\n    def step(self):\n        pass\n\n\
             class Child(Base):\n    def __init__(self):\n        self.cb = self.done\n    def step(self):\n        self.cb()\n    def done(self):\n        pass\n\n\
             class Holder:\n    def __init__(self, item):\n        self.item = item\n    def use(self):\n        self.item.go()\n\n\
             def main():\n    h = Holder(Child())\n    h.use()\n",
        )];
        let v = run(&files, true);
        let ids = v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls")
            .map(|e| (e["source"].as_str().unwrap(), e["target"].as_str().unwrap()))
            .collect::<Vec<_>>();
        assert!(
            ids.contains(&("main_holder_use", "main_base_go")),
            "{ids:?}"
        );
        assert!(
            ids.contains(&("main_base_go", "main_child_step")),
            "{ids:?}"
        );
        assert!(
            ids.contains(&("main_child_step", "main_child_done")),
            "{ids:?}"
        );
    }

    #[test]
    fn decorators_imports_and_containers() {
        let files = [
            (
                "lib.py",
                "def wrap(f):\n    def inner():\n        f()\n    return inner\n\ndef keep(f):\n    return f\n\ndef run_all(fs):\n    for f in fs:\n        f()\n",
            ),
            (
                "main.py",
                "from lib import wrap, keep, run_all\nimport lib as L\n\n\
                 @wrap\ndef task():\n    pass\n\n@keep\ndef other():\n    pass\n\n\
                 def main():\n    task()\n    other()\n    run_all([other])\n    table = {'x': L.keep}\n    table['x'](other)\n",
            ),
        ];
        let v = run(&files, true);
        assert!(has(&v, "main()", "inner()"), "{:?}", calls(&v));
        assert!(has(&v, "inner()", "task()"));
        assert!(has(&v, "run_all()", "other()"));
        assert!(has(&v, "main()", "keep()"));
    }

    #[test]
    fn deeply_nested_generated_code_is_bounded() {
        // A long concatenation parses as a left-deep tree thousands of levels
        // down; the builder must stop at MAX_DEPTH within its worker stack
        // (in debug builds too) and still see the rest of the file.
        let dir = tempfile::tempdir().unwrap();
        let terms = vec!["'x'"; 6000].join(" + ");
        let path = write(
            dir.path(),
            "gen.py",
            &format!("def f(cb):\n    s = {terms}\n    cb()\n\nf(print)\n"),
        );
        // `build_all` runs the builders on their own big-stack workers, so the
        // caller's (here: the test thread's) stack size does not matter.
        let irs = super::build_all(&[(path, "gen.py".into())], &super::Facts::default());
        // The deep expression is cut off; the call after it is still seen.
        assert!(irs[0].as_ref().is_some_and(|ir| !ir.sites.is_empty()));
    }

    #[test]
    fn ambiguous_sites_add_no_edges() {
        let mut body = String::new();
        for i in 0..(super::MAX_TARGETS + 1) {
            body.push_str(&format!("def f{i}():\n    pass\n\n"));
        }
        let names = (0..=super::MAX_TARGETS)
            .map(|i| format!("f{i}"))
            .collect::<Vec<_>>();
        body.push_str(&format!(
            "HANDLERS = [{}]\n\ndef main(k):\n    HANDLERS[k]()\n",
            names.join(", ")
        ));
        let v = run(&[("main.py", &body)], true);
        assert!(precise_edges(&v).is_empty(), "{:?}", precise_edges(&v));
    }
}
