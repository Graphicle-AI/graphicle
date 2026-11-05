// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/go.py
//! Go package-scoped types, case-stable symbols, signatures and call candidates.
use super::base::{file_stem, is_language_builtin, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax};
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;
pub const PREDECLARED_TYPES: &[&str] = &[
    "bool",
    "byte",
    "complex64",
    "complex128",
    "error",
    "float32",
    "float64",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "rune",
    "string",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "any",
    "comparable",
];
pub const PREDECLARED_FUNCS: &[&str] = &[
    "append", "cap", "clear", "close", "complex", "copy", "delete", "imag", "len", "make", "max",
    "min", "new", "panic", "print", "println", "real", "recover",
];
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
/// `_go_type_param_names`: the names a declaration's generic type parameters
/// bind: its own `[T any, R comparable]` list and, for a method, the type
/// arguments of its receiver (`(b Box[T])`; a method declares none of its
/// own). They are local bindings, not corpus symbols (#4383).
pub fn type_param_names(decl: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    for declaration in children(decl)
        .filter(|n| n.kind() == "type_parameter_list")
        .flat_map(children)
        .filter(|n| n.kind() == "type_parameter_declaration")
    {
        for c in children(declaration).filter(|n| n.kind() == "identifier") {
            let text = read_text(c, source);
            if !text.is_empty() {
                names.insert(text);
            }
        }
    }
    if let Some(receiver) = decl.child_by_field_name("receiver") {
        let mut stack = vec![receiver];
        while let Some(n) = stack.pop() {
            if n.kind() == "type_arguments" {
                let mut leaves = vec![n];
                while let Some(leaf) = leaves.pop() {
                    if leaf.kind() == "type_identifier" {
                        let text = read_text(leaf, source);
                        if !text.is_empty() {
                            names.insert(text);
                        }
                    }
                    leaves.extend(children(leaf));
                }
                continue;
            }
            stack.extend(children(n));
        }
    }
    names
}
/// Walk a Go type expression into `(name, role)` references; names in `skip`
/// (the enclosing declaration's type parameters) are bindings and dropped.
pub fn collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut Vec<(String, String)>,
    skip: &HashSet<String>,
) {
    let Some(node) = node else {
        return;
    };
    match node.kind() {
        "type_identifier" | "qualified_type" => {
            let text = read_text(node, source);
            if !text.is_empty()
                && (node.kind() == "qualified_type"
                    || (!PREDECLARED_TYPES.contains(&text.as_str()) && !skip.contains(&text)))
            {
                out.push((text, if generic { "generic_arg" } else { "type" }.into()));
            }
            return;
        }
        "generic_type" => {
            collect_type_refs(node.child_by_field_name("type"), source, generic, out, skip);
            for args in children(node).filter(|n| n.kind() == "type_arguments") {
                for arg in children(args).filter(|n| n.is_named()) {
                    collect_type_refs(Some(arg), source, true, out, skip);
                }
            }
            return;
        }
        _ => {}
    }
    if node.is_named() {
        for child in children(node).filter(|n| n.is_named()) {
            collect_type_refs(Some(child), source, generic, out, skip);
        }
    }
}
/// `_go_strip_generic_args`: a generic receiver's type text without its
/// instantiation arguments (`Box[T]` -> `Box`), so a generic type's methods
/// attach to the type's own id (#4438).
/// `_go_alias_re_exports` (0.9.95, #4579): `type A = pkg.A` (or `pkg.A[...]`)
/// re-exports another package's type under its own name and declares none:
/// references to `A` keep resolving to the original.
fn alias_re_exports(alias: Node<'_>, source: &[u8]) -> bool {
    let mut target = alias.child_by_field_name("type");
    if let Some(generic) = target.filter(|n| n.kind() == "generic_type") {
        target = generic.child_by_field_name("type");
    }
    let Some(target) = target.filter(|n| n.kind() == "qualified_type") else {
        return false;
    };
    match (
        alias.child_by_field_name("name"),
        target.child_by_field_name("name"),
    ) {
        (Some(name), Some(target_name)) => {
            read_text(name, source) == read_text(target_name, source)
        }
        _ => false,
    }
}
pub fn strip_generic_args(type_text: &str) -> &str {
    trim(type_text.split('[').next().unwrap_or(""))
}
/// `_go_single_type_name`: the one declared type a Go type expression names
/// (`Greeter`, `*Greeter`, generic arguments dropped), or None for a slice,
/// map, channel or predeclared type. A qualified type keeps its package as
/// written (`pkg.Greeter`), bound only through the file's import of `pkg`
/// (Graphify 0.9.102, #3368).
pub fn single_type_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let mut node = node?;
    while matches!(node.kind(), "pointer_type" | "parenthesized_type" | "generic_type") {
        node = node
            .child_by_field_name("type")
            .or_else(|| children(node).find(|n| n.is_named()))?;
    }
    if node.kind() == "qualified_type" {
        let package = node.child_by_field_name("package")?;
        let name = node.child_by_field_name("name")?;
        return Some(format!("{}.{}", read_text(package, source), read_text(name, source)));
    }
    if node.kind() != "type_identifier" {
        return None;
    }
    let name = read_text(node, source);
    (!name.is_empty() && !PREDECLARED_TYPES.contains(&name.as_str())).then_some(name)
}
/// `_go_receiver_type_table` (Graphify 0.9.102, #4962): `name -> TypeName`
/// for every binding whose type is written down or inferred from a
/// composite literal (struct field, parameter incl. the method receiver,
/// `var x T`, `x := T{}`, `var x = &T{}`), and `null` for one whose type is
/// unknown, which still shadows a package variable. Function bodies (`block`)
/// and nested closures are left out; `package_only` also leaves out fields
/// and every function. First binding in source order wins.
pub fn receiver_type_table(root: Node<'_>, source: &[u8], package_only: bool) -> Map<String, Value> {
    let mut table = Map::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let kind = n.kind();
        if kind == "block" || (kind == "func_literal" && n.id() != root.id()) {
            continue;
        }
        if package_only
            && matches!(
                kind,
                "function_declaration" | "method_declaration" | "func_literal" | "type_declaration"
            )
        {
            continue;
        }
        match kind {
            "field_declaration"
            | "parameter_declaration"
            | "variadic_parameter_declaration"
            | "var_spec" => {
                let names = children(n)
                    .filter(|c| matches!(c.kind(), "field_identifier" | "identifier"))
                    .collect::<Vec<_>>();
                let type_name = if kind == "variadic_parameter_declaration" {
                    None
                } else {
                    single_type_name(n.child_by_field_name("type"), source)
                };
                let mut inferred = BTreeMap::new();
                if kind == "var_spec"
                    && type_name.is_none()
                    && let Some(value) = n.child_by_field_name("value")
                {
                    inferred = inferred_composite_bindings(
                        names.iter().copied(),
                        children(value).filter(|c| c.is_named()),
                        source,
                    );
                }
                for name in names {
                    let name = read_text(name, source);
                    let ty = type_name.clone().or_else(|| inferred.get(&name).cloned());
                    table.entry(name).or_insert_with(|| json!(ty));
                }
            }
            "short_var_declaration" => {
                if let (Some(left), Some(right)) = (
                    n.child_by_field_name("left"),
                    n.child_by_field_name("right"),
                ) {
                    let names = children(left)
                        .filter(|c| c.kind() == "identifier")
                        .collect::<Vec<_>>();
                    let inferred = inferred_composite_bindings(
                        names.iter().copied(),
                        children(right).filter(|c| c.is_named()),
                        source,
                    );
                    for name in names {
                        let name = read_text(name, source);
                        let ty = inferred.get(&name).cloned();
                        table.entry(name).or_insert_with(|| json!(ty));
                    }
                }
            }
            "range_clause" if children(n).any(|c| c.kind() == ":=") => {
                if let Some(left) = n.child_by_field_name("left") {
                    for name in children(left).filter(|c| c.is_named() && c.kind() == "identifier") {
                        table.entry(read_text(name, source)).or_insert(Value::Null);
                    }
                }
            }
            _ => {}
        }
        let mut kids = children(n).collect::<Vec<_>>();
        kids.reverse();
        stack.extend(kids);
    }
    table
}
/// `_go_inferred_composite_bindings`: each same-position name and `T{}` /
/// `&T{}` value binds the name to `T`.
fn inferred_composite_bindings<'t>(
    names: impl Iterator<Item = Node<'t>>,
    values: impl Iterator<Item = Node<'t>>,
    source: &[u8],
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, mut value) in names.zip(values) {
        if value.kind() == "unary_expression" {
            value = value.child_by_field_name("operand").unwrap_or(value);
        }
        if value.kind() != "composite_literal" {
            continue;
        }
        if let Some(type_name) = single_type_name(value.child_by_field_name("type"), source) {
            out.entry(read_text(name, source)).or_insert(type_name);
        }
    }
    out
}
/// `_go_struct_field_types` (Graphify 0.9.99, #3368): `struct -> {field ->
/// TypeName}` for every struct the file declares, by the rule of the flat
/// receiver table, keyed by the struct.
fn struct_field_types(root: Node<'_>, source: &[u8]) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut by_struct = BTreeMap::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "function_declaration" | "method_declaration") {
            continue;
        }
        if n.kind() == "type_spec" {
            if let (Some(name), Some(body)) =
                (n.child_by_field_name("name"), n.child_by_field_name("type"))
                && body.kind() == "struct_type"
            {
                let mut fields = BTreeMap::new();
                for list in children(body).filter(|c| c.kind() == "field_declaration_list") {
                    for field in children(list).filter(|c| c.kind() == "field_declaration") {
                        let Some(type_name) =
                            single_type_name(field.child_by_field_name("type"), source)
                        else {
                            continue;
                        };
                        for c in children(field).filter(|c| c.kind() == "field_identifier") {
                            fields
                                .entry(read_text(c, source))
                                .or_insert_with(|| type_name.clone());
                        }
                    }
                }
                if !fields.is_empty() {
                    by_struct.insert(read_text(name, source), fields);
                }
            }
            continue;
        }
        stack.extend(children(n));
    }
    by_struct
}
/// A function body to walk for calls: the caller, the body, the method's
/// receiver name and type, and the function's own-scope type table.
struct Body<'tree> {
    caller: String,
    body: Node<'tree>,
    self_name: Option<String>,
    self_type: Option<String>,
    scope_types: Map<String, Value>,
}
struct Extractor<'source, 'tree> {
    source: &'source [u8],
    path: String,
    stem: String,
    scope: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    /// Unique declaration ids, the file id reserved (Graphify 0.9.98, #4854).
    decl: crate::ids::DeclIdRegistry,
    bodies: Vec<Body<'tree>>,
    // `_callable` / `_callable_class` stamps (#2438, #3152).
    callable: HashSet<String>,
    callable_class: HashSet<String>,
    /// Go interfaces: implicit, so nothing else tells their requirements from
    /// a struct's methods (Graphify 0.9.102).
    interfaces: HashSet<String>,
    /// The labels of the types this file declares (Graphify 0.9.102).
    local_type_names: HashSet<String>,
    imports: Map<String, Value>,
    groups: HashMap<String, HashSet<String>>,
    labels: HashMap<String, String>,
    call_pairs: HashSet<(String, String)>,
    raw_calls: Vec<Value>,
}
impl<'tree> Extractor<'_, 'tree> {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        if self.seen.insert(id.into()) {
            self.nodes.push(json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")}));
        }
    }
    fn edge(
        &mut self,
        source: &str,
        target: &str,
        relation: &str,
        line: usize,
        context: Option<&str>,
    ) {
        let mut e = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(c) = context.filter(|s| !s.is_empty()) {
            e["context"] = json!(c);
        }
        self.edges.push(e);
    }
    fn named(&mut self, name: &str) -> String {
        let local = self.type_symbol(name);
        if self.seen.contains(&local) {
            return local;
        }
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    fn type_refs(&self, node: Option<Node<'_>>, skip: &HashSet<String>) -> Vec<(String, String)> {
        let mut refs = vec![];
        collect_type_refs(node, self.source, false, &mut refs, skip);
        refs
    }
    fn refs(
        &mut self,
        node: Option<Node<'_>>,
        from: &str,
        line: usize,
        context: &str,
        skip: &HashSet<String>,
    ) {
        for (name, role) in self.type_refs(node, skip) {
            let target = self.named(&name);
            if target != from {
                self.edge(
                    from,
                    &target,
                    "references",
                    line,
                    Some(if role == "generic_arg" {
                        "generic_arg"
                    } else {
                        context
                    }),
                );
            }
        }
    }
    /// `emit_go_package_var_type_refs` (Graphify 0.9.101, #2360): the types a
    /// package-level `var` names, in its declared type, a composite literal
    /// or a type argument (`new(*pkg.T)`), are referenced by the file; a
    /// closure body is left to the call walk.
    fn package_var_type_refs(&mut self, var_decl: Node<'_>) {
        const TYPE_NODES: [&str; 9] = [
            "type_identifier",
            "qualified_type",
            "pointer_type",
            "generic_type",
            "slice_type",
            "array_type",
            "map_type",
            "channel_type",
            "parenthesized_type",
        ];
        let file = self.file.clone();
        let mut stack = vec![var_decl];
        while let Some(n) = stack.pop() {
            if n.kind() == "func_literal" {
                continue;
            }
            let mut type_nodes = vec![];
            if matches!(n.kind(), "var_spec" | "composite_literal") {
                type_nodes.extend(n.child_by_field_name("type"));
            } else if n.kind() == "argument_list" {
                type_nodes.extend(children(n).filter(|c| TYPE_NODES.contains(&c.kind())));
            }
            for type_node in &type_nodes {
                let line = type_node.start_position().row + 1;
                for (name, role) in self.type_refs(Some(*type_node), &HashSet::new()) {
                    let target = self.named(&name);
                    let context = if role == "generic_arg" {
                        "generic_arg"
                    } else {
                        "variable_type"
                    };
                    self.edge(&file, &target, "references", line, Some(context));
                }
            }
            stack.extend(children(n).filter(|c| !type_nodes.iter().any(|t| t.id() == c.id())));
        }
    }
    /// Parameter and return type references of a signature. Its own generic
    /// type parameters (and those of the receiver, or of an enclosing generic
    /// interface given in `type_params`) are bindings: a `[T any]` signature
    /// no longer mints a sourceless `T` shared by every generic function.
    fn signatures(
        &mut self,
        node: Node<'_>,
        function: &str,
        line: usize,
        type_params: &HashSet<String>,
    ) {
        let mut skip = type_param_names(node, self.source);
        skip.extend(type_params.iter().cloned());
        if let Some(params) = node.child_by_field_name("parameters") {
            for p in children(params).filter(|n| n.kind() == "parameter_declaration") {
                self.refs(
                    p.child_by_field_name("type"),
                    function,
                    line,
                    "parameter_type",
                    &skip,
                );
            }
        }
        if let Some(result) = node.child_by_field_name("result") {
            if result.kind() == "parameter_list" {
                for p in children(result).filter(|n| n.kind() == "parameter_declaration") {
                    self.refs(
                        p.child_by_field_name("type")
                            .or_else(|| children(p).find(|n| n.is_named())),
                        function,
                        line,
                        "return_type",
                        &skip,
                    );
                }
            } else {
                self.refs(Some(result), function, line, "return_type", &skip);
            }
        }
    }
    fn receiver(&self, node: Node<'_>) -> Option<String> {
        let receiver = node.child_by_field_name("receiver")?;
        let parameter = children(receiver).find(|n| n.kind() == "parameter_declaration")?;
        parameter.child_by_field_name("type").map(|n| {
            strip_generic_args(trim(read_text(n, self.source).trim_start_matches('*'))).to_string()
        })
    }
    /// The method receiver's variable name (`s` in `func (s *Server)`).
    fn receiver_name(&self, node: Node<'_>) -> Option<String> {
        let receiver = node.child_by_field_name("receiver")?;
        let parameter = children(receiver).find(|n| n.kind() == "parameter_declaration")?;
        parameter
            .child_by_field_name("name")
            .map(|n| read_text(n, self.source))
    }
    fn scan(&mut self, node: Node<'_>) {
        // #3737: interface method names join the case-collision groups
        // under their owner so case-only siblings get distinct, stable ids.
        if node.kind() == "type_spec"
            && let Some(name) = node.child_by_field_name("name")
        {
            let owner = self.type_symbol(&read_text(name, self.source));
            for body in children(node).filter(|n| n.kind() == "interface_type") {
                for elem in children(body).filter(|n| n.kind() == "method_elem") {
                    if let Some(method) = elem.child_by_field_name("name") {
                        let method = read_text(method, self.source);
                        self.groups
                            .entry(make_id([owner.as_str(), method.as_str()]))
                            .or_default()
                            .insert(method);
                    }
                }
            }
        }
        if matches!(node.kind(), "function_declaration" | "method_declaration") {
            if let Some(name) = node.child_by_field_name("name") {
                let name = read_text(name, self.source);
                let base = if node.kind() == "method_declaration" {
                    self.receiver(node)
                        .filter(|s| !s.is_empty())
                        .map(|r| self.type_symbol(&r))
                        .unwrap_or_else(|| self.stem.clone())
                } else {
                    self.stem.clone()
                };
                self.groups
                    .entry(make_id([base.as_str(), name.as_str()]))
                    .or_default()
                    .insert(name);
            }
            return;
        }
        for n in children(node) {
            self.scan(n);
        }
    }
    /// `_scan_type_names` (0.9.92, #4468; aliases 0.9.95, #4579): every type
    /// declared in the file joins the case-collision groups under the package
    /// scope first, so `type ResponseWriter interface` and `type
    /// responseWriter struct` get distinct ids, and a method is keyed under its
    /// owner's final id. Declarations inside function bodies are not counted.
    fn scan_types(&mut self, node: Node<'_>) {
        if (node.kind() == "type_spec"
            || (node.kind() == "type_alias" && !alias_re_exports(node, self.source)))
            && let Some(name) = node.child_by_field_name("name")
        {
            let name = read_text(name, self.source);
            self.groups
                .entry(make_id([self.scope.as_str(), name.as_str()]))
                .or_default()
                .insert(name);
        }
        if matches!(node.kind(), "function_declaration" | "method_declaration") {
            return;
        }
        for n in children(node) {
            self.scan_types(n);
        }
    }
    /// `type_symbol_nid`: the id of a type name, salted when the file declares
    /// it beside a case-only sibling.
    fn type_symbol(&mut self, name: &str) -> String {
        let plain = make_id([self.scope.as_str(), name]);
        if self
            .groups
            .get(&plain)
            .is_some_and(|names| names.contains(name))
        {
            self.symbol_of(plain, name, "type")
        } else {
            self.decl_id(&plain, name, "type")
        }
    }
    /// The unique declaration id of the file (Graphify 0.9.98, #4854).
    fn decl_id(&mut self, nid: &str, name: &str, kind: &str) -> String {
        let seen = &self.seen;
        self.decl.resolve(nid, name, kind, |id| seen.contains(id))
    }
    fn symbol(&mut self, plain: String, name: &str) -> String {
        self.symbol_of(plain, name, "fn")
    }
    fn symbol_of(&mut self, plain: String, name: &str, kind: &str) -> String {
        let id = self.case_salted(plain, name);
        self.decl_id(&id, name, kind)
    }
    fn case_salted(&self, plain: String, name: &str) -> String {
        let Some(names) = self.groups.get(&plain).filter(|n| n.len() >= 2) else {
            return plain;
        };
        let exported: Vec<_> = names
            .iter()
            .filter(|n| n.chars().next().is_some_and(char::is_uppercase))
            .collect();
        if exported.len() == 1 && exported[0] == name {
            return plain;
        }
        let digest = format!("{:x}", Sha1::digest(name.as_bytes()));
        make_id([plain.as_str(), &digest[..6]])
    }
    fn walk(&mut self, node: Node<'tree>) {
        let line = node.start_position().row + 1;
        match node.kind() {
            "function_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let name = read_text(name, self.source);
                    let id = self.symbol(make_id([self.stem.as_str(), name.as_str()]), &name);
                    self.node(&id, &format!("{name}()"), line);
                    self.callable.insert(id.clone());
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    self.signatures(node, &id, line, &HashSet::new());
                    if let Some(body) = node.child_by_field_name("body") {
                        let scope_types = receiver_type_table(node, self.source, false);
                        self.bodies.push(Body {
                            caller: id,
                            body,
                            self_name: None,
                            self_type: None,
                            scope_types,
                        });
                    }
                }
                return;
            }
            "method_declaration" => {
                let Some(name) = node.child_by_field_name("name") else {
                    return;
                };
                let name = read_text(name, self.source);
                let receiver_type = self.receiver(node).filter(|s| !s.is_empty());
                let receiver_name = self.receiver_name(node);
                let id = if let Some(receiver) = receiver_type.clone() {
                    let parent = self.type_symbol(&receiver);
                    self.node(&parent, &receiver, line);
                    self.callable_class.insert(parent.clone());
                    let id = self.symbol(make_id([parent.as_str(), name.as_str()]), &name);
                    self.node(&id, &format!(".{name}()"), line);
                    self.edge(&parent, &id, "method", line, None);
                    id
                } else {
                    let id = self.symbol(make_id([self.stem.as_str(), name.as_str()]), &name);
                    self.node(&id, &format!("{name}()"), line);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    id
                };
                self.callable.insert(id.clone());
                self.signatures(node, &id, line, &HashSet::new());
                if let Some(body) = node.child_by_field_name("body") {
                    let scope_types = receiver_type_table(node, self.source, false);
                    self.bodies.push(Body {
                        caller: id,
                        body,
                        self_name: receiver_name,
                        self_type: receiver_type,
                        scope_types,
                    });
                }
                return;
            }
            "type_declaration" => {
                for spec in
                    children(node).filter(|n| matches!(n.kind(), "type_spec" | "type_alias"))
                {
                    if spec.kind() == "type_alias" && alias_re_exports(spec, self.source) {
                        continue;
                    }
                    let Some(name) = spec.child_by_field_name("name") else {
                        continue;
                    };
                    let name = read_text(name, self.source);
                    let line = spec.start_position().row + 1;
                    let id = self.type_symbol(&name);
                    self.node(&id, &name, line);
                    self.callable_class.insert(id.clone());
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    // `type A = B` declares a type like `type A B` (0.9.95,
                    // #4579): references land on it. Its right side is not walked.
                    if spec.kind() == "type_alias" {
                        continue;
                    }
                    // A generic type's own parameters (`type Box[T any]`) are
                    // bindings for its fields and methods (#4383).
                    let type_params = type_param_names(spec, self.source);
                    let Some(body) = children(spec)
                        .find(|n| matches!(n.kind(), "struct_type" | "interface_type"))
                    else {
                        continue;
                    };
                    if body.kind() == "struct_type" {
                        for field in children(body)
                            .filter(|n| n.kind() == "field_declaration_list")
                            .flat_map(children)
                            .filter(|n| n.kind() == "field_declaration")
                        {
                            let has_name = children(field).any(|n| n.kind() == "field_identifier");
                            let typ = field.child_by_field_name("type").or_else(|| {
                                children(field)
                                    .find(|n| n.is_named() && n.kind() != "field_identifier")
                            });
                            let line = field.start_position().row + 1;
                            for (name, role) in self.type_refs(typ, &type_params) {
                                let target = self.named(&name);
                                if target == id {
                                    continue;
                                }
                                if !has_name && role == "type" {
                                    self.edge(&id, &target, "embeds", line, None);
                                } else {
                                    self.edge(
                                        &id,
                                        &target,
                                        "references",
                                        line,
                                        Some(if role == "generic_arg" {
                                            "generic_arg"
                                        } else {
                                            "field"
                                        }),
                                    );
                                }
                            }
                        }
                    } else {
                        self.interfaces.insert(id.clone());
                        for elem in children(body) {
                            let line = elem.start_position().row + 1;
                            if elem.kind() == "method_elem" {
                                let Some(name) = elem.child_by_field_name("name").or_else(|| {
                                    children(elem).find(|n| n.kind() == "field_identifier")
                                }) else {
                                    continue;
                                };
                                let name = read_text(name, self.source);
                                let method =
                                    self.symbol(make_id([id.as_str(), name.as_str()]), &name);
                                self.node(&method, &format!(".{name}()"), line);
                                self.edge(&id, &method, "method", line, None);
                                // #3672: interface method signatures keep their
                                // parameter/return type references.
                                self.signatures(elem, &method, line, &type_params);
                                continue;
                            }
                            if elem.kind() != "type_elem" {
                                continue;
                            }
                            let type_set =
                                children(elem).any(|n| matches!(n.kind(), "|" | "negated_type"));
                            let refs: Vec<_> = children(elem)
                                .filter(|n| n.is_named())
                                .flat_map(|n| self.type_refs(Some(n), &type_params))
                                .collect();
                            for (name, role) in refs {
                                let target = self.named(&name);
                                if target == id {
                                    continue;
                                }
                                if type_set {
                                    self.edge(
                                        &id,
                                        &target,
                                        "references",
                                        line,
                                        Some("type_constraint"),
                                    );
                                } else if role == "type" {
                                    self.edge(&id, &target, "embeds", line, None);
                                } else {
                                    self.edge(
                                        &id,
                                        &target,
                                        "references",
                                        line,
                                        Some("generic_arg"),
                                    );
                                }
                            }
                        }
                    }
                }
                return;
            }
            // A package-level `var` initialiser runs at package init: its
            // calls, closures included, are the file's (Graphify 0.9.99,
            // #2360).
            "var_declaration" if node.parent().is_some_and(|p| p.kind() == "source_file") => {
                self.package_var_type_refs(node);
                let scope_types = receiver_type_table(node, self.source, false);
                self.bodies.push(Body {
                    caller: self.file.clone(),
                    body: node,
                    self_name: None,
                    self_type: None,
                    scope_types,
                });
                return;
            }
            "import_declaration" => {
                for n in children(node) {
                    if n.kind() == "import_spec_list" {
                        for spec in children(n).filter(|n| n.kind() == "import_spec") {
                            self.import(spec);
                        }
                    } else if n.kind() == "import_spec" {
                        self.import(n);
                    }
                }
                return;
            }
            _ => {}
        }
        for n in children(node) {
            self.walk(n);
        }
    }
    fn import(&mut self, spec: Node<'_>) {
        if let Some(path) = spec.child_by_field_name("path") {
            let raw = read_text(path, self.source).trim_matches('"').to_string();
            let target = make_id(["go", "pkg", raw.as_str()]);
            self.edge(
                &self.file.clone(),
                &target,
                "imports_from",
                spec.start_position().row + 1,
                Some("import"),
            );
            let local = spec
                .child_by_field_name("name")
                .map(|n| read_text(n, self.source))
                .unwrap_or_else(|| raw.rsplit('/').next().unwrap_or("").into());
            if !local.is_empty() && local != "_" && local != "." {
                self.imports.insert(local, json!(raw));
            }
        }
    }
    /// The call walk. `types` are the bindings in scope at `node`: the
    /// function's parameters, then each local as its declaration is passed,
    /// copied at every block so an inner binding never leaks out; an unknown
    /// type is `null` and still shadows a package variable (Graphify 0.9.102,
    /// #4962).
    fn calls(
        &mut self,
        node: Node<'_>,
        caller: &str,
        scope: &Body<'_>,
        types: &mut Map<String, Value>,
    ) {
        if matches!(node.kind(), "function_declaration" | "method_declaration") {
            return;
        }
        let mut inner_types;
        let types = if matches!(
            node.kind(),
            "block"
                | "if_statement"
                | "for_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "select_statement"
                | "expression_case"
                | "type_case"
                | "communication_case"
                | "default_case"
                | "func_literal"
        ) {
            inner_types = types.clone();
            if node.kind() == "func_literal" {
                for (name, ty) in receiver_type_table(node, self.source, false) {
                    inner_types.insert(name, ty);
                }
            }
            &mut inner_types
        } else {
            types
        };
        let typed = |types: &Map<String, Value>, name: &str| {
            types.get(name).and_then(Value::as_str).map(str::to_string)
        };
        let self_name = scope.self_name.clone().filter(|n| !n.is_empty());
        // `s.field` is the receiver's field only while `s` still names it.
        let self_bound = self_name
            .as_deref()
            .is_some_and(|n| typed(&*types, n) == scope.self_type);
        if node.kind() == "call_expression" {
            let mut callee = None;
            let mut member = false;
            let mut bare = false;
            let mut receiver = None;
            let mut member_receiver = None;
            let mut member_receiver_type = None;
            let mut field_owner: Option<String> = None;
            let mut import_path = None;
            if let Some(function) = node.child_by_field_name("function") {
                if function.kind() == "identifier" {
                    bare = true;
                    callee = Some(read_text(function, self.source));
                } else if function.kind() == "selector_expression" {
                    let operand_node = function.child_by_field_name("operand");
                    let operand = operand_node
                        .map(|n| read_text(n, self.source))
                        .unwrap_or_default();
                    member = !self.imports.contains_key(&operand);
                    if !member {
                        receiver = Some(operand.clone());
                        import_path = self
                            .imports
                            .get(&operand)
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    } else if let Some(op) = operand_node {
                        if op.kind() == "identifier" {
                            member_receiver_type = typed(&*types, &operand);
                            // A type named as the receiver (`T.M`), unless a
                            // binding shadows it.
                            if !types.contains_key(&operand) && self.local_type_names.contains(&operand) {
                                member_receiver_type = Some(operand.clone());
                            }
                            member_receiver = Some(operand.clone());
                        } else if op.kind() == "parenthesized_expression" {
                            // `(*T).M`: the method expression of a local type.
                            let mut expression = op;
                            while expression.kind() == "parenthesized_expression"
                                && let Some(first) = children(expression).find(|c| c.is_named())
                            {
                                expression = first;
                            }
                            if expression.kind() == "unary_expression"
                                && read_text(expression, self.source).starts_with('*')
                            {
                                let name = expression
                                    .child_by_field_name("operand")
                                    .map(|o| read_text(o, self.source))
                                    .unwrap_or_default();
                                if self.local_type_names.contains(&name) && !types.contains_key(&name) {
                                    member_receiver = Some(name.clone());
                                    member_receiver_type = Some(name);
                                }
                            }
                        } else if op.kind() == "selector_expression" {
                            // `s.logger.Log()` names the receiver by the field
                            // only when `s` is this method's own receiver.
                            let inner = op.child_by_field_name("operand");
                            if let (Some(self_name), Some(inner)) = (self_name.as_deref(), inner)
                                && inner.kind() == "identifier"
                                && read_text(inner, self.source) == self_name
                                && self_bound
                            {
                                member_receiver = Some(
                                    op.child_by_field_name("field")
                                        .map(|f| read_text(f, self.source))
                                        .unwrap_or_default(),
                                );
                                // The struct whose field it is, possibly declared
                                // in a sibling file (Graphify 0.9.99, #3368).
                                field_owner = scope.self_type.clone();
                            }
                        }
                    }
                    callee = function
                        .child_by_field_name("field")
                        .map(|n| read_text(n, self.source));
                }
            }
            if bare
                && callee
                    .as_ref()
                    .is_some_and(|n| PREDECLARED_FUNCS.contains(&n.as_str()))
            {
                callee = None;
            }
            if let Some(callee) = callee.filter(|s| !s.is_empty() && !is_language_builtin(s)) {
                // A member call resolves by its receiver's type; the bare name
                // proves nothing about the receiver (Graphify 0.9.102).
                let target = if import_path.as_ref().is_some_and(|s| !s.is_empty()) || member {
                    None
                } else {
                    self.labels.get(&callee).filter(|s| !s.is_empty()).cloned()
                };
                if let Some(target) = target.filter(|s| s != caller) {
                    if self.call_pairs.insert((caller.into(), target.clone())) {
                        self.edge(
                            caller,
                            &target,
                            "calls",
                            node.start_position().row + 1,
                            Some("call"),
                        );
                    }
                } else {
                    // `member_receiver` is a key of its own: `receiver` means
                    // "imported package" here, and other languages' member
                    // resolvers read `receiver`.
                    let bound = field_owner.is_none()
                        && member_receiver.as_ref().is_some_and(|r| types.contains_key(r));
                    self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":member,"language":"go","receiver":receiver,"member_receiver":member_receiver,"receiver_type":member_receiver_type,"receiver_bound":bound,"field_owner":field_owner,"import_path":import_path,"source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
                }
            }
            // A method value passed as an argument (`routing.Wrap(s.Handle)`)
            // registers a handler: an indirect member call through the
            // receiver's type (Graphify 0.9.99, #1566).
            if let Some(args) = node.child_by_field_name("arguments") {
                for arg in children(args).filter(|a| a.kind() == "selector_expression") {
                    let (Some(operand), Some(field)) = (
                        arg.child_by_field_name("operand"),
                        arg.child_by_field_name("field"),
                    ) else {
                        continue;
                    };
                    let (name, receiver_type, field_owner) = if operand.kind() == "identifier" {
                        let name = read_text(operand, self.source);
                        if self.imports.contains_key(&name) {
                            continue;
                        }
                        let receiver_type = typed(&*types, &name);
                        (name, receiver_type, None)
                    } else if operand.kind() == "selector_expression" {
                        // `s.field.Handle`, typed through the field only when
                        // `s` is this method's receiver (Graphify 0.9.101).
                        let inner = operand.child_by_field_name("operand");
                        if !(self_bound
                            && inner.is_some_and(|i| {
                                i.kind() == "identifier"
                                    && Some(read_text(i, self.source).as_str()) == self_name.as_deref()
                            }))
                        {
                            continue;
                        }
                        let name = operand
                            .child_by_field_name("field")
                            .map(|f| read_text(f, self.source))
                            .unwrap_or_default();
                        (name, None, scope.self_type.clone())
                    } else {
                        continue;
                    };
                    let bound = field_owner.is_none() && types.contains_key(&name);
                    self.raw_calls.push(json!({"caller_nid":caller,"callee":read_text(field, self.source),"is_member_call":true,"indirect":true,"context":"argument","language":"go","receiver":null,"member_receiver":name,"receiver_type":receiver_type,"receiver_bound":bound,"field_owner":field_owner,"import_path":null,"source_file":self.path,"source_location":format!("L{}",arg.start_position().row+1)}));
                }
            }
        }
        for n in children(node) {
            self.calls(n, caller, scope, types);
            if matches!(n.kind(), "short_var_declaration" | "var_spec" | "range_clause") {
                for (name, ty) in receiver_type_table(n, self.source, false) {
                    types.insert(name, ty);
                }
            }
        }
    }
}
pub fn extract_go(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(e) => return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)}),
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&tree_sitter_go::LANGUAGE.into()) {
        return json!({"nodes":[],"edges":[],"error":e.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Go parse cancelled"});
    };
    let text = fsutil::path_string(path);
    let file = make_id([text.as_str()]);
    let mut parts = path_syntax::posix_parts(&text);
    let name = path_syntax::posix_name(&parts).to_string();
    if !name.is_empty() {
        parts.pop();
    }
    let stem = file_stem(path);
    let scope = path_syntax::posix_name(&parts);
    let scope = if scope.is_empty() {
        stem.clone()
    } else {
        scope.to_string()
    };
    let mut ex = Extractor {
        source: &source,
        path: text,
        stem,
        scope,
        file: file.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        decl: crate::ids::DeclIdRegistry::new([file.as_str()]),
        bodies: vec![],
        callable: HashSet::new(),
        callable_class: HashSet::new(),
        interfaces: HashSet::new(),
        local_type_names: HashSet::new(),
        imports: Map::new(),
        groups: HashMap::new(),
        labels: HashMap::new(),
        call_pairs: HashSet::new(),
        raw_calls: vec![],
    };
    ex.node(&file, &name, 1);
    ex.scan_types(tree.root_node());
    ex.scan(tree.root_node());
    ex.walk(tree.root_node());
    // A type carries both markers: `_callable_class` narrows `_callable`. A
    // type also carries its import path from go.mod, so a `pkg.T` receiver
    // binds only to a `T` declared at the path the caller imports `pkg` from
    // (Graphify 0.9.102, #3368), and an interface says it is one.
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let package_path = crate::extractors::resolution_types::go_import_path_for_file(
        &absolute,
        absolute.parent().unwrap_or(Path::new("")),
    );
    for n in &mut ex.nodes {
        let id = n["id"].as_str().unwrap_or("");
        let class = ex.callable_class.contains(id);
        let interface = ex.interfaces.contains(id);
        if class || ex.callable.contains(id) {
            n["_callable"] = json!(true);
            if class {
                n["_callable_class"] = json!(true);
                if let Some(package_path) = package_path.as_deref() {
                    n["_go_package_path"] = json!(package_path);
                }
            }
        }
        if interface {
            n["metadata"] = json!({"language": "go", "kind": "interface"});
        }
    }
    // The field types of the structs this file declares ride on the file
    // node, keyed by struct id, so a method in a sibling file can type
    // `s.field` (Graphify 0.9.99, #3368). A `pkg.T` field is stored as
    // `<import path>.T`, settled by this file's imports (0.9.102).
    let mut struct_fields = BTreeMap::new();
    for (name, fields) in struct_field_types(tree.root_node(), &source) {
        let mut typed = BTreeMap::new();
        for (field, type_name) in fields {
            match type_name.split_once('.') {
                None => {
                    typed.insert(field, type_name);
                }
                Some((alias, bare)) => {
                    if let Some(path) = ex.imports.get(alias).and_then(Value::as_str) {
                        typed.insert(field, format!("{path}.{bare}"));
                    }
                }
            }
        }
        if !typed.is_empty() {
            struct_fields.insert(ex.type_symbol(&name), typed);
        }
    }
    if !struct_fields.is_empty()
        && let Some(file_node) = ex.nodes.iter_mut().find(|n| n["id"] == file.as_str())
    {
        file_node["_go_fields"] = json!(struct_fields);
    }
    let type_table = receiver_type_table(tree.root_node(), &source, false);
    let package_vars = receiver_type_table(tree.root_node(), &source, true);
    ex.local_type_names = ex
        .nodes
        .iter()
        .filter(|n| n["id"].as_str().is_some_and(|id| ex.callable_class.contains(id)))
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect();
    // A method is reached through its receiver, never by bare name
    // (Graphify 0.9.102).
    let methods = ex
        .edges
        .iter()
        .filter(|e| e["relation"] == "method")
        .filter_map(|e| e["target"].as_str().map(str::to_string))
        .collect::<HashSet<_>>();
    ex.labels = ex
        .nodes
        .iter()
        .filter(|n| n["id"].as_str().is_none_or(|id| !methods.contains(id)))
        .filter_map(|n| {
            Some((
                n["label"]
                    .as_str()?
                    .trim_matches(['(', ')'])
                    .trim_start_matches('.')
                    .to_string(),
                n["id"].as_str()?.into(),
            ))
        })
        .collect();
    for scope in std::mem::take(&mut ex.bodies) {
        let mut types = scope.scope_types.clone();
        ex.calls(scope.body, &scope.caller, &scope, &mut types);
    }
    ex.edges.retain(|e| {
        e["source"].as_str().is_some_and(|s| ex.seen.contains(s))
            && (e["target"].as_str().is_some_and(|s| ex.seen.contains(s))
                || matches!(e["relation"].as_str(), Some("imports" | "imports_from")))
    });
    let path = ex.path.clone();
    json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls,"go_imports":ex.imports,
        "go_type_table":{"path":path,"table":type_table,"package_vars":package_vars}})
}
#[cfg(test)]
mod tests {
    #[test]
    fn grammar_matches_reference() {
        let l: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 219);
        assert_eq!(l.parse_state_count(), 1442);
        assert_eq!(l.abi_version(), 15);
    }
}
