//! Per-file receiver facts for the opt-in receiver-aware method resolution.
//!
//! Python, TypeScript/JavaScript and Go are re-parsed (through the run-scoped
//! parse cache) into a language-neutral shape: lexical scopes with the
//! bindings that fix a variable's type (constructor call, annotation, method
//! receiver), the member-call sites, class fields and interface names. The
//! resolver in `receiver_resolution` joins these facts with the graph.
use super::base::read_text;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

/// How a receiver's type was fixed; recorded on the edge as `receiver_evidence`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Evidence {
    Constructor,
    Annotation,
    SelfMethod,
    ModuleAttr,
}
impl Evidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Evidence::Constructor => "constructor",
            Evidence::Annotation => "annotation",
            Evidence::SelfMethod => "self_method",
            Evidence::ModuleAttr => "module_attr",
        }
    }
}
/// A type as written at the binding site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeRef {
    /// A (possibly qualified) type name: `Client`, `pkg.Client`, `mod.Client`.
    Name(String),
    /// The return type of the named function called on `line` (Go `x := NewFoo()`).
    Return(String, usize),
}
#[derive(Clone, Debug)]
pub struct Binding {
    pub name: String,
    /// `None` records an assignment whose type is unknown; it shadows any
    /// earlier typed binding of the same name.
    pub ty: Option<TypeRef>,
    pub evidence: Evidence,
    /// Byte offset of the binding; constructor bindings only count before the call.
    pub at: usize,
    /// Parameter and receiver bindings hold for the whole scope.
    pub whole_scope: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub start: usize,
    pub end: usize,
    /// Enclosing class/struct name when this scope is a method body.
    pub class: Option<String>,
    /// Names that denote the receiver instance inside this scope (`self`, `cls`,
    /// `this`, a Go receiver variable).
    pub self_names: Vec<String>,
    pub bindings: Vec<Binding>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Receiver {
    /// `name.method()`; `this` for TypeScript/JavaScript.
    Name(String),
    /// `base.field.method()` where `base` may be a receiver instance.
    Field { base: String, field: String },
    /// Any other expression (call result, index, literal, deeper chain).
    Other(String),
}
impl Receiver {
    pub fn text(&self) -> String {
        match self {
            Receiver::Name(n) => n.clone(),
            Receiver::Field { base, field } => format!("{base}.{field}"),
            Receiver::Other(t) => t.clone(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct CallSite {
    pub byte: usize,
    pub line: usize,
    pub callee: String,
    pub receiver: Receiver,
}
#[derive(Clone, Debug, Default)]
pub struct FileFacts {
    /// `scopes[0]` is the module scope.
    pub scopes: Vec<Scope>,
    pub calls: Vec<CallSite>,
    /// Class/struct name → field bindings (annotation or constructor).
    pub fields: BTreeMap<String, Vec<Binding>>,
    /// Names declared as interfaces (TypeScript `interface`, Go interface types).
    pub interfaces: BTreeSet<String>,
}
impl FileFacts {
    fn new(len: usize) -> Self {
        Self {
            scopes: vec![Scope {
                start: 0,
                end: len,
                ..Scope::default()
            }],
            ..Self::default()
        }
    }
    /// Scopes containing `byte`, innermost first.
    pub fn enclosing(&self, byte: usize) -> Vec<&Scope> {
        let mut v: Vec<&Scope> = self
            .scopes
            .iter()
            .filter(|s| s.start <= byte && byte < s.end.max(s.start + 1))
            .collect();
        v.sort_by_key(|s| s.end - s.start);
        v
    }
}
fn children(n: Node<'_>) -> Vec<Node<'_>> {
    let mut c = n.walk();
    n.children(&mut c).collect()
}
fn named(n: Node<'_>) -> Vec<Node<'_>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}
fn field<'a>(n: Node<'a>, name: &str) -> Option<Node<'a>> {
    n.child_by_field_name(name)
}
fn line(n: Node<'_>) -> usize {
    n.start_position().row + 1
}
fn push_binding(
    scope: &mut Scope,
    name: String,
    ty: Option<TypeRef>,
    evidence: Evidence,
    at: usize,
    whole_scope: bool,
) {
    if name.is_empty() {
        return;
    }
    scope.bindings.push(Binding {
        name,
        ty,
        evidence,
        at,
        whole_scope,
    });
}

// ---------------------------------------------------------------- Python ---

fn python_type(n: Node<'_>, s: &[u8]) -> Option<TypeRef> {
    let n = if n.kind() == "type" {
        named(n).into_iter().next()?
    } else {
        n
    };
    match n.kind() {
        "identifier" | "attribute" => Some(TypeRef::Name(read_text(n, s))),
        "string" => {
            let content = named(n)
                .into_iter()
                .find(|c| c.kind() == "string_content")?;
            let t = read_text(content, s);
            let simple = t
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.');
            simple.then_some(TypeRef::Name(t))
        }
        "generic_type" => {
            let head = named(n).into_iter().next()?;
            let head = read_text(head, s);
            if !matches!(head.as_str(), "Optional" | "typing.Optional") {
                return None;
            }
            let params = named(n)
                .into_iter()
                .find(|c| c.kind() == "type_parameter")?;
            let inner = named(params);
            if inner.len() == 1 {
                python_type(inner[0], s)
            } else {
                None
            }
        }
        "binary_operator" => {
            let (l, r) = (field(n, "left")?, field(n, "right")?);
            match (l.kind(), r.kind()) {
                ("none", _) => python_type(r, s),
                (_, "none") => python_type(l, s),
                _ => None,
            }
        }
        _ => None,
    }
}
fn python_value_type(n: Node<'_>, s: &[u8]) -> Option<TypeRef> {
    if n.kind() != "call" {
        return None;
    }
    let f = field(n, "function")?;
    match f.kind() {
        "identifier" | "attribute" => Some(TypeRef::Name(read_text(f, s))),
        _ => None,
    }
}
struct PyWalker<'s> {
    s: &'s [u8],
    facts: FileFacts,
    /// Stack of scope indices (module scope at the bottom).
    stack: Vec<usize>,
    classes: Vec<String>,
}
impl PyWalker<'_> {
    fn scope(&mut self) -> &mut Scope {
        let i = *self.stack.last().unwrap();
        &mut self.facts.scopes[i]
    }
    fn self_name(&self) -> Option<String> {
        self.stack
            .iter()
            .rev()
            .find_map(|i| self.facts.scopes[*i].self_names.first().cloned())
    }
    fn receiver(&self, obj: Node<'_>) -> Receiver {
        match obj.kind() {
            "identifier" => Receiver::Name(read_text(obj, self.s)),
            "attribute" => {
                let base = field(obj, "object");
                let attr = field(obj, "attribute");
                match (base, attr) {
                    (Some(b), Some(a)) if b.kind() == "identifier" => Receiver::Field {
                        base: read_text(b, self.s),
                        field: read_text(a, self.s),
                    },
                    _ => Receiver::Other(read_text(obj, self.s)),
                }
            }
            _ => Receiver::Other(read_text(obj, self.s)),
        }
    }
    fn walk(&mut self, n: Node<'_>) {
        match n.kind() {
            "class_definition" => {
                let name = field(n, "name").map(|c| read_text(c, self.s));
                if let Some(name) = name {
                    self.classes.push(name);
                    if let Some(body) = field(n, "body") {
                        for c in children(body) {
                            self.walk(c);
                        }
                    }
                    self.classes.pop();
                }
                return;
            }
            "function_definition" => {
                let in_class = n
                    .parent()
                    .and_then(|p| p.parent())
                    .is_some_and(|p| p.kind() == "class_definition");
                let class = if in_class {
                    self.classes.last().cloned()
                } else {
                    None
                };
                let mut scope = Scope {
                    start: n.start_byte(),
                    end: n.end_byte(),
                    class: class.clone(),
                    ..Scope::default()
                };
                if let Some(params) = field(n, "parameters") {
                    let mut first = true;
                    for p in named(params) {
                        let (name, ty) = match p.kind() {
                            "identifier" => (Some(read_text(p, self.s)), None),
                            "default_parameter" => {
                                (field(p, "name").map(|c| read_text(c, self.s)), None)
                            }
                            "typed_parameter" => (
                                named(p)
                                    .into_iter()
                                    .find(|c| c.kind() == "identifier")
                                    .map(|c| read_text(c, self.s)),
                                field(p, "type").and_then(|t| python_type(t, self.s)),
                            ),
                            "typed_default_parameter" => (
                                field(p, "name").map(|c| read_text(c, self.s)),
                                field(p, "type").and_then(|t| python_type(t, self.s)),
                            ),
                            _ => (None, None),
                        };
                        let Some(name) = name else { continue };
                        if first && class.is_some() && ty.is_none() {
                            scope.self_names.push(name.clone());
                            scope.bindings.push(Binding {
                                name,
                                ty: class.clone().map(TypeRef::Name),
                                evidence: Evidence::SelfMethod,
                                at: p.start_byte(),
                                whole_scope: true,
                            });
                        } else {
                            let evidence = Evidence::Annotation;
                            scope.bindings.push(Binding {
                                name,
                                ty,
                                evidence,
                                at: p.start_byte(),
                                whole_scope: true,
                            });
                        }
                        first = false;
                    }
                }
                self.facts.scopes.push(scope);
                self.stack.push(self.facts.scopes.len() - 1);
                if let Some(body) = field(n, "body") {
                    self.walk(body);
                }
                self.stack.pop();
                return;
            }
            "assignment" => {
                let left = field(n, "left");
                let right = field(n, "right");
                let ann = field(n, "type").and_then(|t| python_type(t, self.s));
                let value = right.and_then(|r| python_value_type(r, self.s));
                let (ty, evidence) = match (ann, value) {
                    (Some(t), _) => (Some(t), Evidence::Annotation),
                    (None, Some(t)) => (Some(t), Evidence::Constructor),
                    _ => (None, Evidence::Constructor),
                };
                if let Some(left) = left {
                    match left.kind() {
                        "identifier" if self.stack.len() > 1 || self.classes.is_empty() => {
                            let at = n.start_byte();
                            let name = read_text(left, self.s);
                            push_binding(self.scope(), name, ty, evidence, at, false);
                        }
                        "attribute" => {
                            if let (Some(b), Some(a)) =
                                (field(left, "object"), field(left, "attribute"))
                                && b.kind() == "identifier"
                                && self.self_name().as_deref()
                                    == Some(read_text(b, self.s).as_str())
                                && let Some(class) = self.classes.last().cloned()
                            {
                                self.facts.fields.entry(class).or_default().push(Binding {
                                    name: read_text(a, self.s),
                                    ty,
                                    evidence,
                                    at: n.start_byte(),
                                    whole_scope: true,
                                });
                            }
                        }
                        "pattern_list" | "tuple_pattern" => {
                            for c in named(left).into_iter().filter(|c| c.kind() == "identifier") {
                                let name = read_text(c, self.s);
                                let at = n.start_byte();
                                push_binding(
                                    self.scope(),
                                    name,
                                    None,
                                    Evidence::Constructor,
                                    at,
                                    false,
                                );
                            }
                        }
                        _ => {}
                    }
                }
                if self.stack.len() == 1
                    && let Some(class) = self.classes.last().cloned()
                    && let Some(left) = left.filter(|l| l.kind() == "identifier")
                    && let Some(t) = field(n, "type").and_then(|t| python_type(t, self.s))
                {
                    // Class-level annotation `field: Type` inside a class body.
                    self.facts.fields.entry(class).or_default().push(Binding {
                        name: read_text(left, self.s),
                        ty: Some(t),
                        evidence: Evidence::Annotation,
                        at: n.start_byte(),
                        whole_scope: true,
                    });
                }
                if let Some(r) = right {
                    self.walk(r);
                }
                return;
            }
            "as_pattern" => {
                if let (Some(v), Some(alias)) = (n.child(0), field(n, "alias"))
                    && let Some(target) = named(alias)
                        .into_iter()
                        .next()
                        .filter(|c| c.kind() == "identifier")
                {
                    let ty = python_value_type(v, self.s);
                    let name = read_text(target, self.s);
                    let at = n.start_byte();
                    push_binding(self.scope(), name, ty, Evidence::Constructor, at, false);
                }
            }
            "for_statement" => {
                if let Some(left) = field(n, "left") {
                    let targets = if left.kind() == "identifier" {
                        vec![left]
                    } else {
                        named(left)
                            .into_iter()
                            .filter(|c| c.kind() == "identifier")
                            .collect()
                    };
                    for t in targets {
                        let name = read_text(t, self.s);
                        let at = n.start_byte();
                        push_binding(self.scope(), name, None, Evidence::Constructor, at, false);
                    }
                }
            }
            "call" => {
                if let Some(f) = field(n, "function")
                    && f.kind() == "attribute"
                    && let (Some(obj), Some(attr)) = (field(f, "object"), field(f, "attribute"))
                {
                    let receiver = self.receiver(obj);
                    self.facts.calls.push(CallSite {
                        byte: n.start_byte(),
                        line: line(n),
                        callee: read_text(attr, self.s),
                        receiver,
                    });
                }
            }
            _ => {}
        }
        for c in children(n) {
            self.walk(c);
        }
    }
}
pub fn python_facts(root: Node<'_>, source: &[u8]) -> FileFacts {
    let mut w = PyWalker {
        s: source,
        facts: FileFacts::new(source.len()),
        stack: vec![0],
        classes: vec![],
    };
    w.walk(root);
    w.facts
}

// ------------------------------------------------------- TypeScript / JS ---

fn ts_type(n: Node<'_>, s: &[u8]) -> Option<TypeRef> {
    let n = if n.kind() == "type_annotation" {
        named(n).into_iter().next()?
    } else {
        n
    };
    match n.kind() {
        "type_identifier" | "identifier" => Some(TypeRef::Name(read_text(n, s))),
        "nested_type_identifier" | "member_expression" => Some(TypeRef::Name(read_text(n, s))),
        "generic_type" => field(n, "name").and_then(|c| ts_type(c, s)),
        "union_type" => {
            let parts: Vec<Node<'_>> = named(n)
                .into_iter()
                .filter(|c| {
                    let nullish = matches!(c.kind(), "undefined" | "null")
                        || (c.kind() == "literal_type"
                            && matches!(read_text(*c, s).as_str(), "undefined" | "null"));
                    !nullish
                })
                .collect();
            if parts.len() == 1 {
                ts_type(parts[0], s)
            } else {
                None
            }
        }
        _ => None,
    }
}
fn ts_value_type(n: Node<'_>, s: &[u8]) -> Option<TypeRef> {
    let n = if n.kind() == "parenthesized_expression" {
        named(n).into_iter().next()?
    } else {
        n
    };
    match n.kind() {
        "new_expression" => {
            let c = field(n, "constructor")?;
            matches!(c.kind(), "identifier" | "member_expression")
                .then(|| TypeRef::Name(read_text(c, s)))
        }
        "as_expression" | "satisfies_expression" | "non_null_expression" => {
            let kids = named(n);
            if n.kind() == "non_null_expression" {
                ts_value_type(kids.into_iter().next()?, s)
            } else {
                kids.get(1)
                    .and_then(|t| ts_type(*t, s))
                    .or_else(|| ts_value_type(kids[0], s))
            }
        }
        _ => None,
    }
}
const JS_SCOPES: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_expression",
    "function",
    "generator_function",
    "arrow_function",
    "method_definition",
];
const JS_CLASSES: &[&str] = &["class_declaration", "abstract_class_declaration", "class"];
struct JsWalker<'s> {
    s: &'s [u8],
    facts: FileFacts,
    stack: Vec<usize>,
    classes: Vec<String>,
}
impl JsWalker<'_> {
    fn scope(&mut self) -> &mut Scope {
        let i = *self.stack.last().unwrap();
        &mut self.facts.scopes[i]
    }
    fn params(&mut self, params: Node<'_>, constructor_class: Option<String>) {
        for p in named(params) {
            let (pattern, ty, property) = match p.kind() {
                "identifier" => (Some(p), None, false),
                "required_parameter" | "optional_parameter" => (
                    field(p, "pattern"),
                    field(p, "type").and_then(|t| ts_type(t, self.s)),
                    children(p)
                        .into_iter()
                        .any(|c| c.kind() == "accessibility_modifier"),
                ),
                _ => (None, None, false),
            };
            let Some(pattern) = pattern.filter(|p| p.kind() == "identifier") else {
                continue;
            };
            let name = read_text(pattern, self.s);
            if property && let Some(class) = &constructor_class {
                self.facts
                    .fields
                    .entry(class.clone())
                    .or_default()
                    .push(Binding {
                        name: name.clone(),
                        ty: ty.clone(),
                        evidence: Evidence::Annotation,
                        at: p.start_byte(),
                        whole_scope: true,
                    });
            }
            let at = p.start_byte();
            push_binding(self.scope(), name, ty, Evidence::Annotation, at, true);
        }
    }
    fn receiver(&self, obj: Node<'_>) -> Receiver {
        match obj.kind() {
            "identifier" => Receiver::Name(read_text(obj, self.s)),
            "this" => Receiver::Name("this".into()),
            "member_expression" => {
                let base = field(obj, "object");
                let prop = field(obj, "property");
                match (base, prop) {
                    (Some(b), Some(p)) if matches!(b.kind(), "identifier" | "this") => {
                        Receiver::Field {
                            base: read_text(b, self.s),
                            field: read_text(p, self.s),
                        }
                    }
                    _ => Receiver::Other(read_text(obj, self.s)),
                }
            }
            _ => Receiver::Other(read_text(obj, self.s)),
        }
    }
    fn walk(&mut self, n: Node<'_>) {
        let kind = n.kind();
        if JS_CLASSES.contains(&kind) {
            let name = field(n, "name")
                .map(|c| read_text(c, self.s))
                .unwrap_or_default();
            self.classes.push(name);
            for c in children(n) {
                self.walk(c);
            }
            self.classes.pop();
            return;
        }
        if kind == "interface_declaration" {
            if let Some(name) = field(n, "name") {
                self.facts.interfaces.insert(read_text(name, self.s));
            }
            return;
        }
        if JS_SCOPES.contains(&kind) {
            let is_method =
                kind == "method_definition" && n.parent().is_some_and(|p| p.kind() == "class_body");
            let class = if is_method {
                self.classes.last().cloned().filter(|c| !c.is_empty())
            } else {
                None
            };
            let mut scope = Scope {
                start: n.start_byte(),
                end: n.end_byte(),
                class: class.clone(),
                self_names: if is_method {
                    vec!["this".into()]
                } else {
                    vec![]
                },
                ..Scope::default()
            };
            if let Some(class) = &class {
                scope.bindings.push(Binding {
                    name: "this".into(),
                    ty: Some(TypeRef::Name(class.clone())),
                    evidence: Evidence::SelfMethod,
                    at: n.start_byte(),
                    whole_scope: true,
                });
            }
            self.facts.scopes.push(scope);
            self.stack.push(self.facts.scopes.len() - 1);
            let ctor_class = if is_method
                && field(n, "name").is_some_and(|c| read_text(c, self.s) == "constructor")
            {
                class
            } else {
                None
            };
            if let Some(params) = field(n, "parameters") {
                self.params(params, ctor_class);
            } else if let Some(p) = field(n, "parameter") {
                let name = read_text(p, self.s);
                let at = p.start_byte();
                push_binding(self.scope(), name, None, Evidence::Annotation, at, true);
            }
            if let Some(body) = field(n, "body") {
                self.walk(body);
            }
            self.stack.pop();
            return;
        }
        match kind {
            "variable_declarator" => {
                if let Some(name) = field(n, "name").filter(|c| c.kind() == "identifier") {
                    let ann = field(n, "type").and_then(|t| ts_type(t, self.s));
                    let value = field(n, "value").and_then(|v| ts_value_type(v, self.s));
                    let (ty, evidence) = match (ann, value) {
                        (Some(t), _) => (Some(t), Evidence::Annotation),
                        (None, Some(t)) => (Some(t), Evidence::Constructor),
                        _ => (None, Evidence::Constructor),
                    };
                    let name = read_text(name, self.s);
                    let at = n.start_byte();
                    push_binding(self.scope(), name, ty, evidence, at, false);
                }
            }
            "public_field_definition" | "field_definition" => {
                if let Some(name) = field(n, "name").filter(|c| c.kind() == "property_identifier")
                    && let Some(class) = self.classes.last().cloned().filter(|c| !c.is_empty())
                {
                    let ann = field(n, "type").and_then(|t| ts_type(t, self.s));
                    let value = field(n, "value").and_then(|v| ts_value_type(v, self.s));
                    let (ty, evidence) = match (ann, value) {
                        (Some(t), _) => (Some(t), Evidence::Annotation),
                        (None, Some(t)) => (Some(t), Evidence::Constructor),
                        _ => (None, Evidence::Constructor),
                    };
                    self.facts.fields.entry(class).or_default().push(Binding {
                        name: read_text(name, self.s),
                        ty,
                        evidence,
                        at: n.start_byte(),
                        whole_scope: true,
                    });
                }
            }
            "assignment_expression" => {
                if let (Some(left), Some(right)) = (field(n, "left"), field(n, "right")) {
                    let value = ts_value_type(right, self.s);
                    if left.kind() == "identifier" {
                        let name = read_text(left, self.s);
                        let at = n.start_byte();
                        push_binding(self.scope(), name, value, Evidence::Constructor, at, false);
                    } else if left.kind() == "member_expression"
                        && field(left, "object").is_some_and(|o| o.kind() == "this")
                        && let Some(prop) = field(left, "property")
                        && let Some(class) = self.classes.last().cloned().filter(|c| !c.is_empty())
                    {
                        self.facts.fields.entry(class).or_default().push(Binding {
                            name: read_text(prop, self.s),
                            ty: value,
                            evidence: Evidence::Constructor,
                            at: n.start_byte(),
                            whole_scope: true,
                        });
                    }
                }
            }
            "call_expression" => {
                if let Some(f) = field(n, "function")
                    && f.kind() == "member_expression"
                    && let (Some(obj), Some(prop)) = (field(f, "object"), field(f, "property"))
                {
                    let receiver = self.receiver(obj);
                    self.facts.calls.push(CallSite {
                        byte: n.start_byte(),
                        line: line(n),
                        callee: read_text(prop, self.s),
                        receiver,
                    });
                }
            }
            _ => {}
        }
        for c in children(n) {
            self.walk(c);
        }
    }
}
pub fn js_facts(root: Node<'_>, source: &[u8]) -> FileFacts {
    let mut w = JsWalker {
        s: source,
        facts: FileFacts::new(source.len()),
        stack: vec![0],
        classes: vec![],
    };
    w.walk(root);
    w.facts
}

