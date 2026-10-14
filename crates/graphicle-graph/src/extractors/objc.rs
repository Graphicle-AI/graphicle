// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
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
                self.node(&id, &name, line);
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
                    self.node(&id, &name, line);
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
                    self.node(&id, &format!("<{name}>"), line);
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
                    self.node(&id, &format!("{prefix}{name}"), line);
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
                // NS_OPTIONS parse as ERROR subtrees and stay unsupported.
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
        let id = make_id([self.stem.as_str(), name]);
        self.node(&id, name, line);
        let file = self.file.clone();
        self.edge(&file, &id, "contains", line, None);
        let Some(body) = children(spec).find(|c| c.kind() == "enumerator_list") else {
            return;
        };
        for member in children(body).filter(|c| c.kind() == "enumerator") {
            let Some(member_name) = children(member)
                .find(|c| c.kind() == "identifier")
                .map(|c| read(c, self.source))
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let member_line = member.start_position().row + 1;
            let member_id = make_id([id.as_str(), member_name.as_str()]);
            self.node(&member_id, &member_name, member_line);
            self.edge(&id, &member_id, "case_of", member_line, None);
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
    ex.walk(tree.root_node(), None)?;
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
