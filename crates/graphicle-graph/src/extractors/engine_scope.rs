// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
//! Binding and receiver scopes from graphify/extractors/engine.py.
use super::{base::read_text, engine_types::*, models::LanguageConfig};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;
pub fn python_collect_assignment_targets(
    node: Option<Node<'_>>,
    source: &[u8],
    out: &mut BTreeSet<String>,
) {
    let Some(n) = node else { return };
    if n.kind_name() == "identifier" {
        out.insert(read_text(n, source));
    } else if matches!(
        n.kind_name(),
        "pattern_list" | "tuple_pattern" | "list_pattern"
    ) {
        for c in children(n) {
            python_collect_assignment_targets(Some(c), source, out)
        }
    }
}
fn python_bind_walk(n: Node<'_>, source: &[u8], out: &mut BTreeSet<String>, with_alias: bool) {
    for child in children(n) {
        if matches!(
            child.kind_name(),
            "function_definition" | "class_definition" | "lambda"
        ) {
            continue;
        }
        match child.kind_name() {
            "assignment" | "for_statement" | "for_in_clause" => {
                python_collect_assignment_targets(child.child_by_field_name("left"), source, out)
            }
            "named_expression" => {
                python_collect_assignment_targets(child.child_by_field_name("name"), source, out)
            }
            "with_statement" if with_alias => {
                for clause in children(child)
                    .into_iter()
                    .filter(|c| c.kind_name() == "with_clause")
                {
                    for item in children(clause)
                        .into_iter()
                        .filter(|c| c.kind_name() == "with_item")
                    {
                        python_collect_assignment_targets(
                            item.child_by_field_name("alias"),
                            source,
                            out,
                        )
                    }
                }
            }
            _ => {}
        }
        python_bind_walk(child, source, out, with_alias);
    }
}
pub fn python_local_bound_names(n: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut out = python_param_names(n.child_by_field_name("parameters"), source);
    if let Some(body) = n.child_by_field_name("body") {
        python_bind_walk(body, source, &mut out, true)
    }
    out
}
pub fn python_module_bound_names(root: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    python_bind_walk(root, source, &mut out, false);
    out
}
fn upper_initial(s: &str) -> bool {
    s.chars().next().is_some_and(char::is_uppercase)
}
/// Python class bindings of one scope: `name -> Some(Class)` when every
/// binding names that one class, `None` when the name is bound otherwise.
pub type PythonBindings = BTreeMap<String, Option<String>>;
/// `_python_self_attr` (0.9.80, #2860): `X` when `node` is `self.X`.
pub fn python_self_attr(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let node = node.filter(|n| n.kind_name() == "attribute")?;
    let obj = node.child_by_field_name("object")?;
    let attr = node.child_by_field_name("attribute")?;
    (read_text(obj, source) == "self").then(|| read_text(attr, source))
}
/// `_python_self_attr_bindings` (0.9.80, #2860): `(attr, class)` for every
/// `self.attr = ...` in a method body. The class is the annotation
/// (`self.attr: T = ...`) or the bare class of `self.attr = Ctor(...)`, and
/// None for any other value so the caller can poison the field;
/// `self.attr = None` binds nothing.
pub fn python_self_attr_bindings(body: Node<'_>, source: &[u8]) -> Vec<(String, Option<String>)> {
    let mut out = vec![];
    let mut stack = named(body);
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind_name(),
            "function_definition" | "class_definition" | "lambda"
        ) {
            continue;
        }
        stack.extend(named(node));
        if node.kind_name() != "assignment" {
            continue;
        }
        let Some(left) = node.child_by_field_name("left") else {
            continue;
        };
        let mut value = node.child_by_field_name("type");
        if value.is_none()
            && let Some(right) = node.child_by_field_name("right")
        {
            if right.kind_name() == "none" {
                continue;
            }
            value = if right.kind_name() == "call" {
                right.child_by_field_name("function")
            } else {
                None
            };
        }
        let name = value.map(|v| read_text(v, source)).unwrap_or_default();
        let typed = crate::extract_python::is_identifier(&name) && upper_initial(&name);
        // Only a whole-target assignment types the field; every element of a
        // tuple unpacking is poisoned.
        let whole = left.kind_name() != "pattern_list";
        let targets = if whole { vec![left] } else { named(left) };
        for target in targets {
            if let Some(attr) = python_self_attr(Some(target), source).filter(|a| !a.is_empty()) {
                out.push((attr, (typed && whole).then(|| name.clone())));
            }
        }
    }
    out
}
/// `_python_local_class_bindings` (0.9.80, #4198): `name -> Class` for the
/// parameters and locals of one Python function, from a class annotation, a
/// constructor call or a `with Ctor(...) as name`. The table is
/// flow-insensitive, so a name bound to anything else anywhere in the
/// function maps to None; names bound inside a lambda or a nested class body
/// are poisoned, never typed.
pub fn python_local_class_bindings(body: Node<'_>, source: &[u8]) -> PythonBindings {
    struct Scan<'s> {
        source: &'s [u8],
        bindings: PythonBindings,
        poison_only: bool,
    }
    impl Scan<'_> {
        fn text(&self, n: Node<'_>) -> String {
            read_text(n, self.source)
        }
        fn bind(&mut self, name: String, class: Option<String>) {
            match class {
                Some(class)
                    if !self.poison_only
                        && self
                            .bindings
                            .get(&name)
                            .is_none_or(|v| v.as_ref() == Some(&class)) =>
                {
                    self.bindings.entry(name).or_insert(Some(class));
                }
                _ => {
                    self.bindings.insert(name, None);
                }
            }
        }
        fn poison(&mut self, target: Option<Node<'_>>) {
            let mut names = BTreeSet::new();
            python_collect_assignment_targets(target, self.source, &mut names);
            for name in names {
                self.bindings.insert(name, None);
            }
        }
        fn poison_identifiers(&mut self, node: Node<'_>) {
            let mut stack = vec![node];
            while let Some(cur) = stack.pop() {
                if cur.kind_name() == "identifier" {
                    let name = self.text(cur);
                    self.bindings.insert(name, None);
                }
                stack.extend(children(cur));
            }
        }
        fn visit(&mut self, n: Node<'_>) {
            for child in children(n) {
                match child.kind_name() {
                    "function_definition" => {
                        // Its own caller, but `nonlocal c` inside it can rebind
                        // this function's `c`.
                        let mut stack = vec![child];
                        while let Some(cur) = stack.pop() {
                            if cur.kind_name() == "nonlocal_statement" {
                                self.poison_identifiers(cur);
                            }
                            stack.extend(children(cur));
                        }
                        continue;
                    }
                    "lambda" | "class_definition" => {
                        if child.kind_name() == "lambda"
                            && let Some(params) = child.child_by_field_name("parameters")
                        {
                            self.poison_identifiers(params);
                        }
                        let outer = std::mem::replace(&mut self.poison_only, true);
                        self.visit(child);
                        self.poison_only = outer;
                        continue;
                    }
                    "case_pattern" | "import_statement" | "import_from_statement" => {
                        self.poison_identifiers(child);
                        continue;
                    }
                    "assignment" => {
                        let left = child.child_by_field_name("left");
                        match left.filter(|l| l.kind_name() == "identifier") {
                            Some(left) => {
                                let name = self.text(left);
                                let annotation = child.child_by_field_name("type");
                                let right = child.child_by_field_name("right");
                                if annotation.is_some() {
                                    let class = python_class_ref(annotation, self.source);
                                    self.bind(name, class);
                                } else if let Some(right) =
                                    right.filter(|r| r.kind_name() == "call")
                                {
                                    let class = python_class_ref(
                                        right.child_by_field_name("function"),
                                        self.source,
                                    );
                                    self.bind(name, class);
                                } else {
                                    self.bindings.insert(name, None);
                                }
                            }
                            None => self.poison(left),
                        }
                    }
                    "augmented_assignment" => self.poison(child.child_by_field_name("left")),
                    "as_pattern" => {
                        let alias = child.child_by_field_name("alias");
                        let target = alias.and_then(|a| first_child(a, &["identifier"]));
                        let value = named(child).into_iter().next();
                        if let Some(target) = target {
                            let name = self.text(target);
                            if child.parent().is_some_and(|p| p.kind_name() == "with_item")
                                && let Some(value) = value.filter(|v| v.kind_name() == "call")
                            {
                                let class = python_class_ref(
                                    value.child_by_field_name("function"),
                                    self.source,
                                );
                                self.bind(name, class);
                            } else {
                                // `except E as e`, other patterns: not a constructor result.
                                self.bindings.insert(name, None);
                            }
                        } else if let Some(alias) = alias {
                            self.poison_identifiers(alias);
                        }
                    }
                    "for_statement" | "for_in_clause" => {
                        self.poison(child.child_by_field_name("left"))
                    }
                    "named_expression" => self.poison(child.child_by_field_name("name")),
                    "global_statement" | "nonlocal_statement" => {
                        for c in children(child) {
                            if c.kind_name() == "identifier" {
                                let name = self.text(c);
                                self.bindings.insert(name, None);
                            }
                        }
                    }
                    _ => {}
                }
                self.visit(child);
            }
        }
    }
    let mut scan = Scan {
        source,
        bindings: PythonBindings::new(),
        poison_only: false,
    };
    let params = body
        .parent()
        .filter(|f| f.kind_name() == "function_definition")
        .and_then(|f| f.child_by_field_name("parameters"));
    for name in python_param_names(params, source) {
        // An untyped parameter: unknown, and a later assignment must not type it.
        scan.bindings.insert(name, None);
    }
    if let Some(params) = params {
        for child in children(params) {
            if !matches!(
                child.kind_name(),
                "typed_parameter" | "typed_default_parameter"
            ) {
                continue;
            }
            if let Some(name) = child
                .child_by_field_name("name")
                .or_else(|| first_child(child, &["identifier"]))
            {
                let name = read_text(name, source);
                scan.bindings.remove(&name);
                let class = python_class_ref(child.child_by_field_name("type"), source);
                scan.bind(name, class);
            }
        }
    }
    scan.visit(body);
    scan.bindings
}
/// `_python_is_property` (0.9.87, #4337): `@property`, `@cached_property` or
/// `@x.getter`; reading the attribute runs the function.
pub fn python_is_property(func: Node<'_>, source: &[u8]) -> bool {
    let Some(parent) = func
        .parent()
        .filter(|p| p.kind_name() == "decorated_definition")
    else {
        return false;
    };
    children(parent)
        .into_iter()
        .filter(|d| d.kind_name() == "decorator")
        .any(|decorator| {
            let text = named(decorator)
                .into_iter()
                .next()
                .map(|e| read_text(e, source))
                .unwrap_or_default();
            matches!(
                text.as_str(),
                "property" | "cached_property" | "functools.cached_property"
            ) || text.ends_with(".getter")
        })
}
/// `_same_node`: nodes compared by span and type, as upstream does.
fn same_node(a: Option<Node<'_>>, b: Option<Node<'_>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            (a.start_byte(), a.end_byte(), a.kind_name())
                == (b.start_byte(), b.end_byte(), b.kind_name())
        }
        _ => false,
    }
}
/// `_python_attribute_is_store`: `x.a = ...`, `for x.a in ...` and `del x.a`
/// write `a`; `x.a += ...` reads it first.
fn python_attribute_is_store(node: Node<'_>) -> bool {
    let mut child = node;
    let mut parent = node.parent();
    while let Some(p) = parent.filter(|p| {
        matches!(
            p.kind_name(),
            "pattern_list" | "tuple_pattern" | "list_pattern"
        )
    }) {
        child = p;
        parent = p.parent();
    }
    let Some(parent) = parent else { return false };
    if matches!(
        parent.kind_name(),
        "assignment" | "for_statement" | "for_in_clause"
    ) {
        return same_node(parent.child_by_field_name("left"), Some(child));
    }
    parent.kind_name() == "delete_statement"
}
/// `_python_property_reads` (0.9.87, #4337): raw calls for attribute reads
/// that may run a `@property` getter, recorded only where the receiver is
/// typed the way a member call's would be: `self.a` (minus the fields the
/// class assigns), a typed local or parameter, and `self.X.a` with `X` bound
/// to one class. The resolver keeps only targets marked `_py_property`.
pub fn python_property_reads(
    bodies: &[(String, Node<'_>)],
    source: &[u8],
    path: &str,
    var_types: &BTreeMap<String, PythonBindings>,
    attr_types: &BTreeMap<String, PythonBindings>,
    method_owner: &BTreeMap<String, String>,
) -> Vec<serde_json::Value> {
    use serde_json::json;
    let empty = PythonBindings::new();
    let mut out = vec![];
    for (caller, body) in bodies {
        let owner = method_owner.get(caller).map(String::as_str).unwrap_or("");
        let fields = attr_types.get(owner).unwrap_or(&empty);
        let mut stack = children(*body);
        while let Some(node) = stack.pop() {
            if matches!(
                node.kind_name(),
                "function_definition" | "class_definition" | "lambda"
            ) {
                continue;
            }
            stack.extend(children(node));
            if node.kind_name() != "attribute" {
                continue;
            }
            // A method call: the call walk owns it.
            if node.parent().is_some_and(|p| {
                p.kind_name() == "call" && same_node(p.child_by_field_name("function"), Some(node))
            }) || python_attribute_is_store(node)
            {
                continue;
            }
            let (Some(obj), Some(attr)) = (
                node.child_by_field_name("object"),
                node.child_by_field_name("attribute"),
            ) else {
                continue;
            };
            let attr = read_text(attr, source);
            let mut entry = json!({
                "caller_nid": caller,
                "callee": attr,
                "is_member_call": true,
                "source_file": path,
                "source_location": format!("L{}", node.start_position().row + 1),
                "_python_attr_read": true,
            });
            if obj.kind_name() == "identifier" {
                let name = read_text(obj, source);
                if name == "self" {
                    if owner.is_empty() || fields.contains_key(&attr) {
                        continue;
                    }
                    entry["receiver"] = json!("self");
                } else {
                    let Some(class) = var_types
                        .get(caller)
                        .and_then(|t| t.get(&name))
                        .cloned()
                        .flatten()
                    else {
                        continue;
                    };
                    entry["receiver"] = json!(name);
                    entry["receiver_type"] = json!(class);
                }
            } else {
                let Some(self_attr) = python_self_attr(Some(obj), source).filter(|a| !a.is_empty())
                else {
                    continue;
                };
                let Some(class) = fields.get(&self_attr).cloned().flatten() else {
                    continue;
                };
                entry["receiver"] = json!(format!("self.{self_attr}"));
                entry["_python_self_attr_type"] = json!(class);
            }
            out.push(entry);
        }
    }
    out
}
/// `_python_under_type_checking` (0.9.80, #4179): `node` sits in the body of
/// an `if TYPE_CHECKING:` (or `if <module>.TYPE_CHECKING:`) block. The
/// `else:` branch and `if not TYPE_CHECKING:` run at import time.
pub fn python_under_type_checking(node: Node<'_>, source: &[u8]) -> bool {
    let mut child = node;
    let mut parent = node.parent();
    while let Some(p) = parent {
        if matches!(p.kind_name(), "if_statement" | "elif_clause")
            && p.child_by_field_name("consequence") == Some(child)
        {
            let mut cond = p.child_by_field_name("condition");
            if let Some(c) = cond.filter(|c| c.kind_name() == "attribute") {
                cond = c.child_by_field_name("attribute");
            }
            if cond.is_some_and(|c| {
                c.kind_name() == "identifier" && read_text(c, source) == "TYPE_CHECKING"
            }) {
                return true;
            }
        }
        child = p;
        parent = p.parent();
    }
    false
}
pub fn js_collect_pattern_idents(n: Node<'_>, source: &[u8], bound: &mut BTreeSet<String>) {
    match n.kind_name() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            bound.insert(read_text(n, source));
        }
        "type_annotation" => {}
        "assignment_pattern" => {
            if let Some(left) = n.child_by_field_name("left") {
                js_collect_pattern_idents(left, source, bound)
            }
        }
        "pair_pattern" => {
            if let Some(value) = n.child_by_field_name("value") {
                js_collect_pattern_idents(value, source, bound)
            }
        }
        _ => {
            for c in named(n) {
                js_collect_pattern_idents(c, source, bound)
            }
        }
    }
}
fn js_bind_walk(n: Node<'_>, source: &[u8], bound: &mut BTreeSet<String>, module: bool) {
    for c in children(n) {
        if set("_JS_SCOPE_BOUNDARY").contains(c.kind_name()) {
            continue;
        }
        if c.kind_name() == "variable_declarator" {
            let collect = if module {
                c.child_by_field_name("value")
                    .is_none_or(|n| !set("_JS_FUNCTION_VALUE_TYPES").contains(n.kind_name()))
            } else {
                n.kind_name() == "variable_declaration"
            };
            if collect && let Some(name) = c.child_by_field_name("name") {
                js_collect_pattern_idents(name, source, bound)
            }
        } else if !module
            && c.kind_name() == "for_in_statement"
            && children(c).iter().any(|n| n.kind_name() == "var")
            && let Some(left) = c.child_by_field_name("left")
        {
            js_collect_pattern_idents(left, source, bound);
        }
        js_bind_walk(c, source, bound, module);
    }
}
pub fn js_local_bound_names(n: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut bound = BTreeSet::new();
    for field in ["parameters", "parameter"] {
        if let Some(p) = n.child_by_field_name(field) {
            js_collect_pattern_idents(p, source, &mut bound)
        }
    }
    if let Some(body) = n.child_by_field_name("body") {
        js_bind_walk(body, source, &mut bound, false)
    }
    bound
}
pub fn js_direct_lexical_names(n: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for child in children(n)
        .into_iter()
        .filter(|c| c.kind_name() == "lexical_declaration")
    {
        for declarator in children(child)
            .into_iter()
            .filter(|c| c.kind_name() == "variable_declarator")
        {
            if let Some(name) = declarator.child_by_field_name("name") {
                js_collect_pattern_idents(name, source, &mut out)
            }
        }
    }
    out
}
pub fn js_module_bound_names(root: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    js_bind_walk(root, source, &mut out, true);
    out
}
pub fn js_dispatch_value_idents(n: Node<'_>) -> Vec<Node<'_>> {
    children(n)
        .into_iter()
        .filter_map(|c| {
            if n.kind_name() == "object" {
                match c.kind_name() {
                    "pair" => c
                        .child_by_field_name("value")
                        .filter(|v| v.kind_name() == "identifier"),
                    "shorthand_property_identifier" => Some(c),
                    _ => None,
                }
            } else {
                (c.kind_name() == "identifier").then_some(c)
            }
        })
        .collect()
}
pub fn find_body<'a>(n: Node<'a>, config: &LanguageConfig) -> Option<Node<'a>> {
    n.child_by_field_name(&config.body_field).or_else(|| {
        children(n).into_iter().find(|c| {
            config
                .body_fallback_child_types
                .iter()
                .any(|t| t == c.kind_name())
        })
    })
}
pub fn get_cpp_func_name(n: Node<'_>, source: &[u8]) -> Option<String> {
    if matches!(
        n.kind_name(),
        "identifier"
            | "field_identifier"
            | "destructor_name"
            | "operator_name"
            | "qualified_identifier"
    ) {
        return Some(read_text(n, source));
    }
    if let Some(decl) = n.child_by_field_name("declarator") {
        return get_cpp_func_name(decl, source);
    }
    first_child(n, &["identifier"]).map(|n| read_text(n, source))
}
pub fn cpp_local_var_types(body: Node<'_>, source: &[u8], table: &mut BTreeMap<String, String>) {
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if matches!(n.kind_name(), "function_definition" | "lambda_expression") && n != body {
            continue;
        }
        if n.kind_name() == "declaration"
            && let Some(type_node) = n
                .child_by_field_name("type")
                .filter(|t| matches!(t.kind_name(), "type_identifier" | "qualified_identifier"))
        {
            let name = read_text(type_node, source)
                .rsplit("::")
                .next()
                .unwrap_or("")
                .trim_matches(crate::dedup_strings::whitespace)
                .to_owned();
            let declarators: Vec<_> = children(n)
                .into_iter()
                .filter(|c| {
                    matches!(
                        c.kind_name(),
                        "identifier"
                            | "pointer_declarator"
                            | "reference_declarator"
                            | "init_declarator"
                    )
                })
                .collect();
            if name.chars().next().is_some_and(char::is_uppercase)
                && declarators.len() == 1
                && let Some(var) =
                    super::engine_declarator::cpp_declarator_name(declarators[0], source)
                        .filter(|s| !s.is_empty())
            {
                table.entry(var).or_insert(name);
            }
        }
        stack.extend(children(n));
    }
}
pub fn swift_local_var_types(
    body: Node<'_>,
    source: &[u8],
    table: &mut BTreeMap<String, String>,
    mut factory: Option<&mut BTreeMap<String, (String, String)>>,
) {
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        if n.kind_name() == "function_declaration" && n != body {
            continue;
        }
        if n.kind_name() == "property_declaration" {
            let mut prop_type = None;
            let mut factory_bind = None;
            for c in children(n) {
                if c.kind_name() == "call_expression" {
                    prop_type = swift_constructor_type(c, source);
                    if prop_type.is_none() {
                        factory_bind = swift_factory_call(c, source)
                    }
                    break;
                }
                if c.kind_name() == "navigation_expression" {
                    if let Some(head) = c.child(0).filter(|n| n.kind_name() == "simple_identifier")
                    {
                        let text = read_text(head, source);
                        if text.chars().next().is_some_and(char::is_uppercase) {
                            prop_type = Some(text);
                        }
                    }
                    break;
                }
            }
            if let Some(name) = swift_property_name(n, source).filter(|s| !s.is_empty())
                && !table.contains_key(&name)
            {
                if let Some(t) = prop_type.filter(|s| !s.is_empty()) {
                    table.insert(name, t);
                } else if let (Some(value), Some(factory)) = (factory_bind, factory.as_deref_mut())
                {
                    factory.entry(name).or_insert(value);
                }
            }
        }
        stack.extend(children(n));
    }
}
pub fn csharp_bare_call_name(node: Node<'_>, source: &[u8]) -> String {
    read_text(
        if node.kind_name() == "generic_name" {
            first_child(node, &["identifier"]).unwrap_or(node)
        } else {
            node
        },
        source,
    )
}
pub fn read_csharp_type_name(
    node: Option<Node<'_>>,
    source: &[u8],
) -> Option<(String, bool, String)> {
    let n = node?;
    match n.kind_name() {
        "identifier" | "predefined_type" => {
            return Some((read_text(n, source), false, String::new()));
        }
        "qualified_name" => {
            let raw = read_text(n, source);
            let (prefix, tail) = raw.rsplit_once('.').unwrap_or(("", &raw));
            return Some((
                tail.split('<').next().unwrap_or("").into(),
                true,
                prefix.into(),
            ));
        }
        "generic_name" => {
            if let Some(name) = n.child_by_field_name("name") {
                let qualified = name.kind_name() == "qualified_name";
                let raw = read_text(name, source);
                let (prefix, tail) = raw.rsplit_once('.').unwrap_or(("", &raw));
                return Some((
                    tail.into(),
                    qualified,
                    if qualified {
                        prefix.into()
                    } else {
                        String::new()
                    },
                ));
            }
        }
        _ => {}
    }
    named(n)
        .into_iter()
        .find_map(|c| read_csharp_type_name(Some(c), source))
}
pub fn csharp_receiver_type_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let (name, _, _) = read_csharp_type_name(node, source)?;
    name.chars()
        .next()
        .is_some_and(char::is_uppercase)
        .then_some(name)
}
/// `_csharp_element_type_name` (0.9.83, #4246): the element type of a C#
/// `T[]` / `List<T>` / `IList<T>` / `IReadOnlyList<T>`, so `xs[i].M()` reads
/// a `T`. A jagged array, a nested collection, a type parameter or any other
/// indexer names no single element class.
pub fn csharp_element_type_name(type_node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    const LISTS: [&str; 3] = ["List", "IList", "IReadOnlyList"];
    let node = type_node?;
    let elem = match node.kind_name() {
        "array_type" => node.child_by_field_name("type"),
        "generic_name" => {
            let (name, _, _) = read_csharp_type_name(Some(node), source)?;
            if !LISTS.contains(&name.as_str()) {
                return None;
            }
            let args = first_child(node, &["type_argument_list"])
                .map(named)
                .unwrap_or_default();
            if args.len() == 1 { Some(args[0]) } else { None }
        }
        _ => return None,
    };
    let elem = elem.filter(|e| {
        matches!(
            e.kind_name(),
            "identifier" | "qualified_name" | "generic_name"
        )
    })?;
    let name = csharp_receiver_type_name(Some(elem), source)?;
    if LISTS.contains(&name.as_str())
        || csharp_type_parameters_in_scope(node, source).contains(&name)
    {
        return None;
    }
    Some(name)
}
const CSHARP_LOOP_SEQUENCES: &[&str] = &[
    "List",
    "IList",
    "IReadOnlyList",
    "IEnumerable",
    "ICollection",
    "IReadOnlyCollection",
    "HashSet",
    "ISet",
    "SortedSet",
    "Queue",
    "Stack",
    "LinkedList",
];
const CSHARP_LOOP_DICTIONARIES: &[&str] = &[
    "Dictionary",
    "IDictionary",
    "IReadOnlyDictionary",
    "SortedDictionary",
    "SortedList",
    "ConcurrentDictionary",
];
/// `_csharp_loop_element_types` (0.9.90): what a `foreach` over a field or
/// property of this type yields, by iterated suffix: `""` for `T[]` or a
/// sequence of T, `.Keys`/`.Values` for a dictionary's keys and values.
/// Iterating a dictionary itself (`KeyValuePair`), nested collections, type
/// parameters and primitives are left out.
pub fn csharp_loop_element_types(
    type_node: Option<Node<'_>>,
    source: &[u8],
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(node) = type_node else { return out };
    let (args, suffixes): (Vec<Option<Node<'_>>>, &[&str]) = match node.kind_name() {
        "array_type" => (vec![node.child_by_field_name("type")], &[""]),
        "generic_name" => {
            let info = read_csharp_type_name(Some(node), source);
            let args = first_child(node, &["type_argument_list"])
                .map(named)
                .unwrap_or_default()
                .into_iter()
                .map(Some)
                .collect::<Vec<_>>();
            let name = info.map(|(name, _, _)| name).unwrap_or_default();
            if CSHARP_LOOP_SEQUENCES.contains(&name.as_str()) && args.len() == 1 {
                (args, &[""])
            } else if CSHARP_LOOP_DICTIONARIES.contains(&name.as_str()) && args.len() == 2 {
                (args, &[".Keys", ".Values"])
            } else {
                return out;
            }
        }
        _ => return out,
    };
    let params = csharp_type_parameters_in_scope(node, source);
    for (suffix, arg) in suffixes.iter().zip(args) {
        let Some(arg) = arg.filter(|a| {
            matches!(
                a.kind_name(),
                "identifier" | "qualified_name" | "generic_name"
            )
        }) else {
            continue;
        };
        if let Some(name) = csharp_receiver_type_name(Some(arg), source).filter(|name| {
            !params.contains(name)
                && !CSHARP_LOOP_SEQUENCES.contains(&name.as_str())
                && !CSHARP_LOOP_DICTIONARIES.contains(&name.as_str())
        }) {
            out.insert(suffix.to_string(), name);
        }
    }
    out
}
/// `_csharp_loop_source`: `(field, suffix)` for a `foreach` over `f`,
/// `this.f`, `f.Values` or `f.Keys`; None for anything else.
pub fn csharp_loop_source(expr: Option<Node<'_>>, source: &[u8]) -> Option<(String, String)> {
    let expr = expr?;
    if expr.kind_name() == "identifier" {
        return Some((read_text(expr, source), String::new()));
    }
    if expr.kind_name() != "member_access_expression" {
        return None;
    }
    let inner = expr.child_by_field_name("expression")?;
    let name = expr
        .child_by_field_name("name")
        .filter(|n| n.kind_name() == "identifier")?;
    let member = read_text(name, source);
    if matches!(inner.kind_name(), "this" | "this_expression") {
        return Some((member, String::new()));
    }
    if matches!(member.as_str(), "Values" | "Keys")
        && let Some((root, suffix)) = csharp_loop_source(Some(inner), source)
        && suffix.is_empty()
    {
        return Some((root, format!(".{member}")));
    }
    None
}
/// `_csharp_return_info` (0.9.87, #4266): `[return type, type-parameter
/// count]` of a C# method. The return type is the class a receiver of that
/// type would carry, `"#i"` for the method's own i-th type parameter, or null
/// when no class can be named.
pub fn csharp_return_info(method: Node<'_>, source: &[u8]) -> serde_json::Value {
    let params: Vec<String> = method
        .child_by_field_name("type_parameters")
        .map(named)
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            p.child_by_field_name("name")
                .map(|n| read_text(n, source))
                .unwrap_or_default()
        })
        .collect();
    let returns = method.child_by_field_name("returns");
    let mut ret = csharp_receiver_type_name(returns, source);
    let own = ret
        .as_ref()
        .and_then(|r| params.iter().position(|p| p == r))
        .filter(|_| returns.is_some_and(|t| t.kind_name() == "identifier"));
    if let Some(index) = own {
        ret = Some(format!("#{index}"));
    } else if ret.as_ref().is_some_and(|r| {
        params.contains(r) || csharp_type_parameters_in_scope(method, source).contains(r)
    }) {
        ret = None;
    }
    serde_json::json!([ret, params.len()])
}
/// What a C# receiver name is bound to in one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CsharpBinding {
    Type(String),
    /// `var v = R.M<A>()` (0.9.87, #4266): receiver (an identifier, `this` or
    /// empty for an unqualified call), method and type arguments; the
    /// variable is typed cross-file from M's declared return type.
    Call(String, String, Vec<Option<String>>),
}
/// `_csharp_deferred_call` (0.9.87, #4266): the deferred binding of
/// `var x = R.M<A>(...)`, or None for any other call shape.
pub fn csharp_deferred_call(invocation: Node<'_>, source: &[u8]) -> Option<CsharpBinding> {
    let mut function = invocation.child_by_field_name("function");
    let mut receiver = String::new();
    if let Some(access) = function.filter(|f| f.kind_name() == "member_access_expression") {
        let inner = access.child_by_field_name("expression");
        function = access.child_by_field_name("name");
        match inner {
            Some(i) if matches!(i.kind_name(), "this" | "this_expression") => {
                receiver = "this".into()
            }
            Some(i) if i.kind_name() == "identifier" => receiver = read_text(i, source),
            _ => return None,
        }
    }
    let function = function.filter(|f| matches!(f.kind_name(), "identifier" | "generic_name"))?;
    let mut args = vec![];
    let name = if function.kind_name() == "generic_name" {
        if let Some(list) = first_child(function, &["type_argument_list"]) {
            args.extend(
                named(list)
                    .into_iter()
                    .map(|arg| csharp_receiver_type_name(Some(arg), source)),
            );
        }
        first_child(function, &["identifier"])
    } else {
        Some(function)
    }?;
    Some(CsharpBinding::Call(receiver, read_text(name, source), args))
}
pub type ScopedBindings = BTreeMap<String, Vec<(usize, usize, Option<CsharpBinding>)>>;
pub type CsharpReceiverTable = (ScopedBindings, BTreeMap<String, String>);
pub fn csharp_method_receiver_types(
    method: Node<'_>,
    source: &[u8],
    fields: &BTreeMap<String, String>,
) -> CsharpReceiverTable {
    struct Bindings<'a> {
        bindings: ScopedBindings,
        poisoned: BTreeSet<String>,
        fields: &'a BTreeMap<String, String>,
    }
    impl Bindings<'_> {
        /// `name[]` carries the element type for `name[i].M()` (#4246); every
        /// binding shadows it too, so an untyped local still hides a field's.
        fn bind(
            &mut self,
            name: String,
            type_name: Option<CsharpBinding>,
            scope: Node<'_>,
            elem: Option<String>,
        ) {
            if name.is_empty() {
                return;
            }
            let element = format!("{name}[]");
            for (key, value) in [(name, type_name), (element, elem.map(CsharpBinding::Type))] {
                if self
                    .fields
                    .get(&key)
                    .is_some_and(|t| value.as_ref() != Some(&CsharpBinding::Type(t.clone())))
                {
                    self.poisoned.insert(key.clone());
                }
                self.bindings.entry(key).or_default().push((
                    scope.start_byte(),
                    scope.end_byte(),
                    value,
                ));
            }
        }
        fn parameter(&mut self, n: Node<'_>, scope: Node<'_>, source: &[u8]) {
            if let Some(name) = n.child_by_field_name("name") {
                let ty = n.child_by_field_name("type");
                self.bind(
                    read_text(name, source),
                    csharp_receiver_type_name(ty, source).map(CsharpBinding::Type),
                    scope,
                    csharp_element_type_name(ty, source),
                );
            }
        }
    }
    let mut out = Bindings {
        bindings: BTreeMap::new(),
        poisoned: BTreeSet::new(),
        fields,
    };
    // `foreach (var x in field)` loops, typed once every local is bound, so a
    // local or parameter shadowing the field is seen wherever it is declared
    // (0.9.90): (loop variable, field, suffix, foreach node).
    let mut pending_loops = vec![];
    let body = method.child_by_field_name("body");
    let param_scope = body.unwrap_or(method);
    if let Some(params) = method.child_by_field_name("parameters") {
        for p in children(params)
            .into_iter()
            .filter(|p| p.kind_name() == "parameter")
        {
            out.parameter(p, param_scope, source)
        }
    }
    let mut stack: Vec<_> = body
        .map(children)
        .unwrap_or_default()
        .into_iter()
        .map(|c| (c, param_scope))
        .collect();
    while let Some((n, scope)) = stack.pop() {
        if matches!(
            n.kind_name(),
            "class_declaration"
                | "struct_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
        ) {
            continue;
        }
        match n.kind_name() {
            "lambda_expression" => {
                if let Some(params) = n.child_by_field_name("parameters") {
                    if params.kind_name() == "implicit_parameter" {
                        out.bind(read_text(params, source), None, n, None)
                    } else {
                        for p in children(params) {
                            if p.kind_name() == "parameter" {
                                out.parameter(p, n, source)
                            } else if p.kind_name() == "implicit_parameter" {
                                out.bind(read_text(p, source), None, n, None)
                            }
                        }
                    }
                }
            }
            "local_function_statement" => {
                if let Some(params) = n.child_by_field_name("parameters") {
                    for p in children(params)
                        .into_iter()
                        .filter(|p| p.kind_name() == "parameter")
                    {
                        out.parameter(p, n, source)
                    }
                }
            }
            "local_declaration_statement" => {
                if let Some(vd) = first_child(n, &["variable_declaration"]) {
                    let declared =
                        csharp_receiver_type_name(vd.child_by_field_name("type"), source);
                    let declared_elem =
                        csharp_element_type_name(vd.child_by_field_name("type"), source);
                    for decl in children(vd)
                        .into_iter()
                        .filter(|c| c.kind_name() == "variable_declarator")
                    {
                        let Some(name) = decl
                            .child_by_field_name("name")
                            .or_else(|| first_child(decl, &["identifier"]))
                        else {
                            continue;
                        };
                        // `var v = new T()` recovers T from the object creation;
                        // `var v = R.M<A>()` is typed cross-file from M's
                        // declared return type (#4266).
                        let type_name = match &declared {
                            Some(declared) => Some(CsharpBinding::Type(declared.clone())),
                            None => children(decl)
                                .into_iter()
                                .find_map(|g| match g.kind_name() {
                                    "object_creation_expression" => Some(
                                        csharp_receiver_type_name(
                                            g.child_by_field_name("type"),
                                            source,
                                        )
                                        .map(CsharpBinding::Type),
                                    ),
                                    "invocation_expression" => {
                                        Some(csharp_deferred_call(g, source))
                                    }
                                    _ => None,
                                })
                                .flatten(),
                        };
                        out.bind(
                            read_text(name, source),
                            type_name,
                            scope,
                            declared_elem.clone(),
                        );
                    }
                }
            }
            "foreach_statement" => {
                // The loop variable is visible inside the loop: its explicit
                // type, or for `var` the element type of the field or
                // property it iterates (typed after the walk).
                if let Some(left) = n
                    .child_by_field_name("left")
                    .filter(|l| l.kind_name() == "identifier")
                {
                    let loop_type =
                        csharp_receiver_type_name(n.child_by_field_name("type"), source);
                    let loop_source = if loop_type.is_some() {
                        None
                    } else {
                        csharp_loop_source(n.child_by_field_name("right"), source)
                    };
                    match loop_source {
                        Some((field, suffix)) => {
                            pending_loops.push((read_text(left, source), field, suffix, n))
                        }
                        None => out.bind(
                            read_text(left, source),
                            loop_type.map(CsharpBinding::Type),
                            n,
                            None,
                        ),
                    }
                }
            }
            "declaration_expression" | "declaration_pattern" => {
                if let Some(name) = n
                    .child_by_field_name("name")
                    .filter(|n| n.kind_name() == "identifier")
                {
                    out.bind(
                        read_text(name, source),
                        csharp_receiver_type_name(n.child_by_field_name("type"), source)
                            .map(CsharpBinding::Type),
                        scope,
                        None,
                    )
                }
            }
            _ => {}
        }
        let child_scope = if matches!(
            n.kind_name(),
            "block" | "lambda_expression" | "local_function_statement"
        ) {
            n
        } else {
            scope
        };
        stack.extend(children(n).into_iter().map(|c| (c, child_scope)));
    }
    for (name, field, suffix, node) in pending_loops {
        let shadowed = out.bindings.get(&field).is_some_and(|scopes| {
            scopes
                .iter()
                .any(|(start, end, _)| *start <= node.start_byte() && node.start_byte() < *end)
        });
        let ty = if shadowed {
            None
        } else {
            fields
                .get(&format!("{field}{suffix}{{}}"))
                .cloned()
                .map(CsharpBinding::Type)
        };
        out.bind(name, ty, node, None);
    }
    let base = fields
        .iter()
        .filter(|(name, _)| !out.poisoned.contains(*name))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.bindings.retain(|name, _| !out.poisoned.contains(name));
    (out.bindings, base)
}
pub fn csharp_scoped_receiver_type(
    table: Option<&CsharpReceiverTable>,
    name: Option<&str>,
    call_byte: i64,
) -> Option<CsharpBinding> {
    let (bindings, base) = table?;
    let name = name.filter(|s| !s.is_empty())?;
    let candidates: Vec<_> = bindings
        .get(name)
        .into_iter()
        .flatten()
        .filter(|(start, end, _)| *start as i64 <= call_byte && call_byte < *end as i64)
        .collect();
    let Some(inner) = candidates.iter().map(|(start, end, _)| end - start).min() else {
        return base.get(name).cloned().map(CsharpBinding::Type);
    };
    let mut winners = candidates
        .iter()
        .filter(|(start, end, _)| end - start == inner);
    let winner = winners.next()?;
    if winners.next().is_some() {
        None
    } else {
        winner.2.clone()
    }
}
/// TS class field types by `(class name, static)`: name -> type.
pub type TsFieldTable = BTreeMap<(String, bool), BTreeMap<String, String>>;
pub fn ts_receiver_type_table(
    root: Node<'_>,
    source: &[u8],
    table: &mut BTreeMap<String, String>,
    mut field_table: Option<&mut TsFieldTable>,
) {
    // `: T`: only a single type_identifier names a class.
    let bare = |ann: Node<'_>| {
        let names = named(ann);
        (names.len() == 1 && names[0].kind_name() == "type_identifier")
            .then(|| read_text(names[0], source))
    };
    // `: T | undefined` / `: T | null`: an optional field still holds a T.
    let optional = |ann: Node<'_>| {
        let unions = named(ann);
        if unions.len() != 1 || unions[0].kind_name() != "union_type" {
            return None;
        }
        let members = named(unions[0]);
        let idents = members
            .iter()
            .filter(|c| c.kind_name() == "type_identifier")
            .collect::<Vec<_>>();
        let empties = members
            .iter()
            .filter(|c| matches!(read_text(**c, source).as_str(), "undefined" | "null"))
            .count();
        (idents.len() == 1 && idents.len() + empties == members.len())
            .then(|| read_text(*idents[0], source))
    };
    // `class Foo { .. }` -> Foo; an unnamed class expression -> None.
    let class_name = |body: Option<Node<'_>>| {
        body.filter(|b| b.kind_name() == "class_body")
            .and_then(|b| b.parent())
            .and_then(|owner| owner.child_by_field_name("name"))
            .map(|name| read_text(name, source))
    };
    let new_type = |value: Node<'_>| {
        value
            .child_by_field_name("constructor")
            .filter(|c| matches!(c.kind_name(), "identifier" | "type_identifier"))
            .map(|c| read_text(c, source))
    };
    let annotated = |ann: Node<'_>| bare(ann).or_else(|| optional(ann));
    // Constructor `this.x = ...` assignments, merged after the walk: a
    // declared field type wins over an assignment.
    let mut assigned: TsFieldTable = BTreeMap::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if let Some(fields) = field_table.as_deref_mut() {
            match n.kind_name() {
                // A class field (`protected repo: Repo`, `#root: Node = new
                // Node()`, `private logger = new Logger()`) is what
                // `this.repo.m()` reads, recorded under its own class only;
                // static fields apart (0.9.91).
                "public_field_definition" | "field_definition" => {
                    let owner = class_name(n.parent());
                    let is_static = children(n).iter().any(|c| c.kind_name() == "static");
                    if let Some(name_n) = n
                        .child_by_field_name("name")
                        .or_else(|| n.child_by_field_name("property"))
                        .filter(|name| {
                            matches!(
                                name.kind_name(),
                                "property_identifier" | "private_property_identifier"
                            )
                        })
                    {
                        let ty = match (
                            n.child_by_field_name("type"),
                            n.child_by_field_name("value"),
                        ) {
                            (Some(ann), _) => annotated(ann),
                            (None, Some(value)) if value.kind_name() == "new_expression" => {
                                new_type(value)
                            }
                            _ => None,
                        };
                        let name = read_text(name_n, source);
                        if let Some(ty) = ty.filter(|t| !t.is_empty())
                            && !name.is_empty()
                        {
                            match owner {
                                Some(owner) => {
                                    fields
                                        .entry((owner, is_static))
                                        .or_default()
                                        .entry(name)
                                        .or_insert(ty);
                                }
                                // An unnamed class has no node to key its
                                // fields by: keep the file table.
                                None => {
                                    table.entry(name).or_insert(ty);
                                }
                            }
                        }
                    }
                    stack.extend(children(n));
                    continue;
                }
                "method_definition" => {
                    let mname = n.child_by_field_name("name");
                    let owner = class_name(n.parent());
                    let accessor = children(n)
                        .into_iter()
                        .map(|c| c.kind_name())
                        .find(|k| matches!(*k, "get" | "set"));
                    if let (Some(mname), Some(owner), Some(accessor)) =
                        (mname, owner.clone(), accessor)
                    {
                        // `get repo(): Repo` / `set repo(r: Repo)` type `this.repo`.
                        let ty = if accessor == "get" {
                            n.child_by_field_name("return_type").and_then(annotated)
                        } else {
                            n.child_by_field_name("parameters")
                                .and_then(|ps| {
                                    named(ps).into_iter().find(|p| {
                                        matches!(
                                            p.kind_name(),
                                            "required_parameter" | "optional_parameter"
                                        )
                                    })
                                })
                                .and_then(|p| p.child_by_field_name("type"))
                                .and_then(annotated)
                        };
                        if let Some(ty) = ty.filter(|t| !t.is_empty()) {
                            fields
                                .entry((owner, false))
                                .or_default()
                                .entry(read_text(mname, source))
                                .or_insert(ty);
                        }
                    }
                    if let (Some(mname), Some(owner)) = (mname, owner)
                        && read_text(mname, source) == "constructor"
                    {
                        // Parameter properties (`constructor(private repo:
                        // Repo)`) and `this.repo = repo` / `= new Repo()`.
                        let mut typed: BTreeMap<String, String> = BTreeMap::new();
                        for p in n
                            .child_by_field_name("parameters")
                            .map(named)
                            .unwrap_or_default()
                        {
                            if !matches!(p.kind_name(), "required_parameter" | "optional_parameter")
                            {
                                continue;
                            }
                            let (Some(pat), Some(ann)) = (
                                p.child_by_field_name("pattern"),
                                p.child_by_field_name("type"),
                            ) else {
                                continue;
                            };
                            if pat.kind_name() != "identifier" {
                                continue;
                            }
                            let Some(ty) = annotated(ann).filter(|t| !t.is_empty()) else {
                                continue;
                            };
                            let pname = read_text(pat, source);
                            typed.insert(pname.clone(), ty.clone());
                            if children(p).iter().any(|c| {
                                matches!(c.kind_name(), "accessibility_modifier" | "readonly")
                            }) {
                                fields
                                    .entry((owner.clone(), false))
                                    .or_default()
                                    .entry(pname)
                                    .or_insert(ty);
                            }
                        }
                        for stmt in n.child_by_field_name("body").map(named).unwrap_or_default() {
                            let expr = (stmt.kind_name() == "expression_statement")
                                .then(|| named(stmt).into_iter().next())
                                .flatten()
                                .filter(|e| e.kind_name() == "assignment_expression");
                            let Some(expr) = expr else { continue };
                            let (Some(left), Some(right)) = (
                                expr.child_by_field_name("left"),
                                expr.child_by_field_name("right"),
                            ) else {
                                continue;
                            };
                            if left.kind_name() != "member_expression" {
                                continue;
                            }
                            let (Some(obj), Some(prop)) = (
                                left.child_by_field_name("object"),
                                left.child_by_field_name("property"),
                            ) else {
                                continue;
                            };
                            if obj.kind_name() != "this" {
                                continue;
                            }
                            let ty = match right.kind_name() {
                                "identifier" => typed.get(&read_text(right, source)).cloned(),
                                "new_expression" => new_type(right),
                                _ => None,
                            };
                            if let Some(ty) = ty.filter(|t| !t.is_empty()) {
                                assigned
                                    .entry((owner.clone(), false))
                                    .or_default()
                                    .entry(read_text(prop, source))
                                    .or_insert(ty);
                            }
                        }
                    }
                    stack.extend(children(n));
                    continue;
                }
                _ => {}
            }
        }
        let binding = match n.kind_name() {
            "variable_declarator" => n
                .child_by_field_name("name")
                .filter(|n| n.kind_name() == "identifier")
                .zip(
                    n.child_by_field_name("value")
                        .filter(|n| n.kind_name() == "new_expression")
                        .and_then(|v| v.child_by_field_name("constructor"))
                        .filter(|n| matches!(n.kind_name(), "identifier" | "type_identifier")),
                )
                .map(|(n, t)| (read_text(n, source), read_text(t, source))),
            "required_parameter" | "optional_parameter" => n
                .child_by_field_name("pattern")
                .filter(|n| n.kind_name() == "identifier")
                .zip(n.child_by_field_name("type"))
                .and_then(|(pattern, ann)| Some((read_text(pattern, source), bare(ann)?))),
            // A class field (`protected repo: Repo`, `#root: Node = new
            // Node()`, `private logger = new Logger()`) is what
            // `this.repo.m()` reads (0.9.81, #4231). Static fields are read as
            // `Cls.field`, so they are skipped.
            "public_field_definition" | "field_definition" => n
                .child_by_field_name("name")
                .or_else(|| n.child_by_field_name("property"))
                .filter(|name| {
                    matches!(
                        name.kind_name(),
                        "property_identifier" | "private_property_identifier"
                    ) && !children(n).iter().any(|c| c.kind_name() == "static")
                })
                .and_then(|name| {
                    let class = match (
                        n.child_by_field_name("type"),
                        n.child_by_field_name("value"),
                    ) {
                        (Some(ann), _) => bare(ann).or_else(|| optional(ann)),
                        (None, Some(value)) if value.kind_name() == "new_expression" => value
                            .child_by_field_name("constructor")
                            .filter(|c| matches!(c.kind_name(), "identifier" | "type_identifier"))
                            .map(|c| read_text(c, source)),
                        _ => None,
                    }?;
                    Some((read_text(name, source), class))
                }),
            _ => None,
        };
        if let Some((name, t)) = binding
            && !name.is_empty()
            && !t.is_empty()
        {
            table.entry(name).or_insert(t);
        }
        stack.extend(children(n));
    }
    if let Some(fields) = field_table {
        for (class, names) in assigned {
            let table = fields.entry(class).or_default();
            for (name, ty) in names {
                table.entry(name).or_insert(ty);
            }
        }
    }
}
/// `_ts_attach_class_fields` (0.9.91): each class's field types on its node
/// as `_ts_fields` / `_ts_static_fields`. Node ids are case-folded, so a
/// class and a same-named binding are one node; classes are matched by the
/// folded name, and two classes folding together keep only the fields they
/// type alike.
pub fn ts_attach_class_fields(nodes: &mut [Value], fields: &TsFieldTable, path: &str) {
    let fold = |label: &str| {
        label
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_lowercase()
    };
    let mut by_key: BTreeMap<(String, bool), BTreeMap<String, String>> = BTreeMap::new();
    for ((class, is_static), names) in fields {
        let merged = by_key.entry((fold(class), *is_static)).or_default();
        for (name, ty) in names {
            let keep = merged.get(name).is_none_or(|t| t == ty);
            merged.insert(name.clone(), if keep { ty.clone() } else { String::new() });
        }
    }
    if by_key.is_empty() {
        return;
    }
    for node in nodes.iter_mut() {
        if node["source_file"].as_str() != Some(path) {
            continue;
        }
        let label = fold(node["label"].as_str().unwrap_or(""));
        for (is_static, attr) in [(false, "_ts_fields"), (true, "_ts_static_fields")] {
            let kept = by_key
                .get(&(label.clone(), is_static))
                .into_iter()
                .flatten()
                .filter(|(_, ty)| !ty.is_empty())
                .map(|(name, ty)| (name.clone(), Value::String(ty.clone())))
                .collect::<serde_json::Map<_, _>>();
            if !kept.is_empty() {
                node[attr] = Value::Object(kept);
            }
        }
    }
}
const TS_FUNCTION_NODES: &[&str] = &[
    "arrow_function",
    "method_definition",
    "function_declaration",
    "function_expression",
    "generator_function_declaration",
    "generator_function",
];
/// `_ts_in_static_method`: whether `node` runs in a `static` method, where
/// `this` is the class. Arrow functions keep the enclosing `this`.
pub fn ts_in_static_method(node: Node<'_>) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind_name() == "method_definition" {
            return children(n).iter().any(|c| c.kind_name() == "static");
        }
        if TS_FUNCTION_NODES.contains(&n.kind_name()) && n.kind_name() != "arrow_function" {
            return false;
        }
        current = n.parent();
    }
    false
}
/// `_ts_field_destructured_from_this`: the `this` field `name` was
/// destructured from (`const { creator } = this`, `const { creator: c } =
/// this`) in the function around the call, or around an arrow function
/// holding it; the search stops at the first function binding its own `this`.
pub fn ts_field_destructured_from_this(
    ident: Node<'_>,
    name: &str,
    source: &[u8],
) -> Option<String> {
    let mut current = ident.parent();
    while let Some(node) = current {
        if TS_FUNCTION_NODES.contains(&node.kind_name()) {
            let mut stack = node
                .child_by_field_name("body")
                .into_iter()
                .collect::<Vec<_>>();
            while let Some(n) = stack.pop() {
                if n.kind_name() == "variable_declarator"
                    && let (Some(pattern), Some(value)) = (
                        n.child_by_field_name("name"),
                        n.child_by_field_name("value"),
                    )
                    && pattern.kind_name() == "object_pattern"
                    && value.kind_name() == "this"
                {
                    for p in named(pattern) {
                        if p.kind_name() == "shorthand_property_identifier_pattern"
                            && read_text(p, source) == name
                        {
                            return Some(name.to_string());
                        }
                        if p.kind_name() == "pair_pattern"
                            && let (Some(key), Some(val)) =
                                (p.child_by_field_name("key"), p.child_by_field_name("value"))
                            && val.kind_name() == "identifier"
                            && read_text(val, source) == name
                        {
                            return Some(read_text(key, source));
                        }
                    }
                }
                stack.extend(
                    children(n)
                        .into_iter()
                        .filter(|c| !TS_FUNCTION_NODES.contains(&c.kind_name())),
                );
            }
            if node.kind_name() != "arrow_function" {
                return None;
            }
        }
        current = node.parent();
    }
    None
}
/// `_ts_mark_public_twins` (0.9.87, #4298): a class's `foo` and `#foo` share
/// one member node (ids drop `#`). The node is marked `_ts_public_twin` only
/// when the class declares both, so a `c.foo()` call may reach it; with
/// `#foo` alone the public `foo` comes from somewhere else.
pub fn ts_mark_public_twins(
    root: Node<'_>,
    source: &[u8],
    nodes: &mut [serde_json::Value],
    edges: &[serde_json::Value],
) {
    let mut shared: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind_name() == "class_body"
            && let Some(parent) = n.parent()
        {
            let class = parent.child_by_field_name("name");
            let (mut public, mut private) = (BTreeSet::new(), BTreeSet::new());
            for member in named(n) {
                if !matches!(
                    member.kind_name(),
                    "method_definition" | "public_field_definition" | "field_definition"
                ) {
                    continue;
                }
                let Some(name) = member
                    .child_by_field_name("name")
                    .or_else(|| member.child_by_field_name("property"))
                else {
                    continue;
                };
                let text = read_text(name, source);
                match name.kind_name() {
                    "private_property_identifier" => {
                        private.insert(text.trim_start_matches('#').to_string());
                    }
                    "property_identifier" => {
                        public.insert(text);
                    }
                    _ => {}
                }
            }
            let both: BTreeSet<String> = public.intersection(&private).cloned().collect();
            if let Some(class) = class
                && !both.is_empty()
            {
                shared
                    .entry(read_text(class, source))
                    .or_default()
                    .extend(both);
            }
        }
        stack.extend(children(n));
    }
    if shared.is_empty() {
        return;
    }
    let labels: BTreeMap<&str, &str> = nodes
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n["label"].as_str().unwrap_or(""))))
        .collect();
    let marked: BTreeSet<String> = edges
        .iter()
        .filter(|e| e["relation"] == "method")
        .filter_map(|e| {
            let target = e["target"].as_str()?;
            let source_label = labels.get(e["source"].as_str()?).copied().unwrap_or("");
            let member = labels
                .get(target)
                .copied()
                .unwrap_or("")
                .trim_matches(['(', ')'])
                .trim_start_matches('.')
                .trim_start_matches('#');
            shared
                .get(source_label)
                .is_some_and(|names| names.contains(member))
                .then(|| target.to_string())
        })
        .collect();
    for n in nodes {
        if n["id"].as_str().is_some_and(|id| marked.contains(id)) {
            n["_ts_public_twin"] = serde_json::json!(true);
        }
    }
}
pub fn find_require_call(node: Option<Node<'_>>) -> Option<Node<'_>> {
    let n = node?;
    if n.kind_name() == "call_expression"
        && n.child_by_field_name("function")
            .is_some_and(|f| f.kind_name() == "identifier")
    {
        return Some(n);
    }
    if n.kind_name() == "member_expression" {
        find_require_call(n.child_by_field_name("object"))
    } else {
        None
    }
}
pub fn js_topmost_closures<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
    for c in children(node) {
        if set("_JS_FUNCTION_VALUE_TYPES").contains(c.kind_name()) {
            out.push(c)
        } else {
            js_topmost_closures(c, out)
        }
    }
}
pub fn js_member_assignment_target(
    node: Option<Node<'_>>,
    source: &[u8],
) -> Option<(String, Option<String>, String)> {
    let n = node.filter(|n| n.kind_name() == "member_expression")?;
    let member = read_text(n.child_by_field_name("property")?, source);
    if member.is_empty() {
        return None;
    }
    let obj = n.child_by_field_name("object")?;
    match obj.kind_name() {
        "this" => Some(("this".into(), None, member)),
        "identifier" => {
            let name = read_text(obj, source);
            if name == "exports" {
                Some(("exports".into(), None, member))
            } else {
                Some(("object".into(), Some(name), member))
            }
        }
        "member_expression" => {
            let inner = obj.child_by_field_name("object")?;
            let prop = obj.child_by_field_name("property")?;
            if inner.kind_name() != "identifier" {
                return None;
            }
            let inner_name = read_text(inner, source);
            let prop_name = read_text(prop, source);
            if inner_name == "module" && prop_name == "exports" {
                Some(("exports".into(), None, member))
            } else if prop_name == "prototype" {
                Some(("prototype".into(), Some(inner_name), member))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `_js_find_exported_objects` (0.9.77, #3778): identifiers a JS module
/// exports through ESM `export` forms or CommonJS `module.exports`/`exports`
/// assignments, so `Obj.method = fn` on an exported object becomes a method.
pub fn js_find_exported_objects(program: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut exported = BTreeSet::new();
    if program.kind_name() != "program" {
        return exported;
    }
    let is_exports = |text: &str| {
        matches!(text, "module.exports" | "exports")
            || text.starts_with("module.exports.")
            || text.starts_with("exports.")
    };
    fn declarators(n: Node<'_>) -> Vec<Node<'_>> {
        children(n)
            .into_iter()
            .filter(|d| d.kind_name() == "variable_declarator")
            .collect()
    }
    for child in children(program) {
        match child.kind_name() {
            "export_statement" => {
                for c in children(child) {
                    match c.kind_name() {
                        "identifier" => {
                            exported.insert(read_text(c, source));
                        }
                        "export_clause" => {
                            for spec in children(c)
                                .into_iter()
                                .filter(|s| s.kind_name() == "export_specifier")
                            {
                                if let Some(name) = spec.child_by_field_name("name") {
                                    exported.insert(read_text(name, source));
                                }
                            }
                        }
                        "variable_declaration" | "lexical_declaration" => {
                            for d in declarators(c) {
                                if let Some(name) = d
                                    .child_by_field_name("name")
                                    .filter(|n| n.kind_name() == "identifier")
                                {
                                    exported.insert(read_text(name, source));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "variable_declaration" | "lexical_declaration" => {
                for d in declarators(child) {
                    let (Some(name), Some(value)) = (
                        d.child_by_field_name("name"),
                        d.child_by_field_name("value"),
                    ) else {
                        continue;
                    };
                    if name.kind_name() != "identifier" {
                        continue;
                    }
                    let name = read_text(name, source);
                    let mut current = Some(value);
                    while let Some(assign) =
                        current.filter(|c| c.kind_name() == "assignment_expression")
                    {
                        if assign
                            .child_by_field_name("left")
                            .is_some_and(|l| is_exports(&read_text(l, source)))
                        {
                            exported.insert(name.clone());
                            break;
                        }
                        current = assign.child_by_field_name("right");
                    }
                    if current.is_some_and(|c| {
                        matches!(read_text(c, source).as_str(), "module.exports" | "exports")
                    }) {
                        exported.insert(name);
                    }
                }
            }
            "expression_statement" => {
                let Some(assign) = first_child(child, &["assignment_expression"]) else {
                    continue;
                };
                if !assign
                    .child_by_field_name("left")
                    .is_some_and(|l| is_exports(&read_text(l, source)))
                {
                    continue;
                }
                let mut current = assign.child_by_field_name("right");
                while let Some(inner) = current.filter(|c| c.kind_name() == "assignment_expression")
                {
                    if let Some(left) = inner
                        .child_by_field_name("left")
                        .filter(|l| l.kind_name() == "identifier")
                    {
                        exported.insert(read_text(left, source));
                    }
                    current = inner.child_by_field_name("right");
                }
                match current {
                    Some(c) if c.kind_name() == "identifier" => {
                        exported.insert(read_text(c, source));
                    }
                    Some(c) if c.kind_name() == "object" => {
                        for prop in children(c) {
                            match prop.kind_name() {
                                "shorthand_property_identifier" => {
                                    exported.insert(read_text(prop, source));
                                }
                                "pair" => {
                                    if let Some(v) = prop
                                        .child_by_field_name("value")
                                        .filter(|v| v.kind_name() == "identifier")
                                    {
                                        exported.insert(read_text(v, source));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    exported
}

/// Names bound in one Python scope, module-root plain-import counts, and
/// whether the scope binds names in ways the guard cannot follow.
type ShadowFacts = (BTreeSet<String>, BTreeMap<String, usize>, bool);

const PYTHON_DYNAMIC_NAMESPACE_NAMES: [&str; 7] = [
    "exec", "eval", "globals", "locals", "vars", "setattr", "delattr",
];

/// Conservative lexical guard for plain external Python module receivers
/// (Graphify 6868fff and 33edd89, `_python_receiver_shadow_checker`).
///
/// A module-root plain import is the only eligible binding. Any other binding
/// of the receiver in an enclosing scope fails closed regardless of statement
/// order, as do dynamic namespace operations and unparseable scopes. Scope
/// facts are cached because every member call in a scope asks again.
pub struct PythonReceiverShadow {
    dynamic_binding: bool,
    scopes: BTreeMap<usize, ShadowFacts>,
}

impl PythonReceiverShadow {
    pub fn new(root: Node<'_>, source: &[u8]) -> Self {
        // A nested function can rebind module or captured names later, and
        // namespace operations can replace an import without an assignment.
        let mut dynamic_binding = false;
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            // Namespace mutators can be called directly, qualified, or saved
            // under an alias, so references fail closed as well as calls.
            let kind = node.kind_name();
            if matches!(kind, "global_statement" | "nonlocal_statement")
                || (kind == "identifier"
                    && PYTHON_DYNAMIC_NAMESPACE_NAMES.contains(&read_text(node, source).as_str()))
            {
                dynamic_binding = true;
                break;
            }
            pending.extend(named(node));
        }
        Self {
            dynamic_binding,
            scopes: BTreeMap::new(),
        }
    }

    pub fn is_shadowed(&mut self, call: Node<'_>, receiver: &str, source: &[u8]) -> bool {
        let mut found_module = false;
        let mut node = Some(call);
        while let Some(current) = node {
            let kind = current.kind_name();
            if matches!(
                kind,
                "module" | "function_definition" | "class_definition" | "lambda"
            ) {
                let dynamic_binding = self.dynamic_binding;
                let (bound, plain_imports, unsafe_scope) = self
                    .scopes
                    .entry(current.id())
                    .or_insert_with(|| python_shadow_facts(current, source, dynamic_binding));
                if *unsafe_scope || bound.contains(receiver) {
                    return true;
                }
                if kind == "module" {
                    found_module = true;
                    if plain_imports.get(receiver) != Some(&1) {
                        return true;
                    }
                }
            }
            node = current.parent();
        }
        !found_module
    }
}

fn python_shadow_facts(scope: Node<'_>, source: &[u8], dynamic_binding: bool) -> ShadowFacts {
    let module = scope.kind_name() == "module";
    let mut scan = ShadowScan {
        source,
        scope,
        bound: BTreeSet::new(),
        plain_imports: BTreeMap::new(),
        unsafe_scope: scope.has_error()
            || scope.child_by_field_name("type_parameters").is_some()
            || (module && dynamic_binding),
    };
    if matches!(scope.kind_name(), "function_definition" | "lambda") {
        scan.bound.extend(python_param_names(
            scope.child_by_field_name("parameters"),
            source,
        ));
    }
    let body = if module {
        Some(scope)
    } else {
        scope.child_by_field_name("body")
    };
    match body {
        Some(body) => scan.walk(body),
        None => scan.unsafe_scope = true,
    }
    (scan.bound, scan.plain_imports, scan.unsafe_scope)
}

struct ShadowScan<'a, 't> {
    source: &'a [u8],
    scope: Node<'t>,
    bound: BTreeSet<String>,
    plain_imports: BTreeMap<String, usize>,
    unsafe_scope: bool,
}

impl ShadowScan<'_, '_> {
    fn walk(&mut self, node: Node<'_>) {
        for child in named(node) {
            let kind = child.kind_name();
            match kind {
                "function_definition" | "class_definition" => {
                    match child.child_by_field_name("name") {
                        Some(name) => {
                            self.bound.insert(read_text(name, self.source));
                        }
                        None => self.unsafe_scope = true,
                    }
                    continue;
                }
                "lambda" => continue,
                "global_statement" | "nonlocal_statement" | "match_statement" | "ERROR" => {
                    self.unsafe_scope = true;
                    continue;
                }
                "import_statement" | "import_from_statement" => {
                    match python_import_names(child, self.source) {
                        None => self.unsafe_scope = true,
                        Some(names)
                            if kind == "import_statement"
                                && self.scope.kind_name() == "module"
                                && node == self.scope =>
                        {
                            for name in names {
                                *self.plain_imports.entry(name).or_default() += 1;
                            }
                        }
                        Some(names) => self.bound.extend(names),
                    }
                    continue;
                }
                "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                    self.require_targets(child.child_by_field_name("left"))
                }
                "type_alias_statement" => {
                    let name = child
                        .child_by_field_name("left")
                        .map(|left| {
                            let text = read_text(left, self.source);
                            let head = text.split('[').next().unwrap_or_default();
                            head.trim_matches(crate::dedup_strings::whitespace)
                                .to_string()
                        })
                        .unwrap_or_default();
                    if crate::extract_python::is_identifier(&name) {
                        self.bound.insert(name);
                    } else {
                        self.unsafe_scope = true;
                    }
                }
                "named_expression" => self.require_targets(child.child_by_field_name("name")),
                "with_item" => {
                    let alias = child.child_by_field_name("alias").or_else(|| {
                        child
                            .child_by_field_name("value")
                            .filter(|value| value.kind_name() == "as_pattern")
                            .and_then(|value| value.child_by_field_name("alias"))
                    });
                    if alias.is_some() {
                        self.require_targets(alias)
                    }
                }
                "as_pattern" if node.kind_name() == "except_clause" => {
                    self.require_targets(child.child_by_field_name("alias"))
                }
                "delete_statement" => {
                    for item in named(child) {
                        self.require_targets(Some(item))
                    }
                }
                _ => {}
            }
            self.walk(child);
        }
    }

    fn require_targets(&mut self, target: Option<Node<'_>>) {
        if !python_target_names(target, self.source, &mut self.bound) {
            self.unsafe_scope = true;
        }
    }
}

fn python_target_names(
    node: Option<Node<'_>>,
    source: &[u8],
    names: &mut BTreeSet<String>,
) -> bool {
    let Some(node) = node else { return false };
    match node.kind_name() {
        "identifier" => {
            names.insert(read_text(node, source));
            true
        }
        "tuple_pattern"
        | "list_pattern"
        | "pattern_list"
        | "parenthesized_expression"
        | "as_pattern_target" => named(node)
            .into_iter()
            .all(|child| python_target_names(Some(child), source, names)),
        "attribute" | "subscript" => python_target_names(
            node.child_by_field_name("object")
                .or_else(|| named(node).into_iter().next()),
            source,
            names,
        ),
        _ => false,
    }
}

fn python_import_names(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut past_import = false;
    for item in children(node) {
        match item.kind_name() {
            "import" => past_import = true,
            _ if !past_import => {}
            "aliased_import" => names.push(read_text(item.child_by_field_name("alias")?, source)),
            "dotted_name" => names.push(
                read_text(item, source)
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            ),
            "wildcard_import" => return None,
            _ => {}
        }
    }
    Some(names)
}
