// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/engine.py
//! Generic, statically linked AST extraction engine.
use super::{
    base::{file_stem, make_id, read_text},
    engine_config, engine_parser,
    engine_scope::*,
    engine_types::*,
    models::LanguageConfig,
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use tree_sitter::Node;
pub type Table = BTreeMap<String, String>;
pub(crate) struct Engine<'a, 't> {
    pub config: &'a LanguageConfig,
    pub source: &'a [u8],
    pub path: String,
    pub stem: String,
    pub file: String,
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
    pub raw: Vec<Value>,
    pub seen: BTreeSet<String>,
    pub callable: BTreeSet<String>,
    pub classes: BTreeSet<String>,
    pub namespace: Vec<String>,
    pub scopes: Vec<String>,
    pub bodies: Vec<(String, Node<'t>)>,
    pub initializers: Vec<(String, Node<'t>)>,
    pub locals: BTreeMap<String, BTreeSet<String>>,
    pub closure_locals: BTreeMap<usize, BTreeSet<String>>,
    pub parents: Table,
    pub lexical: BTreeMap<String, Table>,
    pub type_table: Table,
    pub swift_factory: BTreeMap<String, (String, String)>,
    pub swift_extensions: Vec<Value>,
    pub fields: BTreeMap<String, Table>,
    pub method_scopes: BTreeMap<usize, (Node<'t>, String)>,
    pub interfaces: BTreeSet<String>,
    pub protocols: BTreeSet<String>,
    pub swift_classes: BTreeSet<String>,
    pub underscore: BTreeMap<String, BTreeSet<String>>,
    /// C# names that differ only by `_` or case (#4415): types, members.
    pub csharp_type_groups: BTreeMap<String, BTreeSet<String>>,
    pub csharp_member_groups: BTreeMap<String, BTreeSet<String>>,
    pub labels: Table,
    pub labels_ci: Table,
    pub seen_calls: BTreeSet<(String, String)>,
    pub seen_indirect: BTreeSet<(String, String)>,
    pub external: BTreeSet<String>,
    pub seen_dynamic: BTreeSet<(String, String)>,
    pub ruby_namespace: Vec<String>,
    pub ruby_lexical: Vec<String>,
    pub singleton_depth: usize,
    pub ruby_kinds: BTreeMap<String, Vec<String>>,
    pub mixins: Vec<Value>,
    pub listeners: Vec<(String, String, usize)>,
    /// The corpus scan root handed to Python import resolution (#3729).
    pub scan_root: Option<PathBuf>,
    /// PHP closure ordinals per enclosing class or file (#3409).
    pub php_closures: BTreeMap<String, usize>,
    /// Lua: method id -> the table it is attached to (`Animal` for
    /// `Animal:speak`), so `self:other()` resolves to a sibling (#3991).
    pub lua_self_table: Table,
    /// Self-call binding (0.9.75): method id -> owning class,
    /// `(owner, name)` -> method id and in-file `inherits` bases, so
    /// `self.m()` stays on the caller's own class chain.
    pub method_owner: Table,
    pub methods_by_owner: BTreeMap<(String, String), String>,
    pub local_bases: BTreeMap<String, Vec<String>>,
    /// Receiver-shadow guard for plain external module calls (Python only).
    pub python_shadow: Option<super::engine_scope::PythonReceiverShadow>,
    /// JS/TS: names the module exports (#3778), computed on first use;
    /// upstream rescans the program for every `Obj.m = fn` assignment.
    pub js_exported: Option<BTreeSet<String>>,
    /// Classes in the order their member field table was first opened, the
    /// order `member_field_tables` lists them (upstream dict order).
    pub field_order: Vec<String>,
    /// #4334: the id of the file's bare stem (`widget` for `pkg/widget.py`)
    /// and the stub a forward reference minted with it -> the same-named
    /// class's local id, settled after the walk.
    pub stem_id: String,
    pub stem_forward_refs: Table,
    /// Java: every field name each class declares, typed or not (0.9.88).
    pub java_field_names: BTreeMap<String, BTreeSet<String>>,
    /// Java: method id -> the return type all its overloads declare.
    pub java_returns: Table,
    /// Java: per caller, the names a receiver can be without being an
    /// inherited field (`_java_bound_names` and the class's own fields).
    pub java_bound: BTreeMap<String, BTreeSet<String>>,
    /// C#: type id -> its name within the namespace, enclosing types first
    /// (`` Outer`1.Inner ``); None when two declarations disagree (#4363).
    pub csharp_type_paths: BTreeMap<String, Option<String>>,
    /// C#: class id -> method name -> `[return type, type-param count]` per
    /// overload, exported as `csharp_returns` (#4266); insertion order.
    pub csharp_returns: serde_json::Map<String, Value>,
    /// Lexical-call languages (0.9.83): the class around each class
    /// (empty when two nested classes of one name share a node) and each
    /// class's supertypes.
    pub class_parent: Table,
    pub lexical_bases: BTreeMap<String, Vec<String>>,
    /// Python: per function, `name -> class` of typed parameters and locals;
    /// per class, `self.X -> class` (0.9.80).
    pub python_var_types: BTreeMap<String, PythonBindings>,
    pub python_attr_types: BTreeMap<String, PythonBindings>,
    /// C#: plain method id -> `(signature, id, metadata)` of the overloads of
    /// one name on one type, in declaration order (0.9.95, #4584).
    pub csharp_overloads: BTreeMap<String, Vec<(String, String, Value)>>,
    /// JS/TS: the enclosing scope of each `contains` target (a factory
    /// object `const store = {}` inside `makeStore()`), so `store.get()`
    /// inside `store.set = ...` is a sibling call (0.9.94, #4598).
    pub js_factory_scope: Table,
    /// Python: caller -> the parameters pytest may fill with a fixture (tests
    /// and fixtures in test modules and `conftest.py`, 0.9.92, #4332).
    pub python_fixture_params: BTreeMap<String, BTreeSet<String>>,
    /// JS/TS: class body -> member names grouped by the id they would get,
    /// filled on first use (0.9.92, #4475).
    pub js_member_groups: BTreeMap<(String, usize, usize), BTreeMap<String, BTreeSet<String>>>,
}
// Scoped thread-local state replaces `_safe_extract(..., scan_root=root)`:
// `extract_file` enters the corpus root for each file so the Python import
// handler resolves absolute imports against it. Direct extraction of one file
// has none and falls back to the file's own directory, as upstream does.
thread_local! { static SCAN_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) }; }
pub struct ScanRootGuard(Option<PathBuf>);
impl Drop for ScanRootGuard {
    fn drop(&mut self) {
        SCAN_ROOT.with(|root| *root.borrow_mut() = self.0.take());
    }
}
pub fn enter_scan_root(root: Option<&Path>) -> ScanRootGuard {
    ScanRootGuard(SCAN_ROOT.with(|old| old.replace(root.map(Path::to_path_buf))))
}
impl<'a, 't> Engine<'a, 't> {
    pub fn is(&self, lang: &str) -> bool {
        // `ts_module == format!("tree_sitter_{lang}")` without allocating:
        // this runs for every syntax node.
        self.config.ts_module.strip_prefix("tree_sitter_") == Some(lang)
    }
    pub fn js(&self) -> bool {
        self.is("javascript") || self.is("typescript")
    }
    /// Languages whose `self`/`this` member calls bind through
    /// [`Engine::self_call_target`] (0.9.75 `_SELF_CALL_LANGUAGES`; PHP since
    /// 0.9.77 for builtin-named `$this->m()` calls).
    pub fn self_call_language(&self) -> bool {
        self.is("python") || self.js() || self.is("swift") || self.is("ruby") || self.is("php")
    }
    /// In-file target of `self.m()` / `cls.m()` / `super().m()` (Python),
    /// `this.m()` / `super.m()` (JS/TS), `self.m()` / bare `m()` /
    /// `super.m()` (Swift) and self-sends (Ruby), else None.
    ///
    /// The lookup starts at the caller's enclosing class (skipped for
    /// `super`) and walks its in-file bases one level at a time, nearest
    /// first; two hits on one level are a multiple-inheritance tie and bind
    /// nothing. A method of a class outside that chain is never the target,
    /// while a file-wide hit that is not a method still binds. With
    /// `walk_bases` false (JS/TS, whose `extends` edges come later) an
    /// inherited `this.m()` keeps the plain lookup and `super.m()` fails
    /// closed. `require_method_owner` (PHP builtin-named `$this->m()`, 0.9.77)
    /// refuses the file-wide fallback when the caller has no owning class.
    pub fn self_call_target(
        &self,
        caller: &str,
        callee: &str,
        receiver: &str,
        walk_bases: bool,
        require_method_owner: bool,
    ) -> Option<String> {
        let mut scope = Some(caller.to_string());
        let mut climbed = BTreeSet::new();
        while let Some(s) = scope.as_ref() {
            if self.method_owner.contains_key(s) || !climbed.insert(s.clone()) {
                break;
            }
            scope = self.parents.get(s).cloned();
        }
        let fallback = self.labels.get(callee).cloned();
        let Some(owner) = scope.and_then(|s| self.method_owner.get(&s).cloned()) else {
            return if require_method_owner { None } else { fallback };
        };
        let mut level = vec![owner];
        let mut seen = BTreeSet::new();
        let mut skip_own = receiver == "super";
        while !level.is_empty() {
            seen.extend(level.iter().cloned());
            if !skip_own {
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
            }
            if !walk_bases {
                return if skip_own { None } else { fallback };
            }
            skip_own = false;
            let mut next: Vec<String> = Vec::new();
            for c in &level {
                for base in self.local_bases.get(c).into_iter().flatten() {
                    if !seen.contains(base) && !next.contains(base) {
                        next.push(base.clone());
                    }
                }
            }
            level = next;
        }
        fallback.filter(|f| !self.method_owner.contains_key(f))
    }
    pub fn text(&self, n: Node<'_>) -> String {
        read_text(n, self.source)
    }
    pub fn add_node(
        &mut self,
        id: &str,
        label: &str,
        line: usize,
        kind: Option<&str>,
        mut meta: Value,
    ) {
        if !self.seen.insert(id.into()) {
            return;
        }
        if !meta.is_object() {
            meta = json!({});
        }
        if !self.namespace.is_empty() {
            meta.as_object_mut()
                .unwrap()
                .entry("namespace")
                .or_insert(json!(self.namespace.join(".")));
        }
        if !self.scopes.is_empty() && kind != Some("namespace") {
            meta.as_object_mut()
                .unwrap()
                .entry("scope_chain")
                .or_insert(json!(self.scopes));
        }
        let mut n = json!({"id":id,"label":label,"file_type":"code","source_file":self.path,"source_location":format!("L{line}")});
        if let Some(k) = kind {
            n["type"] = json!(k);
        }
        if meta.as_object().is_some_and(|m| !m.is_empty()) {
            n["metadata"] = Value::Object(crate::security::sanitize_metadata(Some(&meta)));
        }
        self.nodes.push(n);
    }
    pub fn edge(
        &mut self,
        src: &str,
        tgt: &str,
        rel: &str,
        line: usize,
        context: Option<&str>,
        meta: Value,
    ) {
        let mut e = json!({"source":src,"target":tgt,"relation":rel,"confidence":"EXTRACTED","source_file":self.path,"source_location":format!("L{line}"),"weight":1.0});
        if let Some(c) = context.filter(|s| !s.is_empty()) {
            e["context"] = json!(c);
        }
        if meta.as_object().is_some_and(|m| !m.is_empty()) {
            e["metadata"] = Value::Object(crate::security::sanitize_metadata(Some(&meta)));
        }
        self.edges.push(e);
    }
    pub fn ensure(&mut self, name: &str, origin: bool) -> String {
        let mut local = make_id([self.stem.as_str(), self.namespace.join(".").as_str(), name]);
        if self.is("c_sharp") {
            local = python_underscore_salted_nid(&local, name, &self.csharp_type_groups);
        } else if self.is("python") {
            // A salted module-level class (0.9.92, #2810).
            local = python_underscore_salted_nid(&local, name, &self.underscore);
        }
        if self.seen.contains(&local) {
            return local;
        }
        let id = make_id([name]);
        if origin && id == self.stem_id {
            // #4334: `Widget` in widget.py slugs to the file-stem id, so the
            // stub can swallow the file node. Decided after the walk.
            self.stem_forward_refs
                .insert(id.clone(), make_id([self.stem.as_str(), name]));
        }
        if self.seen.insert(id.clone()) {
            let mut n = json!({"id":id,"label":name,"file_type":"code","source_file":"","source_location":""});
            if origin {
                n["origin_file"] = json!(self.path);
            }
            self.nodes.push(n);
        }
        id
    }
    /// #4334: a forward reference to a class defined later in this file whose
    /// id equals the file stem's resolves to that class, and the sourceless
    /// stub goes. Anything else keeps the stub for the cross-file rewire.
    pub fn settle_stem_forward_refs(&mut self) {
        for (stub, local) in std::mem::take(&mut self.stem_forward_refs) {
            if !self.classes.contains(&local) {
                continue;
            }
            for e in &mut self.edges {
                if e["target"] == stub.as_str() {
                    e["target"] = json!(local);
                }
            }
            self.nodes
                .retain(|n| !(n["id"] == stub.as_str() && n["source_file"] == ""));
            self.seen.remove(&stub);
        }
    }
    pub fn name(&self, n: Node<'_>) -> Option<String> {
        n.child_by_field_name(&self.config.name_field)
            .or_else(|| {
                children(n).into_iter().find(|c| {
                    self.config
                        .name_fallback_child_types
                        .iter()
                        .any(|t| t == c.kind_name())
                })
            })
            .map(|n| self.text(n))
    }
    pub fn refs(
        &mut self,
        owner: &str,
        n: Option<Node<'t>>,
        line: usize,
        context: &str,
        generic: bool,
    ) {
        self.type_refs(owner, n, line, context, generic, true);
    }
    /// [`Engine::refs`] with the self-reference guard optional: upstream
    /// keeps a type argument that names the declaring type itself in C#
    /// base lists, Kotlin delegation specifiers and Swift inheritance clauses
    /// (`class ObjectDetails : IComparer<ObjectDetails>`), and skips it
    /// everywhere else.
    pub fn type_refs(
        &mut self,
        owner: &str,
        n: Option<Node<'t>>,
        line: usize,
        context: &str,
        generic: bool,
        skip_self: bool,
    ) {
        let mut refs = vec![];
        let s = self.source;
        if self.is("c_sharp") {
            let mut refs = vec![];
            csharp_collect_type_refs(n, s, generic, &mut refs, None);
            for (name, role, qualified, qualifier) in refs {
                let target = self.ensure(&name, true);
                if !skip_self || target != owner {
                    let ctx = if role == "generic_arg" {
                        "generic_arg"
                    } else {
                        context
                    };
                    let mut m = json!({"ref_token":name});
                    if qualified {
                        m["qualified"] = json!(true);
                    }
                    if !qualifier.is_empty() {
                        m["ref_qualifier"] = json!(qualifier);
                    }
                    self.edge(owner, &target, "references", line, Some(ctx), m);
                }
            }
            return;
        }
        match self.config.ts_module.as_str() {
            "tree_sitter_python" => python_collect_type_refs(n, s, generic, &mut refs),
            "tree_sitter_java" | "tree_sitter_groovy" => {
                java_collect_type_refs(n, s, generic, &mut refs, None, false)
            }
            "tree_sitter_php" => php_collect_type_refs(n, s, generic, &mut refs),
            "tree_sitter_kotlin" => kotlin_collect_type_refs(n, s, generic, &mut refs),
            "tree_sitter_swift" => swift_collect_type_refs(n, s, generic, &mut refs),
            "tree_sitter_c" => c_collect_type_refs(n, s, generic, &mut refs),
            "tree_sitter_cpp" => {
                cpp_collect_type_refs(n, s, generic, &mut refs, cpp_dialect_qualifiers(&self.path))
            }
            "tree_sitter_scala" => scala_collect_type_refs(n, s, generic, &mut refs),
            _ => {}
        }
        for (name, role) in refs {
            let target = self.ensure(&name, true);
            if !skip_self || target != owner {
                self.edge(
                    owner,
                    &target,
                    "references",
                    line,
                    Some(if role == "generic_arg" {
                        "generic_arg"
                    } else {
                        context
                    }),
                    Value::Null,
                );
            }
        }
    }
    pub fn walk(&mut self, n: Node<'t>, parent: Option<&str>) {
        let t = n.kind_name();
        let line = n.start_position().row + 1;
        if self.config.import_types.contains(t)
            || (self.is("lua")
                && t == "function_call"
                && n.child_by_field_name("name")
                    .is_some_and(|v| self.text(v) == "require"))
        {
            if let Some(import) = &self.config.import_handler
                && let Some(modules) = import(
                    n,
                    self.source,
                    &self.file,
                    &self.stem,
                    &mut self.edges,
                    &self.path,
                    Some(&self.scopes),
                    self.scan_root.as_deref(),
                )
            {
                for (id, name) in modules {
                    if self.seen.insert(id.clone()) {
                        self.nodes.push(json!({"id":id,"label":name,"file_type":"code","type":"module","source_file":self.path,"source_location":format!("L{line}")}));
                    }
                }
            }
            if t == "export_statement" && first_child(n, &["string"]).is_none() {
                for c in children(n) {
                    self.walk(c, parent);
                }
            }
            return;
        }
        if self.config.class_types.contains(t) {
            self.class(n, parent);
            return;
        }
        if self.field(n, parent) {
            return;
        }
        if self.config.function_types.contains(t) {
            self.function(n, parent);
            return;
        }
        if self.extra(n, parent) {
            return;
        }
        // An explicit module-level type alias declares a type name (0.9.95,
        // #4578): a node, so annotations using it reference it instead of
        // minting a sourceless stub. The statement is then walked as usual.
        if self.is("python")
            && parent.is_none()
            && matches!(t, "type_alias_statement" | "expression_statement")
            && n.parent().is_some_and(|p| p.kind_name() == "module")
            && let Some(alias) = python_alias_name(n, self.source)
        {
            let id = python_underscore_salted_nid(
                &make_id([self.stem.as_str(), alias.as_str()]),
                &alias,
                &self.underscore,
            );
            self.add_node(&id, &alias, line, None, Value::Null);
            let file = self.file.clone();
            self.edge(&file, &id, "contains", line, None, Value::Null);
        }
        if t == "decorated_definition" {
            if self.is("python")
                && let Some(inner) = n.child_by_field_name("definition")
                && let Some(name) = self
                    .name(inner)
                    .filter(|s| !crate::ids::normalize_id(s).is_empty())
            {
                let id = if self.config.class_types.contains(inner.kind_name()) {
                    // Nested classes are qualified by their class (0.9.81, #4228).
                    match parent {
                        Some(parent) => make_id([parent, name.as_str()]),
                        None => make_id([
                            self.stem.as_str(),
                            self.namespace.join(".").as_str(),
                            name.as_str(),
                        ]),
                    }
                } else {
                    make_id([parent.unwrap_or(&self.stem), name.as_str()])
                };
                for d in children(n)
                    .into_iter()
                    .filter(|c| c.kind_name() == "decorator")
                {
                    // `@x.setter` / `@x.getter` / `@x.deleter` is the
                    // property-descriptor protocol, not a reference to a
                    // corpus symbol (0.9.88, #4380).
                    let expr = named(d).into_iter().next().map(|e| self.text(e));
                    if expr.is_some_and(|e| {
                        [".setter", ".getter", ".deleter"]
                            .iter()
                            .any(|s| e.ends_with(s))
                    }) {
                        continue;
                    }
                    if let Some(name) = decorator_name(d, self.source)
                        && !set("_PYTHON_DECORATOR_NOISE").contains(&name)
                    {
                        let target = self.ensure(&name, true);
                        if id != target {
                            self.edge(
                                &id,
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
            for c in children(n) {
                self.walk(c, parent);
            }
            return;
        }
        if (self.is("kotlin") && t == "companion_object")
            || t == "enum_body_declarations"
            || t == "ERROR"
        {
            for c in children(n) {
                if t == "companion_object" && c.kind_name() == "class_body" {
                    for m in children(c) {
                        self.walk(m, parent);
                    }
                } else {
                    self.walk(c, parent);
                }
            }
            return;
        }
        for c in children(n) {
            self.walk(c, None);
        }
    }
    pub fn function(&mut self, n: Node<'t>, parent: Option<&str>) {
        let name = if n.kind_name() == "deinit_declaration" {
            Some("deinit".into())
        } else if n.kind_name() == "subscript_declaration" {
            Some("subscript".into())
        } else if let Some(f) = &self.config.resolve_function_name_fn {
            n.child_by_field_name("declarator")
                .and_then(|d| f(d, self.source))
        } else {
            self.name(n)
        };
        let name = match name.filter(|s| !s.is_empty()) {
            Some(name) => name,
            // PHP closures (#3409): a routing closure is named `VERB /path`;
            // any other gets an ordinal that is stable across line-only edits.
            None if self.is("php")
                && matches!(n.kind_name(), "anonymous_function" | "arrow_function") =>
            {
                php_route_name(n, self.source).unwrap_or_else(|| {
                    let count = self
                        .php_closures
                        .entry(parent.unwrap_or(&self.stem).to_string())
                        .or_default();
                    *count += 1;
                    format!("{{closure#{count}}}")
                })
            }
            None => return,
        };
        let sanitized = self
            .config
            .sanitize_symbol_name_fn
            .as_ref()
            .map(|f| f(&name))
            .unwrap_or_else(|| name.clone());
        if crate::ids::normalize_id(&sanitized).is_empty() {
            return;
        }
        let line = n.start_position().row + 1;
        let mut id = make_id([parent.unwrap_or(&self.stem), &sanitized]);
        // C# overloads (0.9.95, #4584): `(plain id, signature, metadata)`.
        let mut overload = None;
        if self.is("python") {
            id = python_underscore_salted_nid(&id, &sanitized, &self.underscore);
        } else if self.is("c_sharp") && parent.is_some() {
            // `Put()` / `_Put()` are two C# methods (#4415).
            id = python_underscore_salted_nid(&id, &sanitized, &self.csharp_member_groups);
            if n.kind_name() == "method_declaration" {
                // Overloads shared one id, and every later one vanished into
                // the first: the first declaration keeps the plain id, each
                // other signature is salted like #3302/#4415.
                let (signature, mut meta) = csharp_method_signature(n, self.source);
                let group = self.csharp_overloads.entry(id.clone()).or_default();
                let plain = id.clone();
                if !group.is_empty() && !group.iter().any(|(sig, _, _)| *sig == signature) {
                    use sha1::Digest;
                    let salt = format!("{:x}", sha1::Sha1::digest(signature.as_bytes()));
                    id = make_id([id.as_str(), &salt[..6]]);
                }
                meta["signature"] = json!(signature);
                overload = Some((plain, signature, meta));
            }
        } else if self.js()
            && let Some(class) = parent
            && matches!(
                n.kind_name(),
                "method_definition" | "method_signature" | "abstract_method_signature"
            )
        {
            id = self.js_member_nid(class, n, &sanitized);
        }
        let mut meta = Value::Null;
        if let Some((_, _, overload_meta)) = &overload {
            meta = overload_meta.clone();
        }
        if self.is("ruby") && parent.is_some() {
            let kind = if n.kind_name() == "singleton_method" {
                if self.singleton_depth == 0
                    && n.child_by_field_name("object")
                        .is_some_and(|c| self.text(c) == "self")
                {
                    "singleton"
                } else {
                    "ambiguous"
                }
            } else {
                match self.singleton_depth {
                    0 => "instance",
                    1 => "singleton",
                    _ => "ambiguous",
                }
            };
            let kinds = self.ruby_kinds.entry(id.clone()).or_default();
            kinds.push(kind.into());
            let kind = if kinds.len() == 1 { kind } else { "ambiguous" };
            meta = json!({"ruby_method_kind":kind});
            if let Some(node) = self.nodes.iter_mut().find(|v| v["id"] == id) {
                if !node["metadata"].is_object() {
                    node["metadata"] = json!({});
                }
                node["metadata"]["ruby_method_kind"] = json!(kind);
            }
        }
        let fresh = !self.seen.contains(&id);
        self.add_node(
            &id,
            &format!("{}{name}()", if parent.is_some() { "." } else { "" }),
            line,
            None,
            meta,
        );
        if let Some((plain, signature, meta)) = overload {
            let group = self.csharp_overloads.entry(plain).or_default();
            if fresh && !group.iter().any(|(sig, _, _)| *sig == signature) {
                group.push((signature, id.clone(), meta));
            }
        }
        let owner = parent.unwrap_or(&self.file).to_string();
        self.edge(
            &owner,
            &id,
            if parent.is_some() {
                "method"
            } else {
                "contains"
            },
            line,
            None,
            Value::Null,
        );
        if self.is("c_sharp") && parent.is_some() && n.kind_name() == "method_declaration" {
            // Per overload, for typing `var v = R.M<A>()` cross-file (#4266).
            let info = csharp_return_info(n, self.source);
            let methods = self
                .csharp_returns
                .entry(owner.clone())
                .or_insert_with(|| json!({}));
            let overloads = methods
                .as_object_mut()
                .unwrap()
                .entry(name.clone())
                .or_insert_with(|| json!([]));
            overloads.as_array_mut().unwrap().push(info);
        }
        self.callable.insert(id.clone());
        if self.is("python") {
            self.locals
                .insert(id.clone(), python_local_bound_names(n, self.source));
        } else if self.js() {
            self.locals
                .insert(id.clone(), js_local_bound_names(n, self.source));
        }
        self.function_types(n, &id, parent);
        if self.is("java") && parent.is_some() && n.kind_name() == "method_declaration" {
            // Overloads share one node: keep a return type they all declare.
            let ret = java_receiver_type_name(n.child_by_field_name("type"), self.source)
                .unwrap_or_default();
            let kept = match self.java_returns.get(&id) {
                Some(previous) if *previous != ret => String::new(),
                _ => ret,
            };
            self.java_returns.insert(id.clone(), kept);
        }
        if let Some(body) = find_body(n, self.config) {
            if self.js() {
                self.js_members(body, parent.unwrap_or(&id), true);
            }
            if (self.is("java") || self.is("c_sharp"))
                && let Some(parent) = parent
            {
                self.method_scopes.insert(body.id(), (n, parent.into()));
            }
            self.bodies.push((id.clone(), body));
            if self.js() || self.is("python") {
                self.nested(body, &id);
            }
            if self.is("php") {
                // Closures nested in the body become their own functions.
                self.walk(body, parent);
            }
            if self.is("kotlin") {
                self.kotlin_literals(body, &id);
            }
            if self.is("java") || self.is("groovy") {
                self.java_anonymous_classes(body, &id);
            }
        }
    }
    /// `_js_member_nid` (0.9.92, #4475): `make_id` drops `_`, `$` and `#`, so
    /// `updateHash` / `_updateHash`, `close` / `#close` and `set` / `$set` in
    /// one class body minted one id and the second vanished. The member names
    /// of the class body (methods, signatures and function-valued fields) are
    /// grouped by the id they would get; a unique public name keeps the plain
    /// id and every other colliding name is salted. Getter/setter pairs and
    /// overloads share one name, so they are no collision.
    pub fn js_member_nid(&mut self, class: &str, member: Node<'t>, name: &str) -> String {
        let plain = make_id([class, name]);
        let Some(body) = member.parent() else {
            return plain;
        };
        let key = (class.to_string(), body.start_byte(), body.end_byte());
        if !self.js_member_groups.contains_key(&key) {
            let mut names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for c in children(body) {
                let name_node = match c.kind_name() {
                    "method_definition" | "method_signature" | "abstract_method_signature" => {
                        c.child_by_field_name("name")
                    }
                    "field_definition" | "public_field_definition" => {
                        if !c.child_by_field_name("value").is_some_and(|v| {
                            set("_JS_FUNCTION_VALUE_TYPES").contains(v.kind_name())
                        }) {
                            continue;
                        }
                        c.child_by_field_name("property")
                            .or_else(|| c.child_by_field_name("name"))
                    }
                    _ => continue,
                };
                let text = name_node.map(|n| self.text(n)).unwrap_or_default();
                if !text.is_empty() {
                    names
                        .entry(make_id([class, text.as_str()]))
                        .or_default()
                        .insert(text);
                }
            }
            names.retain(|_, names| names.len() >= 2);
            self.js_member_groups.insert(key.clone(), names);
        }
        underscore_salted_nid(&plain, name, &self.js_member_groups[&key], &['_', '$', '#'])
    }
    pub fn nested(&mut self, n: Node<'t>, parent: &str) {
        for c in children(n) {
            let target = if self.is("python") && c.kind_name() == "decorated_definition" {
                c.child_by_field_name("definition").unwrap_or(c)
            } else {
                c
            };
            if (self.is("python") && target.kind_name() == "function_definition")
                || (self.js()
                    && matches!(
                        target.kind_name(),
                        "function_declaration" | "generator_function_declaration"
                    ))
            {
                if let Some(name) = self
                    .name(target)
                    .filter(|s| !crate::ids::normalize_id(s).is_empty())
                {
                    let id = make_id([parent, name.as_str()]);
                    let line = target.start_position().row + 1;
                    self.add_node(&id, &format!("{name}()"), line, None, Value::Null);
                    self.edge(parent, &id, "contains", line, None, Value::Null);
                    self.callable.insert(id.clone());
                    let locals = if self.is("python") {
                        python_local_bound_names(target, self.source)
                    } else {
                        js_local_bound_names(target, self.source)
                    };
                    self.locals.insert(id.clone(), locals);
                    if self.is("python") {
                        self.parents.insert(id.clone(), parent.into());
                        self.lexical
                            .entry(parent.into())
                            .or_default()
                            .insert(name.clone(), id.clone());
                        self.lexical
                            .entry(id.clone())
                            .or_default()
                            .insert(name, id.clone());
                    }
                    if let Some(b) = find_body(target, self.config) {
                        self.bodies.push((id.clone(), b));
                        self.nested(b, &id);
                    }
                }
            } else if self.js() && set("_JS_FUNCTION_VALUE_TYPES").contains(c.kind_name()) {
                if let Some(b) = find_body(c, self.config) {
                    self.nested(b, parent);
                }
            } else {
                self.nested(c, parent);
            }
        }
    }
}
/// `_read_source_bytes` (0.9.78): the bytes tree-sitter parses, decoded by
/// [`super::base::read_source_bytes`] (UTF-16/UTF-32 with a byte-order mark,
/// cp1252/latin-1 fallback, the not-UTF-8 warning once per path).
fn read_source_bytes(path: &Path) -> std::io::Result<super::parse_cache::Source> {
    super::base::read_source_bytes(path, true)
}
/// Python's `Path(path).stem`: the file name without its final suffix.
fn python_path_stem(path: &str) -> String {
    use crate::path_syntax::{posix_name, posix_parts, suffix};
    let parts = posix_parts(&path.replace(std::path::MAIN_SEPARATOR, "/"));
    let name = posix_name(&parts);
    name[..name.len() - suffix(name).len()].to_string()
}
pub fn decorator_name(n: Node<'_>, s: &[u8]) -> Option<String> {
    let mut target = named(n).into_iter().next()?;
    if matches!(target.kind_name(), "call" | "call_expression") {
        target = target.child_by_field_name("function").unwrap_or(target);
    }
    match target.kind_name() {
        "attribute" => target
            .child_by_field_name("attribute")
            .map(|n| read_text(n, s)),
        "member_expression" => target
            .child_by_field_name("property")
            .map(|n| read_text(n, s)),
        "identifier" => Some(read_text(target, s)),
        _ => None,
    }
}
pub fn extract_generic(path: &Path, config: &LanguageConfig, source: Option<&[u8]>) -> Value {
    let loaded;
    let from_file = source.is_none();
    let source: &[u8] = match source {
        Some(s) => s,
        None => match read_source_bytes(path) {
            Ok(s) => {
                loaded = s;
                &loaded
            }
            Err(e) => {
                return json!({"nodes":[],"edges":[],"error":crate::io_compat::read_error(&e, path)});
            }
        },
    };
    // The tree can serve the run's later parses of this file only when it is
    // the file's own bytes: not a rewritten source, and no newline appended.
    let shareable = from_file && (source.is_empty() || source.ends_with(b"\n"));
    let parsed = match engine_parser::parse(config, source) {
        Ok(p) => p,
        Err(e) => return json!({"nodes":[],"edges":[],"error":e}),
    };
    let root = parsed.tree.root_node();
    let source = &parsed.source;
    let path_str = path.to_string_lossy().into_owned();
    let stem_id = make_id([python_path_stem(&path_str).as_str()]);
    let stem = file_stem(path);
    let file = make_id([path_str.as_str()]);
    let mut e = Engine {
        config,
        source,
        path: path_str,
        stem: stem.clone(),
        file: file.clone(),
        nodes: vec![],
        edges: vec![],
        raw: vec![],
        seen: BTreeSet::new(),
        callable: BTreeSet::new(),
        classes: BTreeSet::new(),
        namespace: vec![],
        scopes: vec![],
        bodies: vec![],
        initializers: vec![],
        locals: BTreeMap::new(),
        closure_locals: BTreeMap::new(),
        parents: Table::new(),
        lexical: BTreeMap::new(),
        type_table: Table::new(),
        swift_factory: BTreeMap::new(),
        swift_extensions: vec![],
        fields: BTreeMap::new(),
        method_scopes: BTreeMap::new(),
        interfaces: BTreeSet::new(),
        protocols: BTreeSet::new(),
        swift_classes: BTreeSet::new(),
        underscore: BTreeMap::new(),
        csharp_type_groups: BTreeMap::new(),
        csharp_member_groups: BTreeMap::new(),
        labels: Table::new(),
        labels_ci: Table::new(),
        seen_calls: BTreeSet::new(),
        seen_indirect: BTreeSet::new(),
        external: BTreeSet::new(),
        seen_dynamic: BTreeSet::new(),
        ruby_namespace: vec![],
        ruby_lexical: vec![],
        singleton_depth: 0,
        ruby_kinds: BTreeMap::new(),
        mixins: vec![],
        listeners: vec![],
        scan_root: if config.ts_module == "tree_sitter_python" {
            SCAN_ROOT.with(|root| root.borrow().clone())
        } else {
            None
        },
        php_closures: BTreeMap::new(),
        lua_self_table: Table::new(),
        method_owner: Table::new(),
        methods_by_owner: BTreeMap::new(),
        local_bases: BTreeMap::new(),
        python_shadow: None,
        js_exported: None,
        field_order: vec![],
        stem_id,
        stem_forward_refs: Table::new(),
        java_field_names: BTreeMap::new(),
        java_returns: Table::new(),
        java_bound: BTreeMap::new(),
        csharp_type_paths: BTreeMap::new(),
        csharp_returns: serde_json::Map::new(),
        class_parent: Table::new(),
        lexical_bases: BTreeMap::new(),
        python_var_types: BTreeMap::new(),
        python_attr_types: BTreeMap::new(),
        csharp_overloads: BTreeMap::new(),
        python_fixture_params: BTreeMap::new(),
        js_factory_scope: Table::new(),
        js_member_groups: BTreeMap::new(),
    };
    if e.is("c_sharp") {
        e.interfaces = csharp_pre_scan_interfaces(root, source);
        (e.csharp_type_groups, e.csharp_member_groups) =
            csharp_pre_scan_name_collisions(root, source, &stem);
    }
    if e.is("swift") {
        (e.protocols, e.swift_classes) = swift_pre_scan(root, source);
    }
    if e.is("python") {
        e.underscore = python_pre_scan_underscore_collisions(root, source, &stem);
    }
    if e.js() {
        e.scan_external(root);
    }
    e.add_node(
        &file,
        &path.file_name().unwrap_or_default().to_string_lossy(),
        1,
        None,
        Value::Null,
    );
    if source.is_empty()
        && let Some(n) = e.nodes.iter_mut().find(|n| n["id"] == file.as_str())
    {
        // A zero-byte file has no line 1: the node stays (an empty
        // `__init__.py` is still importable) but is unanchored (0.9.77).
        n["source_location"] = Value::Null;
    }
    if e.is("ruby") {
        e.ruby_file_metadata(root);
    }
    e.walk(root, None);
    e.settle_stem_forward_refs();
    let result = e.finish(root);
    // The run's symbol-resolution pass reads a JS/TS file's declarations,
    // imports, exports, calls and members from its tree; they are derived now
    // from this tree when that pass would parse the same bytes the same way.
    if shareable
        && super::parse_cache::run_active()
        && let Some(tag) = engine_parser::js_tag(config)
        && let Some(syntax) = super::resolution_facts::js_syntax(path, tag, &parsed)
    {
        super::parse_cache::keep_facts(super::resolution_facts::JS_SYNTAX, path, Box::new(syntax));
    }
    // Likewise for the Java and PHP type passes, which parse with the same
    // grammar the extractor used.
    if shareable && super::parse_cache::run_active() {
        use super::resolution_types as types;
        match (config.ts_module.as_str(), config.ts_language_fn.as_str()) {
            ("tree_sitter_java", _) => super::parse_cache::keep_facts(
                types::JAVA_SYNTAX,
                path,
                Box::new(types::java_syntax(&parsed)),
            ),
            ("tree_sitter_php", "language_php") => super::parse_cache::keep_facts(
                types::PHP_SYNTAX,
                path,
                Box::new(types::php_syntax(&parsed)),
            ),
            _ => {}
        }
    }
    // Python's rationale pass parses the file again right after this, on this
    // thread; it takes this tree instead. The run's symbol-resolution pass
    // reads the same tree later; it takes the syntax it needs, kept now.
    if shareable && config.ts_module == "tree_sitter_python" {
        if super::parse_cache::run_active() {
            super::parse_cache::keep_facts(
                super::resolution_facts::PYTHON_SYNTAX,
                path,
                Box::new(super::resolution_facts::python_syntax(path, &parsed)),
            );
        }
        super::parse_cache::hand_off(path, "python", std::sync::Arc::new(parsed));
    }
    result
}
pub fn dispatch(r: &Value) -> crate::Result<Value> {
    let path = Path::new(r["path"].as_str().ok_or("Expected path")?);
    let c = if r["config"].is_object() {
        let mut c: LanguageConfig =
            serde_json::from_value(r["config"].clone()).map_err(crate::Error::from)?;
        let callbacks = &r["config"]["_callbacks"];
        if callbacks["import_handler"].is_string() {
            let module = c.ts_module.clone();
            c.import_handler = Some(std::sync::Arc::new(move |n, s, f, _st, e, p, sc, root| {
                engine_config::import(&module, n, s, f, e, p, sc, root)
            }));
        }
        if callbacks["resolve_function_name_fn"] == "_get_cpp_func_name" {
            c.resolve_function_name_fn = Some(std::sync::Arc::new(get_cpp_func_name));
        }
        if callbacks["resolve_function_name_fn"] == "_get_c_func_name" {
            c.resolve_function_name_fn = Some(std::sync::Arc::new(engine_config::c_func_name));
        }
        if callbacks["sanitize_symbol_name_fn"] == "_ruby_sanitize_method_name" {
            c.sanitize_symbol_name_fn = Some(std::sync::Arc::new(engine_config::ruby_sanitize));
        }
        c
    } else {
        engine_config::config(r["language"].as_str().unwrap_or("python"))?
    };
    let source = r["source"].as_str().map(str::as_bytes);
    let _root = enter_scan_root(r["scan_root"].as_str().map(Path::new));
    if r["action"] == "tree" {
        return Ok(json!(
            engine_parser::parse(&c, r["source"].as_str().unwrap_or("").as_bytes())?
                .tree
                .root_node()
                .to_sexp()
        ));
    }
    // One file through this operation is a run of its own: `extract()` clears
    // the per-run tsconfig/jsconfig alias, baseUrl and package-imports caches
    // when it starts, and a long-lived process must not answer a later file
    // from an earlier run's `paths` (Graphify 0.9.87 language-override tests).
    super::resolution_js::clear();
    Ok(extract_generic(path, &c, source))
}
