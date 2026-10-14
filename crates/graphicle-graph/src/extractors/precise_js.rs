//! Opt-in (`--precise`) JavaScript/TypeScript call resolution with oxc.
//!
//! Graphify binds JS/TS calls by name: imports through its own path rules and
//! member calls by method name alone. This pass parses every JS/TS file with
//! oxc and binds each call's callee through oxc_semantic's scopes: local
//! functions and function-valued constants, named, renamed and default
//! imports, namespace imports (`ns.f()`), `this.m()` and `super.m()` inside
//! classes (with inherited methods), static calls (`Class.m()`), receivers
//! whose class is known from `new Class()` or a type annotation, and
//! `new Class()`. ES module re-exports (`export { a as b } from`,
//! `export * from`, re-exported imports) are followed across files, and module
//! specifiers are resolved with oxc_resolver: tsconfig `paths`/`references`,
//! package.json `exports`/`main`, extension aliases and workspace symlinks.
//!
//! Only callers that are graph functions or methods get edges, and a node pair
//! that already has an edge gets no second one (the graph keeps one edge per
//! pair). Added edges carry `resolution`: `oxc_binding` for calls bound by
//! scope and module resolution (EXTRACTED), `oxc_type` for receivers typed by
//! an annotation or `new` (INFERRED).
use super::resolution::source_key;
use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind,
    ast::{
        BindingPattern, Declaration, ExportDefaultDeclarationKind, Expression, IdentifierReference,
        PropertyKey, TSType, TSTypeName,
    },
};
use oxc_parser::Parser;
use oxc_semantic::{NodeId, Semantic, SemanticBuilder};
use oxc_span::SourceType;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

/// file key -> name -> [(line, node id)].
type ByName<'g> = FxHashMap<String, FxHashMap<&'g str, Vec<(usize, &'g str)>>>;
const EXTENSIONS: &[&str] = &["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"];
/// Limit on re-export and inheritance chains.
const MAX_HOPS: usize = 16;
/// A declaration and its graph node may start a few lines apart (decorators,
/// modifiers, multi-line signatures).
const LINE_SLACK: usize = 3;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn is_js(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e))
        && !path.to_string_lossy().ends_with(".d.ts")
}

/// A declaration: `Local` in the file being read, or an imported binding.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Ref {
    Local { name: String, line: usize },
    Import { spec: String, name: String },
}

#[derive(Clone, Debug)]
enum Target {
    /// `f()`
    Function(Ref),
    /// `ns.f()` for `import * as ns`
    Namespace { spec: String, name: String },
    /// `C.m()` (static) or `x.m()` where `x` is an instance of `C` (`typed`)
    Member {
        class: Ref,
        name: String,
        typed: bool,
    },
    /// `this.m()` / `super.m()`: method `name` of `class` or its bases
    Method { class: Ref, name: String },
    /// `new C()`
    New(Ref),
}

#[derive(Clone, Debug)]
enum Caller {
    Function {
        name: String,
        line: usize,
    },
    Method {
        class: String,
        class_line: usize,
        name: String,
    },
    /// A variable whose initializer holds the call.
    Value {
        name: String,
        line: usize,
    },
}

#[derive(Debug)]
struct Site {
    /// Candidate callers, innermost first (see `Reader::callers`).
    callers: Vec<Caller>,
    line: usize,
    target: Target,
}

#[derive(Debug)]
enum Export {
    Local(Ref),
    From { spec: String, name: String },
}

#[derive(Debug)]
struct ClassInfo {
    name: String,
    line: usize,
    base: Option<Ref>,
}

#[derive(Debug, Default)]
struct Summary {
    key: String,
    dir: PathBuf,
    exports: FxHashMap<String, Export>,
    stars: Vec<String>,
    classes: Vec<ClassInfo>,
    sites: Vec<Site>,
}

/// What a symbol is bound to, as far as calls are concerned.
enum Binding {
    Decl(Ref),
    Class(Ref),
    Namespace(String),
    /// An instance of a class (`new C()` or a `C` annotation).
    Instance(Ref, bool),
}

struct Reader<'s, 'a> {
    semantic: &'s Semantic<'a>,
    lines: Vec<u32>,
}

