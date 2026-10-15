// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
#[derive(Clone, Copy)]
pub struct CallableInfo<'a> {
    pub name: &'a str,
    pub owner: &'a str,
    pub line: usize,
    pub method: bool,
    pub nested: bool,
}
use super::{
    base::make_id, engine::Engine, engine_scope::*, engine_semantics::csharp_namespace_id,
    engine_types::*,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;
impl<'a, 't> Engine<'a, 't> {
    pub(crate) fn js_members(&mut self, body: Node<'t>, owner: &str, this_method: bool) {
        let mut objects = BTreeMap::new();
        for stmt in children(body).into_iter().filter(|n| {
            matches!(
                n.kind_name(),
                "lexical_declaration" | "variable_declaration"
            )
        }) {
            for d in children(stmt)
                .into_iter()
                .filter(|n| n.kind_name() == "variable_declarator")
            {
                if let (Some(name), Some(value)) = (
                    d.child_by_field_name("name"),
                    d.child_by_field_name("value"),
                ) && name.kind_name() == "identifier"
                    && value.kind_name() == "object"
                {
                    objects.insert(self.text(name), d);
                }
            }
        }
        let mut contained = BTreeSet::new();
        for stmt in children(body)
            .into_iter()
            .filter(|n| n.kind_name() == "expression_statement")
        {
            let Some(assign) = first_child(stmt, &["assignment_expression"]) else {
                continue;
            };
            let Some(value) = assign
                .child_by_field_name("right")
                .filter(|v| set("_JS_FUNCTION_VALUE_TYPES").contains(v.kind_name()))
            else {
                continue;
            };
            let Some((kind, name, member)) =
                js_member_assignment_target(assign.child_by_field_name("left"), self.source)
            else {
                continue;
            };
            let name = name.unwrap_or_default();
            let (id, method) = if kind == "this" {
                (owner.to_string(), this_method)
            } else if kind == "object" && objects.contains_key(&name) {
                let id = make_id([owner, name.as_str()]);
                let line = objects[&name].start_position().row + 1;
                self.add_node(&id, &name, line, None, Value::Null);
                if contained.insert(id.clone()) {
                    self.edge(owner, &id, "contains", line, None, Value::Null);
                }
                (id, true)
            } else {
                continue;
            };
            let line = stmt.start_position().row + 1;
            let mid = make_id([id.as_str(), member.as_str()]);
            self.add_node(
                &mid,
                &format!("{}{member}()", if method { "." } else { "" }),
                line,
                None,
                Value::Null,
            );
            self.edge(
                &id,
                &mid,
                if method { "method" } else { "contains" },
                line,
                None,
                Value::Null,
            );
            if let Some(b) = value.child_by_field_name("body") {
                self.bodies.push((mid, b));
            }
        }
    }
    fn js_callable(&mut self, id: &str, context: CallableInfo<'_>, value: Node<'t>) {
        let CallableInfo {
            name,
            owner,
            line,
            method,
            nested,
        } = context;

        self.add_node(
            id,
            &format!("{}{name}()", if method { "." } else { "" }),
            line,
            None,
            Value::Null,
        );
        self.edge(
            owner,
            id,
            if method { "method" } else { "contains" },
            line,
            None,
            Value::Null,
        );
        self.callable.insert(id.into());
        self.locals
            .insert(id.into(), js_local_bound_names(value, self.source));
        if let Some(body) = value.child_by_field_name("body") {
            self.bodies.push((id.into(), body));
            if nested {
                self.nested(body, id);
                self.js_members(body, id, true);
            }
        }
    }
    fn js_closures(&mut self, value: Node<'t>, id: &str) -> bool {
        let mut inner = Some(value);
        while let Some(n) =
            inner.filter(|n| matches!(n.kind_name(), "as_expression" | "satisfies_expression"))
        {
            inner = named(n).into_iter().next();
        }
        if let Some(n) =
            inner.filter(|n| matches!(n.kind_name(), "call_expression" | "new_expression"))
        {
            let mut closures = vec![];
            js_topmost_closures(n, &mut closures);
            let found = !closures.is_empty();
            for c in closures {
                if let Some(body) = c.child_by_field_name("body") {
                    self.closure_locals
                        .insert(body.id(), js_local_bound_names(c, self.source));
                    self.bodies.push((id.into(), body));
                }
            }
            return found;
        }
        false
    }
    pub(crate) fn js_extra(&mut self, n: Node<'t>, parent: Option<&str>) -> bool {
        let t = n.kind_name();
        let file = self.file.clone();
        let stem = self.stem.clone();
        let line = n.start_position().row + 1;
        if t == "expression_statement" {
            if let Some(call) = first_child(n, &["call_expression", "new_expression"]) {
                let mut closures = vec![];
                js_topmost_closures(call, &mut closures);
                for c in closures {
                    if let Some(b) = c.child_by_field_name("body") {
                        self.js_members(b, &file, false);
                        // #4448: the walk runs under the file node, so the
                        // closure's own parameters and locals ride along as
                        // this body's locals; otherwise `forEach(function
                        // (value) { s.on(value) })` resolves `value` to any
                        // same-named function in the corpus.
                        self.closure_locals
                            .insert(b.id(), js_local_bound_names(c, self.source));
                        self.bodies.push((file.clone(), b));
                    }
                }
            }
            if let Some(a) = first_child(n, &["assignment_expression"])
                && let Some(v) = a.child_by_field_name("right")
                && let Some((kind, owner, name)) =
                    js_member_assignment_target(a.child_by_field_name("left"), self.source)
            {
                if set("_JS_FUNCTION_VALUE_TYPES").contains(v.kind_name()) {
                    if kind == "exports" {
                        let id = make_id([stem.as_str(), name.as_str()]);
                        self.js_callable(
                            &id,
                            CallableInfo {
                                name: &name,
                                owner: &file,
                                line,
                                method: false,
                                nested: false,
                            },
                            v,
                        );
                        return true;
                    }
                    if kind == "prototype" {
                        let owner = make_id([stem.as_str(), owner.as_deref().unwrap_or("")]);
                        let id = make_id([owner.as_str(), name.as_str()]);
                        self.js_callable(
                            &id,
                            CallableInfo {
                                name: &name,
                                owner: &owner,
                                line,
                                method: true,
                                nested: false,
                            },
                            v,
                        );
                        return true;
                    }
                    // `Obj.m = fn` at module level on an object the module
                    // exports is a method of that object (0.9.77, #3778).
                    if kind == "object"
                        && let Some(owner) = owner.as_deref()
                        && let Some(program) = n.parent().filter(|p| p.kind_name() == "program")
                        && self
                            .js_exported
                            .get_or_insert_with(|| js_find_exported_objects(program, self.source))
                            .contains(owner)
                    {
                        let owner_id = make_id([stem.as_str(), owner]);
                        let id = make_id([owner_id.as_str(), name.as_str()]);
                        if !self.seen.contains(&owner_id) {
                            self.add_node(&owner_id, owner, line, None, Value::Null);
                            self.edge(&file, &owner_id, "contains", line, None, Value::Null);
                        }
                        self.js_callable(
                            &id,
                            CallableInfo {
                                name: &name,
                                owner: &owner_id,
                                line,
                                method: true,
                                nested: false,
                            },
                            v,
                        );
                        return true;
                    }
                } else if kind == "exports" {
                    let id = make_id([stem.as_str(), name.as_str()]);
                    if self.js_closures(v, &id) {
                        self.add_node(&id, &format!("{name}()"), line, None, Value::Null);
                        self.edge(&file, &id, "contains", line, None, Value::Null);
                        self.callable.insert(id);
                        return true;
                    }
                }
            }
        }
        if let Some(parent) = parent
            && matches!(t, "field_definition" | "public_field_definition")
            && let (Some(name), Some(value)) = (
                n.child_by_field_name("property")
                    .or_else(|| n.child_by_field_name("name")),
                n.child_by_field_name("value"),
            )
            && set("_JS_FUNCTION_VALUE_TYPES").contains(value.kind_name())
        {
            let name = self.text(name);
            if !name.is_empty() {
                let id = make_id([parent, name.as_str()]);
                self.js_callable(
                    &id,
                    CallableInfo {
                        name: &name,
                        owner: parent,
                        line,
                        method: true,
                        nested: false,
                    },
                    value,
                );
                return true;
            }
        }
        if matches!(t, "lexical_declaration" | "variable_declaration") {
            let requires = self.require_js(n, &file);
            let p = n.parent();
            let exported = p.is_some_and(|p| p.kind_name() == "export_statement");
            let module = p.is_some_and(|p| {
                p.kind_name() == "program"
                    || (exported && p.parent().is_some_and(|p| p.kind_name() == "program"))
            });
            let mut found = false;
            if t == "lexical_declaration" && module {
                for d in children(n)
                    .into_iter()
                    .filter(|c| c.kind_name() == "variable_declarator")
                {
                    let (Some(name), Some(value)) = (
                        d.child_by_field_name("name"),
                        d.child_by_field_name("value"),
                    ) else {
                        continue;
                    };
                    let line = d.start_position().row + 1;
                    let name_text = self.text(name);
                    if set("_JS_FUNCTION_VALUE_TYPES").contains(value.kind_name()) {
                        if crate::ids::normalize_id(&name_text).is_empty() {
                            continue;
                        }
                        let id = make_id([stem.as_str(), name_text.as_str()]);
                        self.js_callable(
                            &id,
                            CallableInfo {
                                name: &name_text,
                                owner: &file,
                                line,
                                method: false,
                                nested: true,
                            },
                            value,
                        );
                        found = true;
                    } else if (exported
                        && name.kind_name() == "identifier"
                        && !crate::ids::normalize_id(&name_text).is_empty())
                        || [
                            "object",
                            "array",
                            "as_expression",
                            "satisfies_expression",
                            "call_expression",
                            "new_expression",
                        ]
                        .contains(&value.kind_name())
                    {
                        if exported && name.kind_name() == "object_pattern" {
                            for prop in named(name) {
                                let name = if prop.kind_name()
                                    == "shorthand_property_identifier_pattern"
                                {
                                    Some(self.text(prop))
                                } else if prop.kind_name() == "pair_pattern" {
                                    prop.child_by_field_name("key").map(|n| self.text(n))
                                } else {
                                    None
                                };
                                if let Some(name) =
                                    name.filter(|s| !crate::ids::normalize_id(s).is_empty())
                                {
                                    let id = make_id([stem.as_str(), name.as_str()]);
                                    self.add_node(&id, &name, line, None, Value::Null);
                                    self.edge(&file, &id, "contains", line, None, Value::Null);
                                }
                            }
                        } else {
                            let id = make_id([stem.as_str(), name_text.as_str()]);
                            self.add_node(&id, &name_text, line, None, Value::Null);
                            self.edge(&file, &id, "contains", line, None, Value::Null);
                            self.js_closures(value, &id);
                        }
                        found = true;
                    }
                }
            }
            return found || requires;
        }
        false
    }
    pub(crate) fn extra(&mut self, n: Node<'t>, parent: Option<&str>) -> bool {
        let t = n.kind_name();
        let line = n.start_position().row + 1;
        if self.js() && self.js_extra(n, parent) {
            return true;
        }
        if self.is("typescript") && n.is_named() && matches!(t, "internal_module" | "module") {
            let name = n
                .child_by_field_name("name")
                .or_else(|| first_child(n, &["identifier", "nested_identifier", "string"]));
            if let Some(name) = name {
                let name = self.text(name).trim_matches(['\'', '"', '`']).to_string();
                if !name.is_empty() {
                    let id = make_id([self.stem.as_str(), name.as_str()]);
                    self.add_node(&id, &name, line, None, Value::Null);
                    let file = self.file.clone();
                    self.edge(&file, &id, "contains", line, None, Value::Null);
                }
            }
            if let Some(b) = n
                .child_by_field_name("body")
                .or_else(|| first_child(n, &["statement_block"]))
            {
                for c in children(b) {
                    self.walk(c, parent);
                }
            }
            return true;
        }
        if self.is("c_sharp")
            && matches!(
                t,
                "namespace_declaration" | "file_scoped_namespace_declaration"
            )
        {
            let name = self.name(n).unwrap_or_default().trim().to_string();
            let pushed = !name.is_empty();
            if pushed {
                self.namespace.push(name);
                // `_csharp_scope_id` (0.9.78, #4165): row and column, not the
                // byte offset, so CRLF and LF checkouts agree.
                let at = n.start_position();
                self.scopes.push(format!("s{}:{}", at.row, at.column));
                let label = self.namespace.join(".");
                let id = csharp_namespace_id(&label);
                self.add_node(
                    &id,
                    &label,
                    line,
                    Some("namespace"),
                    json!({"kind":"csharp_namespace"}),
                );
                let file = self.file.clone();
                self.edge(&file, &id, "contains", line, None, Value::Null);
            }
            if t == "namespace_declaration" {
                if let Some(b) = n.child_by_field_name("body") {
                    for c in children(b) {
                        self.walk(c, parent);
                    }
                }
                if pushed {
                    self.namespace.pop();
                    self.scopes.pop();
                }
            }
            return true;
        }
        if self.is("c_sharp") && parent.is_some() && t.starts_with("preproc_") {
            for c in children(n) {
                self.walk(c, parent);
            }
            return true;
        }
        if let Some(parent) = parent
            && ((self.is("scala") && t == "enum_case_definitions")
                || (self.is("cpp") && t == "enumerator")
                || (self.is("php") && t == "enum_case"))
        {
            // Scala 3 enum cases, C++ enumerators (0.9.73, #3939) and PHP 8.1
            // enum cases (0.9.75) become nodes with a `case_of` edge to their
            // enum. One Scala `case A, B` declares several members; each
            // starts with its name.
            let members: Vec<(Node<'t>, Node<'t>)> = if t == "enumerator" || t == "enum_case" {
                n.child_by_field_name("name")
                    .or_else(|| first_child(n, &["identifier", "name"]))
                    .map(|name| (n, name))
                    .into_iter()
                    .collect()
            } else {
                children(n)
                    .into_iter()
                    .filter(|c| matches!(c.kind_name(), "simple_enum_case" | "full_enum_case"))
                    .filter_map(|c| first_child(c, &["identifier"]).map(|name| (c, name)))
                    .collect()
            };
            for (member, name) in members {
                let name = self.text(name);
                if name.is_empty() {
                    continue;
                }
                let line = member.start_position().row + 1;
                let id = make_id([parent, name.as_str()]);
                if !self.seen.contains(&id) {
                    self.add_node(&id, &name, line, None, Value::Null);
                    self.edge(parent, &id, "case_of", line, None, Value::Null);
                }
            }
            return true;
        }
        if self.is("ruby") && self.ruby_extra(n, parent) {
            return true;
        }
        if let Some(parent) = parent {
            let case = (self.is("typescript")
                && n.parent().is_some_and(|p| p.kind_name() == "enum_body")
                && matches!(t, "property_identifier" | "enum_assignment"))
                || (self.is("c_sharp") && t == "enum_member_declaration")
                // Groovy shares Java's `enum_constant` shape (0.9.71).
                || ((self.is("java") || self.is("groovy")) && t == "enum_constant")
                || ((self.is("kotlin") || self.is("swift")) && t == "enum_entry");
            if case {
                let names = if self.is("swift") {
                    children(n)
                        .into_iter()
                        .filter(|n| n.kind_name() == "simple_identifier")
                        .collect::<Vec<_>>()
                } else {
                    n.child_by_field_name("name")
                        .or_else(|| {
                            if t == "property_identifier" {
                                Some(n)
                            } else {
                                first_child(n, &["simple_identifier", "identifier"])
                            }
                        })
                        .into_iter()
                        .collect()
                };
                let mut last = parent.to_string();
                for name in names {
                    // Only TypeScript's enum members lose their quotes
                    // (`'a' = 1`); Swift and Kotlin keep an escaped
                    // identifier's backticks (`` case `default` ``).
                    let name = if self.is("typescript") {
                        self.text(name).trim_matches(['\'', '"', '`']).to_string()
                    } else {
                        self.text(name)
                    };
                    if name.is_empty() {
                        continue;
                    }
                    let mut id = make_id([parent, name.as_str()]);
                    if self.is("c_sharp") {
                        // `enum E { Value, value }`: the pre-scan salts the
                        // pair apart (#4415); the guard stays for anything
                        // else landing on one id.
                        id = python_underscore_salted_nid(&id, &name, &self.csharp_member_groups);
                    }
                    if !self.seen.contains(&id) || !self.is("c_sharp") {
                        self.add_node(&id, &name, line, None, Value::Null);
                        self.edge(parent, &id, "case_of", line, None, Value::Null);
                    }
                    last = id;
                }
                if self.is("swift")
                    && let Some(ps) = first_child(n, &["enum_type_parameters"])
                {
                    for p in named(ps) {
                        self.refs(parent, Some(p), line, "type", false);
                    }
                }
                if (self.is("java") || self.is("groovy") || self.is("kotlin"))
                    && let Some(b) = first_child(n, &["class_body"])
                {
                    for c in children(b) {
                        self.walk(c, Some(&last));
                    }
                }
                if t == "enum_assignment"
                    && let Some(v) = n.child_by_field_name("value")
                {
                    self.walk(v, Some(parent));
                }
                return true;
            }
        }
        false
    }
}
