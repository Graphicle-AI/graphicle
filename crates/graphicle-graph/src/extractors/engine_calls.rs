// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
//! Call traversal, scoped receivers, indirect dispatch and final graph assembly.
use super::{
    base::{file_stem, is_language_builtin, make_id},
    engine::{Engine, Table},
    engine_ruby,
    engine_scope::*,
    engine_types::*,
    resolution_js as js,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tree_sitter::Node;

// PHP language constructs written like functions. `empty($x)`, `isset($a)`,
// `eval($s)` and `die($m)` parse as an ordinary function_call_expression, but a
// reserved word can only ever be a method name (`public function empty()`), so
// binding the bare call by name tied every `empty($x)` to that method (#3830).
// Bare calls only: `$bag->empty()` is a member call and still resolves.
const PHP_LANGUAGE_CONSTRUCTS: [&str; 8] = [
    "array", "die", "empty", "eval", "exit", "isset", "list", "unset",
];

#[derive(Default)]
pub(crate) struct Receivers {
    java: Table,
    csharp: Option<CsharpReceiverTable>,
    ruby: BTreeMap<String, Option<String>>,
    ruby_locals: BTreeSet<String>,
}
#[derive(Default)]
struct Call {
    name: Option<String>,
    member: bool,
    receiver: Option<String>,
    this_field: bool,
    qualified: Option<String>,
    kotlin_object: Option<String>,
    /// `self`/`this`/`super` receiver of a self-call (0.9.75).
    self_receiver: Option<&'static str>,
    /// PHP `Helper::format()`: a member call on the scope, always deferred
    /// to the corpus PHP resolver (0.9.87, #3872).
    scoped: bool,
    /// C# `x.F.M()` / `xs[i].M()`: the receiver's type, then the member read
    /// on it (0.9.83, #4246).
    csharp_chain: Option<Vec<String>>,
    /// Java: a receiver that is itself an expression (0.9.88, #4304).
    java_chain: Option<Value>,
}
fn upper(s: &str) -> bool {
    s.chars().next().is_some_and(char::is_uppercase)
}
fn python_values(n: Node<'_>) -> Vec<Node<'_>> {
    if n.kind_name() == "dictionary" {
        children(n)
            .into_iter()
            .filter(|c| c.kind_name() == "pair")
            .filter_map(|p| p.child_by_field_name("value"))
            .filter(|v| v.kind_name() == "identifier")
            .collect()
    } else {
        children(n)
            .into_iter()
            .filter(|c| c.kind_name() == "identifier")
            .collect()
    }
}
fn ref_values(n: Option<Node<'_>>) -> Vec<Node<'_>> {
    match n {
        Some(n) if n.kind_name() == "identifier" => vec![n],
        Some(n) if n.kind_name() == "expression_list" => children(n)
            .into_iter()
            .filter(|c| c.kind_name() == "identifier")
            .collect(),
        _ => vec![],
    }
}
/// `_lexical_class_bases` (0.9.83, #4252): the supertypes of each class, for
/// [`Engine::lexical_call_target`]. Targets named in `interfaces` (C#: those
/// the file declares) are left out. A base declared further down the file is
/// still a sourceless stub during the call walk, so it is read as the one
/// class of this file that bears its name; an anonymous class (a class inside
/// a method) is labelled with the type after its `new`, read the same way.
#[allow(clippy::too_many_arguments)]
fn lexical_class_bases(
    edges: &[Value],
    relations: &[&str],
    label_by_id: &BTreeMap<&str, &str>,
    nid_to_sf: &BTreeMap<&str, &str>,
    method_owner: &Table,
    methods_by_owner: &BTreeMap<(String, String), String>,
    class_parent: &Table,
    interfaces: &BTreeSet<String>,
) -> BTreeMap<String, Vec<String>> {
    let supers: Vec<(&str, &str)> = edges
        .iter()
        .filter(|e| relations.contains(&e["relation"].as_str().unwrap_or("")))
        .filter_map(|e| Some((e["source"].as_str()?, e["target"].as_str()?)))
        .filter(|(_, target)| {
            !label_by_id
                .get(target)
                .is_some_and(|label| interfaces.contains(*label))
        })
        .collect();
    let mut by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut listed = BTreeSet::new();
    for id in methods_by_owner
        .keys()
        .map(|(owner, _)| owner.as_str())
        .chain(supers.iter().map(|(source, _)| *source))
    {
        // An anonymous class is labelled with its base, not its own name.
        if !listed.insert(id)
            || class_parent
                .get(id)
                .is_some_and(|outer| method_owner.contains_key(outer))
        {
            continue;
        }
        by_name
            .entry(label_by_id.get(id).copied().unwrap_or(""))
            .or_default()
            .push(id);
    }
    let unique = |label: &str| {
        by_name
            .get(label)
            .filter(|named| named.len() == 1)
            .map(|named| named[0].to_string())
    };
    let mut bases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (source, target) in &supers {
        let mut target = target.to_string();
        if nid_to_sf
            .get(target.as_str())
            .is_none_or(|sf| sf.is_empty())
            && let Some(local) = unique(label_by_id.get(target.as_str()).copied().unwrap_or(""))
        {
            target = local;
        }
        bases.entry(source.to_string()).or_default().push(target);
    }
    for (id, outer) in class_parent {
        if method_owner.contains_key(outer) {
            let label = label_by_id.get(id.as_str()).copied().unwrap_or("");
            let base = label
                .split('<')
                .next()
                .unwrap_or("")
                .trim_matches(crate::dedup_strings::whitespace);
            if let Some(local) = unique(base) {
                bases.entry(id.clone()).or_default().push(local);
            }
        }
    }
    bases
}
impl<'a, 't> Engine<'a, 't> {
    /// `_LEXICAL_CALL_LANGUAGES` (0.9.83, #4252) with the relations that
    /// bring a supertype's methods into a class: a Java interface's and a
    /// Scala trait's are inherited, a C# interface's are not.
    pub(crate) fn lexical_relations(&self) -> Option<&'static [&'static str]> {
        match self.config.ts_module.as_str() {
            "tree_sitter_java" | "tree_sitter_c_sharp" => Some(&["inherits", "implements"]),
            "tree_sitter_scala" => Some(&["inherits", "mixes_in"]),
            "tree_sitter_cpp" => Some(&["inherits"]),
            _ => None,
        }
    }
    pub(crate) fn lexical_language(&self) -> bool {
        self.lexical_relations().is_some()
    }
    /// `_lexical_call_target` (0.9.83, #4252): the in-file target of an
    /// unqualified `m()`. The nearest class that has an `m`: the caller's
    /// own class and its in-file bases (nearest first), then the class around
    /// it, outward; an anonymous class steps out through the method holding
    /// it. Two hits on one level tie and bind nothing; a name no class on the
    /// path declares keeps the file-wide lookup.
    fn lexical_call_target(&self, caller: &str, callee: &str) -> Option<String> {
        let mut class = Some(
            self.method_owner
                .get(caller)
                .cloned()
                .unwrap_or_else(|| caller.to_string()),
        );
        let mut visited = BTreeSet::new();
        while let Some(current) = class.filter(|c| !c.is_empty() && !visited.contains(c)) {
            visited.insert(current.clone());
            let mut level = vec![current.clone()];
            let mut seen = BTreeSet::new();
            while !level.is_empty() {
                seen.extend(level.iter().cloned());
                let hits: BTreeSet<&String> = level
                    .iter()
                    .filter_map(|c| self.methods_by_owner.get(&(c.clone(), callee.to_string())))
                    .collect();
                if !hits.is_empty() {
                    return if hits.len() == 1 {
                        hits.into_iter().next().cloned()
                    } else {
                        None
                    };
                }
                let mut next: Vec<String> = Vec::new();
                for c in &level {
                    for base in self.lexical_bases.get(c).into_iter().flatten() {
                        if !seen.contains(base) && !next.contains(base) {
                            next.push(base.clone());
                        }
                    }
                }
                level = next;
            }
            class = self
                .class_parent
                .get(&current)
                .filter(|outer| !outer.is_empty())
                .map(|outer| self.method_owner.get(outer).unwrap_or(outer).clone());
        }
        self.labels.get(callee).cloned()
    }
    /// Python member-call stamps for the corpus resolvers: `self.X.m()`'s
    /// class (0.9.80, #2860), a typed receiver (#4198) and
    /// `super(Cls, obj).m()`, whose MRO starts after `Cls` (0.9.81).
    fn python_member_stamps(&self, n: Node<'t>, caller: &str, call: &Call, raw: &mut Value) {
        let object = n
            .child_by_field_name("function")
            .and_then(|f| f.child_by_field_name("object"));
        if let Some(attr) = python_self_attr(object, self.source)
            && let Some(Some(class)) = self
                .method_owner
                .get(caller)
                .and_then(|owner| self.python_attr_types.get(owner))
                .and_then(|fields| fields.get(&attr))
        {
            raw["_python_self_attr_type"] = json!(class);
        }
        if let Some(receiver) = call.receiver.as_deref().filter(|r| !r.is_empty()) {
            if let Some(Some(class)) = self
                .python_var_types
                .get(caller)
                .and_then(|table| table.get(receiver))
            {
                raw["receiver_type"] = json!(class);
            } else if self
                .python_fixture_params
                .get(caller)
                .is_some_and(|params| params.contains(receiver))
            {
                // A fixture parameter: typed by the resolver from the fixture
                // pytest would pick for it (0.9.92, #4332).
                raw["_python_fixture_param"] = json!(true);
            }
        }
        if call.receiver.as_deref() == Some("super")
            && object
                .and_then(|o| o.child_by_field_name("arguments"))
                .is_some_and(|args| !named(args).is_empty())
        {
            raw["_python_super_args"] = json!(true);
        }
    }
    /// `class` and its in-file ancestors through `inherits` edges, nearest
    /// first (`_fields_up_chain`'s walk).
    fn up_chain(&self, class: &str) -> Vec<String> {
        let mut order = vec![];
        let mut queue = VecDeque::from([class.to_string()]);
        let mut seen = BTreeSet::new();
        while let Some(cls) = queue.pop_front() {
            if cls.is_empty() || !seen.insert(cls.clone()) {
                continue;
            }
            queue.extend(
                self.edges
                    .iter()
                    .filter(|e| e["relation"] == "inherits" && e["source"] == cls.as_str())
                    .filter_map(|e| e["target"].as_str().map(str::to_string)),
            );
            order.push(cls);
        }
        order
    }
    pub(crate) fn scan_external(&mut self, n: Node<'t>) {
        for c in children(n) {
            if c.kind_name() == "import_statement" {
                if let Some(src) = c.child_by_field_name("source") {
                    let raw = self.text(src);
                    let raw = raw.trim_matches(['\'', '"', '`']);
                    let external =
                        js::resolve_js_import_target(raw, &self.path).is_some_and(|(_, p)| {
                            p.is_none_or(|p| {
                                p.components().any(|c| c.as_os_str() == "node_modules")
                            })
                        });
                    if external && let Some(clause) = first_child(c, &["import_clause"]) {
                        for c in children(clause) {
                            if c.kind_name() == "identifier" {
                                self.external.insert(self.text(c));
                            } else if c.kind_name() == "namespace_import" {
                                for c in children(c)
                                    .into_iter()
                                    .filter(|c| c.kind_name() == "identifier")
                                {
                                    self.external.insert(self.text(c));
                                }
                            } else if c.kind_name() == "named_imports" {
                                for spec in children(c)
                                    .into_iter()
                                    .filter(|c| c.kind_name() == "import_specifier")
                                {
                                    if let Some(ident) = children(spec)
                                        .into_iter()
                                        .rfind(|c| c.kind_name() == "identifier")
                                    {
                                        self.external.insert(self.text(ident));
                                    }
                                }
                            }
                        }
                    }
                }
                continue;
            }
            self.scan_external(c);
        }
    }
    pub(crate) fn require_js(&mut self, n: Node<'t>, caller: &str) -> bool {
        if !matches!(
            n.kind_name(),
            "lexical_declaration" | "variable_declaration"
        ) {
            return false;
        }
        let mut found = false;
        for d in children(n)
            .into_iter()
            .filter(|c| c.kind_name() == "variable_declarator")
        {
            let value = d.child_by_field_name("value");
            let Some(call) = find_require_call(value) else {
                continue;
            };
            if call
                .child_by_field_name("function")
                .is_none_or(|c| self.text(c) != "require")
            {
                continue;
            }
            let Some(arg) = call
                .child_by_field_name("arguments")
                .and_then(|a| first_child(a, &["string"]))
            else {
                continue;
            };
            let raw = self.text(arg);
            let raw = raw.trim_matches(['\'', '"', '`', ' ']);
            let Some((mut target, mut path)) = js::resolve_js_import_target(raw, &self.path) else {
                continue;
            };
            // A relative specifier with no file behind it resolves to the
            // attempted absolute path; minting an id from that bakes the
            // checkout location into the graph (0.9.78, #4155).
            if path.as_ref().is_some_and(|p| !p.is_file()) {
                target = make_id(["ref", raw]);
                path = None;
            }
            let line = n.start_position().row + 1;
            self.edge(
                caller,
                &target,
                "imports_from",
                line,
                Some("import"),
                Value::Null,
            );
            if let Some(path) = &path {
                self.edges.last_mut().unwrap()["target_file"] = json!(path);
            }
            found = true;
            let mut names = vec![];
            if let Some(name) = d
                .child_by_field_name("name")
                .filter(|n| n.kind_name() == "object_pattern")
            {
                for prop in children(name) {
                    if prop.kind_name() == "shorthand_property_identifier_pattern" {
                        names.push(self.text(prop));
                    } else if prop.kind_name() == "pair_pattern"
                        && let Some(k) = prop.child_by_field_name("key")
                    {
                        names.push(self.text(k));
                    }
                }
            } else if let Some(v) = value.filter(|v| v.kind_name() == "member_expression")
                && let Some(p) = v.child_by_field_name("property")
            {
                names.push(self.text(p));
            }
            if let Some(path) = path {
                let stem = file_stem(&path);
                for name in names {
                    self.edge(
                        caller,
                        &make_id([stem.as_str(), name.as_str()]),
                        "imports",
                        line,
                        Some("import"),
                        Value::Null,
                    );
                }
            }
        }
        found
    }
    fn dynamic_import(&mut self, n: Node<'t>, caller: &str) -> bool {
        let function = n
            .child_by_field_name("function")
            .or_else(|| n.child(0).filter(|c| self.text(*c) == "import"));
        if function.is_none_or(|f| self.text(f) != "import") {
            return false;
        }
        if let Some(args) = n.child_by_field_name("arguments") {
            for a in children(args) {
                let raw = match a.kind_name() {
                    "template_string" => {
                        if first_child(a, &["template_substitution"]).is_some() {
                            break;
                        }
                        self.text(a).trim_matches('`').to_string()
                    }
                    "string" => self.text(a).trim_matches(['\'', '"', ' ']).into(),
                    _ => continue,
                };
                if let Some((target, path)) = js::resolve_js_import_target(&raw, &self.path)
                    && self.seen_dynamic.insert((caller.into(), target.clone()))
                {
                    self.edge(
                        caller,
                        &target,
                        "imports_from",
                        n.start_position().row + 1,
                        Some("import"),
                        Value::Null,
                    );
                    let e = self.edges.last_mut().unwrap();
                    e["deferred"] = json!(true);
                    if let Some(p) = path {
                        e["target_file"] = json!(p);
                    }
                }
                break;
            }
        }
        true
    }
    fn call_name(&self, n: Node<'t>, receivers: &Receivers) -> Call {
        let mut c = Call::default();
        let text = |n: Node<'_>| self.text(n);
        if self.is("swift") && n.kind_name() == "macro_invocation" {
            // A freestanding macro call `#stringify(...)` (0.9.93, #4551) names a
            // free top-level macro, never a method on `self`: it resolves by bare
            // name, and an unknown `#name` stays unresolved.
            c.name = first_child(n, &["simple_identifier"]).map(text);
            return c;
        }
        if self.is("swift") || self.is("kotlin") || self.is("scala") {
            if let Some(first) = n.child(0) {
                if ["identifier", "simple_identifier"].contains(&first.kind_name()) {
                    c.name = Some(text(first));
                    if self.is("swift") && first.kind_name() == "simple_identifier" {
                        // Inside a method a bare `save()` is `self.save()`.
                        c.self_receiver = Some("self");
                    }
                } else if matches!(
                    first.kind_name(),
                    "navigation_expression" | "field_expression"
                ) {
                    c.member = true;
                    c.name = first.child_by_field_name("field").map(text).or_else(|| {
                        children(first).into_iter().rev().find_map(|c| {
                            if ["simple_identifier", "identifier"].contains(&c.kind_name()) {
                                Some(text(c))
                            } else if c.kind_name() == "navigation_suffix" {
                                first_child(c, &["simple_identifier"]).map(text)
                            } else {
                                None
                            }
                        })
                    });
                    if self.is("swift") {
                        c.receiver = swift_receiver_name(first.child(0), self.source);
                        c.self_receiver = match first.child(0).map(|r| r.kind_name()) {
                            Some("self_expression") => Some("self"),
                            Some("super_expression") => Some("super"),
                            _ => None,
                        };
                    } else if self.is("kotlin") {
                        let raw = text(first);
                        let seg: Vec<_> = raw.split('.').collect();
                        if seg.iter().all(|s| {
                            !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
                        }) {
                            if seg.len() >= 3 {
                                c.qualified = Some(seg[..seg.len() - 1].join("."));
                            } else if seg.len() == 2 && upper(seg[0]) {
                                c.kotlin_object = Some(seg[0].into());
                            }
                        }
                    }
                }
            }
            return c;
        }
        if self.is("php") {
            match n.kind_name() {
                "object_creation_expression" => {
                    c.name = first_child(n, &["name", "qualified_name"])
                        .map(text)
                        .map(|s| s.rsplit('\\').next().unwrap_or("").to_string())
                        .filter(|s| {
                            !["self", "static", "parent"].contains(&s.to_lowercase().as_str())
                        });
                }
                "function_call_expression" => {
                    // PHP names are case-insensitive, so `EMPTY($x)` is the same
                    // construct; dropping the name skips the in-file bind and
                    // keeps it out of raw_calls.
                    c.name = n
                        .child_by_field_name("function")
                        .map(text)
                        .filter(|s| !PHP_LANGUAGE_CONSTRUCTS.contains(&s.to_lowercase().as_str()));
                }
                "scoped_call_expression" => {
                    // `Helper::format()` calls `format` on `Helper`, not
                    // `Helper` (0.9.87, #3872).
                    c.member = true;
                    c.scoped = true;
                    c.name = n.child_by_field_name("name").map(text);
                    c.receiver = n.child_by_field_name("scope").map(text);
                }
                _ => {
                    c.member = true;
                    c.name = n.child_by_field_name("name").map(text);
                    // A builtin-named method on a known `$this` receiver
                    // resolves against its owning class (0.9.77).
                    if n.child_by_field_name("object")
                        .is_some_and(|r| text(r) == "$this")
                        && c.name.as_deref().is_some_and(|s| {
                            !s.is_empty() && is_language_builtin(&crate::dedup_strings::casefold(s))
                        })
                    {
                        c.self_receiver = Some("this");
                    }
                    // `$this->greeter->greet()` types the receiver by the
                    // property name, `$greeter->greet()` by the variable; a
                    // longer chain names neither (0.9.89).
                    let object = n.child_by_field_name("object");
                    match object.map(|o| o.kind_name()) {
                        Some("variable_name") => {
                            c.receiver = php_variable_text(object, self.source);
                        }
                        Some("member_access_expression") => {
                            let o = object.unwrap();
                            if php_variable_text(o.child_by_field_name("object"), self.source)
                                .as_deref()
                                == Some("this")
                            {
                                c.receiver = o.child_by_field_name("name").map(text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            return c;
        }
        if self.is("c_sharp") {
            if n.kind_name() == "object_creation_expression" {
                if let Some((name, q, qual)) =
                    read_csharp_type_name(n.child_by_field_name("type"), self.source)
                {
                    c.name = Some(name);
                    if q && !qual.is_empty() {
                        c.qualified = Some(qual);
                    }
                }
            } else if let Some(f) = n.child_by_field_name("function") {
                match f.kind_name() {
                    _ if csharp_member_call_parts(f).is_some() => {
                        let (name, receiver) = csharp_member_call_parts(f).unwrap();
                        c.member = true;
                        c.name = name.map(|m| csharp_bare_call_name(m, self.source));
                        if let Some(r) = receiver {
                            if matches!(
                                r.kind_name(),
                                "identifier"
                                    | "this"
                                    | "this_expression"
                                    | "base"
                                    | "base_expression"
                            ) {
                                c.receiver = Some(match r.kind_name() {
                                    "this_expression" => "this".into(),
                                    "base_expression" => "base".into(),
                                    _ => text(r),
                                });
                            } else if r.kind_name() == "member_access_expression" {
                                let inner = r.child_by_field_name("expression");
                                let field = r
                                    .child_by_field_name("name")
                                    .filter(|n| n.kind_name() == "identifier");
                                match (inner, field) {
                                    (Some(inner), Some(field))
                                        if matches!(
                                            inner.kind_name(),
                                            "this" | "this_expression"
                                        ) =>
                                    {
                                        c.receiver = Some(text(field));
                                    }
                                    // x.F.M(): x's declared type, then its member F,
                                    // resolved cross-file (0.9.83, #4246).
                                    (Some(inner), Some(field))
                                        if inner.kind_name() == "identifier" =>
                                    {
                                        if let Some(CsharpBinding::Type(root)) =
                                            csharp_scoped_receiver_type(
                                                receivers.csharp.as_ref(),
                                                Some(&text(inner)),
                                                n.start_byte() as i64,
                                            )
                                        {
                                            c.csharp_chain = Some(vec![root, text(field)]);
                                        }
                                    }
                                    _ => {}
                                }
                            } else if r.kind_name() == "element_access_expression" {
                                // xs[i].M() on a `T[]` / `List<T>`: an element is a T.
                                if let Some(inner) = r
                                    .child_by_field_name("expression")
                                    .filter(|i| i.kind_name() == "identifier")
                                    && let Some(CsharpBinding::Type(elem)) =
                                        csharp_scoped_receiver_type(
                                            receivers.csharp.as_ref(),
                                            Some(&format!("{}[]", text(inner))),
                                            n.start_byte() as i64,
                                        )
                                {
                                    c.csharp_chain = Some(vec![elem]);
                                }
                            } else if r.kind_name() == "object_creation_expression" {
                                // new X(...).M(): the receiver is the
                                // constructed X (#1770), typed here now that a
                                // nameless receiver defers (0.9.90).
                                if let Some((name, _, _)) = read_csharp_type_name(
                                    r.child_by_field_name("type"),
                                    self.source,
                                )
                                .filter(|(name, _, _)| !name.is_empty())
                                {
                                    c.csharp_chain = Some(vec![name]);
                                }
                            } else if r.kind_name() == "parenthesized_expression" {
                                // ((IStore)x).M() / (x as IStore).M(): the cast
                                // names the receiver's type (0.9.78, #3797).
                                let inner = named(r).into_iter().next();
                                let cast = inner.and_then(|i| match i.kind_name() {
                                    "cast_expression" => i.child_by_field_name("type"),
                                    "as_expression" => i.child_by_field_name("right"),
                                    _ => None,
                                });
                                if let Some((name, _, _)) = read_csharp_type_name(cast, self.source)
                                    .filter(|(name, _, _)| !name.is_empty())
                                {
                                    c.receiver = Some(name);
                                }
                            }
                        }
                    }
                    "identifier" => c.name = Some(text(f)),
                    "generic_name" => c.name = Some(csharp_bare_call_name(f, self.source)),
                    _ => {
                        c.name = n
                            .child_by_field_name("name")
                            .or_else(|| named(n).into_iter().next())
                            .map(text);
                        if let Some(raw) = c.name.clone()
                            && raw.contains('.')
                        {
                            c.member = true;
                            c.name = raw.rsplit('.').next().map(str::to_string);
                            if raw.matches('.').count() == 1 {
                                c.receiver = raw.split('.').next().map(str::to_string);
                            }
                        }
                    }
                }
            }
            return c;
        }
        if self.is("java") {
            if n.kind_name() == "object_creation_expression" {
                c.name = n.child_by_field_name("type").map(text).map(|s| {
                    s.split('<')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .rsplit('.')
                        .next()
                        .unwrap_or("")
                        .to_string()
                });
            } else {
                c.name = n.child_by_field_name("name").map(text);
                if let Some(r) = n.child_by_field_name("object") {
                    c.member = true;
                    // `super.m()` records its receiver so resolution can bind
                    // it to the unique superclass (0.9.73).
                    if matches!(r.kind_name(), "identifier" | "this" | "super") {
                        c.receiver = Some(text(r));
                    } else if r.kind_name() == "field_access" {
                        if let (Some(owner), Some(field)) = (
                            r.child_by_field_name("object"),
                            r.child_by_field_name("field"),
                        ) && owner.kind_name() == "this"
                        {
                            c.receiver = Some(format!("this.{}", text(field)));
                            c.this_field = true;
                        }
                    } else {
                        // A call, `new` or cast receiver, typed by the resolver
                        // from declared return types (0.9.88, #4304).
                        c.java_chain = java_receiver_chain(Some(r), self.source, 0);
                    }
                }
            }
            return c;
        }
        if self.is("ruby") {
            c.name = n.child_by_field_name("method").map(text);
            let recv = n.child_by_field_name("receiver");
            if recv.is_none_or(|r| r.kind_name() == "self") {
                // `save(x)` and `self.save` both send to the current object.
                c.self_receiver = Some("self");
            }
            if let Some(r) = recv {
                c.member = true;
                if matches!(
                    r.kind_name(),
                    "identifier" | "constant" | "scope_resolution"
                ) {
                    c.receiver = Some(text(r).trim().into());
                }
            }
            return c;
        }
        let f = if matches!(
            n.kind_name(),
            "jsx_opening_element" | "jsx_self_closing_element"
        ) {
            // JSX's transform invokes a bare component tag. Lowercase tags are
            // DOM intrinsics; member/namespaced tags are conservatively skipped
            // because reducing them to a property name can bind an unrelated
            // local function (#3854).
            n.child_by_field_name("name").filter(|name| {
                name.kind_name() == "identifier"
                    && !text(*name).chars().next().is_some_and(char::is_lowercase)
            })
        } else {
            n.child_by_field_name(&self.config.call_function_field)
                .or_else(|| {
                    if n.kind_name() == "new_expression" {
                        n.child_by_field_name("constructor")
                    } else {
                        None
                    }
                })
        };
        if let Some(f) = f {
            if f.kind_name() == "identifier" {
                c.name = Some(text(f));
            } else if self.config.call_accessor_node_types.contains(f.kind_name()) {
                c.member = true;
                let field = if self.is("cpp") && f.kind_name() == "qualified_identifier" {
                    "name"
                } else {
                    &self.config.call_accessor_field
                };
                c.name = f.child_by_field_name(field).map(text);
                let object = if self.is("cpp") {
                    if f.kind_name() == "qualified_identifier" {
                        "scope"
                    } else {
                        "argument"
                    }
                } else {
                    &self.config.call_accessor_object_field
                };
                if let Some(obj) = f.child_by_field_name(object) {
                    if obj.kind_name() == "identifier"
                        || (self.is("cpp")
                            && (matches!(obj.kind_name(), "this" | "namespace_identifier")
                                || f.kind_name() == "qualified_identifier"))
                    {
                        c.receiver = Some(text(obj));
                        // `const { creator } = this; creator.m()` reads the
                        // field as `this.creator.m()` does (0.9.91).
                        if self.js()
                            && obj.kind_name() == "identifier"
                            && let Some(field) =
                                ts_field_destructured_from_this(obj, &text(obj), self.source)
                        {
                            c.receiver = Some(field);
                            c.this_field = true;
                        }
                    } else if matches!(obj.kind_name(), "this" | "super")
                        && self.self_call_language()
                    {
                        c.self_receiver = Some(if obj.kind_name() == "this" {
                            "this"
                        } else {
                            "super"
                        });
                    } else if self.is("python")
                        && obj.kind_name() == "call"
                        && obj
                            .child_by_field_name("function")
                            .is_some_and(|f| f.kind_name() == "identifier" && text(f) == "super")
                    {
                        c.receiver = Some("super".into());
                    } else if self
                        .config
                        .call_accessor_node_types
                        .contains(obj.kind_name())
                        && obj
                            .child_by_field_name(object)
                            .is_some_and(|o| o.kind_name() == "this")
                    {
                        c.receiver = obj
                            .child_by_field_name(&self.config.call_accessor_field)
                            .map(text);
                        c.this_field = true;
                    }
                }
            } else {
                c.name = Some(text(f));
            }
        }
        c
    }
    fn indirect_name(&mut self, name: &str, n: Node<'t>, caller: &str, context: &str) {
        let target = self.labels.get(name).cloned();
        let external = target.as_ref().is_none_or(|id| {
            !self.callable.contains(id)
                && !self
                    .nodes
                    .iter()
                    .any(|n| n["id"] == *id && n["source_file"] == self.path)
        });
        if external {
            self.raw.push(json!({"caller_nid":caller,"callee":name,"is_member_call":false,"indirect":true,"context":context,"source_file":self.path,"source_location":format!("L{}",n.start_position().row+1)}));
            return;
        }
        let target = target.unwrap();
        if target == caller
            || !self.callable.contains(&target)
            || self.classes.contains(&target)
            || self.seen_calls.contains(&(caller.into(), target.clone()))
            || !self.seen_indirect.insert((caller.into(), target.clone()))
        {
            return;
        }
        self.edge(
            caller,
            &target,
            "indirect_call",
            n.start_position().row + 1,
            Some(context),
            Value::Null,
        );
        let e = self.edges.last_mut().unwrap();
        e["confidence"] = json!("INFERRED");
        e["confidence_score"] = json!(0.85);
    }
    fn indirect(
        &mut self,
        n: Option<Node<'t>>,
        caller: &str,
        locals: &BTreeSet<String>,
        context: &str,
    ) {
        if let Some(n) = n.filter(|n| {
            matches!(
                n.kind_name(),
                "identifier" | "shorthand_property_identifier"
            )
        }) {
            let name = self.text(n);
            if !locals.contains(&name)
                && !["self", "cls"].contains(&name.as_str())
                && !self.external.contains(&name)
            {
                self.indirect_name(&name, n, caller, context);
            }
        }
    }
    fn getattr(&mut self, n: Node<'t>, caller: &str) {
        if !n
            .child_by_field_name("function")
            .is_some_and(|f| f.kind_name() == "identifier" && self.text(f) == "getattr")
        {
            return;
        }
        if let Some(args) = n.child_by_field_name("arguments") {
            let pos = named(args)
                .into_iter()
                .filter(|a| !matches!(a.kind_name(), "keyword_argument" | "comment"))
                .collect::<Vec<_>>();
            if let Some(name) = pos.get(1).filter(|n| {
                n.kind_name() == "string" && first_child(**n, &["interpolation"]).is_none()
            }) && let Some(content) = first_child(*name, &["string_content"])
            {
                self.indirect_name(&self.text(content), *name, caller, "getattr");
            }
        }
    }
    fn walk_calls(
        &mut self,
        n: Node<'t>,
        caller: &str,
        receivers: &Receivers,
        extra: &BTreeSet<String>,
    ) {
        // Every syntax node of a body passes through here: read its kind and
        // the language once.
        let kind = n.kind_name();
        let python = self.is("python");
        let js = self.js();
        if self.config.function_boundary_types.contains(kind) {
            if js
                && [
                    "arrow_function",
                    "function_expression",
                    "function_declaration",
                    "generator_function_declaration",
                    "generator_function",
                ]
                .contains(&kind)
                && let Some(b) = n.child_by_field_name("body")
                && !self.bodies.iter().any(|(_, body)| body.id() == b.id())
            {
                let mut locals = extra.clone();
                locals.extend(js_local_bound_names(n, self.source));
                for c in children(n) {
                    self.walk_calls(c, caller, receivers, &locals);
                }
            }
            return;
        }
        if js {
            self.require_js(n, caller);
        }
        // Ruby paren-less self-send (0.9.74 #3960): a bare `identifier` is a
        // call on `self` unless a local of that name is in scope. The
        // `method`/`receiver` of a `call` node belong to the call branch.
        let ruby_bare_self_send = kind == "identifier"
            && self.is("ruby")
            && !n.parent().is_some_and(|p| {
                p.kind_name() == "call"
                    && [
                        p.child_by_field_name("method"),
                        p.child_by_field_name("receiver"),
                    ]
                    .contains(&Some(n))
            })
            && {
                let name = self.text(n);
                !name.is_empty() && !receivers.ruby_locals.contains(&name) && !extra.contains(&name)
            };
        let is_call = self.config.call_types.contains(kind) || ruby_bare_self_send;
        // Only calls, collection literals and Python assignments/returns read
        // the caller's locals; other nodes skip building the set.
        let locals = if is_call
            || (python
                && matches!(
                    kind,
                    "dictionary" | "list" | "set" | "tuple" | "assignment" | "return_statement"
                ))
            || (js && matches!(kind, "object" | "array"))
        {
            let mut locals = self.locals.get(caller).cloned().unwrap_or_default();
            locals.extend(extra.iter().cloned());
            locals
        } else {
            BTreeSet::new()
        };
        if is_call {
            if js && self.dynamic_import(n, caller) {
                for c in children(n) {
                    self.walk_calls(c, caller, receivers, extra);
                }
                return;
            }
            let call = if ruby_bare_self_send {
                Call {
                    name: Some(self.text(n)),
                    self_receiver: Some("self"),
                    ..Default::default()
                }
            } else {
                self.call_name(n, receivers)
            };
            if self.is("c_sharp")
                && let Some(f) = n.child_by_field_name("function")
            {
                let f = csharp_member_call_parts(f)
                    .map(|(name, _)| name.unwrap_or(f))
                    .unwrap_or(f);
                if f.kind_name() == "generic_name"
                    && let Some(args) = first_child(f, &["type_argument_list"])
                {
                    for arg in named(args) {
                        self.refs(
                            caller,
                            Some(arg),
                            n.start_position().row + 1,
                            "generic_arg",
                            true,
                        );
                    }
                }
            }
            let mut call = call;
            // Lua: `self:other()` in a method body calls a sibling on the same
            // table; only the literal `self` receiver is rewritten (#3991).
            let mut lua_self_qualified = false;
            if self.is("lua")
                && call.member
                && call.receiver.as_deref() == Some("self")
                && let Some(callee) = call.name.clone().filter(|s| !s.is_empty())
                && let Some(table) = self.lua_self_table.get(caller).cloned()
            {
                let colon = format!("{table}:{callee}");
                let dot = format!("{table}.{callee}");
                call.name = Some(
                    if !self.labels.contains_key(&colon) && self.labels.contains_key(&dot) {
                        dot
                    } else {
                        colon
                    },
                );
                call.receiver = None;
                lua_self_qualified = true;
            }
            if let Some(name) = call.name.as_deref().filter(|s| !s.is_empty()) {
                let builtin = is_language_builtin(name);
                let php_builtin_self_call = self.is("php") && call.self_receiver == Some("this");
                if !builtin || call.member {
                    // Upstream keeps a Swift call's receiver apart
                    // (`swift_receiver`): the raw call records it, but only
                    // the other languages' receivers defer a member call.
                    let receiver = if self.is("swift") {
                        None
                    } else {
                        call.receiver.as_deref()
                    };
                    let defer = (self.is("python")
                        && call.member
                        && !call
                            .receiver
                            .as_deref()
                            .is_some_and(|s| ["self", "cls", "super"].contains(&s)))
                        || (self.is("java") && call.member)
                        || (self.is("lua") && call.member && !lua_self_qualified)
                        || (call.member && builtin && !php_builtin_self_call)
                        || call.scoped
                        // A C# member call defers even without a capturable
                        // receiver name (`run.Sequence.Apply()`, `Make().M()`):
                        // the bare-name map would bind the caller's own
                        // same-named method (0.9.90).
                        || (self.is("c_sharp") && call.member)
                        || (call.member
                            && receiver.is_some_and(|r| {
                                upper(r) || call.this_field || self.is("c_sharp")
                            }));
                    // JS/TS (0.9.94, #4598): a member call on a receiver other
                    // than `this` defers to receiver-typed resolution instead
                    // of binding by bare name (a wrapper `appendFrame(e) {
                    // return history.appendFrame(e); }` bound to itself). The
                    // exception is the factory object the caller's own method
                    // hangs off (`store.get()` inside `store.set = ...`): the
                    // receiver rebuilds the caller's owner id exactly.
                    let js_factory_sibling = self.js()
                        && call.member
                        && call.receiver.as_deref().is_some_and(|r| {
                            !r.is_empty()
                                && self.method_owner.get(caller).is_some_and(|owner| {
                                    make_id([
                                        self.js_factory_scope.get(owner).map_or("", String::as_str),
                                        r,
                                    ]) == *owner
                                })
                        });
                    let defer = defer
                        || (self.js()
                            && call.member
                            && call.self_receiver.is_none()
                            && !js_factory_sibling);
                    let mut target = None;
                    if !defer {
                        if self.is("python") && !call.member {
                            let mut scope = Some(caller.to_string());
                            let mut seen = BTreeSet::new();
                            while let Some(s) = scope {
                                if !seen.insert(s.clone()) {
                                    break;
                                }
                                if let Some(id) = self.lexical.get(&s).and_then(|m| m.get(name)) {
                                    target = Some(id.clone());
                                    break;
                                }
                                scope = self.parents.get(&s).cloned();
                            }
                        }
                        let self_receiver = call.self_receiver.or_else(|| {
                            if self.is("python") && call.member {
                                match call.receiver.as_deref() {
                                    Some("self") => Some("self"),
                                    Some("cls") => Some("cls"),
                                    Some("super") => Some("super"),
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        });
                        if self.is("python") && !call.member {
                            if target.is_none() {
                                target = self.labels.get(name).cloned();
                            }
                        } else if self_receiver.is_none() && self.lexical_language() && !call.member
                        {
                            // Unqualified `m()` binds through the caller's class
                            // chain, then the enclosing classes (0.9.83, #4252).
                            target = self.lexical_call_target(caller, name);
                        } else if let Some(receiver) = self_receiver {
                            let callee = if php_builtin_self_call {
                                crate::dedup_strings::casefold(name)
                            } else {
                                name.to_string()
                            };
                            target = self.self_call_target(
                                caller,
                                &callee,
                                receiver,
                                !self.js(),
                                php_builtin_self_call,
                            );
                            if php_builtin_self_call
                                && target
                                    .as_ref()
                                    .is_some_and(|t| !self.method_owner.contains_key(t))
                            {
                                target = None;
                            }
                        } else if js_factory_sibling {
                            // Off the caller's own owner, never the file-wide map.
                            target = self.method_owner.get(caller).and_then(|owner| {
                                self.methods_by_owner
                                    .get(&(owner.clone(), name.to_string()))
                                    .cloned()
                            });
                        } else {
                            target = self.labels.get(name).cloned();
                        }
                        if self.is("c_sharp")
                            && call.qualified.is_some()
                            && target.as_ref().is_some_and(|id| {
                                !self.nodes.iter().any(|n| {
                                    n["id"] == *id
                                        && n["source_file"].as_str().is_some_and(|s| !s.is_empty())
                                })
                            })
                        {
                            target = None;
                        }
                        // `new X()` constructs a type, never a method that
                        // shares X's name; the cross-file pass looks for the
                        // type itself (0.9.90).
                        if self.is("c_sharp")
                            && n.kind_name() == "object_creation_expression"
                            && target
                                .as_ref()
                                .is_some_and(|t| self.method_owner.contains_key(t))
                        {
                            target = None;
                        }
                    }
                    // C#: the name found one overload; the argument count picks
                    // the one this call means, else the first declared
                    // (0.9.95, #4584).
                    if self.is("c_sharp")
                        && let Some(found) = target.clone()
                        && let Some(peers) = self.csharp_overloads.values().find(|group| {
                            group.len() > 1 && group.iter().any(|(_, id, _)| *id == found)
                        })
                    {
                        let picked = csharp_pick_overload(
                            peers.iter().map(|(_, id, meta)| (id.as_str(), meta)),
                            csharp_argc(n),
                        );
                        target = Some(picked.unwrap_or(peers[0].1.as_str()).to_string());
                    }
                    let external = target.as_ref().is_some_and(|id| {
                        !self.nodes.iter().any(|n| {
                            n["id"] == *id
                                && n["source_file"].as_str().is_some_and(|s| !s.is_empty())
                        })
                    });
                    if let Some(target) = target.as_ref().filter(|_| !external) {
                        if self.seen_calls.insert((caller.into(), target.clone())) {
                            self.edge(
                                caller,
                                target,
                                "calls",
                                n.start_position().row + 1,
                                Some("call"),
                                Value::Null,
                            );
                        }
                    } else if (target.is_none() || external)
                        && !(self.is("python") && !call.member && locals.contains(name))
                    {
                        let mut raw = json!({"caller_nid":caller,"callee":name,"is_member_call":call.member,"source_file":self.path,"source_location":format!("L{}",n.start_position().row+1),"receiver":call.receiver});
                        if call.member
                            && let Some(shadow) = self.python_shadow.as_mut()
                        {
                            let receiver = call.receiver.as_deref().unwrap_or("");
                            raw["_python_receiver_shadowed"] =
                                json!(shadow.is_shadowed(n, receiver, self.source));
                        }
                        if python && call.member {
                            self.python_member_stamps(n, caller, &call, &mut raw);
                        }
                        // The stub may stand in for a definition in another
                        // file; extract() binds it only with evidence (#3888).
                        if external {
                            raw["via_stub"] = json!(true);
                        }
                        if self.is("ruby")
                            && let Some(r) = &call.receiver
                        {
                            raw["receiver_type"] = json!(receivers.ruby.get(r).cloned().flatten());
                        }
                        // TS/JS `this.x.m()`: the receiver is a field of the
                        // caller's own class, which its field table types.
                        if call.this_field && self.js() {
                            raw["ts_this_field"] = json!(true);
                            if ts_in_static_method(n) {
                                raw["ts_static_this"] = json!(true);
                            }
                        }
                        if self.is("cpp") {
                            raw["lang"] = json!("cpp");
                        }
                        // PHP: a scoped class call (`Helper::m()`) apart from an
                        // instance member call (`$g->m()`), whose receiver the
                        // resolver types through the file's table (0.9.89).
                        if self.is("php") {
                            raw["php_scoped"] = json!(call.scoped);
                        }
                        if self.is("c_sharp") {
                            raw["lang"] = json!("csharp");
                            // The argument count picks an overload (0.9.95, #4584).
                            if let Some(argc) = csharp_argc(n) {
                                raw["arity"] = json!(argc);
                            }
                            if n.kind_name() == "object_creation_expression" {
                                raw["csharp_new"] = json!(true);
                            }
                            if let Some(q) = &call.qualified {
                                raw["qualified_prefix"] = json!(q);
                            }
                            if let Some(chain) = &call.csharp_chain {
                                raw["receiver_chain"] = json!(chain);
                            }
                            let at = n.start_byte() as i64;
                            match csharp_scoped_receiver_type(
                                receivers.csharp.as_ref(),
                                call.receiver.as_deref(),
                                at,
                            ) {
                                // `var v = R.M<A>(); v.X()` (#4266): R is typed
                                // here, M's return type in the cross-file pass.
                                Some(CsharpBinding::Call(recv, method, targs)) => {
                                    let recv = if recv.is_empty() || recv == "this" {
                                        Some(recv)
                                    } else {
                                        match csharp_scoped_receiver_type(
                                            receivers.csharp.as_ref(),
                                            Some(&recv),
                                            at,
                                        ) {
                                            Some(CsharpBinding::Type(t)) => Some(t),
                                            // An untyped local: no guess.
                                            Some(CsharpBinding::Call(..)) => None,
                                            None if !upper(&recv) => None,
                                            None => Some(recv),
                                        }
                                    };
                                    if let Some(recv) = recv {
                                        raw["receiver_call"] = json!([recv, method, targs]);
                                    }
                                }
                                Some(CsharpBinding::Type(ty)) if !ty.is_empty() => {
                                    raw["receiver_type"] = json!(ty);
                                }
                                _ => {}
                            }
                        }
                        if self.is("java") {
                            raw["lang"] = json!("java");
                            let receiver = call.receiver.as_deref().unwrap_or("");
                            if let Some(ty) = receivers.java.get(receiver).filter(|t| !t.is_empty())
                            {
                                raw["receiver_type"] = json!(ty);
                            } else if call.member
                                && !receiver.is_empty()
                                && crate::extract_python::is_identifier(receiver)
                                && !self
                                    .java_bound
                                    .get(caller)
                                    .is_some_and(|names| names.contains(receiver))
                            {
                                // Not a local, a parameter or a field of this
                                // class: a field it inherits from another file,
                                // whose type only the resolver sees (#4307).
                                raw["receiver_unbound"] = json!(true);
                            }
                            if let Some(chain) = call
                                .java_chain
                                .as_ref()
                                .and_then(|chain| java_type_chain_leaf(chain, &receivers.java))
                            {
                                raw["java_receiver_chain"] = chain;
                            }
                        }
                        if self.is("kotlin") {
                            if let Some(q) = &call.qualified {
                                raw["lang"] = json!("kotlin");
                                raw["qualified_prefix"] = json!(q);
                            }
                            if let Some(o) = &call.kotlin_object {
                                raw["lang"] = json!("kotlin");
                                raw["kotlin_object_receiver"] = json!(o);
                            }
                        }
                        self.raw.push(raw);
                    }
                }
            }
            if python || js {
                if let Some(args) = n.child_by_field_name("arguments") {
                    for arg in children(args) {
                        if arg.kind_name() == "identifier" {
                            self.indirect(Some(arg), caller, &locals, "argument");
                        } else if python && arg.kind_name() == "keyword_argument" {
                            self.indirect(
                                arg.child_by_field_name("value"),
                                caller,
                                &locals,
                                "argument",
                            );
                        }
                    }
                }
                if python {
                    self.getattr(n, caller);
                }
            }
            self.php_call_extras(n, caller, call.name.as_deref());
        }
        if self.is("php")
            && (self.config.static_prop_types.contains(kind)
                || kind == "class_constant_access_expression")
            && let Some(scope) = n
                .child_by_field_name("scope")
                .or_else(|| first_child(n, &["name", "qualified_name", "identifier"]))
        {
            let name = self.text(scope);
            if let Some(target) = self.labels_ci.get(&name.to_lowercase()).cloned() {
                let relation = if kind == "class_constant_access_expression" {
                    "references_constant"
                } else {
                    "uses_static_prop"
                };
                self.special(caller, &target, relation, n);
            }
        }
        let vals = if python && ["dictionary", "list", "set", "tuple"].contains(&kind) {
            python_values(n)
        } else if js && matches!(kind, "object" | "array") {
            js_dispatch_value_idents(n)
        } else {
            vec![]
        };
        for v in vals {
            self.indirect(Some(v), caller, &locals, "collection");
        }
        if python {
            let (v, ctx) = if kind == "assignment" {
                (n.child_by_field_name("right"), "assignment")
            } else if kind == "return_statement" {
                (named(n).into_iter().next(), "return")
            } else {
                (None, "")
            };
            for v in ref_values(v) {
                self.indirect(Some(v), caller, &locals, ctx);
            }
        }
        // JS scopes extend the names bound below them; elsewhere the set is
        // passed down as it is.
        let extended;
        let extra = if js
            && matches!(
                kind,
                "catch_clause" | "for_in_statement" | "statement_block" | "for_statement"
            ) {
            let mut more = extra.clone();
            if matches!(kind, "catch_clause" | "for_in_statement") {
                if let Some(p) = n.child_by_field_name(if kind == "catch_clause" {
                    "parameter"
                } else {
                    "left"
                }) {
                    js_collect_pattern_idents(p, self.source, &mut more);
                }
            } else {
                more.extend(js_direct_lexical_names(n, self.source));
            }
            extended = more;
            &extended
        } else {
            extra
        };
        for c in children(n) {
            self.walk_calls(c, caller, receivers, extra);
        }
    }
    fn special(&mut self, src: &str, target: &str, rel: &str, n: Node<'t>) {
        if src == target
            || self
                .edges
                .iter()
                .any(|e| e["source"] == src && e["target"] == target && e["relation"] == rel)
        {
            return;
        }
        self.edge(
            src,
            target,
            rel,
            n.start_position().row + 1,
            None,
            Value::Null,
        );
        self.edges.last_mut().unwrap()["confidence_score"] = json!(1.0);
    }
    fn php_call_extras(&mut self, n: Node<'t>, caller: &str, name: Option<&str>) {
        if !self.is("php") {
            return;
        }
        let Some(name) = name else { return };
        if self.config.helper_fn_names.contains(name)
            && let Some(args) = n.child_by_field_name("arguments")
            && let Some(content) = super::engine_config::descendant(args, "string_content")
        {
            let key = self.text(content);
            let segment = key.split('.').next().unwrap_or("").to_lowercase();
            if let Some(target) = self
                .labels_ci
                .get(&segment)
                .or_else(|| self.labels_ci.get(&format!("{segment}.php")))
                .cloned()
            {
                self.special(caller, &target, &format!("uses_{name}"), n);
            }
        }
        if n.kind_name() == "member_call_expression"
            && self.config.container_bind_methods.contains(name)
        {
            let classes = n
                .child_by_field_name("arguments")
                .map(children)
                .unwrap_or_default()
                .into_iter()
                .filter(|a| a.kind_name() == "argument")
                .filter_map(|a| first_child(a, &["class_constant_access_expression"]))
                .filter_map(|c| {
                    c.child_by_field_name("scope")
                        .or_else(|| first_child(c, &["name", "qualified_name", "identifier"]))
                })
                .take(2)
                .map(|n| self.text(n).to_lowercase())
                .collect::<Vec<_>>();
            if classes.len() == 2
                && let (Some(a), Some(b)) = (
                    self.labels_ci.get(&classes[0]).cloned(),
                    self.labels_ci.get(&classes[1]).cloned(),
                )
            {
                self.special(&a, &b, "bound_to", n);
            }
        }
    }
    fn module_dispatch(&mut self, n: Node<'t>, caller: &str, locals: &BTreeSet<String>) {
        if (self.is("python")
            && matches!(n.kind_name(), "function_definition" | "class_definition"))
            || (self.js() && set("_JS_SCOPE_BOUNDARY").contains(n.kind_name()))
        {
            return;
        }
        let vals = if self.is("python")
            && ["dictionary", "list", "set", "tuple"].contains(&n.kind_name())
        {
            python_values(n)
        } else if self.js() && matches!(n.kind_name(), "object" | "array") && {
            // The native parser can recover a malformed import's braces as
            // an object where the pinned binding produced a binding pattern.
            // Recovery nodes cannot establish a function-value reference.
            let mut parent = n.parent();
            let mut recovered = false;
            while let Some(p) = parent {
                recovered |= p.is_error();
                parent = p.parent();
            }
            !recovered
        } {
            js_dispatch_value_idents(n)
        } else {
            vec![]
        };
        for v in vals {
            self.indirect(Some(v), caller, locals, "collection");
        }
        if self.is("python") {
            if n.kind_name() == "assignment" {
                for v in ref_values(n.child_by_field_name("right")) {
                    self.indirect(Some(v), caller, locals, "assignment");
                }
            }
            if n.kind_name() == "call" {
                self.getattr(n, caller);
            }
        } else if self.js()
            && matches!(n.kind_name(), "call_expression" | "new_expression")
            && let Some(args) = n.child_by_field_name("arguments")
        {
            for a in children(args)
                .into_iter()
                .filter(|a| a.kind_name() == "identifier")
            {
                self.indirect(Some(a), caller, locals, "argument");
            }
        }
        for c in children(n) {
            self.module_dispatch(c, caller, locals);
        }
    }
    pub(crate) fn finish(mut self, root: Node<'t>) -> Value {
        // An enum member is never a call target (0.9.83, #4245).
        let case_of: BTreeSet<String> = self
            .edges
            .iter()
            .filter(|e| e["relation"] == "case_of")
            .filter_map(|e| e["target"].as_str().map(str::to_string))
            .collect();
        for n in &self.nodes {
            if n["type"] == "namespace" {
                continue;
            }
            let id = n["id"].as_str().unwrap_or("").to_string();
            if case_of.contains(&id) {
                continue;
            }
            let label = n["label"]
                .as_str()
                .unwrap_or("")
                .trim_matches(['(', ')'])
                .trim_start_matches('.')
                .to_string();
            if self.is("lua") {
                let sep = if label.contains(':') {
                    Some(':')
                } else if label.contains('.') {
                    Some('.')
                } else {
                    None
                };
                if let Some((table, _)) = sep.and_then(|sep| label.rsplit_once(sep)) {
                    self.lua_self_table.insert(id.clone(), table.to_string());
                }
            }
            if self.parents.contains_key(&id) {
                self.labels.entry(label.clone()).or_insert(id.clone());
                self.labels_ci.entry(label.to_lowercase()).or_insert(id);
            } else {
                self.labels.insert(label.clone(), id.clone());
                self.labels_ci.insert(label.to_lowercase(), id);
            }
        }
        for e in &self.edges {
            if e["relation"] == "inherits"
                && let (Some(src), Some(tgt)) = (e["source"].as_str(), e["target"].as_str())
            {
                self.local_bases
                    .entry(src.to_string())
                    .or_default()
                    .push(tgt.to_string());
            }
        }
        let lexical = self.lexical_relations();
        if self.self_call_language() || lexical.is_some() {
            let label_by_id: BTreeMap<&str, &str> = self
                .nodes
                .iter()
                .filter_map(|n| Some((n["id"].as_str()?, n["label"].as_str().unwrap_or(""))))
                .collect();
            for e in &self.edges {
                let (Some(src), Some(tgt)) = (e["source"].as_str(), e["target"].as_str()) else {
                    continue;
                };
                if e["relation"] == "contains" && lexical.is_some() {
                    // Two nested classes of one name are one node: which class
                    // lies around it is unknown.
                    match self.class_parent.get(tgt) {
                        None => {
                            self.class_parent.insert(tgt.to_string(), src.to_string());
                        }
                        Some(outer) if outer != src => {
                            self.class_parent.insert(tgt.to_string(), String::new());
                        }
                        Some(_) => {}
                    }
                    continue;
                }
                if e["relation"] == "contains" && self.js() {
                    self.js_factory_scope
                        .entry(tgt.to_string())
                        .or_insert_with(|| src.to_string());
                }
                if e["relation"] != "method" {
                    continue;
                }
                self.method_owner.insert(tgt.to_string(), src.to_string());
                let name = label_by_id
                    .get(tgt)
                    .copied()
                    .unwrap_or("")
                    .trim_matches(['(', ')'])
                    .trim_start_matches('.')
                    .to_string();
                let name = if self.is("php") {
                    crate::dedup_strings::casefold(&name)
                } else {
                    name
                };
                self.methods_by_owner
                    .entry((src.to_string(), name))
                    .or_insert_with(|| tgt.to_string());
            }
            if let Some(relations) = lexical {
                let nid_to_sf: BTreeMap<&str, &str> = self
                    .nodes
                    .iter()
                    .filter_map(|n| {
                        Some((n["id"].as_str()?, n["source_file"].as_str().unwrap_or("")))
                    })
                    .collect();
                self.lexical_bases = lexical_class_bases(
                    &self.edges,
                    relations,
                    &label_by_id,
                    &nid_to_sf,
                    &self.method_owner,
                    &self.methods_by_owner,
                    &self.class_parent,
                    &self.interfaces,
                );
            }
        }
        if self.is("python") {
            // `self.X` per class from the bindings in its methods (#2860);
            // conflicting or untypable bindings poison the field.
            for (caller, body) in &self.bodies {
                let Some(owner) = self.method_owner.get(caller) else {
                    continue;
                };
                let fields = self.python_attr_types.entry(owner.clone()).or_default();
                for (attr, class) in python_self_attr_bindings(*body, self.source) {
                    let kept = match fields.get(&attr) {
                        Some(previous) if *previous != class => None,
                        _ => class,
                    };
                    fields.insert(attr, kept);
                }
            }
            // Typed parameters and locals per function (#4198); `@property`
            // readers are marked for the attribute-read pass (#4337).
            let mut properties = BTreeSet::new();
            // pytest fixtures (0.9.92, #4332): what each fixture returns, and
            // the parameters of tests and fixtures pytest fills, in test
            // modules and `conftest.py` only.
            let test_module = python_is_test_module(&self.path);
            let mut fixtures: BTreeMap<String, String> = BTreeMap::new();
            for (caller, body) in &self.bodies {
                let bindings = python_local_class_bindings(*body, self.source);
                let func = body
                    .parent()
                    .filter(|f| f.kind_name() == "function_definition");
                if func.is_some_and(|f| python_is_property(f, self.source)) {
                    properties.insert(caller.clone());
                }
                if let Some(func) = func.filter(|_| test_module) {
                    let fixture = python_is_pytest_fixture(func, self.source);
                    if fixture {
                        fixtures.insert(
                            caller.clone(),
                            python_fixture_type(func, self.source, &bindings),
                        );
                    }
                    if fixture
                        || func
                            .child_by_field_name("name")
                            .is_some_and(|n| self.text(n).starts_with("test"))
                    {
                        self.python_fixture_params
                            .insert(caller.clone(), python_untouched_params(func, self.source));
                    }
                }
                self.python_var_types.insert(caller.clone(), bindings);
            }
            for n in &mut self.nodes {
                if n["id"].as_str().is_some_and(|id| properties.contains(id)) {
                    n["_py_property"] = json!(true);
                }
                if let Some(kind) = n["id"].as_str().and_then(|id| fixtures.get(id)) {
                    n["_py_fixture"] = json!(kind);
                }
            }
        }
        if self.is("java") {
            // Names a receiver can be without being an inherited field, per
            // caller: overloads share one node, so their names are pooled.
            let callers: BTreeMap<usize, String> = self
                .bodies
                .iter()
                .map(|(caller, body)| (body.id(), caller.clone()))
                .collect();
            for (body, (method, class)) in &self.method_scopes {
                let Some(caller) = callers.get(body) else {
                    continue;
                };
                let mut names = java_bound_names(*method, self.source);
                for ancestor in self.up_chain(class) {
                    names.extend(
                        self.java_field_names
                            .get(&ancestor)
                            .into_iter()
                            .flatten()
                            .cloned(),
                    );
                }
                self.java_bound
                    .entry(caller.clone())
                    .or_default()
                    .extend(names);
            }
        }
        let mut receiver_tables = BTreeMap::new();
        for (body, (method, owner)) in &self.method_scopes {
            let mut fields = Table::new();
            for cls in self.up_chain(owner) {
                if let Some(f) = self.fields.get(&cls) {
                    for (k, v) in f {
                        fields.entry(k.clone()).or_insert(v.clone());
                    }
                }
            }
            let mut table = Receivers::default();
            if self.is("java") {
                table.java = java_method_receiver_types(*method, self.source, &fields);
            }
            if self.is("c_sharp") {
                table.csharp = Some(csharp_method_receiver_types(*method, self.source, &fields));
            }
            receiver_tables.insert(*body, table);
        }
        for (_, body) in &self.bodies {
            if self.is("cpp") {
                cpp_local_var_types(*body, self.source, &mut self.type_table);
            }
            if self.is("swift") {
                swift_local_var_types(
                    *body,
                    self.source,
                    &mut self.type_table,
                    Some(&mut self.swift_factory),
                );
            }
        }
        if self.is("python") {
            self.python_shadow = Some(super::engine_scope::PythonReceiverShadow::new(
                root,
                self.source,
            ));
        }
        for (caller, body) in self.bodies.clone() {
            let mut table = receiver_tables.remove(&body.id()).unwrap_or_default();
            if self.is("ruby") {
                table.ruby = engine_ruby::local_class_bindings(body, self.source);
                table.ruby_locals = engine_ruby::local_names(body, self.source);
            }
            let extra = self
                .closure_locals
                .get(&body.id())
                .cloned()
                .unwrap_or_default();
            self.walk_calls(body, &caller, &table, &extra);
        }
        for (caller, n) in self.initializers.clone() {
            self.walk_calls(n, &caller, &Receivers::default(), &BTreeSet::new());
        }
        let mut pairs = BTreeSet::new();
        for (event, listener, line) in self.listeners.clone() {
            if let (Some(a), Some(b)) = (
                self.labels_ci.get(&event.to_lowercase()).cloned(),
                self.labels_ci.get(&listener.to_lowercase()).cloned(),
            ) && a != b
                && pairs.insert((a.clone(), b.clone()))
            {
                self.edge(&a, &b, "listened_by", line, None, Value::Null);
                self.edges.last_mut().unwrap()["confidence_score"] = json!(1.0);
            }
        }
        let file = self.file.clone();
        if self.is("python") {
            self.module_dispatch(root, &file, &python_module_bound_names(root, self.source));
        } else if self.js() {
            self.module_dispatch(root, &file, &js_module_bound_names(root, self.source));
        }
        // Duplicate edges are dropped by their canonical JSON (sorted keys);
        // the bytes are written directly and the kept records move out.
        let mut payloads = BTreeSet::new();
        let edges = std::mem::take(&mut self.edges)
            .into_iter()
            .filter(|e| {
                self.seen.contains(e["source"].as_str().unwrap_or(""))
                    && (self.seen.contains(e["target"].as_str().unwrap_or(""))
                        || ["imports", "imports_from", "re_exports"]
                            .contains(&e["relation"].as_str().unwrap_or("")))
            })
            .filter(|e| payloads.insert(crate::extract_normalize::canonical_bytes(e)))
            .collect::<Vec<_>>();
        self.raw.extend(self.mixins.clone());
        if self.is("python") {
            // Attribute reads that may run a `@property` getter (#4337).
            let reads = python_property_reads(
                &self.bodies,
                self.source,
                &self.path,
                &self.python_var_types,
                &self.python_attr_types,
                &self.method_owner,
            );
            self.raw.extend(reads);
        }
        // Per-file field tables for the corpus member-call resolvers, keyed
        // by class label and source file; C# also exports each class's method
        // return types (`csharp_returns`, #4266).
        let mut fields = vec![];
        let lang = if self.is("java") { "java" } else { "csharp" };
        let tables = self
            .field_order
            .iter()
            .filter_map(|class| Some((lang, class.as_str(), json!(self.fields.get(class)?))))
            .chain(
                self.csharp_returns
                    .iter()
                    .map(|(class, methods)| ("csharp_returns", class.as_str(), methods.clone())),
            );
        for (lang, class, table) in tables {
            if table.as_object().is_some_and(|t| !t.is_empty())
                && let Some(n) = self.nodes.iter().find(|n| n["id"] == class)
                && n["label"].as_str().is_some_and(|l| !l.is_empty())
            {
                fields.push(json!({"lang":lang,"class_label":n["label"],"source_file":n["source_file"],"fields":table}));
            }
        }
        for n in &mut self.nodes {
            if let Some(ret) = n["id"]
                .as_str()
                .and_then(|id| self.java_returns.get(id))
                .filter(|r| !r.is_empty())
            {
                n["_java_ret"] = json!(ret);
            }
        }
        if self.js() {
            ts_mark_public_twins(root, self.source, &mut self.nodes, &edges);
        }
        for n in &mut self.nodes {
            let id = n["id"].as_str().unwrap_or("");
            if self.callable.contains(id) {
                let class = self.classes.contains(id);
                n["_callable"] = json!(true);
                if class {
                    n["_callable_class"] = json!(true);
                }
            }
        }
        // The records move into the result (`json!` would copy each through
        // the serializer); the keys keep their order.
        let mut out = serde_json::Map::new();
        out.insert(
            "nodes".into(),
            Value::Array(std::mem::take(&mut self.nodes)),
        );
        out.insert("edges".into(), Value::Array(edges));
        out.insert(
            "raw_calls".into(),
            Value::Array(std::mem::take(&mut self.raw)),
        );
        let mut out = Value::Object(out);
        if !fields.is_empty() {
            out["member_field_tables"] = json!(fields);
        }
        if root.has_error() {
            // A missing anonymous token can set has_error() without appearing
            // in named_children(). Preserve the smallest containing error node
            // as the location when no named ERROR/MISSING child is available.
            let mut first_error = root;
            while !first_error.is_error() && !first_error.is_missing() {
                let Some(child) = children(first_error).into_iter().find(|n| n.has_error()) else {
                    break;
                };
                first_error = child;
            }
            let mut stack = vec![root];
            let mut multiline = false;
            while let Some(n) = stack.pop() {
                multiline |= n.is_error() && n.end_position().row > n.start_position().row;
                stack.extend(children(n).into_iter().rev());
            }
            out["parse_errors"] = json!({"first_error_line":first_error.start_position().row + 1,"multiline_error":multiline});
        }
        if self.is("kotlin")
            && let Some(p) = children(root)
                .into_iter()
                .find(|n| matches!(n.kind_name(), "package_header" | "package"))
        {
            let raw = self.text(p);
            let name = raw
                .trim()
                .trim_start_matches("package")
                .trim()
                .trim_end_matches(';')
                .trim();
            if !name.is_empty() {
                out["kotlin_package"] = json!(name);
            }
        }
        if !self.swift_extensions.is_empty() {
            out["swift_extensions"] = json!(self.swift_extensions);
        }
        if self.js() {
            // Class fields go on their class node (`_ts_fields`), never into
            // the file-wide table: `this.x` reads the caller's class and its
            // bases, which may live in files an incremental rebuild does not
            // re-extract (0.9.91).
            let mut fields = TsFieldTable::new();
            ts_receiver_type_table(root, self.source, &mut self.type_table, Some(&mut fields));
            if let Some(nodes) = out["nodes"].as_array_mut() {
                ts_attach_class_fields(nodes, &fields, &self.path);
            }
        }
        if self.is("php") {
            php_receiver_type_table(root, self.source, &mut self.type_table);
        }
        let key = if self.is("swift") {
            Some("swift_type_table")
        } else if self.js() {
            Some("ts_type_table")
        } else if self.is("cpp") {
            Some("cpp_type_table")
        } else if self.is("php") {
            Some("php_type_table")
        } else {
            None
        };
        if let Some(key) = key
            && (!self.type_table.is_empty() || !self.swift_factory.is_empty())
        {
            out[key] = json!({"path":self.path,"table":self.type_table});
            if self.is("swift") && !self.swift_factory.is_empty() {
                out[key]["factory"] = json!(self.swift_factory);
            }
        }
        out
    }
}
/// `_csharp_member_call_parts` (0.9.74 #3797): the name and receiver nodes
/// of `recv.M()` and of the null-conditional `recv?.M()`, whose receiver is
/// the `condition` and whose name sits in a `member_binding_expression`.
fn csharp_member_call_parts(f: Node<'_>) -> Option<(Option<Node<'_>>, Option<Node<'_>>)> {
    match f.kind_name() {
        "member_access_expression" => Some((
            f.child_by_field_name("name"),
            f.child_by_field_name("expression"),
        )),
        "conditional_access_expression" => named(f)
            .into_iter()
            .find(|c| c.kind_name() == "member_binding_expression")
            .map(|c| {
                (
                    c.child_by_field_name("name"),
                    f.child_by_field_name("condition"),
                )
            }),
        _ => None,
    }
}
