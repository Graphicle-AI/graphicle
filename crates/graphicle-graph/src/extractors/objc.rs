// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/objc.py
//! Objective-C categories, protocols, selectors and receiver-type evidence.
use super::{
    base::{file_stem, make_id, read_text, ts_index},
    engine_declarator::cpp_declarator_name,
    engine_semantics::semantic_reference_edge,
    resolution_paths::resolve_c_include_path,
};
use crate::{Result, fsutil, io_compat, path_syntax};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};
use tree_sitter::Node;
fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.child_count()).filter_map(move |i| node.child(i))
}
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
}
fn read(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.byte_range()]).into_owned()
}
/// Declaration markers, as the generic engine puts on every other language's
/// definitions (#2438, #3228): `_callable` is a real callable, not a
/// same-named data symbol; `_callable_class` narrows that to a type.
const CALLABLE: &[&str] = &["_callable"];
const CALLABLE_CLASS: &[&str] = &["_callable", "_callable_class"];
/// Pre-order nodes of `kind` below and at `node`.
fn descendants<'a>(node: Node<'a>, kind: &str, out: &mut Vec<Node<'a>>) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == kind {
            out.push(n);
        }
        let mut kids = children(n).collect::<Vec<_>>();
        kids.reverse();
        stack.extend(kids);
    }
}
/// Pre-order leaf tokens below `node`, comments (and their subtrees) left
/// out.
fn leaves(node: Node<'_>) -> Vec<Node<'_>> {
    let mut out = vec![];
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "comment" {
            continue;
        }
        if n.child_count() == 0 {
            out.push(n);
        } else {
            let mut kids = children(n).collect::<Vec<_>>();
            kids.reverse();
            stack.extend(kids);
        }
    }
    out
}
/// An `NS_ENUM`/`NS_OPTIONS` enum: `(name, line, [(member, line)])`.
type MacroEnum = (String, usize, Vec<(String, usize)>);
/// `_macro_enum_members` (0.9.89, #4400, #4401): `typedef NS_ENUM(Type,
/// Name) { ... };` and `NS_OPTIONS`. The grammar does not parse these bodies
/// as enums, so a declaration is selected by its macro header's tokens (one
/// `(`, one `,`, one `)`, one underlying type, one name, after `typedef`) and
/// only its unchanged brace-delimited body is parsed alone as `enum Name
/// {...};`. Every member must map back to the same source bytes; a malformed
/// or conditional body yields no enum at all. Returns `(name, line,
/// members)`.
fn macro_enum_members(
    root: Node<'_>,
    source: &[u8],
    parser: &mut tree_sitter::Parser,
) -> Vec<MacroEnum> {
    let mut accepted = vec![];
    let mut macros = vec![];
    descendants(root, "macro_type_specifier", &mut macros);
    if macros.is_empty() {
        return accepted;
    }
    let source_leaves = leaves(root);
    'macros: for mac in macros {
        let Some(macro_name) = mac.child_by_field_name("name") else {
            continue;
        };
        if !matches!(read(macro_name, source).as_str(), "NS_ENUM" | "NS_OPTIONS") {
            continue;
        }
        // The closest preceding structural token in an enclosing node must be
        // `typedef`: no bare macro use, no text in a definition.
        let mut ancestor = mac.parent();
        while let Some(a) = ancestor {
            let preceding = leaves(a)
                .into_iter()
                .rfind(|t| t.end_byte() <= mac.start_byte());
            if preceding.is_some_and(|t| t.kind() == "typedef") {
                break;
            }
            ancestor = a.parent();
        }
        if ancestor.is_none() {
            continue;
        }
        let tokens = leaves(mac);
        let of = |kind: &str| {
            tokens
                .iter()
                .filter(|t| t.kind() == kind)
                .copied()
                .collect::<Vec<_>>()
        };
        let (opens, closes, commas) = (of("("), of(")"), of(","));
        if opens.len() != 1 || closes.len() != 1 || commas.len() != 1 {
            continue;
        }
        let (opening, closing, comma) = (opens[0], closes[0], commas[0]);
        if !(opening.end_byte() <= comma.start_byte()
            && comma.start_byte() < comma.end_byte()
            && comma.end_byte() <= closing.start_byte())
        {
            continue;
        }
        let mut descriptors = vec![];
        descendants(mac, "type_descriptor", &mut descriptors);
        let underlying = descriptors
            .into_iter()
            .filter(|n| opening.end_byte() <= n.start_byte() && n.end_byte() <= comma.start_byte())
            .collect::<Vec<_>>();
        let outer = underlying
            .iter()
            .filter(|n| {
                !underlying.iter().any(|o| {
                    o.id() != n.id()
                        && o.start_byte() <= n.start_byte()
                        && n.end_byte() <= o.end_byte()
                })
            })
            .copied()
            .collect::<Vec<_>>();
        let names = tokens
            .iter()
            .filter(|t| {
                comma.end_byte() <= t.start_byte()
                    && t.start_byte() < closing.start_byte()
                    && t.is_named()
            })
            .copied()
            .collect::<Vec<_>>();
        if outer.len() != 1
            || names.len() != 1
            || !matches!(names[0].kind(), "identifier" | "type_identifier")
        {
            continue;
        }
        // Valid headers also carry grammar ERROR wrappers: check the whole
        // argument spans rather than ignoring extra tokens in them.
        let (ty, name) = (outer[0], names[0]);
        if tokens.iter().any(|t| t.is_missing())
            || tokens.iter().any(|t| {
                (opening.end_byte() <= t.start_byte()
                    && t.start_byte() < comma.start_byte()
                    && !(ty.start_byte() <= t.start_byte() && t.end_byte() <= ty.end_byte()))
                    || (comma.end_byte() <= t.start_byte()
                        && t.start_byte() < closing.start_byte()
                        && !(name.start_byte() <= t.start_byte()
                            && t.end_byte() <= name.end_byte()))
            })
        {
            continue;
        }
        let following = source_leaves
            .iter()
            .filter(|t| t.start_byte() >= mac.end_byte())
            .copied()
            .collect::<Vec<_>>();
        let Some(&open_brace) = following.first().filter(|t| t.kind() == "{") else {
            continue;
        };
        let mut depth: i64 = 0;
        let mut close_brace = None;
        for t in &following {
            if t.kind() == "{" {
                depth += 1;
            } else if t.kind() == "}" {
                depth -= 1;
                if depth == 0 {
                    close_brace = Some(*t);
                    break;
                }
            }
            if depth < 0 {
                break;
            }
        }
        let Some(close_brace) = close_brace else {
            continue;
        };
        if source_leaves
            .iter()
            .find(|t| t.start_byte() >= close_brace.end_byte())
            .is_none_or(|t| t.kind() != ";")
        {
            continue;
        }
        let name_bytes = &source[name.byte_range()];
        let body_bytes = &source[open_brace.start_byte()..close_brace.end_byte()];
        let mut prefix = b"enum ".to_vec();
        prefix.extend_from_slice(name_bytes);
        prefix.push(b' ');
        let mut fragment = prefix.clone();
        fragment.extend_from_slice(body_bytes);
        fragment.push(b';');
        let Some(tree) = parser.parse(&fragment, None) else {
            continue;
        };
        let fragment_root = tree.root_node();
        let mut specs = vec![];
        descendants(fragment_root, "enum_specifier", &mut specs);
        if fragment_root.has_error() || specs.len() != 1 {
            continue;
        }
        let spec = specs[0];
        let (Some(enum_name), Some(body)) = (
            spec.child_by_field_name("name"),
            spec.child_by_field_name("body"),
        ) else {
            continue;
        };
        if &fragment[enum_name.byte_range()] != name_bytes
            || spec.has_error()
            || body.kind() != "enumerator_list"
            || body.has_error()
            || body.start_byte() != prefix.len()
            || body.end_byte() != prefix.len() + body_bytes.len()
        {
            continue;
        }
        // Conditional members need preprocessor handling: no partial enum.
        if children(body).any(|c| {
            !matches!(
                c.kind(),
                "{" | "}" | "," | "comment" | "enumerator" | "attribute_specifier"
            )
        }) {
            continue;
        }
        let mut members = vec![];
        let mut previous_end: Option<usize> = None;
        for member in children(body).filter(|c| c.kind() == "enumerator") {
            // `Bad Other` parses as two clean enumerators: every later member
            // needs its separator.
            if let Some(end) = previous_end
                && !children(body).any(|t| {
                    t.kind() == "," && end <= t.start_byte() && t.start_byte() < member.start_byte()
                })
            {
                continue 'macros;
            }
            let Some(ident) = member
                .child_by_field_name("name")
                .or_else(|| children(member).find(|c| c.kind() == "identifier"))
            else {
                continue 'macros;
            };
            if member.has_error() || ident.start_byte() < body.start_byte() {
                continue 'macros;
            }
            let start = open_brace.start_byte() + ident.start_byte() - body.start_byte();
            let end = open_brace.start_byte() + ident.end_byte() - body.start_byte();
            let text = &fragment[ident.byte_range()];
            if end > close_brace.end_byte() || source.get(start..end) != Some(text) {
                continue 'macros;
            }
            let line = open_brace.start_position().row + ident.start_position().row
                - body.start_position().row
                + 1;
            members.push((String::from_utf8_lossy(text).into_owned(), line));
            previous_end = Some(member.end_byte());
        }
        accepted.push((read(name, source), mac.start_position().row + 1, members));
    }
    accepted
}
fn identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
pub fn category_base_stem(stem: &str) -> String {
    let (head, tail) = stem
        .rsplit_once('/')
        .map(|(a, b)| (Some(a), b))
        .unwrap_or((None, stem));
    let Some((base, suffix)) = tail.split_once('+') else {
        return stem.into();
    };
    if !identifier(base) || !identifier(suffix) {
        return stem.into();
    }
    head.map(|h| format!("{h}/{base}"))
        .unwrap_or_else(|| base.into())
}
pub fn is_category(node: Node<'_>) -> bool {
    children(node).any(|n| n.kind() == "(")
}
pub fn local_var_types(body: Node<'_>, source: &[u8], table: &mut Map<String, Value>) {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == "method_definition" && node != body {
            continue;
        }
        if node.kind() == "declaration"
            && let Some(typ) = node
                .child_by_field_name("type")
                .or_else(|| children(node).find(|n| n.kind() == "type_identifier"))
                .filter(|n| n.kind() == "type_identifier")
        {
            let typ = trim(&read_text(typ, source)).to_string();
            let declarations: Vec<_> = children(node)
                .filter(|n| {
                    matches!(
                        n.kind(),
                        "identifier" | "pointer_declarator" | "init_declarator"
                    )
                })
                .collect();
            if typ.chars().next().is_some_and(char::is_uppercase)
                && declarations.len() == 1
                && let Some(var) =
                    cpp_declarator_name(declarations[0], source).filter(|s| !s.is_empty())
            {
                table.entry(var).or_insert(json!(typ));
            }
        }
        stack.extend(children(node));
    }
}
fn type_identifiers(node: Node<'_>) -> Vec<Node<'_>> {
    if node.kind() == "type_identifier" {
        vec![node]
    } else {
        children(node).flat_map(type_identifiers).collect()
    }
}
#[derive(Clone)]
struct Body<'tree> {
    id: String,
    node: Node<'tree>,
    container: String,
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
    raw_calls: Vec<Value>,
    types: Map<String, Value>,
    fields: Map<String, Value>,
    all_methods: BTreeSet<String>,
    siblings: HashMap<String, HashSet<String>>,
    calls: HashSet<(String, String)>,
}
impl<'tree> Extractor<'_, 'tree> {
    fn node(&mut self, id: &str, label: &str, line: usize) {
        self.marked(id, label, line, &[]);
    }
    fn marked(&mut self, id: &str, label: &str, line: usize, markers: &[&str]) {
        if self.seen.insert(id.into()) {
            let mut node = json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")});
            for marker in markers {
                node[*marker] = json!(true);
            }
            self.nodes.push(node);
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
        let mut edge = json!({"source":source,"target":target,"relation":relation,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(c) = context.filter(|s| !s.is_empty()) {
            edge["context"] = json!(c);
        }
        self.edges.push(edge);
    }
    fn named(&mut self, name: &str) -> String {
        let local = make_id([self.stem.as_str(), name]);
        if self.seen.contains(&local) {
            return local;
        }
        let id = make_id([name]);
        if self.seen.insert(id.clone()) {
            self.nodes.push(json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":"","origin_file":self.path}));
        }
        id
    }
    fn field_entry(&self, declaration: Node<'_>) -> Option<(String, String)> {
        let types: Vec<_> = children(declaration)
            .filter(|n| n.kind() == "type_identifier")
            .collect();
        let decls: Vec<_> = children(declaration)
            .filter(|n| n.kind() == "struct_declarator")
            .collect();
        if types.len() != 1 || decls.len() != 1 {
            return None;
        }
        let typ = trim(&read(types[0], self.source)).to_string();
        if !typ.chars().next().is_some_and(char::is_uppercase) {
            return None;
        }
        if decls[0].child_count() != 1 {
            return None;
        }
        let field =
            cpp_declarator_name(decls[0].child(0)?, self.source).filter(|s| !s.is_empty())?;
        Some((field, typ))
    }
    fn field(&mut self, class: &str, name: String, typ: String) {
        let table = self
            .fields
            .entry(class)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .unwrap();
        if let Some(old) = table.get_mut(&name) {
            if *old != typ {
                *old = Value::Null;
            }
        } else {
            table.insert(name, json!(typ));
        }
    }
    fn ivars(&mut self, node: Node<'_>, class: &str) {
        for decl in children(node)
            .filter(|n| n.kind() == "instance_variable")
            .flat_map(children)
            .filter(|n| n.kind() == "struct_declaration")
        {
            if let Some((name, typ)) = self.field_entry(decl) {
                self.field(class, name, typ);
            }
        }
    }
    fn reference(&mut self, source: &str, target: &str, context: &str, line: usize) -> Result<()> {
        self.edges.push(semantic_reference_edge(
            source,
            target,
            context,
            &self.path,
            &json!(line),
        )?);
        Ok(())
    }
    fn walk(&mut self, node: Node<'tree>, parent: Option<&str>) -> Result<()> {
        let line = node.start_position().row + 1;
        match node.kind() {
            "preproc_include" => {
                for n in children(node) {
                    if n.kind() == "system_lib_string" {
                        let raw = read(n, self.source);
                        let module = raw
                            .trim_matches(['<', '>'])
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .replace(".h", "");
                        if !module.is_empty() {
                            self.edge(
                                &self.file.clone(),
                                &make_id([module.as_str()]),
                                "imports",
                                line,
                                Some("import"),
                            );
                        }
                    } else if n.kind() == "string_literal" {
                        for sub in children(n).filter(|n| n.kind() == "string_content") {
                            let raw = read(sub, self.source);
                            let resolved = resolve_c_include_path(&raw, &self.path)
                                .map_err(crate::Error::from)?;
                            if let Some(path) = resolved {
                                self.edge(
                                    &self.file.clone(),
                                    &make_id([fsutil::path_string(&path).as_str()]),
                                    "imports",
                                    line,
                                    Some("import"),
                                );
                            } else {
                                let module = raw.rsplit('/').next().unwrap_or("").replace(".h", "");
                                if !module.is_empty() {
                                    self.edge(
                                        &self.file.clone(),
                                        &make_id([module.as_str()]),
                                        "imports",
                                        line,
                                        Some("import"),
                                    );
                                    // The quoted spelling, for the corpus pass that
                                    // resolves it to a unique header (#4354).
                                    let metadata = crate::security::sanitize_metadata(Some(
                                        &json!({"raw_include": raw, "include_kind": "quoted"}),
                                    ));
                                    if !metadata.is_empty() {
                                        self.edges.last_mut().unwrap()["metadata"] =
                                            Value::Object(metadata);
                                    }
                                }
                            }
                        }
                    }
                }
                return Ok(());
            }
            "module_import" => {
                if let Some(path) = node.child_by_field_name("path") {
                    let text = read(path, self.source);
                    let module = trim(text.split('.').next().unwrap_or(""));
                    if !module.is_empty() {
                        self.edge(
                            &self.file.clone(),
                            &make_id([module]),
                            "imports",
                            line,
                            Some("import"),
                        );
                    }
                }
                return Ok(());
            }
            "class_interface" => {
                let Some(name) = children(node).find(|n| n.kind() == "identifier") else {
                    for n in children(node) {
                        self.walk(n, parent)?;
                    }
                    return Ok(());
                };
                let name = read(name, self.source);
                let stem = if is_category(node) {
                    category_base_stem(&self.stem)
                } else {
                    self.stem.clone()
                };
                let id = make_id([stem.as_str(), name.as_str()]);
                self.marked(&id, &name, line, CALLABLE_CLASS);
                self.edge(&self.file.clone(), &id, "contains", line, None);
                let mut colon = false;
                for n in children(node) {
                    if n.kind() == ":" {
                        colon = true;
                    } else if colon && n.kind() == "identifier" {
                        let target = self.named(&read(n, self.source));
                        self.edge(&id, &target, "inherits", line, None);
                        colon = false;
                    } else if n.kind() == "parameterized_arguments" {
                        for typ in children(n)
                            .filter(|n| n.kind() == "type_name")
                            .flat_map(children)
                            .filter(|n| n.kind() == "type_identifier")
                        {
                            let target = self.named(&read(typ, self.source));
                            self.edge(&id, &target, "implements", line, None);
                        }
                    } else if n.kind() == "property_declaration" {
                        let line = n.start_position().row + 1;
                        for decl in children(n).filter(|n| n.kind() == "struct_declaration") {
                            let mut seen_types = HashSet::new();
                            for typ in children(decl)
                                .filter(|n| !matches!(n.kind(), "struct_declarator" | ";"))
                                .flat_map(type_identifiers)
                            {
                                let typ = read(typ, self.source);
                                if seen_types.insert(typ.clone()) {
                                    let target = self.named(&typ);
                                    self.reference(&id, &target, "field", line)?;
                                }
                            }
                            if let Some((name, typ)) = self.field_entry(decl) {
                                self.field(&id, name, typ);
                            }
                        }
                    } else if n.kind() == "instance_variables" {
                        self.ivars(n, &id);
                    } else if n.kind() == "method_declaration" {
                        self.walk(n, Some(&id))?;
                    }
                }
                return Ok(());
            }
            "class_implementation" => {
                let Some(name) = children(node)
                    .find(|n| n.kind() == "identifier")
                    .map(|n| read(n, self.source))
                    .filter(|s| !s.is_empty())
                else {
                    for n in children(node) {
                        self.walk(n, parent)?;
                    }
                    return Ok(());
                };
                let stem = if is_category(node) {
                    category_base_stem(&self.stem)
                } else {
                    self.stem.clone()
                };
                let id = make_id([stem.as_str(), name.as_str()]);
                if !self.seen.contains(&id) {
                    self.marked(&id, &name, line, CALLABLE_CLASS);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                }
                for n in children(node) {
                    if n.kind() == "instance_variables" {
                        self.ivars(n, &id);
                    } else if n.kind() == "implementation_definition" {
                        for sub in children(n) {
                            self.walk(sub, Some(&id))?;
                        }
                    }
                }
                return Ok(());
            }
            "protocol_declaration" => {
                if let Some(name) = children(node)
                    .find(|n| n.kind() == "identifier")
                    .map(|n| read(n, self.source))
                    .filter(|s| !s.is_empty())
                {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    // A protocol is a type declaration like any interface.
                    self.marked(&id, &format!("<{name}>"), line, CALLABLE_CLASS);
                    self.edge(&self.file.clone(), &id, "contains", line, None);
                    for base in children(node)
                        .filter(|n| n.kind() == "protocol_reference_list")
                        .flat_map(children)
                        .filter(|n| n.kind() == "identifier")
                    {
                        let target = self.named(&read(base, self.source));
                        if target != id {
                            self.edge(&id, &target, "implements", line, None);
                        }
                    }
                    for n in children(node) {
                        self.walk(n, Some(&id))?;
                    }
                }
                return Ok(());
            }
            "method_declaration" | "method_definition" => {
                let container = parent
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&self.file)
                    .to_string();
                let prefix = children(node)
                    .find(|n| matches!(n.kind(), "+" | "-"))
                    .map(|n| n.kind())
                    .unwrap_or("-");
                let name = children(node)
                    .filter(|n| n.kind() == "identifier")
                    .map(|n| read(n, self.source))
                    .collect::<String>();
                if !name.is_empty() {
                    let id = make_id([container.as_str(), name.as_str()]);
                    self.marked(&id, &format!("{prefix}{name}"), line, CALLABLE);
                    self.edge(&container, &id, "method", line, None);
                    if node.kind() == "method_definition" {
                        self.bodies.push(Body {
                            id,
                            node,
                            container,
                        });
                    }
                }
                return Ok(());
            }
            "type_definition" => {
                // `typedef enum [Tag] { ... } Name;` is referenced through its
                // alias (the trailing type_identifier), else the tag. NS_ENUM /
                // NS_OPTIONS go through `macro_enum_members`.
                if let Some(spec) = children(node).find(|c| c.kind() == "enum_specifier")
                    && children(spec).any(|c| c.kind() == "enumerator_list")
                    && let Some(alias) = children(node)
                        .filter(|c| c.kind() == "type_identifier")
                        .last()
                        .or_else(|| children(spec).find(|c| c.kind() == "type_identifier"))
                        .map(|c| read(c, self.source))
                        .filter(|s| !s.is_empty())
                {
                    self.emit_enum(spec, &alias, line);
                    return Ok(());
                }
            }
            "enum_specifier" => {
                // A named `enum Color { ... };` definition; forward declarations
                // and anonymous enums are not nameable definitions.
                if let Some(name) = children(node)
                    .find(|c| c.kind() == "type_identifier")
                    .map(|c| read(c, self.source))
                    .filter(|s| !s.is_empty())
                    && children(node).any(|c| c.kind() == "enumerator_list")
                {
                    self.emit_enum(node, &name, line);
                }
                return Ok(());
            }
            _ => {}
        }
        for n in children(node) {
            self.walk(n, parent)?;
        }
        Ok(())
    }
    /// A C-style enum: a type node plus one `case_of` member per enumerator
    /// (0.9.77, mirroring the other languages' enum model).
    fn emit_enum(&mut self, spec: Node<'_>, name: &str, line: usize) {
        let Some(body) = children(spec).find(|c| c.kind() == "enumerator_list") else {
            self.emit_enum_members(name, line, &[]);
            return;
        };
        let members = children(body)
            .filter(|c| c.kind() == "enumerator")
            .filter_map(|member| {
                children(member)
                    .find(|c| c.kind() == "identifier")
                    .map(|c| read(c, self.source))
                    .filter(|s| !s.is_empty())
                    .map(|name| (name, member.start_position().row + 1))
            })
            .collect::<Vec<_>>();
        self.emit_enum_members(name, line, &members);
    }
    /// The enum type node, then its members as `(name, line)`.
    fn emit_enum_members(&mut self, name: &str, line: usize, members: &[(String, usize)]) {
        let id = make_id([self.stem.as_str(), name]);
        self.node(&id, name, line);
        let file = self.file.clone();
        self.edge(&file, &id, "contains", line, None);
        for (member_name, member_line) in members {
            let member_id = make_id([id.as_str(), member_name.as_str()]);
            self.node(&member_id, member_name, *member_line);
            self.edge(&id, &member_id, "case_of", *member_line, None);
        }
    }
    fn call_edge(&mut self, caller: &str, target: &str, line: usize, access: bool) {
        if caller != target && self.calls.insert((caller.into(), target.into())) {
            self.edge(
                caller,
                target,
                if access { "accesses" } else { "calls" },
                line,
                if access { None } else { Some("call") },
            );
        }
    }
    fn raw_call(
        &mut self,
        caller: &str,
        callee: &str,
        receiver: &str,
        line: usize,
        self_field: bool,
    ) {
        let mut call = json!({"caller_nid":caller,"callee":callee,"is_member_call":true,"source_file":self.path,"source_location":format!("L{line}"),"receiver":receiver,"lang":"objc"});
        if self_field {
            call["receiver_kind"] = json!("self_field");
        }
        self.raw_calls.push(call);
    }
    fn walk_calls(&mut self, node: Node<'_>, caller: &str, container: &str) -> Result<()> {
        let line = node.start_position().row + 1;
        match node.kind() {
            "message_expression" => {
                let method = node.child_by_field_name("method");
                let receiver = node.child_by_field_name("receiver");
                if method
                    .is_some_and(|n| n.kind() == "identifier" && read(n, self.source) == "alloc")
                    && let Some(receiver) = receiver.filter(|n| n.kind() == "identifier")
                {
                    let target = self.named(&read(receiver, self.source));
                    if target != caller {
                        self.reference(caller, &target, "type", line)?;
                    }
                }
                let name = (0..node.child_count())
                    .filter_map(|i| {
                        node.child(i).filter(|n| {
                            ts_index(i).and_then(|i| node.field_name_for_child(i)) == Some("method")
                                && n.kind() == "identifier"
                        })
                    })
                    .map(|n| read(n, self.source))
                    .collect::<String>();
                if !name.is_empty() {
                    let needle = make_id(["", name.as_str()])
                        .trim_start_matches('_')
                        .to_string();
                    let candidates: Vec<_> = self
                        .all_methods
                        .iter()
                        .filter(|n| n.ends_with(&needle))
                        .cloned()
                        .collect();
                    for candidate in candidates {
                        self.call_edge(caller, &candidate, line, false);
                    }
                    if let Some(receiver) = receiver {
                        if receiver.kind() == "identifier" {
                            self.raw_call(caller, &name, &read(receiver, self.source), line, false);
                        } else if receiver.kind() == "field_expression" {
                            let kids: Vec<_> = children(receiver).collect();
                            if kids.len() == 3
                                && kids[0].kind() == "identifier"
                                && read(kids[0], self.source) == "self"
                                && kids[1].kind() == "."
                                && kids[2].kind() == "field_identifier"
                            {
                                self.raw_call(
                                    caller,
                                    &name,
                                    &read(kids[2], self.source),
                                    line,
                                    true,
                                );
                            }
                        }
                    }
                }
            }
            "field_expression" => {
                for field in children(node).filter(|n| n.kind() == "field_identifier") {
                    let target = make_id([container, read(field, self.source).as_str()]);
                    if self
                        .siblings
                        .get(container)
                        .is_some_and(|s| s.contains(&target))
                    {
                        self.call_edge(caller, &target, line, true);
                    }
                }
            }
            "selector_expression" => {
                let name = children(node)
                    .filter(|n| n.kind() == "identifier")
                    .map(|n| read(n, self.source))
                    .collect::<String>();
                if !name.is_empty() {
                    let matches: BTreeSet<_> = self
                        .bodies
                        .iter()
                        .filter(|b| {
                            b.id == make_id([b.container.as_str(), name.as_str()]) && b.id != caller
                        })
                        .map(|b| b.id.clone())
                        .collect();
                    if matches.len() == 1 {
                        self.call_edge(caller, matches.first().unwrap(), line, false);
                    }
                }
            }
            _ => {}
        }
        for n in children(node) {
            self.walk_calls(n, caller, container)?;
        }
        Ok(())
    }
}
pub fn extract_objc(path: &Path) -> Result<Value> {
    let mut source = match super::base::read_source_bytes(path, true) {
        Ok(s) => s.to_vec(),
        Err(e) => return Ok(json!({"nodes":[],"edges":[],"error":io_compat::read_error(&e,path)})),
    };
    for marker in [
        b"NS_ASSUME_NONNULL_BEGIN".as_slice(),
        b"NS_ASSUME_NONNULL_END".as_slice(),
    ] {
        let mut start = 0;
        while let Some(offset) = source[start..]
            .windows(marker.len())
            .position(|s| s == marker)
        {
            let begin = start + offset;
            source[begin..begin + marker.len()].fill(b' ');
            start = begin + marker.len();
        }
    }
    let mut parser = tree_sitter::Parser::new();
    if let Err(e) = parser.set_language(&crate::grammar!(
        "grammar-objc",
        "tree_sitter_objc",
        tree_sitter_objc::LANGUAGE
    )?) {
        return Ok(json!({"nodes":[],"edges":[],"error":e.to_string()}));
    }
    let Some(tree) = parser.parse(&source, None) else {
        return Ok(json!({"nodes":[],"edges":[],"error":"Objective-C parse cancelled"}));
    };
    let text = fsutil::path_string(path);
    let file = make_id([fsutil::path_string(path).as_str()]);
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
        raw_calls: vec![],
        types: Map::new(),
        fields: Map::new(),
        all_methods: BTreeSet::new(),
        siblings: HashMap::new(),
        calls: HashSet::new(),
    };
    ex.node(&file, &name, 1);
    let macro_enums = macro_enum_members(tree.root_node(), &source, &mut parser);
    ex.walk(tree.root_node(), None)?;
    for (name, line, members) in &macro_enums {
        ex.emit_enum_members(name, *line, members);
    }
    ex.all_methods = ex.seen.iter().filter(|n| **n != file).cloned().collect();
    for body in &ex.bodies {
        ex.siblings
            .entry(body.container.clone())
            .or_default()
            .insert(body.id.clone());
        local_var_types(body.node, &source, &mut ex.types);
    }
    for body in ex.bodies.clone() {
        ex.walk_calls(body.node, &body.id, &body.container)?;
    }
    let mut result = json!({"nodes":ex.nodes,"edges":ex.edges,"raw_calls":ex.raw_calls,"input_tokens":0,"output_tokens":0});
    if !ex.types.is_empty() {
        result["objc_type_table"] = json!({"path":ex.path,"table":ex.types});
    }
    for table in ex.fields.values_mut() {
        table
            .as_object_mut()
            .unwrap()
            .retain(|_, v| v.as_str().is_some_and(|s| !s.is_empty()));
    }
    ex.fields.retain(|_, v| !v.as_object().unwrap().is_empty());
    if !ex.fields.is_empty() {
        result["objc_field_types"] = json!({"path":ex.path,"tables":ex.fields});
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "grammar-objc")]
    fn grammar_matches_reference() {
        let l: tree_sitter::Language = tree_sitter_objc::LANGUAGE.into();
        assert_eq!(l.node_kind_count(), 588);
        assert_eq!(l.parse_state_count(), 6799);
        assert_eq!(l.abi_version(), 14);
    }
}
