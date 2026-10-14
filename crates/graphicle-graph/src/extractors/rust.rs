// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/rust.py
//! Rust source extraction with trait members and conservative impl ownership.
use super::base::{file_stem, is_language_builtin, make_id, read_text};
use crate::{fsutil, io_compat, path_syntax, regex_compat};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};
use tree_sitter::Node;
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn named(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    children(node).filter(|n| n.is_named())
}
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
const PAYLOAD_TYPES: &[&str] = &[
    "type_identifier",
    "generic_type",
    "scoped_type_identifier",
    "reference_type",
    "primitive_type",
    "tuple_type",
    "array_type",
];
pub const TRAIT_METHOD_BLOCKLIST: &[&str] = &[
    "new",
    "default",
    "parse",
    "from_str",
    "now",
    "clone",
    "into",
    "from",
    "to_string",
    "to_owned",
    "len",
    "is_empty",
    "iter",
    "next",
    "build",
    "start",
    "run",
    "init",
    "app",
    "get",
    "set",
    "push",
    "pop",
    "insert",
    "remove",
    "contains",
    "collect",
    "map",
    "filter",
    "unwrap",
    "expect",
    "ok",
    "err",
    "some",
    "none",
    "send",
    "recv",
    "lock",
    "read",
    "write",
];
/// Rust prelude / std types. Language-local rather than part of the shared
/// builtin globals: `Path`, `Result`, `Error` and friends are ordinary user
/// type names elsewhere. Without it every `Option`/`Vec`/`String` annotation
/// resolves to a few canonical nodes that become the top god nodes.
pub const RUST_BUILTIN_TYPES: &[&str] = &[
    "String",
    "str",
    "Option",
    "Result",
    "Vec",
    "VecDeque",
    "Box",
    "Rc",
    "Arc",
    "Weak",
    "RefCell",
    "Cell",
    "Cow",
    "Pin",
    "HashMap",
    "HashSet",
    "BTreeMap",
    "BTreeSet",
    "BinaryHeap",
    "Path",
    "PathBuf",
    "OsStr",
    "OsString",
    "CStr",
    "CString",
    "Duration",
    "Instant",
    "SystemTime",
    "Ordering",
    "Range",
    "RangeInclusive",
    "RangeFrom",
    "RangeTo",
    "RangeFull",
    "PhantomData",
    "ManuallyDrop",
    "NonZeroU8",
    "NonZeroU16",
    "NonZeroU32",
    "NonZeroU64",
    "NonZeroUsize",
    "Some",
    "None",
    "Ok",
    "Err",
    "Self",
    "Default",
    "Clone",
    "Copy",
    "Debug",
    "Display",
    "Error",
    "From",
    "Into",
    "TryFrom",
    "TryInto",
    "AsRef",
    "AsMut",
    "Iterator",
    "IntoIterator",
    "Extend",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Hash",
    "Send",
    "Sync",
    "Sized",
    "Drop",
    "Deref",
    "DerefMut",
    "Future",
    "Fn",
    "FnMut",
    "FnOnce",
];
const RUST_PRIMITIVE_TYPES: &[&str] = &[
    "bool", "char", "str", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16",
    "u32", "u64", "u128", "usize",
];
/// `_rust_type_last_segment`: `crate::a::Foo<T>` -> `Foo`, the last path
/// segment without generic arguments; a leading `&`, `&'a mut` or `dyn` is
/// not part of it.
pub fn type_last_segment(text: &str) -> String {
    static LEAD: LazyLock<regex::Regex> =
        LazyLock::new(|| regex_compat::compile(r"^(?:&\s*(?:'\w+\s+)?(?:mut\s+)?|dyn\s+)"));
    let text = LEAD.replace(trim(text), "");
    let mut depth = 0usize;
    let kept: String = text
        .chars()
        .filter(|&ch| match ch {
            '<' => {
                depth += 1;
                false
            }
            '>' => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0,
        })
        .collect();
    kept.split("::")
        .map(trim)
        .filter(|s| !s.is_empty())
        .last()
        .unwrap_or("")
        .to_string()
}
/// `_rust_qualifier_type`: the type a `Type::f()` call is qualified by
/// (`Foo`, `crate::a::Foo`, `Foo::<T>`, `i32`; `Self` as written), or `None`
/// for a module path (`fs::read`, `self::helper`) or `<T as Trait>`.
fn qualifier_type(path: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    let last = type_last_segment(&read_text(path?, source));
    (last.chars().next().is_some_and(char::is_uppercase)
        || RUST_PRIMITIVE_TYPES.contains(&last.as_str()))
    .then_some(last)
}
/// `_rust_use_module`: the name a plain `use` imports from, the last path
/// segment of its argument as written (`a::b` -> `b`, `a::b as c` ->
/// `b as c`, `a::*` -> `a`).
pub fn use_module(argument: &str) -> String {
    let clean = argument
        .split('{')
        .next()
        .unwrap_or("")
        .trim_end_matches(':')
        .trim_end_matches('*')
        .trim_end_matches(':');
    trim(clean.split("::").last().unwrap_or("")).to_string()
}
/// `_rust_use_bindings`: each binding of a `use` tree as the argument of the
/// plain `use` that binds the same name, with its line: `a::{b, c as d,
/// e::{self}, f::*}` gives `a::b`, `a::c as d`, `a::e` and `a::f::*`.
pub fn use_bindings(
    node: Node<'_>,
    source: &[u8],
    prefix: &str,
    self_path: &str,
) -> Vec<(String, usize)> {
    let line = node.start_position().row + 1;
    match node.kind() {
        "use_list" => {
            return named(node)
                .flat_map(|child| use_bindings(child, source, prefix, self_path))
                .collect();
        }
        "scoped_use_list" => {
            let Some(group) = node.child_by_field_name("list") else {
                return vec![];
            };
            let lead = format!(
                "{prefix}{}",
                super::base::read_text_range(node.start_byte(), group.start_byte(), source)
            );
            let owner = node
                .child_by_field_name("path")
                .map_or(String::new(), |path| {
                    format!("{prefix}{}", read_text(path, source))
                });
            return use_bindings(group, source, &lead, &owner);
        }
        _ => {}
    }
    let (node, alias) = if node.kind() == "use_as_clause" {
        match node.child_by_field_name("path") {
            Some(path) => (
                Some(path),
                super::base::read_text_range(path.end_byte(), node.end_byte(), source),
            ),
            None => (None, String::new()),
        }
    } else {
        (Some(node), String::new())
    };
    let Some(node) = node else {
        return vec![];
    };
    if node.kind() == "self" && !self_path.is_empty() {
        // `a::{self}` binds `a`.
        return vec![(format!("{self_path}{alias}"), line)];
    }
    if matches!(
        node.kind(),
        "use_wildcard" | "identifier" | "scoped_identifier" | "crate" | "self" | "super"
    ) {
        return vec![(format!("{prefix}{}{alias}", read_text(node, source)), line)];
    }
    vec![]
}
/// A prelude name is dropped unless `local_types` holds it. Type references
/// pass the declared struct/enum/trait names (a local `struct Result<T>`
/// shadows the prelude; a `type Result<T> = ...` alias does not), as
/// upstream's `local_type_names`; the call filter uses every local type name.
fn keep_type(text: &str, local_types: &HashSet<String>) -> bool {
    !text.is_empty() && (!RUST_BUILTIN_TYPES.contains(&text) || local_types.contains(text))
}
pub fn collect_type_refs(
    node: Option<Node<'_>>,
    source: &[u8],
    generic: bool,
    out: &mut Vec<(String, String)>,
    local_types: &HashSet<String>,
) {
    let Some(node) = node else {
        return;
    };
    match node.kind() {
        "primitive_type" => return,
        "type_identifier" | "scoped_type_identifier" => {
            let text = read_text(node, source);
            let text = if node.kind() == "scoped_type_identifier" {
                text.rsplit("::").next().unwrap_or("")
            } else {
                &text
            };
            // The keyword `Self` is declared nowhere; as a type ref it would
            // mint one corpus-wide sourceless stub (upstream PR #2963).
            let keyword = node.kind() == "type_identifier" && text == "Self";
            if keep_type(text, local_types) && !keyword {
                out.push((
                    text.into(),
                    if generic { "generic_arg" } else { "type" }.into(),
                ));
            }
            return;
        }
        "generic_type" => {
            if let Some(name) = node.child_by_field_name("type").or_else(|| {
                children(node)
                    .find(|n| matches!(n.kind(), "type_identifier" | "scoped_type_identifier"))
            }) {
                let text = read_text(name, source);
                let text = text.rsplit("::").next().unwrap_or("");
                if keep_type(text, local_types) {
                    out.push((
                        text.into(),
                        if generic { "generic_arg" } else { "type" }.into(),
                    ));
                }
            }
            for args in children(node).filter(|n| n.kind() == "type_arguments") {
                for arg in named(args) {
                    collect_type_refs(Some(arg), source, true, out, local_types);
                }
            }
            return;
        }
        _ => {}
    }
    if node.is_named() {
        for n in named(node) {
            collect_type_refs(Some(n), source, generic, out, local_types);
        }
    }
}
pub fn simple_generic_impl_key(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.child_by_field_name("trait").is_some() {
        return None;
    }
    let parameters = node.child_by_field_name("type_parameters")?;
    let owner_type = node.child_by_field_name("type")?;
    if owner_type.kind() != "generic_type" || named(node).any(|n| n.kind() == "where_clause") {
        return None;
    }
    let mut params = Vec::new();
    for parameter in named(parameters) {
        if parameter.kind() != "type_parameter" {
            return None;
        }
        let names: Vec<_> = named(parameter).collect();
        if names.len() != 1 || names[0].kind() != "type_identifier" {
            return None;
        }
        let name = read_text(names[0], source);
        if name.is_empty() || params.contains(&name) {
            return None;
        }
        params.push(name);
    }
    if params.is_empty() {
        return None;
    }
    let owner = owner_type.child_by_field_name("type")?;
    if owner.kind() != "type_identifier" {
        return None;
    }
    let arguments = named(owner_type).find(|n| n.kind() == "type_arguments")?;
    let mut args = Vec::new();
    for arg in named(arguments) {
        if arg.kind() != "type_identifier" {
            return None;
        }
        args.push(read_text(arg, source));
    }
    if args != params {
        return None;
    }
    let owner = read_text(owner, source);
    if owner.is_empty() {
        None
    } else {
        Some(format!("{owner}/{}", params.len()))
    }
}
struct Body<'tree> {
    id: String,
    node: Node<'tree>,
    typ: Option<String>,
    key: Option<String>,
}
struct Extractor<'source, 'tree> {
    source: &'source [u8],
    path: String,
    stem: String,
    file: String,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    seen: HashSet<String>,
    bodies: Vec<Body<'tree>>,
    impl_keys: HashMap<String, Option<String>>,
    /// Bare names of free functions and types: what a bare `f()` can reach.
    labels: HashMap<String, String>,
    /// Bare names of methods: what `x.f()` can reach (#4238).
    method_labels: HashMap<String, String>,
    /// (bare impl type, method name) -> the method nodes this file's `impl`
    /// blocks define, for in-file `Type::f()` / `Self::f()` / `self.f()`.
    impl_methods: HashMap<(String, String), BTreeSet<String>>,
    pairs: HashSet<(String, String)>,
    raw_calls: Vec<Value>,
    /// Every name this file defines as a type (struct/enum/trait/union/alias):
    /// the builtin filter of call names.
    local_types: HashSet<String>,
    /// Types a forward reference may bind to: the only file-level item behind
    /// their (casefolded) id (#3782). Also the builtin filter of type
    /// references (upstream rebinds `local_type_names` to this set before
    /// walking), so an alias such as `type Result<T> = ...` mints no node.
    forward_types: HashSet<String>,
    /// Data definitions (labels without `()`): a call onto one constructs.
    type_ids: HashSet<String>,
    /// `macro_rules!` macros defined in this file by bare name, kept apart
    /// from `labels` so `vec!` never cross-binds with a `vec` fn (0.9.75).
    macros: HashMap<String, String>,
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
        let local = make_id([self.stem.as_str(), name]);
        if self.seen.contains(&local) || self.forward_types.contains(name) {
            return local;
        }
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    fn types(&self, node: Option<Node<'_>>) -> Vec<(String, String)> {
        let mut refs = vec![];
        collect_type_refs(node, self.source, false, &mut refs, &self.forward_types);
        refs
    }
    fn refs(&mut self, node: Option<Node<'_>>, from: &str, line: usize, context: &str) {
        for (name, role) in self.types(node) {
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
    fn signatures(&mut self, node: Node<'_>, id: &str, line: usize) {
        if let Some(params) = node.child_by_field_name("parameters") {
            for p in children(params).filter(|n| n.kind() == "parameter") {
                self.refs(p.child_by_field_name("type"), id, line, "parameter_type");
            }
        }
        self.refs(
            node.child_by_field_name("return_type"),
            id,
            line,
            "return_type",
        );
    }
    fn walk(
        &mut self,
        node: Node<'tree>,
        parent: Option<&str>,
        parent_type: Option<&str>,
        parent_key: Option<&str>,
    ) {
        let line = node.start_position().row + 1;
        match node.kind() {
            "function_item" | "function_signature_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let name = read_text(name, self.source);
                    let id = if let Some(parent) = parent.filter(|s| !s.is_empty()) {
                        let id = make_id([parent, name.as_str()]);
                        self.node(&id, &format!(".{name}()"), line);
                        self.edge(parent, &id, "method", line, None);
                        if node.kind() == "function_item"
                            && let Some(owner) = parent_type.filter(|s| !s.is_empty())
                        {
                            self.impl_methods
                                .entry((type_last_segment(owner), name.clone()))
                                .or_default()
                                .insert(id.clone());
                        }
                        id
                    } else {
                        let id = make_id([self.stem.as_str(), name.as_str()]);
                        self.node(&id, &format!("{name}()"), line);
                        self.edge(&self.file.clone(), &id, "contains", line, None);
                        id
                    };
                    self.signatures(node, &id, line);
                    if node.kind() == "function_item"
                        && let Some(body) = node.child_by_field_name("body")
                    {
                        self.bodies.push(Body {
                            id,
                            node: body,
                            typ: parent_type.map(str::to_string),
                            key: parent_key.map(str::to_string),
                        });
                    }
                }
                return;
            }
            "macro_definition" => {
                // `macro_rules! name { ... }`: a macro-qualified id so it never
                // collides with a same-named fn or type; `!` marks the label.
                if let Some(name) = node.child_by_field_name("name") {
                    let name = read_text(name, self.source);
                    if !name.is_empty() {
                        let id = make_id([self.stem.as_str(), "macro", name.as_str()]);
                        self.node(&id, &format!("{name}!"), line);
                        self.edge(&self.file.clone(), &id, "contains", line, None);
                        self.macros.entry(name).or_insert(id);
                    }
                }
                return;
            }
            "struct_item" | "enum_item" | "trait_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let name = read_text(name, self.source);
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.node(&id, &name, line);
                    if let Some(declaration) = self.nodes.iter_mut().find(|n| n["id"] == id) {
                        declaration["_rust_declaration_count"] =
                            json!(declaration["_rust_declaration_count"].as_u64().unwrap_or(0) + 1);
                    }
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    if node.kind() == "trait_item" {
                        for sub in children(node)
                            .filter(|n| n.kind() == "trait_bounds")
                            .flat_map(children)
                            .filter(|n| n.is_named())
                        {
                            for (idx, (name, _)) in self.types(Some(sub)).into_iter().enumerate() {
                                let target = self.named(&name);
                                if target != id {
                                    if idx == 0 {
                                        self.edge(&id, &target, "inherits", line, None);
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
                    if node.kind() == "struct_item" {
                        for field in children(node)
                            .filter(|n| n.kind() == "field_declaration_list")
                            .flat_map(children)
                            .filter(|n| n.kind() == "field_declaration")
                        {
                            let typ = field.child_by_field_name("type").or_else(|| {
                                children(field).find(|n| {
                                    matches!(
                                        n.kind(),
                                        "type_identifier"
                                            | "generic_type"
                                            | "scoped_type_identifier"
                                            | "reference_type"
                                            | "primitive_type"
                                    )
                                })
                            });
                            self.refs(typ, &id, field.start_position().row + 1, "field");
                        }
                        for list in
                            children(node).filter(|n| n.kind() == "ordered_field_declaration_list")
                        {
                            for typ in children(list).filter(|n| PAYLOAD_TYPES.contains(&n.kind()))
                            {
                                self.refs(Some(typ), &id, list.start_position().row + 1, "field");
                            }
                        }
                    }
                    if node.kind() == "enum_item" {
                        for variant in children(node)
                            .filter(|n| n.kind() == "enum_variant_list")
                            .flat_map(children)
                            .filter(|n| n.kind() == "enum_variant")
                        {
                            // Each variant is a node with a `case_of` edge back to the enum.
                            let vline = variant.start_position().row + 1;
                            if let Some(vname) =
                                children(variant).find(|n| n.kind() == "identifier")
                            {
                                let vname = read_text(vname, self.source);
                                if !vname.is_empty() {
                                    let variant_id = make_id([id.as_str(), vname.as_str()]);
                                    self.node(&variant_id, &vname, vline);
                                    self.edge(&id, &variant_id, "case_of", vline, None);
                                }
                            }
                            for n in children(variant) {
                                if n.kind() == "ordered_field_declaration_list" {
                                    for typ in
                                        children(n).filter(|n| PAYLOAD_TYPES.contains(&n.kind()))
                                    {
                                        self.refs(
                                            Some(typ),
                                            &id,
                                            variant.start_position().row + 1,
                                            "field",
                                        );
                                    }
                                } else if n.kind() == "field_declaration_list" {
                                    for field in
                                        children(n).filter(|n| n.kind() == "field_declaration")
                                    {
                                        self.refs(
                                            field.child_by_field_name("type"),
                                            &id,
                                            field.start_position().row + 1,
                                            "field",
                                        );
                                    }
                                }
                            }
                        }
                    }
                    if node.kind() == "trait_item"
                        && let Some(body) = node.child_by_field_name("body")
                    {
                        for n in children(body) {
                            self.walk(n, Some(&id), None, None);
                        }
                    }
                }
                return;
            }
            "static_item" | "const_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let name = read_text(name, self.source);
                    // `const _: () = ...;` is anonymous: an id of the stem alone
                    // would replace the file node (Graphify 0.9.77).
                    if name == "_" {
                        return;
                    }
                    let id = if let Some(parent) = parent.filter(|s| !s.is_empty()) {
                        let id = make_id([parent, name.as_str()]);
                        self.node(&id, &format!(".{name}"), line);
                        self.edge(parent, &id, "contains", line, None);
                        id
                    } else {
                        let id = make_id([self.stem.as_str(), name.as_str()]);
                        self.node(&id, &name, line);
                        self.edge(&self.file.clone(), &id, "contains", line, None);
                        id
                    };
                    self.refs(node.child_by_field_name("type"), &id, line, "field");
                }
                return;
            }
            "impl_item" => {
                let mut id = None;
                let mut typ: Option<String> = None;
                let mut key = None;
                if let Some(n) = node.child_by_field_name("type") {
                    let name = trim(&read_text(n, self.source)).to_string();
                    let nid = make_id([self.stem.as_str(), name.as_str()]);
                    self.node(&nid, &name, line);
                    id = Some(nid);
                    key = simple_generic_impl_key(node, self.source);
                    typ = Some(trim(name.split('<').next().unwrap_or("")).into());
                }
                if let (Some(trait_node), Some(id)) =
                    (node.child_by_field_name("trait"), id.as_ref())
                {
                    for (idx, (name, _)) in self.types(Some(trait_node)).into_iter().enumerate() {
                        let target = self.named(&name);
                        if target != *id {
                            if idx == 0 {
                                self.edge(id, &target, "implements", line, None);
                            } else {
                                self.edge(id, &target, "references", line, Some("generic_arg"));
                            }
                        }
                    }
                }
                if let Some(body) = node.child_by_field_name("body") {
                    if let Some(id) = id.as_ref().filter(|_| {
                        children(body).any(|n| {
                            matches!(n.kind(), "function_item" | "function_signature_item")
                        })
                    }) {
                        let existing = self
                            .impl_keys
                            .entry(id.clone())
                            .or_insert_with(|| key.clone());
                        if *existing != key {
                            *existing = None;
                        }
                        let stored = existing.clone();
                        if let Some(impl_node) = self.nodes.iter_mut().find(|n| n["id"] == *id) {
                            if let Some(key) = stored.filter(|s| !s.is_empty()) {
                                impl_node["_rust_impl_key"] = json!(key);
                            } else {
                                impl_node
                                    .as_object_mut()
                                    .unwrap()
                                    .shift_remove("_rust_impl_key");
                            }
                        }
                    }
                    for n in children(body) {
                        self.walk(n, id.as_deref(), typ.as_deref(), key.as_deref());
                    }
                }
                return;
            }
            "use_declaration" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    let name = use_module(&read_text(argument, self.source));
                    let grouped = matches!(argument.kind(), "use_list" | "scoped_use_list");
                    if !name.is_empty() {
                        self.edge(
                            &self.file.clone(),
                            &make_id([name.as_str()]),
                            "imports_from",
                            line,
                            Some("import"),
                        );
                        if grouped {
                            // Call evidence as before; resolution drops it later.
                            self.edges.last_mut().unwrap()["_rust_use_group"] = json!(true);
                        }
                    }
                    // A group imports what the plain `use` of each binding would
                    // (#4135); these edges are not call evidence.
                    if grouped {
                        for (binding, line) in use_bindings(argument, self.source, "", "") {
                            let name = use_module(&binding);
                            if !name.is_empty() {
                                self.edge(
                                    &self.file.clone(),
                                    &make_id([name.as_str()]),
                                    "imports_from",
                                    line,
                                    Some("import"),
                                );
                                self.edges.last_mut().unwrap()["_rust_use_binding"] = json!(true);
                            }
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        for n in children(node) {
            self.walk(n, None, None, None);
        }
    }
    fn calls(
        &mut self,
        node: Node<'_>,
        caller: &str,
        self_type: Option<&str>,
        self_key: Option<&str>,
    ) {
        if node.kind() == "function_item" {
            return;
        }
        // `name!(...)` invoking a local `macro_rules!` macro. A scoped
        // `log::info!` is cross-module and stays unresolved; the token-tree
        // arguments are still walked below.
        if node.kind() == "macro_invocation"
            && let Some(m) = node
                .child_by_field_name("macro")
                .filter(|m| m.kind() == "identifier")
            && let Some(target) = self
                .macros
                .get(&read_text(m, self.source))
                .filter(|t| *t != caller)
                .cloned()
            && self.pairs.insert((caller.into(), target.clone()))
        {
            self.edge(
                caller,
                &target,
                "calls",
                node.start_position().row + 1,
                Some("call"),
            );
        }
        if node.kind() == "call_expression" {
            let mut name = None;
            let mut member = false;
            let mut scoped = false;
            let mut is_self = false;
            let mut qualifier: Option<String> = None;
            if let Some(function) = node.child_by_field_name("function") {
                match function.kind() {
                    "identifier" => name = Some(read_text(function, self.source)),
                    "field_expression" => {
                        member = true;
                        name = function
                            .child_by_field_name("field")
                            .map(|n| read_text(n, self.source));
                        is_self = function
                            .child_by_field_name("value")
                            .is_some_and(|n| n.kind() == "self");
                    }
                    "scoped_identifier" => {
                        scoped = true;
                        name = function
                            .child_by_field_name("name")
                            .map(|n| read_text(n, self.source));
                        qualifier =
                            qualifier_type(function.child_by_field_name("path"), self.source);
                        if qualifier.as_deref() == Some("Self") {
                            // A trait's default method has no impl type: `""`
                            // matches no method.
                            qualifier = Some(self_type.unwrap_or("").to_string());
                        }
                    }
                    _ => {}
                }
            }
            if let Some(name) = name.filter(|s| {
                !s.is_empty()
                    && !is_language_builtin(s)
                    && (!RUST_BUILTIN_TYPES.contains(&s.as_str()) || self.local_types.contains(s))
            }) {
                let self_type = self_type.filter(|s| !s.is_empty());
                let target = if qualifier.is_some() || (is_self && self_type.is_some()) {
                    // `Type::f()`, `Self::f()` and `self.f()` name the type whose
                    // method is called: only that type's in-file method binds,
                    // not whichever `fn new` the file defines (#4238).
                    let owner = type_last_segment(
                        qualifier
                            .as_deref()
                            .filter(|q| !q.is_empty())
                            .or(self_type)
                            .unwrap_or(""),
                    );
                    let owned = self.impl_methods.get(&(owner, name.clone()));
                    let mut target = owned
                        .filter(|owned| owned.len() == 1)
                        .and_then(|owned| owned.first().cloned());
                    if target.is_none() && qualifier.is_some() {
                        // `Enum::Variant(x)` constructs a value: the variant node.
                        target = self
                            .labels
                            .get(&name)
                            .filter(|id| self.type_ids.contains(*id))
                            .cloned();
                    }
                    target
                } else if member {
                    self.method_labels.get(&name).cloned()
                } else {
                    self.labels.get(&name).cloned()
                };
                if let Some(target) = target.filter(|s| !s.is_empty() && *s != caller) {
                    if self.pairs.insert((caller.into(), target.clone())) {
                        // `Foo(x)` onto a struct/enum constructs a value.
                        let constructor = self.type_ids.contains(&target);
                        self.edge(
                            caller,
                            &target,
                            if constructor { "references" } else { "calls" },
                            node.start_position().row + 1,
                            Some(if constructor { "constructor" } else { "call" }),
                        );
                    }
                } else if !scoped && !TRAIT_METHOD_BLOCKLIST.contains(&name.to_lowercase().as_str())
                {
                    let mut call = json!({"caller_nid":caller,"callee":name,"is_member_call":member,"source_file":self.path,"source_location":format!("L{}",node.start_position().row+1)});
                    if let Some(typ) = self_type.filter(|s| is_self && !s.is_empty()) {
                        call["rust_self_type"] = json!(typ);
                        if let Some(key) = self_key.filter(|s| !s.is_empty()) {
                            call["rust_self_impl_key"] = json!(key);
                        }
                    }
                    self.raw_calls.push(call);
                }
            }
        }
        for n in children(node) {
            self.calls(n, caller, self_type, self_key);
        }
    }
}
fn scan_local_types(node: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    if matches!(
        node.kind(),
        "struct_item" | "enum_item" | "trait_item" | "union_item" | "type_item"
    ) && let Some(name) = node.child_by_field_name("name")
    {
        let name = read_text(name, source);
        if !name.is_empty() {
            out.insert(name);
        }
    }
    for child in children(node) {
        scan_local_types(child, source, out);
    }
}
/// Pre-scan the file-level items behind each id so a type used above its
/// declaration resolves like a use below it, but only when no other item
/// (a casefolded `fn handle`, a `const HANDLE`, an `impl` for another type)
/// shares that id (#3782).
fn scan_type_items(
    node: Node<'_>,
    source: &[u8],
    stem: &str,
    items: &mut HashMap<String, HashSet<(bool, String)>>,
    declared: &mut HashSet<String>,
) {
    let kind = node.kind();
    if matches!(
        kind,
        "struct_item"
            | "enum_item"
            | "trait_item"
            | "function_item"
            | "function_signature_item"
            | "static_item"
            | "const_item"
    ) {
        if let Some(name) = node.child_by_field_name("name") {
            let name = read_text(name, source);
            if matches!(kind, "static_item" | "const_item") && name == "_" {
                return;
            }
            let id = make_id([stem, name.as_str()]);
            let is_type = matches!(kind, "struct_item" | "enum_item" | "trait_item");
            items.entry(id.clone()).or_default().insert((is_type, name));
            if is_type {
                declared.insert(id);
            }
        }
        return;
    }
    if kind == "impl_item" {
        if let Some(ty) = node.child_by_field_name("type") {
            let type_name = trim(&read_text(ty, source)).to_string();
            let mut refs = vec![];
            collect_type_refs(Some(ty), source, false, &mut refs, &HashSet::new());
            let bare = refs
                .into_iter()
                .find(|(_, role)| role == "type")
                .map(|(name, _)| name)
                .unwrap_or_else(|| type_name.clone());
            items
                .entry(make_id([stem, type_name.as_str()]))
                .or_default()
                .insert((true, bare));
        }
        return;
    }
    if kind == "use_declaration" {
        return;
    }
    for child in children(node) {
        scan_type_items(child, source, stem, items, declared);
    }
}
pub fn extract_rust(path: &Path) -> Value {
    let source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s,
        Err(e) => return json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)}),
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&tree_sitter_rust::LANGUAGE.into()) {
        return json!({"nodes":[],"edges":[],"error":e.to_string()});
    }
    let Some(tree) = parser.parse(&source, None) else {
        return json!({"nodes":[],"edges":[],"error":"Rust parse cancelled"});
    };
    let text = fsutil::path_string(path);
    let file = make_id([text.as_str()]);
    let parts = path_syntax::posix_parts(&text);
    let name = path_syntax::posix_name(&parts).to_string();
    let mut ex = Extractor {
        source: &source,
        path: text,
        stem: file_stem(path),
        file: file.clone(),
        nodes: vec![],
        edges: vec![],
        seen: HashSet::new(),
        bodies: vec![],
        impl_keys: HashMap::new(),
        labels: HashMap::new(),
        method_labels: HashMap::new(),
        impl_methods: HashMap::new(),
        pairs: HashSet::new(),
        raw_calls: vec![],
        local_types: HashSet::new(),
        forward_types: HashSet::new(),
        type_ids: HashSet::new(),
        macros: HashMap::new(),
    };
    scan_local_types(tree.root_node(), &source, &mut ex.local_types);
    ex.node(&file, &name, 1);
    let mut items: HashMap<String, HashSet<(bool, String)>> = HashMap::new();
    let mut declared = HashSet::new();
    scan_type_items(
        tree.root_node(),
        &source,
        &ex.stem,
        &mut items,
        &mut declared,
    );
    ex.forward_types = items
        .into_iter()
        .filter(|(id, items)| declared.contains(id) && items.len() == 1)
        .filter_map(|(_, items)| items.into_iter().next().map(|(_, name)| name))
        .collect();
    ex.walk(tree.root_node(), None, None, None);
    // A bare `f()` reaches a free function or a type (a constructor), `x.f()`
    // only a method: one shared index bound `drop(x)` to `impl Drop`'s `drop`
    // and `x.len()` to a free `fn len` (#4238).
    for n in &ex.nodes {
        let (Some(label), Some(id)) = (n["label"].as_str(), n["id"].as_str()) else {
            continue;
        };
        let normalised = label.trim_matches(['(', ')']).trim_start_matches('.');
        if label.starts_with('.') {
            if label.ends_with(')') {
                ex.method_labels.insert(normalised.into(), id.into());
            }
        } else {
            ex.labels.insert(normalised.into(), id.into());
        }
    }
    ex.type_ids = ex
        .nodes
        .iter()
        .filter(|n| !n["label"].as_str().unwrap_or("").ends_with(')'))
        .filter_map(|n| n["id"].as_str().map(str::to_string))
        .collect();
    for body in std::mem::take(&mut ex.bodies) {
        ex.calls(
            body.node,
            &body.id,
            body.typ.as_deref(),
            body.key.as_deref(),
        );
    }
    ex.edges.retain(|e| {
        e["source"].as_str().is_some_and(|s| ex.seen.contains(s))
            && (e["target"].as_str().is_some_and(|s| ex.seen.contains(s))
                || matches!(e["relation"].as_str(), Some("imports" | "imports_from")))
    });
    json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls})
}
#[cfg(test)]
mod tests {
    #[test]
    fn grammar_matches_reference() {
        let l: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 355);
        assert_eq!(l.parse_state_count(), 3825);
        assert_eq!(l.abi_version(), 15);
    }

    #[test]
    fn self_return_type_does_not_mint_a_sourceless_self_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.rs");
        std::fs::write(
            &path,
            "struct P;\nimpl P { fn new() -> Self { P } fn scaled(&self) -> Self { P } }\n",
        )
        .unwrap();
        let result = super::extract_rust(&path);
        let nodes = result["nodes"].as_array().unwrap();
        assert!(
            nodes
                .iter()
                .all(|n| n["label"] != "Self" && n["id"] != "self"),
            "{nodes:?}"
        );
        let labels: Vec<&str> = nodes.iter().map(|n| n["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["p.rs", "P", ".new()", ".scaled()"]);
        let references = result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "references")
            .count();
        assert_eq!(references, 0, "{:?}", result["edges"]);
    }
}
