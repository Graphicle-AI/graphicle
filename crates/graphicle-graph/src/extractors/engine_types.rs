// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
//! Type, annotation and receiver helpers from the pinned generic engine.
use super::base::{make_id, read_text};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};
use tree_sitter::Node;
pub type TypeRefs = Vec<(String, String)>;
pub type CsharpRefs = Vec<(String, String, bool, String)>;
pub static SETS: LazyLock<BTreeMap<String, BTreeSet<String>>> = LazyLock::new(|| {
    let v: serde_json::Value = serde_json::from_str(include_str!("../assets/engine/sets.json"))
        .expect("pinned engine sets");
    v.as_object()
        .unwrap()
        .iter()
        .filter_map(|(k, v)| {
            v.as_array()
                .filter(|a| a.iter().all(serde_json::Value::is_string))
                .map(|a| {
                    (
                        k.clone(),
                        a.iter().map(|v| v.as_str().unwrap().to_string()).collect(),
                    )
                })
        })
        .collect()
});
pub fn set(name: &str) -> &'static BTreeSet<String> {
    &SETS[name]
}
/// Symbol ids at or above this are not tabled (ERROR is `u16::MAX`).
const KIND_TABLE_LIMIT: usize = 4096;
/// Per grammar (by its language pointer), kind names by kind id.
type KindTables = Vec<(usize, Vec<Option<&'static str>>)>;
std::thread_local! {
    /// Kind names already read on this thread, per grammar and kind id.
    /// `Node::kind` measures and UTF-8-validates the grammar's C string on
    /// every call, and extraction asks for the kind of most nodes several times.
    static KIND_NAMES: std::cell::RefCell<KindTables> =
        const { std::cell::RefCell::new(Vec::new()) };
}
/// `Node::kind`, remembered per grammar and kind id.
pub trait KindName {
    fn kind_name(&self) -> &'static str;
}
impl KindName for Node<'_> {
    #[inline]
    fn kind_name(&self) -> &'static str {
        let id = self.kind_id() as usize;
        if id >= KIND_TABLE_LIMIT {
            return leak_kind(self.kind());
        }
        // SAFETY: `ts_node_language` only reads the node's tree pointer.
        let language = unsafe { tree_sitter::ffi::ts_node_language(self.into_raw()) } as usize;
        KIND_NAMES.with(|tables| {
            let mut tables = tables.borrow_mut();
            let at = match tables.iter().position(|(l, _)| *l == language) {
                Some(at) => at,
                None => {
                    tables.push((language, Vec::new()));
                    tables.len() - 1
                }
            };
            let table = &mut tables[at].1;
            if let Some(Some(name)) = table.get(id) {
                debug_assert_eq!(*name, self.kind());
                return *name;
            }
            let name = leak_kind(self.kind());
            if table.len() <= id {
                table.resize(id + 1, None);
            }
            table[id] = Some(name);
            name
        })
    }
}
/// A process-lifetime copy of a kind name (each grammar has a bounded set).
fn leak_kind(kind: &str) -> &'static str {
    static NAMES: LazyLock<std::sync::Mutex<BTreeSet<&'static str>>> =
        LazyLock::new(Default::default);
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(name) = names.get(kind) {
        return name;
    }
    let name: &'static str = Box::leak(kind.to_owned().into_boxed_str());
    names.insert(name);
    name
}
pub fn children(n: Node<'_>) -> Vec<Node<'_>> {
    n.children(&mut n.walk()).collect()
}
pub fn named(n: Node<'_>) -> Vec<Node<'_>> {
    n.named_children(&mut n.walk()).collect()
}
pub fn first_child<'a>(n: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    children(n)
        .into_iter()
        .find(|c| kinds.contains(&c.kind_name()))
}
fn role(generic: bool) -> String {
    if generic { "generic_arg" } else { "type" }.into()
}
fn push(out: &mut TypeRefs, name: String, generic: bool) {
    if !name.is_empty() {
        out.push((name, role(generic)));
    }
}
fn python_noise(name: &str) -> bool {
    set("_PYTHON_TYPE_CONTAINERS").contains(name) || set("_PYTHON_ANNOTATION_NOISE").contains(name)
}
pub fn python_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
) {
    let Some(node) = node else { return };
    match node.kind_name() {
        "identifier" => {
            let name = read_text(node, source);
            if !python_noise(&name) {
                push(out, name, generic)
            }
        }
        "attribute" => {
            let name = read_text(node, source)
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_owned();
            if !python_noise(&name) {
                push(out, name, generic)
            }
        }
        "generic_type" => {
            for c in children(node) {
                match c.kind_name() {
                    "identifier" => {
                        let name = read_text(c, source);
                        if !python_noise(&name) {
                            push(out, name, generic)
                        }
                    }
                    "type_parameter" => {
                        for sub in named(c) {
                            python_collect_type_refs(Some(sub), source, true, out)
                        }
                    }
                    _ => {}
                }
            }
        }
        "subscript" => {
            // Parity quirk: the pinned engine.py walks `value` with the outer
            // role and then re-walks every named child with generic=True,
            // skipping the value only via `c is value`. py-tree-sitter 0.25
            // returns a fresh Node from child_by_field_name so that identity
            // test never matches and the value is emitted twice
            // (`typing.ItemsView[str, str]` -> (ItemsView, type),
            // (ItemsView, generic_arg)). Unchanged in 0.9.66; reproduced here
            // so `references` edge `context` matches byte-for-byte.
            let value = node.child_by_field_name("value");
            python_collect_type_refs(value, source, generic, out);
            for c in named(node) {
                python_collect_type_refs(Some(c), source, true, out)
            }
        }
        "type" => {
            for c in named(node) {
                python_collect_type_refs(Some(c), source, generic, out)
            }
        }
        _ => {
            if node.is_named() {
                for c in named(node) {
                    python_collect_type_refs(Some(c), source, generic, out)
                }
            }
        }
    }
}
/// Declared-name groups: plain id -> the distinct names minting it.
pub type NameGroups = BTreeMap<String, BTreeSet<String>>;
const CSHARP_TYPE_DECLARATIONS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "struct_declaration",
    "record_declaration",
];
/// `_csharp_type_name_for_id` (#4249): a generic type's id folds its arity
/// in (`` Effect`1 ``); arity 0 is the bare name.
pub fn csharp_type_name_for_id(node: Node<'_>, class_name: &str) -> String {
    let arity = children(node)
        .into_iter()
        .filter(|c| c.kind_name() == "type_parameter_list")
        .flat_map(children)
        .filter(|p| matches!(p.kind_name(), "type_parameter" | "identifier"))
        .count();
    if arity > 0 {
        format!("{class_name}`{arity}")
    } else {
        class_name.to_string()
    }
}
/// `_csharp_namespace_name`.
fn csharp_namespace_name(node: Node<'_>, source: &[u8]) -> String {
    if let Some(name) = node.child_by_field_name("name") {
        return read_text(name, source).trim().to_string();
    }
    children(node)
        .into_iter()
        .find(|c| matches!(c.kind_name(), "identifier" | "qualified_name"))
        .map(|c| read_text(c, source).trim().to_string())
        .unwrap_or_default()
}
/// `_csharp_pre_scan_name_collisions` (0.9.90, #4415): C# names that differ
/// only by `_` or case (`Action()`/`_Action()`, `Pair`/`_Pair`) mint one id,
/// so the second declaration was dropped. Returns `(type_groups,
/// member_groups)` for the ids two or more distinct names share; types keyed
/// as the walk mints them, members (methods, properties, enum members) under
/// their type's final (already salted) id. Overloads are one name.
pub fn csharp_pre_scan_name_collisions(
    root: Node<'_>,
    source: &[u8],
    stem: &str,
) -> (NameGroups, NameGroups) {
    fn visit<'a>(
        n: Node<'a>,
        ns: &[String],
        source: &[u8],
        stem: &str,
        types: &mut NameGroups,
        decls: &mut Vec<(Node<'a>, String, String)>,
    ) {
        let mut scoped = ns.to_vec();
        for c in children(n) {
            let kind = c.kind_name();
            if kind == "namespace_declaration" {
                let name = csharp_namespace_name(c, source);
                if let Some(body) = c.child_by_field_name("body") {
                    let mut inner = scoped.clone();
                    if !name.is_empty() {
                        inner.push(name);
                    }
                    visit(body, &inner, source, stem, types, decls);
                }
            } else if kind == "file_scoped_namespace_declaration" {
                // Applies to the rest of the file, as in the walk.
                let name = csharp_namespace_name(c, source);
                if !name.is_empty() {
                    scoped.push(name);
                }
            } else if CSHARP_TYPE_DECLARATIONS.contains(&kind) {
                let name = c
                    .child_by_field_name("name")
                    .map(|n| read_text(n, source))
                    .unwrap_or_default();
                if !name.is_empty() {
                    let plain = make_id([
                        stem,
                        scoped.join(".").as_str(),
                        csharp_type_name_for_id(c, &name).as_str(),
                    ]);
                    types.entry(plain.clone()).or_default().insert(name.clone());
                    decls.push((c, plain, name));
                }
                // Nested types take the namespace, not the enclosing type.
                visit(c, &scoped, source, stem, types, decls);
            } else if kind == "declaration_list" || kind.starts_with("preproc_") {
                visit(c, &scoped, source, stem, types, decls);
            }
        }
    }
    fn members(n: Node<'_>, type_id: &str, source: &[u8], out: &mut NameGroups) {
        for c in children(n) {
            let kind = c.kind_name();
            if matches!(
                kind,
                "method_declaration" | "property_declaration" | "enum_member_declaration"
            ) {
                let name = c
                    .child_by_field_name("name")
                    .map(|n| read_text(n, source))
                    .unwrap_or_default();
                if !name.is_empty() {
                    out.entry(make_id([type_id, name.as_str()]))
                        .or_default()
                        .insert(name);
                }
            } else if matches!(kind, "declaration_list" | "enum_member_declaration_list")
                || kind.starts_with("preproc_")
            {
                members(c, type_id, source, out);
            }
        }
    }
    let mut types = NameGroups::new();
    let mut decls = vec![];
    visit(root, &[], source, stem, &mut types, &mut decls);
    types.retain(|_, names| names.len() >= 2);
    let mut member_names = NameGroups::new();
    for (decl, plain, name) in decls {
        let type_id = python_underscore_salted_nid(&plain, &name, &types);
        members(decl, &type_id, source, &mut member_names);
    }
    member_names.retain(|_, names| names.len() >= 2);
    (types, member_names)
}
pub fn csharp_pre_scan_interfaces(root: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind_name() == "interface_declaration"
            && let Some(name) = n.child_by_field_name("name")
        {
            let name = read_text(name, source);
            if !name.is_empty() {
                out.insert(name);
            }
        }
        stack.extend(children(n));
    }
    out
}
pub fn csharp_classify_base<'a>(name: &str, interfaces: &BTreeSet<String>) -> &'a str {
    if interfaces.contains(name)
        || (name.starts_with('I') && name.chars().nth(1).is_some_and(char::is_uppercase))
    {
        "implements"
    } else {
        "inherits"
    }
}
pub fn csharp_type_parameters_in_scope(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut scope = Some(node);
    while let Some(n) = scope {
        if set("_CSHARP_TYPE_PARAMETER_SCOPE_DECLARATIONS").contains(n.kind_name()) {
            for c in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "type_parameter_list")
            {
                for param in children(c) {
                    let name = match param.kind_name() {
                        "type_parameter" => first_child(param, &["identifier"]),
                        "identifier" => Some(param),
                        _ => None,
                    };
                    if let Some(name) = name {
                        let name = read_text(name, source);
                        if !name.is_empty() {
                            out.insert(name);
                        }
                    }
                }
            }
        }
        scope = n.parent();
    }
    out
}
pub fn csharp_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut CsharpRefs,
    skip: Option<&BTreeSet<String>>,
) {
    let Some(node) = node else { return };
    let local;
    let skip = match skip {
        Some(s) => s,
        None => {
            local = csharp_type_parameters_in_scope(node, source);
            &local
        }
    };
    match node.kind_name() {
        "predefined_type" => {}
        "identifier" => {
            let name = read_text(node, source);
            if !name.is_empty() && !skip.contains(&name) {
                out.push((name, role(generic), false, String::new()));
            }
        }
        "qualified_name" => {
            let raw = read_text(node, source);
            let (prefix, name) = raw.rsplit_once('.').unwrap_or(("", &raw));
            let name = name.split('<').next().unwrap_or("");
            if !name.is_empty() && !skip.contains(name) {
                out.push((name.into(), role(generic), true, prefix.into()));
            }
        }
        "generic_name" => {
            if let Some(n) = node
                .child_by_field_name("name")
                .or_else(|| first_child(node, &["identifier"]))
            {
                let raw = read_text(n, source);
                let (prefix, name) = raw.rsplit_once('.').unwrap_or(("", &raw));
                let qualified = n.kind_name() == "qualified_name";
                if !name.is_empty() && !skip.contains(name) {
                    out.push((
                        name.into(),
                        role(generic),
                        qualified,
                        if qualified {
                            prefix.into()
                        } else {
                            String::new()
                        },
                    ));
                }
            }
            for args in children(node)
                .into_iter()
                .filter(|c| c.kind_name() == "type_argument_list")
            {
                for arg in named(args) {
                    csharp_collect_type_refs(Some(arg), source, true, out, Some(skip));
                }
            }
        }
        "tuple_type" => {
            // A named tuple's element name is a variable, not a type. Traverse
            // only each tuple_element's `type` field (#3796).
            for element in children(node)
                .into_iter()
                .filter(|c| c.kind_name() == "tuple_element")
            {
                csharp_collect_type_refs(
                    element.child_by_field_name("type"),
                    source,
                    generic,
                    out,
                    Some(skip),
                );
            }
        }
        _ => {
            if node.is_named() {
                for c in named(node) {
                    csharp_collect_type_refs(Some(c), source, generic, out, Some(skip))
                }
            }
        }
    }
}
pub fn csharp_attribute_names(node: Node<'_>, source: &[u8]) -> Vec<(String, bool, String)> {
    let skip = csharp_type_parameters_in_scope(node, source);
    let mut out = vec![];
    for list in children(node)
        .into_iter()
        .filter(|c| c.kind_name() == "attribute_list")
    {
        for attr in children(list)
            .into_iter()
            .filter(|c| c.kind_name() == "attribute")
        {
            if let Some(n) = attr
                .child_by_field_name("name")
                .or_else(|| first_child(attr, &["identifier", "qualified_name"]))
            {
                let raw = read_text(n, source);
                let (prefix, name) = raw.rsplit_once('.').unwrap_or(("", &raw));
                let qualified = n.kind_name() == "qualified_name";
                if !name.is_empty() && !skip.contains(name) {
                    out.push((
                        name.into(),
                        qualified,
                        if qualified {
                            prefix.into()
                        } else {
                            String::new()
                        },
                    ));
                }
            }
        }
    }
    out
}
pub fn java_type_parameters_in_scope(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut scope = Some(node);
    while let Some(n) = scope {
        if set("_JAVA_TYPE_PARAMETER_SCOPE_DECLARATIONS").contains(n.kind_name())
            && let Some(params) = n.child_by_field_name("type_parameters")
        {
            for param in children(params)
                .into_iter()
                .filter(|c| c.kind_name() == "type_parameter")
            {
                if let Some(n) = first_child(param, &["type_identifier"]) {
                    out.insert(read_text(n, source));
                }
            }
        }
        scope = n.parent();
    }
    out
}
pub fn java_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
    skip: Option<&BTreeSet<String>>,
    preserve_qualified: bool,
) {
    let Some(node) = node else { return };
    let local;
    let skip = match skip {
        Some(s) => s,
        None => {
            local = java_type_parameters_in_scope(node, source);
            &local
        }
    };
    match node.kind_name() {
        "integral_type" | "floating_point_type" | "boolean_type" | "void_type" => {}
        "type_identifier" => {
            let name = read_text(node, source);
            if !skip.contains(&name) && !set("_JAVA_BUILTIN_TYPES").contains(&name) {
                push(out, name, generic)
            }
        }
        "scoped_type_identifier" => {
            let raw = read_text(node, source);
            let simple = raw.rsplit('.').next().unwrap_or("");
            if !set("_JAVA_BUILTIN_TYPES").contains(simple) {
                push(
                    out,
                    if preserve_qualified {
                        raw.clone()
                    } else {
                        simple.into()
                    },
                    generic,
                )
            }
        }
        "generic_type" => {
            if let Some(c) = first_child(node, &["type_identifier", "scoped_type_identifier"]) {
                let raw = read_text(c, source);
                let simple = raw.rsplit('.').next().unwrap_or("");
                if !set("_JAVA_BUILTIN_TYPES").contains(simple)
                    && (c.kind_name() == "scoped_type_identifier" || !skip.contains(simple))
                {
                    push(
                        out,
                        if preserve_qualified && c.kind_name() == "scoped_type_identifier" {
                            raw.clone()
                        } else {
                            simple.into()
                        },
                        generic,
                    );
                }
            }
            for args in children(node)
                .into_iter()
                .filter(|c| c.kind_name() == "type_arguments")
            {
                for c in named(args) {
                    java_collect_type_refs(
                        Some(c),
                        source,
                        true,
                        out,
                        Some(skip),
                        preserve_qualified,
                    )
                }
            }
        }
        _ => {
            if node.is_named() {
                for c in named(node) {
                    java_collect_type_refs(
                        Some(c),
                        source,
                        generic,
                        out,
                        Some(skip),
                        preserve_qualified,
                    )
                }
            }
        }
    }
}
pub fn java_receiver_type_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let node = node?;
    let name = match node.kind_name() {
        "type_identifier" => read_text(node, source),
        "scoped_type_identifier" => read_text(node, source)
            .rsplit('.')
            .next()
            .unwrap_or("")
            .into(),
        "generic_type" => {
            return java_receiver_type_name(
                first_child(node, &["type_identifier", "scoped_type_identifier"]),
                source,
            );
        }
        _ => return None,
    };
    (!name.is_empty()
        && !set("_JAVA_BUILTIN_TYPES").contains(&name)
        && !java_type_parameters_in_scope(node, source).contains(&name))
    .then_some(name)
}
pub fn java_declarator_names(node: Node<'_>, source: &[u8]) -> Vec<String> {
    children(node)
        .into_iter()
        .filter(|n| n.kind_name() == "variable_declarator")
        .filter_map(|n| n.child_by_field_name("name"))
        .map(|n| read_text(n, source))
        .filter(|s| !s.is_empty())
        .collect()
}
/// `_java_receiver_chain` (0.9.88, #4304): a receiver that is itself an
/// expression, described for the resolver to type from declared return types.
/// `new T()` / `(T) x` -> `["type", "T"]`, `f()` -> `["call", "f", null]`,
/// `x.f()` -> `["call", "f", <x>]`, and the leaves `["this"]`, `["super"]`,
/// `["name", "x"]`. None when any part is something else.
pub fn java_receiver_chain(node: Option<Node<'_>>, source: &[u8], depth: usize) -> Option<Value> {
    let node = node?;
    if depth > 16 {
        return None;
    }
    match node.kind_name() {
        "parenthesized_expression" => {
            let inner = named(node)
                .into_iter()
                .filter(|c| c.kind_name() != "comment")
                .collect::<Vec<_>>();
            if inner.len() == 1 {
                java_receiver_chain(Some(inner[0]), source, depth + 1)
            } else {
                None
            }
        }
        "object_creation_expression" | "cast_expression" => {
            java_receiver_type_name(node.child_by_field_name("type"), source)
                .map(|name| json!(["type", name]))
        }
        kind @ ("this" | "super") => Some(json!([kind])),
        "identifier" => Some(json!(["name", read_text(node, source)])),
        "field_access" => {
            let owner = node.child_by_field_name("object");
            let field = node.child_by_field_name("field");
            match (owner, field) {
                (Some(owner), Some(field)) if owner.kind_name() == "this" => Some(json!([
                    "name",
                    format!("this.{}", read_text(field, source))
                ])),
                _ => None,
            }
        }
        "method_invocation" => {
            let name = node.child_by_field_name("name")?;
            let inner = match node.child_by_field_name("object") {
                Some(receiver) => java_receiver_chain(Some(receiver), source, depth + 1)?,
                None => Value::Null,
            };
            Some(json!(["call", read_text(name, source), inner]))
        }
        _ => None,
    }
}
/// `_java_type_chain_leaf`: the `["name", x]` leaf of a receiver chain
/// replaced by x's declared type (`["type", T]`); a capitalized name nothing
/// declares is a type itself (`Foo.create().run()`). None when x is untyped.
pub fn java_type_chain_leaf(
    chain: &Value,
    receiver_types: &BTreeMap<String, String>,
) -> Option<Value> {
    match chain[0].as_str() {
        Some("call") => {
            if chain[2].is_null() {
                return Some(chain.clone());
            }
            let inner = java_type_chain_leaf(&chain[2], receiver_types)?;
            Some(json!(["call", chain[1], inner]))
        }
        Some("name") => {
            let name = chain[1].as_str().unwrap_or("");
            if let Some(declared) = receiver_types.get(name).filter(|s| !s.is_empty()) {
                return Some(json!(["type", declared]));
            }
            name.chars()
                .next()
                .is_some_and(char::is_uppercase)
                .then(|| json!(["type", name]))
        }
        _ => Some(chain.clone()),
    }
}
/// `_java_bound_names` (0.9.88, #4307): every name a Java method binds itself,
/// typed or not: parameters, locals (`var` too), lambda, catch, for-each,
/// resource and pattern variables. A receiver outside this set and the
/// class's own fields can only be a field the class inherits.
pub fn java_bound_names(method: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    const NAMED: [&str; 6] = [
        "formal_parameter",
        "variable_declarator",
        "enhanced_for_statement",
        "resource",
        "catch_formal_parameter",
        "instanceof_expression",
    ];
    const LISTED: [&str; 3] = [
        "inferred_parameters",
        "record_pattern_component",
        "type_pattern",
    ];
    let mut names = BTreeSet::new();
    let mut stack = vec![
        method.child_by_field_name("parameters"),
        method.child_by_field_name("body"),
    ];
    while let Some(entry) = stack.pop() {
        let Some(node) = entry else { continue };
        let kind = node.kind_name();
        if NAMED.contains(&kind) {
            if let Some(name) = node
                .child_by_field_name("name")
                .filter(|n| n.kind_name() == "identifier")
            {
                names.insert(read_text(name, source));
            }
        } else if LISTED.contains(&kind) {
            names.extend(
                children(node)
                    .into_iter()
                    .filter(|c| c.kind_name() == "identifier")
                    .map(|c| read_text(c, source)),
            );
        } else if kind == "lambda_expression"
            && let Some(params) = node
                .child_by_field_name("parameters")
                .filter(|p| p.kind_name() == "identifier")
        {
            names.insert(read_text(params, source));
        }
        stack.extend(children(node).into_iter().map(Some));
    }
    names
}
pub fn java_lambda_parameters(node: Node<'_>, source: &[u8]) -> Vec<(String, Option<String>)> {
    let Some(params) = node.child_by_field_name("parameters") else {
        return vec![];
    };
    match params.kind_name() {
        "identifier" => vec![(read_text(params, source), None)],
        "inferred_parameters" => children(params)
            .into_iter()
            .filter(|c| c.kind_name() == "identifier")
            .map(|n| (read_text(n, source), None))
            .collect(),
        _ => children(params)
            .into_iter()
            .filter(|c| matches!(c.kind_name(), "formal_parameter" | "spread_parameter"))
            .filter_map(|p| {
                p.child_by_field_name("name").map(|n| {
                    (
                        read_text(n, source),
                        java_receiver_type_name(p.child_by_field_name("type"), source),
                    )
                })
            })
            .collect(),
    }
}
fn bind(
    types: &mut BTreeMap<String, String>,
    ambiguous: &mut BTreeSet<String>,
    name: String,
    type_name: Option<String>,
) {
    let Some(type_name) = type_name.filter(|s| !s.is_empty()) else {
        return;
    };
    if name.is_empty() || ambiguous.contains(&name) {
        return;
    }
    if types
        .get(&name)
        .is_some_and(|previous| previous != &type_name)
    {
        types.remove(&name);
        ambiguous.insert(name);
    } else {
        types.insert(name, type_name);
    }
}
pub fn java_method_receiver_types(
    node: Node<'_>,
    source: &[u8],
    fields: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut types = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    if let Some(params) = node.child_by_field_name("parameters") {
        for p in children(params)
            .into_iter()
            .filter(|c| matches!(c.kind_name(), "formal_parameter" | "spread_parameter"))
        {
            if let Some(n) = p.child_by_field_name("name") {
                bind(
                    &mut types,
                    &mut ambiguous,
                    read_text(n, source),
                    java_receiver_type_name(p.child_by_field_name("type"), source),
                );
            }
        }
    }
    let mut stack = node
        .child_by_field_name("body")
        .map(children)
        .unwrap_or_default();
    while let Some(n) = stack.pop() {
        if matches!(
            n.kind_name(),
            "class_declaration"
                | "class_body"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "annotation_type_declaration"
        ) {
            continue;
        }
        if n.kind_name() == "lambda_expression" {
            for (name, type_name) in java_lambda_parameters(n, source) {
                if type_name.is_none()
                    || fields
                        .get(&name)
                        .is_some_and(|t| Some(t) != type_name.as_ref())
                {
                    types.remove(&name);
                    ambiguous.insert(name);
                } else {
                    bind(&mut types, &mut ambiguous, name, type_name);
                }
            }
        }
        if n.kind_name() == "local_variable_declaration" {
            let type_name = java_receiver_type_name(n.child_by_field_name("type"), source);
            for name in java_declarator_names(n, source) {
                if fields
                    .get(&name)
                    .is_some_and(|t| Some(t) != type_name.as_ref())
                {
                    types.remove(&name);
                    ambiguous.insert(name);
                } else {
                    bind(&mut types, &mut ambiguous, name, type_name.clone());
                }
            }
        }
        stack.extend(children(n));
    }
    let mut out = fields.clone();
    out.extend(types);
    out.retain(|name, _| !ambiguous.contains(name));
    out.extend(
        fields
            .iter()
            .map(|(name, t)| (format!("this.{name}"), t.clone())),
    );
    out
}
pub fn java_annotation_nodes(node: Node<'_>) -> Vec<Node<'_>> {
    first_child(node, &["modifiers"])
        .map(|m| {
            children(m)
                .into_iter()
                .filter(|c| matches!(c.kind_name(), "marker_annotation" | "annotation"))
                .collect()
        })
        .unwrap_or_default()
}
pub fn java_annotation_names(node: Node<'_>, source: &[u8]) -> Vec<(String, String)> {
    java_annotation_nodes(node)
        .into_iter()
        .filter_map(|a| {
            a.child_by_field_name("name")
                .or_else(|| first_child(a, &["identifier", "scoped_identifier", "type_identifier"]))
        })
        .filter_map(|n| {
            let raw = read_text(n, source);
            let text = raw.rsplit('.').next().unwrap_or("").to_owned();
            (!text.is_empty()).then_some((text, raw))
        })
        .collect()
}
pub fn java_annotation_class_literal_refs(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut names = vec![];
    for a in java_annotation_nodes(node) {
        if let Some(args) = a.child_by_field_name("arguments") {
            let mut stack = vec![args];
            while let Some(n) = stack.pop() {
                if n.kind_name() == "class_literal" {
                    let mut refs = vec![];
                    java_collect_type_refs(
                        named(n).first().copied(),
                        source,
                        false,
                        &mut refs,
                        None,
                        true,
                    );
                    names.extend(refs.into_iter().map(|(name, _)| name));
                    continue;
                }
                stack.extend(named(n));
            }
        }
    }
    names
}
pub fn php_name_text(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let name = read_text(node?, source)
        .rsplit('\\')
        .next()
        .unwrap_or("")
        .to_owned();
    (!name.is_empty()).then_some(name)
}
/// `_php_declared_type_name` (0.9.89): the single class a PHP type
/// annotation names (`Greeter`, `?Greeter`); a union, an intersection or a
/// primitive names no one class.
pub fn php_declared_type_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let mut n = node?;
    if n.kind_name() == "optional_type" {
        n = named(n).into_iter().next()?;
    }
    if n.kind_name() != "named_type" {
        return None;
    }
    php_name_text(first_child(n, &["name", "qualified_name"]), source)
}
/// `_php_variable_text`: the bare name a `variable_name` binds (`$g` -> `g`).
pub fn php_variable_text(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let n = node.filter(|n| n.kind_name() == "variable_name")?;
    first_child(n, &["name"]).map(|c| read_text(c, source))
}
/// `_php_first_declared_type`: the first single-class annotation among the
/// node's direct children.
fn php_first_declared_type(node: Node<'_>, source: &[u8]) -> Option<String> {
    children(node)
        .into_iter()
        .find_map(|c| php_declared_type_name(Some(c), source).filter(|t| !t.is_empty()))
}
/// `_php_receiver_type_table` (0.9.89): `name -> TypeName` for every PHP
/// receiver whose type is written down: a typed property, a constructor
/// promotion, a typed parameter and `$g = new Greeter()`. File-scoped and
/// flat; the first binding in source order wins.
pub fn php_receiver_type_table(
    root: Node<'_>,
    source: &[u8],
    table: &mut BTreeMap<String, String>,
) {
    let variable = |n: Node<'_>| {
        first_child(n, &["variable_name"])
            .and_then(|c| php_variable_text(Some(c), source))
            .filter(|t| !t.is_empty())
    };
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        match n.kind_name() {
            "property_declaration" => {
                if let Some(type_name) = php_first_declared_type(n, source) {
                    for element in children(n)
                        .into_iter()
                        .filter(|c| c.kind_name() == "property_element")
                    {
                        if let Some(name) = variable(element) {
                            table.entry(name).or_insert_with(|| type_name.clone());
                        }
                    }
                }
            }
            "property_promotion_parameter" | "simple_parameter" => {
                if let (Some(type_name), Some(name)) =
                    (php_first_declared_type(n, source), variable(n))
                {
                    table.entry(name).or_insert(type_name);
                }
            }
            "assignment_expression" => {
                if let Some(right) = n
                    .child_by_field_name("right")
                    .filter(|r| r.kind_name() == "object_creation_expression")
                    && let Some(name) = php_variable_text(n.child_by_field_name("left"), source)
                        .filter(|t| !t.is_empty())
                    && let Some(type_name) =
                        php_name_text(first_child(right, &["name", "qualified_name"]), source)
                {
                    table.entry(name).or_insert(type_name);
                }
            }
            _ => {}
        }
        stack.extend(children(n).into_iter().rev());
    }
}
pub fn php_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
) {
    let Some(n) = node else { return };
    match n.kind_name() {
        "primitive_type" => {}
        "named_type" => {
            if let Some(name) = php_name_text(first_child(n, &["name", "qualified_name"]), source) {
                push(out, name, generic)
            }
        }
        "name" | "qualified_name" => {
            if let Some(name) = php_name_text(Some(n), source) {
                push(out, name, generic)
            }
        }
        _ => {
            if n.is_named() {
                for c in named(n) {
                    php_collect_type_refs(Some(c), source, generic, out)
                }
            }
        }
    }
}
pub fn php_method_return_type_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut saw = false;
    for c in children(node) {
        if c.kind_name() == "formal_parameters" {
            saw = true;
            continue;
        }
        if saw
            && c.is_named()
            && matches!(
                c.kind_name(),
                "named_type"
                    | "primitive_type"
                    | "nullable_type"
                    | "union_type"
                    | "intersection_type"
                    | "optional_type"
            )
        {
            return Some(c);
        }
    }
    None
}
pub fn kotlin_user_type_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let mut name = None;
    for c in children(node?) {
        let n = if matches!(c.kind_name(), "identifier" | "type_identifier") {
            Some(c)
        } else if c.kind_name() == "simple_user_type" {
            first_child(c, &["identifier", "type_identifier"])
        } else {
            None
        };
        if let Some(n) = n {
            let text = read_text(n, source);
            if !text.is_empty() {
                name = Some(text);
            }
        }
    }
    name
}
const PHP_ROUTING_VERBS: [&str; 9] = [
    "any", "delete", "get", "map", "match", "options", "patch", "post", "put",
];
/// `VERB /path` for a PHP closure passed to a routing call, with the string
/// prefixes of enclosing route groups and fluent chains (`_php_get_route_name`,
/// #3409). A closure nested in a non-route closure gets no route name.
///
/// Upstream compares argument nodes by Python object identity, which never
/// matches, so the first string argument names the route wherever it sits.
pub fn php_route_name(closure: Node<'_>, source: &[u8]) -> Option<String> {
    let string = |n: Node<'_>| matches!(n.kind_name(), "string" | "encapsed_string");
    let unquote = |n: Node<'_>| read_text(n, source).trim_matches(['\'', '"']).to_string();
    let rooted = |p: String| {
        if p.starts_with('/') {
            p
        } else {
            format!("/{p}")
        }
    };
    let mut prefixes = vec![];
    let mut verb: Option<String> = None;
    let mut current = closure.parent();
    while let Some(node) = current {
        if matches!(
            node.kind_name(),
            "function_definition" | "method_declaration" | "class_declaration"
        ) {
            break;
        }
        if node.kind_name() == "argument"
            && let Some(arguments) = node.parent().filter(|a| a.kind_name() == "arguments")
            && let Some(call) = arguments.parent().filter(|c| {
                matches!(
                    c.kind_name(),
                    "member_call_expression"
                        | "function_call_expression"
                        | "scoped_call_expression"
                )
            })
        {
            let method = call
                .child_by_field_name("name")
                .or_else(|| call.child_by_field_name("function"))
                .map(|n| read_text(n, source).to_lowercase())
                .unwrap_or_default();
            let path = children(arguments).into_iter().find_map(|sibling| {
                let target = if sibling.kind_name() == "argument" {
                    children(sibling)
                        .into_iter()
                        .find(|c| string(*c))
                        .unwrap_or(sibling)
                } else {
                    sibling
                };
                string(target).then(|| unquote(target))
            });
            if verb.is_none() {
                match path {
                    Some(p)
                        if p.starts_with('/') && PHP_ROUTING_VERBS.contains(&method.as_str()) =>
                    {
                        verb = Some(method.to_uppercase());
                        prefixes.push(p);
                    }
                    _ => return None,
                }
            } else if let Some(p) = path.filter(|p| !p.is_empty()) {
                prefixes.push(rooted(p));
            }
            let mut fluent = call.child_by_field_name("object");
            while let Some(link) = fluent.filter(|f| f.kind_name() == "member_call_expression") {
                if let Some(args) = link.child_by_field_name("arguments") {
                    for arg in children(args)
                        .into_iter()
                        .filter(|c| c.kind_name() == "argument")
                    {
                        if let Some(s) = children(arg).into_iter().find(|c| string(*c)) {
                            let p = unquote(s);
                            if !p.is_empty() {
                                prefixes.push(rooted(p));
                            }
                        }
                    }
                }
                fluent = link.child_by_field_name("object");
            }
            current = call.parent();
            continue;
        }
        if matches!(node.kind_name(), "anonymous_function" | "arrow_function") && verb.is_none() {
            return None;
        }
        current = node.parent();
    }
    let verb = verb?;
    let segments = prefixes
        .iter()
        .rev()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    Some(format!("{verb} /{}", segments.join("/")))
}
/// Simple names of the annotations in a Kotlin declaration's `modifiers`
/// (`_kotlin_annotation_names`): `@Inject`, `@field:Inject`, `@Named("x")`
/// and bracketed lists such as `@[Inject VisibleForTesting]`.
pub fn kotlin_annotation_names(declaration: Node<'_>, source: &[u8]) -> Vec<String> {
    let Some(modifiers) = first_child(declaration, &["modifiers"]) else {
        return vec![];
    };
    let mut names = vec![];
    for annotation in children(modifiers)
        .into_iter()
        .filter(|c| c.kind_name() == "annotation")
    {
        for sub in children(annotation) {
            let user_type = match sub.kind_name() {
                "user_type" => Some(sub),
                "constructor_invocation" => first_child(sub, &["user_type"]),
                _ => None,
            };
            if let Some(name) = kotlin_user_type_name(user_type, source).filter(|n| !n.is_empty()) {
                names.push(name);
            }
        }
    }
    names
}
/// Type names used as class literals in a Kotlin declaration's annotation
/// arguments (`_kotlin_annotation_class_literal_refs`, 0.9.74 #3835):
/// `@ManyToOne(targetEntity = Customer::class)` yields `Customer`.
pub fn kotlin_annotation_class_literal_refs(declaration: Node<'_>, source: &[u8]) -> Vec<String> {
    let Some(modifiers) = first_child(declaration, &["modifiers"]) else {
        return vec![];
    };
    let mut names = vec![];
    let mut seen = BTreeSet::new();
    for annotation in children(modifiers)
        .into_iter()
        .filter(|c| c.kind_name() == "annotation")
    {
        for invocation in children(annotation)
            .into_iter()
            .filter(|c| c.kind_name() == "constructor_invocation")
        {
            let Some(args) = first_child(invocation, &["value_arguments"]) else {
                continue;
            };
            let mut stack = vec![args];
            while let Some(current) = stack.pop() {
                if current.kind_name() == "navigation_expression" {
                    let (mut double_colon, mut rhs_is_class, mut lhs) = (false, false, None);
                    for ch in children(current) {
                        let text = read_text(ch, source);
                        if ch.kind_name() == "::" || text == "::" {
                            double_colon = true;
                        } else if double_colon
                            && matches!(ch.kind_name(), "identifier" | "simple_identifier")
                            && text == "class"
                        {
                            rhs_is_class = true;
                        } else if !double_colon && ch.is_named() {
                            lhs = Some(ch);
                        }
                    }
                    if let Some(lhs) = lhs.filter(|_| double_colon && rhs_is_class) {
                        let raw = read_text(lhs, source);
                        let text = raw.rsplit('.').next().unwrap_or("").to_owned();
                        if !text.is_empty()
                            && !set("_KOTLIN_BUILTIN_TYPES").contains(&text)
                            && !set("_JAVA_BUILTIN_TYPES").contains(&text)
                            && seen.insert(text.clone())
                        {
                            names.push(text);
                        }
                        continue;
                    }
                }
                stack.extend(named(current));
            }
        }
    }
    names
}
fn kotlin_push(
    n: Node<'_>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
    skip: &BTreeSet<String>,
) {
    let text = read_text(n, source);
    if !set("_KOTLIN_BUILTIN_TYPES").contains(&text)
        && !set("_JAVA_BUILTIN_TYPES").contains(&text)
        && !skip.contains(&text)
    {
        push(out, text, generic)
    }
}
/// The names of the `type_parameter`s of every `type_parameters` list on
/// `node` and its ancestors, each the first child of one of `kinds`. The
/// generic type parameters in scope are local bindings, not corpus symbols:
/// a reference to one would mint a sourceless stub every `<T>` shares
/// (Graphify 0.9.89).
fn type_parameters_in_scope(
    node: Node<'_>,
    source: &[u8],
    list: &str,
    params: &[&str],
    kinds: &[&str],
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut scope = Some(node);
    while let Some(n) = scope {
        for child in children(n).into_iter().filter(|c| c.kind_name() == list) {
            for param in children(child)
                .into_iter()
                .filter(|p| params.contains(&p.kind_name()))
            {
                if let Some(sub) = children(param)
                    .into_iter()
                    .find(|c| kinds.contains(&c.kind_name()))
                {
                    let name = read_text(sub, source);
                    if !name.is_empty() {
                        names.insert(name);
                    }
                }
            }
        }
        scope = n.parent();
    }
    names
}
/// `_kotlin_type_parameters_in_scope` (0.9.89): `class Box<T>` / `fun <R>`.
pub fn kotlin_type_parameters_in_scope(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    type_parameters_in_scope(
        node,
        source,
        "type_parameters",
        &["type_parameter"],
        &["identifier", "type_identifier"],
    )
}
pub fn kotlin_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
) {
    let Some(n) = node else { return };
    let skip = kotlin_type_parameters_in_scope(n, source);
    kotlin_collect_type_refs_in(n, source, generic, out, &skip)
}
fn kotlin_collect_type_refs_in(
    n: Node<'_>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
    skip: &BTreeSet<String>,
) {
    match n.kind_name() {
        "integral_literal" | "boolean_literal" => {}
        "identifier" | "type_identifier" => kotlin_push(n, source, generic, out, skip),
        "user_type" => {
            for c in children(n) {
                if matches!(c.kind_name(), "identifier" | "type_identifier") {
                    kotlin_push(c, source, generic, out, skip);
                    break;
                }
                if c.kind_name() == "simple_user_type" {
                    if let Some(s) = first_child(c, &["identifier", "type_identifier"]) {
                        kotlin_push(s, source, generic, out, skip)
                    }
                    break;
                }
            }
            for args in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "type_arguments")
            {
                for arg in children(args) {
                    if arg.kind_name() == "type_projection" {
                        for sub in named(arg) {
                            kotlin_collect_type_refs_in(sub, source, true, out, skip)
                        }
                    } else if arg.is_named() {
                        kotlin_collect_type_refs_in(arg, source, true, out, skip)
                    }
                }
            }
        }
        _ => {
            if n.is_named() {
                for c in named(n) {
                    kotlin_collect_type_refs_in(c, source, generic, out, skip)
                }
            }
        }
    }
}
pub fn kotlin_property_type_node(node: Node<'_>) -> Option<Node<'_>> {
    for c in children(node) {
        if c.kind_name() == "variable_declaration"
            && let Some(n) = first_child(c, &["user_type", "nullable_type", "type_reference"])
        {
            return Some(n);
        }
        if matches!(
            c.kind_name(),
            "user_type" | "nullable_type" | "type_reference"
        ) {
            return Some(c);
        }
    }
    None
}
pub fn kotlin_function_return_type_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut params = false;
    let mut colon = false;
    for c in children(node) {
        if c.kind_name() == "function_value_parameters" {
            params = true;
            continue;
        }
        if params && c.kind_name() == ":" {
            colon = true;
            continue;
        }
        if colon && c.is_named() {
            return Some(c);
        }
    }
    None
}
pub fn swift_declaration_keyword(node: Node<'_>) -> Option<&str> {
    children(node)
        .into_iter()
        .find(|n| {
            !n.is_named()
                && matches!(
                    n.kind_name(),
                    "class" | "struct" | "enum" | "extension" | "actor"
                )
        })
        .map(|n| n.kind_name())
}
/// `_python_alias_name` (Graphify 0.9.95, #4578): the type name a module-level
/// statement declares as an explicit alias: `type Vector = ...` (PEP 695),
/// `Matrix: TypeAlias = ...` or `UserId = NewType("UserId", int)`. A bare
/// `X = Union[...]` is an alias by convention only. A same-name re-export
/// (`Field: TypeAlias = core.Field`) denotes the original type and declares
/// none; `NewType` always mints a new type, even under its own name.
pub fn python_alias_name(stmt: Node<'_>, source: &[u8]) -> Option<String> {
    let strip = |s: &str| -> String {
        s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
            .to_owned()
    };
    let before_bracket =
        |node: Node<'_>| strip(read_text(node, source).split('[').next().unwrap_or(""));
    let (name, right) = match stmt.kind_name() {
        "type_alias_statement" => (
            stmt.child_by_field_name("left")
                .map(before_bracket)
                .unwrap_or_default(),
            stmt.child_by_field_name("right"),
        ),
        "expression_statement" if named(stmt).len() == 1 => {
            let assign = named(stmt)[0];
            let left = (assign.kind_name() == "assignment")
                .then(|| assign.child_by_field_name("left"))
                .flatten()
                .filter(|left| left.kind_name() == "identifier")?;
            let name = read_text(left, source);
            let right = assign.child_by_field_name("right");
            match assign.child_by_field_name("type") {
                None => {
                    let callee = right
                        .filter(|r| r.kind_name() == "call")
                        .and_then(|r| r.child_by_field_name("function"));
                    return callee
                        .is_some_and(|c| {
                            ["NewType", "typing.NewType", "typing_extensions.NewType"]
                                .contains(&read_text(c, source).as_str())
                        })
                        .then_some(name);
                }
                Some(annotation) => {
                    if ![
                        "TypeAlias",
                        "typing.TypeAlias",
                        "typing_extensions.TypeAlias",
                    ]
                    .contains(&strip(&read_text(annotation, source)).as_str())
                    {
                        return None;
                    }
                }
            }
            (name, right)
        }
        _ => return None,
    };
    // `str.isidentifier()`.
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        || !chars.all(|c| c == '_' || c.is_alphanumeric())
    {
        return None;
    }
    let target = right.map(before_bracket).unwrap_or_default();
    if target.rsplit('.').next().unwrap_or("") == name {
        return None;
    }
    Some(name)
}
/// `_python_pre_scan_underscore_collisions`: names that collapse to one node id
/// in a Python module. Module-level functions, classes and explicit type
/// aliases share the module scope (`class Session` / `def session()`, `Foo` /
/// `FOO`, 0.9.92, #2810); a class's direct methods are keyed under the class's
/// own (possibly salted) id. Decorated definitions count as the definition
/// they wrap.
pub fn python_pre_scan_underscore_collisions(
    root: Node<'_>,
    source: &[u8],
    stem: &str,
) -> BTreeMap<String, BTreeSet<String>> {
    fn undecorated(node: Node<'_>) -> Node<'_> {
        if node.kind_name() == "decorated_definition" {
            node.child_by_field_name("definition").unwrap_or(node)
        } else {
            node
        }
    }
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut classes = vec![];
    for c in children(root).into_iter().map(undecorated) {
        if let Some(alias) = python_alias_name(c, source) {
            groups
                .entry(make_id([stem, &alias]))
                .or_default()
                .insert(alias);
        } else if c.kind_name() == "function_definition" {
            if let Some(n) = c.child_by_field_name("name") {
                let name = read_text(n, source);
                if !name.is_empty() {
                    groups
                        .entry(make_id([stem, &name]))
                        .or_default()
                        .insert(name);
                }
            }
        } else if c.kind_name() == "class_definition"
            && let (Some(n), Some(body)) =
                (c.child_by_field_name("name"), c.child_by_field_name("body"))
        {
            let class = read_text(n, source);
            if class.is_empty() {
                continue;
            }
            groups
                .entry(make_id([stem, &class]))
                .or_default()
                .insert(class.clone());
            classes.push((class, body));
        }
    }
    let mut module_groups = groups.clone();
    module_groups.retain(|_, names| names.len() >= 2);
    for (class, body) in classes {
        let id = python_underscore_salted_nid(&make_id([stem, &class]), &class, &module_groups);
        for member in children(body)
            .into_iter()
            .map(undecorated)
            .filter(|m| m.kind_name() == "function_definition")
        {
            if let Some(n) = member.child_by_field_name("name") {
                let name = read_text(n, source);
                if !name.is_empty() {
                    groups
                        .entry(make_id([id.as_str(), &name]))
                        .or_default()
                        .insert(name);
                }
            }
        }
    }
    groups.retain(|_, names| names.len() >= 2);
    groups
}
pub fn python_underscore_salted_nid(
    plain: &str,
    name: &str,
    groups: &BTreeMap<String, BTreeSet<String>>,
) -> String {
    underscore_salted_nid(plain, name, groups, &['_'])
}
/// `_underscore_salted_nid`: a name with no colliding sibling keeps its plain
/// id; in a collision the one public name keeps it and every other name is
/// salted with a hash of itself. `private` lists the prefixes that make a
/// name non-public (`_`; JS/TS also `$` and `#`, #4475).
pub fn underscore_salted_nid(
    plain: &str,
    name: &str,
    groups: &BTreeMap<String, BTreeSet<String>>,
    private: &[char],
) -> String {
    use sha1::Digest;
    let Some(names) = groups.get(plain).filter(|names| names.len() >= 2) else {
        return plain.into();
    };
    let public: Vec<_> = names.iter().filter(|n| !n.starts_with(private)).collect();
    if public.len() == 1 && name == public[0] {
        return plain.into();
    }
    let hash = format!("{:x}", sha1::Sha1::digest(name.as_bytes()));
    make_id([plain, &hash[..6]])
}
/// `_csharp_method_signature` (Graphify 0.9.95, #4584): a C# method's
/// parameter signature (`` `1:this Foo,ref int,params int[] ``), which tells
/// overloads apart, and the argument counts it accepts: `arity`, `min_arity`
/// for optional, `params` and extension parameters, `variadic` for `params`.
pub fn csharp_method_signature(node: Node<'_>, source: &[u8]) -> (String, Value) {
    let squeeze = |n: Node<'_>| read_text(n, source).split_whitespace().collect::<String>();
    let mut parts = vec![];
    let (mut arity, mut required) = (0i64, 0i64);
    let (mut variadic, mut extension) = (false, false);
    let params = node
        .child_by_field_name("parameters")
        .map(children)
        .unwrap_or_default();
    for (i, c) in params.iter().enumerate() {
        if c.kind_name() == "parameter" {
            let mut words = children(*c)
                .into_iter()
                .filter(|m| m.kind_name() == "modifier")
                .map(|m| read_text(m, source))
                .collect::<Vec<_>>();
            let this = words.iter().any(|m| m == "this");
            words.push(
                c.child_by_field_name("type")
                    .map(squeeze)
                    .unwrap_or_default(),
            );
            parts.push(words.join(" "));
            arity += 1;
            if this && arity == 1 {
                extension = true;
            }
            if !children(*c).into_iter().any(|x| x.kind_name() == "=") {
                required += 1;
            }
        } else if c.kind_name() == "params" {
            parts.push(format!(
                "params {}",
                params.get(i + 1).map(|n| squeeze(*n)).unwrap_or_default()
            ));
            arity += 1;
            variadic = true;
        }
    }
    let type_params = children(node)
        .into_iter()
        .filter(|c| c.kind_name() == "type_parameter_list")
        .flat_map(children)
        .filter(|p| matches!(p.kind_name(), "type_parameter" | "identifier"))
        .count();
    let mut signature = parts.join(",");
    if type_params > 0 {
        signature = format!("`{type_params}:{signature}");
    }
    let mut meta = json!({"arity": arity});
    // An extension method is also called as `x.M(rest)`, one argument short.
    let min_arity = if extension { required - 1 } else { required };
    if min_arity < arity {
        meta["min_arity"] = json!(min_arity);
    }
    if variadic {
        meta["variadic"] = json!(true);
    }
    (signature, meta)
}
/// `_csharp_argc`: the argument count of a C# invocation, `None` for anything
/// else (an object creation).
pub fn csharp_argc(call: Node<'_>) -> Option<i64> {
    if call.kind_name() != "invocation_expression" {
        return None;
    }
    Some(call.child_by_field_name("arguments").map_or(0, |args| {
        children(args)
            .into_iter()
            .filter(|c| c.kind_name() == "argument")
            .count() as i64
    }))
}
/// `_csharp_pick_overload`: the one overload a call with `argc` arguments can
/// mean, from `(id, metadata)` of the overloads of one name on one type.
/// Only the count is known at a call site, so two overloads that both accept
/// it (`M(int)` / `M(string)`) stay undecided.
pub fn csharp_pick_overload<'a>(
    overloads: impl IntoIterator<Item = (&'a str, &'a Value)>,
    argc: Option<i64>,
) -> Option<&'a str> {
    let argc = argc?;
    let mut fits = vec![];
    for (id, meta) in overloads {
        let arity = meta["arity"].as_i64()?;
        let low = meta["min_arity"].as_i64().unwrap_or(arity);
        if low <= argc && (argc <= arity || meta["variadic"] == true) {
            fits.push(id);
        }
    }
    (fits.len() == 1).then(|| fits[0])
}
pub fn swift_pre_scan(root: Node<'_>, source: &[u8]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut protocols = BTreeSet::new();
    let mut classes = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind_name() == "protocol_declaration" {
            if let Some(name) = n
                .child_by_field_name("name")
                .or_else(|| first_child(n, &["type_identifier"]))
            {
                let text = read_text(name, source);
                if !text.is_empty() {
                    protocols.insert(text);
                }
            }
        } else if n.kind_name() == "class_declaration"
            && swift_declaration_keyword(n)
                .is_some_and(|k| matches!(k, "class" | "struct" | "enum" | "actor"))
            && let Some(name) = n.child_by_field_name("name")
        {
            let text = read_text(name, source);
            if !text.is_empty() {
                classes.insert(text);
            }
        }
        stack.extend(children(n));
    }
    (protocols, classes)
}
pub fn swift_classify_base(
    name: &str,
    kind: Option<&str>,
    first: bool,
    protocols: &BTreeSet<String>,
    classes: &BTreeSet<String>,
) -> &'static str {
    if protocols.contains(name) {
        return "implements";
    }
    if classes.contains(name) {
        return "inherits";
    }
    if kind.is_some_and(|k| matches!(k, "struct" | "enum" | "extension" | "actor")) {
        "implements"
    } else if first {
        "inherits"
    } else {
        "implements"
    }
}
pub fn swift_user_type_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let name = read_text(first_child(node?, &["type_identifier"])?, source);
    (!name.is_empty()).then_some(name)
}
/// `_swift_type_parameters_in_scope` (0.9.89): `struct Stack<Element>`,
/// `func map<R>`; a constraint (`T: Equatable`) is a separate `user_type`.
pub fn swift_type_parameters_in_scope(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    type_parameters_in_scope(
        node,
        source,
        "type_parameters",
        &["type_parameter"],
        &["type_identifier"],
    )
}
pub fn swift_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
) {
    let Some(n) = node else { return };
    let skip = swift_type_parameters_in_scope(n, source);
    swift_collect_type_refs_in(n, source, generic, out, &skip)
}
fn swift_collect_type_refs_in(
    n: Node<'_>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
    skip: &BTreeSet<String>,
) {
    // `Self` names the enclosing type, not a corpus symbol: a reference would
    // mint a sourceless `self` stub every `-> Self` method collapses onto
    // (0.9.88, #4382); a generic parameter in scope likewise (0.9.89).
    let push_type = |out: &mut TypeRefs, text: String| {
        if text != "Self" && !skip.contains(&text) {
            push(out, text, generic)
        }
    };
    match n.kind_name() {
        "type_identifier" => push_type(out, read_text(n, source)),
        "user_type" => {
            if let Some(c) = first_child(n, &["type_identifier"]) {
                push_type(out, read_text(c, source))
            }
            for args in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "type_arguments")
            {
                for arg in named(args) {
                    swift_collect_type_refs_in(arg, source, true, out, skip)
                }
            }
        }
        _ => {
            if n.is_named() {
                for c in named(n) {
                    swift_collect_type_refs_in(c, source, generic, out, skip)
                }
            }
        }
    }
}
pub fn swift_property_type_node(node: Node<'_>) -> Option<Node<'_>> {
    first_child(node, &["type_annotation"])
}
pub fn swift_attribute_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    for modifiers in children(node)
        .into_iter()
        .filter(|c| c.kind_name() == "modifiers")
    {
        for attr in children(modifiers)
            .into_iter()
            .filter(|c| c.kind_name() == "attribute")
        {
            if first_child(attr, &["user_type"])
                .is_none_or(|n| read_text(n, source) != "Environment")
            {
                continue;
            }
            let Some(arg) = first_child(attr, &["navigation_expression"]) else {
                continue;
            };
            let parts = named(arg);
            if parts.len() != 2
                || parts[0].kind_name() != "simple_identifier"
                || parts[1].kind_name() != "navigation_suffix"
                || read_text(parts[1], source) != ".self"
            {
                continue;
            }
            let name = read_text(parts[0], source);
            if name.chars().next().is_some_and(char::is_uppercase) {
                return Some(name);
            }
        }
    }
    None
}
pub fn swift_factory_call(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    let first = node.child(0)?;
    if first.kind_name() != "navigation_expression" {
        return None;
    }
    let parts = named(first);
    if parts.len() != 2
        || parts[0].kind_name() != "simple_identifier"
        || parts[1].kind_name() != "navigation_suffix"
    {
        return None;
    }
    let head = read_text(parts[0], source);
    if !head.chars().next().is_some_and(char::is_uppercase) {
        return None;
    }
    let name = read_text(first_child(parts[1], &["simple_identifier"])?, source);
    (!name.is_empty()).then_some((head, name))
}
pub fn swift_property_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    for c in children(node) {
        if c.kind_name() == "pattern"
            && let Some(n) = first_child(c, &["simple_identifier"])
        {
            return Some(read_text(n, source));
        }
        if c.kind_name() == "simple_identifier" {
            return Some(read_text(c, source));
        }
    }
    None
}
pub fn swift_constructor_type(node: Node<'_>, source: &[u8]) -> Option<String> {
    let n = node.child(0)?;
    if n.kind_name() != "simple_identifier" {
        return None;
    }
    let text = read_text(n, source);
    text.chars()
        .next()
        .is_some_and(char::is_uppercase)
        .then_some(text)
}
pub fn swift_receiver_name(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let n = node?;
    if n.kind_name() == "simple_identifier" {
        return Some(read_text(n, source));
    }
    if n.kind_name() == "navigation_expression"
        && let Some(head) = n.child(0)
    {
        if head.kind_name() == "simple_identifier" {
            return Some(read_text(head, source));
        }
        if head.kind_name() == "self_expression" {
            for c in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "navigation_suffix")
            {
                if let Some(n) = first_child(c, &["simple_identifier"]) {
                    return Some(read_text(n, source));
                }
            }
        }
    }
    None
}
pub fn c_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
) {
    let Some(n) = node else { return };
    if set("_C_PRIMITIVE_TYPE_NODES").contains(n.kind_name()) {
        return;
    }
    if n.kind_name() == "type_identifier" {
        push(out, read_text(n, source), generic)
    } else if matches!(
        n.kind_name(),
        "pointer_declarator"
            | "reference_declarator"
            | "array_declarator"
            | "type_qualifier"
            | "type_descriptor"
            | "abstract_pointer_declarator"
            | "abstract_reference_declarator"
            | "abstract_array_declarator"
    ) {
        for c in named(n) {
            c_collect_type_refs(Some(c), source, generic, out)
        }
    }
}
/// `_cpp_type_parameters_in_scope` (0.9.89): the type parameters of every
/// enclosing `template<typename T, class... Us>`; a non-type parameter
/// (`int N`) is a value and is left alone.
pub fn cpp_type_parameters_in_scope(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    type_parameters_in_scope(
        node,
        source,
        "template_parameter_list",
        &[
            "type_parameter_declaration",
            "variadic_type_parameter_declaration",
        ],
        &["type_identifier"],
    )
}
/// `_CPP_DIALECT_TYPE_QUALIFIERS` (Graphify 0.9.93, #4550): GPU C++ dialects
/// route through the C++ extractor, but their address-space, execution-space
/// and function qualifiers are not C++, and tree-sitter-cpp reads a leading
/// `kernel` / `__global__` as the declaration's type. They are legal
/// identifiers in plain C++, so the filter is keyed by file extension.
pub fn cpp_dialect_qualifiers(path: &str) -> &'static [&'static str] {
    let suffix =
        crate::path_syntax::suffix(path.rsplit(['/', '\\']).next().unwrap_or("")).to_lowercase();
    match suffix.as_str() {
        ".metal" => &[
            "kernel",
            "vertex",
            "fragment",
            "device",
            "constant",
            "threadgroup",
            "threadgroup_imageblock",
            "thread",
            "ray_data",
            "object_data",
        ],
        ".cu" | ".cuh" => &[
            "__global__",
            "__device__",
            "__host__",
            "__shared__",
            "__constant__",
            "__managed__",
            "__restrict__",
            "__forceinline__",
            "__noinline__",
            "__grid_constant__",
            "__launch_bounds__",
        ],
        _ => &[],
    }
}
pub fn cpp_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
    dialect: &[&str],
) {
    let Some(n) = node else { return };
    if set("_C_PRIMITIVE_TYPE_NODES").contains(n.kind_name()) {
        return;
    }
    let skip = cpp_type_parameters_in_scope(n, source);
    cpp_collect_type_refs_in(n, source, generic, out, &skip, dialect)
}
fn cpp_collect_type_refs_in(
    n: Node<'_>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
    skip: &BTreeSet<String>,
    dialect: &[&str],
) {
    if set("_C_PRIMITIVE_TYPE_NODES").contains(n.kind_name()) {
        return;
    }
    let push_type = |out: &mut TypeRefs, text: String| {
        if !skip.contains(&text) && !dialect.contains(&text.as_str()) {
            push(out, text, generic)
        }
    };
    match n.kind_name() {
        "type_identifier" => push_type(out, read_text(n, source)),
        "qualified_identifier" => {
            if let Some(name) = n.child_by_field_name("name") {
                cpp_collect_type_refs_in(name, source, generic, out, skip, dialect)
            }
        }
        "template_type" => {
            if let Some(name) = n.child_by_field_name("name") {
                push_type(out, read_text(name, source))
            }
            if let Some(args) = n.child_by_field_name("arguments") {
                for c in named(args) {
                    cpp_collect_type_refs_in(c, source, true, out, skip, dialect)
                }
            }
        }
        "type_descriptor"
        | "pointer_declarator"
        | "reference_declarator"
        | "array_declarator"
        | "type_qualifier"
        | "abstract_pointer_declarator"
        | "abstract_reference_declarator"
        | "abstract_array_declarator" => {
            for c in named(n) {
                cpp_collect_type_refs_in(c, source, generic, out, skip, dialect)
            }
        }
        _ => {}
    }
}
/// `_cpp_declarator_type_name`: the name a typedef declarator introduces
/// (`Handle`, `*HandlePtr`, `(*Fn)(int)`, `Arr[4]`).
fn cpp_declarator_type_name(declarator: Node<'_>, source: &[u8]) -> Option<String> {
    let mut node = Some(declarator);
    while let Some(n) = node.filter(|n| n.kind_name() != "type_identifier") {
        let mut inner = n.child_by_field_name("declarator");
        if inner.is_none() && n.kind_name() == "parenthesized_declarator" {
            inner = named(n).into_iter().next();
        }
        node = inner;
    }
    node.map(|n| read_text(n, source))
}
/// `_cpp_type_last_name`: `Bar` for `ns::Bar` or `ns::Bar<T>`.
fn cpp_type_last_name(type_node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let mut node = type_node;
    if let Some(n) = node.filter(|n| n.kind_name() == "type_descriptor") {
        node = n.child_by_field_name("type");
    }
    node.filter(|n| n.kind_name() == "qualified_identifier")?;
    while let Some(n) = node.filter(|n| n.kind_name() == "qualified_identifier") {
        node = n.child_by_field_name("name");
    }
    if let Some(n) = node.filter(|n| n.kind_name() == "template_type") {
        node = n.child_by_field_name("name");
    }
    node.map(|n| read_text(n, source))
}
/// `_cpp_alias_names` (Graphify 0.9.95, #4580): the type names a namespace- or
/// file-scope `using X = ...;` or `typedef ... X;` declares. A member alias in
/// a class is not one (C++ member ids are not class-qualified, so they would
/// fold into one hub), nor the struct a C-style `typedef struct Foo {...} Foo;`
/// names, nor a same-name re-export (`using Bar = ns::Bar;`). An include guard
/// or `#if` block is a wrapper, not a scope.
pub fn cpp_alias_names(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut scope = node.parent();
    while let Some(s) = scope.filter(|s| {
        s.kind_name() == "template_declaration" || s.kind_name().starts_with("preproc_")
    }) {
        scope = s.parent();
    }
    if !scope.is_some_and(|s| matches!(s.kind_name(), "translation_unit" | "declaration_list")) {
        return vec![];
    }
    let target = node.child_by_field_name("type");
    let candidates: Vec<Option<String>> = if node.kind_name() == "alias_declaration" {
        node.child_by_field_name("name")
            .map(|n| read_text(n, source))
            .into_iter()
            .map(Some)
            .collect()
    } else {
        let mut cursor = node.walk();
        node.children_by_field_name("declarator", &mut cursor)
            .map(|d| cpp_declarator_type_name(d, source))
            .collect()
    };
    let mut seen: BTreeSet<String> = cpp_type_last_name(target, source).into_iter().collect();
    if let Some(t) = target.filter(|t| {
        matches!(
            t.kind_name(),
            "struct_specifier" | "class_specifier" | "union_specifier" | "enum_specifier"
        )
    }) && let Some(name) = t.child_by_field_name("name")
    {
        seen.insert(read_text(name, source));
    }
    let mut names = vec![];
    for name in candidates.into_iter().flatten() {
        if !name.is_empty() && seen.insert(name.clone()) {
            names.push(name);
        }
    }
    names
}
/// `_scala_type_parameters_in_scope` (0.9.89): `class Box[T]`, `def map[R]`,
/// variance-annotated `[+A]`/`[-B]` (the name one level deeper); a bound
/// names a real type and is not a parameter.
pub fn scala_type_parameters_in_scope(node: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut scope = Some(node);
    while let Some(n) = scope {
        for child in children(n)
            .into_iter()
            .filter(|c| c.kind_name() == "type_parameters")
        {
            for param in children(child) {
                let name = match param.kind_name() {
                    "identifier" | "type_identifier" => Some(param),
                    "covariant_type_parameter" | "contravariant_type_parameter" => children(param)
                        .into_iter()
                        .find(|c| matches!(c.kind_name(), "identifier" | "type_identifier")),
                    _ => None,
                };
                if let Some(name) = name.map(|c| read_text(c, source)).filter(|t| !t.is_empty()) {
                    names.insert(name);
                }
            }
        }
        scope = n.parent();
    }
    names
}
pub fn scala_collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
) {
    let Some(n) = node else { return };
    let skip = scala_type_parameters_in_scope(n, source);
    scala_collect_type_refs_in(n, source, generic, out, &skip)
}
fn scala_collect_type_refs_in(
    n: Node<'_>,
    source: &[u8],
    generic: bool,
    out: &mut TypeRefs,
    skip: &BTreeSet<String>,
) {
    let push_type = |out: &mut TypeRefs, text: String| {
        if !skip.contains(&text) {
            push(out, text, generic)
        }
    };
    match n.kind_name() {
        "type_identifier" => push_type(out, read_text(n, source)),
        "generic_type" => {
            if let Some(base) = n
                .child_by_field_name("type")
                .or_else(|| first_child(n, &["type_identifier"]))
                && base.kind_name() == "type_identifier"
            {
                push_type(out, read_text(base, source))
            }
            for args in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "type_arguments")
            {
                for c in named(args) {
                    scala_collect_type_refs_in(c, source, true, out, skip)
                }
            }
        }
        "compound_type" | "infix_type" | "function_type" | "tuple_type" | "annotated_type"
        | "projected_type" | "match_type" | "type_case_clause" => {
            for c in named(n) {
                scala_collect_type_refs_in(c, source, generic, out, skip)
            }
        }
        _ => {}
    }
}
/// The types a `type_parameters` node's context bounds name, recursing into
/// a higher-kinded parameter's own brackets (`F[_]: Async: Temporal`).
pub fn scala_collect_context_bounds(tp: Node<'_>, source: &[u8], out: &mut TypeRefs) {
    for c in children(tp) {
        match c.kind_name() {
            "type_parameters" => scala_collect_context_bounds(c, source, out),
            "context_bound" => {
                scala_collect_type_refs(c.child_by_field_name("type"), source, false, out)
            }
            _ => {}
        }
    }
}
/// `_scala_given_synthetic_name` (0.9.85, #4130): a stable label for an
/// anonymous Scala 3 given (`given Show[Int] with`), from the implemented
/// type's base name, so its members stay scoped to the given.
pub fn scala_given_synthetic_name(node: Node<'_>, source: &[u8]) -> String {
    for c in children(node) {
        let base = match c.kind_name() {
            "generic_type" => children(c).into_iter().find(|cc| {
                matches!(
                    cc.kind_name(),
                    "type_identifier" | "stable_type_identifier" | "identifier"
                )
            }),
            "type_identifier" | "stable_type_identifier" => Some(c),
            _ => None,
        };
        if let Some(base) = base {
            let text = read_text(base, source);
            let text = text.trim_matches(crate::dedup_strings::whitespace);
            if !text.is_empty() {
                return format!("given {text}");
            }
        }
    }
    "given".into()
}
/// `_python_class_ref` (0.9.80, #4198): the single class a Python annotation
/// or constructor callee names. `Request`, `httpx.Request`, `"Request"`,
/// `Optional[Request]` and `Request | None` all name `Request`; a container,
/// a union of two classes or a lowercase callee names none.
pub fn python_class_ref(node: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let node = node?;
    let upper = |s: &str| s.chars().next().is_some_and(char::is_uppercase);
    match node.kind_name() {
        "type" => {
            let parts = named(node);
            if parts.len() == 1 {
                python_class_ref(Some(parts[0]), source)
            } else {
                None
            }
        }
        "identifier" => Some(read_text(node, source)).filter(|name| upper(name)),
        "attribute" => python_class_ref(node.child_by_field_name("attribute"), source),
        "string" => {
            let text = read_text(node, source);
            let name = text
                .trim_matches(['"', '\''])
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim_matches(crate::dedup_strings::whitespace)
                .to_string();
            (crate::extract_python::is_identifier(&name) && upper(&name)).then_some(name)
        }
        "subscript" => {
            let value = node.child_by_field_name("value")?;
            let subs = node
                .children_by_field_name("subscript", &mut node.walk())
                .collect::<Vec<_>>();
            if read_text(value, source).rsplit('.').next() == Some("Optional") && subs.len() == 1 {
                python_class_ref(Some(subs[0]), source)
            } else {
                None
            }
        }
        "generic_type" => {
            let parts = named(node);
            if parts.len() == 2
                && parts[0].kind_name() == "identifier"
                && read_text(parts[0], source) == "Optional"
                && parts[1].kind_name() == "type_parameter"
            {
                let inner = named(parts[1]);
                if inner.len() == 1 {
                    return python_class_ref(Some(inner[0]), source);
                }
            }
            None
        }
        "binary_operator" => {
            let left = node.child_by_field_name("left");
            let right = node.child_by_field_name("right");
            if right.is_some_and(|r| r.kind_name() == "none") {
                return python_class_ref(left, source);
            }
            if left.is_some_and(|l| l.kind_name() == "none") {
                return python_class_ref(right, source);
            }
            None
        }
        _ => None,
    }
}
pub fn python_collect_param_refs(node: Option<Node<'_>>, source: &[u8]) -> TypeRefs {
    let mut out = vec![];
    if let Some(n) = node {
        for child in children(n)
            .into_iter()
            .filter(|c| matches!(c.kind_name(), "typed_parameter" | "typed_default_parameter"))
        {
            python_collect_type_refs(child.child_by_field_name("type"), source, false, &mut out)
        }
    }
    out
}
pub fn python_param_names(node: Option<Node<'_>>, source: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(n) = node else { return out };
    for c in children(n) {
        let name = match c.kind_name() {
            "identifier" => Some(c),
            "typed_parameter"
            | "default_parameter"
            | "typed_default_parameter"
            | "list_splat_pattern"
            | "dictionary_splat_pattern" => c
                .child_by_field_name("name")
                .or_else(|| first_child(c, &["identifier"])),
            _ => None,
        };
        if let Some(n) = name {
            out.insert(read_text(n, source));
        }
    }
    out
}
#[cfg(test)]
mod python_type_ref_tests {
    use super::*;
    fn refs(src: &str) -> Vec<(String, String)> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let func = tree.root_node().child(0).unwrap();
        let mut out = vec![];
        python_collect_type_refs(
            func.child_by_field_name("return_type"),
            src.as_bytes(),
            false,
            &mut out,
        );
        out.extend(python_collect_param_refs(
            func.child_by_field_name("parameters"),
            src.as_bytes(),
        ));
        out
    }
    fn s(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }
    // Expected tuples were captured from pinned engine.py::_python_collect_type_refs
    // under py-tree-sitter 0.25.2. Its `c is value` identity test never matches the
    // fresh Node that child_by_field_name returns, so a subscript's attribute value
    // is emitted twice ((name, type), then (name, generic_arg)); the port matches.
    #[test]
    fn attribute_subscript_emits_value_twice_like_upstream() {
        assert_eq!(
            refs("def f() -> typing.ItemsView[str, str]: pass"),
            s(&[("ItemsView", "type"), ("ItemsView", "generic_arg")])
        );
        assert_eq!(
            refs("def f() -> a.B[C, D.E[F]]: pass"),
            s(&[
                ("B", "type"),
                ("B", "generic_arg"),
                ("C", "generic_arg"),
                ("E", "generic_arg"),
                ("E", "generic_arg"),
                ("F", "generic_arg"),
            ])
        );
        assert_eq!(
            refs("def f(x: a.B[C]) -> None: pass"),
            s(&[("B", "type"), ("B", "generic_arg"), ("C", "generic_arg")])
        );
    }
    #[test]
    fn identifier_generics_and_containers_are_unchanged() {
        assert_eq!(
            refs("def f() -> Foo[T]: pass"),
            s(&[("Foo", "type"), ("T", "generic_arg")])
        );
        assert_eq!(
            refs("def f() -> list[Foo]: pass"),
            s(&[("Foo", "generic_arg")])
        );
        assert_eq!(
            refs("def f(x: Mapping[str, Foo[T]]) -> None: pass"),
            s(&[("Foo", "generic_arg"), ("T", "generic_arg")])
        );
    }
}
