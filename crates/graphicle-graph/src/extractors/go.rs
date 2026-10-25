// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/go.py
//! Go package-scoped types, case-stable symbols, signatures and call candidates.
use super::base::{file_stem, is_language_builtin, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax};
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
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
/// (`Greeter`, `*Greeter`), or None for a slice, map, channel, qualified or
/// predeclared type.
pub fn single_type_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let mut node = node?;
    if node.kind() == "pointer_type" {
        node = children(node).find(|n| n.is_named())?;
    }
    if node.kind() != "type_identifier" {
        return None;
    }
    let name = read_text(node, source);
    (!name.is_empty() && !PREDECLARED_TYPES.contains(&name.as_str())).then_some(name)
}
/// `_go_receiver_type_table`: `name -> TypeName` for every Go receiver whose
/// type is written down (struct field, parameter incl. the method receiver,
/// `var x T`, `x := T{}` / `x := &T{}`), flat over `root`, first binding in
/// source order wins.
pub fn receiver_type_table(root: Node<'_>, source: &[u8]) -> Map<String, Value> {
    let mut table = Map::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "field_declaration" | "parameter_declaration" | "var_spec" => {
                if let Some(type_name) = single_type_name(n.child_by_field_name("type"), source) {
                    for c in children(n)
                        .filter(|c| matches!(c.kind(), "field_identifier" | "identifier"))
                    {
                        table
                            .entry(read_text(c, source))
                            .or_insert_with(|| json!(type_name));
                    }
                }
            }
            "short_var_declaration" => {
                if let (Some(left), Some(right)) = (
                    n.child_by_field_name("left"),
                    n.child_by_field_name("right"),
                ) {
                    let names = children(left).filter(|c| c.kind() == "identifier");
                    let values = children(right).filter(|c| c.is_named());
                    for (name, mut value) in names.zip(values) {
                        if value.kind() == "unary_expression" {
                            value = value.child_by_field_name("operand").unwrap_or(value);
                        }
                        if value.kind() != "composite_literal" {
                            continue;
                        }
                        if let Some(type_name) =
                            single_type_name(value.child_by_field_name("type"), source)
                        {
                            table
                                .entry(read_text(name, source))
                                .or_insert_with(|| json!(type_name));
                        }
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
    bodies: Vec<Body<'tree>>,
    // `_callable` / `_callable_class` stamps (#2438, #3152).
    callable: HashSet<String>,
    callable_class: HashSet<String>,
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
    fn type_symbol(&self, name: &str) -> String {
        let plain = make_id([self.scope.as_str(), name]);
        if self
            .groups
            .get(&plain)
            .is_some_and(|names| names.contains(name))
        {
            self.symbol(plain, name)
        } else {
            plain
        }
    }
    fn symbol(&self, plain: String, name: &str) -> String {
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
                        let scope_types = receiver_type_table(node, self.source);
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
                    let scope_types = receiver_type_table(node, self.source);
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
    fn calls(&mut self, node: Node<'_>, caller: &str, scope: &Body<'_>) {
        if matches!(node.kind(), "function_declaration" | "method_declaration") {
            return;
        }
        if node.kind() == "call_expression" {
            let mut callee = None;
            let mut member = false;
            let mut bare = false;
            let mut receiver = None;
            let mut member_receiver = None;
            let mut member_receiver_type = None;
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
                            // The method's own receiver is typed by its
                            // declaration; other names by the function's scope.
                            member_receiver_type = if scope
                                .self_name
                                .as_deref()
                                .is_some_and(|n| !n.is_empty() && n == operand)
                            {
                                scope.self_type.clone()
                            } else {
                                scope
                                    .scope_types
                                    .get(&operand)
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            };
                            member_receiver = Some(operand.clone());
                        } else if op.kind() == "selector_expression" {
                            // `s.logger.Log()` names the receiver by the field
                            // only when `s` is this method's own receiver.
                            let inner = op.child_by_field_name("operand");
                            if let (Some(self_name), Some(inner)) =
                                (scope.self_name.as_deref().filter(|n| !n.is_empty()), inner)
                                && inner.kind() == "identifier"
                                && read_text(inner, self.source) == self_name
                            {
                                member_receiver = Some(
                                    op.child_by_field_name("field")
                                        .map(|f| read_text(f, self.source))
                                        .unwrap_or_default(),
                                );
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
                let target = if import_path.as_ref().is_some_and(|s| !s.is_empty()) {
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
                    self.raw_calls.push(json!({"caller_nid":caller,"callee":callee,"is_member_call":member,"language":"go","receiver":receiver,"member_receiver":member_receiver,"receiver_type":member_receiver_type,"import_path":import_path,"source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)}));
                }
            }
        }
        for n in children(node) {
            self.calls(n, caller, scope);
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
        bodies: vec![],
        callable: HashSet::new(),
        callable_class: HashSet::new(),
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
    // A type carries both markers: `_callable_class` narrows `_callable`.
    for n in &mut ex.nodes {
        let id = n["id"].as_str().unwrap_or("");
        let class = ex.callable_class.contains(id);
        if class || ex.callable.contains(id) {
            n["_callable"] = json!(true);
            if class {
                n["_callable_class"] = json!(true);
            }
        }
    }
    let type_table = receiver_type_table(tree.root_node(), &source);
    ex.labels = ex
        .nodes
        .iter()
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
        ex.calls(scope.body, &scope.caller, &scope);
    }
    ex.edges.retain(|e| {
        e["source"].as_str().is_some_and(|s| ex.seen.contains(s))
            && (e["target"].as_str().is_some_and(|s| ex.seen.contains(s))
                || matches!(e["relation"].as_str(), Some("imports" | "imports_from")))
    });
    let path = ex.path.clone();
    json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls,"go_imports":ex.imports,
        "go_type_table":{"path":path,"table":type_table}})
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