// -------------------------------------------------------------------- Go ---

fn go_type(n: Node<'_>, s: &[u8]) -> Option<TypeRef> {
    match n.kind() {
        "pointer_type" => named(n).into_iter().next().and_then(|c| go_type(c, s)),
        "type_identifier" | "qualified_type" => Some(TypeRef::Name(read_text(n, s))),
        "parenthesized_type" => named(n).into_iter().next().and_then(|c| go_type(c, s)),
        _ => None,
    }
}
fn go_value_type(n: Node<'_>, s: &[u8]) -> Option<TypeRef> {
    match n.kind() {
        "unary_expression" => (read_text(field(n, "operator")?, s) == "&")
            .then(|| go_value_type(field(n, "operand")?, s))
            .flatten(),
        "composite_literal" => field(n, "type").and_then(|t| go_type(t, s)),
        "call_expression" => {
            let f = field(n, "function")?;
            match f.kind() {
                "identifier" | "selector_expression" => {
                    Some(TypeRef::Return(read_text(f, s), line(n)))
                }
                _ => None,
            }
        }
        "parenthesized_expression" => named(n)
            .into_iter()
            .next()
            .and_then(|c| go_value_type(c, s)),
        _ => None,
    }
}
struct GoWalker<'s> {
    s: &'s [u8],
    facts: FileFacts,
    stack: Vec<usize>,
}
impl GoWalker<'_> {
    fn scope(&mut self) -> &mut Scope {
        let i = *self.stack.last().unwrap();
        &mut self.facts.scopes[i]
    }
    fn params(&mut self, list: Node<'_>) {
        for p in named(list).into_iter().filter(|p| {
            matches!(
                p.kind(),
                "parameter_declaration" | "variadic_parameter_declaration"
            )
        }) {
            let ty = if p.kind() == "parameter_declaration" {
                field(p, "type").and_then(|t| go_type(t, self.s))
            } else {
                None
            };
            let mut c = p.walk();
            let names: Vec<String> = p
                .children_by_field_name("name", &mut c)
                .map(|n| read_text(n, self.s))
                .collect();
            for name in names {
                let at = p.start_byte();
                push_binding(
                    self.scope(),
                    name,
                    ty.clone(),
                    Evidence::Annotation,
                    at,
                    true,
                );
            }
        }
    }
    fn multi_assign(&mut self, left: Node<'_>, right: Option<Node<'_>>, at: usize) {
        let lefts: Vec<String> = named(left)
            .into_iter()
            .filter(|c| c.kind() == "identifier")
            .map(|c| read_text(c, self.s))
            .collect();
        let rights: Vec<Node<'_>> = right.map(named).unwrap_or_default();
        for (i, name) in lefts.iter().enumerate() {
            let ty = if rights.len() == lefts.len() {
                go_value_type(rights[i], self.s)
            } else if rights.len() == 1 && i == 0 {
                // `x, err := New()` binds the first name to the first result.
                go_value_type(rights[0], self.s)
            } else {
                None
            };
            push_binding(
                self.scope(),
                name.clone(),
                ty,
                Evidence::Constructor,
                at,
                false,
            );
        }
    }
    fn receiver(&self, obj: Node<'_>) -> Receiver {
        match obj.kind() {
            "identifier" => Receiver::Name(read_text(obj, self.s)),
            "selector_expression" => match (field(obj, "operand"), field(obj, "field")) {
                (Some(b), Some(f)) if b.kind() == "identifier" => Receiver::Field {
                    base: read_text(b, self.s),
                    field: read_text(f, self.s),
                },
                _ => Receiver::Other(read_text(obj, self.s)),
            },
            _ => Receiver::Other(read_text(obj, self.s)),
        }
    }
    fn walk(&mut self, n: Node<'_>) {
        match n.kind() {
            "function_declaration" | "method_declaration" | "func_literal" => {
                let mut scope = Scope {
                    start: n.start_byte(),
                    end: n.end_byte(),
                    ..Scope::default()
                };
                if n.kind() == "method_declaration"
                    && let Some(recv) = field(n, "receiver")
                    && let Some(p) = named(recv)
                        .into_iter()
                        .find(|p| p.kind() == "parameter_declaration")
                    && let Some(ty) = field(p, "type").and_then(|t| go_type(t, self.s))
                {
                    let class = match &ty {
                        TypeRef::Name(t) => Some(t.clone()),
                        TypeRef::Return(..) => None,
                    };
                    scope.class = class;
                    if let Some(name) = field(p, "name") {
                        let name = read_text(name, self.s);
                        scope.self_names.push(name.clone());
                        scope.bindings.push(Binding {
                            name,
                            ty: Some(ty),
                            evidence: Evidence::SelfMethod,
                            at: p.start_byte(),
                            whole_scope: true,
                        });
                    }
                }
                self.facts.scopes.push(scope);
                self.stack.push(self.facts.scopes.len() - 1);
                if let Some(params) = field(n, "parameters") {
                    self.params(params);
                }
                if let Some(body) = field(n, "body") {
                    self.walk(body);
                }
                self.stack.pop();
                return;
            }
            "type_spec" => {
                if let (Some(name), Some(ty)) = (field(n, "name"), field(n, "type")) {
                    let name = read_text(name, self.s);
                    if ty.kind() == "interface_type" {
                        self.facts.interfaces.insert(name);
                    } else if ty.kind() == "struct_type" {
                        for f in named(ty)
                            .into_iter()
                            .filter(|c| c.kind() == "field_declaration_list")
                            .flat_map(named)
                            .filter(|c| c.kind() == "field_declaration")
                        {
                            let fty = field(f, "type").and_then(|t| go_type(t, self.s));
                            let mut c = f.walk();
                            let names: Vec<String> = f
                                .children_by_field_name("name", &mut c)
                                .map(|c| read_text(c, self.s))
                                .collect();
                            for fname in names {
                                self.facts
                                    .fields
                                    .entry(name.clone())
                                    .or_default()
                                    .push(Binding {
                                        name: fname,
                                        ty: fty.clone(),
                                        evidence: Evidence::Annotation,
                                        at: f.start_byte(),
                                        whole_scope: true,
                                    });
                            }
                        }
                    }
                }
                return;
            }
            "short_var_declaration" | "assignment_statement" => {
                if let Some(left) = field(n, "left") {
                    self.multi_assign(left, field(n, "right"), n.start_byte());
                }
            }
            "var_spec" => {
                let mut c = n.walk();
                let names: Vec<String> = n
                    .children_by_field_name("name", &mut c)
                    .map(|c| read_text(c, self.s))
                    .collect();
                let ann = field(n, "type").and_then(|t| go_type(t, self.s));
                let values: Vec<Node<'_>> = field(n, "value").map(named).unwrap_or_default();
                for (i, name) in names.iter().enumerate() {
                    let (ty, evidence) = if let Some(t) = &ann {
                        (Some(t.clone()), Evidence::Annotation)
                    } else if values.len() == names.len() {
                        (go_value_type(values[i], self.s), Evidence::Constructor)
                    } else {
                        (None, Evidence::Constructor)
                    };
                    let at = n.start_byte();
                    push_binding(self.scope(), name.clone(), ty, evidence, at, false);
                }
            }
            "range_clause" => {
                if let Some(left) = field(n, "left") {
                    for c in named(left).into_iter().filter(|c| c.kind() == "identifier") {
                        let name = read_text(c, self.s);
                        let at = n.start_byte();
                        push_binding(self.scope(), name, None, Evidence::Constructor, at, false);
                    }
                }
            }
            "call_expression" => {
                if let Some(f) = field(n, "function")
                    && f.kind() == "selector_expression"
                    && let (Some(obj), Some(fld)) = (field(f, "operand"), field(f, "field"))
                {
                    let receiver = self.receiver(obj);
                    self.facts.calls.push(CallSite {
                        byte: n.start_byte(),
                        line: line(n),
                        callee: read_text(fld, self.s),
                        receiver,
                    });
                }
            }
            _ => {}
        }
        for c in children(n) {
            self.walk(c);
        }
    }
}
pub fn go_facts(root: Node<'_>, source: &[u8]) -> FileFacts {
    let mut w = GoWalker {
        s: source,
        facts: FileFacts::new(source.len()),
        stack: vec![0],
    };
    w.walk(root);
    w.facts
}
