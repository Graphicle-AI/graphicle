// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
//! Class, field and signature traversal for the shared engine.
use super::{
    base::make_id,
    engine::{Engine, decorator_name},
    engine_scope::*,
    engine_types::*,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tree_sitter::Node;
fn upper(s: &str) -> bool {
    s.chars().next().is_some_and(char::is_uppercase)
}
impl<'a, 't> Engine<'a, 't> {
    pub(crate) fn class(&mut self, n: Node<'t>, parent: Option<&str>) {
        let t = n.kind_name();
        let mut name = match self.name(n) {
            Some(name) => name,
            // A Scala 3 anonymous given (`given Show[Int] with`) is named
            // after the type it implements (0.9.85, #4130).
            None if self.is("scala") && t == "given_definition" => {
                scala_given_synthetic_name(n, self.source)
            }
            None => return,
        };
        let line = n.start_position().row + 1;
        let mut segments = vec![];
        let absolute = self.is("ruby") && name.starts_with("::");
        if self.is("ruby") {
            segments = name.split("::").map(str::to_string).collect::<Vec<_>>();
            let mut all = self.ruby_namespace.clone();
            all.extend(segments.clone());
            name = all.join("::");
        }
        // C#: the arity is part of a generic type's id (`` Effect`1 ``), so a
        // generic and a non-generic type of one name stay apart (0.9.83,
        // #4249); the label is unchanged.
        let mut name_for_id = name.clone();
        if self.is("c_sharp") {
            let arity = children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "type_parameter_list")
                .flat_map(children)
                .filter(|p| matches!(p.kind_name(), "type_parameter" | "identifier"))
                .count();
            if arity > 0 {
                name_for_id = format!("{name}`{arity}");
            }
        }
        let mut id = make_id([
            self.stem.as_str(),
            self.namespace.join(".").as_str(),
            name_for_id.as_str(),
        ]);
        // Python nested classes are qualified by their class (0.9.81, #4228).
        if self.is("python")
            && let Some(parent) = parent
        {
            id = make_id([parent, name.as_str()]);
        }
        let mut meta = json!({});
        if self.is("c_sharp") {
            // The type's path within its namespace, enclosing types first with
            // their arities; None when an enclosing path is unknown or two
            // declarations of this id disagree (0.9.87, #4363).
            let mut type_path = match parent {
                Some(parent) => self
                    .csharp_type_paths
                    .get(parent)
                    .cloned()
                    .flatten()
                    .map(|outer| format!("{outer}.{name_for_id}")),
                None => Some(name_for_id.clone()),
            };
            match self.csharp_type_paths.get(&id) {
                None => {
                    self.csharp_type_paths.insert(id.clone(), type_path.clone());
                }
                Some(previous) if *previous != type_path => {
                    self.csharp_type_paths.insert(id.clone(), None);
                    type_path = None;
                    for node in &mut self.nodes {
                        if node["id"] == id.as_str()
                            && let Some(m) = node["metadata"].as_object_mut()
                        {
                            m.remove("type_path");
                        }
                    }
                }
                Some(_) => {}
            }
            if parent.is_some() {
                meta["is_nested_type"] = json!(true);
            }
            if [
                "class_declaration",
                "struct_declaration",
                "interface_declaration",
                "record_declaration",
            ]
            .contains(&t)
                && children(n)
                    .iter()
                    .any(|c| c.kind_name() == "modifier" && self.text(*c) == "partial")
            {
                meta["is_partial"] = json!(true);
                if let Some(path) = type_path {
                    meta["type_path"] = json!(path);
                }
            }
        }
        if self.is("python")
            && let Some(bases) = n.child_by_field_name("superclasses")
            && children(bases).iter().any(|c| {
                c.is_named()
                    && !matches!(c.kind_name(), "identifier" | "keyword_argument" | "comment")
            })
        {
            // Only a bare-name base becomes an `inherits` edge; a base the
            // edges cannot show (`mod.Base`, `Generic[T]`, a call) still sits
            // in the MRO (0.9.81).
            meta["python_opaque_bases"] = json!(true);
        }
        if self.is("ruby") {
            if absolute {
                meta["ruby_lookup_unsafe"] = json!(true);
            }
            if self.seen.contains(&id) {
                meta["ruby_reopened"] = json!(true);
            }
            if find_body(n, self.config)
                .is_some_and(|b| super::engine_ruby::lookup_barrier(b, self.source))
            {
                meta["ruby_lookup_unsafe"] = json!(true);
            }
        }
        self.add_node(&id, &name, line, None, meta.clone());
        if self.is("ruby")
            && meta.as_object().is_some_and(|m| !m.is_empty())
            && let Some(node) = self.nodes.iter_mut().find(|v| v["id"] == id)
        {
            if !node["metadata"].is_object() {
                node["metadata"] = json!({});
            }
            node["metadata"]
                .as_object_mut()
                .unwrap()
                .extend(meta.as_object().unwrap().clone());
        }
        self.callable.insert(id.clone());
        self.classes.insert(id.clone());
        let owner = parent
            .filter(|p| *p != id)
            .unwrap_or(&self.file)
            .to_string();
        self.edge(&owner, &id, "contains", line, None, Value::Null);
        if self.js() {
            self.ts_decorators(n, &id);
        }
        if self.is("python")
            && let Some(args) = n.child_by_field_name("superclasses")
        {
            for base in children(args)
                .into_iter()
                .filter(|c| c.kind_name() == "identifier")
            {
                let name = self.text(base);
                let target = self.ensure(&name, true);
                self.edge(&id, &target, "inherits", line, None, Value::Null);
            }
        }
        if self.is("swift") {
            if children(n).iter().any(|c| c.kind_name() == "extension") {
                self.swift_extensions.push(json!({"nid":id,"label":name}));
            }
            let mut first = true;
            for c in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "inheritance_specifier")
            {
                if let Some(base) = first_child(c, &["user_type", "type_identifier"]) {
                    let name = if base.kind_name() == "user_type" {
                        swift_user_type_name(Some(base), self.source)
                    } else {
                        Some(self.text(base))
                    };
                    if let Some(name) = name {
                        let target = self.ensure(&name, false);
                        let rel = if t == "protocol_declaration" {
                            "inherits"
                        } else {
                            swift_classify_base(
                                &name,
                                swift_declaration_keyword(n),
                                first,
                                &self.protocols,
                                &self.swift_classes,
                            )
                        };
                        first = false;
                        self.edge(&id, &target, rel, line, None, Value::Null);
                        if let Some(args) = first_child(base, &["type_arguments"]) {
                            for a in named(args) {
                                self.type_refs(&id, Some(a), line, "generic_arg", true, false);
                            }
                        }
                    }
                }
            }
        }
        if self.is("php") {
            for c in children(n) {
                let rel = match c.kind_name() {
                    "base_clause" => "inherits",
                    "class_interface_clause" => "implements",
                    _ => continue,
                };
                for sub in children(c)
                    .into_iter()
                    .filter(|s| matches!(s.kind_name(), "name" | "qualified_name"))
                {
                    if let Some(name) = php_name_text(Some(sub), self.source) {
                        let target = self.ensure(&name, false);
                        self.edge(
                            &id,
                            &target,
                            rel,
                            c.start_position().row + 1,
                            None,
                            Value::Null,
                        );
                    }
                }
            }
            if let Some(b) = find_body(n, self.config) {
                for c in children(b)
                    .into_iter()
                    .filter(|c| c.kind_name() == "use_declaration")
                {
                    for sub in children(c)
                        .into_iter()
                        .filter(|s| matches!(s.kind_name(), "name" | "qualified_name"))
                    {
                        if let Some(name) = php_name_text(Some(sub), self.source) {
                            let target = self.ensure(&name, false);
                            self.edge(
                                &id,
                                &target,
                                "mixes_in",
                                c.start_position().row + 1,
                                None,
                                Value::Null,
                            );
                        }
                    }
                }
            }
        }
        if self.is("kotlin") {
            for (base, rel, ty) in self.kotlin_bases(n) {
                let target = self.ensure(&base, true);
                self.edge(&id, &target, rel, line, None, Value::Null);
                if let Some(args) = first_child(ty, &["type_arguments"]) {
                    for a in named(args) {
                        self.type_refs(&id, Some(a), line, "generic_arg", true, false);
                    }
                }
            }
            self.kotlin_annotations(n, &id, line, true);
            // #3848: `class Foo(val repo: Repo)` declares a property, so its type
            // is a field reference; plain constructor parameters are not.
            for ctor in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "primary_constructor")
            {
                for list in children(ctor)
                    .into_iter()
                    .filter(|c| c.kind_name() == "class_parameters")
                {
                    for p in children(list)
                        .into_iter()
                        .filter(|c| c.kind_name() == "class_parameter")
                    {
                        let parts = children(p);
                        if !parts.iter().any(|c| matches!(c.kind_name(), "val" | "var")) {
                            continue;
                        }
                        let Some(ty) = parts.iter().copied().find(|c| {
                            matches!(
                                c.kind_name(),
                                "user_type" | "nullable_type" | "type_reference"
                            )
                        }) else {
                            continue;
                        };
                        let p_line = p.start_position().row + 1;
                        self.refs(&id, Some(ty), p_line, "field", false);
                        self.kotlin_annotations(p, &id, p_line, true);
                    }
                }
            }
        }
        if self.is("ruby") {
            if let Some(sup) = n.child_by_field_name("superclass")
                && let Some(base) = first_child(sup, &["constant", "scope_resolution"])
            {
                let raw = self.text(base).trim().to_string();
                let name = raw.rsplit("::").next().unwrap_or("");
                if !name.is_empty() {
                    let target = self.ensure(name, true);
                    let scopes: Vec<_> = self.ruby_lexical.iter().rev().collect();
                    self.edge(
                        &id,
                        &target,
                        "inherits",
                        line,
                        None,
                        json!({"ruby_superclass_ref":raw,"ruby_lexical_scopes":scopes}),
                    );
                }
            }
            if let Some(b) = find_body(n, self.config) {
                for stmt in children(b) {
                    if stmt.kind_name() != "call" || stmt.child_by_field_name("receiver").is_some()
                    {
                        continue;
                    }
                    let Some(m) = stmt.child_by_field_name("method") else {
                        continue;
                    };
                    if !["include", "extend", "prepend"].contains(&self.text(m).as_str()) {
                        continue;
                    }
                    if let Some(args) = stmt.child_by_field_name("arguments") {
                        for a in children(args)
                            .into_iter()
                            .filter(|a| matches!(a.kind_name(), "constant" | "scope_resolution"))
                        {
                            let module = self.text(a).trim().to_string();
                            if !module.is_empty() {
                                self.mixins.push(json!({"caller_nid":id,"callee":module,"is_mixin":true,"source_file":self.path,"source_location":format!("L{}",stmt.start_position().row+1)}));
                            }
                        }
                    }
                }
            }
        }
        if self.is("c_sharp") {
            let params = csharp_type_parameters_in_scope(n, self.source);
            if let Some(list) = first_child(n, &["base_list"]) {
                for b in children(list).into_iter().filter(|b| {
                    matches!(
                        b.kind_name(),
                        "identifier" | "generic_name" | "qualified_name"
                    )
                }) {
                    if let Some((name, q, qual)) = read_csharp_type_name(Some(b), self.source) {
                        if params.contains(&name) || name.is_empty() {
                            continue;
                        }
                        // A generic base reaches this file's type of the same
                        // arity (`Tween<T> : Effect<T>`, #4249); otherwise the
                        // bare name, as before.
                        let arity = if b.kind_name() == "generic_name" {
                            children(b)
                                .into_iter()
                                .rfind(|c| c.kind_name() == "type_argument_list")
                                .map_or(0, |list| named(list).len())
                        } else {
                            0
                        };
                        let generic = make_id([
                            self.stem.as_str(),
                            self.namespace.join(".").as_str(),
                            format!("{name}`{arity}").as_str(),
                        ]);
                        let target = if arity > 0 && self.seen.contains(&generic) {
                            generic
                        } else {
                            self.ensure(&name, false)
                        };
                        let rel = if t == "interface_declaration" {
                            "inherits"
                        } else {
                            csharp_classify_base(&name, &self.interfaces)
                        };
                        let mut m = json!({"ref_token":name});
                        if q {
                            m["qualified"] = json!(true);
                        }
                        if !qual.is_empty() {
                            m["ref_qualifier"] = json!(qual);
                        }
                        self.edge(&id, &target, rel, line, None, m);
                        if b.kind_name() == "generic_name"
                            && let Some(args) = first_child(b, &["type_argument_list"])
                        {
                            for a in named(args) {
                                self.type_refs(&id, Some(a), line, "generic_arg", true, false);
                            }
                        }
                    }
                }
            }
            if [
                "class_declaration",
                "record_declaration",
                "struct_declaration",
            ]
            .contains(&t)
                && let Some(ps) = first_child(n, &["parameter_list"])
            {
                for p in children(ps)
                    .into_iter()
                    .filter(|p| p.kind_name() == "parameter")
                {
                    let ty = p.child_by_field_name("type");
                    if let Some(name) = p.child_by_field_name("name")
                        && let Some(recv) = csharp_receiver_type_name(ty, self.source)
                            .filter(|s| upper(s) && !params.contains(s))
                    {
                        let name = self.text(name);
                        self.field_table(&id).insert(name, recv);
                    }
                    self.refs(&id, ty, p.start_position().row + 1, "field", false);
                }
            }
        }
        if self.is("java") || self.is("groovy") {
            let mut parents = vec![];
            if let Some(sup) = n.child_by_field_name("superclass")
                && let Some(b) = named(sup).first()
            {
                parents.push((*b, "inherits"));
            }
            if let Some(ifs) = n.child_by_field_name("interfaces")
                && let Some(list) = first_child(ifs, &["type_list"])
            {
                parents.extend(named(list).into_iter().map(|b| (b, "implements")));
            }
            if t == "interface_declaration" {
                for c in children(n)
                    .into_iter()
                    .filter(|c| c.kind_name() == "extends_interfaces")
                {
                    if let Some(list) = first_child(c, &["type_list"]) {
                        parents.extend(named(list).into_iter().map(|b| (b, "inherits")));
                    }
                }
            }
            for (b, rel) in parents {
                let mut refs = vec![];
                java_collect_type_refs(Some(b), self.source, false, &mut refs, None, false);
                let mut emitted = false;
                for (name, role) in refs {
                    if role == "type" && !emitted {
                        let target = self.ensure(&name, false);
                        self.edge(&id, &target, rel, line, None, Value::Null);
                        emitted = true;
                    } else if role == "generic_arg" {
                        let target = self.ensure(&name, true);
                        if target != id {
                            self.edge(
                                &id,
                                &target,
                                "references",
                                line,
                                Some("generic_arg"),
                                Value::Null,
                            );
                        }
                    }
                }
            }
            self.java_annotations(n, &id, line);
            if t == "record_declaration"
                && let Some(ps) = n.child_by_field_name("parameters")
            {
                for p in named(ps) {
                    let ty = if p.kind_name() == "formal_parameter" {
                        p.child_by_field_name("type")
                    } else if p.kind_name() == "spread_parameter" {
                        named(p)
                            .into_iter()
                            .find(|c| !matches!(c.kind_name(), "modifiers" | "variable_declarator"))
                    } else {
                        None
                    };
                    self.refs(&id, ty, p.start_position().row + 1, "field", false);
                }
            }
        }
        if self.is("cpp")
            && let Some(base) = first_child(n, &["base_class_clause"])
        {
            for b in children(base) {
                let (name, args) = match b.kind_name() {
                    "type_identifier" => (Some(self.text(b)), None),
                    "qualified_identifier" => (
                        Some(self.text(b.child_by_field_name("name").unwrap_or(b))),
                        None,
                    ),
                    "template_type" => (
                        Some(self.text(b.child_by_field_name("name").unwrap_or(b))),
                        b.child_by_field_name("arguments"),
                    ),
                    _ => (None, None),
                };
                if let Some(name) = name.filter(|s| !s.is_empty()) {
                    let target = self.ensure(&name, true);
                    self.edge(&id, &target, "inherits", line, None, Value::Null);
                    if let Some(args) = args {
                        for a in named(args) {
                            self.refs(&id, Some(a), line, "generic_arg", true);
                        }
                    }
                }
            }
        }
        if self.is("scala") {
            if let Some(base) = n
                .child_by_field_name("extend")
                .or_else(|| first_child(n, &["extends_clause"]))
            {
                let mut index = 0;
                for b in children(base) {
                    if !["type_identifier", "stable_type_identifier", "generic_type"]
                        .contains(&b.kind_name())
                    {
                        continue;
                    }
                    if let Some(name) = scala_base_name(b, self.source) {
                        let target = self.ensure(&name, true);
                        if target != id {
                            self.edge(
                                &id,
                                &target,
                                if index == 0 { "inherits" } else { "mixes_in" },
                                b.start_position().row + 1,
                                None,
                                Value::Null,
                            );
                        }
                        index += 1;
                    }
                }
            }
            for ps in children(n)
                .into_iter()
                .filter(|p| p.kind_name() == "class_parameters")
            {
                for p in children(ps)
                    .into_iter()
                    .filter(|p| p.kind_name() == "class_parameter")
                {
                    self.refs(
                        &id,
                        p.child_by_field_name("type"),
                        p.start_position().row + 1,
                        "field",
                        false,
                    );
                }
            }
            // Class-level context bounds (`class Foo[A: Ordering]`, traits and
            // enums too) are the method-level typeclass dependency, attributed
            // to the type (0.9.77, #4080).
            for tp in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "type_parameters")
            {
                let mut refs = vec![];
                scala_collect_context_bounds(tp, self.source, &mut refs);
                for (name, role) in refs {
                    let target = self.ensure(&name, true);
                    if target != id {
                        let context = if role == "generic_arg" {
                            "generic_arg"
                        } else {
                            "type_bound"
                        };
                        self.edge(&id, &target, "references", line, Some(context), Value::Null);
                    }
                }
            }
            // `derives Show, cats.Eq` names typeclasses the type depends on,
            // like a context bound; a qualified name links by its last
            // segment (0.9.86, #2048).
            if let Some(derive) = n.child_by_field_name("derive") {
                for d in derive
                    .children_by_field_name("type", &mut derive.walk())
                    .collect::<Vec<_>>()
                {
                    let tail = if d.kind_name() == "type_identifier" {
                        Some(d)
                    } else {
                        children(d)
                            .into_iter()
                            .rev()
                            .find(|x| x.kind_name() == "type_identifier")
                    };
                    let Some(tail) = tail else { continue };
                    let d_line = d.start_position().row + 1;
                    let target = self.ensure(&self.text(tail), true);
                    if target != id {
                        self.edge(
                            &id,
                            &target,
                            "references",
                            d_line,
                            Some("derives"),
                            Value::Null,
                        );
                    }
                }
            }
        }
        if let Some(body) = find_body(n, self.config) {
            let depth = self.singleton_depth;
            if self.is("ruby") {
                self.ruby_namespace.extend(segments.clone());
                self.ruby_lexical.push(name);
                self.singleton_depth = 0;
            }
            for c in children(body) {
                self.walk(c, Some(&id));
            }
            if self.is("ruby") {
                self.singleton_depth = depth;
                self.ruby_lexical.pop();
                self.ruby_namespace
                    .truncate(self.ruby_namespace.len() - segments.len());
            }
        }
    }
    /// The member field table of `class`, opened in upstream's dict order.
    pub(crate) fn field_table(&mut self, class: &str) -> &mut super::engine::Table {
        if !self.fields.contains_key(class) {
            self.field_order.push(class.to_string());
        }
        self.fields.entry(class.to_string()).or_default()
    }
    /// Java/Groovy anonymous classes (`new Runnable() { ... }`, 0.9.80,
    /// #4140): an owner node per class body in this function body, walked
    /// like a class so its members and their calls get nodes. The base type
    /// is only the label: whether it is an interface or a class is unknown.
    pub(crate) fn java_anonymous_classes(&mut self, body: Node<'t>, owner: &str) {
        let mut stack = children(body);
        let mut found = vec![];
        while let Some(n) = stack.pop() {
            if matches!(n.kind_name(), "method_declaration" | "lambda_expression") {
                continue;
            }
            if n.kind_name() == "object_creation_expression"
                && children(n).iter().any(|c| c.kind_name() == "class_body")
            {
                found.push(n);
                continue;
            }
            stack.extend(children(n));
        }
        found.sort_by_key(Node::start_byte);
        for anon in found {
            let line = anon.start_position().row + 1;
            let base = anon
                .child_by_field_name("type")
                .map(|t| {
                    self.text(t)
                        .trim_matches(crate::dedup_strings::whitespace)
                        .to_string()
                })
                .unwrap_or_default();
            let label = if base.is_empty() {
                format!("object@L{line}")
            } else {
                base
            };
            let id = make_id([
                owner,
                format!("object:{label}").as_str(),
                format!("L{line}").as_str(),
            ]);
            self.add_node(&id, &label, line, None, Value::Null);
            self.edge(owner, &id, "contains", line, None, Value::Null);
            self.callable.insert(id.clone());
            self.classes.insert(id.clone());
            if let Some(b) = first_child(anon, &["class_body"]) {
                for c in children(b) {
                    self.walk(c, Some(&id));
                }
            }
        }
    }
    /// Kotlin annotations reference their type (#3842), once per target.
    /// Class, constructor-property and property annotations also reference
    /// the class literals in their arguments (0.9.74 #3835); upstream leaves
    /// function annotations without them.
    pub(crate) fn kotlin_annotations(
        &mut self,
        n: Node<'t>,
        owner: &str,
        line: usize,
        class_literals: bool,
    ) {
        let mut seen = BTreeSet::new();
        let mut names = kotlin_annotation_names(n, self.source);
        if class_literals {
            names.extend(kotlin_annotation_class_literal_refs(n, self.source));
        }
        for name in names {
            let target = self.ensure(&name, true);
            if target != owner && seen.insert(target.clone()) {
                self.edge(
                    owner,
                    &target,
                    "references",
                    line,
                    Some("attribute"),
                    Value::Null,
                );
            }
        }
    }
    pub(crate) fn java_annotations(&mut self, n: Node<'t>, id: &str, line: usize) {
        let mut names = java_annotation_names(n, self.source)
            .into_iter()
            .map(|(name, raw)| {
                if self.is("java") && raw.contains('.') {
                    raw
                } else {
                    name
                }
            })
            .collect::<Vec<_>>();
        names.extend(java_annotation_class_literal_refs(n, self.source));
        let mut seen = BTreeSet::new();
        for name in names {
            let target = self.ensure(&name, true);
            if target != id && seen.insert(target.clone()) {
                self.edge(
                    id,
                    &target,
                    "references",
                    line,
                    Some("attribute"),
                    Value::Null,
                );
            }
        }
    }
    pub(crate) fn function_types(&mut self, n: Node<'t>, id: &str, parent: Option<&str>) {
        let line = n.start_position().row + 1;
        if self.is("python") {
            for (name, role) in
                python_collect_param_refs(n.child_by_field_name("parameters"), self.source)
            {
                let target = self.ensure(&name, true);
                if target != id {
                    self.edge(
                        id,
                        &target,
                        "references",
                        line,
                        Some(if role == "generic_arg" {
                            "generic_arg"
                        } else {
                            "parameter_type"
                        }),
                        Value::Null,
                    );
                }
            }
            self.refs(
                id,
                n.child_by_field_name("return_type"),
                line,
                "return_type",
                false,
            );
            return;
        }
        let mut ps = n.child_by_field_name("parameters").or_else(|| {
            first_child(
                n,
                &[
                    "formal_parameters",
                    "function_value_parameters",
                    "parameters",
                ],
            )
        });
        if self.is("scala") {
            // `type_parameters` shares the `parameters` field name on a
            // Scala function_definition; upstream finds the clause by type.
            ps = first_child(n, &["parameters"]);
        }
        let mut ret = n.child_by_field_name("return_type");
        if self.is("c_sharp") {
            ret = n.child_by_field_name("returns");
        } else if self.is("java") {
            ret = n.child_by_field_name("type");
        } else if self.is("php") {
            ret = php_method_return_type_node(n);
        } else if self.is("kotlin") {
            ret = kotlin_function_return_type_node(n);
        } else if self.is("c") || self.is("cpp") {
            ret = n.child_by_field_name("type");
            let mut d = n.child_by_field_name("declarator");
            while let Some(inner) =
                d.filter(|d| matches!(d.kind_name(), "pointer_declarator" | "reference_declarator"))
            {
                d = inner.child_by_field_name("declarator");
            }
            ps = d
                .filter(|d| d.kind_name() == "function_declarator")
                .and_then(|d| d.child_by_field_name("parameters"));
        }
        // C and C++ reference the return type before the parameter types;
        // every other language lists the parameters first. When a type is
        // both (`jv f(jv a)`), the order decides which `context` the graph's
        // single edge keeps (the later one, as in upstream's graph build).
        let return_first = self.is("c") || self.is("cpp");
        if return_first {
            self.refs(id, ret, line, "return_type", false);
        }
        let params = if self.is("swift") {
            children(n)
                .into_iter()
                .filter(|p| p.kind_name() == "parameter")
                .collect()
        } else {
            ps.map(children).unwrap_or_default()
        };
        for p in params {
            let accepted = if self.is("java") {
                p.kind_name() == "formal_parameter"
            } else {
                [
                    "parameter",
                    "simple_parameter",
                    "property_promotion_parameter",
                    "parameter_declaration",
                    "required_parameter",
                    "optional_parameter",
                ]
                .contains(&p.kind_name())
            };
            if !accepted {
                continue;
            }
            let ty = p.child_by_field_name("type").or_else(|| {
                if self.is("php") {
                    first_child(
                        p,
                        &[
                            "named_type",
                            "primitive_type",
                            "nullable_type",
                            "union_type",
                            "intersection_type",
                            "optional_type",
                        ],
                    )
                } else if self.is("kotlin") {
                    first_child(p, &["user_type", "nullable_type", "type_reference"])
                } else {
                    None
                }
            });
            self.refs(id, ty, line, "parameter_type", false);
            if self.is("php")
                && p.kind_name() == "property_promotion_parameter"
                && let Some(owner) = parent
            {
                self.refs(owner, ty, line, "field", false);
            }
            if self.is("swift") {
                let mut refs = vec![];
                swift_collect_type_refs(ty, self.source, false, &mut refs);
                if let (Some(name), Some((kind, _))) = (
                    p.child_by_field_name("name"),
                    refs.into_iter().find(|(_, r)| r == "type"),
                ) {
                    self.type_table.insert(self.text(name), kind);
                }
            }
            if self.js()
                && self.name(n).as_deref() == Some("constructor")
                && children(p)
                    .iter()
                    .any(|c| ["accessibility_modifier", "readonly"].contains(&c.kind_name()))
                && let (Some(name), Some(ty)) = (
                    p.child_by_field_name("pattern"),
                    ty.and_then(|t| first_child(t, &["type_identifier"])),
                )
            {
                self.type_table.insert(self.text(name), self.text(ty));
            }
        }
        let start = self.edges.len();
        if !return_first {
            self.refs(id, ret, line, "return_type", false);
        }
        if self.is("scala") {
            // Context bounds (`def sort[A: Ordering]`) name the typeclass the
            // function depends on (Graphify 562dfd6); upper bounds do not.
            for tp in children(n)
                .into_iter()
                .filter(|c| c.kind_name() == "type_parameters")
            {
                let mut refs = vec![];
                scala_collect_context_bounds(tp, self.source, &mut refs);
                for (name, role) in refs {
                    let target = self.ensure(&name, true);
                    if target != id {
                        let context = if role == "generic_arg" {
                            "generic_arg"
                        } else {
                            "type_bound"
                        };
                        self.edge(id, &target, "references", line, Some(context), Value::Null);
                    }
                }
            }
        }
        if self.is("swift") && ret.is_some_and(|r| r.kind_name() == "user_type") {
            let count = self.edges[start..]
                .iter()
                .filter(|e| e["context"] == "return_type")
                .count();
            if count == 1 {
                for e in &mut self.edges[start..] {
                    if e["context"] == "return_type" {
                        e["metadata"] = json!({"swift_plain_return":true});
                    }
                }
            }
        }
        if self.is("java") {
            self.java_annotations(n, id, line);
        }
        if self.is("kotlin") {
            self.kotlin_annotations(n, id, line, false);
        }
        if self.is("c_sharp") {
            for (name, q, qual) in csharp_attribute_names(n, self.source) {
                let target = self.ensure(&name, true);
                if target != id {
                    let mut m = json!({"ref_token":name});
                    if q {
                        m["qualified"] = json!(true);
                    }
                    if !qual.is_empty() {
                        m["ref_qualifier"] = json!(qual);
                    }
                    self.edge(id, &target, "references", line, Some("attribute"), m);
                }
            }
        }
    }
    pub(crate) fn field(&mut self, n: Node<'t>, parent: Option<&str>) -> bool {
        let t = n.kind_name();
        let line = n.start_position().row + 1;
        if self.is("kotlin") && t == "property_declaration" {
            if let Some(p) = parent {
                self.refs(p, kotlin_property_type_node(n), line, "field", false);
                // Upstream reads an unbound `line` here when the property has
                // no explicit type; the property's own line is the intent.
                self.kotlin_annotations(n, p, line, true);
            }
            let owner = parent.unwrap_or(&self.file).to_string();
            let mut after = false;
            for c in children(n) {
                if !c.is_named() {
                    after |= c.kind_name() == "=";
                    continue;
                }
                if after {
                    self.initializers.push((owner.clone(), c));
                } else if c.kind_name() == "property_delegate" {
                    for sub in named(c) {
                        self.initializers.push((owner.clone(), sub));
                    }
                }
            }
            return true;
        }
        // Scala 3 bindings and type aliases (`type`, `opaque type`, match
        // types) can sit at file scope; the file node owns their references
        // then (Graphify 447d41d, cb973c0). Falls through so initializer
        // calls are still walked.
        if self.is("scala") && matches!(t, "val_definition" | "var_definition" | "type_definition")
        {
            let owner = parent.unwrap_or(&self.file).to_string();
            self.refs(&owner, n.child_by_field_name("type"), line, "field", false);
        }
        let Some(parent) = parent else { return false };
        if self.is("c_sharp")
            && t == "constructor_declaration"
            && let Some(body) = n.child_by_field_name("body")
        {
            // A C# constructor has no node of its own: its body's calls are
            // attributed to the declaring type (0.9.83, #4246). Not handled
            // here otherwise, so the walk carries on as before.
            self.method_scopes.insert(body.id(), (n, parent.into()));
            self.bodies.push((parent.into(), body));
            return false;
        }
        if self.is("c_sharp") && matches!(t, "field_declaration" | "property_declaration") {
            if t == "property_declaration"
                && let Some(name) = n.child_by_field_name("name")
            {
                let name = self.text(name);
                if !name.is_empty() {
                    let id = make_id([parent, name.as_str()]);
                    if !self.seen.contains(&id) {
                        self.add_node(&id, &name, line, None, Value::Null);
                        self.edge(parent, &id, "defines", line, Some("field"), Value::Null);
                    }
                }
            }
            let decl = first_child(n, &["variable_declaration"]);
            let ty = n
                .child_by_field_name("type")
                .or_else(|| decl.and_then(|d| d.child_by_field_name("type")));
            if let Some(ty) = ty {
                let params = csharp_type_parameters_in_scope(ty, self.source);
                if let Some(recv) =
                    csharp_receiver_type_name(Some(ty), self.source).filter(|s| !params.contains(s))
                {
                    let names = if t == "property_declaration" {
                        n.child_by_field_name("name")
                            .map(|n| vec![self.text(n)])
                            .unwrap_or_default()
                    } else if upper(&recv) {
                        self.field_table(parent);
                        decl.map(children)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|d| d.kind_name() == "variable_declarator")
                            .filter_map(|d| {
                                d.child_by_field_name("name")
                                    .or_else(|| first_child(d, &["identifier"]))
                            })
                            .map(|n| self.text(n))
                            .collect()
                    } else {
                        vec![]
                    };
                    // `name[]` types an element, for `_items[i].M()` (#4246).
                    let elem = csharp_element_type_name(Some(ty), self.source);
                    for name in names {
                        let table = self.field_table(parent);
                        if let Some(elem) = &elem {
                            table.insert(format!("{name}[]"), elem.clone());
                        }
                        table.insert(name, recv.clone());
                    }
                }
                self.refs(parent, Some(ty), line, "field", false);
            }
            return true;
        }
        if self.is("java") && t == "field_declaration" {
            // Every declared field, typed or not: a receiver outside these and
            // the method's own bindings is an inherited field (0.9.88).
            self.java_field_names
                .entry(parent.into())
                .or_default()
                .extend(java_declarator_names(n, self.source));
            let ty = n.child_by_field_name("type");
            if let Some(recv) = java_receiver_type_name(ty, self.source) {
                let names = java_declarator_names(n, self.source);
                let table = self.field_table(parent);
                for name in names {
                    table.insert(name, recv.clone());
                }
            }
            self.refs(parent, ty, line, "field", false);
            return true;
        }
        if self.is("java") && t == "annotation_type_element_declaration" {
            let mut refs = vec![];
            java_collect_type_refs(
                n.child_by_field_name("type"),
                self.source,
                false,
                &mut refs,
                None,
                true,
            );
            for (name, role) in refs {
                let target = self.ensure(&name, true);
                if target != parent {
                    self.edge(
                        parent,
                        &target,
                        "references",
                        line,
                        Some(if role == "generic_arg" {
                            "generic_arg"
                        } else {
                            "return_type"
                        }),
                        Value::Null,
                    );
                }
            }
            return true;
        }
        if self.is("php") && t == "property_declaration" {
            if self.php_listeners(n) {
                return true;
            }
            let ty = first_child(
                n,
                &[
                    "named_type",
                    "primitive_type",
                    "nullable_type",
                    "union_type",
                    "intersection_type",
                    "optional_type",
                ],
            );
            self.refs(parent, ty, line, "field", false);
            return true;
        }
        if self.is("swift") && t == "property_declaration" {
            let ty = swift_property_type_node(n);
            self.refs(parent, ty, line, "field", false);
            let mut refs = vec![];
            swift_collect_type_refs(ty, self.source, false, &mut refs);
            let mut prop_type = refs
                .into_iter()
                .find(|(_, r)| r == "type")
                .map(|(name, _)| name);
            let mut factory = None;
            for c in children(n) {
                if self.config.call_types.contains(c.kind_name()) {
                    self.initializers.push((parent.into(), c));
                    if prop_type.is_none() {
                        prop_type = swift_constructor_type(c, self.source);
                        if prop_type.is_none() {
                            factory = swift_factory_call(c, self.source);
                        }
                    }
                } else if c.kind_name() == "navigation_expression"
                    && prop_type.is_none()
                    && let Some(h) = c.child(0).filter(|c| c.kind_name() == "simple_identifier")
                {
                    let name = self.text(h);
                    if upper(&name) {
                        prop_type = Some(name);
                    }
                }
            }
            if prop_type.is_none() {
                prop_type = swift_attribute_type_name(n, self.source);
            }
            if let Some(name) = swift_property_name(n, self.source) {
                if let Some(ty) = prop_type {
                    self.type_table.insert(name.clone(), ty);
                } else if let Some(f) = factory {
                    self.swift_factory.entry(name.clone()).or_insert(f);
                }
                let bodies = children(n)
                    .into_iter()
                    .filter(|c| {
                        matches!(c.kind_name(), "computed_property" | "willset_didset_block")
                    })
                    .collect::<Vec<_>>();
                if !bodies.is_empty() {
                    let id = make_id([parent, name.as_str()]);
                    self.add_node(&id, &format!(".{name}"), line, None, Value::Null);
                    self.edge(parent, &id, "method", line, None, Value::Null);
                    for body in bodies {
                        self.bodies.push((id.clone(), body));
                    }
                }
            }
            return true;
        }
        if self.is("scala") && t == "self_type" {
            if let Some(ty) = named(n).get(1) {
                let mut refs = vec![];
                scala_collect_type_refs(Some(*ty), self.source, false, &mut refs);
                for (name, _) in refs {
                    let target = self.ensure(&name, true);
                    if target != parent {
                        self.edge(parent, &target, "requires", line, None, Value::Null);
                    }
                }
            }
            return true;
        }
        if self.is("cpp") && t == "field_declaration" {
            let decls = n
                .children_by_field_name("declarator", &mut n.walk())
                .collect::<Vec<_>>();
            let method = decls.iter().any(|d| {
                d.kind_name() == "function_declarator"
                    || (matches!(d.kind_name(), "pointer_declarator" | "reference_declarator")
                        && first_child(*d, &["function_declarator"]).is_some())
            });
            let ty = n.child_by_field_name("type");
            let nested = ty.is_some_and(|t| {
                self.config.class_types.contains(t.kind_name())
                    && t.child_by_field_name("body").is_some()
            });
            if nested {
                self.walk(ty.unwrap(), Some(parent));
            }
            if !method && !nested {
                self.refs(parent, ty, line, "field", false);
            }
            for d in decls {
                if let Some(name) = get_cpp_func_name(d, self.source) {
                    let id = make_id([parent, name.as_str()]);
                    let line = d.start_position().row + 1;
                    // A member-function declaration (prototype, virtual, or pure
                    // virtual) shares the method shape of its definition.
                    let decl_is_method = d.kind_name() == "function_declarator"
                        || (matches!(d.kind_name(), "pointer_declarator" | "reference_declarator")
                            && children(d)
                                .iter()
                                .any(|c| c.kind_name() == "function_declarator"));
                    if decl_is_method {
                        self.add_node(&id, &format!(".{name}()"), line, None, Value::Null);
                        self.edge(parent, &id, "method", line, None, Value::Null);
                        self.callable.insert(id);
                    } else {
                        self.add_node(&id, &name, line, None, Value::Null);
                        self.edge(parent, &id, "defines", line, Some("field"), Value::Null);
                    }
                }
            }
            return true;
        }
        false
    }
    pub(crate) fn kotlin_bases(&self, n: Node<'t>) -> Vec<(String, &'static str, Node<'t>)> {
        let mut out = vec![];
        if let Some(specs) = first_child(n, &["delegation_specifiers"]) {
            for spec in children(specs)
                .into_iter()
                .filter(|c| c.kind_name() == "delegation_specifier")
            {
                let mut rel = "implements";
                let mut ty = None;
                for sub in children(spec) {
                    match sub.kind_name() {
                        "constructor_invocation" => {
                            rel = "inherits";
                            ty = first_child(sub, &["user_type"]);
                            break;
                        }
                        "user_type" => {
                            ty = Some(sub);
                            break;
                        }
                        "explicit_delegation" => {
                            ty = first_child(sub, &["user_type"]);
                            break;
                        }
                        _ => {}
                    }
                }
                if let Some(ty) = ty
                    && let Some(name) = kotlin_user_type_name(Some(ty), self.source)
                {
                    out.push((name, rel, ty));
                }
            }
        }
        out
    }
    pub(crate) fn kotlin_literals(&mut self, n: Node<'t>, owner: &str) {
        let mut stack = children(n);
        let mut literals = vec![];
        while let Some(n) = stack.pop() {
            if n.kind_name() == "function_declaration" {
                continue;
            }
            if n.kind_name() == "object_literal" {
                literals.push(n);
                continue;
            }
            stack.extend(children(n));
        }
        literals.sort_by_key(Node::start_byte);
        for n in literals {
            let line = n.start_position().row + 1;
            let bases = self.kotlin_bases(n);
            let name = bases
                .first()
                .map(|(s, _, _)| s.clone())
                .unwrap_or_else(|| format!("object@L{line}"));
            let id = make_id([
                owner,
                format!("object:{name}").as_str(),
                format!("L{line}").as_str(),
            ]);
            self.add_node(&id, &name, line, None, Value::Null);
            self.edge(owner, &id, "contains", line, None, Value::Null);
            self.callable.insert(id.clone());
            self.classes.insert(id.clone());
            for (base, rel, _) in bases {
                let target = self.ensure(&base, true);
                if target != id {
                    self.edge(&id, &target, rel, line, None, Value::Null);
                }
            }
            if let Some(b) = first_child(n, &["class_body"]) {
                for c in children(b) {
                    self.walk(c, Some(&id));
                }
            }
        }
    }
    pub(crate) fn ts_decorators(&mut self, n: Node<'t>, id: &str) {
        let mut work = vec![];
        for d in children(n)
            .into_iter()
            .filter(|c| c.kind_name() == "decorator")
        {
            work.push((d, id.to_string()));
        }
        if let Some(p) = n.parent().filter(|p| p.kind_name() == "export_statement") {
            for d in children(p) {
                if d.kind_name() == "decorator" {
                    work.push((d, id.into()));
                } else if ["class_declaration", "abstract_class_declaration"]
                    .contains(&d.kind_name())
                {
                    break;
                }
            }
        }
        if let Some(body) = first_child(n, &["class_body"]) {
            for member in children(body) {
                if member.kind_name() == "decorator" {
                    let mut next = member.next_named_sibling();
                    while let Some(n) = next.filter(|n| n.kind_name() == "decorator") {
                        next = n.next_named_sibling();
                    }
                    let owner = next
                        .filter(|n| n.kind_name() == "method_definition")
                        .and_then(|n| self.name(n))
                        .map(|name| make_id([id, name.as_str()]))
                        .unwrap_or_else(|| id.into());
                    work.push((member, owner));
                } else {
                    let owner = if member.kind_name() == "method_definition" {
                        self.name(member)
                            .map(|name| make_id([id, name.as_str()]))
                            .unwrap_or_else(|| id.into())
                    } else {
                        id.into()
                    };
                    fn scan<'t>(n: Node<'t>, out: &mut Vec<Node<'t>>, top: bool) {
                        for c in children(n) {
                            if c.kind_name() == "decorator" {
                                out.push(c);
                            } else if ["class_declaration", "abstract_class_declaration"]
                                .contains(&c.kind_name())
                                || (c.kind_name() == "method_definition" && !top)
                            {
                                continue;
                            } else {
                                scan(c, out, false);
                            }
                        }
                    }
                    let mut ds = vec![];
                    scan(member, &mut ds, true);
                    for d in ds {
                        work.push((d, owner.clone()));
                    }
                }
            }
        }
        for (d, owner) in work {
            if let Some(name) = decorator_name(d, self.source) {
                let target = self.ensure(&name, true);
                if target != owner {
                    self.edge(
                        &owner,
                        &target,
                        "references",
                        d.start_position().row + 1,
                        Some("decorator"),
                        Value::Null,
                    );
                }
            }
        }
    }
    pub(crate) fn php_listeners(&mut self, n: Node<'t>) -> bool {
        let mut handled = false;
        for elem in children(n)
            .into_iter()
            .filter(|c| c.kind_name() == "property_element")
        {
            let prop = first_child(elem, &["variable_name"])
                .and_then(|n| first_child(n, &["name"]))
                .map(|n| self.text(n));
            let Some(prop) = prop.filter(|p| self.config.event_listener_properties.contains(p))
            else {
                continue;
            };
            let _ = prop;
            let Some(arr) = first_child(elem, &["array_creation_expression"]) else {
                continue;
            };
            handled = true;
            for entry in children(arr)
                .into_iter()
                .filter(|c| c.kind_name() == "array_element_initializer")
            {
                let event = first_child(entry, &["class_constant_access_expression"])
                    .and_then(|n| first_child(n, &["name", "qualified_name"]));
                let listeners = first_child(entry, &["array_creation_expression"]);
                if let (Some(event), Some(listeners)) = (event, listeners) {
                    for l in children(listeners)
                        .into_iter()
                        .filter(|c| c.kind_name() == "array_element_initializer")
                    {
                        if let Some(expr) = first_child(l, &["class_constant_access_expression"])
                            && let Some(name) = first_child(expr, &["name", "qualified_name"])
                        {
                            self.listeners.push((
                                self.text(event),
                                self.text(name),
                                expr.start_position().row + 1,
                            ));
                        }
                    }
                }
            }
        }
        handled
    }
}

fn scala_base_name(n: Node<'_>, s: &[u8]) -> Option<String> {
    match n.kind_name() {
        "type_identifier" => Some(super::base::read_text(n, s)),
        "stable_type_identifier" => children(n)
            .into_iter()
            .rev()
            .find(|n| matches!(n.kind_name(), "type_identifier" | "identifier"))
            .map(|n| super::base::read_text(n, s)),
        "generic_type" => n
            .child_by_field_name("type")
            .or_else(|| first_child(n, &["type_identifier", "stable_type_identifier"]))
            .and_then(|n| scala_base_name(n, s)),
        _ => None,
    }
}