impl<'a> Reader<'_, 'a> {
    fn line(&self, offset: u32) -> usize {
        self.lines.partition_point(|&start| start <= offset)
    }
    fn kind(&self, id: NodeId) -> AstKind<'a> {
        self.semantic.nodes().kind(id)
    }
    fn parent(&self, id: NodeId) -> AstKind<'a> {
        self.semantic.nodes().parent_kind(id)
    }
    /// The declaration node of the symbol `ident` refers to.
    fn declaration(&self, ident: &IdentifierReference<'a>) -> Option<NodeId> {
        let scoping = self.semantic.scoping();
        let symbol = scoping
            .get_reference(ident.reference_id.get()?)
            .symbol_id()?;
        Some(scoping.symbol_declaration(symbol))
    }
    fn import_spec(&self, specifier: NodeId) -> Option<String> {
        match self.parent(specifier) {
            AstKind::ImportDeclaration(decl) => Some(decl.source.value.to_string()),
            _ => None,
        }
    }
    fn binding(&self, ident: &IdentifierReference<'a>, depth: usize) -> Option<Binding> {
        if depth > 4 {
            return None;
        }
        let decl = self.declaration(ident)?;
        Some(match self.kind(decl) {
            AstKind::Function(f) => Binding::Decl(Ref::Local {
                name: f.id.as_ref()?.name.to_string(),
                line: self.line(f.span.start),
            }),
            AstKind::Class(c) => Binding::Class(Ref::Local {
                name: c.id.as_ref()?.name.to_string(),
                line: self.line(c.span.start),
            }),
            AstKind::ImportSpecifier(spec) => {
                let r = Ref::Import {
                    spec: self.import_spec(decl)?,
                    name: spec.imported.name().to_string(),
                };
                Binding::Decl(r)
            }
            AstKind::ImportDefaultSpecifier(_) => Binding::Decl(Ref::Import {
                spec: self.import_spec(decl)?,
                name: "default".into(),
            }),
            AstKind::ImportNamespaceSpecifier(_) => Binding::Namespace(self.import_spec(decl)?),
            AstKind::VariableDeclarator(d) => {
                let name = d.id.get_binding_identifier()?.name.to_string();
                let init = d.init.as_ref().map(Expression::get_inner_expression);
                match init {
                    Some(e) if e.is_function() => Binding::Decl(Ref::Local {
                        name,
                        line: self.line(d.span.start),
                    }),
                    Some(Expression::NewExpression(new)) => {
                        Binding::Instance(self.class_ref(&new.callee)?, false)
                    }
                    Some(Expression::Identifier(other)) if d.type_annotation.is_none() => {
                        self.binding(other, depth + 1)?
                    }
                    _ => Binding::Instance(
                        self.annotated(&d.type_annotation.as_ref()?.type_annotation)?,
                        true,
                    ),
                }
            }
            AstKind::FormalParameter(p) => Binding::Instance(
                self.annotated(&p.type_annotation.as_ref()?.type_annotation)?,
                true,
            ),
            _ => return None,
        })
    }
    /// The class a `new` callee or type name names.
    fn class_ref(&self, callee: &Expression<'a>) -> Option<Ref> {
        match callee.get_inner_expression() {
            Expression::Identifier(ident) => self.class_of(ident),
            _ => None,
        }
    }
    fn class_of(&self, ident: &IdentifierReference<'a>) -> Option<Ref> {
        match self.binding(ident, 0)? {
            Binding::Class(r) | Binding::Decl(r @ Ref::Import { .. }) => Some(r),
            _ => None,
        }
    }
    /// `C`, `C | null`, `C | undefined` as a class reference.
    fn annotated(&self, ty: &TSType<'a>) -> Option<Ref> {
        match ty {
            TSType::TSTypeReference(r) => match &r.type_name {
                TSTypeName::IdentifierReference(ident) => self.class_of(ident),
                _ => None,
            },
            TSType::TSUnionType(u) => {
                let mut found = None;
                for t in &u.types {
                    match t {
                        TSType::TSNullKeyword(_) | TSType::TSUndefinedKeyword(_) => {}
                        other => {
                            if found.is_some() {
                                return None;
                            }
                            found = Some(self.annotated(other)?);
                        }
                    }
                }
                found
            }
            TSType::TSParenthesizedType(p) => self.annotated(&p.type_annotation),
            _ => None,
        }
    }
    /// The class declaration enclosing `node` through methods, properties and
    /// arrow functions (whose `this` is the instance), if any.
    fn enclosing_class(&self, node: NodeId) -> Option<(NodeId, &'a oxc_ast::ast::Class<'a>)> {
        let nodes = self.semantic.nodes();
        let mut in_method = false;
        for id in nodes.ancestor_ids(node) {
            match nodes.kind(id) {
                AstKind::Function(_) => {
                    if !matches!(self.parent(id), AstKind::MethodDefinition(_)) {
                        return None;
                    }
                    in_method = true;
                }
                AstKind::MethodDefinition(_) | AstKind::PropertyDefinition(_) => in_method = true,
                AstKind::Class(c) => return in_method.then_some((id, c)),
                AstKind::Program(_) => return None,
                _ => {}
            }
        }
        None
    }
    fn class_local(&self, c: &oxc_ast::ast::Class<'a>) -> Option<Ref> {
        Some(Ref::Local {
            name: c.id.as_ref()?.name.to_string(),
            line: self.line(c.span.start),
        })
    }
    /// Enclosing named functions, methods and variables, innermost first:
    /// the call belongs to the first one that is a graph node (a local
    /// callback or variable usually is not, so the call falls to the function
    /// around it, as Graphify attributes it). Module-level variables cover
    /// calls in initializers (`export const a = register({ perform() {} })`).
    fn callers(&self, node: NodeId) -> Vec<Caller> {
        let nodes = self.semantic.nodes();
        let mut out = vec![];
        for id in nodes.ancestor_ids(node) {
            let kind = nodes.kind(id);
            let is_fn = matches!(
                kind,
                AstKind::Function(_) | AstKind::ArrowFunctionExpression(_)
            );
            if !is_fn {
                match kind {
                    AstKind::Program(_) => break,
                    AstKind::VariableDeclarator(d) => {
                        // A function-valued declarator was taken with its function.
                        let fn_init = d
                            .init
                            .as_ref()
                            .is_some_and(|e| e.get_inner_expression().is_function());
                        if !fn_init && let Some(b) = d.id.get_binding_identifier() {
                            out.push(Caller::Value {
                                name: b.name.to_string(),
                                line: self.line(d.span.start),
                            });
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match self.parent(id) {
                AstKind::MethodDefinition(m) => {
                    out.extend(self.method_caller(nodes.parent_id(id), &m.key));
                    continue;
                }
                AstKind::PropertyDefinition(p) => {
                    out.extend(self.method_caller(nodes.parent_id(id), &p.key));
                    continue;
                }
                AstKind::VariableDeclarator(d) => {
                    if let Some(b) = d.id.get_binding_identifier() {
                        out.push(Caller::Function {
                            name: b.name.to_string(),
                            line: self.line(d.span.start),
                        });
                        continue;
                    }
                }
                _ => {}
            }
            if let AstKind::Function(f) = kind
                && let Some(name) = &f.id
            {
                out.push(Caller::Function {
                    name: name.name.to_string(),
                    line: self.line(f.span.start),
                });
            }
        }
        out
    }
    fn method_caller(&self, member: NodeId, key: &PropertyKey<'a>) -> Option<Caller> {
        let name = key.static_name()?.to_string();
        let nodes = self.semantic.nodes();
        for id in nodes.ancestor_ids(member) {
            if let AstKind::Class(c) = nodes.kind(id) {
                let class = c.id.as_ref()?;
                return Some(Caller::Method {
                    class: class.name.to_string(),
                    class_line: self.line(c.span.start),
                    name,
                });
            }
        }
        None
    }
    /// The class of `this.field` from the field's annotation or `new` value.
    fn field_class(&self, class: NodeId, field: &str) -> Option<(Ref, bool)> {
        let AstKind::Class(c) = self.kind(class) else {
            return None;
        };
        for element in &c.body.body {
            if let oxc_ast::ast::ClassElement::PropertyDefinition(p) = element
                && p.key.static_name().is_some_and(|n| n == field)
            {
                if let Some(Expression::NewExpression(new)) =
                    p.value.as_ref().map(Expression::get_inner_expression)
                {
                    return Some((self.class_ref(&new.callee)?, false));
                }
                return Some((
                    self.annotated(&p.type_annotation.as_ref()?.type_annotation)?,
                    true,
                ));
            }
        }
        None
    }
    fn target(&self, node: NodeId, callee: &Expression<'a>) -> Option<Target> {
        match callee.get_inner_expression() {
            Expression::Identifier(ident) => match self.binding(ident, 0)? {
                Binding::Decl(r) => Some(Target::Function(r)),
                _ => None,
            },
            Expression::StaticMemberExpression(member) => {
                let name = member.property.name.to_string();
                match member.object.get_inner_expression() {
                    Expression::ThisExpression(_) => {
                        let (_, class) = self.enclosing_class(node)?;
                        Some(Target::Method {
                            class: self.class_local(class)?,
                            name,
                        })
                    }
                    Expression::Super(_) => {
                        let (_, class) = self.enclosing_class(node)?;
                        let base = self.class_ref(class.super_class.as_ref()?)?;
                        Some(Target::Method { class: base, name })
                    }
                    Expression::Identifier(ident) => match self.binding(ident, 0)? {
                        Binding::Namespace(spec) => Some(Target::Namespace { spec, name }),
                        Binding::Class(class) => Some(Target::Member {
                            class,
                            name,
                            typed: false,
                        }),
                        Binding::Decl(r @ Ref::Import { .. }) => Some(Target::Member {
                            class: r,
                            name,
                            typed: false,
                        }),
                        Binding::Instance(class, typed) => {
                            Some(Target::Member { class, name, typed })
                        }
                        Binding::Decl(_) => None,
                    },
                    Expression::StaticMemberExpression(inner)
                        if matches!(
                            inner.object.get_inner_expression(),
                            Expression::ThisExpression(_)
                        ) =>
                    {
                        let (class_node, _) = self.enclosing_class(node)?;
                        let (class, typed) = self.field_class(class_node, &inner.property.name)?;
                        Some(Target::Member { class, name, typed })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
    fn summarize(&self, key: String, dir: PathBuf) -> Summary {
        let mut out = Summary {
            key,
            dir,
            ..Summary::default()
        };
        for node in self.semantic.nodes().iter() {
            match node.kind() {
                AstKind::CallExpression(call) => {
                    let callers = self.callers(node.id());
                    if !callers.is_empty()
                        && let Some(target) = self.target(node.id(), &call.callee)
                    {
                        out.sites.push(Site {
                            callers,
                            line: self.line(call.span.start),
                            target,
                        });
                    }
                }
                AstKind::NewExpression(new) => {
                    let callers = self.callers(node.id());
                    if !callers.is_empty()
                        && let Some(class) = self.class_ref(&new.callee)
                    {
                        out.sites.push(Site {
                            callers,
                            line: self.line(new.span.start),
                            target: Target::New(class),
                        });
                    }
                }
                AstKind::Class(c) => {
                    if let Some(id) = &c.id {
                        out.classes.push(ClassInfo {
                            name: id.name.to_string(),
                            line: self.line(c.span.start),
                            base: c.super_class.as_ref().and_then(|e| self.class_ref(e)),
                        });
                    }
                }
                AstKind::ExportNamedDeclaration(e) => {
                    if let Some(source) = &e.source {
                        for spec in &e.specifiers {
                            out.exports.insert(
                                spec.exported.name().to_string(),
                                Export::From {
                                    spec: source.value.to_string(),
                                    name: spec.local.name().to_string(),
                                },
                            );
                        }
                        continue;
                    }
                    for spec in &e.specifiers {
                        let exported = spec.exported.name().to_string();
                        if let oxc_ast::ast::ModuleExportName::IdentifierReference(ident) =
                            &spec.local
                            && let Some(r) = self.export_ref(ident)
                        {
                            out.exports.insert(exported, Export::Local(r));
                        }
                    }
                    match &e.declaration {
                        Some(Declaration::FunctionDeclaration(f)) => {
                            if let Some(id) = &f.id {
                                out.exports.insert(
                                    id.name.to_string(),
                                    Export::Local(Ref::Local {
                                        name: id.name.to_string(),
                                        line: self.line(f.span.start),
                                    }),
                                );
                            }
                        }
                        Some(Declaration::ClassDeclaration(c)) => {
                            if let (Some(id), Some(r)) = (&c.id, self.class_local(c)) {
                                out.exports.insert(id.name.to_string(), Export::Local(r));
                            }
                        }
                        Some(Declaration::VariableDeclaration(v)) => {
                            for d in &v.declarations {
                                if let BindingPattern::BindingIdentifier(b) = &d.id {
                                    out.exports.insert(
                                        b.name.to_string(),
                                        Export::Local(Ref::Local {
                                            name: b.name.to_string(),
                                            line: self.line(d.span.start),
                                        }),
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                AstKind::ExportDefaultDeclaration(e) => {
                    let r = match &e.declaration {
                        ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                            f.id.as_ref().map(|id| Ref::Local {
                                name: id.name.to_string(),
                                line: self.line(f.span.start),
                            })
                        }
                        ExportDefaultDeclarationKind::ClassDeclaration(c) => self.class_local(c),
                        ExportDefaultDeclarationKind::Identifier(ident) => self.export_ref(ident),
                        _ => None,
                    };
                    if let Some(r) = r {
                        out.exports.insert("default".into(), Export::Local(r));
                    }
                }
                AstKind::ExportAllDeclaration(e) if e.exported.is_none() => {
                    out.stars.push(e.source.value.to_string());
                }
                _ => {}
            }
        }
        out
    }
    fn export_ref(&self, ident: &IdentifierReference<'a>) -> Option<Ref> {
        match self.binding(ident, 0)? {
            Binding::Decl(r) | Binding::Class(r) => Some(r),
            _ => None,
        }
    }
}

fn summarize_file(path: &Path, key: String) -> Option<Summary> {
    let source = std::fs::read_to_string(path).ok()?;
    let mut source_type = SourceType::from_path(path).ok()?;
    // JSX is common in .js sources; it is only ambiguous in TypeScript.
    if source_type.is_javascript() {
        source_type = source_type.with_jsx(true);
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &source, source_type).parse();
    if parsed.panicked {
        return None;
    }
    let semantic = SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&parsed.program)
        .semantic;
    let mut lines = vec![0u32];
    lines.extend(
        source
            .bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i as u32 + 1),
    );
    let reader = Reader {
        semantic: &semantic,
        lines,
    };
    Some(reader.summarize(key, path.parent()?.to_path_buf()))
}

fn summarize_all(files: &[(PathBuf, String)]) -> Vec<Summary> {
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(files.len().div_ceil(8).max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots = Mutex::new(Vec::with_capacity(files.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((path, key)) = files.get(i) else {
                        break;
                    };
                    // Very deep sources can exhaust a worker's stack in the
                    // parser; a failed file only loses its own edges.
                    let summary = std::panic::catch_unwind(|| summarize_file(path, key.clone()))
                        .ok()
                        .flatten();
                    if let Some(summary) = summary {
                        slots
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((i, summary));
                    }
                }
            });
        }
    });
    let mut slots = slots.into_inner().unwrap_or_else(|e| e.into_inner());
    slots.sort_unstable_by_key(|(i, _)| *i);
    slots.into_iter().map(|(_, s)| s).collect()
}

fn resolver(tsconfig: Option<PathBuf>) -> oxc_resolver::Resolver {
    use oxc_resolver::{ResolveOptions, TsconfigDiscovery, TsconfigOptions, TsconfigReferences};
    let strings = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    oxc_resolver::Resolver::new(ResolveOptions {
        extensions: strings(&[
            ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json",
        ]),
        extension_alias: vec![
            (".js".into(), strings(&[".ts", ".tsx", ".js", ".jsx"])),
            (".jsx".into(), strings(&[".tsx", ".jsx"])),
            (".mjs".into(), strings(&[".mts", ".mjs"])),
            (".cjs".into(), strings(&[".cts", ".cjs"])),
        ],
        condition_names: strings(&["source", "import", "module", "require", "node", "default"]),
        main_fields: strings(&["source", "module", "main"]),
        tsconfig: Some(match tsconfig {
            Some(config_file) => TsconfigDiscovery::Manual(TsconfigOptions {
                config_file,
                references: TsconfigReferences::Auto,
            }),
            None => TsconfigDiscovery::Auto,
        }),
        symlinks: true,
        ..ResolveOptions::default()
    })
}

/// Graph nodes by file and name, and methods by class.
struct Index<'g> {
    functions: ByName<'g>,
    classes: ByName<'g>,
    methods: FxHashMap<(&'g str, &'g str), &'g str>,
}

fn node_line(n: &Value) -> Option<usize> {
    s(n, "source_location").strip_prefix('L')?.parse().ok()
}

impl<'g> Index<'g> {
    fn build(nodes: &[&'g Value], edges: &[&'g Value], root: &Path) -> Self {
        let mut index = Self {
            functions: FxHashMap::default(),
            classes: FxHashMap::default(),
            methods: FxHashMap::default(),
        };
        let mut labels = FxHashMap::default();
        let mut keys: FxHashMap<&str, String> = FxHashMap::default();
        for n in nodes {
            let (id, label, file) = (s(n, "id"), s(n, "label"), s(n, "source_file"));
            if id.is_empty() || file.is_empty() || !is_js(Path::new(file)) {
                continue;
            }
            labels.insert(id, label);
            let Some(line) = node_line(n) else { continue };
            let key = keys
                .entry(file)
                .or_insert_with(|| source_key(file, root))
                .clone();
            if let Some(name) = label.strip_suffix("()") {
                if !name.starts_with('.') {
                    index
                        .functions
                        .entry(key)
                        .or_default()
                        .entry(name)
                        .or_default()
                        .push((line, id));
                }
            } else if !label.contains('.') && !label.is_empty() {
                index
                    .classes
                    .entry(key)
                    .or_default()
                    .entry(label)
                    .or_default()
                    .push((line, id));
            }
        }
        for e in edges {
            if s(e, "relation") == "method"
                && let Some(label) = labels.get(s(e, "target"))
                && let Some(name) = label.strip_prefix('.').and_then(|l| l.strip_suffix("()"))
            {
                index.methods.insert((s(e, "source"), name), s(e, "target"));
            }
        }
        index
    }
    fn closest(candidates: Option<&Vec<(usize, &'g str)>>, line: usize) -> Option<&'g str> {
        candidates?
            .iter()
            .filter(|(l, _)| l.abs_diff(line) <= LINE_SLACK)
            .min_by_key(|(l, id)| (l.abs_diff(line), *id))
            .map(|(_, id)| *id)
    }
    fn function(&self, file: &str, name: &str, line: usize) -> Option<&'g str> {
        Self::closest(self.functions.get(file)?.get(name), line)
    }
    fn class(&self, file: &str, name: &str, line: usize) -> Option<&'g str> {
        Self::closest(self.classes.get(file)?.get(name), line)
    }
}

struct Resolve<'a> {
    summaries: &'a [Summary],
    by_path: FxHashMap<PathBuf, usize>,
    resolver: oxc_resolver::Resolver,
    /// The scan root's tsconfig.json, for files whose nearest tsconfig does
    /// not carry the project's `paths` (what `tsc -p <root>` sees).
    root_resolver: Option<oxc_resolver::Resolver>,
    modules: FxHashMap<(usize, String), Option<usize>>,
}

impl Resolve<'_> {
    fn module(&mut self, from: usize, spec: &str) -> Option<usize> {
        let key = (from, spec.to_string());
        if let Some(hit) = self.modules.get(&key) {
            return *hit;
        }
        let dir = &self.summaries[from].dir;
        let found = std::iter::once(&self.resolver)
            .chain(self.root_resolver.as_ref())
            .find_map(|resolver| {
                let resolved = resolver.resolve(dir, spec).ok()?;
                let path = resolved.path();
                let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                self.by_path.get(&path).copied()
            });
        self.modules.insert(key, found);
        found
    }
    /// The local declaration `name` exported from `file` resolves to.
    fn export(&mut self, file: usize, name: &str, hops: usize) -> Option<(usize, Ref)> {
        if hops > MAX_HOPS {
            return None;
        }
        let summaries = self.summaries;
        match summaries[file].exports.get(name) {
            Some(Export::Local(r @ Ref::Local { .. })) => Some((file, r.clone())),
            Some(Export::Local(Ref::Import { spec, name })) | Some(Export::From { spec, name }) => {
                let target = self.module(file, spec)?;
                self.export(target, name, hops + 1)
            }
            None if name != "default" => {
                for star in &summaries[file].stars {
                    if let Some(target) = self.module(file, star)
                        && let Some(found) = self.export(target, name, hops + 1)
                    {
                        return Some(found);
                    }
                }
                None
            }
            None => None,
        }
    }
    fn declaration(&mut self, file: usize, r: &Ref) -> Option<(usize, Ref)> {
        match r {
            Ref::Local { .. } => Some((file, r.clone())),
            Ref::Import { spec, name } => {
                let target = self.module(file, spec)?;
                self.export(target, name, 0)
            }
        }
    }
}

fn edge(src: &str, tgt: &str, line: usize, file: &str, resolution: &str) -> Value {
    let exact = resolution == "oxc_binding";
    json!({"source": src, "target": tgt, "relation": "calls", "context": "call",
        "confidence": if exact { "EXTRACTED" } else { "INFERRED" },
        "confidence_score": if exact { 1.0 } else { 0.85 }, "source_file": file,
        "source_location": format!("L{line}"), "weight": 1.0, "_origin": "ast",
        "resolution": resolution})
}

/// Add precise `calls` edges for the JS/TS files among `paths`. `context_*` is
/// the rest of the graph when only some files are being re-extracted; its
/// JS/TS files are read for imports and classes, but only callers in `paths`
/// get edges.
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
    let mut files: Vec<(PathBuf, String)> = vec![];
    let mut seen = FxHashSet::default();
    for p in paths.iter().filter(|p| is_js(p)) {
        let key = source_key(&p.to_string_lossy(), root);
        if seen.insert(key.clone()) {
            files.push((p.clone(), key));
        }
    }
    if files.is_empty() {
        return;
    }
    let emit = seen.clone();
    for n in context_nodes {
        let sf = s(n, "source_file");
        if is_js(Path::new(sf)) && Path::new(sf).file_name() == Path::new(s(n, "label")).file_name()
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
    let summaries = summarize_all(&files);
    let parsed = started.elapsed();
    let all_nodes = nodes.iter().chain(context_nodes).collect::<Vec<_>>();
    let all_edges = edges.iter().chain(context_edges).collect::<Vec<_>>();
    let index = Index::build(&all_nodes, &all_edges, root);
    let mut pairs: FxHashSet<(&str, &str)> = FxHashSet::default();
    for e in &all_edges {
        let (a, b) = (s(e, "source"), s(e, "target"));
        pairs.insert(if a <= b { (a, b) } else { (b, a) });
    }
    // Summaries skip files that failed to parse; map paths to summaries by key.
    let key_index = summaries
        .iter()
        .enumerate()
        .map(|(i, s)| (s.key.as_str(), i))
        .collect::<FxHashMap<_, _>>();
    let by_path = files
        .iter()
        .filter_map(|(p, key)| {
            let i = *key_index.get(key.as_str())?;
            Some((p.canonicalize().unwrap_or_else(|_| p.clone()), i))
        })
        .collect();
    let mut r = Resolve {
        summaries: &summaries,
        by_path,
        resolver: resolver(None),
        root_resolver: Some(root.join("tsconfig.json"))
            .filter(|p| p.is_file())
            .map(|p| resolver(Some(p))),
        modules: FxHashMap::default(),
    };
    let class_node =
        |file: usize, reference: &Ref, r: &mut Resolve| -> Option<(usize, String, usize)> {
            let (file, Ref::Local { name, line }) = r.declaration(file, reference)? else {
                return None;
            };
            Some((file, name, line))
        };
    let mut added = vec![];
    for (fi, summary) in summaries.iter().enumerate() {
        if !emit.contains(&summary.key) {
            continue;
        }
        for site in &summary.sites {
            let caller = site.callers.iter().find_map(|c| match c {
                Caller::Function { name, line } => index.function(&summary.key, name, *line),
                Caller::Method {
                    class,
                    class_line,
                    name,
                } => index
                    .class(&summary.key, class, *class_line)
                    .and_then(|c| index.methods.get(&(c, name.as_str())).copied()),
                Caller::Value { name, line } => index.class(&summary.key, name, *line),
            });
            let Some(caller) = caller else { continue };
            let (target, resolution) = match &site.target {
                Target::Function(reference) => {
                    let Some((file, Ref::Local { name, line })) = r.declaration(fi, reference)
                    else {
                        continue;
                    };
                    (
                        index.function(&summaries[file].key, &name, line),
                        "oxc_binding",
                    )
                }
                Target::Namespace { spec, name } => {
                    let Some(module) = r.module(fi, spec) else {
                        continue;
                    };
                    let Some((file, Ref::Local { name, line })) = r.export(module, name, 0) else {
                        continue;
                    };
                    (
                        index.function(&summaries[file].key, &name, line),
                        "oxc_binding",
                    )
                }
                Target::New(class) => {
                    let Some((file, name, line)) = class_node(fi, class, &mut r) else {
                        continue;
                    };
                    (
                        index.class(&summaries[file].key, &name, line),
                        "oxc_binding",
                    )
                }
                Target::Member { class, name, typed } => (
                    method(&index, &mut r, fi, class, name),
                    if *typed { "oxc_type" } else { "oxc_binding" },
                ),
                Target::Method { class, name } => {
                    (method(&index, &mut r, fi, class, name), "oxc_binding")
                }
            };
            let Some(target) = target else { continue };
            if target == caller {
                continue;
            }
            let pair = if caller <= target {
                (caller, target)
            } else {
                (target, caller)
            };
            if !pairs.insert(pair) {
                continue;
            }
            added.push(edge(caller, target, site.line, &summary.key, resolution));
        }
    }
    if timing {
        eprintln!(
            "[precise js] files={} parse+semantic={:.0}ms total={:.0}ms added={}",
            summaries.len(),
            parsed.as_secs_f64() * 1e3,
            started.elapsed().as_secs_f64() * 1e3,
            added.len()
        );
    }
    edges.extend(added);
}

/// Method `name` of the class `reference` (from file `file`) or its bases.
fn method<'g>(
    index: &Index<'g>,
    r: &mut Resolve,
    file: usize,
    reference: &Ref,
    name: &str,
) -> Option<&'g str> {
    let (mut file, mut class) = r.declaration(file, reference)?;
    for _ in 0..MAX_HOPS {
        let Ref::Local {
            name: class_name,
            line,
        } = &class
        else {
            return None;
        };
        let summary = &r.summaries[file];
        let node = index.class(&summary.key, class_name, *line)?;
        if let Some(m) = index.methods.get(&(node, name)) {
            return Some(m);
        }
        let base = summary
            .classes
            .iter()
            .find(|c| &c.name == class_name && c.line == *line)?
            .base
            .clone()?;
        (file, class) = r.declaration(file, &base)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::extract::{ExtractOptions, extract};
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn calls(result: &Value) -> Vec<(String, String, String)> {
        let label = |id: &str| {
            result["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["id"] == id)
                .map(|n| n["label"].as_str().unwrap().to_string())
                .unwrap_or_default()
        };
        let mut out = result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "calls")
            .map(|e| {
                (
                    label(e["source"].as_str().unwrap()),
                    label(e["target"].as_str().unwrap()),
                    e["resolution"].as_str().unwrap_or("default").to_string(),
                )
            })
            .collect::<Vec<_>>();
        out.sort();
        out
    }

    #[test]
    fn binds_calls_through_imports_classes_and_types() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::extractors::resolution::resolved(dir.path());
        let files = vec![
            write(
                &root,
                "src/lib/math.ts",
                "export function add(a: number, b: number) { return a + b; }\nexport const twice = (x: number) => add(x, x);\n",
            ),
            write(
                &root,
                "src/lib/index.ts",
                "export * from './math';\nexport { add as plus } from './math';\n",
            ),
            write(
                &root,
                "src/shapes.ts",
                "export class Shape {\n  area(): number { return 0; }\n  describe() { return this.area(); }\n}\nexport class Square extends Shape {\n  area() { return super.area() + 1; }\n  static unit() { return new Square(); }\n}\n",
            ),
            write(
                &root,
                "src/main.ts",
                "import * as lib from './lib';\nimport { plus as sum } from './lib';\nimport { Square, Shape } from './shapes';\n\nexport function run(shape: Shape) {\n  const sq = new Square();\n  sq.describe();\n  shape.area();\n  Square.unit();\n  return lib.twice(sum(1, 2));\n}\n",
            ),
        ];
        let options = ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.clone()),
            parallel: false,
            precise: true,
            ..Default::default()
        };
        // `extract` opens a process-wide parse-cache run; keep clear of the cache tests.
        let _serial = crate::extractors::parse_cache::test_serial();
        let result = extract(&files, &options).unwrap();
        let calls = calls(&result);
        let has = |a: &str, b: &str| calls.iter().any(|(s, t, _)| s == a && t == b);
        for (a, b) in [
            ("run()", "twice()"),
            ("run()", "add()"),
            ("run()", "Square"),
            ("run()", ".describe()"),
            ("run()", ".area()"),
            ("run()", ".unit()"),
            ("twice()", "add()"),
        ] {
            assert!(has(a, b), "missing {a} -> {b}: {calls:?}");
        }
        assert!(
            calls
                .iter()
                .any(|(s, t, r)| s == ".describe()" && t == ".area()" && r != "oxc_type"),
            "{calls:?}"
        );
    }
}
